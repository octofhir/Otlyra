//! The fragment tree: boxes once they have a position and a size.
//!
//! The third tree. A box is a thing the document asked for; a fragment is a place
//! on the page. One box can produce several fragments — a paragraph that spans two
//! columns, an inline that wraps across three lines — which is why this cannot be
//! stored on the box.
//!
//! Fragments carry geometry and paint-relevant style only. They are read by paint
//! and by hit testing, and by nothing that can change them.

use otlyra_css::ComputedStyle;
use otlyra_text::ShapedRun;
use std::sync::Arc;

use crate::box_tree::BoxId;

/// A rectangle in logical pixels, with its origin at the top left of the page.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl Rect {
    /// A rectangle.
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// The right edge.
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    /// The part of this rectangle that is also inside `other`.
    pub fn intersection(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Rect::new(
            x,
            y,
            (self.right().min(other.right()) - x).max(0.0),
            (self.bottom().min(other.bottom()) - y).max(0.0),
        )
    }

    /// Whether this rectangle overlaps `other` at all.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// What a fragment is.
#[derive(Clone, Debug)]
pub enum FragmentKind {
    /// A block box: a background, and eventually borders.
    Box,
    /// One line of an inline formatting context.
    Line,
    /// A replaced box's content: a picture, drawn to fill the fragment.
    Image(otlyra_gfx::peniko::ImageData),
    /// One shaped run of glyphs, positioned relative to the fragment's origin.
    ///
    /// One fragment per run rather than per line, so that the fragment's rectangle
    /// is the run's own: that is what hit testing needs, and a link that is
    /// clickable across the whole line it happens to sit on is worse than no hit
    /// testing at all.
    Text(ShapedRun),
}

/// The edges a box actually got, once layout had resolved them.
///
/// The *used* values, which is what an inspector's box model is asking for and
/// what a computed style cannot always answer: `margin: auto` computes to `auto`
/// and only layout knows what it came out as. Percentages are resolved here too,
/// so the panel reads numbers rather than re-deriving them against a containing
/// block it would have to find for itself.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct UsedEdges {
    /// The used margins.
    pub margin: otlyra_css::Sides<f32>,
    /// The used border widths.
    pub border: otlyra_css::Sides<f32>,
    /// The used padding.
    pub padding: otlyra_css::Sides<f32>,
}

/// One fragment.
#[derive(Clone, Debug)]
pub struct Fragment {
    /// The edges layout resolved for this box, for a panel that asks what it
    /// actually got. Absent for the fragments that are not boxes.
    pub used: Option<UsedEdges>,
    /// The box this came from, absent only for the page itself.
    pub box_id: Option<BoxId>,
    /// Where it is and how big, in logical pixels, in page coordinates.
    pub rect: Rect,
    /// What it is.
    pub kind: FragmentKind,
    /// The style to paint it with.
    pub style: Arc<ComputedStyle>,
    /// Where in the painting order this fragment belongs.
    ///
    /// Everything in the flow is at zero. A positioned box is above it at the same
    /// `z-index`, and `z-index` moves it further up or below — which is the whole
    /// of the painting order CSS gives a document with no transforms, opacity or
    /// filters in it. Descendants take their ancestor's level, so a positioned box
    /// and its contents travel together.
    pub layer: Layer,
    /// The scroll port this fragment moves with, if it is inside one.
    ///
    /// The innermost one: a box inside two scrollable ancestors moves with the
    /// nearer of them, and that one moves with the outer.
    pub scroll_port: Option<BoxId>,
    /// The rectangle this fragment is cut off at, if an ancestor cuts it off.
    ///
    /// In page coordinates, and already the intersection of every ancestor that
    /// clips — so paint applies one rectangle rather than walking back up a tree it
    /// does not have.
    pub clip: Option<Rect>,
    /// What holds this fragment in view while the page scrolls, if anything.
    pub sticky: Option<Sticky>,
    /// Whether scrolling the page moves it.
    ///
    /// A fixed box is placed against the viewport, and so is everything inside it;
    /// its coordinates are already the ones it is drawn at, whatever the reader has
    /// scrolled past.
    pub fixed: bool,
    /// Children, in paint order.
    pub children: Vec<Fragment>,
}

