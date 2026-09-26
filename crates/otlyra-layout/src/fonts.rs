//! From a style's `font-family` to the font stack the text layer shapes with,
//! and from a page's `@font-face` rules and its text to the faces it needs.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use otlyra_css::cascade::{FaceStyle, FontFace};
use otlyra_css::{ComputedStyle, FamilyName, FontFamily, GenericFamily};
use otlyra_text::{FaceDescriptors, FaceQuery, Family, FontStack};

use crate::box_tree::{BoxKind, BoxTree};

/// The font stack for each `font-family` list a layout meets, made once per list.
///
/// Keyed by the list's identity rather than its contents. Every element that
/// inherits its family holds the very list its parent does, so a whole document
/// comes to a handful of entries and a lookup is a pointer and a hash, where
/// comparing the lists would be comparing every name in them. Each entry keeps
/// its list alive, so the address it is keyed by cannot be handed to another
/// list while it is here.
#[derive(Default)]
pub(crate) struct FontStacks {
    made: HashMap<usize, (FontFamily, FontStack)>,
}

impl FontStacks {
    /// The stack for `family`, in the order the page listed the families.
    pub(crate) fn of(&mut self, family: &FontFamily) -> FontStack {
        let (_, stack) = self
            .made
            .entry(family.as_ptr().addr())
            .or_insert_with(|| (family.clone(), stack_of(family)));
        stack.clone()
    }
}

/// The text layer's stack for a list, entry for entry. The text layer puts its
/// standard font behind a list that names no generic, as CSS asks.
fn stack_of(family: &FontFamily) -> FontStack {
    FontStack::new(family.iter().map(|entry| match entry {
        FamilyName::Named(name) => Family::Named(name.to_string()),
        FamilyName::Generic(generic) => Family::Generic(generic_of(*generic)),
    }))
}

fn generic_of(generic: GenericFamily) -> otlyra_text::GenericFamily {
    match generic {
        GenericFamily::Serif => otlyra_text::GenericFamily::Serif,
        GenericFamily::SansSerif => otlyra_text::GenericFamily::SansSerif,
        GenericFamily::Monospace => otlyra_text::GenericFamily::Monospace,
        GenericFamily::Cursive => otlyra_text::GenericFamily::Cursive,
        GenericFamily::Fantasy => otlyra_text::GenericFamily::Fantasy,
        GenericFamily::SystemUi => otlyra_text::GenericFamily::SystemUi,
    }
}

/// Whether a style's text is set in an italic face.
pub(crate) fn is_italic(style: &ComputedStyle) -> bool {
    style.font_style == otlyra_css::FontStyle::Italic
}

/// What a style asks of a family's faces.
pub(crate) fn face_query(style: &ComputedStyle) -> FaceQuery {
    FaceQuery {
        weight: f32::from(style.font_weight),
        italic: is_italic(style),
        stretch: style.font_width,
    }
}

/// A `@font-face` rule's face as the text layer matches it.
#[must_use]
pub fn face_descriptors(face: &FontFace) -> FaceDescriptors {
    FaceDescriptors {
        weight: face.weight.clone(),
        style: match face.style {
            FaceStyle::Normal => otlyra_text::FaceStyle::Normal,
            FaceStyle::Italic => otlyra_text::FaceStyle::Italic,
            FaceStyle::Oblique(low, high) => otlyra_text::FaceStyle::Oblique(low, high),
        },
        stretch: face.stretch.clone(),
        unicode_range: face.unicode_range.clone(),
    }
}

/// What a page sets in one named family, at one weight, style and width.
#[derive(Clone, Debug, PartialEq)]
pub struct FontUse {
    /// The family, as the first style to name it spelled it.
    pub family: Arc<str>,
    /// What is asked of the family's faces.
    pub query: FaceQuery,
    /// The characters, in order and each once.
    pub chars: Vec<char>,
}

