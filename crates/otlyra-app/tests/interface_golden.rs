//! The interface against its golden PNGs, at both scales.
//!
//! Two frames, chosen for what they rasterize rather than for coverage: the
//! toolbar with tabs, history, a spinner and an address exercises text, icons
//! and the strip's layering; the settings surface exercises the controls. The
//! other states — hover, focus, selection, the open menu — are pinned by the
//! display-list outlines in `surface_snapshot.rs`, which fail when a control
//! moves by a pixel without the weight of a PNG per state.
//!
//! Both scales always: the interface bug that got furthest was invisible at 1×
//! and doubled everything inside a scrolling panel at 2×.
//!
//! Drawn with [`TextEngine::isolated`], so the vendored font shapes every label
//! and a machine's own fonts cannot move the picture. Regenerate deliberately:
//!
//! ```sh
//! OTLYRA_UPDATE_GOLDEN=1 cargo test -p otlyra-app --test interface_golden
//! ```

mod common;

use otlyra_app::settings::SettingsSurface;
use otlyra_app::ui::{BrowserUi, Rect, TabLabel, UI_HEIGHT};
use otlyra_app::widget::theme::Theme;
use otlyra_gfx::DisplayList;
use otlyra_gfx::PaintTarget;
use otlyra_gfx::kurbo::Affine;
use otlyra_platform::{Painter, PlatformEvent, Viewport, render_offscreen};
use otlyra_text::TextEngine;

/// The scales every golden exists at.
const SCALES: [f64; 2] = [1.0, 2.0];

fn tabs(titles: &[(&str, bool)]) -> Vec<TabLabel> {
    titles
        .iter()
        .enumerate()
        .map(|(index, (title, loading))| TabLabel {
            id: index as u64 + 1,
            title: (*title).to_owned(),
            loading: *loading,
        })
        .collect()
}

/// The toolbar over a blank page: the busy state, all in one frame.
struct InterfaceFrame {
    ui: BrowserUi,
    tabs: Vec<TabLabel>,
    text: TextEngine,
}

impl InterfaceFrame {
    fn new() -> Self {
        let mut ui = BrowserUi::new();
        ui.address.set_text("https://example.com/some/path");
        // A toolbar showing an address is a toolbar on a page, so the star is the
        // ordinary hollow-and-pressable one rather than the dimmed blank-tab star.
        ui.bookmark = otlyra_app::ui::Bookmarked::No;
        Self {
            ui,
            tabs: tabs(&[
                ("CSS support — Otlyra", false),
                ("A title long enough that it has to be cut short", true),
                ("Otlyra", false),
            ]),
            text: TextEngine::isolated(),
        }
    }

    fn dark() -> Self {
        let mut frame = Self::new();
        frame.ui.set_theme(Theme::dark());
        frame
    }
}

impl Painter for InterfaceFrame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
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
        list.append(&self.ui.build_display_list(
            viewport.logical_width(),
            viewport.logical_height(),
            &self.tabs,
            0,
            (true, true),
            // A fixed phase, not an animation: the spinner is drawn at exactly
            // this angle every time.
            Some(1.2),
            &mut self.text,
        ));
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

/// The settings surface under the toolbar, as the browser shows it.
struct SettingsFrame {
    ui: BrowserUi,
    surface: SettingsSurface,
    text: TextEngine,
}

impl SettingsFrame {
    fn new() -> Self {
        let mut ui = BrowserUi::new();
        ui.address.set_text("about:settings");
        // The download folder is named rather than left to the platform, which
        // would draw whoever ran the test into the picture: the default is the
        // home directory's `Downloads`.
        let mut settings = otlyra_app::settings::Settings::default();
        settings.apply(otlyra_app::settings::Action::SetDownloadDirectory(
            "/downloads".to_owned(),
        ));
        Self {
            ui,
            surface: SettingsSurface::with(settings),
            text: TextEngine::isolated(),
        }
    }

    fn dark() -> Self {
        let mut frame = Self::new();
        frame.ui.set_theme(Theme::dark());
        frame.surface.set_theme(Theme::dark());
        frame
    }
}

impl Painter for SettingsFrame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let (width, height) = (viewport.logical_width(), viewport.logical_height());
        let mut list = DisplayList::new();
        self.surface.build_display_list(
            Rect::new(0.0, UI_HEIGHT, width, (height - UI_HEIGHT).max(0.0)),
            &mut self.text,
            &mut list,
        );
        list.append(&self.ui.build_display_list(
            width,
            height,
            &tabs(&[("Settings", false)]),
            0,
            (true, false),
            None,
            &mut self.text,
        ));
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

