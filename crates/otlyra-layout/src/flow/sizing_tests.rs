//! The sizing properties, checked against whole documents: `calc()` with a
//! percentage in it, the content keywords, `min-*: auto`, and `box-sizing` in
//! every formatting context that sizes a box.

use super::tests::{boxes_of, image_rect, laid_out, laid_out_with_image, picture, rect_of};
use crate::{BoxTree, FragmentKind, FragmentTree};

/// Whether two widths are the same to within the rounding a shaper leaves.
fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

/// A `calc()` that mixes a percentage with a length is resolved against the
/// containing block, not folded to what it would be against nothing — which
/// made `calc(100% - 20px)` a width of minus twenty, and so of nothing.
#[test]
fn a_calc_width_keeps_its_percentage() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } div { width: calc(100% - 20px) }</style><div>x</div>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").width, 380.0);

    // An `em` in it is the cascade's, the percentage layout's.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { font-size: 10px; width: calc(50% + 2em) }</style><div>x</div>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").width, 220.0);
}

/// `min()` is whichever side is smaller *at this width*, which is what keeps
/// a card at three hundred pixels on a wide page and at the page on a narrow
/// one.
#[test]
fn a_maximum_of_min_is_the_smaller_side_at_this_width() {
    let page = "<style>body { margin: 0 } \
                div { max-width: min(100%, 300px); margin: 0 auto }</style><div>x</div>";

    let (tree, boxes) = laid_out(page, 800.0);
    let wide = rect_of(&tree, &boxes, "div");
    assert_eq!(
        (wide.x, wide.width),
        (250.0, 300.0),
        "held to 300 and centred"
    );

    let (tree, boxes) = laid_out(page, 200.0);
    assert_eq!(
        rect_of(&tree, &boxes, "div").width,
        200.0,
        "held to the page"
    );

    // And `clamp()` in a minimum.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { width: 10px; min-width: clamp(50px, 25%, 150px) }</style><div>x</div>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").width, 100.0);
}

/// Margins, padding and insets go through the same door as widths.
#[test]
fn margins_padding_and_insets_resolve_a_calc() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { margin-left: calc(10% + 5px); padding-left: calc(5% - 2px); height: 10px }\
         </style><div></div>",
        400.0,
    );
    let div = rect_of(&tree, &boxes, "div");
    assert_eq!(div.x, 45.0, "ten per cent of 400 and five");
    assert_eq!(
        div.width,
        400.0 - 45.0,
        "the padding is inside the box and the margin outside"
    );
    let used = boxes_of(&tree, &boxes, "div")[0]
        .used
        .expect("a block carries the edges it was given");
    assert_eq!(used.padding.left, 18.0, "five per cent of 400 less two");

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { position: relative; width: 400px; height: 200px } \
         nav { position: absolute; left: calc(50% - 10px); top: calc(100% - 30px); \
         width: 20px; height: 20px }</style><main><nav></nav></main>",
        800.0,
    );
    let nav = rect_of(&tree, &boxes, "nav");
    assert_eq!((nav.x, nav.y), (190.0, 170.0));
}

/// A percentage height is of a height: against a containing block that has
/// one, a `calc()` with a percentage in it is resolved like a bare percentage
/// is, and against one as tall as its contents it is `auto` (CSS 2.2 §10.5).
#[test]
fn a_calc_height_needs_a_containing_block_with_a_height() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { height: 200px } \
         section { height: calc(50% + 10px) }</style><main><section>x</section></main>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 110.0);

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         section { height: calc(50% + 10px) }</style><main><section>x</section></main>",
        400.0,
    );
    let section = rect_of(&tree, &boxes, "section");
    assert!(
        section.height > 0.0 && section.height < 40.0,
        "one line of text, not ten pixels and not half the page: {section:?}"
    );

    // `min()` too: the smaller of half of two hundred and sixty pixels.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { height: 200px } \
         section { height: min(50%, 60px) }</style><main><section>x</section></main>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 60.0);
}

