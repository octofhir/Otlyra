//! Which of the pictures an `<img>` offers is the one to fetch.
//!
//! A modern `<img>` names several files and lets the browser choose: `srcset`
//! lists them with how wide each is or what density it is for, `sizes` says how
//! wide the picture will be drawn, and a wrapping `<picture>` puts whole
//! alternatives in front of it behind a media query or a format. The choice is
//! made once, here, before anything is fetched — which is why it takes a viewport
//! and not a layout: at the moment a picture is asked for, nothing has been laid
//! out yet, and the width `sizes` describes is the author's promise about what
//! layout will do.
//!
//! What comes out is an address and a *density*, and the density is not a
//! decoration: a file chosen at two device pixels per CSS pixel is drawn at half
//! its own width, so the picture the page reserves room for is the file's size
//! divided by it.

use otlyra_css::cascade::Viewport;
use otlyra_dom::{Document, NodeId};

/// The picture an element settled on.
#[derive(Clone, Debug, PartialEq)]
pub struct Chosen {
    /// The address, exactly as the attribute spells it.
    pub url: String,
    /// How many of the file's own pixels go to one CSS pixel.
    pub density: f32,
}

/// One entry of a `srcset`: an address and what it claims about itself.
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    url: String,
    /// The `w` descriptor: how wide the file is, in its own pixels.
    width: Option<f32>,
    /// The `x` descriptor. A candidate with neither descriptor is `1x`.
    density: Option<f32>,
}

/// Which picture `img` asks for, given what the window is.
///
/// `None` means the element names nothing that can be fetched — no `src`, no
/// candidate in any `srcset`, or a `<picture>` whose sources are all for formats
/// nothing here can decode.
pub fn chosen(document: &Document, img: NodeId, viewport: Viewport) -> Option<Chosen> {
    for source in sources_before(document, img) {
        // A `type` nothing can decode and a `media` that does not match are both
        // reasons to move on to the next source rather than to give up: the
        // alternatives are in order of preference, and the `<img>` at the end is
        // the one every browser can show.
        if let Some(kind) = attribute(document, source, "type")
            && !is_decodable_type(&kind)
        {
            continue;
        }
        if let Some(media) = attribute(document, source, "media")
            && !otlyra_css::cascade::media_condition_matches(&media, viewport)
        {
            continue;
        }

        let srcset = attribute(document, source, "srcset").unwrap_or_default();
        let sizes = attribute(document, source, "sizes");
        if let Some(chosen) = pick(
            &parse_srcset(&srcset),
            sizes.as_deref(),
            viewport,
            /* fallback = */ None,
        ) {
            return Some(chosen);
        }
    }

    let srcset = attribute(document, img, "srcset").unwrap_or_default();
    let sizes = attribute(document, img, "sizes");
    let src = attribute(document, img, "src").filter(|src| !src.is_empty());
    pick(&parse_srcset(&srcset), sizes.as_deref(), viewport, src)
}

/// The `<source>` elements a `<picture>` offers before its `<img>`.
///
/// Only before it: a `<source>` written after the `<img>` is never consulted,
/// which is the rule that makes the `<img>` the last resort rather than one
/// alternative among several.
fn sources_before(document: &Document, img: NodeId) -> Vec<NodeId> {
    let Some(parent) = document.get(img).and_then(|node| node.parent) else {
        return Vec::new();
    };
    let is_picture = document
        .get(parent)
        .and_then(|node| node.element())
        .is_some_and(|element| element.name.local.as_ref() == "picture");
    if !is_picture {
        return Vec::new();
    }

    document
        .children(parent)
        .take_while(|&child| child != img)
        .filter(|&child| {
            document
                .get(child)
                .and_then(|node| node.element())
                .is_some_and(|element| element.name.local.as_ref() == "source")
        })
        .collect()
}

/// One attribute of an element, trimmed, if it has it.
fn attribute(document: &Document, node: NodeId, name: &str) -> Option<String> {
    Some(
        document
            .get(node)?
            .element()?
            .attrs
            .iter()
            .find(|attr| attr.name.local.as_ref() == name)?
            .value
            .trim()
            .to_owned(),
    )
}

/// Whether a `type` names a picture format that can be decoded here.
///
/// A source is skipped by its type rather than tried and dropped, because a
/// format nobody can read is exactly what the alternatives after it are for.
fn is_decodable_type(kind: &str) -> bool {
    let kind = kind.split(';').next().unwrap_or_default().trim();
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "image/png"
            | "image/apng"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/svg+xml"
    )
}

