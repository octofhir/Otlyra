//! A box its container lays out as the root of a formatting context of its
//! own, at a rectangle the container chose: a flex item, a grid item.
//!
//! The container decides the size; this measures how tall the box is at a
//! width, and lays it out once, where it ends up.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, MaxSize, Sides, Size};

use crate::box_tree::{BoxId, BoxKind};
use crate::flow::Flow;
use crate::flow::replaced::{replaced_fragment, replaced_height};
use crate::flow::sizing::{BlockSpace, Frame, Limits, block_sizes_in};
use crate::fragment::{Fragment, Rect};

/// Whether a content keyword among a box's block-axis limits asks what its
/// content comes to, which only a measure can answer (CSS Sizing 3 §3.2).
pub(in crate::flow) fn limits_ask_content(style: &ComputedStyle) -> bool {
    matches!(style.min_height, Size::Intrinsic(_))
        || matches!(style.max_height, MaxSize::Intrinsic(_))
}

/// An item with the edges its container resolved: margins with `auto` as zero,
/// and a percentage in either of the first two of the container's width.
pub(in crate::flow) struct ItemBox<'s> {
    pub(in crate::flow) id: BoxId,
    pub(in crate::flow) style: &'s Arc<ComputedStyle>,
    pub(in crate::flow) margin: Sides<f32>,
    pub(in crate::flow) padding: Sides<f32>,
    pub(in crate::flow) border: Sides<f32>,
}

impl ItemBox<'_> {
    /// What its padding and border add to it.
    pub(in crate::flow) fn frame(&self) -> Frame {
        Frame::new(self.padding, self.border)
    }

    /// The width its contents are laid out in when its border box is `width`
    /// wide: the one subtraction its measure and its layout both make, so that
    /// the two ask about the same width to the bit and the second finds what
    /// the first kept.
    pub(in crate::flow) fn content_width(&self, width: f32) -> f32 {
        (width - self.frame().inline).max(0.0)
    }
}

/// The height a flex or grid item is laid out to, as its container decided it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(in crate::flow) enum ItemHeight {
    /// As tall as what it holds at the width it was given, held between its own
    /// minimum and maximum, and at least this: an item nothing stretches.
    AtLeast(f32),
    /// Exactly this, as a border box, whatever it holds: what a flex line or a
    /// grid area stretched it to, and down a flex column what the sharing out
    /// left it. `definite` is whether a percentage inside it has that height to
    /// be of (CSS Flexbox §9.8, CSS Grid 2 §6.5) — a stretched item's does,
    /// and a column item's does when the container had a height of its own to
    /// share out.
    Exactly { height: f32, definite: bool },
}

