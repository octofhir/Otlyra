//! One flex item: what its container needs to know of it before the lines are
//! formed (CSS Flexbox §9.2), how big it is across once its main size is known
//! (§9.4, step 7), and the one layout it gets, at the rectangle the container
//! settled for it.

use std::sync::Arc;

use otlyra_css::{AlignItems, ComputedStyle, FlexBasis, LengthOrAuto, MaxSize, Sides, Size};

use crate::box_tree::{BoxId, BoxKind};
use crate::flow::Flow;
use crate::flow::box_model::{resolve_border, resolve_margin, resolve_padding};
use crate::flow::intrinsic::Wanted;
use crate::flow::replaced::{
    picture_heights, ratio, replaced_fragment, replaced_height, replaced_size,
};
use crate::flow::sizing::{
    BlockSpace, Frame, InlineRoom, Limits, OwnWidth, Sizes, block_sizes_in, content_length,
    is_scroll_container,
};
use crate::fragment::{Fragment, Rect};

use super::Container;
use super::lines::Flexible;

/// The height a flex item is laid out to, as the container decided it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum ItemHeight {
    /// As tall as what it holds at the width it was given, held between its own
    /// minimum and maximum, and at least this: an item the line does not
    /// stretch.
    AtLeast(f32),
    /// Exactly this, as a border box, whatever it holds: what a line stretched
    /// it to, and down a column what the sharing out left it. `definite` is
    /// whether a percentage inside it has that height to be of (CSS Flexbox
    /// §9.8) — a stretched item's does, and a column item's does when the
    /// container had a height of its own to share out.
    Exactly { height: f32, definite: bool },
}

/// One child of a flex container that takes part in flex layout, with the sizes
/// the container works out for it.
///
/// Filled in phase by phase, as CSS Flexbox §9 works them out: the base size and
/// the limits on the main size when the item is collected (§9.2, step 3), the
/// main size when its line is shared out (§9.7), and along a row the cross size
/// once that main size is known (§9.4, step 7). Down a column the cross size is known from the start,
/// because the height the base size is read from depends on it.
pub(super) struct FlexItem {
    pub(super) id: BoxId,
    pub(super) style: Arc<ComputedStyle>,
    /// Its margins, with `auto` as zero; its padding; and its border. A
    /// percentage in the first two is of the container's inner width, whichever
    /// way the container runs (CSS Flexbox §4.2).
    pub(super) margin: Sides<f32>,
    pub(super) padding: Sides<f32>,
    pub(super) border: Sides<f32>,
    /// Its used minimum and maximum main sizes, as a border box: `auto` as a
    /// minimum is its automatic minimum size (§4.5).
    pub(super) limits: Limits,
    /// Its flex base size, as a border box, which its limits have no say in.
    pub(super) base: f32,
    /// Its size along the main axis once its line is shared out.
    pub(super) main: f32,
    /// Its hypothetical cross size, as a border box: along a row, nothing until
    /// the width its line gives it is known.
    pub(super) cross: f32,
    /// Its minimum and maximum across, as a border box: what `stretch` is held
    /// between.
    pub(super) cross_limits: Limits,
    /// How it is aligned across its line: its `align-self`, or its container's
    /// `align-items` where that is `auto` (CSS Flexbox §8.3).
    pub(super) align: AlignItems,
    /// Whether `stretch` fills its line with it (§9.4, step 11), which it
    /// decides before anything is sized: down a column an item the line does
    /// not stretch is sized to fit its content instead.
    pub(super) fills_line: bool,
    pub(super) grow: f32,
    pub(super) shrink: f32,
}

impl FlexItem {
    /// Which of this item's margins along one axis are `auto`, leading then
    /// trailing.
    ///
    /// `horizontal` names the axis rather than the container's direction, so the
    /// cross axis is asked for by passing the opposite of `row`.
    pub(super) fn auto_margin_sides(&self, horizontal: bool) -> (bool, bool) {
        auto_margin_sides(&self.style, horizontal)
    }

    /// How many of them there are, which is what the free space is split between.
    pub(super) fn auto_margins_main(&self, row: bool) -> usize {
        let (lead, trail) = self.auto_margin_sides(row);
        usize::from(lead) + usize::from(trail)
    }

