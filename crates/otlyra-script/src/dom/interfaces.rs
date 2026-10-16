//! Which interface an element is made as.
//!
//! A page asks `el instanceof HTMLAnchorElement`, reads `HTMLInputElement`'s
//! members off an `<input>`, and feature-tests `"content" in template` — so
//! an element's wrapper has to be an instance of the interface the
//! specifications assign to its name, not of `Element` for all of them. That
//! assignment is a table, and this is it: HTML §4 (each element's "DOM
//! interface" line, collected in its index of elements), SVG 2's element
//! index, and MathML Core's single `MathMLElement`.
//!
//! The classes themselves are written in `dom.js`, as subclasses of the native
//! `Element`: a wrapper keeps an `ElementRef` as its data whatever its
//! interface is, so every native `Element` member works on it, and only its
//! prototype differs. Every name this table can answer is a class the glue
//! defines — a test holds the two to each other.

use html5ever::{QualName, ns};

/// The interface an element with this name is made as.
///
/// An HTML element nobody defined is an `HTMLUnknownElement`, unless its name
/// is a valid custom element name, in which case it is an `HTMLElement` waiting
/// to be upgraded (HTML §4.13.3). An SVG element without an interface of its
/// own is an `SVGElement`; one in a namespace we draw nothing from is an
/// `Element`.
#[must_use]
pub fn element_interface(name: &QualName) -> &'static str {
    match name.ns {
        ns!(html) => html_interface(&name.local),
        ns!(svg) => svg_interface(&name.local),
        ns!(mathml) => "MathMLElement",
        _ => "Element",
    }
}

/// HTML §4's assignment, plus the obsolete elements §16 still gives one to.
fn html_interface(local: &str) -> &'static str {
    match local {
        "a" => "HTMLAnchorElement",
        "area" => "HTMLAreaElement",
        "audio" => "HTMLAudioElement",
        "base" => "HTMLBaseElement",
        "blockquote" | "q" => "HTMLQuoteElement",
        "body" => "HTMLBodyElement",
        "br" => "HTMLBRElement",
        "button" => "HTMLButtonElement",
        "canvas" => "HTMLCanvasElement",
        "caption" => "HTMLTableCaptionElement",
        "col" | "colgroup" => "HTMLTableColElement",
        "data" => "HTMLDataElement",
        "datalist" => "HTMLDataListElement",
        "del" | "ins" => "HTMLModElement",
        "details" => "HTMLDetailsElement",
        "dialog" => "HTMLDialogElement",
        "dir" => "HTMLDirectoryElement",
        "div" => "HTMLDivElement",
        "dl" => "HTMLDListElement",
        "embed" => "HTMLEmbedElement",
        "fieldset" => "HTMLFieldSetElement",
        "font" => "HTMLFontElement",
        "form" => "HTMLFormElement",
        "frame" => "HTMLFrameElement",
        "frameset" => "HTMLFrameSetElement",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "HTMLHeadingElement",
        "head" => "HTMLHeadElement",
        "hr" => "HTMLHRElement",
        "html" => "HTMLHtmlElement",
        "iframe" => "HTMLIFrameElement",
        "img" => "HTMLImageElement",
        "input" => "HTMLInputElement",
        "label" => "HTMLLabelElement",
        "legend" => "HTMLLegendElement",
        "li" => "HTMLLIElement",
        "link" => "HTMLLinkElement",
        "map" => "HTMLMapElement",
        "marquee" => "HTMLMarqueeElement",
        "menu" => "HTMLMenuElement",
        "meta" => "HTMLMetaElement",
        "meter" => "HTMLMeterElement",
        "object" => "HTMLObjectElement",
        "ol" => "HTMLOListElement",
        "optgroup" => "HTMLOptGroupElement",
        "option" => "HTMLOptionElement",
        "output" => "HTMLOutputElement",
        "p" => "HTMLParagraphElement",
        "param" => "HTMLParamElement",
        "picture" => "HTMLPictureElement",
        "pre" | "listing" | "xmp" => "HTMLPreElement",
        "progress" => "HTMLProgressElement",
        "script" => "HTMLScriptElement",
        "select" => "HTMLSelectElement",
        "slot" => "HTMLSlotElement",
        "source" => "HTMLSourceElement",
        "span" => "HTMLSpanElement",
        "style" => "HTMLStyleElement",
        "table" => "HTMLTableElement",
        "tbody" | "thead" | "tfoot" => "HTMLTableSectionElement",
        "td" | "th" => "HTMLTableCellElement",
        "template" => "HTMLTemplateElement",
        "textarea" => "HTMLTextAreaElement",
        "time" => "HTMLTimeElement",
        "title" => "HTMLTitleElement",
        "tr" => "HTMLTableRowElement",
        "track" => "HTMLTrackElement",
        "ul" => "HTMLUListElement",
        "video" => "HTMLVideoElement",
        // Elements whose interface *is* `HTMLElement`: the phrasing and
        // sectioning elements with no members of their own, and the obsolete
        // ones §16 keeps as plain elements.
        "abbr" | "acronym" | "address" | "article" | "aside" | "b" | "basefont" | "bdi" | "bdo"
        | "big" | "center" | "cite" | "code" | "dd" | "dfn" | "dt" | "em" | "figcaption"
        | "figure" | "footer" | "header" | "hgroup" | "i" | "kbd" | "main" | "mark" | "nav"
        | "nobr" | "noembed" | "noframes" | "noscript" | "plaintext" | "rb" | "rp" | "rt"
        | "rtc" | "ruby" | "s" | "samp" | "search" | "section" | "selectedcontent" | "small"
        | "strike" | "strong" | "sub" | "summary" | "sup" | "tt" | "u" | "var" | "wbr" => {
            "HTMLElement"
        }
        other if is_valid_custom_element_name(other) => "HTMLElement",
        _ => "HTMLUnknownElement",
    }
}

