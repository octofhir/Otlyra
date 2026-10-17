//! Frames: an `<iframe>` is a document of its own (HTML §4.8.5).
//!
//! Each frame is a slot in this world like a tab's, under a name no tab has,
//! so it is loaded, styled, scripted and given its subresources by the same
//! machinery: its own fetch lane, its own isolate, its own timers. What makes
//! it a frame rather than a tab is only this module's record of it — which
//! page and which element it belongs to, and how big that element's content
//! box is — and that it is drawn into its element instead of into the window.
//!
//! It is drawn the way a canvas is: its display list, built at the size of
//! the element's content box, is rasterized and handed to the parent page as
//! the element's picture. A frame redraws when its own page changed, at most
//! every [`REDRAW_INTERVAL`], because every redraw lays the parent out again.
//!
//! # Contents
//! - [`PageWorld::sync_frames`] — after a frame: start, size, draw and drop
//!   frames to match the pages' `<iframe>` elements.
//! - [`PageWorld::frame_viewport`] — the viewport a frame's script and layout
//!   see.
//! - [`PageWorld::top_of`] — the tab a frame is shown in.
//!
//! # Invariants
//! - A frame belongs to one element of one document: a parent that navigates
//!   or an element that goes, or whose source changes, closes the frame and
//!   everything nested in it.
//! - At most [`FRAME_LIMIT`] frames per document and [`DEPTH_LIMIT`] levels.
//! - A frame's events never reach the window: its id is in no tab strip, and
//!   the window drops what it cannot place.
//!
//! # Not yet
//! - Input: clicks, keys and wheel inside a frame go to its element in the
//!   parent. Its own scrolling with them.
//! - `contentWindow`, `contentDocument`, `window.frames`, `postMessage`.
//! - `loading=lazy`, `sandbox`, navigation of a frame by its own script.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use otlyra_dom::{Document, NodeId};

use super::PageWorld;
use crate::browser::TabId;

/// How many frames one document may have. Pages carry a handful; past this a
/// page is an ad farm, and every frame is an isolate.
pub(super) const FRAME_LIMIT: usize = 16;

/// How deep frames may nest inside a tab.
pub(super) const DEPTH_LIMIT: usize = 3;

/// The size of a frame whose element has not been laid out: an `<iframe>`'s
/// default (HTML §15.4.3).
const DEFAULT_SIZE: (f32, f32) = (300.0, 150.0);

/// How soon a frame is drawn again after it was. Each drawing replaces the
/// element's picture, which lays the parent out again.
const REDRAW_INTERVAL: Duration = Duration::from_millis(200);

/// One frame: whose it is and what it shows.
#[derive(Debug)]
pub(super) struct ChildFrame {
    /// The page the frame's element is in.
    parent: TabId,
    /// Which document of that page: a new document is new elements.
    parent_generation: u64,
    /// The `<iframe>` element.
    element: NodeId,
    /// The address its document was loaded from.
    source: String,
    /// The element's content box, in the parent's logical pixels.
    size: (f32, f32),
    /// The frame's page as it was last drawn: its build and scroll counts and
    /// the size it was drawn at.
    drawn: Option<(u64, u64, (f32, f32))>,
    /// When it was last drawn.
    drawn_at: Option<Instant>,
}

/// What a page's `<iframe>` elements ask for.
struct Wanted {
    parent: TabId,
    parent_generation: u64,
    element: NodeId,
    source: String,
    size: (f32, f32),
}

impl ChildFrame {
    /// The page the frame's element is in.
    pub(super) fn parent(&self) -> TabId {
        self.parent
    }
}

impl PageWorld {
    /// The tab `tab` is shown in: itself, or the tab its frame is nested in.
    pub(super) fn top_of(&self, mut tab: TabId) -> TabId {
        while let Some(frame) = self.frames.get(&tab) {
            tab = frame.parent;
        }
        tab
    }

    /// How many frames `tab` is nested in.
    fn depth_of(&self, mut tab: TabId) -> usize {
        let mut depth = 0;
        while let Some(frame) = self.frames.get(&tab) {
            tab = frame.parent;
            depth += 1;
        }
        depth
    }

    /// The frames inside `tab`, at any depth.
    pub(super) fn frames_in(&self, tab: TabId) -> Vec<TabId> {
        self.frames
            .keys()
            .copied()
            .filter(|frame| *frame != tab && self.top_of(*frame) == tab)
            .collect()
    }

