//! The page zoom: a larger CSS pixel for one site, and the coordinates it bends.
//!
//! A zoom is remembered against a site, stepped along a ladder, and changes the
//! units every question a pointer asks the page is asked in. None of that is about
//! loading or drawing a page, so it lives here with the two conversions it forces.

use crate::ui::UI_HEIGHT;

use super::Browser;

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

impl Browser {
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
    pub(super) fn restore_zoom(&mut self) {
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
    pub(super) fn in_page(&self, x: f64, y: f64) -> (f64, f64) {
        let zoom = f64::from(self.zoom);
        (x / zoom, y / zoom)
    }

    /// The page's top inset, in the page's own coordinates.
    pub(super) fn page_top(&self) -> f32 {
        ((if self.interface { UI_HEIGHT } else { 0.0 }) / f64::from(self.zoom)) as f32
    }
}
