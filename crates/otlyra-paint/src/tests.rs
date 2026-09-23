//! The display list a whole page comes out as, tested from the HTML and CSS that
//! asked for it rather than from fragments built by hand: paint order, stacking,
//! groups, scrolling, pictures, shadows and what a click is tested against.

use otlyra_gfx::{PaintOp, RecordingPainter, render};
use otlyra_layout::{Viewport, build_box_tree, layout};
use otlyra_text::TextEngine;

use super::scrollbar::SCROLLBAR_THUMB;
use super::*;

fn page(html: &str, scroll_y: f32) -> DisplayList {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    let mut boxes = build_box_tree(&parsed.document);
    let mut text = TextEngine::isolated();
    let fragments = layout(
        &mut boxes,
        &mut text,
        Viewport {
            width: 800.0,
            height: 600.0,
        },
    );
    build_display_list(&fragments, (800.0, 600.0), scroll_y)
}

/// A page with one picture in it, at `width` by `height` logical pixels.
fn page_with_image(style: &str, pixels: (u32, u32)) -> DisplayList {
    let html = format!("<style>body {{ margin: 0 }} {style}</style><img src=a.png>");
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    let image = otlyra_gfx::peniko::ImageData {
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![
            0u8;
            pixels.0 as usize
                * pixels.1 as usize
                * 4
        ])),
        format: otlyra_gfx::peniko::ImageFormat::Rgba8,
        alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
        width: pixels.0,
        height: pixels.1,
    };
    let styles = otlyra_css::cascade::style_document(
        &parsed.document,
        otlyra_css::cascade::Viewport {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let images: otlyra_layout::Images =
        otlyra_layout::image_sources(&parsed.document, otlyra_css::cascade::Viewport::default())
            .into_iter()
            .map(|source| (source.node, otlyra_layout::Picture::new(image.clone())))
            .collect();
    let mut boxes =
        otlyra_layout::build_box_tree_with_images(&parsed.document, Some(&styles), &images);
    let mut text = TextEngine::isolated();
    let fragments = layout(
        &mut boxes,
        &mut text,
        Viewport {
            width: 800.0,
            height: 600.0,
        },
    );
    build_display_list(&fragments, (800.0, 600.0), 0.0)
}

/// A page laid out with its own stylesheet, scrolled to `scroll_y`.
fn styled_page(html: &str, scroll_y: f32) -> DisplayList {
    let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
    let styles = otlyra_css::cascade::style_document(
        &parsed.document,
        otlyra_css::cascade::Viewport {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let mut boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
    let mut text = TextEngine::isolated();
    let fragments = layout(
        &mut boxes,
        &mut text,
        Viewport {
            width: 800.0,
            height: 600.0,
        },
    );
    build_display_list(&fragments, (800.0, 600.0), scroll_y)
}

/// The order the fills come out in, by colour, which is the painting order.
fn fill_order(list: &DisplayList) -> Vec<Color> {
    list.items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Solid(colour),
                ..
            } => Some(*colour),
            _ => None,
        })
        .collect()
}

/// A positioned box paints over the boxes it overlaps, whatever document order
/// says; `z-index` moves it further up, or below the flow entirely.
#[test]
fn a_positioned_box_paints_above_the_flow_and_z_index_moves_it() {
    let red = Color::from_rgb8(255, 0, 0);
    let blue = Color::from_rgb8(0, 0, 255);

    // The positioned box is written first, so document order alone would paint
    // it under the one after it.
    let over = fill_order(&styled_page(
        "<style>body { margin: 0 } div { height: 50px }              .a { position: relative; background: rgb(255, 0, 0) }              .b { background: rgb(0, 0, 255) }</style>             <div class=a></div><div class=b></div>",
        0.0,
    ));
    let (red_at, blue_at) = (
        over.iter().position(|colour| *colour == red),
        over.iter().position(|colour| *colour == blue),
    );
    assert!(
        red_at > blue_at,
        "the positioned box painted under the flow"
    );

    let under = fill_order(&styled_page(
        "<style>body { margin: 0 } div { height: 50px }              .a { position: relative; z-index: -1; background: rgb(255, 0, 0) }              .b { background: rgb(0, 0, 255) }</style>             <div class=a></div><div class=b></div>",
        0.0,
    ));
    assert!(
        under.iter().position(|colour| *colour == red)
            < under.iter().position(|colour| *colour == blue),
        "a negative z-index must paint below the flow"
    );
}

/// `z-index` orders a box against its siblings, not against the page.
///
/// A box with a large index inside a box with a small one stays under
/// everything the small one is under: the large number is compared only with
/// the numbers written inside the same positioned ancestor.
#[test]
fn a_large_index_inside_a_small_one_stays_inside_it() {
    let order = fill_order(&styled_page(
        "<style>body { margin: 0 } .box { position: absolute; width: 100px; height: 60px } \
         .outer { left: 0; top: 0; z-index: 1; background: rgb(255, 0, 0) } \
         .inner { left: 20px; top: 20px; z-index: 100; background: rgb(0, 0, 255) } \
         .beside { left: 40px; top: 10px; z-index: 2; background: rgb(0, 255, 0) }</style> \
         <div class='box outer'><div class='box inner'></div></div> \
         <div class='box beside'></div>",
        0.0,
    ));

    let at = |colour: Color| order.iter().position(|painted| *painted == colour);
    let (outer, inner, beside) = (
        at(Color::from_rgb8(255, 0, 0)),
        at(Color::from_rgb8(0, 0, 255)),
        at(Color::from_rgb8(0, 255, 0)),
    );
    assert!(
        outer < inner,
        "a box paints under what is inside it: {order:?}"
    );
    assert!(
        inner < beside,
        "an index of 100 inside a 1 is still under a 2: {order:?}"
    );
}

