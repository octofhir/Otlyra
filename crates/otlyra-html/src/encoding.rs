//! Deciding which encoding a document is written in.
//!
//! The order is the spec's, and the order is the whole algorithm: a BOM outranks
//! everything, the transport's `charset` outranks the document's own claim, and the
//! document's claim only counts if it appears in the first 1024 bytes.

use encoding_rs::Encoding;

use crate::prescan::{apply_overrides, prescan};

/// Why we settled on an encoding. Worth keeping: "we guessed" and "the server said
/// so" are very different when a page renders as mojibake.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EncodingSource {
    /// A byte-order mark. Certain.
    Bom,
    /// The transport's `Content-Type` charset parameter.
    TransportCharset,
    /// A `<meta>` in the first 1024 bytes.
    MetaPrescan,
    /// A `<meta>` the tokenizer reached that the prescan did not, which made us
    /// decode the document again.
    TokenizerIndicator,
    /// Nothing said anything, so the legacy default applies.
    Default,
}

/// The outcome of encoding determination.
#[derive(Copy, Clone, Debug)]
pub struct EncodingDecision {
    /// The encoding to decode with.
    pub encoding: &'static Encoding,
    /// Where it came from.
    pub source: EncodingSource,
}

/// The encoding assumed when nothing at all says otherwise.
///
/// Not UTF-8: an unlabelled document that is not UTF-8 is overwhelmingly likely to
/// be windows-1252, and that is what every browser does. Guessing UTF-8 would render
/// such a page as replacement characters.
pub const DEFAULT_ENCODING: &Encoding = encoding_rs::WINDOWS_1252;

