//! The controls the browser's surfaces are assembled from.
//!
//! Shared on purpose. The toolbar needs a button and a field; the settings page
//! needs a button, a field, a checkbox, a switch, a slider and a set of choices,
//! and it is drawn with these rather than with HTML so that a preference looks
//! like the browser it belongs to and not like a page it loaded. One definition
//! of each means the two surfaces cannot disagree about what a pressed button
//! looks like.
//!
//! Every control here is generic over the action it reports, so none of them has
//! heard of the browser or of the settings page. A control is a *view* of a
//! value its caller owns — a checkbox is told whether it is ticked — and what it
//! reports is an action the caller applies to its own state. The next frame
//! shows the result.
//!
//! Anything that can hold the keyboard takes a [`Focus`] and claims its own id
//! from it, rather than being handed one. The id is then a control's position in
//! the order the frame was built, so the traversal order cannot disagree with
//! the drawing order — and there is no list of ids beside the tree for a caller
//! to hand out twice or in the wrong sequence.

use otlyra_gfx::peniko::Color;
use otlyra_gfx::{DisplayList, kurbo::RoundedRectRadii};

use crate::widget::icon;
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Button, Child, Cx, Described, Event, Fixed, Focus, FocusId, Gap, Insets,
    Label, Named, Padding, Painted, Paragraph, Rect, Role, Size, Stack, Widget, fill_rounded, ring,
};

/// A mark drawn into a rectangle in one colour: an icon, told what shade to be.
///
/// The colour is a parameter rather than the mark's own business, so a control
/// can dim what it holds when it is disabled without every mark in the interface
/// having to know what disabled means.
pub type Mark = Box<dyn Fn(&mut DisplayList, Rect, Color)>;

/// How much a button insists on itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emphasis {
    /// The one thing this surface is for: filled with the accent.
    Primary,
    /// An ordinary choice: a raised face with a border.
    Normal,
    /// A choice that should not draw the eye until it is reached for.
    Quiet,
    /// Something that destroys: filled with the warning colour.
    Danger,
}

/// A square button holding one mark, drawn bare until the pointer reaches it.
///
/// `mark` is one of the functions in [`icon`], or anything with the same shape.
pub fn icon_button<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    action: A,
    enabled: bool,
    name: &str,
    mark: impl Fn(&mut DisplayList, Rect, Color) + 'static,
) -> Child<A> {
    let id = focus.claim(enabled);
    let ink = if enabled {
        theme.ink
    } else {
        theme.ink_disabled
    };
    let size = theme.control_size;
    let mut face = Background::new(
        Theme::CLEAR,
        theme.radius_small,
        Box::new(Painted::new(size, size, move |rect, _cx, list| {
            mark(list, rect, ink);
        })),
    );
    // A button that will not respond does not light up under the pointer: the
    // wash is a promise that a press will do something.
    if enabled {
        face = face.on_hover(theme.hover).on_press(theme.press);
    }
    Box::new(Fixed::new(
        size,
        size,
        // The name is the button's, not the row's: a mark is not words, and a
        // reader told "button" and nothing else has been told nothing.
        Box::new(Named::new(
            name,
            Box::new(
                Button::new(action, Box::new(face))
                    .enabled(enabled)
                    .focus(id),
            ),
        )),
    ))
}

/// A button with a label in it.
pub fn button<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    action: A,
    text: impl Into<String>,
    emphasis: Emphasis,
    enabled: bool,
) -> Child<A> {
    let id = focus.claim(enabled);
    let (face, ink, outline) = match (emphasis, enabled) {
        (_, false) => (theme.surface, theme.ink_disabled, false),
        (Emphasis::Primary, true) => (theme.accent, theme.ink_on_accent, false),
        (Emphasis::Danger, true) => (theme.danger, theme.ink_on_accent, false),
        (Emphasis::Normal, true) => (theme.raised, theme.ink, true),
        (Emphasis::Quiet, true) => (Theme::CLEAR, theme.ink, false),
    };

    let label: Child<A> = Box::new(Align::centre(Box::new(Label::new(
        text,
        theme.font_size,
        ink,
    ))));
    let padded: Child<A> = Box::new(Padding::new(Insets::symmetric(theme.gap * 2.0, 0.0), label));

    let mut background = Background::new(face, theme.radius, padded);
    if enabled {
        background = background.on_hover(theme.hover).on_press(theme.press);
    }

    let mut stack: Child<A> = Box::new(background);
    if outline {
        stack = Box::new(Outline::new(theme.border, theme.radius, stack));
    }
    Box::new(Fixed::height(
        theme.control_height,
        Box::new(Button::new(action, stack).enabled(enabled).focus(id)),
    ))
}

