//! `SkiaPainter` — the primary [`PaintTarget`] backend, over `skia-safe`.
//!
//! This painter owns a **raster** `SkSurface` and hands out its pixels. It owns no
//! GPU context and holds no wgpu handle; getting those pixels onto the screen is
//! `otlyra-platform`'s job. See `docs/skia-wgpu-interop.md` for why.

use kurbo::{Affine, PathEl, Rect, Stroke};
use peniko::{BlendMode, BrushRef, Color, Fill, FontData, ImageBrushRef};
use skia_safe as sk;

use crate::paint_target::{Glyph, PaintShape, PaintTarget};

/// Flattening tolerance for curves Skia cannot consume directly. Quadratics and
/// cubics pass through intact, so today this only bounds shape iteration.
const PATH_TOLERANCE: f64 = 0.1;

/// Failures that can occur setting up or reading back a Skia surface.
#[derive(Debug, thiserror::Error)]
pub enum SkiaError {
    /// Skia declined to allocate a surface of the requested size.
    #[error("skia refused to allocate a {width}x{height} raster surface")]
    SurfaceAllocation {
        /// Requested width in device pixels.
        width: u32,
        /// Requested height in device pixels.
        height: u32,
    },
    /// `readPixels` failed, which in practice means an incompatible `ImageInfo`.
    #[error("skia read_pixels failed for a {width}x{height} surface")]
    ReadPixels {
        /// Surface width in device pixels.
        width: u32,
        /// Surface height in device pixels.
        height: u32,
    },
    /// PNG encoding failed.
    #[error("skia failed to encode the surface as PNG")]
    PngEncode,
    /// An encoded image could not be decoded.
    #[error("skia could not decode the image")]
    ImageDecode,
    /// A zero-area surface was requested. Callers must clamp before asking.
    #[error("surface dimensions must be non-zero, got {width}x{height}")]
    ZeroSize {
        /// Requested width in device pixels.
        width: u32,
        /// Requested height in device pixels.
        height: u32,
    },
}

/// A [`PaintTarget`] that rasterizes into a Skia raster surface.
pub struct SkiaPainter {
    surface: sk::Surface,
    width: u32,
    height: u32,
    /// Number of `save`/`save_layer` pairs currently open, so that an unbalanced
    /// [`PaintTarget::pop_layer`] cannot restore past our own baseline.
    layer_depth: usize,
    typefaces: TypefaceCache,
}

/// Parsing a font file is expensive and every glyph run asks for the same handful
/// of fonts, so typefaces are cached by the identity of their backing blob rather
/// than re-parsed per run.
#[derive(Default)]
struct TypefaceCache {
    /// Keyed by the blob's address, the face index, and the variation
    /// coordinates: a bold instance of a variable font is a different typeface to
    /// Skia, and the whole point of caching is not to build it twice.
    entries: Vec<(usize, u32, Vec<i16>, sk::Typeface)>,
    font_mgr: Option<sk::FontMgr>,
}

impl TypefaceCache {
    fn get(&mut self, font: &FontData, normalized_coords: &[i16]) -> Option<sk::Typeface> {
        // `Blob` is reference-counted and shared, so its data pointer is a stable
        // identity for as long as anyone holds the font.
        let key = font.data.as_ref().as_ptr() as usize;
        let index = font.index;

        if let Some((_, _, _, typeface)) = self.entries.iter().find(|(k, i, coords, _)| {
            *k == key && *i == index && coords.as_slice() == normalized_coords
        }) {
            return Some(typeface.clone());
        }

        let font_mgr = self.font_mgr.get_or_insert_with(sk::FontMgr::new);
        let base = font_mgr.new_from_data(font.data.as_ref(), index as usize)?;
        let typeface = instantiate(&base, normalized_coords).unwrap_or(base);

        self.entries
            .push((key, index, normalized_coords.to_vec(), typeface.clone()));
        Some(typeface)
    }
}

