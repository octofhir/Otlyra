//! What a page asks for once it has been styled: background pictures, pictures
//! chosen again for a new window, and the fonts its sheets bring (in `fonts`).
//!
//! These are named by rules or chosen against the window rather than named by the
//! markup, so they are known only on the way to a frame and are asked for after
//! one. The decoded pictures are kept here too, because every picture — whoever
//! asked for it — is decoded once and looked up by its address.

use otlyra_dom::NodeId;

use crate::fetcher::{Fetched, ResourceKind};
use crate::page::PageScene;

use super::super::TabId;
use super::PageWorld;
use super::load::IMAGE_LIMIT;

/// How many bytes of decoded pictures are kept between loads.
///
/// Decoded, not encoded: a 200 KB photograph is 8 MB of pixels, and it is the
/// pixels this holds. Sixty-four megabytes is a few screenfuls of them.
pub(in crate::browser) const IMAGE_CACHE_BUDGET: usize = 64 * 1024 * 1024;

/// Decoded pictures, kept by address.
///
/// A page that shows the same picture twice decodes it once, and going back to a
/// page that has been visited does not decode its pictures again. Least recently
/// used goes first, because the page in front of the reader is the one whose
/// pictures are worth keeping.
#[derive(Default)]
pub(in crate::browser) struct ImageCache {
    /// Oldest use first; the end is the most recently used.
    entries: Vec<(String, otlyra_gfx::peniko::ImageData)>,
    pub(in crate::browser) bytes: usize,
}

impl ImageCache {
    /// The picture at `url`, if it is here, marked as just used.
    pub(in crate::browser) fn get(&mut self, url: &str) -> Option<otlyra_gfx::peniko::ImageData> {
        let at = self.entries.iter().position(|(key, _)| key == url)?;
        let entry = self.entries.remove(at);
        let image = entry.1.clone();
        self.entries.push(entry);
        Some(image)
    }