/// A box that is ticked or not, with a label beside it.
pub fn checkbox<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    action: A,
    checked: bool,
    text: impl Into<String>,
) -> Child<A> {
    let id = focus.claim(true);
    let side = 16.0;
    let (face, border) = if checked {
        (theme.accent, theme.accent)
    } else {
        (theme.raised, theme.border)
    };
    let tick = theme.ink_on_accent;

    let mark: Child<A> = Box::new(Align::centre(Box::new(Painted::new(
        side,
        side,
        move |rect, _cx, list| {
            fill_rounded(list, rect, face, 4.0);
            ring(list, rect, border, 4.0, 1.0);
            if checked {
                icon::check(list, rect, tick);
            }
        },
    ))));

    Box::new(
        Button::new(
            action,
            Box::new(Stack::row(
                theme.gap,
                vec![
                    mark,
                    Box::new(Align::left(Box::new(Label::new(
                        text,
                        theme.font_size,
                        theme.ink,
                    )))),
                ],
            )),
        )
        .role(Role::CheckBox)
        .value(if checked { "ticked" } else { "not ticked" })
        .focus(id),
    )
}

/// One of several exclusive choices, with a label beside it.
///
/// A circle rather than a box, because that difference is the only thing telling
/// a reader that picking this one un-picks another.
///
/// `group` is what the arrow keys travel within, and comes from
/// [`Focus::group`]: a radio that belonged to no group would be reachable only
/// by Tab, which is not how a set of choices behaves anywhere else.
pub fn radio<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    group: u32,
    action: A,
    chosen: bool,
    text: impl Into<String>,
) -> Child<A> {
    let id = focus.claim_in(group, true);
    let side = 16.0;
    let accent = theme.accent;
    let border = theme.border;
    let raised = theme.raised;

    let mark: Child<A> = Box::new(Align::centre(Box::new(Painted::new(
        side,
        side,
        move |rect, _cx, list| {
            fill_rounded(list, rect, raised, side / 2.0);
            ring(
                list,
                rect,
                if chosen { accent } else { border },
                side / 2.0,
                if chosen { 5.0 } else { 1.0 },
            );
        },
    ))));

    Box::new(
        Button::new(
            action,
            Box::new(Stack::row(
                theme.gap,
                vec![
                    mark,
                    Box::new(Align::left(Box::new(Label::new(
                        text,
                        theme.font_size,
                        theme.ink,
                    )))),
                ],
            )),
        )
        .role(Role::RadioButton)
        .value(if chosen { "chosen" } else { "not chosen" })
        .focus(id),
    )
}

/// A switch: a setting that takes effect the moment it is thrown.
///
/// A checkbox is for a choice confirmed later, a switch for one that is not, and
/// drawing them differently is the only way that difference is visible.
pub fn toggle<A: Clone + 'static>(theme: &Theme, focus: &Focus, action: A, on: bool) -> Child<A> {
    toggle_enabled(theme, focus, action, on, true)
}

/// The same, drawn dimmed and unable to respond.
///
/// A switch for something the browser cannot do yet is drawn rather than hidden:
/// what a browser *will* do and what it does are different facts, and a settings
/// page that silently omits the first reads as one that was never going to.
pub fn toggle_enabled<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    action: A,
    on: bool,
    enabled: bool,
) -> Child<A> {
    let id = focus.claim(enabled);
    let (width, height) = (36.0, 20.0);
    let track = match (on, enabled) {
        (_, false) => theme.border,
        (true, true) => theme.accent,
        (false, true) => theme.border,
    };
    let knob = if enabled { theme.raised } else { theme.surface };

    let painted: Child<A> = Box::new(Painted::new(width, height, move |rect, _cx, list| {
        fill_rounded(list, rect, track, rect.height / 2.0);
        let inset = 2.0;
        let side = rect.height - inset * 2.0;
        let x = if on {
            rect.x + rect.width - side - inset
        } else {
            rect.x + inset
        };
        fill_rounded(
            list,
            Rect::new(x, rect.y + inset, side, side),
            knob,
            side / 2.0,
        );
    }));

    Box::new(Fixed::new(
        width,
        height,
        Box::new(
            Button::new(action, Box::new(Align::centre(painted)))
                .role(Role::Switch)
                .value(if on { "on" } else { "off" })
                .enabled(enabled)
                .focus(id),
        ),
    ))
}

/// A row of exclusive choices, joined into one control.
///
/// What a menu would be if it did not have to open. Three or four short options
/// are quicker to read side by side than behind a click, and a popup needs an
/// overlay layer this surface does not have yet.
pub fn segmented<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    options: Vec<(String, A)>,
    chosen: usize,
) -> Child<A> {
    // A segmented control *is* a group, so it makes its own rather than being
    // told which one it is in.
    let group = focus.group();
    let children: Vec<Child<A>> = options
        .into_iter()
        .enumerate()
        .map(|(index, (text, action))| {
            let id = focus.claim_in(group, true);
            let selected = index == chosen;
            let (face, ink) = if selected {
                (theme.raised, theme.ink)
            } else {
                (Theme::CLEAR, theme.ink_dim)
            };
            let label: Child<A> = Box::new(Align::centre(Box::new(Label::new(
                text,
                theme.font_size,
                ink,
            ))));
            let mut face = Background::new(
                face,
                theme.radius_small,
                Box::new(Padding::new(Insets::symmetric(theme.gap * 1.5, 0.0), label)),
            );
            if !selected {
                face = face.on_hover(theme.hover);
            }
            Box::new(
                Button::new(action, Box::new(face))
                    .role(Role::RadioButton)
                    .value(if selected { "chosen" } else { "not chosen" })
                    .focus(id),
            ) as Child<A>
        })
        .collect();

    Box::new(Fixed::height(
        theme.control_height,
        Box::new(Background::new(
            theme.surface,
            theme.radius,
            Box::new(Padding::new(
                Insets::all(2.0),
                Box::new(Stack::row(2.0, children)),
            )),
        )),
    ))
}

