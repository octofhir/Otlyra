//! Grid layout (CSS Grid 2): items placed into rows and columns.
//!
//! In the specification's order, each step a module of its own:
//!
//! 1. the explicit grid (§7), with its automatic repetitions counted, in
//!    `explicit`;
//! 2. each item's lines resolved and the rest auto-placed (§8), in
//!    `placement`;
//! 3. the columns sized from what the items contribute (§12), in `tracks`;
//! 4. each item's width decided in its area (§11.3), which is what its
//!    height is measured at, and the rows sized from those heights;
//! 5. the tracks aligned in the grid (§11.5), and each item aligned in its
//!    area and laid out, once (§11.4).
//!
//! Baselines are not modelled: `baseline` alignment lays out as `start`, and a
//! grid has no baseline of its own to give a line it sits on. Directions and
//! writing modes are not either: columns run left to right, rows downwards.

mod explicit;
mod placement;
mod tracks;

#[cfg(test)]
mod tests;

use std::ops::Range;
use std::sync::Arc;

use otlyra_css::{
    AlignContent, AlignItems, ComputedStyle, GridTemplate, JustifyContent, LengthOrAuto, MaxSize,
    Size, TrackSize,
};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::Fragment;

use super::Flow;
use super::box_model::{resolve_border, resolve_margin, resolve_padding};
use super::intrinsic::Wanted;
use super::item::{ItemBox, ItemHeight};
use super::sizing::{Frame, InlineRoom, Limits, content_box};
use explicit::{ExplicitAxis, Room, implicit_track};
use placement::{Area, AxisPlacement, Placed, place, resolve_axis};
use tracks::{Contribution, Space, Track, size_tracks};

/// A grid item and where it went.
struct GridItem {
    id: BoxId,
    style: Arc<ComputedStyle>,
    area: Area,
}

/// A grid with its items placed and its tracks' sizing functions resolved,
/// before anything is sized: what laying the grid out and measuring it share.
struct Setup {
    items: Vec<GridItem>,
    columns: Vec<Track>,
    rows: Vec<Track>,
    /// The `auto-fit` repetitions nothing was placed in (§7.2.3.2).
    collapsed_columns: Vec<bool>,
    collapsed_rows: Vec<bool>,
}

/// One axis's tracks as sized, and where each starts and ends.
struct Lines {
    /// Where each track starts and ends, from the grid's content edge.
    tracks: Vec<(f32, f32)>,
}

impl Lines {
    /// Place `sizes` one after another with `gap` between each two that are
    /// there, `leading` before the first and `between` added to each gap.
    fn new(sizes: &[f32], collapsed: &[bool], gap: f32, leading: f32, between: f32) -> Self {
        let mut tracks = Vec::with_capacity(sizes.len());
        let mut at = leading;
        let mut seen = false;
        for (&size, &gone) in sizes.iter().zip(collapsed) {
            if !gone {
                if seen {
                    at += gap + between;
                }
                seen = true;
            }
            tracks.push((at, at + size));
            at += size;
        }
        Self { tracks }
    }

    /// Where a span of tracks starts, and how big it is, the gaps inside it
    /// included.
    fn span(&self, span: &Range<usize>) -> (f32, f32) {
        let start = self.tracks.get(span.start).map_or(0.0, |track| track.0);
        let end = span
            .end
            .checked_sub(1)
            .and_then(|last| self.tracks.get(last))
            .map_or(start, |track| track.1);
        (start, (end - start).max(0.0))
    }

    /// Where the last track ends.
    fn end(&self) -> f32 {
        self.tracks.last().map_or(0.0, |track| track.1)
    }
}

/// The tracks an item's own placement is resolved against: one axis of the
/// explicit grid, and its size.
fn axis_placement(axis: &ExplicitAxis, placement: &otlyra_css::GridPlacement) -> AxisPlacement {
    resolve_axis(placement, &axis.names, axis.tracks.len() as i32)
}

