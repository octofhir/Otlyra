//! The browser itself: tabs, navigation, and the loop's `Painter`.
//!
//! One window, several tabs, one of them active. Each tab owns its document and
//! its scroll position; the interface owns what is typed and what is focused; this
//! type owns the two of them and the one thing they share, the font engine.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use otlyra_css::cascade::ExternalSheets;
use otlyra_dom::NodeId;
use otlyra_gfx::{PaintTarget, render};
use otlyra_layout::Images;
use otlyra_platform::{
    Cursor, FrameRequest, Key, LayerId, LayerRect, Modifiers, Painter, PainterWork, PlatformEvent,
    Scene, SceneLayer, Viewport, Waker,
};
use otlyra_text::TextEngine;

use crate::about::{self, AboutSurface};
use crate::downloads::{self, DownloadsSurface};
use crate::fetcher::{Body, Fetched, Fetcher, Loader, ResourceKind};
use crate::page::{PageScene, title_of};
use crate::settings::{self, SettingsSurface};
use crate::ui::{BrowserUi, ContextCommand, ContextRow, SystemPage, TabLabel, UI_HEIGHT, UiAction};
use crate::widget::runtime::UiSurfaceId;

/// How long a caller with no event loop waits between checks for a finished fetch.
const FETCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

const SURFACE_CHROME: UiSurfaceId = UiSurfaceId::new(1);
const SURFACE_PAGE: UiSurfaceId = UiSurfaceId::new(2);
const SURFACE_SYSTEM: UiSurfaceId = UiSurfaceId::new(3);
const SURFACE_INSPECTOR: UiSurfaceId = UiSurfaceId::new(4);

/// A load in flight, and everything it is still waiting for.
struct PendingLoad {
    /// The request the document itself was asked for under.
    document: u64,
    /// Where the tab was before, which decides whether this is a new place.
    previous_url: String,
    /// Whether arriving should add a history entry. A reload and a step through
    /// the history are the same place again, so they do not.
    record: bool,
    /// Where to put the reader once the page is built.
    restore_scroll: f32,
    sheets: ExternalSheets,
    images: Images,
    /// Which file each of those pictures came from, and at what density.
    picture_sources: HashMap<NodeId, (String, f32)>,
    /// What each outstanding request will feed once it arrives.
    outstanding: HashMap<u64, Vec<PendingResource>>,
}

/// Which way a zoom is being taken.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ZoomStep {
    /// One stop larger.
    In,
    /// One stop smaller.
    Out,
    /// Back to the page's own size.
    Reset,
}

/// What a subresource is for once it lands.
enum PendingResource {
    /// The `<link>` whose stylesheet this is.
    Stylesheet(NodeId),
    /// The `<img>` whose picture this is, the address it settled on as the
    /// markup spells it, and that candidate's density — which is what the file's
    /// own size is divided by.
    Image(NodeId, String, f32),
}

/// Note in the log when a document asked for more than the limit allows.
fn report_limit(asked: usize, limit: usize, what: &str) {
    if asked > limit {
        tracing::warn!(
            asked,
            fetched = limit,
            "the document asks for more {what} than the limit"
        );
    }
}

/// What an open context menu was asked about.
struct ContextTarget {
    /// Where the press landed, in window logical pixels.
    at: (f64, f64),
    /// The link under it, resolved, if it landed on one.
    link: Option<String>,
}

/// One place a tab has been.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// The address that was loaded, after redirects.
    pub url: String,
    /// How far down the reader had got when they left it. Restored on the way
    /// back, which is the difference between going back and starting over.
    pub scroll: f32,
}

/// One tab.
pub struct Tab {
    /// What this tab is, for as long as it is open.
    ///
    /// A number nobody reuses, handed out on creation. Its position in the strip
    /// is not an identity: closing a tab shifts every tab after it, and anything
    /// holding an index would then be holding a different tab without being told
    /// — which is exactly what a driver does between one command and the next.
    pub id: TabId,
    /// What the address bar shows for it.
    pub url: String,
    /// Its title, or the URL until it has one.
    pub title: String,
    /// The document, absent for a blank tab or one whose load failed.
    pub page: Option<PageScene>,
    /// What went wrong, if anything.
    pub error: Option<String>,
    /// The browser's own page this tab is showing, if it is showing one.
    ///
    /// On the tab rather than on the browser, because `about:settings` is a
    /// place a tab can be — one tab may sit on the preferences while another
    /// reads a document, and going back from it must reach what was there.
    pub system: Option<SystemPage>,
    /// The load in flight, if one is.
    pending: Option<PendingLoad>,
    /// Where this tab has been, oldest first.
    ///
    /// A list and a position rather than two stacks: going back and then somewhere
    /// new drops the forward entries, and that rule is one truncation on a list
    /// instead of a second stack to keep in step.
    history: Vec<HistoryEntry>,
    /// Which entry is showing. Meaningless while the history is empty.
    position: usize,
}

/// What names a tab for as long as it is open.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

/// The next identity to hand out.
///
/// A process-wide counter rather than one per browser: two browsers in one test
/// binary handing out the same names would be two tabs a driver cannot tell
/// apart, and the numbers are cheap.
fn next_tab_id() -> TabId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    TabId(NEXT.fetch_add(1, Ordering::Relaxed))
}

impl Tab {
    /// A blank tab.
    pub fn blank() -> Self {
        Self {
            id: next_tab_id(),
            url: String::new(),
            title: "New tab".to_owned(),
            page: None,
            error: None,
            system: None,
            pending: None,
            history: Vec::new(),
            position: 0,
        }
    }

    /// Whether this tab is waiting for something.
    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether there is anywhere to go back to.
    pub fn can_go_back(&self) -> bool {
        self.position > 0
    }

    /// Whether there is anywhere to go forward to.
    pub fn can_go_forward(&self) -> bool {
        self.position + 1 < self.history.len()
    }
}

impl std::fmt::Debug for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tab")
            .field("url", &self.url)
            .field("title", &self.title)
            .field("loaded", &self.page.is_some())
            .finish()
    }
}

/// How many stylesheets one document may pull in.
///
/// A limit rather than none: every one of these is a synchronous fetch on the way
/// to the first frame, and a document that asks for hundreds is either generated
/// or hostile.
const STYLESHEET_LIMIT: usize = 32;

/// How many pictures one document may pull in, for the same reason.
const IMAGE_LIMIT: usize = 64;

/// How many fonts one document may bring with it.
///
/// A page that ships a family ships a handful of faces of it; one that names
/// dozens is asking for a megabyte of typefaces before its first line is set.
const FONT_LIMIT: usize = 16;

/// How many bytes of decoded pictures are kept between loads.
///
/// Decoded, not encoded: a 200 KB photograph is 8 MB of pixels, and it is the
/// pixels this holds. Sixty-four megabytes is a few screenfuls of them.
const IMAGE_CACHE_BUDGET: usize = 64 * 1024 * 1024;

/// Decoded pictures, kept by address.
///
/// A page that shows the same picture twice decodes it once, and going back to a
/// page that has been visited does not decode its pictures again. Least recently
/// used goes first, because the page in front of the reader is the one whose
/// pictures are worth keeping.
#[derive(Default)]
struct ImageCache {
    /// Oldest use first; the end is the most recently used.
    entries: Vec<(String, otlyra_gfx::peniko::ImageData)>,
    bytes: usize,
}

impl ImageCache {
    /// The picture at `url`, if it is here, marked as just used.
    fn get(&mut self, url: &str) -> Option<otlyra_gfx::peniko::ImageData> {
        let at = self.entries.iter().position(|(key, _)| key == url)?;
        let entry = self.entries.remove(at);
        let image = entry.1.clone();
        self.entries.push(entry);
        Some(image)
    }

    /// Keep `image` under `url`, evicting the least recently used until it fits.
    fn insert(&mut self, url: String, image: otlyra_gfx::peniko::ImageData) {
        let size = image.data.as_ref().len();
        if size > IMAGE_CACHE_BUDGET {
            // One picture larger than the whole budget is not worth evicting
            // everything else for.
            return;
        }
        if let Some(at) = self.entries.iter().position(|(key, _)| *key == url) {
            let (_, old) = self.entries.remove(at);
            self.bytes -= old.data.as_ref().len();
        }

        while self.bytes + size > IMAGE_CACHE_BUDGET && !self.entries.is_empty() {
            let (_, evicted) = self.entries.remove(0);
            self.bytes -= evicted.data.as_ref().len();
        }
        self.bytes += size;
        self.entries.push((url, image));
    }
}

/// The document a picture is shown in.
///
/// A browser given a picture and nothing else wraps it in a document of its own —
/// there is no markup to render, and an `<img>` is what the rest of the engine
/// already knows how to place.
fn image_document(url: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <style>html {{ background: #1c1c1e }} \
         body {{ margin: 0; height: 100vh; display: flex; \
         justify-content: center; align-items: center }} \
         img {{ max-width: 100%; max-height: 100% }}</style>\
         <img src=\"{url}\" alt=\"\">",
        name = escape(url.rsplit('/').next().unwrap_or(url)),
        url = escape(url),
    )
}

/// The document text is shown in.
///
/// Text is text: it is wrapped in a `<pre>` so that its own line breaks and spacing
/// survive, and escaped so that a file full of markup is *shown* rather than
/// rendered — which is the whole point of having decided it was not a document.
fn text_document(text: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8>\
         <style>pre {{ font-family: monospace; white-space: pre; margin: 8px }}</style>\
         <pre>{}</pre>",
        escape(text)
    )
}

