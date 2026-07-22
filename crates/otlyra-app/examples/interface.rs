//! Draw the browser's interface on its own, with no document behind it.
//!
//! The states that matter are the ones a single screenshot of a running browser
//! cannot show at once: several tabs, one of them loading, a focused address
//! field, a pointer resting on a control. Each is one frame here, written to a
//! PNG, so a change to the interface can be looked at rather than reasoned
//! about — and so the frames M11 asks to keep as goldens have something that
//! produces them.
//!
//! ```text
//! cargo run -p otlyra-app --example interface -- /tmp/interface
//! ```

use std::path::PathBuf;

use otlyra_app::clipboard::InMemory;
use otlyra_app::ui::{BrowserUi, TabLabel, UI_HEIGHT};
use otlyra_gfx::DisplayList;
use otlyra_gfx::PaintTarget;
use otlyra_gfx::kurbo::Affine;
use otlyra_platform::{Key, Modifiers, Painter, PlatformEvent, Viewport, render_offscreen};
use otlyra_text::TextEngine;

/// One frame of the interface, in a state worth looking at.
struct Frame {
    name: &'static str,
    ui: BrowserUi,
    tabs: Vec<TabLabel>,
    active: usize,
    history: (bool, bool),
    spinner: Option<f32>,
    /// Put the caret in the address field before the frame that is written.
    ///
    /// Which needs a frame drawn first: a focus id is a control's place in the
    /// order a frame built, so until one has been built there is no field to
    /// focus. Every state here is therefore drawn twice and the second one kept.
    focus_address: bool,
    /// Keep the whole address selected, the way ⌘L leaves it. Without this a
    /// focused frame collapses the selection so the caret itself is visible.
    select_address: bool,
    /// How many times Tab is pressed before the frame that is written.
    tabs_pressed: usize,
    text: TextEngine,
    clipboard: InMemory,
}

impl Frame {
    /// The state as it should be looked at, given a frame has already been drawn.
    ///
    /// Written to be worth running twice: every scale settles from the same
    /// start, or the second one would be a different number of Tabs along than
    /// the first and the two shots would not be of one state.
    fn settle(&mut self) {
        self.ui.key_pressed(
            Key::Escape,
            Modifiers::default(),
            &mut self.text,
            &mut self.clipboard,
        );
        if self.focus_address {
            self.ui.focus_address();
            if !self.select_address {
                self.ui.address.move_end(false);
            }
        }
        for _ in 0..self.tabs_pressed {
            self.ui.key_pressed(
                Key::Tab,
                Modifiers::default(),
                &mut self.text,
                &mut self.clipboard,
            );
        }
    }
}

impl Painter for Frame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        // The blank page goes down first, so the hairline at the bottom of the
        // toolbar has something to be a line between.
        let mut list = DisplayList::new();
        otlyra_app::ui::paint_blank_page(
            &mut list,
            &self.ui.theme,
            viewport.logical_width(),
            viewport.logical_height(),
            None,
            None,
            &mut self.text,
        );
        self.ui.build_display_list(
            viewport.logical_width(),
            viewport.logical_height(),
            &self.tabs,
            self.active,
            self.history,
            self.spinner,
            &mut self.text,
            &mut list,
        );
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

/// The history, with a couple of days behind it, as the browser shows it.
struct HistoryFrame {
    ui: BrowserUi,
    surface: otlyra_app::history::HistorySurface,
    store: otlyra_app::history::HistoryStore,
    today: jiff::civil::Date,
    text: TextEngine,
}

