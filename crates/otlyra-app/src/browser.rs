//! The browser itself: tabs, navigation, and the loop's `Painter`.
//!
//! One window, several tabs, one of them active. Each tab owns its document and
//! its scroll position; the interface owns what is typed and what is focused; this
//! type owns the two of them and the one thing they share, the font engine.

use std::collections::HashMap;

use otlyra_css::cascade::ExternalSheets;
use otlyra_dom::Document;
use otlyra_gfx::{PaintTarget, render};
use otlyra_platform::{Cursor, Painter, PlatformEvent, Viewport};
use otlyra_text::TextEngine;

use crate::page::{PageScene, title_of};
use crate::ui::{BrowserUi, TabLabel, UI_HEIGHT, UiAction};

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
}

impl Tab {
    /// A blank tab.
    pub fn blank() -> Self {
        Self {
            url: String::new(),
            title: "New tab".to_owned(),
            page: None,
            error: None,
        }
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

/// How a tab gets its bytes.
///
/// A trait rather than a direct call to `otlyra-net` for one reason: the browser's
/// behaviour around navigation — which tab, what title, what happens on failure —
/// is worth testing without a socket.
pub trait Loader {
    /// Fetch `url`, returning the bytes and the transport's charset.
    fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String>;
}

/// The browser.
pub struct Browser<L: Loader> {
    text: TextEngine,
    ui: BrowserUi,
    tabs: Vec<Tab>,
    active: usize,
    loader: L,
    /// The width of the last frame, so a press can be tested against the geometry
    /// the user was actually looking at.
    last_width: f64,
    /// The mark shown on an empty tab. `None` if it failed to decode, which is a
    /// cosmetic problem and not a reason to refuse to draw a frame.
    mark: Option<otlyra_gfx::peniko::ImageData>,
    /// Where the pointer is, in window logical pixels.
    pointer: (f64, f64),
}

impl<L: Loader> Browser<L> {
    /// A browser with one blank tab.
    pub fn new(loader: L) -> Self {
        Self {
            text: TextEngine::new(),
            ui: BrowserUi::new(),
            tabs: vec![Tab::blank()],
            active: 0,
            loader,
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
        self.navigate_from(&url, false);
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.set_scroll(scroll);
        }
    }

    /// Load `url` into the active tab.
    ///
    /// `user_initiated` says whether the address came from the person rather than
    /// from the page: it is what decides whether a `file:` URL may be reached at
    /// all, and a page from the internet must never be able to claim it.
    fn navigate_from(&mut self, url: &str, user_initiated: bool) {
        let _span = tracing::info_span!("navigation", url).entered();

        if !user_initiated && let Ok(target) = otlyra_net::normalize(url) {
            let from = self.tabs[self.active].url.clone();
            if !otlyra_net::may_navigate(Some(&from), &target) {
                tracing::warn!(%url, %from, "navigation refused by scheme policy");
                let tab = &mut self.tabs[self.active];
                tab.error = Some(format!("Refused to open {url} from {from}"));
                tab.page = None;
                return;
            }
        }

        let tab = &mut self.tabs[self.active];
        tab.url = url.to_owned();
        tab.error = None;
        tab.title = url.to_owned();
        self.ui.address.set_text(url);

        match self.loader.load(url) {
            Ok((bytes, charset, final_url)) => {
                let parsed = otlyra_html::parse(&bytes, charset.as_deref());
                let sheets = self.fetch_stylesheets(&parsed.document, &final_url);
                let tab = &mut self.tabs[self.active];
                tab.title = title_of(&parsed.document).unwrap_or_else(|| final_url.clone());
                tab.url = final_url.clone();
                tab.page = Some(PageScene::with_stylesheets(parsed.document, sheets));
                self.ui.address.set_text(final_url);
            }
            Err(error) => {
                tracing::warn!(%error, "navigation failed");
                let tab = &mut self.tabs[self.active];
                tab.title = "Failed".to_owned();
                tab.page = None;
                tab.error = Some(error);
            }
        }
    }

    /// Fetch every stylesheet `document` links to, resolved against `base`.
    ///
    /// Synchronous, like the navigation it is part of, and for the same reason:
    /// the page cannot be styled before its sheets have arrived, and the real fix
    /// is a load that does not block the event loop rather than a style step that
    /// waits for one.
    ///
    /// A document fetched over the network may not reach a `file:` URL, the same
    /// rule that governs where it may navigate: a stylesheet is a request the page
    /// chose to make, and a page from the internet reading the disk is the failure
    /// that rule exists to prevent.
    fn fetch_stylesheets(&mut self, document: &Document, base: &str) -> ExternalSheets {
        let links = otlyra_css::cascade::stylesheet_links(document);
        let mut sheets = ExternalSheets::default();
        let mut fetched: HashMap<String, Option<String>> = HashMap::new();

        for link in links.iter().take(STYLESHEET_LIMIT) {
            let Some(url) = otlyra_net::resolve(base, &link.href) else {
                continue;
            };
            if let Ok(target) = otlyra_net::normalize(&url)
                && !otlyra_net::may_navigate(Some(base), &target)
            {
                tracing::warn!(%url, %base, "stylesheet refused by scheme policy");
                continue;
            }

            // One fetch per address: a page that links the same sheet from two
            // places is asking for it once.
            let source =
                fetched
                    .entry(url.clone())
                    .or_insert_with(|| match self.loader.load(&url) {
                        Ok((bytes, charset, _)) => Some(decode_css(&bytes, charset.as_deref())),
                        Err(error) => {
                            tracing::warn!(%url, %error, "stylesheet failed to load");
                            None
                        }
                    });

            if let Some(source) = source {
                sheets.insert(link.node, source.clone());
            }
        }

        if links.len() > STYLESHEET_LIMIT {
            tracing::warn!(
                asked = links.len(),
                fetched = STYLESHEET_LIMIT,
                "the document links more stylesheets than the limit"
            );
        }

        sheets
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
                loading: false,
            })
            .collect()
    }
}

