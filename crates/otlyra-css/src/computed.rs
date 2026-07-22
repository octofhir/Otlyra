//! Turning the cascade's answer into the handful of values layout reads.
//!
//! The style engine computes the whole of CSS; layout currently understands twelve
//! properties. This is where the two meet, and it is deliberately a narrowing: a
//! property that layout cannot honour is one this file does not pretend to carry.
//! Widening it is how new CSS support arrives — one property, one conversion, one
//! thing layout does with it.

use std::sync::Arc;

use peniko::Color;
use style::properties::ComputedValues;

use crate::style::{
    AlignItems, Anchor, BackgroundPosition, BackgroundRepeat, BackgroundSize, Border,
    BorderCollapse, BoxSizing, Clear, ComputedStyle, Corners, Display, FlexDirection, FlexWrap,
    Float, FontStyle, Gradient, GradientStop, JustifyContent, Length, LengthOrAuto, LineHeight,
    ObjectFit, Overflow, Placement, Position, Repeat, Shadow, Sides, TextAlign, TextDecoration,
    TextWrap, Track, TransformOp, TransformOrigin, WhiteSpace,
};

/// Convert one element's computed values into the style layout reads.
pub fn to_layout_style(values: &ComputedValues) -> ComputedStyle {
    let font = values.get_font();
    let font_size = font.font_size.used_size().px();

    ComputedStyle {
        display: display_of(values),
        color: colour(values.clone_color()),
        background_color: values
            .get_background()
            .background_color
            .as_absolute()
            .map_or(Color::TRANSPARENT, |value| colour(*value)),
        font_family: font_family(values),
        font_size,
        font_weight: font.font_weight.value().round() as u16,
        font_style: if font.font_style == style::values::computed::FontStyle::NORMAL {
            FontStyle::Normal
        } else {
            FontStyle::Italic
        },
        font_width: font.font_stretch.to_percentage().0 * 100.0,
        optical_sizing: optical_sizing(values),
        font_variations: font_variations(values),
        letter_spacing: values
            .get_inherited_text()
            .letter_spacing
            .0
            .resolve(style::values::computed::Length::new(font_size))
            .px(),
        word_spacing: values
            .get_inherited_text()
            .word_spacing
            .resolve(style::values::computed::Length::new(font_size))
            .px(),
        line_height: line_height(values),
        list_style: list_style(values),
        vertical_align: vertical_align(values),
        border_spacing: {
            let spacing = &values.get_inherited_table().border_spacing;
            (
                spacing.horizontal().to_f32_px(),
                spacing.vertical().to_f32_px(),
            )
        },
        border_collapse: match values.get_inherited_table().border_collapse {
            style::computed_values::border_collapse::T::Collapse => BorderCollapse::Collapse,
            style::computed_values::border_collapse::T::Separate => BorderCollapse::Separate,
        },
        box_sizing: match values.get_position().box_sizing {
            style::computed_values::box_sizing::T::BorderBox => BoxSizing::Border,
            style::computed_values::box_sizing::T::ContentBox => BoxSizing::Content,
        },
        opacity: values.get_effects().opacity.clamp(0.0, 1.0),
        transform: transform(values),
        transform_origin: transform_origin(values),
        white_space: white_space(values),
        text_wrap: text_wrap(values),
        text_decoration: text_decoration(values),
        margin: Sides {
            top: length_or_auto(&values.get_margin().margin_top),
            right: length_or_auto(&values.get_margin().margin_right),
            bottom: length_or_auto(&values.get_margin().margin_bottom),
            left: length_or_auto(&values.get_margin().margin_left),
        },
        padding: Sides {
            top: length(&values.get_padding().padding_top.0),
            right: length(&values.get_padding().padding_right.0),
            bottom: length(&values.get_padding().padding_bottom.0),
            left: length(&values.get_padding().padding_left.0),
        },
        border: border(values),
        text_align: text_align(values),
        width: size(&values.get_position().width),
        height: size(&values.get_position().height),
        min_width: min_size(&values.get_position().min_width),
        max_width: max_size(&values.get_position().max_width),
        min_height: min_size(&values.get_position().min_height),
        max_height: max_size(&values.get_position().max_height),
        float: float_of(values),
        clear: clear_of(values),
        position: position_of(values),
        inset: Sides {
            top: inset(&values.get_position().top),
            right: inset(&values.get_position().right),
            bottom: inset(&values.get_position().bottom),
            left: inset(&values.get_position().left),
        },
        z_index: match values.clone_z_index() {
            style::values::computed::ZIndex::Integer(value) => Some(value),
            style::values::computed::ZIndex::Auto => None,
        },
        overflow: overflow_of(values),
        radius: corners(values),
        background_gradient: background_gradient(values),
        background_image: background_image(values),
        background_size: background_size(values),
        background_repeat: background_repeat(values),
        background_position: background_position(values),
        object_fit: object_fit(values),
        object_position: object_position(values),
        shadows: shadows(values),
        text_shadows: text_shadows(values),
        grid_columns: tracks(&values.get_position().grid_template_columns),
        grid_rows: tracks(&values.get_position().grid_template_rows),
        grid_columns_fill: auto_repeat(&values.get_position().grid_template_columns),
        grid_column: placement(
            &values.get_position().grid_column_start,
            &values.get_position().grid_column_end,
        ),
        grid_row: placement(
            &values.get_position().grid_row_start,
            &values.get_position().grid_row_end,
        ),
        flex_direction: flex_direction(values),
        flex_wrap: flex_wrap(values),
        justify_content: justify_content(values),
        align_items: align_items(values),
        align_self: align_self(values),
        flex_grow: values.clone_flex_grow().0,
        flex_shrink: values.clone_flex_shrink().0,
        flex_basis: flex_basis(values),
        gap: (
            gap(&values.get_position().row_gap),
            gap(&values.get_position().column_gap),
        ),
    }
}

/// `display`, narrowed to the three values layout has formatting contexts for.
///
/// Everything with an inline outer display becomes inline and everything else
/// becomes block. That is a real approximation — a flex container laid out as a
/// block is wrong — but it is wrong in the direction of showing the content, and
/// `display: none` is not approximated at all.
fn display_of(values: &ComputedValues) -> Display {
    use style::values::computed::Display as Computed;
    use style::values::specified::box_::{DisplayInside, DisplayOutside};

    let display = values.clone_display();
    if display == Computed::None {
        return Display::None;
    }
    // A flex container is a flex container whichever way round it sits in its
    // parent; `inline-flex` differs in how it is placed, which is a difference
    // layout cannot express yet.
    if display.inside() == DisplayInside::Flex {
        return Display::Flex;
    }
    if display.inside() == DisplayInside::Grid {
        return Display::Grid;
    }
    if let Some(part) = table_part(display) {
        return part;
    }
    if display.outside() == DisplayOutside::Inline {
        // `inline-block` is a block container that sits in a line: inline outside,
        // a formatting context of its own inside. The difference from `inline` is
        // the whole of what a page uses it for — a width, a height and a padding
        // that push the line around rather than being ignored.
        if display.inside() == DisplayInside::FlowRoot {
            Display::InlineBlock
        } else {
            Display::Inline
        }
    } else {
        Display::Block
    }
}

