//! `Node`, `Element`, `Text` and `Document`, as classes a page can hold.
//!
//! Each wrapper is a `NodeId` and nothing else. The nodes themselves stay in
//! the arena, which is where layout, style and paint already read them from; a
//! wrapper is a name for one, checked on every use. The id is generational, so
//! a wrapper for a node that has been destroyed finds nothing rather than
//! finding whatever was put in its slot afterwards — in a DOM that is a
//! security property and not a convenience.
//!
//! A node has exactly one wrapper, and every binding that hands one to script
//! hands it through [`Wrapped`] — see [`super::identity`], which is the table
//! that makes `a.parentElement === a.parentElement` true and is what a page's
//! event listeners are hung on.

use html5ever::{LocalName, QualName, ns};
use otlyra_dom::{Document, DocumentId, DocumentMutator, NodeData, NodeId};
use otter_macros::{HostClass, js_class};
use otter_runtime::marshal::JsError;

use super::identity::{NodeWrapper, Wrapped};
use super::{Navigation, with_document, with_document_mut};

/// A node, by name.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct NodeRef {
    /// Whose node this is. Checked on every access, so a wrapper made by one
    /// page cannot read another page's tree even when the two share a thread.
    doc: DocumentId,
    id: NodeId,
}

/// An element.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct ElementRef {
    #[host_class(parent)]
    node: NodeRef,
}

/// A run of character data.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct TextRef {
    #[host_class(parent)]
    node: NodeRef,
}

/// The document itself, which is also a node.
#[derive(Debug, Clone, Copy, HostClass)]
pub struct DocumentRef {
    #[host_class(parent)]
    node: NodeRef,
}

/// What `new Node()` and its kind answer.
///
/// Every one of these classes is reachable from `document` and constructible
/// from none of them, which is what the platform says: the constructors exist
/// so that `instanceof` works, and they throw.
fn illegal_constructor(name: &str) -> JsError {
    JsError::Type(format!("Illegal constructor: {name}"))
}

// ---------------------------------------------------------------- Node

#[js_class(name = "Node", feature = WEB)]
impl NodeRef {
    #[constructor]
    fn js_new() -> Result<NodeRef, JsError> {
        Err(illegal_constructor("Node"))
    }

    #[getter(name = "nodeType")]
    fn js_node_type(&self) -> Result<f64, JsError> {
        with_document(self.doc, |document| {
            match document.get(self.id).map(|node| &node.data) {
                Some(NodeData::Element(_)) => 1.0,
                Some(NodeData::Text(_)) => 3.0,
                Some(NodeData::Comment(_)) => 8.0,
                Some(NodeData::Doctype { .. }) => 10.0,
                Some(NodeData::Document) => 9.0,
                None => 0.0,
            }
        })
    }

    #[getter(name = "nodeName")]
    fn js_node_name(&self) -> Result<String, JsError> {
        with_document(self.doc, |document| {
            match document.get(self.id).map(|node| &node.data) {
                Some(NodeData::Element(element)) => element.name.local.to_uppercase(),
                Some(NodeData::Text(_)) => "#text".to_owned(),
                Some(NodeData::Comment(_)) => "#comment".to_owned(),
                Some(NodeData::Document) => "#document".to_owned(),
                Some(NodeData::Doctype { name, .. }) => name.to_string(),
                None => String::new(),
            }
        })
    }

    #[getter(name = "isConnected")]
    fn js_is_connected(&self) -> Result<bool, JsError> {
        with_document(self.doc, |document| is_connected(document, self.id))
    }

    #[getter(name = "parentElement")]
    fn js_parent_element(&self) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        with_document(self.doc, |document| {
            let parent = document.get(self.id)?.parent?;
            document
                .get(parent)?
                .element()
                .map(|_| Wrapped(ElementRef::of(document.id(), parent)))
        })
    }

    #[getter(name = "textContent")]
    fn js_text_content(&self) -> Result<String, JsError> {
        with_document(self.doc, |document| text_content(document, self.id))
    }

    #[setter(name = "textContent")]
    fn js_set_text_content(&mut self, value: String) -> Result<(), JsError> {
        let id = self.id;
        with_document_mut(self.doc, |document| {
            let mut mutator = DocumentMutator::new(document);
            mutator.remove_children(id);
            if !value.is_empty() {
                mutator.append_text(id, value.as_str().into());
            }
        })
    }

    #[method(name = "appendChild", length = 1)]
    fn js_append_child(&self, child: NodeRef) -> Result<Wrapped<NodeRef>, JsError> {
        let parent = self.id;
        let appended = with_document_mut(self.doc, |document| {
            let mut mutator = DocumentMutator::new(document);
            mutator.append(parent, child.id)
        })?;
        if appended {
            Ok(Wrapped(child))
        } else {
            Err(JsError::Type(
                "the node could not be appended here".to_owned(),
            ))
        }
    }

    #[method(name = "removeChild", length = 1)]
    fn js_remove_child(&self, child: NodeRef) -> Result<Wrapped<NodeRef>, JsError> {
        let parent = self.id;
        let ours = with_document(self.doc, |document| {
            document.get(child.id).and_then(|node| node.parent)
        })?;
        if ours != Some(parent) {
            return Err(JsError::Type(
                "the node to be removed is not a child of this node".to_owned(),
            ));
        }
        with_document_mut(self.doc, |document| {
            DocumentMutator::new(document).detach(child.id)
        })?;
        Ok(Wrapped(child))
    }
}

