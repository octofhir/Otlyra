//! Placing a paragraph's lines on the page: where alignment puts each line, and
//! the fragments the line carries — the inline boxes behind its text, the runs
//! of glyphs, and the atomic inlines the shaper made room for.

use std::collections::HashMap;
use std::sync::Arc;

use otlyra_css::ComputedStyle;
use otlyra_text::{LineMetrics, PlacedSpacer, ShapedText};

use crate::box_tree::{BoxId, BoxTree};
use crate::flow::replaced::replaced_fragment;
use crate::flow::sizing::Frame;
use crate::fragment::{Fragment, FragmentKind, Rect};

use super::collect::{InlineContent, leading_spacer, replaced_spacer, trailing_spacer};
use super::lines::Band;
use super::vertical_align::shift_on_line;

/// How far `text-align` moves a line along the room it had.
///
/// Alignment moves the whole line, glyphs and all: the shaper laid it out from
/// the start edge, and where that edge is is the block's decision, not the
/// paragraph's. It is against what the line actually had to fill, which is
/// narrower than the block wherever a float sits beside it and than that by
/// its indent, and against the line without the white space that hangs at its
/// end (CSS Text 3 §4.1.3), which is no part of what is centred.
///
/// A line that wrapped is aligned as `text-align` says; one a forced break or
/// the paragraph's end ended, as `text-align-last` does (§7.3). A justified
/// line was spread by the shaper and starts at the start. `text-align-last:
/// justify` is not done: such a line is aligned to its start.
pub(super) fn line_offset(style: &ComputedStyle, line: &LineMetrics, band: Band) -> f32 {
    use otlyra_css::{TextAlign, TextAlignLast};

    #[derive(Copy, Clone)]
    enum Edge {
        Start,
        Center,
        End,
    }
    let of_align = |align: TextAlign| match align {
        TextAlign::Start | TextAlign::Justify => Edge::Start,
        TextAlign::Center => Edge::Center,
        TextAlign::End => Edge::End,
    };
    let edge = match line.end {
        otlyra_text::LineEnd::Wrapped => of_align(style.text_align),
        otlyra_text::LineEnd::Forced | otlyra_text::LineEnd::Last => match style.text_align_last {
            TextAlignLast::Auto => of_align(style.text_align),
            TextAlignLast::Start | TextAlignLast::Justify => Edge::Start,
            TextAlignLast::Center => Edge::Center,
            TextAlignLast::End => Edge::End,
        },
    };
    let free = band.width - line.indent - (line.width - line.trailing_space);
    match edge {
        Edge::Start => 0.0,
        Edge::Center => (free / 2.0).max(0.0),
        Edge::End => free.max(0.0),
    }
}

/// One line box, where it sits on the page.
pub(super) struct LineBox<'s> {
    /// Which line of the paragraph it is.
    pub(super) index: usize,
    /// The line as the shaper broke it and the paragraph restacked it.
    pub(super) metrics: &'s LineMetrics,
    /// Its left edge, once the floats and `text-align` have had their say.
    pub(super) x: f32,
    /// Its top edge.
    pub(super) y: f32,
    /// How tall it is, down to where the next line starts.
    pub(super) height: f32,
}

impl LineBox<'_> {
    /// Where its baseline is on the page.
    fn baseline(&self) -> f32 {
        self.y + (self.metrics.baseline - self.metrics.top)
    }
}

/// A paragraph, shaped and stacked, and what placing any one of its lines
/// needs to know about it.
pub(super) struct Placement<'p, 'a> {
    pub(super) tree: &'p BoxTree,
    /// The block container the paragraph is in.
    pub(super) parent: BoxId,
    pub(super) content: &'p InlineContent<'a>,
    pub(super) shaped: &'p ShapedText,
    /// How far each box's content sits off the paragraph's baseline.
    pub(super) shifts: &'p HashMap<BoxId, f32>,
    /// How far each run of a `top` or `bottom` span sits off its own line's
    /// baseline, in step with the shaped runs.
    pub(super) run_shift: &'p [Option<f32>],
    /// How far each atomic inline sits off its line's baseline.
    pub(super) replaced_shift: &'p [f32],
    /// Where the shaper put each spacer, by its identifier.
    pub(super) spacers: HashMap<u64, PlacedSpacer>,
    /// The width of the block's content box.
    pub(super) width: f32,
}

