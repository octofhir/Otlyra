//! A small widget layer for the browser's own surfaces.
//!
//! The viewport is not a widget — a page is laid out by CSS, and nothing here
//! touches it. This is for the surfaces the browser draws itself: the tab strip
//! and toolbar, and the settings page, which is built from these controls rather
//! than from HTML so that a preference looks like the browser it belongs to.
//! Written as immediate mode, one rectangle at a time, each of those grows a
//! line of geometry, a line of painting and a line of hit testing for every
//! control added. Three places to keep in step is two too many.
//!
//! Four verbs: measure, place, draw, event. Measurement runs bottom up — a
//! container asks its children how big they want to be, then answers for
//! itself. Placement and drawing run top down. One rule holds the whole layer
//! together: **geometry is computed once**, by [`Widget::place`], stored on the
//! widget, and read by both drawing and hit testing. Neither can drift from the
//! other because neither computes anything.
//!
//! # What a widget reports
//!
//! Widgets are generic over the action they report, `Widget<A>`. The toolbar's
//! `A` is the browser's `UiAction`; the settings page has its own. The layer
//! itself knows about neither, which is what lets one set of controls serve two
//! surfaces that have nothing else in common — and what stops a general button
//! from having to grow a case in somebody else's enum.
//!
//! # What a widget does not hold
//!
//! State. A control is a view of a value the caller owns: a checkbox is told
//! whether it is ticked, a field is told what it contains. The tree is built
//! afresh each frame from that state and thrown away after, so there is no
//! second copy to fall behind and no identity to match between frames. Hover and
//! press need nothing stored either — where the pointer is and whether it is
//! down are known when the frame is drawn, and the rectangle is known once the
//! frame is placed, so *is the pointer over this* is a question with an answer
//! rather than a flag to keep up to date.
//!
//! What survives a frame is the tree itself, kept by the surface, so the next
//! press is tested against exactly the rectangles that were drawn.
//!
//! # What is deliberately absent
//!
//! There is no description tree diffed against live instances. That would buy
//! rebuilding a surface from data — which building afresh already gives — and
//! cost an identity scheme and a downcast at every node.
//!
//! Two further decisions worth stating, because both could have gone the other
//! way:
//!
//! - **Composition is additive.** A control takes no `padding`, `width` or
//!   `align` option. It is wrapped in [`Padding`], [`Fixed`] or [`Align`]. Every
//!   wrapper is one idea, so a new arrangement never means a new field on a
//!   control that did not ask for it.
//! - **Space between, not around.** Rows and columns take a `gap` rather than
//!   controls taking margins, because the space belongs to the join and not to
//!   either side of it.

pub mod controls;
pub mod data;
pub mod icon;
pub mod theme;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use otlyra_gfx::kurbo::{Affine, BezPath, RoundedRect, RoundedRectRadii, Shape};
use otlyra_gfx::peniko::{BlendMode, Brush, Color, Fill};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_text::{FontStack, TextEngine};

use crate::widget::theme::Theme;

/// A rectangle in logical pixels.
///
/// The layer's own geometry type rather than kurbo's, because this one is
/// origin-and-extent — which is what placement deals in — and converts to
/// kurbo's corner-to-corner form only at the point of painting.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

impl Rect {
    /// A rectangle at `x`, `y`, `width` wide and `height` tall.
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The empty rectangle at the origin, which is what a widget holds before it
    /// has been placed.
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };

    /// Whether `x`, `y` falls inside — top and left inclusive, bottom and right
    /// exclusive, so adjacent rectangles never both claim a point.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// The same rectangle, grown by `amount` on every side.
    pub fn inflate(&self, amount: f64) -> Self {
        Self::new(
            self.x - amount,
            self.y - amount,
            self.width + amount * 2.0,
            self.height + amount * 2.0,
        )
    }

    /// The same rectangle moved by `dx`, `dy`.
    pub fn offset(&self, dx: f64, dy: f64) -> Self {
        Self::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    /// The same rectangle in the geometry vocabulary the display list speaks.
    pub fn to_kurbo(self) -> otlyra_gfx::kurbo::Rect {
        otlyra_gfx::kurbo::Rect::new(self.x, self.y, self.x + self.width, self.y + self.height)
    }
}

/// How much space something wants, or has been given.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Size {
    /// Horizontal extent, in logical pixels.
    pub width: f64,
    /// Vertical extent, in logical pixels.
    pub height: f64,
}

impl Size {
    /// A size.
    pub fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }

    /// Nothing.
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };
}

/// Space taken on each side by a wrapper.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Insets {
    /// Left.
    pub left: f64,
    /// Top.
    pub top: f64,
    /// Right.
    pub right: f64,
    /// Bottom.
    pub bottom: f64,
}

impl Insets {
    /// The same on all four sides.
    pub fn all(value: f64) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    /// One value across, another down.
    pub fn symmetric(horizontal: f64, vertical: f64) -> Self {
        Self {
            left: horizontal,
            top: vertical,
            right: horizontal,
            bottom: vertical,
        }
    }

    fn horizontal(&self) -> f64 {
        self.left + self.right
    }

    fn vertical(&self) -> f64 {
        self.top + self.bottom
    }
}

/// Something that happened, on its way down the tree.
///
/// Events carry no position: the position is the pointer's, held by [`Cx`], and
/// tested against the rectangle each widget stored when it was placed. A widget
/// that had to be told where the pointer is could be told the wrong place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The pointer moved to [`Cx::pointer`].
    PointerMoved,
    /// The primary pointer button went down at [`Cx::pointer`].
    PointerPressed,
    /// It came back up.
    PointerReleased,
    /// The surface's keyboard focus was activated — the space or return key on
    /// whatever [`Cx::focus`] names.
    Activate,
    /// An arrow key on whatever holds the focus, as a step of `-1` or `1`.
    ///
    /// What a control with a range does with a keyboard. Moving *between*
    /// controls is the surface's business and never arrives as an event; this is
    /// only for a control that has somewhere to go inside itself.
    Adjust(i32),
}

impl Event {
    /// Whether this event is aimed by the pointer.
    ///
    /// The difference matters to anything that decides by position who may hear
    /// an event: a press outside a panel is not the panel's, but a key is aimed
    /// by the focus and reaches what holds it wherever the pointer happens to be
    /// resting — which is usually nowhere near.
    pub fn from_pointer(&self) -> bool {
        matches!(
            self,
            Self::PointerMoved | Self::PointerPressed | Self::PointerReleased
        )
    }
}

