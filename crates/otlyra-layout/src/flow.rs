//! Block and inline layout.
//!
//! Two formatting contexts, and the rule that keeps them apart: a block container's
//! children are either all block-level, in which case they stack, or all
//! inline-level, in which case they flow into lines. The box tree's anonymous-box
//! fixup is what makes that true, so neither algorithm ever has to ask.
//!
//! Layout is a synchronous, non-reentrant call, and deliberately not a thread of
//! its own: with layout on the stack, "no script runs during layout" is a fact about
//! the call stack rather than a protocol anyone has to maintain.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Length, LengthOrAuto, Sides};
use otlyra_text::{FontStack, TextEngine, TextSpan};

use crate::box_tree::{BoxId, BoxKind, BoxTree};
use crate::fragment::{Fragment, FragmentKind, FragmentTree, Rect};

/// The size of the viewport, in logical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Viewport {
    /// Width available to the initial containing block.
    pub width: f32,
    /// Height of the visible area. Content may exceed it; that is what scrolling is.
    pub height: f32,
}

/// Lay out `tree` into `viewport`.
pub fn layout(tree: &BoxTree, text: &mut TextEngine, viewport: Viewport) -> FragmentTree {
    let _span = tracing::info_span!("layout", width = viewport.width).entered();

    let mut engine = Flow {
        tree,
        text,
        font_stacks: std::collections::HashMap::new(),
    };
    let root = tree.root();
    let mut children = Vec::new();
    let height = engine.layout_children(root, viewport.width, 0.0, 0.0, &mut children);

    let root_fragment = Fragment {
        box_id: Some(root),
        rect: Rect::new(0.0, 0.0, viewport.width, height.max(viewport.height)),
        kind: FragmentKind::Box,
        style: Arc::clone(&tree.node(root).style),
        children,
    };

    tracing::debug!(height, "laid out");
    FragmentTree {
        root: root_fragment,
    }
}

struct Flow<'a> {
    tree: &'a BoxTree,
    text: &'a mut TextEngine,
    /// Font stacks, keyed by the identity of the `font-family` string they were
    /// parsed from.
    ///
    /// Inheritance clones the `Arc<str>`, so every element that did not name its
    /// own family shares one pointer — which makes this a handful of entries for a
    /// whole document instead of one parse per run per layout.
    font_stacks: std::collections::HashMap<usize, FontStack>,
}

impl<'a> Flow<'a> {
    /// Lay out the children of `parent` into a content box starting at
    /// (`x`, `y`) and `width` wide. Returns the height they used.
    fn layout_children(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let children = &self.tree.node(parent).children;
        if children.is_empty() {
            return 0.0;
        }

        // The invariant from the box tree: all block-level, or all inline-level.
        if self.tree.node(children[0]).is_inline_level() {
            return self.layout_inline(parent, width, x, y, out);
        }

        let mut cursor = y;
        for &child in &children.clone() {
            let fragment = self.layout_block(child, width, x, cursor);
            cursor = fragment.rect.bottom() + bottom_margin(&fragment.style, width);
            out.push(fragment);
        }
        cursor - y
    }

    /// One block-level box: margins, borders, padding, a width, and whatever it
    /// contains.
    fn layout_block(&mut self, id: BoxId, containing_width: f32, x: f32, y: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);
        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let (margin, content_width) = resolve_horizontal(&style, containing_width, padding, border);

        let border_x = x + margin.left;
        let border_y = y + margin.top;
        let content_x = border_x + border.left + padding.left;
        let content_y = border_y + border.top + padding.top;

        let mut children = Vec::new();
        let content_height =
            self.layout_children(id, content_width, content_x, content_y, &mut children);
        let content_height = style
            .height
            .resolve(containing_width)
            .unwrap_or(content_height);

