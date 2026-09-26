//! Inline layout, checked against whole documents.

use crate::flow::tests::{boxes_of, image_rect, laid_out, laid_out_with_image, picture};
use crate::{BoxTree, Fragment, FragmentKind, FragmentTree, Rect};

/// The one box fragment the element `tag` generated, and the run of its text.
fn box_and_text(tree: &FragmentTree, boxes: &BoxTree, tag: &str) -> (Fragment, Fragment) {
    let pieces = boxes_of(tree, boxes, tag);
    let [piece] = &pieces[..] else {
        panic!("one box fragment for <{tag}>");
    };
    let text = tree
        .iter()
        .find(|fragment| {
            matches!(fragment.kind, FragmentKind::Text(_)) && fragment.box_id == piece.box_id
        })
        .unwrap_or_else(|| panic!("the text of <{tag}>"));
    (piece.clone(), text.clone())
}

/// A raised inline box with a background moves exactly as far as its own text.
///
/// `super` is measured against the font of the paragraph the box is in, which
/// the text's shift already was. The box's own was measured against the box's
/// own, smaller font, and its background came out a few pixels below the words
/// it was behind.
#[test]
fn a_raised_inline_box_moves_as_far_as_its_text() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0; font: 24px/40px monospace } \
         span, sup { background: #fd0; font-size: 12px }</style>\
         <p>x<span>1</span><sup>1</sup></p>",
        400.0,
    );

    let (span, span_text) = box_and_text(&tree, &boxes, "span");
    let (sup, sup_text) = box_and_text(&tree, &boxes, "sup");
    let raised = span_text.rect.y - sup_text.rect.y;
    assert!(raised > 0.0, "the superscript's text is raised");
    assert!(
        (span.rect.y - sup.rect.y - raised).abs() < 0.01,
        "its box raised {} and its text {raised}",
        span.rect.y - sup.rect.y
    );
}

/// How many line boxes a document laid out to.
fn line_boxes(tree: &crate::FragmentTree) -> usize {
    tree.iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .count()
}

/// A float sized to its own max-content holds its text on one line wherever
/// it sits (CSS Sizing 3 §5.1), with or without a frame around the text.
///
/// The letter spacing makes the advances fractions no binary float holds
/// exactly; the vendored face's 2048 units to the em would otherwise hide the
/// ulp a width loses when it is rebuilt in page coordinates, or taken through
/// the padding and border and back.
#[test]
fn a_float_at_its_max_content_width_is_one_line() {
    for frame in ["", "padding: 0 3.3px; border: 0.3px solid"] {
        for margin in [0, 30, 218, 400] {
            let (tree, _) = laid_out(
                &format!(
                    "<body style=\"margin: 0\"><div style=\"float: left; \
                     margin-left: {margin}px; {frame}; letter-spacing: 0.1px; \
                     font: 12px sans-serif\">The Organization</div>"
                ),
                800.0,
            );
            assert_eq!(line_boxes(&tree), 1, "at {margin}px, framed by {frame:?}");
        }
    }
}

/// The same for a table cell, which is sized to its max-content the same way.
#[test]
fn a_table_cell_at_its_max_content_width_is_one_line() {
    for margin in [0, 30, 218, 400] {
        let (tree, _) = laid_out(
            &format!(
                "<body style=\"margin: 0\"><table style=\"margin-left: {margin}px; \
                 border-spacing: 0\"><tr><td style=\"padding: 0; letter-spacing: 0.1px; \
                 font: 12px sans-serif\">The Organization</td></tr></table>"
            ),
            800.0,
        );
        assert_eq!(line_boxes(&tree), 1, "at {margin}px");
    }
}

/// The run of text that starts with `text`.
fn run_of(tree: &FragmentTree, text: &str) -> Rect {
    tree.iter()
        .find(|fragment| {
            matches!(&fragment.kind, FragmentKind::Text(run) if run.text.starts_with(text))
        })
        .map(|fragment| fragment.rect)
        .unwrap_or_else(|| panic!("no run starting {text:?}"))
}

