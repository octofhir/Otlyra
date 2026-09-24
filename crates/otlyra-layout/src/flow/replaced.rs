//! Replaced boxes: a picture, and the frame around it.
//!
//! A replaced element's content is not laid out. It has a size of its own and a
//! ratio between its sides, and CSS says how a stylesheet and the element's
//! attributes bend those. A picture is sized the same way in a block, in a line
//! and in a flex item, so the sizing lives here rather than in any one of them.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Ratio, Size};

use crate::box_tree::{BoxId, Replaced};
use crate::fragment::{Fragment, FragmentKind, Rect};

use super::box_model::{resolve_border, resolve_padding};
use super::sizing::{
    Frame, InlineRoom, Limits, OwnWidth, PreferredRatio, Sizes, block_sizes_in, content_box,
    preferred_ratio, replaced_widths,
};

/// The size a replaced box is drawn at: its *content* box, which is the picture
/// and not the frame around it.
///
/// CSS first, then whatever the content itself says, and a single given dimension
/// takes the other from the picture's preferred aspect ratio — its natural one,
/// unless `aspect-ratio` says otherwise (CSS Sizing 4 §4.1) — which is what
/// makes `width: 100%` on a photograph keep its shape instead of squashing it.
///
/// `box-sizing: border-box` takes the frame out of the number the page wrote, and
/// the ratio is applied to what is left: a hundred-pixel box with a ten-pixel
/// border holds eighty pixels of picture, and a two-to-one picture is forty tall
/// rather than fifty. The presentational `width` attribute goes through the same
/// door, because it is a rule setting `width` and nothing more; and so do the
/// minimum and the maximum, which hold the picture rather than its frame.
///
/// The sizing properties are read as they are for any box (see
/// [`replaced_widths`] for what is particular to a picture), and a given
/// dimension is held between its limits *before* the other is taken from it, so
/// a maximum narrows a picture rather than squashing it. `containing_height` is
/// what a percentage `height` is of, when there is one.
pub(super) fn replaced_size(
    style: &ComputedStyle,
    content: &Replaced,
    room: InlineRoom,
    containing_height: Option<f32>,
) -> (f32, f32) {
    let frame = Frame::of(style, room.measure);
    let heights = picture_heights(style, content, room.measure, containing_height);
    let own = OwnWidth {
        natural: width_from(style, content, frame, heights),
        hint: content.hint.0,
    };
    let widths = replaced_widths(style, room, frame.inline, own);
    drawn_size(style, content, frame, widths, heights)
}

/// The content-box width a picture comes to with nothing said about its width:
/// its own, or what its heights make of it through its ratio — which is its
/// min-content and its max-content size alike (CSS Sizing 3 §5.1).
///
/// A percentage height is of `containing_height`, the containing block's
/// height when it has one, and `auto` when it has none.
pub(super) fn natural_width(
    style: &ComputedStyle,
    content: &Replaced,
    containing_width: f32,
    containing_height: Option<f32>,
) -> f32 {
    width_from(
        style,
        content,
        Frame::of(style, containing_width),
        picture_heights(style, content, containing_width, containing_height),
    )
}

/// The content-box height of a picture drawn `width` wide, where the width is
/// one its container settled rather than one the picture's own sizing
/// properties asked for: a float or a positioned box that shrank to it, a grid
/// cell, a flex line.
///
/// The height follows the width through the ratio and is held between its own
/// limits (CSS 2.2 §10.6.2). Asking the picture's width percentages again
/// would resolve them against the width they already came to — half of a
/// picture that is already half of its column — and the height taken from that
/// answer would squash the picture into a box it was never drawn to fill.
pub(super) fn replaced_height(
    style: &ComputedStyle,
    content: &Replaced,
    width: f32,
    containing_width: f32,
    containing_height: Option<f32>,
) -> f32 {
    let heights = picture_heights(style, content, containing_width, containing_height);
    height_at(
        style,
        content,
        Frame::of(style, containing_width),
        width,
        heights,
    )
}

