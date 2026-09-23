//! Layout, checked against whole documents.
//!
//! Most of these cross more than one formatting context — a flex item holding a
//! paragraph is flex and inline layout at once — so they are kept together
//! rather than beside any one of the modules they exercise.

use otlyra_css::cascade::{Viewport as StyleViewport, style_document};

use super::replaced::replaced_size;
use super::*;
use crate::{BoxTree, FragmentKind, FragmentTree, build_styled_box_tree};

/// Lay a document out at `width`, with its own stylesheets applied, and keep
/// the boxes: a fragment says where something is, and only the box says what.
fn laid_out(html: &str, width: f32) -> (FragmentTree, BoxTree) {
    let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
    let styles = style_document(
        &document,
        StyleViewport {
            width,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let mut boxes = build_styled_box_tree(&document, &styles);
    let mut text = otlyra_text::TextEngine::isolated();
    let tree = crate::layout(
        &mut boxes,
        &mut text,
        Viewport {
            width,
            height: 600.0,
        },
    );
    (tree, boxes)
}

/// A picture's size, given what a stylesheet and the attributes asked for.
fn drawn_at(style: &ComputedStyle, hint: (Option<f32>, Option<f32>)) -> (f32, f32) {
    replaced_size(
        style,
        &crate::box_tree::Replaced {
            image: None,
            // Four wide and two tall: a ratio of two, so a wrong height is
            // obvious rather than a rounding difference.
            intrinsic: Some((4.0, 2.0)),
            hint,
        },
        800.0,
    )
}

/// `width` and `height` on an `<img>` are presentational hints — the lowest
/// priority rule setting those properties — and not a new intrinsic size.
/// Written into the intrinsic size they took the aspect ratio with them, so
/// `width="40"` drew a four-by-two picture forty wide and still two tall:
/// squashed on one axis, which on a photograph is a wall of vertical
/// streaks and was reported as exactly that.
#[test]
fn a_width_attribute_is_a_hint_and_not_a_new_intrinsic_size() {
    let plain = ComputedStyle::default();

    assert_eq!(drawn_at(&plain, (None, None)), (4.0, 2.0), "nothing asked");
    assert_eq!(
        drawn_at(&plain, (Some(40.0), None)),
        (40.0, 20.0),
        "one dimension takes the other from the ratio"
    );
    assert_eq!(
        drawn_at(&plain, (None, Some(20.0))),
        (40.0, 20.0),
        "from either side"
    );
    assert_eq!(
        drawn_at(&plain, (Some(40.0), Some(5.0))),
        (40.0, 5.0),
        "and both given is both honoured, ratio or not"
    );
    // Downscaling is the same rule and the case the site showed.
    assert_eq!(drawn_at(&plain, (Some(2.0), None)), (2.0, 1.0));

    // A stylesheet outranks the hint, which is what makes it a hint.
    let styled = ComputedStyle {
        width: otlyra_css::LengthOrAuto::Px(80.0),
        ..ComputedStyle::default()
    };
    assert_eq!(drawn_at(&styled, (Some(40.0), None)), (80.0, 40.0));
}

/// How many lines of text a document laid out to.
fn line_count(tree: &FragmentTree) -> usize {
    fn walk(fragment: &Fragment, tops: &mut Vec<i64>) {
        if matches!(fragment.kind, FragmentKind::Text(_)) {
            tops.push((f64::from(fragment.rect.y) * 10.0) as i64);
        }
        for child in &fragment.children {
            walk(child, tops);
        }
    }
    let mut tops = Vec::new();
    walk(&tree.root, &mut tops);
    tops.sort_unstable();
    tops.dedup();
    tops.len()
}

/// `white-space` is two independent bits, and until now the layout had only
/// one of them: whether spaces collapse. Whether a line may break was never
/// asked, so `nowrap` wrapped — which folded the site's own header onto a
/// second line — and so did `pre`.
#[test]
fn whether_a_line_breaks_is_its_own_property() {
    let narrow = |style: &str| {
        let html = format!("<body><div style=\"width:60px;{style}\">one two three four five</div>");
        let (tree, _) = laid_out(&html, 800.0);
        line_count(&tree)
    };

    // The four arrangements of the two bits, and each is a real value of the
    // shorthand: collapse-and-wrap, collapse-and-not, keep-and-not, keep-and-wrap.
    assert!(narrow("") > 1, "normal wraps");
    assert_eq!(narrow("white-space:nowrap"), 1, "nowrap does not");
    assert_eq!(narrow("white-space:pre"), 1, "nor does pre");
    assert!(narrow("white-space:pre-wrap") > 1, "but pre-wrap does");
}

/// The site's header, in the shape that folded it: links told not to break,
/// in a row told not to wrap.
#[test]
fn a_nav_told_not_to_wrap_stays_on_one_line() {
    let (tree, _) = laid_out(
        "<body><nav style=\"display:flex;flex-wrap:nowrap;width:80px\">\
         <a style=\"white-space:nowrap\">Home page</a>\
         <a style=\"white-space:nowrap\">About us</a></nav>",
        800.0,
    );
    assert_eq!(line_count(&tree), 1);
}

/// A row of flex items is as wide as its items side by side. Both intrinsic
/// widths took the *widest* item instead: right for boxes that stack, wrong
/// for boxes that sit in a row — and a flex item is blockified, so it never
/// reached the inline branch that would have summed it. The site's brand came
/// out as wide as its wordmark alone, and the wordmark was drawn over the nav
/// beside it.
#[test]
fn a_flex_row_is_as_wide_as_its_items_side_by_side() {
    let (tree, boxes) = laid_out(
        "<body><div style=\"display:flex;width:600px\">\
         <a id=brand style=\"display:flex;gap:10px\">\
         <span style=\"display:block;width:30px\">.</span>\
         <span style=\"display:block;width:40px\">.</span></a>\
         <nav style=\"display:flex\"><a style=\"display:block;width:50px\">.</a></nav>\
         </div>",
        800.0,
    );

    // Thirty and forty with ten between them: eighty, not the forty that the
    // wider of the two would have given.
    let brand = rect_of(&tree, &boxes, "a");
    assert_eq!(brand.width, 80.0);
    // And what comes after it starts where it ends, rather than over it.
    let nav = rect_of(&tree, &boxes, "nav");
    assert!(
        nav.x >= brand.x + brand.width,
        "the nav at {} runs into the brand ending at {}",
        nav.x,
        brand.x + brand.width
    );
}

/// An `auto` margin on a flex item eats the free space before
/// `justify-content` sees any — which is how a brand is pushed to one end and
/// a nav to the other, and how a lone item is centred.
#[test]
fn an_auto_margin_takes_the_free_space_first() {
    let (tree, boxes) = laid_out(
        "<body><div style=\"display:flex;width:400px\">\
         <a style=\"margin-right:auto;width:50px\">.</a>\
         <b style=\"width:50px\">.</b></div>",
        800.0,
    );
    // Offsets are relative to the container, whose own position carries the
    // body's default margin; what is being asserted is the gap the auto
    // margin opened, not where the page starts.
    let brand = rect_of(&tree, &boxes, "a");
    let end = rect_of(&tree, &boxes, "b");
    assert_eq!(
        end.x - brand.x,
        350.0,
        "pushed to the far end, not left beside the first"
    );

    // Both sides `auto` centres it, the same rule seen twice.
    let (tree, boxes) = laid_out(
        "<body><div style=\"display:flex;width:400px\">\
         <a style=\"margin:0 auto;width:100px\">.</a></div>",
        800.0,
    );
    let container = rect_of(&tree, &boxes, "div");
    let centred = rect_of(&tree, &boxes, "a");
    assert_eq!(centred.x - container.x, 150.0);
}

/// Layout knows what `auto` came out as and a computed style does not, so the
/// edges a box actually got are reported on the fragment for the panel that
/// asks.
#[test]
fn a_fragment_carries_the_edges_it_was_given() {
    let (tree, boxes) = laid_out(
        "<body><div id=x style=\"width:100px;margin:0 auto;padding:5px;border:2px solid\">.</div>",
        400.0,
    );
    let used = tree
        .iter()
        .find(|fragment| {
            fragment
                .box_id
                .and_then(|id| boxes.get(id))
                .and_then(|node| node.tag.as_ref())
                .is_some_and(|tag| tag.as_ref() == "div")
        })
        .and_then(|fragment| fragment.used)
        .expect("a div was laid out with edges");

    assert_eq!(used.padding.left, 5.0);
    assert_eq!(used.border.left, 2.0);
    // A margin the style spells `auto` is a number here, which is the whole
    // reason the panel reads this rather than the computed style. Both sides
    // came out equal and neither is nothing, which is what centring is.
    assert!(used.margin.left > 0.0, "an auto margin resolved to nothing");
    assert!((used.margin.left - used.margin.right).abs() < 0.5);
}

/// Under `nowrap` the whole run is one unbreakable thing, so a flex item may
/// not be shrunk to its longest word — the text it draws would spill over the
/// item beside it, which is what overlapped the site's nav links.
#[test]
fn a_nowrap_item_is_not_shrunk_to_its_longest_word() {
    let (tree, boxes) = laid_out(
        "<body><nav style=\"display:flex;width:60px\">\
         <a style=\"white-space:nowrap\">The name</a>\
         <b style=\"white-space:nowrap\">For agents</b></nav>",
        800.0,
    );
    let first = rect_of(&tree, &boxes, "a");
    let second = rect_of(&tree, &boxes, "b");
    assert!(
        second.x >= first.x + first.width,
        "the items overlap: one is {}..{} and the next starts at {}",
        first.x,
        first.x + first.width,
        second.x
    );
}

/// The first box fragment whose element is `tag`.
fn rect_of(tree: &FragmentTree, boxes: &BoxTree, tag: &str) -> Rect {
    fn walk<'a>(fragment: &'a Fragment, out: &mut Vec<&'a Fragment>) {
        out.push(fragment);
        for child in &fragment.children {
            walk(child, out);
        }
    }
    let mut all = Vec::new();
    walk(&tree.root, &mut all);
    all.into_iter()
        .find(|fragment| {
            matches!(fragment.kind, FragmentKind::Box)
                && fragment
                    .box_id
                    .and_then(|id| boxes.get(id))
                    .and_then(|node| node.tag.as_ref())
                    .is_some_and(|name| name.as_ref() == tag)
        })
        .map(|fragment| fragment.rect)
        .unwrap_or_else(|| panic!("no <{tag}> box fragment"))
}

/// The first line box, which is where alignment shows.
fn first_line(tree: &FragmentTree) -> Rect {
    fn walk(fragment: &Fragment) -> Option<Rect> {
        if matches!(fragment.kind, FragmentKind::Line) {
            return Some(fragment.rect);
        }
        fragment.children.iter().find_map(walk)
    }
    walk(&tree.root).expect("a line box")
}

/// Every box fragment generated by the element `tag`, in order.
fn boxes_of(tree: &FragmentTree, boxes: &BoxTree, tag: &str) -> Vec<Fragment> {
    tree.iter()
        .filter(|fragment| {
            matches!(fragment.kind, FragmentKind::Box)
                && fragment
                    .box_id
                    .and_then(|id| boxes.get(id))
                    .and_then(|node| node.tag.as_ref())
                    .is_some_and(|name| name.as_ref() == tag)
        })
        .cloned()
        .collect()
}

/// The text runs of a laid-out document, left to right within each line.
fn runs(tree: &FragmentTree) -> Vec<Rect> {
    tree.iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Text(_)))
        .map(|fragment| fragment.rect)
        .collect()
}

