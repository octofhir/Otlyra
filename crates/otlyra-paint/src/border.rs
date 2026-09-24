//! A box's border: four sides, each drawn in the line its style asks for.
//!
//! Sides meet on the diagonal between them, a carved style is two shades of one
//! colour, and dashes are measured so that a side ends the way it began. None of
//! that is about the box the border happens to go round — an outline is drawn in
//! the same lines — so it is kept apart from the walk that decides where the box is.

use otlyra_gfx::kurbo::Affine;
use otlyra_gfx::peniko::color::Rgba8;
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::fragment::Fragment;

use crate::shape::shape_with_radii;

/// The four borders of a box, each as a filled rectangle on the inside edge of the
/// border box.
///
/// Rectangles rather than a stroked outline, because each side has its own width
/// and colour and a stroke has one of each. The corners are square: mitring them
/// needs the four trapezia CSS specifies, and the difference only shows where two
/// adjacent sides differ in colour and are thick enough to see.
pub(super) fn paint_borders(
    list: &mut DisplayList,
    fragment: &Fragment,
    rect: otlyra_layout::Rect,
    scroll_y: f32,
) {
    let border = fragment.style.border;
    let (left, top) = (f64::from(rect.x), f64::from(rect.y - scroll_y));
    let (right, bottom) = (f64::from(rect.right()), f64::from(rect.bottom() - scroll_y));

    // A rounded box's border follows its corners, which four rectangles cannot do.
    // One stroke can, as long as every side is the same — and a border that is
    // rounded and different on each side is a shape CSS defines and nobody writes.
    // Only for a plain line: a dashed or three-dimensional rounded border falls
    // through to the four sides below, which draws the right line in the wrong
    // corners rather than the wrong line in the right ones.
    let uniform =
        border.top == border.right && border.right == border.bottom && border.bottom == border.left;
    if fragment.style.radius.any() && uniform && border.top.style == otlyra_css::BorderStyle::Solid
    {
        let side = border.top;
        if !side.is_visible() {
            return;
        }
        let width = f64::from(side.width);
        // Strokes straddle the path, so the path is inset by half the width to put
        // the whole of it inside the box — where CSS draws it.
        let inset = otlyra_layout::Rect::new(
            rect.x + side.width / 2.0,
            rect.y + side.width / 2.0,
            (rect.width - side.width).max(0.0),
            (rect.height - side.width).max(0.0),
        );
        list.push(DisplayItem::Stroke {
            style: otlyra_gfx::kurbo::Stroke::new(width),
            transform: Affine::IDENTITY,
            brush: Brush::Solid(side.color),
            brush_transform: None,
            // The radius belongs to the box's own edge, and the stroke runs half a
            // border in from it: drawn at the radius the page wrote, the outer
            // corner comes out half a border too round and the inner one half a
            // border too tight. A square corner stays square, which is what keeps
            // one rounded corner from rounding the other three.
            shape: shape_with_radii(inset, scroll_y, &fragment.style, -side.width / 2.0),
        });
        return;
    }

    // Each side is a trapezoid, not a rectangle: two borders meet at a corner on
    // the diagonal between them, and drawn as rectangles the second one covers the
    // first — which shows the moment two sides are different colours.
    let quads = [
        (
            border.top,
            Side::Top,
            [
                (left, top),
                (right, top),
                (
                    right - f64::from(border.right.width),
                    top + f64::from(border.top.width),
                ),
                (
                    left + f64::from(border.left.width),
                    top + f64::from(border.top.width),
                ),
            ],
        ),
        (
            border.right,
            Side::Right,
            [
                (right, top),
                (right, bottom),
                (
                    right - f64::from(border.right.width),
                    bottom - f64::from(border.bottom.width),
                ),
                (
                    right - f64::from(border.right.width),
                    top + f64::from(border.top.width),
                ),
            ],
        ),
        (
            border.bottom,
            Side::Bottom,
            [
                (right, bottom),
                (left, bottom),
                (
                    left + f64::from(border.left.width),
                    bottom - f64::from(border.bottom.width),
                ),
                (
                    right - f64::from(border.right.width),
                    bottom - f64::from(border.bottom.width),
                ),
            ],
        ),
        (
            border.left,
            Side::Left,
            [
                (left, bottom),
                (left, top),
                (
                    left + f64::from(border.left.width),
                    top + f64::from(border.top.width),
                ),
                (
                    left + f64::from(border.left.width),
                    bottom - f64::from(border.bottom.width),
                ),
            ],
        ),
    ];

    for (side, which, corners) in quads {
        if !side.is_visible() {
            continue;
        }
        paint_border_side(list, side, which, corners);
    }
}

