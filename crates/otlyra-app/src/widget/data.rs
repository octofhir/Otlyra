//! Four widgets for showing a lot of rows, and the pane that holds two of them.
//!
//! Written for the inspector and general on purpose. A tree of expandable rows,
//! a table of columns that size to their content, a pair of panes with a divider
//! between them, and text in the monospace family: none of those is a devtools
//! idea, and the settings and the toolbar can have them the day they want them.
//! That is also the argument for the inspector being drawn on this layer at all
//! — the alternative brought a second event model and a second text stack into
//! the window to draw a list of rows.
//!
//! # Rows are data, not children
//!
//! [`Tree`] and [`Table`] hold their rows as values and paint them, rather than
//! building a child widget per row. Two reasons, and the second is the one that
//! matters:
//!
//! - A DOM with ten thousand nodes would be ten thousand boxed widgets measured
//!   and placed every time the tree was rebuilt, to draw the twenty that are on
//!   screen.
//! - Row height is fixed, so *which row is at this point* and *which rows are
//!   visible* are both arithmetic. Nothing is searched and nothing is stored:
//!   the same rule that holds for the rest of the layer — geometry computed
//!   once, read by drawing and hit testing — comes out as one division here.
//!
//! What a row cannot be, then, is arbitrary. A row is an indent, a mark, and
//! some text. That has been enough for every list this interface has needed, and
//! a row that wants a control on it wants a column of [`Stack`]s instead.
//!
//! # And they hold no state
//!
//! Which row is selected, which are expanded, and where a divider sits are all
//! the caller's, exactly as a checkbox's tick is. Each of these reports what was
//! asked for and shows what it is told, so there is never a second copy of the
//! selection to fall behind the first.

use otlyra_gfx::DisplayList;
use otlyra_gfx::peniko::Color;
use otlyra_text::FontStack;

use crate::widget::{Child, Cx, Event, FocusId, Rect, Size, Widget, controls, fill_rounded};

// --- monospace ------------------------------------------------------------

/// One line of text in the monospace family.
///
/// A separate widget rather than an option on `Label`, because the family is
/// carried by the context every child reads from: this swaps it for the length
/// of its own measure and draw and puts it back, so a mono row inside an
/// ordinary column does not leak its family to what comes after it.
pub struct Mono {
    content: String,
    size: Option<f32>,
    color: Color,
    rect: Rect,
}

impl Mono {
    /// Code, drawn in `color` at the theme's monospace size.
    pub fn new(content: impl Into<String>, color: Color) -> Self {
        Self {
            content: content.into(),
            size: None,
            color,
            rect: Rect::ZERO,
        }
    }

    /// Draw at `size` instead of the theme's.
    pub fn size(mut self, size: f32) -> Self {
        self.size = Some(size);
        self
    }

    /// Run `body` with the monospace family in place of the interface's.
    fn with_family<T>(cx: &mut Cx, body: impl FnOnce(&mut Cx) -> T) -> T {
        let interface = cx.fonts.clone();
        cx.fonts = FontStack::parse_css(cx.theme.mono);
        let out = body(cx);
        cx.fonts = interface;
        out
    }
}

impl<A> Widget<A> for Mono {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let size = self.size.unwrap_or(cx.theme.font_size_mono);
        Mono::with_family(cx, |cx| {
            let width = cx.measure_text(&self.content, size);
            Size::new(width.min(available.width), cx.line_height(size))
        })
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let size = self.size.unwrap_or(cx.theme.font_size_mono);
        let (content, color, rect) = (self.content.clone(), self.color, self.rect);
        Mono::with_family(cx, |cx| {
            draw_line(cx, list, &content, size, color, rect);
        });
    }
}

