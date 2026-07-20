//! A page on screen, before there is a layout engine.
//!
//! This is **not** layout, and it is not pretending to be. There is no box tree, no
//! cascade, no formatting context, no float, no positioning — those are M4 through
//! M8. What this does is take the DOM we now really do build, pull the text out of
//! it in tree order, and stack the blocks down the viewport at sizes taken from the
//! element names.
//!
//! It exists because the alternative was showing a parsed document only in a
//! terminal, and a browser that cannot put a fetched page in its own window is
//! hard to believe in. Everything here goes through the real seams — the real
//! `DisplayList`, the real `PaintTarget`, the real font stack — so what is on screen
//! is produced by the code path a laid-out page will use. When `otlyra-layout`
//! lands, this file is deleted, not extended.

use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::kurbo::{Affine, Rect, Shape};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList, PaintTarget, render};
use otlyra_platform::{Painter, PlatformEvent, Viewport};
use otlyra_text::{FontStack, ShapedText, TextEngine};

/// The initial containing block's background.
const BACKGROUND: Color = Color::from_rgb8(0xff, 0xff, 0xff);

/// Body text colour.
const INK: Color = Color::from_rgb8(0x11, 0x11, 0x11);

/// Flattening tolerance for shapes entering the display list.
const PATH_TOLERANCE: f64 = 0.1;

/// Page margin, in logical pixels. A stand-in for the UA stylesheet's `body`
/// margin, which arrives with the UA style table at M4.
const MARGIN: f64 = 40.0;

/// Body font size, in logical pixels — the UA default.
const BODY_SIZE: f32 = 16.0;

/// Element names whose subtree is not rendered as page text.
///
/// `<title>` is here because it is browser interface, not content: it names the
/// window. Everything else is `display: none` in the UA stylesheet, or is content
/// we cannot draw yet.
const NOT_RENDERED: [&str; 8] = [
    "script", "style", "noscript", "template", "iframe", "object", "svg", "title",
];

/// Element names that start a new block. Loosely the UA stylesheet's
/// `display: block` set, written out rather than derived, because deriving it is
/// the cascade's job and the cascade is M8.
const BLOCK_LEVEL: [&str; 29] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figure",
    "footer",
    "form",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "caption",
    "tr",
    "td",
    "th",
    "ul",
    "figcaption",
];

/// One run of text to put on its own lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// The text, with whitespace already collapsed.
    pub text: String,
    /// Size in logical pixels.
    pub font_size: f32,
    /// Space above the block, in logical pixels.
    pub space_before: f64,
    /// Left indent, in logical pixels.
    pub indent: f64,
}

/// Pull the renderable text out of a document, in tree order.
pub fn blocks_of(document: &Document) -> Vec<Block> {
    let mut extractor = Extractor {
        document,
        blocks: Vec::new(),
        current: Block {
            text: String::new(),
            font_size: BODY_SIZE,
            space_before: 0.0,
            indent: 0.0,
        },
        depth: 0,
    };
    extractor.walk(document.root());
    extractor.flush();
    extractor.blocks
}

/// The document's `<title>`, if it has one.
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

struct Extractor<'a> {
    document: &'a Document,
    blocks: Vec<Block>,
    current: Block,
    depth: usize,
}

impl Extractor<'_> {
    fn walk(&mut self, id: NodeId) {
        let Some(node) = self.document.get(id) else {
            return;
        };

        match &node.data {
            NodeData::Text(text) => self.push_text(text),
            NodeData::Element(element) => {
                let name = element.name.local.as_ref();
                if NOT_RENDERED.contains(&name) {
                    return;
                }
                if name == "br" {
                    self.current.text.push('\n');
                    return;
                }

                let heading = heading_size(name);
                let is_block = heading.is_some() || BLOCK_LEVEL.contains(&name);
                if is_block {
                    self.flush();
                    self.current.font_size = heading.unwrap_or(BODY_SIZE);
                    self.current.space_before = if heading.is_some() { 20.0 } else { 12.0 };
                    self.current.indent = if name == "li" { 24.0 } else { 0.0 };
                    if name == "li" {
                        self.current.text.push_str("• ");
                    }
                }

                self.depth += 1;
                for child in self.document.children(id) {
                    self.walk(child);
                }
                self.depth -= 1;

                if is_block {
                    self.flush();
                }
            }
            _ => {
                for child in self.document.children(id) {
                    self.walk(child);
                }
            }
        }
    }

    /// Append text with CSS `white-space: normal` collapsing: a run of whitespace
    /// becomes one space, and a block never begins with one.
    ///
    /// Whether two text nodes are separated matters and cannot be reconstructed
    /// later: `<i>italic</i>.` and `<i>italic</i> .` differ, and joining every node
    /// with a space puts one before every closing punctuation mark on the web.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        if text.starts_with(char::is_whitespace) {
            self.push_space();
        }

        let mut words = text.split_whitespace();
        if let Some(first) = words.next() {
            self.current.text.push_str(first);
            for word in words {
                self.current.text.push(' ');
                self.current.text.push_str(word);
            }
        }

        if text.ends_with(char::is_whitespace) {
            self.push_space();
        }
    }

    /// One collapsed space, unless the block would start with it or already ends
    /// with one.
    fn push_space(&mut self) {
        if !self.current.text.is_empty() && !self.current.text.ends_with([' ', '\n']) {
            self.current.text.push(' ');
        }
    }

    fn flush(&mut self) {
        let text = self.current.text.trim().trim_end_matches('•').trim();
        if !text.is_empty() {
            self.blocks.push(Block {
                text: text.to_owned(),
                ..self.current.clone()
            });
        }
        self.current.text.clear();
        self.current.font_size = BODY_SIZE;
        self.current.space_before = 12.0;
        self.current.indent = 0.0;
    }
}

