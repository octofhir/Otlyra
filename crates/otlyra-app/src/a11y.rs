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
use otlyra_platform::accesskit::{Node, NodeId, Rect, Role, Tree, TreeId, TreeUpdate};

use crate::page::PageScene;

/// The identifier of the tree's root. Everything else takes its identifier from
/// the box it describes, which is already unique and already stable across a
/// frame.
const ROOT: NodeId = NodeId(0);

/// Build the tree for `page`, titled `title`.
pub fn tree_for(page: &PageScene, title: &str) -> TreeUpdate {
    let boxes = page.boxes();
    let mut nodes = Vec::new();

    let mut root = Node::new(Role::Document);
    root.set_label(title.to_owned());

    let children = collect(page, boxes, boxes.root(), &mut nodes);
    root.set_children(children);
    nodes.push((ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(Tree::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: ROOT,
    }
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

/// Walk a box's children, appending the nodes they produce, and return their ids.
fn collect(
    page: &PageScene,
    boxes: &BoxTree,
    id: BoxId,
    nodes: &mut Vec<(NodeId, Node)>,
) -> Vec<NodeId> {
    let mut ids = Vec::new();

    for &child in &boxes.node(id).children {
        let node = boxes.node(child);

        // Text is a leaf with a value; everything else is a container that may or
        // may not be worth a node of its own.
        if let BoxKind::Text(text) = &node.kind {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut label = Node::new(Role::Label);
            label.set_value(trimmed.to_owned());
            let node_id = identifier(child);
            nodes.push((node_id, label));
            ids.push(node_id);
            continue;
        }

        let grandchildren = collect(page, boxes, child, nodes);
        let Some(role) = role_of(node.tag.as_ref().map(|tag| tag.as_ref())) else {
            // A box with no role of its own — an anonymous wrapper, a plain `<div>`
            // — contributes its children rather than a level of nesting nobody
            // would want read out.
            ids.extend(grandchildren);
            continue;
        };

        let mut accessible = Node::new(role);
        accessible.set_children(grandchildren);
        if let Some(rect) = page.rect_of(child) {
            accessible.set_bounds(Rect::new(
                f64::from(rect.x),
                f64::from(rect.y),
                f64::from(rect.right()),
                f64::from(rect.bottom()),
            ));
        }
        if role == Role::Link
            && let Some(href) = page.href_of(child)
        {
            accessible.set_url(href);
        }
        if let Some(level) = heading_level(node.tag.as_ref().map(|tag| tag.as_ref())) {
            accessible.set_level(level);
        }

        let node_id = identifier(child);
        nodes.push((node_id, accessible));
        ids.push(node_id);
    }

    ids
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

    #[test]
    fn an_empty_tab_still_has_a_root() {
        let update = empty_tree("New tab");
        assert_eq!(update.nodes.len(), 1);
        assert_eq!(update.focus, ROOT);
    }
}
