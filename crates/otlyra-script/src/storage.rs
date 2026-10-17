//! What `localStorage` and `sessionStorage` keep (HTML §12.2).
//!
//! A [`StorageArea`] is one kind of storage for every origin: the browser
//! keeps one for `localStorage`, shared by every document of an origin in
//! every tab, and one per tab for `sessionStorage`, which outlives the
//! documents the tab moves between. Documents reach theirs from their own
//! thread, so an area is behind a lock and shared by `Arc`.
//!
//! Each origin may keep [`QUOTA`] UTF-16 code units of keys and values, the
//! five mebibytes the other engines give. A write that would go over it is
//! refused and nothing changes.
//!
//! The `localStorage` area of a browser a person uses is kept on disk
//! ([`StorageArea::persisted`]) and written back a while after it changes,
//! off the thread that changed it. Each change is also logged, so the other
//! documents of its origin can be told with a `storage` event.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How much one origin may keep, in UTF-16 code units of its keys and values.
pub const QUOTA: usize = 5 * 1024 * 1024;

/// One origin's items, in the order they were first set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Items {
    entries: Vec<(String, String)>,
    /// The code units the keys and values take, against [`QUOTA`].
    used: usize,
}

impl Items {
    fn position(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|(name, _)| name == key)
    }
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// A write that would take an origin over [`QUOTA`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QuotaExceeded;

/// One change a document made to an origin's storage: what its other
/// documents hear as a `storage` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageChange {
    /// Its place in the log.
    pub seq: u64,
    /// Whose storage it was.
    pub origin: String,
    /// The key, or `None` for `clear()`.
    pub key: Option<String>,
    /// What the key held before.
    pub old: Option<String>,
    /// What it holds now.
    pub new: Option<String>,
    /// The address of the document that made it.
    pub url: String,
    /// Which document made it, which is not told.
    pub source: u64,
}

/// How many changes the log keeps for documents yet to hear them.
const LOG: usize = 1024;

/// One kind of storage, for every origin that has any.
#[derive(Debug, Default)]
pub struct StorageArea {
    origins: Mutex<HashMap<String, Items>>,
    /// The file it is kept in, for an area that outlives the browser.
    file: Option<PathBuf>,
    /// Whether it changed since it was last written.
    dirty: AtomicBool,
    /// The changes documents made, newest last.
    log: Mutex<VecDeque<StorageChange>>,
    next: AtomicU64,
}

impl Drop for StorageArea {
    fn drop(&mut self) {
        if let Some((path, text)) = self.snapshot() {
            write_file(&path, &text);
        }
    }
}

/// `text` with the separators of the file format escaped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Write `text` to `path` whole or not at all: a file half-written when the
/// browser stopped would lose every origin's items, not just the last.
fn write_file(path: &std::path::Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let partial = path.with_extension("partial");
    let written = std::fs::write(&partial, text).and_then(|()| std::fs::rename(&partial, path));
    if let Err(error) = written {
        tracing::warn!(%error, path = %path.display(), "localStorage could not be written");
    }
}

