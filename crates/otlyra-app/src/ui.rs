//! The browser's own interface: the tab strip and the toolbar.
//!
//! Drawn with the same `otlyra-gfx` stack the page is drawn with, and for the
//! same reason the plan gives: by the time an interface is needed we already own
//! text layout, hit testing, input routing and painting, and a second toolkit
//! would duplicate all four and bring a second event model with it.
//!
//! The interface is described with the existing [`crate::widget`] constructors.
//! During retained-tree migration, the tab strip and toolbar live behind
//! persistent boundaries: an address edit replaces and redraws the toolbar but
//! reuses the tab strip's tree, geometry, and display list. Browser model state
//! remains outside both boundaries.
//!
//! Two rows. The tab strip on top, on the recessed grey; the toolbar under it,
//! on white, with the active tab merging into it — so the tab and the page it
//! belongs to read as one surface, and the inactive ones read as behind it.

use otlyra_gfx::kurbo::Affine;
use otlyra_gfx::peniko::{Color, ImageData, ImageSampler};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_platform::{Cursor, Key, Modifiers};
use otlyra_text::TextEngine;

pub use crate::widget::Rect;

use crate::clipboard::Clipboard;
use crate::widget::controls::{self, Elide, FieldHit, FieldView, TextInput};
use crate::widget::icon;
use crate::widget::runtime::{
    NodeSpec, RenderArena, Retained, UiDirty, UiNodeId, WidgetKey, WidgetType,
};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Button, CaptureId, Child, Cx, Described, Event, Fixed, Focus, FocusId,
    FocusKind, FocusScopeId, Insets, Label, Padding, Painted, Role, Size, Stack, Widget,
    fill_rounded,
};

/// Height of the tab strip, in logical pixels.
pub const TAB_STRIP_HEIGHT: f64 = 36.0;
/// Height of the toolbar under it.
const TOOLBAR_HEIGHT: f64 = 42.0;
/// Total height the interface takes from the top of the window.
pub const UI_HEIGHT: f64 = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT;

/// The traversal trap the menu behind the cogwheel declares.
///
/// While it is open, Tab walks its rows and nothing else: the toolbar behind it
/// is covered by a sheet a press cannot reach through, and a keyboard that could
/// still walk out there would be walking to controls a reader cannot see.
const MENU_SCOPE: FocusScopeId = FocusScopeId::new(1);

/// The same trap, for the menu the reader asks for over the page.
const CONTEXT_SCOPE: FocusScopeId = FocusScopeId::new(2);

/// And for the find bar, whose field, arrows and cross are the whole of where
/// Tab may go while it is open.
const FIND_SCOPE: FocusScopeId = FocusScopeId::new(3);

/// The widest a tab is allowed to be, however few there are.
const TAB_MAX_WIDTH: f64 = 220.0;
/// The narrowest a tab may shrink to before the strip overflows instead.
const TAB_MIN_WIDTH: f64 = 92.0;
/// The gap between one tab and the next.
const TAB_GAP: f64 = 2.0;
/// The side of the button that opens a tab.
const NEW_TAB_SIZE: f64 = 28.0;
/// How wide each end's chevron is, when the strip has more than it can show.
const CHEVRON_SIZE: f64 = 22.0;

/// An editable single-line text field.
///
/// Byte offsets, not character counts: the text is UTF-8 and a caret that can land
/// mid-character is a panic waiting for the first non-ASCII address.
///
/// A selection is the stretch between `anchor` and `caret`. The anchor is where
/// the selection began — a shift-press or a drag leaves it behind while the
/// caret travels — and when the two agree there is no selection. One pair of
/// offsets rather than a range beside a flag, so an empty selection and a
/// missing one cannot be two different states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextField {
    text: String,
    caret: usize,
    anchor: usize,
}

impl TextField {
    /// A field holding `text`, with the caret at the end.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            caret: text.len(),
            anchor: text.len(),
            text,
        }
    }

    /// The text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret's byte offset.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selected range, lowest offset first. `None` when nothing is selected.
    pub fn selection(&self) -> Option<std::ops::Range<usize>> {
        (self.anchor != self.caret)
            .then(|| self.anchor.min(self.caret)..self.anchor.max(self.caret))
    }

    /// The selected text. `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|range| &self.text[range])
    }

    /// Select everything, with the caret at the end.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Replace the text and put the caret at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.caret = self.text.len();
        self.anchor = self.caret;
    }

    /// Insert a character at the caret and step over it. A live selection is
    /// what the character replaces.
    pub fn insert(&mut self, character: char) {
        self.remove_selection();
        self.text.insert(self.caret, character);
        self.caret += character.len_utf8();
        self.anchor = self.caret;
    }

    /// Delete the selection, or the character before the caret.
    pub fn backspace(&mut self) {
        if self.remove_selection() || self.caret == 0 {
            return;
        }
        let previous = self.previous_boundary(self.caret);
        self.text.replace_range(previous..self.caret, "");
        self.caret = previous;
        self.anchor = previous;
    }

    /// Delete the selection, or the character after the caret.
    pub fn delete(&mut self) {
        if self.remove_selection() || self.caret >= self.text.len() {
            return;
        }
        let next = self.next_boundary(self.caret);
        self.text.replace_range(self.caret..next, "");
        self.anchor = self.caret;
    }

    /// Move the caret one character left; extending leaves the anchor behind.
    ///
    /// With a selection and no shift, the caret collapses to the selection's
    /// start rather than stepping — the selection was the position, and left
    /// means its left end.
    pub fn move_left(&mut self, extend: bool) {
        if extend {
            self.caret = self.previous_boundary(self.caret);
            return;
        }
        self.caret = match self.selection() {
            Some(range) => range.start,
            None => self.previous_boundary(self.caret),
        };
        self.anchor = self.caret;
    }

    /// Move the caret one character right; extending leaves the anchor behind.
    pub fn move_right(&mut self, extend: bool) {
        if extend {
            self.caret = self.next_boundary(self.caret);
            return;
        }
        self.caret = match self.selection() {
            Some(range) => range.end,
            None => self.next_boundary(self.caret),
        };
        self.anchor = self.caret;
    }

    /// Move the caret to the start; extending selects back to it.
    pub fn move_home(&mut self, extend: bool) {
        self.caret = 0;
        if !extend {
            self.anchor = 0;
        }
    }

    /// Move the caret to the end; extending selects forward to it.
    pub fn move_end(&mut self, extend: bool) {
        self.caret = self.text.len();
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Empty the field.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = 0;
    }

    /// Apply what the pointer did, where the field reported it landing.
    pub fn hit(&mut self, hit: FieldHit) {
        match hit {
            FieldHit::Press { offset, clicks } => self.point(offset, clicks),
            FieldHit::Drag { offset } => self.drag_to(offset),
        }
    }

    /// Put the caret at `offset`: a click. Two clicks select the word there,
    /// three the lot.
    pub fn point(&mut self, offset: usize, clicks: u32) {
        match clicks {
            1 => {
                self.caret = self.snap(offset);
                self.anchor = self.caret;
            }
            2 => {
                let word = self.word_at(offset);
                self.anchor = word.start;
                self.caret = word.end;
            }
            _ => self.select_all(),
        }
    }

    /// Drag the caret to `offset`, leaving the anchor where the press put it.
    pub fn drag_to(&mut self, offset: usize) {
        self.caret = self.snap(offset);
    }

    /// Edit with `key`, if it is a key that edits a field.
    ///
    /// The one place a keystroke becomes an edit, shared by every surface that
    /// owns a field — two copies of this table would already have disagreed
    /// about shift. Returns whether the key was one of the field's.
    pub fn edit(&mut self, key: Key, modifiers: Modifiers, clipboard: &mut dyn Clipboard) -> bool {
        if modifiers.is_accelerator() {
            match key {
                Key::Character('a') => self.select_all(),
                Key::Character('c') => self.copy(clipboard),
                Key::Character('x') => self.cut(clipboard),
                Key::Character('v') => self.paste(clipboard),
                _ => return false,
            }
            return true;
        }
        match key {
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Left => self.move_left(modifiers.shift),
            Key::Right => self.move_right(modifiers.shift),
            Key::Home => self.move_home(modifiers.shift),
            Key::End => self.move_end(modifiers.shift),
            _ => return false,
        }
        true
    }

    /// Put the selected text on the clipboard. Nothing selected, nothing
    /// written: copy with no selection must not eat what was there.
    pub fn copy(&self, clipboard: &mut dyn Clipboard) {
        if let Some(selected) = self.selected_text() {
            clipboard.write(selected.to_owned());
        }
    }

    /// Copy the selection and remove it.
    pub fn cut(&mut self, clipboard: &mut dyn Clipboard) {
        self.copy(clipboard);
        self.remove_selection();
    }

    /// Insert the clipboard's text, replacing a live selection.
    ///
    /// Control characters are dropped: this is a single-line field, and a
    /// newline pasted into an address is a keystroke nobody typed.
    pub fn paste(&mut self, clipboard: &mut dyn Clipboard) {
        let Some(pasted) = clipboard.read() else {
            return;
        };
        self.remove_selection();
        for character in pasted.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.caret, character);
            self.caret += character.len_utf8();
        }
        self.anchor = self.caret;
    }

    /// Delete the selected range, if there is one. Whether there was.
    fn remove_selection(&mut self) -> bool {
        let Some(range) = self.selection() else {
            return false;
        };
        self.caret = range.start;
        self.anchor = range.start;
        self.text.replace_range(range, "");
        true
    }

    /// The nearest character boundary at or before `offset`.
    fn snap(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    /// The run of like characters around `offset`: what a double-click selects.
    ///
    /// Letters, digits and the underscore run together; anything else runs with
    /// its own kind, so a double-click in the middle of `://` picks up the
    /// punctuation and not half the host beside it.
    fn word_at(&self, offset: usize) -> std::ops::Range<usize> {
        if self.text.is_empty() {
            return 0..0;
        }
        let is_word = |character: char| character.is_alphanumeric() || character == '_';
        // A click at the very end lands on the last character, not after it.
        let offset = match self.snap(offset) {
            at if at >= self.text.len() => self.previous_boundary(self.text.len()),
            at => at,
        };
        let kind = self.text[offset..].chars().next().is_some_and(is_word);

        let start = self.text[..offset]
            .char_indices()
            .rev()
            .take_while(|(_, character)| is_word(*character) == kind)
            .last()
            .map_or(offset, |(index, _)| index);
        let end = self.text[offset..]
            .char_indices()
            .take_while(|(_, character)| is_word(*character) == kind)
            .last()
            .map_or(offset, |(index, character)| {
                offset + index + character.len_utf8()
            });
        start..end
    }

    fn previous_boundary(&self, from: usize) -> usize {
        self.text[..from]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..]
            .chars()
            .next()
            .map_or(self.text.len(), |character| from + character.len_utf8())
    }
}

/// What the interface wants the browser to do.
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    /// Nothing.
    None,
    /// Navigate the active tab to this text, as typed.
    Navigate(String),
    /// Open a tab.
    NewTab,
    /// The keyboard walked off the end of the chrome and belongs to the page.
    ///
    /// The interface cannot put it there itself: which surface is beyond it,
    /// and whether there is a document to walk at all, is the browser's to
    /// know. `true` when the reader was going forward.
    LeaveChrome(bool),
    /// Put the tab named `id` at this position in the strip.
    ///
    /// By id rather than by its current index, because the index is what the
    /// move changes: a drag reports several of these as it crosses its
    /// neighbours, and each one is about the same tab.
    MoveTab {
        /// Which tab is being moved.
        id: u64,
        /// Where it should sit once it has moved.
        to: usize,
    },
    /// Close a tab by index.
    CloseTab(usize),
    /// Make a tab active.
    SelectTab(usize),
    /// Load the active tab's address again.
    Reload,
    /// Stop waiting for the active tab's current navigation.
    Stop,
    /// Go back one entry in the active tab's history.
    Back,
    /// Go forward one entry.
    Forward,
    /// Open one of the browser's own pages.
    OpenPage(SystemPage),
    /// Show the inspector, or put it away.
    ToggleInspector,
    /// Keep the page the active tab is on, or stop keeping it.
    ToggleBookmark,
    /// Show the menu behind the cogwheel, or put it away.
    ///
    /// Never reaches the browser: the menu is the interface's own state, like
    /// the caret in the address field.
    ToggleMenu,
    /// Slide the tab strip by a screenful in this direction.
    ///
    /// Never reaches the browser either: where the strip is scrolled to is the
    /// interface's own, in the same way the menu being open is.
    ScrollTabs(bool),
    /// Put the open popup away without doing anything else — what a press
    /// anywhere off its panel means.
    DismissPopup,
    /// A row of the context menu, chosen.
    Context(ContextCommand),
    /// Give this control the keyboard — on the toolbar, always the address field.
    ///
    /// Never reaches the browser: [`BrowserUi::pointer_pressed`] applies it to
    /// its own state and reports [`UiAction::None`]. It is an action rather than
    /// a rectangle test in the press handler because that is what keeps the
    /// field's position known in exactly one place — the widget tree that drew
    /// it. The id comes from the frame that drew the field, so it names what is
    /// on screen rather than a number chosen in advance.
    Focus(FocusId),
    /// The pointer landed in the address field, at this offset in its text.
    /// The field reports where; what a click, a double-click or a drag there
    /// means to the caret and the anchor is the interface's to decide.
    AddressHit(FieldHit),
    /// The same, for the find bar's own field.
    ///
    /// A second action rather than a second reader of the first: the two fields
    /// hold different text and one press belongs to exactly one of them, and an
    /// offset with no field attached would be an offset either could take.
    FindHit(FieldHit),
    /// Go to the next place the query occurs, or the one before it.
    ///
    /// Never reaches the query itself: what is being looked for is in the bar's
    /// field, which the browser reads, so this says only *move*.
    FindStep(bool),
    /// Put the find bar away.
    ///
    /// Never reaches the browser: whether a bar is open is the interface's own
    /// state, like the menu, and whether the page is still searched is answered
    /// by asking the bar rather than by being told.
    CloseFind,
    /// Put the page back to its own size.
    ResetZoom,
}

/// What the find bar says about what it found.
///
/// Written by the browser wherever the page is searched, because how many times
/// a query occurs is a fact about the document. The interface keeps it only to
/// draw *3 of 17* with.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FindStatus {
    /// How many places the query occurs on the page.
    pub total: usize,
    /// Which of them the reader is on, counted from one. Zero when there are
    /// none — there is no zeroth match, and *0 of 0* is what a bar with nothing
    /// found says.
    pub current: usize,
}

/// What one row of the context menu does.
///
/// The browser decides which of these a particular press offers and whether
/// each is available; the interface only draws them and reports the one that
/// was chosen. Where the press landed — which link, which element — is the
/// browser's to remember, because the row says *what* and the browser is the
/// only thing that knows *to what*.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContextCommand {
    /// Open the link that was pressed in a tab of its own.
    OpenLinkInNewTab,
    /// Put the link's address on the clipboard.
    CopyLinkAddress,
    /// Copy what is selected on the page.
    CopySelection,
    /// Select the whole document.
    SelectAll,
    /// Go back one entry.
    Back,
    /// Go forward one entry.
    Forward,
    /// Load this page again.
    Reload,
    /// Open the inspector on the element that was pressed.
    InspectElement,
}

impl ContextCommand {
    /// What the row says.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenLinkInNewTab => "Open Link in New Tab",
            Self::CopyLinkAddress => "Copy Link Address",
            Self::CopySelection => "Copy",
            Self::SelectAll => "Select All",
            Self::Back => "Back",
            Self::Forward => "Forward",
            Self::Reload => "Reload",
            Self::InspectElement => "Inspect Element",
        }
    }

    /// The accelerator that does the same thing, where one does.
    fn shortcut(self) -> Option<&'static str> {
        match self {
            Self::CopySelection => Some("⌘C"),
            Self::SelectAll => Some("⌘A"),
            Self::Reload => Some("⌘R"),
            Self::InspectElement => Some("⌥⌘I"),
            _ => None,
        }
    }

    /// What is drawn in the row's mark column.
    ///
    /// The two arrows, the reload and the inspector are things with a picture
    /// already, and drawing it is what makes a row here and the button that
    /// does the same thing recognizably one command. The rest draw nothing
    /// rather than borrowing a shape that means something else — a page beside
    /// "Copy" is a picture of the wrong idea. The column stays either way, so
    /// the labels line up whether their row has a mark or not.
    fn mark(self) -> fn(&mut DisplayList, Rect, otlyra_gfx::peniko::Color) {
        match self {
            Self::OpenLinkInNewTab => icon::plus,
            Self::CopyLinkAddress | Self::CopySelection | Self::SelectAll => |_, _, _| {},
            Self::Back => {
                |list, rect, color| icon::chevron(list, rect, icon::Direction::Left, color)
            }
            Self::Forward => {
                |list, rect, color| icon::chevron(list, rect, icon::Direction::Right, color)
            }
            Self::Reload => |list, rect, color| icon::reload(list, rect, None, color),
            Self::InspectElement => icon::page,
        }
    }
}

/// One row of a context menu, as the browser decided it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextRow {
    /// Something to choose, drawn dim when it cannot be chosen.
    Command(ContextCommand, bool),
    /// A line between two groups of them.
    Divider,
}

/// A page the browser serves about itself.
///
/// Not URLs yet. When there is an `about:` scheme these become addresses and
/// the menu navigates to them like anything else; until then they name a
/// surface the browser draws instead of a document.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SystemPage {
    /// The preferences.
    Settings,
    /// Where the reader has been.
    History,
    /// What the reader kept.
    Bookmarks,
    /// What was fetched to disk.
    Downloads,
    /// What sites are keeping here.
    Cookies,
    /// What has already been fetched.
    Cache,
    /// What this program is.
    About,
}

impl SystemPage {
    /// Whether this one has been built yet.
    ///
    /// The menu lists all of them and dims the rest, rather than growing an
    /// entry per milestone: what a browser cannot do *yet* is worth saying.
    pub fn available(self) -> bool {
        matches!(
            self,
            Self::Settings
                | Self::History
                | Self::Bookmarks
                | Self::Downloads
                | Self::Cookies
                | Self::Cache
                | Self::About
        )
    }

    /// The address that names this page.
    ///
    /// `about:` rather than a scheme of our own. Both `chrome://settings` and
    /// `firefox://…` are a vendor putting its name in the URL bar of a page
    /// that is not on the web; `about:` is the one spelling every browser
    /// already answers to, it is registered for exactly this, and it does not
    /// have to be renamed if this program is.
    pub fn url(self) -> &'static str {
        match self {
            Self::Settings => "about:settings",
            Self::History => "about:history",
            Self::Bookmarks => "about:bookmarks",
            Self::Downloads => "about:downloads",
            Self::Cookies => "about:cookies",
            Self::Cache => "about:cache",
            Self::About => "about:otlyra",
        }
    }

    /// The page `url` names, if it names one.
    ///
    /// Case-insensitive on the scheme, because a URL bar is typed into by
    /// hand: `About:Settings` is the same request.
    pub fn from_url(url: &str) -> Option<Self> {
        let rest = url
            .strip_prefix("about:")
            .or_else(|| url.strip_prefix("About:"))
            .or_else(|| url.strip_prefix("ABOUT:"))?;
        let rest = rest.trim_end_matches('/').to_ascii_lowercase();
        Some(match rest.as_str() {
            "settings" | "preferences" | "config" => Self::Settings,
            "history" => Self::History,
            "bookmarks" => Self::Bookmarks,
            "downloads" => Self::Downloads,
            "cookies" => Self::Cookies,
            "cache" => Self::Cache,
            // `about:` on its own is the browser talking about itself, which is
            // what every other browser does with it too.
            "otlyra" | "about" | "version" | "" => Self::About,
            _ => return None,
        })
    }

    /// What it is called in the menu.
    pub fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::History => "History",
            Self::Bookmarks => "Bookmarks",
            Self::Downloads => "Downloads",
            Self::Cookies => "Cookies",
            Self::Cache => "Cache",
            Self::About => "About Otlyra",
        }
    }
}

