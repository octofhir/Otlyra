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

/// Which pane of the panel is showing.
///
/// Each one takes the whole panel, which is how every other browser's devtools
/// behave and is not what this started as: the DOM tree used to sit on the left
/// whatever was chosen, so *Elements* was permanently open and the tabs only
/// swapped the half beside it. A console that shares the window with a tree is a
/// console with half a window.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pane {
    /// The tree, and a sidebar about the chosen node.
    Elements,
    /// What the browser said while it worked.
    Console,
    /// What it asked the network for.
    Network,
}

impl Pane {
    /// The three of them, in the order they are offered.
    pub const ALL: [Self; 3] = [Self::Elements, Self::Console, Self::Network];

    /// What this is called on the panel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Elements => "Elements",
            Self::Console => "Console",
            Self::Network => "Network",
        }
    }
}

/// Which sidebar is showing beside the tree.
///
/// Inside *Elements* rather than beside it at the top, because each of these is
/// about the node the tree has chosen: a Styles tab with no tree to choose from
/// would be a tab about nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Sidebar {
    /// What the node is, and what it carries.
    Node,
    /// What the cascade computed for it.
    Styles,
    /// What the layout made of it, in numbers.
    Layout,
}

impl Sidebar {
    /// The three of them, in the order they are offered.
    pub const ALL: [Self; 3] = [Self::Node, Self::Styles, Self::Layout];

    /// What this is called on the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Node => "Node",
            Self::Styles => "Styles",
            Self::Layout => "Layout",
        }
    }
}