        Fragment {
            box_id: Some(id),
            // The border box: the rectangle a background paints and a border is
            // drawn on the inside edge of.
            rect: Rect::new(
                border_x,
                border_y,
                content_width + padding.left + padding.right + border.left + border.right,
                content_height + padding.top + padding.bottom + border.top + border.bottom,
            ),
            kind: FragmentKind::Box,
            style,
            children,
        }
    }

    /// An inline formatting context: everything inside becomes one paragraph, and
    /// the paragraph becomes line boxes.
    ///
    /// The whole context is shaped in one pass rather than element by element,
    /// because a line break belongs to the paragraph: `<b>bold</b> text` has to
    /// break where `bold text` breaks.
    fn layout_inline(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let mut spans = Vec::new();
        let mut sources = Vec::new();
        self.collect_spans(parent, &mut spans, &mut sources);
        if spans.is_empty() {
            return 0.0;
        }

        // Where each span landed in the concatenated text, computed the same way
        // `shape_spans` concatenates it. This is what turns a shaped run back into
        // the box it came from — and therefore into the element a click lands on.
        let mut starts = Vec::with_capacity(spans.len());
        let mut offset = 0usize;
        for span in &spans {
            starts.push(offset);
            offset += span.text.len();
        }

        let shaped = self.text.shape_spans(&spans, Some(width));
        let style = Arc::clone(&self.tree.node(parent).style);

        // parley measures line tops from the text origin, and the first line's top
        // can sit above it by the half-leading. The paragraph's box starts where its
        // first line starts, so everything is rebased onto that.
        let paragraph_top = shaped.lines.first().map_or(0.0, |line| line.top);

        for (index, line) in shaped.lines.iter().enumerate() {
            // Line boxes are contiguous: each one ends where the next begins. Taking
            // the height from the next line's top rather than from the font's line
            // height keeps them so, and avoids the fraction of a pixel of overlap
            // that leading otherwise leaves between them.
            let height = shaped
                .lines
                .get(index + 1)
                .map_or(line.height, |next| next.top - line.top);
            let line_y = y + line.top - paragraph_top;
            // Alignment moves the whole line, glyphs and all: the shaper laid it
            // out from the start edge, and where that edge is is the block's
            // decision, not the paragraph's.
            let line_x = x + match style.text_align {
                otlyra_css::TextAlign::Start => 0.0,
                otlyra_css::TextAlign::Center => ((width - line.width) / 2.0).max(0.0),
                otlyra_css::TextAlign::End => (width - line.width).max(0.0),
            };

            let runs: Vec<Fragment> = shaped
                .runs
                .iter()
                .filter(|run| run.line == index)
                .map(|run| {
                    // Glyph positions come back relative to the paragraph; a
                    // fragment is a place on the page, so they are rebased onto it.
                    let mut run = run.clone();
                    for glyph in &mut run.glyphs {
                        glyph.x -= run.offset_x;
                        glyph.y -= line.top;
                    }

                    let source = starts
                        .partition_point(|&start| start <= run.text_range.start)
                        .saturating_sub(1);
                    let box_id = sources.get(source).copied();

                    Fragment {
                        box_id,
                        rect: Rect::new(line_x + run.offset_x, line_y, run.advance, height),
                        kind: FragmentKind::Text(run),
                        // The run's own style, not the paragraph's: the underline
                        // on a link belongs to the link, and painting from the
                        // block's style would underline the whole paragraph or
                        // none of it.
                        style: box_id.map_or_else(
                            || Arc::clone(&style),
                            |id| Arc::clone(&self.tree.node(id).style),
                        ),
                        children: Vec::new(),
                    }
                })
                .collect();

            out.push(Fragment {
                box_id: Some(parent),
                rect: Rect::new(line_x, line_y, line.width, height),
                kind: FragmentKind::Line,
                style: Arc::clone(&style),
                children: runs,
            });
        }

        shaped.metrics.height
    }

    /// The parsed font stack for a style, from the cache.
    fn font_stack(&mut self, style: &Arc<ComputedStyle>) -> FontStack {
        let key = Arc::as_ptr(&style.font_family) as *const u8 as usize;
        self.font_stacks
            .entry(key)
            .or_insert_with(|| FontStack::parse_css(&style.font_family))
            .clone()
    }

    /// Walk an inline subtree in order, turning each text box into a styled span.
    ///
    /// `sources` records which box each span came from, in step with `spans`: a
    /// run of glyphs is no use to hit testing without knowing which element it
    /// belongs to.
    fn collect_spans(
        &mut self,
        id: BoxId,
        spans: &mut Vec<TextSpan<'a>>,
        sources: &mut Vec<BoxId>,
    ) {
        for &child in &self.tree.node(id).children {
            let node = self.tree.node(child);
            match &node.kind {
                BoxKind::Text(text) => {
                    spans.push(span_for(text, &node.style, self.font_stack(&node.style)));
                    // The text's own box is anonymous as far as the document is
                    // concerned; what a click means is the element around it.
                    sources.push(id);
                }
                BoxKind::Inline => {
                    // `<br>` is a forced break, and a newline is exactly how the
                    // shaper is told about one.
                    if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "br") {
                        // Not through `span_for`: whitespace collapsing would turn
                        // the newline into a space, which is exactly the difference
                        // between a `<br>` and a line ending in the source.
                        spans.push(TextSpan {
                            text: "\n",
                            ..span_for("", &node.style, self.font_stack(&node.style))
                        });
                        sources.push(child);
                    }
                    self.collect_spans(child, spans, sources);
                }
                BoxKind::Block => {
                    // A block inside an inline context. Real CSS splits the inline
                    // around it; we do not yet, so its text joins the paragraph
                    // rather than vanishing.
                    self.collect_spans(child, spans, sources);
                }
            }
        }
    }
}

