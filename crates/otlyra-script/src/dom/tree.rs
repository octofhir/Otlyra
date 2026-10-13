//! The DOM specification's tree algorithms, over our arena.
//!
//! Everything here is a plain function of a [`Document`]: no JavaScript, no
//! wrappers, no loan. The bindings in the sibling modules check their
//! arguments, borrow the lent document and call one of these, so that what the
//! specification says about insertion, removal, comparison and text lives in
//! one place with its section numbers beside it — and so it can be tested
//! without an isolate.

use html5ever::ns;
use html5ever::tendril::StrTendril;
use otlyra_dom::{Document, DocumentMutator, NodeData, NodeId};
use otter_runtime::marshal::JsError;

/// What a node is, as the DOM sorts nodes (DOM §4.4 `nodeType`).
///
/// Our arena has one `Document` variant for two things: the document itself
/// and a detached fragment — a `<template>`'s contents, or one script made.
/// Which it is depends on whether it is the root, which is what [`kind`]
/// answers and what nothing else should try to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An element.
    Element,
    /// A text node.
    Text,
    /// A comment.
    Comment,
    /// The document.
    Document,
    /// A document fragment.
    Fragment,
    /// A doctype.
    Doctype,
}

impl Kind {
    /// The `nodeType` constant (DOM §4.4).
    pub(crate) fn node_type(self) -> u16 {
        match self {
            Self::Element => 1,
            Self::Text => 3,
            Self::Comment => 8,
            Self::Document => 9,
            Self::Doctype => 10,
            Self::Fragment => 11,
        }
    }

    /// Whether this kind may have children at all (DOM §4.2.3 step 1).
    fn may_parent(self) -> bool {
        matches!(self, Self::Element | Self::Document | Self::Fragment)
    }
}

/// What `id` is, or `None` if it is gone.
pub(crate) fn kind(document: &Document, id: NodeId) -> Option<Kind> {
    Some(match &document.get(id)?.data {
        NodeData::Element(_) => Kind::Element,
        NodeData::Text(_) => Kind::Text,
        NodeData::Comment(_) => Kind::Comment,
        NodeData::Document if id == document.root() => Kind::Document,
        NodeData::Document => Kind::Fragment,
        NodeData::Doctype { .. } => Kind::Doctype,
    })
}

/// A `DOMException` by the name its specification gives it.
pub(crate) fn exception(name: &'static str, message: impl Into<String>) -> JsError {
    JsError::Dom {
        name,
        message: message.into(),
    }
}

/// What a binding throws for a node that is no longer in the arena.
pub(crate) fn gone() -> JsError {
    exception("NotFoundError", "the node no longer exists")
}

/// The parent of `id`, if it has one.
pub(crate) fn parent(document: &Document, id: NodeId) -> Option<NodeId> {
    document.get(id)?.parent
}

/// Whether `ancestor` is `node` or one of its ancestors.
pub(crate) fn is_inclusive_ancestor(document: &Document, ancestor: NodeId, node: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = parent(document, id);
    }
    false
}

/// The topmost inclusive ancestor of `id` — its root (DOM §4.2.2).
pub(crate) fn root_of(document: &Document, id: NodeId) -> NodeId {
    let mut current = id;
    while let Some(up) = parent(document, current) {
        current = up;
    }
    current
}

/// Every node under `root`, in tree order, `root` first.
///
/// Iterative, because the tree is the page's and its depth is the page's too.
pub(crate) fn descendants(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        order.push(id);
        let mark = stack.len();
        stack.extend(document.children(id));
        stack[mark..].reverse();
    }
    order
}

/// The element children of `id`, in order.
pub(crate) fn element_children(document: &Document, id: NodeId) -> Vec<NodeId> {
    document
        .children(id)
        .filter(|child| kind(document, *child) == Some(Kind::Element))
        .collect()
}

