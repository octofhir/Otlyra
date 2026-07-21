//! Replaying a display list into a paint target.

use crate::display_list::{DisplayItem, DisplayList};
use crate::paint_target::PaintTarget;
use peniko::ImageBrushRef;

/// Replay `list` into `target`.
///
/// This is the only place the two halves of the renderer meet, and it is
/// deliberately a plain function rather than a method on either: a display list
/// does not know how to draw itself, and a paint target does not know what a
/// display list is.
pub fn render(list: &DisplayList, target: &mut dyn PaintTarget) {
    let _span = tracing::info_span!("paint", items = list.len()).entered();

    for item in list.items() {
        match item {
            DisplayItem::PushLayer {
                blend,
                alpha,
                transform,
                clip,
            } => target.push_layer(*blend, *alpha, *transform, clip),

            DisplayItem::PopLayer => target.pop_layer(),

            DisplayItem::Blurred {
                transform,
                brush,
                blur,
                shape,
            } => target.fill_blurred(*transform, brush.into(), *blur, shape),

            DisplayItem::Fill {
                style,
                transform,
                brush,
                brush_transform,
                shape,
            } => target.fill(*style, *transform, brush.into(), *brush_transform, shape),

            DisplayItem::Stroke {
                style,
                transform,
                brush,
                brush_transform,
                shape,
            } => target.stroke(style, *transform, brush.into(), *brush_transform, shape),

            DisplayItem::Glyphs {
                font,
                font_size,
                normalized_coords,
                brush,
                transform,
                hint,
                blur,
                glyphs,
            } => {
                let Some(font) = list.fonts().get(*font) else {
                    // Only reachable if a list was built with one font table and
                    // replayed against another, which is a construction bug.
                    tracing::error!(?font, "glyph run references a font not in the table");
                    continue;
                };
                target.draw_glyph_run(
                    font,
                    *font_size,
                    normalized_coords,
                    brush.into(),
                    *blur,
                    *transform,
                    *hint,
                    &mut glyphs.iter().copied(),
                );
            }

            DisplayItem::Image {
                image,
                sampler,
                transform,
                clip_rect,
            } => target.draw_image(
                ImageBrushRef {
                    image: image.data(),
                    sampler: *sampler,
                },
                *transform,
                *clip_rect,
            ),

            // Hit-test regions are not painted. They travel in the same list so
            // that hit testing cannot drift away from what was drawn.
            DisplayItem::HitTest { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::HitTestId;
    use crate::{PaintOp, RecordingPainter};
    use kurbo::{Affine, Rect, Shape};
    use peniko::{Brush, Color, Fill, FontData};

    #[test]
    fn every_paintable_item_reaches_the_target() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(Color::BLACK),
            brush_transform: None,
            shape: Rect::new(0.0, 0.0, 1.0, 1.0).to_path(0.1),
        });
        list.push_glyphs(
            &FontData::new(peniko::Blob::new(std::sync::Arc::new(b"font")), 0),
            16.0,
            Vec::new(),
            Brush::Solid(Color::BLACK),
            Affine::IDENTITY,
            true,
            vec![crate::Glyph {
                id: 1,
                x: 0.0,
                y: 0.0,
            }],
        );

        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);

        assert!(matches!(painter.ops()[0], PaintOp::Fill { .. }));
        assert!(matches!(painter.ops()[1], PaintOp::DrawGlyphs { .. }));
        assert_eq!(painter.ops().len(), 2);
    }

    /// Hit-test items must not produce paint operations. If they ever do, every
    /// hit region becomes a visible artifact.
    #[test]
    fn hit_test_items_paint_nothing() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::HitTest {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            transform: Affine::IDENTITY,
            id: HitTestId(1),
        });

        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);
        assert!(painter.ops().is_empty());
    }

    #[test]
    fn layers_are_replayed_in_order_and_stay_balanced() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::PushLayer {
            blend: peniko::BlendMode::default(),
            alpha: 1.0,
            transform: Affine::IDENTITY,
            clip: Rect::new(0.0, 0.0, 4.0, 4.0).to_path(0.1),
        });
        list.push(DisplayItem::PopLayer);

        let mut painter = RecordingPainter::new();
        render(&list, &mut painter);

        assert!(matches!(painter.ops()[0], PaintOp::PushLayer { .. }));
        assert!(matches!(painter.ops()[1], PaintOp::PopLayer));
        assert_eq!(painter.open_layers(), 0);
    }
}
