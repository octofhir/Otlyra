//! `about:cache` — what has already been fetched, and a way to be rid of it.
//!
//! The jar's page answers *who is keeping something about me*; this one answers
//! *what is this browser holding on to*. They are the same question asked of two
//! stores, so they are the same page twice over: grouped by site, a Remove per
//! site, a Remove all, and one name for one site on both — `otlyra_net::cookie`
//! answers that for each, rather than each answering it for itself.
//!
//! What is drawn is what a person can act on. A cache entry's *body* is the page
//! itself and is not shown: it is already on screen when they visit the page, and
//! a list of two hundred bodies is a list nobody reads.

use std::rc::Rc;

use otlyra_gfx::DisplayList;
use otlyra_net::SharedCache;
use otlyra_net::cache::Cache;
use otlyra_platform::{Key, Modifiers};
use otlyra_text::TextEngine;

use crate::widget::controls::{self, Elide, Elided, Emphasis};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Child, Cx, Described, Event, Flex, Focus, FocusId, Gap, Insets, Label,
    Overflow, Padding, Rect, Scroll, Size, Stack, fill_rounded,
};

/// What the cache surface reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Throw away everything held for one site.
    ClearSite(String),
    /// Throw away everything.
    Clear,
    /// Leave the surface.
    Close,
}

/// The height of the surface's own header, above the scrolling content.
const HEADER_HEIGHT: f64 = 52.0;
/// The widest the list is allowed to be, however wide the window is.
const CONTENT_WIDTH: f64 = 680.0;
/// The most addresses one site's card lists before it says how many more.
///
/// A page with two hundred pictures on it is one site and two hundred entries;
/// listing every one of them turns a page a reader is *scanning* into a page they
/// have to scroll past.
const ROWS_PER_SITE: usize = 6;

/// Everything the surface's appearance is a function of.
#[derive(Clone, PartialEq)]
struct Drawn {
    rect: Rect,
    entries: usize,
    bytes: usize,
    /// What the disk is holding, which a restart does not empty.
    kept: (usize, usize),
    scroll: f64,
    pointer: (f64, f64),
    focus: Option<FocusId>,
}

/// How much is held, where, and how much good it has done — the three numbers
/// the summary is drawn from, together because they are read together.
#[derive(Clone, Copy)]
struct Held {
    /// Bytes in memory.
    bytes: usize,
    /// Entries and bytes on the disk.
    kept: (usize, usize),
    /// Hits and misses.
    counts: (u64, u64),
}

/// One site, and everything held for it.
#[derive(Debug)]
struct Site {
    name: String,
    bytes: usize,
    addresses: Vec<String>,
}

/// The cache as a surface: what is held, by whom, and a way to be rid of it.
pub struct CacheSurface {
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

impl Default for CacheSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheSurface {
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
        store: Option<&SharedCache>,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        // Read out from under the lock once: how much is held, and what of.
        let (entries, bytes, kept, sites, hits, misses) = match store {
            Some(store) => {
                let held = store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let (hits, misses) = held.counts();
                let kept = held
                    .disk()
                    .map_or((0, 0), |disk| (disk.len(), disk.bytes()));
                (held.len(), held.bytes(), kept, by_site(&held), hits, misses)
            }
            None => (0, 0, (0, 0), Vec::new(), 0, 0),
        };