impl HistoryFrame {
    fn new() -> Self {
        let mut ui = BrowserUi::new();
        ui.address.set_text("about:history");
        let mut store = otlyra_app::history::HistoryStore::default();
        let today: jiff::civil::Date = "2026-07-22".parse().expect("a date");
        let at = |text: &str| text.parse::<jiff::Timestamp>().expect("a timestamp");
        store.record(
            "https://octofhir.github.io/Otlyra/",
            "Otlyra — a browser engine",
            at("2026-07-20T10:15:00Z"),
        );
        store.record(
            "https://example.com/some/very/long/path/that/needs/cutting",
            "A page with a long address",
            at("2026-07-21T18:40:00Z"),
        );
        store.record(
            "https://example.com/search?q=widgets",
            "Search results",
            at("2026-07-22T09:05:00Z"),
        );
        Self {
            ui,
            surface: otlyra_app::history::HistorySurface::new(),
            store,
            today,
            text: TextEngine::new(),
        }
    }
}

impl Painter for HistoryFrame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let (width, height) = (viewport.logical_width(), viewport.logical_height());
        let mut list = DisplayList::new();
        self.surface.build_display_list(
            otlyra_app::ui::Rect::new(0.0, UI_HEIGHT, width, (height - UI_HEIGHT).max(0.0)),
            &self.store,
            self.today,
            &mut self.text,
            &mut list,
        );
        self.ui.build_display_list(
            width,
            height,
            &tabs(&[("History", false)]),
            0,
            (true, false),
            None,
            &mut self.text,
            &mut list,
        );
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

/// The settings surface, with the toolbar above it, as the browser shows it.
struct SettingsFrame {
    ui: BrowserUi,
    surface: otlyra_app::settings::SettingsSurface,
    /// How far to scroll before the frame that is written.
    scroll: f64,
    /// How many times Tab is pressed before it, for the frame that shows a
    /// control holding the keyboard.
    tabs_pressed: usize,
    text: TextEngine,
}

impl SettingsFrame {
    fn new() -> Self {
        Self {
            ui: BrowserUi::new(),
            surface: otlyra_app::settings::SettingsSurface::new(),
            scroll: 0.0,
            tabs_pressed: 0,
            text: TextEngine::new(),
        }
    }

    fn settle(&mut self) {
        self.surface.settings.scroll = 0.0;
        self.surface.settings.focus = None;
        self.surface.scroll_by(self.scroll);
        for _ in 0..self.tabs_pressed {
            self.surface
                .key_pressed(Key::Tab, Modifiers::default(), &mut InMemory::default());
        }
    }
}

impl Painter for SettingsFrame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let (width, height) = (viewport.logical_width(), viewport.logical_height());
        let mut list = DisplayList::new();
        self.surface.build_display_list(
            otlyra_app::ui::Rect::new(0.0, UI_HEIGHT, width, (height - UI_HEIGHT).max(0.0)),
            &mut self.text,
            &mut list,
        );
        self.ui.build_display_list(
            width,
            height,
            &tabs(&[("Settings", false)]),
            0,
            (true, false),
            None,
            &mut self.text,
            &mut list,
        );
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

/// The inspector docked under a page, with a node chosen.
///
/// The document is parsed here rather than fetched: what this state is for is
/// looking at the panel, and a panel that needed a network to be drawn could not
/// be looked at on a machine without one.
struct InspectorFrame {
    ui: BrowserUi,
    inspector: otlyra_app::inspector::Inspector,
    document: otlyra_dom::Document,
    /// A laid-out copy, for the accessibility pane, which reads boxes.
    page: Option<otlyra_app::page::PageScene>,
    /// A canned network list, for the network pane.
    exchanges: Vec<otlyra_app::fetcher::Exchange>,
    /// Which pane the written frame shows.
    pane: otlyra_app::inspector::Pane,
    /// How many times Down is pressed before the frame that is written.
    steps: usize,
    text: TextEngine,
}

/// The document every inspector state here inspects.
const INSPECTOR_HTML: &[u8] = b"<html><head><title>A page</title></head><body>\
      <header class=\"top\"><h1 id=\"title\">A heading</h1></header>\
      <main><p class=\"lead\">A paragraph of text.</p>\
      <ul><li>one</li><li>two</li></ul></main></body></html>";