/// The content keywords (CSS Sizing 3 §3.2) are sizes the box's own content
/// answers: its longest word, its unbroken line, and the one of those that the
/// room there is lands between.
#[test]
fn the_content_keywords_size_a_block_by_its_content() {
    // What each keyword should come to, measured by boxes that already shrink
    // to fit: a float holding only the longest word, and a float holding the
    // whole phrase with room to spare.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } i { float: left; font-style: normal }</style>\
         <i>alphabetical</i><b style='float: left; font-weight: normal'>a bb alphabetical c</b>",
        800.0,
    );
    let longest_word = rect_of(&tree, &boxes, "i").width;
    let whole_line = rect_of(&tree, &boxes, "b").width;
    assert!(longest_word < whole_line);

    let width_of = |keyword: &str, page: f32| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} div {{ width: {keyword} }}</style>\
                 <div>a bb alphabetical c</div>"
            ),
            page,
        );
        rect_of(&tree, &boxes, "div").width
    };

    assert!(near(width_of("min-content", 800.0), longest_word));
    assert!(near(width_of("max-content", 800.0), whole_line));
    assert!(
        near(width_of("max-content", 50.0), whole_line),
        "max-content does not wrap however narrow the page"
    );
    // `fit-content` is the unbroken line where there is room for it, the page
    // where there is not, and the longest word where not even that fits.
    assert!(near(width_of("fit-content", 800.0), whole_line));
    let between = (longest_word + whole_line) / 2.0;
    assert!(near(width_of("fit-content", between), between));
    assert!(near(width_of("fit-content", 10.0), longest_word));
    // And `fit-content(<length>)` fits into the length it names instead.
    assert!(near(
        width_of(&format!("fit-content({between}px)"), 800.0),
        between
    ));
}

/// `width: fit-content` with `margin: 0 auto` is how a badge or a button is
/// centred without a width of its own: it is as wide as what it says, and the
/// margins share out the rest.
#[test]
fn fit_content_and_auto_margins_centre_what_it_holds() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { width: fit-content; margin: 0 auto; padding: 0 10px }</style><div>Hi</div>",
        400.0,
    );
    let div = rect_of(&tree, &boxes, "div");
    assert!(
        div.width < 60.0,
        "as wide as a word and its padding: {div:?}"
    );
    assert!(
        near(div.x, (400.0 - div.width) / 2.0),
        "and in the middle: {div:?}"
    );
}

/// A keyword is a limit as much as a size: `max-width: max-content` holds a
/// block to its text, and `min-width: max-content` holds it open past its
/// container.
#[test]
fn the_content_keywords_are_limits_too() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } i { float: left; font-style: normal }</style>\
         <i>a bb alphabetical c</i>",
        800.0,
    );
    let whole_line = rect_of(&tree, &boxes, "i").width;

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } div { max-width: max-content }</style>\
         <div>a bb alphabetical c</div>",
        800.0,
    );
    assert!(near(rect_of(&tree, &boxes, "div").width, whole_line));

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { width: 50px } div { min-width: max-content }</style>\
         <main><div>a bb alphabetical c</div></main>",
        800.0,
    );
    assert!(near(rect_of(&tree, &boxes, "div").width, whole_line));
}

/// Down the block axis the keywords are the content's own height, which is
/// what `auto` already is.
#[test]
fn a_keyword_height_is_the_height_of_the_content() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { height: 300px } \
         section { height: max-content } article { height: auto }</style>\
         <main><section>x</section><article>x</article></main>",
        400.0,
    );
    assert_eq!(
        rect_of(&tree, &boxes, "section").height,
        rect_of(&tree, &boxes, "article").height
    );
}

/// `box-sizing: border-box` measures a minimum and a maximum across the border
/// box just as it does a width, so a column held to 1200 pixels with padding on
/// it is 1200 pixels, not 1232.
#[test]
fn a_border_box_holds_its_limits_across_its_edges() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { box-sizing: border-box; max-width: 1200px; padding: 0 16px; margin: 0 auto }\
         </style><div>x</div>",
        1400.0,
    );
    let column = rect_of(&tree, &boxes, "div");
    assert_eq!((column.x, column.width), (100.0, 1200.0));

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { box-sizing: border-box; min-width: 300px; width: 10px; padding: 20px }\
         </style><div>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").width, 300.0);

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { box-sizing: border-box; min-height: 500px; padding: 40px }</style><div>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").height, 500.0);

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { box-sizing: border-box; max-height: 100px; padding: 20px }</style>\
         <div>x<br>x<br>x<br>x<br>x<br>x<br>x<br>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").height, 100.0);
}

/// An absolutely positioned box measures its width the way `box-sizing` says:
/// the whole of its containing block, padding and all, when it asks for all of
/// it across its border box.
#[test]
fn a_positioned_border_box_is_the_width_it_asks_for() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { position: relative; width: 300px; height: 100px } \
         nav { position: absolute; box-sizing: border-box; width: 100%; padding: 10px; \
         border: 2px solid }</style><main><nav>x</nav></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "nav").width, 300.0);

    // And its limits hold a box that shrinks to fit, which they did not at all.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { position: relative; width: 600px; height: 100px } \
         nav { position: absolute; max-width: 100px }</style>\
         <main><nav>a phrase long enough to want more than a hundred pixels</nav></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "nav").width, 100.0);
}

