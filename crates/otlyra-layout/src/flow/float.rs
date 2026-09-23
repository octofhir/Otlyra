//! Floats: boxes taken out of the flow and put against an edge.
//!
//! A float is laid out like a block and then moved, and what it leaves behind is
//! a rectangle the lines beside it keep clear of. Both halves — placing one, and
//! asking how much room the ones already placed have left — are asked by more
//! than one context: a block places floats and clears them, and a paragraph
//! shortens its lines around them.

use std::sync::Arc;

use otlyra_css::{Clear, Float};

use crate::box_tree::BoxId;
use crate::fragment::{Fragment, Layer, Rect};

use super::box_model::{clamp, resolve_margin};
use super::{Flow, offset};

/// A box taken out of the flow and put against an edge.
#[derive(Copy, Clone, Debug)]
pub(super) struct FloatBox {
    /// Which edge it went to.
    side: Float,
    /// Its margin box, which is what lines and other floats keep clear of.
    pub(super) rect: Rect,
}

/// The horizontal space `floats` leave free between `left` and `right`, for a band
/// from `top` down `height` pixels.
///
/// A free function rather than a method, because a line asks this while the shaper
/// holds the engine.
pub(super) fn band_of(
    floats: &[FloatBox],
    top: f32,
    height: f32,
    left: f32,
    right: f32,
) -> (f32, f32) {
    let bottom = top + height;
    let mut from = left;
    let mut to = right;

    for float in floats {
        if float.rect.bottom() <= top || float.rect.y >= bottom {
            continue;
        }
        match float.side {
            Float::Left => from = from.max(float.rect.right()),
            Float::Right => to = to.min(float.rect.x),
            Float::None => {}
        }
    }
    (from, to.max(from))
}

impl<'a> Flow<'a> {
    /// Place a floated box against its edge, at or below `y`.
    ///
    /// It is laid out like any other block and then moved: to the near edge if it
    /// fits beside what is already there, and down past the floats in the way if it
    /// does not.
    pub(super) fn layout_float(
        &mut self,
        id: BoxId,
        containing_width: f32,
        x: f32,
        y: f32,
    ) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A float establishes a formatting context of its own: the floats outside
        // it do not shorten the lines inside it, and the ones inside it do not
        // reach out. Hiding the list for the duration is the whole of that rule.
        let outer_floats = std::mem::take(&mut self.floats);

        // A float with no width of its own shrinks to fit: as wide as its content
        // wants, and never wider than what is left for it. A block would have taken
        // the whole column, which is the one thing a float must not do.
        let mut fragment = match style.width.resolve(containing_width) {
            Some(_) => self.layout_block(id, containing_width, x, y),
            None => {
                let margin = resolve_margin(&style, containing_width);
                let available = (containing_width - margin.left - margin.right).max(0.0);
                let content = self.max_content_width(id, containing_width);
                let floor = self.min_content_width(id, containing_width, true);
                let width = clamp(
                    content.clamp(floor.min(available), available),
                    style.min_width,
                    style.max_width,
                    containing_width,
                );
                self.layout_sized(id, x + margin.left, y, width)
            }
        };
        self.floats = outer_floats;
        // Above every in-flow block, which is where CSS paints a float and what
        // keeps it from disappearing under the background of a paragraph that
        // happens to be written after it.
        fragment.layer = Layer::floated();

        let margin = resolve_margin(&style, containing_width);
        let outer = fragment.rect.width + margin.left + margin.right;

        // Down until there is room. A float never overlaps another one, so the
        // first band with space enough is where it goes.
        let mut top = self.clearance(
            match style.clear {
                Clear::None => Clear::None,
                other => other,
            },
            y,
        );
        let height = fragment.rect.height.max(1.0);
        let left_edge = x;
        let right_edge = x + containing_width;

        loop {
            let (from, to) = self.band(top, height, left_edge, right_edge);
            if to - from >= outer || !self.floats.iter().any(|float| float.rect.height > 0.0) {
                let placed_x = match style.float {
                    Float::Right => to - outer + margin.left,
                    _ => from + margin.left,
                };
                let delta_x = placed_x - fragment.rect.x;
                let delta_y = top - fragment.rect.y;
                offset(&mut fragment, delta_x, delta_y);
                break;
            }

            // The next band starts at the bottom of the nearest float in the way.
            let Some(next) = self
                .floats
                .iter()
                .map(|float| float.rect.bottom())
                .filter(|bottom| *bottom > top)
                .min_by(f32::total_cmp)
            else {
                break;
            };
            top = next;
        }

        self.floats.push(FloatBox {
            side: style.float,
            rect: Rect::new(
                fragment.rect.x - margin.left,
                fragment.rect.y,
                outer,
                fragment.rect.height + margin.top + margin.bottom,
            ),
        });
        fragment
    }

    /// The horizontal space free of floats between `left` and `right`, for a band
    /// from `top` down `height` pixels.
    fn band(&self, top: f32, height: f32, left: f32, right: f32) -> (f32, f32) {
        band_of(&self.floats, top, height, left, right)
    }

    /// The lowest `y` a box with this `clear` may start at.
    pub(super) fn clearance(&self, clear: Clear, y: f32) -> f32 {
        self.floats
            .iter()
            .filter(|float| match clear {
                Clear::Both => true,
                Clear::Left => float.side == Float::Left,
                Clear::Right => float.side == Float::Right,
                Clear::None => false,
            })
            .map(|float| float.rect.bottom())
            .fold(y, f32::max)
    }
}
