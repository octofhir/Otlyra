//! Our own loop over html5ever's tokenizer.
//!
//! html5ever ships one (`driver::Parser::loop_until_done`), and it carries the
//! comment *"FIXME: Properly support `</script>` and encoding indicators somehow"*
//! before discarding both. A browser needs both: `<script>` suspends parsing until
//! the script has run, and `document.write` splices its output in *ahead of* the
//! network bytes. Neither is expressible from the outside, so we drive
//! `Tokenizer::feed` over our own queues instead.
//!
//! Two queues, in priority order. `script_input` is what `document.write` pushes
//! into and is drained first; `network_input` is everything that arrived over the
//! wire. That ordering is the whole of the splice.

use std::collections::HashMap;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{Tokenizer, TokenizerOpts};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts, create_element};
use html5ever::{Attribute, QualName, TokenizerResult, buffer_queue::BufferQueue};
use otlyra_dom::{Document, DomSink, NodeData, NodeId};

/// Somewhere a page's script asked to go.
///
/// Script cannot navigate: it can only say so, and the browser decides. That is
/// not politeness — the isolate holds the document for the length of one turn
/// and the navigation replaces the document, so a binding that navigated where
/// it stands would be destroying the thing it is standing on.
///
/// It lives beside [`ScriptRunner`] rather than in the engine crate because it
/// is what the script point reports back, and the browser reads it from a
/// `dyn ScriptRunner` without knowing which engine is under it.
#[derive(Debug, Clone)]
pub enum Navigation {
    /// `location.href = …`, `location.assign`, `location.replace`.
    Url {
        /// Where to, as the page spelled it. Resolving it against the
        /// document's own address is the browser's.
        href: String,
        /// Whether this replaces the current history entry.
        replace: bool,
    },
    /// `form.submit()`.
    Submit {
        /// The `<form>` element. Its fields and its `action` are read from the
        /// document, which is where they are.
        form: NodeId,
    },
    /// `location.reload()`.
    Reload,
}

/// Something that can execute a script the parser stopped at.
///
/// The parser knows *when* a script runs — that is the whole of its job at a
/// `<script>` — and nothing about *how*. The engine lives on the other side of
/// this trait, so this crate's build graph still has no JavaScript in it.
pub trait ScriptRunner {
    /// Execute `source`, which is the text content of `element`.
    ///
    /// `document` is the tree as it stands — half-built, with everything before
    /// this `<script>` in it and nothing after — and the script may change it.
    ///
    /// Errors are the runner's to report: a script that throws does not stop
    /// the parse, so there is nothing useful to hand back here.
    fn run(&mut self, source: &str, element: NodeId, document: &mut Document);

    /// The last byte has been parsed.
    ///
    /// What a page defers — the load events, the callbacks its scripts
    /// registered — happens after this, and the parser is the only thing that
    /// knows when "after" is.
    /// `more_scripts_coming` says whether external scripts are still to be
    /// fetched and run. When they are, the load events wait for them, which is
    /// what they are for: a page's `DOMContentLoaded` listeners are mostly
    /// registered by exactly those scripts.
    fn document_finished(&mut self, document: &mut Document, more_scripts_coming: bool) {
        let _ = (document, more_scripts_coming);
    }

    /// Everything the document waits for has arrived: fire `load` if the
    /// runner was holding it.
    fn release_load_event(&mut self, document: &mut Document) {
        self.document_finished(document, false);
    }

    /// Run an external script whose bytes have arrived.
    fn run_external(&mut self, source: &str, element: NodeId, document: &mut Document) {
        let _ = (source, element, document);
    }

    /// When the page's next timer comes due, if it has one.
    ///
    /// These scheduling hooks are the event loop's, not the parser's. They
    /// are on this trait because the runner outlives the parse — a page keeps
    /// running after its last byte — and whoever drives the parse is left
    /// holding it as a `dyn ScriptRunner`. A parser with no engine attached
    /// answers all of them with nothing, which is what a browser with scripting
    /// disabled does.
    fn next_deadline(&self) -> Option<std::time::Instant> {
        None
    }

    /// Run the timers that are due now, in order; returns how many ran.
    fn run_due_timers(&mut self, document: &mut Document) -> usize {
        let _ = document;
        0
    }

