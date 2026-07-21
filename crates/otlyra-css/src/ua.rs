//! The user-agent stylesheet, written in Rust.
//!
//! Every browser has one, and it is the reason an unstyled `<h1>` is large and bold
//! and a `<p>` has space around it. Ours is a `match` on the tag name rather than a
//! parsed stylesheet because parsing CSS is M8 and getting pixels out of HTML must
//! not wait for it. It is deliberately shaped like the stylesheet it will become:
//! one arm per selector, values in the same units CSS uses.
//!
//! Source for the values: the HTML standard's rendering section, which is what the
//! real UA stylesheets implement.

use std::sync::Arc;

use peniko::Color;

use crate::style::{
    ComputedStyle, Display, FontStyle, Length, LengthOrAuto, ListStyle, Sides, TextAlign,
    TextDecoration, VerticalAlign, WhiteSpace,
};

/// `1em` expressed against the parent font size.
fn em(style: &ComputedStyle, multiple: f32) -> LengthOrAuto {
    LengthOrAuto::Px(style.font_size * multiple)
}

/// The style of the root: the initial containing block's text and background.
pub fn initial_style() -> ComputedStyle {
    ComputedStyle {
        display: Display::Block,
        background_color: Color::from_rgb8(0xff, 0xff, 0xff),
        ..ComputedStyle::default()
    }
}

