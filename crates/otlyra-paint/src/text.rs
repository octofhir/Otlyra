//! A run of text as it is drawn: its shadows, the decoration lines in effect
//! on it, and its glyphs, in the order css-text-decor-3 §8 paints them.
//!
//! Where a line sits and how it is drawn follow Blink, since the specification
//! leaves both to the user agent: `text-decoration-thickness: auto` is a tenth
//! of the font size; an underline sits where the font puts it, an overline on
//! the text's top edge and a line-through a third of the ascent above the
//! baseline; the second line of a double is a line's width and a pixel away.
//!
//! What is not done: the lines are placed and sized by each run's own font
//! rather than by the font of the box that declared the decoration, an
//! underline does not skip the descenders it crosses, and a shadow does not
//! draw the decoration lines, only the glyphs. `text-decoration-thickness` and
//! `text-underline-offset` are Gecko-only in the cascade, so the automatic
//! values stand.

use otlyra_css::{DecorationStyle, TextDecoration};
use otlyra_gfx::kurbo::{Affine, BezPath, Cap, Rect as KurboRect, Shape, Stroke};
use otlyra_gfx::peniko::{BlendMode, Brush, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::fragment::{Fragment, Rect};
use otlyra_text::ShapedRun;

use crate::{PATH_TOLERANCE, brush_to_color};

/// Which line a decoration draws.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Line {
    Under,
    Over,
    Through,
}

impl Line {
    /// Whether `decoration` draws this line.
    fn drawn_by(self, decoration: &TextDecoration) -> bool {
        match self {
            Self::Under => decoration.lines.underline,
            Self::Over => decoration.lines.overline,
            Self::Through => decoration.lines.line_through,
        }
    }

    /// The top edge of a line `t` thick, for a run whose baseline is at
    /// `baseline`.
    fn top(self, run: &ShapedRun, baseline: f32, t: f32) -> f32 {
        let metrics = run.decoration_metrics;
        match self {
            // Measured from the text's top edge, a whole number of pixels
            // above the baseline, and rounded there, as Blink places it.
            Self::Under => {
                baseline - metrics.ascent.round()
                    + (metrics.ascent - metrics.underline_offset).round()
            }
            Self::Over => baseline - metrics.ascent - t.floor(),
            Self::Through => baseline - metrics.ascent / 3.0 - t / 2.0,
        }
    }

    /// Which way a second line, of a double or under a wave, is moved: away
    /// from the text for an overline, down for the others.
    fn away(self) -> f32 {
        match self {
            Self::Over => -1.0,
            Self::Under | Self::Through => 1.0,
        }
    }
}

/// Paint one run: its text shadows, then the underlines and overlines of every
/// decoration in effect on it, then its glyphs, then the lines through it —
/// so an underline passes under the letters and a line-through over them.
pub(crate) fn paint_text_run(
    list: &mut DisplayList,
    fragment: &Fragment,
    run: &ShapedRun,
    rect: Rect,
    origin: Affine,
    scroll_y: f32,
) {
    // The text's own shadows, behind it: the same glyphs, moved and softened.
    // A shadow has no spread — there is nothing to grow but the letters
    // themselves.
    for shadow in &fragment.style.text_shadows {
        if shadow.color.components[3] <= 0.0 {
            continue;
        }
        list.push_glyph_run(
            &run.font,
            run.font_size,
            run.normalized_coords.clone(),
            Brush::Solid(shadow.color),
            origin * Affine::translate((f64::from(shadow.x), f64::from(shadow.y))),
            true,
            f64::from(shadow.blur),
            run.glyphs.clone(),
        );
    }

    let Some(first) = run.glyphs.first() else {
        return;
    };
    let baseline = rect.y - scroll_y + first.y;
    let thickness = (run.font_size / 10.0).max(1.0);
    let span = (rect.x, rect.x + run.advance);
    let decorate = |list: &mut DisplayList, line: Line| {
        for decoration in fragment.style.decorations.iter() {
            if line.drawn_by(decoration) {
                let top = line.top(run, baseline, thickness);
                paint_line(list, decoration, line, span, top, thickness);
            }
        }
    };

    decorate(list, Line::Under);
    decorate(list, Line::Over);
    list.push_glyphs(
        &run.font,
        run.font_size,
        run.normalized_coords.clone(),
        Brush::Solid(brush_to_color(run.brush)),
        origin,
        true,
        run.glyphs.clone(),
    );
    decorate(list, Line::Through);
}

/// One decoration line from `x0` to `x1`, its top edge at `top` and `t` thick,
/// in the decoration's style and colour (css-text-decor-3 §2.2–§2.4).
fn paint_line(
    list: &mut DisplayList,
    decoration: &TextDecoration,
    line: Line,
    (x0, x1): (f32, f32),
    top: f32,
    t: f32,
) {
    if x1 <= x0 {
        return;
    }
    let brush = Brush::Solid(decoration.color);
    let second = top + line.away() * (t + 1.0);
    match decoration.style {
        DecorationStyle::Solid => fill_line(list, &brush, (x0, x1), top, t),
        DecorationStyle::Double => {
            fill_line(list, &brush, (x0, x1), top, t);
            let second = match line {
                Line::Through => top + (t + 1.0).floor(),
                Line::Under | Line::Over => second,
            };
            fill_line(list, &brush, (x0, x1), second, t);
        }
        DecorationStyle::Dotted | DecorationStyle::Dashed => {
            stroke_line(list, &brush, decoration.style, (x0, x1), top, t);
        }
        DecorationStyle::Wavy => {
            // A wave's middle is a line's width and a pixel from the text
            // side of a straight line, where it swings clear of the letters;
            // one through the text runs along the line itself.
            let middle = match line {
                Line::Through => top + t / 2.0,
                Line::Under | Line::Over => second,
            };
            wave(list, &brush, (x0, x1), middle, t);
        }
    }
}