impl NodeRef {
    /// A wrapper for `id` in `doc`.
    pub(crate) fn of(doc: DocumentId, id: NodeId) -> Self {
        Self { doc, id }
    }
}

// Each of the four classes is a name for one node, which is what lets one table
// hold the wrappers of all of them: a node has exactly one wrapper, whichever
// class that wrapper turned out to be.
impl NodeWrapper for NodeRef {
    fn key(&self) -> (DocumentId, NodeId) {
        (self.doc, self.id)
    }
}

impl NodeWrapper for ElementRef {
    fn key(&self) -> (DocumentId, NodeId) {
        (self.node.doc, self.node.id)
    }
}

impl NodeWrapper for TextRef {
    fn key(&self) -> (DocumentId, NodeId) {
        (self.node.doc, self.node.id)
    }
}

impl NodeWrapper for DocumentRef {
    fn key(&self) -> (DocumentId, NodeId) {
        (self.node.doc, self.node.id)
    }
}

// ------------------------------------------------------------- Element

#[js_class(name = "Element", feature = WEB, extends = NodeRef)]
impl ElementRef {
    #[constructor]
    fn js_new() -> Result<ElementRef, JsError> {
        Err(illegal_constructor("Element"))
    }

    #[getter(name = "tagName")]
    fn js_tag_name(&self) -> Result<String, JsError> {
        with_document(self.doc(), |document| {
            document
                .get(self.id())
                .and_then(|node| node.element())
                .map(|element| element.name.local.to_uppercase())
                .unwrap_or_default()
        })
    }

    #[getter(name = "id")]
    fn js_id(&self) -> Result<String, JsError> {
        self.attribute("id")
    }

    #[setter(name = "id")]
    fn js_set_id(&mut self, value: String) -> Result<(), JsError> {
        self.set_attribute_raw("id", &value)
    }

    #[getter(name = "className")]
    fn js_class_name(&self) -> Result<String, JsError> {
        self.attribute("class")
    }

    #[setter(name = "className")]
    fn js_set_class_name(&mut self, value: String) -> Result<(), JsError> {
        self.set_attribute_raw("class", &value)
    }

    #[method(name = "getAttribute", length = 1)]
    fn js_get_attribute(&self, name: String) -> Result<Option<String>, JsError> {
        let id = self.id();
        with_document(self.doc(), |document| {
            document
                .get(id)
                .and_then(|node| node.element())
                .and_then(|element| element.attr(&name.to_lowercase()))
                .map(str::to_owned)
        })
    }

    #[method(name = "hasAttribute", length = 1)]
    fn js_has_attribute(&self, name: String) -> Result<bool, JsError> {
        Ok(self.js_get_attribute(name)?.is_some())
    }

    #[method(name = "setAttribute", length = 2)]
    fn js_set_attribute(&self, name: String, value: String) -> Result<(), JsError> {
        self.set_attribute_raw(&name.to_lowercase(), &value)
    }

    #[method(name = "removeAttribute", length = 1)]
    fn js_remove_attribute(&self, name: String) -> Result<(), JsError> {
        let id = self.id();
        let lowered = name.to_lowercase();
        with_document_mut(self.doc(), |document| {
            DocumentMutator::new(document).remove_attr(id, &lowered);
        })
    }

    #[getter(name = "children")]
    fn js_children(&self) -> Result<Vec<Wrapped<ElementRef>>, JsError> {
        let id = self.id();
        with_document(self.doc(), |document| {
            document
                .children(id)
                .filter(|child| {
                    document
                        .get(*child)
                        .is_some_and(|node| node.element().is_some())
                })
                .map(|id| Wrapped(ElementRef::of(document.id(), id)))
                .collect()
        })
    }

