//! Presentational hints: the style an element's legacy attributes stand for.
//!
//! HTML's rendering chapter gives some attributes a place in the cascade of their
//! own (§15.2): author-level and of zero specificity, so beneath every rule a page
//! writes and above the user-agent sheet. `<td bgcolor>` beats the transparent
//! default and loses to `td { background: … }`. The engine has an origin for
//! exactly that, and this module is what fills it.
//!
//! An attribute's value is not CSS. Each one is read with the microsyntax HTML
//! gives it — a non-negative integer or a dimension (§2.3.4), a legacy colour
//! (§2.3.6), an enumerated keyword — and becomes a typed declaration. A value that
//! does not parse contributes what the specification says it does, which is
//! usually nothing and never a stylesheet: `bgcolor="red;display:none"` is a
//! colour, however it is spelled.
//!
//! Only the elements the specification names take an attribute's hint; the rest
//! of the page's `width`s and `bgcolor`s mean nothing, as they do in both
//! references.
//!
//! Not yet done, and said so rather than approximated:
//!
//! - a table's `frame`, `rules` and `bordercolor`, and `caption align=bottom`;
//! - `body`'s `link`, `vlink` and `alink`, and the list attributes;
//! - the margins a `frame` or an `iframe` hands the `body` of the document in it
//!   (§15.3.2), because a frame's document is not shown;
//! - the `aspect-ratio` an `img`'s, a `video`'s, an image button's and a
//!   `canvas`'s `width` and `height` map to (§15.4.3), because layout does not
//!   read the property yet: a picture that has not arrived reserves no room;
//! - the `width` and `height` of the `source` a `picture` chose, which §15.4.3
//!   makes the `img`'s dimension attribute source: the `img`'s own are read;
//! - `align=middle` and `align=center` on embedded content, which put the
//!   element's middle on the parent's baseline: they are `vertical-align:
//!   middle`, half an x-height higher, because the keyword for the first
//!   (`-moz-middle-with-baseline`) is only in the style engine's Gecko build.

use html5ever::ns;
use otlyra_dom::form::is_image_button;
use otlyra_dom::{Document, ElementData, NodeId};
use style::color::AbsoluteColor;
use style::properties::longhands::{background_image, text_wrap_mode, white_space_collapse};
use style::properties::{
    Importance, LonghandId, PropertyDeclaration, PropertyDeclarationBlock, PropertyId,
    SourcePropertyDeclaration,
};
use style::servo::attr::{self, LengthOrPercentageOrAuto};
use style::stylesheets::UrlExtraData;
use style::values::computed::Percentage;
use style::values::generics::NonNegative;
use style::values::generics::box_::BaselineShiftKeyword;
use style::values::specified::{
    self, AlignmentBaseline, BaselineShift, BaselineSource, BorderSideWidth, BorderStyle, Clear,
    Float, NoCalcLength, TextAlignKeyword,
};
use style_traits::ParsingMode;

