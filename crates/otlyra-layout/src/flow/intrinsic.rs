//! Intrinsic widths: how wide a box would be if nothing wrapped, and how narrow
//! it can be without spilling.
//!
//! Floats, flex items, table columns, grid tracks and inline blocks all start
//! from one of these two numbers, and both cost a shaping pass over every word
//! in the box. So they are answered in one place, and each answer is kept for
//! the next time the same question is asked.
//!
//! Two questions, kept apart (CSS Sizing 3 §5). A box's *size* is what its
//! content comes to, whatever its own `width` says; its *contribution* is what it
//! asks of the box it is in, which is its own sizing properties applied to that.
//! Only the first is kept, because it is the one that costs a shaping pass and
//! the one a box that asks for `min-content` asks of itself.

use crate::box_tree::BoxId;

use super::Flow;
use super::box_model::resolve_margin;
use super::inline::{ReplacedBox, inline_spacers};
use super::sizing::{Frame, InlineRoom};

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
    /// contribution only; the picture's size is still its own. Margins are the
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
        self.inline_sizes(id, &style, room, frame)
            .used_border_box(frame, || self.content_size(id, containing_width, wanted))
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
        let node = self.tree.node(id);
        let style = self.style_of(id);
        let extra = Frame::of(&style, containing_width).inline;
        // A picture and a widget are as wide as they are whatever they hold — an
        // empty field as a full one — at their widest as at their narrowest (CSS
        // Sizing 3 §5.1), and a widget's text is what it shows rather than what
        // sizes it.
        if let Some(own) = self.own_width(id, &style, containing_width) {
            return own.natural + extra;
        }

        if node.children.is_empty() {
            return extra;
        }
        let inner = if matches!(
            style.display,
            otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex
        ) && style.flex_direction.is_row()
            && style.flex_wrap == otlyra_css::FlexWrap::NoWrap
        {
            // A row of flex items is as wide as its items laid side by side, plus
            // the gaps. The block branch below takes the widest of them, which is
            // right for boxes that stack and wrong for boxes that sit in a row —
            // and a flex item is blockified, so it never reaches the inline branch
            // that would have summed it. A logo beside a wordmark came out as wide
            // as the wordmark alone, and the wordmark was drawn over what came next.
            let children = node.children.clone();
            let gaps =
                style.gap.1.resolve(containing_width) * children.len().saturating_sub(1) as f32;
            children
                .into_iter()
                .map(|child| {
                    let child_style = &self.tree.node(child).style;
                    let margin = resolve_margin(child_style, containing_width);
                    self.contribution(child, containing_width, None, Wanted::Widest)
                        + margin.left
                        + margin.right
                })
                .sum::<f32>()
                + gaps
        } else if self.tree.node(node.children[0]).is_inline_level() {
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
            let children = node.children.clone();
            let mut widest: f32 = 0.0;
            let mut run: f32 = 0.0;
            for child in children {
                let (floated, clears, margin) = {
                    let style = &self.tree.node(child).style;
                    (
                        style.float != otlyra_css::Float::None,
                        style.clear != otlyra_css::Clear::None,
                        resolve_margin(style, containing_width),
                    )
                };
                let width = self.contribution(child, containing_width, None, Wanted::Widest)
                    + margin.left
                    + margin.right;
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
        let node = self.tree.node(id);
        let style = self.style_of(id);
        let extra = Frame::of(&style, containing_width).inline;
        // A picture's own width and a widget's, as at their widest. How far a
        // picture with `max-width: 100%` lets a column shrink is what it
        // contributes to the column (see [`Self::contribution`]); what it is
        // stays what it is, and is how far a flex item may be shrunk.
        if let Some(own) = self.own_width(id, &style, containing_width) {
            return own.natural + extra;
        }

        if node.children.is_empty() {
            return extra;
        }
        let inner = if matches!(
            style.display,
            otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex
        ) && style.flex_direction.is_row()
            && style.flex_wrap == otlyra_css::FlexWrap::NoWrap
        {
            // A row of flex items is one unbreakable run: the items sit beside
            // each other and no shrinking moves one below another, so what the
            // row needs at its narrowest is the sum of what its items need plus
            // the gaps between them. Falling through to the inline branch below
            // takes the *widest* item instead, which for a mark beside a word is
            // the wider of the two rather than the two of them — and the item is
            // then floored at a width its own contents overflow. That is what cut
            // the site's wordmark off beside its logo.
            let children = node.children.clone();
            let gaps =
                style.gap.1.resolve(containing_width) * children.len().saturating_sub(1) as f32;
            children
                .into_iter()
                .map(|child| {
                    let child_style = &self.tree.node(child).style;
                    let margin = resolve_margin(child_style, containing_width);
                    self.contribution(child, containing_width, None, Wanted::Narrowest)
                        + margin.left
                        + margin.right
                })
                .sum::<f32>()
                + gaps
        } else if self.tree.node(node.children[0]).is_inline_level() {
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
            let children = node.children.clone();
            children
                .into_iter()
                .map(|child| {
                    let child_style = &self.tree.node(child).style;
                    let margin = resolve_margin(child_style, containing_width);
                    self.contribution(child, containing_width, None, Wanted::Narrowest)
                        + margin.left
                        + margin.right
                })
                .fold(0.0, f32::max)
        };

        inner + extra
    }
}
