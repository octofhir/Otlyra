//! The box model: margins, borders, padding, and the sizes they come to.
//!
//! Every formatting context asks the same questions of a box before it can place
//! it — how much room its frame takes, what `auto` and a percentage resolve to,
//! and where `min-` and `max-` hold it — and the answers have to be the same
//! whichever context asked. So they are worked out here, once, rather than in
//! whichever context happened to need them first.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Length, LengthOrAuto, Sides};

use crate::box_tree::BoxId;

use super::Flow;

/// Whether any of the four sides is non-zero.
pub(super) fn any_side(sides: Sides<f32>) -> bool {
    sides.top > 0.0 || sides.right > 0.0 || sides.bottom > 0.0 || sides.left > 0.0
}

impl<'a> Flow<'a> {
    /// The height a box asks for, if it asks for one that means anything here.
    ///
    /// A length is itself. A percentage is of the containing block's height, which
    /// most blocks on the web do not have — they are as tall as what is in them —
    /// and against one of those CSS says the percentage is `auto`. Resolving it
    /// against the width instead, which is what the same call does for every
    /// horizontal property, makes a box as tall as its parent is wide.
    pub(super) fn asked_height(&self, style: &ComputedStyle) -> Option<f32> {
        let declared = match style.height {
            LengthOrAuto::Px(px) => Some(px),
            LengthOrAuto::Percent(fraction) => {
                self.containing_height.map(|height| fraction * height)
            }
            LengthOrAuto::Auto => None,
        }?;

        // Whatever the number was measured across, what layout wants is the content
        // box. A percentage is of the containing block's *content* height, so the
        // subtraction is the same either way.
        let padding = resolve_padding(style, 0.0);
        Some(content_height_from(
            declared,
            style,
            padding,
            resolve_border(style),
        ))
    }

    /// The height to resolve the percentages *inside* a box against.
    ///
    /// Its own, when it has one; otherwise nothing, because a box that is as tall
    /// as its contents cannot answer a question its contents are asking.
    pub(super) fn inner_height(&self, style: &ComputedStyle, padding: Sides<f32>) -> Option<f32> {
        self.asked_height(style)
            .map(|height| (height - padding.top - padding.bottom).max(0.0))
    }

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
    let resolve = |value: LengthOrAuto| value.resolve(containing).unwrap_or(0.0);
    Sides {
        top: resolve(style.margin.top),
        right: resolve(style.margin.right),
        bottom: resolve(style.margin.bottom),
        left: resolve(style.margin.left),
    }
}

/// The content width a declared `width` comes to.
///
/// `box-sizing: border-box` — which most of the web sets on everything before it
/// writes a single width — measures the number across the border box, so the
/// padding and the border come *out* of it rather than being added outside it. A
/// box laid out the other way is that much wider than the page asked for, and a
/// row of them is that much wider than the row.
pub(super) fn content_from(
    width: f32,
    style: &ComputedStyle,
    padding: Sides<f32>,
    border: Sides<f32>,
) -> f32 {
    match style.box_sizing {
        otlyra_css::BoxSizing::Content => width,
        otlyra_css::BoxSizing::Border => {
            (width - padding.left - padding.right - border.left - border.right).max(0.0)
        }
    }
}

/// The same down the block axis.
pub(super) fn content_height_from(
    height: f32,
    style: &ComputedStyle,
    padding: Sides<f32>,
    border: Sides<f32>,
) -> f32 {
    match style.box_sizing {
        otlyra_css::BoxSizing::Content => height,
        otlyra_css::BoxSizing::Border => {
            (height - padding.top - padding.bottom - border.top - border.bottom).max(0.0)
        }
    }
}

/// The used horizontal margins and content width.
///
/// This is where `margin: 0 auto` centres. With an explicit width, whatever is
/// left over after the borders, padding and the margins that are not `auto` is
/// shared out between the ones that are — both of them for centring, one of them
/// for pushing a box to an edge. With `width: auto` there is nothing left over by
/// definition, and CSS makes an `auto` margin zero.
pub(super) fn resolve_horizontal(
    style: &ComputedStyle,
    containing: f32,
    padding: Sides<f32>,
    border: Sides<f32>,
) -> (Sides<f32>, f32) {
    let mut margin = resolve_margin(style, containing);
    let extra = padding.left + padding.right + border.left + border.right;

    // `max-width` and `min-width` are applied to whatever `width` worked out to,
    // and the box is then laid out again as if that were the width it asked for —
    // which is what makes `max-width` centre a column that `margin: 0 auto` would
    // otherwise leave full width.
    let width = match style.width.resolve(containing) {
        Some(width) => Some(content_from(
            clamp(width, style.min_width, style.max_width, containing),
            style,
            padding,
            border,
        )),
        None => {
            let available = (containing - margin.left - margin.right - extra).max(0.0);
            let constrained = clamp(available, style.min_width, style.max_width, containing);
            (constrained != available).then_some(constrained)
        }
    };

    let Some(width) = width else {
        let content = (containing - margin.left - margin.right - extra).max(0.0);
        return (margin, content);
    };

    let leftover = containing - width - extra;
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

/// A size held between its minimum and its maximum.
///
/// The maximum is applied first and the minimum second, which is the order CSS
/// gives them and the reason a `min-width` larger than a `max-width` wins.
pub(super) fn clamp(value: f32, min: Length, max: Option<Length>, containing: f32) -> f32 {
    let capped = match max {
        Some(max) => value.min(max.resolve(containing)),
        None => value,
    };
    capped.max(min.resolve(containing))
}

/// The same, for a height.
///
/// A percentage here is of the containing block's *height*, and against an
/// ancestor that is as tall as its own contents CSS says it means nothing at
/// all — so it is dropped rather than resolved against something else. Against
/// the width, which is what everything horizontal resolves against and what this
/// used to be handed, `min-height: 100%` makes a box as tall as its parent is
/// wide: a `<body>` painting its own background over the whole window and hiding
/// the canvas — and the root element's background — behind it.
pub(super) fn clamp_height(
    value: f32,
    min: Length,
    max: Option<Length>,
    containing: Option<f32>,
) -> f32 {
    let resolve = |length: Length| match length {
        Length::Px(px) => Some(px),
        Length::Percent(fraction) => containing.map(|height| fraction * height),
    };
    let capped = match max.and_then(resolve) {
        Some(max) => value.min(max),
        None => value,
    };
    match resolve(min) {
        Some(min) => capped.max(min),
        None => capped,
    }
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
    let resolve = |value: Length| value.resolve(containing);
    Sides {
        top: resolve(style.padding.top),
        right: resolve(style.padding.right),
        bottom: resolve(style.padding.bottom),
        left: resolve(style.padding.left),
    }
}

/// A vertical margin, which `auto` makes zero.
pub(super) fn vertical_margin(margin: LengthOrAuto, containing: f32) -> f32 {
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
