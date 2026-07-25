//! The accessibility tree, built from the document.
//!
//! A screen reader does not read pixels; it reads structure, and the structure of a
//! page is in its DOM. So this walks the box tree — which is the DOM with
//! `display: none` already removed and one style per box — and states what each
//! part *is*: a heading of some level, a link with a destination, a paragraph, a
//! run of text.
//!
//! The plan puts this at M7 rather than later for a reason worth repeating: no
//! toolkit can produce this tree for a browser, so it is our work whenever it
//! happens, and retrofitting it after the rendering path has settled is the
//! expensive order to do it in.

use otlyra_layout::{BoxId, BoxKind, BoxTree};
use otlyra_platform::accesskit::{Node, NodeId, Rect, Role, Toggled, Tree, TreeId, TreeUpdate};

use crate::page::PageScene;
use crate::widget::{Described, FocusId, Role as WidgetRole};

/// The identifier of the tree's root. Everything else takes its identifier from
/// the box it describes, which is already unique and already stable across a
/// frame.
const ROOT: NodeId = NodeId(0);

/// Where the interface's identifiers start.
///
/// The document numbers its nodes from its box ids, which begin at one and count
/// up; the interface numbers its own from the far end of the same space so the
/// two cannot collide however large either grows. A shared counter would have
/// been the alternative, and it would have made every node's identity depend on
/// the order the two trees happened to be built in.
const INTERFACE_BASE: u64 = u64::MAX / 2;

/// Where the identifiers of elements that generated no box start.
///
/// A closed drop-down suppresses its options, so they have no box to be numbered
/// from — and a reader still has to be able to walk them and choose one. They are
/// numbered from their element instead, in a stretch of the space between the
/// boxes and the interface, so an identifier still says on sight which of the
/// three it names.
const ELEMENT_BASE: u64 = u64::MAX / 4;

