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

use crate::widget::controls;
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
    /// What a screen reader would be told the page is.
    Accessibility,
}

impl Pane {
    /// The four of them, in the order they are offered.
    pub const ALL: [Self; 4] = [
        Self::Elements,
        Self::Console,
        Self::Network,
        Self::Accessibility,
    ];

    /// What this is called on the panel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Elements => "Elements",
            Self::Console => "Console",
            Self::Network => "Network",
            // Not "A11y": the panel is read by the people most likely to be
            // using one of these trees, and a numeronym is a word you have to
            // already know.
            Self::Accessibility => "Accessibility",
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

/// How much of the console is worth reading right now.
///
/// A floor rather than a set of ticks: a console is opened either to read
/// everything or to find the bad news, and four exclusive choices are one press
/// where a set of boxes is up to four.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Level {
    /// Everything the browser said, however quietly.
    All,
    /// What it thought worth mentioning, and worse.
    Info,
    /// What it thought was wrong, and worse.
    Warnings,
    /// Only what failed.
    Errors,
}

impl Level {
    /// The four of them, loudest last.
    pub const ALL: [Self; 4] = [Self::All, Self::Info, Self::Warnings, Self::Errors];

    /// What this is called on the filter.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Info => "Info",
            Self::Warnings => "Warnings",
            Self::Errors => "Errors",
        }
    }

    /// Whether a line at `level` is one this filter admits.
    ///
    /// `tracing` orders its levels with the verbose ones *greater*, so a floor
    /// is an upper bound on the number. Written once here rather than at the
    /// call site, where the comparison reads backwards to everyone.
    pub fn admits(self, level: tracing::Level) -> bool {
        match self {
            Self::All => true,
            Self::Info => level <= tracing::Level::INFO,
            Self::Warnings => level <= tracing::Level::WARN,
            Self::Errors => level <= tracing::Level::ERROR,
        }
    }
}

/// Which kinds of request the network list is showing.
///
/// One more than [`crate::fetcher::ResourceKind`] has, because *everything* is a
/// choice a filter offers and a resource kind is not. Its own enum rather than
/// an `Option<ResourceKind>` so it is `Copy`, `Eq` and namable in the cache key
/// and the segmented control without either of those being written out by hand.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KindFilter {
    /// Every request, whatever it was for.
    All,
    /// Only the page itself.
    Documents,
    /// Only stylesheets.
    Stylesheets,
    /// Only pictures.
    Images,
}

impl KindFilter {
    /// The four of them, in the order they are offered.
    pub const ALL: [Self; 4] = [Self::All, Self::Documents, Self::Stylesheets, Self::Images];

    /// What this is called on the filter.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Documents => "Docs",
            Self::Stylesheets => "CSS",
            Self::Images => "Images",
        }
    }

    /// Whether a request of `kind` is one this filter admits.
    pub fn admits(self, kind: crate::fetcher::ResourceKind) -> bool {
        use crate::fetcher::ResourceKind;
        matches!(
            (self, kind),
            (Self::All, _)
                | (Self::Documents, ResourceKind::Document)
                | (Self::Stylesheets, ResourceKind::Stylesheet)
                | (Self::Images, ResourceKind::Image)
        )
    }
}

/// Which side of a chosen request is showing.
///
/// The three Firefox shows, and for the same reason each is worth its own tab:
/// the headers are what was agreed, the response is what came back, and the
/// timings are how long it took — three questions, and a pane that stacked all
/// three would answer none of them well.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NetTab {
    /// What was asked and what was answered, header by header.
    Headers,
    /// The body that came back.
    Response,
    /// How long each part of it took.
    Timings,
}

impl NetTab {
    /// The three of them, in the order they are offered.
    pub const ALL: [Self; 3] = [Self::Headers, Self::Response, Self::Timings];

