//! Computed values: what an element's style is once every question is answered.

use std::fmt;
use std::sync::Arc;

use peniko::Color;

use crate::grid::{GridPlacement, GridTemplate};

pub use crate::calc::Calc;

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
    /// Inline-level outside, a block container inside: it flows in a line as one
    /// unbreakable thing, and what is in it is laid out as a block.
    InlineBlock,
    /// A flex container: block-level outside, and its children are flex items
    /// rather than a block or inline formatting context.
    Flex,
    /// A flex container that is inline-level outside: it takes its place in a line
    /// the way an `inline-block` does, and inside it is the same flex container.
    InlineFlex,
    /// A grid container: its children are placed into rows and columns.
    Grid,
    /// A grid container that is inline-level outside, placed in a line the way
    /// an `inline-block` is.
    InlineGrid,
    /// A table: its rows and cells are placed into a grid of its own, with the
    /// columns sized by what is in them.
    Table,
    /// `thead`, `tbody`, `tfoot`: a run of rows, which the table reads through.
    TableRowGroup,
    /// One row of cells.
    TableRow,
    /// One cell, which is a block container of its own inside its column.
    TableCell,
    /// A table's caption, laid out above it and as wide as it is.
    TableCaption,
}

impl Display {
    /// Whether this is a grid container, block-level or inline-level.
    pub fn is_grid(self) -> bool {
        matches!(self, Self::Grid | Self::InlineGrid)
    }

    /// Whether this is a table or one of the parts a table is made of.
    pub fn is_table_part(self) -> bool {
        matches!(
            self,
            Self::Table
                | Self::TableRowGroup
                | Self::TableRow
                | Self::TableCell
                | Self::TableCaption
        )
    }
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
    /// `normal` or `stretch`, the initial value: a grid's `auto` tracks grow
    /// to take it (CSS Grid 2 §12.8); a flex container, which has nothing to
    /// stretch along its main axis, lays it out as `start` (CSS Align 3 §5.1).
    Stretch,
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

/// How an item is placed in the space it has along one axis: across a flex
/// line, or in its grid area, by `align-*` or `justify-*`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AlignItems {
    /// `normal`, the initial value: `stretch` for a flex item and for most
    /// grid items; `start` for a grid item with a natural aspect ratio (CSS
    /// Align 3 §6.1, §6.2).
    Normal,
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

impl From<JustifyContent> for AlignContent {
    /// The same distribution along the other axis: the two properties share
    /// one set of values (CSS Align 3 §5.1), `normal` included.
    fn from(justify: JustifyContent) -> Self {
        match justify {
            JustifyContent::Stretch => Self::Stretch,
            JustifyContent::Start => Self::Start,
            JustifyContent::End => Self::End,
            JustifyContent::Center => Self::Center,
            JustifyContent::SpaceBetween => Self::SpaceBetween,
            JustifyContent::SpaceAround => Self::SpaceAround,
            JustifyContent::SpaceEvenly => Self::SpaceEvenly,
        }
    }
}

/// `align-content`: how a wrapped container's lines share what is left across it.
///
/// The same shape as `justify-content` with a `stretch` on the end, which is its
/// initial value and the reason a wrapped container's lines fill it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AlignContent {
    /// The lines grow equally to fill the container.
    Stretch,
    /// All the leftover after them.
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

/// A `<length-percentage>`: what padding, a border radius, a gap and every
/// sizing property that names a size come to once the cascade has done what it
/// can.
///
/// The cascade settles every absolute unit and every `em`; what it cannot settle
/// is a percentage, because what it is a percentage *of* is decided by layout. A
/// value that mixes the two — `calc(100% - 16px)`, `min(100%, 18rem)` — is kept as
/// the expression, and resolved the same way a bare percentage is: against a
/// basis, when layout has one.
#[derive(Clone, Debug, PartialEq)]
pub enum Length {
    /// An absolute length in CSS pixels.
    Px(f32),
    /// A fraction of the basis, 0–1 rather than 0–100. For padding and margins
    /// that is the containing block's *width*, as CSS requires even vertically.
    Percent(f32),
    /// A math function with a percentage in it.
    Calc(Calc),
}

impl Length {
    /// Zero.
    pub const ZERO: Self = Self::Px(0.0);

    /// Resolve against a basis — the size a percentage is of.
    pub fn resolve(&self, basis: f32) -> f32 {
        match self {
            Self::Px(px) => *px,
            Self::Percent(fraction) => fraction * basis,
            Self::Calc(calc) => calc.resolve(basis),
        }
    }

    /// Resolve against a basis that may not be known yet, or `None` when this
    /// needs one and there is none.
    ///
    /// The one rule behind two that CSS states separately. A percentage height
    /// against a containing block whose height depends on its content computes to
    /// `auto` (CSS 2.2 §10.5); and while a box's intrinsic size is being measured,
    /// a percentage of the size being measured is *cyclic* and the property is
    /// taken as its initial value (CSS Sizing 3 §5.2.1). Both are a percentage of
    /// something that is not known, and a `calc()` with a percentage anywhere in
    /// it is one — which is why this asks whether the value needs a basis rather
    /// than whether it is a bare percentage.
    pub fn definite(&self, basis: Option<f32>) -> Option<f32> {
        match self {
            Self::Px(px) => Some(*px),
            Self::Percent(_) | Self::Calc(_) => basis.map(|basis| self.resolve(basis)),
        }
    }
}

impl fmt::Display for Length {
    /// The value as a stylesheet would write it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Px(px) => write!(formatter, "{px}px"),
            Self::Percent(fraction) => write!(formatter, "{}%", fraction * 100.0),
            Self::Calc(calc) => calc.fmt(formatter),
        }
    }
}

/// A length, or `auto`: a margin or an inset.
#[derive(Clone, Debug, PartialEq)]
pub enum LengthOrAuto {
    /// A length or a percentage of the containing block.
    Length(Length),
    /// `auto`: the used value is worked out during layout.
    Auto,
}

