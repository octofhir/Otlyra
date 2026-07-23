//! What a form sends, and how it is spelled.
//!
//! Two halves, and HTML keeps them apart on purpose. The *entry list* is what the
//! form holds: a list of name-and-value pairs, in tree order, built by a rule that
//! says exactly which controls contribute and which do not — a disabled one does
//! not, an unticked checkbox does not, a button does not unless it is the one that
//! was pressed. The *encoding* is how that list becomes bytes, and there are three
//! of them, chosen by the form's `enctype`.
//!
//! Neither half needs a script. A form that submits is a form that navigates: a
//! `GET` puts the pairs in the address and follows it, a `POST` sends them as the
//! body. That is the whole of it, and it is what every form on the web did before
//! there was anything else.

use crate::form::{self, Control, InputKind};
use crate::{Document, FormState, NodeId};

/// How a form spells what it holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    /// `application/x-www-form-urlencoded`: the default, and what a `GET` always
    /// uses whatever the form says.
    #[default]
    UrlEncoded,
    /// `multipart/form-data`: what a form carrying a file has to use.
    Multipart,
    /// `text/plain`: rare, and defined by HTML as name, `=`, value, newline.
    Plain,
}

impl Encoding {
    /// What an `enctype` attribute names. Anything unrecognised is the default,
    /// which is what HTML says an invalid value falls back to.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let value = value.trim();
        if value.eq_ignore_ascii_case("multipart/form-data") {
            Self::Multipart
        } else if value.eq_ignore_ascii_case("text/plain") {
            Self::Plain
        } else {
            Self::UrlEncoded
        }
    }

    /// The `Content-Type` a body of this encoding is sent under.
    #[must_use]
    pub fn content_type(self, boundary: &str) -> String {
        match self {
            Self::UrlEncoded => "application/x-www-form-urlencoded".to_owned(),
            Self::Multipart => format!("multipart/form-data; boundary={boundary}"),
            Self::Plain => "text/plain".to_owned(),
        }
    }
}

/// Which way a form's data travels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    /// In the address.
    #[default]
    Get,
    /// In the body.
    Post,
    /// Neither: it closes a dialog. Nothing opens one here, so nothing sends one.
    Dialog,
}

impl Method {
    /// What a `method` attribute names.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let value = value.trim();
        if value.eq_ignore_ascii_case("post") {
            Self::Post
        } else if value.eq_ignore_ascii_case("dialog") {
            Self::Dialog
        } else {
            Self::Get
        }
    }
}

/// One name-and-value pair a form holds.
///
/// A pair rather than a tuple because a value is not always text: a file picker
/// contributes a file, and what a file comes to depends on the encoding — its
/// name in an address, its contents in a multipart body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What the control is called.
    pub name: String,
    /// What it holds.
    pub value: EntryValue,
}

impl Entry {
    /// A plain pair.
    #[must_use]
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: EntryValue::Text(value.into()),
        }
    }

    /// What this entry comes to where only text will do.
    ///
    /// A file is its name and nothing else, which is what a form sends when it is
    /// spelled any way but multipart — the specification is explicit that the
    /// contents are lost there rather than smuggled in.
    #[must_use]
    pub fn as_text(&self) -> &str {
        match &self.value {
            EntryValue::Text(text) => text,
            EntryValue::File(file) => &file.name,
        }
    }
}

/// What one entry holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryValue {
    /// Text, which is nearly everything.
    Text(String),
    /// A file the reader chose.
    File(form::ChosenFile),
}

/// A form, ready to be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    /// Where to, exactly as the markup spells it: resolving it against the
    /// document's own address is the navigating layer's business.
    pub action: String,
    /// Which way.
    pub method: Method,
    /// The address to go to, for a `GET` — the action with the pairs in its query,
    /// whatever query it had before.
    pub url: String,
    /// The body, for a `POST`. Empty for a `GET`.
    pub body: Vec<u8>,
    /// What the body is, for a `POST`.
    pub content_type: String,
}