    /// The margins that take room along the main axis.
    pub(super) fn margin_main(&self, row: bool) -> f32 {
        if row {
            self.margin.left + self.margin.right
        } else {
            self.margin.top + self.margin.bottom
        }
    }

    /// The margins that take room across it.
    pub(super) fn margin_cross(&self, row: bool) -> f32 {
        if row {
            self.margin.top + self.margin.bottom
        } else {
            self.margin.left + self.margin.right
        }
    }

    /// What its padding and border add to it.
    fn frame(&self) -> Frame {
        Frame::new(self.padding, self.border)
    }

    /// The width its contents are laid out in when its border box is `width`
    /// wide.
    fn content_width(&self, width: f32) -> f32 {
        content_width(self.frame(), width)
    }

    /// What breaking the lines and sharing out its line need of it.
    pub(super) fn flexible(&self, row: bool) -> Flexible {
        let frame = self.frame();
        Flexible {
            base: self.base,
            limits: self.limits,
            frame: if row { frame.inline } else { frame.block },
            margins: self.margin_main(row),
            grow: self.grow,
            shrink: self.shrink,
        }
    }
}

/// Which of a box's margins along one axis are `auto`, leading then trailing:
/// `horizontal` names the axis.
fn auto_margin_sides(style: &ComputedStyle, horizontal: bool) -> (bool, bool) {
    let margin = &style.margin;
    if horizontal {
        (
            margin.left == LengthOrAuto::Auto,
            margin.right == LengthOrAuto::Auto,
        )
    } else {
        (
            margin.top == LengthOrAuto::Auto,
            margin.bottom == LengthOrAuto::Auto,
        )
    }
}

/// The size an item that `stretch` fills a line `line` big with comes to across
/// it, as a border box: the line less the item's margins, held between its
/// limits (CSS Flexbox §9.4, step 11).
///
/// One function for the stretching and for the width a column item is measured
/// at, so the width it is laid out at is the very width it was measured at.
pub(super) fn stretched(limits: Limits, line: f32, margins: f32) -> f32 {
    limits.clamp((line - margins).max(0.0))
}

/// The width an item's contents are laid out in when its border box is `width`
/// wide: the one subtraction its measure and its layout both make, so that the
/// two ask about the same width to the bit and the second finds what the first
/// kept.
fn content_width(frame: Frame, width: f32) -> f32 {
    (width - frame.inline).max(0.0)
}

/// An item's used minimum and maximum main sizes, as a border box (CSS Flexbox
/// §9.2, step 3): what its hypothetical main size, and its target main size
/// while its line is shared out (§9.7), are held between.
///
/// `min` is its `min-width` or `min-height`, whichever runs along the main
/// axis, `sizes` what its sizing properties along that axis came to, and
/// `specified` the `width` or `height` among them the page named, with `frame`
/// its padding and border there. `content` is its content size suggestion
/// along the axis, as a border box, which only an `auto` minimum asks for.
fn main_limits(
    style: &ComputedStyle,
    min: &Size,
    sizes: Sizes,
    specified: Option<f32>,
    frame: f32,
    content: impl FnOnce() -> f32,
) -> Limits {
    let limits = sizes.limits.outer(frame);
    match min {
        Size::Auto => Limits {
            min: automatic_minimum(style, sizes, specified, frame, content),
            ..limits
        },
        Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => limits,
    }
}

/// What `auto` comes to as a flex item's minimum along the main axis: its
/// automatic minimum size (CSS Flexbox §4.5), as a border box.
///
/// It is what lets an item shrink, but not past the point where its own
/// content spills out of it. A scroll container's is a content box of
/// nothing, as any other box's `auto` minimum is: what does not fit it
/// scrolls. Any other item's is its content-based minimum size — the smaller
/// of its content size suggestion, which is `content`, and its specified size
/// suggestion, which is `specified`: its `width` or `height` where that names
/// a definite size; its `flex-basis` has no say in it — held to its maximum
/// main size, so that it never outweighs a maximum the page wrote.
/// `min-width: 0` and `overflow: hidden` are the two ways a page lets an item
/// be narrower than its longest word, which is how a title truncated beside a
/// button is written.
///
/// An item with a preferred aspect ratio has its content size suggestion
/// already held between the limits across carried through the ratio (CSS
/// Sizing 4 §4.4) when it arrives as `content`. And where its size across is
/// definite, `content` is what the ratio makes of it, which for a picture is
/// §4.5's transferred size suggestion as well: the smaller of the two is
/// either.
fn automatic_minimum(
    style: &ComputedStyle,
    sizes: Sizes,
    specified: Option<f32>,
    frame: f32,
    content: impl FnOnce() -> f32,
) -> f32 {
    if is_scroll_container(style) {
        return frame;
    }
    let content = content();
    let content_based = match specified {
        Some(specified) => content.min(specified + frame),
        None => content,
    };
    content_based.min(sizes.limits.max + frame)
}

