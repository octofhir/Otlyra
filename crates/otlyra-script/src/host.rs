//! The isolate a page's scripts run in, and the one way in.
//!
//! One [`ScriptHost`] is one page's JavaScript world: one heap, one global
//! object, one microtask queue. Two `<script>` elements in a document share it,
//! which is what makes `var x` in the first visible to the second; two
//! documents never do.
//!
//! The host is Otter's Layer A — a thread-pinned [`Runtime`] the embedder
//! drives itself. That is the right layer for us today because the browser is
//! one `!Send` object on one thread and the script turn is synchronous with the
//! parse: the parser stops at a `<script>`, the script runs, the parser
//! continues. Layer B, the sendable isolate runner, is what a page agent
//! becomes when there is network and there are workers to wait for.
//!
//! Every entry point here does the same three things in the same order: arm the
//! watchdog, run the turn, drain the microtask queue exactly once. The last of
//! those is the HTML event loop's microtask checkpoint, and a task that skips
//! it leaves promise callbacks queued behind a page that has moved on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use otter_runtime::{ConsoleSinkHandle, InterruptHandle, OtterError, Runtime, SourceInput};

use crate::capabilities::{DenyPageCapabilities, page_capabilities};
use crate::console::tracing_console;

/// How long one uninterrupted script turn may run before it is stopped.
///
/// A number, not a policy: a page that computes for ten seconds without
/// yielding has stopped being a page. When there is a UI to ask, this becomes
/// the point at which the reader is offered the choice rather than the point at
/// which we make it for them.
pub const SCRIPT_TIME_LIMIT: Duration = Duration::from_secs(10);

/// The heap one page may fill before the engine refuses to grow it.
const MAX_HEAP_BYTES: u64 = 512 * 1024 * 1024;

/// How deep page script may recurse before the engine reports an overflow.
///
/// The point of a limit here is that the overflow is *reported* — a real stack
/// overflow in the isolate's thread is a crashed browser, and a `RangeError` is
/// a caught exception.
const MAX_STACK_DEPTH: u32 = 1024;

/// A script that did not finish the way it meant to.
#[derive(Debug, thiserror::Error)]
#[error("{specifier}: {source}")]
pub struct ScriptError {
    /// What the script was called — a URL, or a synthetic name for an inline
    /// one. This is what the error is attributed to, so it is what a reader
    /// sees and what a source map will later be looked up by.
    pub specifier: String,
    /// Whether the watchdog stopped it. An interrupted script failed because we
    /// stopped it, which is a different fact from a script that threw, and the
    /// page's author is only responsible for one of them.
    pub interrupted: bool,
    /// What the engine said.
    #[source]
    pub source: OtterError,
}

impl ScriptError {
    /// Where in the script it went wrong, as a byte range.
    ///
    /// Only a runtime diagnostic carries one. It is what turns "something threw
    /// somewhere in a minified bundle" into a line we can read.
    #[must_use]
    pub fn range(&self) -> Option<(u32, u32)> {
        match &self.source {
            OtterError::Runtime { diagnostic } => diagnostic.range.or(diagnostic.span),
            OtterError::Compile { diagnostics } => diagnostics
                .first()
                .and_then(|diagnostic| diagnostic.range.or(diagnostic.span)),
            _ => None,
        }
    }

    /// The call stack the engine unwound, innermost first.
    #[must_use]
    pub fn frames(&self) -> Vec<String> {
        let diagnostic = match &self.source {
            OtterError::Runtime { diagnostic } => diagnostic,
            _ => return Vec::new(),
        };
        diagnostic
            .frames
            .iter()
            .map(|frame| format!("{frame:?}"))
            .collect()
    }
}

/// What a script that finished left behind.
#[derive(Debug, Clone)]
pub struct ScriptOutcome {
    /// The completion value, rendered to text at the isolate boundary.
    ///
    /// A string rather than a value on purpose: nothing outside this crate may
    /// hold a JavaScript value, because a value is a GC handle and a GC handle
    /// held across an allocation is a use-after-move.
    pub completion: String,
    /// How long the turn took, microtask checkpoint included.
    pub duration: Duration,
}

