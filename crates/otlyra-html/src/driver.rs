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

use html5ever::interface::TreeSink;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{Tokenizer, TokenizerOpts};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use html5ever::{TokenizerResult, buffer_queue::BufferQueue};
use otlyra_dom::{Document, DomSink, NodeId};

/// A parser fed decoded text.
///
/// Bytes are decoded before they get here — see [`crate::parse`], which does
/// encoding determination first.
pub struct HtmlParser {
    tokenizer: Tokenizer<TreeBuilder<NodeId, DomSink>>,
    network_input: BufferQueue,
    script_input: BufferQueue,
    scripts_seen: usize,
    encoding_indicator: Option<String>,
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
            encoding_indicator: None,
        }
    }

    /// Feed decoded text that arrived over the network.
    pub fn feed(&mut self, text: StrTendril) {
        self.network_input.push_back(text);
        self.pump();
    }

    /// Splice text in ahead of the network bytes, as `document.write` does.
    ///
    /// Unused until script runs at M12; it is here because it is the reason the
    /// queues are separate, and a driver with one queue cannot grow this later
    /// without being rewritten.
    pub fn write(&mut self, text: StrTendril) {
        self.script_input.push_front(text);
        self.pump();
    }

    /// How many `<script>` elements the tokenizer stopped at.
    pub fn scripts_seen(&self) -> usize {
        self.scripts_seen
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

    /// Finish parsing and take the document.
    pub fn finish(mut self) -> Document {
        self.pump();
        self.tokenizer.end();
        self.tokenizer.sink.sink.finish()
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
                TokenizerResult::Script(_element) => {
                    // The tokenizer has handed us a script to execute. There is no
                    // script engine until M12, so the element stays in the tree,
                    // nothing runs, and parsing resumes — which is exactly what a
                    // browser with scripting disabled does.
                    self.scripts_seen += 1;
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

impl Default for HtmlParser {
    fn default() -> Self {
        Self::new()
    }
}