/// The table displays, which are a formatting context of their own rather than a
/// block that happens to hold rows.
fn table_part(display: style::values::computed::Display) -> Option<Display> {
    use style::values::specified::box_::{DisplayInside, DisplayOutside};

    Some(match display.inside() {
        DisplayInside::Table => Display::Table,
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup => Display::TableRowGroup,
        DisplayInside::TableRow => Display::TableRow,
        DisplayInside::TableCell => Display::TableCell,
        // A column and a column group draw nothing and place nothing; what they
        // carry is width and background, which auto layout takes from the cells.
        DisplayInside::TableColumn | DisplayInside::TableColumnGroup => Display::None,
        _ => {
            return (display.outside() == DisplayOutside::TableCaption)
                .then_some(Display::TableCaption);
        }
    })
}

/// A computed colour, in the paint vocabulary.
fn colour(value: style::color::AbsoluteColor) -> Color {
    let srgb = value.to_color_space(style::color::ColorSpace::Srgb);
    Color::new([
        srgb.components.0,
        srgb.components.1,
        srgb.components.2,
        srgb.alpha,
    ])
}

/// A colour that may be `currentColor`, resolved against the element's own colour.
fn resolve_colour(value: &style::values::computed::Color, current: Color) -> Color {
    value.as_absolute().map_or(current, |value| colour(*value))
}

/// The font stack, as the CSS source list the text layer parses.
fn font_family(values: &ComputedValues) -> Arc<str> {
    use style::values::computed::font::{GenericFontFamily, SingleFontFamily};

    let families: Vec<String> = values
        .get_font()
        .font_family
        .families
        .iter()
        .filter_map(|family| match family {
            SingleFontFamily::FamilyName(name) => Some(name.name.to_string()),
            SingleFontFamily::Generic(generic) => match generic {
                GenericFontFamily::Serif => Some("serif".to_owned()),
                GenericFontFamily::SansSerif => Some("sans-serif".to_owned()),
                GenericFontFamily::Monospace => Some("monospace".to_owned()),
                GenericFontFamily::Cursive => Some("cursive".to_owned()),
                GenericFontFamily::Fantasy => Some("fantasy".to_owned()),
                GenericFontFamily::SystemUi => Some("system-ui".to_owned()),
                // Not a family: the engine's placeholder for "whatever the browser
                // calls its standard font", which is the initial value and so is
                // what every element that says nothing has.
                GenericFontFamily::None => None,
            },
        })
        .collect();

    if families.is_empty() {
        // The standard font, which every browser sets to a serif — and which
        // `medium` is sixteen pixels of, the pair being two halves of one
        // preference. A page that says nothing about its font should look like the
        // same page does everywhere else.
        Arc::from("serif")
    } else {
        Arc::from(families.join(", "))
    }
}

fn line_height(values: &ComputedValues) -> LineHeight {
    use style::values::computed::LineHeight as Computed;

    match values.clone_line_height() {
        Computed::Normal => LineHeight::Normal,
        Computed::Number(number) => LineHeight::Number(number.0),
        Computed::Length(length) => LineHeight::Px(length.px()),
    }
}

fn white_space(values: &ComputedValues) -> WhiteSpace {
    use style::properties::longhands::white_space_collapse::computed_value::T as WhiteSpaceCollapse;

    match values.clone_white_space_collapse() {
        WhiteSpaceCollapse::Collapse => WhiteSpace::Collapse,
        WhiteSpaceCollapse::PreserveBreaks => WhiteSpace::PreserveBreaks,
        WhiteSpaceCollapse::BreakSpaces => WhiteSpace::BreakSpaces,
        // `preserve` and `preserve-spaces`, which differ only in what they do
        // with a line ending — and `preserve-spaces` is not a value any of
        // `white-space`'s own shorthands produce.
        _ => WhiteSpace::Preserve,
    }
}

/// Whether a line may be broken at all.
///
/// The half of `white-space` that `white-space-collapse` cannot say. Without it
/// `nowrap` collapses like `normal` — which it does — and then wraps like
/// `normal` too, which is the whole of what it was written to prevent.
fn text_wrap(values: &ComputedValues) -> TextWrap {
    use style::properties::longhands::text_wrap_mode::computed_value::T as Mode;

    match values.clone_text_wrap_mode() {
        Mode::Wrap => TextWrap::Wrap,
        Mode::Nowrap => TextWrap::NoWrap,
    }
}

/// The four borders, each as the width that is actually used and its colour.
///
/// A border whose style is `none` or `hidden` has a used width of zero however
/// wide it was declared, which is the rule that keeps `border-style` out of the
/// computed style this crate hands to layout.
fn border(values: &ComputedValues) -> Sides<Border> {
    use style::values::computed::BorderStyle;

    let border = values.get_border();
    let text = colour(values.clone_color());
    let side =
        |width: &style::values::computed::BorderSideWidth, style: BorderStyle, colour_value| {
            if matches!(style, BorderStyle::None | BorderStyle::Hidden) {
                return Border::NONE;
            }
            Border {
                width: width.0.to_f32_px(),
                color: resolve_colour(colour_value, text),
            }
        };

    Sides {
        top: side(
            &border.border_top_width,
            border.border_top_style,
            &border.border_top_color,
        ),
        right: side(
            &border.border_right_width,
            border.border_right_style,
            &border.border_right_color,
        ),
        bottom: side(
            &border.border_bottom_width,
            border.border_bottom_style,
            &border.border_bottom_color,
        ),
        left: side(
            &border.border_left_width,
            border.border_left_style,
            &border.border_left_color,
        ),
    }
}

fn text_align(values: &ComputedValues) -> TextAlign {
    use style::values::computed::TextAlign as Computed;

    match values.clone_text_align() {
        Computed::Center | Computed::MozCenter => TextAlign::Center,
        Computed::Right | Computed::End | Computed::MozRight => TextAlign::End,
        // `justify` spaces a line out, which inline layout does not do; its start
        // edge is where a start-aligned line begins, so that is what it gets.
        _ => TextAlign::Start,
    }
}

/// `overflow`, narrowed to whether the box cuts off what does not fit.
///
/// `scroll` and `auto` cut off too — the part of them that layout can honour today
/// is the clip; scrolling the box itself is a scroll port, which is more than a
/// clip and arrives with one.
/// `background-image`, when the topmost layer is a picture.
fn background_image(values: &ComputedValues) -> Option<Arc<str>> {
    use style::values::generics::image::GenericImage as Image;

    let Image::Url(url) = values.get_background().background_image.0.first()? else {
        return None;
    };
    // The address as written, not as resolved: stylesheets are parsed against a
    // placeholder base, so the engine cannot resolve a relative `url()` for us.
    // Resolving it against the page is the caller's, exactly as it is for the
    // address in an `<img src>`.
    Some(match url {
        style::url::ComputedUrl::Valid(resolved) => Arc::from(resolved.as_str()),
        style::url::ComputedUrl::Invalid(specified) => Arc::from(specified.as_str()),
    })
}