        let drawn = Drawn {
            rect,
            entries,
            bytes,
            kept,
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
        let mut root = self.build(
            &theme,
            rect.width,
            &sites,
            Held {
                bytes,
                kept,
                counts: (hits, misses),
            },
            &self.focus,
        );
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
        sites: &[Site],
        held: Held,
        focus: &Focus,
    ) -> Child<Action> {
        let Held {
            bytes,
            kept,
            counts,
        } = held;
        let mut rows: Vec<Child<Action>> = Vec::new();
        if sites.is_empty() && kept.0 == 0 {
            rows.push(Box::new(Padding::new(
                Insets::all(theme.inset * 2.0),
                Box::new(Align::centre(Box::new(Label::new(
                    "Nothing has been kept yet.",
                    theme.font_size,
                    theme.ink_dim,
                )))),
            )));
        } else {
            rows.push(self.summary(theme, bytes, kept, counts));
            for site in sites {
                rows.push(self.site_card(theme, focus, site));
            }
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
                self.header(theme, focus, !sites.is_empty()),
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    /// How much is held, and how much good it is doing.
    ///
    /// The second half is the point of showing either. A cache's whole effect is
    /// invisible — it is the request that was not made — so without the count a
    /// reader has no way to tell a cache that is working from one that is not.
    fn summary(
        &self,
        theme: &Theme,
        bytes: usize,
        (entries, kept): (usize, usize),
        (hits, misses): (u64, u64),
    ) -> Child<Action> {
        // Both tiers, because they answer different questions and one standing
        // for the other is how this page came to say *nothing has been kept* to
        // somebody whose disk held a thousand things. What is in memory is what
        // this run has touched; what is on disk is what survives closing the
        // browser, and that is the number a person means by *is it caching*.
        let held: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            match entries {
                0 => format!("{} held", in_bytes(bytes)),
                _ => format!(
                    "{} held, and {} of it kept between runs",
                    in_bytes(bytes.max(kept)),
                    in_bytes(kept)
                ),
            },
            theme.font_size,
            theme.ink,
        ))));
        let saved: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            match hits + misses {
                0 => "No request has asked for it yet.".to_owned(),
                asked => format!("{hits} of {asked} requests were answered without the network."),
            },
            theme.font_size_small,
            theme.ink_dim,
        ))));
        controls::card_plain(theme, vec![held, saved])
    }

    fn header(&self, theme: &Theme, focus: &Focus, has_entries: bool) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "Cache",
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
                                "Empty",
                                Emphasis::Danger,
                                has_entries,
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

    /// One site: its name, how much of it is held, a way to be rid of it, and
    /// then the first few addresses.
    fn site_card(&self, theme: &Theme, focus: &Focus, site: &Site) -> Child<Action> {
        let name: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            site.name.clone(),
            theme.font_size,
            theme.ink,
            Elide::End,
        ))));
        let count = site.addresses.len();
        let summary: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            format!(
                "{count} {} · {}",
                if count == 1 { "thing" } else { "things" },
                in_bytes(site.bytes)
            ),
            theme.font_size_small,
            theme.ink_dim,
        ))));

        let head: Child<Action> = Box::new(Stack::row(
            theme.inset,
            vec![
                Box::new(Flex::new(
                    1.0,
                    Box::new(Stack::column(theme.gap * 0.5, vec![name, summary])),
                )),
                // Named, because a page of Removes all reading the same word is a
                // page of identical buttons to anything that cannot see the cards.
                Box::new(Align::centre(Box::new(
                    crate::widget::Named::instead_of_its_own(
                        format!("Remove everything held for {}", site.name),
                        controls::button(
                            theme,
                            focus,
                            Action::ClearSite(site.name.clone()),
                            "Remove",
                            Emphasis::Normal,
                            true,
                        ),
                    ),
                ))),
            ],
        ));

        let mut rows: Vec<Child<Action>> = vec![head];
        rows.extend(
            site.addresses
                .iter()
                .take(ROWS_PER_SITE)
                .map(|address| self.address_row(theme, address)),
        );
        if count > ROWS_PER_SITE {
            rows.push(Box::new(Align::left(Box::new(Label::new(
                format!("and {} more", count - ROWS_PER_SITE),
                theme.font_size_small,
                theme.ink_dim,
            )))));
        }
        controls::card_plain(theme, rows)
    }

    /// One address, elided from the front.
    ///
    /// From the front because what tells two of a site's entries apart is the end
    /// of the path: `…/assets/app.css` and `…/assets/app.js` share every character
    /// a reader would be shown by eliding the other way.
    fn address_row(&self, theme: &Theme, address: &str) -> Child<Action> {
        Box::new(Padding::new(
            Insets::symmetric(0.0, theme.gap * 0.5),
            Box::new(Align::left(Box::new(Elided::new(
                address.to_owned(),
                theme.font_size_small,
                theme.ink_dim,
                Elide::Start,
            )))),
        ))
    }
}

/// A size a person reads rather than counts.
fn in_bytes(bytes: usize) -> String {
    const KILO: usize = 1024;
    match bytes {
        0 => "nothing".to_owned(),
        bytes if bytes < KILO => format!("{bytes} bytes"),
        bytes if bytes < KILO * KILO => format!("{} kB", bytes / KILO),
        bytes => format!("{:.1} MB", bytes as f64 / (KILO * KILO) as f64),
    }
}

