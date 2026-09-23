//! Inline layout: a paragraph, and the line boxes it breaks into.
//!
//! Everything in an inline formatting context is gathered into one paragraph of
//! styled spans, shaped in one pass, and turned back into line boxes holding the
//! fragments each line carries. What is not text — the padding and border of an
//! inline box, a picture, an `inline-block` — goes in as a spacer the shaper
//! places among the words. How far each thing reaches above and below its line
//! is a question of its own, answered in `vertical_align`.

mod vertical_align;

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Sides};
use otlyra_text::{FontStack, PlacedSpacer, Spacer, TextSpan};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{Fragment, FragmentKind, Layer, Rect};

use super::Flow;
use super::box_model::{any_side, resolve_border, resolve_margin, resolve_padding};
use super::float::band_of;
use super::replaced::{replaced_edges, replaced_fragment, replaced_size};

use vertical_align::{baseline_of, baseline_shift};

/// An inline element that has a box of its own to draw: a background, a border, or
/// padding that moves the text around it.
///
/// It is not a box fragment yet, because where it starts and ends is only known
/// once the paragraph has been broken into lines.
pub(super) struct InlineBox {
    id: BoxId,
    style: Arc<ComputedStyle>,
    border: Sides<f32>,
    padding: Sides<f32>,
    /// The span its content starts at, and the one it ends before.
    first_span: usize,
    last_span: usize,
}

/// A replaced box in an inline formatting context, waiting for the shaper to say
/// where in the line it landed.
pub(super) struct ReplacedBox {
    id: BoxId,
    style: Arc<ComputedStyle>,
    image: Option<otlyra_gfx::peniko::ImageData>,
    /// The span it sits before.
    at: usize,
    pub(super) width: f32,
    height: f32,
    /// An `inline-block`, already laid out, waiting to be told where its line put
    /// it. A picture has nothing here: its content is the picture.
    content: Option<Box<Fragment>>,
    /// How far below its own top the box's baseline sits.
    ///
    /// An `inline-block` sits on the line by the baseline of its *last line*, which
    /// is what makes two buttons of different heights read as one row of words
    /// rather than two boxes hung from a shelf. A picture has no baseline of its
    /// own and sits with its bottom edge on the line's, which is what this is when
    /// it is the whole height.
    baseline: f32,
    /// How far a rule has raised it off that baseline, positive upwards.
    ///
    /// A box in a line answers `vertical-align` the way a span of text does, and a
    /// bar is the reason it has to: both references set a `<progress>` a fifth of
    /// an em below the baseline, and without this it sits on it.
    shift: f32,
}

/// The spacer identifiers for the two edges of the `index`th inline box.
fn leading_spacer(index: usize) -> u64 {
    index as u64 * 2
}

fn trailing_spacer(index: usize) -> u64 {
    index as u64 * 2 + 1
}

/// The spacer identifier for the `index`th replaced box, in a range of its own so
/// that it cannot collide with an inline box's two edges.
fn replaced_spacer(index: usize) -> u64 {
    (1 << 62) | index as u64
}

/// The room the things in a paragraph that are not text take up.
///
/// Each inline box asks for two spacers, one at each edge, carrying the border
/// and padding on that side; they reserve the room the text has to move over by,
/// and where they land is where the box starts and ends — which the shaper is
/// the only thing that knows, since it decided the lines. A replaced box asks
/// for one, the width of the box itself.
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
                    at: inline.first_span,
                    width: inline.border.left + inline.padding.left,
                    height: 0.0,
                },
                Spacer {
                    id: trailing_spacer(index),
                    at: inline.last_span,
                    width: inline.border.right + inline.padding.right,
                    height: 0.0,
                },
            ]
        })
        // The shaper puts a spacer's bottom edge on the baseline, so what is
        // reserved is the part of the box *above* its own baseline; what hangs
        // below is added to the line's descent when the line is levelled.
        .chain(replaced.iter().enumerate().map(|(index, box_)| Spacer {
            id: replaced_spacer(index),
            at: box_.at,
            width: box_.width,
            height: box_.baseline,
        }))
        .collect()
}

