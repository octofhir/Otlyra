//! The outline of a box, square or rounded.
//!
//! A background, the edge its picture is cut off at, a border and a shadow all
//! follow the same outline, and a corner one of them rounded differently from the
//! others would show as a seam. One module, so there is one answer to where a
//! box's edge is.

use otlyra_gfx::kurbo::{BezPath, Rect as KurboRect, Shape};
use otlyra_layout::fragment::Rect;

use crate::PATH_TOLERANCE;

/// The outline of a box: a rectangle, or a rounded one where `border-radius` says.
///
/// One radius per corner rather than an ellipse's two, and the radii are scaled
/// down together if they overlap — which is the rule CSS gives for a box asked for
/// rounder corners than it has room for.
pub(super) fn box_shape(rect: Rect, scroll_y: f32, style: &otlyra_css::ComputedStyle) -> BezPath {
    shape_with_radii(rect, scroll_y, style, 0.0)
}

/// The same outline, grown by `spread` at every corner as well as every edge.
///
/// A shadow spread outwards is not the box's own curve moved: the specification
/// grows each non-zero radius by the spread, so the shadow of a rounded box stays
/// the same shape rather than turning into a rounded rectangle with tighter corners.
pub(super) fn shape_with_radii(
    rect: Rect,
    scroll_y: f32,
    style: &otlyra_css::ComputedStyle,
    spread: f32,
) -> BezPath {
    let bounds = KurboRect::new(
        f64::from(rect.x),
        f64::from(rect.y - scroll_y),
        f64::from(rect.right()),
        f64::from(rect.bottom() - scroll_y),
    );

    if !style.radius.any() {
        return bounds.to_path(PATH_TOLERANCE);
    }

    let corner = |value: &otlyra_css::Length| {
        let radius = value.resolve(rect.width);
        if radius <= 0.0 {
            // A square corner stays square however far the shadow spreads.
            return 0.0;
        }
        f64::from((radius + spread).max(0.0))
    };
    let mut radii = [
        corner(&style.radius.top_left),
        corner(&style.radius.top_right),
        corner(&style.radius.bottom_right),
        corner(&style.radius.bottom_left),
    ];

    // Two radii along one edge cannot together be longer than the edge.
    let width = f64::from(rect.width);
    let height = f64::from(rect.height);
    let scale = [
        (radii[0] + radii[1], width),
        (radii[2] + radii[3], width),
        (radii[0] + radii[3], height),
        (radii[1] + radii[2], height),
    ]
    .iter()
    .filter(|(sum, _)| *sum > 0.0)
    .map(|(sum, edge)| edge / sum)
    .fold(1.0_f64, f64::min);
    if scale < 1.0 {
        for radius in &mut radii {
            *radius *= scale;
        }
    }

    otlyra_gfx::kurbo::RoundedRect::from_rect(
        bounds,
        otlyra_gfx::kurbo::RoundedRectRadii::new(radii[0], radii[1], radii[2], radii[3]),
    )
    .to_path(PATH_TOLERANCE)
}
