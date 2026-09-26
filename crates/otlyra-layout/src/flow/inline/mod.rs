//! Inline layout: a paragraph, and the line boxes it breaks into.
//!
//! Everything in an inline formatting context is gathered into one paragraph of
//! styled spans, shaped in one pass, and turned back into line boxes holding the
//! fragments each line carries. What is not text — the padding and border of an
//! inline box, a picture, an `inline-block` — goes in as a spacer the shaper
//! places among the words.
//!
//! One step to a module, in the order they run:
//!
//! - `collect` gathers the paragraph: its spans, the box each came from, and the
//!   boxes that are not text.
//! - `vertical_align` levels it: how far the block's strut and each span reach
//!   above and below the baseline, and where `vertical-align` moves things.
//! - `lines` shapes it into the room the floats leave, and stacks the lines as
//!   tall as what landed on each.
//! - `place` puts each line on the page, with the fragments it carries.
//! - `measure` asks the same paragraph how wide it is at its widest and at its
//!   narrowest.

mod collect;
mod lines;
mod measure;
mod place;
mod vertical_align;

use std::sync::Arc;

use otlyra_css::ComputedStyle;
use otlyra_text::FontStack;

use crate::box_tree::BoxId;
use crate::fragment::Fragment;

use super::Flow;

pub(super) use collect::span_for;
use lines::{Band, line_reaches, restack};
use place::{LineBox, Placement, line_offset};

impl<'a> Flow<'a> {
    /// An inline formatting context: everything inside becomes one paragraph, and
    /// the paragraph becomes line boxes.
    ///
    /// The whole context is shaped in one pass rather than element by element,
    /// because a line break belongs to the paragraph: `<b>bold</b> text` has to
    /// break where `bold text` breaks.
    pub(super) fn layout_inline(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let mut content = self.collect_inline(parent, width);
        if content.is_phantom() {
            return 0.0;
        }
        let levels = self.level_line_heights(parent, &mut content);
        let (mut shaped, bands) = self.shape_lines(parent, &content, width, x, y);
        let reach = line_reaches(&content, &levels, &shaped);
        restack(&mut shaped, &reach);
        // Where the first line starts, in the shaper's coordinates. parley
        // measures line tops from the text origin, and the first line's top can
        // sit above it by the half-leading; the paragraph's box starts where its
        // first line starts, so every line is rebased onto that.
        let paragraph_top = shaped.lines.first().map_or(0.0, |line| line.top);

        // The marker of the list item this is the first line of, if it is one.
        if let Some(marker) = self
            .pending_marker
            .take_if(|marker| (marker.x - x).abs() < 0.01)
            && let Some(line) = shaped.lines.first()
            && let Some(fragment) = self.marker_fragment(&marker, x, y, line, paragraph_top)
        {
            out.push(fragment);
        }

        let style = Arc::clone(&self.tree.node(parent).style);
        let placement = Placement {
            tree: self.tree,
            parent,
            content: &content,
            shaped: &shaped,
            shifts: &levels.shifts,
            spacers: shaped
                .spacers
                .iter()
                .map(|spacer| (spacer.id, *spacer))
                .collect(),
            width,
        };
        for (index, metrics) in shaped.lines.iter().enumerate() {
            let band = bands
                .get(index)
                .copied()
                .unwrap_or(Band { start: 0.0, width });
            let line = LineBox {
                index,
                metrics,
                x: x + band.start + line_offset(&style, metrics, band),
                y: y + metrics.top - paragraph_top,
                // Line boxes are contiguous: each one ends where the next
                // begins. Taking the height from the next line's top rather than
                // from the font's line height keeps them so, and avoids the
                // fraction of a pixel of overlap that leading otherwise leaves
                // between them.
                height: shaped
                    .lines
                    .get(index + 1)
                    .map_or(metrics.bottom - metrics.top, |next| next.top - metrics.top),
            };
            out.push(placement.line_fragment(&line));
        }

        shaped.metrics.height
    }

    /// The font stack a style's text is set in.
    pub(super) fn font_stack(&mut self, style: &ComputedStyle) -> FontStack {
        self.font_stacks.of(&style.font_family)
    }

    /// The strut of one style: how far its font reaches above and below.
    pub(super) fn strut_of(
        &mut self,
        style: &ComputedStyle,
        stack: &FontStack,
    ) -> Option<otlyra_text::Strut> {
        let mut strut = self
            .text
            .strut(stack, style.font_size, style.font_weight, false)?;
        // An explicit `line-height` replaces what the font asked for, split evenly
        // above and below the baseline, which is what half-leading is.
        if let otlyra_css::LineHeight::Normal = style.line_height {
            return Some(strut);
        }
        let asked = style.line_height.resolve(style.font_size, strut.height());
        let half = (asked - strut.ascent - strut.descent) / 2.0;
        strut.ascent += half;
        strut.descent += half;
        strut.leading = 0.0;
        Some(strut)
    }
}

#[cfg(test)]
mod tests;