/// Two inline blocks at half the line each, with padding measured inside that
/// half, share the line rather than wrapping onto two.
#[test]
fn two_border_box_halves_share_a_line() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         span { display: inline-block; box-sizing: border-box; width: 50%; padding: 10px }\
         </style><main><span>a</span><span>b</span></main>",
        400.0,
    );
    let halves = boxes_of(&tree, &boxes, "span");
    assert_eq!(halves.len(), 2);
    assert_eq!(halves[0].rect.width, 200.0);
    assert_eq!(
        halves[0].rect.y, halves[1].rect.y,
        "on the same line: {halves:?}"
    );
}

/// A percentage height inside a box with padding is of the box's content
/// height, which is the height it asked for — not that less its padding again.
#[test]
fn a_percentage_height_is_of_the_content_box() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { height: 300px; padding: 20px } \
         section { height: 100% }</style><main><section>x</section></main>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 300.0);

    // Across a border box the content height is what is left inside the edges.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { box-sizing: border-box; height: 300px; padding: 20px } \
         section { height: 100% }</style><main><section>x</section></main>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").height, 260.0);
}

/// `flex-basis` is measured the way `width` is: across the content box unless
/// `box-sizing` says otherwise, so a content-box item with padding starts that
/// much wider than its basis.
#[test]
fn a_flex_basis_is_measured_as_a_width_is() {
    let basis_of = |sizing: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} main {{ display: flex }} \
                 section {{ flex: 0 0 200px; padding: 0 20px; box-sizing: {sizing} }}</style>\
                 <main><section>x</section></main>"
            ),
            800.0,
        );
        rect_of(&tree, &boxes, "section").width
    };
    assert_eq!(basis_of("content-box"), 240.0);
    assert_eq!(basis_of("border-box"), 200.0);
}

/// Half a row less half the gap is what a two-column card grid writes, and it
/// puts two cards on each row — which a basis folded to minus eight did not.
#[test]
fn a_calc_basis_puts_two_items_on_a_row() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { display: flex; flex-wrap: wrap; gap: 16px; width: 400px } \
         section { flex: 0 0 calc(50% - 8px); height: 20px }</style>\
         <main><section></section><section></section><section></section></main>",
        800.0,
    );
    let cards = boxes_of(&tree, &boxes, "section");
    assert_eq!(cards.len(), 3);
    assert!(
        cards.iter().all(|card| card.rect.width == 192.0),
        "{cards:?}"
    );
    assert_eq!(cards[0].rect.y, cards[1].rect.y, "two on the first row");
    assert_eq!(cards[1].rect.x, 208.0, "past the first and the gap");
    assert!(
        cards[2].rect.y > cards[0].rect.y,
        "and the third below them"
    );
}

/// `min-width: auto` on a flex item is its automatic minimum and `min-width: 0`
/// turns it off: the two are different values now, and the second lets an item
/// shrink below its longest word, which is what a page writes it for.
#[test]
fn min_width_zero_lets_a_flex_item_shrink_past_its_content() {
    let item_width = |minimum: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} main {{ display: flex; width: 50px }} \
                 section {{ flex: 0 1 300px; min-width: {minimum} }}</style>\
                 <main><section>unbreakablesupercalifragilistic</section></main>"
            ),
            800.0,
        );
        rect_of(&tree, &boxes, "section").width
    };
    let automatic = item_width("auto");
    assert!(automatic > 50.0, "floored at its word: {automatic}");
    assert_eq!(item_width("0"), 50.0, "the row, word or no word");
}

/// While a float's width is being worked out, a percentage width inside it is
/// a percentage of the very thing being measured, so it counts as `auto`
/// (CSS Sizing 3 §5.2.1) — and then takes its share of what the float came to.
#[test]
fn a_percentage_inside_a_shrinking_box_is_cyclic() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { float: left } \
         section { width: 50% }</style>\
         <main><section>a bb alphabetical c</section></main>\
         <i style='float: left; font-style: normal'>a bb alphabetical c</i>",
        800.0,
    );
    let whole_line = rect_of(&tree, &boxes, "i").width;
    let float = rect_of(&tree, &boxes, "main");
    assert!(
        near(float.width, whole_line),
        "the float is as wide as the text, not as half the page: {float:?}"
    );
    assert!(near(
        rect_of(&tree, &boxes, "section").width,
        whole_line / 2.0
    ));
}

