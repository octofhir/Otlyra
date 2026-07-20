//! Printing a tree in the html5lib-tests format.
//!
//! This is not a debug helper that happens to look like the test format; it *is*
//! the test format, byte for byte, because that is what makes the html5lib
//! tree-construction expectations usable as our expectations. It is also what
//! `--dump-dom` prints, so what we assert on and what a person reads are the same
//! text.

use std::fmt::Write as _;

use html5ever::{QualName, ns};

use crate::node::{NodeData, NodeId};
use crate::tree::Document;

/// Serialize a fragment: the children of the single element the parser built the
/// fragment inside, rather than that element itself.
///
/// The conformance suite states a fragment's expectation as the nodes it produced,
/// and the container is scaffolding — real `innerHTML` hands back those nodes too.
pub fn serialize_fragment(document: &Document) -> String {
    let mut out = String::new();
    let container = document
        .children(document.root())
        .next()
        .unwrap_or_else(|| document.root());
    for child in document.children(container) {
        serialize_node(document, child, 0, &mut out);
    }
    out
}

/// Serialize a whole document.
pub fn serialize(document: &Document) -> String {
    let mut out = String::new();
    for child in document.children(document.root()) {
        serialize_node(document, child, 0, &mut out);
    }
    out
}

/// Serialize one node and everything under it, at `depth` levels of indentation.
pub fn serialize_node(document: &Document, id: NodeId, depth: usize, out: &mut String) {
    let Some(node) = document.get(id) else { return };
    let indent = "  ".repeat(depth);

    match &node.data {
        NodeData::Document => {}
        NodeData::Doctype {
            name,
            public_id,
            system_id,
        } => {
            let _ = write!(out, "| {indent}<!DOCTYPE {name}");
            if !public_id.is_empty() || !system_id.is_empty() {
                let _ = write!(out, " \"{public_id}\" \"{system_id}\"");
            }
            out.push_str(">\n");
        }
        NodeData::Text(text) => {
            let _ = writeln!(out, "| {indent}\"{text}\"");
        }
        NodeData::Comment(text) => {
            let _ = writeln!(out, "| {indent}<!-- {text} -->");
        }
        NodeData::Element(element) => {
            let _ = writeln!(out, "| {indent}<{}>", element_name(&element.name));

            // Attributes are sorted, and the expectations are written that way,
            // because source order carries no meaning once they are on the element.
            let mut attrs: Vec<_> = element
                .attrs
                .iter()
                .map(|attr| (attribute_name(&attr.name), attr.value.to_string()))
                .collect();
            attrs.sort();
            for (name, value) in attrs {
                let _ = writeln!(out, "| {indent}  {name}=\"{value}\"");
            }

            if let Some(contents) = element.template_contents {
                let _ = writeln!(out, "| {indent}  content");
                for child in document.children(contents) {
                    serialize_node(document, child, depth + 2, out);
                }
            }
        }
    }

    for child in document.children(id) {
        serialize_node(document, child, depth + 1, out);
    }
}

/// An element's name, with the namespace spelled out for foreign content.
fn element_name(name: &QualName) -> String {
    match name.ns {
        ns!(svg) => format!("svg {}", name.local),
        ns!(mathml) => format!("math {}", name.local),
        _ => name.local.to_string(),
    }
}

/// An attribute's name, with its prefix when it has one.
fn attribute_name(name: &QualName) -> String {
    match &name.prefix {
        Some(prefix) => format!("{prefix} {}", name.local),
        None => name.local.to_string(),
    }
}
