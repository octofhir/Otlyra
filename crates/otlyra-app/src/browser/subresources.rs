//! What a page asks for once it has been styled: background pictures, pictures
//! chosen again for a new window, and the fonts its sheets bring.
//!
//! These are named by rules or chosen against the window rather than named by the
//! markup, so they are known only on the way to a frame and are asked for after
//! one. The decoded pictures are kept here too, because every picture — whoever
//! asked for it — is decoded once and looked up by its address.

use std::collections::HashMap;

use crate::fetcher::ResourceKind;
use crate::page::PageScene;

use super::Browser;
use super::loading::IMAGE_LIMIT;

/// How many fonts one document may bring with it.
///
/// A page that ships a family ships a handful of faces of it; one that names
/// dozens is asking for a megabyte of typefaces before its first line is set.
const FONT_LIMIT: usize = 16;

/// How many bytes of decoded pictures are kept between loads.
///
/// Decoded, not encoded: a 200 KB photograph is 8 MB of pixels, and it is the
/// pixels this holds. Sixty-four megabytes is a few screenfuls of them.
pub(super) const IMAGE_CACHE_BUDGET: usize = 64 * 1024 * 1024;

/// Decoded pictures, kept by address.
///
/// A page that shows the same picture twice decodes it once, and going back to a
/// page that has been visited does not decode its pictures again. Least recently
/// used goes first, because the page in front of the reader is the one whose
/// pictures are worth keeping.
#[derive(Default)]
pub(super) struct ImageCache {
    /// Oldest use first; the end is the most recently used.
    entries: Vec<(String, otlyra_gfx::peniko::ImageData)>,
    pub(super) bytes: usize,
}

impl ImageCache {
    /// The picture at `url`, if it is here, marked as just used.
    pub(super) fn get(&mut self, url: &str) -> Option<otlyra_gfx::peniko::ImageData> {
        let at = self.entries.iter().position(|(key, _)| key == url)?;
        let entry = self.entries.remove(at);
        let image = entry.1.clone();
        self.entries.push(entry);
        Some(image)
    }

    /// Keep `image` under `url`, evicting the least recently used until it fits.
    pub(super) fn insert(&mut self, url: String, image: otlyra_gfx::peniko::ImageData) {
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

impl Browser {
    /// Ask for the background pictures the pages have found they need.
    ///
    /// A background is named by a rule, so what a page wants is known only once it
    /// has been styled — which happens on the way to a frame. This is called after
    /// one, and the pictures arrive for the frame after that.
    fn fetch_backgrounds(&mut self, tabs: &[usize]) {
        if !self.settings.settings.load_images {
            return;
        }
        for &index in tabs {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            let base = self.tabs[index].url.clone();
            let wanted: Vec<String> = page
                .wanted_pictures()
                .into_iter()
                .filter(|url| !self.background_requests.contains_key(url))
                .take(IMAGE_LIMIT)
                .collect();

            for url in wanted {
                if let Some(picture) = self.images.get(&url) {
                    if let Some(page) = self.tabs[index].page.as_mut() {
                        page.set_picture(url.clone(), picture);
                    }
                    self.background_requests.insert(url, index);
                    continue;
                }
                let Some(target) = Self::subresource_url(&base, &url) else {
                    // Recorded anyway: a picture that may not be fetched must not be
                    // asked for again on every frame.
                    self.background_requests.insert(url, index);
                    continue;
                };
                let id = self.fetcher.request(&target, ResourceKind::Image);
                self.background_requests.insert(url.clone(), index);
                self.background_fetches.insert(id, (index, url));
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
        if !self.settings.settings.load_images {
            return;
        }
        let viewport = self.picture_viewport();
        let window = (viewport.width, viewport.scale);
        if self.picture_window == Some(window) {
            return;
        }
        self.picture_window = Some(window);

        for index in 0..self.tabs.len() {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            let base = self.tabs[index].url.clone();
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
                let Some(target) = Self::subresource_url(&base, &source.src) else {
                    continue;
                };
                // Already decoded: no request, straight into the page.
                if let Some(data) = self.images.get(&target)
                    && let Some(page) = self.tabs[index].page.as_mut()
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
                    .insert(id, (index, source.node, source.src, source.density));
            }
        }
    }

    /// Ask for the fonts the pages' own stylesheets bring with them.
    ///
    /// A `@font-face` rule is only known once the sheet holding it has been parsed,
    /// which is a page's first restyle — so this is asked after a frame rather than
    /// with the pictures the markup names, exactly as a background picture is.
    ///
    /// The address is resolved against the sheet the rule was written in, not
    /// against the page: a sheet in a directory of its own names its fonts beside
    /// itself.
    fn fetch_fonts(&mut self, tabs: &[usize]) {
        for &index in tabs {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            // The faces first: they come from the styler's own rules, and most
            // pages have none. Resolving the sheets is a walk of the whole
            // document, and there is no reason to walk it to place addresses
            // nothing asked for.
            let faces: Vec<otlyra_css::cascade::FontFace> =
                page.wanted_fonts().into_iter().take(FONT_LIMIT).collect();
            if faces.is_empty() {
                continue;
            }
            let base = self.tabs[index].url.clone();
            let sheets: HashMap<otlyra_dom::NodeId, String> =
                otlyra_css::cascade::stylesheet_links(page.document())
                    .into_iter()
                    .filter_map(|link| Some((link.node, Self::subresource_url(&base, &link.href)?)))
                    .collect();

            for face in faces {
                // The first address that resolves, which is as far as the order in
                // the rule is honoured: what the rest of the list is for is formats
                // this cannot read, and there is no telling which those are until
                // the bytes are here.
                let sheet_base = face
                    .sheet
                    .and_then(|node| sheets.get(&node))
                    .unwrap_or(&base);
                let Some(target) = face
                    .sources
                    .iter()
                    .find_map(|source| Self::subresource_url(sheet_base, source))
                else {
                    continue;
                };
                if !self
                    .font_requests
                    .insert((face.family.clone(), target.clone()))
                {
                    continue;
                }
                let id = self.fetcher.request(&target, ResourceKind::Stylesheet);
                self.font_fetches.insert(id, face.family);
            }
        }
    }

    /// The picture and font work that follows a frame, once the rules that name
    /// them have been computed on the way to one.
    pub(super) fn after_frame(&mut self) {
        // Which tabs have anything new to be asked about. A page whose display
        // list was reused cannot want a picture or a font it did not want
        // before: the rules that name them are computed on the way to a list,
        // and no list was built. Asking anyway walks the whole box tree and the
        // whole document, per tab, per frame.
        let sweep: Vec<usize> = (0..self.tabs.len())
            .filter(|&index| {
                let Some(page) = self.tabs[index].page.as_ref() else {
                    return false;
                };
                self.tabs[index].swept_at_build != Some(page.builds())
            })
            .collect();
        if !sweep.is_empty() {
            self.fetch_backgrounds(&sweep);
            self.fetch_fonts(&sweep);
            for index in sweep {
                let built = self.tabs[index].page.as_ref().map(PageScene::builds);
                self.tabs[index].swept_at_build = built;
            }
        }
        // Last, because it is a question about the window this frame was drawn
        // for: the answer is for the next one. It has a guard of its own.
        self.rechoose_pictures();
    }
}