/// The UA style for `element`, inheriting from `parent`.
///
/// An element the table does not know becomes `inline`, which is what the HTML
/// standard says an unknown element is, and is why a page using tags we have never
/// heard of still shows its text.
pub fn ua_style(element: &str, parent: &ComputedStyle) -> ComputedStyle {
    let mut style = ComputedStyle::inheriting_from(parent);

    match element {
        // Not rendered at all.
        "head" | "title" | "meta" | "link" | "style" | "script" | "base" | "template"
        | "noscript" => style.display = Display::None,

        // The standard's `body { margin: 8px }`, which is why text does not touch
        // the window edge in any browser.
        "body" => {
            style.display = Display::Block;
            style.margin = Sides::all(LengthOrAuto::Px(8.0));
        }

        "html" | "div" | "center" | "section" | "article" | "aside" | "header" | "footer"
        | "main" | "nav" | "figcaption" | "form" | "dl" | "dt" => {
            style.display = Display::Block;
        }

        // A table is a formatting context of its own, and the parts are what tell
        // it apart from a stack of blocks.
        "table" => {
            style.display = Display::Table;
            style.border_spacing = (2.0, 2.0);
        }
        "thead" | "tbody" | "tfoot" => style.display = Display::TableRowGroup,
        "tr" => style.display = Display::TableRow,
        "caption" => {
            style.display = Display::TableCaption;
            style.text_align = TextAlign::Center;
        }
        "td" | "th" => {
            style.display = Display::TableCell;
            style.padding = Sides::all(Length::Px(1.0));
            if element == "th" {
                style.font_weight = 700;
                style.text_align = TextAlign::Center;
            }
        }

        "p" => {
            style.display = Display::Block;
            style.margin = Sides::axes(em(parent, 1.0), LengthOrAuto::Px(0.0));
        }

        "blockquote" => {
            style.display = Display::Block;
            style.margin = Sides {
                top: em(parent, 1.0),
                right: LengthOrAuto::Px(40.0),
                bottom: em(parent, 1.0),
                left: LengthOrAuto::Px(40.0),
            };
        }

        "ul" | "menu" | "ol" => {
            style.display = Display::Block;
            style.margin = Sides::axes(em(parent, 1.0), LengthOrAuto::Px(0.0));
            style.padding = Sides {
                left: Length::Px(40.0),
                ..Sides::all(Length::ZERO)
            };
            // Inherited, so the items read it without being told. A list inside a
            // list changes shape in the stylesheet, which has selectors; this table
            // does not, so here every level looks the same.
            style.list_style = if element == "ol" {
                ListStyle::Decimal
            } else {
                ListStyle::Disc
            };
        }

        "li" => style.display = Display::Block,

        "pre" => {
            style.display = Display::Block;
            style.font_family = Arc::from("monospace");
            style.white_space = WhiteSpace::Pre;
            style.margin = Sides::axes(em(parent, 1.0), LengthOrAuto::Px(0.0));
        }

        // Headings: the standard's own sizes, as multiples of the parent font size,
        // and the margins that go with them.
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let (scale, margin) = match element {
                "h1" => (2.00, 0.67),
                "h2" => (1.50, 0.83),
                "h3" => (1.17, 1.00),
                "h4" => (1.00, 1.33),
                "h5" => (0.83, 1.67),
                _ => (0.67, 2.33),
            };
            style.display = Display::Block;
            style.font_size = parent.font_size * scale;
            style.font_weight = 700;
            style.margin = Sides::axes(
                LengthOrAuto::Px(style.font_size * margin),
                LengthOrAuto::Px(0.0),
            );
        }

        "b" | "strong" => style.font_weight = 700,
        "i" | "em" | "cite" | "var" | "dfn" => style.font_style = FontStyle::Italic,

        // Block *and* italic, which is why it cannot live in either list above.
        "address" => {
            style.display = Display::Block;
            style.font_style = FontStyle::Italic;
        }

        // The standard renders a horizontal rule as a bordered block. Borders are
        // not implemented, so this stands in for one: the same line, drawn as a
        // background on a one-pixel-tall box. What it is is right; how it gets
        // there is not, and the difference shows the day `border` exists.
        "hr" => {
            style.display = Display::Block;
            style.height = LengthOrAuto::Px(1.0);
            style.background_color = Color::from_rgb8(0xc0, 0xc0, 0xc0);
            style.margin = Sides::axes(LengthOrAuto::Px(8.0), LengthOrAuto::Px(0.0));
        }

        // Blue and underlined, which between them are how a link says it is one
        // without relying on colour alone.
        "a" => {
            style.color = Color::from_rgb8(0x00, 0x00, 0xee);
            style.text_decoration = TextDecoration::UNDERLINE;
        }

        "u" | "ins" => style.text_decoration = TextDecoration::UNDERLINE,
        "s" | "strike" | "del" => style.text_decoration = TextDecoration::LINE_THROUGH,

        "code" | "kbd" | "samp" | "tt" => style.font_family = Arc::from("monospace"),

        "mark" => style.background_color = Color::from_rgb8(0xff, 0xff, 0x00),

        // Form controls. A real browser draws these as native widgets with a
        // border, a focus ring and a pressed state; we have none of that, so they
        // are text on a tinted background — enough to see that a control is there
        // and where it ends, and no claim to be more.
        "button" | "select" => {
            style.background_color = Color::from_rgb8(0xe6, 0xe6, 0xe8);
            style.font_size = parent.font_size * 0.95;
        }
        // A checkbox or radio is a glyph, not a field: tinting it makes the
        // marker harder to read rather than easier.
        "label" => {}
        "input" => {
            style.background_color = Color::from_rgb8(0xf4, 0xf4, 0xf6);
            style.font_size = parent.font_size * 0.95;
            // A field keeps its spacing: an empty one is sized by the space it
            // reserves, and collapsing that away leaves a field a pixel wide.
            style.white_space = WhiteSpace::Pre;
        }
        "textarea" => {
            style.display = Display::Block;
            style.background_color = Color::from_rgb8(0xf4, 0xf4, 0xf6);
            style.font_family = Arc::from("monospace");
            style.white_space = WhiteSpace::Pre;
        }
        // Present in the standard's stylesheet and cheap to be right about.
        "details" | "summary" | "dialog" | "figure" | "hgroup" | "search" => {
            style.display = Display::Block;
        }
        "fieldset" => {
            style.display = Display::Block;
            style.padding = Sides::all(Length::Px(8.0));
            style.margin = Sides::axes(LengthOrAuto::Px(0.0), LengthOrAuto::Px(2.0));
        }

        // `dd` is indented; `dt` is not. The standard says 40px, and it is the one
        // indent people notice the absence of.
        "dd" => {
            style.display = Display::Block;
            style.margin = Sides {
                left: LengthOrAuto::Px(40.0),
                ..Sides::all(LengthOrAuto::Px(0.0))
            };
        }

        // Smaller, and raised or lowered.
        "sub" | "sup" => {
            style.font_size = parent.font_size * 0.83;
            style.vertical_align = if element == "sub" {
                VerticalAlign::Sub
            } else {
                VerticalAlign::Super
            };
        }

        "small" => style.font_size = parent.font_size * 0.83,

        // Everything else, including elements invented since this was written.
        _ => style.display = Display::Inline,
    }

    style
}