    /// What this is called on the tab.
    pub fn label(self) -> &'static str {
        match self {
            Self::Headers => "Headers",
            Self::Response => "Response",
            Self::Timings => "Timings",
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
    /// The laid-out page behind that document, if there is one.
    ///
    /// What the accessibility pane is a view of: the tree a reader is handed is
    /// built from boxes rather than from nodes, because `display: none` has
    /// already been taken out of one and not the other.
    pub page: Option<&'a crate::page::PageScene>,
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
    /// Choose the accessible thing on this row.
    SelectAccessible(usize),
    /// Open or close it.
    ToggleAccessible(usize),
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
    /// Choose this node, named rather than numbered.
    ///
    /// What the breadcrumbs report: a crumb is an ancestor, and an ancestor is
    /// not always a row — the tree may have it folded away, or a search may have
    /// filtered it out.
    Choose(NodeId),
    /// The pointer landed in the search field, at this offset in its text.
    SearchHit(controls::FieldHit),
    /// Show only the console lines at least this severe.
    ShowLevel(Level),
    /// Forget everything the console is showing.
    ClearConsole,
    /// Choose the request made under this number.
    SelectExchange(u64),
    /// Show only requests of this kind.
    ShowKind(KindFilter),
    /// Show this side of the chosen request.
    ShowNetTab(NetTab),
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

/// One row of the accessibility tree, and what it was built from.
///
/// The same shape as [`Row`] and for the same reason: the tree widget reports an
/// index, and this is what turns that back into something to say.
#[derive(Clone, Debug)]
struct AccessibleRow {
    /// The box's identity, which is what the collapsed set is keyed on.
    key: u64,
    /// What it is, without its children — they are the rows after it.
    item: crate::a11y::Accessible,
    row: TreeRow,
}

/// Everything the panel's appearance is a function of.
#[derive(Clone, PartialEq)]
struct Appearance {
    rect: Rect,
    nodes: usize,
    accessible: usize,
    selected: Option<NodeId>,
    expanded: usize,
    collapsed: usize,
    split: f64,
    scroll: f64,
    pane_scroll: f64,
    picking: bool,
    pane: Pane,
    sidebar: Sidebar,
    search: String,
    caret: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    level: Level,
    exchanges: usize,
    settled: usize,
    exchange: Option<u64>,
    kind_filter: KindFilter,
    net_tab: NetTab,
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
    /// What the tree is being narrowed by.
    ///
    /// The same `TextField` the address bar and the history search are, so
    /// selection, the clipboard and where a click lands in the text came free
    /// and cannot disagree between one field and the next.
    pub search: crate::ui::TextField,
    /// How much of the console is showing.
    pub level: Level,
    /// Which request the network pane has chosen, by its number.
    pub exchange: Option<u64>,
    /// Which kinds the network list is showing.
    pub kind_filter: KindFilter,
    /// Which side of the chosen request is showing.
    pub net_tab: NetTab,
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
    /// The accessibility tree as rows, and which of them are folded away.
    ///
    /// Folded rather than opened: the tree a reader walks is the whole point of
    /// the pane, so it starts open and the set records what somebody has since
    /// put away. The other way round, the pane opens saying nothing.
    accessible: Vec<AccessibleRow>,
    a11y_collapsed: HashSet<u64>,
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
            search: crate::ui::TextField::default(),
            level: Level::All,
            exchange: None,
            kind_filter: KindFilter::All,
            net_tab: NetTab::Headers,
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
            accessible: Vec::new(),
            a11y_collapsed: HashSet::new(),
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

    /// Draw from `theme` from the next frame on. The cache does not key on the
    /// theme, so the stored list goes with the old palette.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.cache = None;
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
            // The selection is the node, not the row: choosing something in the
            // accessibility tree chooses it in the DOM tree as well, and the
            // highlight on the page follows. Two selections would be two
            // answers to *what is being looked at*.
            Action::SelectAccessible(index) => {
                if let Some(row) = self.accessible.get(index) {
                    self.selected = row.item.node;
                }
            }
            Action::ToggleAccessible(index) => {
                if let Some(row) = self.accessible.get(index) {
                    let key = row.key;
                    if !self.a11y_collapsed.remove(&key) {
                        self.a11y_collapsed.insert(key);
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
            Action::Choose(node) => self.selected = Some(node),
            // The field's own state is changed where the focus is known, in
            // `deliver`: a hit that did not also take the keyboard would put a
            // caret in a field that cannot be typed in.
            Action::SearchHit(_) => {}
            Action::ShowLevel(level) => {
                if level != self.level {
                    self.pane_scroll = 0.0;
                }
                self.level = level;
            }
            Action::ClearConsole => crate::observability::journal().clear(),
            Action::SelectExchange(id) => {
                // A new request opens its detail at its top, and at Headers: a
                // response tab left scrolled from the last request would open
                // partway down a body that is not the same body.
                if self.exchange != Some(id) {
                    self.pane_scroll = 0.0;
                }
                self.exchange = Some(id);
            }
            Action::ShowKind(filter) => {
                if filter != self.kind_filter {
                    self.scroll = 0.0;
                }
                self.kind_filter = filter;
            }
            Action::ShowNetTab(tab) => {
                if tab != self.net_tab {
                    self.pane_scroll = 0.0;
                }
                self.net_tab = tab;
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
        // The network list is one thing across the whole pane until a request is
        // chosen; only then is there a right-hand half to send the wheel to.
        let split_shown = matches!(self.pane, Pane::Elements | Pane::Accessibility)
            || (self.pane == Pane::Network && self.exchange.is_some());
        let over_left = self.pointer.0 < self.panel.x + self.panel.width * self.split;
        // The left list takes the wheel when the pointer is over it, and the
        // whole of a network pane with nothing chosen is that list.
        if (split_shown && over_left) || (self.pane == Pane::Network && !split_shown) {
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
        document: Option<&Document>,
        clipboard: &mut dyn crate::clipboard::Clipboard,
    ) -> Option<Action> {
        if !self.open {
            return None;
        }
        if modifiers.is_accelerator() {
            // ⌘F puts the keyboard in the search field wherever it was, and
            // selects what is there so a second search replaces the first —
            // which is what every find field does and what stops the two
            // queries running together.
            if key == Key::Character('f') {
                self.pane = Pane::Elements;
                self.focused = self.focus.first_text();
                self.search.select_all();
                return Some(Action::None);
            }
            // The field's own accelerators — select all, cut, copy, paste —
            // while it holds the keyboard, before the tree's ⌘C is considered.
            if self.searching() && self.search.edit(key, modifiers, clipboard) {
                return Some(Action::None);
            }
            // ⌘C with an element chosen copies the row as the tree shows it:
            // what you see is exactly what lands on the clipboard. Everything
            // else stays the browser's.
            if key == Key::Character('c')
                && let Some(document) = document
                && let Some(row) = self
                    .flatten(document)
                    .into_iter()
                    .find(|row| Some(row.node) == self.selected)
            {
                clipboard.write(row.row.text);
                return Some(Action::None);
            }
            return None;
        }
        // While the field has the keyboard it gets the keys: an arrow moves the
        // caret rather than the selection, and Escape gives the tree back.
        if self.searching() {
            match key {
                Key::Escape => {
                    // Escape empties the field before it lets it go: a search
                    // left behind is a tree still filtered by something nobody
                    // can see the field for.
                    if self.search.text().is_empty() {
                        self.focused = None;
                    } else {
                        self.search.set_text(String::new());
                    }
                    return Some(Action::None);
                }
                Key::Enter => {
                    self.focused = None;
                    return Some(Action::None);
                }
                _ if self.search.edit(key, modifiers, clipboard) => return Some(Action::None),
                _ => {}
            }
        }

        // The arrows walk whichever tree is showing. Both are views of the same
        // selection, so stepping in one moves the highlight in the other.
        if self.pane == Pane::Accessibility {
            return self.accessible_key(key);
        }

        let rows = document
            .map(|document| self.flatten(document))
            .unwrap_or_default();
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
                if let Some(document) = document {
                    self.scroll_to_selection(document);
                }
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
                } else if let Some(parent) =
                    document.and_then(|document| document.get(row.node)?.parent)
                {
                    self.selected = Some(parent);
                }
                Some(Action::None)
            }
            _ => None,
        }
    }

    /// The same keys, against the accessibility tree.
    ///
    /// Its own walk because its rows are its own, and the same rules because a
    /// tree is a tree: Right opens then steps in, Left closes then steps out.
    fn accessible_key(&mut self, key: Key) -> Option<Action> {
        let at = self.chosen_accessible();
        match key {
            Key::Escape => {
                if self.picking {
                    self.picking = false;
                } else {
                    self.open = false;
                }
                Some(Action::None)
            }
            Key::Down | Key::Up => {
                let next = match (at, key == Key::Down) {
                    (Some(at), true) => (at + 1).min(self.accessible.len().saturating_sub(1)),
                    (Some(at), false) => at.saturating_sub(1),
                    (None, _) => 0,
                };
                self.selected = self.accessible.get(next).and_then(|row| row.item.node);
                Some(Action::None)
            }
            Key::Right => {
                let at = at?;
                let row = self.accessible.get(at)?;
                let (folded, key, expandable) = (!row.row.expanded, row.key, row.row.expandable);
                if expandable && folded {
                    self.a11y_collapsed.remove(&key);
                } else {
                    self.selected = self.accessible.get(at + 1).and_then(|row| row.item.node);
                }
                Some(Action::None)
            }
            Key::Left => {
                let at = at?;
                let row = self.accessible.get(at)?;
                let (expanded, key, depth) = (row.row.expanded, row.key, row.row.depth);
                if expanded {
                    self.a11y_collapsed.insert(key);
                } else if let Some(parent) = self.accessible[..at]
                    .iter()
                    .rposition(|row| row.row.depth < depth)
                {
                    self.selected = self.accessible[parent].item.node;
                }
                Some(Action::None)
            }
            _ => None,
        }
    }

    /// Whether the search field holds the keyboard.
    fn searching(&self) -> bool {
        self.focus.kind(self.focused) == Some(crate::widget::FocusKind::Text)
    }

    /// Handle typed text. Returns whether the panel consumed it.
    pub fn text_input(&mut self, character: char) -> bool {
        if !self.open || !self.searching() {
            return false;
        }
        self.search.insert(character);
        true
    }

    /// Offer an event to the last frame's tree and apply what comes back.
    fn deliver(&mut self, event: &Event) -> Action {
        let action = self.offer(event);
        match &action {
            // A press in the field takes the keyboard as well as the caret:
            // the two are one gesture, and a caret that could not be typed at
            // would be a caret that lies.
            Action::SearchHit(hit) => {
                self.focused = self.focus.first_text();
                self.search.hit(*hit);
            }
            // A press anywhere else lets the field go, or typing would keep
            // landing in a field nobody is looking at.
            _ if *event == Event::PointerPressed => self.focused = None,
            _ => {}
        }
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
        let keep = self.matching(document);
        let mut rows = Vec::new();
        self.walk(document, document.root(), 0, keep.as_ref(), &mut rows);
        rows
    }

    /// Which nodes a search leaves standing, or `None` when there is no search.
    ///
    /// The matches and everything above them: a row with no parents is a row
    /// with no place in a tree, so an ancestor is kept for its descendant's
    /// sake even when it matches nothing itself.
    ///
    /// What is matched is the row *as the tree writes it*, which is why one
    /// field answers three questions: `div` finds the tag, `#main` finds the id
    /// and `.card` finds the class, because all three are in the text on screen.
    /// A second syntax for selectors would be a second thing to learn and a
    /// second thing to get wrong.
    fn matching(&self, document: &Document) -> Option<HashSet<NodeId>> {
        let query = self.search.text().trim().to_lowercase();
        if query.is_empty() {
            return None;
        }
        let mut keep = HashSet::new();
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            stack.extend(document.children(node));
            let Some(data) = document.get(node) else {
                continue;
            };
            if is_ignorable(document, node) {
                continue;
            }
            let (text, _) = self.label(document, node, &data.data, false);
            if !text.to_lowercase().contains(&query) {
                continue;
            }
            keep.insert(node);
            let mut parent = data.parent;
            while let Some(id) = parent {
                // Stop where an ancestor is already kept: everything above it
                // was walked the first time it was inserted.
                if !keep.insert(id) {
                    break;
                }
                parent = document.get(id).and_then(|node| node.parent);
            }
        }
        Some(keep)
    }

    fn walk(
        &self,
        document: &Document,
        node: NodeId,
        depth: usize,
        keep: Option<&HashSet<NodeId>>,
        out: &mut Vec<Row>,
    ) {
        let Some(data) = document.get(node) else {
            return;
        };
        let children: Vec<NodeId> = document.children(node).collect();
        let shown: Vec<NodeId> = children
            .iter()
            .copied()
            .filter(|child| !is_ignorable(document, *child))
            .filter(|child| keep.is_none_or(|keep| keep.contains(child)))
            .collect();

        let expandable = !shown.is_empty();
        // A filtered tree opens itself: what survives a search is a handful of
        // rows and their ancestors, and leaving those folded would show a
        // search that found something as a tree with nothing in it.
        let expanded = expandable && (keep.is_some() || self.expanded.contains(&node));
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
            self.walk(document, child, depth + 1, keep, out);
        }
    }

    /// The accessibility tree as rows, in the order they are drawn.
    ///
    /// Built from `a11y::describe_page`, which is the same walk the platform
    /// adapter hands to a real screen reader — so what this pane shows is what
    /// is actually exposed, and not a second opinion about it.
    fn flatten_accessible(&self, page: &crate::page::PageScene) -> Vec<AccessibleRow> {
        let mut out = Vec::new();
        for item in crate::a11y::describe_page(page) {
            self.walk_accessible(item, 0, &mut out);
        }
        out
    }

    fn walk_accessible(
        &self,
        mut item: crate::a11y::Accessible,
        depth: usize,
        out: &mut Vec<AccessibleRow>,
    ) {
        let children = std::mem::take(&mut item.children);
        let key = otlyra_layout::box_id_to_u64(item.box_id);
        let expandable = !children.is_empty();
        let expanded = expandable && !self.a11y_collapsed.contains(&key);
        // Text reads as content and everything else as structure, which is the
        // same distinction the DOM tree draws between a text node and a tag.
        let color = if item.value.is_some() {
            self.theme.ink
        } else {
            self.theme.code_tag
        };
        out.push(AccessibleRow {
            key,
            row: TreeRow {
                depth,
                text: item.spoken(),
                color,
                expandable,
                expanded,
            },
            item,
        });
        if !expanded {
            return;
        }
        for child in children {
            self.walk_accessible(child, depth + 1, out);
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
        // Only while it is being looked at: walking the box tree of a large page
        // for a pane nobody has open is work for nothing.
        let accessible = match (self.pane, facts.page) {
            (Pane::Accessibility, Some(page)) => self.flatten_accessible(page),
            _ => Vec::new(),
        };
        let appearance = Appearance {
            rect,
            nodes: rows.len(),
            accessible: accessible.len(),
            selected: self.selected,
            expanded: self.expanded.len(),
            collapsed: self.a11y_collapsed.len(),
            split: self.split,
            scroll: self.scroll,
            pane_scroll: self.pane_scroll,
            picking: self.picking,
            pane: self.pane,
            sidebar: self.sidebar,
            search: self.search.text().to_owned(),
            caret: self.searching().then(|| self.search.caret()),
            selection: self.searching().then(|| self.search.selection()).flatten(),
            level: self.level,
            // The count notices a request arriving; the settled count notices one
            // finishing. A row's status, timings and body are all filled at the
            // one moment it stops being pending, so a change in the second number
            // is a change in everything a drawn row is made of.
            exchanges: facts.exchanges.len(),
            settled: facts
                .exchanges
                .iter()
                .filter(|exchange| !matches!(exchange.status, crate::fetcher::Status::Pending))
                .count(),
            exchange: self.exchange,
            kind_filter: self.kind_filter,
            net_tab: self.net_tab,
            pointer: self.pointer,
            pointer_down: self.pointer_down,
            focus: self.focused,
        };
        if let Some((built, list)) = &self.cache
            && *built == appearance
            && self.root.is_some()
        {
            self.rows = rows;
            self.accessible = accessible;
            out.append(list);
            return;
        }

        self.builds += 1;
        self.rows = rows;
        self.accessible = accessible;
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
            // The tree, with the field that narrows it above and the path to
            // what is chosen along the bottom — which is where every browser
            // puts a breadcrumb trail, and it is full width because the path to
            // a deep node is longer than half a panel.
            Pane::Elements => Box::new(Stack::column(
                0.0,
                vec![
                    Box::new(Flex::new(
                        1.0,
                        Box::new(Split::row(
                            self.split,
                            Box::new(Padding::new(
                                Insets::all(theme.gap * 0.5),
                                Box::new(Stack::column(
                                    theme.gap * 0.5,
                                    vec![
                                        // A field stretches across a row and
                                        // would stretch down a column as well
                                        // if it were not told otherwise: the
                                        // height it wants is the one its own
                                        // control height gave it.
                                        Box::new(Flex::new(0.0, self.search_row(theme))),
                                        Box::new(Flex::new(1.0, self.tree(theme, facts))),
                                    ],
                                )),
                            )),
                            self.sidebar(theme, facts),
                            Action::SplitAt,
                        )),
                    )),
                    self.breadcrumbs(theme, facts),
                ],
            )),
            Pane::Console => Box::new(Padding::new(
                Insets::all(theme.gap),
                self.console_pane(theme),
            )),
            Pane::Network => Box::new(Padding::new(
                Insets::all(theme.gap),
                self.network_pane(theme, facts),
            )),
            // Split like *Elements*, and by the same value: the divider means
            // one thing on this panel — how much of it the tree gets — and a
            // second number for the same divider would be a second thing to
            // drag and to save.
            Pane::Accessibility => Box::new(Split::row(
                self.split,
                Box::new(Padding::new(
                    Insets::all(theme.gap * 0.5),
                    self.accessibility_tree(theme, facts),
                )),
                Box::new(Padding::new(
                    Insets::all(theme.gap),
                    self.accessibility_detail(theme),
                )),
                Action::SplitAt,
            )),
        };

        Box::new(Stack::column(
            0.0,
            vec![self.header(theme), Box::new(Flex::new(1.0, body))],
        ))
    }

    /// The field the tree is narrowed by.
    ///
    /// The same control the address bar and the history search are — one field,
    /// one set of rules about selection and the clipboard, and one place a bug
    /// in any of it can be.
    fn search_row(&self, theme: &Theme) -> Child<Action> {
        let id = self.focus.claim_text(true);
        let focused = self.focused == Some(id);
        controls::TextInput::new(
            controls::FieldView {
                text: self.search.text().to_owned(),
                caret: focused.then(|| self.search.caret()),
                selection: focused.then(|| self.search.selection()).flatten(),
                placeholder: "Find in the tree — a tag, #an-id, .a-class".to_owned(),
            },
            Action::SearchHit,
        )
        .face(theme.surface_sunken)
        .into_widget(theme)
    }

    /// The path from the root to what is chosen, each step pressable.
    ///
    /// Answers the question a scrolled tree cannot: *where in the page am I*.
    /// The crumbs report a node rather than a row, because an ancestor is not
    /// always a row — a fold, or a search, may have taken it off the list while
    /// leaving it every bit as much an ancestor.
    fn breadcrumbs(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        let (Some(document), Some(selected)) = (facts.document, self.selected) else {
            return Box::new(Gap::new(0.0, 0.0));
        };

        let mut path = vec![selected];
        let mut parent = document.get(selected).and_then(|node| node.parent);
        while let Some(node) = parent {
            path.push(node);
            parent = document.get(node).and_then(|node| node.parent);
        }
        path.reverse();

        let mut crumbs: Vec<Child<Action>> = Vec::new();
        for (index, node) in path.iter().copied().enumerate() {
            if index > 0 {
                crumbs.push(Box::new(Align::centre(Box::new(Label::new(
                    "\u{203a}",
                    theme.font_size_small,
                    theme.ink_dim,
                )))));
            }
            let Some(data) = document.get(node) else {
                continue;
            };
            let (text, _) = self.label(document, node, &data.data, true);
            let last = index + 1 == path.len();
            let crumb: Child<Action> = Box::new(Align::centre(Box::new(
                Mono::new(
                    crumb_text(&text),
                    if last { theme.ink } else { theme.ink_dim },
                )
                .size(theme.font_size_small),
            )));
            crumbs.push(Box::new(
                crate::widget::Button::new(
                    Action::Choose(node),
                    Box::new(
                        Background::new(
                            Theme::CLEAR,
                            theme.radius_small,
                            Box::new(Padding::new(Insets::symmetric(theme.gap * 0.5, 0.0), crumb)),
                        )
                        .on_hover(theme.hover),
                    ),
                )
                .focus(self.focus.claim(true)),
            ));
        }

        Box::new(Fixed::height(
            theme.row_height + 2.0,
            Box::new(Background::new(
                theme.surface,
                0.0,
                // Cut where it runs out rather than squeezed: a path to a deep
                // node is longer than any panel, and the end of it — which is
                // where the selection is — is the half worth keeping, so the
                // row is aligned to its right.
                Box::new(crate::widget::Clip::new(Box::new(Align::right(Box::new(
                    Stack::row(0.0, crumbs),
                ))))),
            )),
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

        // An icon rather than a word, and on the left rather than after the
        // tabs: this is where every browser's devtools put the picker, and a
        // person who has used one already knows both the shape and the corner
        // it lives in. A label reading "Pick" would be a word to read and a word
        // to translate for the same meaning.
        let picker: Child<Action> = Box::new(Align::centre(controls::icon_button(
            theme,
            &self.focus,
            Action::TogglePicker,
            true,
            "Pick an element",
            crate::widget::icon::picker,
        )));
        // Armed, it is lit, because a mode with no sign that it is on is a mode
        // that surprises the next click.
        let picker: Child<Action> = if self.picking {
            Box::new(Background::new(theme.selection, theme.radius_small, picker))
        } else {
            picker
        };

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
                            picker,
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
                            Box::new(Align::centre(controls::icon_button(
                                theme,
                                &self.focus,
                                Action::Close,
                                true,
                                "Close the inspector",
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
        let all = journal.records();
        let said = all.len();
        let rows: Vec<Vec<String>> = all
            .into_iter()
            .rev()
            .filter(|record| self.level.admits(record.level))
            .take(200)
            .map(|record| vec![record.level.to_string(), record.target, record.message])
            .collect();

        // The filter, and the way to start again. Both across the top, because
        // a console's own controls are not lines of the log.
        let controls_row: Child<Action> = Box::new(Stack::row(
            theme.gap,
            vec![
                Box::new(Align::centre(controls::segmented(
                    theme,
                    &self.focus,
                    Level::ALL
                        .iter()
                        .map(|level| (level.label().to_owned(), Action::ShowLevel(*level)))
                        .collect(),
                    Level::ALL
                        .iter()
                        .position(|level| *level == self.level)
                        .unwrap_or(0),
                ))),
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                Box::new(Align::centre(controls::button(
                    theme,
                    &self.focus,
                    Action::ClearConsole,
                    "Clear",
                    controls::Emphasis::Normal,
                    said > 0,
                ))),
            ],
        ));

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
                    controls_row,
                    note,
                    Box::new(Label::new(
                        // What is missing and why are two different messages: a
                        // filter hiding everything looks exactly like a silent
                        // browser, and the person who set the filter is the one
                        // least likely to remember they did.
                        if said > 0 {
                            "Nothing at this level. Try All."
                        } else {
                            "Nothing said yet. `OTLYRA_LOG=debug` is how to hear more."
                        },
                        theme.font_size_small,
                        theme.ink_dim,
                    )),
                ],
            ));
        }

        Box::new(Stack::column(
            theme.gap,
            vec![
                controls_row,
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
    ///
    /// A list on the left and, once a request is chosen, its detail on the right
    /// — the shape Firefox settled on, because a request has more to say than a
    /// row can hold and a page has more requests than a column of detail could
    /// stack. The two share the panel's one divider.
    fn network_pane(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        if facts.exchanges.is_empty() {
            return Box::new(Align::centre(Box::new(Label::new(
                "Nothing has been asked for in this tab.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        }

        // Newest first, and only the kinds the filter admits. Numbered by their
        // own id rather than their place in the list, so a press reports the
        // request and not the row — the list under it may have been filtered or
        // a new request may have pushed it down between frames.
        let shown: Vec<&crate::fetcher::Exchange> = facts
            .exchanges
            .iter()
            .rev()
            .filter(|exchange| self.kind_filter.admits(exchange.kind))
            .collect();

        // One scale for every bar, so two rows drawn the same length took the
        // same time. The longest wall-clock any request took is the full width;
        // a bar is that request's share of it.
        let scale = shown
            .iter()
            .filter_map(|exchange| exchange.waited.or(exchange.took))
            .map(|took| took.as_secs_f64())
            .fold(0.0_f64, f64::max)
            .max(f64::MIN_POSITIVE);

        let filter: Child<Action> = Box::new(Fixed::height(
            theme.control_height + theme.gap,
            Box::new(Padding::new(
                Insets::symmetric(0.0, theme.gap * 0.5),
                Box::new(Stack::row(
                    theme.gap,
                    vec![
                        Box::new(Align::centre(controls::segmented(
                            theme,
                            &self.focus,
                            KindFilter::ALL
                                .iter()
                                .map(|kind| (kind.label().to_owned(), Action::ShowKind(*kind)))
                                .collect(),
                            KindFilter::ALL
                                .iter()
                                .position(|kind| *kind == self.kind_filter)
                                .unwrap_or(0),
                        ))),
                        Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                        Box::new(Align::centre(Box::new(Label::new(
                            format!("{} of {}", shown.len(), facts.exchanges.len()),
                            theme.font_size_small,
                            theme.ink_dim,
                        )))),
                    ],
                )),
            )),
        ));

        let header = net_row(theme, None, scale, false);
        let mut rows: Vec<Child<Action>> = Vec::with_capacity(shown.len());
        for exchange in &shown {
            let id = exchange.id;
            let chosen = self.exchange == Some(id);
            rows.push(Box::new(crate::widget::Button::new(
                Action::SelectExchange(id),
                net_row(theme, Some(exchange), scale, chosen),
            )));
        }
        // Air under the last request, so the end of the list reads as the end
        // rather than as a row cut off by the panel's edge.
        rows.push(Box::new(Gap::new(0.0, theme.gap)));

        let list: Child<Action> = Box::new(Stack::column(
            0.0,
            vec![
                filter,
                header,
                Box::new(Flex::new(
                    1.0,
                    Box::new(crate::widget::Scroll::new(
                        self.scroll,
                        std::rc::Rc::clone(&self.overflow),
                        Box::new(Stack::column(0.0, rows)),
                    )),
                )),
            ],
        ));

        // Without a choice, the list is the whole pane; with one, the detail
        // takes the right of the same divider Elements uses.
        match self
            .exchange
            .and_then(|id| shown.iter().find(|exchange| exchange.id == id).copied())
        {
            Some(exchange) => Box::new(Split::row(
                self.split,
                list,
                Box::new(Padding::new(
                    Insets::all(theme.gap),
                    self.network_detail(theme, exchange),
                )),
                Action::SplitAt,
            )),
            None => list,
        }
    }

    /// The chosen request, taken apart: its headers, its body, or its timings.
    fn network_detail(&self, theme: &Theme, exchange: &crate::fetcher::Exchange) -> Child<Action> {
        let tabs: Child<Action> = Box::new(Fixed::height(
            theme.control_height + theme.gap,
            Box::new(Padding::new(
                Insets::symmetric(0.0, theme.gap * 0.5),
                Box::new(Align::left(controls::segmented(
                    theme,
                    &self.focus,
                    NetTab::ALL
                        .iter()
                        .map(|tab| (tab.label().to_owned(), Action::ShowNetTab(*tab)))
                        .collect(),
                    NetTab::ALL
                        .iter()
                        .position(|tab| *tab == self.net_tab)
                        .unwrap_or(0),
                ))),
            )),
        ));

        let body: Child<Action> = match self.net_tab {
            NetTab::Headers => self.headers_tab(theme, exchange),
            NetTab::Response => self.response_tab(theme, exchange),
            NetTab::Timings => self.timings_tab(theme, exchange),
        };

        Box::new(Stack::column(
            0.0,
            vec![tabs, Box::new(Flex::new(1.0, body))],
        ))
    }

    /// The request and response headers, one table each.
    fn headers_tab(&self, _theme: &Theme, exchange: &crate::fetcher::Exchange) -> Child<Action> {
        let general = vec![
            vec!["Request URL".to_owned(), exchange.url.clone()],
            vec!["Method".to_owned(), exchange.method.to_owned()],
            vec!["Status".to_owned(), status_line(exchange)],
        ];
        let mut rows: Vec<Vec<String>> = general;
        rows.push(vec![String::new(), String::new()]);
        rows.push(vec![
            "\u{2014} Response headers \u{2014}".to_owned(),
            String::new(),
        ]);
        if exchange.response_headers.is_empty() {
            rows.push(vec!["(none recorded)".to_owned(), String::new()]);
        }
        for (name, value) in &exchange.response_headers {
            rows.push(vec![name.clone(), value.clone()]);
        }
        rows.push(vec![String::new(), String::new()]);
        rows.push(vec![
            "\u{2014} Request headers \u{2014}".to_owned(),
            String::new(),
        ]);
        if exchange.request_headers.is_empty() {
            rows.push(vec!["(none recorded)".to_owned(), String::new()]);
        }
        for (name, value) in &exchange.request_headers {
            rows.push(vec![name.clone(), value.clone()]);
        }

        Box::new(Table::new(
            vec!["header".to_owned(), "value".to_owned()],
            rows,
            self.pane_scroll,
            std::rc::Rc::clone(&self.pane_overflow),
        ))
    }

    /// The body that came back, as text or as a picture.
    fn response_tab(&self, theme: &Theme, exchange: &crate::fetcher::Exchange) -> Child<Action> {
        if exchange.body.is_empty() {
            return Box::new(Align::centre(Box::new(Label::new(
                match exchange.status {
                    crate::fetcher::Status::Pending => "Still loading.",
                    _ => "No body was kept for this request.",
                },
                theme.font_size_small,
                theme.ink_dim,
            ))));
        }

        // A picture is shown as one; text is shown as text; anything else is
        // named rather than spilled as bytes nobody can read.
        let content_type = exchange.content_type.as_deref().unwrap_or_default();
        if content_type.starts_with("image/") {
            return self.image_preview(theme, exchange);
        }
        if !is_texty(content_type, &exchange.body) {
            return Box::new(Align::centre(Box::new(Label::new(
                format!(
                    "{} of {}, not shown as text.",
                    exchange
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "binary".to_owned()),
                    match &exchange.status {
                        crate::fetcher::Status::Ok(bytes) => bytes_read(*bytes),
                        _ => bytes_read(exchange.body.len()),
                    }
                ),
                theme.font_size_small,
                theme.ink_dim,
            ))));
        }

        let text = String::from_utf8_lossy(&exchange.body);
        let mut lines: Vec<Child<Action>> = Vec::new();
        if !exchange.body_complete {
            lines.push(Box::new(Label::new(
                "Showing the first part of the body.",
                theme.font_size_small,
                theme.ink_dim,
            )));
        }
        // One monospace line per line of the body, cut where the pane ends. A
        // body of ten thousand lines is a body the pane scrolls through, and
        // only what shows is drawn — the same rule the tables keep.
        for line in text.lines().take(2000) {
            lines.push(Box::new(Mono::new(cut(line, 400), theme.code_value)));
        }
        // The same air under the last line as under the last request.
        lines.push(Box::new(Gap::new(0.0, theme.gap)));
        Box::new(crate::widget::Scroll::new(
            self.pane_scroll,
            std::rc::Rc::clone(&self.pane_overflow),
            Box::new(Stack::column(0.0, lines)),
        ))
    }

    /// The picture the request returned, drawn to fit.
    fn image_preview(&self, theme: &Theme, exchange: &crate::fetcher::Exchange) -> Child<Action> {
        let Ok(picture) = otlyra_gfx::decode_image(&exchange.body) else {
            return Box::new(Align::centre(Box::new(Label::new(
                "The picture did not decode.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        };
        let (width, height) = (picture.width as f64, picture.height as f64);
        let caption = format!("{} \u{00d7} {} px", picture.width, picture.height);
        Box::new(Stack::column(
            theme.gap,
            vec![
                Box::new(Label::new(caption, theme.font_size_small, theme.ink_dim)),
                Box::new(Flex::new(
                    1.0,
                    Box::new(crate::widget::Painted::new(
                        0.0,
                        0.0,
                        move |rect, _cx, list| {
                            if width <= 0.0 || height <= 0.0 {
                                return;
                            }
                            // Fit inside the pane, never enlarged past life size: a
                            // favicon shown at panel width would be a wall of one
                            // colour, and the point of a preview is to recognise it.
                            let scale = (rect.width / width)
                                .min(rect.height / height)
                                .clamp(0.0, 1.0);
                            let w = width * scale;
                            let x = rect.x + (rect.width - w) / 2.0;
                            let scale = scale.max(f64::MIN_POSITIVE);
                            list.push(otlyra_gfx::DisplayItem::Image {
                                image: picture.clone().into(),
                                sampler: otlyra_gfx::peniko::ImageSampler::default(),
                                transform: otlyra_gfx::kurbo::Affine::translate((x, rect.y))
                                    * otlyra_gfx::kurbo::Affine::scale(scale),
                                clip_rect: None,
                            });
                        },
                    )),
                )),
            ],
        ))
    }

    /// How long each part of the request took, as bars and as numbers.
    fn timings_tab(&self, _theme: &Theme, exchange: &crate::fetcher::Exchange) -> Child<Action> {
        let waited = exchange
            .waited
            .map(millis)
            .unwrap_or_else(|| "\u{2014}".to_owned());
        let took = exchange
            .took
            .map(millis)
            .unwrap_or_else(|| "\u{2014}".to_owned());
        let queue = match (exchange.waited, exchange.took) {
            (Some(waited), Some(took)) => millis(waited.saturating_sub(took)),
            _ => "\u{2014}".to_owned(),
        };
        let rows = vec![
            // Waited and took answer different questions and neither stands in
            // for the other: one is how long from the ask, the other is how slow
            // the transport itself was, and the gap between them is the queue.
            vec!["Waited (ask to done)".to_owned(), waited],
            vec!["Queued (before transport)".to_owned(), queue],
            vec!["Transport (took)".to_owned(), took],
        ];
        Box::new(Table::new(
            vec!["stage".to_owned(), "time".to_owned()],
            rows,
            self.pane_scroll,
            std::rc::Rc::clone(&self.pane_overflow),
        ))
    }

    /// The page as a screen reader is handed it.
    fn accessibility_tree(&self, theme: &Theme, facts: &Facts<'_>) -> Child<Action> {
        if facts.page.is_none() {
            return Box::new(Align::centre(Box::new(Label::new(
                "Nothing is loaded in this tab.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        }
        if self.accessible.is_empty() {
            return Box::new(Align::centre(Box::new(Label::new(
                "This page exposes nothing a reader could walk.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        }
        Box::new(
            Tree::new(
                self.accessible.iter().map(|row| row.row.clone()).collect(),
                self.scroll,
                std::rc::Rc::clone(&self.overflow),
                Action::SelectAccessible,
                Action::ToggleAccessible,
            )
            .selected(self.chosen_accessible()),
        )
    }

    /// Which accessible row the chosen node is, if it is one of them.
    ///
    /// Derived rather than stored: the selection is the node, and both trees are
    /// views of it.
    fn chosen_accessible(&self) -> Option<usize> {
        self.selected?;
        self.accessible
            .iter()
            .position(|row| row.item.node.is_some() && row.item.node == self.selected)
    }

    /// What a reader would say about the chosen row, and what it is made of.
    fn accessibility_detail(&self, theme: &Theme) -> Child<Action> {
        let Some(row) = self
            .chosen_accessible()
            .and_then(|at| self.accessible.get(at))
        else {
            return Box::new(Align::centre(Box::new(Label::new(
                "Choose something in the tree.",
                theme.font_size_small,
                theme.ink_dim,
            ))));
        };

        let mut facts: Vec<Vec<String>> = vec![vec![
            "role".to_owned(),
            crate::a11y::role_word(row.item.role).to_owned(),
        ]];
        if let Some(value) = &row.item.value {
            facts.push(vec!["text".to_owned(), value.clone()]);
        }
        if let Some(level) = row.item.level {
            facts.push(vec!["level".to_owned(), level.to_string()]);
        }
        if let Some(url) = &row.item.url {
            facts.push(vec!["goes to".to_owned(), url.clone()]);
        }
        if let Some(bounds) = row.item.bounds {
            facts.push(vec![
                "at".to_owned(),
                format!(
                    "{} × {} at ({}, {})",
                    round(bounds.width()),
                    round(bounds.height()),
                    round(bounds.x0),
                    round(bounds.y0)
                ),
            ]);
        }
        facts.push(vec![
            "in the DOM".to_owned(),
            // The link back to the other tree, said in words: an anonymous box
            // is exposed to a reader without any node of its own to point at.
            match row.item.node {
                Some(_) => "the node chosen in Elements".to_owned(),
                None => "no node — an anonymous box".to_owned(),
            },
        ]);

        Box::new(Stack::column(
            theme.gap,
            vec![
                Box::new(Label::new(
                    "A reader announces:",
                    theme.font_size_small,
                    theme.ink_dim,
                )),
                Box::new(Mono::new(row.item.spoken(), theme.code_value)),
                Box::new(Flex::new(
                    1.0,
                    Box::new(Table::new(
                        vec!["what".to_owned(), "it is".to_owned()],
                        facts,
                        self.pane_scroll,
                        std::rc::Rc::clone(&self.pane_overflow),
                    )),
                )),
            ],
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

/// One row of the network list, or its header when `exchange` is `None`.
///
/// Drawn as a leaf rather than a stack of cells, for the reason the tree and the
/// table are: the columns are arithmetic against the row's own width, so nothing
/// is measured, placed or searched — one row costs the handful of fills and one
/// text run per cell that show. The waterfall bar is the same idea: two rects
/// whose lengths are the two timings against one scale.
fn net_row(
    theme: &Theme,
    exchange: Option<&crate::fetcher::Exchange>,
    scale: f64,
    chosen: bool,
) -> Child<Action> {
    let theme = theme.clone();
    let exchange = exchange.cloned();
    let height = theme.row_height;
    Box::new(crate::widget::Painted::new(
        0.0,
        height,
        move |rect, cx, list| {
            let is_header = exchange.is_none();
            if chosen {
                fill_rounded(list, rect, theme.selection, 0.0);
            } else if is_header {
                fill_rounded(list, rect, theme.surface, 0.0);
            } else if cx.hovered(rect) {
                fill_rounded(list, rect, theme.hover, 0.0);
            }

            let size = theme.font_size_small;
            let line = cx.line_height(size);
            let top = rect.y + (rect.height - line) / 2.0;
            let pad = theme.gap;
            // Fixed columns from the left, the address taking what is left before a
            // fixed bar on the right.
            let code_w = 44.0;
            let method_w = 40.0;
            let kind_w = 56.0;
            let size_w = 60.0;
            let bar_w = 96.0;
            let mut x = rect.x + pad;

            // Each cell clipped to its own column, so a long address stops at the
            // size beside it rather than drawing over it: a table's columns are
            // only columns if what is in one cannot spill into the next.
            let cell = |list: &mut DisplayList, cx: &mut Cx, text: &str, at: Rect, color: Color| {
                list.push(otlyra_gfx::DisplayItem::PushLayer {
                    blend: otlyra_gfx::peniko::BlendMode::default(),
                    alpha: 1.0,
                    transform: otlyra_gfx::kurbo::Affine::IDENTITY,
                    clip: otlyra_gfx::kurbo::Shape::to_path(&at.to_kurbo(), 0.1),
                });
                let mut mono = Mono::new(text, color).size(size);
                crate::widget::Widget::<Action>::place(&mut mono, at, cx);
                crate::widget::Widget::<Action>::draw(&mut mono, cx, list);
                list.push(otlyra_gfx::DisplayItem::PopLayer);
            };

            let ink = if is_header { theme.ink_dim } else { theme.ink };
            let (code, method, kind, size_text, name, bar_ink) = match &exchange {
                None => (
                    "status".to_owned(),
                    "meth".to_owned(),
                    "kind".to_owned(),
                    "size".to_owned(),
                    "address".to_owned(),
                    theme.ink_dim,
                ),
                Some(exchange) => {
                    let (code, tone) = status_cell(&theme, exchange);
                    (
                        code,
                        exchange.method.to_owned(),
                        kind_short(exchange.kind).to_owned(),
                        match &exchange.status {
                            crate::fetcher::Status::Ok(bytes) => bytes_read(*bytes),
                            crate::fetcher::Status::Pending => String::new(),
                            crate::fetcher::Status::Failed(_) => String::new(),
                        },
                        short_url(&exchange.url),
                        tone,
                    )
                }
            };

            cell(list, cx, &code, Rect::new(x, top, code_w, line), bar_ink);
            x += code_w;
            cell(list, cx, &method, Rect::new(x, top, method_w, line), ink);
            x += method_w;
            cell(list, cx, &kind, Rect::new(x, top, kind_w, line), ink);
            x += kind_w;

            let name_w = (rect.x + rect.width - pad - bar_w - size_w - x).max(0.0);
            cell(list, cx, &name, Rect::new(x, top, name_w, line), ink);
            x += name_w;
            cell(
                list,
                cx,
                &size_text,
                Rect::new(x, top, size_w, line),
                theme.ink_dim,
            );
            x += size_w;

            // The bar: the far right column. A header labels it; a row draws its
            // two timings, the queue wait leading and the transport trailing, so
            // which of the two dominates is the shape of the bar and not a number to
            // compare.
            let bar = Rect::new(x, rect.y + (rect.height - 6.0) / 2.0, bar_w, 6.0);
            match &exchange {
                None => cell(
                    list,
                    cx,
                    "timing",
                    Rect::new(x, top, bar_w, line),
                    theme.ink_dim,
                ),
                Some(exchange) => {
                    let full = exchange
                        .waited
                        .or(exchange.took)
                        .map(|took| took.as_secs_f64())
                        .unwrap_or(0.0);
                    if full > 0.0 {
                        let took = exchange.took.map(|took| took.as_secs_f64()).unwrap_or(full);
                        let queue = (full - took).max(0.0);
                        let total_w = (full / scale * bar_w).min(bar_w);
                        let queue_w = (queue / scale * bar_w).min(total_w);
                        if queue_w > 0.0 {
                            fill_rounded(
                                list,
                                Rect::new(bar.x, bar.y, queue_w, bar.height),
                                theme.ink_dim,
                                1.0,
                            );
                        }
                        fill_rounded(
                            list,
                            Rect::new(
                                bar.x + queue_w,
                                bar.y,
                                (total_w - queue_w).max(0.0),
                                bar.height,
                            ),
                            theme.accent,
                            1.0,
                        );
                    }
                }
            }

            if is_header {
                controls::hairline(
                    &theme,
                    list,
                    Rect::new(rect.x, rect.y + rect.height - 1.0, rect.width, 1.0),
                );
            }
        },
    ))
}

/// A resource kind in the width a column has for it.
///
/// The same short words the filter offers, so the list and the control that
/// narrows it call a thing by one name — and short enough that `stylesheet` does
/// not run into the address beside it.
fn kind_short(kind: crate::fetcher::ResourceKind) -> &'static str {
    use crate::fetcher::ResourceKind;
    match kind {
        ResourceKind::Document => "doc",
        ResourceKind::Stylesheet => "css",
        ResourceKind::Image => "img",
    }
}

/// The status code and the colour it earns.
///
/// The ordinary ink for a request that arrived with the page asked for, and the
/// warning colour for one that failed or came back an error — the two tones an
/// inspector needs so a wall of requests shows the bad news as a shape before it
/// is read as numbers. No third colour, because the palette has one warning and
/// inventing a second would be a shade to keep to a contrast floor for one pane.
fn status_cell(theme: &Theme, exchange: &crate::fetcher::Exchange) -> (String, Color) {
    match (&exchange.status, exchange.code) {
        (crate::fetcher::Status::Pending, _) => {
            ("\u{2022}\u{2022}\u{2022}".to_owned(), theme.ink_dim)
        }
        (crate::fetcher::Status::Failed(_), _) => ("fail".to_owned(), theme.danger),
        (crate::fetcher::Status::Ok(_), Some(code)) => (
            code.to_string(),
            if code < 400 { theme.ink } else { theme.danger },
        ),
        // A `file:` load succeeded and has no code; a dash rather than a made-up
        // number, in the ordinary ink.
        (crate::fetcher::Status::Ok(_), None) => ("\u{2014}".to_owned(), theme.ink),
    }
}

/// The status as a sentence, for the detail pane.
fn status_line(exchange: &crate::fetcher::Exchange) -> String {
    match (&exchange.status, exchange.code) {
        (crate::fetcher::Status::Pending, _) => "pending".to_owned(),
        (crate::fetcher::Status::Failed(error), _) => format!("failed \u{2014} {error}"),
        (crate::fetcher::Status::Ok(_), Some(code)) => code.to_string(),
        (crate::fetcher::Status::Ok(_), None) => "200 (no HTTP status)".to_owned(),
    }
}

/// The tail of a URL — the file, and enough of the path to tell two apart.
///
/// A list of full addresses is a list a person reads the same left edge of forty
/// times; the name is where they differ.
fn short_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let tail = without_query.rsplit('/').next().unwrap_or(without_query);
    if tail.is_empty() {
        // A directory URL ending in a slash: show the host rather than nothing.
        url.split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(url)
            .to_owned()
    } else {
        tail.to_owned()
    }
}

/// Whether a body is worth showing as text.
///
/// The declared type first — `text/*`, and the structured types that are text
/// underneath like JSON, XML and SVG — and, when the server said nothing, a
/// glance at the bytes: a body that is valid UTF-8 with no NULs reads as text,
/// and one with a NUL in the first stretch is binary that would fill the pane
/// with replacement characters.
fn is_texty(content_type: &str, body: &[u8]) -> bool {
    let content_type = content_type.split(';').next().unwrap_or("").trim();
    if content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/json"
                | "application/javascript"
                | "application/xml"
                | "application/xhtml+xml"
                | "image/svg+xml"
        )
        || content_type.ends_with("+json")
        || content_type.ends_with("+xml")
    {
        return true;
    }
    if !content_type.is_empty() {
        return false;
    }
    let head = &body[..body.len().min(1024)];
    !head.contains(&0) && std::str::from_utf8(head).is_ok()
}

/// A row's label, as short as a crumb can be and still name the element.
///
/// `<div #one .a.b>` reads as `div#one.a.b`: the same thing a stylesheet would
/// call it, which is shorter than the tree's spelling and no less exact.
fn crumb_text(label: &str) -> String {
    let bare = label.trim_start_matches('<').trim_end_matches('>');
    cut(bare.replace(" #", "#").replace(" .", ".").trim(), 24)
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
pub fn describe(style: &otlyra_css::ComputedStyle) -> Vec<(&'static str, String)> {
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
///
/// Spelled the way a stylesheet spells it rather than the way the engine stores
/// it. A pane — or a driver reading this over the protocol — that reported
/// `fixed(px(200.0))` would be reporting a value nobody can put back into a
/// stylesheet, which is most of what a computed value is for.
fn tracks(template: &[otlyra_css::Track], fill: Option<&[otlyra_css::Track]>) -> String {
    let one = |track: &otlyra_css::Track| match track {
        otlyra_css::Track::Fixed(otlyra_css::Length::Px(px)) => format!("{px}px"),
        otlyra_css::Track::Fixed(otlyra_css::Length::Percent(fraction)) => {
            format!("{}%", fraction * 100.0)
        }
        otlyra_css::Track::Fraction(share) => format!("{share}fr"),
        otlyra_css::Track::Auto => "auto".to_owned(),
    };
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
    /// The edges layout actually used, which is the better answer whenever
    /// there is one: `auto` is resolved and percentages are already numbers.
    pub fn used(used: otlyra_layout::UsedEdges) -> Self {
        let sides = |sides: otlyra_css::Sides<f32>| {
            (
                f64::from(sides.left),
                f64::from(sides.top),
                f64::from(sides.right),
                f64::from(sides.bottom),
            )
        };
        Self {
            margin: sides(used.margin),
            border: sides(used.border),
            padding: sides(used.padding),
        }
    }

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

    use crate::clipboard::Clipboard;

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
            page: None,
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
            page: None,
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

    /// A laid-out page, which is what the accessibility pane is a view of.
    fn page(html: &str) -> crate::page::PageScene {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let mut page = crate::page::PageScene::new(parsed.document);
        let mut text = TextEngine::isolated();
        let _ = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        page
    }

    /// One frame with a page under it, for the panes that read the box tree.
    fn frame_with_page(inspector: &mut Inspector, page: &crate::page::PageScene) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let facts = Facts {
            document: Some(page.document()),
            page: Some(page),
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

    /// One frame with a network list under it.
    fn frame_with_exchanges(inspector: &mut Inspector, exchanges: &[crate::fetcher::Exchange]) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let facts = Facts {
            document: None,
            page: None,
            style: None,
            rect: None,
            containing: None,
            exchanges,
        };
        inspector.build_display_list(
            Rect::new(0.0, 300.0, 900.0, 300.0),
            &facts,
            &mut text,
            &mut list,
        );
    }

    /// A 404 that returned a body is `ok` to the transport and an error to the
    /// reader: the code carries what the `Ok`/`Failed` split cannot.
    #[test]
    fn a_status_code_survives_a_transport_that_succeeded() {
        use crate::fetcher::{Exchange, ResourceKind, Status};
        let mut missing =
            Exchange::for_test(1, ResourceKind::Document, "https://x/y", Status::Ok(20));
        missing.code = Some(404);
        let (cell, colour) = status_cell(&Theme::light(), &missing);
        assert_eq!(cell, "404");
        assert_eq!(colour, Theme::light().danger, "an error code reads as one");

        let mut ok = Exchange::for_test(2, ResourceKind::Image, "https://x/z", Status::Ok(20));
        ok.code = Some(200);
        assert_eq!(status_cell(&Theme::light(), &ok).1, Theme::light().ink);
    }

    /// The filter shows only the kinds it admits, and the list is what a press
    /// reports against.
    #[test]
    fn the_network_filter_narrows_the_list() {
        use crate::fetcher::{Exchange, ResourceKind, Status};
        let exchanges = vec![
            Exchange::for_test(1, ResourceKind::Document, "https://x/", Status::Ok(9)),
            Exchange::for_test(
                2,
                ResourceKind::Stylesheet,
                "https://x/a.css",
                Status::Ok(9),
            ),
            Exchange::for_test(3, ResourceKind::Image, "https://x/p.png", Status::Ok(9)),
        ];
        assert!(KindFilter::Images.admits(ResourceKind::Image));
        assert!(!KindFilter::Images.admits(ResourceKind::Stylesheet));
        assert!(KindFilter::All.admits(ResourceKind::Document));

        let mut inspector = panel();
        inspector.apply(Action::Show(Pane::Network));
        inspector.apply(Action::ShowKind(KindFilter::Stylesheets));
        // A press on a filtered-out request cannot be reported, because it is
        // not drawn: selection is by id and the id has to be in the shown list.
        inspector.apply(Action::SelectExchange(3));
        frame_with_exchanges(&mut inspector, &exchanges);
        // The chosen id is not among the shown, so no detail opens: the split is
        // absent and the pane is the list alone.
        assert_eq!(inspector.kind_filter, KindFilter::Stylesheets);
    }

    /// Choosing a request opens its detail; the two share the panel's divider.
    #[test]
    fn choosing_a_request_opens_its_detail() {
        use crate::fetcher::{Exchange, ResourceKind, Status};
        let mut css = Exchange::for_test(
            1,
            ResourceKind::Stylesheet,
            "https://x/a.css",
            Status::Ok(40),
        );
        css.code = Some(200);
        css.content_type = Some("text/css".to_owned());
        css.body = b"body { color: red; }".to_vec();
        css.response_headers = vec![("content-type".to_owned(), "text/css".to_owned())];

        let mut inspector = panel();
        inspector.apply(Action::Show(Pane::Network));
        inspector.apply(Action::SelectExchange(1));
        assert_eq!(inspector.exchange, Some(1));
        // Every tab of the detail builds without a document — a failed tab still
        // has a request to take apart.
        for tab in NetTab::ALL {
            inspector.apply(Action::ShowNetTab(tab));
            frame_with_exchanges(&mut inspector, std::slice::from_ref(&css));
        }
    }

    /// A body is shown as text when it reads as text, and named otherwise.
    #[test]
    fn a_text_body_is_text_and_bytes_are_not() {
        assert!(is_texty("text/html", b""));
        assert!(is_texty("application/json", b"{}"));
        assert!(is_texty("image/svg+xml", b"<svg/>"));
        assert!(!is_texty("image/png", &[0x89, 0x50, 0x4e, 0x47]));
        // No declared type: the bytes decide, and a NUL says binary.
        assert!(is_texty("", b"hello, plain text"));
        assert!(!is_texty("", &[0, 1, 2, 3]));
    }

    /// The name column is the tail of the URL, which is where two requests to
    /// one host differ.
    #[test]
    fn the_name_is_the_tail_of_the_address() {
        assert_eq!(
            short_url("https://x.example/assets/main.css?v=2"),
            "main.css"
        );
        assert_eq!(short_url("https://x.example/"), "x.example");
        assert_eq!(short_url("https://x.example/a/b/c.png"), "c.png");
    }

    /// The pane is a view of the tree a reader is actually handed — the same
    /// walk, not a second opinion about it.
    #[test]
    fn the_accessibility_pane_says_what_a_reader_would_hear() {
        let page = page("<body><h2>A heading</h2><p>Some <a href=\"/x\">link</a> text");
        let mut inspector = panel();
        inspector.apply(Action::Show(Pane::Accessibility));
        frame_with_page(&mut inspector, &page);

        let said: Vec<&str> = inspector
            .accessible
            .iter()
            .map(|row| row.row.text.as_str())
            .collect();
        assert!(
            said.contains(&"heading level 2"),
            "a heading says its level: {said:?}"
        );
        assert!(
            said.iter().any(|row| row.starts_with("link")),
            "a link says where it goes: {said:?}"
        );
        assert!(
            said.contains(&"text, \u{201c}A heading\u{201d}"),
            "and the words themselves are read: {said:?}"
        );
    }

    /// One selection, two views of it: choosing in either tree chooses the node.
    #[test]
    fn choosing_in_the_accessibility_tree_chooses_the_node_itself() {
        let page = page("<body><h2>A heading</h2><p>text");
        let mut inspector = panel();
        inspector.apply(Action::Show(Pane::Accessibility));
        frame_with_page(&mut inspector, &page);

        let heading = inspector
            .accessible
            .iter()
            .position(|row| row.row.text.starts_with("heading"))
            .expect("the page exposes a heading");
        inspector.apply(Action::SelectAccessible(heading));

        let node = inspector.selected.expect("a node was chosen");
        // The same node the DOM tree would show, reached through the box the
        // reader's tree was built from.
        assert_eq!(
            page.boxes().box_for(node),
            Some(inspector.accessible[heading].item.box_id)
        );
        assert_eq!(inspector.chosen_accessible(), Some(heading));
    }

    /// Folding a branch away takes its children off the list with it.
    #[test]
    fn a_branch_folds_and_what_was_under_it_goes_with_it() {
        let page = page("<body><ul><li>one</li><li>two</li></ul>");
        let mut inspector = panel();
        inspector.apply(Action::Show(Pane::Accessibility));
        frame_with_page(&mut inspector, &page);
        let before = inspector.accessible.len();

        let list = inspector
            .accessible
            .iter()
            .position(|row| row.row.text.starts_with("list"))
            .expect("the page exposes a list");
        inspector.apply(Action::ToggleAccessible(list));
        frame_with_page(&mut inspector, &page);

        assert!(
            inspector.accessible.len() < before,
            "folding a branch takes its children with it: {before} rows before, {} after",
            inspector.accessible.len()
        );
    }

    /// One field answers three questions, because the tree's own spelling of a
    /// row already carries the tag, the id and the classes.
    #[test]
    fn a_search_narrows_the_tree_to_what_matches_and_what_holds_it() {
        let document = document();
        let mut inspector = panel();

        inspector.search.set_text("#one");
        let rows = inspector.flatten(&document);
        let text: Vec<&str> = rows.iter().map(|row| row.row.text.as_str()).collect();
        assert!(
            text.iter().any(|row| row.starts_with("<div #one")),
            "the match itself: {text:?}"
        );
        // Its ancestors come with it, or the match would be a row with no place
        // in a tree — and nothing else does.
        assert!(text.iter().any(|row| row.starts_with("<body")));
        assert!(
            !text.iter().any(|row| row.starts_with("<span")),
            "and nothing that does not match: {text:?}"
        );

        inspector.search.set_text("span");
        let text: Vec<String> = inspector
            .flatten(&document)
            .into_iter()
            .map(|row| row.row.text)
            .collect();
        assert!(text.iter().any(|row| row.starts_with("<span")));
        assert!(!text.iter().any(|row| row.starts_with("<div")));

        // Emptied, the tree is the tree again: closed at the root.
        inspector.search.set_text("");
        assert_eq!(inspector.flatten(&document).len(), 1);
    }

    /// ⌘F puts the keyboard in the field wherever it was, and what is typed
    /// after it lands there rather than walking the tree.
    #[test]
    fn find_takes_the_keyboard_and_typing_goes_to_the_field() {
        let document = document();
        let mut inspector = panel();
        frame(&mut inspector, &document);
        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        let mut clipboard = crate::clipboard::InMemory::default();

        assert!(!inspector.text_input('d'), "nothing has the caret yet");
        assert_eq!(
            inspector.key_pressed(
                Key::Character('f'),
                accelerator,
                Some(&document),
                &mut clipboard
            ),
            Some(Action::None)
        );
        assert!(inspector.text_input('d'));
        assert!(inspector.text_input('i'));
        assert!(inspector.text_input('v'));
        assert_eq!(inspector.search.text(), "div");

        // Escape empties the field before it lets it go: a search left behind
        // is a tree filtered by something whose field is no longer on screen.
        inspector.key_pressed(
            Key::Escape,
            Modifiers::default(),
            Some(&document),
            &mut clipboard,
        );
        assert_eq!(inspector.search.text(), "");
        assert!(inspector.open, "and the panel is still open");
    }

    /// The trail along the bottom answers what a scrolled tree cannot: where in
    /// the page the selection is. Each crumb is an ancestor, and pressing one
    /// chooses it.
    #[test]
    fn a_breadcrumb_chooses_the_ancestor_it_names() {
        let document = document();
        let mut inspector = panel();
        let paragraph = every_node(&document)
            .into_iter()
            .find(|node| {
                matches!(document.get(*node).map(|n| &n.data),
                    Some(NodeData::Element(element)) if element.name.local.as_ref() == "p")
            })
            .expect("the document has a p");
        inspector.reveal(&document, paragraph);
        frame(&mut inspector, &document);

        // Along the bottom of the panel, which is where the trail is drawn.
        let row = 300.0 + 300.0 - inspector.theme.row_height / 2.0;
        let chosen = (0..90)
            .map(|step| inspector.action_at(10.0 + step as f64 * 10.0, row))
            .find_map(|action| match action {
                Action::Choose(node) => Some(node),
                _ => None,
            })
            .expect("the trail is pressable");

        inspector.apply(Action::Choose(chosen));
        assert_eq!(inspector.selected, Some(chosen));
        // What was pressed is on the path from the root to what was chosen.
        let mut path = vec![paragraph];
        let mut parent = document.get(paragraph).and_then(|node| node.parent);
        while let Some(node) = parent {
            path.push(node);
            parent = document.get(node).and_then(|node| node.parent);
        }
        assert!(path.contains(&chosen), "a crumb is an ancestor");
    }

    /// A console is opened either to read everything or to find the bad news.
    #[test]
    fn the_console_filter_is_a_floor_and_not_a_set_of_ticks() {
        use tracing::Level as Said;
        assert!(Level::All.admits(Said::DEBUG));
        assert!(Level::Info.admits(Said::INFO));
        assert!(!Level::Info.admits(Said::DEBUG));
        assert!(!Level::Warnings.admits(Said::INFO));
        assert!(Level::Warnings.admits(Said::ERROR));
        assert!(!Level::Errors.admits(Said::WARN));
        assert!(Level::Errors.admits(Said::ERROR));
    }

    /// Clearing forgets what was said, which is what a person means by it.
    #[test]
    fn clearing_the_console_empties_it() {
        let journal = crate::observability::journal();
        journal.record_for_test(tracing::Level::WARN, "test", "something");
        assert!(!journal.records().is_empty());

        let mut inspector = panel();
        inspector.apply(Action::ClearConsole);
        assert!(journal.records().is_empty());
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

        // The second row: below the panel's own header, the padding around the
        // tree half, and the field that narrows it.
        let theme = &inspector.theme;
        let y = 300.0
            + HEADER
            + theme.gap * 0.5
            + theme.control_height
            + theme.gap * 0.5
            + theme.row_height * 1.5;
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
        inspector.key_pressed(
            Key::Down,
            Modifiers::default(),
            Some(&document),
            &mut crate::clipboard::InMemory::default(),
        );
        assert_eq!(inspector.selected, Some(document.root()));

        // Right opens it rather than moving, and only then steps in.
        inspector.key_pressed(
            Key::Right,
            Modifiers::default(),
            Some(&document),
            &mut crate::clipboard::InMemory::default(),
        );
        assert!(inspector.expanded.contains(&document.root()));
        inspector.key_pressed(
            Key::Right,
            Modifiers::default(),
            Some(&document),
            &mut crate::clipboard::InMemory::default(),
        );
        assert_ne!(inspector.selected, Some(document.root()));

        // Left steps back out to the parent.
        inspector.key_pressed(
            Key::Left,
            Modifiers::default(),
            Some(&document),
            &mut crate::clipboard::InMemory::default(),
        );
        assert_eq!(inspector.selected, Some(document.root()));
    }

    /// The copy the inspector was waiting on since D2: what the tree shows for
    /// the chosen node is what ⌘C puts on the clipboard, byte for byte.
    #[test]
    fn copy_puts_the_selected_row_on_the_clipboard() {
        let document = document();
        let mut inspector = panel();
        frame(&mut inspector, &document);
        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        let mut clipboard = crate::clipboard::InMemory::default();

        // Nothing chosen: the key is not the panel's, and the clipboard keeps
        // what it had.
        assert_eq!(
            inspector.key_pressed(
                Key::Character('c'),
                accelerator,
                Some(&document),
                &mut clipboard
            ),
            None
        );
        assert_eq!(clipboard.read(), None);

        inspector.key_pressed(
            Key::Down,
            Modifiers::default(),
            Some(&document),
            &mut clipboard,
        );
        assert_eq!(
            inspector.key_pressed(
                Key::Character('c'),
                accelerator,
                Some(&document),
                &mut clipboard
            ),
            Some(Action::None)
        );
        let copied = clipboard.read().expect("the chosen row was copied");
        let shown = inspector
            .flatten(&document)
            .into_iter()
            .find(|row| Some(row.node) == inspector.selected)
            .expect("the selection is a visible row")
            .row
            .text;
        assert_eq!(copied, shown, "what you see is what you copy");
    }

    #[test]
    fn escape_puts_the_picker_away_before_the_panel() {
        let document = document();
        let mut inspector = panel();
        inspector.picking = true;

        inspector.key_pressed(
            Key::Escape,
            Modifiers::default(),
            Some(&document),
            &mut crate::clipboard::InMemory::default(),
        );
        assert!(!inspector.picking);
        assert!(inspector.open, "one press does one thing");

        inspector.key_pressed(
            Key::Escape,
            Modifiers::default(),
            Some(&document),
            &mut crate::clipboard::InMemory::default(),
        );
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
        assert_eq!(Pane::ALL.len(), 4);
        assert_eq!(
            Pane::ALL.map(Pane::label),
            ["Elements", "Console", "Network", "Accessibility"]
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
