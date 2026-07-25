//! Pushing an SVG's own stylesheet onto the elements it styles.
//!
//! Skia's SVG parser reads presentation attributes and nothing else — there is
//! no `<style>` in its node set at all — so a file that colours itself with
//! `.st0{fill:#11A14E}` and `class="st0"` draws every shape in the default
//! black. That is exactly what a drawing program exports, which makes it most of
//! the logos on the web.
//!
//! So the rules are written onto the elements they select, as attributes, before
//! the file is handed over. It is a narrow thing on purpose: the selectors a
//! drawing program emits — an element name, a class, an id, and lists of those —
//! and the properties that are presentation attributes anyway. A rule with a
//! combinator, a pseudo-class or an at-rule in it is skipped, and a file this
//! cannot make sense of is handed on exactly as it arrived. The alternative is a
//! CSS engine inside the picture decoder, and the pictures that would need one
//! are not the ones that are broken.
//!
//! Priority is CSS's, as far as it goes: an id beats a class beats an element
//! name, later beats earlier at the same weight, and all of them beat a
//! presentation attribute already on the element — which is why an attribute
//! this writes replaces one that was there.

/// What a rule selects.
#[derive(Debug, PartialEq, Eq)]
enum Selector {
    /// `.name`
    Class(String),
    /// `#name`
    Id(String),
    /// `name`
    Tag(String),
    /// `*`
    Any,
}

impl Selector {
    /// How much it outweighs another, in CSS's own order.
    fn weight(&self) -> u8 {
        match self {
            Self::Id(_) => 3,
            Self::Class(_) => 2,
            Self::Tag(_) => 1,
            Self::Any => 0,
        }
    }

    /// Whether it picks out an element with this name, class list and id.
    fn matches(&self, tag: &str, classes: &str, id: Option<&str>) -> bool {
        match self {
            Self::Any => true,
            Self::Tag(name) => name.eq_ignore_ascii_case(tag),
            Self::Id(name) => id == Some(name.as_str()),
            Self::Class(name) => classes.split_ascii_whitespace().any(|one| one == name),
        }
    }

    /// Read one, or nothing where it is a shape this does not handle.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        // A combinator, a pseudo-class, an attribute selector or a compound of
        // several parts. Every one of them is a rule this would apply too widely
        // or too narrowly, and applying it wrongly is worse than not at all.
        if text.is_empty()
            || text.contains([' ', '>', '+', '~', ':', '[', '(', '\t', '\n'])
            || text[1..].contains(['.', '#'])
        {
            return None;
        }
        Some(match text.split_at(1) {
            (".", name) => Self::Class(name.to_owned()),
            ("#", name) => Self::Id(name.to_owned()),
            ("*", "") => Self::Any,
            _ => Self::Tag(text.to_owned()),
        })
    }
}

/// The properties that are presentation attributes as well, so that writing one
/// onto an element says the same thing the rule said.
///
/// A list rather than everything the stylesheet holds: `enable-background` and
/// friends are not attributes Skia reads, and an attribute nothing reads is
/// noise in a file that is about to be parsed.
const CARRIED: &[&str] = &[
    "clip-path",
    "clip-rule",
    "color",
    "display",
    "fill",
    "fill-opacity",
    "fill-rule",
    "filter",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "mask",
    "opacity",
    "stop-color",
    "stop-opacity",
    "stroke",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
    "stroke-width",
    "text-anchor",
    "visibility",
];

/// One rule: what it picks out, what it sets, and where it was written.
struct Rule {
    selector: Selector,
    declarations: Vec<(String, String)>,
    order: usize,
}

