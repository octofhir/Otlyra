//! The arena, and everything you can ask it without changing it.

use html5ever::interface::QuirksMode;
use slotmap::SlotMap;

use crate::limits::DomLimits;
use crate::node::{Node, NodeData, NodeId};

/// A parsed document: one arena of nodes, plus the handful of facts that belong to
/// the document rather than to any node in it.
///
/// Reading is done through this type; every change goes through
/// [`DocumentMutator`](crate::DocumentMutator). Keeping the two apart is what lets
/// the HTML tree sink be a thin adapter rather than a second implementation of the
/// tree, so replacing the parser later does not mean rewriting the DOM.
#[derive(Debug)]
pub struct Document {
    nodes: SlotMap<NodeId, Node>,
    root: NodeId,
    quirks_mode: QuirksMode,
    limits: DomLimits,
    refused: usize,
}

impl Document {
    /// An empty document: a root node and nothing else.
    pub fn new() -> Self {
        Self::with_limits(DomLimits::default())
    }

    /// An empty document with explicit limits.
    pub fn with_limits(limits: DomLimits) -> Self {
        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(Node::new(NodeData::Document));
        Self {
            nodes,
            root,
            quirks_mode: QuirksMode::NoQuirks,
            limits,
            refused: 0,
        }
    }

    /// The document node. Never removed, so this handle is valid for the lifetime
    /// of the document.
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// The node behind a handle, or `None` if it has been removed.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// The node behind a handle.
    ///
    /// # Panics
    ///
    /// If the handle is stale. Use it only where the tree builder has just given us
    /// the handle and a stale one would mean the parser is broken.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// How many nodes the document holds.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the document holds nothing but its root.
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// The quirks mode the tree builder settled on.
    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks_mode
    }

    /// The limits this document enforces.
    pub fn limits(&self) -> DomLimits {
        self.limits
    }

    /// How many insertions were refused because they would have breached a limit.
    ///
    /// Non-zero means the tree is a truncation of the document, which is a thing a
    /// caller may reasonably want to say out loud.
    pub fn refused_insertions(&self) -> usize {
        self.refused
    }

    /// The element sibling before `id`, skipping text and comments.
    ///
    /// Selector matching walks siblings by element, because `p + p` means the
    /// paragraph before this one and not the whitespace between them.
    pub fn prev_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.get(id)?.prev_sibling();
        while let Some(candidate) = current {
            if self.get(candidate)?.element().is_some() {
                return Some(candidate);
            }
            current = self.get(candidate)?.prev_sibling();
        }
        None
    }

    /// The element sibling after `id`.
    pub fn next_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.get(id)?.next_sibling();
        while let Some(candidate) = current {
            if self.get(candidate)?.element().is_some() {
                return Some(candidate);
            }
            current = self.get(candidate)?.next_sibling();
        }
        None
    }

    /// The first element child of `id`.
    pub fn first_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.children(id)
            .find(|&child| self.get(child).is_some_and(|node| node.element().is_some()))
    }

    /// Whether `id` has no children that count as content — no elements, and no
    /// text that is not whitespace. This is `:empty`.
    pub fn is_empty_element(&self, id: NodeId) -> bool {
        self.children(id)
            .all(|child| match self.get(child).map(|node| &node.data) {
                Some(NodeData::Element(_)) => false,
                Some(NodeData::Text(text)) => text.is_empty(),
                _ => true,
            })
    }

    /// The children of `id`, in tree order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            document: self,
            next: self.nodes.get(id).and_then(Node::first_child),
        }
    }

    /// The distance from the root to `id`, counting the root as zero.
    ///
    /// Walks upward, and stops counting once past the depth limit — the answer is
    /// only ever used to compare against that limit.
    pub fn depth(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.nodes.get(id).and_then(|node| node.parent);
        while let Some(parent) = current {
            depth += 1;
            if depth > self.limits.max_depth {
                break;
            }
            current = self.nodes.get(parent).and_then(|node| node.parent);
        }
        depth
    }

    pub(crate) fn nodes_mut(&mut self) -> &mut SlotMap<NodeId, Node> {
        &mut self.nodes
    }

    pub(crate) fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.quirks_mode = mode;
    }

    pub(crate) fn refuse(&mut self) {
        self.refused += 1;
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over a node's children, produced by [`Document::children`].
#[derive(Debug)]
pub struct Children<'a> {
    document: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let current = self.next?;
        self.next = self.document.get(current).and_then(Node::next_sibling);
        Some(current)
    }
}
