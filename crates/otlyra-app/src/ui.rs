//! The browser's own interface: the tab strip and the toolbar.
//!
//! Drawn with the same `otlyra-gfx` stack the page is drawn with, and for the
//! same reason the plan gives: by the time an interface is needed we already own
//! text layout, hit testing, input routing and painting, and a second toolkit
//! would duplicate all four and bring a second event model with it.
//!
//! The interface is a [`crate::widget`] tree, rebuilt from this struct's state
//! every frame and thrown away after. That sounds wasteful and is not: a toolbar
//! is a dozen boxes, and building from state means there is no second copy of
//! anything to fall out of step — no widget holding a stale title, no hover flag
//! left set when the pointer moved somewhere else, no identity to match between
//! frames. What survives a frame is the *geometry*: the tree is kept so that the
//! next press can be tested against exactly the rectangles that were drawn,
//! which is the one thing that must not be recomputed.
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
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Button, Child, Cx, Described, Event, Fixed, Focus, FocusId, FocusKind,
    Insets, Label, Padding, Painted, Role, Size, Stack, Widget, fill_rounded,
};

/// Height of the tab strip, in logical pixels.
const TAB_STRIP_HEIGHT: f64 = 36.0;
/// Height of the toolbar under it.
const TOOLBAR_HEIGHT: f64 = 42.0;
/// Total height the interface takes from the top of the window.
pub const UI_HEIGHT: f64 = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT;

/// The widest a tab is allowed to be, however few there are.
const TAB_MAX_WIDTH: f64 = 220.0;
/// The narrowest a tab may shrink to before the strip overflows instead.
const TAB_MIN_WIDTH: f64 = 92.0;
/// The gap between one tab and the next.
const TAB_GAP: f64 = 2.0;
/// The side of the button that opens a tab.
const NEW_TAB_SIZE: f64 = 28.0;

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
    /// Close a tab by index.
    CloseTab(usize),
    /// Make a tab active.
    SelectTab(usize),
    /// Load the active tab's address again.
    Reload,
    /// Go back one entry in the active tab's history.
    Back,
    /// Go forward one entry.
    Forward,
    /// Open one of the browser's own pages.
    OpenPage(SystemPage),
    /// Show the inspector, or put it away.
    ToggleInspector,
    /// Show the menu behind the cogwheel, or put it away.
    ///
    /// Never reaches the browser: the menu is the interface's own state, like
    /// the caret in the address field.
    ToggleMenu,
    /// Put the menu away without doing anything else — what a press anywhere
    /// off the panel means.
    CloseMenu,
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
    /// What this program is.
    About,
}

impl SystemPage {
    /// Whether this one has been built yet.
    ///
    /// The menu lists all of them and dims the rest, rather than growing an
    /// entry per milestone: what a browser cannot do *yet* is worth saying.
    pub fn available(self) -> bool {
        matches!(self, Self::Settings | Self::History | Self::About)
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
            Self::About => "About Otlyra",
        }
    }
}

/// What one tab shows in the strip.
#[derive(Clone, Debug)]
pub struct TabLabel {
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
    tabs: Vec<(String, bool)>,
    active: usize,
    history: (bool, bool),
    spinner: Option<f32>,
    pointer: (f64, f64),
    pointer_down: bool,
    address: String,
    caret: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    focus: Option<FocusId>,
    menu: Option<f64>,
}

