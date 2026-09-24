//! Flex layout, checked against whole documents: the phases of CSS Flexbox §9
//! in the order the specification runs them, and what laying each item out once
//! means for the page.

use crate::flow::tests::{boxes_of, laid_out, laid_out_with_image, picture, rect_of};
use crate::{Fragment, FragmentKind, FragmentTree};

/// Every text fragment, with the characters it drew.
fn texts(tree: &FragmentTree) -> Vec<(String, &Fragment)> {
    tree.iter()
        .filter_map(|fragment| match &fragment.kind {
            FragmentKind::Text(run) => Some((run.text.to_string(), fragment)),
            _ => None,
        })
        .collect()
}

/// `depth` flex containers, each the only item of the one around it, around a
/// word.
fn nested(depth: usize, direction: &str) -> String {
    let open = format!("<div style=\"display:flex;flex-direction:{direction}\">");
    format!(
        "<style>body {{ margin: 0 }}</style><body>{}hello{}",
        open.repeat(depth),
        "</div>".repeat(depth)
    )
}

/// Measuring each item, measuring it again at its main size and then laying it
/// out made nested flex containers cost three layouts per level: `3^32` of them
/// here, which would never finish. Laid out once per level and measured once, it
/// is a few hundred, and the word is where it would be with one container.
#[test]
fn deeply_nested_flex_containers_lay_out_and_keep_their_word_in_place() {
    for direction in ["row", "column"] {
        let (tree, boxes) = laid_out(&nested(32, direction), 800.0);
        let (_, word) = texts(&tree)
            .into_iter()
            .find(|(text, _)| text.contains("hello"))
            .expect("the word is laid out");
        assert_eq!((word.rect.x, word.rect.y), (0.0, 0.0), "{direction}");

        let levels = boxes_of(&tree, &boxes, "div");
        assert_eq!(levels.len(), 32, "{direction}: every level, once");
        let innermost = levels.last().expect("32 levels").rect;
        assert!(innermost.height > 0.0, "{direction}: a line tall");
        for level in &levels {
            assert_eq!(level.rect.height, innermost.height, "{direction}");
        }
        if direction == "column" {
            assert!(levels.iter().all(|level| level.rect.width == 800.0));
        }
    }
}

/// A line is as tall as its tallest item, and `stretch` makes the others match
/// (§9.4, step 11) — including an item that is itself a flex container, whose
/// own items are stretched to the height it was given, and so on down.
#[test]
fn a_stretched_chain_reaches_the_height_of_its_tallest_sibling() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex }</style>\
         <div class=flex>\
           <main class=flex><section class=flex><article>x</article></section></main>\
           <aside style='height:100px'>tall</aside>\
         </div>",
        800.0,
    );
    for tag in ["main", "section", "article", "aside"] {
        assert_eq!(rect_of(&tree, &boxes, tag).height, 100.0, "<{tag}>");
    }
}

/// `align-content` shares out what the lines leave of the container's height,
/// and how tall a line is depends on how wide its items ended up: a hundred
/// pixels wide, three words wrap to three lines. Sized from items measured at
/// the container's width — one line of text each — the lines were handed room
/// they did not have, and the last one hung out of the bottom.
#[test]
fn align_content_shares_out_what_the_lines_really_leave() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { display: flex; flex-wrap: wrap; width: 300px; height: 200px; \
               align-content: space-between; align-items: start } \
         span { flex: 0 0 100px }</style>\
         <div><span>aaaaaaa bbbbbbb ccccccc</span><span>a</span><span>b</span>\
         <span>aaaaaaa bbbbbbb ccccccc</span></div>",
        800.0,
    );
    let items = boxes_of(&tree, &boxes, "span");
    assert_eq!(items.len(), 4);
    let (first, last) = (items[0].rect, items[3].rect);
    let one_line = items[1].rect.height;
    assert!(
        first.height > one_line * 2.5,
        "three lines of text, not one: {first:?}"
    );
    assert_eq!(first.y, 0.0, "the first line at the top");
    assert_eq!(last.x, 0.0, "the fourth item wrapped");
    assert!(
        (last.bottom() - 200.0).abs() < 0.01,
        "the last line ends at the container's bottom edge, not at {}",
        last.bottom()
    );
}

/// Measuring an item lays it out somewhere it will never be drawn. A box inside
/// it that scrolls registered a scroll port there, once per measuring pass per
/// level, and the scrollbars left behind stacked into black bars down the page.
/// Measuring leaves nothing behind: the one port is where the box really is.
#[test]
fn a_scrolling_item_deep_in_flex_containers_has_one_port_where_it_is() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex }</style>\
         <div class=flex><div class=flex><div class=flex>\
           <div style='width:300px'>left</div>\
           <section style='overflow:auto;height:200px;width:200px'>\
             <div style='height:1000px'>tall</div></section>\
         </div></div></div>",
        800.0,
    );
    let section = rect_of(&tree, &boxes, "section");
    assert_eq!(tree.scroll_ports.len(), 1, "{:?}", tree.scroll_ports);
    assert_eq!(tree.scroll_ports[0].port, section);
    assert_eq!(section.x, 300.0);
}