/// The five values of `vertical-align` that are a position rather than a
/// shift, each moving its box the way the specification says.
#[test]
fn vertical_align_puts_a_span_where_the_value_names() {
    /// Where the span ended up, and how tall the line it is on came out.
    fn span_and_line(value: &str) -> (Rect, Rect) {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0; font: 16px/80px monospace }} \
                 span {{ font-size: 32px; background: #eee; \
                 line-height: normal; vertical-align: {value} }}</style>\
                 <p>base <span>x</span> after</p>"
            ),
            600.0,
        );
        let pieces = boxes_of(&tree, &boxes, "span");
        let span = pieces.first().expect("a box for the span").rect;
        (span, first_line(&tree))
    }

    let (baseline, _) = span_and_line("baseline");
    let (top, top_line) = span_and_line("top");
    let (bottom, bottom_line) = span_and_line("bottom");
    let (text_top, _) = span_and_line("text-top");
    let (text_bottom, _) = span_and_line("text-bottom");
    let (middle, _) = span_and_line("middle");

    // Ordering rather than absolute edges: an inline box's fragment is
    // still drawn as tall as the *line* rather than as tall as its own
    // text — a separate defect, visible as a background taller than the
    // words it is behind — so its top edge is what can be trusted here.
    assert!(
        top.y < bottom.y,
        "top {top:?} sits above bottom {bottom:?} on {top_line:?} / {bottom_line:?}"
    );
    assert!(
        text_top.y < text_bottom.y,
        "text-top {text_top:?} sits above text-bottom {text_bottom:?}"
    );

    // And every one of them is somewhere: a value that did nothing would
    // land exactly where `baseline` did, which is how these five behaved
    // before the cascade was read for them.
    for (name, rect) in [
        ("top", top),
        ("bottom", bottom),
        ("text-top", text_top),
        ("text-bottom", text_bottom),
        ("middle", middle),
    ] {
        assert!(
            (rect.y - baseline.y).abs() > 0.5,
            "{name} moved nothing: {rect:?} against baseline {baseline:?}"
        );
    }
}