/// The resolved sizing functions of every track along one axis: explicit ones
/// where the explicit grid has them, `grid-auto-*` ones before and after it.
fn axis_tracks(
    axis: &ExplicitAxis,
    count: usize,
    origin: usize,
    auto: &[TrackSize],
    basis: Option<f32>,
) -> Vec<Track> {
    (0..count)
        .map(|index| {
            let explicit = index as isize - origin as isize;
            let size = match usize::try_from(explicit)
                .ok()
                .and_then(|at| axis.tracks.get(at))
            {
                Some(size) => size.clone(),
                None if explicit < 0 => implicit_track(auto, explicit),
                None => implicit_track(auto, explicit - axis.tracks.len() as isize),
            };
            Track::resolve(&size, basis)
        })
        .collect()
}

/// The empty tracks of an `auto-fit` repetition, which collapse (§7.2.3.2).
fn collapsed(
    axis: &ExplicitAxis,
    count: usize,
    origin: usize,
    spans: &[Range<usize>],
) -> Vec<bool> {
    let mut gone = vec![false; count];
    if let Some(repeated) = axis.repeated.clone().filter(|_| axis.fits) {
        for track in repeated {
            let index = track + origin;
            if index < count && !spans.iter().any(|span| span.contains(&index)) {
                gone[index] = true;
            }
        }
    }
    gone
}

/// Size the tracks that are there, leaving the collapsed ones at nothing and
/// out of the gaps.
fn size_visible(
    tracks: &[Track],
    collapsed: &[bool],
    items: &[Contribution],
    space: Space,
    container: Limits,
    gap: f32,
    stretch_auto: bool,
) -> Vec<f32> {
    let kept: Vec<usize> = (0..tracks.len())
        .filter(|&index| !collapsed[index])
        .collect();
    let visible: Vec<Track> = kept.iter().map(|&index| tracks[index]).collect();
    // A collapsed track holds no item, so no span crosses one, and each span
    // is still a run of the tracks that are left.
    let position = |index: usize| kept.partition_point(|&kept| kept < index);
    let items: Vec<Contribution> = items
        .iter()
        .map(|item| Contribution {
            tracks: position(item.tracks.start)..position(item.tracks.end),
            ..item.clone()
        })
        .collect();
    let sized = size_tracks(&visible, &items, space, container, gap, stretch_auto);
    let mut sizes = vec![0.0; tracks.len()];
    for (index, size) in kept.into_iter().zip(sized) {
        sizes[index] = size;
    }
    sizes
}

/// What `justify-content` or `align-content` does with room the tracks leave
/// (§11.5): before them, and between each two. `normal` and `stretch` gave it
/// to the `auto` tracks already (§12.8), and are `start` for what is left; so
/// is room that is not there, since overflow alignment is not modelled.
fn distribute(content: AlignContent, free: f32, tracks: usize) -> (f32, f32) {
    if free <= 0.0 || tracks == 0 {
        return (0.0, 0.0);
    }
    let gaps = tracks.saturating_sub(1) as f32;
    match content {
        AlignContent::Stretch | AlignContent::Start => (0.0, 0.0),
        AlignContent::End => (free, 0.0),
        AlignContent::Center => (free / 2.0, 0.0),
        AlignContent::SpaceBetween if tracks > 1 => (0.0, free / gaps),
        AlignContent::SpaceBetween => (0.0, 0.0),
        AlignContent::SpaceAround => {
            let each = free / tracks as f32;
            (each / 2.0, each)
        }
        AlignContent::SpaceEvenly => {
            let each = free / (tracks as f32 + 1.0);
            (each, each)
        }
    }
}

/// Where an item sits in `free` room along one axis, by its alignment; an
/// `auto` margin on either side takes the room first (§11.2).
fn align_offset(align: AlignItems, free: f32, autos: (bool, bool)) -> f32 {
    let free = free.max(0.0);
    match autos {
        (true, true) => free / 2.0,
        (true, false) => free,
        (false, true) => 0.0,
        (false, false) => match align {
            AlignItems::End => free,
            AlignItems::Center => free / 2.0,
            AlignItems::Start | AlignItems::Stretch | AlignItems::Normal | AlignItems::Baseline => {
                0.0
            }
        },
    }
}

/// An item's alignment with `normal` settled: `stretch`, except for a box with
/// a natural aspect ratio, which starts (CSS Align 3 §6.1, §6.2).
fn used_alignment(align: AlignItems, ratio: bool) -> AlignItems {
    match align {
        AlignItems::Normal if ratio => AlignItems::Start,
        AlignItems::Normal => AlignItems::Stretch,
        other => other,
    }
}