/// `transform`, narrowed to the steps this draws in.
///
/// Two dimensions. A page that turns a card in three is drawing something this
/// cannot draw, and the flat part of it — which is what a `rotate3d` about the z
/// axis or a `translate3d` along x and y is — is taken and the rest dropped,
/// rather than the whole rule being ignored.
fn transform(values: &ComputedValues) -> Arc<[TransformOp]> {
    use style::values::computed::transform::TransformOperation as Op;

    let operations = &values.get_box().transform.0;
    if operations.is_empty() {
        return Arc::from(Vec::new());
    }

    let radians = |angle: &style::values::computed::Angle| angle.radians();
    let steps: Vec<TransformOp> = operations
        .iter()
        .filter_map(|operation| {
            Some(match operation {
                Op::Matrix(matrix) => TransformOp::Matrix([
                    matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f,
                ]),
                Op::Translate(x, y) => TransformOp::Translate(length(x), length(y)),
                Op::TranslateX(x) => TransformOp::Translate(length(x), Length::ZERO),
                Op::TranslateY(y) => TransformOp::Translate(Length::ZERO, length(y)),
                Op::Translate3D(x, y, _) => TransformOp::Translate(length(x), length(y)),
                Op::Scale(x, y) => TransformOp::Scale(*x, *y),
                Op::ScaleX(x) => TransformOp::Scale(*x, 1.0),
                Op::ScaleY(y) => TransformOp::Scale(1.0, *y),
                Op::Scale3D(x, y, _) => TransformOp::Scale(*x, *y),
                Op::Rotate(angle) | Op::RotateZ(angle) => TransformOp::Rotate(radians(angle)),
                Op::Skew(x, y) => TransformOp::Skew(radians(x), radians(y)),
                Op::SkewX(x) => TransformOp::Skew(radians(x), 0.0),
                Op::SkewY(y) => TransformOp::Skew(0.0, radians(y)),
                _ => return None,
            })
        })
        .collect();

    Arc::from(steps)
}

/// `transform-origin`, in the two axes that matter here.
fn transform_origin(values: &ComputedValues) -> TransformOrigin {
    let origin = &values.get_box().transform_origin;
    TransformOrigin {
        x: length(&origin.horizontal),
        y: length(&origin.vertical),
    }
}

/// `background-size` for that layer.
fn background_size(values: &ComputedValues) -> BackgroundSize {
    use style::values::generics::background::GenericBackgroundSize as Generic;

    match values.get_background().background_size.0.first() {
        Some(Generic::Cover) => BackgroundSize::Cover,
        Some(Generic::Contain) => BackgroundSize::Contain,
        Some(Generic::ExplicitSize { width, height }) => {
            let side = |value: &style::values::computed::NonNegativeLengthPercentageOrAuto| {
                match value {
                    style::values::generics::length::GenericLengthPercentageOrAuto::Auto => None,
                    style::values::generics::length::GenericLengthPercentageOrAuto::LengthPercentage(
                        value,
                    ) => Some(length(&value.0)),
                }
            };
            match (side(width), side(height)) {
                (Some(width), Some(height)) => BackgroundSize::Fixed(width, height),
                // One side given and the other from the picture's own ratio is
                // sizing this does not do; its own size is the honest stand-in.
                _ => BackgroundSize::Auto,
            }
        }
        _ => BackgroundSize::Auto,
    }
}

/// `vertical-align`, in every value it can take.
fn vertical_align(values: &ComputedValues) -> crate::style::VerticalAlign {
    use crate::style::VerticalAlign;
    use style::values::computed::box_::AlignmentBaseline;
    use style::values::generics::box_::{BaselineShift, BaselineShiftKeyword as Keyword};

    // The engine models this as CSS Inline 3 does: `vertical-align` is a shorthand
    // of two properties. `baseline-shift` carries the values that move a box along
    // the block axis, and `alignment-baseline` carries the ones that pick a
    // *different baseline* to align to — which is where `text-top`, `text-bottom`
    // and `middle` went. Reading only the first was how those three arrived as
    // `baseline` and did nothing.
    match values.get_box().alignment_baseline {
        AlignmentBaseline::TextTop => return VerticalAlign::TextTop,
        AlignmentBaseline::TextBottom => return VerticalAlign::TextBottom,
        AlignmentBaseline::Middle => {
            return VerticalAlign::Middle;
        }
        _ => {}
    }

    match &values.get_box().baseline_shift {
        BaselineShift::Keyword(Keyword::Sub) => VerticalAlign::Sub,
        BaselineShift::Keyword(Keyword::Super) => VerticalAlign::Super,
        BaselineShift::Length(value) => match value.to_percentage() {
            Some(percentage) if percentage.0 != 0.0 => VerticalAlign::Percent(percentage.0),
            Some(_) => VerticalAlign::Baseline,
            // The initial value is a zero length rather than a keyword, and a box
            // that does not move is worth saying so about: everything downstream
            // can then skip the arithmetic for the common case.
            None => match value.to_used_value(app_units::Au(0)).to_f32_px() {
                0.0 => VerticalAlign::Baseline,
                px => VerticalAlign::Length(px),
            },
        },
        BaselineShift::Keyword(Keyword::Top) => VerticalAlign::Top,
        BaselineShift::Keyword(Keyword::Bottom) => VerticalAlign::Bottom,
        BaselineShift::Keyword(Keyword::Center) => VerticalAlign::Middle,
    }
}

/// `list-style-type`, in the counters we can draw.
///
/// A named counter style we do not know — and the whole of `@counter-style` — is
/// taken as a disc. Drawing nothing would lose the reader their place in the list;
/// drawing the wrong shape only loses them the author's choice of it.
fn list_style(values: &ComputedValues) -> crate::style::ListStyle {
    use crate::style::ListStyle;
    use style::counter_style::CounterStyle;

    match &values.get_list().list_style_type.0 {
        CounterStyle::None => ListStyle::None,
        CounterStyle::Name(name) => match name.0.as_ref() {
            "none" => ListStyle::None,
            "circle" => ListStyle::Circle,
            "square" => ListStyle::Square,
            "decimal" => ListStyle::Decimal,
            "lower-alpha" | "lower-latin" => ListStyle::LowerAlpha,
            "upper-alpha" | "upper-latin" => ListStyle::UpperAlpha,
            "lower-roman" => ListStyle::LowerRoman,
            "upper-roman" => ListStyle::UpperRoman,
            _ => ListStyle::Disc,
        },
        _ => ListStyle::Disc,
    }
}

/// `font-optical-sizing`, which is `auto` unless a page turns it off.
fn optical_sizing(values: &ComputedValues) -> bool {
    use style::computed_values::font_optical_sizing::T as Computed;

    values.get_font().font_optical_sizing == Computed::Auto
}

/// `font-variation-settings`: the axes a page names and the values it wants.
///
/// Shared rather than copied, so an element that merely inherits the property —
/// which is every element on every page that uses it, and every element on every
/// page that does not — costs a refcount rather than a list.
fn font_variations(values: &ComputedValues) -> Arc<[([u8; 4], f32)]> {
    let settings = &values.get_font().font_variation_settings.0;
    if settings.is_empty() {
        return Arc::from([] as [([u8; 4], f32); 0]);
    }
    settings
        .iter()
        .map(|setting| (setting.tag.0.to_be_bytes(), setting.value))
        .collect()
}