/// Whether a content keyword among a box's block-axis limits asks what its
/// content comes to, which only a measure can answer (CSS Sizing 3 §3.2).
fn limits_ask_content(style: &ComputedStyle) -> bool {
    matches!(style.min_height, Size::Intrinsic(_))
        || matches!(style.max_height, MaxSize::Intrinsic(_))
}

/// One item as it is collected, with what its sizing properties asked for: what
/// its base size and its limits are worked out from.
struct ItemSizing<'s> {
    id: BoxId,
    style: &'s ComputedStyle,
    /// Its margins, with `auto` as zero.
    margin: Sides<f32>,
    frame: Frame,
    /// The container's width, less the item's own margins.
    room: InlineRoom,
    /// What `width`, `min-width` and `max-width` came to: the minimum and
    /// maximum its main size or its stretched size across is held between.
    widths: Sizes,
    /// The limits its heights carry onto its width through its preferred
    /// aspect ratio (CSS Sizing 4 §4.4), which hold what its content asks for
    /// — its content size suggestion (CSS Flexbox §4.5), a base size taken
    /// from its content, a width that fits its content — and not the item.
    transferred: Limits,
    /// The width it has of its own, when it is a picture or a widget: taken
    /// with the container's height, so a percentage height is of that.
    own: Option<OwnWidth>,
    /// Whether `stretch` fills its line with it (see [`FlexItem::fills_line`]).
    fills_line: bool,
}

/// What collecting an item settles: its base size and its limits along the main
/// axis, and down a column its cross size and the limits on it.
struct Collected {
    base: f32,
    limits: Limits,
    cross: f32,
    cross_limits: Limits,
}

impl<'a> Flow<'a> {
    /// The in-flow children of a flex container as flex items, each with its
    /// flex base size and its used minimum and maximum main sizes (CSS Flexbox
    /// §9.2, step 3).
    ///
    /// A box that has left the flow is not a flex item (§4.1). It takes no room
    /// on the line, nothing is shared out with it, and it is placed against its
    /// containing block like any other positioned box — which is also where it
    /// picks up the clipping and the layer its own style asks for. Treated as an
    /// item it was laid out by a path that knows nothing about either, which is
    /// how a heading hidden in the way every accessibility helper hides one — a
    /// one-pixel box with its overflow cut off — came out as three lines of text
    /// across the page.
    pub(super) fn collect_items(
        &mut self,
        parent: BoxId,
        container: Container,
        static_y: f32,
        out: &mut Vec<Fragment>,
    ) -> Vec<FlexItem> {
        let children = self.tree.node(parent).children.clone();
        let mut items = Vec::with_capacity(children.len());
        for child in children {
            let style = Arc::clone(&self.tree.node(child).style);
            if style.position.is_out_of_flow() {
                let fragment = self.layout_positioned(child, static_y);
                out.push(fragment);
                continue;
            }
            items.push(self.collect_item(child, style, container));
        }
        items
    }