/// A list item's marker waits for the item's first line, which here is inside
/// the first item of a flex container. Measuring that item took the marker for
/// a line that was then thrown away, and the list had no bullet.
#[test]
fn a_list_item_holding_a_flex_container_keeps_its_marker() {
    let (tree, _) = laid_out(
        "<style>body { margin: 0 } ul { margin: 0; padding-left: 40px }</style>\
         <ul><li><div style='display:flex'><span>one</span><span>two</span></div></li></ul>",
        800.0,
    );
    let markers: Vec<_> = texts(&tree)
        .into_iter()
        .filter(|(text, _)| text.contains('\u{2022}'))
        .collect();
    assert_eq!(markers.len(), 1, "one bullet");
    let (_, marker) = markers[0];
    let (_, one) = texts(&tree)
        .into_iter()
        .find(|(text, _)| text.contains("one"))
        .expect("the first item's text");
    assert!(marker.rect.right() <= 40.0, "outside the content box");
    assert_eq!(marker.rect.y, one.rect.y, "on the first line");
}

/// A percentage in a flex item's padding and margins is of the container's
/// inner width, whichever way the container runs (§4.2) — not of the width the
/// item itself ended up.
#[test]
fn a_percentage_padding_on_a_flex_item_is_of_the_container() {
    for direction in ["row", "column"] {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }}</style>\
                 <div style='display:flex;flex-direction:{direction};width:400px'>\
                 <section style='width:100px;padding:10% 0 0 10%;margin-left:5%'>x</section></div>"
            ),
            800.0,
        );
        let item = boxes_of(&tree, &boxes, "section").remove(0);
        let used = item.used.expect("a flex item carries its edges");
        assert_eq!(used.padding.left, 40.0, "{direction}");
        assert_eq!(used.padding.top, 40.0, "{direction}");
        assert_eq!(used.margin.left, 20.0, "{direction}");
        assert_eq!(item.rect.width, 140.0, "{direction}");
        assert_eq!(item.rect.x, 20.0, "{direction}");
    }
}

/// A flex item is the root of a formatting context of its own (§4): a float
/// inside it stays inside it and makes it tall enough to hold it (CSS 2.2
/// §10.6.7), and does not reach into the item beside it.
#[test]
fn a_flex_item_holds_its_own_floats() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex; align-items: start }</style>\
         <div class=flex>\
           <section><div style='float:left;width:50px;height:80px'></div></section>\
           <article>beside</article>\
         </div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 80.0);
    let article = rect_of(&tree, &boxes, "article");
    let (_, beside) = texts(&tree)
        .into_iter()
        .find(|(text, _)| text.contains("beside"))
        .expect("the second item's text");
    assert_eq!(
        beside.rect.x, article.x,
        "no float reaches into the next item"
    );
}

/// An absolutely positioned box is out of flow (CSS Position 3 §3) and adds
/// nothing to its parent's intrinsic sizes (CSS Sizing 3 §5). A header's third
/// item holds nothing but a positioned alert 300px wide; counted, it took the
/// alert's width from the row and pulled the group pushed right by
/// `margin-left: auto` 300px short of the edge. Both references: the blue box
/// ends at the right edge.
#[test]
fn a_positioned_child_takes_no_room_from_the_row() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         header { display: flex; position: relative; height: 40px } \
         nav { margin-left: auto } \
         b { display: block; width: 40px; height: 30px } \
         aside { position: absolute; top: 50px; left: 10px; width: 300px; height: 20px }</style>\
         <header><div>left</div><nav><b></b></nav><div><aside></aside></div></header>",
        600.0,
    );
    let blue = rect_of(&tree, &boxes, "b");
    assert_eq!((blue.x, blue.right()), (560.0, 600.0));
}