/// The declarations `node`'s presentational attributes stand for, or `None` when
/// none of them apply.
///
/// One arm per element the specification gives hints to; everything else has
/// none, whatever attributes it carries. `base` is the document's base URL, which
/// an attribute that names a picture is resolved against.
pub(crate) fn presentational_hints(
    document: &Document,
    node: NodeId,
    base: &UrlExtraData,
) -> Option<PropertyDeclarationBlock> {
    let element = document.get(node)?.element()?;
    let mut hints = Hints::new(element, base);
    match (&element.name.ns, element.name.local.as_ref()) {
        (&ns!(html), "body") => body_hints(&mut hints),
        (&ns!(html), "div") => hints.align(ALIGN_DESCENDANTS),
        (&ns!(html), "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => hints.align(TEXT_ALIGN),
        (&ns!(html), "pre") => pre_hints(&mut hints),
        (&ns!(html), "br") => br_hints(&mut hints),
        (&ns!(html), "font") => font_hints(&mut hints),
        (&ns!(html), "hr") => hr_hints(&mut hints),
        (&ns!(html), "table") => table_hints(&mut hints),
        (&ns!(html), "thead" | "tbody" | "tfoot" | "tr") => row_hints(&mut hints),
        (&ns!(html), "td" | "th") => cell_hints(&mut hints, document, node),
        // §15.3.8 maps the width on `col` alone; both references map it on a
        // `colgroup` as well, and so do the pages written against them.
        (&ns!(html), "col" | "colgroup") => hints.dimension("width", Zero::Allowed, Axis::Width),
        (&ns!(html), "img" | "object") => picture_hints(&mut hints),
        (&ns!(html), "input") if is_image_button(document, node) => image_button_hints(&mut hints),
        (&ns!(html), "embed") => embed_hints(&mut hints),
        (&ns!(html), "iframe") => iframe_hints(&mut hints),
        (&ns!(html), "video") => dimension_hints(&mut hints),
        (&ns!(html), "marquee") => {
            hints.legacy_colour("bgcolor", Colour::Background);
            dimension_hints(&mut hints);
        }
        (&ns!(svg), "svg") if is_outermost_svg(document, node) => svg_hints(&mut hints),
        _ => {}
    }
    hints.finish()
}

/// Whether a table's `border` attribute draws a border (§15.3.8).
///
/// Present, and not "equivalent to zero": a value that is not a non-negative
/// integer at all draws one, a pixel wide, so `border` and `border=yes` frame the
/// table and `border=0` and `border=0px` do not.
pub(crate) fn draws_border(table: &ElementData) -> bool {
    border_width(table).is_some_and(|width| width != 0)
}

/// A table's `border` attribute as a width, if it has one.
///
/// It maps to the pixel length properties, and a value that fails to parse is
/// one pixel rather than nothing (§15.3.8).
fn border_width(table: &ElementData) -> Option<u32> {
    table
        .attr("border")
        .map(|value| non_negative_integer(value).unwrap_or(1))
}

/// `align` on a `div` (§15.3.3), and on a table's sections, rows and cells
/// (§15.3.8).
///
/// These attributes align the text *and* the blocks inside, which the `-moz-`
/// keywords say and the plain ones do not. `justify` is the exception: the text
/// is justified and the blocks go left, and no one keyword says both — the text,
/// which is what a reader sees first, is what it gets.
const ALIGN_DESCENDANTS: &[(&str, TextAlignKeyword)] = &[
    ("center", TextAlignKeyword::MozCenter),
    ("middle", TextAlignKeyword::MozCenter),
    ("left", TextAlignKeyword::MozLeft),
    ("right", TextAlignKeyword::MozRight),
    ("justify", TextAlignKeyword::Justify),
];

/// `align` on a paragraph or a heading (§15.3.8): the text, and nothing else.
const TEXT_ALIGN: &[(&str, TextAlignKeyword)] = &[
    ("left", TextAlignKeyword::Left),
    ("right", TextAlignKeyword::Right),
    ("center", TextAlignKeyword::Center),
    ("justify", TextAlignKeyword::Justify),
];

/// `align=absmiddle` on a table's sections, rows and cells (§15.3.8).
///
/// Written as a rule of its own in the specification, apart from the others: it
/// centres the text and leaves the blocks where they were.
const ABSMIDDLE: &[(&str, TextAlignKeyword)] = &[("absmiddle", TextAlignKeyword::Center)];

/// `valign` on a table's sections, rows and cells (§15.3.8).
const VALIGN: &[(&str, VerticalAlign)] = &[
    ("top", VerticalAlign::Top),
    ("middle", VerticalAlign::Middle),
    ("bottom", VerticalAlign::Bottom),
    ("baseline", VerticalAlign::Baseline),
];

/// `align` on a table, which places the table rather than anything in it
/// (§15.3.8).
const TABLE_ALIGN: &[(&str, TableAlign)] = &[
    ("left", TableAlign::Float(Float::Left)),
    ("right", TableAlign::Float(Float::Right)),
    ("center", TableAlign::Centre),
];

/// One of the four `margin-*` longhands, as the declaration that sets it.
type MarginSide = fn(specified::Margin) -> PropertyDeclaration;

/// `body`'s margin attributes (§15.3.2), per side: the first of the two that the
/// element has is the one that counts, whether or not it parses.
const BODY_MARGINS: [([&str; 2], MarginSide); 4] = [
    (
        ["marginheight", "topmargin"],
        PropertyDeclaration::MarginTop,
    ),
    (
        ["marginwidth", "rightmargin"],
        PropertyDeclaration::MarginRight,
    ),
    (
        ["marginheight", "bottommargin"],
        PropertyDeclaration::MarginBottom,
    ),
    (
        ["marginwidth", "leftmargin"],
        PropertyDeclaration::MarginLeft,
    ),
];

/// `clear` on a `br` (§15.3.4).
const BR_CLEAR: &[(&str, Clear)] = &[
    ("left", Clear::Left),
    ("right", Clear::Right),
    ("all", Clear::Both),
    ("both", Clear::Both),
];

/// `align` on an `hr` (§15.3.11), which puts the rule against one side or in
/// the middle.
const HR_ALIGN: &[(&str, HrAlign)] = &[
    ("left", HrAlign::Left),
    ("right", HrAlign::Right),
    ("center", HrAlign::Centre),
];

/// `align` on embedded content (§15.4.3), which floats it to one side or lines
/// it up with the text around it.
///
/// `middle` and `center` are meant to put the element's middle on the parent's
/// baseline; they get `middle`, which is half an x-height higher (see the
/// module's list of what is not done).
const EMBEDDED_ALIGN: &[(&str, EmbeddedAlign)] = &[
    ("left", EmbeddedAlign::Float(Float::Left)),
    ("right", EmbeddedAlign::Float(Float::Right)),
    ("top", EmbeddedAlign::Vertical(VerticalAlign::Top)),
    ("baseline", EmbeddedAlign::Vertical(VerticalAlign::Baseline)),
    ("texttop", EmbeddedAlign::Vertical(VerticalAlign::TextTop)),
    ("absmiddle", EmbeddedAlign::Vertical(VerticalAlign::Middle)),
    ("abscenter", EmbeddedAlign::Vertical(VerticalAlign::Middle)),
    ("middle", EmbeddedAlign::Vertical(VerticalAlign::Middle)),
    ("center", EmbeddedAlign::Vertical(VerticalAlign::Middle)),
    ("bottom", EmbeddedAlign::Vertical(VerticalAlign::Bottom)),
];

/// The page (§15.3.2): its background, its text's colour, and its margins in
/// pixels.
fn body_hints(hints: &mut Hints<'_>) {
    hints.legacy_colour("bgcolor", Colour::Background);
    hints.background_picture();
    hints.legacy_colour("text", Colour::Text);
    for (names, side) in BODY_MARGINS {
        let pixels = names
            .iter()
            .find_map(|name| hints.attribute(name))
            .and_then(non_negative_integer);
        if let Some(pixels) = pixels {
            hints.push(side(margin(pixels as f32)));
        }
    }
}

/// `wrap` on a `pre` (§15.3.3): the text keeps its spaces and breaks its lines.
fn pre_hints(hints: &mut Hints<'_>) {
    if hints.attribute("wrap").is_some() {
        hints.white_space(
            white_space_collapse::SpecifiedValue::Preserve,
            text_wrap_mode::SpecifiedValue::Wrap,
        );
    }
}

/// `clear` on a `br` (§15.3.4): the next line starts below the floats on the
/// side it names.
fn br_hints(hints: &mut Hints<'_>) {
    if let Some(clear) = hints.keyword("clear", BR_CLEAR) {
        hints.push(PropertyDeclaration::Clear(clear));
    }
}

/// `font` (§15.3.4): the colour, the family and the size of its text.
///
/// `face` is a `font-family` value, parsed as one and nothing more, so a list of
/// families is a list and a value that is not one sets nothing. `size` is one
/// of the seven sizes of old, from `x-small` to `xxx-large`.
fn font_hints(hints: &mut Hints<'_>) {
    hints.legacy_colour("color", Colour::Text);
    hints.css_value("face", LonghandId::FontFamily, ParsingMode::DEFAULT);
    if let Some(size) = hints.attribute("size").and_then(legacy_font_size) {
        hints.push(PropertyDeclaration::FontSize(
            specified::FontSize::from_html_size(size),
        ));
    }
}

/// A rule (§15.3.11).
///
/// A `color` or a `noshade` makes the inset lines one solid one, and a `size`
/// is then the width of every border between them. Without either, a `size` of
/// one takes the lower line away and a larger one is the height between the two
/// lines, less the two pixels they take themselves.
fn hr_hints(hints: &mut Hints<'_>) {
    if let Some(align) = hints.keyword("align", HR_ALIGN) {
        let (left, right) = match align {
            HrAlign::Left => (margin(0.0), specified::Margin::Auto),
            HrAlign::Right => (specified::Margin::Auto, margin(0.0)),
            HrAlign::Centre => (specified::Margin::Auto, specified::Margin::Auto),
        };
        hints.push(PropertyDeclaration::MarginLeft(left));
        hints.push(PropertyDeclaration::MarginRight(right));
    }

    let solid = hints.attribute("color").is_some() || hints.attribute("noshade").is_some();
    if solid {
        hints.border_style(BorderStyle::Solid);
    }
    hints.legacy_colour("color", Colour::Text);

    match hints.attribute("size").and_then(non_negative_integer) {
        Some(size) if solid => hints.pixels(size as f32 / 2.0, Pixels::BorderWidth),
        Some(1) => hints.push(PropertyDeclaration::BorderBottomWidth(
            BorderSideWidth::from_px(0.0),
        )),
        Some(size @ 2..) => hints.size(
            NoCalcLength::from_px((size - 2) as f32).into(),
            Axis::Height,
        ),
        Some(0) | None => {}
    }

    hints.dimension("width", Zero::Allowed, Axis::Width);
}

/// The table itself (§15.3.8).
fn table_hints(hints: &mut Hints<'_>) {
    hints.legacy_colour("bgcolor", Colour::Background);
    hints.background_picture();
    hints.dimension("width", Zero::Refused, Axis::Width);
    hints.dimension("height", Zero::Allowed, Axis::Height);
    match hints.keyword("align", TABLE_ALIGN) {
        Some(TableAlign::Float(side)) => hints.push(PropertyDeclaration::Float(side)),
        Some(TableAlign::Centre) => hints.centre_inline(),
        None => {}
    }
    if let Some(spacing) = hints
        .attribute("cellspacing")
        .and_then(non_negative_integer)
    {
        hints.pixels(spacing as f32, Pixels::BorderSpacing);
    }
    if let Some(width) = border_width(hints.element) {
        hints.pixels(width as f32, Pixels::BorderWidth);
        if width != 0 {
            hints.border_style(BorderStyle::Outset);
        }
    }
}

/// A row, or a group of them (§15.3.8): the two take the same attributes.
fn row_hints(hints: &mut Hints<'_>) {
    table_part_hints(hints);
    hints.dimension("height", Zero::Allowed, Axis::Height);
}

/// A cell (§15.3.8): its own attributes, and the two its table hands down to it.
fn cell_hints(hints: &mut Hints<'_>, document: &Document, cell: NodeId) {
    table_part_hints(hints);
    hints.dimension("width", Zero::Refused, Axis::Width);
    hints.dimension("height", Zero::Refused, Axis::Height);
    if hints.attribute("nowrap").is_some() {
        hints.white_space(
            white_space_collapse::SpecifiedValue::Collapse,
            nowrap_mode(hints.element, document),
        );
    }

    let Some(table) = owning_table(document, cell) else {
        return;
    };
    if let Some(padding) = table.attr("cellpadding").and_then(non_negative_integer) {
        hints.pixels(padding as f32, Pixels::Padding);
    }
    // A pixel-wide inset line round every cell, however wide the table's own
    // border is — which is what makes `border=5` a thick frame round a thin grid.
    if draws_border(table) {
        hints.pixels(1.0, Pixels::BorderWidth);
        hints.border_style(BorderStyle::Inset);
    }
}

/// What a table's sections, rows and cells share: a background, and where the
/// text goes in both directions.
fn table_part_hints(hints: &mut Hints<'_>) {
    hints.legacy_colour("bgcolor", Colour::Background);
    hints.background_picture();
    hints.align(ALIGN_DESCENDANTS);
    hints.align(ABSMIDDLE);
    if let Some(keyword) = hints.keyword("valign", VALIGN) {
        hints.vertical_align(keyword);
    }
}

/// Whether a `nowrap` cell's lines may break (§15.3.8).
///
/// They may not — except in quirks mode, where a cell that also has a width in
/// pixels wraps after all, as the pages that relied on it expect.
fn nowrap_mode(cell: &ElementData, document: &Document) -> text_wrap_mode::SpecifiedValue {
    let quirks = document.quirks_mode() == html5ever::interface::QuirksMode::Quirks;
    let width_in_pixels = cell
        .attr("width")
        .and_then(|value| dimension(value, Zero::Refused))
        .is_some_and(|width| matches!(width, specified::LengthPercentage::Length(_)));
    if quirks && width_in_pixels {
        text_wrap_mode::SpecifiedValue::Wrap
    } else {
        text_wrap_mode::SpecifiedValue::Nowrap
    }
}

/// The table whose `cellpadding` and `border` reach `cell`.
///
/// The one the cell's row belongs to in the table model (§15.3.8, "any `td` and
/// `th` elements that have corresponding cells in the table"): the row is the
/// cell's parent, and it belongs to the table that is its own parent or its
/// section's. A cell anywhere else has no table to take them from.
fn owning_table(document: &Document, cell: NodeId) -> Option<&ElementData> {
    let (row, element) = html_parent(document, cell)?;
    if element.name.local.as_ref() != "tr" {
        return None;
    }
    let (above, element) = html_parent(document, row)?;
    match element.name.local.as_ref() {
        "table" => Some(element),
        "thead" | "tbody" | "tfoot" => html_parent(document, above)
            .map(|(_, table)| table)
            .filter(|table| table.name.local.as_ref() == "table"),
        _ => None,
    }
}

/// `node`'s parent, when that is an HTML element.
fn html_parent(document: &Document, node: NodeId) -> Option<(NodeId, &ElementData)> {
    let parent = document.get(node)?.parent?;
    let element = document.get(parent)?.element()?;
    (element.name.ns == ns!(html)).then_some((parent, element))
}

/// `width` and `height` as dimensions, zero included: embedded content
/// (§15.4.3) and a `marquee` (§15.5.13).
///
/// They are the lowest-priority rule setting the two properties, so a
/// stylesheet that says `height: auto` has the last word, and a picture given
/// only one of them takes the other from its own ratio.
fn dimension_hints(hints: &mut Hints<'_>) {
    hints.dimension("width", Zero::Allowed, Axis::Width);
    hints.dimension("height", Zero::Allowed, Axis::Height);
}

/// An `img` or an `object` (§15.4.3): its size, where it sits, the room around
/// it and the line round it.
fn picture_hints(hints: &mut Hints<'_>) {
    dimension_hints(hints);
    placement_hints(hints);
    hints.picture_border();
}

/// An `input` in the Image Button state (§15.4.3): an `img`'s attributes,
/// except that its size is only its own while it shows a picture, or will —
/// one with nothing to show is a button round its alternative text, as big as
/// that text is.
fn image_button_hints(hints: &mut Hints<'_>) {
    if hints.element.address("src").is_some() {
        dimension_hints(hints);
    }
    placement_hints(hints);
    hints.picture_border();
}

/// An `embed` (§15.4.3): an `img`'s attributes but the border.
fn embed_hints(hints: &mut Hints<'_>) {
    dimension_hints(hints);
    placement_hints(hints);
}

/// An `iframe` (§15.4.3): its size, its `align`, and a `frameborder` that
/// takes the user-agent sheet's inset border away.
///
/// Zero takes it away, and so does anything that is not an integer at all:
/// `frameborder=no` has no frame and `frameborder=-1` keeps it, as both
/// references draw them. Room round a frame is not something its attributes
/// ask for: it takes no `hspace` or `vspace`.
fn iframe_hints(hints: &mut Hints<'_>) {
    dimension_hints(hints);
    hints.embedded_align();
    let borderless = hints.attribute("frameborder").is_some_and(|value| {
        attr::parse_integer(value.chars())
            .ok()
            .is_none_or(|number| number == 0)
    });
    if borderless {
        hints.pixels(0.0, Pixels::BorderWidth);
    }
}

/// `align`, `hspace` and `vspace` (§15.4.3): where a picture sits among the
/// text, and how much room it keeps from it.
fn placement_hints(hints: &mut Hints<'_>) {
    hints.embedded_align();
    hints.spacing(
        "hspace",
        [
            PropertyDeclaration::MarginLeft,
            PropertyDeclaration::MarginRight,
        ],
    );
    hints.spacing(
        "vspace",
        [
            PropertyDeclaration::MarginTop,
            PropertyDeclaration::MarginBottom,
        ],
    );
}

/// An outermost `svg`'s `width` and `height`.
///
/// Presentation attributes for the geometry properties (SVG 2, "Presentation
/// attributes"), which is to say CSS in the property's own grammar with a bare
/// number read as pixels. They are what sizes an inline icon.
fn svg_hints(hints: &mut Hints<'_>) {
    let mode = ParsingMode::ALLOW_UNITLESS_LENGTH;
    hints.css_value("width", LonghandId::Width, mode);
    hints.css_value("height", LonghandId::Height, mode);
}

/// Whether `node` begins an SVG fragment rather than sitting inside one — the
/// only kind of `svg` that CSS lays out as a box of its own.
fn is_outermost_svg(document: &Document, node: NodeId) -> bool {
    let parent = document
        .get(node)
        .and_then(|node| node.parent)
        .and_then(|parent| document.get(parent))
        .and_then(|parent| parent.element());
    parent.is_none_or(|parent| parent.name.ns != ns!(svg))
}

/// HTML's rules for parsing non-negative integers (§2.3.4.2): leading digits,
/// after any whitespace and a `+`, and whatever follows them ignored.
///
/// Public because layout reads one attribute this way that is not a hint: a
/// `canvas`'s size, which is its bitmap's rather than its box's.
pub fn non_negative_integer(value: &str) -> Option<u32> {
    attr::parse_unsigned_integer(value.chars()).ok()
}

/// HTML's rules for parsing dimension values (§2.3.4.4), or nonzero dimension
/// values (§2.3.4.5): leading digits and a fraction, in pixels, or a percentage if
/// a `%` follows them. `100px` is a hundred pixels and `50abc` fifty.
fn dimension(value: &str, zero: Zero) -> Option<specified::LengthPercentage> {
    let parsed = match zero {
        Zero::Allowed => attr::parse_length(value),
        Zero::Refused => attr::parse_nonzero_length(value),
    };
    match parsed {
        LengthOrPercentageOrAuto::Length(length) => {
            Some(NoCalcLength::from_px(length.to_f32_px()).into())
        }
        LengthOrPercentageOrAuto::Percentage(fraction) if fraction.is_finite() => {
            Some(Percentage(fraction).into())
        }
        // A percentage of more digits than a float holds, and the parser's own
        // word for "not a dimension", which is not the CSS keyword.
        LengthOrPercentageOrAuto::Percentage(_) | LengthOrPercentageOrAuto::Auto => None,
    }
}

/// HTML's rules for parsing a legacy font size (§15.3.4): digits after any
/// whitespace, on the scale of one to seven, or relative to three with a sign in
/// front of them, and clamped to the scale either way.
fn legacy_font_size(value: &str) -> Option<u8> {
    let value = value.trim_start_matches(|character: char| character.is_ascii_whitespace());
    let (sign, rest) = match value.as_bytes().first()? {
        b'+' => (Some(1), &value[1..]),
        b'-' => (Some(-1), &value[1..]),
        _ => (None, value),
    };
    let digits = &rest[..rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len())];
    if digits.is_empty() {
        return None;
    }
    // Only digits are left, so the one way to fail is to be too many of them,
    // and a number that large is past the end of the scale either way.
    let number = digits.parse::<i64>().unwrap_or(i64::MAX);
    let size = match sign {
        Some(sign) => 3i64.saturating_add(sign * number),
        None => number,
    };
    u8::try_from(size.clamp(1, 7)).ok()
}