/// What `height`, `min-height` and `max-height` ask of a picture, as content-box
/// heights, with a `height` attribute standing in for an `auto` height — and
/// only for `auto`, since any height a stylesheet names outranks a hint.
pub(super) fn picture_heights(
    style: &ComputedStyle,
    content: &Replaced,
    containing_width: f32,
    containing_height: Option<f32>,
) -> Sizes {
    let (_, natural_height) = content.intrinsic.unwrap_or_default();
    let heights = block_sizes_in(
        style,
        containing_width,
        containing_height,
        Some(natural_height),
    );
    match style.height {
        Size::Auto => Sizes {
            preferred: content.hint.1.map(|hint| {
                content_box(
                    hint,
                    style.box_sizing,
                    Frame::of(style, containing_width).block,
                )
            }),
            ..heights
        },
        Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => heights,
    }
}

/// A picture's preferred aspect ratio (CSS Sizing 4 §4.1): its natural ratio,
/// when it has both sides to take one from, as `aspect-ratio` lets it have it.
pub(super) fn ratio(style: &ComputedStyle, content: &Replaced) -> Option<PreferredRatio> {
    let natural = content
        .intrinsic
        .and_then(|(width, height)| Ratio::new(width, height));
    preferred_ratio(style, natural)
}

/// The width a picture comes to when nothing is said about its width, from
/// what is said about its height.
fn width_from(style: &ComputedStyle, content: &Replaced, frame: Frame, heights: Sizes) -> f32 {
    drawn_size(style, content, frame, Sizes::AUTO, heights).0
}

/// The content-box height a picture drawn `width` wide comes to: what it asked
/// for, or the width through its ratio, or its own height when it has no ratio
/// — held between its limits.
fn height_at(
    style: &ComputedStyle,
    content: &Replaced,
    frame: Frame,
    width: f32,
    heights: Sizes,
) -> f32 {
    let (_, natural_height) = content.intrinsic.unwrap_or_default();
    heights
        .used(ratio(style, content).map_or(natural_height, |ratio| ratio.height_for(width, frame)))
}

/// The size a picture is with nothing said about either side of it, when it
/// has a ratio: its natural size, or where the ratio is not the picture's own,
/// its natural width and the height the ratio makes of it (see
/// [`PreferredRatio::overrides_natural_height`]). A picture with a natural
/// width and no natural height has no ratio of its own, and so takes its
/// height from the stylesheet's.
///
/// `None` for a picture with no natural width to start from, whose ratio is
/// the stylesheet's alone.
fn natural_size(content: &Replaced, ratio: PreferredRatio, frame: Frame) -> Option<(f32, f32)> {
    let (width, height) = content.intrinsic?;
    let height = if ratio.overrides_natural_height() {
        ratio.height_for(width, frame)
    } else {
        height
    };
    (width > 0.0 && height > 0.0).then_some((width, height))
}

/// The content-box size a picture is drawn at, once its width and its height
/// have been asked what they want.
fn drawn_size(
    style: &ComputedStyle,
    content: &Replaced,
    frame: Frame,
    widths: Sizes,
    heights: Sizes,
) -> (f32, f32) {
    let (natural_width, _) = content.intrinsic.unwrap_or_default();
    let at_width = |width| (width, height_at(style, content, frame, width, heights));
    match (widths.preferred, heights.preferred, ratio(style, content)) {
        // CSS 2.2 §10.3.2: an `auto` width is the used height through the
        // ratio — the height once its own limits have had their say.
        (None, Some(height), Some(ratio)) => {
            let height = heights.limits.clamp(height);
            (widths.limits.clamp(ratio.width_for(height, frame)), height)
        }
        (None, None, Some(ratio)) => match natural_size(content, ratio, frame) {
            Some(natural) => within_keeping_ratio(natural, widths.limits, heights.limits),
            None => at_width(widths.used(natural_width)),
        },
        // And §10.6.2 the other way about: a width that is given, or one that
        // is the picture's own because it has no ratio to take one from, is
        // held between its limits before the height is taken from it.
        (Some(_), _, _) | (None, _, None) => at_width(widths.used(natural_width)),
    }
}

