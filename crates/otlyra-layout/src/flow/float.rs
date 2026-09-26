//! Floats: boxes taken out of the flow and put against an edge.
//!
//! A float is laid out like a block and then moved, and what it leaves behind is
//! a rectangle the lines beside it keep clear of. Both halves — placing one, and
//! asking how much room the ones already placed have left — are asked by more
//! than one context: a block places floats and clears them, and a paragraph
//! shortens its lines around them.

use std::sync::Arc;

use otlyra_css::{Clear, Float, Size};

use crate::box_tree::BoxId;
use crate::fragment::{Fragment, Layer, Rect};

use super::box_model::resolve_margin;
use super::sizing::{Frame, InlineRoom};
use super::{Flow, offset};

/// A box taken out of the flow and put against an edge.
#[derive(Copy, Clone, Debug)]
pub(super) struct FloatBox {
    /// Which edge it went to.
    side: Float,
    /// Its margin box, which is what lines and other floats keep clear of.
    pub(super) rect: Rect,
}

impl FloatBox {
    /// Whether it sits beside any of a band from `top` down to `bottom`.
    fn beside(&self, top: f32, bottom: f32) -> bool {
        self.rect.bottom() > top && self.rect.y < bottom
    }
}

/// The horizontal space `floats` leave free between `left` and `right`, for a band
/// from `top` down `height` pixels.
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
        if !float.beside(top, bottom) {
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

/// The room `floats` leave one line of a paragraph whose content box is `width`
/// wide at `x`, for a band from `top` down `height` pixels: how far in from `x`
/// the room starts, and how wide it is.
///
/// Worked out in the paragraph's own coordinates, so that a line with no float
/// beside it has exactly `width` to fill. Rebuilt as `(x + width) - x` in page
/// coordinates, the width can come back an ulp short wherever `x + width` lands
/// in a higher binade than `width`, and a float or a table cell sized to its own
/// max-content would then wrap its last word — which is the one thing a box at
/// that size must not do (CSS Sizing 3 §5.1; CSS 2.2 §10.3.5).
///
/// A free function rather than a method, because a line asks this while the
/// shaper holds the engine.
pub(super) fn line_room(
    floats: &[FloatBox],
    top: f32,
    height: f32,
    x: f32,
    width: f32,
) -> (f32, f32) {
    let bottom = top + height;
    // Each float edge moved into the paragraph's coordinates and kept inside
    // its content box: a float further out than the box takes nothing from it.
    let inside = |edge: f32| (edge - x).min(width).max(0.0);
    let (mut start, mut end) = (0.0f32, width);
    for float in floats.iter().filter(|float| float.beside(top, bottom)) {
        match float.side {
            Float::Left => start = start.max(inside(float.rect.right())),
            Float::Right => end = end.min(inside(float.rect.x)),
            Float::None => {}
        }
    }
    (start, (end - start).max(0.0))
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
        // the whole column, which is the one thing a float must not do. One that
        // names a width — a length or a keyword alike — is laid out as a block of
        // that width.
        let mut fragment = match &style.width {
            Size::Auto => {
                let margin = resolve_margin(&style, containing_width);
                let frame = Frame::of(&style, containing_width).inline;
                let room = InlineRoom::within(&style, containing_width);
                let width = self.shrink_to_fit_width(id, &style, room, frame);
                self.layout_sized(id, x + margin.left, y, width)
            }
            Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => {
                self.layout_block(id, containing_width, x, y)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A line with no float beside it has exactly the width of its paragraph,
    /// wherever the paragraph is: here `x + width` crosses 128, where adding
    /// `x` and taking it away again loses the width's last bit.
    #[test]
    fn a_line_with_no_float_beside_it_has_exactly_its_width() {
        let width = 101.371_91_f32;
        let x = 30.0;
        assert_ne!((x + width) - x, width, "the case the room is for");

        let above = FloatBox {
            side: Float::Left,
            rect: Rect::new(0.0, 0.0, 60.0, 10.0),
        };
        assert_eq!(line_room(&[], 0.0, 1.0, x, width), (0.0, width));
        assert_eq!(line_room(&[above], 10.0, 1.0, x, width), (0.0, width));
    }

    /// A float beside the line takes what it covers of the paragraph, from its
    /// own edge, and no more than the paragraph has.
    #[test]
    fn a_float_beside_a_line_takes_its_edge_of_the_room() {
        let left = FloatBox {
            side: Float::Left,
            rect: Rect::new(0.0, 0.0, 50.0, 20.0),
        };
        let right = FloatBox {
            side: Float::Right,
            rect: Rect::new(170.0, 0.0, 30.0, 20.0),
        };
        assert_eq!(
            line_room(&[left, right], 5.0, 1.0, 20.0, 180.0),
            (30.0, 120.0)
        );

        let wide = FloatBox {
            side: Float::Left,
            rect: Rect::new(0.0, 0.0, 400.0, 20.0),
        };
        assert_eq!(line_room(&[wide], 5.0, 1.0, 20.0, 180.0), (180.0, 0.0));
    }
}