/// A selection is drawn behind the text it covers: the highlight is pushed
/// before the glyphs of the run it belongs to, so the letters are drawn over it.
#[test]
fn a_selection_is_drawn_under_the_text() {
    let parsed = otlyra_html::parse(b"<body><p>one two three</p>", Some("utf-8"));
    let styles = otlyra_css::cascade::style_document(
        &parsed.document,
        otlyra_css::cascade::Viewport {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let mut boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
    let mut text = TextEngine::isolated();
    let fragments = layout(
        &mut boxes,
        &mut text,
        Viewport {
            width: 800.0,
            height: 600.0,
        },
    );

    let start = otlyra_layout::selection::position_at(&fragments, 0.0, 20.0).expect("a place");
    let end = otlyra_layout::selection::position_at(&fragments, 60.0, 20.0).expect("another");
    let highlight = otlyra_layout::selection::rects(
        &fragments,
        otlyra_layout::Selection {
            anchor: start,
            focus: end,
        },
    );
    assert!(!highlight.is_empty(), "something is selected");
    let highlight: Vec<_> = highlight
        .into_iter()
        .map(|rect| (rect, Highlight::Selection))
        .collect();

    let list = build_display_list_with(
        &fragments,
        &Frame {
            viewport: (800.0, 600.0),
            highlights: &highlight,
            ..Frame::default()
        },
    );

    let mut painted = None;
    for item in list.items() {
        match item {
            DisplayItem::Fill {
                brush: Brush::Solid(colour),
                shape,
                ..
            } if *colour == SELECTION => painted = Some(shape.bounding_box()),
            DisplayItem::Glyphs { .. } => {
                assert!(
                    painted.is_some(),
                    "the glyphs were drawn before the highlight under them"
                );
                break;
            }
            _ => {}
        }
    }
    let painted = painted.expect("the highlight was drawn");
    assert!(
        painted.width() > 0.0 && painted.width() < 200.0,
        "it covers the words it was asked for and no more: {painted:?}"
    );
}

/// A transformed box draws where its transform puts it, and so does everything
/// inside it — including the region a click is tested against, which is the
/// same item with the same transform on it.
#[test]
fn a_transform_moves_the_drawing_and_what_is_tested_against_it() {
    let list = styled_page(
        "<style>body { margin: 0 } .card { width: 100px; height: 50px; \
         background: rgb(255, 0, 0) } .moved { transform: translate(200px, 100px) }</style> \
         <div class='card moved'><a href='/somewhere'>a link inside it</a></div>",
        0.0,
    );

    let red = list
        .items()
        .iter()
        .find_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Solid(colour),
                transform,
                shape,
                ..
            } if *colour == Color::from_rgb8(255, 0, 0) => {
                Some(transform.transform_rect_bbox(shape.bounding_box()))
            }
            _ => None,
        })
        .expect("the card");
    assert!(
        (red.x0 - 200.0).abs() < 0.01 && (red.y0 - 100.0).abs() < 0.01,
        "the box is drawn where the transform puts it: {red:?}"
    );

    // The link inside it is hit where it is drawn, not where it was laid out:
    // the two points land on different things, and the near one on the link.
    let drawn = otlyra_gfx::hit_test(&list, (210.0, 110.0)).expect("something is drawn there");
    let empty = otlyra_gfx::hit_test(&list, (10.0, 10.0));
    assert!(
        empty.is_none_or(|hit| hit.id != drawn.id),
        "the link is still where it was laid out rather than where it is drawn"
    );
}

/// The steps of a transform apply in the order they were written, and about
/// the box's own middle unless it says otherwise.
#[test]
fn transform_steps_apply_in_order_and_about_the_origin() {
    let corner = |css: &str| {
        let list = styled_page(
            &format!(
                "<style>body {{ margin: 0 }} .card {{ width: 100px; height: 100px; \
                 background: rgb(255, 0, 0); {css} }}</style><div class=card></div>"
            ),
            0.0,
        );
        list.items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    transform,
                    shape,
                    ..
                } if *colour == Color::from_rgb8(255, 0, 0) => {
                    Some(transform.transform_rect_bbox(shape.bounding_box()))
                }
                _ => None,
            })
            .expect("the card")
    };

    // Turned about its middle, a square keeps its middle and grows its box.
    let turned = corner("transform: rotate(45deg)");
    assert!(
        (turned.center().x - 50.0).abs() < 0.01 && (turned.center().y - 50.0).abs() < 0.01,
        "a box turns about its own middle: {turned:?}"
    );
    assert!(turned.width() > 140.0, "and its bounds grow: {turned:?}");

    // About the corner instead, the middle moves.
    let cornered = corner("transform: rotate(45deg); transform-origin: 0 0");
    assert!(
        (cornered.y0 - 0.0).abs() < 0.01 && cornered.center().y > 60.0,
        "an origin of its own turns it about that corner: {cornered:?}"
    );

    // Order matters: moving then turning is not turning then moving.
    let first = corner("transform: translate(50px, 0) rotate(30deg)");
    let second = corner("transform: rotate(30deg) translate(50px, 0)");
    assert!(
        (first.center().x - second.center().x).abs() > 1.0
            || (first.center().y - second.center().y).abs() > 1.0,
        "the steps are applied in the order written: {first:?} against {second:?}"
    );
}

