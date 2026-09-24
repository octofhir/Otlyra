//! Intrinsic sizes: how wide a box would be if nothing wrapped, how narrow it
//! can be without spilling, and how tall its contents come to at a given width.
//!
//! Floats, flex items, table columns, grid tracks and inline blocks all start
//! from one of the two widths, and both cost a shaping pass over every word in
//! the box; a flex item's height costs a layout of everything inside it. So they
//! are answered in one place, and each answer is kept for the next time the same
//! question is asked.
//!
//! Two questions, kept apart (CSS Sizing 3 §5). A box's *size* is what its
//! content comes to, whatever its own `width` says; its *contribution* is what it
//! asks of the box it is in, which is its own sizing properties applied to that.
//! Only the first is kept, because it is the one that costs a shaping pass and
//! the one a box that asks for `min-content` asks of itself.

use otlyra_css::{ComputedStyle, Display, FlexWrap};

use crate::box_tree::BoxId;
use crate::fragment::Fragment;

use super::Flow;
use super::box_model::resolve_margin;
use super::inline::{ReplacedBox, inline_spacers};
use super::sizing::{BlockSpace, Frame, InlineRoom};

/// Which of the two intrinsic sizes is wanted, so that two of them cannot be
/// mistaken for one another in the answers already worked out.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Wanted {
    /// How wide it would be if nothing wrapped: the max-content size.
    Widest,
    /// How narrow it can be without spilling: the min-content size.
    Narrowest,
}

impl<'a> Flow<'a> {
    /// The border-box width a box's contents come to under `wanted`, whatever its
    /// own `width` says.
    pub(super) fn content_size(&mut self, id: BoxId, containing_width: f32, wanted: Wanted) -> f32 {
        match wanted {
            Wanted::Widest => self.max_content_size(id, containing_width),
            Wanted::Narrowest => self.min_content_size(id, containing_width),
        }
    }

    /// What a box asks of the box it is in, as a border-box width: its own
    /// `width`, `min-width` and `max-width` applied to what its contents come to.
    ///
    /// A percentage in them is of `basis`, and while the very box they are a
    /// percentage of is being measured there is none: the percentage is cyclic,
    /// and the box contributes as though it were `auto` (CSS Sizing 3 §5.2.1).
    /// A picture and a widget are the exception: each has a size of its own, and
    /// `img { max-width: 100% }` or `input { width: 100% }` asks nothing of a
    /// column at its narrowest (§5.2.2 — see [`replaced_widths`]). That is the
    /// contribution only; the picture's size is still its own. A box with a
    /// preferred aspect ratio and a definite height contributes the width the
    /// ratio makes of that height (CSS Sizing 4 §4.2). Margins are the
    /// caller's, since only the caller knows which it wants.
    pub(super) fn contribution(
        &mut self,
        id: BoxId,
        containing_width: f32,
        basis: Option<f32>,
        wanted: Wanted,
    ) -> f32 {
        self.ensure_collapsed(id);
        let style = self.style_of(id);
        let frame = Frame::of(&style, containing_width).inline;
        let room = InlineRoom::measuring(containing_width, basis, wanted);
        let sizes = self.inline_sizes(id, &style, room, frame);
        sizes.used_border_box(frame, || {
            self.automatic_width(id, &style, room, frame, |flow| {
                flow.content_size(id, containing_width, wanted)
            })
        })
    }

    /// Size the boxes that sit on a line being measured as words do — pictures,
    /// inline blocks, widgets — by what each contributes to it, rather than by
    /// how it would be laid out on a line `containing_width` wide.
    ///
    /// A percentage of the width being measured is as cyclic on a line as on a
    /// line of its own (CSS Sizing 3 §5.2.1), and a box that is a block and one
    /// that is a word have to ask the same of the box they are in: at its
    /// narrowest an inline block is as narrow as its own content can be, not as
    /// wide as it would be laid out.
    fn measure_atomic_inlines(
        &mut self,
        atomic: &mut [ReplacedBox],
        containing_width: f32,
        wanted: Wanted,
    ) {
        for box_ in atomic {
            box_.width = self.contribution(box_.id(), containing_width, None, wanted);
        }
    }

