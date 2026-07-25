//! `about:bookmarks` — what the reader decided to keep.
//!
//! Two halves, the same shape as the history: [`BookmarkStore`] is the record and
//! [`BookmarksSurface`] is the view, with an [`Action`] enum between them and a tree
//! rebuilt from state each frame.
//!
//! What makes this one different from the history is that it has to survive the
//! process. A history that forgets on quit is a browser with no memory; a bookmark
//! that forgets on quit is a browser that lost something a person chose to keep, and
//! there is no recovering it from anywhere. So the store reads its file when the
//! browser starts and writes it whenever it changes — writing from inside the
//! mutations rather than leaving each caller to remember, because a caller that
//! forgot would lose the one thing this file exists to hold.
//!
//! # The file
//!
//! One bookmark per line, tab-separated: when, address, title. Not the
//! `key = value` subset the preferences use, because this is a list and that format
//! has no way to say so; not JSON, because a bookmark file wants to be readable and
//! repairable by hand. A tab cannot appear in a URL and is stripped from a title, so
//! the separator cannot be ambiguous — and a line that does not parse is skipped
//! with a warning, which keeps one bad line from costing every other bookmark.

use std::path::PathBuf;
use std::rc::Rc;

use jiff::Timestamp;
use otlyra_gfx::DisplayList;
use otlyra_platform::{Key, Modifiers};
use otlyra_text::TextEngine;

use crate::widget::controls::{self, Elide, Elided, Emphasis};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Button, Child, Cx, Described, Event, Flex, Focus, FocusId, Gap, Insets,
    Label, Overflow, Padding, Rect, Scroll, Size, Stack, fill_rounded,
};

/// What the bookmark file is called inside the browser's own directory.
const FILE: &str = "bookmarks.tsv";

/// One page the reader kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    /// The address it opens.
    pub url: String,
    /// What to call it: the document's title, or its address while it had none.
    pub title: String,
    /// When it was kept.
    pub when: Timestamp,
}

/// Everything the reader kept, oldest first.
///
/// Keyed by address: keeping a page twice is keeping it once, and the second press
/// of ⌘D on the same page is the reader telling us they are done with it rather
/// than asking for a duplicate.
#[derive(Default)]
pub struct BookmarkStore {
    entries: Vec<Bookmark>,
    /// Bumped on every change, so a surface can key its cache on the store without
    /// comparing every entry.
    revision: u64,
    /// Where to write. `None` in a test and on a platform with nowhere to write,
    /// which turns persistence off rather than turning the store off.
    file: Option<PathBuf>,
}

impl BookmarkStore {
    /// The store as the last run left it, writing every change from now on.
    ///
    /// Not what [`BookmarkStore::default`] gives, and the difference is the point:
    /// a default store keeps nothing between runs, which is what a test and a
    /// headless session want. Reading the file is the shell's job — the same rule
    /// the preferences and the system clipboard already follow — because a browser
    /// that read it in its constructor would mean four hundred tests reading and
    /// *writing* the file of whoever ran them.
    ///
    /// Never fails. A file that is not there is a browser nobody has kept anything
    /// in; a file that cannot be read is a warning and an empty list, because
    /// refusing to start over a bookmark file would be refusing to start.
    pub fn persisted() -> Self {
        let Some(file) = file_path() else {
            tracing::warn!("nowhere to keep bookmarks; they will not survive this run");
            return Self::default();
        };
        let mut store = match std::fs::read_to_string(&file) {
            Ok(text) => from_text(&text),
            // Not a warning: a browser nobody has kept anything in has no file, and
            // saying so on every launch would be noise about the ordinary case.
            Err(_) => Self::default(),
        };
        store.file = Some(file);
        store
    }

    /// Keep `url`. Returns whether it was added rather than already there.
    pub fn add(&mut self, url: impl Into<String>, title: impl Into<String>) -> bool {
        let url = url.into();
        if url.trim().is_empty() || self.contains(&url) {
            return false;
        }
        self.entries.push(Bookmark {
            url,
            title: clean(&title.into()),
            when: Timestamp::now(),
        });
        self.changed();
        true
    }

