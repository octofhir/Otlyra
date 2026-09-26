//! The display list: what to draw, decided independently of how it is drawn.
//!
//! A [`DisplayList`] is a flat, owned, immutable sequence of [`DisplayItem`]s. It
//! is not a tree of boxed virtual objects, and it holds no borrows, no `Rc` and no
//! handle to anything live. That is what makes it `Send`, snapshot-testable, and
//! encodable — three properties that are very hard to add later and nearly free to
//! keep from the start.
//!
//! Resources that are not plain data — fonts today, images later — live in
//! side tables and are referenced by id, so an item stays small and cheap to
//! compare, print and serialize.

use kurbo::{Affine, BezPath, Rect, Stroke};
use peniko::{BlendMode, Brush, Fill, FontData, ImageData, ImageSampler};
use serde::{Deserialize, Serialize};

use crate::paint_target::Glyph;

/// Reference to a font in a [`DisplayList`]'s font table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FontId(pub u32);

/// Identifies what a hit-test region belongs to.
///
/// Opaque here on purpose: `otlyra-gfx` must not learn what a DOM node is. The
/// crate that builds the list decides what the number means.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HitTestId(pub u64);

/// What an item moves with when something scrolls.
///
/// An item carries its geometry in the coordinates of the thing it belongs to —
/// the page, a box that sticks, the viewport — and the offset that puts that
/// thing on the screen is supplied when the list is drawn, not when it is built.
/// That is what lets a scroll be drawn from the list it already has: the page
/// moved, the list did not.
///
/// The number is an index into the offsets handed to
/// [`crate::render_scrolled`]. What each one means is the business of whoever
/// built the list; [`SpatialId::VIEWPORT`] is the one every list has, and never
/// moves.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpatialId(pub u32);

impl SpatialId {
    /// The viewport itself: what an item drawn in window coordinates moves with,
    /// which is nothing.
    pub const VIEWPORT: Self = Self(0);
}

/// One drawing command.
///
/// Seven variants, matching the seven methods of [`crate::PaintTarget`] plus hit
/// testing. Extending this enum is a milestone-sized decision, not a convenience:
/// every variant is one more thing each backend has to be correct about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DisplayItem {
    /// Begin a compositing group.
    PushLayer {
        /// How the group composites into its parent.
        blend: BlendMode,
        /// Group opacity.
        alpha: f32,
        /// Transform applied to `clip`.
        transform: Affine,
        /// Clip geometry.
        clip: BezPath,
    },
    /// End the most recent compositing group.
    PopLayer,
    /// Fill a shape with a blurred edge: a shadow.
    Blurred {
        /// Transform applied to `shape`.
        transform: Affine,
        /// Paint.
        brush: Brush,
        /// The CSS blur radius, twice the standard deviation of the blur itself.
        blur: f64,
        /// Geometry.
        shape: BezPath,
    },
    /// Fill a shape.
    Fill {
        /// Fill rule.
        style: Fill,
        /// Transform applied to `shape`.
        transform: Affine,
        /// Paint.
        brush: Brush,
        /// Optional brush-space transform.
        brush_transform: Option<Affine>,
        /// Geometry.
        shape: BezPath,
    },
    /// Stroke a shape's outline.
    Stroke {
        /// Stroke parameters.
        style: Stroke,
        /// Transform applied to `shape`.
        transform: Affine,
        /// Paint.
        brush: Brush,
        /// Optional brush-space transform.
        brush_transform: Option<Affine>,
        /// Geometry.
        shape: BezPath,
    },
    /// Draw one shaped run of glyphs.
    Glyphs {
        /// Font, by reference into the list's font table.
        font: FontId,
        /// Size in logical pixels.
        font_size: f32,
        /// Variation axis coordinates.
        normalized_coords: Vec<i16>,
        /// Paint.
        brush: Brush,
        /// Transform applied to the run.
        transform: Affine,
        /// Whether to grid fit.
        hint: bool,
        /// The CSS blur radius, for a run drawn as a shadow. Zero for ordinary text.
        blur: f64,
        /// The glyphs, in visual order.
        glyphs: Vec<Glyph>,
    },
    /// Draw a decoded image.
    Image {
        /// Pixel data and dimensions.
        image: ImageResource,
        /// Sampling parameters.
        sampler: ImageSampler,
        /// Transform applied to the image.
        transform: Affine,
        /// Optional bounding rectangle.
        clip_rect: Option<Rect>,
    },
    /// A region that hit testing should attribute to `id`.
    ///
    /// Hit testing is a display list too, emitted into the same sequence as
    /// painting. Keeping them in one list is what stops the two from drifting
    /// apart, which is the failure mode where a link is clickable somewhere other
    /// than where it is drawn.
    HitTest {
        /// Region, in the transformed space.
        rect: Rect,
        /// Transform applied to `rect`.
        transform: Affine,
        /// What the region belongs to.
        id: HitTestId,
    },
}

