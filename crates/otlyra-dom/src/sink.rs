//! The adapter between html5ever's tree builder and our arena.
//!
//! It holds no tree logic of its own: every method translates one tree-builder
//! request into one [`DocumentMutator`] call. That is deliberate. The tree builder
//! is replaceable — a hand-written one is a plausible future — and when it is
//! replaced this file is what gets deleted, not the DOM.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::StrTendril;
use html5ever::{Attribute, LocalName, Namespace, QualName};

use crate::mutator::DocumentMutator;
use crate::node::{NodeData, NodeId};
use crate::tree::Document;

/// An element name handed back to the tree builder.
///
/// The trait's associated type is a GAT borrowing the sink, which our arena cannot
/// satisfy while it is behind a `RefCell` — the borrow would have to outlive the
/// call, and the very next tree-builder step wants the arena mutably. Handing back
/// an owned name sidesteps that entirely, and costs two atom clones: `LocalName` and
/// `Namespace` are interned, so a clone is a refcount bump or, for the static names
/// that cover essentially all of HTML, nothing at all.
#[derive(Clone, Debug)]
pub struct OwnedName(QualName);

impl html5ever::interface::ElemName for OwnedName {
    fn ns(&self) -> &Namespace {
        &self.0.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.0.local
    }
}

/// The sink html5ever's tree builder drives.
///
/// The arena is behind a `RefCell` because every `TreeSink` method takes `&self` and
/// the tree builder calls them one after another, so the borrow is per call and
/// never spans one.
#[derive(Debug)]
pub struct DomSink {
    document: RefCell<Document>,
    parse_errors: Cell<usize>,
}

impl DomSink {
    /// A sink over a fresh document.
    pub fn new() -> Self {
        Self::with_document(Document::new())
    }

    /// A sink over an existing document.
    pub fn with_document(document: Document) -> Self {
        Self {
            document: RefCell::new(document),
            parse_errors: Cell::new(0),
        }
    }

    /// How many parse errors the tokenizer and tree builder reported.
    pub fn parse_errors(&self) -> usize {
        self.parse_errors.get()
    }

    fn mutate<T>(&self, edit: impl FnOnce(&mut DocumentMutator<'_>) -> T) -> T {
        let mut document = self.document.borrow_mut();
        edit(&mut DocumentMutator::new(&mut document))
    }
}

impl Default for DomSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeSink for DomSink {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a>
        = OwnedName
    where
        Self: 'a;

    fn finish(self) -> Document {
        self.document.into_inner()
    }

    fn parse_error(&self, message: Cow<'static, str>) {
        self.parse_errors.set(self.parse_errors.get() + 1);
        tracing::trace!(%message, "parse error");
    }

    fn get_document(&self) -> NodeId {
        self.document.borrow().root()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> OwnedName {
        let document = self.document.borrow();
        let element = document
            .node(*target)
            .element()
            .expect("elem_name on a non-element");
        OwnedName(element.name.clone())
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        self.mutate(|dom| {
            let template_contents = flags.template.then(|| dom.create(NodeData::Document));
            dom.create_element(
                name,
                attrs,
                template_contents,
                flags.mathml_annotation_xml_integration_point,
            )
        })
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.mutate(|dom| dom.create(NodeData::Comment(text)))
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        // The HTML tokenizer has no processing-instruction token; `<?x?>` becomes a
        // comment. Only xml5ever calls this.
        unreachable!("HTML parsing never produces a processing instruction")
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.mutate(|dom| match child {
            NodeOrText::AppendText(text) => dom.append_text(*parent, text),
            NodeOrText::AppendNode(node) => {
                dom.append(*parent, node);
            }
        });
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self
            .document
            .borrow()
            .get(*element)
            .is_some_and(|node| node.parent.is_some());
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        self.mutate(|dom| {
            let root = dom.document().root();
            let doctype = dom.create(NodeData::Doctype {
                name,
                public_id,
                system_id,
            });
            dom.append(root, doctype);
        });
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.document
            .borrow()
            .node(*target)
            .element()
            .and_then(|element| element.template_contents)
            .expect("get_template_contents on something that is not a template")
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.mutate(|dom| dom.set_quirks_mode(mode));
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        self.mutate(|dom| match new_node {
            NodeOrText::AppendText(text) => dom.insert_text_before(*sibling, text),
            NodeOrText::AppendNode(node) => {
                dom.insert_before(*sibling, node);
            }
        });
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        self.mutate(|dom| dom.add_attrs_if_missing(*target, attrs));
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.mutate(|dom| dom.detach(*target));
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.mutate(|dom| dom.reparent_children(*node, *new_parent));
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        self.document
            .borrow()
            .get(*handle)
            .and_then(|node| node.element())
            .is_some_and(|element| element.mathml_annotation_xml_integration_point)
    }
}
