//! Table layout: rows of cells in columns sized by what is in them.
//!
//! Automatic table layout — every column measured at its narrowest and at its
//! widest, and the room shared out between the two — and the grid a table's
//! cells are placed in, which is not the tree they were written in. Collapsed
//! borders are a contest held before any of that, and are in `collapse`.

mod collapse;

use std::sync::Arc;

use crate::box_tree::BoxId;
use crate::fragment::{Fragment, FragmentKind, Layer, Rect};

use super::{Flow, offset};

pub(super) use collapse::TableLines;

/// Share `available` out between columns that each want between a minimum and a
/// maximum.
///
/// The three cases CSS names, in order. Everything fits unwrapped, so every column
/// gets what it wants and the table is narrower than the room it was offered.
/// Nothing fits, so every column is squeezed to its minimum and the table overflows
/// rather than tearing words in half. Or it is in between, and the surplus over the
/// minimums is shared in proportion to how much more each column could use — which
/// is what makes a column of long prose take the room a column of dates does not.
fn share_out(minimums: &[f32], maximums: &[f32], available: f32) -> Vec<f32> {
    let wanted: f32 = maximums.iter().sum();
    if wanted <= available {
        return maximums.to_vec();
    }

    let needed: f32 = minimums.iter().sum();
    if needed >= available {
        return minimums.to_vec();
    }

    let surplus = available - needed;
    let room: f32 = minimums
        .iter()
        .zip(maximums)
        .map(|(min, max)| (max - min).max(0.0))
        .sum();
    if room <= 0.0 {
        return minimums.to_vec();
    }

    minimums
        .iter()
        .zip(maximums)
        .map(|(min, max)| min + surplus * (max - min).max(0.0) / room)
        .collect()
}

/// A table cell, and where in the grid it landed.
///
/// Which column a cell is in is not which child of its row it is: a cell reaching
/// down from an earlier row holds a place in this one, and the cells beside it
/// start after it.
struct Cell {
    id: BoxId,
    column: usize,
    columns: usize,
    /// Rows, resolved: never zero and never past the last row of the table.
    rows: usize,
}

/// Raise `values` until they add up to `wanted`, keeping their proportions.
///
/// What a cell reaching across several of them asks: not that any one is that
/// wide or that tall, but that between them they cover it. What they cannot cover
/// is shared out in proportion to what each already holds, so a column that was
/// wider stays the wider of the two — and evenly when none of them holds anything,
/// which is the only sensible reading of a proportion of nothing.
fn spread(values: &mut [f32], wanted: f32) {
    if values.is_empty() {
        return;
    }
    let have: f32 = values.iter().sum();
    let excess = wanted - have;
    if excess <= 0.0 {
        return;
    }
    if have > 0.0 {
        for value in values.iter_mut() {
            *value += excess * *value / have;
        }
    } else {
        let share = excess / values.len() as f32;
        for value in values.iter_mut() {
            *value += share;
        }
    }
}

