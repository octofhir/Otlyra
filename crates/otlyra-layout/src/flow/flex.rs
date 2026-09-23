//! Flex layout: items along one axis, sharing out the room on it.
//!
//! Every item is measured at its base size, the items are broken into lines, and
//! what each line has left over or is missing is shared out by `flex-grow` and
//! `flex-shrink`; the line then places its items across. The item and the line
//! are this context's own types, and nothing else reads them.

use std::sync::Arc;

use otlyra_css::{
    AlignItems, ComputedStyle, FlexBasis, FlexWrap, JustifyContent, LengthOrAuto, Sides, Size,
};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{Fragment, Rect};

use super::Flow;
use super::box_model::{resolve_border, resolve_margin, resolve_padding};
use super::replaced::{replaced_fragment, replaced_height};
use super::sizing::{Frame, InlineRoom, Limits, Sizes, content_length};

/// How far a laid-out flex line reached, on each axis.
#[derive(Copy, Clone)]
struct PlacedLine {
    cross: f32,
    main: f32,
}

/// Where one flex line sits and how much room it has.
#[derive(Copy, Clone)]
struct FlexLine {
    /// Whether the main axis is horizontal.
    row: bool,
    /// Between two items on the line.
    gap: f32,
    /// The main-axis size the items are fitted into, or infinite when there is
    /// nothing to fit them into.
    inner: f32,
    /// Where the line starts across the container.
    cross_start: f32,
    /// How big the line is across.
    cross: LineCross,
}

/// How big a flex line is across the container.
#[derive(Copy, Clone, Debug, PartialEq)]
enum LineCross {
    /// As big as its biggest item, and at least this: the line of a container
    /// as big across as what it holds, and each of the lines a wrapped container
    /// shares its own cross size out between.
    AtLeast(f32),
    /// Exactly this: the one line of a container that cannot wrap and has a
    /// cross size of its own (CSS Flexbox §9.4, step 8). An item bigger than
    /// that overflows the line rather than growing it — which is how a sidebar
    /// in a row as tall as the window is as tall as the window, and scrolls.
    Exactly(f32),
}

/// The height a flex item is laid out to, as the container decided it.
#[derive(Copy, Clone, Debug, PartialEq)]
enum ItemHeight {
    /// As tall as what it holds at the width it was given, held between its own
    /// minimum and maximum, and at least this: an item the line does not
    /// stretch, and one being measured before there is a line to stretch it to.
    AtLeast(f32),
    /// Exactly this, as a border box, whatever it holds: what a line stretched
    /// it to, and down a column what the sharing out left it. `definite` is
    /// whether a percentage inside it has that height to be of (CSS Flexbox
    /// §9.8) — a stretched item's does, and a column item's does when the
    /// container had a height of its own to share out.
    Exactly { height: f32, definite: bool },
}

/// One child of a flex container, with the sizes the container needs to place it.
struct FlexItem {
    id: BoxId,
    style: Arc<ComputedStyle>,
    /// The smallest it may be shrunk to along the main axis.
    floor: f32,
    /// Its size along the main axis before growing or shrinking.
    base: f32,
    /// Its size along the main axis after.
    main: f32,
    /// Its size across.
    cross: f32,
    /// Its minimum and maximum across, as a border box: what `stretch` is held
    /// between.
    cross_limits: Limits,
    grow: f32,
    shrink: f32,
    margin: Sides<f32>,
}

impl FlexItem {
    /// Which of this item's margins along one axis are `auto`, leading then
    /// trailing.
    ///
    /// `horizontal` names the axis rather than the container's direction, so the
    /// cross axis is asked for by passing the opposite of `row`.
    fn auto_margin_sides(&self, horizontal: bool) -> (bool, bool) {
        use otlyra_css::LengthOrAuto::Auto;
        let margin = &self.style.margin;
        if horizontal {
            (margin.left == Auto, margin.right == Auto)
        } else {
            (margin.top == Auto, margin.bottom == Auto)
        }
    }

    /// How many of them there are, which is what the free space is split between.
    fn auto_margins_main(&self, row: bool) -> usize {
        let (lead, trail) = self.auto_margin_sides(row);
        usize::from(lead) + usize::from(trail)
    }

