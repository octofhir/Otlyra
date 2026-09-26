//! Where a paragraph's lines may break: at its soft wrap opportunities, and
//! nowhere else (CSS Text 3 §5).
//!
//! The shaper finds the text's own opportunities — UAX #14, as Chromium tailors
//! it — and on its own would also break in three places CSS has none. It allows
//! a break after every inline box, and before one that does not fit; but the
//! edges of an inline element are no break at all, and `<a>About</a>Us` is one
//! word however much padding the link has. And it ends a line at a no-break
//! space that does not fit, as it would at a space that hangs; but a no-break
//! space does not hang, and prohibits a break on either side (UAX #14 class GL).
//!
//! So where a paragraph has either, each line's end is chosen here, from the
//! opportunities the text has on its own and the room each line has; and the
//! shaper is handed exactly the width that makes it break there. It still does
//! the breaking — every line is its line, glyphs, boxes and all — but never
//! somewhere this has not chosen.

use std::collections::HashMap;

use parley::{BreakReason, Layout, PositionedLayoutItem};

use crate::engine::{LINE_FIT_SLACK, SpacerKind, SpanBrush};

/// Whether a paragraph's lines have to be chosen here rather than left to the
/// shaper: whether it has anything the shaper breaks at that CSS does not.
pub(crate) fn needs_plan(text: &str, kinds: &HashMap<u64, SpacerKind>) -> bool {
    text.contains('\u{a0}') || kinds.values().any(|kind| *kind != SpacerKind::Atomic)
}

/// How a cluster of glyphs takes part in breaking a line.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum Glyphs {
    /// Anything that stays on the line it is set on.
    Ink,
    /// A space or a tab, which hangs past the end of a line that ends in it
    /// rather than pushing anything onto the next (CSS Text 3 §4.1.3).
    Space,
    /// A forced break: a newline, a `<br>`, a line or paragraph separator.
    Break,
}

impl Glyphs {
    /// What the characters of one cluster are, or `None` for a cluster with
    /// no text behind it — the space the shaper sets an empty paragraph in.
    pub(crate) fn of(chars: Option<&str>) -> Self {
        let Some(chars) = chars.filter(|chars| !chars.is_empty()) else {
            return Self::Space;
        };
        if chars
            .chars()
            .all(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
        {
            Self::Break
        } else if chars.chars().all(|c| matches!(c, ' ' | '\t')) {
            Self::Space
        } else {
            Self::Ink
        }
    }
}

/// One thing a line is made of, in the order the text runs.
#[derive(Copy, Clone, Debug)]
enum Piece {
    /// A cluster of glyphs, and where in the text it starts.
    Text {
        start: usize,
        advance: f32,
        glyphs: Glyphs,
    },
    /// A spacer the caller asked for.
    Spacer { kind: SpacerKind, width: f32 },
}

impl Piece {
    fn advance(&self) -> f32 {
        match *self {
            Self::Text { advance, .. } => advance,
            Self::Spacer { width, .. } => width,
        }
    }

    /// An inline box's edge, which is not content: a break is placed around
    /// it by what is on either side.
    fn is_edge(&self) -> bool {
        matches!(
            self,
            Self::Spacer {
                kind: SpacerKind::Opening | SpacerKind::Closing,
                ..
            }
        )
    }
}

/// Every piece of a broken `layout`, in order.
///
/// Read off lines the shaper already broke, since a cluster's advance is only
/// to be had from a line; where the lines broke does not change what is on
/// them.
fn pieces(layout: &Layout<SpanBrush>, text: &str, kinds: &HashMap<u64, SpacerKind>) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for line in layout.lines() {
        // A run the shaper split by style comes back as several glyph runs,
        // each holding the whole run's clusters.
        let mut last_run = None;
        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(glyph_run) => {
                    let run = glyph_run.run();
                    if last_run == Some(run.index()) {
                        continue;
                    }
                    last_run = Some(run.index());
                    pieces.extend((0..run.len()).filter_map(|index| {
                        let cluster = run.get(index)?;
                        let range = cluster.text_range();
                        Some(Piece::Text {
                            start: range.start,
                            advance: cluster.advance(),
                            glyphs: Glyphs::of(text.get(range)),
                        })
                    }));
                }
                PositionedLayoutItem::InlineBox(placed) => {
                    last_run = None;
                    pieces.push(Piece::Spacer {
                        kind: kinds.get(&placed.id).copied().unwrap_or(SpacerKind::Atomic),
                        width: placed.width,
                    });
                }
            }
        }
    }
    pieces
}

/// The byte offsets where the text of `layout` may break, from a pass that
/// broke it at every one.
///
/// A layout broken to no width at all ends a line at each opportunity it has,
/// and at the two places the shaper breaks where the text does not: after a
/// no-break space, and between two preserved spaces, the first of which it
/// hung. Those two are left out.
fn opportunities(layout: &Layout<SpanBrush>, text: &str) -> Vec<usize> {
    let lines: Vec<_> = layout.lines().collect();
    lines
        .windows(2)
        .filter(|pair| {
            matches!(
                pair[0].break_reason(),
                BreakReason::Regular | BreakReason::Emergency
            )
        })
        .map(|pair| pair[1].text_range().start)
        .filter(|&at| {
            let before = text.get(..at).and_then(|text| text.chars().next_back());
            let after = text.get(at..).and_then(|text| text.chars().next());
            !matches!(
                (before, after),
                (Some('\u{a0}'), _) | (Some(' '), Some(' '))
            )
        })
        .collect()
}