impl<L: Loader> Painter for Browser<L> {
    fn on_event(&mut self, event: PlatformEvent) {
        match event {
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
        self.ui
            .build_display_list(width, &labels, self.active, &mut self.text, &mut list);
        list.transform(scale);
        render(&list, target);
    }
}

#[cfg(test)]
mod tests {
    use otlyra_platform::{Key, Modifiers};

    use super::*;

    /// A loader that serves canned pages, so navigation can be tested without a
    /// socket — including the failure path, which a real server makes awkward.
    #[derive(Default)]
    struct FakeLoader {
        requested: Vec<String>,
    }

    impl Loader for FakeLoader {
        fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
            self.requested.push(url.to_owned());
            match url {
                "broken.example" => Err("could not fetch broken.example".to_owned()),
                // A `file:` URL loads as itself; anything else becomes an https
                // address, the way a bare hostname does.
                _ if url.starts_with("file://") => Ok((
                    format!("<title>Local</title><body><p>Body of {url}").into_bytes(),
                    Some("utf-8".to_owned()),
                    url.to_owned(),
                )),
                _ => Ok((
                    format!("<title>Title of {url}</title><body><p>Body of {url}").into_bytes(),
                    Some("utf-8".to_owned()),
                    format!("https://{url}/"),
                )),
            }
        }
    }

    fn browser() -> Browser<FakeLoader> {
        Browser::new(FakeLoader::default())
    }

    fn type_url(browser: &mut Browser<FakeLoader>, url: &str) {
        browser.ui.address_focused = true;
        for character in url.chars() {
            browser.on_event(PlatformEvent::TextInput(character));
        }
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Enter,
            modifiers: Modifiers::default(),
        });
    }

    #[test]
    fn typing_an_address_and_pressing_enter_loads_it() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");

        assert_eq!(browser.loader.requested, ["example.com"]);
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

    /// A site whose CSS lives in a file next to the page.
    #[derive(Default)]
    struct SiteLoader {
        requested: Vec<String>,
    }

    impl Loader for SiteLoader {
        fn load(&mut self, url: &str) -> Result<(Vec<u8>, Option<String>, String), String> {
            self.requested.push(url.to_owned());
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
        let mut browser = Browser::new(SiteLoader::default());
        browser.navigate("site.example");

        assert_eq!(
            browser.loader.requested,
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
        assert!(browser.tabs[browser.active].page.is_some());
    }

    /// Where the link's text was actually painted, taken from the page's own
    /// targets rather than guessed.
    fn link_position(browser: &Browser<LinkLoader>) -> (f64, f64) {
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
        let mut browser = browser();
        type_url(&mut browser, "example.com");
        browser.reload();

        assert_eq!(
            browser.loader.requested,
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
        let mut painter = otlyra_gfx::RecordingPainter::new();
        browser.paint(&mut painter, Viewport::new(800, 600, 1.0));

        browser.ui.pointer_moved(400.0, 400.0);
        browser.on_event(PlatformEvent::Scroll { x: 0.0, y: 200.0 });
        let scrolled = browser.tabs[0].page.as_ref().expect("page").scroll();
        assert!(scrolled > 0.0);

        browser.reload();
        assert_eq!(
            browser.tabs[0].page.as_ref().expect("page").scroll(),
            scrolled
        );
    }

    #[test]
    fn reloading_a_blank_tab_does_nothing() {
        let mut browser = browser();
        browser.reload();
        assert!(browser.loader.requested.is_empty());
    }

    /// §14's rule: a page from the internet must never be able to open a file.
    #[test]
    fn a_web_page_may_not_navigate_to_a_file_url() {
        let mut browser = browser();
        type_url(&mut browser, "example.com");
        browser.navigate_from("file:///etc/passwd", false);

        assert_eq!(browser.tabs[0].url, "https://example.com/");
        assert!(
            browser.tabs[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Refused"))
        );
        assert_eq!(
            browser.loader.requested,
            ["example.com"],
            "the loader is never even asked"
        );
    }

    #[test]
    fn the_user_may_open_a_file_url_and_so_may_a_local_page() {
        let mut browser = browser();
        type_url(&mut browser, "file:///tmp/one.html");
        assert_eq!(browser.loader.requested.len(), 1);

        browser.navigate_from("file:///tmp/two.html", false);
        assert_eq!(
            browser.loader.requested.len(),
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