    /// The margins that take room along the main axis.
    fn margin_main(&self, row: bool) -> f32 {
        if row {
            self.margin.left + self.margin.right
        } else {
            self.margin.top + self.margin.bottom
        }
    }

    /// The margins that take room across it.
    fn margin_cross(&self, row: bool) -> f32 {
        if row {
            self.margin.top + self.margin.bottom
        } else {
            self.margin.left + self.margin.right
        }
    }
}

/// One item as it came out laid out on its own, with what its sizing properties
/// asked for: what the base size and the floor of it are worked out from.
struct Measured<'s> {
    id: BoxId,
    style: &'s ComputedStyle,
    /// Its border box, laid out as a block in the container's width.
    laid_out: Rect,
    frame: Frame,
    /// The container's width, less the item's own margins.
    room: InlineRoom,
    /// What `width`, `min-width` and `max-width` came to.
    widths: Sizes,
}

/// How a wrapped container's `align-content` shares `leftover` between `count`
/// lines: how much goes before the first, how much between each pair, and how much
/// each line grows by.
///
/// Only `stretch` — the initial value, and the reason a wrapped container's lines
/// fill it — grows the lines; the rest move them and leave them the size they are.
fn share_across(align: otlyra_css::AlignContent, leftover: f32, count: usize) -> (f32, f32, f32) {
    use otlyra_css::AlignContent;

    let lines = count.max(1) as f32;
    match align {
        AlignContent::Stretch => (0.0, 0.0, leftover / lines),
        AlignContent::Start => (0.0, 0.0, 0.0),
        AlignContent::End => (leftover, 0.0, 0.0),
        AlignContent::Center => (leftover / 2.0, 0.0, 0.0),
        // Nothing at the ends, and nothing to share where there is one line.
        AlignContent::SpaceBetween if count > 1 => (0.0, leftover / (lines - 1.0), 0.0),
        AlignContent::SpaceBetween => (0.0, 0.0, 0.0),
        AlignContent::SpaceAround => (leftover / (lines * 2.0), leftover / lines, 0.0),
        AlignContent::SpaceEvenly => (leftover / (lines + 1.0), leftover / (lines + 1.0), 0.0),
    }
}

impl<'a> Flow<'a> {
    /// A flex formatting context: the children are items along one axis.
    ///
    /// One pass in each direction. Along the main axis every item is measured at
    /// its base size, and what is left over — or missing — is shared out by
    /// `flex-grow` and `flex-shrink`; across the cross axis each item is placed by
    /// `align-items`, stretching to the line by default, which is what makes
    /// columns of equal height without anyone saying how tall.
    pub(super) fn layout_flex(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(parent).style);
        let children = self.tree.node(parent).children.clone();
        let row = style.flex_direction.is_row();
        let gap = if row {
            style.gap.1.resolve(width)
        } else {
            style.gap.0.resolve(width)
        };

        // A float outside the container does not reach into it: a flex container
        // establishes a formatting context of its own.
        let outer_floats = std::mem::take(&mut self.floats);

        // A container with a height of its own has a definite cross size when it
        // is a row and a definite main size when it is a column: either way it is
        // the size the items are fitted into rather than one they add up to. It
        // is the height whoever laid the container out settled before its
        // contents — its own `height`, or the one a flex line gave it — which is
        // the height a percentage inside it is of.
        let definite_height = self.containing_height;

