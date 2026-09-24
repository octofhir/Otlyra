//! The sizing properties: what `width`, `height` and their minimums and maximums
//! ask of a box.
//!
//! CSS Sizing 3. A size is a length, a percentage of a size that may not be known
//! yet, a keyword that asks the box's own content, `stretch`, or `auto`; and
//! whichever it is, `box-sizing` says whether the number was measured across the
//! content box or the border box. Every formatting context asks the same questions
//! of a box before it can place it, so they are answered here, once, and the
//! answers are content-box sizes: no context adds a box's padding and border to
//! a number the page wrote, because by the time it has the number that has been
//! done.

use otlyra_css::{
    AspectRatio, BoxSizing, ComputedStyle, Display, Intrinsic, Length, MaxSize, Overflow, Ratio,
    Sides, Size,
};

use crate::box_tree::{BoxId, BoxKind};

use super::Flow;
use super::box_model::{resolve_border, resolve_margin, resolve_padding};
use super::intrinsic::Wanted;
use super::replaced::{natural_width, replaced_size};

/// How much a box's padding and border add to it along each axis.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Frame {
    /// Left and right.
    pub(super) inline: f32,
    /// Top and bottom.
    pub(super) block: f32,
}

impl Frame {
    /// The frame a box's resolved padding and border come to.
    pub(super) fn new(padding: Sides<f32>, border: Sides<f32>) -> Self {
        Self {
            inline: padding.left + padding.right + border.left + border.right,
            block: padding.top + padding.bottom + border.top + border.bottom,
        }
    }

    /// The frame of a box whose padding percentages are of `containing_width`.
    pub(super) fn of(style: &ComputedStyle, containing_width: f32) -> Self {
        Self::new(
            resolve_padding(style, containing_width),
            resolve_border(style),
        )
    }
}

/// A size the page wrote for one of the sizing properties, as the content-box
/// size layout lays out.
///
/// `box-sizing: border-box` — which most of the web sets on everything before it
/// writes a single width — measures the number across the border box, so the frame
/// comes *out* of it rather than being added outside it. It applies to all six of
/// the sizing properties alike (CSS UI 3 §3.1), and to `flex-basis`, which is
/// resolved the way `width` is — which is why a minimum, a maximum and a basis go
/// through here as much as a width does.
pub(super) fn content_box(size: f32, sizing: BoxSizing, frame: f32) -> f32 {
    match sizing {
        BoxSizing::Content => size,
        BoxSizing::Border => (size - frame).max(0.0),
    }
}

/// The other way: the number a sizing property would have to say for a box to
/// be `content` across its content box. A widget's preferred size is worked out
/// for its content and written back as the `width` the rest of layout reads, and
/// written in the box `box-sizing` says it is read in.
pub(super) fn declared_size(content: f32, sizing: BoxSizing, frame: f32) -> f32 {
    match sizing {
        BoxSizing::Content => content,
        BoxSizing::Border => content + frame,
    }
}

/// A length one of the sizing properties names, as a content-box size — or `None`
/// when it is a percentage of a size that is not known, which is `auto` (see
/// [`Length::definite`]).
pub(super) fn content_length(
    length: &Length,
    basis: Option<f32>,
    sizing: BoxSizing,
    frame: f32,
) -> Option<f32> {
    length
        .definite(basis)
        .map(|size| content_box(size, sizing, frame))
}

/// A box's preferred aspect ratio (CSS Sizing 4 §4.1): what its width is to its
/// height when one of them is worked out from the other, and where that ratio
/// comes from.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct PreferredRatio {
    /// Width over height.
    ratio: Ratio,
    /// Where it comes from.
    source: RatioSource,
}

/// Where a preferred aspect ratio comes from, which decides the box its two
/// sides are measured across and whether a picture's natural size gives way to
/// it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RatioSource {
    /// A picture's own sides: measured across its content box, and its natural
    /// size keeps its natural height.
    Natural,
    /// `aspect-ratio`: a `<ratio>` measured across the box `box-sizing` names,
    /// or the one beside `auto` for a box without a natural ratio, across the
    /// content box.
    Style(BoxSizing),
}

/// The largest size, in CSS pixels, a ratio makes of a box.
///
/// A page may write a ratio as steep as a number holds, and a width divided by
/// one of those runs past the largest `f32` to infinity, which is no place to
/// put a box, nor the paragraph after it. CSS leaves an implementation's range
/// to it (CSS Values 4 §5.1); this is the top of Blink's, whose sizes are
/// fixed-point numbers with twenty-five bits of whole pixels, so a box a ratio
/// stretches out of all proportion is held where Chrome holds it: finite,
/// placed and painted.
const LARGEST_SIZE: f32 = 33_554_432.0;

impl PreferredRatio {
    /// The box the ratio's two sides are measured across.
    fn across(self) -> BoxSizing {
        match self.source {
            RatioSource::Natural => BoxSizing::Content,
            RatioSource::Style(sizing) => sizing,
        }
    }

    /// Whether a picture with a natural width and nothing said about its size
    /// is as tall as this ratio makes that width, rather than its natural
    /// height: for every ratio but its own (CSS Sizing 4 §4.1), which is how a
    /// four-by-two photograph with `aspect-ratio: 1` is drawn four by four.
    pub(super) fn overrides_natural_height(self) -> bool {
        match self.source {
            RatioSource::Natural => false,
            RatioSource::Style(_) => true,
        }
    }

