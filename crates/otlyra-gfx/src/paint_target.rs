//! The `PaintTarget` trait — the single seam between display-list construction
//! and rasterization.
//!
//! Seven required methods; everything else here is provided over those seven.

use kurbo::{Affine, PathEl, Rect, Stroke};
use peniko::{BlendMode, BrushRef, Fill, FontData, ImageBrushRef};

/// A glyph positioned in the run's local space, as emitted by parley.
#[derive(Copy, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Glyph {
    /// Glyph index within the font — *not* a Unicode code point.
    pub id: u32,
    /// Horizontal offset from the run origin, in px.
    pub x: f32,
    /// Vertical offset from the run origin (baseline-relative), in px.
    pub y: f32,
}

/// Geometry passed across the seam, in a form that can go through a vtable.
///
/// `kurbo::Shape` cannot be used as `&dyn Shape`: it declares a generic associated
/// type (`PathElementsIter<'iter>`), and a GAT cannot go in a vtable. `PaintShape`
/// is the same idea made dyn-compatible — a visitor rather than a returned
/// iterator — and costs no allocation, unlike the obvious `Box<dyn Iterator>`.
///
/// The blanket impl below covers every `kurbo::Shape`, so callers pass `&Rect`,
/// `&RoundedRect` or `&BezPath` directly.
pub trait PaintShape {
    /// Visit the shape's path elements, flattening curves no coarser than
    /// `tolerance`.
    fn visit_path_elements(&self, tolerance: f64, visit: &mut dyn FnMut(PathEl));

    /// The shape's bounding box, for culling and for backends with a rectangle
    /// fast path.
    fn bounding_box(&self) -> Rect;

    /// `Some` if this shape is exactly an axis-aligned rectangle, so backends can
    /// take a fast path without re-deriving the fact from a path.
    fn as_rect(&self) -> Option<Rect> {
        None
    }
}

impl<T: kurbo::Shape> PaintShape for T {
    fn visit_path_elements(&self, tolerance: f64, visit: &mut dyn FnMut(PathEl)) {
        for element in kurbo::Shape::path_elements(self, tolerance) {
            visit(element);
        }
    }

    fn bounding_box(&self) -> Rect {
        kurbo::Shape::bounding_box(self)
    }

    fn as_rect(&self) -> Option<Rect> {
        kurbo::Shape::as_rect(self)
    }
}

/// The single seam between display-list construction and rasterization.
///
/// Deliberately 7 required methods: every additional method is one four backends
/// must implement, and one more reason a backend cannot be swapped.
///
/// There is deliberately no `draw_box_shadow`: that would be a backend limitation
/// leaking into the interface. Shadows are `push_layer` plus `fill` with a blurred
/// brush, and the blur lives in each backend.
pub trait PaintTarget {
    /// Discard all recorded content and reset the transform and layer stacks.
    fn reset(&mut self);

    /// Push a compositing group. `clip` bounds it; `alpha` and `blend` govern how
    /// it composites into its parent. A `Rect` covers CSS overflow clipping; an
    /// arbitrary shape covers `clip-path`.
    fn push_layer(
        &mut self,
        blend: BlendMode,
        alpha: f32,
        transform: Affine,
        clip: &dyn PaintShape,
    );

    /// Pop the most recent layer, compositing it into its parent.
    fn pop_layer(&mut self);

    /// Fill `shape` with `brush` under `transform`.
    fn fill(
        &mut self,
        style: Fill,
        transform: Affine,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        shape: &dyn PaintShape,
    );

    /// Stroke the outline of `shape` with `brush` under `transform`.
    fn stroke(
        &mut self,
        style: &Stroke,
        transform: Affine,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        shape: &dyn PaintShape,
    );

    /// One shaped run: one font, one size. `hint` requests grid fitting and must be
    /// false for transformed or animating text.
    #[allow(clippy::too_many_arguments)]
    fn draw_glyphs(
        &mut self,
        font: &FontData,
        font_size: f32,
        normalized_coords: &[i16],
        brush: BrushRef<'_>,
        transform: Affine,
        hint: bool,
        glyphs: &mut dyn Iterator<Item = Glyph>,
    );