    /// The widest a box would be if nothing made it wrap.
    ///
    /// CSS calls this the max-content size, and a flex item with no width of its
    /// own starts from it: `display: flex` on three words puts three words on a
    /// line, not three equal columns. Measured by asking the shaper for the
    /// paragraph's own width and by walking blocks for the widest of them.
    pub(super) fn max_content_size(&mut self, id: BoxId, containing_width: f32) -> f32 {
        let key = (id, containing_width.to_bits(), Wanted::Widest);
        if let Some(&answer) = self.measured.get(&key) {
            return answer;
        }
        let answer = self.max_content_size_uncached(id, containing_width);
        self.measured.insert(key, answer);
        answer
    }

    fn max_content_size_uncached(&mut self, id: BoxId, containing_width: f32) -> f32 {
        self.ensure_collapsed(id);
        let style = self.style_of(id);
        let extra = Frame::of(&style, containing_width).inline;
        // A picture and a widget are as wide as they are whatever they hold — an
        // empty field as a full one — at their widest as at their narrowest (CSS
        // Sizing 3 §5.1), and a widget's text is what it shows rather than what
        // sizes it. A percentage height is of nothing while a width is
        // measured (see `heights_against`).
        if let Some(own) = self.own_width(id, &style, containing_width, None) {
            return own.natural + extra;
        }

        let children = self.contributing_children(id);
        let Some(&first) = children.first() else {
            return extra;
        };
        let inner = if is_flex_container(&style) {
            self.flex_content_size(&style, children, containing_width, Wanted::Widest)
        } else if self.tree.node(first).is_inline_level() {
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
            self.measure_atomic_inlines(&mut replaced, containing_width, Wanted::Widest);
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
        } else {
            // Boxes that stack need the widest of them. Floated siblings do
            // not stack — they sit side by side until one clears or
            // something that is not a float comes between them — so a run of
            // them needs what it adds up to, the way a row of flex items
            // does. Taking the widest of a row of floated links is asking a
            // table for a column one word across and getting the links back
            // one to a line.
            let mut widest: f32 = 0.0;
            let mut run: f32 = 0.0;
            for child in children {
                let (floated, clears) = {
                    let style = &self.tree.node(child).style;
                    (
                        style.float != otlyra_css::Float::None,
                        style.clear != otlyra_css::Clear::None,
                    )
                };
                let width = self.outer_contribution(child, containing_width, Wanted::Widest);
                run = match (floated, clears) {
                    (true, false) => run + width,
                    (true, true) => width,
                    (false, _) => 0.0,
                };
                widest = widest.max(run).max(width);
            }
            widest
        };

        inner + extra
    }

    /// The narrowest a box can be without its content spilling out of it.
    ///
    /// CSS calls this the min-content size: the widest single unbreakable thing
    /// inside, which for text is its longest word. It is what a flex item may not
    /// be shrunk below and what a float with no width of its own shrinks to — and
    /// both of those ask it of the content whatever width the box declared, which
    /// is why a box's own `width` is not part of it: a flex item that says
    /// `width: 300px` may still be shrunk, just not past its longest word.
    pub(super) fn min_content_size(&mut self, id: BoxId, containing_width: f32) -> f32 {
        let key = (id, containing_width.to_bits(), Wanted::Narrowest);
        if let Some(&answer) = self.measured.get(&key) {
            return answer;
        }
        let answer = self.min_content_size_uncached(id, containing_width);
        self.measured.insert(key, answer);
        answer
    }

