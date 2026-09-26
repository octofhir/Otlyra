//! Where things sit in a line: `vertical-align`, baselines, and how tall a line
//! has to be.
//!
//! The shaper decides where the words go along a line and nothing about how far
//! a raised span or an inline block reaches above and below it. That is worked
//! out here: how far a rule moves a box, where an inline block's own baseline
//! is, and — before the paragraph is shaped — how far each of its spans reaches,
//! which is what the line boxes are rebuilt from once it has been.

use std::collections::HashMap;
use std::sync::Arc;

use otlyra_css::ComputedStyle;

use crate::box_tree::{BoxId, BoxTree};
use crate::flow::Flow;

use super::collect::InlineContent;

/// The `vertical-align` that places `source`'s content on a line of `block`'s.
///
/// Its own, with one exception: text directly inside the block is in the block's
/// root inline box, which nothing moves. On a block container — a table cell, an
/// inline block — the property places the box itself, in its row or in the line
/// outside it, and says nothing about the lines inside (CSS 2.2 §10.8.1).
///
/// Levelling a line and placing what is on it both ask here, so the room a line
/// makes for a shift and where the glyphs actually go cannot disagree.
fn alignment_on_line(
    source: BoxId,
    block: BoxId,
    style: &ComputedStyle,
) -> &otlyra_css::VerticalAlign {
    if source == block {
        &otlyra_css::VerticalAlign::Baseline
    } else {
        &style.vertical_align
    }
}

/// How far a rule raises the box it is written on off its parent's baseline,
/// in CSS pixels, positive up.
///
/// The two keywords are measured against the *parent's* font size rather than
/// the box's own, which is what makes a superscript sit at the same height
/// whether it is set small or not. A third of the font size, and a fifth of it,
/// are what the specification names as the amounts to use when a UA does not
/// take them from the font — plus the pixel every engine adds on top, which is
/// the amount the web was actually built against.
fn baseline_shift_of(
    align: &otlyra_css::VerticalAlign,
    style: &ComputedStyle,
    parent: &ComputedStyle,
) -> f32 {
    match align {
        otlyra_css::VerticalAlign::Baseline => 0.0,
        // These five are not a shift the box knows on its own: they are a place
        // in a line whose height is not known until the boxes that *do* know
        // have been levelled. `level_line_heights` resolves them in a second
        // pass and records the answer; zero here is what a box sits at until
        // then, and what it keeps if it is never levelled at all.
        otlyra_css::VerticalAlign::Top
        | otlyra_css::VerticalAlign::Bottom
        | otlyra_css::VerticalAlign::Middle
        | otlyra_css::VerticalAlign::TextTop
        | otlyra_css::VerticalAlign::TextBottom => 0.0,
        otlyra_css::VerticalAlign::Super => parent.font_size / 3.0 + 1.0,
        otlyra_css::VerticalAlign::Sub => -(parent.font_size / 5.0 + 1.0),
        // A percentage is of the box's own line height, which is the one place a
        // percentage in CSS is not of the containing block.
        otlyra_css::VerticalAlign::Shift(length) => length.resolve(
            style
                .line_height
                .resolve(style.font_size, style.font_size * 1.2),
        ),
    }
}

/// How far `source`'s content is raised off the baseline of a line of `block`'s,
/// in CSS pixels, positive up.
///
/// Settled by [`Flow::level_line_heights`] for the values that need the line box
/// or the parent's font, and worked out here for the rest — with the same answer
/// to which `vertical-align` applies, so the block's own text stays on the
/// baseline its line was built round.
pub(super) fn shift_on_line(
    shifts: &HashMap<BoxId, f32>,
    tree: &BoxTree,
    source: BoxId,
    block: BoxId,
) -> f32 {
    if let Some(&shift) = shifts.get(&source) {
        return shift;
    }
    let style = &tree.node(source).style;
    baseline_shift_of(
        alignment_on_line(source, block, style),
        style,
        &tree.node(block).style,
    )
}

