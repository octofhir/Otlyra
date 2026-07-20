//! The browser itself: tabs, navigation, and the loop's `Painter`.
//!
//! One window, several tabs, one of them active. Each tab owns its document and
//! its scroll position; the interface owns what is typed and what is focused; this
//! type owns the two of them and the one thing they share, the font engine.

use otlyra_gfx::{PaintTarget, render};
use otlyra_platform::{Painter, PlatformEvent, Viewport};
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
        let _span = tracing::info_span!("navigation", url).entered();
        let tab = &mut self.tabs[self.active];
        tab.url = url.to_owned();
        tab.error = None;
        tab.title = url.to_owned();
        self.ui.address.set_text(url);

        match self.loader.load(url) {
            Ok((bytes, charset, final_url)) => {
                let parsed = otlyra_html::parse(&bytes, charset.as_deref());
                let tab = &mut self.tabs[self.active];
                tab.title = title_of(&parsed.document).unwrap_or_else(|| final_url.clone());
                tab.url = final_url.clone();
                tab.page = Some(PageScene::new(&parsed.document));
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
        }
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
            PlatformEvent::PointerMoved { x, y } => self.ui.pointer_moved(x, y),

            PlatformEvent::PointerPressed => {
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

            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            _ => {}
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
