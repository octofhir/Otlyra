//! The node classes' data, and `Node` itself.
//!
//! Each wrapper is a `NodeId` and nothing else. The nodes themselves stay in
//! the arena, which is where layout, style and paint already read them from; a
//! wrapper is a name for one, checked on every use. The id is generational, so
//! a wrapper for a node that has been destroyed finds nothing rather than
//! finding whatever was put in its slot afterwards — in a DOM that is a
//! security property and not a convenience.
//!
//! The data types form the native half of the DOM's interface tree:
//!
//! ```text
//! Node ─┬─ Element            (every HTML*, SVG* and MathML element; see interfaces.rs)
//!       ├─ CharacterData ─┬─ Text
//!       │                 └─ Comment
//!       ├─ Document
//!       ├─ DocumentFragment
//!       └─ DocumentType
//! ```
//!
//! A node has exactly one wrapper, and every binding that hands one to script
//! hands it as a [`JsNode`] — see [`super::identity`], which is the table that
//! makes `a.parentElement === a.parentElement` true, picks the interface a
//! node is made as, and is what a page's event listeners are hung on.
//!
//! Every member that changes the tree goes through [`super::tree`], which is
//! where the specification's algorithms live; what is here is argument
//! handling and the loan.

use otlyra_dom::{DocumentId, DocumentMutator, NodeData, NodeId};
use otter_macros::{HostClass, js_class};
use otter_runtime::marshal::JsError;

use super::identity::JsNode;
use super::tree::{self, Kind};
use super::{with_document, with_document_mut};

/// A node, by name.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct NodeRef {
    /// Whose node this is. Checked on every access, so a wrapper made by one
    /// page cannot read another page's tree even when the two share a thread.
    pub(crate) doc: DocumentId,
    pub(crate) id: NodeId,
}

/// An element, whatever interface it is made as.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct ElementRef {
    #[host_class(parent)]
    pub(crate) node: NodeRef,
}

/// Text or a comment: a node that is a string.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct CharacterDataRef {
    #[host_class(parent)]
    pub(crate) node: NodeRef,
}

/// A run of text.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct TextRef {
    #[host_class(parent)]
    pub(crate) data: CharacterDataRef,
}

/// A comment.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct CommentRef {
    #[host_class(parent)]
    pub(crate) data: CharacterDataRef,
}

/// The document itself, which is also a node.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct DocumentRef {
    #[host_class(parent)]
    pub(crate) node: NodeRef,
}

/// A fragment: a template's contents, or one script made to build in.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct DocumentFragmentRef {
    #[host_class(parent)]
    pub(crate) node: NodeRef,
}

/// A doctype.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct DocumentTypeRef {
    #[host_class(parent)]
    pub(crate) node: NodeRef,
}

/// What `new Node()` and its kind answer.
///
/// The abstract interfaces are reachable from `document` and constructible
/// from none of them, which is what the platform says: the constructors exist
/// so that `instanceof` works, and they throw. The constructible ones — `Text`,
/// `Comment`, `DocumentFragment` — are given constructors by the glue, which
/// makes their nodes through `document` so they get a wrapper like any other.
pub(crate) fn illegal_constructor(name: &str) -> JsError {
    JsError::Type(format!("Illegal constructor: {name}"))
}

/// A node from another document, which nothing here can move between arenas.
pub(crate) fn same_document(this: DocumentId, other: &NodeRef) -> Result<(), JsError> {
    if other.doc == this {
        Ok(())
    } else {
        Err(tree::exception(
            "WrongDocumentError",
            "the node belongs to another document",
        ))
    }
}

// ---------------------------------------------------------------- Node

#[js_class(name = "Node", feature = WEB)]
impl NodeRef {
    #[constructor]
    fn js_new() -> Result<NodeRef, JsError> {
        Err(illegal_constructor("Node"))
    }

    #[getter(name = "nodeType")]
    fn js_node_type(&self) -> Result<u32, JsError> {
        with_document(self.doc, |document| {
            tree::kind(document, self.id).map_or(0, |kind| u32::from(kind.node_type()))
        })
    }

    #[getter(name = "nodeName")]
    fn js_node_name(&self) -> Result<String, JsError> {
        with_document(self.doc, |document| {
            match document.get(self.id).map(|node| &node.data) {
                Some(NodeData::Element(element)) => super::element::tag_name(element),
                Some(NodeData::Text(_)) => "#text".to_owned(),
                Some(NodeData::Comment(_)) => "#comment".to_owned(),
                Some(NodeData::Document) if document.is_document(self.id) => "#document".to_owned(),
                Some(NodeData::Document) => "#document-fragment".to_owned(),
                Some(NodeData::Doctype { name, .. }) => name.to_string(),
                None => String::new(),
            }
        })
    }

