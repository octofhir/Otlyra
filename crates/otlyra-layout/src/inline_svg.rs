//! An `<svg>` written into the page, drawn as the picture it describes.
//!
//! CSS lays an outermost `svg` out as a replaced element: a box of its own size
//! whose content is not boxes but a drawing (CSS 2 §10.3.2, SVG 2 §5.1). So it
//! is drawn the way an SVG file named by an `<img>` is: its markup goes to the
//! same rasterizer, and the pixels come back as the box's picture. What makes
//! it more than a file is that it is part of the page — `currentColor` is the
//! colour the page's rules gave the element, and a `<use>` may name a symbol
//! defined in some other `svg` further up the document, which is how every icon
//! sprite on the web works.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use html5ever::ns;
use otlyra_dom::{Document, NodeData, NodeId};
use otlyra_gfx::peniko::{Color, ImageData};

use crate::box_tree::Replaced;

/// How many device pixels each CSS pixel of the drawing gets. Two, so an icon is
/// sharp on the displays most people read on; a denser screen magnifies it a
/// little, and a plain one draws it down, which costs nothing it can see.
const DENSITY: f32 = 2.0;

/// How many drawings are kept. A page draws the same icon in many places and
/// draws all of them again on every restyle; a drawing is only made once per
/// markup and colour.
const CACHED: usize = 1024;

/// How deep a `<use>` may reach through the symbols it names, so a symbol that
/// uses itself is a picture and not a loop.
const USE_DEPTH: usize = 8;

/// A drawing and the size its markup declares, or nothing when it did not draw.
type Drawn = Option<(ImageData, (f32, f32))>;

thread_local! {
    static DRAWN: RefCell<HashMap<u64, Drawn>> = RefCell::new(HashMap::new());
}

/// The outermost `svg` at `node`, drawn in `color`, as replaced content.
///
/// `None` when the markup does not draw: the box is then generated anyway, at
/// whatever size its style gives it, the way a picture that failed is.
pub(crate) fn replaced(document: &Document, node: NodeId, color: Color) -> Replaced {
    let markup = markup(document, node, color);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    markup.hash(&mut hasher);
    let key = hasher.finish();

    let drawn = DRAWN.with(|cache| {
        if let Some(drawn) = cache.borrow().get(&key) {
            return drawn.clone();
        }
        let drawn = otlyra_gfx::draw_svg(markup.as_bytes(), DENSITY)
            .inspect_err(|error| tracing::debug!(%error, "an inline svg did not draw"))
            .ok();
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHED {
            cache.clear();
        }
        cache.insert(key, drawn.clone());
        drawn
    });
    match drawn {
        Some((image, size)) => Replaced {
            image: Some(image),
            intrinsic: Some(size),
        },
        None => Replaced::EMPTY,
    }
}

/// The markup of the `svg` at `node` as a document of its own.
fn markup(document: &Document, node: NodeId, color: Color) -> String {
    let mut writer = Writer {
        color,
        out: String::new(),
        ids: HashSet::new(),
        uses: Vec::new(),
    };
    writer.write(document, node, true, None);
    let mut out = std::mem::take(&mut writer.out);

    // Symbols this drawing names but does not hold, copied in as definitions.
    // A sprite sheet is an `svg` of `<symbol>`s hidden somewhere in the page,
    // and each icon is an `svg` holding nothing but a `<use>` of one of them.
    let mut depth = 0;
    while !writer.uses.is_empty() && depth < USE_DEPTH {
        depth += 1;
        let wanted: Vec<String> = std::mem::take(&mut writer.uses)
            .into_iter()
            .filter(|id| !writer.ids.contains(id))
            .collect();
        for id in wanted {
            if writer.ids.contains(&id) {
                continue;
            }
            let Some(target) = element_by_id(document, &id) else {
                continue;
            };
            writer.write(document, target, false, None);
        }
    }
    let defs = writer.out;
    if !defs.is_empty() {
        let close = out.rfind("</svg>").unwrap_or(out.len());
        out.insert_str(close, &format!("<defs>{defs}</defs>"));
    }
    out
}

/// Markup being written, with the ids it defines and the ones its `<use>`s
/// name.
struct Writer {
    color: Color,
    out: String,
    ids: HashSet<String>,
    uses: Vec<String>,
}

