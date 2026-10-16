//! `Element`: names, attributes, element-only traversal and selectors.
//!
//! An attribute name is a *qualified* name to the DOM — `xlink:href` is one
//! name, not a namespace and a local name — and on an HTML element in an HTML
//! document it is lowercased first (DOM §4.9). On an SVG element it is not:
//! `viewBox` stays `viewBox`, which is the difference between an icon a
//! framework built and a blank square.

use html5ever::{LocalName, Namespace, Prefix, QualName, ns};
use otlyra_dom::{Document, DocumentMutator, ElementData, NodeId};
use otter_macros::{IntoJs, js_class};
use otter_runtime::marshal::JsError;

use super::identity::JsNode;
use super::markup;
use super::node::{ElementRef, NodeRef, illegal_constructor};
use super::tree;
use super::{Navigation, with_document, with_document_mut};

/// One attribute as script's `Attr` describes it, for the glue's
/// `NamedNodeMap`.
#[derive(Debug, IntoJs)]
pub struct AttrRecord {
    /// The qualified name.
    name: String,
    /// The value.
    value: String,
    /// The namespace, or `null`.
    #[js(name = "namespaceURI")]
    namespace_uri: Option<String>,
    /// The prefix, or `null`.
    prefix: Option<String>,
    /// The local name.
    #[js(name = "localName")]
    local_name: String,
}

/// An element's `tagName`: its qualified name, uppercased for an HTML element
/// (DOM §4.9 "HTML-uppercased qualified name").
pub(crate) fn tag_name(element: &ElementData) -> String {
    let qualified = match &element.name.prefix {
        Some(prefix) => format!("{prefix}:{}", element.name.local),
        None => element.name.local.to_string(),
    };
    if element.name.ns == ns!(html) {
        qualified.to_ascii_uppercase()
    } else {
        qualified
    }
}

/// An attribute's qualified name.
fn qualified(attr: &html5ever::Attribute) -> String {
    match &attr.name.prefix {
        Some(prefix) => format!("{prefix}:{}", attr.name.local),
        None => attr.name.local.to_string(),
    }
}

/// Whether `name` may name an attribute.
///
/// The DOM's "valid attribute local name" (2025): anything non-empty without
/// ASCII whitespace, NUL, `/`, `=` or `>`. Narrower than what a parser accepts
/// in markup, wider than XML's `Name`, and what every engine now checks.
pub(crate) fn is_valid_attribute_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(|letter: char| {
            letter.is_ascii_whitespace() || matches!(letter, '\0' | '/' | '=' | '>')
        })
}

