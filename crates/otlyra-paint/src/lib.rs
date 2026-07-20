//! # otlyra-paint — fragments to a display list
//!
//! ## Purpose
//!
//! The last step before pixels, and a pure function: a laid-out page in, a flat
//! list of drawing commands out. Nothing here allocates a GPU resource, touches a
//! rasterizer or knows which one is installed.
//!
//! ## Contents
//!
//! - [`build_display_list`] — the whole crate.
//!
//! ## Invariants
//!
//! 1. **Pure.** The same fragment tree and viewport always produce the same list,
//!    which is what makes display-list snapshots a regression test rather than a
//!    record of one machine's mood.
//! 2. **Paint order is document order.** Backgrounds, then text, walking the tree
//!    depth first. Stacking contexts and `z-index` arrive with `position`.
//! 3. **Off-screen fragments produce no items at all.** Culling here is cheaper
//!    than clipping in the rasterizer, and on a long page it removes most of the
//!    page.

use otlyra_gfx::kurbo::{Affine, Rect as KurboRect, Shape};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList, HitTestId};
use otlyra_layout::fragment::{Fragment, FragmentKind, FragmentTree, Rect};

/// Flattening tolerance for shapes entering the display list. Matches the recording
/// backend's, so a display list and its recording agree.
const PATH_TOLERANCE: f64 = 0.1;

/// Build the display list for `tree`, showing the part of the page under
/// `scroll_y`, at `viewport` logical size.
pub fn build_display_list(tree: &FragmentTree, viewport: (f32, f32), scroll_y: f32) -> DisplayList {
    let _span = tracing::info_span!("build_display_list").entered();
    let (width, height) = viewport;
    let mut list = DisplayList::new();

    // The canvas itself. The initial containing block's background paints over the
    // whole viewport, not just the height the content happened to need.
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(tree.root.style.background_color),
        brush_transform: None,
        shape: KurboRect::new(0.0, 0.0, f64::from(width), f64::from(height))
            .to_path(PATH_TOLERANCE),
    });

    let scrolled = Rect::new(0.0, scroll_y, width, height);
    let screen = Rect::new(0.0, 0.0, width, height);
    for fragment in tree.visible(&scrolled, &screen) {
        // The initial containing block was painted as the canvas above; painting it
        // again would put a second full-viewport fill in every frame.
        if std::ptr::eq(fragment, &tree.root) {
            continue;
        }
        paint(fragment, scroll_y, &mut list);
    }

    tracing::debug!(items = list.len(), "display list built");
    list
}

/// One fragment's own drawing. Children are visited by the caller's walk, so this
/// never recurses — a fragment whose parent was culled may still be visible.
fn paint(fragment: &Fragment, scroll_y: f32, list: &mut DisplayList) {
    let rect = fragment.rect;
    // A fixed fragment is already in screen coordinates: it stays where it is
    // however far the page has been scrolled.
    let scroll_y = if fragment.fixed { 0.0 } else { scroll_y };
    let origin = Affine::translate((f64::from(rect.x), f64::from(rect.y - scroll_y)));

    // Hit testing is a display list too, emitted into the same sequence as the
    // painting it belongs to. Keeping them together is what stops a link from being
    // clickable somewhere other than where it is drawn.
    if let Some(box_id) = fragment.box_id
        && !matches!(fragment.kind, FragmentKind::Line)
    {
        list.push(DisplayItem::HitTest {
            rect: KurboRect::new(
                f64::from(rect.x),
                f64::from(rect.y - scroll_y),
                f64::from(rect.right()),
                f64::from(rect.bottom() - scroll_y),
            ),
            transform: Affine::IDENTITY,
            id: HitTestId(otlyra_layout::box_id_to_u64(box_id)),
        });
    }

    match &fragment.kind {
        FragmentKind::Box => {
            let background = fragment.style.background_color;
            // Transparent is the initial value, so most boxes paint nothing at all.
            if background.components[3] > 0.0 {
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(background),
                    brush_transform: None,
                    shape: KurboRect::new(
                        f64::from(rect.x),
                        f64::from(rect.y - scroll_y),
                        f64::from(rect.right()),
                        f64::from(rect.bottom() - scroll_y),
                    )
                    .to_path(PATH_TOLERANCE),
                });
            }

            paint_borders(list, fragment, rect, scroll_y);
        }

        FragmentKind::Line => {}

        FragmentKind::Image(image) => {
            if rect.width <= 0.0 || rect.height <= 0.0 || image.width == 0 || image.height == 0 {
                return;
            }
            // The image carries its own pixel size, so the transform is what makes
            // it the size the page asked for: a scale to the fragment, then a move
            // to where the fragment is.
            let scale = Affine::scale_non_uniform(
                f64::from(rect.width) / f64::from(image.width),
                f64::from(rect.height) / f64::from(image.height),
            );
            list.push(DisplayItem::Image {
                image: otlyra_gfx::ImageResource::from(image.clone()),
                sampler: otlyra_gfx::peniko::ImageSampler::default(),
                transform: origin * scale,
                // No clip: the transform already lands the picture exactly on the
                // fragment, and the rectangle a clip takes is in the transformed
                // space rather than the page's, so a page-space rectangle here
                // would cut the picture down by whatever it was scaled by.
                clip_rect: None,
            });
        }

        FragmentKind::Text(run) => {
            if run.glyphs.is_empty() {
                return;
            }

            // Decorations first, so the glyphs sit on top of them: a line drawn
            // over text is a strikethrough whatever it was meant to be. The offset
            // and thickness come from the font, by way of the shaper.
            for decoration in [run.underline.as_ref(), run.strikethrough.as_ref()]
                .into_iter()
                .flatten()
            {
                let baseline = f64::from(run.glyphs[0].y);
                let top = f64::from(rect.y - scroll_y) + baseline - f64::from(decoration.offset);
                list.push(DisplayItem::Fill {
                    style: Fill::NonZero,
                    transform: Affine::IDENTITY,
                    brush: Brush::Solid(brush_to_color(run.brush)),
                    brush_transform: None,
                    shape: KurboRect::new(
                        f64::from(rect.x),
                        top,
                        f64::from(rect.x) + f64::from(run.advance),
                        top + f64::from(decoration.thickness),
                    )
                    .to_path(PATH_TOLERANCE),
                });
            }

            list.push_glyphs(
                &run.font,
                run.font_size,
                run.normalized_coords.clone(),
                Brush::Solid(brush_to_color(run.brush)),
                origin,
                true,
                run.glyphs.clone(),
            );
        }
    }
}

