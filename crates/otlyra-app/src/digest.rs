//! The page in words, for a caller that cannot look at it.
//!
//! # Why this exists at all
//!
//! An agent driving this browser has two ways to find out what is on a page. It
//! can take a screenshot, which costs a picture's worth of tokens per turn and
//! cannot be clicked on. Or it can run a script and read the DOM — which needs
//! M12's engine, and is not available yet.
//!
//! So there is a third way, and it is one the browser already had. The
//! accessibility tree in [`crate::a11y`] is the page reduced to *what each part
//! is*: a heading of some level, a link with a destination, a field with a label
//! and a value, a button that would do something if pressed. That is the same
//! reduction an agent needs, built for a screen reader and correct for the same
//! reasons. This module reads it twice — once as prose, once as a list of things
//! that can be acted on — and adds nothing of its own.
//!
//! That is the whole design rule here: **no second walk of the document**. A
//! module that answered *what is on this page* from the DOM directly would drift
//! from what a reader is told, and then the browser would have two answers to one
//! question and no way to say which is right.
//!
//! # The two readings
//!
//! [`text`] is for *what does this page say* — Markdown, because it is the one
//! prose format every model has read a great deal of, and because a heading and a
//! list survive it.
//!
//! [`outline`] is for *what can I do here* — one flat row per thing, each naming
//! the DOM node behind it, so the caller can act on a row without having guessed
//! a selector. Flat rather than nested: an agent picks a row by reading it, and
//! nesting costs indentation on every line to express a relationship it does not
//! use.

use otlyra_platform::accesskit::Role;

use crate::a11y::Accessible;

/// One thing on the page, as a caller sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// How deep it sits, so a caller can see the shape without the tree.
    pub depth: usize,
    /// What it is, in the word a reader would use.
    pub role: &'static str,
    /// What it is called: a control's label, a link's or a heading's words.
    pub name: Option<String>,
    /// What it holds, for the controls that hold something.
    pub value: Option<String>,
    /// Where a link goes.
    pub url: Option<String>,
    /// The DOM node behind it, which is what a caller acts on.
    pub node: Option<otlyra_dom::NodeId>,
    /// Where the last frame drew it, in page coordinates.
    pub bounds: Option<(f64, f64, f64, f64)>,
    /// Whether acting on it would do anything.
    pub interactive: bool,
    /// Whether it is a control that is currently refusing input.
    pub disabled: bool,
    /// Ticked, for the controls that can be.
    pub checked: Option<bool>,
}

/// What to leave out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only the rows something could be done to.
    ///
    /// What an agent about to act wants: a page of prose has hundreds of rows and
    /// three of them are buttons.
    pub interactive_only: bool,
    /// How far down to go. `None` for all of it.
    pub max_depth: Option<usize>,
}

/// Whether a role is one a caller can act on.
///
/// By role rather than by tag, which is the whole reason this comes off the
/// accessibility tree: a `<div>` given `role="button"` is a button to a reader
/// and has to be one here, and an `<a>` with no `href` is not a link to either.
#[must_use]
pub fn is_interactive(role: Role) -> bool {
    matches!(
        role,
        Role::Link
            | Role::Button
            | Role::CheckBox
            | Role::RadioButton
            | Role::Switch
            | Role::Slider
            | Role::SpinButton
            | Role::Tab
            | Role::MenuItem
            | Role::ComboBox
            | Role::ListBox
            | Role::ListBoxOption
            | Role::TextInput
            | Role::MultilineTextInput
            | Role::SearchInput
            | Role::EmailInput
            | Role::PhoneNumberInput
            | Role::PasswordInput
            | Role::DateInput
            | Role::TimeInput
            | Role::DateTimeInput
            | Role::ColorWell
    )
}

/// The page as one flat list of rows.
#[must_use]
pub fn outline(items: &[Accessible], filter: Filter) -> Vec<Row> {
    let mut rows = Vec::new();
    walk(items, filter, 0, &mut rows);
    rows
}