    /// The content-box height of a box whose content box is `width` wide.
    pub(super) fn height_for(self, width: f32, frame: Frame) -> f32 {
        let ratio = self.ratio.width_over_height();
        let height = match self.across() {
            BoxSizing::Content => width / ratio,
            BoxSizing::Border => ((width + frame.inline) / ratio - frame.block).max(0.0),
        };
        height.min(LARGEST_SIZE)
    }

    /// The content-box width of a box whose content box is `height` tall.
    pub(super) fn width_for(self, height: f32, frame: Frame) -> f32 {
        let ratio = self.ratio.width_over_height();
        let width = match self.across() {
            BoxSizing::Content => height * ratio,
            BoxSizing::Border => ((height + frame.block) * ratio - frame.inline).max(0.0),
        };
        width.min(LARGEST_SIZE)
    }
}

/// A box's preferred aspect ratio, when it has one (CSS Sizing 4 §4.1).
///
/// `natural` is the ratio of a picture's own sides, when it has both. `auto` is
/// that ratio and no other, so a box that is not a picture has none; a
/// `<ratio>` overrides it; and `auto && <ratio>` gives way to it, which is how
/// a stylesheet gives a picture that has not arrived yet the shape it will
/// have.
pub(super) fn preferred_ratio(
    style: &ComputedStyle,
    natural: Option<Ratio>,
) -> Option<PreferredRatio> {
    let natural = natural.map(|ratio| PreferredRatio {
        ratio,
        source: RatioSource::Natural,
    });
    let of_style = |ratio, across| PreferredRatio {
        ratio,
        source: RatioSource::Style(across),
    };
    match applied_aspect_ratio(style) {
        AspectRatio::Auto => natural,
        AspectRatio::Ratio(ratio) => Some(of_style(ratio, style.box_sizing)),
        AspectRatio::AutoOr(ratio) => natural.or(Some(of_style(ratio, BoxSizing::Content))),
    }
}

/// `aspect-ratio` where it applies, which is to every box but an inline box
/// and an internal ruby or table box (CSS Sizing 4 §4.1). A row group, a row
/// and a cell are sized by their table, so for them it is `auto`, its initial
/// value: a picture laid out as a cell keeps its natural ratio and nothing
/// more. A table and its caption are not internal boxes, and keep theirs.
/// There are no ruby boxes here.
///
/// An inline box is told apart by what it is rather than by its `display`,
/// which is also a picture's: a line lays an inline box out and never asks it
/// for a size, and the box kind keeps one out where a size is asked (see
/// [`Flow::width_ratio`]).
fn applied_aspect_ratio(style: &ComputedStyle) -> AspectRatio {
    match style.display {
        Display::TableRowGroup | Display::TableRow | Display::TableCell => AspectRatio::Auto,
        Display::None
        | Display::Block
        | Display::Inline
        | Display::InlineBlock
        | Display::Flex
        | Display::InlineFlex
        | Display::Grid
        | Display::Table
        | Display::TableCaption => style.aspect_ratio,
    }
}

/// The preferred aspect ratio a box's height is taken through (CSS Sizing 4
/// §4.2): its ratio, where it has one and its height is automatic — `auto`, or
/// a percentage of a height nobody knows, which is as automatic as `auto`.
/// `asked` is the height the page asked for, which neither is.
///
/// A content keyword — as `height`, `min-height` or `max-height` — is the
/// content's height here, ratio or none, and this stops short of what Chrome
/// and Firefox do with one on a box with a ratio: they take its min-content
/// height as the height the ratio makes of its width, whatever it holds, and
/// its max-content height as the larger of that and its content. So
/// `width: 200px; aspect-ratio: 2; min-height: min-content` holding six lines
/// is 100 tall there and 120 here.
///
/// A picture's ratio is its own business and is not asked here (see
/// `replaced`).
pub(super) fn height_ratio(style: &ComputedStyle, asked: Option<f32>) -> Option<PreferredRatio> {
    let automatic = asked.is_none()
        && match style.height {
            Size::Auto | Size::Length(_) | Size::Stretch => true,
            Size::Intrinsic(_) => false,
        };
    preferred_ratio(style, None).filter(|_| automatic)
}

/// Whether a box is a scroll container (CSS Overflow 3 §3), which is what takes
/// its automatic minimum sizes away — a flex item's (CSS Flexbox §4.5) and a box
/// with an aspect ratio's (CSS Sizing 4 §4.3) alike: what does not fit it
/// scrolls.
///
/// `overflow` here is one value for both axes, `visible` or `clip`, and `clip`
/// stands for every value but `visible`: `hidden`, `scroll` and `auto`, which
/// make a scroll container, and `clip`, which does not (§3.1). So a box with
/// `overflow: clip` is taken for a scroll container and loses the automatic
/// minimum a browser would keep. This follows the model of `overflow` split
/// by axis, with `clip` apart from `hidden`, once that is in.
pub(super) fn is_scroll_container(style: &ComputedStyle) -> bool {
    match style.overflow {
        Overflow::Visible => false,
        Overflow::Clip => true,
    }
}

/// The fit-content formula (CSS Sizing 3 §3.2), in border-box widths: as wide as
/// the content wants where that fits, as narrow as it can be where even that does
/// not, and the room there is in between.
///
/// It is also what CSS 2.2 §10.3.5 calls shrink-to-fit, which is how a float, an
/// inline block and an absolutely positioned box with an edge free are sized.
fn fit_content(min_content: f32, max_content: f32, available: f32) -> f32 {
    max_content.min(min_content.max(available))
}

