//! The browser's settings, as a surface the browser draws itself.
//!
//! Not a page. A settings page written in HTML would be laid out by the same
//! engine that lays out untrusted documents, would look like whatever its
//! stylesheet said rather than like the browser around it, and would need a
//! privileged path from a document back into the browser's own state to change
//! anything. Drawn with [`crate::widget`] instead, it is a second surface beside
//! the toolbar: the same controls, the same theme, and preferences that are
//! plain fields on a struct.
//!
//! The shape is the same one the toolbar uses. [`Settings`] is the state;
//! [`Action`] is what the surface reports; [`Settings::apply`] is the only thing
//! that changes state, so every possible change is one match arm long. The tree
//! is rebuilt from the state each frame and kept only so the next press lands on
//! what was drawn.

use std::rc::Rc;

use otlyra_gfx::DisplayList;
use otlyra_platform::{Key, Modifiers};
use otlyra_text::TextEngine;

use crate::widget::controls::{self, Emphasis};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Child, Cx, Event, Flex, Focus, FocusId, FocusKind, Gap, Insets, Label, Overflow,
    Padding, Rect, Scroll, Size, Stack, fill_rounded,
};

/// What happens when the browser starts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OnStart {
    /// A blank tab.
    Blank,
    /// The home address.
    Home,
    /// Whatever was open when it last closed.
    Restore,
}

impl OnStart {
    /// The three of them, in the order they are offered.
    pub const ALL: [Self; 3] = [Self::Blank, Self::Home, Self::Restore];

    /// What this is called on the surface.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blank => "New tab",
            Self::Home => "Home page",
            Self::Restore => "Last session",
        }
    }
}

/// Everything the browser lets someone change about it.
///
/// Plain fields, no indirection: a preference that needed a setter would need a
/// reason, and none of these has one yet. Persisting them is a later milestone —
/// what matters now is that the surface has real state to be a view of rather
/// than a mock-up that cannot be wrong.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Where a new window starts.
    pub on_start: OnStart,
    /// The address the home button goes to.
    pub home: crate::ui::TextField,
    /// Load the pictures a page asks for.
    pub load_images: bool,
    /// Run the scripts a page carries.
    pub run_scripts: bool,
    /// Ask sites not to follow the reader between them.
    pub do_not_track: bool,
    /// Restore the tabs that were open, when `on_start` says to.
    pub restore_tabs: bool,
    /// The size text is drawn at, as a percentage of the default.
    pub text_scale: f64,
    /// Which control has the keyboard.
    ///
    /// The whole of what focus is on this surface: whether the home field shows
    /// a caret is *this* value landing on the field's id, not a second flag
    /// beside it that something has to remember to keep in step.
    pub focus: Option<FocusId>,
    /// How far the surface is scrolled.
    pub scroll: f64,
    overflow: Overflow,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on_start: OnStart::Blank,
            home: crate::ui::TextField::new("https://example.com/"),
            load_images: true,
            run_scripts: true,
            do_not_track: false,
            restore_tabs: true,
            text_scale: 100.0,
            focus: None,
            scroll: 0.0,
            overflow: Overflow::default(),
        }
    }
}

/// What the settings surface reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Nothing.
    None,
    /// Start with this.
    SetOnStart(OnStart),
    /// Give this control the keyboard.
    ///
    /// The id is claimed while the frame is built, so it names the control that
    /// was actually drawn there rather than a number chosen in advance.
    Focus(FocusId),
    /// Load pictures, or stop.
    ToggleImages,
    /// Run scripts, or stop.
    ToggleScripts,
    /// Ask not to be followed, or stop asking.
    ToggleDoNotTrack,
    /// Restore tabs, or stop.
    ToggleRestoreTabs,
    /// Draw text at this percentage of the default.
    SetTextScale(f64),
    /// Put everything back the way it came.
    Reset,
    /// Leave the surface.
    Close,
}

/// The height of the surface's own header, above the scrolling content.
const HEADER_HEIGHT: f64 = 52.0;
/// The widest the content is allowed to be, however wide the window is.
///
/// A settings row stretched across a 2000px window is a title at one end and a
/// switch at the other with nothing joining them.
const CONTENT_WIDTH: f64 = 680.0;