/// Read every `<style>` in the document, in the order they appear.
fn stylesheet(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("<style") {
        let after = &rest[at + "<style".len()..];
        let Some(open) = after.find('>') else { break };
        let body = &after[open + 1..];
        let Some(close) = body.find("</style") else {
            break;
        };
        out.push_str(&body[..close]);
        out.push('\n');
        rest = &body[close..];
    }
    // A comment is not a rule, and one holding a brace would end a block early.
    strip_comments(&out)
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(at) = rest.find("/*") {
        out.push_str(&rest[..at]);
        match rest[at + 2..].find("*/") {
            Some(end) => rest = &rest[at + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The rules a stylesheet holds, in the order they were written.
fn rules(css: &str) -> Vec<Rule> {
    let mut out = Vec::new();
    let mut rest = css;
    let mut order = 0;
    while let Some(open) = rest.find('{') {
        let (heads, after) = rest.split_at(open);
        // Whatever block ended before this one is not part of this one's
        // selectors: a list is read from the last `}` onwards.
        let heads = heads.rsplit('}').next().unwrap_or(heads);

        // An at-rule holds a block of blocks, so its end is not the first `}`.
        // Skipping the whole of it leaves what it held unstyled, which is what
        // was true before any of this ran.
        if heads.contains('@') {
            rest = &after[skip_block(after)..];
            continue;
        }

        let Some(close) = after.find('}') else { break };
        let body = &after[1..close];
        rest = &after[close + 1..];

        for head in heads.split(',') {
            let Some(selector) = Selector::parse(head) else {
                continue;
            };
            let declarations = declarations(body);
            if !declarations.is_empty() {
                out.push(Rule {
                    selector,
                    declarations,
                    order,
                });
                order += 1;
            }
        }
    }
    out
}

/// How far past a `{` its matching `}` is, counting the blocks inside it.
fn skip_block(from: &str) -> usize {
    let mut depth = 0usize;
    for (at, character) in from.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
    }
    from.len()
}

/// The declarations of one block, keeping only what an attribute can say.
fn declarations(body: &str) -> Vec<(String, String)> {
    body.split(';')
        .filter_map(|one| {
            let (property, value) = one.split_once(':')?;
            let property = property.trim().to_ascii_lowercase();
            let value = value.trim();
            // A value with a quote in it cannot be written as an attribute
            // without deciding how to escape it, and none of these need to be.
            if value.is_empty() || value.contains(['"', '<', '>', '&']) {
                return None;
            }
            CARRIED
                .contains(&property.as_str())
                .then(|| (property, value.to_owned()))
        })
        .collect()
}

/// The value of an attribute in a start tag, if it has one.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().next_back();
        let after = &rest[at + name.len()..];
        let separated = before.is_none_or(|c| c.is_whitespace());
        let value = after.trim_start();
        if separated && let Some(value) = value.strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let value = &value[1..];
                return value.find(quote).map(|end| &value[..end]);
            }
        }
        rest = after;
    }
    None
}

/// Rewrite `bytes` with its stylesheet applied to its elements.
///
/// `None` when there is nothing to do or nothing this understands — the caller
/// then uses what it already has, which is what happened before this existed.
pub fn inline_styles(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("<style") {
        return None;
    }
    let rules = rules(&stylesheet(text));
    if rules.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(text.len() + 256);
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        // The stylesheet has been applied, so it goes: what is left behind is
        // dead weight in a file about to be parsed again, and a reader of the
        // result cannot tell whether a rule was honoured or merely present.
        if rest[at..].starts_with("<style") {
            out.push_str(&rest[..at]);
            let after = &rest[at..];
            match after
                .find("</style")
                .and_then(|end| after[end..].find('>').map(|close| end + close + 1))
            {
                Some(end) => {
                    rest = &after[end..];
                    continue;
                }
                None => break,
            }
        }
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        // A closing tag, a comment, a declaration or a processing instruction:
        // none of them is an element to style, and none may be rewritten.
        let name_at = &after[1..];
        if !name_at
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        {
            let Some(end) = after.find('>') else { break };
            out.push_str(&after[..=end]);
            rest = &after[end + 1..];
            continue;
        }
        let Some(end) = after.find('>') else { break };
        let tag = &after[..=end];
        out.push_str(&styled(tag, &rules));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out.into_bytes())
}