/// A half-transparent box and everything in it is composited once: one layer
/// opened before the box and closed after the last thing inside it.
#[test]
fn opacity_composites_a_box_and_its_contents_as_one_group() {
    let list = styled_page(
        "<style>body { margin: 0 } .plate { width: 100px; height: 50px; \
         background: rgb(255, 0, 0) } .half { opacity: 0.5 } \
         .over { position: absolute; left: 10px; top: 10px; width: 20px; height: 20px; \
         background: rgb(0, 0, 255) }</style> \
         <div class='plate half'><div class=over></div></div><div class=plate></div>",
        0.0,
    );

    let mut depth = 0i32;
    let mut inside = Vec::new();
    let mut outside = Vec::new();
    let mut alpha = None;
    for item in list.items() {
        match item {
            DisplayItem::PushLayer { alpha: value, .. } => {
                depth += 1;
                alpha = Some(*value);
            }
            DisplayItem::PopLayer => depth -= 1,
            DisplayItem::Fill {
                brush: Brush::Solid(colour),
                ..
            } => {
                if depth > 0 {
                    inside.push(*colour);
                } else {
                    outside.push(*colour);
                }
            }
            _ => {}
        }
    }

    assert_eq!(depth, 0, "every layer opened was closed");
    assert_eq!(alpha, Some(0.5), "the group carries the box's opacity");
    assert!(
        inside.contains(&Color::from_rgb8(255, 0, 0))
            && inside.contains(&Color::from_rgb8(0, 0, 255)),
        "the box and the positioned box inside it are both in the group: {inside:?}"
    );
    assert!(
        outside.contains(&Color::from_rgb8(255, 0, 0)),
        "the opaque box after it is not: {outside:?}"
    );
}

/// A negative index inside a positioned box paints over that box's background
/// and under its content, rather than dropping below the page's flow.
#[test]
fn a_negative_index_inside_a_positioned_box_stays_inside_it() {
    let order = fill_order(&styled_page(
        "<style>body { margin: 0 } .flow { height: 80px; background: rgb(0, 255, 0) } \
         .parent { position: absolute; left: 0; top: 0; width: 200px; height: 120px; \
         background: rgb(255, 0, 0) } \
         .under { position: absolute; left: 20px; top: 20px; width: 100px; height: 60px; \
         background: rgb(0, 0, 255); z-index: -1 }</style> \
         <div class=flow></div><div class=parent><div class=under></div></div>",
        0.0,
    ));

    let at = |colour: Color| order.iter().position(|painted| *painted == colour);
    let (flow, parent, under) = (
        at(Color::from_rgb8(0, 255, 0)),
        at(Color::from_rgb8(255, 0, 0)),
        at(Color::from_rgb8(0, 0, 255)),
    );
    assert!(
        flow < parent && parent < under,
        "a negative index inside a positioned box paints over that box, \
         not under the page: {order:?}"
    );
}

/// The same for an absolutely positioned box, which takes a different path
/// through layout than a relative one.
#[test]
fn an_absolute_box_with_a_negative_z_index_paints_under_the_flow() {
    let order = fill_order(&styled_page(
        "<style>body { margin: 0 } .row { position: relative; height: 90px }              .flow { height: 90px; background: rgb(0, 0, 255) }              .under { position: absolute; left: 20px; top: 20px; width: 100px;              height: 40px; background: rgb(255, 0, 0); z-index: -1 }</style>             <div class=row><div class=flow>flow</div><div class=under>under</div></div>",
        0.0,
    ));
    let red = order.iter().position(|c| *c == Color::from_rgb8(255, 0, 0));
    let blue = order.iter().position(|c| *c == Color::from_rgb8(0, 0, 255));
    assert!(red.is_some() && blue.is_some(), "both boxes painted");
    assert!(red < blue, "the negative z-index painted over the flow");
}

