//! Shaping and measurement over parley.

use std::collections::HashMap;

use otlyra_gfx::Glyph;
use parley::{
    Alignment, AlignmentOptions, FontContext, FontData, FontVariation, FontVariations,
    LayoutContext, PositionedLayoutItem, StyleProperty,
};

use crate::breaking::{self, Glyphs, Plan};
use crate::web_fonts::{FaceDescriptors, FaceQuery, FaceStyle, Resolved, WebFamilies};
use crate::{FontStack, TEST_FAMILY, TEST_FONT};

/// The optical-size axis, set from the font size on every run.
///
/// A typeface with this axis carries several designs along it: the small end is
/// wider and more open so it stays legible at body sizes, the large end tighter.
/// A font that has the axis and is never told what size it is being set at draws
/// its default design at every size, which is a visible difference in both the
/// shape of the letters and how far apart they sit — and on this platform the
/// system interface font has the axis, so it is most of the text on most pages.
///
/// This is CSS `font-optical-sizing: auto`, which is the initial value: the axis
/// takes the used font size, in pixels.
const OPTICAL_SIZE: parley::setting::Tag = parley::setting::Tag::new(b"opsz");

/// The weight and width axes, set from `font-weight` and `font-width`.
const WEIGHT: parley::setting::Tag = parley::setting::Tag::new(b"wght");
const WIDTH: parley::setting::Tag = parley::setting::Tag::new(b"wdth");

/// The variation settings one span is shaped with, in the order CSS Fonts 4 §7
/// applies them.
///
/// First the font matching variations: the weight and width the span asks
/// for, so a variable face is drawn at them rather than at whatever instance
/// its file defaults to — a face a page declares over a range of weights is
/// one file for all of them. Then the optical size, and what the page asked
/// for last, so a page that names `opsz` itself overrides the automatic one
/// rather than fighting it — which is what `font-optical-sizing: none`
/// alongside an explicit axis means. A font without an axis ignores the
/// setting for it, so they are applied without first asking which font the run
/// resolved to, which is not known until after shaping has picked one. For the
/// same reason a weight outside the range a face's rule declares is not
/// clamped to it, only to the file's own axis.
fn variations(span: &TextSpan<'_>) -> FontVariations<'static> {
    let mut settings = Vec::with_capacity(span.variations.len() + 3);
    settings.push(FontVariation::new(WEIGHT, f32::from(span.font_weight)));
    settings.push(FontVariation::new(WIDTH, span.font_width));
    if span.optical_sizing {
        settings.push(FontVariation::new(OPTICAL_SIZE, span.font_size));
    }
    settings.extend(
        span.variations
            .iter()
            .map(|&(axis, value)| FontVariation::new(parley::setting::Tag::new(&axis), value)),
    );
    FontVariations::List(settings.into())
}

/// How far a font of one size reaches above and below its baseline.
///
/// The parts of a line box, kept apart rather than added up, because what needs
/// them needs them apart: a raised or lowered box grows the line by its own reach
/// plus how far it moved, and the two ends grow independently.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Strut {
    /// Above the baseline.
    pub ascent: f32,
    /// Below it.
    pub descent: f32,
    /// Extra space the font asks for, which CSS splits above and below.
    pub leading: f32,
}

impl Strut {
    /// The whole line: everything above the baseline plus everything below.
    pub fn height(self) -> f32 {
        self.ascent + self.descent + self.leading
    }
}

/// The families whose ascent is lengthened before a line is measured from it.
const LENGTHENED_FAMILIES: &[&str] = &["Times", "Helvetica", "Courier"];

/// How far a font of `family` reaches above its baseline, as a browser
/// measures it: the font's own ascent, and for three families more.
///
/// The three families a browser lengthens. Their vertical metrics are the ones
/// the platform shipped in the 1980s and are tighter than the Microsoft-issued
/// faces of the same names that the web was built against; left alone, a page
/// set in them has visibly less air between its lines than the same page
/// anywhere else. Fifteen per cent of the em box, added to the ascent, is the
/// correction every engine settled on — and every measure taken from the
/// ascent, an overline's place among them, is taken from the lengthened one.
fn browser_ascent(family: &str, ascent: f32, descent: f32) -> f32 {
    if LENGTHENED_FAMILIES
        .iter()
        .any(|name| family.eq_ignore_ascii_case(name))
    {
        ascent + ((ascent + descent) * 0.15 + 0.5).floor()
    } else {
        ascent
    }
}

/// The family name a font file gives itself, typographic first.
fn family_name(font: &parley::FontData) -> Option<String> {
    use skrifa::MetadataProvider as _;
    use skrifa::string::StringId;

    let font = skrifa::FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    [StringId::TYPOGRAPHIC_FAMILY_NAME, StringId::FAMILY_NAME]
        .into_iter()
        .find_map(|id| font.localized_strings(id).english_or_first())
        .map(|name| name.chars().collect())
}

/// The colour carried through shaping, as straight RGBA bytes.
///
/// parley calls this a brush and hands it back with each glyph run, which is what
/// lets one paragraph contain differently coloured spans without shaping each of
/// them separately and losing the line breaks between them.
pub type Brush = [u8; 4];

/// What the shaper carries for each span: its colour, and which span it is.
///
/// The span's index is what keeps a run of glyphs inside one span. The shaper
/// ends a run where the style changes, and two neighbouring spans of the same
/// colour and font would otherwise be one run — which the box tree would then
/// give to the first span's box, with that box's decorations, its link and
/// its hit testing, for text that is not its own.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct SpanBrush {
    colour: Brush,
    span: usize,
}

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
    /// Where the run's font puts the lines a decoration draws.
    pub decoration_metrics: DecorationMetrics,
    /// Which line of the paragraph the run belongs to.
    pub line: usize,
    /// Where the run starts along its line, in logical pixels.
    pub offset_x: f32,
    /// How far the run advances.
    pub advance: f32,
    /// The characters this run drew.
    ///
    /// Carried rather than recovered: a glyph identifier is a place in a font and
    /// says nothing about the character it was chosen for, and everything that
    /// reads text back off the page — a selection, a copy, a screen reader — needs
    /// the characters rather than the shapes.
    pub text: std::sync::Arc<str>,
    /// The byte range of the shaped text this run covers.
    ///
    /// Opaque here — this crate has no idea what the text came from — and the one
    /// thing a caller needs to map a run back to whatever produced it. Hit testing
    /// a link is that mapping.
    pub text_range: std::ops::Range<usize>,
    /// The glyphs, in visual order.
    pub glyphs: Vec<Glyph>,
}

/// What a run's font says about the lines a text decoration draws, at the
/// run's size.
///
/// Whether a line is drawn at all, and how thick, is the style's to say, not
/// the shaper's; where an underline sits is a design decision the typeface
/// already made, and a constant fraction of the em looks wrong in exactly the
/// faces people notice.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DecorationMetrics {
    /// The underline's distance from the baseline, positive upward.
    pub underline_offset: f32,
    /// How far the font reaches above the baseline.
    pub ascent: f32,
}

/// One line of a shaped paragraph.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LineMetrics {
    /// Distance from the paragraph top to this line's top.
    pub top: f32,
    /// Distance from the paragraph top to this line's baseline.
    pub baseline: f32,
    /// The line's height: the space the shaper stacks the next line by.
    pub height: f32,
    /// Distance from the paragraph top to the lowest thing on this line.
    ///
    /// Not always `top + height`. A picture in a line sits with its bottom on the
    /// baseline and the text's descenders hang below that, so the line reaches
    /// further down than the height it is stacked by — and a paragraph that is one
    /// such line is that much taller than the shaper reports.
    pub bottom: f32,
    /// The line's advance width, trailing whitespace and all.
    pub width: f32,
    /// How much of that width is the white space that hangs at the end of the
    /// line: spaces, tabs and the line break itself.
    ///
    /// A space at a line break is not part of the line as far as anything measuring
    /// it is concerned: what a paragraph needs at its narrowest is its longest word,
    /// not that word and the space that follows it (CSS Text 3 §4.1.3). A no-break
    /// space is not one of them: it is not collapsible and does not hang, so a line
    /// ending in one is as wide as its advance says.
    pub trailing_space: f32,
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
    /// CSS `font-width` (`font-stretch`) as a percentage, 100 being normal.
    pub font_width: f32,
    /// Whether the run is italic.
    pub italic: bool,
    /// Colour, straight RGBA.
    pub brush: Brush,
    /// Line height in logical pixels, or `None` for the font's own.
    pub line_height: Option<f32>,
    /// `letter-spacing` in logical pixels, added after every character including
    /// the last, which is what CSS says and what makes a spaced word wider by the
    /// spacing times its whole length.
    pub letter_spacing: f32,
    /// `word-spacing` in logical pixels, added at every space.
    pub word_spacing: f32,
    /// Whether the optical-size axis takes the font size — `font-optical-sizing`,
    /// which is `auto` unless the page says otherwise.
    pub optical_sizing: bool,
    /// `font-variation-settings`: axis tags and the values asked for. Borrowed,
    /// for the same reason the text is, and empty on almost every span there is.
    pub variations: &'a [([u8; 4], f32)],
}