    /// Run due timer tasks until a soft deadline, retaining work not yet run.
    /// Existing runners keep their own scheduling behavior by default.
    fn run_due_timers_until(
        &mut self,
        document: &mut Document,
        deadline: std::time::Instant,
    ) -> usize {
        let _ = deadline;
        self.run_due_timers(document)
    }

    /// Whether the page has asked for an animation frame and not had one.
    fn frames_pending(&self) -> bool {
        false
    }

    /// Change document visibility between script turns, firing
    /// `visibilitychange` only when the state changes.
    fn set_hidden(&mut self, document: &mut Document, hidden: bool) {
        let _ = (document, hidden);
    }

    /// Dispatch a user-input event of type `kind` at `target`, with `init` a
    /// JSON object of its init dictionary. Answers whether the page cancelled
    /// it, which is what decides whether the browser's own action follows.
    fn dispatch_input(
        &mut self,
        document: &mut Document,
        target: NodeId,
        kind: &str,
        init: &str,
    ) -> bool {
        let _ = (document, target, kind, init);
        false
    }

    /// Run an automation client's script in the page's realm, as WebDriver
    /// BiDi's `script.evaluate` (`call` is `None`) or `script.callFunction`
    /// (`call` is the arguments and `this` as JSON LocalValues). Answers the
    /// BiDi evaluate result as JSON text, or why there is none.
    fn automation_script(
        &mut self,
        document: &mut Document,
        source: &str,
        call: Option<(&str, &str)>,
        await_promise: bool,
    ) -> Result<String, String> {
        let _ = (document, source, call, await_promise);
        Err("this page has no script engine".to_owned())
    }

    /// The detached `<img>` elements script gave a `src` since this was last
    /// asked.
    fn take_image_requests(&mut self) -> Vec<NodeId> {
        Vec::new()
    }

    /// The `<canvas>` bitmaps script drew into since this was last asked,
    /// each as it now is.
    fn take_canvas_frames(&mut self) -> Vec<(NodeId, otlyra_dom::Bitmap)> {
        Vec::new()
    }

    /// Where script asked the window to scroll to since this was last asked.
    fn take_scroll_request(&mut self) -> Option<(f64, f64)> {
        None
    }

    /// Whether the page listens for scroll events.
    fn wants_scroll_events(&self) -> bool {
        false
    }

    /// Whether the page listens for wheel events.
    fn wants_wheel_events(&self) -> bool {
        false
    }

    /// Whether an intersection or resize observer is watching, so the frame
    /// step is owed whenever what was drawn moved.
    fn wants_geometry_updates(&self) -> bool {
        false
    }

    /// The focus script asked for since this was last asked: an element, or
    /// `None` inside for a blur.
    fn take_focus_request(&mut self) -> Option<Option<NodeId>> {
        None
    }

    /// Whether the page listens for pointer movement at all.
    fn wants_pointer_moves(&self) -> bool {
        false
    }

    /// The CSS viewport changed between script turns or while parsing is paused.
    fn set_viewport(&mut self, document: &mut Document, width: f64, height: f64, scale: f64) {
        let _ = (document, width, height, scale);
    }

    /// Whether script changed the document since this was last asked.
    ///
    /// Asked after every turn the browser gives the page, and answered by
    /// taking: what the caller wants to know is whether *this* turn made the
    /// style, layout and paint built from the document stale, not whether any
    /// turn ever did.
    fn take_mutated(&mut self) -> bool {
        false
    }

    /// Where *this page's* script asked to go, if it asked.
    ///
    /// Asked of the page rather than of the thread: with two tabs open, a
    /// navigation one of them requested must not be answered by the other.
    fn take_navigation(&mut self) -> Option<Navigation> {
        None
    }

    /// Where the session history moved the document without loading it —
    /// `history.pushState`, `replaceState`, a traversal between entries the
    /// document made — since the last time this was asked, in order.
    fn take_url_changes(&mut self) -> Vec<UrlChange> {
        Vec::new()
    }

    /// Move the document `delta` entries through the session history entries
    /// it made itself, as the browser's back and forward buttons do. False
    /// when it has no such entry there — it was loaded again since — and the
    /// browser loads the entry instead.
    fn traverse_history(&mut self, document: &mut Document, delta: i32) -> bool {
        let _ = (document, delta);
        false
    }

