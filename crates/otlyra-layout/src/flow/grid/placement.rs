//! Where each grid item goes: its lines resolved (CSS Grid 2 §8.3) and the
//! rest placed by the auto-placement algorithm (§8.5).
//!
//! Lines here count from zero, the explicit grid's first line being line 0,
//! and may be negative where an item reaches before it. Once placed, areas
//! are in tracks counted from the first track of the whole grid, implicit
//! ones before the explicit grid included.

use std::ops::Range;

use otlyra_css::{FlowAxis, GridAutoFlow, GridLine, GridPlacement, LineName};

use super::explicit::LineNames;

/// How far from the explicit grid a line may be: the specification's own
/// suggestion for clamping overly large grids (§8, "Clamping Overly Large
/// Grids"). A page cannot make a grid bigger than this.
pub(super) const GRID_LIMIT: i32 = 10_000;

/// An item's position along one axis, as far as its own properties settle it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum AxisPlacement {
    /// Between these two lines.
    Definite(Range<i32>),
    /// Wherever auto-placement puts it, this many tracks wide.
    Auto { span: u32 },
}

impl AxisPlacement {
    fn span(&self) -> u32 {
        match self {
            Self::Definite(lines) => lines.len() as u32,
            Self::Auto { span } => *span,
        }
    }
}

/// One edge before it is resolved against the other.
enum Edge {
    Line(i32),
    Span { count: u32, name: Option<LineName> },
    Auto,
}

/// Resolve one axis of an item's placement (§8.3) against a grid of
/// `explicit` tracks whose lines are named `names`, with the conflicts of
/// §8.3.1 settled.
pub(super) fn resolve_axis(
    placement: &GridPlacement,
    names: &LineNames,
    explicit: i32,
) -> AxisPlacement {
    let start = edge(&placement.start, names, explicit, "-start");
    let end = edge(&placement.end, names, explicit, "-end");
    let clamp = |line: i32| line.clamp(-GRID_LIMIT, GRID_LIMIT);
    let span_of = |count: u32, name: &Option<LineName>| match name {
        // An item placed automatically ignores a span that counts named
        // lines, since it has no line to count them from (§8.3.1).
        Some(_) => 1,
        None => count.min(GRID_LIMIT as u32),
    };
    match (start, end) {
        (Edge::Line(start), Edge::Line(end)) => {
            let (start, end) = (clamp(start), clamp(end));
            match start.cmp(&end) {
                std::cmp::Ordering::Less => AxisPlacement::Definite(start..end),
                std::cmp::Ordering::Equal => AxisPlacement::Definite(start..start + 1),
                std::cmp::Ordering::Greater => AxisPlacement::Definite(end..start),
            }
        }
        (Edge::Line(start), Edge::Span { count, name }) => {
            let start = clamp(start);
            let end = match name {
                Some(name) => forward(names, &name, start, count, explicit),
                None => start + count as i32,
            };
            AxisPlacement::Definite(start..clamp(end).max(start + 1))
        }
        (Edge::Span { count, name }, Edge::Line(end)) => {
            let end = clamp(end);
            let start = match name {
                Some(name) => backward(names, &name, end, count),
                None => end - count as i32,
            };
            AxisPlacement::Definite(clamp(start).min(end - 1)..end)
        }
        (Edge::Line(start), Edge::Auto) => {
            let start = clamp(start);
            AxisPlacement::Definite(start..start + 1)
        }
        (Edge::Auto, Edge::Line(end)) => {
            let end = clamp(end);
            AxisPlacement::Definite(end - 1..end)
        }
        // Two spans: the one at the end is dropped (§8.3.1).
        (Edge::Span { count, name }, Edge::Span { .. } | Edge::Auto)
        | (Edge::Auto, Edge::Span { count, name }) => AxisPlacement::Auto {
            span: span_of(count, &name),
        },
        (Edge::Auto, Edge::Auto) => AxisPlacement::Auto { span: 1 },
    }
}

