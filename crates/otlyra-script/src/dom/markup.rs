//! HTML's dynamic markup APIs, using the same fragment parser as page loading.
//!
//! A parsed fragment has its own arena. Importing its nodes into the page keeps
//! all existing node identities valid and gives new nodes the page's document
//! identity. Both import and serialization walk iteratively, as page nesting
//! must not become Rust stack depth.

use std::io;

use html5ever::serialize::{Serialize, SerializeOpts, Serializer, TraversalScope};
use html5ever::{LocalName, QualName, ns};
use otlyra_dom::{Document, DocumentMutator, NodeData, NodeId};
use otter_runtime::marshal::JsError;

use super::tree;

fn contents(document: &Document, id: NodeId) -> NodeId {
    document
        .get(id)
        .and_then(|node| node.element())
        .and_then(|element| element.template_contents)
        .unwrap_or(id)
}

fn context(document: &Document, id: NodeId) -> (QualName, Vec<html5ever::Attribute>) {
    document
        .get(id)
        .and_then(|node| node.element())
        .map(|element| (element.name.clone(), element.attrs.clone()))
        .unwrap_or_else(|| {
            (
                QualName::new(None, ns!(html), LocalName::from("body")),
                Vec::new(),
            )
        })
}

fn parse(document: &Document, id: NodeId, html: &str) -> Document {
    otlyra_html::parse_fragment_for_element(html, document, id)
}

pub(super) fn set_inner(document: &mut Document, id: NodeId, html: &str) -> Result<(), JsError> {
    let parsed = parse(document, id, html);
    let target = contents(document, id);
    DocumentMutator::new(document).remove_children(target);
    insert(document, target, None, &parsed)
}

pub(super) fn set_outer(document: &mut Document, id: NodeId, html: &str) -> Result<(), JsError> {
    let Some(parent) = tree::parent(document, id) else {
        return Ok(());
    };
    if parent == document.root() {
        return Err(tree::exception(
            "NoModificationAllowedError",
            "the document element cannot be replaced",
        ));
    }
    let parsed = parse(document, parent, html);
    insert(document, parent, Some(id), &parsed)?;
    DocumentMutator::new(document).detach(id);
    Ok(())
}

pub(super) fn insert_adjacent(
    document: &mut Document,
    id: NodeId,
    position: &str,
    html: &str,
) -> Result<(), JsError> {
    let position = position.to_ascii_lowercase();
    let (parent, before) = match position.as_str() {
        "beforebegin" | "afterend" => {
            let parent = tree::parent(document, id)
                .filter(|parent| *parent != document.root())
                .ok_or_else(|| {
                    tree::exception(
                        "NoModificationAllowedError",
                        "the element has no mutable parent",
                    )
                })?;
            let before = if position == "beforebegin" {
                Some(id)
            } else {
                document.get(id).and_then(|node| node.next_sibling())
            };
            (parent, before)
        }
        "afterbegin" => (id, document.get(id).and_then(|node| node.first_child())),
        "beforeend" => (id, None),
        _ => {
            return Err(tree::exception(
                "SyntaxError",
                "invalid HTML insertion position",
            ));
        }
    };
    let (mut name, mut attributes) = context(document, parent);
    if name.ns == ns!(html) && &*name.local == "html" {
        name.local = LocalName::from("body");
        attributes.clear();
    }
    let parsed = if name.ns == ns!(html)
        && &*name.local == "body"
        && document
            .get(parent)
            .and_then(|node| node.element())
            .is_some_and(|element| &*element.name.local == "html")
    {
        otlyra_html::parse_fragment_with_context(html, name, attributes)
    } else {
        parse(document, parent, html)
    };
    // Unlike innerHTML, insertAdjacentHTML inserts into a template's ordinary
    // children, rather than its separate content fragment (HTML §8.5.6).
    insert(document, parent, before, &parsed)
}

fn import(
    mutator: &mut DocumentMutator<'_>,
    source: &Document,
    id: NodeId,
    parent: NodeId,
    before: Option<NodeId>,
) -> Result<(), JsError> {
    let make = |mutator: &mut DocumentMutator<'_>, id: NodeId| {
        let mut data = source.get(id).expect("a parsed node").data.clone();
        if let NodeData::Element(element) = &mut data {
            let template = element
                .template_contents
                .map(|_| mutator.create(NodeData::Document));
            let created = mutator.create_element(
                element.name.clone(),
                element.attrs.clone(),
                template,
                element.mathml_annotation_xml_integration_point,
            );
            // A script markup makes is already started (HTML §8.5 fragment
            // parsing): `innerHTML` does not run what it writes.
            if element.name.ns == ns!(html) && &*element.name.local == "script" {
                mutator.mark_script_started(created);
            }
            return created;
        }
        mutator.create(data)
    };
    let root = make(mutator, id);
    let inserted = match before {
        Some(before) => mutator.insert_before(before, root),
        None => mutator.append(parent, root),
    };
    if !inserted {
        return Err(nesting_limit());
    }
    // Attach each parent before copying its children, so the mutator checks the
    // depth in the destination page, rather than a detached subtree's depth.
    let mut pending = vec![(id, root)];
    while let Some((from, to)) = pending.pop() {
        let mut children: Vec<_> = source.children(from).map(|child| (child, to)).collect();
        let source_contents = contents(source, from);
        if source_contents != from {
            let target_contents = contents(mutator.document(), to);
            children.extend(
                source
                    .children(source_contents)
                    .map(|child| (child, target_contents)),
            );
        }
        for (child, parent) in children {
            let copy = make(mutator, child);
            if mutator.append(parent, copy) {
                pending.push((child, copy));
            } else {
                return Err(nesting_limit());
            }
        }
    }
    Ok(())
}

