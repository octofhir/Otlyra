//! The browser itself: tabs, navigation, and the loop's `Painter`.
//!
//! One window, several tabs, one of them active. Each tab owns its document and
//! its scroll position; the interface owns what is typed and what is focused; this
//! type owns the two of them and the one thing they share, the font engine.
//!
//! What is here is the type and what the rest of it leans on: what it holds, how
//! one is built, the window and palette it draws against, and which of its surfaces
//! the keyboard belongs to. What it does is in the modules beside this one, one
//! responsibility each, so that a change to how a page loads is not a change to how
//! a key is routed.

use std::collections::{HashMap, HashSet};

use otlyra_dom::NodeId;
use otlyra_platform::{Cursor, Viewport};
use otlyra_text::TextEngine;

use crate::about::AboutSurface;
use crate::downloads::DownloadsSurface;
use crate::fetcher::{Fetcher, Loader};
use crate::settings::SettingsSurface;
use crate::ui::{BrowserUi, UI_HEIGHT};
use crate::widget::runtime::UiSurfaceId;

mod chrome;
mod devtools;
mod driver;
mod frame;
mod input;
mod loading;
mod navigation;
mod painter;
mod pump;
mod subresources;
mod system_pages;
mod tabs;
mod zoom;

pub use devtools::Chosen;
pub use tabs::{HistoryEntry, Readiness, Tab, TabId};
pub use zoom::ZoomStep;

use chrome::ContextTarget;
use frame::Scaled;
use subresources::ImageCache;

const SURFACE_CHROME: UiSurfaceId = UiSurfaceId::new(1);
const SURFACE_PAGE: UiSurfaceId = UiSurfaceId::new(2);
const SURFACE_SYSTEM: UiSurfaceId = UiSurfaceId::new(3);
const SURFACE_INSPECTOR: UiSurfaceId = UiSurfaceId::new(4);

