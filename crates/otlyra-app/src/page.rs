//! One document on screen: box tree, layout, and where the reader has scrolled to.
//!
//! Everything below the shell is a pure function of the step before it — DOM to box
//! tree to fragment tree to display list — so this type holds only what is not a
//! function of the document: the scroll offset, and the cached results of the steps
//! a scroll does not invalidate.
//!
//! What that buys: scrolling relays out nothing and reshapes nothing; it rebuilds
//! the display list, which is a walk over the fragments that are actually visible.
//! A resize invalidates layout, because layout is a function of the width.

use otlyra_css::cascade::ExternalSheets;
use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::{BoxId, BoxTree, Damage, FragmentTree, Images, build_box_tree};
use otlyra_text::TextEngine;

/// A parsed document, laid out and painted.
pub struct PageScene {
    /// The stylesheets the document's `<link>` elements asked for, already
    /// fetched. Kept because a restyle needs them again and a restyle must not
    /// wait on a network.
    sheets: ExternalSheets,
    /// The pictures its `<img>` elements asked for, already decoded. Kept for the
    /// same reason as the sheets: rebuilding the box tree must not wait on a
    /// network either.
    images: Images,
    /// The document itself, kept because a click resolves to a box, a box to a
    /// node, and a node's attributes are what say where a link goes.
    document: Document,
    boxes: BoxTree,
    /// The last frame's hit-test targets, in paint order, in window coordinates.
    ///
    /// Extracted from the display list rather than kept as a second structure: the
    /// list is what was drawn, so a target taken from it cannot describe a place
    /// nothing was painted.
    targets: Vec<(otlyra_gfx::kurbo::Rect, BoxId)>,
    /// The last layout, and the width it was made at.
    layout: Option<(f32, FragmentTree)>,
    /// The parsed stylesheets and the cascade machinery over them.
    ///
    /// Kept rather than rebuilt, so a resize does not re-parse a page's CSS. Absent
    /// until the first frame, because parsing is not worth doing for a page nobody
    /// has looked at.
    styler: Option<otlyra_css::cascade::Styler>,
    /// Whether the styles the box tree was built from still hold.
    styled: bool,
    /// What the cascade produced, kept past the box tree it built.
    ///
    /// The box tree carries our own `ComputedStyle`, which is the values and not
    /// where they came from. Answering *which rule set this* needs the engine's
    /// own computed values, because the chain of declarations that won hangs off
    /// them — so they are kept rather than dropped once the boxes exist.
    styled_document: Option<otlyra_css::cascade::StyledDocument>,
    /// The reader's default font size, as a multiple of the specification's.
    text_scale: f32,
    /// How far down the page the reader is, in logical pixels.
    scroll: f32,
    /// The scrollbar the pointer is holding, if it is holding one.
    drag: Option<Drag>,
    /// Whether scrollbars are drawn.
    scrollbars: bool,
    /// Pictures behind boxes, by the address the style names.
    ///
    /// A background is named by a rule rather than by the markup, so what a page
    /// wants is only known once it has been styled — which is after it is first
    /// shown. They arrive late and the page is painted again.
    background_pictures: std::collections::HashMap<String, otlyra_gfx::peniko::ImageData>,
    /// How far each scrollable box inside the page has been scrolled.
    ///
    /// Kept here rather than on the fragment tree, which is rebuilt by every
    /// layout: where the reader had got to inside a panel must survive a resize.
    port_scroll: std::collections::HashMap<BoxId, f32>,
    /// The last frame's content height, so a scroll can be clamped without waiting
    /// for the next one.
    viewport_height: f32,
    /// What the next frame has to redo.
    damage: Damage,
    /// The last list built, and what it was built from.
    ///
    /// The page's half of W10, and the thing `Damage` was written for: every
    /// mutation on this type already records at least `PAINT`, and until now
    /// `build_display_list` took that damage and threw it away. It is read now.
    painted: Option<(Painted, DisplayList)>,
    /// How many lists have been built rather than reused.
    builds: u64,
}

/// Everything a page's display list is a function of, besides the document.
///
/// A value key beside the damage rather than the damage alone. Damage is a
/// claim every mutation has to remember to make, and a claim that is forgotten
/// once shows a stale frame with no way to notice; the things most likely to be
/// forgotten — where the reader has scrolled to, inside the page and inside a
/// panel — are cheap to compare outright. The two together fail safe: either the
/// damage or the key catches a change.
#[derive(Clone, Debug, PartialEq)]
struct Painted {
    width: f32,
    height: f32,
    top: f32,
    scroll: f32,
    scrollbars: bool,
    pictures: usize,
    ports: Vec<(BoxId, f32)>,
}

impl std::fmt::Debug for PageScene {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PageScene")
            .field("scroll", &self.scroll)
            .field("builds", &self.builds)
            .finish_non_exhaustive()
    }
}