/// Draw one line of text at `rect`'s top-left, cut to its width.
///
/// The shared tail of every row this module paints. It takes the family from the
/// context, so the caller decides whether it is code or interface text by what
/// it has put there.
fn draw_line(
    cx: &mut Cx,
    list: &mut DisplayList,
    content: &str,
    size: f32,
    color: Color,
    rect: Rect,
) {
    if content.is_empty() || rect.width <= 0.0 {
        return;
    }
    // Shaped without a wrap width and cut by the clip the row is drawn inside.
    // Wrapping and then drawing only the first line loses everything after the
    // last break that fits — and a message whose next word is a hundred-character
    // path would show the sentence and drop the path, which is the half a person
    // opened the panel for.
    let shaped = cx.text.shape(content, &cx.fonts, size, None);
    for run in shaped.runs.iter().filter(|run| run.line == 0) {
        list.push_glyphs(
            &run.font,
            run.font_size,
            run.normalized_coords.clone(),
            otlyra_gfx::peniko::Brush::Solid(color),
            otlyra_gfx::kurbo::Affine::translate((rect.x, rect.y)),
            true,
            run.glyphs.clone(),
        );
    }
}

// --- tree -----------------------------------------------------------------

/// One row of a [`Tree`], already flattened by whoever owns the expansion.
///
/// Flattened by the caller rather than by the tree, because what a row's
/// children *are* is the caller's business — a DOM node's children come from the
/// document, and asking the widget to hold them would be asking it to hold a
/// second copy of the document.
#[derive(Clone, Debug)]
pub struct TreeRow {
    /// How deep this sits, `0` at the root.
    pub depth: usize,
    /// What the row says. Drawn in the monospace family.
    pub text: String,
    /// What shade the text is, so a tag and a text node can be told apart.
    pub color: Color,
    /// Whether it has children at all, which is what earns a twisty.
    pub expandable: bool,
    /// Whether those children are showing.
    pub expanded: bool,
}

/// Rows at a fixed height, indented, some of them with a twisty to open.
///
/// The selection and what is expanded belong to the caller. What the tree
/// reports is *this row was chosen* and *this row's twisty was pressed*, by
/// index into the rows it was given.
pub struct Tree<A> {
    rows: Vec<TreeRow>,
    selected: Option<usize>,
    offset: f64,
    overflow: crate::widget::Overflow,
    on_select: Box<dyn Fn(usize) -> A>,
    on_toggle: Box<dyn Fn(usize) -> A>,
    focus: Option<FocusId>,
    rect: Rect,
}

/// How far in one level of depth pushes a row.
const INDENT: f64 = 13.0;
/// The width the twisty is given at the front of a row.
const TWISTY: f64 = 13.0;

/// How far past its last row a list may be scrolled.
///
/// Half a row of air under the end of a list, for two reasons. A pane whose
/// height is not a whole number of rows ends mid-row, and a list that could be
/// scrolled only to `content - height` leaves that last row sliced by the panel's
/// own edge — read as the list having lost a row rather than as it having run
/// out. And a row flush against an edge reads as a row that continues past it,
/// which is the same doubt in a pane that happens to divide evenly.
const TAIL: f64 = 9.0;

/// How far a list of `content` logical pixels can scroll inside `height`.
///
/// One definition for the tree and the table both, so the two cannot come to
/// disagree about where the end of a list is.
fn travel(content: f64, height: f64) -> f64 {
    (content + TAIL - height).max(0.0)
}

impl<A> Tree<A> {
    /// A tree showing `rows`, scrolled down by `offset` logical pixels.
    pub fn new(
        rows: Vec<TreeRow>,
        offset: f64,
        overflow: crate::widget::Overflow,
        on_select: impl Fn(usize) -> A + 'static,
        on_toggle: impl Fn(usize) -> A + 'static,
    ) -> Self {
        Self {
            rows,
            selected: None,
            offset,
            overflow,
            on_select: Box::new(on_select),
            on_toggle: Box::new(on_toggle),
            focus: None,
            rect: Rect::ZERO,
        }
    }

    /// Which row is the chosen one.
    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    /// The name the surface knows this tree by, for keyboard traversal.
    pub fn focus(mut self, id: FocusId) -> Self {
        self.focus = Some(id);
        self
    }

    /// How tall all the rows are together.
    fn content_height(&self, cx: &Cx) -> f64 {
        self.rows.len() as f64 * cx.theme.row_height
    }

    /// Which row a point falls in, if it falls in one.
    ///
    /// Arithmetic rather than a search: rows are a fixed height, so this is one
    /// division and it cannot disagree with where they were drawn.
    fn row_at(&self, y: f64, row_height: f64) -> Option<usize> {
        if !self.rect.contains(self.rect.x, y) || row_height <= 0.0 {
            return None;
        }
        let index = ((y - self.rect.y + self.offset) / row_height) as usize;
        (index < self.rows.len()).then_some(index)
    }

