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
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let mut boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
    let mut text = isolated_engine();
    layout(
        &mut boxes,
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
    let mut boxes = build_box_tree(&parsed.document);
    let mut text = isolated_engine();
    layout(
        &mut boxes,
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

/// A paragraph is as tall as its lines, and a line is as tall as what is on it.
///
/// The shaper carries a line height per *run* of glyphs and opens a run when the
/// font changes, so it can neither be told that one span of the same font wants a
/// taller line nor asked to put the baseline where CSS puts it. So the paragraph
/// is restacked from what actually landed on each line. Before that, one height
/// was worked out for the paragraph and every line was given it — which on
/// sixteen-pixel text around one thirty-two-pixel word was nearly twice the
/// paragraph a reference draws.
///
/// Every height here was measured against one.
#[test]
fn only_the_line_a_tall_thing_is_on_is_tall() {
    let paragraph = |markup: &str| {
        let tree = lay_out_styled(
            &format!(
                "<body style='margin:0'><p style='width:300px;margin:0;line-height:1.5'>\
                 one two three four five six seven {markup} eight nine ten eleven twelve \
                 thirteen fourteen fifteen sixteen</p>"
            ),
            500.0,
        );
        let heights: Vec<f32> = lines(&tree).iter().map(|line| line.rect.height).collect();
        let block = text_blocks(&tree)[0].rect.height;
        (heights, block)
    };

    let (plain, plain_height) = paragraph("plain");
    assert!(plain.len() >= 2, "several lines: {plain:?}");
    assert!(
        plain
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() < 0.01),
        "one font, one height: {plain:?}"
    );

    // A larger word makes its own line tall and leaves the rest alone.
    let (mixed, mixed_height) = paragraph("<span style='font-size:32px'>BIG</span>");
    let tall = mixed
        .iter()
        .filter(|height| **height > plain[0] + 0.5)
        .count();
    assert_eq!(tall, 1, "one line grew, and one only: {mixed:?}");
    assert!(
        mixed_height < plain_height + plain[0] * 1.5,
        "the paragraph grew by about one line rather than throughout: \
         {mixed_height} against {plain_height}"
    );

    // The same for a `line-height` the shaper cannot carry at all, because the
    // span shares its font with the text around it.
    let (told, _) = paragraph("<span style='line-height:3'>taller</span>");
    assert_eq!(
        told.iter()
            .filter(|height| **height > plain[0] + 0.5)
            .count(),
        1,
        "one line grew: {told:?}"
    );
}

/// A line is as tall as the block's own font asks for, however small the things
/// inside it are — and as tall as what is *on* it where that is taller.
///
/// The strut is the floor: a paragraph of ordinary text with a smaller inline in
/// it — a `<code>`, a `<small>` — spaces its lines by its own font, not by
/// whichever span happens to be on a line. Getting it from the span was worth two
/// pixels a line, every line, and the lines the small span was nowhere near.
///
/// The ceiling is the line's own: a span lowered below the baseline hangs out of
/// the strut and the line grows to hold it, on that line and no other. Both halves
/// were measured against a reference.
#[test]
fn a_line_is_the_strut_at_least_and_what_is_on_it_at_most() {
    let heights = |tree: &FragmentTree| -> Vec<f32> {
        lines(tree).iter().map(|line| line.rect.height).collect()
    };

    let plain = heights(&lay_out("<body><p>one<br>two", 800.0));
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0], plain[1], "both lines of one font agree");

    // A `<small>` sits on the baseline, so its line is the strut like the others.
    let smaller = heights(&lay_out("<body><p>one<br>two <small>small</small>", 800.0));
    assert_eq!(smaller.len(), 2);
    assert!(
        (smaller[0] - smaller[1]).abs() < 0.5 && (smaller[0] - plain[0]).abs() < 0.5,
        "a smaller inline on the baseline changes nothing: {smaller:?}"
    );

    // A `<sub>` is lowered, so it reaches below the strut and its line grows —
    // the line it is on, and not the one above it.
    let lowered = heights(&lay_out("<body><p>one<br>two <sub>small</sub>", 800.0));
    assert_eq!(lowered.len(), 2);
    assert!(
        (lowered[0] - plain[0]).abs() < 0.5,
        "the line the subscript is nowhere near is unmoved: {lowered:?}"
    );
    assert!(
        lowered[1] > lowered[0] + 0.5,
        "and the line holding it is taller: {lowered:?}"
    );

    // A larger paragraph has taller lines throughout, which is the strut again.
    let heading = heights(&lay_out("<body><h2>one<br>two", 800.0));
    assert!(
        heading[0] > plain[0],
        "a twenty-pixel paragraph has taller lines than a sixteen-pixel one: \
         {heading:?} against {plain:?}"
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

/// Every cell of a table, in the order the markup writes them.
fn cells_of(tree: &FragmentTree) -> Vec<&Fragment> {
    boxes_of(tree)
        .into_iter()
        .filter(|f| f.style.display == otlyra_layout::Display::TableCell)
        .collect()
}

/// A `<col>` gives its column a width and a background, and a `span` gives them
/// to as many columns as it names.
///
/// The width is a floor rather than a cap: a column told sixty and holding a word
/// eighty wide comes out eighty, which is what a reference does and is the only
/// place the rule is written plainly.
#[test]
fn a_column_gives_its_width_and_its_background_to_the_column() {
    let widths = |markup: &str| {
        let tree = lay_out_styled(
            &format!(
                "<body><table style='border-collapse: collapse'>{markup}\
                 <tr><td>a</td><td>b</td><td>cccccccccccccccc</td></tr></table>"
            ),
            800.0,
        );
        cells_of(&tree)
            .into_iter()
            .map(|cell| cell.rect.width)
            .collect::<Vec<_>>()
    };

    let plain = widths("");
    let told = widths("<col style='width:200px'><col><col>");
    assert_eq!(told[0], 200.0, "the column takes the width it was told");
    assert_eq!(
        (told[1], told[2]),
        (plain[1], plain[2]),
        "and the others are unmoved"
    );

    // One `span` describes two columns.
    let spanned = widths("<col span=2 style='width:120px'><col>");
    assert_eq!((spanned[0], spanned[1]), (120.0, 120.0));

    // A width narrower than the longest word loses to the word: the column cannot
    // go below what the content cannot break.
    let narrow = widths("<col><col><col style='width:20px'>");
    assert_eq!(narrow[2], plain[2], "one word, and it does not break");

    // Where the content *can* break, the column is the width it was told and the
    // text wraps inside it — a cap rather than a floor, which is the half a
    // reference had to settle.
    let tree = lay_out_styled(
        "<body><table style='border-collapse: collapse'><col><col style='width:60px'>         <tr><td>a</td><td>one two three four</td></tr></table>",
        800.0,
    );
    let wrapped = cells_of(&tree)[1];
    assert!(
        (wrapped.rect.width - 60.0).abs() < 0.01,
        "the column is the width it was told: {:?}",
        wrapped.rect
    );

    // A `<colgroup>` with no columns in it describes as many as its own `span`.
    let tree = lay_out_styled(
        "<body><table style='border-collapse: collapse'>\
         <colgroup span=2 style='background: rgb(0,255,0)'></colgroup>\
         <colgroup><col style='background: rgb(255,0,0)'></colgroup>\
         <tr><td>a</td><td>b</td><td>c</td></tr></table>",
        800.0,
    );
    let painted: Vec<(f32, otlyra_gfx::peniko::Color)> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| {
            fragment.style.background_color == otlyra_gfx::peniko::Color::from_rgb8(0, 255, 0)
                || fragment.style.background_color
                    == otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0)
        })
        .map(|fragment| (fragment.rect.x, fragment.style.background_color))
        .collect();
    assert_eq!(painted.len(), 3, "three columns painted: {painted:?}");
    assert_eq!(
        painted[0].1,
        otlyra_gfx::peniko::Color::from_rgb8(0, 255, 0)
    );
    assert_eq!(
        painted[1].1,
        otlyra_gfx::peniko::Color::from_rgb8(0, 255, 0)
    );
    assert_eq!(
        painted[2].1,
        otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0),
        "the group's own style does not reach the columns inside it"
    );
    assert!(
        painted[0].0 < painted[1].0 && painted[1].0 < painted[2].0,
        "one per column, in order: {painted:?}"
    );
}