impl Writer {
    /// Write `node` and what is inside it. `viewport` is the size percentages
    /// of its lengths are of, when that is known.
    fn write(
        &mut self,
        document: &Document,
        node: NodeId,
        root: bool,
        viewport: Option<(f32, f32)>,
    ) {
        let Some(dom) = document.get(node) else {
            return;
        };
        match &dom.data {
            NodeData::Element(element) => {
                let name = &*element.name.local;
                self.out.push('<');
                self.out.push_str(name);
                if root {
                    self.out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                    self.out
                        .push_str(" xmlns:xlink=\"http://www.w3.org/1999/xlink\"");
                    // What `currentColor` is: the colour the page's rules gave the
                    // element, which is the page's, not the markup's.
                    let rgba = self.color.to_rgba8();
                    self.out.push_str(&format!(
                        " color=\"rgba({},{},{},{})\"",
                        rgba.r,
                        rgba.g,
                        rgba.b,
                        f32::from(rgba.a) / 255.0
                    ));
                }
                for attribute in &element.attrs {
                    let local = &*attribute.name.local;
                    if root && (local == "xmlns" || local == "color") {
                        continue;
                    }
                    if attribute.name.ns == ns!(xmlns) {
                        continue;
                    }
                    let prefix = if attribute.name.ns == ns!(xlink) {
                        "xlink:"
                    } else if attribute.name.ns == ns!(xml) {
                        "xml:"
                    } else {
                        ""
                    };
                    if local == "id" && prefix.is_empty() {
                        self.ids.insert(attribute.value.to_string());
                    }
                    if name == "use"
                        && local == "href"
                        && let Some(id) = attribute.value.strip_prefix('#')
                    {
                        self.uses.push(id.to_owned());
                    }
                    let worked_out = prefix
                        .is_empty()
                        .then(|| geometry(local, &attribute.value, viewport))
                        .flatten();
                    self.out.push(' ');
                    self.out.push_str(prefix);
                    self.out.push_str(local);
                    self.out.push_str("=\"");
                    escape(
                        worked_out.as_deref().unwrap_or(&attribute.value),
                        true,
                        &mut self.out,
                    );
                    self.out.push('"');
                }
                self.out.push('>');
                let inner = match name {
                    "svg" => own_viewport(element, viewport),
                    _ => viewport,
                };
                for child in document.children(node) {
                    self.write(document, child, false, inner);
                }
                self.out.push_str("</");
                self.out.push_str(name);
                self.out.push('>');
            }
            NodeData::Text(text) => escape(text, false, &mut self.out),
            _ => {}
        }
    }
}

/// A geometry attribute written as `calc()`, worked out to the number it is.
///
/// In SVG 2 an element's position and size are CSS properties, so `width` may
/// be `calc(100% - 2px)`: a rectangle a pixel inside its viewport, which is how
/// an outline is drawn. The rasterizer reads only plain lengths and skips the
/// element otherwise, so the arithmetic is done here, against the viewport the
/// percentage is of. `None` for anything else, which is written as it was.
fn geometry(attribute: &str, value: &str, viewport: Option<(f32, f32)>) -> Option<String> {
    let basis = match attribute {
        "x" | "width" | "cx" | "rx" | "x1" | "x2" => viewport.map(|(width, _)| width),
        "y" | "height" | "cy" | "ry" | "y1" | "y2" => viewport.map(|(_, height)| height),
        // A radius is of neither side, but of their root mean square (SVG 2 §8.9).
        "r" => viewport.map(|(width, height)| ((width * width + height * height) / 2.0).sqrt()),
        _ => return None,
    };
    let value = value.trim();
    if !value.get(..5)?.eq_ignore_ascii_case("calc(") {
        return None;
    }
    let mut calc = Calc { rest: value, basis };
    let number = calc.term()?;
    (calc.rest.trim().is_empty() && number.is_finite()).then(|| number.to_string())
}