/// One edge as far as it resolves on its own.
fn edge(line: &GridLine, names: &LineNames, explicit: i32, suffix: &str) -> Edge {
    match line {
        GridLine::Auto => Edge::Auto,
        // An area's own line if it has one, and otherwise the first line
        // with the name (§8.3).
        GridLine::Area(name) => {
            let own = names.lines(&name.suffixed(suffix));
            match own.first() {
                Some(&line) => Edge::Line(line),
                None => Edge::Line(nth_line(names, Some(name), 1, explicit)),
            }
        }
        GridLine::Line { nth, name } => {
            Edge::Line(nth_line(names, name.as_ref(), nth.get(), explicit))
        }
        GridLine::Span { count, name } => Edge::Span {
            count: count.get(),
            name: name.clone(),
        },
    }
}

/// The `nth` line, counting from the start when positive and from the end
/// when negative — only lines called `name`, when there is one. Where there
/// are too few, the implicit lines past that end of the grid all count as
/// having the name (§8.3).
fn nth_line(names: &LineNames, name: Option<&LineName>, nth: i32, explicit: i32) -> i32 {
    let Some(name) = name else {
        return if nth > 0 { nth - 1 } else { explicit + 1 + nth };
    };
    let lines = names.lines(name);
    let count = lines.len() as i32;
    if nth > 0 {
        match lines.get((nth - 1) as usize) {
            Some(&line) => line,
            None => explicit + (nth - count),
        }
    } else {
        let back = -nth;
        if back <= count {
            lines[(count - back) as usize]
        } else {
            -(back - count)
        }
    }
}

/// The `count`th line called `name` after `from`.
fn forward(names: &LineNames, name: &LineName, from: i32, count: u32, explicit: i32) -> i32 {
    let after: Vec<i32> = names
        .lines(name)
        .iter()
        .copied()
        .filter(|&line| line > from)
        .collect();
    let count = count as usize;
    match after.get(count - 1) {
        Some(&line) => line,
        None => explicit.max(from) + (count - after.len()) as i32,
    }
}

/// The `count`th line called `name` before `from`.
fn backward(names: &LineNames, name: &LineName, from: i32, count: u32) -> i32 {
    let before: Vec<i32> = names
        .lines(name)
        .iter()
        .rev()
        .copied()
        .filter(|&line| line < from)
        .collect();
    let count = count as usize;
    match before.get(count - 1) {
        Some(&line) => line,
        None => from.min(0) - (count - before.len()) as i32,
    }
}

/// An item's area, in tracks counted from the grid's first.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Area {
    pub(super) rows: Range<usize>,
    pub(super) columns: Range<usize>,
}

impl Area {
    fn overlaps(&self, other: &Self) -> bool {
        self.rows.start < other.rows.end
            && other.rows.start < self.rows.end
            && self.columns.start < other.columns.end
            && other.columns.start < self.columns.end
    }

    fn transposed(self) -> Self {
        Self {
            rows: self.columns,
            columns: self.rows,
        }
    }
}

/// The grid once everything is placed.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Placed {
    /// Each item's area, in the order they were given.
    pub(super) areas: Vec<Area>,
    pub(super) rows: usize,
    pub(super) columns: usize,
    /// How many implicit tracks come before the explicit grid's first.
    pub(super) row_origin: usize,
    pub(super) column_origin: usize,
}

/// The cells the items placed so far cover, kept as their areas: a page can
/// place an item ten thousand tracks away, and that must not cost a grid
/// that big.
#[derive(Default)]
struct Occupied(Vec<Area>);

impl Occupied {
    fn is_free(&self, area: &Area) -> bool {
        !self.0.iter().any(|taken| taken.overlaps(area))
    }
}

/// Place every item (§8.5), each given as its (row, column) placement, in
/// order-modified document order, into a grid of `explicit` (rows, columns).
///
/// Written for row flow; column flow is the same with the axes swapped.
pub(super) fn place(
    items: &[(AxisPlacement, AxisPlacement)],
    explicit: (usize, usize),
    flow: GridAutoFlow,
) -> Placed {
    match flow.axis {
        FlowAxis::Row => place_rows(items, explicit, flow.dense),
        FlowAxis::Column => {
            let swapped: Vec<(AxisPlacement, AxisPlacement)> = items
                .iter()
                .map(|(row, column)| (column.clone(), row.clone()))
                .collect();
            let placed = place_rows(&swapped, (explicit.1, explicit.0), flow.dense);
            Placed {
                areas: placed.areas.into_iter().map(Area::transposed).collect(),
                rows: placed.columns,
                columns: placed.rows,
                row_origin: placed.column_origin,
                column_origin: placed.row_origin,
            }
        }
    }
}

