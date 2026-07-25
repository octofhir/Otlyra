//! Fetching, off the thread the window runs on.
//!
//! The event loop blocks, and a load takes as long as a network takes; doing one
//! on the loop's thread is a frozen window for the duration. So a fetch is a task
//! on the shared [`crate::io`] runtime, reachable through one channel and nothing
//! else: results come back, and everything that decides what a result *means* stays
//! where the state it changes lives.
//!
//! What crosses the boundary is owned bytes and a request number. No DOM, no style,
//! no fragment: a document parsed on the wrong thread is a document that has to be
//! `Send` forever after.
//!
//! It was six operating system threads taking from one queue, each blocking on a
//! transport that had a runtime of its own inside it. The shape was right and the
//! implementation was three sleeping mechanisms deep: a thread parked on a mutex,
//! parked on a channel, blocking on a reactor. Now the wait is a suspended future
//! and the only thing bounding how many run at once is the thing that should —
//! [`FETCH_CONCURRENCY`], as a semaphore.
//!
//! Two seams rather than one, because the browser and its tests want opposite
//! things. [`AsyncLoader`] is what a real transport implements: a future, no thread
//! taken while it waits. [`Loader`] is a plain blocking function, which is what the
//! three dozen fake loaders in this crate's tests are and should stay — a canned
//! page is not worth a future — and it reaches the same pool through
//! [`Fetcher::spawn`], which runs it on Tokio's blocking pool.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

pub use otlyra_net::Body;
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
    /// for a free slot in the pool.
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

/// How a tab gets its bytes, when getting them blocks.
///
/// A trait rather than a direct call to `otlyra-net` for one reason: the browser's
/// behaviour around navigation — which tab, what title, what happens on failure —
/// is worth testing without a socket. A test's loader answers out of a table, so a
/// plain function that returns a result is exactly the right shape for one and a
/// future would be ceremony around a `match`.
///
/// `Send + Sync` and `&self`, because one of these is shared by every outstanding
/// fetch: a loader holds a client and a connection pool, and one per request would
/// be several of both.
///
/// A real transport implements [`AsyncLoader`] instead. This one is run on Tokio's
/// blocking pool, which is correct for reading a file or a canned string and wrong
/// for holding a socket open.
pub trait Loader: Send + Sync + 'static {
    /// Fetch `url`, returning the bytes and the little the transport knows about
    /// them. What they *are* is decided above this, from the bytes as well.
    fn load(&self, url: &str) -> Result<Loaded, String>;

    /// Fetch `url`, sending `body` with it.
    ///
    /// A transport that can carry a body overrides this; the default answers a
    /// request that has one with the bytes it answers a plain fetch with, which is
    /// what a loader that stands in for a network wants and what no real one
    /// should do.
    fn send(&self, url: &str, body: Option<Body>) -> Result<Loaded, String> {
        let _ = body;
        self.load(url)
    }
}

/// One fetch in progress, as the pool holds it.
///
/// Boxed rather than an `async fn` in the trait because the trait is used as
/// `dyn`: the browser holds one loader whose type it does not know, and an
/// `-> impl Future` cannot be called through a pointer.
pub type Fetching = std::pin::Pin<Box<dyn Future<Output = Result<Loaded, String>> + Send>>;

/// How a tab gets its bytes without a thread waiting for them.
///
/// The receiver is `Arc<Self>` so the future can outlive the call: the pool spawns
/// it and returns, and the loader has to still be there when the socket answers.
/// That also means one loader — one client, one connection pool, one DNS cache —
/// however many requests are in flight.
pub trait AsyncLoader: Send + Sync + 'static {
    /// Fetch `url`, sending `body` with it if there is one.
    ///
    /// Owned arguments rather than borrowed: what is returned is a future that
    /// outlives this call, and a borrow of the caller's string would not survive
    /// being spawned.
    fn fetch(self: Arc<Self>, url: String, body: Option<Body>) -> Fetching;
}

/// A blocking [`Loader`] made to look like an [`AsyncLoader`].
///
/// The blocking pool rather than a worker thread: `spawn_blocking` is where Tokio
/// puts work that parks its thread, and a fake loader that sleeps to prove the pool
/// overlaps is exactly that. Bounded above by the fetcher's own semaphore, so this
/// cannot grow the blocking pool past [`FETCH_CONCURRENCY`] threads.
struct Blocking<L: Loader>(L);

impl<L: Loader> AsyncLoader for Blocking<L> {
    fn fetch(self: Arc<Self>, url: String, body: Option<Body>) -> Fetching {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || self.0.send(&url, body))
                .await
                // A panic in a loader is a bug in a loader, and the fetch it was
                // answering has to end as *something*: reported as this request's
                // failure, where a person can see it, rather than as a request that
                // stays pending until the window closes.
                .unwrap_or_else(|error| Err(format!("the fetch task failed: {error}")))
        })
    }
}

