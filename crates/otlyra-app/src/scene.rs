//! The placeholder scene.
//!
//! Until there is a DOM, a cascade and a layout engine, something has to prove the
//! pipeline end to end. This paints through the real [`PaintTarget`] seam, so what
//! appears on screen comes out of the code path a real page will use. It goes away
//! once a display list replaces it.

use otlyra_gfx::PaintTarget;
use otlyra_gfx::kurbo::{Affine, Rect, RoundedRect, Stroke};
use otlyra_gfx::peniko::{Brush, Color};
use otlyra_platform::{Painter, PlatformEvent, Viewport};

/// Paints a fixed test scene: a background and a row of coloured rectangles.
#[derive(Debug, Default)]
pub struct DemoScene {
    /// Last viewport we were told about, for logging only.
    last_viewport: Option<Viewport>,
}

impl DemoScene {
    /// A new scene.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Background of the viewport. White, because that is the initial containing
/// block's used background in every browser.
const BACKGROUND: Color = Color::from_rgb8(0xff, 0xff, 0xff);

/// The swatches, in painting order.
const SWATCHES: [Color; 4] = [
    Color::from_rgb8(0xe6, 0x39, 0x46),
    Color::from_rgb8(0xf1, 0xfa, 0xee),
    Color::from_rgb8(0x45, 0x7b, 0x9d),
    Color::from_rgb8(0x1d, 0x35, 0x57),
];

impl Painter for DemoScene {
    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            PlatformEvent::SurfaceReady(viewport) | PlatformEvent::Resized(viewport) => {
                self.last_viewport = Some(viewport);
            }
            PlatformEvent::CloseRequested => {
                tracing::info!("close requested");
            }
            _ => {}
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        // Authored in logical pixels and scaled once, here — the same discipline
        // layout will use. Geometry stays device-scale agnostic.
        let scale = Affine::scale(viewport.scale_factor);
        let width = viewport.logical_width();
        let height = viewport.logical_height();

        let background = Brush::Solid(BACKGROUND);
        target.fill_rect(
            scale,
            (&background).into(),
            Rect::new(0.0, 0.0, width, height),
        );

        let margin = 48.0_f64;
        let gap = 16.0_f64;
        let count = SWATCHES.len() as f64;
        let swatch_width = ((width - margin * 2.0) - gap * (count - 1.0)) / count;
        let swatch_height = (height - margin * 2.0).min(240.0);

        if swatch_width <= 0.0 || swatch_height <= 0.0 {
            // Smaller than the margins. Painting nothing is correct.
            return;
        }

        for (index, color) in SWATCHES.iter().enumerate() {
            let x = margin + index as f64 * (swatch_width + gap);
            let rect = Rect::new(x, margin, x + swatch_width, margin + swatch_height);
            let brush = Brush::Solid(*color);
            target.fill(
                otlyra_gfx::peniko::Fill::NonZero,
                scale,
                (&brush).into(),
                None,
                &RoundedRect::from_rect(rect, 12.0),
            );
        }

        // One stroked outline, so the stroke path is exercised by the image tests
        // too and not only by unit tests.
        let outline = Brush::Solid(Color::from_rgb8(0x1d, 0x35, 0x57));
        target.stroke(
            &Stroke::new(2.0),
            scale,
            (&outline).into(),
            None,
            &Rect::new(
                margin - 8.0,
                margin - 8.0,
                width - margin + 8.0,
                margin + swatch_height + 8.0,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otlyra_gfx::kurbo::Shape;
    use otlyra_gfx::{PaintOp, RecordingPainter};

    fn record(viewport: Viewport) -> Vec<PaintOp> {
        let mut scene = DemoScene::new();
        let mut painter = RecordingPainter::new();
        scene.paint(&mut painter, viewport);
        painter.take()
    }

    #[test]
    fn paints_a_background_then_one_fill_per_swatch_then_an_outline() {
        let ops = record(Viewport::new(1024, 768, 1.0));

        assert_eq!(
            ops.len(),
            1 + SWATCHES.len() + 1,
            "background + swatches + outline"
        );
        assert!(
            matches!(ops[0], PaintOp::Fill { .. }),
            "background is a fill"
        );
        for op in &ops[1..=SWATCHES.len()] {
            assert!(matches!(op, PaintOp::Fill { .. }), "swatches are fills");
        }
        assert!(
            matches!(ops[SWATCHES.len() + 1], PaintOp::Stroke { .. }),
            "outline is a stroke"
        );
    }

    #[test]
    fn the_background_covers_the_whole_logical_viewport() {
        let ops = record(Viewport::new(800, 600, 1.0));
        let PaintOp::Fill { shape, brush, .. } = &ops[0] else {
            panic!("expected a fill")
        };
        assert_eq!(shape.bounding_box(), Rect::new(0.0, 0.0, 800.0, 600.0));
        assert_eq!(*brush, Brush::Solid(BACKGROUND));
    }

    /// HiDPI must change the transform, not the authored geometry. If a future
    /// change starts baking the scale factor into coordinates, this catches it.
    #[test]
    fn hidpi_is_expressed_as_a_transform_not_as_scaled_geometry() {
        let one_x = record(Viewport::new(800, 600, 1.0));
        let two_x = record(Viewport::new(1600, 1200, 2.0));

        let (
            PaintOp::Fill {
                shape: a,
                transform: ta,
                ..
            },
            PaintOp::Fill {
                shape: b,
                transform: tb,
                ..
            },
        ) = (&one_x[0], &two_x[0])
        else {
            panic!("expected fills")
        };
        assert_eq!(a.bounding_box(), b.bounding_box(), "same logical geometry");
        assert_eq!(*ta, Affine::scale(1.0));
        assert_eq!(*tb, Affine::scale(2.0));
    }

    #[test]
    fn a_viewport_smaller_than_the_margins_paints_only_the_background() {
        let ops = record(Viewport::new(16, 16, 1.0));
        assert_eq!(ops.len(), 1);
    }
}
