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
    /// The blur the run being drawn asks for, while it is being drawn.
    blur: Option<f64>,
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
    /// Configured fonts, keyed by typeface — blob, index *and* variation
    /// coordinates — size and hinting.
    ///
    /// The coordinates belong in the key for the same reason they belong in the
    /// typeface key: bold and regular of a variable font share a blob, so leaving
    /// them out hands the bold run whichever weight was configured first.
    ///
    /// An `SkFont` is not a handle to a typeface; it is the key to a *strike* — the
    /// rasterized glyphs at one size with one set of flags. Building a new one per
    /// glyph run makes Skia rasterize the same glyphs again for every run on the
    /// page, which costs the same whatever the window size, and on a text-heavy
    /// page it is the single largest cost in the frame.
    fonts: Vec<(usize, u32, Vec<i16>, u32, bool, sk::Font)>,
    font_mgr: Option<sk::FontMgr>,
}

impl TypefaceCache {
    /// A configured font for this typeface at this size, from the cache.
    fn font(
        &mut self,
        font: &FontData,
        normalized_coords: &[i16],
        size: f32,
        hint: bool,
    ) -> Option<sk::Font> {
        let key = font.data.as_ref().as_ptr() as usize;
        // Size is compared by bits: it comes from a layout that produced the same
        // number for every run at that size, so exact equality is what is wanted.
        let bits = size.to_bits();

        if let Some((_, _, _, _, _, cached)) =
            self.fonts.iter().find(|(k, index, coords, s, h, _)| {
                *k == key
                    && *index == font.index
                    && coords.as_slice() == normalized_coords
                    && *s == bits
                    && *h == hint
            })
        {
            return Some(cached.clone());
        }

        let typeface = self.get(font, normalized_coords)?;
        let mut configured = sk::Font::from_typeface(typeface, size);
        // Grid fitting must be off for transformed or animating text, which is
        // exactly what `hint` reports.
        configured.set_subpixel(!hint);
        configured.set_hinting(if hint {
            sk::FontHinting::Slight
        } else {
            sk::FontHinting::None
        });
        configured.set_edging(sk::font::Edging::AntiAlias);

        self.fonts.push((
            key,
            font.index,
            normalized_coords.to_vec(),
            bits,
            hint,
            configured.clone(),
        ));
        Some(configured)
    }

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
        let bytes = font.data.as_ref();
        let base = match font_mgr.new_from_data(bytes, index as usize) {
            Some(typeface) => typeface,
            // A face inside a font collection: the platform's font manager here
            // takes a collection only at index zero, so the face is lifted out into
            // a font of its own and handed over as that. Without this, every face
            // past the first in a `.ttc` — which on this platform is every bold and
            // italic monospace — draws nothing at all.
            None => {
                let extracted = face_from_collection(bytes, index as usize)?;
                font_mgr.new_from_data(&extracted, 0)?
            }
        };
        let typeface = instantiate(
            &base,
            normalized_coords,
            &axis_segment_maps(bytes, index as usize),
        )
        .unwrap_or(base);

        self.entries
            .push((key, index, normalized_coords.to_vec(), typeface.clone()));
        Some(typeface)
    }
}

/// Read a big-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Read a big-endian `u16` at `at`.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// Where face `index`'s table directory starts, in a font file or a collection.
fn table_directory(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(..4)? != b"ttcf" {
        return (index == 0).then_some(0);
    }
    let faces = u32_at(bytes, 8)? as usize;
    if index >= faces {
        return None;
    }
    Some(u32_at(bytes, 12 + index * 4)? as usize)
}

/// One table of one face, by tag.
fn table<'a>(bytes: &'a [u8], index: usize, tag: &[u8; 4]) -> Option<&'a [u8]> {
    const HEADER: usize = 12;
    const RECORD: usize = 16;

    let directory = table_directory(bytes, index)?;
    let tables = u16_at(bytes, directory + 4)? as usize;
    for table in 0..tables {
        let record = directory + HEADER + table * RECORD;
        if bytes.get(record..record + 4)? != tag {
            continue;
        }
        let offset = u32_at(bytes, record + 8)? as usize;
        let length = u32_at(bytes, record + 12)? as usize;
        return bytes.get(offset..offset + length);
    }
    None
}