    fn collect_item(
        &mut self,
        id: BoxId,
        style: Arc<ComputedStyle>,
        container: Container,
    ) -> FlexItem {
        let margin = resolve_margin(&style, container.width);
        let padding = resolve_padding(&style, container.width);
        let border = resolve_border(&style);
        let frame = Frame::new(padding, border);
        let room = InlineRoom::within(&style, container.width);
        let widths = self.named_inline_sizes(id, &style, room, frame.inline);
        let transferred = self.transferred_width_limits(id, &style, room, widths);
        let own = self.own_width(id, &style, container.width, container.space.height);
        let align = style.align_self.unwrap_or(container.align_items);
        let fills_line = align == AlignItems::Stretch && self.may_stretch(id, &style, container);
        let sizing = ItemSizing {
            id,
            style: &style,
            margin,
            frame,
            room,
            widths,
            transferred,
            own,
            fills_line,
        };
        let collected = if container.row {
            self.collect_row_item(&sizing, container)
        } else {
            self.collect_column_item(&sizing, container)
        };
        FlexItem {
            id,
            margin,
            padding,
            border,
            limits: collected.limits,
            base: collected.base,
            main: collected.base,
            cross: collected.cross,
            cross_limits: collected.cross_limits,
            align,
            fills_line,
            grow: style.flex_grow,
            shrink: style.flex_shrink,
            style,
        }
    }

    /// Whether `stretch` may fill the line with an item (CSS Flexbox §9.4, step
    /// 11): its size across is `auto`, and neither of its margins across is.
    ///
    /// An item that named a size across keeps it: `align-items: stretch` is the
    /// initial value, so stretching over a declared height would make every
    /// `height` in a flex container a suggestion. And an `auto` margin across
    /// takes the room before `stretch` and `align-self` see any (§8.1):
    /// `margin: 0 auto` on a column of content is how a page is centred, and a
    /// container that handed that room to the alignment left the page against
    /// the left edge — the reason the same declaration means *centre me* in a
    /// block and in a flex item alike.
    ///
    /// A picture's `width` and `height` attributes are sizes it named: HTML
    /// maps them onto the properties (HTML §15.4.3), so an icon written
    /// `<svg height=16>` keeps its sixteen pixels in a taller row rather than
    /// being drawn the height of the row.
    fn may_stretch(&self, id: BoxId, style: &ComputedStyle, container: Container) -> bool {
        let hint = match &self.tree.node(id).kind {
            BoxKind::Replaced(content) => content.hint,
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => (None, None),
        };
        let sized = if container.row {
            hint.1.is_some() || self.asked_height(style, container.width).is_some()
        } else {
            hint.0.is_some() || style.width != Size::Auto
        };
        !sized && auto_margin_sides(style, !container.row) == (false, false)
    }

    /// Along a row, the base size and the limits come from the item's widths and
    /// from what its content wants, and nothing is laid out: `flex-basis`, then
    /// `width`, then its content size, which is kept. How tall the item is
    /// waits for the width its line gives it (see [`Self::size_row_item_across`]).
    ///
    /// Its content size is its max-content width for the base and its
    /// min-content width for the automatic minimum — except where it has a
    /// preferred aspect ratio and a size across that is definite already, where
    /// both are the width the ratio makes of that size (§9.2.3 B; see
    /// [`Self::width_through_ratio`]); and a picture's is its own width, taken
    /// with the container's height, so `height: 100%` in a header of a set
    /// height is as wide as that height makes it. Either is held between the
    /// limits its heights carry over through a ratio (CSS Sizing 4 §4.4): a
    /// square no more than fifty pixels tall asks for no more than fifty
    /// across, but may be grown past it, and a basis the page named is not
    /// held at all.
    ///
    /// The base size is not held between the item's minimum and maximum, nor to
    /// the row (§9.2, step 3): its hypothetical size is, and how much of what
    /// the row is missing each item gives up is weighed by the base.
    fn collect_row_item(&mut self, item: &ItemSizing<'_>, container: Container) -> Collected {
        let (id, measure, frame) = (item.id, item.room.measure, item.frame.inline);
        let through_ratio = self.width_through_ratio(item, container);
        let content = |flow: &mut Self, wanted: Wanted| {
            let content = match (through_ratio, item.own) {
                (Some(width), _) => width + frame,
                (None, Some(own)) => own.natural + frame,
                (None, None) => flow.content_size(id, measure, wanted),
            };
            item.transferred.outer(frame).clamp(content)
        };
        let base = match (self.named_basis(item, true, None), &item.style.flex_basis) {
            (Some(basis), _) => basis,
            (None, FlexBasis::Content) => content(self, Wanted::Widest),
            (None, FlexBasis::Size(_)) => match item.widths.preferred {
                Some(width) => width + frame,
                None => content(self, Wanted::Widest),
            },
        };
        let limits = main_limits(
            item.style,
            &item.style.min_width,
            item.widths,
            item.widths.preferred,
            frame,
            || content(self, Wanted::Narrowest),
        );
        Collected {
            base,
            limits,
            cross: 0.0,
            cross_limits: Limits::NONE,
        }
    }

