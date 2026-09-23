//! The scrollbar: where its thumb is, and the thumb itself.
//!
//! The browser's rather than the page's, so it is drawn over everything the page
//! drew and left out of a picture meant for comparison. Its own module because the
//! one piece of geometry here decides both where the thumb is drawn and what a
//! press landed on, and the two are only the same if they are read together.

use otlyra_gfx::kurbo::{Affine, Shape};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_layout::fragment::Rect;

use crate::PATH_TOLERANCE;

/// How wide a scrollbar is drawn, in logical pixels.
const SCROLLBAR_WIDTH: f32 = 8.0;

/// How far a scrollbar sits from the edge it runs along.
const SCROLLBAR_INSET: f32 = 2.0;

/// The shortest a scrollbar's thumb is drawn, so a very long page still has
/// something to see and to aim at.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// The scrollbar's thumb.
pub(super) const SCROLLBAR_THUMB: Color = Color::from_rgba8(0, 0, 0, 0x59);

/// Where a scrollbar's thumb is, for an area showing `content_height` scrolled to
/// `scroll` — or `None` when the content fits and there is no scrollbar.
///
/// One function, used to draw it and to decide what a press landed on: a scrollbar
/// that is drawn in one place and grabbed in another is the same bug as a link that
/// is clickable somewhere else.
pub fn scrollbar_thumb(area: Rect, content_height: f32, scroll: f32) -> Option<Rect> {
    let range = content_height - area.height;
    if range <= 0.5 || area.height <= 0.0 {
        return None;
    }

    let visible = (area.height / content_height).clamp(0.0, 1.0);
    let thumb = (area.height * visible).max(SCROLLBAR_MIN_THUMB.min(area.height));
    let travel = area.height - thumb;
    let at = area.y + travel * (scroll / range).clamp(0.0, 1.0);
    Some(Rect::new(
        area.right() - SCROLLBAR_WIDTH - SCROLLBAR_INSET,
        at,
        SCROLLBAR_WIDTH,
        thumb,
    ))
}

/// How far a scrollbar's thumb travels: the pixels of thumb movement that stand for
/// the whole of the content.
pub fn scrollbar_travel(area: Rect, content_height: f32) -> f32 {
    match scrollbar_thumb(area, content_height, 0.0) {
        Some(thumb) => (area.height - thumb.height).max(0.0),
        None => 0.0,
    }
}

/// Draw a scrollbar down the right edge of `area`.
///
/// Nothing is drawn for content that fits: a scrollbar that says the page cannot
/// move is noise. The thumb's length is the fraction of the content on screen and
/// its position is how far through the content that fraction is, which is the whole
/// of what a scrollbar says.
pub(super) fn paint_scrollbar(
    list: &mut DisplayList,
    area: Rect,
    content_height: f32,
    scroll: f32,
) {
    let Some(thumb) = scrollbar_thumb(area, content_height, scroll) else {
        return;
    };

    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(SCROLLBAR_THUMB),
        brush_transform: None,
        shape: otlyra_gfx::kurbo::RoundedRect::new(
            f64::from(thumb.x),
            f64::from(thumb.y),
            f64::from(thumb.right()),
            f64::from(thumb.bottom()),
            f64::from(SCROLLBAR_WIDTH / 2.0),
        )
        .to_path(PATH_TOLERANCE),
    });
}