/// A cell across two columns covers both of them and the gap between them, and
/// the cells below it keep their own columns.
#[test]
fn a_cell_spans_columns() {
    let tree = lay_out(
        "<body><table><tr><td colspan=2>wide</td><td>c</td></tr>\
         <tr><td>a</td><td>b</td><td>c</td></tr></table>",
        800.0,
    );

    let cells = cells_of(&tree);
    assert_eq!(cells.len(), 5);
    let (spanning, third) = (cells[0], cells[1]);
    let (first, second) = (cells[2], cells[3]);

    assert_eq!(
        spanning.rect.x, first.rect.x,
        "a span starts where it would"
    );
    assert!(
        spanning.rect.right() >= second.rect.right() - 0.01
            && spanning.rect.right() <= third.rect.x,
        "a span across two columns reaches the end of the second: {:?}",
        (spanning.rect.right(), second.rect.right(), third.rect.x),
    );
    assert_eq!(
        third.rect.x, cells[4].rect.x,
        "the third column is untouched"
    );
}

/// A cell down two rows reaches into the next one, and the cells of that row
/// start after it rather than under it.
#[test]
fn a_cell_spans_rows() {
    let tree = lay_out(
        "<body><table><tr><td rowspan=2>tall</td><td>b1</td></tr>\
         <tr><td>b2</td></tr>\
         <tr><td>a3</td><td>b3</td></tr></table>",
        800.0,
    );

    let cells = cells_of(&tree);
    assert_eq!(cells.len(), 5);
    let (tall, first) = (cells[0], cells[1]);
    let (second, third) = (cells[2], cells[3]);

    assert!(
        tall.rect.height > first.rect.height * 1.9,
        "a cell down two rows is as tall as both of them: {} against {}",
        tall.rect.height,
        first.rect.height
    );
    assert!(
        tall.rect.bottom() >= second.rect.bottom() - 0.01,
        "and reaches the bottom of the second row"
    );
    assert_eq!(
        second.rect.x, first.rect.x,
        "the second row's cell is pushed past the one reaching into it"
    );
    assert_eq!(
        third.rect.x, tall.rect.x,
        "and the row below both is back in the first column"
    );
}