/// Which control on a surface has the keyboard.
///
/// A control's position in the order its surface built this frame — see
/// [`Focus`] — rather than a number anybody chose. The layer only compares them.
pub type FocusId = u32;

/// What a focusable control is, as far as the keyboard and the cursor care.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FocusKind {
    /// Something that does one thing when it is activated.
    Press,
    /// A field that text is typed into.
    Text,
}

/// One focusable control, as its surface recorded it.
#[derive(Copy, Clone, Debug)]
struct Entry {
    enabled: bool,
    kind: FocusKind,
    group: Option<u32>,
}

/// The focusable controls a surface built this frame, in the order it built them.
///
/// The same shape as [`Overflow`], and for the same reason: what the traversal
/// order *is* only becomes known while a frame is built, so the frame reports it
/// and the surface reads it back. An id is a control's position in this list
/// rather than a number anybody assigned, which makes *traversal order is
/// drawing order* true by construction. A list of named constants beside the
/// tree would be a second answer to the same question, and the two would
/// disagree the first time a row moved.
///
/// Which id *holds* the keyboard is deliberately not here. That is one value the
/// surface owns and is drawn from, and keeping it out of a shared cell is what
/// lets a surface notice the focus moving: a clone of the surface's state shares
/// this cell, so anything kept in it compares equal to itself however far the
/// focus travelled — and the ring would never be redrawn.
#[derive(Clone, Default)]
pub struct Focus {
    entries: Rc<RefCell<Vec<Entry>>>,
    groups: Rc<Cell<u32>>,
}

impl Focus {
    /// Forget the last frame's order, before building the next one.
    pub fn begin(&self) {
        self.entries.borrow_mut().clear();
        self.groups.set(0);
    }

    /// Claim the next id for something that is pressed.
    pub fn claim(&self, enabled: bool) -> FocusId {
        self.push(enabled, FocusKind::Press, None)
    }

    /// Claim the next id for a field that text is typed into.
    pub fn claim_text(&self, enabled: bool) -> FocusId {
        self.push(enabled, FocusKind::Text, None)
    }

    /// Claim the next id for one of a set of exclusive choices.
    ///
    /// Members of a group are reached from each other by the arrow keys, which
    /// is what a radio set and a segmented control are expected to do.
    pub fn claim_in(&self, group: u32, enabled: bool) -> FocusId {
        self.push(enabled, FocusKind::Press, Some(group))
    }

    /// A group number no other group on this surface has.
    pub fn group(&self) -> u32 {
        let next = self.groups.get();
        self.groups.set(next + 1);
        next
    }

    fn push(&self, enabled: bool, kind: FocusKind, group: Option<u32>) -> FocusId {
        let mut entries = self.entries.borrow_mut();
        entries.push(Entry {
            enabled,
            kind,
            group,
        });
        (entries.len() - 1) as FocusId
    }

    /// How many focusable controls the last frame built.
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    /// Whether the last frame built none at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What kind of control `id` names, if it names one.
    pub fn kind(&self, id: Option<FocusId>) -> Option<FocusKind> {
        let id = id?;
        self.entries
            .borrow()
            .get(id as usize)
            .map(|entry| entry.kind)
    }

    /// Whether `id` names a control that is drawn but does nothing.
    pub fn is_enabled(&self, id: FocusId) -> bool {
        self.entries
            .borrow()
            .get(id as usize)
            .is_some_and(|entry| entry.enabled)
    }

    /// The first field on the surface, for an accelerator that names one.
    pub fn first_text(&self) -> Option<FocusId> {
        self.entries
            .borrow()
            .iter()
            .position(|entry| entry.kind == FocusKind::Text && entry.enabled)
            .map(|index| index as FocusId)
    }

    /// What Tab moves to from `from`, wrapping past the end.
    pub fn next(&self, from: Option<FocusId>) -> Option<FocusId> {
        self.step(from, 1)
    }

    /// What shift-Tab moves to, wrapping past the start.
    pub fn previous(&self, from: Option<FocusId>) -> Option<FocusId> {
        self.step(from, -1)
    }

    /// The next member of `from`'s group, wrapping within it.
    ///
    /// `None` when `from` is in no group, which is how the surface knows to
    /// offer the arrow key to the control itself instead.
    pub fn step_in_group(&self, from: FocusId, forward: bool) -> Option<FocusId> {
        let entries = self.entries.borrow();
        let group = entries.get(from as usize)?.group?;
        let members: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.group == Some(group) && entry.enabled)
            .map(|(index, _)| index)
            .collect();
        let at = members.iter().position(|index| *index == from as usize)?;
        let step = if forward { 1 } else { members.len() - 1 };
        Some(members[(at + step) % members.len()] as FocusId)
    }

    /// Traversal, in either direction, skipping anything disabled.
    fn step(&self, from: Option<FocusId>, by: isize) -> Option<FocusId> {
        let entries = self.entries.borrow();
        let count = entries.len() as isize;
        if count == 0 {
            return None;
        }
        // With nothing focused, forward starts before the first and backward
        // starts after the last, so one step lands on the end being entered.
        let start = match from {
            Some(id) if (id as isize) < count => id as isize,
            _ if by > 0 => -1,
            _ => count,
        };
        (1..=count)
            .map(|step| (start + by * step).rem_euclid(count))
            .find(|index| entries[*index as usize].enabled)
            .map(|index| index as FocusId)
    }
}

impl std::fmt::Debug for Focus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Focus")
            .field("controls", &self.len())
            .finish()
    }
}

/// What a widget needs while it works.
///
/// The text engine is here rather than passed alongside because measuring and
/// drawing must use the same one: a caret placed by a second engine lands a
/// pixel off per glyph.
pub struct Cx<'a> {
    /// The engine that measures and shapes every string the interface draws.
    pub text: &'a mut TextEngine,
    /// Where the pointer was last seen, in logical pixels.
    pub pointer: (f64, f64),
    /// Whether the primary pointer button is down.
    pub pointer_down: bool,
    /// Where it went down, while it is still down.
    ///
    /// What a captured pointer would be, without the capture. A slider tracks
    /// the pointer past its own edge because the press *started* on it — which
    /// is a fact about this drag, not state the slider has to hold and later
    /// remember to clear.
    pub press_origin: Option<(f64, f64)>,
    /// Which control has the keyboard, if the surface tracks that.
    pub focus: Option<FocusId>,
    /// The font stack the interface draws with.
    pub fonts: FontStack,
    /// Every colour and measurement the interface is drawn from.
    pub theme: Theme,
}

