//! Replaced boxes: a picture, and the frame around it.
//!
//! A replaced element's content is not laid out. It has a size of its own and a
//! ratio between its sides, and CSS says how a stylesheet and the element's
//! attributes bend those. A picture is sized the same way in a block, in a line
//! and in a flex item, so the sizing lives here rather than in any one of them.

use std::sync::Arc;

use otlyra_css::ComputedStyle;

use crate::box_tree::BoxId;
use crate::fragment::{Fragment, FragmentKind, Layer, Rect};

use super::box_model::{clamp, content_from, content_height_from, resolve_border, resolve_padding};

/// The size a replaced box is drawn at: its *content* box, which is the picture
/// and not the frame around it.
///
/// CSS first, then whatever the content itself says, and a single given dimension
/// takes the other from the intrinsic ratio — which is what makes `width: 100%` on
/// a photograph keep its shape instead of squashing it.
///
/// `box-sizing: border-box` takes the frame out of the number the page wrote, and
/// the ratio is applied to what is left: a hundred-pixel box with a ten-pixel
/// border holds eighty pixels of picture, and a two-to-one picture is forty tall
/// rather than fifty. The presentational `width` attribute goes through the same
/// door, because it is a rule setting `width` and nothing more.
pub(super) fn replaced_size(
    style: &ComputedStyle,
    content: &crate::box_tree::Replaced,
    containing: f32,
) -> (f32, f32) {
    let intrinsic = content.intrinsic;
    let ratio = intrinsic.and_then(|(width, height)| (height > 0.0).then(|| width / height));
    let padding = resolve_padding(style, containing);
    let border = resolve_border(style);

    // A stylesheet first, then the attribute that stands in for one. Either way
    // a dimension that is given takes the other from the ratio below, so naming
    // one never squashes the picture.
    let width = style
        .width
        .resolve(containing)
        .or(content.hint.0)
        .map(|width| content_from(width, style, padding, border));
    let height = style
        .height
        .resolve(containing)
        .or(content.hint.1)
        .map(|height| content_height_from(height, style, padding, border));

    let (width, height) = match (width, height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, ratio.map_or(0.0, |ratio| width / ratio)),
        (None, Some(height)) => (ratio.map_or(0.0, |ratio| height * ratio), height),
        (None, None) => intrinsic.unwrap_or((0.0, 0.0)),
    };

    (
        clamp(width, style.min_width, style.max_width, containing),
        clamp(height, style.min_height, style.max_height, containing),
    )
}

/// How much wider and taller a replaced element's border box is than its picture.
pub(super) fn replaced_edges(style: &ComputedStyle, containing: f32) -> (f32, f32) {
    let padding = resolve_padding(style, containing);
    let border = resolve_border(style);
    (
        padding.left + padding.right + border.left + border.right,
        padding.top + padding.bottom + border.top + border.bottom,
    )
}

/// The fragment a replaced box becomes: a box the size of its border box, with
/// the picture inside it at its content box.
///
/// Two fragments rather than one, because a replaced element has a background and
/// a border of its own like any other box, and the picture is what fills the room
/// left inside them. Drawn as a single fragment the frame is neither painted nor
/// given room, which is why a page that wants one puts the picture inside
/// something else.
///
/// `x` and `y` are the border box's top left, and `width`/`height` its content.
pub(super) fn replaced_fragment(
    id: BoxId,
    style: &Arc<ComputedStyle>,
    image: Option<otlyra_gfx::peniko::ImageData>,
    origin: (f32, f32),
    content: (f32, f32),
    containing: f32,
) -> Fragment {
    let ((x, y), (width, height)) = (origin, content);
    let padding = resolve_padding(style, containing);
    let border = resolve_border(style);
    let (extra_x, extra_y) = replaced_edges(style, containing);

    let picture = image.map(|image| Fragment {
        used: None,
        // The element's own box carries the hit test; a second one over the
        // picture would put two of them on the same element.
        box_id: None,
        rect: Rect::new(
            x + border.left + padding.left,
            y + border.top + padding.top,
            width,
            height,
        ),
        kind: FragmentKind::Image(image),
        style: Arc::clone(style),
        widget: None,
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children: Vec::new(),
    });

    Fragment {
        used: None,
        box_id: Some(id),
        rect: Rect::new(x, y, width + extra_x, height + extra_y),
        kind: FragmentKind::Box,
        style: Arc::clone(style),
        widget: None,
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children: picture.into_iter().collect(),
    }
}
