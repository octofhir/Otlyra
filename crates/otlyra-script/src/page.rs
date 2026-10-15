//! The parser's script point, answered.
//!
//! [`PageScripts`] is what the HTML parser calls when it stops at a
//! `<script>`: it owns the page's isolate, names each script for diagnostics,
//! runs it, and reports what happened. It reports rather than returns, because
//! a failing script does not fail a document — a browser that stopped parsing
//! at the first exception would render almost nothing on the modern web.

use std::time::{Duration, Instant};

use otlyra_dom::{Document, NodeId};
use otlyra_html::ScriptRunner;
use otter_runtime::ConsoleSinkHandle;

use crate::host::{ScriptError, ScriptHost};

/// Yield between complete timer tasks so a due cohort cannot monopolize content.
const TIMER_SLICE: Duration = Duration::from_millis(4);

/// One page's scripts, and the isolate they run in.
#[derive(Debug)]
pub struct PageScripts {
    /// The document's URL, which every script in it is named after.
    document_url: String,
    /// The isolate. Absent only if building one failed, which is our bug and
    /// not the page's — in that case the page renders without script rather
    /// than not at all.
    host: Option<ScriptHost>,
    /// Where the page's console goes, kept so that a second parse of the same
    /// document gets a new isolate writing to the same place.
    console: Option<ConsoleSinkHandle>,
    /// How many scripts have been handed to us.
    seen: usize,
    /// How many of them ended in an error.
    failed: usize,
    /// Whether any of them changed the document.
    mutated: bool,
    /// What this page's turns write down: whether the document changed, whether
    /// a frame is owed, where the page thinks it is, where it asked to go.
    ///
    /// This page's, not the thread's — it is lent to the isolate alongside the
    /// document, for exactly as long.
    state: crate::dom::PageState,
}

impl PageScripts {
    /// A script world for the document at `document_url`.
    #[must_use]
    pub fn new(document_url: impl Into<String>) -> Self {
        Self::build(document_url.into(), None)
    }

    /// The same, with somewhere else for the page's console to go.
    #[must_use]
    pub fn with_console(document_url: impl Into<String>, console: ConsoleSinkHandle) -> Self {
        Self::build(document_url.into(), Some(console))
    }

    /// Set the initial state before any page script runs, without an event.
    pub fn set_initial_hidden(&mut self, hidden: bool) {
        self.state.set_hidden(hidden);
    }

    /// Set the navigation's monotonic origin before parsing begins.
    pub fn set_initial_time_origin(&mut self, at: Instant) {
        self.state.set_clock(Some(crate::clock::PageClock::at(at)));
    }

    /// Set dimensions before the first page script, without a resize event.
    pub fn set_initial_viewport(&mut self, width: f64, height: f64, scale: f64) {
        self.state
            .set_viewport(crate::dom::WindowViewport::new(width, height, scale));
    }

    /// Resize on the script owner; handlers observe the new metrics.
    pub fn set_viewport(&mut self, document: &mut Document, width: f64, height: f64, scale: f64) {
        let viewport = crate::dom::WindowViewport::new(width, height, scale);
        let previous = self.state.viewport();
        if previous == viewport {
            return;
        }
        self.state.set_viewport(viewport);
        if self.seen == 0 || previous.width == viewport.width && previous.height == viewport.height
        {
            return;
        }
        let Self { host, state, .. } = self;
        let Some(host) = host.as_mut() else {
            return;
        };
        let outcome = crate::dom::loan(document, state, || {
            host.run_classic_script("window.dispatchEvent(new Event('resize'))", "<resize>")
        });
        self.absorb_turn();
        if let Err(error) = outcome {
            self.failed += 1;
            tracing::error!(target: "page.script", %error, "a resize listener failed");
        }
    }

