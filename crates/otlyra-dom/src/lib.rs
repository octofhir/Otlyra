//! # otlyra-dom — the document tree
//!
//! ## Purpose
//!
//! One arena of nodes, the handles that address it, and the only API that changes
//! it. Everything downstream — style, layout, paint, script — reads this tree; the
//! parser writes it through [`DocumentMutator`], and nothing else may.
//!
//! ## Contents
//!
//! - [`limits`] — what a hostile document is not allowed to cost.
//! - [`form`] — what a form control is, and what the reader has made it hold.
//! - [`submit`] — the pairs a form sends, and how they are spelled.
//! - [`node`] — [`NodeId`], [`Node`], [`NodeData`].
//! - [`tree`] — [`Document`]: the arena and its read API.
//! - [`mutator`] — [`DocumentMutator`]: the write API.
//! - [`sink`] — the html5ever adapter, which is a translation layer and no more.
//! - [`dump`] — the html5lib-tests text form, used by both the tests and `--dump-dom`.
//!
//! ## Invariants
//!
//! 1. **Handles are generational.** A handle to a removed node resolves to `None`,
//!    never to whichever node took its slot.
//! 2. **Children are a linked list, not a `Vec`.** Insert-before, detach and
//!    reparent are the tree builder's hot operations and all three are O(1).
//! 3. **No adjacent text nodes.** Text appended next to text is merged, because a
//!    DOM that produced two of them would disagree with every conformance test and
//!    with script.
//! 4. **Depth is bounded.** A document nested past the limit stops growing rather
//!    than handing a stack overflow to the first recursive walk over it.

pub mod dump;
pub mod form;
pub mod limits;
pub mod mutator;
pub mod node;
pub mod sink;
pub mod submit;
pub mod tree;

pub use form::{Control, FormState, InputKind};
pub use limits::DomLimits;
pub use mutator::DocumentMutator;
pub use node::{ElementData, Node, NodeData, NodeId, node_id_from_u64, node_id_to_u64};
pub use sink::DomSink;
pub use submit::{Encoding, Method, Submission};
pub use tree::Document;

#[cfg(test)]
mod tests {
    use html5ever::tendril::StrTendril;
    use html5ever::{Attribute, LocalName, QualName, local_name, ns};

    use super::*;

