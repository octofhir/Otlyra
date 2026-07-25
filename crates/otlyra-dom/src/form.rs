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

    /// Whether this control holds a number the reader moves along a range.
    #[must_use]
    pub fn is_slider(self) -> bool {
        self == Self::Input(InputKind::Range)
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
    /// The files the reader chose, for a file picker.
    ///
    /// Nothing in the markup can put a file here: a page cannot name one, which is
    /// the whole point of the control, so this half has no attribute behind it at
    /// all.
    files: Vec<ChosenFile>,
    /// What a date or a time field is *showing* while it is being filled in.
    ///
    /// A partly typed date is not a value: HTML says such a control holds the
    /// empty string until every part of it is there, so what the reader has
    /// entered so far has nowhere else to live. Kept beside the value rather than
    /// in it, because the two say different things and a form must send the value.
    draft: Option<String>,
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
        let control = self.controls.entry(id).or_default();
        control.value = Some(value);
        // Whatever was half-typed is gone: a value written from outside — a reset,
        // a suggestion taken, a script one day — is the whole of what the control
        // shows.
        control.draft = None;
    }

    /// The files a picker holds.
    #[must_use]
    pub fn files(&self, id: NodeId) -> &[ChosenFile] {
        self.controls
            .get(&id)
            .map_or(&[], |control| control.files.as_slice())
    }

    /// Record what the reader chose.
    pub fn set_files(&mut self, id: NodeId, files: Vec<ChosenFile>) {
        self.controls.entry(id).or_default().files = files;
    }

    /// What a date or a time field is showing while it is being filled in.
    #[must_use]
    pub fn draft(&self, id: NodeId) -> Option<&str> {
        self.controls
            .get(&id)
            .and_then(|control| control.draft.as_deref())
    }

    /// Record what a date or a time field is showing, and what that comes to.
    ///
    /// The two are set together because they must agree: a complete draft *is* the
    /// value, and an incomplete one means the control holds nothing at all.
    pub fn set_draft(&mut self, id: NodeId, draft: String, value: String) {
        let control = self.controls.entry(id).or_default();
        control.value = Some(value);
        control.draft = Some(draft);
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

/// Whether a control takes its suggestions from a `<datalist>` as a list.
///
/// HTML gives the `list` attribute to the text-entry kinds and to the two that
/// hold a value without being typed into; everything else — a checkbox, a button,
/// a file — ignores it. A slider and a colour well are left out here because what
/// they are to show is marks on a track and swatches, not a list of words: they
/// would get a list that means nothing.
#[must_use]
pub fn takes_suggestions(document: &Document, id: NodeId) -> bool {
    matches!(Control::of(document, id), Some(Control::Input(kind)) if kind.is_text_entry())
}

/// The `<datalist>` a control's `list` attribute names, if it names one.
///
/// The attribute names an element by its id and the element has to be a
/// `<datalist>`: `list` pointing at a `<div>` leaves the control with no
/// suggestions rather than with that element's contents.
#[must_use]
pub fn suggestion_list(document: &Document, id: NodeId) -> Option<NodeId> {
    if !takes_suggestions(document, id) {
        return None;
    }
    let named = document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("list"))
        .map(str::to_owned)?;
    descendants(document, document.root())
        .into_iter()
        .find(|&node| {
            is_element(document, node, "datalist")
                && document
                    .get(node)
                    .and_then(|inner| inner.element())
                    .and_then(ElementIdExt::element_id)
                    == Some(named.as_str())
        })
}

/// Every suggestion a control can be filled in with.
///
/// A disabled option is not a suggestion, and neither is one that would put
/// nothing in the field — HTML says both are to be left out of the list a reader
/// is shown.
#[must_use]
pub fn suggestions_of(document: &Document, id: NodeId) -> Vec<NodeId> {
    let Some(list) = suggestion_list(document, id) else {
        return Vec::new();
    };
    options_of(document, list)
        .into_iter()
        .filter(|&option| {
            !is_disabled(document, option) && !option_value(document, option).is_empty()
        })
        .collect()
}

