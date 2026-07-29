//! # otlyra-html — bytes to a document tree
//!
//! ## Purpose
//!
//! The two things html5ever does not do: decide what encoding a byte stream is in,
//! and drive the tokenizer on a browser's terms. Tree construction itself is
//! html5ever's; the DOM it builds is [`otlyra_dom`]'s.
//!
//! ## Contents
//!
//! - [`prescan`] — the byte-level `<meta>` scan of the first 1024 bytes.
//! - [`encoding`] — the full determination algorithm the prescan is one step of.
//! - [`driver`] — [`HtmlParser`], our loop over `Tokenizer` and `BufferQueue`.
//! - [`parse`] — the two of them, end to end.
//!
//! ## Invariants
//!
//! 1. **Encoding is decided before anything is decoded**, from bytes alone. Any
//!    other order assumes the answer.
//! 2. **We never use `driver::Parser`.** It cannot express script-blocking or
//!    `document.write`, and its own source says so.
//! 3. **This crate paints nothing and knows nothing about style or layout.** It
//!    produces a tree; what anyone does with it is not its business.

pub mod driver;
pub mod encoding;
pub mod prescan;

pub use driver::{ExternalSources, HtmlParser, ScriptRunner};
pub use encoding::{DEFAULT_ENCODING, EncodingDecision, EncodingSource, determine};
pub use prescan::{prescan, prescan_scripts};

use otlyra_dom::{Document, NodeId};

/// A parsed document, and how its bytes were read.
#[derive(Debug)]
pub struct ParsedDocument {
    /// The tree.
    pub document: Document,
    /// The encoding used, and why.
    pub encoding: EncodingDecision,
    /// The `<script src>` elements, in document order.
    pub external_scripts: Vec<NodeId>,
    /// Those of them the parse could not run where they stood, for want of
    /// their source, in document order.
    ///
    /// Fetching these is the caller's, and so is running them once the bytes
    /// arrive — which is late, and is what the script prescan exists to keep
    /// this list short.
    pub deferred_scripts: Vec<NodeId>,
}

/// One decode-and-parse pass, and what the document said about its own encoding
/// while we were doing it.
struct Pass {
    document: Document,
    indicator: Option<String>,
    external_scripts: Vec<NodeId>,
    deferred_scripts: Vec<NodeId>,
}

impl Pass {
    /// The encoding the document asked for, after the spec's substitutions.
    fn indicated_encoding(&self) -> Option<&'static encoding_rs::Encoding> {
        let label = self.indicator.as_deref()?;
        encoding_rs::Encoding::for_label(label.as_bytes()).map(prescan::apply_overrides)
    }
}

/// Decode `bytes` under `decision` and run the parser over the result.
fn parse_with(
    bytes: &[u8],
    decision: EncodingDecision,
    runner: Option<Box<dyn ScriptRunner>>,
    sources: &ExternalSources,
) -> (Pass, Option<Box<dyn ScriptRunner>>) {
    let (text, _actual, _had_errors) = decision.encoding.decode(bytes);
    let mut parser = HtmlParser::new().with_external_sources(sources.clone());
    if let Some(runner) = runner {
        parser = parser.with_script_runner(runner);
    }
    // The arena is this function's, and the parser works on it. That is the
    // arrangement a browser has, and it is what lets a caller with an event
    // loop paint what has been parsed while the parse is stopped.
    let mut document = Document::new();
    parser.feed(&mut document, text.as_ref().into());
    // Nothing here has a network, so a parse stopped on a script it does not
    // hold is a parse that stays stopped. Let it past — the script becomes a
    // deferred one, which is what the caller will fetch and run afterwards.
    // A caller that *can* fetch drives this loop itself and supplies the bytes.
    while parser.blocked_on().is_some() {
        parser.skip_script(&mut document);
    }
    let indicator = parser.encoding_indicator().map(str::to_owned);
    let external_scripts = parser.external_scripts().to_vec();
    let deferred_scripts = parser.deferred_scripts().to_vec();
    // The runner comes back out of `finish`, not before it: the load events are
    // due at the last byte, and taking it first would mean nobody was there to
    // run them.
    let runner = parser.finish(&mut document);
    (
        Pass {
            document,
            indicator,
            external_scripts,
            deferred_scripts,
        },
        runner,
    )
}

