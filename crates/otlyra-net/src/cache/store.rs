//! What has been kept, and how to find it again.
//!
//! One entry per address, which is the shape a browser cache actually has: a
//! server that answers differently to different requests says so in `Vary`, and
//! this browser sends so nearly the same headers every time that a second entry
//! under one address would be a second copy of the same bytes. Where `Vary` names
//! a header we did not match on, the entry is not used — which costs a fetch and
//! never serves the wrong body.
//!
//! Held in memory and not on disk. A cache that survives the process is worth
//! having and is a different problem: it needs a format, a size budget measured
//! against a real disk, and an answer to what happens when two windows write at
//! once. What it is *not* is a prerequisite — the fetch a reader notices is the
//! second one on the same page, and that one is this.

use std::collections::HashMap;
use std::time::SystemTime;

use super::policy::{Directives, Lifetime, Times, Use};

/// One response, as it was when it arrived.
#[derive(Clone, Debug)]
pub struct Stored {
    /// The status the server answered with.
    pub status: u16,
    /// Every header it carried, in the order it sent them.
    pub headers: Vec<(String, String)>,
    /// The body, decompressed, as [`crate::LoadedResource`] holds it.
    pub body: Vec<u8>,
    /// The address it finally came from, which a redirect may have changed.
    pub final_url: String,
    /// What its `Cache-Control` asked for.
    pub directives: Directives,
    /// How long it is good for, and on whose say-so.
    pub lifetime: Lifetime,
    /// The clocks it is judged against.
    pub times: Times,
    /// The request headers `Vary` named, with what they were when this was
    /// stored. Empty where nothing varied.
    pub varied: Vec<(String, String)>,
    /// Whether `Vary: *` was said, which means it may never be reused.
    pub varies_on_everything: bool,
}

impl Stored {
    /// The first value of `name` among the response headers.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// This entry as the response it was.
    ///
    /// The request headers are empty rather than invented: what was sent was sent
    /// on a request that is over, and a pane showing this one's headers would be
    /// showing a request nobody made. That a response came from here is the
    /// interesting fact, and it is the caller's to report.
    pub fn as_resource(&self) -> crate::LoadedResource {
        let header = |name: &str| {
            self.headers
                .iter()
                .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        crate::LoadedResource {
            final_url: self.final_url.clone(),
            status: self.status,
            content_type: header("content-type"),
            nosniff: header("x-content-type-options")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("nosniff")),
            request_headers: Vec::new(),
            response_headers: self.headers.clone(),
            body: self.body.clone(),
        }
    }

    /// Whether there is anything to ask the server with.
    ///
    /// An `ETag` or a `Last-Modified`. Without one there is no such thing as
    /// revalidating this: the only way to learn whether it changed is to fetch it.
    pub fn has_validator(&self) -> bool {
        self.header("etag").is_some() || self.header("last-modified").is_some()
    }

    /// The conditional headers a revalidation of this would carry.
    ///
    /// Both where both are known, which is what browsers send: a server that
    /// ignores one may honour the other, and sending both costs two short
    /// headers.
    pub fn conditions(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(tag) = self.header("etag") {
            out.push(("if-none-match", tag.to_owned()));
        }
        if let Some(when) = self.header("last-modified") {
            out.push(("if-modified-since", when.to_owned()));
        }
        out
    }

    /// Whether this entry answers a request carrying `request_headers`.
    ///
    /// `Vary` is the server saying *my answer depends on these*. An entry stored
    /// under one set of them does not answer a request with another, and
    /// `Vary: *` says the answer depends on something not in the headers at all,
    /// which is a way of saying never reuse this.
    pub fn answers(&self, request_headers: &[(String, String)]) -> bool {
        if self.varies_on_everything {
            return false;
        }
        self.varied
            .iter()
            .all(|(name, was)| header_of(request_headers, name).unwrap_or_default() == was.as_str())
    }

    /// Take what a `304 Not Modified` said and make this current again.
    ///
    /// The headers on a `304` replace the ones they name and leave the rest — a
    /// server answering *not changed* is allowed to send a new `Cache-Control`
    /// and usually sends a new `Date`, and taking only the body's word for it
    /// would leave the entry as stale a second later as it was a second before.
    pub fn refresh(&mut self, headers: &[(String, String)], times: Times) {
        for (name, value) in headers {
            match self
                .headers
                .iter_mut()
                .find(|(had, _)| had.eq_ignore_ascii_case(name))
            {
                Some((_, held)) => held.clone_from(value),
                None => self.headers.push((name.clone(), value.clone())),
            }
        }
        self.times = times;
        self.directives = Directives::parse(
            self.headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
                .map(|(_, value)| value.clone()),
        );
        self.lifetime = super::policy::lifetime(
            self.directives,
            self.header("expires"),
            self.header("last-modified"),
            times,
        );
    }
}

