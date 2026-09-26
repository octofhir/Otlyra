//! How wide a paragraph is at its widest and at its narrowest: the two
//! intrinsic sizes of an inline formatting context (CSS Sizing 3 §2.1), taken
//! from the same collected paragraph that layout shapes into lines.

use crate::box_tree::BoxId;
use crate::flow::Flow;
use crate::flow::intrinsic::Wanted;

use super::collect::{InlineContent, ReplacedBox, inline_spacers};

impl Flow<'_> {
    /// How wide the inline formatting context `id` establishes is under
    /// `wanted`, as a content-box width.
    ///
    /// At its widest it is one line, however long: the shaper is asked for the
    /// paragraph with nothing to break it. At its narrowest it is broken at
    /// every opportunity, and the widest line that comes back is the widest
    /// word — or the widest atomic inline, which no break can make narrower.
    pub(in crate::flow) fn inline_content_size(
        &mut self,
        id: BoxId,
        containing_width: f32,
        wanted: Wanted,
    ) -> f32 {
        let mut content = self.collect_inline(id, containing_width);
        self.measure_atomic_inlines(&mut content.replaced, containing_width, wanted);
        match wanted {
            Wanted::Widest => self.widest_line(&content),
            Wanted::Narrowest => {
                // Broken as hard as it will break — unless it may not break at
                // all. Under `text-wrap-mode: nowrap` the whole run is one
                // unbreakable thing, so its min-content size is its full width;
                // asking for the longest word instead would let a flex item
                // shrink to that word while the text it draws stays full length,
                // and the item beside it would be laid over the overflow. That is
                // what folded and then overlapped the site's own header.
                let wraps = self.style_of(id).text_wrap != otlyra_css::TextWrap::NoWrap;
                self.narrowest_line(&content, wraps)
            }
        }
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
            box_.width = self.contribution(box_.id, containing_width, None, wanted);
        }
    }

    /// The paragraph on one line.
    ///
    /// Shaped with the spacers rather than measured without them and added on
    /// afterwards: the width of a run of text is not the sum of its pieces once
    /// something that is not text sits in it. A space between two pictures is
    /// trailing white space at the end of the *text* and no space at all at the
    /// end of the run, and a paragraph measured the other way came back narrower
    /// than the one line it holds — which put the second picture on a line of
    /// its own.
    fn widest_line(&mut self, content: &InlineContent<'_>) -> f32 {
        let spacers = inline_spacers(&content.inlines, &content.replaced);
        if content.spans.is_empty() && spacers.is_empty() {
            return 0.0;
        }
        self.text
            .shape_spans(&content.spans, &spacers, None)
            .metrics
            .width
    }

    /// The paragraph broken at every opportunity it has — at none, when it may
    /// not `wrap` — measured by its widest line without the white space a
    /// break leaves at the end.
    ///
    /// Shaped with the spacers, as the widest line is: the margins, borders
    /// and padding of an inline box are part of the piece of the line they sit
    /// on, which no break separates from its text (CSS Sizing 3 §5.1), and an
    /// atomic inline is a piece of its own, margins and all.
    fn narrowest_line(&mut self, content: &InlineContent<'_>, wraps: bool) -> f32 {
        let spacers = inline_spacers(&content.inlines, &content.replaced);
        if content.spans.is_empty() && spacers.is_empty() {
            return 0.0;
        }
        self.text
            .shape_spans(&content.spans, &spacers, wraps.then_some(0.0))
            .lines
            .iter()
            .map(|line| line.width - line.trailing_space)
            .fold(0.0, f32::max)
    }
}