impl InspectorFrame {
    fn new(steps: usize) -> Self {
        let document = otlyra_html::parse(INSPECTOR_HTML, Some("utf-8")).document;
        let mut inspector = otlyra_app::inspector::Inspector::new();
        inspector.open = true;
        Self {
            ui: BrowserUi::new(),
            inspector,
            document,
            page: None,
            exchanges: Vec::new(),
            pane: otlyra_app::inspector::Pane::Elements,
            steps,
            text: TextEngine::new(),
        }
    }

    /// A frame showing `pane`, with a laid-out page and a canned network list
    /// behind it so the panes that read those have something to draw.
    fn on_pane(pane: otlyra_app::inspector::Pane) -> Self {
        let mut frame = Self::new(1);
        frame.pane = pane;
        let parsed = otlyra_html::parse(INSPECTOR_HTML, Some("utf-8"));
        let mut page = otlyra_app::page::PageScene::new(parsed.document);
        let mut text = TextEngine::isolated();
        let _ = page.build_display_list(&mut text, 1000.0, 500.0, 0.0);
        frame.page = Some(page);
        frame.exchanges = canned_exchanges();
        frame
    }

    fn settle(&mut self) {
        self.inspector
            .apply(otlyra_app::inspector::Action::Show(self.pane));
        self.inspector.selected = None;
        for _ in 0..self.steps {
            self.inspector.key_pressed(
                Key::Down,
                Modifiers::default(),
                Some(&self.document),
                &mut InMemory::default(),
            );
            self.inspector.key_pressed(
                Key::Right,
                Modifiers::default(),
                Some(&self.document),
                &mut InMemory::default(),
            );
        }
        // On the network pane, choose a request so the detail side is drawn too.
        if self.pane == otlyra_app::inspector::Pane::Network
            && let Some(first) = self.exchanges.first()
        {
            self.inspector
                .apply(otlyra_app::inspector::Action::SelectExchange(first.id));
        }
    }
}