/// SVG 2's element index, for the elements a page reaches for by interface.
fn svg_interface(local: &str) -> &'static str {
    match local {
        "a" => "SVGAElement",
        "animate" => "SVGAnimateElement",
        "animateMotion" => "SVGAnimateMotionElement",
        "animateTransform" => "SVGAnimateTransformElement",
        "circle" => "SVGCircleElement",
        "clipPath" => "SVGClipPathElement",
        "defs" => "SVGDefsElement",
        "desc" => "SVGDescElement",
        "ellipse" => "SVGEllipseElement",
        "filter" => "SVGFilterElement",
        "foreignObject" => "SVGForeignObjectElement",
        "g" => "SVGGElement",
        "image" => "SVGImageElement",
        "line" => "SVGLineElement",
        "linearGradient" => "SVGLinearGradientElement",
        "marker" => "SVGMarkerElement",
        "mask" => "SVGMaskElement",
        "metadata" => "SVGMetadataElement",
        "path" => "SVGPathElement",
        "pattern" => "SVGPatternElement",
        "polygon" => "SVGPolygonElement",
        "polyline" => "SVGPolylineElement",
        "radialGradient" => "SVGRadialGradientElement",
        "rect" => "SVGRectElement",
        "script" => "SVGScriptElement",
        "set" => "SVGSetElement",
        "stop" => "SVGStopElement",
        "style" => "SVGStyleElement",
        "svg" => "SVGSVGElement",
        "switch" => "SVGSwitchElement",
        "symbol" => "SVGSymbolElement",
        "text" => "SVGTextElement",
        "textPath" => "SVGTextPathElement",
        "title" => "SVGTitleElement",
        "tspan" => "SVGTSpanElement",
        "use" => "SVGUseElement",
        "view" => "SVGViewElement",
        _ => "SVGElement",
    }
}

pub use otlyra_dom::is_valid_custom_element_name;