/// The last element child of `id`.
pub(crate) fn last_element_child(document: &Document, id: NodeId) -> Option<NodeId> {
    let mut current = document.get(id)?.last_child();
    while let Some(child) = current {
        if kind(document, child) == Some(Kind::Element) {
            return Some(child);
        }
        current = document.get(child)?.prev_sibling();
    }
    None
}

/// The data of a text or comment node.
pub(crate) fn character_data(document: &Document, id: NodeId) -> Option<&str> {
    match &document.get(id)?.data {
        NodeData::Text(data) | NodeData::Comment(data) => Some(data),
        _ => None,
    }
}

/// DOM §4.4 `textContent`, read: the text of every descendant text node for
/// an element or a fragment, the data itself for character data, and nothing
/// — `null` — for a document or a doctype.
pub(crate) fn text_content(document: &Document, id: NodeId) -> Option<String> {
    match kind(document, id)? {
        Kind::Element | Kind::Fragment => Some(descendant_text(document, id)),
        Kind::Text | Kind::Comment => character_data(document, id).map(str::to_owned),
        Kind::Document | Kind::Doctype => None,
    }
}

/// The concatenated data of every text node under `id` (DOM §4.2.4 "child
/// text content" taken over all descendants, which is `textContent`).
pub(crate) fn descendant_text(document: &Document, id: NodeId) -> String {
    let mut out = String::new();
    for node in descendants(document, id) {
        if let Some(NodeData::Text(text)) = document.get(node).map(|inner| &inner.data) {
            out.push_str(text);
        }
    }
    out
}

/// The data of `id`'s text children only — "child text content", which is what
/// `<script>` and `<title>` read.
pub(crate) fn child_text(document: &Document, id: NodeId) -> String {
    let mut out = String::new();
    for child in document.children(id) {
        if let Some(NodeData::Text(text)) = document.get(child).map(|inner| &inner.data) {
            out.push_str(text);
        }
    }
    out
}

/// DOM §4.4 `textContent`, written: "string replace all" for an element or a
/// fragment, a new value for character data, and nothing for the rest.
pub(crate) fn set_text_content(document: &mut Document, id: NodeId, value: &str) {
    match kind(document, id) {
        Some(Kind::Element | Kind::Fragment) => {
            let mut mutator = DocumentMutator::new(document);
            mutator.remove_children(id);
            if !value.is_empty() {
                mutator.append_text(id, value.into());
            }
        }
        Some(Kind::Text | Kind::Comment) => {
            DocumentMutator::new(document).set_character_data(id, value.into());
        }
        Some(Kind::Document | Kind::Doctype) | None => {}
    }
}

/// DOM §4.2.3 "ensure pre-insertion validity" of `node` into `parent` before
/// `child`.
pub(crate) fn ensure_pre_insertion_validity(
    document: &Document,
    node: NodeId,
    parent: NodeId,
    child: Option<NodeId>,
) -> Result<(), JsError> {
    let parent_kind = kind(document, parent).ok_or_else(gone)?;
    let node_kind = kind(document, node).ok_or_else(gone)?;
    if !parent_kind.may_parent() {
        return Err(exception(
            "HierarchyRequestError",
            "only a document, a fragment or an element can have children",
        ));
    }
    if is_inclusive_ancestor(document, node, parent) {
        return Err(exception(
            "HierarchyRequestError",
            "a node cannot be inserted into itself or into one of its descendants",
        ));
    }
    if let Some(child) = child
        && self::parent(document, child) != Some(parent)
    {
        return Err(exception(
            "NotFoundError",
            "the node to insert before is not a child of this node",
        ));
    }
    match node_kind {
        Kind::Document => {
            return Err(exception(
                "HierarchyRequestError",
                "a document cannot be inserted into a tree",
            ));
        }
        Kind::Text if parent_kind == Kind::Document => {
            return Err(exception(
                "HierarchyRequestError",
                "text cannot be a child of a document",
            ));
        }
        Kind::Doctype if parent_kind != Kind::Document => {
            return Err(exception(
                "HierarchyRequestError",
                "a doctype can only be a child of a document",
            ));
        }
        _ => {}
    }
    if parent_kind == Kind::Document {
        document_child_rules(document, node, node_kind, parent, child, None)?;
    }
    Ok(())
}

