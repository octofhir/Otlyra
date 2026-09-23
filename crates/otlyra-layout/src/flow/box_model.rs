//! The box model: margins, borders and padding, and how margins meet.
//!
//! Every formatting context asks the same questions of a box before it can place
//! it — how much room its frame takes, what an `auto` margin comes to, and how
//! margins meet — and the answers have to be the same whichever context asked.
//! So they are worked out here, once, rather than in whichever context happened
//! to need them first. What its sizing properties ask of it is the same kind of
//! question, with enough to it to be answered in `sizing`.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, LengthOrAuto, Sides};

use crate::box_tree::BoxId;

use super::Flow;
use super::sizing::Sizes;

/// Whether any of the four sides is non-zero.
pub(super) fn any_side(sides: Sides<f32>) -> bool {
    sides.top > 0.0 || sides.right > 0.0 || sides.bottom > 0.0 || sides.left > 0.0
}

impl<'a> Flow<'a> {
    /// The style a box is laid out with: the one it computed, or the one its table
    /// gave it when it collapsed its borders.
    pub(super) fn style_of(&self, id: BoxId) -> Arc<ComputedStyle> {
        if let Some(style) = self.collapsed.get(id) {
            return Arc::clone(style);
        }
        Arc::clone(&self.tree.node(id).style)
    }
}

pub(super) fn resolve_margin(style: &ComputedStyle, containing: f32) -> Sides<f32> {
    // `auto` starts as zero; `resolve_horizontal` is what shares out the leftover
    // when there is one to share.
    let resolve = |value: &LengthOrAuto| value.resolve(containing).unwrap_or(0.0);
    Sides {
        top: resolve(&style.margin.top),
        right: resolve(&style.margin.right),
        bottom: resolve(&style.margin.bottom),
        left: resolve(&style.margin.left),
    }
}

/// The used horizontal margins and content width of a block in the flow.
///
/// This is where `margin: 0 auto` centres. The width is what the box asked for,
/// or with `width: auto` whatever its margins and its frame leave of the
/// containing block — held between its minimum and its maximum either way, which
/// is what makes `max-width` centre a column that `margin: 0 auto` would
/// otherwise leave full width. Whatever that leaves over is shared out between
/// the margins that are `auto`: both of them for centring, one of them for
/// pushing a box to an edge (CSS 2.2 §10.3.3). A box that fills the line leaves
/// nothing over, and an `auto` margin beside it is zero.
///
/// `margin` is the box's margins with `auto` as zero, `frame` its horizontal
/// padding and border, and `sizes` what its sizing properties came to.
pub(super) fn resolve_horizontal(
    style: &ComputedStyle,
    containing: f32,
    mut margin: Sides<f32>,
    frame: f32,
    sizes: Sizes,
) -> (Sides<f32>, f32) {
    let fill = (containing - margin.left - margin.right - frame).max(0.0);
    let width = sizes.used(fill);

    let leftover = containing - width - frame;
    let left_auto = style.margin.left == LengthOrAuto::Auto;
    let right_auto = style.margin.right == LengthOrAuto::Auto;
    match (left_auto, right_auto) {
        (true, true) => {
            margin.left = (leftover / 2.0).max(0.0);
            margin.right = margin.left;
        }
        (true, false) => margin.left = (leftover - margin.right).max(0.0),
        (false, true) => margin.right = (leftover - margin.left).max(0.0),
        (false, false) => {}
    }
    (margin, width)
}

/// The four border widths, which are already absolute lengths by this point.
pub(super) fn resolve_border(style: &ComputedStyle) -> Sides<f32> {
    Sides {
        top: style.border.top.width,
        right: style.border.right.width,
        bottom: style.border.bottom.width,
        left: style.border.left.width,
    }
}

pub(super) fn resolve_padding(style: &ComputedStyle, containing: f32) -> Sides<f32> {
    Sides {
        top: style.padding.top.resolve(containing),
        right: style.padding.right.resolve(containing),
        bottom: style.padding.bottom.resolve(containing),
        left: style.padding.left.resolve(containing),
    }
}

/// A vertical margin, which `auto` makes zero.
pub(super) fn vertical_margin(margin: &LengthOrAuto, containing: f32) -> f32 {
    margin.resolve(containing).unwrap_or(0.0)
}

/// Two margins that have met.
///
/// Both positive: the larger wins. Both negative: the more negative wins. One of
/// each: they add, so a negative margin pulls a box back over its neighbour by
/// exactly as much as it asks for.
pub(super) fn collapse(a: f32, b: f32) -> f32 {
    if a >= 0.0 && b >= 0.0 {
        a.max(b)
    } else if a < 0.0 && b < 0.0 {
        a.min(b)
    } else {
        a + b
    }
}
