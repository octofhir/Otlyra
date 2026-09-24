//! Flex layout: items along one axis, sharing out the room on it.
//!
//! The algorithm is CSS Flexbox §9, one phase after another, each a function
//! of its own:
//!
//! 1. the items are collected with their base sizes and the limits on their
//!    main sizes (§9.2, step 3), and put in `order`;
//! 2. they are broken into lines on their hypothetical main sizes (§9.3,
//!    step 5), and each line's flexible lengths are resolved (§9.7);
//! 3. every item is sized across at the main size it got (§9.4, step 7), which
//!    sizes the lines, and the container shares what it has across between them
//!    (§9.4, steps 8 and 9; §9.6, step 16);
//! 4. and only then is each item placed — auto margins, `justify-content`,
//!    `align-self`, `stretch` (§9.4 step 11, §9.5, §9.6) — and laid out, once.
//!
//! The arithmetic of 2 and 3 lives in `lines`, with no box in sight; what
//! the container asks of one item, in `item`.
//!
//! ## What an item costs
//!
//! Nothing before the last phase lays an item out: an item's size along a row
//! comes from its properties and its intrinsic widths, and how tall it is at
//! some width from [`Flow::content_block_size`], which keeps each answer by the
//! width and the height it was asked at. Only the height is kept, never the
//! fragments: where a box inside the item lands can depend on where the item is
//! — an absolutely positioned descendant placed against a box outside it does —
//! and the height is the one thing that does not.
//!
//! So a box laid out for real is laid out once by each flex container above it,
//! when that container is laid out for real, plus the once it was first
//! measured: a box `k` flex containers deep, `k + 1` times. Measuring each item,
//! measuring it again at its main size and then laying it out, as this did,
//! was three times per level and `3^k` in all, and a page whose header nests
//! flex containers fifteen deep never came back.

mod item;
mod lines;

#[cfg(test)]
mod tests;

use std::ops::Range;
use std::sync::Arc;

use otlyra_css::{AlignItems, ComputedStyle, FlexWrap};

use crate::box_tree::BoxId;
use crate::fragment::Fragment;

use super::Flow;
use super::sizing::BlockSpace;
use item::{FlexItem, ItemHeight, stretched};
use lines::{Flexible, break_lines, end_to_end, justify, resolve_flexible_lengths, size_lines};

/// What a flex container's items are sized and placed against.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Container {
    /// Whether the main axis is horizontal.
    row: bool,
    /// Its inner width, which a percentage in an item's margins, padding and
    /// widths is of whichever way the container runs (CSS Flexbox §4.2).
    width: f32,
    /// Its room down the block axis, which is across a row and along a column.
    /// The height, when whoever laid the container out settled it before its
    /// contents — its own `height`, or the one a flex line gave it — is what a
    /// percentage height inside it is of; the limits are its `min-height` and
    /// `max-height`, which hold it where it has no height of its own.
    space: BlockSpace,
    /// Whether its items are laid on one line (`flex-wrap: nowrap`), which is
    /// what makes a stretched item's cross size definite before the line is
    /// sized, where the container's own is (CSS Flexbox §9.8).
    single_line: bool,
    /// Between two items on a line.
    gap: f32,
    /// `align-items`, which is what an item's `align-self: auto` is.
    align_items: AlignItems,
}

impl Container {
    /// Its inner main size, which the items are fitted into (CSS Flexbox §9.3,
    /// step 4) and every one of its lines is as long as (§6): a row's width, and
    /// a column's height.
    ///
    /// A column with no height of its own is as tall as its content, which is
    /// what `content` works out — its items' outer hypothetical main sizes laid
    /// end to end — held between its `min-height` and `max-height`, as any
    /// block-level box with an `auto` height is (CSS 2.2 §10.7). That is what
    /// makes `flex: 1` fill a `min-height: 100vh` page, and a `max-height`
    /// shrink the items. A column that neither limit holds is exactly as long
    /// as its items, so there is nothing to share out: that is the infinite
    /// room, in which every item is its hypothetical size to the bit, rather
    /// than that size give or take a rounding of the sum.
    fn main_room(self, content: impl FnOnce() -> f32) -> f32 {
        if self.row {
            return self.width;
        }
        match self.space.height {
            Some(height) => height,
            None => {
                let content = content();
                let held = self.space.limits.clamp(content);
                if held == content { f32::INFINITY } else { held }
            }
        }
    }
}