/// The rules a document's own children follow (DOM §4.2.3 step 6 of
/// pre-insertion and step 6 of "replace a child"): one element at most, one
/// doctype at most, and the doctype before the element.
///
/// `replacing` is the child being replaced, which is not counted as being
/// there.
fn document_child_rules(
    document: &Document,
    node: NodeId,
    node_kind: Kind,
    parent: NodeId,
    child: Option<NodeId>,
    replacing: Option<NodeId>,
) -> Result<(), JsError> {
    let refuse = || {
        Err(exception(
            "HierarchyRequestError",
            "a document has one element and one doctype, the doctype first",
        ))
    };
    let others = |wanted: Kind| {
        document
            .children(parent)
            .filter(|other| Some(*other) != replacing)
            .any(|other| kind(document, other) == Some(wanted))
    };
    // Whether a node of `wanted` kind follows `child` among the parent's
    // children — "child is non-null and a doctype is following child".
    let follows = |wanted: Kind| {
        let mut current = child.and_then(|child| document.get(child)?.next_sibling());
        while let Some(id) = current {
            if kind(document, id) == Some(wanted) {
                return true;
            }
            current = document.get(id).and_then(|inner| inner.next_sibling());
        }
        false
    };
    let child_is = |wanted: Kind| child.is_some_and(|child| kind(document, child) == Some(wanted));

    match node_kind {
        Kind::Fragment => {
            let elements = element_children(document, node).len();
            let has_text = document
                .children(node)
                .any(|inner| kind(document, inner) == Some(Kind::Text));
            if elements > 1 || has_text {
                return refuse();
            }
            if elements == 1
                && (others(Kind::Element) || child_is(Kind::Doctype) || follows(Kind::Doctype))
            {
                return refuse();
            }
        }
        Kind::Element => {
            if others(Kind::Element) || child_is(Kind::Doctype) || follows(Kind::Doctype) {
                return refuse();
            }
        }
        Kind::Doctype => {
            let element_before = match child {
                Some(child) => {
                    let mut current = document.get(child).and_then(|inner| inner.prev_sibling());
                    let mut found = false;
                    while let Some(id) = current {
                        if kind(document, id) == Some(Kind::Element) && Some(id) != replacing {
                            found = true;
                            break;
                        }
                        current = document.get(id).and_then(|inner| inner.prev_sibling());
                    }
                    found
                }
                None => others(Kind::Element),
            };
            if others(Kind::Doctype) || element_before {
                return refuse();
            }
        }
        Kind::Text | Kind::Comment | Kind::Document => {}
    }
    Ok(())
}

/// DOM §4.2.3 "pre-insert": check, then insert `node` — or, for a fragment, its
/// children — into `parent` before `child`, or at the end.
pub(crate) fn pre_insert(
    document: &mut Document,
    node: NodeId,
    parent: NodeId,
    child: Option<NodeId>,
) -> Result<(), JsError> {
    ensure_pre_insertion_validity(document, node, parent, child)?;
    // Inserting a node before itself means before whatever follows it.
    let reference = match child {
        Some(child) if child == node => document.get(node).and_then(|inner| inner.next_sibling()),
        other => other,
    };
    insert(document, node, parent, reference)
}

/// DOM §4.2.3 "insert", the part after the checks.
fn insert(
    document: &mut Document,
    node: NodeId,
    parent: NodeId,
    before: Option<NodeId>,
) -> Result<(), JsError> {
    let nodes: Vec<NodeId> = match kind(document, node) {
        Some(Kind::Fragment) => document.children(node).collect(),
        _ => vec![node],
    };
    let mut mutator = DocumentMutator::new(document);
    for inserted in nodes {
        let landed = match before {
            Some(before) => mutator.insert_before(before, inserted),
            None => mutator.append(parent, inserted),
        };
        if !landed {
            // The arena's depth limit: the one place we refuse what a browser
            // with unbounded memory would allow, and say so.
            return Err(exception(
                "HierarchyRequestError",
                "the document is nested too deeply to insert this here",
            ));
        }
    }
    Ok(())
}

