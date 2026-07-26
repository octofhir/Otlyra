//! The one cookie jar, and the file it survives in.
//!
//! The jar itself is `otlyra_net::cookie` — the rules, the matching, what a site
//! is entitled to keep. This is the browser's half of it: one jar shared between
//! the loader that fills it and the surfaces that show it, and the file it is read
//! from and written back to.
//!
//! Two jars, in the sense that matters. A session cookie is one the reader is
//! signed in with *now*: it lives in memory and dies with the process, which is
//! what makes closing the browser end a session. A persistent one has an expiry
//! the server asked for, and that is what goes to disk.
//!
//! # When it is written
//!
//! Not on every change, and not on a timer. The jar keeps a revision that moves
//! only when the *persistent* set does, and [`CookieStore::flush`] writes only
//! when it has moved since the last write. A site resetting a session cookie on
//! every response — which is most of them — therefore costs no disk at all, and a
//! sign-in costs one write. Through a temporary file and a rename, like the
//! bookmarks: a crash mid-write cannot leave half a jar where the whole one was.
//!
//! Reading the file is the shell's job, the same rule the bookmarks, the
//! preferences and the system clipboard already follow. A browser that read it in
//! its constructor would mean every test reading and writing the cookies of
//! whoever ran them — and cookies are the one store where that would be somebody's
//! signed-in session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use otlyra_net::SharedJar;
use otlyra_net::cookie::{Capacity, Jar, store};

/// What the cookie file is called inside the browser's own directory.
const FILE: &str = "cookies.tsv";

/// The jar, and where it is kept.
pub struct CookieStore {
    jar: SharedJar,
    /// Where to write. `None` in a test and on a platform with nowhere to write,
    /// which turns persistence off rather than turning cookies off.
    file: Option<PathBuf>,
    /// The revision last written, so a flush with nothing to say costs nothing.
    written: u64,
}

impl Default for CookieStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl CookieStore {
    /// A jar that keeps nothing between runs.
    ///
    /// What a test, a screenshot and every headless mode get, so none of them can
    /// read or overwrite a person's session.
    pub fn in_memory() -> Self {
        Self {
            jar: Arc::new(Mutex::new(Jar::new())),
            file: None,
            written: 0,
        }
    }

    /// Attach the file: read what the last run kept into this jar, and write every
    /// change from now on.
    ///
    /// **The jar itself is not replaced.** A loader was handed this jar before the
    /// shell decided whether to persist it, and swapping it here would leave the
    /// loader filling a jar nobody reads — the bug that shape of wiring always
    /// produces. What is read is put into the jar that already exists.
    ///
    /// Never fails. A file that is not there is a browser nobody has been signed
    /// in with; a file that cannot be read is a warning and an empty jar, because
    /// refusing to start over a cookie file would be refusing to start.
    pub fn persist(&mut self) {
        let Some(file) = file_path() else {
            tracing::warn!("nowhere to keep cookies; sessions will not survive this run");
            return;
        };
        let now = SystemTime::now();
        // A file that is not there is not a warning: a browser nobody has signed in
        // with has none, and saying so on every launch would be noise about the
        // ordinary case.
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let read = store::from_text(&text, Capacity::default(), now);
        let revision = self.with(|jar| {
            for cookie in read.all() {
                jar.put(cookie.clone());
            }
            jar.kept_revision()
        });
        // What was read is already what is on disk, so the first flush has nothing
        // to do.
        self.written = revision;
        self.file = Some(file);
    }

    /// The jar itself, to give to a loader.
    pub fn jar(&self) -> SharedJar {
        Arc::clone(&self.jar)
    }

    /// Whether this store puts anything on disk.
    pub fn is_persistent(&self) -> bool {
        self.file.is_some()
    }

    /// Do something with the jar under its lock.
    ///
    /// The lock is held for the call and no longer. A jar poisoned by a panic
    /// elsewhere is still a list of cookies, so it is taken back rather than
    /// spreading the panic to everything that wanted one.
    pub fn with<T>(&self, act: impl FnOnce(&mut Jar) -> T) -> T {
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        act(&mut jar)
    }

    /// Write the persistent cookies down, if any of them have changed.
    ///
    /// Cheap to call often, which is the point: the caller does not have to know
    /// whether a fetch set a cookie, only that one finished.
    pub fn flush(&mut self) {
        let Some(file) = self.file.clone() else {
            return;
        };
        let now = SystemTime::now();
        let (revision, text) = self.with(|jar| (jar.kept_revision(), store::to_text(jar, now)));
        if revision == self.written {
            return;
        }
        self.written = revision;

        if let Some(directory) = file.parent()
            && let Err(error) = std::fs::create_dir_all(directory)
        {
            tracing::warn!(%error, path = %directory.display(), "could not make the browser's directory");
            return;
        }
        let temporary = file.with_extension("tsv.writing");
        if let Err(error) = std::fs::write(&temporary, text) {
            tracing::warn!(%error, path = %temporary.display(), "could not write the cookies");
            return;
        }
        if let Err(error) = std::fs::rename(&temporary, &file) {
            tracing::warn!(%error, path = %file.display(), "could not replace the cookies");
            let _ = std::fs::remove_file(&temporary);
        }
    }
}

/// Where the cookie file lives, when there is anywhere.
fn file_path() -> Option<PathBuf> {
    Some(crate::preferences::directory()?.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn url(address: &str) -> Url {
        Url::parse(address).expect("a url")
    }

    /// A store with nowhere to write is a working jar, not a broken one.
    #[test]
    fn a_store_with_no_file_still_keeps_cookies() {
        let mut store = CookieStore::in_memory();
        assert!(!store.is_persistent());
        store.with(|jar| {
            jar.set(&url("https://x.test/"), "a=1", SystemTime::now())
                .expect("kept");
        });
        assert_eq!(store.with(|jar| jar.len()), 1);
        // And flushing one is a no-op rather than a failure.
        store.flush();
        assert_eq!(store.with(|jar| jar.len()), 1);
    }

    /// The loader and the surfaces hold the same jar, not two copies of one.
    #[test]
    fn the_jar_handed_out_is_the_jar_kept() {
        let store = CookieStore::in_memory();
        let handed = store.jar();
        handed
            .lock()
            .expect("not poisoned")
            .set(&url("https://x.test/"), "a=1", SystemTime::now())
            .expect("kept");
        assert_eq!(store.with(|jar| jar.len()), 1);
    }

    /// A session cookie must not be able to make the browser write a file. This is
    /// the whole reason the revision counts only what is kept.
    #[test]
    fn a_session_cookie_moves_no_revision() {
        let store = CookieStore::in_memory();
        let before = store.with(|jar| jar.kept_revision());
        store.with(|jar| {
            for line in ["a=1", "b=2", "c=3"] {
                jar.set(&url("https://x.test/"), line, SystemTime::now())
                    .expect("kept");
            }
        });
        assert_eq!(store.with(|jar| jar.kept_revision()), before);

        // And one with an expiry does move it.
        store.with(|jar| {
            jar.set(
                &url("https://x.test/"),
                "d=4; Max-Age=600",
                SystemTime::now(),
            )
            .expect("kept");
        });
        assert_ne!(store.with(|jar| jar.kept_revision()), before);
    }
}