    /// The content-box width a row item's preferred aspect ratio makes of its
    /// size across, where that is definite before its line is sized (CSS
    /// Flexbox §9.8): a line it is stretched to in a single-line container of a
    /// definite height, held between its limits, or a height of its own.
    ///
    /// It is the base size of an item whose width is `auto` (§9.2.3 B) — a
    /// four-by-two picture stretched down a sixty-pixel header is a hundred
    /// and twenty wide, not four hundred — and its content size suggestion
    /// (§4.5). `None` for an item with no ratio or no such size, and for a
    /// widget, whose width is its own.
    fn width_through_ratio(&self, item: &ItemSizing<'_>, container: Container) -> Option<f32> {
        let (style, frame) = (item.style, item.frame);
        let (ratio, heights) = match &self.tree.node(item.id).kind {
            BoxKind::Replaced(content) => (
                ratio(style, content),
                picture_heights(style, content, container.width, container.space.height),
            ),
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => (
                self.width_ratio(item.id, style),
                block_sizes_in(style, container.width, container.space.height, None),
            ),
        };
        let ratio = ratio?;
        let across = match container.space.height {
            Some(line) if item.fills_line && container.single_line => {
                let margins = item.margin.top + item.margin.bottom;
                (stretched(heights.limits.outer(frame.block), line, margins) - frame.block).max(0.0)
            }
            Some(_) | None => heights.definite()?,
        };
        Some(ratio.width_for(across, frame))
    }

    /// Down a column, the item's width comes first, and its height at that
    /// width is the one measure it takes: the base size of an item whose
    /// `flex-basis` names nothing, and the automatic minimum of one whose
    /// `min-height` is `auto`.
    ///
    /// An item that fills the line of a single-line container is as wide as
    /// the container less its margins, held between its limits: that width is
    /// definite before the line is sized (§9.8). Any other with an `auto` width
    /// is `fit-content` (§9.4, step 7): as wide as its content wants, but no
    /// wider than the container less its margins unless its content cannot be
    /// any narrower, and held between its limits and those its heights carry
    /// over through a ratio (CSS Sizing 4 §4.4). That is an item aligned
    /// anything but `stretch` — `baseline` among them, which down a column is
    /// `start`, since an item's inline axis is the cross axis there (§8.3) — an
    /// item with an `auto` margin across, which the free space belongs to
    /// rather than to the item, and an item stretched in a container that
    /// wraps, whose line is not sized until every item on it is: the line
    /// stretches it afterwards, and its main size stays what this width made
    /// of it. A picture that is not stretched is its own width, and a box with
    /// a preferred aspect ratio and a height of its own is the width the ratio
    /// makes of it.
    ///
    /// An item with a ratio and no height of its own is as tall as the ratio
    /// makes its width, or as its content where that is taller (CSS Sizing 4
    /// §4.2, §4.3): that is its base size (§9.2.3 B) and its content size
    /// suggestion both. A picture stretched across a single line is the same:
    /// it is as tall as its stretched width makes it — a two-to-one picture
    /// across a three-hundred-pixel column is a hundred and fifty tall, not the
    /// height it came with. In a container that wraps it keeps that height and
    /// is only widened.
    fn collect_column_item(&mut self, item: &ItemSizing<'_>, container: Container) -> Collected {
        let (frame, style) = (item.frame, item.style);
        let cross_limits = item.widths.limits.outer(frame.inline);
        let stretched_across = (item.fills_line && container.single_line).then(|| {
            stretched(
                cross_limits,
                container.width,
                item.margin.left + item.margin.right,
            )
        });
        let picture = match &self.tree.node(item.id).kind {
            BoxKind::Replaced(content) => Some(match stretched_across {
                Some(cross) => {
                    let height = replaced_height(
                        style,
                        content,
                        content_width(frame, cross),
                        container.width,
                        container.space.height,
                    );
                    (cross, height)
                }
                None => {
                    let (width, height) =
                        replaced_size(style, content, item.room, container.space.height);
                    (width + frame.inline, height)
                }
            }),
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => None,
        };

        let cross = match (picture, stretched_across) {
            (Some((cross, _)), _) | (None, Some(cross)) => cross,
            (None, None) => {
                item.widths
                    .within(item.transferred)
                    .used_border_box(frame.inline, || {
                        self.automatic_width(item.id, style, item.room, frame.inline, |flow| {
                            flow.fit_content_size(item.id, item.room)
                        })
                    })
            }
        };
        let inner = content_width(frame, cross);
        let content = match picture {
            Some((_, height)) => height,
            None => {
                let space = self.content_space(style, container.width, inner);
                self.content_block_size(item.id, inner, space)
            }
        };
        let heights = match picture {
            Some(_) => block_sizes_in(
                style,
                container.width,
                container.space.height,
                Some(content),
            ),
            None => self.block_sizes_of(style, container.width, inner, Some(content)),
        };
        let specified = self.asked_height(style, container.width);
        // How tall it is as a box at that width, before its own limits have a
        // say: its `height`, or with none what its ratio makes of its width or
        // what it holds. A picture's height is the one its ratio and its limits
        // already gave it.
        let automatic = match picture {
            Some(_) => content,
            None => heights.preferred.unwrap_or(content),
        };
        let block = automatic + frame.block;

        let base = self
            .named_basis(item, false, container.space.height)
            .unwrap_or(block);
        // `min-height: auto` is the automatic minimum (CSS Flexbox §4.5): what
        // the item holds, or its own `height` where that is less. It is not a
        // nicety: `flex-basis: 0` in a column with no height of its own leaves
        // every item with a base of nothing and no free space to grow into, so
        // a stack of cards came out as a stack of nothing.
        let suggestion = match specified {
            Some(_) => content,
            None => automatic,
        };
        let limits = main_limits(
            style,
            &style.min_height,
            heights,
            specified,
            frame.block,
            || suggestion + frame.block,
        );
        Collected {
            base,
            limits,
            cross,
            cross_limits,
        }
    }

