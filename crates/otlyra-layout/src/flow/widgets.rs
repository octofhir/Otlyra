//! Form controls drawn as widgets: the size they prefer, and where their
//! baseline sits.
//!
//! A control's preferred size is counted in characters of its own font, which
//! the box tree was built without, so it is settled at the start of layout rather
//! than when the box is made. And a control with nothing written in it still has
//! a line to write on, which gives it a baseline an empty box would not have.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, LengthOrAuto};
use otlyra_text::{FontStack, TextEngine};

use crate::box_tree::{BoxTree, Control, ControlKind};

use super::Flow;
use super::box_model::{resolve_border, resolve_padding};

impl<'a> Flow<'a> {
    /// Where the baseline of a control with nothing written in it sits.
    ///
    /// `None` for a box that is not a control, which keeps CSS's own rule for
    /// everything else: an inline-block with no line boxes really does sit on its
    /// bottom margin edge, and an empty `<div>` must go on doing so.
    ///
    /// A control is different because the box its text goes in is there whether or
    /// not there is any text — a field one has never typed into still has a line to
    /// type on, and that line has a baseline. Measured from the box's own top edge,
    /// so it is the same number `baseline_of` would have answered the moment a
    /// letter arrived.
    pub(super) fn empty_control_baseline(
        &mut self,
        id: crate::box_tree::BoxId,
        style: &Arc<ComputedStyle>,
        containing_width: f32,
    ) -> Option<f32> {
        let control = self.tree.node(id).control.as_ref()?;
        // Only the ones that hold text of their own. A checkbox has no line and no
        // business pretending to; a bar and a slider are drawn shapes.
        if !matches!(
            control.kind,
            ControlKind::Field | ControlKind::Area | ControlKind::DropDown | ControlKind::ListBox
        ) {
            return None;
        }
        let border = resolve_border(style);
        let padding = resolve_padding(style, containing_width);
        let stack = self.font_stack(style);
        let strut = self.strut_of(style, &stack)?;
        Some(border.top + padding.top + strut.ascent)
    }
}

/// Work out how big every widget on the page wants to be, once.
///
/// A control that is drawn as a widget has a *default preferred size*: the
/// size it takes when nothing has said otherwise. It is not what its contents
/// come to — an empty field is as wide as a full one — and it is not a constant
/// either, because for everything that holds text it is counted in characters
/// of the control's own font.
///
/// The counting is HTML's. A field is `(size − 1) × avg + max` wide, which is
/// twenty characters plus the difference between an average one and the widest
/// one; a `<textarea>` is `cols × avg` plus room for a scroll bar and `rows`
/// lines tall. `avg` and `max` here are the advance of a digit and of a capital
/// W, which is an approximation of what a font's own tables report and is
/// within a pixel of it for the families a control is ever set in.
pub(super) fn size_widgets(tree: &mut BoxTree, text: &mut TextEngine) {
    let mut stacks: std::collections::HashMap<usize, FontStack> = std::collections::HashMap::new();
    for id in tree.descendants(tree.root()) {
        let Some(control) = tree.node(id).control.clone() else {
            continue;
        };
        if !control.widget {
            continue;
        }
        // Once, and only once. Layout runs many times over one box tree, and the
        // room a drop-down leaves for its arrow is *added* to the padding rather
        // than replacing it — so a second pass over a control already settled
        // grows it by the width of another arrow.
        if control.sized {
            continue;
        }
        let style = Arc::clone(&tree.node(id).style);
        let Some((width, height)) = widget_size(&control, &style, text, &mut stacks) else {
            continue;
        };
        // Only what the page has not decided. A width in a rule is the page's
        // answer, and a preferred size is what there is in the absence of one.
        let mut sized = (*style).clone();
        let mut changed = false;
        if control.kind == ControlKind::DropDown {
            sized.padding.right =
                otlyra_css::Length::Px(resolve_padding(&style, 0.0).right + ARROW_STRIP);
            changed = true;
        }
        if let Some(width) = width
            && sized.width == LengthOrAuto::Auto
        {
            sized.width = LengthOrAuto::Px(width);
            changed = true;
        }
        if let Some(height) = height
            && sized.height == LengthOrAuto::Auto
        {
            sized.height = LengthOrAuto::Px(height);
            changed = true;
        }
        if changed {
            tree.set_style(id, Arc::new(sized));
        }
        tree.mark_sized(id);
    }
}