impl Settings {
    /// Apply what the surface reported. The only thing that changes state.
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::None | Action::Close => {}
            Action::SetOnStart(choice) => self.on_start = choice,
            Action::Focus(id) => self.focus = Some(id),
            Action::ToggleImages => self.load_images = !self.load_images,
            Action::ToggleScripts => self.run_scripts = !self.run_scripts,
            Action::ToggleDoNotTrack => self.do_not_track = !self.do_not_track,
            Action::ToggleRestoreTabs => self.restore_tabs = !self.restore_tabs,
            // Rounded to fives: a text size of 103% is a number nobody asked for
            // and cannot aim at a second time.
            Action::SetTextScale(scale) => {
                self.text_scale = (scale / 5.0).round() * 5.0;
            }
            Action::Reset => {
                let scroll = self.scroll;
                let focus = self.focus;
                *self = Self::default();
                self.scroll = scroll;
                // Resetting the preferences is not a reason to take the keyboard
                // away from whoever pressed the button that did it.
                self.focus = focus;
            }
        }
    }

    /// Whether two sets of preferences agree about everything that is saved.
    ///
    /// Which is everything except where the caret is, how far the page is
    /// scrolled and what holds the keyboard: those are how the surface is being
    /// looked at rather than what the reader has chosen, and writing them to
    /// disk would make a file that changed every time somebody scrolled.
    pub fn persisted_eq(&self, other: &Self) -> bool {
        self.on_start == other.on_start
            && self.home.text() == other.home.text()
            && self.load_images == other.load_images
            && self.run_scripts == other.run_scripts
            && self.do_not_track == other.do_not_track
            && self.restore_tabs == other.restore_tabs
            && (self.text_scale - other.text_scale).abs() < f64::EPSILON
    }

    /// Edit the home field with `key`, if it is a key that edits one.
    ///
    /// Only reached while the field holds the focus, which is why the arrows
    /// move the caret here and move between controls everywhere else.
    fn edit_home(&mut self, key: Key) -> bool {
        match key {
            Key::Backspace => self.home.backspace(),
            Key::Delete => self.home.delete(),
            Key::Left => self.home.move_left(),
            Key::Right => self.home.move_right(),
            Key::Home => self.home.move_home(),
            Key::End => self.home.move_end(),
            _ => return false,
        }
        true
    }

    /// Scroll by `delta` logical pixels, stopping at the ends.
    ///
    /// Clamped against what the last frame reported it could travel, which is
    /// the only place that number exists: how far a surface can scroll depends
    /// on how tall its content turned out.
    pub fn scroll_by(&mut self, delta: f64) {
        self.scroll = (self.scroll + delta).clamp(0.0, self.overflow.get());
    }

    /// Build this frame's tree, claiming a focus id per control as it goes.
    ///
    /// Built in the order it is drawn, top down, so the order Tab travels in is
    /// the order the eye travels in without either being written down twice.
    /// The header first, then the cards, then the way out.
    pub fn build(&self, theme: &Theme, width: f64, focus: &Focus) -> Child<Action> {
        let header = self.header(theme, focus);
        let rows = vec![
            self.startup_card(theme, focus),
            self.content_card(theme, focus),
            self.privacy_card(theme, focus),
            self.reset_row(theme, focus),
            Box::new(Gap::new(0.0, theme.inset * 2.0)) as Child<Action>,
        ];

        let column: Child<Action> = Box::new(Padding::new(
            Insets::symmetric(theme.inset * 2.0, theme.inset * 2.0),
            Box::new(Stack::column(theme.inset * 1.5, rows)),
        ));

        // Centred, and no wider than a row can be read across.
        let centred: Child<Action> = Box::new(Stack::row(
            0.0,
            vec![
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                Box::new(crate::widget::Fixed::width(
                    CONTENT_WIDTH.min(width),
                    column,
                )),
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
            ],
        ));

        Box::new(Stack::column(
            0.0,
            vec![
                header,
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    /// The bar across the top: what this surface is, and the way out of it.
    fn header(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "Settings",
            theme.font_size + 3.0,
            theme.ink,
        ))));
        Box::new(crate::widget::Fixed::height(
            HEADER_HEIGHT,
            Box::new(crate::widget::Background::new(
                theme.surface,
                0.0,
                Box::new(Padding::new(
                    Insets::symmetric(theme.inset * 2.0, theme.gap),
                    Box::new(Stack::row(
                        theme.gap,
                        vec![
                            Box::new(Flex::new(1.0, title)),
                            Box::new(Align::centre(controls::button(
                                theme,
                                focus,
                                Action::Close,
                                "Done",
                                Emphasis::Primary,
                                true,
                            ))),
                        ],
                    )),
                )),
            )),
        ))
    }

    fn startup_card(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        let choices_group = focus.group();
        let choices: Vec<Child<Action>> = OnStart::ALL
            .iter()
            .map(|choice| {
                controls::radio(
                    theme,
                    focus,
                    choices_group,
                    Action::SetOnStart(*choice),
                    self.on_start == *choice,
                    choice.label(),
                )
            })
            .collect();

        // The field's id is claimed here, where it is built, and answered here
        // too: whether it shows a caret is whether the surface's focus is on it.
        // Nothing has to be told about it afterwards.
        let home_id = focus.claim_text(true);
        let home_field = controls::TextInput::new(controls::FieldView {
            text: self.home.text().to_owned(),
            caret: (self.focus == Some(home_id)).then(|| self.home.caret()),
            placeholder: "https://".to_owned(),
        })
        .into_widget::<Action>(theme);

        controls::card(
            theme,
            "On start",
            vec![
                Box::new(Stack::column(theme.gap, choices)),
                controls::divider(theme),
                controls::setting_row(
                    theme,
                    "Home page",
                    Some("Where the home button and a new window go."),
                    Box::new(crate::widget::Fixed::width(
                        280.0,
                        Box::new(
                            crate::widget::Button::new(Action::Focus(home_id), home_field)
                                .focus(home_id),
                        ),
                    )),
                ),
                controls::setting_row(
                    theme,
                    "Reopen tabs",
                    Some("Bring back what was open when the browser last closed."),
                    controls::toggle(theme, focus, Action::ToggleRestoreTabs, self.restore_tabs),
                ),
            ],
        )
    }

    fn content_card(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        let images = controls::toggle(theme, focus, Action::ToggleImages, self.load_images);
        let scripts =
            controls::toggle_enabled(theme, focus, Action::ToggleScripts, self.run_scripts, false);
        // Five at a time, so the keyboard lands on the same values the pointer
        // does — `apply` rounds to fives, and a step that did not would leave
        // the arrow keys moving the slider without moving the number.
        let scale = Box::new(
            controls::Slider::new(self.text_scale, (50.0, 200.0), Action::SetTextScale)
                .step(5.0)
                .focus(focus.claim(true)),
        );

        controls::card(
            theme,
            "Content",
            vec![
                controls::setting_row(
                    theme,
                    "Load images",
                    Some("Fetch the pictures a page asks for."),
                    images,
                ),
                controls::setting_row(
                    theme,
                    "Run scripts",
                    // A switch that changed nothing would be a switch that lied.
                    // What a browser cannot do *yet* is worth saying, and saying
                    // it here is cheaper than a reader working it out from a
                    // page that never runs.
                    Some("There is no script engine yet, so this changes nothing."),
                    scripts,
                ),
                controls::divider(theme),
                controls::setting_row(
                    theme,
                    format!("Text size — {}%", self.text_scale as i64),
                    // What it actually does, including the part that surprises
                    // people: it is a *default*, so a page that names its own
                    // size still wins, exactly as it would over any other.
                    Some("The default size on pages that do not name one."),
                    scale,
                ),
            ],
        )
    }

    fn privacy_card(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        controls::card(
            theme,
            "Privacy",
            vec![
                controls::setting_row(
                    theme,
                    "Do Not Track",
                    Some("Sends a request sites are free to ignore, and most do."),
                    controls::toggle(theme, focus, Action::ToggleDoNotTrack, self.do_not_track),
                ),
                controls::divider(theme),
                controls::setting_row(
                    theme,
                    "Start with",
                    None,
                    controls::segmented(
                        theme,
                        focus,
                        OnStart::ALL
                            .iter()
                            .map(|choice| (choice.label().to_owned(), Action::SetOnStart(*choice)))
                            .collect(),
                        OnStart::ALL
                            .iter()
                            .position(|choice| *choice == self.on_start)
                            .unwrap_or(0),
                    ),
                ),
            ],
        )
    }

    fn reset_row(&self, theme: &Theme, focus: &Focus) -> Child<Action> {
        let reset = controls::button(
            theme,
            focus,
            Action::Reset,
            "Reset all",
            Emphasis::Danger,
            true,
        );
        Box::new(Stack::row(
            theme.gap,
            vec![
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                reset,
            ],
        ))
    }
}