/// An inline element with a background but nothing else different about it is
/// the case the shaper merges into its neighbours' run: its box has to come
/// from the boundaries it was shaped with, not from a run of its own.
#[test]
fn an_inline_box_covers_its_own_text_inside_a_merged_run() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } span { background: #ff0 }</style>\
         <p>before <span>middle</span> after</p>",
        400.0,
    );

    let pieces = boxes_of(&tree, &boxes, "span");
    let [span] = &pieces[..] else {
        panic!("one box fragment for the span");
    };
    let line = first_line(&tree);
    assert!(span.rect.x > line.x, "it starts after the text before it");
    assert!(span.rect.right() < line.right(), "and ends before the rest");
    assert!(span.rect.width > 0.0);
}

/// Padding and a border on an inline element take room in the line: the text
/// after them moves over by exactly as much.
#[test]
fn padding_and_borders_on_an_inline_box_move_the_text_along() {
    let bare = laid_out(
        "<style>body { margin: 0 }</style><p>a<span>b</span>c</p>",
        400.0,
    )
    .0;
    let padded = laid_out(
        "<style>body { margin: 0 } \
         span { padding: 0 6px; border: 2px solid black }</style>\
         <p>a<span>b</span>c</p>",
        400.0,
    )
    .0;

    let widths = |tree: &FragmentTree| first_line(tree).width;
    assert!(
        (widths(&padded) - widths(&bare) - 16.0).abs() < 0.01,
        "two paddings and two borders wider"
    );

    let last = |tree: &FragmentTree| *runs(tree).last().expect("a run");
    assert!(
        last(&padded).x - last(&bare).x >= 15.0,
        "and the text after the span starts that much further along"
    );
}

/// An inline box broken over two lines is drawn as two pieces, each ending
/// where its line does, and the border on an edge belongs to the piece that
/// edge is on.
#[test]
fn a_wrapped_inline_box_is_open_where_the_line_broke() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } span { border: 2px solid black }</style>\
         <p><span>alpha beta gamma delta</span></p>",
        80.0,
    );

    let pieces = boxes_of(&tree, &boxes, "span");
    assert!(pieces.len() > 1, "the span wrapped");
    let first = pieces.first().expect("a first piece");
    let last = pieces.last().expect("a last piece");

    assert_eq!(first.style.border.left.width, 2.0);
    assert_eq!(first.style.border.right.width, 0.0);
    assert_eq!(last.style.border.left.width, 0.0);
    assert_eq!(last.style.border.right.width, 2.0);
    assert!(last.rect.y > first.rect.y, "on different lines");
}

/// The box a border is drawn on includes the border, and the content sits
/// inside it. Getting this wrong puts text on top of its own frame.
#[test]
fn a_border_makes_the_box_bigger_and_moves_the_content_in() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { border: 5px solid black; padding: 10px }</style><div>text</div>",
        400.0,
    );

    let div = rect_of(&tree, &boxes, "div");
    assert_eq!(div.x, 0.0);
    assert_eq!(div.width, 400.0);

    let line = first_line(&tree);
    assert_eq!(line.x, 15.0, "border plus padding on the left");
    assert_eq!(line.y, 15.0, "border plus padding on the top");
    assert_eq!(
        div.height,
        line.height + 30.0,
        "the border box is the content plus both borders and both paddings"
    );
}

/// A picture of `width` by `height`, with no file behind it: layout reads its
/// dimensions and never its pixels.
fn picture(width: u32, height: u32) -> otlyra_gfx::peniko::ImageData {
    otlyra_gfx::peniko::ImageData {
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![
            0u8;
            width as usize
                * height as usize
                * 4
        ])),
        format: otlyra_gfx::peniko::ImageFormat::Rgba8,
        alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
        width,
        height,
    }
}

