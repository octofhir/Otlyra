//! A document on screen: the pipeline, driven by the window.
//!
//! Everything below the shell is a pure function of the one before it — DOM to box
//! tree to fragment tree to display list — so this type is only three things: the
//! document, where the reader has scrolled to, and the cached results of the steps
//! that a scroll does not invalidate.
//!
//! What that buys: scrolling relays out nothing and reshapes nothing; it rebuilds
//! the display list, which is a walk over the fragments that are actually visible.
//! A resize invalidates layout, because layout is a function of the width.

use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::{DisplayList, PaintTarget, render};
use otlyra_layout::{BoxTree, FragmentTree, build_box_tree};
use otlyra_platform::{Painter, PlatformEvent, Viewport};
use otlyra_text::TextEngine;

/// A parsed document, laid out and painted into a window.
pub struct PageScene {
    text: TextEngine,
    boxes: BoxTree,
    /// The last layout, and the width it was made at. `None` until the first frame.
    layout: Option<(f32, FragmentTree)>,
    /// How far down the page the reader is, in logical pixels.
    scroll: f32,
    /// The last frame's viewport, so a scroll can be clamped without waiting for
    /// the next one.
    viewport: (f32, f32),
}

impl std::fmt::Debug for PageScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageScene")
            .field("boxes", &self.boxes.len())
            .field("scroll", &self.scroll)
            .finish_non_exhaustive()
    }
}

impl PageScene {
    /// A scene showing `document`.
    ///
    /// The engine sees system fonts: a real page is in whatever script it is in,
    /// and the vendored family covers one of them.
    pub fn new(document: &Document) -> Self {
        Self {
            text: TextEngine::new(),
            boxes: build_box_tree(document),
            layout: None,
            scroll: 0.0,
            viewport: (0.0, 0.0),
        }
    }

    /// The box tree behind the page.
    pub fn boxes(&self) -> &BoxTree {
        &self.boxes
    }

    /// Lay the page out for `width`, reusing the last layout if the width has not
    /// changed.
    fn fragments(&mut self, width: f32, height: f32) -> &FragmentTree {
        let stale = !matches!(&self.layout, Some((last, _)) if *last == width);
        if stale {
            let tree = otlyra_layout::layout(
                &self.boxes,
                &mut self.text,
                otlyra_layout::Viewport { width, height },
            );
            self.layout = Some((width, tree));
        }
        &self.layout.as_ref().expect("just laid out").1
    }

    /// Build the frame's display list.
    pub fn build_display_list(&mut self, viewport: Viewport) -> DisplayList {
        let width = viewport.logical_width() as f32;
        let height = viewport.logical_height() as f32;
        self.viewport = (width, height);

        let scroll = self.scroll;
        let fragments = self.fragments(width, height);
        let mut list = otlyra_paint::build_display_list(fragments, (width, height), scroll);

        // Authored in logical pixels and scaled once, as everywhere else: geometry
        // stays device-scale agnostic and HiDPI is a transform, never a multiplier
        // baked into coordinates.
        list.transform(otlyra_gfx::kurbo::Affine::scale(viewport.scale_factor));
        list
    }

    /// Scroll by `delta` logical pixels, clamped to the content.
    fn scroll_by(&mut self, delta: f32) {
        let content = self
            .layout
            .as_ref()
            .map_or(0.0, |(_, tree)| tree.content_height());
        let max = (content - self.viewport.1).max(0.0);
        self.scroll = (self.scroll + delta).clamp(0.0, max);
    }
}

impl Painter for PageScene {
    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            PlatformEvent::Scroll { y, .. } => self.scroll_by(y as f32),
            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            _ => {}
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let list = self.build_display_list(viewport);
        render(&list, target);
    }
}