/// A nav link holding a positioned drop-down `width: max-content` wide: the
/// drop-down is as wide as its line of text, and the link as wide as its own
/// label, so the next link starts where the first ends.
#[test]
fn a_positioned_drop_down_does_not_widen_its_link() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } nav { display: flex } \
         a { position: relative } b { display: block; width: 80px } \
         ul { position: absolute; top: 100%; left: 0; width: max-content; margin: 0 }</style>\
         <nav><a><b>Platform</b><ul>A very long line of a drop-down menu</ul></a>\
         <a><b>Solutions</b></a></nav>",
        800.0,
    );
    let links = boxes_of(&tree, &boxes, "a");
    assert_eq!(links[0].rect.width, 80.0);
    assert_eq!(links[1].rect.x, links[0].rect.right());
    // Its words are in its own flow, not out of it: they are what it is as
    // wide as, all on one line as tall as the label's.
    let menu = rect_of(&tree, &boxes, "ul");
    let label = rect_of(&tree, &boxes, "b");
    assert_eq!(
        menu.height, label.height,
        "one line, not one word to a line: {menu:?}"
    );
}

/// At its widest a row of flex items is their contributions side by side,
/// whether or not the row may wrap (CSS Flexbox 1 §9.9.1). A `flex: none` nav
/// that wraps is sized by that, and taking the widest item instead put each
/// link on a line of its own.
#[test]
fn a_wrapping_row_at_its_widest_adds_its_items_up() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; justify-content: flex-end } \
         section { flex: 1 1 auto } \
         ul { display: flex; flex: none; flex-wrap: wrap; gap: 4px; margin: 0; padding: 0 } \
         li { display: block; width: 50px; margin: 0 3px }</style>\
         <main><section></section><ul><li></li><li></li><li></li><li></li></ul></main>",
        800.0,
    );
    let ul = rect_of(&tree, &boxes, "ul");
    assert_eq!(ul.width, 4.0 * 56.0 + 3.0 * 4.0, "{ul:?}");
    assert_eq!(ul.right(), 800.0);
    let items = boxes_of(&tree, &boxes, "li");
    assert_eq!(items.len(), 4);
    assert!(
        items.iter().all(|li| li.rect.y == items[0].rect.y),
        "all four on one line"
    );

    // A float shrink-wraps round the same sum.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         nav { float: left; display: flex; flex-wrap: wrap } \
         a { display: block; width: 50px }</style>\
         <nav><a></a><a></a><a></a></nav>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "nav").width, 150.0);
    let links = boxes_of(&tree, &boxes, "a");
    assert!(
        links.iter().all(|a| a.rect.y == links[0].rect.y),
        "one line"
    );
}

/// At its narrowest a single line is still all of its items, and a row that
/// may wrap needs only the widest of them (CSS Flexbox 1 §9.9.1). Floated in
/// a column 60px wide, the wrapping row shrinks to its widest item, 70px, and
/// the one that may not wrap stays all 150.
#[test]
fn a_wrapping_row_at_its_narrowest_is_its_widest_item() {
    for (wrap, width) in [("wrap", 70.0), ("wrap-reverse", 70.0), ("nowrap", 150.0)] {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} main {{ width: 60px }} \
                 nav {{ float: left; display: flex; flex-wrap: {wrap} }} \
                 a {{ display: block; height: 10px }}</style>\
                 <main><nav><a style='width:70px'></a><a style='width:50px'></a>\
                 <a style='width:30px'></a></nav></main>"
            ),
            800.0,
        );
        assert_eq!(rect_of(&tree, &boxes, "nav").width, width, "{wrap}");
    }
}

/// Down a column the width is the cross size, the widest of the items
/// (CSS Flexbox 1 §9.9.2). A floated item is not a float (§3), so the items
/// are not a run of floats side by side to be added up.
#[test]
fn a_column_is_as_wide_as_its_widest_item_even_when_they_say_float() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         aside { float: left } \
         nav { display: flex; flex-direction: column } \
         a { display: block; float: left; height: 10px }</style>\
         <aside><nav><a style='width:40px'></a><a style='width:50px'></a></nav></aside>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "aside").width, 50.0);
    assert_eq!(rect_of(&tree, &boxes, "nav").width, 50.0);
}

/// The widths of the `<section>` and `<aside>` items of one row, in order.
fn item_widths(html: &str) -> Vec<f32> {
    let (tree, boxes) = laid_out(
        &format!("<style>body {{ margin: 0 }} main {{ display: flex }}</style>{html}"),
        800.0,
    );
    let mut items: Vec<_> = boxes_of(&tree, &boxes, "section")
        .into_iter()
        .chain(boxes_of(&tree, &boxes, "aside"))
        .map(|item| item.rect)
        .collect();
    items.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    items.iter().map(|rect| rect.width).collect()
}