/// Everything the settings' appearance is a function of.
#[derive(Clone, PartialEq)]
struct Appearance {
    rect: Rect,
    settings: Settings,
    pointer: (f64, f64),
    pointer_down: bool,
}

/// A settings surface that has been drawn, and can therefore be pressed.
///
/// Holds the tree the last frame built, for the same reason the toolbar does:
/// a press must be tested against the rectangles that were drawn, not against
/// rectangles worked out a second time.
pub struct SettingsSurface {
    /// The preferences themselves.
    pub settings: Settings,
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    pointer: (f64, f64),
    pointer_down: bool,
    press_origin: Option<(f64, f64)>,
    engine: TextEngine,
    /// The focusable controls the last frame built, in the order it built them.
    ///
    /// Kept beside the tree rather than inside the preferences: it is a fact
    /// about the frame, not about what the reader has chosen, and putting it in
    /// a value the cache compares would make every frame differ from the last.
    focus: Focus,
    /// What the last built list was built from, and the list itself.
    ///
    /// The same rule the toolbar follows: a frame that agrees with the last one
    /// on everything it draws from would draw the same list, so it does not
    /// build one. This page is a scrolling column of cards, and rebuilding it
    /// every frame shapes every label on it again.
    cache: Option<(Appearance, DisplayList)>,
    builds: u64,
    root: Option<Child<Action>>,
}