    /// Draw a decoded image. `clip_rect` bounds it in the *transformed* space.
    fn draw_image(&mut self, image: ImageBrushRef<'_>, transform: Affine, clip_rect: Option<Rect>);

    // ---------------------------------------------------------------------
    // Provided methods. Every one of these is expressible over the seven above;
    // a new backend implements seven functions and inherits the rest.
    // ---------------------------------------------------------------------

    /// Fill an axis-aligned rectangle. The overwhelmingly common case in CSS
    /// painting — backgrounds, borders, carets, selection highlights.
    fn fill_rect(&mut self, transform: Affine, brush: BrushRef<'_>, rect: Rect) {
        self.fill(Fill::NonZero, transform, brush, None, &rect);
    }

    /// Push a rectangular clip layer with no blending and full opacity — CSS
    /// `overflow: hidden` on a box with no border radius.
    fn push_clip_rect(&mut self, transform: Affine, rect: Rect) {
        self.push_layer(BlendMode::default(), 1.0, transform, &rect);
    }

    /// One shaped run, with its edges blurred where `blur` is not zero — a text
    /// shadow, which is the same run drawn behind itself.
    ///
    /// Provided, for the same reason as [`Self::fill_blurred`]: a backend that
    /// cannot blur draws the run sharp, which is a shadow with a hard edge rather
    /// than a run of missing text.
    #[allow(clippy::too_many_arguments)]
    fn draw_glyph_run(
        &mut self,
        font: &FontData,
        font_size: f32,
        normalized_coords: &[i16],
        brush: BrushRef<'_>,
        blur: f64,
        transform: Affine,
        hint: bool,
        glyphs: &mut dyn Iterator<Item = Glyph>,
    ) {
        let _ = blur;
        self.draw_glyphs(
            font,
            font_size,
            normalized_coords,
            brush,
            transform,
            hint,
            glyphs,
        );
    }

    /// Fill a shape with its edges blurred — CSS `box-shadow`, and later
    /// `text-shadow` and `filter: blur()`.
    ///
    /// Provided rather than required, so a backend that cannot blur is still a
    /// backend: the default draws the shape sharp, which is a shadow with a hard
    /// edge rather than no shadow at all. `blur` is the CSS blur radius, which is
    /// twice the standard deviation the blur is actually done with.
    fn fill_blurred(
        &mut self,
        transform: Affine,
        brush: BrushRef<'_>,
        blur: f64,
        shape: &dyn PaintShape,
    ) {
        let _ = blur;
        self.fill(Fill::NonZero, transform, brush, None, shape);
    }
}

/// Compile-time proof that the seam is object safe. If a required method ever gains
/// a generic parameter or a `Self: Sized` bound, this stops compiling.
const _: () = {
    #[allow(dead_code)]
    fn assert_object_safe(target: Box<dyn PaintTarget>) -> Box<dyn PaintTarget> {
        target
    }
    #[allow(dead_code)]
    fn assert_shape_object_safe(shape: &dyn PaintShape) -> Rect {
        shape.bounding_box()
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PaintOp, RecordingPainter};
    use peniko::Color;

    /// `Box<dyn PaintTarget>` must be constructible and callable through the vtable.
    #[test]
    fn paint_target_is_object_safe_at_runtime() {
        let mut boxed: Box<dyn PaintTarget> = Box::new(RecordingPainter::new());
        let brush = peniko::Brush::Solid(Color::from_rgb8(1, 2, 3));
        boxed.fill_rect(
            Affine::IDENTITY,
            (&brush).into(),
            Rect::new(0.0, 0.0, 1.0, 1.0),
        );
        boxed.reset();
    }

    /// The provided methods must lower onto the required seven, not onto new ops.
    #[test]
    fn provided_methods_lower_onto_required_ones() {
        let mut painter = RecordingPainter::new();
        let brush = peniko::Brush::Solid(Color::from_rgb8(10, 20, 30));
        painter.fill_rect(
            Affine::IDENTITY,
            (&brush).into(),
            Rect::new(0.0, 0.0, 4.0, 2.0),
        );
        painter.push_clip_rect(Affine::IDENTITY, Rect::new(0.0, 0.0, 4.0, 2.0));
        painter.pop_layer();

        assert!(matches!(painter.ops()[0], PaintOp::Fill { .. }));
        assert!(matches!(painter.ops()[1], PaintOp::PushLayer { .. }));
        assert!(matches!(painter.ops()[2], PaintOp::PopLayer));
    }

    /// The blanket impl must keep the `as_rect` fast path intact through the
    /// vtable; a backend that loses it starts tessellating every background.
    #[test]
    fn rectangles_stay_recognizable_through_the_vtable() {
        let rect = Rect::new(1.0, 2.0, 3.0, 4.0);
        let shape: &dyn PaintShape = &rect;
        assert_eq!(shape.as_rect(), Some(rect));

        let rounded = kurbo::RoundedRect::from_rect(rect, 2.0);
        let shape: &dyn PaintShape = &rounded;
        assert_eq!(shape.as_rect(), None);
        assert_eq!(shape.bounding_box(), rect);
    }

    #[test]
    fn visiting_path_elements_yields_a_closed_rectangle() {
        let mut elements = Vec::new();
        let shape: &dyn PaintShape = &Rect::new(0.0, 0.0, 1.0, 1.0);
        shape.visit_path_elements(0.1, &mut |element| elements.push(element));

        assert!(matches!(elements.first(), Some(PathEl::MoveTo(_))));
        assert!(matches!(elements.last(), Some(PathEl::ClosePath)));
    }
}
