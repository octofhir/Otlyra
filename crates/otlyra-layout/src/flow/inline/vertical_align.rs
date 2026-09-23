//! Where things sit in a line: `vertical-align`, baselines, and how tall a line
//! has to be.
//!
//! The shaper decides where the words go along a line and nothing about how far
//! a raised span or an inline block reaches above and below it. That is worked
//! out here: how far a rule moves a box, where an inline block's own baseline
//! is, and — before the paragraph is shaped — how far each of its spans reaches,
//! which is what the line boxes are rebuilt from once it has been.

use std::sync::Arc;

use otlyra_css::ComputedStyle;
use otlyra_text::TextSpan;

use crate::box_tree::BoxId;
use crate::flow::Flow;
use crate::fragment::{Fragment, FragmentKind};

use super::ReplacedBox;

/// How far a rule has raised the box it is written on, in CSS pixels.
///
/// Positive is up. The two keywords are measured against the *parent's* font size
/// rather than the box's own, which is what makes a superscript sit at the same
/// height whether it is set small or not.
///
/// A third of the font size, and a fifth of it, are what the specification names as
/// the amounts to use when a UA does not take them from the font — plus the pixel
/// every engine adds on top, which is the amount the web was actually built
/// against.
pub(super) fn baseline_shift(style: &ComputedStyle, parent: &ComputedStyle) -> f32 {
    baseline_shift_of(&style.vertical_align, style, parent)
}

/// The same, for a value the caller has already picked out.
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

/// How far below a box's top its last baseline sits, if it has one.
///
/// The last line of text in it, wherever that is: a box whose last child is a
/// paragraph sits on that paragraph's last line, which is what `inline-block`
/// alignment is defined as. `None` when there is no text in it at all.
pub(super) fn baseline_of(fragment: &Fragment) -> Option<f32> {
    let mut last = None;
    let mut stack = vec![(fragment, 0.0f32)];
    while let Some((current, _)) = stack.pop() {
        if let FragmentKind::Text(run) = &current.kind
            && let Some(glyph) = run.glyphs.first()
        {
            let at = current.rect.y + glyph.y - fragment.rect.y;
            last = Some(last.map_or(at, |previous: f32| previous.max(at)));
        }
        for child in &current.children {
            // A box that has left the flow has left the line as well: its text is
            // not on the line and its baseline is not the line's. Descending into
            // one makes a drop-down's open list part of the line the drop-down sits
            // on, and the line as tall as the list.
            if child.style.position.is_out_of_flow() {
                continue;
            }
            stack.push((child, 0.0));
        }
    }
    last
}