    /// Run the animation-frame callbacks the page asked for.
    fn run_frame(&mut self, document: &mut Document, timestamp: f64) -> usize {
        let _ = (document, timestamp);
        0
    }

    /// Give clock-aware runners the shared monotonic instant of this frame.
    /// Existing runners retain their elapsed timestamp through the default.
    fn run_frame_at(
        &mut self,
        document: &mut Document,
        timestamp: f64,
        at: std::time::Instant,
    ) -> usize {
        let _ = at;
        self.run_frame(document, timestamp)
    }

    /// Run the page's deferred work to a standstill, or until `budget` is spent.
    ///
    /// For a caller with no event loop to be woken from: a screenshot, a dump,
    /// a test.
    fn settle(&mut self, document: &mut Document, budget: std::time::Duration) -> usize {
        let _ = (document, budget);
        0
    }

    /// The parser met a `<script type="module">`, inline or with a `src`.
    ///
    /// A module never stops the parse: the runner records it, asks for what
    /// its graph needs through [`Self::take_module_fetches`], and runs it when
    /// the graph is in — after the parse for an ordinary one, as soon as it
    /// is ready for an `async` one.
    fn module_script(&mut self, element: NodeId, document: &mut Document) {
        let _ = (element, document);
    }

    /// The parser met a `<script type="importmap">`. It applies to every
    /// module specifier resolved after it.
    fn import_map(&mut self, element: NodeId, document: &mut Document) {
        let _ = (element, document);
    }

    /// Module sources the page needs that nobody has asked for yet: the
    /// entries and dependencies of its module scripts and the targets of its
    /// `import()` calls. Taken, so each address is handed out once; whoever
    /// fetches them answers each through [`Self::deliver_module`].
    fn take_module_fetches(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// The fetch of the module at `url` came back. `Err` is a network
    /// failure, an error status or a response that is not JavaScript.
    ///
    /// The runner runs whatever this completes: module scripts whose graph
    /// is now whole and `import()` calls that were waiting.
    fn deliver_module(
        &mut self,
        document: &mut Document,
        url: &str,
        response: Result<ModuleResponse, String>,
    ) {
        let _ = (document, url, response);
    }

    /// Whether module scripts of this document have still to run. The
    /// window's `load` waits for them.
    fn modules_pending(&self) -> bool {
        false
    }

    /// Throw away everything script has done and start again.
    ///
    /// Called when the document is about to be parsed a second time because
    /// its declared encoding turned out to disagree with the guess. The first
    /// pass's document is discarded, so its scripts' effects must be too — a
    /// browser that reloads a page reloads its script world with it.
    fn reset(&mut self) {}

    /// The requests the page's script made with `fetch` or
    /// `XMLHttpRequest` since this was last asked. The browser fetches each
    /// and answers with [`ScriptRunner::deliver_page_response`].
    fn take_page_requests(&mut self) -> Vec<PageRequest> {
        Vec::new()
    }

    /// The request the page numbered `id` was answered: a response, or why
    /// there is none — a network failure, or a response the page may not
    /// read. The runner settles whatever was waiting on it.
    fn deliver_page_response(
        &mut self,
        document: &mut Document,
        id: u64,
        response: Result<PageResponse, String>,
    ) {
        let _ = (document, id, response);
    }
}

/// The document's address moving without a load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlChange {
    /// The document's address now.
    pub url: String,
    /// What moved it.
    pub kind: HistoryMove,
}

/// How a document's address moved without a load (HTML §7.4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryMove {
    /// `pushState`: a new entry after the current one, which drops any after it.
    Push,
    /// `replaceState`: the current entry's address changed.
    Replace,
    /// The page's own `history.go`/`back`/`forward`, by this many entries.
    Traverse(i32),
    /// A traversal the browser asked for, whose entry it already moved to.
    Followed,
}

/// A request a page's script made, as the browser is to send it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// The page's own number for it, which the answer carries back.
    pub id: u64,
    /// The method, uppercase.
    pub method: String,
    /// The absolute address, resolved against the document.
    pub url: String,
    /// The headers script set, in order.
    pub headers: Vec<(String, String)>,
    /// The body, for a method that has one.
    pub body: Option<Vec<u8>>,
    /// Whether a `<script>` element asked for it: fetched as a script
    /// (no-cors), and given to the page whatever its origin, since the page
    /// runs it rather than reads it.
    pub script: bool,
}

