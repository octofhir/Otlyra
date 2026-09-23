//! A CSS gradient as a brush: the line it runs along and the stops on it.
//!
//! A gradient is a picture with no file behind it, and wherever a page can name a
//! picture it can name one of these — which is why it is kept apart from the
//! background that happens to be the only place it is drawn so far.

use otlyra_layout::fragment::Rect;

/// The gradient a box's background is painted with, as a line across that box.
///
/// CSS gives the angle clockwise from pointing up, and the line is as long as the
/// box needs for the gradient to cover its corners — which is what makes a diagonal
/// gradient reach the ones it points at rather than stopping short of them.
pub(super) fn gradient_brush(
    gradient: &otlyra_css::Gradient,
    rect: Rect,
    scroll_y: f32,
) -> otlyra_gfx::peniko::Gradient {
    use otlyra_gfx::kurbo::Point;

    let (width, height) = (f64::from(rect.width), f64::from(rect.height));
    let centre = Point::new(
        f64::from(rect.x) + width / 2.0,
        f64::from(rect.y - scroll_y) + height / 2.0,
    );

    // Up is negative y on the screen and zero degrees in CSS, and the angle turns
    // clockwise; the length is the specification's own, the projection of the box
    // onto the line.
    let angle = f64::from(gradient.angle);
    let (sin, cos) = angle.sin_cos();
    let length = (width * sin.abs() + height * cos.abs()) / 2.0;
    let along = Point::new(sin * length, -cos * length);

    let mut brush = otlyra_gfx::peniko::Gradient::new_linear(
        Point::new(centre.x - along.x, centre.y - along.y),
        Point::new(centre.x + along.x, centre.y + along.y),
    );
    for stop in &gradient.stops {
        brush.stops.push(otlyra_gfx::peniko::ColorStop {
            offset: stop.at,
            color: stop.color.into(),
        });
    }
    brush
}