/// Whether the topmost background layer repeats, along each axis.
///
/// `space` — tiles spread apart so a whole number fits with gaps between them —
/// is taken as `repeat`. It needs a step that is wider than the tile, which the
/// one fill a tiled background is drawn with cannot express.
fn background_repeat(values: &ComputedValues) -> BackgroundRepeat {
    use style::values::specified::background::BackgroundRepeatKeyword as Keyword;

    let axis = |keyword: Keyword| match keyword {
        Keyword::NoRepeat => Repeat::None,
        Keyword::Round => Repeat::Round,
        _ => Repeat::Repeat,
    };

    match values.get_background().background_repeat.0.first() {
        Some(repeat) => BackgroundRepeat {
            x: axis(repeat.0),
            y: axis(repeat.1),
        },
        None => BackgroundRepeat::REPEAT,
    }
}

/// Where the topmost background layer sits in the box it is behind.
///
/// The computed value is a length, a percentage, or the sum of both — `right 10px`
/// is `calc(100% - 10px)` by the time it gets here — and all three are the same
/// affine function of the room the picture leaves. So each is measured rather than
/// taken apart: what it gives for no room at all is the length, and how much it
/// moves per unit of room is the fraction. A `calc()` that clamps is not affine and
/// is the one shape this reads wrongly; none of the keywords produce one.
fn background_position(values: &ComputedValues) -> BackgroundPosition {
    use style::values::computed::Length;

    let background = values.get_background();
    let anchor = |value: Option<&style::values::computed::LengthPercentage>| match value {
        Some(value) => {
            let offset = value.resolve(Length::new(0.0)).px();
            Anchor {
                fraction: value.resolve(Length::new(1.0)).px() - offset,
                offset,
            }
        }
        None => Anchor::START,
    };

    BackgroundPosition {
        x: anchor(background.background_position_x.0.first()),
        y: anchor(background.background_position_y.0.first()),
    }
}

/// `object-fit`, as the box tree spells it.
fn object_fit(values: &ComputedValues) -> ObjectFit {
    match values.get_position().object_fit {
        style::computed_values::object_fit::T::Fill => ObjectFit::Fill,
        style::computed_values::object_fit::T::Contain => ObjectFit::Contain,
        style::computed_values::object_fit::T::Cover => ObjectFit::Cover,
        style::computed_values::object_fit::T::None => ObjectFit::None,
        style::computed_values::object_fit::T::ScaleDown => ObjectFit::ScaleDown,
    }
}

/// `object-position`, which is `background-position`'s arithmetic with a
/// different starting value.
fn object_position(values: &ComputedValues) -> BackgroundPosition {
    use style::values::computed::Length;

    let position = &values.get_position().object_position;
    let anchor = |value: &style::values::computed::LengthPercentage| {
        let offset = value.resolve(Length::new(0.0)).px();
        Anchor {
            fraction: value.resolve(Length::new(1.0)).px() - offset,
            offset,
        }
    };

    BackgroundPosition {
        x: anchor(&position.horizontal),
        y: anchor(&position.vertical),
    }
}

/// `box-shadow`, outer only.
///
/// An inset shadow is drawn inside the box against its own edges, which is a
/// different shape from the one this draws and is left out rather than approximated.
/// The list comes back in painting order: CSS paints the first-written shadow on
/// top, so the last one is drawn first.
fn shadows(values: &ComputedValues) -> Vec<Shadow> {
    let current = colour_of(values);
    values
        .get_effects()
        .box_shadow
        .0
        .iter()
        .filter(|shadow| !shadow.inset)
        .map(|shadow| Shadow {
            x: shadow.base.horizontal.px(),
            y: shadow.base.vertical.px(),
            blur: shadow.base.blur.0.px(),
            spread: shadow.spread.px(),
            color: resolve_colour(&shadow.base.color, current),
        })
        .rev()
        .collect()
}

/// `text-shadow`, in painting order like `box-shadow`.
///
/// No spread: a text shadow is the glyphs themselves, softened and moved, and
/// there is nothing to grow.
fn text_shadows(values: &ComputedValues) -> Vec<Shadow> {
    let current = colour_of(values);
    values
        .get_inherited_text()
        .text_shadow
        .0
        .iter()
        .map(|shadow| Shadow {
            x: shadow.horizontal.px(),
            y: shadow.vertical.px(),
            blur: shadow.blur.0.px(),
            spread: 0.0,
            color: resolve_colour(&shadow.color, current),
        })
        .rev()
        .collect()
}

/// The element's own `color`, which is what `currentColor` means.
fn colour_of(values: &ComputedValues) -> Color {
    colour(values.clone_color())
}

/// `background-image`, when the topmost layer of it is a linear gradient.
///
/// One layer, because a box with several backgrounds is a box that needs the
/// painting order for them, and the top one is what a page means when it names two.
fn background_gradient(values: &ComputedValues) -> Option<Gradient> {
    use style::values::computed::image::LineDirection;
    use style::values::generics::image::{
        GenericGradient, GenericGradientItem as Item, GenericImage as Image,
    };

    let images = &values.get_background().background_image.0;
    let Image::Gradient(gradient) = images.first()? else {
        return None;
    };
    let GenericGradient::Linear {
        direction, items, ..
    } = &**gradient
    else {
        return None;
    };

    // CSS measures the angle clockwise from up; a corner keyword is turned into the
    // angle that points at it, which is exact for a square box and close enough for
    // one that is not — the alternative needs the box's own proportions, which the
    // cascade does not have.
    let angle = match direction {
        LineDirection::Angle(angle) => angle.radians(),
        LineDirection::Horizontal(side) => match side {
            style::values::specified::position::HorizontalPositionKeyword::Left => {
                -std::f32::consts::FRAC_PI_2
            }
            _ => std::f32::consts::FRAC_PI_2,
        },
        LineDirection::Vertical(side) => match side {
            style::values::specified::position::VerticalPositionKeyword::Top => 0.0,
            _ => std::f32::consts::PI,
        },
        LineDirection::Corner(horizontal, vertical) => {
            use style::values::specified::position::{
                HorizontalPositionKeyword as H, VerticalPositionKeyword as V,
            };
            match (horizontal, vertical) {
                (H::Right, V::Top) => std::f32::consts::FRAC_PI_4,
                (H::Right, V::Bottom) => 3.0 * std::f32::consts::FRAC_PI_4,
                (H::Left, V::Bottom) => 5.0 * std::f32::consts::FRAC_PI_4,
                (H::Left, V::Top) => 7.0 * std::f32::consts::FRAC_PI_4,
            }
        }
    };

    // Stops without a position are spread evenly, which is what CSS does when a
    // gradient names only its colours.
    let colours: Vec<(Color, Option<f32>)> = items
        .iter()
        .filter_map(|item| match item {
            Item::SimpleColorStop(colour) => Some((resolve_colour(colour, Color::BLACK), None)),
            Item::ComplexColorStop { color, position } => Some((
                resolve_colour(color, Color::BLACK),
                position.to_percentage().map(|percentage| percentage.0),
            )),
            Item::InterpolationHint(_) => None,
        })
        .collect();
    if colours.len() < 2 {
        return None;
    }

    let last = colours.len() - 1;
    let stops = colours
        .into_iter()
        .enumerate()
        .map(|(index, (color, at))| GradientStop {
            at: at.unwrap_or(index as f32 / last as f32),
            color,
        })
        .collect();

    Some(Gradient { angle, stops })
}