/// Decoded pixels, compared and printed by content rather than by identity.
///
/// `peniko::Blob` implements `PartialEq` by comparing its allocation id, so two
/// blobs holding identical pixels are unequal. That is reasonable for a cache
/// handle and wrong for a display list, where equality is how a round trip is
/// checked and how a frame is compared to the last one. Its `Debug` would also put
/// every pixel in a snapshot.
#[derive(Clone, Serialize, Deserialize)]
pub struct ImageResource(pub ImageData);

impl ImageResource {
    /// The wrapped pixel data.
    pub fn data(&self) -> &ImageData {
        &self.0
    }
}

impl From<ImageData> for ImageResource {
    fn from(image: ImageData) -> Self {
        Self(image)
    }
}

impl std::ops::Deref for ImageResource {
    type Target = ImageData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PartialEq for ImageResource {
    fn eq(&self, other: &Self) -> bool {
        self.0.width == other.0.width
            && self.0.height == other.0.height
            && self.0.format == other.0.format
            && self.0.alpha_type == other.0.alpha_type
            && self.0.data.as_ref() == other.0.data.as_ref()
    }
}

impl std::fmt::Debug for ImageResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageResource")
            .field("width", &self.0.width)
            .field("height", &self.0.height)
            .field("format", &self.0.format)
            .field("alpha_type", &self.0.alpha_type)
            .field("bytes", &self.0.data.as_ref().len())
            .finish()
    }
}

/// Fonts referenced by a display list.
///
/// Deduplicated by blob identity, so a page that uses one font in a thousand runs
/// carries one entry.
#[derive(Clone, Default)]
pub struct FontTable {
    fonts: Vec<FontData>,
}

/// `FontData` carries no serde derive upstream, but its `Blob` does, so the pair is
/// spelled out here rather than pulled in through a newtype nobody else would use.
#[derive(Serialize, Deserialize)]
struct SerializedFont {
    data: peniko::Blob<u8>,
    index: u32,
}

impl Serialize for FontTable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fonts: Vec<SerializedFont> = self
            .fonts
            .iter()
            .map(|font| SerializedFont {
                data: font.data.clone(),
                index: font.index,
            })
            .collect();
        fonts.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FontTable {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fonts = Vec::<SerializedFont>::deserialize(deserializer)?;
        Ok(Self {
            fonts: fonts
                .into_iter()
                .map(|font| FontData::new(font.data, font.index))
                .collect(),
        })
    }
}

impl std::fmt::Debug for FontTable {
    /// Prints sizes, never contents. A font blob in a snapshot would be hundreds of
    /// kilobytes of noise that changes whenever the font does.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_list();
        for (index, font) in self.fonts.iter().enumerate() {
            list.entry(&format_args!(
                "FontId({index}): {} bytes, face {}",
                font.data.as_ref().len(),
                font.index
            ));
        }
        list.finish()
    }
}

impl PartialEq for FontTable {
    fn eq(&self, other: &Self) -> bool {
        self.fonts.len() == other.fonts.len()
            && self
                .fonts
                .iter()
                .zip(&other.fonts)
                .all(|(a, b)| a.index == b.index && a.data.as_ref() == b.data.as_ref())
    }
}

impl FontTable {
    /// Intern `font`, returning its id. Repeated calls with the same font blob
    /// return the same id.
    pub fn intern(&mut self, font: &FontData) -> FontId {
        let existing = self.fonts.iter().position(|candidate| {
            candidate.index == font.index
                && std::ptr::eq(candidate.data.as_ref(), font.data.as_ref())
        });
        match existing {
            Some(index) => FontId(index as u32),
            None => {
                self.fonts.push(font.clone());
                FontId((self.fonts.len() - 1) as u32)
            }
        }
    }

    /// Look up an interned font.
    pub fn get(&self, id: FontId) -> Option<&FontData> {
        self.fonts.get(id.0 as usize)
    }

