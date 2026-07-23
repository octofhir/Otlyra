//! What a form control *is*, and what it currently holds.
//!
//! Two halves that must not be confused. The first is derived from the markup
//! and never changes while the document stands: which element is a control, what
//! kind, which radio buttons form a group, which control a `<label>` names. The
//! second is state the reader creates by using the page — a checkbox that has
//! been clicked, a field that has been typed into — and it lives in
//! [`FormState`], beside the document rather than in it.
//!
//! The split is the *dirty flag* HTML describes. A control's value is its
//! attribute until the reader changes it, and its own value afterwards; the same
//! for checkedness. Keeping the reader's half in a side table is what makes that
//! rule one lookup instead of a second copy of the DOM: an entry exists only for
//! a control the reader has touched, and resetting a form is dropping entries.
//!
//! Nothing here knows about the pointer, the keyboard or the cascade. It answers
//! questions about the document, and the answers are what the state pseudo-classes
//! and the activation behaviours are defined in terms of.

use std::collections::HashMap;

use crate::{Document, NodeId};

/// What an `<input>`'s `type` says it is.
///
/// The unknown-value default is `Text`, which is what HTML says an unrecognised
/// `type` falls back to — not "ignore the attribute".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// `type=text`, and anything unrecognised.
    Text,
    /// `type=search`.
    Search,
    /// `type=tel`.
    Tel,
    /// `type=url`.
    Url,
    /// `type=email`.
    Email,
    /// `type=password`.
    Password,
    /// `type=date`.
    Date,
    /// `type=month`.
    Month,
    /// `type=week`.
    Week,
    /// `type=time`.
    Time,
    /// `type=datetime-local`.
    DatetimeLocal,
    /// `type=number`.
    Number,
    /// `type=range`.
    Range,
    /// `type=color`.
    Color,
    /// `type=checkbox`.
    Checkbox,
    /// `type=radio`.
    Radio,
    /// `type=file`.
    File,
    /// `type=submit`.
    Submit,
    /// `type=image`.
    Image,
    /// `type=reset`.
    Reset,
    /// `type=button`.
    Button,
    /// `type=hidden`.
    Hidden,
}

impl InputKind {
    /// The kind an attribute value names.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        // ASCII case-insensitive, because that is how HTML compares keywords.
        let mut lowered = value.to_owned();
        lowered.make_ascii_lowercase();
        match lowered.as_str() {
            "search" => Self::Search,
            "tel" => Self::Tel,
            "url" => Self::Url,
            "email" => Self::Email,
            "password" => Self::Password,
            "date" => Self::Date,
            "month" => Self::Month,
            "week" => Self::Week,
            "time" => Self::Time,
            "datetime-local" => Self::DatetimeLocal,
            "number" => Self::Number,
            "range" => Self::Range,
            "color" => Self::Color,
            "checkbox" => Self::Checkbox,
            "radio" => Self::Radio,
            "file" => Self::File,
            "submit" => Self::Submit,
            "image" => Self::Image,
            "reset" => Self::Reset,
            "button" => Self::Button,
            "hidden" => Self::Hidden,
            _ => Self::Text,
        }
    }

    /// Whether this kind holds text the reader can edit.
    ///
    /// The mutability pseudo-classes and the `readonly` attribute are defined over
    /// exactly this set: a checkbox is never `:read-write`, however enabled it is.
    #[must_use]
    pub fn is_text_entry(self) -> bool {
        matches!(
            self,
            Self::Text
                | Self::Search
                | Self::Tel
                | Self::Url
                | Self::Email
                | Self::Password
                | Self::Date
                | Self::Month
                | Self::Week
                | Self::Time
                | Self::DatetimeLocal
                | Self::Number
        )
    }

    /// Whether this kind is drawn and behaves as a button.
    #[must_use]
    pub fn is_button(self) -> bool {
        matches!(
            self,
            Self::Submit | Self::Reset | Self::Button | Self::Image
        )
    }

    /// Whether this kind holds one of two states rather than a value.
    #[must_use]
    pub fn is_checkable(self) -> bool {
        matches!(self, Self::Checkbox | Self::Radio)
    }
}