/// The browser.
pub struct Browser {
    text: TextEngine,
    ui: BrowserUi,
    tabs: Vec<Tab>,
    active: usize,
    fetcher: Fetcher,
    /// When the current load started, so the spinner has something to turn by.
    load_started: std::time::Instant,
    /// When this browser started, which is the zero of the clock every page's
    /// animation integrates against.
    started: std::time::Instant,
    /// Whether the browser's own interface is drawn at all.
    ///
    /// Off is for a screenshot that is going to be compared against another
    /// browser's: the page has to start at the top of the picture, or every
    /// comparison is a comparison of two toolbars.
    interface: bool,
    /// Pictures that have already been decoded.
    images: ImageCache,
    /// Background pictures asked for, so none is asked for twice.
    background_requests: HashMap<String, usize>,
    /// How many navigations in a row page script has asked for.
    ///
    /// A redirector page sends the reader on, and the page it sends them to may
    /// send them on again — that is what a sign-in bounce is. A page that does
    /// it forever is a loop, and this is where it stops being our problem.
    script_hops: u8,
    /// Background fetches in flight, by request number.
    background_fetches: HashMap<u64, (usize, String)>,
    /// Fetches for a picture an element chose again, by request number, with the
    /// tab, the element and the address as its markup spells it.
    picture_fetches: HashMap<u64, (usize, NodeId, String, f32)>,
    /// The window the pictures on screen were last chosen against.
    ///
    /// The choice is a question about the window, so it is put again only when
    /// the window is a different one — which keeps a walk of every document off
    /// the ordinary frame.
    picture_window: Option<(f32, f32)>,
    /// The fonts pages have asked for, by family and address, so none is asked
    /// for twice — a page that names its family in ten rules names one file, and
    /// two families out of one file are two fonts.
    font_requests: HashSet<(String, String)>,
    /// Font fetches in flight, by request number, with the family each one is to
    /// be registered under.
    font_fetches: HashMap<u64, String>,
    /// Whether the pointer is taking a selection across the page.
    ///
    /// A press on the text starts one and the release ends it, so that a drag that
    /// wanders into the toolbar or off the window keeps selecting rather than
    /// stopping where it left.
    selecting: bool,
    /// The width of the last frame, so a press can be tested against the geometry
    /// the user was actually looking at.
    last_width: f64,
    /// And its height, which is what a page key scrolls by.
    last_height: f64,
    /// And how many device pixels went to one of them. A page choosing between
    /// the pictures it offers is choosing by this number.
    last_scale: f64,
    /// How much larger than its CSS pixels the page is drawn. See [`Browser::zoom`].
    zoom: f32,
    /// The mark shown on an empty tab. `None` if it failed to decode, which is a
    /// cosmetic problem and not a reason to refuse to draw a frame.
    mark: Option<otlyra_gfx::peniko::ImageData>,
    /// Where the pointer is, in window logical pixels.
    pointer: (f64, f64),
    /// What the pointer should look like where it last was.
    cursor: Cursor,
    /// The one UI root that owns keyboard, text/IME, clipboard, and a11y focus.
    keyboard_surface: UiSurfaceId,
    /// What the open context menu was asked about, while it is open.
    ///
    /// Kept here rather than on the menu because it is a fact about the
    /// document: the row says "open this link in a new tab" and this is what
    /// *this link* means. It is taken when a row is chosen, so a menu dismissed
    /// without choosing anything leaves nothing behind.
    context_target: Option<ContextTarget>,
    /// The preferences.
    ///
    /// One surface for the whole browser rather than one per tab: a preference
    /// is the browser's, and two tabs showing two copies of it could disagree
    /// about what it currently says.
    settings: SettingsSurface,
    /// What this program is.
    about: AboutSurface,
    /// The panel that shows what the engine built.
    inspector: crate::inspector::Inspector,
    /// Where cut, copy and paste go. In memory by default, for the same reason
    /// the preferences are handed in: a test that wrote the system pasteboard
    /// would trade clipboards with the person running it. The shell swaps in
    /// the system one at startup.
    clipboard: Box<dyn crate::clipboard::Clipboard>,
    /// Everywhere the browser has been. Outlives every tab, which is the point.
    history: crate::history::HistoryStore,
    /// The surface that shows it.
    history_page: crate::history::HistorySurface,
    /// Attachments completed during this browser session.
    downloads: crate::downloads::DownloadStore,
    /// Background file writes started from the downloads page.
    downloads_writer: crate::downloads::DownloadWriter,
    /// The surface that shows them.
    downloads_page: DownloadsSurface,
    /// What the reader kept, which outlives every tab and every run.
    bookmarks: crate::bookmarks::BookmarkStore,
    /// The one jar. In memory until a shell asks for it: see `persist_cookies`.
    cookies: crate::cookies::CookieStore,
    /// What the cache may do about the next navigation. Set by a reload and
    /// cleared by the navigation it was set for.
    next_cache_mode: otlyra_net::CacheMode,
    /// What has already been fetched. `None` for a browser whose loader has no
    /// cache either, which is every test and every canned loader.
    cache: Option<otlyra_net::SharedCache>,
    /// The surface that shows them.
    bookmarks_page: crate::bookmarks::BookmarksSurface,
    cookies_page: crate::cookies::CookiesSurface,
    cache_page: crate::cache::CacheSurface,
    /// What the platform last said the environment is. What *System* follows.
    scheme: otlyra_platform::ColorScheme,
    /// The palette every surface is currently drawn from.
    theme: crate::widget::theme::Theme,
    /// Whether the platform needs a new accessibility tree after the next frame.
    accessibility_dirty: bool,
    /// The page's logical list, the scale it was scaled at, and the device list
    /// that resulted. Lets an unchanged page skip re-scaling: while the page
    /// hands back the same `Arc` and the scale holds, the device list is reused.
    page_device: Option<Scaled>,
    /// The same for the tab strip and toolbar, which hand back their cached list
    /// unchanged for every frame nothing in the interface moved.
    chrome_device: Option<Scaled>,
    /// And for the inspector's panel.
    inspector_device: Option<Scaled>,
}

impl Browser {
    /// A browser with one blank tab, fetching through `loader`.
    pub fn new<L: Loader>(loader: L) -> Self {
        Self::with_settings(loader, crate::settings::Settings::default())
    }

