//! The parts of a date or a time field.
//!
//! A date field is not a string with a caret in it: it is a row of parts, each a
//! number with a range, filled in one at a time, and it holds nothing at all until
//! every part is there. Its own module because typing, the arrows, backspace and a
//! press all mean something different on one, and each of them comes here instead
//! of to the text editing a field otherwise gets.

use otlyra_dom::NodeId;

use crate::page::PageScene;

impl PageScene {
    /// The parts a date or a time field is filled in by, if it is one.
    pub(in crate::page) fn segments_of(
        &self,
        node: NodeId,
    ) -> Option<&'static [otlyra_dom::form::Segment]> {
        let otlyra_dom::form::Control::Input(kind) =
            otlyra_dom::form::Control::of(&self.document, node)?
        else {
            return None;
        };
        otlyra_dom::form::temporal_pattern(kind).map(|(_, segments)| segments)
    }

    /// Take hold of one part of a date field: the part is selected, which is what
    /// shows the reader where the next digit will go.
    pub(in crate::page) fn hold_segment(&mut self, node: NodeId, index: usize) {
        let Some(segments) = self.segments_of(node) else {
            return;
        };
        let Some(segment) = segments.get(index.min(segments.len() - 1)) else {
            return;
        };
        self.segment = index.min(segments.len() - 1);
        self.segment_typed = 0;
        self.field_anchor = Some(segment.at);
        self.caret = segment.at + segment.width;
        self.restart_caret();
    }

    /// Which part of a date field an offset into its text falls in.
    pub(in crate::page) fn segment_at(&self, node: NodeId, offset: usize) -> usize {
        let Some(segments) = self.segments_of(node) else {
            return 0;
        };
        segments
            .iter()
            .rposition(|segment| offset >= segment.at)
            .unwrap_or(0)
    }

    /// Write a number into one part of a date field.
    ///
    /// The whole of what the control holds is settled here as well, because the two
    /// are one answer: a date with a part still missing is a control holding
    /// nothing, whatever is on screen.
    fn write_segment(&mut self, node: NodeId, index: usize, number: u32) -> bool {
        let Some(segments) = self.segments_of(node) else {
            return false;
        };
        let Some(segment) = segments.get(index) else {
            return false;
        };
        let mut shown: Vec<char> = self.field_text(node).chars().collect();
        let digits = format!("{:0width$}", number, width = segment.width);
        for (offset, digit) in digits.chars().enumerate() {
            if let Some(slot) = shown.get_mut(segment.at + offset) {
                *slot = digit;
            }
        }
        self.commit_draft(node, shown.into_iter().collect())
    }

    /// Clear one part back to the shape it started as.
    pub(in crate::page) fn clear_segment(&mut self, node: NodeId, index: usize) -> bool {
        let Some(otlyra_dom::form::Control::Input(kind)) =
            otlyra_dom::form::Control::of(&self.document, node)
        else {
            return false;
        };
        let Some((pattern, segments)) = otlyra_dom::form::temporal_pattern(kind) else {
            return false;
        };
        let Some(segment) = segments.get(index) else {
            return false;
        };
        let empty: Vec<char> = pattern.chars().collect();
        let mut shown: Vec<char> = self.field_text(node).chars().collect();
        for offset in 0..segment.width {
            if let (Some(slot), Some(blank)) = (
                shown.get_mut(segment.at + offset),
                empty.get(segment.at + offset),
            ) {
                *slot = *blank;
            }
        }
        self.commit_draft(node, shown.into_iter().collect())
    }

    /// Record what a date field is showing and what it therefore holds.
    fn commit_draft(&mut self, node: NodeId, shown: String) -> bool {
        let Some(otlyra_dom::form::Control::Input(kind)) =
            otlyra_dom::form::Control::of(&self.document, node)
        else {
            return false;
        };
        if shown == self.field_text(node) {
            return false;
        }
        let value = otlyra_dom::form::temporal_value(&shown, kind);
        self.form.set_draft(node, shown, value);
        self.form.note_interaction(node);
        self.invalidate_styles();
        true
    }

    /// What one part of a date field currently reads, if it has been filled in.
    fn segment_number(&self, node: NodeId, index: usize) -> Option<u32> {
        let segments = self.segments_of(node)?;
        let segment = segments.get(index)?;
        self.field_text(node)
            .chars()
            .skip(segment.at)
            .take(segment.width)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    }

    /// A digit typed into a date field.
    ///
    /// The digits of one part are taken in the order they are typed and the part
    /// moves on when it is full or when another digit could not fit — typing `7`
    /// into a month leaves July and moves on, because no month starts with a seven.
    pub(in crate::page) fn type_into_segment(&mut self, node: NodeId, text: &str) -> bool {
        let Some(segments) = self.segments_of(node) else {
            return false;
        };
        let mut changed = false;
        for digit in text.chars().filter(char::is_ascii_digit) {
            let index = self.segment.min(segments.len() - 1);
            let Some(segment) = segments.get(index) else {
                break;
            };
            let digit = u32::from(digit as u8 - b'0');
            let (low, high) = segment.unit.bounds();
            let held = (self.segment_typed > 0)
                .then(|| self.segment_number(node, index))
                .flatten()
                .unwrap_or(0);
            let mut wanted = held * 10 + digit;
            if wanted > high {
                wanted = digit.clamp(low.min(digit), high);
                self.segment_typed = 0;
            }
            changed |= self.write_segment(node, index, wanted);
            self.segment_typed += 1;
            // Full, or nothing more could be added without going over: the next
            // part is where the reader is now.
            if self.segment_typed >= segment.width || wanted * 10 > high {
                self.hold_segment(node, (index + 1).min(segments.len() - 1));
            } else {
                let at = segment.at;
                self.field_anchor = Some(at);
                self.caret = at + segment.width;
            }
        }
        changed
    }

    /// Move a date field's part one up or down, wrapping at its ends.
    pub(super) fn step_segment(&mut self, node: NodeId, forward: bool) -> bool {
        let Some(segments) = self.segments_of(node) else {
            return false;
        };
        let index = self.segment.min(segments.len() - 1);
        let Some(segment) = segments.get(index) else {
            return false;
        };
        let (low, high) = segment.unit.bounds();
        let held = self.segment_number(node, index);
        let wanted = match (held, forward) {
            (Some(value), true) if value >= high => low,
            (Some(value), true) => value + 1,
            (Some(value), false) if value <= low => high,
            (Some(value), false) => value - 1,
            // Nothing there yet: a step up starts at the bottom of the range and a
            // step down at the top, which is what both references do.
            (None, true) => low,
            (None, false) => high,
        };
        let changed = self.write_segment(node, index, wanted);
        self.segment_typed = 0;
        self.hold_segment(node, index);
        changed
    }
}