    /// Where a row's rectangle is, given the scroll.
    fn row_rect(&self, index: usize, row_height: f64) -> Rect {
        Rect::new(
            self.rect.x,
            self.rect.y + index as f64 * row_height - self.offset,
            self.rect.width,
            row_height,
        )
    }
}

impl<A> Widget<A> for Tree<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        // As tall as it is given: a tree is a window onto its rows, and how many
        // of them there are is not a reason to be taller than the pane.
        self.overflow
            .set(travel(self.content_height(cx), available.height));
        available
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        // Recomputed against the height actually given, which is the one that
        // counts: a column measures every child against the whole of itself and
        // only then hands out the shares, so the height a list was measured at
        // is bigger than the height it gets and an overflow left over from the
        // measure is an overflow too small to reach the last rows.
        self.overflow
            .set(travel(self.content_height(cx), rect.height));
        self.offset = self.offset.clamp(0.0, self.overflow.get());
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let theme = cx.theme.clone();
        let row_height = theme.row_height;
        if row_height <= 0.0 || self.rect.height <= 0.0 {
            return;
        }

        // Only what is on screen. The first row is a division and the last is
        // one more, so a tree of ten thousand costs the twenty that show.
        let first = (self.offset / row_height).floor().max(0.0) as usize;
        let visible = (self.rect.height / row_height).ceil() as usize + 1;
        let last = (first + visible).min(self.rows.len());

        list.push(otlyra_gfx::DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::default(),
            alpha: 1.0,
            transform: otlyra_gfx::kurbo::Affine::IDENTITY,
            clip: otlyra_gfx::kurbo::Shape::to_path(&self.rect.to_kurbo(), 0.1),
        });

        for index in first..last {
            let rect = self.row_rect(index, row_height);
            let row = self.rows[index].clone();

            if self.selected == Some(index) {
                fill_rounded(list, rect, theme.selection, 0.0);
            } else if cx.hovered(rect) && cx.hovered(self.rect) {
                fill_rounded(list, rect, theme.hover, 0.0);
            }

            let indent = row.depth as f64 * INDENT;
            if row.expandable {
                let mark = Rect::new(
                    rect.x + indent + 2.0,
                    rect.y + (row_height - 9.0) / 2.0,
                    9.0,
                    9.0,
                );
                crate::widget::icon::chevron(
                    list,
                    mark,
                    if row.expanded {
                        crate::widget::icon::Direction::Down
                    } else {
                        crate::widget::icon::Direction::Right
                    },
                    theme.ink_dim,
                );
            }

            let text_x = rect.x + indent + TWISTY;
            let line = cx.line_height(theme.font_size_mono);
            let text_rect = Rect::new(
                text_x,
                rect.y + (row_height - line) / 2.0,
                (rect.x + rect.width - text_x - theme.gap).max(0.0),
                line,
            );
            let size = theme.font_size_mono;
            Mono::with_family(cx, |cx| {
                draw_line(cx, list, &row.text, size, row.color, text_rect);
            });
        }

        list.push(otlyra_gfx::DisplayItem::PopLayer);

        if self.focus.is_some() && cx.focus == self.focus {
            controls::focus_ring(&theme, list, self.rect, 0.0);
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        if *event != Event::PointerPressed || !cx.hovered(self.rect) {
            return None;
        }
        let row_height = cx.theme.row_height;
        let index = self.row_at(cx.pointer.1, row_height)?;
        // The twisty is a target of its own inside the row, and it wins: opening
        // a node is not choosing it, and a press on the arrow that also selected
        // would make browsing a tree change what is being looked at.
        let rect = self.row_rect(index, row_height);
        let indent = self.rows[index].depth as f64 * INDENT;
        let twisty = Rect::new(rect.x + indent, rect.y, TWISTY, row_height);
        if self.rows[index].expandable && twisty.contains(cx.pointer.0, cx.pointer.1) {
            return Some((self.on_toggle)(index));
        }
        Some((self.on_select)(index))
    }

    fn flex(&self) -> f64 {
        1.0
    }
}

