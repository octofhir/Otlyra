//! Finding a string in the text of a laid-out page.
//!
//! Every case uses the vendored font through [`TextEngine::isolated`], because a
//! test that wraps a line is a test about where the line broke, and a line
//! measured in a system font breaks somewhere else on someone else's laptop.

use otlyra_layout::selection;
use otlyra_layout::{FragmentTree, Viewport, build_styled_box_tree, find, layout};
use otlyra_text::TextEngine;

/// Lay out `html` at `width` logical pixels, with the document's own stylesheets
/// applied — which is what a `style=` attribute in a fixture needs.
fn lay_out(html: &str, width: f32) -> FragmentTree {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    let styles = otlyra_css::cascade::style_document(
        &parsed.document,
        otlyra_css::cascade::Viewport {
            width,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let mut boxes = build_styled_box_tree(&parsed.document, &styles);
    let mut text = TextEngine::isolated();
    assert!(text.has_family(otlyra_text::TEST_FAMILY));
    layout(
        &mut boxes,
        &mut text,
        Viewport {
            width,
            height: 600.0,
        },
    )
}

/// What each match reads as, which is what proves it is the characters it claims
/// and not merely the right count of them.
fn found(tree: &FragmentTree, query: &str) -> Vec<String> {
    find::matches(tree, query)
        .into_iter()
        .map(|at| selection::text(tree, at))
        .collect()
}

/// A phrase is found where it is written, however the markup broke it up.
#[test]
fn a_match_crosses_the_runs_a_bold_word_breaks_a_sentence_into() {
    let tree = lay_out("<body><p>the <b>bold</b>face is one word</p>", 800.0);

    // `bold` and `face` are two runs on one line with nothing between them, so
    // they are one piece of text and the word crosses the seam.
    assert_eq!(found(&tree, "boldface"), ["boldface"]);
    assert_eq!(found(&tree, "e boldface i"), ["e boldface i"]);

    // And the two positions a match comes back as are the two the highlight is
    // drawn from: one rectangle per run it touches.
    let at = find::matches(&tree, "boldface");
    let rects = selection::rects(&tree, at[0]);
    assert_eq!(
        rects.len(),
        2,
        "a match over two runs is a rectangle over each of them: {rects:?}"
    );
    assert!(
        rects[0].right() <= rects[1].x + 0.5 && (rects[0].y - rects[1].y).abs() < 0.5,
        "and they sit side by side on one line: {rects:?}"
    );
}

/// Case is not part of the question, and no part of the query is a pattern.
#[test]
fn the_search_is_a_lowercased_substring_and_nothing_more() {
    let tree = lay_out("<body><p>Hello HELLO hello</p>", 800.0);

    assert_eq!(found(&tree, "hello").len(), 3, "however it was written");
    assert_eq!(found(&tree, "HeLLo").len(), 3, "and however it was typed");
    assert_eq!(found(&tree, "Hello"), ["Hello", "HELLO", "hello"]);

    // No regular expressions: the characters of the query are the characters
    // looked for.
    let tree = lay_out("<body><p>h.llo hello hxllo</p>", 800.0);
    assert_eq!(found(&tree, "h.llo"), ["h.llo"], "a dot is a dot");
    assert_eq!(found(&tree, "h.*llo"), Vec::<String>::new());
}

/// Overlapping occurrences are one stop each, not one per starting place.
#[test]
fn matches_do_not_overlap() {
    let tree = lay_out("<body><p>aaaa</p>", 800.0);
    assert_eq!(
        found(&tree, "aa").len(),
        2,
        "four letters hold two pairs a reader can be taken to"
    );
}

/// A phrase a reader can see across two lines is one phrase.
#[test]
fn a_match_crosses_a_line_the_paragraph_wrapped_at() {
    // Narrow enough that the paragraph has to break, and the break is inside the
    // phrase looked for.
    let tree = lay_out("<body><p>alpha bravo charlie delta echo</p>", 90.0);

    let lines = tree
        .iter()
        .filter(|fragment| matches!(fragment.kind, otlyra_layout::FragmentKind::Line))
        .count();
    assert!(
        lines > 1,
        "the fixture has to wrap for this to prove anything"
    );

    let whole = selection::text(&tree, selection::all(&tree).expect("a page with text"));
    assert!(
        whole.contains('\n'),
        "and the break has to be a break in the text: {whole:?}"
    );

    assert_eq!(
        find::matches(&tree, "alpha bravo charlie delta echo").len(),
        1,
        "the whole paragraph is one phrase however many lines it took"
    );
}

/// And a phrase never runs out of one paragraph into the next.
#[test]
fn a_match_stops_at_the_edge_of_a_block() {
    let tree = lay_out("<body><p>one two</p><p>three four</p>", 800.0);

    assert_eq!(found(&tree, "two"), ["two"]);
    assert_eq!(found(&tree, "three"), ["three"]);
    assert_eq!(
        find::matches(&tree, "two three"),
        Vec::new(),
        "two paragraphs are two texts, whatever the gap between them looks like"
    );
}

/// Whatever the markup did with its indentation does not decide what is found.
#[test]
fn whitespace_is_collapsed_on_both_sides_of_the_comparison() {
    let tree = lay_out("<body><p>one     two\n\n   three</p>", 800.0);

    assert_eq!(
        find::matches(&tree, "one two three").len(),
        1,
        "the page reads as one space between words"
    );
    assert_eq!(
        find::matches(&tree, "one     two   three").len(),
        1,
        "and so does the query"
    );
}

/// The linearized text is the page in reading order, so it can be looked at.
#[test]
fn the_page_reads_as_one_sequence_in_paint_order() {
    let tree = lay_out("<body><h1>Title</h1><p>the <b>bold</b>face</p>", 800.0);
    let text = find::PageText::of(&tree).text();

    assert_eq!(
        text, "title\nthe boldface",
        "one block per line, one word across the seam, all of it lowercased"
    );
}

/// A page with no text at all is a page with nothing to find.
#[test]
fn an_empty_page_and_an_empty_query_find_nothing() {
    let tree = lay_out("<body>", 800.0);
    assert_eq!(find::matches(&tree, "anything"), Vec::new());

    let tree = lay_out("<body><p>something</p>", 800.0);
    assert_eq!(
        find::matches(&tree, ""),
        Vec::new(),
        "nothing typed is nothing looked for, rather than everywhere"
    );
}