/// The suggestions a control shows for what it holds right now.
///
/// An empty field is offered everything; anything else narrows the list to the
/// suggestions that contain what has been typed, compared without case, which is
/// what both references do and what the specification leaves to the browser.
#[must_use]
pub fn suggestions_for(document: &Document, state: &FormState, id: NodeId) -> Vec<NodeId> {
    let typed = state.value(document, id).to_lowercase();
    suggestions_of(document, id)
        .into_iter()
        .filter(|&option| {
            typed.is_empty()
                || option_value(document, option)
                    .to_lowercase()
                    .contains(&typed)
        })
        .collect()
}

/// Whether an `<option>` is a suggestion rather than an answer.
#[must_use]
pub fn is_suggestion(document: &Document, option: NodeId) -> bool {
    has_ancestor(document, option, "datalist")
}

/// The control an `<option>` inside a `<datalist>` suggests to, if that control is
/// the one showing its list.
///
/// A `<datalist>` can be named by any number of controls, so the option belongs to
/// whichever of them is open rather than to one of them for good.
#[must_use]
pub fn suggested_control(
    document: &Document,
    option: NodeId,
    open: Option<NodeId>,
) -> Option<NodeId> {
    let open = open?;
    suggestions_of(document, open)
        .contains(&option)
        .then_some(open)
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

/// One file the reader handed to a page.
///
/// The bytes travel with the name because a form is sent long after the dialogue
/// closed, and by then the file may have moved: what was chosen is what is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChosenFile {
    /// What to call it in the submission — the last part of the path and no more
    /// of it. A form is not told where on the reader's disk a file came from.
    pub name: String,
    /// What kind of file it is, as far as its name says.
    pub media_type: String,
    /// The file itself.
    pub bytes: Vec<u8>,
}

/// What kind of file a name suggests.
///
/// A short table and a fallback, which is what the specification asks for when
/// nothing better is known: the type is a hint to whatever receives the form, and
/// `application/octet-stream` is the honest answer for anything unrecognised.
#[must_use]
pub fn media_type_of(name: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "text" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "woff2" => "font/woff2",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
    .to_owned()
}

/// What a file picker shows: the file it holds, or that it holds none.
///
/// Both references say the same words for an empty one and neither says the path,
/// which is the rule that matters: a page is told the name of a file and never
/// where it lives.
#[must_use]
pub fn file_label(state: &FormState, id: NodeId) -> String {
    let files = state.files(id);
    match files {
        [] => "No file chosen".to_owned(),
        [only] => only.name.clone(),
        many => format!("{} files", many.len()),
    }
}

/// Whether a file picker takes more than one file.
#[must_use]
pub fn takes_many_files(document: &Document, id: NodeId) -> bool {
    document
        .get(id)
        .and_then(|node| node.element())
        .is_some_and(|element| element.attr("multiple").is_some())
}