/// The document's `<title>`, if it has one.
///
/// Browser interface rather than page content, which is why it is here and not in
/// the box tree: `<title>` is `display: none`, and the window still has to be named
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
    use otlyra_gfx::{PaintOp, RecordingPainter};

    use super::*;

    fn scene(html: &str) -> PageScene {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        PageScene::new(&parsed.document)
    }

    fn record(scene: &mut PageScene, viewport: Viewport) -> Vec<PaintOp> {
        let mut painter = RecordingPainter::new();
        scene.paint(&mut painter, viewport);
        painter.take()
    }

    fn first_glyph_y(ops: &[PaintOp]) -> f64 {
        ops.iter()
            .find_map(|op| match op {
                PaintOp::DrawGlyphs { transform, .. } => Some(transform.as_coeffs()[5]),
                _ => None,
            })
            .expect("some text should be painted")
    }

    #[test]
    fn the_title_names_the_window_and_is_not_page_content() {
        let parsed = otlyra_html::parse(b"<title>A page</title><p>text", Some("utf-8"));
        assert_eq!(title_of(&parsed.document).as_deref(), Some("A page"));
    }

    #[test]
    fn a_document_reaches_the_paint_seam_as_glyphs() {
        let mut scene = scene("<body><h1>heading</h1><p>paragraph");
        let ops = record(&mut scene, Viewport::new(800, 600, 1.0));

        let glyph_runs = ops
            .iter()
            .filter(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
            .count();
        assert_eq!(glyph_runs, 2, "one run for the heading, one for the text");
    }

    #[test]
    fn a_scroll_event_moves_the_painted_text_up() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
        let mut scene = scene(&html);
        let viewport = Viewport::new(800, 600, 1.0);

        let before = first_glyph_y(&record(&mut scene, viewport));
        scene.on_event(PlatformEvent::Scroll { x: 0.0, y: 12.0 });
        let after = first_glyph_y(&record(&mut scene, viewport));

        assert!(
            (before - after - 12.0).abs() < 0.01,
            "text moved from {before} to {after}, expected 12px up"
        );
    }

    #[test]
    fn scrolling_is_clamped_to_the_content() {
        let mut scene = scene("<body><p>short");
        let viewport = Viewport::new(800, 600, 1.0);
        let _ = record(&mut scene, viewport);

        scene.on_event(PlatformEvent::Scroll { x: 0.0, y: -500.0 });
        assert_eq!(scene.scroll, 0.0, "cannot scroll above the top");

        scene.on_event(PlatformEvent::Scroll {
            x: 0.0,
            y: 10_000.0,
        });
        assert_eq!(
            scene.scroll, 0.0,
            "a page shorter than the window cannot scroll"
        );
    }

    /// Scrolling must not relay out: layout is a function of the width, and the
    /// width has not changed.
    #[test]
    fn scrolling_reuses_the_layout_and_resizing_does_not() {
        let mut scene = scene("<body><p>text");
        let _ = record(&mut scene, Viewport::new(800, 600, 1.0));
        let first = scene.layout.as_ref().expect("laid out").0;

        scene.on_event(PlatformEvent::Scroll { x: 0.0, y: 5.0 });
        let _ = record(&mut scene, Viewport::new(800, 600, 1.0));
        assert_eq!(scene.layout.as_ref().expect("laid out").0, first);

        let _ = record(&mut scene, Viewport::new(400, 600, 1.0));
        assert_eq!(scene.layout.as_ref().expect("laid out").0, 400.0);
    }

    /// HiDPI is a transform on the display list, not scaled geometry.
    #[test]
    fn the_device_scale_is_a_transform_not_baked_into_positions() {
        let mut scene = scene("<body><p>text");
        let one_x = record(&mut scene, Viewport::new(800, 600, 1.0));
        let two_x = record(&mut scene, Viewport::new(1600, 1200, 2.0));

        let (PaintOp::DrawGlyphs { glyphs: a, .. }, PaintOp::DrawGlyphs { glyphs: b, .. }) = (
            one_x
                .iter()
                .find(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
                .expect("text"),
            two_x
                .iter()
                .find(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
                .expect("text"),
        ) else {
            unreachable!("filtered above")
        };
        assert_eq!(a, b, "glyph offsets must not depend on the device scale");
    }
}