/// How many fetches may be in flight at once.
///
/// Six, which is the number browsers settled on per host over HTTP/1.1: enough that
/// a page of pictures does not arrive one at a time, few enough that a page can
/// point only so many connections at a server. Ours is a total rather than a
/// per-host count, which is stricter and simpler; per-host queues belong with a
/// real connection pool underneath.
///
/// A semaphore rather than a thread count, so the limit says what it means: six
/// requests may be *outstanding*, and a suspended one holds nothing but a permit.
pub const FETCH_CONCURRENCY: usize = 6;

/// The handle the browser keeps on the fetch pool.
pub struct Fetcher {
    /// Every request made, oldest first, bounded.
    exchanges: Vec<Exchange>,
    loader: Arc<dyn AsyncLoader>,
    /// How many fetches may be awake at once. Held by the task for as long as its
    /// transport runs, which is what makes it a limit rather than a rate.
    permits: Arc<tokio::sync::Semaphore>,
    /// Kept beside the receiver so each task can be handed a clone. The channel
    /// therefore never disconnects while the browser is alive, which is what
    /// [`Fetcher::wait`] relies on to mean *nothing finished yet* rather than
    /// *nothing ever will*.
    finished: Sender<Fetched>,
    results: Receiver<Fetched>,
    /// Set once the platform hands one over, and shared with every fetch task so a
    /// finished load can ask for a frame. `None` in a test, where there is no loop
    /// to wake and nothing to draw.
    waker: Arc<Mutex<Option<Waker>>>,
    next: u64,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher").finish_non_exhaustive()
    }
}

impl Fetcher {
    /// A fetch pool over a blocking `loader`.
    ///
    /// The loader runs on Tokio's blocking pool, so a fake one may sleep, read a
    /// file, or take a lock without stalling anything else. What a real transport
    /// wants is [`Fetcher::spawn_async`].
    pub fn spawn<L: Loader>(loader: L) -> Self {
        Self::over(Arc::new(Blocking(loader)))
    }

    /// A fetch pool over an `loader` that suspends instead of blocking.
    pub fn spawn_async<L: AsyncLoader>(loader: L) -> Self {
        Self::over(Arc::new(loader))
    }

    /// The pool itself, over whichever seam the caller brought.
    ///
    /// Nothing is spawned here and the runtime is not touched: a browser that never
    /// fetches never builds one, which keeps the startup path free of a reactor.
    /// [`FETCH_CONCURRENCY`] fetches then run at once, so a page's pictures arrive
    /// several at a time and a slow one does not hold up the rest — in whatever
    /// order they finish, which is why every reply carries the number it was asked
    /// under.
    fn over(loader: Arc<dyn AsyncLoader>) -> Self {
        let (finished, results) = channel::<Fetched>();
        Self {
            exchanges: Vec::new(),
            loader,
            permits: Arc::new(tokio::sync::Semaphore::new(FETCH_CONCURRENCY)),
            finished,
            results,
            waker: Arc::new(Mutex::new(None)),
            next: 0,
        }
    }

