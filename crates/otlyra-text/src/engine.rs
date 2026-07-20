//! Shaping and measurement over parley.

use otlyra_gfx::Glyph;
use parley::{
    Alignment, AlignmentOptions, FontContext, FontData, LayoutContext, PositionedLayoutItem,
    StyleProperty,
};

use crate::{FontStack, TEST_FAMILY, TEST_FONT};

/// The colour carried through shaping, as straight RGBA bytes.
///
/// parley calls this a brush and hands it back with each glyph run, which is what
/// lets one paragraph contain differently coloured spans without shaping each of
/// them separately and losing the line breaks between them.
pub type Brush = [u8; 4];

/// One run of glyphs: one font, one size, one colour, already positioned.
///
/// The positions are absolute within the layout, in logical pixels, with the
/// origin at the layout's top left. `PaintTarget::draw_glyphs` wants exactly this.
#[derive(Clone, Debug)]
pub struct ShapedRun {
    /// The font the run is shaped in.
    pub font: FontData,
    /// Size in logical pixels.
    pub font_size: f32,
    /// Variation axis coordinates, F2Dot14. Empty for a static font.
    pub normalized_coords: Vec<i16>,
    /// The colour this run was requested in.
    pub brush: Brush,
    /// An underline, if the run asked for one.
    pub underline: Option<Decoration>,
    /// A strikethrough, if the run asked for one.
    pub strikethrough: Option<Decoration>,
    /// Which line of the paragraph the run belongs to.
    pub line: usize,
    /// Where the run starts along its line, in logical pixels.
    pub offset_x: f32,
    /// How far the run advances.
    pub advance: f32,
    /// The byte range of the shaped text this run covers.
    ///
    /// Opaque here — this crate has no idea what the text came from — and the one
    /// thing a caller needs to map a run back to whatever produced it. Hit testing
    /// a link is that mapping.
    pub text_range: std::ops::Range<usize>,
    /// The glyphs, in visual order.
    pub glyphs: Vec<Glyph>,
}

/// A decoration line under or through a run, with the metrics the font gives it.
///
/// Taken from the font rather than guessed from the size: where an underline sits
/// and how thick it is are design decisions the typeface already made, and a
/// constant fraction of the em looks wrong in exactly the faces people notice.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Decoration {
    /// Distance from the baseline, positive upward.
    pub offset: f32,
    /// Thickness in logical pixels.
    pub thickness: f32,
}

/// One line of a shaped paragraph.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LineMetrics {
    /// Distance from the paragraph top to this line's top.
    pub top: f32,
    /// Distance from the paragraph top to this line's baseline.
    pub baseline: f32,
    /// The line's height.
    pub height: f32,
    /// The line's advance width.
    pub width: f32,
}

/// A span of text with one style, for shaping a paragraph made of several.
///
/// The text is borrowed. Layout runs on every resize and a page has thousands of
/// spans; owning each one would be thousands of copies per frame of text that is
/// already sitting in the box tree.
#[derive(Clone, Debug)]
pub struct TextSpan<'a> {
    /// The text itself.
    pub text: &'a str,
    /// The families to try, in order.
    pub font_stack: FontStack,
    /// Size in logical pixels.
    pub font_size: f32,
    /// CSS `font-weight`, 100–900.
    pub font_weight: u16,
    /// Whether the run is italic.
    pub italic: bool,
    /// Whether to draw a line under the run.
    pub underline: bool,
    /// Whether to draw a line through it.
    pub strikethrough: bool,
    /// Colour, straight RGBA.
    pub brush: Brush,
    /// Line height in logical pixels, or `None` for the font's own.
    pub line_height: Option<f32>,
}

/// A gap reserved between two spans, and a marker of where their boundary landed.
///
/// Shaping a paragraph is the only thing that knows where a span boundary ends up
/// once lines have been broken, so a caller that needs to reserve horizontal space
/// at one — the space a border and a padding take on the edge of an inline element —
/// has to ask for it here rather than adding it afterwards.
///
/// A zero-width spacer reserves nothing and still comes back positioned, which is
/// how a caller finds a span's extent inside a run that the shaper merged with its
/// neighbours.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Spacer {
    /// The caller's own identifier, handed back untouched.
    pub id: u64,
    /// Where the spacer goes: before `spans[at]`, or after the last span when `at`
    /// is the number of spans.
    pub at: usize,
    /// Width to reserve, in logical pixels. May be zero.
    pub width: f32,
    /// Height to reserve. A spacer with a height makes the line at least that
    /// tall, which is what an image sitting in a paragraph does; zero leaves the
    /// line the height of its text, which is what a border and a padding do.
    pub height: f32,
}

