//! An element's state, as the selectors that depend on it need it.
//!
//! `:hover` and `:checked` look alike in a stylesheet and come from opposite
//! places: one from where the pointer is, the other from the document. Both end
//! up in the same bitfield, because that is what the matcher reads and what the
//! invalidation machinery compares. This module is where the two are joined.
//!
//! The interaction half arrives from outside as [`Interaction`] — one element
//! under the pointer, one being pressed, one focused. It is not stored in the DOM:
//! the DOM does not know there is a pointer, and a document loaded twice into two
//! windows would have two answers.
//!
//! The chains matter and are the reason this is a type and not a function.
//! `:hover` matches the element under the pointer *and every ancestor of it*, and
//! so does `:active`; `:focus-within` matches the focused element's ancestors.
//! Asking "is this an ancestor of the hovered element" per element would walk the
//! tree once per element; the ancestors are collected once instead, when the
//! answer is first needed.

use std::collections::HashSet;

use otlyra_dom::form::{self, Control, InputKind};
use otlyra_dom::{Document, FormState, NodeId};
pub use stylo_dom::ElementState;

/// Where the pointer and the focus are.
///
/// All three are absent for a page nobody is pointing at, which is the state a
/// page is rendered in when it is screenshotted or printed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interaction {
    /// The innermost element under the pointer.
    pub hover: Option<NodeId>,
    /// The element the pointer was pressed on and has not been released over.
    pub active: Option<NodeId>,
    /// The element holding keyboard focus.
    pub focus: Option<NodeId>,
    /// The element the reader has opened, if any: a `<select>` showing its list.
    ///
    /// Interaction rather than markup — the same document in two windows has one
    /// `<select>` and two answers to whether it is showing — and it goes through
    /// the cascade like every other state, which is what makes `:open` on a
    /// drop-down mean what the specification says it means.
    pub open: Option<NodeId>,
    /// Whether the focus should be drawn — the `:focus-visible` decision, made
    /// where the input is routed rather than here, because it depends on what the
    /// reader did last rather than on what the document says.
    pub focus_visible: bool,
}

impl Interaction {
    /// Nothing hovered, pressed or focused.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// Answers `state_of` for every element of one document.
///
/// Built once per restyle. The ancestor chains are collected on construction
/// because they are three walks up from three nodes, against one walk up per
/// element if they were not.
pub struct States<'a> {
    document: &'a Document,
    form: &'a FormState,
    /// The hovered element and its ancestors.
    hover: HashSet<NodeId>,
    /// The pressed element and its ancestors.
    active: HashSet<NodeId>,
    /// The focused element's ancestors, the element itself excluded.
    focus_within: HashSet<NodeId>,
    /// The focused element.
    focus: Option<NodeId>,
    /// The element the reader has opened.
    open: Option<NodeId>,
    /// Whether the focus is drawn.
    focus_visible: bool,
}

impl<'a> States<'a> {
    /// Prepare to answer for `document`.
    #[must_use]
    pub fn new(document: &'a Document, form: &'a FormState, interaction: Interaction) -> Self {
        let hover = ancestors_and_self(document, interaction.hover);
        let active = ancestors_and_self(document, interaction.active);
        let mut focus_within = ancestors_and_self(document, interaction.focus);
        if let Some(focused) = interaction.focus {
            focus_within.remove(&focused);
        }
        Self {
            document,
            form,
            hover,
            active,
            focus_within,
            focus: interaction.focus,
            open: interaction.open,
            focus_visible: interaction.focus_visible,
        }
    }