    /// Stop keeping `url`. Returns whether there was one to stop keeping.
    pub fn remove(&mut self, url: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|bookmark| bookmark.url != url);
        if self.entries.len() == before {
            return false;
        }
        self.changed();
        true
    }

    /// Keep `url` if it is not kept, stop keeping it if it is.
    ///
    /// Returns whether it is kept now, which is what a caller wants to say to the
    /// reader — the two branches are one answer rather than two.
    pub fn toggle(&mut self, url: impl Into<String>, title: impl Into<String>) -> bool {
        let url = url.into();
        if self.remove(&url) {
            return false;
        }
        self.add(url, title)
    }

    /// Whether `url` is kept.
    pub fn contains(&self, url: &str) -> bool {
        self.entries.iter().any(|bookmark| bookmark.url == url)
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.clear();
        self.changed();
    }

    /// What is kept, newest first — the order a person looks for it in.
    pub fn bookmarks(&self) -> impl Iterator<Item = &Bookmark> {
        self.entries.iter().rev()
    }

    /// How many are kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is kept.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A number that changes whenever the store does.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Note the change and put it on disk.
    ///
    /// Synchronous, and deliberately: it is a few hundred bytes, the preferences are
    /// written the same way, and the alternative — a task on the I/O runtime — would
    /// let two quick changes land in the wrong order and leave the file disagreeing
    /// with the browser. Through a temporary file and a rename, so a crash mid-write
    /// cannot leave half a bookmark list where the whole one was.
    fn changed(&mut self) {
        self.revision += 1;
        let Some(file) = self.file.clone() else {
            return;
        };
        if let Some(directory) = file.parent()
            && let Err(error) = std::fs::create_dir_all(directory)
        {
            tracing::warn!(%error, path = %directory.display(), "could not make the browser's directory");
            return;
        }
        let temporary = file.with_extension("tsv.writing");
        if let Err(error) = std::fs::write(&temporary, self.to_text()) {
            tracing::warn!(%error, path = %temporary.display(), "could not write the bookmarks");
            return;
        }
        if let Err(error) = std::fs::rename(&temporary, &file) {
            tracing::warn!(%error, path = %file.display(), "could not replace the bookmarks");
            let _ = std::fs::remove_file(&temporary);
        }
    }

    /// The store as the file spells it.
    pub fn to_text(&self) -> String {
        let mut text = String::from("# Otlyra's bookmarks: when\taddress\ttitle\n");
        for bookmark in &self.entries {
            text.push_str(&format!(
                "{}\t{}\t{}\n",
                bookmark.when,
                clean(&bookmark.url),
                clean(&bookmark.title)
            ));
        }
        text
    }
}

/// Where the bookmarks live, if the platform will say.
///
/// Beside the preferences, and through the same function, so the environment
/// override that keeps a test out of the developer's home directory covers this
/// file too.
fn file_path() -> Option<PathBuf> {
    Some(crate::preferences::directory()?.join(FILE))
}

/// A tab or a newline in a field would be a second separator, so neither survives.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Read a store back, keeping every line that makes sense.
pub fn from_text(text: &str) -> BookmarkStore {
    let mut store = BookmarkStore::default();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let (Some(when), Some(url)) = (fields.next(), fields.next()) else {
            tracing::warn!(line, "a bookmark line with no address");
            continue;
        };
        let Ok(when) = when.parse::<Timestamp>() else {
            tracing::warn!(line, "a bookmark line whose time is not a time");
            continue;
        };
        let url = url.trim();
        if url.is_empty() {
            tracing::warn!(line, "a bookmark line with a blank address");
            continue;
        }
        // The title may be missing — a page kept before it had one — and an address
        // is a better name for it than nothing at all.
        let title = fields.next().unwrap_or("").trim();
        store.entries.push(Bookmark {
            url: url.to_owned(),
            title: if title.is_empty() {
                url.to_owned()
            } else {
                title.to_owned()
            },
            when,
        });
    }
    // The list says it is newest first, and the file is only in that order because
    // this program wrote it. A hand-edited one need not be, so the order is settled
    // here rather than trusted.
    store.entries.sort_by_key(|bookmark| bookmark.when);
    store
}

/// What the bookmarks surface reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Open what was kept.
    Open(String),
    /// Stop keeping this address.
    Remove(String),
    /// Stop keeping anything.
    Clear,
    /// Leave the surface.
    Close,
}

