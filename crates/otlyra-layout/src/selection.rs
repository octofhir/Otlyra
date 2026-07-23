//! Selecting text on a laid-out page.
//!
//! A selection is two positions in the page's text and everything between them.
//! A position is a run of glyphs and an offset into the characters that run drew —
//! not a point on the screen, because the same point means a different character
//! once the page has been laid out again, and not a place in the document, because
//! nothing between the document and the page knows where a line broke.
//!
//! Which makes the run the unit: runs are laid out in document order, so comparing
//! two positions is comparing their runs and then their offsets, and everything
//! between them is a slice of the same order.

use crate::fragment::{Fragment, FragmentKind, FragmentTree, Rect};

/// A place in the page's text.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    /// Which run of glyphs, counted in document order.
    pub run: usize,
    /// How many bytes into that run's text.
    pub offset: usize,
}

/// What is selected: from one position to another, in either direction.
///
/// The *anchor* is where the drag started and the *focus* is where the pointer is
/// now, which is what lets a selection be extended backwards.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Where it started.
    pub anchor: TextPosition,
    /// Where it is being taken.
    pub focus: TextPosition,
}

impl Selection {
    /// A selection of nothing, at one position.
    pub fn at(position: TextPosition) -> Self {
        Self {
            anchor: position,
            focus: position,
        }
    }

    /// The two ends in page order.
    pub fn ends(&self) -> (TextPosition, TextPosition) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// Whether it covers no characters at all.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// The runs of a page, in document order, with where each one was drawn.
fn runs(tree: &FragmentTree) -> Vec<&Fragment> {
    tree.iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Text(_)))
        .collect()
}

/// The run and offset a point on the page lands on.
///
/// The nearest character *boundary*, so a click in the left half of a letter
/// selects from before it and one in the right half from after it, which is what
/// makes a drag across a word take the letters the pointer actually passed. A
/// point that is on no line at all takes the nearest line above it, which is what
/// a drag into the margin means.
pub fn position_at(tree: &FragmentTree, x: f32, y: f32) -> Option<TextPosition> {
    let runs = runs(tree);
    if runs.is_empty() {
        return None;
    }

    // The run under the point, or the last one that started above it: a drag that
    // leaves the text sideways stays on the line it left, and one that leaves it
    // downwards takes everything above.
    let mut best: Option<(usize, f32)> = None;
    for (index, run) in runs.iter().enumerate() {
        let rect = run.rect;
        let vertical = if y < rect.y {
            rect.y - y
        } else if y > rect.bottom() {
            y - rect.bottom()
        } else {
            0.0
        };
        let horizontal = if x < rect.x {
            rect.x - x
        } else if x > rect.right() {
            x - rect.right()
        } else {
            0.0
        };
        // A line further away vertically loses however close it is across: text is
        // read in lines, and the nearest thing to a point below a paragraph is the
        // end of its last line rather than whatever sits directly under it.
        let distance = vertical * 1000.0 + horizontal;
        if best.is_none_or(|(_, best)| distance < best) {
            best = Some((index, distance));
        }
    }

    let (index, _) = best?;
    Some(TextPosition {
        run: index,
        offset: offset_at(runs[index], x),
    })
}