/// The `accept` attribute, split into the hints it holds.
///
/// Left exactly as the page spelled them — an extension, a type, a type with a
/// star — because what a dialogue does with them is the dialogue's business.
#[must_use]
pub fn accepted_files(document: &Document, id: NodeId) -> Vec<String> {
    document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("accept"))
        .map(|value| {
            value
                .split(',')
                .map(|hint| hint.trim().to_owned())
                .filter(|hint| !hint.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The colour a `<input type=color>` holds, as three bytes.
///
/// HTML sanitises this one hard: anything that is not a seven-character hex colour
/// is black, and there is no such thing as an empty colour well.
#[must_use]
pub fn color_value(document: &Document, state: &FormState, id: NodeId) -> [u8; 3] {
    let text = state.value(document, id);
    let hex = text.strip_prefix('#').unwrap_or("");
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return [0, 0, 0];
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
    [byte(0), byte(2), byte(4)]
}

/// One part of a date or a time, as the reader fills it in.
///
/// A date field is not one string being typed into: it is a row of numbers with
/// fixed widths and fixed places, and every key belongs to whichever of them the
/// reader is on. This is that row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// What the part means, which is what decides how far it counts.
    pub unit: Unit,
    /// How many characters it takes, filled or not.
    pub width: usize,
    /// Where it starts in the text the field shows.
    pub at: usize,
}

/// What one part of a date or a time counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// A year, four digits.
    Year,
    /// A month, one to twelve.
    Month,
    /// A day, one to thirty-one — the longest month, because which month it is
    /// may not have been filled in yet.
    Day,
    /// A week of the year, one to fifty-three.
    Week,
    /// An hour, midnight to twenty-three.
    Hour,
    /// A minute.
    Minute,
}

impl Unit {
    /// The lowest and highest this part counts to.
    #[must_use]
    pub fn bounds(self) -> (u32, u32) {
        match self {
            Self::Year => (1, 9999),
            Self::Month => (1, 12),
            Self::Day => (1, 31),
            Self::Week => (1, 53),
            Self::Hour => (0, 23),
            Self::Minute => (0, 59),
        }
    }
}

/// The shape a date or time field is filled in, and nothing for a field that is
/// plain text.
///
/// The order is the one HTML gives the *value* — year, then month, then day —
/// rather than a local one. Both references order the parts the way the reader's
/// system does and neither agrees with the other on this machine; the value's own
/// order is the one thing about it that is written down.
#[must_use]
pub fn temporal_pattern(kind: InputKind) -> Option<(&'static str, &'static [Segment])> {
    const fn segment(unit: Unit, width: usize, at: usize) -> Segment {
        Segment { unit, width, at }
    }
    const DATE: &[Segment] = &[
        segment(Unit::Year, 4, 0),
        segment(Unit::Month, 2, 5),
        segment(Unit::Day, 2, 8),
    ];
    const MONTH: &[Segment] = &[segment(Unit::Year, 4, 0), segment(Unit::Month, 2, 5)];
    const WEEK: &[Segment] = &[segment(Unit::Year, 4, 0), segment(Unit::Week, 2, 6)];
    const TIME: &[Segment] = &[segment(Unit::Hour, 2, 0), segment(Unit::Minute, 2, 3)];
    const LOCAL: &[Segment] = &[
        segment(Unit::Year, 4, 0),
        segment(Unit::Month, 2, 5),
        segment(Unit::Day, 2, 8),
        segment(Unit::Hour, 2, 11),
        segment(Unit::Minute, 2, 14),
    ];
    match kind {
        InputKind::Date => Some(("yyyy-mm-dd", DATE)),
        InputKind::Month => Some(("yyyy-mm", MONTH)),
        InputKind::Week => Some(("yyyy-Www", WEEK)),
        InputKind::Time => Some(("hh:mm", TIME)),
        InputKind::DatetimeLocal => Some(("yyyy-mm-dd hh:mm", LOCAL)),
        _ => None,
    }
}

/// What a date or time field shows: what has been filled in, over the pattern.
///
/// Neither the value nor the placeholder on its own. A field being filled in shows
/// both at once — the parts that are there and the shape of the ones that are
/// not — which is what makes it obvious that two more digits are wanted.
#[must_use]
pub fn temporal_display(document: &Document, state: &FormState, id: NodeId) -> Option<String> {
    let Some(Control::Input(kind)) = Control::of(document, id) else {
        return None;
    };
    let (pattern, segments) = temporal_pattern(kind)?;
    if let Some(draft) = state.draft(id) {
        return Some(draft.to_owned());
    }
    let value = state.value(document, id);
    Some(match parse_temporal(value, kind) {
        Some(parts) => write_temporal(pattern, segments, &parts),
        None => pattern.to_owned(),
    })
}

/// The numbers a date or time value holds, in the pattern's own order.
///
/// Read out of the text rather than trusted: a `value` attribute can say anything,
/// and HTML says a control whose value does not parse holds nothing.
#[must_use]
pub fn parse_temporal(value: &str, kind: InputKind) -> Option<Vec<u32>> {
    let (_, segments) = temporal_pattern(kind)?;
    let digits: Vec<u32> = value
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u32>().ok())
        .collect::<Option<Vec<u32>>>()?;
    if digits.len() != segments.len() {
        return None;
    }
    for (part, segment) in digits.iter().zip(segments) {
        let (low, high) = segment.unit.bounds();
        if *part < low || *part > high {
            return None;
        }
    }
    Some(digits)
}

