//! Positioned boxes: `relative`, `absolute` and `fixed`.
//!
//! A positioned box is laid out like any other and then placed against something
//! other than the cursor: where the flow put it, its containing block, or the
//! viewport. Which box is the containing block is kept as a stack while its
//! descendants are laid out, and making a box one is a step of its own that the
//! block and flex contexts both go through.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, LengthOrAuto};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{Fragment, Rect};

use super::box_model::resolve_padding;
use super::sizing::{Frame, InlineRoom};
use super::{ContainingBlock, Flow, mark_fixed, mark_layer, offset};

/// How far `relative` moves a box from where the flow put it.
///
/// `left` wins over `right` and `top` over `bottom`, which is what CSS says for a
/// box that names both and cannot honour the two of them.
pub(super) fn relative_offset(style: &ComputedStyle, containing: f32) -> (f32, f32) {
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

impl<'a> Flow<'a> {
    /// Lay out a box's children, making it the containing block for the absolutely
    /// positioned ones if its `position` says so.
    ///
    /// `content_height` is the box's own height, when it has one before its
    /// contents are laid out: it is what a percentage height inside the box is
    /// of (CSS 2.2 §10.5), and a flex container inside it is that tall.
    ///
    /// The height a containing block offers a positioned box is that height, or
    /// the rest of the page when it has none: what a percentage inset resolves
    /// against is the padding box, and a box whose height is its content's is not
    /// measured until its content — including these very children — has been
    /// laid out.
    pub(super) fn layout_inside(
        &mut self,
        id: BoxId,
        content_width: f32,
        content_x: f32,
        content_y: f32,
        content_height: Option<f32>,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let outer_height = std::mem::replace(&mut self.containing_height, content_height);
        let used =
            self.layout_contents(id, content_width, content_x, content_y, content_height, out);
        self.containing_height = outer_height;
        used
    }

    /// The children of a box, with the box pushed as a containing block while
    /// they are laid out when its `position` makes it one.
    fn layout_contents(
        &mut self,
        id: BoxId,
        content_width: f32,
        content_x: f32,
        content_y: f32,
        content_height: Option<f32>,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let style = Arc::clone(&self.tree.node(id).style);
        if !style.position.is_containing_block() {
            return self.layout_children(id, content_width, content_x, content_y, out);
        }

        let padding = resolve_padding(&style, content_width);
        let height =
            content_height.unwrap_or_else(|| (self.viewport.bottom() - content_y).max(0.0));
        self.containing_blocks.push(ContainingBlock {
            rect: Rect::new(
                content_x - padding.left,
                content_y - padding.top,
                content_width + padding.left + padding.right,
                height + padding.top + padding.bottom,
            ),
            height: content_height.map(|height| height + padding.top + padding.bottom),
        });

        let used = self.layout_children(id, content_width, content_x, content_y, out);
        self.containing_blocks.pop();
        used
    }

    /// Place an absolutely or fixed positioned box against its containing block.
    ///
    /// `static_y` is where the box would have been in the flow, which is what an
    /// `auto` inset resolves to — a positioned box with no insets stays where it
    /// was and only leaves the flow.
    pub(super) fn layout_positioned(&mut self, id: BoxId, static_y: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);
        let block = if style.position == otlyra_css::Position::Fixed {
            ContainingBlock {
                rect: self.viewport,
                height: Some(self.viewport.height),
            }
        } else {
            *self
                .containing_blocks
                .last()
                .expect("the initial containing block is always there")
        };
        let area = block.rect;

        let inset = |value: &LengthOrAuto, against: f32| value.resolve(against);
        let left = inset(&style.inset.left, area.width);
        let right = inset(&style.inset.right, area.width);
        let top = inset(&style.inset.top, area.height);
        let bottom = inset(&style.inset.bottom, area.height);

        // A percentage height is of the box it is placed against, not of the
        // one it happened to sit in — and so is the width a picture takes from
        // its height.
        let outer_height = std::mem::replace(&mut self.containing_height, block.height);

        // The border-box width (CSS 2.2 §10.3.7): what it asks for; with an
        // `auto` width, what its two insets leave between them, or when an edge
        // is free what its content wants of what there is; and the minimum and
        // maximum over either. A picture is its own width between two insets as
        // anywhere else (§10.3.8), and its margins take up the rest.
        let frame = Frame::of(&style, area.width).inline;
        let room = InlineRoom::laid_out(
            area.width,
            area.width - left.unwrap_or(0.0) - right.unwrap_or(0.0),
        );
        let picture = matches!(self.tree.node(id).kind, BoxKind::Replaced(_));
        let width = match (left, right) {
            (Some(left), Some(right)) if !picture => self
                .inline_sizes(id, &style, room, frame)
                .used_border_box(frame, || (area.width - left - right).max(0.0)),
            _ => self.shrink_to_fit_width(id, &style, room, frame),
        };

        // A float outside does not reach into a positioned box, and one inside does
        // not reach out.
        let outer_floats = std::mem::take(&mut self.floats);
        let mut fragment = self.layout_sized(id, area.x, area.y, width);
        self.containing_height = outer_height;
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
}