/// A truncated title beside a fixed button: `min-width: 0` and
/// `overflow: hidden` each take the automatic minimum away (CSS Flexbox §4.5),
/// so the title gives up what the row is missing; without either it is never
/// narrower than its line of text, which then overflows the row. Both
/// references: 150 either way, and the text's own width without.
#[test]
fn min_width_zero_or_overflow_lets_an_item_be_narrower_than_its_text() {
    let row = |rule: &str| {
        item_widths(&format!(
            "<main style='width:200px'><section style='flex:1;{rule}'>\
             <div style='white-space:nowrap'>a long unbreakable title here</div></section>\
             <aside style='width:50px;flex:none'></aside></main>"
        ))
    };
    assert_eq!(row("min-width:0"), vec![150.0, 50.0]);
    assert_eq!(row("overflow:hidden"), vec![150.0, 50.0]);

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <i style='float:left;white-space:nowrap;font-style:normal'>a long unbreakable title here</i>",
        800.0,
    );
    let text = rect_of(&tree, &boxes, "i").width;
    let auto = row("");
    assert!(auto[0] >= text, "{auto:?}, the text {text}");
    assert_eq!(auto[1], 50.0);
}

/// Shrinking is weighed by the base sizes (§9.7): a `width: max-content`
/// item far wider than the row beside a `flex: 1` item that starts at nothing
/// gives up all of the shortfall, down to the row, and the other is left with
/// nothing. Both references: 300 and 0.
#[test]
fn a_wide_item_gives_up_what_the_row_is_missing() {
    assert_eq!(
        item_widths(
            "<main style='width:300px'><section style='width:max-content'>\
             a few words that make a line well over three hundred pixels long</section>\
             <aside style='flex:1'></aside></main>"
        ),
        vec![300.0, 0.0]
    );

    // Three items shrinking into 400 give up a third, a sixth and half of
    // the 200 missing: in proportion to 300, 100 and 200.
    let widths = item_widths(
        "<main style='width:400px'><section style='width:300px'></section>\
         <aside style='flex:1 1 100px'></aside><section style='width:200px'></section></main>",
    );
    let expected = [200.0, 400.0 / 6.0, 400.0 / 3.0];
    assert!(
        widths.len() == 3
            && widths
                .iter()
                .zip(expected)
                .all(|(width, expected)| (width - expected).abs() < 0.01),
        "{widths:?}"
    );
}

/// An item held at its minimum is frozen there and the rest of the line is
/// shared out again (§9.7, step 5e): `min-width: min-content` keeps one of two
/// `flex: 1` items at its content, wider than the row, and the other gets
/// nothing. Both references: 150 and 0.
#[test]
fn an_item_held_at_its_minimum_leaves_the_other_nothing() {
    assert_eq!(
        item_widths(
            "<main style='width:100px'><section style='flex:1;min-width:min-content'>\
             <b style='display:block;width:150px'></b></section><aside style='flex:1'></aside></main>"
        ),
        vec![150.0, 0.0]
    );
}

/// Lines break on hypothetical sizes (§9.3, step 5): items at `flex: 1 1 0`
/// with `min-width: 200px` take two hundred each, so two fit in 500 and the
/// third wraps, and each line then grows into all of it.
#[test]
fn items_wrap_on_their_minimums_not_their_bases() {
    assert_eq!(
        item_widths(
            "<main style='width:500px;flex-wrap:wrap'>\
             <section style='flex:1 1 0;min-width:200px;height:10px'></section>\
             <section style='flex:1 1 0;min-width:200px;height:10px'></section>\
             <section style='flex:1 1 0;min-width:200px;height:10px'></section></main>"
        ),
        vec![250.0, 250.0, 500.0]
    );
}

/// `flex: 1 1 0` starts every item at nothing, but the automatic minimum
/// still holds each at its longest word: a row too narrow for two long words
/// overflows rather than cutting them.
#[test]
fn items_from_nothing_are_not_shrunk_past_their_words() {
    let words = ["Supercalifragilistic", "Antidisestablishment"];
    let widths = item_widths(&format!(
        "<main style='width:100px'><section style='flex:1 1 0'>{}</section>\
         <section style='flex:1 1 0'>{}</section></main>",
        words[0], words[1]
    ));
    for (word, width) in words.iter().zip(widths) {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }}</style>\
                 <i style='float:left;font-style:normal'>{word}</i>"
            ),
            800.0,
        );
        assert_eq!(width, rect_of(&tree, &boxes, "i").width, "{word}");
    }
}

/// A growing item stops at its maximum (§9.7, step 5d).
#[test]
fn a_growing_item_stops_at_its_maximum() {
    assert_eq!(
        item_widths("<main><section style='flex:1;max-width:300px;height:10px'></section></main>"),
        vec![300.0]
    );
}

/// Down a column the specified size suggestion is the item's `height` (§4.5):
/// an item `height: 300px` that holds fifty pixels may be shrunk to the
/// column's hundred. Both references: 100.
#[test]
fn a_column_item_with_a_height_shrinks_to_what_it_holds() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { display: flex; flex-direction: column; height: 100px; width: 100px }</style>\
         <main><section style='height:300px'><b style='display:block;height:50px'></b></section>\
         </main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 100.0);
}

