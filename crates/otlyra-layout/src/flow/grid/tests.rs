//! Grid layout, checked against whole documents.

use crate::flow::tests::{boxes_of, image_rect, laid_out, laid_out_with_image, picture};
use crate::{BoxTree, FragmentKind, FragmentTree, Rect};

/// The rectangles of the `t-<name>` elements, in document order.
fn all(tree: &FragmentTree, boxes: &BoxTree, name: &str) -> Vec<Rect> {
    boxes_of(tree, boxes, &format!("t-{name}"))
        .into_iter()
        .map(|fragment| fragment.rect)
        .collect()
}

fn one(tree: &FragmentTree, boxes: &BoxTree, name: &str) -> Rect {
    let found = all(tree, boxes, name);
    let [rect] = found[..] else {
        panic!("one t-{name}, found {found:?}");
    };
    rect
}

fn close(actual: f32, expected: f32, what: &str) {
    assert!(
        (actual - expected).abs() < 0.5,
        "{what}: {actual} against {expected}"
    );
}

fn page(css: &str, body: &str, width: f32) -> (FragmentTree, BoxTree) {
    laid_out(
        &format!(
            "<style>body {{ margin: 0 }} t-a, t-b, t-c, t-f, t-g, t-h, t-i, t-l, t-m, t-p, t-r, t-s, t-w, t-tall, t-skip, t-last {{ display: block }} {css}</style>{body}"
        ),
        width,
    )
}

/// An `auto` column is held to the room there is, so a paragraph in it wraps
/// rather than running out of the grid (§12.6).
#[test]
fn an_auto_column_is_held_to_the_room() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: auto; width: 300px; font: 16px/20px sans-serif }",
        "<t-g><t-p style='margin:0'>A paragraph far too long to sit on one line of \
         a three hundred pixel column, which it therefore wraps inside.</t-p></t-g>",
        800.0,
    );
    let paragraph = one(&tree, &boxes, "p");
    close(paragraph.width, 300.0, "the paragraph's width");
    assert!(paragraph.height > 20.0, "it wraps: {paragraph:?}");
}

/// A `width: 100%` item with padding in a one-column grid is the grid's width,
/// its percentage of the area it was given (yaru).
#[test]
fn a_full_width_item_fills_its_column() {
    let (tree, boxes) = page(
        "t-g { display: grid; width: 300px } \
         t-i { width: 100%; padding-left: 88px; box-sizing: border-box }",
        "<t-g><t-i>x</t-i></t-g>",
        800.0,
    );
    close(one(&tree, &boxes, "i").width, 300.0, "the item");
}

/// `repeat(4, minmax(0, 1fr))` stays as wide as its grid, however wide what
/// is in it wants to be (stripe).
#[test]
fn a_flexible_grid_stays_its_width() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); width: 700px } \
         t-w { width: 2000px }",
        "<t-g><t-w>a</t-w><div>b</div><div>c</div><div>d</div></t-g>",
        1280.0,
    );
    close(one(&tree, &boxes, "g").width, 700.0, "the grid");
}

/// MDN's page grid at 1280: the sidebars at 240, the content at 736.
#[test]
fn the_mdn_page_grid() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 240px) 32px \
         minmax(0, 736px) 32px minmax(0, 240px) minmax(0, 1fr) } \
         t-l { grid-column: 2 } t-m { grid-column: 4 } t-r { grid-column: 6 }",
        "<t-g><t-l>l</t-l><t-m>m</t-m><t-r>r</t-r></t-g>",
        1280.0,
    );
    let (left, main, right) = (
        one(&tree, &boxes, "l"),
        one(&tree, &boxes, "m"),
        one(&tree, &boxes, "r"),
    );
    close(left.width, 240.0, "left");
    close(main.x, 272.0, "main x");
    close(main.width, 736.0, "main");
    close(right.x, 1040.0, "right x");
}