/// The four characters that would otherwise be markup.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Decode bytes that are not a document: a stylesheet, or a text file being shown
/// as itself.
///
/// A BOM or a charset from the transport decides; anything else is UTF-8, which is
/// CSS's own default and not HTML's — an unlabelled *document* is assumed to be
/// windows-1252, an unlabelled *stylesheet* is not.
fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    let decision = otlyra_html::determine(bytes, charset);
    match decision.source {
        otlyra_html::EncodingSource::Bom | otlyra_html::EncodingSource::TransportCharset => {
            decision.encoding.decode(bytes).0.into_owned()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The browser.
pub struct Browser {
    text: TextEngine,
    ui: BrowserUi,
    tabs: Vec<Tab>,
    active: usize,
    fetcher: Fetcher,
    /// When the current load started, so the spinner has something to turn by.
    load_started: std::time::Instant,
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

/// A logical display list, the scale it was scaled at, and the device list that
/// came out — one layer's answer to *has this already been scaled?*
type Scaled = (
    Arc<otlyra_gfx::DisplayList>,
    f64,
    Arc<otlyra_gfx::DisplayList>,
);

/// Scale `logical` to device pixels, reusing `cache` while the same list is
/// being scaled by the same factor.
///
/// Pointer identity is the whole test: a surface that hands back the `Arc` it
/// handed back last frame is saying nothing it draws has moved, so the scaled
/// copy of it is still right. Free rather than a method because each layer keeps
/// its own cache and the borrow checker should see that they are separate.
fn scaled(
    cache: &mut Option<Scaled>,
    logical: Arc<otlyra_gfx::DisplayList>,
    scale: f64,
) -> Arc<otlyra_gfx::DisplayList> {
    if let Some((cached_logical, cached_scale, device)) = cache
        && Arc::ptr_eq(cached_logical, &logical)
        && *cached_scale == scale
    {
        return Arc::clone(device);
    }
    let mut copy = (*logical).clone();
    copy.transform(otlyra_gfx::kurbo::Affine::scale(scale));
    let device = Arc::new(copy);
    *cache = Some((logical, scale, Arc::clone(&device)));
    device
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
            interface: true,
            images: ImageCache::default(),
            background_requests: HashMap::new(),
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

    /// The keys that take a selection on the page, or move the one there is.
    ///
    /// `true` means the key was one of them and the page has answered it.
    /// The keys that edit a field in the page, while one has the focus.
    ///
    /// Answered before the keys that move a selection: an arrow belongs to the
    /// caret while there is a caret, and to the page's selection otherwise.
    fn page_edit_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        use crate::page::EditAction;

        if modifiers.command || modifiers.alt {
            return false;
        }
        // A list that is showing owns the keys that walk it, and the one that puts
        // it away.
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            match key {
                Key::Escape if page.is_open() => return page.close_open(),
                Key::Enter if page.is_open() => return page.accept_open(),
                Key::Up | Key::Down if page.step_selection(key == Key::Down) => return true,
                _ => {}
            }
        }
        // A slider takes the keys that move it before anything else looks at
        // them: an arrow on a focused slider is a step, not a scroll.
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            use crate::page::SliderMotion;
            let motion = match key {
                Key::Left | Key::Down => Some(SliderMotion::Down),
                Key::Right | Key::Up => Some(SliderMotion::Up),
                Key::PageUp => Some(SliderMotion::PageUp),
                Key::PageDown => Some(SliderMotion::PageDown),
                Key::Home => Some(SliderMotion::Start),
                Key::End => Some(SliderMotion::End),
                _ => None,
            };
            if let Some(motion) = motion
                && page.step_value(motion)
            {
                return true;
            }
        }
        let extend = modifiers.shift;
        let action = match key {
            Key::Backspace => EditAction::Backspace,
            Key::Delete => EditAction::Delete,
            Key::Left => EditAction::Left,
            Key::Right => EditAction::Right,
            Key::Home => EditAction::Home,
            Key::End => EditAction::End,
            _ => return false,
        };
        self.tabs[self.active]
            .page
            .as_mut()
            .is_some_and(|page| page.edit_text(action, extend))
    }

    fn page_selection_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        use otlyra_layout::Motion;

        if key == Key::Character('a') && modifiers.command {
            return self.tabs[self.active]
                .page
                .as_mut()
                .is_some_and(PageScene::select_all);
        }

        // Only with shift held. An arrow on a page nobody is editing scrolls it,
        // in every browser and here — turning that into a caret the moment
        // something is selected would take the page's scrolling away for as long
        // as a selection is on screen.
        if !modifiers.shift
            || !self.tabs[self.active]
                .page
                .as_ref()
                .is_some_and(PageScene::has_selection)
        {
            return false;
        }

        // The command key turns a step into a jump: ⇧⌘← reaches the start of the
        // line and ⇧⌘↑ the start of the page.
        let motion = match (key, modifiers.command) {
            (Key::Left, false) => Motion::Back,
            (Key::Right, false) => Motion::Forward,
            (Key::Up, false) => Motion::Up,
            (Key::Down, false) => Motion::Down,
            (Key::Left, true) | (Key::Home, _) => Motion::LineStart,
            (Key::Right, true) | (Key::End, _) => Motion::LineEnd,
            (Key::Up, true) => Motion::Start,
            (Key::Down, true) => Motion::End,
            _ => return false,
        };

        let Some(page) = self.tabs[self.active].page.as_mut() else {
            return false;
        };
        page.move_selection(motion, true);
        true
    }

    /// How much larger than its CSS pixels the page in the active tab is drawn.
    ///
    /// A page zoom is not the device scale and not the reader's text size. The
    /// device scale is how many device pixels go to a CSS pixel and applies to
    /// the whole window, chrome and all. The text size moves the root font and
    /// nothing else, so a page that sizes its cards in pixels does not grow with
    /// it. A zoom makes the CSS pixel itself larger for one page: every length,
    /// border and picture grows, the chrome does not, and the page lays out in
    /// the fewer CSS pixels the window now holds — which is why a zoomed page
    /// reflows rather than being magnified.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Draw the page this much larger, between an eighth and five times.
    ///
    /// The range is every browser's: past those a page is either unreadable or
    /// a wall of one word, and a factor a reader cannot get back from is a
    /// factor they should not be able to reach.
    pub fn set_zoom(&mut self, zoom: f32) {
        let zoom = zoom.clamp(0.25, 5.0);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        // Before the factor changes, because it is the layout being left that
        // knows where the reader was.
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.hold_the_reader_s_place();
        }
        self.zoom = zoom;
        self.ui.zoom = zoom;
        // Remembered against the site rather than the tab or the window: a
        // reader who needs a factor on one site needs it every time they go
        // back, and needs the next site left alone. Its own size is the absence
        // of an entry rather than an entry saying one, so that a browser does
        // not carry a line for every place anyone has ever been.
        if let Some(origin) = self.active_origin() {
            let before = self.settings.settings.clone();
            if (zoom - 1.0).abs() < f32::EPSILON {
                self.settings.settings.zoom.remove(&origin);
            } else {
                self.settings.settings.zoom.insert(origin, zoom);
            }
            self.save_preferences_if_changed(&before);
        }
        // Everything below the zoom is a function of it: the page lays out in a
        // viewport of a different size, so it has to be laid out again.
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.invalidate_layout();
        }
    }

    /// The site the active tab is on, as a zoom is remembered against it.
    ///
    /// Scheme and host, so `http` and `https` are two sites — which they are,
    /// to everything else a browser keeps — and every page of one site is one
    /// site. `None` for a tab showing nothing, one of the browser's own pages,
    /// or an address that is not one.
    fn active_origin(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        if tab.system.is_some() {
            return None;
        }
        let url = otlyra_net::normalize(&tab.url).ok()?;
        let host = url.host_str()?;
        Some(format!("{}://{host}", url.scheme()))
    }

    /// Put the zoom back to whatever this site was left at.
    ///
    /// Called wherever the address is synchronized, because the site is a
    /// property of the address: a tab coming to the front and a navigation are
    /// the same question asked twice.
    fn restore_zoom(&mut self) {
        let wanted = self
            .active_origin()
            .and_then(|origin| self.settings.settings.zoom.get(&origin).copied())
            .unwrap_or(1.0);
        if (wanted - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = wanted;
        self.ui.zoom = wanted;
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.invalidate_layout();
        }
    }

    /// Take the zoom one step along the ladder, or back to where it started.
    ///
    /// A ladder rather than a multiplier, because a reader presses the key until
    /// the page looks right and the stops have to be the ones they recognize —
    /// and because repeated multiplication lands on factors like 121% that no
    /// menu can name. This is the one every browser uses.
    pub fn step_zoom(&mut self, step: ZoomStep) {
        /// The stops, smallest first.
        const LADDER: &[f32] = &[
            0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0,
            5.0,
        ];

        let current = self.zoom;
        let wanted = match step {
            ZoomStep::Reset => 1.0,
            ZoomStep::In => LADDER
                .iter()
                .copied()
                .find(|stop| *stop > current + f32::EPSILON)
                .unwrap_or(current),
            ZoomStep::Out => LADDER
                .iter()
                .copied()
                .rev()
                .find(|stop| *stop < current - f32::EPSILON)
                .unwrap_or(current),
        };
        self.set_zoom(wanted);
    }

    /// A point in the window, in the page's own coordinates.
    ///
    /// A zoomed page is laid out in fewer CSS pixels than the window has logical
    /// ones and drawn back up to fill them, so every question a pointer asks it
    /// has to be asked in its units. One place, because a press answered in a
    /// coordinate system it did not land in is a link that opens when the
    /// pointer was somewhere else.
    fn in_page(&self, x: f64, y: f64) -> (f64, f64) {
        let zoom = f64::from(self.zoom);
        (x / zoom, y / zoom)
    }

    /// The page's top inset, in the page's own coordinates.
    fn page_top(&self) -> f32 {
        ((if self.interface { UI_HEIGHT } else { 0.0 }) / f64::from(self.zoom)) as f32
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

    /// Load `url` into the active tab, as the reader asking for it.
    ///
    /// Nothing waits here: the request goes to the fetch thread and the answer
    /// arrives as an event, because an event loop that waits on the network is a
    /// window that has stopped painting.
    pub fn navigate(&mut self, url: &str) {
        self.navigate_from(url, true);
    }

    /// Show one of the browser's own pages in the active tab.
    ///
    /// Navigation like any other, so it earns a history entry and back reaches
    /// whatever was there before it.
    pub fn open_system(&mut self, page: SystemPage) {
        self.activate_surface(SURFACE_SYSTEM);
        self.navigate(page.url());
    }

    /// Show one of the browser's own pages in a tab of its own.
    ///
    /// A blank tab is used rather than added to: opening the settings from an
    /// empty new tab should fill it, not leave an empty one behind.
    pub fn open_system_in_new_tab(&mut self, page: SystemPage) {
        let blank = {
            let tab = &self.tabs[self.active];
            tab.url.is_empty() && tab.page.is_none() && tab.system.is_none()
        };
        if !blank {
            self.new_tab();
        }
        self.open_system(page);
    }

    /// Which of the browser's own pages the active tab is showing, if any.
    pub fn system_page(&self) -> Option<SystemPage> {
        self.tabs[self.active].system
    }

    /// Go back one entry in the active tab's history.
    ///
    /// The page is loaded again rather than kept: a document costs what it costs
    /// to hold, and a back button that works is worth more than one that is
    /// instant. Where the reader had got to is restored, which is the part they
    /// actually notice.
    pub fn go_back(&mut self) {
        self.travel(-1);
    }

    /// Go forward one entry.
    pub fn go_forward(&mut self) {
        self.travel(1);
    }

    /// Whether the active tab can go back.
    pub fn can_go_back(&self) -> bool {
        self.tabs[self.active].can_go_back()
    }

    /// Whether the active tab can go forward.
    pub fn can_go_forward(&self) -> bool {
        self.tabs[self.active].can_go_forward()
    }

    /// Move `offset` entries through the active tab's history.
    fn travel(&mut self, offset: isize) {
        let tab = &mut self.tabs[self.active];
        let Some(target) = tab.position.checked_add_signed(offset) else {
            return;
        };
        let Some(entry) = tab.history.get(target).cloned() else {
            return;
        };

        self.remember_scroll();
        self.tabs[self.active].position = target;
        // The entry was reached once, so its scheme was allowed once; going back to
        // it is the reader's own request and not the page's.
        self.start_load(&entry.url, true, false, entry.scroll);
    }

    /// Record where the reader is in the entry they are about to leave.
    fn remember_scroll(&mut self) {
        // A browser page keeps its own, on the surface that draws it, so where
        // the number lives depends on what the tab is showing — and the history
        // entry does not care which it was.
        let settings = self.settings.settings.scroll as f32;
        let tab = &mut self.tabs[self.active];
        let scroll = match tab.system {
            Some(SystemPage::Settings) => settings,
            Some(_) => 0.0,
            None => tab.page.as_ref().map_or(0.0, |page| page.scroll()),
        };
        if let Some(entry) = tab.history.get_mut(tab.position) {
            entry.scroll = scroll;
        }
    }

    /// Load the active tab's address again, keeping where the reader had got to.
    ///
    /// Browsers restore the scroll position on reload, and for a page you are
    /// editing that is the whole value of the key: the alternative is finding your
    /// place again after every change.
    pub fn reload(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.url.is_empty() {
            return;
        }

        let url = tab.url.clone();
        let scroll = tab.page.as_ref().map_or(0.0, |page| page.scroll());
        // A reload is the reader saying they think it changed, so the stored copy
        // is not the answer however fresh it still is. It is still worth
        // consulting: the server may say nothing changed, and a reload that costs
        // a header rather than a body is the difference between a page that
        // reappears and a page that loads again.
        self.next_cache_mode = otlyra_net::CacheMode::Revalidate;
        // Reload keeps the entry it is on: a page loaded twice is one place, and
        // going back from it must reach where you were before it, not itself.
        self.start_load(&url, false, false, scroll);
    }

    /// Load the page again without consulting the cache at all.
    ///
    /// What ⌘⇧R means, and the difference from an ordinary reload is the whole
    /// reason there are two: this one does not ask whether anything changed, it
    /// fetches. What comes back is still kept — the point is a new copy, not the
    /// end of having one — and every subresource the page then asks for is
    /// fetched afresh too, which is what makes it the answer to a stylesheet a
    /// server is serving stale.
    pub fn reload_ignoring_cache(&mut self) {
        if let Some(cache) = self.cache.as_ref() {
            cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
        self.next_cache_mode = otlyra_net::CacheMode::Bypass;
        self.reload_keeping_the_mode();
    }

    /// The body of [`Browser::reload`], without setting a mode of its own.
    fn reload_keeping_the_mode(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.url.is_empty() {
            return;
        }
        let url = tab.url.clone();
        let scroll = tab.page.as_ref().map_or(0.0, |page| page.scroll());
        self.start_load(&url, false, false, scroll);
    }

    /// Stop the active navigation and reject every response that still arrives.
    ///
    /// Fetch work may already be inside a platform syscall and finish in the
    /// background. Removing its `PendingLoad` is the cancellation boundary:
    /// `receive` accepts only request ids a live tab still owns, so a late
    /// document, stylesheet, or image cannot replace what the reader kept.
    pub fn stop(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.pending.take().is_some() {
            tracing::info!(url = %tab.url, "navigation stopped");
        }
    }

    /// Load `url` into the active tab.
    ///
    /// `user_initiated` says whether the address came from the person rather than
    /// from the page: it is what decides whether a `file:` URL may be reached at
    /// all, and a page from the internet must never be able to claim it.
    /// Open the dialogue a file picker asked for, and hand the page what came
    /// back.
    ///
    /// The page asked and this answers, which is the whole of the split: what a
    /// reader is shown here is the machine's own dialogue where there is one, and
    /// on a machine with none the request simply goes unanswered and the control
    /// goes on saying that no file was chosen.
    fn answer_file_request(&mut self) {
        let Some(request) = self.tabs[self.active]
            .page
            .as_mut()
            .and_then(PageScene::take_file_request)
        else {
            return;
        };
        let chosen = choose_files(&request);
        if chosen.is_empty() {
            return;
        }
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.set_files(request.node, chosen);
        }
    }

    /// Go wherever a form the reader has just sent points.
    ///
    /// A form that submits is a form that navigates, and that is the whole of it
    /// without a script. An action of nothing at all means the page's own address,
    /// which is what reloads a page with its answers in the query.
    fn follow_submission(&mut self) {
        let Some(sent) = self.tabs[self.active]
            .page
            .as_mut()
            .and_then(PageScene::take_submission)
        else {
            return;
        };
        if sent.method == otlyra_dom::Method::Dialog {
            return;
        }
        // The action is spelled as the markup spells it, so an empty one is the
        // page itself and a relative one is resolved against it.
        let here = self.tabs[self.active].url.clone();
        let target = if sent.url.is_empty() {
            here.clone()
        } else {
            otlyra_net::url::resolve(&here, &sent.url).unwrap_or_else(|| sent.url.clone())
        };
        // A form is the page acting, not the reader, so the same scheme policy that
        // holds for a link holds here: a page from the network may not aim a form
        // at a file.
        if sent.method == otlyra_dom::Method::Post {
            self.remember_scroll();
            self.start_send(
                &target,
                false,
                true,
                0.0,
                Some(Body {
                    content_type: sent.content_type,
                    bytes: sent.body,
                }),
            );
            return;
        }
        self.navigate_from(&target, false);
    }

    /// Carry out what a screen reader asked for on a node the page owns.
    ///
    /// The identifiers the tree hands out for the page are its box ids, so the
    /// node names a box, the box names an element, and the element is pressed or
    /// focused exactly as the pointer would press or focus it — including a link,
    /// which is followed, and a button, which sends its form.
    fn accessibility_request_on_page(
        &mut self,
        node: otlyra_platform::accesskit::NodeId,
        action: otlyra_platform::AccessibilityAction,
    ) {
        let Some(page) = self.tabs[self.active].page.as_mut() else {
            return;
        };
        // A box for nearly everything on the page, and an element for the few
        // things that generated none — an option of a drop-down nobody has opened.
        let box_id = crate::a11y::box_of(node);
        let element = match box_id {
            Some(box_id) => page.boxes().get(box_id).and_then(|found| found.node),
            None => crate::a11y::element_of(node),
        };
        let Some(element) = element else {
            tracing::debug!(?node, "an accessibility request named nothing on the page");
            return;
        };

        let changed = match action {
            otlyra_platform::AccessibilityAction::Focus => page.focus_node(element),
            // A reader asking a slider to move is the same request an arrow key
            // makes, one step further in: the focus goes to the control first, as
            // it would if the reader had reached it, and then it moves.
            otlyra_platform::AccessibilityAction::Increment
            | otlyra_platform::AccessibilityAction::Decrement => {
                let mut changed = page.focus_node(element);
                changed |= page.step_value(
                    if action == otlyra_platform::AccessibilityAction::Increment {
                        crate::page::SliderMotion::Up
                    } else {
                        crate::page::SliderMotion::Down
                    },
                );
                changed
            }
            otlyra_platform::AccessibilityAction::Activate => {
                // A link is followed rather than activated: there is no control
                // behind it, and what pressing one means is a navigation.
                if let Some(href) = box_id.and_then(|box_id| page.href_of(box_id)) {
                    let here = self.tabs[self.active].url.clone();
                    let target =
                        otlyra_net::url::resolve(&here, &href).unwrap_or_else(|| href.clone());
                    self.navigate_from(&target, false);
                    return;
                }
                let changed = page.activate_node(element);
                self.follow_submission();
                self.answer_file_request();
                changed
            }
        };
        // Every event asks for a frame; what `changed` says is only whether
        // anything had to be styled again.
        let _ = changed;
    }

    fn navigate_from(&mut self, url: &str, user_initiated: bool) {
        self.remember_scroll();
        self.start_load(url, user_initiated, true, 0.0);
    }

    /// Ask for `url` and leave the tab waiting for it.
    ///
    /// Nothing here waits: the request goes to the fetch thread and the answer
    /// arrives as an event like any other. `record` says whether reaching it should
    /// become a history entry, and `restore_scroll` where the reader should be put
    /// once it has.
    fn start_load(&mut self, url: &str, user_initiated: bool, record: bool, restore_scroll: f32) {
        self.start_send(url, user_initiated, record, restore_scroll, None);
    }

    /// What the cache is allowed to do about the navigation now being started.
    ///
    /// Set for the one navigation and cleared by it, because a reload is an
    /// instruction about *this* fetch: leaving it on would make every later
    /// click behave like a reload, and a browser that never answers from its
    /// cache is a browser with no cache.
    fn take_cache_mode(&mut self) -> otlyra_net::CacheMode {
        std::mem::take(&mut self.next_cache_mode)
    }

    /// Ask for `url` with a body, and leave the tab waiting for it.
    ///
    /// The same navigation as any other in every respect but the method: the same
    /// scheme policy, the same history entry, the same pending state. What a form
    /// sends is bytes on the request rather than a different way of getting there.
    fn start_send(
        &mut self,
        url: &str,
        user_initiated: bool,
        record: bool,
        restore_scroll: f32,
        body: Option<Body>,
    ) {
        let _span = tracing::info_span!("navigation", url).entered();
        // The document a context menu was asked about is being replaced, and
        // its rows name things in it.
        self.ui.dismiss_context_menu();
        self.context_target = None;

        // A browser's own page is fetched from nothing and parsed from nothing:
        // it is a surface this program draws. Catching it in the one place every
        // navigation passes through — the menu, the address bar, the command
        // line, and a step through the history — is what makes it a place a tab
        // can be rather than a mode the window is in.
        if let Some(page) = SystemPage::from_url(url) {
            self.show_system(page, record, restore_scroll);
            return;
        }
        self.activate_surface(SURFACE_PAGE);
        self.tabs[self.active].system = None;

        if !user_initiated && let Ok(target) = otlyra_net::normalize(url) {
            let from = self.tabs[self.active].url.clone();
            if !otlyra_net::may_navigate(Some(&from), &target) {
                tracing::warn!(%url, %from, "navigation refused by scheme policy");
                let tab = &mut self.tabs[self.active];
                tab.error = Some(format!("Refused to open {url} from {from}"));
                tab.page = None;
                tab.pending = None;
                return;
            }
        }

        let previous_url = self.tabs[self.active].url.clone();
        let cache = self.take_cache_mode();
        let id = self.fetcher.fetch(url, ResourceKind::Document, body, cache);
        self.load_started = std::time::Instant::now();

        let tab = &mut self.tabs[self.active];
        tab.url = url.to_owned();
        tab.error = None;
        tab.title = url.to_owned();
        tab.pending = Some(PendingLoad {
            document: id,
            previous_url,
            record,
            restore_scroll,
            sheets: ExternalSheets::default(),
            images: Images::default(),
            picture_sources: HashMap::new(),
            outstanding: HashMap::new(),
        });
        // Through `sync_address` rather than straight into the field: the address
        // and whether this page is one the reader kept are two things the toolbar
        // draws from the same fact, and setting one without the other is how the
        // star ends up describing the page before this one.
        self.sync_address();
    }

    /// Scroll the page by whatever a key means, if it means one.
    ///
    /// The keys every browser scrolls by, and only when nothing is being typed
    /// into: a space bar that pages down while an address is half-written is the
    /// classic way to lose what was typed.
    fn scroll_by_key(&mut self, key: Key) {
        /// How far an arrow moves, in logical pixels.
        const LINE: f32 = 48.0;
        /// How much of the window a page key keeps, so the reader has an anchor.
        const PAGE_OVERLAP: f32 = 48.0;

        let Some(page) = self.tabs[self.active].page.as_mut() else {
            return;
        };
        if page.editing_text() {
            return;
        }
        let screen = (self.last_height as f32 - UI_HEIGHT as f32 - PAGE_OVERLAP).max(LINE);

        match key {
            Key::Down => page.scroll_by(LINE),
            Key::Up => page.scroll_by(-LINE),
            Key::PageDown | Key::Character(' ') => page.scroll_by(screen),
            Key::PageUp => page.scroll_by(-screen),
            Key::Home => page.set_scroll(0.0),
            Key::End => page.scroll_by(f32::MAX / 4.0),
            _ => {}
        }
    }

    /// How far round the spinner is, or `None` when nothing is loading.
    ///
    /// A function of how long the load has been going rather than of a counter
    /// somewhere: a frame that arrives late then draws where the spinner should be
    /// now, not where the last frame left it.
    fn spinner_phase(&self) -> Option<f32> {
        self.tabs[self.active]
            .loading()
            .then(|| self.load_started.elapsed().as_secs_f32() * 4.0)
    }

    /// Take in everything the fetch thread has finished.
    ///
    /// Called when the loop says it was woken, and by anything with no loop to be
    /// woken by. Returns whether a tab changed, which is whether a frame is worth
    /// drawing.
    pub fn pump(&mut self) -> bool {
        let finished = self.fetcher.poll();
        let mut changed = false;
        for fetched in finished {
            changed |= self.receive(fetched);
        }
        // A fetch that finished is the only moment cookies can have changed, so
        // this is where the file catches up. Cheap when nothing did: the store
        // compares a revision before it writes anything.
        self.cookies.flush();
        for saved in self.downloads_writer.poll() {
            changed = true;
            match saved.result {
                Ok(path) => {
                    tracing::info!(file = %path.display(), "attachment saved");
                    self.downloads
                        .mark_saved(saved.id, path.to_string_lossy().into_owned());
                }
                Err(error) => {
                    tracing::warn!(%error, "attachment could not be saved");
                    self.downloads.mark_save_failed(saved.id, error);
                }
            }
        }
        changed
    }

    /// Paint one frame nobody sees, so that everything a frame *asks for* has been
    /// asked for, and wait for it.
    ///
    /// A background picture is named by a rule, and a rule is computed on the way to
    /// a frame: a window paints again when the picture lands, and a caller with one
    /// frame to get right has to do the first one itself. Only for those callers —
    /// a screenshot, a test — never for the window.
    pub fn prepare_frame(&mut self, viewport: Viewport, timeout: std::time::Duration) {
        let mut discarded = otlyra_gfx::RecordingPainter::new();
        self.paint(&mut discarded, viewport);

        let deadline = std::time::Instant::now() + timeout;
        while !self.background_fetches.is_empty()
            || !self.font_fetches.is_empty()
            || !self.picture_fetches.is_empty()
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("gave up waiting for a background picture or a font");
                return;
            }
            for fetched in self.fetcher.wait(remaining.min(FETCH_POLL)) {
                self.receive(fetched);
            }
        }

        // The font landed after the frame that asked for it: every line was
        // measured in whatever the stack fell back to, so the frame the caller is
        // about to take has to be laid out again.
        self.paint(&mut otlyra_gfx::RecordingPainter::new(), viewport);
    }

    /// Wait for the tab to finish loading, for callers with no event loop.
    ///
    /// The window never calls this — it is woken instead. A screenshot and a test
    /// have nowhere to be woken from, and waiting is what they mean by "load".
    pub fn wait_for_load(&mut self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.tabs.iter().any(|tab| tab.loading()) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("gave up waiting for a load");
                return;
            }
            for fetched in self.fetcher.wait(remaining.min(FETCH_POLL)) {
                self.receive(fetched);
            }
        }
    }

    /// Ask for the background pictures the pages have found they need.
    ///
    /// A background is named by a rule, so what a page wants is known only once it
    /// has been styled — which happens on the way to a frame. This is called after
    /// one, and the pictures arrive for the frame after that.
    fn fetch_backgrounds(&mut self) {
        if !self.settings.settings.load_images {
            return;
        }
        for index in 0..self.tabs.len() {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            let base = self.tabs[index].url.clone();
            let wanted: Vec<String> = page
                .wanted_pictures()
                .into_iter()
                .filter(|url| !self.background_requests.contains_key(url))
                .take(IMAGE_LIMIT)
                .collect();

            for url in wanted {
                if let Some(picture) = self.images.get(&url) {
                    if let Some(page) = self.tabs[index].page.as_mut() {
                        page.set_picture(url.clone(), picture);
                    }
                    self.background_requests.insert(url, index);
                    continue;
                }
                let Some(target) = Self::subresource_url(&base, &url) else {
                    // Recorded anyway: a picture that may not be fetched must not be
                    // asked for again on every frame.
                    self.background_requests.insert(url, index);
                    continue;
                };
                let id = self.fetcher.request(&target, ResourceKind::Image);
                self.background_requests.insert(url.clone(), index);
                self.background_fetches.insert(id, (index, url));
            }
        }
    }

    /// Ask each element again which of the pictures it offers this window wants.
    ///
    /// A page chooses among the files a `srcset` lists against the window it is
    /// loading into, and a window is widened, narrowed and dragged between screens
    /// of different densities. So the question is put again whenever the window is
    /// not the one the pictures on screen were chosen against — and only then,
    /// because asking walks every document.
    ///
    /// Only elements whose picture has already arrived: one that never loaded is
    /// the load's business, and re-asking for it here would fetch it a second time.
    fn rechoose_pictures(&mut self) {
        if !self.settings.settings.load_images {
            return;
        }
        let viewport = self.picture_viewport();
        let window = (viewport.width, viewport.scale);
        if self.picture_window == Some(window) {
            return;
        }
        self.picture_window = Some(window);

        for index in 0..self.tabs.len() {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            let base = self.tabs[index].url.clone();
            let changed: Vec<otlyra_layout::ImageSource> =
                otlyra_layout::image_sources(page.document(), viewport)
                    .into_iter()
                    .take(IMAGE_LIMIT)
                    .filter(|source| {
                        page.picture_source(source.node)
                            .is_some_and(|(src, density)| {
                                src != source.src || density != source.density
                            })
                    })
                    .collect();

            for source in changed {
                let Some(target) = Self::subresource_url(&base, &source.src) else {
                    continue;
                };
                // Already decoded: no request, straight into the page.
                if let Some(data) = self.images.get(&target)
                    && let Some(page) = self.tabs[index].page.as_mut()
                {
                    page.set_image(
                        source.node,
                        source.src,
                        otlyra_layout::Picture {
                            data,
                            density: source.density,
                        },
                    );
                    continue;
                }
                let id = self.fetcher.request(&target, ResourceKind::Image);
                self.picture_fetches
                    .insert(id, (index, source.node, source.src, source.density));
            }
        }
    }

    /// Ask for the fonts the pages' own stylesheets bring with them.
    ///
    /// A `@font-face` rule is only known once the sheet holding it has been parsed,
    /// which is a page's first restyle — so this is asked after a frame rather than
    /// with the pictures the markup names, exactly as a background picture is.
    ///
    /// The address is resolved against the sheet the rule was written in, not
    /// against the page: a sheet in a directory of its own names its fonts beside
    /// itself.
    fn fetch_fonts(&mut self) {
        for index in 0..self.tabs.len() {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            let base = self.tabs[index].url.clone();
            let sheets: HashMap<otlyra_dom::NodeId, String> =
                otlyra_css::cascade::stylesheet_links(page.document())
                    .into_iter()
                    .filter_map(|link| Some((link.node, Self::subresource_url(&base, &link.href)?)))
                    .collect();

            for face in page.wanted_fonts().into_iter().take(FONT_LIMIT) {
                // The first address that resolves, which is as far as the order in
                // the rule is honoured: what the rest of the list is for is formats
                // this cannot read, and there is no telling which those are until
                // the bytes are here.
                let sheet_base = face
                    .sheet
                    .and_then(|node| sheets.get(&node))
                    .unwrap_or(&base);
                let Some(target) = face
                    .sources
                    .iter()
                    .find_map(|source| Self::subresource_url(sheet_base, source))
                else {
                    continue;
                };
                if !self
                    .font_requests
                    .insert((face.family.clone(), target.clone()))
                {
                    continue;
                }
                let id = self.fetcher.request(&target, ResourceKind::Stylesheet);
                self.font_fetches.insert(id, face.family);
            }
        }
    }

    /// Put what is selected on the page on the clipboard.
    ///
    /// Returns whether there was anything to copy, which is what decides whether
    /// the key belonged to the page or to whatever else wanted it.
    fn copy_selection(&mut self) -> bool {
        let Some(text) = self.tabs[self.active]
            .page
            .as_ref()
            .and_then(PageScene::selected_text)
        else {
            return false;
        };
        tracing::debug!(characters = text.len(), "copied the selection");
        self.clipboard.write(text);
        true
    }

    /// One finished fetch. Returns whether it changed anything on screen.
    fn receive(&mut self, fetched: Fetched) -> bool {
        // A font belongs to the shaper rather than to a page: once it is in, every
        // page that names the family is set in it.
        if let Some(family) = self.font_fetches.remove(&fetched.id) {
            let Ok(loaded) = fetched.result else {
                tracing::warn!(%family, url = %fetched.url, "font failed to load");
                return false;
            };
            if !self.text.add_font(&family, loaded.bytes) {
                tracing::warn!(%family, url = %fetched.url, "font failed to register");
                return false;
            }
            tracing::debug!(%family, url = %fetched.url, "font registered");
            for tab in &mut self.tabs {
                if let Some(page) = tab.page.as_mut() {
                    page.font_arrived();
                }
            }
            return true;
        }

        // A background picture belongs to a page rather than to a load, and may
        // arrive long after the page it is for.
        if let Some((index, url)) = self.background_fetches.remove(&fetched.id) {
            let Ok(loaded) = fetched.result else {
                tracing::warn!(%url, "background picture failed to load");
                return false;
            };
            match otlyra_gfx::decode_image(&loaded.bytes) {
                Ok(picture) => {
                    self.images.insert(fetched.url.clone(), picture.clone());
                    match self.tabs.get_mut(index).and_then(|tab| tab.page.as_mut()) {
                        Some(page) => page.set_picture(url, picture),
                        None => tracing::warn!(%url, "no page to give the picture to"),
                    }
                    return true;
                }
                Err(error) => {
                    tracing::warn!(%url, %error, "background picture failed to decode");
                    return false;
                }
            }
        }

        // A picture an element chose again after the page was built: the same
        // element, a different file.
        if let Some((index, node, src, density)) = self.picture_fetches.remove(&fetched.id) {
            let Ok(loaded) = fetched.result else {
                tracing::warn!(%src, "re-chosen picture failed to load");
                return false;
            };
            match otlyra_gfx::decode_image(&loaded.bytes) {
                Ok(data) => {
                    self.images.insert(fetched.url.clone(), data.clone());
                    match self.tabs.get_mut(index).and_then(|tab| tab.page.as_mut()) {
                        Some(page) => {
                            page.set_image(node, src, otlyra_layout::Picture { data, density })
                        }
                        None => tracing::warn!(%src, "no page to give the picture to"),
                    }
                    return true;
                }
                Err(error) => {
                    tracing::warn!(%src, %error, "re-chosen picture failed to decode");
                    return false;
                }
            }
        }

        let Some(index) = self.tabs.iter().position(|tab| {
            tab.pending.as_ref().is_some_and(|pending| {
                pending.document == fetched.id || pending.outstanding.contains_key(&fetched.id)
            })
        }) else {
            // A load nobody is waiting for any more: the tab moved on, or closed.
            return false;
        };

        match fetched.kind {
            ResourceKind::Document => self.receive_document(index, fetched),
            ResourceKind::Stylesheet | ResourceKind::Image => {
                self.receive_subresource(index, fetched);
                true
            }
        }
    }

    /// The page itself arrived.
    ///
    /// The document is shown straight away, before its stylesheets and pictures
    /// have been asked for: a page that is readable now and styled a moment later
    /// beats a blank window for the length of the slowest thing it links to.
    fn receive_document(&mut self, index: usize, fetched: Fetched) -> bool {
        let interface = self.interface;
        let loaded = match fetched.result {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, "navigation failed");
                let tab = &mut self.tabs[index];
                tab.title = "Failed".to_owned();
                tab.page = None;
                tab.error = Some(error);
                tab.pending = None;
                return true;
            }
        };

        // An attachment is a completed download, not a document with unusual
        // bytes. Keep the payload in the browser-owned store and show the page
        // that names the result instead of sending it through MIME sniffing and
        // the HTML parser.
        if let Some(filename) =
            downloads::attachment_filename(&loaded.response_headers, &loaded.final_url)
        {
            let (record, previous_url) = self.tabs[index]
                .pending
                .as_ref()
                .map_or((false, String::new()), |pending| {
                    (pending.record, pending.previous_url.clone())
                });
            let final_url = loaded.final_url;
            let size = loaded.bytes.len();
            let recorded = self.downloads.record(
                filename.clone(),
                final_url.clone(),
                loaded.content_type,
                loaded.bytes,
            );

            // The preference decides here rather than on the page: an automatic
            // download is one nobody pressed anything for, so the write has to
            // start where the bytes arrive.
            if let Some(id) = recorded
                && !self.settings.settings.asks_where_to_save()
                && let Some(directory) = self.settings.settings.download_directory()
                && let Some(bytes) = self.downloads.get(id).map(downloads::Download::payload)
            {
                self.start_save(
                    id,
                    downloads::Destination::Into {
                        directory,
                        filename: filename.clone(),
                    },
                    bytes,
                );
            }

            let tab = &mut self.tabs[index];
            tab.system = Some(SystemPage::Downloads);
            tab.url = SystemPage::Downloads.url().to_owned();
            tab.title = SystemPage::Downloads.label().to_owned();
            tab.error = None;
            tab.page = None;
            tab.pending = None;
            if record {
                self.record_history(index, &previous_url);
            }
            if index == self.active {
                self.sync_address();
                self.activate_surface(SURFACE_SYSTEM);
            }
            tracing::info!(%filename, %final_url, size, "attachment downloaded");
            return true;
        }

        // What the response is, from what the server said and from the bytes: a
        // picture is shown as one and text is shown as text, rather than everything
        // being fed to the HTML parser and rendering as whatever that makes of it.
        let sniffed = otlyra_net::sniff(
            loaded.content_type.as_deref(),
            loaded.nosniff,
            &loaded.bytes,
        );
        let final_url = loaded.final_url;
        tracing::debug!(kind = sniffed.essence(), url = %final_url, "response sniffed");
        let parsed = match &sniffed {
            kind if kind.is_document() => {
                otlyra_html::parse(&loaded.bytes, loaded.charset.as_deref())
            }
            otlyra_net::Sniffed::Image(_) => {
                otlyra_html::parse(image_document(&final_url).as_bytes(), Some("utf-8"))
            }
            _ => {
                let text = decode_text(&loaded.bytes, loaded.charset.as_deref());
                otlyra_html::parse(text_document(&text).as_bytes(), Some("utf-8"))
            }
        };

        // What the page asks for, decided here and fetched on the other thread.
        let mut outstanding: HashMap<u64, Vec<PendingResource>> = HashMap::new();
        // Pictures that were decoded for an earlier page and are still here.
        let mut ready = Images::default();
        let links = otlyra_css::cascade::stylesheet_links(&parsed.document);
        self.request_subresources(
            &mut outstanding,
            &final_url,
            links.iter().take(STYLESHEET_LIMIT).map(|link| {
                (
                    link.href.clone(),
                    PendingResource::Stylesheet(link.node),
                    ResourceKind::Stylesheet,
                )
            }),
        );
        // Which of the pictures an element offers is a question about the
        // window: how wide it is and how many device pixels it has to a CSS
        // pixel. Asked here, before the fetch, because a browser fetches the
        // one it chose rather than all of them.
        let pictures = otlyra_layout::image_sources(&parsed.document, self.picture_viewport());
        let wanted: Vec<_> = pictures
            .iter()
            .take(IMAGE_LIMIT)
            .filter(|source| {
                // Already decoded: no request, no decode, straight into the page.
                let Some(url) = Self::subresource_url(&final_url, &source.src) else {
                    return true;
                };
                match self.images.get(&url) {
                    Some(image) => {
                        ready.insert(
                            source.node,
                            otlyra_layout::Picture {
                                data: image,
                                density: source.density,
                            },
                        );
                        false
                    }
                    None => true,
                }
            })
            .map(|source| {
                (
                    source.src.clone(),
                    PendingResource::Image(source.node, source.src.clone(), source.density),
                    ResourceKind::Image,
                )
            })
            .collect();
        self.request_subresources(&mut outstanding, &final_url, wanted.into_iter());
        report_limit(links.len(), STYLESHEET_LIMIT, "stylesheets");
        report_limit(pictures.len(), IMAGE_LIMIT, "pictures");

        let tab = &mut self.tabs[index];
        tab.title = title_of(&parsed.document).unwrap_or_else(|| final_url.clone());
        tab.url = final_url.clone();
        tab.page = Some(PageScene::new(parsed.document));
        if !interface && let Some(page) = tab.page.as_mut() {
            page.hide_scrollbars();
        }
        if index == self.active {
            self.sync_address();
        }

        let Some(pending) = self.tabs[index].pending.as_mut() else {
            return true;
        };
        pending.outstanding = outstanding;
        pending.images.extend(ready);
        let record = pending.record;
        let previous = pending.previous_url.clone();

        if record {
            self.record_history(index, &previous);
        }
        if self.tabs[index]
            .pending
            .as_ref()
            .is_some_and(|pending| pending.outstanding.is_empty())
        {
            self.finish_load(index);
        }
        true
    }

    /// Ask for a page's subresources, recording which nodes each answer feeds.
    fn request_subresources(
        &mut self,
        outstanding: &mut HashMap<u64, Vec<PendingResource>>,
        base: &str,
        wanted: impl Iterator<Item = (String, PendingResource, ResourceKind)>,
    ) {
        // One request per address: a page that names the same picture in ten places
        // is asking for it once.
        let mut asked: HashMap<String, u64> = HashMap::new();
        for (href, resource, kind) in wanted {
            // A preference the browser reads where the behaviour lives. Refusing
            // here rather than dropping the bytes later is what makes it mean
            // anything: a picture that is fetched and then not shown has already
            // cost the reader their bandwidth and told the server they were here.
            if kind == ResourceKind::Image && !self.settings.settings.load_images {
                continue;
            }
            let Some(url) = Self::subresource_url(base, &href) else {
                continue;
            };
            let id = *asked
                .entry(url.clone())
                .or_insert_with(|| self.fetcher.request(&url, kind));
            outstanding.entry(id).or_default().push(resource);
        }
    }

    /// A stylesheet or a picture arrived.
    fn receive_subresource(&mut self, index: usize, fetched: Fetched) {
        let Some(pending) = self.tabs[index].pending.as_mut() else {
            return;
        };
        let Some(wanted) = pending.outstanding.remove(&fetched.id) else {
            return;
        };

        match fetched.result {
            Ok(loaded) => {
                // Decoded once, however many elements asked for it.
                let decoded = wanted
                    .iter()
                    .any(|resource| matches!(resource, PendingResource::Image(..)))
                    .then(|| {
                        otlyra_gfx::decode_image(&loaded.bytes)
                            .inspect_err(
                                |error| tracing::warn!(url = %fetched.url, %error, "image failed to decode"),
                            )
                            .ok()
                    })
                    .flatten();

                if let Some(image) = decoded.clone() {
                    self.images.insert(fetched.url.clone(), image);
                }

                for resource in wanted {
                    match resource {
                        PendingResource::Stylesheet(node) => {
                            let source = decode_text(&loaded.bytes, loaded.charset.as_deref());
                            pending.sheets.insert(node, source);
                        }
                        PendingResource::Image(node, src, density) => match decoded.as_ref() {
                            Some(image) => {
                                pending.images.insert(
                                    node,
                                    otlyra_layout::Picture {
                                        data: image.clone(),
                                        density,
                                    },
                                );
                                pending.picture_sources.insert(node, (src, density));
                            }
                            None => {
                                tracing::warn!(url = %fetched.url, "image failed to decode");
                            }
                        },
                    }
                }
            }
            Err(error) => {
                tracing::warn!(url = %fetched.url, %error, "subresource failed to load");
            }
        }

        if pending.outstanding.is_empty() {
            self.finish_load(index);
        }
    }

    /// Whether a tab is still waiting for a stylesheet it cannot be drawn without.
    ///
    /// A `<link rel=stylesheet>` in the head is render-blocking, and that is not
    /// a detail: a document painted before its stylesheet arrives is painted in
    /// the wrong fonts, at the wrong widths, in the wrong colours, and then
    /// jumps. Every browser holds the frame instead, and what a reader sees on a
    /// slow load is the last page or nothing — never the author's markup with
    /// none of the author's design on it.
    ///
    /// Pictures are not on this list. They are not render-blocking anywhere, and
    /// a page held back for a photograph is a page nobody can start reading.
    fn blocked_on_style(&self, index: usize) -> bool {
        let Some(pending) = self.tabs[index].pending.as_ref() else {
            return false;
        };
        let document = self.tabs[index].page.as_ref().map(PageScene::document);
        let viewport = self.picture_viewport();
        pending
            .outstanding
            .values()
            .flatten()
            .any(|resource| match resource {
                PendingResource::Stylesheet(node) => {
                    // A sheet written for another medium blocks nothing: a
                    // print-only one holds the screen for a page it will never
                    // style. What it says it is for is asked of the same matcher
                    // the cascade uses, so the two cannot come to disagree.
                    document.is_none_or(|document| {
                        crate::media_of_link(document, *node).is_none_or(|media| {
                            otlyra_css::cascade::media_condition_matches(&media, viewport)
                        })
                    })
                }
                PendingResource::Image(..) => false,
            })
    }

    /// Everything the page asked for has arrived or failed: build it for real.
    fn finish_load(&mut self, index: usize) {
        // A new page names its own backgrounds; what the last one asked for is not
        // an answer for this one.
        self.background_requests.clear();

        let Some(pending) = self.tabs[index].pending.take() else {
            return;
        };
        let scroll = pending.restore_scroll;
        let tab = &mut self.tabs[index];

        // The document is already on screen, unstyled; rebuilding it with what
        // arrived is what turns it into the page the author wrote.
        if (!pending.sheets.is_empty() || !pending.images.is_empty())
            && let Some(page) = tab.page.take()
        {
            tab.page = Some(PageScene::with_resources(
                page.into_document(),
                pending.sheets,
                pending.images,
                pending.picture_sources,
            ));
        }
        if let Some(page) = tab.page.as_mut() {
            page.set_scroll(scroll);
        }
    }

    /// Put one of the browser's own pages in the active tab.
    ///
    /// Everything a finished load does, minus the loading: the tab's address and
    /// title change, whatever was there is dropped, and the arrival earns a
    /// history entry if this navigation was the kind that earns one.
    fn show_system(&mut self, page: SystemPage, record: bool, restore_scroll: f32) {
        if !page.available() {
            let tab = &mut self.tabs[self.active];
            tab.error = Some(format!("{} is not built yet.", page.label()));
            tracing::info!(?page, "system page requested before it exists");
            return;
        }

        let index = self.active;
        let previous_url = self.tabs[index].url.clone();
        let tab = &mut self.tabs[index];
        tab.system = Some(page);
        tab.url = page.url().to_owned();
        tab.title = page.label().to_owned();
        tab.error = None;
        tab.page = None;
        tab.pending = None;

        if record {
            self.record_history(index, &previous_url);
        }
        // A browser page is scrolled like any other, so coming back to one lands
        // where the reader left it. Set rather than added, because the surface is
        // the browser's and the last tab to use it left its own position there.
        self.settings.settings.scroll = f64::from(restore_scroll);
        self.sync_address();
        self.activate_surface(SURFACE_SYSTEM);
    }

    /// Add the entry this load earned, if it earned one.
    fn record_history(&mut self, index: usize, previous_url: &str) {
        let tab = &mut self.tabs[index];

        // A load that did not move is not a second place: reloading a page, or
        // typing the address it is already on, adds nothing.
        if tab.url == previous_url && !tab.history.is_empty() {
            return;
        }

        // Going somewhere new after going back drops what was ahead: the forward
        // entries describe a future that did not happen.
        if !tab.history.is_empty() {
            tab.position += 1;
            tab.history.truncate(tab.position);
        }
        tab.history.push(HistoryEntry {
            url: tab.url.clone(),
            scroll: 0.0,
        });
        tab.position = tab.history.len() - 1;

        // The browser-wide record, beside the tab's own: same seam, so it is
        // once per navigation by construction — a redirect chain arrived here
        // as one final URL, and a reload returned before this line.
        let (url, title) = (tab.url.clone(), tab.title.clone());
        self.history.record(url, title, jiff::Timestamp::now());
    }

    /// The address a subresource is actually fetched from, or `None` if the page
    /// may not reach it.
    ///
    /// A document fetched over the network may not reach a `file:` URL, the same
    /// rule that governs where it may navigate: a subresource is a request the page
    /// chose to make, and a page from the internet reading the disk is the failure
    /// that rule exists to prevent.
    fn subresource_url(base: &str, href: &str) -> Option<String> {
        let url = otlyra_net::resolve(base, href)?;
        if let Ok(target) = otlyra_net::normalize(&url)
            && !otlyra_net::may_navigate(Some(base), &target)
        {
            tracing::warn!(%url, %base, "subresource refused by scheme policy");
            return None;
        }
        Some(url)
    }

    /// Open a tab and make it active.
    pub fn new_tab(&mut self) {
        self.tabs.push(Tab::blank());
        self.active = self.tabs.len() - 1;
        self.ui.address.clear();
        self.ui.focus_address();
        self.activate_surface(SURFACE_CHROME);
    }

    /// Open a tab and say what it is called, without making it active.
    ///
    /// What a driver asks for: it creates a context and then sends commands to
    /// it by name, and whether the person watching is looking at it is a
    /// separate question with its own command.
    pub fn open_tab(&mut self) -> TabId {
        self.tabs.push(Tab::blank());
        self.tabs[self.tabs.len() - 1].id
    }

    /// Where a tab named `id` sits right now, if it is still open.
    pub fn tab_index(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// What the active tab is called.
    pub fn active_id(&self) -> TabId {
        self.tabs[self.active].id
    }

    /// Close a tab. The last one is never closed; it is emptied instead, because a
    /// window with no tabs has nothing to show and nothing to type into.
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            self.tabs[0] = Tab::blank();
            self.ui.address.clear();
            self.ui.focus_address();
            self.activate_surface(SURFACE_CHROME);
            return;
        }
        self.tabs.remove(index);
        self.active = self.active.min(self.tabs.len() - 1);
        self.sync_address();
        let surface = self.tab_surface();
        self.activate_surface(surface);
    }

    /// Put the tab named `id` at `to`, taking the tabs after it along.
    ///
    /// The strip's order is the browser's, so a drag reports the move rather
    /// than keeping an order of its own to apply on release: dropping is then
    /// letting go, and there is no second answer to what the order is if the
    /// drag is interrupted by anything at all. Which tab is being read moves
    /// with it — a tab dragged somewhere else is still the tab you were on.
    pub fn move_tab(&mut self, id: TabId, to: usize) {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let to = to.min(self.tabs.len().saturating_sub(1));
        if from == to {
            return;
        }
        let active = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .unwrap_or(self.active.min(self.tabs.len() - 1));
    }

    /// Make a tab active.
    pub fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.sync_address();
            let surface = self.tab_surface();
            self.activate_surface(surface);
        }
    }

    /// Put the active tab's URL back in the address bar.
    fn sync_address(&mut self) {
        let url = self.tabs[self.active].url.clone();
        // Whether this page is kept is a property of the address, so it is answered
        // wherever the address is: one place, and neither the star nor the menu can
        // end up offering to keep a page that is already kept.
        self.ui.bookmark = if url.trim().is_empty() {
            crate::ui::Bookmarked::Impossible
        } else if self.bookmarks.contains(&url) {
            crate::ui::Bookmarked::Yes
        } else {
            crate::ui::Bookmarked::No
        };
        self.ui.address.set_text(url);
        self.restore_zoom();
        self.sync_find();
    }

    /// Leave the settings when the surface says it is done with them.
    ///
    /// *Done* is *back*, now that the settings are a history entry like any
    /// other: it returns to whatever the tab was showing before, at the scroll
    /// position it was left at. With nothing behind it — the settings opened in
    /// a fresh tab — the tab is emptied instead, because there is nowhere to
    /// return to and staying would make the button do nothing.
    /// Where the home button and a new window go.
    pub fn home(&self) -> String {
        self.settings.settings.home.text().to_owned()
    }

    /// What the preferences say should happen when the browser opens.
    pub fn settings_on_start(&self) -> settings::OnStart {
        self.settings.settings.on_start
    }

    /// Go where the preferences say home is.
    pub fn go_home(&mut self) {
        let home = self.home();
        if home.trim().is_empty() {
            return;
        }
        self.navigate(&home);
    }

    /// Show or hide developer tools and move keyboard ownership with the panel.
    fn toggle_inspector(&mut self) {
        self.inspector.toggle();
        let surface = if self.inspector.open {
            SURFACE_INSPECTOR
        } else {
            self.tab_surface()
        };
        self.activate_surface(surface);
    }

    /// Save the preferences if the surface has changed one.
    ///
    /// Compared rather than announced, because every change already goes through
    /// one place — `Settings::apply` — and a second signal saying *and this one
    /// was worth saving* would be a second thing to keep in step with the first.
    fn save_preferences_if_changed(&mut self, before: &settings::Settings) {
        if self.settings.settings.persisted_eq(before) {
            return;
        }
        // The appearance is a preference like the rest, so the one place that
        // notices a preference changing is the one place the palette follows it.
        self.apply_theme();
        crate::preferences::save(&self.settings.settings);
    }

    /// Answer what the settings surface reported and could not do itself.
    ///
    /// Two things: leaving the surface, which is a tab's business, and putting a
    /// native dialogue on the screen, which is the platform's. Everything else the
    /// surface has already applied to the preferences by the time this runs.
    fn handle_settings_action(&mut self, action: &settings::Action) {
        // Unconditional, and before the match: the surface has already written
        // every change into the preferences, and the jar is the one piece of
        // state a preference reaches that is not read back out of them on use.
        // Doing it here rather than under the one action that needs it is what
        // stops the next such preference from being forgotten.
        self.sync_cookie_policy();
        match action {
            settings::Action::Close => self.close_system_page(),
            settings::Action::ChooseDownloadDirectory => {
                let current = self.settings.settings.download_directory();
                let Some(chosen) = choose_download_directory(current.as_deref()) else {
                    return;
                };
                // Through `apply`, like every other change, and saved here because
                // the caller's own before/after comparison already ran — the
                // dialogue was up while it did.
                self.settings
                    .settings
                    .apply(settings::Action::SetDownloadDirectory(
                        chosen.to_string_lossy().into_owned(),
                    ));
                crate::preferences::save(&self.settings.settings);
            }
            _ => {}
        }
    }

    /// Leave the browser page being shown: back if there is a back, a blank
    /// tab if there is not.
    fn close_system_page(&mut self) {
        if self.tabs[self.active].can_go_back() {
            self.go_back();
            return;
        }
        let tab = &mut self.tabs[self.active];
        tab.system = None;
        tab.url = String::new();
        tab.title = "New tab".to_owned();
        self.ui.address.clear();
        self.ui.bookmark = crate::ui::Bookmarked::Impossible;
        self.ui.focus_address();
        self.activate_surface(SURFACE_CHROME);
    }

    /// Act on what the history surface reported.
    fn handle_history_action(&mut self, action: crate::history::Action) {
        match action {
            crate::history::Action::Open(url) => self.navigate_from(&url, false),
            crate::history::Action::Clear => self.history.clear(),
            crate::history::Action::Close => self.close_system_page(),
            crate::history::Action::None
            | crate::history::Action::Focus(_)
            | crate::history::Action::SearchHit(_) => {}
        }
    }

    /// Keep the page the reader is on, or stop keeping it.
    ///
    /// One command for both, because ⌘D is one key and a reader pressing it twice
    /// means *undo that*. What it did is reported in the log rather than on the
    /// page: the native menu bar is built once at startup and cannot yet be
    /// relabelled, so the browser's own menu — rebuilt every frame — is where the
    /// state is said out loud.
    fn toggle_bookmark(&mut self) {
        let tab = &self.tabs[self.active];
        let url = tab.url.clone();
        if url.trim().is_empty() {
            // A blank tab is not a page, and a bookmark that opens nowhere is
            // worse than no bookmark.
            return;
        }
        let title = if tab.title.trim().is_empty() {
            url.clone()
        } else {
            tab.title.clone()
        };
        let kept = self.bookmarks.toggle(url.clone(), title);
        tracing::info!(%url, kept, "bookmark toggled");
        self.ui.bookmark = if kept {
            crate::ui::Bookmarked::Yes
        } else {
            crate::ui::Bookmarked::No
        };
        self.accessibility_dirty = true;
    }

    /// Keep bookmarks between runs, reading what the last one left.
    ///
    /// Called by the shell for a browser a person is using, beside the system
    /// clipboard and for the same reason: a window means a person, and a person
    /// expects what they kept to still be there tomorrow. Every headless mode — a
    /// screenshot, an automation session, a test — keeps the in-memory store, so
    /// none of them can read or overwrite that person's bookmarks.
    pub fn persist_bookmarks(&mut self) {
        self.bookmarks = crate::bookmarks::BookmarkStore::persisted();
        self.sync_address();
    }

    /// Keep the cookies the last run left, and write every change from now on.
    ///
    /// The same rule as the bookmarks and for a sharper reason: the file is
    /// somebody's signed-in sessions, so every headless mode — a screenshot, an
    /// automation session, a test — keeps the in-memory jar and none of them can
    /// read or overwrite it.
    ///
    /// The jar handed to the loader is unchanged by this; only what is in it and
    /// where it is written are.
    pub fn persist_cookies(&mut self) {
        self.cookies.persist();
    }

    /// The jar, to hand to a loader before the browser is built.
    pub fn cookies(&self) -> &crate::cookies::CookieStore {
        &self.cookies
    }

    /// The jar, to list and to empty.
    pub fn cookies_mut(&mut self) -> &mut crate::cookies::CookieStore {
        &mut self.cookies
    }

    /// Use `cache` as this browser's HTTP cache.
    ///
    /// The same shape as the jar, and for the same reason: the loader is built
    /// before the browser and both have to hold the one cache.
    pub fn set_cache(&mut self, cache: otlyra_net::SharedCache) {
        self.cache = Some(cache);
    }

    /// What has already been fetched, for a surface that lists it or empties it.
    pub fn cache(&self) -> Option<&otlyra_net::SharedCache> {
        self.cache.as_ref()
    }

    /// Use `store` as this browser's jar.
    ///
    /// For a shell that made the jar first because the loader needed it — which is
    /// the ordinary case, since a loader is built before the browser that holds it.
    pub fn set_cookie_store(&mut self, store: crate::cookies::CookieStore) {
        self.cookies = store;
        self.sync_cookie_policy();
    }

    /// Tell the jar what the reader's switch says.
    ///
    /// The jar holds the answer rather than the loader, because the loader is
    /// built once and the switch moves — and because the surfaces have to be able
    /// to show what is in force.
    fn sync_cookie_policy(&mut self) {
        let accepts = !self.settings.settings.block_third_party_cookies;
        self.cookies
            .with(|jar| jar.set_accepts_third_party(accepts));
    }

    /// Whether the page the reader is on is one they kept.
    pub fn is_bookmarked(&self) -> bool {
        self.bookmarks.contains(&self.tabs[self.active].url)
    }

    /// Act on what the bookmarks surface reported.
    fn handle_bookmarks_action(&mut self, action: crate::bookmarks::Action) {
        match action {
            crate::bookmarks::Action::Open(url) => self.navigate_from(&url, false),
            crate::bookmarks::Action::Remove(url) => {
                self.bookmarks.remove(&url);
            }
            crate::bookmarks::Action::Clear => self.bookmarks.clear(),
            crate::bookmarks::Action::Close => self.close_system_page(),
            crate::bookmarks::Action::None => {}
        }
    }

    /// Act on what the cookies surface reported.
    fn handle_cookies_action(&mut self, action: crate::cookies::Action) {
        match action {
            crate::cookies::Action::ClearSite(site) => {
                self.cookies.with(|jar| jar.clear_site(&site));
                // Immediately, not at the next fetch: a person who asked to be rid
                // of something should not have it on the disk while they read the
                // page that says it is gone.
                self.cookies.flush();
            }
            crate::cookies::Action::Clear => {
                self.cookies.with(otlyra_net::cookie::Jar::clear);
                self.cookies.flush();
            }
            crate::cookies::Action::Close => self.close_system_page(),
            crate::cookies::Action::None => {}
        }
    }

    /// Act on what the downloads surface reported.
    fn handle_downloads_action(&mut self, action: downloads::Action) {
        match action {
            downloads::Action::Clear => self.downloads.clear(),
            downloads::Action::Save(id) => {
                let asked = self.downloads.get(id).and_then(|download| {
                    // The dialogue opens in the download folder, so the preference
                    // is where a manual save starts from as well as where an
                    // automatic one ends up.
                    choose_download_path(
                        download.filename(),
                        self.settings.settings.download_directory().as_deref(),
                    )
                    .map(|path| (path, download.payload()))
                });
                if let Some((path, bytes)) = asked {
                    self.start_save(id, downloads::Destination::Exact(path), bytes);
                }
            }
            downloads::Action::Retry(id) => {
                // The directory is read again rather than remembered: the usual
                // reason a write failed is that where it was going was wrong, and
                // the usual answer is that the reader has since changed it.
                let Some(directory) = self.settings.settings.download_directory() else {
                    return;
                };
                let retry = self
                    .downloads
                    .get(id)
                    .map(|download| (download.retry_destination(directory), download.payload()));
                if let Some((destination, bytes)) = retry {
                    self.start_save(id, destination, bytes);
                }
            }
            downloads::Action::Close => self.close_system_page(),
            downloads::Action::None => {}
        }
    }

    /// Mark a row as being written and hand the bytes to the writer.
    ///
    /// One place, because the row's in-progress state and the write starting must
    /// not be able to disagree: a row that says nothing is happening while a file
    /// is being written offers a second Save As over the top of the first.
    fn start_save(
        &mut self,
        id: downloads::DownloadId,
        destination: downloads::Destination,
        bytes: std::sync::Arc<[u8]>,
    ) {
        match &destination {
            downloads::Destination::Exact(path) => self.downloads.mark_saving(
                id,
                path.to_string_lossy().into_owned(),
                Some(path.clone()),
            ),
            // No path yet: which name a directory write lands on is the writer's
            // answer, and the row says the folder until it comes back.
            downloads::Destination::Into { directory, .. } => {
                self.downloads
                    .mark_saving(id, directory.to_string_lossy().into_owned(), None);
            }
        }
        self.downloads_writer.save(id, destination, bytes);
    }

    /// Apply what the inspector reported that the browser has to do about.
    ///
    /// Almost nothing: the panel settles its own state and reports `None` for
    /// it. Editing is the exception, because the panel does not hold the
    /// document — it says what to set and this is what sets it.
    fn apply_inspector(&mut self, action: crate::inspector::Action) {
        let crate::inspector::Action::SetAttribute { name, value } = action else {
            return;
        };
        let Some(node) = self.inspector.selected else {
            return;
        };
        let Some(page) = self.tabs[self.active].page.as_mut() else {
            return;
        };
        // The edit, and then everything downstream of the document: a restyle, a
        // fresh box tree, a relayout. The selection is a node id and the node is
        // still the same node, so what was being looked at is still what is.
        if page.edit(|document| document.set_attr(node, &name, &value)) {
            tracing::info!(%name, "attribute set from the inspector");
        }
    }

    fn apply(&mut self, action: UiAction) {
        match action {
            UiAction::None => {}
            // Focus and the menu belong to the interface and are settled there:
            // the press handler applies them and reports `None`, so these arms
            // are only here to keep the match honest about the whole enum.
            UiAction::Focus(_)
            | UiAction::AddressHit(_)
            | UiAction::FindHit(_)
            | UiAction::CloseFind
            | UiAction::ToggleMenu
            | UiAction::DismissPopup
            | UiAction::ScrollTabs(_) => {}
            UiAction::FindStep(forward) => self.step_match(forward),
            UiAction::ResetZoom => self.step_zoom(ZoomStep::Reset),
            UiAction::ToggleInspector => self.toggle_inspector(),
            UiAction::ToggleBookmark => self.toggle_bookmark(),
            // Chosen from the menu, a browser page opens beside what you were
            // reading rather than over it: the menu is reached *while* looking
            // at something, and losing that to check a preference is the whole
            // reason browsers open these in a tab of their own. Typing the same
            // address, which is a decision to leave, still navigates in place.
            UiAction::OpenPage(page) => self.open_system_in_new_tab(page),
            UiAction::Navigate(url) => self.navigate(&url),
            UiAction::Back => self.go_back(),
            UiAction::Forward => self.go_forward(),
            UiAction::Stop => self.stop(),
            UiAction::NewTab => self.new_tab(),
            UiAction::CloseTab(index) => self.close_tab(index),
            UiAction::SelectTab(index) => self.select_tab(index),
            UiAction::MoveTab { id, to } => self.move_tab(TabId(id), to),
            UiAction::Reload => self.reload(),
            UiAction::Context(command) => self.apply_context(command),
            UiAction::LeaveChrome(forward) => self.hand_keyboard_to_the_page(forward),
        }
    }

    /// Make the active page's search agree with the find bar, and the bar's
    /// count agree with the page.
    ///
    /// A pull rather than a push, because the bar stops being open in more ways
    /// than it starts: Escape, its own cross, and a menu opening over it are all
    /// the same answer to *is the reader still looking for something*, and a
    /// route that had to be remembered at each of them is a route that would be
    /// forgotten at the next one added.
    ///
    /// The query is searched again only when it has changed. Asking for the same
    /// one twice would be honest and would also take the reader back to the
    /// first match every time anything at all happened.
    fn update_find(&mut self) {
        let wanted = self
            .ui
            .finding()
            .then(|| self.ui.find.text().to_owned())
            .filter(|query| !query.is_empty());
        let Some(page) = self.tabs[self.active].page.as_mut() else {
            self.ui.find_status = crate::ui::FindStatus::default();
            return;
        };
        match wanted {
            Some(query) if page.find_query() != Some(query.as_str()) => {
                page.find(&query);
            }
            Some(_) => {}
            None => {
                page.clear_find();
            }
        }
        self.ui.find_status = crate::ui::FindStatus {
            total: page.match_count(),
            current: page.current_match().map_or(0, |at| at + 1),
        };
    }

    /// Go to the next place the query occurs, or the one before it.
    fn step_match(&mut self, forward: bool) {
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.step_match(forward);
        }
        self.update_find();
    }

    /// Show the find bar for whatever the active tab is already searching for.
    ///
    /// The search lives on the page, so a tab coming to the front brings its own
    /// query with it and a tab that is not searching brings no bar. That is what
    /// makes the bar per tab without a second copy of the query to keep in step —
    /// and it is why navigating clears it: the page a search belongs to is gone.
    fn sync_find(&mut self) {
        let query = self.tabs[self.active]
            .page
            .as_ref()
            .and_then(PageScene::find_query)
            .map(str::to_owned);
        match query {
            Some(query) => self.ui.restore_find(&query),
            None => self.ui.close_find(),
        }
        self.update_find();
    }

    /// The keyboard walked off the end of the chrome: give it to the document.
    ///
    /// Only where there is one to walk. A blank tab and a browser page have
    /// nothing to hand it to, so the chrome keeps it and wraps within itself,
    /// which is what it did before there was anywhere else for it to go.
    fn hand_keyboard_to_the_page(&mut self, forward: bool) {
        let entered = self.tabs[self.active].system.is_none()
            && self.tabs[self.active].page.as_mut().is_some_and(|page| {
                // From the end the reader is coming in at, which is what
                // makes shift-Tab out of the toolbar land on the last thing
                // in the document rather than the first.
                page.blur();
                page.focus_step(forward)
            });
        if entered {
            self.activate_surface(SURFACE_PAGE);
            self.accessibility_dirty = true;
            return;
        }
        // Nowhere to go: the chrome takes the keyboard back at its other end.
        self.ui.focus_edge(forward);
    }

    /// Offer the omnibox somewhere to go, from what has been typed.
    ///
    /// Kept pages first and then where the reader has been, newest first,
    /// because a page somebody kept is a page they meant to come back to and
    /// one they visited once may not be. Matching is on the address and on the
    /// title, without case: a person typing "otl" is as likely to be reaching
    /// for the words in the title as for the host.
    fn refresh_suggestions(&mut self) {
        const OFFERED: usize = 6;

        if !self.ui.address_focused() {
            self.ui.set_suggestions(Vec::new());
            return;
        }
        let typed = self.ui.address.text().trim().to_lowercase();
        if typed.is_empty() {
            self.ui.set_suggestions(Vec::new());
            return;
        }
        let matches = |url: &str, title: &str| {
            url.to_lowercase().contains(&typed) || title.to_lowercase().contains(&typed)
        };

        let mut rows: Vec<crate::ui::Suggestion> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for bookmark in self.bookmarks.bookmarks() {
            if rows.len() == OFFERED {
                break;
            }
            if matches(&bookmark.url, &bookmark.title) && seen.insert(bookmark.url.clone()) {
                rows.push(crate::ui::Suggestion {
                    title: bookmark.title.clone(),
                    url: bookmark.url.clone(),
                    kept: true,
                });
            }
        }
        for visit in self.history.visits() {
            if rows.len() == OFFERED {
                break;
            }
            if matches(&visit.url, &visit.title) && seen.insert(visit.url.clone()) {
                rows.push(crate::ui::Suggestion {
                    title: visit.title.clone(),
                    url: visit.url.clone(),
                    kept: false,
                });
            }
        }
        self.ui.set_suggestions(rows);
    }

    /// Offer a menu for whatever the reader asked over.
    ///
    /// The rows are decided here rather than in the interface because every one
    /// of them is a question about the document: is there a link under the
    /// pointer, is anything selected, is there anywhere to go back to. What is
    /// under the pointer is also remembered here, so the row says *what* and
    /// this says *to what* — a menu row that carried a URL would be a second
    /// copy of a fact the browser already holds.
    fn context_menu_requested(&mut self) {
        let (x, y) = self.pointer;
        // The interface has its own menu and the panel has none: a press for a
        // menu that lands on either dismisses whatever is open and stops there,
        // rather than offering the page's rows over something that is not a page.
        self.ui.dismiss_popup();
        if !self.interface && y < UI_HEIGHT {
            return;
        }
        if y < UI_HEIGHT || y >= self.dock_top() {
            return;
        }
        if self.tabs[self.active].page.is_none() {
            return;
        }

        let link = self.link_under_pointer();
        let selection = self.tabs[self.active]
            .page
            .as_ref()
            .is_some_and(PageScene::has_selection);
        let mut rows = Vec::new();
        if link.is_some() {
            rows.push(ContextRow::Command(ContextCommand::OpenLinkInNewTab, true));
            rows.push(ContextRow::Command(ContextCommand::CopyLinkAddress, true));
            rows.push(ContextRow::Divider);
        }
        if selection {
            rows.push(ContextRow::Command(ContextCommand::CopySelection, true));
            rows.push(ContextRow::Divider);
        }
        rows.push(ContextRow::Command(
            ContextCommand::Back,
            self.can_go_back(),
        ));
        rows.push(ContextRow::Command(
            ContextCommand::Forward,
            self.can_go_forward(),
        ));
        rows.push(ContextRow::Command(ContextCommand::Reload, true));
        rows.push(ContextRow::Divider);
        rows.push(ContextRow::Command(ContextCommand::SelectAll, true));
        rows.push(ContextRow::Command(ContextCommand::InspectElement, true));

        self.context_target = Some(ContextTarget { at: (x, y), link });
        self.ui.open_context_menu(x, y, rows);
        self.activate_surface(SURFACE_CHROME);
    }

    /// Do what a row of the context menu says, to what it was asked about.
    fn apply_context(&mut self, command: ContextCommand) {
        // Where the press landed, as it was when the menu opened. A menu that
        // read the pointer now would act on wherever the pointer drifted to
        // between opening the menu and choosing a row.
        let Some(target) = self.context_target.take() else {
            return;
        };
        match command {
            ContextCommand::OpenLinkInNewTab => {
                if let Some(url) = target.link {
                    self.new_tab();
                    self.navigate(&url);
                }
            }
            ContextCommand::CopyLinkAddress => {
                if let Some(url) = target.link {
                    self.clipboard.write(url);
                }
            }
            ContextCommand::CopySelection => {
                self.copy_selection();
            }
            ContextCommand::SelectAll => {
                if let Some(page) = self.tabs[self.active].page.as_mut() {
                    page.select_all();
                }
                self.activate_surface(SURFACE_PAGE);
            }
            ContextCommand::Back => self.go_back(),
            ContextCommand::Forward => self.go_forward(),
            ContextCommand::Reload => self.reload(),
            ContextCommand::InspectElement => {
                if !self.inspector.open {
                    self.toggle_inspector();
                }
                self.pick_at(target.at.0, target.at.1);
                self.activate_surface(SURFACE_INSPECTOR);
            }
        }
    }

    /// How much of a content area `height` tall the inspector takes.
    ///
    /// Only over a document: a browser page is the browser looked at from the
    /// front and has no DOM of its own to inspect, so the panel stays out of the
    /// way rather than showing an empty tree beside one.
    fn dock_height(&self, height: f64) -> f64 {
        if self.tabs[self.active].system.is_some() {
            return 0.0;
        }
        self.inspector.dock_height(height)
    }

    /// The four shades of the chosen box, and its tracks if it has any.
    fn paint_highlight(&mut self, list: &mut otlyra_gfx::DisplayList) {
        let Some(chosen) = self.chosen_box() else {
            return;
        };
        let theme = self.inspector.theme.clone();
        crate::inspector::paint_highlight(
            list,
            &theme,
            chosen.border,
            &chosen.edges,
            chosen.tracks.is_none(),
        );
        if let Some(tracks) = chosen.tracks.as_ref() {
            let mut cx = crate::widget::Cx::new(&mut self.text);
            cx.theme = theme;
            crate::inspector::paint_tracks(
                list,
                &mut cx,
                chosen.edges.content_of(chosen.border),
                tracks,
            );
        }
    }

    /// The panel below it, as the list the panel hands back.
    ///
    /// An `Arc` the panel keeps: while nothing it is drawn from has moved it is
    /// the same list frame after frame, which is what lets the layer above skip
    /// scaling it again.
    fn inspector_panel(
        &mut self,
        width: f64,
        top: f64,
        content_height: f64,
        dock: f64,
    ) -> Arc<otlyra_gfx::DisplayList> {
        let chosen = self.chosen_box();
        let panel = crate::ui::Rect::new(0.0, top + content_height, width, dock);
        // Everything the panel is shown about the page, gathered before it is
        // built: the panel reads, and the browser is what does the reaching.
        let page = self.tabs[self.active].page.as_ref();
        let style = page.and_then(|page| {
            self.inspector
                .selected
                .and_then(|node| page.boxes().box_for(node))
                .and_then(|id| page.boxes().get(id))
                .map(|node| node.style.as_ref())
        });
        // Assembled whether or not the tab has a document: a load that failed
        // has a network list saying why, and hiding the panel behind a page
        // would hide the pane that explains the missing page.
        // Only for the pane that shows them: walking the rule chain for a node
        // nobody is looking at is work for a pane that is not open.
        let rules = match (self.inspector.sidebar, page, self.inspector.selected) {
            (crate::inspector::Sidebar::Rules, Some(page), Some(node)) => page.rules_for(node),
            _ => Vec::new(),
        };
        let facts = crate::inspector::Facts {
            document: page.map(PageScene::document),
            page,
            style,
            rules: &rules,
            rect: chosen.as_ref().map(|chosen| chosen.border),
            containing: chosen.as_ref().and_then(|chosen| chosen.containing),
            exchanges: self.fetcher.exchanges(),
        };
        self.inspector
            .build_display_list(panel, &facts, &mut self.text)
    }

    /// Every request the browser has made, oldest first.
    ///
    /// The fetcher's own list, which is what the inspector's network pane reads:
    /// one account of what was asked for, however it is being looked at.
    pub fn exchanges(&self) -> &[crate::fetcher::Exchange] {
        self.fetcher.exchanges()
    }

    /// The page the active tab is showing, if it has one.
    ///
    /// For a driver asking about the document rather than about the browser: the
    /// same page the inspector reads, so the two cannot answer differently.
    pub fn active_page(&self) -> Option<&PageScene> {
        self.tabs[self.active].page.as_ref()
    }

    /// Where the active tab is, which is what a driver asks after navigating.
    pub fn url(&self) -> String {
        self.tabs[self.active].url.clone()
    }

    /// One frame, as a PNG.
    ///
    /// For a driver with no window: the same path `--screenshot` takes, without
    /// the file. A protocol that had to write to disk and read it back would be
    /// a protocol with a temporary directory in its contract.
    pub fn screenshot(&mut self, viewport: Viewport) -> Result<Vec<u8>, String> {
        otlyra_platform::render_offscreen(self, viewport).map_err(|error| error.to_string())
    }

    /// The inspector, for whoever is driving the browser rather than using it.
    ///
    /// The command line and the screenshot harness both need to open the panel
    /// and choose something in it, and neither has a pointer to do it with.
    pub fn inspector_mut(&mut self) -> &mut crate::inspector::Inspector {
        &mut self.inspector
    }

    /// Choose the element drawn at `x`, `y`, as the picker would.
    ///
    /// Tested against the last frame, like every other hit test here: a point
    /// can only be resolved against a frame that has been drawn.
    pub fn inspect_at(&mut self, x: f64, y: f64) {
        self.inspector.open = true;
        self.pick_at(x, y);
    }

    /// Everything about the chosen node's box that the panel and the overlay
    /// both need.
    ///
    /// The rectangle comes from the same targets a click is tested against, so
    /// the overlay lands exactly where the box did and no second answer to
    /// *where is this* exists.
    fn chosen_box(&self) -> Option<Chosen> {
        self.box_facts(self.inspector.selected?)
    }

    /// The same, for any node rather than the chosen one.
    ///
    /// What a driver asks about: it names a node and wants what the engine made
    /// of it. The overlay and the panel ask through the chosen one, and all
    /// three go through here, so there is one account of what a box is.
    pub fn box_facts(&self, node: otlyra_dom::NodeId) -> Option<Chosen> {
        let page = self.tabs[self.active].page.as_ref()?;
        let id = page.boxes().box_for(node)?;
        let border = to_rect(page.rect_of(id)?);
        let box_node = page.boxes().get(id)?;
        let style = &box_node.style;

        // How wide the containing block is, for the percentages: the parent's
        // content box, worked out the same way this one's is.
        let containing = box_node
            .parent
            .and_then(|parent| Some((page.boxes().get(parent)?, page.rect_of(parent)?)))
            .map(|(parent, rect)| {
                crate::inspector::BoxEdges::of(&parent.style, None)
                    .content_of(to_rect(rect))
                    .width
            });
        // What layout actually gave it, and only failing that what the style
        // says. The used values are the ones a box model is asking about: a
        // computed `margin: auto` is not a number, and the number it came out as
        // is known to layout alone.
        let edges = page
            .used_edges(id)
            .map(crate::inspector::BoxEdges::used)
            .unwrap_or_else(|| crate::inspector::BoxEdges::of(style, containing));

        // A container whose children were laid out into tracks gets the dashed
        // overlay: the lines a stylesheet names are invisible until they are
        // drawn on the page they laid out.
        let tracks = matches!(
            style.display,
            otlyra_css::Display::Grid | otlyra_css::Display::Flex
        )
        .then(|| {
            let items: Vec<crate::ui::Rect> = box_node
                .children
                .iter()
                .filter_map(|child| page.rect_of(*child))
                .map(to_rect)
                .collect();
            crate::inspector::Tracks::of(
                edges.content_of(border),
                &items,
                style.display == otlyra_css::Display::Grid,
                (
                    f64::from(style.gap.0.resolve(border.width as f32)),
                    f64::from(style.gap.1.resolve(border.width as f32)),
                ),
            )
        });

        Some(Chosen {
            border,
            edges,
            containing,
            tracks,
        })
    }

    /// Work out what the pointer should look like where it now is.
    ///
    /// Computed when the pointer moves rather than when the loop asks, because
    /// the loop asks through `&self` and the answer comes from offering the
    /// interface's own tree a press it never applies — which needs the tree.
    /// Asking the tree is what keeps the cursor and the click agreeing: they are
    /// the same question put to the same rectangles.
    fn update_cursor(&mut self, x: f64, y: f64) {
        self.cursor = if let Some(interface) = self.ui.cursor_at(x, y, &mut self.text) {
            interface
        } else if y < UI_HEIGHT || self.ui.popup_owns(x, y) {
            // Over the interface but over nothing in it.
            Cursor::Default
        } else if y >= self.dock_top() {
            match self.inspector.action_at(x, y) {
                crate::inspector::Action::None => Cursor::Default,
                _ => Cursor::Pointer,
            }
        } else if self.inspector.picking {
            // Armed, the whole page is a target, and saying so is what tells a
            // person the next click will not follow a link.
            Cursor::Pointer
        } else {
            match self.tabs[self.active].system {
                Some(SystemPage::Settings) => self.settings.cursor_at(x, y),
                Some(SystemPage::History) => self.history_page.cursor_at(x, y),
                Some(SystemPage::Downloads) => self.downloads_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::Bookmarks) => self.bookmarks_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::Cookies) => self.cookies_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::About) => self.about.cursor_at(x, y, &mut self.text),
                None if self.link_under_pointer().is_some() => Cursor::Pointer,
                None => Cursor::Default,
            }
        };
    }

    /// Choose the element drawn at `x`, `y`, and reveal it in the tree.
    ///
    /// The hit test is the page's own — the one a click is tested against — so
    /// the element the overlay names is the element a click would have hit.
    /// Nothing new is measured and no second answer to *what is here* exists.
    fn pick_at(&mut self, x: f64, y: f64) {
        let (x, y) = self.in_page(x, y);
        let Some(page) = self.tabs[self.active].page.as_ref() else {
            return;
        };
        let Some(node) = page
            .box_at(x, y)
            .and_then(|id| page.boxes().get(id))
            // A box the parser never made a node for is an anonymous one the
            // layout invented. Its nearest real ancestor is what a person means
            // by "this element".
            .and_then(|node| node.node.or_else(|| self.nearest_node(page, node)))
        else {
            return;
        };
        let document = page.document();
        self.inspector.reveal(document, node);
    }

    /// The first node an anonymous box's ancestors carry.
    fn nearest_node(
        &self,
        page: &PageScene,
        node: &otlyra_layout::BoxNode,
    ) -> Option<otlyra_dom::NodeId> {
        let mut current = node.parent;
        while let Some(id) = current {
            let box_node = page.boxes().get(id)?;
            if let Some(node) = box_node.node {
                return Some(node);
            }
            current = box_node.parent;
        }
        None
    }

    /// Where the inspector's panel starts, or the bottom of the window when it
    /// is not showing.
    fn dock_top(&self) -> f64 {
        let top = if self.interface { UI_HEIGHT } else { 0.0 };
        self.last_height - self.dock_height(self.last_height - top)
    }

    /// The link under the pointer, resolved against the tab's own address.
    ///
    /// Resolution happens here rather than at the click, because the cursor has to
    /// know as well, and a link that changes the cursor but goes nowhere — or the
    /// reverse — is worse than neither.
    fn link_under_pointer(&self) -> Option<String> {
        let (x, y) = self.pointer;
        if y < UI_HEIGHT {
            return None;
        }
        let (x, y) = self.in_page(x, y);
        let tab = self.tabs.get(self.active)?;
        let href = tab.page.as_ref()?.link_at(x, y)?;
        Some(otlyra_net::resolve(&tab.url, &href).unwrap_or(href))
    }

    fn labels(&self) -> Vec<TabLabel> {
        self.tabs
            .iter()
            .map(|tab| TabLabel {
                id: tab.id.0,
                title: tab.title.clone(),
                loading: tab.loading(),
            })
            .collect()
    }

    // --- Frame building, shared by the whole-surface and layered paths ---

    /// Run the once-per-frame prelude and settle the geometry and style inputs
    /// every region draws from. Both `paint` and `compose` start here, so they
    /// cannot disagree about what this frame is.
    fn frame_geom(&mut self, viewport: Viewport) -> FrameGeom {
        // Every frame takes in whatever has arrived. A wake is what *asks* for a
        // frame; this is what makes a frame that happened for any other reason —
        // a resize, an animation tick — show what has landed since the last one.
        if self.pump() {
            self.accessibility_dirty = true;
        }

        let width = viewport.logical_width();
        let height = viewport.logical_height();
        self.last_width = width;
        self.last_height = height;
        self.last_scale = viewport.scale_factor;

        // Where the page starts: under the interface, or at the top of the window
        // when there is none.
        let top = if self.interface { UI_HEIGHT } else { 0.0 };
        // The inspector takes its height *out* of the content area rather than
        // sitting over it. A page laid out under a floating panel would be laid
        // out for a width and a height it does not have, and every number the
        // panel then reported about it would be a number about a different page.
        let dock = self.dock_height(height - top);
        let content_height = (height - top - dock).max(0.0);
        let text_scale = (self.settings.settings.text_scale / 100.0) as f32;
        let page_scheme = match self.effective_scheme() {
            otlyra_platform::ColorScheme::Light => otlyra_css::cascade::ColorScheme::Light,
            otlyra_platform::ColorScheme::Dark => otlyra_css::cascade::ColorScheme::Dark,
        };
        FrameGeom {
            width,
            height,
            scale_factor: viewport.scale_factor,
            scale: otlyra_gfx::kurbo::Affine::scale(viewport.scale_factor),
            top,
            dock,
            content_height,
            text_scale,
            page_scheme,
        }
    }

    /// The page, system page, or blank fallback, as one device-space list.
    ///
    /// A real page hands back a cached `Arc` that stays identical while nothing on
    /// it moves, so an unchanged page is scaled to device pixels once and then
    /// reused by pointer identity — no per-frame clone, no per-frame transform.
    fn page_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
        if let Some(system) = self.tabs[self.active].system {
            // A browser page takes the whole content area: it is not a document
            // in a tab, it is the browser looked at from the front.
            let content = crate::ui::Rect::new(0.0, g.top, g.width, g.content_height);
            let mut list = otlyra_gfx::DisplayList::new();
            match system {
                SystemPage::Settings => {
                    self.settings
                        .build_display_list(content, &mut self.text, &mut list);
                }
                SystemPage::History => {
                    self.history_page.build_display_list(
                        content,
                        &self.history,
                        jiff::Zoned::now().date(),
                        &mut self.text,
                        &mut list,
                    );
                }
                SystemPage::Downloads => {
                    self.downloads_page.build_display_list(
                        content,
                        &self.downloads,
                        &mut self.text,
                        &mut list,
                    );
                }
                SystemPage::Bookmarks => {
                    self.bookmarks_page.build_display_list(
                        content,
                        &self.bookmarks,
                        &mut self.text,
                        &mut list,
                    );
                }
                SystemPage::Cookies => {
                    self.cookies_page.build_display_list(
                        content,
                        &self.cookies,
                        &mut self.text,
                        &mut list,
                    );
                }
                _ => self
                    .about
                    .build_display_list(content, &mut self.text, &mut list),
            }
            list.transform(g.scale);
            Arc::new(list)
        } else if self.tabs[self.active].page.is_some() && !self.blocked_on_style(self.active) {
            // Told before the frame is built, because it decides what `medium`
            // computes to and every element that inherited a size inherited that.
            // Laid out in the page's own pixels and drawn back up to the
            // window's. A zoom makes the CSS pixel larger, so the same window
            // holds fewer of them and the page reflows into what is left —
            // which is the difference between zooming a page and magnifying a
            // picture of one. The inset is divided too, so that scaling it back
            // up lands the page under the chrome rather than under a chrome the
            // zoom has moved.
            let zoom = f64::from(self.zoom);
            let logical = {
                let page = self.tabs[self.active].page.as_mut().expect("a page");
                page.set_text_scale(g.text_scale);
                page.set_color_scheme(g.page_scheme);
                page.build_display_list(
                    &mut self.text,
                    (g.width / zoom) as f32,
                    (g.content_height / zoom) as f32,
                    (g.top / zoom) as f32,
                )
            };
            self.scaled_page(logical, g.scale_factor * zoom)
        } else {
            let mut list = otlyra_gfx::DisplayList::new();
            crate::ui::paint_blank_page(
                &mut list,
                &self.theme,
                g.width,
                g.height,
                self.tabs[self.active].error.as_deref(),
                self.mark.as_ref(),
                &mut self.text,
            );
            list.transform(g.scale);
            Arc::new(list)
        }
    }

    /// Scale a page's logical list to device pixels, reusing the last result
    /// while the logical list and the scale are the same.
    ///
    /// The page's own cache returns the same `Arc` frame after frame for an
    /// unchanged page, so pointer identity is a sound "nothing moved" test: on a
    /// hit this returns the already-scaled device list untouched.
    fn scaled_page(
        &mut self,
        logical: Arc<otlyra_gfx::DisplayList>,
        scale: f64,
    ) -> Arc<otlyra_gfx::DisplayList> {
        scaled(&mut self.page_device, logical, scale)
    }

    /// The element overlay, when the inspector has chosen a box.
    fn highlight_list(&mut self, g: &FrameGeom) -> Option<Arc<otlyra_gfx::DisplayList>> {
        self.chosen_box()?;
        let mut list = otlyra_gfx::DisplayList::new();
        self.paint_highlight(&mut list);
        list.transform(g.scale);
        Some(Arc::new(list))
    }

    /// The inspector dock. The caller draws it only when `g.dock > 0`.
    fn inspector_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
        let logical = self.inspector_panel(g.width, g.top, g.content_height, g.dock);
        scaled(&mut self.inspector_device, logical, g.scale_factor)
    }

    /// The tab strip and toolbar.
    fn chrome_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
        let labels = self.labels();
        let logical = self.ui.build_display_list(
            g.width,
            g.height,
            &labels,
            self.active,
            (
                self.tabs[self.active].can_go_back(),
                self.tabs[self.active].can_go_forward(),
            ),
            self.spinner_phase(),
            &mut self.text,
        );
        scaled(&mut self.chrome_device, logical, g.scale_factor)
    }

    /// The picture and font work that follows a frame, once the rules that name
    /// them have been computed on the way to one.
    fn after_frame(&mut self) {
        self.fetch_backgrounds();
        self.fetch_fonts();
        // Last, because it is a question about the window this frame was drawn
        // for: the answer is for the next one.
        self.rechoose_pictures();
    }

    /// A content version for the page layer that changes exactly when the page's
    /// list would draw something different.
    ///
    /// The per-surface `builds` counters advance only on a real rebuild, so an
    /// unchanged page keeps its epoch and its retained pixels. The blank fallback
    /// has no such counter, so its inputs are hashed directly; the active tab
    /// index is folded in so switching between two tabs at the same build count
    /// still re-rasterizes.
    fn page_epoch(&self, g: &FrameGeom) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.active.hash(&mut hasher);
        let tab = &self.tabs[self.active];
        if let Some(system) = tab.system {
            match system {
                SystemPage::Settings => 10u8,
                SystemPage::History => 11u8,
                SystemPage::Downloads => 12u8,
                SystemPage::Bookmarks => 14u8,
                SystemPage::Cookies => 15u8,
                _ => 13u8,
            }
            .hash(&mut hasher);
            self.settings.builds().hash(&mut hasher);
            self.history_page.builds().hash(&mut hasher);
            self.downloads_page.builds().hash(&mut hasher);
            self.bookmarks_page.builds().hash(&mut hasher);
            self.cookies_page.builds().hash(&mut hasher);
            self.about.builds().hash(&mut hasher);
        } else if let Some(page) = tab.page.as_ref() {
            1u8.hash(&mut hasher);
            page.builds().hash(&mut hasher);
        } else {
            2u8.hash(&mut hasher);
            tab.error.hash(&mut hasher);
            g.width.to_bits().hash(&mut hasher);
            g.height.to_bits().hash(&mut hasher);
            g.scale_factor.to_bits().hash(&mut hasher);
            matches!(g.page_scheme, otlyra_css::cascade::ColorScheme::Dark).hash(&mut hasher);
            self.mark.is_some().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// The part of the page layer this frame changed, in device pixels, when the
    /// page changed only part of itself.
    ///
    /// The page answers in its own coordinates — where a box sits in the document
    /// — and this is the one place that turns those into the surface's: down by
    /// the scroll, along by the interface's height, and into device pixels. The
    /// rectangle is grown by a pixel on every side before it is rounded out, so
    /// that a glyph that was antialiased across the boundary is inside it.
    ///
    /// `None` means the whole layer, which is what a page says whenever it has
    /// restyled, relaid out, or scrolled. The compositor cuts what comes back to
    /// the layer's own bounds, so a field scrolled half out of view is not this
    /// function's problem.
    fn page_dirty(&self, g: &FrameGeom) -> Option<LayerRect> {
        let page = self.tabs[self.active].page.as_ref()?;
        let dirty = page.dirty()?;
        let scroll = f64::from(page.scroll());
        let left = (f64::from(dirty.x) - 1.0) * g.scale_factor;
        let top = (f64::from(dirty.y) - scroll + g.top - 1.0) * g.scale_factor;
        let right = (f64::from(dirty.x + dirty.width) + 1.0) * g.scale_factor;
        let bottom = (f64::from(dirty.y + dirty.height) - scroll + g.top + 1.0) * g.scale_factor;
        let x = left.floor().max(0.0) as u32;
        let y = top.floor().max(0.0) as u32;
        let width = (right.ceil().max(0.0) as u32).saturating_sub(x);
        let height = (bottom.ceil().max(0.0) as u32).saturating_sub(y);
        // A known dirty rectangle can be wholly outside the viewport (for
        // example, typing into a field that remains focused after scrolling).
        // Preserve that knowledge as an empty rectangle. `None` means the page
        // could not bound its change and therefore dirtied the whole layer.
        Some(LayerRect {
            x,
            y,
            width,
            height,
        })
    }

    /// The part of the chrome layer changed by this frame, in device pixels.
    fn chrome_dirty(&self, g: &FrameGeom) -> Option<LayerRect> {
        let dirty = self.ui.dirty()?;
        let left = (dirty.x * g.scale_factor).floor().max(0.0) as u32;
        let top = (dirty.y * g.scale_factor).floor().max(0.0) as u32;
        let right = ((dirty.x + dirty.width) * g.scale_factor).ceil().max(0.0) as u32;
        let bottom = ((dirty.y + dirty.height) * g.scale_factor).ceil().max(0.0) as u32;
        Some(LayerRect {
            x: left,
            y: top,
            width: right.saturating_sub(left),
            height: bottom.saturating_sub(top),
        })
    }

    /// A content version for the chrome layer. The tab strip and toolbar each
    /// rebuild only when their own inputs change, so the sum of their build
    /// counters moves exactly when the chrome's pixels would.
    fn chrome_epoch(&self) -> u64 {
        self.ui
            .builds()
            .wrapping_add(self.ui.tab_builds())
            .wrapping_add(self.ui.toolbar_builds())
    }

    /// A content version for the inspector layer, summing its retained
    /// boundaries' build counters for the same reason.
    fn inspector_epoch(&self) -> u64 {
        self.inspector
            .builds()
            .wrapping_add(self.inspector.header_builds())
            .wrapping_add(self.inspector.body_builds())
    }

    /// A content version for the element overlay: the identity and geometry of
    /// the chosen box, so it re-rasterizes when the highlight moves and not
    /// otherwise.
    fn highlight_epoch(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Some(chosen) = self.chosen_box() {
            chosen.border.x.to_bits().hash(&mut hasher);
            chosen.border.y.to_bits().hash(&mut hasher);
            chosen.border.width.to_bits().hash(&mut hasher);
            chosen.border.height.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Stable layer identities for the compositor. Back to front: the page, the
/// element overlay, the inspector dock, the chrome.
const LAYER_PAGE: u64 = 0;
const LAYER_HIGHLIGHT: u64 = 1;
const LAYER_INSPECTOR: u64 = 2;
const LAYER_CHROME: u64 = 3;

/// The per-frame geometry and style inputs both paint paths share.
struct FrameGeom {
    width: f64,
    height: f64,
    scale_factor: f64,
    scale: otlyra_gfx::kurbo::Affine,
    top: f64,
    dock: f64,
    content_height: f64,
    text_scale: f32,
    page_scheme: otlyra_css::cascade::ColorScheme,
}

impl Painter for Browser {
    fn set_waker(&mut self, waker: Waker) {
        self.fetcher.set_waker(waker.clone());
        self.downloads_writer.set_waker(waker);
        // Anything that finished before the loop had a waker to be woken by is
        // sitting in the channel: a page asked for on the command line usually
        // arrives before the window exists.
        self.pump();
    }

    /// Continue only visible animation. Background tabs wake the loop when their
    /// model changes; they do not drive the active window at display pace.
    fn next_frame(&self) -> FrameRequest {
        let Some(tab) = self.tabs.get(self.active) else {
            return FrameRequest::None;
        };
        if tab.loading() {
            return FrameRequest::Vsync;
        }
        // The caret's next half-second and the pause before a control is named:
        // whichever comes first, because one wake serves both and a loop told
        // about the later one would sleep through the earlier.
        let caret = tab
            .page
            .as_ref()
            .and_then(crate::page::PageScene::next_caret_frame);
        let tooltip = self.ui.next_tooltip_frame();
        match (caret, tooltip) {
            (Some(caret), Some(tooltip)) => FrameRequest::At(caret.min(tooltip)),
            (Some(at), None) | (None, Some(at)) => FrameRequest::At(at),
            (None, None) => FrameRequest::None,
        }
    }

    fn work_counters(&self) -> PainterWork {
        let legacy = self.settings.builds()
            + self.history_page.builds()
            + self.downloads_page.builds()
            + self.bookmarks_page.builds()
            + self.about.builds()
            + self.inspector.builds();
        let chrome_roots = legacy + self.ui.builds();
        let retained_boundaries = self.ui.tab_builds()
            + self.ui.toolbar_builds()
            + self.inspector.header_builds()
            + self.inspector.body_builds();
        PainterWork {
            // Legacy surfaces still perform all three passes on a cache miss.
            // BrowserUi additionally reports work performed behind the retained
            // tab-strip and toolbar boundaries.
            chrome_reconciles: chrome_roots,
            chrome_layouts: chrome_roots + retained_boundaries,
            chrome_paints: chrome_roots + retained_boundaries,
            chrome_semantics: self.ui.tab_semantics_builds()
                + self.ui.toolbar_semantics_builds()
                + self.inspector.header_semantics_builds()
                + self.inspector.body_semantics_builds(),
            page_paints: self
                .tabs
                .iter()
                .filter_map(|tab| tab.page.as_ref())
                .map(PageScene::builds)
                .sum(),
        }
    }

    fn handle_event(&mut self, event: PlatformEvent) -> FrameRequest {
        let previous_pointer = self.pointer;
        let picking_before = self.inspector.picking;
        let selected_before = self.inspector.selected;
        let page_damage = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.page.as_ref())
            .map(PageScene::damage);
        self.on_event(event);

        let PlatformEvent::PointerMoved { x, y } = event else {
            self.accessibility_dirty = true;
            return FrameRequest::Now;
        };
        if previous_pointer == (x, y) {
            return FrameRequest::None;
        }

        let current_damage = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.page.as_ref())
            .map(PageScene::damage);
        let dock_top = self.dock_top();
        let chrome_changed = previous_pointer.1 < UI_HEIGHT
            || y < UI_HEIGHT
            || self.ui.popup_open()
            || self.ui.pointer_captured();
        let inspector_changed =
            self.inspector.open && (previous_pointer.1 >= dock_top || y >= dock_top);
        let picker_highlight_changed = picking_before && selected_before != self.inspector.selected;
        let system_page_changed = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.system.is_some())
            && (previous_pointer.1 >= UI_HEIGHT || y >= UI_HEIGHT);
        let request = if chrome_changed
            || inspector_changed
            || picker_highlight_changed
            || system_page_changed
            || page_damage != current_damage
        {
            FrameRequest::Now
        } else {
            FrameRequest::None
        };
        if page_damage != current_damage
            && current_damage.is_some_and(|damage| damage.contains(otlyra_layout::Damage::LAYOUT))
        {
            self.accessibility_dirty = true;
        }
        request
    }

    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            // Background work finished: a fetch or an attachment write. What it
            // was is the browser's business; the loop only knows it should ask.
            PlatformEvent::Woken => {
                self.pump();
            }

            PlatformEvent::AppearanceChanged(scheme) => {
                self.scheme = scheme;
                self.apply_theme();
            }

            // A context menu hangs off a point in a window that has just
            // stopped being that window: the page reflows under it and its rows
            // go on describing what used to be there.
            PlatformEvent::Resized(viewport) => {
                if viewport.logical_width() != self.last_width
                    || viewport.logical_height() != self.last_height
                {
                    self.ui.dismiss_context_menu();
                    self.context_target = None;
                }
                self.set_viewport(viewport);
            }

            PlatformEvent::PointerMoved { x, y } => {
                self.pointer = (x, y);
                // A move can be a drag carrying something: a tab being dragged
                // along the strip reports where it should sit now.
                let action = self.ui.pointer_moved(x, y, &mut self.text);
                self.apply(action);
                self.update_cursor(x, y);
                self.inspector.pointer_moved(x, y);
                // While the picker is armed, moving over the page is enough to
                // show what would be chosen: an overlay that only appeared after
                // a click would be an overlay nobody could aim.
                if self.inspector.picking && y >= UI_HEIGHT && y < self.dock_top() {
                    self.pick_at(x, y);
                    return;
                }

                // A selection being made keeps the pointer the same way a scrollbar
                // does: what is between where the press landed and where the
                // pointer is now is what is selected, wherever it wanders.
                if self.selecting {
                    let top = self.page_top();
                    let (x, y) = self.in_page(x, y);
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        page.select_to(x as f32, y as f32, top);
                        return;
                    }
                }

                // A scrollbar being dragged keeps the pointer until it is let go,
                // wherever the pointer wanders.
                let (width, height) = self.in_page(self.last_width, self.last_height - UI_HEIGHT);
                let (_, page_y) = self.in_page(0.0, y - UI_HEIGHT);
                if let Some(page) = self.tabs[self.active].page.as_mut()
                    && page.dragging_scrollbar()
                {
                    page.drag_scrollbar(page_y as f32, width as f32, height.max(0.0) as f32);
                    return;
                }
                // The page follows the pointer: `:hover` on what it is over, and
                // the widget under it drawn as hovered. Nothing is repainted unless
                // something depends on it, which for most pages is never.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let over_page = y >= UI_HEIGHT && y < self.dock_top();
                    let (page_x, page_y) = self.in_page(x, y);
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        let _ = if over_page {
                            page.pointer_moved(page_x, page_y)
                        } else {
                            page.pointer_left()
                        };
                    }
                }
                match self.tabs[self.active].system {
                    // Moves matter to a surface that has a slider on it: that is
                    // what a drag is made of.
                    Some(SystemPage::Settings) => {
                        let action = self.settings.pointer_moved(x, y);
                        self.handle_settings_action(&action);
                    }
                    Some(SystemPage::History) => self.history_page.pointer_moved(x, y),
                    Some(SystemPage::Downloads) => self.downloads_page.pointer_moved(x, y),
                    Some(SystemPage::Bookmarks) => self.bookmarks_page.pointer_moved(x, y),
                    Some(SystemPage::Cookies) => self.cookies_page.pointer_moved(x, y),
                    Some(SystemPage::About) => self.about.pointer_moved(x, y),
                    _ => {}
                }
            }

            PlatformEvent::PointerPressed { clicks } => {
                // A popup owns the press only where it is drawn — a menu's
                // sheet is drawn everywhere, a list of suggestions only under
                // the field. Outside it, the press is the page's and the list
                // goes away on the way past.
                let owned = self.ui.popup_owns(self.pointer.0, self.pointer.1);
                if !owned {
                    self.ui.dismiss_popup();
                }
                let surface = if owned || self.pointer.1 < UI_HEIGHT {
                    SURFACE_CHROME
                } else if self.inspector.open && self.pointer.1 >= self.dock_top() {
                    SURFACE_INSPECTOR
                } else if self.tabs[self.active].system.is_some() {
                    SURFACE_SYSTEM
                } else {
                    SURFACE_PAGE
                };
                self.activate_surface(surface);
                // A press below the toolbar takes the focus off the address field,
                // wherever it lands — a page, a control, a link, a scrollbar, a
                // system page, the inspector. Every one of those paths answers the
                // press and returns before the toolbar's own press handler runs, so
                // without this the caret and its selection would sit in a field the
                // reader has plainly clicked away from.
                if self.pointer.1 >= UI_HEIGHT && !owned {
                    self.ui.blur();
                }
                // The panel owns everything below its own top edge.
                if self.pointer.1 >= self.dock_top() && !owned {
                    let action = self.inspector.pointer_pressed();
                    self.apply_inspector(action);
                    return;
                }
                // Armed, a press on the page chooses an element instead of
                // following whatever is under it — which is the whole point of
                // arming it, and why the picker disarms itself afterwards.
                if self.inspector.picking && self.pointer.1 >= UI_HEIGHT {
                    self.pick_at(self.pointer.0, self.pointer.1);
                    // One press, one element: staying armed would make the next
                    // click on a link choose a node instead of following it.
                    self.inspector.picking = false;
                    return;
                }
                // A press on a scrollbar belongs to it rather than to the page
                // behind it.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let (width, height) =
                        self.in_page(self.last_width, self.last_height - UI_HEIGHT);
                    let (x, y) = self.in_page(self.pointer.0, self.pointer.1 - UI_HEIGHT);
                    if let Some(page) = self.tabs[self.active].page.as_mut()
                        && page.grab_scrollbar(
                            x as f32,
                            y as f32,
                            width as f32,
                            height.max(0.0) as f32,
                        )
                    {
                        return;
                    }
                }

                // The settings surface owns everything below the toolbar while it
                // is showing, so a press there never reaches the document behind
                // it — there is no document behind it.
                if self.pointer.1 >= UI_HEIGHT && !owned {
                    match self.tabs[self.active].system {
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            let action = self.settings.pointer_pressed(clicks);
                            self.save_preferences_if_changed(&before);
                            self.handle_settings_action(&action);
                            return;
                        }
                        Some(SystemPage::History) => {
                            let action = self.history_page.pointer_pressed(clicks);
                            self.handle_history_action(action);
                            return;
                        }
                        Some(SystemPage::Downloads) => {
                            let action = self.downloads_page.pointer_pressed(&mut self.text);
                            self.handle_downloads_action(action);
                            return;
                        }
                        Some(SystemPage::Bookmarks) => {
                            let action = self.bookmarks_page.pointer_pressed(&mut self.text);
                            self.handle_bookmarks_action(action);
                            return;
                        }
                        Some(SystemPage::Cookies) => {
                            let action = self.cookies_page.pointer_pressed(&mut self.text);
                            self.handle_cookies_action(action);
                            return;
                        }
                        Some(SystemPage::About) => {
                            if self.about.pointer_pressed(&mut self.text)
                                == about::Action::OpenSettings
                            {
                                self.open_system(SystemPage::Settings);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                // A link takes the press before the interface sees it, because the
                // interface has nothing in the page area to claim it — except an
                // open menu, which is drawn over the page and owns every press
                // that lands on it.
                if !self.ui.owns_pointer()
                    && let Some(url) = self.link_under_pointer()
                {
                    self.navigate_from(&url, false);
                    return;
                }
                // A control takes the press before the text under it does: pressing
                // a checkbox is not the start of selecting the word beside it.
                if !self.ui.owns_pointer()
                    && self.tabs[self.active].system.is_none()
                    && self.pointer.1 >= UI_HEIGHT
                {
                    let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                    let pressed = self.tabs[self.active]
                        .page
                        .as_mut()
                        .is_some_and(|page| page.control_under(x, y));
                    if pressed {
                        if let Some(page) = self.tabs[self.active].page.as_mut() {
                            page.clear_selection();
                            page.pointer_pressed_times(x, y, clicks);
                        }
                        return;
                    }
                    // Nothing was pressed but the page itself, so the field the
                    // reader was typing in stops being where their typing goes —
                    // the same statement the toolbar's blur above answers, made
                    // about the document instead. The press then goes on to start
                    // a selection where it landed.
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        page.blur();
                    }
                }
                // A press on the page starts a selection where it landed, and takes
                // away whatever was selected before — which is what a press on a
                // page means everywhere else.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                    let top = self.page_top();
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        let (x, y) = (x as f32, y as f32);
                        // A second click takes the word and a third the block it
                        // is in; a fourth starts over, which is what the count
                        // running past three means.
                        match clicks % 3 {
                            2 => {
                                page.select_word_at(x, y, top);
                            }
                            0 if clicks > 0 => {
                                page.select_paragraph_at(x, y, top);
                            }
                            _ => {
                                page.select_from(x, y, top);
                            }
                        }
                        // A drag after a second or third click extends what that
                        // click took, from wherever it put the far end.
                        self.selecting = true;
                        return;
                    }
                }
                // The press is tested against the geometry of the last frame —
                // which is the frame the user was looking at when they pressed.
                let action = self.ui.pointer_pressed(&mut self.text, clicks);
                self.apply(action);
                // The bar's own cross closes it, and a menu opening over it
                // displaces it. Neither says so — the page's search is asked
                // about rather than told.
                self.update_find();
            }

            PlatformEvent::ContextMenuRequested => self.context_menu_requested(),

            PlatformEvent::PointerReleased => {
                self.selecting = false;
                let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                if let Some(page) = self.tabs[self.active].page.as_mut() {
                    page.release_scrollbar();
                    // A control is activated on the release and only where the
                    // press landed: a press that wanders off the checkbox before it
                    // is let go does not tick it, which is what every platform does
                    // and what makes a press a thing a reader can take back.
                    page.pointer_released(x, y);
                }
                self.follow_submission();
                self.answer_file_request();
                self.settings.pointer_released();
                self.history_page.pointer_released();
                self.ui.pointer_released();
            }

            PlatformEvent::KeyPressed { key, modifiers } => {
                // The one accelerator that is the inspector's. Alt as well as
                // the platform's own modifier, which is what every browser uses
                // and what keeps it clear of ⌘I.
                if key == Key::Character('i') && modifiers.alt && inspector_modifier(modifiers) {
                    self.toggle_inspector();
                    return;
                }
                // The zoom, before anything else reads the key: it belongs to
                // the page whatever holds the keyboard, which is the whole point
                // of being able to reach it while reading.
                if modifiers.is_accelerator()
                    && let Some(step) = match key {
                        Key::Character('=' | '+') => Some(ZoomStep::In),
                        Key::Character('-' | '_') => Some(ZoomStep::Out),
                        Key::Character('0') => Some(ZoomStep::Reset),
                        _ => None,
                    }
                {
                    self.step_zoom(step);
                    return;
                }
                // The panel takes the keys that walk its tree, but only while it
                // is the thing being looked at — and a caret in the address
                // field means the field is, however open the panel may be.
                //
                // With or without a document: a tab whose load failed still has
                // a console to filter and clear, and gating the panel's keys on
                // a page would take them away exactly when they are wanted.
                if self.keyboard_surface == SURFACE_INSPECTOR
                    && self.inspector.open
                    && self
                        .inspector
                        .key_pressed(
                            key,
                            modifiers,
                            self.tabs[self.active]
                                .page
                                .as_ref()
                                .map(PageScene::document),
                            self.clipboard.as_mut(),
                        )
                        .is_some()
                {
                    if !self.inspector.open {
                        let surface = self.tab_surface();
                        self.activate_surface(surface);
                    }
                    return;
                }
                // A browser page shown in the tab gets the key first: it is what
                // the reader is looking at, and Tab on it walks its own controls
                // rather than the toolbar's.
                if self.keyboard_surface == SURFACE_SYSTEM {
                    match self.tabs[self.active].system {
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            if let Some(action) =
                                self.settings
                                    .key_pressed(key, modifiers, self.clipboard.as_mut())
                            {
                                self.save_preferences_if_changed(&before);
                                self.handle_settings_action(&action);
                                return;
                            }
                        }
                        Some(SystemPage::History) => {
                            if let Some(action) = self.history_page.key_pressed(
                                key,
                                modifiers,
                                self.clipboard.as_mut(),
                            ) {
                                self.handle_history_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Downloads) => {
                            if let Some(action) =
                                self.downloads_page
                                    .key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_downloads_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Bookmarks) => {
                            if let Some(action) =
                                self.bookmarks_page
                                    .key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_bookmarks_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Cookies) => {
                            if let Some(action) =
                                self.cookies_page
                                    .key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_cookies_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::About) => {
                            match self.about.key_pressed(key, modifiers, &mut self.text) {
                                Some(about::Action::OpenSettings) => {
                                    self.open_system(SystemPage::Settings);
                                    return;
                                }
                                Some(_) => return,
                                None => {}
                            }
                        }
                        _ => {}
                    }
                }
                // Walking the document with Tab, before anything else reads the
                // key: it is never a character, and it is what a reader without
                // a pointer moves through a page with.
                if key == Key::Tab
                    && !modifiers.is_accelerator()
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                {
                    let forward = !modifiers.shift;
                    let walked = self.tabs[self.active]
                        .page
                        .as_mut()
                        .is_some_and(|page| page.focus_step(forward));
                    if walked {
                        self.accessibility_dirty = true;
                        return;
                    }
                    // Off the end of the document, so the keyboard goes on to
                    // the browser around it — which is the whole of what a
                    // document not trapping the keyboard means.
                    self.activate_surface(SURFACE_CHROME);
                    self.ui.focus_edge(forward);
                    return;
                }

                // Return on a link the keyboard reached follows it, which is
                // the same navigation a click on it is — one route, so the two
                // cannot come to disagree about what following a link means.
                if key == Key::Enter
                    && !modifiers.is_accelerator()
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && let Some(href) = self.tabs[self.active]
                        .page
                        .as_ref()
                        .and_then(PageScene::focused_link)
                {
                    let url =
                        otlyra_net::resolve(&self.tabs[self.active].url, &href).unwrap_or(href);
                    self.navigate_from(&url, false);
                    return;
                }

                // Copying what is selected on the page, before the interface reads
                // the key: the address bar takes ⌘C for its own text only while it
                // holds the caret, and the page's selection is the one on screen.
                if key == Key::Character('c')
                    && modifiers.command
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.copy_selection()
                {
                    return;
                }

                // Selecting the page, and moving what is selected. Both go to the
                // page only while the interface does not hold the caret, for the
                // same reason ⌘C does: the address bar's own text is a selection
                // too, and there is one keyboard between them.
                // Return in a field sends the form it is in, which is why a search
                // box with nothing but a field in it works at all.
                if key == Key::Enter
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && self.tabs[self.active]
                        .page
                        .as_mut()
                        .is_some_and(PageScene::implicit_submit)
                {
                    self.follow_submission();
                    return;
                }
                // Editing what is in a field in the page, before the keys that
                // move a selection: an arrow in a focused field moves the caret
                // and not the page's selection.
                if self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && self.page_edit_key(key, modifiers)
                {
                    return;
                }
                if self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && self.page_selection_key(key, modifiers)
                {
                    return;
                }

                let global = key == Key::F5
                    || modifiers.is_accelerator()
                    || (key == Key::Escape && self.ui.popup_open());
                if self.keyboard_surface == SURFACE_CHROME || global {
                    let typed = self.ui.address.text().to_owned();
                    let action = self.ui.key_pressed(
                        key,
                        modifiers,
                        &mut self.text,
                        self.clipboard.as_mut(),
                    );
                    let none = action == UiAction::None;
                    // Which keyboard the chrome now claims. ⌘F claims it before
                    // the field it claims it for exists — the bar is built by
                    // the next frame — so the wish counts as much as the fact.
                    let chrome = self.ui.address_focused()
                        || self.ui.find_focused()
                        || self.ui.find_wants_keyboard();
                    self.apply(action);
                    // Only when the key changed what is in the field: Escape
                    // puts the list away, and a refresh that ran anyway would
                    // put it straight back.
                    if typed != self.ui.address.text() {
                        self.refresh_suggestions();
                    }
                    // What the page is searching for is whatever the bar says,
                    // and the key may have changed either — ⌘F opened it, a
                    // letter narrowed it, Escape put it away.
                    self.update_find();
                    if chrome {
                        self.activate_surface(SURFACE_CHROME);
                    }
                    if none && self.keyboard_surface == SURFACE_PAGE {
                        self.scroll_by_key(key);
                    }
                } else if self.keyboard_surface == SURFACE_PAGE {
                    self.scroll_by_key(key);
                }
            }

            PlatformEvent::TextInput(character) => {
                // Text/IME is exclusive: a stale caret retained by a background
                // root must never get a chance after the active root declines.
                match self.keyboard_surface {
                    SURFACE_INSPECTOR => {
                        let _ = self.inspector.text_input(character);
                    }
                    SURFACE_PAGE => {
                        if self.tabs[self.active].system.is_none()
                            && let Some(page) = self.tabs[self.active].page.as_mut()
                        {
                            let _ = page.typed(&character.to_string());
                        }
                    }
                    SURFACE_SYSTEM => match self.tabs[self.active].system {
                        Some(SystemPage::History) => {
                            let _ = self.history_page.text_input(character);
                        }
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            if self.settings.text_input(character) {
                                // Typing in the home field is a preference
                                // changing, one character at a time.
                                self.save_preferences_if_changed(&before);
                            }
                        }
                        _ => {}
                    },
                    SURFACE_CHROME if self.ui.text_input(character) => {
                        // What is offered is a function of what is typed, so it
                        // is settled here rather than remembered: one place
                        // that can go stale instead of two.
                        self.refresh_suggestions();
                        // And so is what the page is searching for.
                        self.update_find();
                    }
                    _ => {}
                }
            }

            // Scrolling belongs to the page unless the pointer is over the
            // interface, where there is nothing to scroll.
            //
            // Every one of these adds the delta to an offset and none of them
            // negates it. The event already says which way the reader went, and
            // a consumer that decided that for itself is how the settings came
            // to scroll the opposite way from a document.
            PlatformEvent::Scroll {
                x, y, modifiers, ..
            } => {
                // The wheel with the platform's own modifier held is a zoom
                // rather than a scroll, everywhere. One notch a step, so that a
                // hand on a trackpad does not run the whole ladder in a flick:
                // the delta says how far, and what a reader wants from this is
                // which way.
                if modifiers.is_accelerator() {
                    if y.abs() > f64::EPSILON {
                        self.step_zoom(if y < 0.0 { ZoomStep::In } else { ZoomStep::Out });
                    }
                    return;
                }
                if self.ui.owns_pointer() {
                    // The tab strip is a thing under the pointer like any other,
                    // and a strip with more tabs than it can show is a strip the
                    // wheel should move. Whichever axis the wheel reported the
                    // more of: a mouse with one wheel says `y` and a trackpad
                    // swiped sideways says `x`, and both mean the same thing to a
                    // strip that only runs one way.
                    if self.pointer.1 < crate::ui::TAB_STRIP_HEIGHT && !self.ui.popup_open() {
                        let delta = if x.abs() > y.abs() { x } else { y };
                        self.ui.scroll_tabs_by(delta);
                    }
                    return;
                }
                // The wheel goes to whatever is under the pointer, and the panel
                // is a thing under the pointer like any other.
                if self.pointer.1 >= self.dock_top() {
                    self.inspector.scroll_by(y);
                    return;
                }
                if self.tabs[self.active].system == Some(SystemPage::Settings) {
                    self.settings.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::History) {
                    self.history_page.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::Downloads) {
                    self.downloads_page.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::Bookmarks) {
                    self.bookmarks_page.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::Cookies) {
                    self.cookies_page.scroll_by(y);
                } else if let Some(page) = self.tabs[self.active].page.as_mut() {
                    // The wheel goes to whatever is under the pointer: a box that
                    // scrolls takes it first, and the page takes it once that box
                    // has reached its end.
                    let (x, pointer_y) = self.pointer;
                    // In the page's own pixels, like every other question a
                    // pointer asks it. Read from the field rather than through
                    // `in_page`, which wants a borrow the page already has.
                    let zoom = f64::from(self.zoom);
                    page.scroll_at(
                        (x / zoom) as f32,
                        ((pointer_y - UI_HEIGHT) / zoom) as f32,
                        y as f32,
                    );
                }
            }

            // The menu and the keyboard reach the same commands: one definition of
            // what each means, invoked from wherever the user found it.
            PlatformEvent::MenuCommand(id) => match crate::menu::Command::from_id(id) {
                Some(crate::menu::Command::Reload) => self.reload(),
                Some(crate::menu::Command::ReloadIgnoringCache) => self.reload_ignoring_cache(),
                Some(crate::menu::Command::Stop) => self.stop(),
                Some(crate::menu::Command::Back) => self.go_back(),
                Some(crate::menu::Command::Forward) => self.go_forward(),
                Some(crate::menu::Command::Home) => self.go_home(),
                Some(crate::menu::Command::Settings) => {
                    self.open_system_in_new_tab(SystemPage::Settings);
                }
                Some(crate::menu::Command::ShowHistory) => {
                    self.open_system_in_new_tab(SystemPage::History);
                }
                Some(crate::menu::Command::ShowDownloads) => {
                    self.open_system_in_new_tab(SystemPage::Downloads);
                }
                Some(crate::menu::Command::ShowBookmarks) => {
                    self.open_system_in_new_tab(SystemPage::Bookmarks);
                }
                Some(crate::menu::Command::ShowCookies) => {
                    self.open_system_in_new_tab(SystemPage::Cookies);
                }
                Some(crate::menu::Command::ToggleBookmark) => self.toggle_bookmark(),
                Some(crate::menu::Command::ToggleDevTools) => self.toggle_inspector(),
                Some(crate::menu::Command::ZoomIn) => self.step_zoom(ZoomStep::In),
                Some(crate::menu::Command::ZoomOut) => self.step_zoom(ZoomStep::Out),
                Some(crate::menu::Command::ActualSize) => self.step_zoom(ZoomStep::Reset),
                Some(crate::menu::Command::NewTab) => self.new_tab(),
                Some(crate::menu::Command::CloseTab) => self.close_tab(self.active),
                // The editing four are the keystroke they carry, delivered as
                // one. A menu item that did the copying itself would be a second
                // answer to *what does ⌘C mean here* — and the answer depends on
                // which surface holds the keyboard, which the key path already
                // knows and this would have to learn again.
                Some(
                    command @ (crate::menu::Command::Cut
                    | crate::menu::Command::Copy
                    | crate::menu::Command::Paste
                    | crate::menu::Command::SelectAll),
                ) => {
                    let character = match command {
                        crate::menu::Command::Cut => 'x',
                        crate::menu::Command::Copy => 'c',
                        crate::menu::Command::Paste => 'v',
                        _ => 'a',
                    };
                    self.on_event(PlatformEvent::KeyPressed {
                        key: Key::Character(character),
                        modifiers: Modifiers {
                            command: cfg!(target_os = "macos"),
                            control: !cfg!(target_os = "macos"),
                            ..Modifiers::default()
                        },
                    });
                }
                Some(command) => tracing::info!(?command, "command not implemented yet"),
                None => tracing::warn!(?id, "menu reported an id no command claims"),
            },

            PlatformEvent::AccessibilityRequest { node, action } => {
                self.accessibility_dirty = true;
                // A node the page owns rather than the interface. It takes the
                // route the pointer takes — the focus first and the activation
                // behaviour after — because a reader pressing a control means what
                // a click on it means, down to the form it sends.
                let Some(index) = crate::a11y::described_index(node) else {
                    self.activate_surface(SURFACE_PAGE);
                    self.accessibility_request_on_page(node, action);
                    return;
                };

                // The description is the toolbar's controls followed by the
                // browser page's, so an index past the toolbar belongs to the
                // page. Counting rather than tagging, because the two lists are
                // built one after the other in the same frame and the count is
                // what the identifiers were handed out from.
                let toolbar = self.ui.describe().len();
                if index < toolbar {
                    self.activate_surface(SURFACE_CHROME);
                    let action = self.ui.activate_described(index, &mut self.text);
                    self.apply(action);
                    return;
                }

                let index = index - toolbar;
                self.activate_surface(SURFACE_SYSTEM);
                match self.tabs.get(self.active).and_then(|tab| tab.system) {
                    Some(SystemPage::Settings) => {
                        let before = self.settings.settings.clone();
                        let action = self.settings.activate_described(index);
                        self.save_preferences_if_changed(&before);
                        self.handle_settings_action(&action);
                    }
                    Some(SystemPage::History) => {
                        let action = self.history_page.activate_described(index);
                        self.handle_history_action(action);
                    }
                    Some(SystemPage::Downloads) => {
                        let action = self
                            .downloads_page
                            .activate_described(index, &mut self.text);
                        self.handle_downloads_action(action);
                    }
                    Some(SystemPage::Bookmarks) => {
                        let action = self
                            .bookmarks_page
                            .activate_described(index, &mut self.text);
                        self.handle_bookmarks_action(action);
                    }
                    Some(SystemPage::Cookies) => {
                        let action = self.cookies_page.activate_described(index, &mut self.text);
                        self.handle_cookies_action(action);
                    }
                    Some(SystemPage::About)
                        if self.about.activate_described(index, &mut self.text)
                            == about::Action::OpenSettings =>
                    {
                        self.open_system(SystemPage::Settings);
                    }
                    _ => {}
                }
            }

            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            _ => {}
        }
    }

    fn accessibility(&mut self) -> Option<otlyra_platform::accesskit::TreeUpdate> {
        if !self.accessibility_dirty {
            return None;
        }
        self.accessibility_dirty = false;

        // Rebuilt only after something that can change semantics, geometry or
        // focus. Paint-only animation leaves the last tree valid.
        let tab = self.tabs.get(self.active)?;
        let document = match tab.page.as_ref() {
            Some(page) => crate::a11y::tree_for(page, &tab.title),
            None => crate::a11y::empty_tree(&tab.title),
        };

        // With the interface hidden there is nothing over the page, so the page
        // is the whole window and wrapping it would add a level describing a
        // toolbar that was never drawn.
        if !self.interface {
            return Some(document);
        }

        let title = tab.title.clone();
        let system = tab.system;

        // The toolbar, and then whatever is under it. A browser page is drawn by
        // its own surface, so its controls come from that surface rather than
        // from the document tree, which for an `about:` page has nothing in it.
        let mut described = self.ui.describe();
        let (page_focus, page_described) = match system {
            Some(SystemPage::Settings) => (self.settings.focused(), self.settings.describe()),
            Some(SystemPage::History) => {
                (self.history_page.focused(), self.history_page.describe())
            }
            Some(SystemPage::Downloads) => (
                self.downloads_page.focused(),
                self.downloads_page.describe(),
            ),
            Some(SystemPage::Bookmarks) => (
                self.bookmarks_page.focused(),
                self.bookmarks_page.describe(),
            ),
            Some(SystemPage::Cookies) => {
                (self.cookies_page.focused(), self.cookies_page.describe())
            }
            Some(SystemPage::About) => (self.about.focused(), self.about.describe()),
            // The pages that are still a placeholder draw no controls, so they
            // describe none.
            _ => (None, Vec::new()),
        };
        described.extend(page_described);

        // Only the active root may publish keyboard focus. Background surfaces
        // can retain render state, but never a second accessibility focus.
        let focused = match self.keyboard_surface {
            SURFACE_CHROME => self.ui.focused(),
            SURFACE_SYSTEM => page_focus,
            SURFACE_PAGE | SURFACE_INSPECTOR => None,
            _ => None,
        };

        Some(crate::a11y::window_tree(
            &described, focused, document, &title,
        ))
    }

    fn cursor(&self) -> Cursor {
        self.cursor
    }

    fn window_appearance(&self) -> Option<otlyra_platform::ColorScheme> {
        match self.settings.settings.appearance {
            crate::settings::Appearance::System => None,
            crate::settings::Appearance::Light => Some(otlyra_platform::ColorScheme::Light),
            crate::settings::Appearance::Dark => Some(otlyra_platform::ColorScheme::Dark),
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let geom = self.frame_geom(viewport);

        // The page first, then the interface over it. The page is inset by the
        // interface's height and culled to what is visible, so it cannot paint
        // underneath it — but painting in this order means a future translucent
        // toolbar composites correctly rather than needing a clip.
        let page = self.page_list(&geom);
        render(&page, target);

        // The highlight goes over the page whether or not the panel is open. They
        // are two things: the overlay says *this element*, the panel says
        // everything about it.
        if let Some(highlight) = self.highlight_list(&geom) {
            render(&highlight, target);
        }

        if !self.interface {
            self.after_frame();
            return;
        }

        // The panel under the overlay, so a box that reaches the bottom of the
        // content area is covered by the dock rather than drawn over it.
        if geom.dock > 0.0 {
            let inspector = self.inspector_list(&geom);
            render(&inspector, target);
        }

        let chrome = self.chrome_list(&geom);
        render(&chrome, target);

        self.after_frame();
    }

    /// Publish the interface as retained layers so the compositor re-rasterizes
    /// and re-uploads only what moved.
    ///
    /// The layers are built by the same helpers `paint` renders, in the same
    /// order, so a full composite is pixel-for-pixel what `paint` would draw; the
    /// only addition is a device rectangle and content epoch per layer for the
    /// compositor's damage. Interface-less frames — a screenshot, `--no-interface`
    /// — keep the whole-surface path.
    fn compose(&mut self, viewport: Viewport) -> Option<Scene> {
        if !self.interface {
            return None;
        }
        let geom = self.frame_geom(viewport);

        // Device bands that tile the surface top to bottom with no seam: the
        // chrome above the content, the content, and the dock filling the rest.
        // An open browser menu is an overlay rather than a band, so its chrome
        // layer temporarily covers the viewport instead of clipping at the
        // toolbar's bottom edge.
        let dev = |value: f64| (value * geom.scale_factor).round() as u32;
        let content_top = dev(geom.top).min(viewport.height);
        let content_bottom = dev(geom.top + geom.content_height).min(viewport.height);
        let page_rect = LayerRect {
            x: 0,
            y: content_top,
            width: viewport.width,
            height: content_bottom.saturating_sub(content_top),
        };

        let mut layers = Vec::with_capacity(4);

        let page = self.page_list(&geom);
        // What the page says it changed, if it changed only part of itself: a
        // keystroke re-shapes one field, and the paragraphs around it keep the
        // pixels they have. Taken after the list is built, because building it is
        // what settles the answer.
        let page_dirty = self.page_dirty(&geom);
        layers.push(SceneLayer {
            id: LayerId(LAYER_PAGE),
            rect: page_rect,
            epoch: self.page_epoch(&geom),
            list: page,
            dirty: page_dirty,
        });

        if let Some(highlight) = self.highlight_list(&geom) {
            // The overlay draws within the content area and can spill a little
            // past a box's edges (labels, handles), so it claims the whole content
            // rect; a highlight move re-rasterizes the page under it, which only
            // happens while a person is walking the tree with the inspector.
            layers.push(SceneLayer {
                id: LayerId(LAYER_HIGHLIGHT),
                rect: page_rect,
                epoch: self.highlight_epoch(),
                list: highlight,
                dirty: None,
            });
        }

        if geom.dock > 0.0 {
            let inspector = self.inspector_list(&geom);
            layers.push(SceneLayer {
                id: LayerId(LAYER_INSPECTOR),
                rect: LayerRect {
                    x: 0,
                    y: content_bottom,
                    width: viewport.width,
                    height: viewport.height.saturating_sub(content_bottom),
                },
                epoch: self.inspector_epoch(),
                list: inspector,
                dirty: None,
            });
        }

        let chrome = self.chrome_list(&geom);
        let chrome_dirty = self.chrome_dirty(&geom);
        layers.push(SceneLayer {
            id: LayerId(LAYER_CHROME),
            rect: LayerRect {
                x: 0,
                y: 0,
                width: viewport.width,
                height: if self.ui.popup_open() {
                    viewport.height
                } else {
                    content_top
                },
            },
            epoch: self.chrome_epoch(),
            list: chrome,
            dirty: chrome_dirty,
        });

        self.after_frame();
        Some(Scene { layers })
    }
}

