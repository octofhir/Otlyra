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
/// Ordered by `z-index` first and by whether the box is positioned second, so a
/// positioned box with `z-index: auto` paints over its in-flow neighbours, and one
/// with a negative `z-index` paints under them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Layer {
    /// `z-index`, with `auto` counting as zero.
    pub index: i32,
    /// Whether the box is positioned at all.
    pub positioned: bool,
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
