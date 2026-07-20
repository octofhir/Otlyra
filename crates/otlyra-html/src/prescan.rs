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
    let mut scanner = Scanner { bytes, at: 0 };
    scanner.run()
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
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
                    value.push(byte.to_ascii_lowercase());
                }
            }
            Some(b'>') => {}
            Some(byte) => {
                value.push(byte.to_ascii_lowercase());
                self.at += 1;
                while let Some(byte) = self.peek() {
                    if is_space(byte) || byte == b'>' {
                        break;
                    }
                    value.push(byte.to_ascii_lowercase());
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