/// The lines of a box sized by a keyword are laid out at that width.
#[test]
fn a_min_content_box_breaks_at_every_opportunity() {
    let (tree, _) = laid_out(
        "<style>body { margin: 0 } div { width: min-content }</style>\
         <div>xxxx xxxx xxxx</div>",
        800.0,
    );
    let lines = tree
        .iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .count();
    assert_eq!(lines, 3, "one word to a line");
}

/// A grid's `auto` column is stretched to fill the container in CSS, which is
/// what gives an item at `width: 100%` the whole of it; until that step is
/// taken here the container's width stands in for the item's area, so the one
/// column of a search box is the width of the box rather than of its
/// placeholder.
#[test]
fn a_full_width_grid_item_fills_its_one_auto_column() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: grid; width: 400px } \
         section { width: 100% }</style><main><section>short</section></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").width, 400.0);
}

/// A page whose every picture is 800 by 400, laid out 800 wide.
fn with_a_wide_picture(html: &str) -> (FragmentTree, BoxTree) {
    laid_out_with_image(
        &format!("<style>body {{ margin: 0 }} section {{ width: 400px }}</style>{html}"),
        800.0,
        picture(800, 400),
    )
}

/// `img { max-width: 100% }` — which nearly every stylesheet opens with — in a
/// box that shrinks to fit what it holds. The picture's percentage is of the
/// width being measured, and it asks for nothing at its narrowest (CSS Sizing
/// 3 §5.2.2) and for no more than that width at its widest; so each of these
/// boxes stays inside the 400 pixels it is in, and the picture in it keeps its
/// shape. Measured as the picture's natural 800, every one of them came out 800
/// wide.
#[test]
fn a_picture_at_max_width_100_percent_stays_inside_what_holds_it() {
    let picture = "<img src=a.png style=\"max-width: 100%; display: block\">";
    let holders = [
        (
            "float",
            format!("<section><div style=\"float: left\">{picture}</div></section>"),
        ),
        (
            "inline-block",
            format!("<section><div style=\"display: inline-block\">{picture}</div></section>"),
        ),
        (
            "positioned",
            format!(
                "<section style=\"position: relative; height: 300px\">\
                 <div style=\"position: absolute; left: 0; top: 0\">{picture}</div></section>"
            ),
        ),
        (
            "one-column grid",
            format!("<section style=\"display: grid\"><div>{picture}</div></section>"),
        ),
    ];
    for (holder, html) in holders {
        let (tree, boxes) = with_a_wide_picture(&html);
        assert_eq!(rect_of(&tree, &boxes, "div").width, 400.0, "the {holder}");
        let drawn = image_rect(&tree);
        assert_eq!((drawn.width, drawn.height), (400.0, 200.0), "the {holder}");
    }

    // A table shares its width out between the picture's column and the one
    // beside it, rather than the picture pushing the other off the page.
    let (tree, boxes) = with_a_wide_picture(&format!(
        "<section><table style=\"border-spacing: 0\"><tr><td style=\"padding: 0\">{picture}\
         </td><td style=\"padding: 0\">words beside it</td></tr></table></section>"
    ));
    let table = rect_of(&tree, &boxes, "table");
    assert!(table.width <= 400.0, "the table: {table:?}");
    let cells = boxes_of(&tree, &boxes, "td");
    assert!(
        cells[1].rect.x + cells[1].rect.width <= 400.0,
        "the words: {:?}",
        cells[1].rect
    );
    assert!(
        cells[0].rect.width > 200.0,
        "the picture's column: {:?}",
        cells[0].rect
    );
}

/// A picture on a line and a picture on a line of its own ask the same of the
/// box they are in: at its narrowest, a box holding a picture at `max-width:
/// 100%` holds nothing it cannot give up.
#[test]
fn a_picture_on_a_line_is_measured_as_one_on_its_own() {
    for display in ["block", "inline"] {
        let (tree, boxes) = with_a_wide_picture(&format!(
            "<section><div style=\"width: min-content\">\
             <img src=a.png style=\"max-width: 100%; display: {display}\"></div></section>"
        ));
        assert_eq!(
            rect_of(&tree, &boxes, "div").width,
            0.0,
            "display: {display}"
        );

        let (tree, boxes) = with_a_wide_picture(&format!(
            "<section><div style=\"float: left\">\
             <img src=a.png style=\"max-width: 100%; display: {display}\"></div></section>"
        ));
        assert_eq!(
            rect_of(&tree, &boxes, "div").width,
            400.0,
            "display: {display}"
        );
    }
}