/// The height of the surface's own header, above the scrolling content.
const HEADER_HEIGHT: f64 = 52.0;
/// The widest the list is allowed to be, however wide the window is.
const CONTENT_WIDTH: f64 = 680.0;

/// Everything the surface's appearance is a function of.
#[derive(Clone, PartialEq)]
struct Drawn {
    rect: Rect,
    revision: u64,
    scroll: f64,
    pointer: (f64, f64),
    focus: Option<FocusId>,
}

/// The bookmarks as a surface: a list of what was kept, and a way to drop it.
pub struct BookmarksSurface {
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    focused: Option<FocusId>,
    focus: Focus,
    scroll: f64,
    overflow: Overflow,
    pointer: (f64, f64),
    cache: Option<(Drawn, DisplayList)>,
    builds: u64,
    root: Option<Child<Action>>,
}

impl Default for BookmarksSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl BookmarksSurface {
    /// An empty, unfocused surface in the default theme.
    pub fn new() -> Self {
        Self {
            theme: Theme::light(),
            focused: None,
            focus: Focus::default(),
            scroll: 0.0,
            overflow: Overflow::default(),
            pointer: (-1.0, -1.0),
            cache: None,
            builds: 0,
            root: None,
        }
    }

    /// What the last frame drew, for something that cannot see it.
    pub fn describe(&self) -> Vec<Described> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            root.describe(&mut out);
        }
        out
    }

    /// Which control holds the keyboard.
    pub fn focused(&self) -> Option<FocusId> {
        self.focused
    }

    /// Relinquish the keyboard when another surface becomes active.
    pub fn blur(&mut self) {
        self.focused = None;
    }

    /// Draw from `theme` from the next frame on.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.cache = None;
        }
    }

    /// How many display lists this surface built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Activate the control a reader named.
    pub fn activate_described(&mut self, index: usize, text: &mut TextEngine) -> Action {
        let Some(focus) = self.describe().get(index).and_then(|node| node.focus) else {
            return Action::None;
        };
        self.focused = Some(focus);
        self.offer(&Event::Activate, text)
    }

    /// Note where the pointer is.
    pub fn pointer_moved(&mut self, x: f64, y: f64) {
        self.pointer = (x, y);
    }

    /// Press at the last reported position.
    pub fn pointer_pressed(&mut self, text: &mut TextEngine) -> Action {
        let action = self.offer(&Event::PointerPressed, text);
        if action == Action::None {
            self.focused = None;
        }
        action
    }

    /// Scroll by `delta` logical pixels, stopping at the ends.
    pub fn scroll_by(&mut self, delta: f64) {
        self.scroll = (self.scroll + delta).clamp(0.0, self.overflow.get());
    }

    /// Handle traversal and activation. `None` leaves the key for the toolbar.
    pub fn key_pressed(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        text: &mut TextEngine,
    ) -> Option<Action> {
        if modifiers.is_accelerator() {
            return None;
        }
        match key {
            Key::Tab => {
                self.focused = if modifiers.shift {
                    self.focus.previous(self.focused)
                } else {
                    self.focus.next(self.focused)
                };
                Some(Action::None)
            }
            Key::Escape => match self.focused {
                Some(_) => {
                    self.focused = None;
                    Some(Action::None)
                }
                None => Some(Action::Close),
            },
            Key::Enter | Key::Character(' ') if self.focused.is_some() => {
                Some(self.offer(&Event::Activate, text))
            }
            _ => None,
        }
    }

    /// What a press at `x`, `y` would report, without applying it.
    pub fn action_at(&mut self, x: f64, y: f64, text: &mut TextEngine) -> Action {
        let pointer = self.pointer;
        self.pointer = (x, y);
        let action = self.offer(&Event::PointerPressed, text);
        self.pointer = pointer;
        action
    }

    /// What the pointer should look like at `x`, `y`.
    pub fn cursor_at(&mut self, x: f64, y: f64, text: &mut TextEngine) -> otlyra_platform::Cursor {
        match self.action_at(x, y, text) {
            Action::None => otlyra_platform::Cursor::Default,
            _ => otlyra_platform::Cursor::Pointer,
        }
    }

    fn offer(&mut self, event: &Event, text: &mut TextEngine) -> Action {
        let Some(root) = self.root.as_mut() else {
            return Action::None;
        };
        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.focus = self.focused;
        cx.theme = self.theme.clone();
        root.event(event, &mut cx).unwrap_or(Action::None)
    }

    /// Paint the page into `rect`, in window coordinates.
    pub fn build_display_list(
        &mut self,
        rect: Rect,
        store: &BookmarkStore,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        let drawn = Drawn {
            rect,
            revision: store.revision(),
            scroll: self.scroll,
            pointer: self.pointer,
            focus: self.focused,
        };
        if let Some((built, list)) = &self.cache
            && *built == drawn
            && self.root.is_some()
        {
            out.append(list);
            return;
        }

        self.builds += 1;
        let mut built = DisplayList::new();
        let theme = self.theme.clone();
        fill_rounded(&mut built, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.focus = self.focused;
        cx.theme = theme.clone();

        self.focus.begin();
        let mut root = self.build(&theme, rect.width, store, &self.focus);
        root.measure(Size::new(rect.width, rect.height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, &mut built);
        self.scroll = self.scroll.clamp(0.0, self.overflow.get());

        self.root = Some(root);
        self.cache = Some((drawn, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    fn build(
        &self,
        theme: &Theme,
        width: f64,
        store: &BookmarkStore,
        focus: &Focus,
    ) -> Child<Action> {
        let mut rows: Vec<Child<Action>> = store
            .bookmarks()
            .map(|bookmark| self.bookmark_row(theme, focus, bookmark))
            .collect();
        if rows.is_empty() {
            rows.push(Box::new(Padding::new(
                Insets::all(theme.inset * 2.0),
                Box::new(Align::centre(Box::new(Label::new(
                    "Nothing kept yet. ⌘D keeps the page you are on.",
                    theme.font_size,
                    theme.ink_dim,
                )))),
            )));
        } else {
            rows = vec![controls::card_plain(theme, rows)];
        }
        rows.push(Box::new(Gap::new(0.0, theme.inset * 2.0)));

        let column: Child<Action> = Box::new(Padding::new(
            Insets::symmetric(theme.inset * 2.0, theme.inset * 2.0),
            Box::new(Stack::column(theme.gap, rows)),
        ));
        let centred: Child<Action> = Box::new(Stack::row(
            0.0,
            vec![
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                Box::new(crate::widget::Fixed::width(
                    CONTENT_WIDTH.min(width),
                    column,
                )),
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
            ],
        ));

        Box::new(Stack::column(
            0.0,
            vec![
                self.header(theme, focus, !store.is_empty()),
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    fn header(&self, theme: &Theme, focus: &Focus, has_bookmarks: bool) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "Bookmarks",
            theme.font_size + 3.0,
            theme.ink,
        ))));
        Box::new(crate::widget::Fixed::height(
            HEADER_HEIGHT,
            Box::new(Background::new(
                theme.surface,
                0.0,
                Box::new(Padding::new(
                    Insets::symmetric(theme.inset * 2.0, theme.gap),
                    Box::new(Stack::row(
                        theme.gap,
                        vec![
                            Box::new(Flex::new(1.0, title)),
                            Box::new(Align::centre(controls::button(
                                theme,
                                focus,
                                Action::Clear,
                                "Remove all",
                                Emphasis::Danger,
                                has_bookmarks,
                            ))),
                            Box::new(Align::centre(controls::button(
                                theme,
                                focus,
                                Action::Close,
                                "Done",
                                Emphasis::Primary,
                                true,
                            ))),
                        ],
                    )),
                )),
            )),
        ))
    }

    /// One bookmark: what it is called and where it goes, and a way to drop it.
    ///
    /// The whole row opens, with the Remove button *inside* it. Nested deliberately:
    /// a button offers an event to its child first — which is what makes the close
    /// cross inside a tab work — so the press that lands on Remove removes, and every
    /// other press on the row opens what was kept. Side by side was tried first and
    /// the labels came out measured at their own width, so a long address was elided
    /// to a third of the space it had.
    fn bookmark_row(&self, theme: &Theme, focus: &Focus, bookmark: &Bookmark) -> Child<Action> {
        // Claimed before the button inside it, so Tab reaches the row and then what
        // is on the row rather than the other way round.
        let row = focus.claim(true);
        let name: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            bookmark.title.clone(),
            theme.font_size,
            theme.ink,
            Elide::End,
        ))));
        let where_to: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            bookmark.url.clone(),
            theme.font_size_small,
            theme.ink_dim,
            Elide::End,
        ))));
        let labels: Child<Action> = Box::new(Flex::new(
            1.0,
            Box::new(Stack::column(theme.gap * 0.5, vec![name, where_to])),
        ));
        let inside: Child<Action> = Box::new(Padding::new(
            Insets::symmetric(theme.inset, theme.gap),
            Box::new(Stack::row(
                theme.inset,
                vec![
                    labels,
                    Box::new(Align::centre(controls::button(
                        theme,
                        focus,
                        Action::Remove(bookmark.url.clone()),
                        "Remove",
                        Emphasis::Normal,
                        true,
                    ))),
                ],
            )),
        ));

        Box::new(
            Button::new(
                Action::Open(bookmark.url.clone()),
                Box::new(
                    Background::new(Theme::CLEAR, theme.radius_small, inside).on_hover(theme.hover),
                ),
            )
            .focus(row),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeping_a_page_twice_keeps_it_once() {
        let mut store = BookmarkStore::default();
        assert!(store.add("https://a.example/", "A"));
        assert!(
            !store.add("https://a.example/", "A again"),
            "the second press must not add a duplicate"
        );
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.bookmarks().next().map(|kept| kept.title.as_str()),
            Some("A"),
            "and must not rename what was already kept"
        );
    }

    #[test]
    fn toggling_says_whether_it_is_kept_now() {
        let mut store = BookmarkStore::default();
        assert!(store.toggle("https://a.example/", "A"), "kept");
        assert!(store.contains("https://a.example/"));
        assert!(!store.toggle("https://a.example/", "A"), "dropped");
        assert!(!store.contains("https://a.example/"));
        assert!(store.is_empty());
    }

    #[test]
    fn a_blank_address_is_not_a_bookmark() {
        // The blank tab has no address, and ⌘D on it must not keep a row that
        // opens nowhere.
        let mut store = BookmarkStore::default();
        assert!(!store.add("", "New tab"));
        assert!(!store.add("   ", "New tab"));
        assert!(store.is_empty());
    }

    #[test]
    fn the_newest_is_first_and_removing_removes() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/", "A");
        store.add("https://b.example/", "B");
        let titles: Vec<&str> = store
            .bookmarks()
            .map(|bookmark| bookmark.title.as_str())
            .collect();
        assert_eq!(titles, ["B", "A"]);

        assert!(store.remove("https://a.example/"));
        assert!(!store.remove("https://a.example/"), "twice is once");
        let titles: Vec<&str> = store
            .bookmarks()
            .map(|bookmark| bookmark.title.as_str())
            .collect();
        assert_eq!(titles, ["B"]);
    }

    #[test]
    fn a_change_moves_the_revision_and_a_non_change_does_not() {
        let mut store = BookmarkStore::default();
        let empty = store.revision();
        store.clear();
        assert_eq!(store.revision(), empty, "clearing nothing changed nothing");
        store.add("https://a.example/", "A");
        assert!(store.revision() > empty);
        let kept = store.revision();
        store.add("https://a.example/", "A");
        assert_eq!(store.revision(), kept, "a duplicate changed nothing");
        store.clear();
        assert!(store.revision() > kept);
    }

    #[test]
    fn what_is_written_is_what_is_read_back() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/one", "One");
        store.add("https://b.example/two", "Two — with a dash");

        let read = from_text(&store.to_text());
        let pairs: Vec<(&str, &str)> = read
            .bookmarks()
            .map(|bookmark| (bookmark.url.as_str(), bookmark.title.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("https://b.example/two", "Two — with a dash"),
                ("https://a.example/one", "One"),
            ]
        );
        // And the times survive, because "when did I keep this" is the only thing
        // the list is ordered by if it is ever sorted.
        assert_eq!(
            read.bookmarks().next().map(|bookmark| bookmark.when),
            store.bookmarks().next().map(|bookmark| bookmark.when)
        );
    }

    #[test]
    fn a_file_that_makes_no_sense_keeps_the_lines_that_do() {
        let read = from_text(
            "# a comment\n\
             \n\
             not a bookmark at all\n\
             2026-07-25T10:00:00Z\thttps://a.example/\tA\n\
             nonsense-time\thttps://b.example/\tB\n\
             2026-07-25T11:00:00Z\t\tblank address\n\
             2026-07-25T12:00:00Z\thttps://c.example/\n",
        );
        let pairs: Vec<(&str, &str)> = read
            .bookmarks()
            .map(|bookmark| (bookmark.url.as_str(), bookmark.title.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                // A bookmark with no title is named by its address rather than
                // drawn as a blank row.
                ("https://c.example/", "https://c.example/"),
                ("https://a.example/", "A"),
            ]
        );
    }

    #[test]
    fn a_title_cannot_smuggle_a_separator_into_the_file() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/", "Tabbed\tand\nbroken");
        assert_eq!(
            store.bookmarks().next().map(|kept| kept.title.as_str()),
            Some("Tabbedandbroken")
        );
        // Which is the point: the round trip cannot be made to invent a field.
        assert_eq!(from_text(&store.to_text()).len(), 1);
    }

    #[test]
    fn an_unchanged_surface_reuses_its_display_list() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/", "A");
        let mut surface = BookmarksSurface::new();
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let rect = Rect::new(0.0, 0.0, 900.0, 700.0);
        surface.build_display_list(rect, &store, &mut text, &mut list);
        surface.build_display_list(rect, &store, &mut text, &mut list);
        assert_eq!(surface.builds(), 1);
    }

    /// Both halves of a row are reachable, and they say different things: the row
    /// opens what was kept and the button beside it drops it.
    #[test]
    fn a_row_both_opens_and_removes() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/", "A");
        let mut surface = BookmarksSurface::new();
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let rect = Rect::new(0.0, 0.0, 900.0, 700.0);
        surface.build_display_list(rect, &store, &mut text, &mut list);

        let mut seen = Vec::new();
        for y in (0..700).step_by(4) {
            for x in (0..900).step_by(8) {
                let action = surface.action_at(f64::from(x), f64::from(y), &mut text);
                if action != Action::None && !seen.contains(&action) {
                    seen.push(action);
                }
            }
        }
        assert!(
            seen.contains(&Action::Open("https://a.example/".to_owned())),
            "the row does not open: {seen:?}"
        );
        assert!(
            seen.contains(&Action::Remove("https://a.example/".to_owned())),
            "the row cannot be dropped: {seen:?}"
        );
    }

    /// Tab reaches every control, and Return on one reports what a press reports.
    #[test]
    fn the_keyboard_reaches_the_whole_surface() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/", "A");
        let mut text = TextEngine::new();

        let mut reached = Vec::new();
        for steps in 1..=5 {
            let mut surface = BookmarksSurface::new();
            let mut list = DisplayList::new();
            let rect = Rect::new(0.0, 0.0, 900.0, 700.0);
            surface.build_display_list(rect, &store, &mut text, &mut list);
            for _ in 0..steps {
                surface.key_pressed(Key::Tab, Modifiers::default(), &mut text);
            }
            if let Some(action) = surface.key_pressed(Key::Enter, Modifiers::default(), &mut text) {
                reached.push(action);
            }
        }

        assert!(reached.contains(&Action::Clear), "{reached:?}");
        assert!(reached.contains(&Action::Close), "{reached:?}");
        assert!(
            reached.contains(&Action::Open("https://a.example/".to_owned())),
            "{reached:?}"
        );
        assert!(
            reached.contains(&Action::Remove("https://a.example/".to_owned())),
            "{reached:?}"
        );
    }

    /// A reader that cannot see the surface is told what is on it, and pressing
    /// through that description runs the same code a click runs.
    #[test]
    fn what_a_reader_is_told_is_what_a_reader_can_press() {
        let mut store = BookmarkStore::default();
        store.add("https://a.example/", "A");
        let mut surface = BookmarksSurface::new();
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        surface.build_display_list(
            Rect::new(0.0, 0.0, 900.0, 700.0),
            &store,
            &mut text,
            &mut list,
        );

        let described = surface.describe();
        assert!(
            described.iter().any(|node| node.label == "Remove"),
            "the description does not mention removing: {:?}",
            described
                .iter()
                .map(|node| node.label.clone())
                .collect::<Vec<_>>()
        );
        let index = described
            .iter()
            .position(|node| node.label == "Remove")
            .expect("the Remove button is described");
        assert_eq!(
            surface.activate_described(index, &mut text),
            Action::Remove("https://a.example/".to_owned())
        );
    }
}
