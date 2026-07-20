//! The marks the interface draws, as paths.
//!
//! Not glyphs. A glyph is at the mercy of whichever font the system hands back,
//! and a missing one is a hollow box where a button should be; these are the
//! same on every machine and scale to any size without a second asset. Each
//! function draws into a square and centres itself in whatever rectangle it is
//! given, so a mark can be dropped into a control of any size.

use otlyra_gfx::kurbo::{Affine, Arc, BezPath, Cap, Circle, Join, Point, Shape, Stroke};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};

use crate::widget::Rect;

/// Which way an arrow points.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Back, or previous.
    Left,
    /// Forward, or next.
    Right,
    /// Collapsed upward.
    Up,
    /// Expanded downward.
    Down,
}

/// The centre of `rect`, and the reach a mark of `scale` gets from it.
///
/// Every mark is built from these two numbers, so they all end up optically the
/// same weight in a row without anyone tuning them against each other.
fn centre(rect: Rect, scale: f64) -> (Point, f64) {
    (
        Point::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0),
        rect.width.min(rect.height) * scale,
    )
}

/// A stroked path, with round ends and joins.
fn stroke(list: &mut DisplayList, path: BezPath, color: Color, width: f64) {
    list.push(DisplayItem::Stroke {
        style: Stroke::new(width)
            .with_caps(Cap::Round)
            .with_join(Join::Round),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(color),
        brush_transform: None,
        shape: path,
    });
}

/// A filled path.
fn fill(list: &mut DisplayList, path: BezPath, color: Color) {
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(color),
        brush_transform: None,
        shape: path,
    });
}

/// A chevron: back, forward, or the one on a collapsed row.
pub fn chevron(list: &mut DisplayList, rect: Rect, direction: Direction, color: Color) {
    let (centre, reach) = centre(rect, 0.22);
    let mut path = BezPath::new();
    match direction {
        Direction::Left | Direction::Right => {
            let tip = if direction == Direction::Left {
                -reach
            } else {
                reach
            };
            path.move_to(Point::new(centre.x - tip * 0.7, centre.y - reach));
            path.line_to(Point::new(centre.x + tip * 0.7, centre.y));
            path.line_to(Point::new(centre.x - tip * 0.7, centre.y + reach));
        }
        Direction::Up | Direction::Down => {
            let tip = if direction == Direction::Up {
                -reach
            } else {
                reach
            };
            path.move_to(Point::new(centre.x - reach, centre.y - tip * 0.7));
            path.line_to(Point::new(centre.x, centre.y + tip * 0.7));
            path.line_to(Point::new(centre.x + reach, centre.y - tip * 0.7));
        }
    }
    stroke(list, path, color, 1.7);
}

/// A circular arrow: reload when still, and the load itself when turning.
///
/// `phase` is where the arc starts, in radians. While a page is loading the
/// caller advances it every frame and shortens the sweep, so the gap reads as
/// motion — the only thing on screen that says the browser is busy rather than
/// stuck.
pub fn reload(list: &mut DisplayList, rect: Rect, phase: Option<f32>, color: Color) {
    let (centre, radius) = centre(rect, 0.3);

    let (start, sweep) = match phase {
        Some(phase) => (f64::from(phase), 4.2),
        None => (-0.9, 5.2),
    };
    stroke(
        list,
        Arc::new(centre, (radius, radius), start, sweep, 0.0).to_path(0.05),
        color,
        1.6,
    );

    // The head sits on the arc's end, pointing along it, computed from the same
    // angle the arc ends at so the two cannot drift apart when either changes.
    let end = start + sweep;
    let tip = Point::new(centre.x + radius * end.cos(), centre.y + radius * end.sin());
    let along = Point::new(-end.sin(), end.cos());
    let across = Point::new(end.cos(), end.sin());
    let size = radius * 0.45;
    let mut head = BezPath::new();
    head.move_to(Point::new(tip.x + along.x * size, tip.y + along.y * size));
    head.line_to(Point::new(
        tip.x - along.x * size * 0.4 + across.x * size,
        tip.y - along.y * size * 0.4 + across.y * size,
    ));
    head.line_to(Point::new(
        tip.x - along.x * size * 0.4 - across.x * size,
        tip.y - along.y * size * 0.4 - across.y * size,
    ));
    head.close_path();
    fill(list, head, color);
}