impl Flow<'_> {
    /// How tall an item is, as a border box, when it is `width` wide, and the
    /// limits `stretch` holds its height between: a flex row item's
    /// hypothetical cross size (CSS Flexbox §9.4, step 7), and a grid item's
    /// contribution to its rows (CSS Grid 2 §12.5).
    ///
    /// Its content is measured rather than laid out, and a measure is kept by the
    /// width it was taken at, so the item's final layout — and every later
    /// layout of the container that holds it — asks nothing new. An item that
    /// names a height is that tall whatever it holds, and is not measured at all
    /// unless a content keyword among its limits asks what its content comes to.
    /// A picture is as tall as that width makes it through its ratio.
    pub(in crate::flow) fn block_size_at(
        &mut self,
        item: &ItemBox<'_>,
        width: f32,
        containing_width: f32,
    ) -> (f32, Limits) {
        let frame = item.frame();
        let content_width = item.content_width(width);
        let (height, heights) = match &self.tree.node(item.id).kind {
            BoxKind::Replaced(content) => {
                let height = replaced_height(
                    item.style,
                    content,
                    content_width,
                    containing_width,
                    self.containing_height,
                );
                let heights = block_sizes_in(
                    item.style,
                    containing_width,
                    self.containing_height,
                    Some(height),
                );
                (height, heights)
            }
            BoxKind::Block | BoxKind::Inline | BoxKind::Text(_) => {
                // Measured unless it named a height, which it is whatever it
                // holds; a height its ratio makes of its width is at least what
                // it holds (CSS Sizing 4 §4.3), so that one is measured too.
                let space = self.content_space(item.style, containing_width, content_width);
                let named = self.asked_height(item.style, containing_width).is_some();
                let content = (!named || limits_ask_content(item.style))
                    .then(|| self.content_block_size(item.id, content_width, space));
                let heights =
                    self.block_sizes_of(item.style, containing_width, content_width, content);
                // `auto` is asked for only where nothing was named, and then the
                // content was measured.
                (heights.used(content.unwrap_or_default()), heights)
            }
        };
        (height + frame.block, heights.limits.outer(frame.block))
    }

    /// One item laid out for good, at the rectangle its container decided for
    /// it: the only time the container lays it out, since everything before
    /// this only measured it.
    ///
    /// Its contents are the root of a formatting context of their own (CSS
    /// Flexbox §4, CSS Grid 2 §6), and what does not fit is cut off at its padding edge, and
    /// scrolls, where `overflow` says so — here, where it is, and nowhere else.
    pub(in crate::flow) fn layout_item(
        &mut self,
        item: &ItemBox<'_>,
        origin: (f32, f32),
        width: f32,
        height: ItemHeight,
        containing_width: f32,
    ) -> Fragment {
        let (x, y) = origin;
        let style = &item.style;
        let frame = item.frame();
        let content_width = item.content_width(width);

        // A picture is its own content: the container decided the outer size,
        // the frame comes out of it, and a height the line left open is what
        // the width makes of the picture through its ratio.
        if let BoxKind::Replaced(content) = &self.tree.node(item.id).kind {
            let content_height = match height {
                ItemHeight::Exactly { height, .. } => (height - frame.block).max(0.0),
                ItemHeight::AtLeast(floor) => replaced_height(
                    style,
                    content,
                    content_width,
                    containing_width,
                    self.containing_height,
                )
                .max(floor - frame.block),
            };
            return replaced_fragment(
                item.id,
                style,
                content.image.clone(),
                origin,
                (content_width, content_height),
                containing_width,
            );
        }

        let (padding, border) = (item.padding, item.border);
        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;

        // A height of its own is the height it gets, whatever it holds — an item
        // that overflows the size it asked for is what CSS says happens — and a
        // percentage inside it is of that height.
        let height = match height {
            ItemHeight::AtLeast(floor) => match self.asked_height(style, containing_width) {
                Some(_) => ItemHeight::Exactly {
                    height: floor,
                    definite: true,
                },
                None => ItemHeight::AtLeast(floor),
            },
            exactly @ ItemHeight::Exactly { .. } => exactly,
        };
        // Its contents are laid out in the height the container gave it, where
        // it gave one, and otherwise in what the item's own limits allow.
        let space = match height {
            ItemHeight::Exactly { height, definite } => {
                BlockSpace::exactly((height - frame.block).max(0.0), definite)
            }
            ItemHeight::AtLeast(_) => self.content_space(style, containing_width, content_width),
        };
        let mut children = Vec::new();
        let content_height = self.layout_independent(
            item.id,
            content_width,
            (content_x, content_y),
            space,
            &mut children,
        );
        let outer_height = match height {
            ItemHeight::Exactly { height, .. } => height,
            ItemHeight::AtLeast(floor) => {
                let content_height = self.content_height(item.id, content_height);
                let held = self
                    .block_sizes_of(style, containing_width, content_width, Some(content_height))
                    .used(content_height);
                floor.max(held + frame.block)
            }
        };

        if style.overflow.clips() {
            let padding_box = Rect::new(
                x + border.left,
                y + border.top,
                (width - border.left - border.right).max(0.0),
                (outer_height - border.top - border.bottom).max(0.0),
            );
            self.clip_overflow(item.id, padding_box, padding, &mut children);
        }

        Fragment {
            used: Some(crate::UsedEdges {
                margin: item.margin,
                border,
                padding,
            }),
            ..Fragment::for_box(
                item.id,
                Rect::new(x, y, width, outer_height),
                Arc::clone(style),
                children,
            )
        }
    }
}
