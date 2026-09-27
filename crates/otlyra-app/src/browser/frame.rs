//! Building a frame: its geometry, the lists of its four layers, and what changed.
//!
//! Both ways a frame is painted — the whole surface at once, and the layers the
//! compositor keeps — draw from these helpers, so the two cannot disagree about
//! what a frame is. The epochs and dirty rectangles that let the compositor skip
//! work are worked out beside the lists they describe.

use std::sync::Arc;

use otlyra_platform::{LayerRect, Viewport};

use crate::ui::{SystemPage, TabLabel, UI_HEIGHT};

use super::Browser;

/// How long after the last step of a resize the window counts as still being
/// resized.
pub(super) const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// How long a layout may take and still be done at every step of a resize: half
/// a frame at sixty a second. A page that lays out faster follows the window
/// live; one that is slower is left as it was until the resize stops, rather
/// than taking the window with it.
const LIVE_LAYOUT: std::time::Duration = std::time::Duration::from_millis(8);

/// A logical display list, the scale it was scaled at, and the device list that
/// came out — one layer's answer to *has this already been scaled?*
pub(super) type Scaled = (
    Arc<otlyra_gfx::DisplayList>,
    f64,
    Arc<otlyra_gfx::DisplayList>,
);

/// Scale `logical` to device pixels, reusing `cache` while the same list is
/// being scaled by the same factor.
///
/// Pointer identity is the whole test: a surface that hands back the `Arc` it
/// handed back last frame is saying nothing it draws has moved, so the scaled
/// copy of it is still right. Free rather than a method because each layer keeps
/// its own cache and the borrow checker should see that they are separate.
fn scaled(
    cache: &mut Option<Scaled>,
    logical: Arc<otlyra_gfx::DisplayList>,
    scale: f64,
) -> Arc<otlyra_gfx::DisplayList> {
    if let Some((cached_logical, cached_scale, device)) = cache
        && Arc::ptr_eq(cached_logical, &logical)
        && *cached_scale == scale
    {
        return Arc::clone(device);
    }
    let mut copy = (*logical).clone();
    copy.transform(otlyra_gfx::kurbo::Affine::scale(scale));
    let device = Arc::new(copy);
    *cache = Some((logical, scale, Arc::clone(&device)));
    device
}

impl Browser {
    /// How far round the spinner is, or `None` when nothing is loading.
    ///
    /// A function of how long the load has been going rather than of a counter
    /// somewhere: a frame that arrives late then draws where the spinner should be
    /// now, not where the last frame left it.
    /// Any tab, not the active one. The strip draws a mark per tab and turns the
    /// ones that are loading, so a background tab's spinner needs a phase — and
    /// with the phase taken from the active tab alone, a tab loading behind a
    /// finished one drew a still dot.
    ///
    /// One clock for all of them rather than one each: several tabs loading at
    /// once turn together, which reads as one browser working rather than as
    /// several unrelated things.
    pub(super) fn spinner_phase(&self) -> Option<f32> {
        self.world
            .any_loading()
            .then(|| self.load_started.elapsed().as_secs_f32() * 4.0)
    }

    fn labels(&self) -> Vec<TabLabel> {
        self.tabs
            .iter()
            .map(|tab| TabLabel {
                id: tab.id.0,
                title: tab.title.clone(),
                loading: self.world.loading(tab.id),
            })
            .collect()
    }

    // --- Frame building, shared by the whole-surface and layered paths ---

