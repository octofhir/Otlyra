//! Gathering an inline formatting context into one paragraph: its text as the
//! styled spans the shaper takes, the inline boxes that paint around the text,
//! and the atomic inlines the shaper has to make room for.

use std::ops::Range;
use std::sync::Arc;

use otlyra_css::{ComputedStyle, Sides};
use otlyra_text::{FontStack, Spacer, SpacerKind, TextSpan};

use crate::box_tree::{BoxId, BoxKind};
use crate::flow::Flow;
use crate::flow::box_model::{any_side, resolve_border, resolve_margin, resolve_padding};
use crate::flow::replaced::replaced_size;
use crate::flow::sizing::{Frame, InlineRoom};
use crate::fonts::{face_query, is_italic};
use crate::fragment::Fragment;

use super::vertical_align::{baseline_of, baseline_shift};

/// An inline element that has a box of its own to draw — a background, a border,
/// padding — or edges that take room in its line.
///
/// It is not a box fragment yet, because where it starts and ends is only known
/// once the paragraph has been broken into lines.
pub(super) struct InlineBox {
    pub(super) id: BoxId,
    pub(super) style: Arc<ComputedStyle>,
    pub(super) margin: Sides<f32>,
    pub(super) border: Sides<f32>,
    pub(super) padding: Sides<f32>,
    /// Its content area, which its background and border are drawn around —
    /// or nothing, for a box that draws nothing and only takes room.
    pub(super) content_area: Option<ContentArea>,
    /// The span its content starts at, and the one it ends before.
    pub(super) first_span: usize,
    pub(super) last_span: usize,
}

impl InlineBox {
    /// The room its start edge takes in the line: the margin, border and padding
    /// on that side (CSS 2.2 §8.3, §10.3.1). Signed, so a negative margin pulls
    /// what follows back over it.
    fn leading_room(&self) -> f32 {
        self.margin.left + self.border.left + self.padding.left
    }

    /// The room its end edge takes, the same three in the other order.
    fn trailing_room(&self) -> f32 {
        self.padding.right + self.border.right + self.margin.right
    }

    /// Whether anything along its inline axis takes room: a margin, a border or
    /// padding at either end.
    ///
    /// What keeps a line holding nothing else from being a phantom line box
    /// (CSS Inline 3; CSS 2.2 §9.4.2). Only the inline-axis edges count: padding
    /// on top of an empty span makes no line, and padding on its left does.
    fn takes_inline_room(&self) -> bool {
        [
            self.margin.left,
            self.border.left,
            self.padding.left,
            self.padding.right,
            self.border.right,
            self.margin.right,
        ]
        .iter()
        .any(|&edge| edge != 0.0)
    }
}

/// How far an inline box's content area reaches above and below its baseline:
/// the ascent and descent of its first available font, with none of the leading
/// `line-height` adds (CSS 2.2 §10.6.1).
///
/// Its background and border are drawn around this rather than around the line
/// box, which is why `line-height: 2` spaces a highlighted word out without
/// making its highlight twice as tall. Rounded one end at a time, as the strut
/// is, so a box and the text it is behind agree to the pixel.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(super) struct ContentArea {
    pub(super) ascent: f32,
    pub(super) descent: f32,
}