    fn element(name: &str) -> QualName {
        QualName::new(None, ns!(html), LocalName::from(name))
    }

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: QualName::new(None, ns!(), LocalName::from(name)),
            value: StrTendril::from(value),
        }
    }

    /// Build `<html><body>` and hand back the ids.
    fn skeleton(document: &mut Document) -> (NodeId, NodeId) {
        let mut dom = DocumentMutator::new(document);
        let root = dom.document().root();
        let html = dom.create_element(element("html"), vec![], None, false);
        let body = dom.create_element(element("body"), vec![], None, false);
        dom.append(root, html);
        dom.append(html, body);
        (html, body)
    }

    #[test]
    fn appending_links_children_in_order() {
        let mut document = Document::new();
        let (_html, body) = skeleton(&mut document);

        let mut dom = DocumentMutator::new(&mut document);
        let first = dom.create_element(element("p"), vec![], None, false);
        let second = dom.create_element(element("p"), vec![], None, false);
        dom.append(body, first);
        dom.append(body, second);

        let children: Vec<_> = document.children(body).collect();
        assert_eq!(children, vec![first, second]);
        assert_eq!(document.node(second).prev_sibling(), Some(first));
        assert_eq!(document.node(first).next_sibling(), Some(second));
        assert_eq!(document.node(first).parent, Some(body));
    }

    #[test]
    fn inserting_before_the_first_child_updates_the_parent() {
        let mut document = Document::new();
        let (_html, body) = skeleton(&mut document);

        let mut dom = DocumentMutator::new(&mut document);
        let existing = dom.create_element(element("p"), vec![], None, false);
        dom.append(body, existing);
        let inserted = dom.create_element(element("h1"), vec![], None, false);
        dom.insert_before(existing, inserted);

        assert_eq!(
            document.children(body).collect::<Vec<_>>(),
            vec![inserted, existing]
        );
        assert_eq!(document.node(body).first_child(), Some(inserted));
        assert_eq!(document.node(inserted).prev_sibling(), None);
    }

    #[test]
    fn detaching_repairs_both_neighbours() {
        let mut document = Document::new();
        let (_html, body) = skeleton(&mut document);

        let mut dom = DocumentMutator::new(&mut document);
        let a = dom.create_element(element("a"), vec![], None, false);
        let b = dom.create_element(element("b"), vec![], None, false);
        let c = dom.create_element(element("i"), vec![], None, false);
        dom.append(body, a);
        dom.append(body, b);
        dom.append(body, c);
        dom.detach(b);

        assert_eq!(document.children(body).collect::<Vec<_>>(), vec![a, c]);
        assert_eq!(document.node(a).next_sibling(), Some(c));
        assert_eq!(document.node(c).prev_sibling(), Some(a));
        assert_eq!(document.node(b).parent, None);
    }

    #[test]
    fn adjacent_text_is_merged_in_both_directions() {
        let mut document = Document::new();
        let (_html, body) = skeleton(&mut document);

        let mut dom = DocumentMutator::new(&mut document);
        dom.append_text(body, "one ".into());
        dom.append_text(body, "two".into());
        let marker = dom.create_element(element("br"), vec![], None, false);
        dom.append(body, marker);
        dom.insert_text_before(marker, " three".into());

        let children: Vec<_> = document.children(body).collect();
        assert_eq!(children.len(), 2, "expected one text node and the <br>");
        let NodeData::Text(text) = &document.node(children[0]).data else {
            panic!("expected a text node");
        };
        assert_eq!(&**text, "one two three");
    }

    #[test]
    fn reparenting_moves_every_child_in_order() {
        let mut document = Document::new();
        let (html, body) = skeleton(&mut document);

        let mut dom = DocumentMutator::new(&mut document);
        let a = dom.create_element(element("a"), vec![], None, false);
        let b = dom.create_element(element("b"), vec![], None, false);
        dom.append(body, a);
        dom.append(body, b);
        let target = dom.create_element(element("div"), vec![], None, false);
        dom.append(html, target);
        dom.reparent_children(body, target);

        assert_eq!(document.children(body).count(), 0);
        assert_eq!(document.children(target).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(document.node(a).parent, Some(target));
    }

    #[test]
    fn attributes_are_added_only_when_missing() {
        let mut document = Document::new();
        let mut dom = DocumentMutator::new(&mut document);
        let node = dom.create_element(element("div"), vec![attr("id", "first")], None, false);
        dom.add_attrs_if_missing(node, vec![attr("id", "second"), attr("class", "c")]);

        let element = document.node(node).element().expect("element");
        assert_eq!(element.attrs.len(), 2);
        assert_eq!(&*element.attrs[0].value, "first");
        assert_eq!(element.attrs[1].name.local, local_name!("class"));
    }

    #[test]
    fn a_removed_node_leaves_a_stale_handle_rather_than_an_alias() {
        let mut document = Document::new();
        let stale = {
            let mut dom = DocumentMutator::new(&mut document);
            let node = dom.create_element(element("div"), vec![], None, false);
            dom.detach(node);
            node
        };
        document.nodes_mut().remove(stale);
        // Force the slot to be reused.
        let mut dom = DocumentMutator::new(&mut document);
        let reused = dom.create_element(element("span"), vec![], None, false);

        assert_ne!(stale, reused);
        assert!(document.get(stale).is_none());
    }

    #[test]
    fn nesting_past_the_depth_limit_is_refused_rather_than_recursed() {
        let mut document = Document::with_limits(DomLimits {
            max_depth: 4,
            ..DomLimits::DEFAULT
        });
        let mut dom = DocumentMutator::new(&mut document);
        let mut parent = dom.document().root();
        let mut deepest = parent;
        for _ in 0..10 {
            let child = dom.create_element(element("div"), vec![], None, false);
            if dom.append(parent, child) {
                deepest = child;
                parent = child;
            }
        }

        assert_eq!(document.depth(deepest), 4);
        assert!(document.refused_insertions() > 0);
    }

    #[test]
    fn attributes_past_the_limit_are_dropped() {
        let mut document = Document::with_limits(DomLimits {
            max_attrs_per_element: 3,
            ..DomLimits::DEFAULT
        });
        let mut dom = DocumentMutator::new(&mut document);
        let attrs = (0..10)
            .map(|i| attr(&format!("a{i}"), "v"))
            .collect::<Vec<_>>();
        let node = dom.create_element(element("div"), attrs, None, false);

        assert_eq!(
            document.node(node).element().expect("element").attrs.len(),
            3
        );
        assert_eq!(document.refused_insertions(), 1);
    }
}