/// What the panel is told about the page it is looking at.
///
/// Gathered by the browser and handed over for the length of one frame. The
/// panel never holds a page: what it would hold is a second copy of state the
/// engine owns, and the whole point of an inspector is that what it shows is
/// what the engine actually has.
pub struct Facts<'a> {
    /// The document being shown, if the tab has one.
    ///
    /// Optional because two of the panes are about the browser rather than the
    /// page: a tab whose load failed has a console worth reading and a network
    /// list that says why, and gating the whole panel on a document would hide
    /// exactly the two panes that could explain the failure.
    pub document: Option<&'a Document>,
    /// What the cascade computed for the chosen node, if it has a box.
    pub style: Option<&'a otlyra_css::ComputedStyle>,
    /// Where its border box was drawn, in window coordinates.
    pub rect: Option<Rect>,
    /// How wide the containing block is, for resolving a percentage.
    pub containing: Option<f64>,
    /// Every request the browser has made, oldest first.
    pub exchanges: &'a [crate::fetcher::Exchange],
}

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
    /// Show this pane.
    Show(Pane),
    /// Show this sidebar, beside the tree.
    ShowSidebar(Sidebar),
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
    pane_scroll: f64,
    picking: bool,
    pane: Pane,
    sidebar: Sidebar,
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
    /// Which pane is showing.
    pub pane: Pane,
    /// Which sidebar is showing beside the tree.
    pub sidebar: Sidebar,
    /// How much of the content area the panel takes.
    pub height: f64,
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    expanded: HashSet<NodeId>,
    split: f64,
    /// How far the tree is scrolled, and how far it could be.
    scroll: f64,
    overflow: Overflow,
    /// The same for the pane beside it.
    ///
    /// Two positions rather than one, because they are two lists: a styles
    /// table thirty rows long beside a tree of four would otherwise be held to
    /// the tree's four rows of travel, which is a pane that will not scroll.
    pane_scroll: f64,
    pane_overflow: Overflow,
    /// Where the panel was drawn, so the wheel can be given to whichever half
    /// the pointer is over.
    panel: Rect,
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
            pane: Pane::Elements,
            sidebar: Sidebar::Node,
            height: DEFAULT_HEIGHT,
            theme: Theme::light(),
            expanded: HashSet::new(),
            split: DEFAULT_SPLIT,
            scroll: 0.0,
            overflow: Overflow::default(),
            pane_scroll: 0.0,
            pane_overflow: Overflow::default(),
            panel: Rect::ZERO,
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
            Action::Show(pane) => {
                // A new pane starts at its top. Keeping the old position would
                // open a short list already scrolled past its end.
                if pane != self.pane {
                    self.pane_scroll = 0.0;
                }
                self.pane = pane;
            }
            Action::ShowSidebar(sidebar) => {
                if sidebar != self.sidebar {
                    self.pane_scroll = 0.0;
                }
                self.sidebar = sidebar;
            }
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
        // And show the tree it was revealed in. Choosing an element while the
        // console is open would put the selection somewhere nobody is looking,
        // which is the same as not showing it at all.
        self.pane = Pane::Elements;
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
        // The wheel goes to whatever is under the pointer, which here is one of
        // two lists side by side. Which one is arithmetic against the divider
        // the last frame drew, so it cannot disagree with what was drawn.
        if self.pane == Pane::Elements
            && self.pointer.0 < self.panel.x + self.panel.width * self.split
        {
            self.scroll = (self.scroll + delta).clamp(0.0, self.overflow.get());
        } else {
            self.pane_scroll = (self.pane_scroll + delta).clamp(0.0, self.pane_overflow.get());
        }
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
        facts: &Facts<'_>,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        // Kept every frame, not only when one is built: the wheel is given to
        // whichever half the pointer is over, and a frame served from the cache
        // is still a frame the pointer is over.
        self.panel = rect;
        let rows = facts
            .document
            .map(|document| self.flatten(document))
            .unwrap_or_default();
        let appearance = Appearance {
            rect,
            nodes: rows.len(),
            selected: self.selected,
            expanded: self.expanded.len(),
            split: self.split,
            scroll: self.scroll,
            pane_scroll: self.pane_scroll,
            picking: self.picking,
            pane: self.pane,
            sidebar: self.sidebar,
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
        let mut root = self.build(&theme, facts);
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

    fn build(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        // Each pane takes the whole panel. Only *Elements* is two things side
        // by side, because only it has a selection for a sidebar to be about.
        let body: Child<Action> = match self.pane {
            Pane::Elements => Box::new(Split::row(
                self.split,
                Box::new(Padding::new(
                    Insets::all(theme.gap * 0.5),
                    self.tree(theme, facts),
                )),
                self.sidebar(theme, facts),
                Action::SplitAt,
            )),
            Pane::Console => Box::new(Padding::new(
                Insets::all(theme.gap),
                self.console_pane(theme),
            )),
            Pane::Network => Box::new(Padding::new(
                Insets::all(theme.gap),
                self.network_pane(theme, facts),
            )),
        };

        Box::new(Stack::column(
            0.0,
            vec![self.header(theme), Box::new(Flex::new(1.0, body))],
        ))
    }

    /// The document, as rows.
    fn tree(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        match facts.document {
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
        }
    }

    /// The sidebar beside the tree: its own tabs, and whichever is chosen.
    fn sidebar(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        let tabs: Child<Action> = Box::new(Padding::new(
            Insets::symmetric(theme.gap * 0.5, theme.gap * 0.5),
            Box::new(Align::left(controls::segmented(
                theme,
                &self.focus,
                Sidebar::ALL
                    .iter()
                    .map(|sidebar| (sidebar.label().to_owned(), Action::ShowSidebar(*sidebar)))
                    .collect(),
                Sidebar::ALL
                    .iter()
                    .position(|sidebar| *sidebar == self.sidebar)
                    .unwrap_or(0),
            ))),
        ));

        let body: Child<Action> = if self.selected.is_none() || facts.document.is_none() {
            Box::new(Align::centre(Box::new(Label::new(
                "Choose an element.",
                theme.font_size_small,
                theme.ink_dim,
            ))))
        } else {
            match self.sidebar {
                Sidebar::Node => self.elements_pane(theme, facts),
                Sidebar::Styles => self.styles_pane(theme, facts),
                Sidebar::Layout => self.layout_pane(theme, facts),
            }
        };

        Box::new(Stack::column(
            0.0,
            vec![
                tabs,
                Box::new(Flex::new(
                    1.0,
                    Box::new(Padding::new(Insets::all(theme.gap), body)),
                )),
            ],
        ))
    }

    /// The bar across the panel: what it is, the picker, and the way out.
    fn header(&self, theme: &Theme) -> Child<Action> {
        // The panes are chosen with the control the settings already use for a
        // short list of choices. A second one shaped like tabs would be a second
        // answer to the same question.
        let tabs: Child<Action> = Box::new(Align::centre(controls::segmented(
            theme,
            &self.focus,
            Pane::ALL
                .iter()
                .map(|pane| (pane.label().to_owned(), Action::Show(*pane)))
                .collect(),
            Pane::ALL
                .iter()
                .position(|pane| *pane == self.pane)
                .unwrap_or(0),
        )));

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
                            tabs,
                            // The frame line takes what the tabs leave and is
                            // cut where it runs out, rather than the tabs
                            // shrinking to make room: which pane is showing has
                            // to stay legible on a narrow window, and a stage
                            // timing that scrolled off is one a wider window
                            // brings back.
                            Box::new(Flex::new(
                                1.0,
                                Box::new(crate::widget::Clip::new(Box::new(Align::right(
                                    frame_line(theme),
                                )))),
                            )),
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

    /// What the chosen node is: its tag, and the attributes it carries.
    fn elements_pane(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        let Some(selected) = facts
            .document
            .and_then(|document| self.selected.and_then(|node| document.get(node)))
        else {
            return Box::new(Gap::new(0.0, 0.0));
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
                            self.pane_scroll,
                            std::rc::Rc::clone(&self.pane_overflow),
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
        Box::new(Stack::column(theme.gap, rows))
    }

    /// What the cascade computed for the chosen node.
    fn styles_pane(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        let Some(style) = facts.style else {
            // A node with no box is a node the cascade was never asked about:
            // a comment, or an element under `display: none`.
            return Box::new(Align::centre(Box::new(Label::new(
                "This node has no box, so nothing was computed for it.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        };
        let rows: Vec<Vec<String>> = describe(style)
            .into_iter()
            .map(|(name, value)| vec![name.to_owned(), value])
            .collect();
        Box::new(Table::new(
            vec!["property".to_owned(), "computed".to_owned()],
            rows,
            self.pane_scroll,
            std::rc::Rc::clone(&self.pane_overflow),
        ))
    }

    /// What the browser said while it worked, newest last.
    ///
    /// The same stream the terminal gets, kept where the browser can read it.
    /// When M12 brings a script engine this is where its console lands; until
    /// then the pane says so rather than showing an empty box that looks broken.
    fn console_pane(&self, theme: &Theme) -> Child<Action> {
        let journal = crate::observability::journal();
        let rows: Vec<Vec<String>> = journal
            .records()
            .into_iter()
            .rev()
            .take(200)
            .map(|record| vec![record.level.to_string(), record.target, record.message])
            .collect();

        let note: Child<Action> = Box::new(Label::new(
            "The browser's own log. A page's `console` lands here when there is a \
             script engine to write to it.",
            theme.font_size_small,
            theme.ink_dim,
        ));

        if rows.is_empty() {
            return Box::new(Stack::column(
                theme.gap,
                vec![
                    note,
                    Box::new(Label::new(
                        "Nothing said yet. `OTLYRA_LOG=debug` is how to hear more.",
                        theme.font_size_small,
                        theme.ink_dim,
                    )),
                ],
            ));
        }

        Box::new(Stack::column(
            theme.gap,
            vec![
                note,
                Box::new(Flex::new(
                    1.0,
                    Box::new(Table::new(
                        vec!["level".to_owned(), "from".to_owned(), "said".to_owned()],
                        rows,
                        self.pane_scroll,
                        std::rc::Rc::clone(&self.pane_overflow),
                    )),
                )),
            ],
        ))
    }

    /// What the browser asked the network for, and what came back.
    fn network_pane(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        use crate::fetcher::Status;

        if facts.exchanges.is_empty() {
            return Box::new(Align::centre(Box::new(Label::new(
                "Nothing has been asked for in this tab.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        }

        let rows: Vec<Vec<String>> = facts
            .exchanges
            .iter()
            .rev()
            .map(|exchange| {
                let (status, size) = match &exchange.status {
                    Status::Pending => ("pending".to_owned(), String::new()),
                    Status::Ok(bytes) => ("ok".to_owned(), bytes_read(*bytes)),
                    Status::Failed(error) => ("failed".to_owned(), error.clone()),
                };
                vec![
                    format!("{:?}", exchange.kind).to_lowercase(),
                    status,
                    size,
                    exchange.took.map(millis).unwrap_or_default(),
                    // Two numbers rather than one: how slow the transport was,
                    // and how long the request sat waiting for a thread to run
                    // on. A single figure would hide which of the two a slow
                    // page is suffering from.
                    exchange.waited.map(millis).unwrap_or_default(),
                    exchange.url.clone(),
                ]
            })
            .collect();

        Box::new(Table::new(
            vec![
                "kind".to_owned(),
                "status".to_owned(),
                "size".to_owned(),
                "took".to_owned(),
                "waited".to_owned(),
                "address".to_owned(),
            ],
            rows,
            self.pane_scroll,
            std::rc::Rc::clone(&self.pane_overflow),
        ))
    }

    /// What the layout made of it: the box, taken apart, in numbers.
    fn layout_pane(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        let (Some(style), Some(border)) = (facts.style, facts.rect) else {
            return Box::new(Align::centre(Box::new(Label::new(
                "This node was not drawn, so it has no box to take apart.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        };
        let edges = BoxEdges::of(style, facts.containing);
        let numbers = Box::new(Mono::new(
            format!(
                "{} × {} at ({}, {})",
                round(border.width),
                round(border.height),
                round(border.x),
                round(border.y)
            ),
            theme.ink_dim,
        ));

        Box::new(Stack::column(
            theme.gap,
            vec![
                Box::new(Flex::new(1.0, box_model(theme, border, edges))),
                numbers,
            ],
        ))
    }
}

/// The box, drawn as the four rings it is made of, with the numbers on them.
///
/// The same shades as the overlay on the page, so the diagram and the thing it
/// describes are recognisably one thing.
fn box_model(theme: &Theme, border: Rect, edges: BoxEdges) -> Child<Action> {
    let theme = theme.clone();
    Box::new(crate::widget::Painted::new(
        0.0,
        118.0,
        move |rect, cx, list| {
            let label = |list: &mut DisplayList, cx: &mut Cx, text: &str, at: Rect| {
                let mut mono = Mono::new(text, theme.ink).size(theme.font_size_small);
                crate::widget::Widget::<Action>::place(&mut mono, at, cx);
                crate::widget::Widget::<Action>::draw(&mut mono, cx, list);
            };

            // Nested rings of a fixed thickness rather than to scale: a margin
            // of one pixel and a margin of two hundred have to be equally
            // readable, and the numbers are what says which it is.
            let step = 22.0;
            let rings = [
                (theme.box_margin, "margin", edges.margin),
                (theme.box_border, "border", edges.border),
                (theme.box_padding, "padding", edges.padding),
            ];
            let mut at = Rect::new(rect.x, rect.y, rect.width.min(340.0), rect.height);
            for (color, name, sides) in rings {
                fill_rounded(list, at, color, 2.0);
                label(
                    list,
                    cx,
                    name,
                    Rect::new(at.x + 4.0, at.y + 3.0, 60.0, 12.0),
                );
                let (left, top, right, bottom) = sides;
                label(
                    list,
                    cx,
                    &round(top),
                    Rect::new(at.x + at.width / 2.0 - 8.0, at.y + 3.0, 40.0, 12.0),
                );
                label(
                    list,
                    cx,
                    &round(bottom),
                    Rect::new(
                        at.x + at.width / 2.0 - 8.0,
                        at.y + at.height - 14.0,
                        40.0,
                        12.0,
                    ),
                );
                label(
                    list,
                    cx,
                    &round(left),
                    Rect::new(at.x + 2.0, at.y + at.height / 2.0 - 6.0, 30.0, 12.0),
                );
                label(
                    list,
                    cx,
                    &round(right),
                    Rect::new(
                        at.x + at.width - 26.0,
                        at.y + at.height / 2.0 - 6.0,
                        30.0,
                        12.0,
                    ),
                );
                at = Rect::new(
                    at.x + step,
                    at.y + step,
                    (at.width - step * 2.0).max(0.0),
                    (at.height - step * 2.0).max(0.0),
                );
            }

            fill_rounded(list, at, theme.box_content, 2.0);
            let content = edges.content_of(border);
            label(
                list,
                cx,
                &format!("{} × {}", round(content.width), round(content.height)),
                Rect::new(at.x + 6.0, at.y + at.height / 2.0 - 6.0, 200.0, 12.0),
            );
        },
    ))
}

/// How long the last of each stage took, along the panel's own header.
///
/// A slow page needs a first place to look, and this is it: the stages are the
/// pipeline in order, so the one that is out of proportion is the one to open a
/// pane about. Only stages that have actually run are shown — a line of zeroes
/// would be a line claiming work happened that did not.
fn frame_line<A: 'static>(theme: &Theme) -> Child<A> {
    let latest = crate::observability::journal().latest();
    if latest.is_empty() {
        return Box::new(Gap::new(0.0, 0.0));
    }
    let text = latest
        .iter()
        .map(|timing| format!("{} {}", short(timing.span), millis(timing.took)))
        .collect::<Vec<_>>()
        .join("   ");
    Box::new(Mono::new(text, theme.ink_dim).size(theme.font_size_small))
}

/// A span name short enough to sit in a header.
fn short(span: &str) -> &str {
    match span {
        crate::observability::spans::PARSE_HTML => "parse",
        crate::observability::spans::RECALC_STYLE => "style",
        crate::observability::spans::BUILD_DISPLAY_LIST => "list",
        other => other,
    }
}

/// A duration in milliseconds, as a person reads one.
fn millis(took: std::time::Duration) -> String {
    let ms = took.as_secs_f64() * 1000.0;
    if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

/// A byte count, in the units a person thinks in.
fn bytes_read(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} kB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// A number as a person reads it: whole where it is whole.
fn round(value: f64) -> String {
    if (value - value.round()).abs() < 0.01 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
    }
}

/// What the cascade computed, as rows a table can show.
///
/// A chosen list rather than every field on the struct. The whole of a computed
/// style is a hundred values, most of them the initial one on most elements, and
/// a pane that showed all of them would bury the four that explain the bug. This
/// is the set an inspector is opened to look at.
///
/// What is missing, and known to be: *which rule* each value came from. The
/// cascade is Stylo's and it knows, but it does not hand the winning declaration
/// back with the value, so a pane that showed an origin would be inventing one.
fn describe(style: &otlyra_css::ComputedStyle) -> Vec<(&'static str, String)> {
    use otlyra_css::{Length, LengthOrAuto};

    let length = |value: Length| match value {
        Length::Px(px) => format!("{px}px"),
        Length::Percent(percent) => format!("{}%", percent * 100.0),
    };
    let auto = |value: LengthOrAuto| match value {
        LengthOrAuto::Px(px) => format!("{px}px"),
        LengthOrAuto::Percent(percent) => format!("{}%", percent * 100.0),
        LengthOrAuto::Auto => "auto".to_owned(),
    };
    let four = |sides: &otlyra_css::Sides<LengthOrAuto>| {
        format!(
            "{} {} {} {}",
            auto(sides.top),
            auto(sides.right),
            auto(sides.bottom),
            auto(sides.left)
        )
    };
    let four_length = |sides: &otlyra_css::Sides<Length>| {
        format!(
            "{} {} {} {}",
            length(sides.top),
            length(sides.right),
            length(sides.bottom),
            length(sides.left)
        )
    };

    let mut rows = vec![
        ("display", format!("{:?}", style.display).to_lowercase()),
        ("position", format!("{:?}", style.position).to_lowercase()),
        ("width", auto(style.width)),
        ("height", auto(style.height)),
        ("margin", four(&style.margin)),
        ("padding", four_length(&style.padding)),
        (
            "border-width",
            format!(
                "{}px {}px {}px {}px",
                style.border.top.width,
                style.border.right.width,
                style.border.bottom.width,
                style.border.left.width
            ),
        ),
        ("color", hex(style.color)),
        ("background-color", hex(style.background_color)),
        ("font-family", style.font_family.to_string()),
        ("font-size", format!("{}px", style.font_size)),
        ("font-weight", style.font_weight.to_string()),
        ("line-height", format!("{:?}", style.line_height)),
        (
            "text-align",
            format!("{:?}", style.text_align).to_lowercase(),
        ),
        ("overflow", format!("{:?}", style.overflow).to_lowercase()),
        ("float", format!("{:?}", style.float).to_lowercase()),
        (
            "z-index",
            style.z_index.map_or("auto".to_owned(), |z| z.to_string()),
        ),
    ];

    // The properties of a formatting context are only worth the rows when the
    // element is in one: `flex-grow` on a block is noise, and noise is what
    // buries the value that explains the bug.
    if style.display == otlyra_css::Display::Flex {
        rows.extend([
            (
                "flex-direction",
                format!("{:?}", style.flex_direction).to_lowercase(),
            ),
            ("flex-wrap", format!("{:?}", style.flex_wrap).to_lowercase()),
            (
                "justify-content",
                format!("{:?}", style.justify_content).to_lowercase(),
            ),
            (
                "align-items",
                format!("{:?}", style.align_items).to_lowercase(),
            ),
        ]);
    }
    if style.display == otlyra_css::Display::Grid {
        rows.extend([
            (
                "grid-template-columns",
                tracks(&style.grid_columns, style.grid_columns_fill.as_deref()),
            ),
            ("grid-template-rows", tracks(&style.grid_rows, None)),
        ]);
    }
    if matches!(
        style.display,
        otlyra_css::Display::Flex | otlyra_css::Display::Grid
    ) {
        rows.push((
            "gap",
            format!("{} {}", length(style.gap.0), length(style.gap.1)),
        ));
    }
    rows
}

/// A track list, as CSS would write it.
fn tracks(template: &[otlyra_css::Track], fill: Option<&[otlyra_css::Track]>) -> String {
    let one = |track: &otlyra_css::Track| format!("{track:?}").to_lowercase();
    let mut out: Vec<String> = template.iter().map(one).collect();
    if let Some(fill) = fill {
        out.push(format!(
            "repeat(auto-fill, {})",
            fill.iter().map(one).collect::<Vec<_>>().join(" ")
        ));
    }
    if out.is_empty() {
        return "none".to_owned();
    }
    out.join(" ")
}

/// A colour as CSS writes it, which is what a person is looking for.
fn hex(color: Color) -> String {
    let [red, green, blue, alpha] = color.components;
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    if alpha >= 1.0 {
        format!("#{:02x}{:02x}{:02x}", byte(red), byte(green), byte(blue))
    } else {
        format!(
            "rgba({}, {}, {}, {alpha:.2})",
            byte(red),
            byte(green),
            byte(blue)
        )
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
pub fn paint_highlight(
    list: &mut DisplayList,
    theme: &Theme,
    border: Rect,
    style: &BoxEdges,
    content_too: bool,
) {
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
    // Each shade is the *band* between two edges, not a rectangle laid over
    // everything inside it. Stacked rectangles would put three washes over the
    // content and leave the element unreadable under the thing that is meant to
    // explain it — which is the difference between an overlay and a curtain.
    band(list, margin, border, theme.box_margin);
    band(list, border, padding, theme.box_border);
    band(list, padding, content, theme.box_padding);
    // The content is washed over only when there is nothing else to read there.
    // A container with a track overlay has its items inside this rectangle, and
    // a shade over them is a shade over the thing being looked at.
    if content_too {
        fill_rounded(list, content, theme.box_content, 0.0);
    }
}

/// The ring between two rectangles, as its four strips.
///
/// Four fills rather than one even-odd path, because the sides have different
/// widths — a box with `border-left: 8px` and no border anywhere else is the
/// ordinary case, and a ring of one width cannot say that.
fn band(list: &mut DisplayList, outer: Rect, inner: Rect, color: Color) {
    let top = Rect::new(outer.x, outer.y, outer.width, (inner.y - outer.y).max(0.0));
    let bottom = Rect::new(
        outer.x,
        inner.y + inner.height,
        outer.width,
        (outer.y + outer.height - inner.y - inner.height).max(0.0),
    );
    let left = Rect::new(outer.x, inner.y, (inner.x - outer.x).max(0.0), inner.height);
    let right = Rect::new(
        inner.x + inner.width,
        inner.y,
        (outer.x + outer.width - inner.x - inner.width).max(0.0),
        inner.height,
    );
    for strip in [top, bottom, left, right] {
        fill_rounded(list, strip, color, 0.0);
    }
}

/// The four edges of a box, in logical pixels, left-top-right-bottom.
///
/// Border widths are used values and exact. Padding and margin are resolved
/// here against the containing block, because a fragment carries one rectangle
/// and the rings are the differences between four — and a percentage that has
/// nothing to be a percentage *of* is drawn as nothing rather than as a guess.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct BoxEdges {
    /// Margin, left-top-right-bottom.
    pub margin: (f64, f64, f64, f64),
    /// Border widths.
    pub border: (f64, f64, f64, f64),
    /// Padding.
    pub padding: (f64, f64, f64, f64),
}

impl BoxEdges {
    /// What `style` says the edges are, given how wide the containing block is.
    ///
    /// A percentage is a fraction of the containing block's *width* on every
    /// side, vertically as well, which is what CSS says and what surprises
    /// everyone once.
    pub fn of(style: &otlyra_css::ComputedStyle, containing: Option<f64>) -> Self {
        use otlyra_css::{Length, LengthOrAuto};
        let percent = |fraction: f32| containing.unwrap_or(0.0) * f64::from(fraction);
        let length = |value: Length| match value {
            Length::Px(px) => f64::from(px),
            Length::Percent(fraction) => percent(fraction),
        };
        // `auto` is resolved during layout and is not on the style. Nothing is
        // drawn for it rather than a number nobody computed.
        let auto = |value: LengthOrAuto| match value {
            LengthOrAuto::Px(px) => f64::from(px),
            LengthOrAuto::Percent(fraction) => percent(fraction),
            LengthOrAuto::Auto => 0.0,
        };
        Self {
            margin: (
                auto(style.margin.left),
                auto(style.margin.top),
                auto(style.margin.right),
                auto(style.margin.bottom),
            ),
            border: (
                f64::from(style.border.left.width),
                f64::from(style.border.top.width),
                f64::from(style.border.right.width),
                f64::from(style.border.bottom.width),
            ),
            padding: (
                length(style.padding.left),
                length(style.padding.top),
                length(style.padding.right),
                length(style.padding.bottom),
            ),
        }
    }

    /// The content box inside a border box.
    pub fn content_of(&self, border: Rect) -> Rect {
        Rect::new(
            border.x + self.border.0 + self.padding.0,
            border.y + self.border.1 + self.padding.1,
            (border.width - self.border.0 - self.border.2 - self.padding.0 - self.padding.2)
                .max(0.0),
            (border.height - self.border.1 - self.border.3 - self.padding.1 - self.padding.3)
                .max(0.0),
        )
    }
}

/// Where a container's tracks fall, for the dashed overlay over one.
///
/// Derived from where the items themselves were drawn rather than asked of the
/// layout: the engine sizes its tracks internally and does not report them, and
/// the edges of the boxes that landed in them are the same lines. What that
/// cannot show is a track nothing was placed in — an empty column has no item to
/// have an edge — and that is stated rather than papered over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tracks {
    /// Where the column lines are, in window coordinates.
    pub columns: Vec<Line>,
    /// Where the row lines are.
    pub rows: Vec<Line>,
    /// Whether the lines are numbered, which is a grid's habit and not a flex
    /// container's.
    pub numbered: bool,
}

/// One line of a track overlay.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Line {
    /// Where it is, in window coordinates.
    pub at: f64,
    /// Its number, when it opens a track rather than closing a gutter.
    pub number: Option<usize>,
}

impl Tracks {
    /// The lines the boxes in `items` fall between, inside `container`.
    ///
    /// `gap` — rows first, then columns, as CSS writes it — matters to the
    /// numbering and not to the drawing. A gutter has two edges and both are
    /// drawn, but CSS numbers the *line*, and the gap is how thick that one line
    /// was asked to be. Numbering both would name every track after the first
    /// twice, which is the first thing a person would notice and disbelieve.
    pub fn of(container: Rect, items: &[Rect], numbered: bool, gap: (f64, f64)) -> Self {
        let mut columns: Vec<(f64, bool)> = Vec::new();
        let mut rows: Vec<(f64, bool)> = Vec::new();
        for item in items {
            columns.push((item.x, true));
            columns.push((item.x + item.width, true));
            rows.push((item.y, true));
            rows.push((item.y + item.height, true));
        }
        // The container's own edges are drawn — they bound the overlay — but
        // they are not lines unless a track reaches them. `grid-template-columns:
        // 100px 100px` in a container twice that wide has a right edge that no
        // track ends at, and numbering it would name a line the stylesheet
        // cannot address.
        columns.push((container.x, false));
        columns.push((container.x + container.width, false));
        rows.push((container.y, false));
        rows.push((container.y + container.height, false));
        Self {
            columns: lines(columns, gap.1),
            rows: lines(rows, gap.0),
            numbered,
        }
    }
}

/// The distinct lines in a list of edges, numbered.
///
/// Each edge says whether it came from an item. Sorted, with anything within
/// half a pixel of its neighbour dropped — two items that share a line report it
/// twice, and a line drawn twice looks heavier than the one beside it. What is
/// left is numbered when it is a track's own edge and not:
///
/// - an edge exactly `gap` past the one before it, which is the far side of a
///   gutter and so the same line seen from the other end, or
/// - an edge only the container reached, which bounds the overlay without being
///   a line any stylesheet can name.
fn lines(mut values: Vec<(f64, bool)>, gap: f64) -> Vec<Line> {
    values.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    // An edge both an item and the container reached is the item's: keeping the
    // truer of the two is what `dedup_by` is told to do here, since it drops the
    // first of each pair.
    values.dedup_by(|a, b| {
        let same = (a.0 - b.0).abs() < 0.5;
        if same {
            b.1 |= a.1;
        }
        same
    });

    let mut out: Vec<Line> = Vec::with_capacity(values.len());
    let mut number = 0;
    let mut previous: Option<f64> = None;
    for (at, from_item) in values {
        let closes_a_gutter =
            gap > 0.0 && previous.is_some_and(|last| (at - last - gap).abs() < 0.5);
        if from_item && !closes_a_gutter {
            number += 1;
            out.push(Line {
                at,
                number: Some(number),
            });
        } else {
            out.push(Line { at, number: None });
        }
        previous = Some(at);
    }
    out
}

/// How long one dash is, and the gap after it.
const DASH: f64 = 4.0;

/// A dashed line from one point to another, along one axis.
///
/// Drawn as a row of small fills rather than as a stroked path with a dash
/// pattern, because the display list has no dashes and a rasterizer that grew
/// them would be a feature carried for one overlay. This is four rectangles per
/// twenty pixels and it is the same on every backend.
fn dashed(list: &mut DisplayList, from: (f64, f64), to: (f64, f64), width: f64, color: Color) {
    let horizontal = (to.1 - from.1).abs() < f64::EPSILON;
    let length = if horizontal {
        to.0 - from.0
    } else {
        to.1 - from.1
    };
    let mut at = 0.0;
    while at < length {
        let run = DASH.min(length - at);
        let rect = if horizontal {
            Rect::new(from.0 + at, from.1, run, width)
        } else {
            Rect::new(from.0, from.1 + at, width, run)
        };
        fill_rounded(list, rect, color, 0.0);
        at += DASH * 2.0;
    }
}

/// Paint the dashed track lines of a grid or flex container over the page.
///
/// What Firefox's grid overlay does, and for the same reason: the numbers in a
/// stylesheet are lines, and until they are drawn on the page nobody can tell
/// which of them the item actually landed against.
pub fn paint_tracks(list: &mut DisplayList, cx: &mut Cx, container: Rect, tracks: &Tracks) {
    let theme = cx.theme.clone();
    let color = theme.grid_line;
    for line in &tracks.columns {
        dashed(
            list,
            (line.at, container.y),
            (line.at, container.y + container.height),
            1.0,
            color,
        );
    }
    for line in &tracks.rows {
        dashed(
            list,
            (container.x, line.at),
            (container.x + container.width, line.at),
            1.0,
            color,
        );
    }
    if !tracks.numbered {
        return;
    }

    // The line numbers, which are what a stylesheet names a track by. On a tab
    // of the line's own colour, because text alone over a page of unknown
    // colours is text nobody can read.
    let badge = |list: &mut DisplayList, cx: &mut Cx, at: Rect, number: usize| {
        fill_rounded(list, at, color, 2.0);
        let mut label = Mono::new(number.to_string(), theme.ink_on_accent).size(9.0);
        crate::widget::Widget::<Action>::place(
            &mut label,
            Rect::new(at.x + 3.0, at.y + 1.5, at.width, at.height),
            cx,
        );
        crate::widget::Widget::<Action>::draw(&mut label, cx, list);
    };

    for line in &tracks.columns {
        if let Some(number) = line.number {
            badge(
                list,
                cx,
                Rect::new(line.at, container.y - 13.0, 14.0, 12.0),
                number,
            );
        }
    }
    for line in &tracks.rows {
        if let Some(number) = line.number {
            badge(
                list,
                cx,
                Rect::new(container.x - 15.0, line.at, 14.0, 12.0),
                number,
            );
        }
    }
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

    /// Draw one frame with a node chosen and a style for it, which is what the
    /// panes that are about an element need in order to have anything in them.
    fn frame_with_style(inspector: &mut Inspector, document: &Document) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let style = otlyra_css::ComputedStyle::default();
        let facts = Facts {
            document: Some(document),
            style: Some(&style),
            rect: Some(Rect::new(0.0, 0.0, 100.0, 40.0)),
            containing: Some(400.0),
            exchanges: &[],
        };
        inspector.build_display_list(
            Rect::new(0.0, 300.0, 900.0, 300.0),
            &facts,
            &mut text,
            &mut list,
        );
    }

    /// Draw one frame, which is what gives the panel geometry to be pressed
    /// against.
    fn frame(inspector: &mut Inspector, document: &Document) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let facts = Facts {
            document: Some(document),
            style: None,
            rect: None,
            containing: None,
            exchanges: &[],
        };
        inspector.build_display_list(
            Rect::new(0.0, 300.0, 900.0, 300.0),
            &facts,
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
    fn a_gutter_is_one_line_however_many_edges_it_has() {
        // Three columns of 100 with a 10px gap, as their items' edges report
        // them: 0..100, 110..210, 220..320.
        let edges = vec![
            (0.0, true),
            (100.0, true),
            (110.0, true),
            (210.0, true),
            (220.0, true),
            (320.0, true),
        ];
        let numbered: Vec<Option<usize>> = lines(edges, 10.0)
            .into_iter()
            .map(|line| line.number)
            .collect();

        // Four lines for three tracks, and the far side of each gutter is the
        // same line seen from the other end rather than a fifth and a sixth.
        assert_eq!(
            numbered,
            vec![Some(1), Some(2), None, Some(3), None, Some(4)]
        );
    }

    #[test]
    fn edges_that_share_a_line_are_one_line() {
        // Two items that meet with no gap report the same edge twice.
        let numbered: Vec<Option<usize>> = lines(
            vec![(0.0, true), (50.0, true), (50.0, true), (100.0, true)],
            0.0,
        )
        .into_iter()
        .map(|line| line.number)
        .collect();
        assert_eq!(numbered, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_percentage_is_a_fraction_of_the_containing_block() {
        let mut style = otlyra_css::ComputedStyle::default();
        style.padding.left = otlyra_css::Length::Percent(0.1);
        style.padding.top = otlyra_css::Length::Px(4.0);
        style.margin.right = otlyra_css::LengthOrAuto::Auto;

        let edges = BoxEdges::of(&style, Some(500.0));
        // A tenth is not exactly a tenth in the single precision the style
        // carries, which is why this is a distance and not an equality.
        assert!(
            (edges.padding.0 - 50.0).abs() < 0.001,
            "ten per cent of five hundred, and got {}",
            edges.padding.0
        );
        assert_eq!(edges.padding.1, 4.0);
        // `auto` is resolved by the layout and is not on the style, so nothing
        // is drawn for it rather than a number nobody computed.
        assert_eq!(edges.margin.2, 0.0);
    }

    #[test]
    fn the_content_box_is_the_border_box_less_what_is_around_it() {
        let edges = BoxEdges {
            margin: (10.0, 10.0, 10.0, 10.0),
            border: (3.0, 3.0, 3.0, 3.0),
            padding: (24.0, 24.0, 24.0, 24.0),
        };
        let content = edges.content_of(Rect::new(20.0, 98.0, 960.0, 148.0));
        assert_eq!(content.width, 960.0 - 54.0);
        assert_eq!(content.height, 148.0 - 54.0);
        assert_eq!(content.x, 47.0);
    }

    #[test]
    fn the_styles_pane_says_only_what_the_element_is_in() {
        let mut style = otlyra_css::ComputedStyle::default();
        let named = |style: &otlyra_css::ComputedStyle| -> Vec<&'static str> {
            describe(style).into_iter().map(|(name, _)| name).collect()
        };

        // A block is not in a flex or a grid formatting context, and rows about
        // one would bury the rows that explain it.
        assert!(!named(&style).contains(&"flex-direction"));
        assert!(!named(&style).contains(&"grid-template-columns"));

        style.display = otlyra_css::Display::Flex;
        assert!(named(&style).contains(&"flex-direction"));
        assert!(named(&style).contains(&"gap"));

        style.display = otlyra_css::Display::Grid;
        assert!(named(&style).contains(&"grid-template-columns"));
        assert!(!named(&style).contains(&"flex-direction"));
    }

    #[test]
    fn a_pane_takes_the_whole_panel_and_only_elements_has_a_sidebar() {
        // What this replaced: the tree sat on the left whatever was chosen, so
        // Elements was permanently open and the tabs swapped only the half
        // beside it. A console with half a window is not a console.
        assert_eq!(Pane::ALL.len(), 3);
        assert_eq!(
            Pane::ALL.map(Pane::label),
            ["Elements", "Console", "Network"]
        );
        // Styles and Layout are about the chosen node, so they live inside the
        // pane that has a tree to choose from.
        assert_eq!(
            Sidebar::ALL.map(Sidebar::label),
            ["Node", "Styles", "Layout"]
        );
    }

    #[test]
    fn choosing_an_element_shows_the_tree_it_was_chosen_in() {
        let document = document();
        let mut inspector = panel();
        inspector.apply(Action::Show(Pane::Console));

        let node = every_node(&document)[1];
        inspector.reveal(&document, node);
        // Otherwise the selection lands somewhere nobody is looking, which is
        // the same as not showing it at all.
        assert_eq!(inspector.pane, Pane::Elements);
        assert_eq!(inspector.selected, Some(node));
    }

    #[test]
    fn a_size_is_written_in_the_units_a_person_thinks_in() {
        assert_eq!(bytes_read(512), "512 B");
        assert_eq!(bytes_read(2048), "2.0 kB");
        assert_eq!(bytes_read(3 * 1_048_576), "3.0 MB");
    }

    #[test]
    fn a_duration_keeps_a_decimal_only_while_it_is_worth_one() {
        use std::time::Duration;
        assert_eq!(millis(Duration::from_micros(1500)), "1.5 ms");
        assert_eq!(millis(Duration::from_millis(42)), "42 ms");
    }

    #[test]
    fn the_two_halves_of_the_panel_scroll_apart() {
        let document = document();
        let mut inspector = panel();
        // The styles table is long and the tree, closed, is one row: held to one
        // scroll position between them, the long list would be held to the short
        // one's travel and would not move at all.
        inspector.apply(Action::ShowSidebar(Sidebar::Styles));
        inspector.selected = Some(document.root());
        frame_with_style(&mut inspector, &document);

        // Over the right-hand half.
        inspector.pointer_moved(800.0, 500.0);
        inspector.scroll_by(400.0);
        assert!(inspector.pane_scroll > 0.0, "the pane took the wheel");
        assert_eq!(inspector.scroll, 0.0, "and the tree did not move with it");

        // And over the left-hand half, the tree takes it instead.
        let pane_was = inspector.pane_scroll;
        inspector.pointer_moved(100.0, 500.0);
        inspector.scroll_by(400.0);
        assert_eq!(
            inspector.pane_scroll, pane_was,
            "the pane stayed where it was put"
        );
    }

    #[test]
    fn a_new_pane_opens_at_its_top() {
        let document = document();
        let mut inspector = panel();
        inspector.apply(Action::ShowSidebar(Sidebar::Styles));
        inspector.selected = Some(document.root());
        frame_with_style(&mut inspector, &document);
        inspector.pointer_moved(800.0, 500.0);
        inspector.scroll_by(400.0);
        assert!(inspector.pane_scroll > 0.0);

        // A short list opened at the position a long one was left at would open
        // already scrolled past its own end.
        inspector.apply(Action::ShowSidebar(Sidebar::Node));
        assert_eq!(inspector.pane_scroll, 0.0);
    }

    #[test]
    fn changing_the_pane_rebuilds_the_frame() {
        let document = document();
        let mut inspector = panel();
        frame(&mut inspector, &document);
        assert_eq!(inspector.builds(), 1);

        inspector.apply(Action::ShowSidebar(Sidebar::Styles));
        frame(&mut inspector, &document);
        assert_eq!(
            inspector.builds(),
            2,
            "a different pane is a different frame"
        );
    }

    #[test]
    fn the_overlay_leaves_the_content_readable_when_there_is_something_in_it() {
        let theme = Theme::light();
        let border = Rect::new(100.0, 100.0, 200.0, 50.0);
        let edges = BoxEdges {
            margin: (10.0, 10.0, 10.0, 10.0),
            border: (2.0, 2.0, 2.0, 2.0),
            padding: (5.0, 5.0, 5.0, 5.0),
        };

        // Three bands of four strips, and no wash over the middle: the items of
        // a container are inside that rectangle and are the thing being looked
        // at.
        let mut bands = DisplayList::new();
        paint_highlight(&mut bands, &theme, border, &edges, false);
        assert_eq!(bands.items().len(), 12);

        let mut filled = DisplayList::new();
        paint_highlight(&mut filled, &theme, border, &edges, true);
        assert_eq!(filled.items().len(), 13, "and one more when it is empty");
    }
}