/// Parse `html` as the contents of a `context` element, the way `innerHTML` does.
///
/// The result is a document whose root holds the fragment's nodes. What comes back
/// depends on the context — the same markup inside a `<table>` and inside a `<div>`
/// parse differently — which is why the context is required rather than assumed.
pub fn parse_fragment(html: &str, context: &str) -> Document {
    use html5ever::{LocalName, Namespace, QualName, ns};

    // A context name may carry a namespace, spelled the way the conformance suite
    // spells it: `svg path`, `math ms`.
    let (namespace, local) = match context.split_once(' ') {
        Some(("svg", local)) => (ns!(svg), local),
        Some(("math", local)) => (Namespace::from("http://www.w3.org/1998/Math/MathML"), local),
        Some((_, local)) => (ns!(html), local),
        None => (ns!(html), context),
    };
    let name = QualName::new(None, namespace, LocalName::from(local));

    let mut parser = HtmlParser::for_fragment(Document::new(), name, Vec::new());
    // The context element was built into the parser's own arena, so that arena
    // is the one the fragment is parsed into — a fresh one would have the
    // context's id pointing at nothing.
    let mut document = parser.take_document();
    parser.feed(&mut document, html.into());
    parser.finish(&mut document);
    document
}

/// Parse a complete byte stream into a document.
///
/// `transport_charset` is the `charset` parameter of the response's `Content-Type`,
/// when there was one; it outranks anything the document says about itself.
///
/// The whole stream is decoded at once, which is right for a file and for a response
/// we already hold. Incremental decode belongs with incremental delivery, and that
/// arrives with navigation.
pub fn parse(bytes: &[u8], transport_charset: Option<&str>) -> ParsedDocument {
    parse_with_scripts(bytes, transport_charset, None, ExternalSources::new()).0
}

/// A parse that has begun: the tree so far, and the parser that is building it.
///
/// What a caller with a network gets instead of a finished document. The parse
/// runs until it either ends or stops at a parser-blocking `<script src>` —
/// [`HtmlParser::blocked_on`] says which — and everything parsed up to that
/// point is in `document` and can be styled, laid out and painted. That is what
/// makes a browser show half a page while the rest of it is still coming.
pub struct StartedParse {
    /// The tree as it stands. It is the caller's; the parser borrows it back
    /// for each pump.
    pub document: Document,
    /// The parser, to be driven until it is done.
    pub parser: HtmlParser,
    /// The encoding used, and why.
    pub encoding: EncodingDecision,
}

/// Begin parsing `bytes`, and stop at the first script that blocks the parse.
///
/// The caller drives the rest: fetch what [`HtmlParser::blocked_on`] names,
/// [`HtmlParser::supply_script`] it — or [`HtmlParser::skip_script`] when the
/// fetch failed — and [`HtmlParser::finish`] when nothing is left.
///
/// Unlike [`parse_with_scripts`] this does not decode twice for a `<meta>` the
/// prescan did not reach. A second pass throws away the first one's tree and
/// everything its scripts did, and a parse that has already stopped at a script
/// has a tree somebody may already be looking at. The prescan and the transport
/// between them name the encoding of essentially every page that has scripts in
/// it at all.
pub fn start_parse(
    bytes: &[u8],
    transport_charset: Option<&str>,
    runner: Option<Box<dyn ScriptRunner>>,
    sources: ExternalSources,
) -> StartedParse {
    let span = tracing::info_span!("start_parse", bytes = bytes.len());
    let _entered = span.enter();

    let decision = determine(bytes, transport_charset);
    let (text, _actual, _had_errors) = decision.encoding.decode(bytes);
    let mut parser = HtmlParser::new().with_external_sources(sources);
    if let Some(runner) = runner {
        parser = parser.with_script_runner(runner);
    }
    let mut document = Document::new();
    parser.feed(&mut document, text.as_ref().into());
    StartedParse {
        document,
        parser,
        encoding: decision,
    }
}

/// Parse a byte stream, running the scripts in it.
///
/// The runner comes back out, because the isolate outlives the parse: a page
/// keeps running after its last byte, and the timers and event handlers its
/// scripts registered are in there.
///
/// A document parsed twice — see [`parse`] — resets the runner between passes.
/// The first pass's tree is thrown away for having been decoded wrongly, and
/// what its scripts did to the world has to go with it.
pub fn parse_with_scripts(
    bytes: &[u8],
    transport_charset: Option<&str>,
    runner: Option<Box<dyn ScriptRunner>>,
    sources: ExternalSources,
) -> (ParsedDocument, Option<Box<dyn ScriptRunner>>) {
    let span = tracing::info_span!("parse_html", bytes = bytes.len());
    let _entered = span.enter();

    let mut decision = determine(bytes, transport_charset);
    let (mut document, mut runner) = parse_with(bytes, decision, runner, &sources);

    // A `<meta>` the prescan never saw — past 1024 bytes, or only spelled out once
    // character references were resolved. If we were guessing, the document knows
    // better than we do, and the only way to act on that is to decode it again. Once:
    // the second pass starts from a decided encoding, so it cannot ask for a third.
    if decision.source == EncodingSource::Default
        && let Some(encoding) = document.indicated_encoding()
        && encoding != decision.encoding
    {
        decision = EncodingDecision {
            encoding,
            source: EncodingSource::TokenizerIndicator,
        };
        if let Some(runner) = runner.as_mut() {
            runner.reset();
        }
        (document, runner) = parse_with(bytes, decision, runner, &sources);
    }

    let external_scripts = document.external_scripts;
    let deferred_scripts = document.deferred_scripts;
    let document = document.document;

    tracing::debug!(
        encoding = decision.encoding.name(),
        source = ?decision.source,
        nodes = document.len(),
        "parsed"
    );

    (
        ParsedDocument {
            document,
            encoding: decision,
            external_scripts,
            deferred_scripts,
        },
        runner,
    )
}

