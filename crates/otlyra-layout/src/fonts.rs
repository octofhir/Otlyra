//! From a style's `font-family` to the font stack the text layer shapes with.

use std::collections::HashMap;

use otlyra_css::{FamilyName, FontFamily, GenericFamily};
use otlyra_text::{Family, FontStack};

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
}