/// What a node is, as a form control.
///
/// Only the elements the state pseudo-classes and the activation behaviours are
/// defined over. An element that is not one of these has no control semantics at
/// all, which is a different answer from having them and being disabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Control {
    /// An `<input>` of the given kind.
    Input(InputKind),
    /// A `<button>`.
    Button,
    /// A `<select>`.
    Select,
    /// A `<textarea>`.
    Textarea,
    /// An `<option>`.
    Option,
    /// An `<optgroup>`.
    Optgroup,
    /// An `<output>`.
    Output,
    /// A `<meter>`.
    Meter,
    /// A `<progress>`.
    Progress,
    /// A `<fieldset>`.
    Fieldset,
}

impl Control {
    /// What `id` is, if it is a control at all.
    #[must_use]
    pub fn of(document: &Document, id: NodeId) -> Option<Self> {
        let element = document.get(id)?.element()?;
        if element.name.ns != html5ever::ns!(html) {
            return None;
        }
        Some(match element.name.local.as_ref() {
            "input" => Self::Input(
                element
                    .attr("type")
                    .map_or(InputKind::Text, InputKind::parse),
            ),
            "button" => Self::Button,
            "select" => Self::Select,
            "textarea" => Self::Textarea,
            "option" => Self::Option,
            "optgroup" => Self::Optgroup,
            "output" => Self::Output,
            "meter" => Self::Meter,
            "progress" => Self::Progress,
            "fieldset" => Self::Fieldset,
            _ => return None,
        })
    }

    /// Whether a `<label>` can name this control.
    ///
    /// The list is HTML's *labelable elements*, which is narrower than the list of
    /// things a label can contain: an `<optgroup>` is a control and is not
    /// labelable.
    #[must_use]
    pub fn is_labelable(self) -> bool {
        match self {
            Self::Input(kind) => kind != InputKind::Hidden,
            Self::Button
            | Self::Select
            | Self::Textarea
            | Self::Output
            | Self::Meter
            | Self::Progress => true,
            Self::Option | Self::Optgroup | Self::Fieldset => false,
        }
    }

    /// Whether `disabled` means anything on this element.
    ///
    /// `:enabled` matches only what could have been disabled, so a `<div>` is
    /// neither enabled nor disabled — and neither is a `<meter>`.
    #[must_use]
    pub fn can_be_disabled(self) -> bool {
        matches!(
            self,
            Self::Input(_)
                | Self::Button
                | Self::Select
                | Self::Textarea
                | Self::Option
                | Self::Optgroup
                | Self::Fieldset
        )
    }

    /// Whether this control holds text the reader can edit.
    #[must_use]
    pub fn is_text_entry(self) -> bool {
        match self {
            Self::Input(kind) => kind.is_text_entry(),
            Self::Textarea => true,
            _ => false,
        }
    }
}

/// One control's half of its state: the half the reader made.
#[derive(Clone, Debug, Default)]
struct ControlValue {
    /// Set once the reader has changed the checkedness; the `checked` attribute
    /// stands until then.
    checked: Option<bool>,
    /// Set once the reader has typed; the `value` attribute stands until then.
    value: Option<String>,
    /// Set once the reader has changed the selection of an `<option>`.
    selected: Option<bool>,
    /// Whether the reader has done anything to this control.
    ///
    /// What separates `:user-invalid` from `:invalid`: an empty required field is
    /// invalid the moment the page loads, and telling the reader so before they
    /// have touched it is how a form turns red on arrival.
    interacted: bool,
}

/// The state a document's controls hold, beside the document.
///
/// Empty for a page nobody has touched: every answer then comes from the markup.
/// An entry appears when the reader changes something and disappears when the
/// form is reset, which is exactly what HTML's dirty flags describe.
#[derive(Clone, Debug, Default)]
pub struct FormState {
    controls: HashMap<NodeId, ControlValue>,
}

impl FormState {
    /// No reader has touched anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything, as loading a new document does.
    pub fn clear(&mut self) {
        self.controls.clear();
    }