/// DOM §4.2.3 "replace a child": put `node` where `child` is in `parent`.
pub(crate) fn replace_child(
    document: &mut Document,
    child: NodeId,
    node: NodeId,
    parent: NodeId,
) -> Result<(), JsError> {
    let parent_kind = kind(document, parent).ok_or_else(gone)?;
    let node_kind = kind(document, node).ok_or_else(gone)?;
    if !parent_kind.may_parent() {
        return Err(exception(
            "HierarchyRequestError",
            "only a document, a fragment or an element can have children",
        ));
    }
    if is_inclusive_ancestor(document, node, parent) {
        return Err(exception(
            "HierarchyRequestError",
            "a node cannot be inserted into itself or into one of its descendants",
        ));
    }
    if self::parent(document, child) != Some(parent) {
        return Err(exception(
            "NotFoundError",
            "the node to be replaced is not a child of this node",
        ));
    }
    match node_kind {
        Kind::Document => {
            return Err(exception(
                "HierarchyRequestError",
                "a document cannot be inserted into a tree",
            ));
        }
        Kind::Text if parent_kind == Kind::Document => {
            return Err(exception(
                "HierarchyRequestError",
                "text cannot be a child of a document",
            ));
        }
        Kind::Doctype if parent_kind != Kind::Document => {
            return Err(exception(
                "HierarchyRequestError",
                "a doctype can only be a child of a document",
            ));
        }
        _ => {}
    }
    if parent_kind == Kind::Document {
        document_child_rules(document, node, node_kind, parent, Some(child), Some(child))?;
    }
    let mut reference = document.get(child).and_then(|inner| inner.next_sibling());
    if reference == Some(node) {
        reference = document.get(node).and_then(|inner| inner.next_sibling());
    }
    DocumentMutator::new(document).detach(child);
    insert(document, node, parent, reference)
}

/// DOM §4.4 `compareDocumentPosition`'s bits.
pub(crate) mod position {
    /// The two are in different trees.
    pub(crate) const DISCONNECTED: u16 = 0x01;
    /// The other node comes first.
    pub(crate) const PRECEDING: u16 = 0x02;
    /// The other node comes after.
    pub(crate) const FOLLOWING: u16 = 0x04;
    /// The other node contains this one.
    pub(crate) const CONTAINS: u16 = 0x08;
    /// This node contains the other.
    pub(crate) const CONTAINED_BY: u16 = 0x10;
    /// The order between disconnected nodes is ours to choose.
    pub(crate) const IMPLEMENTATION_SPECIFIC: u16 = 0x20;
}

/// DOM §4.4 `compareDocumentPosition`: where `other` is relative to `this`.
pub(crate) fn compare_position(document: &Document, this: NodeId, other: NodeId) -> u16 {
    use position::{
        CONTAINED_BY, CONTAINS, DISCONNECTED, FOLLOWING, IMPLEMENTATION_SPECIFIC, PRECEDING,
    };
    if this == other {
        return 0;
    }
    let this_path = ancestry(document, this);
    let other_path = ancestry(document, other);
    if this_path.first() != other_path.first() {
        // Consistent, which is all the specification asks of it: whichever
        // node the arena allocated first is taken to come first.
        let direction = if otlyra_dom::node_id_to_u64(other) < otlyra_dom::node_id_to_u64(this) {
            PRECEDING
        } else {
            FOLLOWING
        };
        return DISCONNECTED | IMPLEMENTATION_SPECIFIC | direction;
    }
    if other_path.len() < this_path.len() && this_path[other_path.len() - 1] == other {
        return CONTAINS | PRECEDING;
    }
    if this_path.len() < other_path.len() && other_path[this_path.len() - 1] == this {
        return CONTAINED_BY | FOLLOWING;
    }
    // Where the two paths part, the earlier sibling is the earlier node.
    let shared = this_path
        .iter()
        .zip(&other_path)
        .take_while(|(a, b)| a == b)
        .count();
    let (this_branch, other_branch) = (this_path[shared], other_path[shared]);
    let mut current = document
        .get(this_branch)
        .and_then(|inner| inner.next_sibling());
    while let Some(id) = current {
        if id == other_branch {
            return FOLLOWING;
        }
        current = document.get(id).and_then(|inner| inner.next_sibling());
    }
    PRECEDING
}