/// Lay out a document whose every `<img>` shows the same picture.
fn laid_out_with_image(
    html: &str,
    width: f32,
    image: otlyra_gfx::peniko::ImageData,
) -> (FragmentTree, BoxTree) {
    let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
    let styles = style_document(
        &document,
        StyleViewport {
            width,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let images: crate::Images = crate::image_sources(&document, StyleViewport::default())
        .into_iter()
        .map(|source| (source.node, crate::Picture::new(image.clone())))
        .collect();
    let mut boxes = crate::build_box_tree_with_images(&document, Some(&styles), &images);
    let mut text = otlyra_text::TextEngine::isolated();
    let tree = crate::layout(
        &mut boxes,
        &mut text,
        Viewport {
            width,
            height: 600.0,
        },
    );
    (tree, boxes)
}

/// The first image fragment, which is what a picture is drawn from.
fn image_rect(tree: &FragmentTree) -> Rect {
    tree.iter()
        .find(|fragment| matches!(fragment.kind, FragmentKind::Image(_)))
        .map(|fragment| fragment.rect)
        .expect("an image fragment")
}

/// A picture with no size of its own in the CSS is drawn at the size it is.
#[test]
fn an_image_takes_its_own_size() {
    let (tree, _) = laid_out_with_image(
        "<style>body { margin: 0 }</style><p><img src=a.png></p>",
        400.0,
        picture(64, 32),
    );
    let rect = image_rect(&tree);
    assert_eq!((rect.width, rect.height), (64.0, 32.0));
}

/// One dimension given takes the other from the picture's own ratio, which is
/// what keeps a photograph from being squashed by `width: 100%`.
#[test]
fn one_given_dimension_keeps_the_ratio() {
    let (tree, _) = laid_out_with_image(
        "<style>body { margin: 0 } img { width: 200px }</style><p><img src=a.png></p>",
        400.0,
        picture(100, 50),
    );
    let rect = image_rect(&tree);
    assert_eq!((rect.width, rect.height), (200.0, 100.0));
}

/// The box a replaced element makes, and the picture inside it.
fn boxed_image(tree: &FragmentTree) -> (Rect, Rect) {
    let outer = tree
        .iter()
        .find(|fragment| {
            fragment
                .children
                .iter()
                .any(|child| matches!(child.kind, FragmentKind::Image(_)))
        })
        .expect("a box holding a picture");
    (outer.rect, image_rect(tree))
}

/// A replaced element has a border and a background of its own, and both take
/// room: the box is the picture plus the frame around it.
///
/// Every number here was measured against a reference browser on the same
/// page. A hundred-by-fifty picture, a ten-pixel border and five of padding.
#[test]
fn a_picture_has_a_frame_and_the_frame_takes_room() {
    let laid_out = |rule: &str| {
        laid_out_with_image(
            &format!(
                "<style>body {{ margin: 0 }} img {{ display: block; border: 10px solid red; \
                 padding: 5px; {rule} }}</style><img src=a.png>"
            ),
            820.0,
            picture(100, 50),
        )
        .0
    };

    let size = |tree: &FragmentTree| {
        let (outer, inner) = boxed_image(tree);
        (
            (outer.width, outer.height),
            (inner.width, inner.height),
            (inner.x - outer.x, inner.y - outer.y),
        )
    };

    // Nothing asked: the picture's own size, and the frame outside it.
    assert_eq!(
        size(&laid_out("")),
        ((130.0, 80.0), (100.0, 50.0), (15.0, 15.0))
    );
    // A width is the picture's width, and the frame is still outside it.
    assert_eq!(
        size(&laid_out("width: 100px")),
        ((130.0, 80.0), (100.0, 50.0), (15.0, 15.0))
    );
    // `border-box` measures across the frame instead, and the ratio is
    // applied to what is left over rather than to the number the page wrote.
    assert_eq!(
        size(&laid_out("width: 100px; box-sizing: border-box")),
        ((100.0, 65.0), (70.0, 35.0), (15.0, 15.0))
    );
    assert_eq!(
        size(&laid_out(
            "width: 100px; height: 40px; box-sizing: border-box"
        )),
        ((100.0, 40.0), (70.0, 10.0), (15.0, 15.0))
    );
    // A percentage width is a percentage of the containing block, and the
    // frame is added to it — which is what makes `width: 100%` overflow.
    assert_eq!(
        size(&laid_out("width: 100%")),
        ((850.0, 440.0), (820.0, 410.0), (15.0, 15.0))
    );
}

/// A picture in a line reserves its frame there too, and its background and
/// border are painted like any other box's.
#[test]
fn an_inline_picture_reserves_its_frame_in_the_line() {
    let (tree, _) = laid_out_with_image(
        "<style>body { margin: 0 } img { border: 10px solid red; padding: 5px }\
         </style><p>x <img src=a.png> y</p>",
        820.0,
        picture(100, 50),
    );

    let (outer, inner) = boxed_image(&tree);
    assert_eq!((outer.width, outer.height), (130.0, 80.0));
    assert_eq!((inner.width, inner.height), (100.0, 50.0));
    assert_eq!((inner.x - outer.x, inner.y - outer.y), (15.0, 15.0));

    let line = first_line(&tree);
    assert!(
        line.height >= 80.0,
        "the line holds the whole box: {}",
        line.height
    );
}

/// An image in a line takes room in it: the text after it starts further along
/// and the line is at least as tall as the picture.
#[test]
fn an_image_takes_room_in_the_line_it_sits_in() {
    let (with, _) = laid_out_with_image(
        "<style>body { margin: 0 }</style><p>before <img src=a.png> after</p>",
        400.0,
        picture(80, 60),
    );
    let (without, _) = laid_out_with_image(
        "<style>body { margin: 0 }</style><p>before after</p>",
        400.0,
        picture(80, 60),
    );

    let line = first_line(&with);
    assert!(line.width > first_line(&without).width + 79.0);
    assert!(line.height >= 60.0, "line height was {}", line.height);
}

/// A picture that never arrived generates no image fragment, and the element
/// keeps its `alt` text instead.
#[test]
fn a_missing_picture_leaves_the_alt_text() {
    let (tree, _) = laid_out(
        "<style>body { margin: 0 }</style><p><img src=a.png alt=\"a description\"></p>",
        400.0,
    );
    assert!(
        !tree
            .iter()
            .any(|fragment| matches!(fragment.kind, FragmentKind::Image(_)))
    );
    assert!(first_line(&tree).width > 0.0, "the alt text was laid out");
}

/// A float goes to its edge and the boxes after it stack as though it were not
/// there, because it is not: it is out of the flow.
#[test]
fn a_float_goes_to_its_edge_and_leaves_the_flow() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }              .f { float: right; width: 100px; height: 40px }              p { margin: 0 }</style>             <div class=f>f</div><p>text</p>",
        400.0,
    );

    let float = rect_of(&tree, &boxes, "div");
    assert_eq!(float.x, 300.0, "against the right edge");
    assert_eq!(float.y, 0.0);
    assert_eq!(
        rect_of(&tree, &boxes, "p").y,
        0.0,
        "the paragraph starts level with it"
    );
}

/// The lines beside a float are shortened, and the ones below it are not.
#[test]
fn lines_beside_a_float_are_shorter_than_the_ones_below_it() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }              .f { float: left; width: 200px; height: 30px }              p { margin: 0 }</style>             <div class=f>f</div>             <p>alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu              nu xi omicron pi rho sigma tau upsilon phi chi psi omega</p>",
        400.0,
    );

    // The paragraph's own lines: the float has one too, and it starts at the
    // float's edge rather than beside it.
    let paragraph = boxes_of(&tree, &boxes, "p");
    let lines: Vec<Rect> = paragraph[0]
        .children
        .iter()
        .filter(|fragment| matches!(fragment.kind, FragmentKind::Line))
        .map(|fragment| fragment.rect)
        .collect();
    assert!(lines.len() > 2, "the paragraph wrapped");

    let first = lines.first().expect("a first line");
    let last = lines.last().expect("a last line");
    assert!(first.x >= 200.0, "the first line starts beside the float");
    assert!(last.x < 200.0, "and a line below it starts at the edge");
    assert!(last.width > first.width);
}

/// `clear` puts a box below the floats it names.
#[test]
fn clear_puts_a_box_below_the_float() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }              .f { float: left; width: 100px; height: 80px }              p { margin: 0 } .c { clear: left }</style>             <div class=f>f</div><p class=c>text</p>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "p").y, 80.0);
}