/// A margin of so many pixels.
fn margin(pixels: f32) -> specified::Margin {
    specified::Margin::LengthPercentage(NoCalcLength::from_px(pixels).into())
}

/// HTML's rules for parsing a legacy colour value (§2.3.6).
///
/// Everything is a colour except the empty string and `transparent`: a keyword, a
/// three-digit hex, or whatever hex digits can be dug out of the rest, which is
/// why `00ff00` is green and `red;display:none` is a dark red.
fn legacy_colour(value: &str) -> Option<AbsoluteColor> {
    // The algorithm strips the whitespace and carries on with what is left, and
    // nothing at all pads out to `000`: black. The engine's implementation reads
    // the first character of what is left without asking whether there is one, so
    // that case is answered here.
    if !value.is_empty() && value.trim_ascii().is_empty() {
        return Some(AbsoluteColor::BLACK);
    }
    attr::parse_legacy_color(value).ok()
}

/// One element's hints, as they are gathered.
struct Hints<'a> {
    /// The element whose attributes they are.
    element: &'a ElementData,
    /// The document's base URL.
    base: &'a UrlExtraData,
    /// What they have come to so far.
    block: PropertyDeclarationBlock,
}

/// Which colour a legacy colour attribute sets.
#[derive(Clone, Copy)]
enum Colour {
    /// `background-color`.
    Background,
    /// `color`, the text's.
    Text,
}