/// Lift one face out of a font collection into a font of its own.
///
/// A collection is a header, a list of offsets, and one table directory per face
/// pointing into shared table data. A single font is the same table directory with
/// the tables behind it — so this copies the records, copies the tables they name,
/// and fixes the offsets. Checksums are the originals and stay correct, because no
/// table's bytes change.
fn face_from_collection(bytes: &[u8], index: usize) -> Option<Vec<u8>> {
    if bytes.get(..4)? != b"ttcf" {
        return None;
    }
    let faces = u32_at(bytes, 8)? as usize;

    // The face's own table directory.
    let directory = table_directory(bytes, index)?;
    let sfnt_version = u32_at(bytes, directory)?;
    let tables = u16_at(bytes, directory + 4)? as usize;

    const HEADER: usize = 12;
    const RECORD: usize = 16;
    let mut out = Vec::with_capacity(bytes.len() / faces.max(1));
    out.extend_from_slice(&sfnt_version.to_be_bytes());
    out.extend_from_slice(&(tables as u16).to_be_bytes());
    // The three fields after the count are a binary-search hint, and are allowed to
    // be anything a reader can survive; every reader recomputes them.
    for value in [
        u16_at(bytes, directory + 6)?,
        u16_at(bytes, directory + 8)?,
        u16_at(bytes, directory + 10)?,
    ] {
        out.extend_from_slice(&value.to_be_bytes());
    }

    // The records first, with room for the offsets that are not known until the
    // tables have been placed.
    let records_at = out.len();
    out.resize(HEADER + tables * RECORD, 0);

    for table in 0..tables {
        let record = directory + HEADER + table * RECORD;
        let tag = bytes.get(record..record + 4)?;
        let checksum = u32_at(bytes, record + 4)?;
        let offset = u32_at(bytes, record + 8)? as usize;
        let length = u32_at(bytes, record + 12)? as usize;
        let data = bytes.get(offset..offset + length)?;

        // Tables start on four-byte boundaries, as the format requires.
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let placed = out.len() as u32;
        out.extend_from_slice(data);

        let record_out = records_at + table * RECORD;
        out[record_out..record_out + 4].copy_from_slice(tag);
        out[record_out + 4..record_out + 8].copy_from_slice(&checksum.to_be_bytes());
        out[record_out + 8..record_out + 12].copy_from_slice(&placed.to_be_bytes());
        out[record_out + 12..record_out + 16].copy_from_slice(&(length as u32).to_be_bytes());
    }

    Some(out)
}

/// One axis's `avar` segment map: pairs of coordinates, both normalized, the first
/// what the linear normalization produced and the second what the font wants used.
///
/// Empty when the font has no such table, which means the identity.
type SegmentMap = Vec<(f32, f32)>;

/// The `avar` segment maps of a face, one per variation axis, in `fvar` order.
///
/// Only the version 1 body is read. Version 2 keeps that body byte for byte and
/// adds a second, item-variation-store stage after it, so what is read here is
/// still the right first half of a version 2 mapping.
fn axis_segment_maps(bytes: &[u8], index: usize) -> Vec<SegmentMap> {
    fn parse(table: &[u8]) -> Option<Vec<SegmentMap>> {
        fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
            Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
        }
        fn f2dot14_at(bytes: &[u8], at: usize) -> Option<f32> {
            Some(f32::from(i16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?)) / 16384.0)
        }

        if u16_at(table, 0)? != 1 && u16_at(table, 0)? != 2 {
            return None;
        }
        let axes = u16_at(table, 6)? as usize;
        let mut maps = Vec::with_capacity(axes);
        let mut at = 8;
        for _ in 0..axes {
            let pairs = u16_at(table, at)? as usize;
            at += 2;
            let mut map = Vec::with_capacity(pairs);
            for _ in 0..pairs {
                map.push((f2dot14_at(table, at)?, f2dot14_at(table, at + 2)?));
                at += 4;
            }
            maps.push(map);
        }
        Some(maps)
    }

    table(bytes, index, b"avar")
        .and_then(parse)
        .unwrap_or_default()
}

/// Undo one axis's `avar` mapping: the coordinate that maps *to* `mapped`.
///
/// The table's `to` coordinates are required to be non-decreasing, so the segment
/// containing `mapped` is found by walking them and the position within it is the
/// same linear interpolation the forward direction uses. A segment with no width
/// cannot be inverted and gives back its own start, which is the only answer that
/// is in range.
fn unmap_axis(map: &SegmentMap, mapped: f32) -> f32 {
    if map.len() < 2 {
        return mapped;
    }
    if mapped <= map[0].1 {
        return map[0].0;
    }
    for pair in map.windows(2) {
        let ((from_low, to_low), (from_high, to_high)) = (pair[0], pair[1]);
        if mapped <= to_high {
            let span = to_high - to_low;
            if span <= 0.0 {
                return from_low;
            }
            return from_low + (mapped - to_low) / span * (from_high - from_low);
        }
    }
    map[map.len() - 1].0
}