/// A replaced box in an inline formatting context, waiting for the shaper to say
/// where in the line it landed.
pub(super) struct ReplacedBox {
    pub(super) id: BoxId,
    pub(super) style: Arc<ComputedStyle>,
    pub(super) image: Option<otlyra_gfx::peniko::ImageData>,
    /// The span it sits before.
    pub(super) at: usize,
    /// Its border box.
    pub(super) width: f32,
    pub(super) height: f32,
    /// Its margins, which it is placed by: a line holds an atomic inline's
    /// margin box (CSS 2.2 §10.3.9, §10.8).
    pub(super) margin: Sides<f32>,
    /// An `inline-block`, already laid out, waiting to be told where its line put
    /// it. A picture has nothing here: its content is the picture.
    pub(super) content: Option<Box<Fragment>>,
    /// How far below the top of its margin box the box's baseline sits.
    ///
    /// An `inline-block` sits on the line by the baseline of its *last line*, which
    /// is what makes two buttons of different heights read as one row of words
    /// rather than two boxes hung from a shelf. A picture has no baseline of its
    /// own and sits with its bottom margin edge on the line's, which is what this
    /// is when it is the whole margin box (CSS 2.2 §10.8.1).
    pub(super) baseline: f32,
    /// How far a rule has raised it off that baseline, positive upwards.
    ///
    /// A box in a line answers `vertical-align` the way a span of text does, and a
    /// bar is the reason it has to: both references set a `<progress>` a fifth of
    /// an em below the baseline, and without this it sits on it.
    pub(super) shift: f32,
}

impl ReplacedBox {
    /// The width of its margin box: what it takes along the line.
    pub(super) fn outer_width(&self) -> f32 {
        self.margin.left + self.width + self.margin.right
    }

    /// The height of its margin box: what the line has to hold.
    pub(super) fn outer_height(&self) -> f32 {
        self.margin.top + self.height + self.margin.bottom
    }
}

/// The spacer identifiers for the two edges of the `index`th inline box.
pub(super) fn leading_spacer(index: usize) -> u64 {
    index as u64 * 2
}

pub(super) fn trailing_spacer(index: usize) -> u64 {
    index as u64 * 2 + 1
}

/// The bit that puts a replaced box's spacer in a range of its own.
const REPLACED_SPACERS: u64 = 1 << 62;

/// The spacer identifier for the `index`th replaced box, in a range of its own so
/// that it cannot collide with an inline box's two edges.
pub(super) fn replaced_spacer(index: usize) -> u64 {
    REPLACED_SPACERS | index as u64
}

/// The room the things in a paragraph that are not text take up.
///
/// Each inline box asks for two spacers, one at each edge, carrying the margin,
/// border and padding on that side; they reserve the room the text has to move
/// over by, and where they land is where the box starts and ends — which the
/// shaper is the only thing that knows, since it decided the lines. A replaced
/// box asks for one, the width of its margin box. An edge is no place to break
/// a line, and a picture is one (see [`SpacerKind`]).
///
/// The same list is used to measure a paragraph and to lay it out, because a
/// measurement taken without them is a measurement of a different paragraph.
pub(super) fn inline_spacers(inlines: &[InlineBox], replaced: &[ReplacedBox]) -> Vec<Spacer> {
    inlines
        .iter()
        .enumerate()
        .flat_map(|(index, inline)| {
            [
                Spacer {
                    id: leading_spacer(index),
                    kind: SpacerKind::Opening,
                    at: inline.first_span,
                    width: inline.leading_room(),
                    height: 0.0,
                },
                Spacer {
                    id: trailing_spacer(index),
                    kind: SpacerKind::Closing,
                    at: inline.last_span,
                    width: inline.trailing_room(),
                    height: 0.0,
                },
            ]
        })
        // The shaper puts a spacer's bottom edge on the baseline, so what is
        // reserved is the part of the box *above* its own baseline; what hangs
        // below is added to the line's descent when the line is levelled.
        .chain(replaced.iter().enumerate().map(|(index, box_)| Spacer {
            id: replaced_spacer(index),
            kind: SpacerKind::Atomic,
            at: box_.at,
            width: box_.outer_width(),
            height: box_.baseline,
        }))
        .collect()
}