    /// Dispatch one user-input event at `target` as its own script turn, and
    /// answer whether the page cancelled it.
    ///
    /// `kind` is an event type and `init` a JSON object, both the browser's
    /// own: page-supplied text never reaches the script source.
    pub fn dispatch_input(
        &mut self,
        document: &mut Document,
        target: NodeId,
        kind: &str,
        init: &str,
    ) -> bool {
        debug_assert!(kind.bytes().all(|byte| byte.is_ascii_lowercase()));
        if !kind.bytes().all(|byte| byte.is_ascii_lowercase()) {
            return false;
        }
        let Self { host, state, .. } = self;
        let Some(host) = host.as_mut() else {
            return false;
        };
        state.set_input_target(Some(target));
        let outcome = crate::dom::loan(document, state, || {
            host.run_classic_script(
                &format!("__otlyraDispatchInput('{kind}', {init})"),
                "<input>",
            )
        });
        self.state.set_input_target(None);
        self.absorb_turn();
        match outcome {
            Ok(outcome) => outcome.completion == "1",
            Err(error) => {
                self.failed += 1;
                tracing::error!(target: "page.script", %error, kind, "an input listener failed");
                false
            }
        }
    }

    /// A visibility transition is a script turn on this page's owner. Listeners
    /// see the new state; their mutations and microtasks follow the usual path.
    pub fn set_hidden(&mut self, document: &mut Document, hidden: bool) {
        if self.state.hidden() == hidden {
            return;
        }
        self.state.set_hidden(hidden);
        let Self { host, state, .. } = self;
        let Some(host) = host.as_mut() else {
            return;
        };
        let outcome = crate::dom::loan(document, state, || {
            host.run_classic_script(
                "document.dispatchEvent(new Event('visibilitychange', { bubbles: true }))",
                "<visibilitychange>",
            )
        });
        self.absorb_turn();
        if let Err(error) = outcome {
            self.failed += 1;
            tracing::error!(target: "page.script", %error, "a visibility listener failed");
        }
    }

    fn build(document_url: String, console: Option<ConsoleSinkHandle>) -> Self {
        // The wrapper table is this thread's, and a new page on the same thread
        // inherits it otherwise: entries naming a document nobody will lend
        // again, and roots belonging to an isolate that is gone.
        crate::dom::forget_wrappers();
        let built = match console.clone() {
            Some(console) => ScriptHost::with_console(console),
            None => ScriptHost::new(),
        };
        let host = match built {
            Ok(host) => Some(host),
            Err(error) => {
                tracing::error!(
                    target: "page.script",
                    url = %document_url,
                    %error,
                    "no script engine for this page",
                );
                None
            }
        };
        Self {
            // What `location` is built from, and what a relative navigation is
            // resolved against.
            state: crate::dom::PageState::new(document_url.clone()),
            document_url,
            host,
            console,
            seen: 0,
            failed: 0,
            mutated: false,
        }
    }

    /// Take back what the turn that just ended wrote into the page's state.
    ///
    /// Every entry into the isolate ends here. The state came back with the
    /// document when the loan ended; this folds the one flag the page keeps
    /// across turns out of it.
    fn absorb_turn(&mut self) {
        self.mutated |= self.state.take_dirty();
    }

    /// Whether script changed the document since this was last asked, clearing
    /// the answer.
    ///
    /// The page asks after the parse and again after every timer and animation
    /// frame: a document script has rewritten needs its style, layout and paint
    /// run again, and a document it only read does not. Taking rather than
    /// reading is what makes the second question about the second turn — a
    /// sticky flag would make every frame after the first mutation expensive
    /// forever.
    #[must_use]
    pub fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    /// How many scripts ran, and how many of those failed.
    #[must_use]
    pub fn tally(&self) -> (usize, usize) {
        (self.seen, self.failed)
    }

    /// The isolate, for a caller that has something else to run in it.
    pub fn host_mut(&mut self) -> Option<&mut ScriptHost> {
        self.host.as_mut()
    }