fn place_rows(
    items: &[(AxisPlacement, AxisPlacement)],
    explicit: (usize, usize),
    dense: bool,
) -> Placed {
    // Step 0: the implicit tracks definite lines reach before the explicit
    // grid, and the columns every definite placement and auto span needs.
    let before = |axis: fn(&(AxisPlacement, AxisPlacement)) -> &AxisPlacement| {
        items
            .iter()
            .filter_map(|item| match axis(item) {
                AxisPlacement::Definite(lines) => Some((-lines.start).max(0) as usize),
                AxisPlacement::Auto { .. } => None,
            })
            .max()
            .unwrap_or(0)
    };
    let row_origin = before(|item| &item.0);
    let column_origin = before(|item| &item.1);
    let shift = |lines: &Range<i32>, origin: usize| {
        (lines.start + origin as i32) as usize..(lines.end + origin as i32) as usize
    };
    let mut columns = explicit.1 + column_origin;
    for (_, column) in items {
        columns = columns.max(match column {
            AxisPlacement::Definite(lines) => shift(lines, column_origin).end,
            AxisPlacement::Auto { span } => *span as usize,
        });
    }

    let mut areas: Vec<Option<Area>> = vec![None; items.len()];
    let mut occupied = Occupied::default();

    // Step 1: what is placed in both axes.
    for (index, (row, column)) in items.iter().enumerate() {
        if let (AxisPlacement::Definite(rows), AxisPlacement::Definite(lines)) = (row, column) {
            let area = Area {
                rows: shift(rows, row_origin),
                columns: shift(lines, column_origin),
            };
            occupied.0.push(area.clone());
            areas[index] = Some(area);
        }
    }

    // Step 2: what is locked to a row. Sparse, each row keeps a cursor past
    // the items this step already put in it; dense, each starts at the first
    // column. A row too full for it makes columns of its own.
    let mut row_cursors: Vec<(Range<usize>, usize)> = Vec::new();
    for (index, (row, column)) in items.iter().enumerate() {
        let (AxisPlacement::Definite(rows), AxisPlacement::Auto { span }) = (row, column) else {
            continue;
        };
        let rows = shift(rows, row_origin);
        let span = *span as usize;
        let from = if dense {
            0
        } else {
            row_cursors
                .iter()
                .find(|(cursor_rows, _)| *cursor_rows == rows)
                .map_or(0, |(_, column)| *column)
        };
        let start = (from..)
            .find(|&start| {
                occupied.is_free(&Area {
                    rows: rows.clone(),
                    columns: start..start + span,
                })
            })
            .unwrap_or(from);
        let area = Area {
            rows: rows.clone(),
            columns: start..start + span,
        };
        columns = columns.max(area.columns.end);
        match row_cursors
            .iter_mut()
            .find(|(cursor_rows, _)| *cursor_rows == rows)
        {
            Some((_, cursor)) => *cursor = area.columns.end,
            None => row_cursors.push((rows, area.columns.end)),
        }
        occupied.0.push(area.clone());
        areas[index] = Some(area);
    }

    // Step 4: everything else, with the one auto-placement cursor.
    let (mut cursor_row, mut cursor_column) = (0usize, 0usize);
    for (index, (row, column)) in items.iter().enumerate() {
        if areas[index].is_some() {
            continue;
        }
        let row_span = row.span() as usize;
        if dense {
            (cursor_row, cursor_column) = (0, 0);
        }
        let area = match column {
            AxisPlacement::Definite(lines) => {
                let lines = shift(lines, column_origin);
                if !dense && lines.start < cursor_column {
                    cursor_row += 1;
                }
                cursor_column = lines.start;
                let row = (cursor_row..)
                    .find(|&row| {
                        occupied.is_free(&Area {
                            rows: row..row + row_span,
                            columns: lines.clone(),
                        })
                    })
                    .unwrap_or(cursor_row);
                cursor_row = row;
                Area {
                    rows: row..row + row_span,
                    columns: lines,
                }
            }
            AxisPlacement::Auto { span } => {
                let span = (*span as usize).min(columns);
                loop {
                    let found = (cursor_column..=columns.saturating_sub(span)).find(|&start| {
                        occupied.is_free(&Area {
                            rows: cursor_row..cursor_row + row_span,
                            columns: start..start + span,
                        })
                    });
                    if let Some(start) = found {
                        cursor_column = start;
                        break Area {
                            rows: cursor_row..cursor_row + row_span,
                            columns: start..start + span,
                        };
                    }
                    cursor_row += 1;
                    cursor_column = 0;
                }
            }
        };
        occupied.0.push(area.clone());
        areas[index] = Some(area);
    }

    let areas: Vec<Area> = areas.into_iter().flatten().collect();
    let rows = areas
        .iter()
        .map(|area| area.rows.end)
        .fold(explicit.0 + row_origin, usize::max);
    Placed {
        areas,
        rows,
        columns,
        row_origin,
        column_origin,
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroI32, NonZeroU32};

    use super::*;

    fn line(nth: i32) -> GridLine {
        GridLine::Line {
            nth: NonZeroI32::new(nth).expect("a line"),
            name: None,
        }
    }

    fn span(count: u32) -> GridLine {
        GridLine::Span {
            count: NonZeroU32::new(count).expect("a span"),
            name: None,
        }
    }

    fn name(text: &str) -> LineName {
        LineName(text.into())
    }

    fn between(start: GridLine, end: GridLine) -> GridPlacement {
        GridPlacement { start, end }
    }

    fn resolve(placement: GridPlacement, explicit: i32) -> AxisPlacement {
        resolve_axis(&placement, &LineNames::default(), explicit)
    }

    const ROWS: GridAutoFlow = GridAutoFlow {
        axis: FlowAxis::Row,
        dense: false,
    };

    #[test]
    fn negative_lines_count_from_the_end() {
        assert_eq!(
            resolve(between(line(1), line(-1)), 3),
            AxisPlacement::Definite(0..3)
        );
        assert_eq!(
            resolve(between(line(2), line(-2)), 6),
            AxisPlacement::Definite(1..5)
        );
    }

    #[test]
    fn lines_past_the_explicit_grid_make_implicit_tracks() {
        let placed = place(
            &[(
                AxisPlacement::Auto { span: 1 },
                resolve(between(line(-5), GridLine::Auto), 3),
            )],
            (1, 3),
            ROWS,
        );
        assert_eq!(placed.column_origin, 1);
        assert_eq!(placed.areas[0].columns, 0..1);

        let placed = place(
            &[(
                AxisPlacement::Auto { span: 1 },
                resolve(between(line(5), GridLine::Auto), 3),
            )],
            (1, 3),
            ROWS,
        );
        assert_eq!(placed.columns, 5);
    }

    #[test]
    fn conflicts_resolve_as_the_specification_says() {
        assert_eq!(
            resolve(between(line(3), line(1)), 3),
            AxisPlacement::Definite(0..2)
        );
        assert_eq!(
            resolve(between(span(2), span(3)), 3),
            AxisPlacement::Auto { span: 2 }
        );
        assert_eq!(
            resolve(
                between(
                    GridLine::Span {
                        count: NonZeroU32::MIN,
                        name: Some(name("x")),
                    },
                    GridLine::Auto
                ),
                3
            ),
            AxisPlacement::Auto { span: 1 }
        );
        assert_eq!(
            resolve(GridPlacement::AUTO, 3),
            AxisPlacement::Auto { span: 1 }
        );
    }

    #[test]
    fn a_bare_name_is_an_areas_line_first_and_a_named_line_second() {
        let mut names = LineNames::default();
        names.add(name("content-start"), 1);
        names.add(name("content-end"), 2);
        names.add(name("full"), 0);
        let content = GridPlacement {
            start: GridLine::Area(name("content")),
            end: GridLine::Area(name("content")),
        };
        assert_eq!(
            resolve_axis(&content, &names, 3),
            AxisPlacement::Definite(1..2)
        );
        let full = GridPlacement {
            start: GridLine::Area(name("full")),
            end: GridLine::Auto,
        };
        assert_eq!(
            resolve_axis(&full, &names, 3),
            AxisPlacement::Definite(0..1)
        );
    }

    #[test]
    fn too_few_named_lines_borrow_the_implicit_ones() {
        let mut names = LineNames::default();
        names.add(name("foo"), 1);
        let second = GridPlacement {
            start: GridLine::Line {
                nth: NonZeroI32::new(2).expect("two"),
                name: Some(name("foo")),
            },
            end: GridLine::Auto,
        };
        assert_eq!(
            resolve_axis(&second, &names, 3),
            AxisPlacement::Definite(4..5)
        );
        let spanning = GridPlacement {
            start: line(1),
            end: GridLine::Span {
                count: NonZeroU32::new(2).expect("two"),
                name: Some(name("foo")),
            },
        };
        assert_eq!(
            resolve_axis(&spanning, &names, 3),
            AxisPlacement::Definite(0..4)
        );
    }

    #[test]
    fn a_huge_line_is_clamped_and_costs_nothing() {
        let placed = place(
            &[(
                resolve(between(line(100_000), GridLine::Auto), 1),
                resolve(between(line(100_000), GridLine::Auto), 1),
            )],
            (1, 1),
            ROWS,
        );
        assert_eq!(placed.areas[0].rows.start, GRID_LIMIT as usize);
    }

    #[test]
    fn a_row_span_covers_rows() {
        let placed = place(
            &[
                (
                    resolve(between(span(2), GridLine::Auto), 2),
                    AxisPlacement::Auto { span: 1 },
                ),
                (
                    AxisPlacement::Auto { span: 1 },
                    AxisPlacement::Auto { span: 1 },
                ),
                (
                    AxisPlacement::Auto { span: 1 },
                    AxisPlacement::Auto { span: 1 },
                ),
            ],
            (0, 2),
            ROWS,
        );
        assert_eq!(placed.areas[0].rows, 0..2);
        assert_eq!(
            placed.areas[1],
            Area {
                rows: 0..1,
                columns: 1..2
            }
        );
        assert_eq!(
            placed.areas[2],
            Area {
                rows: 1..2,
                columns: 1..2
            }
        );
    }

    /// bbc: an item placed in a column does not move the cursor for an item
    /// locked to a row, which lands beside it.
    #[test]
    fn a_row_locked_item_lands_beside_a_placed_one() {
        let placed = place(
            &[
                (
                    resolve(between(line(1), span(2)), 2),
                    resolve(between(line(1), GridLine::Auto), 2),
                ),
                (
                    resolve(between(line(1), GridLine::Auto), 2),
                    AxisPlacement::Auto { span: 1 },
                ),
            ],
            (0, 2),
            ROWS,
        );
        assert_eq!(
            placed.areas[1],
            Area {
                rows: 0..1,
                columns: 1..2
            }
        );
    }

    /// Sparse leaves the hole a wide item stepped over; dense fills it.
    #[test]
    fn dense_fills_the_holes_sparse_leaves() {
        let items = [
            (
                AxisPlacement::Auto { span: 1 },
                AxisPlacement::Auto { span: 1 },
            ),
            (
                AxisPlacement::Auto { span: 1 },
                AxisPlacement::Auto { span: 3 },
            ),
            (
                AxisPlacement::Auto { span: 1 },
                AxisPlacement::Auto { span: 1 },
            ),
        ];
        let sparse = place(&items, (0, 3), ROWS);
        assert_eq!(
            sparse.areas[2],
            Area {
                rows: 2..3,
                columns: 0..1
            }
        );
        let dense = place(
            &items,
            (0, 3),
            GridAutoFlow {
                axis: FlowAxis::Row,
                dense: true,
            },
        );
        assert_eq!(
            dense.areas[2],
            Area {
                rows: 0..1,
                columns: 1..2
            }
        );
    }

    /// Column flow fills each column top to bottom.
    #[test]
    fn column_flow_fills_down_first() {
        let items = vec![
            (
                AxisPlacement::Auto { span: 1 },
                AxisPlacement::Auto { span: 1 }
            );
            4
        ];
        let placed = place(
            &items,
            (3, 0),
            GridAutoFlow {
                axis: FlowAxis::Column,
                dense: false,
            },
        );
        assert_eq!(
            placed.areas[2],
            Area {
                rows: 2..3,
                columns: 0..1
            }
        );
        assert_eq!(
            placed.areas[3],
            Area {
                rows: 0..1,
                columns: 1..2
            }
        );
    }
}