/// Where a fragment on a line comes in the order the line is painted: where
/// its content starts in the text, and before the text there when it is the box
/// around it or a box sitting in front of it.
///
/// Inline content is painted in tree order, each inline box's background and
/// border before what is inside it (CSS 2.2 Appendix E, step 7.2.1): a box a
/// negative margin pulls back over the words before it covers them, and the
/// words after it cover it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PaintOrder {
    byte: usize,
    kind: PaintKind,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PaintKind {
    InlineBox,
    Atomic,
    Text,
}

impl Placement<'_, '_> {
    /// The line box's fragment, holding everything on the line in the order it
    /// is painted.
    pub(super) fn line_fragment(&self, line: &LineBox<'_>) -> Fragment {
        let mut ordered = self.inline_box_fragments(line);
        ordered.extend(self.run_fragments(line));
        ordered.extend(self.atomic_fragments(line));
        // Stable, so boxes that start together keep the order they nest in.
        ordered.sort_by_key(|(order, _)| *order);
        Fragment {
            children: ordered.into_iter().map(|(_, fragment)| fragment).collect(),
            ..Fragment::new(
                Some(self.parent),
                Rect::new(line.x, line.y, line.metrics.width, line.height),
                FragmentKind::Line {
                    baseline: line.baseline() - line.y,
                },
                Arc::clone(self.style()),
            )
        }
    }

    /// The block container's own style.
    fn style(&self) -> &Arc<ComputedStyle> {
        &self.tree.node(self.parent).style
    }

    /// How far `source`'s content is raised off this paragraph's baselines.
    fn shift_on_line(&self, source: BoxId) -> f32 {
        shift_on_line(self.shifts, self.tree, source, self.parent)
    }

    /// The inline boxes that reach this line and draw something, before the
    /// text, so their backgrounds and borders sit under the glyphs they belong
    /// to. A box that spans two lines gets one fragment per line, each ending
    /// where the line does — which is what CSS draws.
    fn inline_box_fragments(&self, line: &LineBox<'_>) -> Vec<(PaintOrder, Fragment)> {
        let index = line.index;
        self.content
            .inlines
            .iter()
            .enumerate()
            .filter_map(|(number, inline)| {
                let area = inline.content_area?;
                let start = self.spacers.get(&leading_spacer(number))?;
                let end = self.spacers.get(&trailing_spacer(number))?;
                if index < start.line || index > end.line {
                    return None;
                }
                // The spacers hold the margins as well, and the border box
                // starts inside the one and ends inside the other.
                let left = if index == start.line {
                    start.x + inline.margin.left
                } else {
                    0.0
                };
                // A piece the line breaks in the middle of ends where the
                // line's content does, before the white space that hangs off
                // it (CSS Text 3 §4.1.3).
                let right = if index == end.line {
                    end.x + end.width - inline.margin.right
                } else {
                    line.metrics.width - line.metrics.trailing_space
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
                let shift = self.shift_on_line(inline.id);

                // Its content area, around its own baseline, with the padding
                // and border outside that (CSS 2.2 §10.6.1). Vertical padding
                // and borders spill outside the line box without making it
                // taller: an inline box does not push its neighbours apart
                // vertically.
                let baseline = line.baseline() - shift;
                let order = PaintOrder {
                    byte: self.content.start_of(inline.first_span),
                    kind: PaintKind::InlineBox,
                };
                Some((
                    order,
                    Fragment::for_box(
                        inline.id,
                        Rect::new(
                            line.x + left,
                            baseline - area.ascent - inline.border.top - inline.padding.top,
                            (right - left).max(0.0),
                            area.ascent
                                + area.descent
                                + inline.border.top
                                + inline.padding.top
                                + inline.border.bottom
                                + inline.padding.bottom,
                        ),
                        style,
                        Vec::new(),
                    ),
                ))
            })
            .collect()
    }

    /// The runs of glyphs on this line, each a fragment of the box its text
    /// came from.
    fn run_fragments(&self, line: &LineBox<'_>) -> Vec<(PaintOrder, Fragment)> {
        self.shaped
            .runs
            .iter()
            .enumerate()
            .filter(|(_, run)| run.line == line.index)
            .map(|(number, run)| {
                // Glyph positions come back relative to the paragraph; a
                // fragment is a place on the page, so they are rebased onto it.
                let mut run = run.clone();
                for glyph in &mut run.glyphs {
                    glyph.x -= run.offset_x;
                    glyph.y -= line.metrics.top;
                }

                let box_id = self.content.source_at(run.text_range.start);
                let run_style = box_id.map_or_else(
                    || Arc::clone(self.style()),
                    |id| Arc::clone(&self.tree.node(id).style),
                );

                // `vertical-align`: the glyphs move off the line's baseline,
                // and the room they need was already added to the line's
                // height when its spans were levelled. A run with no box of
                // its own is the block's text, which stays on the baseline.
                let shift = self
                    .run_shift
                    .get(number)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| box_id.map_or(0.0, |id| self.shift_on_line(id)));

                // The fragment moves with its glyphs, which are placed relative
                // to it. Shifting only the glyphs left the background, the
                // underline and the highlight behind on the baseline —
                // invisible on a `super` that moves three pixels, and
                // unmissable on a `text-top` span set larger than the line it
                // is in.
                let rect = Rect::new(
                    line.x + run.offset_x,
                    line.y - shift,
                    run.advance,
                    line.height,
                );
                // The run's own style, not the paragraph's: the underline
                // on a link belongs to the link, and painting from the
                // block's style would underline the whole paragraph or
                // none of it.
                let order = PaintOrder {
                    byte: run.text_range.start,
                    kind: PaintKind::Text,
                };
                (
                    order,
                    Fragment::new(box_id, rect, FragmentKind::Text(run), run_style),
                )
            })
            .collect()
    }

