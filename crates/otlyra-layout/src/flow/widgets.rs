//! Form controls drawn as widgets: the size they prefer, and where their
//! baseline sits.
//!
//! A control's preferred size is counted in characters of its own font, which
//! the box tree was built without, so it is settled at the start of layout rather
//! than when the box is made. And a control with nothing written in it still has
//! a line to write on, which gives it a baseline an empty box would not have.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Length, Size};
use otlyra_text::{FontStack, TextEngine};

use crate::box_tree::{BoxTree, Control, ControlKind, NaturalSize};
use crate::widget_metrics::{
    ARROW_STRIP, CHECK_SIDE, COLOR_HEIGHT, COLOR_WIDTH, RANGE_HEIGHT, RANGE_WIDTH,
};

use super::Flow;
use super::box_model::{resolve_border, resolve_padding};
use super::sizing::{Frame, declared_size};

impl<'a> Flow<'a> {
    /// The height a box's content comes to, which is what `auto` and the content
    /// keywords make its height: what its contents were laid out at — or, for a
    /// widget with a natural height, that height whatever it holds. A text area
    /// is as many rows tall as it asks for however much has been typed into it,
    /// and a checkbox holds nothing at all (CSS Sizing 3 §5.1).
    pub(super) fn content_height(&self, id: crate::box_tree::BoxId, laid_out: f32) -> f32 {
        self.tree.node(id).natural_size().height.unwrap_or(laid_out)
    }

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
/// A control that is drawn as a widget has a *natural size*: the size it takes
/// when nothing has said otherwise. It is not what its contents come to — an
/// empty field is as wide as a full one — and it is not a constant either,
/// because for everything that holds text it is counted in characters of the
/// control's own font.
///
/// The counting is HTML's. A field is `(size − 1) × avg + max` wide, which is
/// twenty characters plus the difference between an average one and the widest
/// one; a `<textarea>` is `cols × avg` plus room for a scroll bar and `rows`
/// lines tall. `avg` and `max` here are the advance of a digit and of a capital
/// W, which is an approximation of what a font's own tables report and is
/// within a pixel of it for the families a control is ever set in.
///
/// It is kept on the box, which is where the content keywords find it, and
/// written into the style as the `width` and `height` of a control whose own
/// are `auto`, which is where everything else does.
pub(super) fn size_widgets(tree: &mut BoxTree, text: &mut TextEngine) {
    let mut stacks: std::collections::HashMap<usize, FontStack> = std::collections::HashMap::new();
    for id in tree.descendants(tree.root()) {
        let Some(control) = tree.node(id).control.clone() else {
            continue;
        };
        if !control.widget || control.natural.is_some() {
            continue;
        }
        let style = Arc::clone(&tree.node(id).style);
        let natural = widget_size(&control, &style, text, &mut stacks);
        // Only what the page has not decided. A width in a rule is the page's
        // answer, and a preferred size is what there is in the absence of one.
        // A content keyword is the page asking for the natural size, which the
        // box answers when it is measured, so it is left as it was written.
        let frame = Frame::new(resolve_padding(&style, 0.0), resolve_border(&style));
        let declared = |content: f32, frame: f32| {
            Size::Length(Length::Px(declared_size(content, style.box_sizing, frame)))
        };
        let mut sized = (*style).clone();
        let mut changed = false;
        if control.kind == ControlKind::DropDown {
            sized.padding.right = Length::Px(resolve_padding(&style, 0.0).right + ARROW_STRIP);
            changed = true;
        }
        if let Some(width) = natural.width
            && sized.width == Size::Auto
        {
            sized.width = declared(width, frame.inline);
            changed = true;
        }
        if let Some(height) = natural.height
            && sized.height == Size::Auto
        {
            sized.height = declared(height, frame.block);
            changed = true;
        }
        if changed {
            tree.set_style(id, Arc::new(sized));
        }
        tree.set_natural_size(id, natural);
    }
}

/// The content-box size a widget prefers, in each axis it has an opinion about.
fn widget_size(
    control: &Control,
    style: &Arc<ComputedStyle>,
    text: &mut TextEngine,
    stacks: &mut std::collections::HashMap<usize, FontStack>,
) -> NaturalSize {
    // What a scroll bar takes from the width a `<textarea>` asks for.
    const SCROLLBAR: f32 = 15.0;

    let (average, widest, line) = character_widths(style, text, stacks);
    let both = |width: f32, height: f32| NaturalSize {
        width: Some(width),
        height: Some(height),
    };

    match control.kind {
        ControlKind::Checkbox | ControlKind::Radio => both(CHECK_SIDE, CHECK_SIDE),
        ControlKind::Field => {
            let size = control.size.unwrap_or(20).max(1) as f32;
            both((size - 1.0) * average + widest, line)
        }
        ControlKind::Area => {
            let width = control.cols.max(1) as f32 * average + SCROLLBAR;
            let height = control.rows.max(1) as f32 * line;
            both(width, height)
        }
        ControlKind::ListBox => {
            let rows = control.size.unwrap_or(4).max(1) as f32;
            NaturalSize {
                width: None,
                height: Some(rows * line),
            }
        }
        // A drop-down is as wide as the option it shows plus the arrow beside
        // it, and the option is its contents — so only the arrow is added, by
        // the padding rather than by a width, since a width would stop the
        // contents from making it wider.
        ControlKind::DropDown => NaturalSize {
            width: None,
            height: Some(line),
        },
        ControlKind::Range => both(RANGE_WIDTH, RANGE_HEIGHT),
        ControlKind::Color => both(COLOR_WIDTH, COLOR_HEIGHT),
        ControlKind::Button | ControlKind::File => NaturalSize::default(),
        ControlKind::Progress | ControlKind::Meter => NaturalSize::default(),
    }
}

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