    /// Whether anything has been touched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.controls.is_empty()
    }

    /// Whether `id` is checked: what the reader made it, or what the markup says.
    #[must_use]
    pub fn checkedness(&self, document: &Document, id: NodeId) -> bool {
        if let Some(checked) = self.controls.get(&id).and_then(|value| value.checked) {
            return checked;
        }
        document
            .get(id)
            .and_then(|node| node.element())
            .is_some_and(|element| element.attr("checked").is_some())
    }

    /// Set `id`'s checkedness, and remember that the reader set it.
    pub fn set_checkedness(&mut self, id: NodeId, checked: bool) {
        self.controls.entry(id).or_default().checked = Some(checked);
    }

    /// Whether `id` is a selected `<option>`.
    ///
    /// Without a reader's answer this is the `selected` attribute, except that a
    /// drop-down with nothing selected shows its first option — so that option is
    /// selected even though nothing in the markup says so.
    #[must_use]
    pub fn selectedness(&self, document: &Document, id: NodeId) -> bool {
        if let Some(selected) = self.controls.get(&id).and_then(|value| value.selected) {
            return selected;
        }
        let Some(element) = document.get(id).and_then(|node| node.element()) else {
            return false;
        };
        if element.attr("selected").is_some() {
            return true;
        }
        let Some(select) = owning_select(document, id) else {
            return false;
        };
        if is_multiple(document, select) || display_size(document, select) > 1 {
            return false;
        }
        // A single-line select always shows something. Its first option that is
        // not disabled is what it shows, and only when nothing else claimed it.
        options_of(document, select)
            .into_iter()
            .find(|&option| !is_disabled(document, option))
            .is_some_and(|first| {
                first == id
                    && !options_of(document, select)
                        .into_iter()
                        .any(|option| self.explicitly_selected(document, option))
            })
    }

    /// Whether `option` is selected by the markup or by the reader, ignoring the
    /// rule that gives a drop-down a first option.
    fn explicitly_selected(&self, document: &Document, option: NodeId) -> bool {
        if let Some(selected) = self.controls.get(&option).and_then(|value| value.selected) {
            return selected;
        }
        document
            .get(option)
            .and_then(|node| node.element())
            .is_some_and(|element| element.attr("selected").is_some())
    }

    /// Set an `<option>`'s selectedness.
    pub fn set_selectedness(&mut self, id: NodeId, selected: bool) {
        self.controls.entry(id).or_default().selected = Some(selected);
    }

    /// What `id` holds: what the reader typed, or what the markup says.
    ///
    /// A `<textarea>`'s markup value is its text content rather than an attribute,
    /// which is the one place the two halves are read from different places.
    #[must_use]
    pub fn value<'a>(&'a self, document: &'a Document, id: NodeId) -> &'a str {
        if let Some(value) = self
            .controls
            .get(&id)
            .and_then(|value| value.value.as_deref())
        {
            return value;
        }
        default_value(document, id)
    }

    /// Whether the reader has typed into `id`.
    #[must_use]
    pub fn is_dirty(&self, id: NodeId) -> bool {
        self.controls
            .get(&id)
            .is_some_and(|value| value.value.is_some())
    }

    /// Set what `id` holds, and remember that the reader set it.
    pub fn set_value(&mut self, id: NodeId, value: String) {
        self.controls.entry(id).or_default().value = Some(value);
    }

    /// Note that the reader has done something to this control.
    pub fn note_interaction(&mut self, id: NodeId) {
        self.controls.entry(id).or_default().interacted = true;
    }

    /// Whether the reader has done anything to this control.
    #[must_use]
    pub fn has_interacted(&self, id: NodeId) -> bool {
        self.controls.get(&id).is_some_and(|value| value.interacted)
    }

    /// Drop everything the reader did to the controls owned by `form`.
    ///
    /// This is what a reset button does: not "set every value to empty", but "let
    /// the markup speak again".
    pub fn reset(&mut self, document: &Document, form: Option<NodeId>) {
        self.controls
            .retain(|&id, _| form_owner(document, id) != form);
    }
}