/// A network list built by really running a fetch, so the panel is drawn over
/// the same data the browser produces rather than a hand-made imitation.
fn canned_exchanges() -> Vec<otlyra_app::fetcher::Exchange> {
    use otlyra_app::fetcher::{Fetcher, Loaded, Loader, ResourceKind};

    struct Canned;
    impl Loader for Canned {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            let (content_type, bytes, status) = if url.ends_with(".css") {
                (
                    "text/css",
                    b"body { color: #222; }\nh1 { font-size: 2rem; }".to_vec(),
                    200,
                )
            } else if url.ends_with(".png") {
                // A missing picture: a 404 with a short HTML body, which is the
                // case the old flat list called "ok".
                ("text/html", b"<h1>Not found</h1>".to_vec(), 404)
            } else {
                ("text/html", INSPECTOR_HTML.to_vec(), 200)
            };
            Ok(Loaded {
                bytes,
                content_type: Some(content_type.to_owned()),
                status: Some(status),
                request_headers: vec![
                    ("user-agent".to_owned(), "Otlyra".to_owned()),
                    ("accept".to_owned(), "*/*".to_owned()),
                ],
                response_headers: vec![
                    ("content-type".to_owned(), content_type.to_owned()),
                    ("content-length".to_owned(), "42".to_owned()),
                ],
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    let mut fetcher = Fetcher::spawn(Canned);
    for (url, kind) in [
        ("https://otlyra.example/", ResourceKind::Document),
        ("https://otlyra.example/style.css", ResourceKind::Stylesheet),
        ("https://otlyra.example/logo.png", ResourceKind::Image),
    ] {
        fetcher.request(url, kind);
    }
    let mut settled = 0;
    while settled < 3 {
        let batch = fetcher.wait(std::time::Duration::from_secs(5));
        if batch.is_empty() {
            break;
        }
        settled += batch.len();
    }
    fetcher.exchanges().to_vec()
}

impl Painter for InspectorFrame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let (width, height) = (viewport.logical_width(), viewport.logical_height());
        let content = (height - UI_HEIGHT).max(0.0);
        let dock = self.inspector.dock_height(content);

        let mut list = DisplayList::new();
        otlyra_app::ui::paint_blank_page(
            &mut list,
            &self.ui.theme,
            width,
            height,
            None,
            None,
            &mut self.text,
        );
        let facts = otlyra_app::inspector::Facts {
            document: Some(&self.document),
            page: self.page.as_ref(),
            style: None,
            rect: None,
            containing: None,
            exchanges: &self.exchanges,
        };
        self.inspector.build_display_list(
            otlyra_app::ui::Rect::new(0.0, UI_HEIGHT + content - dock, width, dock),
            &facts,
            &mut self.text,
            &mut list,
        );
        self.ui.build_display_list(
            width,
            height,
            &tabs(&[("A page", false)]),
            0,
            (true, false),
            None,
            &mut self.text,
            &mut list,
        );
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

fn tabs(titles: &[(&str, bool)]) -> Vec<TabLabel> {
    titles
        .iter()
        .map(|(title, loading)| TabLabel {
            title: (*title).to_owned(),
            loading: *loading,
        })
        .collect()
}

/// A viewport of `width` by `height` logical pixels at `scale`.
///
/// Device pixels, so a 2× shot of a 1100pt-wide window is 2200 across.
fn viewport(width: f64, height: f64, scale: f64) -> Viewport {
    Viewport {
        width: (width * scale) as u32,
        height: (height * scale) as u32,
        scale_factor: scale,
    }
}

/// Both scales, always. The one interface bug that got furthest was invisible at
/// 1× and doubled everything inside a scrolling panel at 2×, so a state written
/// at only one of them is a state half looked at.
const SCALES: [f64; 2] = [1.0, 2.0];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/interface".to_owned())
        .into();
    std::fs::create_dir_all(&directory)?;

    let toolbar = |name, tabs, active, history, spinner| Frame {
        name,
        ui: BrowserUi::new(),
        tabs,
        active,
        history,
        spinner,
        focus_address: false,
        select_address: false,
        tabs_pressed: 0,
        text: TextEngine::new(),
        clipboard: InMemory::default(),
    };

    let mut frames = Vec::new();

    // One tab, nowhere to go back to: what the browser looks like on the first
    // frame after it opens.
    let mut fresh = toolbar(
        "empty",
        tabs(&[("New tab", false)]),
        0,
        (false, false),
        None,
    );
    fresh.ui.address.clear();
    frames.push(fresh);

    // Several tabs, one of them still loading, and history in both directions.
    let mut busy = toolbar(
        "tabs",
        tabs(&[
            ("CSS support — Otlyra", false),
            ("A title long enough that it has to be cut short", true),
            ("Otlyra", false),
        ]),
        0,
        (true, true),
        Some(1.2),
    );
    busy.ui.address.set_text("https://example.com/some/path");
    frames.push(busy);

    // The address field with the caret in it, which is the state every other
    // control has to keep out of the way of.
    let mut focused = toolbar(
        "focused",
        tabs(&[("Otlyra", false), ("Second", false)]),
        1,
        (true, false),
        None,
    );
    focused
        .ui
        .address
        .set_text("https://example.com/search?q=widgets");
    focused.focus_address = true;
    frames.push(focused);

    // The same field with everything selected, the way ⌘L leaves it: the wash
    // has to read as selection against the field's focused white.
    let mut selected = toolbar(
        "selected",
        tabs(&[("Otlyra", false), ("Second", false)]),
        1,
        (true, false),
        None,
    );
    selected
        .ui
        .address
        .set_text("https://example.com/search?q=widgets");
    selected.focus_address = true;
    selected.select_address = true;
    frames.push(selected);

    // The keyboard on the first tab, which is where the focus ring has to be
    // legible against the strip it is drawn on.
    let mut traversed = toolbar(
        "keyboard",
        tabs(&[("Otlyra", false), ("Second", false)]),
        0,
        (true, true),
        None,
    );
    traversed.ui.address.set_text("https://example.com/");
    traversed.tabs_pressed = 1;
    frames.push(traversed);

    // The menu behind the cogwheel, open, which is the one state that reaches
    // past the interface and over the page.
    let mut menu = toolbar("menu", tabs(&[("Otlyra", false)]), 0, (true, false), None);
    menu.ui.address.set_text("https://example.com/");
    menu.ui.menu_open = true;
    frames.push(menu);

    // The pointer resting on the reload button, so the hover wash is visible.
    let mut hovered = toolbar(
        "hover",
        tabs(&[("Otlyra", false), ("Second", false)]),
        0,
        (true, true),
        None,
    );
    hovered.ui.address.set_text("https://example.com/");
    let mut engine = TextEngine::new();
    hovered
        .ui
        .pointer_moved(80.0, UI_HEIGHT - 20.0, &mut engine);
    frames.push(hovered);

    // The settings: the whole page, then the states a tall window cannot show —
    // a short one, one scrolled to the end, a narrow one, and one with the
    // keyboard several controls in.
    for (name, width, height, scroll, tabs_pressed) in [
        ("settings", 1100.0, 760.0, 0.0, 0),
        ("settings-short", 900.0, 420.0, 0.0, 0),
        ("settings-scrolled", 900.0, 420.0, 100_000.0, 0),
        ("settings-narrow", 620.0, 700.0, 0.0, 0),
        ("settings-keyboard", 900.0, 700.0, 0.0, 5),
    ] {
        let mut frame = SettingsFrame::new();
        frame.ui.address.set_text("about:settings");
        frame.scroll = scroll;
        frame.tabs_pressed = tabs_pressed;
        write_states(&directory, name, &mut frame, width, height, |frame| {
            frame.settle();
        })?;
    }

    // The history: a searchable list with two days behind it.
    write_states(
        &directory,
        "history",
        &mut HistoryFrame::new(),
        900.0,
        700.0,
        |_| {},
    )?;

    // The inspector, closed on the document and opened several levels into it.
    for (name, steps) in [("inspector", 1), ("inspector-deep", 4)] {
        let mut frame = InspectorFrame::new(steps);
        write_states(&directory, name, &mut frame, 1000.0, 700.0, |frame| {
            frame.settle();
        })?;
    }

    // The panes that read a page and a network list: the accessibility tree and
    // the network waterfall, each with its detail side open.
    for (name, pane) in [
        ("inspector-a11y", otlyra_app::inspector::Pane::Accessibility),
        ("inspector-network", otlyra_app::inspector::Pane::Network),
    ] {
        let mut frame = InspectorFrame::on_pane(pane);
        write_states(&directory, name, &mut frame, 1000.0, 700.0, |frame| {
            frame.settle();
        })?;
    }

    for mut frame in frames {
        let name = frame.name;
        write_states(
            &directory,
            name,
            &mut frame,
            1100.0,
            UI_HEIGHT + 240.0,
            |frame| frame.settle(),
        )?;
    }

    Ok(())
}

/// Write one state at every scale.
///
/// Two frames per scale, and the second is the one kept: focus ids and how far a
/// panel can scroll are both things a frame only reports once it has been built,
/// so a state that depends on either has to be drawn before it can be set up.
fn write_states<P: Painter>(
    directory: &std::path::Path,
    name: &str,
    frame: &mut P,
    width: f64,
    height: f64,
    settle: impl Fn(&mut P),
) -> Result<(), Box<dyn std::error::Error>> {
    for scale in SCALES {
        let viewport = viewport(width, height, scale);
        let _ = render_offscreen(frame, viewport)?;
        settle(frame);
        let png = render_offscreen(frame, viewport)?;
        let path = directory.join(format!("{name}@{scale}x.png"));
        std::fs::write(&path, &png)?;
        println!("{}", path.display());
    }
    Ok(())
}