impl LengthOrAuto {
    /// Zero, which is where a margin starts.
    pub const ZERO: Self = Self::Length(Length::ZERO);

    /// Resolve against a containing-block size, or `None` for `auto`.
    pub fn resolve(&self, containing: f32) -> Option<f32> {
        match self {
            Self::Length(length) => Some(length.resolve(containing)),
            Self::Auto => None,
        }
    }
}

impl fmt::Display for LengthOrAuto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(length) => length.fmt(formatter),
            Self::Auto => formatter.write_str("auto"),
        }
    }
}

/// The sizes CSS Sizing 3 §3.2 takes from a box's own content rather than from
/// a number: what `min-content`, `max-content` and `fit-content` ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Intrinsic {
    /// As narrow as the box can be without its content spilling: its longest
    /// word, its widest picture.
    MinContent,
    /// As wide as its content is with nothing wrapped.
    MaxContent,
    /// Its max-content size where that fits, its min-content size where even that
    /// does not, and the room in between otherwise. `None` is the keyword, which
    /// fits into the room available; `Some` is `fit-content(<length-percentage>)`,
    /// which fits into the length it names instead.
    FitContent(Option<Length>),
}

impl fmt::Display for Intrinsic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MinContent => formatter.write_str("min-content"),
            Self::MaxContent => formatter.write_str("max-content"),
            Self::FitContent(None) => formatter.write_str("fit-content"),
            Self::FitContent(Some(limit)) => write!(formatter, "fit-content({limit})"),
        }
    }
}

/// `width`, `height`, `min-width` and `min-height`, in every value CSS Sizing 3
/// gives them.
///
/// One type for the preferred size and the minimum, because CSS gives them one
/// grammar. What `auto` *means* is the difference: a preferred size of `auto` is
/// worked out by the formatting context, and a minimum of `auto` is the
/// automatic minimum — zero for almost every box, and for a flex item the size
/// its content cannot go below (CSS Flexbox §4.5). Keeping `auto` apart from a
/// length of zero is what lets `min-width: 0` turn that minimum off.
#[derive(Clone, Debug, PartialEq)]
pub enum Size {
    /// `auto`.
    Auto,
    /// A length or a percentage, measured across the box `box-sizing` names.
    Length(Length),
    /// A size taken from the content.
    Intrinsic(Intrinsic),
    /// `stretch`, and its older spelling `-webkit-fill-available`: as large as
    /// the containing block allows once the box's own margins are taken out
    /// (CSS Sizing 4).
    Stretch,
}

impl fmt::Display for Size {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Length(length) => length.fmt(formatter),
            Self::Intrinsic(keyword) => keyword.fmt(formatter),
            Self::Stretch => formatter.write_str("stretch"),
        }
    }
}

/// `max-width` and `max-height`, where `none` is no limit at all rather than a
/// very large one.
#[derive(Clone, Debug, PartialEq)]
pub enum MaxSize {
    /// `none`.
    None,
    /// A length or a percentage, measured across the box `box-sizing` names.
    Length(Length),
    /// A limit taken from the content.
    Intrinsic(Intrinsic),
    /// `stretch`: no larger than the containing block allows.
    Stretch,
}

impl fmt::Display for MaxSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("none"),
            Self::Length(length) => length.fmt(formatter),
            Self::Intrinsic(keyword) => keyword.fmt(formatter),
            Self::Stretch => formatter.write_str("stretch"),
        }
    }
}

/// A `<ratio>` (CSS Values 4 §7.2) that is not degenerate: a box's width over
/// its height, finite and above zero.
///
/// Made only through [`Ratio::new`], which refuses a degenerate ratio, so a
/// value of this type is one a box can be sized through.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ratio(f32);

impl Ratio {
    /// The ratio of `width` to `height`, or `None` where it is degenerate —
    /// either side zero (CSS Values 4 §7.2) — or its quotient is not a finite
    /// number above zero.
    pub fn new(width: f32, height: f32) -> Option<Self> {
        let quotient = width / height;
        (width > 0.0 && height > 0.0 && quotient.is_finite() && quotient > 0.0)
            .then_some(Self(quotient))
    }

    /// Width over height.
    pub fn width_over_height(self) -> f32 {
        self.0
    }
}

/// `aspect-ratio`: the ratio of a box's width to its height that its automatic
/// sizes keep (CSS Sizing 4 §4.1).
///
/// A degenerate `<ratio>` makes the property behave as `auto`, and so is
/// `Auto` here.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AspectRatio {
    /// `auto`: a replaced element's natural ratio, and none for any other box.
    Auto,
    /// `<ratio>`: this ratio for every box, a picture's own overridden, and
    /// measured across the box `box-sizing` names.
    Ratio(Ratio),
    /// `auto && <ratio>`: a replaced element's natural ratio where it has one,
    /// and this ratio otherwise — measured across the content box either way.
    AutoOr(Ratio),
}

/// `flex-basis`: the size a flex item starts from before the line is shared out.
#[derive(Clone, Debug, PartialEq)]
pub enum FlexBasis {
    /// `content`: the size of what is in the item, whatever its `width` says.
    Content,
    /// Anything `width` can be. `auto` is not a size of its own here: it defers
    /// to the item's `width` or `height`, whichever is along the main axis.
    Size(Size),
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

impl<T: Clone> Sides<T> {
    /// The same value on all four sides.
    pub fn all(value: T) -> Self {
        Self {
            top: value.clone(),
            right: value.clone(),
            bottom: value.clone(),
            left: value,
        }
    }
}

/// `border-style`: the line a border draws.
///
/// `none` and `hidden` are both zero wide and differ in one place only — a
/// collapsed table border, where `hidden` silences the edge outright and `none`
/// merely loses to anything else in the running.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BorderStyle {
    /// Nothing, and nothing to say about it.
    #[default]
    None,
    /// Nothing, and nothing may draw there.
    Hidden,
    /// One unbroken line.
    Solid,
    /// A run of dashes.
    Dashed,
    /// A run of round dots.
    Dotted,
    /// Two lines with a gap between them.
    Double,
    /// Carved into the page: dark on the side the light comes from.
    Groove,
    /// Raised off it, which is `groove` turned over.
    Ridge,
    /// The whole box pressed in: dark along the top and the left.
    Inset,
    /// The whole box standing out, which is `inset` turned over.
    Outset,
}

