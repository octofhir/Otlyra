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
    /// A flex container: block-level outside, and its children are flex items
    /// rather than a block or inline formatting context.
    Flex,
    /// A grid container: its children are placed into rows and columns.
    Grid,
}

/// `flex-direction`, narrowed to the axis and whether it is reversed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlexDirection {
    /// Along the inline axis.
    Row,
    /// Along the inline axis, from the end.
    RowReverse,
    /// Down the block axis.
    Column,
    /// Up the block axis.
    ColumnReverse,
}

impl FlexDirection {
    /// Whether the main axis is horizontal.
    pub fn is_row(self) -> bool {
        matches!(self, Self::Row | Self::RowReverse)
    }

    /// Whether items are placed from the far end of the main axis.
    pub fn is_reverse(self) -> bool {
        matches!(self, Self::RowReverse | Self::ColumnReverse)
    }
}

/// How the leftover main-axis space is shared out.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum JustifyContent {
    /// All of it after the items.
    Start,
    /// All of it before them.
    End,
    /// Half before, half after.
    Center,
    /// Between them, none at the ends.
    SpaceBetween,
    /// Between them and half as much at each end.
    SpaceAround,
    /// Equally between them and at the ends.
    SpaceEvenly,
}

/// How items are placed across the cross axis.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AlignItems {
    /// At the start edge.
    Start,
    /// At the end edge.
    End,
    /// Centred.
    Center,
    /// Filling the line, which is what makes columns of equal height.
    Stretch,
    /// On their first baselines. Not implemented, and laid out as `start`.
    Baseline,
}

/// `flex-wrap`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlexWrap {
    /// One line, however much it overflows.
    NoWrap,
    /// As many lines as the items need.
    Wrap,
    /// As many lines, stacked the other way.
    WrapReverse,
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

/// One border: how wide it is drawn and what colour.
///
/// `border-style` is not a field. A border is either drawn or it is not, and the
/// styles that are not `solid` — dashed, dotted, ridge — are a painting difference
/// this renderer does not make yet. What it must not get wrong is the arithmetic:
/// a `none` or `hidden` border is zero wide, and that changes where content sits.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Border {
    /// The used width in CSS pixels — zero when the style makes the border absent.
    pub width: f32,
    /// `border-*-color`, which defaults to the element's own `color`.
    pub color: Color,
}

impl Border {
    /// No border.
    pub const NONE: Self = Self {
        width: 0.0,
        color: Color::TRANSPARENT,
    };

    /// Whether this border puts anything on the screen.
    pub fn is_visible(self) -> bool {
        self.width > 0.0 && self.color.components[3] > 0.0
    }
}

/// `text-align`, in the values a block formatting context can honour.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextAlign {
    /// The start edge — left, in the writing direction we support.
    Start,
    /// Centred in the content box.
    Center,
    /// The end edge.
    End,
}

/// `background-size`, in the three shapes that mean something without a full
/// two-value model behind them.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum BackgroundSize {
    /// The picture's own size.
    Auto,
    /// As large as fits inside the box, whole.
    Contain,
    /// As small as covers the box, cropped.
    Cover,
    /// A size of its own.
    Fixed(Length, Length),
}

/// One `box-shadow`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Shadow {
    /// How far right it is offset.
    pub x: f32,
    /// How far down.
    pub y: f32,
    /// The CSS blur radius: how far the edge is spread, not its deviation.
    pub blur: f32,
    /// How much larger than the box the shadow is drawn.
    pub spread: f32,
    /// Its colour.
    pub color: Color,
}

/// A colour at a point along a gradient.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GradientStop {
    /// Where along the line it sits, 0 to 1.
    pub at: f32,
    /// What colour it is there.
    pub color: Color,
}

/// A background that is a gradient rather than a colour.
///
/// Linear only, and the direction is kept as the angle CSS gives it: zero points
/// up the page, and it turns clockwise, which is the one convention CSS does not
/// share with the geometry underneath.
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    /// The angle in radians, clockwise from pointing up.
    pub angle: f32,
    /// The stops, in order.
    pub stops: Vec<GradientStop>,
}

/// Where a grid item sits along one axis.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placement {
    /// The line it starts on, counting from one, or `None` for wherever it lands.
    pub line: Option<i32>,
    /// How many tracks it covers.
    pub span: u32,
}

impl Placement {
    /// Placed wherever the auto-placement gets to, one track wide.
    pub const AUTO: Self = Self {
        line: None,
        span: 1,
    };
}

/// One track of a grid: a column's width or a row's height.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Track {
    /// A length or a percentage of the container.
    Fixed(Length),
    /// A share of what is left over, in `fr`.
    Fraction(f32),
    /// As big as its contents need.
    Auto,
}

/// The four corner radii of a box, in CSS order.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Corners {
    /// Top left.
    pub top_left: Length,
    /// Top right.
    pub top_right: Length,
    /// Bottom right.
    pub bottom_right: Length,
    /// Bottom left.
    pub bottom_left: Length,
}

