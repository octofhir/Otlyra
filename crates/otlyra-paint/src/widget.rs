//! What a control looks like.
//!
//! No control here is a widget of the operating system. Both references gave that
//! up: a checkbox is a rounded square with a tick stroked across it, a radio
//! button is two circles, a drop-down's arrow is a triangle, and all of it is
//! drawn by the engine with the same primitives a page is drawn with. Ours go into
//! the same display list as everything else, which is what makes them scale,
//! transform, clip and composite without a word being said about it.
//!
//! The numbers are the references' own, read off their painting code and checked
//! against pictures: a two-pixel corner radius on a box and a field, a one-pixel
//! edge, a tick that starts a fifth of the way across and is stroked at a sixth of
//! the height, a dot inset by a fifth, an arrow twice as wide as it is tall. Where
//! the two disagree — and they mostly do not — the one the pictures are compared
//! against wins.
//!
//! A control the page has restyled never reaches here: the box tree decides that,
//! because the decision is the cascade's and not the painter's.

use otlyra_gfx::kurbo::{Affine, BezPath, RoundedRect, Shape as _};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::box_tree::{Control, ControlKind, ControlState};
use otlyra_layout::fragment::Rect;

/// Flattening tolerance for the shapes a widget draws.
const TOLERANCE: f64 = 0.1;

/// The corner a box, a field and a button are drawn with.
const RADIUS: f64 = 2.0;
/// The line around a box, a field and a button.
const EDGE: f64 = 1.0;
/// How far a checkbox's tick and a radio button's dot sit inside the shape.
const INSET: f64 = 0.2;
/// How thick the tick is drawn, as a fraction of the box.
const TICK: f64 = 0.16;
/// How much wider a drop-down's arrow is than it is tall.
const ARROW_ASPECT: f64 = 2.0;
/// The strip a drop-down's arrow is drawn in, which layout also leaves for it.
const ARROW_STRIP: f64 = 20.0;

/// The unfilled face of a box, a field and a list.
const FIELD: Color = Color::from_rgba8(0xff, 0xff, 0xff, 0xff);
/// The face of a button.
const BUTTON: Color = Color::from_rgba8(0xef, 0xef, 0xef, 0xff);
/// The line around everything.
const BORDER: Color = Color::from_rgba8(0x76, 0x76, 0x76, 0xff);
/// What a ticked box is filled with, and what a focus ring is drawn in.
const ACCENT: Color = Color::from_rgba8(0x00, 0x75, 0xff, 0xff);
/// What is drawn on the accent: the tick itself.
const ON_ACCENT: Color = Color::from_rgba8(0xff, 0xff, 0xff, 0xff);
/// The face of a button under the pointer, and of one held down.
const BUTTON_HOVER: Color = Color::from_rgba8(0xe5, 0xe5, 0xe5, 0xff);
/// A button held down.
const BUTTON_ACTIVE: Color = Color::from_rgba8(0xd5, 0xd5, 0xd5, 0xff);
/// The track of a slider and the trough of a bar.
const TRACK: Color = Color::from_rgba8(0xd9, 0xd9, 0xd9, 0xff);
/// The face of a control nothing can do anything with.
///
/// A colour rather than a transparency, because a white field faded against a
/// white page is a white field: what says "disabled" is that it is *greyer* than
/// the page, not that it is fainter. The reference gets the same answer the other
/// way round — three tenths of the button's own grey over whatever is behind it —
/// and this is that composite against a white page, which is what a page is.
const DISABLED_FACE: Color = Color::from_rgba8(0xfa, 0xfa, 0xfa, 0xff);

/// How far a focus ring sits outside the shape it rings.
const RING_OFFSET: f64 = 0.0;
/// How thick a focus ring is.
const RING_WIDTH: f64 = 2.0;

/// Draw `control` over `rect`, in page coordinates already shifted for the scroll.
///
/// Returns whether the widget drew its own face — in which case the box's CSS
/// background and border are not drawn, because the widget *is* them. That is what
/// both references do, and it is why a control that keeps its widget ignores the
/// user-agent border it also computes.
pub(crate) fn paint(list: &mut DisplayList, control: &Control, rect: Rect) -> bool {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return false;
    }
    let state = control.state;
    match control.kind {
        ControlKind::Checkbox => {
            check_box(list, square(rect), state);
            true
        }
        ControlKind::Radio => {
            radio(list, square(rect), state);
            true
        }
        ControlKind::Button | ControlKind::Color | ControlKind::File => {
            button(list, rect, state);
            true
        }
        ControlKind::Field | ControlKind::Area => {
            field(list, rect, state);
            true
        }
        ControlKind::DropDown => {
            field(list, rect, state);
            arrow(list, rect, state);
            true
        }
        ControlKind::ListBox => {
            field(list, rect, state);
            true
        }
        ControlKind::Range => {
            slider(list, rect, state);
            true
        }
        ControlKind::Progress | ControlKind::Meter => {
            bar(list, rect, state);
            true
        }
    }
}