/// Break `layout` to no width at all, which leaves every piece of it on some
/// line and ends a line wherever the shaper would.
pub(crate) fn break_everywhere(layout: &mut Layout<SpanBrush>) {
    let mut breaker = layout.break_lines();
    breaker.state_mut().set_layout_max_advance(0.0);
    breaker.state_mut().set_line_max_advance(0.0);
    while breaker.break_next().is_some() {}
}

/// Where a paragraph may break and what each piece of it takes, for choosing
/// its lines one at a time.
pub(crate) struct Plan {
    pieces: Vec<Piece>,
    /// Whether a line may break before each piece; one more entry than there
    /// are pieces, the last never.
    breaks_before: Vec<bool>,
}

impl Plan {
    /// The plan for `layout`, already broken everywhere; `opportunities_in`
    /// is the text's own opportunities, broken the same way, when `layout` has
    /// edges in it that would add some of their own.
    pub(crate) fn new(
        layout: &Layout<SpanBrush>,
        opportunities_in: &Layout<SpanBrush>,
        text: &str,
        kinds: &HashMap<u64, SpacerKind>,
    ) -> Self {
        let pieces = pieces(layout, text, kinds);
        let opportunities = opportunities(opportunities_in, text);
        let breaks_before = (0..=pieces.len())
            .map(|slot| may_break_before(&pieces, &opportunities, slot))
            .collect();
        Self {
            pieces,
            breaks_before,
        }
    }

    /// Choose the line that starts at piece `start`, in `room` (slack and
    /// all): the advance to hand the shaper so that it breaks where this line
    /// ends, and the piece the next line starts at.
    ///
    /// The last opportunity whose line fits, with any spaces before it hanging
    /// past the room (CSS Text 3 §4.1.3); the first there is, and the line
    /// overflowing, where none does (§5.2); and the forced break or the end of
    /// the paragraph where either comes first. A break is only taken past
    /// something with width, which is where the shaper would take it.
    pub(crate) fn line(&self, start: usize, room: f32) -> (f32, usize) {
        let mut x = 0.0f32;
        let mut last_fit: Option<(usize, f32)> = None;
        for (slot, piece) in self.pieces.iter().enumerate().skip(start) {
            if slot > start && x != 0.0 && self.breaks_before[slot] {
                last_fit = Some((slot, x));
            }
            match *piece {
                Piece::Text {
                    glyphs: Glyphs::Break,
                    ..
                } => return (room.max(x + LINE_FIT_SLACK), slot + 1),
                Piece::Text {
                    advance,
                    glyphs: Glyphs::Space,
                    ..
                } => x += advance,
                _ => {
                    let next = x + piece.advance();
                    if next > room {
                        return match last_fit {
                            Some((slot, at)) => (at + LINE_FIT_SLACK, self.settle(slot)),
                            None => self.overflow(slot, x),
                        };
                    }
                    x = next;
                }
            }
        }
        (room.max(x + LINE_FIT_SLACK), self.pieces.len())
    }

    /// The line that has to overflow because nothing before `from` fits: it
    /// runs on to the first opportunity after it, or a forced break, or the
    /// end.
    fn overflow(&self, from: usize, mut x: f32) -> (f32, usize) {
        for (slot, piece) in self.pieces.iter().enumerate().skip(from) {
            if slot > from && x != 0.0 && self.breaks_before[slot] {
                return (x + LINE_FIT_SLACK, self.settle(slot));
            }
            if let Piece::Text {
                glyphs: Glyphs::Break,
                ..
            } = piece
            {
                return (f32::INFINITY, slot + 1);
            }
            x += piece.advance();
        }
        (f32::INFINITY, self.pieces.len())
    }

    /// Where the next line really starts, once the shaper has broken before
    /// `slot`: past any spacer with no width, which fits on the line however
    /// full it is and so stays on it.
    fn settle(&self, mut slot: usize) -> usize {
        while let Some(Piece::Spacer { width, .. }) = self.pieces.get(slot)
            && *width <= 0.0
        {
            slot += 1;
        }
        slot
    }
}

/// Whether a line may break before `pieces[slot]`.
///
/// An inline box's edges hold on to what they are the edge of: its opening
/// edge to what follows and its closing edge to what goes before. Past them,
/// a break sits beside an atomic inline, which UAX #14 breaks either side of as
/// it does U+FFFC (LB20), or between two pieces of text that have an
/// opportunity between them on their own.
fn may_break_before(pieces: &[Piece], opportunities: &[usize], slot: usize) -> bool {
    let (Some(before), Some(after)) = (slot.checked_sub(1).map(|at| &pieces[at]), pieces.get(slot))
    else {
        return false;
    };
    let opens = matches!(
        before,
        Piece::Spacer {
            kind: SpacerKind::Opening,
            ..
        }
    );
    let closes = matches!(
        after,
        Piece::Spacer {
            kind: SpacerKind::Closing,
            ..
        }
    );
    if opens || closes {
        return false;
    }
    let content_before = pieces[..slot].iter().rev().find(|piece| !piece.is_edge());
    let content_after = pieces[slot..].iter().find(|piece| !piece.is_edge());
    match (content_before, content_after) {
        (Some(Piece::Text { .. }), Some(Piece::Text { start, .. })) => {
            opportunities.binary_search(start).is_ok()
        }
        (Some(_), Some(_)) => true,
        _ => false,
    }
}