/// The empty string means no namespace, in every method that takes one.
fn namespace(given: Option<String>) -> Namespace {
    match given {
        Some(given) if !given.is_empty() => Namespace::from(given),
        _ => ns!(),
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
        self.read_element(tag_name)
    }

    #[getter(name = "localName")]
    fn js_local_name(&self) -> Result<String, JsError> {
        self.read_element(|element| element.name.local.to_string())
    }

    #[getter(name = "namespaceURI")]
    fn js_namespace_uri(&self) -> Result<Option<String>, JsError> {
        self.read_element(|element| (element.name.ns != ns!()).then(|| element.name.ns.to_string()))
    }

    #[getter(name = "prefix")]
    fn js_prefix(&self) -> Result<Option<String>, JsError> {
        self.read_element(|element| element.name.prefix.as_ref().map(ToString::to_string))
    }

    #[getter(name = "id")]
    fn js_id(&self) -> Result<String, JsError> {
        Ok(self.get("id")?.unwrap_or_default())
    }

    #[setter(name = "id")]
    fn js_set_id(&mut self, value: String) -> Result<(), JsError> {
        self.set("id", &value)
    }

    #[getter(name = "className")]
    fn js_class_name(&self) -> Result<String, JsError> {
        Ok(self.get("class")?.unwrap_or_default())
    }

    /// Whether this `<script>` element is already started, and mark it so
    /// when `mark` is set. Not part of the platform: the glue that runs the
    /// scripts script inserts asks it.
    #[method(name = "__scriptStarted")]
    fn js_script_started(&self, mark: bool) -> Result<bool, JsError> {
        let id = self.node.id;
        super::with_document_unmarked(self.node.doc, |document| {
            let started = document.script_started(id);
            if mark && !started {
                otlyra_dom::DocumentMutator::new(document).mark_script_started(id);
            }
            started
        })
    }

    /// Make this `<script>` element `document.currentScript`. Not part of the
    /// platform.
    #[method(name = "__becomeCurrentScript")]
    fn js_become_current_script(&self) {
        super::set_current_script(Some(self.node.id));
    }

    /// Hand the cascade what script made of this `<style>` element's sheet,
    /// as text. Not part of the platform: the CSSOM glue calls it once a
    /// turn after `insertRule` or `deleteRule`.
    #[method(name = "__setCssomText")]
    fn js_set_cssom_text(&self, text: String) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            otlyra_dom::DocumentMutator::new(document).set_cssom_text(id, text);
        })
    }

    /// Record that a custom element definition has run on this element, so
    /// that it matches `:defined`. Not part of the platform: the custom
    /// element glue calls it once the element's constructor has succeeded.
    #[method(name = "__setCustomized")]
    fn js_set_customized(&self) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            otlyra_dom::DocumentMutator::new(document).set_customized(id);
        })
    }

    /// Attach a shadow root (DOM §4.9 `attachShadow`). Not part of the
    /// platform under this name: the glue checks the options and the element.
    #[method(name = "__attachShadow")]
    fn js_attach_shadow(&self, open: bool) -> Result<JsNode, JsError> {
        let (doc, id) = (self.node.doc, self.node.id);
        let mode = if open {
            otlyra_dom::ShadowMode::Open
        } else {
            otlyra_dom::ShadowMode::Closed
        };
        let root = with_document_mut(doc, |document| {
            otlyra_dom::DocumentMutator::new(document).attach_shadow(id, mode)
        })?;
        root.map(|root| JsNode::new(doc, root)).ok_or_else(|| {
            tree::exception(
                "NotSupportedError",
                "this element already has a shadow root",
            )
        })
    }

    /// The shadow root attached here, whatever its mode. Not part of the
    /// platform: `shadowRoot` answers only an open one.
    #[method(name = "__shadowRoot")]
    fn js_shadow_root(&self) -> Result<Option<JsNode>, JsError> {
        let (doc, id) = (self.node.doc, self.node.id);
        with_document(doc, |document| {
            document
                .shadow_root_of(id)
                .map(|(root, _)| JsNode::new(doc, root))
        })
    }

    /// The slot this element is assigned to. Not part of the platform.
    #[method(name = "__assignedSlot")]
    fn js_assigned_slot(&self) -> Result<Option<JsNode>, JsError> {
        let (doc, id) = (self.node.doc, self.node.id);
        with_document(doc, |document| {
            document
                .assigned_slot(id)
                .map(|slot| JsNode::new(doc, slot))
        })
    }

    /// What this slot is assigned. Not part of the platform.
    #[method(name = "__assignedNodes")]
    fn js_assigned_nodes(&self) -> Result<Vec<JsNode>, JsError> {
        let (doc, id) = (self.node.doc, self.node.id);
        with_document(doc, |document| {
            document
                .assigned_nodes(id)
                .into_iter()
                .map(|node| JsNode::new(doc, node))
                .collect()
        })
    }

    #[getter(name = "innerHTML")]
    fn js_inner_html(&self) -> Result<String, JsError> {
        with_document(self.node.doc, |document| {
            markup::serialize(document, self.node.id, false)
        })?
    }

    #[setter(name = "innerHTML")]
    fn js_set_inner_html(&mut self, html: String) -> Result<(), JsError> {
        with_document_mut(self.node.doc, |document| {
            markup::set_inner(document, self.node.id, &html)
        })?
    }

    #[getter(name = "outerHTML")]
    fn js_outer_html(&self) -> Result<String, JsError> {
        with_document(self.node.doc, |document| {
            markup::serialize(document, self.node.id, true)
        })?
    }

    #[setter(name = "outerHTML")]
    fn js_set_outer_html(&mut self, html: String) -> Result<(), JsError> {
        with_document_mut(self.node.doc, |document| {
            markup::set_outer(document, self.node.id, &html)
        })?
    }

    #[method(name = "insertAdjacentHTML", length = 2)]
    fn js_insert_adjacent_html(&self, position: String, html: String) -> Result<(), JsError> {
        with_document_mut(self.node.doc, |document| {
            markup::insert_adjacent(document, self.node.id, &position, &html)
        })?
    }

    #[setter(name = "className")]
    fn js_set_class_name(&mut self, value: String) -> Result<(), JsError> {
        self.set("class", &value)
    }

    /// The address of the picture this element shows, as it spelled it, if
    /// one has arrived or failed: what `currentSrc` resolves. Not part of
    /// the platform.
    #[method(name = "__otlyraImageSource")]
    fn js_image_source(&self) -> Result<Option<String>, JsError> {
        let id = self.node.id;
        with_document(self.node.doc, |document| {
            let src = document.attr(id, "src").unwrap_or_default();
            document
                .image_state(id)
                .filter(|state| selects_its_source(document, id) || state.src == src)
                .map(|state| state.src.clone())
        })
    }

    /// What became of this element's picture for the `src` it has now:
    /// `[width, height, complete]`, with zero sizes for a picture that has
    /// not arrived or failed. Read by the glue's image accessors.
    #[method(name = "__otlyraImageState")]
    fn js_image_state(&self) -> Result<Vec<f64>, JsError> {
        let id = self.node.id;
        with_document(self.node.doc, |document| {
            let src = document.attr(id, "src").unwrap_or_default();
            // With a `srcset`, or a `<picture>` around it, the browser picks
            // the file (HTML §4.8.4.3), and the picture that arrived is for
            // whichever it picked, not for `src`.
            let selects = selects_its_source(document, id);
            if src.trim().is_empty() && !selects {
                return vec![0.0, 0.0, 1.0];
            }
            match document.image_state(id) {
                Some(state) if selects || state.src == src => {
                    let (width, height) = state.size.unwrap_or((0, 0));
                    vec![f64::from(width), f64::from(height), 1.0]
                }
                _ => vec![0.0, 0.0, 0.0],
            }
        })
    }

    /// The element's border box, `[left, top, right, bottom]` in client
    /// coordinates — laid out again first when the tree changed since the
    /// last layout — or null when it has no box.
    #[method(name = "__otlyraClientRect")]
    fn js_client_rect(&self) -> Result<Option<Vec<f64>>, JsError> {
        let id = self.node.id;
        super::with_document_unmarked(self.node.doc, |document| {
            document
                .client_rect(id)
                .map(|rect| rect.iter().map(|&edge| f64::from(edge)).collect())
        })
    }

    /// `property`'s computed value, serialized, from the tree as script left
    /// it — styled again first if it changed since the last styling — or null
    /// when the element has no style: what `getComputedStyle` reads before it
    /// fills in what the cascade cannot say.
    #[method(name = "__otlyraComputedStyle")]
    fn js_computed_style(&self, property: String) -> Result<Option<String>, JsError> {
        let id = self.node.id;
        super::with_document_unmarked(self.node.doc, |document| {
            document.computed_value(id, &property)
        })
    }

    /// A detached `<img>` was given a `src`: ask the browser for its picture.
    #[method(name = "__otlyraRequestImage")]
    fn js_request_image(&self) {
        super::request_image(self.node.id);
    }

    /// `focus()`: the element is focused for script at once, and the browser
    /// moves its own focus (and fires the events) after the turn.
    #[method(name = "__otlyraFocus")]
    fn js_focus(&self) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| document.set_focused(Some(id)))?;
        super::request_focus(Some(id));
        Ok(())
    }

    /// `blur()`: drop the focus if this element has it.
    #[method(name = "__otlyraBlur")]
    fn js_blur(&self) -> Result<(), JsError> {
        let id = self.node.id;
        let had = with_document(self.node.doc, |document| document.focused() == Some(id))?;
        if had {
            with_document_mut(self.node.doc, |document| document.set_focused(None))?;
            super::request_focus(None);
        }
        Ok(())
    }

    /// What a form control holds: what the reader or script put in it, or its
    /// markup's value. Read by the glue's `value` accessors.
    #[method(name = "__otlyraControlValue")]
    fn js_control_value(&self) -> Result<String, JsError> {
        let id = self.node.id;
        with_document(self.node.doc, |document| {
            document.form().value(document, id).to_owned()
        })
    }

    /// Give a form control a value of its own, leaving its markup's alone
    /// (HTML's dirty value flag).
    #[method(name = "__otlyraSetControlValue", length = 1)]
    fn js_set_control_value(&self, value: String) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            document.form_mut().set_value(id, value);
        })
    }

    /// Whether a checkable control is checked.
    #[method(name = "__otlyraControlChecked")]
    fn js_control_checked(&self) -> Result<bool, JsError> {
        let id = self.node.id;
        with_document(self.node.doc, |document| {
            document.form().checkedness(document, id)
        })
    }

    /// Check or uncheck a control; checking a radio button unchecks the rest
    /// of its group.
    #[method(name = "__otlyraSetControlChecked", length = 1)]
    fn js_set_control_checked(&self, checked: bool) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            if checked {
                for other in otlyra_dom::form::radio_group(document, id) {
                    if other != id {
                        document.form_mut().set_checkedness(other, false);
                    }
                }
            }
            document.form_mut().set_checkedness(id, checked);
        })
    }

    #[method(name = "getAttribute", length = 1)]
    fn js_get_attribute(&self, name: String) -> Result<Option<String>, JsError> {
        let name = self.attribute_name(name)?;
        self.get(&name)
    }

    #[method(name = "getAttributeNS", length = 2)]
    fn js_get_attribute_ns(
        &self,
        ns: Option<String>,
        local: String,
    ) -> Result<Option<String>, JsError> {
        let ns = namespace(ns);
        self.read_element(|element| element.attr_in(&ns, &local).map(str::to_owned))
    }

    #[method(name = "hasAttribute", length = 1)]
    fn js_has_attribute(&self, name: String) -> Result<bool, JsError> {
        Ok(self.js_get_attribute(name)?.is_some())
    }

    #[method(name = "hasAttributeNS", length = 2)]
    fn js_has_attribute_ns(&self, ns: Option<String>, local: String) -> Result<bool, JsError> {
        Ok(self.js_get_attribute_ns(ns, local)?.is_some())
    }

    #[method(name = "hasAttributes")]
    fn js_has_attributes(&self) -> Result<bool, JsError> {
        self.read_element(|element| !element.attrs.is_empty())
    }

    #[method(name = "setAttribute", length = 2)]
    fn js_set_attribute(&self, name: String, value: String) -> Result<(), JsError> {
        if !is_valid_attribute_name(&name) {
            return Err(tree::exception(
                "InvalidCharacterError",
                format!("{name:?} is not a valid attribute name"),
            ));
        }
        let name = self.attribute_name(name)?;
        self.set(&name, &value)
    }

    /// DOM §4.9 `setAttributeNS`: the qualified name is split at its colon,
    /// and the prefix must agree with the namespace it is given.
    #[method(name = "setAttributeNS", length = 3)]
    fn js_set_attribute_ns(
        &self,
        ns: Option<String>,
        qualified_name: String,
        value: String,
    ) -> Result<(), JsError> {
        let name = validate_and_extract(namespace(ns), &qualified_name)?;
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            DocumentMutator::new(document).set_attr_ns(id, name, &value);
        })
    }

    #[method(name = "removeAttribute", length = 1)]
    fn js_remove_attribute(&self, name: String) -> Result<(), JsError> {
        let name = self.attribute_name(name)?;
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            DocumentMutator::new(document).remove_attr(id, &name);
        })
    }

    #[method(name = "removeAttributeNS", length = 2)]
    fn js_remove_attribute_ns(&self, ns: Option<String>, local: String) -> Result<(), JsError> {
        let ns = namespace(ns);
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            DocumentMutator::new(document).remove_attr_ns(id, &ns, &local);
        })
    }

    /// Add the attribute if it is absent, remove it if present — or, with
    /// `force`, make it so. Returns whether it is there afterwards.
    #[method(name = "toggleAttribute", length = 1)]
    fn js_toggle_attribute(&self, name: String, force: Option<bool>) -> Result<bool, JsError> {
        if !is_valid_attribute_name(&name) {
            return Err(tree::exception(
                "InvalidCharacterError",
                format!("{name:?} is not a valid attribute name"),
            ));
        }
        let name = self.attribute_name(name)?;
        let present = self.get(&name)?.is_some();
        let wanted = force.unwrap_or(!present);
        if wanted && !present {
            self.set(&name, "")?;
        } else if !wanted && present {
            let id = self.node.id;
            with_document_mut(self.node.doc, |document| {
                DocumentMutator::new(document).remove_attr(id, &name);
            })?;
        }
        Ok(wanted)
    }

    #[method(name = "getAttributeNames")]
    fn js_get_attribute_names(&self) -> Result<Vec<String>, JsError> {
        self.read_element(|element| element.attrs.iter().map(qualified).collect())
    }

    /// Every attribute, for the glue's `attributes`. Not part of the platform:
    /// the glue takes it and removes it.
    #[method(name = "__attributeRecords")]
    fn js_attribute_records(&self) -> Result<Vec<AttrRecord>, JsError> {
        self.read_element(|element| {
            element
                .attrs
                .iter()
                .map(|attr| AttrRecord {
                    name: qualified(attr),
                    value: attr.value.to_string(),
                    namespace_uri: (attr.name.ns != ns!()).then(|| attr.name.ns.to_string()),
                    prefix: attr.name.prefix.as_ref().map(ToString::to_string),
                    local_name: attr.name.local.to_string(),
                })
                .collect()
        })
    }

    /// The element children, as a list the glue makes an `HTMLCollection` of.
    #[getter(name = "children")]
    fn js_children(&self) -> Result<Vec<JsNode>, JsError> {
        children_of(self.node)
    }

    #[getter(name = "childElementCount")]
    fn js_child_element_count(&self) -> Result<u32, JsError> {
        child_element_count(self.node)
    }

    #[getter(name = "firstElementChild")]
    fn js_first_element_child(&self) -> Result<Option<JsNode>, JsError> {
        first_element_child(self.node)
    }

    #[getter(name = "lastElementChild")]
    fn js_last_element_child(&self) -> Result<Option<JsNode>, JsError> {
        last_element_child(self.node)
    }

    #[getter(name = "previousElementSibling")]
    fn js_previous_element_sibling(&self) -> Result<Option<JsNode>, JsError> {
        let NodeRef { doc, id } = self.node;
        with_document(doc, |document| {
            document
                .prev_element_sibling(id)
                .map(|found| JsNode::new(doc, found))
        })
    }

    #[getter(name = "nextElementSibling")]
    fn js_next_element_sibling(&self) -> Result<Option<JsNode>, JsError> {
        let NodeRef { doc, id } = self.node;
        with_document(doc, |document| {
            document
                .next_element_sibling(id)
                .map(|found| JsNode::new(doc, found))
        })
    }

    #[method(name = "querySelector", length = 1)]
    fn js_query_selector(&self, selector: String) -> Result<Option<JsNode>, JsError> {
        Ok(query(self.node, &selector, true)?.into_iter().next())
    }

    #[method(name = "querySelectorAll", length = 1)]
    fn js_query_selector_all(&self, selector: String) -> Result<Vec<JsNode>, JsError> {
        query(self.node, &selector, false)
    }

    #[method(name = "matches", length = 1)]
    fn js_matches(&self, selector: String) -> Result<bool, JsError> {
        let NodeRef { doc, id } = self.node;
        with_document(doc, |document| {
            otlyra_css::stylo_dom::matches_selector(document, id, &selector)
        })?
        .map_err(selector_error)
    }

    #[method(name = "closest", length = 1)]
    fn js_closest(&self, selector: String) -> Result<Option<JsNode>, JsError> {
        let NodeRef { doc, id } = self.node;
        let found = with_document(doc, |document| {
            otlyra_css::stylo_dom::closest(document, id, &selector)
        })?
        .map_err(selector_error)?;
        Ok(found.map(|found| JsNode::new(doc, found)))
    }

    #[method(name = "getElementsByTagName", length = 1)]
    fn js_get_elements_by_tag_name(&self, name: String) -> Result<Vec<JsNode>, JsError> {
        by_tag_name(self.node, &name)
    }

    #[method(name = "getElementsByTagNameNS", length = 2)]
    fn js_get_elements_by_tag_name_ns(
        &self,
        ns: Option<String>,
        local: String,
    ) -> Result<Vec<JsNode>, JsError> {
        by_tag_name_ns(self.node, ns, &local)
    }

    #[method(name = "getElementsByClassName", length = 1)]
    fn js_get_elements_by_class_name(&self, names: String) -> Result<Vec<JsNode>, JsError> {
        by_class_name(self.node, &names)
    }

    #[method(name = "remove")]
    fn js_remove(&self) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            DocumentMutator::new(document).detach(id);
        })
    }

    /// A `<template>`'s contents. Not on `Element` in the platform: the glue
    /// moves it to `HTMLTemplateElement`.
    #[getter(name = "content")]
    fn js_content(&self) -> Result<Option<JsNode>, JsError> {
        let doc = self.node.doc;
        self.read_element(|element| {
            (element.name.ns == ns!(html))
                .then_some(element.template_contents)
                .flatten()
                .map(|contents| JsNode::new(doc, contents))
        })
    }

    /// `form.submit()`, which is how a redirector page moves the reader on.
    /// The glue moves it to `HTMLFormElement`.
    ///
    /// Recorded rather than performed: the isolate is holding the document
    /// this form is in, and navigating from here would pull it out from under
    /// the turn. The browser reads the request when the turn ends.
    #[method(name = "submit")]
    fn js_submit(&self) -> Result<(), JsError> {
        let is_form = self.read_element(|element| {
            element.name.ns == ns!(html) && &*element.name.local == "form"
        })?;
        if !is_form {
            return Err(JsError::Type("submit() is a form's method".to_owned()));
        }
        super::request_navigation(Navigation::Submit { form: self.node.id });
        Ok(())
    }
}

