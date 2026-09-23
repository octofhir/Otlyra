//! A box's background layers: where a picture's tiles go, how large each is drawn,
//! and the fill that carries them.
//!
//! The colour under the layers is one fill and the walk draws it. A layer is more
//! than that — positioned against the box inside its border, sized against its own
//! proportions, tiled or not along each axis and cut off at the box's outline —
//! and that arithmetic reads better on its own than in the middle of the walk.

use otlyra_gfx::kurbo::{Affine, Rect as KurboRect, Shape};
use otlyra_gfx::peniko::{Brush, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::fragment::{Fragment, Rect};

use crate::gradient::gradient_brush;
use crate::shape::box_shape;
use crate::{BackgroundLookup, PATH_TOLERANCE};

/// Where a background picture's tiles go.
///
/// One tile is placed, and the rest follow from it: a repeating axis is left to the
/// brush, which tiles a picture from wherever its transform puts it, and a
/// non-repeating one is handled by not painting past the one tile — which is what
/// `covered` is narrowed to. The alternative, an extend mode that puts nothing
/// outside the picture, is not one a brush has.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Tiling {
    /// The first tile: where the picture's own top left corner lands, and how
    /// large it is drawn.
    tile: Rect,
    /// The picture's own size in pixels, which the tile is scaled from.
    own: (f32, f32),
    /// The part of the box the picture reaches: the whole of it along an axis that
    /// repeats, one tile's worth along one that does not.
    covered: Rect,
    /// How the brush repeats around the tile.
    sampler: otlyra_gfx::peniko::ImageSampler,
}

impl Tiling {
    /// The brush's own transform: the tile's corner and the scale it is drawn at.
    fn brush_transform(&self, scroll_y: f32) -> Affine {
        Affine::translate((f64::from(self.tile.x), f64::from(self.tile.y - scroll_y)))
            * Affine::scale_non_uniform(
                f64::from(self.tile.width) / f64::from(self.own.0),
                f64::from(self.tile.height) / f64::from(self.own.1),
            )
    }
}

/// Work out that placement from the style, the box and the picture.
///
/// The area a picture is positioned in is the box inside its border, which is what
/// CSS positions a background against however far the painting itself spreads.
fn background_tiling(
    layer: &otlyra_css::BackgroundLayer,
    style: &otlyra_css::ComputedStyle,
    rect: Rect,
    picture: &otlyra_gfx::peniko::ImageData,
) -> Tiling {
    use otlyra_css::Repeat;
    use otlyra_gfx::peniko::Extend;

    let border = style.border;
    let area = Rect::new(
        rect.x + border.left.width,
        rect.y + border.top.width,
        (rect.width - border.left.width - border.right.width).max(0.0),
        (rect.height - border.top.width - border.bottom.width).max(0.0),
    );

    let own = (picture.width as f32, picture.height as f32);
    let (mut width, mut height) = background_extent(layer, area, own);

    // `round` squeezes or stretches the tile so a whole number of them fits, which
    // is the whole of what it does — everything after it is the ordinary tiling.
    let rounded = |extent: f32, along: f32| {
        let count = (along / extent).round().max(1.0);
        along / count
    };
    if layer.repeat.x == Repeat::Round && width > 0.0 {
        width = rounded(width, area.width);
    }
    if layer.repeat.y == Repeat::Round && height > 0.0 {
        height = rounded(height, area.height);
    }

    let x = area.x + layer.position.x.resolve(area.width - width);
    let y = area.y + layer.position.y.resolve(area.height - height);
    let tile = Rect::new(x, y, width, height);

    // Along an axis that does not repeat, the picture reaches only as far as the
    // one tile; along one that does, as far as the box.
    let extent = |repeat: Repeat, start: f32, size: f32, area_start: f32, area_size: f32| {
        if repeat == Repeat::None {
            (start.max(area_start), size.min(area_size))
        } else {
            (area_start, area_size)
        }
    };
    let (left, covered_width) = extent(layer.repeat.x, x, width, area.x, area.width);
    let (top, covered_height) = extent(layer.repeat.y, y, height, area.y, area.height);

    let axis = |repeat: Repeat| match repeat {
        // Nothing is painted outside the one tile, so what the brush would put
        // there never shows; clamping is the cheapest answer that cannot smear.
        Repeat::None => Extend::Pad,
        Repeat::Repeat | Repeat::Round => Extend::Repeat,
    };

    Tiling {
        tile,
        own,
        covered: Rect::new(left, top, covered_width.max(0.0), covered_height.max(0.0)),
        sampler: otlyra_gfx::peniko::ImageSampler {
            x_extend: axis(layer.repeat.x),
            y_extend: axis(layer.repeat.y),
            ..Default::default()
        },
    }
}