/// The pairs a form holds, in tree order.
///
/// `submitter` is the button that was pressed, which is the one button whose own
/// name and value are sent — every other one is left out, because a form with two
/// submit buttons has to say which was used.
#[must_use]
pub fn entry_list(
    document: &Document,
    state: &FormState,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for field in descendants(document, document.root()) {
        if form::form_owner(document, field) != Some(form) {
            continue;
        }
        let Some(control) = Control::of(document, field) else {
            continue;
        };
        // A suggestion is not an answer, and neither is a control nothing can
        // reach.
        if in_datalist(document, field) || form::is_disabled(document, field) {
            continue;
        }
        match control {
            // A hidden input is submitted; it is only hidden. A button is left out
            // unless it is the one that was pressed, an unticked box has nothing to
            // say, and a file picker has offered nothing to send.
            Control::Input(InputKind::Hidden) => {}
            Control::Input(kind)
                if (kind.is_button() && Some(field) != submitter)
                    || (kind.is_checkable() && !state.checkedness(document, field)) =>
            {
                continue;
            }
            Control::Input(_) => {}
            Control::Button => {
                if Some(field) != submitter || !form::is_submit_button(document, field) {
                    continue;
                }
            }
            Control::Select | Control::Textarea => {}
            _ => continue,
        }

        let Some(name) = attribute(document, field, "name").filter(|name| !name.is_empty()) else {
            continue;
        };

        if control == Control::Select {
            for option in form::options_of(document, field) {
                if state.selectedness(document, option) && !form::is_disabled(document, option) {
                    entries.push(Entry::text(
                        name.clone(),
                        form::option_value(document, option),
                    ));
                }
            }
            continue;
        }

        // A picker holding nothing still sends one entry, and it is an empty file
        // rather than an empty string: that is what HTML says, and a receiver that
        // asks for a file has to be able to tell "none" from "a file called
        // nothing".
        if control == Control::Input(InputKind::File) {
            let files = state.files(field);
            if files.is_empty() {
                entries.push(Entry {
                    name,
                    value: EntryValue::File(form::ChosenFile {
                        name: String::new(),
                        media_type: "application/octet-stream".to_owned(),
                        bytes: Vec::new(),
                    }),
                });
            } else {
                for file in files {
                    entries.push(Entry {
                        name: name.clone(),
                        value: EntryValue::File(file.clone()),
                    });
                }
            }
            continue;
        }

        let value = match control {
            // A checkbox with no value of its own submits `on`, which is HTML's
            // own answer and has been since forms existed.
            Control::Input(kind) if kind.is_checkable() => {
                attribute(document, field, "value").unwrap_or_else(|| "on".to_owned())
            }
            // A slider always holds a number, so one is always sent: an
            // `<input type=range>` with no `value` submits the middle of its
            // range rather than nothing at all.
            Control::Input(InputKind::Range) => {
                form::format_number(form::range_value(document, state, field))
            }
            _ => state.value(document, field).to_owned(),
        };
        entries.push(Entry::text(name, value));
    }
    entries
}

