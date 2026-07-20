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
//! - [`damage`] — [`Damage`]: how much of the pipeline a change invalidates.
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
pub mod damage;
pub mod dump;
pub mod flow;
pub mod fragment;

pub use box_tree::{
    BoxId, BoxKind, BoxNode, BoxTree, InvalidationReason, box_id_from_u64, box_id_to_u64,
    first_box_with_mixed_children,
};
pub use builder::build_box_tree;
pub use damage::Damage;
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

    /// Text drawn from the box tree, in order — what the page actually says.
    fn text_of(tree: &BoxTree) -> String {
        tree.descendants(tree.root())
            .into_iter()
            .filter_map(|id| match &tree.node(id).kind {
                BoxKind::Text(text) => Some(text.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// An `<input>` is a void element: without generated content it lays out as
    /// nothing at all, and a form becomes a page of labels with no fields.
    #[test]
    fn a_text_field_shows_its_value_then_its_placeholder() {
        assert!(text_of(&tree_of("<input value=typed placeholder=hint>")).contains("typed"));
        assert!(text_of(&tree_of("<input placeholder=hint>")).contains("hint"));
        assert!(
            text_of(&tree_of("<input>")).len() > 4,
            "an empty field still reserves room to type in"
        );
    }

    #[test]
    fn a_checkbox_shows_whether_it_is_checked() {
        assert!(text_of(&tree_of("<input type=checkbox checked>")).contains("[x]"));
        assert!(text_of(&tree_of("<input type=checkbox>")).contains("[ ]"));
        assert!(text_of(&tree_of("<input type=radio checked>")).contains("(o)"));
    }

    #[test]
    fn a_button_input_is_labelled_by_its_value() {
        assert!(text_of(&tree_of("<input type=submit value=Send>")).contains("Send"));
        assert!(
            text_of(&tree_of("<input type=submit>")).contains("Submit"),
            "and by the default label when it has none"
        );
    }

    #[test]
    fn a_hidden_input_shows_nothing() {
        assert_eq!(text_of(&tree_of("<input type=hidden value=secret>")), "");
    }

    /// A select shows one option. A page of dropdowns that showed all of them
    /// would read as a wall of every choice in each.
    #[test]
    fn a_select_shows_only_the_option_it_displays() {
        let text = text_of(&tree_of(
            "<select><option>First<option selected>Second<option>Third</select>",
        ));
        assert!(text.contains("Second"), "{text}");
        assert!(!text.contains("First") && !text.contains("Third"), "{text}");

        let unselected = text_of(&tree_of("<select><option>First<option>Second</select>"));
        assert!(
            unselected.contains("First") && !unselected.contains("Second"),
            "with nothing selected it is the first: {unselected}"
        );
    }

    #[test]
    fn an_image_stands_in_with_its_alt_text() {
        assert!(text_of(&tree_of("<img src=x alt='a photo'>")).contains("a photo"));
        assert_eq!(text_of(&tree_of("<img src=x alt=''>")), "");
    }

    #[test]
    fn list_markers_are_bullets_and_numbers() {
        assert!(text_of(&tree_of("<ul><li>one<li>two")).starts_with("• one• two"));

        let ordered = text_of(&tree_of("<ol><li>one<li>two<li>three"));
        assert!(ordered.contains("1. one"), "{ordered}");
        assert!(ordered.contains("3. three"), "{ordered}");
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