/// Two floats on the same side sit beside each other while there is room, and
/// the one that does not fit goes below.
#[test]
fn floats_stack_along_the_edge_and_then_down() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }              div { float: left; width: 150px; height: 20px }</style>             <div>one</div><div>two</div><div>three</div>",
        400.0,
    );
    let floats = boxes_of(&tree, &boxes, "div");
    assert_eq!((floats[0].rect.x, floats[0].rect.y), (0.0, 0.0));
    assert_eq!((floats[1].rect.x, floats[1].rect.y), (150.0, 0.0));
    assert_eq!(
        (floats[2].rect.x, floats[2].rect.y),
        (0.0, 20.0),
        "the third has nowhere beside them to go"
    );
}

/// A float is a formatting context of its own: the floats outside it do not
/// shorten the lines inside it.
#[test]
fn a_float_is_not_flowed_around_by_its_own_contents() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 }              .a { float: left; width: 200px; height: 50px }              .b { float: right; width: 100px; height: 50px }</style>             <div class=a>a</div><div class=b>b</div>",
        400.0,
    );
    let floats = boxes_of(&tree, &boxes, "div");
    let inner = floats[1]
        .children
        .iter()
        .find(|child| matches!(child.kind, FragmentKind::Line))
        .expect("the right float's own line");
    assert_eq!(
        inner.rect.x, floats[1].rect.x,
        "its text starts at its own left edge"
    );
}

/// A row of flex items sits along one line, in order, at the sizes the
/// container gave them.
#[test]
fn flex_items_lie_along_the_main_axis() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .flex { display: flex } \
         .a { width: 100px; height: 20px } .b { width: 60px; height: 40px }</style>\
         <div class=flex><div class=a>a</div><div class=b>b</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    // The container first, then its two items.
    assert_eq!(items[1].rect.x, 0.0);
    assert_eq!(items[1].rect.width, 100.0);
    assert_eq!(items[2].rect.x, 100.0);
    assert_eq!(items[2].rect.width, 60.0);
    assert_eq!(items[1].rect.y, items[2].rect.y, "they share a line");
}

/// `order` decides which of its siblings an item is laid out among, and
/// nothing else: the document order is still what the text reads as.
///
/// A stable sort, so items that name the same order keep the order they were
/// written in. Measured against a reference.
#[test]
fn order_rearranges_the_items_and_leaves_the_document_alone() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex } \
         div > div { width: 100px }</style>\
         <div class=flex><div style='order:3'>a</div><div style='order:1'>b</div>\
         <div>c</div><div style='order:-1'>d</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    // The container first, then its four items as they were laid out.
    let placed: Vec<(f32, i32)> = items[1..]
        .iter()
        .map(|item| (item.rect.x, item.style.order))
        .collect();
    assert_eq!(
        placed,
        vec![(0.0, -1), (100.0, 0), (200.0, 1), (300.0, 3)],
        "`order` ascending, and the two that agree keep the order they were written in"
    );
}

/// A margin does not escape a box that establishes a formatting context of
/// its own — a flex item, a float, a cell, anything that clips.
///
/// Found by putting a heading above a flex container inside a flex item and
/// comparing with a reference: the heading's own top margin was collapsing
/// out through the item and pushing the whole row down, which left a gap
/// above the item rather than inside it. On a page of such rows the error
/// added up down the page.
#[test]
fn a_margin_does_not_escape_a_formatting_context() {
    let inside = |row: &str, item: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} .row {{ {row} }} \
                 .item {{ width: 200px; {item} }} h2 {{ margin: 20px 0 0 }}</style>\
                 <div class=row><div class=item><h2>a</h2></div></div>"
            ),
            400.0,
        );
        let item = boxes_of(&tree, &boxes, "div")[1].rect;
        let heading = boxes_of(&tree, &boxes, "h2")[0].rect;
        (item.y, heading.y - item.y)
    };

    // A flex item keeps its child's margin inside itself.
    assert_eq!(
        inside("display: flex", ""),
        (0.0, 20.0),
        "the item starts at the top and the margin is inside it"
    );
    // So does anything that clips, and anything that floats.
    assert_eq!(inside("display: block", "overflow: hidden"), (0.0, 20.0));
    assert_eq!(inside("display: block", "float: left"), (0.0, 20.0));
    // An ordinary block does not: the margin comes out through it and lands
    // above it, which is what collapsing is.
    assert_eq!(inside("display: block", ""), (20.0, 0.0));
}

/// `align-content` shares out the room a wrapped container's lines leave
/// across it. Every number here was measured against a reference.
#[test]
fn align_content_places_the_lines_of_a_wrapped_container() {
    let tops = |align: &str| {
        let (tree, boxes) = laid_out(
            &format!(
                "<style>body {{ margin: 0 }} .flex {{ display: flex; flex-wrap: wrap; \
                 width: 300px; height: 200px; align-content: {align} }} \
                 div > div {{ width: 120px; height: 40px }}</style>\
                 <div class=flex><div>1</div><div>2</div><div>3</div><div>4</div>\
                 <div>5</div></div>"
            ),
            400.0,
        );
        let items = boxes_of(&tree, &boxes, "div");
        // The tops of the three lines: items one, three and five.
        (items[1].rect.y, items[3].rect.y, items[5].rect.y)
    };

    // Three lines of forty in two hundred: eighty over, and `stretch` — the
    // initial value — gives each line a third of it.
    assert_eq!(tops("stretch"), (0.0, 200.0 / 3.0, 400.0 / 3.0));
    assert_eq!(tops("flex-start"), (0.0, 40.0, 80.0));
    assert_eq!(tops("center"), (40.0, 80.0, 120.0));
    assert_eq!(tops("flex-end"), (80.0, 120.0, 160.0));
    assert_eq!(tops("space-between"), (0.0, 80.0, 160.0));
    // Half a share at each end and a whole one between: the share is eighty
    // over three, so the first line starts at half of it and each one after
    // begins forty and a share further down.
    let around = tops("space-around");
    let share = 80.0 / 3.0;
    assert!(
        (around.0 - share / 2.0).abs() < 0.01
            && (around.1 - (share / 2.0 + 40.0 + share)).abs() < 0.01
            && (around.2 - (share / 2.0 + 80.0 + share * 2.0)).abs() < 0.01,
        "{around:?}"
    );
}

/// `inline-flex` is a flex container that takes its place in a line, which is
/// where an `inline-block` goes: inline outside, flex inside.
#[test]
fn an_inline_flex_container_sits_in_the_line() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: inline-flex } \
         .flex > span { width: 40px; height: 20px }</style>\
         <div>before <span class=flex><span>a</span><span>b</span></span> after</div>",
        400.0,
    );
    let placed = boxes_of(&tree, &boxes, "span");
    let container = &placed[0];
    assert_eq!(
        container.rect.width, 80.0,
        "as wide as its items side by side, not as wide as the line"
    );
    assert!(
        container.rect.x > 0.0,
        "and it starts after the text before it: {:?}",
        container.rect
    );
}