/// Where a size stands against one dimension's limits.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Against {
    /// Wider or taller than the maximum.
    Over,
    /// Narrower or shorter than the minimum.
    Under,
    /// Between the two.
    Within,
}

impl Against {
    fn of(size: f32, limits: Limits) -> Self {
        if size > limits.max {
            Self::Over
        } else if size < limits.min {
            Self::Under
        } else {
            Self::Within
        }
    }
}

/// A picture whose `width` and `height` are both `auto`, held between its
/// limits without losing its shape: the table in CSS 2.2 §10.4.
///
/// Held one dimension at a time, `img { max-width: 100% }` — the first rule of
/// nearly every stylesheet on the web — draws an 800-by-400 picture in a
/// 400-pixel column at 400 by 400. Taken through the ratio instead, the
/// dimension that is out of bounds is brought in and the other follows it,
/// unless following it would take *that* one out of its own bounds, where its
/// limit wins and the shape gives. Where both are out of bounds the one that
/// has further to go decides. A maximum below its minimum is the minimum, as it
/// is everywhere else.
///
/// `natural` is the picture's own size, both sides of it above zero: that is
/// what having a ratio means. The shape kept is that size's, across the
/// content box, which is the ratio's shape unless a `<ratio>` of the
/// stylesheet's measures it across the border box of a picture with padding
/// or a border: there the limits are met by a content box of the ratio's
/// shape rather than by a border box of it.
fn within_keeping_ratio(natural: (f32, f32), widths: Limits, heights: Limits) -> (f32, f32) {
    let (width, height) = natural;
    let widths = Limits {
        max: widths.max.max(widths.min),
        ..widths
    };
    let heights = Limits {
        max: heights.max.max(heights.min),
        ..heights
    };
    // The height a width brings with it, and the width a height does.
    let height_for = |across: f32| across * height / width;
    let width_for = |down: f32| down * width / height;
    let narrowed = || (widths.max, height_for(widths.max).max(heights.min));
    let widened = || (widths.min, height_for(widths.min).min(heights.max));
    let shortened = || (width_for(heights.max).max(widths.min), heights.max);
    let lengthened = || (width_for(heights.min).min(widths.max), heights.min);

    match (Against::of(width, widths), Against::of(height, heights)) {
        (Against::Within, Against::Within) => natural,
        (Against::Over, Against::Within) => narrowed(),
        (Against::Under, Against::Within) => widened(),
        (Against::Within, Against::Over) => shortened(),
        (Against::Within, Against::Under) => lengthened(),
        (Against::Over, Against::Over) if widths.max / width <= heights.max / height => narrowed(),
        (Against::Over, Against::Over) => shortened(),
        (Against::Under, Against::Under) if widths.min / width <= heights.min / height => {
            lengthened()
        }
        (Against::Under, Against::Under) => widened(),
        (Against::Under, Against::Over) => (widths.min, heights.max),
        (Against::Over, Against::Under) => (widths.max, heights.min),
    }
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
    let frame = Frame::new(padding, border);

    let picture = image.map(|image| {
        Fragment::new(
            // The element's own box carries the hit test; a second one over the
            // picture would put two of them on the same element.
            None,
            Rect::new(
                x + border.left + padding.left,
                y + border.top + padding.top,
                width,
                height,
            ),
            FragmentKind::Image(image),
            Arc::clone(style),
        )
    });

    Fragment::for_box(
        id,
        Rect::new(x, y, width + frame.inline, height + frame.block),
        Arc::clone(style),
        picture.into_iter().collect(),
    )
}
