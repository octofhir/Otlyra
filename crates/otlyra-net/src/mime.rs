//! What a response actually is, as opposed to what it says it is.
//!
//! Servers are wrong about `Content-Type` often enough that every browser sniffs,
//! and sniffing is a security decision as much as a rendering one: treating a
//! picture as a document is how a file upload becomes a script. So this follows the
//! shape of the WHATWG algorithm rather than inventing one — the supplied type is
//! respected where the specification says it must be, `X-Content-Type-Options:
//! nosniff` ends the discussion, and the pattern tables decide the rest.
//!
//! What is not here: the full 1,500-line table of every format, the Apache
//! `text/plain` bug flag, and font and archive sniffing. Those matter to a browser
//! that renders more than we do, and the gap is stated rather than guessed at.

/// What a response turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sniffed {
    /// Markup, to be parsed as a document.
    Html,
    /// XML, which we also parse as a document.
    Xml,
    /// Text with no markup in it: shown as text, not parsed.
    PlainText,
    /// A picture, with the type that was recognized.
    Image(&'static str),
    /// Anything else, with the essence of whatever the server said.
    Other(String),
}

impl Sniffed {
    /// Whether this is something to parse as a document.
    pub fn is_document(&self) -> bool {
        matches!(self, Self::Html | Self::Xml)
    }

    /// The MIME essence, as a browser would report it.
    pub fn essence(&self) -> &str {
        match self {
            Self::Html => "text/html",
            Self::Xml => "application/xml",
            Self::PlainText => "text/plain",
            Self::Image(kind) => kind,
            Self::Other(other) => other,
        }
    }
}

/// The essence of a `Content-Type` header: the type and subtype, lowercased, with
/// the parameters dropped.
pub fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Decide what a response is, from what the server said and what it sent.
///
/// `nosniff` is the server saying it means what it said; the supplied type is then
/// used as-is, which is exactly the point of the header.
pub fn sniff(content_type: Option<&str>, nosniff: bool, body: &[u8]) -> Sniffed {
    let supplied = content_type.map(essence);

    if nosniff {
        return supplied.map_or(
            Sniffed::Other("application/octet-stream".to_owned()),
            from_essence,
        );
    }

    // The types that mean "I do not know", which the specification says to treat as
    // no type at all.
    let unknown = matches!(
        supplied.as_deref(),
        None | Some("")
            | Some("unknown/unknown")
            | Some("application/unknown")
            | Some("*/*")
            | Some("application/octet-stream")
    );

    if let Some(supplied) = supplied.as_deref()
        && !unknown
    {
        // A picture that says it is a picture is sniffed for *which* picture, since
        // a decoder needs the truth and a server's guess is often the extension.
        if supplied.starts_with("image/")
            && let Some(image) = image_pattern(body)
        {
            return Sniffed::Image(image);
        }
        return from_essence(supplied.to_owned());
    }

    // Nothing useful was said: the bytes decide.
    if let Some(image) = image_pattern(body) {
        return Sniffed::Image(image);
    }
    if looks_like_html(body) {
        return Sniffed::Html;
    }
    if looks_binary(body) {
        return Sniffed::Other("application/octet-stream".to_owned());
    }
    Sniffed::PlainText
}

/// The type a MIME essence names.
fn from_essence(essence: String) -> Sniffed {
    match essence.as_str() {
        "text/html" => Sniffed::Html,
        "text/xml" | "application/xml" | "image/svg+xml" => Sniffed::Xml,
        "text/plain" => Sniffed::PlainText,
        other if other.ends_with("+xml") => Sniffed::Xml,
        _ => Sniffed::Other(essence),
    }
}

