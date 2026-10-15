//! `Document`, `DocumentFragment` and `DocumentType`.
//!
//! The document is the one node script can always reach and the one that
//! makes the others: every node a page creates is created here, in the lent
//! arena, and is detached until something inserts it.

use html5ever::{LocalName, Namespace, Prefix, QualName, ns};
use otlyra_dom::{DocumentMutator, NodeData};
use otter_macros::js_class;
use otter_runtime::marshal::JsError;

use super::element::{self, is_valid_attribute_name};
use super::identity::JsNode;
use super::node::{
    DocumentFragmentRef, DocumentRef, DocumentTypeRef, NodeRef, illegal_constructor, same_document,
};
use super::tree;
use super::{Navigation, with_document, with_document_mut};

/// Whether `name` may name an element.
///
/// The DOM's "valid element local name" (2025): an ASCII letter first, then
/// anything but ASCII whitespace, NUL, `/` and `>` — or, starting with a
/// non-ASCII character, the XML name characters, which are approximated here
/// by refusing the same few. Narrow enough that `createElement("<div>")` still
/// throws, which is what the check is for.
fn is_valid_element_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == ':' || !first.is_ascii())
        && !name.contains(|letter: char| {
            letter.is_ascii_whitespace() || matches!(letter, '\0' | '/' | '>' | '<')
        })
}