/// A spacer once the paragraph has been broken into lines.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PlacedSpacer {
    /// The identifier the caller gave the [`Spacer`].
    pub id: u64,
    /// Which line it landed on.
    pub line: usize,
    /// Where it starts along the paragraph, in the same coordinates as
    /// [`ShapedRun::offset_x`].
    pub x: f32,
    /// The width it reserved.
    pub width: f32,
    /// Where its top sits, measured from the paragraph top like a line's own top.
    pub y: f32,
    /// The height it reserved.
    pub height: f32,
}

/// Metrics of a whole shaped paragraph.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextMetrics {
    /// Width of the widest line, in logical pixels.
    pub width: f32,
    /// Total height of all lines, in logical pixels.
    pub height: f32,
    /// Baseline of the first line, measured from the layout top.
    pub first_baseline: f32,
    /// Number of lines produced.
    pub line_count: usize,
}

/// A shaped paragraph: its runs and its metrics.
#[derive(Clone, Debug)]
pub struct ShapedText {
    /// Positioned glyph runs, in paint order.
    pub runs: Vec<ShapedRun>,
    /// Metrics for the paragraph as a whole.
    pub metrics: TextMetrics,
    /// One entry per line, in order.
    pub lines: Vec<LineMetrics>,
    /// The spacers the caller asked for, positioned. Empty when none were asked
    /// for, which is every paragraph without a bordered inline element in it.
    pub spacers: Vec<PlacedSpacer>,
}

impl ShapedText {
    /// Total advance width of the paragraph.
    pub fn width(&self) -> f32 {
        self.metrics.width
    }

    /// Number of glyphs across every run.
    pub fn glyph_count(&self) -> usize {
        self.runs.iter().map(|run| run.glyphs.len()).sum()
    }
}

/// Owns the font collection and the shaping caches.
///
/// Construction discovers system fonts, which is slow enough that this should be
/// built once and kept, not built per paragraph.
pub struct TextEngine {
    fonts: FontContext,
    layout: LayoutContext<[u8; 4]>,
}

impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine").finish_non_exhaustive()
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    /// Build an engine over the system fonts, with the vendored family registered.
    pub fn new() -> Self {
        let mut fonts = FontContext::new();
        register_test_font(&mut fonts);
        Self {
            fonts,
            layout: LayoutContext::new(),
        }
    }

    /// Build an engine with **only** the vendored family available.
    ///
    /// This is what measurement tests use. It cannot pick up a system font, so a
    /// golden number produced here holds on any machine.
    pub fn isolated() -> Self {
        let mut fonts = FontContext {
            collection: parley::fontique::Collection::new(parley::fontique::CollectionOptions {
                system_fonts: false,
                ..Default::default()
            }),
            source_cache: parley::fontique::SourceCache::default(),
        };
        register_test_font(&mut fonts);

        // Point every generic family at the vendored font. Without this, an isolated
        // engine cannot shape `sans-serif` — which is what every real document asks
        // for — and a layout test would measure an empty line rather than a wrong
        // one. The substitution is exactly what makes the numbers machine
        // independent.
        if let Some(id) = fonts.collection.family_id(TEST_FAMILY) {
            for generic in [
                parley::GenericFamily::Serif,
                parley::GenericFamily::SansSerif,
                parley::GenericFamily::Monospace,
                parley::GenericFamily::Cursive,
                parley::GenericFamily::Fantasy,
                parley::GenericFamily::SystemUi,
                parley::GenericFamily::UiSerif,
                parley::GenericFamily::UiSansSerif,
                parley::GenericFamily::UiMonospace,
                parley::GenericFamily::UiRounded,
                parley::GenericFamily::Emoji,
                parley::GenericFamily::Math,
            ] {
                fonts
                    .collection
                    .set_generic_families(generic, std::iter::once(id));
            }
        }

        Self {
            fonts,
            layout: LayoutContext::new(),
        }
    }

    /// Shape and position `text`, breaking lines at `max_advance` if given.
    ///
    /// `scale` is the device scale factor. parley applies it internally for
    /// hinting decisions; the positions that come back are still logical pixels.
    pub fn shape(
        &mut self,
        text: &str,
        stack: &FontStack,
        font_size: f32,
        max_advance: Option<f32>,
    ) -> ShapedText {
        let mut builder = self.layout.ranged_builder(&mut self.fonts, text, 1.0, true);

        // Applied to every layout, without exception. This is a table of ASCII
        // break-pair rules, not a dependency on any other engine: bare UAX#14
        // disagrees with what the web actually does at several pairs.
        builder.set_line_break_override(Some(parley::CHROMIUM_LINE_BREAK_OVERRIDE));
        builder.push_default(StyleProperty::FontFamily(stack.to_parley()));
        builder.push_default(StyleProperty::FontSize(font_size));

        let mut layout = builder.build(text);
        layout.break_all_lines(max_advance);
        layout.align(Alignment::Start, AlignmentOptions::default());
        collect(&layout)
    }

    /// Shape several differently-styled spans as **one** paragraph.
    ///
    /// This is what an inline formatting context needs, and why it cannot simply
    /// shape each span on its own: a line break may fall between two spans, and
    /// `bold text` must break in the same place whether or not the `bold` is a
    /// separate element. So the spans are concatenated, styled by range, and broken
    /// together; each run comes back carrying the colour it was asked for.
    /// `spacers` reserve horizontal space at span boundaries and come back
    /// positioned; see [`Spacer`].
    pub fn shape_spans(
        &mut self,
        spans: &[TextSpan<'_>],
        spacers: &[Spacer],
        max_advance: Option<f32>,
    ) -> ShapedText {
        let mut text = String::new();
        let mut ranges = Vec::with_capacity(spans.len());
        let mut boundaries = Vec::with_capacity(spans.len() + 1);
        for span in spans {
            let start = text.len();
            boundaries.push(start);
            text.push_str(span.text);
            ranges.push(start..text.len());
        }
        boundaries.push(text.len());

        let mut builder = self
            .layout
            .ranged_builder(&mut self.fonts, &text, 1.0, true);
        builder.set_line_break_override(Some(parley::CHROMIUM_LINE_BREAK_OVERRIDE));

        for spacer in spacers {
            let Some(&index) = boundaries.get(spacer.at) else {
                continue;
            };
            builder.push_inline_box(parley::InlineBox {
                id: spacer.id,
                kind: parley::InlineBoxKind::InFlow,
                index,
                width: spacer.width,
                height: spacer.height,
            });
        }

        for (span, range) in spans.iter().zip(ranges) {
            if range.is_empty() {
                continue;
            }
            builder.push(
                StyleProperty::FontFamily(span.font_stack.to_parley()),
                range.clone(),
            );
            builder.push(StyleProperty::FontSize(span.font_size), range.clone());
            builder.push(
                StyleProperty::FontWeight(parley::FontWeight::new(f32::from(span.font_weight))),
                range.clone(),
            );
            builder.push(
                StyleProperty::FontStyle(if span.italic {
                    parley::FontStyle::Italic
                } else {
                    parley::FontStyle::Normal
                }),
                range.clone(),
            );
            builder.push(StyleProperty::Underline(span.underline), range.clone());
            builder.push(
                StyleProperty::Strikethrough(span.strikethrough),
                range.clone(),
            );
            builder.push(StyleProperty::Brush(span.brush), range.clone());
            if let Some(line_height) = span.line_height {
                builder.push(
                    StyleProperty::LineHeight(parley::LineHeight::Absolute(line_height)),
                    range,
                );
            }
        }

        let mut layout = builder.build(&text);
        layout.break_all_lines(max_advance);
        layout.align(Alignment::Start, AlignmentOptions::default());
        collect(&layout)
    }

    /// Measure without keeping the glyphs.
    pub fn measure(&mut self, text: &str, stack: &FontStack, font_size: f32) -> TextMetrics {
        self.shape(text, stack, font_size, None).metrics
    }

    /// Whether a family name resolves to anything in the collection.
    pub fn has_family(&mut self, name: &str) -> bool {
        self.fonts.collection.family_by_name(name).is_some()
    }
}

