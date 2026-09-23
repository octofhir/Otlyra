//! Grid layout: items placed into rows and columns.
//!
//! Track sizing and auto-placement as CSS Grid describes them, in the simple
//! row-major form. Nothing outside a grid asks it anything but how tall it came
//! out, so all of it is here.

use std::sync::Arc;

use crate::box_tree::BoxId;
use crate::fragment::Fragment;

use super::Flow;
use super::intrinsic::Wanted;

/// The column a grid line names.
///
/// Lines count from one, and a negative line counts back from the end — which is
/// how `grid-column: -1` means the last one without knowing how many there are.
fn line_to_column(line: i32, count: usize) -> usize {
    if line > 0 {
        ((line - 1) as usize).min(count.saturating_sub(1))
    } else if line < 0 && count != usize::MAX {
        count.saturating_sub((-line) as usize)
    } else {
        0
    }
}

impl<'a> Flow<'a> {
    /// A grid formatting context: the children are placed into rows and columns.
    ///
    /// The columns come from `grid-template-columns`: a fixed track takes what it
    /// asks for, an `auto` one takes what its widest item wants, and the `fr` tracks
    /// share out whatever is left. Items are then placed in order, filling each row
    /// before starting the next, and each row is as tall as the tallest thing in it
    /// unless `grid-template-rows` says otherwise.
    pub(super) fn layout_grid(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(parent).style);
        let children = self.tree.node(parent).children.clone();
        if children.is_empty() {
            return 0.0;
        }

        let column_gap = style.gap.1.resolve(width);
        let row_gap = style.gap.0.resolve(width);

        // A grid with no template is one column of everything, which is what a
        // block container would have done and is the least surprising fallback.
        let mut template = style.grid_columns.clone();

        // `repeat(auto-fill, …)`: the pattern goes in as many times as the room
        // left over allows, which is what makes a card grid answer to its width
        // without a media query.
        if let Some(pattern) = style.grid_columns_fill.as_ref()
            && !pattern.is_empty()
        {
            let spent: f32 = template
                .iter()
                .map(|track| match track {
                    otlyra_css::Track::Fixed(length) => length.resolve(width),
                    _ => 0.0,
                })
                .sum::<f32>()
                + column_gap * template.len() as f32;
            let one: f32 = pattern
                .iter()
                .map(|track| match track {
                    otlyra_css::Track::Fixed(length) => length.resolve(width),
                    _ => 0.0,
                })
                .sum::<f32>()
                + column_gap * (pattern.len().saturating_sub(1)) as f32;

            let times = if one > 0.0 {
                (((width - spent + column_gap) / (one + column_gap)).floor() as usize).max(1)
            } else {
                1
            };
            for _ in 0..times {
                template.extend(pattern.iter().cloned());
            }
        }

        if template.is_empty() {
            template.push(otlyra_css::Track::Auto);
        }
        let count = template.len();

        // Where every item goes. An item that names a line takes those cells; the
        // rest are placed in order into whatever is still free, which is the
        // auto-placement CSS describes, in its simple row-major form.
        let mut taken: Vec<bool> = Vec::new();
        let mut cells: Vec<(usize, usize, usize)> = Vec::with_capacity(children.len());
        let mut cursor_cell = 0usize;
        let occupied = |taken: &mut Vec<bool>, row: usize, column: usize| -> bool {
            let at = row * count + column;
            if at >= taken.len() {
                taken.resize(at + 1, false);
            }
            taken[at]
        };
        let occupy = |taken: &mut Vec<bool>, row: usize, column: usize| {
            let at = row * count + column;
            if at >= taken.len() {
                taken.resize(at + 1, false);
            }
            taken[at] = true;
        };

        for &child in &children {
            let item = Arc::clone(&self.tree.node(child).style);
            let span = (item.grid_column.span as usize).clamp(1, count);

            let (row, column) = match (item.grid_column.line, item.grid_row.line) {
                // A line of its own. The cursor never goes backwards, so a free cell
                // left behind by an item placed further along stays empty — CSS
                // fills those only when asked to, with `grid-auto-flow: dense`.
                (Some(line), row) => {
                    let column = line_to_column(line, count);
                    let from = cursor_cell / count;
                    let row = row.map_or_else(
                        || {
                            (from..)
                                .find(|row| {
                                    (0..span).all(|offset| {
                                        !occupied(
                                            &mut taken,
                                            *row,
                                            (column + offset).min(count - 1),
                                        )
                                    })
                                })
                                .unwrap_or(from)
                        },
                        |line| line_to_column(line, usize::MAX),
                    );
                    (row, column)
                }
                (None, Some(line)) => {
                    let row = line_to_column(line, usize::MAX);
                    let column = (0..count)
                        .find(|column| !occupied(&mut taken, row, *column))
                        .unwrap_or(0);
                    (row, column)
                }
                (None, None) => {
                    // The next free run of `span` cells, from wherever the cursor
                    // has got to.
                    loop {
                        let row = cursor_cell / count;
                        let column = cursor_cell % count;
                        let fits = column + span <= count
                            && (0..span).all(|offset| !occupied(&mut taken, row, column + offset));
                        if fits {
                            break (row, column);
                        }
                        cursor_cell += 1;
                    }
                }
            };

            // Wherever it went, the next item starts after it.
            cursor_cell = cursor_cell.max(row * count + column + span);

            for offset in 0..span {
                occupy(&mut taken, row, (column + offset).min(count - 1));
            }
            cells.push((row, column, span));
        }