/// Build the variable-font instance the shaper asked for.
///
/// The shaper reports the position it chose as **normalized** coordinates — the
/// OpenType convention where each axis runs -1 to 1 around its default — while Skia
/// wants design-space values. The mapping is linear on each side of the default,
/// which is the same conversion the specification defines, minus `avar`: a font with
/// an axis-variation table will land slightly off the requested weight. That is a
/// visible-if-you-look difference, not a missing feature, and it costs no glyphs.
///
/// Without this, every variable font renders at its default instance — and on macOS
/// the system UI font is variable, so `<b>` and every heading come out at regular
/// weight. That is what this exists to fix.
fn instantiate(typeface: &sk::Typeface, normalized_coords: &[i16]) -> Option<sk::Typeface> {
    if normalized_coords.is_empty() {
        return None;
    }
    let axes = typeface.variation_design_parameters()?;

    let coordinates: Vec<sk::font_arguments::variation_position::Coordinate> = axes
        .iter()
        .zip(normalized_coords)
        .map(|(axis, &normalized)| {
            // F2Dot14: 1.0 is 16384.
            let normalized = f32::from(normalized) / 16384.0;
            let value = if normalized >= 0.0 {
                axis.def + normalized * (axis.max - axis.def)
            } else {
                axis.def + normalized * (axis.def - axis.min)
            };
            sk::font_arguments::variation_position::Coordinate {
                axis: axis.tag,
                value,
            }
        })
        .collect();

    typeface.clone_with_arguments(&sk::FontArguments::new().set_variation_design_position(
        sk::font_arguments::VariationPosition {
            coordinates: &coordinates,
        },
    ))
}

impl std::fmt::Debug for SkiaPainter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkiaPainter")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("layer_depth", &self.layer_depth)
            .finish()
    }
}

impl SkiaPainter {
    /// Allocate a raster surface of `width` x `height` **device** pixels.
    ///
    /// The caller is responsible for having multiplied by the HiDPI scale factor;
    /// this type knows nothing about logical coordinates.
    pub fn new_raster(width: u32, height: u32) -> Result<Self, SkiaError> {
        if width == 0 || height == 0 {
            return Err(SkiaError::ZeroSize { width, height });
        }
        let surface = sk::surfaces::raster_n32_premul((width as i32, height as i32))
            .ok_or(SkiaError::SurfaceAllocation { width, height })?;
        Ok(Self {
            surface,
            width,
            height,
            layer_depth: 0,
            typefaces: TypefaceCache::default(),
        })
    }

    /// Device-pixel dimensions of the surface.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Reallocate if the requested size differs. Returns `true` if a reallocation
    /// happened, so callers can invalidate anything keyed on surface identity.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<bool, SkiaError> {
        if (width, height) == (self.width, self.height) {
            return Ok(false);
        }
        // Keep the typeface cache across a resize: the fonts have not changed, and
        // re-parsing every font on every window drag is pure waste.
        let typefaces = std::mem::take(&mut self.typefaces);
        *self = Self::new_raster(width, height)?;
        self.typefaces = typefaces;
        Ok(true)
    }

    /// Fill the whole surface with `color`, discarding everything previously drawn.
    pub fn clear(&mut self, color: Color) {
        self.surface.canvas().clear(to_skia_color(color));
    }

    /// Read the surface back as tightly packed, premultiplied RGBA8, stride
    /// exactly `width * 4`. This is what `otlyra-platform` uploads to wgpu.
    pub fn read_rgba8(&mut self) -> Result<Vec<u8>, SkiaError> {
        let info = sk::ImageInfo::new(
            (self.width as i32, self.height as i32),
            sk::ColorType::RGBA8888,
            sk::AlphaType::Premul,
            None,
        );
        let row_bytes = self.width as usize * 4;
        let mut pixels = vec![0u8; row_bytes * self.height as usize];
        if self
            .surface
            .read_pixels(&info, &mut pixels, row_bytes, (0, 0))
        {
            Ok(pixels)
        } else {
            Err(SkiaError::ReadPixels {
                width: self.width,
                height: self.height,
            })
        }
    }

    /// Encode the current surface contents as a PNG. This is what `--screenshot`
    /// writes and what the image tests compare, so it must stay deterministic.
    pub fn encode_png(&mut self) -> Result<Vec<u8>, SkiaError> {
        let image = self.surface.image_snapshot();
        let data = image
            .encode(
                self.surface.direct_context().as_mut(),
                sk::EncodedImageFormat::PNG,
                None,
            )
            .ok_or(SkiaError::PngEncode)?;
        Ok(data.as_bytes().to_vec())
    }

    fn canvas(&mut self) -> &sk::Canvas {
        self.surface.canvas()
    }
}

// ---------------------------------------------------------------------------
// kurbo/peniko -> skia conversions
// ---------------------------------------------------------------------------

fn to_skia_color(color: Color) -> sk::Color4f {
    let [r, g, b, a] = color.to_rgba8().to_u8_array();
    sk::Color4f::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        f32::from(a) / 255.0,
    )
}