impl<'a> Cx<'a> {
    /// A context over `text`, with the pointer offscreen and nothing focused.
    pub fn new(text: &'a mut TextEngine) -> Self {
        Self {
            text,
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            focus: None,
            fonts: FontStack::parse_css("system-ui, sans-serif"),
            theme: Theme::light(),
        }
    }

    /// Width of `content` at `size`, in logical pixels.
    pub fn measure_text(&mut self, content: &str, size: f32) -> f64 {
        f64::from(self.text.measure(content, &self.fonts, size).width)
    }

    /// The height of one line of interface text at `size`.
    pub fn line_height(&self, size: f32) -> f64 {
        f64::from(size) * self.theme.line_height
    }

    /// Whether the pointer is inside `rect`.
    pub fn hovered(&self, rect: Rect) -> bool {
        rect.contains(self.pointer.0, self.pointer.1)
    }

    /// Whether the pointer is inside `rect` with the button held down.
    pub fn pressed(&self, rect: Rect) -> bool {
        self.pointer_down && self.hovered(rect)
    }

    /// Whether the drag in progress began inside `rect`.
    pub fn dragging_from(&self, rect: Rect) -> bool {
        self.press_origin.is_some_and(|(x, y)| rect.contains(x, y))
    }
}

/// What a control is, to something that cannot see it.
///
/// The layer's own vocabulary rather than the accessibility library's, for the
/// same reason `Widget<A>` is generic over the action: this layer knows nothing
/// of the browser and nothing of the platform, and a `Role` from a windowing
/// crate in here would put one in every control that has a name.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Something to press.
    Button,
    /// A box that is ticked or not.
    CheckBox,
    /// One of several exclusive choices.
    RadioButton,
    /// A setting that takes effect the moment it is thrown.
    Switch,
    /// A field text is typed into.
    TextInput,
    /// A value picked along a line.
    Slider,
    /// One tab of several.
    Tab,
    /// A row of a menu.
    MenuItem,
    /// Something with a name and no behaviour: a heading, a caption.
    Label,
}

/// What a widget says about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Described {
    /// Where it is, so a reader can point at it.
    pub rect: Rect,
    /// What kind of thing it is.
    pub role: Role,
    /// What it is called.
    pub label: String,
    /// What it currently says or holds, when that is not its name.
    pub value: Option<String>,
    /// Whether it holds the keyboard.
    pub focused: bool,
    /// Whether it is drawn but will not respond.
    pub enabled: bool,
}

/// One piece of a surface, reporting actions of type `A`.
///
/// The four verbs run in order, once per frame for the first three: `measure`
/// bottom up, then `place` and `draw` top down. `event` runs between frames
/// against the rectangles `place` stored.
pub trait Widget<A> {
    /// How big this wants to be, given at most `available`.
    ///
    /// Bottom up: a container calls this on its children before it can answer
    /// for itself.
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size;

    /// Take `rect`, and give the children theirs.
    ///
    /// The only place geometry is decided. A widget stores `rect` here and every
    /// later pass reads it back rather than working it out again.
    fn place(&mut self, rect: Rect, cx: &mut Cx);

    /// Paint into `list`, at the rectangle last placed.
    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList);

    /// Handle `event`, and say what the surface should do about it.
    ///
    /// `None` means *not mine* — a container keeps offering the event to further
    /// children until one answers.
    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        let _ = (event, cx);
        None
    }

    /// Say what this is, for something that cannot see it.
    ///
    /// The default is *nothing to say*, so a leaf that is decoration — a gap, a
    /// hairline, a mark inside a button that already has a name — stays silent
    /// rather than filling a screen reader with furniture. A container forwards
    /// to its children; a control pushes itself.
    fn describe(&self, out: &mut Vec<Described>) {
        let _ = out;
    }

    /// The words inside this, for a control that takes its name from them.
    ///
    /// A button holds no label of its own: it is wrapped around one. Asking the
    /// child is what lets the button be named without the caller saying the same
    /// string twice, and without a second copy of it to fall out of step.
    fn label_text(&self) -> Option<String> {
        None
    }

    /// Share of a row's or column's leftover space this claims, `0.0` for none.
    ///
    /// The default is intrinsic size, and [`Flex`] asks for the remainder,
    /// rather than the other way around: most of a surface is fixed-size
    /// controls beside one thing that should take what is left, so filling on
    /// request is the rarer case and gets the wrapper.
    fn flex(&self) -> f64 {
        0.0
    }
}

/// A boxed child. Containers hold these; leaves do not care.
pub type Child<A> = Box<dyn Widget<A>>;

// --- leaves ---------------------------------------------------------------

/// One line of text, drawn from its top-left.
pub struct Label {
    content: String,
    size: f32,
    color: Color,
    rect: Rect,
}

impl Label {
    /// A label showing `content`.
    pub fn new(content: impl Into<String>, size: f32, color: Color) -> Self {
        Self {
            content: content.into(),
            size,
            color,
            rect: Rect::ZERO,
        }
    }

    /// Replace what is shown. Cheap: nothing is retained until the next measure.
    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
    }
}

impl<A> Widget<A> for Label {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let width = cx.measure_text(&self.content, self.size);
        Size::new(width.min(available.width), cx.line_height(self.size))
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        if self.content.is_empty() || self.rect.width <= 0.0 {
            return;
        }
        let shaped = cx.text.shape(
            &self.content,
            &cx.fonts,
            self.size,
            Some(self.rect.width as f32),
        );
        // One line only: a toolbar that wrapped would push the page down the
        // window, and a settings row that wrapped would overlap the next.
        for run in shaped.runs.iter().filter(|run| run.line == 0) {
            list.push_glyphs(
                &run.font,
                run.font_size,
                run.normalized_coords.clone(),
                Brush::Solid(self.color),
                Affine::translate((self.rect.x, self.rect.y)),
                true,
                run.glyphs.clone(),
            );
        }
    }
}

/// Text that wraps to as many lines as it needs.
///
/// Separate from [`Label`] rather than an option on it, because the two answer
/// `measure` differently in kind: one line has a height that is known before the
/// width is, and wrapped text does not.
pub struct Paragraph {
    content: String,
    size: f32,
    color: Color,
    rect: Rect,
}