/// One node's box, as the overlay, the panel and a driver all need it.
pub struct Chosen {
    /// The border box, in window coordinates.
    pub border: crate::ui::Rect,
    /// What the style says its four edges are.
    pub edges: crate::inspector::BoxEdges,
    /// How wide its containing block is, for a percentage.
    pub containing: Option<f64>,
    /// Where its children's tracks fall, when it lays its children into any.
    pub tracks: Option<crate::inspector::Tracks>,
}

/// A layout rectangle in the interface's own geometry vocabulary.
fn to_rect(rect: otlyra_layout::Rect) -> crate::ui::Rect {
    crate::ui::Rect::new(
        f64::from(rect.x),
        f64::from(rect.y),
        f64::from(rect.width),
        f64::from(rect.height),
    )
}

/// Whether these modifiers are the platform's "open the inspector" pair.
///
/// Alt and the platform's own accelerator: ⌥⌘I on macOS, Ctrl-Alt-I elsewhere,
/// which is what a person's fingers already know.
fn inspector_modifier(modifiers: Modifiers) -> bool {
    #[cfg(target_os = "macos")]
    {
        modifiers.command
    }
    #[cfg(not(target_os = "macos"))]
    {
        modifiers.control
    }
}

#[cfg(test)]
mod system_page_tests {
    use super::*;
    use crate::fetcher::Loaded;
    use crate::ui::SystemPage;