/// What is held, grouped by the site it came from, alphabetically, with each
/// site's own addresses sorted so the list does not reshuffle as they are used.
///
/// Both tiers, unioned by address. An entry read back from the disk this run is
/// in memory *and* on the disk, and counting it twice would have the page report
/// a site holding twice what it holds. Memory wins where both have it, because
/// that copy is the one being served.
fn by_site(cache: &Cache) -> Vec<Site> {
    let mut sites: Vec<Site> = Vec::new();
    let in_memory: std::collections::HashSet<&str> = cache.entries().map(|(url, _)| url).collect();
    let held: Vec<(&str, usize)> = cache
        .entries()
        .map(|(url, stored)| (url, stored.body.len()))
        .chain(
            cache
                .disk()
                .into_iter()
                .flat_map(otlyra_net::cache::Disk::held)
                .filter(|(url, _)| !in_memory.contains(url)),
        )
        .collect();

    for (url, bytes) in held {
        let name = otlyra_net::cache::store::site_of(url).unwrap_or_else(|| url.to_owned());
        match sites.iter_mut().find(|site| site.name == name) {
            Some(site) => {
                site.bytes += bytes;
                site.addresses.push(url.to_owned());
            }
            None => sites.push(Site {
                name,
                bytes,
                addresses: vec![url.to_owned()],
            }),
        }
    }
    sites.sort_by(|one, other| one.name.cmp(&other.name));
    for site in &mut sites {
        site.addresses.sort();
    }
    sites
}

#[cfg(test)]
mod tests {
    use super::*;
    use otlyra_net::cache::Stored;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// One response, kept for an hour, with `body` in it.
    fn stored(body: &[u8]) -> Stored {
        Stored {
            status: 200,
            headers: vec![("cache-control".to_owned(), "max-age=3600".to_owned())],
            body: body.to_vec(),
            final_url: "https://a.example/x".to_owned(),
            directives: otlyra_net::cache::Directives::parse(["max-age=3600"]),
            lifetime: otlyra_net::cache::Lifetime::Stated(Duration::from_secs(3600)),
            times: otlyra_net::cache::Times {
                requested: now(),
                received: now(),
                date: now(),
                age: Duration::ZERO,
            },
            varied: Vec::new(),
            varies_on_everything: false,
        }
    }

    fn filled() -> SharedCache {
        let mut cache = Cache::new();
        for (url, body) in [
            ("https://www.example.com/index.html", &b"page"[..]),
            ("https://www.example.com/assets/app.css", b"css"),
            ("https://static.example.com/logo.png", b"png"),
            ("https://other.test/thing", b"x"),
        ] {
            cache.store(
                url,
                "GET",
                Stored {
                    status: 200,
                    headers: vec![("cache-control".to_owned(), "max-age=3600".to_owned())],
                    body: body.to_vec(),
                    final_url: url.to_owned(),
                    directives: otlyra_net::cache::Directives::parse(["max-age=3600"]),
                    lifetime: otlyra_net::cache::Lifetime::Stated(Duration::from_secs(3600)),
                    times: otlyra_net::cache::Times {
                        requested: now(),
                        received: now(),
                        date: now(),
                        age: Duration::ZERO,
                    },
                    varied: Vec::new(),
                    varies_on_everything: false,
                },
                &[],
            );
        }
        Arc::new(Mutex::new(cache))
    }

    fn drawn(surface: &mut CacheSurface, store: Option<&SharedCache>) -> Vec<Described> {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        surface.build_display_list(
            Rect::new(0.0, 0.0, 900.0, 600.0),
            store,
            &mut text,
            &mut list,
        );
        surface.describe()
    }

    fn words(described: &[Described]) -> String {
        described
            .iter()
            .map(|node| node.label.clone())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Grouped by the site it came from, and by the same name the cookie page
    /// uses — one name for one site across both pages.
    #[test]
    fn entries_are_grouped_by_site() {
        let store = filled();
        let held = store.lock().expect("not poisoned");
        let sites = by_site(&held);
        assert_eq!(
            sites.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["example.com", "other.test"]
        );
        assert_eq!(sites[0].addresses.len(), 3, "three hosts, one site");
        assert_eq!(sites[0].bytes, 4 + 3 + 3);
        assert_eq!(
            sites[0].addresses[0], "https://static.example.com/logo.png",
            "and sorted, so the list does not reshuffle as they are used"
        );
    }

    /// Every button a reader is told about is one they can press, and each site's
    /// says which site it is the Remove of.
    #[test]
    fn what_a_reader_is_told_is_what_a_reader_can_press() {
        let store = filled();
        let mut surface = CacheSurface::new();
        let described = drawn(&mut surface, Some(&store));
        let mut text = TextEngine::new();

        let mut reported = Vec::new();
        for (index, node) in described.iter().enumerate() {
            if node.focus.is_none() {
                continue;
            }
            let action = surface.activate_described(index, &mut text);
            if action != Action::None {
                reported.push(action);
            }
        }
        assert!(reported.contains(&Action::Clear), "{reported:?}");
        assert!(reported.contains(&Action::Close), "{reported:?}");
        assert!(reported.contains(&Action::ClearSite("example.com".into())));
        assert!(reported.contains(&Action::ClearSite("other.test".into())));

        let names: Vec<&str> = described
            .iter()
            .filter(|node| node.label.starts_with("Remove everything"))
            .map(|node| node.label.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "Remove everything held for example.com",
                "Remove everything held for other.test"
            ]
        );
    }