/// Build the tree for `page`, titled `title`.
pub fn tree_for(page: &PageScene, title: &str) -> TreeUpdate {
    let mut nodes = Vec::new();

    let mut root = Node::new(Role::Document);
    root.set_label(title.to_owned());

    let described = describe_page(page);
    // Where the keyboard is, said in the tree's own terms. A reader that is told
    // the document has the focus while a field holds the caret is describing a
    // different page than the one on screen.
    let focus = focused(&described).unwrap_or(ROOT);
    let children = to_nodes(&described, &mut nodes);
    root.set_children(children);
    nodes.push((ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(Tree::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

/// The identifier of whatever holds the keyboard, if anything on the page does.
fn focused(items: &[Accessible]) -> Option<NodeId> {
    items.iter().find_map(|item| {
        if item.control.as_ref().is_some_and(|facts| facts.focused) {
            return Some(identity(item));
        }
        focused(&item.children)
    })
}

/// One accessible thing on the page, before it is anything a platform knows.
///
/// The middle of the walk, split out so the panel that *shows* the tree and the
/// adapter that *hands it to a reader* are two views of one answer. A panel that
/// walked the boxes itself would be a second account of what a page exposes, and
/// the first bug in it would be the two disagreeing.
#[derive(Clone, Debug)]
pub struct Accessible {
    /// The box it came from, absent for something that generated none — an option
    /// of a drop-down that is closed.
    pub box_id: Option<BoxId>,
    /// The DOM node behind that box, absent for an anonymous one.
    pub node: Option<otlyra_dom::NodeId>,
    /// What it is.
    pub role: Role,
    /// What it says, for the leaves that are text.
    pub value: Option<String>,
    /// A heading's level.
    pub level: Option<usize>,
    /// Where a link goes.
    pub url: Option<String>,
    /// Where it was drawn, in page coordinates.
    pub bounds: Option<Rect>,
    /// What it is, holds and is in the middle of, when it is a form control.
    pub control: Option<crate::page::ControlFacts>,
    /// What it contains.
    pub children: Vec<Accessible>,
}

impl Accessible {
    /// What a screen reader says when the cursor lands here.
    ///
    /// The role first and the words second, which is the order every reader
    /// announces in: *what is this* and only then *what does it say*. Written
    /// out rather than left to the panel, because "what would be read" is a fact
    /// about the tree and not about the panel showing it.
    pub fn spoken(&self) -> String {
        let mut out = role_word(self.role).to_owned();
        if let Some(level) = self.level {
            out.push_str(&format!(" level {level}"));
        }
        if let Some(value) = self.value.as_deref().filter(|value| !value.is_empty()) {
            out.push_str(&format!(", \u{201c}{value}\u{201d}"));
        }
        if let Some(url) = self.url.as_deref() {
            out.push_str(&format!(", {url}"));
        }
        out
    }
}

/// What a reader calls a role, in the words it uses out loud.
///
/// `Debug` would say `ListItem`, which is a Rust identifier and not a word
/// anybody hears.
pub fn role_word(role: Role) -> &'static str {
    match role {
        Role::Heading => "heading",
        Role::Link => "link",
        Role::Paragraph => "paragraph",
        Role::List => "list",
        Role::ListItem => "list item",
        Role::Table => "table",
        Role::Row => "row",
        Role::Cell => "cell",
        Role::Button => "button",
        Role::Image => "image",
        Role::Navigation => "navigation",
        Role::Header => "banner",
        Role::Footer => "content info",
        Role::Main => "main",
        Role::Article => "article",
        Role::Section => "section",
        Role::Blockquote => "quote",
        Role::Code => "code",
        Role::Strong => "strong",
        Role::Emphasis => "emphasis",
        Role::Label => "text",
        Role::Document => "document",
        Role::Window => "window",
        Role::CheckBox => "checkbox",
        Role::RadioButton => "radio button",
        Role::Switch => "switch",
        Role::TextInput => "text field",
        Role::Slider => "slider",
        Role::Tab => "tab",
        Role::MenuItem => "menu item",
        _ => "group",
    }
}

/// What `page` exposes, as the tree a reader would walk.
///
/// The children of the document root: the root itself is the page, and what it
/// is called is the title, which is the caller's to supply.
pub fn describe_page(page: &PageScene) -> Vec<Accessible> {
    let boxes = page.boxes();
    collect(page, boxes, boxes.root())
}

/// One accessible node, and the ones under it, as platform nodes.
fn to_nodes(items: &[Accessible], nodes: &mut Vec<(NodeId, Node)>) -> Vec<NodeId> {
    let mut ids = Vec::new();
    for item in items {
        let grandchildren = to_nodes(&item.children, nodes);
        let mut node = Node::new(item.role);
        node.set_children(grandchildren);
        if let Some(value) = &item.value {
            node.set_value(value.clone());
        }
        if let Some(bounds) = item.bounds {
            node.set_bounds(bounds);
        }
        if let Some(url) = &item.url {
            node.set_url(url.clone());
        }
        if let Some(level) = item.level {
            node.set_level(level);
        }
        if let Some(facts) = &item.control {
            describe_control(&mut node, facts);
        }
        let id = identity(item);
        nodes.push((id, node));
        ids.push(id);
    }
    ids
}

/// The whole window: the interface, and the document under it.
///
/// One tree with one root, because that is what a screen reader walks. The
/// interface comes first, in the order it was drawn, which is the order the
/// keyboard travels it — the same rule that makes `Focus` work.
pub fn window_tree(
    interface: &[Described],
    focused: Option<FocusId>,
    document: TreeUpdate,
    title: &str,
) -> TreeUpdate {
    let mut nodes = document.nodes;

    // The document's own root becomes a child of the window's, so the page keeps
    // being one thing a reader can move in and out of rather than its contents
    // being tipped in beside the toolbar.
    let mut page_children = Vec::new();
    for (id, node) in &mut nodes {
        if *id == ROOT {
            *id = NodeId(INTERFACE_BASE - 1);
            page_children.push(*id);
            node.set_label(title.to_owned());
        }
    }

    let mut children = Vec::new();
    let mut interface_focus = None;
    for (index, described) in interface.iter().enumerate() {
        let id = NodeId(INTERFACE_BASE + index as u64);
        if described.focus.is_some() && described.focus == focused {
            interface_focus = Some(id);
        }
        nodes.push((id, node_for(described)));
        children.push(id);
    }
    children.extend(page_children);

    let mut root = Node::new(Role::Window);
    root.set_label(title.to_owned());
    root.set_children(children);
    nodes.push((ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(Tree::new(ROOT)),
        tree_id: TreeId::ROOT,
        // What the interface is holding, or the document. A reader is told where
        // the keyboard *is*; claiming the window when a field has the caret would
        // be describing a different browser than the one on screen.
        focus: interface_focus.unwrap_or(document.focus.max(ROOT)),
    }
}

/// Which described control a node identifier names, if it names one.
///
/// The inverse of how `window_tree` numbers them, kept beside it so the two
/// cannot drift: an identifier is the base plus the control's position in the
/// description, and a description is rebuilt every frame in drawing order.
pub fn described_index(node: otlyra_platform::accesskit::NodeId) -> Option<usize> {
    (node.0 >= INTERFACE_BASE).then(|| (node.0 - INTERFACE_BASE) as usize)
}

/// One described control, as a node.
fn node_for(described: &Described) -> Node {
    let mut node = Node::new(match described.role {
        WidgetRole::Button => Role::Button,
        WidgetRole::CheckBox => Role::CheckBox,
        WidgetRole::RadioButton => Role::RadioButton,
        WidgetRole::Switch => Role::Switch,
        WidgetRole::TextInput => Role::TextInput,
        WidgetRole::Slider => Role::Slider,
        WidgetRole::Tab => Role::Tab,
        WidgetRole::MenuItem => Role::MenuItem,
        WidgetRole::Label => Role::Label,
    });

    if !described.label.is_empty() {
        node.set_label(described.label.clone());
    }
    if let Some(value) = &described.value {
        node.set_value(value.clone());
    }
    node.set_bounds(Rect::new(
        described.rect.x,
        described.rect.y,
        described.rect.x + described.rect.width,
        described.rect.y + described.rect.height,
    ));

    // A control that says *ticked* or *on* says it twice: once as a value a
    // reader hears, and once as the state its own kind of control has, which is
    // what a reader's shortcuts and its summary of a form both read.
    match (described.role, described.value.as_deref()) {
        (WidgetRole::CheckBox | WidgetRole::Switch, Some("ticked" | "on")) => {
            node.set_toggled(Toggled::True);
        }
        (WidgetRole::CheckBox | WidgetRole::Switch, Some(_)) => node.set_toggled(Toggled::False),
        (WidgetRole::RadioButton | WidgetRole::Tab, Some("chosen" | "selected")) => {
            node.set_selected(true);
        }
        (WidgetRole::RadioButton | WidgetRole::Tab, Some(_)) => node.set_selected(false),
        _ => {}
    }

    if described.enabled {
        // Only the controls that can be pressed advertise the action, so a
        // reader offering "press this" is never offering something that does
        // nothing.
        node.add_action(otlyra_platform::accesskit::Action::Click);
    } else {
        node.set_disabled();
    }

    node
}

/// The tree for a tab with no document — a blank tab, or one that failed.
pub fn empty_tree(label: &str) -> TreeUpdate {
    let mut root = Node::new(Role::Document);
    root.set_label(label.to_owned());
    TreeUpdate {
        nodes: vec![(ROOT, root)],
        tree: Some(Tree::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: ROOT,
    }
}

/// Walk a box's children and say what each of them is.
fn collect(page: &PageScene, boxes: &BoxTree, id: BoxId) -> Vec<Accessible> {
    let mut out = Vec::new();

    for &child in &boxes.node(id).children {
        let node = boxes.node(child);

        // Text is a leaf with a value; everything else is a container that may or
        // may not be worth a node of its own.
        if let BoxKind::Text(text) = &node.kind {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            out.push(Accessible {
                box_id: Some(child),
                node: node.node,
                role: Role::Label,
                value: Some(trimmed.to_owned()),
                level: None,
                url: None,
                bounds: None,
                control: None,
                children: Vec::new(),
            });
            continue;
        }

        // A control says what it is, what it holds and what state it is in, and
        // says it once: the text a widget generates is the value it already
        // reports, so descending into one would have a reader hear it twice.
        if let Some(dom) = node.node
            && let Some(facts) = page.control_facts(dom)
        {
            out.push(Accessible {
                box_id: Some(child),
                node: node.node,
                role: control_role(&facts),
                value: facts.value.clone(),
                level: None,
                url: None,
                bounds: bounds_of(page, child),
                children: if facts.suggests
                    || matches!(facts.control, otlyra_dom::form::Control::Select)
                {
                    // The options a drop-down holds are what a reader moves
                    // through, open or not — so when it is closed and they have no
                    // boxes, they are described from the document instead.
                    // What is under an open list is its options; the one text a
                    // closed one generates is the option it is showing, which it
                    // already reports as its value.
                    let open: Vec<Accessible> = collect(page, boxes, child)
                        .into_iter()
                        .filter(|item| item.control.is_some())
                        .collect();
                    if !open.is_empty() {
                        open
                    } else if facts.suggests {
                        // A field's suggestions are in a `<datalist>` somewhere
                        // else in the document and are never boxes until the list
                        // is showing, so a reader walks them from there.
                        described(page, otlyra_dom::form::suggestions_of(page.document(), dom))
                    } else {
                        described(page, otlyra_dom::form::options_of(page.document(), dom))
                    }
                } else {
                    Vec::new()
                },
                control: Some(facts),
            });
            continue;
        }

        let grandchildren = collect(page, boxes, child);
        let Some(role) = role_of(node.tag.as_ref().map(|tag| tag.as_ref())) else {
            // A box with no role of its own — an anonymous wrapper, a plain `<div>`
            // — contributes its children rather than a level of nesting nobody
            // would want read out.
            out.extend(grandchildren);
            continue;
        };

        out.push(Accessible {
            box_id: Some(child),
            node: node.node,
            role,
            value: None,
            level: heading_level(node.tag.as_ref().map(|tag| tag.as_ref())),
            url: (role == Role::Link).then(|| page.href_of(child)).flatten(),
            bounds: bounds_of(page, child),
            control: None,
            children: grandchildren,
        });
    }

    out
}

/// Say what state a control is in, in the words the platform has for it.
///
/// A state is said as a state rather than as words in the value: a reader's own
/// shortcuts, its summary of a form and the sound it makes when a box is ticked
/// are all driven by these and not by anything it reads out.
fn describe_control(node: &mut Node, facts: &crate::page::ControlFacts) {
    use otlyra_platform::accesskit::{Action, Toggled};

    if let Some(label) = &facts.label {
        node.set_label(label.clone());
    }
    if let Some(checked) = facts.checked {
        node.set_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        });
    }
    if let Some(selected) = facts.selected {
        node.set_selected(selected);
    }
    if facts.required {
        node.set_required();
    }
    if facts.invalid {
        node.set_invalid(otlyra_platform::accesskit::Invalid::True);
    }
    if facts.disabled {
        node.set_disabled();
    }
    if let Some(numeric) = facts.numeric {
        node.set_numeric_value(numeric.value);
        node.set_min_numeric_value(numeric.min);
        node.set_max_numeric_value(numeric.max);
        if let Some(step) = numeric.step {
            node.set_numeric_value_step(step);
        }
    }
    if facts.actionable {
        // Only what would do something. A reader offering "press this" on a
        // control that would ignore it is a reader lying about the page.
        node.add_action(Action::Click);
        node.add_action(Action::Focus);
        // A slider is moved rather than pressed, and a reader has its own two
        // words for that.
        if facts.numeric.is_some() && facts.control.is_slider() {
            node.add_action(Action::Increment);
            node.add_action(Action::Decrement);
        }
    }
}

/// The options a list is not showing.
///
/// Taken from the document rather than from the boxes, because a closed drop-down
/// generates no boxes for its options and a field's suggestions are boxes only
/// while they are on screen — and a reader that cannot walk them cannot choose
/// one. They carry no rectangle for the same reason: nothing was drawn.
fn described(page: &PageScene, options: Vec<otlyra_dom::NodeId>) -> Vec<Accessible> {
    options
        .into_iter()
        .filter_map(|option| {
            let facts = page.control_facts(option)?;
            Some(Accessible {
                box_id: None,
                node: Some(option),
                role: control_role(&facts),
                value: Some(otlyra_dom::form::option_value(page.document(), option)),
                level: None,
                url: None,
                bounds: None,
                control: Some(facts),
                children: Vec::new(),
            })
        })
        .collect()
}

/// Where a box was drawn, as the tree spells a rectangle.
fn bounds_of(page: &PageScene, id: BoxId) -> Option<Rect> {
    page.rect_of(id).map(|rect| {
        Rect::new(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.right()),
            f64::from(rect.bottom()),
        )
    })
}

/// The role a control plays.
///
/// The mapping HTML's own accessibility chapter gives, and no more: a reader's
/// shortcuts, its summary of a form and the words it says are all decided by this,
/// so a control given the wrong role is worse than one given none.
fn control_role(facts: &crate::page::ControlFacts) -> Role {
    use otlyra_dom::form::{Control, InputKind};

    // A field with suggestions behind it is a combo box, whatever it would have
    // been without them: that is the role its list of options belongs to.
    if facts.suggests {
        return Role::ComboBox;
    }
    match facts.control {
        Control::Input(InputKind::Checkbox) => Role::CheckBox,
        Control::Input(InputKind::Radio) => Role::RadioButton,
        Control::Input(InputKind::Range) => Role::Slider,
        Control::Input(InputKind::Number) => Role::SpinButton,
        Control::Input(InputKind::Search) => Role::SearchInput,
        Control::Input(InputKind::Email) => Role::EmailInput,
        Control::Input(InputKind::Tel) => Role::PhoneNumberInput,
        Control::Input(InputKind::Password) => Role::PasswordInput,
        Control::Input(InputKind::Color) => Role::ColorWell,
        Control::Input(InputKind::Date) => Role::DateInput,
        Control::Input(InputKind::Time) => Role::TimeInput,
        Control::Input(InputKind::DatetimeLocal) => Role::DateTimeInput,
        Control::Input(kind) if kind.is_button() => Role::Button,
        Control::Input(_) => Role::TextInput,
        Control::Textarea => Role::MultilineTextInput,
        Control::Button => Role::Button,
        Control::Select => Role::ComboBox,
        Control::Option => Role::ListBoxOption,
        Control::Optgroup => Role::ListBox,
        Control::Meter => Role::Meter,
        Control::Progress => Role::ProgressIndicator,
        Control::Output => Role::Label,
        Control::Fieldset => Role::Group,
    }
}

/// What one accessible thing is called in the tree.
///
/// Its box where it has one and its element where it does not, in two stretches of
/// the space that cannot meet.
#[must_use]
pub fn identity(item: &Accessible) -> NodeId {
    match (item.box_id, item.node) {
        (Some(box_id), _) => identifier(box_id),
        (None, Some(node)) => NodeId(ELEMENT_BASE + otlyra_dom::node_id_to_u64(node)),
        (None, None) => ROOT,
    }
}

/// Which element a node identifier names, if it names one that has no box.
#[must_use]
pub fn element_of(node: NodeId) -> Option<otlyra_dom::NodeId> {
    (node.0 >= ELEMENT_BASE && node.0 < INTERFACE_BASE - 1)
        .then(|| otlyra_dom::node_id_from_u64(node.0 - ELEMENT_BASE))
}

/// A box's identifier in the tree.
///
/// Offset by one so that nothing collides with the root, whose identifier is zero
/// because it belongs to no box.
fn identifier(id: BoxId) -> NodeId {
    NodeId(otlyra_layout::box_id_to_u64(id).wrapping_add(1))
}

/// Which box a node identifier names, if it names one on the page.
///
/// The inverse of [`identifier`], kept beside it so the two cannot drift: an
/// identifier is the box's own number plus one, and the interface's own start at
/// the far end of the space and are not boxes at all.
#[must_use]
pub fn box_of(node: NodeId) -> Option<BoxId> {
    if node.0 == 0 || node.0 >= ELEMENT_BASE {
        return None;
    }
    Some(otlyra_layout::box_id_from_u64(node.0 - 1))
}

/// The role an element plays, or `None` for elements that are only structure.
fn role_of(tag: Option<&str>) -> Option<Role> {
    Some(match tag? {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Role::Heading,
        "a" => Role::Link,
        "p" => Role::Paragraph,
        "ul" | "ol" => Role::List,
        "li" => Role::ListItem,
        "table" => Role::Table,
        "tr" => Role::Row,
        "td" | "th" => Role::Cell,
        "button" => Role::Button,
        "img" => Role::Image,
        "nav" => Role::Navigation,
        "header" => Role::Header,
        "footer" => Role::Footer,
        "main" => Role::Main,
        "article" => Role::Article,
        "section" => Role::Section,
        "blockquote" => Role::Blockquote,
        "code" | "pre" => Role::Code,
        "strong" | "b" => Role::Strong,
        "em" | "i" => Role::Emphasis,
        _ => return None,
    })
}

/// The heading level, for the elements that have one.
fn heading_level(tag: Option<&str>) -> Option<usize> {
    Some(match tag? {
        "h1" => 1,
        "h2" => 2,
        "h3" => 3,
        "h4" => 4,
        "h5" => 5,
        "h6" => 6,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> PageScene {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let mut page = PageScene::new(parsed.document);
        let mut text = otlyra_text::TextEngine::isolated();
        let _ = page.build_display_list(&mut text, 800.0, 600.0, 0.0);
        page
    }

    /// The tree, as a screen reader would walk it: role, then label or value.
    fn outline(update: &TreeUpdate) -> Vec<String> {
        fn walk(update: &TreeUpdate, id: NodeId, depth: usize, out: &mut Vec<String>) {
            let Some((_, node)) = update.nodes.iter().find(|(node_id, _)| *node_id == id) else {
                return;
            };
            let detail = node
                .value()
                .map(str::to_owned)
                .or_else(|| node.label().map(str::to_owned))
                .unwrap_or_default();
            out.push(format!(
                "{}{:?}{}",
                "  ".repeat(depth),
                node.role(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" {detail:?}")
                }
            ));
            for child in node.children() {
                walk(update, *child, depth + 1, out);
            }
        }

        let mut out = Vec::new();
        walk(update, ROOT, 0, &mut out);
        out
    }

    #[test]
    fn headings_links_and_text_come_out_with_their_roles() {
        let page = page("<body><h2>A heading</h2><p>Some <a href=\"/x\">link</a> text");
        let outline = outline(&tree_for(&page, "The title"));

        insta::assert_snapshot!(outline.join("\n"));
    }

    #[test]
    fn a_heading_carries_its_level() {
        let page = page("<body><h3>Third level");
        let update = tree_for(&page, "t");
        let heading = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Heading)
            .expect("a heading");
        assert_eq!(heading.1.level(), Some(3));
    }

    #[test]
    fn a_link_carries_where_it_goes() {
        let page = page("<body><p><a href=\"/next\">go</a>");
        let update = tree_for(&page, "t");
        let link = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Link)
            .expect("a link");
        assert_eq!(link.1.url(), Some("/next"));
    }

    /// Structure nobody would want read out — anonymous wrappers, plain divs —
    /// contributes its children rather than a level of nesting.
    #[test]
    fn a_plain_div_does_not_become_a_level_of_the_tree() {
        let page = page("<body><div><div><p>text");
        let outline = outline(&tree_for(&page, "t"));
        assert_eq!(
            outline,
            vec![
                "Document \"t\"".to_owned(),
                "  Paragraph".to_owned(),
                "    Label \"text\"".to_owned(),
            ]
        );
    }

    #[test]
    fn a_node_knows_where_it_is_on_the_page() {
        let page = page("<body><h1>heading");
        let update = tree_for(&page, "t");
        let heading = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Heading)
            .expect("a heading");
        let bounds = heading.1.bounds().expect("bounds");
        assert!(bounds.width() > 0.0 && bounds.height() > 0.0);
    }

    // --- the page's own controls ------------------------------------------

    /// A control says what it is, what it holds and what state it is in — and the
    /// words beside it are its label, wherever the label was written.
    #[test]
    fn a_control_carries_its_role_its_label_and_its_state() {
        let page = page(
            "<body><label for=n>Your name</label><input id=n value=Ada>\
             <label><input type=checkbox checked> Send me post</label>",
        );
        let update = tree_for(&page, "t");
        let of = |role: Role| {
            update
                .nodes
                .iter()
                .find(|(_, node)| node.role() == role)
                .map(|(_, node)| node.clone())
                .expect("the node")
        };

        let field = of(Role::TextInput);
        assert_eq!(field.label(), Some("Your name"));
        assert_eq!(field.value(), Some("Ada"));
        assert!(field.supports_action(otlyra_platform::accesskit::Action::Click));
        assert!(field.supports_action(otlyra_platform::accesskit::Action::Focus));

        let box_ = of(Role::CheckBox);
        assert_eq!(box_.label(), Some("Send me post"));
        assert_eq!(box_.toggled(), Some(Toggled::True));
        assert_eq!(
            box_.value(),
            None,
            "what a checkbox holds is its state, said once"
        );
    }

    /// A control nothing can reach says so and offers nothing, and a required one
    /// says that too.
    #[test]
    fn a_disabled_control_offers_no_press_and_a_required_one_says_so() {
        let page = page("<body><input disabled value=x><input required id=q>");
        let update = tree_for(&page, "t");
        let fields: Vec<_> = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::TextInput)
            .map(|(_, node)| node.clone())
            .collect();
        assert_eq!(fields.len(), 2);

        let dimmed = fields
            .iter()
            .find(|node| node.is_disabled())
            .expect("the disabled field");
        assert!(!dimmed.supports_action(otlyra_platform::accesskit::Action::Click));

        let wanted = fields
            .iter()
            .find(|node| !node.is_disabled())
            .expect("the other field");
        assert!(wanted.is_required());
    }

    /// A drop-down is a combo box holding what it shows, and the options under it
    /// say which one is chosen.
    #[test]
    fn a_drop_down_says_what_it_shows_and_which_option_is_chosen() {
        let page = page("<body><select><option>Alpha<option selected>Beta</select>");
        let update = tree_for(&page, "t");
        let combo = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::ComboBox)
            .map(|(_, node)| node.clone())
            .expect("the drop-down");
        assert_eq!(combo.value(), Some("Beta"));

        // Closed, its options have no boxes at all — and are still there to be
        // walked, described from the document.
        let closed: Vec<bool> = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::ListBoxOption)
            .map(|(_, node)| node.is_selected().unwrap_or(false))
            .collect();
        assert_eq!(closed, vec![false, true]);

        // Open, they are the boxes on screen, and say the same thing.
        let mut page = page;
        let mut text = otlyra_text::TextEngine::isolated();
        let select = {
            let boxes = page.boxes();
            boxes
                .descendants(boxes.root())
                .into_iter()
                .find(|&id| boxes.node(id).control.is_some())
                .expect("the drop-down")
        };
        let rect = page.rect_of(select).expect("a rectangle");
        let (x, y) = (
            f64::from(rect.x + rect.width / 2.0),
            f64::from(rect.y + rect.height / 2.0),
        );
        page.pointer_pressed(x, y);
        page.pointer_released(x, y);
        let _ = page.build_display_list(&mut text, 800.0, 600.0, 0.0);

        let update = tree_for(&page, "t");
        let chosen: Vec<bool> = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::ListBoxOption)
            .map(|(_, node)| node.is_selected().unwrap_or(false))
            .collect();
        assert_eq!(chosen, vec![false, true]);
    }

    /// A slider says the number it holds and the range it holds it in, and offers
    /// a reader the two words for moving it.
    #[test]
    fn a_slider_says_its_number_and_can_be_moved_by_a_reader() {
        let mut page = page("<body><input type=range min=0 max=10 step=2 value=4>");
        let update = tree_for(&page, "t");
        let (id, slider) = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Slider)
            .map(|(id, node)| (*id, node.clone()))
            .expect("the slider");
        assert_eq!(slider.numeric_value(), Some(4.0));
        assert_eq!(slider.min_numeric_value(), Some(0.0));
        assert_eq!(slider.max_numeric_value(), Some(10.0));
        assert_eq!(slider.numeric_value_step(), Some(2.0));
        assert!(slider.supports_action(otlyra_platform::accesskit::Action::Increment));

        let element = box_of(id)
            .and_then(|box_id| page.boxes().get(box_id).and_then(|node| node.node))
            .expect("the element behind it");
        assert!(page.focus_node(element));
        assert!(page.step_value(crate::page::SliderMotion::Up));
        let update = tree_for(&page, "t");
        let moved = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Slider)
            .map(|(_, node)| node.clone())
            .expect("the slider");
        assert_eq!(moved.numeric_value(), Some(6.0));
    }

    /// A field with suggestions behind it is a combo box, and its suggestions are
    /// the options of the list it shows — walked to, and taken, by name.
    #[test]
    fn a_field_with_suggestions_is_a_combo_box_a_reader_can_take_one_from() {
        let mut page = page(
            "<body><input list=cities>\
             <datalist id=cities><option value=Amsterdam><option value=Berlin>\
             </datalist>",
        );
        let update = tree_for(&page, "t");
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.role() == Role::ComboBox),
            "a field offering a list is a combo box"
        );

        // Closed, the suggestions are described from the document, as a closed
        // drop-down's options are.
        let (id, _) = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::ListBoxOption)
            .nth(1)
            .expect("the second suggestion");
        let element = element_of(*id).expect("it names an element");

        // Taking one is not open to a reader until the list is: a suggestion
        // belongs to whichever field is showing it.
        let field = {
            let boxes = page.boxes();
            boxes
                .descendants(boxes.root())
                .into_iter()
                .find(|&id| boxes.node(id).control.is_some())
                .and_then(|id| boxes.node(id).node)
                .expect("the field")
        };
        assert!(page.focus_node(field));
        assert!(page.step_selection(true), "the list is showing");
        assert!(page.activate_node(element));
        assert_eq!(page.focused_value(), Some("Berlin"));
    }

    /// An option of a closed drop-down is named by its element, because it has no
    /// box — and choosing it through that name is choosing it.
    #[test]
    fn an_option_of_a_closed_drop_down_can_be_chosen_by_name() {
        let mut page = page("<body><select><option>Alpha<option>Beta</select>");
        let update = tree_for(&page, "t");
        let (id, _) = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::ListBoxOption)
            .nth(1)
            .expect("the second option");
        assert_eq!(box_of(*id), None, "it came from no box");
        let element = element_of(*id).expect("it names an element");

        assert!(page.activate_node(element));
        let update = tree_for(&page, "t");
        let combo = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::ComboBox)
            .map(|(_, node)| node.clone())
            .expect("the drop-down");
        assert_eq!(combo.value(), Some("Beta"), "the choice was not taken");
    }

    /// A reader is told where the keyboard is on the page, not just in the
    /// interface.
    #[test]
    fn the_focused_control_is_the_tree_focus() {
        let mut page = page("<body><input id=a><input id=b>");
        let boxes = page.boxes();
        let second = boxes
            .descendants(boxes.root())
            .into_iter()
            .filter(|&id| boxes.node(id).control.is_some())
            .nth(1)
            .expect("the second field");
        let element = page.boxes().node(second).node.expect("its element");
        assert!(page.focus_node(element));

        let update = tree_for(&page, "t");
        assert_eq!(update.focus, identifier(second));
    }

    /// The identifier a press comes back with names the box it was built from.
    #[test]
    fn a_page_node_identifier_names_the_box_it_was_built_from() {
        let page = page("<body><p>text");
        let update = tree_for(&page, "t");
        let (id, _) = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Paragraph)
            .expect("the paragraph");
        assert_eq!(box_of(*id).map(identifier), Some(*id));
        assert_eq!(box_of(ROOT), None, "the root belongs to no box");
        assert_eq!(
            box_of(NodeId(INTERFACE_BASE)),
            None,
            "an interface node is not a box"
        );
    }

    // --- the interface, joined to the document ---------------------------

    fn described(role: WidgetRole, label: &str, value: Option<&str>) -> Described {
        Described {
            rect: crate::widget::Rect::new(0.0, 0.0, 40.0, 20.0),
            role,
            label: label.to_owned(),
            value: value.map(str::to_owned),
            focus: None,
            enabled: true,
        }
    }

    /// The window is one tree: the toolbar, then the page as a thing of its own.
    #[test]
    fn the_window_holds_the_interface_and_the_page_under_it() {
        let page = page("<body><p>text");
        let interface = vec![described(WidgetRole::Button, "Reload", None)];
        let update = window_tree(&interface, None, tree_for(&page, "A page"), "A page");

        assert_eq!(
            outline(&update),
            vec![
                "Window \"A page\"".to_owned(),
                "  Button \"Reload\"".to_owned(),
                "  Document \"A page\"".to_owned(),
                "    Paragraph".to_owned(),
                "      Label \"text\"".to_owned(),
            ]
        );
    }

    /// A tick is said twice on purpose: as words a reader hears, and as the state
    /// its shortcuts and its summary of a form both read.
    #[test]
    fn a_ticked_box_and_a_thrown_switch_carry_their_state() {
        let interface = vec![
            described(WidgetRole::CheckBox, "Load images", Some("ticked")),
            described(WidgetRole::Switch, "Run scripts", Some("off")),
            described(WidgetRole::Tab, "Tab 0", Some("selected")),
        ];
        let update = window_tree(&interface, None, empty_tree("t"), "t");

        let of = |role: Role| {
            update
                .nodes
                .iter()
                .find(|(_, node)| node.role() == role)
                .map(|(_, node)| node.clone())
                .expect("the node")
        };
        assert_eq!(of(Role::CheckBox).toggled(), Some(Toggled::True));
        assert_eq!(of(Role::Switch).toggled(), Some(Toggled::False));
        assert!(of(Role::Tab).is_selected().unwrap_or(false));
    }

    /// A reader is told where the keyboard *is*. Claiming the window while a
    /// control holds it would describe a different browser than the one on screen.
    #[test]
    fn focus_lands_on_the_control_holding_the_keyboard() {
        let mut holder = described(WidgetRole::Button, "Reload", None);
        holder.focus = Some(7);
        let interface = vec![
            described(WidgetRole::Tab, "Tab 0", Some("selected")),
            holder,
        ];

        let update = window_tree(&interface, Some(7), empty_tree("t"), "t");
        assert_eq!(update.focus, NodeId(INTERFACE_BASE + 1));
    }

    /// The identifiers the tree hands out are the ones a press comes back with.
    #[test]
    fn a_node_identifier_names_the_control_it_was_built_from() {
        let interface = vec![
            described(WidgetRole::Button, "Back", None),
            described(WidgetRole::Button, "Reload", None),
        ];
        let update = window_tree(&interface, None, empty_tree("t"), "t");

        let root = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ROOT)
            .expect("the root");
        let first = root.1.children()[0];
        assert_eq!(described_index(first), Some(0));
    }

    /// A page node is not an interface node, and must not be mistaken for the
    /// control that happens to sit at that index.
    #[test]
    fn a_page_node_names_no_control() {
        assert_eq!(described_index(NodeId(3)), None);
    }

    /// A control drawn but unable to act offers no press, so a reader never
    /// offers one that would do nothing.
    #[test]
    fn a_control_that_cannot_be_pressed_is_marked_disabled() {
        let mut dimmed = described(WidgetRole::Switch, "Run scripts", Some("off"));
        dimmed.enabled = false;
        let update = window_tree(&[dimmed], None, empty_tree("t"), "t");

        let (_, node) = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Switch)
            .expect("the switch");
        assert!(node.is_disabled());
        assert!(!node.supports_action(otlyra_platform::accesskit::Action::Click));
    }

    #[test]
    fn an_empty_tab_still_has_a_root() {
        let update = empty_tree("New tab");
        assert_eq!(update.nodes.len(), 1);
        assert_eq!(update.focus, ROOT);
    }
}