    /// Every frame, with the size its element gave it.
    #[cfg(test)]
    pub(in crate::browser) fn frame_sizes(&self) -> Vec<(TabId, (f32, f32))> {
        self.frames
            .iter()
            .map(|(id, frame)| (*id, frame.size))
            .collect()
    }

    /// The address of the page a frame is in, which is who caused its load.
    pub(super) fn frame_parent_url(&self, tab: TabId) -> Option<String> {
        let frame = self.frames.get(&tab)?;
        let page = self.slots.get(&frame.parent)?.page.as_ref()?;
        Some(page.url().to_string())
    }

    /// Why a document may not be shown in the frame `tab`, if it may not: its
    /// response says, with CSP's `frame-ancestors` or `X-Frame-Options`, that
    /// the pages around the frame are not ones it agreed to be shown in. That
    /// is what stops a page from putting a sign-in or a payment form under a
    /// reader's click in a frame of its own (clickjacking). `None` for a tab.
    pub(super) fn frame_refused(
        &self,
        tab: TabId,
        loaded: &crate::fetcher::Loaded,
    ) -> Option<String> {
        if !self.frames.contains_key(&tab) {
            return None;
        }
        let own = url::Url::parse(&loaded.final_url).ok()?.origin();
        // Every page the frame is inside, nearest first: a frame is shown in
        // all of them, and each must be one the document agreed to.
        let mut ancestors = Vec::new();
        let mut current = tab;
        while let Some(frame) = self.frames.get(&current) {
            let page = self.slots.get(&frame.parent)?.page.as_ref()?;
            ancestors.push(page.url().origin());
            current = frame.parent;
        }
        refusal(&loaded.response_headers, &own, &ancestors)
    }

    /// The viewport a frame's script and layout see: its element's content
    /// box, at the density of the window it is shown in. `None` for a tab.
    pub(super) fn frame_viewport(&self, tab: TabId) -> Option<otlyra_css::cascade::Viewport> {
        let frame = self.frames.get(&tab)?;
        let mut viewport = self.environment.viewport;
        viewport.width = frame.size.0;
        viewport.height = frame.size.1;
        Some(viewport)
    }