/// The UA stylesheet's heading sizes, in logical pixels, for `h1`–`h6`.
fn heading_size(name: &str) -> Option<f32> {
    Some(match name {
        "h1" => 32.0,
        "h2" => 24.0,
        "h3" => 19.0,
        "h4" => 16.0,
        "h5" => 13.0,
        "h6" => 11.0,
        _ => return None,
    })
}

/// A document on screen, with a scroll offset.
pub struct PageScene {
    text: TextEngine,
    stack: FontStack,
    blocks: Vec<Block>,
    /// One shaped paragraph per block, kept until the wrap width changes.
    ///
    /// Without this, every frame reshapes every block, so scrolling a long page
    /// costs a full shaping pass per wheel notch and the first frame of a page with
    /// one enormous block can take a visible second. Shaping is the expensive part
    /// of drawing text and it only depends on the text and the width.
    shaped: Vec<Option<ShapedText>>,
    /// The width `shaped` was produced at. A change invalidates all of it.
    shaped_width: f64,
    /// How far down the page we are, in logical pixels. Never negative, and never
    /// past the end of the content.
    scroll: f64,
    /// Height of the last frame's content, so scrolling can be clamped to it.
    content_height: f64,
    /// Logical height of the last frame's viewport, for the same reason.
    viewport_height: f64,
}

impl std::fmt::Debug for PageScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageScene")
            .field("blocks", &self.blocks.len())
            .field("scroll", &self.scroll)
            .finish_non_exhaustive()
    }
}

impl PageScene {
    /// A scene showing `document`.
    ///
    /// Uses the system fonts rather than the vendored test family: a real page is
    /// in whatever script it is in, and the vendored font covers one of them.
    pub fn new(document: &Document) -> Self {
        let blocks = blocks_of(document);
        Self {
            text: TextEngine::new(),
            stack: FontStack::parse_css("Helvetica Neue, system-ui, sans-serif"),
            shaped: vec![None; blocks.len()],
            shaped_width: 0.0,
            blocks,
            scroll: 0.0,
            content_height: 0.0,
            viewport_height: 0.0,
        }
    }

    /// The blocks this scene will paint.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Build the frame's display list.
    pub fn build_display_list(&mut self, viewport: Viewport) -> DisplayList {
        let _span = tracing::info_span!("build_display_list").entered();
        let mut list = DisplayList::new();

        let scale = Affine::scale(viewport.scale_factor);
        let width = viewport.logical_width();
        let height = viewport.logical_height();

        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: scale,
            brush: Brush::Solid(BACKGROUND),
            brush_transform: None,
            shape: Rect::new(0.0, 0.0, width, height).to_path(PATH_TOLERANCE),
        });

        // A width change is the only thing that invalidates shaping, and it
        // invalidates all of it.
        if self.shaped_width != width {
            self.shaped.clear();
            self.shaped.resize(self.blocks.len(), None);
            self.shaped_width = width;
        }

        let mut y = MARGIN;
        for (index, block) in self.blocks.iter().enumerate() {
            if self.shaped[index].is_none() {
                let available = (width - MARGIN * 2.0 - block.indent).max(1.0) as f32;
                self.shaped[index] = Some(self.text.shape(
                    &block.text,
                    &self.stack,
                    block.font_size,
                    Some(available),
                ));
            }
            let shaped = self.shaped[index].as_ref().expect("shaped just above");

            y += block.space_before;
            let top = y - self.scroll;
            y += f64::from(shaped.metrics.height);

            // Culling, not clipping: a block above or below the viewport produces no
            // items at all. On a long page that is most of them.
            if top + f64::from(shaped.metrics.height) < 0.0 || top > height {
                continue;
            }

            for run in &shaped.runs {
                list.push_glyphs(
                    &run.font,
                    run.font_size,
                    run.normalized_coords.clone(),
                    Brush::Solid(INK),
                    scale * Affine::translate((MARGIN + block.indent, top)),
                    true,
                    run.glyphs.clone(),
                );
            }
        }

        self.content_height = y + MARGIN;
        self.viewport_height = height;
        list
    }

    /// Scroll by `delta` logical pixels, clamped so the page cannot be dragged off
    /// its own top or past its own end.
    fn scroll_by(&mut self, delta: f64) {
        let max = (self.content_height - self.viewport_height).max(0.0);
        self.scroll = (self.scroll + delta).clamp(0.0, max);
    }
}