/// `flex-grow` shares out what is left over; `flex-shrink` takes back what is
/// missing.
#[test]
fn grow_and_shrink_share_out_the_main_axis() {
    let (grown, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex } \
         .a { width: 100px; flex-grow: 1 } .b { width: 100px; flex-grow: 3 }</style>\
         <div class=flex><div class=a>a</div><div class=b>b</div></div>",
        400.0,
    );
    let items = boxes_of(&grown, &boxes, "div");
    assert_eq!(items[1].rect.width, 150.0, "one quarter of the 200 spare");
    assert_eq!(items[2].rect.width, 250.0);

    let (shrunk, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex } \
         .a { width: 300px } .b { width: 300px }</style>\
         <div class=flex><div class=a>a</div><div class=b>b</div></div>",
        400.0,
    );
    let items = boxes_of(&shrunk, &boxes, "div");
    assert_eq!(items[1].rect.width, 200.0, "the overflow is shared equally");
    assert_eq!(items[2].rect.width, 200.0);
}

/// `justify-content` decides where the leftover goes, and `align-items`
/// what happens across the line.
#[test]
fn justify_and_align_place_the_items() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .flex { display: flex; justify-content: center; align-items: center; height: 100px } \
         .a { width: 100px; height: 20px }</style>\
         <div class=flex><div class=a>a</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    assert_eq!(items[1].rect.x, 150.0, "centred along the row");
    assert!(items[1].rect.y > 0.0, "and centred across it");
    assert_eq!(items[1].rect.height, 20.0, "not stretched");
}

/// The default is `stretch`, which is what makes columns of equal height
/// without anyone saying how tall.
#[test]
fn items_stretch_to_the_tallest_of_them_by_default() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex } \
         .a { width: 50px; height: 80px } .b { width: 50px }</style>\
         <div class=flex><div class=a>a</div><div class=b>b</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    assert_eq!(items[2].rect.height, 80.0);
}

/// `flex-direction: column` puts the main axis down the page, and a gap goes
/// between the items on whichever axis that is.
#[test]
fn a_column_stacks_its_items_with_the_gap_between_them() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .flex { display: flex; flex-direction: column; gap: 10px } \
         .a { height: 30px } .b { height: 20px }</style>\
         <div class=flex><div class=a>a</div><div class=b>b</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    assert_eq!(items[1].rect.y, 0.0);
    assert_eq!(items[2].rect.y, 40.0, "30 tall plus the 10px gap");
}

/// `flex-wrap: wrap` puts the items that do not fit on a line of their own,
/// below the one before it.
#[test]
fn items_that_do_not_fit_wrap_onto_the_next_line() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .flex { display: flex; flex-wrap: wrap } \
         .a { width: 120px; height: 20px }</style>\
         <div class=flex><div class=a>1</div><div class=a>2</div>\
         <div class=a>3</div><div class=a>4</div></div>",
        300.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    // Two fit on a 300px line, the other two go below.
    assert_eq!(items[1].rect.y, items[2].rect.y);
    assert_eq!(items[3].rect.x, 0.0, "the third starts a line");
    assert!(items[3].rect.y >= items[1].rect.bottom());
    assert_eq!(items[3].rect.y, items[4].rect.y);
}

/// `relative` moves a box and nothing else: the space it left stays where it
/// was, so its neighbours do not shift.
#[test]
fn relative_moves_the_box_and_not_its_neighbours() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } p { margin: 0 } \
         .moved { position: relative; left: 20px; top: 10px }</style>\
         <p class=moved>one</p><p>two</p>",
        400.0,
    );
    let paragraphs = boxes_of(&tree, &boxes, "p");
    assert_eq!(paragraphs[0].rect.x, 20.0);
    assert_eq!(paragraphs[0].rect.y, 10.0);
    assert_eq!(
        paragraphs[1].rect.y, paragraphs[0].rect.height,
        "the second is where it always was"
    );
    assert_eq!(paragraphs[1].rect.x, 0.0);
}

/// An absolutely positioned box leaves the flow: the boxes after it stack as
/// though it were not there, and it is placed against its containing block.
#[test]
fn absolute_leaves_the_flow_and_measures_from_its_ancestor() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } p { margin: 0 } \
         .frame { position: relative; height: 200px } \
         .pinned { position: absolute; right: 10px; bottom: 20px; width: 50px; height: 30px }</style>\
         <div class=frame><p>text</p><div class=pinned>x</div></div>",
        400.0,
    );

    let pinned = boxes_of(&tree, &boxes, "div")
        .into_iter()
        .find(|fragment| fragment.rect.width == 50.0)
        .expect("the pinned box");
    assert_eq!(pinned.rect.x, 340.0, "ten from the right edge");
    assert_eq!(pinned.rect.y, 150.0, "twenty up from a 200px frame");

    let paragraph = rect_of(&tree, &boxes, "p");
    assert_eq!(paragraph.y, 0.0, "the flow did not notice it");
}

/// Both insets given is a width: the box stretches between them.
#[test]
fn two_insets_stretch_an_absolute_box_between_them() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .frame { position: relative } \
         .wide { position: absolute; left: 30px; right: 30px }</style>\
         <div class=frame><div class=wide>x</div></div>",
        400.0,
    );
    let wide = boxes_of(&tree, &boxes, "div")
        .into_iter()
        .find(|fragment| fragment.rect.x == 30.0)
        .expect("the stretched box");
    assert_eq!(wide.rect.width, 340.0);
}

/// A fixed box is placed against the viewport and marked as not scrolling with
/// the page, which is what paint reads to leave it where it is.
#[test]
fn fixed_measures_against_the_viewport_and_does_not_scroll() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .bar { position: fixed; left: 0; bottom: 0; height: 40px; width: 100px }</style>\
         <div class=bar>bar</div>",
        400.0,
    );
    let bar = boxes_of(&tree, &boxes, "div");
    assert_eq!(bar[0].rect.y, 560.0, "forty up from a 600px viewport");
    assert!(bar[0].fixed);
    assert!(
        bar[0].children.iter().all(|child| child.fixed),
        "what is inside it does not scroll either"
    );
}

/// A float with no width of its own is as wide as its content, not as wide as
/// the column it sits in.
#[test]
fn a_float_with_no_width_shrinks_to_fit() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .f { float: left }</style>\
         <div class=f>short</div><p>text beside it</p>",
        400.0,
    );
    let float = rect_of(&tree, &boxes, "div");
    assert!(float.width > 0.0);
    assert!(
        float.width < 200.0,
        "a floated word took {}px of a 400px column",
        float.width
    );
}