impl<'a> TextSpan<'a> {
    /// A span of `text` in one family at one size, with everything else as CSS
    /// leaves it.
    ///
    /// Building one field by field is what a caller with a computed style does;
    /// this is for the callers that have a string and a font and nothing to say
    /// about the rest.
    pub fn new(text: &'a str, font_stack: FontStack, font_size: f32) -> Self {
        Self {
            text,
            font_stack,
            font_size,
            font_weight: 400,
            font_width: 100.0,
            italic: false,
            brush: [0, 0, 0, 255],
            line_height: None,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            optical_sizing: true,
            variations: &[],
        }
    }

    /// What the span asks of a family's faces.
    #[must_use]
    pub fn face_query(&self) -> FaceQuery {
        FaceQuery {
            weight: f32::from(self.font_weight),
            italic: self.italic,
            stretch: self.font_width,
        }
    }
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
    /// What it stands for, which decides whether a line may break beside it.
    pub kind: SpacerKind,
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

/// What a [`Spacer`] stands for in the line, which is what says whether a line
/// may break beside it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SpacerKind {
    /// The start edge of an inline box — its margin, border and padding on that
    /// side. It holds on to what follows it: an element's edges are no soft wrap
    /// opportunity (CSS Text 3 §5.1), so a line breaks before it or not at all.
    Opening,
    /// The end edge of an inline box, which holds on to what goes before it.
    Closing,
    /// An atomic inline — a picture, an inline block, a widget. A line may break
    /// on either side of one, as UAX #14 breaks either side of the object
    /// replacement character it stands for in the text (LB20).
    Atomic,
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
    /// Width of the widest line, in logical pixels, without the white space
    /// that hangs at its end.
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
    layout: LayoutContext<SpanBrush>,
    /// The families pages have brought with `@font-face`.
    web: WebFamilies,
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
        prefer_browser_families(&mut fonts);
        Self {
            fonts,
            layout: LayoutContext::new(),
            web: WebFamilies::default(),
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
            web: WebFamilies::default(),
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
        self.shape_spans(
            &[TextSpan::new(text, stack.clone(), font_size)],
            &[],
            max_advance,
        )
    }

    /// The line a font of this size makes when CSS says `line-height: normal`.
    ///
    /// This is the *strut*: the line box a block has before any text is put in it,
    /// which CSS derives from the block's own font. It is a browser decision rather
    /// than a reading of the font, and the two rules below are what every browser
    /// arrived at — a page laid out without them has the right words in the right
    /// order and the wrong rhythm on every line of it.
    ///
    /// `None` if the stack resolves to no font at all, which leaves the caller to
    /// let the shaper decide.
    pub fn strut(&mut self, stack: &FontStack, font_size: f32, query: FaceQuery) -> Option<Strut> {
        let (blob, index, family) = self.resolve(stack, query)?;
        let font = skrifa::FontRef::from_index(blob.as_ref(), index).ok()?;
        let metrics = skrifa::metrics::Metrics::new(
            &font,
            skrifa::prelude::Size::new(font_size),
            skrifa::instance::LocationRef::default(),
        );

        let descent = -metrics.descent;
        let ascent = browser_ascent(&family, metrics.ascent, descent);

        // Rounded one at a time and then added, rather than added and rounded. The
        // difference is a pixel on many fonts and it is the difference between
        // lines landing where a reference browser puts them and landing a pixel out
        // per line, which accumulates down a page.
        Some(Strut {
            ascent: ascent.round(),
            descent: descent.round(),
            leading: metrics.leading.round(),
        })
    }

    /// The first available font of `stack` (CSS Fonts 4 §5.2), as bytes, face
    /// index and family name: the first that exists and that `unicode-range`
    /// does not exclude the space from.
    fn resolve(
        &mut self,
        stack: &FontStack,
        query: FaceQuery,
    ) -> Option<(parley::fontique::Blob<u8>, u32, String)> {
        let width = parley::FontWidth::from_percentage(query.stretch);
        let style = if query.italic {
            parley::FontStyle::Italic
        } else {
            parley::FontStyle::Normal
        };
        let weight = parley::FontWeight::new(query.weight);

        for entry in self.web.expand(stack, query) {
            let info = match entry {
                Resolved::Web(face) if face.descriptors.covers(' ') => {
                    self.fonts.collection.family_by_name(&face.name)
                }
                Resolved::Web(_) => continue,
                Resolved::Installed(name) => self.fonts.collection.family_by_name(name),
                Resolved::Generic(generic) => {
                    // A generic nothing is registered for is a family that is not
                    // there, and the next one in the stack answers instead. Giving
                    // up on the whole stack here is what left a page set in
                    // `system-ui` with no metrics at all, and so with none of the
                    // rules a line height is made of.
                    let first = self
                        .fonts
                        .collection
                        .generic_families(generic.to_parley())
                        .next();
                    match first {
                        Some(id) => self.fonts.collection.family(id),
                        None => continue,
                    }
                }
            };
            let Some(info) = info else { continue };
            let Some(font) = info.match_font(width, style, weight, true) else {
                continue;
            };
            if let Some(blob) = font.load(Some(&mut self.fonts.source_cache)) {
                return Some((blob, font.index(), info.name().to_owned()));
            }
        }
        None
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
        self.shape_paragraph(spans, spacers, max_advance.is_some(), |_, _| max_advance)
    }