/// A minimum and a maximum along one axis.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Limits {
    /// `min-width` or `min-height`: zero when it is `auto`.
    pub(super) min: f32,
    /// `max-width` or `max-height`: infinite when it is `none`.
    pub(super) max: f32,
}

impl Limits {
    /// No minimum and no maximum: what `auto` and `none` ask for.
    pub(super) const NONE: Self = Self {
        min: 0.0,
        max: f32::INFINITY,
    };

    /// A size held between them.
    ///
    /// The maximum first and the minimum second, which is the order CSS 2.2 §10.4
    /// gives them and the reason a minimum larger than the maximum wins.
    pub(super) fn clamp(self, size: f32) -> f32 {
        size.min(self.max).max(self.min)
    }

    /// The same limits on the border box, for a size that already has the frame
    /// in it.
    pub(super) fn outer(self, frame: f32) -> Self {
        Self {
            min: self.min + frame,
            max: self.max + frame,
        }
    }

    /// These limits and `other`'s at once: the larger minimum and the smaller
    /// maximum.
    pub(super) fn intersection(self, other: Self) -> Self {
        Self {
            min: self.min.max(other.min),
            max: self.max.min(other.max),
        }
    }

    /// What `min-width` and `max-width` ask of a box, as content-box widths —
    /// the one reading of them, for a box being laid out, for one being
    /// measured and for a picture alike.
    ///
    /// `auto` is no minimum, which is what the automatic minimum comes to for
    /// everything but a flex item — and a flex container reads `min-width: auto`
    /// for itself. `none` is no maximum. A percentage of a width that is being
    /// measured is cyclic (CSS Sizing 3 §5.2.1): in a maximum it is `none`, and in
    /// a minimum it is resolved against zero rather than dropped, so
    /// `calc(20px + 50%)` still floors a contribution at twenty pixels.
    ///
    /// What a content keyword comes to depends on what the box is, so `keyword`
    /// answers it — and is asked only when there is one, so a box that names a
    /// length pays nothing for it.
    pub(super) fn inline(
        style: &ComputedStyle,
        room: InlineRoom,
        frame: f32,
        mut keyword: impl FnMut(&Intrinsic) -> f32,
    ) -> Self {
        let sizing = style.box_sizing;
        let min = match &style.min_width {
            Size::Auto => 0.0,
            Size::Length(length) => content_box(
                length
                    .definite(room.basis)
                    .unwrap_or_else(|| length.resolve(0.0)),
                sizing,
                frame,
            ),
            Size::Intrinsic(intrinsic) => keyword(intrinsic),
            Size::Stretch => room.stretch(frame).unwrap_or(0.0),
        };
        let max = match &style.max_width {
            MaxSize::None => f32::INFINITY,
            MaxSize::Length(length) => {
                content_length(length, room.basis, sizing, frame).unwrap_or(f32::INFINITY)
            }
            MaxSize::Intrinsic(intrinsic) => keyword(intrinsic),
            MaxSize::Stretch => room.stretch(frame).unwrap_or(f32::INFINITY),
        };
        Self { min, max }
    }
}

/// What a box's sizing properties along one axis come to, as content-box sizes.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Sizes {
    /// `width` or `height`, when it names a size of its own: `None` is `auto`, and
    /// so is a percentage of a size that is not known.
    pub(super) preferred: Option<f32>,
    /// The minimum and the maximum.
    pub(super) limits: Limits,
}

impl Sizes {
    /// What a box that says nothing about its size along an axis asks: `auto`,
    /// with no minimum and no maximum.
    pub(super) const AUTO: Self = Self {
        preferred: None,
        limits: Limits::NONE,
    };

    /// The same sizes, held between `limits` as well as their own.
    pub(super) fn within(self, limits: Limits) -> Self {
        Self {
            limits: self.limits.intersection(limits),
            ..self
        }
    }

    /// The size the box is: what it asked for, or what `auto` came to when it
    /// asked for nothing, held between its minimum and its maximum.
    pub(super) fn used(self, auto: f32) -> f32 {
        self.limits.clamp(self.preferred.unwrap_or(auto))
    }

    /// What it asked for, held between its limits — the size a percentage inside
    /// the box is of, when the box has one before its contents are laid out.
    pub(super) fn definite(self) -> Option<f32> {
        self.preferred.map(|size| self.limits.clamp(size))
    }

    /// The same as a border-box size, for a box whose `auto` is worked out
    /// across its border box — a contribution, what is left between two insets,
    /// the fit-content formula. `auto` is asked only when the box named nothing.
    ///
    /// The frame goes onto a size the page named; one that already has the
    /// frame in it is held between the limits moved out to the border box,
    /// rather than taken apart and put back together, which in floating point
    /// does not always come back to where it started.
    pub(super) fn used_border_box(self, frame: f32, auto: impl FnOnce() -> f32) -> f32 {
        match self.preferred {
            Some(size) => self.limits.clamp(size) + frame,
            None => self.limits.outer(frame).clamp(auto()),
        }
    }
}