fn to_skia_matrix(transform: Affine) -> sk::Matrix {
    // kurbo stores [a b c d e f] with x' = a*x + c*y + e and y' = b*x + d*y + f.
    // Skia's `new_all` takes (scale_x, skew_x, trans_x, skew_y, scale_y, trans_y, ..).
    let [a, b, c, d, e, f] = transform.as_coeffs();
    sk::Matrix::new_all(
        a as f32, c as f32, e as f32, b as f32, d as f32, f as f32, 0.0, 0.0, 1.0,
    )
}

fn to_skia_path(shape: &dyn PaintShape, fill_type: sk::PathFillType) -> sk::Path {
    // `SkPath` is immutable as of Skia m150; construction goes through
    // `SkPathBuilder` and ends in a single `detach`.
    let mut builder = sk::PathBuilder::new_with_fill_type(fill_type);
    shape.visit_path_elements(PATH_TOLERANCE, &mut |element| match element {
        PathEl::MoveTo(p) => {
            builder.move_to((p.x as f32, p.y as f32));
        }
        PathEl::LineTo(p) => {
            builder.line_to((p.x as f32, p.y as f32));
        }
        PathEl::QuadTo(c, p) => {
            builder.quad_to((c.x as f32, c.y as f32), (p.x as f32, p.y as f32));
        }
        PathEl::CurveTo(c1, c2, p) => {
            builder.cubic_to(
                (c1.x as f32, c1.y as f32),
                (c2.x as f32, c2.y as f32),
                (p.x as f32, p.y as f32),
            );
        }
        PathEl::ClosePath => {
            builder.close();
        }
    });
    builder.detach()
}

fn to_skia_blend(blend: BlendMode) -> sk::BlendMode {
    use peniko::{Compose, Mix};
    // `Mix` maps onto Skia's separable and non-separable modes, `Compose` onto the
    // Porter-Duff set. A pairing of both cannot be one Skia mode, so the mix term
    // wins: that is the one CSS authors reach for.
    match (blend.mix, blend.compose) {
        (Mix::Normal, compose) => match compose {
            Compose::Clear => sk::BlendMode::Clear,
            Compose::Copy => sk::BlendMode::Src,
            Compose::Dest => sk::BlendMode::Dst,
            Compose::SrcOver => sk::BlendMode::SrcOver,
            Compose::DestOver => sk::BlendMode::DstOver,
            Compose::SrcIn => sk::BlendMode::SrcIn,
            Compose::DestIn => sk::BlendMode::DstIn,
            Compose::SrcOut => sk::BlendMode::SrcOut,
            Compose::DestOut => sk::BlendMode::DstOut,
            Compose::SrcAtop => sk::BlendMode::SrcATop,
            Compose::DestAtop => sk::BlendMode::DstATop,
            Compose::Xor => sk::BlendMode::Xor,
            Compose::Plus => sk::BlendMode::Plus,
            Compose::PlusLighter => sk::BlendMode::Plus,
        },
        (mix, _) => match mix {
            Mix::Normal => sk::BlendMode::SrcOver,
            Mix::Multiply => sk::BlendMode::Multiply,
            Mix::Screen => sk::BlendMode::Screen,
            Mix::Overlay => sk::BlendMode::Overlay,
            Mix::Darken => sk::BlendMode::Darken,
            Mix::Lighten => sk::BlendMode::Lighten,
            Mix::ColorDodge => sk::BlendMode::ColorDodge,
            Mix::ColorBurn => sk::BlendMode::ColorBurn,
            Mix::HardLight => sk::BlendMode::HardLight,
            Mix::SoftLight => sk::BlendMode::SoftLight,
            Mix::Difference => sk::BlendMode::Difference,
            Mix::Exclusion => sk::BlendMode::Exclusion,
            Mix::Hue => sk::BlendMode::Hue,
            Mix::Saturation => sk::BlendMode::Saturation,
            Mix::Color => sk::BlendMode::Color,
            Mix::Luminosity => sk::BlendMode::Luminosity,
        },
    }
}