/// A row span past the last row is clamped to the rows there are, and a span of
/// zero is every row that is left.
#[test]
fn a_row_span_stops_at_the_last_row() {
    for markup in [
        "<body><table><tr><td rowspan=9>tall</td><td>b1</td></tr><tr><td>b2</td></tr></table>",
        "<body><table><tr><td rowspan=0>tall</td><td>b1</td></tr><tr><td>b2</td></tr></table>",
    ] {
        let tree = lay_out(markup, 800.0);
        let cells = cells_of(&tree);
        let (tall, last) = (cells[0], cells[2]);
        assert!(
            (tall.rect.bottom() - last.rect.bottom()).abs() < 0.01,
            "a span past the end stops at the last row: {} against {}",
            tall.rect.bottom(),
            last.rect.bottom()
        );
    }
}

/// A cell wider than the columns under it widens them between them, in proportion
/// to what each already asked for.
#[test]
fn a_span_wider_than_its_columns_shares_the_difference_out() {
    let tree = lay_out(
        "<body><table><tr><td colspan=2>a sentence far wider than either column below it</td></tr>\
         <tr><td>a</td><td>bbbb</td></tr></table>",
        800.0,
    );

    let cells = cells_of(&tree);
    let (spanning, narrow, wide) = (cells[0], cells[1], cells[2]);
    // The two columns and the spacing between them: exactly what the span covers.
    assert!(
        (wide.rect.right() - narrow.rect.x - spanning.rect.width).abs() < 0.01,
        "the two columns add up to the span: {:?}",
        (narrow.rect.width, wide.rect.width, spanning.rect.width)
    );
    assert!(
        wide.rect.width > narrow.rect.width,
        "and the column that wanted more still has more"
    );
}