/// An empty `minmax(25px, min-content)` row keeps its 25px (docs.rs).
#[test]
fn an_empty_row_keeps_its_minimum() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-rows: minmax(25px, min-content) auto } \
         t-i { grid-row: 2; height: 10px }",
        "<t-g><t-i></t-i></t-g>",
        400.0,
    );
    close(
        one(&tree, &boxes, "i").y,
        25.0,
        "the item in the second row",
    );
}

/// An `auto` row stretches to the grid's height, and to its minimum height.
#[test]
fn an_auto_row_stretches_to_the_grid() {
    for limit in ["height: 80px", "min-height: 80px"] {
        let (tree, boxes) = page(
            &format!("t-g {{ display: grid; {limit} }}"),
            "<t-g><t-i>x</t-i></t-g>",
            400.0,
        );
        close(one(&tree, &boxes, "i").height, 80.0, limit);
    }
}

/// A percentage row is of a grid that has a height.
#[test]
fn a_percentage_row_is_of_the_grids_height() {
    let (tree, boxes) = page(
        "t-g { display: grid; height: 200px; grid-template-rows: 50% auto }",
        "<t-g><t-a>a</t-a><t-b>b</t-b></t-g>",
        400.0,
    );
    close(one(&tree, &boxes, "a").height, 100.0, "the first row");
    close(one(&tree, &boxes, "b").y, 100.0, "the second row's top");
}

/// An absolutely positioned child takes no cell (§10.2), and the next item
/// has the first one (MDN's skip link).
#[test]
fn a_positioned_child_is_not_a_grid_item() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 100px 100px; position: relative } \
         t-skip { position: absolute; top: -100px }",
        "<t-g><t-skip href=#x>skip</t-skip><t-a>a</t-a></t-g>",
        400.0,
    );
    let first = one(&tree, &boxes, "a");
    close(first.x, 0.0, "the item's x");
    close(first.y, 0.0, "the item's y");
}

/// `grid-column: 2 / -2` spans the tracks between (stripe).
#[test]
fn negative_lines_span_from_the_end() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: repeat(6, minmax(0, 1fr)); width: 600px } \
         t-i { grid-column: 2 / -2 }",
        "<t-g><t-i>x</t-i></t-g>",
        800.0,
    );
    let item = one(&tree, &boxes, "i");
    close(item.x, 100.0, "the item's start");
    close(item.right(), 500.0, "the item's end");
}

/// An item that spans two rows beside two others makes both rows hold it.
#[test]
fn a_row_span_holds_its_rows() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 50px 50px } \
         t-tall { grid-row: span 2; height: 100px } t-s { height: 10px }",
        "<t-g><t-tall></t-tall><t-s></t-s><t-s></t-s></t-g>",
        400.0,
    );
    close(one(&tree, &boxes, "g").height, 100.0, "the grid");
    let small = all(&tree, &boxes, "s");
    close(small[0].x, 50.0, "the first small item beside the tall one");
    close(small[1].x, 50.0, "and the second below it");
}

/// Implicit rows take `grid-auto-rows`, and implicit columns cycle through
/// `grid-auto-columns`.
#[test]
fn implicit_tracks_take_their_sizes() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-auto-rows: 120px }",
        "<t-g><t-i>a</t-i><t-i>b</t-i></t-g>",
        400.0,
    );
    let rows = all(&tree, &boxes, "i");
    close(rows[0].height, 120.0, "the first row");
    close(rows[1].y, 120.0, "the second row's top");

    let (tree, boxes) = page(
        "t-g { display: grid; grid-auto-flow: column; grid-auto-columns: 10px 20px; width: 400px; \
         justify-content: start }",
        "<t-g><t-i>a</t-i><t-i>b</t-i><t-i>c</t-i></t-g>",
        400.0,
    );
    let widths: Vec<f32> = all(&tree, &boxes, "i")
        .iter()
        .map(|rect| rect.width)
        .collect();
    assert_eq!(widths, [10.0, 20.0, 10.0]);
}