    #[getter(name = "firstElementChild")]
    fn js_first_element_child(&self) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        let id = self.id();
        with_document(self.doc(), |document| {
            document
                .first_element_child(id)
                .map(|found| Wrapped(ElementRef::of(document.id(), found)))
        })
    }

    #[method(name = "querySelector", length = 1)]
    fn js_query_selector(&self, selector: String) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        Ok(self.query(&selector)?.into_iter().next())
    }

    #[method(name = "querySelectorAll", length = 1)]
    fn js_query_selector_all(&self, selector: String) -> Result<Vec<Wrapped<ElementRef>>, JsError> {
        self.query(&selector)
    }

    /// `form.submit()`, which is how a redirector page moves the reader on.
    ///
    /// Recorded rather than performed: the isolate is holding the document
    /// this form is in, and navigating from here would pull it out from under
    /// the turn. The browser reads the request when the turn ends.
    #[method(name = "submit")]
    fn js_submit(&self) -> Result<(), JsError> {
        let id = self.id();
        let is_form = with_document(self.doc(), |document| {
            document
                .get(id)
                .and_then(|node| node.element())
                .is_some_and(|element| element.name.local.as_ref() == "form")
        })?;
        if !is_form {
            return Err(JsError::Type("submit() is a form's method".to_owned()));
        }
        super::request_navigation(Navigation::Submit { form: id });
        Ok(())
    }

    /// The same, with the validation a real submit does — which we do not do
    /// here yet, so it is the same.
    #[method(name = "requestSubmit")]
    fn js_request_submit(&self) -> Result<(), JsError> {
        self.js_submit()
    }

    #[method(name = "remove")]
    fn js_remove(&self) -> Result<(), JsError> {
        let id = self.id();
        with_document_mut(self.doc(), |document| {
            DocumentMutator::new(document).detach(id)
        })
    }
}

impl ElementRef {
    pub(crate) fn of(doc: DocumentId, id: NodeId) -> Self {
        Self {
            node: NodeRef::of(doc, id),
        }
    }

    fn id(&self) -> NodeId {
        self.node.id
    }

    fn doc(&self) -> DocumentId {
        self.node.doc
    }

    fn attribute(&self, name: &str) -> Result<String, JsError> {
        let id = self.id();
        with_document(self.doc(), |document| {
            document
                .get(id)
                .and_then(|node| node.element())
                .and_then(|element| element.attr(name))
                .unwrap_or_default()
                .to_owned()
        })
    }

    fn set_attribute_raw(&self, name: &str, value: &str) -> Result<(), JsError> {
        let id = self.id();
        with_document_mut(self.doc(), |document| {
            DocumentMutator::new(document).set_attr(id, name, value);
        })
    }

    /// Everything under this element that `selector` matches, in tree order.
    ///
    /// The matching engine runs over the whole document and the result is
    /// narrowed to our descendants afterwards. That is the honest way round
    /// with the engine we have — a selector like `.a .b` is answered against
    /// the ancestors it really has, which a subtree-only walk would get wrong —
    /// and it is a whole-document walk per call, which is a cost to come back
    /// to when a page makes it matter.
    fn query(&self, selector: &str) -> Result<Vec<Wrapped<ElementRef>>, JsError> {
        let id = self.id();
        with_document(self.doc(), |document| {
            let matched =
                otlyra_css::stylo_dom::select(document, selector).map_err(JsError::Type)?;
            Ok(matched
                .into_iter()
                .filter(|candidate| *candidate != id && has_ancestor(document, *candidate, id))
                .map(|id| Wrapped(ElementRef::of(document.id(), id)))
                .collect())
        })?
    }
}

// ---------------------------------------------------------------- Text

#[js_class(name = "Text", feature = WEB, extends = NodeRef)]
impl TextRef {
    #[constructor]
    fn js_new() -> Result<TextRef, JsError> {
        Err(illegal_constructor("Text"))
    }

    #[getter(name = "data")]
    fn js_data(&self) -> Result<String, JsError> {
        with_document(self.doc(), |document| text_content(document, self.node.id))
    }
}

impl TextRef {
    fn doc(&self) -> DocumentId {
        self.node.doc
    }

    pub(crate) fn of(doc: DocumentId, id: NodeId) -> Self {
        Self {
            node: NodeRef::of(doc, id),
        }
    }
}

// ------------------------------------------------------------ Document

#[js_class(name = "Document", feature = WEB, extends = NodeRef)]
impl DocumentRef {
    #[constructor]
    fn js_new() -> Result<DocumentRef, JsError> {
        Err(illegal_constructor("Document"))
    }

