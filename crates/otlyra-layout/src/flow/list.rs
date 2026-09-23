//! List items: the marker that hangs outside the first line.
//!
//! A marker is known as soon as its item is, and cannot be placed until the
//! item's first line has been shaped — which may be several boxes further down.
//! So it waits between the two, and this is the waiting and the placing.

use std::sync::Arc;

use otlyra_css::ComputedStyle;

use crate::fragment::{Fragment, FragmentKind, Rect};

use super::Flow;
use super::inline::span_for;

/// A list item's marker, waiting for the item's first line.
///
/// Where it goes horizontally is known as soon as the item's content edge is —
/// outside it, to the left — but where it goes vertically is the first line's
/// baseline, and nothing knows that until the line has been shaped.
pub(super) struct PendingMarker {
    pub(super) marker: crate::box_tree::Marker,
    /// The item's own style: a marker is set in its item's font and colour.
    pub(super) style: Arc<ComputedStyle>,
    /// The item's content edge, which is what identifies the line it belongs to.
    pub(super) x: f32,
}

impl<'a> Flow<'a> {
    /// A list item's marker, shaped and placed against the item's first line.
    ///
    /// Outside the content box, which is what `list-style-position: outside` means
    /// and is the whole point of not making it a child: the marker hangs to the
    /// left, and the item's text — including the second line of a long item —
    /// starts at the content edge. Put inside, it pushes the first line right and
    /// the rest of them line up under the marker instead of under the words.
    ///
    /// Where it starts is two rules at once, and both are visible the moment
    /// either is missing. It **ends** one space before the content edge, so the
    /// numbers of a list line up on their full stops however many digits they have
    /// — `i`, `ii` and `iii` all end in the same column. And it **starts** at least
    /// a full em back, so a bullet, which needs far less room than that, still
    /// hangs where a reader expects rather than crowding the word beside it.
    pub(super) fn marker_fragment(
        &mut self,
        marker: &PendingMarker,
        x: f32,
        y: f32,
        line: &otlyra_text::LineMetrics,
        paragraph_top: f32,
    ) -> Option<Fragment> {
        let stack = self.font_stack(&marker.style);
        let mut measure = |text: &str| {
            let mut span = span_for(text, &marker.style, stack.clone());
            // Whatever the item's first line does, the marker is one line of its own.
            span.line_height = None;
            self.text.shape_spans(&[span], &[], None)
        };
        let shaped = measure(&marker.marker.text);
        // The gap is a space set in the item's own font, which is what CSS puts
        // after a counter — measured with the space *leading*, because a shaper
        // drops a trailing one from the width it reports.
        let gap = (measure(&format!(" {}", marker.marker.text)).metrics.width
            - shaped.metrics.width)
            .max(0.0);

        let mut run = shaped.runs.into_iter().next()?;
        // A counter ends against the item's words; a bullet hangs an em back,
        // which is further than its own narrow width would put it.
        let left = if marker.marker.bullet {
            x - marker.style.font_size
        } else {
            x - gap - shaped.metrics.width
        };
        let room = x - left;
        // On the item's baseline rather than its own: a marker that sat on its own
        // baseline would ride up and down with whatever the first line happens to
        // contain.
        let baseline = line.baseline - paragraph_top;
        for glyph in &mut run.glyphs {
            glyph.y = baseline;
        }

        Some(Fragment::new(
            None,
            Rect::new(left, y, room, line.height),
            FragmentKind::Text(run),
            Arc::clone(&marker.style),
        ))
    }
}
