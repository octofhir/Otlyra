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
    let mut out = String::new();
    let mut ids = HashSet::new();
    let mut uses = Vec::new();
    write_node(document, node, true, color, &mut out, &mut ids, &mut uses);

    // Symbols this drawing names but does not hold, copied in as definitions.
    // A sprite sheet is an `svg` of `<symbol>`s hidden somewhere in the page,
    // and each icon is an `svg` holding nothing but a `<use>` of one of them.
    let mut defs = String::new();
    let mut depth = 0;
    while !uses.is_empty() && depth < USE_DEPTH {
        depth += 1;
        let wanted: Vec<String> = std::mem::take(&mut uses)
            .into_iter()
            .filter(|id| !ids.contains(id))
            .collect();
        for id in wanted {
            if ids.contains(&id) {
                continue;
            }
            let Some(target) = element_by_id(document, &id) else {
                continue;
            };
            write_node(
                document, target, false, color, &mut defs, &mut ids, &mut uses,
            );
        }
    }
    if !defs.is_empty() {
        let close = out.rfind("</svg>").unwrap_or(out.len());
        out.insert_str(close, &format!("<defs>{defs}</defs>"));
    }
    out
}

/// Write `node` and what is inside it, noting the ids it defines and the ones
/// its `<use>`s name.
fn write_node(
    document: &Document,
    node: NodeId,
    root: bool,
    color: Color,
    out: &mut String,
    ids: &mut HashSet<String>,
    uses: &mut Vec<String>,
) {
    let Some(dom) = document.get(node) else {
        return;
    };
    match &dom.data {
        NodeData::Element(element) => {
            let name = &*element.name.local;
            out.push('<');
            out.push_str(name);
            if root {
                out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                out.push_str(" xmlns:xlink=\"http://www.w3.org/1999/xlink\"");
                // What `currentColor` is: the colour the page's rules gave the
                // element, which is the page's, not the markup's.
                let rgba = color.to_rgba8();
                out.push_str(&format!(
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
                    ids.insert(attribute.value.to_string());
                }
                if name == "use"
                    && local == "href"
                    && let Some(id) = attribute.value.strip_prefix('#')
                {
                    uses.push(id.to_owned());
                }
                out.push(' ');
                out.push_str(prefix);
                out.push_str(local);
                out.push_str("=\"");
                escape(&attribute.value, true, out);
                out.push('"');
            }
            out.push('>');
            for child in document.children(node) {
                write_node(document, child, false, color, out, ids, uses);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        NodeData::Text(text) => escape(text, false, out),
        _ => {}
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
