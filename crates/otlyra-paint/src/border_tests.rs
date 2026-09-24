//! Borders, and the other things drawn round a box's edge — its background layers
//! and its inset shadows — tested from the styled page they come from.

use otlyra_css::cascade::{Viewport as StyleViewport, style_document};
use otlyra_layout::{Viewport, build_box_tree, layout};
use otlyra_text::TextEngine;

use super::border::shades;
use super::*;

/// The display list for a styled document, at a fixed viewport.
fn list_for(html: &str) -> DisplayList {
    let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
    let styles = style_document(&document, StyleViewport::default());
    let mut boxes = build_box_tree(&document, &styles);
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

/// Every rectangle filled in `colour`, as (x0, y0, x1, y1).
///
/// By colour, because a page always paints a canvas and a body background too,
/// and a test about borders should not count them.
fn rects(list: &DisplayList, colour: Color) -> Vec<[f64; 4]> {
    list.items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill {
                shape,
                brush: Brush::Solid(fill),
                ..
            } if *fill == colour => {
                let bounds = shape.bounding_box();
                Some([bounds.x0, bounds.y0, bounds.x1, bounds.y1])
            }
            _ => None,
        })
        .collect()
}

const RED: Color = Color::from_rgb8(255, 0, 0);
const BLUE: Color = Color::from_rgb8(0, 0, 255);

/// Four sides, four rectangles, each the width it was asked for.
#[test]
fn each_border_side_is_painted_at_its_own_width() {
    let list = list_for(
        "<style>body { margin: 0 } div { border-top: 4px solid red; \
         border-left: 10px solid blue }</style><div>text</div>",
    );
    let top = rects(&list, RED);
    assert_eq!(top.len(), 1, "expected one red side, got {top:?}");
    assert_eq!(top[0], [0.0, 0.0, 800.0, 4.0]);

    let left = rects(&list, BLUE);
    assert_eq!(left.len(), 1, "expected one blue side, got {left:?}");
    assert_eq!(left[0][0], 0.0);
    assert_eq!(left[0][2], 10.0);
}

/// A border whose style is `none` is zero wide however wide it was declared,
/// so nothing is drawn and nothing moves.
#[test]
fn a_border_with_no_style_paints_nothing() {
    let list =
        list_for("<style>body { margin: 0 } div { border: 10px none red }</style><div>text</div>");
    assert!(
        rects(&list, RED).is_empty(),
        "a border with no style was painted"
    );
}

