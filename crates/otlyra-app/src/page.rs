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

use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::{BoxId, BoxTree, Damage, FragmentTree, build_box_tree};
use otlyra_text::TextEngine;

/// A parsed document, laid out and painted.
#[derive(Debug)]
pub struct PageScene {
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
    /// How far down the page the reader is, in logical pixels.
    scroll: f32,
    /// The last frame's content height, so a scroll can be clamped without waiting
    /// for the next one.
    viewport_height: f32,
    /// What the next frame has to redo.
    damage: Damage,
}

impl PageScene {
    /// A scene showing `document`.
    pub fn new(document: Document) -> Self {
        Self {
            boxes: build_box_tree(&document),
            document,
            targets: Vec::new(),
            layout: None,
            scroll: 0.0,
            viewport_height: 0.0,
            damage: Damage::STYLE,
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
        self.damage.take();
        let scroll = self.scroll;
        let fragments = self.fragments(text, width, height);
        let mut list = otlyra_paint::build_display_list(fragments, (width, height), scroll);
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

        list
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

    /// Scroll by `delta` logical pixels, clamped to the content.
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
