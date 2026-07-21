//! The placeholder scene.
//!
//! Until there is a DOM, a cascade and a layout engine, something has to prove the
//! pipeline end to end. This paints through the real [`PaintTarget`] seam and shapes
//! its text through the real font stack, so what appears on screen comes out of the
//! code path a real page will use. It goes away once a display list replaces it.

use otlyra_gfx::kurbo::{Affine, Rect, RoundedRect, Shape, Stroke};
use otlyra_gfx::peniko::{Brush, Color, Fill};
use otlyra_gfx::peniko::{ImageData, ImageSampler};
use otlyra_gfx::{DisplayItem, DisplayList, HitTestId, PaintTarget, decode_image, render};
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

/// The mark drawn beside the label. Decoded once, at construction.
const MARK: &[u8] = include_bytes!("../../../assets/logo/mark-256.png");

/// Size the mark is drawn at, in logical pixels.
const MARK_SIZE: f64 = 48.0;

const LABEL: &str = "Otlyra";
const LABEL_SIZE: f32 = 32.0;

/// Flattening tolerance for shapes entering the display list. Matches what the
/// recording backend uses, so a display list and its recording agree.
const PATH_TOLERANCE: f64 = 0.1;

/// Paints a fixed test scene: a label, a background and a row of rectangles.
#[derive(Debug)]
pub struct DemoScene {
    text: TextEngine,
    stack: FontStack,
    /// `None` if the mark could not be decoded. A missing logo is a cosmetic
    /// problem, not a reason to refuse to draw the frame.
    mark: Option<ImageData>,
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
        let mark = match decode_image(MARK) {
            Ok(image) => Some(image),
            Err(error) => {
                tracing::error!(%error, "the mark failed to decode");
                None
            }
        };

        Self {
            text: TextEngine::isolated(),
            stack: FontStack::named(TEST_FAMILY),
            mark,
        }
    }
}

impl DemoScene {
    /// Build the frame's display list.
    ///
    /// Everything the scene decides happens here. Painting is then a mechanical
    /// replay, which is what lets the same frame be asserted structurally, dumped
    /// as JSON, or rasterized by any backend.
    pub fn build_display_list(&mut self, viewport: Viewport) -> DisplayList {
        let _span = tracing::info_span!("build_display_list").entered();
        let mut list = DisplayList::new();

        // Authored in logical pixels and scaled once, here — the same discipline
        // layout will use. Geometry stays device-scale agnostic.
        let scale = Affine::scale(viewport.scale_factor);
        let width = viewport.logical_width();
        let height = viewport.logical_height();

        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: scale,
            brush: Brush::Solid(BACKGROUND),
            brush_transform: None,
            shape: Rect::new(0.0, 0.0, width, height).to_path(PATH_TOLERANCE),
        });

        let margin = 48.0_f64;
        let gap = 16.0_f64;

        let shaped = {
            let _span = tracing::info_span!("layout", text = LABEL).entered();
            self.text.shape(LABEL, &self.stack, LABEL_SIZE, None)
        };

        // The mark sits left of the label, both vertically centred on the text's
        // line box so they stay aligned whatever the font metrics turn out to be.
        let mut label_x = margin;
        if let Some(mark) = &self.mark {
            let mark_top = margin + (f64::from(shaped.metrics.height) - MARK_SIZE) / 2.0;
            let unit_scale = MARK_SIZE / f64::from(mark.width);
            list.push(DisplayItem::Image {
                image: mark.clone().into(),
                sampler: ImageSampler::default(),
                transform: scale
                    * Affine::translate((margin, mark_top))
                    * Affine::scale(unit_scale),
                clip_rect: None,
            });
            label_x += MARK_SIZE + gap;
        }

        // Glyph positions are already absolute within the layout, baseline
        // included, so the transform carries the layout origin and nothing else.
        for run in &shaped.runs {
            list.push_glyphs(
                &run.font,
                run.font_size,
                run.normalized_coords.clone(),
                Brush::Solid(INK),
                scale * Affine::translate((label_x, margin)),
                true,
                run.glyphs.clone(),
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
            return list;
        }

        for (index, color) in SWATCHES.iter().enumerate() {
            let x = margin + index as f64 * (swatch_width + gap);
            let rect = Rect::new(x, swatch_top, x + swatch_width, swatch_top + swatch_height);
            list.push(DisplayItem::Fill {
                style: Fill::NonZero,
                transform: scale,
                brush: Brush::Solid(*color),
                brush_transform: None,
                shape: RoundedRect::from_rect(rect, 12.0).to_path(PATH_TOLERANCE),
            });
            // Each swatch is hit-testable, so the seam that keeps hit testing and
            // painting in step is exercised rather than merely defined.
            list.push(DisplayItem::HitTest {
                rect,
                transform: scale,
                id: HitTestId(index as u64),
            });
        }

        // One stroked outline, so the stroke path is exercised by the image tests
        // too and not only by unit tests.
        list.push(DisplayItem::Stroke {
            style: Stroke::new(2.0),
            transform: scale,
            brush: Brush::Solid(INK),
            brush_transform: None,
            shape: Rect::new(
                margin - 8.0,
                swatch_top - 8.0,
                width - margin + 8.0,
                swatch_top + swatch_height + 8.0,
            )
            .to_path(PATH_TOLERANCE),
        });

        list
    }
}