/// Which edge of the box a border is on, which is what decides whether a
/// three-dimensional style draws its shadow or its light there.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    /// Whether this is one of the two edges the light is taken to come from.
    fn is_near(self) -> bool {
        matches!(self, Self::Top | Self::Left)
    }
}

/// One side of a border, drawn in whatever line its style asks for.
///
/// `corners` is the side's trapezoid: the two outer corners first, then the two
/// inner ones, going back the way it came.
fn paint_border_side(
    list: &mut DisplayList,
    border: otlyra_css::Border,
    side: Side,
    corners: [(f64, f64); 4],
) {
    use otlyra_css::BorderStyle;

    let fill = |list: &mut DisplayList, colour: Color, quad: [(f64, f64); 4]| {
        let mut path = otlyra_gfx::kurbo::BezPath::new();
        path.move_to(quad[0]);
        for point in &quad[1..] {
            path.line_to(*point);
        }
        path.close_path();
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(colour),
            brush_transform: None,
            shape: path,
        });
    };

    // A trapezoid narrowed towards its inner edge: `from` and `to` are how far
    // across the border's own width the slice runs, as fractions.
    let slice = |from: f64, to: f64| {
        let across = |outer: (f64, f64), inner: (f64, f64), at: f64| {
            (
                outer.0 + (inner.0 - outer.0) * at,
                outer.1 + (inner.1 - outer.1) * at,
            )
        };
        [
            across(corners[0], corners[3], from),
            across(corners[1], corners[2], from),
            across(corners[1], corners[2], to),
            across(corners[0], corners[3], to),
        ]
    };

    match border.style {
        BorderStyle::Double => {
            // Two lines a third of the width each and the rest between them, which
            // is what a reference draws to the pixel — rounded rather than
            // truncated, so an eight-pixel border is three, two, three.
            let line = (f64::from(border.width) / 3.0).round().max(1.0);
            let across = f64::from(border.width).max(1.0);
            fill(list, border.color, slice(0.0, line / across));
            fill(list, border.color, slice(1.0 - line / across, 1.0));
        }
        BorderStyle::Groove | BorderStyle::Ridge => {
            // Half the width carved and half raised. `groove` is dark on the near
            // side of the outer half and on the far side of the inner one, which
            // is what makes the line read as a channel rather than a step.
            let outer_dark = (border.style == BorderStyle::Groove) == side.is_near();
            let (dark, light) = shades(border.color);
            fill(list, if outer_dark { dark } else { light }, slice(0.0, 0.5));
            fill(list, if outer_dark { light } else { dark }, slice(0.5, 1.0));
        }
        BorderStyle::Inset | BorderStyle::Outset => {
            // The whole box pressed in or standing out: one pair of sides dark and
            // the other light, all the way across.
            let is_dark = (border.style == BorderStyle::Inset) == side.is_near();
            let (dark, light) = shades(border.color);
            fill(list, if is_dark { dark } else { light }, slice(0.0, 1.0));
        }
        BorderStyle::Dashed | BorderStyle::Dotted => {
            paint_dashes(list, border, corners);
        }
        _ => fill(list, border.color, slice(0.0, 1.0)),
    }
}

/// A run of dashes or dots along the middle of one side.
///
/// Along the side's own centre line rather than inside its trapezoid: a dash that
/// reaches a corner is drawn to the outer corner, which is where a reference puts
/// it, and the corner is then covered by whichever side is drawn after it.
///
/// The lengths are measured against a reference: a dash is twice the border's
/// width and the gap is one, no dash shorter than three pixels and no gap shorter
/// than two — and then the gap is stretched so that a whole number of dashes ends
/// the side exactly as it began it. A dot is a round cap on a zero-length dash,
/// which is a circle the width of the border every two widths.
fn paint_dashes(list: &mut DisplayList, border: otlyra_css::Border, corners: [(f64, f64); 4]) {
    use otlyra_css::BorderStyle;
    use otlyra_gfx::kurbo::{BezPath, Cap, Stroke};

    let width = f64::from(border.width);
    // The centre line: half way between the outer edge and the inner one, run out
    // to the outer corners at both ends.
    let mid = |outer: (f64, f64), inner: (f64, f64)| {
        (
            outer.0 + (inner.0 - outer.0) / 2.0,
            outer.1 + (inner.1 - outer.1) / 2.0,
        )
    };
    let (start, end) = (mid(corners[0], corners[3]), mid(corners[1], corners[2]));
    let length = ((end.0 - start.0).powi(2) + (end.1 - start.1).powi(2)).sqrt();
    if length <= 0.0 {
        return;
    }

    let dotted = border.style == BorderStyle::Dotted;
    let (dash, gap) = if dotted {
        (0.0, width * 2.0)
    } else {
        let dash = (width * 2.0).max(3.0);
        let gap = width.max(2.0);
        // n dashes and n - 1 gaps fill the side, so the gap takes up whatever the
        // dashes leave. One dash and no gaps where nothing else fits.
        let count = (((length + gap) / (dash + gap)).round() as i64).max(1);
        let spare = length - dash * count as f64;
        let gaps = (count - 1).max(1) as f64;
        (dash, (spare / gaps).max(gap))
    };

    let mut path = BezPath::new();
    path.move_to(start);
    path.line_to(end);
    list.push(DisplayItem::Stroke {
        style: Stroke::new(width)
            .with_dashes(0.0, [dash, gap])
            .with_caps(if dotted { Cap::Round } else { Cap::Butt }),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(border.color),
        brush_transform: None,
        shape: path,
    });
}