/// What one tab shows in the strip.
#[derive(Clone, Debug)]
pub struct TabLabel {
    /// Stable browser-model identity. A strip position is not an identity:
    /// closing the first tab moves every later one.
    pub id: u64,
    /// The tab's title, or its URL until it has one.
    pub title: String,
    /// Whether it is still loading.
    pub loading: bool,
}

/// Everything the interface's appearance is a function of.
///
/// If two frames agree on all of it, they would draw the same list, so the
/// second frame does not build one. This is the whole of the caching rule, and
/// keeping it as one comparable value is what stops it from rotting: a new thing
/// the interface draws has to be added here to be drawn, because otherwise it
/// does not appear until something else changes.
///
/// The window's *height* is deliberately absent. The interface is a fixed band
/// at the top: dragging the bottom edge of the window changes what the page has
/// to lay out in and nothing about the toolbar. The one exception is an open
/// menu, which hangs below the band — so the height only counts while it is
/// open, and that is what `menu` carries.
#[derive(Clone, PartialEq)]
struct Appearance {
    width: f64,
    tabs: Vec<(u64, String, bool)>,
    active: usize,
    history: (bool, bool),
    spinner: Option<f32>,
    pointer: (f64, f64),
    pointer_down: bool,
    address: String,
    caret: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    focus: Option<FocusId>,
    /// What the pointer has rested on long enough to be named, and where.
    ///
    /// Part of what the frame draws, so it belongs in the key: without it the
    /// first tooltip would be the only one the cache ever built.
    tooltip: Option<(String, Rect)>,
    /// The popup drawn over everything, and the window height its sheet covers.
    ///
    /// The whole popup rather than a flag: its rows and where it hangs are
    /// things the frame draws, and a key that did not carry them would draw the
    /// first context menu forever.
    popup: Option<(f64, Popup)>,
    /// What the find bar shows, while one is open.
    ///
    /// Beside the popup rather than inside it, because `Popup::Find` carries
    /// nothing: the query lives on the interface the way the address does, and
    /// the count is the browser's answer about the document. All of it is drawn,
    /// so all of it is in the key.
    find: Option<FindLook>,
    /// Whether the page in the active tab is one the reader kept.
    ///
    /// Part of what the interface draws because the star and the menu both say which
    /// of *keep this* and *stop keeping this* a press will do, and either saying the
    /// wrong one would be the interface lying about what a press does.
    bookmark: Bookmarked,
    /// Shown on the menu, so it belongs to what the frame is a function of.
    zoom: f32,
    tab_scroll: f64,
}

/// Everything the find bar draws.
#[derive(Clone, PartialEq)]
struct FindLook {
    text: String,
    caret: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    status: FindStatus,
}

impl Appearance {
    /// Whether two frames differ only in what the address field draws.
    fn same_except_address(&self, other: &Self) -> bool {
        let mut without_address = self.clone();
        without_address.address.clone_from(&other.address);
        without_address.caret = other.caret;
        without_address.selection.clone_from(&other.selection);
        without_address == *other
    }
}

#[derive(Clone, PartialEq)]
struct TabAppearance {
    width: f64,
    tabs: Vec<(u64, String, bool)>,
    active: usize,
    spinner: Option<f32>,
    pointer: Option<(f64, f64, bool)>,
    focus: Option<FocusId>,
    tab_scroll: f64,
}

struct TabStripRenderNode;
struct TabRenderNode;

#[derive(Clone, PartialEq)]
struct ToolbarAppearance {
    width: f64,
    history: (bool, bool),
    spinner: Option<f32>,
    pointer: Option<(f64, f64, bool)>,
    address: String,
    caret: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    focus: Option<FocusId>,
    /// The star is in the toolbar, so the retained toolbar has to know: a boundary
    /// that does not key on something it draws is a boundary that draws it once and
    /// then keeps the first answer forever.
    bookmark: Bookmarked,
}

/// What the toolbar's star says about the page in the active tab.
///
/// Three states rather than a `bool`, because there are three things to draw and a
/// flag beside a second flag saying *and this one counts* is two things to keep in
/// step. A blank tab has no address, so there is nothing to keep and the star is
/// dimmed rather than lying about being pressable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Bookmarked {
    /// There is no page to keep.
    Impossible,
    /// A page, not kept.
    No,
    /// A page the reader kept.
    Yes,
}

/// One place the omnibox offers under what has been typed.
///
/// Where it came from — somewhere the reader has been, something they kept — is
/// the browser's to decide; the row only shows it and reports the address if it
/// is taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// What the page is called.
    pub title: String,
    /// The address it opens.
    pub url: String,
    /// Whether the reader kept it, which is what the star on the row says.
    pub kept: bool,
}

/// A tab being dragged along the strip.
///
/// The order itself is not here: it is the browser's, and the drag reports
/// [`UiAction::MoveTab`] as the pointer crosses each neighbour rather than
/// keeping a second copy of the strip to be applied on release. What is here is
/// what only the drag knows — which tab, where it started, and whether the
/// pointer has moved far enough for this to be a drag at all.
#[derive(Copy, Clone, Debug, PartialEq)]
struct TabDrag {
    /// The tab that was pressed.
    id: u64,
    /// Where it sat when the press landed, so Escape can put it back.
    from: usize,
    /// Where the press landed.
    origin: f64,
    /// Whether the pointer has passed the threshold. Until it does, the press
    /// is still just a press, and letting go is a click on the tab.
    started: bool,
}

/// What the pointer has been resting on, and since when.
///
/// The label is the control's own — the name a screen reader is given — so what
/// a tooltip says and what a reader is told are one string. Two would drift,
/// and the one nobody can see is the one that would drift.
#[derive(Clone, Debug, PartialEq)]
struct Resting {
    /// What the control is called.
    label: String,
    /// Where it was drawn, which is what the panel hangs off.
    anchor: Rect,
    /// When the pointer came to rest on it.
    since: std::time::Instant,
}

/// How long the pointer rests on a control before it is named.
///
/// Long enough that moving across the toolbar names nothing on the way, short
/// enough that a reader who stopped to ask has an answer before they give up.
const TOOLTIP_DELAY: std::time::Duration = std::time::Duration::from_millis(600);

/// How far the pointer travels before a press on a tab becomes a drag.
///
/// Far enough that a click with an unsteady hand stays a click, near enough
/// that a deliberate drag starts before the reader wonders whether it will.
const DRAG_THRESHOLD: f64 = 5.0;

/// A panel drawn over everything else, which owns the pointer and the keyboard
/// until it is dismissed.
///
/// One value rather than a flag per popup: two popups open at once is a thing
/// the interface would have to decide about at every press, and the answer —
/// opening one dismisses the other — is what one value says by construction.
#[derive(Clone, Debug, PartialEq)]
enum Popup {
    /// The menu behind the cogwheel.
    Menu,
    /// The menu the reader asked for, where they asked for it.
    Context {
        /// Where the pointer was, in logical window coordinates.
        at: (f64, f64),
        /// What the browser decided to offer there.
        rows: Vec<ContextRow>,
    },
    /// Where the omnibox could take what has been typed.
    Suggestions {
        /// What the browser found, best first.
        rows: Vec<Suggestion>,
        /// Which row the arrows have reached, if any.
        ///
        /// Walking is not taking: the field keeps what was typed and keeps the
        /// keyboard, and only Return takes what the mark reached. A list that
        /// filled the field as the arrows moved would narrow itself under the
        /// reader down to the row they had just got to.
        marked: Option<usize>,
    },
    /// The bar that looks for a run of characters in the page.
    ///
    /// It carries nothing: what has been typed is a field on the interface, the
    /// way the address is, and what was found is the browser's answer about the
    /// document.
    Find,
}

impl Popup {
    /// The traversal trap this popup's rows are claimed in.
    ///
    /// `None` for a popup that does not own the keyboard: the omnibox's
    /// suggestions hang under a field the reader is still typing into, so the
    /// keyboard stays where it is and the arrows walk the list instead.
    fn scope(&self) -> Option<FocusScopeId> {
        match self {
            Self::Menu => Some(MENU_SCOPE),
            Self::Context { .. } => Some(CONTEXT_SCOPE),
            Self::Suggestions { .. } => None,
            Self::Find => Some(FIND_SCOPE),
        }
    }

    /// Whether reaching past this panel puts it away.
    ///
    /// A menu is a choice being made, so pressing elsewhere or naming another
    /// control with an accelerator is that choice being abandoned. The find bar
    /// is not a choice — it is a mode the reader is in, with a query they are
    /// part way through — so clicking the page or pressing ⌘L leaves it open,
    /// which is what every browser does and what makes *look for this, then
    /// look at that* possible at all.
    fn transient(&self) -> bool {
        !matches!(self, Self::Find)
    }

    /// Whether a sheet under it takes every press that misses the panel.
    ///
    /// A menu is modal to the pointer: pressing anywhere else means *put this
    /// away* and nothing more. Suggestions are not — a reader who clicks the
    /// page while the list is showing means to click the page, and a sheet
    /// would also stop the wheel from scrolling what is behind it. Neither is
    /// the find bar, for the stronger version of the same reason: reading the
    /// page it is searching is the whole point of having searched it.
    fn has_sheet(&self) -> bool {
        !matches!(self, Self::Suggestions { .. } | Self::Find)
    }
}

/// The interface's own state: what is focused, where the pointer is, what is typed.
pub struct BrowserUi {
    /// The address field.
    pub address: TextField,
    /// The find bar's own field: what is being looked for on the page.
    pub find: TextField,
    /// What the browser found for it.
    pub find_status: FindStatus,
    /// Whether the bar's field should take the keyboard as soon as it exists.
    ///
    /// ⌘F opens the bar *and* focuses it, but a control's focus id is claimed by
    /// the frame that draws it and there is no id to hand the keyboard to before
    /// then. So the wish is recorded here and granted during the build, at the
    /// moment the field claims its id — which is why it is a cell: the tree is
    /// built from `&self`.
    find_takes_keyboard: std::cell::Cell<bool>,
    /// The id the build moved the keyboard to, for the build to adopt afterwards.
    focus_granted: std::cell::Cell<Option<FocusId>>,
    /// Whether the address bar has been asked for the keyboard and has not been
    /// given it against a frame yet.
    ///
    /// # Why an intention and not an id
    ///
    /// A [`FocusId`] is a *position* in the ring the last frame built. Opening a
    /// tab changes which controls that frame has — a blank tab has nothing to go
    /// back to and nothing to reload — so the position the address field had is
    /// somebody else's the moment the toolbar is rebuilt, and the caret set
    /// before the rebuild lands on a button after it. That is the bug this exists
    /// to fix: ⌘T focused the field and the very next frame took it away.
    ///
    /// So the request outlives one build and is resolved against the ring that
    /// build produced, where the answer is right by construction.
    address_wanted: bool,
    /// The panel drawn over everything, if one is open.
    popup: Option<Popup>,
    /// Whether the page in the active tab is one the reader kept.
    ///
    /// Written by the browser wherever the address is synchronized, because it is a
    /// property of the address. The interface keeps it only to draw with: the star
    /// in the toolbar and the words on the menu are the same fact twice.
    pub bookmark: Bookmarked,
    /// How much larger than its own pixels the page is drawn.
    ///
    /// The browser's, written here to be shown: a reader whose page is stuck at
    /// 125% with nothing saying so has no way to find out but by pressing keys
    /// until it looks right again.
    pub zoom: f32,
    /// Every colour and measurement the interface is drawn from.
    pub theme: Theme,
    /// Which control has the keyboard, if any.
    ///
    /// One value rather than a focus id beside an `address_focused` flag: the
    /// field shows a caret exactly when this lands on its id, so there is
    /// nothing to keep in step.
    focused: Option<FocusId>,
    /// Where the keyboard was when the popup opened.
    ///
    /// Escape puts it back there, which is what makes a popup something a
    /// keyboard can look into and leave again. A press that dismisses it does
    /// not: a ring appearing on the toolbar after a click is the interface
    /// answering a question nobody asked.
    popup_return: Option<FocusId>,
    /// The focusable controls the last frame built, in the order it built them.
    focus: Focus,
    /// How far the tab strip is slid along, and how far it could be.
    ///
    /// The strip's own, like the menu being open: which tabs are on screen is a
    /// fact about the interface and not about the browser.
    tab_scroll: f64,
    tab_overflow: crate::widget::Overflow,
    /// Where the active tab was placed, reported by the frame that placed it.
    ///
    /// Written during `place` and read afterwards to bring the tab into view.
    /// Derived from the geometry that was actually used rather than worked out a
    /// second time from the tab count — the strip has separators between some
    /// pairs of tabs and not others, and a second sum would have to know that
    /// and would be wrong the first time it changed.
    active_tab: crate::widget::Placed,
    /// And where the window that shows the strip was placed, so the two can be
    /// compared without either being worked out a second time.
    tab_window: crate::widget::Placed,
    pointer: (f64, f64),
    pointer_down: bool,
    /// Where the pointer went down, while it is still down. What lets a drag
    /// that began in the address field keep selecting past its edge.
    press_origin: Option<(f64, f64)>,
    /// Which control took the pointer, for the one kind of drag that moves the
    /// control being dragged.
    capture: Option<CaptureId>,
    /// The tab being dragged along the strip, once the pointer has moved far
    /// enough for the press to be a drag rather than a click.
    drag: Option<TabDrag>,
    /// What the pointer is resting on, for the panel that names it.
    resting: Option<Resting>,
    /// Where the open popup's panel was drawn, for a press to be tested
    /// against when there is no sheet to catch it.
    popup_rect: crate::widget::Placed,
    /// Where the address field was drawn, which is what the suggestions hang
    /// under. From the frame that placed it, like every other geometry here.
    address_rect: Rect,
    /// Where each tab landed in the last frame, by tab id.
    ///
    /// Read to answer *which tab is the pointer over* during a drag. From the
    /// frame that placed them, like every other geometry question here: a strip
    /// works out tab positions with gaps, separators and a scroll offset, and a
    /// second sum of the same thing would be wrong the first time one of those
    /// changed.
    tab_places: crate::widget::Placements,
    /// How many clicks the current press is the latest of.
    clicks: u32,
    /// What the last built list was built from, and the list itself.
    cache: Option<(Appearance, std::sync::Arc<DisplayList>)>,
    /// The part of the chrome changed by the last build, when it can be bounded.
    ///
    /// `None` means the whole chrome band. Kept beside the cached list so a
    /// second compose of the same frame reports the same promise to the
    /// compositor rather than forgetting it.
    dirty: Option<Rect>,
    /// Persistent migration boundaries. A changed toolbar no longer rebuilds,
    /// measures, shapes, or paints the tab strip beside it.
    tab_tree: Retained<UiAction>,
    toolbar_tree: Retained<UiAction>,
    tab_appearance: Option<TabAppearance>,
    toolbar_appearance: Option<ToolbarAppearance>,
    tab_runtime: RenderArena,
    tab_runtime_root: UiNodeId,
    /// Work attributed by the latest reconciliation, retained for diagnostics.
    tab_runtime_work: Vec<(UiNodeId, UiDirty)>,
    /// Stable focus-id prefix owned by each retained boundary.
    tab_focus_end: usize,
    toolbar_focus_end: usize,
    /// How many lists have been built, as opposed to reused.
    ///
    /// Kept because "it did not rebuild" is the whole claim of the cache, and a
    /// claim a test cannot see is a claim that quietly stops being true.
    builds: u64,
    /// Last frame's tree, kept only so a press lands on what was drawn.
    root: Option<Child<UiAction>>,
}

impl Default for BrowserUi {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BrowserUi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserUi")
            .field("address", &self.address)
            .field("focused", &self.focused)
            .field("pointer", &self.pointer)
            .finish_non_exhaustive()
    }
}

impl BrowserUi {
    /// A new interface with an empty address field.
    pub fn new() -> Self {
        let mut tab_runtime = RenderArena::new();
        let tab_runtime_root =
            tab_runtime.mount(None, WidgetType::of::<TabStripRenderNode>(), None);
        Self {
            address: TextField::default(),
            find: TextField::default(),
            find_status: FindStatus::default(),
            find_takes_keyboard: std::cell::Cell::new(false),
            focus_granted: std::cell::Cell::new(None),
            address_wanted: false,
            popup: None,
            bookmark: Bookmarked::Impossible,
            zoom: 1.0,
            theme: Theme::light(),
            focused: None,
            popup_return: None,
            focus: Focus::default(),
            tab_scroll: 0.0,
            tab_overflow: crate::widget::Overflow::default(),
            active_tab: std::rc::Rc::new(std::cell::Cell::new(crate::widget::Rect::ZERO)),
            tab_window: std::rc::Rc::new(std::cell::Cell::new(crate::widget::Rect::ZERO)),
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            capture: None,
            drag: None,
            resting: None,
            popup_rect: std::rc::Rc::new(std::cell::Cell::new(Rect::ZERO)),
            address_rect: Rect::ZERO,
            tab_places: crate::widget::Placements::default(),
            clicks: 1,
            cache: None,
            dirty: None,
            tab_tree: Retained::new(Box::new(crate::widget::Gap::new(0.0, 0.0))),
            toolbar_tree: Retained::new(Box::new(crate::widget::Gap::new(0.0, 0.0))),
            tab_appearance: None,
            toolbar_appearance: None,
            tab_runtime,
            tab_runtime_root,
            tab_runtime_work: Vec::new(),
            tab_focus_end: 0,
            toolbar_focus_end: 0,
            builds: 0,
            root: None,
        }
    }