/// One page's JavaScript world.
#[derive(Debug)]
pub struct ScriptHost {
    runtime: Runtime,
    watchdog: Watchdog,
}

impl ScriptHost {
    /// Build the isolate a page's scripts will run in.
    ///
    /// # Errors
    ///
    /// Returns whatever the engine says about a configuration it will not
    /// accept. There is nothing a page can do to reach this: it is our own
    /// settings being wrong.
    pub fn new() -> Result<Self, OtterError> {
        Self::with_console(tracing_console())
    }

    /// The same isolate, with somewhere else for its console to go.
    ///
    /// A test wants the page's output back as data rather than as log lines,
    /// and a devtools console will want it as events. Both are the same
    /// substitution.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn with_console(console: ConsoleSinkHandle) -> Result<Self, OtterError> {
        let builder = Runtime::builder()
            .capabilities(page_capabilities())
            .capability_hook(DenyPageCapabilities)
            .console_sink(console)
            .max_heap_bytes(MAX_HEAP_BYTES)
            .max_stack_depth(MAX_STACK_DEPTH);
        // The web platform Otter already has, then the part of it that knows
        // what a document is, which is ours. Order matters only in that both
        // are registered before the first script runs.
        let runtime = otter_web::with_web_apis(builder)
            .extension(&crate::dom::DOM_EXTENSION)
            .build()?;
        let watchdog = Watchdog::spawn(runtime.interrupt_handle(), SCRIPT_TIME_LIMIT);
        Ok(Self { runtime, watchdog })
    }

    /// Run one classic script, then take the microtask checkpoint.
    ///
    /// `specifier` names the script for diagnostics; for an inline one the
    /// caller passes something derived from the document's own URL, because
    /// "line 3" with nothing in front of it is not an error message.
    ///
    /// # Errors
    ///
    /// A syntax error, a thrown exception that nothing caught, or a turn the
    /// watchdog stopped. None of them is fatal to the document: the caller
    /// reports and carries on parsing, which is what a browser does.
    pub fn run_classic_script(
        &mut self,
        source: &str,
        specifier: &str,
    ) -> Result<ScriptOutcome, ScriptError> {
        let guard = self.watchdog.begin();
        let result = self
            .runtime
            .run_script(SourceInput::from_javascript(source), specifier);
        // The checkpoint runs whether or not the script itself succeeded: a
        // script can settle a promise and then throw, and the callbacks that
        // settlement queued are still owed a turn.
        let drained = self.runtime.run_microtasks();
        let interrupted = guard.finish();

        let outcome = result.and_then(|result| {
            drained.map(|()| ScriptOutcome {
                completion: result.completion_string().to_owned(),
                duration: result.duration,
            })
        });

        outcome.map_err(|source| ScriptError {
            specifier: specifier.to_owned(),
            interrupted,
            source,
        })
    }

    /// Run what the page deferred: `DOMContentLoaded`, `load`, animation-frame
    /// callbacks and pending timers, once each.
    ///
    /// This is not an event loop and does not pretend to be one — it is the
    /// single flush a document gets when its last byte has been parsed, so that
    /// the work a page hangs off those two events happens at all. The real
    /// timer wheel and frame loop replace it.
    ///
    /// # Errors
    ///
    /// Whatever escaped the flush itself. A callback that throws is reported by
    /// the flush and does not stop the others.
    pub fn flush_deferred(&mut self, fire_load_events: bool) -> Result<ScriptOutcome, ScriptError> {
        let source = if fire_load_events {
            "__otlyraFlushDeferred(true)"
        } else {
            "__otlyraFlushDeferred(false)"
        };
        self.run_classic_script(source, "<deferred work>")
    }

    /// Drain the microtask queue on its own.
    ///
    /// For the checkpoint that follows a task this crate did not run — a timer
    /// firing, an event dispatched from the UI — once those exist.
    ///
    /// # Errors
    ///
    /// Whatever a microtask threw past every handler.
    pub fn run_microtasks(&mut self) -> Result<(), ScriptError> {
        let guard = self.watchdog.begin();
        let result = self.runtime.run_microtasks();
        let interrupted = guard.finish();
        result.map_err(|source| ScriptError {
            specifier: "<microtask checkpoint>".to_owned(),
            interrupted,
            source,
        })
    }

    /// The isolate underneath, for the DOM bindings that land at M13.
    pub fn runtime_mut(&mut self) -> &mut Runtime {
        &mut self.runtime
    }
}

