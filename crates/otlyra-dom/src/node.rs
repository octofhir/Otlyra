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
