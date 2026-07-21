//! Fetching, off the thread the window runs on.
//!
//! The event loop blocks, and a load takes as long as a network takes; doing one
//! on the loop's thread is a frozen window for the duration. So the loader lives on
//! a thread of its own, reachable through two channels and nothing else: requests
//! go out, results come back, and everything that decides what a result *means*
//! stays where the state it changes lives.
//!
//! What crosses the boundary is owned bytes and a request number. No DOM, no style,
//! no fragment: a document parsed on the wrong thread is a document that has to be
//! `Send` forever after.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use otlyra_platform::Waker;

/// What a fetch is for.
///
/// Carried through so a result can be routed without a second table: the browser
/// asks for a document and two dozen subresources, and the reply says which kind
/// it is answering.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    /// The page itself.
    Document,
    /// A stylesheet the page links to.
    Stylesheet,
    /// A picture the page asks for.
    Image,
}

/// What one fetch returned.
#[derive(Debug, Default)]
pub struct Loaded {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// The charset the transport declared, if it declared one.
    pub charset: Option<String>,
    /// The `Content-Type` the transport declared, if it declared one. What the
    /// bytes actually are is decided from this *and* from them, which is sniffing.
    pub content_type: Option<String>,
    /// Whether the transport said not to sniff.
    pub nosniff: bool,
    /// The address it actually came from, after redirects.
    pub final_url: String,
}

/// A finished fetch, good or bad.
#[derive(Debug)]
pub struct Fetched {
    /// The number the request was made under.
    pub id: u64,
    /// What it was for.
    pub kind: ResourceKind,
    /// The address it was asked for at.
    pub url: String,
    /// What came back.
    pub result: Result<Loaded, String>,
}

/// How a tab gets its bytes.
///
/// A trait rather than a direct call to `otlyra-net` for one reason: the browser's
/// behaviour around navigation — which tab, what title, what happens on failure —
/// is worth testing without a socket.
///
/// `Send + Sync` and `&self`, because the pool shares one of these across every
/// fetch thread: a loader holds a client and a connection pool, and one per thread
/// would be several of both.
pub trait Loader: Send + Sync + 'static {
    /// Fetch `url`, returning the bytes and the little the transport knows about
    /// them. What they *are* is decided above this, from the bytes as well.
    fn load(&self, url: &str) -> Result<Loaded, String>;
}

/// How many fetches may be in flight at once.
///
/// Six, which is the number browsers settled on per host over HTTP/1.1: enough that
/// a page of pictures does not arrive one at a time, few enough that a page can
/// point only so many connections at a server. Ours is a total rather than a
/// per-host count, which is stricter and simpler; per-host queues belong with a
/// real connection pool underneath.
pub const FETCH_CONCURRENCY: usize = 6;

/// The handle the browser keeps on the fetch thread.
pub struct Fetcher {
    requests: Sender<Request>,
    results: Receiver<Fetched>,
    /// Set once the platform hands one over, and shared with the fetch thread so a
    /// finished load can ask for a frame. `None` in a test, where there is no loop
    /// to wake and nothing to draw.
    waker: Arc<Mutex<Option<Waker>>>,
    next: u64,
}

struct Request {
    id: u64,
    kind: ResourceKind,
    url: String,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher").finish_non_exhaustive()
    }
}

