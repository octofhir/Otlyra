//! Sending a form, checking it first, and putting it back.
//!
//! The page does not navigate: a submission is built here and left for whoever
//! does, the same shape a file picker's request takes. Its own module because
//! every route to a sent form — a submit button, return in a field, a script
//! calling `submit()` — ends in the same few steps, and they stay in agreement
//! most easily side by side.

use otlyra_dom::NodeId;

use super::attribute_of;
use crate::page::{PageScene, descendants_of};

impl PageScene {
    /// Send a form because script called `submit()` on it.
    ///
    /// The reader did not press anything, so there is no submitter button and
    /// no validation: `form.submit()` skips constraint validation on the
    /// platform too, which is the difference between it and `requestSubmit()`.
    pub fn submit_from_script(&mut self, form: otlyra_dom::NodeId) -> bool {
        if self
            .document
            .get(form)
            .and_then(|node| node.element())
            .is_none()
        {
            return false;
        }
        self.pending_submit = Some(otlyra_dom::submit::submission(
            &self.document,
            &self.form,
            form,
            None,
        ));
        true
    }

    /// The form a press has asked to send, if it asked.
    ///
    /// Taken rather than read: a submission happens once, and leaving it here for a
    /// second frame to find would send the form twice.
    ///
    /// The form the page has just sent, if it has sent one.
    pub fn take_submission(&mut self) -> Option<otlyra_dom::Submission> {
        self.pending_submit.take()
    }

    /// Whether every control of `form` holds something acceptable.
    ///
    /// A form that does not is not sent, which is what stops a page from having to
    /// check anything itself. Every control the reader has been shown is marked as
    /// interacted with at the same time, so that `:user-invalid` lights up the
    /// fields that are wrong the moment the reader tries to send.
    fn validate(&mut self, form: NodeId) -> bool {
        let fields: Vec<NodeId> = descendants_of(&self.document, self.document.root())
            .into_iter()
            .filter(|&node| {
                otlyra_dom::form::form_owner(&self.document, node) == Some(form)
                    && otlyra_dom::form::is_validated(&self.document, node)
            })
            .collect();
        let mut ok = true;
        for field in fields {
            if otlyra_dom::form::validity(&self.document, &self.form, field).is_invalid() {
                ok = false;
                self.form.note_interaction(field);
            }
        }
        if !ok {
            self.invalidate_styles();
        }
        ok
    }

    /// Send the form `submitter` belongs to, unless it says not to check first.
    pub(super) fn submit(&mut self, submitter: Option<NodeId>) -> bool {
        let Some(node) = submitter else {
            return false;
        };
        let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
            return false;
        };
        let skip_checking = attribute_of(&self.document, node, "formnovalidate").is_some()
            || attribute_of(&self.document, form, "novalidate").is_some();
        if !skip_checking && !self.validate(form) {
            return true;
        }
        self.pending_submit = Some(otlyra_dom::submit::submission(
            &self.document,
            &self.form,
            form,
            submitter,
        ));
        true
    }

    /// Put a form back the way the markup left it.
    pub(super) fn reset(&mut self, form: NodeId) -> bool {
        self.form.reset(&self.document, Some(form));
        self.invalidate_styles();
        true
    }

    /// Return in a field sends the form, if the form has a button to send it with
    /// or only one field to fill in.
    ///
    /// HTML calls it *implicit submission*, and it is why a search box with nothing
    /// but a field in it works at all.
    pub fn implicit_submit(&mut self) -> bool {
        let Some(node) = self.focused_field() else {
            return false;
        };
        let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
            return false;
        };
        let button = otlyra_dom::form::default_button(&self.document, Some(form));
        match button {
            Some(button) => self.submit(Some(button)),
            // No button to press: a form with exactly one field that takes typing
            // is still sent, and one with several is not.
            None => {
                let fields = descendants_of(&self.document, self.document.root())
                    .into_iter()
                    .filter(|&field| {
                        otlyra_dom::form::form_owner(&self.document, field) == Some(form)
                            && otlyra_dom::form::Control::of(&self.document, field)
                                .is_some_and(otlyra_dom::form::Control::is_text_entry)
                    })
                    .count();
                if fields == 1 {
                    self.submit_form(form)
                } else {
                    false
                }
            }
        }
    }

    /// Send a form with no button behind it.
    fn submit_form(&mut self, form: NodeId) -> bool {
        if attribute_of(&self.document, form, "novalidate").is_none() && !self.validate(form) {
            return true;
        }
        self.pending_submit = Some(otlyra_dom::submit::submission(
            &self.document,
            &self.form,
            form,
            None,
        ));
        true
    }
}
