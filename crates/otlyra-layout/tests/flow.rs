//! Block and inline layout, judged on geometry.
//!
//! Every case uses the vendored font through [`TextEngine::isolated`], so the
//! numbers hold on any machine. A layout test measured against a system font is a
//! layout test that fails on someone else's laptop.

use otlyra_layout::fragment::{Fragment, FragmentKind, FragmentTree};
use otlyra_layout::{Viewport, build_box_tree, dump, layout};
use otlyra_text::{FontStack, TextEngine};

/// Lay out `html` at `width` logical pixels, with the document's own stylesheets
/// applied — which is what a `style=` attribute in a fixture needs.
fn lay_out_styled(html: &str, width: f32) -> FragmentTree {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    let styles = otlyra_css::cascade::style_document(
        &parsed.document,
        otlyra_css::cascade::Viewport {
            width,
            height: 600.0,
            scale: 1.0,
        },
    );
    let boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
    let mut text = isolated_engine();
    layout(
        &boxes,
        &mut text,
        Viewport {
            width,
            height: 600.0,
        },
    )
}

/// Lay out `html` at `width` logical pixels.
fn lay_out(html: &str, width: f32) -> FragmentTree {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    let boxes = build_box_tree(&parsed.document);
    let mut text = isolated_engine();
    layout(
        &boxes,
        &mut text,
        Viewport {
            width,
            height: 600.0,
        },
    )
}

/// An engine that can only see the vendored family, whatever the document asks
/// for. The font stack a page names is resolved by name, and on a machine without
/// that font the metrics differ — so the tests pin the font instead.
fn isolated_engine() -> TextEngine {
    let mut engine = TextEngine::isolated();
    assert!(engine.has_family(otlyra_text::TEST_FAMILY));
    engine
}

fn fragments(tree: &FragmentTree) -> Vec<&Fragment> {
    tree.iter().collect()
}

fn lines(tree: &FragmentTree) -> Vec<&Fragment> {
    tree.iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .collect()
}

fn boxes_of(tree: &FragmentTree) -> Vec<&Fragment> {
    tree.iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Box))
        .collect()
}

/// Boxes that directly contain lines — the paragraph-shaped ones, as opposed to
/// the root, `<html>` and `<body>` wrappers that contain those.
fn text_blocks(tree: &FragmentTree) -> Vec<&Fragment> {
    tree.iter()
        .filter(|fragment| {
            matches!(fragment.kind, FragmentKind::Box)
                && fragment
                    .children
                    .iter()
                    .any(|child| matches!(child.kind, FragmentKind::Line))
        })
        .collect()
}

#[test]
fn a_single_paragraph_becomes_one_line() {
    let tree = lay_out("<body><p>hello", 800.0);
    assert_eq!(lines(&tree).len(), 1);
    insta::assert_snapshot!(dump::serialize_fragments(&tree));
}

#[test]
fn blocks_stack_downward_and_never_overlap() {
    let tree = lay_out("<body><p>one</p><p>two</p><p>three</p>", 800.0);
    let paragraphs = text_blocks(&tree);
    assert_eq!(paragraphs.len(), 3);

    for pair in paragraphs.windows(2) {
        assert!(
            pair[1].rect.y >= pair[0].rect.bottom(),
            "{:?} overlaps {:?}",
            pair[1].rect,
            pair[0].rect
        );
    }
}

#[test]
fn a_paragraph_wraps_to_the_width_it_is_given() {
    let text = "the quick brown fox jumps over the lazy dog again and again";
    let wide = lay_out(&format!("<body><p>{text}"), 800.0);
    let narrow = lay_out(&format!("<body><p>{text}"), 200.0);

    assert_eq!(lines(&wide).len(), 1);
    assert!(
        lines(&narrow).len() > 2,
        "expected wrapping at 200px, got {} lines",
        lines(&narrow).len()
    );
    for line in lines(&narrow) {
        assert!(line.rect.width <= 200.0, "line is {:?}", line.rect);
    }
}

#[test]
fn lines_stack_downward_within_a_paragraph() {
    let tree = lay_out(
        "<body><p>the quick brown fox jumps over the lazy dog again and again",
        200.0,
    );
    let lines = lines(&tree);
    assert!(lines.len() > 2);
    for pair in lines.windows(2) {
        assert!(
            pair[1].rect.y >= pair[0].rect.bottom() - 0.01,
            "line at {:?} overlaps the one at {:?}",
            pair[1].rect,
            pair[0].rect
        );
    }
}

#[test]
fn a_margin_separates_two_paragraphs() {
    let tree = lay_out("<body><p>one</p><p>two</p>", 800.0);
    // The UA margin is 1em of the 16px body text, so 16px above and below.
    let paragraphs = text_blocks(&tree);
    let first = paragraphs[0];
    let second = paragraphs[1];
    let gap = second.rect.y - first.rect.bottom();
    assert!(
        (gap - 16.0).abs() < 0.5,
        "gap was {gap}px; the two margins collapse into the larger of them"
    );
}

