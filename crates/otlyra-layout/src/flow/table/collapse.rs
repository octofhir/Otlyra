//! Collapsed table borders: one line on every edge, and which border wins it.
//!
//! Under `border-collapse: collapse` a border belongs to the edge between two
//! cells rather than to either of them. The contest for every edge is held here,
//! before the table is measured; each cell is left room for half of the lines it
//! meets, and the lines themselves are drawn once, by the table.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Sides};

use crate::box_tree::BoxId;
use crate::flow::Flow;
use crate::fragment::{Fragment, FragmentKind, Rect};

use super::Cell;

/// Whether `edge` takes a collapsed boundary from `line`.
///
/// The contest CSS describes, in its order: `hidden` silences the boundary and
/// nothing can put a line back on it; a border that draws nothing loses to one
/// that draws something; then the wider wins; and a tie on width goes to the more
/// insistent line, which is what puts `double` over `solid` and `solid` over a
/// line with gaps in it. A tie on both is left to whoever asked first, and the
/// edges are offered cell before row before table — which is the ownership order
/// CSS finishes with.
fn wins(edge: otlyra_css::Border, line: otlyra_css::Border) -> bool {
    use otlyra_css::BorderStyle;

    /// How loud a style is, for a tie on width.
    fn rank(style: BorderStyle) -> u8 {
        match style {
            BorderStyle::Double => 7,
            BorderStyle::Solid => 6,
            BorderStyle::Dashed => 5,
            BorderStyle::Dotted => 4,
            BorderStyle::Ridge => 3,
            BorderStyle::Outset => 2,
            BorderStyle::Groove => 1,
            BorderStyle::Inset | BorderStyle::None | BorderStyle::Hidden => 0,
        }
    }

    if line.style == BorderStyle::Hidden {
        return false;
    }
    if edge.style == BorderStyle::Hidden {
        return true;
    }
    match (edge.style.draws(), line.style.draws()) {
        (false, _) => false,
        (true, false) => true,
        (true, true) if edge.width != line.width => edge.width > line.width,
        (true, true) => rank(edge.style) > rank(line.style),
    }
}

/// The resolved lines of a collapsed table's grid.
///
/// One down each side of every column, one along the top and bottom of every row
/// — the width and colour that won the contest on each, which is what is actually
/// drawn.
pub(in crate::flow) struct TableLines {
    /// One per square down each side, row-major: `(columns + 1)` per row.
    vertical: Vec<otlyra_css::Border>,
    /// One per square along the top and bottom, row-major: `columns` per band.
    horizontal: Vec<otlyra_css::Border>,
    columns: usize,
    rows: usize,
}

/// One drawn line of a collapsed table's grid, as a fragment of its own.
///
/// A rectangle with the line's colour behind it and no border of its own: what is
/// drawn is a line, and a line with a border on it would be two.
fn line_fragment(border: otlyra_css::Border, rect: Rect) -> Option<Fragment> {
    if !border.is_visible() || rect.width <= 0.0 || rect.height <= 0.0 {
        return None;
    }
    // Snapped to whole pixels. A collapsed line is centred on the boundary between
    // two cells, and a boundary lands wherever the columns put it — so a line one
    // pixel wide falls across two of them and is drawn as two grey ones rather than
    // one black one. Every engine snaps these, and a table of hairlines is where it
    // shows.
    let snap = |from: f32, size: f32| {
        let start = from.round();
        (start, (from + size).round() - start)
    };
    let (x, width) = snap(rect.x, rect.width);
    let (y, height) = snap(rect.y, rect.height);
    let rect = Rect::new(x, y, width.max(1.0), height.max(1.0));
    let style = ComputedStyle {
        background_color: border.color,
        border: Sides::all(otlyra_css::Border::NONE),
        ..ComputedStyle::default()
    };
    Some(Fragment::new(
        None,
        rect,
        FragmentKind::Box,
        Arc::new(style),
    ))
}

impl<'a> Flow<'a> {
    /// Settle a collapsed table's borders before anything reads them, once.
    ///
    /// A table's own edge is one of the borders in the contest, so its box cannot
    /// be measured or placed until the contest has been held — which is why this
    /// sits at the front of everything that asks a table how wide it is.
    pub(in crate::flow) fn ensure_collapsed(&mut self, id: BoxId) {
        let style = &self.tree.node(id).style;
        if style.display == otlyra_css::Display::Table
            && style.border_collapse == otlyra_css::BorderCollapse::Collapse
            && !self.collapsed.contains_key(id)
        {
            self.collapse_borders(id);
        }
    }