// --- table ----------------------------------------------------------------

/// Columns that size to what is in them, under a header.
///
/// The same shape as [`Tree`]: cells are strings, rows are a fixed height, and
/// only what is on screen is drawn.
pub struct Table<A> {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    widths: Vec<f64>,
    offset: f64,
    overflow: crate::widget::Overflow,
    selected: Option<usize>,
    on_select: Option<Box<dyn Fn(usize) -> A>>,
    rect: Rect,
}

impl<A> Table<A> {
    /// A table of `rows` under `header`, scrolled down by `offset`.
    pub fn new(
        header: Vec<String>,
        rows: Vec<Vec<String>>,
        offset: f64,
        overflow: crate::widget::Overflow,
    ) -> Self {
        Self {
            widths: vec![0.0; header.len()],
            header,
            rows,
            offset,
            overflow,
            selected: None,
            on_select: None,
            rect: Rect::ZERO,
        }
    }

    /// Report `on_select` when a row is pressed, and draw `selected` as chosen.
    ///
    /// Optional because most tables here are a list to read rather than a list
    /// to pick from, and a row that highlights under the pointer while nothing
    /// can be done with it is a row that promises something.
    pub fn selectable(
        mut self,
        selected: Option<usize>,
        on_select: impl Fn(usize) -> A + 'static,
    ) -> Self {
        self.selected = selected;
        self.on_select = Some(Box::new(on_select));
        self
    }

    /// Which row a point falls in, if it falls in one. The header is not a row.
    fn row_at(&self, y: f64, row_height: f64) -> Option<usize> {
        let body_top = self.rect.y + row_height;
        if y < body_top || y >= self.rect.y + self.rect.height || row_height <= 0.0 {
            return None;
        }
        let index = ((y - body_top + self.offset) / row_height) as usize;
        (index < self.rows.len()).then_some(index)
    }

    /// How tall the header and all the rows are together.
    fn content_height(&self, cx: &Cx) -> f64 {
        (self.rows.len() + 1) as f64 * cx.theme.row_height
    }

    /// The widest each column has to be to show what is in it.
    ///
    /// Measured with the engine that will draw it, over every row rather than
    /// the visible ones: a column that resized as it was scrolled would move
    /// every other column with it.
    fn measure_columns(&mut self, cx: &mut Cx) {
        let size = cx.theme.font_size_mono;
        let gap = cx.theme.gap * 2.0;
        self.widths = Mono::with_family(cx, |cx| {
            self.header
                .iter()
                .enumerate()
                .map(|(column, title)| {
                    let mut widest = cx.measure_text(title, size);
                    for row in &self.rows {
                        if let Some(cell) = row.get(column) {
                            widest = widest.max(cx.measure_text(cell, size));
                        }
                    }
                    widest + gap
                })
                .collect()
        });
    }
}