/// Collapsed, two cells meet on one edge: the spacing is ignored and neither
/// cell has a gap beside it.
#[test]
fn collapsed_cells_share_an_edge() {
    let tree = lay_out_styled(
        "<body><table style='border-collapse: collapse; border-spacing: 10px'>\
         <tr><td style='border: 1px solid'>a</td><td style='border: 1px solid'>b</td></tr>\
         </table>",
        800.0,
    );

    let cells = cells_of(&tree);
    assert_eq!(cells.len(), 2);
    assert!(
        (cells[1].rect.x - cells[0].rect.right()).abs() < 0.01,
        "collapsed cells meet: {} against {}",
        cells[1].rect.x,
        cells[0].rect.right()
    );

    // Each of them draws half of the one-pixel line between them.
    let edges = cells[0].used.as_ref().expect("a cell has used edges");
    assert!(
        (edges.border.right - 0.5).abs() < 0.01,
        "{:?}",
        edges.border
    );
}

/// The wider of two neighbouring borders is the one that is drawn, and both cells
/// draw half of it.
#[test]
fn the_wider_collapsed_border_wins() {
    let tree = lay_out_styled(
        "<body><table style='border-collapse: collapse'>\
         <tr><td style='border: 1px solid'>a</td><td style='border: 4px solid'>b</td></tr>\
         </table>",
        800.0,
    );

    let cells = cells_of(&tree);
    let left = cells[0].used.as_ref().expect("used edges");
    let right = cells[1].used.as_ref().expect("used edges");
    assert!(
        (left.border.right - 2.0).abs() < 0.01 && (right.border.left - 2.0).abs() < 0.01,
        "the shared edge is four wide and halved: {:?} {:?}",
        left.border,
        right.border
    );
    assert!(
        (left.border.left - 0.5).abs() < 0.01,
        "the edge nobody contested is the cell's own: {:?}",
        left.border
    );
}

/// A table is at least as wide as its caption's longest word, with its columns
/// stretched to fill.
#[test]
fn a_caption_widens_the_table_under_it() {
    let narrow = lay_out("<body><table><tr><td>a</td><td>b</td></tr></table>", 800.0);
    let captioned = lay_out(
        "<body><table><caption>Incomprehensible</caption><tr><td>a</td><td>b</td></tr></table>",
        800.0,
    );

    let width = |tree: &FragmentTree| {
        boxes_of(tree)
            .into_iter()
            .find(|f| f.style.display == otlyra_layout::Display::Table)
            .expect("a table")
            .rect
            .width
    };
    assert!(
        width(&captioned) > width(&narrow) * 2.0,
        "a one-word caption widens the table: {} against {}",
        width(&captioned),
        width(&narrow)
    );

    let cells = cells_of(&captioned);
    assert!(
        (cells[1].rect.right() - cells[0].rect.x - width(&captioned)).abs() < 4.1,
        "and the columns are stretched to fill it: {:?}",
        (cells[0].rect.x, cells[1].rect.right(), width(&captioned))
    );
}

/// A positioned box with a width and padding is that much wider than the width,
/// exactly as a box in the flow is: `width` is the content box.
#[test]
fn a_positioned_box_adds_its_padding_to_its_width() {
    let tree = lay_out_styled(
        "<body><div style='position: relative; height: 100px'>\
         <div id=box style='position: absolute; left: 0; top: 0; width: 160px; \
         height: 90px; padding: 4px'>x</div></div>",
        800.0,
    );

    let positioned = boxes_of(&tree)
        .into_iter()
        .find(|fragment| fragment.style.position == otlyra_css::Position::Absolute)
        .expect("the positioned box");
    assert!(
        (positioned.rect.width - 168.0).abs() < 0.01
            && (positioned.rect.height - 98.0).abs() < 0.01,
        "the border box is the width plus the padding: {:?}",
        positioned.rect
    );
}

