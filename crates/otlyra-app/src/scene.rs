//! The placeholder scene.
//!
//! Until there is a DOM, a cascade and a layout engine, something has to prove the
//! pipeline end to end. This paints through the real [`PaintTarget`] seam and shapes
//! its text through the real font stack, so what appears on screen comes out of the
//! code path a real page will use. It goes away once a display list replaces it.

use otlyra_gfx::PaintTarget;
use otlyra_gfx::kurbo::{Affine, Rect, RoundedRect, Stroke};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_platform::{Painter, PlatformEvent, Viewport};
use otlyra_text::{FontStack, TEST_FAMILY, TextEngine};

/// Background of the viewport. White, because that is the initial containing
/// block's used background in every browser.
const BACKGROUND: Color = Color::from_rgb8(0xff, 0xff, 0xff);

/// Ink colour for the label and the outline.
const INK: Color = Color::from_rgb8(0x1d, 0x35, 0x57);

/// The swatches, in painting order.
const SWATCHES: [Color; 4] = [
    Color::from_rgb8(0xe6, 0x39, 0x46),
    Color::from_rgb8(0xf1, 0xfa, 0xee),
    Color::from_rgb8(0x45, 0x7b, 0x9d),
    Color::from_rgb8(0x1d, 0x35, 0x57),
];

const LABEL: &str = "Otlyra";
const LABEL_SIZE: f32 = 32.0;

/// Paints a fixed test scene: a label, a background and a row of rectangles.
#[derive(Debug)]
pub struct DemoScene {
    text: TextEngine,
    stack: FontStack,
}

impl Default for DemoScene {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoScene {
    /// A new scene.
    ///
    /// The engine is deliberately isolated from system fonts. The only text here is
    /// our own label, and an isolated engine makes the rendered frame identical on
    /// every machine, which is what lets the image test be a merge gate. Real pages
    /// will want system fonts; they arrive with the DOM.
    pub fn new() -> Self {
        Self {
            text: TextEngine::isolated(),
            stack: FontStack::named(TEST_FAMILY),
        }
    }
}

impl Painter for DemoScene {
    fn on_event(&mut self, event: PlatformEvent) {
        if let PlatformEvent::CloseRequested = event {
            tracing::info!("close requested");
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

        let shaped = {
            let _span = tracing::info_span!("layout", text = LABEL).entered();
            self.text.shape(LABEL, &self.stack, LABEL_SIZE, None)
        };

        // Glyph positions are already absolute within the layout, baseline
        // included, so the transform carries the layout origin and nothing else.
        let ink = Brush::Solid(INK);
        for run in &shaped.runs {
            target.draw_glyphs(
                &run.font,
                run.font_size,
                &run.normalized_coords,
                (&ink).into(),
                scale * Affine::translate((margin, margin)),
                true,
                &mut run.glyphs.iter().copied(),
            );
        }

        // Derived from what the text actually measured, not from a magic band
        // height: a constant here is a constant that silently collides the first
        // time the label or its size changes.
        let swatch_top = margin + f64::from(shaped.metrics.height) + gap;
        let count = SWATCHES.len() as f64;
        let swatch_width = ((width - margin * 2.0) - gap * (count - 1.0)) / count;
        let swatch_height = (height - swatch_top - margin).min(240.0);

        if swatch_width <= 0.0 || swatch_height <= 0.0 {
            // Smaller than the margins. Painting nothing is correct.
            return;
        }

        for (index, color) in SWATCHES.iter().enumerate() {
            let x = margin + index as f64 * (swatch_width + gap);
            let rect = Rect::new(x, swatch_top, x + swatch_width, swatch_top + swatch_height);
            let brush = Brush::Solid(*color);
            target.fill(
                Fill::NonZero,
                scale,
                (&brush).into(),
                None,
                &RoundedRect::from_rect(rect, 12.0),
            );
        }

        // One stroked outline, so the stroke path is exercised by the image tests
        // too and not only by unit tests.
        target.stroke(
            &Stroke::new(2.0),
            scale,
            (&ink).into(),
            None,
            &Rect::new(
                margin - 8.0,
                swatch_top - 8.0,
                width - margin + 8.0,
                swatch_top + swatch_height + 8.0,
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
    fn paints_background_then_text_then_swatches_then_an_outline() {
        let ops = record(Viewport::new(1024, 768, 1.0));

        assert!(
            matches!(ops[0], PaintOp::Fill { .. }),
            "background is a fill"
        );
        assert!(
            matches!(ops[1], PaintOp::DrawGlyphs { .. }),
            "then the label"
        );
        for op in &ops[2..2 + SWATCHES.len()] {
            assert!(matches!(op, PaintOp::Fill { .. }), "swatches are fills");
        }
        assert!(
            matches!(ops[2 + SWATCHES.len()], PaintOp::Stroke { .. }),
            "outline is a stroke"
        );
        assert_eq!(ops.len(), 3 + SWATCHES.len());
    }

    /// The label must reach the seam as one run of six glyphs at the size we asked
    /// for. This is the assertion that catches text silently vanishing.
    #[test]
    fn the_label_reaches_the_seam_as_a_shaped_glyph_run() {
        let ops = record(Viewport::new(1024, 768, 1.0));
        let PaintOp::DrawGlyphs {
            font_size,
            glyphs,
            hint,
            ..
        } = &ops[1]
        else {
            panic!("expected a glyph run, got {:?}", ops[1]);
        };

        assert_eq!(*font_size, LABEL_SIZE);
        assert_eq!(glyphs.len(), LABEL.len(), "one glyph per ASCII character");
        assert!(*hint, "static text should be grid fitted");
        for pair in glyphs.windows(2) {
            assert!(pair[1].x > pair[0].x, "glyphs advance left to right");
        }
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

    /// Glyph geometry is authored in logical pixels too: the run's transform
    /// carries the scale, the glyph offsets do not.
    #[test]
    fn glyph_positions_are_logical_and_the_transform_carries_the_scale() {
        let one_x = record(Viewport::new(800, 600, 1.0));
        let two_x = record(Viewport::new(1600, 1200, 2.0));

        let (PaintOp::DrawGlyphs { glyphs: a, .. }, PaintOp::DrawGlyphs { glyphs: b, .. }) =
            (&one_x[1], &two_x[1])
        else {
            panic!("expected glyph runs")
        };
        assert_eq!(a, b, "glyph offsets must not depend on the device scale");
    }

    #[test]
    fn a_viewport_smaller_than_the_margins_paints_only_the_background_and_label() {
        let ops = record(Viewport::new(16, 16, 1.0));
        assert_eq!(ops.len(), 2);
    }
}
