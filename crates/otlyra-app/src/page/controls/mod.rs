//! What a form control does when the reader uses it, with no script anywhere.
//!
//! The activation behaviour HTML gives each control, and the file picker, which is
//! the one control whose answer comes from outside the page. Its own module
//! because a control's behaviour is the control's: the page hands it a press or a
//! key and is told whether anything changed. The controls that do more than answer
//! a press have a module each below this one — the parts of a date, the slider,
//! the lists a drop-down and a field's suggestions open, and sending a form — and
//! so does describing a control in words.

mod facts;
mod lists;
mod segments;
mod slider;
mod submit;

pub use facts::{ControlFacts, Numeric};
pub use slider::SliderMotion;

use otlyra_dom::{Document, NodeId};

use super::PageScene;

impl PageScene {
    /// The file picker a press has asked to open, if it asked.
    ///
    /// Taken rather than read, for the reason a submission is: a dialogue opens
    /// once, and one left here for the next frame to find would open twice.
    pub fn take_file_request(&mut self) -> Option<FileRequest> {
        self.pending_pick.take()
    }

    /// Hand a picker what the reader chose, or that they chose nothing.
    ///
    /// Cancelling is not choosing: a dialogue dismissed leaves the control holding
    /// what it held, which is what both references do.
    pub fn set_files(&mut self, node: NodeId, files: Vec<otlyra_dom::form::ChosenFile>) -> bool {
        if files.is_empty() {
            return false;
        }
        self.form.set_files(node, files);
        self.form.note_interaction(node);
        self.invalidate_styles();
        true
    }

    /// What activating a control does, with no script anywhere.
    ///
    /// HTML calls the checkbox and radio halves *legacy-pre-activation behaviour*
    /// and describes them exactly: a checkbox flips and stops being indeterminate;
    /// a radio button becomes the one that is checked, which unchecks every other
    /// member of its group. A group is the same tree, the same form owner and the
    /// same non-empty name.
    pub(super) fn activate(&mut self, node: NodeId) -> bool {
        use otlyra_dom::form::{Control, InputKind};

        if !otlyra_dom::form::is_mutable(&self.document, node) {
            return false;
        }
        match Control::of(&self.document, node) {
            Some(Control::Input(InputKind::Checkbox)) => {
                let checked = self.form.checkedness(&self.document, node);
                self.form.set_checkedness(node, !checked);
            }
            Some(Control::Input(InputKind::Radio)) => {
                for member in otlyra_dom::form::radio_group(&self.document, node) {
                    self.form.set_checkedness(member, member == node);
                }
            }
            // Pressing a picker asks for a dialogue. Asking is all it does: what
            // opens, and whether anything does, is the shell's answer.
            Some(Control::Input(InputKind::File)) => {
                self.pending_pick = Some(FileRequest {
                    node,
                    many: otlyra_dom::form::takes_many_files(&self.document, node),
                    accept: otlyra_dom::form::accepted_files(&self.document, node),
                });
                return true;
            }
            // A button sends the form it belongs to, or puts it back — which is the
            // whole of what a form does without a script, and what every form on
            // the web did before there was one.
            Some(Control::Input(InputKind::Submit | InputKind::Image) | Control::Button)
                if otlyra_dom::form::is_submit_button(&self.document, node) =>
            {
                self.form.note_interaction(node);
                return self.submit(Some(node));
            }
            Some(Control::Input(InputKind::Reset)) => {
                let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
                    return false;
                };
                return self.reset(form);
            }
            Some(Control::Button)
                if attribute_of(&self.document, node, "type")
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("reset")) =>
            {
                let Some(form) = otlyra_dom::form::form_owner(&self.document, node) else {
                    return false;
                };
                return self.reset(form);
            }
            _ => return false,
        }
        self.form.note_interaction(node);
        self.invalidate_styles();
        true
    }

    /// Do to `node` what a press and a release over it would do.
    ///
    /// The route a reader's *press* takes is the route the pointer takes, one step
    /// further in: the focus moves first, as it does on the way down, and then the
    /// activation behaviour runs, as it does on the way up. Anything else would be
    /// a second account of what pressing a control means, and the first bug in it
    /// would be the two disagreeing.
    pub fn activate_node(&mut self, node: NodeId) -> bool {
        // An option is chosen rather than pressed, and it is chosen through the
        // drop-down that owns it — which is what a press on one in an open list
        // does, and the only thing a press on one can mean.
        if let Some(select) = otlyra_dom::form::owning_select(&self.document, node) {
            if otlyra_dom::form::is_disabled(&self.document, node)
                || !otlyra_dom::form::is_mutable(&self.document, select)
            {
                return false;
            }
            self.choose_option(select, node);
            self.close_open();
            return true;
        }
        // A suggestion is taken rather than chosen, and it goes into the field
        // showing it — the same thing a press on one does.
        if let Some(field) =
            otlyra_dom::form::suggested_control(&self.document, node, self.interaction.open)
        {
            if otlyra_dom::form::is_disabled(&self.document, node)
                || !otlyra_dom::form::is_mutable(&self.document, field)
            {
                return false;
            }
            self.take_suggestion(field, node);
            self.close_open();
            return true;
        }
        let mut changed = self.focus_node(node);
        changed |= self.activate(node);
        changed
    }
}

/// A press on a file picker, for whoever can open a dialogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRequest {
    /// The control that asked, which is where the answer goes back.
    pub node: NodeId,
    /// Whether it takes more than one file.
    pub many: bool,
    /// The hints the page gave about what it wants, exactly as it spelled them.
    pub accept: Vec<String>,
}

/// One attribute of one element.
fn attribute_of(document: &Document, id: NodeId, name: &str) -> Option<String> {
    document.get(id)?.element()?.attr(name).map(str::to_owned)
}
