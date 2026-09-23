//! The lists a control opens over the page: a drop-down's options, and a field's
//! suggestions.
//!
//! Both hang off a control, are walked with the arrows, taken with return and put
//! away with escape, and both have to slide to what the reader is on. Its own
//! module because a `<select>` and a `<datalist>` differ in one thing — choosing
//! an option is a choice, and taking a suggestion is a value typed — and keeping
//! the two side by side is what keeps that the only difference.

use otlyra_dom::NodeId;
use otlyra_layout::BoxId;
use otlyra_text::TextEngine;

use crate::page::PageScene;

impl PageScene {
    /// Put a suggestion into the field it was offered to.
    ///
    /// It is the field's value and nothing else — a suggestion is not a choice the
    /// way an option of a `<select>` is, and once it is taken the `<datalist>` it
    /// came from has no part in what the form sends.
    pub(in crate::page) fn take_suggestion(&mut self, field: NodeId, option: NodeId) {
        let value = otlyra_dom::form::option_value(&self.document, option);
        self.caret = value.len();
        self.field_anchor = Some(self.caret);
        self.field_dragging = false;
        self.form.set_value(field, value);
        self.form.note_interaction(field);
        self.restart_caret();
        self.invalidate_styles();
    }

    /// Make one option of a `<select>` the chosen one.
    ///
    /// Setting one clears the rest, because a drop-down shows one answer — which is
    /// the same rule a radio group follows and for the same reason.
    pub(in crate::page) fn choose_option(&mut self, select: NodeId, option: NodeId) {
        for other in otlyra_dom::form::options_of(&self.document, select) {
            self.form.set_selectedness(other, other == option);
        }
        self.invalidate_styles();
    }

