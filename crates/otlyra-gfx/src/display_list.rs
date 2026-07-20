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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DisplayList {
    items: Vec<DisplayItem>,
    fonts: FontTable,
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
        let font = self.fonts.intern(font);
        self.items.push(DisplayItem::Glyphs {
            font,
            font_size,
            normalized_coords,
            brush,
            transform,
            hint,
            glyphs,
        });
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
}