/// `box-sizing: border-box` takes the padding and the border out of the width
/// rather than adding them outside it.
#[test]
fn a_border_box_measures_across_its_edges() {
    let tree = lay_out_styled(
        "<body style='margin: 0'>\
         <div id=a style='box-sizing: border-box; width: 200px; height: 100px; \
         padding: 20px; border: 5px solid'>a</div>\
         <div id=b style='width: 200px; height: 100px; padding: 20px; border: 5px solid'>b</div>",
        800.0,
    );

    let boxes: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| fragment.style.padding.top != otlyra_css::Length::ZERO)
        .collect();
    assert_eq!(boxes.len(), 2);
    assert!(
        (boxes[0].rect.width - 200.0).abs() < 0.01 && (boxes[0].rect.height - 100.0).abs() < 0.01,
        "a border box is the size it says: {:?}",
        boxes[0].rect
    );
    assert!(
        (boxes[1].rect.width - 250.0).abs() < 0.01 && (boxes[1].rect.height - 150.0).abs() < 0.01,
        "a content box is that size plus its edges: {:?}",
        boxes[1].rect
    );
}

/// A flexible item takes what is left along its container's *main* axis, which
/// down a column is the height rather than the width.
#[test]
fn a_column_shares_out_its_height() {
    let tree = lay_out_styled(
        "<body style='margin: 0'><div style='width: 400px; height: 300px'>\
         <div style='display: flex; flex-direction: column; height: 100%'>\
         <div style='height: 40px'>head</div>\
         <div style='flex: 1'>rest</div></div></div>",
        800.0,
    );

    let rest = boxes_of(&tree)
        .into_iter()
        .find(|fragment| fragment.style.flex_grow > 0.0)
        .expect("the flexible item");
    assert!(
        (rest.rect.height - 260.0).abs() < 0.01,
        "what is left of three hundred after forty: {:?}",
        rest.rect
    );
}

/// A percentage height is a percentage of the containing block's *height*, and
/// means nothing at all against one that is as tall as its own contents.
#[test]
fn a_percentage_height_is_of_a_height() {
    let tree = lay_out_styled(
        "<body style='margin: 0'>\
         <div style='width: 300px'><div id=a style='height: 100%'>x</div></div>\
         <div style='width: 300px; height: 200px'><div id=b style='height: 50%'>x</div></div>",
        800.0,
    );

    let boxes: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| {
            fragment.style.height != otlyra_css::LengthOrAuto::Auto
                && matches!(fragment.style.height, otlyra_css::LengthOrAuto::Percent(_))
        })
        .collect();
    assert_eq!(boxes.len(), 2, "both boxes ask for a percentage");
    assert!(
        boxes[0].rect.height < 30.0,
        "against a box as tall as its contents it is `auto`, not the width: {:?}",
        boxes[0].rect
    );
    assert!(
        (boxes[1].rect.height - 100.0).abs() < 0.01,
        "against a box two hundred tall it is one hundred: {:?}",
        boxes[1].rect
    );
}