    /// Bring the frames in line with the pages' `<iframe>` elements, and draw
    /// those whose page changed into their elements.
    ///
    /// After a frame, because an element's size is what layout gave it.
    pub(super) fn sync_frames(&mut self) {
        let wanted = self.wanted_frames();

        // Frames whose element, document or source went.
        let gone: Vec<TabId> = self
            .frames
            .iter()
            .filter(|(_, frame)| {
                !self.slots.contains_key(&frame.parent)
                    || !wanted.iter().any(|want| {
                        want.parent == frame.parent
                            && want.parent_generation == frame.parent_generation
                            && want.element == frame.element
                            && want.source == frame.source
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.close_frame(id);
        }

        for want in wanted {
            let existing = self
                .frames
                .iter()
                .find(|(_, frame)| {
                    frame.parent == want.parent
                        && frame.parent_generation == want.parent_generation
                        && frame.element == want.element
                })
                .map(|(id, _)| *id);
            match existing {
                Some(id) => self.resize_frame(id, want.size),
                None => self.open_frame(want),
            }
        }

        let ids: Vec<TabId> = self.frames.keys().copied().collect();
        for id in ids {
            self.draw_frame(id);
        }
    }

    /// What the pages' `<iframe>` elements ask for now.
    fn wanted_frames(&self) -> Vec<Wanted> {
        let mut wanted = Vec::new();
        for (tab, slot) in &self.slots {
            if self.depth_of(*tab) >= DEPTH_LIMIT {
                continue;
            }
            let Some(page) = slot.page.as_ref() else {
                continue;
            };
            let document = page.document();
            for element in iframes(document).into_iter().take(FRAME_LIMIT) {
                let Some(source) = frame_source(document, element, page.base_url()) else {
                    continue;
                };
                wanted.push(Wanted {
                    parent: *tab,
                    parent_generation: slot.generation,
                    element,
                    source,
                    size: page.content_size_of(element).unwrap_or(DEFAULT_SIZE),
                });
            }
        }
        wanted
    }

    fn open_frame(&mut self, want: Wanted) {
        let id = crate::browser::tabs::next_tab_id();
        let source = want.source.clone();
        tracing::debug!(target: "page.frames", %source, parent = want.parent.0, "a frame opens");
        self.frames.insert(
            id,
            ChildFrame {
                parent: want.parent,
                parent_generation: want.parent_generation,
                element: want.element,
                source: want.source,
                size: want.size,
                drawn: None,
                drawn_at: None,
            },
        );
        self.start(
            id,
            &source,
            None,
            otlyra_net::CacheMode::Default,
            String::new(),
            false,
            0.0,
        );
    }

    /// A frame and the frames inside it go, and its element shows nothing.
    pub(super) fn close_frame(&mut self, id: TabId) {
        if let Some(frame) = self.frames.remove(&id) {
            if let Some(page) = self
                .slots
                .get_mut(&frame.parent)
                .filter(|slot| slot.generation == frame.parent_generation)
                .and_then(|slot| slot.page.as_mut())
            {
                page.forget_image(frame.element);
            }
        }
        self.close(id);
    }

    /// The element was laid out at a new size: the frame's page is, too.
    fn resize_frame(&mut self, id: TabId, size: (f32, f32)) {
        let Some(frame) = self.frames.get_mut(&id) else {
            return;
        };
        if frame.size == size {
            return;
        }
        frame.size = size;
        let Some(viewport) = self.frame_viewport(id) else {
            return;
        };
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        let Some(page) = slot.page.as_mut() else {
            return;
        };
        let mutated = if let Some(scripts) = slot.scripts.as_mut() {
            page.with_document(|document| {
                scripts.set_viewport(
                    document,
                    f64::from(viewport.width),
                    f64::from(viewport.height),
                    f64::from(viewport.scale),
                );
            });
            scripts.take_mutated()
        } else if let Some(pending) = slot.pending.as_mut() {
            page.with_document(|document| pending.set_viewport(document, viewport))
        } else {
            false
        };
        if mutated {
            page.document_changed();
        }
    }

    /// Draw a frame's page into its element, if it changed since it was last
    /// drawn and it is not too soon.
    fn draw_frame(&mut self, id: TabId) {
        let Some(frame) = self.frames.get(&id) else {
            return;
        };
        let (parent, generation, element, size) = (
            frame.parent,
            frame.parent_generation,
            frame.element,
            frame.size,
        );
        let drawn = frame.drawn;
        if frame
            .drawn_at
            .is_some_and(|at| at.elapsed() < REDRAW_INTERVAL)
        {
            return;
        }
        let scale = self.environment.viewport.scale;
        let Some((page, text)) = self.page_and_text(id) else {
            return;
        };
        let (width, height) = (size.0.max(1.0), size.1.max(1.0));
        let list = page.build_display_list(text, width, height, 0.0);
        let state = (page.builds(), page.scrolls(), size);
        if drawn == Some(state) {
            return;
        }
        let Some(picture) = rasterize(&list, width, height, scale) else {
            return;
        };
        if let Some(frame) = self.frames.get_mut(&id) {
            frame.drawn = Some(state);
            frame.drawn_at = Some(Instant::now());
        }
        let source = self
            .frames
            .get(&id)
            .map(|frame| frame.source.clone())
            .unwrap_or_default();
        if let Some(page) = self
            .slots
            .get_mut(&parent)
            .filter(|slot| slot.generation == generation)
            .and_then(|slot| slot.page.as_mut())
        {
            page.set_image(element, source, picture);
        }
    }
}

/// Whether headers refuse a document at `own` to frames inside `ancestors`.
///
/// CSP 3 `frame-ancestors`, from every policy that has it; where none does,
/// `X-Frame-Options` (HTML §7.1.6), whose `DENY` refuses every frame and
/// `SAMEORIGIN` every frame with an ancestor of another origin.
fn refusal(
    headers: &[(String, String)],
    own: &url::Origin,
    ancestors: &[url::Origin],
) -> Option<String> {
    let named = |name: &'static str| {
        headers
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let mut governed = false;
    for policy in named("content-security-policy").flat_map(|value| value.split(',')) {
        for directive in policy.split(';') {
            let mut words = directive.split_ascii_whitespace();
            if !words
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("frame-ancestors"))
            {
                continue;
            }
            governed = true;
            let sources: Vec<&str> = words.collect();
            if let Some(ancestor) = ancestors
                .iter()
                .find(|ancestor| !sources.iter().any(|source| allows(source, ancestor, own)))
            {
                return Some(format!(
                    "frame-ancestors does not allow {}",
                    ancestor.ascii_serialization()
                ));
            }
        }
    }
    if governed {
        return None;
    }
    for value in named("x-frame-options").flat_map(|value| value.split(',')) {
        match value.trim().to_ascii_lowercase().as_str() {
            "deny" => return Some("X-Frame-Options: DENY".to_owned()),
            "sameorigin" if ancestors.iter().any(|ancestor| ancestor != own) => {
                return Some("X-Frame-Options: SAMEORIGIN".to_owned());
            }
            _ => {}
        }
    }
    None
}

/// Whether one `frame-ancestors` source matches an ancestor's origin (CSP 3
/// §6.7.2): `'none'` matches nothing, `'self'` the document's own origin, `*`
/// any network origin, `https:` a scheme, and a host with an optional scheme,
/// a `*.` wildcard and a port.
fn allows(source: &str, ancestor: &url::Origin, own: &url::Origin) -> bool {
    let url::Origin::Tuple(scheme, host, port) = ancestor else {
        return false;
    };
    let host = host.to_string();
    match source.to_ascii_lowercase().as_str() {
        "'none'" => false,
        "'self'" => ancestor == own,
        "*" => matches!(scheme.as_str(), "http" | "https"),
        source if source.ends_with(':') && !source.contains('/') => {
            let wanted = &source[..source.len() - 1];
            scheme == wanted || (wanted == "http" && scheme == "https")
        }
        source => {
            let (wanted_scheme, rest) = match source.split_once("://") {
                Some((wanted, rest)) => (Some(wanted), rest),
                None => (None, source),
            };
            let scheme_ok = match wanted_scheme {
                Some(wanted) => scheme == wanted || (wanted == "http" && scheme == "https"),
                None => matches!(scheme.as_str(), "http" | "https"),
            };
            let authority = rest.split('/').next().unwrap_or_default();
            let (wanted_host, wanted_port) = match authority.rsplit_once(':') {
                Some((wanted_host, wanted_port)) if !wanted_host.ends_with(']') => {
                    (wanted_host, Some(wanted_port))
                }
                _ => (authority, None),
            };
            let host_ok = match wanted_host.strip_prefix("*.") {
                Some(suffix) => host.ends_with(&format!(".{suffix}")),
                None => host == wanted_host,
            };
            let port_ok = match wanted_port {
                Some("*") => true,
                Some(wanted_port) => wanted_port
                    .parse::<u16>()
                    .is_ok_and(|wanted| wanted == *port),
                None => *port == if scheme == "https" { 443 } else { 80 },
            };
            scheme_ok && host_ok && port_ok
        }
    }
}

/// The `<iframe>` elements in the page's own tree, in tree order.
fn iframes(document: &Document) -> Vec<NodeId> {
    let mut found = Vec::new();
    let mut stack = vec![document.root()];
    while let Some(node) = stack.pop() {
        if document
            .get(node)
            .and_then(|dom| dom.element())
            .is_some_and(|element| {
                &*element.name.ns == "http://www.w3.org/1999/xhtml"
                    && &*element.name.local == "iframe"
            })
        {
            found.push(node);
            // What is inside an `<iframe>` element is never its document.
            continue;
        }
        let children: Vec<NodeId> = document.children(node).collect();
        stack.extend(children.into_iter().rev());
    }
    found
}

/// The address an `<iframe>` loads: its `srcdoc` as a document of its own, or
/// its `src` resolved against the page. `None` for one that shows nothing —
/// no source, `about:blank`, or a scheme a frame may not load.
fn frame_source(document: &Document, element: NodeId, base: &url::Url) -> Option<String> {
    if let Some(markup) = document.attr(element, "srcdoc") {
        return Some(srcdoc_url(markup));
    }
    let src = document.attr(element, "src")?.trim();
    if src.is_empty() {
        return None;
    }
    let url = base.join(src).ok()?;
    matches!(url.scheme(), "http" | "https" | "data").then(|| url.to_string())
}

/// A `srcdoc` as an address the loader reads without a request.
fn srcdoc_url(markup: &str) -> String {
    let mut url = String::from("data:text/html;charset=utf-8,");
    for byte in markup.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// A frame's display list as the picture its element shows, drawn at the
/// window's density.
fn rasterize(
    list: &otlyra_gfx::DisplayList,
    width: f32,
    height: f32,
    scale: f32,
) -> Option<otlyra_layout::Picture> {
    let pixels = |logical: f32| ((logical * scale).ceil() as u32).clamp(1, 8192);
    let (device_width, device_height) = (pixels(width), pixels(height));
    let mut target = otlyra_gfx::SkiaPainter::new_raster(device_width, device_height)
        .inspect_err(
            |error| tracing::warn!(target: "page.frames", %error, "a frame could not be drawn"),
        )
        .ok()?;
    target.clear(otlyra_gfx::peniko::Color::TRANSPARENT);
    let mut scaled = list.clone();
    scaled.transform(otlyra_gfx::kurbo::Affine::scale(f64::from(scale)));
    otlyra_gfx::render(&scaled, &mut target);
    let rgba = target.read_rgba8().ok()?;
    let data = otlyra_gfx::peniko::ImageData {
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(rgba)),
        format: otlyra_gfx::peniko::ImageFormat::Rgba8,
        alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
        width: device_width,
        height: device_height,
    };
    Some(otlyra_layout::Picture {
        data,
        density: scale,
    })
}

/// Frames by id, kept on the world.
pub(super) type Frames = HashMap<TabId, ChildFrame>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_srcdoc_is_an_address_the_loader_reads() {
        assert_eq!(
            srcdoc_url("<p>a & b</p>"),
            "data:text/html;charset=utf-8,%3Cp%3Ea%20%26%20b%3C%2Fp%3E"
        );
    }

    fn origin(url: &str) -> url::Origin {
        url::Url::parse(url).unwrap().origin()
    }

    fn header(name: &str, value: &str) -> Vec<(String, String)> {
        vec![(name.to_owned(), value.to_owned())]
    }

    #[test]
    fn a_document_says_which_pages_may_frame_it() {
        let own = origin("https://bank.example/");
        let same = [origin("https://bank.example/home")];
        let other = [origin("https://evil.example/")];
        let nested = [
            origin("https://bank.example/"),
            origin("https://evil.example/"),
        ];

        let deny = header("X-Frame-Options", "DENY");
        assert!(refusal(&deny, &own, &same).is_some());
        let sameorigin = header("x-frame-options", " sameorigin ");
        assert!(refusal(&sameorigin, &own, &same).is_none());
        assert!(refusal(&sameorigin, &own, &other).is_some());
        assert!(
            refusal(&sameorigin, &own, &nested).is_some(),
            "every ancestor"
        );
        assert!(
            refusal(
                &header("x-frame-options", "ALLOW-FROM https://x"),
                &own,
                &other
            )
            .is_none()
        );

        let csp = |value: &str| header("content-security-policy", value);
        assert!(refusal(&csp("frame-ancestors 'none'"), &own, &same).is_some());
        assert!(
            refusal(
                &csp("default-src 'self'; frame-ancestors 'self'"),
                &own,
                &same
            )
            .is_none()
        );
        assert!(refusal(&csp("frame-ancestors 'self'"), &own, &other).is_some());
        assert!(
            refusal(
                &csp("frame-ancestors *.evil.example"),
                &own,
                &[origin("https://a.evil.example/")]
            )
            .is_none()
        );
        assert!(refusal(&csp("frame-ancestors https://evil.example"), &own, &other).is_none());
        assert!(refusal(&csp("frame-ancestors https:"), &own, &other).is_none());
        assert!(
            refusal(
                &csp("frame-ancestors https://evil.example:8443"),
                &own,
                &other
            )
            .is_some()
        );
        // Where frame-ancestors speaks, X-Frame-Options is not consulted.
        let both = vec![
            ("x-frame-options".to_owned(), "DENY".to_owned()),
            (
                "content-security-policy".to_owned(),
                "frame-ancestors *".to_owned(),
            ),
        ];
        assert!(refusal(&both, &own, &other).is_none());
        assert!(refusal(&[], &own, &other).is_none());
    }

    #[test]
    fn a_frame_loads_only_what_a_frame_may() {
        let document = otlyra_html::parse(
            b"<iframe id=a src=/x></iframe><iframe id=b></iframe><iframe id=c src='javascript:1'></iframe>\
              <iframe id=d srcdoc='<b>hi</b>' src=/ignored></iframe><iframe id=e src=about:blank></iframe>",
            Some("utf-8"),
        )
        .document;
        let base = url::Url::parse("https://example.com/dir/page").unwrap();
        let sources: Vec<Option<String>> = iframes(&document)
            .into_iter()
            .map(|element| frame_source(&document, element, &base))
            .collect();
        assert_eq!(
            sources,
            [
                Some("https://example.com/x".to_owned()),
                None,
                None,
                Some(srcdoc_url("<b>hi</b>")),
                None,
            ]
        );
    }
}