/// The span one text box contributes.
///
/// The text is already collapsed — the box tree did it at load time — so this
/// borrows rather than copies.
fn span_for<'a>(text: &'a str, style: &ComputedStyle, font_stack: FontStack) -> TextSpan<'a> {
    let color = style.color.to_rgba8();
    TextSpan {
        text,
        font_stack,
        font_size: style.font_size,
        font_weight: style.font_weight,
        italic: style.font_style == otlyra_css::FontStyle::Italic,
        underline: style.text_decoration.underline,
        strikethrough: style.text_decoration.line_through,
        brush: [color.r, color.g, color.b, color.a],
        line_height: match style.line_height {
            otlyra_css::LineHeight::Normal => None,
            other => Some(other.resolve(style.font_size, style.font_size * 1.2)),
        },
    }
}

fn resolve_margin(style: &ComputedStyle, containing: f32) -> Sides<f32> {
    // `auto` starts as zero; `resolve_horizontal` is what shares out the leftover
    // when there is one to share.
    let resolve = |value: LengthOrAuto| value.resolve(containing).unwrap_or(0.0);
    Sides {
        top: resolve(style.margin.top),
        right: resolve(style.margin.right),
        bottom: resolve(style.margin.bottom),
        left: resolve(style.margin.left),
    }
}

/// The used horizontal margins and content width.
///
/// This is where `margin: 0 auto` centres. With an explicit width, whatever is
/// left over after the borders, padding and the margins that are not `auto` is
/// shared out between the ones that are — both of them for centring, one of them
/// for pushing a box to an edge. With `width: auto` there is nothing left over by
/// definition, and CSS makes an `auto` margin zero.
fn resolve_horizontal(
    style: &ComputedStyle,
    containing: f32,
    padding: Sides<f32>,
    border: Sides<f32>,
) -> (Sides<f32>, f32) {
    let mut margin = resolve_margin(style, containing);
    let extra = padding.left + padding.right + border.left + border.right;

    let Some(width) = style.width.resolve(containing) else {
        let content = (containing - margin.left - margin.right - extra).max(0.0);
        return (margin, content);
    };

    let leftover = containing - width - extra;
    let left_auto = style.margin.left == LengthOrAuto::Auto;
    let right_auto = style.margin.right == LengthOrAuto::Auto;
    match (left_auto, right_auto) {
        (true, true) => {
            margin.left = (leftover / 2.0).max(0.0);
            margin.right = margin.left;
        }
        (true, false) => margin.left = (leftover - margin.right).max(0.0),
        (false, true) => margin.right = (leftover - margin.left).max(0.0),
        (false, false) => {}
    }
    (margin, width)
}

/// The four border widths, which are already absolute lengths by this point.
fn resolve_border(style: &ComputedStyle) -> Sides<f32> {
    Sides {
        top: style.border.top.width,
        right: style.border.right.width,
        bottom: style.border.bottom.width,
        left: style.border.left.width,
    }
}

fn resolve_padding(style: &ComputedStyle, containing: f32) -> Sides<f32> {
    let resolve = |value: Length| value.resolve(containing);
    Sides {
        top: resolve(style.padding.top),
        right: resolve(style.padding.right),
        bottom: resolve(style.padding.bottom),
        left: resolve(style.padding.left),
    }
}