    /// Number of distinct fonts.
    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    /// Whether no font has been interned.
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }
}

/// A frame's worth of drawing commands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayList {
    items: Vec<DisplayItem>,
    fonts: FontTable,
    /// Which [`SpatialId`] the items move with, as runs: an item belongs to the
    /// last run that starts at or before it, and to the viewport before the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    spatial: Vec<(u32, SpatialId)>,
    /// What [`Self::transform`] has applied to the whole list since it was
    /// built — the device scale, an inset — so that an offset given in the
    /// coordinates the list was built in lands in the ones it is drawn in.
    #[serde(default = "identity", skip_serializing_if = "is_identity")]
    frame: Affine,
}

fn identity() -> Affine {
    Affine::IDENTITY
}

fn is_identity(affine: &Affine) -> bool {
    *affine == Affine::IDENTITY
}

impl Default for DisplayList {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            fonts: FontTable::default(),
            spatial: Vec::new(),
            frame: Affine::IDENTITY,
        }
    }
}

impl DisplayList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// The items, in paint order.
    pub fn items(&self) -> &[DisplayItem] {
        &self.items
    }

    /// The font table.
    pub fn fonts(&self) -> &FontTable {
        &self.fonts
    }

    /// Number of items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the list draws nothing.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Append an item that references no external resource.
    pub fn push(&mut self, item: DisplayItem) {
        self.items.push(item);
    }

    /// Say what the items pushed from now on move with.
    pub fn set_spatial(&mut self, spatial: SpatialId) {
        let current = self.spatial.last().map_or(SpatialId::VIEWPORT, |run| run.1);
        if current == spatial {
            return;
        }
        let start = self.items.len() as u32;
        // A run nothing was pushed into is replaced rather than kept empty.
        if self.spatial.last().is_some_and(|run| run.0 == start) {
            self.spatial.pop();
        }
        let before = self.spatial.last().map_or(SpatialId::VIEWPORT, |run| run.1);
        if before != spatial {
            self.spatial.push((start, spatial));
        }
    }

    /// Each item with what it moves with, in paint order.
    pub fn items_with_spatial(&self) -> impl Iterator<Item = (SpatialId, &DisplayItem)> {
        let mut runs = self.spatial.iter().peekable();
        let mut current = SpatialId::VIEWPORT;
        self.items.iter().enumerate().map(move |(index, item)| {
            while let Some(run) = runs.next_if(|run| run.0 as usize <= index) {
                current = run.1;
            }
            (current, item)
        })
    }

    /// Where the items of each id paint, and how many of them there are, in the
    /// list's coordinates with nothing moved: what a compositor needs to know to
    /// move the pixels one id painted and redraw the ones the others did.
    ///
    /// Conservative. A run of glyphs is bounded by its pen positions and its
    /// size rather than by its outlines, a blur by three standard deviations, a
    /// stroke by its width on either side: each rectangle holds every pixel its
    /// items can touch, and a little more.
    pub fn spatial_bounds(&self) -> Vec<(SpatialId, usize, Rect)> {
        let mut found: Vec<(SpatialId, usize, Rect)> = Vec::new();
        for (spatial, item) in self.items_with_spatial() {
            let Some(bounds) = painted_bounds(item) else {
                continue;
            };
            match found.iter_mut().find(|(id, _, _)| *id == spatial) {
                Some((_, count, union)) => {
                    *count += 1;
                    *union = union.union(bounds);
                }
                None => found.push((spatial, 1, bounds)),
            }
        }
        found
    }

    /// Whether anything in the list moves with something other than the viewport.
    pub fn is_spatial(&self) -> bool {
        !self.spatial.is_empty()
    }

    /// The transform that puts an item of `spatial` where `offsets` say, ahead of
    /// the item's own: the offset, taken in the coordinates the list was built in
    /// and carried through whatever [`Self::transform`] has done since.
    ///
    /// An offset is how far the thing has moved *away*: the page scrolled down by
    /// a hundred has an offset of a hundred, and draws a hundred higher.
    pub fn shift(&self, spatial: SpatialId, offsets: &[kurbo::Vec2]) -> Affine {
        let Some(offset) = offsets.get(spatial.0 as usize).copied() else {
            return Affine::IDENTITY;
        };
        if offset == kurbo::Vec2::ZERO {
            return Affine::IDENTITY;
        }
        let [a, b, c, d, _, _] = self.frame.as_coeffs();
        Affine::translate(kurbo::Vec2::new(
            -(a * offset.x + c * offset.y),
            -(b * offset.x + d * offset.y),
        ))
    }

    /// A copy with `offsets` applied to every item, and nothing left to move.
    ///
    /// For whatever wants the picture at one scroll position as plain geometry — a
    /// test looking for where something landed, a snapshot.
    pub fn with_offsets(&self, offsets: &[kurbo::Vec2]) -> Self {
        let mut shifts: Vec<(SpatialId, Affine)> = Vec::new();
        let mut items = self.items.clone();
        for (index, (spatial, _)) in self.items_with_spatial().enumerate() {
            let shift = match shifts.iter().find(|(id, _)| *id == spatial) {
                Some((_, shift)) => *shift,
                None => {
                    let shift = self.shift(spatial, offsets);
                    shifts.push((spatial, shift));
                    shift
                }
            };
            if shift != Affine::IDENTITY {
                prepend(&mut items[index], shift);
            }
        }
        Self {
            items,
            fonts: self.fonts.clone(),
            spatial: Vec::new(),
            frame: self.frame,
        }
    }

    /// Apply `transform` to every item, ahead of the transform it already carries.
    ///
    /// This is how the device scale reaches the rasterizer: geometry is authored in
    /// logical pixels all the way down, and one transform at the end turns it into
    /// device pixels. Baking the scale into coordinates instead would make every
    /// number in the list depend on which display it was built for.
    pub fn transform(&mut self, transform: Affine) {
        self.frame = transform * self.frame;
        self.transform_from(0, transform);
    }

    /// The same, for the items pushed since `from`.
    ///
    /// What a `transform` on one box needs: everything drawn for it and for what is
    /// inside it, and nothing drawn before it. The items of a subtree are
    /// contiguous, so the index it started at is the whole of what has to be
    /// remembered.
    pub fn transform_from(&mut self, from: usize, transform: Affine) {
        let from = from.min(self.items.len());
        for item in &mut self.items[from..] {
            prepend(item, transform);
        }
    }

    /// Append a glyph run, interning its font.
    #[allow(clippy::too_many_arguments)]
    pub fn push_glyphs(
        &mut self,
        font: &FontData,
        font_size: f32,
        normalized_coords: Vec<i16>,
        brush: Brush,
        transform: Affine,
        hint: bool,
        glyphs: Vec<Glyph>,
    ) {
        self.push_glyph_run(
            font,
            font_size,
            normalized_coords,
            brush,
            transform,
            hint,
            0.0,
            glyphs,
        );
    }

    /// Append a glyph run with its edges blurred: a text shadow.
    ///
    /// `blur` is the CSS blur radius. Zero is the same as [`Self::push_glyphs`],
    /// which is what it delegates to.
    #[allow(clippy::too_many_arguments)]
    pub fn push_glyph_run(
        &mut self,
        font: &FontData,
        font_size: f32,
        normalized_coords: Vec<i16>,
        brush: Brush,
        transform: Affine,
        hint: bool,
        blur: f64,
        glyphs: Vec<Glyph>,
    ) {
        let font = self.fonts.intern(font);
        self.items.push(DisplayItem::Glyphs {
            font,
            font_size,
            normalized_coords,
            brush,
            transform,
            hint,
            blur,
            glyphs,
        });
    }

    /// Append another list's items to this one, in paint order.
    ///
    /// Fonts are re-interned rather than copied across, because a `FontId` names
    /// a row in *its own* list's table: moving a glyph run without remapping it
    /// would draw whatever font happened to sit at that index here. This is what
    /// lets a surface keep the list it built last frame and hand it to a frame
    /// it did not build — the whole point of not rebuilding an interface that
    /// has not changed.
    ///
    /// What the other list's items move with comes across too. Its offsets are
    /// taken in its own coordinates, so a list that has been transformed
    /// differently from this one cannot bring anything that moves: its items are
    /// kept where they stand.
    pub fn append(&mut self, other: &Self) {
        self.items.reserve(other.items.len());
        let resume = self.spatial.last().map_or(SpatialId::VIEWPORT, |run| run.1);
        let carried = other.frame == self.frame;
        if other.is_spatial() && !carried {
            tracing::warn!("appending a scrolled list transformed differently; it will not scroll");
        }
        for (spatial, item) in other.items_with_spatial() {
            self.set_spatial(if carried {
                spatial
            } else {
                SpatialId::VIEWPORT
            });
            match item {
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
                    let Some(data) = other.fonts.get(*font) else {
                        // A run naming a font its own list does not have is a
                        // list built wrong; dropping the run loses text, and
                        // keeping it would draw the wrong face.
                        tracing::warn!(?font, "glyph run names a font its list has not interned");
                        continue;
                    };
                    let font = self.fonts.intern(data);
                    self.items.push(DisplayItem::Glyphs {
                        blur: *blur,
                        font,
                        font_size: *font_size,
                        normalized_coords: normalized_coords.clone(),
                        brush: brush.clone(),
                        transform: *transform,
                        hint: *hint,
                        glyphs: glyphs.clone(),
                    });
                }
                other => self.items.push(other.clone()),
            }
        }
        self.set_spatial(resume);
    }

    /// Hit-test regions, innermost last, in the order they were emitted.
    pub fn hit_test(&self, point: kurbo::Point) -> Option<HitTestId> {
        self.items.iter().rev().find_map(|item| match item {
            DisplayItem::HitTest {
                rect,
                transform,
                id,
            } => {
                let local = transform.inverse() * point;
                rect.contains(local).then_some(*id)
            }
            _ => None,
        })
    }
}