/// The room down the block axis that a box's contents are laid out in, as
/// content-box heights: the box's own height, when it has one before they are
/// laid out, and the minimum and maximum it is held between.
///
/// The height is what a percentage height inside the box is of (CSS 2.2
/// §10.5). The limits are what a box as tall as its contents is held between
/// once they are laid out (§10.7), which is late for a flex container: its items
/// are fitted into its size, so it needs the limits before it has laid them out
/// (CSS Flexbox §9.3 step 4, §9.4 step 15).
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct BlockSpace {
    /// The box's height, when it has one that does not wait for its contents:
    /// `None` is a box as tall as they are.
    pub(super) height: Option<f32>,
    /// Its `min-height` and `max-height`, with `box-sizing` already taken out.
    pub(super) limits: Limits,
}

impl BlockSpace {
    /// The room of a box that is exactly `height` tall because whoever laid it
    /// out said so, as a flex line or a flex container does with its items.
    /// `definite` is whether a percentage inside it has that height to be of
    /// (CSS Flexbox §9.8). Either way the box is held to that height, so a flex
    /// container shares out all of it, even where a percentage cannot be of it.
    pub(super) fn exactly(height: f32, definite: bool) -> Self {
        Self {
            height: definite.then_some(height),
            limits: Limits {
                min: height,
                max: height,
            },
        }
    }

    /// How tall the box's content box is when what it holds comes to `content`:
    /// its own height where it has one, and otherwise that content held between
    /// its limits.
    pub(super) fn used(self, content: f32) -> f32 {
        self.height.unwrap_or_else(|| self.limits.clamp(content))
    }

    /// The room to the bit, which is what a measure taken in it is kept by.
    pub(super) fn bits(self) -> SpaceBits {
        SpaceBits {
            height: self.height.map(f32::to_bits),
            min: self.limits.min.to_bits(),
            max: self.limits.max.to_bits(),
        }
    }
}

/// A [`BlockSpace`] to the bit, which is what a box's measured height is kept
/// by: two spaces that differ anywhere are two questions.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct SpaceBits {
    height: Option<u32>,
    min: u32,
    max: u32,
}

/// The room a box's width is worked out in: what CSS Sizing 3 calls the
/// *available space*.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Available {
    /// A number: the containing block's width less the box's own margins, which is
    /// what `stretch` fills and `fit-content` fits into.
    Definite(f32),
    /// None at all, because the box's contribution to its container's intrinsic
    /// size is being measured: under a min-content constraint `fit-content` comes
    /// out as the narrowest the content can be, and under a max-content one as the
    /// widest.
    Measuring(Wanted),
}

/// What the inline-axis sizing properties of one box are resolved against.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct InlineRoom {
    /// The width the box's content is measured at: its containing block's, which
    /// its own padding and its descendants' percentages resolve against.
    pub(super) measure: f32,
    /// The width a percentage in `width`, `min-width` or `max-width` is of.
    /// `None` while the box's contribution to that very width is being measured,
    /// where a percentage of it is cyclic (CSS Sizing 3 §5.2.1).
    pub(super) basis: Option<f32>,
    /// The room there is.
    available: Available,
}

impl InlineRoom {
    /// The room of a box laid out in a containing block `containing` wide, of
    /// which `available` is left for its border box.
    pub(super) fn laid_out(containing: f32, available: f32) -> Self {
        Self {
            measure: containing,
            basis: Some(containing),
            available: Available::Definite(available.max(0.0)),
        }
    }

    /// The room of a box laid out in a containing block `containing` wide, once
    /// its own margins are out of it — an `auto` one counting as none, which is
    /// the room CSS Sizing 3 says a box can stretch into.
    pub(super) fn within(style: &ComputedStyle, containing: f32) -> Self {
        let margin = resolve_margin(style, containing);
        Self::laid_out(containing, containing - margin.left - margin.right)
    }

    /// The room of a box whose contribution to its container is being measured.
    ///
    /// `basis` is what a percentage of the box's own width is of, which for the
    /// contents of a box being measured is nothing, since it is the width being
    /// measured; a table, whose cells' percentages are of the table, is the one
    /// caller that has a number to give.
    pub(super) fn measuring(measure: f32, basis: Option<f32>, wanted: Wanted) -> Self {
        Self {
            measure,
            basis,
            available: Available::Measuring(wanted),
        }
    }

    /// What `stretch` fills, as a content-box width: nothing, while a
    /// contribution is measured — there it behaves as `auto` (CSS Sizing 4,
    /// `stretch`).
    fn stretch(self, frame: f32) -> Option<f32> {
        match self.available {
            Available::Definite(room) => Some((room - frame).max(0.0)),
            Available::Measuring(_) => None,
        }
    }

    /// Which contribution is being measured with the box's own percentages
    /// cyclic, or `None` when they have a width to be a percentage of.
    fn cyclic(self) -> Option<Wanted> {
        match (self.basis, self.available) {
            (None, Available::Measuring(wanted)) => Some(wanted),
            (Some(_), _) | (None, Available::Definite(_)) => None,
        }
    }
}

/// The width of a box that is its own content, which a picture and a widget
/// are: it has one whatever it holds and whatever its `width` says (CSS Sizing 3
/// §5.1), and it is both its min-content and its max-content size.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct OwnWidth {
    /// The content-box width it comes to with nothing said about its width: a
    /// picture's own, or the width its height makes of it through its ratio; a
    /// widget's natural width.
    pub(super) natural: f32,
}