impl Corners {
    /// No rounding at all.
    pub const SQUARE: Self = Self {
        top_left: Length::ZERO,
        top_right: Length::ZERO,
        bottom_right: Length::ZERO,
        bottom_left: Length::ZERO,
    };

    /// Whether any corner is rounded.
    pub fn any(&self) -> bool {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
        .iter()
        .any(|corner| *corner != Length::ZERO)
    }
}

/// `overflow`, in the distinction layout can act on: whether content that does not
/// fit is shown or cut off.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// Content spills out of the box and is drawn.
    Visible,
    /// Content is cut off at the box's padding edge.
    Clip,
}

/// `position`, which decides what a box's coordinates mean.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Position {
    /// In the flow, at the place the flow puts it.
    Static,
    /// In the flow, and then moved by its insets without moving anything else.
    Relative,
    /// Out of the flow, placed against the nearest positioned ancestor.
    Absolute,
    /// Out of the flow, placed against the viewport and not scrolled with the page.
    Fixed,
    /// In the flow until the page scrolls it to its inset, and then held there.
    Sticky,
}

impl Position {
    /// Whether a box with this `position` is taken out of the flow.
    pub fn is_out_of_flow(self) -> bool {
        matches!(self, Self::Absolute | Self::Fixed)
    }

    /// Whether a box with this `position` is a containing block for the absolutely
    /// positioned boxes inside it.
    pub fn is_containing_block(self) -> bool {
        !matches!(self, Self::Static)
    }
}

/// `float`, which takes a box out of the flow and puts it against an edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Float {
    /// In the flow, like everything else.
    None,
    /// Against the start edge, with the lines beside it shortened.
    Left,
    /// Against the end edge.
    Right,
}

/// `clear`, which pushes a box past the floats it names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Clear {
    /// Nothing to clear.
    None,
    /// Past the bottom of every left float.
    Left,
    /// Past every right float.
    Right,
    /// Past both.
    Both,
}

/// `white-space`, in the two values that matter before CSS parsing exists.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WhiteSpace {
    /// Runs of whitespace collapse to one space and lines wrap.
    Normal,
    /// Whitespace and newlines are kept exactly, and lines do not wrap.
    Pre,
}

/// `text-decoration-line`, as the flags it is.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TextDecoration {
    /// A line below the text.
    pub underline: bool,
    /// A line through it.
    pub line_through: bool,
}

impl TextDecoration {
    /// No decoration at all — the initial value.
    pub const NONE: Self = Self {
        underline: false,
        line_through: false,
    };
    /// `underline`.
    pub const UNDERLINE: Self = Self {
        underline: true,
        line_through: false,
    };
    /// `line-through`.
    pub const LINE_THROUGH: Self = Self {
        underline: false,
        line_through: true,
    };

    /// Whether anything is drawn.
    pub fn is_none(self) -> bool {
        !self.underline && !self.line_through
    }
}

