//! The lines a paragraph breaks into: shaped into the room the floats leave
//! each of them, then stacked as tall as what landed on each one.

use otlyra_text::ShapedText;

use crate::box_tree::BoxId;
use crate::flow::Flow;
use crate::flow::float::line_room;

use super::collect::{InlineContent, inline_spacers};
use super::vertical_align::Levels;

/// The room one line had to fill: where it starts, from the block's content
/// edge, and how wide it is. Narrower than the block wherever a float sits
/// beside the line.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Band {
    pub(super) start: f32,
    pub(super) width: f32,
}

impl<'a> Flow<'a> {
    /// Shape the paragraph `parent` holds into lines, in a block whose content
    /// box is `width` wide at `x`, `y` on the page, and say what room each line
    /// had.
    ///
    /// Each line asks how much room the floats have left it at the height it
    /// landed at, and where that room starts; the width goes to the shaper and
    /// the start is kept for placing the line.
    pub(super) fn shape_lines(
        &mut self,
        parent: BoxId,
        content: &InlineContent<'_>,
        width: f32,
        x: f32,
        y: f32,
    ) -> (ShapedText, Vec<Band>) {
        // `text-wrap-mode: nowrap` — which is half of what `white-space: nowrap`
        // means — is a line that may not be broken however narrow the box is. No
        // width offered to the shaper is exactly that: it lays the run out on one
        // line and lets it overflow, which is what the property asks for.
        let wraps = self.tree.node(parent).style.text_wrap != otlyra_css::TextWrap::NoWrap;
        let mut bands: Vec<Band> = Vec::new();
        let floats = &self.floats;
        let mut collect_band = |index: usize, top: f32| {
            let (start, available) = line_room(floats, y + top, 1.0, x, width);
            if bands.len() <= index {
                bands.resize(index + 1, Band { start: 0.0, width });
            }
            bands[index] = Band {
                start,
                width: available,
            };
            wraps.then_some(available)
        };
        let spacers = inline_spacers(&content.inlines, &content.replaced);
        let shaped = self
            .text
            .shape_spans_wrapping(&content.spans, &spacers, &mut collect_band);
        (shaped, bands)
    }
}

/// How far each line of a shaped paragraph reaches above and below its
/// baseline: as tall as what is *on* it, with the baseline as far down as the
/// tallest thing on it reaches (CSS 2.2 §10.8).
///
/// The shaper answers neither question. It carries a line height per run of
/// glyphs and opens a run when the font changes, so a span that wants a taller
/// line without changing font cannot be told to it at all; and where it can, it
/// centres the font inside the height rather than putting the baseline where the
/// tallest thing on the line needs it. So each line is measured here, from what
/// actually landed on it.
pub(super) fn line_reaches(
    content: &InlineContent<'_>,
    levels: &Levels,
    shaped: &ShapedText,
) -> Vec<(f32, f32)> {
    // A line holding a picture is the shaper's to measure: a picture stands on
    // the baseline with all of its height above, the text beside it hangs
    // below, and the shaper has already put the box around both — which is the
    // answer for a picture whose ink reaches past the box the font would have
    // given it. Every other line starts at the block's own strut.
    let mut holds_picture = vec![false; shaped.lines.len()];
    for spacer in &shaped.spacers {
        let picture = content
            .replaced_by_spacer(spacer.id)
            .is_some_and(|box_| box_.content.is_none());
        if picture && let Some(slot) = holds_picture.get_mut(spacer.line) {
            *slot = true;
        }
    }
    let mut reach: Vec<(f32, f32)> = shaped
        .lines
        .iter()
        .zip(holds_picture)
        .map(|(line, picture)| {
            if picture {
                (line.baseline - line.top, line.bottom - line.baseline)
            } else {
                levels.strut
            }
        })
        .collect();

    // The spans on each line. A span is on a line if any of its bytes were drawn
    // there.
    for run in &shaped.runs {
        let Some(line) = reach.get_mut(run.line) else {
            continue;
        };
        for index in content.spans_in(run.text_range.clone()) {
            let Some(&(above, below)) = levels.span_reach.get(index) else {
                continue;
            };
            line.0 = line.0.max(above);
            line.1 = line.1.max(below);
        }
    }

    // And the boxes in the line, which the shaper placed but did not stack.
    // A picture sits with its bottom margin edge on the baseline, so all of it
    // is above; an inline block sits on its own last baseline and is held
    // above and below that. The margin box asks for the room, not the border
    // box (CSS 2.2 §10.8): a slider with two pixels above and below it makes
    // the line it is in four pixels taller, which is the difference between a
    // row of controls sitting in their line and sitting through it.
    for spacer in &shaped.spacers {
        let (Some(line), Some(box_)) = (
            reach.get_mut(spacer.line),
            content.replaced_by_spacer(spacer.id),
        ) else {
            continue;
        };
        let above = box_.baseline + box_.shift;
        line.0 = line.0.max(above);
        line.1 = line.1.max(box_.outer_height() - above);
    }
    reach
}

/// Stack the lines of a paragraph by how far each reaches — one reach per line,
/// as [`line_reaches`] gives them. Every line ends where the next begins, and
/// the glyphs and boxes on it move with the baseline they sit on.
pub(super) fn restack(shaped: &mut ShapedText, reach: &[(f32, f32)]) {
    let mut cursor = shaped.lines.first().map_or(0.0, |line| line.top);
    let mut shifts: Vec<f32> = Vec::with_capacity(shaped.lines.len());
    for (line, &(above, below)) in shaped.lines.iter_mut().zip(reach) {
        let baseline = cursor + above;
        shifts.push(baseline - line.baseline);
        line.top = cursor;
        line.baseline = baseline;
        line.height = above + below;
        line.bottom = cursor + line.height;
        cursor = line.bottom;
    }
    for run in &mut shaped.runs {
        let shift = shifts.get(run.line).copied().unwrap_or(0.0);
        for glyph in &mut run.glyphs {
            glyph.y += shift;
        }
    }
    for spacer in &mut shaped.spacers {
        spacer.y += shifts.get(spacer.line).copied().unwrap_or(0.0);
    }
    shaped.metrics.first_baseline = shaped.lines.first().map_or(0.0, |line| line.baseline);
    // The paragraph reaches from the top of its first line to the bottom of
    // its last, which is what its own box is.
    if let (Some(first), Some(last)) = (shaped.lines.first(), shaped.lines.last()) {
        shaped.metrics.height = last.bottom - first.top;
    }
}
