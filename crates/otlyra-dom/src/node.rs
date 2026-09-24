//! What a node is, and how nodes are linked to each other.

use html5ever::tendril::StrTendril;
use html5ever::{Attribute, QualName};
use slotmap::new_key_type;

new_key_type! {
    /// A handle to a node in a [`Document`](crate::Document).
    ///
    /// An index plus a generation, so a handle to a removed node resolves to
    /// `None` rather than to whichever node reused the slot. In a DOM that is a
    /// security property: script holds node references indefinitely, and a stale
    /// one that silently aliases a live node is a use-after-free wearing a safe
    /// type.
    pub struct NodeId;
}

/// An element: its name, its attributes, and the two flags the tree builder needs
/// us to remember for it.
#[derive(Clone, Debug)]
pub struct ElementData {
    /// Namespace-qualified tag name.
    pub name: QualName,
    /// Attributes, in tree-order of first appearance.
    pub attrs: Vec<Attribute>,
    /// For `<template>`, the separate fragment its children go into.
    pub template_contents: Option<NodeId>,
    /// Set on a MathML `<annotation-xml>` whose `encoding` makes it an HTML
    /// integration point. The tree builder asks; we only have to remember.
    pub mathml_annotation_xml_integration_point: bool,
}

/// Everything a node can be.
#[derive(Clone, Debug)]
pub enum NodeData {
    /// The document root, or a `<template>`'s detached contents.
    Document,
    /// A doctype. Its three strings are kept verbatim; quirks mode is decided by
    /// the tree builder, not here.
    Doctype {
        /// The name, `html` in anything modern.
        name: StrTendril,
        /// The public identifier, empty when absent.
        public_id: StrTendril,
        /// The system identifier, empty when absent.
        system_id: StrTendril,
    },
    /// Character data.
    Text(StrTendril),
    /// A comment.
    Comment(StrTendril),
    /// An element.
    Element(ElementData),
}

/// A node, and its four links into the tree.
///
/// The links are `Option<NodeId>` rather than a `Vec<NodeId>` of children: the tree
/// builder inserts before a sibling, detaches a subtree and reparents every child of
/// one node onto another, and a linked structure makes all three O(1) instead of a
/// shift of everything after the insertion point.
#[derive(Clone, Debug)]
pub struct Node {
    /// What this node is.
    pub data: NodeData,
    /// The parent, absent for the root and for a detached node.
    pub parent: Option<NodeId>,
    pub(crate) first_child: Option<NodeId>,
    pub(crate) last_child: Option<NodeId>,
    pub(crate) prev_sibling: Option<NodeId>,
    pub(crate) next_sibling: Option<NodeId>,
}

/// A [`NodeId`] as a plain number, and back.
///
/// Some of what a browser hands to other machinery — hit-test identifiers, the
/// style engine's opaque node ids — is a number, and a generational key already is
/// one. Round-tripping it here keeps the conversion beside the type.
pub fn node_id_to_u64(id: NodeId) -> u64 {
    use slotmap::Key as _;
    id.data().as_ffi()
}

/// The inverse of [`node_id_to_u64`].
pub fn node_id_from_u64(raw: u64) -> NodeId {
    NodeId::from(slotmap::KeyData::from_ffi(raw))
}

impl ElementData {
    /// One attribute's value, by local name, in no namespace.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attr_in(&html5ever::ns!(), name)
    }

    /// One attribute's value, by namespace and local name — for the few a
    /// document writes with a prefix, such as `xlink:href`.
    pub fn attr_in(&self, namespace: &html5ever::Namespace, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|attr| attr.name.ns == *namespace && attr.name.local.as_ref() == name)
            .map(|attr| attr.value.as_ref())
    }

    /// The address an attribute names — a `poster`, an `src`, an object's
    /// `data` — without the ASCII whitespace a URL may be written inside
    /// (HTML §2.5.2, "valid URL potentially surrounded by spaces"), or `None`
    /// when nothing is left to fetch.
    ///
    /// Not yet resolved: that takes the document's base URL, which the caller
    /// has and the element does not.
    pub fn address(&self, name: &str) -> Option<&str> {
        self.attr(name)
            .map(str::trim_ascii)
            .filter(|address| !address.is_empty())
    }

    /// Where the element links to, if it is a link.
    ///
    /// An HTML `<a>` or `<area>` with an `href` (HTML §4.6.1), and an SVG `<a>`
    /// with one — as `href` (SVG 2 §16.2) or as `xlink:href`, the SVG 1.1
    /// spelling every drawing program still exports. An `<a>` without one is a
    /// place in the page rather than a way out of it, and is not a link.
    pub fn link_href(&self) -> Option<&str> {
        match (&self.name.ns, self.name.local.as_ref()) {
            (&html5ever::ns!(html), "a" | "area") => self.attr("href"),
            (&html5ever::ns!(svg), "a") => self
                .attr("href")
                .or_else(|| self.attr_in(&html5ever::ns!(xlink), "href")),
            _ => None,
        }
    }

    /// The element's `id`, if it has one.
    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    /// The element's classes, in source order.
    ///
    /// Split on ASCII whitespace, which is what `class` is: a set written as a
    /// space-separated list, not a single name.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attr("class")
            .unwrap_or_default()
            .split_ascii_whitespace()
    }

    /// Whether the element carries `class`.
    pub fn has_class(&self, class: &str, case_sensitive: bool) -> bool {
        self.classes().any(|candidate| {
            if case_sensitive {
                candidate == class
            } else {
                candidate.eq_ignore_ascii_case(class)
            }
        })
    }
}

impl Node {
    pub(crate) fn new(data: NodeData) -> Self {
        Self {
            data,
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
        }
    }

    /// The element data, if this node is an element.
    pub fn element(&self) -> Option<&ElementData> {
        match &self.data {
            NodeData::Element(element) => Some(element),
            _ => None,
        }
    }

    /// The first child.
    pub fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    /// The last child.
    pub fn last_child(&self) -> Option<NodeId> {
        self.last_child
    }

    /// The previous sibling.
    pub fn prev_sibling(&self) -> Option<NodeId> {
        self.prev_sibling
    }

    /// The next sibling.
    pub fn next_sibling(&self) -> Option<NodeId> {
        self.next_sibling
    }
}