/// A bar showing how far along something is, or that it is going at all.
///
/// `progress` is a fraction; `None` is work whose end is not known, drawn as a
/// bar with no fill rather than as a fake one, because a progress bar that
/// invents a position lies about the only thing it exists to say.
pub fn progress<A: 'static>(theme: &Theme, progress: Option<f64>) -> Child<A> {
    let track = theme.surface;
    let fill = theme.accent;
    Box::new(Fixed::height(
        6.0,
        Box::new(Painted::new(0.0, 6.0, move |rect, _cx, list| {
            fill_rounded(list, rect, track, rect.height / 2.0);
            if let Some(fraction) = progress {
                let width = rect.width * fraction.clamp(0.0, 1.0);
                fill_rounded(
                    list,
                    Rect::new(rect.x, rect.y, width, rect.height),
                    fill,
                    rect.height / 2.0,
                );
            }
        })),
    ))
}

/// A value picked along a line.
///
/// The value is the caller's; what the slider reports is where the pointer put
/// it. Dragging works because a press anywhere on the track sets the value, and
/// a move with the button still down keeps setting it — which needs no captured
/// pointer and no drag state of its own.
pub struct Slider<A> {
    value: f64,
    range: (f64, f64),
    step: f64,
    focus: Option<FocusId>,
    on_change: Box<dyn Fn(f64) -> A>,
    rect: Rect,
}

impl<A> Slider<A> {
    /// A slider showing `value` within `range`, reporting where it is dragged.
    pub fn new(value: f64, range: (f64, f64), on_change: impl Fn(f64) -> A + 'static) -> Self {
        Self {
            value,
            range,
            // Twenty steps from end to end, which is a reachable number of key
            // presses. A caller whose value has a grain of its own says so.
            step: (range.1 - range.0) / 20.0,
            focus: None,
            on_change: Box::new(on_change),
            rect: Rect::ZERO,
        }
    }

    /// How far one arrow key moves the value.
    pub fn step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// The name the surface knows this slider by, for keyboard traversal.
    pub fn focus(mut self, id: FocusId) -> Self {
        self.focus = Some(id);
        self
    }

    /// Where the value sits along the track, as a fraction.
    fn fraction(&self) -> f64 {
        let (low, high) = self.range;
        if high <= low {
            return 0.0;
        }
        ((self.value - low) / (high - low)).clamp(0.0, 1.0)
    }

    /// The value the pointer is asking for at `x`.
    fn value_at(&self, x: f64) -> f64 {
        let (low, high) = self.range;
        if self.rect.width <= 0.0 {
            return low;
        }
        let fraction = ((x - self.rect.x) / self.rect.width).clamp(0.0, 1.0);
        low + (high - low) * fraction
    }
}

impl<A> Widget<A> for Slider<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        Size::new(available.width.min(200.0), cx.theme.control_height)
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let theme = cx.theme.clone();
        let centre = self.rect.y + self.rect.height / 2.0;
        let track = Rect::new(self.rect.x, centre - 2.0, self.rect.width, 4.0);
        fill_rounded(list, track, theme.border, 2.0);

        let filled = self.rect.width * self.fraction();
        fill_rounded(
            list,
            Rect::new(track.x, track.y, filled, track.height),
            theme.accent,
            2.0,
        );

        let knob = 16.0;
        fill_rounded(
            list,
            Rect::new(
                self.rect.x + filled - knob / 2.0,
                centre - knob / 2.0,
                knob,
                knob,
            ),
            theme.raised,
            knob / 2.0,
        );
        ring(
            list,
            Rect::new(
                self.rect.x + filled - knob / 2.0,
                centre - knob / 2.0,
                knob,
                knob,
            ),
            theme.border,
            knob / 2.0,
            1.0,
        );

        if self.focus.is_some() && cx.focus == self.focus {
            // Around the whole track rather than around the knob: a ring that
            // travelled with the value would be a second thing moving for one
            // change, and it would leave the control when the value did.
            focus_ring(&theme, list, self.rect, theme.radius);
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        let inside = cx.hovered(self.rect.inflate(4.0));
        match event {
            Event::PointerPressed if inside => Some((self.on_change)(self.value_at(cx.pointer.0))),
            // The keyboard's version of a drag. Clamped here rather than by the
            // caller, because the range is the slider's to know.
            Event::Adjust(by) if self.focus.is_some() && cx.focus == self.focus => {
                let wanted = self.value + f64::from(*by) * self.step;
                Some((self.on_change)(wanted.clamp(self.range.0, self.range.1)))
            }
            // A drag follows the pointer wherever it goes, as long as it began
            // on this slider: dragging past the end and holding there is how a
            // value is pinned to the maximum. No capture and no drag flag — the
            // press's origin says all of it.
            Event::PointerMoved if cx.pointer_down && cx.dragging_from(self.rect.inflate(4.0)) => {
                Some((self.on_change)(self.value_at(cx.pointer.0)))
            }
            _ => None,
        }
    }

    fn describe(&self, out: &mut Vec<Described>) {
        out.push(Described {
            rect: self.rect,
            role: Role::Slider,
            // Named by the row it sits in; a slider holds no words of its own.
            label: String::new(),
            // The number, not the fraction: a reader is told *what the setting
            // is*, and a percentage of a range it was never told the ends of
            // says nothing.
            value: Some(format!("{}", self.value.round())),
            focus: self.focus,
            enabled: true,
        });
    }
}