    /// What the last frame drew, for something that cannot see it.
    ///
    /// Asked of the tree that drew the frame, like the cursor and like a press:
    /// a description worked out a second time from state kept elsewhere would be
    /// a second opinion about the interface, and the two would part company the
    /// first time one of them was updated and the other was not.
    ///
    /// Empty before the first frame, which is honest — nothing has been drawn.
    pub fn describe(&self) -> Vec<Described> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            root.describe(&mut out);
        }
        out
    }

    /// Which control holds the keyboard, for whoever is reading the description.
    pub fn focused(&self) -> Option<FocusId> {
        self.focused
    }

    /// How many display lists this interface has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Tab-strip display lists built behind its retained boundary.
    pub fn tab_builds(&self) -> u64 {
        self.tab_tree.builds()
    }

    /// Toolbar display lists built behind its retained boundary.
    pub fn toolbar_builds(&self) -> u64 {
        self.toolbar_tree.builds()
    }

    /// Tab-strip semantic descriptions built behind its retained boundary.
    pub fn tab_semantics_builds(&self) -> u64 {
        self.tab_tree.semantics_builds()
    }

    /// Toolbar semantic descriptions built behind its retained boundary.
    pub fn toolbar_semantics_builds(&self) -> u64 {
        self.toolbar_tree.semantics_builds()
    }

    /// What the latest rebuilt chrome list changed, in logical coordinates.
    ///
    /// Typing in the omnibox changes its face, text, selection and caret but no
    /// tab or neighbouring toolbar control. All other rebuilds conservatively
    /// answer `None`, which means the whole chrome layer.
    pub fn dirty(&self) -> Option<Rect> {
        self.dirty
    }

    /// Draw from `theme` from the next frame on.
    ///
    /// Through a method rather than the field, because the cache does not key
    /// on the theme: a stored list is a list in the old palette, and it has to
    /// go when the palette does.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.cache = None;
            self.tab_appearance = None;
            self.toolbar_appearance = None;
        }
    }

    /// Note where the pointer is. Kept so a press can be tested against the same
    /// geometry the last frame drew.
    ///
    /// While the button is down, the move is offered to the tree: a drag that
    /// began in the address field is a selection growing, and the field is the
    /// one that knows which offset the pointer is over.
    pub fn pointer_moved(&mut self, x: f64, y: f64, text: &mut TextEngine) -> UiAction {
        self.pointer = (x, y);
        self.rest_on_what_is_under_the_pointer();
        if self.press_origin.is_none() {
            return UiAction::None;
        }
        // A control that took the pointer gets the move and nothing else does,
        // which is the whole of what capture buys: the tab under the pointer
        // must not answer a drag that belongs to the tab being dragged.
        if self.capture.is_some() {
            return self.drag_moved(x);
        }
        let mut cx = self.cx(text);
        let action = self
            .root
            .as_mut()
            .and_then(|root| root.event(&Event::PointerMoved, &mut cx));
        match action {
            Some(UiAction::AddressHit(hit)) => self.address.hit(hit),
            Some(UiAction::FindHit(hit)) => self.find.hit(hit),
            _ => {}
        }
        UiAction::None
    }

    /// The pointer moved while a tab holds it.
    ///
    /// Nothing happens until it has travelled far enough for the press to be a
    /// drag; after that, the tab is asked to sit wherever the pointer is, which
    /// is whichever tab's rectangle the pointer is over in the frame on screen.
    fn drag_moved(&mut self, x: f64) -> UiAction {
        let Some(drag) = self.drag.as_mut() else {
            return UiAction::None;
        };
        if !drag.started {
            if (x - drag.origin).abs() < DRAG_THRESHOLD {
                return UiAction::None;
            }
            drag.started = true;
        }
        let id = drag.id;
        let places = self.tab_places.borrow();
        let at = places.iter().position(|(key, _)| *key == id);
        // Where the pointer is, among the tabs as they were last drawn. Past
        // the last tab's right edge means the end of the strip, and past the
        // first tab's left edge means the start: a drag that leaves the strip
        // sideways still says which end it is heading for.
        let over = places
            .iter()
            .position(|(_, rect)| x >= rect.x && x < rect.x + rect.width)
            .or_else(|| {
                let last = places.len().checked_sub(1)?;
                let (_, first_rect) = places.first()?;
                let (_, last_rect) = places.last()?;
                if x >= last_rect.x + last_rect.width {
                    Some(last)
                } else if x < first_rect.x {
                    Some(0)
                } else {
                    None
                }
            });
        drop(places);
        match (at, over) {
            (Some(at), Some(over)) if at != over => UiAction::MoveTab { id, to: over },
            _ => UiAction::None,
        }
    }

    /// Note what the pointer has come to rest on, if it is anything.
    ///
    /// Read off the last frame's own description of itself, which is where
    /// every other question about what is on screen is answered: a control's
    /// rectangle and its name are both there, and neither is worked out a
    /// second time. Coming to rest on the *same* control keeps the clock
    /// running, so crossing a wide button does not restart it every pixel.
    fn rest_on_what_is_under_the_pointer(&mut self) {
        // Nothing is named while the button is down or a panel is open: a
        // reader dragging or choosing is not asking what anything is called.
        if self.pointer_down || self.popup_open() {
            self.resting = None;
            return;
        }
        let (x, y) = self.pointer;
        // The innermost match, which is the last described: the cross inside a
        // tab is what the pointer is on when it is over both.
        let found = self
            .describe()
            .into_iter()
            .rfind(|node| node.enabled && !node.label.is_empty() && node.rect.contains(x, y));
        match found {
            Some(node) => {
                let same = self
                    .resting
                    .as_ref()
                    .is_some_and(|resting| resting.label == node.label);
                if !same {
                    self.resting = Some(Resting {
                        label: node.label,
                        anchor: node.rect,
                        since: std::time::Instant::now(),
                    });
                }
            }
            None => self.resting = None,
        }
    }

    /// When the panel naming what the pointer rests on is due, if one is.
    ///
    /// `None` once it is showing: it does not need waking again, and a deadline
    /// in the past asked for a frame every tick.
    pub fn next_tooltip_frame(&self) -> Option<std::time::Instant> {
        let resting = self.resting.as_ref()?;
        let due = resting.since + TOOLTIP_DELAY;
        (std::time::Instant::now() < due).then_some(due)
    }

    /// What to name, and where, if the pointer has rested long enough.
    fn tooltip(&self) -> Option<(&str, Rect)> {
        let resting = self.resting.as_ref()?;
        (resting.since.elapsed() >= TOOLTIP_DELAY)
            .then_some((resting.label.as_str(), resting.anchor))
    }

    /// Wind the resting clock back, so a test need not wait for a tooltip.
    pub fn wind_rest_back(&mut self, by: std::time::Duration) {
        if let Some(resting) = self.resting.as_mut() {
            resting.since -= by;
        }
    }

    /// Whether a tab is being dragged along the strip right now.
    pub fn dragging_tab(&self) -> bool {
        self.drag.is_some_and(|drag| drag.started)
    }

    /// Where each tab landed in the last frame, by tab id.
    ///
    /// From the frame that placed them, for anything that has to ask where a
    /// tab is without working the strip's arithmetic out a second time.
    pub fn tab_places(&self) -> crate::widget::Placements {
        std::rc::Rc::clone(&self.tab_places)
    }

    /// Whether the address field has the keyboard.
    ///
    /// A question about where the focus is, not a flag. There are two fields on
    /// this surface once the find bar is open, and they hold different text —
    /// so *a caret is somewhere* is not the question, and which scope the
    /// keyboard is in is what tells them apart.
    pub fn address_focused(&self) -> bool {
        self.focus.kind(self.focused) == Some(FocusKind::Text) && !self.find_focused()
    }

    /// Whether the find bar's own field has it.
    pub fn find_focused(&self) -> bool {
        self.focus.kind(self.focused) == Some(FocusKind::Text)
            && self.focus.scope(self.focused) == Some(FIND_SCOPE)
    }

    /// Whether the find bar is open.
    pub fn finding(&self) -> bool {
        self.popup == Some(Popup::Find)
    }

    /// Whether ⌘F has asked for the keyboard and the bar has yet to be built.
    ///
    /// What the browser routes the next keystroke by: the field is about to hold
    /// the keyboard and does not hold it yet, and a surface chosen on the second
    /// fact alone would send the letter after ⌘F to the page.
    pub fn find_wants_keyboard(&self) -> bool {
        self.find_takes_keyboard.get()
    }

    /// Open the find bar and give it the keyboard, which is what ⌘F means.
    ///
    /// Everything already in the field is selected, so the next character typed
    /// replaces the last search rather than extending it — a reader who presses
    /// ⌘F is starting a search, and one who wanted to keep the old query still
    /// has it in front of them to step through.
    pub fn open_find(&mut self) {
        if !self.finding() {
            self.open_popup(Popup::Find);
        }
        self.find.select_all();
        self.find_takes_keyboard.set(true);
    }

    /// Show the bar for a search the page is already carrying.
    ///
    /// What a tab coming to the front does: the query belongs to the page, so
    /// the bar is the page's search made visible rather than a second copy of
    /// it. The keyboard stays where it is — arriving at a tab is not asking to
    /// type into it.
    pub fn restore_find(&mut self, query: &str) {
        if !self.finding() {
            self.open_popup(Popup::Find);
        }
        self.find.set_text(query);
    }

    /// Put the find bar away.
    pub fn close_find(&mut self) {
        if self.finding() {
            self.close_popup(true);
        }
    }

    /// Put away a panel that reaching past it dismisses.
    ///
    /// Every press that lands elsewhere and every accelerator that names another
    /// control comes through here rather than through [`Self::close_popup`], so
    /// that the one panel which is a mode rather than a choice stays open.
    fn dismiss_transient(&mut self) {
        if self.popup.as_ref().is_some_and(Popup::transient) {
            self.close_popup(false);
        }
    }

    /// Take the focus off whatever holds it — the toolbar's job when a press
    /// lands below it. The caret and any address selection are drawn only while
    /// the field is focused, so dropping the focus is what puts them away.
    pub fn blur(&mut self) {
        self.focused = None;
    }

    /// Put the keyboard on the first or last control this surface has.
    ///
    /// What another root hands the keyboard over with: a document walked to its
    /// end passes it on, and where it lands depends on which way the reader was
    /// going.
    pub fn focus_edge(&mut self, forward: bool) {
        self.focused = if forward {
            self.focus.next(None)
        } else {
            self.focus.previous(None)
        };
    }

    /// Put the caret in the address field, for an accelerator that names it.
    ///
    /// The whole address is selected, which is what ⌘L is *for*: the next
    /// keystroke replaces it. Nothing happens before the first frame, because
    /// until then no field has been drawn for the caret to be in.
    pub fn focus_address(&mut self) {
        // Now, against the ring as it stands, so a caller that never draws a
        // frame still sees the caret where it asked for it — and again after the
        // next build, which is the one that decides where the field really is.
        if let Some(id) = self.focus.first_text() {
            self.focused = Some(id);
        }
        self.address.select_all();
        self.address_wanted = true;
    }

    /// What a press at `x`, `y` would report, without reporting it.
    ///
    /// Asked of the tree that drew the frame — the surface knows where it drew
    /// things, and this is how anything else asks rather than working the
    /// geometry out a second time and drifting from it. The same shape every
    /// surface answers, which is what lets one test helper probe them all.
    pub fn action_at(&mut self, x: f64, y: f64, text: &mut TextEngine) -> Option<UiAction> {
        let (pointer, down) = (self.pointer, self.pointer_down);
        self.pointer = (x, y);
        self.pointer_down = false;
        let mut cx = self.cx(text);
        let action = self
            .root
            .as_mut()
            .and_then(|root| root.event(&Event::PointerPressed, &mut cx));
        self.pointer = pointer;
        self.pointer_down = down;
        action
    }

    /// What the pointer should look like at `x`, `y`, if the interface claims it.
    pub fn cursor_at(&mut self, x: f64, y: f64, text: &mut TextEngine) -> Option<Cursor> {
        match self.action_at(x, y, text) {
            Some(UiAction::Focus(_) | UiAction::AddressHit(_) | UiAction::FindHit(_)) => {
                Some(Cursor::Text)
            }
            // The sheet behind an open menu answers everywhere, and everywhere
            // is not a thing to point at: dismissing is what happens when you
            // press *nothing*, so it reads as nothing.
            Some(UiAction::DismissPopup) | None => None,
            Some(_) => Some(Cursor::Pointer),
        }
    }

    /// Whether the pointer is over the interface rather than the page.
    ///
    /// An open menu counts as the interface wherever it reaches, which is how a
    /// press on the panel stops being a press on the document under it.
    pub fn owns_pointer(&self) -> bool {
        self.pointer.1 < UI_HEIGHT || self.popup_owns(self.pointer.0, self.pointer.1)
    }

    /// Whether a press at `x`, `y` belongs to the open popup.
    ///
    /// A menu's sheet answers everywhere, which is what makes a press outside
    /// it mean *put this away* and nothing else. The omnibox's suggestions
    /// claim only the panel itself, so a press on the page is a press on the
    /// page — dismissing the list on the way through rather than swallowing
    /// the click the reader plainly meant.
    pub fn popup_owns(&self, x: f64, y: f64) -> bool {
        match self.popup.as_ref() {
            Some(popup) if popup.has_sheet() => true,
            Some(_) => self.popup_rect.get().contains(x, y),
            None => false,
        }
    }

    /// Whether a press that began in the interface still owns pointer motion.
    pub fn pointer_captured(&self) -> bool {
        self.press_origin.is_some()
    }

    /// Handle a press at the last reported pointer position.
    ///
    /// The press is offered to the tree the last frame drew. Nothing is measured
    /// again and no rectangle is worked out a second time, so a control cannot
    /// be drawn in one place and clicked in another.
    pub fn pointer_pressed(&mut self, text: &mut TextEngine, clicks: u32) -> UiAction {
        self.pointer_down = true;
        self.press_origin = Some(self.pointer);
        self.clicks = clicks;
        // A reader who has pressed is no longer asking what the thing is
        // called, and a panel left over the control they just pressed is in
        // the way of seeing what the press did.
        self.resting = None;
        if self.pointer.1 >= UI_HEIGHT && !self.popup_open() {
            // The press belongs to the page, and it takes focus away from the
            // address field — which is what every browser does, and what makes
            // typing after clicking a page do nothing surprising.
            self.focused = None;
            return UiAction::None;
        }

        let mut cx = self.cx(text);
        let action = self
            .root
            .as_mut()
            .and_then(|root| root.event(&Event::PointerPressed, &mut cx));
        // Whoever took the pointer keeps it until the button comes up. The
        // claim is read here rather than applied by the widget because the tree
        // is rebuilt every frame and a claim left in it would not survive one.
        self.capture = cx.claimed_pointer();
        if let Some(capture) = self.capture {
            let id = capture.value();
            self.drag = self
                .tab_places
                .borrow()
                .iter()
                .position(|(key, _)| *key == id)
                .map(|from| TabDrag {
                    id,
                    from,
                    origin: self.pointer.0,
                    started: false,
                });
        }

        match action {
            Some(UiAction::Focus(id)) => {
                self.dismiss_transient();
                self.focused = Some(id);
                UiAction::None
            }
            // A press in the field: the keyboard moves there and the caret goes
            // where the press landed — or the word does, or the lot, by the
            // click count. The field said where; whose keyboard it is stays
            // the surface's business.
            Some(UiAction::AddressHit(hit)) => {
                self.dismiss_transient();
                if let Some(id) = self.focus.first_text() {
                    self.focused = Some(id);
                }
                self.address.hit(hit);
                UiAction::None
            }
            // The same for the find bar's field, which is the last one built
            // because the panel it is in is drawn over everything else.
            Some(UiAction::FindHit(hit)) => {
                if let Some(id) = self.focus.last_text() {
                    self.focused = Some(id);
                }
                self.find.hit(hit);
                UiAction::None
            }
            Some(UiAction::ToggleMenu) => {
                self.toggle_menu();
                UiAction::None
            }
            // Where the strip is slid to is the interface's own, so the press
            // is answered here and the browser never hears about it.
            Some(UiAction::ScrollTabs(forward)) => {
                self.scroll_tabs_page(forward);
                UiAction::None
            }
            Some(UiAction::DismissPopup) => {
                self.close_popup(false);
                UiAction::None
            }
            // Choosing something from the menu closes it. A menu that stayed
            // open over the page it just opened would have to be dismissed by
            // hand every time.
            Some(UiAction::OpenPage(page)) => {
                self.dismiss_transient();
                UiAction::OpenPage(page)
            }
            // The same for the inspector: chosen from the menu, the menu goes
            // away and what was chosen is what happens.
            Some(UiAction::ToggleInspector) => {
                self.dismiss_transient();
                UiAction::ToggleInspector
            }
            // A row of the context menu, which is the whole of what that menu is
            // for: it goes away and the browser is told what was chosen.
            Some(UiAction::Context(command)) => {
                self.dismiss_transient();
                UiAction::Context(command)
            }
            // The bar's own cross, which is the one press that closes it.
            Some(UiAction::CloseFind) => {
                self.close_popup(true);
                UiAction::None
            }
            // Its arrows keep the keyboard where it is: a reader stepping with
            // the pointer is still typing into the field.
            Some(UiAction::FindStep(forward)) => UiAction::FindStep(forward),
            Some(action) => {
                if !matches!(
                    action,
                    UiAction::Reload | UiAction::Stop | UiAction::Back | UiAction::Forward
                ) {
                    self.focused = None;
                }
                action
            }
            None => {
                self.focused = None;
                self.dismiss_transient();
                UiAction::None
            }
        }
    }

    /// Open the menu behind the cogwheel, or put it away again.
    fn toggle_menu(&mut self) {
        if self.menu_open() {
            self.close_popup(false);
        } else {
            self.open_popup(Popup::Menu);
        }
    }

    /// Show the browser menu, for a caller that is not a press on the cogwheel.
    pub fn open_menu(&mut self) {
        if !self.menu_open() {
            self.open_popup(Popup::Menu);
        }
    }

    /// Show `rows` as a menu at `x`, `y`, where the reader asked for one.
    ///
    /// The rows are the browser's decision: what is under the pointer is a
    /// question about the document, and the interface is the wrong place to ask
    /// it. Nothing is offered at all when the list is empty, because a menu with
    /// no rows is a rectangle that only gets in the way.
    pub fn open_context_menu(&mut self, x: f64, y: f64, rows: Vec<ContextRow>) {
        if rows.is_empty() {
            return;
        }
        self.open_popup(Popup::Context { at: (x, y), rows });
    }

    /// Open one popup, which is what closes any other.
    fn open_popup(&mut self, popup: Popup) {
        self.resting = None;
        // The keyboard to come back to is the one from before *any* popup: a
        // context menu opened over an open menu must not offer its rows as the
        // place to return to.
        if self.popup.is_none() {
            self.popup_return = self.focused;
        }
        self.popup = Some(popup);
        self.focused = None;
    }

    /// Put the open popup away.
    ///
    /// `restore` gives the keyboard back to whatever held it before the popup
    /// opened, which is what leaving one by Escape means. A press dismisses
    /// without restoring: the reader is looking at where they clicked, not at
    /// the button they walked away from.
    fn close_popup(&mut self, restore: bool) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        let back = self.popup_return.take();
        // Only the keyboard that was inside the popup moves. One resting
        // somewhere else — a field the reader pressed on the way out — stays
        // exactly where the press put it.
        // Only a popup that owned the keyboard gives it back or takes it away.
        // The omnibox's suggestions never had it: the caret stayed in the field
        // the whole time, and moving it now would be the list taking something
        // it was never given.
        if let Some(scope) = popup.scope()
            && (self.focused.is_none() || self.focus.scope(self.focused) == Some(scope))
        {
            self.focused = if restore { back } else { None };
        }
    }

    /// Put any popup away because what it belongs to has gone.
    ///
    /// The browser says when that happens, because the interface cannot see it:
    /// a popup belongs to the root that opened it, and a root that stops being
    /// the active one has no business keeping a panel over the window.
    ///
    /// The find bar belongs to the page rather than to the chrome, so the
    /// keyboard moving to the document is not its root going away — it is the
    /// reader reading what they searched for. It goes when the page does, which
    /// is a different question, asked by whoever owns the page.
    pub fn dismiss_popup(&mut self) {
        self.dismiss_transient();
    }

    /// Put away only the menu the reader asked for over the page.
    ///
    /// Its rows are about a document at a point — this link, this element — and
    /// a navigation or a resize leaves every one of them describing something
    /// that is no longer there. The menu behind the cogwheel is about the
    /// browser and survives both.
    pub fn dismiss_context_menu(&mut self) {
        if matches!(self.popup, Some(Popup::Context { .. })) {
            self.close_popup(false);
        }
    }

    /// Offer these places under the address field, or none at all.
    ///
    /// The browser decides what they are and when they change; the interface
    /// only shows them. Nothing is offered unless the field has the keyboard —
    /// a list under a field nobody is typing into is a panel in the way — and
    /// a menu already open wins, because it was asked for and this was not.
    pub fn set_suggestions(&mut self, rows: Vec<Suggestion>) {
        if rows.is_empty() || !self.address_focused() {
            if matches!(self.popup, Some(Popup::Suggestions { .. })) {
                self.close_popup(false);
            }
            return;
        }
        match self.popup.as_mut() {
            Some(Popup::Suggestions {
                rows: showing,
                marked,
            }) => {
                if *showing != rows {
                    *showing = rows;
                    // What was reached is a row in the old list. Keeping the
                    // number would leave the mark on whatever now sits there.
                    *marked = None;
                }
            }
            Some(_) => {}
            None => {
                self.popup = Some(Popup::Suggestions { rows, marked: None });
                self.resting = None;
            }
        }
    }

    /// The address the marked suggestion would take, if one is marked.
    fn marked_suggestion(&self) -> Option<String> {
        let Some(Popup::Suggestions { rows, marked }) = self.popup.as_ref() else {
            return None;
        };
        rows.get((*marked)?).map(|row| row.url.clone())
    }

    /// Walk the offered places, wrapping at both ends.
    ///
    /// Past the last row is back to no row at all rather than round to the
    /// first: leaving the list is how a reader keeps what they typed.
    fn mark_suggestion(&mut self, forward: bool) {
        let Some(Popup::Suggestions { rows, marked }) = self.popup.as_mut() else {
            return;
        };
        let count = rows.len();
        *marked = match (*marked, forward) {
            (None, true) => Some(0),
            (None, false) => Some(count - 1),
            (Some(at), true) if at + 1 < count => Some(at + 1),
            (Some(_), true) => None,
            (Some(0), false) => None,
            (Some(at), false) => Some(at - 1),
        };
    }

    /// Whether the omnibox is offering anywhere to go.
    pub fn suggesting(&self) -> bool {
        matches!(self.popup, Some(Popup::Suggestions { .. }))
    }

    /// What it is offering, in the order it offers it.
    pub fn suggestions(&self) -> &[Suggestion] {
        match self.popup.as_ref() {
            Some(Popup::Suggestions { rows, .. }) => rows,
            _ => &[],
        }
    }

    /// Whether the menu behind the cogwheel is open.
    pub fn menu_open(&self) -> bool {
        self.popup == Some(Popup::Menu)
    }

    /// Whether any panel is drawn over the window.
    ///
    /// What the browser routes a press by: a popup owns the pointer wherever it
    /// reaches, so the page under it neither follows a link nor starts a
    /// selection while one is open.
    pub fn popup_open(&self) -> bool {
        self.popup.is_some()
    }

    /// The press ended: drags stop growing selections.
    pub fn pointer_released(&mut self) {
        self.pointer_down = false;
        self.press_origin = None;
        // The pointer goes back to whatever is under it. A drag that got as far
        // as moving anything has already moved it, so there is nothing to apply
        // here — dropping is letting go, and the strip is already the answer.
        self.capture = None;
        self.drag = None;
    }

    /// Abandon a drag, putting the tab back where it was picked up.
    ///
    /// Escape during a drag is the reader saying they did not mean it, and a
    /// drag that could not be taken back is one a reader has to be careful
    /// with. `None` when nothing was moved, so Escape can go on to mean
    /// whatever else it means.
    fn cancel_drag(&mut self) -> Option<UiAction> {
        let drag = self.drag.take()?;
        self.capture = None;
        drag.started.then_some(UiAction::MoveTab {
            id: drag.id,
            to: drag.from,
        })
    }

    /// Activate the control a reader named, by the index it was described at.
    ///
    /// The focus is moved onto it and then the ordinary activation runs — the
    /// same `Event::Activate` the space bar raises, reaching the same widget in
    /// the same tree. A second path that reported the action directly would be a
    /// second answer to *what does pressing this do*, and the two would agree
    /// only until one of them was changed.
    pub fn activate_described(&mut self, index: usize, text: &mut TextEngine) -> UiAction {
        let Some(focus) = self.describe().get(index).and_then(|node| node.focus) else {
            // A node with no focus id cannot be pressed: it is a label, or a
            // field whose caret is its focus. Nothing happens, which is what a
            // press on it would do.
            return UiAction::None;
        };
        self.focused = Some(focus);
        self.activate(text)
    }

    /// Activate whatever holds the keyboard, through the path a press takes.
    fn activate(&mut self, text: &mut TextEngine) -> UiAction {
        let mut cx = self.cx(text);
        let action = self
            .root
            .as_mut()
            .and_then(|root| root.event(&Event::Activate, &mut cx));
        match action {
            Some(UiAction::Focus(id)) => {
                self.focused = Some(id);
                UiAction::None
            }
            Some(UiAction::ToggleMenu) => {
                self.toggle_menu();
                UiAction::None
            }
            Some(UiAction::ScrollTabs(forward)) => {
                self.scroll_tabs_page(forward);
                UiAction::None
            }
            Some(UiAction::OpenPage(page)) => {
                self.close_popup(true);
                UiAction::OpenPage(page)
            }
            Some(UiAction::ToggleInspector) => {
                self.close_popup(true);
                UiAction::ToggleInspector
            }
            Some(UiAction::Context(command)) => {
                self.close_popup(true);
                UiAction::Context(command)
            }
            Some(action) => action,
            None => UiAction::None,
        }
    }

    /// Handle a key press. Returns what the browser should do about it.
    pub fn key_pressed(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        text: &mut TextEngine,
        clipboard: &mut dyn Clipboard,
    ) -> UiAction {
        // Typing puts away the panel naming whatever the pointer happens to be
        // resting on: it was asked for by resting, and the reader has moved on.
        self.resting = None;

        // Accelerators work whether or not the field has focus.
        // F5 reloads whatever has focus, including the address field: it is not
        // a character, so it cannot be something the user meant to type.
        if key == Key::F5 {
            return UiAction::Reload;
        }

        // Escape gets out of whatever is in progress, innermost first: a drag
        // being carried, then a panel being looked at.
        if key == Key::Escape
            && let Some(undo) = self.cancel_drag()
        {
            return undo;
        }

        if key == Key::Escape && self.popup_open() {
            self.close_popup(true);
            return UiAction::None;
        }

        // Return in the find bar's field steps rather than submits: there is
        // nothing to submit, and stepping is what a reader who has typed a query
        // and pressed Return means. Before the accelerator block, because
        // shift-Return is the only modifier it takes and it is not one.
        if self.find_focused() && key == Key::Enter {
            return UiAction::FindStep(!modifiers.shift);
        }

        // The arrows walk the offered places without taking one and without
        // moving the keyboard, which stays in the field the reader is typing
        // into. That is what a list under a field is: a longer answer to what
        // is in it, not somewhere else to be.
        if self.suggesting() && matches!(key, Key::Down | Key::Up) && !modifiers.is_accelerator() {
            self.mark_suggestion(key == Key::Down);
            return UiAction::None;
        }

        // The arrows walk an open menu, which is what every menu on every
        // platform does and what a reader who opened one from the keyboard
        // reaches for before Tab. They are the way *into* it as well: the
        // keyboard is on the cogwheel behind the sheet, and traversal out of a
        // scope it is not in enters that scope.
        if self.popup_open() && matches!(key, Key::Down | Key::Up) && !modifiers.is_accelerator() {
            self.focused = if key == Key::Down {
                self.focus.next(self.focused)
            } else {
                self.focus.previous(self.focused)
            };
            return UiAction::None;
        }

        if modifiers.is_accelerator() {
            // A focused field gets first claim on the editing accelerators —
            // ⌘C in the address bar is a copy, not a browser command. The rest
            // stay the browser's: ⌘L and ⌘R work from inside the field too.
            if self.address_focused() && self.address.edit(key, modifiers, clipboard) {
                return UiAction::None;
            }
            if self.find_focused() && self.find.edit(key, modifiers, clipboard) {
                return UiAction::None;
            }
            return match key {
                Key::Character('r') => UiAction::Reload,
                // ⌘F opens the bar and gives it the keyboard, and ⌘G steps
                // through what it found without asking for the keyboard at all —
                // which is what makes *find, then read* possible: the reader is
                // looking at the page and the bar is only keeping count.
                Key::Character('f') => {
                    self.open_find();
                    UiAction::None
                }
                Key::Character('g') => UiAction::FindStep(!modifiers.shift),
                // The bracket keys are what this platform's browsers use, and the
                // arrows are what the rest of them use; both are here because a
                // person's fingers know one of the two.
                Key::Character('[') | Key::Left => UiAction::Back,
                Key::Character(']') | Key::Right => UiAction::Forward,
                Key::Character('t') => UiAction::NewTab,
                Key::Character('l') => {
                    // An accelerator that names a control is the keyboard
                    // leaving the menu, and a popup a keyboard has left is a
                    // popup that is closed — unless it is the find bar, which
                    // is a mode rather than a choice and stays where it is.
                    self.dismiss_transient();
                    self.focus_address();
                    UiAction::None
                }
                _ => UiAction::None,
            };
        }

        // Traversal, before anything a control might read the key as: Tab is
        // never a character the address field wants.
        if key == Key::Tab {
            // At the end of the chrome's own order, the keyboard belongs to
            // what is beyond it — the document — rather than back at this
            // surface's other end. A popup that owns the keyboard keeps it:
            // that is what makes it a trap.
            let forward = !modifiers.shift;
            if self.focus.scope(self.focused).is_none()
                && self.focused.is_some()
                && self.focused == self.focus.edge(forward)
            {
                self.focused = None;
                return UiAction::LeaveChrome(forward);
            }
            self.focused = if modifiers.shift {
                self.focus.previous(self.focused)
            } else {
                self.focus.next(self.focused)
            };
            return UiAction::None;
        }

        // The find bar's own field takes what is typed into it, the way the
        // address field does. Not Escape, Tab or Return: all three were answered
        // above, because what they mean here is about the bar rather than about
        // the text in it.
        if self.find_focused() {
            self.find.edit(key, modifiers, clipboard);
            return UiAction::None;
        }

        if !self.address_focused() {
            // Space and Return on anything else are what a press on it would be,
            // reported through the same path so the two cannot diverge.
            if matches!(key, Key::Enter | Key::Character(' ')) && self.focused.is_some() {
                return self.activate(text);
            }
            if key == Key::Escape {
                self.focused = None;
            }
            return UiAction::None;
        }

        match key {
            Key::Enter => {
                // What the arrows reached, if they reached anything; otherwise
                // what was typed. Taking the mark is the only thing that takes
                // it — walking never put it in the field.
                let taken = self.marked_suggestion();
                self.close_popup(false);
                self.focused = None;
                let typed = taken.unwrap_or_else(|| self.address.text().trim().to_owned());
                if typed.is_empty() {
                    UiAction::None
                } else {
                    UiAction::Navigate(typed)
                }
            }
            Key::Escape => {
                self.focused = None;
                UiAction::None
            }
            _ => {
                self.address.edit(key, modifiers, clipboard);
                UiAction::None
            }
        }
    }

    /// Handle typed text. Returns whether the interface consumed it.
    pub fn text_input(&mut self, character: char) -> bool {
        if self.find_focused() {
            self.find.insert(character);
            return true;
        }
        if !self.address_focused() {
            return false;
        }
        self.address.insert(character);
        true
    }

    /// How far the tab strip is slid along.
    pub fn tab_scroll(&self) -> f64 {
        self.tab_scroll
    }

    /// Slide the strip by `delta` logical pixels, stopping at the ends.
    pub fn scroll_tabs_by(&mut self, delta: f64) {
        self.tab_scroll = (self.tab_scroll + delta).clamp(0.0, self.tab_overflow.get());
    }

    /// Slide it by most of a screenful, which is what a chevron means.
    ///
    /// Most rather than all: a page that moved exactly its own width would put
    /// the tab that was at the edge just past the other edge, and a person
    /// following a run of tabs would lose their place at every press.
    fn scroll_tabs_page(&mut self, forward: bool) {
        let window = self.tab_window.get().width;
        let step = (window * 0.8).max(TAB_MIN_WIDTH);
        self.scroll_tabs_by(if forward { step } else { -step });
    }

    /// Bring the active tab back onto the strip if it has gone off an end.
    ///
    /// Against the rectangle the last frame placed it at, which is the only
    /// account of where it is. A tab off the left is brought to the left edge
    /// and one off the right to the right edge, so the strip moves as little as
    /// it can — a tab that jumped to the middle would take every other tab with
    /// it for no reason the person pressing could see.
    fn reveal_active_tab(&mut self) {
        let travel = self.tab_overflow.get();
        if travel <= 0.0 {
            // Nothing to slide, and anything left over from when there was
            // would be a strip scrolled past a strip that now fits.
            self.tab_scroll = 0.0;
            return;
        }
        let tab = self.active_tab.get();
        if tab.width <= 0.0 {
            return;
        }
        // The window the strip shows, in the same coordinates the tab was placed
        // in: it was placed inside the scroll, so it has already had the offset
        // taken off it.
        let window = self.tab_window.get();
        if window.width <= 0.0 {
            return;
        }
        let (left, right) = (window.x, window.x + window.width);
        let shift = if tab.x < left {
            tab.x - left
        } else if tab.x + tab.width > right {
            tab.x + tab.width - right
        } else {
            return;
        };
        self.tab_scroll = (self.tab_scroll + shift).clamp(0.0, travel);
    }

    /// Paint the interface across `width` logical pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn build_display_list(
        &mut self,
        width: f64,
        height: f64,
        tabs: &[TabLabel],
        active: usize,
        history: (bool, bool),
        spinner: Option<f32>,
        text: &mut TextEngine,
    ) -> std::sync::Arc<DisplayList> {
        // Before the key is taken, so a frame that has to slide is the frame
        // that is built rather than the one after it. What it reads is the last
        // frame's geometry, which is the rule every hit test here already keeps.
        self.reveal_active_tab();

        let mut appearance = Appearance {
            width,
            tabs: tabs
                .iter()
                .map(|tab| (tab.id, tab.title.clone(), tab.loading))
                .collect(),
            active,
            history,
            spinner,
            // A pointer over the page below hovers nothing in the toolbar, so its
            // exact position there is not something the toolbar is drawn from:
            // every such position is collapsed to one, or the toolbar would be
            // rebuilt — every tab title reshaped — on each pixel the pointer moved
            // over the document, which is what made scrolling with the mouse
            // moving lag. A press in progress and an open menu both reach past the
            // toolbar's edge, so the real pointer stands then.
            pointer: if self.pointer.1 >= UI_HEIGHT && !self.popup_open() && !self.pointer_down {
                (-1.0, -1.0)
            } else {
                self.pointer
            },
            pointer_down: self.pointer_down,
            address: self.address.text().to_owned(),
            caret: self.address_focused().then(|| self.address.caret()),
            selection: self
                .address_focused()
                .then(|| self.address.selection())
                .flatten(),
            focus: self.focused,
            popup: self.popup.clone().map(|popup| (height, popup)),
            find: self.finding().then(|| FindLook {
                text: self.find.text().to_owned(),
                caret: self.find_focused().then(|| self.find.caret()),
                selection: self.find_focused().then(|| self.find.selection()).flatten(),
                status: self.find_status,
            }),
            tooltip: self
                .tooltip()
                .map(|(label, anchor)| (label.to_owned(), anchor)),
            bookmark: self.bookmark,
            zoom: self.zoom,
            tab_scroll: self.tab_scroll,
        };

        // Nothing it is drawn from has moved, so last frame's list is this
        // frame's list. The tree is kept too, so a press still meets the
        // rectangles that are on screen.
        if let Some((built, list_of)) = &self.cache
            && *built == appearance
            && self.root.is_some()
        {
            return std::sync::Arc::clone(list_of);
        }

        let address_only = self
            .cache
            .as_ref()
            .is_some_and(|(built, _)| built.same_except_address(&appearance));
        self.prepare_retained(&appearance, tabs, active, history, spinner, text);
        self.builds += 1;
        let mut built = DisplayList::new();
        let list = &mut built;
        let theme = self.theme.clone();

        // The two surfaces, painted before the tree so that everything the tree
        // draws lands on top of them. The strip is recessed and the toolbar is
        // raised, which is what lets the active tab merge downward into it.
        fill_rounded(
            list,
            Rect::new(0.0, 0.0, width, TAB_STRIP_HEIGHT),
            theme.surface,
            0.0,
        );
        fill_rounded(
            list,
            Rect::new(0.0, TAB_STRIP_HEIGHT, width, TOOLBAR_HEIGHT),
            theme.raised,
            0.0,
        );

        // The tree covers the whole window rather than the interface's own
        // band: an open menu hangs below the toolbar, and both drawing and hit
        // testing have to reach it there.
        let surface = Size::new(width, height.max(UI_HEIGHT));
        let mut cx = self.cx(text);
        let mut root = self.build(&mut cx);
        root.measure(surface, &mut cx);
        root.place(Rect::new(0.0, 0.0, surface.width, surface.height), &mut cx);
        root.draw(&mut cx, list);
        let tab_nodes = self
            .tab_runtime
            .children(self.tab_runtime_root)
            .unwrap_or_default()
            .to_vec();
        for node in tab_nodes {
            self.tab_runtime.clear_dirty(node, UiDirty::ALL);
        }
        self.tab_runtime
            .clear_dirty(self.tab_runtime_root, UiDirty::ALL);

        // The line the page starts under. Drawn last so nothing overlaps it, and
        // it is what tells the eye where the browser stops and the document
        // begins — without it a white toolbar and a white page are one surface.
        controls::hairline(
            &theme,
            list,
            Rect::new(
                0.0,
                UI_HEIGHT - theme.hairline_width,
                width,
                theme.hairline_width,
            ),
        );

        self.root = Some(root);
        // The address bar, asked for before this ring existed. Resolved here,
        // against the ring this build just filled, because that is the first
        // moment the answer is a position that means anything.
        if self.address_wanted {
            self.address_wanted = false;
            if let Some(id) = self.focus.first_text() {
                self.focused = Some(id);
                appearance.focus = Some(id);
            }
        }
        // A field that took the keyboard as it was built. The key is corrected
        // rather than left alone: it says what this list was drawn from, and a
        // key claiming the keyboard was elsewhere would let this frame — the one
        // with the caret in it — be handed back for a frame without one.
        if let Some(id) = self.focus_granted.take() {
            self.focused = Some(id);
            appearance.focus = Some(id);
            appearance.find = self.finding().then(|| FindLook {
                text: self.find.text().to_owned(),
                caret: Some(self.find.caret()),
                selection: self.find.selection(),
                status: self.find_status,
            });
        }
        // Where the field is, from the frame that placed it: the suggestions
        // hang under it, and a rectangle worked out a second time from the
        // toolbar's arithmetic would part company with the one on screen.
        let field = {
            let mut described = Vec::new();
            self.root
                .as_ref()
                .expect("the chrome tree was just built")
                .describe(&mut described);
            described
                .into_iter()
                .find(|node| node.role == Role::TextInput)
                .expect("the toolbar has an address field")
                .rect
        };
        self.address_rect = field;
        // The focus halo reaches three logical pixels outside the field and
        // antialiasing can touch one more.
        self.dirty = address_only.then(|| field.inflate(4.0));
        let built = std::sync::Arc::new(built);
        self.cache = Some((appearance, std::sync::Arc::clone(&built)));
        built
    }

    /// Update only retained boundaries whose visible inputs changed.
    #[allow(clippy::too_many_arguments)]
    fn prepare_retained(
        &mut self,
        appearance: &Appearance,
        tabs: &[TabLabel],
        active: usize,
        history: (bool, bool),
        spinner: Option<f32>,
        text: &mut TextEngine,
    ) {
        let tab_pointer = (appearance.pointer.0 >= 0.0
            && appearance.pointer.1 >= 0.0
            && appearance.pointer.1 < TAB_STRIP_HEIGHT)
            .then_some((
                appearance.pointer.0,
                appearance.pointer.1,
                appearance.pointer_down,
            ));
        let toolbar_pointer = (appearance.pointer.0 >= 0.0
            && appearance.pointer.1 >= TAB_STRIP_HEIGHT
            && appearance.pointer.1 < UI_HEIGHT)
            .then_some((
                appearance.pointer.0,
                appearance.pointer.1,
                appearance.pointer_down,
            ));
        let tab_appearance = TabAppearance {
            width: appearance.width,
            tabs: appearance.tabs.clone(),
            active: appearance.active,
            spinner: appearance.spinner,
            pointer: tab_pointer,
            focus: appearance.focus,
            tab_scroll: appearance.tab_scroll,
        };
        let toolbar_appearance = ToolbarAppearance {
            width: appearance.width,
            history: appearance.history,
            spinner: appearance.spinner,
            pointer: toolbar_pointer,
            address: appearance.address.clone(),
            caret: appearance.caret,
            selection: appearance.selection.clone(),
            focus: appearance.focus,
            bookmark: appearance.bookmark,
        };

        let previous = self.tab_appearance.as_ref();
        let previous_active = previous
            .and_then(|state| state.tabs.get(state.active))
            .map(|tab| tab.0);
        let current_active = tabs.get(active).map(|tab| tab.id);
        let geometry_changed = previous.is_none_or(|state| {
            state.width != appearance.width || state.tab_scroll != appearance.tab_scroll
        });
        let specs = tabs.iter().map(|tab| {
            let old = previous.and_then(|state| state.tabs.iter().find(|old| old.0 == tab.id));
            let mut dirty = UiDirty::default();
            if geometry_changed {
                dirty = dirty.union(UiDirty::LAYOUT).union(UiDirty::SEMANTICS);
            }
            if old.is_none_or(|old| old.1 != tab.title || old.2 != tab.loading)
                || previous.is_some_and(|state| state.focus != appearance.focus)
                || previous_active != current_active
                    && (previous_active == Some(tab.id) || current_active == Some(tab.id))
                || previous.is_some_and(|state| state.spinner != spinner)
                    && (tab.loading || old.is_some_and(|old| old.2))
            {
                dirty = dirty.union(UiDirty::PAINT).union(UiDirty::SEMANTICS);
            }
            NodeSpec::new::<TabRenderNode>()
                .keyed(WidgetKey::from_u64(tab.id))
                .changed(dirty)
        });
        let tab_nodes = self
            .tab_runtime
            .reconcile_children(self.tab_runtime_root, specs);
        self.tab_runtime_work = tab_nodes
            .iter()
            .filter_map(|id| self.tab_runtime.dirty(*id).map(|dirty| (*id, dirty)))
            .collect();

        let tab_changed = self.tab_appearance.as_ref() != Some(&tab_appearance);
        let tab_semantics_dirty = self
            .tab_runtime_work
            .iter()
            .any(|(_, dirty)| dirty.contains(UiDirty::SEMANTICS));
        let toolbar_semantics_dirty = self.toolbar_appearance.as_ref().is_none_or(|old| {
            old.width != toolbar_appearance.width
                || old.history != toolbar_appearance.history
                || old.address != toolbar_appearance.address
                || old.caret != toolbar_appearance.caret
                || old.selection != toolbar_appearance.selection
                || old.focus != toolbar_appearance.focus
        });
        let toolbar_changed =
            tab_changed || self.toolbar_appearance.as_ref() != Some(&toolbar_appearance);

        if tab_changed {
            self.focus.begin();
            let theme = self.theme.clone();
            let focus = self.focus.clone();
            let mut cx = self.cx(text);
            let child = tab_strip(
                &theme,
                &focus,
                &mut cx,
                appearance.width,
                tabs,
                active,
                spinner,
                &Sliding {
                    scroll: self.tab_scroll,
                    overflow: &self.tab_overflow,
                    active_tab: &self.active_tab,
                    window: &self.tab_window,
                    places: &self.tab_places,
                },
            );
            self.tab_tree.replace_with_dirty(
                child,
                if tab_semantics_dirty {
                    UiDirty::SEMANTICS
                } else {
                    UiDirty::PAINT
                },
            );
            self.tab_focus_end = self.focus.len();
        } else if toolbar_changed {
            self.focus.truncate(self.tab_focus_end);
        }

        if toolbar_changed {
            let theme = self.theme.clone();
            let focus = self.focus.clone();
            self.toolbar_tree.replace_with_dirty(
                toolbar(&theme, &focus, self, history, spinner),
                if toolbar_semantics_dirty {
                    UiDirty::SEMANTICS
                } else {
                    UiDirty::PAINT
                },
            );
            self.toolbar_focus_end = self.focus.len();
        }

        // Menu controls are still in the short-lived adapter and are rebuilt
        // after this point. Remove their previous suffix while retaining the
        // stable ids owned by the two migrated boundaries.
        self.focus.truncate(self.toolbar_focus_end);
        self.tab_appearance = Some(tab_appearance);
        self.toolbar_appearance = Some(toolbar_appearance);
    }

    /// A drawing context over `text`, carrying this interface's pointer and theme.
    fn cx<'a>(&self, text: &'a mut TextEngine) -> Cx<'a> {
        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.clicks = self.clicks;
        cx.focus = self.focused;
        cx.capture = self.capture;
        cx.theme = self.theme.clone();
        cx
    }

    /// Build the short-lived parent around two persistent migration boundaries.
    fn build(&self, cx: &mut Cx) -> Child<UiAction> {
        let theme = self.theme.clone();
        let focus = self.focus.clone();
        // A column with an empty flexible tail rather than an aligner: an
        // aligner would shrink the interface to what it measured, and what the
        // toolbar measures is its buttons — not the window it has to span.
        let rows: Child<UiAction> = Box::new(Stack::column(
            0.0,
            vec![
                Box::new(Fixed::height(
                    UI_HEIGHT,
                    Box::new(Stack::column(
                        0.0,
                        vec![
                            Box::new(Fixed::height(TAB_STRIP_HEIGHT, self.tab_tree.widget())),
                            Box::new(Fixed::height(TOOLBAR_HEIGHT, self.toolbar_tree.widget())),
                        ],
                    )),
                )),
                Box::new(crate::widget::Flex::new(
                    1.0,
                    Box::new(crate::widget::Gap::new(0.0, 0.0)),
                )),
            ],
        ));

        let Some(popup) = self.popup.as_ref() else {
            // A tooltip is not a popup: it takes no focus, catches no press and
            // is dismissed by the pointer moving off what it names. It hangs
            // just under the control, flipped above it near the bottom edge.
            let Some((label, anchor)) = self.tooltip() else {
                return rows;
            };
            return Box::new(crate::widget::Overlay::new(vec![
                rows,
                Box::new(
                    crate::widget::Anchored::at(
                        anchor.x,
                        anchor.y + anchor.height + 4.0,
                        controls::tooltip(&theme, label),
                    )
                    .flipped(),
                ),
            ]));
        };

        // Its rows claim their ids inside the popup's own scope, which is what
        // keeps Tab on the panel while it is open. A popup that does not own
        // the keyboard opens none: its rows are not places the keyboard goes.
        if let Some(scope) = popup.scope() {
            focus.open_scope(scope);
        }
        // Each panel reports where it landed — inside its anchor rather than
        // around it, or the rectangle would be the whole window the anchor was
        // given rather than the panel it placed inside it.
        let reported = |panel: Child<UiAction>| -> Child<UiAction> {
            Box::new(crate::widget::Report::new(
                std::rc::Rc::clone(&self.popup_rect),
                panel,
            ))
        };
        let panel: Child<UiAction> = match popup {
            Popup::Menu => Box::new(crate::widget::Anchored::from_right(
                theme.inset,
                UI_HEIGHT - 2.0,
                reported(menu(&theme, &focus, self.bookmark, self.zoom)),
            )),
            // Under the field, as wide as the field, because it is a longer
            // answer to what is in it — a panel of another width would read as
            // a different thing rather than as more of the same one.
            Popup::Suggestions { rows, marked } => Box::new(crate::widget::Anchored::at(
                self.address_rect.x,
                self.address_rect.y + self.address_rect.height + 4.0,
                reported(Box::new(Fixed::width(
                    self.address_rect.width,
                    suggestions(&theme, cx, self.address_rect.width, rows, *marked),
                ))),
            )),
            // Under the toolbar at the right-hand end, which is where every
            // browser puts it: out of the way of the text a page begins with,
            // and against the edge the reader's eye is not reading along.
            Popup::Find => Box::new(crate::widget::Anchored::from_right(
                theme.inset,
                UI_HEIGHT + theme.gap,
                reported(find_bar(
                    &theme,
                    &focus,
                    &self.find,
                    self.focused,
                    self.find_status,
                    &self.find_takes_keyboard,
                    &self.focus_granted,
                )),
            )),
            // At the pointer, and flipped back onto the window at an edge: a
            // menu asked for near the bottom right belongs above and to the
            // left of the press, not half off the window.
            Popup::Context { at, rows } => Box::new(
                crate::widget::Anchored::at(
                    at.0,
                    at.1,
                    reported(context_menu(&theme, &focus, rows)),
                )
                .flipped(),
            ),
        };
        if popup.scope().is_some() {
            focus.close_scope();
        }
        // Panel first in the list so it is drawn last and answers first; the
        // sheet under it catches every press that misses, which is what makes
        // clicking anywhere else dismiss the popup without also doing whatever
        // was under the pointer.
        let mut layers: Vec<Child<UiAction>> = vec![rows];
        if popup.has_sheet() {
            layers.push(controls::scrim(UiAction::DismissPopup));
        }
        layers.push(panel);
        Box::new(crate::widget::Overlay::new(layers))
    }
}

