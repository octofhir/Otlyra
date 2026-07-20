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
#[derive(Debug)]
pub struct Loaded {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// The charset the transport declared, if it declared one.
    pub charset: Option<String>,
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
/// is worth testing without a socket. `Send` because this runs on the fetch thread.
pub trait Loader: Send + 'static {
    /// Fetch `url`, returning the bytes, the transport's charset, and the address
    /// the bytes actually came from.
    fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String>;
}

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
    /// Start a fetch thread over `loader`.
    ///
    /// One thread, so requests are served in the order they were made and a page
    /// cannot open fifty sockets by listing fifty pictures. Parallel fetching is a
    /// pool on this side of the channel and changes nothing above it.
    pub fn spawn<L: Loader>(mut loader: L) -> Self {
        let (request_sender, request_receiver) = channel::<Request>();
        let (result_sender, result_receiver) = channel::<Fetched>();
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));

        let thread_waker = Arc::clone(&waker);
        std::thread::Builder::new()
            .name("otlyra-fetch".to_owned())
            .spawn(move || {
                // Ends when the browser drops its sender, which is when the window
                // has gone and there is nothing left to load for.
                while let Ok(request) = request_receiver.recv() {
                    let result = loader
                        .load(&request.url)
                        .map(|(bytes, charset, final_url)| Loaded {
                            bytes,
                            charset,
                            final_url,
                        });
                    let fetched = Fetched {
                        id: request.id,
                        kind: request.kind,
                        url: request.url,
                        result,
                    };
                    if result_sender.send(fetched).is_err() {
                        break;
                    }
                    if let Some(waker) = thread_waker.lock().ok().and_then(|waker| waker.clone()) {
                        waker.wake();
                    }
                }
            })
            .expect("the fetch thread must start");

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