/// The root element's background belongs to the canvas, not to the element:
/// it covers the viewport however short the document is, and it is not painted
/// a second time over whatever a negative `z-index` put below the flow.
#[test]
fn the_root_background_is_the_canvas_and_is_painted_once() {
    let list = styled_page(
        "<style>html { background: rgb(0, 128, 0) } \
         .below { position: relative; z-index: -1; background: rgb(255, 0, 0); \
         height: 40px }</style><div class=below>below</div>",
        0.0,
    );
    let greens: Vec<_> = list
        .items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Solid(colour),
                shape,
                ..
            } if *colour == Color::from_rgb8(0, 128, 0) => Some(shape.bounding_box()),
            _ => None,
        })
        .collect();

    assert_eq!(greens.len(), 1, "the root background was painted twice");
    assert_eq!(
        greens[0].y1, 600.0,
        "and not only as far as the content goes"
    );
    assert!(
        fill_order(&list)
            .iter()
            .position(|colour| *colour == Color::from_rgb8(255, 0, 0))
            > Some(0),
        "the box below the flow still paints over the canvas"
    );
}

/// `overflow: hidden` cuts its contents off, which reaches the rasterizer as a
/// clip layer around whatever is inside the box — and every layer pushed is
/// popped, or everything after it would be clipped too.
#[test]
fn a_clipping_box_wraps_its_contents_in_a_layer() {
    let list = styled_page(
        "<style>body { margin: 0 } \
         .card { overflow: hidden; height: 40px; width: 100px } \
         .tall { height: 200px; background: rgb(255, 0, 0) }</style>\
         <div class=card><div class=tall>tall</div></div>",
        0.0,
    );

    let mut depth = 0i32;
    let mut deepest = 0i32;
    let mut clips = Vec::new();
    for item in list.items() {
        match item {
            DisplayItem::PushLayer { clip, .. } => {
                depth += 1;
                deepest = deepest.max(depth);
                clips.push(clip.bounding_box());
            }
            DisplayItem::PopLayer => depth -= 1,
            _ => {}
        }
        assert!(depth >= 0, "a layer was popped that was never pushed");
    }

    assert_eq!(depth, 0, "a layer was left open");
    assert!(deepest > 0, "nothing was clipped");
    assert_eq!(clips[0].y1, 40.0, "cut off at the box, not at its contents");
}

/// A scrollbar says two things and nothing else: how much of the content is on
/// screen, and how far through it the reader is. Content that fits gets none.
#[test]
fn a_scrollbar_shows_where_the_reader_is_and_only_when_there_is_somewhere_to_go() {
    let thumb = |list: &DisplayList| -> Option<otlyra_gfx::kurbo::Rect> {
        list.items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Solid(colour),
                    shape,
                    ..
                } if *colour == SCROLLBAR_THUMB => Some(shape.bounding_box()),
                _ => None,
            })
            .next_back()
    };

    let short = "<style>body { margin: 0 } p { height: 100px }</style><p>short</p>";
    assert!(
        thumb(&styled_page(short, 0.0)).is_none(),
        "a page that fits was given a scrollbar"
    );

    let long = "<style>body { margin: 0 } p { height: 3000px }</style><p>long</p>";
    let at_top = thumb(&styled_page(long, 0.0)).expect("a scrollbar");
    let further = thumb(&styled_page(long, 1000.0)).expect("a scrollbar");

    assert!(
        at_top.y0.abs() < 0.01,
        "at the top of the page it is at the top"
    );
    assert!(further.y0 > at_top.y0, "it did not move with the reader");
    assert!(
        at_top.height() < 600.0 / 4.0,
        "a fifth of the content on screen should be a short thumb"
    );
    assert!(at_top.x1 <= 800.0, "it is drawn inside the viewport");
}

/// `border-radius` rounds the background and the border together, and a radius
/// larger than the box is scaled down rather than folding over itself.
#[test]
fn a_rounded_box_is_drawn_round() {
    use otlyra_gfx::kurbo::PathEl;

    let curves = |html: &str| -> usize {
        styled_page(html, 0.0)
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill { shape, .. } | DisplayItem::Stroke { shape, .. } => Some(shape),
                _ => None,
            })
            .flat_map(|shape| shape.elements())
            .filter(|element| matches!(element, PathEl::CurveTo(..) | PathEl::QuadTo(..)))
            .count()
    };

    let square = "<style>body { margin: 0 } div { background: rgb(0, 0, 255); height: 40px }                      </style><div></div>";
    let round = "<style>body { margin: 0 } div { background: rgb(0, 0, 255); height: 40px;                      border-radius: 8px }</style><div></div>";
    assert_eq!(curves(square), 0, "a square box has no curves in it");
    assert!(curves(round) > 0, "a rounded one does");

    // A pill: the radius is larger than the box and has to be scaled down, or
    // the corners would overlap and the path would fold over itself.
    let pill = "<style>body { margin: 0 } div { background: rgb(0, 0, 255); height: 40px;                     width: 100px; border-radius: 999px }</style><div></div>";
    let bounds = styled_page(pill, 0.0)
        .items()
        .iter()
        .find_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Solid(colour),
                shape,
                ..
            } if *colour == Color::from_rgb8(0, 0, 255) => Some(shape.bounding_box()),
            _ => None,
        })
        .expect("the box");
    assert_eq!(bounds.width(), 100.0, "it is still the size it was");
    assert_eq!(bounds.height(), 40.0);
}

