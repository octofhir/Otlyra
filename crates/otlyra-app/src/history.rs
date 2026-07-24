//! `about:history` — where the browser has been, drawn rather than fetched.
//!
//! Two halves. [`HistoryStore`] is the record: it belongs to the browser and
//! outlives every tab, because *where have I been* is a question about the
//! browser and a store kept on a tab would forget the answer when the tab
//! closed. [`HistorySurface`] is the view: the same shape as the settings —
//! state, an [`Action`] enum, a tree rebuilt from state each frame and kept
//! only so the next press lands on what was drawn.
//!
//! Visits are grouped by the reader's local day, which is what a person means
//! by "yesterday" — a day boundary in UTC would move their evening browsing
//! into tomorrow or last night depending on where they live.

use std::rc::Rc;

use jiff::Timestamp;
use jiff::civil::Date;
use otlyra_gfx::DisplayList;
use otlyra_platform::{Key, Modifiers};
use otlyra_text::TextEngine;

use crate::clipboard::Clipboard;
use crate::ui::TextField;
use crate::widget::controls::{self, Elide, Elided, Emphasis, FieldHit, FieldView, TextInput};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Button, Child, Cx, Described, Event, Flex, Focus, FocusId, FocusKind, Gap,
    Insets, Label, Overflow, Padding, Rect, Scroll, Size, Stack, fill_rounded,
};

/// One completed navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct Visit {
    /// Where the load ended up — the final URL, after any redirects.
    pub url: String,
    /// The document's title, or its URL while it had none.
    pub title: String,
    /// When it happened.
    pub when: Timestamp,
}

/// Every visit the browser has made, oldest first.
///
/// Recorded beside the per-tab history rather than derived from it: a tab's
/// history is a back/forward stack that truncates when the reader branches,
/// and a record that forgot the branch that did not happen would not be a
/// record of where they have been.
#[derive(Default)]
pub struct HistoryStore {
    visits: Vec<Visit>,
    /// Bumped on every change, so a surface can key its cache on the store
    /// without comparing every visit.
    revision: u64,
}

impl HistoryStore {
    /// Note a completed navigation.
    ///
    /// Called once per navigation, with the final URL — the caller records
    /// where the load *ended up*, so a redirect chain is one visit, not one
    /// per hop.
    pub fn record(&mut self, url: impl Into<String>, title: impl Into<String>, when: Timestamp) {
        self.visits.push(Visit {
            url: url.into(),
            title: title.into(),
            when,
        });
        self.revision += 1;
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.visits.clear();
        self.revision += 1;
    }

    /// The visits, newest first — the order a person looks for them in.
    pub fn visits(&self) -> impl Iterator<Item = &Visit> {
        self.visits.iter().rev()
    }

    /// A number that changes whenever the store does.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// What the history surface reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Nothing.
    None,
    /// Give this control the keyboard.
    Focus(FocusId),
    /// The pointer landed in the search field, at this offset in its text.
    SearchHit(FieldHit),
    /// Navigate to a visited address.
    Open(String),
    /// Forget everything.
    Clear,
    /// Leave the surface.
    Close,
}

/// The height of the surface's own header, above the scrolling content.
const HEADER_HEIGHT: f64 = 52.0;
/// The widest the list is allowed to be, however wide the window is.
const CONTENT_WIDTH: f64 = 680.0;

/// The day a visit belongs to, in the reader's own timezone.
fn local_date(when: Timestamp) -> Date {
    when.to_zoned(jiff::tz::TimeZone::system()).date()
}

/// What a day is called in the list: "Today", "Yesterday", then the date.
fn day_label(date: Date, today: Date) -> String {
    let days = date.until(today).map_or(i32::MAX, |span| span.get_days());
    match days {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => date.strftime("%d %B %Y").to_string(),
    }
}

/// Whether a visit answers a search.
fn matches(visit: &Visit, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    visit.url.to_lowercase().contains(&query) || visit.title.to_lowercase().contains(&query)
}

/// Everything the surface's appearance is a function of.
#[derive(Clone, PartialEq)]
struct Drawn {
    rect: Rect,
    revision: u64,
    today: Date,
    search: String,
    caret: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    scroll: f64,
    pointer: (f64, f64),
    pointer_down: bool,
    focus: Option<FocusId>,
}

/// The history as a surface: a searchable list of where the browser has been.
pub struct HistorySurface {
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    /// What the reader has typed to narrow the list.
    pub search: TextField,
    /// Which control has the keyboard.
    focused: Option<FocusId>,
    focus: Focus,
    scroll: f64,
    overflow: Overflow,
    pointer: (f64, f64),
    pointer_down: bool,
    press_origin: Option<(f64, f64)>,
    clicks: u32,
    /// Built on first `offer`, not at construction: event handling shapes no
    /// text, and building an engine enumerates the system's fonts — cost this
    /// surface's `offer` never needs and startup should not carry.
    engine: Option<TextEngine>,
    cache: Option<(Drawn, DisplayList)>,
    builds: u64,
    root: Option<Child<Action>>,
}