impl Default for SettingsSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsSurface {
    /// A surface over `settings`, which is what a browser hands its saved ones.
    pub fn with(settings: Settings) -> Self {
        Self {
            settings,
            ..Self::new()
        }
    }

    /// A surface over default settings.
    pub fn new() -> Self {
        Self {
            settings: Settings::default(),
            theme: Theme::light(),
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            engine: TextEngine::new(),
            focus: Focus::default(),
            cache: None,
            builds: 0,
            root: None,
        }
    }

    /// Note where the pointer is.
    pub fn pointer_moved(&mut self, x: f64, y: f64) -> Action {
        self.pointer = (x, y);
        // A move matters while the button is down: that is what dragging a
        // slider is made of.
        if !self.pointer_down {
            return Action::None;
        }
        self.deliver(&Event::PointerMoved)
    }

    /// Press at the last reported position.
    pub fn pointer_pressed(&mut self) -> Action {
        self.pointer_down = true;
        self.press_origin = Some(self.pointer);
        let action = self.deliver(&Event::PointerPressed);
        // A press on anything that does not ask for the keyboard takes it away
        // from whatever had it — which is what takes the caret out of the field.
        if !matches!(action, Action::Focus(_)) {
            self.settings.focus = None;
        }
        action
    }

    /// Handle a key: traversal, activation, and the arrows.
    ///
    /// `None` means *not mine* — the same word the widgets use — so a key this
    /// surface has no use for still reaches the toolbar behind it. Traversal is
    /// the surface's own business rather than the layer's, because the surface
    /// is what built the order. What the layer does is compare ids, so a control
    /// activated from here reports through exactly the path a press reports
    /// through and the two cannot drift.
    pub fn key_pressed(&mut self, key: Key, modifiers: Modifiers) -> Option<Action> {
        // An accelerator is the browser's, wherever it is pressed: reload and
        // new-tab do not stop working because a preference is on screen.
        if modifiers.is_accelerator() {
            return None;
        }

        let holder = self.settings.focus;
        let editing = self.focus.kind(holder) == Some(FocusKind::Text);

        if key == Key::Tab {
            self.settings.focus = if modifiers.shift {
                self.focus.previous(holder)
            } else {
                self.focus.next(holder)
            };
            return Some(Action::None);
        }

        // A focused field takes the editing keys before anything else can read
        // them as navigation: Left in a field is a caret, not a control.
        if editing {
            match key {
                Key::Enter | Key::Escape => {
                    self.settings.focus = None;
                    return Some(Action::None);
                }
                _ if self.settings.edit_home(key) => return Some(Action::None),
                _ => {}
            }
        }

        Some(match key {
            // Escape leaves the surface only when there is no focus to drop
            // first, so one press never does both.
            Key::Escape => match holder {
                Some(_) => {
                    self.settings.focus = None;
                    Action::None
                }
                None => Action::Close,
            },
            Key::Enter | Key::Character(' ') if holder.is_some() => self.deliver(&Event::Activate),
            Key::Left | Key::Up | Key::Right | Key::Down => {
                let forward = matches!(key, Key::Right | Key::Down);
                let Some(holder) = holder else {
                    // Nothing focused: the arrows are the page's, and a browser
                    // page that scrolls is scrolled by them.
                    return None;
                };
                // Inside a set of choices the arrows move between them and pick
                // what they land on, which is what a radio group does
                // everywhere. Outside one they are offered to the control
                // itself, which is what a slider wants them for.
                match self.focus.step_in_group(holder, forward) {
                    Some(next) => {
                        self.settings.focus = Some(next);
                        self.deliver(&Event::Activate)
                    }
                    None => self.deliver(&Event::Adjust(if forward { 1 } else { -1 })),
                }
            }
            // Anything else — a bare character, a function key — was never this
            // surface's, and saying so is what lets it through.
            _ => return None,
        })
    }