impl Paragraph {
    /// A paragraph showing `content`.
    pub fn new(content: impl Into<String>, size: f32, color: Color) -> Self {
        Self {
            content: content.into(),
            size,
            color,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Paragraph {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        if self.content.is_empty() || available.width <= 0.0 {
            return Size::ZERO;
        }
        let shaped = cx.text.shape(
            &self.content,
            &cx.fonts,
            self.size,
            Some(available.width as f32),
        );
        let lines = shaped.runs.iter().map(|run| run.line).max().unwrap_or(0) + 1;
        Size::new(available.width, lines as f64 * cx.line_height(self.size))
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        if self.content.is_empty() || self.rect.width <= 0.0 {
            return;
        }
        let line_height = cx.line_height(self.size);
        let shaped = cx.text.shape(
            &self.content,
            &cx.fonts,
            self.size,
            Some(self.rect.width as f32),
        );
        for run in &shaped.runs {
            list.push_glyphs(
                &run.font,
                run.font_size,
                run.normalized_coords.clone(),
                Brush::Solid(self.color),
                Affine::translate((self.rect.x, self.rect.y + run.line as f64 * line_height)),
                true,
                run.glyphs.clone(),
            );
        }
    }

    fn flex(&self) -> f64 {
        0.0
    }
}

/// Fixed empty space. The separator between controls in a row.
pub struct Gap {
    size: Size,
}

impl Gap {
    /// A gap of `width` by `height`.
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            size: Size::new(width, height),
        }
    }
}

impl<A> Widget<A> for Gap {
    fn measure(&mut self, _available: Size, _cx: &mut Cx) -> Size {
        self.size
    }
    fn place(&mut self, _rect: Rect, _cx: &mut Cx) {}
    fn draw(&mut self, _cx: &mut Cx, _list: &mut DisplayList) {}
}

/// A leaf that paints itself, at a size it asks for.
///
/// Every mark in the interface that is not text is a path: an arrow, a cross, a
/// plus. Drawing them rather than typing them keeps them the same on machines
/// whose fonts disagree, and a glyph the system does not have is a hollow box
/// where a button should be.
pub struct Painted<F> {
    paint: F,
    size: Size,
    rect: Rect,
}

impl<F> Painted<F>
where
    F: FnMut(Rect, &mut Cx, &mut DisplayList),
{
    /// A leaf `width` by `height` that paints itself with `paint`.
    pub fn new(width: f64, height: f64, paint: F) -> Self {
        Self {
            paint,
            size: Size::new(width, height),
            rect: Rect::ZERO,
        }
    }
}

impl<A, F> Widget<A> for Painted<F>
where
    F: FnMut(Rect, &mut Cx, &mut DisplayList),
{
    fn measure(&mut self, _available: Size, _cx: &mut Cx) -> Size {
        self.size
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        (self.paint)(self.rect, cx, list);
    }
}

// --- wrappers -------------------------------------------------------------

/// A rounded rectangle behind a child, optionally a different one under the
/// pointer or while it is held down.
pub struct Background<A> {
    child: Child<A>,
    color: Color,
    hover: Option<Color>,
    press: Option<Color>,
    radii: RoundedRectRadii,
    rect: Rect,
}

impl<A> Background<A> {
    /// Paint `color` behind `child`, rounded equally at all four corners.
    pub fn new(color: Color, radius: f64, child: Child<A>) -> Self {
        Self {
            child,
            color,
            hover: None,
            press: None,
            radii: RoundedRectRadii::from_single_radius(radius),
            rect: Rect::ZERO,
        }
    }

    /// Paint `color` behind `child`, rounded per corner from the top left round.
    pub fn rounded(color: Color, radii: impl Into<RoundedRectRadii>, child: Child<A>) -> Self {
        Self {
            child,
            color,
            hover: None,
            press: None,
            radii: radii.into(),
            rect: Rect::ZERO,
        }
    }

    /// Paint `color` instead while the pointer is inside.
    pub fn on_hover(mut self, color: Color) -> Self {
        self.hover = Some(color);
        self
    }

    /// Paint `color` instead while the pointer is inside and held down.
    ///
    /// Worth the extra shade: without it a press has no acknowledgement until
    /// whatever it triggered finishes, and a slow action reads as a dead button.
    pub fn on_press(mut self, color: Color) -> Self {
        self.press = Some(color);
        self
    }
}

impl<A> Widget<A> for Background<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.child.place(rect, cx);
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let color = match (self.press, self.hover) {
            (Some(press), _) if cx.pressed(self.rect) => press,
            (_, Some(hover)) if cx.hovered(self.rect) => hover,
            _ => self.color,
        };
        fill_rounded(list, self.rect, color, self.radii);
        self.child.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        self.child.flex()
    }
}

/// Space around a child.
pub struct Padding<A> {
    child: Child<A>,
    insets: Insets,
    rect: Rect,
}

impl<A> Padding<A> {
    /// Inset `child` by `insets`.
    pub fn new(insets: Insets, child: Child<A>) -> Self {
        Self {
            child,
            insets,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Padding<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let inner = Size::new(
            (available.width - self.insets.horizontal()).max(0.0),
            (available.height - self.insets.vertical()).max(0.0),
        );
        let child = self.child.measure(inner, cx);
        Size::new(
            child.width + self.insets.horizontal(),
            child.height + self.insets.vertical(),
        )
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.child.place(
            Rect::new(
                rect.x + self.insets.left,
                rect.y + self.insets.top,
                (rect.width - self.insets.horizontal()).max(0.0),
                (rect.height - self.insets.vertical()).max(0.0),
            ),
            cx,
        );
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        self.child.flex()
    }
}

/// A child pinned to a size, in either axis or both.
pub struct Fixed<A> {
    child: Child<A>,
    width: Option<f64>,
    height: Option<f64>,
}

impl<A> Fixed<A> {
    /// Force both axes.
    pub fn new(width: f64, height: f64, child: Child<A>) -> Self {
        Self {
            child,
            width: Some(width),
            height: Some(height),
        }
    }

    /// Force the width, leaving the height to the child.
    pub fn width(width: f64, child: Child<A>) -> Self {
        Self {
            child,
            width: Some(width),
            height: None,
        }
    }

    /// Force the height, leaving the width to the child.
    pub fn height(height: f64, child: Child<A>) -> Self {
        Self {
            child,
            width: None,
            height: Some(height),
        }
    }
}

impl<A> Widget<A> for Fixed<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        // The child is measured against the pinned size, not against what the
        // parent offered. Otherwise a paragraph counts its lines at one width
        // and is drawn at another, and the difference is the number of lines
        // that end up over whatever was placed underneath it.
        let inner = Size::new(
            self.width.unwrap_or(available.width).min(available.width),
            self.height.unwrap_or(available.height),
        );
        let child = self.child.measure(inner, cx);
        Size::new(
            self.width.unwrap_or(child.width),
            self.height.unwrap_or(child.height),
        )
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        // A pinned axis is honoured at placement too, and the child is centred
        // in whatever it was given on that axis. Passing the whole rectangle
        // through would make the pin a suggestion that only `measure` heard: a
        // 30-tall control in a 42-tall row would draw 42 tall.
        let width = self.width.unwrap_or(rect.width).min(rect.width);
        let height = self.height.unwrap_or(rect.height).min(rect.height);
        self.child.place(
            Rect::new(
                rect.x + (rect.width - width) / 2.0,
                rect.y + (rect.height - height) / 2.0,
                width,
                height,
            ),
            cx,
        );
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        // A pinned width is a refusal to stretch across; a pinned height is not,
        // and the field in the toolbar is exactly that — one fixed height, all
        // the width that is going.
        match self.width {
            Some(_) => 0.0,
            None => self.child.flex(),
        }
    }
}