/// The text `id` holds when the reader has not typed anything.
#[must_use]
pub fn default_value(document: &Document, id: NodeId) -> &str {
    let Some(element) = document.get(id).and_then(|node| node.element()) else {
        return "";
    };
    if element.name.local.as_ref() == "textarea" {
        // The child text of a `<textarea>` is its value, and it is the only
        // control whose value is written as content rather than as an attribute.
        return document
            .children(id)
            .find_map(|child| match &document.get(child)?.data {
                crate::NodeData::Text(text) => Some(&**text),
                _ => None,
            })
            .unwrap_or("");
    }
    element.attr("value").unwrap_or("")
}

/// Whether `id` is disabled — by its own attribute or by an ancestor `<fieldset>`.
///
/// A disabled `<fieldset>` disables its descendants, except those inside its
/// *first* `<legend>`: the legend is how a reader turns the fieldset back on, so
/// disabling it would be a trap.
#[must_use]
pub fn is_disabled(document: &Document, id: NodeId) -> bool {
    let Some(control) = Control::of(document, id) else {
        return false;
    };
    if !control.can_be_disabled() {
        return false;
    }
    if document
        .get(id)
        .and_then(|node| node.element())
        .is_some_and(|element| element.attr("disabled").is_some())
    {
        return true;
    }
    // An `<option>` is disabled by its `<optgroup>` as well.
    if control == Control::Option
        && document
            .get(id)
            .and_then(|node| node.parent)
            .filter(|&parent| is_element(document, parent, "optgroup"))
            .is_some_and(|group| {
                document
                    .get(group)
                    .and_then(|node| node.element())
                    .is_some_and(|element| element.attr("disabled").is_some())
            })
    {
        return true;
    }

    let mut child = id;
    let mut ancestor = document.get(id).and_then(|node| node.parent);
    while let Some(current) = ancestor {
        if is_element(document, current, "fieldset")
            && document
                .get(current)
                .and_then(|node| node.element())
                .is_some_and(|element| element.attr("disabled").is_some())
        {
            let first_legend = document
                .children(current)
                .find(|&node| is_element(document, node, "legend"));
            if first_legend != Some(child) {
                return true;
            }
        }
        child = current;
        ancestor = document.get(current).and_then(|node| node.parent);
    }
    false
}

/// Whether `id` can be changed by the reader at all.
///
/// HTML's *mutable*: neither disabled nor read-only. Activation and editing are
/// both defined in terms of it.
#[must_use]
pub fn is_mutable(document: &Document, id: NodeId) -> bool {
    !is_disabled(document, id) && !has_readonly_attribute(document, id)
}

/// Whether `readonly` is both applicable to `id` and set on it.
#[must_use]
pub fn has_readonly_attribute(document: &Document, id: NodeId) -> bool {
    let Some(control) = Control::of(document, id) else {
        return false;
    };
    if !control.is_text_entry() {
        return false;
    }
    document
        .get(id)
        .and_then(|node| node.element())
        .is_some_and(|element| element.attr("readonly").is_some())
}

/// Whether `id` matches `:read-write` — a text control the reader may edit, or an
/// editing host.
///
/// Everything else is `:read-only`, including a `<div>`, which is why the two are
/// not each other's negation over controls alone.
#[must_use]
pub fn is_read_write(document: &Document, id: NodeId) -> bool {
    if let Some(control) = Control::of(document, id)
        && control.is_text_entry()
    {
        return is_mutable(document, id);
    }
    document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("contenteditable"))
        .is_some_and(|value| !value.eq_ignore_ascii_case("false"))
}

/// Whether `id` is a control that `required` applies to.
#[must_use]
pub fn can_be_required(document: &Document, id: NodeId) -> bool {
    match Control::of(document, id) {
        Some(Control::Input(kind)) => {
            kind.is_text_entry() || kind.is_checkable() || kind == InputKind::File
        }
        Some(Control::Select | Control::Textarea) => true,
        _ => false,
    }
}