    /// Register a mutation observer on this node. Not part of the platform:
    /// `MutationObserver.prototype.observe` checks and defaults the options
    /// and passes them as bits — child list, attributes, character data,
    /// subtree, attribute old value, character data old value, filter — with
    /// the filter's names one per line.
    #[method(name = "__observeMutations")]
    fn js_observe_mutations(
        &self,
        observer: u32,
        flags: u32,
        filter: String,
    ) -> Result<(), JsError> {
        let options = otlyra_dom::MutationOptions {
            child_list: flags & 1 != 0,
            attributes: flags & 2 != 0,
            character_data: flags & 4 != 0,
            subtree: flags & 8 != 0,
            attribute_old_value: flags & 16 != 0,
            character_data_old_value: flags & 32 != 0,
            attribute_filter: (flags & 64 != 0)
                .then(|| filter.split('\n').map(str::to_owned).collect()),
        };
        let id = self.id;
        super::with_document_unmarked(self.doc, |document| {
            document.observe_mutations(observer, id, options);
        })
    }

    /// Whether the node is in the page's own tree, through shadow hosts: what
    /// decides whether a script runs, a custom element connects or a picture
    /// is drawn. Not part of the platform; `isConnected` is also true in a
    /// document script made.
    #[method(name = "__inPage")]
    fn js_in_page(&self) -> Result<bool, JsError> {
        with_document(self.doc, |document| document.is_connected(self.id))
    }

    #[getter(name = "isConnected")]
    fn js_is_connected(&self) -> Result<bool, JsError> {
        // Through any shadow roots: a node in a shadow tree is connected
        // when its host is (DOM §4.4).
        with_document(self.doc, |document| document.is_in_a_document(self.id))
    }

    /// The document a node belongs to; `null` for the document itself.
    #[getter(name = "ownerDocument")]
    fn js_owner_document(&self) -> Result<Option<JsNode>, JsError> {
        with_document(self.doc, |document| {
            (!document.is_document(self.id))
                .then(|| JsNode::new(self.doc, document.node_document(self.id)))
        })
    }

    #[getter(name = "parentNode")]
    fn js_parent_node(&self) -> Result<Option<JsNode>, JsError> {
        with_document(self.doc, |document| {
            tree::parent(document, self.id).map(|parent| JsNode::new(self.doc, parent))
        })
    }

    #[getter(name = "parentElement")]
    fn js_parent_element(&self) -> Result<Option<JsNode>, JsError> {
        with_document(self.doc, |document| {
            tree::parent(document, self.id)
                .filter(|parent| tree::kind(document, *parent) == Some(Kind::Element))
                .map(|parent| JsNode::new(self.doc, parent))
        })
    }

    /// The children, as a list the glue makes a `NodeList` of.
    #[getter(name = "childNodes")]
    fn js_child_nodes(&self) -> Result<Vec<JsNode>, JsError> {
        with_document(self.doc, |document| {
            document
                .children(self.id)
                .map(|child| JsNode::new(self.doc, child))
                .collect()
        })
    }

    #[getter(name = "firstChild")]
    fn js_first_child(&self) -> Result<Option<JsNode>, JsError> {
        self.link(|node| node.first_child())
    }

    #[getter(name = "lastChild")]
    fn js_last_child(&self) -> Result<Option<JsNode>, JsError> {
        self.link(|node| node.last_child())
    }

    #[getter(name = "previousSibling")]
    fn js_previous_sibling(&self) -> Result<Option<JsNode>, JsError> {
        self.link(|node| node.prev_sibling())
    }

    #[getter(name = "nextSibling")]
    fn js_next_sibling(&self) -> Result<Option<JsNode>, JsError> {
        self.link(|node| node.next_sibling())
    }

    /// The data of text and comments; `null` for everything else.
    #[getter(name = "nodeValue")]
    fn js_node_value(&self) -> Result<Option<String>, JsError> {
        with_document(self.doc, |document| {
            tree::character_data(document, self.id).map(str::to_owned)
        })
    }

    #[setter(name = "nodeValue")]
    fn js_set_node_value(&mut self, value: Option<String>) -> Result<(), JsError> {
        let id = self.id;
        with_document_mut(self.doc, |document| {
            if tree::character_data(document, id).is_some() {
                DocumentMutator::new(document)
                    .set_character_data(id, value.unwrap_or_default().as_str().into());
            }
        })
    }

    #[getter(name = "textContent")]
    fn js_text_content(&self) -> Result<Option<String>, JsError> {
        with_document(self.doc, |document| tree::text_content(document, self.id))
    }

    #[setter(name = "textContent")]
    fn js_set_text_content(&mut self, value: Option<String>) -> Result<(), JsError> {
        let id = self.id;
        with_document_mut(self.doc, |document| {
            tree::set_text_content(document, id, value.as_deref().unwrap_or_default());
        })
    }

    #[method(name = "hasChildNodes")]
    fn js_has_child_nodes(&self) -> Result<bool, JsError> {
        with_document(self.doc, |document| {
            document.children(self.id).next().is_some()
        })
    }

    /// Whether `other` is this node or one of its descendants (DOM §4.4).
    #[method(name = "contains", length = 1)]
    fn js_contains(&self, other: Option<NodeRef>) -> Result<bool, JsError> {
        let Some(other) = other.filter(|other| other.doc == self.doc) else {
            return Ok(false);
        };
        with_document(self.doc, |document| {
            tree::is_inclusive_ancestor(document, self.id, other.id)
        })
    }