/// One start tag, with whatever the stylesheet says about it written onto it.
fn styled(tag: &str, rules: &[Rule]) -> String {
    // `<name` and then the attributes; the end is `>` or `/>`.
    let inner = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end()
        .trim_end_matches('/');
    let name = inner
        .split([' ', '\t', '\n', '\r'])
        .next()
        .unwrap_or_default();
    let classes = attribute(inner, "class").unwrap_or_default();
    let id = attribute(inner, "id");
    if classes.is_empty() && id.is_none() && !rules.iter().any(|rule| rule.selector.weight() <= 1) {
        return tag.to_owned();
    }

    // What wins, property by property: the heaviest selector, and the last of
    // those written.
    let mut winning: Vec<(&str, &str, u8, usize)> = Vec::new();
    for rule in rules {
        if !rule.selector.matches(name, classes, id) {
            continue;
        }
        for (property, value) in &rule.declarations {
            let weight = rule.selector.weight();
            match winning
                .iter_mut()
                .find(|(held, _, _, _)| *held == property.as_str())
            {
                Some(held) if held.2 <= weight && held.3 <= rule.order => {
                    *held = (property, value, weight, rule.order);
                }
                Some(_) => {}
                None => winning.push((property, value, weight, rule.order)),
            }
        }
    }
    if winning.is_empty() {
        return tag.to_owned();
    }

    // The attributes the element already carries, minus the ones a rule
    // replaces: a rule outweighs a presentation attribute, whatever it says.
    let mut kept = String::new();
    let mut rest = &inner[name.len()..];
    while let Some(equals) = rest.find('=') {
        let property = rest[..equals]
            .trim()
            .rsplit([' ', '\t', '\n', '\r'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let value = rest[equals + 1..].trim_start();
        let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let Some(end) = value[1..].find(quote) else {
            break;
        };
        let held = &value[1..1 + end];
        if !winning.iter().any(|(name, ..)| *name == property) {
            kept.push(' ');
            kept.push_str(&property);
            kept.push_str("=\"");
            kept.push_str(held);
            kept.push('"');
        }
        rest = &value[1 + end + 1..];
    }

    let mut out = String::with_capacity(tag.len() + winning.len() * 24);
    out.push('<');
    out.push_str(name);
    out.push_str(&kept);
    for (property, value, _, _) in &winning {
        out.push(' ');
        out.push_str(property);
        out.push_str("=\"");
        out.push_str(value);
        out.push('"');
    }
    if tag.trim_end_matches('>').trim_end().ends_with('/') {
        out.push_str("/>");
    } else {
        out.push('>');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a drawing program exports: a stylesheet of classes, and elements
    /// that name one.
    #[test]
    fn a_class_rule_is_written_onto_what_it_selects() {
        let svg = br##"<svg viewBox="0 0 10 10">
            <style type="text/css">.st0{fill:#11A14E;}.st1{fill:#0673BA;}</style>
            <rect class="st0" width="5" height="10"/>
            <rect class="st1" x="5" width="5" height="10"/></svg>"##;
        let out = inline_styles(svg).expect("a file with a stylesheet is rewritten");
        let out = String::from_utf8(out).expect("still text");

        assert!(out.contains(r##"fill="#11A14E""##), "{out}");
        assert!(out.contains(r##"fill="#0673BA""##), "{out}");
        // And nothing it was already carrying is lost.
        assert!(
            out.contains(r#"width="5""#) && out.contains(r#"x="5""#),
            "{out}"
        );
    }

    /// A rule outweighs a presentation attribute, so it replaces one.
    #[test]
    fn a_rule_replaces_the_attribute_it_disagrees_with() {
        let svg = br##"<svg><style>.a{fill:#00ff00}</style>
            <rect class="a" fill="#ff0000" width="5"/></svg>"##;
        let out = String::from_utf8(inline_styles(svg).expect("rewritten")).expect("text");
        assert!(out.contains(r##"fill="#00ff00""##), "{out}");
        assert!(
            !out.contains("#ff0000"),
            "the losing attribute stayed: {out}"
        );
    }

    /// An id beats a class, and a later rule beats an earlier one of the same
    /// weight — which is the whole of the cascade a picture needs.
    #[test]
    fn the_heavier_and_the_later_rule_wins() {
        let svg = br##"<svg><style>
            .a{fill:#111111}
            .a{fill:#222222}
            #b{fill:#333333}
            </style><rect class="a" id="b"/><rect class="a"/></svg>"##;
        let out = String::from_utf8(inline_styles(svg).expect("rewritten")).expect("text");
        let first = out.find("#333333").expect("the id rule won somewhere");
        let second = out.find("#222222").expect("the later class rule won too");
        assert!(first < second, "each element took its own answer: {out}");
        assert!(!out.contains("#111111"), "the earlier rule won: {out}");
    }

    /// A selector this does not understand is left alone rather than guessed at.
    #[test]
    fn a_selector_with_a_combinator_is_skipped() {
        let svg = br##"<svg><style>
            g .a{fill:#111111}
            .a:hover{fill:#222222}
            @media print{.a{fill:#333333}}
            .b{fill:#444444}
            </style><rect class="a b"/></svg>"##;
        let out = String::from_utf8(inline_styles(svg).expect("rewritten")).expect("text");
        assert!(out.contains(r##"fill="#444444""##), "{out}");
        for skipped in ["#111111", "#222222", "#333333"] {
            assert!(!out.contains(skipped), "{skipped} was applied: {out}");
        }
    }

    /// A file with nothing to do is not touched, so nothing it holds can be
    /// mangled on the way through.
    #[test]
    fn a_file_without_a_stylesheet_is_left_exactly_as_it_is() {
        assert!(inline_styles(br##"<svg><rect fill="#ff0000"/></svg>"##).is_none());
        // A stylesheet holding nothing this can carry is the same answer.
        assert!(inline_styles(b"<svg><style>.a{enable-background:new}</style></svg>").is_none());
        // And something that is not text at all.
        assert!(inline_styles(&[0xff, 0xfe, 0x00]).is_none());
    }

    /// Comments hold braces, and a brace read as the end of a block would take
    /// the rest of the stylesheet with it.
    #[test]
    fn a_comment_is_not_a_rule() {
        let svg = br##"<svg><style>/* a { fill:#ff0000 } */ .a{fill:#00ff00}</style>
            <rect class="a"/></svg>"##;
        let out = String::from_utf8(inline_styles(svg).expect("rewritten")).expect("text");
        assert!(out.contains(r##"fill="#00ff00""##), "{out}");
        assert!(!out.contains("#ff0000"), "{out}");
    }
}