    /// Run the once-per-frame prelude and settle the geometry and style inputs
    /// every region draws from. Both `paint` and `compose` start here, so they
    /// cannot disagree about what this frame is.
    pub(super) fn frame_geom(&mut self, viewport: Viewport) -> FrameGeom {
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
    pub(super) fn page_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
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
                SystemPage::Downloads => {
                    self.downloads_page.build_display_list(
                        content,
                        &self.downloads,
                        &mut self.text,
                        &mut list,
                    );
                }
                SystemPage::Bookmarks => {
                    self.bookmarks_page.build_display_list(
                        content,
                        &self.bookmarks,
                        &mut self.text,
                        &mut list,
                    );
                }
                SystemPage::Cookies => {
                    self.cookies_page.build_display_list(
                        content,
                        &self.cookies,
                        &mut self.text,
                        &mut list,
                    );
                }
                SystemPage::Cache => {
                    self.cache_page.build_display_list(
                        content,
                        self.cache.as_ref(),
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
        } else if let Some(device) = self.deferred_page(g) {
            device
        } else if self.page(self.active).is_some() && !self.blocked_on_style(self.active) {
            // Told before the frame is built, because it decides what `medium`
            // computes to and every element that inherited a size inherited that.
            // Laid out in the page's own pixels and drawn back up to the
            // window's. A zoom makes the CSS pixel larger, so the same window
            // holds fewer of them and the page reflows into what is left —
            // which is the difference between zooming a page and magnifying a
            // picture of one. The inset is divided too, so that scaling it back
            // up lands the page under the chrome rather than under a chrome the
            // zoom has moved.
            let zoom = f64::from(self.zoom);
            let logical = {
                let tab = self.tabs[self.active].id;
                let (page, text) = self.world.page_and_text(tab).expect("a page");
                page.set_text_scale(g.text_scale);
                page.set_color_scheme(g.page_scheme);
                page.build_display_list(
                    text,
                    (g.width / zoom) as f32,
                    (g.content_height / zoom) as f32,
                    (g.top / zoom) as f32,
                )
            };
            self.scaled_page(logical, g.scale_factor * zoom)
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

    /// The page as it was last drawn, while the window is being resized and the
    /// page is too slow to lay out at every step of it.
    ///
    /// What every browser does one way or another: the window's own interface
    /// follows the pointer, and the page catches up. Laying out a long page takes
    /// a tenth of a second or more, and doing it for each of the dozens of sizes a
    /// window passes through on the way to the one it is let go at is a window
    /// that cannot be resized at all. The page is drawn on its own colour, so the
    /// part of a wider window it does not reach is the page's and not a hole.
    fn deferred_page(&mut self, g: &FrameGeom) -> Option<Arc<otlyra_gfx::DisplayList>> {
        let until = self.resizing_until?;
        if std::time::Instant::now() >= until {
            self.resizing_until = None;
            return None;
        }
        let zoom = f64::from(self.zoom);
        let tab = &self.tabs[self.active];
        if tab.system.is_some() {
            return None;
        }
        let (width, took) = self.world.page(tab.id)?.last_layout()?;
        if (f64::from(width) - g.width / zoom).abs() < f64::EPSILON || took <= LIVE_LAYOUT {
            return None;
        }
        let (_, scale, device) = self.page_device.as_ref()?;
        (*scale == g.scale_factor * zoom).then(|| Arc::clone(device))
    }

    /// What the page is drawn on: its canvas colour, for a document.
    pub(super) fn page_background(&self) -> Option<otlyra_gfx::peniko::Color> {
        let tab = &self.tabs[self.active];
        match self.world.page(tab.id) {
            Some(page) if tab.system.is_none() => Some(page.canvas()),
            _ => None,
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
        scaled(&mut self.page_device, logical, scale)
    }

    /// The element overlay, when the inspector has chosen a box.
    pub(super) fn highlight_list(&mut self, g: &FrameGeom) -> Option<Arc<otlyra_gfx::DisplayList>> {
        self.chosen_box()?;
        let mut list = otlyra_gfx::DisplayList::new();
        self.paint_highlight(&mut list);
        list.transform(g.scale);
        Some(Arc::new(list))
    }

    /// The inspector dock. The caller draws it only when `g.dock > 0`.
    pub(super) fn inspector_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
        let logical = self.inspector_panel(g.width, g.top, g.content_height, g.dock);
        scaled(&mut self.inspector_device, logical, g.scale_factor)
    }

    /// The tab strip and toolbar.
    pub(super) fn chrome_list(&mut self, g: &FrameGeom) -> Arc<otlyra_gfx::DisplayList> {
        let labels = self.labels();
        let logical = self.ui.build_display_list(
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
        );
        scaled(&mut self.chrome_device, logical, g.scale_factor)
    }

    /// How far each part of the page's list has moved: what it is drawn with.
    ///
    /// Empty for a browser page or the blank fallback, which are drawn where they
    /// stand. A zoom is not applied here — the list's own transform carries it to
    /// whatever the offsets move.
    ///
    /// Each offset is put on the screen's pixel grid. A trackpad scrolls by
    /// fractions of a pixel, and a page drawn at a fraction is a page whose every
    /// edge is resampled — and whose pixels cannot be reused by moving them,
    /// because a move is whole pixels. Every browser does the same.
    pub(super) fn page_offsets(&self, g: &FrameGeom) -> Vec<otlyra_gfx::kurbo::Vec2> {
        let tab = &self.tabs[self.active];
        let device = g.scale_factor * f64::from(self.zoom);
        let snap = |value: f64| (value * device).round() / device;
        match self.world.page(tab.id) {
            Some(page) if tab.system.is_none() => page
                .offsets()
                .into_iter()
                .map(|offset| otlyra_gfx::kurbo::Vec2::new(snap(offset.x), snap(offset.y)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A content version for the page layer that changes exactly when the page's
    /// list would draw something different.
    ///
    /// The per-surface `builds` counters advance only on a real rebuild, so an
    /// unchanged page keeps its epoch and its retained pixels. The blank fallback
    /// has no such counter, so its inputs are hashed directly; the active tab
    /// index is folded in so switching between two tabs at the same build count
    /// still re-rasterizes.
    pub(super) fn page_epoch(&self, g: &FrameGeom) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.active.hash(&mut hasher);
        let tab = &self.tabs[self.active];
        if let Some(system) = tab.system {
            match system {
                SystemPage::Settings => 10u8,
                SystemPage::History => 11u8,
                SystemPage::Downloads => 12u8,
                SystemPage::Bookmarks => 14u8,
                SystemPage::Cookies => 15u8,
                SystemPage::Cache => 16u8,
                _ => 13u8,
            }
            .hash(&mut hasher);
            self.settings.builds().hash(&mut hasher);
            self.history_page.builds().hash(&mut hasher);
            self.downloads_page.builds().hash(&mut hasher);
            self.bookmarks_page.builds().hash(&mut hasher);
            self.cookies_page.builds().hash(&mut hasher);
            self.cache_page.builds().hash(&mut hasher);
            self.about.builds().hash(&mut hasher);
        } else if let Some(page) = self.world.page(tab.id) {
            1u8.hash(&mut hasher);
            page.builds().hash(&mut hasher);
            // The same list scrolled is a different picture.
            page.scrolls().hash(&mut hasher);
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

    /// The part of the page layer this frame changed, in device pixels, when the
    /// page changed only part of itself.
    ///
    /// The page answers in its own coordinates — where a box sits in the document
    /// — and this is the one place that turns those into the surface's: down by
    /// the scroll, along by the interface's height, and into device pixels. The
    /// rectangle is grown by a pixel on every side before it is rounded out, so
    /// that a glyph that was antialiased across the boundary is inside it.
    ///
    /// `None` means the whole layer, which is what a page says whenever it has
    /// restyled, relaid out, or scrolled. The compositor cuts what comes back to
    /// the layer's own bounds, so a field scrolled half out of view is not this
    /// function's problem.
    pub(super) fn page_dirty(&self, g: &FrameGeom) -> Option<LayerRect> {
        let page = self.page(self.active)?;
        let dirty = page.dirty()?;
        let scroll = f64::from(page.scroll());
        let left = (f64::from(dirty.x) - 1.0) * g.scale_factor;
        let top = (f64::from(dirty.y) - scroll + g.top - 1.0) * g.scale_factor;
        let right = (f64::from(dirty.x + dirty.width) + 1.0) * g.scale_factor;
        let bottom = (f64::from(dirty.y + dirty.height) - scroll + g.top + 1.0) * g.scale_factor;
        let x = left.floor().max(0.0) as u32;
        let y = top.floor().max(0.0) as u32;
        let width = (right.ceil().max(0.0) as u32).saturating_sub(x);
        let height = (bottom.ceil().max(0.0) as u32).saturating_sub(y);
        // A known dirty rectangle can be wholly outside the viewport (for
        // example, typing into a field that remains focused after scrolling).
        // Preserve that knowledge as an empty rectangle. `None` means the page
        // could not bound its change and therefore dirtied the whole layer.
        Some(LayerRect {
            x,
            y,
            width,
            height,
        })
    }

    /// The part of the chrome layer changed by this frame, in device pixels.
    pub(super) fn chrome_dirty(&self, g: &FrameGeom) -> Option<LayerRect> {
        let dirty = self.ui.dirty()?;
        let left = (dirty.x * g.scale_factor).floor().max(0.0) as u32;
        let top = (dirty.y * g.scale_factor).floor().max(0.0) as u32;
        let right = ((dirty.x + dirty.width) * g.scale_factor).ceil().max(0.0) as u32;
        let bottom = ((dirty.y + dirty.height) * g.scale_factor).ceil().max(0.0) as u32;
        Some(LayerRect {
            x: left,
            y: top,
            width: right.saturating_sub(left),
            height: bottom.saturating_sub(top),
        })
    }

    /// A content version for the chrome layer. The tab strip and toolbar each
    /// rebuild only when their own inputs change, so the sum of their build
    /// counters moves exactly when the chrome's pixels would.
    pub(super) fn chrome_epoch(&self) -> u64 {
        self.ui
            .builds()
            .wrapping_add(self.ui.tab_builds())
            .wrapping_add(self.ui.toolbar_builds())
    }

    /// A content version for the inspector layer, summing its retained
    /// boundaries' build counters for the same reason.
    pub(super) fn inspector_epoch(&self) -> u64 {
        self.inspector
            .builds()
            .wrapping_add(self.inspector.header_builds())
            .wrapping_add(self.inspector.body_builds())
    }

    /// A content version for the element overlay: the identity and geometry of
    /// the chosen box, so it re-rasterizes when the highlight moves and not
    /// otherwise.
    pub(super) fn highlight_epoch(&self) -> u64 {
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
pub(super) const LAYER_PAGE: u64 = 0;
pub(super) const LAYER_HIGHLIGHT: u64 = 1;
pub(super) const LAYER_INSPECTOR: u64 = 2;
pub(super) const LAYER_CHROME: u64 = 3;

/// The per-frame geometry and style inputs both paint paths share.
pub(super) struct FrameGeom {
    width: f64,
    height: f64,
    pub(super) scale_factor: f64,
    scale: otlyra_gfx::kurbo::Affine,
    pub(super) top: f64,
    pub(super) dock: f64,
    pub(super) content_height: f64,
    text_scale: f32,
    page_scheme: otlyra_css::cascade::ColorScheme,
}