    /// The base size `flex-basis` names, as a border box, when it names one
    /// (CSS Flexbox §9.2, step 3A).
    ///
    /// Measured across the box `box-sizing` says, as a width is. A percentage
    /// basis down a column whose height is not known is `content` (§7.2.3), and so
    /// are the content keywords there; `None` leaves the base to the item's own
    /// size and its content.
    fn named_basis(
        &mut self,
        item: &ItemSizing<'_>,
        row: bool,
        main_height: Option<f32>,
    ) -> Option<f32> {
        match &item.style.flex_basis {
            FlexBasis::Content | FlexBasis::Size(Size::Auto) => None,
            FlexBasis::Size(size) if row => self
                .preferred_width(item.id, item.style, size, item.room, item.frame.inline)
                .map(|basis| basis + item.frame.inline),
            FlexBasis::Size(Size::Length(length)) => {
                content_length(length, main_height, item.style.box_sizing, item.frame.block)
                    .map(|basis| basis + item.frame.block)
            }
            FlexBasis::Size(Size::Intrinsic(_) | Size::Stretch) => None,
        }
    }

    /// A row item's hypothetical cross size (CSS Flexbox §9.4, step 7): how tall
    /// it is at the main size its line gave it, and the limits `stretch` holds it
    /// between.
    ///
    /// Its content is measured rather than laid out, and a measure is kept by the
    /// width it was taken at, so the item's final layout — and every later
    /// layout of the container that holds it — asks nothing new. An item that
    /// names a height is that tall whatever it holds, and is not measured at all
    /// unless a content keyword among its limits asks what its content comes to.
    /// A picture is as tall as that width makes it through its ratio.
    pub(super) fn size_row_item_across(&mut self, item: &mut FlexItem, containing_width: f32) {
        let frame = item.frame();
        let content_width = item.content_width(item.main);
        let (height, heights) = match &self.tree.node(item.id).kind {
            BoxKind::Replaced(content) => {
                let height = replaced_height(
                    &item.style,
                    content,
                    content_width,
                    containing_width,
                    self.containing_height,
                );
                let heights = block_sizes_in(
                    &item.style,
                    containing_width,
                    self.containing_height,
                    Some(height),
                );
                (height, heights)
            }
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => {
                // Measured unless it named a height, which it is whatever it
                // holds; a height its ratio makes of its width is at least what
                // it holds (CSS Sizing 4 §4.3), so that one is measured too.
                let space = self.content_space(&item.style, containing_width, content_width);
                let named = self.asked_height(&item.style, containing_width).is_some();
                let content = (!named || limits_ask_content(&item.style))
                    .then(|| self.content_block_size(item.id, content_width, space));
                let heights =
                    self.block_sizes_of(&item.style, containing_width, content_width, content);
                // `auto` is asked for only where nothing was named, and then the
                // content was measured.
                (heights.used(content.unwrap_or_default()), heights)
            }
        };
        item.cross = height + frame.block;
        item.cross_limits = heights.limits.outer(frame.block);
    }