/// Every pixel `item` can paint, in the coordinates its transform puts it in, or
/// `None` for an item that paints nothing of its own.
fn painted_bounds(item: &DisplayItem) -> Option<Rect> {
    use kurbo::Shape;
    let bounds = match item {
        DisplayItem::PushLayer { .. } | DisplayItem::PopLayer | DisplayItem::HitTest { .. } => {
            return None;
        }
        DisplayItem::Blurred {
            transform,
            blur,
            shape,
            ..
        } => transform.transform_rect_bbox(shape.bounding_box().inflate(blur * 1.5, blur * 1.5)),
        DisplayItem::Fill {
            transform, shape, ..
        } => transform.transform_rect_bbox(shape.bounding_box()),
        DisplayItem::Stroke {
            style,
            transform,
            shape,
            ..
        } => transform.transform_rect_bbox(shape.bounding_box().inflate(style.width, style.width)),
        DisplayItem::Glyphs {
            font_size,
            transform,
            blur,
            glyphs,
            ..
        } => {
            let size = f64::from(*font_size);
            let (mut x0, mut y0, mut x1, mut y1) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
            for (index, glyph) in glyphs.iter().enumerate() {
                let (x, y) = (f64::from(glyph.x), f64::from(glyph.y));
                if index == 0 {
                    (x0, y0, x1, y1) = (x, y, x, y);
                } else {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                }
            }
            // A glyph reaches up by its ascent and across by its advance, both of
            // which a face may make larger than its size; twice the size up and
            // across and one down covers every face that is not playing games.
            let reach = size + blur * 1.5;
            transform.transform_rect_bbox(Rect::new(
                x0 - reach,
                y0 - size * 2.0 - blur * 1.5,
                x1 + size * 2.0 + blur * 1.5,
                y1 + reach,
            ))
        }
        DisplayItem::Image {
            image,
            transform,
            clip_rect,
            ..
        } => {
            let whole = Rect::new(
                0.0,
                0.0,
                f64::from(image.data().width),
                f64::from(image.data().height),
            );
            transform.transform_rect_bbox(clip_rect.map_or(whole, |clip| clip.intersect(whole)))
        }
    };
    Some(bounds)
}

