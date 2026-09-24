//! Block layout: boxes that stack.
//!
//! The block formatting context, which is what most of the web is made of: each
//! child goes below the last, the margins between them collapse, and a box
//! settles its own width and height around what it holds. It is also where a
//! container is handed to the context its `display` asks for, since every box
//! arrives here first.

use std::sync::Arc;

use otlyra_css::{Clear, Float, Sides};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{Fragment, Rect, ScrollPort, Sticky};

use super::box_model::{
    collapse, resolve_border, resolve_horizontal, resolve_margin, resolve_padding, vertical_margin,
};
use super::list::PendingMarker;
use super::positioned::relative_offset;
use super::replaced::{replaced_fragment, replaced_height, replaced_size};
use super::sizing::{Frame, InlineRoom, Sizes, height_ratio};
use super::{
    Flow, is_popup, mark_layer, mark_sticky, offset, set_clip, set_container, set_scroll_port,
    set_sticky_containers, shift,
};

/// What a box's bottom edge does with the bottom margin of the last box in it
/// (CSS 2.2 §8.3.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BottomEdge {
    /// Lets it through, to collapse with the box's own: nothing stands on the
    /// edge, and the box is as tall as what it holds.
    Open,
    /// Keeps it inside, where it is part of what the box holds: a border,
    /// padding, a line of text, a height of its own or a formatting context of
    /// its own stands on the edge.
    Closed,
    /// Neither: the box's height is the one its preferred aspect ratio makes of
    /// its width, which margins do not collapse through, as they do not through
    /// a height of the page's (CSS Sizing 4 §4.2.1). What it holds is counted
    /// only as its min-content height — its height were it `auto`, the margin
    /// gone through the edge — which the box grows to where that is taller than
    /// the ratio makes it (§4.3). So a card whose text ends in a paragraph is as
    /// tall as the text and not a margin taller.
    Ratio,
}

impl BottomEdge {
    /// Whether the last box's bottom margin is part of what the box holds.
    fn keeps_margin(self) -> bool {
        match self {
            Self::Closed => true,
            Self::Open | Self::Ratio => false,
        }
    }
}