fn walk(items: &[Accessible], filter: Filter, depth: usize, rows: &mut Vec<Row>) {
    if filter.max_depth.is_some_and(|max| depth > max) {
        return;
    }
    for item in items {
        let interactive = is_interactive(item.role);
        // A run of text is not a row of its own: it is what the thing above it is
        // called, and it is already spelled there. Emitting both would have every
        // heading appear twice.
        let is_bare_text = item.role == Role::Label && item.control.is_none();
        if !is_bare_text && (interactive || !filter.interactive_only) {
            rows.push(Row {
                depth,
                role: crate::a11y::role_word(item.role),
                name: name_of(item),
                value: item
                    .control
                    .as_ref()
                    .and_then(|facts| facts.value.clone())
                    .or_else(|| item.value.clone()),
                url: item.url.clone(),
                node: item.node,
                bounds: item
                    .bounds
                    .map(|rect| (rect.x0, rect.y0, rect.width(), rect.height())),
                interactive,
                disabled: item.control.as_ref().is_some_and(|facts| facts.disabled),
                checked: item.control.as_ref().and_then(|facts| facts.checked),
            });
        }
        walk(&item.children, filter, depth + 1, rows);
    }
}

/// What one thing is called.
///
/// A control's `<label>` where it has one, and otherwise the words inside it —
/// which is what a reader announces and what a person would point at. A control
/// falls back to its own text because a button whose label is its content is the
/// common case and a row reading `button` alone is useless to whoever has to pick
/// one.
fn name_of(item: &Accessible) -> Option<String> {
    if let Some(label) = item
        .control
        .as_ref()
        .and_then(|facts| facts.label.clone())
        .filter(|label| !label.is_empty())
    {
        return Some(label);
    }
    if let Some(value) = item.value.as_ref().filter(|value| !value.is_empty()) {
        return Some(value.clone());
    }
    let inside = inline(&item.children);
    (!inside.is_empty()).then_some(inside)
}

/// The words under `items`, run together the way a line of text reads.
fn inline(items: &[Accessible]) -> String {
    run(items, false)
}

/// The same, as Markdown: a link inside a line of prose keeps where it goes.
///
/// Told apart from [`inline`] because the two callers want different things from
/// the same words. A reading is prose and a link in it is `[words](url)`; a row's
/// *name* is what a person would call the thing, and link syntax in it would be
/// punctuation an agent then has to strip before matching on it.
fn rich(items: &[Accessible]) -> String {
    run(items, true)
}

fn run(items: &[Accessible], markdown: bool) -> String {
    let mut out = String::new();
    for item in items {
        let words = match item.role {
            Role::Label => item.value.clone().unwrap_or_default(),
            _ => item
                .value
                .clone()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| run(&item.children, markdown)),
        };
        let words = words.trim();
        if words.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        match (&item.url, markdown) {
            (Some(url), true) if item.role == Role::Link => {
                out.push_str(&format!("[{words}]({url})"));
            }
            _ => out.push_str(words),
        }
    }
    out
}

/// The page as Markdown.
///
/// `title` heads it, because a document's title is the one thing about a page
/// that is not in its body and is the first thing a caller wants.
#[must_use]
pub fn text(items: &[Accessible], title: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
        out.push_str("# ");
        out.push_str(title);
        out.push_str("\n\n");
    }
    let mut writer = Markdown::default();
    writer.block(items, &mut out);
    // One trailing newline, whatever the page ended with: a caller comparing two
    // readings should not see a difference that is only blank lines.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// The state a Markdown reading carries between siblings.
#[derive(Default)]
struct Markdown {
    /// How deep in lists we are, so a nested item indents.
    list: usize,
}

impl Markdown {
    /// Write `items` as blocks, one after another.
    fn block(&mut self, items: &[Accessible], out: &mut String) {
        for item in items {
            self.one(item, out);
        }
    }

