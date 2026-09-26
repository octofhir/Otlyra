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

/// How tall each line of a paragraph is, and where the things placed by the
/// line rather than by a baseline ended up on it.
pub(super) struct LineLevels {
    /// How far each line reaches above and below its baseline.
    pub(super) reach: Vec<(f32, f32)>,
    /// How far each run's glyphs are raised off their line's baseline, for
    /// the runs of `top` and `bottom` spans; `None` for the rest, which are
    /// raised as far as their span is.
    pub(super) run_shift: Vec<Option<f32>>,
    /// How far each atomic inline is raised off its line's baseline, in step
    /// with the paragraph's.
    pub(super) replaced_shift: Vec<f32>,
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
/// actually landed on it: first the block's strut, then everything aligned to a
/// baseline, then what is aligned to the line's top or bottom, which is
/// placed in the line the rest made (§10.8: those are aligned last) and makes
/// it taller only where it is taller than that line.
///
/// `run_reach` is how far each run's own font reaches, where its line height is
/// `normal`, already moved by its span's shift: a line is as tall as the fonts
/// actually on it ask, fallback fonts included (CSS Inline 3 §4.2).
pub(super) fn line_reaches(
    content: &InlineContent<'_>,
    levels: &Levels,
    shaped: &ShapedText,
    run_reach: &[Option<(f32, f32)>],
) -> LineLevels {
    use super::collect::LineAlign;

    let mut reach = vec![levels.strut; shaped.lines.len()];
    let edge_of = |span: usize| levels.line_relative.iter().find(|edge| edge.span == span);

    // The spans on each line. A span is on a line if any of its bytes were drawn
    // there.
    for (run, own) in shaped.runs.iter().zip(run_reach) {
        let Some(line) = reach.get_mut(run.line) else {
            continue;
        };
        let spans: Vec<usize> = content.spans_in(run.text_range.clone()).collect();
        if spans.iter().any(|&span| edge_of(span).is_some()) {
            continue;
        }
        if let Some((above, below)) = *own {
            line.0 = line.0.max(above);
            line.1 = line.1.max(below);
        }
        for index in spans {
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
        if let LineAlign::Shift(shift) = box_.align {
            let above = box_.baseline + shift;
            line.0 = line.0.max(above);
            line.1 = line.1.max(box_.outer_height() - above);
        }
    }

    // What hangs from the line's top or stands on its bottom needs the line to
    // be at least as tall as it is, and grows it the other way.
    let grow = |line: &mut (f32, f32), height: f32, top: bool| {
        if top {
            line.1 = line.1.max(height - line.0);
        } else {
            line.0 = line.0.max(height - line.1);
        }
    };
    for run in &shaped.runs {
        let edge = content.spans_in(run.text_range.clone()).find_map(edge_of);
        if let (Some(edge), Some(line)) = (edge, reach.get_mut(run.line)) {
            grow(line, edge.strut.height(), edge.top);
        }
    }
    for spacer in &shaped.spacers {
        let (Some(line), Some(box_)) = (
            reach.get_mut(spacer.line),
            content.replaced_by_spacer(spacer.id),
        ) else {
            continue;
        };
        match box_.align {
            LineAlign::Top => grow(line, box_.outer_height(), true),
            LineAlign::Bottom => grow(line, box_.outer_height(), false),
            LineAlign::Shift(_) => {}
        }
    }

    // Now each line is as tall as it will be, the edge-aligned things go to
    // its edges.
    let run_shift = shaped
        .runs
        .iter()
        .map(|run| {
            let edge = content.spans_in(run.text_range.clone()).find_map(edge_of)?;
            let &(above, below) = reach.get(run.line)?;
            Some(if edge.top {
                above - edge.strut.ascent
            } else {
                edge.strut.descent - below
            })
        })
        .collect();
    let replaced_shift = content
        .replaced
        .iter()
        .enumerate()
        .map(|(number, box_)| {
            let line = shaped
                .spacers
                .iter()
                .find(|spacer| spacer.id == super::collect::replaced_spacer(number))
                .and_then(|spacer| reach.get(spacer.line));
            match (box_.align, line) {
                (LineAlign::Shift(shift), _) => shift,
                (LineAlign::Top, Some(&(above, _))) => above - box_.baseline,
                (LineAlign::Bottom, Some(&(_, below))) => {
                    box_.outer_height() - below - box_.baseline
                }
                (LineAlign::Top | LineAlign::Bottom, None) => 0.0,
            }
        })
        .collect();

    LineLevels {
        reach,
        run_shift,
        replaced_shift,
    }
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
