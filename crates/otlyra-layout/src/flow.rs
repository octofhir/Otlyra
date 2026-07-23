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

use crate::box_tree::{BoxId, BoxKind, BoxTree, Control, ControlKind};
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
pub fn layout(tree: &mut BoxTree, text: &mut TextEngine, viewport: Viewport) -> FragmentTree {
    let _span = tracing::info_span!("layout", width = viewport.width).entered();

    let initial = Rect::new(0.0, 0.0, viewport.width, viewport.height);
    // A widget's own size needs the font, which the box tree was built without, so
    // it is settled here — into the tree, where every question about a box's size
    // already looks.
    size_widgets(tree, text);
    let tree = &*tree;
    let mut engine = Flow {
        tree,
        text,
        font_stacks: std::collections::HashMap::new(),
        line_shifts: std::collections::HashMap::new(),
        floats: Vec::new(),
        containing_blocks: vec![initial],
        viewport: initial,
        scroll_ports: Vec::new(),
        pending_marker: None,
        table_width: None,
        collapsed: slotmap::SecondaryMap::new(),
        collapsed_lines: slotmap::SecondaryMap::new(),
        measured: std::collections::HashMap::new(),
        containing_height: None,
        line_reach: (0.0, 0.0),
        span_reach: Vec::new(),
    };
    let root = tree.root();
    let mut children = Vec::new();
    let height = engine.layout_children(root, viewport.width, 0.0, 0.0, &mut children);

    let root_fragment = Fragment {
        used: None,
        box_id: Some(root),
        rect: Rect::new(0.0, 0.0, viewport.width, height.max(viewport.height)),
        kind: FragmentKind::Box,
        style: Arc::clone(&tree.node(root).style),
        widget: None,
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children,
    };

    let mut root_fragment = root_fragment;
    // Where every box sits in the painting order, which is a question about its
    // ancestors as much as about itself and so cannot be answered until they are
    // all here.
    crate::fragment::assign_paint_order(&mut root_fragment);

    tracing::debug!(height, "laid out");
    let mut fragments = FragmentTree {
        root: root_fragment,
        scroll_ports: engine.scroll_ports,
    };
    // The widget a fragment draws, filled in one pass rather than at each of the
    // dozen places a box fragment is made: a widget missing from one of them is a
    // control that is a widget everywhere except in a table cell.
    attach_widgets(&mut fragments.root, tree);
    fragments
}

/// Move a fragment and everything under it.
fn shift(fragment: &mut Fragment, dx: f32, dy: f32) {
    fragment.rect.x += dx;
    fragment.rect.y += dy;
    for child in &mut fragment.children {
        shift(child, dx, dy);
    }
}