/// Whether `id` is required.
#[must_use]
pub fn is_required(document: &Document, id: NodeId) -> bool {
    can_be_required(document, id)
        && document
            .get(id)
            .and_then(|node| node.element())
            .is_some_and(|element| element.attr("required").is_some())
}

/// Whether `id` is showing its placeholder rather than a value.
#[must_use]
pub fn is_placeholder_shown(document: &Document, state: &FormState, id: NodeId) -> bool {
    let Some(control) = Control::of(document, id) else {
        return false;
    };
    if !control.is_text_entry() {
        return false;
    }
    let has_placeholder = document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("placeholder"))
        .is_some_and(|value| !value.is_empty());
    has_placeholder && state.value(document, id).is_empty()
}

/// Whether `id` is what the markup nominated: a checkbox or radio with `checked`,
/// an `<option>` with `selected`, or a form's first submit button.
#[must_use]
pub fn is_default(document: &Document, id: NodeId) -> bool {
    let Some(control) = Control::of(document, id) else {
        return false;
    };
    let element = match document.get(id).and_then(|node| node.element()) {
        Some(element) => element,
        None => return false,
    };
    match control {
        Control::Input(kind) if kind.is_checkable() => element.attr("checked").is_some(),
        Control::Option => element.attr("selected").is_some(),
        Control::Input(kind) if kind == InputKind::Submit || kind == InputKind::Image => {
            default_button(document, form_owner(document, id)) == Some(id)
        }
        Control::Button => {
            is_submit_button(document, id)
                && default_button(document, form_owner(document, id)) == Some(id)
        }
        _ => false,
    }
}

/// Whether `id` is `:indeterminate`: a radio button whose group holds nothing
/// checked, or a `<progress>` with no value of its own.
///
/// The third way — a checkbox a script made indeterminate — needs a script.
#[must_use]
pub fn is_indeterminate(document: &Document, state: &FormState, id: NodeId) -> bool {
    match Control::of(document, id) {
        Some(Control::Input(InputKind::Radio)) => !radio_group(document, id)
            .into_iter()
            .any(|member| state.checkedness(document, member)),
        Some(Control::Progress) => document
            .get(id)
            .and_then(|node| node.element())
            .is_some_and(|element| element.attr("value").is_none()),
        _ => false,
    }
}

/// Every radio button in `id`'s group, `id` included.
///
/// The group is HTML's: the same tree, the same form owner, `type=radio`, and a
/// non-empty `name` that matches. A radio without a name is a group of one, which
/// is why the answer is never empty.
#[must_use]
pub fn radio_group(document: &Document, id: NodeId) -> Vec<NodeId> {
    if Control::of(document, id) != Some(Control::Input(InputKind::Radio)) {
        return vec![id];
    }
    let name = document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("name"))
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let Some(name) = name else {
        return vec![id];
    };
    let owner = form_owner(document, id);

    let mut group = Vec::new();
    let mut stack = vec![document.root()];
    let mut order = Vec::new();
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(
            document
                .children(node)
                .collect::<Vec<_>>()
                .into_iter()
                .rev(),
        );
    }
    for node in order {
        if Control::of(document, node) != Some(Control::Input(InputKind::Radio)) {
            continue;
        }
        let matches_name = document
            .get(node)
            .and_then(|inner| inner.element())
            .and_then(|element| element.attr("name"))
            .is_some_and(|other| other == name);
        if matches_name && form_owner(document, node) == owner {
            group.push(node);
        }
    }
    if group.is_empty() { vec![id] } else { group }
}

/// The control a `<label>` names, if any.
///
/// `for` wins over containment, and a `for` that names nothing labelable leaves
/// the label with no control at all rather than falling back to a descendant.
#[must_use]
pub fn labeled_control(document: &Document, label: NodeId) -> Option<NodeId> {
    if !is_element(document, label, "label") {
        return None;
    }
    let target = document
        .get(label)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("for"))
        .map(str::to_owned);

    if let Some(target) = target {
        let found = descendants(document, document.root())
            .into_iter()
            .find(|&node| {
                document
                    .get(node)
                    .and_then(|inner| inner.element())
                    .and_then(ElementIdExt::element_id)
                    == Some(target.as_str())
            })?;
        return Control::of(document, found)
            .filter(|control| control.is_labelable())
            .map(|_| found);
    }

    descendants(document, label)
        .into_iter()
        .find(|&node| Control::of(document, node).is_some_and(Control::is_labelable))
}