/// The interface's own state: what is focused, where the pointer is, what is typed.
pub struct BrowserUi {
    /// The address field.
    pub address: TextField,
    /// Whether the menu behind the cogwheel is open.
    pub menu_open: bool,
    /// Every colour and measurement the interface is drawn from.
    pub theme: Theme,
    /// Which control has the keyboard, if any.
    ///
    /// One value rather than a focus id beside an `address_focused` flag: the
    /// field shows a caret exactly when this lands on its id, so there is
    /// nothing to keep in step.
    focused: Option<FocusId>,
    /// The focusable controls the last frame built, in the order it built them.
    focus: Focus,
    pointer: (f64, f64),
    pointer_down: bool,
    /// Where the pointer went down, while it is still down. What lets a drag
    /// that began in the address field keep selecting past its edge.
    press_origin: Option<(f64, f64)>,
    /// How many clicks the current press is the latest of.
    clicks: u32,
    /// What the last built list was built from, and the list itself.
    cache: Option<(Appearance, DisplayList)>,
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
        Self {
            address: TextField::default(),
            menu_open: false,
            theme: Theme::light(),
            focused: None,
            focus: Focus::default(),
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            clicks: 1,
            cache: None,
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

    /// Draw from `theme` from the next frame on.
    ///
    /// Through a method rather than the field, because the cache does not key
    /// on the theme: a stored list is a list in the old palette, and it has to
    /// go when the palette does.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.cache = None;
        }
    }

    /// Note where the pointer is. Kept so a press can be tested against the same
    /// geometry the last frame drew.
    ///
    /// While the button is down, the move is offered to the tree: a drag that
    /// began in the address field is a selection growing, and the field is the
    /// one that knows which offset the pointer is over.
    pub fn pointer_moved(&mut self, x: f64, y: f64, text: &mut TextEngine) {
        self.pointer = (x, y);
        if self.press_origin.is_none() {
            return;
        }
        let mut cx = self.cx(text);
        let action = self
            .root
            .as_mut()
            .and_then(|root| root.event(&Event::PointerMoved, &mut cx));
        if let Some(UiAction::AddressHit(hit)) = action {
            self.address.hit(hit);
        }
    }

    /// Whether the address field has the keyboard.
    ///
    /// A question about where the focus is, not a flag: the field is the one
    /// text entry the toolbar builds, so the caret is on exactly when the focus
    /// is on something a caret belongs in.
    pub fn address_focused(&self) -> bool {
        self.focus.kind(self.focused) == Some(FocusKind::Text)
    }

    /// Put the caret in the address field, for an accelerator that names it.
    ///
    /// The whole address is selected, which is what ⌘L is *for*: the next
    /// keystroke replaces it. Nothing happens before the first frame, because
    /// until then no field has been drawn for the caret to be in.
    pub fn focus_address(&mut self) {
        if let Some(id) = self.focus.first_text() {
            self.focused = Some(id);
            self.address.select_all();
        }
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
            Some(UiAction::Focus(_) | UiAction::AddressHit(_)) => Some(Cursor::Text),
            // The sheet behind an open menu answers everywhere, and everywhere
            // is not a thing to point at: dismissing is what happens when you
            // press *nothing*, so it reads as nothing.
            Some(UiAction::CloseMenu) | None => None,
            Some(_) => Some(Cursor::Pointer),
        }
    }

    /// Whether the pointer is over the interface rather than the page.
    ///
    /// An open menu counts as the interface wherever it reaches, which is how a
    /// press on the panel stops being a press on the document under it.
    pub fn owns_pointer(&self) -> bool {
        self.pointer.1 < UI_HEIGHT || self.menu_open
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
        if self.pointer.1 >= UI_HEIGHT && !self.menu_open {
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

        match action {
            Some(UiAction::Focus(id)) => {
                self.focused = Some(id);
                self.menu_open = false;
                UiAction::None
            }
            // A press in the field: the keyboard moves there and the caret goes
            // where the press landed — or the word does, or the lot, by the
            // click count. The field said where; whose keyboard it is stays
            // the surface's business.
            Some(UiAction::AddressHit(hit)) => {
                if let Some(id) = self.focus.first_text() {
                    self.focused = Some(id);
                }
                self.menu_open = false;
                self.address.hit(hit);
                UiAction::None
            }
            Some(UiAction::ToggleMenu) => {
                self.menu_open = !self.menu_open;
                self.focused = None;
                UiAction::None
            }
            Some(UiAction::CloseMenu) => {
                self.menu_open = false;
                UiAction::None
            }
            // Choosing something from the menu closes it. A menu that stayed
            // open over the page it just opened would have to be dismissed by
            // hand every time.
            Some(UiAction::OpenPage(page)) => {
                self.menu_open = false;
                UiAction::OpenPage(page)
            }
            // The same for the inspector: chosen from the menu, the menu goes
            // away and what was chosen is what happens.
            Some(UiAction::ToggleInspector) => {
                self.menu_open = false;
                UiAction::ToggleInspector
            }
            Some(action) => {
                if !matches!(
                    action,
                    UiAction::Reload | UiAction::Back | UiAction::Forward
                ) {
                    self.focused = None;
                }
                action
            }
            None => {
                self.focused = None;
                self.menu_open = false;
                UiAction::None
            }
        }
    }

    /// The press ended: drags stop growing selections.
    pub fn pointer_released(&mut self) {
        self.pointer_down = false;
        self.press_origin = None;
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
                self.menu_open = !self.menu_open;
                UiAction::None
            }
            Some(UiAction::OpenPage(page)) => {
                self.menu_open = false;
                UiAction::OpenPage(page)
            }
            Some(UiAction::ToggleInspector) => {
                self.menu_open = false;
                UiAction::ToggleInspector
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
        // Accelerators work whether or not the field has focus.
        // F5 reloads whatever has focus, including the address field: it is not
        // a character, so it cannot be something the user meant to type.
        if key == Key::F5 {
            return UiAction::Reload;
        }

        if key == Key::Escape && self.menu_open {
            self.menu_open = false;
            return UiAction::None;
        }

        if modifiers.is_accelerator() {
            // A focused field gets first claim on the editing accelerators —
            // ⌘C in the address bar is a copy, not a browser command. The rest
            // stay the browser's: ⌘L and ⌘R work from inside the field too.
            if self.address_focused() && self.address.edit(key, modifiers, clipboard) {
                return UiAction::None;
            }
            return match key {
                Key::Character('r') => UiAction::Reload,
                // The bracket keys are what this platform's browsers use, and the
                // arrows are what the rest of them use; both are here because a
                // person's fingers know one of the two.
                Key::Character('[') | Key::Left => UiAction::Back,
                Key::Character(']') | Key::Right => UiAction::Forward,
                Key::Character('t') => UiAction::NewTab,
                Key::Character('l') => {
                    self.focus_address();
                    UiAction::None
                }
                _ => UiAction::None,
            };
        }

        // Traversal, before anything a control might read the key as: Tab is
        // never a character the address field wants.
        if key == Key::Tab {
            self.focused = if modifiers.shift {
                self.focus.previous(self.focused)
            } else {
                self.focus.next(self.focused)
            };
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
                self.focused = None;
                let typed = self.address.text().trim().to_owned();
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
        if !self.address_focused() {
            return false;
        }
        self.address.insert(character);
        true
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
        out: &mut DisplayList,
    ) {
        let appearance = Appearance {
            width,
            tabs: tabs
                .iter()
                .map(|tab| (tab.title.clone(), tab.loading))
                .collect(),
            active,
            history,
            spinner,
            pointer: self.pointer,
            pointer_down: self.pointer_down,
            address: self.address.text().to_owned(),
            caret: self.address_focused().then(|| self.address.caret()),
            selection: self
                .address_focused()
                .then(|| self.address.selection())
                .flatten(),
            focus: self.focused,
            menu: self.menu_open.then_some(height),
        };

        // Nothing it is drawn from has moved, so last frame's list is this
        // frame's list. The tree is kept too, so a press still meets the
        // rectangles that are on screen.
        if let Some((built, list_of)) = &self.cache
            && *built == appearance
            && self.root.is_some()
        {
            out.append(list_of);
            return;
        }

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
        self.focus.begin();
        let mut root = self.build(width, tabs, active, history, spinner, text);
        let mut cx = self.cx(text);
        root.measure(surface, &mut cx);
        root.place(Rect::new(0.0, 0.0, surface.width, surface.height), &mut cx);
        root.draw(&mut cx, list);

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
        self.cache = Some((appearance, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    /// A drawing context over `text`, carrying this interface's pointer and theme.
    fn cx<'a>(&self, text: &'a mut TextEngine) -> Cx<'a> {
        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.clicks = self.clicks;
        cx.focus = self.focused;
        cx.theme = self.theme.clone();
        cx
    }

    /// Build this frame's tree from this frame's state.
    fn build(
        &self,
        width: f64,
        tabs: &[TabLabel],
        active: usize,
        history: (bool, bool),
        spinner: Option<f32>,
        text: &mut TextEngine,
    ) -> Child<UiAction> {
        let theme = self.theme.clone();
        let focus = self.focus.clone();
        let mut cx = self.cx(text);
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
                            Box::new(Fixed::height(
                                TAB_STRIP_HEIGHT,
                                tab_strip(&theme, &focus, &mut cx, width, tabs, active, spinner),
                            )),
                            Box::new(Fixed::height(
                                TOOLBAR_HEIGHT,
                                toolbar(&theme, &focus, self, history, spinner),
                            )),
                        ],
                    )),
                )),
                Box::new(crate::widget::Flex::new(
                    1.0,
                    Box::new(crate::widget::Gap::new(0.0, 0.0)),
                )),
            ],
        ));

        if !self.menu_open {
            return rows;
        }

        // Panel first in the list so it is drawn last and answers first; the
        // sheet under it catches every press that misses, which is what makes
        // clicking anywhere else dismiss the menu without also doing whatever
        // was under the pointer.
        Box::new(crate::widget::Overlay::new(vec![
            rows,
            controls::scrim(UiAction::CloseMenu),
            Box::new(crate::widget::Anchored::from_right(
                theme.inset,
                UI_HEIGHT - 2.0,
                menu(&theme, &focus),
            )),
        ]))
    }
}