impl<'a> Flow<'a> {
    /// Lay out the children of `parent` into a content box starting at
    /// (`x`, `y`) and `width` wide. Returns the height they used.
    pub(super) fn layout_children(
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
        let (top_open, bottom_edge) = self.open_edges(parent, width);
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

            let bottom = if index == last && !bottom_edge.keeps_margin() {
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

    /// A block laid out at a width the caller decided, rather than one worked out
    /// from its containing block.
    pub(super) fn layout_sized(&mut self, id: BoxId, x: f32, y: f32, width: f32) -> Fragment {
        let style = Arc::clone(&self.tree.node(id).style);

        // A picture is its own content: it has no children to lay out and its
        // height comes from its own proportions rather than from anything inside it.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            // The caller decided the *outer* width, so what is left for the
            // picture is that less the frame around it, and the height follows
            // from that.
            let frame = Frame::of(&style, width);
            let inner = (width - frame.inline).max(0.0);
            let height = replaced_height(&style, content, inner, width, self.containing_height);
            return replaced_fragment(
                id,
                &style,
                content.image.clone(),
                (x, y),
                (inner, height),
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
        // What the box's own contents resolve a percentage height against — its
        // height, when it has one to give — and the limits it is held between.
        let space = self.content_space(&style, width, content_width);
        let content_height = self.layout_inside(
            id,
            content_width,
            content_x,
            content_y,
            space,
            &mut children,
        );
        // A box laid out at a width the caller chose keeps it, table or not — a
        // flex item is as wide as its line gave it. Taken rather than left, so a
        // table inside one does not report its width to the block outside.
        self.table_width = None;
        let content_height = self.content_height(id, content_height);
        let content_height = self
            .block_sizes_of(&style, width, content_width, Some(content_height))
            .used(content_height);

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

        Fragment::for_box(
            id,
            Rect::new(
                x,
                y,
                width,
                content_height + padding.top + padding.bottom + border.top + border.bottom,
            ),
            style,
            children,
        )
    }

    /// What `overflow` other than `visible` does to a box once its contents are
    /// laid out: they are cut off at its padding box, and when they reach further
    /// down than it shows, the box is a scroll port for them.
    ///
    /// The rectangle is handed down rather than pushed as a layer, so a fragment
    /// carries the one rectangle it is cut off at however deep it is.
    pub(super) fn clip_overflow(
        &mut self,
        id: BoxId,
        padding_box: Rect,
        padding: Sides<f32>,
        children: &mut [Fragment],
    ) {
        for child in children.iter_mut() {
            if !is_popup(self.tree, child) {
                set_clip(child, padding_box);
            }
        }

        // How much there is to see: the furthest any of its contents reaches.
        // More than the box can show is what makes it a scroll port.
        let content_top = padding_box.y + padding.top;
        let reach = children
            .iter()
            .filter(|child| !is_popup(self.tree, child))
            .map(|child| child.rect.bottom())
            .fold(content_top, f32::max);
        let inside = reach - content_top + padding.top + padding.bottom;
        if inside > padding_box.height + 0.5 {
            self.scroll_ports.push(ScrollPort {
                id,
                port: padding_box,
                content_height: inside,
            });
            for child in children.iter_mut() {
                set_scroll_port(child, id);
            }
        }
    }

    /// Whether margins pass through the top edge of `id`, and what its bottom
    /// edge does with them.
    ///
    /// An edge is open when nothing sits on it: no border, no padding, and no line
    /// of text, since a line box is content and content is what a margin cannot
    /// pass through. The root is closed at both ends however empty it is — a
    /// document's own margins stay inside it.
    fn open_edges(&self, id: BoxId, containing_width: f32) -> (bool, BottomEdge) {
        if id == self.tree.root() {
            return (false, BottomEdge::Closed);
        }
        let node = self.tree.node(id);
        if node
            .children
            .first()
            .is_some_and(|&child| self.tree.node(child).is_inline_level())
        {
            return (false, BottomEdge::Closed);
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
            return (false, BottomEdge::Closed);
        }

        let border = resolve_border(style);
        let padding = resolve_padding(style, containing_width);
        // A height of its own stops a margin at the bottom edge: the box ends where
        // it says it does, not where its last child does. A percentage of a
        // height nobody knows is not one, and neither is a keyword (CSS 2.2
        // §8.3.1: the margins meet through a box whose height is `auto`).
        let asked = self.asked_height(style, containing_width);
        let bottom = if border.bottom != 0.0 || padding.bottom != 0.0 || asked.is_some() {
            BottomEdge::Closed
        } else if height_ratio(style, asked).is_some() {
            BottomEdge::Ratio
        } else {
            BottomEdge::Open
        };

        (border.top == 0.0 && padding.top == 0.0, bottom)
    }

    /// The margin `id` presents to whatever is above it, including any that
    /// escaped from its own first children.
    fn collapsed_top(&self, id: BoxId, containing_width: f32) -> f32 {
        let node = self.tree.node(id);
        let mut margin = vertical_margin(&node.style.margin.top, containing_width);
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
        let mut margin = vertical_margin(&node.style.margin.bottom, containing_width);
        if self.open_edges(id, containing_width).1 == BottomEdge::Open
            && let Some(&last) = node.children.last()
        {
            margin = collapse(margin, self.collapsed_bottom(last, containing_width));
        }
        margin
    }

    /// One block-level box: margins, borders, padding, a width, and whatever it
    /// contains.
    pub(super) fn layout_block(
        &mut self,
        id: BoxId,
        containing_width: f32,
        x: f32,
        y: f32,
    ) -> Fragment {
        self.ensure_collapsed(id);
        let style = self.style_of(id);
        // A box that cuts its contents off is a formatting context of its own: the
        // floats outside it do not shorten the lines inside it, and its own do not
        // reach out. This is the rule `overflow: hidden` is best known for.
        //
        // A table cell and an `inline-block` are roots of one for the same reason
        // and without saying so. Leaving them out is what let a footer laid out as
        // a table of floated links collapse: the floats escaped the cell, the cell
        // came out empty, and every row landed on the one above it.
        let root = style.overflow == otlyra_css::Overflow::Clip
            || matches!(
                style.display,
                otlyra_css::Display::TableCell | otlyra_css::Display::InlineBlock
            );
        let outer_floats = root.then(|| std::mem::take(&mut self.floats));
        // A block-level picture is its own size and has no children to lay out;
        // everything else about it — margins, borders — is an ordinary block's.
        if let BoxKind::Replaced(content) = &self.tree.node(id).kind {
            let room = InlineRoom::within(&style, containing_width);
            let (width, height) = replaced_size(&style, content, room, self.containing_height);
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
        let frame = Frame::new(padding, border);
        let room = InlineRoom::within(&style, containing_width);
        let sizes = self.inline_sizes(id, &style, room, frame.inline);
        // An `auto` width fills the line, unless the box has a preferred aspect
        // ratio and a height to take its width from (CSS Sizing 4 §4.2): then
        // it is that width, and its margins are what is left, as a picture's
        // are.
        let sizes = Sizes {
            preferred: sizes
                .preferred
                .or_else(|| self.ratio_width(id, &style, room)),
            ..sizes
        };
        let (margin, content_width) = resolve_horizontal(
            &style,
            containing_width,
            resolve_margin(&style, containing_width),
            frame.inline,
            sizes,
        );

        let border_x = x + margin.left;
        let border_y = y;
        let content_x = border_x + border.left + padding.left;
        let content_y = border_y + border.top + padding.top;

        let mut children = Vec::new();
        self.table_width = None;
        // A table with no width of its own shrinks to its columns, but not past
        // its minimum; the table is told what that is (see `table_floor`).
        let shrinks = style.display == otlyra_css::Display::Table && sizes.preferred.is_none();
        self.table_floor = if shrinks { sizes.limits.min } else { 0.0 };
        // What this box's contents resolve a percentage height against — its own
        // height, when it has one to give them — and the limits it is held
        // between.
        let space = self.content_space(&style, containing_width, content_width);
        let mut content_height = self.layout_inside(
            id,
            content_width,
            content_x,
            content_y,
            space,
            &mut children,
        );
        self.table_floor = 0.0;
        // A root of a formatting context is at least as tall as the floats inside
        // it. Everywhere else a float is out of the flow and adds nothing to the
        // height of what holds it — which is the whole of what floating means —
        // but nothing outside this box can be moved by them, so a box that did not
        // grow to hold its own would leave them hanging out of its bottom.
        if outer_floats.is_some() {
            let reach = self
                .floats
                .iter()
                .map(|float| float.rect.bottom())
                .fold(content_y, f32::max);
            content_height = content_height.max(reach - content_y);
        }
        // A table with no width of its own is only as wide as its columns turned
        // out to need. One that names a width keeps it, and its columns were
        // stretched to fill it instead.
        let shrunk = self.table_width.take();
        let content_width = match sizes.preferred {
            Some(_) => content_width,
            None => shrunk.unwrap_or(content_width),
        };
        let content_height = self.content_height(id, content_height);
        let content_height = self
            .block_sizes_of(
                &style,
                containing_width,
                content_width,
                Some(content_height),
            )
            .used(content_height);

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

        if style.overflow == otlyra_css::Overflow::Clip {
            let padding_box = Rect::new(
                border_x + border.left,
                border_y + border.top,
                content_width + padding.left + padding.right,
                content_height + padding.top + padding.bottom,
            );
            self.clip_overflow(id, padding_box, padding, &mut children);
        }

        // The border box: the rectangle a background paints and a border is
        // drawn on the inside edge of.
        let border_box = Rect::new(
            border_x,
            border_y,
            content_width + padding.left + padding.right + border.left + border.right,
            content_height + padding.top + padding.bottom + border.top + border.bottom,
        );
        Fragment {
            used: Some(crate::UsedEdges {
                margin,
                border,
                padding,
            }),
            ..Fragment::for_box(id, border_box, style, children)
        }
    }
}
