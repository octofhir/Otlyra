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

pub use driver::HtmlParser;
pub use encoding::{DEFAULT_ENCODING, EncodingDecision, EncodingSource, determine};
pub use prescan::prescan;

use otlyra_dom::Document;

/// A parsed document, and how its bytes were read.
#[derive(Debug)]
pub struct ParsedDocument {
    /// The tree.
    pub document: Document,
    /// The encoding used, and why.
    pub encoding: EncodingDecision,
}

/// One decode-and-parse pass, and what the document said about its own encoding
/// while we were doing it.
struct Pass {
    document: Document,
    indicator: Option<String>,
}

impl Pass {
    /// The encoding the document asked for, after the spec's substitutions.
    fn indicated_encoding(&self) -> Option<&'static encoding_rs::Encoding> {
        let label = self.indicator.as_deref()?;
        encoding_rs::Encoding::for_label(label.as_bytes()).map(prescan::apply_overrides)
    }
}

/// Decode `bytes` under `decision` and run the parser over the result.
fn parse_with(bytes: &[u8], decision: EncodingDecision) -> Pass {
    let (text, _actual, _had_errors) = decision.encoding.decode(bytes);
    let mut parser = HtmlParser::new();
    parser.feed(text.as_ref().into());
    let indicator = parser.encoding_indicator().map(str::to_owned);
    Pass {
        document: parser.finish(),
        indicator,
    }
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
    parser.feed(html.into());
    parser.finish()
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
    let span = tracing::info_span!("parse_html", bytes = bytes.len());
    let _entered = span.enter();

    let mut decision = determine(bytes, transport_charset);
    let mut document = parse_with(bytes, decision);

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
        document = parse_with(bytes, decision);
    }

    let document = document.document;

    tracing::debug!(
        encoding = decision.encoding.name(),
        source = ?decision.source,
        nodes = document.len(),
        "parsed"
    );

    ParsedDocument {
        document,
        encoding: decision,
    }
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
