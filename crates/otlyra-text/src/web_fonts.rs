//! The faces a page brings with `@font-face` (CSS Fonts 4 §4), and which of a
//! family's faces a run of text is set in (§5.2).
//!
//! The rule itself is the cascade's to read; what arrives here is only what a
//! face is — its descriptors — and what a run asks for. The narrowing is kept a
//! pure function of the two so that what decides which files to fetch and what
//! decides which registered face to shape with are one and the same answer.

use std::collections::HashMap;
use std::ops::{Range, RangeInclusive};

use unicode_segmentation::UnicodeSegmentation;

use crate::{Family, FontStack, GenericFamily};

/// The `font-style` a face declares, on the scale CSS Fonts 4 §5.2 matches on.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FaceStyle {
    /// Upright: `normal`, which the descriptor also spells `oblique 0deg`.
    Normal,
    /// `italic`.
    Italic,
    /// `oblique` over a range of angles, in degrees, the smaller first.
    Oblique(f32, f32),
}

/// What an `@font-face` rule says its face is, apart from where it comes from.
///
/// An absent descriptor has its initial value. For `font-weight` and
/// `font-stretch` that is `auto`, which for a face with no variation axes is
/// `normal`: a face is taken to be what its descriptors say rather than what
/// the file claims, which is what lets one file stand in for several weights.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceDescriptors {
    /// `font-weight`, as the range of weights the face covers.
    pub weight: RangeInclusive<f32>,
    /// `font-style`.
    pub style: FaceStyle,
    /// `font-stretch`, as the range of widths the face covers, in percent.
    pub stretch: RangeInclusive<f32>,
    /// `unicode-range`: the code points the face may be used for. Empty is
    /// every code point, which is the descriptor's initial value.
    pub unicode_range: Vec<RangeInclusive<u32>>,
}

impl Default for FaceDescriptors {
    fn default() -> Self {
        Self {
            weight: 400.0..=400.0,
            style: FaceStyle::Normal,
            stretch: 100.0..=100.0,
            unicode_range: Vec::new(),
        }
    }
}

impl FaceDescriptors {
    /// Whether the face may be used for `c`.
    #[must_use]
    pub fn covers(&self, c: char) -> bool {
        self.unicode_range.is_empty()
            || self
                .unicode_range
                .iter()
                .any(|range| range.contains(&u32::from(c)))
    }

    /// Whether the face may be used for any of `chars`: whether a page that
    /// sets these characters in its family needs this face's file at all
    /// (CSS Fonts 4 §4.8).
    #[must_use]
    pub fn intersects(&self, chars: &[char]) -> bool {
        chars.iter().any(|&c| self.covers(c))
    }
}

/// What a run of text asks of a family: the computed `font-weight`, whether
/// `font-style` is italic, and `font-width` in percent.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FaceQuery {
    /// `font-weight`.
    pub weight: f32,
    /// Whether the run is italic.
    pub italic: bool,
    /// `font-width`, in percent.
    pub stretch: f32,
}

impl Default for FaceQuery {
    fn default() -> Self {
        Self {
            weight: 400.0,
            italic: false,
            stretch: 100.0,
        }
    }
}

/// How near a face comes to what was asked, on one descriptor: the pass of
/// the search it is found in, then how far into that pass. Lower is nearer.
type Nearness = (u8, f32);