/// The image types we recognize, by their leading bytes.
fn image_pattern(body: &[u8]) -> Option<&'static str> {
    const PATTERNS: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"BM", "image/bmp"),
        (b"\x00\x00\x01\x00", "image/x-icon"),
        (b"\x00\x00\x02\x00", "image/x-icon"),
    ];

    for (pattern, kind) in PATTERNS {
        if body.starts_with(pattern) {
            return Some(kind);
        }
    }
    // RIFF containers name their form four bytes in, which is what tells a WebP
    // from a wave file.
    if body.len() >= 12 && body.starts_with(b"RIFF") && &body[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Whether the bytes begin with something only markup begins with.
fn looks_like_html(body: &[u8]) -> bool {
    const TAGS: &[&[u8]] = &[
        b"<!DOCTYPE HTML",
        b"<HTML",
        b"<HEAD",
        b"<SCRIPT",
        b"<IFRAME",
        b"<H1",
        b"<DIV",
        b"<FONT",
        b"<TABLE",
        b"<A",
        b"<STYLE",
        b"<TITLE",
        b"<B",
        b"<BODY",
        b"<BR",
        b"<P",
        b"<!--",
    ];

    let start = body
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(body.len());
    let rest = &body[start..];

    TAGS.iter().any(|tag| {
        rest.len() > tag.len()
            && rest[..tag.len()].eq_ignore_ascii_case(tag)
            // A tag ends in a space or a bracket; without this `<p` would match
            // `<pre-release notes>` and `<a` would match anything at all.
            && (rest[tag.len()] == b' ' || rest[tag.len()] == b'>')
    })
}

/// Whether the bytes contain something no text file contains.
///
/// The specification's binary data byte set: the control characters that no
/// encoding uses for text.
fn looks_binary(body: &[u8]) -> bool {
    body.iter()
        .take(512)
        .any(|byte| matches!(byte, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_type_is_respected() {
        assert_eq!(
            sniff(Some("text/html"), false, b"not markup"),
            Sniffed::Html
        );
        assert_eq!(
            sniff(Some("text/plain; charset=utf-8"), false, b"<html>"),
            Sniffed::PlainText
        );
    }

    /// The types that mean "I do not know" are treated as nothing said at all.
    #[test]
    fn an_unknown_type_is_sniffed() {
        for supplied in [
            None,
            Some("unknown/unknown"),
            Some("application/unknown"),
            Some("*/*"),
            Some("application/octet-stream"),
        ] {
            assert_eq!(
                sniff(supplied, false, b"<!DOCTYPE HTML><p>hi"),
                Sniffed::Html,
                "supplied {supplied:?}"
            );
        }
    }

    /// A picture is recognized by what it is, not by what it is called: a decoder
    /// needs the truth, and a server's guess is usually the file extension.
    #[test]
    fn a_picture_is_recognized_by_its_own_bytes() {
        assert_eq!(
            sniff(Some("image/gif"), false, b"\x89PNG\r\n\x1a\n rest"),
            Sniffed::Image("image/png")
        );
        assert_eq!(
            sniff(None, false, b"RIFF\0\0\0\0WEBPVP8 "),
            Sniffed::Image("image/webp")
        );
        assert_eq!(
            sniff(None, false, b"\xFF\xD8\xFF\xE0 jfif"),
            Sniffed::Image("image/jpeg")
        );
    }

    /// `nosniff` is the server saying it means what it said, and is the one answer
    /// that must not be second-guessed: it is what stops an upload from being run
    /// as a document.
    #[test]
    fn nosniff_ends_the_discussion() {
        assert_eq!(
            sniff(Some("text/plain"), true, b"<!DOCTYPE HTML><p>hi"),
            Sniffed::PlainText
        );
        assert_eq!(
            sniff(None, true, b"<!DOCTYPE HTML>"),
            Sniffed::Other("application/octet-stream".to_owned())
        );
    }

    #[test]
    fn text_and_binary_are_told_apart() {
        assert_eq!(sniff(None, false, b"plain words\n"), Sniffed::PlainText);
        assert_eq!(
            sniff(None, false, b"words\x00with a null"),
            Sniffed::Other("application/octet-stream".to_owned())
        );
    }

    /// The tag table must not match a longer word that starts the same way.
    #[test]
    fn a_tag_pattern_ends_where_a_tag_ends() {
        assert_eq!(
            sniff(None, false, b"<pre-release notes"),
            Sniffed::PlainText
        );
        assert_eq!(sniff(None, false, b"<p>a paragraph"), Sniffed::Html);
        assert_eq!(sniff(None, false, b"   \n<TITLE>spaced"), Sniffed::Html);
    }

    #[test]
    fn an_essence_drops_its_parameters_and_its_case() {
        assert_eq!(essence("Text/HTML; charset=UTF-8"), "text/html");
        assert_eq!(essence("  application/json  "), "application/json");
    }
}