impl<'a> Flow<'a> {
    /// A grid formatting context: the children are placed into rows and
    /// columns, and each is laid out in its area. Returns the height the
    /// rows come to.
    pub(super) fn layout_grid(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(parent).style);
        let template = style.grid.clone().unwrap_or_default();
        let height = self.containing_height;
        let limits = self.container_limits;
        let column_gap = style.gap.1.resolve(width);
        // A percentage gap down the block axis is of a height the grid has,
        // and nothing where it has none (CSS Align 3 §8.3).
        let row_gap = style.gap.0.definite(height).unwrap_or(0.0);

        // An absolutely positioned child is not a grid item (§10.2): it goes
        // where it would have gone with nothing placed, at the content edge.
        let mut children = Vec::new();
        for child in self.tree.node(parent).children.clone() {
            if self.tree.node(child).style.position.is_out_of_flow() {
                let fragment = self.layout_positioned(child, y);
                out.push(fragment);
            } else {
                children.push(child);
            }
        }
        if children.is_empty() {
            return 0.0;
        }

        let setup = self.grid_setup(
            &template,
            children,
            Room {
                size: Some(width),
                limits: Limits::NONE,
                gap: column_gap,
            },
            Room {
                size: height,
                limits,
                gap: row_gap,
            },
        );

        // The columns, from the items' widths.
        let contributions = self.column_contributions(&setup, width, &setup.columns);
        let column_sizes = size_visible(
            &setup.columns,
            &setup.collapsed_columns,
            &contributions,
            Space::Definite(width),
            Limits::NONE,
            column_gap,
            style.justify_content == JustifyContent::Stretch,
        );
        let columns_total = sum_with_gaps(&column_sizes, &setup.collapsed_columns, column_gap);
        let (lead, between) = distribute(
            style.justify_content.into(),
            width - columns_total,
            visible(&setup.collapsed_columns),
        );
        let columns = Lines::new(
            &column_sizes,
            &setup.collapsed_columns,
            column_gap,
            lead,
            between,
        );

        // Each item's width in its area, which is what its height is measured
        // at; with no height yet for a percentage inside it to be of.
        let widths: Vec<(f32, f32)> = setup
            .items
            .iter()
            .map(|item| {
                let (_, area_width) = columns.span(&item.area.columns);
                self.item_width(item, &style, area_width)
            })
            .collect();
        let measuring = self.containing_height.take();
        let row_contributions: Vec<Contribution> = setup
            .items
            .iter()
            .zip(&widths)
            .map(|(item, &(item_width, _))| {
                let (_, area_width) = columns.span(&item.area.columns);
                self.row_contribution(item, item_width, area_width, &setup.rows)
            })
            .collect();
        self.containing_height = measuring;

        let row_space = height.map_or(Space::MaxContent, Space::Definite);
        let row_sizes = size_visible(
            &setup.rows,
            &setup.collapsed_rows,
            &row_contributions,
            row_space,
            limits,
            row_gap,
            style.align_content == AlignContent::Stretch,
        );
        let rows_total = sum_with_gaps(&row_sizes, &setup.collapsed_rows, row_gap);
        let room_down = height.unwrap_or_else(|| limits.clamp(rows_total));
        let (lead, between) = distribute(
            style.align_content,
            room_down - rows_total,
            visible(&setup.collapsed_rows),
        );
        let rows = Lines::new(&row_sizes, &setup.collapsed_rows, row_gap, lead, between);

        // Each item aligned in its area and laid out, once.
        let outer_floats = std::mem::take(&mut self.floats);
        for (item, &(item_width, offset)) in setup.items.iter().zip(&widths) {
            let (column_x, area_width) = columns.span(&item.area.columns);
            let (row_y, area_height) = rows.span(&item.area.rows);
            let fragment = self.place_item(
                item,
                &style,
                (x + column_x + offset, y + row_y),
                item_width,
                (area_width, area_height),
            );
            out.push(fragment);
        }
        self.floats = outer_floats;