    /// Handle typed text. Returns whether the surface consumed it.
    pub fn text_input(&mut self, character: char) -> bool {
        if self.focus.kind(self.settings.focus) != Some(FocusKind::Text) {
            return false;
        }
        self.settings.home.insert(character);
        true
    }

    /// What the pointer should look like at `x`, `y`.
    ///
    /// Asked of the tree that drew the frame rather than worked out again: the
    /// answer is *what would a press here report*, so a control cannot light up
    /// under the pointer in one place and respond in another.
    pub fn cursor_at(&mut self, x: f64, y: f64) -> otlyra_platform::Cursor {
        match self.action_at(x, y) {
            Action::None => otlyra_platform::Cursor::Default,
            Action::Focus(_) => otlyra_platform::Cursor::Text,
            _ => otlyra_platform::Cursor::Pointer,
        }
    }

    /// Let go.
    pub fn pointer_released(&mut self) {
        self.pointer_down = false;
        self.press_origin = None;
    }

    /// Scroll the surface.
    pub fn scroll_by(&mut self, delta: f64) {
        self.settings.scroll_by(delta);
    }

    /// What a press at `x`, `y` would report, without reporting it.
    ///
    /// The surface knows where it drew things; this is how anything else asks,
    /// rather than working the geometry out a second time and drifting from it.
    pub fn action_at(&mut self, x: f64, y: f64) -> Action {
        let (pointer, down) = (self.pointer, self.pointer_down);
        self.pointer = (x, y);
        self.pointer_down = true;
        let action = self.offer(&Event::PointerPressed);
        self.pointer = pointer;
        self.pointer_down = down;
        action
    }

    /// Offer an event to the last frame's tree and apply what comes back.
    fn deliver(&mut self, event: &Event) -> Action {
        let action = self.offer(event);
        self.settings.apply(action.clone());
        action
    }