        // What each column has to hold, which is what an `auto` track is measured
        // from: the widest item in it, and an item spanning several tracks counts
        // towards none of them on its own.
        //
        // An item's percentage width is of its grid area, which is what is being
        // measured, so CSS makes it cyclic (CSS Sizing 3 §5.2.1) and counts on the
        // `auto` tracks then stretching to fill the container (CSS Grid §11.8) to
        // give the item its share. That step is not taken here, so the
        // container's width stands in for the area — which is what it comes to in
        // the one-column grid a search box or a stack of cards is written as.
        let mut column_content = vec![0.0f32; count];
        for (index, &child) in children.iter().enumerate() {
            let (_, column, span) = cells[index];
            if span == 1 {
                let wanted = self.contribution(child, width, Some(width), Wanted::Widest);
                column_content[column] = column_content[column].max(wanted);
            }
        }

        // The columns: fixed first, then `auto` from content, then the leftover
        // shared out by `fr`.
        let gaps = column_gap * (count.saturating_sub(1)) as f32;
        let mut columns = vec![0.0f32; count];
        let mut fractions = 0.0f32;
        let mut used = gaps;
        for (index, track) in template.iter().enumerate() {
            match track {
                otlyra_css::Track::Fixed(length) => {
                    columns[index] = length.resolve(width);
                    used += columns[index];
                }
                otlyra_css::Track::Auto => {
                    columns[index] = column_content[index];
                    used += columns[index];
                }
                otlyra_css::Track::Fraction(share) => fractions += share.max(0.0),
            }
        }
        let leftover = (width - used).max(0.0);
        if fractions > 0.0 {
            for (index, track) in template.iter().enumerate() {
                if let otlyra_css::Track::Fraction(share) = track {
                    columns[index] = leftover * share.max(0.0) / fractions;
                }
            }
        }

        // Where each column starts.
        let mut offsets = Vec::with_capacity(count);
        let mut at = x;
        for column in &columns {
            offsets.push(at);
            at += column + column_gap;
        }

        // A grid establishes a formatting context of its own.
        let outer_floats = std::mem::take(&mut self.floats);

        let rows = cells.iter().map(|(row, _, _)| row + 1).max().unwrap_or(0);
        let mut row_tops = vec![y; rows + 1];
        let mut fragments: Vec<(usize, Fragment)> = Vec::with_capacity(children.len());

        // One row at a time, because a row is as tall as the tallest thing in it and
        // the next row starts where it ends.
        for row in 0..rows {
            let mut height = 0.0f32;
            for (index, &child) in children.iter().enumerate() {
                let (item_row, column, span) = cells[index];
                if item_row != row {
                    continue;
                }
                // A span covers its columns and the gaps between them.
                let cell_width: f32 = (0..span)
                    .map(|offset| columns.get(column + offset).copied().unwrap_or(0.0))
                    .sum::<f32>()
                    + column_gap * (span.saturating_sub(1)) as f32;
                let fragment = self.layout_sized(
                    child,
                    offsets.get(column).copied().unwrap_or(x),
                    row_tops[row],
                    cell_width,
                );
                height = height.max(fragment.rect.height);
                fragments.push((index, fragment));
            }

            // A row with a size of its own takes it, whatever is in it. An `fr` down
            // the block axis needs a definite height to share out; without one it is
            // what the content needs, which is what `auto` already gives.
            if let Some(otlyra_css::Track::Fixed(length)) = style.grid_rows.get(row) {
                height = length.resolve(width);
            }

            for (index, fragment) in &mut fragments {
                if cells[*index].0 != row {
                    continue;
                }
                // Stretched to the row, which is `align-items: stretch` and is what
                // makes a row of cards the same height.
                let id = fragment.box_id.expect("a grid item came from a box");
                let has_height = self
                    .asked_height(&self.tree.node(id).style, fragment.rect.width)
                    .is_some();
                if !has_height && fragment.rect.height < height {
                    fragment.rect.height = height;
                }
            }

            row_tops[row + 1] = row_tops[row] + height + row_gap;
        }

        for (_, fragment) in fragments {
            out.push(fragment);
        }

        self.floats = outer_floats;
        (row_tops[rows] - row_gap - y).max(0.0)
    }
}