/// What a field is showing this frame.
///
/// A snapshot, not a handle: the text and the caret belong to whatever owns the
/// field's state, and this is a description of how it should look right now.
#[derive(Clone, Debug, Default)]
pub struct FieldView {
    /// What is in the field.
    pub text: String,
    /// The caret's byte offset, when the field has focus.
    pub caret: Option<usize>,
    /// The selected byte range, when text is selected. Drawn behind the text,
    /// and while it is non-empty the caret is not drawn: the caret is the
    /// selection's live end, and a bar inside a wash says nothing the wash's
    /// edge does not.
    pub selection: Option<std::ops::Range<usize>>,
    /// What to show, dimmed, while the field is empty.
    pub placeholder: String,
}

/// Where in a field's text the pointer landed.
///
/// Byte offsets into the field's full text, snapped to character boundaries by
/// construction: the map they come from has an entry per boundary of what was
/// drawn and nothing else.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FieldHit {
    /// The button went down at this offset. `clicks` counts the run the press
    /// ends: `2` for a double-click, `3` for a triple.
    Press {
        /// The byte offset the press landed nearest.
        offset: usize,
        /// How many presses this is the latest of.
        clicks: u32,
    },
    /// The pointer, still down from a press that began in this field, is over
    /// this offset now. What a drag-selection is made of.
    Drag {
        /// The byte offset under the pointer.
        offset: usize,
    },
}

/// A single-line text field.
///
/// It edits nothing. Keys are handled by whoever owns the text, because a field
/// that consumed keystrokes would need to know about focus, and focus is a
/// property of the surface rather than of any control on it. What it does
/// answer for itself is *where* in the text the pointer is: it reports a
/// [`FieldHit`] through `on_hit`, and the owner decides what a click, a
/// double-click or a drag at that offset means to its own caret and anchor.
pub struct TextInput<A> {
    view: FieldView,
    on_hit: Box<dyn Fn(FieldHit) -> A>,
    leading: Option<Mark>,
    face: Option<Color>,
    rect: Rect,
    /// One `(x, byte offset)` entry per character boundary of the text drawn,
    /// computed while drawing with the engine that drew it. Hit testing reads
    /// this rather than measuring again — measured a second time, by a second
    /// engine, a press would land a glyph away from where it was aimed.
    hit_map: Vec<(f64, usize)>,
}

impl<A> TextInput<A> {
    /// A field showing `view`, reporting where presses and drags land.
    pub fn new(view: FieldView, on_hit: impl Fn(FieldHit) -> A + 'static) -> Self {
        Self {
            view,
            on_hit: Box::new(on_hit),
            leading: None,
            face: None,
            rect: Rect::ZERO,
            hit_map: Vec::new(),
        }
    }

    /// A mark inside the field's left end, before the text.
    pub fn leading(mut self, mark: impl Fn(&mut DisplayList, Rect, Color) + 'static) -> Self {
        self.leading = Some(Box::new(mark));
        self
    }

    /// What the field's face is while it is not focused.
    ///
    /// A field has to be told, because it cannot see what it is sitting on: the
    /// default raised white is right on a grey settings surface and invisible on
    /// the white toolbar, which wants the recessed grey instead.
    pub fn face(mut self, color: Color) -> Self {
        self.face = Some(color);
        self
    }

    /// Wrap in the face, border and focus ring a field is drawn with.
    pub fn into_widget(self, theme: &Theme) -> Child<A>
    where
        A: 'static,
    {
        let focused = self.view.caret.is_some();
        // Focus raises the field to white whatever it rests on, so the text
        // being edited always has the most contrast on screen behind it.
        let face = if focused {
            theme.raised
        } else {
            self.face.unwrap_or(theme.raised)
        };
        Box::new(Fixed::height(
            theme.control_height,
            Box::new(Field {
                inner: Box::new(self),
                face,
                border: if focused { theme.accent } else { theme.border },
                halo: focused.then_some(theme.accent_halo),
                radius: theme.radius,
                rect: Rect::ZERO,
            }),
        ))
    }
}

