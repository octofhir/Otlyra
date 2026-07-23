//! `appearance`, carried through a cascade that does not have it.
//!
//! The style engine ships `appearance` as a property of the other embedding, not
//! of ours: it is declared but not compiled in, so a stylesheet that says
//! `appearance: none` has that declaration dropped on the floor before anything
//! can read it. The property matters — it is how a page turns off the widget we
//! draw and styles the control itself — and every alternative to having it is
//! worse than carrying it.
//!
//! So it is carried as a custom property. A declaration's *name* is rewritten
//! before the sheet is parsed, and everything after that is the real cascade:
//! specificity, origin, `!important`, `revert`, shorthand-free inheritance rules,
//! all of it, because a custom property is a first-class thing in the engine and a
//! home-made side table would not be.
//!
//! The rewrite is over tokens rather than text. `appearance` inside a string, in
//! a comment, or in a `url()` is not a declaration, and a scanner that does not
//! know that would corrupt the sheet. The tokenizer knows, so it does the reading
//! and only the property name's own bytes are replaced.

use std::borrow::Cow;

use cssparser::{ParseError, Parser, ParserInput, Token};
use style_traits::ToCss as _;

/// The custom property `appearance` is carried as.
///
/// Registered in the user-agent sheet so that it does not inherit — a widget's
/// appearance is its own, and an `<option>` inside a restyled `<select>` should
/// not be told that it is restyled too.
pub const CARRIER: &str = "--otlyra-appearance";

/// What the cascade decided a widget's appearance is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    /// The widget is drawn by us: a checkbox is a box with a tick in it.
    ///
    /// The default for a control, because the user-agent sheet says so — the
    /// property's own initial value is `none`, which is what everything that is
    /// not a widget keeps.
    #[default]
    Auto,
    /// The widget is suppressed, and the element is drawn by the ordinary rules
    /// of CSS. Every decoration that is not caused by a style rule goes with it:
    /// the arrow on a drop-down, the tick in a checkbox, and a checkbox's size.
    None,
}

impl Appearance {
    /// What a keyword means.
    ///
    /// The compatibility keywords — `checkbox`, `textfield`, `menulist-button`
    /// and the rest — all mean `auto`. They exist because pages were written
    /// against a property that named every look a control could have, and the one
    /// thing they must not do is turn a widget off.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("none") {
            Self::None
        } else {
            Self::Auto
        }
    }

    /// Whether the widget is drawn by us.
    #[must_use]
    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Rewrite every `appearance` declaration in a stylesheet to the carrier.
///
/// Borrows the source back unchanged when there is nothing to rewrite, which is
/// almost every sheet.
#[must_use]
pub fn rewrite_stylesheet(source: &str) -> Cow<'_, str> {
    rewrite(source, false)
}

/// The same, for the contents of a `style` attribute.
///
/// An attribute is a declaration list with no braces around it, so what is inside
/// a block in a sheet is at the top level here.
#[must_use]
pub fn rewrite_declarations(source: &str) -> Cow<'_, str> {
    rewrite(source, true)
}

fn rewrite(source: &str, declarations: bool) -> Cow<'_, str> {
    if !mentions_appearance(source) {
        return Cow::Borrowed(source);
    }
    let mut edits = Vec::new();
    {
        let mut input = ParserInput::new(source);
        let mut parser = Parser::new(&mut input);
        scan(&mut parser, declarations, &mut edits);
    }
    if edits.is_empty() {
        return Cow::Borrowed(source);
    }
    edits.sort_unstable_by_key(|range| range.0);

    let mut out = String::with_capacity(source.len() + edits.len() * CARRIER.len());
    let mut cursor = 0;
    for (start, end) in edits {
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(CARRIER);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    Cow::Owned(out)
}

/// Whether the source contains the word at all.
///
/// A plain substring search over the bytes, so that a sheet that never mentions
/// the property is not tokenized to find that out. It is allowed to say yes to a
/// mention inside a comment; the scan that follows will say no.
fn mentions_appearance(source: &str) -> bool {
    source
        .as_bytes()
        .windows(APPEARANCE.len())
        .any(|window| window.eq_ignore_ascii_case(APPEARANCE))
}

const APPEARANCE: &[u8] = b"appearance";

/// Whether an identifier is one of the two spellings of the property.
fn is_appearance(name: &str) -> bool {
    name.eq_ignore_ascii_case("appearance") || name.eq_ignore_ascii_case("-webkit-appearance")
}

/// Collect the byte ranges of every `appearance` property name.
///
/// `inside` says whether a declaration could start here. At the top level of a
/// sheet it could not — what is there is a selector or an at-rule — and an ident
/// followed by a colon there is a pseudo-class, not a property.
fn scan(parser: &mut Parser<'_, '_>, inside: bool, edits: &mut Vec<(usize, usize)>) {
    let mut pending: Option<(usize, usize)> = None;
    loop {
        let start = parser.position().byte_index();
        let Ok(token) = parser.next_including_whitespace_and_comments().cloned() else {
            break;
        };
        match token {
            Token::WhiteSpace(_) | Token::Comment(_) => {}
            Token::Colon => {
                if let Some(range) = pending.take() {
                    edits.push(range);
                }
            }
            Token::Ident(ref name) if inside && is_appearance(name) => {
                pending = Some((start, parser.position().byte_index()));
            }
            Token::CurlyBracketBlock
            | Token::ParenthesisBlock
            | Token::SquareBracketBlock
            | Token::Function(_) => {
                pending = None;
                // A declaration can begin inside a `{}` and nowhere else. The
                // other three are entered so that a stray brace inside them does
                // not confuse the count, and left as they were found.
                let curly = matches!(token, Token::CurlyBracketBlock);
                let _ = parser.parse_nested_block(|nested| {
                    scan(nested, inside || curly, edits);
                    Ok::<(), ParseError<'_, ()>>(())
                });
            }
            _ => pending = None,
        }
    }
}