/// The `<form>` that owns `id`, by its `form` attribute or by containment.
#[must_use]
pub fn form_owner(document: &Document, id: NodeId) -> Option<NodeId> {
    let named = document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("form"))
        .map(str::to_owned);
    if let Some(named) = named {
        return descendants(document, document.root())
            .into_iter()
            .find(|&node| {
                is_element(document, node, "form")
                    && document
                        .get(node)
                        .and_then(|inner| inner.element())
                        .and_then(ElementIdExt::element_id)
                        == Some(named.as_str())
            });
    }
    let mut ancestor = document.get(id).and_then(|node| node.parent);
    while let Some(current) = ancestor {
        if is_element(document, current, "form") {
            return Some(current);
        }
        ancestor = document.get(current).and_then(|node| node.parent);
    }
    None
}

/// Whether `id` submits the form it belongs to when it is activated.
#[must_use]
pub fn is_submit_button(document: &Document, id: NodeId) -> bool {
    match Control::of(document, id) {
        Some(Control::Input(kind)) => matches!(kind, InputKind::Submit | InputKind::Image),
        Some(Control::Button) => document
            .get(id)
            .and_then(|node| node.element())
            .and_then(|element| element.attr("type"))
            .is_none_or(|kind| kind.eq_ignore_ascii_case("submit")),
        _ => false,
    }
}

/// The first submit button of `form` in tree order.
#[must_use]
pub fn default_button(document: &Document, form: Option<NodeId>) -> Option<NodeId> {
    let form = form?;
    descendants(document, document.root())
        .into_iter()
        .find(|&node| is_submit_button(document, node) && form_owner(document, node) == Some(form))
}

/// Every `<option>` a `<select>` holds, in tree order, through `<optgroup>`.
#[must_use]
pub fn options_of(document: &Document, select: NodeId) -> Vec<NodeId> {
    descendants(document, select)
        .into_iter()
        .filter(|&node| is_element(document, node, "option"))
        .collect()
}

/// The `<select>` an `<option>` belongs to.
#[must_use]
pub fn owning_select(document: &Document, option: NodeId) -> Option<NodeId> {
    let mut ancestor = document.get(option).and_then(|node| node.parent);
    while let Some(current) = ancestor {
        if is_element(document, current, "select") {
            return Some(current);
        }
        if !is_element(document, current, "optgroup") {
            return None;
        }
        ancestor = document.get(current).and_then(|node| node.parent);
    }
    None
}

/// Whether a `<select>` takes more than one answer.
#[must_use]
pub fn is_multiple(document: &Document, select: NodeId) -> bool {
    document
        .get(select)
        .and_then(|node| node.element())
        .is_some_and(|element| element.attr("multiple").is_some())
}

/// How many rows a `<select>` shows: its `size`, or one, or four when it takes
/// more than one answer.
#[must_use]
pub fn display_size(document: &Document, select: NodeId) -> u32 {
    let declared = document
        .get(select)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("size"))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|&size| size > 0);
    declared.unwrap_or(if is_multiple(document, select) { 4 } else { 1 })
}

/// Whether `id` is an HTML element with the given local name.
fn is_element(document: &Document, id: NodeId, name: &str) -> bool {
    document
        .get(id)
        .and_then(|node| node.element())
        .is_some_and(|element| {
            element.name.ns == html5ever::ns!(html) && element.name.local.as_ref() == name
        })
}

/// Every node under `root`, in tree order, `root` excluded.
fn descendants(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack: Vec<NodeId> = document.children(root).collect::<Vec<_>>();
    stack.reverse();
    while let Some(node) = stack.pop() {
        order.push(node);
        let children: Vec<NodeId> = document.children(node).collect();
        stack.extend(children.into_iter().rev());
    }
    order
}

