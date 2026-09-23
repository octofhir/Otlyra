//! The caret and the text inside a field.
//!
//! A field is edited in units of its own: the caret and a field's selection are
//! byte offsets into what the control holds, not places in the page's text, and a
//! field showing a placeholder is showing text that is in no control at all. Its
//! own module because everything that moves those offsets — typing, the editing
//! keys, the blink, keeping the caret in sight — counts in that one unit and in no
//! other.

use otlyra_dom::NodeId;
use otlyra_layout::{BoxId, FragmentTree};
use otlyra_text::TextEngine;

use super::PageScene;

/// Time between the visible and hidden halves of a caret blink.
pub(super) const CARET_BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

impl PageScene {
    /// The text a field is showing, which is not always what it holds.
    ///
    /// A date field shows what has been filled in over the shape of what has not,
    /// and holds nothing at all until every part of it is there — so a caret, a
    /// selection and a hit test all have to be counted in what is *drawn*.
    pub(super) fn field_text(&self, node: NodeId) -> String {
        otlyra_dom::form::temporal_display(&self.document, &self.form, node)
            .unwrap_or_else(|| self.form.value(&self.document, node).to_owned())
    }

    /// The reader typed. Returns whether the page has to be drawn again.
    pub fn typed(&mut self, text: &str) -> bool {
        let Some(node) = self.focused_field() else {
            return false;
        };
        // Control characters are not text: a return, a tab and an escape arrive
        // here on some platforms and none of them is a letter.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return false;
        }
        // A date field takes digits into the part the reader is on rather than
        // letters into a string: everything about typing into one is different, so
        // it is answered before any of this.
        if self.segments_of(node).is_some() {
            return self.type_into_segment(node, &text);
        }
        // What is typed over a selection replaces it.
        self.take_field_selection();
        let mut value = self.form.value(&self.document, node).to_owned();
        let at = self.caret.min(value.len());
        value.insert_str(at, &text);
        self.caret = at + text.len();
        self.field_anchor = Some(self.caret);
        self.restart_caret();
        self.form.set_value(node, value);
        self.value_changed(node);
        self.refresh_suggestions(node);
        true
    }

    /// The reader pressed a key that edits or moves the caret rather than typing.
    pub fn edit_text(&mut self, action: EditAction, extend: bool) -> bool {
        self.keyboard = true;
        self.restart_caret();
        let Some(node) = self.focused_field() else {
            return false;
        };
        // The same for the keys that move a caret: a date field has parts rather
        // than characters, so an arrow moves between them and a backspace empties
        // the one the reader is on.
        if let Some(segments) = self.segments_of(node) {
            let last = segments.len() - 1;
            let index = self.segment.min(last);
            match action {
                EditAction::Left => self.hold_segment(node, index.saturating_sub(1)),
                EditAction::Right => self.hold_segment(node, (index + 1).min(last)),
                EditAction::Home => self.hold_segment(node, 0),
                EditAction::End => self.hold_segment(node, last),
                EditAction::Backspace | EditAction::Delete => {
                    let cleared = self.clear_segment(node, index);
                    self.segment_typed = 0;
                    return cleared;
                }
            }
            return true;
        }
        // A backspace or a delete over a selection takes the selection, not one
        // character beside it.
        if matches!(action, EditAction::Backspace | EditAction::Delete)
            && self.take_field_selection()
        {
            self.value_changed(node);
            self.refresh_suggestions(node);
            return true;
        }
        let mut value = self.form.value(&self.document, node).to_owned();
        let at = self.caret.min(value.len());
        match action {
            EditAction::Backspace => {
                let Some(previous) = previous_boundary(&value, at) else {
                    return false;
                };
                value.replace_range(previous..at, "");
                self.caret = previous;
                self.field_anchor = Some(self.caret);
            }
            EditAction::Delete => {
                let Some(next) = next_boundary(&value, at) else {
                    return false;
                };
                value.replace_range(at..next, "");
                self.field_anchor = Some(self.caret);
            }
            // Moving the caret changes nothing the page holds, so it neither
            // restyles nor relays out — but it is still something that happened,
            // and once there is a caret to draw it is something to draw.
            //
            // Held with shift it takes the letters it passes; without, it drops
            // whatever was taken and goes to the end it was pushed towards — which
            // is why an arrow out of a selection lands at its edge rather than one
            // character in from where the caret happened to be.
            EditAction::Left | EditAction::Right | EditAction::Home | EditAction::End => {
                let collapsed = (!extend).then(|| self.field_selection()).flatten();
                self.caret = match (action, collapsed) {
                    (EditAction::Left, Some((_, from, _))) => from,
                    (EditAction::Right, Some((_, _, to))) => to,
                    (EditAction::Left, None) => previous_boundary(&value, at).unwrap_or(0),
                    (EditAction::Right, None) => next_boundary(&value, at).unwrap_or(value.len()),
                    (EditAction::Home, _) => 0,
                    (EditAction::End, _) => value.len(),
                    _ => at,
                };
                if !extend {
                    self.field_anchor = Some(self.caret);
                }
                self.show_ring();
                return true;
            }
        }
        self.form.set_value(node, value);
        self.value_changed(node);
        self.refresh_suggestions(node);
        true
    }

    /// What is selected inside the focused field, as a pair of offsets in order.
    ///
    /// Empty when the two ends are the same, which is a caret and not a selection.
    pub(super) fn field_selection(&self) -> Option<(NodeId, usize, usize)> {
        let node = self.focused_field()?;
        let anchor = self.field_anchor?;
        let (from, to) = (anchor.min(self.caret), anchor.max(self.caret));
        (from < to).then_some((node, from, to))
    }

    /// Take out what is selected inside the focused field, leaving the caret where
    /// it was.
    ///
    /// What every editor does before it puts anything in: typing over a selection
    /// replaces it, and so does a backspace.
    fn take_field_selection(&mut self) -> bool {
        let Some((node, from, to)) = self.field_selection() else {
            return false;
        };
        let mut value = self.form.value(&self.document, node).to_owned();
        value.replace_range(from..to, "");
        self.caret = from;
        self.field_anchor = Some(from);
        self.form.set_value(node, value);
        true
    }

    /// Whether the page has a caret that has to keep being drawn.
    ///
    /// What makes the frame loop keep asking for frames: a caret that blinks is
    /// the one thing on a still page that changes on its own.
    #[must_use]
    pub fn caret_blinks(&self) -> bool {
        self.caret_source().is_some()
    }

    /// The next instant at which the caret changes visibility.
    ///
    /// Returning a deadline instead of "animating" keeps an otherwise idle
    /// browser asleep for the whole half-second between transitions.
    pub fn next_caret_frame(&self) -> Option<std::time::Instant> {
        self.caret_source()?;
        let elapsed = std::time::Instant::now().saturating_duration_since(self.caret_since);
        let intervals = elapsed.as_millis() / CARET_BLINK_INTERVAL.as_millis() + 1;
        let millis = intervals
            .saturating_mul(CARET_BLINK_INTERVAL.as_millis())
            .min(u128::from(u64::MAX)) as u64;
        Some(self.caret_since + std::time::Duration::from_millis(millis))
    }

    /// Whether the caret is showing this instant.
    ///
    /// Half a second on and half a second off, which is what every platform does
    /// and close enough to all of them that nobody will look twice. Measured from
    /// the last time the caret moved, so it is solid under the reader's fingers
    /// while they type and only starts blinking once they stop.
    pub(super) fn caret_showing(&self) -> bool {
        (self.caret_since.elapsed().as_millis() / CARET_BLINK_INTERVAL.as_millis())
            .is_multiple_of(2)
    }

    /// The caret was put somewhere: start its blinking over.
    pub(super) fn restart_caret(&mut self) {
        self.caret_since = std::time::Instant::now();
    }

    /// Pretend the caret was put where it is longer ago than it was.
    ///
    /// For the test of the blinking, which would otherwise have to sleep for half
    /// a second to watch half a blink.
    #[cfg(test)]
    pub(super) fn wind_caret_back(&mut self, by: std::time::Duration) {
        self.caret_since -= by;
    }

    /// Slide the text inside a field so that the caret is inside it.
    ///
    /// A field is one line long however much is typed into it, so the line moves
    /// under the box. Which way it has to move is a question about where the caret
    /// is, and where the caret is takes a layout — so this lays the page out, looks,
    /// and lays it out again only when the answer moved. It moves when the caret
    /// reaches an edge and not on every letter.
    pub(super) fn keep_caret_in_view(
        &mut self,
        caret: Option<Caret>,
        text: &mut TextEngine,
        width: f32,
        height: f32,
    ) {
        let Some(Caret::InField { box_id, offset }) = caret else {
            return;
        };
        let was = self.boxes.control_scroll(box_id);

        let fragments = self.fragments(text, width, height);
        let Some(caret_rect) = otlyra_layout::selection::caret_in(fragments, box_id, offset) else {
            return;
        };
        let Some(inner) = otlyra_layout::selection::content_box(fragments, box_id) else {
            return;
        };

        // A hair of room on each side, so a caret at the very end is beside the
        // edge rather than under it.
        const MARGIN: f32 = 1.0;
        let mut now = was;
        if caret_rect.right() > inner.right() - MARGIN {
            now.0 += caret_rect.right() - (inner.right() - MARGIN);
        }
        if caret_rect.x < inner.x + MARGIN {
            now.0 -= (inner.x + MARGIN) - caret_rect.x;
        }
        if caret_rect.bottom() > inner.bottom() {
            now.1 += caret_rect.bottom() - inner.bottom();
        }
        if caret_rect.y < inner.y {
            now.1 -= inner.y - caret_rect.y;
        }
        // Never past the start: a field with room to spare shows its first letter
        // at its left edge and its first line at its top, not a gap where one used
        // to be.
        now = (now.0.max(0.0), now.1.max(0.0));

        if (now.0 - was.0).abs() > 0.01 || (now.1 - was.1).abs() > 0.01 {
            self.boxes.set_control_scroll(box_id, now);
            self.layout_stale = true;
        }
    }

    /// What the caret belongs to, if an editable field has keyboard focus.
    ///
    /// A collapsed page-text selection is only the anchor for a possible drag.
    /// Browsers do not draw it as a caret unless a separate caret-browsing mode is
    /// enabled, which Otlyra does not currently expose.
    pub(super) fn caret_source(&self) -> Option<Caret> {
        let node = self.focused_field()?;
        let box_id = self.boxes.box_for(node)?;
        let value = self.field_text(node);
        Some(Caret::InField {
            box_id,
            offset: self.caret.min(value.len()),
        })
    }

    /// What the focused control holds, for a panel that wants to show it.
    pub fn focused_value(&self) -> Option<&str> {
        let node = self.interaction.focus?;
        Some(self.form.value(&self.document, node))
    }
}