/// Put `transform` ahead of the one `item` already carries.
fn prepend(item: &mut DisplayItem, transform: Affine) {
    match item {
        DisplayItem::PushLayer { transform: t, .. }
        | DisplayItem::Blurred { transform: t, .. }
        | DisplayItem::Fill { transform: t, .. }
        | DisplayItem::Stroke { transform: t, .. }
        | DisplayItem::Glyphs { transform: t, .. }
        | DisplayItem::Image { transform: t, .. }
        | DisplayItem::HitTest { transform: t, .. } => *t = transform * *t,
        DisplayItem::PopLayer => {}
    }
}

/// A display list must stay sendable across threads: it is what a renderer process
/// would receive, and what a snapshot test compares. If a variant ever gains an
/// `Rc` or a live handle, this stops compiling.
const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_sync<T: Sync>() {}
    let _ = assert_send::<DisplayList>;
    let _ = assert_sync::<DisplayList>;
};

#[cfg(test)]
mod append_tests {
    use super::*;
    use kurbo::Shape;

    fn a_font() -> FontData {
        FontData::new(
            peniko::Blob::new(std::sync::Arc::new(vec![1u8, 2, 3, 4])),
            0,
        )
    }

    fn another_font() -> FontData {
        FontData::new(peniko::Blob::new(std::sync::Arc::new(vec![9u8, 9, 9])), 0)
    }