/// What `width`, `min-width` and `max-width` ask of a replaced box, as
/// content-box widths.
///
/// A picture has no content to measure but itself, so the content keywords are
/// its own width, `own.natural` (CSS Sizing 3 §5.1): as a width and as a limit
/// alike.
///
/// And while its contribution is measured, a percentage of the width being
/// measured is cyclic, which a replaced box resolves rather than drops
/// (§5.2.1). For its min-content contribution it is resolved against zero, so a
/// column is free to be narrower than a picture with `max-width: 100%` — which
/// is what §5.2.2 calls a compressible replaced element, and the rule nearly
/// every stylesheet on the web opens with. That is a rule about what the
/// picture asks of its container and not about the picture: its min-content
/// *size* is still its own width, which is what a flex item may not be shrunk
/// below.
///
/// For its max-content contribution the percentage is `auto` and `none`, and
/// the specification counts on the container to hold the picture back — a
/// grid's `auto` tracks stretch to fill it (CSS Grid §11.8), a table shares its
/// own width out. Neither does that here yet, so what the percentage would come
/// to in the width it is measured in stands in as a maximum: an 800-pixel
/// picture in a 400-pixel float or table cell asks for 400, and a 64-pixel one
/// with `width: 100%` for 64.
pub(super) fn replaced_widths(
    style: &ComputedStyle,
    room: InlineRoom,
    frame: f32,
    own: OwnWidth,
) -> Sizes {
    let sizes = |room: InlineRoom| Sizes {
        preferred: match &style.width {
            Size::Auto => None,
            Size::Length(length) => content_length(length, room.basis, style.box_sizing, frame),
            Size::Intrinsic(_) => Some(own.natural),
            Size::Stretch => room.stretch(frame),
        },
        limits: Limits::inline(style, room, frame, |_| own.natural),
    };
    match room.cyclic() {
        None => sizes(room),
        Some(Wanted::Narrowest) => sizes(InlineRoom {
            basis: Some(0.0),
            ..room
        }),
        Some(Wanted::Widest) => {
            let mut sizes = sizes(room);
            let percentages = [
                match &style.width {
                    Size::Length(length) => Some(length),
                    Size::Auto | Size::Intrinsic(_) | Size::Stretch => None,
                },
                match &style.max_width {
                    MaxSize::Length(length) => Some(length),
                    MaxSize::None | MaxSize::Intrinsic(_) | MaxSize::Stretch => None,
                },
            ];
            sizes.limits.max = percentages
                .into_iter()
                .flatten()
                .filter(|length| length.definite(None).is_none())
                .map(|length| content_box(length.resolve(room.measure), style.box_sizing, frame))
                .fold(sizes.limits.max, f32::min);
            sizes
        }
    }
}

/// What the block-axis sizing properties of one box are resolved against.
#[derive(Copy, Clone, Debug, PartialEq)]
struct BlockRoom {
    /// The containing block's height, when it has one that does not depend on
    /// what is in it. A percentage of any other is no size at all
    /// (CSS 2.2 §10.5).
    basis: Option<f32>,
    /// The border-box height `stretch` fills: that height less the box's own
    /// vertical margins, when there is one.
    available: Option<f32>,
}

impl BlockRoom {
    /// What `stretch` fills, as a content-box height.
    fn stretch(self, frame: f32) -> Option<f32> {
        self.available.map(|available| (available - frame).max(0.0))
    }
}

/// What `height`, `min-height` and `max-height` ask of a box, as content-box
/// heights.
///
/// Down the block axis the content keywords are the content's own height (CSS
/// Sizing 3 §3.2): a `height` of `min-content` is `auto`, and a minimum or a
/// maximum of one is the height the content came to. `content` is that height
/// once the box has been laid out; before, there is no such number, and a keyword
/// limit holds nothing yet.
fn block_sizes(style: &ComputedStyle, room: BlockRoom, frame: f32, content: Option<f32>) -> Sizes {
    let length = |length: &Length| content_length(length, room.basis, style.box_sizing, frame);

    let preferred = match &style.height {
        Size::Auto | Size::Intrinsic(_) => None,
        Size::Length(size) => length(size),
        Size::Stretch => room.stretch(frame),
    };
    // A percentage minimum against a height nobody knows is zero and a percentage
    // maximum is `none` — CSS 2.2 §10.7, and the initial value of each.
    let min = match &style.min_height {
        Size::Auto => None,
        Size::Length(size) => length(size),
        Size::Intrinsic(_) => content,
        Size::Stretch => room.stretch(frame),
    };
    let max = match &style.max_height {
        MaxSize::None => None,
        MaxSize::Length(size) => length(size),
        MaxSize::Intrinsic(_) => content,
        MaxSize::Stretch => room.stretch(frame),
    };

    Sizes {
        preferred,
        limits: Limits {
            min: min.unwrap_or(0.0),
            max: max.unwrap_or(f32::INFINITY),
        },
    }
}

/// What `height`, `min-height` and `max-height` ask of a box laid out in a
/// containing block `containing_width` wide, against the containing block's
/// height when it has one of its own. `content` is what the content keywords
/// come to, when that is known.
pub(super) fn block_sizes_in(
    style: &ComputedStyle,
    containing_width: f32,
    containing_height: Option<f32>,
    content: Option<f32>,
) -> Sizes {
    let frame = Frame::of(style, containing_width);
    // `stretch` fills what the box's own margins leave, and a percentage margin
    // is of the width even here.
    let margin = resolve_margin(style, containing_width);
    block_sizes(
        style,
        BlockRoom {
            basis: containing_height,
            available: containing_height
                .map(|height| (height - margin.top - margin.bottom).max(0.0)),
        },
        frame.block,
        content,
    )
}