/// The path from the root down to `id`, inclusive.
fn ancestry(document: &Document, id: NodeId) -> Vec<NodeId> {
    let mut path = vec![id];
    let mut current = id;
    while let Some(up) = parent(document, current) {
        path.push(up);
        current = up;
    }
    path.reverse();
    path
}

/// DOM §4.4 "equals": same kind, same name and data, same attributes in any
/// order, and children that are equal in order.
pub(crate) fn is_equal_node(document: &Document, a: NodeId, b: NodeId) -> bool {
    let mut pending = vec![(a, b)];
    while let Some((a, b)) = pending.pop() {
        let (Some(left), Some(right)) = (document.get(a), document.get(b)) else {
            return false;
        };
        let same = match (&left.data, &right.data) {
            (
                NodeData::Doctype {
                    name,
                    public_id,
                    system_id,
                },
                NodeData::Doctype {
                    name: other_name,
                    public_id: other_public,
                    system_id: other_system,
                },
            ) => name == other_name && public_id == other_public && system_id == other_system,
            (NodeData::Element(left), NodeData::Element(right)) => {
                left.name == right.name
                    && left.attrs.len() == right.attrs.len()
                    && left.attrs.iter().all(|attr| {
                        right
                            .attrs
                            .iter()
                            .any(|other| other.name == attr.name && other.value == attr.value)
                    })
            }
            (NodeData::Text(left), NodeData::Text(right))
            | (NodeData::Comment(left), NodeData::Comment(right)) => left == right,
            (NodeData::Document, NodeData::Document) => kind(document, a) == kind(document, b),
            _ => false,
        };
        if !same {
            return false;
        }
        let left_children: Vec<NodeId> = document.children(a).collect();
        let right_children: Vec<NodeId> = document.children(b).collect();
        if left_children.len() != right_children.len() {
            return false;
        }
        pending.extend(left_children.into_iter().zip(right_children));
    }
    true
}

/// DOM §4.4 `normalize()`: no empty text nodes under `root`, and no two text
/// nodes side by side — each run becomes its first node, holding the run's
/// text.
pub(crate) fn normalize(document: &mut Document, root: NodeId) {
    for node in descendants(document, root) {
        let Some(NodeData::Text(text)) = document.get(node).map(|inner| &inner.data) else {
            continue;
        };
        if text.is_empty() {
            DocumentMutator::new(document).detach(node);
            continue;
        }
        let mut merged: StrTendril = text.clone();
        let mut following = Vec::new();
        let mut current = document.get(node).and_then(|inner| inner.next_sibling());
        while let Some(id) = current {
            let Some(NodeData::Text(next)) = document.get(id).map(|inner| &inner.data) else {
                break;
            };
            merged.push_tendril(next);
            following.push(id);
            current = document.get(id).and_then(|inner| inner.next_sibling());
        }
        if following.is_empty() {
            continue;
        }
        let mut mutator = DocumentMutator::new(document);
        mutator.set_character_data(node, merged);
        for id in following {
            mutator.detach(id);
        }
    }
}

