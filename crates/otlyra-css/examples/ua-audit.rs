//! What our built-in stylesheet says about every HTML element.
//!
//! One line per element, tab-separated, so it can be put beside the same list from
//! a reference browser and diffed. The properties are the ones that decide whether
//! an element *renders* like itself — what box it makes, what room it takes, and
//! what its text looks like — rather than everything the cascade computes.
//!
//! ```text
//! cargo run -p otlyra-css --example ua-audit
//! ```

use otlyra_css::cascade::{Viewport, style_document};
use otlyra_css::{Display, Length, LengthOrAuto};

/// Every element in the HTML standard's index, plus the ones the web still
/// contains and the parser still has to place.
const ELEMENTS: &str = "\
html body article section nav aside h1 h2 h3 h4 h5 h6 hgroup header footer address \
p hr pre blockquote ol ul menu li dl dt dd figure figcaption main div \
a em strong small s cite q dfn abbr ruby rt rp data time code var samp kbd sub sup \
i b u mark bdi bdo span br wbr ins del picture source img iframe embed object video \
audio track map area table caption colgroup col tbody thead tfoot tr td th form \
label input button select datalist optgroup option textarea output progress meter \
fieldset legend details summary dialog script noscript template slot canvas center \
font big strike tt marquee frameset";

fn main() {
    println!(
        "element\tdisplay\tmargin\tpadding\tfont\tweight\tstyle\tfamily\twhite-space\talign\tdecoration\tlist\tborder"
    );

    for name in ELEMENTS.split_whitespace() {
        // Each element on its own, inside a body, so that inheritance is the
        // ordinary one and nothing else is in the way.
        let markup = format!("<body><{name} id=probe>x</{name}></body>");
        let parsed = otlyra_html::parse(markup.as_bytes(), Some("utf-8"));
        let styled = style_document(&parsed.document, Viewport::default());

        let Some(node) = otlyra_css::stylo_dom::select(&parsed.document, "#probe")
            .ok()
            .and_then(|found| found.into_iter().next())
        else {
            println!("{name}\tNOT-PARSED");
            continue;
        };
        let Some(values) = styled.style_of(node) else {
            println!("{name}\tNOT-STYLED");
            continue;
        };
        let style = otlyra_css::computed::to_layout_style(values);

        println!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            display(style.display),
            sides_auto(style.margin),
            sides(style.padding),
            round(style.font_size),
            style.font_weight,
            format_args!("{:?}", style.font_style),
            style.font_family,
            format_args!("{:?}", style.white_space),
            format_args!("{:?}", style.text_align),
            decoration(&style.text_decoration),
            format_args!("{:?}", style.list_style),
            round(style.border.top.width),
        );
    }
}

fn display(value: Display) -> &'static str {
    match value {
        Display::None => "none",
        Display::Block => "block",
        Display::Inline => "inline",
        Display::InlineBlock => "inline-block",
        Display::Flex => "flex",
        Display::Grid => "grid",
        Display::Table => "table",
        Display::TableRowGroup => "table-row-group",
        Display::TableRow => "table-row",
        Display::TableCell => "table-cell",
        Display::TableCaption => "table-caption",
    }
}

fn sides(value: otlyra_css::Sides<Length>) -> String {
    let one = |length: Length| match length {
        Length::Px(px) => round(px),
        Length::Percent(fraction) => format!("{}%", fraction * 100.0),
    };
    format!(
        "{} {} {} {}",
        one(value.top),
        one(value.right),
        one(value.bottom),
        one(value.left)
    )
}

fn sides_auto(value: otlyra_css::Sides<LengthOrAuto>) -> String {
    let one = |length: LengthOrAuto| match length {
        LengthOrAuto::Px(px) => round(px),
        LengthOrAuto::Percent(fraction) => format!("{}%", fraction * 100.0),
        LengthOrAuto::Auto => "auto".to_owned(),
    };
    format!(
        "{} {} {} {}",
        one(value.top),
        one(value.right),
        one(value.bottom),
        one(value.left)
    )
}

fn decoration(value: &otlyra_css::TextDecoration) -> String {
    let mut out = Vec::new();
    if value.underline {
        out.push("underline");
    }
    if value.line_through {
        out.push("line-through");
    }
    if out.is_empty() {
        "none".to_owned()
    } else {
        out.join(" ")
    }
}

fn round(value: f32) -> String {
    format!("{:.1}", value)
}