    fn min_content_size_uncached(&mut self, id: BoxId, containing_width: f32) -> f32 {
        self.ensure_collapsed(id);
        let style = self.style_of(id);
        let extra = Frame::of(&style, containing_width).inline;
        // A picture's own width and a widget's, as at their widest. How far a
        // picture with `max-width: 100%` lets a column shrink is what it
        // contributes to the column (see [`Self::contribution`]); what it is
        // stays what it is, and is how far a flex item may be shrunk.
        if let Some(own) = self.own_width(id, &style, containing_width, None) {
            return own.natural + extra;
        }

        let children = self.contributing_children(id);
        let Some(&first) = children.first() else {
            return extra;
        };
        let inner = if is_flex_container(&style) {
            self.flex_content_size(&style, children, containing_width, Wanted::Narrowest)
        } else if self.tree.node(first).is_inline_level() {
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
            self.measure_atomic_inlines(&mut replaced, containing_width, Wanted::Narrowest);
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
            let atomic = replaced.iter().map(|box_| box_.width).fold(0.0, f32::max);
            text.max(atomic)
        } else {
            children
                .into_iter()
                .map(|child| self.outer_contribution(child, containing_width, Wanted::Narrowest))
                .fold(0.0, f32::max)
        };

        inner + extra
    }

    /// The children a box's intrinsic sizes are made of: all of them but the
    /// absolutely positioned ones.
    ///
    /// An absolutely positioned box is out of flow (CSS Position 3 §3, CSS 2.2
    /// §9.6): it is placed against a containing block of its own and takes no
    /// room where it sits, so it contributes nothing to its parent's min-content
    /// and max-content sizes (CSS Sizing 3 §5), and in a flex container it is
    /// not a flex item (CSS Flexbox 1 §4.1). Counted, a nav link's hidden
    /// drop-down made the link as wide as the whole menu, and a header's empty
    /// holder of positioned alerts took their width from the row. A float stays
    /// among them: it is out of flow too, but the lines beside it make room for
    /// it, and a box shrink-wrapped round it is as wide as it is.
    ///
    /// A positioned box among words is still measured with them. The builder
    /// makes a block of each run of words beside a positioned block, so that
    /// one never reaches a line; but a positioned picture stays in its run, and
    /// `collect_spans` measures and places it as part of the line, as it does
    /// whatever it reaches through a block inside an inline. That walk lays the
    /// line out as well, and taking positioned boxes out of a line belongs with
    /// inline layout rather than here.
    fn contributing_children(&self, id: BoxId) -> Vec<BoxId> {
        self.tree
            .node(id)
            .children
            .iter()
            .copied()
            .filter(|&child| !self.tree.node(child).is_absolutely_positioned())
            .collect()
    }

    /// What a child asks of the box it is in, margins and all: the outer
    /// contribution its parent's intrinsic sizes are made of (CSS Sizing 3 §5.1).
    fn outer_contribution(&mut self, child: BoxId, containing_width: f32, wanted: Wanted) -> f32 {
        let margin = resolve_margin(&self.tree.node(child).style, containing_width);
        self.contribution(child, containing_width, None, wanted) + margin.left + margin.right
    }

    /// What a flex container's items come to across its width: its content
    /// size under `wanted` (CSS Flexbox 1 §9.9).
    ///
    /// Along a row the items sit side by side, so at its widest the row is
    /// their outer contributions added up with the gaps between them, whether
    /// or not it may wrap (§9.9.1): wrapping is what a row does when it is
    /// given less than that, not what it asks for. Taking the widest item
    /// instead stacked a wrapping nav one link to a line. At its narrowest a
    /// single line is still all of them — shrinking never moves one item below
    /// another, and a logo that floored its row at the wordmark beside it had
    /// the wordmark cut off — while a row that wraps can give each item a line
    /// of its own and needs only the widest.
    ///
    /// Down a column the width is the cross size, and a single line's is the
    /// widest of its items' (§9.9.2). A floated item is not a float — `float`
    /// does not apply to a flex item (§3) — so there is no run of floats to add
    /// up the way a block's content does. A column that wraps is taken as one
    /// line too: how many lines it would make depends on its height, and the
    /// sum of those lines that §9.9.2 asks for is not worked out here.
    fn flex_content_size(
        &mut self,
        style: &ComputedStyle,
        items: Vec<BoxId>,
        containing_width: f32,
        wanted: Wanted,
    ) -> f32 {
        let gaps = style.gap.1.resolve(containing_width) * items.len().saturating_sub(1) as f32;
        let contributions = items
            .into_iter()
            .map(|item| self.outer_contribution(item, containing_width, wanted));
        if items_side_by_side(style, wanted) {
            contributions.sum::<f32>() + gaps
        } else {
            contributions.fold(0.0, f32::max)
        }
    }