/// Build a Skia `Paint` from a peniko brush.
///
/// Gradients and image brushes are not lowered yet. Falling back to a flat colour
/// and logging it beats painting nothing or painting a wrong gradient silently.
fn to_skia_paint(brush: BrushRef<'_>, brush_transform: Option<Affine>) -> sk::Paint {
    let mut paint = sk::Paint::default();
    paint.set_anti_alias(true);

    let color = match brush {
        BrushRef::Solid(color) => color,
        BrushRef::Gradient(gradient) => {
            tracing::warn!(
                stops = gradient.stops.len(),
                "gradient brushes are not lowered to skia yet; using the first stop"
            );
            gradient
                .stops
                .first()
                .map(|stop| stop.color.to_alpha_color())
                .unwrap_or(Color::BLACK)
        }
        BrushRef::Image(_) => {
            tracing::warn!("image brushes are not lowered to skia yet; using transparent black");
            Color::TRANSPARENT
        }
    };
    paint.set_color4f(to_skia_color(color), None);

    if brush_transform.is_some() {
        tracing::warn!("brush_transform is ignored until gradient brushes are lowered");
    }
    paint
}

fn apply_stroke(paint: &mut sk::Paint, style: &Stroke) {
    use kurbo::{Cap, Join};
    paint.set_style(sk::paint::Style::Stroke);
    paint.set_stroke_width(style.width as f32);
    paint.set_stroke_miter(style.miter_limit as f32);
    paint.set_stroke_cap(match style.start_cap {
        Cap::Butt => sk::paint::Cap::Butt,
        Cap::Square => sk::paint::Cap::Square,
        Cap::Round => sk::paint::Cap::Round,
    });
    paint.set_stroke_join(match style.join {
        Join::Bevel => sk::paint::Join::Bevel,
        Join::Miter => sk::paint::Join::Miter,
        Join::Round => sk::paint::Join::Round,
    });
}

impl PaintTarget for SkiaPainter {
    fn reset(&mut self) {
        // Unwind layers a malformed frame left open, or the clear lands inside a
        // stale layer instead of on the surface.
        while self.layer_depth > 0 {
            self.pop_layer();
        }
        self.surface.canvas().clear(sk::Color::TRANSPARENT);
    }

    fn push_layer(
        &mut self,
        blend: BlendMode,
        alpha: f32,
        transform: Affine,
        clip: &dyn PaintShape,
    ) {
        let matrix = to_skia_matrix(transform);
        let path = to_skia_path(clip, sk::PathFillType::Winding);
        let mut paint = sk::Paint::default();
        paint.set_alpha_f(alpha);
        paint.set_blend_mode(to_skia_blend(blend));

        let canvas = self.canvas();
        canvas.save();
        canvas.concat(&matrix);
        canvas.clip_path(&path, sk::ClipOp::Intersect, true);
        canvas.save_layer(&sk::canvas::SaveLayerRec::default().paint(&paint));
        self.layer_depth += 1;
    }

    fn pop_layer(&mut self) {
        if self.layer_depth == 0 {
            tracing::warn!("pop_layer with no layer open; ignoring");
            return;
        }
        self.layer_depth -= 1;
        let canvas = self.canvas();
        canvas.restore(); // the save_layer
        canvas.restore(); // the save carrying the clip and transform
    }

    fn fill(
        &mut self,
        style: Fill,
        transform: Affine,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        shape: &dyn PaintShape,
    ) {
        let matrix = to_skia_matrix(transform);
        let path = to_skia_path(
            shape,
            match style {
                Fill::NonZero => sk::PathFillType::Winding,
                Fill::EvenOdd => sk::PathFillType::EvenOdd,
            },
        );
        let mut paint = to_skia_paint(brush, brush_transform);
        paint.set_style(sk::paint::Style::Fill);

        let canvas = self.canvas();
        canvas.save();
        canvas.concat(&matrix);
        canvas.draw_path(&path, &paint);
        canvas.restore();
    }

    fn stroke(
        &mut self,
        style: &Stroke,
        transform: Affine,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        shape: &dyn PaintShape,
    ) {
        let matrix = to_skia_matrix(transform);
        let path = to_skia_path(shape, sk::PathFillType::Winding);
        let mut paint = to_skia_paint(brush, brush_transform);
        apply_stroke(&mut paint, style);

        let canvas = self.canvas();
        canvas.save();
        canvas.concat(&matrix);
        canvas.draw_path(&path, &paint);
        canvas.restore();
    }