/// The strip of tabs, and the button that opens another.
///
/// Tabs shrink to share the strip, down to a floor. Past that floor they no
/// longer shrink — a tab narrower than its own close cross is a tab that cannot
/// be read or shut — and the strip slides instead, with a chevron at whichever
/// end still has tabs beyond it. A tab you have opened is a tab you can reach.
#[allow(clippy::too_many_arguments)]
fn tab_strip(
    theme: &Theme,
    focus: &Focus,
    cx: &mut Cx,
    width: f64,
    tabs: &[TabLabel],
    active: usize,
    spinner: Option<f32>,
    sliding: &Sliding<'_>,
) -> Child<UiAction> {
    let Sliding {
        scroll,
        overflow,
        active_tab,
        window,
        places,
    } = *sliding;
    let inset = theme.inset * 0.75;
    let fixed = inset * 2.0 + NEW_TAB_SIZE + theme.gap;
    // What the tabs may share before anything scrolls. The chevrons take their
    // room from the same total, and only when they are there — a strip that
    // reserved space for them would be narrower than it needs to be in the
    // ordinary case of a few tabs.
    let plain = (width - fixed).max(0.0);
    let each = if tabs.is_empty() {
        TAB_MAX_WIDTH
    } else {
        TAB_MAX_WIDTH
            .min(plain / tabs.len() as f64 - TAB_GAP)
            .max(TAB_MIN_WIDTH)
    };
    // Whether the tabs at that width need more room than there is. The strip's
    // own arithmetic rather than the placed overflow, because what is being
    // decided here is whether to build the chevrons at all, and that has to be
    // known before anything is measured.
    let wanted = tabs.len() as f64 * (each + TAB_GAP);
    let scrolls = wanted > plain;
    let available = if scrolls {
        (plain - CHEVRON_SIZE * 2.0 - theme.gap * 2.0).max(0.0)
    } else {
        plain
    };
    let travel = (wanted - available).max(0.0);
    let scroll = scroll.clamp(0.0, travel);

    let mut children: Vec<Child<UiAction>> = Vec::with_capacity(tabs.len() * 2 + 2);
    // Stale entries — a tab that has since been closed — would answer a drag
    // with a rectangle nothing is drawn in, so the list is the tabs this frame
    // builds and only those.
    places.borrow_mut().clear();
    for (index, label) in tabs.iter().enumerate() {
        let one = tab(
            theme,
            focus,
            cx,
            label,
            index,
            index == active,
            each,
            spinner,
        );
        // Every tab reports where it landed, which is what a drag reads to ask
        // which tab the pointer is over.
        let one: Child<UiAction> = Box::new(crate::widget::Track::new(
            std::rc::Rc::clone(places),
            label.id,
            one,
        ));
        // The active one reports its rectangle a second time, on its own, so
        // the strip can be slid to bring it back when it has gone off an end.
        children.push(if index == active {
            Box::new(crate::widget::Report::new(
                std::rc::Rc::clone(active_tab),
                one,
            ))
        } else {
            one
        });
        // A hairline between two tabs that are both in the background, so a run
        // of them reads as several rather than as one wide empty area. Beside
        // the active tab there is nothing to separate: its own edge does that.
        let next_is_active = index + 1 == active;
        if index + 1 < tabs.len() && index != active && !next_is_active {
            children.push(separator(theme));
        }
    }
    let new_tab = |focus: &Focus| {
        controls::icon_button(theme, focus, UiAction::NewTab, true, "New tab", icon::plus)
    };
    let pad = |row: Child<UiAction>| -> Child<UiAction> {
        Box::new(Padding::new(
            Insets {
                left: inset,
                top: 4.0,
                right: inset,
                bottom: 0.0,
            },
            row,
        ))
    };

    // While they fit, nothing scrolls and nothing is pinned: the button that
    // opens a tab sits directly after the last one, which is where it is
    // reached for, and the leftover is empty strip.
    if !scrolls {
        children.push(new_tab(focus));
        children.push(Box::new(crate::widget::Flex::new(
            1.0,
            Box::new(crate::widget::Gap::new(0.0, 0.0)),
        )));
        overflow.set(0.0);
        window.set(crate::widget::Rect::ZERO);
        return pad(Box::new(Stack::row(TAB_GAP, children)));
    }

    // Past the floor the tabs no longer shrink, so the strip slides instead. The
    // button that opens a tab is pinned outside the sliding part — a new tab has
    // to be openable whatever the strip is scrolled to.
    children.push(Box::new(crate::widget::Flex::new(
        1.0,
        Box::new(crate::widget::Gap::new(0.0, 0.0)),
    )));
    let strip: Child<UiAction> = Box::new(Stack::row(TAB_GAP, children));

    // Dimmed at the end it can go no further in, rather than taken away: a
    // control that came and went would move everything beside it, and a strip
    // whose tabs shifted under the pointer as it scrolled is one that is hard to
    // aim at.
    pad(Box::new(Stack::row(
        theme.gap,
        vec![
            chevron_button(theme, focus, false, scroll > 0.5),
            Box::new(crate::widget::Flex::new(
                1.0,
                Box::new(crate::widget::Report::new(
                    std::rc::Rc::clone(window),
                    Box::new(
                        crate::widget::Scroll::row(scroll, std::rc::Rc::clone(overflow), strip)
                            .bar(false),
                    ),
                )),
            )),
            chevron_button(theme, focus, true, scroll < travel - 0.5),
            new_tab(focus),
        ],
    )))
}