    /// Every state bit `id` currently holds.
    #[must_use]
    pub fn state_of(&self, id: NodeId) -> ElementState {
        let document = self.document;
        let Some(element) = document.get(id).and_then(|node| node.element()) else {
            return ElementState::empty();
        };
        let mut state = ElementState::empty();

        // Nothing here is a custom element, so everything here is defined.
        state |= ElementState::DEFINED;

        if self.hover.contains(&id) {
            state |= ElementState::HOVER;
        }
        if self.active.contains(&id) {
            state |= ElementState::ACTIVE;
        }
        if self.focus == Some(id) {
            state |= ElementState::FOCUS;
            if self.focus_visible {
                state |= ElementState::FOCUSRING;
            }
        }
        if self.focus_within.contains(&id) || self.focus == Some(id) {
            state |= ElementState::FOCUS_WITHIN;
        }

        // A link is unvisited and stays unvisited: there is no history to ask, and
        // guessing would leak one if there were.
        if matches!(element.name.local.as_ref(), "a" | "area" | "link")
            && element.attr("href").is_some()
        {
            state |= ElementState::UNVISITED;
        }

        // `<details>` and `<dialog>` carry their openness in an attribute, which
        // makes `:open` the one element-display-state pseudo-class we can answer
        // without anything opening it.
        if matches!(element.name.local.as_ref(), "details" | "dialog")
            && element.attr("open").is_some()
        {
            state |= ElementState::OPEN;
        }
        if self.open == Some(id) {
            state |= ElementState::OPEN;
        }

        if form::is_read_write(document, id) {
            state |= ElementState::READWRITE;
        } else {
            // Everything that is not read-write is read-only, a `<div>` included.
            state |= ElementState::READONLY;
        }

        // A form and a fieldset are valid when everything they hold is: what
        // `:invalid` on a `<form>` means, and the only way a page can style the
        // whole of a form against the state of its parts.
        if matches!(element.name.local.as_ref(), "form" | "fieldset") {
            let holds_invalid = descendants_of(document, id).into_iter().any(|node| {
                form::is_validated(document, node)
                    && form::validity(document, self.form, node).is_invalid()
            });
            state |= if holds_invalid {
                ElementState::INVALID
            } else {
                ElementState::VALID
            };
        }

        let Some(control) = Control::of(document, id) else {
            return state;
        };

        if control.can_be_disabled() {
            if form::is_disabled(document, id) {
                state |= ElementState::DISABLED;
            } else {
                state |= ElementState::ENABLED;
            }
        }

        if form::can_be_required(document, id) {
            if form::is_required(document, id) {
                state |= ElementState::REQUIRED;
            } else {
                state |= ElementState::OPTIONAL_;
            }
        }

        let checked = match control {
            Control::Input(kind) if kind.is_checkable() => self.form.checkedness(document, id),
            Control::Option => self.form.selectedness(document, id),
            _ => false,
        };
        if checked {
            state |= ElementState::CHECKED;
        }

        if form::is_indeterminate(document, self.form, id) {
            state |= ElementState::INDETERMINATE;
        }
        if form::is_placeholder_shown(document, self.form, id) {
            state |= ElementState::PLACEHOLDER_SHOWN;
        }
        if form::is_default(document, id) {
            state |= ElementState::DEFAULT;
        }
        // What is wrong with what it holds, and whether the reader has been told.
        // `:invalid` is true from the moment a page with an empty required field
        // loads; `:user-invalid` waits until the reader has touched the control,
        // which is the difference between a form that reads as broken on arrival
        // and one that answers as it is filled in.
        if form::is_validated(document, id) {
            let validity = form::validity(document, self.form, id);
            if validity.is_invalid() {
                state |= ElementState::INVALID;
                if self.form.has_interacted(id) {
                    state |= ElementState::USER_INVALID;
                }
            } else {
                state |= ElementState::VALID;
                if self.form.has_interacted(id) {
                    state |= ElementState::USER_VALID;
                }
            }
        }
        match form::range_state(document, self.form, id) {
            Some(true) => state |= ElementState::INRANGE,
            Some(false) => state |= ElementState::OUTOFRANGE,
            None => {}
        }

        if control.is_text_entry() && self.form.value(document, id).is_empty() {
            state |= ElementState::VALUE_EMPTY;
        }
        if control == Control::Input(InputKind::Hidden) {
            // A hidden input is not interactive at all; drop what the pointer and
            // the focus would otherwise have put on it.
            state.remove(ElementState::HOVER | ElementState::ACTIVE | ElementState::FOCUS);
        }

        state
    }
}

/// Every element whose state can differ between two interactions.
///
/// Only the six chains can: an element nobody is pointing at, pressing or focusing
/// in either interaction holds the same bits in both, whatever else changed.
#[must_use]
pub fn touched_nodes(document: &Document, before: Interaction, after: Interaction) -> Vec<NodeId> {
    let mut touched: Vec<NodeId> = Vec::new();
    let mut seen = HashSet::new();
    for node in [
        before.hover,
        before.active,
        before.focus,
        before.open,
        after.hover,
        after.active,
        after.focus,
        after.open,
    ] {
        for id in ancestors_and_self(document, node) {
            if seen.insert(id) {
                touched.push(id);
            }
        }
    }
    touched
}

/// Every node under `root`, in tree order.
fn descendants_of(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack: Vec<NodeId> = document.children(root).collect();
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(document.children(node));
    }
    order
}

/// `node` and everything above it, or nothing when there is no node.
fn ancestors_and_self(document: &Document, node: Option<NodeId>) -> HashSet<NodeId> {
    let mut chain = HashSet::new();
    let mut current = node;
    while let Some(id) = current {
        if !chain.insert(id) {
            break;
        }
        current = document.get(id).and_then(|inner| inner.parent);
    }
    chain
}