/// An item may be shrunk, but not past the point where its own content spills
/// out of it: the automatic minimum size.
#[test]
fn a_flex_item_is_not_shrunk_below_its_content() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .flex { display: flex } \
         .a { width: 400px } .b { width: 400px }</style>\
         <div class=flex><div class=a>an unbreakable-looking phrase</div>\
         <div class=b>another phrase</div></div>",
        200.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    let widest_word = 20.0;
    assert!(
        items[1].rect.width > widest_word,
        "shrunk to {}px, which is past its content",
        items[1].rect.width
    );
}

/// A box that cuts its contents off is a formatting context of its own: a float
/// inside it does not shorten the lines outside it.
#[test]
fn a_clipping_box_keeps_its_floats_to_itself() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } p { margin: 0 } \
         .card { overflow: hidden; height: 40px } \
         .f { float: left; width: 200px; height: 100px }</style>\
         <div class=card><div class=f>f</div></div><p>text</p>",
        400.0,
    );
    let paragraph = boxes_of(&tree, &boxes, "p");
    let line = paragraph[0]
        .children
        .iter()
        .find(|child| matches!(child.kind, FragmentKind::Line))
        .expect("a line");
    assert_eq!(line.rect.x, 0.0, "the float reached out of the box");
}

/// `fr` shares out what is left after the fixed tracks and the gaps.
#[test]
fn grid_columns_take_their_share_of_the_row() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: 100px 1fr 1fr; gap: 20px }</style>\
         <div class=grid><div>a</div><div>b</div><div>c</div></div>",
        520.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    // The container is first; then the three cells.
    assert_eq!(items[1].rect.width, 100.0);
    // 520 - 100 - two 20px gaps = 380, halved.
    assert_eq!(items[2].rect.width, 190.0);
    assert_eq!(items[3].rect.width, 190.0);
    assert_eq!(items[2].rect.x, 120.0, "after the first column and its gap");
    assert_eq!(items[3].rect.x, 330.0);
}

/// Items fill a row before starting the next one, and a row is as tall as the
/// tallest thing in it — which is what makes a grid of cards line up.
#[test]
fn grid_items_wrap_into_rows_of_equal_height() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: 1fr 1fr } \
         .tall { height: 60px }</style>\
         <div class=grid><div class=tall>a</div><div>b</div><div>c</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    assert_eq!(
        items[1].rect.y, items[2].rect.y,
        "the first two share a row"
    );
    assert_eq!(
        items[2].rect.height, 60.0,
        "the shorter one is stretched to the row"
    );
    assert_eq!(items[3].rect.y, 60.0, "the third starts the next row");
    assert_eq!(items[3].rect.x, 0.0);
}

/// `grid-template-rows` gives a row a height of its own, whatever is in it.
#[test]
fn a_template_row_takes_the_height_it_asks_for() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: 1fr; \
         grid-template-rows: 80px 30px }</style>\
         <div class=grid><div>a</div><div>b</div></div>",
        300.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    assert_eq!(items[1].rect.height, 80.0);
    assert_eq!(items[2].rect.y, 80.0);
    assert_eq!(items[2].rect.height, 30.0);
}

/// An item can be given a line to sit on and a number of tracks to cover, and
/// the rest are placed around it.
#[test]
fn a_grid_item_can_be_placed_on_a_line_and_span_tracks() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: 100px 100px 100px } \
         .wide { grid-column: span 2 } .last { grid-column: 3 }</style>\
         <div class=grid><div class=wide>wide</div><div class=last>last</div>\
         <div>after</div></div>",
        300.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    assert_eq!(items[1].rect.x, 0.0);
    assert_eq!(items[1].rect.width, 200.0, "two tracks and the gap between");
    assert_eq!(items[2].rect.x, 200.0, "the third line is the third column");
    assert_eq!(
        items[2].rect.y, items[1].rect.y,
        "and it fits on the first row"
    );
    assert_eq!(items[3].rect.x, 0.0, "the next item starts a new row");
    assert!(items[3].rect.y > items[1].rect.y);
}

/// Auto-placement does not go backwards: a cell left free by an item placed
/// further along stays free, because filling it is what `grid-auto-flow: dense`
/// is for and nobody asked for it.
#[test]
fn auto_placement_leaves_the_gaps_it_stepped_over() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: repeat(4, 100px) } \
         .wide { grid-column: span 2 } .last { grid-column: 4 }</style>\
         <div class=grid><div class=wide>wide</div><div class=last>last</div>\
         <div>after</div></div>",
        400.0,
    );
    let items = boxes_of(&tree, &boxes, "div");
    let after = items[3].rect;
    assert_eq!(after.x, 0.0);
    assert!(
        after.y > items[1].rect.y,
        "it filled the free cell in the first row instead of starting a new one"
    );
}

/// `repeat(auto-fill, …)` puts in as many tracks as the container has room for,
/// which is what makes a card grid answer to its width without a media query.
#[test]
fn auto_fill_puts_in_as_many_tracks_as_fit() {
    let wide = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: repeat(auto-fill, 100px) }</style>\
         <div class=grid><div>a</div><div>b</div><div>c</div></div>",
        320.0,
    );
    let items = boxes_of(&wide.0, &wide.1, "div");
    assert_eq!(items[1].rect.y, items[2].rect.y, "three fit across 320px");
    assert_eq!(items[2].rect.y, items[3].rect.y);

    let narrow = laid_out(
        "<style>body { margin: 0 } \
         .grid { display: grid; grid-template-columns: repeat(auto-fill, 100px) }</style>\
         <div class=grid><div>a</div><div>b</div><div>c</div></div>",
        220.0,
    );
    let items = boxes_of(&narrow.0, &narrow.1, "div");
    assert_eq!(items[1].rect.y, items[2].rect.y, "two fit across 220px");
    assert!(items[3].rect.y > items[1].rect.y, "and the third wraps");
}

/// Two margins that meet make one gap, the larger of them.
#[test]
fn margins_between_siblings_collapse() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         .a { margin-bottom: 30px } .b { margin-top: 10px }</style>\
         <div class=a>one</div><div class=b>two</div>",
        400.0,
    );
    let divs = boxes_of(&tree, &boxes, "div");
    let gap = divs[1].rect.y - divs[0].rect.bottom();
    assert!((gap - 30.0).abs() < 0.01, "gap was {gap}");
}