/// A limit on one side of a picture whose `width` and `height` are both `auto`
/// is carried to the other through its ratio (CSS 2.2 §10.4), and a width that
/// is held back takes its height with it (§10.6.2): a picture is narrowed or
/// shortened, never squashed.
#[test]
fn a_limit_narrows_a_picture_without_squashing_it() {
    let drawn = |rule: &str| {
        let (tree, _) = with_a_wide_picture(&format!(
            "<style>img {{ display: block; {rule} }}</style><section><img src=a.png></section>"
        ));
        let rect = image_rect(&tree);
        (rect.width, rect.height)
    };
    assert_eq!(drawn("max-width: 100%"), (400.0, 200.0));
    assert_eq!(drawn("width: 100%; max-width: 300px"), (300.0, 150.0));
    assert_eq!(drawn("max-height: 100px"), (200.0, 100.0));
    assert_eq!(drawn("min-width: 1000px"), (1000.0, 500.0));
    // Both too large: the one with further to go decides.
    assert_eq!(drawn("max-width: 400px; max-height: 100px"), (200.0, 100.0));
    // And where following the ratio would break the other limit, the limit
    // wins and the shape gives.
    assert_eq!(drawn("max-width: 400px; min-height: 300px"), (400.0, 300.0));
}

/// A form control's content keywords are its natural size (CSS Sizing 3 §5.1),
/// as a size and as a limit: `w-fit`, `w-max` and `w-min` on a field leave it
/// the field it was, and do not shrink it to the text inside it — or, for a
/// checkbox, which holds no text at all, to nothing.
#[test]
fn a_control_keeps_its_natural_size_under_every_content_keyword() {
    let size_of = |tag: &str, html: &str, rule: &str| {
        let (tree, boxes) = laid_out(
            &format!("<style>body {{ margin: 0 }} {tag} {{ {rule} }}</style>{html}"),
            800.0,
        );
        let rect = rect_of(&tree, &boxes, tag);
        (rect.width, rect.height)
    };
    let same = |a: (f32, f32), b: (f32, f32)| near(a.0, b.0) && near(a.1, b.1);

    let widths = [
        "width: min-content",
        "width: max-content",
        "width: fit-content",
        "width: fit-content(10px)",
        "max-width: min-content",
        "max-width: max-content",
        "min-width: max-content",
    ];
    for (tag, html) in [
        ("input", "<input value=\"a few words typed\">"),
        ("input", "<input type=checkbox>"),
        ("input", "<input type=range>"),
        ("textarea", "<textarea>abc</textarea>"),
    ] {
        let natural = size_of(tag, html, "");
        assert!(
            natural.0 > 10.0,
            "{html} has a width of its own: {natural:?}"
        );
        for rule in widths {
            let sized = size_of(tag, html, rule);
            assert!(
                same(sized, natural),
                "{html} with {rule}: {sized:?}, not {natural:?}"
            );
        }
    }

    // Down the block axis too: a text area is as many rows tall as it asks for,
    // whatever is typed into it.
    let natural = size_of("textarea", "<textarea>abc</textarea>", "");
    for rule in [
        "height: min-content",
        "height: max-content",
        "height: fit-content",
        "max-height: min-content",
        "min-height: max-content",
    ] {
        let sized = size_of("textarea", "<textarea>abc</textarea>", rule);
        assert!(same(sized, natural), "{rule}: {sized:?}, not {natural:?}");
    }
    let natural = size_of("input", "<input type=checkbox>", "");
    let sized = size_of("input", "<input type=checkbox>", "height: max-content");
    assert!(
        same(sized, natural),
        "a checkbox: {sized:?}, not {natural:?}"
    );
}

/// A positioned box's percentage height is of the box it is placed against,
/// whose padding box has a height of its own here — not of the box it happens
/// to sit in, which does not (CSS 2.2 §10.5, §10.1).
#[test]
fn a_positioned_percentage_height_is_of_its_containing_block() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { position: relative; height: 80px; padding: 10px } \
         nav { position: absolute; top: 0; width: 10px; height: 50% }</style>\
         <main><div><nav></nav></div></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "nav").height, 50.0);
}

/// A field with a percentage width is measured as a picture is (CSS Sizing 3
/// §5.2.2): at its widest it asks for its natural size and at its narrowest for
/// nothing but its frame. So a float holding `input { width: 100% }` is as wide
/// as the field rather than the page, and a box as narrow as its content can be
/// is as narrow as the field's frame.
#[test]
fn a_field_at_width_100_percent_is_compressible() {
    let width_of = |html: &str, tag: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} input {{ padding: 1px; border: 2px solid }}</style>\
                 {html}"
            ),
            800.0,
        );
        rect_of(&tree, &boxes, tag).width
    };
    let natural = width_of("<input>", "input");

    let float = width_of(
        "<div style=\"float: left\"><input style=\"width: 100%\"></div>",
        "div",
    );
    assert!(near(float, natural), "{float}, not {natural}");

    let narrowest = width_of(
        "<div style=\"width: min-content\"><input style=\"width: 100%\"></div>",
        "div",
    );
    assert_eq!(narrowest, 6.0, "the frame alone");
}