    /// Keep `image` under `url`, evicting the least recently used until it fits.
    pub(in crate::browser) fn insert(&mut self, url: String, image: otlyra_gfx::peniko::ImageData) {
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

impl PageWorld {
    /// Ask for the background pictures the pages have found they need.
    ///
    /// A background is named by a rule, so what a page wants is known only once it
    /// has been styled — which happens on the way to a frame. This is called after
    /// one, and the pictures arrive for the frame after that.
    fn fetch_backgrounds(&mut self, tabs: &[TabId]) {
        if !self.environment.load_images {
            return;
        }
        for &tab in tabs {
            let Some(page) = self.page(tab) else {
                continue;
            };
            // Absolute already: every sheet resolved its own `url()`s as it was
            // parsed. The document is still asked whether it may reach them.
            let document = page.url().to_string();
            let wanted: Vec<String> = page
                .wanted_pictures()
                .into_iter()
                .filter(|url| !self.background_requests.contains_key(url))
                .take(IMAGE_LIMIT)
                .collect();

            for url in wanted {
                if let Some(picture) = self.images.get(&url) {
                    if let Some(page) = self.page_mut(tab) {
                        page.set_picture(url.clone(), picture);
                    }
                    self.background_requests.insert(url, tab);
                    continue;
                }
                let reachable =
                    url::Url::parse(&url).is_ok_and(|target| Self::may_reach(&document, &target));
                if !reachable {
                    // Recorded anyway: a picture that may not be fetched must not be
                    // asked for again on every frame.
                    self.background_requests.insert(url, tab);
                    continue;
                }
                let id = self.fetcher.request(&url, ResourceKind::Image);
                self.background_requests.insert(url.clone(), tab);
                self.background_fetches.insert(id, (tab, url));
            }
        }
    }

    /// Ask each element again which of the pictures it offers this window wants.
    ///
    /// A page chooses among the files a `srcset` lists against the window it is
    /// loading into, and a window is widened, narrowed and dragged between screens
    /// of different densities. So the question is put again whenever the window is
    /// not the one the pictures on screen were chosen against — and only then,
    /// because asking walks every document.
    ///
    /// Only elements whose picture has already arrived: one that never loaded is
    /// the load's business, and re-asking for it here would fetch it a second time.
    fn rechoose_pictures(&mut self) {
        if !self.environment.load_images {
            return;
        }
        let viewport = self.environment.viewport;
        let window = (viewport.width, viewport.scale);
        if self.picture_window == Some(window) {
            return;
        }
        self.picture_window = Some(window);

        let tabs: Vec<TabId> = self.slots.keys().copied().collect();
        for tab in tabs {
            let Some(page) = self.page(tab) else {
                continue;
            };
            let (document, base) = (page.url().to_string(), page.base_url().clone());
            let changed: Vec<otlyra_layout::ImageSource> =
                otlyra_layout::image_sources(page.document(), viewport)
                    .into_iter()
                    .take(IMAGE_LIMIT)
                    .filter(|source| {
                        page.picture_source(source.node)
                            .is_some_and(|(src, density)| {
                                src != source.src || density != source.density
                            })
                    })
                    .collect();

            for source in changed {
                let Some(target) = Self::subresource_url(&document, &base, &source.src) else {
                    continue;
                };
                // Already decoded: no request, straight into the page.
                if let Some(data) = self.images.get(&target)
                    && let Some(page) = self.page_mut(tab)
                {
                    page.set_image(
                        source.node,
                        source.src,
                        otlyra_layout::Picture {
                            data,
                            density: source.density,
                        },
                    );
                    continue;
                }
                let id = self.fetcher.request(&target, ResourceKind::Image);
                self.picture_fetches
                    .insert(id, (tab, source.node, source.src, source.density));
            }
        }
    }

    /// The picture and font work that follows a frame, once the rules that name
    /// them have been computed on the way to one.
    pub(in crate::browser) fn after_frame(&mut self) {
        // Which tabs have anything new to be asked about. A page whose display
        // list was reused cannot want a picture or a font it did not want
        // before: the rules that name them are computed on the way to a list,
        // and no list was built. Asking anyway walks the whole box tree and the
        // whole document, per tab, per frame.
        let sweep: Vec<TabId> = self
            .slots
            .iter()
            .filter(|(_, slot)| {
                slot.page
                    .as_ref()
                    .is_some_and(|page| slot.swept_at_build != Some(page.builds()))
            })
            .map(|(tab, _)| *tab)
            .collect();
        if !sweep.is_empty() {
            self.fetch_backgrounds(&sweep);
            self.fetch_fonts(&sweep);
            for tab in sweep {
                if let Some(slot) = self.slots.get_mut(&tab) {
                    slot.swept_at_build = slot.page.as_ref().map(PageScene::builds);
                }
            }
        }
        // Last, because it is a question about the window this frame was drawn
        // for: the answer is for the next one. It has a guard of its own.
        self.rechoose_pictures();
    }

    /// A background picture a page asked for arrived. It belongs to a page
    /// rather than to a load, and may arrive long after the page it is for.
    pub(super) fn background_arrived(&mut self, tab: TabId, url: String, fetched: Fetched) -> bool {
        let Ok(mut loaded) = fetched.result else {
            tracing::warn!(%url, "background picture failed to load");
            return false;
        };
        match loaded.picture() {
            Ok(picture) => {
                self.images.insert(fetched.url.clone(), picture.clone());
                match self.page_mut(tab) {
                    Some(page) => page.set_picture(url, picture),
                    None => tracing::warn!(%url, "no page to give the picture to"),
                }
                true
            }
            Err(error) => {
                tracing::warn!(%url, %error, "background picture failed to decode");
                false
            }
        }
    }

    /// A picture an element chose again after the page was built arrived: the
    /// same element, a different file.
    pub(super) fn picture_arrived(
        &mut self,
        tab: TabId,
        node: NodeId,
        src: String,
        density: f32,
        fetched: Fetched,
    ) -> bool {
        let Ok(mut loaded) = fetched.result else {
            tracing::warn!(%src, "re-chosen picture failed to load");
            return false;
        };
        match loaded.picture() {
            Ok(data) => {
                self.images.insert(fetched.url.clone(), data.clone());
                match self.page_mut(tab) {
                    Some(page) => {
                        page.set_image(node, src, otlyra_layout::Picture { data, density })
                    }
                    None => tracing::warn!(%src, "no page to give the picture to"),
                }
                true
            }
            Err(error) => {
                tracing::warn!(%src, %error, "re-chosen picture failed to decode");
                false
            }
        }
    }
}