impl Painter for PageScene {
    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            // Clamped against the last frame's measurements. A scroll always
            // follows at least one paint, so they exist.
            PlatformEvent::Scroll { y, .. } => self.scroll_by(y),
            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            _ => {}
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let list = self.build_display_list(viewport);
        render(&list, target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(html: &str) -> Vec<Block> {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        blocks_of(&parsed.document)
    }

    fn texts(html: &str) -> Vec<String> {
        blocks(html).into_iter().map(|block| block.text).collect()
    }

    #[test]
    fn each_block_level_element_starts_a_block() {
        assert_eq!(
            texts("<body><p>one</p><p>two</p><div>three</div>"),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn inline_elements_stay_in_one_block() {
        assert_eq!(
            texts("<body><p>plain <b>bold</b> and <i>italic</i> text</p>"),
            ["plain bold and italic text"]
        );
    }

    #[test]
    fn whitespace_is_collapsed_the_way_css_collapses_it() {
        assert_eq!(
            texts("<body><p>  many   \n  spaces\t\there  </p>"),
            ["many spaces here"]
        );
    }

    #[test]
    fn script_and_style_contents_are_not_text() {
        assert_eq!(
            texts("<body><style>p{color:red}</style><script>var x=1</script><p>only this"),
            ["only this"]
        );
    }

    #[test]
    fn headings_come_out_larger_than_body_text() {
        let blocks = blocks("<body><h1>title</h1><p>body</p>");
        assert_eq!(blocks[0].font_size, 32.0);
        assert_eq!(blocks[1].font_size, BODY_SIZE);
    }

    #[test]
    fn list_items_get_a_marker_and_an_indent() {
        let blocks = blocks("<body><ul><li>first</li><li>second</li></ul>");
        assert_eq!(blocks[0].text, "• first");
        assert!(blocks[0].indent > 0.0);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn the_title_names_the_window_and_is_not_page_text() {
        let parsed = otlyra_html::parse(b"<title>A page</title><p>text", Some("utf-8"));
        assert_eq!(title_of(&parsed.document).as_deref(), Some("A page"));
        assert_eq!(texts("<title>A page</title><p>text"), ["text"]);
    }

    /// The bug this catches: joining every text node with a space, which puts one
    /// before the full stop after every link on the web.
    #[test]
    fn punctuation_after_an_inline_element_does_not_gain_a_space() {
        assert_eq!(
            texts("<body><p>see <i>this</i>. and <b>that</b>, too</p>"),
            ["see this. and that, too"]
        );
        assert_eq!(
            texts("<body><p>spaced <i>out</i> . here</p>"),
            ["spaced out . here"],
            "a space that is really in the source survives"
        );
    }

    #[test]
    fn an_empty_document_produces_no_blocks() {
        assert!(texts("").is_empty());
        assert!(texts("<html><head></head><body></body></html>").is_empty());
    }

    #[test]
    fn scrolling_is_clamped_to_the_content() {
        let parsed = otlyra_html::parse(b"<body><p>short", Some("utf-8"));
        let mut scene = PageScene::new(&parsed.document);
        let viewport = Viewport::new(800, 600, 1.0);
        let _ = scene.build_display_list(viewport);

        scene.scroll_by(-500.0);
        assert_eq!(scene.scroll, 0.0, "cannot scroll above the top");

        scene.scroll_by(10_000.0);
        assert!(
            scene.scroll <= scene.content_height,
            "cannot scroll past the end"
        );
    }

    /// The end-to-end assertion for scrolling: a wheel event moves the painted
    /// text, by the distance the event carried, in the direction a reader expects.
    #[test]
    fn a_scroll_event_moves_the_painted_text_up() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let mut scene = PageScene::new(&parsed.document);
        let viewport = Viewport::new(800, 600, 1.0);

        // Less than the first block is tall, so the run being measured is still the
        // same run afterwards and the comparison means something.
        let before = first_glyph_y(&mut scene, viewport);
        scene.on_event(PlatformEvent::Scroll { x: 0.0, y: 12.0 });
        let after = first_glyph_y(&mut scene, viewport);

        assert!(
            (before - after - 12.0).abs() < 0.01,
            "text moved from {before} to {after}, expected 12px up"
        );
    }

    /// The `y` translation of the first glyph run in the frame.
    fn first_glyph_y(scene: &mut PageScene, viewport: Viewport) -> f64 {
        let list = scene.build_display_list(viewport);
        list.items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Glyphs { transform, .. } => Some(transform.as_coeffs()[5]),
                _ => None,
            })
            .expect("some text should be painted")
    }

    #[test]
    fn a_page_taller_than_the_viewport_culls_what_is_off_screen() {
        let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(400);
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let mut scene = PageScene::new(&parsed.document);
        let list = scene.build_display_list(Viewport::new(800, 600, 1.0));

        assert_eq!(scene.blocks().len(), 400);
        assert!(
            list.len() < 100,
            "only the visible blocks should reach the display list, got {}",
            list.len()
        );
    }
}