/// The colour a shaped run carried, back as a paint colour.
/// The four borders of a box, each as a filled rectangle on the inside edge of the
/// border box.
///
/// Rectangles rather than a stroked outline, because each side has its own width
/// and colour and a stroke has one of each. The corners are square: mitring them
/// needs the four trapezia CSS specifies, and the difference only shows where two
/// adjacent sides differ in colour and are thick enough to see.
fn paint_borders(
    list: &mut DisplayList,
    fragment: &Fragment,
    rect: otlyra_layout::Rect,
    scroll_y: f32,
) {
    let border = fragment.style.border;
    let (left, top) = (f64::from(rect.x), f64::from(rect.y - scroll_y));
    let (right, bottom) = (f64::from(rect.right()), f64::from(rect.bottom() - scroll_y));

    let sides = [
        (
            border.top,
            [left, top, right, top + f64::from(border.top.width)],
        ),
        (
            border.right,
            [right - f64::from(border.right.width), top, right, bottom],
        ),
        (
            border.bottom,
            [left, bottom - f64::from(border.bottom.width), right, bottom],
        ),
        (
            border.left,
            [left, top, left + f64::from(border.left.width), bottom],
        ),
    ];

    for (side, [x0, y0, x1, y1]) in sides {
        if !side.is_visible() {
            continue;
        }
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(side.color),
            brush_transform: None,
            shape: KurboRect::new(x0, y0, x1, y1).to_path(PATH_TOLERANCE),
        });
    }
}

fn brush_to_color(brush: [u8; 4]) -> Color {
    Color::from_rgba8(brush[0], brush[1], brush[2], brush[3])
}

#[cfg(test)]
mod tests {
    use otlyra_gfx::{PaintOp, RecordingPainter, render};
    use otlyra_layout::{Viewport, build_box_tree, layout};
    use otlyra_text::TextEngine;

    use super::*;

    fn page(html: &str, scroll_y: f32) -> DisplayList {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let boxes = build_box_tree(&parsed.document);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
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
            },
        );
        let images: otlyra_layout::Images = otlyra_layout::image_sources(&parsed.document)
            .into_iter()
            .map(|source| (source.node, image.clone()))
            .collect();
        let boxes =
            otlyra_layout::build_box_tree_with_images(&parsed.document, Some(&styles), &images);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
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
            },
        );
        let boxes = otlyra_layout::build_styled_box_tree(&parsed.document, &styles);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
            &mut text,
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );
        build_display_list(&fragments, (800.0, 600.0), scroll_y)
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
}

#[cfg(test)]
mod border_tests {
    use otlyra_css::cascade::{Viewport as StyleViewport, style_document};
    use otlyra_layout::{Viewport, build_styled_box_tree, layout};
    use otlyra_text::TextEngine;

    use super::*;

    /// The display list for a styled document, at a fixed viewport.
    fn list_for(html: &str) -> DisplayList {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styles = style_document(&document, StyleViewport::default());
        let boxes = build_styled_box_tree(&document, &styles);
        let mut text = TextEngine::isolated();
        let fragments = layout(
            &boxes,
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
        let list = list_for(
            "<style>body { margin: 0 } div { border: 10px none red }</style><div>text</div>",
        );
        assert!(
            rects(&list, RED).is_empty(),
            "a border with no style was painted"
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
}
