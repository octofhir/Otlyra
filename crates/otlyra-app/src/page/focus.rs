//! Where the keyboard is, and the interaction state the page is styled from.
//!
//! The focus is the one thing both the pointer and the keyboard move, so what can
//! take it, the order Tab walks it in and what moving it costs are kept apart from
//! either of them. Its own module because moving the focus, the hover or an open
//! list is a change of the state selectors read, and whether that change is a
//! restyle or only a repaint is a question with one answer that is asked from
//! everywhere.

use otlyra_dom::NodeId;
use otlyra_layout::Damage;

use super::{PageScene, in_document_order};

impl PageScene {
    /// Whether an element can take the focus.
    pub(super) fn is_focusable(&self, node: NodeId) -> bool {
        // A link is focusable and is not a form control, so it is asked about
        // first: without this the keyboard could reach a page's fields and
        // never its links, which is most of what a page is.
        if self.is_link(node) {
            return true;
        }
        // Anything the page put a readable `tabindex` on, whatever it is
        // otherwise: that is what the attribute is for.
        if self.tabindex_of(node).is_some() {
            return true;
        }
        match otlyra_dom::form::Control::of(&self.document, node) {
            Some(control) => {
                !matches!(
                    control,
                    otlyra_dom::form::Control::Option
                        | otlyra_dom::form::Control::Optgroup
                        | otlyra_dom::form::Control::Output
                        | otlyra_dom::form::Control::Meter
                        | otlyra_dom::form::Control::Progress
                        | otlyra_dom::form::Control::Fieldset
                ) && !otlyra_dom::form::is_disabled(&self.document, node)
            }
            None => false,
        }
    }

    /// Whether `node` is a link a reader can follow.
    ///
    /// A name with no address is not one: `<a name=…>` is a place in the page
    /// rather than a way out of it, and it is not focusable anywhere else
    /// either.
    fn is_link(&self, node: NodeId) -> bool {
        self.document
            .get(node)
            .and_then(otlyra_dom::Node::element)
            .is_some_and(|element| element.name.local.as_ref() == "a")
            && self.attribute(node, "href").is_some()
    }

    /// What `tabindex` says about a node, if it says anything readable.
    ///
    /// A value that is not a number is no value: HTML says an invalid one is
    /// ignored, which leaves the element focusable exactly as it would have
    /// been without the attribute.
    fn tabindex_of(&self, node: NodeId) -> Option<i32> {
        self.attribute(node, "tabindex")?.trim().parse().ok()
    }

    /// Everything the keyboard may stop on, in the order a reader meets it.
    ///
    /// HTML's sequential focus navigation order: the positive `tabindex` values
    /// first, ascending, then everything focusable by default or by `tabindex=0`
    /// in document order. Ties keep document order, which is what makes the
    /// order stable rather than an accident of how it was collected. A negative
    /// value is focusable — a script or a press may put the focus there — and
    /// is not walked to, which is the whole of what it is for.
    fn focus_order(&self) -> Vec<NodeId> {
        let mut ordered: Vec<(i32, usize, NodeId)> =
            in_document_order(&self.document, self.document.root())
                .into_iter()
                .enumerate()
                .filter_map(|(position, node)| {
                    if !self.is_focusable(node) {
                        return None;
                    }
                    let index = self.tabindex_of(node).unwrap_or(0);
                    (index >= 0).then_some((index, position, node))
                })
                .collect();
        // Zero comes last among the numbers, so it sorts as though it were
        // larger than every positive one rather than smaller than all of them.
        ordered.sort_by_key(|(index, position, _)| {
            (
                if *index == 0 {
                    i64::MAX
                } else {
                    i64::from(*index)
                },
                *position,
            )
        });
        ordered.into_iter().map(|(_, _, node)| node).collect()
    }