/// What `position: sticky` needs at paint time.
///
/// Layout knows where the box is and how far it may travel; only paint knows how
/// far the page has been scrolled. So layout resolves the constraint and paint
/// applies it — the box is where the flow put it until the scroll would take it
/// past its inset, and then it stops there until its container runs out.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Sticky {
    /// Distance from the top of the viewport it may not pass, if `top` says so.
    pub top: Option<f32>,
    /// The same at the bottom.
    pub bottom: Option<f32>,
    /// The box's own border box, where the flow put it.
    pub own: Rect,
    /// The containing block it may not leave.
    pub container: Rect,
}

/// A place in the painting order.
///
/// `z-index` is not a number the whole page is sorted by. It orders a box against
/// its *siblings* inside the nearest box that makes a stacking context, and that
/// box is ordered against its own siblings in the same way — so a box with
/// `z-index: 100` inside a `z-index: 1` box stays below everything the `1` is
/// below, however large its own number is.
///
/// That is what the path here is: one step per context down from the page, and a
/// last step for the box itself. Each step is *which level* — negative for a
/// `z-index` below the flow, zero for the flow, above it for a positioned box —
/// and *where in the document* the box was, which is what settles two boxes on the
/// same level and is what keeps a box's contents between it and its next sibling.
/// Comparing two paths is comparing them at the first step where they part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    /// `z-index`, with `auto` counting as zero. Meaningless unless positioned.
    pub index: i32,
    /// Whether the box is positioned at all, which is what lifts it off the flow's
    /// level and makes a context of it.
    pub positioned: bool,
    /// One step per level, outermost first.
    ///
    /// Filled in once the tree is built, because a box's place in the order is its
    /// ancestors' places first — and a fragment is finished before the box holding
    /// it is.
    order: std::sync::Arc<[(i64, u32)]>,
    /// Where this fragment starts in document order, and where its subtree ends.
    ///
    /// What a group needs and the order alone cannot say: `opacity` applies to an
    /// element and its contents *once*, so painting has to know where the contents
    /// stop. Two numbers rather than a second walk of the tree, and they answer the
    /// only question asked of them — whether one fragment is inside another.
    pub enter: u32,
    /// One past the last fragment of this one's subtree.
    pub exit: u32,
}

/// Two fragments are compared by where they sit in the order and by nothing else:
/// the index and the flag are what the order was *computed* from.
impl Ord for Layer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order.cmp(&other.order)
    }
}

impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for Layer {
    /// The flow: the level everything unpositioned sits on.
    fn default() -> Self {
        Self::flow()
    }
}

impl Layer {
    /// Which level a box is on: the flow's, or however far a `z-index` moves it.
    ///
    /// One number, and the arithmetic is the ordering: a positioned box at index
    /// zero sits above the flow, and a negative index sits below it.
    fn level(index: i32, positioned: bool) -> i64 {
        if positioned {
            i64::from(index) * 2 + 1
        } else {
            0
        }
    }

    /// The level in-flow content sits on, before the tree knows where it is.
    pub fn flow() -> Self {
        Self {
            index: 0,
            positioned: false,
            order: std::sync::Arc::from(Vec::new()),
            enter: 0,
            exit: 0,
        }
    }

    /// The level of a positioned box, before the tree knows where it sits.
    pub fn positioned(index: i32) -> Self {
        Self {
            index,
            positioned: true,
            ..Self::flow()
        }
    }

    /// Whether `other` is this fragment or inside it.
    pub fn contains(&self, other: &Self) -> bool {
        other.enter >= self.enter && other.enter < self.exit
    }

    /// This path with one more step on the end.
    fn descend(&self, level: i64, at: u32) -> std::sync::Arc<[(i64, u32)]> {
        let mut order = Vec::with_capacity(self.order.len() + 1);
        order.extend_from_slice(&self.order);
        order.push((level, at));
        std::sync::Arc::from(order)
    }
}

