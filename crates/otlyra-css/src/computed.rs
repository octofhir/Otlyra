//! Turning the cascade's answer into the handful of values layout reads.
//!
//! The style engine computes the whole of CSS; layout currently understands twelve
//! properties. This is where the two meet, and it is deliberately a narrowing: a
//! property that layout cannot honour is one this file does not pretend to carry.
//! Widening it is how new CSS support arrives — one property, one conversion, one
//! thing layout does with it.

use std::sync::Arc;

use peniko::Color;
use style::properties::ComputedValues;

use crate::style::{
    ComputedStyle, Display, FontStyle, Length, LengthOrAuto, LineHeight, Sides, TextDecoration,
    WhiteSpace,
};

/// Convert one element's computed values into the style layout reads.
pub fn to_layout_style(values: &ComputedValues) -> ComputedStyle {
    let font = values.get_font();
    let font_size = font.font_size.used_size().px();

    ComputedStyle {
        display: display_of(values),
        color: colour(values.clone_color()),
        background_color: values
            .get_background()
            .background_color
            .as_absolute()
            .map_or(Color::TRANSPARENT, |value| colour(*value)),
        font_family: font_family(values),
        font_size,
        font_weight: font.font_weight.value().round() as u16,
        font_style: if font.font_style == style::values::computed::FontStyle::NORMAL {
            FontStyle::Normal
        } else {
            FontStyle::Italic
        },
        line_height: line_height(values),
        white_space: white_space(values),
        text_decoration: text_decoration(values),
        margin: Sides {
            top: length_or_auto(&values.get_margin().margin_top),
            right: length_or_auto(&values.get_margin().margin_right),
            bottom: length_or_auto(&values.get_margin().margin_bottom),
            left: length_or_auto(&values.get_margin().margin_left),
        },
        padding: Sides {
            top: length(&values.get_padding().padding_top.0),
            right: length(&values.get_padding().padding_right.0),
            bottom: length(&values.get_padding().padding_bottom.0),
            left: length(&values.get_padding().padding_left.0),
        },
        width: size(&values.get_position().width),
        height: size(&values.get_position().height),
    }
}

/// `display`, narrowed to the three values layout has formatting contexts for.
///
/// Everything with an inline outer display becomes inline and everything else
/// becomes block. That is a real approximation — a flex container laid out as a
/// block is wrong — but it is wrong in the direction of showing the content, and
/// `display: none` is not approximated at all.
fn display_of(values: &ComputedValues) -> Display {
    use style::values::computed::Display as Computed;

    let display = values.clone_display();
    if display == Computed::None {
        return Display::None;
    }
    if display.outside() == style::values::specified::box_::DisplayOutside::Inline {
        Display::Inline
    } else {
        Display::Block
    }
}

/// A computed colour, in the paint vocabulary.
fn colour(value: style::color::AbsoluteColor) -> Color {
    let srgb = value.to_color_space(style::color::ColorSpace::Srgb);
    Color::new([
        srgb.components.0,
        srgb.components.1,
        srgb.components.2,
        srgb.alpha,
    ])
}

/// The font stack, as the CSS source list the text layer parses.
fn font_family(values: &ComputedValues) -> Arc<str> {
    use style::values::computed::font::{GenericFontFamily, SingleFontFamily};

    let families: Vec<String> = values
        .get_font()
        .font_family
        .families
        .iter()
        .filter_map(|family| match family {
            SingleFontFamily::FamilyName(name) => Some(name.name.to_string()),
            SingleFontFamily::Generic(generic) => match generic {
                GenericFontFamily::Serif => Some("serif".to_owned()),
                GenericFontFamily::SansSerif => Some("sans-serif".to_owned()),
                GenericFontFamily::Monospace => Some("monospace".to_owned()),
                GenericFontFamily::Cursive => Some("cursive".to_owned()),
                GenericFontFamily::Fantasy => Some("fantasy".to_owned()),
                GenericFontFamily::SystemUi => Some("system-ui".to_owned()),
                // An internal placeholder, not a family anything can be matched to.
                GenericFontFamily::None => None,
            },
        })
        .collect();

    if families.is_empty() {
        Arc::from("sans-serif")
    } else {
        Arc::from(families.join(", "))
    }
}

fn line_height(values: &ComputedValues) -> LineHeight {
    use style::values::computed::LineHeight as Computed;

    match values.clone_line_height() {
        Computed::Normal => LineHeight::Normal,
        Computed::Number(number) => LineHeight::Number(number.0),
        Computed::Length(length) => LineHeight::Px(length.px()),
    }
}

fn white_space(values: &ComputedValues) -> WhiteSpace {
    use style::properties::longhands::white_space_collapse::computed_value::T as WhiteSpaceCollapse;

    match values.clone_white_space_collapse() {
        WhiteSpaceCollapse::Collapse => WhiteSpace::Normal,
        // Preserve, PreserveBreaks, PreserveSpaces and BreakSpaces all keep more
        // than `normal` does; layout has one bit for that today.
        _ => WhiteSpace::Pre,
    }
}