/// The horizontal margins of an inline box take room at its two ends, outside
/// its border box (CSS 2.2 §8.3, §10.3.1).
#[test]
fn a_margin_on_an_inline_box_takes_room_in_the_line() {
    let (tree, _) = laid_out(
        "<body style=\"margin: 0\"><div><b style=\"margin-right: 40px\">Big</b>after</div>",
        400.0,
    );
    let gap = run_of(&tree, "after").x - run_of(&tree, "Big").right();
    assert!(gap >= 40.0 - 0.01, "a gap of {gap}");

    let (tree, boxes) = laid_out(
        "<body style=\"margin: 0\"><div>A<span style=\"margin: 0 20px; \
         background: #fc9\">x</span>B</div>",
        400.0,
    );
    let (span, x) = box_and_text(&tree, &boxes, "span");
    let (a, b) = (run_of(&tree, "A"), run_of(&tree, "B"));
    assert!(
        (span.rect.x - a.right() - 20.0).abs() < 0.01,
        "{span:?} after {a:?}"
    );
    assert!(
        (b.x - span.rect.right() - 20.0).abs() < 0.01,
        "{b:?} after {span:?}"
    );
    assert!(
        (span.rect.x - x.rect.x).abs() < 0.01 && (span.rect.width - x.rect.width).abs() < 0.01,
        "the border box is its text, not its margins"
    );
}

/// A negative margin pulls what follows back over the box, and the line is
/// that much shorter.
#[test]
fn a_negative_margin_on_an_inline_box_pulls_its_neighbours_in() {
    let line_width = |span: &str| {
        let (tree, _) = laid_out(
            &format!("<body style=\"margin: 0\"><div>ab<span {span}>cd</span>ef</div>"),
            400.0,
        );
        tree.iter()
            .find(|fragment| matches!(fragment.kind, FragmentKind::Line))
            .expect("a line box")
            .rect
            .width
    };
    let pulled = line_width("") - line_width("style=\"margin-left: -8px\"");
    assert!((pulled - 8.0).abs() < 0.01, "pulled in by {pulled}");
}

/// An atomic inline is placed in its line by its margin box, a picture and an
/// `inline-block` alike (CSS 2.2 §10.3.9).
#[test]
fn an_atomic_inline_takes_its_margins_in_the_line() {
    let (tree, boxes) = laid_out(
        "<body style=\"margin: 0\"><div>A<span style=\"display: inline-block; width: 10px; \
         height: 10px; margin: 0 20px\"></span>B</div>",
        400.0,
    );
    let chip = boxes_of(&tree, &boxes, "span")
        .first()
        .expect("the inline-block")
        .rect;
    let (a, b) = (run_of(&tree, "A"), run_of(&tree, "B"));
    assert!(
        (chip.x - a.right() - 20.0).abs() < 0.01,
        "{chip:?} after {a:?}"
    );
    assert!(
        (b.x - chip.right() - 20.0).abs() < 0.01,
        "{b:?} after {chip:?}"
    );

    let (tree, _) = laid_out_with_image(
        "<body style=\"margin: 0\"><div>A<img src=x.png style=\"margin: 0 20px\">B</div>",
        400.0,
        picture(10, 10),
    );
    let image = image_rect(&tree);
    let (a, b) = (run_of(&tree, "A"), run_of(&tree, "B"));
    assert!(
        (image.x - a.right() - 20.0).abs() < 0.01,
        "{image:?} after {a:?}"
    );
    assert!(
        (b.x - image.right() - 20.0).abs() < 0.01,
        "{b:?} after {image:?}"
    );
}

/// A checkbox between two letters is held apart from them by the margins the
/// user-agent sheet gives it: four pixels before and three after.
#[test]
fn a_checkbox_keeps_its_margins_from_the_letters_beside_it() {
    let (tree, boxes) = laid_out(
        "<body style=\"margin: 0\"><div>A<input type=checkbox>B</div>",
        400.0,
    );
    let check = boxes_of(&tree, &boxes, "input")
        .first()
        .expect("the checkbox")
        .rect;
    let (a, b) = (run_of(&tree, "A"), run_of(&tree, "B"));
    assert!(
        (check.x - a.right() - 4.0).abs() < 0.01,
        "{check:?} after {a:?}"
    );
    assert!(
        (b.x - check.right() - 3.0).abs() < 0.01,
        "{b:?} after {check:?}"
    );
}

