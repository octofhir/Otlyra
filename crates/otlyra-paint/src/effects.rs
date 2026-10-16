//! What a box does to everything it draws, as one: its opacity and its transform.
//!
//! Both belong to a box's whole subtree rather than to any one fragment in it — a
//! half-transparent card is composited once, and a turned one turns everything
//! inside it. The walk is flat, so it opens a group when it meets such a box and
//! closes it when it leaves that box's layer; what a group is, and what closing
//! one does, is here. Its own module because it is the one part of painting whose
//! reach is a subtree rather than a fragment.

use otlyra_gfx::kurbo::Affine;
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::fragment::Fragment;

/// A box that draws its contents as a group: composited once, moved as one, or
/// both.
pub(super) struct Group<'a> {
    pub(super) fragment: &'a Fragment,
    /// Whether a compositing layer was opened for it, which has to be closed.
    pub(super) layer: bool,
    /// What to move everything it drew by, if it is transformed.
    pub(super) transform: Option<Affine>,
    /// Where a masked box is drawn, when it is masked: the space its mask
    /// pictures are laid out in.
    pub(super) masked: Option<otlyra_gfx::SpatialId>,
    /// The clip its ancestors cut a transformed box off at, pushed as a
    /// layer outside the transform.
    pub(super) outer_clip: Option<otlyra_layout::Rect>,
    /// The first item drawn inside it.
    pub(super) from: usize,
}

/// Finish a group: move what it drew, then close the layer it opened.
///
/// In that order. The layer is a compositing step around the drawing, and the
/// transform is a property of the drawing itself — applied after the layer was
/// closed it would move the boundary rather than the contents.
pub(super) fn close(
    group: Group<'_>,
    list: &mut DisplayList,
    pictures: Option<crate::BackgroundLookup<'_>>,
) {
    // The mask, over what the group drew, keeping it only where the mask's
    // pictures are opaque (CSS Masking §7, alpha masks): drawn into a layer
    // that is composited `destination-in`, inside the group's own layer.
    if let Some(spatial) = group.masked {
        use otlyra_gfx::kurbo::Shape as _;
        let fragment = group.fragment;
        let rect = fragment.rect;
        // Over the box: the mask's painting area is its border box.
        list.set_spatial(spatial);
        list.push(DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::new(
                otlyra_gfx::peniko::Mix::Normal,
                otlyra_gfx::peniko::Compose::DestIn,
            ),
            alpha: 1.0,
            transform: Affine::IDENTITY,
            clip: otlyra_gfx::kurbo::Rect::new(
                f64::from(rect.x),
                f64::from(rect.y),
                f64::from(rect.right()),
                f64::from(rect.bottom()),
            )
            .to_path(crate::PATH_TOLERANCE),
        });
        for layer in fragment.style.masks.iter().rev() {
            crate::background::paint_background_layer(
                list,
                layer,
                fragment,
                fragment.rect,
                pictures,
            );
        }
        list.push(DisplayItem::PopLayer);
    }
    if let Some(transform) = group.transform {
        list.transform_from(group.from, transform);
    }
    if group.layer {
        list.push(DisplayItem::PopLayer);
    }
    if group.outer_clip.is_some() {
        list.push(DisplayItem::PopLayer);
    }
}

/// The matrix a box's `transform` comes to, about its own origin.
///
/// `None` when the box is not transformed, which is nearly every box on nearly
/// every page. The origin is a point in the box's *border* box — the middle of it
/// unless `transform-origin` says otherwise — so the steps are applied there and
/// the box put back afterwards, which is what makes `rotate()` turn a card about
/// its middle rather than swing it about the corner of the page.
pub(super) fn transform_of(fragment: &Fragment) -> Option<Affine> {
    let steps = &fragment.style.transform;
    if steps.is_empty() {
        return None;
    }

    let rect = fragment.rect;
    let mut matrix = Affine::IDENTITY;
    for step in steps.iter() {
        matrix *= match step {
            otlyra_css::TransformOp::Translate(x, y) => Affine::translate((
                f64::from(x.resolve(rect.width)),
                f64::from(y.resolve(rect.height)),
            )),
            &otlyra_css::TransformOp::Scale(x, y) => Affine::scale_non_uniform(x.into(), y.into()),
            &otlyra_css::TransformOp::Rotate(radians) => Affine::rotate(radians.into()),
            &otlyra_css::TransformOp::Skew(x, y) => {
                Affine::new([1.0, f64::from(y).tan(), f64::from(x).tan(), 1.0, 0.0, 0.0])
            }
            &otlyra_css::TransformOp::Matrix([a, b, c, d, e, f]) => {
                Affine::new([a.into(), b.into(), c.into(), d.into(), e.into(), f.into()])
            }
        };
    }

    let origin = (
        f64::from(rect.x + fragment.style.transform_origin.x.resolve(rect.width)),
        f64::from(rect.y + fragment.style.transform_origin.y.resolve(rect.height)),
    );
    Some(Affine::translate(origin) * matrix * Affine::translate((-origin.0, -origin.1)))
}