/// Pull runs, lines and metrics out of a broken parley layout.
fn collect(layout: &parley::Layout<Brush>) -> ShapedText {
    let mut runs = Vec::new();
    let mut lines = Vec::new();
    let mut spacers = Vec::new();
    let mut first_baseline = 0.0;

    // How many of each parley run's clusters have already been handed out. A run
    // spans line breaks and style changes, so the glyph runs that come back are
    // slices of it, in order — and the byte range of a slice is only recoverable by
    // walking the clusters alongside them. parley reports a range for the whole run,
    // which is not the same thing and is what a naive reading gets wrong: with one
    // font and one size, a paragraph of differently coloured links is *one* run.
    let mut consumed: Vec<usize> = Vec::new();

    for (index, line) in layout.lines().enumerate() {
        let metrics = line.metrics();
        if index == 0 {
            first_baseline = metrics.baseline;
        }
        lines.push(LineMetrics {
            top: metrics.block_min_coord,
            baseline: metrics.baseline,
            height: metrics.line_height,
            width: metrics.advance,
        });

        for item in line.items() {
            let glyph_run = match item {
                PositionedLayoutItem::GlyphRun(glyph_run) => glyph_run,
                PositionedLayoutItem::InlineBox(placed) => {
                    spacers.push(PlacedSpacer {
                        id: placed.id,
                        line: index,
                        x: placed.x,
                        width: placed.width,
                        y: placed.y,
                        height: placed.height,
                    });
                    continue;
                }
            };
            let style = glyph_run.style();
            let brush = style.brush;
            let run = glyph_run.run();
            let metrics = run.metrics();

            let underline = style.underline.as_ref().map(|decoration| Decoration {
                offset: decoration.offset.unwrap_or(metrics.underline_offset),
                thickness: decoration.size.unwrap_or(metrics.underline_size).max(1.0),
            });
            let strikethrough = style.strikethrough.as_ref().map(|decoration| Decoration {
                offset: decoration.offset.unwrap_or(metrics.strikethrough_offset),
                thickness: decoration
                    .size
                    .unwrap_or(metrics.strikethrough_size)
                    .max(1.0),
            });

            let run_index = run.index();
            if consumed.len() <= run_index {
                consumed.resize(run_index + 1, 0);
            }
            let text_range = consume_clusters(run, &mut consumed[run_index], glyph_run.advance());

            let glyphs: Vec<Glyph> = glyph_run
                .positioned_glyphs()
                .map(|glyph| Glyph {
                    id: glyph.id,
                    x: glyph.x,
                    y: glyph.y,
                })
                .collect();

            if glyphs.is_empty() {
                continue;
            }

            runs.push(ShapedRun {
                font: run.font().clone(),
                font_size: run.font_size(),
                normalized_coords: run.normalized_coords().to_vec(),
                brush,
                underline,
                strikethrough,
                line: index,
                offset_x: glyph_run.offset(),
                advance: glyph_run.advance(),
                text_range,
                glyphs,
            });
        }
    }

    ShapedText {
        metrics: TextMetrics {
            width: layout.width(),
            height: layout.height(),
            first_baseline,
            line_count: layout.len(),
        },
        runs,
        lines,
        spacers,
    }
}

/// Take clusters from `run`, starting at `from`, until they add up to `advance`,
/// and report the byte range they cover.
///
/// Advances are floats and a slice of them will not sum exactly, so the comparison
/// carries a half-pixel of slack; overshooting by one cluster would attribute a
/// character to the wrong element, which for a link means the wrong destination.
fn consume_clusters(
    run: &parley::Run<'_, Brush>,
    from: &mut usize,
    advance: f32,
) -> std::ops::Range<usize> {
    let mut taken = 0.0;
    let mut start = None;
    let mut end = *from;

    for cluster in run.clusters().skip(*from) {
        if taken > 0.0 && taken + cluster.advance() > advance + 0.5 {
            break;
        }
        let range = cluster.text_range();
        start.get_or_insert(range.start);
        end = range.end;
        taken += cluster.advance();
        *from += 1;
        if taken >= advance - 0.5 {
            break;
        }
    }

    start.unwrap_or(end)..end
}

