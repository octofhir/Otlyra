//! CSS font stacks: an ordered list of families ending in a generic.

/// A CSS generic family keyword.
///
/// This is the subset CSS 2.1 defines plus `system-ui`. The `ui-*` and `math`
/// keywords parley also knows are omitted until something asks for them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GenericFamily {
    /// `serif`
    Serif,
    /// `sans-serif`
    SansSerif,
    /// `monospace`
    Monospace,
    /// `cursive`
    Cursive,
    /// `fantasy`
    Fantasy,
    /// `system-ui`
    SystemUi,
}

impl GenericFamily {
    /// The CSS keyword spelling.
    pub fn as_css(self) -> &'static str {
        match self {
            Self::Serif => "serif",
            Self::SansSerif => "sans-serif",
            Self::Monospace => "monospace",
            Self::Cursive => "cursive",
            Self::Fantasy => "fantasy",
            Self::SystemUi => "system-ui",
        }
    }

    /// Parse a CSS generic keyword, ASCII-case-insensitively as CSS requires.
    pub fn parse(keyword: &str) -> Option<Self> {
        Some(match keyword.to_ascii_lowercase().as_str() {
            "serif" => Self::Serif,
            "sans-serif" => Self::SansSerif,
            "monospace" => Self::Monospace,
            "cursive" => Self::Cursive,
            "fantasy" => Self::Fantasy,
            "system-ui" => Self::SystemUi,
            _ => return None,
        })
    }

    pub(crate) fn to_parley(self) -> parley::GenericFamily {
        match self {
            Self::Serif => parley::GenericFamily::Serif,
            Self::SansSerif => parley::GenericFamily::SansSerif,
            Self::Monospace => parley::GenericFamily::Monospace,
            Self::Cursive => parley::GenericFamily::Cursive,
            Self::Fantasy => parley::GenericFamily::Fantasy,
            Self::SystemUi => parley::GenericFamily::SystemUi,
        }
    }
}

/// One entry in a font stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Family {
    /// A family named explicitly, matched case-insensitively against the collection.
    Named(String),
    /// A generic keyword, resolved by the platform.
    Generic(GenericFamily),
}

/// The last resort behind every stack: the browser's standard font.
///
/// CSS says a list with nothing in it that matches falls back to the font the
/// browser would have used anyway, and every browser makes that a serif.
const STANDARD: GenericFamily = GenericFamily::Serif;

/// An ordered list of families, tried left to right.
///
/// This is the value of CSS `font-family`. It is always non-empty: a stack with no
/// usable entry would leave the shaper with nothing to fall back to, so
/// [`FontStack::new`] appends the standard font when the caller supplies no generic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontStack {
    families: Vec<Family>,
}

impl FontStack {
    /// Build a stack, appending the standard font if `families` contains no generic.
    pub fn new(families: impl IntoIterator<Item = Family>) -> Self {
        let mut families: Vec<Family> = families.into_iter().collect();
        let has_generic = families
            .iter()
            .any(|family| matches!(family, Family::Generic(_)));
        if !has_generic {
            families.push(Family::Generic(STANDARD));
        }
        Self { families }
    }

    /// A stack of one named family, plus the implied fallback behind it.
    pub fn named(name: impl Into<String>) -> Self {
        Self::new([Family::Named(name.into())])
    }

    /// A stack of one generic family.
    pub fn generic(generic: GenericFamily) -> Self {
        Self::new([Family::Generic(generic)])
    }

    /// Parse a CSS `font-family` list: comma-separated, optionally quoted.
    ///
    /// This is not the full CSS grammar — it does not handle escapes or unquoted
    /// multi-identifier names with unusual whitespace. It handles what a stylesheet
    /// realistically contains, and the real parser arrives with the CSS crate.
    pub fn parse_css(source: &str) -> Self {
        let families = source.split(',').filter_map(|entry| {
            let entry = entry.trim();
            let unquoted = entry
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .or_else(|| {
                    entry
                        .strip_prefix('\'')
                        .and_then(|rest| rest.strip_suffix('\''))
                });

            match unquoted {
                // A quoted name is always a family name, even if it spells a
                // generic keyword. That is what CSS says.
                Some(name) if !name.is_empty() => Some(Family::Named(name.to_owned())),
                Some(_) => None,
                None if entry.is_empty() => None,
                None => Some(match GenericFamily::parse(entry) {
                    Some(generic) => Family::Generic(generic),
                    None => Family::Named(entry.to_owned()),
                }),
            }
        });
        Self::new(families)
    }

    /// The families, in priority order.
    pub fn families(&self) -> &[Family] {
        &self.families
    }

    pub(crate) fn to_parley(&self) -> parley::FontFamily<'static> {
        let names = self
            .families
            .iter()
            .map(|family| match family {
                Family::Named(name) => parley::FontFamilyName::Named(name.clone().into()),
                Family::Generic(generic) => parley::FontFamilyName::Generic(generic.to_parley()),
            })
            .collect::<Vec<_>>();
        parley::FontFamily::List(names.into())
    }
}

impl Default for FontStack {
    fn default() -> Self {
        Self::generic(STANDARD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_always_ends_in_a_generic() {
        let stack = FontStack::named("Helvetica");
        assert_eq!(
            stack.families(),
            [
                Family::Named("Helvetica".to_owned()),
                Family::Generic(GenericFamily::Serif)
            ]
        );
    }

    #[test]
    fn an_explicit_generic_is_not_duplicated() {
        let stack = FontStack::new([Family::Generic(GenericFamily::Monospace)]);
        assert_eq!(stack.families().len(), 1);
    }

    #[test]
    fn css_lists_parse_in_order() {
        let stack = FontStack::parse_css("  Inter , 'Noto Sans' , monospace ");
        assert_eq!(
            stack.families(),
            [
                Family::Named("Inter".to_owned()),
                Family::Named("Noto Sans".to_owned()),
                Family::Generic(GenericFamily::Monospace),
            ]
        );
    }

    /// A quoted keyword is a family name, not a generic. CSS is explicit about this
    /// and getting it wrong silently changes which font a page gets.
    #[test]
    fn quoting_a_generic_keyword_makes_it_a_name() {
        let stack = FontStack::parse_css("\"monospace\"");
        assert_eq!(
            stack.families(),
            [
                Family::Named("monospace".to_owned()),
                Family::Generic(GenericFamily::Serif),
            ]
        );
    }

    #[test]
    fn generic_keywords_are_case_insensitive() {
        assert_eq!(
            GenericFamily::parse("SANS-SERIF"),
            Some(GenericFamily::SansSerif)
        );
        assert_eq!(
            GenericFamily::parse("Monospace"),
            Some(GenericFamily::Monospace)
        );
        assert_eq!(GenericFamily::parse("not-a-generic"), None);
    }

    #[test]
    fn an_empty_list_still_yields_a_usable_stack() {
        let stack = FontStack::parse_css("");
        assert_eq!(stack.families(), [Family::Generic(GenericFamily::Serif)]);
    }
}