/// What came back for a [`PageRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageResponse {
    /// The HTTP status; 200 for a response that has none, like a file.
    pub status: u16,
    /// Where it came from after redirects.
    pub url: String,
    /// The response headers, in order, names lowercase.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// A module source that arrived, as the network delivered it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleResponse {
    /// Where the response came from after redirects: the module's own URL,
    /// which its relative imports and `import.meta.url` use.
    pub final_url: String,
    /// The response's `Content-Type`, already checked to be JavaScript.
    pub content_type: Option<String>,
    /// The decoded source.
    pub text: String,
}

/// The source of every external script whose bytes are already in hand, keyed by
/// `src` exactly as the document spells it.
///
/// Filled by whoever fetched them — see [`crate::prescan::prescan_scripts`],
/// which reads the addresses out of the bytes before the parse so that they can
/// be. A `src` that is not in here is a script the parse could not run at its
/// own point, and the caller runs it late.
pub type ExternalSources = HashMap<String, String>;

/// A parser fed decoded text.
///
/// Bytes are decoded before they get here — see [`crate::parse`], which does
/// encoding determination first.
pub struct HtmlParser {
    tokenizer: Tokenizer<TreeBuilder<NodeId, DomSink>>,
    network_input: BufferQueue,
    script_input: BufferQueue,
    scripts_seen: usize,
    scripts_run: usize,
    /// The `<script src>` elements the parse went past, in document order.
    external_scripts: Vec<NodeId>,
    /// Those of them whose source was not in hand, in document order.
    deferred_scripts: Vec<NodeId>,
    /// The sources that were.
    external_sources: ExternalSources,
    /// The `<script src>` the parse is stopped at, if it is stopped at one.
    ///
    /// This is the parser-blocking script, and it is the whole of what makes a
    /// browser show half a page: everything before it is in the tree and can be
    /// painted, and nothing after it is even tokenized until it has run.
    blocked: Option<NodeId>,
    encoding_indicator: Option<String>,
    script_runner: Option<Box<dyn ScriptRunner>>,
}

impl HtmlParser {
    /// A parser writing into a fresh document.
    pub fn new() -> Self {
        Self::with_document(Document::new())
    }

    /// A parser writing into `document`.
    pub fn with_document(document: Document) -> Self {
        let sink = DomSink::with_document(document);
        let tree_builder = TreeBuilder::new(sink, TreeBuilderOpts::default());
        Self {
            tokenizer: Tokenizer::new(tree_builder, TokenizerOpts::default()),
            network_input: BufferQueue::default(),
            script_input: BufferQueue::default(),
            scripts_seen: 0,
            scripts_run: 0,
            external_scripts: Vec::new(),
            deferred_scripts: Vec::new(),
            external_sources: ExternalSources::new(),
            blocked: None,
            encoding_indicator: None,
            script_runner: None,
        }
    }

    /// A parser for a fragment, as `innerHTML` parses one.
    ///
    /// A fragment is parsed *as if* it were inside `context`, and that changes the
    /// answer completely: the same bytes inside a `<table>` and inside a `<div>`
    /// produce different trees, and inside a `<title>` they produce no elements at
    /// all. The context also decides which tokenizer state to start in, which is
    /// why this cannot be the document parser with a different root.
    pub fn for_fragment(document: Document, context: QualName, attrs: Vec<Attribute>) -> Self {
        Self::for_fragment_with_form(document, context, attrs, false)
    }