impl BorderStyle {
    /// Whether this style draws anything at all.
    pub fn draws(self) -> bool {
        !matches!(self, Self::None | Self::Hidden)
    }
}

/// One border: how wide it is drawn, what colour, and what line.
///
/// The width is the *used* width, so a `none` or a `hidden` border is zero wide
/// however wide it was declared — which is what keeps the arithmetic right for
/// everything downstream that only wants to know where the content sits.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Border {
    /// The used width in CSS pixels — zero when the style makes the border absent.
    pub width: f32,
    /// `border-*-color`, which defaults to the element's own `color`.
    pub color: Color,
    /// `border-*-style`.
    pub style: BorderStyle,
}

impl Border {
    /// No border.
    pub const NONE: Self = Self {
        width: 0.0,
        color: Color::TRANSPARENT,
        style: BorderStyle::None,
    };

    /// Whether this border puts anything on the screen.
    pub fn is_visible(self) -> bool {
        self.width > 0.0 && self.color.components[3] > 0.0 && self.style.draws()
    }
}

/// `box-sizing`: what a `width` and a `height` are measured across.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BoxSizing {
    /// The content box: the padding and the border are added outside it, which is
    /// what CSS starts from.
    Content,
    /// The border box: the padding and the border come out of the number, which is
    /// what most of the web sets on everything and then writes its widths against.
    Border,
}

/// `border-collapse`: whether a table's cells each draw their own edge or share
/// one between them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BorderCollapse {
    /// Each cell has its own border, with `border-spacing` between them.
    Separate,
    /// Neighbouring cells share one edge, drawn once, and the spacing is ignored.
    Collapse,
}

/// One step of a `transform`, in the two dimensions this draws in.
///
/// Kept as the steps rather than multiplied into one matrix, because a percentage
/// in a `translate()` is of the box's own size and the box is not measured until
/// layout has run. The rasterizer resolves them against the box it is drawing.
#[derive(Clone, Debug, PartialEq)]
pub enum TransformOp {
    /// Move, each axis a length or a fraction of the box's own size.
    Translate(Length, Length),
    /// Multiply, around the origin.
    Scale(f32, f32),
    /// Turn, in radians, clockwise.
    Rotate(f32),
    /// Slant, in radians.
    Skew(f32, f32),
    /// The six numbers of a 2D matrix, in CSS order.
    Matrix([f32; 6]),
}

/// Where a `transform` is applied from: the point the box turns and grows about.
#[derive(Clone, Debug, PartialEq)]
pub struct TransformOrigin {
    /// Across the box.
    pub x: Length,
    /// Down it.
    pub y: Length,
}

impl Default for TransformOrigin {
    /// The middle of the box, which is what CSS starts from.
    fn default() -> Self {
        Self {
            x: Length::Percent(0.5),
            y: Length::Percent(0.5),
        }
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
    /// Spread from edge to edge, the last line and one before a forced break
    /// aligned as `text-align-last` says.
    Justify,
}

/// `text-align-last` (CSS Text 3 §7.3): how the last line of a paragraph, and
/// one ended by a forced break, is aligned.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum TextAlignLast {
    /// As `text-align` says, except that `justify` is `start`.
    #[default]
    Auto,
    /// The start edge.
    Start,
    /// The end edge.
    End,
    /// Centred.
    Center,
    /// Spread from edge to edge.
    Justify,
}

/// `text-justify` (CSS Text 3 §7.4): what justified text is spread by.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum TextJustify {
    /// The UA's choice, which is between words.
    #[default]
    Auto,
    /// Justification is off: a justified line is aligned to its start.
    None,
    /// Between words.
    InterWord,
    /// Between letters.
    InterCharacter,
}

/// `text-indent` (CSS Text 3 §8.1). The cascade parses `hanging` and
/// `each-line` only in its Gecko build, so from it they are always off; they
/// are carried for when that changes.
#[derive(Clone, Debug, PartialEq)]
pub struct TextIndent {
    /// How far the first line is indented; a percentage is of the block's
    /// content width.
    pub length: Length,
    /// `hanging`: every other line is indented instead.
    pub hanging: bool,
    /// `each-line`: the first line after each forced break is indented too.
    pub each_line: bool,
}

impl TextIndent {
    /// No indent — the initial value.
    pub const NONE: Self = Self {
        length: Length::ZERO,
        hanging: false,
        each_line: false,
    };
}

/// `tab-size` (CSS Text 3 §4.2).
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TabSize {
    /// So many spaces.
    Spaces(f32),
    /// A length, in CSS pixels.
    Px(f32),
}

/// What `text-overflow` draws at the end of a line cut off by its box
/// (CSS Overflow 4 §3.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TextOverflow {
    /// Nothing: the text is clipped.
    #[default]
    Clip,
    /// A horizontal ellipsis.
    Ellipsis,
    /// The given string.
    String(Arc<str>),
}

/// `background-size`, in the three shapes that mean something without a full
/// two-value model behind them.
#[derive(Clone, Debug, PartialEq)]
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

/// `vertical-align`, in the values that can be answered from the fonts alone.
///
/// `top` and `bottom` align against the line box, which is not known until every
/// box on the line has been placed — and where they are placed depends on how tall
/// the line is. Resolving that needs a second pass over the line, so they are left
/// on the baseline rather than guessed at.
#[derive(Clone, Debug, PartialEq)]
pub enum VerticalAlign {
    /// On the parent's baseline. The initial value, and almost every box.
    Baseline,
    /// Lowered to where the parent's font puts a subscript.
    Sub,
    /// Raised to where it puts a superscript.
    Super,
    /// Raised by a length of its own; negative lowers. A percentage in it is of
    /// the element's own `line-height`, which is the one place a percentage in
    /// CSS is not of the containing block.
    Shift(Length),
    /// Top edge against the line box's top edge.
    Top,
    /// Bottom edge against the line box's bottom edge.
    Bottom,
    /// Middle against the parent's baseline plus half its x-height.
    Middle,
    /// Top edge against the top of the parent's own text.
    TextTop,
    /// Bottom edge against the bottom of the parent's own text.
    TextBottom,
}

