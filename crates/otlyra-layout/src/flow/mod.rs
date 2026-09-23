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
//!
//! ## The shape
//!
//! This module holds the two ways in, [`layout`] and [`relayout_contained`], the
//! engine they both drive, and the walks that move and mark a subtree once it has
//! been laid out — which every formatting context needs and none of them owns.
//! The contexts are a module each:
//!
//! - `block` stacks boxes and collapses their margins, and hands a container to
//!   whichever context its `display` names.
//! - `inline` gathers a paragraph, shapes it and builds its line boxes; `list`
//!   hangs a list item's marker off the first of them.
//! - `flex`, `grid` and `table` are the other three containers.
//! - `float` and `positioned` are the boxes that leave the flow.
//! - `replaced` sizes a picture, and `widgets` a form control.
//! - `box_model` resolves margins, borders, padding and the sizes they come to,
//!   and `intrinsic` measures how wide a box's content wants to be.

mod block;
mod box_model;
mod flex;
mod float;
mod grid;
mod inline;
mod intrinsic;
mod list;
mod positioned;
mod replaced;
mod table;
mod widgets;

use std::sync::Arc;

use otlyra_css::ComputedStyle;
use otlyra_text::{FontStack, TextEngine};

use crate::box_tree::{BoxId, BoxTree};
use crate::fragment::{Fragment, FragmentKind, FragmentTree, Layer, Rect, ScrollPort, Sticky};

use float::FloatBox;
use intrinsic::Wanted;
use list::PendingMarker;
use table::TableLines;
use widgets::size_widgets;

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
        // The initial containing block is the viewport, and the viewport has a
        // height. So `html { min-height: 100% }` — which a great many pages open
        // with — is a hundred percent of something rather than of nothing.
        containing_height: Some(viewport.height),
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

/// Lay out one box's contents again, in the room it already occupies.
///
/// A *relayout boundary*, as Blink names it: a box whose size cannot be changed
/// by what is inside it, so laying its contents out again cannot move anything
/// outside it. A text field is the case this exists for — its width comes from
/// `size` or from CSS, and what it holds is clipped and scrolled inside it — so
/// what a reader types changes the glyphs in the field and nothing else on the
/// page. Laying a whole document out to answer one keystroke is the difference
/// between a field that keeps up with typing and one that does not.
///
/// `content` is the box's content rectangle in page coordinates: where its
/// children were placed, and where they are placed again. The caller is
/// responsible for the boundary being a real one — [`crate::BoxTree`] cannot
/// check it — and for splicing the result back with
/// [`FragmentTree::replace_contents`].
pub fn relayout_contained(
    tree: &BoxTree,
    text: &mut TextEngine,
    id: BoxId,
    content: Rect,
) -> Vec<Fragment> {
    let _span = tracing::info_span!("layout", width = content.width, contained = true).entered();
    let mut engine = Flow {
        tree,
        text,
        font_stacks: std::collections::HashMap::new(),
        line_shifts: std::collections::HashMap::new(),
        floats: Vec::new(),
        containing_blocks: vec![content],
        viewport: content,
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
    let mut children = Vec::new();
    engine.layout_children(id, content.width, content.x, content.y, &mut children);
    for child in &mut children {
        attach_widgets(child, tree);
    }
    children
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

#[cfg(test)]
mod tests;
