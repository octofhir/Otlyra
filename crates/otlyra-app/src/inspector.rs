//! The inspector: what the engine actually built, in the window, while it runs.
//!
//! Everything else in the interface can be checked by looking at it. The engine
//! cannot. Until now the only way to ask what the DOM or the box tree came out
//! as was a `--dump-*` flag on a fresh process, which answers *what does this
//! document parse to* and never *why is this element here, on this page, right
//! now*. This is that second question.
//!
//! # Why it is drawn on our own layer
//!
//! The research plan once admitted `egui` behind a feature flag. That was the
//! right call when the browser had no widget layer and is not now. A toolkit
//! here would bring a second event model, a second text stack, a second theme
//! and a second accessibility tree into the same window in order to draw panels,
//! scrolling lists and rows [`crate::widget`] already draws. What devtools
//! needed beyond what we had was four widgets — a tree, a splitter, a table and
//! monospace text — and every one of them is general: the settings and the
//! toolbar can use them the day they want them. That is less work than the
//! integration, and it leaves one stack instead of two.
//!
//! # It reads and never writes
//!
//! The document is borrowed to be walked and nothing here can change it.
//! Editing a style or an attribute from the panel needs mutation to run through
//! `DocumentMutator` *and* an invalidation path that does not exist yet, so the
//! panel that would pretend to offer it does not.

use std::collections::HashSet;

use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::DisplayList;
use otlyra_gfx::peniko::Color;
use otlyra_platform::{Key, Modifiers};
use otlyra_text::TextEngine;

use crate::widget::controls::{self, Emphasis};
use crate::widget::data::{Mono, Split, Table, Tree, TreeRow};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Child, Cx, Event, Fixed, Flex, Focus, Gap, Insets, Label, Overflow, Padding,
    Rect, Size, Stack, fill_rounded,
};

/// What the inspector reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Nothing.
    None,
    /// Choose the node on this row.
    Select(usize),
    /// Open or close the node on this row.
    Toggle(usize),
    /// Put the divider between the panes here, as a share of the width.
    SplitAt(f64),
    /// Make the panel this tall, as a share of the content area.
    HeightAt(f64),
    /// Start or stop picking an element with the pointer.
    TogglePicker,
    /// Put the panel away.
    Close,
}

/// The least and most of the content area the panel may take.
const MIN_HEIGHT: f64 = 0.15;
/// The most of it, so the page never disappears behind its own inspector.
const MAX_HEIGHT: f64 = 0.8;
/// How tall the panel's own header is.
const HEADER: f64 = 30.0;
/// How much of the panel's width the tree gets before anybody drags it.
const DEFAULT_SPLIT: f64 = 0.55;
/// How much of the content area the panel takes before anybody drags it.
const DEFAULT_HEIGHT: f64 = 0.38;
/// The longest a text node is shown before it is cut.
const TEXT_LIMIT: usize = 60;

/// One row of the tree, and which node it came from.
///
/// The mapping is kept beside the rows rather than inside them because the tree
/// widget is general and has never heard of a DOM: what it reports is a row
/// index, and this is what turns that back into a node.
#[derive(Clone, Debug)]
struct Row {
    node: NodeId,
    row: TreeRow,
}

/// Everything the panel's appearance is a function of.
#[derive(Clone, PartialEq)]
struct Appearance {
    rect: Rect,
    nodes: usize,
    selected: Option<NodeId>,
    expanded: usize,
    split: f64,
    scroll: f64,
    picking: bool,
    pointer: (f64, f64),
    pointer_down: bool,
    focus: Option<crate::widget::FocusId>,
}

/// The panel, and what it is showing.
pub struct Inspector {
    /// Whether the panel is on screen at all.
    pub open: bool,
    /// Whether the next press on the page chooses an element instead.
    pub picking: bool,
    /// The chosen node.
    pub selected: Option<NodeId>,
    /// How much of the content area the panel takes.
    pub height: f64,
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    expanded: HashSet<NodeId>,
    split: f64,
    scroll: f64,
    overflow: Overflow,
    rows: Vec<Row>,
    focus: Focus,
    focused: Option<crate::widget::FocusId>,
    pointer: (f64, f64),
    pointer_down: bool,
    press_origin: Option<(f64, f64)>,
    engine: TextEngine,
    cache: Option<(Appearance, DisplayList)>,
    builds: u64,
    root: Option<Child<Action>>,
}

