//! Dedicated workers (HTML §10.2): a script with an isolate and a thread of
//! its own, talking to the page that made it only through messages.
//!
//! Each worker is an Otter runtime built on its own OS thread, with the web
//! APIs Otter has, a timer wheel turned by that thread, and none of the DOM.
//! Messages cross as text: the structured-clone serialization `clone.js`
//! writes and reads on both sides, so no engine value ever leaves the isolate
//! it belongs to.
//!
//! A worker never touches the network itself. Its script, `importScripts` and
//! `fetch` are asked of the page, which sends them through the browser's
//! `Fetcher` like any page request — same policy, same cookies — and hands
//! the answer back. The worker's thread waits for it; the page's never does.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use otter_runtime::{
    InterruptHandle, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeValue, SourceInput, runtime_arg_to_string, runtime_string_value, runtime_type_error,
    web_modules::{WebModuleGraph, WebModuleMap, WebModuleSource},
};

use crate::timers::TimerWheel;

/// The most workers one page may have running at once. A page that asks for
/// more is told it cannot, as a browser out of threads tells it.
pub const MAX_WORKERS: usize = 16;

/// The heap one worker may use.
const WORKER_HEAP_BYTES: u64 = 256 * 1024 * 1024;

/// The glue that makes an isolate a worker's: `self`, `postMessage`,
/// `importScripts`, `close`, `location`, `fetch` through the page.
const WORKER_GLOBAL: &str = concat!(
    "(function () {\n",
    include_str!("dom/js/clone.js"),
    "\n",
    include_str!("worker_global.js"),
    "\n})();"
);

/// How the browser is woken when a worker has something for its page: the
/// content thread may be asleep with no timer due.
static WAKE: OnceLock<Arc<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Say how to wake the browser for a worker's message. Set once, by the
/// embedder, before any page runs.
pub fn set_wake(wake: Arc<dyn Fn() + Send + Sync>) {
    let _ = WAKE.set(wake);
}

pub(crate) fn wake() {
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// What a worker's thread sends its page.
#[derive(Debug)]
pub(crate) enum FromWorker {
    /// `postMessage(data)`, serialized.
    Message(u64, String),
    /// An exception nothing in the worker caught.
    Error(u64, String),
    /// A request the worker needs answered: its number in the worker, the
    /// address, and whether it is a script.
    Fetch {
        worker: u64,
        request: u64,
        url: String,
        script: bool,
    },
    /// The worker has stopped: `close()`, `terminate()`, or its script
    /// failed to load.
    Closed(u64),
}

/// What the page sends a worker's thread.
#[derive(Debug)]
enum ToWorker {
    /// `worker.postMessage(data)`, serialized.
    Message(String),
    /// The answer to one of its requests: status, final address, body.
    Response(u64, Result<(u16, String, Vec<u8>), String>),
    /// `worker.terminate()`, or the page going away.
    Terminate,
}

/// The page's end of one worker.
struct Handle {
    to: Sender<ToWorker>,
    /// Filled by the worker's thread once its isolate exists, to stop a
    /// script that runs forever.
    interrupt: Arc<Mutex<Option<InterruptHandle>>>,
}

/// A page's workers, and what they have sent it.
pub(crate) struct Workers {
    next: u64,
    running: HashMap<u64, Handle>,
    /// Shared with every worker's thread: what they posted, oldest first.
    inbox: Arc<Mutex<Vec<FromWorker>>>,
    /// Page request numbers that are a worker's requests: whose, and its own
    /// number for it.
    fetches: HashMap<u64, (u64, u64)>,
    /// Messages and errors waiting for the page's glue to take them.
    events: VecDeque<(u64, &'static str, String)>,
}

impl Default for Workers {
    fn default() -> Self {
        Self {
            next: 0,
            running: HashMap::new(),
            inbox: Arc::new(Mutex::new(Vec::new())),
            fetches: HashMap::new(),
            events: VecDeque::new(),
        }
    }
}

impl std::fmt::Debug for Workers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workers")
            .field("running", &self.running.len())
            .finish_non_exhaustive()
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        for id in self.running.keys().copied().collect::<Vec<_>>() {
            self.terminate(id);
        }
    }
}

