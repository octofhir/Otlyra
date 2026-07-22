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
    /// The HTTP status the server answered with, when the fetch was over HTTP.
    ///
    /// `None` for a `file:` load, which has no status — and drawn as such rather
    /// than as an invented `200`. A `404` arrives here beside a body, because a
    /// transport that returned bytes *succeeded*; whether those bytes are the
    /// page asked for is what the status says and the `Ok`/`Failed` split cannot.
    pub status: Option<u16>,
    /// The headers put on the request.
    pub request_headers: Vec<(String, String)>,
    /// The headers the response carried.
    pub response_headers: Vec<(String, String)>,
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
    /// How long the transport took, measured around the loader itself.
    ///
    /// Around the loader rather than from when the browser asked, because the
    /// two answer different questions: this one is how slow the network was, and
    /// the wait before it is how busy the queue was. The panel shows both, so
    /// neither has to stand in for the other.
    pub took: std::time::Duration,
    /// What came back.
    pub result: Result<Loaded, String>,
}

/// How a request ended, as the panel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Still out.
    Pending,
    /// Came back, with this many bytes.
    Ok(usize),
    /// Did not.
    Failed(String),
}

/// The most of a response body the fetcher keeps for the inspector to show.
///
/// A body is shown, not the whole of one: a page's own bytes have already been
/// parsed and drawn, and a second full copy of every resource for a pane that
/// might be opened would be the page's memory twice over. A quarter-megabyte is
/// enough to read a stylesheet or preview a small picture, and a truncated body
/// says as much in the pane rather than pretending to be whole.
const BODY_KEPT: usize = 256 * 1024;

/// One request the browser made, and what became of it.
///
/// Kept by the fetcher because the fetcher is what knows: it has the number, the
/// address, the kind and the timing, and nowhere a person could see any of it.
#[derive(Clone, Debug)]
pub struct Exchange {
    /// The number it was made under.
    pub id: u64,
    /// What it was for, which is the nearest thing to *what asked for it* the
    /// browser currently records — the element that named it is not tracked.
    pub kind: ResourceKind,
    /// The method it was made with. `GET` for everything the browser fetches
    /// today; a field rather than a constant so the day a form posts, the pane
    /// already has somewhere to say so.
    pub method: &'static str,
    /// The address.
    pub url: String,
    /// How it ended.
    pub status: Status,
    /// The HTTP status code, when the fetch reached a server that answered one.
    pub code: Option<u16>,
    /// What the transport said the body is.
    pub content_type: Option<String>,
    /// The headers put on the request.
    pub request_headers: Vec<(String, String)>,
    /// The headers the response carried.
    pub response_headers: Vec<(String, String)>,
    /// As much of the body as is kept, and whether that is all of it.
    pub body: Vec<u8>,
    /// Whether `body` is the whole of what arrived.
    pub body_complete: bool,
    /// How long the transport took, once it ended.
    pub took: Option<std::time::Duration>,
    /// How long from the ask to the browser noticing, which includes the wait
    /// for a free fetch thread.
    pub waited: Option<std::time::Duration>,
    asked_at: std::time::Instant,
}

impl Exchange {
    /// A finished exchange, for a test that needs a network list without a
    /// socket. Everything the panel reads is a public field to be set after.
    #[cfg(test)]
    pub fn for_test(id: u64, kind: ResourceKind, url: &str, status: Status) -> Self {
        Self {
            id,
            kind,
            method: "GET",
            url: url.to_owned(),
            status,
            code: None,
            content_type: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            body: Vec::new(),
            body_complete: true,
            took: None,
            waited: None,
            asked_at: std::time::Instant::now(),
        }
    }
}

/// How many requests the list keeps before the oldest goes.
const EXCHANGE_LIMIT: usize = 300;

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
    /// Every request made, oldest first, bounded.
    exchanges: Vec<Exchange>,
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
                        let started = std::time::Instant::now();
                        let result = loader.load(&request.url);
                        let fetched = Fetched {
                            id: request.id,
                            kind: request.kind,
                            url: request.url,
                            took: started.elapsed(),
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
            exchanges: Vec::new(),
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
        if self.exchanges.len() >= EXCHANGE_LIMIT {
            self.exchanges.remove(0);
        }
        self.exchanges.push(Exchange {
            id,
            kind,
            method: "GET",
            url: url.to_owned(),
            status: Status::Pending,
            code: None,
            content_type: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            body: Vec::new(),
            body_complete: false,
            took: None,
            waited: None,
            asked_at: std::time::Instant::now(),
        });
        let _ = self.requests.send(Request {
            id,
            kind,
            url: url.to_owned(),
        });
        id
    }

    /// Every request made, oldest first.
    pub fn exchanges(&self) -> &[Exchange] {
        &self.exchanges
    }

    /// Everything that has finished since the last call. Never blocks.
    pub fn poll(&mut self) -> Vec<Fetched> {
        let mut finished = Vec::new();
        while let Ok(fetched) = self.results.try_recv() {
            self.record(&fetched);
            finished.push(fetched);
        }
        finished
    }

    /// Note what became of one request.
    ///
    /// Here rather than at the call site that consumes the result: a caller that
    /// forgot would leave a request listed as pending forever, and there are
    /// three of them.
    fn record(&mut self, fetched: &Fetched) {
        let Some(exchange) = self
            .exchanges
            .iter_mut()
            .find(|exchange| exchange.id == fetched.id)
        else {
            return;
        };
        exchange.status = match &fetched.result {
            Ok(loaded) => Status::Ok(loaded.bytes.len()),
            Err(error) => Status::Failed(error.clone()),
        };
        // The parts the panel's detail side shows, cloned here — the last place
        // the bytes are still in hand before the browser moves them out of the
        // result to parse them.
        if let Ok(loaded) = &fetched.result {
            exchange.code = loaded.status;
            exchange.content_type = loaded.content_type.clone();
            exchange.request_headers = loaded.request_headers.clone();
            exchange.response_headers = loaded.response_headers.clone();
            exchange.body_complete = loaded.bytes.len() <= BODY_KEPT;
            exchange.body = loaded.bytes[..loaded.bytes.len().min(BODY_KEPT)].to_vec();
        }
        exchange.took = Some(fetched.took);
        exchange.waited = Some(exchange.asked_at.elapsed());
    }

    /// Block until something finishes, or until `timeout` passes.
    ///
    /// For a caller with no event loop to be woken by — a test, or a one-shot
    /// screenshot — and for nothing else: the window's thread must never wait here.
    pub fn wait(&mut self, timeout: std::time::Duration) -> Vec<Fetched> {
        match self.results.recv_timeout(timeout) {
            Ok(first) => {
                self.record(&first);
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