/// A plus: open a tab, add a row.
pub fn plus(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.25);
    let mut path = BezPath::new();
    path.move_to(Point::new(centre.x - reach, centre.y));
    path.line_to(Point::new(centre.x + reach, centre.y));
    path.move_to(Point::new(centre.x, centre.y - reach));
    path.line_to(Point::new(centre.x, centre.y + reach));
    stroke(list, path, color, 1.6);
}

/// A cross: close a tab, clear a field, dismiss a message.
pub fn cross(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.22);
    let mut path = BezPath::new();
    path.move_to(Point::new(centre.x - reach, centre.y - reach));
    path.line_to(Point::new(centre.x + reach, centre.y + reach));
    path.move_to(Point::new(centre.x + reach, centre.y - reach));
    path.line_to(Point::new(centre.x - reach, centre.y + reach));
    stroke(list, path, color, 1.5);
}

/// A tick: a checkbox that is on, a setting that took.
pub fn check(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.28);
    let mut path = BezPath::new();
    path.move_to(Point::new(centre.x - reach, centre.y));
    path.line_to(Point::new(centre.x - reach * 0.25, centre.y + reach * 0.65));
    path.line_to(Point::new(centre.x + reach, centre.y - reach * 0.6));
    stroke(list, path, color, 2.0);
}

/// A padlock, closed: the address is on a transport that was authenticated.
///
/// Drawn shut only. There is no open padlock, because an open padlock is a claim
/// about a page that a neutral absence of a mark makes more honestly.
pub fn lock(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.3);
    let body_width = reach * 1.5;
    let body_height = reach * 1.2;
    let body = Rect::new(
        centre.x - body_width / 2.0,
        centre.y - body_height / 2.0 + reach * 0.35,
        body_width,
        body_height,
    );
    fill(
        list,
        otlyra_gfx::kurbo::RoundedRect::from_rect(body.to_kurbo(), reach * 0.25).to_path(0.05),
        color,
    );

    // The shackle is a half circle sitting on the body, drawn at the same reach
    // so it cannot end up wider than what it locks.
    let shackle = reach * 0.5;
    stroke(
        list,
        Arc::new(
            Point::new(centre.x, body.y),
            (shackle, shackle),
            std::f64::consts::PI,
            std::f64::consts::PI,
            0.0,
        )
        .to_path(0.05),
        color,
        1.4,
    );
}

/// A page: the address is on a transport with nothing to say about it, or is a
/// local file.
pub fn page(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.3);
    let mut path = BezPath::new();
    let (left, right) = (centre.x - reach * 0.7, centre.x + reach * 0.7);
    let (top, bottom) = (centre.y - reach, centre.y + reach);
    let fold = reach * 0.55;
    path.move_to(Point::new(left, top));
    path.line_to(Point::new(right - fold, top));
    path.line_to(Point::new(right, top + fold));
    path.line_to(Point::new(right, bottom));
    path.line_to(Point::new(left, bottom));
    path.close_path();
    stroke(list, path, color, 1.3);
}

/// A filled dot, used where a tab has no icon of its own yet.
pub fn dot(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, radius) = centre(rect, 0.2);
    fill(list, Circle::new(centre, radius).to_path(0.05), color);
}