/// The faces of one family a run asks for, by CSS Fonts 4 §5.2: the set is
/// narrowed by width, then by style, then by weight, each time to the faces
/// that include the nearest value any face offers.
///
/// Every face with the winning descriptors is returned, which is every
/// `unicode-range` subset of the composite face they make together; which of
/// those a character is set in is a question of coverage, not of matching.
/// Indices are into `faces`, in the order given.
#[must_use]
pub fn matching_faces<'a>(
    faces: impl IntoIterator<Item = &'a FaceDescriptors>,
    query: FaceQuery,
) -> Vec<usize> {
    let faces: Vec<&FaceDescriptors> = faces.into_iter().collect();
    let mut set: Vec<usize> = (0..faces.len()).collect();

    // Width, then weight, narrow to the faces whose range includes the value
    // found; style compares on a scale mixing italic and oblique, so it keeps
    // the faces as near as the nearest.
    let width = nearest(&set, |i| {
        range_nearness(&faces[i].stretch, query.stretch, stretch_nearness)
    });
    if let Some((_, value)) = width {
        set.retain(|&i| faces[i].stretch.contains(&value));
    }

    let style = set
        .iter()
        .map(|&i| style_nearness(faces[i].style, query.italic))
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    if let Some(best) = style {
        set.retain(|&i| style_nearness(faces[i].style, query.italic) == best);
    }

    let weight = nearest(&set, |i| {
        range_nearness(&faces[i].weight, query.weight, weight_nearness)
    });
    if let Some((_, value)) = weight {
        set.retain(|&i| faces[i].weight.contains(&value));
    }
    set
}