/// An inline box's background is as tall as its font's content area, around
/// its own baseline, however tall the line it is on (CSS 2.2 §10.6.1).
#[test]
fn an_inline_box_is_as_tall_as_its_font_not_its_line() {
    let (tree, boxes) = laid_out(
        "<body style=\"margin: 0\"><p style=\"font: 16px/40px sans-serif\">\
         <span style=\"background: red\">x</span></p>",
        400.0,
    );
    let strut = otlyra_text::TextEngine::isolated()
        .strut(
            &otlyra_text::FontStack::named(otlyra_text::TEST_FAMILY),
            16.0,
            400,
            false,
        )
        .expect("the vendored font");
    let (span, text) = box_and_text(&tree, &boxes, "span");
    let FragmentKind::Text(run) = &text.kind else {
        unreachable!("a text fragment");
    };
    let baseline = text.rect.y + run.glyphs.first().expect("a glyph").y;

    assert!(
        (span.rect.height - (strut.ascent + strut.descent)).abs() < 0.01,
        "{} tall for a font of {strut:?}",
        span.rect.height
    );
    assert!(span.rect.height < 40.0);
    assert!(
        (span.rect.y - (baseline - strut.ascent)).abs() < 0.01,
        "its top at {} and the baseline at {baseline}",
        span.rect.y
    );
}

/// A block holding nothing but a no-break space holds a line.
#[test]
fn a_no_break_space_is_a_line_of_text() {
    let (tree, _) = laid_out(
        "<body style=\"margin: 0\"><div style=\"font: 16px/20px sans-serif\">&nbsp;</div>",
        400.0,
    );
    let line = tree
        .iter()
        .find(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .expect("a line box");
    assert_eq!(line.rect.height, 20.0);
}

/// A line with nothing on it but empty inline boxes is a phantom line box and
/// is not there at all — unless one of them has an inline-axis margin, border
/// or padding (CSS Inline 3; CSS 2.2 §9.4.2).
#[test]
fn only_inline_edges_keep_an_empty_line() {
    let lines = |body: &str| line_boxes(&laid_out(&format!("<body>{body}"), 400.0).0);
    assert_eq!(lines("<p><span></span>\n<span></span></p>"), 0);
    assert_eq!(
        lines("<div><span style=\"padding-top: 4px\"></span></div>"),
        0
    );
    assert_eq!(
        lines("<div><span style=\"background: red\"></span></div>"),
        0
    );
    assert_eq!(
        lines("<div><span style=\"padding-left: 4px\"></span></div>"),
        1
    );
    assert_eq!(
        lines("<div><span style=\"margin-right: 4px\"></span></div>"),
        1
    );
    assert_eq!(
        lines("<div><span style=\"border-left: 1px solid\"></span></div>"),
        1
    );
}

/// A no-break space is text as wide as a space, however its box is sized: a
/// cell or a float holding nothing else is one line, and not one of no width.
#[test]
fn a_box_of_no_break_spaces_is_one_line_wide_enough_for_them() {
    let (tree, _) = laid_out(
        "<body style=\"margin: 0\"><table><tr><td>&nbsp;</td><td>x</td>\
         <td>&nbsp;&nbsp;</td></tr></table>",
        400.0,
    );
    assert_eq!(line_boxes(&tree), 3, "one line to a cell");

    let (tree, boxes) = laid_out(
        "<body style=\"margin: 0\"><div style=\"float: left\">&nbsp;</div>",
        400.0,
    );
    assert_eq!(line_boxes(&tree), 1);
    let float = &boxes_of(&tree, &boxes, "div")[0];
    assert!(float.rect.width > 0.0, "{:?}", float.rect);
}

/// The edges of an inline box are not a place to break a line (CSS Text 3
/// §5.1): a margin on a link does not split the word it is part of, in a box
/// sized to its narrowest or squeezed below it.
#[test]
fn an_inline_box_edge_is_no_place_to_break_a_line() {
    for body in [
        "<table style=\"width: 1px\"><tr><td><a style=\"margin-left: 5px\">About</a>Us\
         </td></tr></table>",
        "<div style=\"display: flex; width: 1px\"><div><span style=\"margin-right: 6px\">*\
         </span>Favourites</div></div>",
        "<div style=\"float: left; width: min-content\">x<b style=\"margin-left: 8px\">Bold\
         </b>y</div>",
        "<table style=\"width: 1px\"><tr><td><a style=\"margin: 0 5px\">Home</a></td></tr>\
         </table>",
    ] {
        let (tree, _) = laid_out(&format!("<body style=\"margin: 0\">{body}"), 400.0);
        assert_eq!(line_boxes(&tree), 1, "{body}");
    }
}

/// A word too long for its line inside a link with a margin overflows with
/// the margin in front of it, and the next word starts the next line.
#[test]
fn a_margined_word_too_long_for_its_line_keeps_its_margin() {
    let (tree, _) = laid_out(
        "<body style=\"margin: 0\"><div style=\"width: 60px\">\
         <a style=\"margin-left: 6px\">Averyveryverylongword</a> tail</div>",
        400.0,
    );
    assert_eq!(line_boxes(&tree), 2);
    assert!((run_of(&tree, "Avery").x - 6.0).abs() < 0.01);
    assert!(run_of(&tree, "tail").x.abs() < 0.01, "no space before it");
}

/// An inline box's margin, border and padding are part of the piece of line
/// they sit on, at its narrowest as at its widest (CSS Sizing 3 §5.1).
#[test]
fn the_narrowest_line_holds_an_inline_box_edges() {
    let width = |content: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<body style=\"margin: 0\"><div style=\"float: left; width: min-content\">\
                 {content}</div>"
            ),
            400.0,
        );
        boxes_of(&tree, &boxes, "div")[0].rect.width
    };
    let framed = width("<span style=\"padding: 0 7px; margin-right: 6px\">word</span>");
    let bare = width("word");
    assert!(
        (framed - bare - 20.0).abs() < 0.01,
        "{framed} against {bare}"
    );
}

