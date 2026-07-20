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

/// One fragment.
#[derive(Clone, Debug)]
pub struct Fragment {
    /// The box this came from, absent only for the page itself.
    pub box_id: Option<BoxId>,
    /// Where it is and how big, in logical pixels, in page coordinates.
    pub rect: Rect,
    /// What it is.
    pub kind: FragmentKind,
    /// The style to paint it with.
    pub style: Arc<ComputedStyle>,
    /// Children, in paint order.
    pub children: Vec<Fragment>,
}

/// A laid-out page.
#[derive(Clone, Debug)]
pub struct FragmentTree {
    /// The root fragment: the initial containing block.
    pub root: Fragment,
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

    /// Fragments that touch `viewport`, in paint order.
    ///
    /// Culling, not clipping: a fragment outside the viewport is not painted at all
    /// rather than painted and discarded, and on a long page that is most of them.
    pub fn visible<'a>(&'a self, viewport: &'a Rect) -> impl Iterator<Item = &'a Fragment> {
        self.iter()
            .filter(move |fragment| fragment.rect.intersects(viewport))
    }
}