    /// A loader that fails everything, so a test that reaches the network is a
    /// test that was wrong to.
    struct NoNetwork;

    impl Loader for NoNetwork {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Err(format!("nothing may be fetched in this test: {url}"))
        }
    }

    /// Press where the interface drew something, going through the whole path
    /// a person's click takes: the platform event, the interface's geometry,
    /// and whatever the browser makes of what comes back.
    fn press(browser: &mut Browser, x: f64, y: f64) {
        browser.on_event(PlatformEvent::PointerMoved { x, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    }

    /// Draw one frame at `width` by `height`, which is what gives the interface
    /// the geometry the next press is tested against.
    fn frame(browser: &mut Browser, width: f64, height: f64) {
        let viewport = Viewport {
            width: width as u32,
            height: height as u32,
            scale_factor: 1.0,
        };
        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, viewport);
    }

    // --- what a screen reader is handed -----------------------------------

    /// The identifiers `window_tree` hands out, in the order it hands them out.
    fn described_labels(browser: &mut Browser) -> Vec<String> {
        browser
            .accessibility()
            .expect("a tree")
            .nodes
            .into_iter()
            .filter_map(|(id, node)| crate::a11y::described_index(id).map(|index| (index, node)))
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_values()
            .map(|node| node.label().unwrap_or_default().to_owned())
            .collect()
    }

    /// One tree, with the toolbar over the document rather than beside it.
    #[test]
    fn the_tree_holds_the_interface_and_the_document_together() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);

        let labels = described_labels(&mut browser);
        assert!(
            labels.iter().any(|label| label == "New tab"),
            "the toolbar is not in the tree: {labels:?}"
        );
    }

    /// With no interface drawn there is nothing to wrap the page in, and a level
    /// describing a toolbar that was never drawn would be a level about nothing.
    #[test]
    fn a_browser_with_no_interface_hands_over_the_page_alone() {
        let mut browser = Browser::new(NoNetwork);
        browser.hide_interface();
        frame(&mut browser, 1000.0, 700.0);

        assert!(described_labels(&mut browser).is_empty());
    }

    /// The settings' own controls join the toolbar's, so a reader on the page
    /// finds the switches rather than an empty document.
    #[test]
    fn a_browser_page_describes_the_controls_it_drew() {
        let mut browser = Browser::new(NoNetwork);
        browser.open_system(SystemPage::Settings);
        frame(&mut browser, 1000.0, 700.0);

        let labels = described_labels(&mut browser);
        assert!(
            labels.iter().any(|label| label.starts_with("Text size")),
            "the settings' controls are not in the tree: {labels:?}"
        );
    }

    /// A press asked for by a reader does what a click on the same control does.
    #[test]
    fn a_reader_can_throw_a_switch_on_the_settings() {
        // Throwing a switch saves the preferences, and saving them must not reach
        // the file the person running the tests browses with. Nothing in this
        // binary loads them any more, so pointing the write somewhere else is the
        // whole of what this needs.
        //
        // SAFETY: set to one constant value, once, and only ever read by
        // `preferences::path` — never changed under a running read.
        unsafe {
            std::env::set_var(
                "OTLYRA_CONFIG_DIR",
                std::env::temp_dir().join("otlyra-tests"),
            )
        };
        std::fs::create_dir_all(std::env::temp_dir().join("otlyra-tests"))
            .expect("a place to save preferences");

        let mut browser = Browser::new(NoNetwork);
        browser.open_system(SystemPage::Settings);
        frame(&mut browser, 1000.0, 700.0);

        let before = browser.settings.settings.load_images;
        let update = browser.accessibility().expect("a tree");
        let (id, _) = update
            .nodes
            .iter()
            .find(|(id, node)| {
                crate::a11y::described_index(*id).is_some() && node.label() == Some("Load images")
            })
            .expect("the images switch");

        browser.on_event(PlatformEvent::AccessibilityRequest {
            node: *id,
            action: otlyra_platform::AccessibilityAction::Activate,
        });
        assert_ne!(
            browser.settings.settings.load_images, before,
            "the switch did not move"
        );
    }

    #[test]
    fn the_menu_opens_the_pages_that_exist_and_closes_over_the_ones_that_do_not() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);

        // The cogwheel is the last control on the toolbar, at its right end.
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        assert!(browser.ui().menu_open(), "the cogwheel opens the menu");

        // The panel hangs below the toolbar at the right-hand edge; its rows are
        // 30 tall under a heading, so this is the first of them.
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 34.0);

        assert!(
            !browser.ui().menu_open(),
            "choosing something closes the menu"
        );
        assert_eq!(
            browser.system_page(),
            Some(SystemPage::Settings),
            "the first row is the settings, and it opens them"
        );
    }

    #[test]
    fn the_history_row_opens_the_history() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame(&mut browser, 1000.0, 700.0);

        // The second row is History, and since W8 it is a real page.
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 65.0);
        assert!(!browser.ui().menu_open());
        assert_eq!(browser.system_page(), Some(SystemPage::History));
    }

    #[test]
    fn the_downloads_row_opens_the_downloads() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame(&mut browser, 1000.0, 700.0);

        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 127.0);
        assert!(!browser.ui().menu_open());
        assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
    }

    #[test]
    fn the_bookmarks_row_opens_the_bookmarks() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame(&mut browser, 1000.0, 700.0);

        // The third row. It was the dimmed one — the test that used to live here
        // proved that a press on a page that did not exist yet fell through to the
        // sheet and only dismissed the menu. Every row on this menu is now a real
        // page, so what is worth checking is that this one opens.
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 96.0);
        assert!(!browser.ui().menu_open());
        assert_eq!(browser.system_page(), Some(SystemPage::Bookmarks));
    }

    #[test]
    fn choosing_a_page_from_the_menu_opens_it_beside_what_was_being_read() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame(&mut browser, 1000.0, 700.0);

        // The first tab is blank, so the settings fill it rather than leaving an
        // empty tab behind.
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 34.0);
        assert_eq!(browser.tabs().len(), 1);
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));

        // From a tab that is showing something, a second one opens.
        browser.open_system_in_new_tab(SystemPage::About);
        assert_eq!(browser.tabs().len(), 2);
        assert_eq!(browser.active(), 1);
        assert_eq!(browser.system_page(), Some(SystemPage::About));
        assert_eq!(
            browser.tabs()[0].system,
            Some(SystemPage::Settings),
            "what was being read stayed where it was"
        );
    }

    #[test]
    fn typing_the_same_address_navigates_in_place() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:otlyra");
        browser.navigate("about:settings");

        assert_eq!(browser.tabs().len(), 1, "typing is a decision to leave");
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    }

    #[test]
    fn a_browser_page_belongs_to_its_tab_and_not_to_the_window() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:settings");
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));

        // A second tab is a second place, and it is not on the settings.
        browser.new_tab();
        assert_eq!(browser.system_page(), None);

        browser.select_tab(0);
        assert_eq!(
            browser.system_page(),
            Some(SystemPage::Settings),
            "the first tab kept what it was showing"
        );
    }

    #[test]
    fn a_browser_page_earns_a_history_entry_and_back_leaves_it() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:otlyra");
        browser.navigate("about:settings");
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
        assert!(browser.can_go_back());

        browser.go_back();
        assert_eq!(
            browser.system_page(),
            Some(SystemPage::About),
            "back reaches the browser page that was there"
        );

        browser.go_forward();
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    }

    #[test]
    fn done_on_the_settings_goes_back_rather_than_emptying_the_tab() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:otlyra");
        browser.navigate("about:settings");

        browser.handle_settings_action(&crate::settings::Action::Close);
        assert_eq!(
            browser.system_page(),
            Some(SystemPage::About),
            "done is back"
        );
    }

    #[test]
    fn done_with_nowhere_behind_it_empties_the_tab() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:settings");

        browser.handle_settings_action(&crate::settings::Action::Close);
        assert_eq!(browser.system_page(), None);
        assert_eq!(browser.tabs()[0].title, "New tab");
    }

    #[test]
    fn typing_a_browser_address_opens_a_surface_rather_than_fetching() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:settings");

        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
        assert_eq!(browser.tabs()[0].url, "about:settings");
        assert_eq!(browser.ui().address.text(), "about:settings");
        assert!(
            browser.tabs()[0].error.is_none(),
            "nothing was fetched, so nothing failed"
        );
    }

    #[test]
    fn the_spellings_a_person_might_type_all_arrive_at_the_same_page() {
        for spelling in ["about:settings", "About:Settings", "about:preferences/"] {
            let mut browser = Browser::new(NoNetwork);
            browser.navigate(spelling);
            assert_eq!(
                browser.system_page(),
                Some(SystemPage::Settings),
                "{spelling} should open the settings"
            );
        }

        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:otlyra");
        assert_eq!(browser.system_page(), Some(SystemPage::About));
    }

    #[test]
    fn native_commands_open_the_surfaces_they_name() {
        let mut settings = crate::settings::Settings::default();
        settings.home.set_text("about:otlyra");
        let mut browser = Browser::with_settings(NoNetwork, settings);

        browser.on_event(PlatformEvent::MenuCommand(crate::menu::Command::Home.id()));
        assert_eq!(browser.system_page(), Some(SystemPage::About));

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::Settings.id(),
        ));
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ShowHistory.id(),
        ));
        assert_eq!(browser.system_page(), Some(SystemPage::History));

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ShowDownloads.id(),
        ));
        assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
    }

    #[test]
    fn native_developer_tools_command_toggles_the_inspector() {
        let mut browser = Browser::new(NoNetwork);

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ToggleDevTools.id(),
        ));
        assert!(browser.inspector.open);
        assert_eq!(browser.keyboard_surface, SURFACE_INSPECTOR);

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ToggleDevTools.id(),
        ));
        assert!(!browser.inspector.open);
        assert_eq!(browser.keyboard_surface, browser.tab_surface());
    }

    #[test]
    fn downloads_address_opens_the_native_surface_without_fetching() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:downloads");

        assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
        assert_eq!(browser.tabs()[0].url, "about:downloads");
        assert!(browser.tabs()[0].error.is_none());
    }

    #[test]
    fn an_attachment_becomes_a_completed_download_instead_of_a_document() {
        struct Attachment;

        impl Loader for Attachment {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    bytes: b"id,name\n1,Ada\n".to_vec(),
                    content_type: Some("text/csv".to_owned()),
                    response_headers: vec![(
                        "content-disposition".to_owned(),
                        "attachment; filename=people.csv".to_owned(),
                    )],
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Attachment);
        browser.navigate("https://example.test/export");
        browser.wait_for_load(std::time::Duration::from_secs(5));

        assert_eq!(browser.system_page(), Some(SystemPage::Downloads));
        assert_eq!(browser.tabs()[0].url, "about:downloads");
        assert!(browser.tabs()[0].page.is_none());
        let download = browser
            .downloads
            .downloads()
            .next()
            .expect("the attachment was retained");
        assert_eq!(download.filename(), "people.csv");
        assert_eq!(download.content_type(), Some("text/csv"));
        assert_eq!(download.bytes(), b"id,name\n1,Ada\n");
    }

    /// With asking turned off, an attachment reaches the disk on its own — nobody
    /// presses anything, so the write has to start where the bytes arrive.
    #[test]
    fn an_attachment_saves_itself_when_the_preference_says_not_to_ask() {
        struct Attachment;

        impl Loader for Attachment {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    bytes: b"id,name\n1,Ada\n".to_vec(),
                    content_type: Some("text/csv".to_owned()),
                    response_headers: vec![(
                        "content-disposition".to_owned(),
                        "attachment; filename=people.csv".to_owned(),
                    )],
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        // A directory this test owns. The preference is set directly rather than
        // through the environment, because the environment is process-wide and the
        // rest of the suite is running beside this.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "otlyra-automatic-download-{}-{unique}",
            std::process::id()
        ));

        let mut settings = crate::settings::Settings::default();
        settings.apply(crate::settings::Action::ToggleDownloadAsk);
        settings.apply(crate::settings::Action::SetDownloadDirectory(
            directory.to_string_lossy().into_owned(),
        ));
        assert!(!settings.asks_where_to_save());

        let mut browser = Browser::with_settings(Attachment, settings);
        browser.navigate("https://example.test/export");
        browser.wait_for_load(std::time::Duration::from_secs(5));

        // The write is asynchronous, so the row is pending here and the file
        // arrives through `pump` — the same route the running browser takes.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let saved = loop {
            browser.pump();
            if let Some(saved) = browser
                .downloads
                .downloads()
                .next()
                .and_then(|download| download.saved_to())
            {
                break saved.to_owned();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the download never reached the disk: {:?}",
                browser.downloads.downloads().next().map(|download| (
                    download.saving_to().map(str::to_owned),
                    download.save_error().map(str::to_owned)
                ))
            );
            std::thread::yield_now();
        };

        assert_eq!(
            std::path::Path::new(&saved),
            directory.join("people.csv"),
            "the file went somewhere other than the download folder"
        );
        assert_eq!(
            std::fs::read(&saved).expect("the saved download"),
            b"id,name\n1,Ada\n"
        );
        std::fs::remove_dir_all(&directory).expect("remove the test-owned directory");
    }

    /// And with asking left on — the default — nothing is written without a person.
    #[test]
    fn an_attachment_waits_to_be_asked_about_by_default() {
        struct Attachment;

        impl Loader for Attachment {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    bytes: b"id,name\n1,Ada\n".to_vec(),
                    response_headers: vec![(
                        "content-disposition".to_owned(),
                        "attachment; filename=people.csv".to_owned(),
                    )],
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Attachment);
        assert!(browser.settings.settings.asks_where_to_save());
        browser.navigate("https://example.test/export");
        browser.wait_for_load(std::time::Duration::from_secs(5));
        browser.pump();

        let download = browser
            .downloads
            .downloads()
            .next()
            .expect("the attachment was retained");
        assert!(download.saved_to().is_none(), "it saved itself uninvited");
        assert!(download.saving_to().is_none());
        assert!(download.save_error().is_none());
    }

    #[test]
    fn bookmarks_address_opens_the_native_surface_without_fetching() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:bookmarks");

        assert_eq!(browser.system_page(), Some(SystemPage::Bookmarks));
        assert_eq!(browser.tabs()[0].url, "about:bookmarks");
        assert!(browser.tabs()[0].error.is_none());
    }

    /// ⌘D keeps the page, and ⌘D again stops keeping it. One command both ways,
    /// because that is what one key can mean.
    #[test]
    fn the_bookmark_command_keeps_the_page_and_then_drops_it() {
        struct Page;

        impl Loader for Page {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    bytes: b"<title>Kept</title><p>hello".to_vec(),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Page);
        browser.navigate("https://example.test/keep");
        browser.wait_for_load(std::time::Duration::from_secs(5));
        assert!(!browser.is_bookmarked());
        assert_eq!(browser.ui().bookmark, crate::ui::Bookmarked::No);

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ToggleBookmark.id(),
        ));
        assert!(browser.is_bookmarked());
        assert_eq!(
            browser.ui().bookmark,
            crate::ui::Bookmarked::Yes,
            "the star and the menu must know, or they offer to keep it twice"
        );
        let kept: Vec<(String, String)> = browser
            .bookmarks
            .bookmarks()
            .map(|bookmark| (bookmark.url.clone(), bookmark.title.clone()))
            .collect();
        assert_eq!(
            kept,
            [("https://example.test/keep".to_owned(), "Kept".to_owned())],
            "the document's own title names it"
        );

        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ToggleBookmark.id(),
        ));
        assert!(!browser.is_bookmarked());
        assert_eq!(browser.ui().bookmark, crate::ui::Bookmarked::No);
        assert!(browser.bookmarks.is_empty());
    }

    /// A hard reload empties the cache, which is what makes it the answer to a
    /// stylesheet a server is serving stale: the page and everything it then asks
    /// for are all fetched afresh.
    #[test]
    fn a_hard_reload_empties_the_cache_and_an_ordinary_one_does_not() {
        let cache: otlyra_net::SharedCache =
            std::sync::Arc::new(std::sync::Mutex::new(otlyra_net::cache::Cache::new()));
        let mut browser = Browser::new(NoNetwork);
        browser.set_cache(std::sync::Arc::clone(&cache));
        browser.navigate("https://example.test/");

        let put = |cache: &otlyra_net::SharedCache| {
            let stored = otlyra_net::cache::Stored {
                status: 200,
                headers: vec![("cache-control".to_owned(), "max-age=3600".to_owned())],
                body: b"body".to_vec(),
                final_url: "https://example.test/a".to_owned(),
                directives: otlyra_net::cache::Directives::parse(["max-age=3600"]),
                lifetime: otlyra_net::cache::Lifetime::Stated(std::time::Duration::from_secs(3600)),
                times: otlyra_net::cache::Times {
                    requested: std::time::SystemTime::now(),
                    received: std::time::SystemTime::now(),
                    date: std::time::SystemTime::now(),
                    age: std::time::Duration::ZERO,
                },
                varied: Vec::new(),
                varies_on_everything: false,
            };
            cache
                .lock()
                .expect("not poisoned")
                .store("https://example.test/a", "GET", stored, &[]);
        };

        put(&cache);
        assert_eq!(cache.lock().expect("not poisoned").len(), 1);
        browser.reload();
        assert_eq!(
            cache.lock().expect("not poisoned").len(),
            1,
            "an ordinary reload asks about what is kept rather than throwing it away"
        );

        browser.reload_ignoring_cache();
        assert!(
            cache.lock().expect("not poisoned").is_empty(),
            "and a hard one starts over"
        );
    }

    /// The mode is an instruction about one navigation. Left set, every later
    /// click would behave like a reload and the cache would answer nothing.
    #[test]
    fn a_reload_does_not_make_every_later_click_a_reload() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("https://example.test/");
        browser.reload();
        assert_eq!(
            browser.next_cache_mode,
            otlyra_net::CacheMode::Default,
            "the navigation it was set for took it"
        );
        browser.reload_ignoring_cache();
        assert_eq!(browser.next_cache_mode, otlyra_net::CacheMode::Default);
    }

    /// The cookies page is reachable the three ways every browser page is: by
    /// address, from the menu, and by the keyboard once it is open.
    #[test]
    fn the_cookies_page_opens_and_closes() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:cookies");
        assert_eq!(browser.system_page(), Some(SystemPage::Cookies));
        assert_eq!(browser.tabs()[0].url, "about:cookies");

        let mut fresh = Browser::new(NoNetwork);
        fresh.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ShowCookies.id(),
        ));
        assert_eq!(fresh.system_page(), Some(SystemPage::Cookies));
    }

    /// Throwing a site away throws that site away and leaves the rest.
    #[test]
    fn the_page_can_be_rid_of_one_site_or_of_everything() {
        let mut browser = Browser::new(NoNetwork);
        let now = std::time::SystemTime::now();
        browser.cookies.with(|jar| {
            for address in ["https://one.test/", "https://two.test/"] {
                jar.set(&url::Url::parse(address).expect("a url"), "a=1", now)
                    .expect("kept");
            }
        });
        assert_eq!(browser.cookies.with(|jar| jar.len()), 2);

        browser.handle_cookies_action(crate::cookies::Action::ClearSite("one.test".into()));
        assert_eq!(browser.cookies.with(|jar| jar.len()), 1);
        browser.handle_cookies_action(crate::cookies::Action::Clear);
        assert!(browser.cookies.with(|jar| jar.is_empty()));
    }

    /// The switch in the preferences is the switch in the jar. Two places that
    /// could disagree about whether a cookie is refused would be one place too
    /// many.
    #[test]
    fn the_third_party_switch_reaches_the_jar() {
        let mut browser = Browser::new(NoNetwork);
        assert!(
            browser.cookies.with(|jar| jar.accepts_third_party()),
            "the default is what every browser still ships"
        );

        browser
            .settings
            .settings
            .apply(settings::Action::ToggleThirdPartyCookies);
        browser.handle_settings_action(&settings::Action::ToggleThirdPartyCookies);
        assert!(!browser.cookies.with(|jar| jar.accepts_third_party()));

        // And a browser built from preferences that already say so starts that
        // way, rather than only after somebody presses the switch again.
        let mut settings = crate::settings::Settings::default();
        settings.block_third_party_cookies = true;
        let started = Browser::with_fetcher(crate::fetcher::Fetcher::spawn(NoNetwork), settings);
        assert!(!started.cookies.with(|jar| jar.accepts_third_party()));
    }

    /// A blank tab has no address, and a bookmark that opens nowhere is worse than
    /// no bookmark.
    #[test]
    fn the_bookmark_command_does_nothing_on_a_blank_tab() {
        let mut browser = Browser::new(NoNetwork);
        assert_eq!(
            browser.ui().bookmark,
            crate::ui::Bookmarked::Impossible,
            "and the star says as much rather than looking pressable"
        );
        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ToggleBookmark.id(),
        ));
        assert!(browser.bookmarks.is_empty());
    }

    #[test]
    fn the_native_bookmarks_command_opens_the_page() {
        let mut browser = Browser::new(NoNetwork);
        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ShowBookmarks.id(),
        ));
        assert_eq!(browser.system_page(), Some(SystemPage::Bookmarks));
    }

    /// Pressing a row goes there, and the address bar agrees that this is a page
    /// the reader kept.
    #[test]
    fn a_kept_page_can_be_opened_from_the_list_again() {
        struct Page;

        impl Loader for Page {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    bytes: b"<title>Kept</title>".to_vec(),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Page);
        browser.bookmarks.add("https://example.test/keep", "Kept");
        browser.handle_bookmarks_action(crate::bookmarks::Action::Open(
            "https://example.test/keep".to_owned(),
        ));
        browser.wait_for_load(std::time::Duration::from_secs(5));

        assert_eq!(browser.tabs()[0].url, "https://example.test/keep");
        assert_eq!(
            browser.ui().bookmark,
            crate::ui::Bookmarked::Yes,
            "arriving at a kept page must fill the star"
        );
    }

    #[test]
    fn leaving_the_settings_leaves_the_tab_blank_rather_than_still_on_them() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:settings");
        browser.handle_settings_action(&crate::settings::Action::Close);

        assert_eq!(browser.system_page(), None);
        assert_eq!(browser.ui().address.text(), "");
        assert_eq!(browser.tabs()[0].title, "New tab");
    }
}