/// The two shades a three-dimensional border is drawn in: the side in shadow,
/// then the lit side.
///
/// CSS leaves the shading to the user agent (css-backgrounds-3, "Line Patterns:
/// the `border-style` properties"), so these are Chrome's, to the bit
/// (`CalculateInsetOutsetColor` in its border painter). The shadow side is the
/// colour darkened and the lit side the colour lightened, with an exception at
/// each end of the scale, which is measured in relative luminance:
///
/// - a colour brighter than [`LIGHT_END`] has nowhere lighter to go, and its lit
///   side is the colour itself;
/// - a colour no brighter than [`DARK_END`] would darken to black, so it is
///   lightened once for the shadow side and twice for the lit one — which is why
///   a `<table border>` in black text is framed in two greys.
///
/// Firefox shades by a curve of its own and lands a few levels away from these.
pub(super) fn shades(colour: Color) -> (Color, Color) {
    let colour = colour.to_rgba8();
    let luminance = relative_luminance(colour);
    let (dark, light) = if luminance <= relative_luminance(DARK_END) {
        let dark = lighten(colour);
        (dark, lighten(dark))
    } else if luminance > relative_luminance(LIGHT_END) {
        (darken(colour), colour)
    } else {
        (darken(colour), lighten(colour))
    };
    (Color::from(dark), Color::from(light))
}

/// The lightest colour [`shades`] treats as too dark to darken.
const DARK_END: Rgba8 = Rgba8 {
    r: 0x20,
    g: 0x20,
    b: 0x20,
    a: 0xff,
};

/// The lightest colour [`shades`] still lightens.
const LIGHT_END: Rgba8 = Rgba8 {
    r: 0xeb,
    g: 0xeb,
    b: 0xeb,
    a: 0xff,
};

/// How far [`lighten`] and [`darken`] move a colour's brightest channel, as a
/// fraction of the whole scale.
const STEP: f32 = 0.33;

/// A colour's relative luminance, as WCAG 2 defines it: the sRGB channels
/// linearised and weighted by how bright each looks. Alpha plays no part.
fn relative_luminance(colour: Rgba8) -> f32 {
    let linear = |channel: u8| {
        let channel = f32::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(colour.r) + 0.7152 * linear(colour.g) + 0.0722 * linear(colour.b)
}

/// The colour with its brightest channel raised by [`STEP`], short of white, and
/// the others in proportion. Black has no proportion to keep and becomes the grey
/// that far up.
fn lighten(colour: Rgba8) -> Rgba8 {
    let peak = brightest(colour);
    if peak == 0.0 {
        let grey = (STEP * 255.0) as u8;
        return Rgba8 {
            r: grey,
            g: grey,
            b: grey,
            a: colour.a,
        };
    }
    scale(colour, (peak + STEP).min(1.0) / peak)
}

/// The colour with its brightest channel lowered by [`STEP`], no further than
/// black, and the others in proportion.
fn darken(colour: Rgba8) -> Rgba8 {
    let peak = brightest(colour);
    let factor = if peak == 0.0 {
        0.0
    } else {
        ((peak - STEP) / peak).max(0.0)
    };
    scale(colour, factor)
}

/// A colour's brightest channel, from nought to one.
fn brightest(colour: Rgba8) -> f32 {
    f32::from(colour.r.max(colour.g).max(colour.b)) / 255.0
}

/// Every colour channel times `factor`, alpha untouched.
///
/// Truncated rather than rounded, against a scale a hair under 256 — so a whole
/// channel stays whole and anything short of a level drops to the one below, as
/// the reference's arithmetic does. Rounding instead is a level out on half the
/// colours there are.
fn scale(colour: Rgba8, factor: f32) -> Rgba8 {
    let channel = |value: u8| (f32::from(value) / 255.0 * factor * 256.0_f32.next_down()) as u8;
    Rgba8 {
        r: channel(colour.r),
        g: channel(colour.g),
        b: channel(colour.b),
        a: colour.a,
    }
}