    /// One flex item laid out for good, at the rectangle the container decided
    /// for it: the only time this container lays it out, since everything
    /// before this only measured it.
    ///
    /// Its contents are the root of a formatting context of their own (CSS
    /// Flexbox §4), and what does not fit is cut off at its padding edge, and
    /// scrolls, where `overflow` says so — here, where it is, and nowhere else.
    pub(super) fn layout_item(
        &mut self,
        item: &FlexItem,
        origin: (f32, f32),
        width: f32,
        height: ItemHeight,
        containing_width: f32,
    ) -> Fragment {
        let (x, y) = origin;
        let style = &item.style;
        let frame = item.frame();
        let content_width = item.content_width(width);

        // A picture is its own content: the container decided the outer size,
        // the frame comes out of it, and a height the line left open is what
        // the width makes of the picture through its ratio.
        if let BoxKind::Replaced(content) = &self.tree.node(item.id).kind {
            let content_height = match height {
                ItemHeight::Exactly { height, .. } => (height - frame.block).max(0.0),
                ItemHeight::AtLeast(floor) => replaced_height(
                    style,
                    content,
                    content_width,
                    containing_width,
                    self.containing_height,
                )
                .max(floor - frame.block),
            };
            return replaced_fragment(
                item.id,
                style,
                content.image.clone(),
                origin,
                (content_width, content_height),
                containing_width,
            );
        }

        let (padding, border) = (item.padding, item.border);
        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;

        // A height of its own is the height it gets, whatever it holds — an item
        // that overflows the size it asked for is what CSS says happens — and a
        // percentage inside it is of that height.
        let height = match height {
            ItemHeight::AtLeast(floor) => match self.asked_height(style, containing_width) {
                Some(_) => ItemHeight::Exactly {
                    height: floor,
                    definite: true,
                },
                None => ItemHeight::AtLeast(floor),
            },
            exactly @ ItemHeight::Exactly { .. } => exactly,
        };
        // Its contents are laid out in the height the container gave it, where
        // it gave one, and otherwise in what the item's own limits allow.
        let space = match height {
            ItemHeight::Exactly { height, definite } => {
                BlockSpace::exactly((height - frame.block).max(0.0), definite)
            }
            ItemHeight::AtLeast(_) => self.content_space(style, containing_width, content_width),
        };
        let mut children = Vec::new();
        let content_height = self.layout_independent(
            item.id,
            content_width,
            (content_x, content_y),
            space,
            &mut children,
        );
        let outer_height = match height {
            ItemHeight::Exactly { height, .. } => height,
            ItemHeight::AtLeast(floor) => {
                let content_height = self.content_height(item.id, content_height);
                let held = self
                    .block_sizes_of(style, containing_width, content_width, Some(content_height))
                    .used(content_height);
                floor.max(held + frame.block)
            }
        };

        if style.overflow == otlyra_css::Overflow::Clip {
            let padding_box = Rect::new(
                x + border.left,
                y + border.top,
                (width - border.left - border.right).max(0.0),
                (outer_height - border.top - border.bottom).max(0.0),
            );
            self.clip_overflow(item.id, padding_box, padding, &mut children);
        }

        Fragment {
            used: Some(crate::UsedEdges {
                margin: item.margin,
                border,
                padding,
            }),
            ..Fragment::for_box(
                item.id,
                Rect::new(x, y, width, outer_height),
                Arc::clone(style),
                children,
            )
        }
    }
}