impl Default for Inspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Inspector {
    /// A panel that is not showing.
    pub fn new() -> Self {
        Self {
            open: false,
            picking: false,
            selected: None,
            height: DEFAULT_HEIGHT,
            theme: Theme::light(),
            expanded: HashSet::new(),
            split: DEFAULT_SPLIT,
            scroll: 0.0,
            overflow: Overflow::default(),
            rows: Vec::new(),
            focus: Focus::default(),
            focused: None,
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            engine: TextEngine::new(),
            cache: None,
            builds: 0,
            root: None,
        }
    }

    /// How many display lists this panel has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// How tall the panel is in a content area of `height` logical pixels.
    ///
    /// Zero when it is closed, which is what makes the dock a subtraction the
    /// page can be laid out against rather than something drawn over it.
    pub fn dock_height(&self, height: f64) -> f64 {
        if !self.open {
            return 0.0;
        }
        (height * self.height.clamp(MIN_HEIGHT, MAX_HEIGHT)).round()
    }

    /// Show the panel, or put it away.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.picking = false;
        }
    }

    /// Apply what the panel reported. The only thing that changes its state.
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Select(index) => {
                self.selected = self.rows.get(index).map(|row| row.node);
            }
            Action::Toggle(index) => {
                if let Some(row) = self.rows.get(index) {
                    let node = row.node;
                    if !self.expanded.remove(&node) {
                        self.expanded.insert(node);
                    }
                }
            }
            Action::SplitAt(share) => self.split = share,
            Action::HeightAt(share) => {
                self.height = share.clamp(MIN_HEIGHT, MAX_HEIGHT);
            }
            Action::TogglePicker => self.picking = !self.picking,
            Action::Close => {
                self.open = false;
                self.picking = false;
            }
        }
    }

    /// Choose `node`, opening whatever was hiding it.
    ///
    /// What the picker reports, and what a selection from anywhere else will
    /// report: a node revealed in a collapsed branch has to open its ancestors,
    /// or the tree scrolls to a row that is not there.
    pub fn reveal(&mut self, document: &Document, node: NodeId) {
        self.selected = Some(node);
        let mut current = document.get(node).and_then(|node| node.parent);
        while let Some(id) = current {
            self.expanded.insert(id);
            current = document.get(id).and_then(|node| node.parent);
        }
        self.scroll_to_selection(document);
    }

    /// Put the chosen row on screen, if it has fallen off it.
    fn scroll_to_selection(&mut self, document: &Document) {
        let rows = self.flatten(document);
        let Some(index) = rows.iter().position(|row| Some(row.node) == self.selected) else {
            return;
        };
        let row_height = self.theme.row_height;
        let top = index as f64 * row_height;
        let window = (self.overflow.get() + row_height).max(row_height);
        let _ = window;
        if top < self.scroll {
            self.scroll = top;
        } else {
            // The panel's own height is not known here, so the row is brought to
            // the top rather than to the nearest edge. A row that is already
            // showing is left where it is by the test above.
            let visible = self.rows.len() as f64 * row_height - self.overflow.get();
            if top > self.scroll + visible - row_height {
                self.scroll = (top - visible + row_height).max(0.0);
            }
        }
    }

    /// Note where the pointer is.
    pub fn pointer_moved(&mut self, x: f64, y: f64) -> Action {
        self.pointer = (x, y);
        if !self.pointer_down {
            return Action::None;
        }
        self.deliver(&Event::PointerMoved)
    }

    /// Press at the last reported position.
    pub fn pointer_pressed(&mut self) -> Action {
        self.pointer_down = true;
        self.press_origin = Some(self.pointer);
        self.deliver(&Event::PointerPressed)
    }

    /// Let go.
    pub fn pointer_released(&mut self) {
        self.pointer_down = false;
        self.press_origin = None;
    }

    /// Scroll the tree.
    pub fn scroll_by(&mut self, delta: f64) {
        self.scroll = (self.scroll + delta).clamp(0.0, self.overflow.get());
    }

    /// Whether the panel owns the point — it is inside the dock.
    pub fn owns(&self, y: f64, dock_top: f64) -> bool {
        self.open && y >= dock_top
    }

    /// What a press at `x`, `y` would report, without reporting it.
    pub fn action_at(&mut self, x: f64, y: f64) -> Action {
        let (pointer, down) = (self.pointer, self.pointer_down);
        self.pointer = (x, y);
        self.pointer_down = false;
        let action = self.offer(&Event::PointerPressed);
        self.pointer = pointer;
        self.pointer_down = down;
        action
    }

    /// Handle a key. `None` means the key was never the panel's.
    pub fn key_pressed(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        document: &Document,
    ) -> Option<Action> {
        if modifiers.is_accelerator() || !self.open {
            return None;
        }
        let rows = self.flatten(document);
        let at = rows.iter().position(|row| Some(row.node) == self.selected);

        match key {
            Key::Escape => {
                // The picker first, then the panel: Escape undoes the most
                // recent thing, and starting to pick is the most recent thing.
                if self.picking {
                    self.picking = false;
                } else {
                    self.open = false;
                }
                Some(Action::None)
            }
            Key::Down | Key::Up => {
                let next = match (at, key == Key::Down) {
                    (Some(at), true) => (at + 1).min(rows.len().saturating_sub(1)),
                    (Some(at), false) => at.saturating_sub(1),
                    (None, _) => 0,
                };
                self.selected = rows.get(next).map(|row| row.node);
                self.scroll_to_selection(document);
                Some(Action::None)
            }
            // Right opens a closed node and steps into an open one; Left closes
            // an open node and steps out of a closed one. Which is what every
            // tree does, and the reason neither needs a second key.
            Key::Right => {
                let row = rows.get(at?)?;
                if row.row.expandable && !row.row.expanded {
                    self.expanded.insert(row.node);
                } else {
                    self.selected = rows.get(at? + 1).map(|row| row.node);
                }
                Some(Action::None)
            }
            Key::Left => {
                let row = rows.get(at?)?;
                if row.row.expanded {
                    self.expanded.remove(&row.node);
                } else if let Some(parent) = document.get(row.node).and_then(|node| node.parent) {
                    self.selected = Some(parent);
                }
                Some(Action::None)
            }
            _ => None,
        }
    }

    /// Offer an event to the last frame's tree and apply what comes back.
    fn deliver(&mut self, event: &Event) -> Action {
        let action = self.offer(event);
        self.apply(action.clone());
        action
    }

    /// Offer an event to the last frame's tree, changing nothing.
    fn offer(&mut self, event: &Event) -> Action {
        let Some(root) = self.root.as_mut() else {
            return Action::None;
        };
        let mut cx = Cx::new(&mut self.engine);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.focus = self.focused;
        cx.theme = self.theme.clone();
        root.event(event, &mut cx).unwrap_or(Action::None)
    }

    /// The document as rows, in the order they are drawn.
    ///
    /// Rebuilt from the document rather than kept: the tree the panel shows is a
    /// view of what the engine holds, and a stored copy would be a second answer
    /// to what the page is.
    fn flatten(&self, document: &Document) -> Vec<Row> {
        let mut rows = Vec::new();
        self.walk(document, document.root(), 0, &mut rows);
        rows
    }

    fn walk(&self, document: &Document, node: NodeId, depth: usize, out: &mut Vec<Row>) {
        let Some(data) = document.get(node) else {
            return;
        };
        let children: Vec<NodeId> = document.children(node).collect();
        let shown: Vec<NodeId> = children
            .iter()
            .copied()
            .filter(|child| !is_ignorable(document, *child))
            .collect();

        let expandable = !shown.is_empty();
        let expanded = expandable && self.expanded.contains(&node);
        let (text, color) = self.label(document, node, &data.data, expanded);
        out.push(Row {
            node,
            row: TreeRow {
                depth,
                text,
                color,
                expandable,
                expanded,
            },
        });

        if !expanded {
            return;
        }
        for child in shown {
            self.walk(document, child, depth + 1, out);
        }
    }

    /// What one node's row says, and what shade it is drawn in.
    fn label(
        &self,
        document: &Document,
        node: NodeId,
        data: &NodeData,
        expanded: bool,
    ) -> (String, Color) {
        let theme = &self.theme;
        match data {
            NodeData::Document => ("#document".to_owned(), theme.ink_dim),
            NodeData::Doctype { name, .. } => (format!("<!DOCTYPE {name}>"), theme.ink_dim),
            NodeData::Comment(text) => (
                format!("<!-- {} -->", cut(text.trim(), TEXT_LIMIT)),
                theme.ink_dim,
            ),
            NodeData::Text(text) => (format!("\"{}\"", cut(text.trim(), TEXT_LIMIT)), theme.ink),
            NodeData::Element(element) => {
                let tag = element.name.local.as_ref();
                let mut label = format!("<{tag}");
                if let Some(id) = element.id() {
                    label.push_str(&format!(" #{id}"));
                }
                let classes: Vec<&str> = element.classes().collect();
                if !classes.is_empty() {
                    label.push_str(&format!(" .{}", classes.join(".")));
                }
                label.push('>');
                // A closed element with nothing but text in it shows that text
                // on its own row, because opening it to read three words is a
                // click for no information.
                if !expanded {
                    let children: Vec<NodeId> = document.children(node).collect();
                    if let [only] = children.as_slice()
                        && let Some(NodeData::Text(text)) = document.get(*only).map(|n| &n.data)
                    {
                        label.push_str(&format!(" {}", cut(text.trim(), 32)));
                    }
                }
                (label, theme.code_tag)
            }
        }
    }

    /// The rows the panel is currently showing, for whoever asks what is on it.
    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    /// Paint the panel into `rect`, in window coordinates.
    pub fn build_display_list(
        &mut self,
        rect: Rect,
        document: Option<&Document>,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        let rows = document
            .map(|document| self.flatten(document))
            .unwrap_or_default();
        let appearance = Appearance {
            rect,
            nodes: rows.len(),
            selected: self.selected,
            expanded: self.expanded.len(),
            split: self.split,
            scroll: self.scroll,
            picking: self.picking,
            pointer: self.pointer,
            pointer_down: self.pointer_down,
            focus: self.focused,
        };
        if let Some((built, list)) = &self.cache
            && *built == appearance
            && self.root.is_some()
        {
            self.rows = rows;
            out.append(list);
            return;
        }

        self.builds += 1;
        self.rows = rows;
        let mut built = DisplayList::new();
        let list = &mut built;
        let theme = self.theme.clone();
        fill_rounded(list, rect, theme.raised, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.focus = self.focused;
        cx.theme = theme.clone();

        self.focus.begin();
        let mut root = self.build(&theme, document);
        root.measure(Size::new(rect.width, rect.height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, list);

        // The edge the panel is dragged by, and the line that says the page
        // stops here.
        controls::hairline(&theme, list, Rect::new(rect.x, rect.y, rect.width, 1.0));

        self.root = Some(root);
        self.cache = Some((appearance, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    fn build(&self, theme: &Theme, document: Option<&Document>) -> Child<Action> {
        let tree: Child<Action> = match document {
            Some(_) => Box::new(
                Tree::new(
                    self.rows.iter().map(|row| row.row.clone()).collect(),
                    self.scroll,
                    std::rc::Rc::clone(&self.overflow),
                    Action::Select,
                    Action::Toggle,
                )
                .selected(
                    self.rows
                        .iter()
                        .position(|row| Some(row.node) == self.selected),
                ),
            ),
            None => Box::new(Align::centre(Box::new(Label::new(
                "Nothing is loaded in this tab.",
                theme.font_size,
                theme.ink_dim,
            )))),
        };

        let panes: Child<Action> = Box::new(Split::row(
            self.split,
            Box::new(Padding::new(Insets::all(theme.gap * 0.5), tree)),
            self.details(theme, document),
            Action::SplitAt,
        ));

        Box::new(Stack::column(
            0.0,
            vec![self.header(theme), Box::new(Flex::new(1.0, panes))],
        ))
    }

    /// The bar across the panel: what it is, the picker, and the way out.
    fn header(&self, theme: &Theme) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "Elements",
            theme.font_size_small,
            theme.ink_dim,
        ))));

        // The picker stays lit while it is armed, because a mode with no sign
        // that it is on is a mode that surprises the next click.
        let picker = controls::button(
            theme,
            &self.focus,
            Action::TogglePicker,
            "Pick",
            if self.picking {
                Emphasis::Primary
            } else {
                Emphasis::Quiet
            },
            true,
        );

        Box::new(Fixed::height(
            HEADER,
            Box::new(Background::new(
                theme.surface,
                0.0,
                Box::new(Padding::new(
                    Insets::symmetric(theme.gap, 0.0),
                    Box::new(Stack::row(
                        theme.gap,
                        vec![
                            Box::new(Flex::new(1.0, title)),
                            Box::new(Align::centre(picker)),
                            Box::new(Align::centre(controls::icon_button(
                                theme,
                                &self.focus,
                                Action::Close,
                                true,
                                crate::widget::icon::cross,
                            ))),
                        ],
                    )),
                )),
            )),
        ))
    }

    /// The right-hand pane: what the chosen node is, in numbers and attributes.
    fn details(&self, theme: &Theme, document: Option<&Document>) -> Child<Action> {
        let Some(document) = document else {
            return Box::new(Gap::new(0.0, 0.0));
        };
        let Some(selected) = self.selected.and_then(|node| document.get(node)) else {
            return Box::new(Align::centre(Box::new(Label::new(
                "Choose an element.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        };

        let mut rows: Vec<Child<Action>> = Vec::new();
        match &selected.data {
            NodeData::Element(element) => {
                rows.push(Box::new(Mono::new(
                    format!("<{}>", element.name.local.as_ref()),
                    theme.code_tag,
                )));
                let attributes: Vec<Vec<String>> = element
                    .attrs
                    .iter()
                    .map(|attr| vec![attr.name.local.as_ref().to_owned(), attr.value.to_string()])
                    .collect();
                if attributes.is_empty() {
                    rows.push(Box::new(Label::new(
                        "No attributes.",
                        theme.font_size_small,
                        theme.ink_dim,
                    )));
                } else {
                    rows.push(Box::new(Flex::new(
                        1.0,
                        Box::new(Table::new(
                            vec!["attribute".to_owned(), "value".to_owned()],
                            attributes,
                            0.0,
                            Overflow::default(),
                        )),
                    )));
                }
            }
            NodeData::Text(text) => {
                rows.push(Box::new(Label::new(
                    "Text node",
                    theme.font_size_small,
                    theme.ink_dim,
                )));
                rows.push(Box::new(Mono::new(cut(text.trim(), 200), theme.code_value)));
            }
            other => rows.push(Box::new(Mono::new(
                match other {
                    NodeData::Document => "#document".to_owned(),
                    NodeData::Doctype { name, .. } => format!("<!DOCTYPE {name}>"),
                    NodeData::Comment(text) => format!("<!-- {} -->", cut(text.trim(), 200)),
                    NodeData::Element(_) | NodeData::Text(_) => String::new(),
                },
                theme.ink_dim,
            ))),
        }

        Box::new(Padding::new(
            Insets::all(theme.gap),
            Box::new(Stack::column(theme.gap, rows)),
        ))
    }
}

/// Whether a node is one the tree does not bother showing.
///
/// Whitespace between tags is a node the parser is right to keep and a row
/// nobody wants to read past.
fn is_ignorable(document: &Document, node: NodeId) -> bool {
    document
        .get(node)
        .is_some_and(|node| matches!(&node.data, NodeData::Text(text) if text.trim().is_empty()))
}

/// `text` cut to `limit` characters, with an ellipsis if anything was dropped.
fn cut(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().nth(limit).is_some() {
        out.push(controls::ELLIPSIS);
    }
    out
}

/// Paint the four shades of a box over the page.
///
/// The rectangle is the one the last frame drew, in window coordinates, so the
/// overlay lands exactly where the box did — asked of the same targets a click
/// is tested against rather than worked out a second time from the fragments.
pub fn paint_highlight(list: &mut DisplayList, theme: &Theme, border: Rect, style: &BoxEdges) {
    let margin = Rect::new(
        border.x - style.margin.0,
        border.y - style.margin.1,
        border.width + style.margin.0 + style.margin.2,
        border.height + style.margin.1 + style.margin.3,
    );
    let padding = Rect::new(
        border.x + style.border.0,
        border.y + style.border.1,
        (border.width - style.border.0 - style.border.2).max(0.0),
        (border.height - style.border.1 - style.border.3).max(0.0),
    );
    let content = Rect::new(
        padding.x + style.padding.0,
        padding.y + style.padding.1,
        (padding.width - style.padding.0 - style.padding.2).max(0.0),
        (padding.height - style.padding.1 - style.padding.3).max(0.0),
    );

    // Outermost first, each over the last: the shades are the ones every
    // inspector has used for twenty years, and stacking them is what makes each
    // ring read as the space between two edges.
    fill_rounded(list, margin, theme.box_margin, 0.0);
    fill_rounded(list, border, theme.box_border, 0.0);
    fill_rounded(list, padding, theme.box_padding, 0.0);
    fill_rounded(list, content, theme.box_content, 0.0);
}

/// The four edges of a box, in logical pixels, left-top-right-bottom.
///
/// Taken from the computed style rather than measured, because the fragment
/// carries one rectangle and the rings are the difference between four.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct BoxEdges {
    /// Margin, left-top-right-bottom.
    pub margin: (f64, f64, f64, f64),
    /// Border widths.
    pub border: (f64, f64, f64, f64),
    /// Padding.
    pub padding: (f64, f64, f64, f64),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document every test here inspects.
    fn document() -> Document {
        otlyra_html::parse(
            b"<html><head><title>t</title></head>\
             <body><div id=\"one\" class=\"a b\"><p>hello</p></div>\
             <!-- a note --><span>x</span></body></html>",
            Some("utf-8"),
        )
        .document
    }

    fn panel() -> Inspector {
        let mut inspector = Inspector::new();
        inspector.open = true;
        inspector
    }

    /// Draw one frame, which is what gives the panel geometry to be pressed
    /// against.
    fn frame(inspector: &mut Inspector, document: &Document) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        inspector.build_display_list(
            Rect::new(0.0, 300.0, 900.0, 300.0),
            Some(document),
            &mut text,
            &mut list,
        );
    }

    #[test]
    fn a_closed_panel_takes_no_room_from_the_page() {
        let mut inspector = Inspector::new();
        assert_eq!(inspector.dock_height(600.0), 0.0);
        inspector.toggle();
        assert!(inspector.dock_height(600.0) > 0.0);
        assert!(
            inspector.dock_height(600.0) < 600.0,
            "the page never disappears behind its own inspector"
        );
    }

    #[test]
    fn the_tree_shows_the_document_it_is_given() {
        let document = document();
        let inspector = panel();
        let rows = inspector.flatten(&document);

        // Closed at the root, so one row and a twisty on it.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row.text, "#document");
        assert!(rows[0].row.expandable);
    }

    #[test]
    fn opening_a_node_shows_the_children_the_engine_built() {
        let document = document();
        let mut inspector = panel();

        // Open the root, then the html element below it.
        inspector.expanded.insert(document.root());
        let rows = inspector.flatten(&document);
        let html = rows
            .iter()
            .find(|row| row.row.text.starts_with("<html"))
            .expect("the parser built an html element");
        inspector.expanded.insert(html.node);

        let rows = inspector.flatten(&document);
        let text: Vec<&str> = rows.iter().map(|row| row.row.text.as_str()).collect();
        assert!(text.iter().any(|row| row.starts_with("<head")));
        assert!(text.iter().any(|row| row.starts_with("<body")));
    }

    #[test]
    fn a_row_says_which_element_it_is_and_what_names_it_carries() {
        let document = document();
        let mut inspector = panel();
        for node in every_node(&document) {
            inspector.expanded.insert(node);
        }
        let rows = inspector.flatten(&document);
        let div = rows
            .iter()
            .find(|row| row.row.text.starts_with("<div"))
            .expect("the document has a div");
        assert_eq!(div.row.text, "<div #one .a.b>");
    }

    #[test]
    fn whitespace_between_tags_is_not_a_row() {
        let document =
            otlyra_html::parse(b"<body>\n  <p>text</p>\n</body>", Some("utf-8")).document;
        let mut inspector = panel();
        for node in every_node(&document) {
            inspector.expanded.insert(node);
        }
        let rows = inspector.flatten(&document);
        assert!(
            !rows.iter().any(|row| row.row.text.trim() == "\"\""),
            "a row of nothing is a row nobody reads: {:?}",
            rows.iter().map(|row| &row.row.text).collect::<Vec<_>>()
        );
    }

    /// Every node in the document, so a test can open all of them.
    fn every_node(document: &Document) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            out.push(node);
            stack.extend(document.children(node));
        }
        out
    }

    #[test]
    fn choosing_a_node_opens_whatever_was_hiding_it() {
        let document = document();
        let mut inspector = panel();
        let deep = every_node(&document)
            .into_iter()
            .find(|node| {
                matches!(document.get(*node).map(|n| &n.data),
                    Some(NodeData::Element(element)) if element.name.local.as_ref() == "p")
            })
            .expect("the document has a p");

        inspector.reveal(&document, deep);
        let rows = inspector.flatten(&document);
        assert!(
            rows.iter().any(|row| row.node == deep),
            "the chosen node is a row that can be seen"
        );
        assert_eq!(inspector.selected, Some(deep));
    }

    #[test]
    fn a_press_chooses_the_row_it_is_drawn_over() {
        let document = document();
        let mut inspector = panel();
        inspector.expanded.insert(document.root());
        frame(&mut inspector, &document);

        // The second row, below the panel's own header.
        let y = 300.0 + HEADER + inspector.theme.row_height * 1.5 + 3.0;
        inspector.pointer_moved(200.0, y);
        let action = inspector.pointer_pressed();
        assert!(
            matches!(action, Action::Select(_)),
            "a press in the tree chooses something: {action:?}"
        );
        assert!(inspector.selected.is_some());
    }

    #[test]
    fn the_arrows_walk_the_tree_and_open_what_they_meet() {
        let document = document();
        let mut inspector = panel();
        frame(&mut inspector, &document);

        // Down from nothing lands on the first row, which is the document.
        inspector.key_pressed(Key::Down, Modifiers::default(), &document);
        assert_eq!(inspector.selected, Some(document.root()));

        // Right opens it rather than moving, and only then steps in.
        inspector.key_pressed(Key::Right, Modifiers::default(), &document);
        assert!(inspector.expanded.contains(&document.root()));
        inspector.key_pressed(Key::Right, Modifiers::default(), &document);
        assert_ne!(inspector.selected, Some(document.root()));

        // Left steps back out to the parent.
        inspector.key_pressed(Key::Left, Modifiers::default(), &document);
        assert_eq!(inspector.selected, Some(document.root()));
    }

    #[test]
    fn escape_puts_the_picker_away_before_the_panel() {
        let document = document();
        let mut inspector = panel();
        inspector.picking = true;

        inspector.key_pressed(Key::Escape, Modifiers::default(), &document);
        assert!(!inspector.picking);
        assert!(inspector.open, "one press does one thing");

        inspector.key_pressed(Key::Escape, Modifiers::default(), &document);
        assert!(!inspector.open);
    }

    #[test]
    fn an_unchanged_frame_is_not_built_a_second_time() {
        let document = document();
        let mut inspector = panel();
        frame(&mut inspector, &document);
        assert_eq!(inspector.builds(), 1);

        frame(&mut inspector, &document);
        assert_eq!(inspector.builds(), 1, "nothing about it moved");

        inspector.apply(Action::TogglePicker);
        frame(&mut inspector, &document);
        assert_eq!(inspector.builds(), 2, "the picker is drawn lit");
    }

    #[test]
    fn the_highlight_is_the_four_shades_around_one_rectangle() {
        let theme = Theme::light();
        let mut list = DisplayList::new();
        paint_highlight(
            &mut list,
            &theme,
            Rect::new(100.0, 100.0, 200.0, 50.0),
            &BoxEdges {
                margin: (10.0, 10.0, 10.0, 10.0),
                border: (2.0, 2.0, 2.0, 2.0),
                padding: (5.0, 5.0, 5.0, 5.0),
            },
        );
        assert_eq!(list.items().len(), 4, "one fill per ring, outermost first");
    }
}