/// A column with no height of its own is as tall as its items, held between
/// its `min-height` and `max-height` (§9.3, step 4), and `flex: 1` grows into
/// what a minimum leaves: the sticky footer. With the column as tall as its
/// items instead, there was nothing to grow into and the footer sat under the
/// header. Chrome: 510 and 560.
#[test]
fn flex_grow_fills_a_min_height_column_and_the_footer_sits_at_the_bottom() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;flex-direction:column;min-height:600px'>\
         <header style='height:50px'></header><main style='flex:1'>x</main>\
         <footer style='height:40px'>f</footer></div>",
        800.0,
    );
    let main = rect_of(&tree, &boxes, "main");
    assert_eq!((main.y, main.height), (50.0, 510.0));
    assert_eq!(rect_of(&tree, &boxes, "footer").y, 560.0);
    assert_eq!(rect_of(&tree, &boxes, "div").height, 600.0);
}

/// Shared out of a minimum, an item's height is not definite (§9.8): a
/// percentage inside it is of nothing, and `auto`. Chrome agrees.
#[test]
fn a_percentage_inside_an_item_of_a_min_height_column_is_auto() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;flex-direction:column;min-height:600px'>\
         <main style='flex:1'><p style='height:50%;margin:0'>x</p></main></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "main").height, 600.0);
    assert!(rect_of(&tree, &boxes, "p").height < 50.0);
}

/// An item's height as the container shared it out holds the item's own
/// contents too, definite or not: a `flex: 1` row inside a `min-height`
/// column is as tall as its share, and centres in it. That is the app shell
/// of every page that opens with `min-height: 100vh`. Chrome: 130.
#[test]
fn a_row_given_its_height_by_a_column_centres_in_all_of_it() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;flex-direction:column;min-height:300px'>\
         <main style='flex:1;display:flex;align-items:center'>\
         <b style='display:block;width:40px;height:40px'></b></main></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "main").height, 300.0);
    assert_eq!(rect_of(&tree, &boxes, "b").y, 130.0);
}

/// A row's single line is held between the container's `min-height` and
/// `max-height` (§9.4, step 8): the hero, whose item `align-items: center`
/// centres in the minimum. Chrome: 130.
#[test]
fn a_single_line_is_as_tall_as_the_row_minimum_and_centres_in_it() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;min-height:300px;align-items:center'>\
         <b style='display:block;height:40px'></b></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "b").y, 130.0);
    assert_eq!(rect_of(&tree, &boxes, "div").height, 300.0);
}

/// And a maximum holds the line down: a stretched item is the line, an item
/// with a height of its own overflows it. Chrome: 50 and 100.
#[test]
fn a_row_maximum_holds_its_line_down() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;max-height:50px'>\
         <b style='display:block;height:100px'>x</b><i>y</i></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").height, 50.0);
    assert_eq!(rect_of(&tree, &boxes, "b").height, 100.0);
    assert_eq!(rect_of(&tree, &boxes, "i").height, 50.0);
}

/// Down a column held down by its `max-height`, the items shrink into it:
/// three of 100 into 100.
#[test]
fn a_max_height_column_shrinks_its_items() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } b { display: block; height: 100px }</style>\
         <div style='display:flex;flex-direction:column;max-height:100px'>\
         <b></b><b></b><b></b></div>",
        800.0,
    );
    let items = boxes_of(&tree, &boxes, "b");
    assert_eq!(items.len(), 3);
    for item in &items {
        assert!((item.rect.height - 100.0 / 3.0).abs() < 0.01, "{item:?}");
    }
    assert_eq!(rect_of(&tree, &boxes, "div").height, 100.0);
}

/// A wrapping column breaks its lines at its `max-height`, and is as tall as
/// its longest line. Chrome: 60 tall, the second line 150 across.
#[test]
fn a_wrapping_column_breaks_at_its_max_height() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } b { display: block; width: 50px; height: 60px }</style>\
         <div style='display:flex;flex-direction:column;flex-wrap:wrap;max-height:100px;\
         width:300px'><b></b><b></b></div>",
        800.0,
    );
    let items = boxes_of(&tree, &boxes, "b");
    assert_eq!((items[1].rect.x, items[1].rect.y), (150.0, 0.0));
    assert_eq!(rect_of(&tree, &boxes, "div").height, 60.0);
}