impl ElementRef {
    /// Read something off the element's data.
    fn read_element<R>(&self, read: impl FnOnce(&ElementData) -> R) -> Result<R, JsError> {
        let NodeRef { doc, id } = self.node;
        with_document(doc, |document| {
            document.get(id).and_then(|node| node.element()).map(read)
        })?
        .ok_or_else(tree::gone)
    }

    /// The name an attribute is looked up by: lowercased on an HTML element,
    /// as written on any other (DOM §4.9 "get an attribute by name", step 1).
    fn attribute_name(&self, name: String) -> Result<String, JsError> {
        let html = self.read_element(|element| element.name.ns == ns!(html))?;
        Ok(if html {
            name.to_ascii_lowercase()
        } else {
            name
        })
    }

    /// The first attribute with this qualified name.
    fn get(&self, name: &str) -> Result<Option<String>, JsError> {
        self.read_element(|element| {
            element
                .attrs
                .iter()
                .find(|attr| qualified(attr) == name)
                .map(|attr| attr.value.to_string())
        })
    }

    fn set(&self, name: &str, value: &str) -> Result<(), JsError> {
        let id = self.node.id;
        with_document_mut(self.node.doc, |document| {
            DocumentMutator::new(document).set_attr(id, name, value);
        })
    }
}

