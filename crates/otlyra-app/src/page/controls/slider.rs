//! A slider: where the pointer puts it, and how far a key moves it.
//!
//! Its own module because a range input is the one control that is dragged and the
//! one whose value is a number read off a position: the pointer holds it from the
//! press to the release, and a key moves it by steps rather than through
//! characters. The same keys step the parts of a date field, which is asked here
//! first and handed to the parts.

use otlyra_dom::NodeId;

use crate::page::PageScene;

impl PageScene {
    /// Whether `node` is a slider the reader can move.
    pub(in crate::page) fn slider(&self, node: NodeId) -> bool {
        otlyra_dom::form::Control::of(&self.document, node)
            == Some(otlyra_dom::form::Control::Input(
                otlyra_dom::form::InputKind::Range,
            ))
            && otlyra_dom::form::is_mutable(&self.document, node)
    }

    /// Put a slider where the pointer is.
    ///
    /// The thumb travels between the two ends rather than off them, so the value
    /// is taken from where the *middle* of the thumb would have to be — which is
    /// what makes a press at the very left edge give the minimum rather than
    /// something a little above it.
    pub(in crate::page) fn slide_to(&mut self, node: NodeId, x: f64) -> bool {
        /// The thumb's width, which the painter draws and layout leaves room for.
        const THUMB: f32 = 14.0;

        let Some(box_id) = self.boxes.box_for(node) else {
            return false;
        };
        let Some(rect) = self.rect_of(box_id) else {
            return false;
        };
        let travel = (rect.width - THUMB).max(1.0);
        let along = ((x as f32 - rect.x - THUMB / 2.0) / travel).clamp(0.0, 1.0);
        let (min, max, _) = otlyra_dom::form::range_bounds(&self.document, node);
        let wanted = otlyra_dom::form::snap_to_step(
            &self.document,
            node,
            min + f64::from(along) * (max - min),
        );
        self.set_slider(node, wanted)
    }

    /// Write a number into a slider, if it is not the number it already holds.
    fn set_slider(&mut self, node: NodeId, value: f64) -> bool {
        let before = otlyra_dom::form::range_value(&self.document, &self.form, node);
        let (min, max, _) = otlyra_dom::form::range_bounds(&self.document, node);
        let value = value.clamp(min, max);
        if (value - before).abs() < f64::EPSILON
            && !self.form.value(&self.document, node).is_empty()
        {
            return false;
        }
        self.form
            .set_value(node, otlyra_dom::form::format_number(value));
        self.form.note_interaction(node);
        self.invalidate_styles();
        true
    }

    /// Move the focused slider by one step, or to an end of its range.
    ///
    /// What the arrows, the page keys, home and end do on a slider. The
    /// specification leaves the interface to the browser and both references do
    /// this, down to a page key being ten steps.
    pub fn step_value(&mut self, motion: SliderMotion) -> bool {
        // A date field answers the same two keys, one part at a time: up on a
        // month is next month, and it wraps rather than running off the end.
        if let Some(node) = self
            .focused_field()
            .filter(|&node| self.segments_of(node).is_some())
        {
            let stepped = match motion {
                SliderMotion::Up | SliderMotion::PageUp => self.step_segment(node, true),
                SliderMotion::Down | SliderMotion::PageDown => self.step_segment(node, false),
                // Home and end belong to the parts rather than to the numbers in
                // them; the caret keys already answer those.
                SliderMotion::Start | SliderMotion::End => return false,
            };
            self.keyboard = true;
            self.show_ring();
            return stepped;
        }
        let Some(node) = self.interaction.focus.filter(|&node| self.slider(node)) else {
            return false;
        };
        let (min, max, step) = otlyra_dom::form::range_bounds(&self.document, node);
        // A slider with no stepping still answers a key: the reference moves it by
        // a hundredth of its range, which is a step in everything but name.
        let step = if step > 0.0 {
            step
        } else {
            (max - min) / 100.0
        };
        let value = otlyra_dom::form::range_value(&self.document, &self.form, node);
        let wanted = match motion {
            SliderMotion::Up => value + step,
            SliderMotion::Down => value - step,
            SliderMotion::PageUp => value + step * 10.0,
            SliderMotion::PageDown => value - step * 10.0,
            SliderMotion::Start => min,
            SliderMotion::End => max,
        };
        self.keyboard = true;
        let moved = self.set_slider(
            node,
            otlyra_dom::form::snap_to_step(&self.document, node, wanted),
        );
        self.show_ring();
        moved
    }
}

/// How far a key moves a slider.
///
/// Not the same set as the caret's: a slider has a step and a range, so the keys
/// mean amounts rather than positions in a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliderMotion {
    /// One step towards the maximum.
    Up,
    /// One step towards the minimum.
    Down,
    /// Ten steps up.
    PageUp,
    /// Ten steps down.
    PageDown,
    /// The minimum.
    Start,
    /// The maximum.
    End,
}