/// Everything about a strip that has more tabs than it can show at once.
///
/// One parameter rather than four, because they are one thing: where the strip
/// is slid to, how far it may slide, and the two rectangles that answer whether
/// the tab being read is on screen.
struct Sliding<'a> {
    /// How far along the strip is.
    scroll: f64,
    /// How far it could be, written by the frame that places it.
    overflow: &'a crate::widget::Overflow,
    /// Where the active tab landed.
    active_tab: &'a crate::widget::Placed,
    /// And the window it has to land inside.
    window: &'a crate::widget::Placed,
    /// Where every tab landed, by tab id, for a drag to read.
    places: &'a crate::widget::Placements,
}

/// One end of the strip: a chevron that slides it a screenful that way.
fn chevron_button(theme: &Theme, focus: &Focus, forward: bool, enabled: bool) -> Child<UiAction> {
    let direction = if forward {
        icon::Direction::Right
    } else {
        icon::Direction::Left
    };
    Box::new(crate::widget::Fixed::width(
        CHEVRON_SIZE,
        controls::icon_button(
            theme,
            focus,
            UiAction::ScrollTabs(forward),
            enabled,
            if forward {
                "Later tabs"
            } else {
                "Earlier tabs"
            },
            move |list, rect, color| icon::chevron(list, rect, direction, color),
        ),
    ))
}

/// The menu behind the cogwheel: everything the browser is, as opposed to
/// everything a page is.
fn menu(theme: &Theme, focus: &Focus, bookmark: Bookmarked, zoom: f32) -> Child<UiAction> {
    use SystemPage::{About, Bookmarks, Downloads, History, Settings};

    let row = |page: SystemPage,
               mark: fn(&mut DisplayList, Rect, otlyra_gfx::peniko::Color),
               shortcut: Option<&str>| {
        controls::menu_item(
            theme,
            focus,
            UiAction::OpenPage(page),
            page.available(),
            mark,
            page.label(),
            shortcut,
        )
    };

    controls::menu_panel(
        theme,
        248.0,
        vec![
            controls::menu_heading(theme, "Otlyra"),
            row(Settings, icon::gear, Some("⌘,")),
            row(History, icon::clock, Some("⌘Y")),
            row(Bookmarks, icon::star, Some("⌥⌘B")),
            row(Downloads, icon::download, Some("⌘⇧J")),
            controls::divider(theme),
            // Below the line, with the other thing that acts on what is open
            // rather than opening something. The one item on this menu whose words
            // depend on the page: the native menu bar is built once at startup and
            // cannot yet be relabelled, so this is where a reader is told which way
            // ⌘D will go.
            controls::menu_item(
                theme,
                focus,
                UiAction::ToggleBookmark,
                bookmark != Bookmarked::Impossible,
                icon::star,
                if bookmark == Bookmarked::Yes {
                    "Remove bookmark"
                } else {
                    "Bookmark this page"
                },
                Some("⌘D"),
            ),
            // What the page is drawn at, and the way back to its own size. The
            // only place a reader is told: the native menu bar is built once at
            // startup and cannot be relabelled, so a page left at 125% would
            // otherwise say so nowhere at all.
            controls::menu_item(
                theme,
                focus,
                UiAction::ResetZoom,
                (zoom - 1.0).abs() > f32::EPSILON,
                icon::page,
                format!("Zoom — {}%", (zoom * 100.0).round() as i32),
                Some("⌘0"),
            ),
            controls::menu_item(
                theme,
                focus,
                UiAction::ToggleInspector,
                true,
                icon::page,
                "Inspect",
                Some("⌥⌘I"),
            ),
            controls::divider(theme),
            row(About, icon::info, None),
        ],
    )
}

/// The menu the reader asked for, over whatever they asked on.
///
/// The same rows the browser menu is made of, so a context menu looks like a
/// menu rather than like a second idea of one, and so anything learned about
/// hover, disabled rows, keyboard traversal or accessibility applies to both.
fn context_menu(theme: &Theme, focus: &Focus, rows: &[ContextRow]) -> Child<UiAction> {
    let built = rows
        .iter()
        .map(|row| match row {
            ContextRow::Command(command, enabled) => controls::menu_item(
                theme,
                focus,
                UiAction::Context(*command),
                *enabled,
                command.mark(),
                command.label(),
                command.shortcut(),
            ),
            ContextRow::Divider => controls::divider(theme),
        })
        .collect();
    controls::menu_panel(theme, 236.0, built)
}

/// What the omnibox offers under what has been typed.
///
/// A row says what the page is called and where it goes, because a title alone
/// is not enough to tell two pages of a site apart and an address alone is not
/// what a person remembers a page by. The marked row is drawn as though the
/// pointer were on it: the arrows and the pointer reach the same rows, and
/// there is no second way of showing which one is about to be taken.
/// The bar that looks for a run of characters in the page.
///
/// A field, what it found, the two ways through it, and the cross that puts it
/// away — in that order, which is the order a reader uses them in and therefore
/// the order Tab walks them in.
///
/// The field takes the keyboard here rather than where ⌘F was pressed, because
/// its focus id does not exist until this builds it: `takes_keyboard` is the
/// wish and `granted` is how the frame tells the surface what it did about it.
///
/// Which is also why it is handed `keyboard` — where the focus is — rather than
/// *whether the field has it*. That question is answered against the ids a frame
/// claimed, and this frame has not claimed the field's yet when it is asked: the
/// answer was always no, and the bar drew without a caret in it.
fn find_bar(
    theme: &Theme,
    focus: &Focus,
    field: &TextField,
    keyboard: Option<FocusId>,
    status: FindStatus,
    takes_keyboard: &std::cell::Cell<bool>,
    granted: &std::cell::Cell<Option<FocusId>>,
) -> Child<UiAction> {
    /// How wide the field is. Wide enough for a phrase, narrow enough that the
    /// bar does not cover the page it is searching.
    const FIELD_WIDTH: f64 = 220.0;

    let field_id = focus.claim_text(true);
    let focused = if takes_keyboard.replace(false) {
        granted.set(Some(field_id));
        true
    } else {
        keyboard == Some(field_id)
    };
    let input = Box::new(Fixed::width(
        FIELD_WIDTH,
        TextInput::new(
            FieldView {
                text: field.text().to_owned(),
                caret: focused.then(|| field.caret()),
                selection: focused.then(|| field.selection()).flatten(),
                placeholder: "Find in page".to_owned(),
            },
            UiAction::FindHit,
        )
        .face(theme.surface)
        .into_widget(theme),
    )) as Child<UiAction>;

    // *3 of 17*, and *0 of 0* where there is nothing: a bar that said nothing
    // about a query that found nothing would look like a bar that had not been
    // asked yet.
    let count = Box::new(Align::centre(Box::new(Label::new(
        format!("{} of {}", status.current, status.total),
        theme.font_size_small,
        if status.total == 0 {
            theme.ink_dim
        } else {
            theme.ink
        },
    )))) as Child<UiAction>;

    let steps = status.total > 0;
    let row = Stack::row(
        theme.gap,
        vec![
            input,
            count,
            controls::icon_button(
                theme,
                focus,
                UiAction::FindStep(false),
                steps,
                "Previous match",
                |list, rect, color| icon::chevron(list, rect, icon::Direction::Up, color),
            ),
            controls::icon_button(
                theme,
                focus,
                UiAction::FindStep(true),
                steps,
                "Next match",
                |list, rect, color| icon::chevron(list, rect, icon::Direction::Down, color),
            ),
            controls::icon_button(
                theme,
                focus,
                UiAction::CloseFind,
                true,
                "Close find bar",
                icon::cross,
            ),
        ],
    );

    Box::new(Background::new(
        theme.raised,
        theme.radius,
        Box::new(controls::Outline::new(
            theme.border,
            theme.radius,
            Box::new(Padding::new(Insets::all(theme.gap), Box::new(row))),
        )),
    ))
}

