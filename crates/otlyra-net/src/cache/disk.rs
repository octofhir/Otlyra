//! The cache, kept where a restart cannot reach it.
//!
//! # Why a browser needs this and not only the one in memory
//!
//! The memory cache answers the second request for the same picture on the same
//! page, which is the win that shows up in a profile. What a *person* notices is
//! different and slower: every site they visit every day is fetched from nothing
//! every time the browser starts. A logo that has not changed in a year, a
//! stylesheet, a font — all of it, again, on every launch, over whatever
//! connection they happen to be on.
//!
//! # One file per entry
//!
//! Not one index file and a directory of bodies. Two files that have to agree is
//! a pair that will one day not — a crash between the two writes, and the index
//! names a body that is not there or misses one that is. Here an entry is one
//! file with its own metadata in front of its own body: it is either whole or it
//! is not there, and a file that will not parse is dropped rather than repaired.
//!
//! The index is rebuilt at startup by reading the *headers* of those files and
//! not their bodies, so opening a cache of a thousand pictures costs a thousand
//! short reads and no megabytes.
//!
//! # What is written where
//!
//! The file is named for a hash of the address, and the address is written
//! *inside* it. Two addresses that hash alike would otherwise be one entry
//! silently answering for the other; instead a read checks the address it found
//! against the one it wanted, and a mismatch is a miss. A cache that loses an
//! entry is doing its job badly for a moment. A cache that hands back the wrong
//! body is a bug that looks like a haunted page.
//!
//! # What this is not protected against
//!
//! The bodies are written in the clear, which is what every browser does with its
//! HTTP cache and is worth stating rather than leaving to be discovered. A cached
//! page from a site somebody is signed in to is readable by anything that can
//! read their files. The directory is created private to its owner; that is the
//! whole of the protection, and it is the same protection the browser's other
//! stores rely on except the cookie jar — which is sealed, because a session is
//! the one thing worth a key. See [`crate::cache`] for what is stored at all: a
//! response the server marked `no-store` never reaches here.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::policy::Times;
use super::store::Stored;

/// What the format is called, and which version of it this is.
///
/// Read before anything else. A directory written by a later build is left alone
/// rather than half-understood: an entry that parses as something it is not is
/// worse than one that is refetched.
const MAGIC: &[u8; 12] = b"OTLYRACACHE\x01";

/// What the disk holds, and where.
///
/// # Why the writing happens somewhere else
///
/// This lives inside [`super::store::Cache`], which lives inside one mutex that
/// every fetch and the browser's own thread take. Anything done here is done with
/// that mutex held. A `write` that called [`std::fs::write`] would therefore hold
/// the one cache lock across a syscall — and the fetches run on a two-worker
/// runtime, so two of them writing at once would park every worker in the process
/// and stall the reactor, the timers and the four other permitted fetches with it.
/// The browser's thread pays it too: clearing a site would unlink a thousand files
/// with the window frozen.
///
/// So nothing here touches a file. The index is kept in memory and every change to
/// the directory is a message to one thread that owns the writing. What is left
/// under the caller's lock is a serialise and a send, which is a memcpy.
///
/// Ordering is the channel's: jobs are applied in the order they were made, so a
/// write followed by a remove leaves no file behind. A read that arrives before
/// its own write has drained finds nothing, drops the entry, and the address is
/// fetched again — a cache is allowed to miss, and the alternative is a lock held
/// over a disk.
#[derive(Debug)]
pub struct Disk {
    dir: PathBuf,
    /// One per file on disk, by the address it answers.
    index: HashMap<String, Entry>,
    /// How many bytes of body may be kept in total.
    capacity: usize,
    /// How many entries may be held, whatever they weigh.
    ///
    /// A budget in bytes alone does not bound a cache of empty bodies: a server
    /// answering a thousand addresses with a cacheable nothing costs no bytes and
    /// a thousand files, and eviction that sorts by size would never reach them.
    entries: usize,
    held: usize,
    /// Where the writing is done. `None` only after [`Disk::close`].
    writer: Option<Sender<Job>>,
    /// How many bytes of body are queued and not yet written.
    ///
    /// The queue is bounded by this rather than by a count, because what would
    /// exhaust memory is one large body repeated and not many small ones. A write
    /// that would go past it is dropped rather than waited for: blocking here is
    /// blocking under the cache lock, which is the whole thing this avoids.
    queued: Arc<AtomicUsize>,
    hand: Option<std::thread::JoinHandle<()>>,
}