impl<A> Widget<A> for TextInput<A> {
    fn describe(&self, out: &mut Vec<Described>) {
        out.push(Described {
            rect: self.rect,
            role: Role::TextInput,
            // The placeholder is the field's name: it is the words that say what
            // belongs here, and they are the same words whether the field is
            // empty or not.
            label: self.view.placeholder.clone(),
            value: Some(self.view.text.clone()),
            // A field has no focus id — the caret being present *is* the field
            // having the keyboard, and that is already in the view it was given.
            focus: None,
            enabled: true,
        });
    }

    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        // No intrinsic width at all: a field wants whatever is left over, and it
        // asks for that through `flex` rather than by claiming the whole row and
        // pushing its siblings off the end of it.
        let _ = available;
        Size::new(0.0, cx.line_height(cx.theme.font_size))
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let theme = cx.theme.clone();
        let inset = theme.gap * 1.5;
        let mut text_x = self.rect.x + inset;

        if let Some(mark) = &self.leading {
            let side = 14.0;
            let rect = Rect::new(
                text_x,
                self.rect.y + (self.rect.height - side) / 2.0,
                side,
                side,
            );
            mark(list, rect, theme.ink_dim);
            text_x = rect.x + side + theme.gap;
        }

        let available = (self.rect.x + self.rect.width - inset - text_x).max(0.0);
        let placeholder = self.view.text.is_empty();
        let content = if placeholder {
            self.view.placeholder.clone()
        } else {
            self.view.text.clone()
        };

        // A focused field keeps its end in view, because that is where typing
        // happens; an unfocused one keeps its front, because that is where the
        // scheme and host are.
        let shown = elide(
            cx,
            &content,
            available,
            if self.view.caret.is_some() && !placeholder {
                Elide::Start
            } else {
                Elide::End
            },
        );

        let line = cx.line_height(theme.font_size);
        let top = self.rect.y + (self.rect.height - line) / 2.0;

        // The map from x to byte offset, built from what is actually drawn and
        // with the engine drawing it. Everything that answers *where in the
        // text* — the selection wash, the caret, a press — reads this one map,
        // so none of them can disagree about where a character is.
        self.hit_map.clear();
        if placeholder {
            self.hit_map.push((text_x, 0));
        } else {
            // What survived elision is a literal slice of the text: the tail
            // when the front was cut, the front when the tail was.
            let visible = shown
                .trim_start_matches(ELLIPSIS)
                .trim_end_matches(ELLIPSIS);
            let base = if shown.starts_with(ELLIPSIS) {
                self.view.text.len() - visible.len()
            } else {
                0
            };
            let start_x = if shown.starts_with(ELLIPSIS) {
                text_x + cx.measure_text(&ELLIPSIS.to_string(), theme.font_size)
            } else {
                text_x
            };
            self.hit_map.push((start_x, base));
            for (index, character) in visible.char_indices() {
                let end = index + character.len_utf8();
                let advance = cx.measure_text(&visible[..end], theme.font_size);
                self.hit_map.push((start_x + advance, base + end));
            }
        }

        let selection = self
            .view
            .selection
            .clone()
            .filter(|range| !range.is_empty() && !placeholder);

        if let Some(range) = &selection {
            // Clamped to what is on screen: a selection running past the elided
            // end is drawn to the edge of what is visible, which is where the
            // selected text stops being visible too.
            let from = self.x_of(range.start);
            let to = self.x_of(range.end);
            fill_rounded(
                list,
                Rect::new(from, top, (to - from).max(0.0), line),
                theme.selection,
                0.0,
            );
        }

        let ink = if placeholder {
            theme.ink_dim
        } else {
            theme.ink
        };
        let mut label = Label::new(shown.clone(), theme.font_size, ink);
        Widget::<A>::place(&mut label, Rect::new(text_x, top, available, line), cx);
        Widget::<A>::draw(&mut label, cx, list);

        if let Some(caret) = self
            .view
            .caret
            .filter(|_| !placeholder && selection.is_none())
        {
            let caret = caret.min(self.view.text.len());
            let advance = self.x_of(caret);
            fill_rounded(
                list,
                Rect::new(advance, top + 1.0, 1.5, line - 2.0),
                theme.ink,
                0.0,
            );
        }
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        match event {
            Event::PointerPressed if cx.hovered(self.rect) => {
                Some((self.on_hit)(FieldHit::Press {
                    offset: self.offset_at(cx.pointer.0),
                    clicks: cx.clicks,
                }))
            }
            // A drag belongs to the field the press began in, wherever the
            // pointer has wandered since — which is what lets a selection keep
            // growing past the field's edge.
            Event::PointerMoved if cx.pointer_down && cx.dragging_from(self.rect) => {
                Some((self.on_hit)(FieldHit::Drag {
                    offset: self.offset_at(cx.pointer.0),
                }))
            }
            _ => None,
        }
    }

    fn flex(&self) -> f64 {
        1.0
    }
}