    /// Work out a collapsed table's borders: which line is drawn on every edge of
    /// the grid, and how much room each cell has to leave for the ones it meets.
    ///
    /// With `border-collapse: collapse` a border is no longer a property of a box.
    /// It belongs to the edge between two cells: how wide it is, and what colour,
    /// is settled between the two of them — the wider wins, with the row's and the
    /// table's own edges in the running at the ends — and the line is then drawn
    /// once, by the table. Each of the two cells leaves half of it, so the two
    /// halves meet on the edge and add up to its width.
    ///
    /// Edge by edge rather than line by line: the boundary under one cell may be
    /// three pixels of one colour and one pixel of another under the cell beside
    /// it, and a table drawn a line at a time paints the loudest of them the whole
    /// way across. What a cell leaves room for is the widest edge along that side
    /// of it, so a line never needs more room than the cells gave it.
    ///
    /// The width is the whole of the contest here. CSS breaks a tie by border
    /// style and then by who owns the edge; ties are left to the first of the two,
    /// and `<col>`, which is one of the owners, generates no box for us to ask.
    fn collapse_borders(&mut self, table: BoxId) {
        let mut captions = Vec::new();
        let mut rows: Vec<BoxId> = Vec::new();
        self.collect_rows(table, &mut captions, &mut rows);
        let cells = self.place_cells(&rows);
        let columns = cells
            .iter()
            .flatten()
            .map(|cell| cell.column + cell.columns)
            .max()
            .unwrap_or(0);
        if columns == 0 || rows.is_empty() {
            return;
        }

        // Which cell holds each square of the grid, so that the two boxes meeting
        // on an edge can be asked what they wanted of it.
        let mut owner: Vec<Option<BoxId>> = vec![None; rows.len() * columns];
        for (index, row) in cells.iter().enumerate() {
            for cell in row {
                for band in index..index + cell.rows {
                    for column in cell.column..cell.column + cell.columns {
                        owner[band * columns + column] = Some(cell.id);
                    }
                }
            }
        }

        let own = Arc::clone(&self.tree.node(table).style);
        let widest = |line: &mut otlyra_css::Border, edge: otlyra_css::Border| {
            if wins(edge, *line) {
                *line = edge;
            }
        };
        let border_of = |id: Option<BoxId>| id.map(|id| &self.tree.node(id).style.border);

        // Every edge of the grid: one down each side of every square, one along the
        // top and bottom of each.
        let mut vertical = vec![otlyra_css::Border::NONE; (columns + 1) * rows.len()];
        let mut horizontal = vec![otlyra_css::Border::NONE; columns * (rows.len() + 1)];

        for band in 0..rows.len() {
            let row_style = Arc::clone(&self.tree.node(rows[band]).style);
            for column in 0..=columns {
                let edge = &mut vertical[band * (columns + 1) + column];
                if column > 0
                    && let Some(border) = border_of(owner[band * columns + column - 1])
                {
                    widest(edge, border.right);
                }
                if column < columns
                    && let Some(border) = border_of(owner[band * columns + column])
                {
                    widest(edge, border.left);
                }
                if column == 0 {
                    widest(edge, row_style.border.left);
                    widest(edge, own.border.left);
                }
                if column == columns {
                    widest(edge, row_style.border.right);
                    widest(edge, own.border.right);
                }
            }
        }

        for band in 0..=rows.len() {
            for column in 0..columns {
                let edge = &mut horizontal[band * columns + column];
                if band > 0 {
                    if let Some(border) = border_of(owner[(band - 1) * columns + column]) {
                        widest(edge, border.bottom);
                    }
                    widest(edge, self.tree.node(rows[band - 1]).style.border.bottom);
                }
                if band < rows.len() {
                    if let Some(border) = border_of(owner[band * columns + column]) {
                        widest(edge, border.top);
                    }
                    widest(edge, self.tree.node(rows[band]).style.border.top);
                }
                if band == 0 {
                    widest(edge, own.border.top);
                }
                if band == rows.len() {
                    widest(edge, own.border.bottom);
                }
            }
        }

        // What a box leaves room for on one of its sides: half of the widest edge
        // along it, which is what makes the halves on either side of every edge add
        // up to the line drawn on it.
        let widest_vertical = |column: usize, bands: std::ops::Range<usize>| -> f32 {
            bands
                .map(|band| vertical[band * (columns + 1) + column].width)
                .fold(0.0, f32::max)
                / 2.0
        };
        let widest_horizontal = |band: usize, span: std::ops::Range<usize>| -> f32 {
            span.map(|column| horizontal[band * columns + column].width)
                .fold(0.0, f32::max)
                / 2.0
        };
        let room = |width: f32| otlyra_css::Border {
            width,
            // The line is drawn by the table, once. What is left on a box is the
            // room for it and nothing to see.
            color: otlyra_gfx::peniko::Color::TRANSPARENT,
            style: otlyra_css::BorderStyle::Solid,
        };

        for (index, row) in cells.iter().enumerate() {
            for cell in row {
                let bands = index..index + cell.rows;
                let span = cell.column..cell.column + cell.columns;
                let mut style = (*self.tree.node(cell.id).style).clone();
                style.border = Sides {
                    top: room(widest_horizontal(index, span.clone())),
                    right: room(widest_vertical(cell.column + cell.columns, bands.clone())),
                    bottom: room(widest_horizontal(index + cell.rows, span)),
                    left: room(widest_vertical(cell.column, bands)),
                };
                self.collapsed.insert(cell.id, Arc::new(style));
            }
        }

        let mut style = (*own).clone();
        style.border = Sides {
            top: room(widest_horizontal(0, 0..columns)),
            right: room(widest_vertical(columns, 0..rows.len())),
            bottom: room(widest_horizontal(rows.len(), 0..columns)),
            left: room(widest_vertical(0, 0..rows.len())),
        };
        self.collapsed.insert(table, Arc::new(style));

        // A row's own border went into the edges above and below it and is drawn
        // there; drawn again on the row it would be drawn twice.
        for &row in &rows {
            let mut style = (*self.tree.node(row).style).clone();
            style.border = Sides::all(otlyra_css::Border::NONE);
            self.collapsed.insert(row, Arc::new(style));
        }

        self.collapsed_lines.insert(
            table,
            TableLines {
                vertical,
                horizontal,
                columns,
                rows: rows.len(),
            },
        );
    }