/// A child that claims a share of what a row or column has left over.
pub struct Flex<A> {
    child: Child<A>,
    share: f64,
}

impl<A> Flex<A> {
    /// Claim `share` of the leftover, relative to the other flexible siblings.
    pub fn new(share: f64, child: Child<A>) -> Self {
        Self { child, share }
    }
}

impl<A> Widget<A> for Flex<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.child.place(rect, cx);
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        self.share
    }
}

/// A child placed by fraction within the space given to it.
///
/// `0.0` is left or top, `0.5` centre, `1.0` right or bottom, and anything in
/// between is what the fraction says — one wrapper instead of an enum that has
/// to grow a case every time a design wants a third of the way down.
pub struct Align<A> {
    child: Child<A>,
    x: f64,
    y: f64,
}

impl<A> Align<A> {
    /// Place `child` at fractions `x` and `y` of the leftover space.
    pub fn new(x: f64, y: f64, child: Child<A>) -> Self {
        Self { child, x, y }
    }

    /// Centre in both axes.
    pub fn centre(child: Child<A>) -> Self {
        Self::new(0.5, 0.5, child)
    }

    /// Against the left edge, centred down the middle.
    pub fn left(child: Child<A>) -> Self {
        Self::new(0.0, 0.5, child)
    }

    /// Against the right edge, centred down the middle.
    pub fn right(child: Child<A>) -> Self {
        Self::new(1.0, 0.5, child)
    }
}

impl<A> Widget<A> for Align<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        let wanted = self.child.measure(Size::new(rect.width, rect.height), cx);
        let width = wanted.width.min(rect.width);
        let height = wanted.height.min(rect.height);
        self.child.place(
            Rect::new(
                rect.x + (rect.width - width) * self.x,
                rect.y + (rect.height - height) * self.y,
                width,
                height,
            ),
            cx,
        );
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }
}

/// A child that is painted only within its own rectangle.
///
/// The clip is a compositing layer in the display list rather than a rectangle
/// test at paint time, so it applies to glyphs and images as well as to fills —
/// text that overflows a scrolling panel has to be cut, not merely not drawn.
pub struct Clip<A> {
    child: Child<A>,
    radius: f64,
    rect: Rect,
}

impl<A> Clip<A> {
    /// Clip `child` to the rectangle it is given.
    pub fn new(child: Child<A>) -> Self {
        Self {
            child,
            radius: 0.0,
            rect: Rect::ZERO,
        }
    }