/// Which size a dimension attribute sets.
#[derive(Clone, Copy)]
enum Axis {
    /// `width`.
    Width,
    /// `height`.
    Height,
}

/// Whether a dimension of zero is one, or an error (§15.2: "maps to the
/// dimension property", with or without "ignoring zero").
#[derive(Clone, Copy)]
enum Zero {
    /// Zero is a size like any other.
    Allowed,
    /// Zero is as good as no attribute at all.
    Refused,
}

/// What a pixel length attribute sets.
#[derive(Clone, Copy)]
enum Pixels {
    /// All four paddings.
    Padding,
    /// All four border widths.
    BorderWidth,
    /// The spacing between cells, both ways.
    BorderSpacing,
}

/// The `vertical-align` keywords a `valign` or an embedded element's `align`
/// can name.
#[derive(Clone, Copy)]
enum VerticalAlign {
    /// `top`: a cell's content against the top of its row, a picture against
    /// the top of its line.
    Top,
    /// `middle`: centred in the row, which is where a cell sits by default; a
    /// picture's middle half an x-height above the baseline.
    Middle,
    /// `bottom`: against the bottom of the row, or of the line.
    Bottom,
    /// `baseline`: the first line on the row's shared baseline, a picture's
    /// bottom on the line's.
    Baseline,
    /// `text-top`: a picture's top against the top of the parent's text.
    TextTop,
}

/// What an embedded element's `align` does with it.
#[derive(Clone, Copy)]
enum EmbeddedAlign {
    /// Floats it to one side, the text wrapping round it.
    Float(Float),
    /// Leaves it in the line, at a height the keyword names.
    Vertical(VerticalAlign),
}

/// Where an `hr`'s `align` puts it.
#[derive(Clone, Copy)]
enum HrAlign {
    /// Against the left edge.
    Left,
    /// Against the right edge.
    Right,
    /// In the middle.
    Centre,
}

/// Where a table's `align` puts it.
#[derive(Clone, Copy)]
enum TableAlign {
    /// Floated to one side.
    Float(Float),
    /// Centred between its margins.
    Centre,
}

impl<'a> Hints<'a> {
    fn new(element: &'a ElementData, base: &'a UrlExtraData) -> Self {
        Self {
            element,
            base,
            block: PropertyDeclarationBlock::new(),
        }
    }

    /// The block, if anything went into it.
    fn finish(self) -> Option<PropertyDeclarationBlock> {
        (!self.block.is_empty()).then_some(self.block)
    }