#[cfg(test)]
mod tests {
    use otlyra_dom::dump;

    use super::*;

    fn tree(html: &str) -> String {
        dump::serialize(&parse(html.as_bytes(), Some("utf-8")).document)
    }

    #[test]
    fn a_minimal_document_gets_the_implied_elements() {
        assert_eq!(
            tree("<title>hi</title>"),
            "\
| <html>
|   <head>
|     <title>
|       \"hi\"
|   <body>
"
        );
    }

    #[test]
    fn a_doctype_is_kept_and_text_lands_in_the_body() {
        assert_eq!(
            tree("<!DOCTYPE html><p>text"),
            "\
| <!DOCTYPE html>
| <html>
|   <head>
|   <body>
|     <p>
|       \"text\"
"
        );
    }

    #[test]
    fn unclosed_tags_are_closed_for_us() {
        assert_eq!(
            tree("<body><p>one<p>two"),
            "\
| <html>
|   <head>
|   <body>
|     <p>
|       \"one\"
|     <p>
|       \"two\"
"
        );
    }

    #[test]
    fn misnested_formatting_goes_through_the_adoption_agency() {
        assert_eq!(
            tree("<body><b>1<i>2</b>3</i>"),
            "\
| <html>
|   <head>
|   <body>
|     <b>
|       \"1\"
|       <i>
|         \"2\"
|     <i>
|       \"3\"
"
        );
    }

    #[test]
    fn text_in_a_table_is_foster_parented_out_of_it() {
        assert_eq!(
            tree("<table>stray<tr><td>cell"),
            "\
| <html>
|   <head>
|   <body>
|     \"stray\"
|     <table>
|       <tbody>
|         <tr>
|           <td>
|             \"cell\"
"
        );
    }

    #[test]
    fn template_contents_go_into_their_own_fragment() {
        assert_eq!(
            tree("<template><p>inside</p></template>"),
            "\
| <html>
|   <head>
|     <template>
|       content
|         <p>
|           \"inside\"
|   <body>
"
        );
    }

    #[test]
    fn foreign_content_keeps_its_namespace() {
        assert_eq!(
            tree("<body><svg><circle/></svg>"),
            "\
| <html>
|   <head>
|   <body>
|     <svg svg>
|       <svg circle>
"
        );
    }

    #[test]
    fn attributes_are_kept_and_printed_sorted() {
        assert_eq!(
            tree("<body><div id=x class=\"a b\" data-z>"),
            "\
| <html>
|   <head>
|   <body>
|     <div>
|       class=\"a b\"
|       data-z=\"\"
|       id=\"x\"
"
        );
    }

    #[test]
    fn a_legacy_encoding_declared_in_the_document_is_honoured() {
        // "Привет" in windows-1251, declared by the document itself.
        let mut bytes = b"<meta charset=windows-1251><p>".to_vec();
        bytes.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
        let parsed = parse(&bytes, None);

        assert_eq!(parsed.encoding.source, EncodingSource::MetaPrescan);
        assert!(
            dump::serialize(&parsed.document).contains("\"Привет\""),
            "{}",
            dump::serialize(&parsed.document)
        );
    }

    /// The prescan stops at 1024 bytes; the tokenizer does not. A declaration it
    /// finds later has to send us back to the bytes, because the text we produced
    /// from them is wrong.
    #[test]
    fn a_meta_past_the_prescan_limit_makes_us_decode_again() {
        let mut bytes = format!("<!--{}-->", " ".repeat(1100)).into_bytes();
        bytes.extend_from_slice(b"<meta charset=windows-1251><p>");
        bytes.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
        let parsed = parse(&bytes, None);

        assert_eq!(parsed.encoding.source, EncodingSource::TokenizerIndicator);
        assert_eq!(parsed.encoding.encoding, encoding_rs::WINDOWS_1251);
        assert!(dump::serialize(&parsed.document).contains("\"Привет\""));
    }