/// Everything in one inline formatting context, in document order: its text as
/// styled spans, and the boxes in it that are not text.
///
/// Gathered once, by [`Flow::collect_inline`], and read by every step after it:
/// levelling, shaping, stacking the lines and placing what is on them — and by
/// the measurements that ask how wide the same paragraph would be.
pub(super) struct InlineContent<'a> {
    /// The text, one span per text box and one per `<br>`.
    pub(super) spans: Vec<TextSpan<'a>>,
    /// The box each span came from, in step with `spans`: a run of glyphs is no
    /// use to hit testing without knowing which element it belongs to.
    pub(super) sources: Vec<BoxId>,
    /// Where each span starts in the text the shaper is given, which lays the
    /// spans end to end, and where the last one ends: one more entry than there
    /// are spans. Kept in step by [`InlineContent::push_text`], the one way a
    /// span is added.
    starts: Vec<usize>,
    /// The inline boxes that draw something or take room.
    pub(super) inlines: Vec<InlineBox>,
    /// The pictures, inline blocks and widgets.
    pub(super) replaced: Vec<ReplacedBox>,
}

impl<'a> InlineContent<'a> {
    fn new() -> Self {
        Self {
            spans: Vec::new(),
            sources: Vec::new(),
            starts: vec![0],
            inlines: Vec::new(),
            replaced: Vec::new(),
        }
    }

    /// Add a span of text, from the box `source`, at the end of the paragraph.
    fn push_text(&mut self, span: TextSpan<'a>, source: BoxId) {
        let end = self.starts.last().copied().unwrap_or(0) + span.text.len();
        self.spans.push(span);
        self.sources.push(source);
        self.starts.push(end);
    }

    /// Whether the paragraph would make nothing but a phantom line box: no
    /// text, no atomic inline, and no inline box whose inline-axis margin,
    /// border or padding takes room. Such a line is not there for any purpose
    /// (CSS Inline 3; CSS 2.2 §9.4.2), so there is nothing to lay out.
    pub(super) fn is_phantom(&self) -> bool {
        self.spans.is_empty()
            && self.replaced.is_empty()
            && !self.inlines.iter().any(InlineBox::takes_inline_room)
    }

    /// Where span `index` starts in the shaped text, or where the text ends for
    /// the index one past the last span.
    pub(super) fn start_of(&self, index: usize) -> usize {
        self.starts
            .get(index)
            .or(self.starts.last())
            .copied()
            .unwrap_or(0)
    }

    /// The box a byte of the shaped text came from: the one whose span is the
    /// last to start at or before it. This is what turns a shaped run back into
    /// the box it came from — and therefore into the element a click lands on.
    pub(super) fn source_at(&self, byte: usize) -> Option<BoxId> {
        let span_starts = &self.starts[..self.spans.len()];
        let index = span_starts
            .partition_point(|&start| start <= byte)
            .checked_sub(1)?;
        self.sources.get(index).copied()
    }

    /// The spans with at least one byte in `range` of the shaped text.
    pub(super) fn spans_in(&self, range: Range<usize>) -> impl Iterator<Item = usize> + '_ {
        self.starts
            .windows(2)
            .enumerate()
            .filter(move |(_, span)| span[1] > range.start && span[0] < range.end)
            .map(|(index, _)| index)
    }

    /// The replaced box a spacer was reserved for, if it was reserved for one.
    pub(super) fn replaced_by_spacer(&self, id: u64) -> Option<&ReplacedBox> {
        if id & REPLACED_SPACERS == 0 {
            return None;
        }
        let index = usize::try_from(id & !REPLACED_SPACERS).ok()?;
        self.replaced.get(index)
    }
}