/// Where every fragment sits in the painting order, and where its subtree ends.
///
/// A box that makes a stacking context takes its contents with it: what is inside
/// a box that paints above its neighbours paints above them too, and what is
/// inside a box that is *not* one is ordered against that box's siblings directly.
/// So the path is handed down and extended only where a context is made.
pub(crate) fn assign_paint_order(fragment: &mut Fragment) {
    let root = Layer::flow();
    let mut next = 0;
    assign(fragment, &root, &mut next);
}

fn assign(fragment: &mut Fragment, context: &Layer, next: &mut u32) {
    let enter = *next;
    *next += 1;

    let positioned = fragment.layer.positioned;
    let index = fragment.layer.index;
    let own = Layer {
        index,
        positioned,
        order: context.descend(Layer::level(index, positioned), enter),
        enter,
        exit: enter,
    };

    // A positioned box makes a context of its own. So does a half-transparent one —
    // it is composited once, contents and all, and something inside it that painted
    // somewhere else could not be part of that — and so does a transformed one, for
    // the same reason: what is inside it is drawn in its space. Only a box; a line
    // and a run of glyphs carry the style of the block they are in, and a context
    // per line of a faded paragraph would be a context per line of it.
    let grouped = matches!(fragment.kind, FragmentKind::Box)
        && (fragment.style.opacity < 1.0 || !fragment.style.transform.is_empty());
    let context = if positioned || grouped { &own } else { context };

    for child in &mut fragment.children {
        assign(child, context, next);
    }

    fragment.layer = Layer { exit: *next, ..own };
}

/// A box that cuts its contents off and has more of them than it can show.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollPort {
    /// The box itself.
    pub id: BoxId,
    /// What is on screen: its padding box, in page coordinates.
    pub port: Rect,
    /// How tall its contents are.
    pub content_height: f32,
}

impl ScrollPort {
    /// The furthest it can be scrolled.
    pub fn range(&self) -> f32 {
        (self.content_height - self.port.height).max(0.0)
    }
}

/// A laid-out page.
#[derive(Clone, Debug)]
pub struct FragmentTree {
    /// The root fragment: the initial containing block.
    pub root: Fragment,
    /// The boxes inside the page that scroll, outermost first.
    pub scroll_ports: Vec<ScrollPort>,
}

impl FragmentTree {
    /// The height the content actually needed, which is what a scrollbar measures
    /// against and what the window has to be told about.
    pub fn content_height(&self) -> f32 {
        self.root.rect.height
    }

    /// Every fragment, depth first, parents before children — which is paint order.
    pub fn iter(&self) -> impl Iterator<Item = &Fragment> {
        let mut stack = vec![&self.root];
        std::iter::from_fn(move || {
            let fragment = stack.pop()?;
            stack.extend(fragment.children.iter().rev());
            Some(fragment)
        })
    }

    /// Fragments that touch the visible area, in paint order.
    ///
    /// Culling, not clipping: a fragment outside the viewport is not painted at all
    /// rather than painted and discarded, and on a long page that is most of them.
    ///
    /// Two rectangles because there are two coordinate spaces: `scrolled` is the
    /// part of the page on screen, and `screen` is the screen itself, which is what
    /// a fixed fragment is already placed in.
    pub fn visible<'a>(
        &'a self,
        scrolled: &'a Rect,
        screen: &'a Rect,
    ) -> impl Iterator<Item = &'a Fragment> {
        self.iter().filter(move |fragment| {
            // A sticky fragment is culled by where it may travel rather than by
            // where the flow put it: a heading held at the top of the screen is on
            // screen precisely when its section still is.
            if let Some(sticky) = fragment.sticky {
                return sticky.container.intersects(scrolled);
            }
            let against = if fragment.fixed { screen } else { scrolled };
            fragment.rect.intersects(against)
        })
    }
}