    /// A fragment parser that knows whether the context has an HTML form
    /// ancestor. The parser's form pointer prevents markup APIs from creating
    /// nested forms that ordinary page parsing would reject.
    pub fn for_fragment_with_form(
        document: Document,
        context: QualName,
        attrs: Vec<Attribute>,
        form_ancestor: bool,
    ) -> Self {
        let context_is_form = context.ns == html5ever::ns!(html) && &*context.local == "form";
        let quirks_mode = document.quirks_mode();
        let sink = DomSink::with_document(document);
        let context_element = create_element(&sink, context, attrs);
        let form = if context_is_form {
            Some(context_element)
        } else if form_ancestor {
            Some(create_element(
                &sink,
                QualName::new(
                    None,
                    html5ever::ns!(html),
                    html5ever::LocalName::from("form"),
                ),
                Vec::new(),
            ))
        } else {
            None
        };

        let tree_builder = TreeBuilder::new_for_fragment(
            sink,
            context_element,
            form,
            TreeBuilderOpts {
                quirks_mode,
                ..TreeBuilderOpts::default()
            },
        );
        let tokenizer_opts = TokenizerOpts {
            initial_state: Some(tree_builder.tokenizer_state_for_context_elem(true)),
            ..TokenizerOpts::default()
        };

        Self {
            tokenizer: Tokenizer::new(tree_builder, tokenizer_opts),
            network_input: BufferQueue::default(),
            script_input: BufferQueue::default(),
            scripts_seen: 0,
            scripts_run: 0,
            external_scripts: Vec::new(),
            deferred_scripts: Vec::new(),
            external_sources: ExternalSources::new(),
            blocked: None,
            encoding_indicator: None,
            // Tokenization follows a scripting-enabled browser (notably for
            // noscript), but markup APIs never execute the scripts they parse.
            script_runner: None,
        }
    }

    /// Feed decoded text that arrived over the network, into `document`.
    ///
    /// The arena is the caller's and is lent for the pump. It comes back
    /// holding everything parsed so far — including when the parse stopped part
    /// way, at a `<script src>` whose bytes have not arrived: see
    /// [`Self::blocked_on`], which is what a caller checks afterwards.
    pub fn feed(&mut self, document: &mut Document, text: StrTendril) {
        self.network_input.push_back(text);
        self.with_arena(document, Self::pump);
    }

    /// Lend `document` to the tree builder for one call.
    ///
    /// Moved in and moved back, so a caller still holding it cannot reach it
    /// while the parser can. The restoring is a `Drop`, because a tree builder
    /// that panics mid-token must still give the tree back rather than leave
    /// the caller with an empty one.
    fn with_arena<R>(&mut self, document: &mut Document, run: impl FnOnce(&mut Self) -> R) -> R {
        let taken = std::mem::take(document);
        let placeholder = self.tokenizer.sink.sink.lend(taken);
        drop(placeholder);

        struct Restore<'a, 'b> {
            parser: &'a mut HtmlParser,
            document: &'b mut Document,
        }
        impl Drop for Restore<'_, '_> {
            fn drop(&mut self) {
                *self.document = self.parser.tokenizer.sink.sink.reclaim();
            }
        }

        let restore = Restore {
            parser: self,
            document,
        };
        run(restore.parser)
    }

    /// Take the arena this parser was built over.
    ///
    /// For a caller that did not supply one — a fragment parser builds its
    /// context element into a document of its own, and that document is the one
    /// the fragment must be parsed into.
    pub fn take_document(&mut self) -> Document {
        self.tokenizer.sink.sink.reclaim()
    }

    /// The `<script src>` the parse is stopped at, if it is stopped at one.
    pub fn blocked_on(&self) -> Option<NodeId> {
        self.blocked
    }

    /// Update the script world while parsing waits for an external script,
    /// returning whether the visibility listener changed the document.
    pub fn set_hidden(&mut self, document: &mut Document, hidden: bool) -> bool {
        if let Some(runner) = self.script_runner.as_mut() {
            runner.set_hidden(document, hidden);
            runner.take_mutated()
        } else {
            false
        }
    }

    /// Forward dimensions to the runner retained by a paused parser.
    pub fn set_viewport(
        &mut self,
        document: &mut Document,
        width: f64,
        height: f64,
        scale: f64,
    ) -> bool {
        if let Some(runner) = self.script_runner.as_mut() {
            runner.set_viewport(document, width, height, scale);
            runner.take_mutated()
        } else {
            false
        }
    }

