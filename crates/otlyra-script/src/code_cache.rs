//! The compiled form of a page's scripts, kept between runs.
//!
//! A site opened every morning sends the same bundles every morning, and the
//! HTTP cache already hands their bytes back without a request. Parsing and
//! compiling them again is then most of what is left of the engine's work on
//! the load. The engine can hand out what it compiled and take it back next
//! time; this is where those bytes live meanwhile.
//!
//! # Contents
//!
//! - [`CodeCache`] — one directory, one file per key: a module's URL, or a
//!   classic script's text ([`CodeCache::script_key`]).
//! - [`EngineCodeCache`] — the same directory as the engine's own hook, for
//!   the `eval` text it keeps by keys of its own.
//!
//! # Invariants
//!
//! - The bytes are the engine's own and checked by the engine before use:
//!   they are valid only for the same engine build and the exact source text,
//!   so a script that changed, or an engine that was rebuilt, simply compiles
//!   again. Nothing here interprets them, and a stale file is never a wrong
//!   answer, only a miss.
//! - A file names the key it was written for and is used only for that key:
//!   two keys whose names hash alike cannot answer for each other.
//! - A classic script is found by its text, not its address: a bundle whose
//!   query string changes on every load is the same compile each time, and
//!   the engine stamps the address it runs under. A module is found by its
//!   URL, which its imports resolve against.
//! - Writing never waits on the page's thread: a batch of files is written
//!   on a thread of its own, each through a temporary file renamed into
//!   place, so a reader sees a whole file or none.
//! - It is kept for the same reason and cleared under the same rule as the
//!   HTTP cache it sits beside.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// First bytes of every file: which program wrote it and in what layout.
const MAGIC: &[u8] = b"otlyra-code-cache\n";

/// Largest file worth keeping or reading back.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The engine's code-cache hook over a [`CodeCache`]: what the engine keeps
/// itself, which is the text a page hands `eval`, filed beside the scripts.
#[derive(Debug)]
pub struct EngineCodeCache(pub std::sync::Arc<CodeCache>);

impl otter_runtime::RuntimeCodeCacheHook for EngineCodeCache {
    fn load(&self, key: &str) -> Option<Vec<u8>> {
        self.0.load(&format!("engine:{key}"))
    }

    fn store(&self, key: &str, bytes: Vec<u8>) {
        self.0.store(vec![(format!("engine:{key}"), bytes)]);
    }
}

/// Shortest script worth a file: below this, compiling it again costs less
/// than opening and checking what was kept.
pub const MIN_SCRIPT_BYTES: usize = 2048;

/// A directory of compiled scripts, keyed by URL.
#[derive(Debug)]
pub struct CodeCache {
    directory: PathBuf,
}

impl CodeCache {
    /// Keep compiled scripts in `directory`, made on first write.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// The key a classic script's compiled form is kept under.
    #[must_use]
    pub fn script_key(source: &str) -> String {
        let mut hasher = rustc_hash::FxHasher::default();
        source.hash(&mut hasher);
        format!("script:{:016x}:{}", hasher.finish(), source.len())
    }

    /// The bytes written for `url`, if any.
    #[must_use]
    pub fn load(&self, url: &str) -> Option<Vec<u8>> {
        let path = self.path_of(url);
        if std::fs::metadata(&path).ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let file = std::fs::read(path).ok()?;
        let rest = file.strip_prefix(MAGIC)?;
        let (stored_url, bytes) = split_line(rest)?;
        (stored_url == url.as_bytes()).then(|| bytes.to_vec())
    }

    /// Write each `(url, bytes)` pair, off this thread.
    pub fn store(&self, entries: Vec<(String, Vec<u8>)>) {
        if entries.is_empty() {
            return;
        }
        let directory = self.directory.clone();
        let paths: Vec<PathBuf> = entries.iter().map(|(url, _)| self.path_of(url)).collect();
        let spawned = std::thread::Builder::new()
            .name("otlyra-code-cache".to_owned())
            .spawn(move || {
                if std::fs::create_dir_all(&directory).is_err() {
                    return;
                }
                for ((url, bytes), path) in entries.into_iter().zip(paths) {
                    if bytes.len() as u64 > MAX_FILE_BYTES {
                        continue;
                    }
                    if let Err(error) = write_whole(&path, &url, &bytes) {
                        tracing::debug!(%url, %error, "a compiled script was not kept");
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::debug!(%error, "no thread to keep compiled scripts on");
        }
    }

    /// Forget every compiled script.
    pub fn clear(&self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }

    fn path_of(&self, url: &str) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut hasher);
        self.directory
            .join(format!("{:016x}.code", hasher.finish()))
    }
}

/// The bytes up to the first newline, and the rest after it.
fn split_line(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = bytes.iter().position(|&byte| byte == b'\n')?;
    Some((&bytes[..end], &bytes[end + 1..]))
}

fn write_whole(path: &Path, url: &str, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    let mut contents = Vec::with_capacity(MAGIC.len() + url.len() + 1 + bytes.len());
    contents.extend_from_slice(MAGIC);
    contents.extend_from_slice(url.as_bytes());
    contents.push(b'\n');
    contents.extend_from_slice(bytes);
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_answers_only_for_the_url_it_was_written_for() {
        let directory =
            std::env::temp_dir().join(format!("otlyra-code-cache-{}", std::process::id()));
        let cache = CodeCache::new(&directory);
        cache.clear();
        assert_eq!(cache.load("https://a.test/app.js"), None);
        std::fs::create_dir_all(&directory).expect("directory");
        write_whole(
            &cache.path_of("https://a.test/app.js"),
            "https://a.test/app.js",
            b"one",
        )
        .expect("written");
        assert_eq!(
            cache.load("https://a.test/app.js").as_deref(),
            Some(&b"one"[..])
        );
        // The same file under another name is not that name's.
        std::fs::copy(
            cache.path_of("https://a.test/app.js"),
            cache.path_of("https://b.test/other.js"),
        )
        .expect("copied");
        assert_eq!(cache.load("https://b.test/other.js"), None);
        cache.clear();
        assert_eq!(cache.load("https://a.test/app.js"), None);
    }
}