/// Register the vendored font under [`TEST_FAMILY`], overriding whatever family
/// name is baked into the file, so the name is ours and cannot collide with a
/// system font that happens to be called Roboto.
fn register_test_font(fonts: &mut FontContext) {
    let blob = parley::fontique::Blob::new(std::sync::Arc::new(TEST_FONT));
    let registered = fonts.collection.register_fonts(
        blob,
        Some(parley::fontique::FontInfoOverride {
            family_name: Some(TEST_FAMILY),
            ..Default::default()
        }),
    );

    if registered.is_empty() {
        // The font is compiled into the binary, so this cannot be a missing-file
        // problem; it means the file itself is unreadable.
        tracing::error!("the vendored test font failed to register");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> TextEngine {
        TextEngine::isolated()
    }

    fn test_stack() -> FontStack {
        FontStack::named(TEST_FAMILY)
    }

    #[test]
    fn the_vendored_family_is_registered() {
        let mut engine = engine();
        assert!(
            engine.has_family(TEST_FAMILY),
            "{TEST_FAMILY} should resolve"
        );
    }

    #[test]
    fn shaping_produces_one_glyph_per_ascii_character() {
        let mut engine = engine();
        let shaped = engine.shape("Otlyra", &test_stack(), 32.0, None);
        assert_eq!(shaped.glyph_count(), 6);
        assert_eq!(shaped.metrics.line_count, 1);
    }

    /// The advance-width golden. Measured against the repo-vendored font, so it is
    /// machine-independent. If this number moves, either the font changed or the
    /// shaper did — both are things we want to be told about.
    #[test]
    fn advance_width_matches_the_golden_number() {
        let mut engine = engine();
        let metrics = engine.measure("Otlyra", &test_stack(), 32.0);
        let expected = 82.968_75_f32;
        assert!(
            (metrics.width - expected).abs() < 0.01,
            "advance width was {}, expected {expected} (±0.01)",
            metrics.width
        );
    }

    #[test]
    fn advance_width_scales_with_font_size() {
        let mut engine = engine();
        let small = engine.measure("Otlyra", &test_stack(), 16.0).width;
        let large = engine.measure("Otlyra", &test_stack(), 32.0).width;
        assert!(
            (large - small * 2.0).abs() < 0.1,
            "32px width {large} should be twice the 16px width {small}"
        );
    }

    #[test]
    fn runs_carry_the_font_and_size_they_were_shaped_at() {
        let mut engine = engine();
        let shaped = engine.shape("Otlyra", &test_stack(), 24.0, None);
        let run = shaped.runs.first().expect("one run");
        assert_eq!(run.font_size, 24.0);
        assert!(!run.font.data.as_ref().is_empty());
    }

    #[test]
    fn glyph_positions_advance_left_to_right() {
        let mut engine = engine();
        let shaped = engine.shape("Otlyra", &test_stack(), 32.0, None);
        let glyphs = &shaped.runs[0].glyphs;
        for pair in glyphs.windows(2) {
            assert!(pair[1].x > pair[0].x, "glyphs should advance: {pair:?}");
        }
    }

    #[test]
    fn a_max_advance_breaks_lines() {
        let mut engine = engine();
        let text = "the quick brown fox jumps over the lazy dog";
        let unbroken = engine.shape(text, &test_stack(), 16.0, None);
        let broken = engine.shape(text, &test_stack(), 16.0, Some(100.0));

        assert_eq!(unbroken.metrics.line_count, 1);
        assert!(broken.metrics.line_count > 1, "expected wrapping at 100px");
        assert!(broken.metrics.width <= 100.0);
        assert!(broken.metrics.height > unbroken.metrics.height);
    }

    /// Break opportunities over a fixed corpus. The override table is what makes
    /// these match web behaviour rather than plain UAX#14.
    #[test]
    fn break_opportunities_match_a_fixed_table() {
        let mut engine = engine();
        // Wide enough for one word, never two, so the line count is exactly the
        // number of break opportunities taken plus one.
        let cases: [(&str, usize); 4] = [
            ("alpha beta", 2),
            ("alpha beta gamma", 3),
            ("alphabeta", 1),
            ("alpha-beta gamma", 3),
        ];

        for (text, expected_lines) in cases {
            let shaped = engine.shape(text, &test_stack(), 16.0, Some(48.0));
            assert_eq!(
                shaped.metrics.line_count, expected_lines,
                "{text:?} should break into {expected_lines} lines"
            );
        }
    }

    fn span(text: &str, size: f32, brush: Brush) -> TextSpan<'_> {
        TextSpan {
            text,
            font_stack: test_stack(),
            font_size: size,
            font_weight: 400,
            italic: false,
            underline: false,
            strikethrough: false,
            brush,
            line_height: None,
        }
    }

    #[test]
    fn spans_keep_the_colour_they_were_asked_for() {
        let mut engine = engine();
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        let shaped = engine.shape_spans(
            &[span("red ", 16.0, red), span("blue", 16.0, blue)],
            &[],
            None,
        );

        let brushes: Vec<Brush> = shaped.runs.iter().map(|run| run.brush).collect();
        assert_eq!(brushes, vec![red, blue]);
    }

    /// A zero-width spacer measures a span boundary without moving anything, which
    /// is what finds a span inside a run the shaper merged with its neighbours.
    #[test]
    fn zero_width_spacers_mark_a_span_boundary_without_moving_it() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [span("one", 16.0, brush), span("two", 16.0, brush)];
        let plain = engine.shape_spans(&spans, &[], None);
        let marked = engine.shape_spans(
            &spans,
            &[
                Spacer {
                    id: 1,
                    at: 1,
                    width: 0.0,
                    height: 0.0,
                },
                Spacer {
                    id: 2,
                    at: 2,
                    width: 0.0,
                    height: 0.0,
                },
            ],
            None,
        );

        assert_eq!(marked.metrics.width, plain.metrics.width);
        let placed: Vec<(u64, f32)> = marked
            .spacers
            .iter()
            .map(|spacer| (spacer.id, spacer.x))
            .collect();
        assert_eq!(placed.len(), 2);
        assert!(placed[0].1 > 0.0, "the boundary is past the first span");
        assert_eq!(
            placed[1].1, marked.metrics.width,
            "and the end of the text is the end of the line"
        );
    }

    /// A spacer with a width is how an inline element's border and padding take
    /// room: the text after it moves over by exactly that much.
    #[test]
    fn a_spacer_reserves_room_in_the_line() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [span("one", 16.0, brush), span("two", 16.0, brush)];
        let plain = engine.shape_spans(&spans, &[], None);
        let spaced = engine.shape_spans(
            &spans,
            &[Spacer {
                id: 1,
                at: 1,
                width: 20.0,
                height: 0.0,
            }],
            None,
        );

        assert!((spaced.metrics.width - plain.metrics.width - 20.0).abs() < 0.01);
    }

    /// The reason spans are shaped together rather than one at a time: the break
    /// belongs to the paragraph, not to whichever element the words came from.
    #[test]
    fn a_line_break_may_fall_between_two_spans() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let together = engine.shape_spans(
            &[span("alpha ", 16.0, brush), span("beta", 16.0, brush)],
            &[],
            Some(48.0),
        );
        let one_string = engine.shape("alpha beta", &test_stack(), 16.0, Some(48.0));

        assert_eq!(together.metrics.line_count, 2);
        assert_eq!(
            together.metrics.line_count, one_string.metrics.line_count,
            "styling must not change where the text breaks"
        );
        assert!(
            (together.metrics.height - one_string.metrics.height).abs() < 0.01,
            "nor how tall it is"
        );
    }

    #[test]
    fn a_larger_span_makes_its_line_taller_and_shares_the_baseline() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let mixed = engine.shape_spans(
            &[span("small ", 12.0, brush), span("BIG", 32.0, brush)],
            &[],
            None,
        );

        assert_eq!(mixed.lines.len(), 1);
        let line = mixed.lines[0];
        assert!(line.height >= 32.0, "line height was {}", line.height);
        for run in &mixed.runs {
            let baseline_y = run.glyphs[0].y;
            assert!(
                (baseline_y - line.baseline).abs() < 0.01,
                "run at {baseline_y} should sit on the line's baseline {}",
                line.baseline
            );
        }
    }

    #[test]
    fn line_metrics_stack_top_to_bottom() {
        let mut engine = engine();
        let shaped = engine.shape("alpha beta gamma", &test_stack(), 16.0, Some(48.0));
        assert_eq!(shaped.lines.len(), 3);
        for pair in shaped.lines.windows(2) {
            assert!(pair[1].top > pair[0].top);
            assert!(pair[1].baseline > pair[0].baseline);
        }
        assert!(shaped.runs.iter().all(|run| run.line < 3));
    }

    #[test]
    fn empty_text_shapes_to_nothing() {
        let mut engine = engine();
        let shaped = engine.shape("", &test_stack(), 32.0, None);
        assert_eq!(shaped.glyph_count(), 0);
        assert_eq!(shaped.width(), 0.0);
    }
}
