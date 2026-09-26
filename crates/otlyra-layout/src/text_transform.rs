//! `text-transform` (CSS Text 3 §2.1): the text a box shows, put in the case
//! and width its style asks for.
//!
//! Applied once white space has been processed and before anything is shaped
//! (CSS Text 3 Appendix A), over an inline formatting context's text in order,
//! so that `capitalize` knows a word that began in one box and goes on in the
//! next is one word.
//!
//! The mappings are Unicode's own and not language-sensitive: Turkish dotted
//! and dotless i, Lithuanian dot retention and Greek accent removal in upper
//! case need the content language, which layout is not told.

use std::borrow::Cow;

use otlyra_css::{TextCase, TextTransform};
use unicode_segmentation::UnicodeSegmentation;

/// Where a context's text has got to, as far as `capitalize` is concerned.
#[derive(Debug)]
pub(crate) struct Casing {
    /// Whether the next letter or number starts a word.
    word_start: bool,
}

impl Default for Casing {
    fn default() -> Self {
        Self { word_start: true }
    }
}

impl Casing {
    /// `text` as `transform` shows it, the word it may be part of carried on
    /// from the text before it.
    pub(crate) fn apply<'a>(&mut self, text: &'a str, transform: TextTransform) -> Cow<'a, str> {
        let cased = match transform.case {
            TextCase::None => {
                self.follow(text);
                Cow::Borrowed(text)
            }
            TextCase::Uppercase => {
                self.follow(text);
                Cow::Owned(text.to_uppercase())
            }
            TextCase::Lowercase => {
                self.follow(text);
                Cow::Owned(text.to_lowercase())
            }
            TextCase::Capitalize => Cow::Owned(self.capitalize(text)),
        };
        if transform.full_width {
            Cow::Owned(cased.chars().map(full_width).collect())
        } else {
            cased
        }
    }

    /// Something that is not text came between — a picture, an inline block,
    /// a forced break — and whatever follows it starts a word.
    pub(crate) fn interrupt(&mut self) {
        self.word_start = true;
    }

    /// Walk `text` without changing it, to know whether what follows it
    /// starts a word.
    fn follow(&mut self, text: &str) {
        for cluster in text.graphemes(true) {
            self.step(cluster);
        }
    }

    /// The first letter of every word in title case, the rest as written.
    fn capitalize(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for cluster in text.graphemes(true) {
            let starts = self.word_start;
            let mut chars = cluster.chars();
            match chars.next() {
                Some(base) if starts && is_word(base) => {
                    push_titlecase(&mut out, base);
                    out.extend(chars);
                }
                _ => out.push_str(cluster),
            }
            self.step(cluster);
        }
        out
    }

    /// Move past one cluster.
    ///
    /// A word is a run of clusters based on letters and numbers — the marks on
    /// them belong to their cluster — and an apostrophe inside one does not end
    /// it, so `don't` is one word. This is the part of UAX #29's word boundaries
    /// that capitalization depends on, not the whole of it.
    fn step(&mut self, cluster: &str) {
        let Some(base) = cluster.chars().next() else {
            return;
        };
        if is_word(base) {
            self.word_start = false;
        } else if self.word_start || !matches!(base, '\'' | '\u{2019}') {
            self.word_start = true;
        }
    }
}

/// Whether a character is part of a word: a letter or a number.
fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// `c` in title case, which is upper case except where Unicode says
/// otherwise.
///
/// The digraphs DŽ, LJ, NJ and DZ have a title case of their own, a capital
/// and a small letter in one character. Georgian Mkhedruli has none: its
/// upper case is Mtavruli, a different alphabet, and a capitalized Georgian
/// word is left as it is.
fn push_titlecase(out: &mut String, c: char) {
    match c {
        '\u{01C4}'..='\u{01C6}' => out.push('\u{01C5}'),
        '\u{01C7}'..='\u{01C9}' => out.push('\u{01C8}'),
        '\u{01CA}'..='\u{01CC}' => out.push('\u{01CB}'),
        '\u{01F1}'..='\u{01F3}' => out.push('\u{01F2}'),
        '\u{10D0}'..='\u{10FF}' => out.push(c),
        _ => out.extend(c.to_uppercase()),
    }
}

/// `full-width`: printable ASCII as its full-width form, and the space as the
/// ideographic space.
fn full_width(c: char) -> char {
    match c {
        ' ' => '\u{3000}',
        '!'..='~' => char::from_u32(u32::from(c) - 0x21 + 0xFF01).unwrap_or(c),
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transformed(text: &str, case: TextCase, full_width: bool) -> String {
        Casing::default()
            .apply(text, TextTransform { case, full_width })
            .into_owned()
    }

    #[test]
    fn upper_case_uses_the_full_mapping() {
        assert_eq!(transformed("straße", TextCase::Uppercase, false), "STRASSE");
    }

    #[test]
    fn lower_case_knows_a_final_sigma() {
        assert_eq!(transformed("ΣΑΣ", TextCase::Lowercase, false), "σας");
    }

    #[test]
    fn capitalize_takes_an_apostrophe_as_inside_a_word() {
        assert_eq!(
            transformed("don't stop-now 3d", TextCase::Capitalize, false),
            "Don't Stop-Now 3d"
        );
        assert_eq!(
            transformed("'quoted' words", TextCase::Capitalize, false),
            "'Quoted' Words"
        );
    }

    #[test]
    fn capitalize_uses_titlecase_where_it_differs() {
        assert_eq!(transformed("ǆemal", TextCase::Capitalize, false), "ǅemal");
        assert_eq!(
            transformed("ქართული", TextCase::Capitalize, false),
            "ქართული"
        );
    }

    #[test]
    fn full_width_is_the_full_width_forms() {
        assert_eq!(transformed("A1 b", TextCase::None, true), "Ａ１\u{3000}ｂ");
        assert_eq!(transformed("a", TextCase::Uppercase, true), "Ａ");
    }

    /// A word that goes on into the next box is the same word, and one after
    /// a space in a box that transforms nothing is a new one.
    #[test]
    fn a_word_is_carried_across_boxes() {
        let capitalize = TextTransform {
            case: TextCase::Capitalize,
            full_width: false,
        };
        let mut casing = Casing::default();
        assert_eq!(casing.apply("h", capitalize), "H");
        assert_eq!(casing.apply("ello world", capitalize), "ello World");
        assert_eq!(casing.apply(" ", TextTransform::NONE), " ");
        assert_eq!(casing.apply("again", capitalize), "Again");
    }
}