/// Ask the machine for files, where the machine has a way of asking.
///
/// The dialogue is modal and blocks this thread while it is up, which is what a
/// file dialogue is everywhere: nothing else in the window can be answered until
/// the reader has chosen or dismissed it.
///
/// The bytes are read here rather than remembered as a path, because a form is
/// sent long after the dialogue closed and by then the file may have moved: what
/// was chosen is what is sent. A file too large to hold is not offered at all —
/// this is the one place the browser reads a whole file into memory, and it is
/// worth saying out loud rather than discovering.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn choose_files(request: &crate::page::FileRequest) -> Vec<otlyra_dom::form::ChosenFile> {
    /// The most of one file the browser will hold.
    const LARGEST: u64 = 256 * 1024 * 1024;

    let mut dialogue = rfd::FileDialog::new();
    // Extensions are the only hint the dialogue takes; a media type or a
    // `image/*` is a hint about kinds it has no list for, so those are left to it.
    let extensions: Vec<&str> = request
        .accept
        .iter()
        .filter_map(|hint| hint.strip_prefix('.'))
        .collect();
    if !extensions.is_empty() {
        dialogue = dialogue.add_filter("Accepted", &extensions);
    }
    let paths = if request.many {
        dialogue.pick_files().unwrap_or_default()
    } else {
        dialogue.pick_file().into_iter().collect()
    };

    paths
        .into_iter()
        .filter_map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match std::fs::metadata(&path) {
                Ok(about) if about.len() > LARGEST => {
                    tracing::warn!(file = %name, size = about.len(), "the file is too large to send");
                    return None;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(file = %name, %error, "the file could not be read");
                    return None;
                }
            }
            let bytes = std::fs::read(&path)
                .inspect_err(|error| tracing::warn!(file = %name, %error, "the file could not be read"))
                .ok()?;
            Some(otlyra_dom::form::ChosenFile {
                media_type: otlyra_dom::form::media_type_of(&name),
                name,
                bytes,
            })
        })
        .collect()
}

/// The same, where there is nothing to ask.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn choose_files(_request: &crate::page::FileRequest) -> Vec<otlyra_dom::form::ChosenFile> {
    tracing::debug!("no file dialogue on this platform; the picker keeps what it held");
    Vec::new()
}

/// Ask where one completed attachment should be written.
///
/// The dialogue itself is modal by platform convention. The write starts only
/// after it closes and runs through [`downloads::DownloadWriter`], never on the
/// browser thread.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn choose_download_path(
    filename: &str,
    directory: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let mut dialogue = rfd::FileDialog::new().set_file_name(filename);
    // Only a directory that is there: a dialogue told to open somewhere that does
    // not exist opens somewhere of its own choosing on some platforms and refuses
    // on others, and neither is what the preference meant.
    if let Some(directory) = directory.filter(|directory| directory.is_dir()) {
        dialogue = dialogue.set_directory(directory);
    }
    dialogue.save_file()
}

/// The same, where the platform has no native file dialogue.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn choose_download_path(
    _filename: &str,
    _directory: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    tracing::debug!("no save dialogue on this platform; the attachment remains in the session");
    None
}

/// Ask which folder automatic downloads should go into.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn choose_download_directory(current: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let mut dialogue = rfd::FileDialog::new();
    if let Some(current) = current.filter(|current| current.is_dir()) {
        dialogue = dialogue.set_directory(current);
    }
    dialogue.pick_folder()
}