/// A second click takes a word, a third takes the block it is in, and neither
/// stops at the edge of a run.
#[test]
fn a_word_and_a_paragraph_are_what_the_second_and_third_click_take() {
    use otlyra_layout::selection;

    let tree = lay_out_styled(
        "<body><p>one two-part <b>thr</b>ee</p><p>four five</p>",
        800.0,
    );

    let word = |x: f32, y: f32| {
        let at = selection::position_at(&tree, x, y).expect("a position");
        selection::text(&tree, selection::word_at(&tree, at))
    };

    assert_eq!(word(4.0, 20.0), "one", "the word the click landed in");
    // A hyphen is not a letter, so it is its own word — which is what a second
    // click on one selects everywhere else.
    let across = word(60.0, 20.0);
    assert!(
        across == "two" || across == "-" || across == "part",
        "one part of the hyphenated word rather than all of it: {across:?}"
    );

    // `thr` and `ee` are two runs and one word, and the join is a join because
    // they meet on the same line with nothing between them.
    let split = selection::position_at(&tree, 800.0, 20.0).expect("the end of the line");
    let joined = selection::text(&tree, selection::word_at(&tree, split));
    assert_eq!(joined, "three", "a word half in bold is still a word");

    let at = selection::position_at(&tree, 4.0, 20.0).expect("a position");
    let paragraph = selection::text(&tree, selection::paragraph_at(&tree, at));
    assert_eq!(
        paragraph, "one two-part three",
        "the block it is in, and not the one after it"
    );

    let everything = selection::text(&tree, selection::all(&tree).expect("a page with text"));
    assert!(
        everything.contains("one two-part three") && everything.contains("four five"),
        "and everything is everything: {everything:?}"
    );
}

/// A ligature is one glyph and two letters, and a click after one lands where the
/// letters are rather than where the glyphs are.
///
/// The test face draws `fi` as a single shape, so this line is drawn with two
/// glyphs fewer than it has characters. Stepping the glyphs and the characters
/// together — which is what a run used to do — puts every offset after the first
/// ligature a letter early, and a second click on the last word takes the one
/// before it.
#[test]
fn a_click_past_a_ligature_lands_on_the_letter_it_is_over() {
    use otlyra_layout::selection;

    let tree = lay_out("<body><p>difficult offices xyz</p>", 800.0);

    let fragment = tree
        .iter()
        .find(|fragment| matches!(fragment.kind, FragmentKind::Text(_)))
        .expect("a run of text");
    let FragmentKind::Text(run) = &fragment.kind else {
        unreachable!()
    };
    assert!(
        run.glyphs.len() < run.text.chars().count(),
        "the test face draws every pair separately, so this proves nothing"
    );

    // The last three glyphs are `xyz`, which nothing ligates: the first of them
    // is where the last word starts on the screen.
    let last_word = run.glyphs[run.glyphs.len() - 3];
    let x = fragment.rect.x + last_word.x + 1.0;
    let y = fragment.rect.y + fragment.rect.height / 2.0;

    let at = selection::position_at(&tree, x, y).expect("a position");
    assert_eq!(
        selection::text(&tree, selection::word_at(&tree, at)),
        "xyz",
        "the word under the pointer, two ligatures along the line"
    );
}

/// A point on the page resolves to a place in its text, and the text between two
/// such places comes back as the characters that were drawn there.
#[test]
fn a_selection_reads_the_words_it_covers() {
    use otlyra_layout::selection;

    let tree = lay_out("<body><p>one two three</p><p>four five</p>", 800.0);

    let first = selection::position_at(&tree, 0.0, 20.0).expect("a position on the first line");
    let last = selection::position_at(&tree, 800.0, 60.0).expect("a position on the last");
    let all = otlyra_layout::Selection {
        anchor: first,
        focus: last,
    };

    let text = selection::text(&tree, all);
    assert!(
        text.contains("one two three") && text.contains("four five"),
        "a selection across both paragraphs reads both: {text:?}"
    );
    assert!(
        text.contains('\n'),
        "and the line between them is a line: {text:?}"
    );

    // A selection of nothing reads nothing, and covers nothing.
    let empty = otlyra_layout::Selection::at(first);
    assert!(selection::text(&tree, empty).is_empty());
    assert!(selection::rects(&tree, empty).is_empty());

    // Part of one line is a rectangle inside that line rather than the whole of it.
    let line = otlyra_layout::Selection {
        anchor: first,
        focus: otlyra_layout::TextPosition {
            run: first.run,
            offset: 3,
        },
    };
    let rects = selection::rects(&tree, line);
    assert_eq!(rects.len(), 1, "one line, one rectangle: {rects:?}");
    let lines: Vec<&Fragment> = tree
        .iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .collect();
    assert!(
        rects[0].width > 0.0 && rects[0].width < lines[0].rect.width,
        "part of the line rather than all of it: {:?} of {:?}",
        rects[0],
        lines[0].rect
    );
}