/// The document's `<html>` element's child named `name`, which is how `head`
/// and `body` are found (HTML §3.1.4, ignoring `<frameset>`, which we do not
/// lay out).
pub(crate) fn root_child_named(document: &Document, name: &str) -> Option<NodeId> {
    let root = document.first_element_child(document.root())?;
    document.children(root).find(|child| {
        document
            .get(*child)
            .and_then(|node| node.element())
            .is_some_and(|element| element.name.ns == ns!(html) && &*element.name.local == name)
    })
}

/// Text in UTF-16 code units, which is how every offset and length a DOM
/// method takes or gives is counted.
pub(crate) fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// UTF-16 back to text. A lone surrogate cannot be stored in the arena, which
/// is UTF-8; it becomes U+FFFD, which is what serializing it would do.
pub(crate) fn from_utf16(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(html: &str) -> Document {
        otlyra_html::parse(html.as_bytes(), Some("utf-8")).document
    }

    fn by_id(document: &Document, id: &str) -> NodeId {
        descendants(document, document.root())
            .into_iter()
            .find(|node| document.attr(*node, "id") == Some(id))
            .expect("the element is in the test document")
    }

    #[test]
    fn a_node_cannot_be_inserted_into_its_own_subtree() {
        let mut document = parsed("<div id=a><div id=b></div></div>");
        let (a, b) = (by_id(&document, "a"), by_id(&document, "b"));
        let error = pre_insert(&mut document, a, b, None).expect_err("a cycle is refused");
        assert!(matches!(
            error,
            JsError::Dom {
                name: "HierarchyRequestError",
                ..
            }
        ));
    }

    #[test]
    fn a_fragment_is_emptied_into_the_place_it_is_inserted() {
        let mut document = parsed("<ul id=list><li id=last></li></ul>");
        let (list, last) = (by_id(&document, "list"), by_id(&document, "last"));
        let fragment = DocumentMutator::new(&mut document).create(NodeData::Document);
        for text in ["one", "two"] {
            DocumentMutator::new(&mut document).append_text(fragment, text.into());
            let node = DocumentMutator::new(&mut document).create(NodeData::Comment(text.into()));
            DocumentMutator::new(&mut document).append(fragment, node);
        }
        pre_insert(&mut document, fragment, list, Some(last)).expect("a fragment inserts");
        assert_eq!(document.children(fragment).count(), 0);
        assert_eq!(document.children(list).count(), 5);
        assert_eq!(document.children(list).last(), Some(last));
    }

    #[test]
    fn positions_are_reported_from_the_other_nodes_side() {
        let document = parsed("<div id=a><p id=b></p></div><div id=c></div>");
        let (a, b, c) = (
            by_id(&document, "a"),
            by_id(&document, "b"),
            by_id(&document, "c"),
        );
        assert_eq!(
            compare_position(&document, a, b),
            position::CONTAINED_BY | position::FOLLOWING
        );
        assert_eq!(
            compare_position(&document, b, a),
            position::CONTAINS | position::PRECEDING
        );
        assert_eq!(compare_position(&document, c, b), position::PRECEDING);
        assert_eq!(compare_position(&document, b, c), position::FOLLOWING);
    }

    #[test]
    fn normalizing_merges_runs_and_drops_empty_text() {
        let mut document = parsed("<p id=p></p>");
        let p = by_id(&document, "p");
        let mut mutator = DocumentMutator::new(&mut document);
        for text in ["a", "", "b"] {
            let node = mutator.create(NodeData::Text(text.into()));
            mutator.append(p, node);
        }
        normalize(&mut document, p);
        assert_eq!(document.children(p).count(), 1);
        assert_eq!(descendant_text(&document, p), "ab");
    }

    #[test]
    fn a_document_takes_one_element() {
        let mut document = parsed("<html></html>");
        let root = document.root();
        let extra = DocumentMutator::new(&mut document).create_element(
            html5ever::QualName::new(None, ns!(html), html5ever::LocalName::from("div")),
            Vec::new(),
            None,
            false,
        );
        assert!(pre_insert(&mut document, extra, root, None).is_err());
    }
}