/// The pattern with the numbers written into it.
#[must_use]
pub fn write_temporal(pattern: &str, segments: &[Segment], parts: &[u32]) -> String {
    let mut text: Vec<char> = pattern.chars().collect();
    for (segment, part) in segments.iter().zip(parts) {
        let digits = format!("{:0width$}", part, width = segment.width);
        for (offset, digit) in digits.chars().enumerate() {
            if let Some(slot) = text.get_mut(segment.at + offset) {
                *slot = digit;
            }
        }
    }
    text.into_iter().collect()
}

/// What a filled-in date or time comes to, or nothing while a part is missing.
///
/// HTML is strict about this: a control the reader has half filled in holds the
/// empty string, so a form sends nothing rather than sending half a date.
#[must_use]
pub fn temporal_value(display: &str, kind: InputKind) -> String {
    let Some((pattern, segments)) = temporal_pattern(kind) else {
        return String::new();
    };
    let shown: Vec<char> = display.chars().collect();
    let mut parts = Vec::new();
    for segment in segments {
        let text: String = shown
            .iter()
            .skip(segment.at)
            .take(segment.width)
            .copied()
            .collect();
        let Ok(number) = text.parse::<u32>() else {
            return String::new();
        };
        let (low, high) = segment.unit.bounds();
        if number < low || number > high {
            return String::new();
        }
        parts.push(number);
    }
    // A day the month does not have is not a date. Nothing else here needs a
    // calendar: the bounds cover every other part.
    if let Some(day) = segments
        .iter()
        .position(|segment| segment.unit == Unit::Day)
        .and_then(|at| parts.get(at).copied())
        && let Some(month) = segments
            .iter()
            .position(|segment| segment.unit == Unit::Month)
            .and_then(|at| parts.get(at).copied())
    {
        let year = parts.first().copied().unwrap_or(1);
        if day > days_in_month(year, month) {
            return String::new();
        }
    }
    let written = write_temporal(pattern, segments, &parts);
    // The value's own spelling, which differs from the shown one in one place: the
    // specification joins a local date and time with a `T`.
    if kind == InputKind::DatetimeLocal {
        written.replacen(' ', "T", 1)
    } else {
        written
    }
}

/// How many days a month has, leap years included.
#[must_use]
pub fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 31,
    }
}

/// How a `<meter>`'s value reads against the range it was told is good.
///
/// Three levels rather than a number, because that is what a meter says: the same
/// value is good on one meter and bad on another, and which it is depends on where
/// `optimum` sits rather than on how large the value is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    /// In the range the element calls best.
    #[default]
    Optimum,
    /// Outside it, but not on the far side of it.
    Suboptimal,
    /// On the far side of the range from the optimum.
    Poor,
}

/// The numbers a slider works in: its lowest, its highest and its step.
///
/// HTML's own defaults, and its own repair: a `max` below the `min` is raised to
/// it, so a slider never has a range that runs backwards.
#[must_use]
pub fn range_bounds(document: &Document, id: NodeId) -> (f64, f64, f64) {
    let number = |key: &str| {
        document
            .get(id)
            .and_then(|node| node.element())
            .and_then(|element| element.attr(key))
            .and_then(|text| text.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite())
    };
    let min = number("min").unwrap_or(0.0);
    let max = number("max").unwrap_or(100.0).max(min);
    // `any` turns stepping off; anything else that is not a positive number falls
    // back to one, which is the specification's default step for a slider.
    let step = document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.attr("step"))
        .map_or(Some(1.0), |text| {
            if text.trim().eq_ignore_ascii_case("any") {
                None
            } else {
                Some(
                    text.trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|value| value.is_finite() && *value > 0.0)
                        .unwrap_or(1.0),
                )
            }
        })
        .unwrap_or(0.0);
    (min, max, step)
}