/// How many bytes into a run's text the point `x` falls.
fn offset_at(fragment: &Fragment, x: f32) -> usize {
    let FragmentKind::Text(run) = &fragment.kind else {
        return 0;
    };
    if run.text.is_empty() {
        return 0;
    }

    // Each glyph says which characters it drew, so a boundary is a place a glyph
    // starts and the text it started on. Two glyphs of one cluster give the same
    // boundary twice, which costs an entry and changes no answer: there is no
    // place to put a caret inside a ligature.
    let mut boundaries: Vec<(f32, usize)> = Vec::with_capacity(run.glyphs.len() + 1);
    for glyph in &run.glyphs {
        boundaries.push((fragment.rect.x + glyph.x, glyph.text_offset as usize));
    }
    boundaries.push((fragment.rect.right(), run.text.len()));

    // The nearest boundary, which is what puts the caret on the side of the letter
    // the pointer is on.
    boundaries
        .iter()
        .min_by(|one, other| {
            (one.0 - x)
                .abs()
                .partial_cmp(&(other.0 - x).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(0, |(_, at)| *at)
}

/// The word around a position: what a second click selects.
///
/// A word is a run of letters and digits, or a run of anything else that is not
/// a space — so a double click in the middle of `on-screen` takes `on`, and one
/// on the hyphen takes the hyphen, which is what every browser does. It reaches
/// across runs where the text does: a word half in bold is two runs and one
/// word, and the join is only a join where the runs sit on the same line and
/// meet without a gap.
pub fn word_at(tree: &FragmentTree, position: TextPosition) -> Selection {
    let runs = runs(tree);
    let Some(fragment) = runs.get(position.run) else {
        return Selection::at(position);
    };
    let Some(text) = text_of(fragment) else {
        return Selection::at(position);
    };

    // A position at the very end of a run belongs to the character before it,
    // which is what makes a click after the last letter select the last word.
    let at = position.offset.min(text.len());
    let kind = char_before(text, at)
        .filter(|_| at == text.len())
        .or_else(|| text[at..].chars().next())
        .map(class);

    let Some(kind) = kind.filter(|kind| *kind != Class::Space) else {
        return Selection::at(position);
    };

    let mut start = at;
    while let Some(previous) = char_before(text, start).filter(|c| class(*c) == kind) {
        start -= previous.len_utf8();
    }
    let mut end = at;
    while let Some(next) = text[end..].chars().next().filter(|c| class(*c) == kind) {
        end += next.len_utf8();
    }

    let mut anchor = TextPosition {
        run: position.run,
        offset: start,
    };
    let mut focus = TextPosition {
        run: position.run,
        offset: end,
    };

    // Across runs: only where the neighbour continues the same word on the same
    // line with nothing between them.
    while anchor.offset == 0
        && let Some(index) = anchor.run.checked_sub(1)
        && let Some(previous) = runs.get(index)
        && joins(previous, runs[anchor.run])
        && let Some(text) = text_of(previous)
        && char_before(text, text.len()).is_some_and(|c| class(c) == kind)
    {
        let mut start = text.len();
        while let Some(previous) = char_before(text, start).filter(|c| class(*c) == kind) {
            start -= previous.len_utf8();
        }
        anchor = TextPosition {
            run: index,
            offset: start,
        };
    }
    while let Some(next) = runs.get(focus.run + 1)
        && text_of(runs[focus.run]).is_some_and(|text| focus.offset == text.len())
        && joins(runs[focus.run], next)
        && let Some(text) = text_of(next)
        && text.chars().next().is_some_and(|c| class(c) == kind)
    {
        let mut end = 0;
        while let Some(character) = text[end..].chars().next().filter(|c| class(*c) == kind) {
            end += character.len_utf8();
        }
        focus = TextPosition {
            run: focus.run + 1,
            offset: end,
        };
    }

    Selection { anchor, focus }
}

/// The block of text around a position: what a third click selects.
///
/// The block is read off the tree rather than off the geometry: a run sits in a
/// line and a line sits in the box that laid it out, so the box above the line
/// is the paragraph, whatever the margins around it happen to be. Two paragraphs
/// one line apart and two lines of one paragraph look the same on the page and
/// are not the same thing.
pub fn paragraph_at(tree: &FragmentTree, position: TextPosition) -> Selection {
    let runs = runs(tree);
    let blocks = blocks_of(tree);
    let Some(block) = blocks.get(position.run).copied() else {
        return Selection::at(position);
    };

    let mut first = position.run;
    while let Some(index) = first.checked_sub(1)
        && blocks.get(index).copied() == Some(block)
    {
        first = index;
    }
    let mut last = position.run;
    while blocks.get(last + 1).copied() == Some(block) {
        last += 1;
    }

    Selection {
        anchor: TextPosition {
            run: first,
            offset: 0,
        },
        focus: TextPosition {
            run: last,
            offset: runs
                .get(last)
                .and_then(|run| text_of(run))
                .map_or(0, str::len),
        },
    }
}

/// Which block each run belongs to, in the same order [`runs`] returns them.
///
/// A run is in a line and a line is in the box that broke it, so the box above
/// the line is the answer. Anonymous boxes have no identity of their own, so the
/// line's own place in the walk stands in for one.
fn blocks_of(tree: &FragmentTree) -> Vec<usize> {
    fn visit(fragment: &Fragment, block: usize, next: &mut usize, out: &mut Vec<usize>) {
        if matches!(fragment.kind, FragmentKind::Text(_)) {
            out.push(block);
        }
        // A box holding lines is a block, and every line in it — and everything
        // in those lines — is that one block. A box holding no lines passes on
        // whatever it was given, which is what keeps an inline box inside the
        // paragraph it is part of.
        let block = if fragment
            .children
            .iter()
            .any(|child| matches!(child.kind, FragmentKind::Line))
        {
            *next += 1;
            *next
        } else {
            block
        };
        for child in &fragment.children {
            visit(child, block, next, out);
        }
    }

    let mut out = Vec::new();
    let mut next = 0;
    visit(&tree.root, 0, &mut next, &mut out);
    out
}

/// Everything on the page, from its first character to its last.
pub fn all(tree: &FragmentTree) -> Option<Selection> {
    let runs = runs(tree);
    let last = runs.len().checked_sub(1)?;
    Some(Selection {
        anchor: TextPosition { run: 0, offset: 0 },
        focus: TextPosition {
            run: last,
            offset: text_of(runs[last]).map_or(0, str::len),
        },
    })
}

/// What kind of character this is, for the purpose of finding a word.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Class {
    /// A letter, a digit, or the underscore that goes with them.
    Word,
    /// Anything else that is not a space.
    Symbol,
    /// A space, which no word crosses.
    Space,
}

fn class(character: char) -> Class {
    if character.is_whitespace() {
        Class::Space
    } else if character.is_alphanumeric() || character == '_' {
        Class::Word
    } else {
        Class::Symbol
    }
}

/// The text a run drew, if it is a run of text.
fn text_of(fragment: &Fragment) -> Option<&str> {
    match &fragment.kind {
        FragmentKind::Text(run) => Some(&*run.text),
        _ => None,
    }
}

/// The character ending at `at`.
fn char_before(text: &str, at: usize) -> Option<char> {
    text.get(..at).and_then(|before| before.chars().next_back())
}

/// Whether two runs are neighbours a word may cross: same line, no gap.
fn joins(left: &Fragment, right: &Fragment) -> bool {
    (left.rect.y - right.rect.y).abs() < 0.5 && (right.rect.x - left.rect.right()).abs() < 0.5
}

/// One step a caret takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One character back.
    Back,
    /// One character on.
    Forward,
    /// The line above, keeping roughly the same place across it.
    Up,
    /// The line below, likewise.
    Down,
    /// The start of the line this is on.
    LineStart,
    /// The end of it.
    LineEnd,
    /// The first character of the page.
    Start,
    /// The last.
    End,
}

/// Where `position` goes when the caret is moved.
///
/// Characters rather than bytes: a step that landed inside one would be a step
/// into the middle of a letter, and every offset here is a boundary in the text
/// a run drew.
pub fn moved(tree: &FragmentTree, position: TextPosition, motion: Motion) -> TextPosition {
    let runs = runs(tree);
    if runs.is_empty() {
        return position;
    }
    let last = runs.len() - 1;
    let length = |index: usize| {
        runs.get(index)
            .and_then(|run| text_of(run))
            .map_or(0, str::len)
    };

    match motion {
        Motion::Back => {
            let text = runs.get(position.run).and_then(|run| text_of(run));
            match text.and_then(|text| char_before(text, position.offset.min(text.len()))) {
                Some(character) => TextPosition {
                    run: position.run,
                    offset: position.offset - character.len_utf8(),
                },
                // The start of a run is the end of the one before it: the two are
                // the same place, and stopping at both would take two presses to
                // cross one boundary.
                None => match position.run.checked_sub(1) {
                    Some(previous) => TextPosition {
                        run: previous,
                        offset: length(previous),
                    },
                    None => position,
                },
            }
        }
        Motion::Forward => {
            let text = runs.get(position.run).and_then(|run| text_of(run));
            match text.and_then(|text| text.get(position.offset..)?.chars().next()) {
                Some(character) => TextPosition {
                    run: position.run,
                    offset: position.offset + character.len_utf8(),
                },
                None if position.run < last => TextPosition {
                    run: position.run + 1,
                    offset: 0,
                },
                None => position,
            }
        }
        Motion::Up | Motion::Down => {
            let Some(fragment) = runs.get(position.run) else {
                return position;
            };
            let x = edge(fragment, position.offset);
            let step = fragment.rect.height.max(1.0);
            let y = if motion == Motion::Up {
                fragment.rect.y - step / 2.0
            } else {
                fragment.rect.bottom() + step / 2.0
            };
            position_at(tree, x, y).unwrap_or(position)
        }
        Motion::LineStart | Motion::LineEnd => {
            let Some(fragment) = runs.get(position.run) else {
                return position;
            };
            let on_line = |other: &&Fragment| (other.rect.y - fragment.rect.y).abs() < 0.5;
            match motion {
                Motion::LineStart => {
                    let mut first = position.run;
                    while let Some(index) = first.checked_sub(1)
                        && on_line(&runs[index])
                    {
                        first = index;
                    }
                    TextPosition {
                        run: first,
                        offset: 0,
                    }
                }
                _ => {
                    let mut end = position.run;
                    while end < last && on_line(&runs[end + 1]) {
                        end += 1;
                    }
                    TextPosition {
                        run: end,
                        offset: length(end),
                    }
                }
            }
        }
        Motion::Start => TextPosition { run: 0, offset: 0 },
        Motion::End => TextPosition {
            run: last,
            offset: length(last),
        },
    }
}

/// The rectangles a selection covers, one per run it touches.
///
/// In page coordinates, at the height of the run's own box, so a selection across
/// a heading and a paragraph is drawn at the height each of them was set at.
pub fn rects(tree: &FragmentTree, selection: Selection) -> Vec<Rect> {
    let runs = runs(tree);
    let (start, end) = selection.ends();
    let mut out = Vec::new();

    for (index, fragment) in runs.iter().enumerate() {
        if index < start.run || index > end.run {
            continue;
        }
        let FragmentKind::Text(run) = &fragment.kind else {
            continue;
        };
        let from = if index == start.run { start.offset } else { 0 };
        let to = if index == end.run {
            end.offset
        } else {
            run.text.len()
        };
        if from >= to {
            continue;
        }

        let left = edge(fragment, from);
        let right = edge(fragment, to);
        if right > left {
            out.push(Rect::new(
                left,
                fragment.rect.y,
                right - left,
                fragment.rect.height,
            ));
        }
    }

    out
}

/// Where a byte offset sits across a run.
fn edge(fragment: &Fragment, offset: usize) -> f32 {
    let FragmentKind::Text(run) = &fragment.kind else {
        return fragment.rect.x;
    };
    if offset >= run.text.len() {
        return fragment.rect.right();
    }

    // The first glyph that starts at or past the offset, which for an offset
    // inside a ligature is the glyph *after* it: a selection cannot end half way
    // through one shape, so it takes the whole of it.
    for glyph in &run.glyphs {
        if glyph.text_offset as usize >= offset {
            return fragment.rect.x + glyph.x;
        }
    }
    fragment.rect.right()
}

/// The characters a selection covers, in document order.
///
/// One run runs into the next with nothing between them, because a run ends where
/// the next begins — except across lines, where the break is a break in the text
/// as well and comes back as a newline.
pub fn text(tree: &FragmentTree, selection: Selection) -> String {
    let runs = runs(tree);
    let (start, end) = selection.ends();
    let mut out = String::new();
    let mut previous_bottom: Option<f32> = None;

    for (index, fragment) in runs.iter().enumerate() {
        if index < start.run || index > end.run {
            continue;
        }
        let FragmentKind::Text(run) = &fragment.kind else {
            continue;
        };
        let from = if index == start.run { start.offset } else { 0 };
        let to = if index == end.run {
            end.offset
        } else {
            run.text.len()
        };
        let Some(slice) = run.text.get(from..to) else {
            continue;
        };
        if slice.is_empty() {
            continue;
        }

        if previous_bottom.is_some_and(|bottom| fragment.rect.y >= bottom - 0.01) {
            out.push('\n');
        }
        out.push_str(slice);
        previous_bottom = Some(fragment.rect.bottom());
    }

    out
}
