//! One serialized, coalescing owner for sealed cookie writes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use otlyra_net::SharedJar;
use otlyra_net::cookie::{Jar, store};

use super::PURPOSE;
use crate::secret::Key;

const RETRY: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests;

struct State {
    wanted: u64,
    written: u64,
    attempts: u64,
    last_attempt: Option<(u64, bool)>,
    retry_at: Option<Instant>,
    stopping: bool,
    finished: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

pub(super) struct CookieWriter {
    jar: SharedJar,
    shared: Arc<Shared>,
    owner: Option<JoinHandle<bool>>,
}

impl CookieWriter {
    pub(super) fn spawn(
        jar: SharedJar,
        key: Arc<OnceLock<Option<Key>>>,
        file: PathBuf,
        written: u64,
    ) -> std::io::Result<Self> {
        Self::spawn_with(jar, key, file, written, write_atomic)
    }

    fn spawn_with(
        jar: SharedJar,
        key: Arc<OnceLock<Option<Key>>>,
        file: PathBuf,
        written: u64,
        mut write: impl FnMut(&Path, &Key, &Jar) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                wanted: written,
                written,
                attempts: 0,
                last_attempt: None,
                retry_at: None,
                stopping: false,
                finished: false,
            }),
            changed: Condvar::new(),
        });
        let owner_shared = Arc::clone(&shared);
        let owner_jar = Arc::clone(&jar);
        let owner = std::thread::Builder::new()
            .name("otlyra-cookies".into())
            .spawn(move || {
                // Also wake a synchronous waiter if a worker unexpectedly panics.
                let _completion = Completion(Arc::clone(&owner_shared));
                loop {
                    let mut state = owner_shared.state.lock().unwrap();
                    loop {
                        if state.stopping && state.written >= state.wanted {
                            return true;
                        }
                        if state.written < state.wanted {
                            if state.stopping
                                || state.retry_at.is_none_or(|at| at <= Instant::now())
                            {
                                break;
                            }
                            let delay = state
                                .retry_at
                                .unwrap()
                                .saturating_duration_since(Instant::now());
                            state = owner_shared.changed.wait_timeout(state, delay).unwrap().0;
                        } else {
                            state = owner_shared.changed.wait(state).unwrap();
                        }
                    }
                    drop(state);
                    // Copy under the jar lock, then serialize, unlock the keychain,
                    // seal and write with neither jar nor scheduling lock held.
                    let (revision, snapshot) = {
                        let jar = owner_jar.lock().unwrap_or_else(|error| error.into_inner());
                        (jar.kept_revision(), jar.clone())
                    };
                    let key = key.get_or_init(Key::from_keychain);
                    let result = match key {
                        Some(key) => write(&file, key, &snapshot),
                        None => Err("no key to seal cookies with".into()),
                    };
                    if let Err(error) = &result {
                        tracing::warn!(%error, path = %file.display(), "could not persist cookies");
                    }
                    let mut state = owner_shared.state.lock().unwrap();
                    state.wanted = state.wanted.max(revision);
                    state.attempts += 1;
                    state.last_attempt = Some((revision, result.is_ok()));
                    if result.is_ok() {
                        state.written = revision;
                        state.retry_at = None;
                    } else {
                        state.retry_at = Some(Instant::now() + RETRY);
                    }
                    owner_shared.changed.notify_all();
                    if key.is_none() || state.stopping && result.is_err() {
                        return false;
                    }
                }
            })?;
        Ok(Self {
            jar,
            shared,
            owner: Some(owner),
        })
    }

    pub(super) fn schedule(&self, revision: u64) {
        let mut state = self.shared.state.lock().unwrap();
        if revision > state.wanted {
            state.wanted = revision;
            state.retry_at = None;
            self.shared.changed.notify_one();
        }
    }

    pub(super) fn flush(&self, revision: u64) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        if state.written >= revision {
            return true;
        }
        let before = state.attempts;
        state.wanted = state.wanted.max(revision);
        state.retry_at = None;
        self.shared.changed.notify_one();
        loop {
            if state.written >= revision {
                return true;
            }
            if state.finished
                || state.attempts > before
                    && state
                        .last_attempt
                        .is_some_and(|(attempted, ok)| attempted >= revision && !ok)
            {
                return false;
            }
            state = self.shared.changed.wait(state).unwrap();
        }
    }

    pub(super) fn finish(&mut self) -> bool {
        let revision = self
            .jar
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .kept_revision();
        {
            let mut state = self.shared.state.lock().unwrap();
            state.wanted = state.wanted.max(revision);
            state.stopping = true;
            state.retry_at = None;
            self.shared.changed.notify_one();
        }
        self.owner.take().map_or_else(
            || self.shared.state.lock().unwrap().written >= revision,
            |owner| owner.join().unwrap_or(false),
        )
    }
}

impl Drop for CookieWriter {
    fn drop(&mut self) {
        self.finish();
    }
}

struct Completion(Arc<Shared>);
impl Drop for Completion {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.finished = true;
        self.0.changed.notify_all();
    }
}

fn write_atomic(file: &Path, key: &Key, jar: &Jar) -> Result<(), String> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let text = store::to_text(jar, SystemTime::now());
    let sealed = key
        .seal(PURPOSE, text.as_bytes())
        .ok_or("could not seal cookies")?;
    let directory = file.parent().ok_or("cookie file has no directory")?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let temporary = file.with_extension(format!(
        "dat.{}.{}.writing",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.write_all(&sealed)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(&temporary, file)?;
        #[cfg(unix)]
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| error.to_string())
}