/// An inline block on a line being measured asks what its own content asks: at
/// its narrowest it is as narrow as its longest word, not as wide as it would
/// be laid out on a line of its own.
#[test]
fn an_inline_block_is_measured_by_what_it_contributes() {
    let width_of = |content: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} div {{ width: min-content }}</style>\
                 <div>{content}</div>"
            ),
            800.0,
        );
        rect_of(&tree, &boxes, "div").width
    };
    let words = width_of("two words");
    let boxed = width_of("<span style=\"display: inline-block\">two words</span>");
    assert!(near(boxed, words), "{boxed}, not {words}");
}

/// A picture's min-content size is its own width (CSS Sizing 3 §5.1), and a
/// flex item may not be shrunk past that unless its maximum says less (CSS
/// Flexbox §4.5). `img { max-width: 100% }` compresses the picture's
/// *contribution* to a column (§5.2.2), not the picture: as a flex item beside
/// one that will not shrink, it stays as wide as the row, where both
/// references put it, rather than being squeezed to the leftover hundred.
#[test]
fn a_picture_in_a_row_is_not_shrunk_past_its_own_width() {
    let drawn = |picture_rule: &str| {
        let (tree, boxes) = with_a_wide_picture(&format!(
            "<style>img {{ max-width: 100%; {picture_rule} }} \
             main {{ display: flex; width: 400px }} \
             p {{ flex: 0 0 300px; margin: 0 }}</style>\
             <main><img src=a.png><p>fixed</p></main>"
        ));
        let img = rect_of(&tree, &boxes, "img");
        let p = rect_of(&tree, &boxes, "p");
        (img.width, img.height, p.x)
    };
    assert_eq!(drawn(""), (400.0, 200.0, 400.0), "max-width: 100%");
    assert_eq!(drawn("width: 100%"), (400.0, 200.0, 400.0), "width: 100%");

    // A `width` attribute is the width the picture asks for, and the floor.
    let (tree, boxes) = with_a_wide_picture(
        "<style>main { display: flex; width: 400px } p { flex: 0 0 300px; margin: 0 }</style>\
         <main><img src=a.png width=200><p>fixed</p></main>",
    );
    let img = rect_of(&tree, &boxes, "img");
    assert_eq!((img.width, img.height), (200.0, 100.0));
}

/// The media object: a picture beside a heading too long for the row. The
/// picture keeps its size and its shape — and keeps its height where nothing
/// stretches it, rather than being measured at a height of nothing. How much
/// of the shortfall the heading then takes is the flex shrink loop's business
/// (CSS Flexbox §9.7), which is not what is being checked.
#[test]
fn a_picture_beside_long_text_keeps_its_size() {
    for align in ["stretch", "flex-start", "center"] {
        let (tree, boxes) = laid_out_with_image(
            &format!(
                "<style>body {{ margin: 0 }} img {{ max-width: 100% }} \
                 main {{ display: flex; width: 400px; align-items: {align} }} \
                 h1 {{ margin: 0; font-size: 14px }}</style>\
                 <main><img src=a.png><h1>A headline for the card that is long enough \
                 to want five hundred pixels of room</h1></main>"
            ),
            800.0,
            picture(200, 100),
        );
        let img = rect_of(&tree, &boxes, "img");
        let h1 = rect_of(&tree, &boxes, "h1");
        assert_eq!(img.width, 200.0, "align-items: {align}");
        assert_eq!(h1.x, 200.0, "align-items: {align}");
        if align != "stretch" {
            assert_eq!(img.height, 100.0, "align-items: {align}");
        }
    }
}