    /// The document is parsed; run what its scripts deferred.
    ///
    /// `DOMContentLoaded`, then `load`, then the animation-frame and timer
    /// callbacks that were registered while parsing — once each. A page does
    /// most of what it does to itself in exactly those, so a browser that never
    /// ran them would render every scripted page as its skeleton.
    pub fn document_finished(&mut self, document: &mut Document, fire_load_events: bool) {
        self.state.set_ready(true);
        let Self { host, state, .. } = self;
        let Some(host) = host.as_mut() else {
            return;
        };
        let outcome = crate::dom::loan(document, state, || host.flush_deferred(fire_load_events));
        self.absorb_turn();
        match outcome {
            Ok(outcome) => tracing::debug!(
                target: "page.script",
                callbacks = outcome.completion.as_str(),
                micros = outcome.duration.as_micros(),
                "deferred work ran",
            ),
            Err(error) => {
                self.failed += 1;
                tracing::error!(target: "page.script", %error, "deferred work failed");
            }
        }
    }

    /// When the browser must wake to run this page's next timer, if ever.
    ///
    /// Read between events: it is what turns "wait for the next window event"
    /// into "wait for the next window event, or until this page's `setTimeout`
    /// comes due, whichever is sooner".
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.host.as_ref()?.timers().next_deadline()
    }

    /// Run a bounded slice of due timer tasks in order, retaining the rest.
    ///
    /// Returns how many callbacks ran. A callback that throws is reported and
    /// the next one still runs, which is what the event loop does: one task's
    /// exception is not the next task's business.
    pub fn run_due_timers(&mut self, document: &mut Document) -> usize {
        self.run_due_timers_until(document, Instant::now() + TIMER_SLICE)
    }

    /// Run complete callbacks and their microtasks until a soft deadline.
    /// A single callback can overrun it; the watchdog retains its own limit.
    pub fn run_due_timers_until(&mut self, document: &mut Document, deadline: Instant) -> usize {
        let cutoff = Instant::now();
        let deadline = deadline.min(cutoff + TIMER_SLICE);
        if cutoff >= deadline {
            return 0;
        }
        // Destructured so the state can be lent while the isolate and the
        // failure tally are still reachable: they are three disjoint fields, and
        // borrowing `self` whole would make them one.
        let Self {
            host,
            state,
            failed,
            ..
        } = self;
        let Some(host) = host.as_mut() else {
            return 0;
        };
        if host.timers().next_deadline().is_none_or(|at| at > cutoff) {
            return 0;
        }
        let mut ran = 0;
        crate::dom::loan(document, state, || {
            while let Some(token) = host.timers().take_due(cutoff) {
                match host.fire_timer(token) {
                    // A token that named nothing was cancelled between coming
                    // due and being run, which the callback before it may have
                    // done. Not a task and not an error.
                    Ok(fired) => ran += usize::from(fired),
                    Err(error) => {
                        *failed += 1;
                        tracing::error!(target: "page.script", %error, "a timer callback failed");
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        });
        self.absorb_turn();
        ran
    }

    /// Run the animation-frame callbacks the page has asked for.
    ///
    /// `timestamp` is what `requestAnimationFrame` hands its callback: the time
    /// the frame is being drawn for, in milliseconds since the page began.
    /// Callbacks registered *by* these run in the next frame, not this one — a
    /// frame that ran them would be a loop with no display in it.
    pub fn run_frame(&mut self, document: &mut Document, timestamp: f64) -> usize {
        if self.state.hidden() {
            return 0;
        }
        // Cleared before the callbacks rather than after them: the frame they
        // asked for is the one being drawn now, and a callback that asks again
        // is asking for the next one. Clearing afterwards would throw that
        // second ask away and stop the loop dead.
        self.state.clear_frames_owed();
        let Self { host, state, .. } = self;
        let Some(host) = host.as_mut() else {
            return 0;
        };
        let outcome = crate::dom::loan(document, state, || {
            host.run_classic_script(&format!("__otlyraRunFrame({timestamp})"), "<frame>")
        });
        self.absorb_turn();
        match outcome {
            Ok(outcome) => outcome.completion.parse().unwrap_or(0),
            Err(error) => {
                self.failed += 1;
                tracing::error!(target: "page.script", %error, "a frame callback failed");
                0
            }
        }
    }

    /// Whether the page has asked for an animation frame it has not been given.
    ///
    /// Read once a frame, and answered without entering the isolate: a frame is
    /// drawn sixty times a second and almost none of them are owed a callback.
    #[must_use]
    pub fn frames_pending(&self) -> bool {
        self.host.is_some() && self.state.frames_owed()
    }

    /// Where this page's script asked to go, if it asked.
    ///
    /// Asked of the page rather than of the thread: with two tabs open, a
    /// navigation requested by one of them must not be answered by the other.
    pub fn take_navigation(&mut self) -> Option<crate::dom::Navigation> {
        self.state.take_navigation()
    }

    /// Run this page's deferred work to a standstill, or until `budget` is
    /// spent.
    ///
    /// For a caller with no event loop to be woken from — a screenshot, a test,
    /// a dump. It sleeps until the next deadline, which is exactly what a
    /// browser must never do and exactly what those callers mean by "let the
    /// page settle".
    ///
    /// Returns how many callbacks ran. A page that keeps scheduling more work
    /// stops at the budget rather than holding the process: `setInterval` is a
    /// loop, and a screenshot is not owed all of it.
    pub fn settle(&mut self, document: &mut Document, budget: Duration) -> usize {
        let deadline = Instant::now() + budget;
        let mut ran = 0;
        loop {
            ran += self.run_due_timers_until(document, deadline);
            if Instant::now() >= deadline {
                break;
            }
            let Some(next) = self.next_deadline() else {
                break;
            };
            if next >= deadline {
                break;
            }
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        ran
    }

    /// Run an external script whose bytes have arrived.
    ///
    /// It runs in the same isolate and sees everything the inline scripts left
    /// behind, which is what a browser gives it. What it does *not* get is the
    /// parse: our external scripts all behave as `defer` ones, because the
    /// bytes are fetched on another thread and the parse does not wait.
    pub fn run_external(&mut self, source: &str, element: NodeId, document: &mut Document) {
        self.seen += 1;
        // Named by its own address rather than the page's: an error in a
        // fetched bundle belongs to the bundle, and a stack frame that says
        // which file it is in is the difference between a diagnostic and a
        // riddle.
        let specifier = document.attr(element, "src").map_or_else(
            || format!("{} (external script {})", self.document_url, self.seen),
            str::to_owned,
        );
        let Self { host, state, .. } = self;
        state.set_current_script(Some(element));
        let outcome = crate::dom::loan(document, state, || Self::execute(host, source, &specifier));
        state.set_current_script(None);
        self.absorb_turn();
        match outcome {
            None => self.failed += 1,
            Some(Ok(())) => {}
            Some(Err(error)) => {
                self.failed += 1;
                tracing::error!(
                    target: "page.script",
                    %error,
                    at = error.excerpt(source).unwrap_or_default(),
                    range = ?error.range(),
                    frames = ?error.frames(),
                    "external script failed",
                );
            }
        }
    }

    /// Run one script and report the outcome. The name is what diagnostics
    /// attribute it to. `None` means there was no engine to run it in.
    ///
    /// Takes the isolate rather than the whole page, so that a caller can lend
    /// the page's state to that isolate at the same time.
    fn execute(
        host: &mut Option<ScriptHost>,
        source: &str,
        specifier: &str,
    ) -> Option<Result<(), ScriptError>> {
        let host = host.as_mut()?;
        Some(host.run_classic_script(source, specifier).map(|outcome| {
            tracing::debug!(
                target: "page.script",
                specifier,
                micros = outcome.duration.as_micros(),
                "script ran",
            );
        }))
    }
}

impl ScriptRunner for PageScripts {
    fn run(&mut self, source: &str, element: NodeId, document: &mut Document) {
        self.seen += 1;
        // Inline scripts have no URL of their own, so they borrow the
        // document's and are numbered in document order. That is what makes
        // "third script on this page" a thing an error message can say.
        let specifier = format!("{} (inline script {})", self.document_url, self.seen);
        // The document is the isolate's for exactly this turn. Anything the
        // script changed in it comes back with it, and the flag says whether
        // there was anything.
        let Self { host, state, .. } = self;
        state.set_current_script(Some(element));
        let outcome = crate::dom::loan(document, state, || Self::execute(host, source, &specifier));
        state.set_current_script(None);
        self.absorb_turn();
        match outcome {
            None => self.failed += 1,
            Some(Ok(())) => {}
            Some(Err(error)) => {
                self.failed += 1;
                if error.interrupted {
                    tracing::warn!(target: "page.script", %error, "script stopped by the watchdog");
                } else {
                    tracing::error!(
                        target: "page.script",
                        %error,
                        at = error.excerpt(source).unwrap_or_default(),
                        range = ?error.range(),
                        frames = ?error.frames(),
                        "script failed",
                    );
                }
            }
        }
    }

    fn document_finished(&mut self, document: &mut Document, more_scripts_coming: bool) {
        // The load events wait for the external scripts when there are any: a
        // page's `DOMContentLoaded` listeners are mostly registered by them, and
        // an event fired before its listener exists is an event nobody heard.
        PageScripts::document_finished(self, document, !more_scripts_coming);
    }

    fn next_deadline(&self) -> Option<Instant> {
        PageScripts::next_deadline(self)
    }

    fn run_due_timers(&mut self, document: &mut Document) -> usize {
        PageScripts::run_due_timers(self, document)
    }

    fn run_due_timers_until(&mut self, document: &mut Document, deadline: Instant) -> usize {
        PageScripts::run_due_timers_until(self, document, deadline)
    }

    fn frames_pending(&self) -> bool {
        PageScripts::frames_pending(self)
    }

    fn set_hidden(&mut self, document: &mut Document, hidden: bool) {
        PageScripts::set_hidden(self, document, hidden);
    }

    fn set_viewport(&mut self, document: &mut Document, width: f64, height: f64, scale: f64) {
        PageScripts::set_viewport(self, document, width, height, scale);
    }

    fn dispatch_input(
        &mut self,
        document: &mut Document,
        target: NodeId,
        kind: &str,
        init: &str,
    ) -> bool {
        PageScripts::dispatch_input(self, document, target, kind, init)
    }

    fn take_mutated(&mut self) -> bool {
        PageScripts::take_mutated(self)
    }

    fn take_navigation(&mut self) -> Option<crate::dom::Navigation> {
        PageScripts::take_navigation(self)
    }

    fn run_frame(&mut self, document: &mut Document, timestamp: f64) -> usize {
        PageScripts::run_frame(self, document, timestamp)
    }

    fn run_frame_at(&mut self, document: &mut Document, timestamp: f64, at: Instant) -> usize {
        let timestamp = self
            .state
            .clock()
            .map_or(timestamp, |clock| clock.relative(at));
        PageScripts::run_frame(self, document, timestamp)
    }

    fn settle(&mut self, document: &mut Document, budget: Duration) -> usize {
        PageScripts::settle(self, document, budget)
    }

    fn run_external(&mut self, source: &str, element: NodeId, document: &mut Document) {
        PageScripts::run_external(self, source, element, document);
    }

    fn reset(&mut self) {
        // A fresh isolate, because the document this one was scripting is being
        // thrown away. Reusing it would leave the second pass's scripts looking
        // at globals the first pass's set. The state goes with it — `build`
        // makes a new one, back to `readyState: "loading"`.
        //
        // And the wrappers too: they name nodes in the tree that is going,
        // and their roots belong to the isolate that is going.
        crate::dom::forget_wrappers();
        let hidden = self.state.hidden();
        let viewport = self.state.viewport();
        let clock = self.state.clock();
        *self = Self::build(self.document_url.clone(), self.console.clone());
        self.state.set_hidden(hidden);
        self.state.set_viewport(viewport);
        self.state.set_clock(clock);
    }
}