/// The thread that stops a script that will not stop itself.
///
/// It holds the engine's interrupt flag, which is the only thing about a
/// running isolate that is safe to touch from another thread: tripping it makes
/// the interpreter unwind at its next check, on its own thread, in its own
/// time. Nothing here reaches into the heap.
#[derive(Debug)]
struct Watchdog {
    tx: Sender<Turn>,
    tripped: Arc<AtomicBool>,
    interrupt: InterruptHandle,
    thread: Option<JoinHandle<()>>,
}

/// What the isolate's thread tells the watchdog.
#[derive(Debug, Clone, Copy)]
enum Turn {
    /// A script turn has started; start counting.
    Begin,
    /// It ended, one way or another; stop counting.
    End,
    /// The page is gone; so are you.
    Stop,
}

impl Watchdog {
    fn spawn(interrupt: InterruptHandle, limit: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<Turn>();
        let tripped = Arc::new(AtomicBool::new(false));
        let thread = {
            let tripped = Arc::clone(&tripped);
            let interrupt = interrupt.clone();
            std::thread::Builder::new()
                .name("otlyra-script-watchdog".to_owned())
                .spawn(move || {
                    loop {
                        // Idle: nothing is running, so wait indefinitely.
                        match rx.recv() {
                            Ok(Turn::Begin) => {}
                            Ok(Turn::End) => continue,
                            Ok(Turn::Stop) | Err(_) => return,
                        }
                        // Counting.
                        match rx.recv_timeout(limit) {
                            Ok(Turn::End | Turn::Begin) => continue,
                            Ok(Turn::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                            Err(RecvTimeoutError::Timeout) => {
                                tripped.store(true, Ordering::Release);
                                interrupt.interrupt();
                                tracing::warn!(
                                    target: "page.script",
                                    limit_secs = limit.as_secs_f32(),
                                    "page script ran past its budget and was interrupted",
                                );
                                // Wait for the turn we just stopped to admit it
                                // is over, so the next one is timed from its
                                // own start rather than from this one's.
                                match rx.recv() {
                                    Ok(Turn::Stop) | Err(_) => return,
                                    Ok(_) => {}
                                }
                            }
                        }
                    }
                })
                .expect("the watchdog thread is one thread with a default stack")
        };
        Self {
            tx,
            tripped,
            interrupt,
            thread: Some(thread),
        }
    }

    /// Arm the watchdog for one turn.
    fn begin(&self) -> TurnGuard<'_> {
        self.tripped.store(false, Ordering::Release);
        // A send that fails means the watchdog thread is gone. That costs this
        // turn its time limit and nothing else, and a page that cannot be
        // interrupted is still a page that runs — so it is a warning, not a
        // refusal to execute.
        if self.tx.send(Turn::Begin).is_err() {
            tracing::warn!(target: "page.script", "the script watchdog is not running");
        }
        TurnGuard { watchdog: self }
    }

    fn end(&self) -> bool {
        let _ = self.tx.send(Turn::End);
        let tripped = self.tripped.swap(false, Ordering::AcqRel);
        if tripped {
            // The flag stays set until somebody clears it, and the next turn
            // would unwind immediately on a script that had done nothing wrong.
            self.interrupt.reset();
        }
        tripped
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.tx.send(Turn::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Ends a turn even if the path out of it is a panic.
struct TurnGuard<'a> {
    watchdog: &'a Watchdog,
}

impl TurnGuard<'_> {
    /// End the turn and say whether the watchdog had to stop it.
    fn finish(self) -> bool {
        let tripped = self.watchdog.end();
        std::mem::forget(self);
        tripped
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.watchdog.end();
    }
}