/// The most that may be waiting to be written before writes are dropped.
const QUEUE_LIMIT: usize = 32 * 1024 * 1024;

/// How many entries the index holds before the least recently used go.
const ENTRY_LIMIT: usize = 8192;

/// One change to the directory, as the writing thread receives it.
enum Job {
    Write(PathBuf, Vec<u8>, usize),
    Remove(PathBuf),
    /// Answered once everything sent before it has been done. What a test waits
    /// on, and what [`Disk::close`] uses to finish before the process does.
    Sync(Sender<()>),
}

/// One entry, as the index knows it without having read its body.
#[derive(Clone, Debug)]
struct Entry {
    file: PathBuf,
    bytes: usize,
    /// When it was last written or read, which is what eviction sorts by.
    used: SystemTime,
}

impl Disk {
    /// Open the cache kept in `dir`, making it if it is not there.
    ///
    /// Everything already in it that can be read is kept. Anything that cannot —
    /// a file from a later version, a write that a crash cut in half, something
    /// that is not ours at all — is left where it is and not indexed, so it is
    /// neither served nor deleted.
    pub fn open(dir: impl AsRef<Path>, capacity: usize) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        private(&dir);

        let queued = Arc::new(AtomicUsize::new(0));
        let (writer, jobs) = channel::<Job>();
        let hand = {
            let queued = Arc::clone(&queued);
            std::thread::Builder::new()
                .name("otlyra-cache".to_owned())
                .spawn(move || {
                    for job in jobs {
                        match job {
                            Job::Write(file, bytes, weight) => {
                                if let Err(error) = write_file(&file, &bytes) {
                                    tracing::warn!(
                                        path = %file.display(),
                                        %error,
                                        "the cache could not be written to"
                                    );
                                }
                                queued.fetch_sub(weight, Ordering::Relaxed);
                            }
                            Job::Remove(file) => {
                                let _ = std::fs::remove_file(&file);
                            }
                            // Dropping the sender is the answer: a caller waiting
                            // on it is woken by the disconnect just as well.
                            Job::Sync(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                })?
        };

        let mut disk = Self {
            dir,
            index: HashMap::new(),
            capacity,
            entries: ENTRY_LIMIT,
            held: 0,
            writer: Some(writer),
            queued,
            hand: Some(hand),
        };
        disk.scan();
        disk.make_room();
        Ok(disk)
    }

    /// Hand one change to the thread that owns the writing.
    fn send(&self, job: Job) {
        if let Some(writer) = self.writer.as_ref() {
            let _ = writer.send(job);
        }
    }

    /// Wait until everything asked for so far has reached the disk.
    ///
    /// For a caller that is about to look at the directory itself — a test, and
    /// the shutdown path. Nothing on the loading path waits for this.
    pub fn settle(&self) {
        let (done, wait) = channel();
        self.send(Job::Sync(done));
        let _ = wait.recv();
    }

    /// Finish every queued write and stop the thread.
    ///
    /// Called by `Drop`, and safe to call before it. What it buys is that closing
    /// the browser keeps what the last page put in the cache.
    pub fn close(&mut self) {
        self.writer = None;
        if let Some(hand) = self.hand.take() {
            let _ = hand.join();
        }
    }

    /// How many entries are held, and how many bytes of body.
    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The bytes of body held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.held
    }

    /// Every address held, with the size of the body kept for it.
    ///
    /// Read out of the index, so listing a cache of a thousand entries costs no
    /// file reads at all — which is what makes it safe for a page that redraws.
    pub fn held(&self) -> impl Iterator<Item = (&str, usize)> {
        self.index
            .iter()
            .map(|(url, entry)| (url.as_str(), entry.bytes))
    }

    /// Whether there is an entry for `url` without reading its body.
    ///
    /// What a caller asks before deciding to pay for the read.
    #[must_use]
    pub fn holds(&self, url: &str) -> bool {
        self.index.contains_key(url)
    }

    /// Which file answers for `url`, without opening it.
    ///
    /// The index half of a read, which is a hash lookup. The other half is
    /// [`Disk::read_at`], and they are apart so that a caller holding a lock can
    /// do this one under it and the file read outside it.
    #[must_use]
    pub fn file_for(&self, url: &str) -> Option<PathBuf> {
        self.index.get(url).map(|entry| entry.file.clone())
    }

    /// Read one entry file, whole. Answers the address it was stored under.
    ///
    /// An associated function rather than a method because it touches no index
    /// and needs no `Disk`: the point of it is to be callable with every lock
    /// dropped. The caller checks the address against the one it wanted — see
    /// [`Disk::read`], which is this with the checking done.
    #[must_use]
    pub fn read_at(file: &Path) -> Option<(String, Stored)> {
        read_file(file)
    }

    /// Note that `url` was used just now, so eviction sorts it last.
    pub fn mark_used(&mut self, url: &str) {
        if let Some(entry) = self.index.get_mut(url) {
            entry.used = SystemTime::now();
        }
    }

    /// Read the entry for `url` back, whole.
    ///
    /// Does the file read itself, so a caller holding a lock wants
    /// [`Disk::file_for`] and [`Disk::read_at`] instead.
    pub fn read(&mut self, url: &str) -> Option<Stored> {
        let entry = self.index.get(url)?.clone();
        match read_file(&entry.file) {
            // The address inside the file is the one that decides. A hash
            // collision would otherwise be one entry answering for another.
            Some((key, stored)) if key == url => {
                if let Some(entry) = self.index.get_mut(url) {
                    entry.used = SystemTime::now();
                }
                Some(stored)
            }
            _ => {
                tracing::debug!(%url, "a cache file did not answer for its address");
                self.forget(url);
                None
            }
        }
    }

    /// Write `stored` down as the answer for `url`.
    ///
    /// A write that fails is a cache that did not keep something, which is not an
    /// error a caller can do anything about: the response is already in hand and
    /// the page is already being drawn. Reported once, and then forgotten.
    pub fn write(&mut self, url: &str, stored: &Stored) {
        if stored.body.len() > self.capacity {
            return;
        }
        // The serialising happens here, under whatever lock the caller holds,
        // because it is a memcpy and moving it would mean holding the response
        // alive for the writer as well. The syscalls are what goes elsewhere.
        let encoded = encode(url, stored);
        let bytes = stored.body.len();
        let weight = encoded.len();

        // A cache is allowed not to keep something. Waiting here would be waiting
        // under the cache lock, which is what the writing thread exists to avoid.
        if self.queued.load(Ordering::Relaxed) + weight > QUEUE_LIMIT {
            tracing::debug!(%url, "the cache is writing too slowly to keep this");
            return;
        }
        self.queued.fetch_add(weight, Ordering::Relaxed);

        let file = self.dir.join(name_for(url));
        self.send(Job::Write(file.clone(), encoded, weight));
        if let Some(gone) = self.index.insert(
            url.to_owned(),
            Entry {
                file,
                bytes,
                used: SystemTime::now(),
            },
        ) {
            self.held = self.held.saturating_sub(gone.bytes);
        }
        self.held += bytes;
        self.make_room();
    }

    /// Forget one address, and take its file with it.
    pub fn forget(&mut self, url: &str) -> bool {
        let Some(entry) = self.index.remove(url) else {
            return false;
        };
        self.held = self.held.saturating_sub(entry.bytes);
        self.send(Job::Remove(entry.file));
        true
    }

    /// Forget everything one site is keeping. Answers how many went.
    pub fn forget_site(&mut self, site: &str) -> usize {
        let doomed: Vec<String> = self
            .index
            .keys()
            .filter(|url| super::store::site_of(url).as_deref() == Some(site))
            .cloned()
            .collect();
        let count = doomed.len();
        for url in doomed {
            self.forget(&url);
        }
        count
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        for (_, entry) in std::mem::take(&mut self.index) {
            self.send(Job::Remove(entry.file));
        }
        self.held = 0;
    }

    /// Read the headers of everything in the directory, and nothing else.
    fn scan(&mut self) {
        let Ok(listing) = std::fs::read_dir(&self.dir) else {
            return;
        };
        // One buffer for the whole scan. A fresh 64 KB per file would be a third
        // of a gigabyte of zeroing to open a cache of five thousand entries, all
        // of it on the way to the first frame.
        let mut front = vec![0u8; HEADER_READ];
        for entry in listing.flatten() {
            let file = entry.path();
            if !file.is_file() {
                continue;
            }
            match read_header(&file, &mut front) {
                Some((url, bytes, used)) => {
                    self.held += bytes;
                    self.index.insert(url, Entry { file, bytes, used });
                }
                // Not ours, from a later version, or cut in half by a crash.
                // Left where it is: deleting a file this build cannot read would
                // be deleting whatever wrote it.
                None => {
                    tracing::debug!(file = %file.display(), "not a cache entry this build reads")
                }
            }
        }
        tracing::debug!(
            entries = self.index.len(),
            bytes = self.held,
            "cache opened"
        );
    }

    /// Evict, oldest use first, until both budgets are met.
    ///
    /// Both, because they bound different things: the bytes stop a cache of
    /// pictures from taking somebody's disk, and the count stops a cache of empty
    /// bodies from taking their inodes. A response with no body costs nothing
    /// against the first and would never be evicted by it.
    fn make_room(&mut self) {
        if self.held <= self.capacity && self.index.len() <= self.entries {
            return;
        }
        let mut oldest: Vec<(SystemTime, String)> = self
            .index
            .iter()
            .map(|(url, entry)| (entry.used, url.clone()))
            .collect();
        oldest.sort_unstable();
        for (_, url) in oldest {
            if self.held <= self.capacity && self.index.len() <= self.entries {
                break;
            }
            self.forget(&url);
        }
    }
}

impl Drop for Disk {
    /// Finish what was queued rather than abandoning it.
    ///
    /// A browser being closed has usually just written the page it was showing,
    /// and dropping that on the way out would make the cache worth least exactly
    /// when it is worth most — the next launch.
    fn drop(&mut self) {
        self.close();
    }
}

/// Make a directory readable by its owner and nobody else.
///
/// The whole of what protects a cache of pages somebody was signed in to. Best
/// effort: a filesystem that cannot express it — a mounted share — leaves the
/// cache no worse off than the rest of the profile beside it.
fn private(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// What one address's file is called.
///
/// A hash, because an address is not a filename: it is longer than a filename may
/// be, it contains separators, and two that differ only in case would be one file
/// on a Mac. The address itself is written inside, which is what makes a
/// collision a miss rather than a wrong answer.
fn name_for(url: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}.entry", hasher.finish())
}

// --- the file ---------------------------------------------------------------
//
// Length-prefixed throughout, and binary rather than lines of text. A header
// value may hold anything a server chose to send, and a format with a separator
// in it is a format with an escaping rule — which is one more thing to get wrong
// than a length is. Times are seconds since the epoch, which is what they are.
//
// `Directives` and `Lifetime` are *not* written. Both are read out of the headers
// and the times, so writing them would be writing a second copy of something
// already here — and a second copy is a thing that can disagree with the first.

/// One entry as the bytes that will be on the disk.
///
/// Apart from the writing so that the serialising can happen under the caller's
/// lock and the syscalls cannot: what crosses to the writing thread is a finished
/// buffer, not a borrow of a response somebody else is about to drop.
fn encode(url: &str, stored: &Stored) -> Vec<u8> {
    let mut out = Vec::with_capacity(stored.body.len() + 512);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&stored.status.to_be_bytes());
    out.push(u8::from(stored.varies_on_everything));
    for time in [
        stored.times.requested,
        stored.times.received,
        stored.times.date,
    ] {
        out.extend_from_slice(&seconds(time).to_be_bytes());
    }
    out.extend_from_slice(&stored.times.age.as_secs().to_be_bytes());
    put_str(&mut out, url);
    put_str(&mut out, &stored.final_url);
    put_pairs(&mut out, &stored.headers);
    put_pairs(&mut out, &stored.varied);
    out.extend_from_slice(&(stored.body.len() as u64).to_be_bytes());
    out.extend_from_slice(&stored.body);
    out
}