/// A wrapped row held up by its `min-height` shares what its lines leave by
/// `align-content` (§9.4 step 15, §9.6 step 16): centred, the two 20px lines
/// start at 180. Chrome: 180.
#[test]
fn align_content_centres_lines_in_a_min_height() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } b { display: block; width: 200px; height: 20px }</style>\
         <div style='display:flex;flex-wrap:wrap;min-height:400px;width:300px;\
         align-content:center'><b></b><b></b></div>",
        800.0,
    );
    let lines: Vec<f32> = boxes_of(&tree, &boxes, "b")
        .iter()
        .map(|item| item.rect.y)
        .collect();
    assert_eq!(lines, vec![180.0, 200.0]);
}

/// A container's own percentage height is of its parent, not of itself, and
/// it is the line its items are aligned in: `flex-end` puts the item's bottom
/// at 200 of a 400px parent.
#[test]
fn a_percentage_height_container_aligns_its_items_in_that_height() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style><section style='height:400px'>\
         <div style='display:flex;height:50%;align-items:flex-end'>\
         <b style='display:block;height:40px'></b></div></section>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "b").bottom(), 200.0);
}

/// A stretched item's height is definite (§9.8): a `height: 100%` inside it is
/// the line's height, here the row's minimum. Chrome: 300.
#[test]
fn a_percentage_inside_a_stretched_item_is_of_the_line() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;min-height:300px'><section>\
         <p style='height:100%;margin:0'>x</p></section></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 300.0);
    assert_eq!(rect_of(&tree, &boxes, "p").height, 300.0);
}

/// Down a column, an item the line does not stretch is `fit-content` across
/// it (§9.4, step 7), so `align-items` has room to move it; the default
/// `stretch` still fills the line. Chrome: "Go" 20 wide at 190, the
/// `flex-end` item's right edge at 400.
#[test]
fn a_column_item_that_is_not_stretched_fits_its_content() {
    let column = |align: &str, item: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }}</style>\
                 <main style='display:flex;flex-direction:column;width:400px;{align}'>\
                 <div style='{item}'>Go</div></main>"
            ),
            800.0,
        );
        rect_of(&tree, &boxes, "div")
    };

    let centred = column("align-items:center", "");
    assert!(centred.width > 0.0 && centred.width < 60.0, "{centred:?}");
    assert!(
        (centred.x - (400.0 - centred.width) / 2.0).abs() < 0.01,
        "{centred:?}"
    );

    let end = column("", "align-self:flex-end");
    assert!(end.width < 60.0, "{end:?}");
    assert!((end.right() - 400.0).abs() < 0.01, "{end:?}");

    let stretched = column("", "");
    assert_eq!((stretched.x, stretched.width), (0.0, 400.0));

    // `baseline` has no baseline across a column to share, and is `start`.
    let baseline = column("align-items:baseline", "");
    assert!(baseline.width < 60.0 && baseline.x == 0.0, "{baseline:?}");
}

/// The limits a column is fitted into are its content box's: a border-box
/// `min-height` of 200 with 20px of padding leaves 160 to grow into.
#[test]
fn a_border_box_min_height_leaves_its_content_box_to_grow_into() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;flex-direction:column;min-height:200px;\
         box-sizing:border-box;padding:20px'><main style='flex:1'></main></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "main").height, 160.0);
    assert_eq!(rect_of(&tree, &boxes, "div").height, 200.0);
}

/// The border-box size of the `<section>` in a page laid out 800 wide.
fn section_in(html: &str) -> (f32, f32) {
    let (tree, boxes) = laid_out(&format!("<style>body {{ margin: 0 }}</style>{html}"), 800.0);
    let section = rect_of(&tree, &boxes, "section");
    (section.width, section.height)
}

/// Along a row, an item with a ratio and no height of its own is as tall as
/// its main size makes it (CSS Sizing 4 §4.2), which is definite (CSS Flexbox
/// §9.8) — on its own line, and as the tallest item a line is stretched to.
#[test]
fn a_row_item_is_as_tall_as_its_ratio_makes_its_width() {
    let item = "<section style='flex:0 0 200px;aspect-ratio:2'></section>";
    assert_eq!(
        section_in(&format!(
            "<div style='display:flex;align-items:start'>{item}</div>"
        )),
        (200.0, 100.0)
    );
    assert_eq!(
        section_in(&format!(
            "<div style='display:flex'>{item}<p style='height:50px;width:10px;margin:0'></p></div>"
        )),
        (200.0, 100.0)
    );
    assert_eq!(
        section_in(
            "<div style='display:flex;width:300px'><section style='flex:1;aspect-ratio:3'>\
             </section><p style='flex:1;margin:0'></p></div>"
        ),
        (150.0, 50.0)
    );
}