impl PageScene {
    /// A scene showing `document`, with nothing fetched for it.
    pub fn new(document: Document) -> Self {
        Self::with_resources(document, ExternalSheets::default(), Images::default())
    }

    /// A scene showing `document` with the stylesheets and pictures it asked for.
    pub fn with_resources(document: Document, sheets: ExternalSheets, images: Images) -> Self {
        Self {
            sheets,
            images,
            boxes: build_box_tree(&document),
            document,
            targets: Vec::new(),
            layout: None,
            styler: None,
            styled: false,
            styled_document: None,
            text_scale: 1.0,
            scroll: 0.0,
            port_scroll: std::collections::HashMap::new(),
            drag: None,
            scrollbars: true,
            background_pictures: std::collections::HashMap::new(),
            viewport_height: 0.0,
            damage: Damage::STYLE,
            painted: None,
            builds: 0,
        }
    }

    /// What the next frame has to redo.
    pub fn damage(&self) -> Damage {
        self.damage
    }

    /// The document behind the page.
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Take the document back out, to build the page again with more of what it
    /// asked for — a stylesheet that has since arrived, a picture that has decoded.
    /// Parsing it twice would be the alternative, and the bytes are gone by then.
    pub fn into_document(self) -> Document {
        self.document
    }

    /// The box tree behind the page.
    pub fn boxes(&self) -> &BoxTree {
        &self.boxes
    }

    /// How far down the page the reader is.
    pub fn scroll(&self) -> f32 {
        self.scroll
    }

    /// Lay the page out for `width`, reusing the last layout if the width has not
    /// changed.
    fn fragments(&mut self, text: &mut TextEngine, width: f32, height: f32) -> &FragmentTree {
        self.restyle_if_needed(width, height);

        let stale = !matches!(&self.layout, Some((last, _)) if *last == width);
        if stale {
            self.damage.add(Damage::of(
                otlyra_layout::InvalidationReason::ViewportResized,
            ));
            let tree =
                otlyra_layout::layout(&self.boxes, text, otlyra_layout::Viewport { width, height });
            self.layout = Some((width, tree));
        }
        &self.layout.as_ref().expect("just laid out").1
    }

    /// Run the cascade for a viewport of `width` by `height` if this viewport can
    /// change what it computed.
    ///
    /// Most resizes cannot: without a media query or a viewport unit, every element
    /// keeps the style it had, and the width a box is laid out at is layout's
    /// business rather than the cascade's. Asking is what turns a resize from a
    /// re-parse and a re-cascade of the whole document into a relayout.
    fn restyle_if_needed(&mut self, width: f32, height: f32) {
        let viewport = otlyra_css::cascade::Viewport {
            width,
            height,
            scale: 1.0,
            text_scale: self.text_scale,
        };

        let stale = match self.styler.as_mut() {
            Some(styler) => styler.resize(viewport),
            None => {
                self.styler = Some(otlyra_css::cascade::Styler::new(
                    &self.document,
                    viewport,
                    &self.sheets,
                ));
                true
            }
        };

        if !stale && self.styled {
            return;
        }

        let styles = self
            .styler
            .as_mut()
            .expect("a styler was just made if there was none")
            .style(&self.document);
        self.boxes =
            otlyra_layout::build_box_tree_with_images(&self.document, Some(&styles), &self.images);
        self.styled_document = Some(styles);
        self.styled = true;
        self.layout = None;
        self.damage.add(Damage::of(
            otlyra_layout::InvalidationReason::DocumentLoaded,
        ));
    }