/// An `inline-block` takes its place in a line rather than a line of its own, at
/// the size it was given, and what is in it is laid out as a block.
#[test]
fn inline_blocks_sit_in_one_line() {
    let tree = lay_out_styled(
        "<body><p><span id=a style='display: inline-block; width: 90px; height: 70px'>a</span>\
         <span id=b style='display: inline-block; width: 60px; height: 30px'>b</span></p>",
        800.0,
    );

    let boxes: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| fragment.style.display == otlyra_layout::Display::InlineBlock)
        .collect();
    assert_eq!(boxes.len(), 2, "both are boxes of their own");
    assert!(
        boxes[1].rect.x >= boxes[0].rect.right() - 0.01
            && (boxes[1].rect.y - boxes[0].rect.y).abs() < 20.0,
        "the second sits after the first rather than under it: {:?}",
        (boxes[0].rect, boxes[1].rect)
    );
    assert!(
        (boxes[0].rect.width - 90.0).abs() < 0.01 && (boxes[0].rect.height - 70.0).abs() < 0.01,
        "a width and a height are honoured, which is what makes it not an inline: {:?}",
        boxes[0].rect
    );
}

/// Two of them of different heights sit on one baseline: their *own* last
/// baselines line up, which is what makes a row of buttons read as a row of words.
#[test]
fn inline_blocks_share_a_baseline() {
    let tree = lay_out_styled(
        "<body><p><span style='display: inline-block; padding: 20px 4px'>tall</span>\
         <span style='display: inline-block; padding: 4px'>short</span></p>",
        800.0,
    );

    let baselines: Vec<f32> = tree
        .iter()
        .filter_map(|fragment| match &fragment.kind {
            FragmentKind::Text(run) => Some(fragment.rect.y + run.glyphs.first()?.y),
            _ => None,
        })
        .collect();
    assert!(baselines.len() >= 2, "both have text in them");
    for baseline in &baselines {
        assert!(
            (baseline - baselines[0]).abs() < 0.01,
            "they sit on one baseline: {baselines:?}"
        );
    }
}

/// A collapsed table draws its grid itself, once: the cells leave room for the
/// lines and paint none of them, and the line that is drawn is the one that won
/// the edge.
#[test]
fn a_collapsed_table_draws_the_line_that_won() {
    let tree = lay_out_styled(
        "<body><table style='border-collapse: collapse'>\
         <tr><td style='border: 1px solid rgb(0,0,0)'>a</td>\
         <td style='border: 4px solid rgb(255,0,0)'>b</td></tr></table>",
        800.0,
    );

    for cell in cells_of(&tree) {
        for side in [
            cell.style.border.top,
            cell.style.border.right,
            cell.style.border.bottom,
            cell.style.border.left,
        ] {
            assert!(
                !side.is_visible(),
                "a cell leaves room for the line and draws none of it: {side:?}"
            );
        }
    }

    // The four-pixel red edge between the two cells, drawn once and whole.
    let red = otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0);
    let lines: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| fragment.style.background_color == red)
        .collect();
    let shared = cells_of(&tree)[0].rect.right();
    assert!(
        lines.iter().any(|line| {
            (line.rect.width - 4.0).abs() < 0.01
                && (line.rect.x + line.rect.width / 2.0 - shared).abs() < 1.0
        }),
        "the wider border is drawn whole, on the edge the cells meet at: {:?}",
        lines.iter().map(|line| line.rect).collect::<Vec<_>>()
    );
}