/// How far a paragraph and the things in it reach above and below the
/// baseline, as far as that can be known before the paragraph is broken into
/// lines.
///
/// The shaper is told how tall a line is but decides for itself where inside it
/// the baseline sits, by centring the font. CSS does not: a line reaches as far
/// above its baseline as its tallest thing does, and as far below as its
/// deepest. So the line boxes are rebuilt from these once the shaper has said
/// what landed on each line.
#[derive(Default)]
pub(super) struct Levels {
    /// The block's own strut, above and below the baseline: the line it would
    /// have with nothing in it, and what every line of the paragraph is at least.
    pub(super) strut: (f32, f32),
    /// How far each span reaches above and below the baseline, in step with the
    /// spans.
    ///
    /// The shaper carries a line height per *run* of glyphs and opens a run when
    /// the font changes, so it cannot be told that one span of the same font
    /// wants a taller line; and even where it can, what it is told is a height
    /// rather than where inside it the baseline goes. Both are settled per line,
    /// from these, once it is known which span landed on which.
    pub(super) span_reach: Vec<(f32, f32)>,
    /// What each line-relative `vertical-align` resolved to, by the box it is
    /// written on.
    ///
    /// `top`, `bottom`, `middle`, `text-top` and `text-bottom` are a position
    /// within a line rather than a shift a box knows on its own, so they are
    /// settled here and read back when the glyphs are placed. Working them out
    /// twice would be two answers to where a box sits.
    pub(super) shifts: HashMap<BoxId, f32>,
    /// How far each span is raised off the line's baseline, in step with the
    /// spans: the sum of its own shift and every enclosing inline box's.
    pub(super) span_shift: Vec<f32>,
    /// The spans set `top` or `bottom`, with their struts: a place in the line
    /// they land on, settled line by line once the rest of it is levelled.
    pub(super) line_relative: Vec<LineRelative>,
}

/// A span whose `vertical-align` is `top` or `bottom`.
pub(super) struct LineRelative {
    pub(super) span: usize,
    pub(super) top: bool,
    pub(super) strut: otlyra_text::Strut,
}

/// What levelling a paragraph has worked out so far about the boxes its text
/// is in: each one's strut, and how far it sits off the block's baseline.
struct Frames {
    block: BoxId,
    struts: HashMap<BoxId, otlyra_text::Strut>,
    totals: HashMap<BoxId, f32>,
}