/// DOM §1.4 "validate and extract": split a qualified name, and refuse the
/// combinations of prefix and namespace XML reserves.
fn validate_and_extract(ns: Namespace, qualified_name: &str) -> Result<QualName, JsError> {
    let (prefix, local) = match qualified_name.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local),
        None => (None, qualified_name),
    };
    if !is_valid_attribute_name(local)
        || prefix.is_some_and(|prefix| !is_valid_attribute_name(prefix))
    {
        return Err(tree::exception(
            "InvalidCharacterError",
            format!("{qualified_name:?} is not a valid qualified name"),
        ));
    }
    let namespace_error = || {
        Err(tree::exception(
            "NamespaceError",
            format!("{qualified_name:?} cannot be in this namespace"),
        ))
    };
    let is_xmlns = qualified_name == "xmlns" || prefix == Some("xmlns");
    if prefix.is_some() && ns == ns!() {
        return namespace_error();
    }
    if prefix == Some("xml") && ns != ns!(xml) {
        return namespace_error();
    }
    if is_xmlns != (ns == ns!(xmlns)) {
        return namespace_error();
    }
    Ok(QualName::new(
        prefix.map(Prefix::from),
        ns,
        LocalName::from(local),
    ))
}

/// What a selector that does not parse throws (DOM §4.2.6 step 2).
fn selector_error(message: String) -> JsError {
    tree::exception("SyntaxError", message)
}