        rows.end().max(rows_total)
    }

    /// A grid container's intrinsic width (§12.1): its columns sized under
    /// the constraint `wanted` names, and the gaps between them. A percentage
    /// track is `auto` here and a percentage gap nothing (§7.2.1).
    pub(super) fn grid_content_width(
        &mut self,
        id: BoxId,
        containing_width: f32,
        wanted: Wanted,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(id).style);
        let template = style.grid.clone().unwrap_or_default();
        let column_gap = style.gap.1.definite(None).unwrap_or(0.0);
        let children: Vec<BoxId> = self
            .tree
            .node(id)
            .children
            .iter()
            .copied()
            .filter(|&child| !self.tree.node(child).style.position.is_out_of_flow())
            .collect();
        if children.is_empty() {
            return 0.0;
        }
        let frame = Frame::of(&style, containing_width).inline;
        let own = fixed_inline_limits(&style, frame);
        let setup = self.grid_setup(
            &template,
            children,
            Room {
                size: None,
                limits: own,
                gap: column_gap,
            },
            Room {
                size: None,
                limits: Limits::NONE,
                gap: 0.0,
            },
        );
        let contributions = self.column_contributions(&setup, containing_width, &setup.columns);
        let space = match wanted {
            Wanted::Narrowest => Space::MinContent,
            Wanted::Widest => Space::MaxContent,
        };
        let sizes = size_visible(
            &setup.columns,
            &setup.collapsed_columns,
            &contributions,
            space,
            Limits::NONE,
            column_gap,
            false,
        );
        sum_with_gaps(&sizes, &setup.collapsed_columns, column_gap)
    }

    /// The explicit grid, the items placed in it and every track's sizing
    /// functions.
    fn grid_setup(
        &mut self,
        template: &GridTemplate,
        children: Vec<BoxId>,
        column_room: Room,
        row_room: Room,
    ) -> Setup {
        // `order` changes where an item is placed and painted, and nothing
        // else (§6.3). A stable sort, so equal orders keep document order.
        let mut children: Vec<(BoxId, Arc<ComputedStyle>)> = children
            .into_iter()
            .map(|child| (child, Arc::clone(&self.tree.node(child).style)))
            .collect();
        children.sort_by_key(|(_, style)| style.order);

        let column_areas: Vec<_> = template
            .areas
            .iter()
            .map(|area| (area, area.columns.clone()))
            .collect();
        let row_areas: Vec<_> = template
            .areas
            .iter()
            .map(|area| (area, area.rows.clone()))
            .collect();
        let explicit_columns = ExplicitAxis::new(
            &template.columns,
            &column_areas,
            template.area_columns,
            &template.auto_columns,
            column_room,
        );
        let explicit_rows = ExplicitAxis::new(
            &template.rows,
            &row_areas,
            template.area_rows,
            &template.auto_rows,
            row_room,
        );

        let placements: Vec<(AxisPlacement, AxisPlacement)> = children
            .iter()
            .map(|(_, style)| {
                (
                    axis_placement(&explicit_rows, &style.grid_row),
                    axis_placement(&explicit_columns, &style.grid_column),
                )
            })
            .collect();
        let Placed {
            areas,
            rows,
            columns,
            row_origin,
            column_origin,
        } = place(
            &placements,
            (explicit_rows.tracks.len(), explicit_columns.tracks.len()),
            template.flow,
        );

        let column_spans: Vec<Range<usize>> =
            areas.iter().map(|area| area.columns.clone()).collect();
        let row_spans: Vec<Range<usize>> = areas.iter().map(|area| area.rows.clone()).collect();
        Setup {
            columns: axis_tracks(
                &explicit_columns,
                columns,
                column_origin,
                &template.auto_columns,
                column_room.size,
            ),
            rows: axis_tracks(
                &explicit_rows,
                rows,
                row_origin,
                &template.auto_rows,
                row_room.size,
            ),
            collapsed_columns: collapsed(&explicit_columns, columns, column_origin, &column_spans),
            collapsed_rows: collapsed(&explicit_rows, rows, row_origin, &row_spans),
            items: children
                .into_iter()
                .zip(areas)
                .map(|((id, style), area)| GridItem { id, style, area })
                .collect(),
        }
    }

    /// What each item asks of the columns it spans (§12.5): its min-content
    /// and max-content widths and its minimum contribution, margins in.
    /// Percentages of the area it is measured for are cyclic (CSS Sizing 3
    /// §5.2.1); the tracks stretching afterwards is what gives it its share.
    fn column_contributions(
        &mut self,
        setup: &Setup,
        width: f32,
        tracks: &[Track],
    ) -> Vec<Contribution> {
        setup
            .items
            .iter()
            .map(|item| {
                let margin = resolve_margin(&item.style, width);
                let margins = margin.left + margin.right;
                let min_content =
                    self.contribution(item.id, width, None, Wanted::Narrowest) + margins;
                let max_content = self.contribution(item.id, width, None, Wanted::Widest) + margins;
                let frame = Frame::of(&item.style, width).inline;
                let room = InlineRoom::measuring(width, None, Wanted::Narrowest);
                let own_min = self
                    .inline_sizes(item.id, &item.style, room, frame)
                    .limits
                    .min;
                let minimum = automatic_minimum(
                    &item.style.min_width,
                    &item.style,
                    &tracks[item.area.columns.clone()],
                    min_content,
                    own_min + frame + margins,
                );
                Contribution {
                    tracks: item.area.columns.clone(),
                    minimum,
                    min_content,
                    max_content,
                }
            })
            .collect()
    }

    /// An item's width in an area `area_width` wide (§11.3), as a border box,
    /// and how far along the area it starts.
    fn item_width(&mut self, item: &GridItem, grid: &ComputedStyle, area_width: f32) -> (f32, f32) {
        let style = &item.style;
        let margin = resolve_margin(style, area_width);
        let margins = margin.left + margin.right;
        let frame = Frame::of(style, area_width).inline;
        let justify = used_alignment(
            style.justify_self.unwrap_or(grid.justify_items),
            has_ratio(&self.tree.node(item.id).kind),
        );
        let autos = (
            style.margin.left == LengthOrAuto::Auto,
            style.margin.right == LengthOrAuto::Auto,
        );
        let room = InlineRoom::laid_out(area_width, area_width - margins);
        let sizes = self.inline_sizes(item.id, style, room, frame);
        let stretches =
            justify == AlignItems::Stretch && sizes.preferred.is_none() && !autos.0 && !autos.1;
        let width = if stretches {
            sizes
                .limits
                .outer(frame)
                .clamp((area_width - margins).max(0.0))
        } else {
            self.shrink_to_fit_width(item.id, style, room, frame)
        };
        let offset = margin.left + align_offset(justify, area_width - margins - width, autos);
        (width, offset)
    }

    /// What an item `width` wide asks of the rows it spans: its height, and
    /// its minimum contribution, margins in.
    fn row_contribution(
        &mut self,
        item: &GridItem,
        width: f32,
        area_width: f32,
        tracks: &[Track],
    ) -> Contribution {
        let boxed = edges(item, area_width);
        let margins = boxed.margin.top + boxed.margin.bottom;
        let (height, limits) = self.block_size_at(&boxed, width, area_width);
        let outer = height + margins;
        let minimum = automatic_minimum(
            &item.style.min_height,
            &item.style,
            &tracks[item.area.rows.clone()],
            outer,
            limits.min + margins,
        );
        Contribution {
            tracks: item.area.rows.clone(),
            minimum,
            min_content: outer,
            max_content: outer,
        }
    }

    /// Align an item in its area down the block axis (§11.4) and lay it out.
    fn place_item(
        &mut self,
        item: &GridItem,
        grid: &ComputedStyle,
        (x, y): (f32, f32),
        width: f32,
        (area_width, area_height): (f32, f32),
    ) -> Fragment {
        let boxed = edges(item, area_width);
        let margins = boxed.margin.top + boxed.margin.bottom;
        let style = &item.style;
        let align = used_alignment(
            style.align_self.unwrap_or(grid.align_items),
            has_ratio(&self.tree.node(item.id).kind),
        );
        let autos = (
            style.margin.top == LengthOrAuto::Auto,
            style.margin.bottom == LengthOrAuto::Auto,
        );

        // The area is what a percentage height inside the item is of (§6.5).
        let outer_height = self.containing_height.replace(area_height);
        let (natural, limits) = self.block_size_at(&boxed, width, area_width);
        let stretches = align == AlignItems::Stretch
            && self.asked_height(style, area_width).is_none()
            && !autos.0
            && !autos.1;
        let (height, laid_out) = if stretches {
            let height = limits.clamp((area_height - margins).max(0.0));
            (
                height,
                ItemHeight::Exactly {
                    height,
                    definite: true,
                },
            )
        } else {
            (natural, ItemHeight::AtLeast(natural))
        };
        let offset = boxed.margin.top + align_offset(align, area_height - margins - height, autos);
        let fragment = self.layout_item(&boxed, (x, y + offset), width, laid_out, area_width);
        self.containing_height = outer_height;
        fragment
    }
}