/// The strip of tabs, and the button that opens another.
fn tab_strip(
    theme: &Theme,
    focus: &Focus,
    cx: &mut Cx,
    width: f64,
    tabs: &[TabLabel],
    active: usize,
    spinner: Option<f32>,
) -> Child<UiAction> {
    let inset = theme.inset * 0.75;
    // Tabs shrink to share the strip rather than overflowing it: a tab you
    // cannot see is a tab you cannot close. Past the floor they do overflow,
    // which is a stated gap rather than a surprise.
    let available = (width - inset * 2.0 - NEW_TAB_SIZE - theme.gap).max(0.0);
    let each = if tabs.is_empty() {
        TAB_MAX_WIDTH
    } else {
        TAB_MAX_WIDTH
            .min(available / tabs.len() as f64 - TAB_GAP)
            .max(TAB_MIN_WIDTH)
    };

    let mut children: Vec<Child<UiAction>> = Vec::with_capacity(tabs.len() * 2 + 2);
    for (index, label) in tabs.iter().enumerate() {
        children.push(tab(
            theme,
            focus,
            cx,
            label,
            index,
            index == active,
            each,
            spinner,
        ));
        // A hairline between two tabs that are both in the background, so a run
        // of them reads as several rather than as one wide empty area. Beside
        // the active tab there is nothing to separate: its own edge does that.
        let next_is_active = index + 1 == active;
        if index + 1 < tabs.len() && index != active && !next_is_active {
            children.push(separator(theme));
        }
    }

    children.push(controls::icon_button(
        theme,
        focus,
        UiAction::NewTab,
        true,
        "New tab",
        icon::plus,
    ));
    // Everything is pushed to the left; whatever is left over is empty strip.
    children.push(Box::new(crate::widget::Flex::new(
        1.0,
        Box::new(crate::widget::Gap::new(0.0, 0.0)),
    )));

    Box::new(Padding::new(
        Insets {
            left: inset,
            top: 4.0,
            right: inset,
            bottom: 0.0,
        },
        Box::new(Stack::row(TAB_GAP, children)),
    ))
}

