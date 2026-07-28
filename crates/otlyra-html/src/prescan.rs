//! The `<meta>` prescan: guessing the encoding by reading bytes as bytes.
//!
//! This is the WHATWG algorithm "prescan a byte stream to determine its encoding".
//! It exists because a document may declare its own encoding inside itself, which
//! means the first 1024 bytes have to be examined *before* anything is decoded —
//! decoding is what we are trying to decide. So this file has no `str` in it: it
//! walks bytes, lowercases ASCII by hand, and never assumes any encoding at all.
//!
//! No crate does this. `encoding_rs` decodes once you have named an encoding;
//! `html5ever` starts after the bytes are text.

use encoding_rs::Encoding;

/// How many bytes of the stream the prescan is allowed to look at.
///
/// A `<meta charset>` past this point does not count, which is the spec's rule and
/// also the only way the algorithm can be bounded.
pub const PRESCAN_LIMIT: usize = 1024;

/// ASCII whitespace, as HTML defines it.
fn is_space(byte: u8) -> bool {
    matches!(byte, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

fn is_alpha(byte: u8) -> bool {
    byte.is_ascii_alphabetic()
}

/// Run the prescan over at most the first [`PRESCAN_LIMIT`] bytes.
pub fn prescan(bytes: &[u8]) -> Option<&'static Encoding> {
    let bytes = &bytes[..bytes.len().min(PRESCAN_LIMIT)];
    let mut scanner = Scanner {
        bytes,
        at: 0,
        fold_values: true,
    };
    scanner.run()
}

/// Every address a `<script src>` names, in document order, without repeats.
///
/// The second prescan, and for the same reason as the first: something has to be
/// known about a document before it is parsed. A `<script src>` blocks the parse
/// at the point it appears — everything after it is supposed to see what it did —
/// and a browser that only learns of it *at* that point can either stop and wait
/// on the network or run it late. Running it late is what `defer` means, and a
/// page whose inline scripts call into a bundle breaks under it. So the addresses
/// are read out of the bytes first, asked for at once, and the parse starts with
/// them already in hand.
///
/// This walks bytes rather than text because it runs beside [`prescan`], before
/// the encoding is settled. An address that is not ASCII in an ASCII-compatible
/// encoding is out of its reach, as it is out of the encoding prescan's; the
/// parser still finds those scripts, and they load the old way, late.
///
/// Being ahead of the parser, this sees things the parser will not: a `<script
/// src>` inside a `<template>`, or one the tree builder drops. The cost of that
/// is a request nobody uses. The other direction — a script this misses — costs
/// nothing but the lateness we already have.
pub fn prescan_scripts(bytes: &[u8]) -> Vec<String> {
    let mut scanner = Scanner {
        bytes,
        at: 0,
        fold_values: false,
    };
    scanner.scripts()
}

/// The elements whose contents are text rather than markup.
///
/// A `<script src>` spelled inside one of them is not an element and must not be
/// fetched. `<script>` is on the list too, and handled separately because its
/// `src` is the thing we came for.
const RAW_TEXT: [&[u8]; 7] = [
    b"style",
    b"textarea",
    b"title",
    b"xmp",
    b"iframe",
    b"noembed",
    b"noframes",
];

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
    /// Whether attribute values are lowercased as they are read.
    ///
    /// The encoding scan wants them folded — it compares them against labels.
    /// The script scan must not: an address is case-sensitive after the host.
    fold_values: bool,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn starts_with_ignore_ascii_case(&self, prefix: &[u8]) -> bool {
        self.bytes
            .get(self.at..self.at + prefix.len())
            .is_some_and(|window| window.eq_ignore_ascii_case(prefix))
    }

    /// One byte of an attribute value, folded or not as the scan wants.
    fn fold(&self, byte: u8) -> u8 {
        if self.fold_values {
            byte.to_ascii_lowercase()
        } else {
            byte
        }
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.at += 1;
        }
    }

    /// Advance to just past the next occurrence of `needle`, or to the end.
    fn skip_past(&mut self, needle: &[u8]) {
        let mut at = self.at;
        while at + needle.len() <= self.bytes.len() {
            if &self.bytes[at..at + needle.len()] == needle {
                self.at = at + needle.len();
                return;
            }
            at += 1;
        }
        self.at = self.bytes.len();
    }

    fn run(&mut self) -> Option<&'static Encoding> {
        while self.at < self.bytes.len() {
            if self.starts_with_ignore_ascii_case(b"<!--") {
                self.at += 2;
                self.skip_past(b"-->");
            } else if self.starts_with_ignore_ascii_case(b"<meta")
                && self
                    .bytes
                    .get(self.at + 5)
                    .is_some_and(|&byte| is_space(byte) || byte == b'/')
            {
                self.at += 5;
                if let Some(encoding) = self.meta_tag() {
                    return Some(encoding);
                }
            } else if self.looks_like_a_tag() {
                self.skip_tag();
            } else if self.starts_with_ignore_ascii_case(b"<!")
                || self.starts_with_ignore_ascii_case(b"</")
                || self.starts_with_ignore_ascii_case(b"<?")
            {
                self.skip_past(b">");
            } else {
                self.at += 1;
            }
        }
        None
    }

    /// Whether the cursor is on a start tag named `name`.
    fn tag_named(&self, name: &[u8]) -> bool {
        if self.peek() != Some(b'<') {
            return false;
        }
        let after = self.at + 1 + name.len();
        self.bytes
            .get(self.at + 1..after)
            .is_some_and(|window| window.eq_ignore_ascii_case(name))
            && self
                .bytes
                .get(after)
                .is_none_or(|&byte| is_space(byte) || byte == b'/' || byte == b'>')
    }

    /// The whole-document walk behind [`prescan_scripts`].
    fn scripts(&mut self) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        while self.at < self.bytes.len() {
            if self.starts_with_ignore_ascii_case(b"<!--") {
                self.at += 2;
                self.skip_past(b"-->");
            } else if self.tag_named(b"script") {
                self.at += 1 + b"script".len();
                let src = self.tag_attribute(b"src");
                // Whatever is between here and the close tag is source, not
                // markup, and a `<script src=…>` written inside it is a string.
                self.skip_past(b"</script");
                if let Some(src) = src
                    && !src.is_empty()
                    && !found.contains(&src)
                {
                    found.push(src);
                }
            } else if let Some(name) = RAW_TEXT.iter().find(|name| self.tag_named(name)) {
                self.at += 1 + name.len();
                let mut close = b"</".to_vec();
                close.extend_from_slice(name);
                self.skip_past(&close);
            } else if self.looks_like_a_tag() {
                self.skip_tag();
            } else if self.starts_with_ignore_ascii_case(b"<!")
                || self.starts_with_ignore_ascii_case(b"</")
                || self.starts_with_ignore_ascii_case(b"<?")
            {
                self.skip_past(b">");
            } else {
                self.at += 1;
            }
        }
        found
    }

    /// Read a tag's attributes and return the value of `name`, trimmed.
    ///
    /// The cursor is left past the tag either way: the attributes have to be read
    /// through in any case, because `>` inside a quoted value is not the end.
    fn tag_attribute(&mut self, wanted: &[u8]) -> Option<String> {
        let mut value: Option<String> = None;
        while let Some((name, found)) = self.attribute() {
            if name == wanted && value.is_none() {
                value = Some(String::from_utf8_lossy(&found).trim().to_owned());
            }
        }
        value
    }

    fn looks_like_a_tag(&self) -> bool {
        if self.peek() != Some(b'<') {
            return false;
        }
        match self.bytes.get(self.at + 1) {
            Some(&byte) if is_alpha(byte) => true,
            Some(b'/') => self.bytes.get(self.at + 2).copied().is_some_and(is_alpha),
            _ => false,
        }
    }

    /// Skip a tag we do not care about, including its attributes — attributes have
    /// to be parsed rather than skipped to, because `>` may appear inside a quoted
    /// attribute value.
    fn skip_tag(&mut self) {
        self.at += 1;
        while let Some(byte) = self.peek() {
            if is_space(byte) || byte == b'>' {
                break;
            }
            self.at += 1;
        }
        while self.attribute().is_some() {}
    }

    /// The `<meta>` branch: read attributes, then apply the spec's pragma rules.
    fn meta_tag(&mut self) -> Option<&'static Encoding> {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut got_pragma = false;
        let mut need_pragma: Option<bool> = None;
        let mut charset: Option<&'static Encoding> = None;

        while let Some((name, value)) = self.attribute() {
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());

            match name.as_slice() {
                b"http-equiv" => {
                    if value.eq_ignore_ascii_case(b"content-type") {
                        got_pragma = true;
                    }
                }
                b"content" => {
                    if charset.is_none()
                        && let Some(encoding) = encoding_from_meta_content(&value)
                    {
                        charset = Some(encoding);
                        need_pragma = Some(true);
                    }
                }
                b"charset" => {
                    charset = Encoding::for_label(&value);
                    need_pragma = Some(false);
                }
                _ => {}
            }
        }

        match need_pragma {
            None => None,
            Some(true) if !got_pragma => None,
            _ => charset.map(apply_overrides),
        }
    }

    /// Read one attribute, byte for byte. Returns `None` at `>` or end of input.
    fn attribute(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        while self
            .peek()
            .is_some_and(|byte| is_space(byte) || byte == b'/')
        {
            self.at += 1;
        }
        if self.peek()? == b'>' {
            return None;
        }

        let mut name = Vec::new();
        let mut value = Vec::new();

        loop {
            match self.peek() {
                None => return (!name.is_empty()).then_some((name, value)),
                Some(b'=') if !name.is_empty() => {
                    self.at += 1;
                    break;
                }
                Some(byte) if is_space(byte) => {
                    self.skip_spaces();
                    if self.peek() != Some(b'=') {
                        return Some((name, value));
                    }
                    self.at += 1;
                    break;
                }
                Some(byte @ (b'/' | b'>')) => {
                    if byte == b'/' {
                        self.at += 1;
                    }
                    return Some((name, value));
                }
                Some(byte) => {
                    name.push(byte.to_ascii_lowercase());
                    self.at += 1;
                }
            }
        }

        self.skip_spaces();
        match self.peek() {
            None => {}
            Some(quote @ (b'"' | b'\'')) => {
                self.at += 1;
                while let Some(byte) = self.peek() {
                    self.at += 1;
                    if byte == quote {
                        break;
                    }
                    value.push(self.fold(byte));
                }
            }
            Some(b'>') => {}
            Some(byte) => {
                value.push(self.fold(byte));
                self.at += 1;
                while let Some(byte) = self.peek() {
                    if is_space(byte) || byte == b'>' {
                        break;
                    }
                    value.push(self.fold(byte));
                    self.at += 1;
                }
            }
        }

        Some((name, value))
    }
}