    /// Move the keyboard to the next thing it can stop on.
    ///
    /// `false` when there is nothing that way, which is the page saying the
    /// keyboard belongs to whatever is beyond it. Leaving rather than wrapping
    /// is what makes Tab reach the toolbar at the end of a document instead of
    /// trapping a reader inside one.
    pub fn focus_step(&mut self, forward: bool) -> bool {
        let order = self.focus_order();
        if order.is_empty() {
            return false;
        }
        let at = self
            .interaction
            .focus
            .and_then(|node| order.iter().position(|candidate| *candidate == node));
        let next = match (at, forward) {
            (Some(at), true) => at.checked_add(1).filter(|next| *next < order.len()),
            (Some(at), false) => at.checked_sub(1),
            (None, true) => Some(0),
            (None, false) => Some(order.len() - 1),
        };
        let Some(next) = next else {
            // Off the end: the focus goes with the keyboard rather than being
            // left behind on a control nobody is on any more.
            self.blur();
            return false;
        };
        // Whether the focus *moved*, which is not whether the page has to be
        // styled again: a link with no rule that reads `:focus` restyles to the
        // same thing, and a walk that reported that as "there was nowhere to
        // go" would hand the keyboard to the toolbar on the first link.
        self.focus_node(order[next]);
        true
    }

    /// The address the focused element goes to, if it is a link.
    pub fn focused_link(&self) -> Option<String> {
        let node = self.interaction.focus?;
        self.is_link(node).then(|| self.attribute(node, "href"))?
    }

    /// Whether the focused control takes typing.
    pub(super) fn focused_field(&self) -> Option<NodeId> {
        let node = self.interaction.focus?;
        let control = otlyra_dom::form::Control::of(&self.document, node)?;
        (control.is_text_entry() && otlyra_dom::form::is_mutable(&self.document, node))
            .then_some(node)
    }

    /// Whether the page's keyboard focus is in an editable text control.
    ///
    /// Browser-level default actions ask this before interpreting Space, Home,
    /// End, or an arrow as page scrolling. Text input arrives as a separate
    /// platform event, so the key event itself has to be claimed here or a space
    /// typed into a field also pages the document down.
    pub fn editing_text(&self) -> bool {
        self.focused_field().is_some()
    }

    /// Adopt a new interaction, restyling only if it can change anything.
    ///
    /// Two reasons it can. A selector may depend on the state — which the engine's
    /// own index answers without looking at a rule — or the element may be a
    /// control, whose widget is drawn from the state whether a rule mentions it or
    /// not. The second is why hovering a button repaints on a page with no `:hover`
    /// rule in it at all.
    pub(super) fn set_interaction(&mut self, next: otlyra_css::state::Interaction) -> bool {
        let before = self.interaction;
        if before == next {
            return false;
        }
        self.interaction = next;

        let touched = otlyra_css::state::touched_nodes(&self.document, before, next);
        let control_touched = touched
            .iter()
            .any(|&node| otlyra_dom::form::Control::of(&self.document, node).is_some());
        let styled_touched = self.styler.as_mut().is_some_and(|styler| {
            styler.interaction_changes_style(&self.document, &self.form, before, next)
        });
        if !control_touched && !styled_touched {
            return false;
        }
        // A pointer moving onto a widget greys it and touches nothing else — no
        // rule matched, so the cascade and the layout are the same as they were,
        // and rebuilding them to redraw one button is what made scrolling with the
        // pointer over a control lag. When only a widget's own picture changed, its
        // state is written straight into the box and the fragment already laid out,
        // and the frame is merely repainted. A rule that *does* read the state
        // takes the whole restyle, because then more than a picture has moved.
        //
        // Only three bits are a picture and nothing else: hovered, held down, and
        // whether the ring is drawn. The rest of an interaction is more than a
        // picture — opening a list builds boxes that were not there, moving the
        // focus moves the caret, walking the suggestions marks a different one — so
        // the cheap path is taken only when none of those moved.
        let picture_only = before.open == next.open
            && before.focus == next.focus
            && before.suggestion == next.suggestion;
        if picture_only
            && control_touched
            && !styled_touched
            && self.repaint_touched_controls(before, next)
        {
            return true;
        }
        self.invalidate_styles();
        true
    }

