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
use otlyra_gfx::{DisplayItem, DisplayList};
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

    let visible = Rect::new(0.0, scroll_y, width, height);
    for fragment in tree.visible(&visible) {
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
    let origin = Affine::translate((f64::from(rect.x), f64::from(rect.y - scroll_y)));

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
        }

        FragmentKind::Line => {}

        FragmentKind::Text(runs) => {
            for run in runs {
                if run.glyphs.is_empty() {
                    continue;
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
}

/// The colour a shaped run carried, back as a paint colour.
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
        assert!(
            all.len() < 40,
            "only the visible paragraphs should be painted, got {} items",
            all.len()
        );
    }

    #[test]
    fn an_empty_document_still_paints_the_canvas() {
        let list = page("", 0.0);
        assert_eq!(list.len(), 1);
    }
}