/// One flex line, once the container has sized it across.
struct FlexLine {
    /// The items on it.
    items: Range<usize>,
    /// How long it is: the container's inner main size, which every line is
    /// (CSS Flexbox §6), and infinite for a column as long as its items (see
    /// [`Container::main_room`]).
    length: f32,
    /// Where it starts across the container.
    cross_start: f32,
    /// How big it is across.
    cross: f32,
}

/// Resolve one line's flexible lengths (CSS Flexbox §9.3, step 6): each item's
/// main size, out of the line's `length` once the gaps between its items are
/// taken.
fn resolve_main_sizes(line: &mut [FlexItem], flexible: &[Flexible], length: f32, gap: f32) {
    let gaps = gap * line.len().saturating_sub(1) as f32;
    let mains = resolve_flexible_lengths(flexible, length - gaps);
    for (item, main) in line.iter_mut().zip(mains) {
        item.main = main;
    }
}

impl<'a> Flow<'a> {
    /// A flex formatting context (CSS Flexbox §9): the children are items along
    /// one axis.
    ///
    /// Along the main axis each item starts at its base size, and what the line
    /// has left over — or is missing — is shared out by `flex-grow` and
    /// `flex-shrink`; across it each item is placed by `align-items`, stretching
    /// to the line by default, which is what makes columns of equal height
    /// without anyone saying how tall. Returns the height the container's
    /// content comes to.
    pub(super) fn layout_flex(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(parent).style);
        let row = style.flex_direction.is_row();
        let (row_gap, column_gap) = (style.gap.0.resolve(width), style.gap.1.resolve(width));
        let (gap, cross_gap) = if row {
            (column_gap, row_gap)
        } else {
            (row_gap, column_gap)
        };
        let container = Container {
            row,
            width,
            space: BlockSpace {
                height: self.containing_height,
                limits: self.container_limits,
            },
            single_line: style.flex_wrap == FlexWrap::NoWrap,
            gap,
            align_items: style.align_items,
        };

        let mut items = self.collect_items(parent, container, y, out);
        if items.is_empty() {
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

        let flexible: Vec<Flexible> = items
            .iter()
            .map(|item| item.flexible(container.row))
            .collect();
        let outer: Vec<f32> = flexible.iter().map(Flexible::outer_hypothetical).collect();
        let length = container.main_room(|| end_to_end(&outer, gap));
        let lines = if container.single_line {
            std::iter::once(0..items.len()).collect()
        } else {
            break_lines(&outer, gap, length)
        };

        for line in &lines {
            resolve_main_sizes(
                &mut items[line.clone()],
                &flexible[line.clone()],
                length,
                gap,
            );
        }

        // How big each item is across depends on how big it ended up along, and
        // until its line was shared out nobody knew that. Down a column it was
        // known from the start.
        if container.row {
            for item in &mut items {
                self.size_row_item_across(item, width);
            }
        }

        // What the lines want across, and the cross size the container has to
        // give out, once it knows what they come to: a column's is always its
        // width, and a row's is its own height where it has one and otherwise
        // its lines held between its `min-height` and `max-height` (§9.4, step
        // 15) — which is how `align-items: center` centres anything in a
        // `min-height` hero.
        let wanted: Vec<f32> = lines
            .iter()
            .map(|line| {
                items[line.clone()]
                    .iter()
                    .map(|item| item.cross + item.margin_cross(container.row))
                    .fold(0.0f32, f32::max)
            })
            .collect();
        let across = size_lines(
            &wanted,
            |lines| {
                if container.row {
                    container.space.used(lines)
                } else {
                    width
                }
            },
            container.single_line,
            cross_gap,
            style.align_content,
        );

        let mut cross_start = across.spacing.leading;
        let mut main_extent = 0.0f32;
        for (items_on_line, &cross) in lines.into_iter().zip(&across.sizes) {
            let line = FlexLine {
                items: items_on_line,
                length,
                cross_start,
                cross,
            };
            let reach = self.place_line(&items, &line, &style, container, (x, y), out);
            main_extent = main_extent.max(reach);
            cross_start += cross + cross_gap + across.spacing.between;
        }

        // The height the container's content comes to: its cross size when it
        // is a row (§9.4, step 15), and when it is a column how far along its
        // longest line reaches, which whoever laid the container out holds
        // between its limits as it does any box's content.
        if container.row {
            across.container
        } else {
            main_extent
        }
    }