impl Default for HistorySurface {
    fn default() -> Self {
        Self::new()
    }
}

impl HistorySurface {
    /// A surface with nothing typed and nothing focused.
    pub fn new() -> Self {
        Self {
            theme: Theme::light(),
            search: TextField::default(),
            focused: None,
            focus: Focus::default(),
            scroll: 0.0,
            overflow: Overflow::default(),
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            clicks: 1,
            engine: None,
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

    /// Draw from `theme` from the next frame on. The cache does not key on the
    /// theme, so the stored list goes with the old palette.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.cache = None;
        }
    }

    /// How many display lists this surface has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Whether the search field has the keyboard.
    fn searching(&self) -> bool {
        self.focus.kind(self.focused) == Some(FocusKind::Text)
    }

    /// Activate the control a reader named, through the path a press takes.
    pub fn activate_described(&mut self, index: usize) -> Action {
        let Some(focus) = self.describe().get(index).and_then(|node| node.focus) else {
            return Action::None;
        };
        self.focused = Some(focus);
        self.deliver(&Event::Activate)
    }

    /// Note where the pointer is. A drag in the search field grows a selection.
    pub fn pointer_moved(&mut self, x: f64, y: f64) {
        self.pointer = (x, y);
        if self.press_origin.is_none() {
            return;
        }
        if let Action::SearchHit(hit) = self.offer(&Event::PointerMoved) {
            self.search.hit(hit);
        }
    }

    /// Press at the last reported position, `clicks` deep into a run of them.
    pub fn pointer_pressed(&mut self, clicks: u32) -> Action {
        self.pointer_down = true;
        self.press_origin = Some(self.pointer);
        self.clicks = clicks;
        let action = self.deliver(&Event::PointerPressed);
        if !matches!(action, Action::Focus(_) | Action::SearchHit(_)) {
            self.focused = None;
        }
        action
    }

    /// Let go.
    pub fn pointer_released(&mut self) {
        self.pointer_down = false;
        self.press_origin = None;
    }

    /// Scroll by `delta` logical pixels, stopping at the ends.
    pub fn scroll_by(&mut self, delta: f64) {
        self.scroll = (self.scroll + delta).clamp(0.0, self.overflow.get());
    }

    /// Handle a key. `None` means *not mine*, and the key goes on to the toolbar.
    pub fn key_pressed(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        clipboard: &mut dyn Clipboard,
    ) -> Option<Action> {
        if modifiers.is_accelerator() {
            if self.searching() && self.search.edit(key, modifiers, clipboard) {
                return Some(Action::None);
            }
            return None;
        }

        if key == Key::Tab {
            self.focused = if modifiers.shift {
                self.focus.previous(self.focused)
            } else {
                self.focus.next(self.focused)
            };
            return Some(Action::None);
        }

        if self.searching() {
            match key {
                Key::Enter | Key::Escape => {
                    self.focused = None;
                    return Some(Action::None);
                }
                _ if self.search.edit(key, modifiers, clipboard) => {
                    return Some(Action::None);
                }
                _ => {}
            }
        }

        Some(match key {
            Key::Escape => match self.focused {
                Some(_) => {
                    self.focused = None;
                    Action::None
                }
                None => Action::Close,
            },
            Key::Enter | Key::Character(' ') if self.focused.is_some() => {
                self.deliver(&Event::Activate)
            }
            _ => return None,
        })
    }

    /// Handle typed text. Returns whether the surface consumed it.
    pub fn text_input(&mut self, character: char) -> bool {
        if !self.searching() {
            return false;
        }
        self.search.insert(character);
        true
    }

    /// What a press at `x`, `y` would report, without reporting it.
    pub fn action_at(&mut self, x: f64, y: f64) -> Action {
        let (pointer, down) = (self.pointer, self.pointer_down);
        self.pointer = (x, y);
        self.pointer_down = true;
        let action = self.offer(&Event::PointerPressed);
        self.pointer = pointer;
        self.pointer_down = down;
        action
    }

    /// What the pointer should look like at `x`, `y`.
    pub fn cursor_at(&mut self, x: f64, y: f64) -> otlyra_platform::Cursor {
        match self.action_at(x, y) {
            Action::None => otlyra_platform::Cursor::Default,
            Action::Focus(_) | Action::SearchHit(_) => otlyra_platform::Cursor::Text,
            _ => otlyra_platform::Cursor::Pointer,
        }
    }