/// A solid line, snapped to whole pixels down its height as Blink draws it so
/// it is crisp rather than smeared over two rows.
fn fill_line(list: &mut DisplayList, brush: &Brush, (x0, x1): (f32, f32), top: f32, t: f32) {
    let top = f64::from((top + 0.5).floor());
    let height = f64::from(t.floor().max(1.0));
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: brush.clone(),
        brush_transform: None,
        shape: KurboRect::new(f64::from(x0), top, f64::from(x1), top + height)
            .to_path(PATH_TOLERANCE),
    });
}

/// A dotted or dashed line, stroked along its middle.
///
/// A dash is three widths long with a gap of two, or two and one from three
/// pixels thick, and the gap is stretched so whole dashes fill the line. A
/// dotted line thinner than that is square dots a width apart; a thicker one
/// is round dots, drawn inside its ends.
fn stroke_line(
    list: &mut DisplayList,
    brush: &Brush,
    style: DecorationStyle,
    (x0, x1): (f32, f32),
    top: f32,
    t: f32,
) {
    let width = t.round().max(1.0);
    let mut middle = (top + (t / 2.0).max(0.5)).floor();
    // An odd width is centred on a pixel's middle, so it covers whole rows.
    if width % 2.0 == 1.0 {
        middle += 0.5;
    }
    let length = x1 - x0;
    let square = style == DecorationStyle::Dashed || width <= 3.0;
    let (dash, gap, cap, inset) = if square {
        let (dash, gap) = match style {
            DecorationStyle::Dashed if width >= 3.0 => (2.0 * width, width),
            DecorationStyle::Dashed => (3.0 * width, 2.0 * width),
            _ => (width, width),
        };
        if length <= 2.0 * dash {
            return;
        }
        (dash, best_gap(length, dash, gap), Cap::Butt, 0.0)
    } else {
        let gap = best_gap(length, width, width);
        (0.0, gap + width - 0.01, Cap::Round, width / 2.0)
    };

    let mut path = BezPath::new();
    path.move_to((f64::from(x0 + inset), f64::from(middle)));
    path.line_to((f64::from(x1 - inset), f64::from(middle)));
    list.push(DisplayItem::Stroke {
        style: Stroke::new(f64::from(width))
            .with_dashes(0.0, [f64::from(dash), f64::from(gap)])
            .with_caps(cap),
        transform: Affine::IDENTITY,
        brush: brush.clone(),
        brush_transform: None,
        shape: path,
    });
}

/// The gap that lets whole dashes fill `length` with gaps nearest `gap`.
fn best_gap(length: f32, dash: f32, gap: f32) -> f32 {
    let fewer = ((length + gap) / (dash + gap)).floor();
    let more = fewer + 1.0;
    let gap_for = |dashes: f32| (length - dashes * dash) / (dashes - 1.0);
    let (fewer_gap, more_gap) = (gap_for(fewer), gap_for(more));
    if more_gap <= 0.0 || (fewer_gap - gap).abs() < (more_gap - gap).abs() {
        fewer_gap
    } else {
        more_gap
    }
}

/// A wave along `middle` from `x0` to `x1`, for a line `t` thick: one cubic
/// curve per wavelength, starting a wavelength before the line and cut off at
/// both of its ends, so every wave is whole where it shows.
fn wave(list: &mut DisplayList, brush: &Brush, (x0, x1): (f32, f32), middle: f32, t: f32) {
    let t = f64::from(t.max(1.0));
    let wavelength = 1.0 + 2.0 * (2.0 * t + 0.5).round();
    let reach = 0.5 + (3.0 * t + 0.5).round();
    let (x0, x1, middle) = (f64::from(x0), f64::from(x1), f64::from(middle));

    let mut path = BezPath::new();
    let mut x = x0 - wavelength;
    path.move_to((x, middle));
    while x < x1 {
        path.curve_to(
            (x + wavelength / 2.0, middle + reach),
            (x + wavelength / 2.0, middle - reach),
            (x + wavelength, middle),
        );
        x += wavelength;
    }

    // The band the wave swings through, as far as the curves reach, with the
    // stroke's width on either side.
    let half = reach * 0.75 + t;
    list.push(DisplayItem::PushLayer {
        blend: BlendMode::default(),
        alpha: 1.0,
        transform: Affine::IDENTITY,
        clip: KurboRect::new(x0, (middle - half).floor(), x1, (middle + half).ceil())
            .to_path(PATH_TOLERANCE),
    });
    list.push(DisplayItem::Stroke {
        style: Stroke::new(t),
        transform: Affine::IDENTITY,
        brush: brush.clone(),
        brush_transform: None,
        shape: path,
    });
    list.push(DisplayItem::PopLayer);
}