    /// The pictures and the inline blocks that landed on this line, where the
    /// shaper put them.
    fn atomic_fragments(&self, line: &LineBox<'_>) -> Vec<(PaintOrder, Fragment)> {
        self.content
            .replaced
            .iter()
            .enumerate()
            .filter_map(|(number, box_)| {
                let spacer = self.spacers.get(&replaced_spacer(number))?;
                if spacer.line != line.index {
                    return None;
                }
                // The spacer reserved the margin box, and the box goes inside
                // its margins: its baseline is measured from the margin box's
                // top, which is where the line put that.
                let x = line.x + spacer.x + box_.margin.left;
                let shift = self.replaced_shift.get(number).copied().unwrap_or(0.0);
                let y = line.baseline() - shift - box_.baseline + box_.margin.top;
                let order = PaintOrder {
                    byte: self.content.start_of(box_.at),
                    kind: PaintKind::Atomic,
                };
                // An inline block was laid out at the origin, contents and all,
                // and is moved to where its line put it — everything inside it
                // goes with it, which is what makes it one thing in the line. It
                // sits on the line's baseline by its *own* last baseline, which
                // is what makes two buttons of different heights read as a row
                // of words.
                if let Some(content) = box_.content.as_deref() {
                    let mut fragment = content.clone();
                    let (dx, dy) = (x - fragment.rect.x, y - fragment.rect.y);
                    crate::flow::offset(&mut fragment, dx, dy);
                    return Some((order, fragment));
                }
                // The picture fills what the frame leaves inside its border box.
                // A box with no picture in it — a frame, a video before its
                // poster — is still a box, with a background and a border of
                // its own.
                let frame = Frame::of(&box_.style, self.width);
                let fragment = replaced_fragment(
                    box_.id,
                    &box_.style,
                    box_.image.clone(),
                    (x, y),
                    (
                        (box_.width - frame.inline).max(0.0),
                        (box_.height - frame.block).max(0.0),
                    ),
                    self.width,
                );
                Some((order, fragment))
            })
            .collect()
    }
}