/// The full-bleed template: named lines, and an item on the `content` lines
/// and one on the `full` ones.
#[test]
fn named_lines_place_items() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: [full-start] minmax(16px, 1fr) \
         [content-start] minmax(0, 600px) [content-end] minmax(16px, 1fr) [full-end] } \
         t-c { grid-column: content } t-f { grid-column: full }",
        "<t-g><t-c style='margin:0'>c</t-c><t-f>f</t-f></t-g>",
        1000.0,
    );
    let content = one(&tree, &boxes, "c");
    close(content.x, 200.0, "content start");
    close(content.width, 600.0, "content width");
    let full = one(&tree, &boxes, "f");
    close(full.x, 0.0, "full start");
    close(full.width, 1000.0, "full width");
}

/// `repeat(auto-fill, minmax(100px, 1fr))` in 350 makes three columns that
/// share it; `auto-fit` with two items collapses the rest.
#[test]
fn automatic_repetitions_fill_and_fit() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: repeat(auto-fill, minmax(100px, 1fr)); width: 350px }",
        "<t-g><t-i>a</t-i></t-g>",
        800.0,
    );
    close(one(&tree, &boxes, "i").width, 350.0 / 3.0, "a third");

    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: repeat(auto-fit, minmax(100px, 1fr)); width: 400px }",
        "<t-g><t-i>a</t-i><t-i>b</t-i></t-g>",
        800.0,
    );
    let items = all(&tree, &boxes, "i");
    close(items[0].width, 200.0, "half");
    close(items[1].x, 200.0, "the second half");
}

/// Named areas place their items.
#[test]
fn named_areas_place_items() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 100px 300px; \
         grid-template-areas: 'h h' 's m' 'f f' } \
         t-h { grid-area: h } t-s { grid-area: s } t-m { grid-area: m } t-f { grid-area: f }",
        "<t-g><t-f>f</t-f><t-m>m</t-m><t-s>s</t-s>\
         <t-h>h</t-h></t-g>",
        800.0,
    );
    let (header, side, main, footer) = (
        one(&tree, &boxes, "h"),
        one(&tree, &boxes, "s"),
        one(&tree, &boxes, "m"),
        one(&tree, &boxes, "f"),
    );
    close(header.width, 400.0, "header across");
    close(side.x, 0.0, "side at the start");
    close(main.x, 100.0, "main beside it");
    close(side.y, header.bottom(), "the second row under the first");
    close(footer.y, main.bottom(), "the footer last");
}

/// `place-items: center` centres an item in its area.
#[test]
fn items_align_in_their_areas() {
    let (tree, boxes) = page(
        "t-g { display: grid; place-items: center; width: 200px; height: 200px } \
         t-i { width: 40px; height: 20px }",
        "<t-g><t-i></t-i></t-g>",
        400.0,
    );
    let item = one(&tree, &boxes, "i");
    close(item.x, 80.0, "x");
    close(item.y, 90.0, "y");

    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 100px } t-i { justify-self: end; width: 30px }",
        "<t-g><t-i>x</t-i></t-g>",
        400.0,
    );
    close(one(&tree, &boxes, "i").right(), 100.0, "justify-self: end");

    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 100px } t-i { margin: 0 auto; width: 30px }",
        "<t-g><t-i>x</t-i></t-g>",
        400.0,
    );
    close(one(&tree, &boxes, "i").x, 35.0, "auto margins centre");
}

/// bbc: `justify-items: center` with a `max-width` bar keeps its maximum and
/// centres it, margins and all.
#[test]
fn a_held_item_centres() {
    let (tree, boxes) = page(
        "t-g { display: grid; justify-items: center; width: 600px } \
         t-i { width: 100%; max-width: 368px; margin-inline: 16px }",
        "<t-g><t-i>x</t-i></t-g>",
        800.0,
    );
    let item = one(&tree, &boxes, "i");
    close(item.width, 368.0, "held at its maximum");
    close(item.x, 116.0, "centred");
}