    /// Whether a list is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.interaction.open.is_some()
    }

    /// Put away whatever is open, leaving what it was showing untaken.
    pub fn close_open(&mut self) -> bool {
        self.set_interaction(otlyra_css::state::Interaction {
            open: None,
            suggestion: None,
            ..self.interaction
        })
    }

    /// Put away whatever is open, taking what the reader had walked to.
    ///
    /// The difference between escape and return over an open list. A `<select>`
    /// has already taken it — the arrows move its choice as they go, which is what
    /// they do everywhere — and a field has not, because a suggestion walked to is
    /// not a value typed.
    pub fn accept_open(&mut self) -> bool {
        if let Some(option) = self.interaction.suggestion
            && let Some(field) =
                otlyra_dom::form::suggested_control(&self.document, option, self.interaction.open)
        {
            self.take_suggestion(field, option);
        }
        self.close_open()
    }

    /// Move the chosen option of the focused `<select>` by one.
    ///
    /// What the arrows do on a drop-down, open or not: the specification leaves the
    /// interface to the browser, and every one of them moves the choice.
    pub fn step_selection(&mut self, forward: bool) -> bool {
        let Some(select) = self.interaction.focus else {
            return false;
        };
        if otlyra_dom::form::takes_suggestions(&self.document, select) {
            return self.step_suggestion(select, forward);
        }
        if otlyra_dom::form::Control::of(&self.document, select)
            != Some(otlyra_dom::form::Control::Select)
        {
            return false;
        }
        let options: Vec<NodeId> = otlyra_dom::form::options_of(&self.document, select)
            .into_iter()
            .filter(|&option| !otlyra_dom::form::is_disabled(&self.document, option))
            .collect();
        if options.is_empty() {
            return false;
        }
        let current = options
            .iter()
            .position(|&option| self.form.selectedness(&self.document, option))
            .unwrap_or(0);
        let next = if forward {
            (current + 1).min(options.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        if next == current {
            return false;
        }
        self.choose_option(select, options[next]);
        true
    }

    /// Walk the suggestions of a field, showing them if they are not showing.
    ///
    /// Walking to one does not put it in the field: the list is narrowed by what
    /// has been typed, so filling the field as the reader walked would narrow the
    /// list under them to the one line they had just reached. Return takes it.
    fn step_suggestion(&mut self, field: NodeId, forward: bool) -> bool {
        if !otlyra_dom::form::is_mutable(&self.document, field) {
            return false;
        }
        let suggestions = otlyra_dom::form::suggestions_for(&self.document, &self.form, field);
        if suggestions.is_empty() {
            return false;
        }
        if self.interaction.open != Some(field) {
            return self.set_interaction(otlyra_css::state::Interaction {
                open: Some(field),
                suggestion: None,
                ..self.interaction
            });
        }
        let at = self
            .interaction
            .suggestion
            .and_then(|option| suggestions.iter().position(|&other| other == option));
        // Down from nothing lands on the first, up from nothing on the last: what
        // a menu does when it is walked into from either end.
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => suggestions.len() - 1,
            (Some(at), true) => (at + 1).min(suggestions.len() - 1),
            (Some(at), false) => at.saturating_sub(1),
        };
        self.set_interaction(otlyra_css::state::Interaction {
            suggestion: Some(suggestions[next]),
            ..self.interaction
        })
    }

    /// A field's suggestions follow what has been typed into it.
    ///
    /// Typing shows them, narrows them, and puts them away once nothing is left
    /// that matches — which is what makes a `<datalist>` a list of completions
    /// rather than a menu that has to be opened.
    pub(in crate::page) fn refresh_suggestions(&mut self, node: NodeId) {
        if !otlyra_dom::form::takes_suggestions(&self.document, node) {
            return;
        }
        let showing =
            !otlyra_dom::form::suggestions_for(&self.document, &self.form, node).is_empty();
        self.set_interaction(otlyra_css::state::Interaction {
            open: showing.then_some(node),
            suggestion: None,
            ..self.interaction
        });
    }

    /// The box an open drop-down's list is in, if one is open.
    fn open_list(&self) -> Option<BoxId> {
        let select = self.interaction.open?;
        let box_id = self.boxes.box_for(select)?;
        self.boxes
            .node(box_id)
            .children
            .iter()
            .copied()
            .find(|&child| self.boxes.node(child).control.is_some())
    }

    /// Slide an open list so that the chosen option is in it.
    ///
    /// The same shape as keeping the caret in sight, and for the same reason: how
    /// far it has to move is only known once it has been laid out, so the page lays
    /// out, looks, and lays out again when the answer moved.
    pub(in crate::page) fn keep_choice_in_view(
        &mut self,
        text: &mut TextEngine,
        width: f32,
        height: f32,
    ) {
        let Some(list) = self.open_list() else {
            return;
        };
        let Some(select) = self.interaction.open else {
            return;
        };
        // A drop-down slides to what is chosen and a field to the suggestion the
        // arrows have reached: the same question, asked of whichever of the two
        // the open list belongs to.
        let chosen = self
            .interaction
            .suggestion
            .or_else(|| {
                otlyra_dom::form::options_of(&self.document, select)
                    .into_iter()
                    .find(|&option| self.form.selectedness(&self.document, option))
            })
            .and_then(|option| self.boxes.box_for(option));
        let Some(chosen) = chosen else { return };

        let was = self.boxes.control_scroll(list);
        let fragments = self.fragments(text, width, height);
        let Some(option) = otlyra_layout::selection::content_box(fragments, chosen) else {
            return;
        };
        let Some(inner) = otlyra_layout::selection::content_box(fragments, list) else {
            return;
        };

        let mut now = was;
        if option.bottom() > inner.bottom() {
            now.1 += option.bottom() - inner.bottom();
        }
        if option.y < inner.y {
            now.1 -= inner.y - option.y;
        }
        now.1 = now.1.max(0.0);
        if (now.1 - was.1).abs() > 0.01 {
            self.boxes.set_control_scroll(list, now);
            self.layout_stale = true;
        }
    }
}