    #[method(name = "appendChild", length = 1)]
    fn js_append_child(&self, node: NodeRef) -> Result<JsNode, JsError> {
        same_document(self.doc, &node)?;
        let parent = self.id;
        with_document_mut(self.doc, |document| {
            tree::pre_insert(document, node.id, parent, None)
        })??;
        Ok(JsNode::new(self.doc, node.id))
    }

    #[method(name = "insertBefore", length = 2)]
    fn js_insert_before(&self, node: NodeRef, child: Option<NodeRef>) -> Result<JsNode, JsError> {
        same_document(self.doc, &node)?;
        if let Some(child) = &child {
            same_document(self.doc, child)?;
        }
        let parent = self.id;
        with_document_mut(self.doc, |document| {
            tree::pre_insert(document, node.id, parent, child.map(|child| child.id))
        })??;
        Ok(JsNode::new(self.doc, node.id))
    }

    /// Put `node` where `child` is, and hand back `child` (DOM §4.4).
    #[method(name = "replaceChild", length = 2)]
    fn js_replace_child(&self, node: NodeRef, child: NodeRef) -> Result<JsNode, JsError> {
        same_document(self.doc, &node)?;
        same_document(self.doc, &child)?;
        let parent = self.id;
        with_document_mut(self.doc, |document| {
            tree::replace_child(document, child.id, node.id, parent)
        })??;
        Ok(JsNode::new(self.doc, child.id))
    }

    #[method(name = "removeChild", length = 1)]
    fn js_remove_child(&self, child: NodeRef) -> Result<JsNode, JsError> {
        same_document(self.doc, &child)?;
        let parent = self.id;
        let ours = with_document(self.doc, |document| tree::parent(document, child.id))?;
        if ours != Some(parent) {
            return Err(tree::exception(
                "NotFoundError",
                "the node to be removed is not a child of this node",
            ));
        }
        with_document_mut(self.doc, |document| {
            DocumentMutator::new(document).detach(child.id);
        })?;
        Ok(JsNode::new(self.doc, child.id))
    }

    /// A copy of this node, and with `deep` of everything under it; the copy
    /// is detached (DOM §4.4).
    #[method(name = "cloneNode")]
    fn js_clone_node(&self, deep: Option<bool>) -> Result<JsNode, JsError> {
        let id = self.id;
        let copy = with_document_mut(self.doc, |document| {
            let mut mutator = DocumentMutator::new(document);
            if deep.unwrap_or(false) {
                mutator.deep_clone(id)
            } else {
                mutator.shallow_clone(id)
            }
        })?;
        copy.map(|copy| JsNode::new(self.doc, copy))
            .ok_or_else(tree::gone)
    }

    #[method(name = "isEqualNode", length = 1)]
    fn js_is_equal_node(&self, other: Option<NodeRef>) -> Result<bool, JsError> {
        let Some(other) = other.filter(|other| other.doc == self.doc) else {
            return Ok(false);
        };
        with_document(self.doc, |document| {
            tree::is_equal_node(document, self.id, other.id)
        })
    }

    #[method(name = "isSameNode", length = 1)]
    fn js_is_same_node(&self, other: Option<NodeRef>) -> bool {
        other.is_some_and(|other| other.doc == self.doc && other.id == self.id)
    }

    #[method(name = "compareDocumentPosition", length = 1)]
    fn js_compare_document_position(&self, other: NodeRef) -> Result<u32, JsError> {
        if other.doc != self.doc {
            // Two documents are two trees, which is the disconnected case.
            return Ok(u32::from(
                tree::position::DISCONNECTED
                    | tree::position::IMPLEMENTATION_SPECIFIC
                    | tree::position::FOLLOWING,
            ));
        }
        with_document(self.doc, |document| {
            u32::from(tree::compare_position(document, self.id, other.id))
        })
    }

    #[method(name = "normalize")]
    fn js_normalize(&self) -> Result<(), JsError> {
        let id = self.id;
        with_document_mut(self.doc, |document| tree::normalize(document, id))
    }

    /// The topmost ancestor (DOM §4.4). With no shadow trees there is nothing
    /// for `composed` to cross, so the options are not read.
    #[method(name = "getRootNode")]
    fn js_get_root_node(&self) -> Result<JsNode, JsError> {
        with_document(self.doc, |document| {
            JsNode::new(self.doc, tree::root_of(document, self.id))
        })
    }
}

impl NodeRef {
    /// A wrapper for `id` in `doc`.
    pub(crate) fn of(doc: DocumentId, id: NodeId) -> Self {
        Self { doc, id }
    }

    /// One of the node's four links, as a node script can hold.
    fn link(
        &self,
        pick: impl FnOnce(&otlyra_dom::Node) -> Option<NodeId>,
    ) -> Result<Option<JsNode>, JsError> {
        with_document(self.doc, |document| {
            document
                .get(self.id)
                .and_then(pick)
                .map(|id| JsNode::new(self.doc, id))
        })
    }
}