    /// Offer an event to the last frame's tree and act on what comes back.
    fn deliver(&mut self, event: &Event) -> Action {
        let action = self.offer(event);
        match &action {
            Action::Focus(id) => self.focused = Some(*id),
            Action::SearchHit(hit) => {
                self.focused = self.focus.first_text();
                self.search.hit(*hit);
            }
            _ => {}
        }
        action
    }

    /// Offer an event to the last frame's tree, changing nothing.
    fn offer(&mut self, event: &Event) -> Action {
        let Some(root) = self.root.as_mut() else {
            return Action::None;
        };
        let mut cx = Cx::new(self.engine.get_or_insert_with(TextEngine::new));
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.clicks = self.clicks;
        cx.focus = self.focused;
        cx.theme = self.theme.clone();
        root.event(event, &mut cx).unwrap_or(Action::None)
    }

    /// Paint the surface into `rect`, in window coordinates.
    pub fn build_display_list(
        &mut self,
        rect: Rect,
        store: &HistoryStore,
        today: Date,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        let drawn = Drawn {
            rect,
            revision: store.revision(),
            today,
            search: self.search.text().to_owned(),
            caret: self.searching().then(|| self.search.caret()),
            selection: self.searching().then(|| self.search.selection()).flatten(),
            scroll: self.scroll,
            pointer: self.pointer,
            pointer_down: self.pointer_down,
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
        let list = &mut built;
        let theme = self.theme.clone();
        fill_rounded(list, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.focus = self.focused;
        cx.theme = theme.clone();

        self.focus.begin();
        let mut root = self.build(&theme, rect.width, store, today, &self.focus);
        root.measure(Size::new(rect.width, rect.height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, list);

        // What the frame could scroll is known only now that it is placed;
        // clamp the offset so a cleared list is not left scrolled into space.
        self.scroll = self.scroll.clamp(0.0, self.overflow.get());

        self.root = Some(root);
        self.cache = Some((drawn, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    /// Build this frame's tree, claiming a focus id per control as it goes.
    fn build(
        &self,
        theme: &Theme,
        width: f64,
        store: &HistoryStore,
        today: Date,
        focus: &Focus,
    ) -> Child<Action> {
        let header = self.header(theme, focus);

        let query = self.search.text().to_owned();
        let visits: Vec<&Visit> = store
            .visits()
            .filter(|visit| matches(visit, &query))
            .collect();

        let mut rows: Vec<Child<Action>> = vec![self.search_row(theme, focus)];

        if visits.is_empty() {
            let words = if store.visits().next().is_none() {
                "Nowhere yet. Everywhere you go ends up here."
            } else {
                "Nothing matches the search."
            };
            rows.push(Box::new(Padding::new(
                Insets::symmetric(0.0, theme.inset * 2.0),
                Box::new(Align::centre(Box::new(Label::new(
                    words,
                    theme.font_size,
                    theme.ink_dim,
                )))),
            )));
        }

        let mut day = None;
        let mut group: Vec<Child<Action>> = Vec::new();
        for visit in visits {
            let date = local_date(visit.when);
            if day != Some(date) {
                if !group.is_empty() {
                    rows.push(controls::card_plain(theme, std::mem::take(&mut group)));
                }
                day = Some(date);
                rows.push(Box::new(Padding::new(
                    Insets {
                        left: theme.inset,
                        top: theme.inset,
                        right: 0.0,
                        bottom: theme.gap,
                    },
                    Box::new(Align::left(Box::new(Label::new(
                        day_label(date, today),
                        theme.font_size_small,
                        theme.ink_dim,
                    )))),
                )));
            }
            group.push(self.visit_row(theme, focus, visit));
        }
        if !group.is_empty() {
            rows.push(controls::card_plain(theme, group));
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
                header,
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    /// The bar across the top: the name, forgetting, and the way out.
    fn header(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "History",
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
                                "Clear history",
                                Emphasis::Danger,
                                true,
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

    /// The field the list is narrowed by.
    fn search_row(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        let search_id = focus.claim_text(true);
        let focused = self.focused == Some(search_id);
        TextInput::new(
            FieldView {
                text: self.search.text().to_owned(),
                caret: focused.then(|| self.search.caret()),
                selection: focused.then(|| self.search.selection()).flatten(),
                placeholder: "Search history".to_owned(),
            },
            Action::SearchHit,
        )
        .into_widget(theme)
    }

    /// One visit: the title, and where it was, pressable end to end.
    fn visit_row(&self, theme: &Theme, focus: &Focus, visit: &Visit) -> Child<Action> {
        let id = focus.claim(true);
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            visit.title.clone(),
            theme.font_size,
            theme.ink,
        ))));
        let url: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            visit.url.clone(),
            theme.font_size,
            theme.ink_dim,
            Elide::End,
        ))));
        let row: Child<Action> = Box::new(Padding::new(
            Insets::symmetric(theme.inset, theme.gap),
            Box::new(Stack::row(
                theme.gap,
                vec![
                    Box::new(Flex::new(1.0, Box::new(crate::widget::Clip::new(title)))),
                    Box::new(Flex::new(1.0, url)),
                ],
            )),
        ));
        Box::new(crate::widget::Fixed::height(
            theme.control_height,
            Box::new(
                Button::new(
                    Action::Open(visit.url.clone()),
                    Box::new(
                        Background::new(Theme::CLEAR, theme.radius_small, row)
                            .on_hover(theme.hover),
                    ),
                )
                .focus(id),
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().expect("a valid timestamp")
    }

    #[test]
    fn the_store_remembers_in_the_order_it_is_asked_back() {
        let mut store = HistoryStore::default();
        store.record("https://a.example/", "A", at("2026-07-20T10:00:00Z"));
        store.record("https://b.example/", "B", at("2026-07-21T10:00:00Z"));

        let urls: Vec<&str> = store.visits().map(|visit| visit.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://b.example/", "https://a.example/"],
            "newest first"
        );
    }

    #[test]
    fn clearing_clears() {
        let mut store = HistoryStore::default();
        store.record("https://a.example/", "A", at("2026-07-20T10:00:00Z"));
        let before = store.revision();
        store.clear();
        assert_eq!(store.visits().count(), 0);
        assert!(
            store.revision() > before,
            "a cleared store is a changed store"
        );
    }

    #[test]
    fn days_are_named_from_the_readers_today() {
        let today: Date = "2026-07-22".parse().expect("a date");
        assert_eq!(day_label("2026-07-22".parse().unwrap(), today), "Today");
        assert_eq!(day_label("2026-07-21".parse().unwrap(), today), "Yesterday");
        assert_eq!(
            day_label("2026-07-01".parse().unwrap(), today),
            "01 July 2026"
        );
    }

    #[test]
    fn a_search_matches_title_and_url_and_ignores_case() {
        let visit = Visit {
            url: "https://example.com/Docs".to_owned(),
            title: "The Manual".to_owned(),
            when: at("2026-07-20T10:00:00Z"),
        };
        assert!(matches(&visit, ""));
        assert!(matches(&visit, "manual"));
        assert!(matches(&visit, "DOCS"));
        assert!(!matches(&visit, "nowhere"));
    }

    /// Draw one frame, which is what gives the surface geometry to be pressed.
    fn frame(surface: &mut HistorySurface, store: &HistoryStore) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        surface.build_display_list(
            Rect::new(0.0, 0.0, 900.0, 700.0),
            store,
            "2026-07-22".parse().expect("a date"),
            &mut text,
            &mut list,
        );
    }

    fn store_with_two() -> HistoryStore {
        let mut store = HistoryStore::default();
        store.record("https://a.example/", "First", at("2026-07-21T10:00:00Z"));
        store.record("https://b.example/", "Second", at("2026-07-22T10:00:00Z"));
        store
    }

    #[test]
    fn a_press_on_a_row_reports_the_address_it_shows() {
        let store = store_with_two();
        let mut surface = HistorySurface::new();
        frame(&mut surface, &store);

        // Scan the drawn surface for the row, the way the cursor would find it.
        let open = (0..700)
            .step_by(2)
            .find_map(|y| match surface.action_at(300.0, f64::from(y)) {
                Action::Open(url) => Some(url),
                _ => None,
            })
            .expect("a visit is drawn where a press can reach it");
        assert!(open.ends_with(".example/"), "{open}");
    }

    #[test]
    fn typing_narrows_the_list_and_the_frame_rebuilds() {
        let store = store_with_two();
        let mut surface = HistorySurface::new();
        frame(&mut surface, &store);
        let builds = surface.builds();

        // Focus the field through the frame that drew it, then type.
        surface.focused = surface.focus.first_text();
        assert!(surface.text_input('f'));
        frame(&mut surface, &store);
        assert_eq!(
            surface.builds(),
            builds + 1,
            "the narrowed list is a new frame"
        );

        // "f" matches "First" and not "Second".
        let found = (0..700)
            .step_by(2)
            .filter_map(|y| match surface.action_at(300.0, f64::from(y)) {
                Action::Open(url) => Some(url),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(found.len(), 1);
        assert!(found.contains("https://a.example/"));
    }

    #[test]
    fn an_unchanged_frame_is_not_built_a_second_time() {
        let store = store_with_two();
        let mut surface = HistorySurface::new();
        frame(&mut surface, &store);
        frame(&mut surface, &store);
        assert_eq!(surface.builds(), 1, "nothing about it moved");
    }
}