/// With a size across that is definite before the line is — a height of its
/// own, or a line it is stretched to in a single-line container of a set
/// height — an item's base size is that size through its ratio (§9.2.3 B), and
/// so is its automatic minimum (§4.5), unless it scrolls. Chrome's numbers.
#[test]
fn a_definite_size_across_gives_a_ratio_item_its_width() {
    let row = |container: &str, item: &str| {
        section_in(&format!(
            "<div style='display:flex;{container}'><section style='{item}'></section></div>"
        ))
    };
    assert_eq!(
        row("height:100px", "aspect-ratio:2"),
        (200.0, 100.0),
        "stretched"
    );
    assert_eq!(
        row("height:100px;align-items:start", "aspect-ratio:2"),
        (0.0, 0.0),
        "not stretched, nothing across to take a width from"
    );
    assert_eq!(
        row("width:100px", "height:100px;aspect-ratio:2"),
        (200.0, 100.0),
        "not shrunk below it"
    );
    assert_eq!(
        row("width:100px;height:100px", "aspect-ratio:2;flex:1 1 0"),
        (200.0, 100.0),
        "a basis of nothing grows to it"
    );
    assert_eq!(
        row("width:100px", "height:100px;aspect-ratio:2;overflow:hidden"),
        (100.0, 100.0),
        "a scroll container shrinks past it"
    );
    assert_eq!(
        row("width:500px", "aspect-ratio:1;min-height:300px"),
        (300.0, 300.0),
        "a minimum across carried over (CSS Sizing 4 §4.4)"
    );
}

/// Down a column, an item stretched across is as tall as its ratio makes its
/// width, or as its content where that is taller, and that is the least it
/// shrinks to (§9.2.3 B, §4.5); with a height of its own and not stretched,
/// it is as wide as the ratio makes that height. In a container that wraps
/// the width it is stretched to is not definite (§9.8): its height is taken
/// from the width that fits its content, and the line only widens it — as
/// Chrome and Firefox both draw it.
#[test]
fn a_column_item_takes_its_height_from_its_width_through_its_ratio() {
    let column = |container: &str, item: &str, inside: &str| {
        section_in(&format!(
            "<div style='display:flex;flex-direction:column;width:400px;{container}'>\
             <section style='{item}'>{inside}</section></div>"
        ))
    };
    assert_eq!(column("", "aspect-ratio:2", ""), (400.0, 200.0));
    assert_eq!(
        column("", "aspect-ratio:4", "<div style='height:300px'></div>"),
        (400.0, 300.0)
    );
    assert_eq!(
        column("height:100px", "aspect-ratio:2;flex:1", ""),
        (400.0, 200.0)
    );
    assert_eq!(
        column("height:100px", "aspect-ratio:2;flex:1;overflow:hidden", ""),
        (400.0, 100.0)
    );
    assert_eq!(
        column("align-items:start", "height:100px;aspect-ratio:2", ""),
        (200.0, 100.0)
    );
    let block = "<div style='width:40px;height:20px'></div>";
    assert_eq!(
        column("height:100px", "aspect-ratio:4", block),
        (400.0, 100.0),
        "a single line: stretched, and definite"
    );
    assert_eq!(
        column("height:100px;flex-wrap:wrap", "aspect-ratio:4", ""),
        (400.0, 0.0),
        "a line of a container that wraps is not definite"
    );
    assert_eq!(
        column("height:100px;flex-wrap:wrap", "aspect-ratio:4", block),
        (400.0, 20.0),
        "as tall as what it holds, forty across making ten"
    );
    assert_eq!(
        column("flex-wrap:wrap", "aspect-ratio:4", ""),
        (400.0, 0.0),
        "nor with no height of its own"
    );
}

/// A flex item's heights carried through its ratio onto its width (CSS Sizing
/// 4 §4.4) hold what its content asks for — its base size where that is its
/// content, its automatic minimum (CSS Flexbox §4.5), a width that fits its
/// content — and never the item: a line grows it past them, a basis the page
/// named ignores them, and `stretch` fills the line with it whatever they say.
/// Each size is what Chrome and Firefox draw.
#[test]
fn a_maximum_carried_through_a_ratio_holds_a_flex_items_content_not_the_item() {
    let square = "aspect-ratio:1;max-height:50px";
    let (tree, boxes) = laid_out(
        &format!(
            "<style>body {{ margin: 0 }}</style><div style='display:flex;width:300px'>\
             <section style='{square};flex:1'></section><p style='flex:1;height:10px;margin:0'>\
             </p></div>"
        ),
        800.0,
    );
    let section = rect_of(&tree, &boxes, "section");
    assert_eq!((section.width, section.height), (150.0, 50.0), "grown");
    assert_eq!(rect_of(&tree, &boxes, "p").x, 150.0);

    assert_eq!(
        section_in(&format!(
            "<div style='display:flex;width:400px'><section style='{square};flex:0 0 150px'>\
             </section></div>"
        )),
        (150.0, 50.0),
        "a basis the page named"
    );
    assert_eq!(
        section_in(&format!(
            "<div style='display:flex;flex-direction:column;width:400px'>\
             <section style='{square}'></section></div>"
        )),
        (400.0, 50.0),
        "stretched across a column"
    );

    let wide = "<span style='display:inline-block;width:150px;height:10px'></span>";
    assert_eq!(
        section_in(&format!(
            "<div style='display:flex;width:400px;align-items:start'>\
             <section style='{square}'>{wide}</section></div>"
        )),
        (50.0, 50.0),
        "a base size taken from content, and the minimum"
    );
    assert_eq!(
        section_in(&format!(
            "<div style='display:flex;flex-direction:column;width:400px;align-items:start'>\
             <section style='{square}'>{wide}</section></div>"
        )),
        (50.0, 50.0),
        "a width that fits the content, down a column"
    );
}