/// The size an `svg` gives what is inside it: its `viewBox`, or failing that
/// its own width and height. `None` when neither says, as for an outermost
/// `svg` sized by the page's rules.
fn own_viewport(
    element: &otlyra_dom::ElementData,
    outer: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    let attr = |name: &str| {
        element
            .attrs
            .iter()
            .find(|attribute| attribute.name.ns == ns!() && &*attribute.name.local == name)
            .map(|attribute| attribute.value.trim().to_owned())
    };
    if let Some(view_box) = attr("viewBox") {
        let numbers: Vec<f32> = view_box
            .split(|c: char| c.is_ascii_whitespace() || c == ',')
            .filter(|part| !part.is_empty())
            .map_while(|part| part.parse().ok())
            .collect();
        if let [_, _, width, height] = numbers[..]
            && width > 0.0
            && height > 0.0
        {
            return Some((width, height));
        }
    }
    let side = |name: &str, of: Option<f32>| -> Option<f32> {
        let value = attr(name)?;
        match value.strip_suffix('%') {
            Some(percent) => Some(percent.trim().parse::<f32>().ok()? / 100.0 * of?),
            None => value
                .strip_suffix("px")
                .unwrap_or(&value)
                .trim()
                .parse()
                .ok(),
        }
    };
    Some((
        side("width", outer.map(|outer| outer.0))?,
        side("height", outer.map(|outer| outer.1))?,
    ))
}

/// `calc()` over pixels and percentages of `basis`, read from the front of
/// `rest`. Any other unit is a length this cannot know, and makes it `None`.
struct Calc<'a> {
    rest: &'a str,
    basis: Option<f32>,
}

impl Calc<'_> {
    fn sum(&mut self) -> Option<f32> {
        let mut value = self.product()?;
        loop {
            self.rest = self.rest.trim_start();
            if let Some(rest) = self.rest.strip_prefix('+') {
                self.rest = rest;
                value += self.product()?;
            } else if let Some(rest) = self.rest.strip_prefix('-') {
                self.rest = rest;
                value -= self.product()?;
            } else {
                return Some(value);
            }
        }
    }

    fn product(&mut self) -> Option<f32> {
        let mut value = self.term()?;
        loop {
            self.rest = self.rest.trim_start();
            if let Some(rest) = self.rest.strip_prefix('*') {
                self.rest = rest;
                value *= self.term()?;
            } else if let Some(rest) = self.rest.strip_prefix('/') {
                self.rest = rest;
                value /= self.term()?;
            } else {
                return Some(value);
            }
        }
    }

    fn term(&mut self) -> Option<f32> {
        self.rest = self.rest.trim_start();
        let open = if self
            .rest
            .get(..5)
            .is_some_and(|start| start.eq_ignore_ascii_case("calc("))
        {
            Some(5)
        } else {
            self.rest.starts_with('(').then_some(1)
        };
        if let Some(open) = open {
            self.rest = &self.rest[open..];
            let value = self.sum()?;
            self.rest = self.rest.trim_start().strip_prefix(')')?;
            return Some(value);
        }
        let end = self
            .rest
            .char_indices()
            .find(|&(at, c)| !(c.is_ascii_digit() || c == '.' || (at == 0 && c == '-')))
            .map_or(self.rest.len(), |(at, _)| at);
        let number: f32 = self.rest[..end].parse().ok()?;
        self.rest = &self.rest[end..];
        if let Some(rest) = self.rest.strip_prefix('%') {
            self.rest = rest;
            return Some(number / 100.0 * self.basis?);
        }
        if let Some(rest) = self.rest.strip_prefix("px") {
            self.rest = rest;
        }
        if self.rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return None;
        }
        Some(number)
    }
}

/// The element whose `id` is `id`, anywhere in the document.
fn element_by_id(document: &Document, id: &str) -> Option<NodeId> {
    let mut stack = vec![document.root()];
    while let Some(node) = stack.pop() {
        if document.attr(node, "id") == Some(id) {
            return Some(node);
        }
        let children: Vec<NodeId> = document.children(node).collect();
        stack.extend(children.into_iter().rev());
    }
    None
}

fn escape(text: &str, attribute: bool, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
}