impl VerticalAlign {
    /// Whether this is settled while a line is levelled rather than from the
    /// two styles alone.
    ///
    /// These five are a *position* rather than a shift: three are measured
    /// against the parent's own font and two against the finished line box. All
    /// five are worked out once, where the fonts are already in hand, and read
    /// back when the glyphs are placed — so nothing works them out twice and
    /// gets two answers.
    pub fn resolved_while_levelling(&self) -> bool {
        matches!(
            self,
            Self::Top | Self::Bottom | Self::Middle | Self::TextTop | Self::TextBottom
        )
    }
}

/// `list-style-type`, in the counters a list actually uses.
///
/// The three bullets and the four numberings the HTML `type` attribute has always
/// had. A counter style we do not know is drawn as a disc rather than as nothing,
/// which is what a reader can still follow.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ListStyle {
    /// No marker at all.
    None,
    /// A filled circle.
    Disc,
    /// A hollow one.
    Circle,
    /// A filled square.
    Square,
    /// 1, 2, 3.
    Decimal,
    /// a, b, c.
    LowerAlpha,
    /// A, B, C.
    UpperAlpha,
    /// i, ii, iii.
    LowerRoman,
    /// I, II, III.
    UpperRoman,
}

impl ListStyle {
    /// Whether this style counts its items rather than marking each the same.
    pub fn is_ordered(self) -> bool {
        matches!(
            self,
            Self::Decimal
                | Self::LowerAlpha
                | Self::UpperAlpha
                | Self::LowerRoman
                | Self::UpperRoman
        )
    }
}

/// `background-repeat` along one axis.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Repeat {
    /// Tiled, and cut off where the box ends.
    Repeat,
    /// Drawn once.
    None,
    /// Tiled, with the tile stretched or squeezed so a whole number of them fits.
    Round,
}

/// One layer of a box's background.
///
/// `background-image` is a list, and every property that describes a layer is a
/// list beside it — so a page may put a pattern over a gradient, or a badge in
/// each corner, in one rule. A layer that names neither a picture nor a gradient
/// is `none`, which is what an empty slot in the list means and is kept so that
/// the slots after it still line up with the sizes and positions written for them.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundLayer {
    /// The address of the picture, exactly as written. Resolving and fetching it
    /// is the caller's, as it is for a stylesheet.
    pub image: Option<Arc<str>>,
    /// The gradient, where the layer is one rather than a picture.
    pub gradient: Option<Gradient>,
    /// How the picture is sized against its box.
    pub size: BackgroundSize,
    /// Whether and how it is tiled, per axis.
    pub repeat: BackgroundRepeat,
    /// Where it sits in the box it is behind.
    pub position: BackgroundPosition,
}

impl BackgroundLayer {
    /// Whether this layer would draw anything at all.
    pub fn draws(&self) -> bool {
        self.image.is_some() || self.gradient.is_some()
    }
}

/// `background-repeat`, which CSS gives per axis and a page usually gives once.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BackgroundRepeat {
    /// Across.
    pub x: Repeat,
    /// Down.
    pub y: Repeat,
}

impl BackgroundRepeat {
    /// The initial value: tiled both ways.
    pub const REPEAT: Self = Self {
        x: Repeat::Repeat,
        y: Repeat::Repeat,
    };
}

/// `background-position`, one length per axis.
///
/// Each is a length measured from the start edge, and a percentage in it is of
/// the room the picture *leaves* in its box rather than of the box: `50%` is half
/// of what is left over, and `right 10px` computes to `calc(100% - 10px)`. A
/// percentage of nothing left over is nothing, which is why a picture as large as
/// its box sits at the same place whatever the position says. So the value is
/// resolved against that leftover room, and a `calc()` in it — `min()` and
/// `clamp()` included — is resolved exactly as it is anywhere else.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundPosition {
    /// Across.
    pub x: Length,
    /// Down.
    pub y: Length,
}

impl BackgroundPosition {
    /// The initial value: the box's own top left corner.
    pub const START: Self = Self {
        x: Length::Percent(0.0),
        y: Length::Percent(0.0),
    };

    /// The middle of the box, which is where `object-position` starts.
    pub const CENTER: Self = Self {
        x: Length::Percent(0.5),
        y: Length::Percent(0.5),
    };
}

/// `object-fit`: how a replaced element's own picture is fitted into the box the
/// page gave it.
///
/// The box is decided by layout and this decides what happens inside it. The
/// default is `Fill`, which stretches — the behaviour every picture had before
/// the property existed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ObjectFit {
    /// Stretched to the box, ratio abandoned.
    #[default]
    Fill,
    /// As large as fits with the ratio kept: the box may show through.
    Contain,
    /// Small enough to cover the box with the ratio kept: the picture is cut off.
    Cover,
    /// Its own size, whatever the box is.
    None,
    /// `None`, unless that overflows, in which case `Contain`.
    ScaleDown,
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
    /// Whether it falls inside the box rather than behind it.
    ///
    /// An inset shadow is the shadow the box's own hole casts: it is drawn over
    /// the background, clipped to the padding box, and grows *inwards* — so the
    /// spread and the offset both move the lit part rather than the shadow.
    pub inset: bool,
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

/// The four corner radii of a box, in CSS order.
#[derive(Clone, Debug, PartialEq)]
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
            &self.top_left,
            &self.top_right,
            &self.bottom_right,
            &self.bottom_left,
        ]
        .into_iter()
        .any(|corner| *corner != Length::ZERO)
    }
}