/// The largest square inside a rectangle, centred.
///
/// Both references do this and say so: a checkbox told to be forty pixels wide and
/// thirteen tall is a thirteen-pixel checkbox in the middle of it, not an ellipse.
fn square(rect: Rect) -> Rect {
    let side = rect.width.min(rect.height);
    Rect::new(
        rect.x + (rect.width - side) / 2.0,
        rect.y + (rect.height - side) / 2.0,
        side,
        side,
    )
}

/// A checkbox: a rounded square, filled with the accent when it is ticked, and the
/// tick stroked over it.
fn check_box(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let shape = rounded(rect, RADIUS);
    let marked = state.checked || state.indeterminate;

    if marked {
        fill(list, &shape, dim(ACCENT, state));
    } else {
        fill(list, &shape, dim(face(FIELD, state), state));
        stroke(
            list,
            &inset(rect, EDGE / 2.0),
            RADIUS,
            EDGE,
            dim(BORDER, state),
        );
    }

    if state.indeterminate {
        // A dash rather than a tick: eight by two on a thirteen-pixel box, which
        // is the reference's own shape scaled to whatever size the box is.
        let width = f64::from(rect.width);
        let height = f64::from(rect.height);
        let dash = Rect::new(
            rect.x + (width * 2.5 / 13.0) as f32,
            rect.y + (height * 5.5 / 13.0) as f32,
            (width * 8.0 / 13.0) as f32,
            (height * 2.0 / 13.0) as f32,
        );
        fill(list, &rounded(dash, 1.0), dim(ON_ACCENT, state));
    } else if state.checked {
        fill_path(
            list,
            tick(rect),
            dim(ON_ACCENT, state),
            f64::from(rect.height) * TICK,
        );
    }

    ring(list, &shape, state);
}

/// The tick, as a stroked path: in from the left at the middle, down and to the
/// right, then up to a fifth from the top on the right.
fn tick(rect: Rect) -> BezPath {
    let (x, y) = (f64::from(rect.x), f64::from(rect.y));
    let (w, h) = (f64::from(rect.width), f64::from(rect.height));
    let mut path = BezPath::new();
    path.move_to((x + w * INSET, y + h / 2.0));
    path.line_to((x + w * INSET + w * INSET, y + h / 2.0 + h * INSET));
    path.line_to((x + w - w * INSET, y + h * INSET));
    path
}

/// A radio button: a circle, with a smaller one inside it when it is selected.
fn radio(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let radius = f64::from(rect.width) / 2.0;
    let shape = rounded(rect, radius);

    if state.checked {
        fill(list, &shape, dim(ACCENT, state));
        let dot = inset(rect, f64::from(rect.width) * INSET);
        fill(
            list,
            &rounded(dot, f64::from(dot.width) / 2.0),
            dim(ON_ACCENT, state),
        );
    } else {
        fill(list, &shape, dim(face(FIELD, state), state));
        stroke(
            list,
            &inset(rect, EDGE / 2.0),
            radius,
            EDGE,
            dim(BORDER, state),
        );
    }

    ring(list, &shape, state);
}

/// A field, a text area, a list and the body of a drop-down: a face and an edge.
fn field(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let inner = inset(rect, EDGE / 2.0);
    let face = if state.disabled { DISABLED_FACE } else { FIELD };
    fill(list, &rounded(rect, RADIUS), face);
    stroke(list, &inner, RADIUS, EDGE, dim(BORDER, state));
    ring(list, &rounded(rect, RADIUS), state);
}

/// A button: the same, in the button's own colours, and darker while it is held.
fn button(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let face = if state.disabled {
        DISABLED_FACE
    } else if state.active && state.hovered {
        BUTTON_ACTIVE
    } else if state.hovered {
        BUTTON_HOVER
    } else {
        BUTTON
    };
    let inner = inset(rect, EDGE / 2.0);
    fill(list, &rounded(rect, RADIUS), face);
    stroke(list, &inner, RADIUS, EDGE, dim(BORDER, state));
    ring(list, &rounded(rect, RADIUS), state);
}

/// The triangle a drop-down shows on its inline end.
fn arrow(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let strip = ARROW_STRIP.min(f64::from(rect.width));
    let width = (strip / 2.0).min(f64::from(rect.width));
    let height = width / ARROW_ASPECT;
    let centre_x = f64::from(rect.x + rect.width) - strip / 2.0;
    let centre_y = f64::from(rect.y + rect.height / 2.0);

    let mut path = BezPath::new();
    path.move_to((centre_x - width / 2.0, centre_y - height / 2.0));
    path.line_to((centre_x + width / 2.0, centre_y - height / 2.0));
    path.line_to((centre_x, centre_y + height / 2.0));
    path.close_path();
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(dim(BORDER, state)),
        brush_transform: None,
        shape: path,
    });
}