/// The bottom margin of a laid-out block.
///
/// Adjacent margins do **not** collapse yet. That is a real difference from CSS and
/// it is stated here rather than silently approximated: two paragraphs sit twice as
/// far apart as they should until margin collapsing lands.
fn bottom_margin(style: &ComputedStyle, containing: f32) -> f32 {
    style.margin.bottom.resolve(containing).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use otlyra_css::cascade::{Viewport as StyleViewport, style_document};

    use super::*;
    use crate::{BoxTree, FragmentKind, FragmentTree, build_styled_box_tree};

    /// Lay a document out at `width`, with its own stylesheets applied, and keep
    /// the boxes: a fragment says where something is, and only the box says what.
    fn laid_out(html: &str, width: f32) -> (FragmentTree, BoxTree) {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styles = style_document(
            &document,
            StyleViewport {
                width,
                height: 600.0,
                scale: 1.0,
            },
        );
        let boxes = build_styled_box_tree(&document, &styles);
        let mut text = otlyra_text::TextEngine::isolated();
        let tree = crate::layout(
            &boxes,
            &mut text,
            Viewport {
                width,
                height: 600.0,
            },
        );
        (tree, boxes)
    }

    /// The first box fragment whose element is `tag`.
    fn rect_of(tree: &FragmentTree, boxes: &BoxTree, tag: &str) -> Rect {
        fn walk<'a>(fragment: &'a Fragment, out: &mut Vec<&'a Fragment>) {
            out.push(fragment);
            for child in &fragment.children {
                walk(child, out);
            }
        }
        let mut all = Vec::new();
        walk(&tree.root, &mut all);
        all.into_iter()
            .find(|fragment| {
                matches!(fragment.kind, FragmentKind::Box)
                    && fragment
                        .box_id
                        .and_then(|id| boxes.get(id))
                        .and_then(|node| node.tag.as_ref())
                        .is_some_and(|name| name.as_ref() == tag)
            })
            .map(|fragment| fragment.rect)
            .unwrap_or_else(|| panic!("no <{tag}> box fragment"))
    }

    /// The first line box, which is where alignment shows.
    fn first_line(tree: &FragmentTree) -> Rect {
        fn walk(fragment: &Fragment) -> Option<Rect> {
            if matches!(fragment.kind, FragmentKind::Line) {
                return Some(fragment.rect);
            }
            fragment.children.iter().find_map(walk)
        }
        walk(&tree.root).expect("a line box")
    }

    /// The box a border is drawn on includes the border, and the content sits
    /// inside it. Getting this wrong puts text on top of its own frame.
    #[test]
    fn a_border_makes_the_box_bigger_and_moves_the_content_in() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             div { border: 5px solid black; padding: 10px }</style><div>text</div>",
            400.0,
        );

        let div = rect_of(&tree, &boxes, "div");
        assert_eq!(div.x, 0.0);
        assert_eq!(div.width, 400.0);

        let line = first_line(&tree);
        assert_eq!(line.x, 15.0, "border plus padding on the left");
        assert_eq!(line.y, 15.0, "border plus padding on the top");
        assert_eq!(
            div.height,
            line.height + 30.0,
            "the border box is the content plus both borders and both paddings"
        );
    }

    /// `margin: 0 auto` on a box with a width is how a page is centred, and the
    /// one place two `auto` values mean "share out what is left over".
    #[test]
    fn two_auto_margins_centre_a_box_with_a_width() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } div { width: 200px; margin: 0 auto }</style><div>x</div>",
            400.0,
        );
        let div = rect_of(&tree, &boxes, "div");
        assert_eq!(div.x, 100.0);
        assert_eq!(div.width, 200.0);
    }

    /// One `auto` margin takes the whole of the leftover, which pushes a box to an
    /// edge without anything having to know how wide the page is.
    #[test]
    fn one_auto_margin_pushes_the_box_to_the_other_edge() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } div { width: 200px; margin-left: auto }</style><div>x</div>",
            400.0,
        );
        assert_eq!(rect_of(&tree, &boxes, "div").x, 200.0);
    }

    #[test]
    fn text_align_moves_the_line_within_the_block() {
        let start = laid_out("<style>body{margin:0}</style><p>x</p>", 400.0).0;
        let centre = laid_out(
            "<style>body{margin:0} p{text-align:center}</style><p>x</p>",
            400.0,
        )
        .0;
        let end = laid_out(
            "<style>body{margin:0} p{text-align:right}</style><p>x</p>",
            400.0,
        )
        .0;

        assert_eq!(first_line(&start).x, 0.0);
        assert!(first_line(&centre).x > first_line(&start).x);
        assert!(first_line(&centre).x < first_line(&end).x);
        assert!(
            first_line(&end).x > 380.0,
            "a right-aligned line ends at the edge"
        );
    }
}