impl Workers {
    /// Start a worker running the script at `url`, or the `source` given for
    /// it (an object URL's, which only the page can read). `None` when the
    /// page has as many as it may.
    pub(crate) fn start(
        &mut self,
        url: String,
        module: bool,
        source: Option<String>,
    ) -> Option<u64> {
        if self.running.len() >= MAX_WORKERS {
            return None;
        }
        self.next += 1;
        let id = self.next;
        let (to, from_page) = mpsc::channel();
        let interrupt = Arc::new(Mutex::new(None));
        let thread = Thread {
            id,
            url,
            module,
            source,
            from_page: Arc::new(Mutex::new(from_page)),
            to_page: Arc::clone(&self.inbox),
            interrupt: Arc::clone(&interrupt),
            incoming: Arc::new(Mutex::new(VecDeque::new())),
            closing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            next_request: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let spawned = std::thread::Builder::new()
            .name(format!("otlyra-worker-{id}"))
            // The engine recurses on the native stack, as the content thread's
            // does, and is given as much.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || thread.run());
        if let Err(error) = spawned {
            tracing::warn!(target: "page.script", %error, "a worker thread could not start");
            return None;
        }
        self.running.insert(id, Handle { to, interrupt });
        Some(id)
    }

    /// `worker.postMessage(data)`.
    pub(crate) fn post(&mut self, id: u64, data: String) {
        if let Some(handle) = self.running.get(&id) {
            let _ = handle.to.send(ToWorker::Message(data));
        }
    }

    /// `worker.terminate()`: stop it now, mid-script if need be.
    pub(crate) fn terminate(&mut self, id: u64) {
        if let Some(handle) = self.running.remove(&id) {
            let _ = handle.to.send(ToWorker::Terminate);
            if let Ok(interrupt) = handle.interrupt.lock()
                && let Some(interrupt) = interrupt.as_ref()
            {
                interrupt.interrupt();
            }
        }
    }

    /// Take what the workers sent: messages and errors become events for
    /// the glue, requests become page requests through `queue`, which
    /// answers the page's number for each.
    pub(crate) fn drain(&mut self, mut queue: impl FnMut(String, bool) -> u64) -> bool {
        let taken = match self.inbox.lock() {
            Ok(mut inbox) => std::mem::take(&mut *inbox),
            Err(_) => return false,
        };
        let mut any = false;
        for item in taken {
            match item {
                FromWorker::Message(id, data) if self.running.contains_key(&id) => {
                    self.events.push_back((id, "message", data));
                    any = true;
                }
                FromWorker::Error(id, message) if self.running.contains_key(&id) => {
                    self.events.push_back((id, "error", message));
                    any = true;
                }
                FromWorker::Fetch {
                    worker,
                    request,
                    url,
                    script,
                } if self.running.contains_key(&worker) => {
                    let page_request = queue(url, script);
                    self.fetches.insert(page_request, (worker, request));
                }
                FromWorker::Closed(id) => {
                    self.running.remove(&id);
                }
                _ => {}
            }
        }
        any
    }

    /// Whether page request `id` was a worker's, and if so hand it the
    /// answer.
    pub(crate) fn answer(
        &mut self,
        id: u64,
        response: &Result<otlyra_html::PageResponse, String>,
    ) -> bool {
        let Some((worker, request)) = self.fetches.remove(&id) else {
            return false;
        };
        if let Some(handle) = self.running.get(&worker) {
            let answer = response
                .as_ref()
                .map(|response| (response.status, response.url.clone(), response.body.clone()))
                .map_err(Clone::clone);
            let _ = handle.to.send(ToWorker::Response(request, answer));
        }
        true
    }

    /// The next message or error for the glue: the worker's number, the
    /// event's type, and its data.
    pub(crate) fn next_event(&mut self) -> Option<(u64, &'static str, String)> {
        self.events.pop_front()
    }

    /// Whether events are waiting for the glue.
    pub(crate) fn has_events(&self) -> bool {
        !self.events.is_empty()
    }
}

/// Everything a worker's thread owns.
struct Thread {
    id: u64,
    url: String,
    module: bool,
    source: Option<String>,
    from_page: Arc<Mutex<Receiver<ToWorker>>>,
    to_page: Arc<Mutex<Vec<FromWorker>>>,
    interrupt: Arc<Mutex<Option<InterruptHandle>>>,
    /// Messages from the page the worker's script has yet to hear.
    incoming: Arc<Mutex<VecDeque<String>>>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    next_request: Arc<std::sync::atomic::AtomicU64>,
}

/// What a worker's natives need to reach the page: one clonable bundle.
#[derive(Clone)]
struct Link {
    id: u64,
    from_page: Arc<Mutex<Receiver<ToWorker>>>,
    to_page: Arc<Mutex<Vec<FromWorker>>>,
    incoming: Arc<Mutex<VecDeque<String>>>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    next_request: Arc<std::sync::atomic::AtomicU64>,
}

impl Link {
    fn send(&self, item: FromWorker) {
        if let Ok(mut inbox) = self.to_page.lock() {
            inbox.push(item);
        }
        wake();
    }