fn text_decoration(values: &ComputedValues) -> TextDecoration {
    let line = values.clone_text_decoration_line();
    TextDecoration {
        underline: line.contains(style::values::computed::TextDecorationLine::UNDERLINE),
        line_through: line.contains(style::values::computed::TextDecorationLine::LINE_THROUGH),
    }
}

fn length_or_auto(value: &style::values::computed::Margin) -> LengthOrAuto {
    use style::values::generics::length::GenericMargin as Generic;

    match value {
        Generic::Auto => LengthOrAuto::Auto,
        Generic::LengthPercentage(value) | Generic::AnchorContainingCalcFunction(value) => {
            match value.to_percentage() {
                Some(percentage) => LengthOrAuto::Percent(percentage.0),
                None => LengthOrAuto::Px(value.to_used_value(app_units::Au(0)).to_f32_px()),
            }
        }
        // Anchor positioning, which layout does not do.
        Generic::AnchorSizeFunction(_) => LengthOrAuto::Auto,
    }
}

fn length(value: &style::values::computed::LengthPercentage) -> Length {
    match value.to_percentage() {
        Some(percentage) => Length::Percent(percentage.0),
        None => Length::Px(value.to_used_value(app_units::Au(0)).to_f32_px()),
    }
}

fn size(value: &style::values::computed::Size) -> LengthOrAuto {
    use style::values::generics::length::GenericSize as Generic;

    match value {
        Generic::Auto => LengthOrAuto::Auto,
        Generic::LengthPercentage(value) => match value.0.to_percentage() {
            Some(percentage) => LengthOrAuto::Percent(percentage.0),
            None => LengthOrAuto::Px(value.0.to_used_value(app_units::Au(0)).to_f32_px()),
        },
        // `min-content`, `max-content` and `fit-content` need intrinsic sizing,
        // which layout does not do; auto is the value it can honour.
        _ => LengthOrAuto::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::{Viewport, style_document};

    /// The layout style of the first element matching `selector`.
    fn layout_style(html: &str, selector: &str) -> ComputedStyle {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styled = style_document(&document, Viewport::default());
        let node = crate::stylo_dom::select(&document, selector)
            .expect("the selector should parse")
            .into_iter()
            .next()
            .expect("something should match");
        to_layout_style(styled.style_of(node).expect("a styled element"))
    }

    #[test]
    fn an_author_rule_reaches_the_values_layout_reads() {
        let style = layout_style(
            "<style>p { color: #f00; font-weight: bold; font-style: italic }</style><p>x",
            "p",
        );
        assert_eq!(style.color, Color::from_rgb8(255, 0, 0));
        assert_eq!(style.font_weight, 700);
        assert_eq!(style.font_style, FontStyle::Italic);
    }

    /// Every length arrives in pixels: `em` is resolved against the parent's font
    /// size before layout ever sees it.
    #[test]
    fn relative_lengths_arrive_resolved() {
        let style = layout_style(
            "<style>div { font-size: 20px } p { font-size: 1.5em; margin-top: 2em }</style>\
             <div><p>x</p></div>",
            "p",
        );
        assert_eq!(style.font_size, 30.0);
        assert_eq!(style.margin.top, LengthOrAuto::Px(60.0));
    }

    #[test]
    fn a_percentage_stays_a_percentage_for_layout_to_resolve() {
        let style = layout_style(
            "<style>p { width: 50%; margin-left: auto }</style><p>x",
            "p",
        );
        assert_eq!(style.width, LengthOrAuto::Percent(0.5));
        assert_eq!(style.margin.left, LengthOrAuto::Auto);
    }

    #[test]
    fn the_font_stack_comes_back_as_css_text() {
        let style = layout_style(
            "<style>p { font-family: \"Some Face\", monospace }</style><p>x",
            "p",
        );
        assert_eq!(&*style.font_family, "Some Face, monospace");
    }

    /// `display: none` has to survive the narrowing: it is the one display value
    /// that changes whether a box exists at all.
    #[test]
    fn display_none_is_not_approximated() {
        assert_eq!(
            layout_style("<style>p { display: none }</style><p>x", "p").display,
            Display::None
        );
        assert_eq!(
            layout_style("<style>p { display: inline }</style><p>x", "p").display,
            Display::Inline
        );
        assert_eq!(
            layout_style(
                "<style>span { display: block }</style><span>x</span>",
                "span"
            )
            .display,
            Display::Block
        );
    }

    /// A formatting context layout does not have still generates a box, laid out as
    /// a block. Dropping the element instead would hide its content entirely.
    #[test]
    fn an_unsupported_display_falls_back_to_block() {
        assert_eq!(
            layout_style("<style>p { display: grid }</style><p>x", "p").display,
            Display::Block
        );
    }

    #[test]
    fn preserved_whitespace_survives_as_a_flag() {
        assert_eq!(
            layout_style("<style>p { white-space: pre }</style><p>x", "p").white_space,
            WhiteSpace::Pre
        );
        assert_eq!(layout_style("<p>x", "p").white_space, WhiteSpace::Normal);
    }
}