impl<'a> Flow<'a> {
    /// Lay out a table: rows of cells in columns sized by what is in them.
    ///
    /// This is *auto* table layout, which is the one the web is written against and
    /// the one a table with no widths on it gets. Every column is measured twice —
    /// how narrow it can be without its contents spilling, and how wide it would be
    /// if nothing wrapped — and the room is shared out between those two answers.
    /// A table is shrink-to-fit: if everything in it fits without wrapping, it is
    /// exactly that wide and no wider, which is why a two-column table of short
    /// words does not stretch across the page.
    pub(super) fn layout_table(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> Option<f32> {
        let style = Arc::clone(&self.tree.node(parent).style);
        // Collapsed, the cells meet on a line rather than sitting apart on their
        // own edges, and `border-spacing` says nothing at all.
        let (spacing_x, spacing_y) = match style.border_collapse {
            otlyra_css::BorderCollapse::Collapse => (0.0, 0.0),
            otlyra_css::BorderCollapse::Separate => style.border_spacing,
        };

        let mut captions = Vec::new();
        let mut rows: Vec<BoxId> = Vec::new();
        self.collect_rows(parent, &mut captions, &mut rows);
        let cells = self.place_cells(&rows);

        let columns = cells
            .iter()
            .flatten()
            .map(|cell| cell.column + cell.columns)
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return None;
        }

        // Room for the gaps first: what is left is what the columns share.
        let gaps = spacing_x * (columns + 1) as f32;
        let available = (width - gaps).max(0.0);

        let mut minimums = vec![0.0f32; columns];
        let mut maximums = vec![0.0f32; columns];
        for cell in cells.iter().flatten().filter(|cell| cell.columns == 1) {
            let column = cell.column;
            minimums[column] =
                minimums[column].max(self.min_content_width(cell.id, available, false));
            maximums[column] = maximums[column].max(self.max_content_width(cell.id, available));
        }

        // A cell across several columns asks nothing of any one of them: it asks
        // that they add up, and only what they cannot cover between them is shared
        // out. The narrower spans go first, so a wide one sees what the spans
        // inside it have already asked for.
        let mut spanning: Vec<&Cell> = cells
            .iter()
            .flatten()
            .filter(|cell| cell.columns > 1)
            .collect();
        spanning.sort_by_key(|cell| cell.columns);
        let spanning: Vec<(std::ops::Range<usize>, f32, f32)> = spanning
            .iter()
            .map(|cell| {
                let covered = cell.column..cell.column + cell.columns;
                let between = spacing_x * (cell.columns - 1) as f32;
                (
                    covered,
                    self.min_content_width(cell.id, available, false) - between,
                    self.max_content_width(cell.id, available) - between,
                )
            })
            .collect();
        for (covered, wanted_min, wanted_max) in spanning {
            spread(&mut minimums[covered.clone()], wanted_min);
            spread(&mut maximums[covered.clone()], wanted_max);
            for column in covered {
                maximums[column] = maximums[column].max(minimums[column]);
            }
        }

        // A `<col>` says how wide its column *wants* to be, which is what a cell's
        // own width says: the column is drawn at that width where the content fits
        // in it, and at whatever the content cannot go below where it does not. So
        // the number replaces what the content wanted rather than being taken
        // alongside it, and the content's own floor still wins. Measured against a
        // reference on both halves — a column told forty wraps its text to forty,
        // and one told sixty holding a sixty-two-pixel word is sixty-two.
        let declared = self.tree.columns(parent).to_vec();
        for (column, style) in declared.iter().enumerate().take(columns) {
            let Some(asked) = style.width.resolve(available) else {
                continue;
            };
            maximums[column] = asked.max(minimums[column]);
        }

        let mut widths = share_out(&minimums, &maximums, available);
        // A table told how wide to be fills that width: the columns keep their
        // proportions and share out what is left over, rather than sitting narrow
        // in a box that was asked to be wide.
        if style.width.resolve(width).is_some() {
            let taken: f32 = widths.iter().sum();
            if taken > 0.0 && available > taken {
                let scale = available / taken;
                for column in &mut widths {
                    *column *= scale;
                }
            }
        }
        // A caption cannot be narrower than its longest word, and the table cannot
        // be narrower than its caption: a two-letter table under a one-word caption
        // is as wide as the word, with its columns stretched to fill. The floor is
        // on the table's border box, so its own edges come off it first.
        let frame = {
            let style = self.style_of(parent);
            style.border.left.width + style.border.right.width
        };
        let caption_floor = captions
            .iter()
            .map(|&caption| self.min_content_width(caption, available, false))
            .fold(0.0f32, f32::max)
            - frame;
        let taken: f32 = widths.iter().sum();
        if taken > 0.0 && caption_floor - gaps > taken {
            let scale = (caption_floor - gaps) / taken;
            for column in &mut widths {
                *column *= scale;
            }
        }

        let table_width = widths.iter().sum::<f32>() + gaps;

        let mut cursor = y;
        let mut fragments = Vec::new();

        // A caption is a block of its own, as wide as the table and above it. CSS
        // can put it below; nothing writes that.
        for caption in captions {
            let fragment = self.layout_block(caption, table_width, x, cursor);
            cursor = fragment.rect.bottom();
            fragments.push(fragment);
        }

        // Where each column starts.
        let mut offsets = Vec::with_capacity(columns);
        let mut at = x + spacing_x;
        for column in &widths {
            offsets.push(at);
            at += column + spacing_x;
        }

        // The cells are laid out before the rows have anywhere to be: how tall a
        // row is depends on what is in it, and a cell reaching into the rows below
        // depends on all of them. So they are laid out at the top and moved down
        // once the bands are known.
        let mut placed: Vec<Vec<Fragment>> = Vec::with_capacity(rows.len());
        for row in &cells {
            let mut laid = Vec::with_capacity(row.len());
            for cell in row {
                let covered = cell.column..cell.column + cell.columns;
                let cell_width =
                    widths[covered].iter().sum::<f32>() + spacing_x * (cell.columns - 1) as f32;
                laid.push(self.layout_block(cell.id, cell_width, offsets[cell.column], 0.0));
            }
            placed.push(laid);
        }

        // A row is as tall as the tallest cell that ends in it; a cell reaching
        // further down asks only that the rows it covers add up, the way a cell
        // across several columns asks it of them.
        let mut heights = vec![0.0f32; rows.len()];
        let mut reaching: Vec<(std::ops::Range<usize>, f32)> = Vec::new();
        for (index, row) in cells.iter().enumerate() {
            for (cell, fragment) in row.iter().zip(&placed[index]) {
                if cell.rows == 1 {
                    heights[index] = heights[index].max(fragment.rect.height);
                } else {
                    reaching.push((
                        index..index + cell.rows,
                        fragment.rect.height - spacing_y * (cell.rows - 1) as f32,
                    ));
                }
            }
        }
        reaching.sort_by_key(|(covered, _)| covered.len());
        for (covered, wanted) in reaching {
            spread(&mut heights[covered], wanted);
        }

        let mut tops = Vec::with_capacity(rows.len());
        for height in &heights {
            cursor += spacing_y;
            tops.push(cursor);
            cursor += height;
        }
        cursor += spacing_y;

        // A column's own background, behind the cells that sit in it and in front
        // of the table's. A column is not a box — nothing is laid out in it — so
        // this is the whole of what one draws.
        if let (Some(&first), Some((&last, &height))) =
            (tops.first(), tops.last().zip(heights.last()))
        {
            for (column, style) in declared.iter().enumerate().take(columns) {
                let paints = style.background_color.components[3] > 0.0
                    || style.backgrounds.iter().any(|layer| layer.draws());
                if !paints {
                    continue;
                }
                fragments.push(Fragment {
                    used: None,
                    box_id: None,
                    rect: Rect::new(
                        offsets[column],
                        first,
                        widths[column],
                        last + height - first,
                    ),
                    kind: FragmentKind::Box,
                    style: Arc::clone(style),
                    widget: None,
                    fixed: false,
                    scroll_port: None,
                    clip: None,
                    sticky: None,
                    layer: Layer::default(),
                    children: Vec::new(),
                });
            }
        }

        for (index, row) in rows.iter().enumerate() {
            let row_style = self.style_of(*row);
            let mut laid = std::mem::take(&mut placed[index]);

            for (cell, fragment) in cells[index].iter().zip(&mut laid) {
                offset(fragment, 0.0, tops[index]);
                // Every cell fills the band it covers: one that stopped short
                // would leave a hole in its own background.
                let covered = index..index + cell.rows;
                fragment.rect.height =
                    heights[covered].iter().sum::<f32>() + spacing_y * (cell.rows - 1) as f32;
            }

            fragments.push(Fragment {
                used: None,
                box_id: Some(*row),
                rect: Rect::new(
                    x + spacing_x,
                    tops[index],
                    table_width - spacing_x * 2.0,
                    heights[index],
                ),
                kind: FragmentKind::Box,
                style: row_style,
                widget: None,
                fixed: false,
                scroll_port: None,
                clip: None,
                sticky: None,
                layer: Layer::default(),
                children: laid,
            });
        }

        // The grid, drawn once and last: a collapsed border is the edge between two
        // cells rather than either cell's own, and the cells left room for it
        // without drawing it.
        fragments.extend(self.collapsed_grid(parent, &widths, &tops, &heights, &cells, x));

        out.extend(fragments);
        self.table_width = Some(table_width);
        Some(cursor - y)
    }