/// A picture stretched across a single-line column of a set width takes its
/// height from the width it is stretched to (CSS Flexbox §9.8, §9.2.3 B), and
/// keeps its shape: a two-to-one picture across three hundred pixels is a
/// hundred and fifty tall, as Chrome and Firefox draw it — not three hundred by
/// the hundred it came with. In a column that wraps it keeps the height it
/// came with, and the line widens it.
#[test]
fn a_picture_stretched_across_a_column_keeps_its_shape() {
    let drawn = |column: &str, size: (u32, u32)| {
        let (tree, boxes) = laid_out_with_image(
            &format!(
                "<style>body {{ margin: 0 }}</style>\
                 <div style='display:flex;flex-direction:column;width:300px;{column}'>\
                 <img src=a.png></div>"
            ),
            800.0,
            picture(size.0, size.1),
        );
        let img = rect_of(&tree, &boxes, "img");
        (img.width, img.height)
    };
    assert_eq!(drawn("", (200, 100)), (300.0, 150.0));
    assert_eq!(drawn("", (400, 200)), (300.0, 150.0));
    assert_eq!(
        drawn("align-items:start", (200, 100)),
        (200.0, 100.0),
        "not stretched: its own size"
    );
    assert_eq!(
        drawn("flex-wrap:wrap", (200, 100)),
        (300.0, 100.0),
        "a line of a container that wraps is not definite: widened, not taller"
    );
}

/// The three probes the foundation review left open, as Chrome and Firefox
/// both draw them: a picture in a row of a set height is as wide as the
/// height it ends up with makes it, whether that height is a percentage of
/// the row's, a percentage maximum, or the row it is stretched to.
#[test]
fn a_picture_in_a_row_of_a_set_height_keeps_its_shape() {
    let drawn = |row: &str, rule: &str, size: (u32, u32)| {
        let (tree, boxes) = laid_out_with_image(
            &format!(
                "<style>body {{ margin: 0 }}</style>\
                 <div style='display:flex;{row}'><img src=a.png style='{rule}'></div>"
            ),
            800.0,
            picture(size.0, size.1),
        );
        let img = rect_of(&tree, &boxes, "img");
        (img.width, img.height)
    };
    assert_eq!(
        drawn("height:48px", "height:100%", (200, 80)),
        (120.0, 48.0)
    );
    assert_eq!(
        drawn("height:60px", "max-height:50%", (100, 100)),
        (30.0, 30.0)
    );
    assert_eq!(drawn("height:60px", "", (400, 200)), (120.0, 60.0));
    assert_eq!(
        drawn("height:60px", "margin:5px 0", (400, 200)),
        (100.0, 50.0),
        "stretched to the line less its margins"
    );
    assert_eq!(
        drawn(
            "height:48px",
            "height:100%;padding:4px;box-sizing:border-box",
            (400, 200)
        ),
        (88.0, 48.0),
        "the ratio is the picture's, across its content box"
    );
    assert_eq!(
        drawn("height:60px;align-items:center", "", (400, 200)),
        (400.0, 200.0),
        "not stretched: its own size"
    );
    assert_eq!(
        drawn("height:60px;flex-wrap:wrap", "", (400, 200)),
        (400.0, 200.0),
        "a line of a container that wraps is not definite"
    );
}

/// A picture's `height` attribute is a height it named, as `height: 16px`
/// would be: the row does not stretch it, and it keeps its shape. Stretched,
/// it was drawn as tall as the row and as wide as sixteen pixels made it.
#[test]
fn a_height_attribute_keeps_a_picture_out_of_the_stretch() {
    let (tree, boxes) = laid_out_with_image(
        "<style>body { margin: 0 }</style>\
         <div style='display:flex;height:40px'><img src=a.png height=16></div>",
        800.0,
        picture(400, 200),
    );
    let img = rect_of(&tree, &boxes, "img");
    assert_eq!((img.width, img.height), (32.0, 16.0));
}
