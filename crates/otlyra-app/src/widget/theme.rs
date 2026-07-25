//! Every colour and measurement the interface draws with, in one place.
//!
//! Two reasons this is a module and not a scatter of constants next to the
//! controls that use them. A control that names its own grey is a control that
//! drifts from the one beside it, and there is no way to see that it has: the
//! evidence is spread over a thousand lines. And the interface has more than one
//! surface — the toolbar, and the settings page, which is drawn with these same
//! controls rather than with HTML — so *what a pressed button looks like* has to
//! be one answer, given once.
//!
//! Colours are opaque unless the name says otherwise. The three that are not are
//! the hover and press washes, which sit over whatever is beneath them, and the
//! focus ring's halo. They are alpha rather than a mixed opaque value because
//! what is beneath them differs: the same wash goes over the toolbar, over a
//! tab, and over a white field.

use otlyra_gfx::peniko::Color;

/// The palette and the metrics the interface is drawn from.
///
/// Passed by reference rather than read from a global, so a second theme is a
/// value to construct and not a mode to switch the process into. Dark mode is
/// exactly that: another `Theme`, and no change to any control.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Behind the toolbar and the tab strip.
    pub surface: Color,
    /// Behind a raised thing on the surface: the active tab, a field, a card.
    pub raised: Color,
    /// Behind a whole surface that is itself a background for cards — the
    /// settings page, where the cards are the raised things and the page is not.
    pub surface_sunken: Color,
    /// The line between the interface and the page, and between grouped rows.
    pub hairline: Color,
    /// The line around a field or an outlined control.
    pub border: Color,

    /// Text, and marks drawn as paths.
    pub ink: Color,
    /// Text that is secondary: a placeholder, a URL's path, a hint under a row.
    pub ink_dim: Color,
    /// Text and marks on a control that will not respond.
    pub ink_disabled: Color,
    /// Text on top of [`Theme::accent`].
    pub ink_on_accent: Color,

    /// What the interface points at itself with: focus, selection, the toggle
    /// that is on.
    pub accent: Color,
    /// The focus ring's halo, which sits over whatever surrounds the field.
    pub accent_halo: Color,
    /// A warning, and the reload button while a load is failing.
    pub danger: Color,

    /// A wash over anything the pointer is inside.
    pub hover: Color,
    /// A stronger wash, for the thing being pressed.
    pub press: Color,
    /// Behind the row of a list or a tree that is the chosen one.
    ///
    /// A wash rather than the accent itself: a selected row still has to be read,
    /// and the text on it is the ordinary ink.
    pub selection: Color,

    /// A tag name, in text that is code.
    pub code_tag: Color,
    /// An attribute's name.
    pub code_name: Color,
    /// An attribute's value, and a string.
    pub code_value: Color,

    /// The four shades a box is taken apart into, outermost first.
    ///
    /// Here rather than beside the inspector for the same reason every other
    /// colour is: one place, or the interface drifts a shade at a time. They are
    /// translucent because they are drawn over a page nobody wrote for them.
    pub box_margin: Color,
    /// The border ring of a box under inspection.
    pub box_border: Color,
    /// Its padding.
    pub box_padding: Color,
    /// Its content.
    pub box_content: Color,
    /// The dashed lines a grid's or a flex container's tracks are drawn with,
    /// and the tabs its line numbers sit on.
    pub grid_line: Color,

    /// Corner radius on a button, a field, a card.
    pub radius: f64,
    /// Corner radius on a small square button — one holding a single mark.
    pub radius_small: f64,
    /// Corner radius on a tab's two top corners.
    pub radius_tab: f64,

    /// Interface text.
    pub font_size: f32,
    /// Text that is deliberately smaller: a hint, a badge.
    pub font_size_small: f32,
    /// Text that is code: source, a selector, a tag name.
    pub font_size_mono: f32,
    /// The families code is drawn in, as a CSS list.
    ///
    /// A string parsed into a stack rather than one name, because the interface
    /// cannot know which of these a machine has, and a family it does not have
    /// is a row of hollow boxes where a tag name should be.
    pub mono: &'static str,
    /// The line box a single line of interface text occupies, as a multiple of
    /// the font size.
    pub line_height: f64,

    /// One row of a tree or a table.
    ///
    /// Fixed, and that is what makes a long list cheap: which row a point is in
    /// is arithmetic, and which rows are on screen is arithmetic, so a tree of
    /// ten thousand nodes measures and draws the twenty that are visible.
    pub row_height: f64,
    /// The side of a square button holding one mark.
    pub control_size: f64,
    /// The height of a field, or of a button with a label in it.
    pub control_height: f64,
    /// The space between controls that belong together.
    pub gap: f64,
    /// The space from the edge of a surface to what is on it.
    pub inset: f64,
    /// The width of a hairline, before the device scale is applied.
    pub hairline_width: f64,
}