/// Whether `node` is an `svg` that CSS lays out as a box of its own: one whose
/// parent is not an SVG element.
pub(crate) fn is_outermost(document: &Document, node: NodeId) -> bool {
    let parent = document
        .get(node)
        .and_then(|dom| dom.parent)
        .and_then(|parent| document.get(parent))
        .and_then(|parent| parent.element());
    parent.is_none_or(|parent| parent.name.ns != ns!(svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_svg(document: &Document) -> NodeId {
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            if document
                .get(node)
                .and_then(|dom| dom.element())
                .is_some_and(|element| element.name.ns == ns!(svg) && &*element.name.local == "svg")
            {
                return node;
            }
            let children: Vec<NodeId> = document.children(node).collect();
            stack.extend(children.into_iter().rev());
        }
        panic!("no svg in the document");
    }

    #[test]
    fn an_inline_svg_is_drawn_at_its_own_size_and_in_its_colour() {
        let document = otlyra_html::parse(
            b"<p><svg width=16 height=8 viewBox=\"0 0 2 1\"><rect width=2 height=1 fill=currentColor /></svg>",
            Some("utf-8"),
        )
        .document;
        let content = replaced(&document, first_svg(&document), Color::from_rgb8(255, 0, 0));
        assert_eq!(content.intrinsic, Some((16.0, 8.0)));
        let image = content.image.expect("the svg drew");
        assert_eq!(
            (image.width, image.height),
            (32, 16),
            "drawn at twice its size"
        );
        let pixel = &image.data.data()[..4];
        assert_eq!(
            pixel,
            [255, 0, 0, 255],
            "currentColor is the element's colour"
        );
    }

    /// An outline drawn a pixel inside its `svg`, the way pages draw one:
    /// with lengths the rasterizer cannot read on its own.
    #[test]
    fn a_calculated_length_is_worked_out_against_its_viewport() {
        let document = otlyra_html::parse(
            b"<p><svg width=10 height=10><rect x=1 y=1 width=\"calc(100% - 2px)\" \
              height=\"calc((100% - 2px) / 2)\" fill=#f00 /></svg>",
            Some("utf-8"),
        )
        .document;
        let image = replaced(&document, first_svg(&document), Color::BLACK)
            .image
            .expect("the svg drew");
        let pixel = |x: u32, y: u32| {
            let at = ((y * image.width + x) * 4) as usize;
            image.data.data()[at..at + 4].to_vec()
        };
        // Drawn at twice its size: the rectangle is 2..18 across, 2..10 down.
        assert_eq!(pixel(10, 5), [255, 0, 0, 255], "inside");
        assert_eq!(pixel(1, 5)[3], 0, "left of it");
        assert_eq!(pixel(18, 5)[3], 0, "right of it");
        assert_eq!(pixel(10, 11)[3], 0, "below it");
    }

    #[test]
    fn calc_takes_pixels_and_percentages_and_nothing_it_cannot_know() {
        let viewport = Some((200.0, 100.0));
        let worked_out = |attribute: &str, value: &str| geometry(attribute, value, viewport);
        assert_eq!(
            worked_out("width", "calc(100% - 2px)").as_deref(),
            Some("198")
        );
        assert_eq!(
            worked_out("y", "CALC(50% + 3 * 2px)").as_deref(),
            Some("56")
        );
        assert_eq!(
            worked_out("x", "calc(-10px + calc(10%))").as_deref(),
            Some("10")
        );
        assert_eq!(
            worked_out("width", "10px"),
            None,
            "a plain length is left alone"
        );
        assert_eq!(worked_out("width", "calc(1em + 2px)"), None);
        assert_eq!(worked_out("fill", "calc(1px)"), None);
        assert_eq!(geometry("width", "calc(100% - 2px)", None), None);
        assert_eq!(
            geometry("width", "calc(4px - 2px)", None).as_deref(),
            Some("2")
        );
    }

    #[test]
    fn a_use_reaches_a_symbol_defined_elsewhere_in_the_page() {
        let document = otlyra_html::parse(
            b"<svg style=display:none><symbol id=dot viewBox=\"0 0 1 1\"><rect width=1 height=1 fill=#00f /></symbol></svg>\
              <p><svg width=4 height=4><use href=#dot width=4 height=4 /></svg>",
            Some("utf-8"),
        )
        .document;
        let mut svgs = Vec::new();
        let mut stack = vec![document.root()];
        while let Some(node) = stack.pop() {
            if document
                .get(node)
                .and_then(|dom| dom.element())
                .is_some_and(|element| &*element.name.local == "svg")
            {
                svgs.push(node);
            }
            let children: Vec<NodeId> = document.children(node).collect();
            stack.extend(children.into_iter().rev());
        }
        let icon = svgs[1];
        let image = replaced(&document, icon, Color::BLACK)
            .image
            .expect("the icon drew");
        let middle = ((image.height / 2 * image.width + image.width / 2) * 4) as usize;
        assert_eq!(&image.data.data()[middle..middle + 4], [0, 0, 255, 255]);
    }
}