impl<'a> Flow<'a> {
    /// An inline formatting context: everything inside becomes one paragraph, and
    /// the paragraph becomes line boxes.
    ///
    /// See [`inline_spacers`] for the room the things that are not text
    /// take in it.
    ///
    /// The whole context is shaped in one pass rather than element by element,
    /// because a line break belongs to the paragraph: `<b>bold</b> text` has to
    /// break where `bold text` breaks.
    pub(super) fn layout_inline(
        &mut self,
        parent: BoxId,
        width: f32,
        x: f32,
        y: f32,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let mut spans = Vec::new();
        let mut sources = Vec::new();
        let mut inlines = Vec::new();
        let mut replaced = Vec::new();
        self.collect_spans(
            parent,
            width,
            &mut spans,
            &mut sources,
            &mut inlines,
            &mut replaced,
        );
        if spans.is_empty() && replaced.is_empty() {
            return 0.0;
        }
        self.level_line_heights(parent, &mut spans, &sources, &replaced);

        // Where each span landed in the concatenated text, computed the same way
        // `shape_spans` concatenates it. This is what turns a shaped run back into
        // the box it came from — and therefore into the element a click lands on.
        let mut starts = Vec::with_capacity(spans.len());
        let mut offset = 0usize;
        for span in &spans {
            starts.push(offset);
            offset += span.text.len();
        }

        let spacers = inline_spacers(&inlines, &replaced);

        // Each line asks how much room the floats have left it at the height it
        // landed at, and where that room starts; the width goes to the shaper and
        // the offset is kept for placing the line.
        // `text-wrap-mode: nowrap` — which is half of what `white-space: nowrap`
        // means — is a line that may not be broken however narrow the box is. No
        // width offered to the shaper is exactly that: it lays the run out on one
        // line and lets it overflow, which is what the property asks for.
        let wraps = self.tree.node(parent).style.text_wrap != otlyra_css::TextWrap::NoWrap;
        let mut bands: Vec<(f32, f32)> = Vec::new();
        let mut shaped = {
            let floats = &self.floats;
            let mut collect_band = |index: usize, top: f32| {
                let (from, to) = band_of(floats, y + top, 1.0, x, x + width);
                let available = (to - from).max(0.0);
                if bands.len() <= index {
                    bands.resize(index + 1, (0.0, width));
                }
                bands[index] = (from - x, available);
                wraps.then_some(available)
            };
            self.text
                .shape_spans_wrapping(&spans, &spacers, &mut collect_band)
        };
        let style = Arc::clone(&self.tree.node(parent).style);

        // The line boxes, put where CSS puts them: as far above each baseline as the
        // paragraph reaches and as far below. The shaper centres the font inside the
        // height it was given instead, which is the same thing for a line of plain
        // text and is not for one holding a box taller than the words beside it.
        // The line boxes, put where CSS puts them: as tall as what is *on* each
        // line, with the baseline as far down as the tallest thing on it reaches.
        //
        // The shaper answers neither question. It carries a line height per run of
        // glyphs and opens a run when the font changes, so a span that wants a
        // taller line without changing font cannot be told to it at all; and where
        // it can, it centres the font inside the height rather than putting the
        // baseline where the tallest thing on the line needs it. So the paragraph
        // is restacked here, from what actually landed on each line.
        let strut = self.line_reach;
        // What the shaper made of each line, kept before anything is moved: it is
        // the answer for a line holding a picture, whose ink reaches past the box
        // the font would have given it.
        let shaper: Vec<(f32, f32)> = shaped
            .lines
            .iter()
            .map(|line| (line.baseline - line.top, line.bottom - line.baseline))
            .collect();
        // A line holding a picture is the shaper's to measure: a picture stands on
        // the baseline with all of its height above, the text beside it hangs
        // below, and the shaper has already put the box around both. Every other
        // line starts at the block's own strut.
        let mut reach: Vec<(f32, f32)> = {
            let mut holds_picture = vec![false; shaped.lines.len()];
            for spacer in &shaped.spacers {
                let picture = replaced.iter().enumerate().any(|(index, box_)| {
                    replaced_spacer(index) == spacer.id && box_.content.is_none()
                });
                if picture && let Some(slot) = holds_picture.get_mut(spacer.line) {
                    *slot = true;
                }
            }
            holds_picture
                .into_iter()
                .enumerate()
                .map(|(index, picture)| {
                    if picture {
                        shaper.get(index).copied().unwrap_or(strut)
                    } else {
                        strut
                    }
                })
                .collect()
        };
        {
            // Which bytes of the shaped text each span covers. The shaper lays the
            // spans end to end, so this is a running total of their lengths.
            let mut starts = Vec::with_capacity(spans.len() + 1);
            let mut at = 0usize;
            for span in &spans {
                starts.push(at);
                at += span.text.len();
            }
            starts.push(at);

            for run in &shaped.runs {
                let Some(line) = reach.get_mut(run.line) else {
                    continue;
                };
                for (index, window) in starts.windows(2).enumerate() {
                    // A span is on this line if any of its bytes were drawn there.
                    if window[1] <= run.text_range.start || window[0] >= run.text_range.end {
                        continue;
                    }
                    let Some(&(above, below)) = self.span_reach.get(index) else {
                        continue;
                    };
                    line.0 = line.0.max(above);
                    line.1 = line.1.max(below);
                }
            }

            // And the boxes in the line, which the shaper placed but did not stack.
            // A picture sits with its bottom edge on the baseline, so all of it is
            // above; an inline block sits on its own last baseline and is held
            // above and below that.
            for spacer in &shaped.spacers {
                let Some(line) = reach.get_mut(spacer.line) else {
                    continue;
                };
                let box_ = replaced
                    .iter()
                    .enumerate()
                    .find(|(index, _)| replaced_spacer(*index) == spacer.id)
                    .map(|(_, box_)| box_);
                match box_ {
                    // The margin box asks for the room, not the border box: a
                    // slider with two pixels above and below it makes the line it
                    // is in four pixels taller, which is what both references do
                    // and the difference between a row of controls sitting in
                    // their line and sitting through it.
                    Some(box_) if box_.content.is_some() => {
                        let margin = resolve_margin(&box_.style, 0.0);
                        line.0 = line.0.max(box_.baseline + box_.shift + margin.top);
                        line.1 = line
                            .1
                            .max(box_.height - box_.baseline - box_.shift + margin.bottom);
                    }
                    Some(box_) => {
                        line.0 = line.0.max(spacer.height + box_.shift);
                        line.1 = line.1.max(-box_.shift);
                    }
                    None => {}
                }
            }
        }

        // Restack: every line ends where the next begins, and the glyphs on it move
        // with the baseline they sit on.
        let mut cursor = shaped.lines.first().map_or(0.0, |line| line.top);
        let mut shifts: Vec<f32> = Vec::with_capacity(shaped.lines.len());
        for (index, line) in shaped.lines.iter_mut().enumerate() {
            let (above, below) = reach.get(index).copied().unwrap_or(strut);
            let baseline = cursor + above;
            shifts.push(baseline - line.baseline);
            line.top = cursor;
            line.baseline = baseline;
            line.height = above + below;
            line.bottom = cursor + line.height;
            cursor = line.bottom;
        }
        for run in &mut shaped.runs {
            let shift = shifts.get(run.line).copied().unwrap_or(0.0);
            for glyph in &mut run.glyphs {
                glyph.y += shift;
            }
        }
        for spacer in &mut shaped.spacers {
            spacer.y += shifts.get(spacer.line).copied().unwrap_or(0.0);
        }
        shaped.metrics.first_baseline = shaped.lines.first().map_or(0.0, |line| line.baseline);
        // The paragraph reaches from the top of its first line to the bottom of
        // its last, which is what its own box is.
        if let (Some(first), Some(last)) = (shaped.lines.first(), shaped.lines.last()) {
            shaped.metrics.height = last.bottom - first.top;
        }

        // parley measures line tops from the text origin, and the first line's top
        // can sit above it by the half-leading. The paragraph's box starts where its
        // first line starts, so everything is rebased onto that.
        let paragraph_top = shaped.lines.first().map_or(0.0, |line| line.top);

        // The marker of the list item this is the first line of, if it is one.
        if let Some(marker) = self
            .pending_marker
            .take_if(|marker| (marker.x - x).abs() < 0.01)
            && let Some(line) = shaped.lines.first()
            && let Some(fragment) = self.marker_fragment(&marker, x, y, line, paragraph_top)
        {
            out.push(fragment);
        }

        let placed: std::collections::HashMap<u64, PlacedSpacer> = shaped
            .spacers
            .iter()
            .map(|spacer| (spacer.id, *spacer))
            .collect();

        for (index, line) in shaped.lines.iter().enumerate() {
            // Line boxes are contiguous: each one ends where the next begins. Taking
            // the height from the next line's top rather than from the font's line
            // height keeps them so, and avoids the fraction of a pixel of overlap
            // that leading otherwise leaves between them.
            let height = shaped
                .lines
                .get(index + 1)
                .map_or(line.bottom - line.top, |next| next.top - line.top);
            let line_y = y + line.top - paragraph_top;
            // Alignment moves the whole line, glyphs and all: the shaper laid it
            // out from the start edge, and where that edge is is the block's
            // decision, not the paragraph's.
            // Alignment is against what the line actually had to fill, which is
            // narrower than the block wherever a float sits beside it.
            let (indent, available) = bands.get(index).copied().unwrap_or((0.0, width));
            let line_x = x
                + indent
                + match style.text_align {
                    otlyra_css::TextAlign::Start => 0.0,
                    otlyra_css::TextAlign::Center => ((available - line.width) / 2.0).max(0.0),
                    otlyra_css::TextAlign::End => (available - line.width).max(0.0),
                };

            // The inline boxes that reach this line, before the text, so their
            // backgrounds and borders sit under the glyphs they belong to. A box
            // that spans two lines gets one fragment per line, each ending where
            // the line does — which is what CSS draws.
            let mut children: Vec<Fragment> = inlines
                .iter()
                .enumerate()
                .filter_map(|(number, inline)| {
                    let start = placed.get(&leading_spacer(number))?;
                    let end = placed.get(&trailing_spacer(number))?;
                    if index < start.line || index > end.line {
                        return None;
                    }
                    let left = if index == start.line { start.x } else { 0.0 };
                    let right = if index == end.line {
                        end.x + end.width
                    } else {
                        line.width
                    };

                    // A box broken over two lines is drawn as CSS says: the border
                    // on the start edge belongs to the piece that starts it and the
                    // one on the end edge to the piece that ends it, so the middle
                    // of a wrapped element is open at both ends rather than boxed
                    // twice.
                    let style = if index == start.line && index == end.line {
                        Arc::clone(&inline.style)
                    } else {
                        let mut broken = (*inline.style).clone();
                        if index != start.line {
                            broken.border.left = otlyra_css::Border::NONE;
                        }
                        if index != end.line {
                            broken.border.right = otlyra_css::Border::NONE;
                        }
                        Arc::new(broken)
                    };

                    // The same shift its text got. An inline box drawn on the
                    // baseline while its own glyphs sat somewhere else was a
                    // background that missed the words it was behind — which is
                    // what a `vertical-align` on a span with a background looks
                    // like when only half of it moves.
                    let shift = self
                        .line_shifts
                        .get(&inline.id)
                        .copied()
                        .unwrap_or_else(|| baseline_shift(&inline.style, &style));

                    Some(Fragment {
                        used: None,
                        box_id: Some(inline.id),
                        // Vertical padding and a horizontal border spill outside
                        // the line box without making it taller: an inline box does
                        // not push its neighbours apart vertically.
                        rect: Rect::new(
                            line_x + left,
                            line_y - shift - inline.border.top - inline.padding.top,
                            (right - left).max(0.0),
                            height
                                + inline.border.top
                                + inline.padding.top
                                + inline.border.bottom
                                + inline.padding.bottom,
                        ),
                        kind: FragmentKind::Box,
                        style,
                        widget: None,
                        fixed: false,
                        scroll_port: None,
                        clip: None,
                        sticky: None,
                        layer: Layer::default(),
                        children: Vec::new(),
                    })
                })
                .collect();

            let runs: Vec<Fragment> = shaped
                .runs
                .iter()
                .filter(|run| run.line == index)
                .map(|run| {
                    // Glyph positions come back relative to the paragraph; a
                    // fragment is a place on the page, so they are rebased onto it.
                    let mut run = run.clone();
                    for glyph in &mut run.glyphs {
                        glyph.x -= run.offset_x;
                        glyph.y -= line.top;
                    }

                    let source = starts
                        .partition_point(|&start| start <= run.text_range.start)
                        .saturating_sub(1);
                    let box_id = sources.get(source).copied();
                    let run_style = box_id.map_or_else(
                        || Arc::clone(&style),
                        |id| Arc::clone(&self.tree.node(id).style),
                    );

                    // `vertical-align`: the glyphs move off the line's baseline,
                    // and the room they need was already added to the line's
                    // height when its spans were levelled.
                    // Resolved once, in the levelling pass, for the five values
                    // that need the line box; worked out here for the rest,
                    // which need only the two fonts.
                    let shift = box_id
                        .and_then(|id| self.line_shifts.get(&id).copied())
                        .unwrap_or_else(|| baseline_shift(&run_style, &style));
                    // The glyphs are placed relative to the fragment, so moving
                    // the fragment moves them with it. Moving both was moving
                    // everything twice as far as it was asked to go.

                    Fragment {
                        used: None,
                        box_id,
                        // The fragment moves with its glyphs. Shifting only the
                        // glyphs left the background, the underline and the
                        // highlight behind on the baseline — invisible on a
                        // `super` that moves three pixels, and unmissable on a
                        // `text-top` span set larger than the line it is in.
                        rect: Rect::new(line_x + run.offset_x, line_y - shift, run.advance, height),
                        widget: None,
                        fixed: false,
                        scroll_port: None,
                        clip: None,
                        sticky: None,
                        layer: Layer::default(),
                        kind: FragmentKind::Text(run),
                        // The run's own style, not the paragraph's: the underline
                        // on a link belongs to the link, and painting from the
                        // block's style would underline the whole paragraph or
                        // none of it.
                        style: run_style,
                        children: Vec::new(),
                    }
                })
                .collect();
            children.extend(runs);

            // The pictures and the inline blocks that landed on this line, where
            // the shaper put them.
            children.extend(replaced.iter().enumerate().filter_map(|(number, box_)| {
                let spacer = placed.get(&replaced_spacer(number))?;
                if spacer.line != index {
                    return None;
                }
                let at = (line_x + spacer.x, y + spacer.y - paragraph_top - box_.shift);
                // An inline block was laid out at the origin, contents and all, and
                // is moved to where its line put it — everything inside it goes
                // with it, which is what makes it one thing in the line. It sits on
                // the line's baseline by its *own* last baseline, which is what
                // makes two buttons of different heights read as a row of words.
                if let Some(content) = box_.content.as_deref() {
                    let mut fragment = content.clone();
                    let top = y + line.baseline - paragraph_top - box_.baseline - box_.shift;
                    let (dx, dy) = (at.0 - fragment.rect.x, top - fragment.rect.y);
                    crate::flow::offset(&mut fragment, dx, dy);
                    return Some(fragment);
                }
                let image = box_.image.clone()?;
                // The spacer reserved the whole box; the picture fills what the
                // frame leaves inside it.
                let (extra_x, extra_y) = replaced_edges(&box_.style, width);
                Some(replaced_fragment(
                    box_.id,
                    &box_.style,
                    Some(image),
                    at,
                    (
                        (spacer.width - extra_x).max(0.0),
                        (spacer.height - extra_y).max(0.0),
                    ),
                    width,
                ))
            }));

            out.push(Fragment {
                used: None,
                box_id: Some(parent),
                rect: Rect::new(line_x, line_y, line.width, height),
                kind: FragmentKind::Line,
                style: Arc::clone(&style),
                widget: None,
                fixed: false,
                scroll_port: None,
                clip: None,
                sticky: None,
                layer: Layer::default(),
                children,
            });
        }

        shaped.metrics.height
    }