    /// Put every cell somewhere in the table's grid, row by row.
    ///
    /// A cell goes in the first column its row has left, which is what makes a
    /// cell reaching down from an earlier row push the ones beside it along: the
    /// cells it covers are spoken for before this row is read. A `rowspan` of zero
    /// is HTML's "the rest of them", which is only knowable here.
    fn place_cells(&self, rows: &[BoxId]) -> Vec<Vec<Cell>> {
        let mut taken: Vec<Vec<bool>> = vec![Vec::new(); rows.len()];
        let mut placed = Vec::with_capacity(rows.len());

        for (index, &row) in rows.iter().enumerate() {
            let mut cells = Vec::new();
            let mut column = 0;

            for &id in &self.tree.node(row).children {
                if self.tree.node(id).style.display != otlyra_css::Display::TableCell {
                    continue;
                }
                while taken[index].get(column).copied().unwrap_or(false) {
                    column += 1;
                }

                let span = self.tree.span(id);
                let down = match span.rows {
                    0 => rows.len() - index,
                    wanted => wanted.min(rows.len() - index),
                };
                for band in &mut taken[index..index + down] {
                    if band.len() < column + span.columns {
                        band.resize(column + span.columns, false);
                    }
                    band[column..column + span.columns].fill(true);
                }

                cells.push(Cell {
                    id,
                    column,
                    columns: span.columns,
                    rows: down,
                });
                column += span.columns;
            }

            placed.push(cells);
        }

        placed
    }

    /// Walk a table's children for its captions and its rows, through whatever row
    /// groups it has.
    fn collect_rows(&self, parent: BoxId, captions: &mut Vec<BoxId>, rows: &mut Vec<BoxId>) {
        for &child in &self.tree.node(parent).children {
            match self.tree.node(child).style.display {
                otlyra_css::Display::TableCaption => captions.push(child),
                otlyra_css::Display::TableRow => rows.push(child),
                otlyra_css::Display::TableRowGroup => self.collect_rows(child, captions, rows),
                _ => {}
            }
        }
    }
}