    /// A browser over `loader`, starting from `settings`.
    ///
    /// Preferences are handed in rather than read here. Reading them inside the
    /// constructor made every browser depend on a file in the home directory,
    /// which meant a test that saved one changed what the *next* test loaded —
    /// and the suite passed or failed according to what had been clicked last.
    /// Loading them is the shell's job; this is what a browser does with them.
    pub fn with_settings<L: Loader>(loader: L, settings: crate::settings::Settings) -> Self {
        Self::with_fetcher(Fetcher::spawn(loader), settings)
    }

    /// A browser over a transport that suspends rather than blocking.
    ///
    /// What the shell builds. Everything else in the crate hands in a blocking
    /// [`Loader`], because a canned page does not need a future.
    pub fn with_async_loader<L: crate::fetcher::AsyncLoader>(
        loader: L,
        settings: crate::settings::Settings,
    ) -> Self {
        Self::with_fetcher(Fetcher::spawn_async(loader), settings)
    }

    /// The constructor itself, over a pool that has already been built.
    fn with_fetcher(fetcher: Fetcher, settings: crate::settings::Settings) -> Self {
        let mut browser = Self {
            text: TextEngine::new(),
            ui: BrowserUi::new(),
            tabs: vec![Tab::blank()],
            active: 0,
            fetcher,
            load_started: std::time::Instant::now(),
            started: std::time::Instant::now(),
            interface: true,
            images: ImageCache::default(),
            background_requests: HashMap::new(),
            script_hops: 0,
            background_fetches: HashMap::new(),
            picture_fetches: HashMap::new(),
            picture_window: None,
            selecting: false,
            font_requests: HashSet::new(),
            font_fetches: HashMap::new(),
            last_width: 1024.0,
            last_height: 768.0,
            last_scale: 1.0,
            zoom: 1.0,
            mark: otlyra_gfx::decode_image(crate::MARK)
                .inspect_err(|error| tracing::error!(%error, "the mark failed to decode"))
                .ok(),
            pointer: (0.0, 0.0),
            cursor: Cursor::Default,
            keyboard_surface: SURFACE_CHROME,
            context_target: None,
            settings: SettingsSurface::with(settings),
            inspector: crate::inspector::Inspector::new(),
            about: AboutSurface::new(),
            clipboard: Box::new(crate::clipboard::InMemory::default()),
            history: crate::history::HistoryStore::default(),
            history_page: crate::history::HistorySurface::new(),
            downloads: crate::downloads::DownloadStore::default(),
            downloads_writer: crate::downloads::DownloadWriter::new(),
            downloads_page: DownloadsSurface::new(),
            // In memory, and nothing on disk until a shell asks for it: see
            // `persist_bookmarks`.
            bookmarks: crate::bookmarks::BookmarkStore::default(),
            cookies: crate::cookies::CookieStore::in_memory(),
            next_cache_mode: otlyra_net::CacheMode::Default,
            cache: None,
            bookmarks_page: crate::bookmarks::BookmarksSurface::new(),
            cookies_page: crate::cookies::CookiesSurface::new(),
            cache_page: crate::cache::CacheSurface::new(),
            scheme: otlyra_platform::ColorScheme::Light,
            theme: crate::widget::theme::Theme::light(),
            accessibility_dirty: true,
            page_device: None,
            chrome_device: None,
            inspector_device: None,
        };
        browser.apply_theme();
        browser.sync_cookie_policy();
        browser
    }

    /// The palette the appearance preference and the platform agree on, applied
    /// to every surface. Cheap when nothing changed: each surface compares.
    fn apply_theme(&mut self) {
        use crate::widget::theme::Theme;
        let theme = match self.effective_scheme() {
            otlyra_platform::ColorScheme::Light => Theme::light(),
            otlyra_platform::ColorScheme::Dark => Theme::dark(),
        };
        self.theme = theme.clone();
        self.ui.set_theme(theme.clone());
        self.settings.set_theme(theme.clone());
        self.inspector.set_theme(theme.clone());
        self.history_page.set_theme(theme.clone());
        self.downloads_page.set_theme(theme.clone());
        self.bookmarks_page.set_theme(theme.clone());
        self.cookies_page.set_theme(theme.clone());
        self.about.set_theme(theme);
    }