    /// A glyph run names its font by an index into its own list's table. Moving
    /// a run to a list whose table holds something else at that index has to
    /// remap it, or the text is drawn in whatever face happened to be there.
    #[test]
    fn appending_remaps_the_fonts_a_run_names() {
        let mut destination = DisplayList::new();
        destination.push_glyphs(
            &another_font(),
            12.0,
            Vec::new(),
            peniko::Brush::Solid(peniko::Color::BLACK),
            kurbo::Affine::IDENTITY,
            true,
            Vec::new(),
        );

        let mut source = DisplayList::new();
        source.push_glyphs(
            &a_font(),
            13.0,
            Vec::new(),
            peniko::Brush::Solid(peniko::Color::BLACK),
            kurbo::Affine::IDENTITY,
            true,
            Vec::new(),
        );

        destination.append(&source);

        let DisplayItem::Glyphs { font, .. } = destination.items()[1] else {
            panic!("the appended item is a glyph run");
        };
        let landed = destination
            .fonts()
            .get(font)
            .expect("interned on the way in");
        assert_eq!(
            landed.data.as_ref(),
            a_font().data.as_ref(),
            "the run kept the face it was drawn with"
        );
        assert_eq!(
            destination.fonts().len(),
            2,
            "and did not overwrite the other"
        );
    }

    #[test]
    fn appending_keeps_paint_order_and_everything_that_is_not_a_run() {
        let mut destination = DisplayList::new();
        destination.push(DisplayItem::PopLayer);

        let mut source = DisplayList::new();
        source.push(DisplayItem::PushLayer {
            blend: peniko::BlendMode::default(),
            alpha: 1.0,
            transform: kurbo::Affine::IDENTITY,
            clip: kurbo::Rect::new(0.0, 0.0, 1.0, 1.0).to_path(0.1),
        });
        source.push(DisplayItem::PopLayer);

        destination.append(&source);
        assert_eq!(destination.len(), 3);
        assert!(matches!(
            destination.items()[1],
            DisplayItem::PushLayer { .. }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurbo::Shape;
    use peniko::Color;

    fn font(bytes: &'static [u8]) -> FontData {
        FontData::new(peniko::Blob::new(std::sync::Arc::new(bytes)), 0)
    }

    #[test]
    fn interning_the_same_font_twice_yields_one_entry() {
        let mut table = FontTable::default();
        let a = font(b"font-bytes");
        let b = a.clone();

        assert_eq!(table.intern(&a), FontId(0));
        assert_eq!(table.intern(&b), FontId(0));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn distinct_fonts_get_distinct_ids() {
        let mut table = FontTable::default();
        assert_eq!(table.intern(&font(b"one")), FontId(0));
        assert_eq!(table.intern(&font(b"two")), FontId(1));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn glyph_runs_reference_the_interned_font() {
        let mut list = DisplayList::new();
        let f = font(b"font-bytes");
        list.push_glyphs(
            &f,
            16.0,
            Vec::new(),
            Brush::Solid(Color::BLACK),
            Affine::IDENTITY,
            true,
            vec![Glyph {
                id: 1,
                x: 0.0,
                y: 0.0,
                text_offset: 0,
            }],
        );

        let DisplayItem::Glyphs { font: id, .. } = &list.items()[0] else {
            panic!("expected a glyph run");
        };
        assert!(list.fonts().get(*id).is_some());
    }

    /// The font table must not print blob contents: a snapshot containing half a
    /// megabyte of font bytes is a snapshot nobody will ever review.
    #[test]
    fn the_font_table_debug_prints_sizes_not_contents() {
        let mut table = FontTable::default();
        table.intern(&font(b"abcdefghij"));
        let debug = format!("{table:?}");
        assert!(debug.contains("10 bytes"), "{debug}");
        assert!(!debug.contains("abcdefghij"), "{debug}");
    }

    #[test]
    fn a_display_list_round_trips_through_json() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::translate((3.0, 4.0)),
            brush: Brush::Solid(Color::from_rgb8(1, 2, 3)),
            brush_transform: None,
            shape: Rect::new(0.0, 0.0, 10.0, 20.0).to_path(0.1),
        });
        list.push(DisplayItem::HitTest {
            rect: Rect::new(0.0, 0.0, 10.0, 20.0),
            transform: Affine::IDENTITY,
            id: HitTestId(7),
        });
        list.push_glyphs(
            &font(b"font-bytes"),
            16.0,
            vec![1, 2],
            Brush::Solid(Color::BLACK),
            Affine::IDENTITY,
            false,
            vec![Glyph {
                id: 9,
                x: 1.0,
                y: 2.0,
                text_offset: 0,
            }],
        );

        let json = serde_json::to_string(&list).expect("serialize");
        let decoded: DisplayList = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, list);
    }

    #[test]
    fn hit_testing_returns_the_last_matching_region() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::HitTest {
            rect: Rect::new(0.0, 0.0, 100.0, 100.0),
            transform: Affine::IDENTITY,
            id: HitTestId(1),
        });
        list.push(DisplayItem::HitTest {
            rect: Rect::new(10.0, 10.0, 20.0, 20.0),
            transform: Affine::IDENTITY,
            id: HitTestId(2),
        });