fn nesting_limit() -> JsError {
    tree::exception(
        "HierarchyRequestError",
        "the document's nesting limit was reached",
    )
}

fn insert(
    document: &mut Document,
    parent: NodeId,
    before: Option<NodeId>,
    parsed: &Document,
) -> Result<(), JsError> {
    // html5ever's fragment tree has a synthetic html element under the root.
    let root = parsed
        .children(parsed.root())
        .next()
        .unwrap_or(parsed.root());
    let mut mutator = DocumentMutator::new(document);
    for child in parsed.children(root) {
        import(&mut mutator, parsed, child, parent, before)?;
    }
    Ok(())
}

struct Html<'a> {
    document: &'a Document,
    root: NodeId,
}

impl Serialize for Html<'_> {
    fn serialize<S: Serializer>(
        &self,
        serializer: &mut S,
        scope: TraversalScope,
    ) -> io::Result<()> {
        let mut pending = Vec::new();
        match scope {
            TraversalScope::IncludeNode => pending.push((self.root, false)),
            TraversalScope::ChildrenOnly(_) => {
                pending.extend(
                    self.document
                        .children(contents(self.document, self.root))
                        .map(|id| (id, false)),
                );
                pending.reverse();
            }
        }
        while let Some((id, closing)) = pending.pop() {
            let node = self.document.get(id).expect("a serialized node");
            match &node.data {
                NodeData::Element(element) if closing => {
                    serializer.end_elem(element.name.clone())?
                }
                NodeData::Element(element) => {
                    serializer.start_elem(
                        element.name.clone(),
                        element.attrs.iter().map(|attr| (&attr.name, &*attr.value)),
                    )?;
                    pending.push((id, true));
                    if !is_void(&element.name) {
                        let start = pending.len();
                        pending.extend(
                            self.document
                                .children(contents(self.document, id))
                                .map(|child| (child, false)),
                        );
                        pending[start..].reverse();
                    }
                }
                NodeData::Text(text) => serializer.write_text(text)?,
                NodeData::Comment(text) => serializer.write_comment(text)?,
                NodeData::Doctype { name, .. } => serializer.write_doctype(name)?,
                NodeData::Document => {
                    let start = pending.len();
                    pending.extend(self.document.children(id).map(|child| (child, false)));
                    pending[start..].reverse();
                }
            }
        }
        Ok(())
    }
}

fn is_void(name: &QualName) -> bool {
    name.ns == ns!(html)
        && matches!(
            &*name.local,
            "area"
                | "base"
                | "basefont"
                | "bgsound"
                | "br"
                | "col"
                | "embed"
                | "frame"
                | "hr"
                | "img"
                | "input"
                | "keygen"
                | "link"
                | "meta"
                | "param"
                | "source"
                | "track"
                | "wbr"
        )
}

pub(super) fn serialize(document: &Document, root: NodeId, outer: bool) -> Result<String, JsError> {
    let (name, _) = context(document, root);
    let scope = if outer {
        TraversalScope::IncludeNode
    } else {
        TraversalScope::ChildrenOnly((name.ns == ns!(html)).then_some(name))
    };
    let mut bytes = Vec::new();
    html5ever::serialize::serialize(
        &mut bytes,
        &Html { document, root },
        SerializeOpts {
            traversal_scope: scope,
            ..SerializeOpts::default()
        },
    )
    .map_err(|error| JsError::Type(error.to_string()))?;
    Ok(String::from_utf8(bytes).expect("HTML serialization writes UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importing_fragment_nodes_respects_the_destination_depth_limit() {
        let mut document = Document::with_limits(otlyra_dom::DomLimits {
            max_depth: 4,
            ..otlyra_dom::DomLimits::default()
        });
        let root = document.root();
        let mut mutator = DocumentMutator::new(&mut document);
        let outer = mutator.create_element(
            QualName::new(None, ns!(html), LocalName::from("div")),
            Vec::new(),
            None,
            false,
        );
        assert!(mutator.append(root, outer));
        let target = mutator.create_element(
            QualName::new(None, ns!(html), LocalName::from("div")),
            Vec::new(),
            None,
            false,
        );
        assert!(mutator.append(outer, target));
        assert!(set_inner(&mut document, target, "<p><b>too deep</b></p>").is_err());
        for node in tree::descendants(&document, root) {
            assert!(document.depth(node) <= document.limits().max_depth);
        }
        assert!(document.refused_insertions() > 0);
    }
}