/// The WHATWG "extract a character encoding from a meta element" algorithm, over
/// the value of a `content` attribute.
pub fn encoding_from_meta_content(content: &[u8]) -> Option<&'static Encoding> {
    let lowered: Vec<u8> = content.to_ascii_lowercase();
    let mut at = 0;

    loop {
        let position = lowered
            .get(at..)?
            .windows(7)
            .position(|w| w == b"charset")?;
        at += position + 7;

        let mut cursor = at;
        while lowered.get(cursor).is_some_and(|&b| is_space(b)) {
            cursor += 1;
        }
        if lowered.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while lowered.get(cursor).is_some_and(|&b| is_space(b)) {
            cursor += 1;
        }

        let label: &[u8] = match lowered.get(cursor) {
            None => return None,
            Some(&quote @ (b'"' | b'\'')) => {
                let start = cursor + 1;
                let end = lowered
                    .get(start..)?
                    .iter()
                    .position(|&byte| byte == quote)?;
                &lowered[start..start + end]
            }
            Some(_) => {
                let start = cursor;
                let end = lowered[start..]
                    .iter()
                    .position(|&byte| is_space(byte) || byte == b';')
                    .unwrap_or(lowered.len() - start);
                &lowered[start..start + end]
            }
        };

        return Encoding::for_label(label);
    }
}