/// `overflow`, in the distinctions layout can act on: whether content that does
/// not fit is shown or cut off, and whether the box that cuts it off is a scroll
/// container (CSS Overflow 3 §3).
///
/// One value for both axes: the box cuts off in both if it does in either, and
/// is a scroll container if either axis makes it one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// Content spills out of the box and is drawn.
    Visible,
    /// `clip`: content is cut off at the box's padding edge, and that is all —
    /// the box is not a scroll container and establishes nothing (§3.1).
    Clip,
    /// `hidden`, `scroll` or `auto`: content is cut off the same way, and the
    /// box is a scroll container, whether or not anything scrolls it yet.
    Scroll,
}

impl Overflow {
    /// Whether content that does not fit is cut off at the padding edge.
    pub fn clips(self) -> bool {
        match self {
            Self::Visible => false,
            Self::Clip | Self::Scroll => true,
        }
    }

    /// Whether the box is a scroll container: what establishes an independent
    /// formatting context (CSS Display 3 §2.2), takes a flex item's automatic
    /// minimum size away (CSS Flexbox 1 §4.5), and sits an inline block on its
    /// bottom margin edge (CSS 2.2 §10.8.1).
    pub fn is_scroll_container(self) -> bool {
        match self {
            Self::Visible | Self::Clip => false,
            Self::Scroll => true,
        }
    }
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

/// `white-space-collapse`: what happens to runs of spaces and to newlines.
///
/// Only the collapsing half of the old `white-space` shorthand. Whether a line
/// may break is [`TextWrap`], because CSS models the two as independent
/// longhands and `white-space: nowrap` is exactly the pair that this enum alone
/// cannot say: collapse the spaces *and* do not wrap.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WhiteSpace {
    /// Runs of white space collapse to one space, and a line ending in the
    /// source is one more piece of white space.
    Collapse,
    /// Every space, tab and line ending is kept, and a line ending breaks the
    /// line.
    Preserve,
    /// Spaces and tabs collapse; a line ending is still a break. `pre-line`.
    PreserveBreaks,
    /// Everything is kept, and a line may break inside a run of spaces rather
    /// than only between words. `break-spaces`.
    BreakSpaces,
}

impl WhiteSpace {
    /// Whether a run of spaces and tabs collapses to one space.
    pub fn collapses_spaces(self) -> bool {
        matches!(self, Self::Collapse | Self::PreserveBreaks)
    }

    /// Whether a line ending in the source breaks the line.
    pub fn preserves_breaks(self) -> bool {
        !matches!(self, Self::Collapse)
    }
}

/// `word-break` (CSS Text 3 §5.2): where inside a word a line may break.
/// `break-word`, its deprecated fourth value, is Gecko's alone in the cascade
/// and never reaches here.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WordBreak {
    /// Words break where the text's own rules say (UAX #14).
    #[default]
    Normal,
    /// A line may also break between any two letters of a word.
    BreakAll,
    /// A line may not break inside a word, CJK text included.
    KeepAll,
}

/// `overflow-wrap` (CSS Text 3 §5.5): whether a word too long for its line may
/// be broken where nothing else lets it, and whether those breaks count
/// towards the text's min-content size.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum OverflowWrap {
    /// A word too long for its line overflows it.
    #[default]
    Normal,
    /// It is broken, but its min-content size is the whole word.
    BreakWord,
    /// It is broken, and the breaks are soft wrap opportunities for sizing
    /// too.
    Anywhere,
}

/// `text-wrap-mode`: whether a line may be broken at all.
///
/// The other half of `white-space`. Kept apart from [`WhiteSpace`] because the
/// four combinations are all real — `normal`, `pre`, `nowrap` and `pre-wrap` are
/// the two bits in their four arrangements — and one enum of two values could
/// only ever spell two of them.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum TextWrap {
    /// Lines break where they have to.
    #[default]
    Wrap,
    /// Lines do not break, whatever the box is wide.
    NoWrap,
}

/// `text-decoration-line`, as the flags it is.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct DecorationLines {
    /// A line below the text.
    pub underline: bool,
    /// A line above it.
    pub overline: bool,
    /// A line through it.
    pub line_through: bool,
}

impl DecorationLines {
    /// No line at all — the initial value.
    pub const NONE: Self = Self {
        underline: false,
        overline: false,
        line_through: false,
    };

    /// Whether any line is drawn.
    pub fn is_none(self) -> bool {
        !self.underline && !self.overline && !self.line_through
    }
}

/// `text-decoration-style`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum DecorationStyle {
    /// One line.
    #[default]
    Solid,
    /// Two lines.
    Double,
    /// A line of dots.
    Dotted,
    /// A line of dashes.
    Dashed,
    /// A wave.
    Wavy,
}

/// `text-decoration`: which lines an element draws, how, and in what colour.
///
/// This is the element's own value. What a text run is decorated with is every
/// decoration in effect on it, its ancestors' included (css-text-decor-3
/// §2.1), which is [`ComputedStyle::decorations`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextDecoration {
    /// `text-decoration-line`.
    pub lines: DecorationLines,
    /// `text-decoration-style`.
    pub style: DecorationStyle,
    /// `text-decoration-color`, already resolved: `currentColor` is the colour
    /// of the element that declared the decoration, not of the text under it.
    pub color: Color,
}

impl TextDecoration {
    /// No decoration at all — the initial value.
    pub const NONE: Self = Self {
        lines: DecorationLines::NONE,
        style: DecorationStyle::Solid,
        color: Color::BLACK,
    };

    /// Whether anything is drawn.
    pub fn is_none(self) -> bool {
        self.lines.is_none()
    }
}

/// The case part of `text-transform` (CSS Text 3 §2.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TextCase {
    /// Left as written.
    #[default]
    None,
    /// Every letter in upper case.
    Uppercase,
    /// Every letter in lower case.
    Lowercase,
    /// The first letter of each word in title case.
    Capitalize,
}

