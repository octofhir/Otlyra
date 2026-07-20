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
use otlyra_platform::{Cursor, Painter, PlatformEvent, Viewport, Waker};
use otlyra_text::TextEngine;

use crate::fetcher::{Fetched, Fetcher, Loader, ResourceKind};
use crate::page::{PageScene, title_of};
use crate::ui::{BrowserUi, TabLabel, UI_HEIGHT, UiAction};

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

/// Decode a fetched stylesheet.
///
/// A BOM or a charset from the transport decides; anything else is UTF-8, which
/// is CSS's own default and not HTML's — an unlabelled *document* is assumed to be
/// windows-1252, an unlabelled *stylesheet* is not.
fn decode_css(bytes: &[u8], charset: Option<&str>) -> String {
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
    /// The width of the last frame, so a press can be tested against the geometry
    /// the user was actually looking at.
    last_width: f64,
    /// The mark shown on an empty tab. `None` if it failed to decode, which is a
    /// cosmetic problem and not a reason to refuse to draw a frame.
    mark: Option<otlyra_gfx::peniko::ImageData>,
    /// Where the pointer is, in window logical pixels.
    pointer: (f64, f64),
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
            last_width: 1024.0,
            mark: otlyra_gfx::decode_image(crate::MARK)
                .inspect_err(|error| tracing::error!(%error, "the mark failed to decode"))
                .ok(),
            pointer: (0.0, 0.0),
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

        let parsed = otlyra_html::parse(&loaded.bytes, loaded.charset.as_deref());
        let final_url = loaded.final_url;

        // What the page asks for, decided here and fetched on the other thread.
        let mut outstanding: HashMap<u64, Vec<PendingResource>> = HashMap::new();
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
        self.request_subresources(
            &mut outstanding,
            &final_url,
            pictures.iter().take(IMAGE_LIMIT).map(|source| {
                (
                    source.src.clone(),
                    PendingResource::Image(source.node),
                    ResourceKind::Image,
                )
            }),
        );
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
                for resource in wanted {
                    match resource {
                        PendingResource::Stylesheet(node) => {
                            let source = decode_css(&loaded.bytes, loaded.charset.as_deref());
                            pending.sheets.insert(node, source);
                        }
                        PendingResource::Image(node) => {
                            match otlyra_gfx::decode_image(&loaded.bytes) {
                                Ok(image) => {
                                    pending.images.insert(node, image);
                                }
                                Err(error) => {
                                    tracing::warn!(url = %fetched.url, %error, "image failed to decode");
                                }
                            }
                        }
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

    fn apply(&mut self, action: UiAction) {
        match action {
            UiAction::None => {}
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
            }

            PlatformEvent::PointerPressed => {
                // A link takes the press before the interface sees it, because the
                // interface has nothing in the page area to claim it.
                if let Some(url) = self.link_under_pointer() {
                    self.navigate_from(&url, false);
                    return;
                }
                // Width is not carried on the event, so the interface is asked
                // against the geometry of the last frame — which is the frame the
                // user was looking at when they pressed.
                let action = self.ui.pointer_pressed(self.last_width, self.tabs.len());
                self.apply(action);
            }

            PlatformEvent::KeyPressed { key, modifiers } => {
                let action = self.ui.key_pressed(key, modifiers);
                self.apply(action);
            }

            PlatformEvent::TextInput(character) => {
                self.ui.text_input(character);
            }

            // Scrolling belongs to the page unless the pointer is over the
            // interface, where there is nothing to scroll.
            PlatformEvent::Scroll { y, .. } => {
                if !self.ui.owns_pointer()
                    && let Some(page) = self.tabs[self.active].page.as_mut()
                {
                    page.scroll_by(y as f32);
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
        let width = viewport.logical_width();
        let height = viewport.logical_height();
        self.last_width = width;

        let scale = otlyra_gfx::kurbo::Affine::scale(viewport.scale_factor);

        // The page first, then the interface over it. The page is inset by the
        // interface's height and culled to what is visible, so it cannot paint
        // underneath it — but painting in this order means a future translucent
        // toolbar composites correctly rather than needing a clip.
        if let Some(page) = self.tabs[self.active].page.as_mut() {
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
mod tests {
    use otlyra_platform::{Key, Modifiers};

    use super::*;

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
        fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            match url {
                "broken.example" => Err("could not fetch broken.example".to_owned()),
                // A `file:` URL loads as itself; anything else becomes an https
                // address, the way a bare hostname does.
                _ if url.starts_with("file://") => Ok((
                    format!("<title>Local</title><body><p>Body of {url}").into_bytes(),
                    Some("utf-8".to_owned()),
                    url.to_owned(),
                )),
                // A bare hostname becomes an https address, the way the real
                // loader normalizes one; an address that already is one is left
                // alone, or going back to it would grow a second scheme each time.
                _ => {
                    let final_url = if url.contains("://") {
                        url.to_owned()
                    } else {
                        format!("https://{url}/")
                    };
                    Ok((
                        format!("<title>Title of {url}</title><body><p>Body of {url}").into_bytes(),
                        Some("utf-8".to_owned()),
                        final_url,
                    ))
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
        fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
            // Anything that is not already an address on this host is treated as
            // its root, which is what typing a bare hostname means.
            let path = match url.strip_prefix("https://start.example") {
                Some("") | None => "/",
                Some(path) => path,
            };
            Ok((
                b"<title>Linked</title><body><p><a href=\"/next\">go on</a></p>".to_vec(),
                Some("utf-8".to_owned()),
                format!("https://start.example{path}"),
            ))
        }
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
        fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
            self.requested
                .lock()
                .expect("no panic on the fetch thread")
                .push(url.to_owned());
            match url {
                "https://site.example/site.css" => Ok((
                    b"p { color: rgb(0, 128, 0) }".to_vec(),
                    Some("utf-8".to_owned()),
                    url.to_owned(),
                )),
                "https://site.example/missing.css" => Err("404".to_owned()),
                _ => Ok((
                    b"<link rel=stylesheet href=site.css>\
                      <link rel=stylesheet href=missing.css>\
                      <link rel=icon href=favicon.ico>\
                      <body><p>text"
                        .to_vec(),
                    Some("utf-8".to_owned()),
                    "https://site.example/".to_owned(),
                )),
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

        assert_eq!(
            asked_for(&requested),
            vec![
                "site.example".to_owned(),
                "https://site.example/site.css".to_owned(),
                "https://site.example/missing.css".to_owned(),
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
            fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
                assert!(
                    !url.starts_with("file:"),
                    "the loader must never be asked for {url}"
                );
                Ok((
                    b"<link rel=stylesheet href=\"file:///etc/theme.css\"><body><p>x".to_vec(),
                    Some("utf-8".to_owned()),
                    "https://site.example/".to_owned(),
                ))
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
        fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
            let body = "<title>Long</title><body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
            Ok((
                body.into_bytes(),
                Some("utf-8".to_owned()),
                format!("https://{url}/"),
            ))
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