/// The nearest a face in `set` comes, and the value in its range that does.
fn nearest(set: &[usize], nearness: impl Fn(usize) -> (Nearness, f32)) -> Option<(Nearness, f32)> {
    set.iter()
        .map(|&i| nearness(i))
        .min_by(|(a, _), (b, _)| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
}

/// How near a range comes to `wanted`, through the value in it nearest to
/// `wanted`, which is where any search reaches the range first.
fn range_nearness(
    range: &RangeInclusive<f32>,
    wanted: f32,
    nearness: fn(f32, f32) -> Nearness,
) -> (Nearness, f32) {
    let value = wanted.clamp(*range.start(), *range.end());
    (nearness(value, wanted), value)
}

/// `font-width`: at or below normal, narrower widths first, nearest first,
/// then wider ones; above normal, the other way round.
fn stretch_nearness(value: f32, wanted: f32) -> Nearness {
    let first = if wanted <= 100.0 {
        value <= wanted
    } else {
        value >= wanted
    };
    (u8::from(!first), (wanted - value).abs())
}

/// `font-weight`: between 400 and 500 inclusive, heavier weights up to 500,
/// then lighter ones, then those past 500; below 400, lighter first; above
/// 500, heavier first.
fn weight_nearness(value: f32, wanted: f32) -> Nearness {
    let distance = (wanted - value).abs();
    if (400.0..=500.0).contains(&wanted) {
        if value >= wanted && value <= 500.0 {
            (0, distance)
        } else if value < wanted {
            (1, distance)
        } else {
            (2, distance)
        }
    } else {
        let first = if wanted < 400.0 {
            value <= wanted
        } else {
            value >= wanted
        };
        (u8::from(!first), distance)
    }
}

/// `font-style`, where a run is either italic or normal.
///
/// Italic takes an italic face, then an oblique one from 11 degrees up, then
/// one below 11 down towards upright, then upright or leaning back. Normal
/// takes upright or leaning forward, the least first, then italic, then
/// leaning back. Upright is oblique 0deg throughout.
fn style_nearness(style: FaceStyle, italic: bool) -> Nearness {
    /// Where an italic request starts on the oblique scale: CSS maps an italic
    /// value of 1 to the same point as 11deg.
    const ITALIC_ANGLE: f32 = 11.0;

    let (low, high) = match style {
        FaceStyle::Italic => return if italic { (0, 0.0) } else { (1, 0.0) },
        FaceStyle::Normal => (0.0, 0.0),
        FaceStyle::Oblique(low, high) => (low, high),
    };
    if italic {
        let angle = ITALIC_ANGLE.clamp(low, high);
        if angle >= ITALIC_ANGLE {
            (1, angle - ITALIC_ANGLE)
        } else if angle > 0.0 {
            (2, ITALIC_ANGLE - angle)
        } else {
            (3, -angle)
        }
    } else if high >= 0.0 {
        (0, low.max(0.0))
    } else {
        (2, -high)
    }
}

/// One face a page brought, as the collection knows it.
#[derive(Debug)]
pub(crate) struct WebFace {
    /// The family name it is registered under: one no page can spell, so that
    /// each face is a family of its own and the choice between a family's
    /// faces is made here, by [`matching_faces`], rather than by the font
    /// library's own matching over whatever happened to arrive.
    pub(crate) name: String,
    /// What its rule says it is.
    pub(crate) descriptors: FaceDescriptors,
    /// Where its rule stands among the page's `@font-face` rules.
    order: usize,
}

/// The families pages have defined with `@font-face`, and their faces.
///
/// Process-wide, like the collection the faces are registered in: two
/// documents that define a family of the same name share its faces, where a
/// browser keeps each document's rules to itself.
#[derive(Debug, Default)]
pub(crate) struct WebFamilies {
    /// Faces by ASCII-lowercased family name, last-declared first.
    families: HashMap<String, Vec<WebFace>>,
    /// How many faces have been registered, which numbers the next one.
    registered: usize,
}

/// One entry of a font stack once the page's own families are in it.
#[derive(Debug)]
pub(crate) enum Resolved<'a> {
    /// A face a page brought.
    Web(&'a WebFace),
    /// An installed family, by name.
    Installed(&'a str),
    /// A generic family.
    Generic(GenericFamily),
}

impl WebFamilies {
    /// A collection family name for the next face of `family`.
    pub(crate) fn next_name(&mut self, family: &str) -> String {
        self.registered += 1;
        format!("\u{1}{family}\u{1}{}", self.registered)
    }

    /// Record a face registered under `name`.
    pub(crate) fn insert(
        &mut self,
        family: &str,
        name: String,
        descriptors: FaceDescriptors,
        order: usize,
    ) {
        let faces = self
            .families
            .entry(family.to_ascii_lowercase())
            .or_default();
        let at = faces.partition_point(|face| face.order >= order);
        faces.insert(
            at,
            WebFace {
                name,
                descriptors,
                order,
            },
        );
    }

    /// Whether a page has brought a face of `family`.
    pub(crate) fn contains(&self, family: &str) -> bool {
        self.families.contains_key(&family.to_ascii_lowercase())
    }

    /// The stack as it is to be searched for a run asking `query`.
    ///
    /// A family a page defined stands for the faces of it that match, in
    /// reverse order of declaration (CSS Fonts 4 §4.5), and an installed
    /// family of the same name is not consulted (§5.2): the page's rule is
    /// what the name means now. Which of the faces sets a given character is
    /// then the shaper's per-character fallback through them.
    ///
    /// The faces matched are those registered, not all the page declares: a
    /// face still loading, or whose every source failed, is not there, and
    /// the nearest one that is stands in for it where CSS would fall through
    /// to the next family.
    pub(crate) fn expand<'a>(
        &'a self,
        stack: &'a FontStack,
        query: FaceQuery,
    ) -> Vec<Resolved<'a>> {
        let mut resolved = Vec::with_capacity(stack.families().len());
        for family in stack.families() {
            match family {
                Family::Named(name) => match self.families.get(&name.to_ascii_lowercase()) {
                    Some(faces) => resolved.extend(
                        matching_faces(faces.iter().map(|face| &face.descriptors), query)
                            .into_iter()
                            .map(|i| Resolved::Web(&faces[i])),
                    ),
                    None => resolved.push(match system_alias(name) {
                        Some(generic) => Resolved::Generic(generic),
                        None => Resolved::Installed(name),
                    }),
                },
                Family::Generic(generic) => resolved.push(Resolved::Generic(*generic)),
            }
        }
        resolved
    }

    /// The stack as the shaper takes it, for a run asking `query`, with
    /// every face the run matches in it.
    pub(crate) fn parley_family(
        &self,
        stack: &FontStack,
        query: FaceQuery,
    ) -> parley::FontFamily<'static> {
        to_parley(&self.expand(stack, query), |_| true)
    }

    /// The stack as the shaper takes it for `text`, a run asking `query`,
    /// piece by piece: each byte range of the text with the stack its
    /// characters may be set in.
    ///
    /// A face whose `unicode-range` excludes a character is not a face that
    /// character may be set in (CSS Fonts 4 §4.5), whatever glyphs its file
    /// happens to carry — a subset served for one script often carries the
    /// Latin letters too. The shaper's own fallback goes by the glyphs, so it
    /// is handed each run of clusters with only the faces whose ranges include
    /// them. A cluster goes by its first character, its base.
    pub(crate) fn parley_families(
        &self,
        stack: &FontStack,
        query: FaceQuery,
        text: &str,
    ) -> Vec<(Range<usize>, parley::FontFamily<'static>)> {
        let expanded = self.expand(stack, query);
        let ranged: Vec<&FaceDescriptors> = expanded
            .iter()
            .filter_map(|entry| match entry {
                Resolved::Web(face) if !face.descriptors.unicode_range.is_empty() => {
                    Some(&face.descriptors)
                }
                Resolved::Web(_) | Resolved::Installed(_) | Resolved::Generic(_) => None,
            })
            .collect();
        if ranged.is_empty() {
            return vec![(0..text.len(), to_parley(&expanded, |_| true))];
        }

        let coverage = |cluster: &str| -> Vec<bool> {
            let base = cluster.chars().next().unwrap_or(' ');
            ranged.iter().map(|face| face.covers(base)).collect()
        };
        let mut pieces: Vec<(Range<usize>, Vec<bool>)> = Vec::new();
        for (at, cluster) in text.grapheme_indices(true) {
            let covered = coverage(cluster);
            match pieces.last_mut() {
                Some((range, last)) if *last == covered => range.end = at + cluster.len(),
                _ => pieces.push((at..at + cluster.len(), covered)),
            }
        }
        pieces
            .into_iter()
            .map(|(range, covered)| {
                let family = to_parley(&expanded, |face| {
                    ranged
                        .iter()
                        .position(|ranged| std::ptr::eq(*ranged, face))
                        .is_none_or(|i| covered[i])
                });
                (range, family)
            })
            .collect()
    }
}

