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

use otlyra_css::{
    AlignItems, Clear, ComputedStyle, FlexWrap, Float, JustifyContent, Length, LengthOrAuto, Sides,
};
use otlyra_text::{FontStack, PlacedSpacer, Spacer, TextEngine, TextSpan};

use crate::box_tree::{BoxId, BoxKind, BoxTree};
use crate::fragment::{Fragment, FragmentKind, FragmentTree, Layer, Rect, ScrollPort, Sticky};

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

    let initial = Rect::new(0.0, 0.0, viewport.width, viewport.height);
    let mut engine = Flow {
        tree,
        text,
        font_stacks: std::collections::HashMap::new(),
        floats: Vec::new(),
        containing_blocks: vec![initial],
        viewport: initial,
        scroll_ports: Vec::new(),
        pending_marker: None,
    };
    let root = tree.root();
    let mut children = Vec::new();
    let height = engine.layout_children(root, viewport.width, 0.0, 0.0, &mut children);

    let root_fragment = Fragment {
        box_id: Some(root),
        rect: Rect::new(0.0, 0.0, viewport.width, height.max(viewport.height)),
        kind: FragmentKind::Box,
        style: Arc::clone(&tree.node(root).style),
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children,
    };

    tracing::debug!(height, "laid out");
    FragmentTree {
        root: root_fragment,
        scroll_ports: engine.scroll_ports,
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
    /// The floats placed so far, in page coordinates.
    ///
    /// One list for the document rather than one per formatting context: a float
    /// affects the lines it sits beside, and until block formatting contexts are
    /// told apart, "beside" is a question about the page.
    floats: Vec<FloatBox>,
    /// The padding box of the nearest positioned ancestor, which is what an
    /// absolutely positioned box measures its insets against. The first entry is
    /// the initial containing block, so the stack is never empty.
    containing_blocks: Vec<Rect>,
    /// The viewport, which is what a fixed box measures against.
    viewport: Rect,
    /// The boxes that cut their contents off and have more than they can show.
    scroll_ports: Vec<ScrollPort>,
    /// A list item's marker, between learning where its content starts and the
    /// first line being shaped inside it. See [`PendingMarker`].
    pending_marker: Option<PendingMarker>,
}

/// A list item's marker, waiting for the item's first line.
///
/// Where it goes horizontally is known as soon as the item's content edge is —
/// outside it, to the left — but where it goes vertically is the first line's
/// baseline, and nothing knows that until the line has been shaped.
struct PendingMarker {
    marker: crate::box_tree::Marker,
    /// The item's own style: a marker is set in its item's font and colour.
    style: Arc<ComputedStyle>,
    /// The item's content edge, which is what identifies the line it belongs to.
    x: f32,
}

/// A box taken out of the flow and put against an edge.
#[derive(Copy, Clone, Debug)]
struct FloatBox {
    /// Which edge it went to.
    side: Float,
    /// Its margin box, which is what lines and other floats keep clear of.
    rect: Rect,
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

/// How far a laid-out flex line reached, on each axis.
#[derive(Copy, Clone)]
struct PlacedLine {
    cross: f32,
    main: f32,
}

/// Where one flex line sits and how much room it has.
#[derive(Copy, Clone)]
struct FlexLine {
    /// Whether the main axis is horizontal.
    row: bool,
    /// Between two items on the line.
    gap: f32,
    /// The main-axis size the items are fitted into, or infinite when there is
    /// nothing to fit them into.
    inner: f32,
    /// Where the line starts across the container.
    cross_start: f32,
    /// A cross size the line is at least as big as, which is how a lone line fills
    /// a container that has a height of its own.
    cross_floor: Option<f32>,
}

/// One child of a flex container, with the sizes the container needs to place it.
struct FlexItem {
    id: BoxId,
    style: Arc<ComputedStyle>,
    /// The smallest it may be shrunk to along the main axis.
    floor: f32,
    /// Its size along the main axis before growing or shrinking.
    base: f32,
    /// Its size along the main axis after.
    main: f32,
    /// Its size across.
    cross: f32,
    grow: f32,
    shrink: f32,
    margin: Sides<f32>,
}

impl FlexItem {
    /// The margins that take room along the main axis.
    fn margin_main(&self, row: bool) -> f32 {
        if row {
            self.margin.left + self.margin.right
        } else {
            self.margin.top + self.margin.bottom
        }
    }

    /// The margins that take room across it.
    fn margin_cross(&self, row: bool) -> f32 {
        if row {
            self.margin.top + self.margin.bottom
        } else {
            self.margin.left + self.margin.right
        }
    }
}

/// A replaced box in an inline formatting context, waiting for the shaper to say
/// where in the line it landed.
struct ReplacedBox {
    id: BoxId,
    style: Arc<ComputedStyle>,
    image: Option<otlyra_gfx::peniko::ImageData>,
    /// The span it sits before.
    at: usize,
    width: f32,
    height: f32,
}

/// The spacer identifiers for the two edges of the `index`th inline box.
fn leading_spacer(index: usize) -> u64 {
    index as u64 * 2
}

fn trailing_spacer(index: usize) -> u64 {
    index as u64 * 2 + 1
}

/// The spacer identifier for the `index`th replaced box, in a range of its own so
/// that it cannot collide with an inline box's two edges.
fn replaced_spacer(index: usize) -> u64 {
    (1 << 62) | index as u64
}

/// The size a replaced box is drawn at.
///
/// CSS first, then whatever the content itself says, and a single given dimension
/// takes the other from the intrinsic ratio — which is what makes `width: 100%` on
/// a photograph keep its shape instead of squashing it.
fn replaced_size(
    style: &ComputedStyle,
    content: &crate::box_tree::Replaced,
    containing: f32,
) -> (f32, f32) {
    let intrinsic = content.intrinsic;
    let ratio = intrinsic.and_then(|(width, height)| (height > 0.0).then(|| width / height));

    let width = style.width.resolve(containing);
    let height = style.height.resolve(containing);

    let (width, height) = match (width, height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, ratio.map_or(0.0, |ratio| width / ratio)),
        (None, Some(height)) => (ratio.map_or(0.0, |ratio| height * ratio), height),
        (None, None) => intrinsic.unwrap_or((0.0, 0.0)),
    };

    (
        clamp(width, style.min_width, style.max_width, containing),
        clamp(height, style.min_height, style.max_height, containing),
    )
}

/// The horizontal space `floats` leave free between `left` and `right`, for a band
/// from `top` down `height` pixels.
///
/// A free function rather than a method, because a line asks this while the shaper
/// holds the engine.
fn band_of(floats: &[FloatBox], top: f32, height: f32, left: f32, right: f32) -> (f32, f32) {
    let bottom = top + height;
    let mut from = left;
    let mut to = right;

    for float in floats {
        if float.rect.bottom() <= top || float.rect.y >= bottom {
            continue;
        }
        match float.side {
            Float::Left => from = from.max(float.rect.right()),
            Float::Right => to = to.min(float.rect.x),
            Float::None => {}
        }
    }
    (from, to.max(from))
}

/// How far `relative` moves a box from where the flow put it.
///
/// `left` wins over `right` and `top` over `bottom`, which is what CSS says for a
/// box that names both and cannot honour the two of them.
fn relative_offset(style: &ComputedStyle, containing: f32) -> (f32, f32) {
    let x = match (
        style.inset.left.resolve(containing),
        style.inset.right.resolve(containing),
    ) {
        (Some(left), _) => left,
        (None, Some(right)) => -right,
        (None, None) => 0.0,
    };
    let y = match (
        style.inset.top.resolve(containing),
        style.inset.bottom.resolve(containing),
    ) {
        (Some(top), _) => top,
        (None, Some(bottom)) => -bottom,
        (None, None) => 0.0,
    };
    (x, y)
}

/// Give a fragment and everything inside it the same sticky constraint, so the
/// whole box travels together when the page scrolls past it.
fn mark_sticky(fragment: &mut Fragment, sticky: Sticky) {
    fragment.sticky = Some(sticky);
    for child in &mut fragment.children {
        mark_sticky(child, sticky);
    }
}

/// Say which scroll port a fragment moves with.
///
/// The innermost wins: a fragment already inside a nearer port keeps it, and that
/// port's own fragment is the one this marks.
fn set_scroll_port(fragment: &mut Fragment, port: BoxId) {
    if fragment.scroll_port.is_none() {
        fragment.scroll_port = Some(port);
    }
    for child in &mut fragment.children {
        set_scroll_port(child, port);
    }
}

/// Cut a fragment and everything inside it off at `clip`.
///
/// Intersected rather than replaced: a box inside two clipping ancestors is cut off
/// by both, and the smaller rectangle is the one that survives.
fn set_clip(fragment: &mut Fragment, clip: Rect) {
    fragment.clip = Some(match fragment.clip {
        Some(existing) => existing.intersection(&clip),
        None => clip,
    });
    for child in &mut fragment.children {
        set_clip(child, clip);
    }
}

/// Correct the containers of the sticky boxes directly inside a laid-out box.
fn set_sticky_containers(children: &mut [Fragment], container: Rect) {
    for child in children {
        if child.sticky.is_some() {
            set_container(child, container);
        }
    }
}

/// Tell a sticky subtree how far it may travel.
fn set_container(fragment: &mut Fragment, container: Rect) {
    if let Some(sticky) = fragment.sticky.as_mut() {
        sticky.container = container;
    }
    for child in &mut fragment.children {
        set_container(child, container);
    }
}

/// Put a fragment and everything inside it on one painting layer.
///
/// The whole subtree, because a positioned box takes its contents with it: text
/// inside a box that paints above its neighbours paints above them too.
fn mark_layer(fragment: &mut Fragment, layer: Layer) {
    fragment.layer = layer;
    for child in &mut fragment.children {
        // A positioned descendant has a place of its own in the order and keeps it:
        // an absolutely positioned box with a negative `z-index` inside a relative
        // one still paints below the flow, not with its parent.
        if child.layer.positioned {
            continue;
        }
        mark_layer(child, layer);
    }
}

/// Mark a fragment and everything inside it as not moving with the page.
fn mark_fixed(fragment: &mut Fragment) {
    fragment.fixed = true;
    for child in &mut fragment.children {
        mark_fixed(child);
    }
}

/// The fragment a replaced box becomes: its content, at the size it was given.
fn replaced_fragment(
    id: BoxId,
    style: &Arc<ComputedStyle>,
    image: Option<otlyra_gfx::peniko::ImageData>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) -> Fragment {
    Fragment {
        box_id: Some(id),
        rect: Rect::new(x, y, width, height),
        kind: image.map_or(FragmentKind::Box, FragmentKind::Image),
        style: Arc::clone(style),
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children: Vec::new(),
    }
}

/// The column a grid line names.
///
/// Lines count from one, and a negative line counts back from the end — which is
/// how `grid-column: -1` means the last one without knowing how many there are.
fn line_to_column(line: i32, count: usize) -> usize {
    if line > 0 {
        ((line - 1) as usize).min(count.saturating_sub(1))
    } else if line < 0 && count != usize::MAX {
        count.saturating_sub((-line) as usize)
    } else {
        0
    }
}

