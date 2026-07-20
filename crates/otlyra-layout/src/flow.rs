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
use otlyra_text::{FontStack, PlacedSpacer, Spacer, TextEngine, TextSpan};

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

/// An inline element that has a box of its own to draw: a background, a border, or
/// padding that moves the text around it.
///
/// It is not a box fragment yet, because where it starts and ends is only known
/// once the paragraph has been broken into lines.
struct InlineBox {
    id: BoxId,
    style: Arc<ComputedStyle>,
    border: Sides<f32>,
    padding: Sides<f32>,
    /// The span its content starts at, and the one it ends before.
    first_span: usize,
    last_span: usize,
}

/// The spacer identifiers for the two edges of the `index`th inline box.
fn leading_spacer(index: usize) -> u64 {
    index as u64 * 2
}

fn trailing_spacer(index: usize) -> u64 {
    index as u64 * 2 + 1
}

/// Whether any of the four sides is non-zero.
fn any_side(sides: Sides<f32>) -> bool {
    sides.top > 0.0 || sides.right > 0.0 || sides.bottom > 0.0 || sides.left > 0.0
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
        let content_height = clamp(
            style
                .height
                .resolve(containing_width)
                .unwrap_or(content_height),
            style.min_height,
            style.max_height,
            containing_width,
        );

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
        let mut inlines = Vec::new();
        self.collect_spans(parent, width, &mut spans, &mut sources, &mut inlines);
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

        // Each inline box asks for two spacers: one at each edge, carrying the
        // border and padding on that side. They reserve the room the text has to
        // move over by, and where they land is where the box starts and ends —
        // which the shaper is the only thing that knows, since it decided the
        // lines.
        let spacers: Vec<Spacer> = inlines
            .iter()
            .enumerate()
            .flat_map(|(index, inline)| {
                [
                    Spacer {
                        id: leading_spacer(index),
                        at: inline.first_span,
                        width: inline.border.left + inline.padding.left,
                    },
                    Spacer {
                        id: trailing_spacer(index),
                        at: inline.last_span,
                        width: inline.border.right + inline.padding.right,
                    },
                ]
            })
            .collect();

        let shaped = self.text.shape_spans(&spans, &spacers, Some(width));
        let style = Arc::clone(&self.tree.node(parent).style);

        // parley measures line tops from the text origin, and the first line's top
        // can sit above it by the half-leading. The paragraph's box starts where its
        // first line starts, so everything is rebased onto that.
        let paragraph_top = shaped.lines.first().map_or(0.0, |line| line.top);

        let placed: std::collections::HashMap<u64, PlacedSpacer> = shaped
            .spacers
            .iter()
            .map(|spacer| (spacer.id, *spacer))
            .collect();

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

            // The inline boxes that reach this line, before the text, so their
            // backgrounds and borders sit under the glyphs they belong to. A box
            // that spans two lines gets one fragment per line, each ending where
            // the line does — which is what CSS draws.
            let mut children: Vec<Fragment> = inlines
                .iter()
                .enumerate()
                .filter_map(|(number, inline)| {
                    let start = placed.get(&leading_spacer(number))?;
                    let end = placed.get(&trailing_spacer(number))?;
                    if index < start.line || index > end.line {
                        return None;
                    }
                    let left = if index == start.line { start.x } else { 0.0 };
                    let right = if index == end.line {
                        end.x + end.width
                    } else {
                        line.width
                    };

                    // A box broken over two lines is drawn as CSS says: the border
                    // on the start edge belongs to the piece that starts it and the
                    // one on the end edge to the piece that ends it, so the middle
                    // of a wrapped element is open at both ends rather than boxed
                    // twice.
                    let style = if index == start.line && index == end.line {
                        Arc::clone(&inline.style)
                    } else {
                        let mut broken = (*inline.style).clone();
                        if index != start.line {
                            broken.border.left = otlyra_css::Border::NONE;
                        }
                        if index != end.line {
                            broken.border.right = otlyra_css::Border::NONE;
                        }
                        Arc::new(broken)
                    };

                    Some(Fragment {
                        box_id: Some(inline.id),
                        // Vertical padding and a horizontal border spill outside
                        // the line box without making it taller: an inline box does
                        // not push its neighbours apart vertically.
                        rect: Rect::new(
                            line_x + left,
                            line_y - inline.border.top - inline.padding.top,
                            (right - left).max(0.0),
                            height
                                + inline.border.top
                                + inline.padding.top
                                + inline.border.bottom
                                + inline.padding.bottom,
                        ),
                        kind: FragmentKind::Box,
                        style,
                        children: Vec::new(),
                    })
                })
                .collect();

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
            children.extend(runs);

            out.push(Fragment {
                box_id: Some(parent),
                rect: Rect::new(line_x, line_y, line.width, height),
                kind: FragmentKind::Line,
                style: Arc::clone(&style),
                children,
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
        containing_width: f32,
        spans: &mut Vec<TextSpan<'a>>,
        sources: &mut Vec<BoxId>,
        inlines: &mut Vec<InlineBox>,
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

                    // An inline box only becomes a fragment if it has something to
                    // draw or something to reserve; the rest of them — a `<span>`
                    // that only changes the colour — stay what they are, which is
                    // the style on a run of text.
                    let border = resolve_border(&node.style);
                    let padding = resolve_padding(&node.style, containing_width);
                    let paints = node.style.background_color.components[3] > 0.0
                        || any_side(border)
                        || any_side(padding);
                    let slot = paints.then(|| {
                        inlines.push(InlineBox {
                            id: child,
                            style: Arc::clone(&node.style),
                            border,
                            padding,
                            first_span: spans.len(),
                            last_span: spans.len(),
                        });
                        inlines.len() - 1
                    });

                    self.collect_spans(child, containing_width, spans, sources, inlines);

                    if let Some(slot) = slot {
                        inlines[slot].last_span = spans.len();
                    }
                }
                BoxKind::Block => {
                    // A block inside an inline context. Real CSS splits the inline
                    // around it; we do not yet, so its text joins the paragraph
                    // rather than vanishing.
                    self.collect_spans(child, containing_width, spans, sources, inlines);
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

    // `max-width` and `min-width` are applied to whatever `width` worked out to,
    // and the box is then laid out again as if that were the width it asked for —
    // which is what makes `max-width` centre a column that `margin: 0 auto` would
    // otherwise leave full width.
    let width = match style.width.resolve(containing) {
        Some(width) => Some(clamp(width, style.min_width, style.max_width, containing)),
        None => {
            let available = (containing - margin.left - margin.right - extra).max(0.0);
            let constrained = clamp(available, style.min_width, style.max_width, containing);
            (constrained != available).then_some(constrained)
        }
    };

    let Some(width) = width else {
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

/// A size held between its minimum and its maximum.
///
/// The maximum is applied first and the minimum second, which is the order CSS
/// gives them and the reason a `min-width` larger than a `max-width` wins.
fn clamp(value: f32, min: Length, max: Option<Length>, containing: f32) -> f32 {
    let capped = match max {
        Some(max) => value.min(max.resolve(containing)),
        None => value,
    };
    capped.max(min.resolve(containing))
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

    /// Every box fragment generated by the element `tag`, in order.
    fn boxes_of(tree: &FragmentTree, boxes: &BoxTree, tag: &str) -> Vec<Fragment> {
        tree.iter()
            .filter(|fragment| {
                matches!(fragment.kind, FragmentKind::Box)
                    && fragment
                        .box_id
                        .and_then(|id| boxes.get(id))
                        .and_then(|node| node.tag.as_ref())
                        .is_some_and(|name| name.as_ref() == tag)
            })
            .cloned()
            .collect()
    }

    /// The text runs of a laid-out document, left to right within each line.
    fn runs(tree: &FragmentTree) -> Vec<Rect> {
        tree.iter()
            .filter(|fragment| matches!(fragment.kind, FragmentKind::Text(_)))
            .map(|fragment| fragment.rect)
            .collect()
    }

    /// An inline element with a background but nothing else different about it is
    /// the case the shaper merges into its neighbours' run: its box has to come
    /// from the boundaries it was shaped with, not from a run of its own.
    #[test]
    fn an_inline_box_covers_its_own_text_inside_a_merged_run() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } span { background: #ff0 }</style>\
             <p>before <span>middle</span> after</p>",
            400.0,
        );

        let pieces = boxes_of(&tree, &boxes, "span");
        let [span] = &pieces[..] else {
            panic!("one box fragment for the span");
        };
        let line = first_line(&tree);
        assert!(span.rect.x > line.x, "it starts after the text before it");
        assert!(span.rect.right() < line.right(), "and ends before the rest");
        assert!(span.rect.width > 0.0);
    }

    /// Padding and a border on an inline element take room in the line: the text
    /// after them moves over by exactly as much.
    #[test]
    fn padding_and_borders_on_an_inline_box_move_the_text_along() {
        let bare = laid_out(
            "<style>body { margin: 0 }</style><p>a<span>b</span>c</p>",
            400.0,
        )
        .0;
        let padded = laid_out(
            "<style>body { margin: 0 } \
             span { padding: 0 6px; border: 2px solid black }</style>\
             <p>a<span>b</span>c</p>",
            400.0,
        )
        .0;

        let widths = |tree: &FragmentTree| first_line(tree).width;
        assert!(
            (widths(&padded) - widths(&bare) - 16.0).abs() < 0.01,
            "two paddings and two borders wider"
        );

        let last = |tree: &FragmentTree| *runs(tree).last().expect("a run");
        assert!(
            last(&padded).x - last(&bare).x >= 15.0,
            "and the text after the span starts that much further along"
        );
    }

    /// An inline box broken over two lines is drawn as two pieces, each ending
    /// where its line does, and the border on an edge belongs to the piece that
    /// edge is on.
    #[test]
    fn a_wrapped_inline_box_is_open_where_the_line_broke() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } span { border: 2px solid black }</style>\
             <p><span>alpha beta gamma delta</span></p>",
            80.0,
        );

        let pieces = boxes_of(&tree, &boxes, "span");
        assert!(pieces.len() > 1, "the span wrapped");
        let first = pieces.first().expect("a first piece");
        let last = pieces.last().expect("a last piece");

        assert_eq!(first.style.border.left.width, 2.0);
        assert_eq!(first.style.border.right.width, 0.0);
        assert_eq!(last.style.border.left.width, 0.0);
        assert_eq!(last.style.border.right.width, 2.0);
        assert!(last.rect.y > first.rect.y, "on different lines");
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

    /// The pattern nearly every readable page is built on: a column held to a
    /// measure and centred, with no width of its own.
    #[test]
    fn max_width_and_auto_margins_centre_a_column() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             div { max-width: 300px; margin: 0 auto }</style><div>x</div>",
            800.0,
        );
        let column = rect_of(&tree, &boxes, "div");
        assert_eq!(column.width, 300.0);
        assert_eq!(column.x, 250.0);
    }

    /// A maximum below the width asked for wins, and a minimum below the maximum
    /// does not.
    #[test]
    fn min_and_max_hold_a_width_between_them() {
        let capped = laid_out(
            "<style>body { margin: 0 } div { width: 600px; max-width: 200px }</style><div>x</div>",
            800.0,
        );
        assert_eq!(rect_of(&capped.0, &capped.1, "div").width, 200.0);

        let floored = laid_out(
            "<style>body { margin: 0 } div { width: 50px; min-width: 120px }</style><div>x</div>",
            800.0,
        );
        assert_eq!(rect_of(&floored.0, &floored.1, "div").width, 120.0);

        // A minimum larger than the maximum wins, because the minimum is applied
        // last.
        let both = laid_out(
            "<style>body { margin: 0 } \
             div { width: 400px; max-width: 100px; min-width: 300px }</style><div>x</div>",
            800.0,
        );
        assert_eq!(rect_of(&both.0, &both.1, "div").width, 300.0);
    }

    /// `min-height` makes a box taller than its content, which is how a page's
    /// footer stays at the bottom of a short page.
    #[test]
    fn min_height_makes_a_box_taller_than_its_content() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } div { min-height: 400px }</style><div>x</div>",
            800.0,
        );
        assert_eq!(rect_of(&tree, &boxes, "div").height, 400.0);
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