    fn draw_glyphs(
        &mut self,
        font: &FontData,
        font_size: f32,
        normalized_coords: &[i16],
        brush: BrushRef<'_>,
        transform: Affine,
        hint: bool,
        glyphs: &mut dyn Iterator<Item = Glyph>,
    ) {
        // Drain the iterator before any early return: the caller's contract is that
        // it is consumed exactly once.
        let (ids, positions): (Vec<sk::GlyphId>, Vec<sk::Point>) = glyphs
            .map(|glyph| (glyph.id as sk::GlyphId, sk::Point::new(glyph.x, glyph.y)))
            .unzip();

        if ids.is_empty() {
            return;
        }

        let Some(typeface) = self.typefaces.get(font, normalized_coords) else {
            tracing::error!("skia could not parse the font for a glyph run; dropping it");
            return;
        };

        let mut sk_font = sk::Font::from_typeface(typeface, font_size);
        // Grid fitting must be off for transformed or animating text, which is
        // exactly what `hint` reports.
        sk_font.set_subpixel(!hint);
        sk_font.set_hinting(if hint {
            sk::FontHinting::Slight
        } else {
            sk::FontHinting::None
        });
        sk_font.set_edging(sk::font::Edging::AntiAlias);

        let mut paint = to_skia_paint(brush, None);
        paint.set_style(sk::paint::Style::Fill);

        let canvas = self.canvas();
        canvas.save();
        canvas.concat(&to_skia_matrix(transform));
        canvas.draw_glyphs_at(
            &ids,
            positions.as_slice(),
            sk::Point::new(0.0, 0.0),
            &sk_font,
            &paint,
        );
        canvas.restore();
    }

    fn draw_image(&mut self, image: ImageBrushRef<'_>, transform: Affine, clip_rect: Option<Rect>) {
        let data = image.image;
        let Some(expected) = data.format.size_in_bytes(data.width, data.height) else {
            tracing::error!(
                width = data.width,
                height = data.height,
                "image dimensions overflow"
            );
            return;
        };
        if data.data.as_ref().len() < expected {
            tracing::error!(
                got = data.data.as_ref().len(),
                expected,
                "image buffer is smaller than its dimensions claim"
            );
            return;
        }

        let color_type = match data.format {
            peniko::ImageFormat::Rgba8 => sk::ColorType::RGBA8888,
            peniko::ImageFormat::Bgra8 => sk::ColorType::BGRA8888,
            // `ImageFormat` is non-exhaustive; an unknown format is better dropped
            // loudly than guessed at.
            other => {
                tracing::error!(?other, "unsupported image format");
                return;
            }
        };
        let alpha_type = match data.alpha_type {
            peniko::ImageAlphaType::Alpha => sk::AlphaType::Unpremul,
            peniko::ImageAlphaType::AlphaPremultiplied => sk::AlphaType::Premul,
        };

        let info = sk::ImageInfo::new(
            (data.width as i32, data.height as i32),
            color_type,
            alpha_type,
            None,
        );
        let row_bytes = data.width as usize * 4;
        let pixels = sk::Data::new_copy(&data.data.as_ref()[..expected]);
        let Some(sk_image) = sk::images::raster_from_data(&info, pixels, row_bytes) else {
            tracing::error!("skia rejected the image data");
            return;
        };

        let mut paint = sk::Paint::default();
        paint.set_anti_alias(true);
        paint.set_alpha_f(image.sampler.alpha);

        let sampling = match image.sampler.quality {
            peniko::ImageQuality::Low => {
                sk::SamplingOptions::new(sk::FilterMode::Nearest, sk::MipmapMode::None)
            }
            _ => sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::Linear),
        };

        let canvas = self.canvas();
        canvas.save();
        canvas.concat(&to_skia_matrix(transform));
        if let Some(clip) = clip_rect {
            canvas.clip_rect(
                sk::Rect::new(
                    clip.x0 as f32,
                    clip.y0 as f32,
                    clip.x1 as f32,
                    clip.y1 as f32,
                ),
                sk::ClipOp::Intersect,
                true,
            );
        }
        canvas.draw_image_with_sampling_options(&sk_image, (0.0, 0.0), sampling, Some(&paint));
        canvas.restore();
    }
}