/// What `height`, `min-height` and `max-height` ask of a box whose height its
/// preferred aspect ratio makes of its width (see [`height_ratio`]), when its
/// content box is `inline_size` wide: the width through the ratio (CSS Sizing 4
/// §4.2). The height is definite, as the width it comes from is, so a
/// percentage inside the box has it to be of.
///
/// And it is no less than the box's content, which is the automatic minimum
/// of §4.3: `min-height: auto` is the min-content height, which is `content`
/// once the box has been laid out, so text longer than the ratio has room for
/// grows the box rather than spilling out of it. Not for a scroll container,
/// which keeps its shape and scrolls, nor where the page named a minimum of
/// its own. The limits are left as they are, and hold the height as they hold
/// any other.
fn ratio_height(
    style: &ComputedStyle,
    sizes: Sizes,
    ratio: PreferredRatio,
    frame: Frame,
    inline_size: f32,
    content: Option<f32>,
) -> Sizes {
    let through_ratio = ratio.height_for(inline_size, frame);
    let content_floor = match (&style.min_height, content) {
        (Size::Auto, Some(content)) if !is_scroll_container(style) => content,
        (Size::Auto | Size::Length(_) | Size::Intrinsic(_) | Size::Stretch, _) => 0.0,
    };
    Sizes {
        preferred: Some(through_ratio.max(content_floor)),
        ..sizes
    }
}

/// CSS Sizing 4 §4.4: a box's definite minimum and maximum heights, carried
/// through its preferred aspect ratio onto its width, where its own
/// `min-width` and `max-width` say nothing — which is how `aspect-ratio: 16/9;
/// max-height: 80vh` keeps a video's shape on a tall, narrow screen rather than
/// letting it run the width of the page. These are the limits carried, with
/// no minimum and no maximum where nothing is.
///
/// A carried minimum is no more than a width or a maximum the page named, and
/// a carried maximum no less than a width or a minimum it named, or than the
/// carried minimum where there is one: a ratio never overrules a width that
/// was asked for.
///
/// Only this way round. The other, a box's width limits carried onto an
/// automatic height, changes nothing: that height is taken through the ratio
/// from a width its limits have already held.
fn transfer_height_limits(
    style: &ComputedStyle,
    widths: Sizes,
    heights: Limits,
    ratio: PreferredRatio,
    frame: Frame,
) -> Limits {
    let Sizes { preferred, limits } = widths;
    let min = match style.min_width {
        Size::Auto if heights.min > 0.0 => Some(
            ratio
                .width_for(heights.min, frame)
                .min(preferred.unwrap_or(f32::INFINITY))
                .min(limits.max),
        ),
        Size::Auto | Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => None,
    };
    let max = match style.max_width {
        MaxSize::None if heights.max.is_finite() => Some(
            ratio
                .width_for(heights.max, frame)
                .max(preferred.unwrap_or(0.0))
                .max(limits.min)
                .max(min.unwrap_or(0.0)),
        ),
        MaxSize::None | MaxSize::Length(_) | MaxSize::Intrinsic(_) | MaxSize::Stretch => None,
    };
    Limits {
        min: min.unwrap_or(Limits::NONE.min),
        max: max.unwrap_or(Limits::NONE.max),
    }
}

impl<'a> Flow<'a> {
    /// What `width`, `min-width` and `max-width` ask of a box, as content-box
    /// widths, held between the limits its heights carry onto its width
    /// through its preferred aspect ratio as well (see
    /// [`Self::transferred_width_limits`]) — what a block, a float, an inline
    /// block and a positioned box are sized by.
    pub(super) fn inline_sizes(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
        frame: f32,
    ) -> Sizes {
        let named = self.named_inline_sizes(id, style, room, frame);
        named.within(self.transferred_width_limits(id, style, room, named))
    }

    /// What `width`, `min-width` and `max-width` ask of a box, as content-box
    /// widths: what the page named and nothing a ratio carried over, which is
    /// what a flex item's main size and its size across are held between.
    ///
    /// For a box being laid out and for one whose contribution to its container
    /// is being measured alike: `room` says which. A content keyword is the box's
    /// own content measured, which a picture and a widget answer from their own
    /// width instead (see [`replaced_widths`]).
    ///
    /// What a box's `auto` width comes to through its preferred aspect ratio is
    /// the formatting context's to ask (see [`Self::ratio_width`]): it is what
    /// `auto` means there, not a width the page asked for.
    pub(super) fn named_inline_sizes(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
        frame: f32,
    ) -> Sizes {
        let heights_against = self.heights_against(room);
        if let Some(own) = self.own_width(id, style, room.measure, heights_against) {
            return replaced_widths(style, room, frame, own);
        }
        let preferred = self.preferred_width(id, style, &style.width, room, frame);
        let limits = Limits::inline(style, room, frame, |keyword| {
            self.intrinsic_width(id, style, keyword, room, frame)
        });
        Sizes { preferred, limits }
    }

