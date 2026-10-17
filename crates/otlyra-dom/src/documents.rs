//! Documents besides the page's own: what `createHTMLDocument`, `createDocument`
//! and `DOMParser` answer with.
//!
//! Each is a document node of its own in this arena, beside the page's root, the
//! way a `<template>`'s contents already are. That keeps every native working on
//! them unchanged — a node is a node wherever its tree is rooted — and keeps them
//! out of everything that renders, runs scripts or loads pictures, which all ask
//! [`Document::is_connected`], and that is about the page's own tree only.
//!
//! What has to be remembered is a node's *node document* (DOM §4.4): for a node
//! in a tree it is the document at the tree's root, and for one that is not, the
//! document it was made by or last belonged to. Only the second needs storing, and
//! only for the documents that are not the page's, so a page that never makes one
//! keeps two empty maps and pays nothing.

use std::collections::HashMap;

use crate::node::NodeId;
use crate::tree::Document;

/// What kind of document a secondary one is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryDocument {
    /// Its `contentType`: `text/html` for an HTML document, an XML type for the
    /// others.
    pub content_type: String,
}

impl SecondaryDocument {
    /// Whether it is an HTML document, which decides how names are cased and
    /// what `createElement` makes.
    pub fn is_html(&self) -> bool {
        self.content_type == "text/html"
    }
}

#[derive(Debug, Default)]
pub(crate) struct Documents {
    /// The document nodes that are documents, not fragments.
    pub(crate) kinds: HashMap<NodeId, SecondaryDocument>,
    /// The node document of a detached tree whose document is a secondary one,
    /// by the tree's root. A tree rooted in a document needs no entry.
    pub(crate) owners: HashMap<NodeId, NodeId>,
}

impl Documents {
    pub(crate) fn any(&self) -> bool {
        !self.kinds.is_empty()
    }
}

impl Document {
    /// Whether `node` is a document: the page's own, or a secondary one.
    pub fn is_document(&self, node: NodeId) -> bool {
        node == self.root() || self.documents.kinds.contains_key(&node)
    }

    /// The secondary document `node` is, if it is one.
    pub fn secondary_document(&self, node: NodeId) -> Option<&SecondaryDocument> {
        self.documents.kinds.get(&node)
    }

    /// The document `node` belongs to (DOM §4.4 "node document"): the one at
    /// the root of its tree — through shadow hosts — or, for a tree with none,
    /// the one it was made by or taken out of. A document is its own.
    pub fn node_document(&self, node: NodeId) -> NodeId {
        if !self.documents.any() {
            return self.root();
        }
        let mut current = node;
        loop {
            if self.is_document(current) {
                return current;
            }
            if let Some(owner) = self.documents.owners.get(&current) {
                return *owner;
            }
            match self.get(current).and_then(|data| data.parent) {
                Some(parent) => current = parent,
                None => match self.shadow_host_of(current) {
                    Some(host) => current = host,
                    None => return self.root(),
                },
            }
        }
    }

    /// Whether `node`'s tree, through shadow hosts, is rooted in a document —
    /// any document, which is what `isConnected` asks (DOM §4.4). Not the
    /// question the renderer asks; that is [`Document::is_connected`].
    pub fn is_in_a_document(&self, node: NodeId) -> bool {
        let mut current = node;
        loop {
            let root = self.tree_root_of(current);
            if self.is_document(root) {
                return true;
            }
            match self.shadow_host_of(root) {
                Some(host) => current = host,
                None => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use html5ever::{LocalName, QualName, ns};

    use super::*;
    use crate::{DocumentMutator, NodeData};

    fn element(mutator: &mut DocumentMutator<'_>, name: &str) -> NodeId {
        mutator.create_element(
            QualName::new(None, ns!(html), LocalName::from(name)),
            Vec::new(),
            None,
            false,
        )
    }

    fn html() -> SecondaryDocument {
        SecondaryDocument {
            content_type: "text/html".to_owned(),
        }
    }

    #[test]
    fn a_node_belongs_to_the_document_that_made_it_until_it_moves() {
        let mut document = Document::new();
        let page = document.root();
        let (other, made, child) = {
            let mut mutator = DocumentMutator::new(&mut document);
            let other = mutator.create_document(html());
            let made = element(&mut mutator, "div");
            mutator.set_node_document(made, other);
            let child = element(&mut mutator, "span");
            mutator.append(made, child);
            (other, made, child)
        };
        assert!(document.is_document(other));
        assert_eq!(document.node_document(made), other, "made by it");
        assert_eq!(document.node_document(child), other, "in a tree it made");
        assert!(!document.is_in_a_document(made));

        DocumentMutator::new(&mut document).append(other, made);
        assert!(document.is_in_a_document(child));
        assert!(
            !document.is_connected(child),
            "not the page's, so not drawn"
        );

        // Taken out, it is still the other document's.
        DocumentMutator::new(&mut document).detach(child);
        assert_eq!(document.node_document(child), other);

        // Put in the page, it is the page's, and stays so when taken out again.
        DocumentMutator::new(&mut document).append(page, child);
        assert_eq!(document.node_document(child), page);
        DocumentMutator::new(&mut document).detach(child);
        assert_eq!(document.node_document(child), page);
    }

    #[test]
    fn a_copy_is_of_the_original_s_document() {
        let mut document = Document::new();
        let mut mutator = DocumentMutator::new(&mut document);
        let other = mutator.create_document(html());
        let made = element(&mut mutator, "p");
        mutator.append(other, made);
        let copy = mutator.deep_clone(made).expect("a copy");
        let copied_document = mutator.shallow_clone(other).expect("a copy");
        assert_eq!(document.node_document(copy), other);
        assert!(document.is_document(copied_document));
        assert!(matches!(
            document.get(copied_document).map(|node| &node.data),
            Some(NodeData::Document)
        ));
    }

    #[test]
    fn a_page_without_other_documents_remembers_nothing() {
        let mut document = Document::new();
        let made = element(&mut DocumentMutator::new(&mut document), "div");
        assert_eq!(document.node_document(made), document.root());
        assert!(document.documents.owners.is_empty());
    }
}