    /// A cache's whole effect is the request that was not made, so without the
    /// count a reader cannot tell one that is working from one that is not.
    #[test]
    fn the_page_says_how_much_good_it_is_doing() {
        let store = filled();
        store
            .lock()
            .expect("not poisoned")
            .look_up("https://other.test/thing", &[], now());
        let mut surface = CacheSurface::new();
        let said = words(&drawn(&mut surface, Some(&store)));
        assert!(said.contains("Empty"), "{said:?}");

        let held = store.lock().expect("not poisoned");
        assert_eq!(held.counts(), (1, 0));
    }

    /// With nothing kept there is nothing to empty, and the button says so rather
    /// than being a press that does nothing.
    #[test]
    fn an_empty_cache_offers_nothing_to_empty() {
        let empty: SharedCache = Arc::new(Mutex::new(Cache::new()));
        let mut surface = CacheSurface::new();
        let described = drawn(&mut surface, Some(&empty));
        let button = described
            .iter()
            .find(|node| node.label == "Empty")
            .expect("the button is there");
        assert!(!button.enabled);

        // And a browser with no cache at all draws the same page rather than
        // nothing: every headless mode has none, and a surface that panicked or
        // vanished would be a surface no test ever drew.
        let mut surface = CacheSurface::new();
        let described = drawn(&mut surface, None);
        assert!(described.iter().any(|node| node.label == "Done"));
    }

    /// A frame nothing changed for reuses its display list, and a change to what
    /// is held is a change however unchanged the count is.
    #[test]
    fn an_unchanged_surface_reuses_its_display_list() {
        let store = filled();
        let mut surface = CacheSurface::new();
        drawn(&mut surface, Some(&store));
        let built = surface.builds();
        drawn(&mut surface, Some(&store));
        assert_eq!(surface.builds(), built, "nothing changed");

        store.lock().expect("not poisoned").clear_site("other.test");
        drawn(&mut surface, Some(&store));
        assert_eq!(surface.builds(), built + 1, "a site went");
    }

    /// A size a person reads rather than counts.
    #[test]
    fn what_survived_the_last_run_is_listed_even_though_memory_is_empty() {
        // The bug this exists to stop coming back: the page read the memory tier
        // only, so a browser that had just started said "nothing has been kept"
        // to somebody whose disk held a thousand things.
        let directory =
            std::env::temp_dir().join(format!("otlyra-cache-page-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let disk = otlyra_net::cache::Disk::open(&directory, 1024 * 1024).expect("opened");
        let mut cache = Cache::new().with_disk(disk);
        cache.store(
            "https://kept.example/logo.png",
            "GET",
            stored(b"pretend this is a picture"),
            &[],
        );
        // The writing happens on a thread of its own so that the cache lock is
        // never held over a syscall, so a test that goes and looks at the
        // directory says when it wants it to have landed.
        cache.settle();
        // What a restart is: the disk as it was, and nothing in memory.
        let reopened = otlyra_net::cache::Disk::open(&directory, 1024 * 1024).expect("reopened");
        let cold = Cache::new().with_disk(reopened);
        assert!(cold.is_empty(), "memory should start empty");

        let sites = by_site(&cold);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!(sites[0].name, "kept.example");
        assert_eq!(sites[0].addresses, ["https://kept.example/logo.png"]);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_entry_in_both_tiers_is_counted_once() {
        let directory =
            std::env::temp_dir().join(format!("otlyra-cache-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let disk = otlyra_net::cache::Disk::open(&directory, 1024 * 1024).expect("opened");
        let mut cache = Cache::new().with_disk(disk);
        cache.store("https://a.example/x", "GET", stored(b"1234"), &[]);

        // It is in memory and on the disk at once, which is the ordinary state of
        // anything just fetched. Counting both copies would report a site holding
        // twice what it holds.
        let sites = by_site(&cache);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!(sites[0].addresses.len(), 1, "{sites:?}");
        assert_eq!(sites[0].bytes, 4);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_size_is_said_in_words() {
        assert_eq!(in_bytes(0), "nothing");
        assert_eq!(in_bytes(512), "512 bytes");
        assert_eq!(in_bytes(2048), "2 kB");
        assert_eq!(in_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