fn suggestions(
    theme: &Theme,
    cx: &mut Cx,
    width: f64,
    rows: &[Suggestion],
    marked: Option<usize>,
) -> Child<UiAction> {
    // What is left for the words once the mark, the gaps and the panel's own
    // padding have had their share. Cut here, with the engine that will draw
    // them: a title that overflowed would run out of the panel.
    let room = (width - 16.0 - theme.gap * 4.5 - theme.inset).max(0.0);
    let built = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let title = controls::elide(cx, &row.title, room, theme.font_size, Elide::End);
            // An address is cut at the *front*: what tells two of them apart is
            // usually the end of the path, and the host is already the part a
            // reader can guess.
            let url = controls::elide(cx, &row.url, room, theme.font_size_small, Elide::Start);
            let ink = theme.ink;
            let dim = theme.ink_dim;
            let kept = row.kept;
            let mark: Child<UiAction> = Box::new(Align::centre(Box::new(Painted::new(
                16.0,
                16.0,
                move |rect, _cx, list| {
                    if kept {
                        icon::star(list, rect, ink);
                    } else {
                        icon::clock(list, rect, dim);
                    }
                },
            ))));
            let words = Stack::column(
                0.0,
                vec![
                    Box::new(Align::left(Box::new(Label::new(
                        title,
                        theme.font_size,
                        theme.ink,
                    )))),
                    Box::new(Align::left(Box::new(Label::new(
                        url,
                        theme.font_size_small,
                        theme.ink_dim,
                    )))),
                ],
            );
            let mut face = Background::new(
                if marked == Some(index) {
                    theme.hover
                } else {
                    Theme::CLEAR
                },
                theme.radius_small,
                Box::new(Padding::new(
                    Insets::symmetric(theme.gap * 1.5, theme.gap * 0.5),
                    Box::new(Stack::row(
                        theme.gap * 1.5,
                        vec![
                            mark,
                            Box::new(crate::widget::Flex::new(1.0, Box::new(words))),
                        ],
                    )),
                )),
            );
            face = face.on_hover(theme.hover).on_press(theme.press);
            Box::new(Button::new(
                UiAction::Navigate(row.url.clone()),
                Box::new(face),
            )) as Child<UiAction>
        })
        .collect();

    Box::new(Background::new(
        theme.raised,
        theme.radius,
        Box::new(controls::Outline::new(
            theme.border,
            theme.radius,
            Box::new(Padding::new(
                Insets::all(theme.gap * 0.75),
                Box::new(Stack::column(1.0, built)),
            )),
        )),
    ))
}

/// The hairline between two background tabs.
fn separator(theme: &Theme) -> Child<UiAction> {
    let color = theme.hairline;
    Box::new(Fixed::width(
        1.0,
        Box::new(Painted::new(1.0, 16.0, move |rect, _cx, list| {
            let height = 16.0;
            fill_rounded(
                list,
                Rect::new(
                    rect.x,
                    rect.y + (rect.height - height) / 2.0,
                    1.0,
                    height.min(rect.height),
                ),
                color,
                0.0,
            );
        })),
    ))
}

/// One tab: a mark, a title, and a cross.
#[allow(clippy::too_many_arguments)]
fn tab(
    theme: &Theme,
    focus: &Focus,
    cx: &mut Cx,
    label: &TabLabel,
    index: usize,
    active: bool,
    width: f64,
    spinner: Option<f32>,
) -> Child<UiAction> {
    // The tab itself before the cross inside it, so Tab reaches a tab and then
    // the way to close it, which is the order they are read in.
    let id = focus.claim(true);
    let face = if active { theme.raised } else { Theme::CLEAR };
    let ink = if active { theme.ink } else { theme.ink_dim };

    // A loading tab turns where a still one has a dot, so the strip says which
    // of several tabs is the one still working.
    let phase = spinner.filter(|_| label.loading);
    let mark_ink = if label.loading { theme.accent } else { ink };
    let mark = Box::new(Align::centre(Box::new(Painted::new(
        14.0,
        14.0,
        move |rect, _cx, list| match phase {
            Some(phase) => icon::reload(list, rect, Some(phase), mark_ink),
            None => icon::dot(list, rect, mark_ink),
        },
    ))));

    // The title is cut to what the tab can show before it is handed over, with
    // the same engine that will draw it — a title that overflowed would be
    // clipped mid-word with no sign that anything was lost.
    let room = width - 14.0 - 18.0 - theme.gap * 3.0 - theme.inset;
    let title = controls::elide(cx, &label.title, room, theme.font_size, Elide::End);

    let close = controls::icon_button(
        theme,
        focus,
        UiAction::CloseTab(index),
        true,
        "Close tab",
        icon::cross,
    );
    let close = Box::new(Fixed::new(18.0, 18.0, Box::new(Align::centre(close))));

    let row = Stack::row(
        theme.gap,
        vec![
            mark,
            Box::new(crate::widget::Flex::new(
                1.0,
                Box::new(Align::new(
                    0.0,
                    0.5,
                    Box::new(Label::new(title, theme.font_size, ink)),
                )),
            )),
            close,
        ],
    );

    let mut background = Background::rounded(
        face,
        // The two bottom corners are square so the active tab runs into the
        // toolbar beneath it rather than sitting on it.
        (theme.radius_tab, theme.radius_tab, 0.0, 0.0),
        Box::new(Padding::new(
            Insets::symmetric(theme.gap * 1.5, 0.0),
            Box::new(row),
        )),
    );
    if !active {
        background = background.on_hover(theme.hover);
    }

    Box::new(Fixed::width(
        width,
        Box::new(
            Button::new(UiAction::SelectTab(index), Box::new(background))
                .role(Role::Tab)
                .value(if active { "selected" } else { "not selected" })
                .focus(id)
                // A tab is the one control here that moves while it is dragged,
                // so it takes the pointer with the press: by the time the drag
                // is under way it is no longer where the press landed, and
                // "the drag began inside my rectangle" names the wrong tab.
                .capture(CaptureId::new(label.id)),
        ),
    ))
}

/// The row under the tabs: where you have been, and where you are.
fn toolbar(
    theme: &Theme,
    focus: &Focus,
    ui: &BrowserUi,
    history: (bool, bool),
    spinner: Option<f32>,
) -> Child<UiAction> {
    let (can_go_back, can_go_forward) = history;

    let back = controls::icon_button(
        theme,
        focus,
        UiAction::Back,
        can_go_back,
        "Back",
        |list, rect, color| {
            icon::chevron(list, rect, icon::Direction::Left, color);
        },
    );
    let forward = controls::icon_button(
        theme,
        focus,
        UiAction::Forward,
        can_go_forward,
        "Forward",
        |list, rect, color| icon::chevron(list, rect, icon::Direction::Right, color),
    );
    let reload = match spinner {
        Some(_) => controls::icon_button(
            theme,
            focus,
            UiAction::Stop,
            true,
            "Stop loading",
            icon::cross,
        ),
        None => controls::icon_button(
            theme,
            focus,
            UiAction::Reload,
            true,
            "Reload",
            move |list, rect, color| {
                icon::reload(list, rect, None, color);
            },
        ),
    };

    // The scheme decides the mark, and only a transport that was authenticated
    // gets the padlock. Everything else gets a page, which claims nothing.
    let secure = ui.address.text().starts_with("https://");
    let address_id = focus.claim_text(true);
    let focused = ui.focused == Some(address_id);
    let field = TextInput::new(
        FieldView {
            text: ui.address.text().to_owned(),
            caret: focused.then(|| ui.address.caret()),
            selection: focused.then(|| ui.address.selection()).flatten(),
            placeholder: "Search or enter address".to_owned(),
        },
        UiAction::AddressHit,
    )
    .leading(move |list, rect, color| {
        if secure {
            icon::lock(list, rect, color);
        } else {
            icon::page(list, rect, color);
        }
    })
    .face(theme.surface)
    .into_widget(theme);

    // Filled and accented when this page is kept, hollow when it is not, dimmed
    // when there is no page to keep. The colour is captured rather than taken from
    // the button, which hands every icon the same ink: a star that is *on* is the
    // one thing in this toolbar that is not drawn in the foreground colour.
    let kept = ui.bookmark;
    let accent = theme.accent;
    let bookmark = controls::icon_button(
        theme,
        focus,
        UiAction::ToggleBookmark,
        kept != Bookmarked::Impossible,
        if kept == Bookmarked::Yes {
            "Remove bookmark"
        } else {
            "Bookmark this page"
        },
        move |list, rect, color| {
            if kept == Bookmarked::Yes {
                icon::star(list, rect, accent);
            } else {
                icon::star_hollow(list, rect, color);
            }
        },
    );

    Box::new(Padding::new(
        Insets::symmetric(theme.inset, (TOOLBAR_HEIGHT - theme.control_height) / 2.0),
        Box::new(Stack::row(
            theme.gap * 0.5,
            vec![
                back,
                forward,
                reload,
                controls::gap(theme.gap * 0.5),
                field,
                // Immediately right of the address, where every browser puts it, and
                // before the menu so the two things that act on *this page* are not
                // separated by the one that opens other pages.
                bookmark,
                controls::gap(theme.gap * 0.5),
                controls::icon_button(
                    theme,
                    focus,
                    UiAction::ToggleMenu,
                    true,
                    "Browser menu",
                    icon::gear,
                ),
            ],
        )),
    ))
}

/// Size the mark is drawn at on an empty tab, in logical pixels.
const BLANK_MARK_SIZE: f64 = 96.0;

/// Paint a tab that has no document: the empty state, or why the load failed.
///
/// The mark is centred in the content area rather than in the window, so it does
/// not creep upward as the interface grows a toolbar.
pub fn paint_blank_page(
    list: &mut DisplayList,
    theme: &Theme,
    width: f64,
    height: f64,
    error: Option<&str>,
    mark: Option<&ImageData>,
    text: &mut TextEngine,
) {
    fill_rounded(list, Rect::new(0.0, 0.0, width, height), theme.raised, 0.0);

    let mut cx = Cx::new(text);
    let content_top = UI_HEIGHT;
    let content_height = (height - content_top).max(0.0);
    let centre_y = content_top + content_height / 2.0;

    // An error is a message, not a greeting: it replaces the mark rather than
    // sitting under it, because a logo above a failure reads as decoration on bad
    // news.
    if let Some(error) = error {
        centred_text(&mut cx, list, error, width, centre_y, theme.ink);
        return;
    }

    let mut caption_y = centre_y;
    if let Some(mark) = mark {
        let scale = BLANK_MARK_SIZE / f64::from(mark.width);
        let x = (width - BLANK_MARK_SIZE) / 2.0;
        let y = centre_y - BLANK_MARK_SIZE * 0.75;
        list.push(DisplayItem::Image {
            image: mark.clone().into(),
            sampler: ImageSampler::default(),
            transform: Affine::translate((x, y)) * Affine::scale(scale),
            clip_rect: None,
        });
        caption_y = y + BLANK_MARK_SIZE + 20.0;
    }

    centred_text(
        &mut cx,
        list,
        "Type a URL above",
        width,
        caption_y,
        theme.ink_dim,
    );
}