/// The cookies surface under the toolbar, with a jar somebody has been browsing
/// with.
///
/// The addresses and the values are invented and fixed, so the picture is the
/// same on every machine — and the values are here at all only to pin that they
/// do *not* appear: a golden is the one test that would catch a value getting
/// drawn.
struct CookiesFrame {
    ui: BrowserUi,
    surface: otlyra_app::cookies::CookiesSurface,
    store: otlyra_app::cookies::CookieStore,
    text: TextEngine,
}

impl CookiesFrame {
    fn new() -> Self {
        let mut ui = BrowserUi::new();
        ui.address.set_text("about:cookies");
        let store = otlyra_app::cookies::CookieStore::in_memory();
        // A fixed instant, so an expiry is the same distance away every run.
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        store.with(|jar| {
            for (address, line) in [
                ("https://www.example.com/app/x", "session=abc123; HttpOnly"),
                ("https://www.example.com/", "theme=dark; Max-Age=600"),
                (
                    "https://api.example.com/",
                    "token=xyz789; Domain=example.com",
                ),
                (
                    "https://analytics.test/",
                    "visitor=0f3d; Max-Age=600; Secure; SameSite=None",
                ),
                ("https://news.test/", "consent=yes; Max-Age=600"),
            ] {
                jar.set(&url::Url::parse(address).expect("a url"), line, now)
                    .expect("kept");
            }
        });
        Self {
            ui,
            surface: otlyra_app::cookies::CookiesSurface::new(),
            store,
            text: TextEngine::isolated(),
        }
    }

    fn dark() -> Self {
        let mut frame = Self::new();
        frame.ui.set_theme(Theme::dark());
        frame.surface.set_theme(Theme::dark());
        frame
    }
}

impl Painter for CookiesFrame {
    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let (width, height) = (viewport.logical_width(), viewport.logical_height());
        let mut list = DisplayList::new();
        self.surface.build_display_list(
            Rect::new(0.0, UI_HEIGHT, width, (height - UI_HEIGHT).max(0.0)),
            &self.store,
            &mut self.text,
            &mut list,
        );
        list.append(&self.ui.build_display_list(
            width,
            height,
            &tabs(&[("Cookies", false)]),
            0,
            (true, false),
            None,
            &mut self.text,
        ));
        list.transform(Affine::scale(viewport.scale_factor));
        otlyra_gfx::render(&list, target);
    }

    fn on_event(&mut self, _event: PlatformEvent) {}
}

fn assert_frame_matches(name: &str, frame: &mut dyn Painter, width: f64, height: f64) {
    for scale in SCALES {
        let viewport = Viewport::new(
            (width * scale).round() as u32,
            (height * scale).round() as u32,
            scale,
        );
        let rendered = render_offscreen(frame, viewport).expect("render a frame");
        let path = common::goldens_dir()
            .join("interface")
            .join(format!("{name}@{scale}x.png"));
        common::assert_matches_golden(&rendered, &path);
    }
}

#[test]
fn the_toolbar_matches_its_goldens_at_both_scales() {
    assert_frame_matches("interface", &mut InterfaceFrame::new(), 900.0, 300.0);
}

#[test]
fn the_settings_match_their_goldens_at_both_scales() {
    assert_frame_matches("settings", &mut SettingsFrame::new(), 900.0, 700.0);
}

// Dark is a palette, not a layout — but a palette is exactly what a golden
// PNG pins and an outline snapshot does not: the outline records geometry
// and named colours, the picture records what they rasterize to.

#[test]
fn the_dark_toolbar_matches_its_goldens_at_both_scales() {
    assert_frame_matches("interface-dark", &mut InterfaceFrame::dark(), 900.0, 300.0);
}

#[test]
fn the_dark_settings_match_their_goldens_at_both_scales() {
    assert_frame_matches("settings-dark", &mut SettingsFrame::dark(), 900.0, 700.0);
}

#[test]
fn the_cookies_match_their_goldens_at_both_scales() {
    assert_frame_matches("cookies", &mut CookiesFrame::new(), 900.0, 700.0);
}

#[test]
fn the_dark_cookies_match_their_goldens_at_both_scales() {
    assert_frame_matches("cookies-dark", &mut CookiesFrame::dark(), 900.0, 700.0);
}