    /// Place one line's items along it and across it, and lay each of them out
    /// (CSS Flexbox §9.4 step 11, §9.5, §9.6 steps 13 and 14).
    ///
    /// Returns how far along the main axis the line reached.
    fn place_line(
        &mut self,
        items: &[FlexItem],
        line: &FlexLine,
        style: &ComputedStyle,
        container: Container,
        origin: (f32, f32),
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let (x, y) = origin;
        let (row, gap, inner) = (container.row, container.gap, line.length);
        let on_line = &items[line.items.clone()];
        let count = on_line.len();
        let gaps = gap * count.saturating_sub(1) as f32;

        let content_main: f32 = on_line
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
        let autos: usize = on_line.iter().map(|item| item.auto_margins_main(row)).sum();
        let per_auto = if autos > 0 && leftover > 0.0 {
            leftover / autos as f32
        } else {
            0.0
        };
        let leftover = if autos > 0 { 0.0 } else { leftover };
        let spacing = justify(style.justify_content, leftover, count);

        let order: Vec<&FlexItem> = if style.flex_direction.is_reverse() {
            on_line.iter().rev().collect()
        } else {
            on_line.iter().collect()
        };

        let mut cursor = spacing.leading;
        for item in order {
            // An `auto` margin across the line takes the room first, and takes it
            // from both `stretch` and `align-self` (§8.1).
            let (cross_lead_auto, cross_trail_auto) = item.auto_margin_sides(!row);
            let cross_autos = usize::from(cross_lead_auto) + usize::from(cross_trail_auto);
            // Stretched, an item is the line less its margins, held between its
            // own minimum and maximum across — not as big as its content, which
            // a line of a set size may be smaller than.
            let cross_size = if item.fills_line {
                stretched(item.cross_limits, line.cross, item.margin_cross(row))
            } else {
                item.cross
            };
            let free_cross = line.cross - cross_size - item.margin_cross(row);
            let cross_offset = line.cross_start
                + if cross_autos > 0 {
                    let share = free_cross.max(0.0) / cross_autos as f32;
                    if cross_lead_auto { share } else { 0.0 }
                } else {
                    match item.align {
                        AlignItems::End => free_cross,
                        AlignItems::Center => free_cross / 2.0,
                        // `baseline` needs a baseline to align on, which a box
                        // does not carry yet; it lays out as `start`, which is
                        // where it would be for a single line of text anyway.
                        AlignItems::Start | AlignItems::Stretch | AlignItems::Baseline => 0.0,
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
                    if item.fills_line {
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
                    // Definite only where the container's own height is: a
                    // column held up by its `min-height` shares that out, but
                    // a percentage inside an item is still of nothing (§9.8).
                    ItemHeight::Exactly {
                        height: item.main,
                        definite: container.space.height.is_some(),
                    },
                )
            };

            let fragment = self.layout_item(
                item,
                (item_x, item_y),
                item_width,
                item_height,
                container.width,
            );
            out.push(fragment);
            let advance = item.main + item.margin_main(row) + lead + trail;
            cursor += advance + gap + spacing.between;
        }

        // What the line actually reached along the main axis: the last gap is
        // spent moving the cursor past an item that is not there.
        (cursor - gap - spacing.between).max(0.0)
    }
}