impl<'a> Flow<'a> {
    /// How far the paragraph and each span in it reach, and where the spans a
    /// line-relative `vertical-align` moves end up.
    ///
    /// It also floors every span's line height at the block's strut, for the
    /// shaper's sake. The shaper closes a run of glyphs *after* it has already
    /// moved on to the next span's style, so a run is measured with its
    /// neighbour's line height rather than its own — which made a paragraph of
    /// ordinary text with one `<code>` in it two pixels short on every line,
    /// including the lines the `<code>` is nowhere near. The strut is what every
    /// line is at least anyway (CSS 2.2 §10.8.1), so the floor cannot be got
    /// wrong that way; how much taller than it each line is, is settled from what
    /// this returns once the shaper has said what landed on the line.
    pub(super) fn level_line_heights(
        &mut self,
        parent: BoxId,
        content: &mut InlineContent<'_>,
    ) -> Levels {
        let style = Arc::clone(&self.tree.node(parent).style);
        let stack = self.font_stack(&style);
        // The strut: the line the block would have with no text in it at all.
        let Some(strut) = self.strut_of(&style, &stack) else {
            return Levels::default();
        };

        // The two ends grow apart: a raised box reaches further above the baseline
        // by however far it moved plus its own ascent, and a lowered one further
        // below. A box on the baseline is already inside the strut wherever the
        // block's font is the larger, which is the ordinary case.
        let (mut above, mut below) = (strut.ascent, strut.descent);
        let mut span_reach = vec![(0.0, 0.0); content.spans.len()];
        // Kept from the first pass so the second does not shape anything twice:
        // a strut is a font lookup, and the line-relative boxes need theirs
        // again once the line is known.
        let mut line_relative: Vec<(usize, BoxId, otlyra_css::VerticalAlign, otlyra_text::Strut)> =
            Vec::new();
        let mut shifts = HashMap::new();
        let mut span_shift = vec![0.0; content.spans.len()];
        let mut frames = Frames {
            block: parent,
            struts: HashMap::from([(parent, strut)]),
            totals: HashMap::from([(parent, 0.0)]),
        };

        for (index, (span, source)) in content.spans.iter().zip(&content.sources).enumerate() {
            let span_style = Arc::clone(&self.tree.node(*source).style);
            let own = match self.strut_of(&span_style, &span.font_stack) {
                Some(own) => own,
                None => continue,
            };
            let align = alignment_on_line(*source, parent, &span_style);

            // `top` and `bottom` are the only two that need the line box, and
            // they are a position *within* it: the line does not grow to fit
            // them, it is what they are measured against. Everything else —
            // including `text-top`, `text-bottom` and `middle`, which are
            // measured against the parent's own font — is a shift the box knows
            // here, and the line grows to hold it like any other.
            if matches!(
                align,
                otlyra_css::VerticalAlign::Top | otlyra_css::VerticalAlign::Bottom
            ) {
                // Its own height still asks for room; where it goes does not
                // depend on that, but how tall the line is does — the line it
                // lands on, which is settled once the rest of it is.
                above = above.max(own.ascent);
                below = below.max(own.descent);
                line_relative.push((index, *source, align.clone(), own));
                continue;
            }

            let shift = self.total_shift(*source, &mut frames);
            span_shift[index] = shift;
            above = above.max(shift + own.ascent);
            below = below.max(own.descent - shift);
            // A span that has been moved is one the shaper cannot place: it knows
            // the span's own height and nothing of the shift, so the room a shift
            // needs is carried here, to the line the span lands on.
            span_reach[index] = (shift + own.ascent, own.descent - shift);
            // An explicit `line-height` on a span still asks for its own room.
            if let Some(asked) = span.line_height {
                above = above.max(asked - strut.descent);
                // And where the shaper cannot carry it, the paragraph must. A line
                // height belongs to a *run* of glyphs, and a run is opened when the
                // font changes — so a span that differs from the text around it
                // only in `line-height` shares their run and its own height is lost
                // on the way through. Those, and only those, are folded into the
                // floor: folding in the rest would make a paragraph with one larger
                // word in it that tall on every line.
                let (reach_above, reach_below) = &mut span_reach[index];
                *reach_above = reach_above.max(asked - own.descent);
                *reach_below = reach_below.max(own.descent);
            }
        }

        // The second pass, for the two that had to wait: the line box is settled
        // now, so there is something for them to be a position in.
        //
        // Every other box's place is its shift added up through the boxes it is
        // in, which placing the glyphs and the boxes reads back.
        shifts.extend(frames.totals.iter().map(|(&id, &total)| (id, total)));
        //
        // The boxes those inline boxes draw — a background, a border — are
        // placed by the paragraph's line; their text by its own line.
        let mut spans_at_edges = Vec::new();
        for (index, source, align, own) in line_relative {
            let top = matches!(align, otlyra_css::VerticalAlign::Top);
            let shift = if top {
                above - own.ascent
            } else {
                own.descent - below
            };
            shifts.insert(source, shift);
            span_shift[index] = shift;
            spans_at_edges.push(LineRelative {
                span: index,
                top,
                strut: own,
            });
        }

        // A picture or an inline block moves with the boxes it is inside, as
        // text does (CSS 2.2 §10.8.1: a box is aligned against its parent).
        let replaced: Vec<(usize, BoxId)> = content
            .replaced
            .iter()
            .enumerate()
            .map(|(number, box_)| (number, box_.id))
            .collect();
        for (number, id) in replaced {
            let align = self.atomic_align(id, &content.replaced[number], &mut frames);
            content.replaced[number].align = align;
        }

        // The shaper is still told a height per span, because that is what it
        // measures a line by while it is breaking one. Where each line's box ends
        // up is settled afterwards, from what landed on it.
        let floor = strut.height();
        if floor.is_finite() && floor > 0.0 {
            for span in &mut content.spans {
                span.line_height = Some(span.line_height.map_or(floor, |own| own.max(floor)));
            }
        }

        Levels {
            // What *every* line of the paragraph is at least, and no more than
            // that: the block's own strut. Everything else — a taller span, a
            // raised one, a box — belongs to the line it is actually on and is
            // folded in there.
            strut: (strut.ascent, strut.descent + strut.leading),
            span_reach,
            shifts,
            span_shift,
            line_relative: spans_at_edges,
        }
    }