    /// Build the display list for a content area `width` by `height` logical pixels
    /// with its top-left at (0, `top`).
    pub fn build_display_list(
        &mut self,
        text: &mut TextEngine,
        width: f32,
        height: f32,
        top: f32,
    ) -> DisplayList {
        self.viewport_height = height;
        let damage = self.damage.take();

        let mut ports: Vec<(BoxId, f32)> =
            self.port_scroll.iter().map(|(id, at)| (*id, *at)).collect();
        ports.sort_by_key(|(id, _)| otlyra_layout::box_id_to_u64(*id));
        let key = Painted {
            width,
            height,
            top,
            scroll: self.scroll,
            scrollbars: self.scrollbars,
            pictures: self.background_pictures.len(),
            ports,
        };
        // Nothing has been reported changed and nothing it is drawn from has
        // moved, so the last list is this frame's list. The hit-test targets go
        // with it untouched — they were taken from this very list, so a press
        // still meets what is on screen.
        if damage.is_none()
            && let Some((built, list)) = &self.painted
            && *built == key
        {
            return list.clone();
        }

        self.builds += 1;
        let scroll = self.scroll;
        // Taken before the layout is borrowed: the offsets are a handful of floats,
        // and the alternative is holding a borrow of the page across the walk.
        let ports = self.port_scroll.clone();
        let pictures = self.background_pictures.clone();
        let scrollbars = self.scrollbars;
        let fragments = self.fragments(text, width, height);
        let mut list = otlyra_paint::build_display_list_with(
            fragments,
            &otlyra_paint::Frame {
                viewport: (width, height),
                scroll_y: scroll,
                port_offset: Some(&|id| ports.get(&id).copied().unwrap_or(0.0)),
                background: Some(&|url: &str| pictures.get(url).cloned()),
                scrollbars,
            },
        );
        if top != 0.0 {
            list.transform(otlyra_gfx::kurbo::Affine::translate((0.0, f64::from(top))));
        }

        self.targets = list
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::HitTest {
                    rect,
                    transform,
                    id,
                } => Some((
                    transform.transform_rect_bbox(*rect),
                    otlyra_layout::box_id_from_u64(id.0),
                )),
                _ => None,
            })
            .collect();

        // Laying out and cascading are part of building *this* list, and both
        // report damage as they go. Clearing after rather than before is what
        // keeps that from being read as a reason to build the next one again.
        self.damage = Damage::NONE;
        self.painted = Some((key, list.clone()));
        list
    }

    /// How many display lists this page has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// The topmost box at `point`, in window logical coordinates.
    ///
    /// Reads the last frame's targets: a click lands on what the user was looking
    /// at, which is the frame that was on screen, not the one that would be built
    /// now.
    pub fn box_at(&self, x: f64, y: f64) -> Option<BoxId> {
        let point = otlyra_gfx::kurbo::Point::new(x, y);
        self.targets
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, id)| *id)
    }

    /// The `href` of the link at `point`, if there is one.
    ///
    /// Walks up the box tree, because the text inside `<a><b>text</b></a>` belongs
    /// to the `<b>` and the link is two boxes above it.
    pub fn link_at(&self, x: f64, y: f64) -> Option<String> {
        let mut current = self.box_at(x, y);
        while let Some(id) = current {
            let node = self.boxes.get(id)?;
            if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "a")
                && let Some(href) = node.node.and_then(|node| self.attribute(node, "href"))
            {
                return Some(href);
            }
            current = node.parent;
        }
        None
    }

    /// What the reader has asked the default font size to be, as a multiple.
    ///
    /// A restyle when it changes, because it changes what `medium` computes to
    /// and every element that inherited a size inherited that.
    pub fn set_text_scale(&mut self, scale: f32) {
        if (self.text_scale - scale).abs() < f32::EPSILON {
            return;
        }
        self.text_scale = scale;
        self.styled = false;
        self.damage.add(otlyra_layout::Damage::LAYOUT);
    }

    /// Where a box was drawn on the last frame, if it was.
    pub fn rect_of(&self, id: BoxId) -> Option<otlyra_layout::Rect> {
        self.targets
            .iter()
            .find(|(_, target)| *target == id)
            .map(|(rect, _)| {
                otlyra_layout::Rect::new(
                    rect.x0 as f32,
                    rect.y0 as f32,
                    rect.width() as f32,
                    rect.height() as f32,
                )
            })
    }

    /// Which rules set the values on a node, weakest first.
    ///
    /// Empty for a node the cascade was never asked about — a text node, or an
    /// element under `display: none` — which is the same answer the computed
    /// pane gives for one, and for the same reason.
    pub fn rules_for(&self, node: NodeId) -> Vec<otlyra_css::cascade::MatchedRule> {
        let Some(styler) = self.styler.as_ref() else {
            return Vec::new();
        };
        self.styled_document
            .as_ref()
            .and_then(|styled| styled.style_of(node))
            .map(|style| styler.rules_for(style))
            .unwrap_or_default()
    }

    /// The edges layout actually gave a box, if it laid one out.
    ///
    /// The used values. A computed style says `auto` for a margin and only
    /// layout knows what `auto` came out as, so a panel that resolved the
    /// computed style itself would be right about everything except the one
    /// case it was opened to look at.
    pub fn used_edges(&self, id: BoxId) -> Option<otlyra_layout::UsedEdges> {
        self.layout
            .as_ref()?
            .1
            .iter()
            .find(|fragment| fragment.box_id == Some(id))
            .and_then(|fragment| fragment.used)
    }

    /// The `href` of a box, if it is a link with one.
    pub fn href_of(&self, id: BoxId) -> Option<String> {
        let node = self.boxes.get(id)?;
        if node.tag.as_ref().is_none_or(|tag| tag.as_ref() != "a") {
            return None;
        }
        self.attribute(node.node?, "href")
    }

    /// One attribute of an element node.
    fn attribute(&self, node: NodeId, name: &str) -> Option<String> {
        self.document
            .get(node)?
            .element()?
            .attrs
            .iter()
            .find(|attr| attr.name.local.as_ref() == name)
            .map(|attr| attr.value.to_string())
    }

    /// Put the reader back where they were, as a reload does.
    ///
    /// Not clamped here: the new document may be shorter or taller, and the clamp
    /// happens on the next scroll or the next frame, once there is a layout to
    /// clamp against.
    pub fn set_scroll(&mut self, scroll: f32) {
        self.scroll = scroll.max(0.0);
        self.damage.add(Damage::PAINT);
    }

    /// Draw no scrollbars, for a picture that is going to be compared with one
    /// from elsewhere.
    pub fn hide_scrollbars(&mut self) {
        self.scrollbars = false;
    }

    /// The background pictures this page names and has not been given.
    ///
    /// Asked for after a frame, because the styles that name them are computed on
    /// the way to one.
    pub fn wanted_pictures(&self) -> Vec<String> {
        let mut wanted: Vec<String> = Vec::new();
        for id in self.boxes.descendants(self.boxes.root()) {
            let Some(url) = self.boxes.node(id).style.background_image.as_deref() else {
                continue;
            };
            if self.background_pictures.contains_key(url) {
                continue;
            }
            if !wanted.iter().any(|already| already == url) {
                wanted.push(url.to_owned());
            }
        }
        wanted
    }

    /// Hand over a picture the page asked for.
    pub fn set_picture(&mut self, url: String, picture: otlyra_gfx::peniko::ImageData) {
        self.background_pictures.insert(url, picture);
        self.damage.add(Damage::PAINT);
    }

    /// Take hold of a scrollbar under (`x`, `y`), if one is there.
    ///
    /// Returns whether it grabbed anything: a press that lands on a scrollbar
    /// belongs to it and not to the page behind it.
    pub fn grab_scrollbar(&mut self, x: f32, y: f32, width: f32, height: f32) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };

        // The page's own bar first: it is drawn over everything, so it is grabbed
        // before anything under it.
        let page_area = otlyra_layout::fragment::Rect::new(0.0, 0.0, width, height);
        if let Some(thumb) =
            otlyra_paint::scrollbar_thumb(page_area, tree.content_height(), self.scroll)
            && contains(thumb, x, y)
        {
            self.drag = Some(Drag {
                target: None,
                grabbed_at: y - thumb.y,
            });
            return true;
        }

        for port in &tree.scroll_ports {
            let mut area = port.port;
            area.y -= self.scroll;
            let at = self.port_scroll.get(&port.id).copied().unwrap_or(0.0);
            if let Some(thumb) = otlyra_paint::scrollbar_thumb(area, port.content_height, at)
                && contains(thumb, x, y)
            {
                self.drag = Some(Drag {
                    target: Some(port.id),
                    grabbed_at: y - thumb.y,
                });
                return true;
            }
        }

        false
    }

    /// Whether a scrollbar is being dragged.
    pub fn dragging_scrollbar(&self) -> bool {
        self.drag.is_some()
    }

    /// Let go of whatever was grabbed.
    pub fn release_scrollbar(&mut self) {
        self.drag = None;
    }

    /// Drag the grabbed scrollbar to `y`.
    ///
    /// The thumb follows the pointer and the content follows the thumb, which is
    /// the way round that makes a drag feel attached to the hand rather than to the
    /// document.
    pub fn drag_scrollbar(&mut self, y: f32, width: f32, height: f32) {
        let Some(drag) = self.drag else {
            return;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };

        let (area, content, range) = match drag.target {
            None => {
                let area = otlyra_layout::fragment::Rect::new(0.0, 0.0, width, height);
                let content = tree.content_height();
                (area, content, (content - height).max(0.0))
            }
            Some(id) => {
                let Some(port) = tree.scroll_ports.iter().find(|port| port.id == id) else {
                    return;
                };
                let mut area = port.port;
                area.y -= self.scroll;
                (area, port.content_height, port.range())
            }
        };

        let travel = otlyra_paint::scrollbar_travel(area, content);
        if travel <= 0.0 {
            return;
        }
        let wanted = ((y - drag.grabbed_at - area.y) / travel).clamp(0.0, 1.0) * range;

        match drag.target {
            None => self.set_scroll(wanted),
            Some(id) => {
                self.port_scroll.insert(id, wanted);
                self.damage.add(Damage::PAINT);
            }
        }
    }

    /// Scroll whatever is under (`x`, `y`) by `delta` logical pixels.
    ///
    /// A box that cuts its contents off and has more of them than it can show takes
    /// the wheel before the page does, and hands it back once it has reached its
    /// end — which is what makes a scrollable panel inside a page feel right rather
    /// than trapping the reader in it.
    pub fn scroll_at(&mut self, x: f32, y: f32, delta: f32) {
        let page_point = (x, y + self.scroll);
        let port = self.layout.as_ref().and_then(|(_, tree)| {
            // Innermost last: a port inside a port is pushed after it.
            tree.scroll_ports
                .iter()
                .rev()
                .find(|port| {
                    let offset = self.port_scroll.get(&port.id).copied().unwrap_or(0.0);
                    let _ = offset;
                    let rect = port.port;
                    page_point.0 >= rect.x
                        && page_point.0 < rect.right()
                        && page_point.1 >= rect.y
                        && page_point.1 < rect.bottom()
                })
                .copied()
        });

        if let Some(port) = port {
            let at = self.port_scroll.entry(port.id).or_insert(0.0);
            let wanted = *at + delta;
            let clamped = wanted.clamp(0.0, port.range());
            if (clamped - *at).abs() > f32::EPSILON {
                *at = clamped;
                self.damage.add(Damage::PAINT);
                return;
            }
            // At its end: the page takes the rest, rather than the wheel doing
            // nothing at all.
        }

        self.scroll_by(delta);
    }

    /// Scroll the page by `delta` logical pixels, clamped to the content.
    ///
    /// Damages paint and no more: where the content is has not changed, only which
    /// part of it is on screen.
    pub fn scroll_by(&mut self, delta: f32) {
        self.damage.add(Damage::PAINT);
        let content = self
            .layout
            .as_ref()
            .map_or(0.0, |(_, tree)| tree.content_height());
        let max = (content - self.viewport_height).max(0.0);
        self.scroll = (self.scroll + delta).clamp(0.0, max);
    }
}