    /// Shape several spans as one paragraph, with the width decided line by line.
    ///
    /// `line_width` is asked for each line in turn, given its index and where its
    /// top has landed, and answers how wide that line may be. This is what lets
    /// text flow around something beside it: a float shortens the lines it sits
    /// next to and no others, and only the shaper knows where one line ends and the
    /// next begins.
    ///
    /// The width is asked for before the line is broken, so what it is given is the
    /// top of the line and not its height. A float that starts partway down a line
    /// therefore takes effect from the next one, which is the same approximation
    /// every engine makes somewhere and is invisible at ordinary line heights.
    pub fn shape_spans_wrapping(
        &mut self,
        spans: &[TextSpan<'_>],
        spacers: &[Spacer],
        line_width: impl FnMut(usize, f32) -> Option<f32>,
    ) -> ShapedText {
        self.shape_paragraph(spans, spacers, true, line_width)
    }

    /// Shape a paragraph and break it into lines as wide as `line_width` says,
    /// choosing where they break when `wraps` and the shaper would get that
    /// wrong (see [`crate::breaking`]).
    fn shape_paragraph(
        &mut self,
        spans: &[TextSpan<'_>],
        spacers: &[Spacer],
        wraps: bool,
        line_width: impl FnMut(usize, f32) -> Option<f32>,
    ) -> ShapedText {
        let text: String = spans.iter().map(|span| span.text).collect();
        let kinds: HashMap<u64, SpacerKind> = spacers
            .iter()
            .map(|spacer| (spacer.id, spacer.kind))
            .collect();

        let mut layout = self.build_layout(spans, spacers, &text);
        let plan = (wraps && breaking::needs_plan(&text, &kinds)).then(|| {
            breaking::break_everywhere(&mut layout);
            // The text's own opportunities, from the same text without the
            // edges of its inline boxes, which the shaper would break at too.
            let unedged = kinds
                .values()
                .any(|kind| *kind != SpacerKind::Atomic)
                .then(|| {
                    let atomic: Vec<Spacer> = spacers
                        .iter()
                        .filter(|spacer| spacer.kind == SpacerKind::Atomic)
                        .copied()
                        .collect();
                    let mut unedged = self.build_layout(spans, &atomic, &text);
                    breaking::break_everywhere(&mut unedged);
                    unedged
                });
            Plan::new(&layout, unedged.as_ref().unwrap_or(&layout), &text, &kinds)
        });
        break_lines(&mut layout, plan.as_ref(), line_width);
        layout.align(Alignment::Start, AlignmentOptions::default());
        let mut shaped = collect(&layout, &text, &kinds);

        // A tab is not a character of a width: it is a jump to the next tab stop,
        // and where that is depends on how far along the line the tab sits. The
        // shaper has no idea of one, so the glyphs after a tab are moved here,
        // once the line they landed on is known.
        if text.contains('\t') {
            let stop = self.tab_stop(spans.first());
            expand_tabs(&mut shaped, stop);
        }
        shaped
    }

    /// The spans laid end to end as one paragraph, styled by range, with the
    /// spacers at their span boundaries: shaped, and not yet broken into lines.
    fn build_layout(
        &mut self,
        spans: &[TextSpan<'_>],
        spacers: &[Spacer],
        text: &str,
    ) -> parley::Layout<SpanBrush> {
        let mut ranges = Vec::with_capacity(spans.len());
        let mut boundaries = Vec::with_capacity(spans.len() + 1);
        let mut end = 0;
        for span in spans {
            boundaries.push(end);
            ranges.push(end..end + span.text.len());
            end += span.text.len();
        }
        boundaries.push(end);

        let mut builder = self
            .layout
            .ranged_builder(&mut self.fonts, text, 1.0, false);
        builder.set_line_break_override(Some(parley::CHROMIUM_LINE_BREAK_OVERRIDE));

        // The first span's font, as the default under the ranged ones. A range is
        // only pushed for a span that has text in it, so a paragraph of nothing but
        // empty spans would otherwise be measured against the shaper's own default
        // font rather than the one the page asked for.
        if let Some(first) = spans.first() {
            builder.push_default(StyleProperty::FontFamily(
                self.web
                    .parley_family(&first.font_stack, first.face_query()),
            ));
            builder.push_default(StyleProperty::FontSize(first.font_size));
        }

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
        for (index, (span, range)) in spans.iter().zip(ranges).enumerate() {
            if range.is_empty() {
                continue;
            }
            for (piece, family) in
                self.web
                    .parley_families(&span.font_stack, span.face_query(), span.text)
            {
                let piece = range.start + piece.start..range.start + piece.end;
                builder.push(StyleProperty::FontFamily(family), piece);
            }
            builder.push(StyleProperty::FontSize(span.font_size), range.clone());
            builder.push(
                StyleProperty::FontVariations(variations(span)),
                range.clone(),
            );
            builder.push(
                StyleProperty::FontWeight(parley::FontWeight::new(f32::from(span.font_weight))),
                range.clone(),
            );
            builder.push(
                StyleProperty::FontWidth(parley::FontWidth::from_percentage(span.font_width)),
                range.clone(),
            );
            builder.push(
                StyleProperty::LetterSpacing(span.letter_spacing),
                range.clone(),
            );
            builder.push(StyleProperty::WordSpacing(span.word_spacing), range.clone());
            builder.push(
                StyleProperty::FontStyle(if span.italic {
                    parley::FontStyle::Italic
                } else {
                    parley::FontStyle::Normal
                }),
                range.clone(),
            );
            builder.push(
                StyleProperty::Brush(SpanBrush {
                    colour: span.brush,
                    span: index,
                }),
                range.clone(),
            );
            if let Some(line_height) = span.line_height {
                builder.push(
                    StyleProperty::LineHeight(parley::LineHeight::Absolute(line_height)),
                    range,
                );
            }
        }

        builder.build(text)
    }

    /// How far apart the tab stops are: eight spaces of the paragraph's own font,
    /// which is `tab-size`'s initial value and the only one read.
    fn tab_stop(&mut self, span: Option<&TextSpan<'_>>) -> f32 {
        const TAB_SIZE: f32 = 8.0;

        let Some(span) = span else {
            return 0.0;
        };
        // The *advance* of a space rather than the width of the line it makes:
        // a line of nothing but white space is a line of no width, because
        // trailing white space is not part of what a paragraph measures.
        let space = self
            .shape(" ", &span.font_stack, span.font_size, None)
            .lines
            .first()
            .map_or(0.0, |line| line.width);
        space * TAB_SIZE
    }

    /// Measure without keeping the glyphs.
    pub fn measure(&mut self, text: &str, stack: &FontStack, font_size: f32) -> TextMetrics {
        self.shape(text, stack, font_size, None).metrics
    }

    /// Add a face a page brought with it, as its `@font-face` rule describes
    /// it: of `family`, with `descriptors`, the `order`th rule of its page.
    ///
    /// The family is the rule's rather than the file's, and so are the weight,
    /// style and width, which is the whole of what `@font-face` does: a page
    /// may call a face anything it likes and then ask for it by that name, and
    /// what is baked into the file is not what the cascade will be asking
    /// about. Returns whether the bytes were a font.
    pub fn add_web_face(
        &mut self,
        family: &str,
        descriptors: &FaceDescriptors,
        order: usize,
        bytes: Vec<u8>,
    ) -> bool {
        let blob = parley::fontique::Blob::new(std::sync::Arc::new(unpack(bytes)));
        self.register_web_face(family, descriptors, order, blob)
    }

    /// Add an installed face as one a page's rule names with `local()`.
    ///
    /// The installed family called `name` is looked up, and its face nearest
    /// the rule's descriptors is taken. CSS Fonts 4 §4.3 has `local()` name a
    /// single face by its full or PostScript name; a family name is what pages
    /// write there as often, and it is the only name the collection is keyed
    /// by. Returns whether there was such a face.
    pub fn add_local_face(
        &mut self,
        name: &str,
        family: &str,
        descriptors: &FaceDescriptors,
        order: usize,
    ) -> bool {
        let (width, style, weight) = registered_attributes(descriptors);
        let Some(font) = self
            .fonts
            .collection
            .family_by_name(name)
            .and_then(|info| info.match_font(width, style, weight, false).cloned())
        else {
            return false;
        };
        let Some(blob) = font.load(Some(&mut self.fonts.source_cache)) else {
            return false;
        };
        // One face of a collection is lifted out on its own, or registering it
        // would register every face beside it under the rule's descriptors.
        let blob = if blob.as_ref().starts_with(b"ttcf") {
            match otlyra_gfx::face_from_collection(blob.as_ref(), font.index() as usize) {
                Some(face) => parley::fontique::Blob::new(std::sync::Arc::new(face)),
                None => return false,
            }
        } else {
            blob
        };
        self.register_web_face(family, descriptors, order, blob)
    }

    /// Register a face under a family name of its own, with the attributes
    /// its rule gives it.
    fn register_web_face(
        &mut self,
        family: &str,
        descriptors: &FaceDescriptors,
        order: usize,
        blob: parley::fontique::Blob<u8>,
    ) -> bool {
        let name = self.web.next_name(family);
        let (width, style, weight) = registered_attributes(descriptors);
        let registered = self.fonts.collection.register_fonts(
            blob,
            Some(parley::fontique::FontInfoOverride {
                family_name: Some(&name),
                width: Some(width),
                style: Some(style),
                weight: Some(weight),
                axes: None,
            }),
        );
        if registered.is_empty() {
            return false;
        }
        self.web.insert(family, name, descriptors.clone(), order);
        true
    }

    /// Whether a family name resolves to anything: a family a page brought,
    /// or one installed.
    pub fn has_family(&mut self, name: &str) -> bool {
        self.web.contains(name) || self.fonts.collection.family_by_name(name).is_some()
    }
}

/// How far past its width a line may reach and still fit: a sixty-fourth of a
/// pixel, which is Blink's `LayoutUnit`.
///
/// A box sized to its own max-content is given that width back through f32
/// border-box arithmetic — the content plus its padding and border, and the
/// padding and border taken off again — and it can come back an ulp narrower
/// than the text it was measured from. Its last word would then wrap, which a
/// box at its max-content size never does (CSS Sizing 3 §5.1). A line that
/// overflows by less than a layout unit overflows by nothing any engine can
/// draw, so it fits.
pub(crate) const LINE_FIT_SLACK: f32 = 1.0 / 64.0;

/// Break `layout` into lines, asking `line_width` how wide each one may be.
///
/// One line at a time rather than all at once, because the answer for a line
/// depends on where the line landed. Where there is a `plan`, it chooses where
/// each line ends and the shaper is given the advance that ends it there.
fn break_lines(
    layout: &mut parley::Layout<SpanBrush>,
    plan: Option<&Plan>,
    mut line_width: impl FnMut(usize, f32) -> Option<f32>,
) {
    let mut breaker = layout.break_lines();
    let mut index = 0usize;
    let mut top = 0.0f32;
    let mut start = 0usize;

    loop {
        let room = line_width(index, top).map_or(f32::INFINITY, |width| width + LINE_FIT_SLACK);
        let width = match plan {
            Some(plan) => {
                let (advance, next) = plan.line(start, room);
                start = next;
                advance
            }
            None => room,
        };
        // parley asserts the two are the same, and they are two names for one
        // thing until a line can be narrower than the paragraph it is in.
        breaker.state_mut().set_layout_max_advance(width);
        breaker.state_mut().set_line_max_advance(width);

        match breaker.break_next() {
            Some(parley::YieldData::LineBreak(line)) => {
                top = line.line_y_end as f32;
                index += 1;
            }
            // The other yields are for callers that place their own boxes or cap
            // the height; neither is asked for here.
            Some(_) => {}
            None => break,
        }
    }
}

/// Move what follows each tab to the next tab stop.
///
/// A tab in CSS is a jump and not a character: what it advances by is however far
/// it is to the next stop, so it can only be settled once the glyphs before it on
/// the line have been placed. The shaper knows nothing of stops — it gives the
/// tab whatever advance the font has for it — so the glyphs after one are moved
/// along here, and the line grows by what they moved.
///
/// Left to right, because where each tab lands depends on the ones before it. A
/// line that was *broken* with the tab at its font width was broken a little
/// early, which shows only where a paragraph both preserves tabs and wraps.
fn expand_tabs(shaped: &mut ShapedText, stop: f32) {
    if stop <= 0.0 {
        return;
    }

    for line in 0..shaped.lines.len() {
        for (run_index, glyph_index) in tabs_on(shaped, line) {
            let run = &shaped.runs[run_index];
            let at = run.glyphs[glyph_index].x;
            let after = run
                .glyphs
                .get(glyph_index + 1)
                .map_or(run.offset_x + run.advance, |glyph| glyph.x);
            // The next stop strictly past where the tab starts: a tab that lands
            // exactly on one still goes to the following one, which is what makes
            // a tab always take room.
            let target = ((at / stop).floor() + 1.0) * stop;
            let delta = target - after;
            if delta.abs() < 0.01 {
                continue;
            }
            shift_after(shaped, line, run_index, glyph_index, delta);
        }
    }
}

/// Where the tabs on one line are, as a run and a glyph in it, left to right.
///
/// Taken before anything moves and read back as things do: shifting a glyph does
/// not change which glyph it is, and each tab's own position is read again when
/// its turn comes.
///
/// Each glyph is asked which character it drew rather than counted against the
/// text: a tab is never part of a ligature, but anything ligated in front of one
/// on the same line would otherwise put the jump on the wrong glyph.
fn tabs_on(shaped: &ShapedText, line: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (run_index, run) in shaped.runs.iter().enumerate() {
        if run.line != line {
            continue;
        }
        for (glyph_index, glyph) in run.glyphs.iter().enumerate() {
            if run
                .text
                .get(glyph.text_offset as usize..)
                .is_some_and(|rest| rest.starts_with('\t'))
            {
                out.push((run_index, glyph_index));
            }
        }
    }
    out
}

/// Move everything after one glyph on a line along by `delta`.
fn shift_after(
    shaped: &mut ShapedText,
    line: usize,
    run_index: usize,
    glyph_index: usize,
    delta: f32,
) {
    let from = shaped.runs[run_index].offset_x;

    for (index, run) in shaped.runs.iter_mut().enumerate() {
        if run.line != line || index < run_index {
            continue;
        }
        if index == run_index {
            for glyph in run.glyphs.iter_mut().skip(glyph_index + 1) {
                glyph.x += delta;
            }
            run.advance += delta;
        } else {
            run.offset_x += delta;
            for glyph in &mut run.glyphs {
                glyph.x += delta;
            }
        }
    }

    for spacer in &mut shaped.spacers {
        if spacer.line == line && spacer.x >= from {
            spacer.x += delta;
        }
    }

    if let Some(metrics) = shaped.lines.get_mut(line) {
        metrics.width += delta;
        shaped.metrics.width = shaped.metrics.width.max(metrics.width);
    }
}

/// Pull runs, lines and metrics out of a broken parley layout.
///
/// `text` is what was shaped. A layout with nothing in it still comes back with
/// a line, and that line still carries a cluster — the one an empty paragraph needs
/// so a caret in it has a height — and that cluster is past the end of the text it
/// claims to be part of. It is dropped here rather than handed on as a glyph nobody
/// asked to draw.
fn collect(
    layout: &parley::Layout<SpanBrush>,
    text: &str,
    kinds: &HashMap<u64, SpacerKind>,
) -> ShapedText {
    let text_len = text.len();
    let mut runs = Vec::new();
    let mut lines = Vec::new();
    let mut spacers = Vec::new();
    // How far the ink of each line actually reached, which is not the line box: it
    // is only asked about where something in the line is not part of the box the
    // shaper measured. See the spacer pass below.
    let mut reached: Vec<(f32, f32)> = Vec::new();
    let mut first_baseline = 0.0;
    // Each font's own family name, read once per paragraph: what decides
    // whether its ascent is lengthened.
    let mut families: HashMap<(u64, u32), String> = HashMap::new();

    // How many of each run's clusters have already been handed out. One run is
    // split into several glyph runs where the style changes along it — with one
    // font and one size, a paragraph of differently coloured links is *one* run —
    // and the byte range of each slice is only recoverable by walking the
    // clusters alongside them, because the range parley reports is the whole
    // run's.
    //
    // Counted per line and reset with each one. A run's clusters are the clusters
    // of the line it is on, so carrying the count across a line break makes every
    // line after the first take its text from further along than it drew: the
    // second line of a paragraph came back holding the tail of its own text and
    // the third came back holding none, which is what a selection dragged down a
    // wrapped paragraph then copied.
    let mut consumed: Vec<usize> = Vec::new();

    for (index, line) in layout.lines().enumerate() {
        consumed.clear();
        let metrics = line.metrics();
        if index == 0 {
            first_baseline = metrics.baseline;
        }
        // The line *box*, not the ink. CSS stacks lines by the line box, and the
        // glyphs of a face whose ascent is taller than the leading allows hang out
        // of it — measuring the box from where the ink reached makes every line as
        // tall as its tallest letter and a page's rhythm a function of what is
        // written on it. The box is centred on the baseline the shaper placed the
        // glyphs on: half of what is left over after the font's own ascent and
        // descent goes above and half below, which is what half-leading is.
        let half_leading = (metrics.line_height - metrics.ascent - metrics.descent) / 2.0;
        let top = metrics.baseline - metrics.ascent - half_leading;
        reached.push((metrics.block_min_coord, metrics.block_max_coord));
        lines.push(LineMetrics {
            top,
            baseline: metrics.baseline,
            height: metrics.line_height,
            trailing_space: hanging_space(&line, text, kinds),
            bottom: top + metrics.line_height,
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
            let brush = style.brush.colour;
            let run = glyph_run.run();
            let metrics = run.metrics();

            let key = (run.font().data.id(), run.font().index);
            let family = families
                .entry(key)
                .or_insert_with(|| family_name(run.font()).unwrap_or_default());
            let decoration_metrics = DecorationMetrics {
                underline_offset: metrics.underline_offset,
                ascent: browser_ascent(family, metrics.ascent, metrics.descent),
            };

            let run_index = run.index();
            if consumed.len() <= run_index {
                consumed.resize(run_index + 1, 0);
            }
            let (text_range, starts) =
                consume_clusters(run, &mut consumed[run_index], glyph_run.advance());
            if text_range.start >= text_len {
                continue;
            }

            // Each glyph is told where in the run's text its cluster began. A
            // cluster is one glyph often enough that this looks like counting, but
            // where it is not — a ligature, a letter and its mark — it is the whole
            // difference between a selection that lands on a letter and one that
            // lands a letter along.
            let mut starts = starts.iter();
            let glyphs: Vec<Glyph> = glyph_run
                .positioned_glyphs()
                .map(|glyph| Glyph {
                    id: glyph.id,
                    x: glyph.x,
                    y: glyph.y,
                    text_offset: starts
                        .next()
                        .map_or(0, |start| start.saturating_sub(text_range.start) as u32),
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
                decoration_metrics,
                line: index,
                offset_x: glyph_run.offset(),
                advance: glyph_run.advance(),
                // The characters the run drew, kept with it: a run is what a
                // selection is measured in, and glyph identifiers cannot be read
                // back into the text they came from.
                text: text.get(text_range.clone()).unwrap_or_default().into(),
                text_range,
                glyphs,
            });
        }
    }

    // A picture in a line sits with its bottom edge on the baseline, and the text
    // beside it hangs below that: the line reaches further down than the box it is
    // stacked by, and a paragraph that is one such line is that much taller. That is
    // what `bottom` is for, and it is the one place ink beats the line box.
    // The shaper stacks a line holding a picture by the picture's own height and
    // puts the picture's bottom edge on the baseline, so the text's descenders hang
    // below the box it reports. On those lines — and only there — the ink is the
    // answer.
    for spacer in &spacers {
        if let (Some(line), Some(&(above, below))) =
            (lines.get_mut(spacer.line), reached.get(spacer.line))
        {
            // A picture is taller than the text beside it and sits on the baseline:
            // the box reaches up to hold it and down to hold the descenders, and
            // neither end is where the font alone would have put it.
            line.top = line.top.min(above).min(spacer.y);
            line.bottom = line
                .bottom
                .max(below)
                .max(spacer.y + spacer.height)
                .max(line.top + line.height);
        }
    }

    // The paragraph reaches from the top of its first line to the bottom of its
    // last, which is not the sum of the heights they are stacked by.
    let height = match (lines.first(), lines.last()) {
        (Some(first), Some(last)) => last.bottom - first.top,
        _ => layout.height(),
    };

    // The widest line without what hangs off its end. The shaper's own answer
    // leaves out every kind of white space a line ends in, no-break spaces too,
    // and a box sized to that is one no-break space too narrow for its text.
    let width = lines
        .iter()
        .map(|line| line.width - line.trailing_space)
        .fold(0.0, f32::max);

    ShapedText {
        metrics: TextMetrics {
            width,
            height,
            first_baseline,
            line_count: layout.len(),
        },
        runs,
        lines,
        spacers,
    }
}

/// How much of `line`'s advance is white space hanging off its end: the spaces
/// and tabs it ends in, and the line break if it ends in one (CSS Text 3
/// §4.1.3). The end edges of inline boxes after them are no reason to keep
/// them: a link that ends in a space ends its line on its last letter.
///
/// Worked out here rather than taken from the shaper, which counts every kind of
/// white space a line ends in. A no-break space does not hang — it is text that
/// happens to be blank, and `<td>&nbsp;</td>` is a cell a space wide — and nor
/// does any other space Unicode has that CSS does not collapse.
fn hanging_space(
    line: &parley::Line<'_, SpanBrush>,
    text: &str,
    kinds: &HashMap<u64, SpacerKind>,
) -> f32 {
    let items: Vec<_> = line.items().collect();
    let mut hanging = 0.0;
    for item in items.iter().rev() {
        match item {
            PositionedLayoutItem::InlineBox(placed) => {
                if kinds.get(&placed.id) == Some(&SpacerKind::Atomic) {
                    return hanging;
                }
            }
            PositionedLayoutItem::GlyphRun(glyph_run) => {
                let run = glyph_run.run();
                for index in (0..run.len()).rev() {
                    let Some(cluster) = run.get(index) else {
                        continue;
                    };
                    if Glyphs::of(text.get(cluster.text_range())) == Glyphs::Ink {
                        return hanging;
                    }
                    hanging += cluster.advance();
                }
                // The glyph runs of one run each hold all of its clusters.
                return hanging;
            }
        }
    }
    hanging
}

/// Take clusters from `run`, starting at `from`, until they add up to `advance`.
///
/// Reports the byte range they cover and, per glyph they drew, where in the text
/// that glyph's cluster began — one entry per glyph, in the order the glyphs come
/// back, so a cluster of two glyphs contributes its start twice and a cluster of
/// none contributes nothing.
///
/// Advances are floats and a slice of them will not sum exactly, so the comparison
/// carries a half-pixel of slack; overshooting by one cluster would attribute a
/// character to the wrong element, which for a link means the wrong destination.
fn consume_clusters(
    run: &parley::Run<'_, SpanBrush>,
    from: &mut usize,
    advance: f32,
) -> (std::ops::Range<usize>, Vec<usize>) {
    let mut taken = 0.0;
    let mut start = None;
    let mut end = *from;
    let mut starts = Vec::new();

    for cluster in run.clusters().skip(*from) {
        if taken > 0.0 && taken + cluster.advance() > advance + 0.5 {
            break;
        }
        let range = cluster.text_range();
        start.get_or_insert(range.start);
        end = range.end;
        starts.extend(cluster.glyphs().map(|_| range.start));
        taken += cluster.advance();
        *from += 1;
        if taken >= advance - 0.5 {
            break;
        }
    }

    (start.unwrap_or(end)..end, starts)
}

/// Which family each CSS generic resolves to, in preference order.
///
/// This is a *browser* preference and belongs to us: a generic keyword names a
/// role, and which face fills that role is a decision every browser makes for
/// itself and offers as a setting. The font library underneath has its own answers,
/// and it is not wrong — it is answering a different question, for programs that
/// are not browsers. Where the two differ, a page that asks for `monospace` gets a
/// different typeface here than everywhere else, which is a compatibility
/// difference rather than a matter of taste.
///
/// A name that no font on the machine answers to is skipped, and a generic with
/// nothing left is left as the library set it.
const PREFERRED_FAMILIES: &[(parley::GenericFamily, &[&str])] = &[
    #[cfg(target_os = "macos")]
    (parley::GenericFamily::Serif, &["Times", "Times New Roman"]),
    #[cfg(target_os = "macos")]
    (parley::GenericFamily::SansSerif, &["Helvetica", "Arial"]),
    #[cfg(target_os = "macos")]
    (
        parley::GenericFamily::Monospace,
        &["Menlo", "Monaco", "Courier"],
    ),
    #[cfg(target_os = "macos")]
    (parley::GenericFamily::Cursive, &["Apple Chancery"]),
    #[cfg(target_os = "macos")]
    (parley::GenericFamily::Fantasy, &["Papyrus"]),
    #[cfg(target_os = "windows")]
    (
        parley::GenericFamily::Serif,
        &["Times New Roman", "Georgia"],
    ),
    #[cfg(target_os = "windows")]
    (parley::GenericFamily::SansSerif, &["Arial", "Segoe UI"]),
    #[cfg(target_os = "windows")]
    (
        parley::GenericFamily::Monospace,
        &["Consolas", "Courier New"],
    ),
    #[cfg(target_os = "windows")]
    (parley::GenericFamily::Cursive, &["Comic Sans MS"]),
    #[cfg(target_os = "windows")]
    (parley::GenericFamily::Fantasy, &["Impact"]),
];

/// Point each generic at the family a browser would use for it.
fn prefer_browser_families(fonts: &mut FontContext) {
    for (generic, names) in PREFERRED_FAMILIES {
        let ids: Vec<_> = names
            .iter()
            .filter_map(|name| fonts.collection.family_id(name))
            .collect();
        if !ids.is_empty() {
            fonts
                .collection
                .set_generic_families(*generic, ids.into_iter());
        }
    }
}

/// The width, style and weight a face is registered with, from its rule.
///
/// They decide nothing about which face is chosen, which is
/// [`crate::web_fonts::matching_faces`]'s to say; they decide what is
/// synthesized once it has been. A face is emboldened, or slanted, when what a
/// run asks for is bolder, or more italic, than what its rule says it is — and
/// a variable face is set along its axes instead. A range is registered at its
/// point nearest normal, so a static face declared over a range is emboldened
/// above that point even inside the range, where CSS would take it as it is.
fn registered_attributes(
    descriptors: &FaceDescriptors,
) -> (parley::FontWidth, parley::FontStyle, parley::FontWeight) {
    let nearest = |range: &std::ops::RangeInclusive<f32>, normal: f32| {
        normal.clamp(*range.start(), *range.end())
    };
    let style = match descriptors.style {
        FaceStyle::Normal => parley::FontStyle::Normal,
        FaceStyle::Italic => parley::FontStyle::Italic,
        FaceStyle::Oblique(low, high) => parley::FontStyle::Oblique(Some(0.0_f32.clamp(low, high))),
    };
    (
        parley::FontWidth::from_percentage(nearest(&descriptors.stretch, 100.0)),
        style,
        parley::FontWeight::new(nearest(&descriptors.weight, 400.0)),
    )
}

/// An OpenType font, out of whatever container the page shipped it in.
///
/// What a `@font-face` on the web names is almost never a bare font: it is a WOFF
/// or a WOFF2, which is the same tables packed and compressed for the wire, and
/// every font library here reads the bare thing. The container says which it is in
/// its first four bytes; anything else is handed on untouched, because a font this
/// cannot unpack may still be a font the shaper can read.
fn unpack(bytes: Vec<u8>) -> Vec<u8> {
    let unpacked = match bytes.get(..4) {
        Some(b"wOF2") => wuff::decompress_woff2(&bytes),
        Some(b"wOFF") => wuff::decompress_woff1(&bytes),
        _ => return bytes,
    };

    match unpacked {
        Ok(font) => font,
        Err(error) => {
            tracing::warn!(%error, "a packed font could not be unpacked");
            bytes
        }
    }
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

    /// A run carries the characters it drew, and on the second line of a
    /// paragraph those are the second line's characters.
    ///
    /// The clusters a glyph run has handed out are counted per line: carrying the
    /// count across a break made every line after the first report text from
    /// further along than it drew, which is what a selection reads back and what
    /// a copy puts on the clipboard.
    #[test]
    fn every_line_of_a_paragraph_carries_its_own_text() {
        let mut engine = engine();
        let shaped = engine.shape("first line\nsecond line\nthird", &test_stack(), 16.0, None);

        let text_of = |line: usize| {
            shaped
                .runs
                .iter()
                .filter(|run| run.line == line)
                .map(|run| run.text.to_string())
                .collect::<String>()
        };

        assert_eq!(shaped.lines.len(), 3);
        assert_eq!(text_of(0), "first line");
        assert_eq!(text_of(1), "second line");
        assert_eq!(text_of(2), "third");

        // Every line drew something, and none of them drew more than the line
        // holds. (Not one glyph per character: a face that ligates draws `fi`
        // with one, which is why a run carries its text rather than being read
        // back out of its glyphs.)
        for run in &shaped.runs {
            assert!(
                !run.glyphs.is_empty() && run.glyphs.len() <= run.text.chars().count(),
                "a run of {:?} drew {} glyphs",
                run.text,
                run.glyphs.len()
            );
        }
    }

    /// A glyph says which characters it drew, and where a face ligates that is
    /// not where the glyph sits among the others.
    ///
    /// The offsets go up, never past the text, and every one of them is a
    /// character boundary — which is what the readers of a run slice on. Where a
    /// pair is drawn as one shape the count of glyphs is short of the count of
    /// characters and the offsets step by two.
    #[test]
    fn a_glyph_knows_which_characters_it_drew() {
        let mut engine = engine();
        let text = "difficult offices";
        let shaped = engine.shape(text, &test_stack(), 16.0, None);

        let mut ligated = false;
        for run in &shaped.runs {
            let mut previous = None;
            for glyph in &run.glyphs {
                let at = glyph.text_offset as usize;
                assert!(at < run.text.len(), "{at} is past {:?}", run.text);
                assert!(
                    run.text.is_char_boundary(at),
                    "{at} splits a character of {:?}",
                    run.text
                );
                if let Some(previous) = previous {
                    assert!(at >= previous, "offsets went backwards in {:?}", run.text);
                    ligated |= at > previous + 1;
                }
                previous = Some(at);
            }
            assert_eq!(run.glyphs.first().map(|glyph| glyph.text_offset), Some(0));
            ligated |= run.glyphs.len() < run.text.chars().count();
        }

        assert!(
            ligated,
            "the test face draws every pair separately, so this proves nothing"
        );
    }

    /// A tab is a jump to the next stop and not a character of its own width, so
    /// where it lands depends on how far along the line it sits.
    #[test]
    fn a_tab_reaches_the_next_stop() {
        let mut engine = engine();
        let stack = test_stack();
        let stop = engine
            .shape(" ", &stack, 16.0, None)
            .lines
            .first()
            .map_or(0.0, |line| line.width)
            * 8.0;
        assert!(stop > 0.0);

        let mut width = |text: &str| {
            engine
                .shape(text, &stack, 16.0, None)
                .lines
                .first()
                .map_or(0.0, |line| line.width)
        };

        let alone = width("\t");
        assert!(
            (alone - stop).abs() < 0.5,
            "a tab at the start of a line reaches the first stop: {alone} of {stop}"
        );

        let letter = width("a");
        let after = width("a\ta");
        assert!(
            (after - (stop + letter)).abs() < 0.5,
            "and one after a letter reaches the same stop: {after}"
        );

        // Past a stop, it goes to the one after — a tab always takes room.
        let long = "aaaaaaaaaa";
        let ten = width(long);
        let jumped = width(&format!("{long}\ta"));
        let expected = ((ten / stop).floor() + 1.0) * stop + letter;
        assert!(
            (jumped - expected).abs() < 0.5,
            "{jumped} rather than {expected}"
        );
    }

    /// A span asks for its weight and width along the axes of whatever
    /// variable face it lands in, ahead of the optical size and of what the
    /// page set itself, which override them.
    #[test]
    fn a_span_asks_for_its_weight_and_width_along_the_axes() {
        let settings = [(*b"wght", 300.0)];
        let span = TextSpan {
            font_weight: 700,
            font_width: 87.5,
            variations: &settings,
            ..TextSpan::new("a", test_stack(), 20.0)
        };
        let FontVariations::List(list) = variations(&span) else {
            panic!("a list");
        };
        let pairs: Vec<(parley::setting::Tag, f32)> = list
            .iter()
            .map(|setting| (setting.tag, setting.value))
            .collect();
        assert_eq!(
            pairs,
            [
                (WEIGHT, 700.0),
                (WIDTH, 87.5),
                (OPTICAL_SIZE, 20.0),
                (WEIGHT, 300.0)
            ]
        );
    }

    /// A page's own font arrives packed — a WOFF or a WOFF2, which is what the web
    /// ships — and the container is unpacked before the shaper is handed anything.
    /// A bare font goes through untouched, and a container that will not unpack is
    /// refused rather than fatal.
    #[test]
    fn a_font_is_unpacked_before_it_is_registered() {
        let mut engine = TextEngine::isolated();
        let plain = FaceDescriptors::default();
        assert!(
            engine.add_web_face("Bare Along", &plain, 0, TEST_FONT.to_vec()),
            "a font that is already a font registers"
        );
        assert!(engine.has_family("Bare Along"));

        // Packed, and not a font at all: unpacking fails, and what is handed on is
        // refused by the collection rather than taken as a face.
        let mut packed = b"wOF2".to_vec();
        packed.extend_from_slice(&[0u8; 64]);
        assert!(!engine.add_web_face("Broken Along", &plain, 0, packed));
        assert!(!engine.has_family("Broken Along"));
    }

    /// The font a one-span paragraph of `text` is set in, by its data's
    /// identity, for a run of `weight`.
    fn font_of(engine: &mut TextEngine, text: &str, family: &str, weight: u16) -> u64 {
        let span = TextSpan {
            font_weight: weight,
            ..TextSpan::new(text, FontStack::named(family), 16.0)
        };
        let shaped = engine.shape_spans(&[span], &[], None);
        let run = shaped.runs.first().expect("a run");
        assert!(
            shaped
                .runs
                .iter()
                .all(|other| other.font.data.id() == run.font.data.id()),
            "one font for all of {text:?}"
        );
        run.font.data.id()
    }

    /// A face declared for Cyrillic is not the font of a Latin letter, though
    /// its file has one: the letter falls through to the next family. A
    /// Cyrillic letter is set in it.
    #[test]
    fn unicode_range_decides_which_characters_a_face_sets() {
        let mut engine = engine();
        let cyrillic = FaceDescriptors {
            unicode_range: vec![0x400..=0x4FF],
            ..FaceDescriptors::default()
        };
        assert!(engine.add_web_face("Split", &cyrillic, 0, TEST_FONT.to_vec()));

        let fallback = font_of(&mut engine, "A", TEST_FAMILY, 400);
        let latin = font_of(&mut engine, "A", "Split", 400);
        let letter = font_of(&mut engine, "ж", "Split", 400);
        assert_eq!(latin, fallback, "'A' is outside the range");
        assert_ne!(letter, fallback, "'ж' is inside it");
    }

    /// One file declared as two weights is two faces, and a bold run is set
    /// in the one declared bold.
    #[test]
    fn a_rule_says_what_weight_its_face_is() {
        let mut engine = engine();
        for (order, weight) in [(0, 400.0), (1, 700.0)] {
            let face = FaceDescriptors {
                weight: weight..=weight,
                ..FaceDescriptors::default()
            };
            assert!(engine.add_web_face("Pair", &face, order, TEST_FONT.to_vec()));
        }
        let weights: Vec<f32> = engine
            .web
            .expand(&FontStack::named("Pair"), FaceQuery::default())
            .iter()
            .chain(&engine.web.expand(
                &FontStack::named("Pair"),
                FaceQuery {
                    weight: 700.0,
                    ..FaceQuery::default()
                },
            ))
            .filter_map(|entry| match entry {
                Resolved::Web(face) => Some(*face.descriptors.weight.start()),
                Resolved::Installed(_) | Resolved::Generic(_) => None,
            })
            .collect();
        assert_eq!(weights, [400.0, 700.0]);

        let regular = font_of(&mut engine, "a", "pair", 400);
        let bold = font_of(&mut engine, "a", "PAIR", 700);
        assert_ne!(
            regular, bold,
            "and the family is named without regard to case"
        );
        assert_eq!(font_of(&mut engine, "a", "Pair", 800), bold);
    }

    /// Of two faces with the same descriptors, the one declared later is
    /// tried first, whichever arrived first.
    #[test]
    fn the_last_declared_face_is_tried_first() {
        let mut engine = engine();
        let plain = FaceDescriptors::default();
        assert!(engine.add_web_face("Twice", &plain, 1, TEST_FONT.to_vec()));
        let later = font_of(&mut engine, "a", "Twice", 400);
        assert!(engine.add_web_face("Twice", &plain, 0, TEST_FONT.to_vec()));
        assert_eq!(font_of(&mut engine, "a", "Twice", 400), later);
        assert!(engine.add_web_face("Twice", &plain, 2, TEST_FONT.to_vec()));
        assert_ne!(font_of(&mut engine, "a", "Twice", 400), later);
    }

    /// A family's first available font is its first face that may set a
    /// space, which is where its strut comes from.
    #[test]
    fn the_strut_comes_from_the_face_that_covers_a_space() {
        let mut engine = engine();
        let cyrillic = FaceDescriptors {
            unicode_range: vec![0x400..=0x4FF],
            ..FaceDescriptors::default()
        };
        assert!(engine.add_web_face("Strut", &cyrillic, 1, TEST_FONT.to_vec()));
        assert!(engine.add_web_face("Strut", &FaceDescriptors::default(), 0, TEST_FONT.to_vec()));

        let (_, _, family) = engine
            .resolve(&FontStack::named("Strut"), FaceQuery::default())
            .expect("a font");
        let stack = FontStack::named("Strut");
        let names: Vec<&str> = engine
            .web
            .expand(&stack, FaceQuery::default())
            .into_iter()
            .filter_map(|entry| match entry {
                Resolved::Web(face) => Some(face.name.as_str()),
                Resolved::Installed(_) | Resolved::Generic(_) => None,
            })
            .collect();
        assert_eq!(
            family, names[1],
            "not the later face, which has no space: {names:?}"
        );
    }

    /// `local()` takes an installed family's face under the rule's name.
    #[test]
    fn a_local_source_is_an_installed_face() {
        let mut engine = engine();
        let plain = FaceDescriptors::default();
        assert!(engine.add_local_face(TEST_FAMILY, "Mine", &plain, 0));
        assert!(engine.has_family("Mine"));
        assert_eq!(
            font_of(&mut engine, "a", "Mine", 400),
            font_of(&mut engine, "a", TEST_FAMILY, 400),
            "the installed face's own data"
        );
        assert!(!engine.add_local_face("Not Installed Anywhere", "Theirs", &plain, 0));
        assert!(!engine.has_family("Theirs"));
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

    /// A line given its own width back an ulp short still fits, which is what a
    /// box sized to its max-content through f32 arithmetic can be given. A pixel
    /// short does not.
    #[test]
    fn a_line_an_ulp_too_wide_still_fits() {
        let mut engine = engine();
        let text = "the quick brown fox";
        let width = engine.shape(text, &test_stack(), 16.0, None).metrics.width;
        let short = f32::from_bits(width.to_bits() - 1);

        let fitted = engine.shape(text, &test_stack(), 16.0, Some(short));
        assert_eq!(fitted.metrics.line_count, 1);
        let narrower = engine.shape(text, &test_stack(), 16.0, Some(width - 1.0));
        assert!(narrower.metrics.line_count > 1);
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
            brush,
            ..TextSpan::new(text, test_stack(), size)
        }
    }

    /// `letter-spacing` goes after every character, the last one included — which
    /// is what CSS says, and what makes a spaced word wider by the spacing times
    /// its whole length rather than one less.
    #[test]
    fn letter_spacing_is_added_after_every_character() {
        let mut engine = engine();
        let plain = engine.shape_spans(&[TextSpan::new("abcdef", test_stack(), 16.0)], &[], None);
        let spaced = engine.shape_spans(
            &[TextSpan {
                letter_spacing: 2.0,
                ..TextSpan::new("abcdef", test_stack(), 16.0)
            }],
            &[],
            None,
        );

        assert!(
            (spaced.metrics.width - plain.metrics.width - 12.0).abs() < 0.01,
            "six characters at two pixels each: {} against {}",
            spaced.metrics.width,
            plain.metrics.width
        );
    }

    /// `word-spacing` goes at the spaces and nowhere else.
    #[test]
    fn word_spacing_is_added_at_every_space() {
        let mut engine = engine();
        let plain = engine.shape_spans(&[TextSpan::new("a b c", test_stack(), 16.0)], &[], None);
        let spaced = engine.shape_spans(
            &[TextSpan {
                word_spacing: 10.0,
                ..TextSpan::new("a b c", test_stack(), 16.0)
            }],
            &[],
            None,
        );

        assert!(
            (spaced.metrics.width - plain.metrics.width - 20.0).abs() < 0.01,
            "two spaces at ten pixels each: {} against {}",
            spaced.metrics.width,
            plain.metrics.width
        );
    }

    /// A font with no axes is not disturbed by being told about any: the vendored
    /// family is static, so every variation setting there is has to leave it exactly
    /// where it was.
    #[test]
    fn a_static_font_is_unmoved_by_variation_settings() {
        let mut engine = engine();
        let plain = engine
            .shape_spans(&[TextSpan::new("Otlyra", test_stack(), 32.0)], &[], None)
            .metrics
            .width;

        for span in [
            TextSpan {
                optical_sizing: false,
                ..TextSpan::new("Otlyra", test_stack(), 32.0)
            },
            TextSpan {
                variations: &[(*b"wght", 700.0), (*b"opsz", 8.0)],
                ..TextSpan::new("Otlyra", test_stack(), 32.0)
            },
            TextSpan {
                font_width: 62.5,
                ..TextSpan::new("Otlyra", test_stack(), 32.0)
            },
        ] {
            let width = engine.shape_spans(&[span], &[], None).metrics.width;
            assert!(
                (width - plain).abs() < 0.01,
                "a static font moved from {plain} to {width}"
            );
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
                    kind: SpacerKind::Opening,
                    at: 1,
                    width: 0.0,
                    height: 0.0,
                },
                Spacer {
                    id: 2,
                    kind: SpacerKind::Closing,
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

    /// A spacer sits with its bottom on the baseline, so the line reaches further
    /// down than the height the shaper stacks the next line by — and a paragraph
    /// that is one such line is that much taller than the sum of its line heights.
    #[test]
    fn a_paragraph_is_as_tall_as_it_reaches() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [span("before ", 16.0, brush), span(" after", 16.0, brush)];
        let plain = engine.shape_spans(&spans, &[], None);
        let with_box = engine.shape_spans(
            &spans,
            &[Spacer {
                id: 1,
                kind: SpacerKind::Atomic,
                at: 1,
                width: 32.0,
                height: 32.0,
            }],
            None,
        );

        let line = with_box.lines.first().expect("one line");
        assert!(
            line.bottom > line.top + line.height,
            "the line reaches below the height it is stacked by: {line:?}"
        );
        assert!(
            (with_box.metrics.height - (line.bottom - line.top)).abs() < 0.01,
            "and the paragraph is as tall as its one line reaches: {} against {}",
            with_box.metrics.height,
            line.bottom - line.top
        );
        assert!(with_box.metrics.height > 32.0, "which is past the picture");
        assert!(with_box.metrics.height > plain.metrics.height);
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
                kind: SpacerKind::Opening,
                at: 1,
                width: 20.0,
                height: 0.0,
            }],
            None,
        );

        assert!((spaced.metrics.width - plain.metrics.width - 20.0).abs() < 0.01);
    }

    /// A narrower line breaks earlier than a wide one in the same paragraph, which
    /// is the whole mechanism behind text flowing around something beside it.
    #[test]
    fn a_line_may_be_narrower_than_the_paragraph() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [span("alpha beta gamma delta epsilon", 16.0, brush)];

        let even = engine.shape_spans(&spans, &[], Some(200.0));
        // The first two lines are half as wide, as a float beside them would make
        // them; the rest of the paragraph gets the full width back.
        let stepped = engine.shape_spans_wrapping(&spans, &[], |index, _| {
            Some(if index < 2 { 100.0 } else { 200.0 })
        });

        assert!(stepped.lines.len() > even.lines.len());
        assert!(stepped.lines[0].width <= 100.0);
        assert!(
            stepped.lines.last().expect("a last line").width > 0.0,
            "the paragraph still finishes"
        );
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

    fn edge(id: u64, kind: SpacerKind, at: usize, width: f32) -> Spacer {
        Spacer {
            id,
            kind,
            at,
            width,
            height: 0.0,
        }
    }

    /// The text of each line, as the runs on it spell it.
    fn line_texts(shaped: &ShapedText) -> Vec<String> {
        (0..shaped.lines.len())
            .map(|line| {
                shaped
                    .runs
                    .iter()
                    .filter(|run| run.line == line)
                    .map(|run| run.text.as_ref())
                    .collect()
            })
            .collect()
    }

    /// A no-break space is text: it counts in the width of the line it ends,
    /// and a line as wide as it holds it.
    #[test]
    fn a_no_break_space_takes_room_and_does_not_hang() {
        let mut engine = engine();
        let one = engine.shape("\u{a0}", &test_stack(), 16.0, None);
        assert!(one.metrics.width > 0.0, "{:?}", one.metrics);
        assert_eq!(one.lines[0].trailing_space, 0.0);

        let two = engine.shape("\u{a0}\u{a0}", &test_stack(), 16.0, None);
        let fitted = engine.shape("\u{a0}\u{a0}", &test_stack(), 16.0, Some(two.metrics.width));
        assert_eq!(fitted.lines.len(), 1, "{:?}", fitted.lines);
        assert!((two.metrics.width - 2.0 * one.metrics.width).abs() < 0.01);
    }

    /// A no-break space is no place to break a line (UAX #14 class GL), however
    /// narrow the line: the words either side of it are one.
    #[test]
    fn a_line_never_breaks_at_a_no_break_space() {
        let mut engine = engine();
        let joined = engine.shape("10\u{a0}km away", &test_stack(), 16.0, Some(0.0));
        assert_eq!(line_texts(&joined), ["10\u{a0}km ", "away"]);
        let whole = engine.shape("10\u{a0}km", &test_stack(), 16.0, None);
        assert!(
            (joined.lines[0].width - joined.lines[0].trailing_space - whole.metrics.width).abs()
                < 0.01
        );
    }

    /// The edges of an inline box are not a place to break: `<a>About</a>Us` is
    /// one word, and the edge goes with the text it is the edge of.
    #[test]
    fn a_line_never_breaks_at_an_inline_box_edge() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [span("About", 16.0, brush), span("Us", 16.0, brush)];
        let edges = [
            edge(0, SpacerKind::Opening, 0, 5.0),
            edge(1, SpacerKind::Closing, 1, 0.0),
        ];
        let narrowest = engine.shape_spans(&spans, &edges, Some(0.0));
        assert_eq!(line_texts(&narrowest), ["AboutUs"]);
        let widest = engine.shape_spans(&spans, &edges, None);
        assert!((narrowest.lines[0].width - widest.metrics.width).abs() < 0.01);
    }

    /// A word too long for its line that starts inside a box with a margin
    /// overflows with the margin in front of it, rather than leaving the margin
    /// on a line of its own; and the next line starts at the next word, not at
    /// the space before it.
    #[test]
    fn an_edge_at_the_start_of_a_line_stays_with_its_word() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [
            span("Averyveryverylongword", 16.0, brush),
            span(" tail", 16.0, brush),
        ];
        let edges = [
            edge(0, SpacerKind::Opening, 0, 6.0),
            edge(1, SpacerKind::Closing, 1, 0.0),
        ];
        let shaped = engine.shape_spans(&spans, &edges, Some(60.0));
        assert_eq!(line_texts(&shaped), ["Averyveryverylongword ", "tail"]);
        let start = shaped.spacers.iter().find(|spacer| spacer.id == 0);
        assert_eq!(start.map(|spacer| (spacer.line, spacer.x)), Some((0, 0.0)));
    }

    /// A line still breaks either side of an atomic inline, as it does either
    /// side of the object replacement character it stands for.
    #[test]
    fn a_line_breaks_either_side_of_an_atomic_inline() {
        let mut engine = engine();
        let brush = [0, 0, 0, 255];
        let spans = [span("ab", 16.0, brush), span("cd", 16.0, brush)];
        let picture = Spacer {
            height: 10.0,
            ..edge(0, SpacerKind::Atomic, 1, 10.0)
        };
        let shaped = engine.shape_spans(&spans, &[picture], Some(0.0));
        assert_eq!(line_texts(&shaped), ["ab", "", "cd"]);
    }

    #[test]
    fn empty_text_shapes_to_nothing() {
        let mut engine = engine();
        let shaped = engine.shape("", &test_stack(), 32.0, None);
        assert_eq!(shaped.glyph_count(), 0);
        assert_eq!(shaped.width(), 0.0);
    }
}