impl Painter for DemoScene {
    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            PlatformEvent::MenuCommand(id) => match crate::menu::Command::from_id(id) {
                Some(command) => tracing::info!(?command, "menu command (not implemented yet)"),
                None => tracing::warn!(?id, "menu reported an id no command claims"),
            },
            _ => {}
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let list = self.build_display_list(viewport);
        render(&list, target);
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

    /// Located by kind rather than index: a scene gains and loses elements, and a
    /// test that hard-codes positions fails for reasons that are not regressions.
    fn glyph_run(ops: &[PaintOp]) -> &PaintOp {
        ops.iter()
            .find(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
            .expect("the label should reach the seam")
    }

    #[test]
    fn the_frame_is_painted_back_to_front() {
        let ops = record(Viewport::new(1024, 768, 1.0));

        let kinds: Vec<&str> = ops
            .iter()
            .map(|op| match op {
                PaintOp::Fill { .. } => "fill",
                PaintOp::FillBlurred { .. } => "shadow",
                PaintOp::Stroke { .. } => "stroke",
                PaintOp::DrawGlyphs { .. } => "glyphs",
                PaintOp::DrawImage { .. } => "image",
                PaintOp::PushLayer { .. } => "push",
                PaintOp::PopLayer => "pop",
                PaintOp::Reset => "reset",
            })
            .collect();

        assert_eq!(
            kinds,
            [
                "fill",   // background
                "image",  // the mark
                "glyphs", // the label
                "fill", "fill", "fill", "fill",   // swatches
                "stroke", // outline
            ]
        );
    }

    /// The assertion that catches text silently vanishing.
    #[test]
    fn the_label_reaches_the_seam_as_a_shaped_glyph_run() {
        let ops = record(Viewport::new(1024, 768, 1.0));
        let PaintOp::DrawGlyphs {
            font_size,
            glyphs,
            hint,
            ..
        } = glyph_run(&ops)
        else {
            unreachable!("filtered above")
        };

        assert_eq!(*font_size, LABEL_SIZE);
        assert_eq!(glyphs.len(), LABEL.len(), "one glyph per ASCII character");
        assert!(*hint, "static text should be grid fitted");
        for pair in glyphs.windows(2) {
            assert!(pair[1].x > pair[0].x, "glyphs advance left to right");
        }
    }

    /// The mark must be square on screen. It is decoded at whatever size the asset
    /// happens to be, so the transform has to normalize it.
    #[test]
    fn the_mark_is_drawn_square_and_at_the_requested_size() {
        let ops = record(Viewport::new(1024, 768, 1.0));
        let PaintOp::DrawImage {
            width,
            height,
            transform,
            ..
        } = ops
            .iter()
            .find(|op| matches!(op, PaintOp::DrawImage { .. }))
            .expect("the mark should be drawn")
        else {
            unreachable!("filtered above")
        };

        assert_eq!(width, height, "the source asset is square");

        let coeffs = transform.as_coeffs();
        let drawn_width = coeffs[0] * f64::from(*width);
        let drawn_height = coeffs[3] * f64::from(*height);
        assert!(
            (drawn_width - MARK_SIZE).abs() < 0.01,
            "drawn {drawn_width}px wide"
        );
        assert!(
            (drawn_height - MARK_SIZE).abs() < 0.01,
            "drawn {drawn_height}px tall"
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

    /// Glyph geometry is authored in logical pixels too: the run's transform
    /// carries the scale, the glyph offsets do not.
    #[test]
    fn glyph_positions_are_logical_and_the_transform_carries_the_scale() {
        let one_x = record(Viewport::new(800, 600, 1.0));
        let two_x = record(Viewport::new(1600, 1200, 2.0));

        let (PaintOp::DrawGlyphs { glyphs: a, .. }, PaintOp::DrawGlyphs { glyphs: b, .. }) =
            (glyph_run(&one_x), glyph_run(&two_x))
        else {
            unreachable!("filtered above")
        };
        assert_eq!(a, b, "glyph offsets must not depend on the device scale");
    }

    #[test]
    fn a_viewport_too_small_for_swatches_still_paints_the_header() {
        let ops = record(Viewport::new(16, 16, 1.0));
        assert!(
            ops.iter().all(|op| !matches!(op, PaintOp::Stroke { .. })),
            "no swatch band, so no outline"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, PaintOp::DrawGlyphs { .. }))
        );
    }
}
