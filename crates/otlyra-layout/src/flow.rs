//! Block and inline layout.
//!
//! Two formatting contexts, and the rule that keeps them apart: a block container's
//! children are either all block-level, in which case they stack, or all
//! inline-level, in which case they flow into lines. The box tree's anonymous-box
//! fixup is what makes that true, so neither algorithm ever has to ask.
//!
//! Layout is a synchronous, non-reentrant call. Servo deleted its layout thread for
//! this reason: with layout on the stack, "no script runs during layout" is a fact
//! about the call stack rather than a protocol anyone has to maintain.

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

    let mut engine = Flow { tree, text };
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
}

impl Flow<'_> {
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

    /// One block-level box: margins, padding, a width, and whatever it contains.
    fn layout_block(&mut self, id: BoxId, containing_width: f32, x: f32, y: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);
        let margin = resolve_margin(&style, containing_width);
        let padding = resolve_padding(&style, containing_width);

        // `width: auto` fills the containing block minus its own margins and
        // padding. That is the whole of block width resolution for now: no
        // min/max-width, no `box-sizing`, no over-constrained-margin rule.
        let border_x = x + margin.left;
        let border_y = y + margin.top;
        let available = (containing_width - margin.left - margin.right).max(0.0);
        let content_width = match style.width.resolve(containing_width) {
            Some(explicit) => explicit,
            None => (available - padding.left - padding.right).max(0.0),
        };

        let content_x = border_x + padding.left;
        let content_y = border_y + padding.top;

        let mut children = Vec::new();
        let content_height =
            self.layout_children(id, content_width, content_x, content_y, &mut children);
        let content_height = style
            .height
            .resolve(containing_width)
            .unwrap_or(content_height);

        Fragment {
            box_id: Some(id),
            rect: Rect::new(
                border_x,
                border_y,
                content_width + padding.left + padding.right,
                content_height + padding.top + padding.bottom,
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
        self.collect_spans(parent, &mut spans);
        if spans.is_empty() {
            return 0.0;
        }

        let shaped = self.text.shape_spans(&spans, Some(width));
        let style = Arc::clone(&self.tree.node(parent).style);

        // parley measures line tops from the text origin, and the first line's top
        // can sit above it by the half-leading. The paragraph's box starts where its
        // first line starts, so everything is rebased onto that.
        let paragraph_top = shaped.lines.first().map_or(0.0, |line| line.top);

        for (index, line) in shaped.lines.iter().enumerate() {
            // Glyph positions come back relative to the paragraph; a line fragment
            // is a place on the page, so they are rebased onto it.
            let runs: Vec<_> = shaped
                .runs
                .iter()
                .filter(|run| run.line == index)
                .map(|run| {
                    let mut run = run.clone();
                    for glyph in &mut run.glyphs {
                        glyph.y -= line.top;
                    }
                    run
                })
                .collect();

            // Line boxes are contiguous: each one ends where the next begins. Taking
            // the height from the next line's top rather than from the font's line
            // height keeps them so, and avoids the fraction of a pixel of overlap
            // that leading otherwise leaves between them.
            let height = shaped
                .lines
                .get(index + 1)
                .map_or(line.height, |next| next.top - line.top);
            let rect = Rect::new(x, y + line.top - paragraph_top, line.width, height);
            let text = Fragment {
                box_id: Some(parent),
                rect,
                kind: FragmentKind::Text(runs),
                style: Arc::clone(&style),
                children: Vec::new(),
            };
            out.push(Fragment {
                box_id: Some(parent),
                rect,
                kind: FragmentKind::Line,
                style: Arc::clone(&style),
                children: vec![text],
            });
        }

        shaped.metrics.height
    }

    /// Walk an inline subtree in order, turning each text box into a styled span.
    fn collect_spans(&self, id: BoxId, spans: &mut Vec<TextSpan>) {
        for &child in &self.tree.node(id).children {
            let node = self.tree.node(child);
            match &node.kind {
                BoxKind::Text(text) => spans.push(span_for(text, &node.style)),
                BoxKind::Inline => {
                    // `<br>` is a forced break, and a newline is exactly how the
                    // shaper is told about one.
                    if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "br") {
                        // Not through `span_for`: whitespace collapsing would turn
                        // the newline into a space, which is exactly the difference
                        // between a `<br>` and a line ending in the source.
                        spans.push(TextSpan {
                            text: "\n".to_owned(),
                            ..span_for("", &node.style)
                        });
                    }
                    self.collect_spans(child, spans);
                }
                BoxKind::Block => {
                    // A block inside an inline context. Real CSS splits the inline
                    // around it; we do not yet, so its text joins the paragraph
                    // rather than vanishing.
                    self.collect_spans(child, spans);
                }
            }
        }
    }
}

/// The span one text box contributes.
fn span_for(text: &str, style: &ComputedStyle) -> TextSpan {
    let color = style.color.to_rgba8();
    TextSpan {
        text: collapse_whitespace(text),
        font_stack: FontStack::parse_css(&style.font_family),
        font_size: style.font_size,
        font_weight: style.font_weight,
        brush: [color.r, color.g, color.b, color.a],
        line_height: match style.line_height {
            otlyra_css::LineHeight::Normal => None,
            other => Some(other.resolve(style.font_size, style.font_size * 1.2)),
        },
    }
}

/// CSS `white-space: normal` collapsing.
///
/// A run of whitespace becomes one space, and a newline in the source is just more
/// whitespace — `<br>` is what makes a line break, not a line ending in the markup.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(character);
            in_space = false;
        }
    }
    out
}

fn resolve_margin(style: &ComputedStyle, containing: f32) -> Sides<f32> {
    // `auto` margins resolve to zero here. Centring with `margin: 0 auto` needs the
    // over-constrained rules, which are not in this milestone.
    let resolve = |value: LengthOrAuto| value.resolve(containing).unwrap_or(0.0);
    Sides {
        top: resolve(style.margin.top),
        right: resolve(style.margin.right),
        bottom: resolve(style.margin.bottom),
        left: resolve(style.margin.left),
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