    /// The limits a box's definite heights carry onto its width through its
    /// preferred aspect ratio (see [`transfer_height_limits`]), against the
    /// widths the page `named`: no minimum and no maximum for a box that has
    /// no ratio, or whose width is its own (see [`Self::width_ratio`]).
    ///
    /// Kept apart from the named ones because a flex item holds only what its
    /// content asks for by them — its content size suggestion (CSS Flexbox
    /// §4.5) among it — and not the minimum and maximum its main size and its
    /// stretched size across are held between.
    pub(super) fn transferred_width_limits(
        &self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
        named: Sizes,
    ) -> Limits {
        match self.width_ratio(id, style) {
            Some(ratio) => transfer_height_limits(
                style,
                named,
                block_sizes_in(style, room.measure, self.heights_against(room), None).limits,
                ratio,
                Frame::of(style, room.measure),
            ),
            None => Limits::NONE,
        }
    }

    /// The preferred aspect ratio a box's width is worked out through here
    /// (CSS Sizing 4 §4.1).
    ///
    /// `None` for a box with no ratio; for a box whose width is its own — a
    /// picture, whose ratio is already in the width it has (see `replaced`),
    /// and a widget with a natural width; and for an inline box and a run of
    /// text, which `aspect-ratio` does not apply to, since a line sizes them.
    pub(super) fn width_ratio(&self, id: BoxId, style: &ComputedStyle) -> Option<PreferredRatio> {
        let node = self.tree.node(id);
        match &node.kind {
            BoxKind::Block if node.natural_size().width.is_none() => preferred_ratio(style, None),
            BoxKind::Block | BoxKind::Replaced(_) | BoxKind::Inline | BoxKind::Text(_) => None,
        }
    }

    /// The height of the containing block a box's own heights are resolved
    /// against while its width is worked out: this context's, for a box being
    /// laid out, and none while its contribution to its container is measured.
    ///
    /// There a percentage height is taken for `auto`. That is right where the
    /// containing block's height waits for its contents, and short of what
    /// browsers do where it does not: they resolve it, so a picture at `height:
    /// 100%` in a float of a set height makes the float as wide as the picture
    /// is at that height (CSS Sizing 4 §4.4, example). Measuring here does not
    /// yet carry each measured box's height down to what it holds.
    fn heights_against(&self, room: InlineRoom) -> Option<f32> {
        match room.available {
            Available::Definite(_) => self.containing_height,
            Available::Measuring(_) => None,
        }
    }

    /// The width of a box that is its own content — a picture, or a widget that
    /// has a natural width — or `None` for a box whose width is what its
    /// contents come to.
    ///
    /// A picture's is worked out without its own `width`, `min-width` and
    /// `max-width`, which are what it asks of its container rather than what it
    /// is, but with its heights, which it takes its width from when it has a
    /// ratio: a picture told to be fifty pixels tall is a hundred wide at two to
    /// one, whatever holds it. A percentage height is of `containing_height`,
    /// the containing block's height where it has one — a picture at `height:
    /// 100%` in a header of a set height is as wide as that height makes it —
    /// and `auto` where it has none.
    pub(super) fn own_width(
        &self,
        id: BoxId,
        style: &ComputedStyle,
        containing_width: f32,
        containing_height: Option<f32>,
    ) -> Option<OwnWidth> {
        let node = self.tree.node(id);
        match &node.kind {
            BoxKind::Replaced(content) => Some(OwnWidth {
                natural: natural_width(style, content, containing_width, containing_height),
            }),
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => node
                .natural_size()
                .width
                .map(|natural| OwnWidth { natural }),
        }
    }

    /// The content-box width `size` names for a box, as a preferred width: what
    /// `width` says, and what `flex-basis` says along a row, which takes the same
    /// values. `None` is `auto`, which each formatting context answers its own way.
    pub(super) fn preferred_width(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        size: &Size,
        room: InlineRoom,
        frame: f32,
    ) -> Option<f32> {
        match size {
            Size::Auto => None,
            Size::Length(length) => content_length(length, room.basis, style.box_sizing, frame),
            Size::Intrinsic(keyword) => Some(self.intrinsic_width(id, style, keyword, room, frame)),
            Size::Stretch => room.stretch(frame),
        }
    }

    /// The content-box width one of the content keywords asks for (CSS Sizing 3
    /// §3.2).
    ///
    /// The keywords name the box's own content, so they are measured without the
    /// box's own `width` — a box that says `width: min-content` is asking what its
    /// content needs, not what it said. And they are sizes of the whole box, which
    /// `box-sizing` has nothing to say about: the frame is part of what the
    /// content needs whichever box a length would have been measured across.
    fn intrinsic_width(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        keyword: &Intrinsic,
        room: InlineRoom,
        frame: f32,
    ) -> f32 {
        let border_box = match keyword {
            Intrinsic::MinContent => self.min_content_size(id, room.measure),
            Intrinsic::MaxContent => self.max_content_size(id, room.measure),
            Intrinsic::FitContent(None) => self.fit_content_size(id, room),
            Intrinsic::FitContent(Some(limit)) => {
                // `fit-content(<length-percentage>)` fits into the length it names
                // rather than into the room there is. The length is a width like
                // any other, measured across the box `box-sizing` says; a
                // percentage of a width being measured is cyclic, and the room
                // stands in for it.
                match content_length(limit, room.basis, style.box_sizing, frame) {
                    Some(limit) => fit_content(
                        self.min_content_size(id, room.measure),
                        self.max_content_size(id, room.measure),
                        limit + frame,
                    ),
                    None => self.fit_content_size(id, room),
                }
            }
        };
        (border_box - frame).max(0.0)
    }