/// A picture keeps its natural size in a taller row: `normal` does not
/// stretch what has a ratio.
#[test]
fn a_picture_keeps_its_size() {
    let (tree, _) = laid_out_with_image(
        "<style>body { margin: 0 } t-g { display: grid; grid-template-rows: 100px }</style>\
         <t-g><img src=a.png></t-g>",
        400.0,
        picture(40, 20),
    );
    let image = image_rect(&tree);
    close(image.width, 40.0, "width");
    close(image.height, 20.0, "height");
}

/// A stretched item's child with `height: 100%` fills the row.
#[test]
fn a_stretched_items_percentage_height_is_of_the_row() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-rows: 90px } t-c { height: 100% }",
        "<t-g><div><t-c>x</t-c></div></t-g>",
        400.0,
    );
    close(one(&tree, &boxes, "c").height, 90.0, "the child");
}

/// `justify-content: center` moves the tracks into the middle, and
/// `align-content: end` to the bottom.
#[test]
fn content_distribution_moves_the_tracks() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 100px 100px; width: 400px; \
         justify-content: center; height: 300px; align-content: end; grid-template-rows: 50px }",
        "<t-g><t-i>a</t-i><t-i>b</t-i></t-g>",
        800.0,
    );
    let first = all(&tree, &boxes, "i")[0];
    close(first.x, 100.0, "centred across");
    close(first.y, 250.0, "at the bottom");
}

/// A grid's max-content width is the sum of its columns and gaps, so a float
/// holding one is exactly as wide (bbc).
#[test]
fn a_grids_intrinsic_width_is_its_columns() {
    let (tree, boxes) = page(
        "t-f { float: left } t-g { display: grid; grid-template-columns: repeat(3, 100px); gap: 5px }",
        "<t-f><t-g><i>a</i><i>b</i><i>c</i></t-g></t-f>",
        800.0,
    );
    close(one(&tree, &boxes, "f").width, 310.0, "the float");
}

/// An `inline-grid` sits on the line with the text around it.
#[test]
fn an_inline_grid_is_inline() {
    let (tree, boxes) = page(
        "t-g { display: inline-grid; grid-template-columns: 20px 20px }",
        "<p style='margin:0'>before <t-g><i>a</i><i>b</i></t-g> after</p>",
        800.0,
    );
    let grid = one(&tree, &boxes, "g");
    assert!(grid.x > 0.0, "after the text before it: {grid:?}");
    close(grid.width, 40.0, "its columns");
    let after = tree
        .iter()
        .find_map(|fragment| match &fragment.kind {
            FragmentKind::Text(run) if run.text.contains("after") => Some(fragment.rect),
            _ => None,
        })
        .expect("the text after it");
    assert!(
        after.x >= grid.right() - 0.5,
        "after it on the line: {after:?}"
    );
    assert!(
        after.y < grid.bottom(),
        "on the same line: {after:?} and {grid:?}"
    );
}

/// An item's own `order` places it and paints it first.
#[test]
fn order_moves_an_item_first() {
    let (tree, boxes) = page(
        "t-g { display: grid; grid-template-columns: 50px 50px } t-last { order: -1 }",
        "<t-g><t-a>a</t-a><t-last>b</t-last></t-g>",
        400.0,
    );
    close(one(&tree, &boxes, "last").x, 0.0, "first");
    close(one(&tree, &boxes, "a").x, 50.0, "second");
}

/// A grid that sizes itself to its own content asks its columns, not itself,
/// what that is.
#[test]
fn a_grid_sized_to_its_content_measures_its_columns() {
    for width in ["min-content", "max-content", "fit-content"] {
        let (tree, boxes) = page(
            &format!(
                "t-g {{ display: grid; width: {width}; grid-template-columns: 50px 70px; \
                 min-width: min-content }}"
            ),
            "<t-g><t-a>a</t-a><t-b>b</t-b></t-g>",
            400.0,
        );
        close(one(&tree, &boxes, "g").width, 120.0, width);
    }
}