/// Put one encoded entry where it belongs. Runs on the writing thread only.
fn write_file(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Written beside and renamed over, like every other store here: a crash
    // partway through a write must not leave half an entry where a whole one was.
    let temporary = file.with_extension("writing");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, file)
}

/// Enough for the fixed part and a generous set of headers. An entry whose
/// headers are longer than this is not indexed, which is rare and safe.
const HEADER_READ: usize = 64 * 1024;

/// The address and the body's size, without reading the body.
///
/// `front` is the caller's scratch buffer, reused across a scan.
fn read_header(file: &Path, front: &mut [u8]) -> Option<(String, usize, SystemTime)> {
    let mut handle = std::fs::File::open(file).ok()?;
    let read = handle.read(front).ok()?;
    let front = &front[..read];

    let mut at = 0;
    if front.len() < MAGIC.len() || &front[..MAGIC.len()] != MAGIC {
        return None;
    }
    at += MAGIC.len();
    at += 2 + 1 + 8 * 4;
    let url = take_str(front, &mut at)?;
    let _final_url = take_str(front, &mut at)?;
    let _headers = take_pairs(front, &mut at)?;
    let _varied = take_pairs(front, &mut at)?;
    let body_len = take_u64(front, &mut at)? as usize;

    let used = handle
        .metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .unwrap_or_else(SystemTime::now);
    // The seek confirms the file really is as long as it claims: a body cut short
    // by a crash would otherwise be indexed and then served truncated.
    let end = handle.seek(SeekFrom::End(0)).ok()?;
    if (at as u64) + (body_len as u64) != end {
        return None;
    }
    Some((url, body_len, used))
}