        // Innermost wins, which here means the one emitted last.
        assert_eq!(list.hit_test((15.0, 15.0).into()), Some(HitTestId(2)));
        assert_eq!(list.hit_test((50.0, 50.0).into()), Some(HitTestId(1)));
        assert_eq!(list.hit_test((500.0, 500.0).into()), None);
    }

    /// Hit regions are transformed like everything else; a scaled region must be
    /// hit in the space it was drawn in, not the space it was authored in.
    #[test]
    fn hit_testing_accounts_for_the_transform() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::HitTest {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            transform: Affine::scale(2.0),
            id: HitTestId(1),
        });

        assert_eq!(list.hit_test((15.0, 15.0).into()), Some(HitTestId(1)));
        assert_eq!(list.hit_test((25.0, 25.0).into()), None);
    }

    fn square(list: &mut DisplayList, y: f64) {
        list.push(DisplayItem::Fill {
            style: Fill::NonZero,
            transform: Affine::IDENTITY,
            brush: Brush::Solid(Color::BLACK),
            brush_transform: None,
            shape: kurbo::Rect::new(0.0, y, 10.0, y + 10.0).to_path(0.1),
        });
    }

    fn tops(list: &DisplayList) -> Vec<f64> {
        list.items()
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Fill {
                    shape, transform, ..
                } => Some(transform.transform_rect_bbox(shape.bounding_box()).y0),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn what_scrolls_moves_by_its_offset_and_what_does_not_stays() {
        let mut list = DisplayList::new();
        square(&mut list, 0.0);
        list.set_spatial(SpatialId(1));
        square(&mut list, 100.0);
        list.set_spatial(SpatialId::VIEWPORT);
        square(&mut list, 20.0);

        let offsets = [kurbo::Vec2::ZERO, kurbo::Vec2::new(0.0, 40.0)];
        assert_eq!(tops(&list.with_offsets(&offsets)), vec![0.0, 60.0, 20.0]);
    }

    #[test]
    fn an_offset_is_taken_in_the_coordinates_the_list_was_built_in() {
        // Built in logical pixels, scaled to a 2x screen afterwards: a scroll of
        // forty logical pixels is eighty on that screen.
        let mut list = DisplayList::new();
        list.set_spatial(SpatialId(1));
        square(&mut list, 100.0);
        list.transform(Affine::scale(2.0));

        let offsets = [kurbo::Vec2::ZERO, kurbo::Vec2::new(0.0, 40.0)];
        assert_eq!(tops(&list.with_offsets(&offsets)), vec![120.0]);
    }

    #[test]
    fn an_empty_run_is_not_kept_and_appending_carries_what_moves() {
        let mut list = DisplayList::new();
        list.set_spatial(SpatialId(3));
        list.set_spatial(SpatialId(1));
        square(&mut list, 0.0);
        let mut into = DisplayList::new();
        square(&mut into, 0.0);
        into.append(&list);
        square(&mut into, 0.0);

        let spatials: Vec<SpatialId> = into.items_with_spatial().map(|(id, _)| id).collect();
        assert_eq!(
            spatials,
            vec![SpatialId::VIEWPORT, SpatialId(1), SpatialId::VIEWPORT],
            "what was appended moves, and what came after it does not"
        );
    }
}