    /// Where an atomic inline goes in its line: `top` and `bottom` are a place
    /// in the line, settled once it is known; everything else is a shift off
    /// the baseline, added to the shift of the boxes it is inside.
    ///
    /// `middle` puts the box's middle half an x-height above its parent's
    /// baseline, and `text-top` and `text-bottom` its edges on the parent's
    /// text — the font's own ascent and descent, without the leading the line
    /// height adds (CSS 2.2 §10.8.1).
    fn atomic_align(
        &mut self,
        id: BoxId,
        box_: &super::collect::ReplacedBox,
        frames: &mut Frames,
    ) -> super::collect::LineAlign {
        use super::collect::LineAlign;
        use otlyra_css::VerticalAlign as Align;

        let Some(up) = self.tree.node(id).parent else {
            return LineAlign::Shift(0.0);
        };
        let style = Arc::clone(&box_.style);
        let parent = Arc::clone(&self.tree.node(up).style);
        let outer = box_.outer_height();
        let own = match &style.vertical_align {
            Align::Top => return LineAlign::Top,
            Align::Bottom => return LineAlign::Bottom,
            Align::Middle => parent.font_size * 0.25 + outer / 2.0 - box_.baseline,
            edge @ (Align::TextTop | Align::TextBottom) => {
                let stack = self.font_stack(&parent);
                let Some(text) =
                    self.text
                        .strut(&stack, parent.font_size, crate::fonts::face_query(&parent))
                else {
                    return LineAlign::Shift(0.0);
                };
                if matches!(edge, Align::TextTop) {
                    text.ascent - box_.baseline
                } else {
                    outer - box_.baseline - text.descent
                }
            }
            other => baseline_shift_of(other, &style, &parent),
        };
        LineAlign::Shift(self.total_shift(up, frames) + own)
    }

    /// How far `id`'s content sits off the block's baseline: its own shift
    /// against its parent's baseline, plus its parent's (CSS 2.2 §10.8.1 — a
    /// box is aligned relative to its parent inline box). A `sup` holding a
    /// link holding a `span` raises all three.
    ///
    /// `top` and `bottom` on an enclosing box add nothing here: they are a
    /// place in the line, settled only for the box a run of text comes from.
    fn total_shift(&mut self, id: BoxId, frames: &mut Frames) -> f32 {
        if let Some(&total) = frames.totals.get(&id) {
            return total;
        }
        let Some(up) = self.tree.node(id).parent else {
            return 0.0;
        };
        let above = self.total_shift(up, frames);
        let own = self.own_shift(id, up, frames);
        frames.totals.insert(id, above + own);
        above + own
    }

    /// How far `id`'s `vertical-align` moves it off its parent `up`'s
    /// baseline.
    fn own_shift(&mut self, id: BoxId, up: BoxId, frames: &mut Frames) -> f32 {
        let style = Arc::clone(&self.tree.node(id).style);
        let parent = Arc::clone(&self.tree.node(up).style);
        let align = alignment_on_line(id, frames.block, &style);
        let needs_struts = matches!(
            align,
            otlyra_css::VerticalAlign::TextTop
                | otlyra_css::VerticalAlign::TextBottom
                | otlyra_css::VerticalAlign::Middle
        );
        if !needs_struts {
            return baseline_shift_of(align, &style, &parent);
        }
        let (Some(own), Some(outer)) = (self.frame_strut(id, frames), self.frame_strut(up, frames))
        else {
            return 0.0;
        };
        match align {
            // The parent's own text rather than the whole line: what
            // `text-top` and `text-bottom` mean is the edge of the text the
            // box is set beside, not the edge of the tallest thing on the row.
            otlyra_css::VerticalAlign::TextTop => outer.ascent - own.ascent,
            otlyra_css::VerticalAlign::TextBottom => own.descent - outer.descent,
            // The box's middle against the parent's baseline plus half its
            // x-height. No font here reports an x-height, so half of it is
            // taken as a quarter of the font size — the same shape of fallback
            // the specification names for `sub` and `super`, and the number the
            // web was built against.
            _ => parent.font_size * 0.25 - (own.ascent - own.descent) / 2.0,
        }
    }

    /// A box's strut, worked out once per paragraph.
    fn frame_strut(&mut self, id: BoxId, frames: &mut Frames) -> Option<otlyra_text::Strut> {
        if let Some(&strut) = frames.struts.get(&id) {
            return Some(strut);
        }
        let style = Arc::clone(&self.tree.node(id).style);
        let stack = self.font_stack(&style);
        let strut = self.strut_of(&style, &stack)?;
        frames.struts.insert(id, strut);
        Some(strut)
    }
}