/// Decode an encoded image (PNG, JPEG, …) into pixels the display list can carry.
///
/// Skia is already a dependency and already contains decoders, so this costs no new
/// crate. When resource loading exists this moves out of the renderer.
pub fn decode_image(bytes: &[u8]) -> Result<peniko::ImageData, SkiaError> {
    let data = sk::Data::new_copy(bytes);
    let image = sk::Image::from_encoded(data).ok_or(SkiaError::ImageDecode)?;

    let width = image.width() as u32;
    let height = image.height() as u32;
    let info = sk::ImageInfo::new(
        (image.width(), image.height()),
        sk::ColorType::RGBA8888,
        sk::AlphaType::Premul,
        None,
    );

    let row_bytes = width as usize * 4;
    let mut pixels = vec![0u8; row_bytes * height as usize];
    if !image.read_pixels(
        &info,
        &mut pixels,
        row_bytes,
        (0, 0),
        sk::image::CachingHint::Allow,
    ) {
        return Err(SkiaError::ImageDecode);
    }

    Ok(peniko::ImageData {
        data: peniko::Blob::new(std::sync::Arc::new(pixels)),
        format: peniko::ImageFormat::Rgba8,
        alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pixel read back from a `read_rgba8` buffer.
    fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    #[test]
    fn zero_sized_surfaces_are_rejected_rather_than_clamped() {
        assert!(matches!(
            SkiaPainter::new_raster(0, 10),
            Err(SkiaError::ZeroSize {
                width: 0,
                height: 10
            })
        ));
    }

    #[test]
    fn clear_then_read_back_gives_the_clear_colour() {
        let mut painter = SkiaPainter::new_raster(8, 4).expect("8x4 raster surface");
        painter.clear(Color::from_rgb8(0x22, 0x44, 0x88));
        let pixels = painter.read_rgba8().expect("read back");
        assert_eq!(pixels.len(), 8 * 4 * 4);
        assert_eq!(pixel_at(&pixels, 8, 0, 0), [0x22, 0x44, 0x88, 0xff]);
        assert_eq!(pixel_at(&pixels, 8, 7, 3), [0x22, 0x44, 0x88, 0xff]);
    }

    #[test]
    fn fill_rect_lands_where_the_geometry_says_it_should() {
        let mut painter = SkiaPainter::new_raster(16, 16).expect("16x16 raster surface");
        painter.clear(Color::WHITE);
        let brush = peniko::Brush::Solid(Color::from_rgb8(0xff, 0x00, 0x00));
        painter.fill_rect(
            Affine::IDENTITY,
            (&brush).into(),
            Rect::new(4.0, 4.0, 12.0, 12.0),
        );

        let pixels = painter.read_rgba8().expect("read back");
        assert_eq!(
            pixel_at(&pixels, 16, 8, 8),
            [0xff, 0x00, 0x00, 0xff],
            "inside the rect"
        );
        assert_eq!(
            pixel_at(&pixels, 16, 1, 1),
            [0xff, 0xff, 0xff, 0xff],
            "outside the rect"
        );
    }

    #[test]
    fn transforms_translate_geometry() {
        let mut painter = SkiaPainter::new_raster(16, 16).expect("16x16 raster surface");
        painter.clear(Color::WHITE);
        let brush = peniko::Brush::Solid(Color::BLACK);
        painter.fill_rect(
            Affine::translate((8.0, 0.0)),
            (&brush).into(),
            Rect::new(0.0, 0.0, 4.0, 4.0),
        );

        let pixels = painter.read_rgba8().expect("read back");
        assert_eq!(
            pixel_at(&pixels, 16, 10, 2),
            [0x00, 0x00, 0x00, 0xff],
            "shifted right by 8"
        );
        assert_eq!(
            pixel_at(&pixels, 16, 2, 2),
            [0xff, 0xff, 0xff, 0xff],
            "origin is now empty"
        );
    }

    #[test]
    fn clip_layers_bound_what_is_painted() {
        let mut painter = SkiaPainter::new_raster(16, 16).expect("16x16 raster surface");
        painter.clear(Color::WHITE);
        let brush = peniko::Brush::Solid(Color::BLACK);

        painter.push_clip_rect(Affine::IDENTITY, Rect::new(0.0, 0.0, 8.0, 16.0));
        painter.fill_rect(
            Affine::IDENTITY,
            (&brush).into(),
            Rect::new(0.0, 0.0, 16.0, 16.0),
        );
        painter.pop_layer();

        let pixels = painter.read_rgba8().expect("read back");
        assert_eq!(
            pixel_at(&pixels, 16, 4, 8),
            [0x00, 0x00, 0x00, 0xff],
            "inside the clip"
        );
        assert_eq!(
            pixel_at(&pixels, 16, 12, 8),
            [0xff, 0xff, 0xff, 0xff],
            "outside the clip"
        );
        assert_eq!(painter.layer_depth, 0);
    }

    #[test]
    fn encode_png_produces_a_png_signature() {
        let mut painter = SkiaPainter::new_raster(4, 4).expect("4x4 raster surface");
        painter.clear(Color::WHITE);
        let png = painter.encode_png().expect("encode");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
