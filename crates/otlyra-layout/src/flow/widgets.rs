//! Form controls drawn as widgets: the size they prefer, and where their
//! baseline sits.
//!
//! A control's preferred size is counted in characters of its own font, which
//! the box tree was built without, so it is settled at the start of layout rather
//! than when the box is made. And a control with nothing written in it still has
//! a line to write on, which gives it a baseline an empty box would not have.

use std::sync::Arc;

use otlyra_css::{ComputedStyle, Length, Size};
use otlyra_text::TextEngine;

use crate::box_tree::{BoxTree, Control, ControlKind, NaturalSize};
use crate::fonts::FontStacks;
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

    /// Place a control's contents in its content box, once they are laid out:
    /// centred down it when they came to less than it is tall (`laid_out`
    /// against `used`), and slid along by however far the reader has scrolled
    /// a field. A drop-down's open list is neither: it hangs off the control
    /// and is placed against it.
    ///
    /// HTML's button layout centres the anonymous button content box — which
    /// is what puts a label, and the baseline the line sits on, in the middle
    /// of a button taller than its text. A button laid out as a flex or grid
    /// container is not one: its items are placed as that layout says.
    pub(super) fn place_control_contents(
        &self,
        id: crate::box_tree::BoxId,
        style: &ComputedStyle,
        (laid_out, used): (f32, f32),
        children: &mut [crate::fragment::Fragment],
    ) {
        let Some(control) = self.tree.node(id).control.as_ref() else {
            return;
        };
        let lays_out_its_own = matches!(
            style.display,
            otlyra_css::Display::Flex
                | otlyra_css::Display::InlineFlex
                | otlyra_css::Display::Grid
                | otlyra_css::Display::InlineGrid
        );
        let centred = if control.kind.centres_contents() && !lays_out_its_own {
            ((used - laid_out) / 2.0).max(0.0)
        } else {
            0.0
        };
        let (dx, dy) = (-control.scroll.0, centred - control.scroll.1);
        if (dx, dy) == (0.0, 0.0) {
            return;
        }
        for child in children {
            if !super::is_popup(self.tree, child) {
                super::shift(child, dx, dy);
            }
        }
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
    ///
    /// A checkbox, a radio button and a slider have no line and no business
    /// pretending to, but both references sit them on the baseline by their bottom
    /// *border* edge, `height` below their top, with their margins hanging below
    /// it. The rest — a button with no label, a colour well, a file picker, a bar,
    /// a meter — keep CSS's rule.
    pub(super) fn empty_control_baseline(
        &mut self,
        id: crate::box_tree::BoxId,
        style: &Arc<ComputedStyle>,
        containing_width: f32,
        height: f32,
    ) -> Option<f32> {
        let control = self.tree.node(id).control.as_ref()?;
        match control.kind {
            ControlKind::Field
            | ControlKind::Area
            | ControlKind::DropDown
            | ControlKind::ListBox => {}
            ControlKind::Checkbox | ControlKind::Radio | ControlKind::Range => return Some(height),
            ControlKind::Button
            | ControlKind::Color
            | ControlKind::File
            | ControlKind::Progress
            | ControlKind::Meter => return None,
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
    let mut stacks = FontStacks::default();
    for id in tree.descendants(tree.root()) {
        let Some(control) = tree.node(id).control.clone() else {
            continue;
        };
        // A control whose widget is not drawn — `appearance: none`, or a
        // field styled out of its native look — is drawn as a box, and keeps
        // the size it would have had: devolving changes how a control looks,
        // not how big it is (CSS UI 4 §7.2). One with nothing but its drawing
        // to be measured by has no size left once that is gone.
        if control.natural.is_some() || !(control.widget || control.kind.keeps_its_size()) {
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
        // The arrow strip is part of the drawing, and goes with it.
        if control.kind == ControlKind::DropDown && control.widget {
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
    stacks: &mut FontStacks,
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
    stacks: &mut FontStacks,
) -> (f32, f32, f32) {
    let stack = stacks.of(&style.font_family);
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