/// A box broken across two lines ends the first where that line's text does,
/// not past the space that hangs off it (CSS Text 3 §4.1.3).
#[test]
fn a_box_broken_across_lines_ends_at_its_last_letter() {
    let pieces = |text: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<body style=\"margin: 0\"><div style=\"width: 1px\">\
                 <span style=\"background: #fd0\">{text}</span></div>"
            ),
            400.0,
        );
        boxes_of(&tree, &boxes, "span")
    };
    let broken = pieces("aaa bbb");
    assert_eq!(broken.len(), 2);
    let alone = pieces("aaa");
    assert!(
        (broken[0].rect.width - alone[0].rect.width).abs() < 0.01,
        "{:?} against {:?}",
        broken[0].rect,
        alone[0].rect
    );
}

/// A line is painted in tree order: a box a negative margin pulls back over
/// the text before it is painted after that text, and covers it (CSS 2.2
/// Appendix E).
#[test]
fn a_line_is_painted_in_tree_order() {
    let (tree, boxes) = laid_out(
        "<body style=\"margin: 0\"><div>abc<span style=\"margin-left: -20px; \
         background: red\">x</span>d</div>",
        400.0,
    );
    let line = tree
        .iter()
        .find(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .expect("a line box");
    let span = boxes_of(&tree, &boxes, "span")[0].box_id;
    let position = |found: &dyn Fn(&Fragment) -> bool| {
        line.children
            .iter()
            .position(found)
            .expect("the fragment is on the line")
    };
    let text = |start: &'static str| move |fragment: &Fragment| matches!(&fragment.kind, FragmentKind::Text(run) if run.text.starts_with(start));
    let span_at =
        position(&|fragment| matches!(fragment.kind, FragmentKind::Box) && fragment.box_id == span);
    assert!(position(&text("abc")) < span_at);
    assert!(span_at < position(&text("x")));
}

/// An inline block that is a scroll container sits on its bottom margin edge
/// rather than on its text (CSS 2.2 §10.8.1); one that only clips is not a
/// scroll container and keeps its text's baseline.
#[test]
fn an_inline_block_that_scrolls_sits_on_its_bottom_margin_edge() {
    // How far the box's bottom margin edge sits below the line's baseline.
    let below_baseline = |overflow: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<body style=\"margin: 0\"><div style=\"font: 16px/20px sans-serif\">x\
                 <span style=\"display: inline-block; overflow: {overflow}; \
                 margin-bottom: 8px\">y</span></div>"
            ),
            400.0,
        );
        let x = tree
            .iter()
            .find_map(|fragment| match &fragment.kind {
                FragmentKind::Text(run) if run.text.starts_with('x') => {
                    Some(fragment.rect.y + run.glyphs[0].y)
                }
                _ => None,
            })
            .expect("the text beside it");
        boxes_of(&tree, &boxes, "span")[0].rect.bottom() + 8.0 - x
    };
    assert!(
        below_baseline("hidden").abs() < 0.01,
        "{}",
        below_baseline("hidden")
    );
    let visible = below_baseline("visible");
    assert!(visible > 10.0, "its text on the baseline: {visible}");
    assert!((below_baseline("clip") - visible).abs() < 0.01);
}
