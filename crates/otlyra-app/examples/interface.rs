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

use otlyra_app::ui::{BrowserUi, TabLabel, UI_HEIGHT};
use otlyra_gfx::DisplayList;
use otlyra_gfx::PaintTarget;
use otlyra_gfx::kurbo::Affine;
use otlyra_platform::{Painter, PlatformEvent, Viewport, render_offscreen};
use otlyra_text::TextEngine;

/// One frame of the interface, in a state worth looking at.
struct Frame {
    name: &'static str,
    ui: BrowserUi,
    tabs: Vec<TabLabel>,
    active: usize,
    history: (bool, bool),
    spinner: Option<f32>,
    text: TextEngine,
}

impl Painter for Frame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        // The blank page goes down first, so the hairline at the bottom of the
        // toolbar has something to be a line between.
        let mut list = DisplayList::new();
        otlyra_app::ui::paint_blank_page(
            &mut list,
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

/// The settings surface, with the toolbar above it, as the browser shows it.
struct SettingsFrame {
    ui: BrowserUi,
    surface: otlyra_app::settings::SettingsSurface,
    text: TextEngine,
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

fn tabs(titles: &[(&str, bool)]) -> Vec<TabLabel> {
    titles
        .iter()
        .map(|(title, loading)| TabLabel {
            title: (*title).to_owned(),
            loading: *loading,
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/interface".to_owned())
        .into();
    std::fs::create_dir_all(&directory)?;

    // Device pixels, so a 2× shot of a 1100pt-wide window is 2200 across.
    let scale = 2.0;
    let viewport = Viewport {
        width: (1100.0 * scale) as u32,
        height: ((UI_HEIGHT + 240.0) * scale) as u32,
        scale_factor: scale,
    };

    let mut frames = Vec::new();

    // One tab, nowhere to go back to: what the browser looks like on the first
    // frame after it opens.
    let mut fresh = Frame {
        name: "empty",
        ui: BrowserUi::new(),
        tabs: tabs(&[("New tab", false)]),
        active: 0,
        history: (false, false),
        spinner: None,
        text: TextEngine::new(),
    };
    fresh.ui.address.clear();
    frames.push(fresh);

    // Several tabs, one of them still loading, and history in both directions.
    let mut busy = Frame {
        name: "tabs",
        ui: BrowserUi::new(),
        tabs: tabs(&[
            ("CSS support — Otlyra", false),
            ("A title long enough that it has to be cut short", true),
            ("Otlyra", false),
        ]),
        active: 0,
        history: (true, true),
        spinner: Some(1.2),
        text: TextEngine::new(),
    };
    busy.ui.address.set_text("https://example.com/some/path");
    frames.push(busy);

    // The address field with the caret in it, which is the state every other
    // control has to keep out of the way of.
    let mut focused = Frame {
        name: "focused",
        ui: BrowserUi::new(),
        tabs: tabs(&[("Otlyra", false), ("Second", false)]),
        active: 1,
        history: (true, false),
        spinner: None,
        text: TextEngine::new(),
    };
    focused
        .ui
        .address
        .set_text("https://example.com/search?q=widgets");
    focused.ui.address_focused = true;
    frames.push(focused);

    // The menu behind the cogwheel, open, which is the one state that reaches
    // past the interface and over the page.
    let mut menu = Frame {
        name: "menu",
        ui: BrowserUi::new(),
        tabs: tabs(&[("Otlyra", false)]),
        active: 0,
        history: (true, false),
        spinner: None,
        text: TextEngine::new(),
    };
    menu.ui.address.set_text("https://example.com/");
    menu.ui.menu_open = true;
    frames.push(menu);

    // The pointer resting on the reload button, so the hover wash is visible.
    let mut hovered = Frame {
        name: "hover",
        ui: BrowserUi::new(),
        tabs: tabs(&[("Otlyra", false), ("Second", false)]),
        active: 0,
        history: (true, true),
        spinner: None,
        text: TextEngine::new(),
    };
    hovered.ui.address.set_text("https://example.com/");
    hovered.ui.pointer_moved(80.0, UI_HEIGHT - 20.0);
    frames.push(hovered);

    // The settings, which are twice as tall as anything else here because they
    // scroll.
    let mut settings = SettingsFrame {
        ui: BrowserUi::new(),
        surface: otlyra_app::settings::SettingsSurface::new(),
        text: TextEngine::new(),
    };
    settings.ui.address.set_text("Settings");
    let tall = Viewport {
        width: (1100.0 * scale) as u32,
        height: (760.0 * scale) as u32,
        scale_factor: scale,
    };
    let png = render_offscreen(&mut settings, tall)?;
    let path = directory.join("settings.png");
    std::fs::write(&path, &png)?;
    println!("{}", path.display());

    // A short window, scrolled to the end: the state where a panel that gets
    // its own height wrong shows it.
    for (name, width, height, scroll) in [
        ("settings-short", 900.0, 420.0, 0.0),
        ("settings-scrolled", 900.0, 420.0, 100_000.0),
        ("settings-narrow", 620.0, 700.0, 0.0),
    ] {
        let mut frame = SettingsFrame {
            ui: BrowserUi::new(),
            surface: otlyra_app::settings::SettingsSurface::new(),
            text: TextEngine::new(),
        };
        frame.ui.address.set_text("about:settings");
        let viewport = Viewport {
            width: (width * scale) as u32,
            height: (height * scale) as u32,
            scale_factor: scale,
        };
        // One frame to measure against, then the scroll, then the frame that is
        // written: how far a panel can go is only known once it has been drawn.
        let _ = render_offscreen(&mut frame, viewport)?;
        frame.surface.scroll_by(scroll);
        let png = render_offscreen(&mut frame, viewport)?;
        let path = directory.join(format!("{name}.png"));
        std::fs::write(&path, &png)?;
        println!("{}", path.display());
    }

    for mut frame in frames {
        let name = frame.name;
        let png = render_offscreen(&mut frame, viewport)?;
        let path = directory.join(format!("{name}.png"));
        std::fs::write(&path, &png)?;
        println!("{}", path.display());
    }

    Ok(())
}