impl Theme {
    /// The interface as it is drawn in a light environment.
    ///
    /// The greys are near-neutral with a trace of blue, which is what stops a
    /// large flat surface reading as dirty next to white page content.
    pub fn light() -> Self {
        Self {
            surface: Color::from_rgb8(0xe4, 0xe4, 0xea),
            raised: Color::from_rgb8(0xff, 0xff, 0xff),
            surface_sunken: Color::from_rgb8(0xf4, 0xf4, 0xf7),
            hairline: Color::from_rgb8(0xcf, 0xcf, 0xd7),
            border: Color::from_rgb8(0xd5, 0xd5, 0xdd),

            ink: Color::from_rgb8(0x1c, 0x1c, 0x21),
            ink_dim: Color::from_rgb8(0x6e, 0x6e, 0x78),
            ink_disabled: Color::from_rgb8(0xb4, 0xb4, 0xbd),
            ink_on_accent: Color::from_rgb8(0xff, 0xff, 0xff),

            accent: Color::from_rgb8(0x2f, 0x6f, 0xd6),
            accent_halo: Color::from_rgba8(0x2f, 0x6f, 0xd6, 0x38),
            danger: Color::from_rgb8(0xc0, 0x36, 0x2c),

            hover: Color::from_rgba8(0x1c, 0x1c, 0x21, 0x14),
            press: Color::from_rgba8(0x1c, 0x1c, 0x21, 0x28),
            selection: Color::from_rgba8(0x2f, 0x6f, 0xd6, 0x2e),

            code_tag: Color::from_rgb8(0x8b, 0x1a, 0x8b),
            code_name: Color::from_rgb8(0xa8, 0x5c, 0x00),
            code_value: Color::from_rgb8(0x1a, 0x63, 0x2e),

            // The shades every browser's inspector has used for twenty years.
            // Familiarity is the whole point: an overlay that had to be learned
            // would be one more thing between a person and their bug.
            box_margin: Color::from_rgba8(0xf6, 0xb7, 0x3c, 0x66),
            box_border: Color::from_rgba8(0xd6, 0xa0, 0x6a, 0x80),
            box_padding: Color::from_rgba8(0x8b, 0xc4, 0x6a, 0x66),
            box_content: Color::from_rgba8(0x6b, 0xa8, 0xd6, 0x66),
            grid_line: Color::from_rgba8(0x9a, 0x3c, 0xc4, 0xcc),

            radius: 8.0,
            radius_small: 7.0,
            radius_tab: 9.0,

            font_size: 13.0,
            font_size_small: 11.0,
            font_size_mono: 11.5,
            mono: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
            line_height: 1.35,

            row_height: 18.0,
            control_size: 28.0,
            control_height: 30.0,
            gap: 6.0,
            inset: 8.0,
            hairline_width: 1.0,
        }
    }

