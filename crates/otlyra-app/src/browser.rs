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
use crate::fetcher::{Body, Fetched, Fetcher, Loader, ResourceKind};
use crate::page::{PageScene, title_of};
use crate::settings::{self, SettingsSurface};
use crate::ui::{BrowserUi, SystemPage, TabLabel, UI_HEIGHT, UiAction};

/// How long a caller with no event loop waits between checks for a finished fetch.
const FETCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

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
    /// The mark shown on an empty tab. `None` if it failed to decode, which is a
    /// cosmetic problem and not a reason to refuse to draw a frame.
    mark: Option<otlyra_gfx::peniko::ImageData>,
    /// Where the pointer is, in window logical pixels.
    pointer: (f64, f64),
    /// What the pointer should look like where it last was.
    cursor: Cursor,
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
    /// What the platform last said the environment is. What *System* follows.
    scheme: otlyra_platform::ColorScheme,
    /// The palette every surface is currently drawn from.
    theme: crate::widget::theme::Theme,
    /// Whether the platform needs a new accessibility tree after the next frame.
    accessibility_dirty: bool,
    /// The page's logical list, the scale it was scaled at, and the device list
    /// that resulted. Lets an unchanged page skip re-scaling: while the page
    /// hands back the same `Arc` and the scale holds, the device list is reused.
    page_device: Option<(
        Arc<otlyra_gfx::DisplayList>,
        f64,
        Arc<otlyra_gfx::DisplayList>,
    )>,
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
        let mut browser = Self {
            text: TextEngine::new(),
            ui: BrowserUi::new(),
            tabs: vec![Tab::blank()],
            active: 0,
            fetcher: Fetcher::spawn(loader),
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
            mark: otlyra_gfx::decode_image(crate::MARK)
                .inspect_err(|error| tracing::error!(%error, "the mark failed to decode"))
                .ok(),
            pointer: (0.0, 0.0),
            cursor: Cursor::Default,
            settings: SettingsSurface::with(settings),
            inspector: crate::inspector::Inspector::new(),
            about: AboutSurface::new(),
            clipboard: Box::new(crate::clipboard::InMemory::default()),
            history: crate::history::HistoryStore::default(),
            history_page: crate::history::HistorySurface::new(),
            scheme: otlyra_platform::ColorScheme::Light,
            theme: crate::widget::theme::Theme::light(),
            accessibility_dirty: true,
            page_device: None,
        };
        browser.apply_theme();
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
        // Reload keeps the entry it is on: a page loaded twice is one place, and
        // going back from it must reach where you were before it, not itself.
        self.start_load(&url, false, false, scroll);
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

        // A browser's own page is fetched from nothing and parsed from nothing:
        // it is a surface this program draws. Catching it in the one place every
        // navigation passes through — the menu, the address bar, the command
        // line, and a step through the history — is what makes it a place a tab
        // can be rather than a mode the window is in.
        if let Some(page) = SystemPage::from_url(url) {
            self.show_system(page, record, restore_scroll);
            return;
        }
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
        let id = self.fetcher.send(url, ResourceKind::Document, body);
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
        self.ui.address.set_text(url);
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
            self.ui.address.set_text(&final_url);
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
            return;
        }
        self.tabs.remove(index);
        self.active = self.active.min(self.tabs.len() - 1);
        self.sync_address();
    }

    /// Make a tab active.
    pub fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.sync_address();
        }
    }

    /// Put the active tab's URL back in the address bar.
    fn sync_address(&mut self) {
        let url = self.tabs[self.active].url.clone();
        self.ui.address.set_text(url);
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

    fn close_settings_if(&mut self, action: &settings::Action) {
        if *action == settings::Action::Close {
            self.close_system_page();
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
            | UiAction::ToggleMenu
            | UiAction::CloseMenu
            | UiAction::ScrollTabs(_) => {}
            UiAction::ToggleInspector => self.inspector.toggle(),
            // Chosen from the menu, a browser page opens beside what you were
            // reading rather than over it: the menu is reached *while* looking
            // at something, and losing that to check a preference is the whole
            // reason browsers open these in a tab of their own. Typing the same
            // address, which is a decision to leave, still navigates in place.
            UiAction::OpenPage(page) => self.open_system_in_new_tab(page),
            UiAction::Navigate(url) => self.navigate(&url),
            UiAction::Back => self.go_back(),
            UiAction::Forward => self.go_forward(),
            UiAction::NewTab => self.new_tab(),
            UiAction::CloseTab(index) => self.close_tab(index),
            UiAction::SelectTab(index) => self.select_tab(index),
            UiAction::Reload => self.reload(),
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

    /// The panel below it.
    fn paint_inspector(
        &mut self,
        list: &mut otlyra_gfx::DisplayList,
        width: f64,
        top: f64,
        content_height: f64,
        dock: f64,
    ) {
        let chosen = self.chosen_box();
        let panel = crate::ui::Rect::new(0.0, top + content_height, width, dock);
        // Everything the panel is shown about the page, gathered before it is
        // built: the panel reads, and the browser is what does the reaching.
        let mut built = otlyra_gfx::DisplayList::new();
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
            .build_display_list(panel, &facts, &mut self.text, &mut built);
        list.append(&built);
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
        } else if y < UI_HEIGHT || self.ui.menu_open {
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
                Some(SystemPage::About) => self.about.cursor_at(x, y, &mut self.text),
                Some(_) => Cursor::Default,
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
                _ => self
                    .about
                    .build_display_list(content, &mut self.text, &mut list),
            }
            list.transform(g.scale);
            Arc::new(list)
        } else if self.tabs[self.active].page.is_some() {
            // Told before the frame is built, because it decides what `medium`
            // computes to and every element that inherited a size inherited that.
            let logical = {
                let page = self.tabs[self.active].page.as_mut().expect("a page");
                page.set_text_scale(g.text_scale);
                page.set_color_scheme(g.page_scheme);
                page.build_display_list(
                    &mut self.text,
                    g.width as f32,
                    g.content_height as f32,
                    g.top as f32,
                )
            };
            self.scaled_page(logical, g.scale_factor)
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
        if let Some((cached_logical, cached_scale, device)) = &self.page_device
            && Arc::ptr_eq(cached_logical, &logical)
            && *cached_scale == scale
        {
            return Arc::clone(device);
        }
        let mut scaled = (*logical).clone();
        scaled.transform(otlyra_gfx::kurbo::Affine::scale(scale));
        let device = Arc::new(scaled);
        self.page_device = Some((logical, scale, Arc::clone(&device)));
        device
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
        let mut list = otlyra_gfx::DisplayList::new();
        self.paint_inspector(&mut list, g.width, g.top, g.content_height, g.dock);
        list.transform(g.scale);
        Arc::new(list)
    }

    /// The tab strip and toolbar.
    fn chrome_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
        let mut list = otlyra_gfx::DisplayList::new();
        let labels = self.labels();
        self.ui.build_display_list(
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
            &mut list,
        );
        list.transform(g.scale);
        Arc::new(list)
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
                _ => 12u8,
            }
            .hash(&mut hasher);
            self.settings.builds().hash(&mut hasher);
            self.history_page.builds().hash(&mut hasher);
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
        self.fetcher.set_waker(waker);
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
        tab.page
            .as_ref()
            .and_then(crate::page::PageScene::next_caret_frame)
            .map_or(FrameRequest::None, FrameRequest::At)
    }

    fn work_counters(&self) -> PainterWork {
        let legacy = self.settings.builds()
            + self.history_page.builds()
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
            || self.ui.menu_open
            || self.ui.pointer_captured();
        let inspector_changed =
            self.inspector.open && (previous_pointer.1 >= dock_top || y >= dock_top);
        let system_page_changed = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.system.is_some())
            && (previous_pointer.1 >= UI_HEIGHT || y >= UI_HEIGHT);
        let request = if chrome_changed
            || inspector_changed
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
            // Something finished on the fetch thread. What it was is the browser's
            // business; the loop only knows it should ask.
            PlatformEvent::Woken => {
                self.pump();
            }

            PlatformEvent::AppearanceChanged(scheme) => {
                self.scheme = scheme;
                self.apply_theme();
            }

            PlatformEvent::PointerMoved { x, y } => {
                self.pointer = (x, y);
                self.ui.pointer_moved(x, y, &mut self.text);
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
                    let top = UI_HEIGHT as f32;
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        page.select_to(x as f32, y as f32, top);
                        return;
                    }
                }

                // A scrollbar being dragged keeps the pointer until it is let go,
                // wherever the pointer wanders.
                let (width, height) = (self.last_width, self.last_height - UI_HEIGHT);
                if let Some(page) = self.tabs[self.active].page.as_mut()
                    && page.dragging_scrollbar()
                {
                    page.drag_scrollbar(
                        (y - UI_HEIGHT) as f32,
                        width as f32,
                        height.max(0.0) as f32,
                    );
                    return;
                }
                // The page follows the pointer: `:hover` on what it is over, and
                // the widget under it drawn as hovered. Nothing is repainted unless
                // something depends on it, which for most pages is never.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let over_page = y >= UI_HEIGHT && y < self.dock_top();
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        let _ = if over_page {
                            page.pointer_moved(x, y)
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
                        self.close_settings_if(&action);
                    }
                    Some(SystemPage::History) => self.history_page.pointer_moved(x, y),
                    Some(SystemPage::About) => self.about.pointer_moved(x, y),
                    _ => {}
                }
            }

            PlatformEvent::PointerPressed { clicks } => {
                // A press below the toolbar takes the focus off the address field,
                // wherever it lands — a page, a control, a link, a scrollbar, a
                // system page, the inspector. Every one of those paths answers the
                // press and returns before the toolbar's own press handler runs, so
                // without this the caret and its selection would sit in a field the
                // reader has plainly clicked away from.
                if self.pointer.1 >= UI_HEIGHT && !self.ui.menu_open {
                    self.ui.blur();
                }
                // The panel owns everything below its own top edge.
                if self.pointer.1 >= self.dock_top() && !self.ui.menu_open {
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
                    let (x, y) = self.pointer;
                    let (width, height) = (self.last_width, self.last_height - UI_HEIGHT);
                    if let Some(page) = self.tabs[self.active].page.as_mut()
                        && page.grab_scrollbar(
                            x as f32,
                            (y - UI_HEIGHT) as f32,
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
                if self.pointer.1 >= UI_HEIGHT && !self.ui.menu_open {
                    match self.tabs[self.active].system {
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            let action = self.settings.pointer_pressed(clicks);
                            self.save_preferences_if_changed(&before);
                            self.close_settings_if(&action);
                            return;
                        }
                        Some(SystemPage::History) => {
                            let action = self.history_page.pointer_pressed(clicks);
                            self.handle_history_action(action);
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
                    let (x, y) = self.pointer;
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
                }
                // A press on the page starts a selection where it landed, and takes
                // away whatever was selected before — which is what a press on a
                // page means everywhere else.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let (x, y) = self.pointer;
                    let top = UI_HEIGHT as f32;
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
            }

            PlatformEvent::PointerReleased => {
                self.selecting = false;
                let (x, y) = self.pointer;
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
                    self.inspector.toggle();
                    return;
                }
                // The panel takes the keys that walk its tree, but only while it
                // is the thing being looked at — and a caret in the address
                // field means the field is, however open the panel may be.
                //
                // With or without a document: a tab whose load failed still has
                // a console to filter and clear, and gating the panel's keys on
                // a page would take them away exactly when they are wanted.
                if self.inspector.open
                    && !self.ui.address_focused()
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
                    return;
                }
                // A browser page shown in the tab gets the key first: it is what
                // the reader is looking at, and Tab on it walks its own controls
                // rather than the toolbar's.
                match self.tabs[self.active].system {
                    Some(SystemPage::Settings) => {
                        let before = self.settings.settings.clone();
                        if let Some(action) =
                            self.settings
                                .key_pressed(key, modifiers, self.clipboard.as_mut())
                        {
                            self.save_preferences_if_changed(&before);
                            self.close_settings_if(&action);
                            return;
                        }
                    }
                    Some(SystemPage::History) => {
                        if let Some(action) =
                            self.history_page
                                .key_pressed(key, modifiers, self.clipboard.as_mut())
                        {
                            self.handle_history_action(action);
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
                // Copying what is selected on the page, before the interface reads
                // the key: the address bar takes ⌘C for its own text only while it
                // holds the caret, and the page's selection is the one on screen.
                if key == Key::Character('c')
                    && modifiers.command
                    && !self.ui.address_focused()
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
                    && !self.ui.address_focused()
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
                if !self.ui.address_focused()
                    && self.tabs[self.active].system.is_none()
                    && self.page_edit_key(key, modifiers)
                {
                    return;
                }
                if !self.ui.address_focused()
                    && self.tabs[self.active].system.is_none()
                    && self.page_selection_key(key, modifiers)
                {
                    return;
                }

                let action =
                    self.ui
                        .key_pressed(key, modifiers, &mut self.text, self.clipboard.as_mut());
                if action == UiAction::None && !self.ui.address_focused() {
                    self.scroll_by_key(key);
                }
                self.apply(action);
            }

            PlatformEvent::TextInput(character) => {
                // The panel's own fields, while one holds the caret. Before the
                // pages below, because the panel is drawn over them and a caret
                // is where the typing goes.
                if self.inspector.text_input(character) {
                    return;
                }
                // A field *in the page* holds the caret, and the interface's own
                // does not. One keyboard between them, so the page only gets the
                // letters nothing above it wanted.
                if !self.ui.address_focused()
                    && self.tabs[self.active].system.is_none()
                    && let Some(page) = self.tabs[self.active].page.as_mut()
                    && page.typed(&character.to_string())
                {
                    return;
                }
                if self.tabs[self.active].system == Some(SystemPage::History)
                    && self.history_page.text_input(character)
                {
                    return;
                }
                if self.tabs[self.active].system == Some(SystemPage::Settings) {
                    let before = self.settings.settings.clone();
                    if self.settings.text_input(character) {
                        // Typing in the home field is a preference changing, one
                        // character at a time.
                        self.save_preferences_if_changed(&before);
                        return;
                    }
                }
                self.ui.text_input(character);
            }

            // Scrolling belongs to the page unless the pointer is over the
            // interface, where there is nothing to scroll.
            //
            // Every one of these adds the delta to an offset and none of them
            // negates it. The event already says which way the reader went, and
            // a consumer that decided that for itself is how the settings came
            // to scroll the opposite way from a document.
            PlatformEvent::Scroll { x, y, .. } => {
                if self.ui.owns_pointer() {
                    // The tab strip is a thing under the pointer like any other,
                    // and a strip with more tabs than it can show is a strip the
                    // wheel should move. Whichever axis the wheel reported the
                    // more of: a mouse with one wheel says `y` and a trackpad
                    // swiped sideways says `x`, and both mean the same thing to a
                    // strip that only runs one way.
                    if self.pointer.1 < crate::ui::TAB_STRIP_HEIGHT && !self.ui.menu_open {
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
                } else if let Some(page) = self.tabs[self.active].page.as_mut() {
                    // The wheel goes to whatever is under the pointer: a box that
                    // scrolls takes it first, and the page takes it once that box
                    // has reached its end.
                    let (x, pointer_y) = self.pointer;
                    page.scroll_at(x as f32, (pointer_y - UI_HEIGHT) as f32, y as f32);
                }
            }

            // The menu and the keyboard reach the same commands: one definition of
            // what each means, invoked from wherever the user found it.
            PlatformEvent::MenuCommand(id) => match crate::menu::Command::from_id(id) {
                Some(crate::menu::Command::Reload | crate::menu::Command::ReloadIgnoringCache) => {
                    self.reload();
                }
                Some(crate::menu::Command::Back) => self.go_back(),
                Some(crate::menu::Command::Forward) => self.go_forward(),
                Some(crate::menu::Command::NewTab) => self.new_tab(),
                Some(crate::menu::Command::CloseTab) => self.close_tab(self.active),
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
                    let action = self.ui.activate_described(index, &mut self.text);
                    self.apply(action);
                    return;
                }

                let index = index - toolbar;
                match self.tabs.get(self.active).and_then(|tab| tab.system) {
                    Some(SystemPage::Settings) => {
                        let before = self.settings.settings.clone();
                        let action = self.settings.activate_described(index);
                        self.save_preferences_if_changed(&before);
                        self.close_settings_if(&action);
                    }
                    Some(SystemPage::History) => {
                        let action = self.history_page.activate_described(index);
                        self.handle_history_action(action);
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
            Some(SystemPage::About) => (self.about.focused(), self.about.describe()),
            // The pages that are still a placeholder draw no controls, so they
            // describe none.
            _ => (None, Vec::new()),
        };
        described.extend(page_described);

        // Whichever of the two is holding the keyboard. They cannot both be: a
        // press on one takes the focus off the other, which is what the surfaces
        // already do to each other.
        let focused = self.ui.focused().or(page_focus);

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
        layers.push(SceneLayer {
            id: LayerId(LAYER_PAGE),
            rect: page_rect,
            epoch: self.page_epoch(&geom),
            list: page,
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
            });
        }

        let chrome = self.chrome_list(&geom);
        layers.push(SceneLayer {
            id: LayerId(LAYER_CHROME),
            rect: LayerRect {
                x: 0,
                y: 0,
                width: viewport.width,
                height: content_top,
            },
            epoch: self.chrome_epoch(),
            list: chrome,
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
        assert!(browser.ui().menu_open, "the cogwheel opens the menu");

        // The panel hangs below the toolbar at the right-hand edge; its rows are
        // 30 tall under a heading, so this is the first of them.
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 34.0);

        assert!(
            !browser.ui().menu_open,
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
        assert!(!browser.ui().menu_open);
        assert_eq!(browser.system_page(), Some(SystemPage::History));
    }

    #[test]
    fn a_row_for_a_page_that_does_not_exist_yet_only_closes_the_menu() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame(&mut browser, 1000.0, 700.0);

        // The third row is Bookmarks, which is dimmed: the press falls through
        // it to the sheet behind the panel, which dismisses the menu and does
        // nothing else. That is the whole of what a disabled row does.
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 96.0);
        assert!(!browser.ui().menu_open);
        assert_eq!(browser.system_page(), None);
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

        browser.close_settings_if(&crate::settings::Action::Close);
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

        browser.close_settings_if(&crate::settings::Action::Close);
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
    fn a_page_that_does_not_exist_yet_says_so_instead_of_showing_nothing() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:downloads");

        assert_eq!(browser.system_page(), None);
        assert!(
            browser.tabs()[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Downloads")),
            "the tab says which page is missing"
        );
    }

    #[test]
    fn leaving_the_settings_leaves_the_tab_blank_rather_than_still_on_them() {
        let mut browser = Browser::new(NoNetwork);
        browser.navigate("about:settings");
        browser.close_settings_if(&crate::settings::Action::Close);

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

    fn browser() -> Browser {
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
    fn go(browser: &mut Browser, url: &str) {
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

        browser.ui.menu_open = true;
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
            !browser.ui.menu_open,
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
        });
        let page = browser.tabs[0].page.as_ref().expect("a page").scroll();
        assert!(page > 0.0, "a positive delta goes down the document");

        browser.open_system(SystemPage::Settings);
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));
        browser.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 120.0,
            source: otlyra_platform::ScrollSource::Wheel,
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

        browser.close_settings_if(&settings::Action::Close);
        settle(&mut browser);
        assert_eq!(
            browser.tabs[0].url, document,
            "the reader goes back to what they were reading"
        );

        // And with nowhere to go back to, an empty tab rather than a settings
        // page nobody can leave.
        let mut fresh = Browser::new(LongLoader);
        fresh.open_system(SystemPage::Settings);
        fresh.close_settings_if(&settings::Action::Close);
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
