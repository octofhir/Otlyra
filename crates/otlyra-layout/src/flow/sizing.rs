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

use otlyra_css::{BoxSizing, ComputedStyle, Intrinsic, Length, MaxSize, Sides, Size};

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
        limits: Limits {
            min: 0.0,
            max: f32::INFINITY,
        },
    };

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
    /// What a picture's `width` attribute asked for, which HTML makes a rule of
    /// the lowest priority setting `width`: it stands in for an `auto` width, and
    /// any width a stylesheet names outranks it.
    pub(super) hint: Option<f32>,
}

/// What `width`, `min-width` and `max-width` ask of a replaced box, as
/// content-box widths.
///
/// A picture has no content to measure but itself, so the content keywords are
/// its own width, `own.natural` (CSS Sizing 3 §5.1): as a width and as a limit
/// alike, and ahead of a `width` attribute, which a keyword outranks as any
/// stylesheet does.
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
            Size::Auto => own
                .hint
                .map(|hint| content_box(hint, style.box_sizing, frame)),
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

impl<'a> Flow<'a> {
    /// What `width`, `min-width` and `max-width` ask of a box, as content-box
    /// widths.
    ///
    /// For a box being laid out and for one whose contribution to its container
    /// is being measured alike: `room` says which. A content keyword is the box's
    /// own content measured, which a picture and a widget answer from their own
    /// width instead (see [`replaced_widths`]).
    pub(super) fn inline_sizes(
        &mut self,
        id: BoxId,
        style: &ComputedStyle,
        room: InlineRoom,
        frame: f32,
    ) -> Sizes {
        if let Some(own) = self.own_width(id, style, room.measure) {
            return replaced_widths(style, room, frame, own);
        }
        let preferred = self.preferred_width(id, style, &style.width, room, frame);
        let limits = Limits::inline(style, room, frame, |keyword| {
            self.intrinsic_width(id, style, keyword, room, frame)
        });
        Sizes { preferred, limits }
    }

    /// The width of a box that is its own content — a picture, or a widget that
    /// has a natural width — or `None` for a box whose width is what its
    /// contents come to.
    ///
    /// A picture's is worked out without its own `width`, `min-width` and
    /// `max-width`, which are what it asks of its container rather than what it
    /// is, but with its heights, which it takes its width from when it has a
    /// ratio: a picture told to be fifty pixels tall is a hundred wide at two to
    /// one, whatever holds it. A percentage height is of a containing block
    /// nobody is asking about while the box is measured, so it is `auto` here.
    pub(super) fn own_width(
        &self,
        id: BoxId,
        style: &ComputedStyle,
        containing_width: f32,
    ) -> Option<OwnWidth> {
        let node = self.tree.node(id);
        match &node.kind {
            BoxKind::Replaced(content) => Some(OwnWidth {
                natural: natural_width(style, content, containing_width),
                hint: content.hint.0,
            }),
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => {
                node.natural_size().width.map(|natural| OwnWidth {
                    natural,
                    hint: None,
                })
            }
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
    fn fit_content_size(&mut self, id: BoxId, room: InlineRoom) -> f32 {
        match room.available {
            Available::Definite(available) => fit_content(
                self.min_content_size(id, room.measure),
                self.max_content_size(id, room.measure),
                available,
            ),
            Available::Measuring(wanted) => self.content_size(id, room.measure, wanted),
        }
    }

    /// The border-box width of a box whose `auto` width shrinks to fit: a float,
    /// an inline block, and an absolutely positioned box with an edge left free.
    ///
    /// Whatever its own `width` says wins; `auto` is the fit-content formula
    /// against the room there is; and the minimum and maximum hold either.
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
        self.inline_sizes(id, style, room, frame)
            .used_border_box(frame, || self.fit_content_size(id, room))
    }

    /// What `height`, `min-height` and `max-height` ask of a box laid out in a
    /// containing block `containing_width` wide, against the containing block
    /// height this context has — the one question every block-axis caller asks.
    pub(super) fn block_sizes_of(
        &self,
        style: &ComputedStyle,
        containing_width: f32,
        content: Option<f32>,
    ) -> Sizes {
        block_sizes_in(style, containing_width, self.containing_height, content)
    }

    /// The content-box height a box asks for, when it asks for one that means
    /// anything here: a length, or a percentage of a containing block that has a
    /// height of its own. A keyword and a percentage of a height nobody knows are
    /// both `auto`.
    pub(super) fn asked_height(&self, style: &ComputedStyle, containing_width: f32) -> Option<f32> {
        self.block_sizes_of(style, containing_width, None).preferred
    }

    /// The height to resolve the percentages *inside* a box against: its own
    /// content height, when it has one before its contents are laid out, held
    /// between its minimum and its maximum. A box as tall as its contents has
    /// none, because it cannot answer a question its contents are asking.
    pub(super) fn inner_height(&self, style: &ComputedStyle, containing_width: f32) -> Option<f32> {
        self.block_sizes_of(style, containing_width, None)
            .definite()
    }
}