/// Build the variable-font instance the shaper asked for.
///
/// The shaper reports the position it chose as **normalized** coordinates — the
/// OpenType convention where each axis runs -1 to 1 around its default — while Skia
/// wants design-space values, and normalizes them again itself. So this runs the
/// conversion backwards: undo the font's own `avar` mapping, then undo the linear
/// normalization, which is linear on each side of the default.
///
/// Skipping `avar` was the earlier shape of this, and it lands a font that has the
/// table off the position it was shaped at — on this platform, the system interface
/// font's optical-size axis is mapped that way, so the glyphs are painted from a
/// slightly different design to the one they were measured in.
///
/// Without any of this, a variable font renders at its default instance — on this
/// platform the system interface font is variable, so `<b>` and every heading come
/// out at regular weight. That is what this exists to fix.
fn instantiate(
    typeface: &sk::Typeface,
    normalized_coords: &[i16],
    segment_maps: &[SegmentMap],
) -> Option<sk::Typeface> {
    if normalized_coords.is_empty() {
        return None;
    }
    let axes = typeface.variation_design_parameters()?;

    let coordinates: Vec<sk::font_arguments::variation_position::Coordinate> = axes
        .iter()
        .zip(normalized_coords)
        .enumerate()
        .map(|(index, (axis, &normalized))| {
            // F2Dot14: 1.0 is 16384.
            let normalized = f32::from(normalized) / 16384.0;
            let normalized = match segment_maps.get(index) {
                Some(map) => unmap_axis(map, normalized),
                None => normalized,
            };
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
            blur: None,
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

    /// Read one device-pixel rectangle back as tightly packed, premultiplied
    /// RGBA8, stride exactly `width * 4`.
    ///
    /// This is the retained compositor's readback: after re-rasterizing only the
    /// damaged region, only that region is read and uploaded. The rectangle is
    /// clamped to the surface, so a damage rect that runs off the edge reads what
    /// is there rather than failing.
    pub fn read_rgba8_rect(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, SkiaError> {
        let x = x.min(self.width);
        let y = y.min(self.height);
        let width = width.min(self.width - x).max(1);
        let height = height.min(self.height - y).max(1);
        let info = sk::ImageInfo::new(
            (width as i32, height as i32),
            sk::ColorType::RGBA8888,
            sk::AlphaType::Premul,
            None,
        );
        let row_bytes = width as usize * 4;
        let mut pixels = vec![0u8; row_bytes * height as usize];
        if self
            .surface
            .read_pixels(&info, &mut pixels, row_bytes, (x as i32, y as i32))
        {
            Ok(pixels)
        } else {
            Err(SkiaError::ReadPixels { width, height })
        }
    }

    /// Replace one rectangle's pixels with transparency, honouring the surface's
    /// premultiplied alpha.
    ///
    /// The retained compositor calls this before re-rasterizing a damaged region:
    /// `PaintTarget::reset` clears the *whole* surface, which would discard the
    /// retained pixels this design keeps. `Clear` blend replaces rather than
    /// composites, so the region is genuinely blank and not merely darkened.
    pub fn clear_rect(&mut self, rect: Rect) {
        let mut paint = sk::Paint::default();
        paint.set_blend_mode(sk::BlendMode::Clear);
        paint.set_anti_alias(false);
        self.surface.canvas().draw_rect(
            sk::Rect::new(
                rect.x0 as f32,
                rect.y0 as f32,
                rect.x1 as f32,
                rect.y1 as f32,
            ),
            &paint,
        );
    }

    /// Restrict all subsequent drawing to `rect` until [`SkiaPainter::reset_clip`].
    ///
    /// The retained compositor sets this to the damaged region so re-rendering the
    /// layers that intersect it cannot touch a pixel outside it — an unchanged
    /// neighbour keeps exactly what it had. Balances one-to-one with `reset_clip`.
    pub fn clip_to(&mut self, rect: Rect) {
        let canvas = self.surface.canvas();
        canvas.save();
        canvas.clip_rect(
            sk::Rect::new(
                rect.x0 as f32,
                rect.y0 as f32,
                rect.x1 as f32,
                rect.y1 as f32,
            ),
            sk::ClipOp::Intersect,
            false,
        );
    }

    /// Undo the clip a matching [`SkiaPainter::clip_to`] installed.
    pub fn reset_clip(&mut self) {
        self.surface.canvas().restore();
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
/// Image brushes are not lowered yet; a gradient is, since a page that says
/// `linear-gradient` and gets its first colour is a page that looks wrong in a way
/// nobody can see the cause of.
fn to_skia_paint(brush: BrushRef<'_>, brush_transform: Option<Affine>) -> sk::Paint {
    let mut paint = sk::Paint::default();
    paint.set_anti_alias(true);

    match brush {
        BrushRef::Solid(color) => {
            paint.set_color4f(to_skia_color(color), None);
        }
        BrushRef::Gradient(gradient) => match to_skia_gradient(gradient, brush_transform) {
            Some(shader) => {
                paint.set_shader(shader);
            }
            None => {
                let color = gradient
                    .stops
                    .first()
                    .map(|stop| stop.color.to_alpha_color())
                    .unwrap_or(Color::BLACK);
                paint.set_color4f(to_skia_color(color), None);
            }
        },
        BrushRef::Image(image) => match to_skia_image_shader(image, brush_transform) {
            Some(shader) => {
                paint.set_shader(shader);
                paint.set_alpha_f(image.sampler.alpha);
            }
            None => {
                paint.set_color4f(to_skia_color(Color::TRANSPARENT), None);
            }
        },
    }

    paint
}

/// A picture as a shader, so a shape can be filled with it repeated.
///
/// This is what tiles a background: one fill of the box rather than one drawing
/// command per tile, which is the difference between a small texture costing
/// nothing and costing a display-list item per square it covers.
///
/// The brush transform is the shader's own: it says where the picture's top left
/// corner lands and how large one tile is drawn, both in the space the shape is in.
fn to_skia_image_shader(
    image: ImageBrushRef<'_>,
    brush_transform: Option<Affine>,
) -> Option<sk::Shader> {
    fn tile_mode(extend: peniko::Extend) -> sk::TileMode {
        match extend {
            peniko::Extend::Pad => sk::TileMode::Clamp,
            peniko::Extend::Repeat => sk::TileMode::Repeat,
            peniko::Extend::Reflect => sk::TileMode::Mirror,
        }
    }

    let sk_image = to_skia_image(image.image)?;
    let matrix = brush_transform.map(to_skia_matrix);
    sk_image.to_shader(
        Some((
            tile_mode(image.sampler.x_extend),
            tile_mode(image.sampler.y_extend),
        )),
        to_skia_sampling(image.sampler.quality),
        matrix.as_ref(),
    )
}

/// Lower a peniko gradient to a Skia shader.
///
/// Linear and radial only, in the two extend modes a browser uses; a sweep gradient
/// is a shape CSS spells `conic-gradient` and nothing above asks for yet.
fn to_skia_gradient(
    gradient: &peniko::Gradient,
    brush_transform: Option<Affine>,
) -> Option<sk::Shader> {
    if gradient.stops.len() < 2 {
        return None;
    }

    let colors: Vec<sk::Color4f> = gradient
        .stops
        .iter()
        .map(|stop| to_skia_color(stop.color.to_alpha_color()))
        .collect();
    let offsets: Vec<f32> = gradient.stops.iter().map(|stop| stop.offset).collect();
    let mode = match gradient.extend {
        peniko::Extend::Pad => sk::TileMode::Clamp,
        peniko::Extend::Repeat => sk::TileMode::Repeat,
        peniko::Extend::Reflect => sk::TileMode::Mirror,
    };
    let matrix = brush_transform.map(to_skia_matrix);

    let stops = sk::gradient::Colors::new(&colors, Some(&offsets[..]), mode, None);
    let shader = sk::gradient::Gradient::new(stops, sk::gradient::Interpolation::default());

    match gradient.kind {
        peniko::GradientKind::Linear(line) => sk::shaders::linear_gradient(
            (
                sk::Point::new(line.start.x as f32, line.start.y as f32),
                sk::Point::new(line.end.x as f32, line.end.y as f32),
            ),
            &shader,
            matrix.as_ref(),
        ),
        peniko::GradientKind::Radial(circles) => sk::shaders::radial_gradient(
            (
                sk::Point::new(circles.end_center.x as f32, circles.end_center.y as f32),
                circles.end_radius,
            ),
            &shader,
            matrix.as_ref(),
        ),
        // A sweep gradient is `conic-gradient`, which nothing asks for yet.
        _ => None,
    }
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

    // A dashed line is a path effect here rather than a stroke setting, and it
    // wants the intervals in pairs: an odd list means the pattern alternates on
    // each repetition, which is not what anything asking for dashes wants.
    if !style.dash_pattern.is_empty() {
        let mut intervals: Vec<f32> = style.dash_pattern.iter().map(|on| *on as f32).collect();
        if intervals.len() % 2 == 1 {
            let repeated = intervals.clone();
            intervals.extend(repeated);
        }
        if let Some(effect) = sk::PathEffect::dash(&intervals, style.dash_offset as f32) {
            paint.set_path_effect(effect);
        }
    }
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
        // The transform positions the *clip*, and nothing else. Every item in
        // this list carries its own absolute transform — a fill inside a layer
        // is not expressed relative to it — so the matrix is reset once the clip
        // has been taken, or the layer's transform is applied a second time to
        // everything inside it. That is invisible while layers are only ever
        // pushed with the identity, and doubles a scrolling panel the moment the
        // whole list is scaled for a HiDPI screen.
        canvas.concat(&matrix);
        canvas.clip_path(&path, sk::ClipOp::Intersect, true);
        canvas.reset_matrix();
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

    fn fill_blurred(
        &mut self,
        transform: Affine,
        brush: BrushRef<'_>,
        blur: f64,
        shape: &dyn PaintShape,
    ) {
        let path = to_skia_path(shape, sk::PathFillType::Winding);
        let mut paint = to_skia_paint(brush, None);
        paint.set_style(sk::paint::Style::Fill);

        // CSS's blur radius is twice the standard deviation the blur is done with,
        // which is the conversion every engine applies and the reason a shadow
        // written as `10px` does not look ten pixels wide.
        if blur > 0.0
            && let Some(filter) =
                sk::MaskFilter::blur(sk::BlurStyle::Normal, (blur / 2.0) as f32, None)
        {
            paint.set_mask_filter(filter);
        }

        let matrix = to_skia_matrix(transform);
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
        self.blur = (blur > 0.0).then_some(blur);
        self.draw_glyphs(
            font,
            font_size,
            normalized_coords,
            brush,
            transform,
            hint,
            glyphs,
        );
        self.blur = None;
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

        let Some(sk_font) = self
            .typefaces
            .font(font, normalized_coords, font_size, hint)
        else {
            tracing::error!(
                bytes = font.data.as_ref().len(),
                index = font.index,
                glyphs = ids.len(),
                "skia could not parse the font for a glyph run; dropping it"
            );
            return;
        };

        let mut paint = to_skia_paint(brush, None);
        paint.set_style(sk::paint::Style::Fill);
        // A run being drawn as a shadow: the same glyphs, softened. Set by
        // `draw_glyph_run` for the length of one call rather than passed down, so
        // that the seven required methods keep the signatures they have.
        if let Some(blur) = self.blur
            && let Some(filter) =
                sk::MaskFilter::blur(sk::BlurStyle::Normal, (blur / 2.0) as f32, None)
        {
            paint.set_mask_filter(filter);
        }

        let canvas = self.canvas();
        canvas.save();
        canvas.concat(&to_skia_matrix(transform));
        // The baseline lands on a whole device pixel. A line box may sit at a
        // fraction of one — that is what keeps a page's rhythm right — but a
        // baseline between two rows of pixels blurs every horizontal stem in the
        // run, and a face is drawn to be read on a whole row. Across the line
        // there is no row to land on, so nothing there is moved. Rounded here,
        // where the scale is known: rounding in CSS pixels would snap to every
        // second device pixel on a screen that has two of them per pixel.
        let matrix = canvas.local_to_device_as_3x3();
        let (scale, offset) = (matrix.scale_y(), matrix.translate_y());
        let positions: Vec<sk::Point> = if scale.abs() > f32::EPSILON {
            positions
                .into_iter()
                .map(|point| {
                    sk::Point::new(
                        point.x,
                        ((point.y * scale + offset).round() - offset) / scale,
                    )
                })
                .collect()
        } else {
            positions
        };

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
        let Some(sk_image) = to_skia_image(image.image) else {
            return;
        };

        let mut paint = sk::Paint::default();
        paint.set_anti_alias(true);
        paint.set_alpha_f(image.sampler.alpha);

        let sampling = to_skia_sampling(image.sampler.quality);

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

/// Wrap decoded pixels in a Skia image, or refuse them loudly.
///
/// The buffer is checked against the dimensions it claims before Skia is handed a
/// pointer to it: a short buffer here is a read past the end of an allocation.
fn to_skia_image(data: &peniko::ImageData) -> Option<sk::Image> {
    let Some(expected) = data.format.size_in_bytes(data.width, data.height) else {
        tracing::error!(
            width = data.width,
            height = data.height,
            "image dimensions overflow"
        );
        return None;
    };
    if data.data.as_ref().len() < expected {
        tracing::error!(
            got = data.data.as_ref().len(),
            expected,
            "image buffer is smaller than its dimensions claim"
        );
        return None;
    }

    let color_type = match data.format {
        peniko::ImageFormat::Rgba8 => sk::ColorType::RGBA8888,
        peniko::ImageFormat::Bgra8 => sk::ColorType::BGRA8888,
        // `ImageFormat` is non-exhaustive; an unknown format is better dropped
        // loudly than guessed at.
        other => {
            tracing::error!(?other, "unsupported image format");
            return None;
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
    let image = sk::images::raster_from_data(&info, pixels, row_bytes);
    if image.is_none() {
        tracing::error!("skia rejected the image data");
    }
    image
}

/// How a picture is sampled when it is drawn at other than its own size.
fn to_skia_sampling(quality: peniko::ImageQuality) -> sk::SamplingOptions {
    match quality {
        peniko::ImageQuality::Low => {
            sk::SamplingOptions::new(sk::FilterMode::Nearest, sk::MipmapMode::None)
        }
        _ => sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::Linear),
    }
}

/// Whether the bytes are an SVG document.
///
/// By the markup rather than by what the server called it: a picture written into
/// a page carries whatever type the page felt like writing, and the first tag is
/// the thing that cannot be wrong. An XML declaration, a doctype or a comment may
/// come first, so the whole of a short prefix is searched rather than the start.
fn looks_like_svg(bytes: &[u8]) -> bool {
    const PREFIX: usize = 1024;
    let head = &bytes[..bytes.len().min(PREFIX)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    head.contains("<svg")
}

/// Draw an SVG at the size it declares.
///
/// The size is the document's own — its `width` and `height`, or the extent of its
/// `viewBox` — and a document that declares neither is drawn at a modest square,
/// because something has to be picked and a picture nobody sized is an icon often
/// enough.
fn draw_svg(bytes: &[u8]) -> Result<peniko::ImageData, SkiaError> {
    /// What an SVG with no size of its own is drawn at.
    const UNSIZED: f32 = 150.0;
    /// As large as one will be drawn, so that a document declaring a page-sized
    /// picture cannot ask for a surface measured in gigabytes.
    const LARGEST: f32 = 4096.0;

    let fonts = sk::FontMgr::new();
    let dom = sk::svg::Dom::from_bytes(bytes, fonts).map_err(|_| SkiaError::ImageDecode)?;

    let declared = dom.root().intrinsic_size();
    let size = |value: f32| {
        if value.is_finite() && value >= 1.0 {
            value.min(LARGEST)
        } else {
            UNSIZED
        }
    };
    let (width, height) = (size(declared.width), size(declared.height));

    let mut surface = sk::surfaces::raster_n32_premul((width as i32, height as i32))
        .ok_or(SkiaError::ImageDecode)?;
    let mut dom = dom;
    dom.set_container_size((width, height));
    dom.render(surface.canvas());

    let image = surface.image_snapshot();
    let info = sk::ImageInfo::new(
        (image.width(), image.height()),
        sk::ColorType::RGBA8888,
        sk::AlphaType::Premul,
        None,
    );
    let row_bytes = image.width() as usize * 4;
    let mut pixels = vec![0u8; row_bytes * image.height() as usize];
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
        width: image.width() as u32,
        height: image.height() as u32,
    })
}

/// Decode an encoded image (PNG, JPEG, …) into pixels the display list can carry.
///
/// Skia is already a dependency and already contains decoders, so this costs no new
/// crate. When resource loading exists this moves out of the renderer.
pub fn decode_image(bytes: &[u8]) -> Result<peniko::ImageData, SkiaError> {
    // A vector picture is not decoded, it is *drawn*: there are no pixels in the
    // file, only instructions for making some. Drawn at the size it says it is,
    // which is what everything downstream reads as its intrinsic size — a rule
    // that names a size of its own then scales these pixels, which is what a
    // vector picture is for and is the one place it shows.
    if looks_like_svg(bytes) {
        return draw_svg(bytes);
    }

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
    fn a_sub_rect_reads_back_only_that_rectangle() {
        let mut painter = SkiaPainter::new_raster(16, 16).expect("16x16 raster surface");
        painter.clear(Color::WHITE);
        let brush = peniko::Brush::Solid(Color::from_rgb8(0x00, 0xff, 0x00));
        painter.fill_rect(
            Affine::IDENTITY,
            (&brush).into(),
            Rect::new(4.0, 4.0, 8.0, 8.0),
        );

        // A tight 4x4 read at the painted corner: stride is the sub-width, so the
        // green pixel sits at local (0,0) of the returned buffer.
        let pixels = painter.read_rgba8_rect(4, 4, 4, 4).expect("sub read");
        assert_eq!(pixels.len(), 4 * 4 * 4, "tightly packed 4x4 RGBA8");
        assert_eq!(pixel_at(&pixels, 4, 0, 0), [0x00, 0xff, 0x00, 0xff]);
        assert_eq!(pixel_at(&pixels, 4, 3, 3), [0x00, 0xff, 0x00, 0xff]);

        // A rect running off the edge is clamped, not an error.
        let clamped = painter.read_rgba8_rect(14, 14, 8, 8).expect("clamped read");
        assert_eq!(clamped.len(), 2 * 2 * 4, "clamped to the 2x2 corner");
    }

    #[test]
    fn clearing_a_rect_leaves_the_rest_of_the_surface_intact() {
        let mut painter = SkiaPainter::new_raster(16, 16).expect("16x16 raster surface");
        painter.clear(Color::from_rgb8(0xff, 0x00, 0x00));
        painter.clear_rect(Rect::new(4.0, 4.0, 8.0, 8.0));

        let pixels = painter.read_rgba8().expect("read back");
        // Inside the cleared rect: transparent (premultiplied, so all zero).
        assert_eq!(pixel_at(&pixels, 16, 6, 6), [0x00, 0x00, 0x00, 0x00]);
        // Outside it: the red is untouched, which is the whole point.
        assert_eq!(pixel_at(&pixels, 16, 1, 1), [0xff, 0x00, 0x00, 0xff]);
        assert_eq!(pixel_at(&pixels, 16, 12, 12), [0xff, 0x00, 0x00, 0xff]);
    }

    /// A shape filled with a picture repeats it across the shape, and the brush
    /// transform is what says where one tile starts and how large it is. This is
    /// how a background tiles: one fill, not one command per tile.
    #[test]
    fn a_shape_filled_with_a_picture_repeats_it() {
        // Two pixels: red at the top left, blue at the bottom right, so a tile's
        // own orientation is visible in the result.
        let image = peniko::ImageData {
            data: peniko::Blob::new(std::sync::Arc::new(vec![
                0xff, 0x00, 0x00, 0xff, // red
                0xff, 0x00, 0x00, 0xff, // red
                0x00, 0x00, 0xff, 0xff, // blue
                0x00, 0x00, 0xff, 0xff, // blue
            ])),
            format: peniko::ImageFormat::Rgba8,
            alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
            width: 2,
            height: 2,
        };
        let brush = peniko::Brush::Image(peniko::ImageBrush {
            image,
            sampler: peniko::ImageSampler {
                x_extend: peniko::Extend::Repeat,
                y_extend: peniko::Extend::Repeat,
                quality: peniko::ImageQuality::Low,
                alpha: 1.0,
            },
        });

        let mut painter = SkiaPainter::new_raster(16, 16).expect("16x16 raster surface");
        painter.clear(Color::WHITE);
        // Four-pixel tiles, so each source pixel is two device pixels across.
        painter.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            (&brush).into(),
            Some(Affine::scale(2.0)),
            &Rect::new(0.0, 0.0, 16.0, 16.0),
        );

        let pixels = painter.read_rgba8().expect("read back");
        // The first tile, and the third one along: the same picture, repeated.
        for x in [1, 9] {
            assert_eq!(
                pixel_at(&pixels, 16, x, 1),
                [0xff, 0x00, 0x00, 0xff],
                "the top of a tile at x={x}"
            );
            assert_eq!(
                pixel_at(&pixels, 16, x, 3),
                [0x00, 0x00, 0xff, 0xff],
                "and the bottom of it"
            );
        }
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

    /// A layer positions its own clip and nothing else.
    ///
    /// Every item in a display list carries an absolute transform, so if the
    /// layer's matrix stayed on the canvas it would be applied a second time to
    /// everything inside it. Invisible while layers are pushed with the identity
    /// — and it doubles the size and the offset of a scrolling panel the moment
    /// the whole list is scaled for a HiDPI screen, which is exactly what a
    /// display list scaled by the device factor does.
    #[test]
    fn what_is_inside_a_transformed_layer_is_not_transformed_twice() {
        let mut painter = SkiaPainter::new_raster(32, 32).expect("32x32 raster surface");
        painter.clear(Color::WHITE);
        let brush = peniko::Brush::Solid(Color::BLACK);
        let scale = Affine::scale(2.0);

        // The clip covers the whole surface once scaled; the fill lands at 8..16
        // in device pixels because its own transform already carries the scale.
        painter.push_clip_rect(scale, Rect::new(0.0, 0.0, 16.0, 16.0));
        painter.fill_rect(scale, (&brush).into(), Rect::new(4.0, 4.0, 8.0, 8.0));
        painter.pop_layer();

        let pixels = painter.read_rgba8().expect("read back");
        assert_eq!(
            pixel_at(&pixels, 32, 12, 12),
            [0x00, 0x00, 0x00, 0xff],
            "the fill is where its own transform puts it"
        );
        assert_eq!(
            pixel_at(&pixels, 32, 20, 20),
            [0xff, 0xff, 0xff, 0xff],
            "and not at twice that, which is where the layer's matrix would put it"
        );
        assert_eq!(painter.layer_depth, 0);
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

#[cfg(test)]
mod collection_tests {
    use super::face_from_collection;

    /// Build a collection of two faces, each with one table of its own, sharing a
    /// third: the shape a real `.ttc` has, in miniature.
    fn collection() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"ttcf");
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        // Two directory offsets, filled in once their positions are known.
        let offsets_at = out.len();
        out.extend_from_slice(&[0; 8]);

        // The shared table data, first, so the directories point backwards as they
        // may in a real file.
        let shared_at = out.len() as u32;
        out.extend_from_slice(b"shared--");
        let first_at = out.len() as u32;
        out.extend_from_slice(b"first---");
        let second_at = out.len() as u32;
        out.extend_from_slice(b"second--");

        let directory = |own_tag: &[u8; 4], own_at: u32, out: &mut Vec<u8>| -> u32 {
            let at = out.len() as u32;
            out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            out.extend_from_slice(&2u16.to_be_bytes());
            out.extend_from_slice(&[0; 6]);
            for (tag, offset) in [(b"shrd", shared_at), (own_tag, own_at)] {
                out.extend_from_slice(tag);
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(&8u32.to_be_bytes());
            }
            at
        };

        let first = directory(b"one_", first_at, &mut out);
        let second = directory(b"two_", second_at, &mut out);
        out[offsets_at..offsets_at + 4].copy_from_slice(&first.to_be_bytes());
        out[offsets_at + 4..offsets_at + 8].copy_from_slice(&second.to_be_bytes());
        out
    }

    /// Each face comes out as a font of its own, carrying the tables it named and
    /// the shared one, with offsets that point at them.
    #[test]
    fn a_face_is_lifted_out_of_a_collection_whole() {
        let bytes = collection();

        for (index, own) in [(0usize, &b"first---"[..]), (1, &b"second--"[..])] {
            let face = face_from_collection(&bytes, index).expect("a face");

            assert_eq!(
                &face[..4],
                &0x0001_0000u32.to_be_bytes(),
                "an sfnt, not a collection"
            );
            assert_eq!(u16::from_be_bytes([face[4], face[5]]), 2, "both tables");

            for table in 0..2 {
                let record = 12 + table * 16;
                let offset =
                    u32::from_be_bytes(face[record + 8..record + 12].try_into().unwrap()) as usize;
                let length =
                    u32::from_be_bytes(face[record + 12..record + 16].try_into().unwrap()) as usize;
                assert_eq!(offset % 4, 0, "tables start on a four-byte boundary");
                let data = &face[offset..offset + length];
                if table == 0 {
                    assert_eq!(data, b"shared--");
                } else {
                    assert_eq!(data, own);
                }
            }
        }
    }

    /// Anything that is not a collection, or a face that is not in it, is refused
    /// rather than guessed at.
    #[test]
    fn only_a_real_collection_is_taken_apart() {
        assert!(face_from_collection(b"not a font at all", 0).is_none());
        assert!(face_from_collection(&collection(), 2).is_none());
    }
}

#[cfg(test)]
mod variation_tests {
    /// A vector picture is drawn rather than decoded, at the size it declares.
    #[test]
    fn an_svg_is_drawn_at_the_size_it_declares() {
        use crate::decode_image;

        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
             <rect width="40" height="20" fill="#ff0000"/></svg>"##;
        let drawn = decode_image(svg).expect("a vector picture draws");
        assert_eq!((drawn.width, drawn.height), (40, 20));

        // And it is actually painted: the middle of it is the colour it asked for.
        let at = ((10 * 40 + 20) * 4) as usize;
        let pixel = &drawn.data.as_ref()[at..at + 3];
        assert!(
            pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60,
            "the rectangle was drawn: {pixel:?}"
        );

        // Something that is not a picture at all is still refused.
        assert!(decode_image(b"not a picture").is_err());
    }

    use super::{SegmentMap, axis_segment_maps, unmap_axis};

    /// A font of one table: an `avar` with the given per-axis maps.
    fn font_with_avar(maps: &[&[(f32, f32)]]) -> Vec<u8> {
        let f2dot14 = |value: f32| ((value * 16384.0).round() as i16).to_be_bytes();

        let mut avar = Vec::new();
        avar.extend_from_slice(&1u16.to_be_bytes());
        avar.extend_from_slice(&0u16.to_be_bytes());
        avar.extend_from_slice(&0u16.to_be_bytes());
        avar.extend_from_slice(&(maps.len() as u16).to_be_bytes());
        for map in maps {
            avar.extend_from_slice(&(map.len() as u16).to_be_bytes());
            for &(from, to) in *map {
                avar.extend_from_slice(&f2dot14(from));
                avar.extend_from_slice(&f2dot14(to));
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        let offset = (out.len() + 16) as u32;
        out.extend_from_slice(b"avar");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&offset.to_be_bytes());
        out.extend_from_slice(&(avar.len() as u32).to_be_bytes());
        out.extend_from_slice(&avar);
        out
    }

    #[test]
    fn segment_maps_are_read_one_per_axis() {
        let identity: &[(f32, f32)] = &[(-1.0, -1.0), (0.0, 0.0), (1.0, 1.0)];
        let bent: &[(f32, f32)] = &[(-1.0, -1.0), (-0.5, -0.25), (0.0, 0.0), (1.0, 1.0)];
        let maps = axis_segment_maps(&font_with_avar(&[identity, bent]), 0);

        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].len(), 3);
        assert_eq!(maps[1], bent.to_vec());
    }

    /// A font with no such table maps nothing, and the caller must then treat every
    /// axis as unmapped rather than as mapped by an empty table.
    #[test]
    fn a_font_without_the_table_has_no_maps() {
        assert!(axis_segment_maps(b"not a font at all", 0).is_empty());
        assert!(axis_segment_maps(&font_with_avar(&[]), 0).is_empty());
    }

    /// The inverse of the mapping is the mapping run backwards: whatever the
    /// forward direction sends to a coordinate is what comes back from it.
    #[test]
    fn the_inverse_undoes_the_mapping() {
        let map: SegmentMap = vec![(-1.0, -1.0), (-0.5, -0.25), (0.0, 0.0), (1.0, 1.0)];

        // Segment ends land exactly.
        assert!((unmap_axis(&map, -1.0) - -1.0).abs() < 1e-6);
        assert!((unmap_axis(&map, -0.25) - -0.5).abs() < 1e-6);
        assert!((unmap_axis(&map, 0.0)).abs() < 1e-6);
        assert!((unmap_axis(&map, 1.0) - 1.0).abs() < 1e-6);

        // And so does the middle of one: halfway from -1 to -0.25 in the mapped
        // coordinate is halfway from -1 to -0.5 in the unmapped one.
        assert!((unmap_axis(&map, -0.625) - -0.75).abs() < 1e-6);
    }

    /// An axis the font does not bend, and one it does not describe at all, both
    /// come back untouched — the identity is the only safe answer.
    #[test]
    fn an_unmapped_axis_is_left_alone() {
        let identity: SegmentMap = vec![(-1.0, -1.0), (0.0, 0.0), (1.0, 1.0)];
        for value in [-1.0, -0.3, 0.0, 0.75, 1.0] {
            assert!((unmap_axis(&identity, value) - value).abs() < 1e-6);
            assert!((unmap_axis(&Vec::new(), value) - value).abs() < 1e-6);
        }
    }

    /// Past either end the mapping stops rather than being extrapolated: a
    /// coordinate is already clamped to -1..1 before it gets here, and running a
    /// segment on past its own end is how a wrong instance is asked for.
    #[test]
    fn the_inverse_does_not_run_past_the_table() {
        let map: SegmentMap = vec![(-0.5, -0.5), (0.5, 0.25)];
        assert!((unmap_axis(&map, -1.0) - -0.5).abs() < 1e-6);
        assert!((unmap_axis(&map, 1.0) - 0.5).abs() < 1e-6);
    }
}