    /// Redraw the widgets a pointer or a focus move touched, without a restyle.
    ///
    /// Only the three interaction bits a widget draws from — hovered, held down,
    /// ringed — can have changed here, and only for a control that has a box: the
    /// rest of a control's state is the document's and the document did not move.
    /// Returns whether anything was found to repaint, so the caller can fall back
    /// to the full path for a control with no box yet.
    fn repaint_touched_controls(
        &mut self,
        before: otlyra_css::state::Interaction,
        next: otlyra_css::state::Interaction,
    ) -> bool {
        use otlyra_css::state::{ElementState, States};

        let states = States::new(&self.document, &self.form, next);
        let mut painted = false;
        for node in otlyra_css::state::touched_nodes(&self.document, before, next) {
            if otlyra_dom::form::Control::of(&self.document, node).is_none() {
                continue;
            }
            let Some(box_id) = self.boxes.box_for(node) else {
                // A control with no box — an option of a closed drop-down — cannot
                // be repainted in place; let the caller rebuild.
                return false;
            };
            let Some(control) = self.boxes.node(box_id).control.clone() else {
                continue;
            };
            let bits = states.state_of(node);
            let mut state = control.state;
            state.hovered = bits.contains(ElementState::HOVER);
            state.active = bits.contains(ElementState::ACTIVE);
            state.focus_ring = bits.contains(ElementState::FOCUSRING);
            if state == control.state {
                continue;
            }
            self.boxes.set_control_state(box_id, state);
            if let Some((_, tree)) = self.layout.as_mut() {
                tree.set_widget_state(box_id, state);
            }
            painted = true;
        }
        if painted {
            self.damage.add(Damage::PAINT);
        }
        painted
    }

    /// Take the focus off whatever holds it, and answer whether anything moved.
    ///
    /// What a press somewhere else means. The caret and the focus ring say *this
    /// is where your typing goes*, and a reader who has pressed on the page has
    /// said it does not go there any more — so both go away, along with any open
    /// list, exactly as they do in every other browser. Without this a field kept
    /// its caret blinking in it while the reader worked somewhere else, which is
    /// the browser saying something that is not true.
    ///
    /// A press *on a control* does not come through here: that press moves the
    /// focus rather than removing it, and clearing it first would restyle the page
    /// twice for one press.
    pub fn blur(&mut self) -> bool {
        if self.interaction.focus.is_none()
            && self.interaction.open.is_none()
            && self.interaction.suggestion.is_none()
        {
            return false;
        }
        // A drag inside a field ends with the focus that owned it: the pointer is
        // somewhere else now, and the next move is not an extension of a selection
        // in a field nobody is in.
        self.field_dragging = false;
        self.field_anchor = None;
        self.sliding = None;
        self.set_interaction(otlyra_css::state::Interaction {
            focus: None,
            focus_visible: false,
            open: None,
            suggestion: None,
            ..self.interaction
        })
    }

    /// The keyboard has been used, so the focus is to be shown from now on.
    pub(super) fn show_ring(&mut self) -> bool {
        self.set_interaction(otlyra_css::state::Interaction {
            focus_visible: true,
            ..self.interaction
        })
    }

    /// Put the keyboard on `node`, as a reader asking for it does.
    ///
    /// The same focus the pointer and the tab key move, so what a reader is told
    /// holds the keyboard is what does. The ring is shown, because a reader who
    /// moved the focus without touching the page is in exactly the position the
    /// keyboard heuristic is for.
    pub fn focus_node(&mut self, node: NodeId) -> bool {
        if !self.is_focusable(node) {
            return false;
        }
        self.keyboard = true;
        let moved = self.set_interaction(otlyra_css::state::Interaction {
            focus: Some(node),
            focus_visible: true,
            ..self.interaction
        });
        self.caret = self.field_text(node).len();
        self.field_anchor = Some(self.caret);
        self.restart_caret();
        // Reached rather than clicked into: a date field hands the reader its
        // first part, which is where the next digit would go.
        if self.segments_of(node).is_some() {
            self.hold_segment(node, 0);
        }
        moved
    }
}