/// Lets `id()` be named as a function in an `and_then`, where the inherent method
/// would need a closure.
trait ElementIdExt {
    fn element_id(&self) -> Option<&str>;
}

impl ElementIdExt for crate::ElementData {
    fn element_id(&self) -> Option<&str> {
        self.id()
    }
}

/// Why a control's value is not acceptable.
///
/// HTML calls these *validity states*, and a control is invalid when it holds any
/// of them. They are separate rather than one flag because a page may style each,
/// and because "this is required" and "this is not a number" are different things
/// to tell a reader.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Validity {
    /// Required, and holding nothing.
    pub value_missing: bool,
    /// Not the shape the `type` asks for: an address that is not one.
    pub type_mismatch: bool,
    /// Longer than `maxlength`, and the reader made it so.
    pub too_long: bool,
    /// Shorter than `minlength`, and the reader made it so.
    pub too_short: bool,
    /// Below `min`.
    pub range_underflow: bool,
    /// Above `max`.
    pub range_overflow: bool,
    /// Not on one of the steps `step` allows.
    pub step_mismatch: bool,
    /// Not a value of this kind at all: letters in a number.
    pub bad_input: bool,
}

impl Validity {
    /// Whether anything is wrong.
    #[must_use]
    pub fn is_invalid(self) -> bool {
        self != Self::default()
    }
}

/// Whether `id` is a control whose value is checked at all.
///
/// HTML calls the ones that are not *barred from constraint validation*: a button
/// has nothing to check, a disabled control is not submitted, a read-only one
/// cannot be corrected, and a control inside a `<datalist>` is a suggestion rather
/// than an answer.
#[must_use]
pub fn is_validated(document: &Document, id: NodeId) -> bool {
    let Some(control) = Control::of(document, id) else {
        return false;
    };
    match control {
        Control::Input(kind) => {
            if kind.is_button() || kind == InputKind::Hidden {
                return false;
            }
        }
        Control::Select | Control::Textarea => {}
        _ => return false,
    }
    if is_disabled(document, id) || has_readonly_attribute(document, id) {
        return false;
    }
    !has_ancestor(document, id, "datalist")
}