/// Whether an element's *children* are rendered at all.
///
/// Separate from `display: none` because the reason differs: a `<script>`'s text is
/// program source, not content, and no styling can make it text on the page.
pub fn has_renderable_children(element: &str) -> bool {
    !matches!(
        element,
        "script" | "style" | "template" | "noscript" | "iframe" | "object"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> ComputedStyle {
        initial_style()
    }

    #[test]
    fn the_table_assigns_the_displays_the_standard_does() {
        let parent = root();
        assert_eq!(ua_style("div", &parent).display, Display::Block);
        assert_eq!(ua_style("p", &parent).display, Display::Block);
        assert_eq!(ua_style("span", &parent).display, Display::Inline);
        assert_eq!(ua_style("a", &parent).display, Display::Inline);
        assert_eq!(ua_style("head", &parent).display, Display::None);
        assert_eq!(ua_style("script", &parent).display, Display::None);
    }

    #[test]
    fn an_unknown_element_is_inline() {
        let style = ua_style("some-web-component", &root());
        assert_eq!(style.display, Display::Inline);
    }

    #[test]
    fn headings_are_larger_and_bolder_than_their_parent() {
        let parent = root();
        let h1 = ua_style("h1", &parent);
        let h6 = ua_style("h6", &parent);

        assert_eq!(h1.font_size, 32.0);
        assert_eq!(h1.font_weight, 700);
        assert!(h6.font_size < parent.font_size);
        assert_eq!(h6.font_weight, 700);
    }

    /// Heading sizes are relative, so a heading inside a larger context is larger.
    #[test]
    fn font_sizes_compose_through_inheritance() {
        let parent = ComputedStyle {
            font_size: 20.0,
            ..root()
        };
        assert_eq!(ua_style("h1", &parent).font_size, 40.0);

        let big = ua_style("h1", &parent);
        assert_eq!(
            ua_style("span", &big).font_size,
            40.0,
            "an inline inside a heading inherits its size"
        );
    }

    #[test]
    fn paragraph_margins_are_one_em_of_the_parent() {
        let parent = ComputedStyle {
            font_size: 16.0,
            ..root()
        };
        let paragraph = ua_style("p", &parent);
        assert_eq!(paragraph.margin.top, LengthOrAuto::Px(16.0));
        assert_eq!(paragraph.margin.left, LengthOrAuto::Px(0.0));
    }

    #[test]
    fn links_are_blue_and_bold_elements_are_bold() {
        let parent = root();
        assert_eq!(ua_style("a", &parent).color, Color::from_rgb8(0, 0, 0xee));
        assert_eq!(ua_style("strong", &parent).font_weight, 700);
        assert_eq!(ua_style("em", &parent).font_weight, 400);
    }

    #[test]
    fn script_and_style_children_are_never_content() {
        assert!(!has_renderable_children("script"));
        assert!(!has_renderable_children("style"));
        assert!(has_renderable_children("div"));
    }
}