/// Every `border-style` draws its own line, and each is a different shape of
/// display item — a fill for the ones that are continuous, a dashed stroke
/// for the ones that are not.
///
/// Every number here was read off a reference: a double border is two lines a
/// rounded third of the width each, a dash is twice the width and a dot is a
/// round cap the width across.
#[test]
fn a_border_style_decides_what_is_drawn() {
    let fills = |css: &str| {
        let list = list_for(&format!(
            "<style>body {{ margin: 0 }} div {{ width: 100px; height: 20px; \
             border: 9px {css} red }}</style><div></div>"
        ));
        rects(&list, RED)
    };
    let strokes = |css: &str| {
        let list = list_for(&format!(
            "<style>body {{ margin: 0 }} div {{ width: 100px; height: 20px; \
             border: 9px {css} red }}</style><div></div>"
        ));
        list.items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Stroke { style, .. } => {
                    Some((style.width, style.dash_pattern.to_vec()))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(fills("solid").len(), 4, "one shape a side");
    // Two lines a side, each a third of the width.
    let double = fills("double");
    assert_eq!(double.len(), 8, "two lines a side: {double:?}");
    assert!(
        (double[0][3] - double[0][1] - 3.0).abs() < 0.01,
        "a third of nine is three: {:?}",
        double[0]
    );
    // Half the width each way. Red's brightest channel is already full, so
    // lightening it changes nothing: the lit half is the red the page named and
    // the other half is not.
    assert_eq!(fills("groove").len(), 4, "one lit half a side");
    assert_eq!(fills("inset").len(), 2, "two sides lit, two in shadow");

    let dashed = strokes("dashed");
    assert_eq!(dashed.len(), 4, "one stroke a side: {dashed:?}");
    assert_eq!(dashed[0].0, 9.0);
    assert_eq!(dashed[0].1[0], 18.0, "a dash is twice the width");
    let dotted = strokes("dotted");
    assert_eq!(dotted[0].1[0], 0.0, "a dot is a cap on nothing");
    assert_eq!(dotted[0].1[1], 18.0, "one every two widths");

    // `hidden` is zero wide, so there is nothing to draw and nothing to stroke.
    assert!(fills("hidden").is_empty() && strokes("hidden").is_empty());
}

/// A three-dimensional border darkens the side the shadow falls on and lightens
/// the other — except at the two ends of the scale, where there is nowhere
/// further to go.
///
/// Every pair is what Chrome paints an `outset` border in that colour, read off
/// its pixels.
#[test]
fn a_carved_border_darkens_one_side_and_lightens_the_other() {
    let pair = |r, g, b| {
        let (dark, light) = shades(Color::from_rgb8(r, g, b));
        let bytes = |colour: Color| {
            let [r, g, b, _] = colour.to_rgba8().to_u8_array();
            [r, g, b]
        };
        (bytes(dark), bytes(light))
    };

    // Truncated as the reference truncates: rounding would make these 64 and 128.
    assert_eq!(pair(48, 96, 192), ([27, 54, 108], [63, 127, 255]));
    assert_eq!(pair(128, 128, 128), ([44, 44, 44], [212, 212, 212]));
    assert_eq!(pair(255, 0, 0), ([171, 0, 0], [255, 0, 0]));
    assert_eq!(pair(48, 48, 48), ([0, 0, 0], [132, 132, 132]));

    // Too light to lighten: the lit side is the colour itself — past the edge,
    // and not on it.
    assert_eq!(pair(255, 255, 255), ([171, 171, 171], [255, 255, 255]));
    assert_eq!(pair(0xec, 0xec, 0xec), ([152, 152, 152], [236, 236, 236]));
    assert_eq!(pair(0xeb, 0xeb, 0xeb), ([151, 151, 151], [255, 255, 255]));

    // Too dark to darken: lightened once for the shadow and twice for the light,
    // on the edge as well as past it — so black is two greys.
    assert_eq!(pair(0x21, 0x21, 0x21), ([0, 0, 0], [117, 117, 117]));
    assert_eq!(pair(0x20, 0x20, 0x20), ([116, 116, 116], [200, 200, 200]));
    assert_eq!(pair(10, 20, 0), ([52, 104, 0], [94, 188, 0]));
    assert_eq!(pair(0, 0, 0), ([84, 84, 84], [168, 168, 168]));

    // Alpha is carried through, and plays no part in how dark a colour is.
    let (dark, light) = shades(Color::from_rgba8(0, 0, 0, 128));
    assert_eq!(dark.to_rgba8().to_u8_array(), [84, 84, 84, 128]);
    assert_eq!(light.to_rgba8().to_u8_array(), [168, 168, 168, 128]);
}

/// A box may have several backgrounds, each placed and sized by its own
/// values, and they are painted in the reverse of the order they were written
/// — so the first one a page names is the one on top.
#[test]
fn every_background_layer_is_drawn_in_the_order_it_was_written() {
    let one = otlyra_gfx::peniko::ImageData {
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![0u8; 10 * 10 * 4])),
        format: otlyra_gfx::peniko::ImageFormat::Rgba8,
        alpha_type: otlyra_gfx::peniko::ImageAlphaType::AlphaPremultiplied,
        width: 10,
        height: 10,
    };
    let two = otlyra_gfx::peniko::ImageData {
        width: 20,
        height: 20,
        data: otlyra_gfx::peniko::Blob::new(std::sync::Arc::new(vec![0u8; 20 * 20 * 4])),
        ..one.clone()
    };

    let document = otlyra_html::parse(
        b"<style>body { margin: 0 } div { width: 100px; height: 100px; \
          background-image: url(https://x.test/top.png), url(https://x.test/bottom.png); \
          background-repeat: no-repeat; \
          background-position: left top, right bottom }</style><div></div>",
        Some("utf-8"),
    )
    .document;
    let styles = style_document(&document, StyleViewport::default());
    let mut boxes = build_box_tree(&document, &styles);
    let mut text = TextEngine::isolated();
    let fragments = layout(
        &mut boxes,
        &mut text,
        Viewport {
            width: 800.0,
            height: 600.0,
        },
    );
    let list = build_display_list_with(
        &fragments,
        &Frame {
            viewport: (800.0, 600.0),
            background: Some(&|url: &str| match url {
                "https://x.test/top.png" => Some(one.clone()),
                "https://x.test/bottom.png" => Some(two.clone()),
                _ => None,
            }),
            ..Frame::default()
        },
    );

    // The layer written second is drawn first, so the one written first ends
    // up over it.
    let drawn: Vec<(u32, f32, f32)> = list
        .items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill {
                brush: Brush::Image(image),
                brush_transform: Some(transform),
                ..
            } => {
                let coeffs = transform.as_coeffs();
                Some((image.image.width, coeffs[4] as f32, coeffs[5] as f32))
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        drawn,
        vec![(20, 80.0, 80.0), (10, 0.0, 0.0)],
        "the second layer at the far corner first, then the first at the near one"
    );
}