    /// The interface as it is drawn in a dark environment.
    ///
    /// The same trace of blue in the greys, for the same reason. The accent is
    /// lighter than the light theme's, because the same blue that clears the
    /// contrast floor on white fails it on near-black; the washes are white
    /// rather than black, because a wash has to differ from what it sits on.
    /// Every metric is the light theme's: dark is a palette, not a layout.
    pub fn dark() -> Self {
        Self {
            surface: Color::from_rgb8(0x1e, 0x1e, 0x24),
            raised: Color::from_rgb8(0x2d, 0x2d, 0x35),
            surface_sunken: Color::from_rgb8(0x26, 0x26, 0x2d),
            hairline: Color::from_rgb8(0x3c, 0x3c, 0x45),
            border: Color::from_rgb8(0x47, 0x47, 0x51),

            ink: Color::from_rgb8(0xe9, 0xe9, 0xee),
            ink_dim: Color::from_rgb8(0xa4, 0xa4, 0xb0),
            ink_disabled: Color::from_rgb8(0x5e, 0x5e, 0x68),
            // Dark on the accent, not white: the accent is lighter here than in
            // the light theme, and white on it fails the floor white clears
            // there.
            ink_on_accent: Color::from_rgb8(0x14, 0x18, 0x20),

            accent: Color::from_rgb8(0x6a, 0x9d, 0xe8),
            accent_halo: Color::from_rgba8(0x6a, 0x9d, 0xe8, 0x38),
            danger: Color::from_rgb8(0xe0, 0x60, 0x55),

            hover: Color::from_rgba8(0xff, 0xff, 0xff, 0x14),
            press: Color::from_rgba8(0xff, 0xff, 0xff, 0x28),
            selection: Color::from_rgba8(0x6a, 0x9d, 0xe8, 0x3c),

            code_tag: Color::from_rgb8(0xd8, 0x93, 0xd8),
            code_name: Color::from_rgb8(0xdf, 0xb2, 0x6d),
            code_value: Color::from_rgb8(0x92, 0xd0, 0xa5),

            // The box overlays and grid lines are translucent washes over a
            // page nobody wrote for them, and they read on dark pages already;
            // changing them per theme would make the inspector's vocabulary
            // depend on the toolbar's palette.
            ..Self::light()
        }
    }

    /// Nothing, in the theme's own units: a colour that paints no pixels.
    ///
    /// Used where a control has a background only sometimes — an icon button is
    /// bare until the pointer reaches it — so that *bare* is a colour like any
    /// other rather than an `Option` threaded through every constructor.
    pub const CLEAR: Color = Color::from_rgba8(0, 0, 0, 0);
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of an opaque colour.
    fn luminance(color: Color) -> f64 {
        let linear = |channel: f32| {
            let channel = f64::from(channel);
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        let [r, g, b, _] = color.components;
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    /// WCAG contrast ratio, 1 to 21.
    fn contrast(a: Color, b: Color) -> f64 {
        let (lighter, darker) = {
            let (a, b) = (luminance(a), luminance(b));
            (a.max(b), a.min(b))
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Every colour meets a floor against the surface it is drawn on.
    ///
    /// Table-driven, and over both themes, because a palette drifts one shade
    /// at a time: no single change looks like the one that made a label
    /// unreadable. The floors are what the light theme establishes — body text
    /// at the AA level, secondary and iconography at the large-text level —
    /// and dark has to clear the same bar, not its own.
    #[test]
    fn every_ink_clears_its_contrast_floor_in_both_themes() {
        for (name, theme) in [("light", Theme::light()), ("dark", Theme::dark())] {
            let table: [(&str, Color, Color, f64); 12] = [
                ("ink on surface", theme.ink, theme.surface, 7.0),
                ("ink on raised", theme.ink, theme.raised, 7.0),
                ("ink on sunken", theme.ink, theme.surface_sunken, 7.0),
                ("dim ink on surface", theme.ink_dim, theme.surface, 3.5),
                ("dim ink on raised", theme.ink_dim, theme.raised, 3.5),
                (
                    "dim ink on sunken",
                    theme.ink_dim,
                    theme.surface_sunken,
                    3.5,
                ),
                ("ink on accent", theme.ink_on_accent, theme.accent, 3.0),
                ("accent on surface", theme.accent, theme.surface, 3.0),
                ("accent on raised", theme.accent, theme.raised, 3.0),
                ("danger on raised", theme.danger, theme.raised, 3.0),
                ("code tag on raised", theme.code_tag, theme.raised, 3.5),
                ("code value on raised", theme.code_value, theme.raised, 3.5),
            ];
            for (what, ink, on, floor) in table {
                let ratio = contrast(ink, on);
                assert!(
                    ratio >= floor,
                    "{name}: {what} is {ratio:.2}, below the {floor} floor"
                );
            }
        }
    }

    /// The washes have to be washes: translucent, or they would erase what
    /// they sit on instead of tinting it.
    #[test]
    fn the_washes_stay_translucent_in_both_themes() {
        for theme in [Theme::light(), Theme::dark()] {
            for wash in [theme.hover, theme.press, theme.selection, theme.accent_halo] {
                assert!(wash.components[3] < 0.5, "a wash more than half opaque");
            }
        }
    }
}