    /// Tell the browser how big the window is going to be, before it has drawn
    /// one.
    ///
    /// A page chooses between the pictures it offers while it is loading, and a
    /// load can finish before the first frame — so a screenshot would otherwise
    /// choose against the size a browser starts out assuming rather than the one
    /// it was asked for. A frame overwrites this with what it actually drew.
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.last_width = viewport.logical_width();
        self.last_height = viewport.logical_height();
        self.last_scale = viewport.scale_factor;
    }

    /// The window as a picture chooses against it: how wide it is, and how many
    /// device pixels go to one CSS pixel.
    ///
    /// The last frame's, because the choice is made when a page loads and the
    /// last frame is the best evidence of what the next one will be.
    fn picture_viewport(&self) -> otlyra_css::cascade::Viewport {
        otlyra_css::cascade::Viewport {
            width: self.last_width as f32,
            height: (self.last_height - if self.interface { UI_HEIGHT } else { 0.0 }).max(0.0)
                as f32,
            scale: self.last_scale as f32,
            text_scale: (self.settings.settings.text_scale / 100.0) as f32,
            color_scheme: match self.effective_scheme() {
                otlyra_platform::ColorScheme::Light => otlyra_css::cascade::ColorScheme::Light,
                otlyra_platform::ColorScheme::Dark => otlyra_css::cascade::ColorScheme::Dark,
            },
        }
    }

    /// The palette in force: the appearance preference, or what the platform
    /// says when that preference is to follow it.
    ///
    /// One answer for two readers — the interface's own theme and the
    /// `prefers-color-scheme` a page is styled against — because a browser
    /// whose toolbar is dark and whose pages are told `light` is answering two
    /// different questions about the same preference.
    fn effective_scheme(&self) -> otlyra_platform::ColorScheme {
        use crate::settings::Appearance;
        match self.settings.settings.appearance {
            Appearance::Light => otlyra_platform::ColorScheme::Light,
            Appearance::Dark => otlyra_platform::ColorScheme::Dark,
            Appearance::System => self.scheme,
        }
    }

    /// Cut, copy and paste against `clipboard` instead of the default memory.
    ///
    /// The shell hands in the system clipboard here; nothing else should.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn crate::clipboard::Clipboard>) {
        self.clipboard = clipboard;
    }

    /// Draw the page and nothing else, for a picture that is going to be compared
    /// with one from elsewhere.
    pub fn hide_interface(&mut self) {
        self.interface = false;
        for tab in &mut self.tabs {
            if let Some(page) = tab.page.as_mut() {
                page.hide_scrollbars();
            }
        }
    }

    /// The tabs, in order.
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Which tab is active.
    pub fn active(&self) -> usize {
        self.active
    }

    /// The interface state, for tests and for the shell.
    pub fn ui(&self) -> &BrowserUi {
        &self.ui
    }

    /// The natural keyboard root for what the active tab currently shows.
    fn tab_surface(&self) -> UiSurfaceId {
        let tab = &self.tabs[self.active];
        if tab.system.is_some() {
            SURFACE_SYSTEM
        } else if tab.page.is_some() || tab.pending.is_some() || tab.error.is_some() {
            SURFACE_PAGE
        } else {
            SURFACE_CHROME
        }
    }

    /// Give one UI root exclusive ownership of keyboard-like input.
    ///
    /// A popup belongs to the root that opened it, so a root becoming the
    /// active one puts away whatever popup the previous one had open. That is
    /// the focus-loss rule and the parent-destruction rule at once: switching
    /// tabs, opening a browser page, or pressing into the panel all arrive here.
    fn activate_surface(&mut self, surface: UiSurfaceId) {
        if self.keyboard_surface == surface {
            return;
        }
        if surface != SURFACE_CHROME {
            self.ui.dismiss_popup();
            self.context_target = None;
        }
        if surface != SURFACE_CHROME {
            self.ui.blur();
        }
        if surface != SURFACE_PAGE
            && let Some(page) = self.tabs[self.active].page.as_mut()
        {
            page.blur();
        }
        if surface != SURFACE_INSPECTOR {
            self.inspector.blur();
        }
        if surface != SURFACE_SYSTEM {
            self.settings.blur();
            self.history_page.blur();
            self.downloads_page.blur();
            self.bookmarks_page.blur();
            self.cookies_page.blur();
            self.about.blur();
        }
        self.keyboard_surface = surface;
        self.accessibility_dirty = true;
    }
}

#[cfg(test)]
mod system_page_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod find_tests;