    /// The document of the page this isolate is running.
    ///
    /// Not part of the platform: it is how the bootstrap script gets hold of
    /// the singleton, and the bootstrap deletes it from the class afterwards.
    #[static_method(name = "__self")]
    fn js_self() -> Result<Wrapped<DocumentRef>, JsError> {
        let owner = super::lent_document().ok_or_else(|| {
            JsError::Type("there is no document for this isolate right now".to_owned())
        })?;
        with_document(owner, |document| {
            Wrapped(DocumentRef {
                node: NodeRef::of(owner, document.root()),
            })
        })
    }

    /// The document's own address, for the `location` the bootstrap builds.
    #[static_method(name = "__url")]
    fn js_url() -> String {
        super::document_url()
    }

    /// Ask the browser to go somewhere. Not part of the platform: it is what
    /// `location`'s setters and methods call.
    #[static_method(name = "__navigate")]
    fn js_navigate(href: String, replace: bool) -> Result<(), JsError> {
        if href.is_empty() {
            return Err(JsError::Type("navigation needs an address".to_owned()));
        }
        super::request_navigation(Navigation::Url { href, replace });
        Ok(())
    }

    /// Ask the browser to fetch this page again.
    #[static_method(name = "__reload")]
    fn js_reload() {
        super::request_navigation(Navigation::Reload);
    }

    /// Note that the page asked for an animation frame.
    ///
    /// Not part of the platform: `requestAnimationFrame` keeps its callbacks in
    /// JavaScript, where the collector can see them, and tells this so the
    /// browser can ask whether a frame is owed without entering the isolate to
    /// find out.
    #[static_method(name = "__frameRequested")]
    fn js_frame_requested() {
        super::note_frame_request();
    }

    #[getter(name = "documentElement")]
    fn js_document_element(&self) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        with_document(self.doc(), |document| {
            document
                .first_element_child(document.root())
                .map(|id| Wrapped(ElementRef::of(document.id(), id)))
        })
    }

    #[getter(name = "head")]
    fn js_head(&self) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        with_document(self.doc(), |document| {
            find_in_root(document, "head").map(|id| Wrapped(ElementRef::of(document.id(), id)))
        })
    }

    #[getter(name = "body")]
    fn js_body(&self) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        with_document(self.doc(), |document| {
            find_in_root(document, "body").map(|id| Wrapped(ElementRef::of(document.id(), id)))
        })
    }

    #[getter(name = "title")]
    fn js_title(&self) -> Result<String, JsError> {
        with_document(self.doc(), |document| {
            let head = find_in_root(document, "head")?;
            let title = document
                .children(head)
                .find(|child| is_element(document, *child, "title"))?;
            Some(text_content(document, title))
        })
        .map(Option::unwrap_or_default)
    }

    #[setter(name = "title")]
    fn js_set_title(&mut self, value: String) -> Result<(), JsError> {
        let head = with_document(self.doc(), |document| find_in_root(document, "head"))?;
        let Some(head) = head else {
            return Ok(());
        };
        with_document_mut(self.doc(), |document| {
            let existing = document
                .children(head)
                .find(|child| is_element(document, *child, "title"));
            let mut mutator = DocumentMutator::new(document);
            let title = match existing {
                Some(title) => title,
                None => {
                    let created = mutator.create_element(
                        QualName::new(None, ns!(html), LocalName::from("title")),
                        Vec::new(),
                        None,
                        false,
                    );
                    mutator.append(head, created);
                    created
                }
            };
            mutator.remove_children(title);
            if !value.is_empty() {
                mutator.append_text(title, value.as_str().into());
            }
        })
    }

    /// Always `"visible"`, and honestly so: we have one window and we draw it.
    #[getter(name = "visibilityState")]
    fn js_visibility_state(&self) -> String {
        "visible".to_owned()
    }

    #[getter(name = "hidden")]
    fn js_hidden(&self) -> bool {
        false
    }

    /// `"loading"` while the parser is still running, `"complete"` after.
    ///
    /// There is no `"interactive"`: the step it names is the one between the
    /// last byte and the subresources, and we do not run script in it yet.
    #[getter(name = "readyState")]
    fn js_ready_state(&self) -> String {
        if super::is_ready() {
            "complete".to_owned()
        } else {
            "loading".to_owned()
        }
    }

    #[method(name = "getElementById", length = 1)]
    fn js_get_element_by_id(&self, id: String) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        with_document(self.doc(), |document| {
            descendants(document, document.root())
                .find(|candidate| {
                    document
                        .get(*candidate)
                        .and_then(|node| node.element())
                        .and_then(|element| element.attr("id"))
                        == Some(id.as_str())
                })
                .map(|id| Wrapped(ElementRef::of(document.id(), id)))
        })
    }

    #[method(name = "querySelector", length = 1)]
    fn js_query_selector(&self, selector: String) -> Result<Option<Wrapped<ElementRef>>, JsError> {
        Ok(select_all(self.doc(), &selector)?.into_iter().next())
    }

    #[method(name = "querySelectorAll", length = 1)]
    fn js_query_selector_all(&self, selector: String) -> Result<Vec<Wrapped<ElementRef>>, JsError> {
        select_all(self.doc(), &selector)
    }

    #[method(name = "createElement", length = 1)]
    fn js_create_element(&self, name: String) -> Result<Wrapped<ElementRef>, JsError> {
        let local = LocalName::from(name.to_lowercase());
        with_document_mut(self.doc(), |document| {
            let created = DocumentMutator::new(document).create_element(
                QualName::new(None, ns!(html), local),
                Vec::new(),
                None,
                false,
            );
            Wrapped(ElementRef::of(document.id(), created))
        })
    }

    #[method(name = "createTextNode", length = 1)]
    fn js_create_text_node(&self, data: String) -> Result<Wrapped<TextRef>, JsError> {
        with_document_mut(self.doc(), |document| {
            let created =
                DocumentMutator::new(document).create(NodeData::Text(data.as_str().into()));
            Wrapped(TextRef::of(document.id(), created))
        })
    }
}