/// `querySelector`/`querySelectorAll` under `scope`.
pub(crate) fn query(scope: NodeRef, selector: &str, first: bool) -> Result<Vec<JsNode>, JsError> {
    let NodeRef { doc, id } = scope;
    let matched = with_document(doc, |document| {
        otlyra_css::stylo_dom::select_within(document, id, selector, first)
    })?
    .map_err(selector_error)?;
    Ok(matched
        .into_iter()
        .map(|found| JsNode::new(doc, found))
        .collect())
}

/// The element children of `parent`.
pub(crate) fn children_of(parent: NodeRef) -> Result<Vec<JsNode>, JsError> {
    with_document(parent.doc, |document| {
        tree::element_children(document, parent.id)
            .into_iter()
            .map(|child| JsNode::new(parent.doc, child))
            .collect()
    })
}

/// How many element children `parent` has.
pub(crate) fn child_element_count(parent: NodeRef) -> Result<u32, JsError> {
    with_document(parent.doc, |document| {
        u32::try_from(tree::element_children(document, parent.id).len()).unwrap_or(u32::MAX)
    })
}

/// The first element child of `parent`.
pub(crate) fn first_element_child(parent: NodeRef) -> Result<Option<JsNode>, JsError> {
    with_document(parent.doc, |document| {
        document
            .first_element_child(parent.id)
            .map(|found| JsNode::new(parent.doc, found))
    })
}