/// The content-box size a widget prefers, in each axis it has an opinion about.
fn widget_size(
    control: &Control,
    style: &Arc<ComputedStyle>,
    text: &mut TextEngine,
    stacks: &mut std::collections::HashMap<usize, FontStack>,
) -> Option<(Option<f32>, Option<f32>)> {
    // A checkbox and a radio button, in CSS pixels. Both references agree within a
    // pixel and neither takes it from the font: a checkbox in a heading is the same
    // checkbox.
    const BOX_SIDE: f32 = 13.0;
    // What a scroll bar takes from the width a `<textarea>` asks for.
    const SCROLLBAR: f32 = 15.0;

    let (average, widest, line) = character_widths(style, text, stacks);
    // A field is measured across its content box and a checkbox across its
    // border box; the styles say which, and what is subtracted here is what the
    // difference comes to.
    let edges = |style: &ComputedStyle| {
        let padding = resolve_padding(style, 0.0);
        let border = resolve_border(style);
        (
            padding.left + padding.right + border.left + border.right,
            padding.top + padding.bottom + border.top + border.bottom,
        )
    };
    let (across, down) = edges(style);
    let border_box = style.box_sizing == otlyra_css::BoxSizing::Border;
    let inline = |content: f32| {
        Some(if border_box {
            content + across
        } else {
            content
        })
    };
    let block = |content: f32| Some(if border_box { content + down } else { content });

    match control.kind {
        ControlKind::Checkbox | ControlKind::Radio => Some((inline(BOX_SIDE), block(BOX_SIDE))),
        ControlKind::Field => {
            let size = control.size.unwrap_or(20).max(1) as f32;
            Some((inline((size - 1.0) * average + widest), block(line)))
        }
        ControlKind::Area => {
            let width = control.cols.max(1) as f32 * average + SCROLLBAR;
            let height = control.rows.max(1) as f32 * line;
            Some((inline(width), block(height)))
        }
        ControlKind::ListBox => {
            let rows = control.size.unwrap_or(4).max(1) as f32;
            Some((None, block(rows * line)))
        }
        // A drop-down is as wide as the option it shows plus the arrow beside
        // it, and the option is its contents — so only the arrow is added, by
        // the padding rather than by a width, since a width would stop the
        // contents from making it wider.
        ControlKind::DropDown => Some((None, block(line))),
        ControlKind::Range => Some((inline(129.0), block(16.0))),
        ControlKind::Color => Some((inline(44.0), block(23.0))),
        ControlKind::Button | ControlKind::File => None,
        ControlKind::Progress | ControlKind::Meter => None,
    }
}

/// The strip a drop-down leaves on its inline end for the arrow.
///
/// Room rather than a width: a drop-down is as wide as the option it shows, and
/// a width would stop a long option from making it wider. Both references
/// reserve the same twenty pixels give or take two, and both give it back when
/// the page turns the widget off — which is the one visible thing
/// `appearance: none` does to a `<select>`.
const ARROW_STRIP: f32 = 20.0;

/// An average character, the widest one, and the height of one line, in the font
/// this style asks for.
///
/// The line is the *strut* rather than what a digit measures: a line is as tall as
/// the font reaches above and below the baseline plus whatever `line-height` asks
/// for, and a digit is neither. A field a digit tall is two pixels shorter than
/// every reference, and a `<textarea>` is that twice per row.
fn character_widths(
    style: &Arc<ComputedStyle>,
    text: &mut TextEngine,
    stacks: &mut std::collections::HashMap<usize, FontStack>,
) -> (f32, f32, f32) {
    let key = Arc::as_ptr(&style.font_family) as *const u8 as usize;
    let stack = stacks
        .entry(key)
        .or_insert_with(|| FontStack::parse_css(&style.font_family))
        .clone();
    let size = style.font_size;
    let average = text.measure("0", &stack, size).width;
    let widest = text.measure("W", &stack, size).width;
    let line = text
        .strut(&stack, size, style.font_weight, false)
        .map_or(size, |strut| match style.line_height {
            otlyra_css::LineHeight::Normal => strut.height(),
            ref asked => asked.resolve(size, strut.height()),
        });
    (average, widest, line)
}