/// The two substitutions the spec makes on any encoding a document names for
/// itself.
///
/// A document cannot be written in UTF-16 and say so in ASCII, so the label is a
/// lie either way and UTF-8 is the safe reading. `x-user-defined` is a legacy
/// escape hatch that must behave as windows-1252.
pub fn apply_overrides(encoding: &'static Encoding) -> &'static Encoding {
    if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
        encoding_rs::UTF_8
    } else if encoding == encoding_rs::X_USER_DEFINED {
        encoding_rs::WINDOWS_1252
    } else {
        encoding
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scripts(html: &str) -> Vec<String> {
        prescan_scripts(html.as_bytes())
    }

    #[test]
    fn the_scan_finds_every_src_in_document_order() {
        assert_eq!(
            scripts(
                "<head><script src=one.js></script></head><body>text\
                 <script SRC='/two.js' async></script>\
                 <script src=\"http://x/Three.js\" defer></script>"
            ),
            ["one.js", "/two.js", "http://x/Three.js"]
        );
    }

    #[test]
    fn a_src_keeps_its_case() {
        assert_eq!(scripts("<script src=/A/b/C.JS></script>"), ["/A/b/C.JS"]);
    }

    #[test]
    fn an_inline_script_is_not_one_and_its_contents_are_not_markup() {
        assert!(scripts("<script>var s = '<script src=fake.js></script>';").is_empty());
    }

    #[test]
    fn the_same_address_twice_is_asked_for_once() {
        assert_eq!(
            scripts("<script src=a.js></script><script src=a.js></script>"),
            ["a.js"]
        );
    }

    #[test]
    fn a_script_in_a_comment_or_in_text_content_is_not_one() {
        assert!(
            scripts(
                "<!-- <script src=commented.js></script> -->\
                 <textarea><script src=typed.js></script></textarea>\
                 <title><script src=titled.js></script></title>"
            )
            .is_empty()
        );
    }

    #[test]
    fn an_empty_or_missing_src_is_not_an_address() {
        assert!(
            scripts("<script src></script><script src=''></script><script></script>").is_empty()
        );
    }

    #[test]
    fn malformed_markup_does_not_hang_or_panic() {
        for input in [
            "<script",
            "<script src",
            "<script src=",
            "<script src=a.js",
            "<script src=a.js>",
            "<textarea><script src=a.js>",
            "<!-- <script",
            "<script src=a.js></script".repeat(200).as_str(),
        ] {
            let _ = scripts(input);
        }
    }
}