fn invalid_name(name: &str) -> JsError {
    tree::exception(
        "InvalidCharacterError",
        format!("{name:?} is not a valid element name"),
    )
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
    /// Not part of the platform: it is how the glue gets hold of the
    /// singleton, and the glue removes it from the class afterwards.
    #[static_method(name = "__self")]
    fn js_self() -> Result<JsNode, JsError> {
        let owner = super::lent_document().ok_or_else(|| {
            JsError::Type("there is no document for this isolate right now".to_owned())
        })?;
        with_document(owner, |document| JsNode::new(owner, document.root()))
    }

    /// The document's own address, for the `location` the glue builds.
    #[static_method(name = "__url")]
    fn js_url() -> String {
        super::document_url()
    }

    #[static_method(name = "__timeMetric")]
    fn js_time_metric(which: u32) -> f64 {
        super::time_metric(which)
    }

    #[static_method(name = "__windowMetric")]
    fn js_window_metric(which: u32) -> f64 {
        super::window_metric(which)
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
    fn js_document_element(&self) -> Result<Option<JsNode>, JsError> {
        element::first_element_child(self.node)
    }

    #[getter(name = "head")]
    fn js_head(&self) -> Result<Option<JsNode>, JsError> {
        let doc = self.node.doc;
        with_document(doc, |document| {
            tree::root_child_named(document, "head").map(|id| JsNode::new(doc, id))
        })
    }

    #[getter(name = "body")]
    fn js_body(&self) -> Result<Option<JsNode>, JsError> {
        let doc = self.node.doc;
        with_document(doc, |document| {
            tree::root_child_named(document, "body").map(|id| JsNode::new(doc, id))
        })
    }

    #[getter(name = "doctype")]
    fn js_doctype(&self) -> Result<Option<JsNode>, JsError> {
        let doc = self.node.doc;
        with_document(doc, |document| {
            document
                .children(document.root())
                .find(|child| tree::kind(document, *child) == Some(tree::Kind::Doctype))
                .map(|id| JsNode::new(doc, id))
        })
    }

    /// The text of the first `<title>`, with its white space stripped and
    /// collapsed (HTML §3.1.3).
    #[getter(name = "title")]
    fn js_title(&self) -> Result<String, JsError> {
        with_document(self.node.doc, |document| {
            let title = tree::descendants(document, document.root())
                .into_iter()
                .find(|id| {
                    document
                        .get(*id)
                        .and_then(|node| node.element())
                        .is_some_and(|element| {
                            &*element.name.local == "title"
                                && matches!(element.name.ns, ns!(html) | ns!(svg))
                        })
                });
            title.map_or_else(String::new, |title| {
                tree::child_text(document, title)
                    .split_ascii_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
    }

    #[setter(name = "title")]
    fn js_set_title(&mut self, value: String) -> Result<(), JsError> {
        let doc = self.node.doc;
        let head = with_document(doc, |document| tree::root_child_named(document, "head"))?;
        let Some(head) = head else {
            return Ok(());
        };
        with_document_mut(doc, |document| {
            let existing = document.children(head).find(|child| {
                document
                    .get(*child)
                    .and_then(|node| node.element())
                    .is_some_and(|element| &*element.name.local == "title")
            });
            let title = match existing {
                Some(title) => title,
                None => {
                    let mut mutator = DocumentMutator::new(document);
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
            tree::set_text_content(document, title, &value);
        })
    }

    /// The visibility of this page, including while it is being parsed.
    #[getter(name = "visibilityState")]
    fn js_visibility_state(&self) -> String {
        if super::is_hidden() {
            "hidden"
        } else {
            "visible"
        }
        .to_owned()
    }

    #[getter(name = "hidden")]
    fn js_hidden(&self) -> bool {
        super::is_hidden()
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

    /// The `<script>` element whose code is running, or `null` outside one.
    #[getter(name = "currentScript")]
    fn js_current_script(&self) -> Option<JsNode> {
        super::current_script().map(|element| JsNode::new(self.node.doc, element))
    }

    /// `"BackCompat"` in quirks mode, `"CSS1Compat"` otherwise (DOM §4.5).
    #[getter(name = "compatMode")]
    fn js_compat_mode(&self) -> Result<String, JsError> {
        with_document(self.node.doc, |document| {
            match document.quirks_mode() {
                html5ever::interface::QuirksMode::Quirks => "BackCompat",
                _ => "CSS1Compat",
            }
            .to_owned()
        })
    }

    #[getter(name = "children")]
    fn js_children(&self) -> Result<Vec<JsNode>, JsError> {
        element::children_of(self.node)
    }

    #[getter(name = "childElementCount")]
    fn js_child_element_count(&self) -> Result<u32, JsError> {
        element::child_element_count(self.node)
    }

    #[getter(name = "firstElementChild")]
    fn js_first_element_child(&self) -> Result<Option<JsNode>, JsError> {
        element::first_element_child(self.node)
    }

    #[getter(name = "lastElementChild")]
    fn js_last_element_child(&self) -> Result<Option<JsNode>, JsError> {
        element::last_element_child(self.node)
    }

    #[method(name = "getElementById", length = 1)]
    fn js_get_element_by_id(&self, id: String) -> Result<Option<JsNode>, JsError> {
        by_id(self.node, &id)
    }

    #[method(name = "querySelector", length = 1)]
    fn js_query_selector(&self, selector: String) -> Result<Option<JsNode>, JsError> {
        Ok(element::query(self.node, &selector, true)?
            .into_iter()
            .next())
    }

    #[method(name = "querySelectorAll", length = 1)]
    fn js_query_selector_all(&self, selector: String) -> Result<Vec<JsNode>, JsError> {
        element::query(self.node, &selector, false)
    }

    #[method(name = "getElementsByTagName", length = 1)]
    fn js_get_elements_by_tag_name(&self, name: String) -> Result<Vec<JsNode>, JsError> {
        element::by_tag_name(self.node, &name)
    }

    #[method(name = "getElementsByTagNameNS", length = 2)]
    fn js_get_elements_by_tag_name_ns(
        &self,
        ns: Option<String>,
        local: String,
    ) -> Result<Vec<JsNode>, JsError> {
        element::by_tag_name_ns(self.node, ns, &local)
    }

    #[method(name = "getElementsByClassName", length = 1)]
    fn js_get_elements_by_class_name(&self, names: String) -> Result<Vec<JsNode>, JsError> {
        element::by_class_name(self.node, &names)
    }

    #[method(name = "getElementsByName", length = 1)]
    fn js_get_elements_by_name(&self, name: String) -> Result<Vec<JsNode>, JsError> {
        element::by_name(self.node, &name)
    }

    /// An HTML element, its name lowercased (DOM §4.5 `createElement`).
    #[method(name = "createElement", length = 1)]
    fn js_create_element(&self, name: String) -> Result<JsNode, JsError> {
        if !is_valid_element_name(&name) {
            return Err(invalid_name(&name));
        }
        let local = LocalName::from(name.to_ascii_lowercase());
        self.create_element(QualName::new(None, ns!(html), local))
    }

    /// An element in any namespace — SVG, above all, where a framework builds
    /// every icon this way (DOM §4.5 `createElementNS`).
    #[method(name = "createElementNS", length = 2)]
    fn js_create_element_ns(
        &self,
        ns: Option<String>,
        qualified_name: String,
    ) -> Result<JsNode, JsError> {
        let namespace = match ns {
            Some(ns) if !ns.is_empty() => Namespace::from(ns),
            _ => ns!(),
        };
        let (prefix, local) = match qualified_name.split_once(':') {
            Some((prefix, local)) => (Some(prefix), local),
            None => (None, qualified_name.as_str()),
        };
        if !is_valid_element_name(local)
            || prefix.is_some_and(|prefix| !is_valid_attribute_name(prefix))
        {
            return Err(invalid_name(&qualified_name));
        }
        if prefix.is_some() && namespace == ns!() {
            return Err(tree::exception(
                "NamespaceError",
                format!("{qualified_name:?} has a prefix and no namespace"),
            ));
        }
        self.create_element(QualName::new(
            prefix.map(Prefix::from),
            namespace,
            LocalName::from(local),
        ))
    }

    #[method(name = "createTextNode", length = 1)]
    fn js_create_text_node(&self, data: String) -> Result<JsNode, JsError> {
        self.create(NodeData::Text(data.as_str().into()))
    }

    #[method(name = "createComment", length = 1)]
    fn js_create_comment(&self, data: String) -> Result<JsNode, JsError> {
        self.create(NodeData::Comment(data.as_str().into()))
    }

    #[method(name = "createDocumentFragment")]
    fn js_create_document_fragment(&self) -> Result<JsNode, JsError> {
        self.create(NodeData::Document)
    }

    /// A copy of `node` for this document (DOM §4.5). The page has one arena,
    /// so a node of ours is copied rather than translated.
    #[method(name = "importNode", length = 1)]
    fn js_import_node(&self, node: NodeRef, deep: Option<bool>) -> Result<JsNode, JsError> {
        same_document(self.node.doc, &node)?;
        let copy = with_document_mut(self.node.doc, |document| {
            let mut mutator = DocumentMutator::new(document);
            if deep.unwrap_or(false) {
                mutator.deep_clone(node.id)
            } else {
                mutator.shallow_clone(node.id)
            }
        })?;
        copy.map(|copy| JsNode::new(self.node.doc, copy))
            .ok_or_else(tree::gone)
    }

    /// Take `node` out of wherever it is, to be inserted elsewhere (DOM §4.5).
    #[method(name = "adoptNode", length = 1)]
    fn js_adopt_node(&self, node: NodeRef) -> Result<JsNode, JsError> {
        same_document(self.node.doc, &node)?;
        if node.id == with_document(self.node.doc, otlyra_dom::Document::root)? {
            return Err(tree::exception(
                "NotSupportedError",
                "a document cannot be adopted",
            ));
        }
        with_document_mut(self.node.doc, |document| {
            DocumentMutator::new(document).detach(node.id);
        })?;
        Ok(JsNode::new(self.node.doc, node.id))
    }
}

impl DocumentRef {
    fn create(&self, data: NodeData) -> Result<JsNode, JsError> {
        let doc = self.node.doc;
        with_document_mut(doc, |document| {
            JsNode::new(doc, DocumentMutator::new(document).create(data))
        })
    }

    fn create_element(&self, name: QualName) -> Result<JsNode, JsError> {
        let doc = self.node.doc;
        // A `<template>` made by script has contents like a parsed one.
        let is_template = name.ns == ns!(html) && &*name.local == "template";
        with_document_mut(doc, |document| {
            let mut mutator = DocumentMutator::new(document);
            let contents = is_template.then(|| mutator.create(NodeData::Document));
            JsNode::new(
                doc,
                mutator.create_element(name, Vec::new(), contents, false),
            )
        })
    }
}

/// The first element under `root` with this `id`, in tree order.
fn by_id(root: NodeRef, id: &str) -> Result<Option<JsNode>, JsError> {
    if id.is_empty() {
        return Ok(None);
    }
    with_document(root.doc, |document| {
        tree::descendants(document, root.id)
            .into_iter()
            .skip(1)
            .find(|candidate| document.attr(*candidate, "id") == Some(id))
            .map(|found| JsNode::new(root.doc, found))
    })
}

// ---------------------------------------------------- DocumentFragment

#[js_class(name = "DocumentFragment", feature = WEB, extends = NodeRef)]
impl DocumentFragmentRef {
    #[constructor]
    fn js_new() -> Result<DocumentFragmentRef, JsError> {
        Err(illegal_constructor("DocumentFragment"))
    }

    #[method(name = "getElementById", length = 1)]
    fn js_get_element_by_id(&self, id: String) -> Result<Option<JsNode>, JsError> {
        by_id(self.node, &id)
    }

    #[method(name = "querySelector", length = 1)]
    fn js_query_selector(&self, selector: String) -> Result<Option<JsNode>, JsError> {
        Ok(element::query(self.node, &selector, true)?
            .into_iter()
            .next())
    }

    #[method(name = "querySelectorAll", length = 1)]
    fn js_query_selector_all(&self, selector: String) -> Result<Vec<JsNode>, JsError> {
        element::query(self.node, &selector, false)
    }

    #[getter(name = "children")]
    fn js_children(&self) -> Result<Vec<JsNode>, JsError> {
        element::children_of(self.node)
    }

    #[getter(name = "childElementCount")]
    fn js_child_element_count(&self) -> Result<u32, JsError> {
        element::child_element_count(self.node)
    }

    #[getter(name = "firstElementChild")]
    fn js_first_element_child(&self) -> Result<Option<JsNode>, JsError> {
        element::first_element_child(self.node)
    }

    #[getter(name = "lastElementChild")]
    fn js_last_element_child(&self) -> Result<Option<JsNode>, JsError> {
        element::last_element_child(self.node)
    }
}

// -------------------------------------------------------- DocumentType

#[js_class(name = "DocumentType", feature = WEB, extends = NodeRef)]
impl DocumentTypeRef {
    #[constructor]
    fn js_new() -> Result<DocumentTypeRef, JsError> {
        Err(illegal_constructor("DocumentType"))
    }

    #[getter(name = "name")]
    fn js_name(&self) -> Result<String, JsError> {
        self.field(|name, _, _| name)
    }

    #[getter(name = "publicId")]
    fn js_public_id(&self) -> Result<String, JsError> {
        self.field(|_, public_id, _| public_id)
    }

    #[getter(name = "systemId")]
    fn js_system_id(&self) -> Result<String, JsError> {
        self.field(|_, _, system_id| system_id)
    }
}

impl DocumentTypeRef {
    fn field(
        &self,
        pick: impl for<'a> FnOnce(&'a str, &'a str, &'a str) -> &'a str,
    ) -> Result<String, JsError> {
        with_document(self.node.doc, |document| {
            match document.get(self.node.id).map(|node| &node.data) {
                Some(NodeData::Doctype {
                    name,
                    public_id,
                    system_id,
                }) => Some(pick(name, public_id, system_id).to_owned()),
                _ => None,
            }
        })?
        .ok_or_else(tree::gone)
    }
}