/// A picture floated or positioned at `max-width: 50%` is half of its
/// containing block, and its height follows that width through its ratio —
/// not a width worked out again as half of the half it already is (both
/// references: 204 by 104 with the border).
#[test]
fn a_shrunk_picture_takes_its_height_from_its_width() {
    for holder in [
        "<section><img src=a.png style=\"float: left\"></section>",
        "<section style=\"position: relative; height: 300px\">\
         <img src=a.png style=\"position: absolute; left: 0; top: 0\"></section>",
    ] {
        let (tree, boxes) = laid_out_with_image(
            &format!(
                "<style>body {{ margin: 0 }} section {{ width: 400px }} \
                 img {{ max-width: 50%; border: 2px solid }}</style>{holder}"
            ),
            800.0,
            picture(400, 200),
        );
        let img = rect_of(&tree, &boxes, "img");
        assert_eq!((img.width, img.height), (204.0, 104.0), "{holder}");
    }

    // Between two insets a picture is still its own width (CSS 2.2 §10.3.8),
    // not stretched between them.
    let (tree, boxes) = with_a_wide_picture(
        "<section style=\"position: relative; height: 300px\">\
         <img src=a.png style=\"position: absolute; left: 0; right: 0; top: 0\"></section>",
    );
    let img = rect_of(&tree, &boxes, "img");
    assert_eq!((img.width, img.height), (800.0, 400.0));
}

/// Down a column with a height of its own, an item's height is what the
/// sharing out left it, and `min-height: 0` lets that be less than its
/// content: the content overflows the item — and is cut off, where `overflow`
/// says so — rather than pushing the item past the next one. Both references:
/// eighty and twenty, the second item at eighty.
#[test]
fn a_column_item_is_the_height_it_was_given() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { display: flex; flex-direction: column; height: 100px; width: 200px } \
         section { flex: 1; min-height: 0; overflow: hidden } \
         aside { height: 20px; flex: none }</style>\
         <main><section><div style=\"height: 200px\">x</div></section><aside></aside></main>",
        800.0,
    );
    let section = rect_of(&tree, &boxes, "section");
    let aside = rect_of(&tree, &boxes, "aside");
    assert_eq!((section.y, section.height), (0.0, 80.0));
    assert_eq!((aside.y, aside.height), (80.0, 20.0));

    // What overflows is cut off at the item and scrolls there.
    let port = tree
        .scroll_ports
        .iter()
        .find(|port| boxes.node(port.id).tag.as_deref() == Some("section"))
        .expect("the item is a scroll port");
    assert_eq!(port.port.height, 80.0);
    assert_eq!(port.content_height, 200.0);
    assert_eq!(
        tree.scroll_ports.len(),
        1,
        "and nothing else is, the item as it was measured included"
    );

    // A percentage inside it is of that height.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { display: flex; flex-direction: column; height: 100px } \
         section { flex: 1; min-height: 0 } aside { height: 20px; flex: none } \
         div { height: 100% }</style>\
         <main><section><div></div></section><aside></aside></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").height, 80.0);
}

/// A column whose container has no height of its own has none to share out:
/// an item at `flex: 1 1 0px` with its floor removed is nothing tall, and one
/// at `flex: 1` — a percentage basis of a height nobody knows, which is its
/// content — is as tall as what it holds.
#[test]
fn a_column_without_a_height_has_nothing_to_share() {
    let heights = |flex: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} \
                 main {{ display: flex; flex-direction: column }} \
                 section {{ flex: {flex}; min-height: 0 }}</style>\
                 <main><section><div style=\"height: 200px\"></div></section></main>"
            ),
            800.0,
        );
        (
            rect_of(&tree, &boxes, "main").height,
            rect_of(&tree, &boxes, "section").height,
        )
    };
    assert_eq!(heights("1 1 0px"), (0.0, 0.0));
    assert_eq!(heights("1"), (200.0, 200.0));
}

/// The one line of a container that cannot wrap and has a height of its own is
/// that tall (CSS Flexbox §9.4, step 8): an item taller than it overflows it,
/// is centred against it, and a stretched item is the line's height held
/// between its own limits rather than its content's height.
#[test]
fn a_row_with_a_height_of_its_own_is_one_line_that_tall() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; height: 64px; width: 300px } \
         section, article { width: 50px } div { height: 100px }</style>\
         <main style=\"align-items: center\"><section style=\"height: 100px\"></section>\
         <article><div></div></article></main>",
        800.0,
    );
    let section = rect_of(&tree, &boxes, "section");
    let article = rect_of(&tree, &boxes, "article");
    assert_eq!((section.y, section.height), (-18.0, 100.0));
    assert_eq!((article.y, article.height), (-18.0, 100.0));

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; height: 64px; width: 300px } \
         section, article { width: 50px } div { height: 100px }</style>\
         <main><section><div></div></section><article style=\"max-height: 30px\"></article></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "main").height, 64.0);
    assert_eq!(rect_of(&tree, &boxes, "section").height, 64.0);
    assert_eq!(rect_of(&tree, &boxes, "article").height, 30.0);
}