/// The same, where there is nothing to ask.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn choose_download_directory(_current: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    tracing::debug!("no folder dialogue on this platform; the download folder is unchanged");
    None
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::fetcher::Loaded;

    /// The smallest PNG that decodes: one opaque pixel.
    const ONE_PIXEL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// What a fake loader was asked for, shared because the loader itself lives on
    /// the fetch thread and a test cannot reach into it.
    type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// A loader that serves canned pages, so navigation can be tested without a
    /// socket — including the failure path, which a real server makes awkward.
    #[derive(Default)]
    struct FakeLoader {
        requested: Requests,
    }

    impl Loader for FakeLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            match url {
                "broken.example" => Err("could not fetch broken.example".to_owned()),
                // A `file:` URL loads as itself; anything else becomes an https
                // address, the way a bare hostname does.
                _ if url.starts_with("file://") => Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: format!("<title>Local</title><body><p>Body of {url}").into_bytes(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                }),
                // A bare hostname becomes an https address, the way the real
                // loader normalizes one; an address that already is one is left
                // alone, or going back to it would grow a second scheme each time.
                _ => {
                    let final_url = if url.contains("://") {
                        url.to_owned()
                    } else {
                        format!("https://{url}/")
                    };
                    Ok(Loaded {
                        content_type: Some("text/html".to_owned()),
                        bytes: format!("<title>Title of {url}</title><body><p>Body of {url}")
                            .into_bytes(),
                        charset: Some("utf-8".to_owned()),
                        final_url,
                        ..Default::default()
                    })
                }
            }
        }
    }

    pub(super) fn browser() -> Browser {
        browser_with_log().0
    }

    /// A browser and the list of what its loader was asked for.
    fn browser_with_log() -> (Browser, Requests) {
        let requested = Requests::default();
        let loader = FakeLoader {
            requested: std::sync::Arc::clone(&requested),
        };
        (Browser::new(loader), requested)
    }

    #[test]
    fn a_repeated_pointer_position_requests_no_frame() {
        let mut browser = browser();
        let event = PlatformEvent::PointerMoved { x: 320.0, y: 240.0 };

        assert_eq!(browser.handle_event(event), FrameRequest::Now);
        assert_eq!(
            browser.handle_event(event),
            FrameRequest::None,
            "identical input changes neither hover nor drag geometry"
        );
    }

    #[test]
    fn pointer_motion_across_an_unchanged_page_requests_no_frame() {
        let mut browser = browser();
        assert_eq!(
            browser.handle_event(PlatformEvent::PointerMoved { x: 320.0, y: 240.0 }),
            FrameRequest::Now,
            "leaving the initial off-window position clears any chrome hover"
        );
        assert_eq!(
            browser.handle_event(PlatformEvent::PointerMoved { x: 420.0, y: 340.0 }),
            FrameRequest::None,
            "the cursor moved, but no pixels changed"
        );
    }

    #[test]
    fn only_the_active_loading_tab_drives_vsync() {
        let mut browser = browser();
        browser.navigate("example.com");
        assert_eq!(browser.next_frame(), FrameRequest::Vsync);

        browser.new_tab();
        assert_eq!(
            browser.next_frame(),
            FrameRequest::None,
            "a background load wakes on model changes instead of repainting continuously"
        );
    }

    #[test]
    fn a_static_browser_page_has_no_follow_up_frame() {
        let mut browser = browser();
        browser.open_system(SystemPage::About);
        assert_eq!(browser.next_frame(), FrameRequest::None);
    }

    #[test]
    fn an_unchanged_frame_does_not_rebuild_accessibility() {
        let mut browser = browser();
        let mut target = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));

        assert!(
            browser.accessibility().is_some(),
            "the first tree is published"
        );
        assert!(
            browser.accessibility().is_none(),
            "and remains valid until semantics, geometry or focus changes"
        );

        let _ = browser.handle_event(PlatformEvent::PointerMoved { x: 300.0, y: 300.0 });
        let request = browser.handle_event(PlatformEvent::PointerMoved { x: 400.0, y: 300.0 });
        assert_eq!(request, FrameRequest::None);
        assert!(
            browser.accessibility().is_none(),
            "paint-free pointer motion changes no accessibility nodes"
        );
    }

    /// Wait for whatever was asked for to arrive.
    ///
    /// Loading happens on another thread and the window is woken when it finishes;
    /// a test has no window, so it waits instead.
    fn settle(browser: &mut Browser) {
        browser.wait_for_load(std::time::Duration::from_secs(5));
    }

    /// Navigate and wait, which is what every test means by "load this".
    pub(super) fn go(browser: &mut Browser, url: &str) {
        browser.navigate(url);
        settle(browser);
    }

    fn asked_for(requests: &Requests) -> Vec<String> {
        requests
            .lock()
            .expect("no panic on the fetch thread")
            .clone()
    }

    fn type_url(browser: &mut Browser, url: &str) {
        // A frame first: the address field's focus id is its place in the order
        // a frame built, so until one has been drawn there is no field to put a
        // caret in. This is the same rule presses follow.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        browser.ui.focus_address();
        for character in url.chars() {
            browser.on_event(PlatformEvent::TextInput(character));
        }
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Enter,
            modifiers: Modifiers::default(),
        });
        settle(browser);
    }

    #[test]
    fn typing_an_address_and_pressing_enter_loads_it() {
        let (mut browser, requested) = browser_with_log();
        type_url(&mut browser, "example.com");

        assert_eq!(asked_for(&requested), ["example.com"]);
        assert_eq!(browser.tabs[0].title, "Title of example.com");
        assert!(browser.tabs[0].page.is_some());
    }

    /// One navigation, one visit — and the visit is where the load *ended up*.
    /// The loader normalizes `example.com` to `https://example.com/` the way a
    /// redirect would move it, and only the final address is recorded.
    #[test]
    fn a_navigation_lands_in_the_history_once_with_its_final_url() {
        let mut browser = browser();
        go(&mut browser, "example.com");
        let urls: Vec<&str> = browser
            .history
            .visits()
            .map(|visit| visit.url.as_str())
            .collect();
        assert_eq!(urls, ["https://example.com/"]);

        // The same address again moved nowhere, so it is not a second visit.
        go(&mut browser, "https://example.com/");
        assert_eq!(browser.history.visits().count(), 1);

        // And going back re-reads a place already recorded.
        go(&mut browser, "https://two.example/");
        browser.go_back();
        settle(&mut browser);
        assert_eq!(
            browser.history.visits().count(),
            2,
            "back re-reads, it does not re-visit"
        );
    }

    #[test]
    fn a_press_on_a_history_row_navigates_there() {
        let mut browser = browser();
        go(&mut browser, "example.com");
        browser.open_system(SystemPage::History);

        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(900, 700, 1.0));

        // Walk down the list area until a press lands on the visit's row. The
        // frame was drawn once and presses are tested against it, which is the
        // same rule every surface test follows.
        let navigated = ((UI_HEIGHT as u32 + 120)..680).step_by(4).any(|y| {
            browser.on_event(PlatformEvent::PointerMoved {
                x: 300.0,
                y: f64::from(y),
            });
            browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
            settle(&mut browser);
            browser.system_page().is_none()
        });
        assert!(navigated, "a visit's row navigates when pressed");
        assert_eq!(browser.ui.address.text(), "https://example.com/");
    }

    /// The address bar shows where the load ended up, not what was typed: a
    /// redirect that leaves the old text in place is a lie about what is on screen.
    #[test]
    fn the_address_bar_shows_the_final_url() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");
        assert_eq!(browser.ui.address.text(), "https://example.com/");
    }

    /// The whole point of the *System* default: the platform saying "dark now"
    /// is enough, with no restart and nothing saved.
    #[test]
    fn the_interface_follows_the_system_appearance_without_a_restart() {
        use crate::widget::theme::Theme;
        let mut browser = browser();
        assert_eq!(browser.ui.theme, Theme::light());

        browser.on_event(PlatformEvent::AppearanceChanged(
            otlyra_platform::ColorScheme::Dark,
        ));
        assert_eq!(browser.ui.theme, Theme::dark());
        assert_eq!(browser.settings.theme, Theme::dark());
        assert_eq!(browser.about.theme, Theme::dark());
    }

    /// A person who chose a palette chose it over the platform's opinion.
    #[test]
    fn a_chosen_appearance_outranks_the_system() {
        use crate::widget::theme::Theme;
        let mut browser = browser();
        browser
            .settings
            .settings
            .apply(settings::Action::SetAppearance(settings::Appearance::Light));
        browser.apply_theme();

        browser.on_event(PlatformEvent::AppearanceChanged(
            otlyra_platform::ColorScheme::Dark,
        ));
        assert_eq!(
            browser.ui.theme,
            Theme::light(),
            "Light means light, whatever the platform says"
        );
    }

    #[test]
    fn a_failed_load_keeps_the_tab_and_says_what_happened() {
        let mut browser = browser();
        type_url(&mut browser, "broken.example");

        assert_eq!(browser.tabs.len(), 1);
        assert!(browser.tabs[0].page.is_none());
        assert!(
            browser.tabs[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("broken.example"))
        );
    }

    #[test]
    fn tabs_are_opened_selected_and_closed() {
        let mut browser = browser();
        type_url(&mut browser, "first.example");

        browser.new_tab();
        assert_eq!(browser.tabs.len(), 2);
        assert_eq!(browser.active, 1);
        type_url(&mut browser, "second.example");

        browser.select_tab(0);
        assert_eq!(browser.active, 0);
        assert_eq!(
            browser.ui.address.text(),
            "https://first.example/",
            "switching tabs puts that tab's address back"
        );

        browser.close_tab(0);
        assert_eq!(browser.tabs.len(), 1);
        assert_eq!(browser.tabs[0].title, "Title of second.example");
    }

    #[test]
    fn closing_the_last_tab_empties_it_rather_than_leaving_no_tabs() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");
        browser.close_tab(0);

        assert_eq!(browser.tabs.len(), 1);
        assert!(browser.tabs[0].page.is_none());
        assert_eq!(browser.ui.address.text(), "");
    }

    /// Each tab scrolls independently: a scroll in one is not a scroll in another.
    #[test]
    fn scrolling_belongs_to_the_active_tab() {
        let mut browser = browser();
        type_url(&mut browser, "long.example");
        browser.new_tab();
        type_url(&mut browser, "other.example");

        // Paint so both pages have a layout to clamp a scroll against.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.ui.pointer_moved(400.0, 400.0, &mut browser.text);
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 50.0,
            source: otlyra_platform::ScrollSource::Wheel,
            modifiers: Default::default(),
        });

        let active = browser.active;
        assert_eq!(
            browser.tabs[1 - active]
                .page
                .as_ref()
                .expect("page")
                .scroll(),
            0.0
        );
    }

    #[test]
    fn a_scroll_over_the_interface_does_not_scroll_the_page() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.ui.pointer_moved(400.0, 10.0, &mut browser.text);
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 100.0,
            source: otlyra_platform::ScrollSource::Wheel,
            modifiers: Default::default(),
        });
        assert_eq!(browser.tabs[0].page.as_ref().expect("page").scroll(), 0.0);
    }

    /// Clicking a link navigates, and the address it navigates to is resolved
    /// against the page the link was on — a relative href is meaningless otherwise.
    #[test]
    fn clicking_a_link_navigates_to_it() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);

        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        let (x, y) = link_position(&browser);
        browser.on_event(PlatformEvent::PointerMoved { x, y });
        assert_eq!(
            browser.cursor(),
            Cursor::Pointer,
            "the pointer says so first"
        );

        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        assert_eq!(browser.tabs[0].url, "https://start.example/next");
    }

    /// Dragging a tab along the strip reorders the browser's own tabs, and the
    /// tab being read stays the tab being read wherever it lands.
    #[test]
    fn dragging_a_tab_reorders_the_strip() {
        let mut browser = Browser::new(LinkLoader);
        browser.new_tab();
        browser.new_tab();
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(1000, 700, 1.0),
        );
        let order: Vec<TabId> = browser.tabs.iter().map(|tab| tab.id).collect();
        assert_eq!(browser.tabs[browser.active].id, order[2]);

        let places = browser.ui().tab_places();
        let rect_of = |id: TabId| {
            places
                .borrow()
                .iter()
                .find(|(key, _)| *key == id.0)
                .map(|(_, rect)| *rect)
                .expect("every tab was drawn")
        };
        let last = rect_of(order[2]);
        let first = rect_of(order[0]);

        // Press the last tab and carry it to the front.
        browser.on_event(PlatformEvent::PointerMoved {
            x: last.x + last.width / 2.0,
            y: last.y + last.height / 2.0,
        });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        browser.on_event(PlatformEvent::PointerMoved {
            x: first.x + first.width / 2.0,
            y: last.y + last.height / 2.0,
        });
        browser.on_event(PlatformEvent::PointerReleased);

        assert_eq!(
            browser.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![order[2], order[0], order[1]],
            "the dragged tab did not land where it was dropped"
        );
        assert_eq!(
            browser.tabs[browser.active].id, order[2],
            "the tab being read must still be the tab being read"
        );
    }

    /// Put the caret in the address bar the way a reader does, so the browser
    /// knows the chrome is what the keyboard belongs to.
    fn name_the_address_bar(browser: &mut Browser) {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Character('l'),
            modifiers: Modifiers {
                command: cfg!(target_os = "macos"),
                control: !cfg!(target_os = "macos"),
                ..Modifiers::default()
            },
        });
        assert!(browser.ui().address_focused(), "the caret is in the field");
    }

    /// Tab walks the document: links and fields, in the order they are written,
    /// with the ring following. Without it a reader without a pointer cannot
    /// reach anything on a page at all.
    #[test]
    fn tab_walks_the_page_and_leaves_it_at_the_end() {
        struct TwoLinks;

        impl Loader for TwoLinks {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<title>Two</title><body>\
                        <p><a href=\"/first\">first</a> and <a href=\"/second\">second</a></p>\
                        <input id=field value=\"typed\">"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(TwoLinks);
        go(&mut browser, "https://walk.example/");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(900, 700, 1.0),
        );
        // A press on the page is what makes the document the active surface,
        // the same way a reader starts reading before they start walking.
        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        browser.on_event(PlatformEvent::PointerReleased);

        let tab = |browser: &mut Browser, shift: bool| {
            browser.on_event(PlatformEvent::KeyPressed {
                key: Key::Tab,
                modifiers: Modifiers {
                    shift,
                    ..Modifiers::default()
                },
            });
        };
        let focused_link = |browser: &Browser| {
            browser.tabs[browser.active]
                .page
                .as_ref()
                .and_then(PageScene::focused_link)
        };

        tab(&mut browser, false);
        assert_eq!(
            focused_link(&browser).as_deref(),
            Some("/first"),
            "Tab reached nothing, so the page cannot be walked at all"
        );
        tab(&mut browser, false);
        assert_eq!(focused_link(&browser).as_deref(), Some("/second"));
        tab(&mut browser, true);
        assert_eq!(
            focused_link(&browser).as_deref(),
            Some("/first"),
            "shift-Tab walks back the way it came"
        );

        // Return on a link the keyboard reached follows it, resolved against
        // the page it was on.
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Enter,
            modifiers: Modifiers::default(),
        });
        settle(&mut browser);
        assert_eq!(
            browser.tabs[browser.active].url,
            "https://walk.example/first"
        );
    }

    /// And back again: at the end of the toolbar's own order the keyboard
    /// returns to the document, entering from the end the reader is coming in
    /// at. Without this the chrome is its own trap and the page is unreachable
    /// once the keyboard has left it.
    #[test]
    fn tab_off_the_end_of_the_chrome_goes_back_into_the_page() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(900, 700, 1.0),
        );

        // Walk the chrome until the keyboard runs off its end, which is where
        // the document begins again.
        let mut entered = false;
        for _ in 0..40 {
            browser.on_event(PlatformEvent::KeyPressed {
                key: Key::Tab,
                modifiers: Modifiers::default(),
            });
            if browser.keyboard_surface == SURFACE_PAGE {
                entered = true;
                break;
            }
        }
        assert!(entered, "the toolbar kept the keyboard to itself");
        assert_eq!(
            browser.tabs[browser.active]
                .page
                .as_ref()
                .and_then(PageScene::focused_link)
                .as_deref(),
            Some("/next"),
            "coming in forwards lands on the first thing in the document"
        );
    }

    /// `tabindex` orders a page's own traversal: a positive value comes before
    /// everything the browser would have walked by default, and a negative one
    /// is reachable but never walked to.
    #[test]
    fn tabindex_orders_the_walk_and_takes_things_out_of_it() {
        struct Indexed;

        impl Loader for Indexed {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<title>Order</title><body>\
                        <a href=\"/plain\">plain</a>\
                        <a href=\"/skipped\" tabindex=\"-1\">skipped</a>\
                        <a href=\"/first\" tabindex=\"1\">first</a>"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Indexed);
        go(&mut browser, "https://order.example/");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(900, 700, 1.0),
        );
        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        browser.on_event(PlatformEvent::PointerReleased);

        let mut reached = Vec::new();
        for _ in 0..2 {
            browser.on_event(PlatformEvent::KeyPressed {
                key: Key::Tab,
                modifiers: Modifiers::default(),
            });
            reached.push(
                browser.tabs[browser.active]
                    .page
                    .as_ref()
                    .and_then(PageScene::focused_link),
            );
        }

        assert_eq!(
            reached,
            vec![Some("/first".to_owned()), Some("/plain".to_owned())],
            "a positive tabindex comes first and a negative one is not walked to"
        );
    }

    /// Past the last thing on the page, the keyboard goes to the browser around
    /// it: a document that trapped Tab would be a document a reader could not
    /// leave without a pointer.
    #[test]
    fn tab_past_the_end_of_the_page_reaches_the_interface() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(900, 700, 1.0),
        );
        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        browser.on_event(PlatformEvent::PointerReleased);

        // The page the loader serves has one link, so two steps run off it.
        for _ in 0..2 {
            browser.on_event(PlatformEvent::KeyPressed {
                key: Key::Tab,
                modifiers: Modifiers::default(),
            });
        }

        assert!(
            browser.ui().focused().is_some(),
            "the keyboard left the page and landed nowhere"
        );
        assert!(
            browser.tabs[browser.active]
                .page
                .as_ref()
                .and_then(PageScene::focused_link)
                .is_none(),
            "the page kept the focus it handed on"
        );
    }

    /// Typing in the omnibox offers where the reader has been and what they
    /// kept, and Return takes what the arrows reached rather than what was
    /// typed — which is the whole reason to offer anything.
    #[test]
    fn typing_offers_places_and_the_arrows_take_one() {
        let (mut browser, requests) = browser_with_log();
        go(&mut browser, "start.example");
        browser
            .bookmarks
            .add("https://kept.example/page", "A kept page");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(1000, 700, 1.0),
        );

        name_the_address_bar(&mut browser);
        browser.ui.address.clear();
        for character in "example".chars() {
            browser.on_event(PlatformEvent::TextInput(character));
        }
        assert!(
            browser.ui().suggesting(),
            "typing something both stores know offered nothing"
        );

        // A kept page comes before a place merely visited: it was kept because
        // the reader meant to come back to it.
        let offered: Vec<String> = browser
            .ui()
            .suggestions()
            .iter()
            .map(|row| row.url.clone())
            .collect();
        assert_eq!(
            offered.first().map(String::as_str),
            Some("https://kept.example/page")
        );
        assert!(offered.iter().any(|url| url.contains("start.example")));
        assert!(browser.ui().suggestions()[0].kept);

        // Down marks the first row without touching what was typed, and Return
        // takes the marked one.
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Down,
            modifiers: Modifiers::default(),
        });
        assert_eq!(
            browser.ui().address.text(),
            "example",
            "walking the list rewrote the field"
        );
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Enter,
            modifiers: Modifiers::default(),
        });
        settle(&mut browser);
        assert_eq!(
            asked_for(&requests).last().map(String::as_str),
            Some("https://kept.example/page"),
            "Return took what was typed rather than what the arrows reached"
        );
        assert!(!browser.ui().suggesting(), "the list outlived the choice");
    }

    /// Escape puts the list away and leaves the caret where it was.
    #[test]
    fn escape_puts_the_offered_places_away_and_keeps_the_caret() {
        let (mut browser, _requests) = browser_with_log();
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(1000, 700, 1.0),
        );
        name_the_address_bar(&mut browser);
        browser.ui.address.clear();
        for character in "start".chars() {
            browser.on_event(PlatformEvent::TextInput(character));
        }
        assert!(browser.ui().suggesting());

        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Escape,
            modifiers: Modifiers::default(),
        });
        assert!(!browser.ui().suggesting(), "Escape left the list showing");
        assert!(
            browser.ui().address_focused(),
            "Escape took the caret out of the field as well as the list"
        );
    }

    /// A press on the page while the list is showing is a press on the page:
    /// the list has no sheet, so it dismisses on the way through rather than
    /// swallowing the click the reader meant.
    #[test]
    fn a_press_on_the_page_goes_through_the_suggestions() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(1000, 700, 1.0),
        );
        name_the_address_bar(&mut browser);
        browser.ui.address.clear();
        for character in "start".chars() {
            browser.on_event(PlatformEvent::TextInput(character));
        }
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(1000, 700, 1.0),
        );
        assert!(browser.ui().suggesting());

        let (x, y) = link_position(&browser);
        browser.on_event(PlatformEvent::PointerMoved { x, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        settle(&mut browser);

        assert!(!browser.ui().suggesting(), "the list stayed over the page");
        assert_eq!(
            browser.tabs[browser.active].url, "https://start.example/next",
            "the press that dismissed the list never reached the link"
        );
    }

    /// Ask for a menu where the pointer is, the way the platform does.
    fn ask_for_a_menu(browser: &mut Browser, x: f64, y: f64) {
        browser.on_event(PlatformEvent::PointerMoved { x, y });
        browser.on_event(PlatformEvent::ContextMenuRequested);
    }

    /// What the open context menu offers, in the order it offers it.
    fn context_rows(browser: &Browser) -> Vec<ContextCommand> {
        browser
            .ui()
            .describe()
            .into_iter()
            .filter(|node| node.role == crate::widget::Role::MenuItem)
            .filter_map(|node| {
                [
                    ContextCommand::OpenLinkInNewTab,
                    ContextCommand::CopyLinkAddress,
                    ContextCommand::CopySelection,
                    ContextCommand::SelectAll,
                    ContextCommand::Back,
                    ContextCommand::Forward,
                    ContextCommand::Reload,
                    ContextCommand::InspectElement,
                ]
                .into_iter()
                .find(|command| command.label() == node.label)
            })
            .collect()
    }

    /// A menu asked for over a link offers what can be done with a link, and
    /// what it offers is decided from the document rather than guessed.
    #[test]
    fn a_menu_asked_for_over_a_link_offers_the_link() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );

        let (x, y) = link_position(&browser);
        ask_for_a_menu(&mut browser, x, y);
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );

        let rows = context_rows(&browser);
        assert_eq!(
            rows.first(),
            Some(&ContextCommand::OpenLinkInNewTab),
            "the link is what the reader asked about, so it comes first"
        );
        assert!(rows.contains(&ContextCommand::CopyLinkAddress));
        assert!(rows.contains(&ContextCommand::InspectElement));
        assert!(
            !rows.contains(&ContextCommand::CopySelection),
            "nothing is selected, so there is nothing to copy"
        );

        // And choosing the first row opens the link the press landed on — in a
        // tab of its own, with the one being read left where it was.
        let before = browser.tabs.len();
        let panel = browser
            .ui()
            .describe()
            .into_iter()
            .position(|node| node.label == ContextCommand::OpenLinkInNewTab.label())
            .expect("the row that was just described");
        let action = browser.ui.activate_described(panel, &mut browser.text);
        browser.apply(action);
        settle(&mut browser);

        assert_eq!(browser.tabs.len(), before + 1);
        assert_eq!(browser.tabs[before].url, "https://start.example/next");
        assert_eq!(browser.tabs[0].url, "https://start.example/");
        assert!(!browser.ui().popup_open(), "choosing a row closes the menu");
    }

    /// Away from a link the same menu is the page's own, and Back says whether
    /// there is anywhere to go back to rather than pretending there is.
    #[test]
    fn a_menu_over_the_page_offers_what_the_page_can_do() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );

        ask_for_a_menu(&mut browser, 700.0, 500.0);
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );

        let rows = context_rows(&browser);
        assert_eq!(
            rows,
            vec![
                ContextCommand::Back,
                ContextCommand::Forward,
                ContextCommand::Reload,
                ContextCommand::SelectAll,
                ContextCommand::InspectElement,
            ]
        );
        let back = browser
            .ui()
            .describe()
            .into_iter()
            .find(|node| node.label == ContextCommand::Back.label())
            .expect("the back row");
        assert!(
            !back.enabled,
            "there is nowhere to go back to, and a row that says otherwise is a lie"
        );
    }

    /// A press for a menu never reaches the page behind it: it does not follow
    /// the link it lands on, and it does not start a selection.
    #[test]
    fn asking_for_a_menu_over_a_link_does_not_follow_it() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );

        let (x, y) = link_position(&browser);
        ask_for_a_menu(&mut browser, x, y);
        settle(&mut browser);

        assert_eq!(browser.tabs[0].url, "https://start.example/");
        assert!(browser.ui().popup_open());
    }

    /// The document the menu was asked about is gone, and so are its rows.
    #[test]
    fn navigating_puts_away_a_menu_asked_for_on_the_page() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );
        ask_for_a_menu(&mut browser, 700.0, 500.0);
        assert!(browser.ui().popup_open());

        browser.navigate("start.example/elsewhere");
        assert!(
            !browser.ui().popup_open(),
            "the menu outlived the page it described"
        );
        settle(&mut browser);
    }

    /// So is a menu whose window has just been resized under it.
    #[test]
    fn resizing_puts_away_a_menu_asked_for_on_the_page() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );
        ask_for_a_menu(&mut browser, 700.0, 500.0);
        assert!(browser.ui().popup_open());

        browser.on_event(PlatformEvent::Resized(Viewport::new(640, 480, 1.0)));
        assert!(!browser.ui().popup_open());
    }

    /// A menu asked for over the interface is not the page's menu: the browser
    /// has its own, and offering "Inspect Element" over the toolbar would be
    /// offering to inspect something the press did not land on.
    #[test]
    fn asking_for_a_menu_over_the_toolbar_offers_nothing() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        browser.paint(
            &mut otlyra_gfx::RecordingPainter::new(),
            Viewport::new(800, 600, 1.0),
        );

        ask_for_a_menu(&mut browser, 400.0, UI_HEIGHT - 20.0);
        assert!(!browser.ui().popup_open());
    }

    #[test]
    fn the_cursor_is_ordinary_away_from_a_link() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
        assert_eq!(browser.cursor(), Cursor::Default);

        // Over the empty end of the tab strip, where nothing responds.
        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 10.0 });
        assert_eq!(browser.cursor(), Cursor::Default);

        // Over a tab, which does: the hand is a promise that pressing does
        // something, and it is owed by the interface as much as by a link.
        browser.on_event(PlatformEvent::PointerMoved { x: 100.0, y: 10.0 });
        assert_eq!(browser.cursor(), Cursor::Pointer);

        // And over the address field, where text goes.
        browser.on_event(PlatformEvent::PointerMoved {
            x: 400.0,
            y: UI_HEIGHT - 20.0,
        });
        assert_eq!(browser.cursor(), Cursor::Text);
    }

    #[test]
    fn a_press_on_the_page_that_is_not_a_link_navigates_nowhere() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        assert_eq!(browser.tabs[0].url, "https://start.example/");
    }

    /// A drag across the page selects the words the pointer passed, and ⌘C puts
    /// them on the clipboard.
    #[test]
    fn dragging_across_the_page_selects_text_and_copies_it() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        // Across the first line of the page, which is the paragraph the loader
        // serves. The pointer is moved first, because a press lands where the
        // pointer last was.
        let line = browser.tabs[0]
            .page
            .as_ref()
            .expect("a page")
            .rect_of(
                browser.tabs[0]
                    .page
                    .as_ref()
                    .expect("a page")
                    .box_at(30.0, UI_HEIGHT + 20.0)
                    .expect("something under the pointer"),
            )
            .expect("it was drawn");

        let y = f64::from(line.y) + UI_HEIGHT + 6.0;
        browser.on_event(PlatformEvent::PointerMoved { x: 9.0, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        browser.on_event(PlatformEvent::PointerMoved { x: 400.0, y });
        browser.on_event(PlatformEvent::PointerReleased);

        let selected = browser.tabs[0]
            .page
            .as_ref()
            .expect("a page")
            .selected_text()
            .expect("a drag across the words selected some of them");
        assert!(
            selected.contains("go on") || selected.contains("go"),
            "the words the pointer passed: {selected:?}"
        );

        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Character('c'),
            modifiers: Modifiers {
                command: true,
                ..Modifiers::default()
            },
        });
        assert_eq!(
            browser.clipboard.read().as_deref(),
            Some(selected.as_str()),
            "what was selected is what was copied"
        );
    }

    /// An open menu is drawn over the page and owns every press that lands on
    /// it — including the second of a double click, which would otherwise
    /// select a word behind the menu instead of choosing the item under the
    /// pointer.
    #[test]
    fn a_press_on_an_open_menu_never_reaches_the_page_behind_it() {
        let mut browser = Browser::new(LinkLoader);
        go(&mut browser, "start.example");
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.ui.open_menu();
        browser.on_event(PlatformEvent::PointerMoved {
            x: 700.0,
            y: UI_HEIGHT + 40.0,
        });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });

        assert!(
            !browser.selecting,
            "the page took a press that belonged to the menu"
        );
        assert!(
            !browser.tabs[0]
                .page
                .as_ref()
                .expect("a page")
                .has_selection(),
            "and started selecting behind it"
        );
        assert!(
            !browser.ui.menu_open(),
            "the interface got the press, and a press outside an open menu \
             closes it"
        );
    }

    /// The second rank of selecting: a word, the block it is in, the whole page,
    /// and the far end moved by the keyboard.
    #[test]
    fn a_second_click_takes_a_word_and_a_third_takes_the_block() {
        /// Two paragraphs of ordinary words, so a word and a block are
        /// different amounts of text.
        struct Prose;
        impl Loader for Prose {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<body><p>alpha beta gamma</p><p>delta epsilon</p>".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Prose);
        go(&mut browser, "prose.example");
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        let selected = |browser: &Browser| {
            browser.tabs[0]
                .page
                .as_ref()
                .expect("a page")
                .selected_text()
                .unwrap_or_default()
        };

        // Into the first word of the first paragraph.
        let y = UI_HEIGHT + 14.0;
        browser.on_event(PlatformEvent::PointerMoved { x: 12.0, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 2 });
        browser.on_event(PlatformEvent::PointerReleased);
        assert_eq!(selected(&browser), "alpha", "a second click takes the word");

        browser.on_event(PlatformEvent::PointerPressed { clicks: 3 });
        browser.on_event(PlatformEvent::PointerReleased);
        assert_eq!(
            selected(&browser),
            "alpha beta gamma",
            "a third takes the block it is in and stops there"
        );

        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Character('a'),
            modifiers: Modifiers {
                command: true,
                ..Modifiers::default()
            },
        });
        let everything = selected(&browser);
        assert!(
            everything.contains("alpha beta gamma") && everything.contains("delta epsilon"),
            "and ⌘A takes the page: {everything:?}"
        );

        // Back to one word, then one character further with the keyboard.
        browser.on_event(PlatformEvent::PointerPressed { clicks: 2 });
        browser.on_event(PlatformEvent::PointerReleased);
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Right,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        });
        assert_eq!(
            selected(&browser),
            "alpha ",
            "shift and an arrow move the far end and keep the near one"
        );

        // An arrow with nothing held down is still the page scrolling, which is
        // what it means on a page nobody is editing.
        let before = selected(&browser);
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Right,
            modifiers: Modifiers::default(),
        });
        assert_eq!(selected(&browser), before, "and a bare arrow moves nothing");
    }

    /// A loader whose page brings a font with it, from a stylesheet in a
    /// directory of its own — so the address is only right if it is resolved
    /// against the sheet rather than against the page.
    struct FontLoader;

    impl Loader for FontLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            let page = |bytes: Vec<u8>, kind: &str| {
                Ok(Loaded {
                    content_type: Some(kind.to_owned()),
                    bytes,
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            };
            match url {
                "https://type.example/" => page(
                    b"<link rel=stylesheet href=/style/page.css><p>set in it".to_vec(),
                    "text/html",
                ),
                "https://type.example/style/page.css" => page(
                    b"@font-face { font-family: Brought; src: url(../fonts/brought.ttf) }\n\
                      p { font-family: Brought }"
                        .to_vec(),
                    "text/css",
                ),
                "https://type.example/fonts/brought.ttf" => {
                    page(otlyra_text::TEST_FONT.to_vec(), "font/ttf")
                }
                other => Err(format!("404 {other}")),
            }
        }
    }

    /// A page that brings its own typeface gets it: the rule is found in the
    /// fetched sheet, the address is resolved against that sheet, and the family
    /// is one the shaper can answer for afterwards.
    #[test]
    fn a_page_brings_its_own_font() {
        let mut browser = Browser::new(FontLoader);
        assert!(
            !browser.text.has_family("Brought"),
            "the family cannot exist before the page that defines it"
        );

        go(&mut browser, "https://type.example/");
        // A frame: a `@font-face` rule is only known once the sheet holding it has
        // been parsed, which is the page's first restyle.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        settle(&mut browser);
        browser.prepare_frame(
            Viewport::new(800, 600, 1.0),
            std::time::Duration::from_secs(5),
        );

        assert!(
            browser.text.has_family("Brought"),
            "the family the page defined is the shaper's now"
        );
        assert!(
            browser
                .fetcher
                .exchanges()
                .iter()
                .any(|exchange| exchange.url == "https://type.example/fonts/brought.ttf"),
            "the address is resolved against the sheet, not the page: {:?}",
            browser
                .fetcher
                .exchanges()
                .iter()
                .map(|exchange| exchange.url.clone())
                .collect::<Vec<_>>()
        );
    }

    /// A loader whose pages contain one link, so the click path has something to
    /// land on.
    struct LinkLoader;

    impl Loader for LinkLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            // Anything that is not already an address on this host is treated as
            // its root, which is what typing a bare hostname means.
            let path = match url.strip_prefix("https://start.example") {
                Some("") | None => "/",
                Some(path) => path,
            };
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>Linked</title><body><p><a href=\"/next\">go on</a></p>".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: format!("https://start.example{path}"),
                ..Default::default()
            })
        }
    }

    #[test]
    fn the_inspector_takes_its_height_out_of_the_page_rather_than_over_it() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        let content = 600.0 - UI_HEIGHT;
        assert_eq!(
            browser.dock_height(content),
            0.0,
            "closed, it takes nothing"
        );

        browser.inspector.toggle();
        let dock = browser.dock_height(content);
        assert!(dock > 0.0);
        assert!(
            dock < content,
            "the page keeps room to be a page: {dock} of {content}"
        );
        // The panel starts where the page stops, so neither is drawn over the
        // other and the page is laid out for the height it actually has.
        assert_eq!(browser.dock_top(), 600.0 - dock);
    }

    #[test]
    fn the_picker_chooses_the_element_a_click_would_have_hit() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.inspector.toggle();
        browser.inspector.picking = true;

        // Where the link is — the same point that would follow it if the picker
        // were not armed, which is the property worth having: one hit test, two
        // readings of the answer.
        let (x, y) = link_position(&browser);
        browser.on_event(PlatformEvent::PointerMoved { x, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });

        assert_eq!(
            browser.tabs[0].url, "https://start.example/",
            "armed, the press picked rather than followed the link"
        );
        let selected = browser.inspector.selected.expect("something was chosen");
        let page = browser.tabs[0].page.as_ref().expect("a page is loaded");
        let named = page
            .document()
            .get(selected)
            .map(|node| match &node.data {
                otlyra_dom::NodeData::Element(element) => element.name.local.to_string(),
                otlyra_dom::NodeData::Text(_) => "#text".to_owned(),
                _ => "other".to_owned(),
            })
            .expect("the chosen node is in the document it came from");
        assert!(
            ["a", "p", "body", "#text"].contains(&named.as_str()),
            "picked {named}, which is not on the line that was pointed at"
        );

        // And the picker disarms itself, so the next press is a press again.
        assert!(!browser.inspector.picking);
    }

    #[test]
    fn moving_the_picker_between_page_elements_requests_each_highlight_frame() {
        struct TwoBlocks;

        impl Loader for TwoBlocks {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<style>div { height: 80px }</style>\
                             <body><div>one</div><div>two</div>"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(TwoBlocks);
        browser.navigate("https://blocks.example/");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        let positions = {
            let page = browser.tabs[0].page.as_ref().expect("a page is loaded");
            let boxes = page.boxes();
            boxes
                .descendants(boxes.root())
                .into_iter()
                .filter(|&id| {
                    boxes
                        .node(id)
                        .tag
                        .as_ref()
                        .is_some_and(|tag| tag.as_ref() == "div")
                })
                .map(|id| {
                    let rect = page.rect_of(id).expect("the block was laid out");
                    (
                        f64::from(rect.x + rect.width / 2.0),
                        f64::from(rect.y + rect.height / 2.0),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(positions.len(), 2);

        browser.inspector.toggle();
        browser.inspector.picking = true;

        let (first_x, first_y) = positions[0];
        assert_eq!(
            browser.handle_event(PlatformEvent::PointerMoved {
                x: first_x,
                y: first_y,
            }),
            FrameRequest::Now
        );
        let first = browser
            .inspector
            .selected
            .expect("the first block was highlighted");

        let (second_x, second_y) = positions[1];
        assert_eq!(
            browser.handle_event(PlatformEvent::PointerMoved {
                x: second_x,
                y: second_y,
            }),
            FrameRequest::Now,
            "a new picker target changes overlay pixels even inside an otherwise static page"
        );
        assert_ne!(
            browser.inspector.selected,
            Some(first),
            "the second point must exercise a different element"
        );
    }

    #[test]
    fn the_highlight_is_where_the_engine_drew_the_chosen_box() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.inspector.toggle();
        let (x, y) = link_position(&browser);
        browser.inspector.picking = true;
        browser.on_event(PlatformEvent::PointerMoved { x, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });

        let rect = browser
            .chosen_box()
            .expect("the chosen box was drawn")
            .border;
        // Asserted against the engine's own answer rather than against numbers:
        // whatever box the hit test names, the overlay is that box's rectangle.
        let page = browser.tabs[0].page.as_ref().expect("a page is loaded");
        let id = page
            .boxes()
            .box_for(browser.inspector.selected.expect("something was chosen"))
            .expect("the chosen node has a box");
        let expected = page.rect_of(id).expect("the box was drawn");
        assert_eq!(rect.x, f64::from(expected.x));
        assert_eq!(rect.y, f64::from(expected.y));
        assert_eq!(rect.width, f64::from(expected.width));
        assert_eq!(rect.height, f64::from(expected.height));
        assert!(
            rect.y >= UI_HEIGHT,
            "the overlay is in window coordinates, below the toolbar"
        );
    }

    /// A page whose one element lays its children into tracks.
    struct GridLoader;

    impl Loader for GridLoader {
        fn load(&self, _url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<style>.g { display: grid; gap: 10px; \
                         grid-template-columns: 100px 100px; }</style>\
                         <div class=g><div>a</div><div>b</div>\
                         <div>c</div><div>d</div></div>\
                         <p>a block, which lays nothing into anything"
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: "https://grid.example/".to_owned(),
                ..Default::default()
            })
        }
    }

    /// Choose the first element the document has whose tag is `tag`.
    fn choose(browser: &mut Browser, tag: &str) {
        let page = browser.tabs[0].page.as_ref().expect("a page");
        let document = page.document();
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            let matches = document.get(node).is_some_and(|node| {
                matches!(&node.data,
                    otlyra_dom::NodeData::Element(element)
                        if element.name.local.as_ref() == tag)
            });
            if matches {
                browser.inspector.selected = Some(node);
                return;
            }
            stack.extend(document.children(node));
        }
        panic!("the document has no {tag}");
    }

    #[test]
    fn a_container_that_lays_its_children_into_tracks_gets_the_dashed_overlay() {
        let mut browser = Browser::new(GridLoader);
        browser.navigate("grid.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        choose(&mut browser, "div");
        let chosen = browser.chosen_box().expect("the grid was drawn");
        let tracks = chosen.tracks.expect("a grid has tracks");
        assert!(
            tracks.numbered,
            "a grid names its lines and a flex row does not"
        );

        // Two columns of a hundred with a ten-pixel gutter: three lines, and the
        // far side of the gutter is the same line rather than a fourth.
        let numbered = tracks
            .columns
            .iter()
            .filter(|line| line.number.is_some())
            .count();
        assert_eq!(numbered, 3, "columns: {:?}", tracks.columns);

        // And a block lays nothing into anything, so it has no lines to draw.
        choose(&mut browser, "p");
        let block = browser.chosen_box().expect("the paragraph was drawn");
        assert!(block.tracks.is_none());
    }

    /// A picture of `bytes` bytes, with no pixels worth looking at.
    fn picture(bytes: usize) -> otlyra_gfx::peniko::ImageData {
        let side = 1u32;
        otlyra_gfx::peniko::ImageData {
            data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![0u8; bytes])),
            format: otlyra_gfx::peniko::ImageFormat::Rgba8,
            alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
            width: side,
            height: side,
        }
    }

    /// The cache keeps what fits and drops what has not been looked at longest.
    #[test]
    fn the_image_cache_evicts_the_least_recently_used() {
        let mut cache = ImageCache::default();
        let big = IMAGE_CACHE_BUDGET / 2;

        cache.insert("a".to_owned(), picture(big));
        cache.insert("b".to_owned(), picture(big));
        assert!(cache.get("a").is_some(), "both fit");

        // `a` was just used, so `b` is the one that goes.
        cache.insert("c".to_owned(), picture(big));
        assert!(cache.get("b").is_none(), "the older one should have gone");
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.bytes <= IMAGE_CACHE_BUDGET);
    }

    /// One larger than the whole budget is not worth emptying the cache for.
    #[test]
    fn an_oversized_picture_is_not_cached_at_all() {
        let mut cache = ImageCache::default();
        cache.insert("small".to_owned(), picture(1024));
        cache.insert("huge".to_owned(), picture(IMAGE_CACHE_BUDGET + 1));

        assert!(cache.get("huge").is_none());
        assert!(cache.get("small").is_some(), "it emptied the cache anyway");
    }

    /// A picture that has already been decoded is not fetched again — which is what
    /// the cache is for, and is visible in what the loader was asked for.
    #[test]
    fn a_cached_picture_is_not_fetched_twice() {
        struct PictureLoader {
            requested: Requests,
        }

        impl Loader for PictureLoader {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                self.requested
                    .lock()
                    .expect("no panic on the fetch thread")
                    .push(url.to_owned());
                if url.ends_with(".png") {
                    // A one-pixel PNG, so the decoder has something real to do.
                    return Ok(Loaded {
                        content_type: Some("image/png".to_owned()),
                        bytes: ONE_PIXEL_PNG.to_vec(),
                        final_url: url.to_owned(),
                        ..Default::default()
                    });
                }
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<body><img src=\"/pic.png\"><img src=\"/pic.png\">".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: "https://pictures.example/".to_owned(),
                    ..Default::default()
                })
            }
        }

        let requested = Requests::default();
        let mut browser = Browser::new(PictureLoader {
            requested: std::sync::Arc::clone(&requested),
        });

        go(&mut browser, "pictures.example");
        let first = asked_for(&requested);
        assert_eq!(
            first.iter().filter(|url| url.ends_with(".png")).count(),
            1,
            "one address, one fetch, however many elements ask for it"
        );

        go(&mut browser, "pictures.example");
        let second = asked_for(&requested);
        assert_eq!(
            second.iter().filter(|url| url.ends_with(".png")).count(),
            1,
            "the picture was decoded again on the second visit"
        );
    }

    /// A window that grows past the file its pictures were chosen for asks again.
    ///
    /// The choice among the several a `srcset` offers is made against the window
    /// the page loads into, and that window is not the one it stays in. Chosen
    /// once and never revisited, a page opened narrow and then widened keeps the
    /// small file and draws it stretched.
    #[test]
    fn a_widened_window_asks_for_a_larger_picture() {
        struct PictureLoader {
            requested: Requests,
        }

        impl Loader for PictureLoader {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                self.requested
                    .lock()
                    .expect("no panic on the fetch thread")
                    .push(url.to_owned());
                if url.ends_with(".png") {
                    return Ok(Loaded {
                        content_type: Some("image/png".to_owned()),
                        bytes: ONE_PIXEL_PNG.to_vec(),
                        final_url: url.to_owned(),
                        ..Default::default()
                    });
                }
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<body><img sizes=\"100vw\" \
                             srcset=\"/narrow.png 400w, /wide.png 1600w\" src=\"/narrow.png\">"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: "https://pictures.example/".to_owned(),
                    ..Default::default()
                })
            }
        }

        let requested = Requests::default();
        let mut browser = Browser::new(PictureLoader {
            requested: std::sync::Arc::clone(&requested),
        });

        let narrow = Viewport::new(400, 600, 1.0);
        browser.set_viewport(narrow);
        go(&mut browser, "pictures.example");

        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, narrow);
        let asked = asked_for(&requested);
        assert!(
            asked.iter().any(|url| url.ends_with("/narrow.png"))
                && !asked.iter().any(|url| url.ends_with("/wide.png")),
            "a narrow window takes the small file: {asked:?}"
        );

        // Wider than the small file can cover, so the element now wants the
        // large one.
        let wide = Viewport::new(1400, 600, 1.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !asked_for(&requested)
            .iter()
            .any(|url| url.ends_with("/wide.png"))
            && std::time::Instant::now() < deadline
        {
            browser.paint(&mut painter, wide);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(
            asked_for(&requested)
                .iter()
                .any(|url| url.ends_with("/wide.png")),
            "the widened window never asked for the larger file: {:?}",
            asked_for(&requested)
        );

        // And it is the picture the element now holds, rather than a fetch that
        // went nowhere.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            browser.paint(&mut painter, wide);
            let held = browser.tabs[0]
                .page
                .as_ref()
                .and_then(|page| {
                    let node = otlyra_layout::image_sources(
                        page.document(),
                        otlyra_css::cascade::Viewport::default(),
                    )
                    .first()?
                    .node;
                    Some(page.picture_source(node)?.0.to_owned())
                })
                .unwrap_or_default();
            if held.ends_with("/wide.png") || std::time::Instant::now() >= deadline {
                assert!(
                    held.ends_with("/wide.png"),
                    "the element still holds {held}"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A frame takes in whatever has arrived, even when nothing woke the loop.
    ///
    /// The regression this pins: a page asked for before the window exists finishes
    /// before there is a waker to be woken by, so that wake is lost. If a frame did
    /// not take results in as well, the tab would stay loading — and the spinner
    /// would turn for a page that had already arrived.
    #[test]
    fn a_frame_takes_in_a_load_that_nothing_woke_the_loop_for() {
        let mut browser = browser();
        browser.navigate("example.com");

        // Nothing here pumps but painting, which is the whole point.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while browser.tabs[0].loading() && std::time::Instant::now() < deadline {
            browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(!browser.tabs[0].loading(), "the tab is still loading");
        assert!(browser.tabs[0].page.is_some());
    }

    /// Back and forward walk the addresses the tab has been to, and a new tab has
    /// nowhere to go in either direction.
    #[test]
    fn back_and_forward_walk_the_history() {
        let mut browser = browser();
        assert!(!browser.can_go_back() && !browser.can_go_forward());

        browser.navigate("one.example");
        settle(&mut browser);
        assert!(!browser.can_go_back(), "one entry is nowhere to go back to");
        browser.navigate("two.example");
        settle(&mut browser);
        browser.navigate("three.example");
        settle(&mut browser);

        assert!(browser.can_go_back() && !browser.can_go_forward());
        browser.go_back();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, "https://two.example/");
        assert!(browser.can_go_forward());

        browser.go_back();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, "https://one.example/");
        assert!(!browser.can_go_back());
        browser.go_back();
        settle(&mut browser);
        assert_eq!(
            browser.tabs[0].url, "https://one.example/",
            "and no further"
        );

        browser.go_forward();
        settle(&mut browser);
        browser.go_forward();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, "https://three.example/");
        browser.go_forward();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, "https://three.example/", "nor further");
    }

    /// Going somewhere new after going back drops what was ahead: those entries
    /// describe a future that did not happen.
    #[test]
    fn navigating_after_going_back_drops_the_forward_entries() {
        let mut browser = browser();
        browser.navigate("one.example");
        settle(&mut browser);
        browser.navigate("two.example");
        settle(&mut browser);
        browser.go_back();
        settle(&mut browser);
        browser.navigate("three.example");
        settle(&mut browser);

        assert!(!browser.can_go_forward());
        browser.go_back();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, "https://one.example/");
    }

    /// A reload is the same place twice, not two places.
    #[test]
    fn a_reload_adds_no_history_entry() {
        let mut browser = browser();
        browser.navigate("one.example");
        settle(&mut browser);
        browser.navigate("two.example");
        settle(&mut browser);
        browser.reload();
        settle(&mut browser);

        assert!(!browser.can_go_forward());
        browser.go_back();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, "https://one.example/");
    }

    /// Where the reader had got to comes back with the page, which is the part of
    /// a back button people actually notice.
    /// One scroll event, and everything it can land on goes the same way.
    ///
    /// The property that was broken: the page added the delta and the browser's
    /// own surfaces subtracted it, so a wheel that went down a document went up
    /// the settings. Nobody notices which of the two is "right" until they are
    /// different, which is why this is asserted rather than commented.
    #[test]
    fn a_scroll_goes_the_same_way_on_a_document_and_on_a_browser_page() {
        let mut painter = otlyra_gfx::RecordingPainter::new();

        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        browser.on_event(PlatformEvent::PointerMoved { x: 400.0, y: 400.0 });
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 120.0,
            source: otlyra_platform::ScrollSource::Wheel,
            modifiers: Default::default(),
        });
        let page = browser.tabs[0].page.as_ref().expect("a page").scroll();
        assert!(page > 0.0, "a positive delta goes down the document");

        browser.open_system(SystemPage::Settings);
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 120.0,
            source: otlyra_platform::ScrollSource::Wheel,
            modifiers: Default::default(),
        });
        assert!(
            browser.settings.settings.scroll > 0.0,
            "and down the browser's own page, by the same event"
        );
    }

    /// A trackpad's small precise deltas are a distance, not a notch.
    #[test]
    fn a_trackpad_scrolls_by_what_it_says_rather_than_by_a_notch() {
        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        browser.on_event(PlatformEvent::PointerMoved { x: 400.0, y: 400.0 });

        // Three pixels is three pixels. A browser that read this as a notch
        // would jump the page by a wheel's worth for a gesture that moved a
        // finger a hair.
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 3.0,
            source: otlyra_platform::ScrollSource::Trackpad,
            modifiers: Default::default(),
        });
        assert_eq!(browser.tabs[0].page.as_ref().expect("a page").scroll(), 3.0);
    }

    #[test]
    fn going_back_restores_where_the_reader_was() {
        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);
        browser.tabs[0]
            .page
            .as_mut()
            .expect("a page")
            .set_scroll(120.0);

        browser.navigate("long.example/second");
        settle(&mut browser);
        browser.go_back();
        settle(&mut browser);
        assert_eq!(
            browser.tabs[0].page.as_ref().expect("a page").scroll(),
            120.0
        );
    }

    #[test]
    fn the_network_list_holds_every_request_and_what_became_of_it() {
        use crate::fetcher::{ResourceKind, Status};

        let mut browser = Browser::new(SiteLoader::default());
        browser.navigate("https://site.example/");
        settle(&mut browser);
        // A frame, because a stylesheet is asked for while the document is being
        // turned into one.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        settle(&mut browser);

        let listed: Vec<(&str, ResourceKind)> = browser
            .fetcher
            .exchanges()
            .iter()
            .map(|exchange| (exchange.url.as_str(), exchange.kind))
            .collect();
        assert_eq!(
            listed,
            [
                ("https://site.example/", ResourceKind::Document),
                ("https://site.example/site.css", ResourceKind::Stylesheet),
                ("https://site.example/missing.css", ResourceKind::Stylesheet),
            ],
            "exactly what was asked for, in the order it was asked for"
        );

        // And what became of each: the one that arrived says how much of it
        // there was, and the one that did not says why.
        let by_url = |wanted: &str| {
            browser
                .fetcher
                .exchanges()
                .iter()
                .find(|exchange| exchange.url == wanted)
                .expect("listed above")
                .clone()
        };
        assert!(matches!(
            by_url("https://site.example/site.css").status,
            Status::Ok(bytes) if bytes > 0
        ));
        assert_eq!(
            by_url("https://site.example/missing.css").status,
            Status::Failed("404".to_owned())
        );
        assert!(
            by_url("https://site.example/site.css").took.is_some(),
            "a finished request knows how long the transport took"
        );
    }

    #[test]
    fn the_text_size_preference_is_the_default_a_page_inherits() {
        /// A page that names no size, and one that names its own.
        struct Sized;
        impl Loader for Sized {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                let bytes = if url.contains("named") {
                    b"<body><p style=\"font-size: 15px\">text".to_vec()
                } else {
                    b"<body><p>text".to_vec()
                };
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes,
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        /// The size the one paragraph was computed at.
        fn paragraph(browser: &mut Browser) -> f32 {
            let mut painter = otlyra_gfx::RecordingPainter::new();
            browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
            let page = browser.active_page().expect("a page");
            let boxes = page.boxes();
            boxes
                .descendants(boxes.root())
                .into_iter()
                .filter_map(|id| boxes.get(id))
                .find(|node| node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "p"))
                .expect("a paragraph")
                .style
                .font_size
        }

        let mut browser = Browser::new(Sized);
        browser.navigate("https://plain.example/");
        settle(&mut browser);
        let ordinary = paragraph(&mut browser);

        browser.settings.settings.text_scale = 200.0;
        let doubled = paragraph(&mut browser);
        assert!(
            (doubled - ordinary * 2.0).abs() < 0.01,
            "a page that names no size inherits the reader's default: \
             {ordinary} became {doubled}"
        );

        // And a page that names one still wins, because this is a default and
        // not an override — which is the part that surprises people, and the
        // part that would be wrong the other way round.
        browser.navigate("https://named.example/");
        settle(&mut browser);
        assert!(
            (paragraph(&mut browser) - 15.0).abs() < 0.01,
            "a page that names its own size keeps it"
        );
    }

    #[test]
    fn the_appearance_preference_is_what_a_page_asks_for() {
        /// A page that draws itself differently in the dark.
        struct Themed;
        impl Loader for Themed {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<style>\
                             p { background: rgb(255, 255, 255) }\
                             @media (prefers-color-scheme: dark) { \
                               p { background: rgb(0, 0, 0) } }\
                             </style><body><p>text"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        /// What the one paragraph is painted behind, after a frame.
        fn background(browser: &mut Browser) -> [u8; 4] {
            let mut painter = otlyra_gfx::RecordingPainter::new();
            browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
            let page = browser.active_page().expect("a page");
            let boxes = page.boxes();
            let colour = boxes
                .descendants(boxes.root())
                .into_iter()
                .filter_map(|id| boxes.get(id))
                .find(|node| node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "p"))
                .expect("a paragraph")
                .style
                .background_color;
            colour.to_rgba8().to_u8_array()
        }

        let mut browser = Browser::new(Themed);
        browser.navigate("https://themed.example/");
        settle(&mut browser);
        assert_eq!(
            background(&mut browser),
            [255, 255, 255, 255],
            "the preference starts at light"
        );

        browser.settings.settings.appearance = crate::settings::Appearance::Dark;
        assert_eq!(
            background(&mut browser),
            [0, 0, 0, 255],
            "and the page follows it"
        );
    }

    #[test]
    fn turning_pictures_off_means_none_are_asked_for() {
        /// A page with a picture in it, and a log of everything asked for.
        #[derive(Default)]
        struct Pictures {
            requested: Requests,
        }

        impl Loader for Pictures {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                self.requested
                    .lock()
                    .expect("no panic on the fetch thread")
                    .push(url.to_owned());
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<body><img src=picture.png><p>text".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: "https://pictures.example/".to_owned(),
                    ..Default::default()
                })
            }
        }

        let asked = |browser: &mut Browser, requested: &Requests| {
            browser.navigate("https://pictures.example/");
            settle(browser);
            let mut painter = otlyra_gfx::RecordingPainter::new();
            browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
            settle(browser);
            requested
                .lock()
                .expect("no panic on the fetch thread")
                .clone()
        };

        let loader = Pictures::default();
        let requested = std::sync::Arc::clone(&loader.requested);
        let mut browser = Browser::new(loader);
        let with = asked(&mut browser, &requested);
        assert!(
            with.iter().any(|url| url.contains("picture.png")),
            "the picture is asked for by default: {with:?}"
        );

        let loader = Pictures::default();
        let requested = std::sync::Arc::clone(&loader.requested);
        let mut browser = Browser::new(loader);
        browser.settings.settings.load_images = false;
        let without = asked(&mut browser, &requested);
        // Refused before the request rather than after it: a picture fetched and
        // then not shown has already cost the reader their bandwidth and told
        // the server they were here.
        assert!(
            !without.iter().any(|url| url.contains("picture.png")),
            "and not asked for at all when the preference says so: {without:?}"
        );
        assert!(
            without.iter().any(|url| url.contains("pictures.example")),
            "the page itself still loads"
        );
    }

    #[test]
    fn two_tabs_keep_their_own_contents_across_a_switch() {
        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);

        browser.new_tab();
        browser.open_system(SystemPage::Settings);
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));

        // A browser page is a place a tab can be rather than a mode the window
        // is in, so the other tab is still on its document.
        browser.select_tab(0);
        assert_eq!(browser.system_page(), None);
        assert!(browser.tabs[0].page.is_some());

        browser.select_tab(1);
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
        assert!(browser.tabs[1].page.is_none());
    }

    #[test]
    fn the_history_walks_through_a_browser_page_like_any_other() {
        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);
        let document = browser.tabs[0].url.clone();

        // Left somewhere down the page, which is the thing going back has to
        // bring back along with the document.
        browser.tabs[0]
            .page
            .as_mut()
            .expect("a page")
            .set_scroll(120.0);

        browser.open_system(SystemPage::Settings);
        assert_eq!(browser.tabs[0].url, "about:settings");

        browser.go_back();
        settle(&mut browser);
        assert_eq!(browser.tabs[0].url, document);
        assert_eq!(
            browser.tabs[0].page.as_ref().expect("a page").scroll(),
            120.0,
            "and at the place it was left at"
        );

        browser.go_forward();
        settle(&mut browser);
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    }

    #[test]
    fn done_goes_back_rather_than_wiping_the_tab() {
        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);
        let document = browser.tabs[0].url.clone();
        browser.open_system(SystemPage::Settings);

        browser.handle_settings_action(&settings::Action::Close);
        settle(&mut browser);
        assert_eq!(
            browser.tabs[0].url, document,
            "the reader goes back to what they were reading"
        );

        // And with nowhere to go back to, an empty tab rather than a settings
        // page nobody can leave.
        let mut fresh = Browser::new(LongLoader);
        fresh.open_system(SystemPage::Settings);
        fresh.handle_settings_action(&settings::Action::Close);
        assert_eq!(fresh.system_page(), None);
        assert!(fresh.tabs[0].url.is_empty());
    }

    #[test]
    fn a_browser_page_is_left_and_returned_to_where_the_reader_was() {
        let mut browser = Browser::new(LongLoader);
        browser.open_system(SystemPage::Settings);
        // Drawn, because how far a surface can scroll is only known once it has
        // been: the same rule the panel and the page both follow.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        browser.settings.scroll_by(140.0);
        let left_at = browser.settings.settings.scroll;
        assert!(left_at > 0.0, "the settings scrolled at all");

        browser.navigate("long.example");
        settle(&mut browser);

        // Scrambled while nobody is looking at it, because the surface is the
        // browser's and another tab may have used it in between. What brings the
        // reader back to where they were is the history entry, not the surface
        // happening to still hold the number.
        browser.settings.settings.scroll = 999.0;

        browser.go_back();
        settle(&mut browser);
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
        assert_eq!(
            browser.settings.settings.scroll, left_at,
            "and a browser page is returned to where the reader was, like any other"
        );
    }

    #[test]
    fn a_browser_page_is_reached_by_typing_its_address() {
        let mut browser = Browser::new(LongLoader);
        // Every navigation goes through one place, so the address bar reaches
        // `about:` the same way the menu does.
        browser.navigate("about:settings");
        assert_eq!(browser.system_page(), Some(SystemPage::Settings));
        assert_eq!(browser.ui.address.text(), "about:settings");
    }

    /// A screen reader's press on the page is a press: it ticks the box, and the
    /// tree says so afterwards.
    #[test]
    fn a_readers_press_ticks_a_checkbox_on_the_page() {
        struct FormPage;

        impl Loader for FormPage {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<body><label><input type=checkbox> Send me post</label>".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(FormPage);
        go(&mut browser, "https://site.example/");
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(1000, 700, 1.0));

        let update = browser.accessibility().expect("a tree");
        let (id, node) = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == otlyra_platform::accesskit::Role::CheckBox)
            .expect("the checkbox");
        assert_eq!(
            node.toggled(),
            Some(otlyra_platform::accesskit::Toggled::False)
        );

        browser.on_event(PlatformEvent::AccessibilityRequest {
            node: *id,
            action: otlyra_platform::AccessibilityAction::Activate,
        });
        browser.paint(&mut painter, Viewport::new(1000, 700, 1.0));

        let update = browser.accessibility().expect("a tree");
        let (_, node) = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == otlyra_platform::accesskit::Role::CheckBox)
            .expect("the checkbox");
        assert_eq!(
            node.toggled(),
            Some(otlyra_platform::accesskit::Toggled::True),
            "the press was swallowed"
        );
    }

    /// And a press on a button sends the form behind it, which is the whole of
    /// what pressing one means without a script.
    #[test]
    fn a_readers_press_sends_the_form() {
        struct SearchPage;

        impl Loader for SearchPage {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<body><form action=/search><input name=q value=cats>\
                      <input type=submit value=Go></form>"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(SearchPage);
        go(&mut browser, "https://site.example/");
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(1000, 700, 1.0));

        let update = browser.accessibility().expect("a tree");
        let (id, _) = update
            .nodes
            .iter()
            .find(|(id, node)| {
                crate::a11y::described_index(*id).is_none()
                    && node.role() == otlyra_platform::accesskit::Role::Button
            })
            .expect("the button");

        browser.on_event(PlatformEvent::AccessibilityRequest {
            node: *id,
            action: otlyra_platform::AccessibilityAction::Activate,
        });
        settle(&mut browser);
        assert_eq!(
            browser.tabs[browser.active].url,
            "https://site.example/search?q=cats"
        );
    }

    /// A form that posts sends its body, and the answer becomes the page.
    ///
    /// Everything before the request is tested where it is built; what this holds
    /// is the last stretch, which was the missing one: the method, the body and the
    /// type reach the transport, and the page they come back with is the tab's.
    #[test]
    fn a_form_that_posts_sends_its_body() {
        /// Every request the browser made, with whatever body it carried.
        type Sent = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<Body>)>>>;

        struct PostLoader {
            sent: Sent,
        }

        impl Loader for PostLoader {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                self.send(url, None)
            }

            fn send(&self, url: &str, body: Option<Body>) -> Result<Loaded, String> {
                self.sent
                    .lock()
                    .expect("no panic on the fetch thread")
                    .push((url.to_owned(), body.clone()));
                let bytes = if body.is_some() {
                    b"<body><p>saved".to_vec()
                } else {
                    b"<body><form method=post action=/save>\
                      <input name=who value=Ada><input type=submit value=Go></form>"
                        .to_vec()
                };
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes,
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let sent = Sent::default();
        let mut browser = Browser::new(PostLoader {
            sent: std::sync::Arc::clone(&sent),
        });
        go(&mut browser, "https://site.example/");

        // Pressed where it was drawn, which needs a frame: the button's rectangle
        // is the last layout's, like every other press.
        let active = browser.active;
        let page = browser.tabs[active].page.as_mut().expect("a page");
        page.build_display_list(&mut TextEngine::isolated(), 800.0, 600.0, 0.0);
        let boxes = page.boxes();
        let button = boxes
            .descendants(boxes.root())
            .into_iter()
            .filter(|&id| boxes.node(id).control.is_some())
            .nth(1)
            .expect("the button");
        let rect = page.rect_of(button).expect("a rectangle");
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        browser.follow_submission();
        settle(&mut browser);

        let sent = sent.lock().expect("no panic on the fetch thread").clone();
        let (url, body) = sent.last().expect("the form was sent").clone();
        assert_eq!(url, "https://site.example/save");
        let body = body.expect("a POST carries a body");
        assert_eq!(body.content_type, "application/x-www-form-urlencoded");
        assert_eq!(body.bytes, b"who=Ada");
        assert_eq!(
            browser.tabs[browser.active].url, "https://site.example/save",
            "and the tab is where the form sent it"
        );
    }

    /// A site whose CSS lives in a file next to the page.
    #[derive(Default)]
    struct SiteLoader {
        requested: Requests,
    }

    impl Loader for SiteLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            match url {
                "https://site.example/site.css" => Ok(Loaded {
                    content_type: Some("text/css".to_owned()),
                    bytes: b"p { color: rgb(0, 128, 0) }".to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                }),
                "https://site.example/missing.css" => Err("404".to_owned()),
                _ => Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<link rel=stylesheet href=site.css>\
                      <link rel=stylesheet href=missing.css>\
                      <link rel=icon href=favicon.ico>\
                      <body><p>text"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: "https://site.example/".to_owned(),
                    ..Default::default()
                }),
            }
        }
    }

    /// A linked stylesheet is fetched against the page's own address, and only the
    /// links that are stylesheets are fetched at all.
    #[test]
    fn navigation_fetches_the_stylesheets_the_page_links() {
        let requested = Requests::default();
        let mut browser = Browser::new(SiteLoader {
            requested: std::sync::Arc::clone(&requested),
        });
        go(&mut browser, "site.example");

        // Sorted, because the fetch pool serves several at once and the order two
        // stylesheets come back in is not the browser's to promise.
        let mut asked = asked_for(&requested);
        asked.sort();
        assert_eq!(
            asked,
            vec![
                "https://site.example/missing.css".to_owned(),
                "https://site.example/site.css".to_owned(),
                "site.example".to_owned(),
            ],
            "the icon is not a stylesheet and is not fetched"
        );

        let active = browser.active;
        let page = browser.tabs[active].page.as_mut().expect("a page");
        // The cascade runs on the way to a frame, so ask for one.
        page.build_display_list(&mut TextEngine::isolated(), 800.0, 600.0, 0.0);

        let boxes = page.boxes();
        let coloured = boxes.descendants(boxes.root()).into_iter().any(|id| {
            boxes.node(id).style.color == otlyra_gfx::peniko::Color::from_rgb8(0, 128, 0)
        });
        assert!(coloured, "the fetched sheet reached the box tree");
    }

    /// A document is not drawn before the stylesheet it links.
    ///
    /// It was, and what a reader saw on any page with an external sheet was the
    /// author's markup in none of the author's design, replaced a moment later
    /// by the real thing. That reads as the CSS arriving slowly; it is the frame
    /// arriving early. A picture is not on the list — nothing holds a page back
    /// for a photograph.
    #[test]
    fn a_document_is_not_drawn_before_the_stylesheet_it_links() {
        /// A loader that answers the document and never the sheet, which is what
        /// a slow server looks like from here.
        struct SlowSheet;

        impl Loader for SlowSheet {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                if url.ends_with(".css") {
                    // Long enough that the frame below is built while it is still
                    // outstanding, short enough that the test does not hang.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    return Ok(Loaded {
                        content_type: Some("text/css".to_owned()),
                        bytes: b"p { color: #008000 }".to_vec(),
                        charset: Some("utf-8".to_owned()),
                        final_url: url.to_owned(),
                        ..Default::default()
                    });
                }
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<title>T</title><link rel=stylesheet href=/s.css><body><p>text"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    // Absolute, so the `<link>` beside it has something to
                    // resolve against.
                    final_url: format!("https://{url}/"),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(SlowSheet);
        browser.navigate("slow.example");
        // The document itself is in; the sheet is not.
        browser.wait_for_load(std::time::Duration::from_millis(120));
        let active = browser.active;
        assert!(
            browser.blocked_on_style(active),
            "the sheet is still outstanding"
        );

        // The document's one paragraph is four letters, and nothing the chrome
        // or the blank page draws is a run of exactly four. Counting runs alone
        // would prove nothing: the blank page draws a line of its own, so the
        // number is the same either way and only *which* run is there differs.
        let paragraph = |browser: &mut Browser| {
            let mut target = otlyra_gfx::RecordingPainter::default();
            browser.paint(&mut target, Viewport::new(800, 600, 1.0));
            target.ops().iter().any(|op| {
                matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { glyphs, .. } if glyphs.len() == 4)
            })
        };

        assert!(
            !paragraph(&mut browser),
            "the document was drawn before its stylesheet arrived"
        );

        browser.wait_for_load(std::time::Duration::from_secs(5));
        assert!(!browser.blocked_on_style(active), "the sheet arrived");
        assert!(
            paragraph(&mut browser),
            "and it was drawn once the stylesheet was in"
        );
    }

    /// A zoomed page is laid out in fewer CSS pixels and drawn back up to fill
    /// the window — it reflows, rather than being magnified.
    ///
    /// That is the whole difference between a page zoom and a picture of a page
    /// scaled up, and it is what a reader wants: bigger text that still uses the
    /// window it is in.
    #[test]
    fn a_zoomed_page_reflows_rather_than_being_magnified() {
        struct Prose;

        impl Loader for Prose {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<title>T</title><body style='margin:0'><p>one two three four \
                             five six seven eight nine ten eleven twelve thirteen fourteen \
                             fifteen sixteen seventeen eighteen nineteen twenty"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: format!("https://{url}/"),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(Prose);
        go(&mut browser, "prose.example");

        let runs = |browser: &mut Browser| {
            let mut target = otlyra_gfx::RecordingPainter::default();
            browser.paint(&mut target, Viewport::new(800, 600, 1.0));
            target
                .ops()
                .iter()
                .filter(|op| matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { .. }))
                .count()
        };

        assert_eq!(browser.zoom(), 1.0, "a page opens at its own size");
        let plain = runs(&mut browser);

        browser.set_zoom(2.0);
        assert_eq!(browser.zoom(), 2.0);
        let zoomed = runs(&mut browser);
        assert!(
            zoomed > plain,
            "the paragraph broke into more lines in half the pixels: {plain} then {zoomed}"
        );

        // And back, exactly: a reader who undoes a zoom gets the page they had.
        browser.set_zoom(1.0);
        assert_eq!(runs(&mut browser), plain);

        // The range is every browser's, and a factor outside it is brought back
        // rather than refused — a control that silently does nothing is worse
        // than one that stops.
        browser.set_zoom(50.0);
        assert_eq!(browser.zoom(), 5.0);
        browser.set_zoom(0.01);
        assert_eq!(browser.zoom(), 0.25);
    }

    /// The zoom is reached the three ways a reader reaches it, and lands on the
    /// stops a menu can name.
    #[test]
    fn the_zoom_steps_along_a_ladder_from_the_keyboard_the_menu_and_the_wheel() {
        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        let mut browser = browser();
        go(&mut browser, "example.com");

        let press = |browser: &mut Browser, character: char| {
            browser.on_event(PlatformEvent::KeyPressed {
                key: Key::Character(character),
                modifiers: accelerator,
            });
        };

        press(&mut browser, '=');
        assert_eq!(browser.zoom(), 1.1, "one stop up, not one and a bit");
        press(&mut browser, '=');
        assert_eq!(browser.zoom(), 1.25);
        press(&mut browser, '-');
        assert_eq!(browser.zoom(), 1.1);
        press(&mut browser, '0');
        assert_eq!(browser.zoom(), 1.0, "and back to the page's own size");

        // The menu reaches the same ladder.
        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ZoomIn.id(),
        ));
        assert_eq!(browser.zoom(), 1.1);
        browser.on_event(PlatformEvent::MenuCommand(
            crate::menu::Command::ActualSize.id(),
        ));
        assert_eq!(browser.zoom(), 1.0);

        // And the wheel, but only with the modifier held: without it the page
        // scrolls, which is what a wheel is for.
        let wheel = |browser: &mut Browser, y: f64, modifiers: Modifiers| {
            browser.on_event(PlatformEvent::Scroll {
                x: 0.0,
                y,
                source: otlyra_platform::ScrollSource::Wheel,
                modifiers,
            });
        };
        wheel(&mut browser, -40.0, accelerator);
        assert_eq!(browser.zoom(), 1.1, "away from the reader is larger");
        wheel(&mut browser, 40.0, accelerator);
        assert_eq!(browser.zoom(), 1.0);
        wheel(&mut browser, -40.0, Modifiers::default());
        assert_eq!(
            browser.zoom(),
            1.0,
            "a bare wheel scrolls and does not zoom"
        );

        // The ends of the ladder are ends, not a wrap: a reader holding the key
        // down stops at the largest rather than starting again at the smallest.
        for _ in 0..40 {
            press(&mut browser, '=');
        }
        assert_eq!(browser.zoom(), 5.0);
        for _ in 0..40 {
            press(&mut browser, '-');
        }
        assert_eq!(browser.zoom(), 0.25);
    }

    /// A zoom is remembered against the site, and only against the site the
    /// reader set it on.
    #[test]
    fn a_zoom_belongs_to_the_site_it_was_set_on() {
        let mut browser = browser();
        go(&mut browser, "one.example");
        browser.step_zoom(ZoomStep::In);
        assert_eq!(browser.zoom(), 1.1);

        // Another site is another zoom, which is the whole reason this is not
        // one number for the browser.
        go(&mut browser, "two.example");
        assert_eq!(browser.zoom(), 1.0, "the next site is left alone");

        go(&mut browser, "one.example");
        assert_eq!(browser.zoom(), 1.1, "and the first is as it was left");

        // Every page of a site is the site: a zoom set on one page is the zoom
        // on the next.
        go(&mut browser, "https://one.example/deep/page");
        assert_eq!(browser.zoom(), 1.1);

        // Back to its own size and the site stops being one this browser knows
        // anything about — a preferences file should not carry a line for every
        // place anyone has ever been.
        browser.step_zoom(ZoomStep::Reset);
        assert!(browser.settings.settings.zoom.is_empty());
    }

    /// A zoom keeps the reader where they were reading.
    ///
    /// The scroll is in the page's own pixels and survives, but the content
    /// around it is a different height once the lines have broken elsewhere —
    /// so the same offset points at different words. What is held is the place
    /// in the text at the top of the window.
    #[test]
    fn a_zoom_keeps_the_reader_where_they_were_reading() {
        struct LongPage;

        impl Loader for LongPage {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                // Long enough that a tenth off the width breaks them
                // differently, which is what makes the offset in pixels stop
                // meaning what it meant.
                let paragraphs = (0..60)
                    .map(|n| {
                        format!(
                            "<p>paragraph number {n} with a great many words in it so that \
                             taking a tenth off the width it is laid out in breaks its lines \
                             somewhere else entirely and the pixels stop lining up</p>"
                        )
                    })
                    .collect::<String>();
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: format!("<title>T</title><body style='margin:0'>{paragraphs}")
                        .into_bytes(),
                    charset: Some("utf-8".to_owned()),
                    final_url: format!("https://{url}/"),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(LongPage);
        go(&mut browser, "long.example");
        let draw = |browser: &mut Browser| {
            let mut target = otlyra_gfx::RecordingPainter::default();
            browser.paint(&mut target, Viewport::new(800, 600, 1.0));
        };
        draw(&mut browser);

        // Half way down, and read what is at the top of the window.
        // What is at the top of the window: `select_word_at` takes a window
        // point and a top inset and adds the scroll itself, so the top of the
        // window is `y == top`.
        let words = |browser: &mut Browser| {
            let page = browser.tabs[browser.active].page.as_mut().expect("a page");
            page.select_word_at(40.0, 0.0, 0.0);
            page.selected_text()
        };
        if let Some(page) = browser.tabs[browser.active].page.as_mut() {
            page.set_scroll(700.0);
        }
        draw(&mut browser);
        let before = words(&mut browser);
        assert!(before.is_some(), "there are words at the top of the window");

        browser.step_zoom(ZoomStep::In);
        draw(&mut browser);
        let after = browser.tabs[browser.active]
            .page
            .as_ref()
            .expect("a page")
            .scroll();
        assert_ne!(
            after, 700.0,
            "the page moved to keep the reader's place rather than staying at an \
             offset that now points at other words"
        );
        assert_eq!(
            words(&mut browser),
            before,
            "and the same words are at the top of the window"
        );
    }

    /// A press lands where the reader aimed it, whatever the zoom.
    ///
    /// The page is laid out in its own pixels, so a pointer arriving in the
    /// window's has to be converted — and a press answered in the coordinate
    /// system it did not land in is a link that opens when the pointer was
    /// somewhere else.
    #[test]
    fn a_press_on_a_zoomed_page_lands_where_it_was_aimed() {
        let mut browser = browser();
        go(&mut browser, "example.com");

        let draw = |browser: &mut Browser| {
            let mut target = otlyra_gfx::RecordingPainter::default();
            browser.paint(&mut target, Viewport::new(800, 600, 1.0));
        };
        let hit = |browser: &Browser, x: f64, y: f64| {
            let (x, y) = browser.in_page(x, y);
            browser.tabs[browser.active]
                .page
                .as_ref()
                .expect("a page")
                .box_at(x, y)
        };

        draw(&mut browser);
        // Thirty of the page's own pixels into its content, which unzoomed is
        // thirty of the window's below the chrome.
        let plain = hit(&mut browser, 30.0, UI_HEIGHT + 30.0);
        assert!(
            plain.is_some(),
            "the paragraph is under that point unzoomed"
        );

        browser.set_zoom(2.0);
        draw(&mut browser);
        // The same thirty page pixels, drawn twice as large: twice as far into
        // the window, and the chrome's own inset is not doubled with them.
        assert_eq!(
            hit(&mut browser, 60.0, UI_HEIGHT + 60.0),
            plain,
            "the same place in the page, aimed at where it is now drawn"
        );
    }

    /// A sheet written for another medium holds nothing back.
    ///
    /// The rule that stops a frame is *this page cannot be drawn right yet*, and
    /// a print-only sheet is never going to draw any of it. Holding the screen
    /// for one is holding it for nothing.
    #[test]
    fn a_print_stylesheet_does_not_hold_the_screen() {
        struct PrintSheet;

        impl Loader for PrintSheet {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                if url.ends_with(".css") {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    return Ok(Loaded {
                        content_type: Some("text/css".to_owned()),
                        bytes: b"p { color: #008000 }".to_vec(),
                        charset: Some("utf-8".to_owned()),
                        final_url: url.to_owned(),
                        ..Default::default()
                    });
                }
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes:
                        b"<title>T</title><link rel=stylesheet media=print href=/p.css><body><p>text"
                            .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: format!("https://{url}/"),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(PrintSheet);
        browser.navigate("print.example");
        browser.wait_for_load(std::time::Duration::from_millis(120));
        let active = browser.active;
        assert!(
            !browser.blocked_on_style(active),
            "a print-only sheet held the screen"
        );

        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));
        assert!(
            target.ops().iter().any(|op| {
                matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { glyphs, .. } if glyphs.len() == 4)
            }),
            "the document was drawn while the print sheet was still coming"
        );
    }

    /// A page from the network asking for a stylesheet on disk is the rule that
    /// keeps a web page out of the filesystem, and it holds for subresources and
    /// not only for navigation.
    #[test]
    fn a_web_page_may_not_link_a_stylesheet_on_disk() {
        struct DiskLoader;

        impl Loader for DiskLoader {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                assert!(
                    !url.starts_with("file:"),
                    "the loader must never be asked for {url}"
                );
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<link rel=stylesheet href=\"file:///etc/theme.css\"><body><p>x"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: "https://site.example/".to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(DiskLoader);
        browser.navigate("site.example");
        settle(&mut browser);
        assert!(browser.tabs[browser.active].page.is_some());
    }

    /// Where the link's text was actually painted, taken from the page's own
    /// targets rather than guessed.
    fn link_position(browser: &Browser) -> (f64, f64) {
        let page = browser.tabs[browser.active].page.as_ref().expect("page");
        let mut x = 0.0;
        let mut y = 0.0;
        for offset in 0..2000 {
            let candidate_x = 4.0 + f64::from(offset);
            let candidate_y = UI_HEIGHT + 30.0;
            if page.link_at(candidate_x, candidate_y).is_some() {
                x = candidate_x;
                y = candidate_y;
                break;
            }
        }
        assert!(x > 0.0, "the link should be somewhere on the first line");
        (x, y)
    }

    #[test]
    fn reloading_fetches_the_same_address_again() {
        let (mut browser, requested) = browser_with_log();
        type_url(&mut browser, "example.com");
        browser.reload();
        settle(&mut browser);

        assert_eq!(
            asked_for(&requested),
            ["example.com", "https://example.com/"],
            "the reload asks for where the first load ended up"
        );
    }

    #[test]
    fn stopping_a_load_rejects_its_late_document() {
        struct SlowSecondPage;

        impl Loader for SlowSecondPage {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                if url.contains("second") {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let title = if url.contains("second") {
                    "Second"
                } else {
                    "First"
                };
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: format!("<title>{title}</title><body>{title}").into_bytes(),
                    charset: Some("utf-8".to_owned()),
                    final_url: format!("https://{url}/"),
                    ..Default::default()
                })
            }
        }

        let mut browser = Browser::new(SlowSecondPage);
        go(&mut browser, "first.example");
        browser.navigate("second.example");
        assert!(browser.tabs[0].loading());

        browser.stop();
        assert!(
            !browser.tabs[0].loading(),
            "the spinner should stop at once"
        );

        std::thread::sleep(std::time::Duration::from_millis(75));
        browser.pump();
        let page = browser.tabs[0]
            .page
            .as_ref()
            .expect("the first page remains");
        assert_eq!(
            crate::page::title_of(page.document()).as_deref(),
            Some("First"),
            "the cancelled navigation arrived late and replaced the page"
        );
    }

    /// A reload keeps your place. For a page you are editing that is the whole
    /// value of the key.
    #[test]
    fn reloading_keeps_the_scroll_position() {
        let mut browser = Browser::new(LongLoader);
        browser.navigate("long.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.ui.pointer_moved(400.0, 400.0, &mut browser.text);
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 200.0,
            source: otlyra_platform::ScrollSource::Wheel,
            modifiers: Default::default(),
        });
        let scrolled = browser.tabs[0].page.as_ref().expect("page").scroll();
        assert!(scrolled > 0.0);

        browser.reload();
        settle(&mut browser);
        assert_eq!(
            browser.tabs[0].page.as_ref().expect("page").scroll(),
            scrolled
        );
    }

    #[test]
    fn reloading_a_blank_tab_does_nothing() {
        let (mut browser, requested) = browser_with_log();
        browser.reload();
        settle(&mut browser);
        assert!(asked_for(&requested).is_empty());
    }

    /// §14's rule: a page from the internet must never be able to open a file.
    #[test]
    fn a_web_page_may_not_navigate_to_a_file_url() {
        let (mut browser, requested) = browser_with_log();
        type_url(&mut browser, "example.com");
        browser.navigate_from("file:///etc/passwd", false);
        settle(&mut browser);

        assert_eq!(browser.tabs[0].url, "https://example.com/");
        assert!(
            browser.tabs[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Refused"))
        );
        assert_eq!(
            asked_for(&requested),
            ["example.com"],
            "the loader is never even asked"
        );
    }

    #[test]
    fn the_user_may_open_a_file_url_and_so_may_a_local_page() {
        let (mut browser, requested) = browser_with_log();
        type_url(&mut browser, "file:///tmp/one.html");
        assert_eq!(asked_for(&requested).len(), 1);

        browser.navigate_from("file:///tmp/two.html", false);
        settle(&mut browser);
        assert_eq!(
            asked_for(&requested).len(),
            2,
            "a local page's own link is allowed"
        );
    }

    /// A page long enough to scroll.
    struct LongLoader;

    impl Loader for LongLoader {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            let body = "<title>Long</title><body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: body.into_bytes(),
                charset: Some("utf-8".to_owned()),
                // A transport hands back the address it actually reached, and an
                // address that already has a scheme is one it reached as given.
                // Prepending one unconditionally made a fake that a second visit
                // to the same page — which is what going back is — turned into
                // `https://https://…`, and only the browser looked wrong.
                final_url: if url.contains("://") {
                    url.to_owned()
                } else {
                    format!("https://{url}/")
                },
                ..Default::default()
            })
        }
    }

    /// The content version of one layer in a composed scene.
    fn epoch_of(scene: &Scene, id: u64) -> u64 {
        scene
            .layers
            .iter()
            .find(|layer| layer.id == LayerId(id))
            .unwrap_or_else(|| panic!("layer {id} present"))
            .epoch
    }

    #[test]
    fn an_unchanged_frame_composes_to_the_same_layer_epochs() {
        let mut browser = Browser::new(LongLoader);
        go(&mut browser, "long.example");
        let viewport = Viewport::new(1024, 768, 2.0);

        // The first frame settles the caches; two more are what a no-op yields.
        let _ = browser.compose(viewport).expect("the interface composes");
        let before = browser.compose(viewport).expect("the interface composes");
        let after = browser.compose(viewport).expect("the interface composes");

        let ids: Vec<_> = before.layers.iter().map(|layer| layer.id).collect();
        assert!(ids.contains(&LayerId(LAYER_PAGE)), "a page layer");
        assert!(ids.contains(&LayerId(LAYER_CHROME)), "a chrome layer");
        assert_eq!(before.layers.len(), after.layers.len());
        for (b, a) in before.layers.iter().zip(after.layers.iter()) {
            assert_eq!(b.id, a.id);
            assert_eq!(b.rect, a.rect);
            assert_eq!(
                b.epoch, a.epoch,
                "layer {:?} is unchanged between two no-op frames",
                b.id
            );
        }
    }

    #[test]
    fn an_open_menu_expands_the_composited_chrome_layer() {
        let mut browser = Browser::new(LongLoader);
        let viewport = Viewport::new(2048, 1536, 2.0);
        let closed = browser.compose(viewport).expect("the interface composes");
        let closed_chrome = closed
            .layers
            .iter()
            .find(|layer| layer.id == LayerId(LAYER_CHROME))
            .expect("the chrome layer");
        assert_eq!(
            closed_chrome.rect.height,
            (UI_HEIGHT * viewport.scale_factor) as u32
        );

        // The real event route uses logical pointer coordinates at every scale.
        browser.handle_event(PlatformEvent::PointerMoved {
            x: viewport.logical_width() - 22.0,
            y: UI_HEIGHT - 21.0,
        });
        browser.handle_event(PlatformEvent::PointerPressed { clicks: 1 });
        assert!(browser.ui.menu_open(), "the cogwheel opened the menu");

        let open = browser.compose(viewport).expect("the interface composes");
        let open_chrome = open
            .layers
            .iter()
            .find(|layer| layer.id == LayerId(LAYER_CHROME))
            .expect("the chrome layer");
        assert_eq!(
            open_chrome.rect.height, viewport.height,
            "the popup must not be clipped at the toolbar"
        );
    }

    /// The display list of one layer in a composed scene.
    fn list_of(scene: &Scene, id: u64) -> Arc<otlyra_gfx::DisplayList> {
        Arc::clone(
            &scene
                .layers
                .iter()
                .find(|layer| layer.id == LayerId(id))
                .unwrap_or_else(|| panic!("layer {id} present"))
                .list,
        )
    }

    #[test]
    fn an_unchanged_layer_hands_back_the_device_list_it_handed_back_before() {
        let mut browser = Browser::new(LongLoader);
        go(&mut browser, "long.example");
        browser.inspector.open = true;
        let viewport = Viewport::new(1024, 768, 2.0);

        // Two frames after the caches have settled. Nothing moved between them, so
        // no layer may be built again, cloned, or scaled to device pixels again:
        // pointer identity is the only way to say that, because an equal list
        // built twice is exactly the work this is here to prevent.
        let _ = browser.compose(viewport).expect("the interface composes");
        let before = browser.compose(viewport).expect("the interface composes");
        let after = browser.compose(viewport).expect("the interface composes");

        for id in [LAYER_PAGE, LAYER_CHROME, LAYER_INSPECTOR] {
            assert!(
                Arc::ptr_eq(&list_of(&before, id), &list_of(&after, id)),
                "layer {id} was rebuilt or re-scaled for a frame that changed nothing"
            );
        }
    }

    #[test]
    fn a_changed_scale_scales_the_interface_again() {
        let mut browser = Browser::new(LongLoader);
        go(&mut browser, "long.example");

        let one = browser
            .compose(Viewport::new(1024, 768, 1.0))
            .expect("the interface composes");
        let chrome_at_one = list_of(&one, LAYER_CHROME);
        let two = browser
            .compose(Viewport::new(2048, 1536, 2.0))
            .expect("the interface composes");

        // The logical list is the same one — nothing in the interface changed —
        // but the device list it is scaled into is not, or the toolbar would be
        // drawn at half size on a retina display.
        assert!(!Arc::ptr_eq(&chrome_at_one, &list_of(&two, LAYER_CHROME)));
    }

    #[test]
    fn scrolling_the_page_moves_its_layer_epoch_and_leaves_the_chrome_alone() {
        let mut browser = Browser::new(LongLoader);
        go(&mut browser, "long.example");
        let viewport = Viewport::new(1024, 768, 2.0);

        let _ = browser.compose(viewport).expect("the interface composes");
        let before = browser.compose(viewport).expect("the interface composes");

        // Scroll the long page. Only the page's own list is rebuilt; the tab strip
        // and toolbar are drawing nothing new.
        browser.tabs[browser.active]
            .page
            .as_mut()
            .expect("a loaded page")
            .scroll_by(300.0);
        let after = browser.compose(viewport).expect("the interface composes");

        assert_ne!(
            epoch_of(&before, LAYER_PAGE),
            epoch_of(&after, LAYER_PAGE),
            "the page scrolled, so its layer must be re-rasterized"
        );
        assert_eq!(
            epoch_of(&before, LAYER_CHROME),
            epoch_of(&after, LAYER_CHROME),
            "the chrome did not change, so the compositor leaves it untouched"
        );
    }

    #[test]
    fn a_press_on_the_page_blurs_the_address_field() {
        let mut browser = Browser::new(LongLoader);
        go(&mut browser, "long.example");
        // A frame first: the field to focus and the layout to press against are
        // both things the last frame drew.
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(1024, 768, 1.0));

        browser.ui.focus_address();
        assert!(browser.ui.address_focused(), "the address starts focused");

        browser.on_event(PlatformEvent::PointerMoved { x: 500.0, y: 400.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        assert!(
            !browser.ui.address_focused(),
            "a press on the page takes the focus off the address field"
        );
    }

    #[test]
    fn a_press_on_a_system_page_blurs_the_address_field() {
        // The system-page press paths answer the click and return before the
        // toolbar's own handler, so this is the case that regressed.
        let mut browser = Browser::new(LongLoader);
        browser.open_system(SystemPage::Settings);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(1024, 768, 1.0));

        browser.ui.focus_address();
        assert!(browser.ui.address_focused(), "the address starts focused");

        browser.on_event(PlatformEvent::PointerMoved { x: 500.0, y: 400.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        assert!(
            !browser.ui.address_focused(),
            "a press on a system page blurs the address field too"
        );
    }

    #[test]
    fn the_interface_and_the_page_both_reach_the_paint_seam() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");

        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 2.0));
        let ops = painter.take();

        assert!(
            ops.iter()
                .filter(|op| matches!(op, otlyra_gfx::PaintOp::DrawGlyphs { .. }))
                .count()
                >= 2,
            "the page's text and the interface's own"
        );
    }
}

