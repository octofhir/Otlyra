//! Positioned boxes: `relative`, `absolute` and `fixed`.
//!
//! A positioned box is laid out like any other and then placed against something
//! other than the cursor: where the flow put it, its containing block, or the
//! viewport. Which box is the containing block is kept as a stack while its
//! descendants are laid out, and making a box one is a step of its own that the
//! block and flex contexts both go through.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, LengthOrAuto};

use crate::box_tree::BoxId;
use crate::fragment::{Fragment, Rect};

use super::box_model::{resolve_border, resolve_padding};
use super::{Flow, mark_fixed, mark_layer, offset};

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
    /// The height it offers is the height it was given, or the rest of the page
    /// when it has none of its own: what a percentage inset resolves against is the
    /// padding box, and a box whose height is its content's is not measured until
    /// its content — including these very children — has been laid out.
    pub(super) fn layout_inside(
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
    pub(super) fn layout_positioned(&mut self, id: BoxId, static_y: f32) -> Fragment {
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
}