/// The last element child of `parent`.
pub(crate) fn last_element_child(parent: NodeRef) -> Result<Option<JsNode>, JsError> {
    with_document(parent.doc, |document| {
        tree::last_element_child(document, parent.id).map(|found| JsNode::new(parent.doc, found))
    })
}

/// The elements under `root`, `root` excluded, that `keep` accepts.
fn elements_under(
    document: &Document,
    root: NodeId,
    mut keep: impl FnMut(&ElementData) -> bool,
) -> Vec<NodeId> {
    tree::descendants(document, root)
        .into_iter()
        .skip(1)
        .filter(|id| {
            document
                .get(*id)
                .and_then(|node| node.element())
                .is_some_and(&mut keep)
        })
        .collect()
}

/// DOM §4.2.6 "list of elements with qualified name": everything for `*`, and
/// otherwise the qualified name — lowercased for HTML elements, which is what
/// lets `getElementsByTagName("DIV")` find `<div>`s and leaves `foreignObject`
/// findable.
pub(crate) fn by_tag_name(root: NodeRef, name: &str) -> Result<Vec<JsNode>, JsError> {
    let lowered = name.to_ascii_lowercase();
    with_document(root.doc, |document| {
        elements_under(document, root.id, |element| {
            if name == "*" {
                return true;
            }
            let qualified = match &element.name.prefix {
                Some(prefix) => format!("{prefix}:{}", element.name.local),
                None => element.name.local.to_string(),
            };
            if element.name.ns == ns!(html) {
                qualified == lowered
            } else {
                qualified == name
            }
        })
        .into_iter()
        .map(|id| JsNode::new(root.doc, id))
        .collect()
    })
}

