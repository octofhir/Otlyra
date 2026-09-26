//! The faces pages bring with `@font-face`: which of them a page needs, and
//! each source of one tried in turn until one gives a font (CSS Fonts 4 §4).
//!
//! A face belongs to the shaper rather than to a page: once it is in, every
//! page that names its family is set in it. `font-display` is not read: text
//! is set in its fallback until a face arrives and in the face from then on,
//! which is `swap` whatever the rule asked for.

use otlyra_css::cascade::{FaceSource, FontFace};
use otlyra_layout::{FontUse, face_descriptors};
use otlyra_text::matching_faces;

use crate::fetcher::{Fetched, ResourceKind};

use super::Browser;

/// How many faces one sweep of one page may start loading.
///
/// A safety cap and nothing else: what is loaded is what the page's text
/// needs, and a page whose text needs more than this many faces at once is
/// asking for megabytes of typefaces before its first line is set.
const FACE_LIMIT: usize = 64;

/// A face on its way in: the rule, where it stands among its page's rules, the
/// document that asked, and which of its sources is to be tried next.
#[derive(Debug)]
pub(super) struct FaceLoad {
    face: FontFace,
    order: usize,
    document: String,
    next_source: usize,
}

/// The faces of `faces` the text described by `uses` needs, by index: for
/// each family used, the faces matching what its text asks for (§5.2) whose
/// `unicode-range` includes a character of that text (§4.8).
fn needed_faces(faces: &[FontFace], uses: &[FontUse]) -> Vec<usize> {
    let mut needed: Vec<usize> = Vec::new();
    for used in uses {
        let (indices, descriptors): (Vec<usize>, Vec<_>) = faces
            .iter()
            .enumerate()
            .filter(|(_, face)| face.family.eq_ignore_ascii_case(&used.family))
            .map(|(index, face)| (index, face_descriptors(face)))
            .unzip();
        needed.extend(
            matching_faces(&descriptors, used.query)
                .into_iter()
                .filter(|&matched| descriptors[matched].intersects(&used.chars))
                .map(|matched| indices[matched]),
        );
    }
    needed.sort_unstable();
    needed.dedup();
    needed
}

impl Browser {
    /// Start loading the faces the pages' text needs.
    ///
    /// A `@font-face` rule is only known once the sheet holding it has been
    /// parsed, and which text uses it only once the page has boxes — both of
    /// which happen on the way to a frame — so this is asked after one, exactly
    /// as a background picture is.
    pub(super) fn fetch_fonts(&mut self, tabs: &[usize]) {
        for &index in tabs {
            let Some(page) = self.tabs[index].page.as_ref() else {
                continue;
            };
            let faces = page.wanted_fonts();
            if faces.is_empty() {
                continue;
            }
            let document = page.url().to_string();
            let uses = otlyra_layout::font_uses(page.boxes());

            for order in needed_faces(&faces, &uses).into_iter().take(FACE_LIMIT) {
                let face = &faces[order];
                if self.requested_faces.contains(face) {
                    continue;
                }
                self.requested_faces.push(face.clone());
                self.load_face(FaceLoad {
                    face: face.clone(),
                    order,
                    document: document.clone(),
                    next_source: 0,
                });
            }
        }
    }

    /// Try a face's sources from the next one on, until one is fetching or
    /// has given a font. Returns whether a face was registered now, which a
    /// `local()` source does without waiting.
    fn load_face(&mut self, mut load: FaceLoad) -> bool {
        while let Some(source) = load.face.sources.get(load.next_source).cloned() {
            load.next_source += 1;
            match source {
                FaceSource::Url(url) => {
                    if !Self::may_reach(&load.document, &url) {
                        continue;
                    }
                    let id = self.fetcher.request(url.as_str(), ResourceKind::Font);
                    self.font_fetches.insert(id, load);
                    return false;
                }
                FaceSource::Local(name) => {
                    let descriptors = face_descriptors(&load.face);
                    if self
                        .text
                        .add_local_face(&name, &load.face.family, &descriptors, load.order)
                    {
                        tracing::debug!(family = %load.face.family, %name, "installed face registered");
                        self.face_registered();
                        return true;
                    }
                }
            }
        }
        tracing::warn!(family = %load.face.family, "no source of a face gave a font");
        false
    }

    /// A face's file arrived, or failed to. Returns whether a face was
    /// registered, now or from the next source.
    ///
    /// A file that did not arrive, or is not a font, is a source that did not
    /// work, and the next one is tried (§4.3): a page lists a second format
    /// for the browser that cannot read the first.
    pub(super) fn face_arrived(&mut self, fetched: Fetched, load: FaceLoad) -> bool {
        let family = &load.face.family;
        match fetched.result {
            Ok(loaded) => {
                let descriptors = face_descriptors(&load.face);
                if self
                    .text
                    .add_web_face(family, &descriptors, load.order, loaded.bytes)
                {
                    tracing::debug!(%family, url = %fetched.url, "face registered");
                    self.face_registered();
                    return true;
                }
                tracing::warn!(%family, url = %fetched.url, "font failed to register");
            }
            Err(error) => {
                tracing::warn!(%family, url = %fetched.url, %error, "font failed to load")
            }
        }
        self.load_face(load)
    }

    /// Every page is set again: its lines were measured in whatever its
    /// stacks fell back to, and a face that has arrived may be one of them.
    fn face_registered(&mut self) {
        for tab in &mut self.tabs {
            if let Some(page) = tab.page.as_mut() {
                page.font_arrived();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use otlyra_css::cascade::FaceStyle;
    use otlyra_text::FaceQuery;

    use super::*;

    fn face(
        family: &str,
        weight: f32,
        unicode_range: Vec<std::ops::RangeInclusive<u32>>,
    ) -> FontFace {
        FontFace {
            family: family.to_owned(),
            sources: vec![FaceSource::Local(family.to_owned())],
            weight: weight..=weight,
            style: FaceStyle::Normal,
            stretch: 100.0..=100.0,
            unicode_range,
        }
    }

    fn using(family: &str, weight: f32, text: &str) -> FontUse {
        FontUse {
            family: family.into(),
            query: FaceQuery {
                weight,
                ..FaceQuery::default()
            },
            chars: text.chars().collect(),
        }
    }

    /// Of twenty declared faces, the one the text asks for is the one needed,
    /// wherever it stands.
    #[test]
    fn only_the_face_the_text_matches_is_needed() {
        let faces: Vec<FontFace> = (1..=20)
            .map(|n| face("Many", 100.0 + n as f32 * 10.0, Vec::new()))
            .collect();
        assert_eq!(needed_faces(&faces, &[using("many", 300.0, "a")]), [19]);
        assert_eq!(
            needed_faces(&faces, &[using("Other", 300.0, "a")]),
            [] as [usize; 0]
        );
    }

    /// Of a Latin and a Cyrillic subset, English text needs the Latin one.
    #[test]
    fn a_subset_is_needed_only_for_its_characters() {
        let faces = [
            face("Split", 400.0, vec![0..=0xFF]),
            face("Split", 400.0, vec![0x400..=0x4FF]),
        ];
        assert_eq!(needed_faces(&faces, &[using("Split", 400.0, "hello")]), [0]);
        assert_eq!(
            needed_faces(&faces, &[using("Split", 400.0, "hi мир")]),
            [0, 1]
        );
    }
}