    /// How tall a box's contents come to laid out `content_width` wide, in the
    /// block space `space` (see [`BlockSpace`]): its max-content block size,
    /// which for a block container is its min-content block size too (CSS
    /// Sizing 3 §5).
    ///
    /// Measured by laying the contents out and throwing them away, and kept by
    /// the box and exactly what they were laid out against: the width, and the
    /// box's own height and limits already resolved, never its containing
    /// block's. The
    /// same question is asked each time a container is laid out again, so every
    /// time after the first is an answer rather than a layout (see `flex`).
    ///
    /// A measure leaves nothing behind. What it laid out is at the origin rather
    /// than anywhere real, so a scroll port it registered is dropped, and a list
    /// item's marker waiting for its first line is still waiting for the line it
    /// will really be laid out on. A widget that has a height of its own is that
    /// height whatever it holds, and is not laid out at all.
    pub(super) fn content_block_size(
        &mut self,
        id: BoxId,
        content_width: f32,
        space: BlockSpace,
    ) -> f32 {
        if let Some(natural) = self.tree.node(id).natural_size().height {
            return natural;
        }
        let key = (id, content_width.to_bits(), space.bits());
        if let Some(&height) = self.measured_heights.get(&key) {
            return height;
        }
        let ports = self.scroll_ports.len();
        let marker = self.pending_marker.take();
        let mut discarded = Vec::new();
        let height = self.layout_independent(id, content_width, (0.0, 0.0), space, &mut discarded);
        self.scroll_ports.truncate(ports);
        self.pending_marker = marker;
        self.measured_heights.insert(key, height);
        height
    }

    /// Lay out the contents of a box that is the root of a formatting context of
    /// its own, as a flex item is (CSS Flexbox §4), and say how tall they came to.
    ///
    /// No float outside it reaches into it, none inside reaches out, and it is
    /// tall enough to hold the ones it has (CSS 2.2 §10.6.7). A table inside
    /// it reports how wide it turned out to be, and the box keeps the width it
    /// was given instead (see `table_width`).
    pub(super) fn layout_independent(
        &mut self,
        id: BoxId,
        content_width: f32,
        (x, y): (f32, f32),
        space: BlockSpace,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let outer_floats = std::mem::take(&mut self.floats);
        let used = self.layout_inside(id, content_width, x, y, space, out);
        let floats = std::mem::replace(&mut self.floats, outer_floats);
        self.table_width = None;
        let reach = floats
            .iter()
            .map(|float| float.rect.bottom())
            .fold(y, f32::max);
        used.max(reach - y)
    }
}

/// Whether `display` makes a box a flex container, whose children are flex
/// items whichever way the box itself is placed.
fn is_flex_container(style: &ComputedStyle) -> bool {
    matches!(style.display, Display::Flex | Display::InlineFlex)
}

/// Whether a flex container's items lie side by side across its width when it
/// is as wide, or as narrow, as `wanted` says (CSS Flexbox 1 §9.9.1): along a
/// row at its widest they always do, and at its narrowest only on a single
/// line; down a column they never do.
fn items_side_by_side(style: &ComputedStyle, wanted: Wanted) -> bool {
    let single_line = style.flex_wrap == FlexWrap::NoWrap;
    style.flex_direction.is_row() && (wanted == Wanted::Widest || single_line)
}