/// Choose among `candidates`, with `fallback` standing in as a `1x` where the
/// rules say a bare `src` counts as one.
fn pick(
    candidates: &[Candidate],
    sizes: Option<&str>,
    viewport: Viewport,
    fallback: Option<String>,
) -> Option<Chosen> {
    let mut candidates = candidates.to_vec();

    // A `src` beside a `srcset` is another candidate, at one device pixel per CSS
    // pixel — but only where the list leaves room for one: a list written in
    // widths is describing every size the picture comes in, and a list that
    // already has a `1x` in it has said what to use at that density.
    if let Some(src) = fallback {
        let widths = candidates.iter().any(|candidate| candidate.width.is_some());
        let has_1x = candidates
            .iter()
            .any(|candidate| candidate.density.unwrap_or(1.0) == 1.0);
        if candidates.is_empty() || (!widths && !has_1x) {
            candidates.push(Candidate {
                url: src,
                width: None,
                density: Some(1.0),
            });
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // A width descriptor says how wide the *file* is; what turns that into a
    // density is how wide the picture will be drawn, which is what `sizes` is
    // for. Without one it is the whole viewport, which is the specification's
    // default and is why a `srcset` written in widths and no `sizes` picks the
    // largest file on a wide window.
    let source_size = resolve_sizes(sizes, viewport);
    let density_of = |candidate: &Candidate| match (candidate.width, candidate.density) {
        (Some(width), _) if source_size > 0.0 => width / source_size,
        (_, Some(density)) => density,
        _ => 1.0,
    };

    let wanted = viewport.scale.max(0.0);
    // The smallest file that is at least as dense as the screen; failing that,
    // the densest there is. Never the one *below* what the screen can show,
    // which would be a picture chosen to look worse than the page can afford.
    let best = candidates
        .iter()
        .filter(|candidate| density_of(candidate) >= wanted)
        .min_by(|a, b| density_of(a).total_cmp(&density_of(b)))
        .or_else(|| {
            candidates
                .iter()
                .max_by(|a, b| density_of(a).total_cmp(&density_of(b)))
        })?;

    Some(Chosen {
        url: best.url.clone(),
        density: density_of(best).max(f32::MIN_POSITIVE),
    })
}

/// Take a `srcset` apart into its candidates.
///
/// A comma separates candidates, and an address may contain one — so an address
/// runs to the first space, and a comma is a separator only where a descriptor
/// could have started. That is the specification's rule and not a shortcut: it is
/// the only reason `a,b.png 1x, c.png 2x` is two pictures rather than three.
fn parse_srcset(srcset: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let bytes: Vec<char> = srcset.chars().collect();
    let mut at = 0usize;

    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_whitespace() || bytes[at] == ',') {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }

        let start = at;
        while at < bytes.len() && !bytes[at].is_whitespace() {
            at += 1;
        }
        let raw: String = bytes[start..at].iter().collect();

        // An address ending in commas carries no descriptor: the commas are the
        // separator that would otherwise have come after one.
        let (url, descriptors) = match raw.strip_suffix(',') {
            Some(_) => (raw.trim_end_matches(',').to_owned(), String::new()),
            None => {
                let from = at;
                while at < bytes.len() && bytes[at] != ',' {
                    at += 1;
                }
                let descriptors: String = bytes[from..at].iter().collect();
                at = (at + 1).min(bytes.len());
                (raw, descriptors)
            }
        };

        if url.is_empty() {
            continue;
        }

        let mut candidate = Candidate {
            url,
            width: None,
            density: None,
        };
        for descriptor in descriptors.split_whitespace() {
            if let Some(width) = descriptor.strip_suffix('w')
                && let Ok(width) = width.parse::<f32>()
                && width > 0.0
            {
                candidate.width = Some(width);
            } else if let Some(density) = descriptor.strip_suffix('x')
                && let Ok(density) = density.parse::<f32>()
                && density > 0.0
            {
                candidate.density = Some(density);
            }
        }
        out.push(candidate);
    }

    out
}

/// How wide the picture will be drawn, as `sizes` describes it.
///
/// The entries are tried in order and the first whose condition matches wins; a
/// bare length is a condition that always matches, which is why it is written
/// last. Nothing matching, or no attribute at all, is the whole viewport.
fn resolve_sizes(sizes: Option<&str>, viewport: Viewport) -> f32 {
    let Some(sizes) = sizes.filter(|sizes| !sizes.trim().is_empty()) else {
        return viewport.width;
    };

    for entry in split_top_level(sizes, ',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // The length is the last component; everything before it is the media
        // condition, which may itself hold spaces and brackets.
        let (condition, length) = match entry.rfind(|c: char| c.is_whitespace()) {
            Some(at) if entry.starts_with('(') || entry.starts_with("not ") => entry.split_at(at),
            _ => ("", entry),
        };
        let Some(length) = length_in_pixels(length.trim(), viewport) else {
            continue;
        };
        if otlyra_css::cascade::media_condition_matches(condition.trim(), viewport) {
            return length.max(0.0);
        }
    }

    viewport.width
}