impl<A> TextInput<A> {
    /// The x where `offset` sits, read from the map the last draw built.
    ///
    /// An offset outside what was drawn answers with the nearest visible edge:
    /// the caret of a selection scrolled out of view is at the view's edge, as
    /// far as drawing is concerned.
    fn x_of(&self, offset: usize) -> f64 {
        let mut nearest = self.rect.x;
        let mut distance = usize::MAX;
        for &(x, at) in &self.hit_map {
            let gap = at.abs_diff(offset);
            if gap < distance {
                distance = gap;
                nearest = x;
            }
        }
        nearest
    }

    /// The character boundary nearest to `x`, as a byte offset into the text.
    fn offset_at(&self, x: f64) -> usize {
        let mut nearest = 0;
        let mut distance = f64::INFINITY;
        for &(at, offset) in &self.hit_map {
            let gap = (at - x).abs();
            if gap < distance {
                distance = gap;
                nearest = offset;
            }
        }
        nearest
    }
}

/// The face, border and focus ring around a field.
struct Field<A> {
    inner: Child<A>,
    face: Color,
    border: Color,
    halo: Option<Color>,
    radius: f64,
    rect: Rect,
}

impl<A> Widget<A> for Field<A> {
    fn describe(&self, out: &mut Vec<Described>) {
        self.inner.describe(out);
    }

    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.inner.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.inner.place(rect, cx);
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        if let Some(halo) = self.halo {
            // Outside the field rather than inside it, so focus does not move
            // the text by the ring's width.
            let spread = 3.0;
            fill_rounded(list, self.rect.inflate(spread), halo, self.radius + spread);
        }
        fill_rounded(list, self.rect, self.face, self.radius);
        ring(list, self.rect, self.border, self.radius, 1.0);
        self.inner.draw(cx, list);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.inner.event(event, cx)
    }

    fn flex(&self) -> f64 {
        self.inner.flex()
    }
}

/// A border drawn around a child, inside its own rectangle.
pub struct Outline<A> {
    child: Child<A>,
    color: Color,
    radius: f64,
    rect: Rect,
}

impl<A> Outline<A> {
    /// Draw `color` around `child`.
    pub fn new(color: Color, radius: f64, child: Child<A>) -> Self {
        Self {
            child,
            color,
            radius,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Outline<A> {
    fn describe(&self, out: &mut Vec<Described>) {
        self.child.describe(out);
    }

    fn label_text(&self) -> Option<String> {
        self.child.label_text()
    }

    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        self.child.measure(available, cx)
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        self.rect = rect;
        self.child.place(rect, cx);
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        self.child.draw(cx, list);
        ring(list, self.rect, self.color, self.radius, 1.0);
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.child.event(event, cx)
    }

    fn flex(&self) -> f64 {
        self.child.flex()
    }
}

/// A raised panel with a title, holding a group of rows.
///
/// Settings are read in groups, and a group with an edge is easier to scan than
/// one held together only by the space around it.
pub fn card<A: 'static>(theme: &Theme, title: impl Into<String>, rows: Vec<Child<A>>) -> Child<A> {
    let heading: Child<A> = Box::new(Label::new(title, theme.font_size, theme.ink_dim));
    let mut children: Vec<Child<A>> = vec![heading, Box::new(Gap::new(0.0, theme.gap))];
    children.extend(rows);
    card_plain(theme, children)
}

/// A card with no heading: the raised face and the outline alone, for a group
/// whose title lives outside it — a day's visits under the day's own label.
pub fn card_plain<A: 'static>(theme: &Theme, rows: Vec<Child<A>>) -> Child<A> {
    Box::new(Background::new(
        theme.raised,
        theme.radius,
        Box::new(Outline::new(
            theme.border,
            theme.radius,
            Box::new(Padding::new(
                Insets::all(theme.inset * 1.5),
                Box::new(Stack::column(theme.gap * 1.5, rows)),
            )),
        )),
    ))
}

/// One setting: what it is called, what it does, and the control that sets it.
///
/// The hint is under the title rather than beside the control, because a line of
/// explanation next to a switch is read as part of the switch's label and
/// doubles the width of every row in the card.
pub fn setting_row<A: 'static>(
    theme: &Theme,
    title: impl Into<String>,
    hint: Option<&str>,
    control: Child<A>,
) -> Child<A> {
    let title = title.into();
    // The control is named by the row, so a switch or a slider — neither of
    // which holds any words — is called what the row calls it, with no second
    // copy of the string to fall out of step.
    let control: Child<A> = Box::new(Named::new(title.clone(), control));
    let mut text: Vec<Child<A>> = vec![Box::new(Label::new(title, theme.font_size, theme.ink))];
    if let Some(hint) = hint {
        text.push(Box::new(Paragraph::new(
            hint,
            theme.font_size_small,
            theme.ink_dim,
        )));
    }

    Box::new(Stack::row(
        theme.gap * 2.0,
        vec![
            Box::new(crate::widget::Flex::new(
                1.0,
                Box::new(Stack::column(2.0, text)),
            )),
            Box::new(Align::right(control)),
        ],
    ))
}