    /// Tell the fetch tasks how to ask for a frame when something finishes.
    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker);
        }
    }

    /// Ask for `url`. The number returned is what the result will carry.
    pub fn request(&mut self, url: &str, kind: ResourceKind) -> u64 {
        self.send(url, kind, None)
    }

    /// Ask for `url`, sending `body` with it. The number returned is what the
    /// result will carry.
    pub fn send(&mut self, url: &str, kind: ResourceKind, body: Option<Body>) -> u64 {
        self.next += 1;
        let id = self.next;
        let method = if body.is_some() { "POST" } else { "GET" };
        if self.exchanges.len() >= EXCHANGE_LIMIT {
            self.exchanges.remove(0);
        }
        self.exchanges.push(Exchange {
            id,
            kind,
            method,
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
        let loader = Arc::clone(&self.loader);
        let permits = Arc::clone(&self.permits);
        let finished = self.finished.clone();
        let waker = Arc::clone(&self.waker);
        let url = url.to_owned();
        crate::io::shared().spawn(async move {
            // The queue is the wait for a permit, and it is deliberately outside the
            // timing below: `took` is how slow the transport was and `waited` is how
            // busy the pool was, and the panel shows both so neither has to stand in
            // for the other.
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            let started = std::time::Instant::now();
            let result = loader.fetch(url.clone(), body).await;
            let fetched = Fetched {
                id,
                kind,
                url,
                took: started.elapsed(),
                result,
            };
            // A closed channel is a browser that has gone. Nothing to report to and
            // nothing to wake, so the bytes are dropped here rather than kept for a
            // receiver that will not read them.
            if finished.send(fetched).is_err() {
                return;
            }
            if let Some(waker) = waker.lock().ok().and_then(|waker| waker.clone()) {
                waker.wake();
            }
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

    /// Drain `fetcher` until `count` results have arrived, or fail loudly.
    fn drain(fetcher: &mut Fetcher, count: usize) -> Vec<Fetched> {
        let mut finished = Vec::new();
        while finished.len() < count {
            let batch = fetcher.wait(std::time::Duration::from_secs(10));
            if batch.is_empty() {
                panic!("the pool stalled; {} of {count} arrived", finished.len());
            }
            finished.extend(batch);
        }
        finished
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

        drain(&mut fetcher, FETCH_CONCURRENCY);

        assert!(
            highest.load(Ordering::SeqCst) > 1,
            "only one fetch ever ran at a time"
        );
    }

    /// And the limit is a limit. Asking for four times the allowance must never put
    /// more than the allowance on the wire at once — that is the whole reason the
    /// count exists, and it used to be enforced by there being exactly six threads.
    /// Now that a fetch is a task, nothing but the semaphore stands between a page
    /// of two hundred pictures and two hundred sockets.
    #[test]
    fn no_more_than_the_allowance_is_ever_in_flight() {
        let highest = Arc::new(AtomicUsize::new(0));
        let mut fetcher = Fetcher::spawn(SlowLoader {
            in_flight: Arc::new(AtomicUsize::new(0)),
            highest: Arc::clone(&highest),
        });

        let asked = FETCH_CONCURRENCY * 4;
        for index in 0..asked {
            fetcher.request(
                &format!("https://example.test/{index}"),
                ResourceKind::Image,
            );
        }
        drain(&mut fetcher, asked);

        assert_eq!(
            highest.load(Ordering::SeqCst),
            FETCH_CONCURRENCY,
            "the pool ran more fetches at once than it is allowed to"
        );
    }

    /// A transport that suspends rather than blocking reaches the same pool, and its
    /// results carry the same numbers. This is the seam the real network uses; the
    /// blocking one above is what the fake loaders use.
    #[test]
    fn an_async_loader_answers_through_the_same_pool() {
        struct Suspending {
            in_flight: Arc<AtomicUsize>,
            highest: Arc<AtomicUsize>,
        }

        impl AsyncLoader for Suspending {
            fn fetch(self: Arc<Self>, url: String, body: Option<Body>) -> Fetching {
                Box::pin(async move {
                    assert!(body.is_none(), "nothing here posts");
                    let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    self.highest.fetch_max(now, Ordering::SeqCst);
                    // A timer rather than a sleep: the point of the async seam is
                    // that a waiting fetch holds no thread at all.
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    self.in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(Loaded {
                        final_url: url,
                        ..Default::default()
                    })
                })
            }
        }

        let highest = Arc::new(AtomicUsize::new(0));
        let mut fetcher = Fetcher::spawn_async(Suspending {
            in_flight: Arc::new(AtomicUsize::new(0)),
            highest: Arc::clone(&highest),
        });

        let asked: Vec<u64> = (0..FETCH_CONCURRENCY * 2)
            .map(|index| {
                fetcher.request(
                    &format!("https://example.test/{index}"),
                    ResourceKind::Image,
                )
            })
            .collect();

        let mut ids: Vec<u64> = drain(&mut fetcher, asked.len())
            .into_iter()
            .map(|fetched| fetched.id)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, asked);
        let highest = highest.load(Ordering::SeqCst);
        assert!(highest > 1, "the async pool ran one fetch at a time");
        assert!(
            highest <= FETCH_CONCURRENCY,
            "the async pool ran {highest} fetches at once, past its allowance"
        );
    }

    /// A loader that panics ends the request it was answering rather than leaving it
    /// pending forever. A row stuck on *Pending* with no thread behind it is the
    /// worst of both: nothing is loading and nothing says so.
    #[test]
    fn a_panicking_loader_fails_its_request() {
        struct Broken;

        impl Loader for Broken {
            fn load(&self, _url: &str) -> Result<Loaded, String> {
                panic!("the transport came apart");
            }
        }

        let mut fetcher = Fetcher::spawn(Broken);
        fetcher.request("https://example.test/", ResourceKind::Document);
        let finished = drain(&mut fetcher, 1);

        assert!(finished[0].result.is_err());
        assert!(matches!(
            fetcher.exchanges()[0].status,
            Status::Failed(ref error) if error.contains("fetch task failed")
        ));
    }

    /// A body reaches the transport, and the list says which request carried one.
    #[test]
    fn a_request_with_a_body_is_a_post() {
        struct BodyLoader {
            seen: std::sync::Arc<Mutex<Vec<Option<Body>>>>,
        }

        impl Loader for BodyLoader {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                self.send(url, None)
            }

            fn send(&self, url: &str, body: Option<Body>) -> Result<Loaded, String> {
                self.seen.lock().expect("no panic").push(body);
                Ok(Loaded {
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut fetcher = Fetcher::spawn(BodyLoader {
            seen: Arc::clone(&seen),
        });
        let body = Body {
            content_type: "text/plain".to_owned(),
            bytes: b"who=Ada".to_vec(),
        };
        fetcher.send(
            "https://example.test/save",
            ResourceKind::Document,
            Some(body.clone()),
        );
        while fetcher.wait(std::time::Duration::from_secs(5)).is_empty() {}

        assert_eq!(*seen.lock().expect("no panic"), vec![Some(body)]);
        assert_eq!(fetcher.exchanges()[0].method, "POST");
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
