//! The geometry of a widget, where more than one part of the browser has to
//! agree on it.
//!
//! Layout leaves a strip for a drop-down's arrow that paint then draws in, and
//! paint puts a slider's thumb where the pointer then has to find it again. A
//! number written down twice is two numbers that one day disagree — a thumb drawn
//! in one place and dragged from another — so each is written here once, with
//! the one mapping between a slider's value and where its thumb is.
//!
//! The sizes are the references' own defaults, in CSS pixels: what a control is
//! when nothing has said otherwise, and not taken from the font, so a checkbox in
//! a heading is the same checkbox. Each is a content box; a control whose style
//! says `box-sizing: border-box` has its edges added on top, which is what makes
//! the two come out the same size.

use crate::fragment::Rect;

/// The side of a checkbox and of a radio button.
///
/// Both references agree within a pixel.
pub const CHECK_SIDE: f32 = 13.0;

/// How wide a slider is when nothing says otherwise.
pub const RANGE_WIDTH: f32 = 129.0;
/// How tall a slider is when nothing says otherwise.
pub const RANGE_HEIGHT: f32 = 16.0;

/// How wide a colour well is when nothing says otherwise.
pub const COLOR_WIDTH: f32 = 44.0;
/// How tall a colour well is when nothing says otherwise.
pub const COLOR_HEIGHT: f32 = 23.0;

/// The strip a drop-down leaves on its inline end for the arrow, and the strip
/// the arrow is drawn in.
///
/// Room rather than a width: a drop-down is as wide as the option it shows, and
/// a width would stop a long option from making it wider. Both references
/// reserve the same twenty pixels give or take two, and both give it back when
/// the page turns the widget off — which is the one visible thing
/// `appearance: none` does to a `<select>`.
pub const ARROW_STRIP: f32 = 20.0;

/// How wide a slider's thumb is, and how tall where the slider has the room.
pub const THUMB: f32 = 14.0;

/// The side of the thumb a slider occupying `rect` is drawn with.
///
/// Round, so as wide as it is tall, and never larger than the slider: one told
/// to be eight pixels tall has an eight-pixel thumb rather than one that spills
/// out of it.
pub fn thumb_side(rect: Rect) -> f32 {
    THUMB.min(rect.height).min(rect.width)
}

/// Where the middle of the thumb is, across the page, when a slider occupying
/// `rect` is `position` of the way along — zero at its minimum and one at its
/// maximum.
///
/// The thumb travels between the two ends rather than off them: at the minimum
/// its left edge is the slider's, at the maximum its right edge is. So the
/// filled part of the track reaches the middle of the thumb, and never past the
/// end of the track.
pub fn thumb_centre(rect: Rect, position: f32) -> f32 {
    let side = thumb_side(rect);
    let travel = (rect.width - side).max(0.0);
    rect.x + side / 2.0 + travel * position.clamp(0.0, 1.0)
}

/// How far along a slider occupying `rect` its thumb would be with its middle
/// at `x`: the inverse of [`thumb_centre`], clamped to the slider's ends.
///
/// Taken from where the *middle* of the thumb would have to be, which is what
/// makes a press at the very left edge give the minimum rather than something a
/// little above it. A slider no wider than its thumb has no travel to divide
/// by; a pixel of it stands in, which makes a press either side of the middle
/// the end it is nearer to.
pub fn position_at(rect: Rect, x: f32) -> f32 {
    let side = thumb_side(rect);
    let travel = (rect.width - side).max(1.0);
    ((x - rect.x - side / 2.0) / travel).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default slider, where the page put it.
    const SLIDER: Rect = Rect::new(10.0, 20.0, RANGE_WIDTH, RANGE_HEIGHT);

    #[test]
    fn the_thumb_stays_inside_the_slider_at_both_ends() {
        let half = THUMB / 2.0;
        assert_eq!(thumb_centre(SLIDER, 0.0), SLIDER.x + half);
        assert_eq!(thumb_centre(SLIDER, 1.0), SLIDER.right() - half);
        assert_eq!(
            thumb_centre(SLIDER, 7.0),
            thumb_centre(SLIDER, 1.0),
            "a position past the end is the end"
        );
    }

    /// What paint draws and what the pointer reads are one mapping: a press on
    /// the thumb's middle is the position it was drawn at.
    #[test]
    fn a_press_on_the_thumb_reads_back_where_it_was_drawn() {
        for position in [0.0, 0.25, 0.5, 0.9, 1.0] {
            let read = position_at(SLIDER, thumb_centre(SLIDER, position));
            assert!(
                (read - position).abs() < 1e-5,
                "drawn at {position}, read back as {read}"
            );
        }
        assert_eq!(position_at(SLIDER, SLIDER.x), 0.0, "the left edge");
        assert_eq!(position_at(SLIDER, SLIDER.right()), 1.0, "the right edge");
    }

    /// A slider shorter than the thumb draws a smaller one, and is read with the
    /// smaller one too — or the reader would drag a thumb that is not the one
    /// on the screen.
    #[test]
    fn a_short_slider_is_read_with_the_thumb_it_draws() {
        let short = Rect::new(0.0, 0.0, 200.0, 8.0);
        assert_eq!(thumb_side(short), 8.0);
        assert_eq!(thumb_centre(short, 0.0), 4.0);
        assert_eq!(position_at(short, 4.0), 0.0);
        assert_eq!(position_at(short, 196.0), 1.0);
    }
}
