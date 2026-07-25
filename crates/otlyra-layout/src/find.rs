//! Finding a string in the text of a laid-out page.
//!
//! A match is the same thing a selection is — two places in the page's text — so
//! everything that already knows where a place in the page's text is drawn works
//! on a match without being told about one. That is the whole reason this answers
//! in [`Selection`]s: ligatures, line breaks and bidi runs are settled arithmetic
//! in `selection`, and a second opinion about them would be a second set of bugs.
//!
//! What this adds is the reading order the search runs over. A page's text is not
//! one string anywhere: it is a run per piece of styling and a run per line, so
//! `<b>bold</b>face` is two runs and one word, and a paragraph that wraps is a run
//! per line. So the runs are strung together into one sequence of characters with,
//! for each character, the run and the bytes it came from — and a match found in
//! the sequence comes back as two positions in the page.
//!
//! Three rules make that sequence the one a reader would look for a phrase in:
//!
//! - Runs that meet on a line with nothing between them are one piece of text,
//!   because that is what a word broken by a `<b>` looks like.
//! - Runs in the same block that do not meet — a line break, or a gap — are joined
//!   by one space, because a phrase a reader can see on two lines is one phrase.
//! - Runs in different blocks are separated by a newline, which no query can
//!   contain, so a phrase never runs out of one paragraph into the next.
//!
//! Whitespace is collapsed to one space on both sides of the comparison, so
//! whatever the markup did with its indentation does not decide whether a phrase
//! is found. The comparison itself is a lowercased substring and nothing else:
//! **there are no regular expressions here**, no whole-word option and no
//! diacritic folding.

use crate::fragment::FragmentTree;
use crate::selection::{self, Selection, TextPosition};

/// Where one character of the page's linearized text came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Origin {
    /// Which run drew it, counted the way a selection counts runs.
    run: usize,
    /// The byte it starts at in that run's own text.
    start: usize,
    /// The byte after it.
    end: usize,
}

/// The text of a laid-out page as one sequence, and the way back into the page.
///
/// Characters rather than bytes, because lowercasing is not a byte-for-byte
/// operation and a match has to come back as an offset into what the run actually
/// drew. Each character carries the bytes it came from, so a match that starts in
/// one run and ends in another is two positions and needs no further arithmetic.
pub struct PageText {
    /// The page's text, lowercased, with its whitespace collapsed.
    characters: Vec<char>,
    /// Where each of those characters came from, one entry each.
    origins: Vec<Origin>,
}

impl PageText {
    /// String the page's runs together, in the order they are painted in.
    pub fn of(tree: &FragmentTree) -> Self {
        let runs = selection::runs(tree);
        let blocks = selection::blocks_of(tree);
        let mut out = Self {
            characters: Vec::new(),
            origins: Vec::new(),
        };

        let mut previous: Option<usize> = None;
        for (index, fragment) in runs.iter().enumerate() {
            let Some(text) = selection::text_of(fragment) else {
                continue;
            };

            // The seam with the run before this one. Its own end is where a
            // separator is anchored: a separator is not a character anybody drew,
            // so it is given no width, and a match that begins or ends on one
            // begins or ends where the run it follows did.
            if let Some(before) = previous {
                let at = Origin {
                    run: before,
                    start: selection::text_of(runs[before]).map_or(0, str::len),
                    end: selection::text_of(runs[before]).map_or(0, str::len),
                };
                if blocks.get(before) != blocks.get(index) {
                    out.push_break(at);
                } else if !selection::joins(runs[before], runs[index]) {
                    out.push_space(at);
                }
            }

            for (at, character) in text.char_indices() {
                let origin = Origin {
                    run: index,
                    start: at,
                    end: at + character.len_utf8(),
                };
                if character.is_whitespace() {
                    out.push_space(origin);
                } else {
                    // Lowercasing one character can produce several. They share
                    // the bytes they came from, so a match that ends part way
                    // through one takes the whole of it — which is the only thing
                    // it could mean on the page.
                    for lowered in character.to_lowercase() {
                        out.push(lowered, origin);
                    }
                }
            }
            previous = Some(index);
        }

        out
    }

    /// The characters the page reads as, for whoever wants to look at them.
    pub fn text(&self) -> String {
        self.characters.iter().collect()
    }

    /// Every place `query` occurs, in document order and not overlapping.
    ///
    /// Not overlapping because a reader stepping through `aa` in `aaaa` expects
    /// two stops rather than three: the count in a find bar is the count of
    /// places to go, and two of them cannot be the same characters.
    pub fn matches(&self, query: &str) -> Vec<Selection> {
        let needle = normalize(query);
        if needle.is_empty() || needle.len() > self.characters.len() {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut at = 0;
        while at + needle.len() <= self.characters.len() {
            if self.characters[at..at + needle.len()] == needle[..] {
                out.push(self.span(at, at + needle.len()));
                at += needle.len();
            } else {
                at += 1;
            }
        }
        out
    }

    /// The two places in the page a run of characters `from..to` covers.
    fn span(&self, from: usize, to: usize) -> Selection {
        let first = self.origins[from];
        let last = self.origins[to - 1];
        Selection {
            anchor: TextPosition {
                run: first.run,
                offset: first.start,
            },
            focus: TextPosition {
                run: last.run,
                offset: last.end,
            },
        }
    }

    fn push(&mut self, character: char, origin: Origin) {
        self.characters.push(character);
        self.origins.push(origin);
    }

    /// A space, unless the last character already separates.
    fn push_space(&mut self, origin: Origin) {
        if matches!(self.characters.last(), None | Some(' ' | '\n')) {
            return;
        }
        self.push(' ', origin);
    }

    /// A break nothing matches across, swallowing a space it lands on.
    fn push_break(&mut self, origin: Origin) {
        match self.characters.last() {
            None | Some('\n') => {}
            Some(' ') => {
                let last = self.characters.len() - 1;
                self.characters[last] = '\n';
                self.origins[last] = origin;
            }
            Some(_) => self.push('\n', origin),
        }
    }
}

/// Every place `query` occurs in the text of the page, in document order.
///
/// The whole of the search for a caller with a tree and a query and no interest
/// in the sequence they were compared in.
pub fn matches(tree: &FragmentTree, query: &str) -> Vec<Selection> {
    PageText::of(tree).matches(query)
}

/// A query as it is looked for: lowercased, its whitespace collapsed the way the
/// page's own is.
///
/// A newline never survives this, which is what keeps a match inside one block:
/// the page puts one between two paragraphs and nothing a reader can type will
/// ever meet it.
fn normalize(query: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for character in query.chars() {
        if character.is_whitespace() {
            if out.last() != Some(&' ') {
                out.push(' ');
            }
        } else {
            out.extend(character.to_lowercase());
        }
    }
    out
}