/// An inset shadow is drawn over the background and clipped to the padding
/// box, as a shape with the lit part cut out of it — one blur rather than one
/// per edge, which would darken where the edges overlap.
#[test]
fn an_inset_shadow_is_a_hole_clipped_to_the_padding_box() {
    let list = list_for(
        "<style>body { margin: 0 } div { width: 100px; height: 60px; \
         border: 5px solid black; box-shadow: inset 0 0 0 10px red }</style><div></div>",
    );

    let blurred: Vec<KurboRect> = list
        .items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Blurred { shape, brush, .. } if *brush == Brush::Solid(RED) => {
                Some(shape.bounding_box())
            }
            _ => None,
        })
        .collect();
    assert_eq!(blurred.len(), 1, "one blurred shape: {blurred:?}");
    assert!(
        blurred[0].x0 < 5.0 && blurred[0].x1 > 105.0,
        "the shape reaches out past the box and is clipped back to it: {:?}",
        blurred[0]
    );

    // Clipped to the padding box, which is the border box less the border.
    let clip = list.items().iter().find_map(|item| match item {
        DisplayItem::PushLayer { clip, .. } => Some(clip.bounding_box()),
        _ => None,
    });
    assert_eq!(
        clip.map(|rect| (rect.x0, rect.y0, rect.x1, rect.y1)),
        Some((5.0, 5.0, 105.0, 65.0))
    );

    // An outset shadow of the same size is not clipped at all.
    let plain = list_for(
        "<style>body { margin: 0 } div { width: 100px; height: 60px; \
         box-shadow: 0 0 0 10px red }</style><div></div>",
    );
    assert!(
        !plain
            .items()
            .iter()
            .any(|item| matches!(item, DisplayItem::PushLayer { .. })),
        "an outset shadow needs no clip"
    );
}

/// A run inside a block carries that block's style. Painting a border from it
/// would frame the text rather than the box — twice over, once per line.
#[test]
fn a_blocks_border_is_painted_once_and_not_around_its_text() {
    let list = list_for(
        "<style>body { margin: 0 } p { border: 2px solid red }</style>\
         <p>a line of text long enough to be its own run</p>",
    );
    let sides = rects(&list, RED);
    assert_eq!(sides.len(), 4, "expected four sides, got {sides:?}");
}