/// Work out everything about sending `form`, pressed by `submitter`.
///
/// The document's own address is needed to know what an empty `action` means and
/// what a relative one resolves against; both are the caller's, so what comes back
/// spells the action as the markup did and leaves resolving to whoever navigates.
#[must_use]
pub fn submission(
    document: &Document,
    state: &FormState,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Submission {
    // The button that was pressed may override where the form goes and how, which
    // is what `formaction` and `formmethod` are for.
    let overridden = |key: &str| submitter.and_then(|button| attribute(document, button, key));
    let action = overridden("formaction")
        .or_else(|| attribute(document, form, "action"))
        .unwrap_or_default();
    let method = overridden("formmethod")
        .or_else(|| attribute(document, form, "method"))
        .map_or(Method::Get, |value| Method::parse(&value));
    let encoding = overridden("formenctype")
        .or_else(|| attribute(document, form, "enctype"))
        .map_or(Encoding::default(), |value| Encoding::parse(&value));

    let entries = entry_list(document, state, form, submitter);
    // A `GET` always puts the pairs in the address, whatever the form's `enctype`
    // says: the encoding is about a body, and a `GET` has none.
    let (url, body, content_type) = match method {
        Method::Get | Method::Dialog => (with_query(&action, &entries), Vec::new(), String::new()),
        Method::Post => {
            let boundary = BOUNDARY.to_owned();
            let body = match encoding {
                Encoding::UrlEncoded => urlencoded(&entries).into_bytes(),
                Encoding::Multipart => multipart(&entries, &boundary),
                Encoding::Plain => plain(&entries).into_bytes(),
            };
            (action.clone(), body, encoding.content_type(&boundary))
        }
    };

    Submission {
        action,
        method,
        url,
        body,
        content_type,
    }
}

/// The boundary a multipart body is cut at.
///
/// Fixed rather than random: nothing here has a source of randomness, and a body
/// is checked against a snapshot in the tests. It is long enough and odd enough
/// that no form's value will contain it by accident.
const BOUNDARY: &str = "----otlyraFormBoundary8f2a1c";

/// An address with the pairs as its query, replacing whatever query it had.
///
/// HTML is explicit about the replacing: a `GET` form pointed at `?page=2` does not
/// send `?page=2&name=…`, it sends only what the form holds.
#[must_use]
pub fn with_query(action: &str, entries: &[Entry]) -> String {
    let (before, after) = match action.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (action, None),
    };
    let base = before.split_once('?').map_or(before, |(head, _)| head);
    let query = urlencoded(entries);
    let mut out = String::with_capacity(base.len() + query.len() + 2);
    out.push_str(base);
    out.push('?');
    out.push_str(&query);
    if let Some(fragment) = after {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// `name=value&name=value`, in HTML's own spelling of it.
#[must_use]
pub fn urlencoded(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|entry| format!("{}={}", percent(&entry.name), percent(entry.as_text())))
        .collect::<Vec<_>>()
        .join("&")
}

/// One part per entry, cut at the boundary.
#[must_use]
pub fn multipart(entries: &[Entry], boundary: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for entry in entries {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        match &entry.value {
            EntryValue::Text(text) => {
                out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                        escape_quotes(&entry.name)
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(text.as_bytes());
            }
            // A file carries two more things: what it is called and what it is.
            // The bytes go in as they are — a multipart body is bytes, not text,
            // which is the whole reason a form carrying a file must use it.
            EntryValue::File(file) => {
                out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\
                         Content-Type: {}\r\n\r\n",
                        escape_quotes(&entry.name),
                        escape_quotes(&file.name),
                        file.media_type
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(&file.bytes);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

/// `name=value`, one per line.
#[must_use]
pub fn plain(entries: &[Entry]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&entry.name);
        out.push('=');
        out.push_str(entry.as_text());
        out.push_str("\r\n");
    }
    out
}

/// The form-urlencoded spelling: a space is a plus, and everything that is not
/// unreserved is a percent and two hex digits.
fn percent(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A quotation mark inside a part's name, which would otherwise end it.
fn escape_quotes(value: &str) -> String {
    value.replace('"', "%22")
}

/// One attribute of one element.
fn attribute(document: &Document, id: NodeId, name: &str) -> Option<String> {
    document.get(id)?.element()?.attr(name).map(str::to_owned)
}

/// Whether a control is a suggestion inside a `<datalist>` rather than an answer.
fn in_datalist(document: &Document, id: NodeId) -> bool {
    let mut ancestor = document.get(id).and_then(|node| node.parent);
    while let Some(current) = ancestor {
        if document
            .get(current)
            .and_then(|node| node.element())
            .is_some_and(|element| element.name.local.as_ref() == "datalist")
        {
            return true;
        }
        ancestor = document.get(current).and_then(|node| node.parent);
    }
    false
}

/// Every node under `root`, in tree order.
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