/// An expanded stack as the shaper names it, with the faces `keep` refuses
/// left out.
fn to_parley(
    expanded: &[Resolved<'_>],
    keep: impl Fn(&FaceDescriptors) -> bool,
) -> parley::FontFamily<'static> {
    let names = expanded
        .iter()
        .filter_map(|entry| match entry {
            Resolved::Web(face) => keep(&face.descriptors)
                .then(|| parley::FontFamilyName::Named(face.name.clone().into())),
            Resolved::Installed(name) => {
                Some(parley::FontFamilyName::Named((*name).to_owned().into()))
            }
            Resolved::Generic(generic) => {
                Some(parley::FontFamilyName::Generic(generic.to_parley()))
            }
        })
        .collect::<Vec<_>>();
    parley::FontFamily::List(names.into())
}

/// The generic an installed-family name stands for on this system.
///
/// `-apple-system` and `BlinkMacSystemFont` are the names pages used for the
/// system interface font before `system-ui` existed, and still list first.
/// Blink and Gecko on macOS take both for `system-ui`; nowhere else do they
/// name anything.
fn system_alias(name: &str) -> Option<GenericFamily> {
    let alias = ["-apple-system", "BlinkMacSystemFont"]
        .iter()
        .any(|alias| name.eq_ignore_ascii_case(alias));
    (cfg!(target_os = "macos") && alias).then_some(GenericFamily::SystemUi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weighing(weight: f32) -> FaceDescriptors {
        FaceDescriptors {
            weight: weight..=weight,
            ..FaceDescriptors::default()
        }
    }

    fn wanting(weight: f32) -> FaceQuery {
        FaceQuery {
            weight,
            ..FaceQuery::default()
        }
    }

    /// The weights of §5.2's example: between 400 and 500 a heavier face up
    /// to 500 is preferred and a lighter one next; above 500, heavier first.
    #[test]
    fn weight_is_matched_as_css_fonts_orders_it() {
        let faces: Vec<FaceDescriptors> = [300.0, 400.0, 600.0, 700.0]
            .into_iter()
            .map(weighing)
            .collect();
        let chosen = |weight| {
            matching_faces(&faces, wanting(weight))
                .into_iter()
                .map(|i| *faces[i].weight.start())
                .collect::<Vec<_>>()
        };
        assert_eq!(chosen(500.0), [400.0], "lighter before past 500");
        assert_eq!(chosen(600.0), [600.0]);
        assert_eq!(chosen(650.0), [700.0], "heavier first above 500");
        assert_eq!(chosen(350.0), [300.0], "lighter first below 400");
        assert_eq!(chosen(450.0), [400.0]);
    }

    /// A range contains what it covers, and a request inside it matches it
    /// ahead of a static face that is nearer to nothing.
    #[test]
    fn a_weight_range_contains_its_weights() {
        let faces = [
            weighing(400.0),
            FaceDescriptors {
                weight: 500.0..=900.0,
                ..FaceDescriptors::default()
            },
        ];
        assert_eq!(matching_faces(&faces, wanting(650.0)), [1]);
        assert_eq!(matching_faces(&faces, wanting(400.0)), [0]);
    }

    /// Italic asked of a family with no italic takes the upright face, and an
    /// oblique one ahead of it when there is one.
    #[test]
    fn italic_falls_back_to_oblique_then_upright() {
        let italic = FaceQuery {
            italic: true,
            ..FaceQuery::default()
        };
        let upright = [FaceDescriptors::default()];
        assert_eq!(matching_faces(&upright, italic), [0]);

        let leaning = [
            FaceDescriptors::default(),
            FaceDescriptors {
                style: FaceStyle::Oblique(8.0, 8.0),
                ..FaceDescriptors::default()
            },
            FaceDescriptors {
                style: FaceStyle::Italic,
                ..FaceDescriptors::default()
            },
        ];
        assert_eq!(matching_faces(&leaning, italic), [2]);
        assert_eq!(matching_faces(&leaning[..2], italic), [1]);
        assert_eq!(
            matching_faces(&leaning, FaceQuery::default()),
            [0],
            "and upright asked is upright"
        );
    }

    /// Width narrows before weight: a normal-width face of the wrong weight
    /// beats a condensed face of the right one.
    #[test]
    fn stretch_narrows_first() {
        let faces = [
            FaceDescriptors {
                stretch: 75.0..=75.0,
                ..weighing(700.0)
            },
            weighing(400.0),
        ];
        assert_eq!(matching_faces(&faces, wanting(700.0)), [1]);
        let condensed = FaceQuery {
            stretch: 75.0,
            ..wanting(400.0)
        };
        assert_eq!(matching_faces(&faces, condensed), [0]);

        let wide = [
            FaceDescriptors::default(),
            FaceDescriptors {
                stretch: 125.0..=125.0,
                ..FaceDescriptors::default()
            },
        ];
        let wanting_wide = |stretch| FaceQuery {
            stretch,
            ..FaceQuery::default()
        };
        assert_eq!(matching_faces(&wide, wanting_wide(125.0)), [1]);
        assert_eq!(
            matching_faces(&wide, wanting_wide(110.0)),
            [1],
            "wider first"
        );
        assert_eq!(
            matching_faces(&wide, wanting_wide(90.0)),
            [0],
            "narrower first"
        );
    }

    /// Faces that differ only in the code points they cover all match: they
    /// are one composite face.
    #[test]
    fn subsets_match_together() {
        let subset = |start: u32, end: u32| FaceDescriptors {
            unicode_range: vec![start..=end],
            ..FaceDescriptors::default()
        };
        let faces = [subset(0, 0xFF), subset(0x400, 0x4FF), weighing(700.0)];
        assert_eq!(matching_faces(&faces, FaceQuery::default()), [0, 1]);
        assert!(faces[0].intersects(&['a']));
        assert!(!faces[1].intersects(&['a', 'b']));
        assert!(faces[1].intersects(&['a', 'ж']));
        assert!(faces[2].covers('ж'), "no range is every code point");
    }

    /// On macOS, the names pages give the system interface font are that
    /// font, unless a page has defined a family of that name itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn apple_system_is_the_system_font_on_macos() {
        let families = WebFamilies::default();
        for name in ["-apple-system", "blinkmacsystemfont"] {
            let stack = FontStack::named(name);
            let expanded = families.expand(&stack, FaceQuery::default());
            assert!(
                matches!(expanded[0], Resolved::Generic(GenericFamily::SystemUi)),
                "{name}: {expanded:?}"
            );
        }
    }
}