    /// Run the script the parse is stopped at, and carry on tokenizing.
    ///
    /// This is the moment a browser has been waiting for: the bytes of the
    /// parser-blocking script arrived, it runs against the half-built tree, and
    /// everything it did is in the document before the next token is seen —
    /// which is the whole difference between a script and a `defer`.
    pub fn supply_script(&mut self, document: &mut Document, source: &str) {
        let Some(element) = self.blocked.take() else {
            return;
        };
        self.with_arena(document, |parser| {
            if !source.trim().is_empty()
                && let Some(runner) = parser.script_runner.as_mut()
            {
                parser.scripts_run += 1;
                let mut document = parser.tokenizer.sink.sink.document_mut();
                runner.run_external(source, element, &mut document);
            }
            parser.pump();
        });
    }

    /// Carry on without running it: its fetch failed, or it is not ours to run.
    pub fn skip_script(&mut self, document: &mut Document) {
        if let Some(element) = self.blocked.take() {
            self.deferred_scripts.push(element);
            self.with_arena(document, Self::pump);
        }
    }

    /// Splice text in ahead of the network bytes, as `document.write` does.
    ///
    /// Unused until script runs at M12; it is here because it is the reason the
    /// queues are separate, and a driver with one queue cannot grow this later
    /// without being rewritten.
    pub fn write(&mut self, document: &mut Document, text: StrTendril) {
        self.script_input.push_front(text);
        self.with_arena(document, Self::pump);
    }

    /// Attach the thing that runs scripts.
    ///
    /// Without one, a `<script>` is an element and nothing happens at it.
    #[must_use]
    pub fn with_script_runner(mut self, runner: Box<dyn ScriptRunner>) -> Self {
        self.script_runner = Some(runner);
        self
    }

    /// Hand the parser the sources of the scripts the document links to.
    ///
    /// With them, a `<script src>` runs where it stands, which is what a plain
    /// one is supposed to do: everything after it in the document sees what it
    /// did. Without them it is a `defer`, and an inline script that calls into a
    /// bundle is calling into nothing.
    #[must_use]
    pub fn with_external_sources(mut self, sources: ExternalSources) -> Self {
        self.external_sources = sources;
        self
    }

    /// Take the script runner back.
    ///
    /// The isolate outlives the parse — timers, events and everything else a
    /// page does after its last byte arrives run in it — so whoever attached it
    /// gets it back rather than losing it with the parser.
    pub fn take_script_runner(&mut self) -> Option<Box<dyn ScriptRunner>> {
        self.script_runner.take()
    }

    /// How many `<script>` elements the tokenizer stopped at.
    pub fn scripts_seen(&self) -> usize {
        self.scripts_seen
    }

    /// The `<script src>` elements, in document order.
    ///
    /// Fetching them is the caller's: this crate has no network in its build
    /// graph and a parser that waited for one would be a parser that blocks.
    pub fn external_scripts(&self) -> &[NodeId] {
        &self.external_scripts
    }

    /// The external scripts that did *not* run at their own point, because their
    /// source was not in hand when the parse reached them.
    ///
    /// These are the ones left for the caller to fetch and run afterwards — the
    /// old `defer`-shaped path, now the exception rather than the rule.
    pub fn deferred_scripts(&self) -> &[NodeId] {
        &self.deferred_scripts
    }

    /// How many of them were actually executed.
    ///
    /// The difference is the scripts we declined: external ones, and those
    /// written in something that is not classic JavaScript.
    pub fn scripts_run(&self) -> usize {
        self.scripts_run
    }

    /// Hand the runner and the tree to `run`, once the tree is not borrowed.
    fn with_runner(&mut self, run: impl FnOnce(&mut dyn ScriptRunner, &mut Document)) {
        let Some(runner) = self.script_runner.as_mut() else {
            return;
        };
        let mut document = self.tokenizer.sink.sink.document_mut();
        run(runner.as_mut(), &mut document);
    }

    /// The runner a stopped parse is holding, for the module sources that
    /// arrive while it waits.
    pub fn runner_mut(&mut self) -> Option<&mut dyn ScriptRunner> {
        match self.script_runner.as_mut() {
            Some(runner) => Some(runner.as_mut()),
            None => None,
        }
    }