/// How large one tile of a background picture is drawn.
///
/// `cover` and `contain` are the two that need the picture's own proportions: one
/// fills the area and is cropped, the other fits inside it whole.
fn background_extent(
    layer: &otlyra_css::BackgroundLayer,
    area: Rect,
    own: (f32, f32),
) -> (f32, f32) {
    let (own_width, own_height) = own;
    let ratio = own_width / own_height.max(1.0);

    match layer.size {
        otlyra_css::BackgroundSize::Auto => (own_width, own_height),
        otlyra_css::BackgroundSize::Fixed(width, height) => {
            (width.resolve(area.width), height.resolve(area.height))
        }
        otlyra_css::BackgroundSize::Cover => {
            if area.width / area.height.max(1.0) > ratio {
                (area.width, area.width / ratio)
            } else {
                (area.height * ratio, area.height)
            }
        }
        otlyra_css::BackgroundSize::Contain => {
            if area.width / area.height.max(1.0) > ratio {
                (area.height * ratio, area.height)
            } else {
                (area.width, area.width / ratio)
            }
        }
    }
}

/// One layer of a box's background: a picture, a gradient, or nothing.
///
/// Each layer is placed, sized and tiled by its own values, and they are drawn one
/// over another in the order the page wrote them — which is why a page can put a
/// pattern over a wash and get both.
pub(super) fn paint_background_layer(
    list: &mut DisplayList,
    layer: &otlyra_css::BackgroundLayer,
    fragment: &Fragment,
    rect: otlyra_layout::Rect,
    scroll_y: f32,
    background_picture: Option<BackgroundLookup<'_>>,
) {
    if let Some(gradient) = layer.gradient.as_ref() {
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Gradient(gradient_brush(gradient, rect, scroll_y)),
            brush_transform: None,
            shape: box_shape(rect, scroll_y, &fragment.style),
        });
        return;
    }

    let Some(url) = layer.image.as_deref() else {
        return;
    };
    let Some(picture) = background_picture.and_then(|lookup| lookup(url)) else {
        return;
    };
    if picture.width == 0 || picture.height == 0 || rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }

    let tiling = background_tiling(layer, &fragment.style, rect, &picture);
    if tiling.covered.width <= 0.0 || tiling.covered.height <= 0.0 {
        return;
    }

    // A background belongs to its box: `cover` is meant to overflow and be cut
    // off, not to spill onto whatever is drawn next. The box's own outline is the
    // edge, so rounded corners cut the picture too.
    list.push(DisplayItem::PushLayer {
        blend: otlyra_gfx::peniko::BlendMode::default(),
        alpha: 1.0,
        transform: Affine::IDENTITY,
        clip: box_shape(rect, scroll_y, &fragment.style),
    });
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Image(otlyra_gfx::peniko::ImageBrush {
            image: picture,
            sampler: tiling.sampler,
        }),
        brush_transform: Some(tiling.brush_transform(scroll_y)),
        shape: KurboRect::new(
            f64::from(tiling.covered.x),
            f64::from(tiling.covered.y - scroll_y),
            f64::from(tiling.covered.right()),
            f64::from(tiling.covered.bottom() - scroll_y),
        )
        .to_path(PATH_TOLERANCE),
    });
    list.push(DisplayItem::PopLayer);
}
