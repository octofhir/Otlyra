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

    /// Run an external script whose bytes have arrived.
    fn run_external(&mut self, source: &str, element: NodeId, document: &mut Document) {
        let _ = (source, element, document);
    }

    /// When the page's next timer comes due, if it has one.
    ///
    /// This and the three below are the event loop's, not the parser's. They
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

    /// Whether the page has asked for an animation frame and not had one.
    fn frames_pending(&self) -> bool {
        false
    }

    /// Run the animation-frame callbacks the page asked for.
    fn run_frame(&mut self, document: &mut Document, timestamp: f64) -> usize {
        let _ = (document, timestamp);
        0
    }

    /// Run the page's deferred work to a standstill, or until `budget` is spent.
    ///
    /// For a caller with no event loop to be woken from: a screenshot, a dump,
    /// a test.
    fn settle(&mut self, document: &mut Document, budget: std::time::Duration) -> usize {
        let _ = (document, budget);
        0
    }

    /// Throw away everything script has done and start again.
    ///
    /// Called when the document is about to be parsed a second time because
    /// its declared encoding turned out to disagree with the guess. The first
    /// pass's document is discarded, so its scripts' effects must be too — a
    /// browser that reloads a page reloads its script world with it.
    fn reset(&mut self) {}
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
        let sink = DomSink::with_document(document);
        let context_element = create_element(&sink, context, attrs);

        let tree_builder =
            TreeBuilder::new_for_fragment(sink, context_element, None, TreeBuilderOpts::default());
        let tokenizer_opts = TokenizerOpts {
            initial_state: Some(tree_builder.tokenizer_state_for_context_elem(false)),
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
            // A fragment parser never runs script: `innerHTML` parses its input
            // with scripting off, and a `<script>` in it becomes an element and
            // nothing more.
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

        let restore = Restore { parser: self, document };
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

    /// Run the script the tokenizer stopped at, if it is one we run.
    fn execute(&mut self, element: NodeId) {
        if self.script_runner.is_none() {
            return;
        }

        // The borrow of the tree ends before the runner is called. A script can
        // reach back into the document — that is the point of having one — and
        // a `Ref` still outstanding would make the first such reach a panic.
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
                // `type` names a language, and the only ones we execute are the
                // ones the spec calls classic JavaScript. A `type="module"`, an
                // import map, or a `<script type="text/template">` full of markup
                // are all data as far as this is concerned.
                match data.attr("type").map(str::trim) {
                    None => {}
                    Some("") => {}
                    Some(kind) if is_classic_javascript(kind) => {}
                    Some(kind) => {
                        tracing::debug!(target: "page.script", kind, "script of a type we do not run");
                        return;
                    }
                }

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
fn is_classic_javascript(kind: &str) -> bool {
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
