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

use std::collections::HashMap;
use std::sync::Mutex;

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

/// One kind of storage, for every origin that has any.
#[derive(Debug, Default)]
pub struct StorageArea {
    origins: Mutex<HashMap<String, Items>>,
}

impl StorageArea {
    /// Nothing kept yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    }

    /// Forget everything `origin` keeps.
    pub fn clear(&self, origin: &str) {
        self.with(|origins| {
            origins.remove(origin);
        });
    }

    /// Forget what every origin `wanted` says yes to keeps: what clearing a
    /// site's data, or all of it, does to storage.
    pub fn forget(&self, wanted: impl Fn(&str) -> bool) {
        self.with(|origins| origins.retain(|origin, _| !wanted(origin)));
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