/// A gradient background reaches the rasterizer as a gradient, with its stops
/// in order and its line pointing where CSS says.
#[test]
fn a_gradient_background_is_painted_as_one() {
    let gradient = |css: &str| {
        let html = format!(
            "<style>body {{ margin: 0 }} div {{ height: 100px; width: 200px; \
             background: {css} }}</style><div></div>"
        );
        styled_page(&html, 0.0)
            .items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Fill {
                    brush: Brush::Gradient(gradient),
                    ..
                } => Some(gradient.clone()),
                _ => None,
            })
            .expect("a gradient")
    };

    let down = gradient("linear-gradient(rgb(255, 0, 0), rgb(0, 0, 255))");
    assert_eq!(down.stops.len(), 2);
    assert_eq!(down.stops[0].offset, 0.0);
    assert_eq!(down.stops[1].offset, 1.0);
    let otlyra_gfx::peniko::GradientKind::Linear(line) = down.kind else {
        panic!("a linear gradient");
    };
    assert!(line.end.y > line.start.y, "the default runs down the box");
    assert!(
        (line.start.x - line.end.x).abs() < 0.01,
        "and straight down, not across"
    );

    let across = gradient("linear-gradient(to right, rgb(255, 0, 0), rgb(0, 0, 255))");
    let otlyra_gfx::peniko::GradientKind::Linear(line) = across.kind else {
        panic!("a linear gradient");
    };
    assert!(line.end.x > line.start.x, "to right runs across the box");
    assert!((line.start.y - line.end.y).abs() < 0.01);

    // Stops without a position are spread evenly.
    let three = gradient("linear-gradient(rgb(255,0,0), rgb(0,255,0), rgb(0,0,255))");
    assert_eq!(three.stops.len(), 3);
    assert!((three.stops[1].offset - 0.5).abs() < 0.001);
}

/// A shadow is drawn behind the box that casts it, offset and blurred as the
/// page asked, and a spread grows its corners with it.
#[test]
fn a_box_shadow_is_cast_behind_the_box() {
    let list = styled_page(
        "<style>body { margin: 0 } \
         div { height: 40px; width: 100px; background: rgb(0, 128, 0); \
         border-radius: 6px; box-shadow: 4px 8px 12px 2px rgb(0, 0, 0) }</style>\
         <div></div>",
        0.0,
    );

    let (blur, bounds) = list
        .items()
        .iter()
        .find_map(|item| match item {
            DisplayItem::Blurred { shape, blur, .. } => Some((*blur, shape.bounding_box())),
            _ => None,
        })
        .expect("a shadow");

    assert_eq!(blur, 12.0, "the CSS radius reaches the rasterizer as it is");
    // Offset by four and eight, grown by two on every side.
    assert_eq!(bounds.x0, 2.0);
    assert_eq!(bounds.y0, 6.0);
    assert_eq!(bounds.width(), 104.0);
    assert_eq!(bounds.height(), 44.0);

    // Behind the box: the shadow comes first in the list.
    let shadow_index = list
        .items()
        .iter()
        .position(|item| matches!(item, DisplayItem::Blurred { .. }))
        .expect("a shadow");
    let background = list
        .items()
        .iter()
        .position(|item| {
            matches!(item, DisplayItem::Fill { brush: Brush::Solid(colour), .. }
                if *colour == Color::from_rgb8(0, 128, 0))
        })
        .expect("the background");
    assert!(shadow_index < background, "the shadow painted over its box");
}

/// A text shadow is the same run drawn behind itself, moved and softened.
#[test]
fn text_shadows_are_drawn_behind_the_text() {
    let list = styled_page(
        "<style>body { margin: 0 } \
         p { color: rgb(0, 0, 0); text-shadow: 2px 3px 4px rgb(255, 0, 0) }</style>\
         <p>text</p>",
        0.0,
    );

    let runs: Vec<_> = list
        .items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Glyphs {
                brush: Brush::Solid(colour),
                transform,
                blur,
                ..
            } => Some((*colour, transform.as_coeffs(), *blur)),
            _ => None,
        })
        .collect();

    assert_eq!(runs.len(), 2, "one shadow and the text itself");
    let (shadow, shadow_at, blur) = runs[0];
    let (text, text_at, text_blur) = runs[1];

    assert_eq!(
        shadow,
        Color::from_rgb8(255, 0, 0),
        "the shadow comes first"
    );
    assert_eq!(text, Color::from_rgb8(0, 0, 0));
    assert_eq!(blur, 4.0);
    assert_eq!(text_blur, 0.0, "the text itself is not blurred");
    assert_eq!(shadow_at[4] - text_at[4], 2.0, "moved right by two");
    assert_eq!(shadow_at[5] - text_at[5], 3.0, "and down by three");
}

