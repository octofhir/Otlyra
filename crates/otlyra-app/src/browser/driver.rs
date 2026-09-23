//! What a caller with no window uses: waits, held requests, and a frame as a picture.
//!
//! A screenshot, a test and a protocol driver have no loop to be woken by and no
//! pointer to reach anything with. They wait for a load or a frame instead, hold
//! requests and let them go, and take a frame as a PNG — none of which the window
//! ever does, so none of it is mixed into what the window does.

use otlyra_platform::{Painter, Viewport};

use crate::page::PageScene;

use super::{Browser, Readiness};

/// How long a caller with no event loop waits between checks for a finished fetch.
const FETCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

impl Browser {
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
            // Everything left is stopped at the gate, waiting for a driver — and
            // the driver is answered by this thread. Waiting here would be waiting
            // for a command nobody is left to read, which is a deadlock and not a
            // slow page. The frame goes out with what arrived.
            if self.only_held_remains() {
                tracing::debug!("a driver is holding everything this frame was waiting for");
                return;
            }
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

    /// Draw a frame from what has already arrived, waiting for nothing.
    ///
    /// What a command loop uses. [`Browser::prepare_frame`] waits for the
    /// pictures and the fonts a frame asked for, which is right for a screenshot
    /// and wrong for anything that also has a socket to read: the wait is on the
    /// network, and a thread inside it is a thread not answering commands.
    pub fn draw_frame(&mut self, viewport: Viewport) {
        self.paint(&mut otlyra_gfx::RecordingPainter::new(), viewport);
    }

    /// Whether everything still outstanding is stopped at the gate.
    ///
    /// `false` when nothing is outstanding at all, because *nothing to wait for*
    /// is the loop's own condition and not this one's.
    fn only_held_remains(&self) -> bool {
        let mut any = false;
        for id in self
            .background_fetches
            .keys()
            .chain(self.font_fetches.keys())
            .chain(self.picture_fetches.keys())
        {
            any = true;
            if !self.fetcher.is_held(*id) {
                return false;
            }
        }
        any
    }

    /// How far the tab at `index` has got.
    ///
    /// Read rather than pushed, like everything else a driver asks about: the
    /// browser is driven from one thread and its state is already here, so the
    /// protocol reads it between events instead of the loader calling into a
    /// socket. A tab with nothing in flight is complete however little is in it —
    /// a blank tab has arrived everywhere it was going.
    pub fn readiness(&self, index: usize) -> Readiness {
        match self.tabs.get(index).and_then(|tab| tab.pending.as_ref()) {
            None => Readiness::Complete,
            Some(pending) if pending.document_arrived => Readiness::Interactive,
            Some(_) => Readiness::Started,
        }
    }

    /// Wait for the tab to finish loading, for callers with no event loop.
    ///
    /// The window never calls this — it is woken instead. A screenshot and a test
    /// have nowhere to be woken from, and waiting is what they mean by "load".
    pub fn wait_for_load(&mut self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.tabs.iter().any(|tab| tab.loading()) {
            // The document itself is stopped at the gate. Only a driver can let it
            // go, and a driver is answered by whichever thread called this — so
            // the wait would be for a command that cannot be read until it ends.
            if self.tabs.iter().all(|tab| {
                tab.pending
                    .as_ref()
                    .is_none_or(|pending| self.fetcher.is_held(pending.document))
            }) {
                tracing::debug!("a driver is holding the document this wait was for");
                return;
            }
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

    /// Hold every request whose address `gate` says yes to, before it is sent.
    ///
    /// Only a driver sets this, and only while it has said it wants to intercept:
    /// a browser nobody is driving holds nothing, and a request held with nobody
    /// to let it go is a page that never loads. See [`crate::fetcher::Fetcher`].
    pub fn hold_requests(&mut self, gate: Option<crate::fetcher::Gate>) {
        self.fetcher.set_gate(gate);
    }

    /// The requests being held, oldest first.
    pub fn held(&self) -> &[crate::fetcher::Held] {
        self.fetcher.held()
    }

    /// Let a held request go, with whatever was changed about it.
    pub fn resume_request(&mut self, id: u64, change: crate::fetcher::Change) -> bool {
        self.fetcher.resume(id, change)
    }

    /// Answer a held request with a response nobody sent.
    pub fn fulfil_request(&mut self, id: u64, response: crate::fetcher::Loaded) -> bool {
        self.fetcher.fulfil(id, response)
    }

    /// End a held request as a failure, which is what blocking one means.
    pub fn fail_request(&mut self, id: u64, why: &str) -> bool {
        self.fetcher.fail(id, why)
    }

    /// Every request this browser has made, oldest first.
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

    /// A picture of one rectangle of what the frame would have drawn.
    ///
    /// # Why this is not a crop
    ///
    /// The obvious way to answer *photograph this element* is to render the whole
    /// frame and cut the rectangle out of the pixels. That means decoding a PNG
    /// to cut it and encoding another — a dependency this crate does not have —
    /// and it rasterizes a page's worth of glyphs to keep a button.
    ///
    /// This composes the same lists the frame is made of, moves them so the
    /// rectangle's corner is the origin, and rasterizes into a surface the size
    /// of the rectangle. Nothing outside it is drawn at all, so a clip of a
    /// hundred pixels costs a hundred pixels. `clip` is in the same logical
    /// coordinates every other geometry here is in.
    pub fn screenshot_clipped(
        &mut self,
        viewport: Viewport,
        clip: otlyra_layout::Rect,
    ) -> Result<Vec<u8>, String> {
        let scale = viewport.scale_factor;
        let width = (f64::from(clip.width) * scale).round().max(1.0) as u32;
        let height = (f64::from(clip.height) * scale).round().max(1.0) as u32;

        // The frame, as `paint` composes it, in one list rather than four
        // renders: a list can be moved and four `render` calls cannot.
        let geom = self.frame_geom(viewport);
        let mut whole = otlyra_gfx::DisplayList::new();
        whole.append(&self.page_list(&geom));
        if let Some(highlight) = self.highlight_list(&geom) {
            whole.append(&highlight);
        }
        if self.interface {
            if geom.dock > 0.0 {
                whole.append(&self.inspector_list(&geom));
            }
            whole.append(&self.chrome_list(&geom));
        }
        self.after_frame();

        whole.transform(otlyra_gfx::kurbo::Affine::scale(scale).then_translate(
            otlyra_gfx::kurbo::Vec2::new(-f64::from(clip.x) * scale, -f64::from(clip.y) * scale),
        ));

        let mut target = otlyra_gfx::SkiaPainter::new_raster(width, height)
            .map_err(|error| error.to_string())?;
        otlyra_gfx::render(&whole, &mut target);
        target.encode_png().map_err(|error| error.to_string())
    }
}
