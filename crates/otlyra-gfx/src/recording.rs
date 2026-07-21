//! `RecordingPainter` — the second [`PaintTarget`] backend, present from day one.
//!
//! It keeps the trait honest — a trait with one implementation is a trait shaped
//! like that implementation — and it is the snapshot-test seam: assertions about
//! *what* was painted, with no GPU, no fonts and no golden PNG.

use kurbo::{Affine, BezPath, Rect, Stroke};
use peniko::{BlendMode, Brush, BrushRef, Fill, FontData, ImageBrushRef};

use crate::paint_target::{Glyph, PaintShape, PaintTarget};

/// Flattening tolerance for recorded geometry. Fixed, not configurable: identical
/// input must produce identical output, so this can never depend on device scale.
const RECORD_TOLERANCE: f64 = 0.1;

/// One recorded painting operation.
///
/// `Debug` is part of this type's contract: it is what snapshot tests compare, so
/// field names and ordering are stable and changing them is a deliberate,
/// snapshot-invalidating act.
#[derive(Clone, Debug, PartialEq)]
pub enum PaintOp {
    /// [`PaintTarget::push_layer`].
    PushLayer {
        /// Compositing mode for the group.
        blend: BlendMode,
        /// Group opacity in `0.0..=1.0`.
        alpha: f32,
        /// Transform applied to `clip`.
        transform: Affine,
        /// Clip geometry, flattened at [`RECORD_TOLERANCE`].
        clip: BezPath,
    },
    /// [`PaintTarget::pop_layer`].
    PopLayer,
    /// [`PaintTarget::fill`].
    Fill {
        /// Fill rule.
        style: Fill,
        /// Transform applied to `shape`.
        transform: Affine,
        /// Owned copy of the brush.
        brush: Brush,
        /// Optional brush-space transform.
        brush_transform: Option<Affine>,
        /// Geometry, flattened at [`RECORD_TOLERANCE`].
        shape: BezPath,
    },
    /// [`PaintTarget::stroke`].
    Stroke {
        /// Stroke parameters.
        style: Stroke,
        /// Transform applied to `shape`.
        transform: Affine,
        /// Owned copy of the brush.
        brush: Brush,
        /// Optional brush-space transform.
        brush_transform: Option<Affine>,
        /// Geometry, flattened at [`RECORD_TOLERANCE`].
        shape: BezPath,
    },
    /// [`PaintTarget::draw_glyphs`].
    DrawGlyphs {
        /// Font size in px.
        font_size: f32,
        /// Variation axis coordinates, normalized to `-1.0..=1.0` as `i16` F2Dot14.
        normalized_coords: Vec<i16>,
        /// Owned copy of the brush.
        brush: Brush,
        /// Transform applied to the run origin.
        transform: Affine,
        /// Whether grid fitting was requested.
        hint: bool,
        /// The run's glyphs, in order.
        glyphs: Vec<Glyph>,
    },
    /// [`PaintTarget::fill_blurred`].
    FillBlurred {
        /// Transform applied to the shape.
        transform: Affine,
        /// Paint.
        brush: Brush,
        /// The CSS blur radius.
        blur: f64,
        /// Geometry, flattened.
        shape: BezPath,
    },
    /// [`PaintTarget::draw_image`].
    DrawImage {
        /// Image width in px.
        width: u32,
        /// Image height in px.
        height: u32,
        /// Transform applied to the image's unit-pixel space.
        transform: Affine,
        /// Optional bounding rectangle.
        clip_rect: Option<Rect>,
    },
    /// [`PaintTarget::reset`], recorded rather than swallowed: "the frame was
    /// cleared here" is information a snapshot wants.
    Reset,
}

/// A [`PaintTarget`] that records operations instead of rasterizing them.
#[derive(Clone, Debug, Default)]
pub struct RecordingPainter {
    ops: Vec<PaintOp>,
    layer_depth: usize,
}

impl RecordingPainter {
    /// An empty recording.
    pub fn new() -> Self {
        Self::default()
    }

    /// The operations recorded so far, in order.
    pub fn ops(&self) -> &[PaintOp] {
        &self.ops
    }

    /// Take ownership of the recording, leaving this painter empty.
    pub fn take(&mut self) -> Vec<PaintOp> {
        self.layer_depth = 0;
        std::mem::take(&mut self.ops)
    }

    /// Number of layers currently open.
    ///
    /// A well-formed frame ends at zero; a non-zero value at present time is a
    /// missing [`PaintTarget::pop_layer`].
    pub fn open_layers(&self) -> usize {
        self.layer_depth
    }

    fn record(&mut self, op: PaintOp) {
        self.ops.push(op);
    }
}

fn flatten(shape: &dyn PaintShape) -> BezPath {
    let mut path = BezPath::new();
    shape.visit_path_elements(RECORD_TOLERANCE, &mut |element| path.push(element));
    path
}

impl PaintTarget for RecordingPainter {
    fn reset(&mut self) {
        self.ops.clear();
        self.layer_depth = 0;
        self.record(PaintOp::Reset);
    }

    fn push_layer(
        &mut self,
        blend: BlendMode,
        alpha: f32,
        transform: Affine,
        clip: &dyn PaintShape,
    ) {
        self.layer_depth += 1;
        self.record(PaintOp::PushLayer {
            blend,
            alpha,
            transform,
            clip: flatten(clip),
        });
    }