/// A grid's own `min-width` and `max-width` where they are lengths, as content
/// widths: what an automatic repetition is counted against while the grid is
/// measured (§7.2.3.2). A percentage of the width being measured is cyclic,
/// and a content keyword asks the very size being measured, so neither
/// counts.
fn fixed_inline_limits(style: &ComputedStyle, frame: f32) -> Limits {
    let min = match &style.min_width {
        Size::Length(length) => length
            .definite(None)
            .map_or(0.0, |size| content_box(size, style.box_sizing, frame)),
        Size::Auto | Size::Intrinsic(_) | Size::Stretch => 0.0,
    };
    let max = match &style.max_width {
        MaxSize::Length(length) => length.definite(None).map_or(f32::INFINITY, |size| {
            content_box(size, style.box_sizing, frame)
        }),
        MaxSize::None | MaxSize::Intrinsic(_) | MaxSize::Stretch => f32::INFINITY,
    };
    Limits { min, max }
}

/// An item's edges, a percentage in its margins and padding being of its
/// area's width (§6.4).
fn edges(item: &GridItem, area_width: f32) -> ItemBox<'_> {
    ItemBox {
        id: item.id,
        style: &item.style,
        margin: resolve_margin(&item.style, area_width),
        padding: resolve_padding(&item.style, area_width),
        border: resolve_border(&item.style),
    }
}