impl<A> Widget<A> for Table<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.measure_columns(cx);
        self.overflow
            .set(travel(self.content_height(cx), available.height));
        available
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        if self.widths.len() != self.header.len() {
            self.measure_columns(cx);
        }
        // The same recomputation the tree does, and it was missing here — which
        // is what ate the last rows of every table in the panel. A column
        // measures its children against the whole of itself before it hands out
        // the shares, so the height a table is measured at is taller than the
        // height it is placed at; an overflow kept from the measure is short by
        // exactly the difference, and those rows cannot be scrolled to.
        self.overflow
            .set(travel(self.content_height(cx), rect.height));
        self.offset = self.offset.clamp(0.0, self.overflow.get());
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let theme = cx.theme.clone();
        let row_height = theme.row_height;
        if row_height <= 0.0 || self.rect.height <= 0.0 {
            return;
        }

        let header = Rect::new(self.rect.x, self.rect.y, self.rect.width, row_height);
        fill_rounded(list, header, theme.surface, 0.0);
        // Clipped like the body: a column title is as able to run past the edge
        // as a cell is, and text that escaped its table would land on whatever
        // is beside it.
        list.push(otlyra_gfx::DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::default(),
            alpha: 1.0,
            transform: otlyra_gfx::kurbo::Affine::IDENTITY,
            clip: otlyra_gfx::kurbo::Shape::to_path(&header.to_kurbo(), 0.1),
        });
        draw_cells(cx, list, &self.header, &self.widths, header, theme.ink_dim);
        list.push(otlyra_gfx::DisplayItem::PopLayer);
        controls::hairline(
            &theme,
            list,
            Rect::new(header.x, header.y + row_height - 1.0, header.width, 1.0),
        );

        let body = Rect::new(
            self.rect.x,
            self.rect.y + row_height,
            self.rect.width,
            (self.rect.height - row_height).max(0.0),
        );
        list.push(otlyra_gfx::DisplayItem::PushLayer {
            blend: otlyra_gfx::peniko::BlendMode::default(),
            alpha: 1.0,
            transform: otlyra_gfx::kurbo::Affine::IDENTITY,
            clip: otlyra_gfx::kurbo::Shape::to_path(&body.to_kurbo(), 0.1),
        });

        let first = (self.offset / row_height).floor().max(0.0) as usize;
        let visible = (body.height / row_height).ceil() as usize + 1;
        for index in first..(first + visible).min(self.rows.len()) {
            let rect = Rect::new(
                body.x,
                body.y + index as f64 * row_height - self.offset,
                body.width,
                row_height,
            );
            if self.on_select.is_some() {
                if self.selected == Some(index) {
                    fill_rounded(list, rect, theme.selection, 0.0);
                } else if cx.hovered(rect) && cx.hovered(body) {
                    fill_rounded(list, rect, theme.hover, 0.0);
                }
            }
            draw_cells(cx, list, &self.rows[index], &self.widths, rect, theme.ink);
        }

        list.push(otlyra_gfx::DisplayItem::PopLayer);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        let on_select = self.on_select.as_ref()?;
        if *event != Event::PointerPressed || !cx.hovered(self.rect) {
            return None;
        }
        let index = self.row_at(cx.pointer.1, cx.theme.row_height)?;
        Some(on_select(index))
    }

    fn flex(&self) -> f64 {
        1.0
    }
}

/// One row's cells, laid across `rect` at the measured column widths.
fn draw_cells(
    cx: &mut Cx,
    list: &mut DisplayList,
    cells: &[String],
    widths: &[f64],
    rect: Rect,
    color: Color,
) {
    let (size, line) = (
        cx.theme.font_size_mono,
        cx.line_height(cx.theme.font_size_mono),
    );
    let top = rect.y + (rect.height - line) / 2.0;
    let mut x = rect.x + cx.theme.gap;
    let cells: Vec<String> = cells.to_vec();
    let widths: Vec<f64> = widths.to_vec();
    Mono::with_family(cx, |cx| {
        for (index, cell) in cells.iter().enumerate() {
            let width = widths.get(index).copied().unwrap_or(0.0);
            let room = width.min((rect.x + rect.width - x).max(0.0));
            draw_line(cx, list, cell, size, color, Rect::new(x, top, room, line));
            x += width;
        }
    });
}

// --- split ----------------------------------------------------------------

/// Two panes with a divider between them that can be dragged.
///
/// Where the divider sits is the caller's value, in the same way a slider's is:
/// the split reports where a drag has put it and shows what it is told. A
/// divider that remembered its own position would be a second copy of a number
/// the surface has to save anyway.
pub struct Split<A> {
    first: Child<A>,
    second: Child<A>,
    axis: crate::widget::Axis,
    position: f64,
    on_drag: Box<dyn Fn(f64) -> A>,
    rect: Rect,
    divider: Rect,
}

/// How wide the divider is to the eye.
const DIVIDER: f64 = 1.0;
/// How wide it is to the pointer, which needs more than a hairline to catch.
const DIVIDER_GRAB: f64 = 7.0;
/// The least of the whole either pane may be squeezed to.
const MIN_SHARE: f64 = 0.15;

impl<A> Split<A> {
    /// Panes side by side, `position` of the width going to the first.
    pub fn row(
        position: f64,
        first: Child<A>,
        second: Child<A>,
        on_drag: impl Fn(f64) -> A + 'static,
    ) -> Self {
        Self::new(
            crate::widget::Axis::Horizontal,
            position,
            first,
            second,
            on_drag,
        )
    }