/// Split on `separator`, ignoring any inside brackets.
fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == separator && depth <= 0 => {
                out.push(&text[start..at]);
                start = at + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out
}

/// A CSS length in pixels, for the handful of units a `sizes` is written in.
///
/// Percentages are deliberately absent: a `sizes` describes a width with nothing
/// to be a percentage *of*, and the specification does not allow one.
fn length_in_pixels(text: &str, viewport: Viewport) -> Option<f32> {
    let text = text.trim();
    if let Some(inner) = text
        .strip_prefix("calc(")
        .or_else(|| text.strip_prefix("CALC("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return calc(inner, viewport);
    }

    let font_size = 16.0 * viewport.text_scale;
    let (number, unit) = split_unit(text)?;
    let value: f32 = number.parse().ok()?;
    Some(match unit.to_ascii_lowercase().as_str() {
        "px" | "" if value == 0.0 || unit.eq_ignore_ascii_case("px") => value,
        "em" | "rem" => value * font_size,
        "vw" => value * viewport.width / 100.0,
        "vh" => value * viewport.height / 100.0,
        "vmin" => value * viewport.width.min(viewport.height) / 100.0,
        "vmax" => value * viewport.width.max(viewport.height) / 100.0,
        "cm" => value * 96.0 / 2.54,
        "mm" => value * 96.0 / 25.4,
        "q" => value * 96.0 / 101.6,
        "in" => value * 96.0,
        "pt" => value * 96.0 / 72.0,
        "pc" => value * 16.0,
        _ => return None,
    })
}

/// Split `12.5px` into `12.5` and `px`.
fn split_unit(text: &str) -> Option<(&str, &str)> {
    let at = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(text.len());
    if at == 0 {
        return None;
    }
    Some((&text[..at], &text[at..]))
}

/// Evaluate the inside of a `calc()`.
///
/// Lengths add and subtract; a bare number only multiplies and divides, which is
/// what CSS allows and is enough for the `calc(100vw - 2rem)` that a `sizes` is
/// usually written with. Anything else — a percentage, a nested function that is
/// not another `calc()` — gives nothing, and the entry is skipped rather than
/// guessed at.
fn calc(text: &str, viewport: Viewport) -> Option<f32> {
    let mut total = 0.0;
    let mut sign = 1.0;
    let mut at = 0usize;
    let text = text.trim();

    while at < text.len() {
        let rest = text[at..].trim_start();
        at = text.len() - rest.len();
        if rest.is_empty() {
            break;
        }

        // One term: a length or a number, then any run of `*` and `/`.
        let end = rest.find(['+', '-']).map_or(rest.len(), |found| found);
        // A `-` inside a term belongs to the term only where it opens it, and a
        // `sizes` has no negative lengths, so the split is at the sign.
        let (term, next) = rest.split_at(end);
        total += sign * term_value(term.trim(), viewport)?;
        at += term.len();

        match next.chars().next() {
            Some('+') => sign = 1.0,
            Some('-') => sign = -1.0,
            _ => break,
        }
        at += 1;
    }

    Some(total)
}

/// One `calc()` term: lengths and numbers joined by `*` and `/`.
fn term_value(term: &str, viewport: Viewport) -> Option<f32> {
    let mut parts = term.split_inclusive(['*', '/']);
    let mut value: Option<f32> = None;
    let mut operator = '*';

    for part in parts.by_ref() {
        let next_operator = part.chars().last().filter(|c| *c == '*' || *c == '/');
        let piece = part.trim_end_matches(['*', '/']).trim();
        if piece.is_empty() {
            return None;
        }
        let number = length_in_pixels(piece, viewport).or_else(|| piece.parse::<f32>().ok())?;
        value = Some(match value {
            None => number,
            Some(so_far) if operator == '*' => so_far * number,
            Some(so_far) if number != 0.0 => so_far / number,
            Some(_) => return None,
        });
        if let Some(next) = next_operator {
            operator = next;
        }
    }

    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window every case below is measured against: eight hundred and twenty
    /// CSS pixels wide, one device pixel to each.
    fn window() -> Viewport {
        Viewport {
            width: 820.0,
            height: 900.0,
            ..Viewport::default()
        }
    }

    fn choose(html: &str, viewport: Viewport) -> Option<Chosen> {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let mut stack = vec![document.root()];
        while let Some(id) = stack.pop() {
            if document
                .get(id)
                .and_then(|node| node.element())
                .is_some_and(|element| element.name.local.as_ref() == "img")
            {
                return chosen(&document, id, viewport);
            }
            stack.extend(document.children(id).collect::<Vec<_>>().into_iter().rev());
        }
        None
    }

    #[test]
    fn a_density_list_is_read_at_the_screens_density() {
        let html = "<img srcset='one.png 1x, two.png 2x' src='fallback.png'>";
        let chosen = choose(html, window()).expect("a picture");
        assert_eq!(chosen.url, "one.png");
        assert_eq!(chosen.density, 1.0);

        let retina = Viewport {
            scale: 2.0,
            ..window()
        };
        assert_eq!(choose(html, retina).expect("a picture").url, "two.png");
    }

    /// A bare address is `1x`, and the `src` beside it is not a second one.
    #[test]
    fn a_bare_candidate_is_the_one_times_candidate() {
        let chosen = choose(
            "<img srcset='one.png, two.png 2x' src='fallback.png'>",
            window(),
        )
        .expect("a picture");
        assert_eq!(chosen.url, "one.png");
    }

    /// With nothing at this density in the list, the `src` is what is left.
    #[test]
    fn a_src_stands_in_where_the_list_has_no_one_times() {
        let chosen =
            choose("<img srcset='two.png 2x' src='fallback.png'>", window()).expect("a picture");
        assert_eq!(chosen.url, "fallback.png");
        assert_eq!(chosen.density, 1.0);
    }

    /// Widths are read against how wide the picture says it will be drawn.
    #[test]
    fn a_width_list_is_read_against_sizes() {
        let chosen = choose(
            "<img srcset='a.png 100w, b.png 200w, c.png 400w' sizes='150px'>",
            window(),
        )
        .expect("a picture");
        assert_eq!(chosen.url, "b.png");
        assert!((chosen.density - 200.0 / 150.0).abs() < 0.001);
    }

    /// Nothing dense enough: the densest there is, rather than a file chosen to
    /// look worse than the window can show.
    #[test]
    fn the_largest_file_wins_when_none_is_dense_enough() {
        let chosen = choose(
            "<img srcset='a.png 100w, b.png 200w, c.png 400w' \
             sizes='(max-width: 600px) 100vw, 50vw'>",
            window(),
        )
        .expect("a picture");
        assert_eq!(chosen.url, "c.png");
        assert!((chosen.density - 400.0 / 410.0).abs() < 0.001);
    }

    /// No `sizes` at all is the whole viewport, which is the specification's
    /// default rather than a guess.
    #[test]
    fn widths_with_no_sizes_are_read_against_the_viewport() {
        let chosen = choose("<img srcset='c.png 400w'>", window()).expect("a picture");
        assert_eq!(chosen.url, "c.png");
        assert!((chosen.density - 400.0 / 820.0).abs() < 0.001);
    }

    #[test]
    fn a_picture_takes_the_first_source_that_applies() {
        let html = "<picture>\
             <source media='(min-width: 700px)' srcset='wide.png'>\
             <source type='image/nonesuch' srcset='unreadable.png'>\
             <img src='fallback.png'></picture>";
        assert_eq!(choose(html, window()).expect("a picture").url, "wide.png");

        let narrow = Viewport {
            width: 500.0,
            ..window()
        };
        assert_eq!(
            choose(html, narrow).expect("a picture").url,
            "fallback.png",
            "the format nothing can read is passed over rather than chosen"
        );
    }

    /// The comma in an address is not the comma between candidates.
    #[test]
    fn an_address_may_contain_a_comma() {
        let parsed = parse_srcset("a,b.png 1x, c.png 2x");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].url, "a,b.png");
        assert_eq!(parsed[1].url, "c.png");

        // And a comma with no space after it is not a separator at all: the
        // whole run is one address, which is what a reference browser then tries
        // to fetch and fails on.
        let run = parse_srcset("a.png,b.png");
        assert_eq!(run.len(), 1);
        assert_eq!(run[0].url, "a.png,b.png");

        let separated = parse_srcset("a.png 1x,b.png 2x");
        assert_eq!(separated.len(), 2, "a descriptor ends the candidate");
    }

    #[test]
    fn sizes_reads_the_units_it_is_written_in() {
        let viewport = window();
        assert_eq!(resolve_sizes(Some("120px"), viewport), 120.0);
        assert_eq!(resolve_sizes(Some("50vw"), viewport), 410.0);
        assert_eq!(resolve_sizes(Some("2rem"), viewport), 32.0);
        assert_eq!(
            resolve_sizes(Some("calc(100vw - 2rem)"), viewport),
            820.0 - 32.0
        );
        assert_eq!(
            resolve_sizes(Some("(min-width: 2000px) 100px, 33px"), viewport),
            33.0,
            "the first condition that matches wins, and a bare length always does"
        );
        assert_eq!(
            resolve_sizes(Some("nonsense"), viewport),
            viewport.width,
            "an entry that says nothing readable leaves the viewport"
        );
    }
}