/// A cogwheel: the browser's own settings.
///
/// Teeth computed rather than placed by hand, so the shape stays even at any
/// size, and the hole punched with the even-odd rule in the same path — filling
/// the surface's colour over the middle would need to know what is behind, and
/// over a menu row that changes with the pointer.
pub fn gear(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.34);
    let teeth = 8;
    let (root, tip) = (reach * 0.74, reach);
    let step = std::f64::consts::TAU / teeth as f64;

    let point = |radius: f64, angle: f64| {
        Point::new(
            centre.x + radius * angle.cos(),
            centre.y + radius * angle.sin(),
        )
    };

    let mut path = BezPath::new();
    for tooth in 0..teeth {
        let base = step * tooth as f64;
        // Narrow at the tip and wider at the root, which is what makes a cog
        // read as a cog rather than as a star.
        for (radius, angle) in [
            (root, base - step * 0.28),
            (tip, base - step * 0.13),
            (tip, base + step * 0.13),
            (root, base + step * 0.28),
        ] {
            let at = point(radius, angle);
            if tooth == 0 && radius == root && angle < base {
                path.move_to(at);
            } else {
                path.line_to(at);
            }
        }
    }
    path.close_path();

    // The hole, as a second subpath the even-odd rule turns into a hole.
    Circle::new(centre, reach * 0.34)
        .to_path(0.05)
        .elements()
        .iter()
        .for_each(|element| path.push(*element));

    list.push(DisplayItem::Fill {
        style: Fill::EvenOdd,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(color),
        brush_transform: None,
        shape: path,
    });
}

/// A clock face: what was visited, and when.
pub fn clock(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, radius) = centre(rect, 0.36);
    stroke(list, Circle::new(centre, radius).to_path(0.05), color, 1.4);
    let mut hands = BezPath::new();
    hands.move_to(Point::new(centre.x, centre.y - radius * 0.55));
    hands.line_to(centre);
    hands.line_to(Point::new(centre.x + radius * 0.45, centre.y));
    stroke(list, hands, color, 1.4);
}

/// A star: a page kept on purpose.
pub fn star(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.42);
    let mut path = BezPath::new();
    for point in 0..10 {
        let radius = if point % 2 == 0 { reach } else { reach * 0.45 };
        // Starting a quarter turn back, so the star stands on two legs rather
        // than balancing on a point.
        let angle = std::f64::consts::TAU * point as f64 / 10.0 - std::f64::consts::FRAC_PI_2;
        let at = Point::new(
            centre.x + radius * angle.cos(),
            centre.y + radius * angle.sin(),
        );
        if point == 0 {
            path.move_to(at);
        } else {
            path.line_to(at);
        }
    }
    path.close_path();
    fill(list, path, color);
}

/// An arrow into a tray: what was fetched to disk.
pub fn download(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, reach) = centre(rect, 0.36);
    let mut arrow = BezPath::new();
    arrow.move_to(Point::new(centre.x, centre.y - reach));
    arrow.line_to(Point::new(centre.x, centre.y + reach * 0.35));
    arrow.move_to(Point::new(centre.x - reach * 0.5, centre.y - reach * 0.15));
    arrow.line_to(Point::new(centre.x, centre.y + reach * 0.35));
    arrow.line_to(Point::new(centre.x + reach * 0.5, centre.y - reach * 0.15));
    stroke(list, arrow, color, 1.5);

    let mut tray = BezPath::new();
    tray.move_to(Point::new(centre.x - reach, centre.y + reach * 0.7));
    tray.line_to(Point::new(centre.x + reach, centre.y + reach * 0.7));
    stroke(list, tray, color, 1.5);
}

/// A letter *i* in a circle: what this program is.
pub fn info(list: &mut DisplayList, rect: Rect, color: Color) {
    let (centre, radius) = centre(rect, 0.36);
    stroke(list, Circle::new(centre, radius).to_path(0.05), color, 1.4);
    fill(
        list,
        Circle::new(Point::new(centre.x, centre.y - radius * 0.45), 1.1).to_path(0.05),
        color,
    );
    let mut stem = BezPath::new();
    stem.move_to(Point::new(centre.x, centre.y - radius * 0.1));
    stem.line_to(Point::new(centre.x, centre.y + radius * 0.5));
    stroke(list, stem, color, 1.5);
}