/// What is wrong with what `id` holds, if anything.
#[must_use]
pub fn validity(document: &Document, state: &FormState, id: NodeId) -> Validity {
    let mut validity = Validity::default();
    if !is_validated(document, id) {
        return validity;
    }
    let Some(control) = Control::of(document, id) else {
        return validity;
    };
    let attribute = |key: &str| {
        document
            .get(id)
            .and_then(|node| node.element())
            .and_then(|element| element.attr(key))
    };
    let value = state.value(document, id);

    // Required and holding nothing. What "nothing" is differs by kind: a checkbox
    // is missing when it is not ticked, a radio group when none of it is, and a
    // select when what is chosen is the empty placeholder option.
    if is_required(document, id) {
        validity.value_missing = match control {
            Control::Input(InputKind::Checkbox) => !state.checkedness(document, id),
            Control::Input(InputKind::Radio) => !radio_group(document, id)
                .into_iter()
                .any(|member| state.checkedness(document, member)),
            Control::Select => options_of(document, id)
                .into_iter()
                .find(|&option| state.selectedness(document, option))
                .is_none_or(|option| option_value(document, option).is_empty()),
            _ => value.is_empty(),
        };
    }

    if value.is_empty() {
        return validity;
    }

    if let Control::Input(kind) = control {
        match kind {
            // One `@`, something on each side of it, and a dot in the host. Not the
            // whole of the specification's address grammar, which no reader has
            // ever typed the edges of, but the part that catches a mistake.
            InputKind::Email => {
                validity.type_mismatch = !looks_like_address(value);
            }
            // An absolute address: a scheme and something after it.
            InputKind::Url => {
                validity.type_mismatch = !looks_like_url(value);
            }
            InputKind::Number | InputKind::Range => {
                match value.trim().parse::<f64>() {
                    Err(_) => validity.bad_input = true,
                    Ok(number) => {
                        let min = attribute("min").and_then(|text| text.trim().parse::<f64>().ok());
                        let max = attribute("max").and_then(|text| text.trim().parse::<f64>().ok());
                        if let Some(min) = min {
                            validity.range_underflow = number < min;
                        }
                        if let Some(max) = max {
                            validity.range_overflow = number > max;
                        }
                        // A step of `any` allows everything; the default step for
                        // both of these kinds is one.
                        let step = attribute("step").map_or(Some(1.0), |text| {
                            if text.trim().eq_ignore_ascii_case("any") {
                                None
                            } else {
                                text.trim().parse::<f64>().ok().filter(|&step| step > 0.0)
                            }
                        });
                        if let Some(step) = step {
                            let base = min.unwrap_or(0.0);
                            let steps = (number - base) / step;
                            validity.step_mismatch = (steps - steps.round()).abs() > 1e-9;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Length is counted in characters rather than bytes, and only against what the
    // reader typed: a `value` attribute longer than `maxlength` is the page's own
    // doing and is not the reader's to correct.
    if state.is_dirty(id) {
        let characters = value.chars().count();
        if let Some(most) =
            attribute("maxlength").and_then(|text| text.trim().parse::<usize>().ok())
        {
            validity.too_long = characters > most;
        }
        if let Some(least) =
            attribute("minlength").and_then(|text| text.trim().parse::<usize>().ok())
        {
            validity.too_short = characters > 0 && characters < least;
        }
    }

    validity
}

/// Whether a numeric control's value is between its `min` and its `max`.
///
/// Separate from validity because `:in-range` and `:out-of-range` match only where
/// there is a range at all: a field with neither is in no range and out of none.
#[must_use]
pub fn range_state(document: &Document, state: &FormState, id: NodeId) -> Option<bool> {
    let Some(Control::Input(kind)) = Control::of(document, id) else {
        return None;
    };
    if !matches!(
        kind,
        InputKind::Number | InputKind::Range | InputKind::Date | InputKind::Time
    ) {
        return None;
    }
    let attribute = |key: &str| {
        document
            .get(id)
            .and_then(|node| node.element())
            .and_then(|element| element.attr(key))
    };
    if attribute("min").is_none() && attribute("max").is_none() {
        return None;
    }
    if !is_validated(document, id) {
        return None;
    }
    let value = state.value(document, id).trim().parse::<f64>().ok()?;
    let under = attribute("min")
        .and_then(|text| text.trim().parse::<f64>().ok())
        .is_some_and(|min| value < min);
    let over = attribute("max")
        .and_then(|text| text.trim().parse::<f64>().ok())
        .is_some_and(|max| value > max);
    Some(!under && !over)
}

/// What an `<option>` submits: its `value`, or its text when it has none.
#[must_use]
pub fn option_value(document: &Document, option: NodeId) -> String {
    document
        .get(option)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("value"))
        .map_or_else(
            || text_content(document, option).trim().to_owned(),
            str::to_owned,
        )
}

/// All the text under a node, run together.
fn text_content(document: &Document, node: NodeId) -> String {
    let mut out = String::new();
    for id in descendants(document, node) {
        if let Some(crate::NodeData::Text(text)) = document.get(id).map(|inner| &inner.data) {
            out.push_str(text);
        }
    }
    out
}

/// Whether `id` has an ancestor with the given local name.
fn has_ancestor(document: &Document, id: NodeId, name: &str) -> bool {
    let mut ancestor = document.get(id).and_then(|node| node.parent);
    while let Some(current) = ancestor {
        if is_element(document, current, name) {
            return true;
        }
        ancestor = document.get(current).and_then(|node| node.parent);
    }
    false
}

/// One `@`, something on each side, and a dot in the host.
fn looks_like_address(value: &str) -> bool {
    let mut parts = value.split('@');
    let (Some(local), Some(host), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !host.is_empty()
        && host.contains('.')
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !value.contains(char::is_whitespace)
}

/// A scheme, a colon, and something after it.
fn looks_like_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !value.contains(char::is_whitespace)
}