    /// Panes one above the other, `position` of the height going to the first.
    pub fn column(
        position: f64,
        first: Child<A>,
        second: Child<A>,
        on_drag: impl Fn(f64) -> A + 'static,
    ) -> Self {
        Self::new(
            crate::widget::Axis::Vertical,
            position,
            first,
            second,
            on_drag,
        )
    }

    fn new(
        axis: crate::widget::Axis,
        position: f64,
        first: Child<A>,
        second: Child<A>,
        on_drag: impl Fn(f64) -> A + 'static,
    ) -> Self {
        Self {
            first,
            second,
            axis,
            position: position.clamp(MIN_SHARE, 1.0 - MIN_SHARE),
            on_drag: Box::new(on_drag),
            rect: Rect::ZERO,
            divider: Rect::ZERO,
        }
    }

    /// The share of the whole a point is at, clamped so neither pane vanishes.
    fn share_at(&self, x: f64, y: f64) -> f64 {
        let share = match self.axis {
            crate::widget::Axis::Horizontal if self.rect.width > 0.0 => {
                (x - self.rect.x) / self.rect.width
            }
            crate::widget::Axis::Vertical if self.rect.height > 0.0 => {
                (y - self.rect.y) / self.rect.height
            }
            _ => self.position,
        };
        share.clamp(MIN_SHARE, 1.0 - MIN_SHARE)
    }
}

impl<A> Widget<A> for Split<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        // The panes are measured even though the split's own answer does not
        // depend on them: a column places its children at the heights its last
        // measure recorded, so a container that placed without measuring would
        // leave every row of a pane stacked on the one above it.
        let (first, second) = match self.axis {
            crate::widget::Axis::Horizontal => {
                let split = available.width * self.position;
                (
                    Size::new(split, available.height),
                    Size::new(
                        (available.width - split - DIVIDER).max(0.0),
                        available.height,
                    ),
                )
            }
            crate::widget::Axis::Vertical => {
                let split = available.height * self.position;
                (
                    Size::new(available.width, split),
                    Size::new(
                        available.width,
                        (available.height - split - DIVIDER).max(0.0),
                    ),
                )
            }
        };
        self.first.measure(first, cx);
        self.second.measure(second, cx);
        available
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        match self.axis {
            crate::widget::Axis::Horizontal => {
                let split = (rect.width * self.position).round();
                self.divider = Rect::new(rect.x + split, rect.y, DIVIDER, rect.height);
                self.first
                    .place(Rect::new(rect.x, rect.y, split, rect.height), cx);
                self.second.place(
                    Rect::new(
                        rect.x + split + DIVIDER,
                        rect.y,
                        (rect.width - split - DIVIDER).max(0.0),
                        rect.height,
                    ),
                    cx,
                );
            }
            crate::widget::Axis::Vertical => {
                let split = (rect.height * self.position).round();
                self.divider = Rect::new(rect.x, rect.y + split, rect.width, DIVIDER);
                self.first
                    .place(Rect::new(rect.x, rect.y, rect.width, split), cx);
                self.second.place(
                    Rect::new(
                        rect.x,
                        rect.y + split + DIVIDER,
                        rect.width,
                        (rect.height - split - DIVIDER).max(0.0),
                    ),
                    cx,
                );
            }
        }
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.first.draw(cx, list);
        self.second.draw(cx, list);
        let theme = cx.theme.clone();
        // The divider brightens under the pointer, which is the only thing
        // saying it can be taken hold of at all.
        let color = if cx.hovered(grab(self.divider, self.axis)) {
            theme.accent
        } else {
            theme.hairline
        };
        fill_rounded(list, self.divider, color, 0.0);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        let grab = grab(self.divider, self.axis);
        match event {
            // A drag that began on the divider follows the pointer anywhere,
            // the same way a slider's does and for the same reason: where the
            // press started is a fact about this drag, not state to keep.
            Event::PointerPressed if cx.hovered(grab) => {
                Some((self.on_drag)(self.share_at(cx.pointer.0, cx.pointer.1)))
            }
            Event::PointerMoved if cx.pointer_down && cx.dragging_from(grab) => {
                Some((self.on_drag)(self.share_at(cx.pointer.0, cx.pointer.1)))
            }
            _ => self
                .first
                .event(event, cx)
                .or_else(|| self.second.event(event, cx)),
        }
    }

    fn flex(&self) -> f64 {
        1.0
    }
}