/// The one tile a background picture is placed by: where it starts, how large
/// it is, and how far the fill that repeats it reaches.
fn background_tile(declarations: &str) -> (Rect, Rect, otlyra_gfx::peniko::ImageSampler) {
    let source = format!(
        "<style>body {{ margin: 0 }} div {{ height: 50px; width: 200px; \
         background-image: url(behind.png); {declarations} }}</style><div></div>"
    );
    let parsed = otlyra_html::parse(source.as_bytes(), Some("utf-8"));
    let styles = otlyra_css::cascade::style_document(
        &parsed.document,
        otlyra_css::cascade::Viewport {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
            text_scale: 1.0,
            color_scheme: Default::default(),
        },
    );
    let mut boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
    let mut text = TextEngine::isolated();
    let fragments = layout(
        &mut boxes,
        &mut text,
        Viewport {
            width: 800.0,
            height: 600.0,
        },
    );

    // A twenty by ten picture, so its own proportions are visible in what
    // `cover` and `contain` make of it.
    let picture = otlyra_gfx::peniko::ImageData {
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![0u8; 20 * 10 * 4])),
        format: otlyra_gfx::peniko::ImageFormat::Rgba8,
        alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
        width: 20,
        height: 10,
    };

    let list = build_display_list_with(
        &fragments,
        &Frame {
            viewport: (800.0, 600.0),
            background: Some(&|url: &str| (url == "behind.png").then(|| picture.clone())),
            ..Frame::default()
        },
    );

    list.items()
        .iter()
        .find_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Image(image),
                brush_transform: Some(transform),
                shape,
                ..
            } => {
                let coeffs = transform.as_coeffs();
                let tile = Rect::new(
                    coeffs[4] as f32,
                    coeffs[5] as f32,
                    (coeffs[0] * f64::from(image.image.width)) as f32,
                    (coeffs[3] * f64::from(image.image.height)) as f32,
                );
                let covered = shape.bounding_box();
                Some((
                    tile,
                    Rect::new(
                        covered.x0 as f32,
                        covered.y0 as f32,
                        covered.width() as f32,
                        covered.height() as f32,
                    ),
                    image.sampler,
                ))
            }
            _ => None,
        })
        .expect("a background picture")
}

/// A picture named by a rule is drawn behind the box, sized as the rule says.
#[test]
fn a_background_picture_is_drawn_behind_its_box() {
    let (tile, _, _) = background_tile("background-size: cover");
    assert_eq!(tile.width, 200.0, "cover fills the box across");
    assert_eq!(
        tile.height, 100.0,
        "and overflows it down rather than squashing"
    );

    let (tile, _, _) = background_tile("background-size: contain");
    assert_eq!(tile.width, 100.0, "contain fits inside the box");
    assert_eq!(tile.height, 50.0, "whole");
}

/// A picture tiles by default, and the fill that carries it covers the box; one
/// told not to repeat covers exactly one tile, which is what keeps a brush that
/// has no way to put nothing outside a picture from smearing its edge.
#[test]
fn a_background_picture_tiles_unless_told_not_to() {
    use otlyra_gfx::peniko::Extend;

    let (tile, covered, sampler) = background_tile("");
    assert_eq!((tile.width, tile.height), (20.0, 10.0), "its own size");
    assert_eq!(covered, Rect::new(0.0, 0.0, 200.0, 50.0), "the whole box");
    assert_eq!(sampler.x_extend, Extend::Repeat);
    assert_eq!(sampler.y_extend, Extend::Repeat);

    let (_, covered, sampler) = background_tile("background-repeat: no-repeat");
    assert_eq!(covered, Rect::new(0.0, 0.0, 20.0, 10.0), "one tile");
    assert_eq!(sampler.x_extend, Extend::Pad);

    let (_, covered, sampler) = background_tile("background-repeat: repeat-x");
    assert_eq!(
        covered,
        Rect::new(0.0, 0.0, 200.0, 10.0),
        "a band across the box"
    );
    assert_eq!(sampler.x_extend, Extend::Repeat);
    assert_eq!(sampler.y_extend, Extend::Pad);
}

/// A position moves the tile within the room the picture leaves in its box,
/// which is why a percentage is not a percentage of the box.
#[test]
fn a_background_position_moves_the_tile_by_what_is_left_over() {
    let (tile, _, _) = background_tile("background-repeat: no-repeat; background-position: 0 0");
    assert_eq!((tile.x, tile.y), (0.0, 0.0));

    // 180 across and 40 down are what a twenty by ten picture leaves.
    let (tile, _, _) =
        background_tile("background-repeat: no-repeat; background-position: 50% 50%");
    assert_eq!((tile.x, tile.y), (90.0, 20.0));

    let (tile, _, _) =
        background_tile("background-repeat: no-repeat; background-position: right bottom");
    assert_eq!((tile.x, tile.y), (180.0, 40.0));

    let (tile, _, _) =
        background_tile("background-repeat: no-repeat; background-position: right 10px top 4px");
    assert_eq!((tile.x, tile.y), (170.0, 4.0));
}

/// `round` squeezes the tile so a whole number of them fits the box, which is
/// the whole of the difference between it and `repeat`.
#[test]
fn round_fits_a_whole_number_of_tiles() {
    // A thirty-pixel tile across two hundred: seven of them at 28.57 rather
    // than six and two thirds at thirty.
    let (tile, _, _) = background_tile("background-repeat: round; background-size: 30px 10px");
    assert!(
        (tile.width - 200.0 / 7.0).abs() < 0.01,
        "tile was {} wide",
        tile.width
    );
    assert!((tile.height - 10.0).abs() < 0.01, "and untouched down");
}