/// Whether a box has a natural aspect ratio, which keeps `normal` from
/// stretching it.
fn has_ratio(kind: &BoxKind) -> bool {
    matches!(kind, BoxKind::Replaced(_))
}

/// An item's minimum contribution along one axis (§6.6): its own minimum
/// size where it names one, and otherwise its automatic minimum — the
/// content-based one where it spans an `auto`-minimum track and is not a
/// scroll container, unless it spans a flexible track among several; nothing
/// otherwise.
fn automatic_minimum(
    min: &Size,
    style: &ComputedStyle,
    tracks: &[Track],
    content: f32,
    named: f32,
) -> f32 {
    match min {
        Size::Auto => {
            let auto_min = tracks
                .iter()
                .any(|track| track.min == tracks::Minimum::Auto);
            let flexible_span = tracks.len() > 1
                && tracks
                    .iter()
                    .any(|track| matches!(track.max, tracks::Maximum::Flex(_)));
            if auto_min && !flexible_span && !style.overflow.is_scroll_container() {
                // Held by the tracks' fixed maximums, where every one has one.
                let fixed: Option<f32> = tracks
                    .iter()
                    .map(|track| match track.max {
                        tracks::Maximum::Fixed(size) => Some(size),
                        tracks::Maximum::Flex(_)
                        | tracks::Maximum::Auto
                        | tracks::Maximum::MinContent
                        | tracks::Maximum::MaxContent
                        | tracks::Maximum::FitContent(_) => None,
                    })
                    .sum();
                fixed.map_or(content, |cap| content.min(cap.max(named)))
            } else {
                named
            }
        }
        Size::Length(_) | Size::Intrinsic(_) | Size::Stretch => named,
    }
}

/// The tracks that are there, which the gaps go between.
fn visible(collapsed: &[bool]) -> usize {
    collapsed.iter().filter(|gone| !**gone).count()
}

/// The tracks laid end to end with the gaps between those that are there.
fn sum_with_gaps(sizes: &[f32], collapsed: &[bool], gap: f32) -> f32 {
    sizes.iter().sum::<f32>() + gap * visible(collapsed).saturating_sub(1) as f32
}
