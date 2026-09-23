//! The shadows a box casts on the inside of itself.
//!
//! An outset shadow is one blurred shape, and the walk draws it where it meets the
//! box, behind everything the box draws. An inset one is a shape with a hole in
//! it, clipped to the padding box and drawn between the background and the border,
//! which is enough of a construction to be read on its own.

use otlyra_gfx::kurbo::{Affine, Rect as KurboRect, Shape};
use otlyra_gfx::peniko::Brush;
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::fragment::{Fragment, Rect};

use crate::PATH_TOLERANCE;
use crate::shape::{box_shape, shape_with_radii};

/// The shadows a box's own hole casts on the inside of it.
///
/// An inset shadow is the outset one turned inside out: what is drawn is
/// everything the shadow's rectangle does *not* cover, clipped to the padding box,
/// so the shadow crowds in from the edges and the offset and the spread both move
/// the lit part rather than the dark one. Expressed as a shape with a hole in it —
/// one enormous rectangle and the shadow's own rectangle wound the other way —
/// because a blur is a blur of one shape, and four blurred edges laid over each
/// other would darken where they overlap.
pub(super) fn paint_inset_shadows(
    list: &mut DisplayList,
    fragment: &Fragment,
    rect: otlyra_layout::Rect,
    scroll_y: f32,
) {
    let inset: Vec<&otlyra_css::Shadow> = fragment
        .style
        .shadows
        .iter()
        .filter(|shadow| shadow.inset && shadow.color.components[3] > 0.0)
        .collect();
    if inset.is_empty() {
        return;
    }

    // The padding box: an inset shadow is cast on the inside of the border rather
    // than on the inside of the box.
    let border = fragment.style.border;
    let hole = Rect::new(
        rect.x + border.left.width,
        rect.y + border.top.width,
        (rect.width - border.left.width - border.right.width).max(0.0),
        (rect.height - border.top.width - border.bottom.width).max(0.0),
    );
    if hole.width <= 0.0 || hole.height <= 0.0 {
        return;
    }

    list.push(DisplayItem::PushLayer {
        blend: otlyra_gfx::peniko::BlendMode::default(),
        alpha: 1.0,
        transform: Affine::IDENTITY,
        clip: box_shape(hole, scroll_y, &fragment.style),
    });

    for shadow in inset {
        // Far enough out that the blurred edge of the outer rectangle never
        // reaches the padding box, whatever the shadow asked for.
        let reach =
            f64::from(shadow.blur + shadow.spread.abs() + shadow.x.abs() + shadow.y.abs()) + 8.0;
        let lit = Rect::new(
            hole.x + shadow.x + shadow.spread,
            hole.y + shadow.y + shadow.spread,
            (hole.width - shadow.spread * 2.0).max(0.0),
            (hole.height - shadow.spread * 2.0).max(0.0),
        );

        let mut shape = KurboRect::new(
            f64::from(hole.x) - reach,
            f64::from(hole.y - scroll_y) - reach,
            f64::from(hole.right()) + reach,
            f64::from(hole.bottom() - scroll_y) + reach,
        )
        .to_path(PATH_TOLERANCE);
        // Wound the other way, so the fill leaves it empty rather than covering it.
        shape.extend(
            shape_with_radii(lit, scroll_y, &fragment.style, -shadow.spread)
                .reverse_subpaths()
                .iter(),
        );

        list.push(DisplayItem::Blurred {
            transform: Affine::IDENTITY,
            brush: Brush::Solid(shadow.color),
            blur: f64::from(shadow.blur),
            shape,
        });
    }

    list.push(DisplayItem::PopLayer);
}