/// Two collapsed borders of the same width are settled by their style, and
/// `hidden` silences the edge outright.
///
/// Both were left to source order before: `double` and `solid` at the same width
/// came down to which cell was written first, and `hidden` was a zero-width
/// border like `none` — so a neighbour put its own line back on an edge the page
/// had asked to be left blank.
#[test]
fn a_collapsed_border_is_settled_by_style_where_the_widths_agree() {
    let colour = |css: &str| {
        let tree = lay_out_styled(
            &format!(
                "<body><table style='border-collapse: collapse'>\
                 <tr><td style='border: 3px solid rgb(0,0,255)'>a</td>\
                 <td style='{css}'>b</td></tr></table>"
            ),
            800.0,
        );
        let shared = cells_of(&tree)[0].rect.right();
        boxes_of(&tree)
            .into_iter()
            .filter(|fragment| fragment.rect.width <= 4.0 && fragment.rect.height > 4.0)
            .find(|line| (line.rect.x + line.rect.width / 2.0 - shared).abs() < 1.0)
            .map(|line| line.style.background_color)
    };

    // Same width, louder style: `double` beats `solid` however they were written.
    assert_eq!(
        colour("border: 3px double rgb(255,0,0)"),
        Some(otlyra_gfx::peniko::Color::from_rgb8(255, 0, 0)),
        "double takes an edge from solid at the same width"
    );
    // Same width, quieter style: the first one keeps it.
    assert_eq!(
        colour("border: 3px dotted rgb(255,0,0)"),
        Some(otlyra_gfx::peniko::Color::from_rgb8(0, 0, 255)),
        "solid keeps an edge from dotted at the same width"
    );
    // `hidden` is not a narrow border. It is a blank edge nothing may draw on.
    assert_eq!(
        colour("border-style: hidden"),
        None,
        "hidden silenced the edge, so nothing is drawn on it"
    );
}

/// A cell that reaches across a boundary is not divided by it: no line is drawn
/// inside a `colspan`.
#[test]
fn a_collapsed_line_is_not_drawn_through_a_span() {
    let tree = lay_out_styled(
        "<body><table style='border-collapse: collapse'>\
         <tr><td colspan=2 style='border: 1px solid rgb(0,0,0)'>wide</td></tr>\
         <tr><td style='border: 1px solid rgb(0,0,0)'>a</td>\
         <td style='border: 1px solid rgb(0,0,0)'>b</td></tr></table>",
        800.0,
    );

    let cells = cells_of(&tree);
    let (spanning, first) = (cells[0], cells[1]);
    let boundary = first.rect.right();
    let black = otlyra_gfx::peniko::Color::from_rgb8(0, 0, 0);

    let through: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| {
            fragment.style.background_color == black
                && fragment.rect.width <= 2.0
                && (fragment.rect.x - boundary).abs() < 2.0
                && fragment.rect.y < spanning.rect.bottom() - 1.0
        })
        .collect();
    assert!(
        through.is_empty(),
        "a line was drawn through the span: {:?}",
        through.iter().map(|line| line.rect).collect::<Vec<_>>()
    );

    // And it *is* drawn between the two cells below it.
    let between: Vec<&Fragment> = boxes_of(&tree)
        .into_iter()
        .filter(|fragment| {
            fragment.style.background_color == black
                && fragment.rect.width <= 2.0
                && (fragment.rect.x - boundary).abs() < 2.0
        })
        .collect();
    assert!(!between.is_empty(), "the boundary below the span is drawn");
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

/// An inline block is laid out at the origin and moved into its line afterwards.
/// The rectangle it cuts its contents off at has to travel with it: left behind at
/// the origin it cuts the box off where the box is not, which is a field whose text
/// has disappeared.
#[test]
fn a_clip_travels_with_the_inline_block_it_belongs_to() {
    let tree = lay_out_styled(
        "<body><p>before <span style='display: inline-block; overflow: hidden; \
         width: 60px'>inside</span>",
        800.0,
    );
    let clipped = tree
        .iter()
        .find_map(|fragment| fragment.clip)
        .expect("something inside the inline block is clipped");
    assert!(
        clipped.x > 20.0,
        "the clip stayed at the origin: {clipped:?}"
    );
}