/// `text-transform`.
///
/// `full-size-kana` is read by the cascade and not applied here: small kana
/// stay small. `math-auto` applies to MathML, which is not laid out.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TextTransform {
    /// Which case the letters are put in.
    pub case: TextCase,
    /// `full-width`: ASCII set as its full-width forms.
    pub full_width: bool,
}

impl TextTransform {
    /// No transform — the initial value.
    pub const NONE: Self = Self {
        case: TextCase::None,
        full_width: false,
    };

    /// Whether the text is left as it is.
    pub fn is_none(self) -> bool {
        self == Self::NONE
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

/// A generic font family: a keyword standing for whichever face the browser
/// sets that kind of type in (CSS Fonts 4 §2.1.5).
///
/// The keywords the cascade reads as generics. `ui-serif`, `ui-sans-serif`,
/// `ui-monospace`, `ui-rounded`, `emoji`, `math` and `fangsong` are not among
/// them: Stylo takes each for a family name, and so it arrives as one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GenericFamily {
    /// `serif`.
    Serif,
    /// `sans-serif`.
    SansSerif,
    /// `monospace`.
    Monospace,
    /// `cursive`.
    Cursive,
    /// `fantasy`.
    Fantasy,
    /// `system-ui`: the face the platform sets its own interface in.
    SystemUi,
}

impl GenericFamily {
    /// The keyword, as CSS spells it.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Serif => "serif",
            Self::SansSerif => "sans-serif",
            Self::Monospace => "monospace",
            Self::Cursive => "cursive",
            Self::Fantasy => "fantasy",
            Self::SystemUi => "system-ui",
        }
    }
}

impl fmt::Display for GenericFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.keyword())
    }
}

/// One entry of a `font-family` list (CSS Fonts 4 §2.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FamilyName {
    /// A family asked for by name. A quoted keyword is one of these — `"serif"`
    /// names a family called serif, not the generic — and so is a quoted name
    /// with a comma in it, which is one name and not two.
    Named(Arc<str>),
    /// A generic family.
    Generic(GenericFamily),
}

impl fmt::Display for FamilyName {
    /// A name that is a single CSS identifier, and no keyword `font-family` could
    /// read it as, is written as it is; any other is written as a string. CSS
    /// Fonts 4 §2.1.1 requires a name that spells a keyword to be quoted, and
    /// recommends quoting one with white space or punctuation in it; quoting
    /// both is what makes the list read back as the same list.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic(generic) => write!(f, "{generic}"),
            Self::Named(name) if is_plain_identifier(name) && !is_family_keyword(name) => {
                f.write_str(name)
            }
            Self::Named(name) => cssparser::serialize_string(name, f),
        }
    }
}

/// Whether `name`, tokenised as CSS, is one identifier spelling exactly itself —
/// no escapes, no white space around it (an `<ident-token>`, CSS Syntax 3 §4).
fn is_plain_identifier(name: &str) -> bool {
    let mut input = cssparser::ParserInput::new(name);
    let mut parser = cssparser::Parser::new(&mut input);
    let spelt = parser.expect_ident().is_ok_and(|ident| **ident == *name);
    spelt && parser.is_exhausted()
}

/// Whether an unquoted `name` would be read as a keyword rather than as a
/// family: a generic, including the ones CSS Fonts 4 adds that the cascade does
/// not know yet, or one of the keywords every property takes.
fn is_family_keyword(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
        "ui-serif",
        "ui-sans-serif",
        "ui-monospace",
        "ui-rounded",
        "emoji",
        "math",
        "fangsong",
        "inherit",
        "initial",
        "unset",
        "revert",
        "revert-layer",
        "default",
    ];
    KEYWORDS
        .iter()
        .any(|keyword| keyword.eq_ignore_ascii_case(name))
}

/// `font-family`: the families to try, in order (CSS Fonts 4 §2.1).
///
/// Shared rather than copied, like [`ComputedStyle::font_variations`]: an
/// element that inherits its family holds the very list its parent does, which
/// is also what lets layout make one font stack per list rather than one per
/// element. Never empty — see [`FontFamily::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFamily(Arc<[FamilyName]>);

impl FontFamily {
    /// The families, in order; the standard font when there are none.
    pub fn new(families: impl IntoIterator<Item = FamilyName>) -> Self {
        let families: Arc<[FamilyName]> = families.into_iter().collect();
        if families.is_empty() {
            Self::default()
        } else {
            Self(families)
        }
    }
}

impl std::ops::Deref for FontFamily {
    type Target = [FamilyName];

    fn deref(&self) -> &[FamilyName] {
        &self.0
    }
}

impl Default for FontFamily {
    /// The standard font, which every browser sets to a serif — and which
    /// `medium` is sixteen pixels of, the pair being two halves of one
    /// preference. A page that says nothing about its font should look like the
    /// same page does everywhere else.
    fn default() -> Self {
        Self(Arc::new([FamilyName::Generic(GenericFamily::Serif)]))
    }
}