/// The names `Vary` listed, lowercased. `None` where it said `*`.
fn varied_names(headers: &[(String, String)]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("vary") {
            continue;
        }
        for listed in value.split(',') {
            let listed = listed.trim();
            if listed == "*" {
                return None;
            }
            if !listed.is_empty() {
                names.push(listed.to_ascii_lowercase());
            }
        }
    }
    Some(names)
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// How much the cache will hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// Bytes of body, in total. What is counted is the bodies: the headers are a
    /// rounding error beside a picture and counting them exactly would be
    /// pretending to a precision the eviction does not have.
    pub bytes: usize,
    /// The largest single response worth keeping.
    ///
    /// One response must not be able to empty the cache of everything else. A
    /// video is streamed past a cache this size anyway.
    pub largest: usize,
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            bytes: 64 * 1024 * 1024,
            largest: 8 * 1024 * 1024,
        }
    }
}

/// What has been fetched already.
#[derive(Debug)]
pub struct Cache {
    entries: HashMap<String, Stored>,
    /// The order they were last used in, oldest first. What eviction reads.
    order: Vec<String>,
    capacity: Capacity,
    held: usize,
    hits: u64,
    misses: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    /// An empty cache with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(Capacity::default())
    }

    /// An empty cache that will hold what `capacity` says.
    pub fn with_capacity(capacity: Capacity) -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            capacity,
            held: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// How many bodies are held, and how many bytes of them.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of body held.
    pub fn bytes(&self) -> usize {
        self.held
    }

    /// How often a request found something, and how often it did not.
    ///
    /// A cache with no counters is a cache nobody can tell is working: the whole
    /// of what it does is invisible except as speed.
    pub fn counts(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Throw everything away.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.held = 0;
    }

    /// What may be done about a request for `url`, given the headers it will
    /// carry.
    ///
    /// Counted: this is the one place a hit and a miss can be told apart.
    pub fn look_up(
        &mut self,
        url: &str,
        request_headers: &[(String, String)],
        now: SystemTime,
    ) -> Option<(&Stored, Use)> {
        let stored = self.entries.get(url)?;
        if !stored.answers(request_headers) {
            self.misses += 1;
            return None;
        }
        let use_ = super::policy::use_of(
            stored.directives,
            stored.lifetime,
            stored.has_validator(),
            stored.times,
            now,
        );
        if use_ == Use::Refetch {
            self.misses += 1;
            return None;
        }
        if use_ == Use::Fresh {
            self.hits += 1;
        }
        // Used is used, whether it was served or only asked about.
        self.touch(url);
        self.entries.get(url).map(|stored| (stored, use_))
    }

    /// Keep a response, if it is one worth keeping.
    ///
    /// Answers whether it was kept, which is what a test asserts on and what an
    /// inspector would show.
    pub fn store(
        &mut self,
        url: &str,
        method: &str,
        stored: Stored,
        request_headers: &[(String, String)],
    ) -> bool {
        if !super::policy::may_store(
            method,
            stored.status,
            stored.directives,
            stored.lifetime,
            stored.has_validator(),
        ) {
            // A response that may not be kept replaces one that was: the server
            // has just said something new about this address, and leaving the old
            // body behind would serve it again.
            self.remove(url);
            return false;
        }
        if stored.body.len() > self.capacity.largest {
            self.remove(url);
            return false;
        }
        // `Vary` is recorded against the request that was actually made, because
        // that is what a later request has to match.
        let mut stored = stored;
        match varied_names(&stored.headers) {
            Some(names) => {
                stored.varied = names
                    .into_iter()
                    .map(|name| {
                        let was = header_of(request_headers, &name).unwrap_or_default();
                        (name, was.to_owned())
                    })
                    .collect();
                stored.varies_on_everything = false;
            }
            None => {
                stored.varies_on_everything = true;
                stored.varied.clear();
            }
        }

        self.remove(url);
        self.held += stored.body.len();
        self.entries.insert(url.to_owned(), stored);
        self.order.push(url.to_owned());
        self.make_room();
        true
    }

    /// Bring an entry up to date from a `304`, and say whether there was one.
    pub fn refresh(&mut self, url: &str, headers: &[(String, String)], times: Times) -> bool {
        let Some(stored) = self.entries.get_mut(url) else {
            return false;
        };
        stored.refresh(headers, times);
        self.hits += 1;
        self.touch(url);
        true
    }

    /// Forget one address.
    pub fn remove(&mut self, url: &str) -> bool {
        let Some(stored) = self.entries.remove(url) else {
            return false;
        };
        self.held = self.held.saturating_sub(stored.body.len());
        self.order.retain(|held| held != url);
        true
    }

    fn touch(&mut self, url: &str) {
        self.order.retain(|held| held != url);
        self.order.push(url.to_owned());
    }

    /// Evict until the cache is inside its capacity, least recently used first.
    fn make_room(&mut self) {
        while self.held > self.capacity.bytes {
            let Some(oldest) = self.order.first().cloned() else {
                break;
            };
            if !self.remove(&oldest) {
                // The order and the entries disagreed, which cannot happen — but
                // looping forever over it could, so the stale name goes.
                self.order.retain(|held| *held != oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn times() -> Times {
        Times {
            requested: now(),
            received: now(),
            date: now(),
            age: Duration::ZERO,
        }
    }

    fn pairs(headers: &[(&str, &str)]) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn response(headers: &[(&str, &str)], body: &[u8]) -> Stored {
        let headers = pairs(headers);
        let directives = Directives::parse(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
                .map(|(_, value)| value.clone()),
        );
        let lifetime = super::super::policy::lifetime(
            directives,
            header_of(&headers, "expires"),
            header_of(&headers, "last-modified"),
            times(),
        );
        Stored {
            status: 200,
            headers,
            body: body.to_vec(),
            final_url: "https://x.test/a".to_owned(),
            directives,
            lifetime,
            times: times(),
            varied: Vec::new(),
            varies_on_everything: false,
        }
    }

    #[test]
    fn what_is_kept_is_found_again() {
        let mut cache = Cache::new();
        assert!(cache.store(
            "https://x.test/a",
            "GET",
            response(&[("cache-control", "max-age=3600")], b"body"),
            &[]
        ));
        let (found, use_) = cache
            .look_up("https://x.test/a", &[], now())
            .expect("a hit");
        assert_eq!(found.body, b"body");
        assert_eq!(use_, Use::Fresh);
        assert_eq!(cache.counts(), (1, 0));
        assert_eq!(cache.bytes(), 4);
    }

    /// Once stale it is asked about rather than served, and asking is only
    /// possible where there is something to ask with.
    #[test]
    fn a_stale_entry_is_revalidated_or_forgotten() {
        let mut cache = Cache::new();
        cache.store(
            "https://x.test/tagged",
            "GET",
            response(
                &[("cache-control", "max-age=60"), ("etag", "\"v1\"")],
                b"body",
            ),
            &[],
        );
        let later = now() + Duration::from_secs(61);
        let (found, use_) = cache
            .look_up("https://x.test/tagged", &[], later)
            .expect("still there");
        assert_eq!(use_, Use::Revalidate);
        assert_eq!(
            found.conditions(),
            vec![("if-none-match", "\"v1\"".to_owned())]
        );

        // The same, with nothing to ask with: not a hit, and not a revalidation.
        cache.store(
            "https://x.test/bare",
            "GET",
            response(&[("cache-control", "max-age=60")], b"body"),
            &[],
        );
        assert!(cache.look_up("https://x.test/bare", &[], later).is_none());
    }

    /// A `304` replaces the headers it names and leaves the rest, which is what
    /// makes the entry fresh again rather than stale a second later.
    #[test]
    fn a_not_modified_makes_it_current() {
        let mut cache = Cache::new();
        cache.store(
            "https://x.test/a",
            "GET",
            response(
                &[("cache-control", "max-age=60"), ("etag", "\"v1\"")],
                b"body",
            ),
            &[],
        );
        let later = now() + Duration::from_secs(61);
        let refreshed = Times {
            requested: later,
            received: later,
            date: later,
            age: Duration::ZERO,
        };
        assert!(cache.refresh(
            "https://x.test/a",
            &pairs(&[("cache-control", "max-age=600"), ("etag", "\"v2\"")]),
            refreshed
        ));

        // Well past the *old* sixty seconds and inside the new six hundred. This
        // is the assertion that means anything: resetting the clock alone would
        // make it fresh here, so the length has to come from the `304`'s own
        // header or the entry is stale again a minute later.
        let inside_the_new = later + Duration::from_secs(300);
        let (found, use_) = cache
            .look_up("https://x.test/a", &[], inside_the_new)
            .expect("a hit");
        assert_eq!(use_, Use::Fresh, "fresh for the length the 304 gave");
        assert_eq!(found.body, b"body", "the body was never re-sent");
        assert_eq!(
            found.header("etag"),
            Some("\"v2\""),
            "and a header the 304 named replaced the one it named"
        );
        assert_eq!(
            found.header("cache-control"),
            Some("max-age=600"),
            "not appended beside the old one"
        );

        // And past the new length it is asked about again.
        assert_eq!(
            cache
                .look_up("https://x.test/a", &[], later + Duration::from_secs(601))
                .expect("still there")
                .1,
            Use::Revalidate
        );
    }

    /// `Vary` is the server saying *my answer depends on these*. An entry stored
    /// under one set of them does not answer a request with another.
    #[test]
    fn an_entry_only_answers_the_request_it_was_stored_for() {
        let mut cache = Cache::new();
        cache.store(
            "https://x.test/a",
            "GET",
            response(
                &[
                    ("cache-control", "max-age=3600"),
                    ("vary", "Accept-Language"),
                ],
                b"english",
            ),
            &pairs(&[("accept-language", "en")]),
        );

        assert!(
            cache
                .look_up(
                    "https://x.test/a",
                    &pairs(&[("accept-language", "en")]),
                    now()
                )
                .is_some()
        );
        assert!(
            cache
                .look_up(
                    "https://x.test/a",
                    &pairs(&[("accept-language", "fr")]),
                    now()
                )
                .is_none(),
            "another language is another answer"
        );
        assert!(
            cache.look_up("https://x.test/a", &[], now()).is_none(),
            "and so is none at all"
        );
    }

    /// `Vary: *` says the answer depends on something not in the headers, which
    /// is a way of saying never reuse this.
    #[test]
    fn vary_on_everything_is_never_reused() {
        let mut cache = Cache::new();
        cache.store(
            "https://x.test/a",
            "GET",
            response(&[("cache-control", "max-age=3600"), ("vary", "*")], b"body"),
            &[],
        );
        assert!(cache.look_up("https://x.test/a", &[], now()).is_none());
    }

    /// A response that may not be kept must also unseat the one that was: the
    /// server has just said something new about this address.
    #[test]
    fn a_response_that_may_not_be_kept_removes_what_was() {
        let mut cache = Cache::new();
        cache.store(
            "https://x.test/a",
            "GET",
            response(&[("cache-control", "max-age=3600")], b"old"),
            &[],
        );
        assert_eq!(cache.len(), 1);
        assert!(!cache.store(
            "https://x.test/a",
            "GET",
            response(&[("cache-control", "no-store")], b"new"),
            &[]
        ));
        assert!(cache.is_empty(), "and the old body is gone with it");
        assert_eq!(cache.bytes(), 0);
    }

    /// One response must not be able to empty the cache of everything else.
    #[test]
    fn one_response_too_large_is_not_kept() {
        let mut cache = Cache::with_capacity(Capacity {
            bytes: 1000,
            largest: 100,
        });
        assert!(!cache.store(
            "https://x.test/big",
            "GET",
            response(&[("cache-control", "max-age=3600")], &[b'x'; 101]),
            &[]
        ));
        assert!(cache.is_empty());
    }

    /// Full, the least recently used goes — and looking something up is using it.
    #[test]
    fn eviction_takes_what_was_used_longest_ago() {
        let mut cache = Cache::with_capacity(Capacity {
            bytes: 200,
            largest: 200,
        });
        for name in ["a", "b"] {
            cache.store(
                &format!("https://x.test/{name}"),
                "GET",
                response(&[("cache-control", "max-age=3600")], &[b'x'; 100]),
                &[],
            );
        }
        assert_eq!(cache.len(), 2);

        // Use the older one, which makes the newer one the older.
        cache.look_up("https://x.test/a", &[], now());
        cache.store(
            "https://x.test/c",
            "GET",
            response(&[("cache-control", "max-age=3600")], &[b'x'; 100]),
            &[],
        );

        assert_eq!(cache.len(), 2);
        assert!(cache.look_up("https://x.test/a", &[], now()).is_some());
        assert!(cache.look_up("https://x.test/c", &[], now()).is_some());
        assert!(
            cache.look_up("https://x.test/b", &[], now()).is_none(),
            "it was used longest ago"
        );
        assert!(cache.bytes() <= 200);
    }

    /// Both conditions where both are known: a server that ignores one may honour
    /// the other, and sending both costs two short headers.
    #[test]
    fn a_revalidation_asks_with_everything_it_has() {
        let both = response(
            &[
                ("etag", "\"v1\""),
                ("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ],
            b"body",
        );
        assert_eq!(
            both.conditions(),
            vec![
                ("if-none-match", "\"v1\"".to_owned()),
                (
                    "if-modified-since",
                    "Sun, 06 Nov 1994 08:49:37 GMT".to_owned()
                ),
            ]
        );
        assert!(both.has_validator());
        assert!(!response(&[("cache-control", "max-age=60")], b"body").has_validator());
    }

    #[test]
    fn what_is_thrown_away_is_gone() {
        let mut cache = Cache::new();
        cache.store(
            "https://x.test/a",
            "GET",
            response(&[("cache-control", "max-age=3600")], b"body"),
            &[],
        );
        assert!(cache.remove("https://x.test/a"));
        assert!(!cache.remove("https://x.test/a"));
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);

        cache.store(
            "https://x.test/b",
            "GET",
            response(&[("cache-control", "max-age=3600")], b"body"),
            &[],
        );
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }
}