/// A slider: a track across the middle and a round thumb on it.
fn slider(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let thickness = 4.0_f32.min(rect.height);
    let track = Rect::new(
        rect.x,
        rect.y + (rect.height - thickness) / 2.0,
        rect.width,
        thickness,
    );
    fill(
        list,
        &rounded(track, f64::from(thickness) / 2.0),
        dim(TRACK, state),
    );

    let side = rect.height.min(rect.width);
    let thumb = Rect::new(
        rect.x + (rect.width - side) / 2.0,
        rect.y + (rect.height - side) / 2.0,
        side,
        side,
    );
    fill(
        list,
        &rounded(thumb, f64::from(side) / 2.0),
        dim(ACCENT, state),
    );
    ring(list, &rounded(rect, RADIUS), state);
}

/// A progress or meter bar: a trough with a filled part in it.
///
/// How full it is needs the element's numbers, which a box does not carry yet, so
/// it is drawn empty rather than drawn wrong.
fn bar(list: &mut DisplayList, rect: Rect, state: ControlState) {
    let radius = f64::from(rect.height) / 2.0;
    fill(list, &rounded(rect, radius), dim(TRACK, state));
}

/// The ring that shows where the keyboard is.
///
/// Outside the shape rather than on it, so that it is visible against a control of
/// any colour, and drawn only when the focus is to be *shown* — which is a
/// different question from whether the control is focused. See `:focus-visible`.
fn ring(list: &mut DisplayList, shape: &RoundedRect, state: ControlState) {
    if !state.focus_ring {
        return;
    }
    let rect = shape.rect().inflate(
        RING_OFFSET + RING_WIDTH / 2.0,
        RING_OFFSET + RING_WIDTH / 2.0,
    );
    let radii = shape.radii().top_left + RING_WIDTH / 2.0;
    list.push(DisplayItem::Stroke {
        style: otlyra_gfx::kurbo::Stroke::new(RING_WIDTH),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(ACCENT),
        brush_transform: None,
        shape: RoundedRect::from_rect(rect, radii).to_path(TOLERANCE),
    });
}

/// The face a control shows while the pointer is over it.
fn face(colour: Color, state: ControlState) -> Color {
    if state.hovered && !state.disabled {
        blend(colour, BUTTON_HOVER, 0.35)
    } else {
        colour
    }
}

/// A disabled control is the same control, faded into the page behind it.
///
/// Both references do this by alpha rather than by a second palette — one at
/// three tenths and one at a half — which is what keeps a disabled control
/// recognisable as the control it is.
fn dim(colour: Color, state: ControlState) -> Color {
    if state.disabled {
        colour.multiply_alpha(0.3)
    } else {
        colour
    }
}

/// Mix two colours.
fn blend(a: Color, b: Color, amount: f32) -> Color {
    let mix = |x: f32, y: f32| x + (y - x) * amount;
    Color::new([
        mix(a.components[0], b.components[0]),
        mix(a.components[1], b.components[1]),
        mix(a.components[2], b.components[2]),
        a.components[3],
    ])
}

/// A rectangle with the same corner all round.
fn rounded(rect: Rect, radius: f64) -> RoundedRect {
    RoundedRect::new(
        f64::from(rect.x),
        f64::from(rect.y),
        f64::from(rect.x + rect.width),
        f64::from(rect.y + rect.height),
        radius.min(f64::from(rect.width.min(rect.height)) / 2.0),
    )
}

/// A rectangle pulled in on all four sides.
fn inset(rect: Rect, by: f64) -> Rect {
    let by = by as f32;
    Rect::new(
        rect.x + by,
        rect.y + by,
        (rect.width - by * 2.0).max(0.0),
        (rect.height - by * 2.0).max(0.0),
    )
}

fn fill(list: &mut DisplayList, shape: &RoundedRect, colour: Color) {
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(colour),
        brush_transform: None,
        shape: shape.to_path(TOLERANCE),
    });
}

fn fill_path(list: &mut DisplayList, path: BezPath, colour: Color, width: f64) {
    list.push(DisplayItem::Stroke {
        style: otlyra_gfx::kurbo::Stroke::new(width.max(1.0)),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(colour),
        brush_transform: None,
        shape: path,
    });
}

fn stroke(list: &mut DisplayList, rect: &Rect, radius: f64, width: f64, colour: Color) {
    list.push(DisplayItem::Stroke {
        style: otlyra_gfx::kurbo::Stroke::new(width),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(colour),
        brush_transform: None,
        shape: rounded(*rect, radius).to_path(TOLERANCE),
    });
}