    /// Ask the page for `url` and wait for the answer, keeping the messages
    /// that arrive meanwhile for later.
    fn fetch(&self, url: &str, script: bool) -> Result<(u16, String, Vec<u8>), String> {
        let request = self
            .next_request
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.send(FromWorker::Fetch {
            worker: self.id,
            request,
            url: url.to_owned(),
            script,
        });
        let receiver = self
            .from_page
            .lock()
            .map_err(|_| "the page is gone".to_owned())?;
        loop {
            match receiver.recv() {
                Ok(ToWorker::Response(id, answer)) if id == request => return answer,
                Ok(ToWorker::Response(..)) => {}
                Ok(ToWorker::Message(data)) => {
                    if let Ok(mut incoming) = self.incoming.lock() {
                        incoming.push_back(data);
                    }
                }
                Ok(ToWorker::Terminate) | Err(_) => {
                    self.closing
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err("the worker was terminated".to_owned());
                }
            }
        }
    }
}

fn arg_string(ctx: &RuntimeNativeCtx<'_>, args: &[RuntimeValue], index: usize) -> String {
    runtime_arg_to_string(args, index, ctx.heap())
}

/// Bytes as a string of the code units 0–255, which the glue turns back into
/// bytes: how a body crosses into the isolate without a typed array API.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

fn install(link: Link) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        let post = link.clone();
        realm.install_native_global_call(
            "__otlyraWorkerPost",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    post.send(FromWorker::Message(post.id, arg_string(ctx, args, 0)));
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let next = link.clone();
        realm.install_native_global_call(
            "__otlyraWorkerNext",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let item = next
                        .incoming
                        .lock()
                        .ok()
                        .and_then(|mut queue| queue.pop_front());
                    match item {
                        Some(data) => runtime_string_value(ctx, &data),
                        None => Ok(RuntimeValue::null()),
                    }
                },
            )),
        )?;
        let fetch = link.clone();
        realm.install_native_global_call(
            "__otlyraWorkerFetch",
            2,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let url = arg_string(ctx, args, 0);
                    let script = args
                        .get(1)
                        .and_then(|value| value.as_boolean())
                        .unwrap_or(false);
                    match fetch.fetch(&url, script) {
                        // Status, address and body in one string the glue
                        // splits: two lines of text, then the bytes.
                        Ok((status, url, body)) => runtime_string_value(
                            ctx,
                            &format!("{status}\n{url}\n{}", latin1(&body)),
                        ),
                        Err(message) => Err(runtime_type_error(
                            "fetch",
                            format!("NetworkError: {url}: {message}"),
                        )),
                    }
                },
            )),
        )?;
        let close = link.clone();
        realm.install_native_global_call(
            "__otlyraWorkerClose",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |_ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    close
                        .closing
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        Ok(())
    })
}

