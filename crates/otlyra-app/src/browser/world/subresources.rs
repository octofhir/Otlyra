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
    entries: Vec<(String, crate::fetcher::Decoded)>,
    pub(in crate::browser) bytes: usize,
}

impl ImageCache {
    /// The picture at `url`, if it is here, marked as just used.
    pub(in crate::browser) fn get(&mut self, url: &str) -> Option<crate::fetcher::Decoded> {
        let at = self.entries.iter().position(|(key, _)| key == url)?;
        let entry = self.entries.remove(at);
        let image = entry.1.clone();
        self.entries.push(entry);
        Some(image)
    }

    /// Keep `image` under `url`, evicting the least recently used until it fits.
    pub(in crate::browser) fn insert(&mut self, url: String, image: crate::fetcher::Decoded) {
        let size = image.image.data.as_ref().len();
        if size > IMAGE_CACHE_BUDGET {
            // One picture larger than the whole budget is not worth evicting
            // everything else for.
            return;
        }
        if let Some(at) = self.entries.iter().position(|(key, _)| *key == url) {
            let (_, old) = self.entries.remove(at);
            self.bytes -= old.image.data.as_ref().len();
        }

        while self.bytes + size > IMAGE_CACHE_BUDGET && !self.entries.is_empty() {
            let (_, evicted) = self.entries.remove(0);
            self.bytes -= evicted.image.data.as_ref().len();
        }
        self.bytes += size;
        self.entries.push((url, image));
    }
}

/// A picture one element asked for outside its document's load.
pub(super) struct PictureFetch {
    tab: TabId,
    /// The slot generation of the document whose element asked.
    generation: u64,
    node: NodeId,
    /// The address as the markup spells it.
    src: String,
    density: f32,
}

impl PictureFetch {
    /// Whether this was asked for by `tab`'s page.
    pub(super) fn of(&self, tab: TabId) -> bool {
        self.tab == tab
    }
}

impl PageWorld {
    /// Ask for the pictures elements want and do not have, once the load that
    /// asked for the markup's own pictures is over.
    ///
    /// A page's script adds pictures and changes the one an element shows long
    /// after its load: a gallery, a framework rendering its first view, a
    /// placeholder swapped for the real thing. Each element and address is
    /// asked for once; a decoded picture already kept is handed over directly.
    fn fetch_element_pictures(&mut self, tabs: &[TabId]) {
        if !self.environment.load_images {
            return;
        }
        let viewport = self.environment.viewport;
        for &tab in tabs {
            let Some(slot) = self.slots.get(&tab) else {
                continue;
            };
            if slot.pending.is_some() {
                continue;
            }
            let Some(page) = slot.page.as_ref() else {
                continue;
            };
            let generation = slot.generation;
            let (document, base) = (page.url().to_string(), page.base_url().clone());
            let wanted: Vec<otlyra_layout::ImageSource> =
                otlyra_layout::image_sources(page.document(), viewport)
                    .into_iter()
                    // The limit is on what is asked for at once, so it counts
                    // only what still has to be: a page that has shown 256
                    // pictures and adds one more is shown that one too.
                    .filter(|source| {
                        page.picture_source(source.node)
                            != Some((source.src.as_str(), source.density))
                    })
                    .take(IMAGE_LIMIT)
                    .collect();
            for source in wanted {
                let Some(target) = Self::subresource_url(&document, &base, &source.src) else {
                    continue;
                };
                if let Some(data) = self.images.get(&target)
                    && let Some(page) = self.page_mut(tab)
                {
                    let size = data.natural_size();
                    page.set_image(
                        source.node,
                        source.src.clone(),
                        data.for_element(source.density),
                    );
                    self.record_pictures(tab, vec![(source.node, source.src, Some(size))]);
                    continue;
                }
                let Some(slot) = self.slots.get_mut(&tab) else {
                    break;
                };
                if !slot
                    .pictures_asked
                    .insert((source.node, source.src.clone()))
                {
                    continue;
                }
                tracing::debug!(
                    target: "page.subresource",
                    url = %target,
                    "asking for a picture an element wants after its load"
                );
                let id = self
                    .fetcher
                    .request_for(tab.0, &target, ResourceKind::Image);
                self.picture_fetches.insert(
                    id,
                    PictureFetch {
                        tab,
                        generation,
                        node: source.node,
                        src: source.src,
                        density: source.density,
                    },
                );
            }
        }
    }

