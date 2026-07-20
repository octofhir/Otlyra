//! The only way to change a document.
//!
//! Every tree edit the HTML tree builder asks for is one call here, so the sink can
//! stay an adapter and the link surgery lives in one place with one set of tests.

use html5ever::tendril::StrTendril;
use html5ever::{Attribute, QualName};

use crate::node::{ElementData, Node, NodeData, NodeId};
use crate::tree::Document;

/// A mutable view of a [`Document`].
#[derive(Debug)]
pub struct DocumentMutator<'a> {
    document: &'a mut Document,
}

impl<'a> DocumentMutator<'a> {
    /// Borrow `document` for mutation.
    pub fn new(document: &'a mut Document) -> Self {
        Self { document }
    }

    /// Read the document being mutated.
    pub fn document(&self) -> &Document {
        self.document
    }

    /// Create a detached node.
    pub fn create(&mut self, data: NodeData) -> NodeId {
        self.document.nodes_mut().insert(Node::new(data))
    }

    /// Create a detached element.
    ///
    /// Attributes beyond the limit are dropped rather than truncating the document:
    /// an element with ten thousand attributes is an attack on whatever walks them,
    /// not content anyone wrote.
    pub fn create_element(
        &mut self,
        name: QualName,
        mut attrs: Vec<Attribute>,
        template_contents: Option<NodeId>,
        mathml_annotation_xml_integration_point: bool,
    ) -> NodeId {
        let limit = self.document.limits().max_attrs_per_element;
        if attrs.len() > limit {
            attrs.truncate(limit);
            self.document.refuse();
        }
        self.create(NodeData::Element(ElementData {
            name,
            attrs,
            template_contents,
            mathml_annotation_xml_integration_point,
        }))
    }

    /// Set the document's quirks mode.
    pub fn set_quirks_mode(&mut self, mode: html5ever::interface::QuirksMode) {
        self.document.set_quirks_mode(mode);
    }

    /// Append `child` as the last child of `parent`.
    ///
    /// Returns `false` if the insertion was refused for depth, in which case `child`
    /// stays detached and the document records the refusal.
    pub fn append(&mut self, parent: NodeId, child: NodeId) -> bool {
        if !self.may_insert_under(parent) {
            return false;
        }
        self.detach(child);

        let last = self.document.node(parent).last_child();
        {
            let node = &mut self.document.nodes_mut()[child];
            node.parent = Some(parent);
            node.prev_sibling = last;
        }
        match last {
            Some(last) => self.document.nodes_mut()[last].next_sibling = Some(child),
            None => self.document.nodes_mut()[parent].first_child = Some(child),
        }
        self.document.nodes_mut()[parent].last_child = Some(child);
        true
    }

    /// Append text as the last child of `parent`, merging into an existing trailing
    /// text node rather than creating a second one.
    ///
    /// The merge is not an optimization: the DOM has no concept of two adjacent text
    /// nodes produced by parsing, and every tree-construction test says so.
    pub fn append_text(&mut self, parent: NodeId, text: StrTendril) {
        if let Some(last) = self.document.node(parent).last_child()
            && let NodeData::Text(existing) = &mut self.document.nodes_mut()[last].data
        {
            existing.push_tendril(&text);
            return;
        }
        let node = self.create(NodeData::Text(text));
        self.append(parent, node);
    }

    /// Insert `new_node` immediately before `sibling`.
    pub fn insert_before(&mut self, sibling: NodeId, new_node: NodeId) -> bool {
        let Some(parent) = self.document.node(sibling).parent else {
            return false;
        };
        if !self.may_insert_under(parent) {
            return false;
        }
        self.detach(new_node);

        let previous = self.document.node(sibling).prev_sibling();
        {
            let node = &mut self.document.nodes_mut()[new_node];
            node.parent = Some(parent);
            node.prev_sibling = previous;
            node.next_sibling = Some(sibling);
        }
        match previous {
            Some(previous) => self.document.nodes_mut()[previous].next_sibling = Some(new_node),
            None => self.document.nodes_mut()[parent].first_child = Some(new_node),
        }
        self.document.nodes_mut()[sibling].prev_sibling = Some(new_node);
        true
    }

    /// Insert text immediately before `sibling`, merging into the text node already
    /// there if there is one.
    pub fn insert_text_before(&mut self, sibling: NodeId, text: StrTendril) {
        if let Some(previous) = self.document.node(sibling).prev_sibling()
            && let NodeData::Text(existing) = &mut self.document.nodes_mut()[previous].data
        {
            existing.push_tendril(&text);
            return;
        }
        let node = self.create(NodeData::Text(text));
        self.insert_before(sibling, node);
    }

    /// Unlink `target` from its parent and siblings. Its own subtree is untouched.
    pub fn detach(&mut self, target: NodeId) {
        let (parent, previous, next) = {
            let node = &mut self.document.nodes_mut()[target];
            (
                node.parent.take(),
                node.prev_sibling.take(),
                node.next_sibling.take(),
            )
        };
        let Some(parent) = parent else { return };

        match previous {
            Some(previous) => self.document.nodes_mut()[previous].next_sibling = next,
            None => self.document.nodes_mut()[parent].first_child = next,
        }
        match next {
            Some(next) => self.document.nodes_mut()[next].prev_sibling = previous,
            None => self.document.nodes_mut()[parent].last_child = previous,
        }
    }

    /// Move every child of `node` to the end of `new_parent`, in order.
    ///
    /// This is the adoption agency's primitive, so it has to be a move rather than a
    /// copy: the tree builder still holds handles to those children.
    pub fn reparent_children(&mut self, node: NodeId, new_parent: NodeId) {
        while let Some(child) = self.document.node(node).first_child() {
            if !self.append(new_parent, child) {
                // Refused for depth. Detach anyway, or the loop never ends.
                self.detach(child);
            }
        }
    }

    /// Add each attribute that the element does not already have.
    pub fn add_attrs_if_missing(&mut self, target: NodeId, attrs: Vec<Attribute>) {
        let limit = self.document.limits().max_attrs_per_element;
        let mut refused = false;
        if let NodeData::Element(element) = &mut self.document.nodes_mut()[target].data {
            for attr in attrs {
                if element
                    .attrs
                    .iter()
                    .any(|existing| existing.name == attr.name)
                {
                    continue;
                }
                if element.attrs.len() >= limit {
                    refused = true;
                    break;
                }
                element.attrs.push(attr);
            }
        }
        if refused {
            self.document.refuse();
        }
    }

    /// Whether a child may be inserted under `parent` without breaching the depth
    /// limit. Records the refusal if not.
    fn may_insert_under(&mut self, parent: NodeId) -> bool {
        if self.document.depth(parent) < self.document.limits().max_depth {
            return true;
        }
        self.document.refuse();
        false
    }
}