/// A scrollbar being dragged.
#[derive(Copy, Clone, Debug)]
struct Drag {
    /// Which scroll port's bar, or the page's own.
    target: Option<BoxId>,
    /// Where on the thumb it was taken hold of, so it does not jump to the pointer.
    grabbed_at: f32,
}

/// Whether a rectangle contains a point.
fn contains(rect: otlyra_layout::fragment::Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

/// The document's `<title>`, if it has one.
///
/// Browser interface rather than page content, which is why it is here and not in
/// the box tree: `<title>` is `display: none`, and the tab still has to be named
/// something.
pub fn title_of(document: &Document) -> Option<String> {
    fn find(document: &Document, id: NodeId) -> Option<String> {
        if let Some(element) = document.get(id).and_then(|node| node.element())
            && element.name.local.as_ref() == "title"
        {
            let mut text = String::new();
            for child in document.children(id) {
                if let Some(NodeData::Text(chunk)) = document.get(child).map(|node| &node.data) {
                    text.push_str(chunk);
                }
            }
            let text = text.trim().to_owned();
            return (!text.is_empty()).then_some(text);
        }
        document
            .children(id)
            .find_map(|child| find(document, child))
    }
    find(document, document.root())
}

#[cfg(test)]
mod tests {
    use otlyra_gfx::{DisplayItem, PaintOp, RecordingPainter, render};

    use super::*;

    fn scene(html: &str) -> (PageScene, TextEngine) {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        (PageScene::new(parsed.document), TextEngine::isolated())
    }

    /// W10's page half: an idle page with a still reader does no work. Every
    /// mutation records damage and the frame reads it, which is what `Damage`
    /// was written for and what nothing did until now.
    #[test]
    fn an_unchanged_page_is_not_painted_a_second_time() {
        let (mut page, mut text) = scene("<body><h1>Title</h1><p>Some text to lay out.</p>");
        let first = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 1);

        let again = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 1, "nothing about it moved");
        assert_eq!(first, again, "and the frame is the same frame");

        // Scrolling is a repaint, and a repaint is what it asks for.
        page.set_scroll(40.0);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 2);
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 2, "and then it is still again");

        // A resize is a relayout, and a different band of the window to draw in
        // is a different frame even when nothing else moved.
        page.build_display_list(&mut text, 700.0, 600.0, 0.0);
        assert_eq!(page.builds(), 3);
        page.build_display_list(&mut text, 700.0, 600.0, 12.0);
        assert_eq!(page.builds(), 4, "the page moved down the window");
    }

    /// A press still lands on what is on screen when the frame was reused: the
    /// targets came out of that very list, so they describe it exactly.
    #[test]
    fn a_reused_frame_is_still_the_frame_a_press_is_tested_against() {
        let (mut page, mut text) = scene("<body><p><a href=\"/next\">go</a></p>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let before = page.boxes().root();
        let _ = before;
        let hit = (0..600)
            .step_by(4)
            .find_map(|y| page.box_at(20.0, f64::from(y)));

        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(page.builds(), 1, "the frame was reused");
        assert_eq!(
            hit,
            (0..600)
                .step_by(4)
                .find_map(|y| page.box_at(20.0, f64::from(y))),
            "and it still answers where things are"
        );
    }

    fn glyph_ys(list: &DisplayList) -> Vec<f64> {
        let mut painter = RecordingPainter::new();
        render(list, &mut painter);
        painter
            .take()
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawGlyphs { transform, .. } => Some(transform.as_coeffs()[5]),
                _ => None,
            })
            .collect()
    }

    /// The colour of the first paragraph, which is what a media query in these
    /// tests changes.
    fn paragraph_colour(page: &PageScene) -> otlyra_gfx::peniko::Color {
        let boxes = page.boxes();
        boxes
            .descendants(boxes.root())
            .into_iter()
            .find(|&id| {
                boxes
                    .node(id)
                    .tag
                    .as_ref()
                    .is_some_and(|tag| tag.as_ref() == "p")
            })
            .map(|id| boxes.node(id).style.color)
            .expect("a paragraph")
    }

    /// A resize relays out; it re-cascades only when the viewport is something a
    /// rule reads.
    #[test]
    fn a_resize_restyles_only_when_a_rule_reads_the_viewport() {
        let (mut page, mut text) = scene(
            "<style>@media (min-width: 700px) { p { color: rgb(255, 0, 0) } }</style><p>text",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(
            paragraph_colour(&page),
            otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0)
        );

        page.build_display_list(&mut text, 500.0, 600.0, 0.0);
        assert_ne!(
            paragraph_colour(&page),
            otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0),
            "the query stopped matching and nothing noticed"
        );
    }

    /// A resize with nothing to restyle keeps the styles it had, and lays out
    /// again at the new width — which is the whole point of asking first.
    #[test]
    fn a_resize_nothing_reads_still_relays_out() {
        let (mut page, mut text) = scene("<style>p { color: rgb(0, 128, 0) }</style><p>text</p>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let colour = paragraph_colour(&page);

        page.build_display_list(&mut text, 300.0, 600.0, 0.0);
        assert_eq!(paragraph_colour(&page), colour);
        assert_eq!(
            page.layout.as_ref().expect("a layout").0,
            300.0,
            "laid out at the new width"
        );
    }

    /// A scrollbar can be taken hold of and dragged, and the content follows the
    /// thumb rather than the other way round.
    #[test]
    fn dragging_a_scrollbar_scrolls_the_page() {
        let (mut page, mut text) =
            scene("<style>body { margin: 0 } p { height: 3000px }</style><p>tall</p>");
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // Nowhere near the bar: the press is not for it.
        assert!(!page.grab_scrollbar(400.0, 300.0, 800.0, 600.0));

        // On the thumb, which sits at the top of a page that has not been scrolled.
        assert!(page.grab_scrollbar(795.0, 10.0, 800.0, 600.0));
        assert!(page.dragging_scrollbar());

        page.drag_scrollbar(300.0, 800.0, 600.0);
        let halfway = page.scroll();
        assert!(halfway > 0.0, "the drag did not move the page");

        page.drag_scrollbar(600.0, 800.0, 600.0);
        assert!(
            page.scroll() > halfway,
            "further down did not scroll further"
        );

        page.release_scrollbar();
        page.drag_scrollbar(0.0, 800.0, 600.0);
        assert!(page.scroll() > halfway, "it moved after being let go");
    }

    /// A scrolled panel's contents stay inside it. The regression this pins: the
    /// clip was decided by whether the contents fitted where the flow put them,
    /// which stopped being true the moment the panel scrolled — and the contents
    /// were then drawn over everything around the panel instead of under its edge.
    #[test]
    fn a_scrolled_panel_clips_what_it_has_moved() {
        let (mut page, mut text) = scene(
            "<style>body { margin: 0 } \
             .panel { overflow: hidden; height: 100px } \
             .item { height: 60px }</style>\
             <div class=panel><div class=item>a</div><div class=item>b</div>\
             <div class=item>c</div></div>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        page.scroll_at(50.0, 50.0, 80.0);

        let list = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);

        // Everything drawn while a layer is open is inside it; the panel's own
        // rectangle is what that layer is.
        let mut depth = 0i32;
        let mut clipped_glyphs = 0;
        let mut loose_glyphs = 0;
        for op in painter.take() {
            match op {
                PaintOp::PushLayer { .. } => depth += 1,
                PaintOp::PopLayer => depth -= 1,
                PaintOp::DrawGlyphs { transform, .. } => {
                    let y = transform.as_coeffs()[5];
                    // The panel is the first hundred pixels of the page.
                    if !(0.0..=100.0).contains(&y) {
                        if depth > 0 {
                            clipped_glyphs += 1;
                        } else {
                            loose_glyphs += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        assert!(
            clipped_glyphs > 0,
            "the scroll moved nothing out of the panel, so this proves nothing"
        );
        assert_eq!(
            loose_glyphs, 0,
            "text scrolled out of the panel was drawn outside it"
        );
    }

    /// What a scrolled panel actually draws: the contents move, the box does not.
    #[test]
    fn scrolling_a_panel_moves_its_contents_and_not_its_edge() {
        let (mut page, mut text) = scene(
            "<style>body { margin: 0 } \
             .panel { overflow: hidden; height: 100px; background: rgb(0, 0, 255) } \
             .tall { height: 400px; background: rgb(255, 0, 0) }</style>\
             <div class=panel><div class=tall>inside</div></div>",
        );

        let tops = |page: &mut PageScene, text: &mut TextEngine| {
            let list = page.build_display_list(text, 800.0, 600.0, 0.0);
            let mut painter = RecordingPainter::new();
            render(&list, &mut painter);
            let mut panel = None;
            let mut inside = None;
            use otlyra_gfx::kurbo::Shape as _;
            for op in painter.take() {
                if let PaintOp::Fill { brush, shape, .. } = op {
                    if brush
                        == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                            0, 0, 255,
                        ))
                    {
                        panel = Some(shape.bounding_box().y0);
                    }
                    if brush
                        == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                            255, 0, 0,
                        ))
                    {
                        inside = Some(shape.bounding_box().y0);
                    }
                }
            }
            (panel.expect("the panel"), inside.expect("its contents"))
        };

        let (panel_before, inside_before) = tops(&mut page, &mut text);
        page.scroll_at(50.0, 50.0, 60.0);
        let (panel_after, inside_after) = tops(&mut page, &mut text);

        assert_eq!(panel_before, panel_after, "the box itself moved");
        assert_eq!(
            inside_before - inside_after,
            60.0,
            "its contents did not move by what the wheel said"
        );
    }

    /// A box that cuts its contents off and has more than it can show takes the
    /// wheel; the page takes it once that box has reached its end.
    #[test]
    fn a_scrollable_box_takes_the_wheel_before_the_page_does() {
        let (mut page, mut text) = scene(
            "<style>body { margin: 0 } \
             .panel { overflow: hidden; height: 100px } \
             .tall { height: 400px } \
             .after { height: 2000px }</style>\
             <div class=panel><div class=tall>inside</div></div>\
             <div class=after>after</div>",
        );
        page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // Over the panel: the panel scrolls and the page does not.
        page.scroll_at(50.0, 50.0, 60.0);
        assert_eq!(page.scroll(), 0.0, "the page moved instead of the panel");

        // Past the panel's end, the rest goes to the page.
        page.scroll_at(50.0, 50.0, 1000.0);
        page.scroll_at(50.0, 50.0, 40.0);
        assert!(page.scroll() > 0.0, "the panel kept the wheel to itself");

        // Below the panel, the page scrolls from the first turn.
        let was = page.scroll();
        page.scroll_at(50.0, 400.0, 30.0);
        assert!(page.scroll() > was);
    }

    #[test]
    fn the_title_names_the_tab_and_is_not_page_content() {
        let parsed = otlyra_html::parse(b"<title>A page</title><p>text", Some("utf-8"));
        assert_eq!(title_of(&parsed.document).as_deref(), Some("A page"));
    }

    #[test]
    fn a_document_reaches_the_paint_seam_as_glyphs() {
        let (mut scene, mut text) = scene("<body><h1>heading</h1><p>paragraph");
        let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(glyph_ys(&list).len(), 2, "the heading and the paragraph");
    }

    #[test]
    fn the_top_inset_moves_the_page_below_the_interface() {
        let (mut scene, mut text) = scene("<body><p>text");
        let flush = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));
        let inset = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 72.0));
        assert!((inset[0] - flush[0] - 72.0).abs() < 0.01);
    }

    #[test]
    fn scrolling_moves_the_page_up_and_is_clamped_to_the_content() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
        let (mut scene, mut text) = scene(&html);
        let before = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));

        scene.scroll_by(12.0);
        let after = glyph_ys(&scene.build_display_list(&mut text, 800.0, 600.0, 0.0));
        assert!((before[0] - after[0] - 12.0).abs() < 0.01);

        scene.scroll_by(-1000.0);
        assert_eq!(scene.scroll(), 0.0);
    }

    #[test]
    fn a_page_shorter_than_the_window_cannot_scroll() {
        let (mut scene, mut text) = scene("<body><p>short");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        scene.scroll_by(500.0);
        assert_eq!(scene.scroll(), 0.0);
    }

    /// Scrolling must not relay out: layout is a function of the width, and the
    /// width has not changed.
    #[test]
    fn scrolling_reuses_the_layout_and_resizing_does_not() {
        let (mut scene, mut text) = scene("<body><p>text");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        scene.scroll_by(5.0);
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(scene.layout.as_ref().expect("laid out").0, 800.0);

        let _ = scene.build_display_list(&mut text, 400.0, 600.0, 0.0);
        assert_eq!(scene.layout.as_ref().expect("laid out").0, 400.0);
    }

    /// The assertion that keeps clicking honest: the link's target is the
    /// rectangle its text was drawn in, and nothing else on the page is.
    #[test]
    fn a_point_on_a_link_resolves_to_its_href() {
        let (mut scene, mut text) =
            scene("<body><p>before <a href=\"/next\">the link</a> after</p>");
        let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // Find where the link's own run was drawn, from the display list itself.
        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);
        let ops = painter.take();
        let blue = ops
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawGlyphs {
                    brush, transform, ..
                } if *brush
                    == otlyra_gfx::peniko::Brush::Solid(otlyra_gfx::peniko::Color::from_rgb8(
                        0, 0, 0xee,
                    )) =>
                {
                    Some(transform.as_coeffs())
                }
                _ => None,
            })
            .next()
            .expect("the link is painted in the UA blue");

        let (x, y) = (blue[4] + 4.0, blue[5] + 6.0);
        assert_eq!(scene.link_at(x, y).as_deref(), Some("/next"));
        assert_eq!(scene.link_at(x, y + 400.0), None, "below the text");
        assert_eq!(scene.link_at(2.0, y), None, "before the link starts");
    }

    #[test]
    fn a_link_around_other_elements_is_still_a_link() {
        let (mut scene, mut text) = scene("<body><p><a href=\"/x\"><b>bold link</b></a>");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);

        // The narrowest target is the text run itself; the wide ones are the
        // blocks it sits inside.
        let hit = scene
            .targets
            .iter()
            .map(|(rect, _)| *rect)
            .min_by(|a, b| a.width().total_cmp(&b.width()))
            .expect("something was drawn");
        assert_eq!(
            scene.link_at(hit.x0 + 2.0, hit.y0 + 2.0).as_deref(),
            Some("/x"),
            "the text belongs to the <b>, and the link is above it"
        );
    }

    #[test]
    fn an_anchor_without_an_href_is_not_a_link() {
        let (mut scene, mut text) = scene("<body><p><a>not a link</a>");
        let _ = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert_eq!(scene.link_at(10.0, 20.0), None);
    }

    #[test]
    fn an_empty_page_still_paints_its_canvas() {
        let (mut scene, mut text) = scene("");
        let list = scene.build_display_list(&mut text, 800.0, 600.0, 0.0);
        assert!(matches!(
            list.items().first(),
            Some(DisplayItem::Fill { .. })
        ));
    }
}