    /// The transport outranks the document, so a late `<meta>` must not undo it.
    #[test]
    fn a_late_meta_does_not_override_the_transport() {
        let mut bytes = format!("<!--{}-->", " ".repeat(1100)).into_bytes();
        bytes.extend_from_slice(b"<meta charset=windows-1251><p>hi");
        let parsed = parse(&bytes, Some("utf-8"));

        assert_eq!(parsed.encoding.source, EncodingSource::TransportCharset);
        assert_eq!(parsed.encoding.encoding, encoding_rs::UTF_8);
    }

    /// The parser-blocking script: the parse stops at it, everything before it
    /// is in the tree and can be painted, and nothing after it exists until the
    /// bytes arrive. This is the whole reason a browser shows half a page.
    #[test]
    fn the_parse_stops_at_a_script_whose_bytes_have_not_arrived() {
        use otlyra_dom::Document;

        let mut parser = HtmlParser::new().with_script_runner(Box::new(CountingRunner::default()));
        let mut document = Document::new();
        parser.feed(
            &mut document,
            "<body><p>before</p><script src=app.js></script><p>after</p>".into(),
        );

        let blocked = parser.blocked_on().expect("the parse stopped at the script");
        let tree = dump::serialize(&document);
        assert!(tree.contains("\"before\""), "what came first is in the tree:\n{tree}");
        assert!(
            !tree.contains("\"after\""),
            "nothing past the script was tokenized:\n{tree}"
        );

        // The bytes arrive. The script runs against the half-built tree, and
        // the rest of the document is parsed after it.
        parser.supply_script(&mut document, "/* the bundle */");
        assert_eq!(parser.blocked_on(), None, "the parse carried on");
        parser.finish(&mut document);
        let tree = dump::serialize(&document);
        assert!(tree.contains("\"after\""), "the rest was parsed:\n{tree}");
        let _ = blocked;
    }

    /// A script whose fetch failed does not stop the page for ever.
    #[test]
    fn a_script_that_never_arrives_can_be_stepped_over() {
        use otlyra_dom::Document;

        let mut parser = HtmlParser::new().with_script_runner(Box::new(CountingRunner::default()));
        let mut document = Document::new();
        parser.feed(
            &mut document,
            "<body><script src=gone.js></script><p>after</p>".into(),
        );
        assert!(parser.blocked_on().is_some());
        parser.skip_script(&mut document);
        parser.finish(&mut document);
        assert!(dump::serialize(&document).contains("\"after\""));
    }

    /// A runner that does nothing, so a parse can be driven without an engine.
    #[derive(Default)]
    struct CountingRunner {
        ran: usize,
    }

    impl ScriptRunner for CountingRunner {
        fn run(&mut self, _source: &str, _element: NodeId, _document: &mut otlyra_dom::Document) {
            self.ran += 1;
        }

        fn run_external(
            &mut self,
            _source: &str,
            _element: NodeId,
            _document: &mut otlyra_dom::Document,
        ) {
            self.ran += 1;
        }
    }

    #[test]
    fn a_script_element_does_not_stop_the_parse() {
        assert_eq!(
            tree("<body><script>var x = 1 < 2;</script><p>after"),
            "\
| <html>
|   <head>
|   <body>
|     <script>
|       \"var x = 1 < 2;\"
|     <p>
|       \"after\"
"
        );
    }

    /// The spec step that copies an option's contents into `<selectedcontent>`.
    /// html5ever only calls it on an explicit `</option>`, which is why the four
    /// html5lib cases without one stay in the expectations ledger.
    #[test]
    fn a_closed_option_is_cloned_into_selectedcontent() {
        let tree = tree(
            "<select><button><selectedcontent></selectedcontent></button><option>Chosen</option></select>",
        );
        assert!(
            tree.contains("selectedcontent") && tree.matches("\"Chosen\"").count() == 2,
            "the option's text should appear both in it and in the selectedcontent:\n{tree}"
        );
    }

    #[test]
    fn malformed_input_produces_a_tree_rather_than_a_panic() {
        for input in [
            "",
            "<",
            "</",
            "</>",
            "<!",
            "<!-- unterminated",
            "<p<p<p<p",
            "<a href=",
            "<div ".repeat(200).as_str(),
            "&notanentity;&#xZZ;&#99999999999;",
            "<table><table><table>",
            "<svg><math><svg><p>",
        ] {
            let parsed = parse(input.as_bytes(), Some("utf-8"));
            // Every document gets at least <html><head><body>, and serializing is
            // itself a full recursive walk of whatever came out.
            assert!(
                dump::serialize(&parsed.document).contains("<html>"),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn nesting_deeper_than_the_limit_is_truncated_rather_than_overflowing() {
        let html = "<div>".repeat(5_000);
        let parsed = parse(html.as_bytes(), Some("utf-8"));
        assert!(parsed.document.refused_insertions() > 0);
        // Serializing is itself a recursive walk, so this asserts the cap works.
        assert!(!dump::serialize(&parsed.document).is_empty());
    }
}