/// What the cascade decided this element's appearance is.
///
/// The carrier is looked for in both halves of the custom-property store: it is
/// registered as not inheriting, and a sheet that has not been through the
/// registration would put it in the other half.
#[must_use]
pub fn of(style: &style::properties::ComputedValues) -> Appearance {
    let name = style::custom_properties::Name::from(&CARRIER[2..]);
    let properties = style.custom_properties();
    let value = properties
        .non_inherited
        .get(&name)
        .or_else(|| properties.inherited.get(&name));
    let Some(value) = value else {
        return Appearance::None;
    };
    let Some(universal) = value.as_universal() else {
        return Appearance::Auto;
    };
    Appearance::parse(&universal.to_css_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sheet_that_never_says_the_word_is_handed_back_unchanged() {
        let source = "p { color: red } a:hover { color: blue }";
        assert!(matches!(rewrite_stylesheet(source), Cow::Borrowed(_)));
    }

    #[test]
    fn a_declaration_is_rewritten_and_its_value_is_not() {
        assert_eq!(
            rewrite_stylesheet("input { appearance: none; color: red }"),
            "input { --otlyra-appearance: none; color: red }"
        );
        assert_eq!(
            rewrite_stylesheet("input { -webkit-appearance: checkbox }"),
            "input { --otlyra-appearance: checkbox }"
        );
    }

    #[test]
    fn the_word_inside_a_string_a_comment_or_a_url_is_left_alone() {
        for source in [
            "p { content: \"appearance: none\" }",
            "/* appearance: none */ p { color: red }",
            "p { background: url(appearance:none) }",
        ] {
            assert_eq!(rewrite_stylesheet(source), source);
        }
    }

    #[test]
    fn a_selector_is_not_a_declaration() {
        // An element named `appearance` is not a property, and neither is what
        // follows the colon after it.
        assert_eq!(
            rewrite_stylesheet("appearance:hover { color: red }"),
            "appearance:hover { color: red }"
        );
    }

    #[test]
    fn a_style_attribute_is_a_declaration_list_with_no_braces() {
        assert_eq!(
            rewrite_declarations("appearance: none; color: red"),
            "--otlyra-appearance: none; color: red"
        );
    }

    #[test]
    fn a_declaration_inside_an_at_rule_is_still_a_declaration() {
        assert_eq!(
            rewrite_stylesheet("@media screen { input { appearance: none } }"),
            "@media screen { input { --otlyra-appearance: none } }"
        );
    }

    #[test]
    fn several_declarations_are_all_rewritten() {
        assert_eq!(
            rewrite_stylesheet(
                "input { appearance: none } select { -webkit-appearance: none; appearance: none }"
            ),
            "input { --otlyra-appearance: none } \
             select { --otlyra-appearance: none; --otlyra-appearance: none }"
        );
    }

    /// What the cascade says the element with the given id looks like.
    fn cascaded(html: &str, id: &str) -> Appearance {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styled = crate::cascade::style_document(&document, crate::cascade::Viewport::default());
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            if document
                .get(node)
                .and_then(|node| node.element())
                .and_then(|element| element.id())
                == Some(id)
            {
                return of(styled.style_of(node).expect("the element was styled"));
            }
            stack.extend(document.children(node));
        }
        panic!("no element with id {id}");
    }

    #[test]
    fn a_control_is_a_widget_and_everything_else_is_not() {
        let html = "<input id=field><div id=plain></div><button id=press>x</button>";
        assert_eq!(cascaded(html, "field"), Appearance::Auto);
        assert_eq!(cascaded(html, "press"), Appearance::Auto);
        assert_eq!(cascaded(html, "plain"), Appearance::None);
    }

    #[test]
    fn a_page_can_turn_a_widget_off_and_back_on() {
        assert_eq!(
            cascaded(
                "<style>input { appearance: none }</style><input id=field>",
                "field"
            ),
            Appearance::None
        );
        assert_eq!(
            cascaded(
                "<style>input { -webkit-appearance: none }</style><input id=field>",
                "field"
            ),
            Appearance::None
        );
        // The compatibility keywords turn nothing off.
        assert_eq!(
            cascaded(
                "<style>input { appearance: none }                  input[type=checkbox] { appearance: checkbox }</style>                 <input id=box type=checkbox>",
                "box"
            ),
            Appearance::Auto
        );
    }

    #[test]
    fn a_style_attribute_carries_it_too() {
        assert_eq!(
            cascaded("<input id=field style=\"appearance:none\">", "field"),
            Appearance::None
        );
    }

    #[test]
    fn it_does_not_reach_the_children_of_the_control_it_is_on() {
        assert_eq!(
            cascaded(
                "<style>select { appearance: none }</style>                 <select id=menu><option id=first>a</option></select>",
                "menu"
            ),
            Appearance::None
        );
        // An option inside a restyled select is not itself restyled.
        assert_eq!(
            cascaded(
                "<style>select { appearance: none }</style>                 <select id=menu><option id=first>a</option></select>",
                "first"
            ),
            Appearance::None
        );
    }

    #[test]
    fn a_keyword_that_is_not_none_leaves_the_widget_alone() {
        assert_eq!(Appearance::parse("none"), Appearance::None);
        assert_eq!(Appearance::parse("NONE"), Appearance::None);
        assert_eq!(Appearance::parse("auto"), Appearance::Auto);
        assert_eq!(Appearance::parse("checkbox"), Appearance::Auto);
        assert_eq!(Appearance::parse("menulist-button"), Appearance::Auto);
    }
}