/// One row of a menu panel: a mark, a name, and sometimes a key that does the
/// same thing.
///
/// A row that will not respond is drawn dimmed rather than hidden. What a
/// browser can do and what it can do *yet* are different facts, and a menu that
/// silently omits the second reads as a browser that was never going to have it.
pub fn menu_item<A: Clone + 'static>(
    theme: &Theme,
    focus: &Focus,
    action: A,
    enabled: bool,
    mark: impl Fn(&mut DisplayList, Rect, Color) + 'static,
    text: impl Into<String>,
    shortcut: Option<&str>,
) -> Child<A> {
    let id = focus.claim(enabled);
    let ink = if enabled {
        theme.ink
    } else {
        theme.ink_disabled
    };
    let side = 16.0;

    let mut row: Vec<Child<A>> = vec![
        Box::new(Align::centre(Box::new(Painted::new(
            side,
            side,
            move |rect, _cx, list| mark(list, rect, ink),
        )))),
        Box::new(crate::widget::Flex::new(
            1.0,
            Box::new(Align::left(Box::new(Label::new(
                text,
                theme.font_size,
                ink,
            )))),
        )),
    ];
    if let Some(shortcut) = shortcut {
        row.push(Box::new(Align::right(Box::new(Label::new(
            shortcut,
            theme.font_size_small,
            theme.ink_disabled,
        )))));
    }

    let mut face = Background::new(
        Theme::CLEAR,
        theme.radius_small,
        Box::new(Padding::new(
            Insets::symmetric(theme.gap * 1.5, 0.0),
            Box::new(Stack::row(theme.gap * 1.5, row)),
        )),
    );
    if enabled {
        face = face.on_hover(theme.hover).on_press(theme.press);
    }

    Box::new(Fixed::height(
        30.0,
        Box::new(
            Button::new(action, Box::new(face))
                .role(Role::MenuItem)
                .enabled(enabled)
                .focus(id),
        ),
    ))
}

/// The panel a menu's rows sit on: a raised card with a border, over whatever
/// was already on screen.
///
/// There is no shadow, because the display list has no blur yet. A border does
/// the same work — saying *this is in front* — and does not fake a thing the
/// rasterizer cannot draw.
pub fn menu_panel<A: 'static>(theme: &Theme, width: f64, rows: Vec<Child<A>>) -> Child<A> {
    Box::new(Fixed::width(
        width,
        Box::new(Background::new(
            theme.raised,
            theme.radius,
            Box::new(Outline::new(
                theme.border,
                theme.radius,
                Box::new(Padding::new(
                    Insets::all(theme.gap * 0.75),
                    Box::new(Stack::column(1.0, rows)),
                )),
            )),
        )),
    ))
}

/// A caption above a group of menu rows.
pub fn menu_heading<A: 'static>(theme: &Theme, text: impl Into<String>) -> Child<A> {
    Box::new(Padding::new(
        Insets {
            left: theme.gap * 1.5,
            top: theme.gap,
            right: theme.gap * 1.5,
            bottom: 2.0,
        },
        Box::new(Label::new(text, theme.font_size_small, theme.ink_dim)),
    ))
}

/// Something that swallows every press that reaches it, and reports one action.
///
/// The sheet behind an open menu. Without it a press meant to dismiss the menu
/// lands on whatever was underneath and does two things at once — closes the
/// menu and follows a link.
pub fn scrim<A: Clone + 'static>(action: A) -> Child<A> {
    Box::new(Button::new(
        action,
        Box::new(Painted::new(0.0, 0.0, |_rect, _cx, _list| {})),
    ))
}

/// A one-pixel line across, used to separate the interface from the page and one
/// group of rows from the next.
pub fn hairline(theme: &Theme, list: &mut DisplayList, rect: Rect) {
    fill_rounded(list, rect, theme.hairline, 0.0);
}

/// A hairline as a child, for a column that wants a rule between two rows.
pub fn divider<A: 'static>(theme: &Theme) -> Child<A> {
    let color = theme.hairline;
    Box::new(Fixed::height(
        1.0,
        Box::new(Painted::new(0.0, 1.0, move |rect, _cx, list| {
            fill_rounded(list, rect, color, 0.0);
        })),
    ))
}

/// The ring that says a control has the keyboard.
///
/// Outside the control, in the accent, and always the same shape wherever it is
/// drawn: focus that looked different on each control would have to be learned
/// separately on each.
pub fn focus_ring(theme: &Theme, list: &mut DisplayList, rect: Rect, radius: f64) {
    ring(list, rect.inflate(2.0), theme.accent, radius + 2.0, 2.0);
}

/// A border, drawn as the gap between two rounded rectangles.
pub fn outline(list: &mut DisplayList, rect: Rect, color: Color, radius: f64, width: f64) {
    ring(list, rect, color, radius, width);
}

/// Spacing, as a child, for callers assembling rows by hand.
pub fn gap<A: 'static>(size: f64) -> Child<A> {
    Box::new(Gap::new(size, size))
}

/// Corner radii for a control whose two bottom corners must stay square.
pub fn top_rounded(radius: f64) -> RoundedRectRadii {
    RoundedRectRadii::new(radius, radius, 0.0, 0.0)
}