    /// Run the script the tokenizer stopped at, if it is one we run.
    fn execute(&mut self, element: NodeId) {
        // Whatever becomes of it here, a script the parser met is started: a
        // page that moves it later does not run it a second time.
        {
            let mut document = self.tokenizer.sink.sink.document_mut();
            otlyra_dom::DocumentMutator::new(&mut document).mark_script_started(element);
        }
        if self.script_runner.is_none() {
            return;
        }

        // The borrow of the tree ends before the runner is called. A script can
        // reach back into the document — that is the point of having one — and
        // a `Ref` still outstanding would make the first such reach a panic.
        // A module script and an import map are the runner's to keep: neither
        // is run here, and neither stops the parse.
        let module_kind = {
            let document = self.tokenizer.sink.sink.document();
            document.node(element).element().and_then(|data| {
                let kind = data.attr("type")?.trim();
                if kind.eq_ignore_ascii_case("module") {
                    Some(true)
                } else if kind.eq_ignore_ascii_case("importmap") {
                    Some(false)
                } else {
                    None
                }
            })
        };
        match module_kind {
            Some(true) => {
                return self
                    .with_runner(|runner, document| runner.module_script(element, document));
            }
            Some(false) => {
                return self.with_runner(|runner, document| runner.import_map(element, document));
            }
            None => {}
        }

        let mut external = false;
        let source = {
            let document = self.tokenizer.sink.sink.document();
            let node = document.node(element);
            let Some(data) = node.element() else {
                return;
            };

            // A script that names a file rather than carrying one. Its bytes
            // were asked for before the parse began — see
            // [`crate::prescan::prescan_scripts`] — so the usual case is that
            // they are here and it runs where it stands, in document order with
            // the inline ones around it. One the prescan did not see is handed
            // back to the caller to fetch, and runs after the parse.
            // `type` names a language. Modules and import maps went to the
            // runner above; of the rest the only ones executed here are the
            // ones the spec calls classic JavaScript, and a
            // `<script type="text/template">` full of markup is data.
            match data.attr("type").map(str::trim) {
                None | Some("") => {}
                Some(kind) if is_classic_javascript(kind) => {}
                Some(kind) => {
                    tracing::debug!(target: "page.script", kind, "script of a type we do not run");
                    return;
                }
            }
            // A browser that runs modules does not run the fallback written
            // for browsers that do not.
            if data.attr("nomodule").is_some() {
                tracing::debug!(target: "page.script", "a nomodule script is not run");
                return;
            }

            // `defer` and `async` take an external script out of the parse
            // (HTML §4.12.1.1): it neither stops the tokenizer nor runs where
            // it stands, even when its bytes are already here. A Next.js page
            // reads `#__NEXT_DATA__`, written at the end of the body, from a
            // deferred script in the head. Both run after the parse, in
            // document order — `defer`'s order; `async` may run in any.
            if data.attr("src").is_some()
                && (data.attr("defer").is_some() || data.attr("async").is_some())
            {
                self.external_scripts.push(element);
                self.deferred_scripts.push(element);
                return;
            }

            if let Some(src) = data.attr("src") {
                let src = src.trim().to_owned();
                self.external_scripts.push(element);
                let Some(source) = self.external_sources.get(&src) else {
                    // The parser-blocking case: the bytes are not here, so
                    // nothing after this script may be tokenized until they
                    // are. The parse stops where it stands and the caller is
                    // told which script it is waiting on.
                    tracing::debug!(
                        target: "page.script",
                        %src,
                        "the parse is blocked on a script"
                    );
                    self.blocked = Some(element);
                    return;
                };
                external = true;
                source.clone()
            } else {
                let mut source = String::new();
                let mut child = node.first_child();
                while let Some(id) = child {
                    if let NodeData::Text(text) = &document.node(id).data {
                        source.push_str(text);
                    }
                    child = document.node(id).next_sibling();
                }
                source
            }
        };

        if source.trim().is_empty() {
            return;
        }

        self.scripts_run += 1;
        // The document goes with the script, because a script that cannot reach
        // the document is a script that can only print. The borrow is held for
        // exactly the turn: the tokenizer is stopped, so nothing else is
        // touching the tree while it runs.
        let Some(runner) = self.script_runner.as_mut() else {
            return;
        };
        let mut document = self.tokenizer.sink.sink.document_mut();
        if external {
            runner.run_external(&source, element, &mut document);
        } else {
            runner.run(&source, element, &mut document);
        }
    }