    /// The border-box width the fit-content formula gives a box's content in
    /// `room`.
    pub(super) fn fit_content_size(&mut self, id: BoxId, room: InlineRoom) -> f32 {
        match room.available {
            Available::Definite(available) => fit_content(
                self.min_content_size(id, room.measure),
                self.max_content_size(id, room.measure),
                available,
            ),
            Available::Measuring(wanted) => self.content_size(id, room.measure, wanted),
        }
    }

    /// The content-box width a box's automatic width comes to through its
    /// preferred aspect ratio, when it has one and a definite height to take
    /// the width from (CSS Sizing 4 §4.2) — or `None`, and the formatting
    /// context answers `auto` its own way.
    ///
    /// The height is held between its own limits before it is carried over, as
    /// a picture's is. And the width is no less than the box's min-content
    /// width where `min-width` is `auto` and the box does not scroll: §4.3's
    /// automatic minimum works in both axes, so a square a hundred pixels tall
    /// holding a word a hundred and fifty wide is a hundred and fifty wide.
    ///
    /// A picture and a widget have widths of their own and are not asked (see
    /// [`Self::width_ratio`]).
    pub(super) fn ratio_width(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
    ) -> Option<f32> {
        let ratio = self.width_ratio(id, style)?;
        let height =
            block_sizes_in(style, room.measure, self.heights_against(room), None).definite()?;
        let frame = Frame::of(style, room.measure);
        let width = ratio.width_for(height, frame);
        Some(match style.min_width {
            Size::Auto if !is_scroll_container(style) => {
                width.max(self.min_content_size(id, room.measure) - frame.inline)
            }
            Size::Auto | Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => width,
        })
    }

    /// The border-box width `auto` comes to for a box: through its preferred
    /// aspect ratio where that has one to give it (see [`Self::ratio_width`]),
    /// and otherwise what `otherwise` — the formatting context's own answer —
    /// makes of it.
    pub(super) fn automatic_width(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
        frame: f32,
        otherwise: impl FnOnce(&mut Self) -> f32,
    ) -> f32 {
        match self.ratio_width(id, style, room) {
            Some(width) => width + frame,
            None => otherwise(self),
        }
    }

    /// The border-box width of a box whose `auto` width shrinks to fit: a float,
    /// an inline block, and an absolutely positioned box with an edge left free.
    ///
    /// Whatever its own `width` says wins; `auto` is the width its preferred
    /// aspect ratio gives it where it has one and a definite height, and the
    /// fit-content formula against the room there is otherwise; and the
    /// minimum and maximum hold either.
    ///
    /// A picture does not shrink to fit anything. Floated or positioned, its
    /// width is worked out as it is on a line (CSS 2.2 §10.3.6, §10.3.8), by
    /// the same function: from its ratio and whatever height it is given, a
    /// percentage of the containing block's included.
    pub(super) fn shrink_to_fit_width(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
        frame: f32,
    ) -> f32 {
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            return replaced_size(style, content, room, self.containing_height).0 + frame;
        }
        let sizes = self.inline_sizes(id, style, room, frame);
        sizes.used_border_box(frame, || {
            self.automatic_width(id, style, room, frame, |flow| {
                flow.fit_content_size(id, room)
            })
        })
    }

    /// What `height`, `min-height` and `max-height` ask of a box laid out in a
    /// containing block `containing_width` wide, against the containing block
    /// height this context has — the one question every block-axis caller asks.
    ///
    /// `inline_size` is the width of the box's own content box, which an
    /// automatic height is taken from when the box has a preferred aspect
    /// ratio (see [`ratio_height`]).
    pub(super) fn block_sizes_of(
        &self,
        style: &ComputedStyle,
        containing_width: f32,
        inline_size: f32,
        content: Option<f32>,
    ) -> Sizes {
        let sizes = block_sizes_in(style, containing_width, self.containing_height, content);
        match height_ratio(style, sizes.preferred) {
            Some(ratio) => ratio_height(
                style,
                sizes,
                ratio,
                Frame::of(style, containing_width),
                inline_size,
                content,
            ),
            None => sizes,
        }
    }

    /// The content-box height a box asks for, when it asks for one that means
    /// anything here: a length, or a percentage of a containing block that has a
    /// height of its own. A keyword and a percentage of a height nobody knows are
    /// both `auto`, and so is a height a preferred aspect ratio would make of
    /// the box's width, which is not one the page asked for.
    pub(super) fn asked_height(&self, style: &ComputedStyle, containing_width: f32) -> Option<f32> {
        block_sizes_in(style, containing_width, self.containing_height, None).preferred
    }

    /// The block space a box's own contents are laid out in, when its content
    /// box is `inline_size` wide: its content height, when it has one before
    /// they are laid out, held between its minimum and its maximum — which a
    /// box as tall as its contents has not, because it cannot answer a
    /// question its contents are asking — and those limits themselves.
    pub(super) fn content_space(
        &self,
        style: &ComputedStyle,
        containing_width: f32,
        inline_size: f32,
    ) -> BlockSpace {
        let sizes = self.block_sizes_of(style, containing_width, inline_size, None);
        BlockSpace {
            height: sizes.definite(),
            limits: sizes.limits,
        }
    }
}