impl<'a> Flow<'a> {
    /// Give every span of a paragraph the same line height: the tallest any of
    /// them, or the block itself, asks for — and enough room for anything a rule
    /// has raised or lowered.
    ///
    /// CSS is finer than this. A line box is as tall as the tallest thing *on that
    /// line*, and the block's own font sets a floor — the strut — that a line has
    /// even when nothing on it is that tall. So a paragraph whose middle line holds
    /// one large word should have one tall line and the rest short.
    ///
    /// Levelling them is a workaround, and it is worth stating what for. The shaper
    /// closes a run of glyphs *after* it has already moved on to the next span's
    /// style, so a run is measured with its neighbour's line height rather than its
    /// own — which makes a paragraph of ordinary text with one `<code>` in it two
    /// pixels short on every line, including the lines the `<code>` is nowhere
    /// near. One height throughout cannot be got wrong that way, and for the shape
    /// this actually happens in — a smaller inline inside ordinary prose — the
    /// floor is the answer CSS gives anyway. What it gets wrong is the opposite
    /// case: a paragraph with one larger inline in it is tall on every line rather
    /// than on the line that holds it.
    pub(super) fn level_line_heights(
        &mut self,
        parent: BoxId,
        spans: &mut [TextSpan<'_>],
        sources: &[BoxId],
        replaced: &[ReplacedBox],
    ) {
        let style = Arc::clone(&self.tree.node(parent).style);
        let stack = self.font_stack(&style);
        // The strut: the line the block would have with no text in it at all.
        let Some(strut) = self.strut_of(&style, &stack) else {
            return;
        };

        // The two ends grow apart: a raised box reaches further above the baseline
        // by however far it moved plus its own ascent, and a lowered one further
        // below. A box on the baseline is already inside the strut wherever the
        // block's font is the larger, which is the ordinary case.
        let (mut above, mut below) = (strut.ascent, strut.descent);
        // The same reach with the *unshifted* spans left out.
        //
        // Every line of the paragraph is at least this tall, and no line is made
        // tall by a span that is not on it. A span sitting on the baseline needs
        // nothing from the paragraph — the shaper knows its own height and which
        // line it landed on — but one that has been raised, lowered or hung off
        // the line box does: how far it moved is worked out here, and the shaper
        // never hears about it.
        let (mut floor_above, mut floor_below) = (strut.ascent, strut.descent);
        self.span_reach.clear();
        self.span_reach.resize(spans.len(), (0.0, 0.0));
        // Kept from the first pass so the second does not shape anything twice:
        // a strut is a font lookup, and the line-relative boxes need theirs
        // again once the line is known.
        let mut line_relative: Vec<(BoxId, otlyra_css::VerticalAlign, otlyra_text::Strut)> =
            Vec::new();
        self.line_shifts.clear();

        for (index, span) in spans.iter().enumerate() {
            let Some(source) = sources.get(index) else {
                continue;
            };
            let span_style = Arc::clone(&self.tree.node(*source).style);
            let own = match self.strut_of(&span_style, &span.font_stack.clone()) {
                Some(own) => own,
                None => continue,
            };

            // `top` and `bottom` are the only two that need the line box, and
            // they are a position *within* it: the line does not grow to fit
            // them, it is what they are measured against. Everything else —
            // including `text-top`, `text-bottom` and `middle`, which are
            // measured against the parent's own font — is a shift the box knows
            // here, and the line grows to hold it like any other.
            if matches!(
                span_style.vertical_align,
                otlyra_css::VerticalAlign::Top | otlyra_css::VerticalAlign::Bottom
            ) {
                // Its own height still asks for room; where it goes does not
                // depend on that, but how tall the line is does.
                above = above.max(own.ascent);
                below = below.max(own.descent);
                floor_above = floor_above.max(own.ascent);
                floor_below = floor_below.max(own.descent);
                self.span_reach[index] = (own.ascent, own.descent);
                line_relative.push((*source, span_style.vertical_align.clone(), own));
                continue;
            }

            let shift = match &span_style.vertical_align {
                // The parent's own text rather than the whole line: what
                // `text-top` and `text-bottom` mean is the edge of the text the
                // box is set beside, not the edge of the tallest thing on the row.
                otlyra_css::VerticalAlign::TextTop => strut.ascent - own.ascent,
                otlyra_css::VerticalAlign::TextBottom => own.descent - strut.descent,
                // The box's middle against the parent's baseline plus half its
                // x-height. No font here reports an x-height, so half of it is
                // taken as a quarter of the font size — the same shape of
                // fallback the specification names for `sub` and `super`, and the
                // number the web was built against.
                otlyra_css::VerticalAlign::Middle => {
                    style.font_size * 0.25 - (own.ascent - own.descent) / 2.0
                }
                other => baseline_shift_of(other, &span_style, &style),
            };
            if span_style.vertical_align.resolved_while_levelling() {
                self.line_shifts.insert(*source, shift);
            }
            above = above.max(shift + own.ascent);
            below = below.max(own.descent - shift);
            self.span_reach[index] = (shift + own.ascent, own.descent - shift);
            // A span that has been moved is one the shaper cannot place: it knows
            // the span's own height and nothing of the shift, so the room a shift
            // needs comes from the paragraph. A span sitting where the shaper put
            // it asks nothing of the floor and is left to its own line.
            if shift != 0.0 {
                floor_above = floor_above.max(shift + own.ascent);
                floor_below = floor_below.max(own.descent - shift);
            }
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
                let (reach_above, reach_below) = &mut self.span_reach[index];
                *reach_above = reach_above.max(asked - own.descent);
                *reach_below = reach_below.max(own.descent);
            }
        }

        // The second pass, for the two that had to wait: the line box is settled
        // now, so there is something for them to be a position in.
        for (source, align, own) in line_relative {
            let shift = match align {
                otlyra_css::VerticalAlign::Top => above - own.ascent,
                otlyra_css::VerticalAlign::Bottom => own.descent - below,
                _ => 0.0,
            };
            self.line_shifts.insert(source, shift);
        }

        // A picture is deliberately *not* folded in. It sits with its bottom edge on
        // the baseline, so all of it is above — and one large picture would make
        // every line of the paragraph as tall as itself, which for a picture beside
        // a sentence is far more wrong than the pixel it saves. The shaper reserves
        // the room for it within the line it is actually on.
        //
        // An inline block *is* folded in, both ends of it: it is a box in the line
        // like a tall word, sitting on its own last baseline, and CSS grows the
        // line to hold it above and below rather than letting it hang out.
        for box_ in replaced.iter().filter(|box_| box_.content.is_some()) {
            above = above.max(box_.baseline);
            below = below.max(box_.height - box_.baseline);
            floor_above = floor_above.max(box_.baseline);
            floor_below = floor_below.max(box_.height - box_.baseline);
        }

        // What *every* line of the paragraph is at least, and no more than that:
        // the block's own strut, which is the line it would have with nothing in
        // it. Everything else — a taller span, a raised one, a box — belongs to the
        // line it is actually on and is folded in there.
        let _ = (above, below, floor_above, floor_below);
        self.line_reach = (strut.ascent, strut.descent + strut.leading);

        // The shaper is still told a height per span, because that is what it
        // measures a line by while it is breaking one. Where each line's box ends
        // up is settled afterwards, from what landed on it.
        let floor = strut.height();
        if floor.is_finite() && floor > 0.0 {
            for span in spans {
                span.line_height = Some(span.line_height.map_or(floor, |own| own.max(floor)));
            }
        }
    }
}