/// A background is positioned against the box inside its border, however far
/// the painting itself spreads.
#[test]
fn a_background_is_positioned_inside_the_border() {
    let (tile, covered, _) = background_tile(
        "border: 5px solid black; background-repeat: no-repeat; background-position: 0 0",
    );
    assert_eq!((tile.x, tile.y), (5.0, 5.0));
    assert_eq!(covered, Rect::new(5.0, 5.0, 20.0, 10.0));
}

/// A sticky heading rides with the page, stops at its inset, and is carried off
/// the top when its section runs out.
#[test]
fn a_sticky_box_stops_at_its_inset_and_leaves_with_its_container() {
    let green = Color::from_rgb8(0, 128, 0);
    // The heading starts 60px down its section, so at rest it is below its
    // inset and has nothing to stick to yet.
    let html = "<style>body { margin: 0 }                     section { height: 400px; padding-top: 60px }                     h2 { position: sticky; top: 10px; height: 30px; margin: 0;                     background: rgb(0, 128, 0) }                     p { height: 300px; margin: 0 }</style>                    <section><h2>one</h2><p>body</p></section>                    <section><h2>two</h2><p>body</p></section>";

    let heading_tops = |scroll: f32| -> Vec<f64> {
        styled_page(html, scroll)
            .items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill { brush, shape, .. } if *brush == Brush::Solid(green) => {
                    Some(shape.bounding_box().y0)
                }
                _ => None,
            })
            .collect()
    };

    assert_eq!(heading_tops(0.0).first().copied(), Some(60.0), "at rest");
    assert_eq!(
        heading_tops(100.0).first().copied(),
        Some(10.0),
        "held at its inset while its section is still under it"
    );
    // Far enough down that the first section has nearly gone: the heading is
    // pushed back off the top rather than following the reader forever.
    assert!(
        heading_tops(430.0).first().copied().expect("the heading") < 10.0,
        "its container ran out and did not take it with it"
    );
}

/// A fixed box stays on screen while the page moves under it — which is the
/// whole of what `position: fixed` is for.
#[test]
fn a_fixed_box_does_not_move_when_the_page_scrolls() {
    let html = "<style>body { margin: 0 }                     .bar { position: fixed; top: 10px; left: 0; width: 100px; height: 20px;                     background: rgb(255, 0, 0) }                     p { height: 400px }</style>                    <div class=bar>bar</div><p>tall</p><p>tall</p>";

    let top_of = |list: &DisplayList| {
        list.items()
            .iter()
            .find_map(|item| match item {
                DisplayItem::Fill { brush, shape, .. }
                    if *brush == Brush::Solid(Color::from_rgb8(255, 0, 0)) =>
                {
                    Some(shape.bounding_box().y0)
                }
                _ => None,
            })
            .expect("the fixed bar")
    };

    assert_eq!(top_of(&styled_page(html, 0.0)), 10.0);
    assert_eq!(
        top_of(&styled_page(html, 300.0)),
        10.0,
        "it moved with the page"
    );
}

/// Where a picture actually lands: the transform is what decides its size, so
/// this maps the image's own corners through it and checks the rectangle.
fn image_rect(list: &DisplayList) -> (f64, f64, f64, f64) {
    let item = list
        .items()
        .iter()
        .find_map(|item| match item {
            DisplayItem::Image {
                image, transform, ..
            } => Some((image.width, image.height, *transform)),
            _ => None,
        })
        .expect("an image item");
    let (width, height, transform) = item;
    let origin = transform * otlyra_gfx::kurbo::Point::new(0.0, 0.0);
    let far = transform * otlyra_gfx::kurbo::Point::new(f64::from(width), f64::from(height));
    (origin.x, origin.y, far.x - origin.x, far.y - origin.y)
}

/// `object-fit` decides what happens to the picture inside the box layout
/// gave it, and never what that box is.
#[test]
fn object_fit_places_the_picture_inside_its_box() {
    // A box twice as tall as it is wide, and a picture twice as wide as it
    // is tall, so every value has something to do.
    let page = |fit: &str| {
        page_with_image(
            &format!("img {{ width: 100px; height: 200px; object-fit: {fit} }}"),
            (200, 100),
        )
    };

    assert_eq!(
        image_rect(&page("fill")),
        (0.0, 0.0, 100.0, 200.0),
        "stretched to the box, ratio abandoned"
    );
    assert_eq!(
        image_rect(&page("contain")),
        (0.0, 75.0, 100.0, 50.0),
        "as large as fits, centred in what is left"
    );
    assert_eq!(
        image_rect(&page("cover")),
        (-150.0, 0.0, 400.0, 200.0),
        "large enough to cover, and cut off by the box"
    );
    assert_eq!(
        image_rect(&page("none")),
        (-50.0, 50.0, 200.0, 100.0),
        "its own size, centred"
    );
    assert_eq!(
        image_rect(&page("scale-down")),
        (0.0, 75.0, 100.0, 50.0),
        "which here is `contain`, because its own size does not fit"
    );
}

