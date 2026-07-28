//! The parser's script point, answered.
//!
//! [`PageScripts`] is what the HTML parser calls when it stops at a
//! `<script>`: it owns the page's isolate, names each script for diagnostics,
//! runs it, and reports what happened. It reports rather than returns, because
//! a failing script does not fail a document — a browser that stopped parsing
//! at the first exception would render almost nothing on the modern web.

use otlyra_dom::{Document, NodeId};
use otlyra_html::ScriptRunner;
use otter_runtime::ConsoleSinkHandle;

use crate::host::{ScriptError, ScriptHost};

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

    fn build(document_url: String, console: Option<ConsoleSinkHandle>) -> Self {
        // What `location` is built from, and what a relative navigation is
        // resolved against.
        crate::dom::set_document_url(&document_url);
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
            document_url,
            host,
            console,
            seen: 0,
            failed: 0,
            mutated: false,
        }
    }

    /// Whether script changed the document while it ran.
    ///
    /// The page asks after the parse: a document script has rewritten needs its
    /// style, layout and paint run again, and a document it only read does not.
    #[must_use]
    pub fn mutated(&self) -> bool {
        self.mutated
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
        crate::dom::set_ready(true);
        let Some(host) = self.host.as_mut() else {
            return;
        };
        let outcome = crate::dom::loan(document, || host.flush_deferred(fire_load_events));
        self.mutated |= crate::dom::take_dirty();
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
        let specifier = document
            .get(element)
            .and_then(|node| node.element())
            .and_then(|element| element.attr("src"))
            .map_or_else(
                || format!("{} (external script {})", self.document_url, self.seen),
                str::to_owned,
            );
        let outcome = crate::dom::loan(document, || self.execute(source, &specifier));
        self.mutated |= crate::dom::take_dirty();
        match outcome {
            None => self.failed += 1,
            Some(Ok(())) => {}
            Some(Err(error)) => {
                self.failed += 1;
                tracing::error!(
                    target: "page.script",
                    %error,
                    range = ?error.range(),
                    frames = ?error.frames(),
                    "external script failed",
                );
            }
        }
    }

    /// Run one script and report the outcome. The name is what diagnostics
    /// attribute it to. `None` means there was no engine to run it in.
    fn execute(&mut self, source: &str, specifier: &str) -> Option<Result<(), ScriptError>> {
        let host = self.host.as_mut()?;
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
    fn run(&mut self, source: &str, _element: NodeId, document: &mut Document) {
        self.seen += 1;
        // Inline scripts have no URL of their own, so they borrow the
        // document's and are numbered in document order. That is what makes
        // "third script on this page" a thing an error message can say.
        let specifier = format!("{} (inline script {})", self.document_url, self.seen);
        // The document is the isolate's for exactly this turn. Anything the
        // script changed in it comes back with it, and the flag says whether
        // there was anything.
        let outcome = crate::dom::loan(document, || self.execute(source, &specifier));
        self.mutated |= crate::dom::take_dirty();
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

    fn run_external(&mut self, source: &str, element: NodeId, document: &mut Document) {
        PageScripts::run_external(self, source, element, document);
    }

    fn reset(&mut self) {
        // A fresh isolate, because the document this one was scripting is being
        // thrown away. Reusing it would leave the second pass's scripts looking
        // at globals the first pass's set.
        crate::dom::set_ready(false);
        // And the wrappers with it: they name nodes in the tree that is going,
        // and their roots belong to the isolate that is going.
        crate::dom::forget_wrappers();
        *self = Self::build(self.document_url.clone(), self.console.clone());
    }
}