/// Finding a run of characters in the page, from the bar down to the wash.
#[cfg(test)]
mod find_tests {
    use super::tests::*;
    use super::*;

    /// The platform's own accelerator modifier, whichever platform this is.
    const ACCELERATOR: Modifiers = Modifiers {
        command: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        shift: false,
        alt: false,
    };

    fn frame(browser: &mut Browser) {
        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(800, 600, 1.0));
    }

    fn key(browser: &mut Browser, key: Key, modifiers: Modifiers) {
        browser.on_event(PlatformEvent::KeyPressed { key, modifiers });
    }

    /// Open the bar and type `query` into it, the way a reader does.
    fn look_for(browser: &mut Browser, query: &str) {
        frame(browser);
        key(browser, Key::Character('f'), ACCELERATOR);
        frame(browser);
        for character in query.chars() {
            browser.on_event(PlatformEvent::TextInput(character));
        }
    }

    /// A page holding the same word three times, so stepping has somewhere to go.
    fn three_needles() -> Browser {
        let mut browser = browser();
        go(&mut browser, "file:///a/needle/needle/needle");
        browser
    }

    /// ⌘F, a query, and the page is searched: the count reaches the bar and the
    /// wash reaches the page.
    #[test]
    fn the_bar_searches_the_page_and_steps_through_what_it_found() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");

        assert!(browser.ui.finding());
        assert_eq!(
            browser.ui.find_status,
            crate::ui::FindStatus {
                total: 3,
                current: 1
            },
            "the bar counts what the page found"
        );
        let page = browser.tabs[0].page.as_ref().expect("a loaded page");
        assert_eq!(page.match_count(), 3);
        assert_eq!(
            page.match_rects().len(),
            3,
            "and every one of them has somewhere to be drawn"
        );

        // Return steps on, shift-Return steps back, and both wrap.
        key(&mut browser, Key::Enter, Modifiers::default());
        assert_eq!(browser.ui.find_status.current, 2);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        key(&mut browser, Key::Enter, shift);
        assert_eq!(browser.ui.find_status.current, 1);
        key(&mut browser, Key::Enter, shift);
        assert_eq!(browser.ui.find_status.current, 3, "round the start");

        // Escape closes the bar and takes the wash off the page with it.
        key(&mut browser, Key::Escape, Modifiers::default());
        assert!(!browser.ui.finding());
        let page = browser.tabs[0].page.as_ref().expect("a loaded page");
        assert_eq!(page.match_count(), 0);
        assert!(page.match_rects().is_empty());
    }

    /// A query nothing on the page holds is still a query: the bar says none
    /// rather than saying nothing, and there is nothing to step to.
    #[test]
    fn a_query_the_page_does_not_hold_counts_none() {
        let mut browser = three_needles();
        look_for(&mut browser, "haystack");

        assert_eq!(
            browser.ui.find_status,
            crate::ui::FindStatus {
                total: 0,
                current: 0
            }
        );
        key(&mut browser, Key::Enter, Modifiers::default());
        assert_eq!(browser.ui.find_status.current, 0, "nowhere to step to");
    }

    /// ⌘G steps without the bar holding the keyboard, which is what lets a
    /// reader look at the page they searched.
    #[test]
    fn command_g_steps_while_the_page_has_the_keyboard() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");
        assert_eq!(browser.ui.find_status.current, 1);

        // The keyboard goes back to the document.
        browser.ui.blur();
        browser.activate_surface(SURFACE_PAGE);
        assert!(!browser.ui.find_focused());

        key(&mut browser, Key::Character('g'), ACCELERATOR);
        assert_eq!(browser.ui.find_status.current, 2);
        key(
            &mut browser,
            Key::Character('g'),
            Modifiers {
                shift: true,
                ..ACCELERATOR
            },
        );
        assert_eq!(browser.ui.find_status.current, 1);
        assert!(
            browser.ui.finding(),
            "stepping never took the bar away or gave it the keyboard"
        );
        assert!(!browser.ui.find_focused());
    }

    /// A search belongs to the page it was made in: another tab has its own, and
    /// going somewhere else leaves it behind.
    #[test]
    fn a_search_belongs_to_its_tab_and_goes_when_the_page_does() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");
        assert_eq!(browser.ui.find_status.total, 3);

        // A second tab is not searching anything, so it shows no bar.
        browser.new_tab();
        go(&mut browser, "example.com");
        assert!(!browser.ui.finding(), "the bar came along to another tab");

        // Back to the first, which still is: the query is the page's, so the
        // bar is the page's search made visible rather than a copy of it.
        browser.select_tab(0);
        assert!(browser.ui.finding());
        assert_eq!(browser.ui.find.text(), "needle");
        assert_eq!(browser.ui.find_status.total, 3);

        // And going somewhere else in that tab leaves the search behind, because
        // the page it was a search of is gone.
        go(&mut browser, "example.com");
        assert!(!browser.ui.finding());
        assert_eq!(browser.ui.find_status.total, 0);
    }

    /// ⌘C in the bar's field copies what is selected in it, rather than the
    /// page's selection or nothing at all.
    #[test]
    fn command_c_in_the_find_bar_copies_the_bars_own_text() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");
        assert_eq!(browser.ui.find.text(), "needle");

        // Select the whole query the way ⌘A does, then copy it.
        key(&mut browser, Key::Character('a'), ACCELERATOR);
        assert_eq!(browser.ui.find.selected_text(), Some("needle"));
        key(&mut browser, Key::Character('c'), ACCELERATOR);
        assert_eq!(
            browser.clipboard.read().as_deref(),
            Some("needle"),
            "⌘C in the find bar copied something else"
        );
    }

    /// Ctrl+C on a platform whose accelerator is ⌘ is not a copy — and must not
    /// become a character in the query either.
    #[test]
    fn a_control_key_that_is_not_the_accelerator_types_nothing_into_the_bar() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");

        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        key(&mut browser, Key::Character('c'), control);
        assert_eq!(
            browser.ui.find.text(),
            "needle",
            "a control chord left a character in the query"
        );
        assert_eq!(
            browser.ui.find_status.total, 3,
            "and changed what was found"
        );
    }

    /// A double-click in the bar's field takes the word under it, the way it
    /// does in the address field: selecting is what a copy needs first.
    #[test]
    fn the_pointer_selects_inside_the_find_bars_field() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");
        frame(&mut browser);

        // Where the field was drawn, from the frame that drew it.
        let field = browser
            .ui
            .describe()
            .into_iter()
            .rfind(|node| node.role == crate::widget::Role::TextInput)
            .expect("the bar's field");
        let (x, y) = (
            field.rect.x + field.rect.width / 2.0,
            field.rect.y + field.rect.height / 2.0,
        );

        browser.on_event(PlatformEvent::PointerMoved { x, y });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 2 });
        browser.on_event(PlatformEvent::PointerReleased);
        assert_eq!(
            browser.ui.find.selected_text(),
            Some("needle"),
            "a double-click in the field selected nothing"
        );

        key(&mut browser, Key::Character('c'), ACCELERATOR);
        assert_eq!(browser.clipboard.read().as_deref(), Some("needle"));
    }

    /// What was found is what is selected, so ⌘C copies it — with the bar open
    /// and, once the bar has been closed, still.
    #[test]
    fn the_current_match_is_the_selection_and_can_be_copied() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");

        let page = browser.tabs[0].page.as_ref().expect("a loaded page");
        assert_eq!(
            page.selected_text().as_deref(),
            Some("needle"),
            "the match a reader was taken to is what is selected"
        );

        // With the keyboard in the document, ⌘C copies the page's selection.
        browser.ui.blur();
        browser.activate_surface(SURFACE_PAGE);
        key(&mut browser, Key::Character('c'), ACCELERATOR);
        assert_eq!(browser.clipboard.read().as_deref(), Some("needle"));

        // And closing the bar leaves the last match selected, the way every
        // browser does: the wash goes and what was found stays copyable.
        key(&mut browser, Key::Escape, Modifiers::default());
        let page = browser.tabs[0].page.as_ref().expect("a loaded page");
        assert_eq!(page.match_count(), 0, "the wash is gone");
        assert_eq!(page.selected_text().as_deref(), Some("needle"));
    }

    /// A resize renumbers every run, and what was selected has to follow the
    /// match rather than whatever took its number.
    #[test]
    fn a_resize_keeps_the_selection_on_the_match_it_was_on() {
        let mut browser = three_needles();
        look_for(&mut browser, "needle");
        frame(&mut browser);

        let mut target = otlyra_gfx::RecordingPainter::default();
        browser.paint(&mut target, Viewport::new(360, 600, 1.0));

        let page = browser.tabs[0].page.as_ref().expect("a loaded page");
        assert_eq!(page.match_count(), 3, "still three of them, laid out anew");
        assert_eq!(
            page.selected_text().as_deref(),
            Some("needle"),
            "the selection followed the match across the relayout"
        );
    }
}