/// Move a fragment and everything inside it.
///
/// A float is laid out where it would have gone in the flow and then moved to its
/// edge; its descendants were positioned in page coordinates, so they move with it.
fn offset(fragment: &mut Fragment, x: f32, y: f32) {
    fragment.rect.x += x;
    fragment.rect.y += y;
    for child in &mut fragment.children {
        offset(child, x, y);
    }
}

/// Whether any of the four sides is non-zero.
/// How far a rule has raised the box it is written on, in CSS pixels.
///
/// Positive is up. The two keywords are measured against the *parent's* font size
/// rather than the box's own, which is what makes a superscript sit at the same
/// height whether it is set small or not.
///
/// A third of the font size, and a fifth of it, are what the specification names as
/// the amounts to use when a UA does not take them from the font — plus the pixel
/// every engine adds on top, which is the amount the web was actually built
/// against.
fn baseline_shift(style: &ComputedStyle, parent: &ComputedStyle) -> f32 {
    match style.vertical_align {
        otlyra_css::VerticalAlign::Baseline => 0.0,
        otlyra_css::VerticalAlign::Super => parent.font_size / 3.0 + 1.0,
        otlyra_css::VerticalAlign::Sub => -(parent.font_size / 5.0 + 1.0),
        otlyra_css::VerticalAlign::Length(px) => px,
        // A percentage is of the box's own line height, which is the one place a
        // percentage in CSS is not of the containing block.
        otlyra_css::VerticalAlign::Percent(fraction) => {
            fraction
                * style
                    .line_height
                    .resolve(style.font_size, style.font_size * 1.2)
        }
    }
}

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
        // A list item's marker waits here for the first line laid out at this
        // item's own content edge. It cannot be placed now — where it sits
        // vertically is the first line's baseline, which only shaping knows — and
        // it cannot be a box inside the content, because CSS puts it outside.
        //
        // Matched on the left edge rather than taken by whoever asks first: an item
        // whose only child is a nested list would otherwise hand its marker to that
        // list's first item, which is indented and is not the line it belongs to.
        if let Some(marker) = self.tree.marker(parent) {
            self.pending_marker = Some(PendingMarker {
                marker: marker.clone(),
                style: Arc::clone(&self.tree.node(parent).style),
                x,
            });
        }

        let children = &self.tree.node(parent).children;
        if children.is_empty() {
            return 0.0;
        }

        match self.tree.node(parent).style.display {
            otlyra_css::Display::Flex => return self.layout_flex(parent, width, x, y, out),
            otlyra_css::Display::Grid => return self.layout_grid(parent, width, x, y, out),
            _ => {}
        }

        // The invariant from the box tree: all block-level, or all inline-level.
        if self.tree.node(children[0]).is_inline_level() {
            return self.layout_inline(parent, width, x, y, out);
        }

        // Vertical margins between siblings collapse: two blocks that each ask for
        // a margin are separated by the larger of the two and not by their sum,
        // which is why a page of paragraphs is spaced the way its author expected.
        // `pending` is the run of margins that has met and not yet been spent.
        //
        // A margin also escapes through an edge with no border and no padding on
        // it, so the first child's top margin is the *parent's* to spend, and was
        // spent before this call. The same at the bottom.
        let (top_open, bottom_open) = self.open_edges(parent, width);
        let mut cursor = y;
        let mut pending = 0.0;
        let last = children.len() - 1;
        // Which fragments are waiting to be told how far they may travel: a sticky
        // box may not leave its container, and how tall that is is only known once
        // everything in it has been laid out.
        let mut sticky: Vec<usize> = Vec::new();

        for (index, &child) in children.clone().iter().enumerate() {
            let style = Arc::clone(&self.tree.node(child).style);

            // An absolutely positioned box is out of the flow entirely: it takes no
            // room and its siblings stack as though it did not exist. It is placed
            // against a containing block rather than against the cursor.
            if style.position.is_out_of_flow() {
                let fragment = self.layout_positioned(child, cursor + pending);
                out.push(fragment);
                continue;
            }

            // A float is out of the flow: it does not move the cursor, and the
            // boxes after it stack as though it were not there. What it does do is
            // shorten the lines it sits beside, which is the inline layout's
            // business and is why it is recorded rather than returned.
            if style.float != Float::None {
                let fragment = self.layout_float(child, width, x, cursor + pending);
                out.push(fragment);
                continue;
            }

            if index == 0 && top_open {
                pending = 0.0;
            } else {
                pending = collapse(pending, self.collapsed_top(child, width));
            }

            // `clear` puts a box below the floats it names rather than beside them.
            if style.clear != Clear::None {
                let cleared = self.clearance(style.clear, cursor + pending);
                if cleared > cursor + pending {
                    cursor = cleared;
                    pending = 0.0;
                }
            }

            let mut fragment = self.layout_block(child, width, x, cursor + pending);

            // `relative` moves a box after the flow has placed it, and moves
            // nothing else: the gap it left stays where it was, which is what makes
            // it a nudge rather than a layout. So the flow is advanced by where the
            // box was, not by where it went.
            let flowed = fragment.rect;
            if style.position == otlyra_css::Position::Relative {
                let (dx, dy) = relative_offset(&style, width);
                offset(&mut fragment, dx, dy);
                mark_layer(
                    &mut fragment,
                    Layer {
                        index: style.z_index.unwrap_or(0),
                        positioned: true,
                    },
                );
            }

            let bottom = if index == last && bottom_open {
                0.0
            } else {
                self.collapsed_bottom(child, width)
            };

            // A box with no height and nothing to separate its own two margins
            // collapses through: its top and bottom join the same run rather than
            // opening a gap on each side of nothing.
            if style.position == otlyra_css::Position::Sticky {
                mark_sticky(
                    &mut fragment,
                    Sticky {
                        top: style.inset.top.resolve(width),
                        bottom: style.inset.bottom.resolve(width),
                        own: flowed,
                        // Filled in below, once the container's height is known.
                        container: flowed,
                    },
                );
                sticky.push(out.len());
            }

            if flowed.height == 0.0 {
                pending = collapse(pending, bottom);
            } else {
                cursor = flowed.bottom();
                pending = bottom;
            }
            out.push(fragment);
        }

        let used = cursor + pending - y;

        // A provisional container: the height the children came to. Whoever laid
        // this box out replaces it once the box's own height is settled, since a
        // `height` of its own is what a sticky child may actually travel down.
        if !sticky.is_empty() {
            let container = Rect::new(x, y, width, used);
            for index in sticky {
                set_container(&mut out[index], container);
            }
        }

        used
    }

    /// Lay out a box's children, making it the containing block for the absolutely
    /// positioned ones if its `position` says so.
    ///
    /// The height it offers is the height it was given, or the rest of the page
    /// when it has none of its own: what a percentage inset resolves against is the
    /// padding box, and a box whose height is its content's is not measured until
    /// its content — including these very children — has been laid out.
    fn layout_inside(
        &mut self,
        id: BoxId,
        content_width: f32,
        content_x: f32,
        content_y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(id).style);
        if !style.position.is_containing_block() {
            return self.layout_children(id, content_width, content_x, content_y, out);
        }

        let padding = resolve_padding(&style, content_width);
        let height = style
            .height
            .resolve(content_width)
            .unwrap_or_else(|| (self.viewport.bottom() - content_y).max(0.0));
        self.containing_blocks.push(Rect::new(
            content_x - padding.left,
            content_y - padding.top,
            content_width + padding.left + padding.right,
            height + padding.top + padding.bottom,
        ));

        let used = self.layout_children(id, content_width, content_x, content_y, out);
        self.containing_blocks.pop();
        used
    }

    /// Place an absolutely or fixed positioned box against its containing block.
    ///
    /// `static_y` is where the box would have been in the flow, which is what an
    /// `auto` inset resolves to — a positioned box with no insets stays where it
    /// was and only leaves the flow.
    fn layout_positioned(&mut self, id: BoxId, static_y: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);
        let area = if style.position == otlyra_css::Position::Fixed {
            self.viewport
        } else {
            *self
                .containing_blocks
                .last()
                .expect("the initial containing block is always there")
        };

        let inset = |value: LengthOrAuto, against: f32| value.resolve(against);
        let left = inset(style.inset.left, area.width);
        let right = inset(style.inset.right, area.width);
        let top = inset(style.inset.top, area.height);
        let bottom = inset(style.inset.bottom, area.height);

        // The width: what it asks for, what its two insets leave between them, or
        // what its content wants.
        let width = match (style.width.resolve(area.width), left, right) {
            (Some(width), _, _) => width,
            (None, Some(left), Some(right)) => (area.width - left - right).max(0.0),
            _ => self
                .max_content_width(id, area.width)
                .min(area.width)
                .max(0.0),
        };

        // A float outside does not reach into a positioned box, and one inside does
        // not reach out.
        let outer_floats = std::mem::take(&mut self.floats);
        let mut fragment = self.layout_sized(id, area.x, area.y, width);
        self.floats = outer_floats;

        let x = match (left, right) {
            (Some(left), _) => area.x + left,
            (None, Some(right)) => area.x + area.width - right - fragment.rect.width,
            (None, None) => area.x,
        };
        let y = match (top, bottom) {
            (Some(top), _) => area.y + top,
            (None, Some(bottom)) => area.y + area.height - bottom - fragment.rect.height,
            // No inset at all: where the flow would have put it.
            (None, None) => static_y,
        };

        let (delta_x, delta_y) = (x - fragment.rect.x, y - fragment.rect.y);
        offset(&mut fragment, delta_x, delta_y);
        if style.position == otlyra_css::Position::Fixed {
            mark_fixed(&mut fragment);
        }
        mark_layer(
            &mut fragment,
            Layer {
                index: style.z_index.unwrap_or(0),
                positioned: true,
            },
        );
        fragment
    }

    /// A block laid out at a width the caller decided, rather than one worked out
    /// from its containing block.
    fn layout_sized(&mut self, id: BoxId, x: f32, y: f32, width: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A picture is its own content: it has no children to lay out and its
        // height comes from its own proportions rather than from anything inside it.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            let (_, height) = replaced_size(&style, content, width);
            return replaced_fragment(id, &style, content.image.clone(), x, y, width, height);
        }

        let padding = resolve_padding(&style, width);
        let border = resolve_border(&style);
        let content_width =
            (width - padding.left - padding.right - border.left - border.right).max(0.0);

        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;
        let mut children = Vec::new();
        let content_height =
            self.layout_inside(id, content_width, content_x, content_y, &mut children);
        let content_height = clamp(
            style.height.resolve(width).unwrap_or(content_height),
            style.min_height,
            style.max_height,
            width,
        );

        Fragment {
            box_id: Some(id),
            rect: Rect::new(
                x,
                y,
                width,
                content_height + padding.top + padding.bottom + border.top + border.bottom,
            ),
            kind: FragmentKind::Box,
            style,
            fixed: false,
            scroll_port: None,
            clip: None,
            sticky: None,
            layer: Layer::default(),
            children,
        }
    }

    /// Place a floated box against its edge, at or below `y`.
    ///
    /// It is laid out like any other block and then moved: to the near edge if it
    /// fits beside what is already there, and down past the floats in the way if it
    /// does not.
    fn layout_float(&mut self, id: BoxId, containing_width: f32, x: f32, y: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A float establishes a formatting context of its own: the floats outside
        // it do not shorten the lines inside it, and the ones inside it do not
        // reach out. Hiding the list for the duration is the whole of that rule.
        let outer_floats = std::mem::take(&mut self.floats);

        // A float with no width of its own shrinks to fit: as wide as its content
        // wants, and never wider than what is left for it. A block would have taken
        // the whole column, which is the one thing a float must not do.
        let mut fragment = match style.width.resolve(containing_width) {
            Some(_) => self.layout_block(id, containing_width, x, y),
            None => {
                let margin = resolve_margin(&style, containing_width);
                let available = (containing_width - margin.left - margin.right).max(0.0);
                let content = self.max_content_width(id, containing_width);
                let floor = self.min_content_width(id, containing_width, true);
                let width = clamp(
                    content.clamp(floor.min(available), available),
                    style.min_width,
                    style.max_width,
                    containing_width,
                );
                self.layout_sized(id, x + margin.left, y, width)
            }
        };
        self.floats = outer_floats;

        let margin = resolve_margin(&style, containing_width);
        let outer = fragment.rect.width + margin.left + margin.right;

        // Down until there is room. A float never overlaps another one, so the
        // first band with space enough is where it goes.
        let mut top = self.clearance(
            match style.clear {
                Clear::None => Clear::None,
                other => other,
            },
            y,
        );
        let height = fragment.rect.height.max(1.0);
        let left_edge = x;
        let right_edge = x + containing_width;

        loop {
            let (from, to) = self.band(top, height, left_edge, right_edge);
            if to - from >= outer || !self.floats.iter().any(|float| float.rect.height > 0.0) {
                let placed_x = match style.float {
                    Float::Right => to - outer + margin.left,
                    _ => from + margin.left,
                };
                let delta_x = placed_x - fragment.rect.x;
                let delta_y = top - fragment.rect.y;
                offset(&mut fragment, delta_x, delta_y);
                break;
            }

            // The next band starts at the bottom of the nearest float in the way.
            let Some(next) = self
                .floats
                .iter()
                .map(|float| float.rect.bottom())
                .filter(|bottom| *bottom > top)
                .min_by(f32::total_cmp)
            else {
                break;
            };
            top = next;
        }

        self.floats.push(FloatBox {
            side: style.float,
            rect: Rect::new(
                fragment.rect.x - margin.left,
                fragment.rect.y,
                outer,
                fragment.rect.height + margin.top + margin.bottom,
            ),
        });
        fragment
    }

    /// The horizontal space free of floats between `left` and `right`, for a band
    /// from `top` down `height` pixels.
    fn band(&self, top: f32, height: f32, left: f32, right: f32) -> (f32, f32) {
        band_of(&self.floats, top, height, left, right)
    }

    /// The lowest `y` a box with this `clear` may start at.
    fn clearance(&self, clear: Clear, y: f32) -> f32 {
        self.floats
            .iter()
            .filter(|float| match clear {
                Clear::Both => true,
                Clear::Left => float.side == Float::Left,
                Clear::Right => float.side == Float::Right,
                Clear::None => false,
            })
            .map(|float| float.rect.bottom())
            .fold(y, f32::max)
    }

    /// Whether margins pass through the top and bottom edges of `id`.
    ///
    /// An edge is open when nothing sits on it: no border, no padding, and no line
    /// of text, since a line box is content and content is what a margin cannot
    /// pass through. The root is closed at both ends however empty it is — a
    /// document's own margins stay inside it.
    fn open_edges(&self, id: BoxId, containing_width: f32) -> (bool, bool) {
        if id == self.tree.root() {
            return (false, false);
        }
        let node = self.tree.node(id);
        if node
            .children
            .first()
            .is_some_and(|&child| self.tree.node(child).is_inline_level())
        {
            return (false, false);
        }

        let style = &node.style;
        let border = resolve_border(style);
        let padding = resolve_padding(style, containing_width);
        // A height of its own stops a margin at the bottom edge: the box ends where
        // it says it does, not where its last child does.
        let sized = style.height != LengthOrAuto::Auto;

        (
            border.top == 0.0 && padding.top == 0.0,
            border.bottom == 0.0 && padding.bottom == 0.0 && !sized,
        )
    }

    /// The margin `id` presents to whatever is above it, including any that
    /// escaped from its own first children.
    fn collapsed_top(&self, id: BoxId, containing_width: f32) -> f32 {
        let node = self.tree.node(id);
        let mut margin = vertical_margin(node.style.margin.top, containing_width);
        if self.open_edges(id, containing_width).0
            && let Some(&first) = node.children.first()
        {
            margin = collapse(margin, self.collapsed_top(first, containing_width));
        }
        margin
    }

    /// The margin `id` presents to whatever is below it.
    fn collapsed_bottom(&self, id: BoxId, containing_width: f32) -> f32 {
        let node = self.tree.node(id);
        let mut margin = vertical_margin(node.style.margin.bottom, containing_width);
        if self.open_edges(id, containing_width).1
            && let Some(&last) = node.children.last()
        {
            margin = collapse(margin, self.collapsed_bottom(last, containing_width));
        }
        margin
    }

    /// One block-level box: margins, borders, padding, a width, and whatever it
    /// contains.
    fn layout_block(&mut self, id: BoxId, containing_width: f32, x: f32, y: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);
        // A box that cuts its contents off is a formatting context of its own: the
        // floats outside it do not shorten the lines inside it, and its own do not
        // reach out. This is the rule `overflow: hidden` is best known for.
        let outer_floats = (style.overflow == otlyra_css::Overflow::Clip)
            .then(|| std::mem::take(&mut self.floats));
        // A block-level picture is its own size and has no children to lay out;
        // everything else about it — margins, borders — is an ordinary block's.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            let (width, height) = replaced_size(&style, content, containing_width);
            let image = content.image.clone();
            let margin = resolve_margin(&style, containing_width);
            return Fragment {
                box_id: Some(id),
                rect: Rect::new(x + margin.left, y, width, height),
                kind: image.map_or(FragmentKind::Box, FragmentKind::Image),
                style,
                fixed: false,
                scroll_port: None,
                clip: None,
                sticky: None,
                layer: Layer::default(),
                children: Vec::new(),
            };
        }

        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let (margin, content_width) = resolve_horizontal(&style, containing_width, padding, border);

        let border_x = x + margin.left;
        let border_y = y;
        let content_x = border_x + border.left + padding.left;
        let content_y = border_y + border.top + padding.top;

        let mut children = Vec::new();
        let content_height =
            self.layout_inside(id, content_width, content_x, content_y, &mut children);
        let content_height = clamp(
            style
                .height
                .resolve(containing_width)
                .unwrap_or(content_height),
            style.min_height,
            style.max_height,
            containing_width,
        );

        if let Some(floats) = outer_floats {
            self.floats = floats;
        }

        set_sticky_containers(
            &mut children,
            Rect::new(content_x, content_y, content_width, content_height),
        );

        // `overflow` other than `visible` cuts its contents off at the padding
        // edge. The rectangle is handed down rather than pushed as a layer, so a
        // fragment carries the one rectangle it is cut off at however deep it is.
        if style.overflow == otlyra_css::Overflow::Clip {
            let padding_box = Rect::new(
                border_x + border.left,
                border_y + border.top,
                content_width + padding.left + padding.right,
                content_height + padding.top + padding.bottom,
            );
            for child in &mut children {
                set_clip(child, padding_box);
            }

            // How much there is to see: the furthest any of its contents reaches.
            // More than the box can show is what makes it a scroll port.
            let reach = children
                .iter()
                .map(|child| child.rect.bottom())
                .fold(content_y, f32::max);
            let inside = reach - content_y + padding.top + padding.bottom;
            if inside > padding_box.height + 0.5 {
                self.scroll_ports.push(ScrollPort {
                    id,
                    port: padding_box,
                    content_height: inside,
                });
                for child in &mut children {
                    set_scroll_port(child, id);
                }
            }
        }

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
            fixed: false,
            scroll_port: None,
            clip: None,
            sticky: None,
            layer: Layer::default(),
            children,
        }
    }

    /// A grid formatting context: the children are placed into rows and columns.
    ///
    /// The columns come from `grid-template-columns`: a fixed track takes what it
    /// asks for, an `auto` one takes what its widest item wants, and the `fr` tracks
    /// share out whatever is left. Items are then placed in order, filling each row
    /// before starting the next, and each row is as tall as the tallest thing in it
    /// unless `grid-template-rows` says otherwise.
    fn layout_grid(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(parent).style);
        let children = self.tree.node(parent).children.clone();
        if children.is_empty() {
            return 0.0;
        }

        let column_gap = style.gap.1.resolve(width);
        let row_gap = style.gap.0.resolve(width);

        // A grid with no template is one column of everything, which is what a
        // block container would have done and is the least surprising fallback.
        let mut template = style.grid_columns.clone();

        // `repeat(auto-fill, …)`: the pattern goes in as many times as the room
        // left over allows, which is what makes a card grid answer to its width
        // without a media query.
        if let Some(pattern) = style.grid_columns_fill.as_ref()
            && !pattern.is_empty()
        {
            let spent: f32 = template
                .iter()
                .map(|track| match track {
                    otlyra_css::Track::Fixed(length) => length.resolve(width),
                    _ => 0.0,
                })
                .sum::<f32>()
                + column_gap * template.len() as f32;
            let one: f32 = pattern
                .iter()
                .map(|track| match track {
                    otlyra_css::Track::Fixed(length) => length.resolve(width),
                    _ => 0.0,
                })
                .sum::<f32>()
                + column_gap * (pattern.len().saturating_sub(1)) as f32;

            let times = if one > 0.0 {
                (((width - spent + column_gap) / (one + column_gap)).floor() as usize).max(1)
            } else {
                1
            };
            for _ in 0..times {
                template.extend(pattern.iter().copied());
            }
        }

        if template.is_empty() {
            template.push(otlyra_css::Track::Auto);
        }
        let count = template.len();

        // Where every item goes. An item that names a line takes those cells; the
        // rest are placed in order into whatever is still free, which is the
        // auto-placement CSS describes, in its simple row-major form.
        let mut taken: Vec<bool> = Vec::new();
        let mut cells: Vec<(usize, usize, usize)> = Vec::with_capacity(children.len());
        let mut cursor_cell = 0usize;
        let occupied = |taken: &mut Vec<bool>, row: usize, column: usize| -> bool {
            let at = row * count + column;
            if at >= taken.len() {
                taken.resize(at + 1, false);
            }
            taken[at]
        };
        let occupy = |taken: &mut Vec<bool>, row: usize, column: usize| {
            let at = row * count + column;
            if at >= taken.len() {
                taken.resize(at + 1, false);
            }
            taken[at] = true;
        };

        for &child in &children {
            let item = Arc::clone(&self.tree.node(child).style);
            let span = (item.grid_column.span as usize).clamp(1, count);

            let (row, column) = match (item.grid_column.line, item.grid_row.line) {
                // A line of its own. The cursor never goes backwards, so a free cell
                // left behind by an item placed further along stays empty — CSS
                // fills those only when asked to, with `grid-auto-flow: dense`.
                (Some(line), row) => {
                    let column = line_to_column(line, count);
                    let from = cursor_cell / count;
                    let row = row.map_or_else(
                        || {
                            (from..)
                                .find(|row| {
                                    (0..span).all(|offset| {
                                        !occupied(
                                            &mut taken,
                                            *row,
                                            (column + offset).min(count - 1),
                                        )
                                    })
                                })
                                .unwrap_or(from)
                        },
                        |line| line_to_column(line, usize::MAX),
                    );
                    (row, column)
                }
                (None, Some(line)) => {
                    let row = line_to_column(line, usize::MAX);
                    let column = (0..count)
                        .find(|column| !occupied(&mut taken, row, *column))
                        .unwrap_or(0);
                    (row, column)
                }
                (None, None) => {
                    // The next free run of `span` cells, from wherever the cursor
                    // has got to.
                    loop {
                        let row = cursor_cell / count;
                        let column = cursor_cell % count;
                        let fits = column + span <= count
                            && (0..span).all(|offset| !occupied(&mut taken, row, column + offset));
                        if fits {
                            break (row, column);
                        }
                        cursor_cell += 1;
                    }
                }
            };

            // Wherever it went, the next item starts after it.
            cursor_cell = cursor_cell.max(row * count + column + span);

            for offset in 0..span {
                occupy(&mut taken, row, (column + offset).min(count - 1));
            }
            cells.push((row, column, span));
        }

        // What each column has to hold, which is what an `auto` track is measured
        // from: the widest item in it, and an item spanning several tracks counts
        // towards none of them on its own.
        let mut column_content = vec![0.0f32; count];
        for (index, &child) in children.iter().enumerate() {
            let (_, column, span) = cells[index];
            if span == 1 {
                let wanted = self.max_content_width(child, width);
                column_content[column] = column_content[column].max(wanted);
            }
        }

        // The columns: fixed first, then `auto` from content, then the leftover
        // shared out by `fr`.
        let gaps = column_gap * (count.saturating_sub(1)) as f32;
        let mut columns = vec![0.0f32; count];
        let mut fractions = 0.0f32;
        let mut used = gaps;
        for (index, track) in template.iter().enumerate() {
            match track {
                otlyra_css::Track::Fixed(length) => {
                    columns[index] = length.resolve(width);
                    used += columns[index];
                }
                otlyra_css::Track::Auto => {
                    columns[index] = column_content[index];
                    used += columns[index];
                }
                otlyra_css::Track::Fraction(share) => fractions += share.max(0.0),
            }
        }
        let leftover = (width - used).max(0.0);
        if fractions > 0.0 {
            for (index, track) in template.iter().enumerate() {
                if let otlyra_css::Track::Fraction(share) = track {
                    columns[index] = leftover * share.max(0.0) / fractions;
                }
            }
        }

        // Where each column starts.
        let mut offsets = Vec::with_capacity(count);
        let mut at = x;
        for column in &columns {
            offsets.push(at);
            at += column + column_gap;
        }

        // A grid establishes a formatting context of its own.
        let outer_floats = std::mem::take(&mut self.floats);

        let rows = cells.iter().map(|(row, _, _)| row + 1).max().unwrap_or(0);
        let mut row_tops = vec![y; rows + 1];
        let mut fragments: Vec<(usize, Fragment)> = Vec::with_capacity(children.len());

        // One row at a time, because a row is as tall as the tallest thing in it and
        // the next row starts where it ends.
        for row in 0..rows {
            let mut height = 0.0f32;
            for (index, &child) in children.iter().enumerate() {
                let (item_row, column, span) = cells[index];
                if item_row != row {
                    continue;
                }
                // A span covers its columns and the gaps between them.
                let cell_width: f32 = (0..span)
                    .map(|offset| columns.get(column + offset).copied().unwrap_or(0.0))
                    .sum::<f32>()
                    + column_gap * (span.saturating_sub(1)) as f32;
                let fragment = self.layout_sized(
                    child,
                    offsets.get(column).copied().unwrap_or(x),
                    row_tops[row],
                    cell_width,
                );
                height = height.max(fragment.rect.height);
                fragments.push((index, fragment));
            }

            // A row with a size of its own takes it, whatever is in it. An `fr` down
            // the block axis needs a definite height to share out; without one it is
            // what the content needs, which is what `auto` already gives.
            if let Some(otlyra_css::Track::Fixed(length)) = style.grid_rows.get(row) {
                height = length.resolve(width);
            }

            for (index, fragment) in &mut fragments {
                if cells[*index].0 != row {
                    continue;
                }
                // Stretched to the row, which is `align-items: stretch` and is what
                // makes a row of cards the same height.
                let id = fragment.box_id.expect("a grid item came from a box");
                let has_height = self.tree.node(id).style.height != LengthOrAuto::Auto;
                if !has_height && fragment.rect.height < height {
                    fragment.rect.height = height;
                }
            }

            row_tops[row + 1] = row_tops[row] + height + row_gap;
        }

        for (_, fragment) in fragments {
            out.push(fragment);
        }

        self.floats = outer_floats;
        (row_tops[rows] - row_gap - y).max(0.0)
    }

    /// A flex formatting context: the children are items along one axis.
    ///
    /// One pass in each direction. Along the main axis every item is measured at
    /// its base size, and what is left over — or missing — is shared out by
    /// `flex-grow` and `flex-shrink`; across the cross axis each item is placed by
    /// `align-items`, stretching to the line by default, which is what makes
    /// columns of equal height without anyone saying how tall.
    fn layout_flex(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(parent).style);
        let children = self.tree.node(parent).children.clone();
        let row = style.flex_direction.is_row();
        let gap = if row {
            style.gap.1.resolve(width)
        } else {
            style.gap.0.resolve(width)
        };

        // A float outside the container does not reach into it: a flex container
        // establishes a formatting context of its own.
        let outer_floats = std::mem::take(&mut self.floats);

        // The base size every item starts at, measured by laying it out on its own.
        let mut items: Vec<FlexItem> = Vec::with_capacity(children.len());
        for &child in &children {
            let item_style = Arc::clone(&self.tree.node(child).style);
            let fragment = self.layout_block(child, width, x, y);
            let margin = resolve_margin(&item_style, width);

            // The base size: `flex-basis` if it says, then the item's own size, and
            // for an auto width along a row the size its content wants — a flex
            // item is not a block, and does not fill the line it is on.
            let basis = match item_style.flex_basis.and_then(|basis| basis.resolve(width)) {
                Some(basis) => basis,
                None if row => match item_style.width.resolve(width) {
                    Some(_) => fragment.rect.width,
                    None => {
                        let content = self.max_content_width(child, width);
                        clamp(
                            content.min(width),
                            item_style.min_width,
                            item_style.max_width,
                            width,
                        )
                    }
                },
                None => fragment.rect.height,
            };

            // A flex item's automatic minimum size: it may be shrunk, but not past
            // the point where its own content spills out of it. An item that says
            // `min-width` or `overflow` of its own would override this; neither is
            // read yet, so the content is the floor.
            let floor = if row && item_style.min_width == Length::ZERO {
                self.min_content_width(child, width, true).min(basis)
            } else {
                item_style.min_width.resolve(width)
            };

            items.push(FlexItem {
                id: child,
                floor,
                base: basis,
                main: basis,
                cross: if row {
                    fragment.rect.height
                } else {
                    fragment.rect.width
                },
                grow: item_style.flex_grow,
                shrink: item_style.flex_shrink,
                margin,
                style: item_style,
            });
        }

        if items.is_empty() {
            self.floats = outer_floats;
            return 0.0;
        }

        // A container with a height of its own has a definite cross size when it
        // is a row and a definite main size when it is a column: either way it is
        // the size the items are fitted into rather than one they add up to.
        let definite_height = style
            .height
            .resolve(width)
            .map(|height| clamp(height, style.min_height, style.max_height, width));
        let inner = if row {
            width
        } else {
            definite_height.unwrap_or(f32::INFINITY)
        };

        // Lines: one, unless wrapping is allowed and the items do not fit on it.
        let lines: Vec<std::ops::Range<usize>> =
            if style.flex_wrap == FlexWrap::NoWrap || !inner.is_finite() {
                std::iter::once(0..items.len()).collect()
            } else {
                let mut lines = Vec::new();
                let mut start = 0;
                let mut used = 0.0;
                for (index, item) in items.iter().enumerate() {
                    let outer = item.base + item.margin_main(row);
                    let with_gap = if index == start { outer } else { outer + gap };
                    if index > start && used + with_gap > inner {
                        lines.push(start..index);
                        start = index;
                        used = outer;
                    } else {
                        used += with_gap;
                    }
                }
                lines.push(start..items.len());
                lines
            };

        // A single line fills a container that has a cross size of its own — which
        // is what makes `align-items: center` centre against the container rather
        // than against the tallest item. Several lines share the container out by
        // taking what they need, which is `align-content: flex-start`.
        let line_cross_floor = match (row, definite_height, lines.len()) {
            (true, Some(height), 1) => Some(height),
            _ => None,
        };

        let mut cross_cursor = 0.0f32;
        let mut main_extent = 0.0f32;
        let line_count = lines.len();
        for (number, line) in lines.into_iter().enumerate() {
            let placed = self.layout_flex_line(
                &mut items,
                line,
                &style,
                FlexLine {
                    row,
                    gap,
                    inner,
                    cross_start: cross_cursor,
                    cross_floor: line_cross_floor,
                },
                (x, y),
                out,
            );
            cross_cursor += placed.cross;
            main_extent = main_extent.max(placed.main);
            if number + 1 < line_count {
                cross_cursor += if row { style.gap.0.resolve(width) } else { gap };
            }
        }

        self.floats = outer_floats;

        // The height a flex container takes: across the lines when it is a row,
        // along the longest of them when it is a column.
        if row { cross_cursor } else { main_extent }
    }

    /// One line of a flex container: the main axis shared out, the cross axis
    /// aligned, and every item placed.
    ///
    /// Returns how much room the line took across the container.
    fn layout_flex_line(
        &mut self,
        items: &mut [FlexItem],
        line: std::ops::Range<usize>,
        style: &Arc<ComputedStyle>,
        geometry: FlexLine,
        origin: (f32, f32),
        out: &mut Vec<Fragment>,
    ) -> PlacedLine {
        let (x, y) = origin;
        let FlexLine {
            row,
            gap,
            inner,
            cross_start,
            cross_floor,
        } = geometry;
        let count = line.len();
        if count == 0 {
            return PlacedLine {
                cross: 0.0,
                main: 0.0,
            };
        }
        let gaps = gap * (count - 1) as f32;

        // The main axis: share out what is left over, or take back what is missing.
        let used: f32 = items[line.clone()]
            .iter()
            .map(|item| item.base + item.margin_main(row))
            .sum();
        let free = inner - used - gaps;

        for item in &mut items[line.clone()] {
            item.main = item.base;
        }
        if free.is_finite() && free != 0.0 {
            let factors: f32 = items[line.clone()]
                .iter()
                .map(|item| if free > 0.0 { item.grow } else { item.shrink })
                .sum();
            if factors > 0.0 {
                for item in &mut items[line.clone()] {
                    let factor = if free > 0.0 { item.grow } else { item.shrink };
                    item.main = (item.base + free * factor / factors).max(item.floor);
                }
            }
        }

        // The cross axis: the line is as big as its largest item, and `stretch`
        // makes the rest of them match it.
        let line_cross = items[line.clone()]
            .iter()
            .map(|item| item.cross + item.margin_cross(row))
            .fold(cross_floor.unwrap_or(0.0), f32::max);

        let content_main: f32 = items[line.clone()]
            .iter()
            .map(|item| item.main + item.margin_main(row))
            .sum::<f32>()
            + gaps;
        let leftover = if inner.is_finite() {
            (inner - content_main).max(0.0)
        } else {
            0.0
        };
        let count = count as f32;
        let (leading, between) = match style.justify_content {
            JustifyContent::Start => (0.0, 0.0),
            JustifyContent::End => (leftover, 0.0),
            JustifyContent::Center => (leftover / 2.0, 0.0),
            JustifyContent::SpaceBetween if count > 1.0 => (0.0, leftover / (count - 1.0)),
            JustifyContent::SpaceBetween => (0.0, 0.0),
            JustifyContent::SpaceAround => (leftover / count / 2.0, leftover / count),
            JustifyContent::SpaceEvenly => (leftover / (count + 1.0), leftover / (count + 1.0)),
        };

        let order: Vec<usize> = if style.flex_direction.is_reverse() {
            line.clone().rev().collect()
        } else {
            line.clone().collect()
        };

        let mut cursor = leading;
        for index in order {
            let item = &items[index];
            let align = item.style.align_self.unwrap_or(style.align_items);
            let cross_size = match align {
                AlignItems::Stretch => (line_cross - item.margin_cross(row)).max(item.cross),
                _ => item.cross,
            };
            let cross_offset = cross_start
                + match align {
                    AlignItems::End => line_cross - cross_size - item.margin_cross(row),
                    AlignItems::Center => (line_cross - cross_size - item.margin_cross(row)) / 2.0,
                    // `baseline` needs a baseline to align on, which a box does not
                    // carry yet; it lays out as `start`, which is where it would be
                    // for a single line of text anyway.
                    _ => 0.0,
                };

            let (item_x, item_y, item_width, item_height) = if row {
                (
                    x + cursor + item.margin.left,
                    y + cross_offset + item.margin.top,
                    item.main,
                    cross_size,
                )
            } else {
                (
                    x + cross_offset + item.margin.left,
                    y + cursor + item.margin.top,
                    cross_size,
                    item.main,
                )
            };

            let id = item.id;
            let advance = item.main + item.margin_main(row);
            let fragment = self.layout_item(id, item_x, item_y, item_width, item_height);
            out.push(fragment);
            cursor += advance + gap + between;
        }

        PlacedLine {
            cross: line_cross,
            // What the line actually reached along the main axis: the last gap is
            // spent moving the cursor past an item that is not there.
            main: (cursor - gap - between).max(0.0),
        }
    }

    /// The widest a box would be if nothing made it wrap.
    ///
    /// CSS calls this the max-content size, and a flex item with no width of its
    /// own starts from it: `display: flex` on three words puts three words on a
    /// line, not three equal columns. Measured by asking the shaper for the
    /// paragraph's own width and by walking blocks for the widest of them.
    fn max_content_width(&mut self, id: BoxId, containing_width: f32) -> f32 {
        let node = self.tree.node(id);
        let style = Arc::clone(&node.style);
        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let extra = padding.left + padding.right + border.left + border.right;

        // A width of its own is the answer, whatever it holds.
        if let Some(width) = style.width.resolve(containing_width) {
            return width + extra;
        }

        let inner = match &node.kind {
            BoxKind::Replaced(content) => replaced_size(&style, content, containing_width).0,
            _ if node.children.is_empty() => 0.0,
            _ if self.tree.node(node.children[0]).is_inline_level() => {
                // One line, however long: the shaper is asked for the paragraph
                // with nothing to break it.
                let mut spans = Vec::new();
                let mut sources = Vec::new();
                let mut inlines = Vec::new();
                let mut replaced = Vec::new();
                self.collect_spans(
                    id,
                    containing_width,
                    &mut spans,
                    &mut sources,
                    &mut inlines,
                    &mut replaced,
                );
                let text = if spans.is_empty() {
                    0.0
                } else {
                    self.text.shape_spans(&spans, &[], None).metrics.width
                };
                let pictures: f32 = replaced.iter().map(|box_| box_.width).sum();
                text + pictures
            }
            _ => {
                let children = node.children.clone();
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.max_content_width(child, containing_width) + margin.left + margin.right
                    })
                    .fold(0.0, f32::max)
            }
        };

        inner + extra
    }

    /// The narrowest a box can be without its content spilling out of it.
    ///
    /// CSS calls this the min-content size: the widest single unbreakable thing
    /// inside, which for text is its longest word. It is what a flex item may not
    /// be shrunk below, and what a float with no width of its own shrinks to.
    /// `from_content` asks what the box's own contents need whatever width it
    /// declared, which is what a flex item's automatic minimum size is: a box that
    /// says `width: 300px` may still be shrunk, just not past its longest word.
    fn min_content_width(&mut self, id: BoxId, containing_width: f32, from_content: bool) -> f32 {
        let node = self.tree.node(id);
        let style = Arc::clone(&node.style);
        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let extra = padding.left + padding.right + border.left + border.right;

        if !from_content && let Some(width) = style.width.resolve(containing_width) {
            return width + extra;
        }

        let inner = match &node.kind {
            BoxKind::Replaced(content) => replaced_size(&style, content, containing_width).0,
            _ if node.children.is_empty() => 0.0,
            _ if self.tree.node(node.children[0]).is_inline_level() => {
                // Broken as hard as it will break: the widest line that comes back
                // is the widest word.
                let mut spans = Vec::new();
                let mut sources = Vec::new();
                let mut inlines = Vec::new();
                let mut replaced = Vec::new();
                self.collect_spans(
                    id,
                    containing_width,
                    &mut spans,
                    &mut sources,
                    &mut inlines,
                    &mut replaced,
                );
                let text = if spans.is_empty() {
                    0.0
                } else {
                    self.text
                        .shape_spans(&spans, &[], Some(0.0))
                        .lines
                        .iter()
                        .map(|line| line.width)
                        .fold(0.0, f32::max)
                };
                let pictures = replaced.iter().map(|box_| box_.width).fold(0.0, f32::max);
                text.max(pictures)
            }
            _ => {
                let children = node.children.clone();
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.min_content_width(child, containing_width, false)
                            + margin.left
                            + margin.right
                    })
                    .fold(0.0, f32::max)
            }
        };

        inner + extra
    }

    /// One flex item, laid out at the size the container decided for it.
    fn layout_item(&mut self, id: BoxId, x: f32, y: f32, width: f32, height: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A picture is its own content: the container decided its size, and there is
        // nothing inside it to lay out.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            return replaced_fragment(id, &style, content.image.clone(), x, y, width, height);
        }

        let padding = resolve_padding(&style, width);
        let border = resolve_border(&style);

        let content_width =
            (width - padding.left - padding.right - border.left - border.right).max(0.0);
        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;

        let mut children = Vec::new();
        let content_height =
            self.layout_inside(id, content_width, content_x, content_y, &mut children);

        // A height of its own is the height it gets, whatever it holds — an item
        // that overflows the size it asked for is what CSS says happens. Without
        // one, the container's figure is a floor rather than the answer: it is
        // where `stretch` put it, and content taller than that still fits.
        let outer_height = match style.height.resolve(width) {
            Some(_) => height,
            None => height
                .max(content_height + padding.top + padding.bottom + border.top + border.bottom),
        };

        Fragment {
            box_id: Some(id),
            rect: Rect::new(x, y, width, outer_height),
            kind: FragmentKind::Box,
            style,
            fixed: false,
            scroll_port: None,
            clip: None,
            sticky: None,
            layer: Layer::default(),
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
        let mut replaced = Vec::new();
        self.collect_spans(
            parent,
            width,
            &mut spans,
            &mut sources,
            &mut inlines,
            &mut replaced,
        );
        if spans.is_empty() && replaced.is_empty() {
            return 0.0;
        }
        self.level_line_heights(parent, &mut spans, &sources, &replaced);

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
                        height: 0.0,
                    },
                    Spacer {
                        id: trailing_spacer(index),
                        at: inline.last_span,
                        width: inline.border.right + inline.padding.right,
                        height: 0.0,
                    },
                ]
            })
            .chain(replaced.iter().enumerate().map(|(index, box_)| Spacer {
                id: replaced_spacer(index),
                at: box_.at,
                width: box_.width,
                height: box_.height,
            }))
            .collect();

        // Each line asks how much room the floats have left it at the height it
        // landed at, and where that room starts; the width goes to the shaper and
        // the offset is kept for placing the line.
        let mut bands: Vec<(f32, f32)> = Vec::new();
        let shaped = {
            let floats = &self.floats;
            let mut collect_band = |index: usize, top: f32| {
                let (from, to) = band_of(floats, y + top, 1.0, x, x + width);
                let available = (to - from).max(0.0);
                if bands.len() <= index {
                    bands.resize(index + 1, (0.0, width));
                }
                bands[index] = (from - x, available);
                Some(available)
            };
            self.text
                .shape_spans_wrapping(&spans, &spacers, &mut collect_band)
        };
        let style = Arc::clone(&self.tree.node(parent).style);

        // parley measures line tops from the text origin, and the first line's top
        // can sit above it by the half-leading. The paragraph's box starts where its
        // first line starts, so everything is rebased onto that.
        let paragraph_top = shaped.lines.first().map_or(0.0, |line| line.top);

        // The marker of the list item this is the first line of, if it is one.
        if let Some(marker) = self
            .pending_marker
            .take_if(|marker| (marker.x - x).abs() < 0.01)
            && let Some(line) = shaped.lines.first()
            && let Some(fragment) = self.marker_fragment(&marker, x, y, line, paragraph_top)
        {
            out.push(fragment);
        }

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
                .map_or(line.bottom - line.top, |next| next.top - line.top);
            let line_y = y + line.top - paragraph_top;
            // Alignment moves the whole line, glyphs and all: the shaper laid it
            // out from the start edge, and where that edge is is the block's
            // decision, not the paragraph's.
            // Alignment is against what the line actually had to fill, which is
            // narrower than the block wherever a float sits beside it.
            let (indent, available) = bands.get(index).copied().unwrap_or((0.0, width));
            let line_x = x
                + indent
                + match style.text_align {
                    otlyra_css::TextAlign::Start => 0.0,
                    otlyra_css::TextAlign::Center => ((available - line.width) / 2.0).max(0.0),
                    otlyra_css::TextAlign::End => (available - line.width).max(0.0),
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
                        fixed: false,
                        scroll_port: None,
                        clip: None,
                        sticky: None,
                        layer: Layer::default(),
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
                    let run_style = box_id.map_or_else(
                        || Arc::clone(&style),
                        |id| Arc::clone(&self.tree.node(id).style),
                    );

                    // `vertical-align`: the glyphs move off the line's baseline,
                    // and the room they need was already added to the line's
                    // height when its spans were levelled.
                    let shift = baseline_shift(&run_style, &style);
                    if shift != 0.0 {
                        for glyph in &mut run.glyphs {
                            glyph.y -= shift;
                        }
                    }

                    Fragment {
                        box_id,
                        rect: Rect::new(line_x + run.offset_x, line_y, run.advance, height),
                        fixed: false,
                        scroll_port: None,
                        clip: None,
                        sticky: None,
                        layer: Layer::default(),
                        kind: FragmentKind::Text(run),
                        // The run's own style, not the paragraph's: the underline
                        // on a link belongs to the link, and painting from the
                        // block's style would underline the whole paragraph or
                        // none of it.
                        style: run_style,
                        children: Vec::new(),
                    }
                })
                .collect();
            children.extend(runs);

            // The pictures that landed on this line, where the shaper put them.
            children.extend(replaced.iter().enumerate().filter_map(|(number, box_)| {
                let spacer = placed.get(&replaced_spacer(number))?;
                if spacer.line != index {
                    return None;
                }
                let image = box_.image.clone()?;
                Some(Fragment {
                    box_id: Some(box_.id),
                    rect: Rect::new(
                        line_x + spacer.x,
                        y + spacer.y - paragraph_top,
                        spacer.width,
                        spacer.height,
                    ),
                    kind: FragmentKind::Image(image),
                    style: Arc::clone(&box_.style),
                    fixed: false,
                    scroll_port: None,
                    clip: None,
                    sticky: None,
                    layer: Layer::default(),
                    children: Vec::new(),
                })
            }));

            out.push(Fragment {
                box_id: Some(parent),
                rect: Rect::new(line_x, line_y, line.width, height),
                kind: FragmentKind::Line,
                style: Arc::clone(&style),
                fixed: false,
                scroll_port: None,
                clip: None,
                sticky: None,
                layer: Layer::default(),
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

    /// Give every span of a paragraph the same line height: the tallest any of
    /// them, or the block itself, asks for — and enough room for anything a rule
    /// has raised or lowered.
    ///
    /// CSS is finer than this. A line box is as tall as the tallest thing *on that
    /// line*, and the block's own font sets a floor — the strut — that a line has
    /// even when nothing on it is that tall. So a paragraph whose middle line holds
    /// one large word should have one tall line and the rest short.
    ///
    /// Levelling them is a workaround, and it is worth stating what for. The shaper
    /// closes a run of glyphs *after* it has already moved on to the next span's
    /// style, so a run is measured with its neighbour's line height rather than its
    /// own — which makes a paragraph of ordinary text with one `<code>` in it two
    /// pixels short on every line, including the lines the `<code>` is nowhere
    /// near. One height throughout cannot be got wrong that way, and for the shape
    /// this actually happens in — a smaller inline inside ordinary prose — the
    /// floor is the answer CSS gives anyway. What it gets wrong is the opposite
    /// case: a paragraph with one larger inline in it is tall on every line rather
    /// than on the line that holds it.
    fn level_line_heights(
        &mut self,
        parent: BoxId,
        spans: &mut [TextSpan<'_>],
        sources: &[BoxId],
        replaced: &[ReplacedBox],
    ) {
        let style = Arc::clone(&self.tree.node(parent).style);
        let stack = self.font_stack(&style);
        // The strut: the line the block would have with no text in it at all.
        let Some(strut) = self.strut_of(&style, &stack) else {
            return;
        };

        // The two ends grow apart: a raised box reaches further above the baseline
        // by however far it moved plus its own ascent, and a lowered one further
        // below. A box on the baseline is already inside the strut wherever the
        // block's font is the larger, which is the ordinary case.
        let (mut above, mut below) = (strut.ascent, strut.descent);
        for (index, span) in spans.iter().enumerate() {
            let Some(source) = sources.get(index) else {
                continue;
            };
            let span_style = Arc::clone(&self.tree.node(*source).style);
            let shift = baseline_shift(&span_style, &style);
            let own = match self.strut_of(&span_style, &span.font_stack.clone()) {
                Some(own) => own,
                None => continue,
            };
            above = above.max(shift + own.ascent);
            below = below.max(own.descent - shift);
            // An explicit `line-height` on a span still asks for its own room.
            if let Some(asked) = span.line_height {
                above = above.max(asked - strut.descent);
            }
        }

        // A picture is deliberately *not* folded in. It sits with its bottom edge on
        // the baseline, so all of it is above — and one large picture would make
        // every line of the paragraph as tall as itself, which for a picture beside
        // a sentence is far more wrong than the pixel it saves. The shaper reserves
        // the room for it within the line it is actually on.
        let _ = replaced;

        let height = above + below + strut.leading;
        if height.is_finite() && height > 0.0 {
            for span in spans {
                span.line_height = Some(height);
            }
        }
    }

    /// The strut of one style: how far its font reaches above and below.
    fn strut_of(&mut self, style: &ComputedStyle, stack: &FontStack) -> Option<otlyra_text::Strut> {
        let mut strut = self
            .text
            .strut(stack, style.font_size, style.font_weight, false)?;
        // An explicit `line-height` replaces what the font asked for, split evenly
        // above and below the baseline, which is what half-leading is.
        if let otlyra_css::LineHeight::Normal = style.line_height {
            return Some(strut);
        }
        let asked = style.line_height.resolve(style.font_size, strut.height());
        let half = (asked - strut.ascent - strut.descent) / 2.0;
        strut.ascent += half;
        strut.descent += half;
        strut.leading = 0.0;
        Some(strut)
    }

    /// A list item's marker, shaped and placed against the item's first line.
    ///
    /// Outside the content box, which is what `list-style-position: outside` means
    /// and is the whole point of not making it a child: the marker hangs to the
    /// left, and the item's text — including the second line of a long item —
    /// starts at the content edge. Put inside, it pushes the first line right and
    /// the rest of them line up under the marker instead of under the words.
    ///
    /// Where it starts is two rules at once, and both are visible the moment
    /// either is missing. It **ends** one space before the content edge, so the
    /// numbers of a list line up on their full stops however many digits they have
    /// — `i`, `ii` and `iii` all end in the same column. And it **starts** at least
    /// a full em back, so a bullet, which needs far less room than that, still
    /// hangs where a reader expects rather than crowding the word beside it.
    fn marker_fragment(
        &mut self,
        marker: &PendingMarker,
        x: f32,
        y: f32,
        line: &otlyra_text::LineMetrics,
        paragraph_top: f32,
    ) -> Option<Fragment> {
        let stack = self.font_stack(&marker.style);
        let mut measure = |text: &str| {
            let mut span = span_for(text, &marker.style, stack.clone());
            // Whatever the item's first line does, the marker is one line of its own.
            span.line_height = None;
            self.text.shape_spans(&[span], &[], None)
        };
        let shaped = measure(&marker.marker.text);
        // The gap is a space set in the item's own font, which is what CSS puts
        // after a counter — measured with the space *leading*, because a shaper
        // drops a trailing one from the width it reports.
        let gap = (measure(&format!(" {}", marker.marker.text)).metrics.width
            - shaped.metrics.width)
            .max(0.0);

        let mut run = shaped.runs.into_iter().next()?;
        // A counter ends against the item's words; a bullet hangs an em back,
        // which is further than its own narrow width would put it.
        let left = if marker.marker.bullet {
            x - marker.style.font_size
        } else {
            x - gap - shaped.metrics.width
        };
        let room = x - left;
        // On the item's baseline rather than its own: a marker that sat on its own
        // baseline would ride up and down with whatever the first line happens to
        // contain.
        let baseline = line.baseline - paragraph_top;
        for glyph in &mut run.glyphs {
            glyph.y = baseline;
        }

        Some(Fragment {
            box_id: None,
            rect: Rect::new(left, y, room, line.height),
            kind: FragmentKind::Text(run),
            style: Arc::clone(&marker.style),
            fixed: false,
            scroll_port: None,
            clip: None,
            sticky: None,
            layer: Layer::default(),
            children: Vec::new(),
        })
    }

    /// The span one text box contributes, with `line-height: normal` already
    /// resolved against the font it will be set in.
    ///
    /// Resolved here rather than left to the shaper because `normal` is a browser
    /// decision about a *font*, not about a paragraph: it is the strut, and the
    /// strut comes from the block's own font whatever the runs inside it turn out
    /// to be. Answering it needs the font, so it needs the engine, so it cannot
    /// live in the plain function below.
    fn styled_span<'t>(&mut self, text: &'t str, style: &'t Arc<ComputedStyle>) -> TextSpan<'t> {
        let stack = self.font_stack(style);
        let mut span = span_for(text, style, stack.clone());
        if span.line_height.is_none() {
            span.line_height = self
                .text
                .strut(&stack, style.font_size, style.font_weight, span.italic)
                .map(otlyra_text::Strut::height);
        }
        span
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
        replaced: &mut Vec<ReplacedBox>,
    ) {
        for &child in &self.tree.node(id).children {
            let node = self.tree.node(child);
            match &node.kind {
                BoxKind::Replaced(content) => {
                    // A picture in a line is a box the shaper has to make room for,
                    // horizontally and vertically both: the line it sits in is at
                    // least as tall as it is.
                    let (width, height) = replaced_size(&node.style, content, containing_width);
                    replaced.push(ReplacedBox {
                        id: child,
                        style: Arc::clone(&node.style),
                        image: content.image.clone(),
                        at: spans.len(),
                        width,
                        height,
                    });
                }
                BoxKind::Text(text) => {
                    spans.push(self.styled_span(text, &node.style));
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
                            ..self.styled_span("", &node.style)
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

                    self.collect_spans(child, containing_width, spans, sources, inlines, replaced);

                    if let Some(slot) = slot {
                        inlines[slot].last_span = spans.len();
                    }
                }
                BoxKind::Block => {
                    // A block inside an inline context. Real CSS splits the inline
                    // around it; we do not yet, so its text joins the paragraph
                    // rather than vanishing.
                    self.collect_spans(child, containing_width, spans, sources, inlines, replaced);
                }
            }
        }
    }
}

/// The span one text box contributes.
///
/// The text is already collapsed — the box tree did it at load time — so this
/// borrows rather than copies.
fn span_for<'a>(text: &'a str, style: &'a ComputedStyle, font_stack: FontStack) -> TextSpan<'a> {
    let color = style.color.to_rgba8();
    TextSpan {
        text,
        font_stack,
        font_size: style.font_size,
        font_weight: style.font_weight,
        font_width: style.font_width,
        italic: style.font_style == otlyra_css::FontStyle::Italic,
        underline: style.text_decoration.underline,
        strikethrough: style.text_decoration.line_through,
        brush: [color.r, color.g, color.b, color.a],
        line_height: match style.line_height {
            otlyra_css::LineHeight::Normal => None,
            other => Some(other.resolve(style.font_size, style.font_size * 1.2)),
        },
        letter_spacing: style.letter_spacing,
        word_spacing: style.word_spacing,
        optical_sizing: style.optical_sizing,
        variations: &style.font_variations,
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

/// A vertical margin, which `auto` makes zero.
fn vertical_margin(margin: LengthOrAuto, containing: f32) -> f32 {
    margin.resolve(containing).unwrap_or(0.0)
}

/// Two margins that have met.
///
/// Both positive: the larger wins. Both negative: the more negative wins. One of
/// each: they add, so a negative margin pulls a box back over its neighbour by
/// exactly as much as it asks for.
fn collapse(a: f32, b: f32) -> f32 {
    if a >= 0.0 && b >= 0.0 {
        a.max(b)
    } else if a < 0.0 && b < 0.0 {
        a.min(b)
    } else {
        a + b
    }
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

    /// A picture of `width` by `height`, with no file behind it: layout reads its
    /// dimensions and never its pixels.
    fn picture(width: u32, height: u32) -> otlyra_gfx::peniko::ImageData {
        otlyra_gfx::peniko::ImageData {
            data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![
                0u8;
                width as usize
                    * height as usize
                    * 4
            ])),
            format: otlyra_gfx::peniko::ImageFormat::Rgba8,
            alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
            width,
            height,
        }
    }

    /// Lay out a document whose every `<img>` shows the same picture.
    fn laid_out_with_image(
        html: &str,
        width: f32,
        image: otlyra_gfx::peniko::ImageData,
    ) -> (FragmentTree, BoxTree) {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styles = style_document(
            &document,
            StyleViewport {
                width,
                height: 600.0,
                scale: 1.0,
            },
        );
        let images: crate::Images = crate::image_sources(&document)
            .into_iter()
            .map(|source| (source.node, image.clone()))
            .collect();
        let boxes = crate::build_box_tree_with_images(&document, Some(&styles), &images);
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

    /// The first image fragment, which is what a picture is drawn from.
    fn image_rect(tree: &FragmentTree) -> Rect {
        tree.iter()
            .find(|fragment| matches!(fragment.kind, FragmentKind::Image(_)))
            .map(|fragment| fragment.rect)
            .expect("an image fragment")
    }

    /// A picture with no size of its own in the CSS is drawn at the size it is.
    #[test]
    fn an_image_takes_its_own_size() {
        let (tree, _) = laid_out_with_image(
            "<style>body { margin: 0 }</style><p><img src=a.png></p>",
            400.0,
            picture(64, 32),
        );
        let rect = image_rect(&tree);
        assert_eq!((rect.width, rect.height), (64.0, 32.0));
    }

    /// One dimension given takes the other from the picture's own ratio, which is
    /// what keeps a photograph from being squashed by `width: 100%`.
    #[test]
    fn one_given_dimension_keeps_the_ratio() {
        let (tree, _) = laid_out_with_image(
            "<style>body { margin: 0 } img { width: 200px }</style><p><img src=a.png></p>",
            400.0,
            picture(100, 50),
        );
        let rect = image_rect(&tree);
        assert_eq!((rect.width, rect.height), (200.0, 100.0));
    }

    /// An image in a line takes room in it: the text after it starts further along
    /// and the line is at least as tall as the picture.
    #[test]
    fn an_image_takes_room_in_the_line_it_sits_in() {
        let (with, _) = laid_out_with_image(
            "<style>body { margin: 0 }</style><p>before <img src=a.png> after</p>",
            400.0,
            picture(80, 60),
        );
        let (without, _) = laid_out_with_image(
            "<style>body { margin: 0 }</style><p>before after</p>",
            400.0,
            picture(80, 60),
        );

        let line = first_line(&with);
        assert!(line.width > first_line(&without).width + 79.0);
        assert!(line.height >= 60.0, "line height was {}", line.height);
    }

    /// A picture that never arrived generates no image fragment, and the element
    /// keeps its `alt` text instead.
    #[test]
    fn a_missing_picture_leaves_the_alt_text() {
        let (tree, _) = laid_out(
            "<style>body { margin: 0 }</style><p><img src=a.png alt=\"a description\"></p>",
            400.0,
        );
        assert!(
            !tree
                .iter()
                .any(|fragment| matches!(fragment.kind, FragmentKind::Image(_)))
        );
        assert!(first_line(&tree).width > 0.0, "the alt text was laid out");
    }

    /// A float goes to its edge and the boxes after it stack as though it were not
    /// there, because it is not: it is out of the flow.
    #[test]
    fn a_float_goes_to_its_edge_and_leaves_the_flow() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 }              .f { float: right; width: 100px; height: 40px }              p { margin: 0 }</style>             <div class=f>f</div><p>text</p>",
            400.0,
        );

        let float = rect_of(&tree, &boxes, "div");
        assert_eq!(float.x, 300.0, "against the right edge");
        assert_eq!(float.y, 0.0);
        assert_eq!(
            rect_of(&tree, &boxes, "p").y,
            0.0,
            "the paragraph starts level with it"
        );
    }

    /// The lines beside a float are shortened, and the ones below it are not.
    #[test]
    fn lines_beside_a_float_are_shorter_than_the_ones_below_it() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 }              .f { float: left; width: 200px; height: 30px }              p { margin: 0 }</style>             <div class=f>f</div>             <p>alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu              nu xi omicron pi rho sigma tau upsilon phi chi psi omega</p>",
            400.0,
        );

        // The paragraph's own lines: the float has one too, and it starts at the
        // float's edge rather than beside it.
        let paragraph = boxes_of(&tree, &boxes, "p");
        let lines: Vec<Rect> = paragraph[0]
            .children
            .iter()
            .filter(|fragment| matches!(fragment.kind, FragmentKind::Line))
            .map(|fragment| fragment.rect)
            .collect();
        assert!(lines.len() > 2, "the paragraph wrapped");

        let first = lines.first().expect("a first line");
        let last = lines.last().expect("a last line");
        assert!(first.x >= 200.0, "the first line starts beside the float");
        assert!(last.x < 200.0, "and a line below it starts at the edge");
        assert!(last.width > first.width);
    }

    /// `clear` puts a box below the floats it names.
    #[test]
    fn clear_puts_a_box_below_the_float() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 }              .f { float: left; width: 100px; height: 80px }              p { margin: 0 } .c { clear: left }</style>             <div class=f>f</div><p class=c>text</p>",
            400.0,
        );
        assert_eq!(rect_of(&tree, &boxes, "p").y, 80.0);
    }

    /// Two floats on the same side sit beside each other while there is room, and
    /// the one that does not fit goes below.
    #[test]
    fn floats_stack_along_the_edge_and_then_down() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 }              div { float: left; width: 150px; height: 20px }</style>             <div>one</div><div>two</div><div>three</div>",
            400.0,
        );
        let floats = boxes_of(&tree, &boxes, "div");
        assert_eq!((floats[0].rect.x, floats[0].rect.y), (0.0, 0.0));
        assert_eq!((floats[1].rect.x, floats[1].rect.y), (150.0, 0.0));
        assert_eq!(
            (floats[2].rect.x, floats[2].rect.y),
            (0.0, 20.0),
            "the third has nowhere beside them to go"
        );
    }

    /// A float is a formatting context of its own: the floats outside it do not
    /// shorten the lines inside it.
    #[test]
    fn a_float_is_not_flowed_around_by_its_own_contents() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 }              .a { float: left; width: 200px; height: 50px }              .b { float: right; width: 100px; height: 50px }</style>             <div class=a>a</div><div class=b>b</div>",
            400.0,
        );
        let floats = boxes_of(&tree, &boxes, "div");
        let inner = floats[1]
            .children
            .iter()
            .find(|child| matches!(child.kind, FragmentKind::Line))
            .expect("the right float's own line");
        assert_eq!(
            inner.rect.x, floats[1].rect.x,
            "its text starts at its own left edge"
        );
    }

    /// A row of flex items sits along one line, in order, at the sizes the
    /// container gave them.
    #[test]
    fn flex_items_lie_along_the_main_axis() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .flex { display: flex } \
             .a { width: 100px; height: 20px } .b { width: 60px; height: 40px }</style>\
             <div class=flex><div class=a>a</div><div class=b>b</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        // The container first, then its two items.
        assert_eq!(items[1].rect.x, 0.0);
        assert_eq!(items[1].rect.width, 100.0);
        assert_eq!(items[2].rect.x, 100.0);
        assert_eq!(items[2].rect.width, 60.0);
        assert_eq!(items[1].rect.y, items[2].rect.y, "they share a line");
    }

    /// `flex-grow` shares out what is left over; `flex-shrink` takes back what is
    /// missing.
    #[test]
    fn grow_and_shrink_share_out_the_main_axis() {
        let (grown, boxes) = laid_out(
            "<style>body { margin: 0 } .flex { display: flex } \
             .a { width: 100px; flex-grow: 1 } .b { width: 100px; flex-grow: 3 }</style>\
             <div class=flex><div class=a>a</div><div class=b>b</div></div>",
            400.0,
        );
        let items = boxes_of(&grown, &boxes, "div");
        assert_eq!(items[1].rect.width, 150.0, "one quarter of the 200 spare");
        assert_eq!(items[2].rect.width, 250.0);

        let (shrunk, boxes) = laid_out(
            "<style>body { margin: 0 } .flex { display: flex } \
             .a { width: 300px } .b { width: 300px }</style>\
             <div class=flex><div class=a>a</div><div class=b>b</div></div>",
            400.0,
        );
        let items = boxes_of(&shrunk, &boxes, "div");
        assert_eq!(items[1].rect.width, 200.0, "the overflow is shared equally");
        assert_eq!(items[2].rect.width, 200.0);
    }

    /// `justify-content` decides where the leftover goes, and `align-items`
    /// what happens across the line.
    #[test]
    fn justify_and_align_place_the_items() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .flex { display: flex; justify-content: center; align-items: center; height: 100px } \
             .a { width: 100px; height: 20px }</style>\
             <div class=flex><div class=a>a</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        assert_eq!(items[1].rect.x, 150.0, "centred along the row");
        assert!(items[1].rect.y > 0.0, "and centred across it");
        assert_eq!(items[1].rect.height, 20.0, "not stretched");
    }

    /// The default is `stretch`, which is what makes columns of equal height
    /// without anyone saying how tall.
    #[test]
    fn items_stretch_to_the_tallest_of_them_by_default() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } .flex { display: flex } \
             .a { width: 50px; height: 80px } .b { width: 50px }</style>\
             <div class=flex><div class=a>a</div><div class=b>b</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        assert_eq!(items[2].rect.height, 80.0);
    }

    /// `flex-direction: column` puts the main axis down the page, and a gap goes
    /// between the items on whichever axis that is.
    #[test]
    fn a_column_stacks_its_items_with_the_gap_between_them() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .flex { display: flex; flex-direction: column; gap: 10px } \
             .a { height: 30px } .b { height: 20px }</style>\
             <div class=flex><div class=a>a</div><div class=b>b</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        assert_eq!(items[1].rect.y, 0.0);
        assert_eq!(items[2].rect.y, 40.0, "30 tall plus the 10px gap");
    }

    /// `flex-wrap: wrap` puts the items that do not fit on a line of their own,
    /// below the one before it.
    #[test]
    fn items_that_do_not_fit_wrap_onto_the_next_line() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .flex { display: flex; flex-wrap: wrap } \
             .a { width: 120px; height: 20px }</style>\
             <div class=flex><div class=a>1</div><div class=a>2</div>\
             <div class=a>3</div><div class=a>4</div></div>",
            300.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        // Two fit on a 300px line, the other two go below.
        assert_eq!(items[1].rect.y, items[2].rect.y);
        assert_eq!(items[3].rect.x, 0.0, "the third starts a line");
        assert!(items[3].rect.y >= items[1].rect.bottom());
        assert_eq!(items[3].rect.y, items[4].rect.y);
    }

    /// `relative` moves a box and nothing else: the space it left stays where it
    /// was, so its neighbours do not shift.
    #[test]
    fn relative_moves_the_box_and_not_its_neighbours() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } p { margin: 0 } \
             .moved { position: relative; left: 20px; top: 10px }</style>\
             <p class=moved>one</p><p>two</p>",
            400.0,
        );
        let paragraphs = boxes_of(&tree, &boxes, "p");
        assert_eq!(paragraphs[0].rect.x, 20.0);
        assert_eq!(paragraphs[0].rect.y, 10.0);
        assert_eq!(
            paragraphs[1].rect.y, paragraphs[0].rect.height,
            "the second is where it always was"
        );
        assert_eq!(paragraphs[1].rect.x, 0.0);
    }

    /// An absolutely positioned box leaves the flow: the boxes after it stack as
    /// though it were not there, and it is placed against its containing block.
    #[test]
    fn absolute_leaves_the_flow_and_measures_from_its_ancestor() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } p { margin: 0 } \
             .frame { position: relative; height: 200px } \
             .pinned { position: absolute; right: 10px; bottom: 20px; width: 50px; height: 30px }</style>\
             <div class=frame><p>text</p><div class=pinned>x</div></div>",
            400.0,
        );

        let pinned = boxes_of(&tree, &boxes, "div")
            .into_iter()
            .find(|fragment| fragment.rect.width == 50.0)
            .expect("the pinned box");
        assert_eq!(pinned.rect.x, 340.0, "ten from the right edge");
        assert_eq!(pinned.rect.y, 150.0, "twenty up from a 200px frame");

        let paragraph = rect_of(&tree, &boxes, "p");
        assert_eq!(paragraph.y, 0.0, "the flow did not notice it");
    }

    /// Both insets given is a width: the box stretches between them.
    #[test]
    fn two_insets_stretch_an_absolute_box_between_them() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .frame { position: relative } \
             .wide { position: absolute; left: 30px; right: 30px }</style>\
             <div class=frame><div class=wide>x</div></div>",
            400.0,
        );
        let wide = boxes_of(&tree, &boxes, "div")
            .into_iter()
            .find(|fragment| fragment.rect.x == 30.0)
            .expect("the stretched box");
        assert_eq!(wide.rect.width, 340.0);
    }

    /// A fixed box is placed against the viewport and marked as not scrolling with
    /// the page, which is what paint reads to leave it where it is.
    #[test]
    fn fixed_measures_against_the_viewport_and_does_not_scroll() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .bar { position: fixed; left: 0; bottom: 0; height: 40px; width: 100px }</style>\
             <div class=bar>bar</div>",
            400.0,
        );
        let bar = boxes_of(&tree, &boxes, "div");
        assert_eq!(bar[0].rect.y, 560.0, "forty up from a 600px viewport");
        assert!(bar[0].fixed);
        assert!(
            bar[0].children.iter().all(|child| child.fixed),
            "what is inside it does not scroll either"
        );
    }

    /// A float with no width of its own is as wide as its content, not as wide as
    /// the column it sits in.
    #[test]
    fn a_float_with_no_width_shrinks_to_fit() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } .f { float: left }</style>\
             <div class=f>short</div><p>text beside it</p>",
            400.0,
        );
        let float = rect_of(&tree, &boxes, "div");
        assert!(float.width > 0.0);
        assert!(
            float.width < 200.0,
            "a floated word took {}px of a 400px column",
            float.width
        );
    }

    /// An item may be shrunk, but not past the point where its own content spills
    /// out of it: the automatic minimum size.
    #[test]
    fn a_flex_item_is_not_shrunk_below_its_content() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } .flex { display: flex } \
             .a { width: 400px } .b { width: 400px }</style>\
             <div class=flex><div class=a>an unbreakable-looking phrase</div>\
             <div class=b>another phrase</div></div>",
            200.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        let widest_word = 20.0;
        assert!(
            items[1].rect.width > widest_word,
            "shrunk to {}px, which is past its content",
            items[1].rect.width
        );
    }

    /// A box that cuts its contents off is a formatting context of its own: a float
    /// inside it does not shorten the lines outside it.
    #[test]
    fn a_clipping_box_keeps_its_floats_to_itself() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } p { margin: 0 } \
             .card { overflow: hidden; height: 40px } \
             .f { float: left; width: 200px; height: 100px }</style>\
             <div class=card><div class=f>f</div></div><p>text</p>",
            400.0,
        );
        let paragraph = boxes_of(&tree, &boxes, "p");
        let line = paragraph[0]
            .children
            .iter()
            .find(|child| matches!(child.kind, FragmentKind::Line))
            .expect("a line");
        assert_eq!(line.rect.x, 0.0, "the float reached out of the box");
    }

    /// `fr` shares out what is left after the fixed tracks and the gaps.
    #[test]
    fn grid_columns_take_their_share_of_the_row() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: 100px 1fr 1fr; gap: 20px }</style>\
             <div class=grid><div>a</div><div>b</div><div>c</div></div>",
            520.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        // The container is first; then the three cells.
        assert_eq!(items[1].rect.width, 100.0);
        // 520 - 100 - two 20px gaps = 380, halved.
        assert_eq!(items[2].rect.width, 190.0);
        assert_eq!(items[3].rect.width, 190.0);
        assert_eq!(items[2].rect.x, 120.0, "after the first column and its gap");
        assert_eq!(items[3].rect.x, 330.0);
    }

    /// Items fill a row before starting the next one, and a row is as tall as the
    /// tallest thing in it — which is what makes a grid of cards line up.
    #[test]
    fn grid_items_wrap_into_rows_of_equal_height() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: 1fr 1fr } \
             .tall { height: 60px }</style>\
             <div class=grid><div class=tall>a</div><div>b</div><div>c</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        assert_eq!(
            items[1].rect.y, items[2].rect.y,
            "the first two share a row"
        );
        assert_eq!(
            items[2].rect.height, 60.0,
            "the shorter one is stretched to the row"
        );
        assert_eq!(items[3].rect.y, 60.0, "the third starts the next row");
        assert_eq!(items[3].rect.x, 0.0);
    }

    /// `grid-template-rows` gives a row a height of its own, whatever is in it.
    #[test]
    fn a_template_row_takes_the_height_it_asks_for() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: 1fr; \
             grid-template-rows: 80px 30px }</style>\
             <div class=grid><div>a</div><div>b</div></div>",
            300.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        assert_eq!(items[1].rect.height, 80.0);
        assert_eq!(items[2].rect.y, 80.0);
        assert_eq!(items[2].rect.height, 30.0);
    }

    /// An item can be given a line to sit on and a number of tracks to cover, and
    /// the rest are placed around it.
    #[test]
    fn a_grid_item_can_be_placed_on_a_line_and_span_tracks() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: 100px 100px 100px } \
             .wide { grid-column: span 2 } .last { grid-column: 3 }</style>\
             <div class=grid><div class=wide>wide</div><div class=last>last</div>\
             <div>after</div></div>",
            300.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        assert_eq!(items[1].rect.x, 0.0);
        assert_eq!(items[1].rect.width, 200.0, "two tracks and the gap between");
        assert_eq!(items[2].rect.x, 200.0, "the third line is the third column");
        assert_eq!(
            items[2].rect.y, items[1].rect.y,
            "and it fits on the first row"
        );
        assert_eq!(items[3].rect.x, 0.0, "the next item starts a new row");
        assert!(items[3].rect.y > items[1].rect.y);
    }

    /// Auto-placement does not go backwards: a cell left free by an item placed
    /// further along stays free, because filling it is what `grid-auto-flow: dense`
    /// is for and nobody asked for it.
    #[test]
    fn auto_placement_leaves_the_gaps_it_stepped_over() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: repeat(4, 100px) } \
             .wide { grid-column: span 2 } .last { grid-column: 4 }</style>\
             <div class=grid><div class=wide>wide</div><div class=last>last</div>\
             <div>after</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        let after = items[3].rect;
        assert_eq!(after.x, 0.0);
        assert!(
            after.y > items[1].rect.y,
            "it filled the free cell in the first row instead of starting a new one"
        );
    }

    /// `repeat(auto-fill, …)` puts in as many tracks as the container has room for,
    /// which is what makes a card grid answer to its width without a media query.
    #[test]
    fn auto_fill_puts_in_as_many_tracks_as_fit() {
        let wide = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: repeat(auto-fill, 100px) }</style>\
             <div class=grid><div>a</div><div>b</div><div>c</div></div>",
            320.0,
        );
        let items = boxes_of(&wide.0, &wide.1, "div");
        assert_eq!(items[1].rect.y, items[2].rect.y, "three fit across 320px");
        assert_eq!(items[2].rect.y, items[3].rect.y);

        let narrow = laid_out(
            "<style>body { margin: 0 } \
             .grid { display: grid; grid-template-columns: repeat(auto-fill, 100px) }</style>\
             <div class=grid><div>a</div><div>b</div><div>c</div></div>",
            220.0,
        );
        let items = boxes_of(&narrow.0, &narrow.1, "div");
        assert_eq!(items[1].rect.y, items[2].rect.y, "two fit across 220px");
        assert!(items[3].rect.y > items[1].rect.y, "and the third wraps");
    }

    /// Two margins that meet make one gap, the larger of them.
    #[test]
    fn margins_between_siblings_collapse() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } \
             .a { margin-bottom: 30px } .b { margin-top: 10px }</style>\
             <div class=a>one</div><div class=b>two</div>",
            400.0,
        );
        let divs = boxes_of(&tree, &boxes, "div");
        let gap = divs[1].rect.y - divs[0].rect.bottom();
        assert!((gap - 30.0).abs() < 0.01, "gap was {gap}");
    }

    /// A margin passes through an edge with nothing on it, and is stopped by a
    /// border or a padding.
    #[test]
    fn a_margin_escapes_an_open_edge_and_not_a_closed_one() {
        let open = laid_out(
            "<style>body { margin: 0 } section { margin: 0 } \
             p { margin: 40px 0 }</style><section><p>x</p></section>",
            400.0,
        );
        let section = rect_of(&open.0, &open.1, "section");
        let paragraph = rect_of(&open.0, &open.1, "p");
        assert_eq!(section.y, 40.0, "the child's margin moved the parent");
        assert_eq!(paragraph.y, section.y, "and they start together");

        let closed = laid_out(
            "<style>body { margin: 0 } section { margin: 0; border-top: 1px solid black } \
             p { margin: 40px 0 }</style><section><p>x</p></section>",
            400.0,
        );
        let section = rect_of(&closed.0, &closed.1, "section");
        let paragraph = rect_of(&closed.0, &closed.1, "p");
        assert_eq!(section.y, 0.0, "a border keeps the margin inside");
        assert_eq!(paragraph.y, 41.0, "below the border, by its own margin");
    }

    /// A negative margin pulls a box back over what is above it, which is what
    /// makes the two rules — larger of two positives, sum across signs — worth
    /// stating apart.
    #[test]
    fn a_negative_margin_pulls_the_next_box_up() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } .a { margin-bottom: 20px } \
             .b { margin-top: -8px }</style><div class=a>one</div><div class=b>two</div>",
            400.0,
        );
        let divs = boxes_of(&tree, &boxes, "div");
        let gap = divs[1].rect.y - divs[0].rect.bottom();
        assert!((gap - 12.0).abs() < 0.01, "gap was {gap}");
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
