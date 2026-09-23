//! A box's border: four sides, each drawn in the line its style asks for.
//!
//! Sides meet on the diagonal between them, a carved style is two shades of one
//! colour, and dashes are measured so that a side ends the way it began. None of
//! that is about the box the border happens to go round — an outline is drawn in
//! the same lines — so it is kept apart from the walk that decides where the box is.

use otlyra_gfx::kurbo::Affine;
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

/// The two shades a three-dimensional border is drawn in.
///
/// A reference darkens the colour towards the shadow side and leaves the other
/// side the colour it was told — unless the colour is already dark enough that
/// darkening it makes black, where it lightens the lit side instead, so that the
/// two halves can still be told apart. Both curves are the reference's own and
/// were read off a ramp of colours rather than guessed.
pub(super) fn shades(colour: Color) -> (Color, Color) {
    let [r, g, b, a] = colour.components;
    let peak = r.max(g).max(b);
    // A third of the way up is where a darkened colour reaches black, and below it
    // there is nothing left to take away.
    const FLOOR: f32 = 0.33;

    let scaled = |factor: f32| Color::new([r * factor, g * factor, b * factor, a]);
    let dark = scaled(if peak > 0.0 {
        ((peak - FLOOR) / peak).max(0.0)
    } else {
        0.0
    });
    if peak > FLOOR {
        return (dark, colour);
    }
    // Nothing to darken: the lit side is lightened instead, and a colour with no
    // light in it at all is lightened from black to the grey a reference uses.
    let light = if peak > 0.0 {
        scaled((peak + FLOOR).min(1.0) / peak)
    } else {
        Color::new([FLOOR, FLOOR, FLOOR, a])
    };
    (dark, light)
}