/// Decide the encoding for `bytes`, given whatever the transport said.
pub fn determine(bytes: &[u8], transport_charset: Option<&str>) -> EncodingDecision {
    if let Some((encoding, _length)) = Encoding::for_bom(bytes) {
        return EncodingDecision {
            encoding,
            source: EncodingSource::Bom,
        };
    }

    if let Some(label) = transport_charset
        && let Some(encoding) = Encoding::for_label(label.as_bytes())
    {
        return EncodingDecision {
            encoding: apply_overrides(encoding),
            source: EncodingSource::TransportCharset,
        };
    }

    if let Some(encoding) = prescan(bytes) {
        return EncodingDecision {
            encoding,
            source: EncodingSource::MetaPrescan,
        };
    }

    EncodingDecision {
        encoding: DEFAULT_ENCODING,
        source: EncodingSource::Default,
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::{SHIFT_JIS, UTF_8, WINDOWS_1251, WINDOWS_1252};

    use super::*;

    fn decide(bytes: &[u8], charset: Option<&str>) -> EncodingDecision {
        determine(bytes, charset)
    }

    #[test]
    fn a_bom_outranks_everything() {
        for (bom, expected) in [
            (&[0xEF, 0xBB, 0xBF][..], UTF_8),
            (&[0xFF, 0xFE][..], encoding_rs::UTF_16LE),
            (&[0xFE, 0xFF][..], encoding_rs::UTF_16BE),
        ] {
            let mut bytes = bom.to_vec();
            bytes.extend_from_slice(b"<meta charset=shift_jis>");
            let decision = decide(&bytes, Some("windows-1251"));
            assert_eq!(decision.encoding, expected);
            assert_eq!(decision.source, EncodingSource::Bom);
        }
    }

    #[test]
    fn the_transport_outranks_the_document() {
        let decision = decide(b"<meta charset=shift_jis>", Some("windows-1251"));
        assert_eq!(decision.encoding, WINDOWS_1251);
        assert_eq!(decision.source, EncodingSource::TransportCharset);
    }

    #[test]
    fn an_unrecognized_transport_label_falls_through_to_the_document() {
        let decision = decide(b"<meta charset=shift_jis>", Some("nonsense"));
        assert_eq!(decision.encoding, SHIFT_JIS);
        assert_eq!(decision.source, EncodingSource::MetaPrescan);
    }

    #[test]
    fn utf16_and_x_user_defined_are_substituted() {
        assert_eq!(decide(b"", Some("utf-16le")).encoding, UTF_8);
        assert_eq!(decide(b"", Some("utf-16be")).encoding, UTF_8);
        assert_eq!(decide(b"", Some("x-user-defined")).encoding, WINDOWS_1252);
        assert_eq!(
            decide(b"<meta charset=utf-16le>", None).encoding,
            UTF_8,
            "the same substitution applies to the document's own claim"
        );
    }

    #[test]
    fn nothing_at_all_means_windows_1252() {
        let decision = decide(b"<html><body>hi", None);
        assert_eq!(decision.encoding, WINDOWS_1252);
        assert_eq!(decision.source, EncodingSource::Default);
    }

    #[test]
    fn the_meta_charset_form_is_recognized() {
        assert_eq!(decide(b"<meta charset=utf-8>", None).encoding, UTF_8);
        assert_eq!(decide(b"<meta charset='utf-8'>", None).encoding, UTF_8);
        assert_eq!(decide(b"<meta CHARSET=\"UTF-8\">", None).encoding, UTF_8);
        assert_eq!(decide(b"<meta/charset=utf-8>", None).encoding, UTF_8);
    }

    #[test]
    fn the_http_equiv_form_needs_its_pragma() {
        let with_pragma =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=shift_jis\">";
        assert_eq!(decide(with_pragma, None).encoding, SHIFT_JIS);

        let without_pragma = b"<meta content=\"text/html; charset=shift_jis\">";
        assert_eq!(
            decide(without_pragma, None).source,
            EncodingSource::Default,
            "a content= charset without the http-equiv pragma does not count"
        );
    }

    #[test]
    fn a_meta_inside_a_comment_is_ignored() {
        let bytes = b"<!-- <meta charset=shift_jis> --><meta charset=utf-8>";
        assert_eq!(decide(bytes, None).encoding, UTF_8);
    }

    #[test]
    fn a_meta_past_the_first_1024_bytes_is_ignored() {
        let mut bytes = vec![b' '; 1024];
        bytes.extend_from_slice(b"<meta charset=shift_jis>");
        assert_eq!(decide(&bytes, None).source, EncodingSource::Default);

        let mut just_inside = vec![b' '; 1000];
        just_inside.extend_from_slice(b"<meta charset=shift_jis>");
        assert_eq!(decide(&just_inside, None).encoding, SHIFT_JIS);
    }

    #[test]
    fn a_greater_than_inside_a_quoted_attribute_does_not_end_the_tag() {
        let bytes = b"<div title=\"a > b\"><meta charset=utf-8>";
        assert_eq!(decide(bytes, None).encoding, UTF_8);
    }

    #[test]
    fn the_first_usable_meta_wins() {
        let bytes = b"<meta charset=nonsense><meta charset=utf-8>";
        assert_eq!(
            decide(bytes, None).encoding,
            UTF_8,
            "an unusable label does not stop the scan"
        );
    }

    #[test]
    fn attributes_before_the_charset_are_stepped_over() {
        let bytes = b"<meta name=\"viewport\" content=\"width=device-width\" charset=utf-8>";
        assert_eq!(decide(bytes, None).encoding, UTF_8);
    }

    #[test]
    fn a_duplicate_attribute_name_is_ignored() {
        let bytes = b"<meta charset=utf-8 charset=shift_jis>";
        assert_eq!(decide(bytes, None).encoding, UTF_8);
    }

    #[test]
    fn a_doctype_or_comment_before_the_meta_does_not_confuse_the_scan() {
        let bytes = b"<!DOCTYPE html>\n<!-- hello -->\n<html>\n<head>\n<meta charset=utf-8>";
        assert_eq!(decide(bytes, None).encoding, UTF_8);
    }

    #[test]
    fn a_bare_less_than_is_not_a_tag() {
        assert_eq!(decide(b"a < b <meta charset=utf-8>", None).encoding, UTF_8);
        assert_eq!(decide(b"1<2 <meta charset=utf-8>", None).encoding, UTF_8);
    }

    /// `<d ...>` is a tag as far as the prescan is concerned, so everything up to
    /// its `>` is that tag's attributes — including a `<meta>` written inside it.
    /// The tokenizer proper will disagree later; the prescan is deliberately cruder
    /// than the tokenizer, because it runs before there is any text to tokenize.
    #[test]
    fn a_meta_swallowed_by_an_unterminated_tag_does_not_count() {
        let bytes = b"c<d <meta charset=utf-8>";
        assert_eq!(decide(bytes, None).source, EncodingSource::Default);
    }

    #[test]
    fn an_unterminated_tag_does_not_hang_or_panic() {
        assert_eq!(
            decide(b"<meta charset=", None).source,
            EncodingSource::Default
        );
        assert_eq!(decide(b"<!--", None).source, EncodingSource::Default);
        assert_eq!(decide(b"<meta charset=\"utf-8", None).encoding, UTF_8);
    }
}