/// The divider as the pointer sees it: wider than it is drawn, because a
/// one-pixel target is one nobody can hit.
fn grab(divider: Rect, axis: crate::widget::Axis) -> Rect {
    match axis {
        crate::widget::Axis::Horizontal => Rect::new(
            divider.x - DIVIDER_GRAB / 2.0,
            divider.y,
            DIVIDER_GRAB,
            divider.height,
        ),
        crate::widget::Axis::Vertical => Rect::new(
            divider.x,
            divider.y - DIVIDER_GRAB / 2.0,
            divider.width,
            DIVIDER_GRAB,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::Overflow;
    use otlyra_text::TextEngine;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Act {
        Select(usize),
        Toggle(usize),
        Drag(i64),
    }

    fn rows(count: usize) -> Vec<TreeRow> {
        (0..count)
            .map(|index| TreeRow {
                depth: index % 3,
                text: format!("row {index}"),
                color: Color::BLACK,
                expandable: index % 2 == 0,
                expanded: false,
            })
            .collect()
    }

    fn tree(count: usize, offset: f64) -> Tree<Act> {
        Tree::new(
            rows(count),
            offset,
            Overflow::default(),
            Act::Select,
            Act::Toggle,
        )
    }

    #[test]
    fn a_press_lands_on_the_row_it_is_drawn_over() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let height = cx.theme.row_height;
        let mut tree = tree(100, 0.0);
        Widget::<Act>::place(&mut tree, Rect::new(0.0, 0.0, 200.0, 100.0), &mut cx);

        // Well inside the fourth row, and past the twisty at its front.
        cx.pointer = (150.0, height * 3.0 + height / 2.0);
        assert_eq!(
            tree.event(&Event::PointerPressed, &mut cx),
            Some(Act::Select(3))
        );
    }

    #[test]
    fn scrolling_moves_which_row_a_point_is_in() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let height = cx.theme.row_height;
        // Ten rows down, so what was the eleventh row is now the first.
        let mut tree = tree(100, height * 10.0);
        Widget::<Act>::place(&mut tree, Rect::new(0.0, 0.0, 200.0, 100.0), &mut cx);

        cx.pointer = (150.0, height / 2.0);
        assert_eq!(
            tree.event(&Event::PointerPressed, &mut cx),
            Some(Act::Select(10))
        );
    }

    #[test]
    fn the_twisty_opens_a_row_rather_than_choosing_it() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let height = cx.theme.row_height;
        let mut tree = tree(10, 0.0);
        Widget::<Act>::place(&mut tree, Rect::new(0.0, 0.0, 200.0, 100.0), &mut cx);

        // Row 0 is expandable and sits at depth 0, so its twisty is at the left.
        cx.pointer = (TWISTY / 2.0, height / 2.0);
        assert_eq!(
            tree.event(&Event::PointerPressed, &mut cx),
            Some(Act::Toggle(0))
        );
    }

    #[test]
    fn a_tree_reports_how_far_it_could_scroll() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let overflow = Overflow::default();
        let mut tree = Tree::new(
            rows(100),
            0.0,
            std::rc::Rc::clone(&overflow),
            Act::Select,
            Act::Toggle,
        );
        Widget::<Act>::measure(&mut tree, Size::new(200.0, 100.0), &mut cx);
        Widget::<Act>::place(&mut tree, Rect::new(0.0, 0.0, 200.0, 100.0), &mut cx);

        assert_eq!(overflow.get(), 100.0 * cx.theme.row_height + TAIL - 100.0);
    }

    /// The bug that ate the bottom of every table in the inspector: a column
    /// measures each child against the whole of itself and only then hands out
    /// the shares, so the height a list is *measured* at is taller than the one
    /// it is *placed* at. A list that kept the measure's answer could not be
    /// scrolled to its last rows.
    #[test]
    fn a_list_can_be_scrolled_to_its_last_row_however_it_was_measured() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let row = cx.theme.row_height;

        let reachable = |overflow: &Overflow, content: f64, height: f64| {
            // The furthest the list can go has to put the last row's bottom
            // edge inside the box it is drawn in.
            content - overflow.get() <= height
        };

        // Measured against 400 — the whole panel — and placed into 100.
        let overflow = Overflow::default();
        let mut table = Table::new(
            vec!["a".to_owned()],
            (0..40).map(|n| vec![n.to_string()]).collect(),
            0.0,
            std::rc::Rc::clone(&overflow),
        );
        Widget::<Act>::measure(&mut table, Size::new(200.0, 400.0), &mut cx);
        Widget::<Act>::place(&mut table, Rect::new(0.0, 0.0, 200.0, 100.0), &mut cx);
        // Forty rows and a header.
        assert!(
            reachable(&overflow, 41.0 * row, 100.0),
            "the last row is out of reach: {} of {}",
            overflow.get(),
            41.0 * row - 100.0
        );

        let overflow = Overflow::default();
        let mut tree = Tree::new(
            rows(40),
            0.0,
            std::rc::Rc::clone(&overflow),
            Act::Select,
            Act::Toggle,
        );
        Widget::<Act>::measure(&mut tree, Size::new(200.0, 400.0), &mut cx);
        Widget::<Act>::place(&mut tree, Rect::new(0.0, 0.0, 200.0, 100.0), &mut cx);
        assert!(reachable(&overflow, 40.0 * row, 100.0));
    }

    /// A list that fits needs no travel at all, tail or no tail.
    #[test]
    fn a_list_that_fits_does_not_scroll() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let overflow = Overflow::default();
        let mut table = Table::new(
            vec!["a".to_owned()],
            vec![vec!["one".to_owned()], vec!["two".to_owned()]],
            0.0,
            std::rc::Rc::clone(&overflow),
        );
        Widget::<Act>::measure(&mut table, Size::new(200.0, 400.0), &mut cx);
        Widget::<Act>::place(&mut table, Rect::new(0.0, 0.0, 200.0, 400.0), &mut cx);
        assert_eq!(overflow.get(), 0.0);
    }

    #[test]
    fn a_press_below_the_last_row_chooses_nothing() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let mut tree = tree(2, 0.0);
        Widget::<Act>::place(&mut tree, Rect::new(0.0, 0.0, 200.0, 200.0), &mut cx);

        cx.pointer = (100.0, 190.0);
        assert_eq!(tree.event(&Event::PointerPressed, &mut cx), None);
    }

    #[test]
    fn a_column_of_panes_reports_where_a_drag_puts_the_divider() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let mut split: Split<Act> = Split::column(
            0.5,
            Box::new(crate::widget::Gap::new(0.0, 0.0)),
            Box::new(crate::widget::Gap::new(0.0, 0.0)),
            |share| Act::Drag((share * 100.0).round() as i64),
        );
        split.place(Rect::new(0.0, 0.0, 200.0, 200.0), &mut cx);

        // On the divider, which sits halfway down.
        cx.pointer = (100.0, 100.0);
        assert_eq!(
            split.event(&Event::PointerPressed, &mut cx),
            Some(Act::Drag(50))
        );

        // Dragged past the top edge, and pinned rather than let go of: a pane
        // that could be squeezed to nothing is a pane that cannot be brought
        // back.
        cx.pointer_down = true;
        cx.press_origin = Some((100.0, 100.0));
        cx.pointer = (100.0, -500.0);
        assert_eq!(
            split.event(&Event::PointerMoved, &mut cx),
            Some(Act::Drag(15))
        );
    }

    #[test]
    fn a_table_sizes_a_column_to_the_widest_thing_in_it() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let mut table = Table::new(
            vec!["name".to_owned(), "value".to_owned()],
            vec![
                vec!["id".to_owned(), "x".to_owned()],
                vec!["a-much-longer-name".to_owned(), "y".to_owned()],
            ],
            0.0,
            Overflow::default(),
        );
        Widget::<Act>::measure(&mut table, Size::new(400.0, 100.0), &mut cx);

        assert!(
            table.widths[0] > table.widths[1],
            "the column with the long name is the wider one: {:?}",
            table.widths
        );
    }
}