/// The tracks a `grid-template-*` names.
///
/// `repeat()` with a count is expanded here, where the count is known; `auto-fill`
/// and `auto-fit` depend on the container's size and are left for layout, which
/// does not do them yet and treats them as one track.
fn tracks(template: &style::values::computed::GridTemplateComponent) -> Vec<Track> {
    use style::values::generics::grid::{
        GenericTrackListValue as ListValue, GenericTrackSize as Size, RepeatCount,
        TrackBreadth as Breadth,
    };

    let breadth = |value: &Breadth<style::values::computed::LengthPercentage>| match value {
        Breadth::Breadth(length) => Track::Fixed(length_percentage(length)),
        Breadth::Flex(flex) => Track::Fraction(flex.0),
        _ => Track::Auto,
    };
    let size = |value: &Size<style::values::computed::LengthPercentage>| match value {
        Size::Breadth(value) => breadth(value),
        // A range is laid out at its larger end, which is what a grid does when
        // there is room; the smaller end matters when there is not, and that needs
        // sizing this does not do.
        Size::Minmax(_, max) => breadth(max),
        Size::FitContent(_) => Track::Auto,
    };

    let mut out = Vec::new();
    let style::values::generics::grid::GenericGridTemplateComponent::TrackList(list) = template
    else {
        return out;
    };

    for value in list.values.iter() {
        match value {
            ListValue::TrackSize(track) => out.push(size(track)),
            ListValue::TrackRepeat(repeat) => {
                // `auto-fill` and `auto-fit` need the container's size to know how
                // many times they go in; they come back through `auto_repeat`
                // instead, and layout decides.
                let RepeatCount::Number(count) = repeat.count else {
                    continue;
                };
                for _ in 0..count.max(0) {
                    out.extend(repeat.track_sizes.iter().map(&size));
                }
            }
        }
    }
    out
}

/// Where a grid item sits along one axis, from the two lines it names.
///
/// A named line needs the container's line names, which an item cannot see from
/// here; a numbered one is what a page writes and is what this reads.
fn placement(
    start: &style::values::computed::GridLine,
    end: &style::values::computed::GridLine,
) -> Placement {
    let numbered = |line: &style::values::computed::GridLine| {
        (!line.is_span && line.line_num != 0).then_some(line.line_num)
    };
    let span_of = |line: &style::values::computed::GridLine| {
        (line.is_span && line.line_num > 0).then_some(line.line_num as u32)
    };

    let line = numbered(start);
    let span = match (span_of(start), span_of(end), line, numbered(end)) {
        (Some(span), _, _, _) | (_, Some(span), _, _) => span,
        // Two numbered lines: the item covers what is between them.
        (None, None, Some(from), Some(to)) if to > from => (to - from) as u32,
        _ => 1,
    };

    Placement {
        line,
        span: span.max(1),
    }
}

/// The pattern inside a `repeat(auto-fill)` or `repeat(auto-fit)`, if the template
/// has one: how many times it goes in is the container's business.
fn auto_repeat(template: &style::values::computed::GridTemplateComponent) -> Option<Vec<Track>> {
    use style::values::generics::grid::{
        GenericTrackListValue as ListValue, GenericTrackSize as Size, RepeatCount,
        TrackBreadth as Breadth,
    };

    let style::values::generics::grid::GenericGridTemplateComponent::TrackList(list) = template
    else {
        return None;
    };

    list.values.iter().find_map(|value| {
        let ListValue::TrackRepeat(repeat) = value else {
            return None;
        };
        if matches!(repeat.count, RepeatCount::Number(_)) {
            return None;
        }
        Some(
            repeat
                .track_sizes
                .iter()
                .map(|size| match size {
                    Size::Breadth(Breadth::Breadth(length)) => {
                        Track::Fixed(length_percentage(length))
                    }
                    Size::Breadth(Breadth::Flex(flex)) => Track::Fraction(flex.0),
                    Size::Minmax(_, Breadth::Breadth(length)) => {
                        Track::Fixed(length_percentage(length))
                    }
                    _ => Track::Auto,
                })
                .collect(),
        )
    })
}

/// A computed `<length-percentage>`, as the length layout reads.
fn length_percentage(value: &style::values::computed::LengthPercentage) -> Length {
    match value.to_percentage() {
        Some(percentage) => Length::Percent(percentage.0),
        None => Length::Px(value.to_used_value(app_units::Au(0)).to_f32_px()),
    }
}

/// `border-radius`, taking the horizontal radius of each corner.
fn corners(values: &ComputedValues) -> Corners {
    let border = values.get_border();
    let radius = |corner: &style::values::computed::BorderCornerRadius| length(&corner.0.width.0);
    Corners {
        top_left: radius(&border.border_top_left_radius),
        top_right: radius(&border.border_top_right_radius),
        bottom_right: radius(&border.border_bottom_right_radius),
        bottom_left: radius(&border.border_bottom_left_radius),
    }
}

fn overflow_of(values: &ComputedValues) -> Overflow {
    use style::computed_values::overflow_x::T as Computed;

    let box_ = values.get_box();
    let clipped = |value: Computed| value != Computed::Visible;
    if clipped(box_.overflow_x) || clipped(box_.overflow_y) {
        Overflow::Clip
    } else {
        Overflow::Visible
    }
}

fn position_of(values: &ComputedValues) -> Position {
    use style::computed_values::position::T as Computed;

    match values.clone_position() {
        Computed::Relative => Position::Relative,
        Computed::Absolute => Position::Absolute,
        Computed::Fixed => Position::Fixed,
        Computed::Sticky => Position::Sticky,
        _ => Position::Static,
    }
}

/// One of `top`, `right`, `bottom`, `left`.
fn inset(value: &style::values::computed::Inset) -> LengthOrAuto {
    use style::values::generics::position::GenericInset as Generic;

    match value {
        Generic::LengthPercentage(value) => match value.to_percentage() {
            Some(percentage) => LengthOrAuto::Percent(percentage.0),
            None => LengthOrAuto::Px(value.to_used_value(app_units::Au(0)).to_f32_px()),
        },
        // `auto`, and the anchor functions, which need an anchor element to
        // measure against and are `auto` until there is one.
        _ => LengthOrAuto::Auto,
    }
}

fn flex_direction(values: &ComputedValues) -> FlexDirection {
    use style::computed_values::flex_direction::T as Computed;

    match values.clone_flex_direction() {
        Computed::Row => FlexDirection::Row,
        Computed::RowReverse => FlexDirection::RowReverse,
        Computed::Column => FlexDirection::Column,
        Computed::ColumnReverse => FlexDirection::ColumnReverse,
    }
}

fn flex_wrap(values: &ComputedValues) -> FlexWrap {
    use style::computed_values::flex_wrap::T as Computed;

    match values.clone_flex_wrap() {
        Computed::Nowrap => FlexWrap::NoWrap,
        Computed::Wrap => FlexWrap::Wrap,
        Computed::WrapReverse => FlexWrap::WrapReverse,
    }
}

fn justify_content(values: &ComputedValues) -> JustifyContent {
    use style::values::specified::align::AlignFlags;

    match values.clone_justify_content().primary().value() {
        AlignFlags::SPACE_BETWEEN => JustifyContent::SpaceBetween,
        AlignFlags::SPACE_AROUND => JustifyContent::SpaceAround,
        AlignFlags::SPACE_EVENLY => JustifyContent::SpaceEvenly,
        AlignFlags::CENTER => JustifyContent::Center,
        AlignFlags::END | AlignFlags::FLEX_END | AlignFlags::RIGHT => JustifyContent::End,
        _ => JustifyContent::Start,
    }
}

