//! Intrinsic widths: how wide a box would be if nothing wrapped, and how narrow
//! it can be without spilling.
//!
//! Floats, flex items, table columns, grid tracks and inline blocks all start
//! from one of these two numbers, and both cost a shaping pass over every word
//! in the box. So they are answered in one place, and each answer is kept for
//! the next time the same question is asked.

use crate::box_tree::{BoxId, BoxKind};

use super::Flow;
use super::box_model::{resolve_border, resolve_margin, resolve_padding};
use super::inline::inline_spacers;
use super::replaced::replaced_size;

/// Which question was asked of a box's contents, so that two of them cannot be
/// mistaken for one another in the answers already worked out.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Wanted {
    /// How wide it would be if nothing wrapped.
    Widest,
    /// How narrow it can be without spilling.
    Narrowest,
    /// The same, ignoring a width it declared — which is a flex item's automatic
    /// minimum size and a different number.
    NarrowestOfContent,
}

impl<'a> Flow<'a> {
    /// The widest a box would be if nothing made it wrap.
    ///
    /// CSS calls this the max-content size, and a flex item with no width of its
    /// own starts from it: `display: flex` on three words puts three words on a
    /// line, not three equal columns. Measured by asking the shaper for the
    /// paragraph's own width and by walking blocks for the widest of them.
    pub(super) fn max_content_width(&mut self, id: BoxId, containing_width: f32) -> f32 {
        let key = (id, containing_width.to_bits(), Wanted::Widest);
        if let Some(&answer) = self.measured.get(&key) {
            return answer;
        }
        let answer = self.max_content_width_uncached(id, containing_width);
        self.measured.insert(key, answer);
        answer
    }

    fn max_content_width_uncached(&mut self, id: BoxId, containing_width: f32) -> f32 {
        self.ensure_collapsed(id);
        let node = self.tree.node(id);
        let style = self.style_of(id);
        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let extra = padding.left + padding.right + border.left + border.right;

        // A width of its own is the answer, whatever it holds.
        if let Some(width) = style.width.resolve(containing_width) {
            return width + extra;
        }

        let inner = match &node.kind {
            BoxKind::Replaced(content) => replaced_size(&style, content, containing_width).0,
            _ if node.children.is_empty() => 0.0,
            // A row of flex items is as wide as its items laid side by side, plus
            // the gaps. The block branch below takes the widest of them, which is
            // right for boxes that stack and wrong for boxes that sit in a row —
            // and a flex item is blockified, so it never reaches the inline branch
            // that would have summed it. A logo beside a wordmark came out as wide
            // as the wordmark alone, and the wordmark was drawn over what came next.
            _ if matches!(
                style.display,
                otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex
            ) && style.flex_direction.is_row()
                && style.flex_wrap == otlyra_css::FlexWrap::NoWrap =>
            {
                let children = node.children.clone();
                let gaps =
                    style.gap.1.resolve(containing_width) * children.len().saturating_sub(1) as f32;
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.max_content_width(child, containing_width) + margin.left + margin.right
                    })
                    .sum::<f32>()
                    + gaps
            }
            _ if self.tree.node(node.children[0]).is_inline_level() => {
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
            }
            _ => {
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
                    let width = self.max_content_width(child, containing_width)
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
            }
        };

        inner + extra
    }

    /// The narrowest a box can be without its content spilling out of it.
    ///
    /// CSS calls this the min-content size: the widest single unbreakable thing
    /// inside, which for text is its longest word. It is what a flex item may not
    /// be shrunk below, and what a float with no width of its own shrinks to.
    /// `from_content` asks what the box's own contents need whatever width it
    /// declared, which is what a flex item's automatic minimum size is: a box that
    /// says `width: 300px` may still be shrunk, just not past its longest word.
    pub(super) fn min_content_width(
        &mut self,
        id: BoxId,
        containing_width: f32,
        from_content: bool,
    ) -> f32 {
        let key = (
            id,
            containing_width.to_bits(),
            if from_content {
                Wanted::NarrowestOfContent
            } else {
                Wanted::Narrowest
            },
        );
        if let Some(&answer) = self.measured.get(&key) {
            return answer;
        }
        let answer = self.min_content_width_uncached(id, containing_width, from_content);
        self.measured.insert(key, answer);
        answer
    }

    fn min_content_width_uncached(
        &mut self,
        id: BoxId,
        containing_width: f32,
        from_content: bool,
    ) -> f32 {
        self.ensure_collapsed(id);
        let node = self.tree.node(id);
        let style = self.style_of(id);
        let padding = resolve_padding(&style, containing_width);
        let border = resolve_border(&style);
        let extra = padding.left + padding.right + border.left + border.right;

        if !from_content && let Some(width) = style.width.resolve(containing_width) {
            return width + extra;
        }

        let inner = match &node.kind {
            BoxKind::Replaced(content) => replaced_size(&style, content, containing_width).0,
            _ if node.children.is_empty() => 0.0,
            // A row of flex items is one unbreakable run: the items sit beside
            // each other and no shrinking moves one below another, so what the
            // row needs at its narrowest is the sum of what its items need plus
            // the gaps between them. Falling through to the inline branch below
            // takes the *widest* item instead, which for a mark beside a word is
            // the wider of the two rather than the two of them — and the item is
            // then floored at a width its own contents overflow. That is what cut
            // the site's wordmark off beside its logo.
            _ if matches!(
                style.display,
                otlyra_css::Display::Flex | otlyra_css::Display::InlineFlex
            ) && style.flex_direction.is_row()
                && style.flex_wrap == otlyra_css::FlexWrap::NoWrap =>
            {
                let children = node.children.clone();
                let gaps =
                    style.gap.1.resolve(containing_width) * children.len().saturating_sub(1) as f32;
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.min_content_width(child, containing_width, false)
                            + margin.left
                            + margin.right
                    })
                    .sum::<f32>()
                    + gaps
            }
            _ if self.tree.node(node.children[0]).is_inline_level() => {
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
                let pictures = replaced.iter().map(|box_| box_.width).fold(0.0, f32::max);
                text.max(pictures)
            }
            _ => {
                let children = node.children.clone();
                children
                    .into_iter()
                    .map(|child| {
                        let child_style = &self.tree.node(child).style;
                        let margin = resolve_margin(child_style, containing_width);
                        self.min_content_width(child, containing_width, false)
                            + margin.left
                            + margin.right
                    })
                    .fold(0.0, f32::max)
            }
        };

        inner + extra
    }
}
