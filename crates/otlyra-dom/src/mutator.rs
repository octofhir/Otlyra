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
        document.note_mutation();
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

    /// Attach a shadow root to `host` (DOM §4.2.14 "attach a shadow root"):
    /// a fragment the document keeps beside it, which the flat tree renders
    /// in place of its children. `None` when it already has one.
    pub fn attach_shadow(&mut self, host: NodeId, mode: crate::ShadowMode) -> Option<NodeId> {
        if self.document.shadow_root_of(host).is_some() {
            return None;
        }
        let root = self.create(NodeData::Document);
        self.document.shadow.attach(host, root, mode);
        Some(root)
    }

    /// Say which constructed style sheets `root` adopts, by their text.
    pub fn set_adopted_styles(&mut self, root: NodeId, texts: Vec<String>) {
        if texts.is_empty() {
            self.document.shadow.adopted.remove(&root);
        } else {
            self.document.shadow.adopted.insert(root, texts);
        }
    }

    /// Record that a custom element definition has run on `element`: it is
    /// `:defined` from now on.
    pub fn set_customized(&mut self, element: NodeId) {
        self.document.customized.insert(element);
    }

    /// Point a `<template>` at `contents` as what it holds: a declarative
    /// shadow root, which the parser then fills as it would a template's
    /// own contents.
    pub fn set_template_contents(&mut self, template: NodeId, contents: NodeId) {
        if let Some(NodeData::Element(element)) = self
            .document
            .nodes_mut()
            .get_mut(template)
            .map(|node| &mut node.data)
        {
            element.template_contents = Some(contents);
        }
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

    /// Copy a subtree, returning the copy's root. The copy is detached.
    ///
    /// The DOM's `cloneNode(true)`, and the one place a node is duplicated rather
    /// than moved. Iterative rather than recursive: a document is untrusted input,
    /// and a clone that recurses inherits its nesting as stack depth.
    ///
    /// A `<template>`'s contents are copied with it, into contents of the copy's
    /// own (DOM §4.5 "clone a node", HTML §4.12.3's cloning steps): two templates
    /// sharing one fragment would each see the other's edits.
    pub fn deep_clone(&mut self, source: NodeId) -> Option<NodeId> {
        let root = self.shallow_clone(source)?;

        let mut pending = vec![(source, root)];
        while let Some((from, to)) = pending.pop() {
            // The template's contents travel beside its children, not under them.
            let contents = self.template_contents(from).zip(self.template_contents(to));
            let mut sources: Vec<(NodeId, NodeId)> = self
                .document
                .children(from)
                .map(|child| (child, to))
                .collect();
            if let Some((from_contents, to_contents)) = contents {
                sources.extend(
                    self.document
                        .children(from_contents)
                        .map(|child| (child, to_contents)),
                );
            }
            for (child, parent) in sources {
                let Some(copy) = self.shallow_clone(child) else {
                    continue;
                };
                if self.append(parent, copy) {
                    pending.push((child, copy));
                }
            }
        }
        Some(root)
    }

    /// Copy one node and none of its children. The copy is detached.
    ///
    /// The DOM's `cloneNode(false)`. An element keeps its name and attributes; a
    /// `<template>` gets a new, empty contents fragment of its own rather than
    /// the original's, which [`Self::deep_clone`] then fills.
    pub fn shallow_clone(&mut self, source: NodeId) -> Option<NodeId> {
        let mut data = self.document.get(source)?.data.clone();
        if let NodeData::Element(element) = &mut data
            && element.template_contents.is_some()
        {
            element.template_contents = Some(self.create(NodeData::Document));
        }
        Some(self.create(data))
    }

    /// A `<template>`'s contents fragment, if `id` is a template.
    fn template_contents(&self, id: NodeId) -> Option<NodeId> {
        self.document.get(id)?.element()?.template_contents
    }

    /// Replace the text a text or comment node holds.
    ///
    /// Returns whether `target` was character data. Its place in the tree is
    /// untouched, which is what makes this the DOM's `data` setter rather than a
    /// remove and an insert.
    pub fn set_character_data(&mut self, target: NodeId, text: StrTendril) -> bool {
        match self
            .document
            .nodes_mut()
            .get_mut(target)
            .map(|node| &mut node.data)
        {
            Some(NodeData::Text(data) | NodeData::Comment(data)) => {
                *data = text;
                true
            }
            _ => false,
        }
    }

    /// Remove every child of `parent` from the tree.
    pub fn remove_children(&mut self, parent: NodeId) {
        while let Some(child) = self.document.node(parent).first_child() {
            self.detach(child);
        }
    }

    /// Set one attribute, replacing whatever was there.
    ///
    /// The parser never needs this — it only ever adds what is missing, which is
    /// what the tree construction algorithm says — but everything that edits a
    /// document afterwards does. Returns whether it landed on an element.
    ///
    /// `name` is a *qualified* name, as `setAttribute` takes one (DOM §4.9): the
    /// first attribute spelled that way — `xlink:href` included — is the one
    /// changed, and a new one is created in no namespace.
    ///
    /// An empty value is still a value: `hidden=""` is set, not absent, and the
    /// two mean different things to a selector.
    pub fn set_attr(&mut self, target: NodeId, name: &str, value: &str) -> bool {
        let qualified = |attr: &Attribute| qualified_name_is(&attr.name, name);
        self.put_attr(
            target,
            qualified,
            || html5ever::QualName::new(None, html5ever::ns!(), html5ever::LocalName::from(name)),
            value,
        )
    }

    /// Set one attribute by namespace and local name, as `setAttributeNS` does.
    ///
    /// The attribute that is replaced is the one with the same namespace and
    /// local name, whatever its prefix (DOM §4.9 "set an attribute value"); a new
    /// one keeps the prefix it was given, which is what it serializes with.
    pub fn set_attr_ns(&mut self, target: NodeId, name: QualName, value: &str) -> bool {
        let (ns, local) = (name.ns.clone(), name.local.clone());
        self.put_attr(
            target,
            |attr: &Attribute| attr.name.ns == ns && attr.name.local == local,
            || name,
            value,
        )
    }

    /// The shared half of the two setters: find by `matches`, else append.
    fn put_attr(
        &mut self,
        target: NodeId,
        matches: impl Fn(&Attribute) -> bool,
        name: impl FnOnce() -> QualName,
        value: &str,
    ) -> bool {
        let limit = self.document.limits().max_attrs_per_element;
        let mut refused = false;
        let set = match self
            .document
            .nodes_mut()
            .get_mut(target)
            .map(|node| &mut node.data)
        {
            Some(NodeData::Element(element)) => match element.attrs.iter().position(&matches) {
                Some(at) => {
                    element.attrs[at].value = value.into();
                    true
                }
                None if element.attrs.len() >= limit => {
                    refused = true;
                    false
                }
                None => {
                    element.attrs.push(Attribute {
                        name: name(),
                        value: value.into(),
                    });
                    true
                }
            },
            _ => false,
        };
        if refused {
            self.document.refuse();
        }
        set
    }

    /// Remove one attribute, by qualified name. Returns whether it was there to
    /// remove.
    pub fn remove_attr(&mut self, target: NodeId, name: &str) -> bool {
        self.drop_attr(target, |attr| qualified_name_is(&attr.name, name))
    }

    /// Remove one attribute by namespace and local name. Returns whether it was
    /// there to remove.
    pub fn remove_attr_ns(
        &mut self,
        target: NodeId,
        ns: &html5ever::Namespace,
        local: &str,
    ) -> bool {
        self.drop_attr(target, |attr| {
            attr.name.ns == *ns && &*attr.name.local == local
        })
    }

    /// Remove the first attribute `matches` picks.
    fn drop_attr(&mut self, target: NodeId, matches: impl Fn(&Attribute) -> bool) -> bool {
        if let Some(NodeData::Element(element)) = self
            .document
            .nodes_mut()
            .get_mut(target)
            .map(|node| &mut node.data)
            && let Some(at) = element.attrs.iter().position(matches)
        {
            element.attrs.remove(at);
            return true;
        }
        false
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

/// Whether an attribute's qualified name — `prefix:local`, or `local` with no
/// prefix — is `name`.
fn qualified_name_is(attr: &QualName, name: &str) -> bool {
    match &attr.prefix {
        Some(prefix) => name
            .strip_prefix(&**prefix)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|local| local == &*attr.local),
        None => name == &*attr.local,
    }
}