/// A one-line label that cuts itself with an ellipsis when it does not fit.
///
/// [`Label`] draws what it was given; this draws what *fits*, decided at draw
/// time against the width it was actually placed at — the only moment that
/// width is known.
pub struct Elided {
    text: String,
    size: f32,
    ink: Color,
    end: Elide,
    rect: Rect,
}

impl Elided {
    /// A label showing `text`, cut at `end` when it must be.
    pub fn new(text: impl Into<String>, size: f32, ink: Color, end: Elide) -> Self {
        Self {
            text: text.into(),
            size,
            ink,
            end,
            rect: Rect::ZERO,
        }
    }
}

impl<A> Widget<A> for Elided {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let width = cx.measure_text(&self.text, self.size);
        Size::new(width.min(available.width), cx.line_height(self.size))
    }

    fn place(&mut self, rect: Rect, _cx: &mut Cx) {
        self.rect = rect;
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let shown = elide(cx, &self.text, self.rect.width, self.end);
        let mut label = Label::new(shown, self.size, self.ink);
        Widget::<A>::place(&mut label, self.rect, cx);
        Widget::<A>::draw(&mut label, cx, list);
    }
}

/// The character standing in for what was cut.
pub const ELLIPSIS: char = '…';

/// Which end of a string to cut when it does not fit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Elide {
    /// Cut the front: keeps the end, which is where a caret is.
    Start,
    /// Cut the tail: keeps the front, which is where a title and a host are.
    End,
}

/// `content` cut to fit `available`, with an ellipsis standing in for the rest.
///
/// Binary search over character boundaries, measuring with the engine that will
/// draw the result. Counting characters would be wrong the moment the text is
/// not monospaced, which is always.
pub fn elide(cx: &mut Cx, content: &str, available: f64, end: Elide) -> String {
    let size = cx.theme.font_size;
    if available <= 0.0 {
        return String::new();
    }
    if cx.measure_text(content, size) <= available {
        return content.to_owned();
    }

    let boundaries: Vec<usize> = content
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(content.len()))
        .collect();

    let mut low = 0;
    let mut high = boundaries.len() - 1;
    let mut best = String::from(ELLIPSIS);
    while low <= high {
        let middle = (low + high) / 2;
        let candidate = match end {
            Elide::End => format!("{}{ELLIPSIS}", &content[..boundaries[middle]]),
            Elide::Start => format!(
                "{ELLIPSIS}{}",
                &content[boundaries[boundaries.len() - 1 - middle]..]
            ),
        };
        if cx.measure_text(&candidate, size) <= available {
            best = candidate;
            low = middle + 1;
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use otlyra_text::TextEngine;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Act {
        Set(i64),
    }

    #[test]
    fn a_slider_reports_the_value_the_pointer_asks_for() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let mut slider = Slider::new(0.0, (0.0, 100.0), |value| Act::Set(value.round() as i64));
        Widget::<Act>::place(&mut slider, Rect::new(0.0, 0.0, 100.0, 20.0), &mut cx);

        cx.pointer = (25.0, 10.0);
        assert_eq!(
            slider.event(&Event::PointerPressed, &mut cx),
            Some(Act::Set(25))
        );

        // A press at the far end of the track is the maximum; a press off the
        // track entirely is not this slider's business at all.
        cx.pointer = (99.0, 10.0);
        assert_eq!(
            slider.event(&Event::PointerPressed, &mut cx),
            Some(Act::Set(99))
        );
        cx.pointer = (400.0, 10.0);
        assert_eq!(slider.event(&Event::PointerPressed, &mut cx), None);
    }

    #[test]
    fn a_slider_follows_the_pointer_only_while_the_button_is_down() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let mut slider = Slider::new(0.0, (0.0, 100.0), |value| Act::Set(value.round() as i64));
        Widget::<Act>::place(&mut slider, Rect::new(0.0, 0.0, 100.0, 20.0), &mut cx);
        cx.pointer = (50.0, 10.0);

        assert_eq!(slider.event(&Event::PointerMoved, &mut cx), None);
        cx.pointer_down = true;
        cx.press_origin = Some((10.0, 10.0));
        assert_eq!(
            slider.event(&Event::PointerMoved, &mut cx),
            Some(Act::Set(50))
        );

        // Dragged off the end, still held: pinned to the maximum rather than
        // let go of.
        cx.pointer = (400.0, 400.0);
        assert_eq!(
            slider.event(&Event::PointerMoved, &mut cx),
            Some(Act::Set(100))
        );
    }

    #[test]
    fn eliding_keeps_what_fits_and_says_that_it_cut() {
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);
        let long = "a title far too long for the space it has been given";

        let cut = elide(&mut cx, long, 60.0, Elide::End);
        assert!(cut.ends_with(ELLIPSIS), "{cut:?} should say it was cut");
        assert!(cx.measure_text(&cut, cx.theme.font_size) <= 60.0);

        let front = elide(&mut cx, long, 60.0, Elide::Start);
        assert!(front.starts_with(ELLIPSIS));

        // What fits is returned whole, with nothing added.
        assert_eq!(elide(&mut cx, "short", 500.0, Elide::End), "short");
    }
}