    /// Clip to a rounded rectangle instead of a square one.
    pub fn rounded(radius: f64, child: Child<A>) -> Self {
        Self {
            child,
            radius,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Clip<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.child.place(rect, cx);
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        list.push(DisplayItem::PushLayer {
            blend: BlendMode::default(),
            alpha: 1.0,
            transform: Affine::IDENTITY,
            clip: RoundedRect::from_rect(self.rect.to_kurbo(), self.radius).to_path(0.1),
        });
        self.child.draw(cx, list);
        list.push(DisplayItem::PopLayer);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        // A press outside the clip never reaches the child, however far the
        // child was placed: what is not drawn cannot be clicked. A key is not
        // aimed with the pointer, so it passes through regardless — a control
        // reached by Tab answers wherever the pointer is resting.
        if event.from_pointer() && !cx.hovered(self.rect) {
            return None;
        }
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        self.child.flex()
    }
}

/// How far a [`Scroll`] could be scrolled, reported back to whoever owns the
/// offset.
///
/// A scrolling panel is the one thing here that cannot be a pure view of its
/// caller's state: how far it *can* go depends on how tall the content turned
/// out, which is not known until the frame is measured. So the frame reports it
/// and the caller clamps its own offset before the next one — rather than the
/// panel keeping a scroll position of its own that the caller would then have
/// two copies of.
pub type Overflow = Rc<Cell<f64>>;

/// A panel that shows a window onto a taller child.
pub struct Scroll<A> {
    child: Child<A>,
    offset: f64,
    overflow: Overflow,
    content: f64,
    rect: Rect,
}

impl<A> Scroll<A> {
    /// Show `child` shifted up by `offset`, reporting the overflow into
    /// `overflow`.
    pub fn new(offset: f64, overflow: Overflow, child: Child<A>) -> Self {
        Self {
            child,
            offset,
            overflow,
            content: 0.0,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Scroll<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        // The child is measured against unbounded height — the whole point is
        // that it may be taller than what it is shown in.
        self.content = self
            .child
            .measure(Size::new(available.width, f64::INFINITY), cx)
            .height;
        Size::new(available.width, available.height)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.overflow.set((self.content - rect.height).max(0.0));
        let offset = self.offset.clamp(0.0, self.overflow.get());
        self.child.place(
            Rect::new(rect.x, rect.y - offset, rect.width, self.content),
            cx,
        );
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        list.push(DisplayItem::PushLayer {
            blend: BlendMode::default(),
            alpha: 1.0,
            transform: Affine::IDENTITY,
            clip: self.rect.to_kurbo().to_path(0.1),
        });
        self.child.draw(cx, list);
        list.push(DisplayItem::PopLayer);

        // A bar only when there is something to scroll, drawn over the content
        // rather than beside it: a bar that took width would reflow the panel
        // the moment its content grew past the bottom.
        let overflow = self.overflow.get();
        if overflow > 0.0 {
            let theme = cx.theme.clone();
            let track = self.rect.height;
            let thumb = (track * (track / self.content)).max(24.0);
            let travel = track - thumb;
            let offset = self.offset.clamp(0.0, overflow);
            fill_rounded(
                list,
                Rect::new(
                    self.rect.x + self.rect.width - 7.0,
                    self.rect.y + travel * (offset / overflow),
                    4.0,
                    thumb,
                ),
                theme.ink_disabled,
                2.0,
            );
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        // The wheel and a press belong to whatever the pointer is over; a key
        // belongs to whatever holds the focus, which may be scrolled out of
        // sight and is still the thing that answers.
        if event.from_pointer() && !cx.hovered(self.rect) {
            return None;
        }
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        1.0
    }
}

/// Children drawn one over another, in the space they all share.
///
/// Drawing runs first to last, so the last child is on top. Events run the other
/// way — a popup that covers a button must be offered the press before the
/// button gets it, or the press falls through what is drawn over it, which is
/// the classic overlay bug.
///
/// The children are not laid out relative to each other: each is given the whole
/// rectangle and is expected to place itself within it, usually by being wrapped
/// in an [`Align`] or an [`Anchored`].
pub struct Overlay<A> {
    children: Vec<Child<A>>,
    rect: Rect,
}

impl<A> Overlay<A> {
    /// Stack `children`, the last of them on top.
    pub fn new(children: Vec<Child<A>>) -> Self {
        Self {
            children,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Overlay<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let mut size = Size::ZERO;
        for child in &mut self.children {
            let child = child.measure(available, cx);
            size.width = size.width.max(child.width);
            size.height = size.height.max(child.height);
        }
        size
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        for child in &mut self.children {
            child.place(rect, cx);
        }
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        for child in &mut self.children {
            child.draw(cx, list);
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        for child in self.children.iter_mut().rev() {
            if let Some(action) = child.event(event, cx) {
                return Some(action);
            }
        }
        None
    }
}

/// A child of its own size, put at a point rather than in a flow.
///
/// What a popup needs: the panel under the button that opened it is not part of
/// any row, and pinning it by its top-right corner is what keeps it on screen
/// when the window is narrow.
pub struct Anchored<A> {
    child: Child<A>,
    at: (f64, f64),
    from_right: bool,
    rect: Rect,
}

impl<A> Anchored<A> {
    /// Put `child`'s top-left corner at `x`, `y` within the space given.
    pub fn at(x: f64, y: f64, child: Child<A>) -> Self {
        Self {
            child,
            at: (x, y),
            from_right: false,
            rect: Rect::ZERO,
        }
    }

    /// Put `child`'s top-*right* corner `x` in from the right edge.
    pub fn from_right(x: f64, y: f64, child: Child<A>) -> Self {
        Self {
            child,
            at: (x, y),
            from_right: true,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Anchored<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        let wanted = self.child.measure(Size::new(rect.width, rect.height), cx);
        let x = if self.from_right {
            rect.x + rect.width - self.at.0 - wanted.width
        } else {
            rect.x + self.at.0
        };
        self.child.place(
            Rect::new(
                // Never off the left edge, however narrow the window: a panel
                // that cannot be reached cannot be dismissed either.
                x.max(rect.x),
                rect.y + self.at.1,
                wanted.width.min(rect.width),
                wanted.height,
            ),
            cx,
        );
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }
}

// --- containers -----------------------------------------------------------

/// Which way a [`Stack`] runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

/// Children in a line, separated by a gap.
pub struct Stack<A> {
    axis: Axis,
    gap: f64,
    children: Vec<Child<A>>,
    measured: Vec<Size>,
    rect: Rect,
}

impl<A> Stack<A> {
    /// A row.
    pub fn row(gap: f64, children: Vec<Child<A>>) -> Self {
        Self::new(Axis::Horizontal, gap, children)
    }

    /// A column.
    pub fn column(gap: f64, children: Vec<Child<A>>) -> Self {
        Self::new(Axis::Vertical, gap, children)
    }

    fn new(axis: Axis, gap: f64, children: Vec<Child<A>>) -> Self {
        Self {
            axis,
            gap,
            measured: vec![Size::ZERO; children.len()],
            children,
            rect: Rect::ZERO,
        }
    }

    fn main(&self, size: Size) -> f64 {
        match self.axis {
            Axis::Horizontal => size.width,
            Axis::Vertical => size.height,
        }
    }

    fn gaps(&self) -> f64 {
        self.gap * (self.children.len().saturating_sub(1)) as f64
    }
}

impl<A> Widget<A> for Stack<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let mut main = self.gaps();
        let mut cross: f64 = 0.0;
        for (index, child) in self.children.iter_mut().enumerate() {
            let size = child.measure(available, cx);
            self.measured[index] = size;
            match self.axis {
                Axis::Horizontal => {
                    main += size.width;
                    cross = cross.max(size.height);
                }
                Axis::Vertical => {
                    main += size.height;
                    cross = cross.max(size.width);
                }
            }
        }
        match self.axis {
            Axis::Horizontal => Size::new(main, cross),
            Axis::Vertical => Size::new(cross, main),
        }
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        if self.measured.len() != self.children.len() {
            self.measured = vec![Size::ZERO; self.children.len()];
        }
        // A flexible child's own measurement does not count toward what the
        // fixed ones need: it takes its size from the leftover, not from its
        // content. Counting it would let a paragraph that measured itself
        // against the whole row push everything after it off the end — which is
        // exactly how a settings control and an address field end up outside
        // the window they were drawn in.
        let intrinsic: f64 = self
            .children
            .iter()
            .zip(&self.measured)
            .filter(|(child, _)| child.flex() == 0.0)
            .map(|(_, size)| self.main(*size))
            .sum::<f64>()
            + self.gaps();

        let shares: f64 = self.children.iter().map(|child| child.flex()).sum();
        let leftover = (self.main(Size::new(rect.width, rect.height)) - intrinsic).max(0.0);

        let mut offset = 0.0;
        for index in 0..self.children.len() {
            let share = self.children[index].flex();
            let mut extent = if share > 0.0 {
                0.0
            } else {
                self.main(self.measured[index])
            };
            if shares > 0.0 {
                extent += leftover * share / shares;
            }
            let child_rect = match self.axis {
                // Cross axis is filled: a button in a toolbar is as tall as the
                // toolbar unless something wraps it to say otherwise.
                Axis::Horizontal => Rect::new(rect.x + offset, rect.y, extent, rect.height),
                Axis::Vertical => Rect::new(rect.x, rect.y + offset, rect.width, extent),
            };
            self.children[index].place(child_rect, cx);
            offset += extent + self.gap;
        }
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        for child in &mut self.children {
            child.draw(cx, list);
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        // Front to back would be right if children overlapped. In a stack they
        // do not, so document order is fine and cheaper to reason about.
        for child in &mut self.children {
            if let Some(action) = child.event(event, cx) {
                return Some(action);
            }
        }
        None
    }
}

// --- controls -------------------------------------------------------------

/// Something to press, which reports one action when pressed.
///
/// The button holds no colour and no label: it is wrapped in [`Background`] and
/// given a child. What it holds is the rectangle it was placed at and the action
/// — the two things only it can know.
pub struct Button<A> {
    child: Child<A>,
    action: A,
    enabled: bool,
    focus: Option<FocusId>,
    rect: Rect,
}

impl<A> Button<A> {
    /// A button around `child` that reports `action`.
    pub fn new(action: A, child: Child<A>) -> Self {
        Self {
            child,
            action,
            enabled: true,
            focus: None,
            rect: Rect::ZERO,
        }
    }

    /// A button that is drawn but does nothing, like *back* with no history.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The name the surface knows this button by, for keyboard traversal.
    ///
    /// Given one, the button answers the surface's activation key as well as a
    /// press — which is the whole of what focus means to a button.
    pub fn focus(mut self, id: FocusId) -> Self {
        self.focus = Some(id);
        self
    }
}

impl<A: Clone> Widget<A> for Button<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.child.place(rect, cx);
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
        if self.enabled && self.focus.is_some() && cx.focus == self.focus {
            controls::focus_ring(&cx.theme.clone(), list, self.rect, cx.theme.radius);
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        // The child is offered the event first, so a button inside a button —
        // the close cross inside a tab — wins the press it is sitting on. The
        // outer one would otherwise swallow every press over its own area,
        // which is all of them.
        if let Some(action) = self.child.event(event, cx) {
            return Some(action);
        }
        if !self.enabled {
            return None;
        }
        match event {
            Event::PointerPressed if cx.hovered(self.rect) => Some(self.action.clone()),
            Event::Activate if self.focus.is_some() && cx.focus == self.focus => {
                Some(self.action.clone())
            }
            _ => None,
        }
    }

    fn flex(&self) -> f64 {
        self.child.flex()
    }
}

// --- painting -------------------------------------------------------------

/// A filled rectangle, rounded when a radius is above zero.
///
/// Fully transparent fills are dropped rather than pushed: a control that paints
/// nothing until it is hovered would otherwise fill a rectangle with nothing on
/// every frame, and the display list is a thing we compare against goldens.
pub fn fill_rounded(
    list: &mut DisplayList,
    rect: Rect,
    color: Color,
    radii: impl Into<RoundedRectRadii>,
) {
    if rect.width <= 0.0 || rect.height <= 0.0 || color.components[3] <= 0.0 {
        return;
    }
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(color),
        brush_transform: None,
        shape: RoundedRect::from_rect(rect.to_kurbo(), radii).to_path(0.1),
    });
}

/// A filled rectangle with the same radius at every corner.
pub fn fill(list: &mut DisplayList, rect: Rect, color: Color, radius: f64) {
    fill_rounded(list, rect, color, radius);
}

/// A ring of `width` inside `rect`, drawn as the gap between two rounded
/// rectangles.
///
/// A stroked path would straddle the edge, putting half the line outside the
/// rectangle the control was given and over whatever is next to it. The
/// even-odd rule is what makes the middle a hole rather than more paint.
pub fn ring(list: &mut DisplayList, rect: Rect, color: Color, radius: f64, width: f64) {
    if rect.width <= width * 2.0 || rect.height <= width * 2.0 || color.components[3] <= 0.0 {
        return;
    }
    let inner = Rect::new(
        rect.x + width,
        rect.y + width,
        rect.width - width * 2.0,
        rect.height - width * 2.0,
    );
    let mut path = BezPath::new();
    let outer_shape = RoundedRect::from_rect(rect.to_kurbo(), radius);
    let inner_shape = RoundedRect::from_rect(inner.to_kurbo(), (radius - width).max(0.0));
    outer_shape
        .path_elements(0.05)
        .for_each(|element| path.push(element));
    inner_shape
        .path_elements(0.05)
        .for_each(|element| path.push(element));
    list.push(DisplayItem::Fill {
        style: Fill::EvenOdd,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(color),
        brush_transform: None,
        shape: path,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// What the widgets under test report. Any type will do, which is the point
    /// of the layer being generic: it has never heard of the browser's actions.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Act {
        First,
        Second,
    }

    /// Where each fake child was placed, in the order they were placed.
    type Log = Rc<RefCell<Vec<(usize, Rect)>>>;

    /// A leaf of a known size that records its placement, so container geometry
    /// can be tested without a font and without reaching through a `dyn Widget`.
    struct Fake {
        id: usize,
        want: Size,
        log: Log,
    }

    impl Fake {
        fn new(id: usize, width: f64, height: f64, log: &Log) -> Self {
            Self {
                id,
                want: Size::new(width, height),
                log: Rc::clone(log),
            }
        }
    }

    impl<A> Widget<A> for Fake {
        fn measure(&mut self, _available: Size, _cx: &mut Cx) -> Size {
            self.want
        }
        fn place(&mut self, rect: Rect, _cx: &mut Cx) {
            self.log.borrow_mut().push((self.id, rect));
        }
        fn draw(&mut self, _cx: &mut Cx, _list: &mut DisplayList) {}
    }

    /// The rectangle child `id` was last placed at.
    fn placed(log: &Log, id: usize) -> Rect {
        log.borrow()
            .iter()
            .rev()
            .find(|(each, _)| *each == id)
            .map(|(_, rect)| *rect)
            .expect("child was never placed")
    }

    fn cx(text: &mut TextEngine) -> Cx<'_> {
        Cx::new(text)
    }

    #[test]
    fn row_measures_children_plus_gaps() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut row: Stack<Act> = Stack::row(
            4.0,
            vec![
                Box::new(Fake::new(0, 10.0, 8.0, &log)),
                Box::new(Fake::new(1, 20.0, 12.0, &log)),
            ],
        );
        // 10 + 4 + 20 across, and as tall as the tallest.
        assert_eq!(
            row.measure(Size::new(200.0, 40.0), &mut cx),
            Size::new(34.0, 12.0)
        );
    }

