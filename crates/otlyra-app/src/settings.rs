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
    Align, Child, Cx, Event, Flex, Gap, Insets, Label, Overflow, Padding, Rect, Scroll, Size,
    Stack, fill_rounded,
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
#[derive(Clone, Debug)]
pub struct Settings {
    /// Where a new window starts.
    pub on_start: OnStart,
    /// The address the home button goes to.
    pub home: crate::ui::TextField,
    /// Whether the home field has the keyboard.
    pub home_focused: bool,
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
    /// Which control has the keyboard, for traversal by key.
    pub focus: Option<u32>,
    /// How far the surface is scrolled.
    pub scroll: f64,
    overflow: Overflow,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on_start: OnStart::Blank,
            home: crate::ui::TextField::new("https://example.com/"),
            home_focused: false,
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
    /// Put the caret in the home field.
    FocusHome,
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
            Action::FocusHome => self.home_focused = true,
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
                *self = Self::default();
                self.scroll = scroll;
            }
        }
    }

    /// Handle a key. Typing goes to the home field when it has the caret.
    pub fn key_pressed(&mut self, key: Key, _modifiers: Modifiers) -> Action {
        if !self.home_focused {
            return match key {
                Key::Escape => Action::Close,
                _ => Action::None,
            };
        }
        match key {
            Key::Enter | Key::Escape => {
                self.home_focused = false;
                Action::None
            }
            Key::Backspace => {
                self.home.backspace();
                Action::None
            }
            Key::Delete => {
                self.home.delete();
                Action::None
            }
            Key::Left => {
                self.home.move_left();
                Action::None
            }
            Key::Right => {
                self.home.move_right();
                Action::None
            }
            Key::Home => {
                self.home.move_home();
                Action::None
            }
            Key::End => {
                self.home.move_end();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Handle typed text. Returns whether the surface consumed it.
    pub fn text_input(&mut self, character: char) -> bool {
        if !self.home_focused {
            return false;
        }
        self.home.insert(character);
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

    /// Build this frame's tree.
    pub fn build(&self, theme: &Theme, width: f64) -> Child<Action> {
        let rows = vec![
            self.startup_card(theme),
            self.content_card(theme),
            self.privacy_card(theme),
            self.reset_row(theme),
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
                self.header(theme),
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    /// The bar across the top: what this surface is, and the way out of it.
    fn header(&self, theme: &Theme) -> Child<Action> {
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

    fn startup_card(&self, theme: &Theme) -> Child<Action> {
        let choices: Vec<Child<Action>> = OnStart::ALL
            .iter()
            .map(|choice| {
                controls::radio(
                    theme,
                    Action::SetOnStart(*choice),
                    self.on_start == *choice,
                    choice.label(),
                )
            })
            .collect();

        let home_field = controls::TextInput::new(controls::FieldView {
            text: self.home.text().to_owned(),
            caret: self.home_focused.then(|| self.home.caret()),
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
                        Box::new(crate::widget::Button::new(Action::FocusHome, home_field)),
                    )),
                ),
                controls::setting_row(
                    theme,
                    "Reopen tabs",
                    Some("Bring back what was open when the browser last closed."),
                    controls::toggle(theme, Action::ToggleRestoreTabs, self.restore_tabs),
                ),
            ],
        )
    }

    fn content_card(&self, theme: &Theme) -> Child<Action> {
        controls::card(
            theme,
            "Content",
            vec![
                controls::setting_row(
                    theme,
                    "Load images",
                    Some("Fetch the pictures a page asks for."),
                    controls::toggle(theme, Action::ToggleImages, self.load_images),
                ),
                controls::setting_row(
                    theme,
                    "Run scripts",
                    Some("Execute the JavaScript a page carries."),
                    controls::toggle(theme, Action::ToggleScripts, self.run_scripts),
                ),
                controls::divider(theme),
                controls::setting_row(
                    theme,
                    format!("Text size — {}%", self.text_scale as i64),
                    Some("Scales the text on every page."),
                    Box::new(controls::Slider::new(
                        self.text_scale,
                        (50.0, 200.0),
                        Action::SetTextScale,
                    )),
                ),
            ],
        )
    }

    fn privacy_card(&self, theme: &Theme) -> Child<Action> {
        controls::card(
            theme,
            "Privacy",
            vec![
                controls::setting_row(
                    theme,
                    "Do Not Track",
                    Some("Sends a request sites are free to ignore, and most do."),
                    controls::toggle(theme, Action::ToggleDoNotTrack, self.do_not_track),
                ),
                controls::divider(theme),
                controls::setting_row(
                    theme,
                    "Start with",
                    None,
                    controls::segmented(
                        theme,
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

    fn reset_row(&self, theme: &Theme) -> Child<Action> {
        Box::new(Stack::row(
            theme.gap,
            vec![
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                controls::button(theme, Action::Reset, "Reset all", Emphasis::Danger, true),
            ],
        ))
    }
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
    root: Option<Child<Action>>,
}

impl Default for SettingsSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsSurface {
    /// A surface over default settings.
    pub fn new() -> Self {
        Self {
            settings: Settings::default(),
            theme: Theme::light(),
            pointer: (-1.0, -1.0),
            pointer_down: false,
            press_origin: None,
            engine: TextEngine::new(),
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
        // A press anywhere but the home field takes the caret out of it.
        if action != Action::FocusHome {
            self.settings.home_focused = false;
        }
        action
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
    pub fn build_display_list(
        &mut self,
        rect: Rect,
        text: &mut TextEngine,
        list: &mut DisplayList,
    ) {
        let theme = self.theme.clone();
        let (width, height) = (rect.width, rect.height);
        fill_rounded(list, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.pointer_down = self.pointer_down;
        cx.press_origin = self.press_origin;
        cx.focus = self.settings.focus;
        cx.theme = theme.clone();

        let mut root = self.settings.build(&theme, width);
        root.measure(Size::new(width, height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, list);

        controls::hairline(
            &theme,
            list,
            Rect::new(rect.x, rect.y + HEADER_HEIGHT - 1.0, width, 1.0),
        );

        self.root = Some(root);
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
        for x in [730, 700, 620, 485, 200, 140] {
            for y in (56..700).step_by(2) {
                if probe.action_at(f64::from(x), f64::from(y)) == *wanted {
                    return (f64::from(x), f64::from(y));
                }
            }
        }
        panic!("nothing on the surface reports {wanted:?}");
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
        let mut settings = Settings::default();
        assert!(!settings.text_input('x'));

        settings.home_focused = true;
        settings.home.clear();
        assert!(settings.text_input('h'));
        assert_eq!(settings.home.text(), "h");
    }
}