    /// Fetch the pictures detached `<img>` elements asked for — `new Image()`
    /// preloading — so their elements hear `load` or `error`.
    pub(super) fn fetch_detached_pictures(&mut self) {
        let tabs: Vec<TabId> = self.slots.keys().copied().collect();
        for tab in tabs {
            let Some(slot) = self.slots.get_mut(&tab) else {
                continue;
            };
            if slot.pending.is_some() {
                continue;
            }
            let generation = slot.generation;
            let Some(page) = slot.page.as_mut() else {
                continue;
            };
            let wanted = page.take_detached_pictures();
            if wanted.is_empty() {
                continue;
            }
            let (document, base) = (page.url().to_string(), page.base_url().clone());
            // Asked now, so the sweep does not ask again once it is inserted.
            slot.pictures_asked.extend(wanted.iter().cloned());
            for (node, src) in wanted {
                let Some(target) = Self::subresource_url(&document, &base, &src) else {
                    self.record_pictures(tab, vec![(node, src, None)]);
                    continue;
                };
                if !self.environment.load_images {
                    continue;
                }
                if let Some(data) = self.images.get(&target) {
                    self.record_pictures(tab, vec![(node, src, Some(data.natural_size()))]);
                    continue;
                }
                let id = self
                    .fetcher
                    .request_for(tab.0, &target, ResourceKind::Image);
                self.picture_fetches.insert(
                    id,
                    PictureFetch {
                        tab,
                        generation,
                        node,
                        src,
                        density: 1.0,
                    },
                );
            }
        }
    }

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
                let id = self.fetcher.request_for(tab.0, &url, ResourceKind::Image);
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
                    .filter(|source| {
                        page.picture_source(source.node)
                            .is_some_and(|(src, density)| {
                                src != source.src || density != source.density
                            })
                    })
                    .take(IMAGE_LIMIT)
                    .collect();

            for source in changed {
                let Some(target) = Self::subresource_url(&document, &base, &source.src) else {
                    continue;
                };
                // Already decoded: no request, straight into the page.
                if let Some(data) = self.images.get(&target)
                    && let Some(page) = self.page_mut(tab)
                {
                    let size = data.natural_size();
                    page.set_image(
                        source.node,
                        source.src.clone(),
                        data.for_element(source.density),
                    );
                    self.record_pictures(tab, vec![(source.node, source.src, Some(size))]);
                    continue;
                }
                let Some(slot) = self.slots.get_mut(&tab) else {
                    break;
                };
                slot.pictures_asked
                    .insert((source.node, source.src.clone()));
                let generation = slot.generation;
                let id = self
                    .fetcher
                    .request_for(tab.0, &target, ResourceKind::Image);
                self.picture_fetches.insert(
                    id,
                    PictureFetch {
                        tab,
                        generation,
                        node: source.node,
                        src: source.src,
                        density: source.density,
                    },
                );
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
            self.fetch_element_pictures(&sweep);
            self.fetch_backgrounds(&sweep);
            self.fetch_fonts(&sweep);
            for tab in sweep {
                if let Some(slot) = self.slots.get_mut(&tab) {
                    slot.swept_at_build = slot.page.as_ref().map(PageScene::builds);
                }
            }
        }
        self.fetch_detached_pictures();
        // Last, because it is a question about the window this frame was drawn
        // for: the answer is for the next one. It has a guard of its own.
        self.rechoose_pictures();
        // The pages' `<iframe>` elements were laid out on the way to this
        // frame: their frames take those sizes, and are drawn into them.
        self.sync_frames();
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
                tracing::warn!(
                    %url,
                    %error,
                    status = ?loaded.status,
                    content_type = ?loaded.content_type,
                    bytes = loaded.bytes.len(),
                    "background picture failed to decode"
                );
                false
            }
        }
    }

    /// A picture an element asked for after its document's load arrived.
    pub(super) fn picture_arrived(&mut self, fetch: PictureFetch, fetched: Fetched) -> bool {
        let PictureFetch {
            tab,
            generation,
            node,
            src,
            density,
        } = fetch;
        // The element's document is gone, or is being replaced: node ids name
        // elements of one document only.
        let current = self.slots.get(&tab).is_some_and(|slot| {
            slot.generation == generation
                && slot
                    .pending
                    .as_ref()
                    .is_none_or(|pending| !pending.document_arrived)
        });
        if !current {
            return false;
        }
        let Ok(mut loaded) = fetched.result else {
            tracing::warn!(%src, "picture failed to load");
            self.record_pictures(tab, vec![(node, src, None)]);
            return false;
        };
        match loaded.picture() {
            Ok(data) => {
                self.images.insert(fetched.url.clone(), data.clone());
                let size = data.natural_size();
                match self.page_mut(tab) {
                    // A detached element shows nothing; it only hears `load`.
                    Some(page) if !page.is_connected(node) => {}
                    Some(page) => page.set_image(node, src.clone(), data.for_element(density)),
                    None => tracing::warn!(%src, "no page to give the picture to"),
                }
                self.record_pictures(tab, vec![(node, src, Some(size))]);
                true
            }
            Err(error) => {
                // What came instead of a picture says why: a counter's pixel
                // answered with an empty body or a page is not a broken decoder.
                tracing::warn!(
                    %src,
                    %error,
                    status = ?loaded.status,
                    content_type = ?loaded.content_type,
                    bytes = loaded.bytes.len(),
                    final_url = %loaded.final_url,
                    "picture failed to decode"
                );
                self.record_pictures(tab, vec![(node, src, None)]);
                false
            }
        }
    }
}