impl fmt::Display for FontFamily {
    /// The list as CSS writes it, comma-separated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, family) in self.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{family}")?;
        }
        Ok(())
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
    /// `text-shadow`. Inherited, and drawn behind the text rather than behind the
    /// box, which is why it is a list of its own.
    pub text_shadows: Vec<Shadow>,
    /// `box-shadow`, outermost last — the order they are painted in, which is the
    /// reverse of the order they are written.
    pub shadows: Vec<Shadow>,
    /// The layers of `background-image`, topmost first — the order the page wrote
    /// them, which is the reverse of the order they are painted in.
    pub backgrounds: Vec<BackgroundLayer>,
    /// How a replaced element's picture is fitted into its box.
    pub object_fit: ObjectFit,
    /// And where inside the box what is left of it sits. Its initial value is
    /// the middle, which is not `background-position`'s corner.
    pub object_position: BackgroundPosition,
    /// `font-family`, family by family. Inherited.
    pub font_family: FontFamily,
    /// `font-size` in CSS pixels. Inherited.
    pub font_size: f32,
    /// `font-weight`, 100–900. Inherited.
    pub font_weight: u16,
    /// `font-style`. Inherited.
    pub font_style: FontStyle,
    /// `font-width` (`font-stretch`) as a percentage, 100 being normal. Inherited.
    pub font_width: f32,
    /// `font-optical-sizing`: whether the optical-size axis takes the font size.
    /// Inherited.
    pub optical_sizing: bool,
    /// `font-variation-settings`: axis tags and values, ordered by tag rather than
    /// as written, which is the order a shaper resolves a repeated tag in.
    /// Inherited, and empty on almost every element there is — shared rather than
    /// copied, because inheriting it is the common case and cloning a list per
    /// element would be a cost every page pays for a property almost none uses.
    pub font_variations: Arc<[([u8; 4], f32)]>,
    /// The OpenType features the text is set with, in the order CSS Fonts 4
    /// §7.2 applies them — `font-kerning`, the `font-variant-*` properties,
    /// then `font-feature-settings` — a later setting of a tag winning.
    /// Inherited, and shared like the variations, being empty almost always.
    pub font_features: Arc<[([u8; 4], u16)]>,
    /// `letter-spacing` in CSS pixels. Inherited.
    pub letter_spacing: f32,
    /// `word-spacing` in CSS pixels. Inherited.
    pub word_spacing: f32,
    /// `line-height`. Inherited.
    pub line_height: LineHeight,
    /// `list-style-type`. Inherited, because a list sets it and its items read it.
    pub list_style: ListStyle,
    /// `vertical-align`. Not inherited: it moves the box it is written on.
    pub vertical_align: VerticalAlign,
    /// `border-spacing`, horizontal and vertical, in CSS pixels. Inherited, which
    /// is what lets it be written on the table and read by the cells.
    pub border_spacing: (f32, f32),
    /// `border-collapse`. Inherited, and read on the table: it decides whether the
    /// cells each draw their own edge inside the spacing or share one between them.
    pub border_collapse: BorderCollapse,
    /// `box-sizing`.
    pub box_sizing: BoxSizing,
    /// `opacity`, 0 to 1. Not inherited, and not a property of the text either: it
    /// applies to the element and everything in it *once*, as a group, which is why
    /// a half-transparent box with overlapping children does not show the overlap
    /// through itself.
    pub opacity: f32,
    /// `transform`, in the order the steps were written. Empty for `none`.
    ///
    /// Shared: a page that transforms a hundred cards writes one list and every
    /// one of them points at it.
    pub transform: Arc<[TransformOp]>,
    /// `transform-origin`.
    pub transform_origin: TransformOrigin,
    /// `margin`.
    pub margin: Sides<LengthOrAuto>,
    /// `padding`.
    pub padding: Sides<Length>,
    /// `border-*-width` and `border-*-color`, resolved together.
    pub border: Sides<Border>,
    /// `text-align`. Inherited.
    pub text_align: TextAlign,
    /// `text-align-last`. Inherited.
    pub text_align_last: TextAlignLast,
    /// `text-justify`. Inherited.
    pub text_justify: TextJustify,
    /// `text-indent`. Inherited.
    pub text_indent: TextIndent,
    /// `tab-size`. Inherited.
    pub tab_size: TabSize,
    /// `text-overflow`. Not inherited: it is a property of the block container
    /// whose lines it cuts.
    pub text_overflow: TextOverflow,
    /// `white-space-collapse`. Inherited.
    pub white_space: WhiteSpace,
    /// `text-wrap-mode`. Inherited.
    pub text_wrap: TextWrap,
    /// `word-break`. Inherited.
    pub word_break: WordBreak,
    /// `overflow-wrap`, and its old name `word-wrap`. Inherited.
    pub overflow_wrap: OverflowWrap,
    /// `text-decoration`, the element's own. Not inherited: it *propagates*
    /// (css-text-decor-3 §2.1), which is [`Self::decorations`].
    pub text_decoration: TextDecoration,
    /// Every decoration in effect on this box's text, outermost first: those
    /// its ancestors propagated to it, then its own. A descendant cannot take
    /// one away — `text-decoration: none` on it removes nothing but its own.
    /// Filled in by the box tree, which knows which boxes a decoration reaches;
    /// carried to an anonymous box, which is part of its parent's contents.
    pub decorations: Arc<[TextDecoration]>,
    /// `text-transform`. Inherited.
    pub text_transform: TextTransform,
    /// `width`.
    pub width: Size,
    /// `height`.
    pub height: Size,
    /// `min-width`, which floors whatever `width` resolves to.
    pub min_width: Size,
    /// `max-width`. This is what holds a page's text column to a readable
    /// measure, so it is the one of the four that shows on nearly every real page.
    pub max_width: MaxSize,
    /// `min-height`.
    pub min_height: Size,
    /// `max-height`.
    pub max_height: MaxSize,
    /// `aspect-ratio`, which the automatic sizes above keep. Not inherited.
    pub aspect_ratio: AspectRatio,
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
    /// The grid a grid container defines: its tracks, line names, areas and
    /// flow. `None` on anything that is not a grid container.
    pub grid: Option<Arc<GridTemplate>>,
    /// `grid-column-start` and `grid-column-end`, read by an item rather than
    /// by the container.
    pub grid_column: GridPlacement,
    /// `grid-row-start` and `grid-row-end`.
    pub grid_row: GridPlacement,
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
    /// `justify-items`: where a grid container's items sit across their areas
    /// along the inline axis, unless one says otherwise.
    pub justify_items: AlignItems,
    /// `justify-self`, or `None` for `auto`, which defers to the container's
    /// `justify-items`.
    pub justify_self: Option<AlignItems>,
    /// `align-content`: how the *lines* of a wrapped container share the room
    /// across it. It says nothing at all about a container with one line.
    pub align_content: AlignContent,
    /// `order`: which of its siblings a flex item is laid out among.
    ///
    /// A visual reordering and nothing more — the document order is what a screen
    /// reader and a copy still read, which is why CSS warns against using it for
    /// anything that changes the meaning.
    pub order: i32,
    /// `flex-grow`, read by a flex item.
    pub flex_grow: f32,
    /// `flex-shrink`.
    pub flex_shrink: f32,
    /// `flex-basis`.
    pub flex_basis: FlexBasis,
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
            backgrounds: Vec::new(),
            object_fit: ObjectFit::Fill,
            object_position: BackgroundPosition::CENTER,
            shadows: Vec::new(),
            text_shadows: Vec::new(),
            font_family: FontFamily::default(),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: 400,
            font_style: FontStyle::Normal,
            font_width: 100.0,
            optical_sizing: true,
            font_variations: Arc::from([] as [([u8; 4], f32); 0]),
            font_features: Arc::from([] as [([u8; 4], u16); 0]),
            letter_spacing: 0.0,
            word_spacing: 0.0,
            line_height: LineHeight::Normal,
            list_style: ListStyle::Disc,
            vertical_align: VerticalAlign::Baseline,
            border_spacing: (0.0, 0.0),
            border_collapse: BorderCollapse::Separate,
            box_sizing: BoxSizing::Content,
            opacity: 1.0,
            transform: Arc::from(Vec::new()),
            transform_origin: TransformOrigin::default(),
            white_space: WhiteSpace::Collapse,
            text_wrap: TextWrap::Wrap,
            word_break: WordBreak::Normal,
            overflow_wrap: OverflowWrap::Normal,
            text_decoration: TextDecoration::NONE,
            decorations: Arc::from([] as [TextDecoration; 0]),
            text_transform: TextTransform::NONE,
            margin: Sides::all(LengthOrAuto::ZERO),
            padding: Sides::all(Length::ZERO),
            border: Sides::all(Border::NONE),
            text_align: TextAlign::Start,
            text_align_last: TextAlignLast::Auto,
            text_justify: TextJustify::Auto,
            text_indent: TextIndent::NONE,
            tab_size: TabSize::Spaces(8.0),
            text_overflow: TextOverflow::Clip,
            width: Size::Auto,
            height: Size::Auto,
            min_width: Size::Auto,
            max_width: MaxSize::None,
            min_height: Size::Auto,
            max_height: MaxSize::None,
            aspect_ratio: AspectRatio::Auto,
            float: Float::None,
            clear: Clear::None,
            position: Position::Static,
            inset: Sides::all(LengthOrAuto::Auto),
            z_index: None,
            overflow: Overflow::Visible,
            radius: Corners::SQUARE,
            grid: None,
            grid_column: GridPlacement::AUTO,
            grid_row: GridPlacement::AUTO,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            justify_content: JustifyContent::Stretch,
            align_items: AlignItems::Normal,
            align_self: None,
            justify_items: AlignItems::Normal,
            justify_self: None,
            align_content: AlignContent::Stretch,
            order: 0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::Size(Size::Auto),
            gap: (Length::ZERO, Length::ZERO),
        }
    }
}