impl DocumentRef {
    fn doc(&self) -> DocumentId {
        self.node.doc
    }
}

// --------------------------------------------------------------- Shared

/// Everything `selector` matches in the document, in tree order.
fn select_all(owner: DocumentId, selector: &str) -> Result<Vec<Wrapped<ElementRef>>, JsError> {
    with_document(owner, |document| {
        otlyra_css::stylo_dom::select(document, selector)
            .map(|matched| {
                matched
                    .into_iter()
                    .map(|id| Wrapped(ElementRef::of(owner, id)))
                    .collect()
            })
            .map_err(JsError::Type)
    })?
}

/// All the text under a node, run together.
fn text_content(document: &Document, node: NodeId) -> String {
    let mut out = String::new();
    for id in descendants(document, node) {
        if let Some(NodeData::Text(text)) = document.get(id).map(|inner| &inner.data) {
            out.push_str(text);
        }
    }
    out
}

/// Every node under `root`, in tree order, `root` first.
fn descendants(document: &Document, root: NodeId) -> impl Iterator<Item = NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        order.push(id);
        let children: Vec<NodeId> = document.children(id).collect();
        stack.extend(children.into_iter().rev());
    }
    order.into_iter()
}

/// Whether `candidate` is somewhere under `ancestor`.
fn has_ancestor(document: &Document, candidate: NodeId, ancestor: NodeId) -> bool {
    let mut walk = document.get(candidate).and_then(|node| node.parent);
    while let Some(id) = walk {
        if id == ancestor {
            return true;
        }
        walk = document.get(id).and_then(|node| node.parent);
    }
    false
}

/// Whether the node is an element with this local name.
fn is_element(document: &Document, id: NodeId, name: &str) -> bool {
    document
        .get(id)
        .and_then(|node| node.element())
        .is_some_and(|element| element.name.local.as_ref() == name)
}

/// Whether the node is still attached to the document's root.
fn is_connected(document: &Document, id: NodeId) -> bool {
    let root = document.root();
    id == root || has_ancestor(document, id, root)
}

/// `<head>` or `<body>`, found under the root element.
fn find_in_root(document: &Document, name: &str) -> Option<NodeId> {
    let root = document.first_element_child(document.root())?;
    document
        .children(root)
        .find(|child| is_element(document, *child, name))
}

otter_macros::romp! {
    name = "dom",
    ident = DOM_EXTENSION,
    classes = [
        NodeRefIntrinsic,
        ElementRefIntrinsic,
        TextRefIntrinsic,
        DocumentRefIntrinsic,
    ],
    js = [
        (include_str!("dom.js"), defines = [
            "window",
            "self",
            "top",
            "parent",
            "document",
            "matchMedia",
            "requestAnimationFrame",
            "cancelAnimationFrame",
            "addEventListener",
            "removeEventListener",
            "dispatchEvent",
            "setTimeout",
            "setInterval",
            "clearTimeout",
            "clearInterval",
            "__otlyraFlushDeferred",
            "location",
        ]),
    ],
}
