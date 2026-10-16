//! Shadow trees (DOM §4.2.2) and the flat tree they make with the document
//! (CSS Scoping §2.1).
//!
//! A shadow root is a fragment node the document keeps beside a host, not
//! under it: it is not one of the host's children, so nothing that walks the
//! document's tree — selectors, `querySelector`, serialization — reaches into
//! it. What is rendered is the flat tree instead, where a host's children are
//! its shadow root's, and a `<slot>`'s are the host's children assigned to it.
//!
//! # Contents
//! - [`Document::shadow_root_of`], [`Document::shadow_host_of`] — the link.
//! - [`Document::flat_children`] — what a node's children are when rendered.
//! - [`Document::assigned_nodes`], [`Document::assigned_slot`] — slotting.
//! - [`Document::is_connected`] — in the document, through any shadow roots.

use std::collections::HashMap;

use crate::{Document, NodeData, NodeId};

/// Whether script outside a shadow tree may reach into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowMode {
    /// `element.shadowRoot` answers it.
    Open,
    /// It does not.
    Closed,
}

/// The document's shadow roots, both ways round.
#[derive(Clone, Debug, Default)]
pub(crate) struct ShadowTrees {
    roots: HashMap<NodeId, (NodeId, ShadowMode)>,
    hosts: HashMap<NodeId, NodeId>,
    /// The text of the constructed style sheets each shadow root adopts.
    pub(crate) adopted: HashMap<NodeId, Vec<String>>,
}

impl ShadowTrees {
    pub(crate) fn attach(&mut self, host: NodeId, root: NodeId, mode: ShadowMode) {
        self.roots.insert(host, (root, mode));
        self.hosts.insert(root, host);
    }
}

impl Document {
    /// The shadow root attached to `host`, and its mode.
    pub fn shadow_root_of(&self, host: NodeId) -> Option<(NodeId, ShadowMode)> {
        self.shadow.roots.get(&host).copied()
    }

    /// The host `root` is attached to, if it is a shadow root.
    pub fn shadow_host_of(&self, root: NodeId) -> Option<NodeId> {
        self.shadow.hosts.get(&root).copied()
    }

    /// The text of the style sheets `root` adopts (CSSOM §6.4.1
    /// `adoptedStyleSheets`), which apply after its `<style>` elements.
    pub fn adopted_styles(&self, root: NodeId) -> &[String] {
        self.shadow.adopted.get(&root).map_or(&[], Vec::as_slice)
    }

    /// Every shadow root, with its host.
    pub fn shadow_roots(&self) -> impl Iterator<Item = (NodeId, NodeId)> + '_ {
        self.shadow
            .roots
            .iter()
            .map(|(host, (root, _))| (*host, *root))
    }

    /// Whether the document has any shadow roots at all: what lets the flat
    /// tree cost nothing on a page without them.
    pub fn has_shadow_trees(&self) -> bool {
        !self.shadow.roots.is_empty()
    }

    /// The root of `node`'s tree: the document, a shadow root, or the top of
    /// a detached subtree.
    pub fn tree_root_of(&self, node: NodeId) -> NodeId {
        let mut current = node;
        while let Some(parent) = self.get(current).and_then(|data| data.parent) {
            current = parent;
        }
        current
    }

    /// The shadow root `node` is in, if it is in one.
    pub fn containing_shadow_root(&self, node: NodeId) -> Option<NodeId> {
        let root = self.tree_root_of(node);
        self.shadow_host_of(root).map(|_| root)
    }

    /// Whether `node` is in the document (DOM §4.2 "connected"): its tree is
    /// the document's, or a shadow tree whose host is.
    pub fn is_connected(&self, node: NodeId) -> bool {
        let mut current = node;
        loop {
            let root = self.tree_root_of(current);
            if root == self.root() {
                return true;
            }
            match self.shadow_host_of(root) {
                Some(host) => current = host,
                None => return false,
            }
        }
    }

    /// Whether `node` is a `<slot>` in a shadow tree.
    fn is_slot(&self, node: NodeId) -> bool {
        self.get(node)
            .and_then(|data| data.element())
            .is_some_and(|element| &*element.name.local == "slot")
            && self.containing_shadow_root(node).is_some()
    }

    /// The name a slot takes, or that a slottable asks for: the `name` or
    /// `slot` attribute, empty for the default slot (DOM §4.2.2.1).
    fn slot_name<'a>(&'a self, node: NodeId, attribute: &str) -> &'a str {
        self.attr(node, attribute).unwrap_or("")
    }

    /// The slot in `root` named `name`: the first in tree order.
    fn find_slot(&self, root: NodeId, name: &str) -> Option<NodeId> {
        let mut stack: Vec<NodeId> = self.children(root).collect();
        stack.reverse();
        while let Some(node) = stack.pop() {
            if self.is_slot(node) && self.slot_name(node, "name") == name {
                return Some(node);
            }
            let mut children: Vec<NodeId> = self.children(node).collect();
            children.reverse();
            stack.extend(children);
        }
        None
    }

    /// The slot `node` — a child of a shadow host — is assigned to, if any
    /// (DOM §4.2.2.3 "find a slot"). Only elements and text can be slotted.
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        let data = self.get(node)?;
        if !matches!(data.data, NodeData::Element(_) | NodeData::Text(_)) {
            return None;
        }
        let host = data.parent?;
        let (root, _) = self.shadow_root_of(host)?;
        let name = match &data.data {
            NodeData::Element(_) => self.slot_name(node, "slot"),
            _ => "",
        };
        self.find_slot(root, name)
    }

    /// What `slot` is assigned: the host's children that find it, in order
    /// (DOM §4.2.2.3 "find slottables").
    pub fn assigned_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        let Some(root) = self.containing_shadow_root(slot) else {
            return Vec::new();
        };
        let Some(host) = self.shadow_host_of(root) else {
            return Vec::new();
        };
        self.children(host)
            .filter(|&child| self.assigned_slot(child) == Some(slot))
            .collect()
    }

    /// `node`'s children in the flat tree: a host's shadow root's children,
    /// a slot's assigned nodes — or its own children when nothing is assigned
    /// — and anything else's own children.
    pub fn flat_children(&self, node: NodeId) -> Vec<NodeId> {
        if !self.has_shadow_trees() {
            return self.children(node).collect();
        }
        if let Some((root, _)) = self.shadow_root_of(node) {
            return self.children(root).collect();
        }
        if self.is_slot(node) {
            let assigned = self.assigned_nodes(node);
            if !assigned.is_empty() {
                return assigned;
            }
        }
        self.children(node).collect()
    }

    /// `node`'s parent in the flat tree: a slotted node's slot, a shadow
    /// root's child's host, and otherwise its parent. A host's child that no
    /// slot takes is not in the flat tree, and has none.
    pub fn flat_parent(&self, node: NodeId) -> Option<NodeId> {
        let parent = self.get(node)?.parent?;
        if !self.has_shadow_trees() {
            return Some(parent);
        }
        if self.shadow_root_of(parent).is_some() {
            return self.assigned_slot(node);
        }
        if let Some(host) = self.shadow_host_of(parent) {
            return Some(host);
        }
        Some(parent)
    }
}