    fn pop_layer(&mut self) {
        // Recorded rather than panicking, so this backend stays usable on exactly
        // the malformed frames it is meant to debug.
        self.layer_depth = self.layer_depth.saturating_sub(1);
        self.record(PaintOp::PopLayer);
    }

    fn fill(
        &mut self,
        style: Fill,
        transform: Affine,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        shape: &dyn PaintShape,
    ) {
        self.record(PaintOp::Fill {
            style,
            transform,
            brush: brush.to_owned(),
            brush_transform,
            shape: flatten(shape),
        });
    }

    fn fill_blurred(
        &mut self,
        transform: Affine,
        brush: BrushRef<'_>,
        blur: f64,
        shape: &dyn PaintShape,
    ) {
        self.record(PaintOp::FillBlurred {
            transform,
            brush: brush.to_owned(),
            blur,
            shape: flatten(shape),
        });
    }

    fn stroke(
        &mut self,
        style: &Stroke,
        transform: Affine,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        shape: &dyn PaintShape,
    ) {
        self.record(PaintOp::Stroke {
            style: style.clone(),
            transform,
            brush: brush.to_owned(),
            brush_transform,
            shape: flatten(shape),
        });
    }

    fn draw_glyphs(
        &mut self,
        _font: &FontData,
        font_size: f32,
        normalized_coords: &[i16],
        brush: BrushRef<'_>,
        transform: Affine,
        hint: bool,
        glyphs: &mut dyn Iterator<Item = Glyph>,
    ) {
        self.record(PaintOp::DrawGlyphs {
            font_size,
            normalized_coords: normalized_coords.to_vec(),
            brush: brush.to_owned(),
            transform,
            hint,
            glyphs: glyphs.collect(),
        });
    }

    fn draw_image(&mut self, image: ImageBrushRef<'_>, transform: Affine, clip_rect: Option<Rect>) {
        self.record(PaintOp::DrawImage {
            width: image.image.width,
            height: image.image.height,
            transform,
            clip_rect,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peniko::Color;

    fn brush() -> Brush {
        Brush::Solid(Color::from_rgb8(0x33, 0x66, 0x99))
    }

    #[test]
    fn records_the_ops_it_was_given_in_order() {
        let mut painter = RecordingPainter::new();
        let b = brush();

        painter.reset();
        painter.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            (&b).into(),
            None,
            &Rect::new(0.0, 0.0, 10.0, 20.0),
        );
        painter.stroke(
            &Stroke::new(2.0),
            Affine::translate((1.0, 1.0)),
            (&b).into(),
            None,
            &Rect::new(0.0, 0.0, 4.0, 4.0),
        );

        let ops = painter.take();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0], PaintOp::Reset);

        let PaintOp::Fill {
            style,
            shape,
            brush: recorded,
            ..
        } = &ops[1]
        else {
            panic!("expected a fill, got {:?}", ops[1]);
        };
        assert_eq!(*style, Fill::NonZero);
        assert_eq!(*recorded, b);
        assert_eq!(
            PaintShape::bounding_box(shape),
            Rect::new(0.0, 0.0, 10.0, 20.0)
        );

        assert!(matches!(ops[2], PaintOp::Stroke { .. }));
    }

    #[test]
    fn layer_depth_tracks_push_and_pop() {
        let mut painter = RecordingPainter::new();
        assert_eq!(painter.open_layers(), 0);
        painter.push_clip_rect(Affine::IDENTITY, Rect::new(0.0, 0.0, 1.0, 1.0));
        painter.push_clip_rect(Affine::IDENTITY, Rect::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(painter.open_layers(), 2);
        painter.pop_layer();
        painter.pop_layer();
        assert_eq!(painter.open_layers(), 0);
        painter.pop_layer();
        assert_eq!(painter.open_layers(), 0);
    }

    #[test]
    fn glyph_runs_are_recorded_with_their_glyphs() {
        let mut painter = RecordingPainter::new();
        let b = brush();
        let glyphs = [
            Glyph {
                id: 7,
                x: 0.0,
                y: 0.0,
            },
            Glyph {
                id: 9,
                x: 12.5,
                y: 0.0,
            },
        ];

        painter.draw_glyphs(
            &FontData::new(peniko::Blob::new(std::sync::Arc::new(Vec::new())), 0),
            32.0,
            &[],
            (&b).into(),
            Affine::translate((10.0, 40.0)),
            true,
            &mut glyphs.iter().copied(),
        );

        let PaintOp::DrawGlyphs {
            font_size,
            hint,
            glyphs: recorded,
            ..
        } = &painter.ops()[0]
        else {
            panic!("expected a glyph run");
        };
        assert_eq!(*font_size, 32.0);
        assert!(*hint);
        assert_eq!(recorded.as_slice(), &glyphs);
    }

    /// `Debug` output is the snapshot contract; it must be deterministic across
    /// runs for identical input.
    #[test]
    fn debug_output_is_stable() {
        let render = || {
            let mut painter = RecordingPainter::new();
            let b = brush();
            painter.fill_rect(Affine::IDENTITY, (&b).into(), Rect::new(0.0, 0.0, 3.0, 4.0));
            format!("{:?}", painter.ops())
        };
        assert_eq!(render(), render());
    }
}
