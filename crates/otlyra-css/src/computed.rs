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
    Border, ComputedStyle, Display, FontStyle, Length, LengthOrAuto, LineHeight, Sides, TextAlign,
    TextDecoration, WhiteSpace,
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
        border: border(values),
        text_align: text_align(values),
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

/// A colour that may be `currentColor`, resolved against the element's own colour.
fn resolve_colour(value: &style::values::computed::Color, current: Color) -> Color {
    value.as_absolute().map_or(current, |value| colour(*value))
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

/// The four borders, each as the width that is actually used and its colour.
///
/// A border whose style is `none` or `hidden` has a used width of zero however
/// wide it was declared, which is the rule that keeps `border-style` out of the
/// computed style this crate hands to layout.
fn border(values: &ComputedValues) -> Sides<Border> {
    use style::values::computed::BorderStyle;

    let border = values.get_border();
    let text = colour(values.clone_color());
    let side =
        |width: &style::values::computed::BorderSideWidth, style: BorderStyle, colour_value| {
            if matches!(style, BorderStyle::None | BorderStyle::Hidden) {
                return Border::NONE;
            }
            Border {
                width: width.0.to_f32_px(),
                color: resolve_colour(colour_value, text),
            }
        };

    Sides {
        top: side(
            &border.border_top_width,
            border.border_top_style,
            &border.border_top_color,
        ),
        right: side(
            &border.border_right_width,
            border.border_right_style,
            &border.border_right_color,
        ),
        bottom: side(
            &border.border_bottom_width,
            border.border_bottom_style,
            &border.border_bottom_color,
        ),
        left: side(
            &border.border_left_width,
            border.border_left_style,
            &border.border_left_color,
        ),
    }
}

fn text_align(values: &ComputedValues) -> TextAlign {
    use style::values::computed::TextAlign as Computed;

    match values.clone_text_align() {
        Computed::Center | Computed::MozCenter => TextAlign::Center,
        Computed::Right | Computed::End | Computed::MozRight => TextAlign::End,
        // `justify` spaces a line out, which inline layout does not do; its start
        // edge is where a start-aligned line begins, so that is what it gets.
        _ => TextAlign::Start,
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

    /// `border-style` decides whether a declared width is used at all, which is
    /// why layout is handed a width rather than a style to interpret.
    #[test]
    fn a_border_width_counts_only_when_the_style_draws_something() {
        let drawn = layout_style("<style>p { border: 4px solid #00f }</style><p>x", "p");
        assert_eq!(drawn.border.top.width, 4.0);
        assert_eq!(drawn.border.left.color, Color::from_rgb8(0, 0, 255));

        let absent = layout_style("<style>p { border: 4px none #00f }</style><p>x", "p");
        assert_eq!(absent.border.top.width, 0.0);
        assert!(!absent.border.top.is_visible());
    }

    /// A border with no colour of its own takes the element's, which is what makes
    /// `border: 1px solid` follow the text it frames.
    #[test]
    fn a_border_defaults_to_the_elements_own_colour() {
        let style = layout_style(
            "<style>p { color: #0a0; border: 1px solid }</style><p>x",
            "p",
        );
        assert_eq!(style.border.top.color, Color::from_rgb8(0, 170, 0));
    }

    #[test]
    fn text_align_narrows_to_the_three_a_line_box_can_honour() {
        let align = |css: &str| {
            layout_style(
                &format!("<style>p {{ text-align: {css} }}</style><p>x"),
                "p",
            )
            .text_align
        };
        assert_eq!(align("center"), TextAlign::Center);
        assert_eq!(align("right"), TextAlign::End);
        assert_eq!(align("left"), TextAlign::Start);
        // Justification spaces a line out, which inline layout does not do.
        assert_eq!(align("justify"), TextAlign::Start);
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