        // The base size every item starts at, measured by laying it out on its own.
        let mut items: Vec<FlexItem> = Vec::with_capacity(children.len());
        for &child in &children {
            let item_style = Arc::clone(&self.tree.node(child).style);

            // A box that has left the flow is not a flex item. It takes no room
            // on the line, nothing is shared out with it, and it is placed
            // against its containing block like any other positioned box — which
            // is also where it picks up the clipping and the layer its own style
            // asks for. Treated as an item it was laid out by a path that knows
            // nothing about either, which is how a heading hidden in the way
            // every accessibility helper hides one — a one-pixel box with its
            // overflow cut off — came out as three lines of text across the page.
            if item_style.position.is_out_of_flow() {
                let fragment = self.layout_positioned(child, y);
                out.push(fragment);
                continue;
            }
            // Laid out on its own to be measured, and thrown away: a box inside
            // it that scrolls is a scroll port once, where it is laid out for
            // good, and not again where it was measured.
            let ports = self.scroll_ports.len();
            let fragment = self.layout_block(child, width, x, y);
            self.scroll_ports.truncate(ports);
            let margin = resolve_margin(&item_style, width);
            let room = InlineRoom::within(&item_style, width);
            let frame = Frame::of(&item_style, width);
            let measured = Measured {
                id: child,
                style: &item_style,
                laid_out: fragment.rect,
                frame,
                room,
                widths: self.inline_sizes(child, &item_style, room, frame.inline),
            };
            let basis = self.base_size(&measured, row, definite_height);
            let floor = self.main_floor(&measured, row, basis);

            // Across the line, an item with an `auto` margin on that side is an
            // item the free space belongs to — so it cannot also be the item that
            // filled the line. Laid out on its own it took the whole width, the
            // way a block does; what it is owed is the width its content wants,
            // and the margin gets the rest.
            let cross_auto_margin = !row
                && (item_style.margin.left == LengthOrAuto::Auto
                    || item_style.margin.right == LengthOrAuto::Auto)
                && measured.widths.preferred.is_none();
            let cross = if row {
                fragment.rect.height
            } else if cross_auto_margin {
                self.wanted_width(&measured)
            } else {
                fragment.rect.width
            };
            let cross_limits = if row {
                let content = (fragment.rect.height - frame.block).max(0.0);
                self.block_sizes_of(&item_style, width, Some(content))
                    .limits
                    .outer(frame.block)
            } else {
                measured.widths.limits.outer(frame.inline)
            };

            items.push(FlexItem {
                id: child,
                floor,
                base: basis,
                main: basis,
                cross,
                cross_limits,
                grow: item_style.flex_grow,
                shrink: item_style.flex_shrink,
                margin,
                style: item_style,
            });
        }

        if items.is_empty() {
            self.floats = outer_floats;
            return 0.0;
        }

        // `order` reorders the items and nothing else: it is a visual arrangement,
        // and the document order is what a screen reader and a copy still read. A
        // stable sort, so items that name the same order keep the order they were
        // written in — which is what CSS says and is the whole difference between
        // `order` and a shuffle.
        if items.iter().any(|item| item.style.order != 0) {
            items.sort_by_key(|item| item.style.order);
        }

        let inner = if row {
            width
        } else {
            definite_height.unwrap_or(f32::INFINITY)
        };

        // Lines: one, unless wrapping is allowed and the items do not fit on it.
        let lines: Vec<std::ops::Range<usize>> =
            if style.flex_wrap == FlexWrap::NoWrap || !inner.is_finite() {
                std::iter::once(0..items.len()).collect()
            } else {
                let mut lines = Vec::new();
                let mut start = 0;
                let mut used = 0.0;
                for (index, item) in items.iter().enumerate() {
                    let outer = item.base + item.margin_main(row);
                    let with_gap = if index == start { outer } else { outer + gap };
                    if index > start && used + with_gap > inner {
                        lines.push(start..index);
                        start = index;
                        used = outer;
                    } else {
                        used += with_gap;
                    }
                }
                lines.push(start..items.len());
                lines
            };

        let line_count = lines.len();
        let cross_gap = if row { style.gap.0.resolve(width) } else { gap };
        // The cross size the container has to give out, and what the lines want of
        // it. A row's is its own height, where it has one; a column's is always its
        // width.
        let container_cross = if row { definite_height } else { Some(width) };
        let wanted: Vec<f32> = lines
            .iter()
            .map(|line| {
                items[line.clone()]
                    .iter()
                    .map(|item| item.cross + item.margin_cross(row))
                    .fold(0.0f32, f32::max)
            })
            .collect();
        let leftover = container_cross.map_or(0.0, |cross| {
            (cross - wanted.iter().sum::<f32>() - cross_gap * (line_count - 1) as f32).max(0.0)
        });