    fn one(&mut self, item: &Accessible, out: &mut String) {
        match item.role {
            Role::Heading => {
                // Six is as deep as Markdown goes; a page nesting further gets the
                // deepest heading rather than a line of hashes that is not one.
                let level = item.level.unwrap_or(1).clamp(1, 6);
                paragraph(
                    out,
                    &format!("{} {}", "#".repeat(level), rich(&item.children)),
                );
            }
            Role::List => {
                self.list += 1;
                self.block(&item.children, out);
                self.list -= 1;
                if self.list == 0 {
                    out.push('\n');
                }
            }
            Role::ListItem => {
                let indent = "  ".repeat(self.list.saturating_sub(1));
                let words = rich(&item.children);
                if !words.is_empty() {
                    out.push_str(&format!("{indent}- {words}\n"));
                }
                // A list inside an item is written under it rather than beside it.
                let nested: Vec<Accessible> = item
                    .children
                    .iter()
                    .filter(|child| child.role == Role::List)
                    .cloned()
                    .collect();
                self.block(&nested, out);
            }
            Role::Blockquote => paragraph(out, &format!("> {}", rich(&item.children))),
            Role::Code => paragraph(out, &format!("```\n{}\n```", inline(&item.children))),
            Role::Table => {
                self.table(item, out);
            }
            Role::Link => {
                // A link on its own line is a line; one inside a paragraph is
                // written by `inline` as part of it. This is the first case.
                let words = rich(&item.children);
                match &item.url {
                    Some(url) if !words.is_empty() => paragraph(out, &format!("[{words}]({url})")),
                    _ if !words.is_empty() => paragraph(out, &words),
                    _ => {}
                }
            }
            Role::Image => {
                let alt = item.value.clone().unwrap_or_default();
                paragraph(out, &format!("![{alt}]()"));
            }
            Role::Label => {
                if let Some(words) = item.value.as_deref().map(str::trim)
                    && !words.is_empty()
                {
                    paragraph(out, words);
                }
            }
            _ if item.control.is_some() => {
                // A control is not prose, and a reading that dropped it would
                // describe a form as an empty page. One line saying what it is.
                paragraph(out, &control_line(item));
            }
            Role::Paragraph => {
                let words = rich(&item.children);
                if !words.is_empty() {
                    paragraph(out, &words);
                }
            }
            // Everything else is a container: it contributes its children and no
            // line of its own, which is the same rule `collect` follows for a
            // `<div>`.
            _ => self.block(&item.children, out),
        }
    }

    /// A table, as the pipe form Markdown has for one.
    ///
    /// The first row is treated as the header, because that is the only shape
    /// Markdown can express and a table whose first row is data still reads
    /// correctly with it — the separator is the lie, and it is one line.
    fn table(&mut self, item: &Accessible, out: &mut String) {
        let rows: Vec<Vec<String>> = flatten_rows(&item.children)
            .into_iter()
            .map(|row| {
                row.children
                    .iter()
                    .map(|cell| rich(std::slice::from_ref(cell)))
                    .collect()
            })
            .collect();
        let Some(first) = rows.first() else {
            return;
        };
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let line = |cells: &[String]| {
            let mut padded: Vec<String> = cells.to_vec();
            padded.resize(width, String::new());
            format!("| {} |", padded.join(" | "))
        };
        out.push_str(&line(first));
        out.push('\n');
        out.push_str(&format!("|{}\n", " --- |".repeat(width)));
        for row in rows.iter().skip(1) {
            out.push_str(&line(row));
            out.push('\n');
        }
        out.push('\n');
    }
}

/// Every row under a table, however many section levels are between.
///
/// `<thead>`, `<tbody>` and `<tfoot>` are groups in the tree and rows are under
/// them, so a table's rows are not always its children.
fn flatten_rows(items: &[Accessible]) -> Vec<&Accessible> {
    let mut rows = Vec::new();
    for item in items {
        if item.role == Role::Row {
            rows.push(item);
        } else {
            rows.extend(flatten_rows(&item.children));
        }
    }
    rows
}