    /// Offer an event to the last frame's tree, changing nothing.
    fn offer(&mut self, event: &Event) -> Action {
        let Some(root) = self.root.as_mut() else {
            return Action::None;
        };
        // Nothing in the tree measures text to decide whether it was hit, but
        // the context needs an engine to exist. It is the surface's own, kept
        // rather than made: building one enumerates the system's fonts.
        let mut cx = Cx::new(&mut self.engine);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.focus = self.settings.focus;
        cx.theme = self.theme.clone();

        root.event(event, &mut cx).unwrap_or(Action::None)
    }

    /// Paint the surface into `rect`, in window coordinates.
    ///
    /// A rectangle rather than a size, because the surface does not own the
    /// window: it sits under the browser's toolbar, and the pointer positions it
    /// is asked about are the window's. Placing it anywhere else would mean
    /// translating every press on the way in.
    pub fn build_display_list(&mut self, rect: Rect, text: &mut TextEngine, out: &mut DisplayList) {
        let appearance = Appearance {
            rect,
            settings: self.settings.clone(),
            pointer: self.pointer,
            pointer_down: self.pointer_down,
        };
        if let Some((built, list)) = &self.cache
            && *built == appearance
            && self.root.is_some()
        {
            out.append(list);
            return;
        }

        self.builds += 1;
        let mut built = DisplayList::new();
        let list = &mut built;
        let theme = self.theme.clone();
        let (width, height) = (rect.width, rect.height);
        fill_rounded(list, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.focus = self.settings.focus;
        cx.theme = theme.clone();

        // The order is rebuilt exactly when the tree is, so a frame served from
        // the cache keeps the order that belongs to the tree still standing.
        self.focus.begin();
        let mut root = self.settings.build(&theme, width, &self.focus);
        root.measure(Size::new(width, height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, list);

        controls::hairline(
            &theme,
            list,
            Rect::new(rect.x, rect.y + HEADER_HEIGHT - 1.0, width, 1.0),
        );

        self.root = Some(root);
        self.cache = Some((appearance, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    /// How many display lists this surface has built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draw one frame, which is what gives the surface geometry to be pressed
    /// against.
    fn frame(surface: &mut SettingsSurface) {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        surface.build_display_list(Rect::new(0.0, 0.0, 900.0, 700.0), &mut text, &mut list);
    }

    /// Where on a freshly drawn surface a press reports `wanted`.
    ///
    /// The surface knows where it drew things; the test asks it rather than
    /// repeating the arithmetic, which is the whole point of geometry being
    /// computed in one place. A fresh surface per probe, because a press that
    /// lands changes the state the next frame is built from.
    fn find(wanted: &Action) -> (f64, f64) {
        let mut probe = SettingsSurface::new();
        frame(&mut probe);
        for x in (0..900).step_by(8) {
            for y in (0..700).step_by(2) {
                if probe.action_at(f64::from(x), f64::from(y)) == *wanted {
                    return (f64::from(x), f64::from(y));
                }
            }
        }
        panic!("nothing on the surface reports {wanted:?}");
    }

    #[test]
    fn an_unchanged_frame_is_not_built_a_second_time() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        assert_eq!(surface.builds(), 1);

        frame(&mut surface);
        assert_eq!(surface.builds(), 1, "nothing about it moved");

        // A preference is part of what it is drawn from, so changing one does
        // rebuild it — otherwise the switch would not appear to flip.
        surface.settings.apply(Action::ToggleImages);
        frame(&mut surface);
        assert_eq!(surface.builds(), 2);
    }

    #[test]
    fn a_switch_flips_the_preference_it_is_a_view_of() {
        let (x, y) = find(&Action::ToggleImages);

        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        assert!(surface.settings.load_images);

        surface.pointer_moved(x, y);
        assert_eq!(surface.pointer_pressed(), Action::ToggleImages);
        assert!(!surface.settings.load_images, "the press took effect");
    }

    #[test]
    fn a_radio_picks_one_and_only_one() {
        let (x, y) = find(&Action::SetOnStart(OnStart::Restore));

        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        assert_eq!(surface.settings.on_start, OnStart::Blank);

        surface.pointer_moved(x, y);
        surface.pointer_pressed();
        assert_eq!(surface.settings.on_start, OnStart::Restore);
    }

    #[test]
    fn resetting_puts_every_preference_back() {
        let mut surface = SettingsSurface::new();
        surface.settings.apply(Action::ToggleImages);
        surface.settings.apply(Action::ToggleDoNotTrack);
        surface.settings.apply(Action::SetTextScale(150.0));

        surface.settings.apply(Action::Reset);
        assert!(surface.settings.load_images);
        assert!(!surface.settings.do_not_track);
        assert_eq!(surface.settings.text_scale, 100.0);
    }

    #[test]
    fn the_text_size_lands_on_fives_rather_than_wherever_the_pointer_was() {
        let mut settings = Settings::default();
        settings.apply(Action::SetTextScale(103.2));
        assert_eq!(settings.text_scale, 105.0);
        settings.apply(Action::SetTextScale(101.0));
        assert_eq!(settings.text_scale, 100.0);
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);

        surface.scroll_by(-500.0);
        assert_eq!(surface.settings.scroll, 0.0, "cannot scroll above the top");

        surface.scroll_by(100_000.0);
        let bottom = surface.settings.scroll;
        surface.scroll_by(100.0);
        assert_eq!(
            surface.settings.scroll, bottom,
            "cannot scroll past the end of the content"
        );
    }

    #[test]
    fn typing_reaches_the_home_field_only_when_it_has_the_caret() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        assert!(!surface.text_input('x'), "nothing is focused yet");

        // Tab until the caret is somewhere, which is the field and nothing else:
        // it is the one control on the surface text goes into.
        while !surface.text_input('h') {
            surface.key_pressed(Key::Tab, Modifiers::default());
        }
        assert!(surface.settings.home.text().ends_with('h'));
    }

    /// Press Tab `steps` times on a surface that has been drawn, then activate
    /// whatever holds the keyboard.
    ///
    /// A fresh surface each time, because activating changes the state the next
    /// frame would be built from.
    fn activate_after(steps: usize) -> Option<Action> {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        for _ in 0..steps {
            surface.key_pressed(Key::Tab, Modifiers::default());
        }
        surface.key_pressed(Key::Enter, Modifiers::default())
    }

    /// What every control on the surface reports, in the order Tab reaches them.
    fn traversal() -> Vec<Action> {
        (1..=13).filter_map(activate_after).collect()
    }

    #[test]
    fn tab_reaches_every_control_in_the_order_it_is_drawn() {
        use OnStart::{Blank, Home, Restore};
        assert_eq!(
            traversal(),
            vec![
                // The header, then the cards top to bottom, then the way out —
                // which is exactly the order they are read in.
                Action::Close,
                Action::SetOnStart(Blank),
                Action::SetOnStart(Home),
                Action::SetOnStart(Restore),
                // The home field: Return there closes the caret rather than
                // reporting anything, which is why it says nothing here.
                Action::None,
                Action::ToggleRestoreTabs,
                Action::ToggleImages,
                // Not `ToggleScripts`: there is no script engine, so that switch
                // is drawn dimmed and traversal skips it — a control that cannot
                // be pressed is not a place the keyboard stops.
                // The slider answers the arrows, not Return.
                Action::None,
                Action::ToggleDoNotTrack,
                Action::SetOnStart(Blank),
                Action::SetOnStart(Home),
                Action::SetOnStart(Restore),
                Action::Reset,
            ]
        );
    }

    #[test]
    fn tab_from_the_last_control_wraps_to_the_first() {
        let first = activate_after(1);
        let wrapped = activate_after(traversal().len() + 1);
        assert_eq!(wrapped, first, "past the end is the beginning again");

        // And backwards from nothing is the last of them, not the first.
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        surface.key_pressed(
            Key::Tab,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(
            surface.key_pressed(Key::Enter, Modifiers::default()),
            Some(Action::Reset)
        );
    }

    #[test]
    fn activating_by_key_reports_what_a_press_on_the_same_control_reports() {
        // The property that keeps the two paths from drifting: whatever the
        // keyboard reaches, a pointer reaches the same thing and says the same
        // word about it.
        for wanted in [
            Action::ToggleImages,
            Action::ToggleDoNotTrack,
            Action::Reset,
            Action::Close,
        ] {
            let (x, y) = find(&wanted);
            let mut pressed = SettingsSurface::new();
            frame(&mut pressed);
            pressed.pointer_moved(x, y);
            assert_eq!(pressed.pointer_pressed(), wanted);

            let by_key = (1..=13)
                .filter_map(activate_after)
                .find(|action| *action == wanted);
            assert_eq!(by_key, Some(wanted), "the keyboard cannot reach it");
        }
    }

    #[test]
    fn the_arrows_move_within_a_set_of_choices_and_pick_what_they_land_on() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        // Onto the first of the three start-up choices.
        for _ in 0..2 {
            surface.key_pressed(Key::Tab, Modifiers::default());
        }
        assert_eq!(surface.settings.on_start, OnStart::Blank);

        surface.key_pressed(Key::Down, Modifiers::default());
        assert_eq!(surface.settings.on_start, OnStart::Home);
        surface.key_pressed(Key::Down, Modifiers::default());
        assert_eq!(surface.settings.on_start, OnStart::Restore);
        // Round the end of the group rather than out of it.
        surface.key_pressed(Key::Down, Modifiers::default());
        assert_eq!(surface.settings.on_start, OnStart::Blank);
    }

    #[test]
    fn the_arrows_move_a_slider_that_holds_the_keyboard() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        // The text-size slider is the eighth control the keyboard stops at —
        // the ninth thing drawn, with the dimmed scripts switch passed over.
        for _ in 0..8 {
            surface.key_pressed(Key::Tab, Modifiers::default());
        }
        assert_eq!(surface.settings.text_scale, 100.0);

        surface.key_pressed(Key::Right, Modifiers::default());
        assert_eq!(surface.settings.text_scale, 105.0);
        surface.key_pressed(Key::Left, Modifiers::default());
        surface.key_pressed(Key::Left, Modifiers::default());
        assert_eq!(surface.settings.text_scale, 95.0);
    }

    #[test]
    fn escape_drops_the_focus_before_it_leaves_the_surface() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        surface.key_pressed(Key::Tab, Modifiers::default());

        assert_eq!(
            surface.key_pressed(Key::Escape, Modifiers::default()),
            Some(Action::None),
            "the first press only lets go of the control"
        );
        assert_eq!(
            surface.key_pressed(Key::Escape, Modifiers::default()),
            Some(Action::Close),
            "with nothing focused, it leaves"
        );
    }