        // A container that cannot wrap has one line, and that line fills whatever
        // cross size the container has — which is what makes `align-items: center`
        // centre against the container rather than against the tallest item.
        // `align-content` says nothing about such a container, so it is not asked.
        let unwrapped = style.flex_wrap == FlexWrap::NoWrap;
        let (lead, between, stretch) = if unwrapped {
            (0.0, 0.0, leftover)
        } else {
            share_across(style.align_content, leftover, line_count)
        };

        let mut cross_cursor = lead;
        let mut main_extent = 0.0f32;
        for (number, line) in lines.into_iter().enumerate() {
            let placed = self.layout_flex_line(
                &mut items,
                line,
                &style,
                FlexLine {
                    row,
                    gap,
                    inner,
                    cross_start: cross_cursor,
                    cross: match container_cross {
                        Some(cross) if unwrapped => LineCross::Exactly(cross),
                        Some(_) => LineCross::AtLeast(wanted[number] + stretch),
                        None => LineCross::AtLeast(0.0),
                    },
                },
                (x, y),
                out,
            );
            cross_cursor += placed.cross;
            main_extent = main_extent.max(placed.main);
            if number + 1 < line_count {
                cross_cursor += cross_gap + between;
            }
        }
        // The room the lines were told to leave after them is still room they take.
        if container_cross.is_some() {
            cross_cursor += lead;
        }

        self.floats = outer_floats;