/// A stretched item's height is definite (CSS Flexbox §9.8): a percentage
/// inside it is of the line, even in a row as tall as its tallest item.
#[test]
fn a_percentage_inside_a_stretched_item_is_of_the_line() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; width: 300px } \
         section, article { width: 50px }</style>\
         <main><section><div style=\"height: 80px\"></div></section>\
         <article><p style=\"height: 50%; margin: 0\"></p></article></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "article").height, 80.0);
    assert_eq!(rect_of(&tree, &boxes, "p").height, 40.0);
}

/// Stretched across a column, an item is held to its own maximum: a card at
/// `max-width: 200px` in a wider column is 200 wide.
#[test]
fn a_column_item_is_stretched_no_wider_than_its_maximum() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; flex-direction: column; width: 300px } \
         section { max-width: 200px }</style><main><section>x</section></main>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "section").width, 200.0);
}

/// The application shell: a header, a pane that takes what is left of a
/// window-tall column and is told it may be shorter than its content, and a
/// footer. The pane is a row whose list scrolls, and it ends where the footer
/// starts rather than running on under it (both references: the pane 310 at
/// 50, the list 310, the footer at 360).
#[test]
fn an_application_shell_keeps_its_panes_between_header_and_footer() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         main { display: flex; flex-direction: column; height: 400px } \
         header { height: 50px; flex: none } \
         section { flex: 1; min-height: 0; display: flex } \
         nav { width: 200px; overflow: auto } article { flex: 1 } \
         footer { height: 40px; flex: none }</style>\
         <main><header></header><section><nav><div style=\"height: 3000px\"></div></nav>\
         <article></article></section><footer></footer></main><p>after</p>",
        800.0,
    );
    let pane = rect_of(&tree, &boxes, "section");
    assert_eq!((pane.y, pane.height), (50.0, 310.0));
    let list = rect_of(&tree, &boxes, "nav");
    assert_eq!((list.y, list.height), (50.0, 310.0));
    assert_eq!(rect_of(&tree, &boxes, "article").height, 310.0);
    assert_eq!(rect_of(&tree, &boxes, "footer").y, 360.0);
    let port = tree
        .scroll_ports
        .iter()
        .find(|port| boxes.node(port.id).tag.as_deref() == Some("nav"))
        .expect("the list scrolls");
    assert_eq!((port.port.height, port.content_height), (310.0, 3000.0));
}

/// A table with no width of its own is as wide as its columns need, but not
/// narrower than its `min-width`, and its columns are stretched to fill that
/// (both references: 300 wide around a single letter, half of a 400-pixel box
/// for `min-width: 50%`, and 300 across a bordered one).
#[test]
fn a_shrinking_table_is_held_to_its_minimum() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } table { min-width: 300px; border-spacing: 0 } \
         td { padding: 0 }</style><table><tr><td>x</td></tr></table>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "table").width, 300.0);
    assert_eq!(rect_of(&tree, &boxes, "td").width, 300.0);

    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } div { width: 400px } \
         table { min-width: 50%; border-spacing: 0 } td { padding: 0 }</style>\
         <div><table><tr><td>x</td></tr></table></div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "table").width, 200.0);

    // A table's width is measured across its border (HTML's rendering section
    // puts `box-sizing: border-box` on it), and so is its minimum.
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }          table { min-width: 300px; border: 5px solid; padding: 3px }</style>         <table><tr><td>x</td><td>yy</td></tr></table>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "table").width, 300.0);
}

/// A picture's own width is what its height makes of it through its ratio
/// (CSS Sizing 3 §5.1): an 800-by-400 picture told to be fifty pixels tall is a
/// hundred wide to a float that holds it, to a box as narrow as its content and
/// to a flex row, as it is in both references.
#[test]
fn a_picture_told_its_height_is_as_wide_as_its_ratio_makes_it() {
    for (holder, html) in [
        ("float", "<div style=\"float: left\"><img src=a.png></div>"),
        (
            "min-content",
            "<div style=\"width: min-content\"><img src=a.png></div>",
        ),
    ] {
        let (tree, boxes) = with_a_wide_picture(&format!(
            "<style>img {{ height: 50px; display: block }}</style>{html}"
        ));
        assert_eq!(rect_of(&tree, &boxes, "div").width, 100.0, "{holder}");
    }

    let (tree, boxes) = with_a_wide_picture(
        "<style>img { height: 50px } main { display: flex; width: 400px } \
         p { flex: 1; margin: 0 }</style><main><img src=a.png><p></p></main>",
    );
    let img = rect_of(&tree, &boxes, "img");
    assert_eq!((img.width, img.height), (100.0, 50.0));
    assert_eq!(rect_of(&tree, &boxes, "p").x, 100.0);
}