impl Fetcher {
    /// Start a pool of fetch threads over `loader`.
    ///
    /// [`FETCH_CONCURRENCY`] threads take from one queue, so a page's pictures are
    /// fetched several at a time and a slow one does not hold up the rest. Results
    /// therefore arrive in whatever order they finish, which is why every reply
    /// carries the number it was asked under.
    pub fn spawn<L: Loader>(loader: L) -> Self {
        let (request_sender, request_receiver) = channel::<Request>();
        let (result_sender, result_receiver) = channel::<Fetched>();
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));

        let loader: Arc<dyn Loader> = Arc::new(loader);
        // One queue, several takers: the mutex is held only long enough to take the
        // next request, never across a fetch.
        let queue = Arc::new(Mutex::new(request_receiver));

        for worker in 0..FETCH_CONCURRENCY {
            let queue = Arc::clone(&queue);
            let loader = Arc::clone(&loader);
            let results = result_sender.clone();
            let thread_waker = Arc::clone(&waker);

            std::thread::Builder::new()
                .name(format!("otlyra-fetch-{worker}"))
                .spawn(move || {
                    // Ends when the browser drops its sender, which is when the
                    // window has gone and there is nothing left to load for.
                    while let Ok(request) = {
                        let queue = queue.lock().expect("no panic while taking a request");
                        queue.recv()
                    } {
                        let result = loader.load(&request.url);
                        let fetched = Fetched {
                            id: request.id,
                            kind: request.kind,
                            url: request.url,
                            result,
                        };
                        if results.send(fetched).is_err() {
                            break;
                        }
                        if let Some(waker) =
                            thread_waker.lock().ok().and_then(|waker| waker.clone())
                        {
                            waker.wake();
                        }
                    }
                })
                .expect("a fetch thread must start");
        }

        Self {
            requests: request_sender,
            results: result_receiver,
            waker,
            next: 0,
        }
    }

    /// Tell the fetch thread how to ask for a frame when something finishes.
    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker);
        }
    }

    /// Ask for `url`. The number returned is what the result will carry.
    pub fn request(&mut self, url: &str, kind: ResourceKind) -> u64 {
        self.next += 1;
        let id = self.next;
        let _ = self.requests.send(Request {
            id,
            kind,
            url: url.to_owned(),
        });
        id
    }

    /// Everything that has finished since the last call. Never blocks.
    pub fn poll(&mut self) -> Vec<Fetched> {
        let mut finished = Vec::new();
        while let Ok(fetched) = self.results.try_recv() {
            finished.push(fetched);
        }
        finished
    }

    /// Block until something finishes, or until `timeout` passes.
    ///
    /// For a caller with no event loop to be woken by — a test, or a one-shot
    /// screenshot — and for nothing else: the window's thread must never wait here.
    pub fn wait(&mut self, timeout: std::time::Duration) -> Vec<Fetched> {
        match self.results.recv_timeout(timeout) {
            Ok(first) => {
                let mut finished = vec![first];
                finished.extend(self.poll());
                finished
            }
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A loader that takes its time and records how many fetches overlapped.
    struct SlowLoader {
        in_flight: Arc<AtomicUsize>,
        highest: Arc<AtomicUsize>,
    }

    impl Loader for SlowLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.highest.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Loaded {
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    /// The point of the pool: a page that asks for several things gets them at
    /// once rather than one after another.
    #[test]
    fn several_fetches_are_in_flight_at_once() {
        let highest = Arc::new(AtomicUsize::new(0));
        let mut fetcher = Fetcher::spawn(SlowLoader {
            in_flight: Arc::new(AtomicUsize::new(0)),
            highest: Arc::clone(&highest),
        });

        for index in 0..FETCH_CONCURRENCY {
            fetcher.request(
                &format!("https://example.test/{index}"),
                ResourceKind::Image,
            );
        }

        let mut finished = 0;
        while finished < FETCH_CONCURRENCY {
            let batch = fetcher.wait(std::time::Duration::from_secs(5));
            if batch.is_empty() {
                panic!("the pool never finished; {finished} of {FETCH_CONCURRENCY} arrived");
            }
            finished += batch.len();
        }

        assert!(
            highest.load(Ordering::SeqCst) > 1,
            "only one fetch ever ran at a time"
        );
    }

    /// Every reply carries the number it was asked under, which is what makes an
    /// out-of-order pool usable at all.
    #[test]
    fn a_reply_carries_the_number_it_was_asked_under() {
        let mut fetcher = Fetcher::spawn(SlowLoader {
            in_flight: Arc::new(AtomicUsize::new(0)),
            highest: Arc::new(AtomicUsize::new(0)),
        });

        let first = fetcher.request("https://example.test/one", ResourceKind::Document);
        let second = fetcher.request("https://example.test/two", ResourceKind::Image);
        assert_ne!(first, second);

        let mut seen = Vec::new();
        while seen.len() < 2 {
            let batch = fetcher.wait(std::time::Duration::from_secs(5));
            assert!(!batch.is_empty(), "the pool never finished");
            seen.extend(batch.into_iter().map(|fetched| (fetched.id, fetched.url)));
        }
        seen.sort();

        assert_eq!(
            seen,
            vec![
                (first, "https://example.test/one".to_owned()),
                (second, "https://example.test/two".to_owned()),
            ]
        );
    }
}
