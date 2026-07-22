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

/// Build the tree for `page`, titled `title`.
pub fn tree_for(page: &PageScene, title: &str) -> TreeUpdate {
    let mut nodes = Vec::new();

    let mut root = Node::new(Role::Document);
    root.set_label(title.to_owned());

    let children = to_nodes(&describe_page(page), &mut nodes);
    root.set_children(children);
    nodes.push((ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(Tree::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: ROOT,
    }
}

/// One accessible thing on the page, before it is anything a platform knows.
///
/// The middle of the walk, split out so the panel that *shows* the tree and the
/// adapter that *hands it to a reader* are two views of one answer. A panel that
/// walked the boxes itself would be a second account of what a page exposes, and
/// the first bug in it would be the two disagreeing.
#[derive(Clone, Debug)]
pub struct Accessible {
    /// The box it came from, which is also its identity in the platform tree.
    pub box_id: BoxId,
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
        let id = identifier(item.box_id);
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
                box_id: child,
                node: node.node,
                role: Role::Label,
                value: Some(trimmed.to_owned()),
                level: None,
                url: None,
                bounds: None,
                children: Vec::new(),
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
            box_id: child,
            node: node.node,
            role,
            value: None,
            level: heading_level(node.tag.as_ref().map(|tag| tag.as_ref())),
            url: (role == Role::Link).then(|| page.href_of(child)).flatten(),
            bounds: page.rect_of(child).map(|rect| {
                Rect::new(
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.right()),
                    f64::from(rect.bottom()),
                )
            }),
            children: grandchildren,
        });
    }

    out
}

/// A box's identifier in the tree.
///
/// Offset by one so that nothing collides with the root, whose identifier is zero
/// because it belongs to no box.
fn identifier(id: BoxId) -> NodeId {
    NodeId(otlyra_layout::box_id_to_u64(id).wrapping_add(1))
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