/// A key that edits or moves the caret rather than adding a letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// Remove what is before the caret.
    Backspace,
    /// Remove what is after it.
    Delete,
    /// One character back.
    Left,
    /// One character on.
    Right,
    /// To the start.
    Home,
    /// To the end.
    End,
}

/// The byte before `at` that starts a character, or nothing at the start.
///
/// Characters rather than bytes, because a backspace that removes a byte of a
/// multi-byte letter leaves a string that is not text.
fn previous_boundary(value: &str, at: usize) -> Option<usize> {
    if at == 0 {
        return None;
    }
    value[..at]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
}

/// The byte after `at` that starts a character, or nothing at the end.
fn next_boundary(value: &str, at: usize) -> Option<usize> {
    let rest = value.get(at..)?;
    let mut chars = rest.chars();
    let next = chars.next()?;
    Some(at + next.len_utf8())
}

/// What the caret belongs to.
///
/// The field is settled before the layout is borrowed and its rectangle after,
/// so this remains a value rather than a borrowed fragment rectangle.
#[derive(Clone, Copy, Debug)]
pub(super) enum Caret {
    /// In a field, at a byte offset into its value.
    InField {
        /// The box the field generated.
        box_id: BoxId,
        /// How far into what it holds.
        offset: usize,
    },
}

impl Caret {
    /// Where it is drawn.
    pub(super) fn rect(self, fragments: &FragmentTree) -> Option<otlyra_layout::Rect> {
        match self {
            Self::InField { box_id, offset } => {
                otlyra_layout::selection::caret_in(fragments, box_id, offset)
            }
        }
    }
}

/// The word around a byte offset: from the first character of it to past the last.
///
/// A word is a run of what is not white space, which is coarser than the page's
/// own idea of one and is what a field needs — there are no paragraphs in a field
/// and nothing to break a word across.
pub(super) fn word_around(value: &str, at: usize) -> (usize, usize) {
    let at = at.min(value.len());
    let mut from = at;
    for (index, character) in value[..at].char_indices().rev() {
        if character.is_whitespace() {
            break;
        }
        from = index;
    }
    let mut to = at;
    for (index, character) in value[at..].char_indices() {
        if character.is_whitespace() {
            break;
        }
        to = at + index + character.len_utf8();
    }
    (from, to)
}