    /// The parsed font stack for a style, from the cache.
    pub(super) fn font_stack(&mut self, style: &Arc<ComputedStyle>) -> FontStack {
        let key = Arc::as_ptr(&style.font_family) as *const u8 as usize;
        self.font_stacks
            .entry(key)
            .or_insert_with(|| FontStack::parse_css(&style.font_family))
            .clone()
    }

    /// The strut of one style: how far its font reaches above and below.
    pub(super) fn strut_of(
        &mut self,
        style: &ComputedStyle,
        stack: &FontStack,
    ) -> Option<otlyra_text::Strut> {
        let mut strut = self
            .text
            .strut(stack, style.font_size, style.font_weight, false)?;
        // An explicit `line-height` replaces what the font asked for, split evenly
        // above and below the baseline, which is what half-leading is.
        if let otlyra_css::LineHeight::Normal = style.line_height {
            return Some(strut);
        }
        let asked = style.line_height.resolve(style.font_size, strut.height());
        let half = (asked - strut.ascent - strut.descent) / 2.0;
        strut.ascent += half;
        strut.descent += half;
        strut.leading = 0.0;
        Some(strut)
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
                .strut(&stack, style.font_size, style.font_weight, span.italic)
                .map(otlyra_text::Strut::height);
        }
        span
    }

    /// Walk an inline subtree in order, turning each text box into a styled span.
    ///
    /// `sources` records which box each span came from, in step with `spans`: a
    /// run of glyphs is no use to hit testing without knowing which element it
    /// belongs to.
    pub(super) fn collect_spans(
        &mut self,
        id: BoxId,
        containing_width: f32,
        spans: &mut Vec<TextSpan<'a>>,
        sources: &mut Vec<BoxId>,
        inlines: &mut Vec<InlineBox>,
        replaced: &mut Vec<ReplacedBox>,
    ) {
        for &child in &self.tree.node(id).children {
            let node = self.tree.node(child);
            match &node.kind {
                BoxKind::Replaced(content) => {
                    // A picture in a line is a box the shaper has to make room for,
                    // horizontally and vertically both: the line it sits in is at
                    // least as tall as it is — and as tall as the border and
                    // padding around it, which take room in a line like any other
                    // part of the box.
                    let (width, height) = replaced_size(&node.style, content, containing_width);
                    let (extra_x, extra_y) = replaced_edges(&node.style, containing_width);
                    let (width, height) = (width + extra_x, height + extra_y);
                    replaced.push(ReplacedBox {
                        id: child,
                        style: Arc::clone(&node.style),
                        image: content.image.clone(),
                        at: spans.len(),
                        width,
                        height,
                        content: None,
                        baseline: height,
                        shift: baseline_shift(&node.style, &self.tree.node(id).style),
                    });
                }
                BoxKind::Text(text) => {
                    spans.push(self.styled_span(text, &node.style));
                    // The text's own box is anonymous as far as the document is
                    // concerned; what a click means is the element around it.
                    sources.push(id);
                }
                BoxKind::Inline => {
                    // `<br>` is a forced break, and a newline is exactly how the
                    // shaper is told about one.
                    if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "br") {
                        // Not through `span_for`: whitespace collapsing would turn
                        // the newline into a space, which is exactly the difference
                        // between a `<br>` and a line ending in the source.
                        spans.push(TextSpan {
                            text: "\n",
                            ..self.styled_span("", &node.style)
                        });
                        sources.push(child);
                    }

                    // An inline box only becomes a fragment if it has something to
                    // draw or something to reserve; the rest of them — a `<span>`
                    // that only changes the colour — stay what they are, which is
                    // the style on a run of text.
                    let border = resolve_border(&node.style);
                    let padding = resolve_padding(&node.style, containing_width);
                    let paints = node.style.background_color.components[3] > 0.0
                        || any_side(border)
                        || any_side(padding);
                    let slot = paints.then(|| {
                        inlines.push(InlineBox {
                            id: child,
                            style: Arc::clone(&node.style),
                            border,
                            padding,
                            first_span: spans.len(),
                            last_span: spans.len(),
                        });
                        inlines.len() - 1
                    });

                    self.collect_spans(child, containing_width, spans, sources, inlines, replaced);

                    if let Some(slot) = slot {
                        inlines[slot].last_span = spans.len();
                    }
                }
                BoxKind::Block
                    if matches!(
                        node.style.display,
                        otlyra_css::Display::InlineBlock | otlyra_css::Display::InlineFlex
                    ) =>
                {
                    // Laid out here and now, as the block container it is, at the
                    // width it shrinks to: what the line has to make room for is
                    // its finished size, and nothing about the line changes it.
                    // Where it *goes* is the shaper's answer, so it is laid out at
                    // the origin and moved once the line is broken.
                    let style = Arc::clone(&node.style);
                    let shift_of_box = baseline_shift(&style, &self.tree.node(id).style);
                    let width = match style.width.resolve(containing_width) {
                        Some(width) => {
                            let padding = resolve_padding(&style, containing_width);
                            let border = resolve_border(&style);
                            width + padding.left + padding.right + border.left + border.right
                        }
                        None => self
                            .max_content_width(child, containing_width)
                            .min(containing_width),
                    };
                    let fragment = self.layout_sized(child, 0.0, 0.0, width);
                    let height = fragment.rect.height;
                    // Its own last baseline, or its bottom edge when it has no line
                    // of text in it at all — which is what CSS says an empty one
                    // and one that hides its overflow both sit on.
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
                    let baseline = baseline_of(&fragment)
                        .or_else(|| self.empty_control_baseline(child, &style, containing_width))
                        .unwrap_or(height);
                    replaced.push(ReplacedBox {
                        id: child,
                        style,
                        image: None,
                        at: spans.len(),
                        width: fragment.rect.width,
                        height,
                        content: Some(Box::new(fragment)),
                        baseline,
                        shift: shift_of_box,
                    });
                }
                BoxKind::Block => {
                    // A block inside an inline context. Real CSS splits the inline
                    // around it; we do not yet, so its text joins the paragraph
                    // rather than vanishing.
                    self.collect_spans(child, containing_width, spans, sources, inlines, replaced);
                }
            }
        }
    }
}

/// The span one text box contributes.
///
/// The text is already collapsed — the box tree did it at load time — so this
/// borrows rather than copies.
pub(super) fn span_for<'a>(
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
        italic: style.font_style == otlyra_css::FontStyle::Italic,
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