    #[test]
    fn row_places_children_in_order_and_fills_the_cross_axis() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut row: Stack<Act> = Stack::row(
            4.0,
            vec![
                Box::new(Fake::new(0, 10.0, 8.0, &log)),
                Box::new(Fake::new(1, 20.0, 12.0, &log)),
            ],
        );
        row.measure(Size::new(200.0, 40.0), &mut cx);
        row.place(Rect::new(5.0, 6.0, 200.0, 40.0), &mut cx);

        assert_eq!(placed(&log, 0), Rect::new(5.0, 6.0, 10.0, 40.0));
        assert_eq!(placed(&log, 1), Rect::new(19.0, 6.0, 20.0, 40.0));
    }

    #[test]
    fn flex_takes_the_leftover() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut row: Stack<Act> = Stack::row(
            0.0,
            vec![
                Box::new(Fake::new(0, 10.0, 8.0, &log)),
                Box::new(Flex::new(1.0, Box::new(Fake::new(1, 10.0, 8.0, &log)))),
            ],
        );
        row.measure(Size::new(100.0, 20.0), &mut cx);
        row.place(Rect::new(0.0, 0.0, 100.0, 20.0), &mut cx);

        // 100 across, 20 of it intrinsic: the 80 left over all goes to the one
        // that asked, and the fixed one keeps the width it measured.
        assert_eq!(placed(&log, 0).width, 10.0);
        assert_eq!(placed(&log, 1), Rect::new(10.0, 0.0, 90.0, 20.0));
    }

    #[test]
    fn two_flexible_children_split_the_leftover_by_share() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut row: Stack<Act> = Stack::row(
            0.0,
            vec![
                Box::new(Flex::new(1.0, Box::new(Fake::new(0, 0.0, 8.0, &log)))),
                Box::new(Flex::new(3.0, Box::new(Fake::new(1, 0.0, 8.0, &log)))),
            ],
        );
        row.measure(Size::new(100.0, 20.0), &mut cx);
        row.place(Rect::new(0.0, 0.0, 100.0, 20.0), &mut cx);

        assert_eq!(placed(&log, 0).width, 25.0);
        assert_eq!(placed(&log, 1).width, 75.0);
    }

    #[test]
    fn padding_inflates_the_measure_and_insets_the_placement() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut padded: Padding<Act> =
            Padding::new(Insets::all(3.0), Box::new(Fake::new(0, 10.0, 8.0, &log)));
        assert_eq!(
            padded.measure(Size::new(100.0, 100.0), &mut cx),
            Size::new(16.0, 14.0)
        );
        padded.place(Rect::new(0.0, 0.0, 16.0, 14.0), &mut cx);
        assert_eq!(placed(&log, 0), Rect::new(3.0, 3.0, 10.0, 8.0));
    }

    #[test]
    fn align_centres_a_child_in_the_space_it_was_given() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut centred: Align<Act> = Align::centre(Box::new(Fake::new(0, 10.0, 10.0, &log)));
        centred.place(Rect::new(0.0, 0.0, 50.0, 30.0), &mut cx);
        assert_eq!(placed(&log, 0), Rect::new(20.0, 10.0, 10.0, 10.0));
    }

    #[test]
    fn a_button_answers_only_for_presses_inside_it() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut button = Button::new(Act::First, Box::new(Fake::new(0, 10.0, 10.0, &log)));
        button.place(Rect::new(10.0, 10.0, 20.0, 20.0), &mut cx);

        cx.pointer = (15.0, 15.0);
        assert_eq!(
            button.event(&Event::PointerPressed, &mut cx),
            Some(Act::First)
        );

        cx.pointer = (5.0, 5.0);
        assert_eq!(button.event(&Event::PointerPressed, &mut cx), None);
    }

    #[test]
    fn a_disabled_button_answers_for_nothing() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut button =
            Button::new(Act::First, Box::new(Fake::new(0, 10.0, 10.0, &log))).enabled(false);
        button.place(Rect::new(0.0, 0.0, 20.0, 20.0), &mut cx);
        cx.pointer = (5.0, 5.0);
        assert_eq!(button.event(&Event::PointerPressed, &mut cx), None);
    }

    #[test]
    fn a_focused_button_answers_the_activation_key_with_the_pointer_elsewhere() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut button =
            Button::new(Act::Second, Box::new(Fake::new(0, 10.0, 10.0, &log))).focus(7);
        button.place(Rect::new(0.0, 0.0, 20.0, 20.0), &mut cx);
        cx.pointer = (500.0, 500.0);

        cx.focus = Some(7);
        assert_eq!(button.event(&Event::Activate, &mut cx), Some(Act::Second));

        cx.focus = Some(8);
        assert_eq!(button.event(&Event::Activate, &mut cx), None);
    }

    #[test]
    fn a_row_offers_an_event_to_each_child_until_one_takes_it() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut row = Stack::row(
            0.0,
            vec![
                Box::new(Button::new(
                    Act::First,
                    Box::new(Fake::new(0, 10.0, 10.0, &log)),
                )) as Child<Act>,
                Box::new(Button::new(
                    Act::Second,
                    Box::new(Fake::new(1, 10.0, 10.0, &log)),
                )),
            ],
        );
        row.measure(Size::new(20.0, 10.0), &mut cx);
        row.place(Rect::new(0.0, 0.0, 20.0, 10.0), &mut cx);

        cx.pointer = (15.0, 5.0);
        assert_eq!(
            row.event(&Event::PointerPressed, &mut cx),
            Some(Act::Second)
        );
    }

    #[test]
    fn a_press_outside_a_clip_never_reaches_what_it_covers() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let mut clipped: Clip<Act> = Clip::new(Box::new(Button::new(
            Act::First,
            Box::new(Fake::new(0, 10.0, 10.0, &log)),
        )));
        clipped.place(Rect::new(0.0, 0.0, 20.0, 20.0), &mut cx);

        cx.pointer = (10.0, 10.0);
        assert_eq!(
            clipped.event(&Event::PointerPressed, &mut cx),
            Some(Act::First)
        );

        // Inside the child, which was placed the full size, but outside the clip.
        cx.pointer = (10.0, 30.0);
        assert_eq!(clipped.event(&Event::PointerPressed, &mut cx), None);
    }

    #[test]
    fn a_scroll_reports_how_far_it_could_go_and_shifts_its_child_by_no_more() {
        let mut text = TextEngine::new();
        let mut cx = cx(&mut text);
        let log: Log = Log::default();
        let overflow: Overflow = Overflow::default();

        // A 300-tall child in a 100-tall panel can travel 200.
        let mut scroll: Scroll<Act> = Scroll::new(
            500.0,
            Rc::clone(&overflow),
            Box::new(Fake::new(0, 50.0, 300.0, &log)),
        );
        scroll.measure(Size::new(50.0, 100.0), &mut cx);
        scroll.place(Rect::new(0.0, 0.0, 50.0, 100.0), &mut cx);

        assert_eq!(overflow.get(), 200.0);
        // The offset asked for was past the end, so it was clamped rather than
        // scrolling the content off the top.
        assert_eq!(placed(&log, 0).y, -200.0);
    }
}