/// One `align-items`-shaped keyword, whatever property it came from.
fn align_keyword(value: style::values::specified::align::AlignFlags) -> Option<AlignItems> {
    use style::values::specified::align::AlignFlags;

    match value.value() {
        AlignFlags::CENTER => Some(AlignItems::Center),
        AlignFlags::START | AlignFlags::SELF_START | AlignFlags::FLEX_START => {
            Some(AlignItems::Start)
        }
        AlignFlags::END | AlignFlags::SELF_END | AlignFlags::FLEX_END => Some(AlignItems::End),
        AlignFlags::STRETCH | AlignFlags::NORMAL => Some(AlignItems::Stretch),
        AlignFlags::BASELINE | AlignFlags::LAST_BASELINE => Some(AlignItems::Baseline),
        _ => None,
    }
}

fn align_items(values: &ComputedValues) -> AlignItems {
    align_keyword(values.clone_align_items().0).unwrap_or(AlignItems::Stretch)
}

fn align_self(values: &ComputedValues) -> Option<AlignItems> {
    use style::values::specified::align::AlignFlags;

    let value = values.clone_align_self().0;
    if value.value() == AlignFlags::AUTO {
        return None;
    }
    align_keyword(value)
}

/// A `row-gap` or `column-gap`. `normal` is no gap outside a multi-column layout.
fn gap(value: &style::values::computed::length::NonNegativeLengthPercentageOrNormal) -> Length {
    use style::values::generics::length::GenericLengthPercentageOrNormal as Generic;

    match value {
        Generic::Normal => Length::ZERO,
        Generic::LengthPercentage(length_percentage) => length(&length_percentage.0),
    }
}

fn flex_basis(values: &ComputedValues) -> Option<LengthOrAuto> {
    use style::values::generics::flex::FlexBasis as Generic;

    match values.clone_flex_basis() {
        Generic::Content => None,
        Generic::Size(value) => match size(&value) {
            LengthOrAuto::Auto => None,
            other => Some(other),
        },
    }
}

fn float_of(values: &ComputedValues) -> Float {
    use style::computed_values::float::T as Computed;

    match values.clone_float() {
        Computed::Left => Float::Left,
        Computed::Right => Float::Right,
        _ => Float::None,
    }
}

fn clear_of(values: &ComputedValues) -> Clear {
    use style::computed_values::clear::T as Computed;

    match values.clone_clear() {
        Computed::Left => Clear::Left,
        Computed::Right => Clear::Right,
        Computed::Both => Clear::Both,
        _ => Clear::None,
    }
}

fn text_decoration(values: &ComputedValues) -> TextDecoration {
    let line = values.clone_text_decoration_line();
    TextDecoration {
        underline: line.contains(style::values::computed::TextDecorationLine::UNDERLINE),
        line_through: line.contains(style::values::computed::TextDecorationLine::LINE_THROUGH),
    }
}

fn length_or_auto(value: &style::values::computed::Margin) -> LengthOrAuto {
    use style::values::generics::length::GenericMargin as Generic;

    match value {
        Generic::Auto => LengthOrAuto::Auto,
        Generic::LengthPercentage(value) | Generic::AnchorContainingCalcFunction(value) => {
            match value.to_percentage() {
                Some(percentage) => LengthOrAuto::Percent(percentage.0),
                None => LengthOrAuto::Px(value.to_used_value(app_units::Au(0)).to_f32_px()),
            }
        }
        // Anchor positioning, which layout does not do.
        Generic::AnchorSizeFunction(_) => LengthOrAuto::Auto,
    }
}

fn length(value: &style::values::computed::LengthPercentage) -> Length {
    match value.to_percentage() {
        Some(percentage) => Length::Percent(percentage.0),
        None => Length::Px(value.to_used_value(app_units::Au(0)).to_f32_px()),
    }
}

fn size(value: &style::values::computed::Size) -> LengthOrAuto {
    use style::values::generics::length::GenericSize as Generic;

    match value {
        Generic::Auto => LengthOrAuto::Auto,
        Generic::LengthPercentage(value) => match value.0.to_percentage() {
            Some(percentage) => LengthOrAuto::Percent(percentage.0),
            None => LengthOrAuto::Px(value.0.to_used_value(app_units::Au(0)).to_f32_px()),
        },
        // `min-content`, `max-content` and `fit-content` need intrinsic sizing,
        // which layout does not do; auto is the value it can honour.
        _ => LengthOrAuto::Auto,
    }
}

/// `min-width` and `min-height`. `auto` floors at nothing, which is what it means
/// outside a flex or grid item.
fn min_size(value: &style::values::computed::Size) -> Length {
    match size(value) {
        LengthOrAuto::Px(px) => Length::Px(px),
        LengthOrAuto::Percent(fraction) => Length::Percent(fraction),
        LengthOrAuto::Auto => Length::ZERO,
    }
}