/// `font-style`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FontStyle {
    /// Upright.
    #[default]
    Normal,
    /// Italic, or oblique where the family has no italic face.
    Italic,
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
    /// `text-shadow`. Inherited, and drawn behind the text rather than behind the
    /// box, which is why it is a list of its own.
    pub text_shadows: Vec<Shadow>,
    /// `box-shadow`, outermost last — the order they are painted in, which is the
    /// reverse of the order they are written.
    pub shadows: Vec<Shadow>,
    /// `background-image`, when it is a picture: the address, exactly as written.
    /// Resolving and fetching it is the caller's, as it is for a stylesheet.
    pub background_image: Option<Arc<str>>,
    /// How a background picture is sized against its box.
    pub background_size: BackgroundSize,
    /// Whether a background picture is tiled.
    pub background_repeat: bool,
    /// `background-image`, when it is a gradient. A picture behind a box is not
    /// read yet; a gradient is, because it is what a page uses it for.
    pub background_gradient: Option<Gradient>,
    /// `font-family`, as the CSS source list. Inherited.
    pub font_family: Arc<str>,
    /// `font-size` in CSS pixels. Inherited.
    pub font_size: f32,
    /// `font-weight`, 100–900. Inherited.
    pub font_weight: u16,
    /// `font-style`. Inherited.
    pub font_style: FontStyle,
    /// `line-height`. Inherited.
    pub line_height: LineHeight,
    /// `margin`.
    pub margin: Sides<LengthOrAuto>,
    /// `padding`.
    pub padding: Sides<Length>,
    /// `border-*-width` and `border-*-color`, resolved together.
    pub border: Sides<Border>,
    /// `text-align`. Inherited.
    pub text_align: TextAlign,
    /// `white-space`. Inherited.
    pub white_space: WhiteSpace,
    /// `text-decoration-line`.
    ///
    /// Not inherited in CSS — it *propagates*, which is a different thing: a
    /// descendant cannot turn its ancestor's underline off. Propagating it as
    /// inheritance is the approximation here, and it differs only for a case we
    /// cannot express yet (`text-decoration: none` on a child).
    pub text_decoration: TextDecoration,
    /// `width`.
    pub width: LengthOrAuto,
    /// `height`.
    pub height: LengthOrAuto,
    /// `min-width`, which floors whatever `width` resolves to.
    pub min_width: Length,
    /// `max-width`, or `None` for `none`. This is what holds a page's text column
    /// to a readable measure, so it is the one of the four that shows on nearly
    /// every real page.
    pub max_width: Option<Length>,
    /// `min-height`.
    pub min_height: Length,
    /// `max-height`, or `None` for `none`.
    pub max_height: Option<Length>,
    /// `float`.
    pub float: Float,
    /// `clear`.
    pub clear: Clear,
    /// `position`.
    pub position: Position,
    /// `top`, `right`, `bottom` and `left`, which only a positioned box reads.
    pub inset: Sides<LengthOrAuto>,
    /// `z-index`, or `None` for `auto`. Only a positioned box reads it.
    pub z_index: Option<i32>,
    /// `overflow`, as the one thing layout does about it.
    pub overflow: Overflow,
    /// `border-radius`, per corner. Only the horizontal radius of each: an ellipse
    /// with two different radii is a corner nobody writes.
    pub radius: Corners,
    /// `grid-template-columns`, with `repeat()` of a definite count expanded.
    pub grid_columns: Vec<Track>,
    /// `grid-template-rows`, the same.
    pub grid_rows: Vec<Track>,
    /// The pattern of `repeat(auto-fill, ...)` in the columns, if there is one: how
    /// many times it goes in depends on the container, so layout decides.
    pub grid_columns_fill: Option<Vec<Track>>,
    /// `grid-column`, read by an item rather than by the container.
    pub grid_column: Placement,
    /// `grid-row`.
    pub grid_row: Placement,
    /// `flex-direction`, read by a flex container.
    pub flex_direction: FlexDirection,
    /// `flex-wrap`.
    pub flex_wrap: FlexWrap,
    /// `justify-content`, along the main axis.
    pub justify_content: JustifyContent,
    /// `align-items`, across it.
    pub align_items: AlignItems,
    /// `align-self`, which overrides the container's `align-items` for one item.
    /// `None` is `auto`: take the container's.
    pub align_self: Option<AlignItems>,
    /// `flex-grow`, read by a flex item.
    pub flex_grow: f32,
    /// `flex-shrink`.
    pub flex_shrink: f32,
    /// `flex-basis`, or `None` for `auto` — take the item's own size.
    pub flex_basis: Option<LengthOrAuto>,
    /// `row-gap` and `column-gap`, which a flex container puts between its items.
    pub gap: (Length, Length),
}

/// The initial values, as CSS defines them, with the UA's font defaults.
pub const DEFAULT_FONT_SIZE: f32 = 16.0;

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::Inline,
            color: Color::from_rgb8(0, 0, 0),
            background_color: Color::TRANSPARENT,
            background_gradient: None,
            background_image: None,
            background_size: BackgroundSize::Auto,
            background_repeat: true,
            shadows: Vec::new(),
            text_shadows: Vec::new(),
            font_family: Arc::from("system-ui, sans-serif"),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: 400,
            font_style: FontStyle::Normal,
            line_height: LineHeight::Normal,
            white_space: WhiteSpace::Normal,
            text_decoration: TextDecoration::NONE,
            margin: Sides::all(LengthOrAuto::Px(0.0)),
            padding: Sides::all(Length::ZERO),
            border: Sides::all(Border::NONE),
            text_align: TextAlign::Start,
            width: LengthOrAuto::Auto,
            height: LengthOrAuto::Auto,
            min_width: Length::ZERO,
            max_width: None,
            min_height: Length::ZERO,
            max_height: None,
            float: Float::None,
            clear: Clear::None,
            position: Position::Static,
            inset: Sides::all(LengthOrAuto::Auto),
            z_index: None,
            overflow: Overflow::Visible,
            radius: Corners::SQUARE,
            grid_columns: Vec::new(),
            grid_rows: Vec::new(),
            grid_columns_fill: None,
            grid_column: Placement::AUTO,
            grid_row: Placement::AUTO,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            justify_content: JustifyContent::Start,
            align_items: AlignItems::Stretch,
            align_self: None,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            gap: (Length::ZERO, Length::ZERO),
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
            text_shadows: parent.text_shadows.clone(),
            color: parent.color,
            font_family: Arc::clone(&parent.font_family),
            font_size: parent.font_size,
            font_weight: parent.font_weight,
            font_style: parent.font_style,
            line_height: parent.line_height,
            white_space: parent.white_space,
            text_decoration: parent.text_decoration,
            text_align: parent.text_align,
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