    /// The element's attribute `name`, in no namespace.
    fn attribute(&self, name: &str) -> Option<&'a str> {
        self.element.attr(name)
    }

    /// The keyword an enumerated attribute names, matched as HTML matches them:
    /// the whole value, ignoring ASCII case, with no whitespace trimmed.
    fn keyword<T: Copy>(&self, name: &str, keywords: &[(&str, T)]) -> Option<T> {
        let value = self.attribute(name)?;
        keywords
            .iter()
            .find(|(keyword, _)| value.eq_ignore_ascii_case(keyword))
            .map(|&(_, keyword)| keyword)
    }

    /// One declaration, of the normal importance every hint has.
    fn push(&mut self, declaration: PropertyDeclaration) {
        self.block.push(declaration, Importance::Normal);
    }

    /// One value on all four sides of the box.
    fn sides<T: Clone>(&mut self, value: T, sides: [fn(T) -> PropertyDeclaration; 4]) {
        for side in sides {
            self.push(side(value.clone()));
        }
    }

    /// A colour attribute, read as a legacy colour.
    fn legacy_colour(&mut self, name: &str, target: Colour) {
        let Some(colour) = self.attribute(name).and_then(legacy_colour) else {
            return;
        };
        let colour = specified::Color::from_absolute_color(colour);
        self.push(match target {
            Colour::Background => PropertyDeclaration::BackgroundColor(colour),
            Colour::Text => PropertyDeclaration::Color(specified::ColorPropertyValue(colour)),
        });
    }

    /// A `background` attribute, as the one `background-image` layer it names
    /// (§15.3.2, §15.3.8).
    ///
    /// The value is a URL resolved against the document's base, the way
    /// `href` is, and not CSS: it is never inside a `url()`, so nothing in it
    /// can close one. An empty value or one that does not parse is no picture.
    /// The specification encodes a query in the document's own encoding; this
    /// encodes it in UTF-8, the same as every other address here.
    fn background_picture(&mut self) {
        let Some(url) = self
            .attribute("background")
            .filter(|value| !value.is_empty())
            .and_then(|value| self.base.0.join(value).ok())
        else {
            return;
        };
        let picture =
            specified::Image::Url(style::values::CssUrl::for_cascade(servo_arc::Arc::new(url)));
        self.push(PropertyDeclaration::BackgroundImage(
            background_image::SpecifiedValue(vec![picture].into()),
        ));
    }

    /// A size attribute, read as a dimension.
    fn dimension(&mut self, name: &str, zero: Zero, axis: Axis) {
        if let Some(size) = self
            .attribute(name)
            .and_then(|value| dimension(value, zero))
        {
            self.size(size, axis);
        }
    }

    /// `width` or `height`.
    fn size(&mut self, size: specified::LengthPercentage, axis: Axis) {
        let size = specified::Size::LengthPercentage(NonNegative(size));
        self.push(match axis {
            Axis::Width => PropertyDeclaration::Width(size),
            Axis::Height => PropertyDeclaration::Height(size),
        });
    }

    /// A pixel length.
    fn pixels(&mut self, px: f32, target: Pixels) {
        match target {
            Pixels::Padding => self.sides(
                specified::NonNegativeLengthPercentage::from(NoCalcLength::from_px(px)),
                [
                    PropertyDeclaration::PaddingTop,
                    PropertyDeclaration::PaddingRight,
                    PropertyDeclaration::PaddingBottom,
                    PropertyDeclaration::PaddingLeft,
                ],
            ),
            Pixels::BorderWidth => self.sides(
                BorderSideWidth::from_px(px),
                [
                    PropertyDeclaration::BorderTopWidth,
                    PropertyDeclaration::BorderRightWidth,
                    PropertyDeclaration::BorderBottomWidth,
                    PropertyDeclaration::BorderLeftWidth,
                ],
            ),
            Pixels::BorderSpacing => {
                let spacing = || specified::NonNegativeLength::from(NoCalcLength::from_px(px));
                self.push(PropertyDeclaration::BorderSpacing(
                    specified::BorderSpacing::new(spacing(), spacing()),
                ));
            }
        }
    }

    /// `border-style`, all round.
    fn border_style(&mut self, style: BorderStyle) {
        self.sides(
            style,
            [
                PropertyDeclaration::BorderTopStyle,
                PropertyDeclaration::BorderRightStyle,
                PropertyDeclaration::BorderBottomStyle,
                PropertyDeclaration::BorderLeftStyle,
            ],
        );
    }

    /// `margin-inline: auto`, which centres a box that is narrower than its
    /// container.
    fn centre_inline(&mut self) {
        self.push(PropertyDeclaration::MarginInlineStart(
            specified::Margin::Auto,
        ));
        self.push(PropertyDeclaration::MarginInlineEnd(
            specified::Margin::Auto,
        ));
    }

    /// An `align` attribute, as `text-align`: whichever of `keywords` it names.
    fn align(&mut self, keywords: &[(&str, TextAlignKeyword)]) {
        if let Some(keyword) = self.keyword("align", keywords) {
            self.push(PropertyDeclaration::TextAlign(
                specified::TextAlign::Keyword(keyword),
            ));
        }
    }

    /// `vertical-align`, as the three longhands the shorthand expands a keyword
    /// into (css-inline-3 §4): `middle` picks a baseline, `top` and `bottom` shift
    /// to the line's edge, and whichever is not named is reset.
    fn vertical_align(&mut self, keyword: VerticalAlign) {
        let (alignment, shift) = match keyword {
            VerticalAlign::Top => (
                AlignmentBaseline::Baseline,
                BaselineShift::Keyword(BaselineShiftKeyword::Top),
            ),
            VerticalAlign::Bottom => (
                AlignmentBaseline::Baseline,
                BaselineShift::Keyword(BaselineShiftKeyword::Bottom),
            ),
            VerticalAlign::Middle => (AlignmentBaseline::Middle, BaselineShift::zero()),
            VerticalAlign::Baseline => (AlignmentBaseline::Baseline, BaselineShift::zero()),
            VerticalAlign::TextTop => (AlignmentBaseline::TextTop, BaselineShift::zero()),
        };
        self.push(PropertyDeclaration::AlignmentBaseline(alignment));
        self.push(PropertyDeclaration::BaselineShift(shift));
        self.push(PropertyDeclaration::BaselineSource(BaselineSource::Auto));
    }

    /// `align` on embedded content (§15.4.3): a float, or a place in the line.
    fn embedded_align(&mut self) {
        match self.keyword("align", EMBEDDED_ALIGN) {
            Some(EmbeddedAlign::Float(side)) => self.push(PropertyDeclaration::Float(side)),
            Some(EmbeddedAlign::Vertical(keyword)) => self.vertical_align(keyword),
            None => {}
        }
    }

    /// `hspace` or `vspace` (§15.4.3): a dimension, on the two margins either
    /// side of the element along one axis.
    fn spacing(&mut self, name: &str, sides: [MarginSide; 2]) {
        let Some(space) = self
            .attribute(name)
            .and_then(|value| dimension(value, Zero::Allowed))
        else {
            return;
        };
        for side in sides {
            self.push(side(specified::Margin::LengthPercentage(space.clone())));
        }
    }

    /// `border` on a picture (§15.4.3): a solid line that many pixels wide, in
    /// the element's own colour. Zero, or a value that is not a number, is no
    /// line at all — unlike a table's, which draws one pixel for anything it
    /// cannot read.
    fn picture_border(&mut self) {
        let Some(width) = self
            .attribute("border")
            .and_then(non_negative_integer)
            .filter(|&width| width > 0)
        else {
            return;
        };
        self.pixels(width as f32, Pixels::BorderWidth);
        self.border_style(BorderStyle::Solid);
    }

    /// `white-space`, as the two longhands the shorthand sets (css-text-4 §3):
    /// whether spaces collapse, and whether a line may break.
    fn white_space(
        &mut self,
        collapse: white_space_collapse::SpecifiedValue,
        wrap: text_wrap_mode::SpecifiedValue,
    ) {
        self.push(PropertyDeclaration::WhiteSpaceCollapse(collapse));
        self.push(PropertyDeclaration::TextWrapMode(wrap));
    }

    /// An attribute whose value is CSS: `property`'s own grammar, one value and
    /// nothing after it, read in `mode`. A value that does not parse as one sets
    /// nothing at all.
    fn css_value(&mut self, name: &str, property: LonghandId, mode: ParsingMode) {
        let Some(value) = self.attribute(name) else {
            return;
        };
        let mut declarations = SourcePropertyDeclaration::default();
        let parsed = style::properties::parse_one_declaration_into(
            &mut declarations,
            PropertyId::NonCustom(property.into()),
            value,
            style::stylesheets::Origin::Author,
            self.base,
            None,
            mode,
            style::context::QuirksMode::NoQuirks,
            style::stylesheets::CssRuleType::Style,
        );
        if parsed.is_ok() {
            self.block.extend(declarations.drain(), Importance::Normal);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::computed::tests::{layout_style, layout_style_at};
    use crate::style::{
        BorderStyle, Clear, Display, Length, LengthOrAuto, Size, TextAlign, TextWrap,
        VerticalAlign, WhiteSpace,
    };
    use peniko::Color;

    /// A colour as the eight-bit channels it is painted in, which is what two
    /// routes to the same colour agree on to the last bit.
    fn rgba(colour: Color) -> [u8; 4] {
        colour.to_rgba8().to_u8_array()
    }

    /// The background of the first element matching `selector`.
    fn background(html: &str, selector: &str) -> [u8; 4] {
        rgba(layout_style(html, selector).background_color)
    }

    /// An opaque colour.
    fn rgb(r: u8, g: u8, b: u8) -> [u8; 4] {
        [r, g, b, 255]
    }

    #[test]
    fn a_legacy_colour_needs_no_hash() {
        assert_eq!(
            background("<table><tr><td bgcolor=ff0000>x", "td"),
            rgb(255, 0, 0)
        );
        assert_eq!(
            background("<table><tr><td bgcolor=00ff00>x", "td"),
            rgb(0, 255, 0)
        );
        assert_eq!(
            background("<table><tr><td bgcolor=\"#abc\">x", "td"),
            rgb(0xaa, 0xbb, 0xcc)
        );
        assert_eq!(
            background("<table><tr><td bgcolor=Navy>x", "td"),
            rgb(0, 0, 128)
        );
    }

    /// An attribute's value is a colour however it is spelled, and never a
    /// stylesheet: what looks like a second declaration is dug for hex digits
    /// like the rest of it.
    #[test]
    fn a_colour_attribute_cannot_say_anything_but_a_colour() {
        let html = "<table><tr><td bgcolor=\"red;display:none\">x";
        let cell = layout_style(html, "td");
        assert_eq!(cell.display, Display::TableCell);
        // `0ed0d0000a00000e`, padded to eighteen, split in three and trimmed of
        // the zeros the three share: `ed`, `00`, `00` — which is what a reference
        // paints the cell.
        assert_eq!(rgba(cell.background_color), rgb(0xed, 0, 0));
    }

    /// The empty string and `transparent` are no colour at all; whitespace alone
    /// is black, which the algorithm arrives at by padding nothing with zeros.
    #[test]
    fn the_legacy_colour_edge_cases_are_the_specifications() {
        let transparent = rgba(Color::TRANSPARENT);
        assert_eq!(background("<table><tr><td bgcolor>x", "td"), transparent);
        assert_eq!(
            background("<table><tr><td bgcolor=transparent>x", "td"),
            transparent
        );
        assert_eq!(
            background("<table><tr><td bgcolor=\"  \">x", "td"),
            rgb(0, 0, 0)
        );
    }

    /// Only the elements the specification names take a `bgcolor`.
    #[test]
    fn bgcolor_is_honoured_only_where_it_means_something() {
        let transparent = rgba(Color::TRANSPARENT);
        assert_eq!(background("<div bgcolor=red>x</div>", "div"), transparent);
        assert_eq!(background("<p><span bgcolor=red>x", "span"), transparent);
        let red = rgb(255, 0, 0);
        for (html, selector) in [
            ("<body bgcolor=red>", "body"),
            ("<table bgcolor=red><tr><td>x", "table"),
            ("<table><tbody bgcolor=red><tr><td>x", "tbody"),
            ("<table><tr bgcolor=red><td>x", "tr"),
            ("<table><tr><th bgcolor=red>x", "th"),
            ("<marquee bgcolor=red>x</marquee>", "marquee"),
        ] {
            assert_eq!(background(html, selector), red, "{html}");
        }
    }

    /// Two cells side by side with different attributes keep them apart: the
    /// engine's style sharing must not hand one cell the other's hints.
    #[test]
    fn neighbouring_cells_keep_their_own_colours() {
        let html = "<table><tr><td id=a bgcolor=red>a</td><td id=b bgcolor=blue>b";
        assert_eq!(background(html, "#a"), rgb(255, 0, 0));
        assert_eq!(background(html, "#b"), rgb(0, 0, 255));
    }

    /// Three hex digits are the short form only after a `#`: without one they are
    /// three one-digit channels, which is why `00f` is all but black.
    #[test]
    fn font_color_is_the_texts() {
        let colour = |attribute: &str| {
            rgba(layout_style(&format!("<p><font color={attribute}>x</font>"), "font").color)
        };
        assert_eq!(colour("\"#00f\""), rgb(0, 0, 255));
        assert_eq!(colour("00f"), rgb(0, 0, 0x0f));
    }

    /// A dimension is leading digits and a fraction, in pixels or as a
    /// percentage, whatever follows them; a table's width refuses zero.
    #[test]
    fn a_dimension_is_read_as_html_reads_it() {
        let width = |attribute: &str| {
            layout_style(&format!("<table width={attribute}><tr><td>x"), "table").width
        };
        assert_eq!(width("100px"), Size::Length(Length::Px(100.0)));
        assert_eq!(width("50abc"), Size::Length(Length::Px(50.0)));
        assert_eq!(width("12.5"), Size::Length(Length::Px(12.5)));
        assert_eq!(width("50%"), Size::Length(Length::Percent(0.5)));
        assert_eq!(width("1e2"), Size::Length(Length::Px(1.0)));
        assert_eq!(width("+5"), Size::Auto);
        assert_eq!(width("lots"), Size::Auto);
        assert_eq!(width("0"), Size::Auto, "a table's width ignores zero");
        assert_eq!(
            layout_style("<table height=0><tr><td>x", "table").height,
            Size::Length(Length::Px(0.0)),
            "and its height does not"
        );
    }

    #[test]
    fn a_column_and_a_group_of_them_take_a_width() {
        let html = "<table><colgroup width=90><col width=40></colgroup><tr><td>x";
        assert_eq!(
            layout_style(html, "col").width,
            Size::Length(Length::Px(40.0))
        );
        assert_eq!(
            layout_style(html, "colgroup").width,
            Size::Length(Length::Px(90.0))
        );
    }

    /// `width` on an element the specification gives no width to is nothing.
    #[test]
    fn width_is_honoured_only_where_it_means_something() {
        assert_eq!(
            layout_style("<div width=100>x</div>", "div").width,
            Size::Auto
        );
        assert_eq!(
            layout_style("<table><tr><td width=40 height=20>x", "td").width,
            Size::Length(Length::Px(40.0))
        );
        assert_eq!(
            layout_style("<table><tr><td width=0>x", "td").width,
            Size::Auto,
            "a cell's width ignores zero"
        );
    }

    /// An outermost `svg`'s size is CSS with a bare number for pixels; an
    /// `iframe`'s is a dimension.
    #[test]
    fn an_icon_and_a_frame_are_still_sized() {
        let icon = layout_style("<p><svg width=24 height=24></svg>", "svg");
        assert_eq!(icon.width, Size::Length(Length::Px(24.0)));
        assert_eq!(icon.height, Size::Length(Length::Px(24.0)));
        assert_eq!(
            layout_style("<p><svg width=2em></svg>", "svg").width,
            Size::Length(Length::Px(32.0))
        );
        // One value in the property's grammar and nothing after it: a second
        // value, or a second declaration, is not a width at all.
        for value in ["\"24 24\"", "\"24;display:none\"", "\"24px!important\""] {
            let icon = layout_style(&format!("<p><svg width={value}></svg>"), "svg");
            assert_eq!(icon.width, Size::Auto, "{value}");
            assert_eq!(icon.display, Display::Inline, "{value}");
        }

        let frame = layout_style("<iframe width=560 height=315></iframe>", "iframe");
        assert_eq!(frame.width, Size::Length(Length::Px(560.0)));
        assert_eq!(frame.height, Size::Length(Length::Px(315.0)));
    }

    /// A `border` attribute is a width on the table and an inset pixel on every
    /// cell; one that is not a number is one pixel, and one that is zero draws
    /// nothing, however it is written.
    #[test]
    fn a_border_attribute_frames_the_table_and_its_cells() {
        let border = |attribute: &str, selector: &str| {
            layout_style(&format!("<table {attribute}><tr><td>x"), selector)
                .border
                .top
                .width
        };
        assert_eq!(border("border", "table"), 1.0);
        assert_eq!(border("border=yes", "table"), 1.0);
        assert_eq!(border("border=2px", "table"), 2.0);
        assert_eq!(border("border=5", "table"), 5.0);
        assert_eq!(border("border=5", "td"), 1.0, "a cell's is one pixel");
        assert_eq!(border("border", "td"), 1.0);
        for zero in ["border=0", "border=0px", "border=00"] {
            assert_eq!(border(zero, "table"), 0.0, "{zero}");
            assert_eq!(border(zero, "td"), 0.0, "{zero}");
        }
        assert_eq!(border("", "td"), 0.0, "no attribute, no border");
    }

    /// The lines a `border` attribute draws are outset round the table and inset
    /// round each cell, and §15.3.8 gives them no colour: each is its own
    /// element's `currentColor`, as both references draw it.
    #[test]
    fn a_border_attribute_draws_in_the_elements_own_colour() {
        let html = "<table border=5 style=color:red><tr><td style=color:blue>x";
        let table = layout_style(html, "table").border.top;
        assert_eq!(table.style, BorderStyle::Outset);
        assert_eq!(rgba(table.color), rgb(255, 0, 0));
        let cell = layout_style(html, "td").border.top;
        assert_eq!(cell.style, BorderStyle::Inset);
        assert_eq!(rgba(cell.color), rgb(0, 0, 255));
    }

    #[test]
    fn cellspacing_is_the_border_spacing() {
        assert_eq!(
            layout_style("<table cellspacing=5><tr><td>x", "table").border_spacing,
            (5.0, 5.0)
        );
        assert_eq!(
            layout_style("<table cellspacing=0><tr><td>x", "td").border_spacing,
            (0.0, 0.0),
            "inherited, so the cells can read it"
        );
    }

    /// `cellpadding` belongs to the table and pads its cells — its own, not the
    /// cells of a table nested inside one of them — and an author rule beats it.
    #[test]
    fn cellpadding_pads_the_tables_own_cells() {
        let html = "<table cellpadding=7><tr><td id=outer>\
                    <table cellpadding=0><tr><td id=inner>x</td></tr></table>";
        assert_eq!(layout_style(html, "#outer").padding.top, Length::Px(7.0));
        assert_eq!(layout_style(html, "#outer").padding.left, Length::Px(7.0));
        assert_eq!(layout_style(html, "#inner").padding.top, Length::Px(0.0));
        assert_eq!(
            layout_style("<table><tr><td>x", "td").padding.top,
            Length::Px(1.0),
            "the user-agent sheet's pixel, without one"
        );
        assert_eq!(
            layout_style(
                "<style>td { padding: 4px }</style><table cellpadding=0><tr><td>x",
                "td"
            )
            .padding
            .top,
            Length::Px(4.0)
        );
    }

    /// A cell sits in the middle of its row, unless it, or its row, says where.
    #[test]
    fn valign_reaches_the_cell() {
        let valign = |html: &str| layout_style(html, "td").vertical_align;
        assert_eq!(valign("<table><tr><td>x"), VerticalAlign::Middle);
        assert_eq!(valign("<table><tr><td valign=top>x"), VerticalAlign::Top);
        assert_eq!(
            valign("<table><tr><td valign=BOTTOM>x"),
            VerticalAlign::Bottom
        );
        assert_eq!(
            valign("<table><tr valign=bottom><td>x"),
            VerticalAlign::Bottom,
            "a row's reaches its cells through `inherit`"
        );
        assert_eq!(
            valign("<table><tbody valign=top><tr><td>x"),
            VerticalAlign::Top
        );
        assert_eq!(
            valign("<table><tr><td valign=\" top\">x"),
            VerticalAlign::Middle,
            "a keyword is the whole value"
        );
        // Under a row that says otherwise, so a cell's own `middle` has to win
        // for the answer to be it.
        assert_eq!(
            valign("<table><tr valign=top><td valign=middle>x"),
            VerticalAlign::Middle
        );
        assert_eq!(
            valign("<table><tr><td valign=baseline>x"),
            VerticalAlign::Baseline
        );
    }

    #[test]
    fn nowrap_keeps_a_cell_on_one_line() {
        assert_eq!(
            layout_style("<!doctype html><table><tr><td nowrap>x", "td").text_wrap,
            TextWrap::NoWrap
        );
        assert_eq!(
            layout_style("<!doctype html><table><tr><td nowrap width=50>x", "td").text_wrap,
            TextWrap::NoWrap
        );
        // Quirks mode lets a cell with a width in pixels wrap after all.
        assert_eq!(
            layout_style("<table><tr><td nowrap width=50>x", "td").text_wrap,
            TextWrap::Wrap
        );
        assert_eq!(
            layout_style("<table><tr><td nowrap width=50%>x", "td").text_wrap,
            TextWrap::NoWrap
        );
    }

    #[test]
    fn align_moves_the_text_and_a_tables_align_moves_the_table() {
        let align = |html: &str, selector: &str| layout_style(html, selector).text_align;
        assert_eq!(align("<div align=center>x</div>", "div"), TextAlign::Center);
        assert_eq!(align("<div align=middle>x</div>", "div"), TextAlign::Center);
        assert_eq!(align("<p align=right>x", "p"), TextAlign::End);
        assert_eq!(align("<h2 align=center>x</h2>", "h2"), TextAlign::Center);
        assert_eq!(align("<p align=middle>x", "p"), TextAlign::Start);
        assert_eq!(align("<table><tr><td align=right>x", "td"), TextAlign::End);
        assert_eq!(
            align("<table><tr><td align=absmiddle>x", "td"),
            TextAlign::Center
        );

        let centred = layout_style("<table align=center><tr><td>x", "table");
        assert_eq!(centred.margin.left, LengthOrAuto::Auto);
        assert_eq!(centred.margin.right, LengthOrAuto::Auto);
        assert_eq!(
            layout_style("<table align=left><tr><td>x", "table").float,
            crate::style::Float::Left
        );
    }

    /// The inspector names the block for what it is rather than as a `style`
    /// attribute it is not.
    #[test]
    fn the_inspector_names_the_hints() {
        use crate::cascade::{StyleSources, Styler, Viewport};

        let html = "<table><tr><td bgcolor=red style=color:blue>x";
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let mut styler = Styler::new(&document, Viewport::default(), &StyleSources::default());
        let styled = styler.style(&document);
        let cell = crate::stylo_dom::select(&document, "td").expect("a selector")[0];
        let named: Vec<(String, &str)> = styler
            .rules_for(styled.style_of(cell).expect("a styled cell"))
            .into_iter()
            .map(|rule| (rule.selector, rule.origin))
            .collect();
        assert!(named.contains(&("presentational hints".to_owned(), "attribute")));
        assert!(named.contains(&("element.style".to_owned(), "attribute")));
    }

    /// A `font`'s `size` is one of the seven sizes of old: a number on the
    /// scale, or one relative to three with a sign in front of it, clamped to the
    /// scale either way.
    #[test]
    fn font_size_is_one_of_seven() {
        let size = |attribute: &str| {
            layout_style(&format!("<p><font size={attribute}>x</font>"), "font").font_size
        };
        let keyword = |keyword: &str| {
            layout_style(
                &format!("<p><span style=font-size:{keyword}>x</span>"),
                "span",
            )
            .font_size
        };
        assert_eq!(size("+2"), 24.0);
        assert_eq!(size("7"), 48.0);
        assert_eq!(size("1"), keyword("x-small"));
        assert_eq!(size("0"), keyword("x-small"));
        assert_eq!(size("-1"), keyword("small"));
        assert_eq!(size("-9"), keyword("x-small"));
        assert_eq!(size("+10"), 48.0);
        assert_eq!(size("99999999999999999999999"), 48.0);
        assert_eq!(size("\" 5px\""), keyword("x-large"));
        // Not a size at all, so the text's own.
        assert_eq!(size("big"), 16.0);
        assert_eq!(size("+"), 16.0);
    }

    /// A `face` is a `font-family` value and nothing more: a list is a list, and
    /// anything that is not one sets nothing — least of all a second property.
    #[test]
    fn font_face_is_a_family_list_and_nothing_else() {
        let font = |face: &str| layout_style(&format!("<p><font face=\"{face}\">x</font>"), "font");
        assert_eq!(&*font("Nope, monospace").font_family, "Nope, monospace");
        let injected = font("x; display:none");
        assert_eq!(&*injected.font_family, "serif");
        assert_eq!(injected.display, Display::Inline);
        assert_eq!(
            rgba(layout_style("<p><font color=red>x</font>", "font").color),
            rgb(255, 0, 0)
        );
    }

    /// `body`'s `text` is the page's text colour, which an author's beats.
    #[test]
    fn body_text_is_the_pages_colour() {
        assert_eq!(
            rgba(layout_style("<body text=red><p>x", "p").color),
            rgb(255, 0, 0)
        );
        assert_eq!(
            rgba(layout_style("<style>p { color: blue }</style><body text=red><p>x", "p").color),
            rgb(0, 0, 255)
        );
    }

    /// `body`'s margins, in pixels: of the two attributes for a side, the first
    /// one it has is the one that counts, even when it does not parse.
    #[test]
    fn body_margin_attributes_are_its_margins() {
        let margins = |attributes: &str| {
            let body = layout_style(&format!("<body {attributes}>"), "body");
            [
                body.margin.top,
                body.margin.right,
                body.margin.bottom,
                body.margin.left,
            ]
        };
        let px = |px: f32| LengthOrAuto::Length(Length::Px(px));
        assert_eq!(
            margins("leftmargin=0 topmargin=0"),
            [px(0.0), px(8.0), px(8.0), px(0.0)]
        );
        assert_eq!(
            margins("marginwidth=3 marginheight=4 leftmargin=9"),
            [px(4.0), px(3.0), px(4.0), px(3.0)]
        );
        assert_eq!(
            margins("rightmargin=5 bottommargin=6"),
            [px(8.0), px(5.0), px(6.0), px(8.0)]
        );
        assert_eq!(
            margins("marginheight=x topmargin=2"),
            [px(8.0), px(8.0), px(8.0), px(8.0)],
            "the first attribute is the one, parsed or not"
        );
    }

    /// A rule is inset lines until a `color` or `noshade` makes it solid; its
    /// `size` is then every border's width, and otherwise the height between the
    /// two lines, or no lower line at all.
    #[test]
    fn an_hr_is_sized_and_shaded_by_its_attributes() {
        let hr = |attributes: &str| layout_style(&format!("<hr {attributes}>"), "hr");

        let plain = hr("");
        assert_eq!(plain.border.top.style, BorderStyle::Inset);
        assert_eq!(plain.border.bottom.width, 1.0);
        assert_eq!(plain.height, Size::Auto);
        assert_eq!(rgba(plain.color), rgb(128, 128, 128));

        let noshade = hr("noshade size=6");
        assert_eq!(noshade.border.top.style, BorderStyle::Solid);
        assert_eq!(noshade.border.left.width, 3.0);
        assert_eq!(noshade.height, Size::Auto);

        let coloured = hr("color=red");
        assert_eq!(coloured.border.top.style, BorderStyle::Solid);
        assert_eq!(rgba(coloured.color), rgb(255, 0, 0));
        assert_eq!(rgba(coloured.border.top.color), rgb(255, 0, 0));

        assert_eq!(hr("size=5").height, Size::Length(Length::Px(3.0)));
        assert_eq!(hr("size=1").border.bottom.width, 0.0);
        assert_eq!(hr("size=1").border.top.width, 1.0);
        assert_eq!(hr("width=50%").width, Size::Length(Length::Percent(0.5)));

        let left = hr("align=left width=100");
        assert_eq!(left.margin.left, LengthOrAuto::Length(Length::Px(0.0)));
        assert_eq!(left.margin.right, LengthOrAuto::Auto);
        let right = hr("align=RIGHT");
        assert_eq!(right.margin.left, LengthOrAuto::Auto);
        assert_eq!(right.margin.right, LengthOrAuto::Length(Length::Px(0.0)));
        assert_eq!(hr("").margin.left, LengthOrAuto::Auto);
    }

    /// `pre wrap` keeps the spaces and breaks the lines.
    #[test]
    fn pre_wrap_wraps() {
        let pre = layout_style("<pre wrap>x</pre>", "pre");
        assert_eq!(pre.white_space, WhiteSpace::Preserve);
        assert_eq!(pre.text_wrap, TextWrap::Wrap);
        assert_eq!(
            layout_style("<pre>x</pre>", "pre").text_wrap,
            TextWrap::NoWrap
        );
    }

    /// `br clear` names the side, `all` and `both` alike; anything else is
    /// nothing.
    #[test]
    fn br_clear_is_clear() {
        let clear = |value: &str| layout_style(&format!("<p>x<br clear={value}>y"), "br").clear;
        assert_eq!(clear("left"), Clear::Left);
        assert_eq!(clear("Right"), Clear::Right);
        assert_eq!(clear("all"), Clear::Both);
        assert_eq!(clear("both"), Clear::Both);
        assert_eq!(clear("none"), Clear::None);
    }

    /// A `background` attribute is a picture at an address resolved against the
    /// document's base, on the page and on a table's parts and nowhere else; an
    /// author's rule beats it.
    #[test]
    fn a_background_attribute_is_a_picture_at_the_documents_base() {
        let base = "https://x.test/dir/page.html";
        let picture = |html: &str, selector: &str| {
            layout_style_at(html, base, selector)
                .backgrounds
                .first()
                .and_then(|layer| layer.image.as_deref().map(str::to_owned))
        };
        let tile = Some("https://x.test/dir/tile.png".to_owned());
        for (html, selector) in [
            ("<body background=tile.png>", "body"),
            ("<table background=tile.png><tr><td>x", "table"),
            ("<table><tbody background=tile.png><tr><td>x", "tbody"),
            ("<table><tr background=tile.png><td>x", "tr"),
            ("<table><tr><td background=tile.png>x", "td"),
            ("<table><tr><th background=tile.png>x", "th"),
        ] {
            assert_eq!(picture(html, selector), tile, "{html}");
        }
        assert_eq!(
            picture("<body background=/up.png>", "body").as_deref(),
            Some("https://x.test/up.png")
        );
        assert_eq!(picture("<div background=tile.png>x</div>", "div"), None);
        assert_eq!(picture("<body background>", "body"), None, "empty is none");
        // An address and nothing else: what would end a `url()` is part of it.
        assert_eq!(
            picture("<body background=\"a.png) ; color: red\">", "body").as_deref(),
            Some("https://x.test/dir/a.png)%20;%20color:%20red")
        );
        assert_eq!(
            picture(
                "<style>body { background-image: none }</style><body background=tile.png>",
                "body"
            ),
            None
        );
    }

    /// Embedded content's `width` and `height` are its dimension properties —
    /// an `img`'s, an `object`'s, an `embed`'s, a frame's, a video's, and an
    /// image button's while it has a picture to show — and a stylesheet's
    /// `height: auto` beats them. A canvas's are its bitmap's size and not CSS
    /// at all.
    #[test]
    fn embedded_content_is_sized_by_its_attributes() {
        let px = |px: f32| Size::Length(Length::Px(px));
        for (html, selector) in [
            ("<img width=40 height=20>", "img"),
            ("<object width=40 height=20></object>", "object"),
            ("<embed width=40 height=20>", "embed"),
            ("<iframe width=40 height=20></iframe>", "iframe"),
            ("<video width=40 height=20></video>", "video"),
            ("<input type=image src=a.png width=40 height=20>", "input"),
        ] {
            let style = layout_style(html, selector);
            assert_eq!((style.width, style.height), (px(40.0), px(20.0)), "{html}");
        }
        assert_eq!(
            layout_style("<img width=50%>", "img").width,
            Size::Length(Length::Percent(0.5))
        );
        assert_eq!(layout_style("<img width=0>", "img").width, px(0.0));
        assert_eq!(
            layout_style("<img width=800 height=400 style=height:auto>", "img").height,
            Size::Auto
        );
        let canvas = layout_style("<canvas width=100 height=50></canvas>", "canvas");
        assert_eq!((canvas.width, canvas.height), (Size::Auto, Size::Auto));
        assert_eq!(
            layout_style("<input type=image alt=Go width=40>", "input").width,
            Size::Auto,
            "a button of text is as wide as its text"
        );
        assert_eq!(layout_style("<p width=40>x", "p").width, Size::Auto);
    }

    /// `hspace` and `vspace` are the margins either side, and `border` a solid
    /// line in the element's own colour — on an `img`, an `object` and an image
    /// button, and not on a frame.
    #[test]
    fn a_pictures_space_and_border_are_its_margins_and_border() {
        let px = |px: f32| LengthOrAuto::Length(Length::Px(px));
        for (html, selector) in [
            ("<img hspace=10 vspace=5 border=2>", "img"),
            ("<object hspace=10 vspace=5 border=2></object>", "object"),
            ("<input type=image hspace=10 vspace=5 border=2>", "input"),
        ] {
            let style = layout_style(html, selector);
            assert_eq!(
                [
                    style.margin.top,
                    style.margin.right,
                    style.margin.bottom,
                    style.margin.left
                ],
                [px(5.0), px(10.0), px(5.0), px(10.0)],
                "{html}"
            );
            assert_eq!(style.border.left.width, 2.0, "{html}");
            assert_eq!(style.border.left.style, BorderStyle::Solid, "{html}");
        }
        let embed = layout_style("<embed hspace=10 border=2>", "embed");
        assert_eq!(embed.margin.left, px(10.0));
        assert_eq!(embed.border.left.width, 0.0, "an embed takes no border");
        let frame = layout_style("<iframe hspace=10></iframe>", "iframe");
        assert_eq!(frame.margin.left, px(0.0), "nor a frame any space");
        for zero in ["border=0", "border=none"] {
            assert_eq!(
                layout_style(&format!("<img {zero}>"), "img")
                    .border
                    .left
                    .style,
                BorderStyle::None,
                "{zero}"
            );
        }
    }

    /// `align` floats a picture to either side or puts it somewhere in its
    /// line; on a video it means nothing.
    #[test]
    fn an_embedded_elements_align_floats_it_or_aligns_it() {
        let style = |html: &str, selector: &str| layout_style(html, selector);
        assert_eq!(
            style("<img align=left>", "img").float,
            crate::style::Float::Left
        );
        assert_eq!(
            style("<iframe align=RIGHT></iframe>", "iframe").float,
            crate::style::Float::Right
        );
        for (align, expected) in [
            ("top", VerticalAlign::Top),
            ("texttop", VerticalAlign::TextTop),
            ("absmiddle", VerticalAlign::Middle),
            ("abscenter", VerticalAlign::Middle),
            ("middle", VerticalAlign::Middle),
            ("center", VerticalAlign::Middle),
            ("bottom", VerticalAlign::Bottom),
            ("baseline", VerticalAlign::Baseline),
        ] {
            assert_eq!(
                style(&format!("<p><img align={align}>"), "img").vertical_align,
                expected,
                "{align}"
            );
        }
        assert_eq!(
            style("<video align=left></video>", "video").float,
            crate::style::Float::None
        );
    }

    /// A frame's `frameborder` takes the user-agent sheet's border away when it
    /// is zero or not a number at all, and leaves it otherwise.
    #[test]
    fn frameborder_zero_takes_the_frames_border_away() {
        let border = |attribute: &str| {
            layout_style(&format!("<iframe {attribute}></iframe>"), "iframe")
                .border
                .top
                .width
        };
        assert_eq!(border(""), 2.0);
        assert_eq!(border("frameborder=0"), 0.0);
        assert_eq!(border("frameborder=no"), 0.0);
        assert_eq!(border("frameborder=0px"), 0.0);
        assert_eq!(border("frameborder=1"), 2.0);
        assert_eq!(border("frameborder=-1"), 2.0);
    }

    /// The style an author writes beats every hint, which is the reason they are
    /// an origin of their own rather than part of the user-agent sheet.
    #[test]
    fn an_author_rule_beats_a_hint() {
        assert_eq!(
            layout_style(
                "<style>table { width: 100px }</style><table width=300><tr><td>x",
                "table"
            )
            .width,
            Size::Length(Length::Px(100.0))
        );
    }
}