/// Every named family the text in `tree` may be set in, with what it asks of
/// the family and the characters it would set.
///
/// A face is loaded only when the page needs it (CSS Fonts 4 §4.8): when
/// text is styled with its family, at descriptors it matches, and has a
/// character in its `unicode-range`. Every family a text's list names counts,
/// not just the first, because which characters the first cannot set is not
/// known before its faces are here. Generic families are the system's, and
/// have no faces to load.
#[must_use]
pub fn font_uses(tree: &BoxTree) -> Vec<FontUse> {
    let mut uses: Vec<(Arc<str>, FaceQuery, BTreeSet<char>)> = Vec::new();
    let mut add = |style: &ComputedStyle, text: &str| {
        let query = face_query(style);
        for family in style.font_family.iter() {
            let name = match family {
                FamilyName::Named(name) => name,
                FamilyName::Generic(_) => continue,
            };
            let at = uses
                .iter()
                .position(|(used, asked, _)| used.eq_ignore_ascii_case(name) && *asked == query)
                .unwrap_or_else(|| {
                    uses.push((name.clone(), query, BTreeSet::new()));
                    uses.len() - 1
                });
            uses[at].2.extend(text.chars());
        }
    };
    for id in tree.descendants(tree.root()) {
        let node = tree.node(id);
        match &node.kind {
            BoxKind::Text(text) => add(&node.style, text),
            BoxKind::Block | BoxKind::Inline | BoxKind::Replaced(_) => {}
        }
        if let Some(marker) = tree.marker(id) {
            add(&node.style, &marker.text);
        }
    }
    uses.into_iter()
        .map(|(family, query, chars)| FontUse {
            family,
            query,
            chars: chars.into_iter().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use otlyra_css::ComputedStyle;

    use super::*;

    /// A child that inherits its family shares its parent's stack, made once,
    /// and the stack keeps the families in the order they were listed.
    #[test]
    fn an_inherited_family_is_one_stack_in_the_order_listed() {
        let parent = ComputedStyle {
            font_family: FontFamily::new([
                FamilyName::Named("serif".into()),
                FamilyName::Named("A, B".into()),
                FamilyName::Generic(GenericFamily::Monospace),
            ]),
            ..ComputedStyle::default()
        };
        let child = ComputedStyle::inheriting_from(&parent);

        let mut stacks = FontStacks::default();
        let first = stacks.of(&parent.font_family);
        let second = stacks.of(&child.font_family);

        assert_eq!(first, second);
        assert_eq!(stacks.made.len(), 1, "made once, for both");
        assert_eq!(
            first.families(),
            [
                Family::Named("serif".to_owned()),
                Family::Named("A, B".to_owned()),
                Family::Generic(otlyra_text::GenericFamily::Monospace),
            ]
        );
    }

    /// A list that names no generic still ends in one: the standard font.
    #[test]
    fn a_list_of_names_falls_back_to_the_standard_font() {
        let mut stacks = FontStacks::default();
        let stack = stacks.of(&FontFamily::new([FamilyName::Named("Inter".into())]));
        assert_eq!(
            stack.families(),
            [
                Family::Named("Inter".to_owned()),
                Family::Generic(otlyra_text::GenericFamily::Serif),
            ]
        );
    }

    /// The box tree `html` makes, styled by the cascade as a page would be.
    fn tree_of(html: &str) -> BoxTree {
        let parsed = otlyra_html::parse(html.as_bytes(), Some("utf-8"));
        let styles = otlyra_css::cascade::style_document(&parsed.document, Default::default());
        crate::build_box_tree(&parsed.document, &styles)
    }

    /// Each named family is used at the weight and style its text asks, with
    /// the characters of that text; a family named differently in case is
    /// the same family, a generic is no family to load, and a list marker's
    /// text counts.
    #[test]
    fn a_page_uses_each_family_at_what_its_text_asks() {
        let tree = tree_of(
            "<body style='font-family: Brought, sans-serif'>\
               <p>ab<b style='font-family: brought'>ba</b></p>\
               <ul><li style='font-family: Listed'>x</ul>",
        );
        let uses = font_uses(&tree);
        let find = |family: &str, weight: f32| {
            uses.iter()
                .find(|used| {
                    used.family.eq_ignore_ascii_case(family) && used.query.weight == weight
                })
                .unwrap_or_else(|| panic!("{family} at {weight}: {uses:?}"))
        };
        assert_eq!(find("Brought", 400.0).chars, ['a', 'b']);
        assert_eq!(find("Brought", 700.0).chars, ['a', 'b']);
        assert!(find("Listed", 400.0).chars.contains(&'x'));
        assert!(
            find("Listed", 400.0).chars.contains(&'•'),
            "the bullet: {uses:?}"
        );
        assert!(
            uses.iter()
                .all(|used| !used.family.eq_ignore_ascii_case("sans-serif")),
            "{uses:?}"
        );
        assert_eq!(uses.len(), 3, "{uses:?}");
    }
}