/// DOM §4.2.6 "list of elements with namespace and local name", `*` matching
/// any of either.
pub(crate) fn by_tag_name_ns(
    root: NodeRef,
    ns: Option<String>,
    local: &str,
) -> Result<Vec<JsNode>, JsError> {
    let any_ns = ns.as_deref() == Some("*");
    let ns = namespace(ns);
    with_document(root.doc, |document| {
        elements_under(document, root.id, |element| {
            (any_ns || element.name.ns == ns) && (local == "*" || &*element.name.local == local)
        })
        .into_iter()
        .map(|id| JsNode::new(root.doc, id))
        .collect()
    })
}

/// DOM §4.2.6 "list of elements with class names": every class in `names`,
/// compared exactly — or ASCII case-insensitively in quirks mode.
pub(crate) fn by_class_name(root: NodeRef, names: &str) -> Result<Vec<JsNode>, JsError> {
    let wanted: Vec<&str> = names.split_ascii_whitespace().collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    with_document(root.doc, |document| {
        let exact = document.quirks_mode() != html5ever::interface::QuirksMode::Quirks;
        elements_under(document, root.id, |element| {
            wanted.iter().all(|class| element.has_class(class, exact))
        })
        .into_iter()
        .map(|id| JsNode::new(root.doc, id))
        .collect()
    })
}

/// The HTML elements under `root` whose `name` attribute is `name`.
pub(crate) fn by_name(root: NodeRef, name: &str) -> Result<Vec<JsNode>, JsError> {
    with_document(root.doc, |document| {
        elements_under(document, root.id, |element| {
            element.name.ns == ns!(html) && element.attr("name") == Some(name)
        })
        .into_iter()
        .map(|id| JsNode::new(root.doc, id))
        .collect()
    })
}

/// Whether the browser picks `img`'s file from candidates rather than taking
/// its `src`: it has a `srcset`, or its parent is a `<picture>`.
fn selects_its_source(document: &otlyra_dom::Document, img: otlyra_dom::NodeId) -> bool {
    document.attr(img, "srcset").is_some()
        || document
            .get(img)
            .and_then(|node| node.parent)
            .and_then(|parent| document.get(parent))
            .and_then(otlyra_dom::Node::element)
            .is_some_and(|parent| &*parent.name.local == "picture")
}