/// Give every fragment the widget its box describes.
fn attach_widgets(fragment: &mut Fragment, tree: &BoxTree) {
    if let Some(id) = fragment.box_id
        && matches!(fragment.kind, FragmentKind::Box)
        && let Some(control) = tree.node(id).control.as_ref()
        && control.widget
    {
        fragment.widget = Some(control.clone());
    }
    for child in &mut fragment.children {
        attach_widgets(child, tree);
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
    /// What each line-relative `vertical-align` resolved to, for the paragraph
    /// being laid out.
    ///
    /// `top`, `bottom`, `middle`, `text-top` and `text-bottom` are a position
    /// within a line rather than a shift a box knows on its own, so they are
    /// settled once the line has been levelled and read back when the glyphs are
    /// placed. Working them out twice would be two answers to where a box sits.
    line_shifts: std::collections::HashMap<BoxId, f32>,
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
    /// How wide the table just laid out turned out to be.
    ///
    /// A table is shrink-to-fit: it is as wide as its columns need and no wider,
    /// however much room it was offered. Only the table's own formatting context
    /// knows that width, and only after it has measured every cell — by which time
    /// the block that holds the table has already committed to one. So it is
    /// reported back here and the block narrows itself to it.
    table_width: Option<f32>,
    /// What a box's contents needed at their narrowest and at their widest, by the
    /// box and the width it was asked about.
    ///
    /// Both answers cost a shaping pass over every word in the box, and both are
    /// asked for repeatedly: a flex line measures its items, then lays them out; a
    /// table measures every cell twice per column pass. On a page of four hundred
    /// cards that was four thousand eight hundred shaping passes, of which most
    /// were the same question asked again — measured, and it was five sixths of the
    /// time layout took.
    ///
    /// Keyed by the width because the answer depends on it: a percentage inside the
    /// box resolves against it. Cleared with the layout it belongs to, since a box
    /// tree lives no longer than that.
    measured: std::collections::HashMap<(BoxId, u32, Wanted), f32>,
    /// The height of the containing block, when it has one of its own.
    ///
    /// A percentage height is a percentage of the *height* of what holds the box,
    /// and only means anything when that height is settled without looking at the
    /// contents. Where it is not — which is most of the web, where a column is as
    /// tall as what is in it — CSS says the percentage computes to `auto`, and the
    /// box is as tall as its own contents.
    containing_height: Option<f32>,
    /// How far the paragraph being laid out reaches above and below its baseline,
    /// as its own struts and inline blocks settled it.
    ///
    /// The shaper is told how tall a line is but decides for itself where inside it
    /// the baseline sits, by centring the font. CSS does not: a line reaches as far
    /// above its baseline as its tallest thing does, and as far below as its
    /// deepest. So the line boxes are rebuilt around the baselines the shaper
    /// placed the glyphs on, which moves the boxes and leaves the text where it is.
    line_reach: (f32, f32),
    /// How far each span of the paragraph being laid out reaches above and below
    /// the baseline, in step with the spans themselves.
    ///
    /// The shaper carries a line height per *run* of glyphs and opens a run when
    /// the font changes, so it cannot be told that one span of the same font wants
    /// a taller line; and even where it can, what it is told is a height rather
    /// than where inside it the baseline goes. Both are settled here, per line,
    /// once the shaper has said which span landed on which.
    span_reach: Vec<(f32, f32)>,
    /// The lines of each collapsed table's grid, for the table to draw.
    ///
    /// A collapsed border belongs to the edge rather than to a cell, so it is
    /// drawn once, by the table, rather than half by each of the two cells that
    /// meet on it: two halves are two strokes, and where the cells disagree about
    /// the colour they would be two colours.
    collapsed_lines: slotmap::SecondaryMap<BoxId, TableLines>,
    /// The style a box is laid out and painted with when its table collapses its
    /// borders, for the tables and cells that have one.
    ///
    /// A collapsed border belongs to the edge between two cells rather than to
    /// either of them: how wide it is is decided by both, and each of them draws
    /// half. That is a used value with no property behind it, so it is carried as a
    /// style of its own rather than read back out of the box tree.
    collapsed: slotmap::SecondaryMap<BoxId, Arc<ComputedStyle>>,
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
    /// Which of this item's main-axis margins are `auto`, leading then trailing.
    fn auto_margin_sides(&self, row: bool) -> (bool, bool) {
        use otlyra_css::LengthOrAuto::Auto;
        let margin = &self.style.margin;
        if row {
            (margin.left == Auto, margin.right == Auto)
        } else {
            (margin.top == Auto, margin.bottom == Auto)
        }
    }

    /// How many of them there are, which is what the free space is split between.
    fn auto_margins_main(&self, row: bool) -> usize {
        let (lead, trail) = self.auto_margin_sides(row);
        usize::from(lead) + usize::from(trail)
    }

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
    /// An `inline-block`, already laid out, waiting to be told where its line put
    /// it. A picture has nothing here: its content is the picture.
    content: Option<Box<Fragment>>,
    /// How far below its own top the box's baseline sits.
    ///
    /// An `inline-block` sits on the line by the baseline of its *last line*, which
    /// is what makes two buttons of different heights read as one row of words
    /// rather than two boxes hung from a shelf. A picture has no baseline of its
    /// own and sits with its bottom edge on the line's, which is what this is when
    /// it is the whole height.
    baseline: f32,
    /// How far a rule has raised it off that baseline, positive upwards.
    ///
    /// A box in a line answers `vertical-align` the way a span of text does, and a
    /// bar is the reason it has to: both references set a `<progress>` a fifth of
    /// an em below the baseline, and without this it sits on it.
    shift: f32,
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

/// The room the things in a paragraph that are not text take up.
///
/// Each inline box asks for two spacers, one at each edge, carrying the border
/// and padding on that side; they reserve the room the text has to move over by,
/// and where they land is where the box starts and ends — which the shaper is
/// the only thing that knows, since it decided the lines. A replaced box asks
/// for one, the width of the box itself.
///
/// The same list is used to measure a paragraph and to lay it out, because a
/// measurement taken without them is a measurement of a different paragraph.
fn inline_spacers(inlines: &[InlineBox], replaced: &[ReplacedBox]) -> Vec<Spacer> {
    inlines
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
        // The shaper puts a spacer's bottom edge on the baseline, so what is
        // reserved is the part of the box *above* its own baseline; what hangs
        // below is added to the line's descent when the line is levelled.
        .chain(replaced.iter().enumerate().map(|(index, box_)| Spacer {
            id: replaced_spacer(index),
            at: box_.at,
            width: box_.width,
            height: box_.baseline,
        }))
        .collect()
}

/// The size a replaced box is drawn at: its *content* box, which is the picture
/// and not the frame around it.
///
/// CSS first, then whatever the content itself says, and a single given dimension
/// takes the other from the intrinsic ratio — which is what makes `width: 100%` on
/// a photograph keep its shape instead of squashing it.
///
/// `box-sizing: border-box` takes the frame out of the number the page wrote, and
/// the ratio is applied to what is left: a hundred-pixel box with a ten-pixel
/// border holds eighty pixels of picture, and a two-to-one picture is forty tall
/// rather than fifty. The presentational `width` attribute goes through the same
/// door, because it is a rule setting `width` and nothing more.
fn replaced_size(
    style: &ComputedStyle,
    content: &crate::box_tree::Replaced,
    containing: f32,
) -> (f32, f32) {
    let intrinsic = content.intrinsic;
    let ratio = intrinsic.and_then(|(width, height)| (height > 0.0).then(|| width / height));
    let padding = resolve_padding(style, containing);
    let border = resolve_border(style);

    // A stylesheet first, then the attribute that stands in for one. Either way
    // a dimension that is given takes the other from the ratio below, so naming
    // one never squashes the picture.
    let width = style
        .width
        .resolve(containing)
        .or(content.hint.0)
        .map(|width| content_from(width, style, padding, border));
    let height = style
        .height
        .resolve(containing)
        .or(content.hint.1)
        .map(|height| content_height_from(height, style, padding, border));

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

/// How much wider and taller a replaced element's border box is than its picture.
fn replaced_edges(style: &ComputedStyle, containing: f32) -> (f32, f32) {
    let padding = resolve_padding(style, containing);
    let border = resolve_border(style);
    (
        padding.left + padding.right + border.left + border.right,
        padding.top + padding.bottom + border.top + border.bottom,
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

/// Whether this fragment is the list an open control shows over the page.
///
/// Hanging the list off the control is what places it against the control, and
/// that is the whole of what it takes from it: it is not slid with the control's
/// own text, not cut off at the control's edge, and not part of what makes the
/// control a port with something to scroll.
fn is_popup(tree: &crate::box_tree::BoxTree, fragment: &Fragment) -> bool {
    fragment.box_id.is_some_and(|id| {
        let node = tree.node(id);
        node.anonymous && node.control.as_ref().is_some_and(|control| control.open)
    })
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

/// Note that this box is positioned, and at what index.
///
/// The box alone: where it lands in the painting order is a question about its
/// ancestors as well as itself, and that is settled once the tree is built.
fn mark_layer(fragment: &mut Fragment, index: i32) {
    fragment.layer = Layer::positioned(index);
}

/// Mark a fragment and everything inside it as not moving with the page.
fn mark_fixed(fragment: &mut Fragment) {
    fragment.fixed = true;
    for child in &mut fragment.children {
        mark_fixed(child);
    }
}

/// The fragment a replaced box becomes: a box the size of its border box, with
/// the picture inside it at its content box.
///
/// Two fragments rather than one, because a replaced element has a background and
/// a border of its own like any other box, and the picture is what fills the room
/// left inside them. Drawn as a single fragment the frame is neither painted nor
/// given room, which is why a page that wants one puts the picture inside
/// something else.
///
/// `x` and `y` are the border box's top left, and `width`/`height` its content.
fn replaced_fragment(
    id: BoxId,
    style: &Arc<ComputedStyle>,
    image: Option<otlyra_gfx::peniko::ImageData>,
    origin: (f32, f32),
    content: (f32, f32),
    containing: f32,
) -> Fragment {
    let ((x, y), (width, height)) = (origin, content);
    let padding = resolve_padding(style, containing);
    let border = resolve_border(style);
    let (extra_x, extra_y) = replaced_edges(style, containing);

    let picture = image.map(|image| Fragment {
        used: None,
        // The element's own box carries the hit test; a second one over the
        // picture would put two of them on the same element.
        box_id: None,
        rect: Rect::new(
            x + border.left + padding.left,
            y + border.top + padding.top,
            width,
            height,
        ),
        kind: FragmentKind::Image(image),
        style: Arc::clone(style),
        widget: None,
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children: Vec::new(),
    });

    Fragment {
        used: None,
        box_id: Some(id),
        rect: Rect::new(x, y, width + extra_x, height + extra_y),
        kind: FragmentKind::Box,
        style: Arc::clone(style),
        widget: None,
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children: picture.into_iter().collect(),
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
    // The rectangle a fragment is cut off at is in the same space its own is, so
    // it travels with it. An inline block is laid out at the origin and moved into
    // its line afterwards; a clip left behind at the origin cuts the box off where
    // the box no longer is, which is a field whose text has vanished.
    if let Some(clip) = fragment.clip.as_mut() {
        clip.x += x;
        clip.y += y;
    }
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
    baseline_shift_of(style.vertical_align, style, parent)
}

/// The same, for a value the caller has already picked out.
fn baseline_shift_of(
    align: otlyra_css::VerticalAlign,
    style: &ComputedStyle,
    parent: &ComputedStyle,
) -> f32 {
    match align {
        otlyra_css::VerticalAlign::Baseline => 0.0,
        // These five are not a shift the box knows on its own: they are a place
        // in a line whose height is not known until the boxes that *do* know
        // have been levelled. `level_line_heights` resolves them in a second
        // pass and records the answer; zero here is what a box sits at until
        // then, and what it keeps if it is never levelled at all.
        otlyra_css::VerticalAlign::Top
        | otlyra_css::VerticalAlign::Bottom
        | otlyra_css::VerticalAlign::Middle
        | otlyra_css::VerticalAlign::TextTop
        | otlyra_css::VerticalAlign::TextBottom => 0.0,
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

/// Share `available` out between columns that each want between a minimum and a
/// maximum.
///
/// The three cases CSS names, in order. Everything fits unwrapped, so every column
/// gets what it wants and the table is narrower than the room it was offered.
/// Nothing fits, so every column is squeezed to its minimum and the table overflows
/// rather than tearing words in half. Or it is in between, and the surplus over the
/// minimums is shared in proportion to how much more each column could use — which
/// is what makes a column of long prose take the room a column of dates does not.
fn share_out(minimums: &[f32], maximums: &[f32], available: f32) -> Vec<f32> {
    let wanted: f32 = maximums.iter().sum();
    if wanted <= available {
        return maximums.to_vec();
    }

    let needed: f32 = minimums.iter().sum();
    if needed >= available {
        return minimums.to_vec();
    }

    let surplus = available - needed;
    let room: f32 = minimums
        .iter()
        .zip(maximums)
        .map(|(min, max)| (max - min).max(0.0))
        .sum();
    if room <= 0.0 {
        return minimums.to_vec();
    }

    minimums
        .iter()
        .zip(maximums)
        .map(|(min, max)| min + surplus * (max - min).max(0.0) / room)
        .collect()
}

/// How a wrapped container's `align-content` shares `leftover` between `count`
/// lines: how much goes before the first, how much between each pair, and how much
/// each line grows by.
///
/// Only `stretch` — the initial value, and the reason a wrapped container's lines
/// fill it — grows the lines; the rest move them and leave them the size they are.
fn share_across(align: otlyra_css::AlignContent, leftover: f32, count: usize) -> (f32, f32, f32) {
    use otlyra_css::AlignContent;

    let lines = count.max(1) as f32;
    match align {
        AlignContent::Stretch => (0.0, 0.0, leftover / lines),
        AlignContent::Start => (0.0, 0.0, 0.0),
        AlignContent::End => (leftover, 0.0, 0.0),
        AlignContent::Center => (leftover / 2.0, 0.0, 0.0),
        // Nothing at the ends, and nothing to share where there is one line.
        AlignContent::SpaceBetween if count > 1 => (0.0, leftover / (lines - 1.0), 0.0),
        AlignContent::SpaceBetween => (0.0, 0.0, 0.0),
        AlignContent::SpaceAround => (leftover / (lines * 2.0), leftover / lines, 0.0),
        AlignContent::SpaceEvenly => (leftover / (lines + 1.0), leftover / (lines + 1.0), 0.0),
    }
}

/// Whether `edge` takes a collapsed boundary from `line`.
///
/// The contest CSS describes, in its order: `hidden` silences the boundary and
/// nothing can put a line back on it; a border that draws nothing loses to one
/// that draws something; then the wider wins; and a tie on width goes to the more
/// insistent line, which is what puts `double` over `solid` and `solid` over a
/// line with gaps in it. A tie on both is left to whoever asked first, and the
/// edges are offered cell before row before table — which is the ownership order
/// CSS finishes with.
fn wins(edge: otlyra_css::Border, line: otlyra_css::Border) -> bool {
    use otlyra_css::BorderStyle;

    /// How loud a style is, for a tie on width.
    fn rank(style: BorderStyle) -> u8 {
        match style {
            BorderStyle::Double => 7,
            BorderStyle::Solid => 6,
            BorderStyle::Dashed => 5,
            BorderStyle::Dotted => 4,
            BorderStyle::Ridge => 3,
            BorderStyle::Outset => 2,
            BorderStyle::Groove => 1,
            BorderStyle::Inset | BorderStyle::None | BorderStyle::Hidden => 0,
        }
    }

    if line.style == BorderStyle::Hidden {
        return false;
    }
    if edge.style == BorderStyle::Hidden {
        return true;
    }
    match (edge.style.draws(), line.style.draws()) {
        (false, _) => false,
        (true, false) => true,
        (true, true) if edge.width != line.width => edge.width > line.width,
        (true, true) => rank(edge.style) > rank(line.style),
    }
}

/// The resolved lines of a collapsed table's grid.
///
/// One down each side of every column, one along the top and bottom of every row
/// — the width and colour that won the contest on each, which is what is actually
/// drawn.
struct TableLines {
    /// One per square down each side, row-major: `(columns + 1)` per row.
    vertical: Vec<otlyra_css::Border>,
    /// One per square along the top and bottom, row-major: `columns` per band.
    horizontal: Vec<otlyra_css::Border>,
    columns: usize,
    rows: usize,
}

/// One drawn line of a collapsed table's grid, as a fragment of its own.
///
/// A rectangle with the line's colour behind it and no border of its own: what is
/// drawn is a line, and a line with a border on it would be two.
fn line_fragment(border: otlyra_css::Border, rect: Rect) -> Option<Fragment> {
    if !border.is_visible() || rect.width <= 0.0 || rect.height <= 0.0 {
        return None;
    }
    // Snapped to whole pixels. A collapsed line is centred on the boundary between
    // two cells, and a boundary lands wherever the columns put it — so a line one
    // pixel wide falls across two of them and is drawn as two grey ones rather than
    // one black one. Every engine snaps these, and a table of hairlines is where it
    // shows.
    let snap = |from: f32, size: f32| {
        let start = from.round();
        (start, (from + size).round() - start)
    };
    let (x, width) = snap(rect.x, rect.width);
    let (y, height) = snap(rect.y, rect.height);
    let rect = Rect::new(x, y, width.max(1.0), height.max(1.0));
    let style = ComputedStyle {
        background_color: border.color,
        border: Sides::all(otlyra_css::Border::NONE),
        ..ComputedStyle::default()
    };
    Some(Fragment {
        used: None,
        box_id: None,
        rect,
        kind: FragmentKind::Box,
        style: Arc::new(style),
        widget: None,
        fixed: false,
        scroll_port: None,
        clip: None,
        sticky: None,
        layer: Layer::default(),
        children: Vec::new(),
    })
}

/// How far below a box's top its last baseline sits, if it has one.
///
/// The last line of text in it, wherever that is: a box whose last child is a
/// paragraph sits on that paragraph's last line, which is what `inline-block`
/// alignment is defined as. `None` when there is no text in it at all.
fn baseline_of(fragment: &Fragment) -> Option<f32> {
    let mut last = None;
    let mut stack = vec![(fragment, 0.0f32)];
    while let Some((current, _)) = stack.pop() {
        if let FragmentKind::Text(run) = &current.kind
            && let Some(glyph) = run.glyphs.first()
        {
            let at = current.rect.y + glyph.y - fragment.rect.y;
            last = Some(last.map_or(at, |previous: f32| previous.max(at)));
        }
        for child in &current.children {
            // A box that has left the flow has left the line as well: its text is
            // not on the line and its baseline is not the line's. Descending into
            // one makes a drop-down's open list part of the line the drop-down sits
            // on, and the line as tall as the list.
            if child.style.position.is_out_of_flow() {
                continue;
            }
            stack.push((child, 0.0));
        }
    }
    last
}

/// Which question was asked of a box's contents, so that two of them cannot be
/// mistaken for one another in the answers already worked out.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Wanted {
    /// How wide it would be if nothing wrapped.
    Widest,
    /// How narrow it can be without spilling.
    Narrowest,
    /// The same, ignoring a width it declared — which is a flex item's automatic
    /// minimum size and a different number.
    NarrowestOfContent,
}

/// A table cell, and where in the grid it landed.
///
/// Which column a cell is in is not which child of its row it is: a cell reaching
/// down from an earlier row holds a place in this one, and the cells beside it
/// start after it.
struct Cell {
    id: BoxId,
    column: usize,
    columns: usize,
    /// Rows, resolved: never zero and never past the last row of the table.
    rows: usize,
}

/// Raise `values` until they add up to `wanted`, keeping their proportions.
///
/// What a cell reaching across several of them asks: not that any one is that
/// wide or that tall, but that between them they cover it. What they cannot cover
/// is shared out in proportion to what each already holds, so a column that was
/// wider stays the wider of the two — and evenly when none of them holds anything,
/// which is the only sensible reading of a proportion of nothing.
fn spread(values: &mut [f32], wanted: f32) {
    if values.is_empty() {
        return;
    }
    let have: f32 = values.iter().sum();
    let excess = wanted - have;
    if excess <= 0.0 {
        return;
    }
    if have > 0.0 {
        for value in values.iter_mut() {
            *value += excess * *value / have;
        }
    } else {
        let share = excess / values.len() as f32;
        for value in values.iter_mut() {
            *value += share;
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
            otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex => {
                return self.layout_flex(parent, width, x, y, out);
            }
            otlyra_css::Display::Grid => return self.layout_grid(parent, width, x, y, out),
            // A table with no rows in it is not a table; it falls through and its
            // children are stacked, which at least shows what is in them.
            otlyra_css::Display::Table => {
                if let Some(height) = self.layout_table(parent, width, x, y, out) {
                    return height;
                }
            }
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
                mark_layer(&mut fragment, style.z_index.unwrap_or(0));
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
            // `width` is the *content* box, and what is laid out is the border box:
            // a positioned box with padding on it is that much wider than the number
            // it was given, exactly as one in the flow is.
            (Some(width), _, _) => {
                let padding = resolve_padding(&style, area.width);
                let border = resolve_border(&style);
                width + padding.left + padding.right + border.left + border.right
            }
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
        mark_layer(&mut fragment, style.z_index.unwrap_or(0));
        fragment
    }

    /// A block laid out at a width the caller decided, rather than one worked out
    /// from its containing block.
    fn layout_sized(&mut self, id: BoxId, x: f32, y: f32, width: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A picture is its own content: it has no children to lay out and its
        // height comes from its own proportions rather than from anything inside it.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            // The caller decided the *outer* width, so what is left for the
            // picture is that less the frame around it.
            let (extra_x, _) = replaced_edges(&style, width);
            let (_, height) = replaced_size(&style, content, width);
            return replaced_fragment(
                id,
                &style,
                content.image.clone(),
                (x, y),
                ((width - extra_x).max(0.0), height),
                width,
            );
        }

        let padding = resolve_padding(&style, width);
        let border = resolve_border(&style);
        let content_width =
            (width - padding.left - padding.right - border.left - border.right).max(0.0);

        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;
        let mut children = Vec::new();
        self.table_width = None;
        // What the box's own contents resolve a percentage height against: its
        // height, when it has one to give.
        let outer_height = self.containing_height;
        self.containing_height = self.inner_height(&style, padding);
        let content_height =
            self.layout_inside(id, content_width, content_x, content_y, &mut children);
        self.containing_height = outer_height;
        // A box laid out at a width the caller chose keeps it, table or not — a
        // flex item is as wide as its line gave it. Taken rather than left, so a
        // table inside one does not report its width to the block outside.
        self.table_width = None;
        let content_height = clamp(
            self.asked_height(&style).unwrap_or(content_height),
            style.min_height,
            style.max_height,
            width,
        );

        // A field is one line long however much has been typed into it, so what
        // moves is the line and not the box. Before the clip, because what is slid
        // out of the box is exactly what the clip is for.
        let slid = self
            .tree
            .node(id)
            .control
            .as_ref()
            .map_or((0.0, 0.0), |control| control.scroll);
        if slid != (0.0, 0.0) {
            for child in &mut children {
                if !is_popup(self.tree, child) {
                    shift(child, -slid.0, -slid.1);
                }
            }
        }
        // A box that is inline outside cuts its contents off at its padding edge
        // like any other, and until a field slid its text under itself there was
        // nothing inside one that ever reached the edge to notice.
        if style.overflow == otlyra_css::Overflow::Clip {
            let padding_box = Rect::new(
                x + border.left,
                y + border.top,
                content_width + padding.left + padding.right,
                content_height + padding.top + padding.bottom,
            );
            for child in &mut children {
                if !is_popup(self.tree, child) {
                    set_clip(child, padding_box);
                }
            }
        }

        Fragment {
            used: None,
            box_id: Some(id),
            rect: Rect::new(
                x,
                y,
                width,
                content_height + padding.top + padding.bottom + border.top + border.bottom,
            ),
            kind: FragmentKind::Box,
            style,
            widget: None,
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
        // A box that establishes a formatting context of its own keeps what is
        // inside it inside it: a margin does not collapse out through the edge of a
        // flex item, a float, a cell, or anything that clips. Without this a
        // heading at the top of a flex item pushes the *container* down and leaves
        // a gap above the item rather than inside it.
        let establishes = style.overflow != otlyra_css::Overflow::Visible
            || style.float != otlyra_css::Float::None
            || matches!(
                style.position,
                otlyra_css::Position::Absolute | otlyra_css::Position::Fixed
            )
            || matches!(
                style.display,
                otlyra_css::Display::Flex
                    | otlyra_css::Display::InlineFlex
                    | otlyra_css::Display::Grid
                    | otlyra_css::Display::InlineBlock
                    | otlyra_css::Display::Table
                    | otlyra_css::Display::TableCell
            )
            || node.parent.is_some_and(|parent| {
                matches!(
                    self.tree.node(parent).style.display,
                    otlyra_css::Display::Flex
                        | otlyra_css::Display::InlineFlex
                        | otlyra_css::Display::Grid
                )
            });
        if establishes {
            return (false, false);
        }

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
        self.ensure_collapsed(id);
        let style = self.style_of(id);
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
            return replaced_fragment(
                id,
                &style,
                image,
                (x + margin.left, y),
                (width, height),
                containing_width,
            );
        }

        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let (margin, content_width) = resolve_horizontal(&style, containing_width, padding, border);

        let border_x = x + margin.left;
        let border_y = y;
        let content_x = border_x + border.left + padding.left;
        let content_y = border_y + border.top + padding.top;

        let mut children = Vec::new();
        self.table_width = None;
        // What this box's contents resolve a percentage height against: its own
        // height, when it has one to give them.
        let outer_height = self.containing_height;
        self.containing_height = self.inner_height(&style, padding);
        let content_height =
            self.layout_inside(id, content_width, content_x, content_y, &mut children);
        self.containing_height = outer_height;
        // A table with no width of its own is only as wide as its columns turned
        // out to need. One that names a width keeps it, and its columns were
        // stretched to fill it instead.
        let shrunk = self.table_width.take();
        let content_width = match style.width.resolve(containing_width) {
            Some(_) => content_width,
            None => shrunk.unwrap_or(content_width),
        };
        let content_height = clamp(
            self.asked_height(&style).unwrap_or(content_height),
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

        // A field is one line long however much has been typed into it, so what
        // moves is the line and not the box. Before the clip, because what is slid
        // out of the box is what the clip is for.
        let slid = self
            .tree
            .node(id)
            .control
            .as_ref()
            .map_or((0.0, 0.0), |control| control.scroll);
        if slid != (0.0, 0.0) {
            for child in &mut children {
                if !is_popup(self.tree, child) {
                    shift(child, -slid.0, -slid.1);
                }
            }
        }

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
                if !is_popup(self.tree, child) {
                    set_clip(child, padding_box);
                }
            }

            // How much there is to see: the furthest any of its contents reaches.
            // More than the box can show is what makes it a scroll port.
            let reach = children
                .iter()
                .filter(|child| !is_popup(self.tree, child))
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
            used: Some(crate::UsedEdges {
                margin,
                border,
                padding,
            }),
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
            widget: None,
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
    /// Lay out a table: rows of cells in columns sized by what is in them.
    ///
    /// This is *auto* table layout, which is the one the web is written against and
    /// the one a table with no widths on it gets. Every column is measured twice —
    /// how narrow it can be without its contents spilling, and how wide it would be
    /// if nothing wrapped — and the room is shared out between those two answers.
    /// A table is shrink-to-fit: if everything in it fits without wrapping, it is
    /// exactly that wide and no wider, which is why a two-column table of short
    /// words does not stretch across the page.
    fn layout_table(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> Option<f32> {
        let style = Arc::clone(&self.tree.node(parent).style);
        // Collapsed, the cells meet on a line rather than sitting apart on their
        // own edges, and `border-spacing` says nothing at all.
        let (spacing_x, spacing_y) = match style.border_collapse {
            otlyra_css::BorderCollapse::Collapse => (0.0, 0.0),
            otlyra_css::BorderCollapse::Separate => style.border_spacing,
        };

        let mut captions = Vec::new();
        let mut rows: Vec<BoxId> = Vec::new();
        self.collect_rows(parent, &mut captions, &mut rows);
        let cells = self.place_cells(&rows);

        let columns = cells
            .iter()
            .flatten()
            .map(|cell| cell.column + cell.columns)
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return None;
        }

        // Room for the gaps first: what is left is what the columns share.
        let gaps = spacing_x * (columns + 1) as f32;
        let available = (width - gaps).max(0.0);

        let mut minimums = vec![0.0f32; columns];
        let mut maximums = vec![0.0f32; columns];
        for cell in cells.iter().flatten().filter(|cell| cell.columns == 1) {
            let column = cell.column;
            minimums[column] =
                minimums[column].max(self.min_content_width(cell.id, available, false));
            maximums[column] = maximums[column].max(self.max_content_width(cell.id, available));
        }

        // A cell across several columns asks nothing of any one of them: it asks
        // that they add up, and only what they cannot cover between them is shared
        // out. The narrower spans go first, so a wide one sees what the spans
        // inside it have already asked for.
        let mut spanning: Vec<&Cell> = cells
            .iter()
            .flatten()
            .filter(|cell| cell.columns > 1)
            .collect();
        spanning.sort_by_key(|cell| cell.columns);
        let spanning: Vec<(std::ops::Range<usize>, f32, f32)> = spanning
            .iter()
            .map(|cell| {
                let covered = cell.column..cell.column + cell.columns;
                let between = spacing_x * (cell.columns - 1) as f32;
                (
                    covered,
                    self.min_content_width(cell.id, available, false) - between,
                    self.max_content_width(cell.id, available) - between,
                )
            })
            .collect();
        for (covered, wanted_min, wanted_max) in spanning {
            spread(&mut minimums[covered.clone()], wanted_min);
            spread(&mut maximums[covered.clone()], wanted_max);
            for column in covered {
                maximums[column] = maximums[column].max(minimums[column]);
            }
        }

        // A `<col>` says how wide its column *wants* to be, which is what a cell's
        // own width says: the column is drawn at that width where the content fits
        // in it, and at whatever the content cannot go below where it does not. So
        // the number replaces what the content wanted rather than being taken
        // alongside it, and the content's own floor still wins. Measured against a
        // reference on both halves — a column told forty wraps its text to forty,
        // and one told sixty holding a sixty-two-pixel word is sixty-two.
        let declared = self.tree.columns(parent).to_vec();
        for (column, style) in declared.iter().enumerate().take(columns) {
            let Some(asked) = style.width.resolve(available) else {
                continue;
            };
            maximums[column] = asked.max(minimums[column]);
        }

        let mut widths = share_out(&minimums, &maximums, available);
        // A table told how wide to be fills that width: the columns keep their
        // proportions and share out what is left over, rather than sitting narrow
        // in a box that was asked to be wide.
        if style.width.resolve(width).is_some() {
            let taken: f32 = widths.iter().sum();
            if taken > 0.0 && available > taken {
                let scale = available / taken;
                for column in &mut widths {
                    *column *= scale;
                }
            }
        }
        // A caption cannot be narrower than its longest word, and the table cannot
        // be narrower than its caption: a two-letter table under a one-word caption
        // is as wide as the word, with its columns stretched to fill. The floor is
        // on the table's border box, so its own edges come off it first.
        let frame = {
            let style = self.style_of(parent);
            style.border.left.width + style.border.right.width
        };
        let caption_floor = captions
            .iter()
            .map(|&caption| self.min_content_width(caption, available, false))
            .fold(0.0f32, f32::max)
            - frame;
        let taken: f32 = widths.iter().sum();
        if taken > 0.0 && caption_floor - gaps > taken {
            let scale = (caption_floor - gaps) / taken;
            for column in &mut widths {
                *column *= scale;
            }
        }

        let table_width = widths.iter().sum::<f32>() + gaps;

        let mut cursor = y;
        let mut fragments = Vec::new();

        // A caption is a block of its own, as wide as the table and above it. CSS
        // can put it below; nothing writes that.
        for caption in captions {
            let fragment = self.layout_block(caption, table_width, x, cursor);
            cursor = fragment.rect.bottom();
            fragments.push(fragment);
        }

        // Where each column starts.
        let mut offsets = Vec::with_capacity(columns);
        let mut at = x + spacing_x;
        for column in &widths {
            offsets.push(at);
            at += column + spacing_x;
        }

        // The cells are laid out before the rows have anywhere to be: how tall a
        // row is depends on what is in it, and a cell reaching into the rows below
        // depends on all of them. So they are laid out at the top and moved down
        // once the bands are known.
        let mut placed: Vec<Vec<Fragment>> = Vec::with_capacity(rows.len());
        for row in &cells {
            let mut laid = Vec::with_capacity(row.len());
            for cell in row {
                let covered = cell.column..cell.column + cell.columns;
                let cell_width =
                    widths[covered].iter().sum::<f32>() + spacing_x * (cell.columns - 1) as f32;
                laid.push(self.layout_block(cell.id, cell_width, offsets[cell.column], 0.0));
            }
            placed.push(laid);
        }

        // A row is as tall as the tallest cell that ends in it; a cell reaching
        // further down asks only that the rows it covers add up, the way a cell
        // across several columns asks it of them.
        let mut heights = vec![0.0f32; rows.len()];
        let mut reaching: Vec<(std::ops::Range<usize>, f32)> = Vec::new();
        for (index, row) in cells.iter().enumerate() {
            for (cell, fragment) in row.iter().zip(&placed[index]) {
                if cell.rows == 1 {
                    heights[index] = heights[index].max(fragment.rect.height);
                } else {
                    reaching.push((
                        index..index + cell.rows,
                        fragment.rect.height - spacing_y * (cell.rows - 1) as f32,
                    ));
                }
            }
        }
        reaching.sort_by_key(|(covered, _)| covered.len());
        for (covered, wanted) in reaching {
            spread(&mut heights[covered], wanted);
        }

        let mut tops = Vec::with_capacity(rows.len());
        for height in &heights {
            cursor += spacing_y;
            tops.push(cursor);
            cursor += height;
        }
        cursor += spacing_y;

        // A column's own background, behind the cells that sit in it and in front
        // of the table's. A column is not a box — nothing is laid out in it — so
        // this is the whole of what one draws.
        if let (Some(&first), Some((&last, &height))) =
            (tops.first(), tops.last().zip(heights.last()))
        {
            for (column, style) in declared.iter().enumerate().take(columns) {
                let paints = style.background_color.components[3] > 0.0
                    || style.backgrounds.iter().any(|layer| layer.draws());
                if !paints {
                    continue;
                }
                fragments.push(Fragment {
                    used: None,
                    box_id: None,
                    rect: Rect::new(
                        offsets[column],
                        first,
                        widths[column],
                        last + height - first,
                    ),
                    kind: FragmentKind::Box,
                    style: Arc::clone(style),
                    widget: None,
                    fixed: false,
                    scroll_port: None,
                    clip: None,
                    sticky: None,
                    layer: Layer::default(),
                    children: Vec::new(),
                });
            }
        }

        for (index, row) in rows.iter().enumerate() {
            let row_style = self.style_of(*row);
            let mut laid = std::mem::take(&mut placed[index]);

            for (cell, fragment) in cells[index].iter().zip(&mut laid) {
                offset(fragment, 0.0, tops[index]);
                // Every cell fills the band it covers: one that stopped short
                // would leave a hole in its own background.
                let covered = index..index + cell.rows;
                fragment.rect.height =
                    heights[covered].iter().sum::<f32>() + spacing_y * (cell.rows - 1) as f32;
            }

            fragments.push(Fragment {
                used: None,
                box_id: Some(*row),
                rect: Rect::new(
                    x + spacing_x,
                    tops[index],
                    table_width - spacing_x * 2.0,
                    heights[index],
                ),
                kind: FragmentKind::Box,
                style: row_style,
                widget: None,
                fixed: false,
                scroll_port: None,
                clip: None,
                sticky: None,
                layer: Layer::default(),
                children: laid,
            });
        }

        // The grid, drawn once and last: a collapsed border is the edge between two
        // cells rather than either cell's own, and the cells left room for it
        // without drawing it.
        fragments.extend(self.collapsed_grid(parent, &widths, &tops, &heights, &cells, x));

        out.extend(fragments);
        self.table_width = Some(table_width);
        Some(cursor - y)
    }

    /// The height a box asks for, if it asks for one that means anything here.
    ///
    /// A length is itself. A percentage is of the containing block's height, which
    /// most blocks on the web do not have — they are as tall as what is in them —
    /// and against one of those CSS says the percentage is `auto`. Resolving it
    /// against the width instead, which is what the same call does for every
    /// horizontal property, makes a box as tall as its parent is wide.
    fn asked_height(&self, style: &ComputedStyle) -> Option<f32> {
        let declared = match style.height {
            LengthOrAuto::Px(px) => Some(px),
            LengthOrAuto::Percent(fraction) => {
                self.containing_height.map(|height| fraction * height)
            }
            LengthOrAuto::Auto => None,
        }?;

        // Whatever the number was measured across, what layout wants is the content
        // box. A percentage is of the containing block's *content* height, so the
        // subtraction is the same either way.
        let padding = resolve_padding(style, 0.0);
        Some(content_height_from(
            declared,
            style,
            padding,
            resolve_border(style),
        ))
    }

    /// The height to resolve the percentages *inside* a box against.
    ///
    /// Its own, when it has one; otherwise nothing, because a box that is as tall
    /// as its contents cannot answer a question its contents are asking.
    fn inner_height(&self, style: &ComputedStyle, padding: Sides<f32>) -> Option<f32> {
        self.asked_height(style)
            .map(|height| (height - padding.top - padding.bottom).max(0.0))
    }

    /// The style a box is laid out with: the one it computed, or the one its table
    /// gave it when it collapsed its borders.
    fn style_of(&self, id: BoxId) -> Arc<ComputedStyle> {
        if let Some(style) = self.collapsed.get(id) {
            return Arc::clone(style);
        }
        Arc::clone(&self.tree.node(id).style)
    }

    /// Settle a collapsed table's borders before anything reads them, once.
    ///
    /// A table's own edge is one of the borders in the contest, so its box cannot
    /// be measured or placed until the contest has been held — which is why this
    /// sits at the front of everything that asks a table how wide it is.
    fn ensure_collapsed(&mut self, id: BoxId) {
        let style = &self.tree.node(id).style;
        if style.display == otlyra_css::Display::Table
            && style.border_collapse == otlyra_css::BorderCollapse::Collapse
            && !self.collapsed.contains_key(id)
        {
            self.collapse_borders(id);
        }
    }

    /// Work out a collapsed table's borders: which line is drawn on every edge of
    /// the grid, and how much room each cell has to leave for the ones it meets.
    ///
    /// With `border-collapse: collapse` a border is no longer a property of a box.
    /// It belongs to the edge between two cells: how wide it is, and what colour,
    /// is settled between the two of them — the wider wins, with the row's and the
    /// table's own edges in the running at the ends — and the line is then drawn
    /// once, by the table. Each of the two cells leaves half of it, so the two
    /// halves meet on the edge and add up to its width.
    ///
    /// Edge by edge rather than line by line: the boundary under one cell may be
    /// three pixels of one colour and one pixel of another under the cell beside
    /// it, and a table drawn a line at a time paints the loudest of them the whole
    /// way across. What a cell leaves room for is the widest edge along that side
    /// of it, so a line never needs more room than the cells gave it.
    ///
    /// The width is the whole of the contest here. CSS breaks a tie by border
    /// style and then by who owns the edge; ties are left to the first of the two,
    /// and `<col>`, which is one of the owners, generates no box for us to ask.
    fn collapse_borders(&mut self, table: BoxId) {
        let mut captions = Vec::new();
        let mut rows: Vec<BoxId> = Vec::new();
        self.collect_rows(table, &mut captions, &mut rows);
        let cells = self.place_cells(&rows);
        let columns = cells
            .iter()
            .flatten()
            .map(|cell| cell.column + cell.columns)
            .max()
            .unwrap_or(0);
        if columns == 0 || rows.is_empty() {
            return;
        }

        // Which cell holds each square of the grid, so that the two boxes meeting
        // on an edge can be asked what they wanted of it.
        let mut owner: Vec<Option<BoxId>> = vec![None; rows.len() * columns];
        for (index, row) in cells.iter().enumerate() {
            for cell in row {
                for band in index..index + cell.rows {
                    for column in cell.column..cell.column + cell.columns {
                        owner[band * columns + column] = Some(cell.id);
                    }
                }
            }
        }

        let own = Arc::clone(&self.tree.node(table).style);
        let widest = |line: &mut otlyra_css::Border, edge: otlyra_css::Border| {
            if wins(edge, *line) {
                *line = edge;
            }
        };
        let border_of = |id: Option<BoxId>| id.map(|id| &self.tree.node(id).style.border);

        // Every edge of the grid: one down each side of every square, one along the
        // top and bottom of each.
        let mut vertical = vec![otlyra_css::Border::NONE; (columns + 1) * rows.len()];
        let mut horizontal = vec![otlyra_css::Border::NONE; columns * (rows.len() + 1)];

        for band in 0..rows.len() {
            let row_style = Arc::clone(&self.tree.node(rows[band]).style);
            for column in 0..=columns {
                let edge = &mut vertical[band * (columns + 1) + column];
                if column > 0
                    && let Some(border) = border_of(owner[band * columns + column - 1])
                {
                    widest(edge, border.right);
                }
                if column < columns
                    && let Some(border) = border_of(owner[band * columns + column])
                {
                    widest(edge, border.left);
                }
                if column == 0 {
                    widest(edge, row_style.border.left);
                    widest(edge, own.border.left);
                }
                if column == columns {
                    widest(edge, row_style.border.right);
                    widest(edge, own.border.right);
                }
            }
        }

        for band in 0..=rows.len() {
            for column in 0..columns {
                let edge = &mut horizontal[band * columns + column];
                if band > 0 {
                    if let Some(border) = border_of(owner[(band - 1) * columns + column]) {
                        widest(edge, border.bottom);
                    }
                    widest(edge, self.tree.node(rows[band - 1]).style.border.bottom);
                }
                if band < rows.len() {
                    if let Some(border) = border_of(owner[band * columns + column]) {
                        widest(edge, border.top);
                    }
                    widest(edge, self.tree.node(rows[band]).style.border.top);
                }
                if band == 0 {
                    widest(edge, own.border.top);
                }
                if band == rows.len() {
                    widest(edge, own.border.bottom);
                }
            }
        }

        // What a box leaves room for on one of its sides: half of the widest edge
        // along it, which is what makes the halves on either side of every edge add
        // up to the line drawn on it.
        let widest_vertical = |column: usize, bands: std::ops::Range<usize>| -> f32 {
            bands
                .map(|band| vertical[band * (columns + 1) + column].width)
                .fold(0.0, f32::max)
                / 2.0
        };
        let widest_horizontal = |band: usize, span: std::ops::Range<usize>| -> f32 {
            span.map(|column| horizontal[band * columns + column].width)
                .fold(0.0, f32::max)
                / 2.0
        };
        let room = |width: f32| otlyra_css::Border {
            width,
            // The line is drawn by the table, once. What is left on a box is the
            // room for it and nothing to see.
            color: otlyra_gfx::peniko::Color::TRANSPARENT,
            style: otlyra_css::BorderStyle::Solid,
        };

        for (index, row) in cells.iter().enumerate() {
            for cell in row {
                let bands = index..index + cell.rows;
                let span = cell.column..cell.column + cell.columns;
                let mut style = (*self.tree.node(cell.id).style).clone();
                style.border = Sides {
                    top: room(widest_horizontal(index, span.clone())),
                    right: room(widest_vertical(cell.column + cell.columns, bands.clone())),
                    bottom: room(widest_horizontal(index + cell.rows, span)),
                    left: room(widest_vertical(cell.column, bands)),
                };
                self.collapsed.insert(cell.id, Arc::new(style));
            }
        }

        let mut style = (*own).clone();
        style.border = Sides {
            top: room(widest_horizontal(0, 0..columns)),
            right: room(widest_vertical(columns, 0..rows.len())),
            bottom: room(widest_horizontal(rows.len(), 0..columns)),
            left: room(widest_vertical(0, 0..rows.len())),
        };
        self.collapsed.insert(table, Arc::new(style));

        // A row's own border went into the edges above and below it and is drawn
        // there; drawn again on the row it would be drawn twice.
        for &row in &rows {
            let mut style = (*self.tree.node(row).style).clone();
            style.border = Sides::all(otlyra_css::Border::NONE);
            self.collapsed.insert(row, Arc::new(style));
        }

        self.collapsed_lines.insert(
            table,
            TableLines {
                vertical,
                horizontal,
                columns,
                rows: rows.len(),
            },
        );
    }

    /// The lines of a collapsed table, as fragments the table draws itself.
    ///
    /// Edge by edge, because a cell that reaches across a boundary is not divided
    /// by it — the line inside a `colspan` is not drawn, and neither is the one
    /// inside a `rowspan` — and because two edges of one boundary may have been
    /// won by different borders. Neighbouring edges that agree become one
    /// rectangle, so an ordinary table is a handful of fragments rather than one
    /// per cell edge.
    ///
    /// Each line is centred on the boundary, which is where the cells left half of
    /// it on either side; the ends reach half of the crossing line, so the corners
    /// are filled rather than notched.
    fn collapsed_grid(
        &self,
        table: BoxId,
        widths: &[f32],
        tops: &[f32],
        heights: &[f32],
        cells: &[Vec<Cell>],
        x: f32,
    ) -> Vec<Fragment> {
        let Some(lines) = self.collapsed_lines.get(table) else {
            return Vec::new();
        };
        let (columns, rows) = (lines.columns, lines.rows);
        if rows == 0 || widths.len() < columns || heights.len() < rows {
            return Vec::new();
        }

        // Which boundaries a cell reaches across, and so which are not drawn.
        let mut crossed_vertical = vec![false; (columns + 1) * rows];
        let mut crossed_horizontal = vec![false; columns * (rows + 1)];
        for (index, row) in cells.iter().enumerate() {
            for cell in row {
                for column in cell.column + 1..cell.column + cell.columns {
                    for band in index..index + cell.rows {
                        crossed_vertical[band * (columns + 1) + column] = true;
                    }
                }
                for band in index + 1..index + cell.rows {
                    for column in cell.column..cell.column + cell.columns {
                        crossed_horizontal[band * columns + column] = true;
                    }
                }
            }
        }

        // Where each boundary sits: the cells tile without gaps, so a column
        // boundary is where the previous column ended.
        let mut down = Vec::with_capacity(columns + 1);
        let mut at = x;
        for width in widths.iter().take(columns) {
            down.push(at);
            at += width;
        }
        down.push(at);

        let mut across: Vec<f32> = tops.iter().take(rows).copied().collect();
        across.push(tops[rows - 1] + heights[rows - 1]);

        // Half of the widest edge on a crossing boundary, which is how far a line
        // reaches past the square it belongs to.
        let vertical_reach = |band: usize| -> f32 {
            (0..=columns)
                .map(|column| lines.vertical[band.min(rows - 1) * (columns + 1) + column].width)
                .fold(0.0, f32::max)
                / 2.0
        };
        let horizontal_reach = |column: usize| -> f32 {
            (0..=rows)
                .map(|band| lines.horizontal[band * columns + column.min(columns - 1)].width)
                .fold(0.0, f32::max)
                / 2.0
        };

        let mut out = Vec::new();

        for column in 0..=columns {
            let mut band = 0;
            while band < rows {
                let edge = lines.vertical[band * (columns + 1) + column];
                if crossed_vertical[band * (columns + 1) + column] || !edge.is_visible() {
                    band += 1;
                    continue;
                }
                let start = band;
                while band < rows
                    && !crossed_vertical[band * (columns + 1) + column]
                    && lines.vertical[band * (columns + 1) + column] == edge
                {
                    band += 1;
                }
                let top = across[start] - horizontal_reach(column);
                let bottom = across[band] + horizontal_reach(column);
                out.extend(line_fragment(
                    edge,
                    Rect::new(
                        down[column] - edge.width / 2.0,
                        top,
                        edge.width,
                        bottom - top,
                    ),
                ));
            }
        }

        for band in 0..=rows {
            let mut column = 0;
            while column < columns {
                let edge = lines.horizontal[band * columns + column];
                if crossed_horizontal[band * columns + column] || !edge.is_visible() {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < columns
                    && !crossed_horizontal[band * columns + column]
                    && lines.horizontal[band * columns + column] == edge
                {
                    column += 1;
                }
                let left = down[start] - vertical_reach(band);
                let right = down[column] + vertical_reach(band);
                out.extend(line_fragment(
                    edge,
                    Rect::new(
                        left,
                        across[band] - edge.width / 2.0,
                        right - left,
                        edge.width,
                    ),
                ));
            }
        }

        out
    }

    /// Put every cell somewhere in the table's grid, row by row.
    ///
    /// A cell goes in the first column its row has left, which is what makes a
    /// cell reaching down from an earlier row push the ones beside it along: the
    /// cells it covers are spoken for before this row is read. A `rowspan` of zero
    /// is HTML's "the rest of them", which is only knowable here.
    fn place_cells(&self, rows: &[BoxId]) -> Vec<Vec<Cell>> {
        let mut taken: Vec<Vec<bool>> = vec![Vec::new(); rows.len()];
        let mut placed = Vec::with_capacity(rows.len());

        for (index, &row) in rows.iter().enumerate() {
            let mut cells = Vec::new();
            let mut column = 0;

            for &id in &self.tree.node(row).children {
                if self.tree.node(id).style.display != otlyra_css::Display::TableCell {
                    continue;
                }
                while taken[index].get(column).copied().unwrap_or(false) {
                    column += 1;
                }

                let span = self.tree.span(id);
                let down = match span.rows {
                    0 => rows.len() - index,
                    wanted => wanted.min(rows.len() - index),
                };
                for band in &mut taken[index..index + down] {
                    if band.len() < column + span.columns {
                        band.resize(column + span.columns, false);
                    }
                    band[column..column + span.columns].fill(true);
                }

                cells.push(Cell {
                    id,
                    column,
                    columns: span.columns,
                    rows: down,
                });
                column += span.columns;
            }

            placed.push(cells);
        }

        placed
    }

    /// Walk a table's children for its captions and its rows, through whatever row
    /// groups it has.
    fn collect_rows(&self, parent: BoxId, captions: &mut Vec<BoxId>, rows: &mut Vec<BoxId>) {
        for &child in &self.tree.node(parent).children {
            match self.tree.node(child).style.display {
                otlyra_css::Display::TableCaption => captions.push(child),
                otlyra_css::Display::TableRow => rows.push(child),
                otlyra_css::Display::TableRowGroup => self.collect_rows(child, captions, rows),
                _ => {}
            }
        }
    }

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

        // `order` reorders the items and nothing else: it is a visual arrangement,
        // and the document order is what a screen reader and a copy still read. A
        // stable sort, so items that name the same order keep the order they were
        // written in — which is what CSS says and is the whole difference between
        // `order` and a shuffle.
        if items.iter().any(|item| item.style.order != 0) {
            items.sort_by_key(|item| item.style.order);
        }

        // A container with a height of its own has a definite cross size when it
        // is a row and a definite main size when it is a column: either way it is
        // the size the items are fitted into rather than one they add up to.
        let definite_height = self
            .asked_height(&style)
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

        let line_count = lines.len();
        let cross_gap = if row { style.gap.0.resolve(width) } else { gap };
        // The cross size the container has to give out, and what the lines want of
        // it. A row's is its own height, where it has one; a column's is always its
        // width.
        let container_cross = if row { definite_height } else { Some(width) };
        let wanted: Vec<f32> = lines
            .iter()
            .map(|line| {
                items[line.clone()]
                    .iter()
                    .map(|item| item.cross + item.margin_cross(row))
                    .fold(0.0f32, f32::max)
            })
            .collect();
        let leftover = container_cross.map_or(0.0, |cross| {
            (cross - wanted.iter().sum::<f32>() - cross_gap * (line_count - 1) as f32).max(0.0)
        });

        // A container that cannot wrap has one line, and that line fills whatever
        // cross size the container has — which is what makes `align-items: center`
        // centre against the container rather than against the tallest item.
        // `align-content` says nothing about such a container, so it is not asked.
        let unwrapped = style.flex_wrap == FlexWrap::NoWrap;
        let (lead, between, stretch) = if unwrapped {
            (0.0, 0.0, leftover)
        } else {
            share_across(style.align_content, leftover, line_count)
        };

        let mut cross_cursor = lead;
        let mut main_extent = 0.0f32;
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
                    cross_floor: (container_cross.is_some()).then(|| wanted[number] + stretch),
                },
                (x, y),
                out,
            );
            cross_cursor += placed.cross;
            main_extent = main_extent.max(placed.main);
            if number + 1 < line_count {
                cross_cursor += cross_gap + between;
            }
        }
        // The room the lines were told to leave after them is still room they take.
        if container_cross.is_some() {
            cross_cursor += lead;
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
        // Auto margins eat the free space before `justify-content` sees any.
        // That is what `margin-right: auto` on one item in a row is for — it is
        // how a brand is pushed left and a nav right — and a container that
        // handed the leftover to `justify-content` instead would leave both of
        // them bunched at the start, which is what it did.
        let autos: usize = items[line.clone()]
            .iter()
            .map(|item| item.auto_margins_main(row))
            .sum();
        let per_auto = if autos > 0 && leftover > 0.0 {
            leftover / autos as f32
        } else {
            0.0
        };
        let leftover = if autos > 0 { 0.0 } else { leftover };

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
            // `stretch` fills the line with an item that has no size of its own
            // across it. An item that named one keeps it: `align-items: stretch`
            // is the initial value, so stretching over a declared height would
            // make every `height` in a flex container a suggestion.
            let definite = if row {
                self.asked_height(&item.style).is_some()
            } else {
                item.style.width.resolve(inner).is_some()
            };
            let cross_size = match align {
                AlignItems::Stretch if !definite => {
                    (line_cross - item.margin_cross(row)).max(item.cross)
                }
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

            // An `auto` margin on the leading side pushes this item along; one on
            // the trailing side pushes everything after it.
            let (lead_auto, trail_auto) = item.auto_margin_sides(row);
            let lead = if lead_auto { per_auto } else { 0.0 };
            let trail = if trail_auto { per_auto } else { 0.0 };

            let (item_x, item_y, item_width, item_height) = if row {
                (
                    x + cursor + item.margin.left + lead,
                    y + cross_offset + item.margin.top,
                    item.main,
                    cross_size,
                )
            } else {
                (
                    x + cross_offset + item.margin.left,
                    y + cursor + item.margin.top + lead,
                    cross_size,
                    item.main,
                )
            };

            let id = item.id;
            let advance = item.main + item.margin_main(row) + lead + trail;
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
        let key = (id, containing_width.to_bits(), Wanted::Widest);
        if let Some(&answer) = self.measured.get(&key) {
            return answer;
        }
        let answer = self.max_content_width_uncached(id, containing_width);
        self.measured.insert(key, answer);
        answer
    }

    fn max_content_width_uncached(&mut self, id: BoxId, containing_width: f32) -> f32 {
        self.ensure_collapsed(id);
        let node = self.tree.node(id);
        let style = self.style_of(id);
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
            // A row of flex items is as wide as its items laid side by side, plus
            // the gaps. The block branch below takes the widest of them, which is
            // right for boxes that stack and wrong for boxes that sit in a row —
            // and a flex item is blockified, so it never reaches the inline branch
            // that would have summed it. A logo beside a wordmark came out as wide
            // as the wordmark alone, and the wordmark was drawn over what came next.
            _ if matches!(
                style.display,
                otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex
            ) && style.flex_direction.is_row()
                && style.flex_wrap == otlyra_css::FlexWrap::NoWrap =>
            {
                let children = node.children.clone();
                let gaps =
                    style.gap.1.resolve(containing_width) * children.len().saturating_sub(1) as f32;
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.max_content_width(child, containing_width) + margin.left + margin.right
                    })
                    .sum::<f32>()
                    + gaps
            }
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
                // Shaped with the spacers rather than measured without them and
                // added on afterwards: the width of a run of text is not the sum
                // of its pieces once something that is not text sits in it. A
                // space between two pictures is trailing white space at the end
                // of the *text* and no space at all at the end of the run, and a
                // paragraph measured the other way came back narrower than the
                // one line it holds — which put the second picture on a line of
                // its own.
                let spacers = inline_spacers(&inlines, &replaced);
                if spans.is_empty() && spacers.is_empty() {
                    0.0
                } else {
                    self.text.shape_spans(&spans, &spacers, None).metrics.width
                }
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
        let key = (
            id,
            containing_width.to_bits(),
            if from_content {
                Wanted::NarrowestOfContent
            } else {
                Wanted::Narrowest
            },
        );
        if let Some(&answer) = self.measured.get(&key) {
            return answer;
        }
        let answer = self.min_content_width_uncached(id, containing_width, from_content);
        self.measured.insert(key, answer);
        answer
    }

    fn min_content_width_uncached(
        &mut self,
        id: BoxId,
        containing_width: f32,
        from_content: bool,
    ) -> f32 {
        self.ensure_collapsed(id);
        let node = self.tree.node(id);
        let style = self.style_of(id);
        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let extra = padding.left + padding.right + border.left + border.right;

        if !from_content && let Some(width) = style.width.resolve(containing_width) {
            return width + extra;
        }

        let inner = match &node.kind {
            BoxKind::Replaced(content) => replaced_size(&style, content, containing_width).0,
            _ if node.children.is_empty() => 0.0,
            // A row of flex items is one unbreakable run: the items sit beside
            // each other and no shrinking moves one below another, so what the
            // row needs at its narrowest is the sum of what its items need plus
            // the gaps between them. Falling through to the inline branch below
            // takes the *widest* item instead, which for a mark beside a word is
            // the wider of the two rather than the two of them — and the item is
            // then floored at a width its own contents overflow. That is what cut
            // the site's wordmark off beside its logo.
            _ if matches!(
                style.display,
                otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex
            ) && style.flex_direction.is_row()
                && style.flex_wrap == otlyra_css::FlexWrap::NoWrap =>
            {
                let children = node.children.clone();
                let gaps =
                    style.gap.1.resolve(containing_width) * children.len().saturating_sub(1) as f32;
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.min_content_width(child, containing_width, false)
                            + margin.left
                            + margin.right
                    })
                    .sum::<f32>()
                    + gaps
            }
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
                // Broken as hard as it will break — unless it may not break at
                // all. Under `text-wrap-mode: nowrap` the whole run is one
                // unbreakable thing, so its min-content size is its full width;
                // asking for the longest word instead would let a flex item
                // shrink to that word while the text it draws stays full length,
                // and the item beside it would be laid over the overflow. That is
                // what folded and then overlapped the site's own header.
                let wrap_at = (style.text_wrap != otlyra_css::TextWrap::NoWrap).then_some(0.0);
                let text = if spans.is_empty() {
                    0.0
                } else {
                    self.text
                        .shape_spans(&spans, &[], wrap_at)
                        .lines
                        .iter()
                        .map(|line| line.width - line.trailing_space)
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
            // The container decided the outer size; the frame comes out of it.
            let (extra_x, extra_y) = replaced_edges(&style, width);
            return replaced_fragment(
                id,
                &style,
                content.image.clone(),
                (x, y),
                ((width - extra_x).max(0.0), (height - extra_y).max(0.0)),
                width,
            );
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
        let outer_height = match self.asked_height(&style) {
            Some(_) => height,
            None => height
                .max(content_height + padding.top + padding.bottom + border.top + border.bottom),
        };

        Fragment {
            used: Some(crate::UsedEdges {
                margin: resolve_margin(&style, width),
                border,
                padding,
            }),
            box_id: Some(id),
            rect: Rect::new(x, y, width, outer_height),
            kind: FragmentKind::Box,
            style,
            widget: None,
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
    /// See [`Layout::inline_spacers`] for the room the things that are not text
    /// take in it.
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

        let spacers = inline_spacers(&inlines, &replaced);

        // Each line asks how much room the floats have left it at the height it
        // landed at, and where that room starts; the width goes to the shaper and
        // the offset is kept for placing the line.
        // `text-wrap-mode: nowrap` — which is half of what `white-space: nowrap`
        // means — is a line that may not be broken however narrow the box is. No
        // width offered to the shaper is exactly that: it lays the run out on one
        // line and lets it overflow, which is what the property asks for.
        let wraps = self.tree.node(parent).style.text_wrap != otlyra_css::TextWrap::NoWrap;
        let mut bands: Vec<(f32, f32)> = Vec::new();
        let mut shaped = {
            let floats = &self.floats;
            let mut collect_band = |index: usize, top: f32| {
                let (from, to) = band_of(floats, y + top, 1.0, x, x + width);
                let available = (to - from).max(0.0);
                if bands.len() <= index {
                    bands.resize(index + 1, (0.0, width));
                }
                bands[index] = (from - x, available);
                wraps.then_some(available)
            };
            self.text
                .shape_spans_wrapping(&spans, &spacers, &mut collect_band)
        };
        let style = Arc::clone(&self.tree.node(parent).style);

        // The line boxes, put where CSS puts them: as far above each baseline as the
        // paragraph reaches and as far below. The shaper centres the font inside the
        // height it was given instead, which is the same thing for a line of plain
        // text and is not for one holding a box taller than the words beside it.
        // The line boxes, put where CSS puts them: as tall as what is *on* each
        // line, with the baseline as far down as the tallest thing on it reaches.
        //
        // The shaper answers neither question. It carries a line height per run of
        // glyphs and opens a run when the font changes, so a span that wants a
        // taller line without changing font cannot be told to it at all; and where
        // it can, it centres the font inside the height rather than putting the
        // baseline where the tallest thing on the line needs it. So the paragraph
        // is restacked here, from what actually landed on each line.
        let strut = self.line_reach;
        // What the shaper made of each line, kept before anything is moved: it is
        // the answer for a line holding a picture, whose ink reaches past the box
        // the font would have given it.
        let shaper: Vec<(f32, f32)> = shaped
            .lines
            .iter()
            .map(|line| (line.baseline - line.top, line.bottom - line.baseline))
            .collect();
        // A line holding a picture is the shaper's to measure: a picture stands on
        // the baseline with all of its height above, the text beside it hangs
        // below, and the shaper has already put the box around both. Every other
        // line starts at the block's own strut.
        let mut reach: Vec<(f32, f32)> = {
            let mut holds_picture = vec![false; shaped.lines.len()];
            for spacer in &shaped.spacers {
                let picture = replaced.iter().enumerate().any(|(index, box_)| {
                    replaced_spacer(index) == spacer.id && box_.content.is_none()
                });
                if picture && let Some(slot) = holds_picture.get_mut(spacer.line) {
                    *slot = true;
                }
            }
            holds_picture
                .into_iter()
                .enumerate()
                .map(|(index, picture)| {
                    if picture {
                        shaper.get(index).copied().unwrap_or(strut)
                    } else {
                        strut
                    }
                })
                .collect()
        };
        {
            // Which bytes of the shaped text each span covers. The shaper lays the
            // spans end to end, so this is a running total of their lengths.
            let mut starts = Vec::with_capacity(spans.len() + 1);
            let mut at = 0usize;
            for span in &spans {
                starts.push(at);
                at += span.text.len();
            }
            starts.push(at);

            for run in &shaped.runs {
                let Some(line) = reach.get_mut(run.line) else {
                    continue;
                };
                for (index, window) in starts.windows(2).enumerate() {
                    // A span is on this line if any of its bytes were drawn there.
                    if window[1] <= run.text_range.start || window[0] >= run.text_range.end {
                        continue;
                    }
                    let Some(&(above, below)) = self.span_reach.get(index) else {
                        continue;
                    };
                    line.0 = line.0.max(above);
                    line.1 = line.1.max(below);
                }
            }

            // And the boxes in the line, which the shaper placed but did not stack.
            // A picture sits with its bottom edge on the baseline, so all of it is
            // above; an inline block sits on its own last baseline and is held
            // above and below that.
            for spacer in &shaped.spacers {
                let Some(line) = reach.get_mut(spacer.line) else {
                    continue;
                };
                let box_ = replaced
                    .iter()
                    .enumerate()
                    .find(|(index, _)| replaced_spacer(*index) == spacer.id)
                    .map(|(_, box_)| box_);
                match box_ {
                    // The margin box asks for the room, not the border box: a
                    // slider with two pixels above and below it makes the line it
                    // is in four pixels taller, which is what both references do
                    // and the difference between a row of controls sitting in
                    // their line and sitting through it.
                    Some(box_) if box_.content.is_some() => {
                        let margin = resolve_margin(&box_.style, 0.0);
                        line.0 = line.0.max(box_.baseline + box_.shift + margin.top);
                        line.1 = line
                            .1
                            .max(box_.height - box_.baseline - box_.shift + margin.bottom);
                    }
                    Some(box_) => {
                        line.0 = line.0.max(spacer.height + box_.shift);
                        line.1 = line.1.max(-box_.shift);
                    }
                    None => {}
                }
            }
        }

        // Restack: every line ends where the next begins, and the glyphs on it move
        // with the baseline they sit on.
        let mut cursor = shaped.lines.first().map_or(0.0, |line| line.top);
        let mut shifts: Vec<f32> = Vec::with_capacity(shaped.lines.len());
        for (index, line) in shaped.lines.iter_mut().enumerate() {
            let (above, below) = reach.get(index).copied().unwrap_or(strut);
            let baseline = cursor + above;
            shifts.push(baseline - line.baseline);
            line.top = cursor;
            line.baseline = baseline;
            line.height = above + below;
            line.bottom = cursor + line.height;
            cursor = line.bottom;
        }
        for run in &mut shaped.runs {
            let shift = shifts.get(run.line).copied().unwrap_or(0.0);
            for glyph in &mut run.glyphs {
                glyph.y += shift;
            }
        }
        for spacer in &mut shaped.spacers {
            spacer.y += shifts.get(spacer.line).copied().unwrap_or(0.0);
        }
        shaped.metrics.first_baseline = shaped.lines.first().map_or(0.0, |line| line.baseline);
        // The paragraph reaches from the top of its first line to the bottom of
        // its last, which is what its own box is.
        if let (Some(first), Some(last)) = (shaped.lines.first(), shaped.lines.last()) {
            shaped.metrics.height = last.bottom - first.top;
        }

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

                    // The same shift its text got. An inline box drawn on the
                    // baseline while its own glyphs sat somewhere else was a
                    // background that missed the words it was behind — which is
                    // what a `vertical-align` on a span with a background looks
                    // like when only half of it moves.
                    let shift = self
                        .line_shifts
                        .get(&inline.id)
                        .copied()
                        .unwrap_or_else(|| baseline_shift(&inline.style, &style));

                    Some(Fragment {
                        used: None,
                        box_id: Some(inline.id),
                        // Vertical padding and a horizontal border spill outside
                        // the line box without making it taller: an inline box does
                        // not push its neighbours apart vertically.
                        rect: Rect::new(
                            line_x + left,
                            line_y - shift - inline.border.top - inline.padding.top,
                            (right - left).max(0.0),
                            height
                                + inline.border.top
                                + inline.padding.top
                                + inline.border.bottom
                                + inline.padding.bottom,
                        ),
                        kind: FragmentKind::Box,
                        style,
                        widget: None,
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
                    // Resolved once, in the levelling pass, for the five values
                    // that need the line box; worked out here for the rest,
                    // which need only the two fonts.
                    let shift = box_id
                        .and_then(|id| self.line_shifts.get(&id).copied())
                        .unwrap_or_else(|| baseline_shift(&run_style, &style));
                    // The glyphs are placed relative to the fragment, so moving
                    // the fragment moves them with it. Moving both was moving
                    // everything twice as far as it was asked to go.

                    Fragment {
                        used: None,
                        box_id,
                        // The fragment moves with its glyphs. Shifting only the
                        // glyphs left the background, the underline and the
                        // highlight behind on the baseline — invisible on a
                        // `super` that moves three pixels, and unmissable on a
                        // `text-top` span set larger than the line it is in.
                        rect: Rect::new(line_x + run.offset_x, line_y - shift, run.advance, height),
                        widget: None,
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

            // The pictures and the inline blocks that landed on this line, where
            // the shaper put them.
            children.extend(replaced.iter().enumerate().filter_map(|(number, box_)| {
                let spacer = placed.get(&replaced_spacer(number))?;
                if spacer.line != index {
                    return None;
                }
                let at = (line_x + spacer.x, y + spacer.y - paragraph_top - box_.shift);
                // An inline block was laid out at the origin, contents and all, and
                // is moved to where its line put it — everything inside it goes
                // with it, which is what makes it one thing in the line. It sits on
                // the line's baseline by its *own* last baseline, which is what
                // makes two buttons of different heights read as a row of words.
                if let Some(content) = box_.content.as_deref() {
                    let mut fragment = content.clone();
                    let top = y + line.baseline - paragraph_top - box_.baseline - box_.shift;
                    let (dx, dy) = (at.0 - fragment.rect.x, top - fragment.rect.y);
                    crate::flow::offset(&mut fragment, dx, dy);
                    return Some(fragment);
                }
                let image = box_.image.clone()?;
                // The spacer reserved the whole box; the picture fills what the
                // frame leaves inside it.
                let (extra_x, extra_y) = replaced_edges(&box_.style, width);
                Some(replaced_fragment(
                    box_.id,
                    &box_.style,
                    Some(image),
                    at,
                    (
                        (spacer.width - extra_x).max(0.0),
                        (spacer.height - extra_y).max(0.0),
                    ),
                    width,
                ))
            }));

            out.push(Fragment {
                used: None,
                box_id: Some(parent),
                rect: Rect::new(line_x, line_y, line.width, height),
                kind: FragmentKind::Line,
                style: Arc::clone(&style),
                widget: None,
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
        // The same reach with the *unshifted* spans left out.
        //
        // Every line of the paragraph is at least this tall, and no line is made
        // tall by a span that is not on it. A span sitting on the baseline needs
        // nothing from the paragraph — the shaper knows its own height and which
        // line it landed on — but one that has been raised, lowered or hung off
        // the line box does: how far it moved is worked out here, and the shaper
        // never hears about it.
        let (mut floor_above, mut floor_below) = (strut.ascent, strut.descent);
        self.span_reach.clear();
        self.span_reach.resize(spans.len(), (0.0, 0.0));
        // Kept from the first pass so the second does not shape anything twice:
        // a strut is a font lookup, and the line-relative boxes need theirs
        // again once the line is known.
        let mut line_relative: Vec<(BoxId, otlyra_css::VerticalAlign, otlyra_text::Strut)> =
            Vec::new();
        self.line_shifts.clear();

        for (index, span) in spans.iter().enumerate() {
            let Some(source) = sources.get(index) else {
                continue;
            };
            let span_style = Arc::clone(&self.tree.node(*source).style);
            let own = match self.strut_of(&span_style, &span.font_stack.clone()) {
                Some(own) => own,
                None => continue,
            };

            // `top` and `bottom` are the only two that need the line box, and
            // they are a position *within* it: the line does not grow to fit
            // them, it is what they are measured against. Everything else —
            // including `text-top`, `text-bottom` and `middle`, which are
            // measured against the parent's own font — is a shift the box knows
            // here, and the line grows to hold it like any other.
            if matches!(
                span_style.vertical_align,
                otlyra_css::VerticalAlign::Top | otlyra_css::VerticalAlign::Bottom
            ) {
                // Its own height still asks for room; where it goes does not
                // depend on that, but how tall the line is does.
                above = above.max(own.ascent);
                below = below.max(own.descent);
                floor_above = floor_above.max(own.ascent);
                floor_below = floor_below.max(own.descent);
                self.span_reach[index] = (own.ascent, own.descent);
                line_relative.push((*source, span_style.vertical_align, own));
                continue;
            }

            let shift = match span_style.vertical_align {
                // The parent's own text rather than the whole line: what
                // `text-top` and `text-bottom` mean is the edge of the text the
                // box is set beside, not the edge of the tallest thing on the row.
                otlyra_css::VerticalAlign::TextTop => strut.ascent - own.ascent,
                otlyra_css::VerticalAlign::TextBottom => own.descent - strut.descent,
                // The box's middle against the parent's baseline plus half its
                // x-height. No font here reports an x-height, so half of it is
                // taken as a quarter of the font size — the same shape of
                // fallback the specification names for `sub` and `super`, and the
                // number the web was built against.
                otlyra_css::VerticalAlign::Middle => {
                    style.font_size * 0.25 - (own.ascent - own.descent) / 2.0
                }
                other => baseline_shift_of(other, &span_style, &style),
            };
            if span_style.vertical_align.resolved_while_levelling() {
                self.line_shifts.insert(*source, shift);
            }
            above = above.max(shift + own.ascent);
            below = below.max(own.descent - shift);
            self.span_reach[index] = (shift + own.ascent, own.descent - shift);
            // A span that has been moved is one the shaper cannot place: it knows
            // the span's own height and nothing of the shift, so the room a shift
            // needs comes from the paragraph. A span sitting where the shaper put
            // it asks nothing of the floor and is left to its own line.
            if shift != 0.0 {
                floor_above = floor_above.max(shift + own.ascent);
                floor_below = floor_below.max(own.descent - shift);
            }
            // An explicit `line-height` on a span still asks for its own room.
            if let Some(asked) = span.line_height {
                above = above.max(asked - strut.descent);
                // And where the shaper cannot carry it, the paragraph must. A line
                // height belongs to a *run* of glyphs, and a run is opened when the
                // font changes — so a span that differs from the text around it
                // only in `line-height` shares their run and its own height is lost
                // on the way through. Those, and only those, are folded into the
                // floor: folding in the rest would make a paragraph with one larger
                // word in it that tall on every line.
                let (reach_above, reach_below) = &mut self.span_reach[index];
                *reach_above = reach_above.max(asked - own.descent);
                *reach_below = reach_below.max(own.descent);
            }
        }

        // The second pass, for the two that had to wait: the line box is settled
        // now, so there is something for them to be a position in.
        for (source, align, own) in line_relative {
            let shift = match align {
                otlyra_css::VerticalAlign::Top => above - own.ascent,
                otlyra_css::VerticalAlign::Bottom => own.descent - below,
                _ => 0.0,
            };
            self.line_shifts.insert(source, shift);
        }

        // A picture is deliberately *not* folded in. It sits with its bottom edge on
        // the baseline, so all of it is above — and one large picture would make
        // every line of the paragraph as tall as itself, which for a picture beside
        // a sentence is far more wrong than the pixel it saves. The shaper reserves
        // the room for it within the line it is actually on.
        //
        // An inline block *is* folded in, both ends of it: it is a box in the line
        // like a tall word, sitting on its own last baseline, and CSS grows the
        // line to hold it above and below rather than letting it hang out.
        for box_ in replaced.iter().filter(|box_| box_.content.is_some()) {
            above = above.max(box_.baseline);
            below = below.max(box_.height - box_.baseline);
            floor_above = floor_above.max(box_.baseline);
            floor_below = floor_below.max(box_.height - box_.baseline);
        }

        // What *every* line of the paragraph is at least, and no more than that:
        // the block's own strut, which is the line it would have with nothing in
        // it. Everything else — a taller span, a raised one, a box — belongs to the
        // line it is actually on and is folded in there.
        let _ = (above, below, floor_above, floor_below);
        self.line_reach = (strut.ascent, strut.descent + strut.leading);

        // The shaper is still told a height per span, because that is what it
        // measures a line by while it is breaking one. Where each line's box ends
        // up is settled afterwards, from what landed on it.
        let floor = strut.height();
        if floor.is_finite() && floor > 0.0 {
            for span in spans {
                span.line_height = Some(span.line_height.map_or(floor, |own| own.max(floor)));
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
            used: None,
            box_id: None,
            rect: Rect::new(left, y, room, line.height),
            kind: FragmentKind::Text(run),
            style: Arc::clone(&marker.style),
            widget: None,
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
                    // least as tall as it is — and as tall as the border and
                    // padding around it, which take room in a line like any other
                    // part of the box.
                    let (width, height) = replaced_size(&node.style, content, containing_width);
                    let (extra_x, extra_y) = replaced_edges(&node.style, containing_width);
                    let (width, height) = (width + extra_x, height + extra_y);
                    replaced.push(ReplacedBox {
                        id: child,
                        style: Arc::clone(&node.style),
                        image: content.image.clone(),
                        at: spans.len(),
                        width,
                        height,
                        content: None,
                        baseline: height,
                        shift: baseline_shift(&node.style, &self.tree.node(id).style),
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
                BoxKind::Block
                    if matches!(
                        node.style.display,
                        otlyra_css::Display::InlineBlock | otlyra_css::Display::InlineFlex
                    ) =>
                {
                    // Laid out here and now, as the block container it is, at the
                    // width it shrinks to: what the line has to make room for is
                    // its finished size, and nothing about the line changes it.
                    // Where it *goes* is the shaper's answer, so it is laid out at
                    // the origin and moved once the line is broken.
                    let style = Arc::clone(&node.style);
                    let shift_of_box = baseline_shift(&style, &self.tree.node(id).style);
                    let width = match style.width.resolve(containing_width) {
                        Some(width) => {
                            let padding = resolve_padding(&style, containing_width);
                            let border = resolve_border(&style);
                            width + padding.left + padding.right + border.left + border.right
                        }
                        None => self
                            .max_content_width(child, containing_width)
                            .min(containing_width),
                    };
                    let fragment = self.layout_sized(child, 0.0, 0.0, width);
                    let height = fragment.rect.height;
                    // Its own last baseline, or its bottom edge when it has no line
                    // of text in it at all — which is what CSS says an empty one
                    // and one that hides its overflow both sit on.
                    let baseline = baseline_of(&fragment).unwrap_or(height);
                    replaced.push(ReplacedBox {
                        id: child,
                        style,
                        image: None,
                        at: spans.len(),
                        width: fragment.rect.width,
                        height,
                        content: Some(Box::new(fragment)),
                        baseline,
                        shift: shift_of_box,
                    });
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
/// The content width a declared `width` comes to.
///
/// `box-sizing: border-box` — which most of the web sets on everything before it
/// writes a single width — measures the number across the border box, so the
/// padding and the border come *out* of it rather than being added outside it. A
/// box laid out the other way is that much wider than the page asked for, and a
/// row of them is that much wider than the row.
fn content_from(width: f32, style: &ComputedStyle, padding: Sides<f32>, border: Sides<f32>) -> f32 {
    match style.box_sizing {
        otlyra_css::BoxSizing::Content => width,
        otlyra_css::BoxSizing::Border => {
            (width - padding.left - padding.right - border.left - border.right).max(0.0)
        }
    }
}

/// The same down the block axis.
fn content_height_from(
    height: f32,
    style: &ComputedStyle,
    padding: Sides<f32>,
    border: Sides<f32>,
) -> f32 {
    match style.box_sizing {
        otlyra_css::BoxSizing::Content => height,
        otlyra_css::BoxSizing::Border => {
            (height - padding.top - padding.bottom - border.top - border.bottom).max(0.0)
        }
    }
}

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
        Some(width) => Some(content_from(
            clamp(width, style.min_width, style.max_width, containing),
            style,
            padding,
            border,
        )),
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

/// Work out how big every widget on the page wants to be, once.
///
/// A control that is drawn as a widget has a *default preferred size*: the
/// size it takes when nothing has said otherwise. It is not what its contents
/// come to — an empty field is as wide as a full one — and it is not a constant
/// either, because for everything that holds text it is counted in characters
/// of the control's own font.
///
/// The counting is HTML's. A field is `(size − 1) × avg + max` wide, which is
/// twenty characters plus the difference between an average one and the widest
/// one; a `<textarea>` is `cols × avg` plus room for a scroll bar and `rows`
/// lines tall. `avg` and `max` here are the advance of a digit and of a capital
/// W, which is an approximation of what a font's own tables report and is
/// within a pixel of it for the families a control is ever set in.
fn size_widgets(tree: &mut BoxTree, text: &mut TextEngine) {
    let mut stacks: std::collections::HashMap<usize, FontStack> = std::collections::HashMap::new();
    for id in tree.descendants(tree.root()) {
        let Some(control) = tree.node(id).control.clone() else {
            continue;
        };
        if !control.widget {
            continue;
        }
        // Once, and only once. Layout runs many times over one box tree, and the
        // room a drop-down leaves for its arrow is *added* to the padding rather
        // than replacing it — so a second pass over a control already settled
        // grows it by the width of another arrow.
        if control.sized {
            continue;
        }
        let style = Arc::clone(&tree.node(id).style);
        let Some((width, height)) = widget_size(&control, &style, text, &mut stacks) else {
            continue;
        };
        // Only what the page has not decided. A width in a rule is the page's
        // answer, and a preferred size is what there is in the absence of one.
        let mut sized = (*style).clone();
        let mut changed = false;
        if control.kind == ControlKind::DropDown {
            sized.padding.right =
                otlyra_css::Length::Px(resolve_padding(&style, 0.0).right + ARROW_STRIP);
            changed = true;
        }
        if let Some(width) = width
            && sized.width == LengthOrAuto::Auto
        {
            sized.width = LengthOrAuto::Px(width);
            changed = true;
        }
        if let Some(height) = height
            && sized.height == LengthOrAuto::Auto
        {
            sized.height = LengthOrAuto::Px(height);
            changed = true;
        }
        if changed {
            tree.set_style(id, Arc::new(sized));
        }
        tree.mark_sized(id);
    }
}

/// The content-box size a widget prefers, in each axis it has an opinion about.
fn widget_size(
    control: &Control,
    style: &Arc<ComputedStyle>,
    text: &mut TextEngine,
    stacks: &mut std::collections::HashMap<usize, FontStack>,
) -> Option<(Option<f32>, Option<f32>)> {
    // A checkbox and a radio button, in CSS pixels. Both references agree within a
    // pixel and neither takes it from the font: a checkbox in a heading is the same
    // checkbox.
    const BOX_SIDE: f32 = 13.0;
    // What a scroll bar takes from the width a `<textarea>` asks for.
    const SCROLLBAR: f32 = 15.0;

    let (average, widest, line) = character_widths(style, text, stacks);
    // A field is measured across its content box and a checkbox across its
    // border box; the styles say which, and what is subtracted here is what the
    // difference comes to.
    let edges = |style: &ComputedStyle| {
        let padding = resolve_padding(style, 0.0);
        let border = resolve_border(style);
        (
            padding.left + padding.right + border.left + border.right,
            padding.top + padding.bottom + border.top + border.bottom,
        )
    };
    let (across, down) = edges(style);
    let border_box = style.box_sizing == otlyra_css::BoxSizing::Border;
    let inline = |content: f32| {
        Some(if border_box {
            content + across
        } else {
            content
        })
    };
    let block = |content: f32| Some(if border_box { content + down } else { content });

    match control.kind {
        ControlKind::Checkbox | ControlKind::Radio => Some((inline(BOX_SIDE), block(BOX_SIDE))),
        ControlKind::Field => {
            let size = control.size.unwrap_or(20).max(1) as f32;
            Some((inline((size - 1.0) * average + widest), block(line)))
        }
        ControlKind::Area => {
            let width = control.cols.max(1) as f32 * average + SCROLLBAR;
            let height = control.rows.max(1) as f32 * line;
            Some((inline(width), block(height)))
        }
        ControlKind::ListBox => {
            let rows = control.size.unwrap_or(4).max(1) as f32;
            Some((None, block(rows * line)))
        }
        // A drop-down is as wide as the option it shows plus the arrow beside
        // it, and the option is its contents — so only the arrow is added, by
        // the padding rather than by a width, since a width would stop the
        // contents from making it wider.
        ControlKind::DropDown => Some((None, block(line))),
        ControlKind::Range => Some((inline(129.0), block(16.0))),
        ControlKind::Color => Some((inline(44.0), block(23.0))),
        ControlKind::Button | ControlKind::File => None,
        ControlKind::Progress | ControlKind::Meter => None,
    }
}

/// The strip a drop-down leaves on its inline end for the arrow.
///
/// Room rather than a width: a drop-down is as wide as the option it shows, and
/// a width would stop a long option from making it wider. Both references
/// reserve the same twenty pixels give or take two, and both give it back when
/// the page turns the widget off — which is the one visible thing
/// `appearance: none` does to a `<select>`.
const ARROW_STRIP: f32 = 20.0;

/// An average character, the widest one, and the height of one line, in the font
/// this style asks for.
///
/// The line is the *strut* rather than what a digit measures: a line is as tall as
/// the font reaches above and below the baseline plus whatever `line-height` asks
/// for, and a digit is neither. A field a digit tall is two pixels shorter than
/// every reference, and a `<textarea>` is that twice per row.
fn character_widths(
    style: &Arc<ComputedStyle>,
    text: &mut TextEngine,
    stacks: &mut std::collections::HashMap<usize, FontStack>,
) -> (f32, f32, f32) {
    let key = Arc::as_ptr(&style.font_family) as *const u8 as usize;
    let stack = stacks
        .entry(key)
        .or_insert_with(|| FontStack::parse_css(&style.font_family))
        .clone();
    let size = style.font_size;
    let average = text.measure("0", &stack, size).width;
    let widest = text.measure("W", &stack, size).width;
    let line = text
        .strut(&stack, size, style.font_weight, false)
        .map_or(size, |strut| match style.line_height {
            otlyra_css::LineHeight::Normal => strut.height(),
            ref asked => asked.resolve(size, strut.height()),
        });
    (average, widest, line)
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
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let mut boxes = build_styled_box_tree(&document, &styles);
        let mut text = otlyra_text::TextEngine::isolated();
        let tree = crate::layout(
            &mut boxes,
            &mut text,
            Viewport {
                width,
                height: 600.0,
            },
        );
        (tree, boxes)
    }

    /// A picture's size, given what a stylesheet and the attributes asked for.
    fn drawn_at(style: &ComputedStyle, hint: (Option<f32>, Option<f32>)) -> (f32, f32) {
        replaced_size(
            style,
            &crate::box_tree::Replaced {
                image: None,
                // Four wide and two tall: a ratio of two, so a wrong height is
                // obvious rather than a rounding difference.
                intrinsic: Some((4.0, 2.0)),
                hint,
            },
            800.0,
        )
    }

    /// `width` and `height` on an `<img>` are presentational hints — the lowest
    /// priority rule setting those properties — and not a new intrinsic size.
    /// Written into the intrinsic size they took the aspect ratio with them, so
    /// `width="40"` drew a four-by-two picture forty wide and still two tall:
    /// squashed on one axis, which on a photograph is a wall of vertical
    /// streaks and was reported as exactly that.
    #[test]
    fn a_width_attribute_is_a_hint_and_not_a_new_intrinsic_size() {
        let plain = ComputedStyle::default();

        assert_eq!(drawn_at(&plain, (None, None)), (4.0, 2.0), "nothing asked");
        assert_eq!(
            drawn_at(&plain, (Some(40.0), None)),
            (40.0, 20.0),
            "one dimension takes the other from the ratio"
        );
        assert_eq!(
            drawn_at(&plain, (None, Some(20.0))),
            (40.0, 20.0),
            "from either side"
        );
        assert_eq!(
            drawn_at(&plain, (Some(40.0), Some(5.0))),
            (40.0, 5.0),
            "and both given is both honoured, ratio or not"
        );
        // Downscaling is the same rule and the case the site showed.
        assert_eq!(drawn_at(&plain, (Some(2.0), None)), (2.0, 1.0));

        // A stylesheet outranks the hint, which is what makes it a hint.
        let styled = ComputedStyle {
            width: otlyra_css::LengthOrAuto::Px(80.0),
            ..ComputedStyle::default()
        };
        assert_eq!(drawn_at(&styled, (Some(40.0), None)), (80.0, 40.0));
    }

    /// How many lines of text a document laid out to.
    fn line_count(tree: &FragmentTree) -> usize {
        fn walk(fragment: &Fragment, tops: &mut Vec<i64>) {
            if matches!(fragment.kind, FragmentKind::Text(_)) {
                tops.push((f64::from(fragment.rect.y) * 10.0) as i64);
            }
            for child in &fragment.children {
                walk(child, tops);
            }
        }
        let mut tops = Vec::new();
        walk(&tree.root, &mut tops);
        tops.sort_unstable();
        tops.dedup();
        tops.len()
    }

    /// `white-space` is two independent bits, and until now the layout had only
    /// one of them: whether spaces collapse. Whether a line may break was never
    /// asked, so `nowrap` wrapped — which folded the site's own header onto a
    /// second line — and so did `pre`.
    #[test]
    fn whether_a_line_breaks_is_its_own_property() {
        let narrow = |style: &str| {
            let html =
                format!("<body><div style=\"width:60px;{style}\">one two three four five</div>");
            let (tree, _) = laid_out(&html, 800.0);
            line_count(&tree)
        };

        // The four arrangements of the two bits, and each is a real value of the
        // shorthand: collapse-and-wrap, collapse-and-not, keep-and-not, keep-and-wrap.
        assert!(narrow("") > 1, "normal wraps");
        assert_eq!(narrow("white-space:nowrap"), 1, "nowrap does not");
        assert_eq!(narrow("white-space:pre"), 1, "nor does pre");
        assert!(narrow("white-space:pre-wrap") > 1, "but pre-wrap does");
    }

    /// The site's header, in the shape that folded it: links told not to break,
    /// in a row told not to wrap.
    #[test]
    fn a_nav_told_not_to_wrap_stays_on_one_line() {
        let (tree, _) = laid_out(
            "<body><nav style=\"display:flex;flex-wrap:nowrap;width:80px\">\
             <a style=\"white-space:nowrap\">Home page</a>\
             <a style=\"white-space:nowrap\">About us</a></nav>",
            800.0,
        );
        assert_eq!(line_count(&tree), 1);
    }

    /// A row of flex items is as wide as its items side by side. Both intrinsic
    /// widths took the *widest* item instead: right for boxes that stack, wrong
    /// for boxes that sit in a row — and a flex item is blockified, so it never
    /// reached the inline branch that would have summed it. The site's brand came
    /// out as wide as its wordmark alone, and the wordmark was drawn over the nav
    /// beside it.
    #[test]
    fn a_flex_row_is_as_wide_as_its_items_side_by_side() {
        let (tree, boxes) = laid_out(
            "<body><div style=\"display:flex;width:600px\">\
             <a id=brand style=\"display:flex;gap:10px\">\
             <span style=\"display:block;width:30px\">.</span>\
             <span style=\"display:block;width:40px\">.</span></a>\
             <nav style=\"display:flex\"><a style=\"display:block;width:50px\">.</a></nav>\
             </div>",
            800.0,
        );

        // Thirty and forty with ten between them: eighty, not the forty that the
        // wider of the two would have given.
        let brand = rect_of(&tree, &boxes, "a");
        assert_eq!(brand.width, 80.0);
        // And what comes after it starts where it ends, rather than over it.
        let nav = rect_of(&tree, &boxes, "nav");
        assert!(
            nav.x >= brand.x + brand.width,
            "the nav at {} runs into the brand ending at {}",
            nav.x,
            brand.x + brand.width
        );
    }

    /// An `auto` margin on a flex item eats the free space before
    /// `justify-content` sees any — which is how a brand is pushed to one end and
    /// a nav to the other, and how a lone item is centred.
    #[test]
    fn an_auto_margin_takes_the_free_space_first() {
        let (tree, boxes) = laid_out(
            "<body><div style=\"display:flex;width:400px\">\
             <a style=\"margin-right:auto;width:50px\">.</a>\
             <b style=\"width:50px\">.</b></div>",
            800.0,
        );
        // Offsets are relative to the container, whose own position carries the
        // body's default margin; what is being asserted is the gap the auto
        // margin opened, not where the page starts.
        let brand = rect_of(&tree, &boxes, "a");
        let end = rect_of(&tree, &boxes, "b");
        assert_eq!(
            end.x - brand.x,
            350.0,
            "pushed to the far end, not left beside the first"
        );

        // Both sides `auto` centres it, the same rule seen twice.
        let (tree, boxes) = laid_out(
            "<body><div style=\"display:flex;width:400px\">\
             <a style=\"margin:0 auto;width:100px\">.</a></div>",
            800.0,
        );
        let container = rect_of(&tree, &boxes, "div");
        let centred = rect_of(&tree, &boxes, "a");
        assert_eq!(centred.x - container.x, 150.0);
    }

    /// Layout knows what `auto` came out as and a computed style does not, so the
    /// edges a box actually got are reported on the fragment for the panel that
    /// asks.
    #[test]
    fn a_fragment_carries_the_edges_it_was_given() {
        let (tree, boxes) = laid_out(
            "<body><div id=x style=\"width:100px;margin:0 auto;padding:5px;border:2px solid\">.</div>",
            400.0,
        );
        let used = tree
            .iter()
            .find(|fragment| {
                fragment
                    .box_id
                    .and_then(|id| boxes.get(id))
                    .and_then(|node| node.tag.as_ref())
                    .is_some_and(|tag| tag.as_ref() == "div")
            })
            .and_then(|fragment| fragment.used)
            .expect("a div was laid out with edges");

        assert_eq!(used.padding.left, 5.0);
        assert_eq!(used.border.left, 2.0);
        // A margin the style spells `auto` is a number here, which is the whole
        // reason the panel reads this rather than the computed style. Both sides
        // came out equal and neither is nothing, which is what centring is.
        assert!(used.margin.left > 0.0, "an auto margin resolved to nothing");
        assert!((used.margin.left - used.margin.right).abs() < 0.5);
    }

    /// Under `nowrap` the whole run is one unbreakable thing, so a flex item may
    /// not be shrunk to its longest word — the text it draws would spill over the
    /// item beside it, which is what overlapped the site's nav links.
    #[test]
    fn a_nowrap_item_is_not_shrunk_to_its_longest_word() {
        let (tree, boxes) = laid_out(
            "<body><nav style=\"display:flex;width:60px\">\
             <a style=\"white-space:nowrap\">The name</a>\
             <b style=\"white-space:nowrap\">For agents</b></nav>",
            800.0,
        );
        let first = rect_of(&tree, &boxes, "a");
        let second = rect_of(&tree, &boxes, "b");
        assert!(
            second.x >= first.x + first.width,
            "the items overlap: one is {}..{} and the next starts at {}",
            first.x,
            first.x + first.width,
            second.x
        );
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

    /// The five values of `vertical-align` that are a position rather than a
    /// shift, each moving its box the way the specification says.
    #[test]
    fn vertical_align_puts_a_span_where_the_value_names() {
        /// Where the span ended up, and how tall the line it is on came out.
        fn span_and_line(value: &str) -> (Rect, Rect) {
            let (tree, boxes) = laid_out(
                &format!(
                    "<style>body {{ margin: 0; font: 16px/80px monospace }} \
                     span {{ font-size: 32px; background: #eee; \
                     line-height: normal; vertical-align: {value} }}</style>\
                     <p>base <span>x</span> after</p>"
                ),
                600.0,
            );
            let pieces = boxes_of(&tree, &boxes, "span");
            let span = pieces.first().expect("a box for the span").rect;
            (span, first_line(&tree))
        }

        let (baseline, _) = span_and_line("baseline");
        let (top, top_line) = span_and_line("top");
        let (bottom, bottom_line) = span_and_line("bottom");
        let (text_top, _) = span_and_line("text-top");
        let (text_bottom, _) = span_and_line("text-bottom");
        let (middle, _) = span_and_line("middle");

        // Ordering rather than absolute edges: an inline box's fragment is
        // still drawn as tall as the *line* rather than as tall as its own
        // text — a separate defect, visible as a background taller than the
        // words it is behind — so its top edge is what can be trusted here.
        assert!(
            top.y < bottom.y,
            "top {top:?} sits above bottom {bottom:?} on {top_line:?} / {bottom_line:?}"
        );
        assert!(
            text_top.y < text_bottom.y,
            "text-top {text_top:?} sits above text-bottom {text_bottom:?}"
        );

        // And every one of them is somewhere: a value that did nothing would
        // land exactly where `baseline` did, which is how these five behaved
        // before the cascade was read for them.
        for (name, rect) in [
            ("top", top),
            ("bottom", bottom),
            ("text-top", text_top),
            ("text-bottom", text_bottom),
            ("middle", middle),
        ] {
            assert!(
                (rect.y - baseline.y).abs() > 0.5,
                "{name} moved nothing: {rect:?} against baseline {baseline:?}"
            );
        }
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
                text_scale: 1.0,
                color_scheme: Default::default(),
            },
        );
        let images: crate::Images = crate::image_sources(&document, StyleViewport::default())
            .into_iter()
            .map(|source| (source.node, crate::Picture::new(image.clone())))
            .collect();
        let mut boxes = crate::build_box_tree_with_images(&document, Some(&styles), &images);
        let mut text = otlyra_text::TextEngine::isolated();
        let tree = crate::layout(
            &mut boxes,
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

    /// The box a replaced element makes, and the picture inside it.
    fn boxed_image(tree: &FragmentTree) -> (Rect, Rect) {
        let outer = tree
            .iter()
            .find(|fragment| {
                fragment
                    .children
                    .iter()
                    .any(|child| matches!(child.kind, FragmentKind::Image(_)))
            })
            .expect("a box holding a picture");
        (outer.rect, image_rect(tree))
    }

    /// A replaced element has a border and a background of its own, and both take
    /// room: the box is the picture plus the frame around it.
    ///
    /// Every number here was measured against a reference browser on the same
    /// page. A hundred-by-fifty picture, a ten-pixel border and five of padding.
    #[test]
    fn a_picture_has_a_frame_and_the_frame_takes_room() {
        let laid_out = |rule: &str| {
            laid_out_with_image(
                &format!(
                    "<style>body {{ margin: 0 }} img {{ display: block; border: 10px solid red; \
                     padding: 5px; {rule} }}</style><img src=a.png>"
                ),
                820.0,
                picture(100, 50),
            )
            .0
        };

        let size = |tree: &FragmentTree| {
            let (outer, inner) = boxed_image(tree);
            (
                (outer.width, outer.height),
                (inner.width, inner.height),
                (inner.x - outer.x, inner.y - outer.y),
            )
        };

        // Nothing asked: the picture's own size, and the frame outside it.
        assert_eq!(
            size(&laid_out("")),
            ((130.0, 80.0), (100.0, 50.0), (15.0, 15.0))
        );
        // A width is the picture's width, and the frame is still outside it.
        assert_eq!(
            size(&laid_out("width: 100px")),
            ((130.0, 80.0), (100.0, 50.0), (15.0, 15.0))
        );
        // `border-box` measures across the frame instead, and the ratio is
        // applied to what is left over rather than to the number the page wrote.
        assert_eq!(
            size(&laid_out("width: 100px; box-sizing: border-box")),
            ((100.0, 65.0), (70.0, 35.0), (15.0, 15.0))
        );
        assert_eq!(
            size(&laid_out(
                "width: 100px; height: 40px; box-sizing: border-box"
            )),
            ((100.0, 40.0), (70.0, 10.0), (15.0, 15.0))
        );
        // A percentage width is a percentage of the containing block, and the
        // frame is added to it — which is what makes `width: 100%` overflow.
        assert_eq!(
            size(&laid_out("width: 100%")),
            ((850.0, 440.0), (820.0, 410.0), (15.0, 15.0))
        );
    }

    /// A picture in a line reserves its frame there too, and its background and
    /// border are painted like any other box's.
    #[test]
    fn an_inline_picture_reserves_its_frame_in_the_line() {
        let (tree, _) = laid_out_with_image(
            "<style>body { margin: 0 } img { border: 10px solid red; padding: 5px }\
             </style><p>x <img src=a.png> y</p>",
            820.0,
            picture(100, 50),
        );

        let (outer, inner) = boxed_image(&tree);
        assert_eq!((outer.width, outer.height), (130.0, 80.0));
        assert_eq!((inner.width, inner.height), (100.0, 50.0));
        assert_eq!((inner.x - outer.x, inner.y - outer.y), (15.0, 15.0));

        let line = first_line(&tree);
        assert!(
            line.height >= 80.0,
            "the line holds the whole box: {}",
            line.height
        );
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

    /// `order` decides which of its siblings an item is laid out among, and
    /// nothing else: the document order is still what the text reads as.
    ///
    /// A stable sort, so items that name the same order keep the order they were
    /// written in. Measured against a reference.
    #[test]
    fn order_rearranges_the_items_and_leaves_the_document_alone() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } .flex { display: flex } \
             div > div { width: 100px }</style>\
             <div class=flex><div style='order:3'>a</div><div style='order:1'>b</div>\
             <div>c</div><div style='order:-1'>d</div></div>",
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        // The container first, then its four items as they were laid out.
        let placed: Vec<(f32, i32)> = items[1..]
            .iter()
            .map(|item| (item.rect.x, item.style.order))
            .collect();
        assert_eq!(
            placed,
            vec![(0.0, -1), (100.0, 0), (200.0, 1), (300.0, 3)],
            "`order` ascending, and the two that agree keep the order they were written in"
        );
    }

    /// A margin does not escape a box that establishes a formatting context of
    /// its own — a flex item, a float, a cell, anything that clips.
    ///
    /// Found by putting a heading above a flex container inside a flex item and
    /// comparing with a reference: the heading's own top margin was collapsing
    /// out through the item and pushing the whole row down, which left a gap
    /// above the item rather than inside it. On a page of such rows the error
    /// added up down the page.
    #[test]
    fn a_margin_does_not_escape_a_formatting_context() {
        let inside = |row: &str, item: &str| {
            let (tree, boxes) = laid_out(
                &format!(
                    "<style>body {{ margin: 0 }} .row {{ {row} }} \
                     .item {{ width: 200px; {item} }} h2 {{ margin: 20px 0 0 }}</style>\
                     <div class=row><div class=item><h2>a</h2></div></div>"
                ),
                400.0,
            );
            let item = boxes_of(&tree, &boxes, "div")[1].rect;
            let heading = boxes_of(&tree, &boxes, "h2")[0].rect;
            (item.y, heading.y - item.y)
        };

        // A flex item keeps its child's margin inside itself.
        assert_eq!(
            inside("display: flex", ""),
            (0.0, 20.0),
            "the item starts at the top and the margin is inside it"
        );
        // So does anything that clips, and anything that floats.
        assert_eq!(inside("display: block", "overflow: hidden"), (0.0, 20.0));
        assert_eq!(inside("display: block", "float: left"), (0.0, 20.0));
        // An ordinary block does not: the margin comes out through it and lands
        // above it, which is what collapsing is.
        assert_eq!(inside("display: block", ""), (20.0, 0.0));
    }

    /// `align-content` shares out the room a wrapped container's lines leave
    /// across it. Every number here was measured against a reference.
    #[test]
    fn align_content_places_the_lines_of_a_wrapped_container() {
        let tops = |align: &str| {
            let (tree, boxes) = laid_out(
                &format!(
                    "<style>body {{ margin: 0 }} .flex {{ display: flex; flex-wrap: wrap; \
                     width: 300px; height: 200px; align-content: {align} }} \
                     div > div {{ width: 120px; height: 40px }}</style>\
                     <div class=flex><div>1</div><div>2</div><div>3</div><div>4</div>\
                     <div>5</div></div>"
                ),
                400.0,
            );
            let items = boxes_of(&tree, &boxes, "div");
            // The tops of the three lines: items one, three and five.
            (items[1].rect.y, items[3].rect.y, items[5].rect.y)
        };

        // Three lines of forty in two hundred: eighty over, and `stretch` — the
        // initial value — gives each line a third of it.
        assert_eq!(tops("stretch"), (0.0, 200.0 / 3.0, 400.0 / 3.0));
        assert_eq!(tops("flex-start"), (0.0, 40.0, 80.0));
        assert_eq!(tops("center"), (40.0, 80.0, 120.0));
        assert_eq!(tops("flex-end"), (80.0, 120.0, 160.0));
        assert_eq!(tops("space-between"), (0.0, 80.0, 160.0));
        // Half a share at each end and a whole one between: the share is eighty
        // over three, so the first line starts at half of it and each one after
        // begins forty and a share further down.
        let around = tops("space-around");
        let share = 80.0 / 3.0;
        assert!(
            (around.0 - share / 2.0).abs() < 0.01
                && (around.1 - (share / 2.0 + 40.0 + share)).abs() < 0.01
                && (around.2 - (share / 2.0 + 80.0 + share * 2.0)).abs() < 0.01,
            "{around:?}"
        );
    }

    /// `inline-flex` is a flex container that takes its place in a line, which is
    /// where an `inline-block` goes: inline outside, flex inside.
    #[test]
    fn an_inline_flex_container_sits_in_the_line() {
        let (tree, boxes) = laid_out(
            "<style>body { margin: 0 } .flex { display: inline-flex } \
             .flex > span { width: 40px; height: 20px }</style>\
             <div>before <span class=flex><span>a</span><span>b</span></span> after</div>",
            400.0,
        );
        let placed = boxes_of(&tree, &boxes, "span");
        let container = &placed[0];
        assert_eq!(
            container.rect.width, 80.0,
            "as wide as its items side by side, not as wide as the line"
        );
        assert!(
            container.rect.x > 0.0,
            "and it starts after the text before it: {:?}",
            container.rect
        );
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