/// What a slider holds: what it was told, or the middle of its range.
///
/// A slider always holds a number — there is no such thing as an empty one, which
/// is why an `<input type=range>` with no `value` still submits one.
#[must_use]
pub fn range_value(document: &Document, state: &FormState, id: NodeId) -> f64 {
    let (min, max, _) = range_bounds(document, id);
    let held = state
        .value(document, id)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite());
    held.unwrap_or(min + (max - min) / 2.0).clamp(min, max)
}

/// A slider's value moved to the nearest step and kept inside its range.
#[must_use]
pub fn snap_to_step(document: &Document, id: NodeId, value: f64) -> f64 {
    let (min, max, step) = range_bounds(document, id);
    if step <= 0.0 {
        return value.clamp(min, max);
    }
    // Counted from the `min` rather than from zero: that is the step base, and a
    // slider from 1 to 10 by 2 holds 1, 3, 5 and not 2, 4, 6.
    let steps = ((value - min) / step).round();
    (min + steps * step).clamp(min, max)
}

/// Where a slider's thumb sits, from 0 at the start of its range to 1 at the end.
#[must_use]
pub fn range_position(document: &Document, state: &FormState, id: NodeId) -> f64 {
    let (min, max, _) = range_bounds(document, id);
    if max <= min {
        return 0.0;
    }
    (range_value(document, state, id) - min) / (max - min)
}

/// How full a `<progress>` is, or `None` when it does not know.
///
/// A progress bar with no `value` is HTML's *indeterminate* bar: the task is going
/// on and how far along it is unknown, which is a different thing from empty.
#[must_use]
pub fn progress_position(document: &Document, id: NodeId) -> Option<f64> {
    let number = |key: &str| {
        document
            .get(id)
            .and_then(|node| node.element())
            .and_then(|element| element.attr(key))
            .and_then(|text| text.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite())
    };
    let value = number("value")?.max(0.0);
    let max = number("max").filter(|max| *max > 0.0).unwrap_or(1.0);
    Some((value / max).clamp(0.0, 1.0))
}

/// How full a `<meter>` is, and how its value reads.
///
/// The rule for the level is the specification's own and is about where `optimum`
/// sits rather than about the value: below the low mark the low end is the good
/// end, above the high mark the high end is, and between them both ends are merely
/// less good than the middle — which is why a meter that way round is never bad.
#[must_use]
pub fn meter_reading(document: &Document, id: NodeId) -> (f64, Level) {
    let number = |key: &str| {
        document
            .get(id)
            .and_then(|node| node.element())
            .and_then(|element| element.attr(key))
            .and_then(|text| text.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite())
    };
    let min = number("min").unwrap_or(0.0);
    let max = number("max").unwrap_or(1.0).max(min);
    let value = number("value").unwrap_or(0.0).clamp(min, max);
    let low = number("low").unwrap_or(min).clamp(min, max);
    let high = number("high").unwrap_or(max).clamp(low, max);
    let optimum = number("optimum")
        .unwrap_or(min + (max - min) / 2.0)
        .clamp(min, max);

    let level = if optimum < low {
        if value <= low {
            Level::Optimum
        } else if value <= high {
            Level::Suboptimal
        } else {
            Level::Poor
        }
    } else if optimum > high {
        if value >= high {
            Level::Optimum
        } else if value >= low {
            Level::Suboptimal
        } else {
            Level::Poor
        }
    } else if value >= low && value <= high {
        Level::Optimum
    } else {
        Level::Suboptimal
    };

    let position = if max <= min {
        0.0
    } else {
        (value - min) / (max - min)
    };
    (position, level)
}

/// A number as HTML writes one: no trailing zeros, and no point when there is
/// nothing after it.
#[must_use]
pub fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        let mut text = format!("{value}");
        if text.contains('.') {
            text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
        }
        text
    }
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
