//! # otlyra-layout — the box tree
//!
//! ## Purpose
//!
//! Turn a styled DOM into the tree layout actually operates on. Three trees exist
//! between an element and a pixel — DOM, box tree, fragment tree — and none of them
//! maps one-to-one onto its neighbour. This crate owns the second, and at M5 the
//! third.
//!
//! ## Contents
//!
//! - [`box_tree`] — [`BoxTree`], [`BoxNode`], and the invariant checker.
//! - [`builder`] — [`build_box_tree`]: DOM plus UA style becomes boxes.
//! - [`flow`] — [`layout`]: block and inline formatting contexts.
//! - [`fragment`] — [`FragmentTree`]: boxes once they have a position and a size.
//! - [`dump`] — the text forms, for `--dump-boxes`, `--dump-fragments` and snapshots.
//!
//! ## Invariants
//!
//! 1. **A box's children are either all block-level or all inline-level.** Anonymous
//!    block boxes are generated wherever a document breaks that, which is what lets
//!    block layout and inline layout be two separate algorithms.
//! 2. **`display: none` generates nothing**, for the element and every descendant.
//! 3. **The box tree is not the DOM.** It is built from it and refers back to it by
//!    `NodeId`, and nothing here mutates it.

pub mod box_tree;
pub mod builder;
pub mod dump;
pub mod flow;
pub mod fragment;

pub use box_tree::{
    BoxId, BoxKind, BoxNode, BoxTree, InvalidationReason, first_box_with_mixed_children,
};
pub use builder::build_box_tree;
pub use flow::{Viewport, layout};
pub use fragment::{Fragment, FragmentKind, FragmentTree, Rect};

#[cfg(test)]
mod tests {
    use otlyra_css::Display;

    use super::*;

    fn tree_of(html: &str) -> BoxTree {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        build_box_tree(&parsed.document)
    }

    fn dump(html: &str) -> String {
        dump::serialize(&tree_of(html))
    }

    /// Run on every fixture: the invariant is the point of the milestone.
    fn assert_no_mixed_children(tree: &BoxTree) {
        if let Some(id) = first_box_with_mixed_children(tree) {
            panic!(
                "box {:?} has both block and inline children:\n{}",
                tree.node(id).tag,
                dump::serialize(tree)
            );
        }
    }

    #[test]
    fn a_block_document_becomes_a_block_tree() {
        let tree = tree_of("<body><div><p>text</p></div>");
        assert_no_mixed_children(&tree);
        insta::assert_snapshot!(dump::serialize(&tree));
    }

    #[test]
    fn mixed_children_get_anonymous_block_wrappers() {
        let tree = tree_of("<body><div>loose text<p>a paragraph</p>more text</div>");
        assert_no_mixed_children(&tree);

        let wrappers = tree
            .descendants(tree.root())
            .into_iter()
            .filter(|&id| tree.node(id).anonymous)
            .count();
        assert_eq!(wrappers, 2, "one wrapper per run of inline children");
        insta::assert_snapshot!(dump::serialize(&tree));
    }

    #[test]
    fn a_block_with_only_inline_children_needs_no_wrapper() {
        let tree = tree_of("<body><p>plain <span>and inline</span> text</p>");
        assert_no_mixed_children(&tree);
        assert!(
            tree.descendants(tree.root())
                .into_iter()
                .all(|id| !tree.node(id).anonymous),
            "nothing to fix, so nothing invented"
        );
    }

    #[test]
    fn display_none_generates_no_box_for_the_element_or_its_descendants() {
        let tree = tree_of("<head><title>hidden</title></head><body><p>shown");
        assert_no_mixed_children(&tree);

        let dumped = dump::serialize(&tree);
        assert!(!dumped.contains("title"), "{dumped}");
        assert!(!dumped.contains("hidden"), "{dumped}");
        assert!(dumped.contains("\"shown\""), "{dumped}");
    }

    #[test]
    fn script_and_style_text_never_becomes_a_box() {
        let dumped = dump("<body><style>p{color:red}</style><script>var x=1</script><p>real");
        assert!(!dumped.contains("color:red"), "{dumped}");
        assert!(!dumped.contains("var x"), "{dumped}");
    }

    #[test]
    fn nested_inlines_stay_inline() {
        let tree = tree_of("<body><p><span><b>deep</b></span></p>");
        assert_no_mixed_children(&tree);

        let inlines = tree
            .descendants(tree.root())
            .into_iter()
            .filter(|&id| tree.node(id).kind == BoxKind::Inline)
            .count();
        assert_eq!(inlines, 2, "span and b");
    }

    #[test]
    fn an_unknown_element_is_inline_and_still_appears() {
        let tree = tree_of("<body><p>before <my-widget>inside</my-widget> after");
        assert_no_mixed_children(&tree);

        let widget = tree
            .descendants(tree.root())
            .into_iter()
            .find(|&id| {
                tree.node(id)
                    .tag
                    .as_ref()
                    .is_some_and(|tag| tag.as_ref() == "my-widget")
            })
            .expect("the unknown element should generate a box");
        assert_eq!(tree.node(widget).style.display, Display::Inline);
    }

    #[test]
    fn headings_carry_the_ua_font_size_down_to_their_text() {
        let tree = tree_of("<body><h1>big</h1><p>normal</p>");
        let sizes: Vec<f32> = tree
            .descendants(tree.root())
            .into_iter()
            .filter(|&id| matches!(tree.node(id).kind, BoxKind::Text(_)))
            .map(|id| tree.node(id).style.font_size)
            .collect();
        assert_eq!(sizes, vec![32.0, 16.0]);
    }

    #[test]
    fn whitespace_between_blocks_generates_no_text_box() {
        let tree = tree_of("<body>\n  <p>one</p>\n  <p>two</p>\n");
        let texts = tree
            .descendants(tree.root())
            .into_iter()
            .filter(|&id| matches!(tree.node(id).kind, BoxKind::Text(_)))
            .count();
        assert_eq!(texts, 2);
        assert_no_mixed_children(&tree);
    }

    #[test]
    fn the_invariant_holds_on_a_deliberately_malformed_document() {
        for html in [
            "<body><b>1<i>2</b>3</i>",
            "<table>stray<tr><td>cell",
            "<body><div><p>a<div>b</div>c</p></div>",
            "<body><span><div>block inside inline</div></span>",
            "<ul><li>one<li>two",
        ] {
            let tree = tree_of(html);
            assert_no_mixed_children(&tree);
        }
    }
}