fn read_file(file: &Path) -> Option<(String, Stored)> {
    let bytes = std::fs::read(file).ok()?;
    let mut at = 0;
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return None;
    }
    at += MAGIC.len();

    let status = u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]);
    at += 2;
    let varies_on_everything = *bytes.get(at)? != 0;
    at += 1;
    let requested = instant(take_u64(&bytes, &mut at)?);
    let received = instant(take_u64(&bytes, &mut at)?);
    let date = instant(take_u64(&bytes, &mut at)?);
    let age = Duration::from_secs(take_u64(&bytes, &mut at)?);

    let url = take_str(&bytes, &mut at)?;
    let final_url = take_str(&bytes, &mut at)?;
    let headers = take_pairs(&bytes, &mut at)?;
    let varied = take_pairs(&bytes, &mut at)?;
    let body_len = take_u64(&bytes, &mut at)? as usize;
    let body = bytes.get(at..at + body_len)?.to_vec();

    let times = Times {
        requested,
        received,
        date,
        age,
    };
    // Worked out again rather than read back, so what this entry is good for is
    // decided by the same code that decided it when it arrived.
    let directives = super::policy::Directives::parse(
        headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
            .map(|(_, value)| value.as_str()),
    );
    let header = |name: &str| {
        headers
            .iter()
            .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let lifetime = super::policy::lifetime(
        directives,
        header("expires"),
        header("last-modified"),
        times,
    );

    Some((
        url,
        Stored {
            status,
            headers,
            body,
            final_url,
            directives,
            lifetime,
            times,
            varied,
            varies_on_everything,
        },
    ))
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

fn instant(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn put_pairs(out: &mut Vec<u8>, pairs: &[(String, String)]) {
    out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
    for (name, value) in pairs {
        put_str(out, name);
        put_str(out, value);
    }
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let slice: [u8; 8] = bytes.get(*at..*at + 8)?.try_into().ok()?;
    *at += 8;
    Some(u64::from_be_bytes(slice))
}

fn take_u32(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let slice: [u8; 4] = bytes.get(*at..*at + 4)?.try_into().ok()?;
    *at += 4;
    Some(u32::from_be_bytes(slice))
}

fn take_str(bytes: &[u8], at: &mut usize) -> Option<String> {
    let len = take_u32(bytes, at)? as usize;
    let text = String::from_utf8(bytes.get(*at..*at + len)?.to_vec()).ok()?;
    *at += len;
    Some(text)
}

fn take_pairs(bytes: &[u8], at: &mut usize) -> Option<Vec<(String, String)>> {
    let count = take_u32(bytes, at)? as usize;
    let mut pairs = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        pairs.push((take_str(bytes, at)?, take_str(bytes, at)?));
    }
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(body: &[u8], max_age: u64) -> Stored {
        let times = Times {
            requested: SystemTime::now(),
            received: SystemTime::now(),
            date: SystemTime::now(),
            age: Duration::ZERO,
        };
        let headers = vec![
            ("Cache-Control".to_owned(), format!("max-age={max_age}")),
            ("Content-Type".to_owned(), "text/plain".to_owned()),
        ];
        let directives = super::super::policy::Directives::parse(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
                .map(|(_, value)| value.as_str()),
        );
        Stored {
            status: 200,
            headers,
            body: body.to_vec(),
            final_url: "https://a.example/x".to_owned(),
            directives,
            lifetime: super::super::policy::lifetime(directives, None, None, times),
            times,
            varied: Vec::new(),
            varies_on_everything: false,
        }
    }

    fn somewhere() -> PathBuf {
        let at = std::env::temp_dir().join(format!(
            "otlyra-cache-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&at);
        at
    }

    #[test]
    fn what_was_written_is_what_comes_back() {
        let dir = somewhere();
        let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
        disk.write("https://a.example/x", &stored(b"the body", 3600));
        // The writing happens on a thread of its own, so a test that wants to see
        // the file rather than the index says so. Nothing on the loading path
        // waits like this: what was just written is in the memory tier in front.
        disk.settle();

        let back = disk.read("https://a.example/x").expect("an entry");
        assert_eq!(back.body, b"the body");
        assert_eq!(back.status, 200);
        assert_eq!(back.header("content-type"), Some("text/plain"));
        // Worked out again from the headers rather than written down, so it
        // cannot have drifted from them.
        assert_eq!(back.directives.max_age, Some(3600));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_a_restart_finds_is_what_the_last_run_left() {
        // The whole point: a browser that has been closed and opened again does
        // not fetch the same logo from nothing.
        let dir = somewhere();
        {
            let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
            disk.write("https://a.example/one", &stored(b"first", 3600));
            disk.write("https://a.example/two", &stored(b"second", 3600));
        }

        let mut reopened = Disk::open(&dir, 1024 * 1024).expect("reopened");
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.bytes(), b"first".len() + b"second".len());
        assert_eq!(
            reopened
                .read("https://a.example/two")
                .expect("an entry")
                .body,
            b"second"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_that_does_not_answer_for_its_address_is_a_miss() {
        let dir = somewhere();
        let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
        disk.write("https://a.example/x", &stored(b"body", 3600));
        disk.settle();

        // What a hash collision would look like from the inside. Handing this
        // back would be a page haunted by another page's bytes.
        let file = dir.join(name_for("https://a.example/x"));
        disk.index.insert(
            "https://other.example/y".to_owned(),
            Entry {
                file,
                bytes: 4,
                used: SystemTime::now(),
            },
        );
        assert!(disk.read("https://other.example/y").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_cut_in_half_is_not_indexed() {
        let dir = somewhere();
        {
            let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
            disk.write("https://a.example/x", &stored(b"a good long body", 3600));
        }
        // A crash between the write and the rename cannot produce this, but a
        // full disk can, and a truncated body served as whole is a broken page
        // with nothing to say why.
        let file = dir.join(name_for("https://a.example/x"));
        let bytes = std::fs::read(&file).expect("read");
        std::fs::write(&file, &bytes[..bytes.len() - 4]).expect("written");

        assert!(Disk::open(&dir, 1024 * 1024).expect("opened").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn something_that_is_not_ours_is_left_alone_rather_than_deleted() {
        let dir = somewhere();
        std::fs::create_dir_all(&dir).expect("made");
        let stranger = dir.join("please-do-not-delete-me");
        std::fs::write(&stranger, b"not a cache entry").expect("written");

        let disk = Disk::open(&dir, 1024 * 1024).expect("opened");
        assert!(disk.is_empty());
        assert!(
            stranger.exists(),
            "a file this build cannot read is not its to remove"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_budget_is_kept_by_dropping_what_was_used_longest_ago() {
        let dir = somewhere();
        let mut disk = Disk::open(&dir, 16).expect("opened");
        disk.write("https://a.example/one", &stored(b"aaaaaaaa", 3600));
        std::thread::sleep(std::time::Duration::from_millis(10));
        disk.write("https://a.example/two", &stored(b"bbbbbbbb", 3600));
        std::thread::sleep(std::time::Duration::from_millis(10));
        disk.write("https://a.example/three", &stored(b"cccccccc", 3600));

        assert!(disk.bytes() <= 16, "held {}", disk.bytes());
        assert!(
            !disk.holds("https://a.example/one"),
            "the oldest should have gone"
        );
        assert!(disk.holds("https://a.example/three"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A budget in bytes does not bound a cache of empty bodies. A server that
    /// answers a thousand addresses with a cacheable nothing costs no bytes at
    /// all, and eviction that sorts by size would never reach any of them.
    #[test]
    fn a_cache_of_empty_bodies_is_still_bounded() {
        let dir = somewhere();
        let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
        disk.entries = 4;
        for index in 0..32 {
            disk.write(&format!("https://a.example/{index}"), &stored(b"", 3600));
        }

        assert_eq!(disk.bytes(), 0, "empty bodies weigh nothing");
        assert!(
            disk.len() <= 4,
            "the index grew to {} with no bytes to evict on",
            disk.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason the writing is somewhere else: a write must not be a
    /// syscall on the thread that holds the cache lock. Timing is a poor thing to
    /// assert on, so what is checked is that the queue drains to nothing — which
    /// is only true if the writing thread, and not the caller, did it.
    #[test]
    fn writing_does_not_happen_on_the_calling_thread() {
        let dir = somewhere();
        let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
        for index in 0..16 {
            disk.write(
                &format!("https://a.example/{index}"),
                &stored(&[b'x'; 4096], 3600),
            );
        }
        // Queued and accounted for, whether or not any of it has landed yet.
        disk.settle();
        assert_eq!(
            disk.queued.load(Ordering::Relaxed),
            0,
            "the writing thread did not account for what it wrote"
        );
        assert_eq!(disk.len(), 16);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_takes_the_file_with_it() {
        let dir = somewhere();
        let mut disk = Disk::open(&dir, 1024 * 1024).expect("opened");
        disk.write("https://a.example/x", &stored(b"body", 3600));
        disk.settle();
        let file = dir.join(name_for("https://a.example/x"));
        assert!(file.exists());

        disk.forget("https://a.example/x");
        disk.settle();
        assert!(
            !file.exists(),
            "a forgotten entry must not be left on the disk"
        );
        assert!(disk.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