impl<'a> Flow<'a> {
    /// Gather the inline formatting context `parent` establishes, with its
    /// percentages resolved against `containing_width`.
    pub(super) fn collect_inline(
        &mut self,
        parent: BoxId,
        containing_width: f32,
    ) -> InlineContent<'a> {
        let mut content = InlineContent::new();
        self.collect_spans(parent, containing_width, &mut content);
        content
    }

    /// The span one text box contributes, with `line-height: normal` already
    /// resolved against the font it will be set in.
    ///
    /// Resolved here rather than left to the shaper because `normal` is a browser
    /// decision about a *font*, not about a paragraph: it is the strut, and the
    /// strut comes from the block's own font whatever the runs inside it turn out
    /// to be. Answering it needs the font, so it needs the engine, so it cannot
    /// live in the plain function below.
    fn styled_span<'t>(&mut self, text: &'t str, style: &'t Arc<ComputedStyle>) -> TextSpan<'t> {
        let stack = self.font_stack(style);
        let mut span = span_for(text, style, stack.clone());
        if span.line_height.is_none() {
            span.line_height = self
                .text
                .strut(&stack, style.font_size, span.face_query())
                .map(otlyra_text::Strut::height);
        }
        span
    }

    /// The content area of an inline box styled `style`.
    ///
    /// A stack that resolves to no font at all has none, and the box is then only
    /// as tall as its border and padding: it has no text to draw either.
    fn content_area(&mut self, style: &ComputedStyle) -> ContentArea {
        let stack = self.font_stack(style);
        self.text
            .strut(&stack, style.font_size, face_query(style))
            .map_or_else(ContentArea::default, |strut| ContentArea {
                ascent: strut.ascent,
                descent: strut.descent,
            })
    }

    /// Walk an inline subtree in order, turning each text box into a styled span.
    fn collect_spans(&mut self, id: BoxId, containing_width: f32, content: &mut InlineContent<'a>) {
        for &child in &self.tree.node(id).children {
            let node = self.tree.node(child);
            match &node.kind {
                BoxKind::Replaced(replaced) => {
                    // A picture in a line is a box the shaper has to make room for,
                    // horizontally and vertically both: the line it sits in is at
                    // least as tall as it is — and as tall as the border and
                    // padding around it, which take room in a line like any other
                    // part of the box.
                    let room = InlineRoom::within(&node.style, containing_width);
                    let (width, height) =
                        replaced_size(&node.style, replaced, room, self.containing_height);
                    let frame = Frame::of(&node.style, containing_width);
                    let (width, height) = (width + frame.inline, height + frame.block);
                    let margin = resolve_margin(&node.style, containing_width);
                    content.replaced.push(ReplacedBox {
                        id: child,
                        style: Arc::clone(&node.style),
                        image: replaced.image.clone(),
                        at: content.spans.len(),
                        width,
                        height,
                        margin,
                        content: None,
                        baseline: margin.top + height + margin.bottom,
                        shift: baseline_shift(&node.style, &self.tree.node(id).style),
                    });
                }
                BoxKind::Text(text) => {
                    let span = self.styled_span(text, &node.style);
                    // The text's own box is anonymous as far as the document is
                    // concerned; what a click means is the element around it.
                    content.push_text(span, id);
                }
                BoxKind::Inline => {
                    // `<br>` is a forced break, and a newline is exactly how the
                    // shaper is told about one.
                    if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "br") {
                        // Not through `span_for`: whitespace collapsing would turn
                        // the newline into a space, which is exactly the difference
                        // between a `<br>` and a line ending in the source.
                        let span = TextSpan {
                            text: "\n",
                            ..self.styled_span("", &node.style)
                        };
                        content.push_text(span, child);
                    }

                    // An inline box is only kept if it has something to draw or
                    // edges that take room, and only becomes a fragment if it
                    // draws; the rest of them — a `<span>` that only changes the
                    // colour — stay what they are, which is the style on a run of
                    // text.
                    let margin = resolve_margin(&node.style, containing_width);
                    let border = resolve_border(&node.style);
                    let padding = resolve_padding(&node.style, containing_width);
                    let paints = node.style.background_color.components[3] > 0.0
                        || any_side(border)
                        || any_side(padding);
                    let content_area = paints.then(|| self.content_area(&node.style));
                    let inline = InlineBox {
                        id: child,
                        style: Arc::clone(&node.style),
                        margin,
                        border,
                        padding,
                        content_area,
                        first_span: content.spans.len(),
                        last_span: content.spans.len(),
                    };
                    let slot = (paints || inline.takes_inline_room()).then(|| {
                        content.inlines.push(inline);
                        content.inlines.len() - 1
                    });

                    self.collect_spans(child, containing_width, content);

                    if let Some(slot) = slot {
                        content.inlines[slot].last_span = content.spans.len();
                    }
                }
                BoxKind::Block
                    if matches!(
                        node.style.display,
                        otlyra_css::Display::InlineBlock
                            | otlyra_css::Display::InlineFlex
                            | otlyra_css::Display::InlineGrid
                    ) =>
                {
                    // Laid out here and now, as the block container it is, at the
                    // width it shrinks to: what the line has to make room for is
                    // its finished size, and nothing about the line changes it.
                    // Where it *goes* is the shaper's answer, so it is laid out at
                    // the origin and moved once the line is broken.
                    let style = Arc::clone(&node.style);
                    let shift_of_box = baseline_shift(&style, &self.tree.node(id).style);
                    // Shrink-to-fit, like a float: its own width when it names one,
                    // what its content wants of the line otherwise, and its minimum
                    // and maximum over both — as the border box the line holds.
                    let frame = Frame::of(&style, containing_width).inline;
                    let room = InlineRoom::within(&style, containing_width);
                    let width = self.shrink_to_fit_width(child, &style, room, frame);
                    let fragment = self.layout_sized(child, 0.0, 0.0, width);
                    let height = fragment.rect.height;
                    // Its own last baseline, or its bottom margin edge when it has
                    // no line of text in it at all or is a scroll container —
                    // which is what CSS says an empty one and one that hides its
                    // overflow both sit on (CSS 2.2 §10.8.1). One that only clips
                    // is not a scroll container, and keeps its text's baseline.
                    // Measured from the top of its margin box, which is what the
                    // line holds.
                    //
                    // A control is the exception, and it is not a small one. An
                    // empty field has no text, so it has no line to take a baseline
                    // from, and the rule above hangs it from its own bottom edge:
                    // the line then reserves the whole field above the baseline and
                    // the text's descender below it, and comes out about four
                    // pixels taller than it should be. Every line with an empty
                    // field on it, which is most of the lines on most forms. Both
                    // references put the baseline where the field's own text would
                    // sit whether or not any is there, because the box a field
                    // types into exists empty, and that is what this does.
                    let margin = resolve_margin(&style, containing_width);
                    let own = (!style.overflow.is_scroll_container())
                        .then(|| baseline_of(&fragment))
                        .flatten();
                    let baseline = own
                        .or_else(|| {
                            self.empty_control_baseline(child, &style, containing_width, height)
                        })
                        .map_or(margin.top + height + margin.bottom, |own| margin.top + own);
                    content.replaced.push(ReplacedBox {
                        id: child,
                        style,
                        image: None,
                        at: content.spans.len(),
                        width: fragment.rect.width,
                        height,
                        margin,
                        content: Some(Box::new(fragment)),
                        baseline,
                        shift: shift_of_box,
                    });
                }
                BoxKind::Block => {
                    // A block inside an inline context. Real CSS splits the inline
                    // around it; we do not yet, so its text joins the paragraph
                    // rather than vanishing.
                    self.collect_spans(child, containing_width, content);
                }
            }
        }
    }
}

/// The span one text box contributes.
///
/// The text is already collapsed — the box tree did it at load time — so this
/// borrows rather than copies.
pub(in crate::flow) fn span_for<'a>(
    text: &'a str,
    style: &'a ComputedStyle,
    font_stack: FontStack,
) -> TextSpan<'a> {
    let color = style.color.to_rgba8();
    TextSpan {
        text,
        font_stack,
        font_size: style.font_size,
        font_weight: style.font_weight,
        font_width: style.font_width,
        italic: is_italic(style),
        underline: style.text_decoration.underline,
        strikethrough: style.text_decoration.line_through,
        brush: [color.r, color.g, color.b, color.a],
        line_height: match style.line_height {
            otlyra_css::LineHeight::Normal => None,
            other => Some(other.resolve(style.font_size, style.font_size * 1.2)),
        },
        letter_spacing: style.letter_spacing,
        word_spacing: style.word_spacing,
        optical_sizing: style.optical_sizing,
        variations: &style.font_variations,
    }
}