impl ComputedStyle {
    /// A style that inherits from `parent` everything CSS says is inherited, and
    /// takes the initial value for everything else.
    ///
    /// For the boxes no element generated: an anonymous box has no style of its
    /// own for the cascade to have computed, and inherits what it can from the
    /// box around it (CSS 2 §9.2.1.1). An element's style is the cascade's, which
    /// does its own inheriting.
    pub fn inheriting_from(parent: &Self) -> Self {
        Self {
            text_shadows: parent.text_shadows.clone(),
            color: parent.color,
            font_family: parent.font_family.clone(),
            font_size: parent.font_size,
            font_weight: parent.font_weight,
            font_style: parent.font_style,
            font_width: parent.font_width,
            optical_sizing: parent.optical_sizing,
            font_variations: Arc::clone(&parent.font_variations),
            font_features: Arc::clone(&parent.font_features),
            letter_spacing: parent.letter_spacing,
            word_spacing: parent.word_spacing,
            line_height: parent.line_height,
            list_style: parent.list_style,
            border_spacing: parent.border_spacing,
            border_collapse: parent.border_collapse,
            white_space: parent.white_space,
            text_wrap: parent.text_wrap,
            word_break: parent.word_break,
            overflow_wrap: parent.overflow_wrap,
            decorations: Arc::clone(&parent.decorations),
            text_transform: parent.text_transform,
            text_align: parent.text_align,
            text_align_last: parent.text_align_last,
            text_justify: parent.text_justify,
            text_indent: parent.text_indent.clone(),
            tab_size: parent.tab_size,
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
            margin: Sides::all(LengthOrAuto::Length(Length::Px(10.0))),
            ..ComputedStyle::default()
        };

        let child = ComputedStyle::inheriting_from(&parent);
        assert_eq!(child.color, parent.color);
        assert_eq!(child.font_size, 24.0);
        assert_eq!(child.display, Display::Inline, "display does not inherit");
        assert_eq!(
            child.margin.top,
            LengthOrAuto::ZERO,
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
        assert_eq!(
            LengthOrAuto::Length(Length::Percent(0.5)).resolve(200.0),
            Some(100.0)
        );
        assert_eq!(
            LengthOrAuto::Length(Length::Px(30.0)).resolve(200.0),
            Some(30.0)
        );
        assert_eq!(LengthOrAuto::Auto.resolve(200.0), None);
        assert_eq!(Length::Percent(0.25).resolve(200.0), 50.0);
    }

    /// A length needs no basis, and a percentage of a size nobody knows is no
    /// size at all — which is the whole of what `definite` decides.
    #[test]
    fn a_percentage_of_an_unknown_size_is_not_a_size() {
        assert_eq!(Length::Px(30.0).definite(None), Some(30.0));
        assert_eq!(Length::Percent(0.5).definite(None), None);
        assert_eq!(Length::Percent(0.5).definite(Some(300.0)), Some(150.0));
    }
}