    #[test]
    fn moving_the_focus_rebuilds_the_frame_so_the_ring_is_drawn() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        assert_eq!(surface.builds(), 1);

        surface.key_pressed(Key::Tab, Modifiers::default());
        frame(&mut surface);
        assert_eq!(surface.builds(), 2, "the ring is part of the frame");
    }

    #[test]
    fn an_accelerator_is_the_browsers_even_while_the_settings_are_open() {
        let mut surface = SettingsSurface::new();
        frame(&mut surface);
        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        assert_eq!(
            surface.key_pressed(Key::Character('t'), accelerator),
            None,
            "a new tab is not the settings' to swallow"
        );
    }

    #[test]
    fn the_pointer_says_what_it_is_over() {
        use otlyra_platform::Cursor;
        let mut surface = SettingsSurface::new();
        frame(&mut surface);

        let (x, y) = find(&Action::ToggleImages);
        assert_eq!(surface.cursor_at(x, y), Cursor::Pointer);

        // Over the field, where a caret can go, it becomes a text bar.
        let field = (56..700)
            .step_by(2)
            .find(|y| matches!(surface.action_at(485.0, f64::from(*y)), Action::Focus(_)))
            .expect("the surface has a field to type into");
        assert_eq!(surface.cursor_at(485.0, f64::from(field)), Cursor::Text);

        // And over the surface itself it stays an arrow.
        assert_eq!(surface.cursor_at(5.0, 5.0), Cursor::Default);
    }
}
