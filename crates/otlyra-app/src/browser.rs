//! The browser itself: tabs, navigation, and the loop's `Painter`.
//!
//! One window, several tabs, one of them active. Each tab owns its document and
//! its scroll position; the interface owns what is typed and what is focused; this
//! type owns the two of them and the one thing they share, the font engine.

use std::collections::HashMap;

use otlyra_css::cascade::ExternalSheets;
use otlyra_dom::NodeId;
use otlyra_gfx::{PaintTarget, render};
use otlyra_layout::Images;
use otlyra_platform::{Cursor, Key, Painter, PlatformEvent, Viewport, Waker};
use otlyra_text::TextEngine;

use crate::about::{self, AboutSurface};
use crate::fetcher::{Fetched, Fetcher, Loader, ResourceKind};
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
    /// What each outstanding request will feed once it arrives.
    outstanding: HashMap<u64, Vec<PendingResource>>,
}

/// What a subresource is for once it lands.
enum PendingResource {
    /// The `<link>` whose stylesheet this is.
    Stylesheet(NodeId),
    /// The `<img>` whose picture this is.
    Image(NodeId),
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

impl Tab {
    /// A blank tab.
    pub fn blank() -> Self {
        Self {
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
    /// Pictures that have already been decoded.
    images: ImageCache,
    /// The width of the last frame, so a press can be tested against the geometry
    /// the user was actually looking at.
    last_width: f64,
    /// And its height, which is what a page key scrolls by.
    last_height: f64,
    /// The mark shown on an empty tab. `None` if it failed to decode, which is a
    /// cosmetic problem and not a reason to refuse to draw a frame.
    mark: Option<otlyra_gfx::peniko::ImageData>,
    /// Where the pointer is, in window logical pixels.
    pointer: (f64, f64),
    /// The preferences.
    ///
    /// One surface for the whole browser rather than one per tab: a preference
    /// is the browser's, and two tabs showing two copies of it could disagree
    /// about what it currently says.
    settings: SettingsSurface,
    /// What this program is.
    about: AboutSurface,
}

impl Browser {
    /// A browser with one blank tab, fetching through `loader`.
    pub fn new<L: Loader>(loader: L) -> Self {
        Self {
            text: TextEngine::new(),
            ui: BrowserUi::new(),
            tabs: vec![Tab::blank()],
            active: 0,
            fetcher: Fetcher::spawn(loader),
            load_started: std::time::Instant::now(),
            images: ImageCache::default(),
            last_width: 1024.0,
            last_height: 768.0,
            mark: otlyra_gfx::decode_image(crate::MARK)
                .inspect_err(|error| tracing::error!(%error, "the mark failed to decode"))
                .ok(),
            pointer: (0.0, 0.0),
            settings: SettingsSurface::new(),
            about: AboutSurface::new(),
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

    /// Load `url` into the active tab.
    ///
    /// The load is synchronous, and that is temporary: the event loop must never
    /// wait on the network, which is why the loader's method is the shape it is and
    /// why M9 replaces this body with a channel and a pending state rather than
    /// replacing the caller.
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
        let tab = &mut self.tabs[self.active];
        let scroll = tab.page.as_ref().map_or(0.0, |page| page.scroll());
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
        let _span = tracing::info_span!("navigation", url).entered();

        // A browser's own page is fetched from nothing and parsed from nothing:
        // it is a surface this program draws. Catching it in the one place every
        // navigation passes through — the menu, the address bar, the command
        // line, and a step through the history — is what makes it a place a tab
        // can be rather than a mode the window is in.
        if let Some(page) = SystemPage::from_url(url) {
            self.show_system(page, record);
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
        let id = self.fetcher.request(url, ResourceKind::Document);
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

    /// One finished fetch. Returns whether it changed anything on screen.
    fn receive(&mut self, fetched: Fetched) -> bool {
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
        let pictures = otlyra_layout::image_sources(&parsed.document);
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
                        ready.insert(source.node, image);
                        false
                    }
                    None => true,
                }
            })
            .map(|source| {
                (
                    source.src.clone(),
                    PendingResource::Image(source.node),
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
                    .any(|resource| matches!(resource, PendingResource::Image(_)))
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
                        PendingResource::Image(node) => match decoded.as_ref() {
                            Some(image) => {
                                pending.images.insert(node, image.clone());
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
    fn show_system(&mut self, page: SystemPage, record: bool) {
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
        self.ui.address_focused = true;
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
    fn close_settings_if(&mut self, action: &settings::Action) {
        if *action != settings::Action::Close {
            return;
        }
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

    fn apply(&mut self, action: UiAction) {
        match action {
            UiAction::None => {}
            // Focus and the menu belong to the interface and are settled there:
            // the press handler applies them and reports `None`, so these arms
            // are only here to keep the match honest about the whole enum.
            UiAction::FocusAddress | UiAction::ToggleMenu | UiAction::CloseMenu => {}
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
                title: tab.title.clone(),
                loading: tab.loading(),
            })
            .collect()
    }
}

impl Painter for Browser {
    fn set_waker(&mut self, waker: Waker) {
        self.fetcher.set_waker(waker);
        // Anything that finished before the loop had a waker to be woken by is
        // sitting in the channel: a page asked for on the command line usually
        // arrives before the window exists.
        self.pump();
    }

    /// A frame at the display's pace while something is loading, so the spinner
    /// turns; nothing at all when the browser is idle.
    fn animating(&self) -> bool {
        self.tabs.iter().any(Tab::loading)
    }

    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            // Something finished on the fetch thread. What it was is the browser's
            // business; the loop only knows it should ask.
            PlatformEvent::Woken => {
                self.pump();
            }

            PlatformEvent::PointerMoved { x, y } => {
                self.pointer = (x, y);
                self.ui.pointer_moved(x, y);

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
                match self.tabs[self.active].system {
                    // Moves matter to a surface that has a slider on it: that is
                    // what a drag is made of.
                    Some(SystemPage::Settings) => {
                        let action = self.settings.pointer_moved(x, y);
                        self.close_settings_if(&action);
                    }
                    Some(SystemPage::About) => self.about.pointer_moved(x, y),
                    _ => {}
                }
            }

            PlatformEvent::PointerPressed => {
                // A press on a scrollbar belongs to it rather than to the page
                // behind it.
                if self.pointer.1 >= UI_HEIGHT && self.tabs[self.active].system.is_none() {
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
                            let action = self.settings.pointer_pressed();
                            self.close_settings_if(&action);
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
                // interface has nothing in the page area to claim it.
                if let Some(url) = self.link_under_pointer() {
                    self.navigate_from(&url, false);
                    return;
                }
                // The press is tested against the geometry of the last frame —
                // which is the frame the user was looking at when they pressed.
                let action = self.ui.pointer_pressed(&mut self.text);
                self.apply(action);
            }

            PlatformEvent::PointerReleased => {
                if let Some(page) = self.tabs[self.active].page.as_mut() {
                    page.release_scrollbar();
                }
                self.settings.pointer_released();
            }

            PlatformEvent::KeyPressed { key, modifiers } => {
                if self.tabs[self.active].system == Some(SystemPage::Settings) {
                    let action = self.settings.settings.key_pressed(key, modifiers);
                    self.close_settings_if(&action);
                    if action != settings::Action::None {
                        return;
                    }
                }
                let action = self.ui.key_pressed(key, modifiers);
                if action == UiAction::None && !self.ui.address_focused {
                    self.scroll_by_key(key);
                }
                self.apply(action);
            }

            PlatformEvent::TextInput(character) => {
                if self.tabs[self.active].system == Some(SystemPage::Settings)
                    && self.settings.settings.text_input(character)
                {
                    return;
                }
                self.ui.text_input(character);
            }

            // Scrolling belongs to the page unless the pointer is over the
            // interface, where there is nothing to scroll.
            PlatformEvent::Scroll { y, .. } => {
                if self.ui.owns_pointer() {
                    return;
                }
                if self.tabs[self.active].system == Some(SystemPage::Settings) {
                    self.settings.scroll_by(-y);
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

            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            _ => {}
        }
    }

    fn accessibility(&mut self) -> Option<otlyra_platform::accesskit::TreeUpdate> {
        // Rebuilt whenever the frame it describes changed. The tree is a function
        // of the document and the last layout, so anything cheaper would be a
        // second copy of that state to keep honest.
        let tab = self.tabs.get(self.active)?;
        Some(match tab.page.as_ref() {
            Some(page) => crate::a11y::tree_for(page, &tab.title),
            None => crate::a11y::empty_tree(&tab.title),
        })
    }

    fn cursor(&self) -> Cursor {
        if self.link_under_pointer().is_some() {
            Cursor::Pointer
        } else {
            Cursor::Default
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        // Every frame takes in whatever has arrived. A wake is what *asks* for a
        // frame; this is what makes a frame that happened for any other reason —
        // a resize, an animation tick — show what has landed since the last one.
        self.pump();

        let width = viewport.logical_width();
        let height = viewport.logical_height();
        self.last_width = width;
        self.last_height = height;

        let scale = otlyra_gfx::kurbo::Affine::scale(viewport.scale_factor);

        // The page first, then the interface over it. The page is inset by the
        // interface's height and culled to what is visible, so it cannot paint
        // underneath it — but painting in this order means a future translucent
        // toolbar composites correctly rather than needing a clip.
        if let Some(system) = self.tabs[self.active].system {
            // A browser page takes the whole content area: it is not a document
            // in a tab, it is the browser looked at from the front.
            let content =
                crate::ui::Rect::new(0.0, UI_HEIGHT, width, (height - UI_HEIGHT).max(0.0));
            let mut list = otlyra_gfx::DisplayList::new();
            match system {
                SystemPage::Settings => {
                    self.settings
                        .build_display_list(content, &mut self.text, &mut list);
                }
                _ => self
                    .about
                    .build_display_list(content, &mut self.text, &mut list),
            }
            list.transform(scale);
            render(&list, target);
        } else if let Some(page) = self.tabs[self.active].page.as_mut() {
            let mut list = page.build_display_list(
                &mut self.text,
                width as f32,
                (height - UI_HEIGHT).max(0.0) as f32,
                UI_HEIGHT as f32,
            );
            list.transform(scale);
            render(&list, target);
        } else {
            let mut list = otlyra_gfx::DisplayList::new();
            crate::ui::paint_blank_page(
                &mut list,
                width,
                height,
                self.tabs[self.active].error.as_deref(),
                self.mark.as_ref(),
                &mut self.text,
            );
            list.transform(scale);
            render(&list, target);
        }

        let mut list = otlyra_gfx::DisplayList::new();
        let labels = self.labels();
        self.ui.build_display_list(
            width,
            height,
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
        list.transform(scale);
        render(&list, target);
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
        browser.on_event(PlatformEvent::PointerPressed);
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
    fn a_row_for_a_page_that_does_not_exist_yet_only_closes_the_menu() {
        let mut browser = Browser::new(NoNetwork);
        frame(&mut browser, 1000.0, 700.0);
        press(&mut browser, 1000.0 - 22.0, UI_HEIGHT - 21.0);
        frame(&mut browser, 1000.0, 700.0);

        // The second row is History, which is dimmed: the press falls through it
        // to the sheet behind the panel, which dismisses the menu and does
        // nothing else. That is the whole of what a disabled row does.
        press(&mut browser, 1000.0 - 120.0, UI_HEIGHT + 65.0);
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

#[cfg(test)]
mod tests {
    use otlyra_platform::{Key, Modifiers};

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
        browser.ui.address_focused = true;
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

    /// The address bar shows where the load ended up, not what was typed: a
    /// redirect that leaves the old text in place is a lie about what is on screen.
    #[test]
    fn the_address_bar_shows_the_final_url() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");
        assert_eq!(browser.ui.address.text(), "https://example.com/");
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

        browser.ui.pointer_moved(400.0, 400.0);
        browser.on_event(PlatformEvent::Scroll { x: 0.0, y: 50.0 });

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

        browser.ui.pointer_moved(400.0, 10.0);
        browser.on_event(PlatformEvent::Scroll { x: 0.0, y: 100.0 });
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

        browser.on_event(PlatformEvent::PointerPressed);
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

        // And over the interface, where the page's links cannot reach.
        browser.on_event(PlatformEvent::PointerMoved { x: 100.0, y: 10.0 });
        assert_eq!(browser.cursor(), Cursor::Default);
    }

    #[test]
    fn a_press_on_the_page_that_is_not_a_link_navigates_nowhere() {
        let mut browser = Browser::new(LinkLoader);
        browser.navigate("start.example");
        settle(&mut browser);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.on_event(PlatformEvent::PointerMoved { x: 700.0, y: 500.0 });
        browser.on_event(PlatformEvent::PointerPressed);
        assert_eq!(browser.tabs[0].url, "https://start.example/");
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

        browser.ui.pointer_moved(400.0, 400.0);
        browser.on_event(PlatformEvent::Scroll { x: 0.0, y: 200.0 });
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
                final_url: format!("https://{url}/"),
                ..Default::default()
            })
        }
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