impl Thread {
    fn link(&self) -> Link {
        Link {
            id: self.id,
            from_page: Arc::clone(&self.from_page),
            to_page: Arc::clone(&self.to_page),
            incoming: Arc::clone(&self.incoming),
            closing: Arc::clone(&self.closing),
            next_request: Arc::clone(&self.next_request),
        }
    }

    fn run(self) {
        let link = self.link();
        if let Err(message) = self.serve(&link) {
            link.send(FromWorker::Error(self.id, message));
        }
        link.send(FromWorker::Closed(self.id));
    }

    fn serve(&self, link: &Link) -> Result<(), String> {
        let modules = WebModuleMap::new();
        let builder = Runtime::builder()
            .web_modules(modules.clone())
            .console_sink(crate::console::tracing_console())
            .timeout(Duration::ZERO)
            .max_heap_bytes(WORKER_HEAP_BYTES)
            .max_stack_depth(1024)
            .process_global(false)
            .worker_global(false)
            .extension_installer(install(link.clone()));
        let mut runtime = otter_web::with_web_apis(builder)
            .build()
            .map_err(|error| format!("the worker's isolate could not be built: {error}"))?;
        let wheel = TimerWheel::new();
        runtime.install_timer_scheduler(Arc::new(wheel.clone()));
        if let Ok(mut interrupt) = self.interrupt.lock() {
            *interrupt = Some(runtime.interrupt_handle());
        }
        let setup = format!(
            "{WORKER_GLOBAL}\n__otlyraWorkerSetup({});",
            js_string(&self.url)
        );
        run(&mut runtime, &setup, "<worker>")?;

        // The script: fetched through the page unless the page had it.
        let (url, source) = match &self.source {
            Some(source) => (self.url.clone(), source.clone()),
            None => {
                let (status, url, body) = link.fetch(&self.url, true)?;
                if !(200..300).contains(&status) {
                    return Err(format!(
                        "{}: the worker's script answered {status}",
                        self.url
                    ));
                }
                (url, String::from_utf8_lossy(&body).into_owned())
            }
        };
        if self.module {
            modules.set_document_base(&url);
            modules.deliver(
                &url,
                Ok(WebModuleSource {
                    final_url: url.clone(),
                    content_type: Some("text/javascript".to_owned()),
                    text: source.clone(),
                }),
            );
            // The rest of the graph, one request at a time, until it is all
            // here: a worker may wait, and this one has nothing else to do.
            for _ in 0..512 {
                match modules.graph_status(&url) {
                    Ok(WebModuleGraph::Ready) => break,
                    Ok(WebModuleGraph::Pending { fetch }) => {
                        for wanted in fetch {
                            let answer =
                                link.fetch(&wanted, true)
                                    .and_then(|(status, final_url, body)| {
                                        if (200..300).contains(&status) {
                                            Ok(WebModuleSource {
                                                final_url,
                                                content_type: Some("text/javascript".to_owned()),
                                                text: String::from_utf8_lossy(&body).into_owned(),
                                            })
                                        } else {
                                            Err(format!("answered {status}"))
                                        }
                                    });
                            modules.deliver(&wanted, answer);
                        }
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            let result = runtime.run_module_source(SourceInput::from_javascript(&source), &url);
            let drained = crate::host::microtask_checkpoint(&mut runtime);
            if let Err(error) = result.map(drop).and(drained) {
                link.send(FromWorker::Error(self.id, error.to_string()));
            }
        } else if let Err(message) = run(&mut runtime, &source, &url) {
            link.send(FromWorker::Error(self.id, message));
        }

        // The event loop: messages from the page and timers coming due, one
        // task at a time, until the worker closes or is terminated.
        let closing = || link.closing.load(std::sync::atomic::Ordering::Relaxed);
        while !closing() {
            let now = Instant::now();
            while let Some(token) = wheel.take_due(now) {
                let fired = runtime.fire_timer(token);
                let drained = crate::host::microtask_checkpoint(&mut runtime);
                if let Err(error) = fired.map(drop).and(drained) {
                    link.send(FromWorker::Error(self.id, error.to_string()));
                }
                if closing() {
                    return Ok(());
                }
            }
            if self.incoming.lock().is_ok_and(|queue| !queue.is_empty()) {
                if let Err(message) = run(&mut runtime, "__otlyraWorkerDispatch()", "<message>") {
                    link.send(FromWorker::Error(self.id, message));
                }
                continue;
            }
            let wait = wheel
                .next_deadline()
                .map_or(Duration::from_secs(3600), |at| {
                    at.saturating_duration_since(Instant::now())
                });
            let received = match self.from_page.lock() {
                Ok(receiver) => receiver.recv_timeout(wait),
                Err(_) => return Ok(()),
            };
            match received {
                Ok(ToWorker::Message(data)) => {
                    if let Ok(mut queue) = self.incoming.lock() {
                        queue.push_back(data);
                    }
                }
                Ok(ToWorker::Response(..)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(ToWorker::Terminate) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        Ok(())
    }
}

/// Run a classic script and its microtasks; what it threw, as text.
fn run(runtime: &mut Runtime, source: &str, url: &str) -> Result<(), String> {
    let result = runtime.run_script(SourceInput::from_javascript(source), url);
    let drained = crate::host::microtask_checkpoint(runtime);
    result
        .map(drop)
        .and(drained)
        .map_err(|error| error.to_string())
}

/// `text` as a JavaScript string literal.
fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drain until the worker has said something, or give up.
    fn next_event(workers: &mut Workers) -> Option<(u64, &'static str, String)> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            workers.drain(|_, _| 0);
            if let Some(event) = workers.next_event() {
                return Some(event);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn a_worker_runs_on_its_own_thread_and_answers_messages() {
        let mut workers = Workers::default();
        let source =
            "onmessage = (e) => postMessage({ got: e.data.n * 2, worker: typeof document });";
        let id = workers
            .start(
                "https://site.test/w.js".to_owned(),
                false,
                Some(source.to_owned()),
            )
            .expect("a worker");
        workers.post(id, r#"["o",0,[["n",["n","21"]]]]"#.to_owned());
        let (from, kind, data) = next_event(&mut workers).expect("an answer");
        assert_eq!((from, kind), (id, "message"));
        assert!(data.contains(r#"["got",["n","42"]]"#), "{data}");
        assert!(data.contains(r#"["s","undefined"]"#), "{data}");
        workers.terminate(id);
    }

    #[test]
    fn an_uncaught_exception_is_an_error_event() {
        let mut workers = Workers::default();
        let id = workers
            .start(
                "https://site.test/w.js".to_owned(),
                false,
                Some("throw new Error('boom')".to_owned()),
            )
            .expect("a worker");
        let (from, kind, data) = next_event(&mut workers).expect("an error");
        assert_eq!((from, kind), (id, "error"));
        assert!(data.contains("boom"), "{data}");
    }

    #[test]
    fn a_page_has_only_so_many_workers() {
        let mut workers = Workers::default();
        for _ in 0..MAX_WORKERS {
            assert!(
                workers
                    .start(
                        "https://site.test/w.js".to_owned(),
                        false,
                        Some(String::new())
                    )
                    .is_some()
            );
        }
        assert!(
            workers
                .start(
                    "https://site.test/w.js".to_owned(),
                    false,
                    Some(String::new())
                )
                .is_none()
        );
    }
}