#[test]
fn padding_moves_content_inward_and_makes_the_box_larger() {
    // <ul> is the one element in the UA table with padding: 40px on the left.
    let tree = lay_out("<body><ul><li>item", 800.0);
    let list = tree
        .iter()
        .find(|fragment| {
            matches!(fragment.kind, FragmentKind::Box)
                && fragment.style.padding.left != otlyra_css::Length::ZERO
        })
        .expect("the list should have padding");
    let line = lines(&tree)[0];

    assert!(
        line.rect.x >= list.rect.x + 40.0,
        "text at {} should sit inside the padding of a box at {}",
        line.rect.x,
        list.rect.x
    );
}

#[test]
fn a_heading_is_taller_than_body_text() {
    let heading = lay_out("<body><h1>title", 800.0);
    let paragraph = lay_out("<body><p>title", 800.0);
    assert!(
        lines(&heading)[0].rect.height > lines(&paragraph)[0].rect.height,
        "a 32px heading must make a taller line than 16px body text"
    );
}

/// Mixed font sizes on one line: the tall span sets the line height and both sit on
/// one baseline. This is the case a hand-rolled line builder gets wrong first.
#[test]
fn one_line_holds_two_font_sizes_on_a_shared_baseline() {
    let tree = lay_out("<body><p>normal <small>smaller</small> normal", 800.0);
    let line = lines(&tree)[0];

    let runs: Vec<_> = line
        .children
        .iter()
        .filter_map(|child| match &child.kind {
            FragmentKind::Text(run) => Some(run),
            _ => None,
        })
        .collect();

    assert!(runs.len() >= 2, "expected several runs on the line");
    let baselines: Vec<f32> = runs.iter().map(|run| run.glyphs[0].y).collect();
    for baseline in &baselines {
        assert!(
            (baseline - baselines[0]).abs() < 0.01,
            "runs sit at {baselines:?}; they must share a baseline"
        );
    }
}

#[test]
fn br_forces_a_line_break() {
    let single = lay_out("<body><p>one two", 800.0);
    let broken = lay_out("<body><p>one<br>two", 800.0);

    assert_eq!(lines(&single).len(), 1);
    assert_eq!(lines(&broken).len(), 2);
}

#[test]
fn an_empty_block_takes_no_height() {
    let tree = lay_out("<body><div></div>", 800.0);
    let empty = boxes_of(&tree)
        .into_iter()
        .find(|fragment| fragment.children.is_empty())
        .expect("the empty div");
    assert_eq!(empty.rect.height, 0.0);
}

#[test]
fn layout_is_deterministic_across_runs() {
    let html = "<body><h1>title</h1><p>the quick brown fox jumps over the lazy dog";
    let first = dump::serialize_fragments(&lay_out(html, 300.0));
    for _ in 0..9 {
        assert_eq!(dump::serialize_fragments(&lay_out(html, 300.0)), first);
    }
    insta::assert_snapshot!(first);
}

/// A resize is a reflow, not a rebuild: the same boxes land in different places.
#[test]
fn the_same_document_reflows_at_three_widths() {
    let html = "<body><h1>A heading that is long enough to wrap</h1><p>the quick brown fox jumps over the lazy dog";

    let mut previous_lines = 0;
    for width in [900.0, 500.0, 260.0] {
        let tree = lay_out(html, width);
        let lines = lines(&tree);

        for line in &lines {
            assert!(
                line.rect.right() <= width + 0.5,
                "a line at {:?} runs past a {width}px viewport",
                line.rect
            );
        }
        assert!(
            lines.len() > previous_lines,
            "narrower must mean more lines: {} at {width}px after {previous_lines}",
            lines.len()
        );
        previous_lines = lines.len();
    }
}

#[test]
fn culling_keeps_only_the_fragments_that_touch_the_viewport() {
    let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(200);
    let tree = lay_out(&html, 800.0);
    let viewport = otlyra_layout::Rect::new(0.0, 0.0, 800.0, 600.0);

    let visible = tree.visible(&viewport, &viewport).count();
    assert!(visible < fragments(&tree).len() / 4, "{visible} visible");
    assert!(visible > 0);
}

/// The font stack a document names is resolved by name; when nothing matches, the
/// generic fallback has to still produce glyphs rather than nothing.
#[test]
fn text_still_shapes_when_the_named_family_is_missing() {
    let stack = FontStack::parse_css("NoSuchFamily, sans-serif");
    assert_eq!(stack.families().len(), 2);

    let tree = lay_out("<body><p>text", 800.0);
    let FragmentKind::Text(run) = &lines(&tree)[0].children[0].kind else {
        panic!("expected text");
    };
    assert!(!run.glyphs.is_empty());
}