    /// The first encoding label a `<meta>` in the document declared, if any.
    ///
    /// The prescan usually gets there first. This is what catches the rest: a
    /// declaration past the first 1024 bytes, or one whose bytes only became a
    /// `<meta>` after the tokenizer resolved a character reference. Deciding what to
    /// do about it — keep going or start over with the right encoding — is the
    /// caller's, because only the caller still has the bytes.
    pub fn encoding_indicator(&self) -> Option<&str> {
        self.encoding_indicator.as_deref()
    }

    /// Finish parsing and take the document, and the runner with it.
    ///
    /// Both, because the two are needed together and in this order: the last
    /// byte is what makes the load events due, and the isolate that runs them
    /// keeps running afterwards — its timers, its listeners and its frames are
    /// in there. A caller that took the runner back *before* finishing would
    /// get a document whose load events never fired.
    pub fn finish(mut self, document: &mut Document) -> Option<Box<dyn ScriptRunner>> {
        self.with_arena(document, |parser| {
            parser.pump();
            parser.tokenizer.end();
            if let Some(runner) = parser.script_runner.as_mut() {
                let more_coming = !parser.deferred_scripts.is_empty();
                let mut document = parser.tokenizer.sink.sink.document_mut();
                runner.document_finished(&mut document, more_coming);
            }
        });
        self.script_runner.take()
    }

    /// Run the tokenizer until both queues are empty.
    fn pump(&mut self) {
        loop {
            let queue = if self.script_input.is_empty() {
                &self.network_input
            } else {
                &self.script_input
            };

            match self.tokenizer.feed(queue) {
                TokenizerResult::Done => {
                    if self.script_input.is_empty() && self.network_input.is_empty() {
                        return;
                    }
                }
                TokenizerResult::Script(element) => {
                    // The tokenizer has handed us a script and stopped. With no
                    // runner attached the element stays in the tree, nothing
                    // runs, and parsing resumes — which is exactly what a
                    // browser with scripting disabled does.
                    self.scripts_seen += 1;
                    self.execute(element);
                    // A script whose bytes have not arrived stops the parse
                    // here. Everything before it is in the tree and can be
                    // painted; nothing after it exists yet.
                    if self.blocked.is_some() {
                        return;
                    }
                }
                TokenizerResult::EncodingIndicator(label) => {
                    // The other half of html5ever's discarded FIXME: a `<meta>` the
                    // prescan did not reach. Record the first one and keep parsing.
                    if self.encoding_indicator.is_none() {
                        self.encoding_indicator = Some(label.to_string());
                    }
                }
            }
        }
    }
}

/// Whether a `<script type>` names classic JavaScript.
///
/// The list is the spec's "JavaScript MIME type essence match", which is long
/// because the web spent twenty years disagreeing with itself about what to
/// call the language. Anything not on it — `module`, `importmap`,
/// `application/json`, `text/template` — is not a classic script, and the
/// element's contents are data.
pub(crate) fn is_classic_javascript(kind: &str) -> bool {
    is_javascript_mime(kind)
}

/// Whether a `Content-Type` (or a `type` attribute) is a JavaScript MIME type,
/// parameters and case aside. A module whose response is anything else is a
/// failed fetch, which is the check that stops a stylesheet or an HTML error
/// page being run as code.
#[must_use]
pub fn is_javascript_mime(kind: &str) -> bool {
    const ESSENCES: [&str; 16] = [
        "application/ecmascript",
        "application/javascript",
        "application/x-ecmascript",
        "application/x-javascript",
        "text/ecmascript",
        "text/javascript",
        "text/javascript1.0",
        "text/javascript1.1",
        "text/javascript1.2",
        "text/javascript1.3",
        "text/javascript1.4",
        "text/javascript1.5",
        "text/jscript",
        "text/livescript",
        "text/x-ecmascript",
        "text/x-javascript",
    ];

    // The essence is the type before any parameters — `text/javascript;
    // charset=utf-8` is still JavaScript — and the comparison is ASCII
    // case-insensitive.
    let essence = kind.split(';').next().unwrap_or(kind).trim();
    ESSENCES
        .iter()
        .any(|known| essence.eq_ignore_ascii_case(known))
}

impl Default for HtmlParser {
    fn default() -> Self {
        Self::new()
    }
}
