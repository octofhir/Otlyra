//! A control described in words rather than in pixels, for the accessibility
//! tree.
//!
//! Its own module because it is read-only and for a different reader: nothing
//! here changes the page, and every answer comes from the markup and the form
//! state rather than from the widget that was drawn, so what a screen reader is
//! told and what is on screen cannot disagree.

use otlyra_dom::NodeId;

use crate::page::{PageScene, descendants_of};

/// What a control is, holds and is in the middle of, for whoever has to say it.
///
/// Built for the accessibility tree and useful to anything else that has to
/// describe a control in words rather than in pixels. Every field is read from the
/// document and the reader's own answers rather than from the widget that was
/// drawn: what a reader is told and what is on screen come from one place.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlFacts {
    /// Which kind of control it is.
    pub control: otlyra_dom::form::Control,
    /// The words beside it, from its `<label>`.
    pub label: Option<String>,
    /// What it holds, where holding something is meaningful.
    pub value: Option<String>,
    /// Ticked, for the two that can be.
    pub checked: Option<bool>,
    /// Chosen, for an option in a list.
    pub selected: Option<bool>,
    /// Whether nothing can reach it.
    pub disabled: bool,
    /// Whether it has to be filled in.
    pub required: bool,
    /// Whether what it holds fails the constraints, once the reader has been
    /// through it.
    pub invalid: bool,
    /// Whether the keyboard is on it.
    pub focused: bool,
    /// Whether pressing it would do anything.
    pub actionable: bool,
    /// Whether it offers a list of suggestions to fill it in with.
    pub suggests: bool,
    /// The number it holds and the range it holds it in, for the controls that
    /// are a number rather than words.
    pub numeric: Option<Numeric>,
}

/// A control's value as a number, with the range a reader announces it against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Numeric {
    /// What it holds.
    pub value: f64,
    /// The lowest it goes.
    pub min: f64,
    /// The highest.
    pub max: f64,
    /// How far one step moves it, where it steps at all.
    pub step: Option<f64>,
}

impl PageScene {
    /// What a control holds and what state it is in, for a reader that is told
    /// rather than shown.
    ///
    /// Answered from the same two places every other question about a control is
    /// answered from — the markup and the side table the reader's own answers live
    /// in — so a screen reader and the pixels can never disagree about whether a
    /// box is ticked.
    #[must_use]
    pub fn control_facts(&self, node: NodeId) -> Option<ControlFacts> {
        use otlyra_dom::form::{self, Control, InputKind};

        let control = Control::of(&self.document, node)?;
        let disabled = form::is_disabled(&self.document, node);
        let checkable = matches!(
            control,
            Control::Input(InputKind::Checkbox | InputKind::Radio)
        );
        Some(ControlFacts {
            control,
            label: self.label_for(node),
            value: match control {
                // What a checkbox holds is its state, and the state is said as a
                // state; saying `on` as well is a reader announcing a word nobody
                // put on the page.
                _ if checkable => None,
                Control::Select => self.chosen_option_text(node),
                _ if control.is_text_entry() => {
                    Some(self.form.value(&self.document, node).to_owned())
                }
                // A slider and the two bars hold a number and say it as one: a
                // reader that is told "fifty" can be told what it is out of, and a
                // slider with nothing typed into it still holds the middle of its
                // range.
                Control::Input(InputKind::Range) => Some(form::format_number(form::range_value(
                    &self.document,
                    &self.form,
                    node,
                ))),
                Control::Progress => form::progress_position(&self.document, node)
                    .map(|position| form::format_number((position * 100.0).round())),
                Control::Meter => Some(form::format_number(
                    (form::meter_reading(&self.document, node).0 * 100.0).round(),
                )),
                // A button's own words are its label rather than a value, and the
                // ones that carry them in `value` are the input-shaped ones.
                Control::Input(kind) if kind.is_button() => {
                    Some(self.form.value(&self.document, node).to_owned())
                }
                _ => None,
            },
            checked: checkable.then(|| self.form.checkedness(&self.document, node)),
            // A suggestion the arrows have reached reads as the chosen one, which
            // is what it looks like on screen and what a reader walking the list
            // has to be told.
            selected: matches!(control, Control::Option).then(|| {
                self.form.selectedness(&self.document, node)
                    || self.interaction.suggestion == Some(node)
            }),
            disabled,
            required: form::is_required(&self.document, node),
            // Only once the reader has been through it. A form that reads as broken
            // the moment it loads is a form nobody can fill in.
            invalid: self.form.has_interacted(node)
                && form::is_validated(&self.document, node)
                && form::validity(&self.document, &self.form, node).is_invalid(),
            focused: self.interaction.focus == Some(node),
            actionable: !disabled && form::is_mutable(&self.document, node),
            suggests: !form::suggestions_of(&self.document, node).is_empty(),
            // The three numbers a reader announces a slider by. A bar has them
            // too, and both references give them as a percentage of what it is
            // out of rather than in the page's own units.
            numeric: match control {
                Control::Input(InputKind::Range) => {
                    let (min, max, step) = form::range_bounds(&self.document, node);
                    Some(Numeric {
                        value: form::range_value(&self.document, &self.form, node),
                        min,
                        max,
                        step: (step > 0.0).then_some(step),
                    })
                }
                Control::Progress => {
                    form::progress_position(&self.document, node).map(|position| Numeric {
                        value: (position * 100.0).round(),
                        min: 0.0,
                        max: 100.0,
                        step: None,
                    })
                }
                Control::Meter => Some(Numeric {
                    value: (form::meter_reading(&self.document, node).0 * 100.0).round(),
                    min: 0.0,
                    max: 100.0,
                    step: None,
                }),
                _ => None,
            },
        })
    }

    /// The words beside a control: its `<label>`, wherever the label is.
    fn label_for(&self, node: NodeId) -> Option<String> {
        let root = self.document.root();
        descendants_of(&self.document, root)
            .into_iter()
            .filter(|&candidate| {
                self.document
                    .get(candidate)
                    .and_then(|inner| inner.element())
                    .is_some_and(|element| element.name.local.as_ref() == "label")
            })
            .find(|&label| otlyra_dom::form::labeled_control(&self.document, label) == Some(node))
            .map(|label| self.text_under(label))
            .filter(|text| !text.is_empty())
    }

    /// What a closed drop-down is showing.
    fn chosen_option_text(&self, select: NodeId) -> Option<String> {
        let options = otlyra_dom::form::options_of(&self.document, select);
        let chosen = options
            .iter()
            .copied()
            .find(|&option| self.form.selectedness(&self.document, option))
            .or_else(|| options.first().copied())?;
        Some(self.text_under(chosen))
    }

    /// All the text under a node, run together and trimmed.
    fn text_under(&self, node: NodeId) -> String {
        let mut out = String::new();
        for id in descendants_of(&self.document, node) {
            if let Some(otlyra_dom::NodeData::Text(text)) =
                self.document.get(id).map(|inner| &inner.data)
            {
                out.push_str(text);
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}