/// Every interface name [`element_interface`] can answer, for the test that
/// holds this table to the classes `dom.js` defines.
#[cfg(test)]
pub(crate) fn all_element_interfaces() -> Vec<&'static str> {
    const HTML: &[&str] = &[
        "a",
        "area",
        "audio",
        "base",
        "blockquote",
        "body",
        "br",
        "button",
        "canvas",
        "caption",
        "col",
        "data",
        "datalist",
        "del",
        "details",
        "dialog",
        "dir",
        "div",
        "dl",
        "embed",
        "fieldset",
        "font",
        "form",
        "frame",
        "frameset",
        "h1",
        "head",
        "hr",
        "html",
        "iframe",
        "img",
        "input",
        "label",
        "legend",
        "li",
        "link",
        "map",
        "marquee",
        "menu",
        "meta",
        "meter",
        "object",
        "ol",
        "optgroup",
        "option",
        "output",
        "p",
        "param",
        "picture",
        "pre",
        "progress",
        "script",
        "select",
        "slot",
        "source",
        "span",
        "style",
        "table",
        "tbody",
        "td",
        "template",
        "textarea",
        "time",
        "title",
        "tr",
        "track",
        "ul",
        "video",
        "section",
        "x-custom",
        "unknownthing",
    ];
    const SVG: &[&str] = &[
        "a",
        "animate",
        "animateMotion",
        "animateTransform",
        "circle",
        "clipPath",
        "defs",
        "desc",
        "ellipse",
        "filter",
        "foreignObject",
        "g",
        "image",
        "line",
        "linearGradient",
        "marker",
        "mask",
        "metadata",
        "path",
        "pattern",
        "polygon",
        "polyline",
        "radialGradient",
        "rect",
        "script",
        "set",
        "stop",
        "style",
        "svg",
        "switch",
        "symbol",
        "text",
        "textPath",
        "title",
        "tspan",
        "use",
        "view",
        "feGaussianBlur",
    ];
    let mut names: Vec<&'static str> = HTML.iter().map(|local| html_interface(local)).collect();
    names.extend(SVG.iter().map(|local| svg_interface(local)));
    names.extend(["MathMLElement", "Element"]);
    names.sort_unstable();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use html5ever::LocalName;

    fn html(local: &str) -> &'static str {
        element_interface(&QualName::new(None, ns!(html), LocalName::from(local)))
    }

    #[test]
    fn html_elements_are_made_as_their_own_interface() {
        assert_eq!(html("a"), "HTMLAnchorElement");
        assert_eq!(html("h3"), "HTMLHeadingElement");
        assert_eq!(html("td"), "HTMLTableCellElement");
        assert_eq!(html("article"), "HTMLElement");
    }

    #[test]
    fn every_element_interface_is_defined_by_the_bootstrap() {
        for name in all_element_interfaces() {
            assert!(
                name == "Element"
                    || super::super::interface_tree::INTERFACE_TREE
                        .iter()
                        .any(|(defined, _)| *defined == name),
                "{name} has a native mapping but no bootstrap interface"
            );
        }
    }

    #[test]
    fn a_custom_element_name_is_an_html_element_and_anything_else_unknown() {
        assert_eq!(html("my-widget"), "HTMLElement");
        assert_eq!(html("blink"), "HTMLUnknownElement");
        assert!(!is_valid_custom_element_name("font-face"));
        assert!(!is_valid_custom_element_name("My-widget"));
        assert!(!is_valid_custom_element_name("widget"));
    }

    #[test]
    fn foreign_elements_keep_their_case_and_their_namespace() {
        let svg =
            |local: &str| element_interface(&QualName::new(None, ns!(svg), LocalName::from(local)));
        assert_eq!(svg("foreignObject"), "SVGForeignObjectElement");
        assert_eq!(svg("feTurbulence"), "SVGElement");
        assert_eq!(
            element_interface(&QualName::new(None, ns!(mathml), LocalName::from("mi"))),
            "MathMLElement"
        );
    }
}