/// Rendering tests: what the pipeline puts on a surface, read back as pixels.
///
/// A dump says what style an element computed; only pixels say whether the
/// difference reached the screen.
#[cfg(test)]
mod raster_tests {
    use super::*;

    /// How many non-white pixels each row of the rendered page has.
    fn ink_per_row(html: &str, width: u32, height: u32) -> Vec<u32> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let mut page = PageScene::new(document);
        // System fonts, not the vendored one: the vendored family has a single
        // static face, and a weight that no face can express is a weight no
        // rendering bug can be seen in.
        let mut text = otlyra_text::TextEngine::new();
        let list = page.build_display_list(&mut text, width as f32, height as f32, 0.0);

        let mut painter =
            otlyra_gfx::SkiaPainter::new_raster(width, height).expect("a raster surface");
        painter.clear(otlyra_gfx::peniko::Color::WHITE);
        otlyra_gfx::render(&list, &mut painter);
        let pixels = painter.read_rgba8().expect("read back");

        (0..height)
            .map(|y| {
                (0..width)
                    .filter(|x| {
                        let i = ((y * width + x) * 4) as usize;
                        pixels[i] < 200
                    })
                    .count() as u32
            })
            .collect()
    }

    /// Bold and regular of a variable font share one font file, so anything that
    /// caches by file alone hands the second run the first run's weight. The only
    /// way to see that is to draw both and count the ink.
    #[test]
    fn bold_text_is_heavier_than_regular_text_in_the_same_frame() {
        let ink = ink_per_row(
            "<style>p { font-size: 40px; margin: 0 } .b { font-weight: 700 }</style>\
             <p>iiiiiiii</p><p class=\"b\">iiiiiiii</p>",
            400,
            120,
        );

        let half = ink.len() / 2;
        let regular: u32 = ink[..half].iter().sum();
        let bold: u32 = ink[half..].iter().sum();
        assert!(regular > 0, "the regular line drew nothing");
        assert!(
            bold > regular,
            "bold inked {bold} pixels and regular {regular}"
        );
    }
}