        // The height a flex container takes: across the lines when it is a row,
        // along the longest of them when it is a column.
        if row { cross_cursor } else { main_extent }
    }

    /// What an item's content wants of the row, as a border box held between its
    /// limits: the base size of an item with nothing to say about its own width,
    /// and the width of one that is pushed about by an `auto` margin rather than
    /// stretched. Never more than the row, which is where the shrinking starts.
    fn wanted_width(&mut self, item: &Measured<'_>) -> f32 {
        let row = item.room.measure;
        item.widths
            .limits
            .outer(item.frame.inline)
            .clamp(self.max_content_size(item.id, row).min(row))
    }

    /// An item's base size along the main axis (CSS Flexbox §9.2.3), as a border
    /// box.
    ///
    /// `flex-basis` if it names a size, measured across the box `box-sizing` says
    /// as a width is; then the item's own size, and for an `auto` width along a
    /// row the size its content wants — a flex item is not a block, and does not
    /// fill the line it is on. A percentage basis down a column whose height is
    /// not known is `content` (CSS Flexbox §7.2.3), and so are the content
    /// keywords there, which down a column are the height the item came to.
    fn base_size(&mut self, item: &Measured<'_>, row: bool, main_height: Option<f32>) -> f32 {
        let named = match &item.style.flex_basis {
            FlexBasis::Content | FlexBasis::Size(Size::Auto) => None,
            FlexBasis::Size(size) if row => self
                .preferred_width(item.id, item.style, size, item.room, item.frame.inline)
                .map(|basis| basis + item.frame.inline),
            FlexBasis::Size(Size::Length(length)) => {
                content_length(length, main_height, item.style.box_sizing, item.frame.block)
                    .map(|basis| basis + item.frame.block)
            }
            FlexBasis::Size(Size::Intrinsic(_) | Size::Stretch) => None,
        };
        match (named, &item.style.flex_basis) {
            (Some(basis), _) => basis,
            (None, FlexBasis::Content) if row => self.wanted_width(item),
            (None, _) if row => match item.widths.preferred {
                Some(_) => item.laid_out.width,
                None => self.wanted_width(item),
            },
            (None, _) => item.laid_out.height,
        }
    }

    /// How far an item may be shrunk along the main axis, as a border box.
    ///
    /// A minimum of its own is that. `auto` is the automatic minimum (CSS
    /// Flexbox §4.5), which is taken as the content here: the item may be shrunk,
    /// but not past the point where its own content spills out of it, nor past
    /// the base size it started from. `overflow` does not relax that yet.
    ///
    /// The content is its min-content size, held to its maximum where it has one
    /// (§4.5 again): a picture is as narrow as it is, and `img { max-width: 100% }`
    /// is what brings that down to the row rather than to nothing.
    ///
    /// Down a column that floor is the item's own height, and it is not a
    /// nicety: `flex-basis: 0` down a column whose container has no height of its
    /// own leaves every item with a base of nothing and no free space to grow
    /// into, so a stack of cards came out as a stack of nothing.
    fn main_floor(&mut self, item: &Measured<'_>, row: bool, basis: f32) -> f32 {
        if row {
            return match &item.style.min_width {
                Size::Auto => {
                    let maximum = item.widths.limits.outer(item.frame.inline).max;
                    self.min_content_size(item.id, item.room.measure)
                        .min(maximum)
                        .min(basis)
                }
                Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => {
                    item.widths.limits.min + item.frame.inline
                }
            };
        }
        match &item.style.min_height {
            Size::Auto => item.laid_out.height,
            Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => {
                let content = (item.laid_out.height - item.frame.block).max(0.0);
                self.block_sizes_of(item.style, item.room.measure, Some(content))
                    .limits
                    .min
                    + item.frame.block
            }
        }
    }

    /// One line of a flex container: the main axis shared out, the cross axis
    /// aligned, and every item placed.
    ///
    /// Returns how much room the line took across the container.
    fn layout_flex_line(
        &mut self,
        items: &mut [FlexItem],
        line: std::ops::Range<usize>,
        style: &Arc<ComputedStyle>,
        geometry: FlexLine,
        origin: (f32, f32),
        out: &mut Vec<Fragment>,
    ) -> PlacedLine {
        let (x, y) = origin;
        let FlexLine {
            row,
            gap,
            inner,
            cross_start,
            cross,
        } = geometry;
        let count = line.len();
        if count == 0 {
            return PlacedLine {
                cross: 0.0,
                main: 0.0,
            };
        }
        let gaps = gap * (count - 1) as f32;

        // The main axis: share out what is left over, or take back what is missing.
        let used: f32 = items[line.clone()]
            .iter()
            .map(|item| item.base + item.margin_main(row))
            .sum();
        let free = inner - used - gaps;

        // The floor applies whether or not there is free space to share: a
        // container with no main size of its own has none to share, and an item
        // that started at nothing would stay nothing.
        for item in &mut items[line.clone()] {
            item.main = item.base.max(item.floor);
        }
        if free.is_finite() && free != 0.0 {
            let factors: f32 = items[line.clone()]
                .iter()
                .map(|item| if free > 0.0 { item.grow } else { item.shrink })
                .sum();
            if factors > 0.0 {
                for item in &mut items[line.clone()] {
                    let factor = if free > 0.0 { item.grow } else { item.shrink };
                    item.main = (item.base + free * factor / factors).max(item.floor);
                }
            }
        }

        // How tall an item is depends on how wide it ended up, and until the
        // sharing above has run nobody knows how wide that is. The base sizes
        // were measured against the whole container, where a column of wrapping
        // text comes back one line tall — so a line that took its cross size from
        // those had nothing for `stretch` to stretch to, and a row of cards kept
        // its own ragged heights while each one grew past the line it was on.
        // Measured again at the size each item actually got.
        if row {
            let floats = std::mem::take(&mut self.floats);
            let ports = self.scroll_ports.len();
            for index in line.clone() {
                // An item that names a height is that tall whatever it holds and
                // whatever width it ended up at, and the first measurement
                // already said so. Asking again with no height to give it would
                // be asking what it is worth without the thing it declared.
                if self.asked_height(&items[index].style, inner).is_some() {
                    continue;
                }
                let (id, main) = (items[index].id, items[index].main);
                items[index].cross = self
                    .layout_item(id, 0.0, 0.0, main, ItemHeight::AtLeast(0.0))
                    .rect
                    .height;
            }
            self.scroll_ports.truncate(ports);
            self.floats = floats;
        }

        // The cross axis: the line is as big as its largest item unless the
        // container settled it, and `stretch` makes the rest of them match it.
        let line_cross = match cross {
            LineCross::Exactly(size) => size,
            LineCross::AtLeast(floor) => items[line.clone()]
                .iter()
                .map(|item| item.cross + item.margin_cross(row))
                .fold(floor, f32::max),
        };

        let content_main: f32 = items[line.clone()]
            .iter()
            .map(|item| item.main + item.margin_main(row))
            .sum::<f32>()
            + gaps;
        let leftover = if inner.is_finite() {
            (inner - content_main).max(0.0)
        } else {
            0.0
        };
        // Auto margins eat the free space before `justify-content` sees any.
        // That is what `margin-right: auto` on one item in a row is for — it is
        // how a brand is pushed left and a nav right — and a container that
        // handed the leftover to `justify-content` instead would leave both of
        // them bunched at the start, which is what it did.
        let autos: usize = items[line.clone()]
            .iter()
            .map(|item| item.auto_margins_main(row))
            .sum();
        let per_auto = if autos > 0 && leftover > 0.0 {
            leftover / autos as f32
        } else {
            0.0
        };
        let leftover = if autos > 0 { 0.0 } else { leftover };

        let count = count as f32;
        let (leading, between) = match style.justify_content {
            JustifyContent::Start => (0.0, 0.0),
            JustifyContent::End => (leftover, 0.0),
            JustifyContent::Center => (leftover / 2.0, 0.0),
            JustifyContent::SpaceBetween if count > 1.0 => (0.0, leftover / (count - 1.0)),
            JustifyContent::SpaceBetween => (0.0, 0.0),
            JustifyContent::SpaceAround => (leftover / count / 2.0, leftover / count),
            JustifyContent::SpaceEvenly => (leftover / (count + 1.0), leftover / (count + 1.0)),
        };

        let order: Vec<usize> = if style.flex_direction.is_reverse() {
            line.clone().rev().collect()
        } else {
            line.clone().collect()
        };

        let mut cursor = leading;
        for index in order {
            let item = &items[index];
            let align = item.style.align_self.unwrap_or(style.align_items);
            // `stretch` fills the line with an item that has no size of its own
            // across it. An item that named one keeps it: `align-items: stretch`
            // is the initial value, so stretching over a declared height would
            // make every `height` in a flex container a suggestion.
            let definite = if row {
                self.asked_height(&item.style, inner).is_some()
            } else {
                item.style.width != Size::Auto
            };
            // An `auto` margin across the line takes the room first, and takes it
            // from both `stretch` and `align-self`: `margin: 0 auto` on a column
            // of content is how a page is centred, and a container that handed
            // that space to the alignment instead left the whole page against the
            // left edge. Flexbox §8.1, and the reason the same declaration means
            // *centre me* in a block and in a flex item alike.
            let (cross_lead_auto, cross_trail_auto) = item.auto_margin_sides(!row);
            let cross_autos = usize::from(cross_lead_auto) + usize::from(cross_trail_auto);
            // Stretched, an item is the line less its margins, held between its
            // own minimum and maximum across (CSS Flexbox §9.4, step 11) — not
            // as big as its content, which a line of a set size may be smaller
            // than.
            let stretched = align == AlignItems::Stretch && !definite && cross_autos == 0;
            let cross_size = if stretched {
                item.cross_limits
                    .clamp((line_cross - item.margin_cross(row)).max(0.0))
            } else {
                item.cross
            };
            let free_cross = line_cross - cross_size - item.margin_cross(row);
            let cross_offset = cross_start
                + if cross_autos > 0 {
                    let share = free_cross.max(0.0) / cross_autos as f32;
                    if cross_lead_auto { share } else { 0.0 }
                } else {
                    match align {
                        AlignItems::End => free_cross,
                        AlignItems::Center => free_cross / 2.0,
                        // `baseline` needs a baseline to align on, which a box does
                        // not carry yet; it lays out as `start`, which is where it
                        // would be for a single line of text anyway.
                        _ => 0.0,
                    }
                };

            // An `auto` margin on the leading side pushes this item along; one on
            // the trailing side pushes everything after it.
            let (lead_auto, trail_auto) = item.auto_margin_sides(row);
            let lead = if lead_auto { per_auto } else { 0.0 };
            let trail = if trail_auto { per_auto } else { 0.0 };

            // Along a row the height is the line's business: exactly what
            // `stretch` made of it, or what the item holds. Down a column it is
            // what the sharing out left the item, whatever that holds — an item
            // with `min-height: 0` is let shrink past its content so that the
            // content can overflow it, and scroll, rather than push past the
            // item and over whatever the container put after it.
            let (item_x, item_y, item_width, item_height) = if row {
                (
                    x + cursor + item.margin.left + lead,
                    y + cross_offset + item.margin.top,
                    item.main,
                    if stretched {
                        ItemHeight::Exactly {
                            height: cross_size,
                            definite: true,
                        }
                    } else {
                        ItemHeight::AtLeast(cross_size)
                    },
                )
            } else {
                (
                    x + cross_offset + item.margin.left,
                    y + cursor + item.margin.top + lead,
                    cross_size,
                    ItemHeight::Exactly {
                        height: item.main,
                        definite: inner.is_finite(),
                    },
                )
            };

            let id = item.id;
            let advance = item.main + item.margin_main(row) + lead + trail;
            let fragment = self.layout_item(id, item_x, item_y, item_width, item_height);
            out.push(fragment);
            cursor += advance + gap + between;
        }

        PlacedLine {
            cross: line_cross,
            // What the line actually reached along the main axis: the last gap is
            // spent moving the cursor past an item that is not there.
            main: (cursor - gap - between).max(0.0),
        }
    }

    /// One flex item, laid out at the size the container decided for it.
    fn layout_item(
        &mut self,
        id: BoxId,
        x: f32,
        y: f32,
        width: f32,
        height: ItemHeight,
    ) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A picture is its own content: the container decided its size, and there is
        // nothing inside it to lay out.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            // The container decided the outer size; the frame comes out of it.
            // A height the line left open is what the width makes of the
            // picture through its ratio.
            let frame = Frame::of(&style, width);
            let inner_width = (width - frame.inline).max(0.0);
            let inner_height = match height {
                ItemHeight::Exactly { height, .. } => (height - frame.block).max(0.0),
                ItemHeight::AtLeast(floor) => {
                    replaced_height(&style, content, inner_width, width, self.containing_height)
                        .max(floor - frame.block)
                }
            };
            return replaced_fragment(
                id,
                &style,
                content.image.clone(),
                (x, y),
                (inner_width, inner_height),
                width,
            );
        }

        let padding = resolve_padding(&style, width);
        let border = resolve_border(&style);
        let frame = Frame::new(padding, border);

        let content_width = (width - frame.inline).max(0.0);
        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;

        // A height of its own is the height it gets, whatever it holds — an item
        // that overflows the size it asked for is what CSS says happens — and a
        // percentage inside it is of that height.
        let height = match height {
            ItemHeight::AtLeast(floor) => match self.asked_height(&style, width) {
                Some(_) => ItemHeight::Exactly {
                    height: floor,
                    definite: true,
                },
                None => ItemHeight::AtLeast(floor),
            },
            exactly @ ItemHeight::Exactly { .. } => exactly,
        };
        let definite = match height {
            ItemHeight::Exactly {
                height,
                definite: true,
            } => Some((height - frame.block).max(0.0)),
            ItemHeight::Exactly {
                definite: false, ..
            }
            | ItemHeight::AtLeast(_) => None,
        };
        let mut children = Vec::new();
        let content_height = self.layout_inside(
            id,
            content_width,
            content_x,
            content_y,
            definite,
            &mut children,
        );
        let outer_height = match height {
            ItemHeight::Exactly { height, .. } => height,
            ItemHeight::AtLeast(floor) => {
                let content_height = self.content_height(id, content_height);
                let held = self
                    .block_sizes_of(&style, width, Some(content_height))
                    .used(content_height);
                floor.max(held + frame.block)
            }
        };

        // What does not fit is cut off at the padding edge, and scrolls, when
        // `overflow` says so — which down a column that has shared out a
        // height of its own is most of what `min-height: 0` is written for.
        if style.overflow == otlyra_css::Overflow::Clip {
            let padding_box = Rect::new(
                x + border.left,
                y + border.top,
                (width - border.left - border.right).max(0.0),
                (outer_height - border.top - border.bottom).max(0.0),
            );
            self.clip_overflow(id, padding_box, padding, &mut children);
        }

        Fragment {
            used: Some(crate::UsedEdges {
                margin: resolve_margin(&style, width),
                border,
                padding,
            }),
            ..Fragment::for_box(id, Rect::new(x, y, width, outer_height), style, children)
        }
    }
}