/// A margin passes through an edge with nothing on it, and is stopped by a
/// border or a padding.
#[test]
fn a_margin_escapes_an_open_edge_and_not_a_closed_one() {
    let open = laid_out(
        "<style>body { margin: 0 } section { margin: 0 } \
         p { margin: 40px 0 }</style><section><p>x</p></section>",
        400.0,
    );
    let section = rect_of(&open.0, &open.1, "section");
    let paragraph = rect_of(&open.0, &open.1, "p");
    assert_eq!(section.y, 40.0, "the child's margin moved the parent");
    assert_eq!(paragraph.y, section.y, "and they start together");

    let closed = laid_out(
        "<style>body { margin: 0 } section { margin: 0; border-top: 1px solid black } \
         p { margin: 40px 0 }</style><section><p>x</p></section>",
        400.0,
    );
    let section = rect_of(&closed.0, &closed.1, "section");
    let paragraph = rect_of(&closed.0, &closed.1, "p");
    assert_eq!(section.y, 0.0, "a border keeps the margin inside");
    assert_eq!(paragraph.y, 41.0, "below the border, by its own margin");
}

/// A negative margin pulls a box back over what is above it, which is what
/// makes the two rules — larger of two positives, sum across signs — worth
/// stating apart.
#[test]
fn a_negative_margin_pulls_the_next_box_up() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } .a { margin-bottom: 20px } \
         .b { margin-top: -8px }</style><div class=a>one</div><div class=b>two</div>",
        400.0,
    );
    let divs = boxes_of(&tree, &boxes, "div");
    let gap = divs[1].rect.y - divs[0].rect.bottom();
    assert!((gap - 12.0).abs() < 0.01, "gap was {gap}");
}

/// The pattern nearly every readable page is built on: a column held to a
/// measure and centred, with no width of its own.
#[test]
fn max_width_and_auto_margins_centre_a_column() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } \
         div { max-width: 300px; margin: 0 auto }</style><div>x</div>",
        800.0,
    );
    let column = rect_of(&tree, &boxes, "div");
    assert_eq!(column.width, 300.0);
    assert_eq!(column.x, 250.0);
}

/// A maximum below the width asked for wins, and a minimum below the maximum
/// does not.
#[test]
fn min_and_max_hold_a_width_between_them() {
    let capped = laid_out(
        "<style>body { margin: 0 } div { width: 600px; max-width: 200px }</style><div>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&capped.0, &capped.1, "div").width, 200.0);

    let floored = laid_out(
        "<style>body { margin: 0 } div { width: 50px; min-width: 120px }</style><div>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&floored.0, &floored.1, "div").width, 120.0);

    // A minimum larger than the maximum wins, because the minimum is applied
    // last.
    let both = laid_out(
        "<style>body { margin: 0 } \
         div { width: 400px; max-width: 100px; min-width: 300px }</style><div>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&both.0, &both.1, "div").width, 300.0);
}

/// `min-height` makes a box taller than its content, which is how a page's
/// footer stays at the bottom of a short page.
#[test]
fn min_height_makes_a_box_taller_than_its_content() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } div { min-height: 400px }</style><div>x</div>",
        800.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").height, 400.0);
}

/// `margin: 0 auto` on a box with a width is how a page is centred, and the
/// one place two `auto` values mean "share out what is left over".
#[test]
fn two_auto_margins_centre_a_box_with_a_width() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } div { width: 200px; margin: 0 auto }</style><div>x</div>",
        400.0,
    );
    let div = rect_of(&tree, &boxes, "div");
    assert_eq!(div.x, 100.0);
    assert_eq!(div.width, 200.0);
}

/// The same declaration inside a flex container, which is how a modern page
/// centres itself: a column of `display: flex` whose content is one child
/// with a width and `margin: 0 auto`. Handing that space to `align-items`
/// instead leaves the whole page against the left edge.
#[test]
fn two_auto_margins_centre_a_flex_item_across_the_line() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; flex-direction: column }\
         div { width: 200px; margin: 0 auto }</style><main><div>x</div></main>",
        400.0,
    );
    let div = rect_of(&tree, &boxes, "div");
    assert_eq!(div.x, 100.0);
    assert_eq!(div.width, 200.0);
}

/// And one of them, which is the *push it to the far side* idiom across the
/// line rather than along it.
#[test]
fn one_auto_cross_margin_pushes_a_flex_item_to_the_far_side() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; flex-direction: column }\
         div { width: 200px; margin-left: auto }</style><main><div>x</div></main>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").x, 200.0);
}

/// An `auto` margin across the line also stops the stretch: the free space is
/// the margin's, so there is none left to grow into.
#[test]
fn an_auto_cross_margin_stops_a_flex_item_stretching() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex; flex-direction: column }\
         div { margin-left: auto }</style><main><div>x</div></main>",
        400.0,
    );
    let div = rect_of(&tree, &boxes, "div");
    assert!(div.width < 400.0, "it stretched anyway: {}", div.width);
}

/// A positioned box inside a flex container is not a flex item: it takes no
/// room on the line and its siblings lay out as though it were not there.
#[test]
fn an_absolutely_positioned_child_is_not_a_flex_item() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } main { display: flex } \
         a { position: absolute; top: 0; width: 50px } \
         p { width: 100px; margin: 0 }</style>\
         <main><a>a</a><p>b</p></main>",
        400.0,
    );
    assert_eq!(
        rect_of(&tree, &boxes, "p").x,
        0.0,
        "the item was laid out after the positioned box instead of in its place"
    );
}

/// One `auto` margin takes the whole of the leftover, which pushes a box to an
/// edge without anything having to know how wide the page is.
#[test]
fn one_auto_margin_pushes_the_box_to_the_other_edge() {
    let (tree, boxes) = laid_out(
        "<style>body { margin: 0 } div { width: 200px; margin-left: auto }</style><div>x</div>",
        400.0,
    );
    assert_eq!(rect_of(&tree, &boxes, "div").x, 200.0);
}

#[test]
fn text_align_moves_the_line_within_the_block() {
    let start = laid_out("<style>body{margin:0}</style><p>x</p>", 400.0).0;
    let centre = laid_out(
        "<style>body{margin:0} p{text-align:center}</style><p>x</p>",
        400.0,
    )
    .0;
    let end = laid_out(
        "<style>body{margin:0} p{text-align:right}</style><p>x</p>",
        400.0,
    )
    .0;

    assert_eq!(first_line(&start).x, 0.0);
    assert!(first_line(&centre).x > first_line(&start).x);
    assert!(first_line(&centre).x < first_line(&end).x);
    assert!(
        first_line(&end).x > 380.0,
        "a right-aligned line ends at the edge"
    );
}