/// One line for a control, saying what it is and what it holds.
fn control_line(item: &Accessible) -> String {
    let facts = item.control.as_ref();
    let role = crate::a11y::role_word(item.role);
    let label = facts
        .and_then(|facts| facts.label.clone())
        .or_else(|| item.value.clone())
        .unwrap_or_default();
    let mut line = if label.is_empty() {
        format!("[{role}]")
    } else {
        format!("[{role}: {label}]")
    };
    if let Some(facts) = facts {
        if let Some(checked) = facts.checked {
            line.push_str(if checked {
                " (checked)"
            } else {
                " (unchecked)"
            });
        }
        if let Some(value) = facts.value.as_deref().filter(|value| !value.is_empty())
            && facts.checked.is_none()
        {
            line.push_str(&format!(" = {value:?}"));
        }
        if facts.disabled {
            line.push_str(" (disabled)");
        }
        if facts.required {
            line.push_str(" (required)");
        }
    }
    line
}

/// Write `words` as its own paragraph.
fn paragraph(out: &mut String, words: &str) {
    out.push_str(words);
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageScene;

    fn page(markup: &str) -> PageScene {
        let parsed = otlyra_html::parse(markup.as_bytes(), Some("utf-8"));
        let mut page = PageScene::new(parsed.document);
        let mut text = otlyra_text::TextEngine::isolated();
        let _ = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        page
    }

    fn read(markup: &str) -> String {
        let page = page(markup);
        text(&crate::a11y::describe_page(&page), None)
    }

    #[test]
    fn a_heading_keeps_its_level() {
        assert!(read("<h2>Chapter</h2>").contains("## Chapter"));
    }

    #[test]
    fn a_link_keeps_where_it_goes() {
        // The destination is the half of a link an agent acts on, and a reading
        // that dropped it would leave it with nowhere to navigate.
        let out = read(r#"<p><a href="https://a.example/">Onwards</a></p>"#);
        assert!(out.contains("Onwards"), "{out}");
        assert!(out.contains("https://a.example/"), "{out}");
    }

    #[test]
    fn a_list_reads_as_a_list() {
        let out = read("<ul><li>one</li><li>two</li></ul>");
        assert!(out.contains("- one"), "{out}");
        assert!(out.contains("- two"), "{out}");
    }

    #[test]
    fn a_table_reads_as_a_table() {
        let out = read(
            "<table><tr><th>Name</th><th>Age</th></tr><tr><td>Ada</td><td>36</td></tr></table>",
        );
        assert!(out.contains("| Name | Age |"), "{out}");
        assert!(out.contains("| Ada | 36 |"), "{out}");
    }

    #[test]
    fn a_form_control_is_not_silently_dropped() {
        // A page whose whole content is a form would otherwise read as blank,
        // which is the reading that would make an agent give up on a real site.
        let out = read(r#"<label for=q>Search</label><input id=q value=cats>"#);
        assert!(out.contains("Search"), "{out}");
        assert!(out.contains("cats"), "{out}");
    }

    #[test]
    fn the_title_heads_the_reading() {
        let page = page("<p>body</p>");
        let out = text(&crate::a11y::describe_page(&page), Some("Some page"));
        assert!(out.starts_with("# Some page"), "{out}");
    }

    #[test]
    fn an_outline_names_the_node_behind_each_row() {
        let page = page(r#"<a href="/x">go</a><button>press</button>"#);
        let rows = outline(&crate::a11y::describe_page(&page), Filter::default());

        let acting: Vec<&Row> = rows.iter().filter(|row| row.interactive).collect();
        assert!(acting.len() >= 2, "{rows:#?}");
        for row in acting {
            // Without this a caller has a row it can read and cannot act on.
            assert!(row.node.is_some(), "{row:?}");
        }
    }

    #[test]
    fn asking_only_for_what_can_be_acted_on_leaves_the_prose_out() {
        let page = page("<p>a great deal of text</p><button>press</button>");
        let filtered = outline(
            &crate::a11y::describe_page(&page),
            Filter {
                interactive_only: true,
                ..Filter::default()
            },
        );
        assert!(filtered.iter().all(|row| row.interactive), "{filtered:#?}");
        assert!(
            filtered.iter().any(|row| row.role == "button"),
            "{filtered:#?}"
        );
    }
}
