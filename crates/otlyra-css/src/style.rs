//! Computed values: what an element's style is once every question is answered.

use std::sync::Arc;

use peniko::Color;

/// The `display` values we model.
///
/// Three, not thirty. `inline-block`, `flex`, `grid` and the table displays each
/// bring a formatting context with them, and a formatting context we cannot lay out
/// is a value we would have to lie about.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Display {
    /// Generates no box at all, and neither do its descendants.
    None,
    /// Block-level: takes a whole line, participates in a block formatting context.
    Block,
    /// Inline-level: flows in a line box.
    Inline,
}

/// A length, or `auto`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LengthOrAuto {
    /// An absolute length in CSS pixels.
    Px(f32),
    /// A fraction of the containing block, 0–1 rather than 0–100.
    Percent(f32),
    /// `auto`: the used value is worked out during layout.
    Auto,
}

impl LengthOrAuto {
    /// Resolve against a containing-block size, or `None` for `auto`.
    pub fn resolve(self, containing: f32) -> Option<f32> {
        match self {
            Self::Px(px) => Some(px),
            Self::Percent(fraction) => Some(fraction * containing),
            Self::Auto => None,
        }
    }
}

/// A length that cannot be `auto` — padding and borders.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Length {
    /// An absolute length in CSS pixels.
    Px(f32),
    /// A fraction of the containing block's *width*, as CSS requires even
    /// vertically.
    Percent(f32),
}

impl Length {
    /// Resolve against a containing-block size.
    pub fn resolve(self, containing: f32) -> f32 {
        match self {
            Self::Px(px) => px,
            Self::Percent(fraction) => fraction * containing,
        }
    }

    /// Zero.
    pub const ZERO: Self = Self::Px(0.0);
}

/// The four sides of a box, in CSS order.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Sides<T> {
    /// Top.
    pub top: T,
    /// Right.
    pub right: T,
    /// Bottom.
    pub bottom: T,
    /// Left.
    pub left: T,
}

impl<T: Copy> Sides<T> {
    /// The same value on all four sides.
    pub const fn all(value: T) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    /// Vertical and horizontal, as the two-value CSS shorthand.
    pub const fn axes(vertical: T, horizontal: T) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }
}

/// `line-height`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LineHeight {
    /// `normal`: the font's own line spacing.
    Normal,
    /// A multiple of the font size — the value that inherits as a number, not a
    /// length, and so means something different in each descendant.
    Number(f32),
    /// An absolute length in CSS pixels.
    Px(f32),
}

impl LineHeight {
    /// Resolve against a font size, given the font's natural line spacing.
    pub fn resolve(self, font_size: f32, natural: f32) -> f32 {
        match self {
            Self::Normal => natural,
            Self::Number(factor) => factor * font_size,
            Self::Px(px) => px,
        }
    }
}

/// The computed style of one element.
///
/// Exactly the properties the plan names for this milestone, and no more. Every
/// property added here is one the cascade, the box tree, layout and paint all have
/// to keep honest, and one that is easy to add later and awkward to remove.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStyle {
    /// `display`.
    pub display: Display,
    /// `color`. Inherited.
    pub color: Color,
    /// `background-color`.
    pub background_color: Color,
    /// `font-family`, as the CSS source list. Inherited.
    pub font_family: Arc<str>,
    /// `font-size` in CSS pixels. Inherited.
    pub font_size: f32,
    /// `font-weight`, 100–900. Inherited.
    pub font_weight: u16,
    /// `line-height`. Inherited.
    pub line_height: LineHeight,
    /// `margin`.
    pub margin: Sides<LengthOrAuto>,
    /// `padding`.
    pub padding: Sides<Length>,
    /// `width`.
    pub width: LengthOrAuto,
    /// `height`.
    pub height: LengthOrAuto,
}

/// The initial values, as CSS defines them, with the UA's font defaults.
pub const DEFAULT_FONT_SIZE: f32 = 16.0;

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::Inline,
            color: Color::from_rgb8(0, 0, 0),
            background_color: Color::TRANSPARENT,
            font_family: Arc::from("system-ui, sans-serif"),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: 400,
            line_height: LineHeight::Normal,
            margin: Sides::all(LengthOrAuto::Px(0.0)),
            padding: Sides::all(Length::ZERO),
            width: LengthOrAuto::Auto,
            height: LengthOrAuto::Auto,
        }
    }
}

impl ComputedStyle {
    /// A style that inherits from `parent` everything CSS says is inherited, and
    /// takes the initial value for everything else.
    ///
    /// This is the whole of inheritance for now: there is no cascade to inherit
    /// *through* until M8, but a heading inside a body still has to know what
    /// colour and font it sits in.
    pub fn inheriting_from(parent: &Self) -> Self {
        Self {
            color: parent.color,
            font_family: Arc::clone(&parent.font_family),
            font_size: parent.font_size,
            font_weight: parent.font_weight,
            line_height: parent.line_height,
            ..Self::default()
        }
    }

    /// Whether this style generates a block-level box.
    pub fn is_block_level(&self) -> bool {
        self.display == Display::Block
    }

    /// The used `line-height`, given the font's natural spacing.
    pub fn used_line_height(&self, natural: f32) -> f32 {
        self.line_height.resolve(self.font_size, natural)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inheritance_carries_the_inherited_properties_and_nothing_else() {
        let parent = ComputedStyle {
            color: Color::from_rgb8(1, 2, 3),
            font_size: 24.0,
            display: Display::Block,
            margin: Sides::all(LengthOrAuto::Px(10.0)),
            ..ComputedStyle::default()
        };

        let child = ComputedStyle::inheriting_from(&parent);
        assert_eq!(child.color, parent.color);
        assert_eq!(child.font_size, 24.0);
        assert_eq!(child.display, Display::Inline, "display does not inherit");
        assert_eq!(
            child.margin.top,
            LengthOrAuto::Px(0.0),
            "margin does not inherit"
        );
    }

    #[test]
    fn line_height_number_scales_with_the_font_size_it_lands_on() {
        let height = LineHeight::Number(1.5);
        assert_eq!(height.resolve(16.0, 18.0), 24.0);
        assert_eq!(height.resolve(32.0, 36.0), 48.0);
        assert_eq!(LineHeight::Normal.resolve(16.0, 18.4), 18.4);
        assert_eq!(LineHeight::Px(20.0).resolve(16.0, 18.0), 20.0);
    }

    #[test]
    fn percentages_resolve_against_the_containing_block() {
        assert_eq!(LengthOrAuto::Percent(0.5).resolve(200.0), Some(100.0));
        assert_eq!(LengthOrAuto::Px(30.0).resolve(200.0), Some(30.0));
        assert_eq!(LengthOrAuto::Auto.resolve(200.0), None);
        assert_eq!(Length::Percent(0.25).resolve(200.0), 50.0);
    }
}