/// The menu behind the cogwheel: everything the browser is, as opposed to
/// everything a page is.
fn menu(theme: &Theme, focus: &Focus) -> Child<UiAction> {
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
            row(Bookmarks, icon::star, None),
            row(Downloads, icon::download, Some("⌘⇧J")),
            controls::divider(theme),
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
    let title = controls::elide(cx, &label.title, room, Elide::End);

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
                .focus(id),
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
    let reload = controls::icon_button(
        theme,
        focus,
        UiAction::Reload,
        true,
        "Reload",
        move |list, rect, color| {
            icon::reload(list, rect, spinner, color);
        },
    );

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
                title: format!("Tab {index}"),
                loading: false,
            })
            .collect()
    }

    /// Draw one frame, which is what gives the interface the geometry every
    /// press is then tested against.
    fn frame(ui: &mut BrowserUi, text: &mut TextEngine, width: f64, tabs: usize) {
        let mut list = DisplayList::default();
        ui.build_display_list(
            width,
            600.0,
            &labels(tabs),
            0,
            (true, true),
            None,
            text,
            &mut list,
        );
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
        ui.build_display_list(
            900.0,
            600.0,
            &labels(1),
            0,
            (false, false),
            None,
            &mut text,
            &mut list,
        );

        let described = ui.describe();
        assert!(
            described.iter().any(|node| !node.enabled),
            "a browser with no history describes no disabled control"
        );
    }

    /// The rectangle the widget tree placed something at, found by pressing.
    fn press(ui: &mut BrowserUi, text: &mut TextEngine, x: f64, y: f64) -> UiAction {
        ui.pointer_moved(x, y, text);
        ui.pointer_pressed(text, 1)
    }

    /// Draw one frame at a given window size, and say what it drew.
    fn frame_at(ui: &mut BrowserUi, text: &mut TextEngine, width: f64, height: f64) -> DisplayList {
        let mut list = DisplayList::new();
        ui.build_display_list(
            width,
            height,
            &labels(2),
            0,
            (true, true),
            None,
            text,
            &mut list,
        );
        list
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
    fn a_narrower_window_does_rebuild_it() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();

        frame_at(&mut ui, &mut text, 1000.0, 800.0);
        // Width is the one thing the interface is laid out against: the tabs
        // share it and the address field takes what is left.
        frame_at(&mut ui, &mut text, 700.0, 800.0);
        assert_eq!(ui.builds(), 2);
    }

    #[test]
    fn an_open_menu_makes_the_height_matter_again() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        ui.menu_open = true;

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
        ui.build_display_list(
            1000.0,
            600.0,
            &labels(1),
            0,
            (false, false),
            None,
            &mut text,
            &mut list,
        );

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
    /// is interface work this milestone does not do.
    #[test]
    fn many_tabs_shrink_to_share_the_strip_and_stop_at_a_floor() {
        let mut text = TextEngine::new();
        let mut ui = BrowserUi::new();
        frame(&mut ui, &mut text, 1000.0, 20);

        // Twenty tabs across a 1000px strip would be 47px each if they kept
        // dividing, and a 47px tab holds no title. They stop at the floor
        // instead, so the right end of the strip is the tenth of them and the
        // rest have run off the edge — visible in that the press lands on a tab
        // in the middle of the run rather than on the last of them.
        let strip_end = press(&mut ui, &mut text, 990.0, TAB_STRIP_HEIGHT / 2.0);
        assert_eq!(strip_end, UiAction::SelectTab(10));

        // And the floor is the floor: the tab before that one starts a full
        // tab-width earlier.
        assert_eq!(
            press(
                &mut ui,
                &mut text,
                990.0 - TAB_MIN_WIDTH - TAB_GAP,
                TAB_STRIP_HEIGHT / 2.0
            ),
            UiAction::SelectTab(9)
        );
    }
}