    /// The lines of a collapsed table, as fragments the table draws itself.
    ///
    /// Edge by edge, because a cell that reaches across a boundary is not divided
    /// by it — the line inside a `colspan` is not drawn, and neither is the one
    /// inside a `rowspan` — and because two edges of one boundary may have been
    /// won by different borders. Neighbouring edges that agree become one
    /// rectangle, so an ordinary table is a handful of fragments rather than one
    /// per cell edge.
    ///
    /// Each line is centred on the boundary, which is where the cells left half of
    /// it on either side; the ends reach half of the crossing line, so the corners
    /// are filled rather than notched.
    pub(super) fn collapsed_grid(
        &self,
        table: BoxId,
        widths: &[f32],
        tops: &[f32],
        heights: &[f32],
        cells: &[Vec<Cell>],
        x: f32,
    ) -> Vec<Fragment> {
        let Some(lines) = self.collapsed_lines.get(table) else {
            return Vec::new();
        };
        let (columns, rows) = (lines.columns, lines.rows);
        if rows == 0 || widths.len() < columns || heights.len() < rows {
            return Vec::new();
        }

        // Which boundaries a cell reaches across, and so which are not drawn.
        let mut crossed_vertical = vec![false; (columns + 1) * rows];
        let mut crossed_horizontal = vec![false; columns * (rows + 1)];
        for (index, row) in cells.iter().enumerate() {
            for cell in row {
                for column in cell.column + 1..cell.column + cell.columns {
                    for band in index..index + cell.rows {
                        crossed_vertical[band * (columns + 1) + column] = true;
                    }
                }
                for band in index + 1..index + cell.rows {
                    for column in cell.column..cell.column + cell.columns {
                        crossed_horizontal[band * columns + column] = true;
                    }
                }
            }
        }

        // Where each boundary sits: the cells tile without gaps, so a column
        // boundary is where the previous column ended.
        let mut down = Vec::with_capacity(columns + 1);
        let mut at = x;
        for width in widths.iter().take(columns) {
            down.push(at);
            at += width;
        }
        down.push(at);

        let mut across: Vec<f32> = tops.iter().take(rows).copied().collect();
        across.push(tops[rows - 1] + heights[rows - 1]);

        // Half of the widest edge on a crossing boundary, which is how far a line
        // reaches past the square it belongs to.
        let vertical_reach = |band: usize| -> f32 {
            (0..=columns)
                .map(|column| lines.vertical[band.min(rows - 1) * (columns + 1) + column].width)
                .fold(0.0, f32::max)
                / 2.0
        };
        let horizontal_reach = |column: usize| -> f32 {
            (0..=rows)
                .map(|band| lines.horizontal[band * columns + column.min(columns - 1)].width)
                .fold(0.0, f32::max)
                / 2.0
        };

        let mut out = Vec::new();

        for column in 0..=columns {
            let mut band = 0;
            while band < rows {
                let edge = lines.vertical[band * (columns + 1) + column];
                if crossed_vertical[band * (columns + 1) + column] || !edge.is_visible() {
                    band += 1;
                    continue;
                }
                let start = band;
                while band < rows
                    && !crossed_vertical[band * (columns + 1) + column]
                    && lines.vertical[band * (columns + 1) + column] == edge
                {
                    band += 1;
                }
                let top = across[start] - horizontal_reach(column);
                let bottom = across[band] + horizontal_reach(column);
                out.extend(line_fragment(
                    edge,
                    Rect::new(
                        down[column] - edge.width / 2.0,
                        top,
                        edge.width,
                        bottom - top,
                    ),
                ));
            }
        }

        for band in 0..=rows {
            let mut column = 0;
            while column < columns {
                let edge = lines.horizontal[band * columns + column];
                if crossed_horizontal[band * columns + column] || !edge.is_visible() {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < columns
                    && !crossed_horizontal[band * columns + column]
                    && lines.horizontal[band * columns + column] == edge
                {
                    column += 1;
                }
                let left = down[start] - vertical_reach(band);
                let right = down[column] + vertical_reach(band);
                out.extend(line_fragment(
                    edge,
                    Rect::new(
                        left,
                        across[band] - edge.width / 2.0,
                        right - left,
                        edge.width,
                    ),
                ));
            }
        }

        out
    }
}