/// One line of interface text, centred horizontally, with `y` as its top.
fn centred_text(
    cx: &mut Cx,
    list: &mut DisplayList,
    content: &str,
    width: f64,
    y: f64,
    color: Color,
) {
    let size = cx.theme.font_size;
    let measured = cx.measure_text(content, size);
    let mut label = Label::new(content, size, color);
    let height = cx.line_height(size);
    let rect = Rect::new(((width - measured) / 2.0).max(0.0), y, width, height);
    Widget::<UiAction>::place(&mut label, rect, cx);
    Widget::<UiAction>::draw(&mut label, cx, list);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(count: usize) -> Vec<TabLabel> {
        (0..count)
            .map(|index| TabLabel {
                id: index as u64 + 1,
                title: format!("Tab {index}"),
                loading: false,
            })
            .collect()
    }

    /// Draw one frame, which is what gives the interface the geometry every
    /// press is then tested against.
    fn frame(ui: &mut BrowserUi, text: &mut TextEngine, width: f64, tabs: usize) {
        let mut list = DisplayList::default();
        list.append(&ui.build_display_list(
            width,
            600.0,
            &labels(tabs),
            0,
            (true, true),
            None,
            text,
        ));
    }

    // --- what a reader is told -------------------------------------------

    /// The strip says one thing per tab, with its title and whether it is the
    /// one being read.
    #[test]
    fn every_tab_is_described_with_its_title_and_whether_it_is_selected() {
        let mut ui = BrowserUi::new();
        let mut text = TextEngine::isolated();
        frame(&mut ui, &mut text, 900.0, 3);

        let tabs: Vec<_> = ui
            .describe()
            .into_iter()
            .filter(|node| node.role == Role::Tab)
            .map(|node| (node.label, node.value))
            .collect();

        assert_eq!(
            tabs,
            vec![
                ("Tab 0".to_owned(), Some("selected".to_owned())),
                ("Tab 1".to_owned(), Some("not selected".to_owned())),
                ("Tab 2".to_owned(), Some("not selected".to_owned())),
            ]
        );
    }

    /// The address field reports what is in it, and that it is a field.
    #[test]
    fn the_address_field_reports_its_contents() {
        let mut ui = BrowserUi::new();
        let mut text = TextEngine::isolated();
        ui.address = TextField::new("example.com/page");
        frame(&mut ui, &mut text, 900.0, 1);

        let field = ui
            .describe()
            .into_iter()
            .find(|node| node.role == Role::TextInput)
            .expect("the address field");
        assert_eq!(field.value.as_deref(), Some("example.com/page"));
    }

    /// Nothing is described before a frame, because nothing has been drawn — and
    /// a description of geometry that does not exist would be rectangles at zero.
    #[test]
    fn nothing_is_described_before_the_first_frame() {
        assert!(BrowserUi::new().describe().is_empty());
    }

    /// Everything described has been placed, so a reader pointing at one is
    /// pointing at where it actually is.
    #[test]
    fn everything_described_has_a_rectangle_on_screen() {
        let mut ui = BrowserUi::new();
        let mut text = TextEngine::isolated();
        frame(&mut ui, &mut text, 900.0, 2);

        for node in ui.describe() {
            assert!(
                node.rect.width > 0.0 && node.rect.height > 0.0,
                "{:?} was described at {:?}",
                node.role,
                node.rect
            );
        }
    }

    /// A press through the accessibility path reports what a click reports.
    #[test]
    fn a_reader_pressing_a_tab_selects_it_like_a_click_would() {
        let mut ui = BrowserUi::new();
        let mut text = TextEngine::isolated();
        frame(&mut ui, &mut text, 900.0, 3);

        let index = ui
            .describe()
            .iter()
            .position(|node| node.role == Role::Tab && node.label == "Tab 2")
            .expect("the third tab");

        assert_eq!(
            ui.activate_described(index, &mut text),
            UiAction::SelectTab(2)
        );
    }

    /// A button that is drawn but cannot act says so, rather than being missing:
    /// what a browser will do and what it does are different facts.
    #[test]
    fn a_disabled_button_is_described_and_marked_disabled() {
        let mut ui = BrowserUi::new();
        let mut text = TextEngine::isolated();
        let mut list = DisplayList::default();
        // Neither back nor forward has anywhere to go.
        list.append(&ui.build_display_list(
            900.0,
            600.0,
            &labels(1),
            0,
            (false, false),
            None,
            &mut text,
        ));

        let described = ui.describe();
        assert!(
            described.iter().any(|node| !node.enabled),
            "a browser with no history describes no disabled control"
        );
    }

    /// The platform's own accelerator modifier, whichever platform this is.
    const ACCELERATOR: Modifiers = Modifiers {
        command: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        shift: false,
        alt: false,
    };

    /// The rectangle the widget tree placed something at, found by pressing.
    fn press(ui: &mut BrowserUi, text: &mut TextEngine, x: f64, y: f64) -> UiAction {
        ui.pointer_moved(x, y, text);
        ui.pointer_pressed(text, 1)
    }

    /// Draw one frame at a given window size, and say what it drew.
    fn frame_at(ui: &mut BrowserUi, text: &mut TextEngine, width: f64, height: f64) -> DisplayList {
        frame_with_labels(ui, text, width, height, &labels(2), 0)
    }

    fn frame_with_labels(
        ui: &mut BrowserUi,
        text: &mut TextEngine,
        width: f64,
        height: f64,
        tabs: &[TabLabel],
        active: usize,
    ) -> DisplayList {
        let mut list = DisplayList::new();
        list.append(&ui.build_display_list(width, height, tabs, active, (true, true), None, text));
        list
    }

    #[test]
    fn closing_a_tab_preserves_the_runtime_identity_of_the_others() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let mut tabs = labels(3);
        frame_with_labels(&mut ui, &mut text, 1000.0, 800.0, &tabs, 0);
        let before: Vec<_> = ui.tab_runtime_work.iter().map(|(id, _)| *id).collect();

        tabs.remove(0);
        frame_with_labels(&mut ui, &mut text, 1000.0, 800.0, &tabs, 0);
        let after: Vec<_> = ui.tab_runtime_work.iter().map(|(id, _)| *id).collect();

        assert_eq!(after, before[1..]);
    }

    #[test]
    fn a_title_change_invalidates_only_the_tab_that_owns_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let mut tabs = labels(3);
        frame_with_labels(&mut ui, &mut text, 1000.0, 800.0, &tabs, 0);

        tabs[1].title = "Renamed".to_owned();
        frame_with_labels(&mut ui, &mut text, 1000.0, 800.0, &tabs, 0);
        let work: Vec<_> = ui
            .tab_runtime_work
            .iter()
            .map(|(_, dirty)| *dirty)
            .collect();

        assert!(work[0].is_empty());
        assert!(work[1].contains(UiDirty::PAINT));
        assert!(work[1].contains(UiDirty::SEMANTICS));
        assert!(!work[1].contains(UiDirty::LAYOUT));
        assert!(work[2].is_empty());
    }

    #[test]
    fn a_taller_window_does_not_rebuild_the_interface() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        let first = frame_at(&mut ui, &mut text, 1000.0, 800.0);
        assert_eq!(ui.builds(), 1);

        // Dragging the bottom edge changes what the *page* has to lay out in.
        // The interface is a fixed band at the top of the window: nothing about
        // it moved, so nothing about it is measured, shaped or built again.
        let taller = frame_at(&mut ui, &mut text, 1000.0, 400.0);
        assert_eq!(ui.builds(), 1, "a height-only resize rebuilds nothing");
        assert_eq!(taller, first, "and draws exactly what the last frame drew");
    }

    #[test]
    fn a_pointer_over_the_page_does_not_rebuild_the_interface() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let first = frame_at(&mut ui, &mut text, 1000.0, 800.0);
        assert_eq!(ui.builds(), 1);

        // The pointer moves around the document, well below the toolbar. Nothing
        // in the toolbar is hovered wherever it goes, so the toolbar is not
        // rebuilt — which is what keeps scrolling with the mouse moving from
        // reshaping every tab title on every frame.
        for y in [200.0, 400.0, 600.0, 799.0] {
            ui.pointer_moved(500.0, y, &mut text);
            let again = frame_at(&mut ui, &mut text, 1000.0, 800.0);
            assert_eq!(ui.builds(), 1, "a pointer at y={y} rebuilt the toolbar");
            assert_eq!(again, first, "and it drew something different");
        }

        // Back up onto a toolbar control, and it does rebuild: now the hover is
        // its own.
        ui.pointer_moved(500.0, TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0, &mut text);
        let _ = frame_at(&mut ui, &mut text, 1000.0, 800.0);
        assert_eq!(
            ui.builds(),
            2,
            "a pointer over the toolbar is its own hover"
        );
    }

    #[test]
    fn a_narrower_window_does_rebuild_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        let tab_builds = ui.tab_builds();
        let toolbar_builds = ui.toolbar_builds();
        // Width is the one thing the interface is laid out against: the tabs
        // share it and the address field takes what is left.
        frame_at(&mut ui, &mut text, 700.0, 800.0);
        assert_eq!(ui.builds(), 2);
        assert_eq!(ui.tab_builds(), tab_builds + 1);
        assert_eq!(ui.toolbar_builds(), toolbar_builds + 1);
    }

    #[test]
    fn typing_in_the_omnibox_reuses_the_retained_tab_strip() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        ui.focus_address();
        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        ui.describe();
        let field = field_rect(&ui).inflate(4.0);
        let tab_builds = ui.tab_builds();
        let toolbar_builds = ui.toolbar_builds();
        let tab_semantics = ui.tab_semantics_builds();
        let toolbar_semantics = ui.toolbar_semantics_builds();

        assert!(ui.text_input('o'));
        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        ui.describe();

        assert_eq!(
            ui.tab_builds(),
            tab_builds,
            "an address edit reshaped or repainted the tab strip"
        );
        assert_eq!(
            ui.toolbar_builds(),
            toolbar_builds + 1,
            "the changed field itself was not rebuilt"
        );
        assert_eq!(ui.tab_semantics_builds(), tab_semantics);
        assert_eq!(ui.toolbar_semantics_builds(), toolbar_semantics + 1);
        assert_eq!(
            ui.dirty(),
            Some(field),
            "an address edit dirtied more than the field"
        );
    }

    #[test]
    fn toolbar_hover_reuses_the_retained_tab_strip() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        ui.describe();
        let tab_builds = ui.tab_builds();
        let toolbar_builds = ui.toolbar_builds();
        let tab_semantics = ui.tab_semantics_builds();
        let toolbar_semantics = ui.toolbar_semantics_builds();
        ui.pointer_moved(60.0, TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0, &mut text);
        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        ui.describe();

        assert_eq!(ui.tab_builds(), tab_builds);
        assert_eq!(ui.toolbar_builds(), toolbar_builds + 1);
        assert_eq!(ui.tab_semantics_builds(), tab_semantics);
        assert_eq!(
            ui.toolbar_semantics_builds(),
            toolbar_semantics,
            "paint-only hover rebuilt toolbar semantics"
        );
    }

    #[test]
    fn an_open_menu_makes_the_height_matter_again() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        ui.open_menu();

        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        // The panel hangs below the band and its sheet covers the window, so
        // this is the one case where the window's height is the interface's
        // business.
        frame_at(&mut ui, &mut text, 1000.0, 400.0);
        assert_eq!(ui.builds(), 2);
    }

    #[test]
    fn moving_the_pointer_rebuilds_it_because_hover_is_drawn() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        ui.pointer_moved(60.0, UI_HEIGHT - 20.0, &mut text);
        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        assert_eq!(
            ui.builds(),
            2,
            "the wash under the pointer is part of the frame"
        );
    }

    #[test]
    fn the_toolbar_buttons_sit_in_the_order_they_are_drawn() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 2);

        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;
        assert_eq!(press(&mut ui, &mut text, 20.0, middle), UiAction::Back);
        assert_eq!(press(&mut ui, &mut text, 50.0, middle), UiAction::Forward);
        assert_eq!(press(&mut ui, &mut text, 80.0, middle), UiAction::Reload);
    }

    #[test]
    fn reload_turns_into_stop_while_the_tab_is_loading() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let mut tabs = labels(1);
        tabs[0].loading = true;
        ui.build_display_list(
            1000.0,
            600.0,
            &tabs,
            0,
            (false, false),
            Some(0.25),
            &mut text,
        );

        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;
        assert_eq!(press(&mut ui, &mut text, 80.0, middle), UiAction::Stop);
    }

    #[test]
    fn a_press_selects_the_tab_it_is_drawn_over() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 3);

        // Well inside the second tab, and away from its cross.
        let x = 6.0 + TAB_MAX_WIDTH + TAB_GAP + 30.0;
        assert_eq!(
            press(&mut ui, &mut text, x, TAB_STRIP_HEIGHT / 2.0),
            UiAction::SelectTab(1)
        );
    }

    #[test]
    fn the_cross_inside_a_tab_wins_over_the_tab_it_sits_in() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 2);

        // The cross is at the tab's right end, inside its padding.
        let x = 6.0 + TAB_MAX_WIDTH - 16.0;
        assert_eq!(
            press(&mut ui, &mut text, x, TAB_STRIP_HEIGHT / 2.0),
            UiAction::CloseTab(0)
        );
    }

    #[test]
    fn the_button_that_opens_a_tab_sits_after_the_last_of_them() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 2);

        let x = 6.0 + (TAB_MAX_WIDTH + TAB_GAP) * 2.0 + NEW_TAB_SIZE / 2.0;
        assert_eq!(
            press(&mut ui, &mut text, x, TAB_STRIP_HEIGHT / 2.0),
            UiAction::NewTab
        );
    }

    #[test]
    fn pressing_the_address_field_focuses_it_and_pressing_the_page_does_not() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);

        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;
        assert_eq!(press(&mut ui, &mut text, 500.0, middle), UiAction::None);
        assert!(ui.address_focused(), "a press in the field focuses it");

        assert_eq!(
            press(&mut ui, &mut text, 400.0, UI_HEIGHT + 100.0),
            UiAction::None
        );
        assert!(!ui.address_focused(), "clicking the page takes focus away");
    }

    #[test]
    fn asking_what_a_press_would_do_does_not_do_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);

        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;
        assert_eq!(
            ui.action_at(80.0, middle, &mut text),
            Some(UiAction::Reload),
            "the probe answers what the press helper presses"
        );
        assert_eq!(ui.pointer, (-1.0, -1.0), "and the pointer has not moved");
        assert!(!ui.pointer_down, "and no press happened");
    }

    #[test]
    fn a_press_on_empty_strip_focuses_nothing_and_does_nothing() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);
        ui.focus_address();

        assert_eq!(
            press(&mut ui, &mut text, 900.0, TAB_STRIP_HEIGHT / 2.0),
            UiAction::None
        );
        assert!(!ui.address_focused());
    }

    #[test]
    fn traversal_skips_a_control_that_is_drawn_but_does_nothing() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        // Nowhere to go back or forward to, so both of those are drawn dimmed
        // and neither answers a press.
        let mut list = DisplayList::default();
        list.append(&ui.build_display_list(
            1000.0,
            600.0,
            &labels(1),
            0,
            (false, false),
            None,
            &mut text,
        ));

        // The tab, its cross, the button that opens another, and then — past
        // both dimmed arrows without stopping on either — reload.
        for _ in 0..4 {
            ui.key_pressed(
                Key::Tab,
                Modifiers::default(),
                &mut text,
                &mut crate::clipboard::InMemory::default(),
            );
        }
        assert_eq!(
            ui.key_pressed(
                Key::Enter,
                Modifiers::default(),
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::Reload,
            "a control that cannot be pressed is not a place the keyboard stops"
        );
    }

    /// Whether the frame drew a panel naming something.
    fn named_by_a_tooltip(ui: &BrowserUi) -> Option<String> {
        ui.tooltip().map(|(label, _)| label.to_owned())
    }

    /// A pointer resting on a control is a reader asking what it is, and after
    /// a pause they are told — in the control's own name, which is the same
    /// string a screen reader is given.
    #[test]
    fn resting_on_a_control_names_it_after_a_pause() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);

        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;
        ui.pointer_moved(20.0, middle, &mut text);
        assert_eq!(named_by_a_tooltip(&ui), None, "named without a pause");
        assert!(
            ui.next_tooltip_frame().is_some(),
            "a pause nobody is woken for is a pause that never ends"
        );

        ui.wind_rest_back(TOOLTIP_DELAY);
        assert_eq!(named_by_a_tooltip(&ui).as_deref(), Some("Back"));
        assert_eq!(
            ui.next_tooltip_frame(),
            None,
            "a panel already drawn does not need waking again"
        );

        // The frame draws it, and the one after it does not rebuild for nothing.
        let builds = ui.builds();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        assert_eq!(ui.builds(), builds + 1);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        assert_eq!(
            ui.builds(),
            builds + 1,
            "a still tooltip rebuilt the chrome"
        );
    }

    /// Moving on takes it away, and a press never gets one at all.
    #[test]
    fn a_tooltip_goes_when_the_pointer_or_the_reader_moves_on() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;

        ui.pointer_moved(20.0, middle, &mut text);
        ui.wind_rest_back(TOOLTIP_DELAY);
        assert!(named_by_a_tooltip(&ui).is_some());

        // Off the control and onto plain toolbar.
        ui.pointer_moved(20.0, 700.0 - 10.0, &mut text);
        assert_eq!(named_by_a_tooltip(&ui), None);
        assert_eq!(ui.next_tooltip_frame(), None, "nothing is being rested on");

        // And a press on one is a reader who has stopped asking.
        ui.pointer_moved(20.0, middle, &mut text);
        ui.wind_rest_back(TOOLTIP_DELAY);
        assert!(named_by_a_tooltip(&ui).is_some());
        ui.pointer_pressed(&mut text, 1);
        assert_eq!(named_by_a_tooltip(&ui), None);
    }

    /// Crossing one wide control keeps its clock running rather than starting
    /// over at every pixel, or a slow hand would never be told anything.
    #[test]
    fn moving_within_one_control_does_not_restart_the_pause() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        let middle = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0;

        ui.pointer_moved(18.0, middle, &mut text);
        ui.wind_rest_back(TOOLTIP_DELAY);
        ui.pointer_moved(22.0, middle + 2.0, &mut text);
        assert_eq!(named_by_a_tooltip(&ui).as_deref(), Some("Back"));
    }

    /// Where a tab landed in the last frame, by its id.
    fn tab_rect(ui: &BrowserUi, id: u64) -> Rect {
        ui.tab_places
            .borrow()
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, rect)| *rect)
            .expect("the tab was drawn")
    }

    /// A press on a tab and a drag past its neighbour reorders the strip, and
    /// says so by naming the tab rather than a position — the position is what
    /// the move changes.
    #[test]
    fn dragging_a_tab_past_its_neighbour_asks_for_the_move() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let tabs = labels(3);
        frame_with_labels(&mut ui, &mut text, 1000.0, 700.0, &tabs, 0);

        let first = tab_rect(&ui, tabs[0].id);
        let second = tab_rect(&ui, tabs[1].id);
        let start = first.x + first.width / 2.0;
        ui.pointer_moved(start, first.y + first.height / 2.0, &mut text);
        assert_eq!(
            ui.pointer_pressed(&mut text, 1),
            UiAction::SelectTab(0),
            "a press on a tab is still a press on a tab"
        );

        // A twitch is not a drag: nothing moves until the pointer has gone far
        // enough that the reader plainly meant it to.
        assert_eq!(
            ui.pointer_moved(start + 2.0, first.y + 5.0, &mut text),
            UiAction::None
        );
        assert!(!ui.dragging_tab());

        let over = second.x + second.width / 2.0;
        assert_eq!(
            ui.pointer_moved(over, first.y + 5.0, &mut text),
            UiAction::MoveTab {
                id: tabs[0].id,
                to: 1
            }
        );
        assert!(ui.dragging_tab());
    }

    /// Escape during a drag puts the tab back where it was picked up.
    #[test]
    fn escape_during_a_drag_puts_the_tab_back() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let tabs = labels(3);
        // The strip as it stands after the first tab has been dragged to the
        // middle: the drag started at position 0 and the tab is now second.
        frame_with_labels(&mut ui, &mut text, 1000.0, 700.0, &tabs, 0);
        let first = tab_rect(&ui, tabs[0].id);
        let second = tab_rect(&ui, tabs[1].id);
        ui.pointer_moved(
            first.x + first.width / 2.0,
            first.y + first.height / 2.0,
            &mut text,
        );
        ui.pointer_pressed(&mut text, 1);
        ui.pointer_moved(second.x + second.width / 2.0, first.y + 5.0, &mut text);

        assert_eq!(
            ui.key_pressed(
                Key::Escape,
                Modifiers::default(),
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::MoveTab {
                id: tabs[0].id,
                to: 0
            },
            "a drag a reader took back has to be taken back"
        );
        assert!(!ui.dragging_tab(), "the drag is over either way");
    }

    /// A press that never travels is a click: letting go asks for nothing.
    #[test]
    fn a_press_on_a_tab_that_does_not_travel_moves_nothing() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        let tabs = labels(3);
        frame_with_labels(&mut ui, &mut text, 1000.0, 700.0, &tabs, 0);
        let first = tab_rect(&ui, tabs[0].id);
        ui.pointer_moved(
            first.x + first.width / 2.0,
            first.y + first.height / 2.0,
            &mut text,
        );
        ui.pointer_pressed(&mut text, 1);
        ui.pointer_released();

        assert!(!ui.dragging_tab());
        assert_eq!(
            ui.key_pressed(
                Key::Escape,
                Modifiers::default(),
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::None,
            "Escape after a plain click has no drag to take back"
        );
    }

    /// The cogwheel, which is the last control on the toolbar.
    fn open_the_menu(ui: &mut BrowserUi, text: &mut TextEngine) {
        frame_at(ui, text, 1000.0, 700.0);
        press(ui, text, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        assert!(ui.menu_open(), "the cogwheel opens the menu");
        frame_at(ui, text, 1000.0, 700.0);
    }

    fn tab(ui: &mut BrowserUi, text: &mut TextEngine, shift: bool) {
        ui.key_pressed(
            Key::Tab,
            Modifiers {
                shift,
                ..Modifiers::default()
            },
            text,
            &mut crate::clipboard::InMemory::default(),
        );
    }

    /// An open menu is a place the keyboard cannot walk out of. Without this,
    /// Tab walked from its last row onto the toolbar behind the sheet — to
    /// controls a press cannot reach and an eye cannot see the ring on.
    #[test]
    fn tab_inside_an_open_menu_never_reaches_the_toolbar_behind_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        open_the_menu(&mut ui, &mut text);

        let mut reached = Vec::new();
        // More steps than the menu has rows, so a traversal that leaks would
        // land outside and be caught rather than merely be lucky.
        for _ in 0..16 {
            tab(&mut ui, &mut text, false);
            reached.push(ui.focused().expect("traversal reached something"));
        }
        for id in &reached {
            assert_eq!(
                ui.focus.scope(Some(*id)),
                Some(MENU_SCOPE),
                "Tab left the open menu"
            );
        }
        let rows: std::collections::BTreeSet<_> = reached.iter().copied().collect();
        assert!(rows.len() > 1, "the menu has more than one row to reach");
        assert_eq!(
            reached.first(),
            reached.get(rows.len()),
            "traversal inside the menu wraps"
        );

        // And backwards from the first row is the last one, not the cogwheel.
        ui.focused = reached.first().copied();
        tab(&mut ui, &mut text, true);
        assert_eq!(ui.focus.scope(ui.focused()), Some(MENU_SCOPE));
    }

    /// Escape leaves the menu and puts the keyboard back where it was, which is
    /// what makes looking into a menu something a keyboard can undo.
    #[test]
    fn escape_closes_the_menu_and_returns_the_keyboard() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        tab(&mut ui, &mut text, false);
        let before = ui.focused().expect("Tab reached the first control");

        press(&mut ui, &mut text, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        assert!(ui.menu_open());
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        ui.key_pressed(
            Key::Down,
            Modifiers::default(),
            &mut text,
            &mut crate::clipboard::InMemory::default(),
        );
        assert_eq!(
            ui.focus.scope(ui.focused()),
            Some(MENU_SCOPE),
            "Down is the way into an open menu"
        );

        ui.key_pressed(
            Key::Escape,
            Modifiers::default(),
            &mut text,
            &mut crate::clipboard::InMemory::default(),
        );
        assert!(!ui.menu_open(), "Escape dismisses the menu");
        assert_eq!(ui.focused(), Some(before), "the keyboard came back");
    }

    /// A press that dismisses the menu leaves no ring behind: the reader is
    /// looking at where they clicked, not at the control they walked away from.
    #[test]
    fn a_press_outside_the_menu_dismisses_it_without_a_ring() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        tab(&mut ui, &mut text, false);
        assert!(ui.focused().is_some());

        press(&mut ui, &mut text, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        // Well below the panel, on the sheet that covers the window.
        press(&mut ui, &mut text, 100.0, 600.0);

        assert!(!ui.menu_open());
        assert_eq!(ui.focused(), None);
    }

    /// An accelerator that names a control takes the keyboard out of the menu,
    /// and a menu the keyboard has left is a menu that is closed.
    #[test]
    fn naming_the_address_bar_closes_an_open_menu() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        open_the_menu(&mut ui, &mut text);

        ui.key_pressed(
            Key::Character('l'),
            Modifiers {
                command: cfg!(target_os = "macos"),
                control: !cfg!(target_os = "macos"),
                ..Modifiers::default()
            },
            &mut text,
            &mut crate::clipboard::InMemory::default(),
        );
        assert!(!ui.menu_open(), "the menu stayed open behind the caret");
        assert!(ui.address_focused());
    }

    #[test]
    fn activating_by_key_reports_what_a_press_reports() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);

        // The first thing Tab reaches is the first tab, which is what a press
        // on it reports too — one path, so the two cannot drift apart.
        ui.key_pressed(
            Key::Tab,
            Modifiers::default(),
            &mut text,
            &mut crate::clipboard::InMemory::default(),
        );
        assert_eq!(
            ui.key_pressed(
                Key::Character(' '),
                Modifiers::default(),
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::SelectTab(0)
        );
        assert_eq!(
            press(&mut ui, &mut text, 40.0, TAB_STRIP_HEIGHT / 2.0),
            UiAction::SelectTab(0)
        );
    }

    #[test]
    fn a_press_before_the_first_frame_reports_nothing_rather_than_guessing() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        assert_eq!(press(&mut ui, &mut text, 20.0, 20.0), UiAction::None);
    }

    #[test]
    fn typing_goes_to_the_address_bar_only_when_it_has_focus() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);

        assert!(!ui.text_input('a'));
        assert_eq!(ui.address.text(), "");

        ui.focus_address();
        assert!(ui.text_input('a'));
        assert!(ui.text_input('b'));
        assert_eq!(ui.address.text(), "ab");
    }

    #[test]
    fn enter_navigates_to_what_was_typed_and_drops_focus() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);
        ui.focus_address();
        for character in "example.com".chars() {
            ui.text_input(character);
        }

        let action = ui.key_pressed(
            Key::Enter,
            Modifiers::default(),
            &mut text,
            &mut crate::clipboard::InMemory::default(),
        );
        assert_eq!(action, UiAction::Navigate("example.com".to_owned()));
        assert!(!ui.address_focused());
    }

    #[test]
    fn an_empty_address_navigates_nowhere() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);
        ui.focus_address();
        assert_eq!(
            ui.key_pressed(
                Key::Enter,
                Modifiers::default(),
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::None
        );
    }

    #[test]
    fn editing_keys_move_and_delete_by_character_not_by_byte() {
        // Every one of these steps lands mid-byte-sequence if the field counts
        // bytes: each of these characters is two bytes.
        let mut field = TextField::new("привет");
        field.move_left(false);
        field.backspace();
        assert_eq!(field.text(), "привт", "backspace deletes before the caret");

        field.move_home(false);
        field.delete();
        assert_eq!(field.text(), "ривт", "delete removes after it");

        field.move_end(false);
        field.insert('о');
        assert_eq!(field.text(), "ривто", "the caret survives at the end");
    }

    #[test]
    fn the_caret_never_lands_inside_a_character() {
        let mut field = TextField::new("日本語");
        for _ in 0..5 {
            field.move_left(false);
        }
        assert_eq!(field.caret(), 0);
        for _ in 0..5 {
            field.move_right(false);
        }
        assert_eq!(field.caret(), field.text().len());
    }

    #[test]
    fn selection_offsets_never_land_inside_a_character() {
        // The same non-ASCII strings the caret tests use: every character here
        // is more than one byte, so a selection counted in bytes tears one.
        for text in ["привет", "日本語", "héllo"] {
            let mut field = TextField::new(text);
            let shift = Modifiers {
                shift: true,
                ..Modifiers::default()
            };
            for _ in 0..text.chars().count() + 2 {
                field.move_left(shift.shift);
                let range = field.selection().expect("extending selects");
                assert!(field.text().is_char_boundary(range.start));
                assert!(field.text().is_char_boundary(range.end));
                assert_eq!(field.selected_text(), Some(&field.text()[range]));
            }
        }
    }

    #[test]
    fn a_point_off_a_boundary_snaps_to_one() {
        let mut field = TextField::new("привет");
        // Byte 1 is inside the first two-byte character.
        field.point(1, 1);
        assert_eq!(field.caret(), 0);
        field.drag_to(3);
        assert_eq!(field.selection(), Some(0..2), "a drag snaps too");
    }

    #[test]
    fn copy_puts_exactly_the_selected_bytes_on_the_clipboard() {
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut field = TextField::new("https://example.com/путь");
        field.select_all();
        field.copy(&mut clipboard);
        assert_eq!(
            clipboard.read().as_deref(),
            Some("https://example.com/путь")
        );

        // A copy with nothing selected keeps its hands off what was there.
        let field = TextField::new("something else");
        field.copy(&mut clipboard);
        assert_eq!(
            clipboard.read().as_deref(),
            Some("https://example.com/путь")
        );
    }

    #[test]
    fn a_paste_over_a_selection_replaces_it() {
        let mut clipboard = crate::clipboard::InMemory::default();
        clipboard.write("отлира".to_owned());

        let mut field = TextField::new("example.com/old");
        // Select "old": the last three characters.
        field.move_end(false);
        for _ in 0..3 {
            field.move_left(true);
        }
        field.paste(&mut clipboard);
        assert_eq!(field.text(), "example.com/отлира");
        assert_eq!(field.selection(), None, "the pasted text is not selected");
        assert_eq!(field.caret(), field.text().len());
    }

    #[test]
    fn a_paste_drops_control_characters() {
        let mut clipboard = crate::clipboard::InMemory::default();
        clipboard.write("two\nlines\tand a tab\r".to_owned());
        let mut field = TextField::new("");
        field.paste(&mut clipboard);
        assert_eq!(field.text(), "twolinesand a tab");
    }

    #[test]
    fn cut_copies_and_removes_in_one_motion() {
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut field = TextField::new("front-back");
        field.move_home(false);
        for _ in 0..5 {
            field.move_right(true);
        }
        field.cut(&mut clipboard);
        assert_eq!(clipboard.read().as_deref(), Some("front"));
        assert_eq!(field.text(), "-back");
        assert_eq!(field.caret(), 0);
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut field = TextField::new("привет");
        field.select_all();
        field.insert('a');
        assert_eq!(field.text(), "a");
        assert_eq!(field.caret(), 1);

        let mut field = TextField::new("привет");
        field.select_all();
        field.backspace();
        assert_eq!(
            field.text(),
            "",
            "backspace eats the selection, not a character"
        );
    }

    #[test]
    fn two_clicks_take_the_word_and_three_take_the_lot() {
        let mut field = TextField::new("https://example.com/path");
        // In the middle of "example".
        field.point(10, 2);
        assert_eq!(field.selected_text(), Some("example"));
        // On the punctuation, the punctuation is the word.
        field.point(6, 2);
        assert_eq!(field.selected_text(), Some("://"));
        field.point(10, 3);
        assert_eq!(field.selected_text(), Some("https://example.com/path"));
        // At the very end, the last word rather than nothing.
        field.point(field.text().len(), 2);
        assert_eq!(field.selected_text(), Some("path"));
    }

    #[test]
    fn arrows_collapse_a_selection_to_its_ends() {
        let mut field = TextField::new("абвгд");
        field.select_all();
        field.move_left(false);
        assert_eq!(field.selection(), None);
        assert_eq!(field.caret(), 0, "left lands at the selection's start");

        field.select_all();
        field.move_right(false);
        assert_eq!(field.caret(), field.text().len(), "right at its end");
    }

    #[test]
    fn focusing_the_address_by_accelerator_selects_the_lot() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        ui.address.set_text("https://example.com/");
        frame(&mut ui, &mut text, 1000.0, 1);
        ui.focus_address();
        assert_eq!(
            ui.address.selection(),
            Some(0..ui.address.text().len()),
            "⌘L means: the next keystroke replaces the address"
        );
    }

    /// Where the address field was drawn, according to the frame that drew it.
    fn field_rect(ui: &BrowserUi) -> Rect {
        ui.describe()
            .into_iter()
            .find(|node| node.role == Role::TextInput)
            .expect("the toolbar has an address field")
            .rect
    }

    #[test]
    fn a_press_in_the_field_puts_the_caret_where_it_landed() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        ui.address.set_text("example.com");
        frame(&mut ui, &mut text, 1000.0, 1);

        let rect = field_rect(&ui);
        let middle = rect.y + rect.height / 2.0;

        // Near the left edge of the text, before any glyph's midpoint.
        assert_eq!(
            press(&mut ui, &mut text, rect.x + 2.0, middle),
            UiAction::None
        );
        assert!(
            ui.address_focused(),
            "a press in the field takes the keyboard"
        );
        assert_eq!(ui.address.caret(), 0);
        ui.pointer_released();

        // Well past the last glyph: the caret lands at the end.
        assert_eq!(
            press(&mut ui, &mut text, rect.x + rect.width - 4.0, middle),
            UiAction::None
        );
        assert_eq!(ui.address.caret(), ui.address.text().len());
        ui.pointer_released();
    }

    #[test]
    fn a_drag_across_the_field_selects_what_it_crossed() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        ui.address.set_text("example.com");
        frame(&mut ui, &mut text, 1000.0, 1);

        let rect = field_rect(&ui);
        let middle = rect.y + rect.height / 2.0;

        press(&mut ui, &mut text, rect.x + 2.0, middle);
        // The pointer travels past the field's right edge, and the selection
        // follows: the drag began in the field, so the field keeps it.
        ui.pointer_moved(rect.x + rect.width + 40.0, middle, &mut text);
        assert_eq!(
            ui.address.selection(),
            Some(0..ui.address.text().len()),
            "dragging from the front past the end selects everything"
        );
        ui.pointer_released();

        // The next frame draws the selection: it is part of the appearance.
        let before = ui.builds();
        frame(&mut ui, &mut text, 1000.0, 1);
        assert_eq!(ui.builds(), before + 1, "a new selection is a new frame");
    }

    #[test]
    fn a_double_click_in_the_field_selects_the_word_under_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        ui.address.set_text("example.com");
        frame(&mut ui, &mut text, 1000.0, 1);

        let rect = field_rect(&ui);
        let middle = rect.y + rect.height / 2.0;
        ui.pointer_moved(rect.x + 30.0, middle, &mut text);
        ui.pointer_pressed(&mut text, 2);
        assert_eq!(
            ui.address.selected_text(),
            Some("example"),
            "two clicks a few glyphs in select the first word"
        );
        ui.pointer_released();
    }

    #[test]
    fn the_editing_accelerators_stay_the_fields_and_the_rest_the_browsers() {
        let mut text = TextEngine::new();
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut ui = BrowserUi::new();
        ui.address.set_text("copied");
        frame(&mut ui, &mut text, 1000.0, 1);
        ui.focus_address();

        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        assert_eq!(
            ui.key_pressed(Key::Character('c'), accelerator, &mut text, &mut clipboard),
            UiAction::None
        );
        assert_eq!(
            clipboard.read().as_deref(),
            Some("copied"),
            "⌘C in the focused field copies its selection"
        );
        assert_eq!(
            ui.key_pressed(Key::Character('r'), accelerator, &mut text, &mut clipboard),
            UiAction::Reload,
            "⌘R stays the browser's even while the field has the keyboard"
        );
    }

    #[test]
    fn the_accelerator_opens_a_tab_whatever_has_focus() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 1);
        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        assert_eq!(
            ui.key_pressed(
                Key::Character('t'),
                accelerator,
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::NewTab
        );
        assert_eq!(
            ui.key_pressed(
                Key::Character('l'),
                accelerator,
                &mut text,
                &mut crate::clipboard::InMemory::default()
            ),
            UiAction::None
        );
        assert!(ui.address_focused(), "cmd-L focuses the address bar");
    }

    /// Tabs shrink to share the width, down to a floor. Past the floor they run
    /// off the edge, which is a stated gap: a scrolling or collapsing tab strip
    /// is what W9 closed.
    #[test]
    fn many_tabs_shrink_to_share_the_strip_and_stop_at_a_floor() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 20);

        // Twenty tabs across a 1000px strip would be 47px each if they kept
        // dividing, and a 47px tab holds no title. They stop at the floor and
        // the strip slides instead — which is the difference between a tab that
        // is off screen and a tab that is lost.
        let strip = on_the_strip(&mut ui, &mut text, 1000.0);
        assert!(
            strip.len() < 20,
            "twenty tabs do not fit a 1000px strip: {strip:?}"
        );
        assert!(
            ui.tab_overflow.get() > 0.0,
            "so the strip has somewhere to slide to"
        );
        // The floor holds: two neighbouring tabs are a floor apart, not the 47px
        // they would be if they had kept dividing.
        let width = TAB_MIN_WIDTH + TAB_GAP;
        assert!(width > 47.0);
    }

    /// The whole of W9: a tab you have opened is a tab you can reach.
    #[test]
    fn every_tab_can_be_reached_by_scrolling() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 20);

        // Every one of them, one at a time: the tab being read is the tab that
        // has to be on the strip, and a browser reaches a tab by selecting it.
        // Twice per step, because how far the strip can slide and where the tab
        // landed are both things only a drawn frame reports.
        let missing: Vec<usize> = (0..20)
            .filter(|active| {
                frame_active(&mut ui, &mut text, 1000.0, 20, *active);
                frame_active(&mut ui, &mut text, 1000.0, 20, *active);
                !on_the_strip(&mut ui, &mut text, 1000.0).contains(active)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "these tabs cannot be reached: {missing:?}"
        );

        // And by hand from either end, without a selection moving anything: the
        // wheel and the chevrons reach the first and the last.
        ui.scroll_tabs_by(-10_000.0);
        frame_active(&mut ui, &mut text, 1000.0, 20, 0);
        assert!(on_the_strip(&mut ui, &mut text, 1000.0).contains(&0));
        ui.scroll_tabs_by(10_000.0);
        frame_active(&mut ui, &mut text, 1000.0, 20, 19);
        assert!(on_the_strip(&mut ui, &mut text, 1000.0).contains(&19));
    }

    /// And the one you are reading is on screen without being looked for.
    #[test]
    fn the_active_tab_is_brought_into_view() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        for active in [19, 0, 12] {
            // Twice: how far the strip can slide, and where the active tab
            // landed, are both things only a drawn frame reports — so the frame
            // that slides is the one after the frame that placed it.
            for _ in 0..2 {
                frame_active(&mut ui, &mut text, 1000.0, 20, active);
            }
            assert!(
                on_the_strip(&mut ui, &mut text, 1000.0).contains(&active),
                "tab {active} is the one being read and is not on the strip"
            );
        }
    }

    /// A strip that fits does not slide, and does not keep an offset from when
    /// it did — a strip scrolled past a strip that now fits shows empty space
    /// where its first tabs are.
    #[test]
    fn closing_tabs_until_they_fit_puts_the_strip_back() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 20);
        ui.scroll_tabs_by(10_000.0);
        assert!(ui.tab_scroll() > 0.0);

        frame(&mut ui, &mut text, 1000.0, 2);
        frame(&mut ui, &mut text, 1000.0, 2);
        assert_eq!(ui.tab_scroll(), 0.0);
        assert_eq!(on_the_strip(&mut ui, &mut text, 1000.0), vec![0, 1]);
    }

    /// The chevrons slide the strip and never reach the browser: where a strip
    /// is scrolled to is the interface's own, like the menu being open.
    #[test]
    fn a_chevron_slides_the_strip_and_the_browser_never_hears_of_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 20);

        // The right-hand chevron sits just before the button that opens a tab.
        let x = 1000.0 - 6.0 - NEW_TAB_SIZE - CHEVRON_SIZE / 2.0 - 6.0;
        let was = ui.tab_scroll();
        assert_eq!(
            press(&mut ui, &mut text, x, TAB_STRIP_HEIGHT / 2.0),
            UiAction::None,
            "the strip is the interface's own business"
        );
        assert!(ui.tab_scroll() > was, "and it moved");
    }

    /// Which tabs can be pressed on the strip as it is drawn right now.
    fn on_the_strip(ui: &mut BrowserUi, text: &mut TextEngine, width: f64) -> Vec<usize> {
        let mut seen = Vec::new();
        let mut x = 0.0;
        while x < width {
            if let Some(UiAction::SelectTab(index)) = ui.action_at(x, TAB_STRIP_HEIGHT / 2.0, text)
                && !seen.contains(&index)
            {
                seen.push(index);
            }
            x += 4.0;
        }
        seen.sort_unstable();
        seen
    }

    /// One frame with `active` the tab being read.
    fn frame_active(
        ui: &mut BrowserUi,
        text: &mut TextEngine,
        width: f64,
        tabs: usize,
        active: usize,
    ) {
        let mut list = DisplayList::default();
        list.append(&ui.build_display_list(
            width,
            600.0,
            &labels(tabs),
            active,
            (true, true),
            None,
            text,
        ));
    }

    /// The accelerator every browser uses: ⌘F opens the bar, gives it the
    /// keyboard, and selects what was there so the next letter replaces it.
    #[test]
    fn command_f_opens_the_find_bar_and_gives_it_the_keyboard() {
        let mut text = TextEngine::isolated();
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);

        assert!(!ui.finding(), "nothing is being looked for yet");
        assert_eq!(
            ui.key_pressed(Key::Character('f'), ACCELERATOR, &mut text, &mut clipboard),
            UiAction::None,
            "the bar is the interface's own, so the browser hears nothing"
        );
        assert!(ui.finding());
        assert!(
            ui.find_wants_keyboard(),
            "the field it asked for does not exist until the next frame"
        );

        // The frame that builds it is the frame that grants it.
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        assert!(ui.find_focused(), "the bar's field has the keyboard");
        assert!(!ui.address_focused(), "and the address field has not");
        assert!(!ui.find_wants_keyboard(), "the wish was granted once");

        // What is typed goes into the bar rather than into the address.
        assert!(ui.text_input('n'));
        assert!(ui.text_input('e'));
        assert_eq!(ui.find.text(), "ne");
        assert_eq!(
            ui.address.text(),
            "",
            "the address field was not typed into"
        );

        // And ⌘F again selects it all, so the next letter starts over.
        ui.key_pressed(Key::Character('f'), ACCELERATOR, &mut text, &mut clipboard);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        assert_eq!(ui.find.selected_text(), Some("ne"));
    }

    /// Return steps forward, shift-Return steps back, and ⌘G does both without
    /// the bar needing the keyboard at all.
    #[test]
    fn the_bar_steps_with_return_and_with_command_g() {
        let mut text = TextEngine::isolated();
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);

        ui.key_pressed(Key::Character('f'), ACCELERATOR, &mut text, &mut clipboard);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        assert_eq!(
            ui.key_pressed(Key::Enter, Modifiers::default(), &mut text, &mut clipboard),
            UiAction::FindStep(true),
            "Return in the bar is *next*, not *submit*"
        );
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            ui.key_pressed(Key::Enter, shift, &mut text, &mut clipboard),
            UiAction::FindStep(false)
        );

        // The keyboard goes back to the page and ⌘G still steps: a reader who
        // has found what they were looking for is reading it, not typing.
        ui.blur();
        assert!(!ui.find_focused());
        assert_eq!(
            ui.key_pressed(Key::Character('g'), ACCELERATOR, &mut text, &mut clipboard),
            UiAction::FindStep(true)
        );
        let shift_accelerator = Modifiers {
            shift: true,
            ..ACCELERATOR
        };
        assert_eq!(
            ui.key_pressed(
                Key::Character('g'),
                shift_accelerator,
                &mut text,
                &mut clipboard
            ),
            UiAction::FindStep(false)
        );
    }

    /// Escape closes the bar and hands the keyboard back to where it was, which
    /// is what makes the bar something a keyboard can look into and leave.
    #[test]
    fn escape_closes_the_find_bar_and_returns_the_keyboard() {
        let mut text = TextEngine::isolated();
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        ui.focus_address();
        let before = ui.focused();

        ui.key_pressed(Key::Character('f'), ACCELERATOR, &mut text, &mut clipboard);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        assert!(ui.find_focused());

        ui.key_pressed(Key::Escape, Modifiers::default(), &mut text, &mut clipboard);
        assert!(!ui.finding(), "the bar is gone");
        assert_eq!(
            ui.focused(),
            before,
            "and the keyboard is back where it was"
        );
    }

    /// Tab walks the bar's own controls and no further: the field, the two
    /// arrows and the cross are the whole of where the keyboard may go.
    #[test]
    fn tab_is_trapped_inside_the_open_find_bar() {
        let mut text = TextEngine::isolated();
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        // Something found, so the arrows are live: a dead control is not a place
        // the keyboard goes, here or anywhere else.
        ui.find_status = FindStatus {
            total: 17,
            current: 1,
        };
        ui.key_pressed(Key::Character('f'), ACCELERATOR, &mut text, &mut clipboard);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);

        let mut seen = Vec::new();
        for _ in 0..8 {
            ui.key_pressed(Key::Tab, Modifiers::default(), &mut text, &mut clipboard);
            assert_eq!(
                ui.focus.scope(ui.focused()),
                Some(FIND_SCOPE),
                "Tab walked out of the bar"
            );
            let at = ui.focused();
            if !seen.contains(&at) {
                seen.push(at);
            }
        }
        assert_eq!(
            seen.len(),
            4,
            "the field, both arrows and the cross: {seen:?}"
        );
    }

    /// The bar is a mode rather than a choice, so reaching past it leaves it
    /// open — a menu would have gone away.
    #[test]
    fn a_press_elsewhere_and_command_l_leave_the_find_bar_open() {
        let mut text = TextEngine::isolated();
        let mut clipboard = crate::clipboard::InMemory::default();
        let mut ui = BrowserUi::new();
        frame_at(&mut ui, &mut text, 1000.0, 700.0);
        ui.key_pressed(Key::Character('f'), ACCELERATOR, &mut text, &mut clipboard);
        frame_at(&mut ui, &mut text, 1000.0, 700.0);

        // ⌘L takes the keyboard to the address field. The bar stays: a reader
        // who types an address has not stopped looking for anything.
        ui.key_pressed(Key::Character('l'), ACCELERATOR, &mut text, &mut clipboard);
        assert!(ui.address_focused());
        assert!(ui.finding(), "⌘L closed the find bar");

        // A press on plain toolbar, which dismisses a menu.
        ui.pointer_moved(500.0, TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT / 2.0, &mut text);
        ui.pointer_pressed(&mut text, 1);
        assert!(ui.finding(), "a press elsewhere closed the find bar");

        // The menu, for contrast: opening it displaces the bar, because there
        // is one popup, and pressing away from the menu puts the menu away.
        ui.open_menu();
        assert!(!ui.finding(), "two panels at once");
    }
}