/// A line is as tall as the block's own font asks for, however small the things
/// inside it are.
///
/// This is the strut: a paragraph of ordinary text with a smaller inline in it —
/// a `<code>`, a `<small>` — spaces its lines by its own font, not by whichever
/// span happens to be on a line. Getting it from the span was worth two pixels a
/// line, every line, and the lines the small span was nowhere near.
#[test]
fn a_smaller_inline_does_not_make_its_line_shorter() {
    let plain = lay_out("<body><p>one<br>two", 800.0);
    // A heading is larger than the text in it, and `<sub>` is smaller than the
    // heading — so the second line holds something shorter than its own block.
    let mixed = lay_out("<body><h2>one<br>two <sub>small</sub>", 800.0);

    let heights = |tree: &FragmentTree| -> Vec<f32> {
        lines(tree).iter().map(|line| line.rect.height).collect()
    };
    let plain = heights(&plain);
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0], plain[1], "both lines of one font agree");

    let mixed = heights(&mixed);
    assert_eq!(mixed.len(), 2);
    // Within the fraction of a pixel that separates a line measured from the next
    // line's top from the last line, measured from its own height.
    assert!(
        (mixed[0] - mixed[1]).abs() < 0.5,
        "and so do both lines of two: {mixed:?}"
    );
    assert!(
        mixed[0] > plain[0],
        "a twenty-pixel paragraph has taller lines than a sixteen-pixel one: \
         {mixed:?} against {plain:?}"
    );
}

/// `vertical-align` moves a box off the line's baseline and makes the line taller
/// to fit it.
///
/// Both halves matter and only together: raised glyphs that do not grow the line
/// are clipped by whatever is above them, and a taller line with the glyphs still
/// on the baseline is a gap for nothing.
#[test]
fn a_raised_or_lowered_box_moves_and_makes_room() {
    /// Every glyph baseline in the page, in page coordinates.
    fn baselines(tree: &FragmentTree) -> Vec<f32> {
        tree.iter()
            .filter_map(|fragment| match &fragment.kind {
                FragmentKind::Text(run) => Some(fragment.rect.y + run.glyphs.first()?.y),
                _ => None,
            })
            .collect()
    }
    fn line_height(tree: &FragmentTree) -> f32 {
        tree.iter()
            .find_map(|f| matches!(f.kind, FragmentKind::Line).then_some(f.rect.height))
            .expect("a line")
    }

    let plain = lay_out("<body><p>x", 800.0);
    let flat = baselines(&plain);
    let sitting = flat[0];

    let raised = lay_out("<body><p>x<sup>up</sup>", 800.0);
    assert!(
        baselines(&raised).iter().any(|&y| y < sitting - 1.0),
        "a superscript sits above the baseline: {:?}",
        baselines(&raised)
    );
    assert!(
        line_height(&raised) > line_height(&plain),
        "and its line grew to hold it"
    );

    let lowered = lay_out("<body><p>x<sub>dn</sub>", 800.0);
    assert!(
        baselines(&lowered).iter().any(|&y| y > sitting + 1.0),
        "a subscript sits below it: {:?}",
        baselines(&lowered)
    );
    assert!(line_height(&lowered) > line_height(&plain));

    // And nothing moves when nothing asks to.
    let level = lay_out("<body><p>x<b>y</b>", 800.0);
    assert!(
        baselines(&level)
            .iter()
            .all(|&y| (y - sitting).abs() < 0.01)
    );
}

/// A table places its cells in columns sized by what is in them, and is only as
/// wide as those columns need.
#[test]
fn a_table_sizes_its_columns_from_their_contents() {
    let tree = lay_out(
        "<body><table><tr><td>a</td><td>a much longer cell</td></tr>\
         <tr><td>b</td><td>short</td></tr></table>",
        800.0,
    );

    // Two rows of two, and every cell in a column starts at the same x and every
    // cell in a row at the same y.
    let cells: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|f| f.style.display == otlyra_layout::Display::TableCell)
        .collect();
    assert_eq!(cells.len(), 4);
    assert_eq!(
        cells[0].rect.x, cells[2].rect.x,
        "one column, one left edge"
    );
    assert_eq!(cells[1].rect.x, cells[3].rect.x);
    assert_eq!(cells[0].rect.y, cells[1].rect.y, "one row, one top edge");
    assert!(cells[2].rect.y > cells[0].rect.y, "and the rows stack");

    // The column holding a long sentence is wider than the one holding a letter.
    assert!(
        cells[1].rect.width > cells[0].rect.width * 3.0,
        "columns are sized by their contents: {:?}",
        cells.iter().map(|c| c.rect.width).collect::<Vec<_>>()
    );

    // And the table is nowhere near the eight hundred pixels it was offered.
    let table = boxes_of(&tree)
        .into_iter()
        .find(|f| f.style.display == otlyra_layout::Display::Table)
        .expect("a table");
    assert!(
        table.rect.width < 400.0,
        "a table is as wide as its columns need: {}",
        table.rect.width
    );
    assert!(table.rect.width > cells[0].rect.width + cells[1].rect.width);
}

/// A table told how wide to be fills that width rather than sitting narrow in it.
#[test]
fn a_table_with_a_width_fills_it() {
    let tree = lay_out_styled(
        "<body><table style='width: 400px'><tr><td>a</td><td>b</td></tr></table>",
        800.0,
    );
    let table = boxes_of(&tree)
        .into_iter()
        .find(|f| f.style.display == otlyra_layout::Display::Table)
        .expect("a table");
    assert!(
        (table.rect.width - 400.0).abs() < 0.01,
        "table was {} wide",
        table.rect.width
    );
}