impl StorageArea {
    /// Nothing kept yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The area kept in `path`: what it held when last written, and every
    /// change written back to it.
    ///
    /// One line per item — origin, key, value, tab-separated and escaped — in
    /// the order the items were set. A file that cannot be read is an empty
    /// area, and a line that cannot be is skipped.
    #[must_use]
    pub fn persisted(path: PathBuf) -> Self {
        let mut origins: HashMap<String, Items> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let mut fields = line.split('\t');
                let (Some(origin), Some(key), Some(value), None) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let (key, value) = (unescape(key), unescape(value));
                let items = origins.entry(unescape(origin)).or_default();
                items.used += units(&key) + units(&value);
                items.entries.push((key, value));
            }
        }
        Self {
            origins: Mutex::new(origins),
            file: Some(path),
            dirty: AtomicBool::new(false),
            log: Mutex::new(VecDeque::new()),
            next: AtomicU64::new(0),
        }
    }

    /// The file and its new text, when the area is kept in one and changed
    /// since it was last written.
    fn snapshot(&self) -> Option<(PathBuf, String)> {
        let path = self.file.clone()?;
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        let text = self.with(|origins| {
            let mut text = String::new();
            let mut names: Vec<&String> = origins.keys().collect();
            names.sort();
            for origin in names {
                for (key, value) in &origins[origin].entries {
                    text.push_str(&escape(origin));
                    text.push('\t');
                    text.push_str(&escape(key));
                    text.push('\t');
                    text.push_str(&escape(value));
                    text.push('\n');
                }
            }
            text
        });
        Some((path, text))
    }

    /// Write the area back to its file if it changed, on a thread of its own:
    /// the page that changed it is not kept waiting on the disk.
    pub fn flush(&self) {
        if let Some((path, text)) = self.snapshot() {
            let spawned = std::thread::Builder::new()
                .name("otlyra-storage".to_owned())
                .spawn(move || write_file(&path, &text));
            if let Err(error) = spawned {
                tracing::warn!(%error, "localStorage could not be written");
            }
        }
    }

    /// Whether it changed since it was last written.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Log a change one document made, for the others of its origin.
    pub fn record(&self, mut change: StorageChange) {
        change.seq = self.next.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut log) = self.log.lock() {
            if log.len() >= LOG {
                log.pop_front();
            }
            log.push_back(change);
        }
    }

    /// The newest change's place in the log: where a document starts
    /// listening from.
    #[must_use]
    pub fn latest(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// The changes after `seen` to `origin` that `source` did not make.
    #[must_use]
    pub fn changes_since(&self, seen: u64, origin: &str, source: u64) -> Vec<StorageChange> {
        self.log.lock().map_or_else(
            |_| Vec::new(),
            |log| {
                log.iter()
                    .filter(|change| {
                        change.seq > seen && change.origin == origin && change.source != source
                    })
                    .cloned()
                    .collect()
            },
        )
    }

    fn changed(&self) {
        if self.file.is_some() {
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut HashMap<String, Items>) -> R) -> R {
        f(&mut self
            .origins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// How many items `origin` keeps.
    #[must_use]
    pub fn length(&self, origin: &str) -> usize {
        self.with(|origins| origins.get(origin).map_or(0, |items| items.entries.len()))
    }

    /// The `index`th key `origin` keeps.
    #[must_use]
    pub fn key(&self, origin: &str, index: usize) -> Option<String> {
        self.with(|origins| {
            origins
                .get(origin)
                .and_then(|items| items.entries.get(index))
                .map(|(key, _)| key.clone())
        })
    }

    /// The value `origin` keeps under `key`.
    #[must_use]
    pub fn get(&self, origin: &str, key: &str) -> Option<String> {
        self.with(|origins| {
            let items = origins.get(origin)?;
            items.position(key).map(|at| items.entries[at].1.clone())
        })
    }

    /// Keep `value` under `key` for `origin`, unless that would take it over
    /// [`QUOTA`].
    pub fn set(&self, origin: &str, key: &str, value: &str) -> Result<(), QuotaExceeded> {
        self.with(|origins| {
            let items = origins.entry(origin.to_owned()).or_default();
            let existing = items.position(key);
            let freed = existing.map_or(0, |at| {
                units(&items.entries[at].0) + units(&items.entries[at].1)
            });
            let needed = units(key) + units(value);
            let used = items.used - freed + needed;
            if used > QUOTA {
                if items.entries.is_empty() {
                    origins.remove(origin);
                }
                return Err(QuotaExceeded);
            }
            items.used = used;
            match existing {
                Some(at) => value.clone_into(&mut items.entries[at].1),
                None => items.entries.push((key.to_owned(), value.to_owned())),
            }
            Ok(())
        })
        .inspect(|()| self.changed())
    }

    /// Forget `key` for `origin`.
    pub fn remove(&self, origin: &str, key: &str) {
        self.with(|origins| {
            let Some(items) = origins.get_mut(origin) else {
                return;
            };
            if let Some(at) = items.position(key) {
                let (key, value) = items.entries.remove(at);
                items.used -= units(&key) + units(&value);
            }
            if items.entries.is_empty() {
                origins.remove(origin);
            }
        });
        self.changed();
    }

    /// Forget everything `origin` keeps.
    pub fn clear(&self, origin: &str) {
        self.with(|origins| {
            origins.remove(origin);
        });
        self.changed();
    }

    /// Forget what every origin `wanted` says yes to keeps: what clearing a
    /// site's data, or all of it, does to storage.
    pub fn forget(&self, wanted: impl Fn(&str) -> bool) {
        self.with(|origins| origins.retain(|origin, _| !wanted(origin)));
        self.changed();
    }

    /// The origins that keep anything, in no order.
    #[must_use]
    pub fn origins(&self) -> Vec<String> {
        self.with(|origins| origins.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_keep_their_order_and_are_per_origin() {
        let area = StorageArea::new();
        area.set("https://a.test", "one", "1").unwrap();
        area.set("https://a.test", "two", "2").unwrap();
        area.set("https://a.test", "one", "uno").unwrap();
        area.set("https://b.test", "one", "b").unwrap();
        assert_eq!(area.length("https://a.test"), 2);
        assert_eq!(area.key("https://a.test", 0).as_deref(), Some("one"));
        assert_eq!(area.key("https://a.test", 1).as_deref(), Some("two"));
        assert_eq!(area.get("https://a.test", "one").as_deref(), Some("uno"));
        assert_eq!(area.get("https://b.test", "one").as_deref(), Some("b"));
        area.remove("https://a.test", "one");
        assert_eq!(area.get("https://a.test", "one"), None);
        area.clear("https://a.test");
        assert_eq!(area.length("https://a.test"), 0);
        assert_eq!(area.origins(), vec!["https://b.test".to_owned()]);
    }

    #[test]
    fn a_persisted_area_comes_back_from_its_file() {
        let directory = std::env::temp_dir().join(format!(
            "otlyra-storage-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = directory.join("local-storage.tsv");
        let _ = std::fs::remove_file(&path);
        {
            let area = StorageArea::persisted(path.clone());
            area.set("https://a.test", "line\tbreak", "one\ntwo \\ three")
                .unwrap();
            area.set("https://a.test", "second", "2").unwrap();
            area.set("https://b.test", "k", "v").unwrap();
            assert!(area.is_dirty());
        }
        let area = StorageArea::persisted(path.clone());
        assert_eq!(
            area.get("https://a.test", "line\tbreak").as_deref(),
            Some("one\ntwo \\ three")
        );
        assert_eq!(area.key("https://a.test", 1).as_deref(), Some("second"));
        assert_eq!(area.get("https://b.test", "k").as_deref(), Some("v"));
        assert!(!area.is_dirty(), "nothing changed since it was read");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_log_tells_the_other_documents_of_an_origin() {
        let area = StorageArea::new();
        let seen = area.latest();
        let change = |origin: &str, source| StorageChange {
            seq: 0,
            origin: origin.to_owned(),
            key: Some("k".to_owned()),
            old: None,
            new: Some("v".to_owned()),
            url: format!("{origin}/page"),
            source,
        };
        area.record(change("https://a.test", 1));
        area.record(change("https://b.test", 1));
        area.record(change("https://a.test", 2));
        let heard = area.changes_since(seen, "https://a.test", 2);
        assert_eq!(heard.len(), 1);
        assert_eq!(heard[0].source, 1);
        assert!(
            area.changes_since(area.latest(), "https://a.test", 9)
                .is_empty()
        );
    }

    #[test]
    fn a_write_over_the_quota_changes_nothing() {
        let area = StorageArea::new();
        let big = "x".repeat(QUOTA - 10);
        area.set("https://a.test", "big", &big).unwrap();
        assert_eq!(
            area.set("https://a.test", "more", "0123456789"),
            Err(QuotaExceeded)
        );
        assert_eq!(area.get("https://a.test", "more"), None);
        // Replacing a value counts what the old one freed.
        area.set("https://a.test", "big", "small").unwrap();
        area.set("https://a.test", "more", "0123456789").unwrap();
        // An origin that keeps nothing is not listed.
        assert_eq!(
            area.set("https://c.test", "huge", &"y".repeat(QUOTA + 1)),
            Err(QuotaExceeded)
        );
        assert!(!area.origins().contains(&"https://c.test".to_owned()));
    }
}
