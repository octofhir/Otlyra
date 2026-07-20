//! `about:otlyra` — what this program is, drawn rather than fetched.
//!
//! The one page a browser must be able to show when everything else is broken:
//! no network, no parser, no cascade. Built from [`crate::widget`] for that
//! reason as much as for the look — a page that needed the engine to work could
//! not be shown to report that the engine does not.

use otlyra_gfx::DisplayList;
use otlyra_text::TextEngine;

use crate::widget::controls;
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Child, Cx, Event, Flex, Gap, Insets, Label, Padding, Paragraph, Rect, Size, Stack,
    fill_rounded,
};

/// What this build is called.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the about page reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Go to the settings.
    OpenSettings,
}

/// The about page.
pub struct AboutSurface {
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    pointer: (f64, f64),
    root: Option<Child<Action>>,
}

impl Default for AboutSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl AboutSurface {
    /// A page in the default theme.
    pub fn new() -> Self {
        Self {
            theme: Theme::light(),
            pointer: (-1.0, -1.0),
            root: None,
        }
    }

    /// Note where the pointer is.
    pub fn pointer_moved(&mut self, x: f64, y: f64) {
        self.pointer = (x, y);
    }

    /// Press at the last reported position.
    pub fn pointer_pressed(&mut self, text: &mut TextEngine) -> Action {
        let Some(root) = self.root.as_mut() else {
            return Action::None;
        };
        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.theme = self.theme.clone();
        root.event(&Event::PointerPressed, &mut cx)
            .unwrap_or(Action::None)
    }

    /// Paint the page into `rect`, in window coordinates.
    pub fn build_display_list(
        &mut self,
        rect: Rect,
        text: &mut TextEngine,
        list: &mut DisplayList,
    ) {
        let theme = self.theme.clone();
        fill_rounded(list, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.theme = theme.clone();

        let mut root = self.build(&theme);
        root.measure(Size::new(rect.width, rect.height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, list);
        self.root = Some(root);
    }

    fn build(&self, theme: &Theme) -> Child<Action> {
        let facts: Vec<(&str, String)> = vec![
            ("Version", VERSION.to_owned()),
            ("Engine", "Otlyra, in-house".to_owned()),
            ("Layout", "block and inline, floats, flex".to_owned()),
            ("Style", "Stylo cascade".to_owned()),
            ("Text", "parley shaping, Skia rasterization".to_owned()),
            ("Rasterizer", "Skia, behind a seven-method seam".to_owned()),
        ];

        let rows: Vec<Child<Action>> = facts
            .into_iter()
            .map(|(name, value)| {
                Box::new(Stack::row(
                    theme.gap * 2.0,
                    vec![
                        Box::new(crate::widget::Fixed::width(
                            120.0,
                            Box::new(Align::left(Box::new(Label::new(
                                name,
                                theme.font_size,
                                theme.ink_dim,
                            )))),
                        )),
                        Box::new(Flex::new(
                            1.0,
                            Box::new(Align::left(Box::new(Label::new(
                                value,
                                theme.font_size,
                                theme.ink,
                            )))),
                        )),
                    ],
                )) as Child<Action>
            })
            .collect();

        let heading: Child<Action> =
            Box::new(Label::new("Otlyra", theme.font_size + 9.0, theme.ink));
        let blurb: Child<Action> = Box::new(Paragraph::new(
            "A browser engine written from the parser up: its own DOM, its own \
             box tree, its own display list. This page is drawn by the same \
             widget layer as the toolbar, so it can still be shown when the \
             engine underneath it cannot lay out a document.",
            theme.font_size,
            theme.ink_dim,
        ));

        let column: Child<Action> = Box::new(Stack::column(
            theme.inset * 1.5,
            vec![
                heading,
                blurb,
                controls::card(theme, "This build", rows),
                Box::new(Stack::row(
                    theme.gap,
                    vec![
                        controls::button(
                            theme,
                            Action::OpenSettings,
                            "Settings",
                            controls::Emphasis::Normal,
                            true,
                        ),
                        Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                    ],
                )),
            ],
        ));

        // Centred and no wider than a paragraph can be read across, the same
        // rule the settings follow — one measure of a comfortable line, not two.
        Box::new(Stack::column(
            0.0,
            vec![
                Box::new(Padding::new(
                    Insets::all(theme.inset * 3.0),
                    Box::new(Stack::row(
                        0.0,
                        vec![
                            Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                            Box::new(crate::widget::Fixed::width(560.0, column)),
                            Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                        ],
                    )),
                )),
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
            ],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_button_is_reachable_where_it_is_drawn() {
        let mut about = AboutSurface::new();
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        about.build_display_list(Rect::new(0.0, 0.0, 900.0, 700.0), &mut text, &mut list);

        let mut found = None;
        for y in (0..700).step_by(4) {
            for x in (0..900).step_by(20) {
                about.pointer_moved(f64::from(x), f64::from(y));
                if about.pointer_pressed(&mut text) == Action::OpenSettings {
                    found = Some((x, y));
                    break;
                }
            }
        }
        assert!(found.is_some(), "the settings button was not on the page");
    }

    #[test]
    fn the_page_says_which_build_it_is() {
        assert!(!VERSION.is_empty());
    }
}