/// `max-width` and `max-height`, where `none` is no limit at all rather than a
/// very large one.
fn max_size(value: &style::values::computed::MaxSize) -> Option<Length> {
    use style::values::generics::length::GenericMaxSize as Generic;

    match value {
        Generic::None => None,
        Generic::LengthPercentage(value) => Some(match value.0.to_percentage() {
            Some(percentage) => Length::Percent(percentage.0),
            None => Length::Px(value.0.to_used_value(app_units::Au(0)).to_f32_px()),
        }),
        // The intrinsic keywords need intrinsic sizing, which layout does not do.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::{Viewport, style_document};

    /// The layout style of the first element matching `selector`.
    fn layout_style(html: &str, selector: &str) -> ComputedStyle {
        let document = otlyra_html::parse(html.as_bytes(), Some("utf-8")).document;
        let styled = style_document(&document, Viewport::default());
        let node = crate::stylo_dom::select(&document, selector)
            .expect("the selector should parse")
            .into_iter()
            .next()
            .expect("something should match");
        to_layout_style(styled.style_of(node).expect("a styled element"))
    }

    #[test]
    fn an_author_rule_reaches_the_values_layout_reads() {
        let style = layout_style(
            "<style>p { color: #f00; font-weight: bold; font-style: italic }</style><p>x",
            "p",
        );
        assert_eq!(style.color, Color::from_rgb8(255, 0, 0));
        assert_eq!(style.font_weight, 700);
        assert_eq!(style.font_style, FontStyle::Italic);
    }

    /// Every length arrives in pixels: `em` is resolved against the parent's font
    /// size before layout ever sees it.
    #[test]
    fn relative_lengths_arrive_resolved() {
        let style = layout_style(
            "<style>div { font-size: 20px } p { font-size: 1.5em; margin-top: 2em }</style>\
             <div><p>x</p></div>",
            "p",
        );
        assert_eq!(style.font_size, 30.0);
        assert_eq!(style.margin.top, LengthOrAuto::Px(60.0));
    }

    #[test]
    fn a_percentage_stays_a_percentage_for_layout_to_resolve() {
        let style = layout_style(
            "<style>p { width: 50%; margin-left: auto }</style><p>x",
            "p",
        );
        assert_eq!(style.width, LengthOrAuto::Percent(0.5));
        assert_eq!(style.margin.left, LengthOrAuto::Auto);
    }

    #[test]
    fn the_font_stack_comes_back_as_css_text() {
        let style = layout_style(
            "<style>p { font-family: \"Some Face\", monospace }</style><p>x",
            "p",
        );
        assert_eq!(&*style.font_family, "Some Face, monospace");
    }

    /// `display: none` has to survive the narrowing: it is the one display value
    /// that changes whether a box exists at all.
    #[test]
    fn display_none_is_not_approximated() {
        assert_eq!(
            layout_style("<style>p { display: none }</style><p>x", "p").display,
            Display::None
        );
        assert_eq!(
            layout_style("<style>p { display: inline }</style><p>x", "p").display,
            Display::Inline
        );
        assert_eq!(
            layout_style(
                "<style>span { display: block }</style><span>x</span>",
                "span"
            )
            .display,
            Display::Block
        );
    }

    /// A formatting context layout does not have still generates a box, laid out as
    /// a block. Dropping the element instead would hide its content entirely.
    #[test]
    fn a_background_picture_keeps_the_address_it_was_written_with() {
        let style = layout_style(
            "<style>div { background-image: url(pic.png) }</style><div>x</div>",
            "div",
        );
        assert_eq!(style.background_image.as_deref(), Some("pic.png"));
    }

    /// A page that says nothing about its font gets the browser's standard one,
    /// which is a serif — the same thing every browser shows it in.
    #[test]
    fn an_unstyled_element_takes_the_standard_font() {
        let style = layout_style("<p>text</p>", "p");
        assert_eq!(style.font_family.as_ref(), "serif");
        assert_eq!(style.font_size, 16.0);
    }

    /// The rest of what CSS says about a font, all of which the shaper can act on.
    #[test]
    fn the_font_properties_a_shaper_needs_are_all_read() {
        let style = layout_style(
            "<style>p { font-stretch: 62.5%; letter-spacing: 0.25em; word-spacing: 4px; \
             font-optical-sizing: none; \
             font-variation-settings: \"wght\" 350, \"opsz\" 8 }</style><p>text</p>",
            "p",
        );

        assert_eq!(style.font_width, 62.5);
        // A quarter of the sixteen-pixel font size, not of anything else.
        assert_eq!(style.letter_spacing, 4.0);
        assert_eq!(style.word_spacing, 4.0);
        assert!(!style.optical_sizing);
        // By tag rather than as written: the engine orders them, and a shaper takes
        // the last of a repeated tag, so the order has to be one both agree on.
        assert_eq!(
            style.font_variations.as_ref(),
            [(*b"opsz", 8.0), (*b"wght", 350.0)]
        );
    }

    /// And their initial values, which is what almost every element on a page has.
    #[test]
    fn the_font_properties_have_the_values_css_starts_them_at() {
        let style = layout_style("<p>text</p>", "p");
        assert_eq!(style.font_width, 100.0);
        assert_eq!(style.letter_spacing, 0.0);
        assert_eq!(style.word_spacing, 0.0);
        assert!(style.optical_sizing);
        assert!(style.font_variations.is_empty());
    }

    /// `vertical-align` in the values that move a box without needing the line
    /// box, plus the ones that do and are left where they were.
    #[test]
    fn vertical_align_is_read_in_every_value_it_takes() {
        use crate::style::VerticalAlign;

        let align = |source: &str| {
            layout_style(
                &format!("<style>span {{ vertical-align: {source} }}</style><p><span>x</span></p>"),
                "span",
            )
            .vertical_align
        };

        assert_eq!(align("baseline"), VerticalAlign::Baseline);
        assert_eq!(align("sub"), VerticalAlign::Sub);
        assert_eq!(align("super"), VerticalAlign::Super);
        assert_eq!(align("4px"), VerticalAlign::Length(4.0));
        assert_eq!(align("-2px"), VerticalAlign::Length(-2.0));
        assert_eq!(align("50%"), VerticalAlign::Percent(0.5));
        // The five that are a position rather than a shift. Three of them —
        // `middle`, `text-top` and `text-bottom` — arrive on
        // `alignment-baseline` rather than on `baseline-shift`, which is how
        // CSS Inline 3 splits the shorthand and is why reading only the second
        // had them all landing on the baseline and doing nothing.
        assert_eq!(align("top"), VerticalAlign::Top);
        assert_eq!(align("bottom"), VerticalAlign::Bottom);
        assert_eq!(align("middle"), VerticalAlign::Middle);
        assert_eq!(align("text-top"), VerticalAlign::TextTop);
        assert_eq!(align("text-bottom"), VerticalAlign::TextBottom);

        // All five are settled while the line is levelled; a shift the box knows
        // on its own is not.
        assert!(VerticalAlign::Top.resolved_while_levelling());
        assert!(VerticalAlign::Middle.resolved_while_levelling());
        assert!(!VerticalAlign::Super.resolved_while_levelling());

        // And the user-agent sheet's own use of it.
        assert_eq!(
            layout_style("<p><sup>x</sup></p>", "sup").vertical_align,
            VerticalAlign::Super
        );
        assert_eq!(
            layout_style("<p><sub>x</sub></p>", "sub").vertical_align,
            VerticalAlign::Sub
        );
    }

    #[test]
    fn background_repeat_is_read_per_axis() {
        let repeat = |source: &str| {
            layout_style(
                &format!("<style>div {{ background-repeat: {source} }}</style><div>x</div>"),
                "div",
            )
            .background_repeat
        };

        assert_eq!(repeat("repeat"), BackgroundRepeat::REPEAT);
        assert_eq!(
            repeat("no-repeat"),
            BackgroundRepeat {
                x: Repeat::None,
                y: Repeat::None
            }
        );
        assert_eq!(
            repeat("repeat-x"),
            BackgroundRepeat {
                x: Repeat::Repeat,
                y: Repeat::None
            }
        );
        assert_eq!(
            repeat("round no-repeat"),
            BackgroundRepeat {
                x: Repeat::Round,
                y: Repeat::None
            }
        );
    }

    /// A position is a fraction of the room the picture leaves plus a length, and
    /// every spelling CSS allows lands as some combination of the two — including
    /// the ones that compute to a `calc()`, which is what makes the two-probe
    /// reading worth having.
    #[test]
    fn object_fit_and_position_are_read() {
        let style = |declarations: &str| {
            layout_style(
                &format!("<style>img {{ {declarations} }}</style><img src=x.png>"),
                "img",
            )
        };

        assert_eq!(style("").object_fit, ObjectFit::Fill, "the initial value");
        assert_eq!(
            style("").object_position,
            BackgroundPosition::CENTER,
            "which starts in the middle rather than the corner"
        );
        assert_eq!(style("object-fit: cover").object_fit, ObjectFit::Cover);
        assert_eq!(
            style("object-fit: scale-down").object_fit,
            ObjectFit::ScaleDown
        );
        assert_eq!(
            style("object-position: left bottom").object_position,
            BackgroundPosition {
                x: Anchor::START,
                y: Anchor {
                    fraction: 1.0,
                    offset: 0.0
                },
            }
        );
    }

    #[test]
    fn background_position_reads_both_halves_of_a_calc() {
        let position = |source: &str| {
            layout_style(
                &format!("<style>div {{ background-position: {source} }}</style><div>x</div>"),
                "div",
            )
            .background_position
        };

        assert_eq!(position("0 0"), BackgroundPosition::START);
        assert_eq!(
            position("10px 4px"),
            BackgroundPosition {
                x: Anchor {
                    fraction: 0.0,
                    offset: 10.0
                },
                y: Anchor {
                    fraction: 0.0,
                    offset: 4.0
                },
            }
        );
        assert_eq!(
            position("center"),
            BackgroundPosition {
                x: Anchor {
                    fraction: 0.5,
                    offset: 0.0
                },
                y: Anchor {
                    fraction: 0.5,
                    offset: 0.0
                },
            }
        );
        assert_eq!(
            position("right 10px bottom 4px"),
            BackgroundPosition {
                x: Anchor {
                    fraction: 1.0,
                    offset: -10.0
                },
                y: Anchor {
                    fraction: 1.0,
                    offset: -4.0
                },
            }
        );
    }

    #[test]
    fn a_grid_container_is_recognized() {
        let style = layout_style("<style>div { display: grid }</style><div>x</div>", "div");
        assert_eq!(style.display, Display::Grid);
    }

    #[test]
    fn an_unsupported_display_falls_back_to_block() {
        // `ruby` is a display with a formatting context we do not have; its boxes
        // stack as blocks rather than being dropped.
        assert_eq!(
            layout_style("<style>p { display: ruby }</style><p>x", "p").display,
            Display::Block
        );
        assert_eq!(
            layout_style("<style>p { display: inline-block }</style><p>x", "p").display,
            Display::InlineBlock
        );
    }

    /// A table and every part of one is its own display: what tells a table apart
    /// from a stack of blocks is that layout has a formatting context for it.
    #[test]
    fn the_table_displays_are_read_as_themselves() {
        let display = |markup: &str, selector: &str| layout_style(markup, selector).display;

        assert_eq!(display("<table><tr><td>x", "table"), Display::Table);
        assert_eq!(display("<table><tr><td>x", "tr"), Display::TableRow);
        assert_eq!(display("<table><tr><td>x", "td"), Display::TableCell);
        assert_eq!(display("<table><tr><th>x", "th"), Display::TableCell);
        assert_eq!(
            display("<table><tbody><tr><td>x", "tbody"),
            Display::TableRowGroup
        );
        assert_eq!(
            display("<table><thead><tr><td>x", "thead"),
            Display::TableRowGroup
        );
        assert_eq!(
            display("<table><caption>c</caption><tr><td>x", "caption"),
            Display::TableCaption
        );
        // Two pixels between the cells, which is what a table has unless it says
        // otherwise, and it is inherited so the cells can read it.
        assert_eq!(
            layout_style("<table><tr><td>x", "table").border_spacing,
            (2.0, 2.0)
        );
    }

    /// The presentational attributes: style written in the markup, cascading below
    /// every author rule and above the user-agent sheet.
    #[test]
    fn presentational_attributes_are_style() {
        let style = layout_style(
            "<table bgcolor=\"#ff0000\" width=\"300\"><tr><td>x",
            "table",
        );
        assert_eq!(style.background_color.to_rgba8().r, 255);
        assert_eq!(style.width, LengthOrAuto::Px(300.0));

        // A percentage is a percentage, and a value that is not a dimension at all
        // contributes nothing rather than being guessed at.
        assert_eq!(
            layout_style("<table width=\"50%\"><tr><td>x", "table").width,
            LengthOrAuto::Percent(0.5)
        );
        assert_eq!(
            layout_style("<table width=\"lots\"><tr><td>x", "table").width,
            LengthOrAuto::Auto
        );

        // A border attribute draws on the table and on its cells.
        let table = layout_style("<table border=\"3\"><tr><td>x", "table");
        assert_eq!(table.border.top.width, 3.0);
        assert_eq!(
            layout_style("<table border=\"3\"><tr><td>x", "td")
                .border
                .top
                .width,
            1.0,
            "a cell's own border is one pixel however wide the table's is"
        );
        assert_eq!(
            layout_style("<table border=\"0\"><tr><td>x", "td")
                .border
                .top
                .width,
            0.0,
            "and none at all when the attribute asks for none"
        );

        // Author CSS beats them, which is the whole reason they are an origin of
        // their own rather than part of the user-agent sheet.
        assert_eq!(
            layout_style(
                "<style>table { width: 100px }</style><table width=\"300\"><tr><td>x",
                "table"
            )
            .width,
            LengthOrAuto::Px(100.0)
        );
        // And they beat the user-agent sheet.
        assert_eq!(
            layout_style("<table align=\"center\"><tr><td>x", "table")
                .margin
                .left,
            LengthOrAuto::Auto
        );
    }

    /// `border-style` decides whether a declared width is used at all, which is
    /// why layout is handed a width rather than a style to interpret.
    #[test]
    fn a_border_width_counts_only_when_the_style_draws_something() {
        let drawn = layout_style("<style>p { border: 4px solid #00f }</style><p>x", "p");
        assert_eq!(drawn.border.top.width, 4.0);
        assert_eq!(drawn.border.left.color, Color::from_rgb8(0, 0, 255));

        let absent = layout_style("<style>p { border: 4px none #00f }</style><p>x", "p");
        assert_eq!(absent.border.top.width, 0.0);
        assert!(!absent.border.top.is_visible());
    }

    /// A border with no colour of its own takes the element's, which is what makes
    /// `border: 1px solid` follow the text it frames.
    #[test]
    fn a_border_defaults_to_the_elements_own_colour() {
        let style = layout_style(
            "<style>p { color: #0a0; border: 1px solid }</style><p>x",
            "p",
        );
        assert_eq!(style.border.top.color, Color::from_rgb8(0, 170, 0));
    }

    #[test]
    fn text_align_narrows_to_the_three_a_line_box_can_honour() {
        let align = |css: &str| {
            layout_style(
                &format!("<style>p {{ text-align: {css} }}</style><p>x"),
                "p",
            )
            .text_align
        };
        assert_eq!(align("center"), TextAlign::Center);
        assert_eq!(align("right"), TextAlign::End);
        assert_eq!(align("left"), TextAlign::Start);
        // Justification spaces a line out, which inline layout does not do.
        assert_eq!(align("justify"), TextAlign::Start);
    }

    /// The four ways a box can treat the white space in it, which is more than
    /// the two `white-space` looks like it has.
    #[test]
    fn every_way_of_treating_white_space_arrives_as_itself() {
        let mode = |value: &str| {
            layout_style(
                &format!("<style>p {{ white-space: {value} }}</style><p>x"),
                "p",
            )
            .white_space
        };

        assert_eq!(mode("normal"), WhiteSpace::Collapse);
        assert_eq!(
            mode("nowrap"),
            WhiteSpace::Collapse,
            "which wraps is a
             different question, and is `text-wrap-mode`"
        );
        assert_eq!(mode("pre"), WhiteSpace::Preserve);
        assert_eq!(mode("pre-wrap"), WhiteSpace::Preserve);
        assert_eq!(mode("pre-line"), WhiteSpace::PreserveBreaks);
        assert_eq!(mode("break-spaces"), WhiteSpace::BreakSpaces);
        assert_eq!(layout_style("<p>x", "p").white_space, WhiteSpace::Collapse);
    }
}