/// A picture larger than its box is cut off at the box rather than spilling
/// over whatever is drawn next — and the rectangle that does the cutting is
/// in the picture's own pixels, because that is the space the clip is
/// applied in.
#[test]
fn a_picture_that_overflows_its_box_is_clipped_to_it() {
    let list = page_with_image(
        "img { width: 100px; height: 100px; object-fit: none }",
        (200, 100),
    );
    let clip = list
        .items()
        .iter()
        .find_map(|item| match item {
            DisplayItem::Image { clip_rect, .. } => *clip_rect,
            _ => None,
        })
        .expect("a clip");
    assert_eq!((clip.x0, clip.x1), (50.0, 150.0), "the middle hundred");
    assert_eq!((clip.y0, clip.y1), (0.0, 100.0), "and all of the height");
}

/// `object-position` moves what is left of the picture inside the box, with
/// the same arithmetic a background's position uses.
#[test]
fn object_position_moves_the_picture_in_its_box() {
    let (x, y, width, height) = image_rect(&page_with_image(
        "img { width: 100px; height: 200px; object-fit: contain; \
         object-position: 0 100% }",
        (200, 100),
    ));
    assert_eq!((x, y), (0.0, 150.0), "at the bottom rather than the middle");
    assert_eq!((width, height), (100.0, 50.0));
}

/// A picture asked for at a size is drawn at that size, whatever size its file
/// is: the scale in the transform is the only thing that decides it.
#[test]
fn a_picture_is_drawn_at_the_size_the_page_asked_for() {
    let (x, _, width, height) = image_rect(&page_with_image(
        "img { width: 200px; height: 100px }",
        (64, 64),
    ));
    assert_eq!(x, 0.0);
    assert_eq!((width, height), (200.0, 100.0));

    let (_, _, width, height) = image_rect(&page_with_image("img { width: 320px }", (160, 80)));
    assert_eq!((width, height), (320.0, 160.0), "the ratio is kept");

    let (_, _, width, height) = image_rect(&page_with_image("", (48, 24)));
    assert_eq!((width, height), (48.0, 24.0), "and its own size otherwise");
}

fn ops(list: &DisplayList) -> Vec<PaintOp> {
    let mut painter = RecordingPainter::new();
    render(list, &mut painter);
    painter.take()
}

#[test]
fn a_page_paints_its_background_first_and_then_its_text() {
    let ops = ops(&page("<body><p>hello", 0.0));
    assert!(matches!(ops.first(), Some(PaintOp::Fill { .. })));
    assert!(
        ops.iter()
            .any(|op| matches!(op, PaintOp::DrawGlyphs { .. })),
        "the text has to reach the seam"
    );
}

#[test]
fn scrolling_moves_the_text_up_by_exactly_the_scroll_offset() {
    let unscrolled = ops(&page("<body><p>hello", 0.0));
    let scrolled = ops(&page("<body><p>hello", 5.0));

    let y = |ops: &[PaintOp]| {
        ops.iter()
            .find_map(|op| match op {
                PaintOp::DrawGlyphs { transform, .. } => Some(transform.as_coeffs()[5]),
                _ => None,
            })
            .expect("some text")
    };
    assert!((y(&unscrolled) - y(&scrolled) - 5.0).abs() < 0.01);
}

#[test]
fn a_link_is_painted_in_the_ua_stylesheets_blue() {
    let ops = ops(&page("<body><p><a>link</a>", 0.0));
    let PaintOp::DrawGlyphs { brush, .. } = ops
        .iter()
        .find(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
        .expect("the link text")
    else {
        unreachable!("filtered above")
    };
    assert_eq!(*brush, Brush::Solid(Color::from_rgb8(0, 0, 0xee)));
}

#[test]
fn off_screen_content_produces_no_items() {
    let html = "<body>".to_owned() + &"<p>a paragraph</p>".repeat(400);
    let all = page(&html, 0.0);
    // A screenful is some tens of items; four hundred paragraphs would be an
    // order of magnitude more.
    assert!(
        all.len() < 100,
        "only the visible paragraphs should be painted, got {} items",
        all.len()
    );
}

#[test]
fn an_empty_document_still_paints_the_canvas() {
    let list = page("", 0.0);
    let ops = ops(&list);
    assert_eq!(ops.len(), 1, "one fill and nothing else to draw");
    assert!(matches!(ops[0], PaintOp::Fill { .. }));

    // The empty `<html>` and `<body>` are still hit-testable — a click on blank
    // space lands on the document, not on nothing.
    assert!(
        list.items()
            .iter()
            .any(|item| matches!(item, DisplayItem::HitTest { .. }))
    );
}

/// Every text run is its own target. A link that is clickable across the whole
/// line it happens to sit on is worse than no hit testing.
#[test]
fn each_text_run_gets_its_own_target() {
    let list = page("<body><p>before <a href=\"/x\">link</a> after", 0.0);
    let targets: Vec<_> = list
        .items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::HitTest { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();

    // html, body, p, and one per run.
    assert!(targets.len() >= 6, "got {} targets", targets.len());
    let runs: Vec<_> = targets.iter().filter(|rect| rect.width() < 700.0).collect();
    assert!(runs.len() >= 3, "one target per run on the line");
    for pair in runs.windows(2) {
        assert!(
            pair[1].x0 >= pair[0].x1 - 0.5,
            "run targets must not overlap: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}
