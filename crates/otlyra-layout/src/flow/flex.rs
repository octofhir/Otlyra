//! Flex layout: items along one axis, sharing out the room on it.
//!
//! Every item is measured at its base size, the items are broken into lines, and
//! what each line has left over or is missing is shared out by `flex-grow` and
//! `flex-shrink`; the line then places its items across. The item and the line
//! are this context's own types, and nothing else reads them.

use std::sync::Arc;

use otlyra_css::{
    AlignItems, ComputedStyle, FlexWrap, JustifyContent, Length, LengthOrAuto, Sides,
};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{Fragment, FragmentKind, Layer, Rect};

use super::Flow;
use super::box_model::{clamp, resolve_border, resolve_margin, resolve_padding};
use super::replaced::{replaced_edges, replaced_fragment};

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
    /// A cross size the line is at least as big as, which is how a lone line fills
    /// a container that has a height of its own.
    cross_floor: Option<f32>,
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
            let fragment = self.layout_block(child, width, x, y);
            let margin = resolve_margin(&item_style, width);

            // The base size: `flex-basis` if it says, then the item's own size, and
            // for an auto width along a row the size its content wants — a flex
            // item is not a block, and does not fill the line it is on.
            let basis = match item_style.flex_basis.and_then(|basis| basis.resolve(width)) {
                Some(basis) => basis,
                None if row => match item_style.width.resolve(width) {
                    Some(_) => fragment.rect.width,
                    None => {
                        let content = self.max_content_width(child, width);
                        clamp(
                            content.min(width),
                            item_style.min_width,
                            item_style.max_width,
                            width,
                        )
                    }
                },
                None => fragment.rect.height,
            };

            // A flex item's automatic minimum size: it may be shrunk, but not past
            // the point where its own content spills out of it. An item that says
            // `min-width` or `overflow` of its own would override this; neither is
            // read yet, so the content is the floor.
            //
            // Down a column that floor is the item's own height, and it is not a
            // nicety: `flex-basis: 0` down a column whose container has no height
            // of its own leaves every item with a base of nothing and no free
            // space to grow into, so a stack of cards came out as a stack of
            // nothing. `min-height: auto` is what CSS calls the rule that stops
            // it.
            let floor = if row {
                if item_style.min_width == Length::ZERO {
                    self.min_content_width(child, width, true).min(basis)
                } else {
                    item_style.min_width.resolve(width)
                }
            } else if item_style.min_height == Length::ZERO {
                fragment.rect.height
            } else {
                item_style.min_height.resolve(width)
            };

            // Across the line, an item with an `auto` margin on that side is an
            // item the free space belongs to — so it cannot also be the item that
            // filled the line. Laid out on its own it took the whole width, the
            // way a block does; what it is owed is the width its content wants,
            // and the margin gets the rest.
            let cross_auto_margin = !row
                && (item_style.margin.left == LengthOrAuto::Auto
                    || item_style.margin.right == LengthOrAuto::Auto)
                && item_style.width.resolve(width).is_none();
            let cross = if row {
                fragment.rect.height
            } else if cross_auto_margin {
                clamp(
                    self.max_content_width(child, width).min(width),
                    item_style.min_width,
                    item_style.max_width,
                    width,
                )
            } else {
                fragment.rect.width
            };

            items.push(FlexItem {
                id: child,
                floor,
                base: basis,
                main: basis,
                cross,
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

        // A container with a height of its own has a definite cross size when it
        // is a row and a definite main size when it is a column: either way it is
        // the size the items are fitted into rather than one they add up to.
        let definite_height = self
            .asked_height(&style)
            .map(|height| clamp(height, style.min_height, style.max_height, width));
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
                    cross_floor: (container_cross.is_some()).then(|| wanted[number] + stretch),
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
            cross_floor,
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
                if self.asked_height(&items[index].style).is_some() {
                    continue;
                }
                let (id, main) = (items[index].id, items[index].main);
                items[index].cross = self.layout_item(id, 0.0, 0.0, main, 0.0).rect.height;
            }
            self.scroll_ports.truncate(ports);
            self.floats = floats;
        }

        // The cross axis: the line is as big as its largest item, and `stretch`
        // makes the rest of them match it.
        let line_cross = items[line.clone()]
            .iter()
            .map(|item| item.cross + item.margin_cross(row))
            .fold(cross_floor.unwrap_or(0.0), f32::max);

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
                self.asked_height(&item.style).is_some()
            } else {
                item.style.width.resolve(inner).is_some()
            };
            // An `auto` margin across the line takes the room first, and takes it
            // from both `stretch` and `align-self`: `margin: 0 auto` on a column
            // of content is how a page is centred, and a container that handed
            // that space to the alignment instead left the whole page against the
            // left edge. Flexbox §8.1, and the reason the same declaration means
            // *centre me* in a block and in a flex item alike.
            let (cross_lead_auto, cross_trail_auto) = item.auto_margin_sides(!row);
            let cross_autos = usize::from(cross_lead_auto) + usize::from(cross_trail_auto);
            let cross_size = match align {
                AlignItems::Stretch if !definite && cross_autos == 0 => {
                    (line_cross - item.margin_cross(row)).max(item.cross)
                }
                _ => item.cross,
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

            let (item_x, item_y, item_width, item_height) = if row {
                (
                    x + cursor + item.margin.left + lead,
                    y + cross_offset + item.margin.top,
                    item.main,
                    cross_size,
                )
            } else {
                (
                    x + cross_offset + item.margin.left,
                    y + cursor + item.margin.top + lead,
                    cross_size,
                    item.main,
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
    fn layout_item(&mut self, id: BoxId, x: f32, y: f32, width: f32, height: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A picture is its own content: the container decided its size, and there is
        // nothing inside it to lay out.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            // The container decided the outer size; the frame comes out of it.
            let (extra_x, extra_y) = replaced_edges(&style, width);
            return replaced_fragment(
                id,
                &style,
                content.image.clone(),
                (x, y),
                ((width - extra_x).max(0.0), (height - extra_y).max(0.0)),
                width,
            );
        }

        let padding = resolve_padding(&style, width);
        let border = resolve_border(&style);

        let content_width =
            (width - padding.left - padding.right - border.left - border.right).max(0.0);
        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;

        let mut children = Vec::new();
        let content_height =
            self.layout_inside(id, content_width, content_x, content_y, &mut children);

        // A height of its own is the height it gets, whatever it holds — an item
        // that overflows the size it asked for is what CSS says happens. Without
        // one, the container's figure is a floor rather than the answer: it is
        // where `stretch` put it, and content taller than that still fits.
        let outer_height = match self.asked_height(&style) {
            Some(_) => height,
            None => height
                .max(content_height + padding.top + padding.bottom + border.top + border.bottom),
        };

        Fragment {
            used: Some(crate::UsedEdges {
                margin: resolve_margin(&style, width),
                border,
                padding,
            }),
            box_id: Some(id),
            rect: Rect::new(x, y, width, outer_height),
            kind: FragmentKind::Box,
            style,
            widget: None,
            fixed: false,
            scroll_port: None,
            clip: None,
            sticky: None,
            layer: Layer::default(),
            children,
        }
    }
}
