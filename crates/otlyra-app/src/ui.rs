//! The browser's own interface: the tab strip and the address bar.
//!
//! Drawn with the same `otlyra-gfx` stack the page is drawn with, and for the same
//! reason the plan gives: by the time an interface is needed we already own text
//! layout, hit testing, input routing and painting, and a second toolkit would
//! duplicate all four and bring a second event model with it.
//!
//! One rule holds this file together: **geometry is computed once**, by
//! [`UiLayout::new`], and both painting and hit testing read it. A widget that is
//! drawn in one place and clicked in another is the classic interface bug, and it
//! is only possible when two pieces of code each work the geometry out.

use otlyra_gfx::kurbo::{Affine, Arc, BezPath, Point, RoundedRect, Shape, Stroke};
use otlyra_gfx::peniko::{Brush, Color, Fill, ImageData, ImageSampler};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_platform::{Key, Modifiers};
use otlyra_text::{FontStack, TextEngine};

/// Height of the tab strip, in logical pixels.
const TAB_STRIP_HEIGHT: f64 = 34.0;
/// Height of the address bar.
const ADDRESS_BAR_HEIGHT: f64 = 38.0;
/// Total height the interface takes from the top of the window.
pub const UI_HEIGHT: f64 = TAB_STRIP_HEIGHT + ADDRESS_BAR_HEIGHT;

/// Width of each of the three buttons at the left end of the tab strip.
const BUTTON_WIDTH: f64 = 30.0;

const TAB_WIDTH: f64 = 200.0;
const TAB_GAP: f64 = 2.0;
const NEW_TAB_WIDTH: f64 = 34.0;
const PADDING: f64 = 8.0;
const FONT_SIZE: f32 = 13.0;

const BACKGROUND: Color = Color::from_rgb8(0xe8, 0xe8, 0xea);
const TAB_ACTIVE: Color = Color::from_rgb8(0xff, 0xff, 0xff);
const TAB_INACTIVE: Color = Color::from_rgb8(0xd4, 0xd4, 0xd8);
const FIELD: Color = Color::from_rgb8(0xff, 0xff, 0xff);
const FIELD_FOCUSED: Color = Color::from_rgb8(0xff, 0xff, 0xff);
const FIELD_BORDER: Color = Color::from_rgb8(0x45, 0x7b, 0x9d);
const INK: Color = Color::from_rgb8(0x1d, 0x1d, 0x1f);
const INK_DIM: Color = Color::from_rgb8(0x6b, 0x6b, 0x70);
const DISABLED: Color = Color::from_rgb8(0xb0, 0xb0, 0xb6);

/// A rectangle in logical pixels.
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
    fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn to_kurbo(self) -> otlyra_gfx::kurbo::Rect {
        otlyra_gfx::kurbo::Rect::new(self.x, self.y, self.x + self.width, self.y + self.height)
    }
}

/// Where every part of the interface is, for one window width.
#[derive(Clone, Debug)]
pub struct UiLayout {
    /// The back button.
    pub back: Rect,
    /// The forward button.
    pub forward: Rect,
    /// The reload button.
    pub reload: Rect,
    /// One rectangle per tab, in order.
    pub tabs: Vec<Rect>,
    /// The close target inside each tab.
    pub closes: Vec<Rect>,
    /// The new-tab button.
    pub new_tab: Rect,
    /// The address field.
    pub address: Rect,
}

impl UiLayout {
    /// Work out the geometry for `tab_count` tabs across `width` logical pixels.
    pub fn new(width: f64, tab_count: usize) -> Self {
        let mut tabs = Vec::with_capacity(tab_count);
        let mut closes = Vec::with_capacity(tab_count);

        let button = |index: f64| {
            Rect::new(
                PADDING + index * BUTTON_WIDTH,
                6.0,
                BUTTON_WIDTH - 6.0,
                TAB_STRIP_HEIGHT - 10.0,
            )
        };
        let back = button(0.0);
        let forward = button(1.0);
        let reload = button(2.0);
        let strip_start = reload.x + reload.width + TAB_GAP * 3.0;

        // Tabs shrink to fit rather than overflowing: a tab you cannot see is a tab
        // you cannot close.
        let available = (width - strip_start - NEW_TAB_WIDTH - PADDING).max(0.0);
        let each = if tab_count == 0 {
            TAB_WIDTH
        } else {
            TAB_WIDTH
                .min((available / tab_count as f64) - TAB_GAP)
                .max(60.0)
        };

        for index in 0..tab_count {
            let x = strip_start + index as f64 * (each + TAB_GAP);
            let rect = Rect::new(x, 4.0, each, TAB_STRIP_HEIGHT - 4.0);
            closes.push(Rect::new(
                rect.x + rect.width - 22.0,
                rect.y + 5.0,
                18.0,
                18.0,
            ));
            tabs.push(rect);
        }

        let new_tab_x = tabs
            .last()
            .map_or(strip_start, |last| last.x + last.width + TAB_GAP);
        Self {
            back,
            forward,
            reload,
            new_tab: Rect::new(new_tab_x, 6.0, NEW_TAB_WIDTH - 6.0, TAB_STRIP_HEIGHT - 10.0),
            address: Rect::new(
                PADDING,
                TAB_STRIP_HEIGHT + 4.0,
                (width - PADDING * 2.0).max(0.0),
                ADDRESS_BAR_HEIGHT - 10.0,
            ),
            tabs,
            closes,
        }
    }
}

/// An editable single-line text field.
///
/// Byte offsets, not character counts: the text is UTF-8 and a caret that can land
/// mid-character is a panic waiting for the first non-ASCII address.
#[derive(Clone, Debug, Default)]
pub struct TextField {
    text: String,
    caret: usize,
}

impl TextField {
    /// A field holding `text`, with the caret at the end.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            caret: text.len(),
            text,
        }
    }

    /// The text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret's byte offset.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Replace the text and put the caret at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.caret = self.text.len();
    }

    /// Select everything, which for a field with no selection model means putting
    /// the caret at the end and remembering nothing.
    pub fn insert(&mut self, character: char) {
        self.text.insert(self.caret, character);
        self.caret += character.len_utf8();
    }

    /// Delete the character before the caret.
    pub fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let previous = self.previous_boundary();
        self.text.replace_range(previous..self.caret, "");
        self.caret = previous;
    }

    /// Delete the character after the caret.
    pub fn delete(&mut self) {
        if self.caret >= self.text.len() {
            return;
        }
        let next = self.next_boundary();
        self.text.replace_range(self.caret..next, "");
    }

    /// Move the caret one character left.
    pub fn move_left(&mut self) {
        self.caret = self.previous_boundary();
    }

    /// Move the caret one character right.
    pub fn move_right(&mut self) {
        self.caret = self.next_boundary();
    }

    /// Move the caret to the start.
    pub fn move_home(&mut self) {
        self.caret = 0;
    }

    /// Move the caret to the end.
    pub fn move_end(&mut self) {
        self.caret = self.text.len();
    }

    /// Empty the field.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    fn previous_boundary(&self) -> usize {
        self.text[..self.caret]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.caret..]
            .chars()
            .next()
            .map_or(self.text.len(), |character| {
                self.caret + character.len_utf8()
            })
    }
}

/// What the interface wants the browser to do.
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    /// Nothing.
    None,
    /// Navigate the active tab to this text, as typed.
    Navigate(String),
    /// Open a tab.
    NewTab,
    /// Close a tab by index.
    CloseTab(usize),
    /// Make a tab active.
    SelectTab(usize),
    /// Load the active tab's address again.
    Reload,
    /// Go back one entry in the active tab's history.
    Back,
    /// Go forward one entry.
    Forward,
}

/// What one tab shows in the strip.
#[derive(Clone, Debug)]
pub struct TabLabel {
    /// The tab's title, or its URL until it has one.
    pub title: String,
    /// Whether it is still loading.
    pub loading: bool,
}

/// The interface's own state: what is focused, where the pointer is, what is typed.
#[derive(Clone, Debug, Default)]
pub struct BrowserUi {
    /// The address field.
    pub address: TextField,
    /// Whether the address field has keyboard focus.
    pub address_focused: bool,
    pointer: (f64, f64),
}

impl BrowserUi {
    /// A new interface with an empty address field.
    pub fn new() -> Self {
        Self::default()
    }

    /// Note where the pointer is. Kept so a press can be tested against the same
    /// geometry the last frame drew.
    pub fn pointer_moved(&mut self, x: f64, y: f64) {
        self.pointer = (x, y);
    }

    /// Whether the pointer is over the interface rather than the page.
    pub fn owns_pointer(&self) -> bool {
        self.pointer.1 < UI_HEIGHT
    }

    /// Handle a press at the last reported pointer position.
    pub fn pointer_pressed(&mut self, width: f64, tab_count: usize) -> UiAction {
        let (x, y) = self.pointer;
        if y >= UI_HEIGHT {
            // The press belongs to the page, and it takes focus away from the
            // address bar — which is what every browser does and what makes typing
            // after clicking a page do nothing surprising.
            self.address_focused = false;
            return UiAction::None;
        }

        let layout = UiLayout::new(width, tab_count);

        if layout.back.contains(x, y) {
            return UiAction::Back;
        }
        if layout.forward.contains(x, y) {
            return UiAction::Forward;
        }
        if layout.reload.contains(x, y) {
            return UiAction::Reload;
        }

        for (index, close) in layout.closes.iter().enumerate() {
            if close.contains(x, y) {
                return UiAction::CloseTab(index);
            }
        }
        for (index, tab) in layout.tabs.iter().enumerate() {
            if tab.contains(x, y) {
                self.address_focused = false;
                return UiAction::SelectTab(index);
            }
        }
        if layout.new_tab.contains(x, y) {
            return UiAction::NewTab;
        }
        if layout.address.contains(x, y) {
            self.address_focused = true;
            return UiAction::None;
        }

        self.address_focused = false;
        UiAction::None
    }

    /// Handle a key press. Returns what the browser should do about it.
    pub fn key_pressed(&mut self, key: Key, modifiers: Modifiers) -> UiAction {
        // Accelerators work whether or not the field has focus.
        // F5 reloads whatever has focus, including the address bar: it is not a
        // character, so it cannot be something the user meant to type.
        if key == Key::F5 {
            return UiAction::Reload;
        }

        if modifiers.is_accelerator() {
            return match key {
                Key::Character('r') => UiAction::Reload,
                // The bracket keys are what this platform's browsers use, and the
                // arrows are what the rest of them use; both are here because a
                // person's fingers know one of the two.
                Key::Character('[') | Key::Left => UiAction::Back,
                Key::Character(']') | Key::Right => UiAction::Forward,
                Key::Character('t') => UiAction::NewTab,
                Key::Character('l') => {
                    self.address_focused = true;
                    UiAction::None
                }
                _ => UiAction::None,
            };
        }

        if !self.address_focused {
            return UiAction::None;
        }

        match key {
            Key::Enter => {
                self.address_focused = false;
                let text = self.address.text().trim().to_owned();
                if text.is_empty() {
                    UiAction::None
                } else {
                    UiAction::Navigate(text)
                }
            }
            Key::Escape => {
                self.address_focused = false;
                UiAction::None
            }
            Key::Backspace => {
                self.address.backspace();
                UiAction::None
            }
            Key::Delete => {
                self.address.delete();
                UiAction::None
            }
            Key::Left => {
                self.address.move_left();
                UiAction::None
            }
            Key::Right => {
                self.address.move_right();
                UiAction::None
            }
            Key::Home => {
                self.address.move_home();
                UiAction::None
            }
            Key::End => {
                self.address.move_end();
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    /// Handle typed text. Returns whether the interface consumed it.
    pub fn text_input(&mut self, character: char) -> bool {
        if !self.address_focused {
            return false;
        }
        self.address.insert(character);
        true
    }

    /// Paint the interface across `width` logical pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn build_display_list(
        &self,
        width: f64,
        tabs: &[TabLabel],
        active: usize,
        history: (bool, bool),
        spinner: Option<f32>,
        text: &mut TextEngine,
        list: &mut DisplayList,
    ) {
        let layout = UiLayout::new(width, tabs.len());
        let stack = FontStack::parse_css("system-ui, sans-serif");

        let (can_go_back, can_go_forward) = history;
        fill(list, Rect::new(0.0, 0.0, width, UI_HEIGHT), BACKGROUND, 0.0);
        draw_chevron(list, layout.back, Direction::Back, can_go_back);
        draw_chevron(list, layout.forward, Direction::Forward, can_go_forward);
        draw_reload(list, layout.reload, spinner);

        for (index, (rect, label)) in layout.tabs.iter().zip(tabs).enumerate() {
            let active = index == active;
            fill(
                list,
                *rect,
                if active { TAB_ACTIVE } else { TAB_INACTIVE },
                6.0,
            );

            let title = if label.loading {
                format!("… {}", label.title)
            } else {
                label.title.clone()
            };
            draw_text(
                list,
                text,
                &stack,
                &title,
                rect.x + 10.0,
                rect.y + 6.0,
                (rect.width - 34.0).max(0.0),
                if active { INK } else { INK_DIM },
            );

            // The close target is drawn as the glyph it is, so what is clicked and
            // what is seen are the same rectangle.
            if let Some(close) = layout.closes.get(index) {
                draw_text(
                    list,
                    text,
                    &stack,
                    // Multiplication sign, not the heavier "✕": the latter is a
                    // dingbat that many system fonts have no glyph for, and a
                    // missing glyph is a hollow box where the close button should
                    // be.
                    "×",
                    close.x + 4.0,
                    close.y + 1.0,
                    close.width,
                    INK_DIM,
                );
            }
        }

        fill(list, layout.new_tab, TAB_INACTIVE, 6.0);
        draw_text(
            list,
            text,
            &stack,
            "+",
            layout.new_tab.x + 9.0,
            layout.new_tab.y + 3.0,
            layout.new_tab.width,
            INK,
        );

        let field = layout.address;
        if self.address_focused {
            // The focus ring is a slightly larger rounded rect behind the field:
            // one fill, no stroke, and it cannot be mistaken for a border colour.
            fill(
                list,
                Rect::new(
                    field.x - 2.0,
                    field.y - 2.0,
                    field.width + 4.0,
                    field.height + 4.0,
                ),
                FIELD_BORDER,
                8.0,
            );
        }
        fill(
            list,
            field,
            if self.address_focused {
                FIELD_FOCUSED
            } else {
                FIELD
            },
            6.0,
        );

        let content = self.address.text();
        let placeholder = content.is_empty() && !self.address_focused;
        draw_text(
            list,
            text,
            &stack,
            if placeholder { "Enter a URL" } else { content },
            field.x + 10.0,
            field.y + 5.0,
            field.width - 20.0,
            if placeholder { INK_DIM } else { INK },
        );

        if self.address_focused {
            // The caret sits after the text up to the caret offset, measured with
            // the same engine that drew it — anything else drifts by a pixel per
            // glyph and lands in the wrong place on a long address.
            let before = &content[..self.address.caret().min(content.len())];
            let advance = text.measure(before, &stack, FONT_SIZE).width;
            let caret_x = field.x + 10.0 + f64::from(advance);
            fill(
                list,
                Rect::new(caret_x, field.y + 5.0, 1.5, f64::from(FONT_SIZE) * 1.3),
                INK,
                0.0,
            );
        }
    }
}

/// Size the mark is drawn at on an empty tab, in logical pixels.
const BLANK_MARK_SIZE: f64 = 96.0;

/// Paint a tab that has no document: the empty state, or why the load failed.
///
/// The mark is centred in the content area rather than in the window, so it does
/// not creep upward as the interface grows a toolbar.
pub fn paint_blank_page(
    list: &mut DisplayList,
    width: f64,
    height: f64,
    error: Option<&str>,
    mark: Option<&ImageData>,
    text: &mut TextEngine,
) {
    fill(
        list,
        Rect::new(0.0, 0.0, width, height),
        Color::from_rgb8(0xff, 0xff, 0xff),
        0.0,
    );

    let stack = FontStack::parse_css("system-ui, sans-serif");
    let content_top = UI_HEIGHT;
    let content_height = (height - content_top).max(0.0);
    let centre_y = content_top + content_height / 2.0;

    // An error is a message, not a greeting: it replaces the mark rather than
    // sitting under it, because a logo above a failure reads as decoration on bad
    // news.
    if let Some(error) = error {
        draw_centred_text(list, text, &stack, error, width, centre_y, INK);
        return;
    }

    let mut caption_y = centre_y;
    if let Some(mark) = mark {
        let scale = BLANK_MARK_SIZE / f64::from(mark.width);
        let x = (width - BLANK_MARK_SIZE) / 2.0;
        let y = centre_y - BLANK_MARK_SIZE * 0.75;
        list.push(DisplayItem::Image {
            image: mark.clone().into(),
            sampler: ImageSampler::default(),
            transform: Affine::translate((x, y)) * Affine::scale(scale),
            clip_rect: None,
        });
        caption_y = y + BLANK_MARK_SIZE + 20.0;
    }

    draw_centred_text(
        list,
        text,
        &stack,
        "Type a URL above",
        width,
        caption_y,
        INK_DIM,
    );
}

/// One line of interface text, centred horizontally, with `y` as its top.
fn draw_centred_text(
    list: &mut DisplayList,
    engine: &mut TextEngine,
    stack: &FontStack,
    content: &str,
    width: f64,
    y: f64,
    color: Color,
) {
    let measured = f64::from(engine.measure(content, stack, FONT_SIZE).width);
    draw_text(
        list,
        engine,
        stack,
        content,
        ((width - measured) / 2.0).max(0.0),
        y,
        width,
        color,
    );
}

/// Which way a chevron points.
#[derive(Copy, Clone, PartialEq)]
enum Direction {
    Back,
    Forward,
}

/// A back or forward button: a chevron, drawn rather than typed, and dimmed when
/// there is nowhere to go — a button that does nothing should look like one.
fn draw_chevron(list: &mut DisplayList, rect: Rect, direction: Direction, enabled: bool) {
    fill(list, rect, TAB_INACTIVE, 6.0);

    let centre = Point::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    let reach = rect.height.min(rect.width) / 4.0;
    let tip = if direction == Direction::Back {
        -reach
    } else {
        reach
    };

    let mut path = BezPath::new();
    path.move_to(Point::new(centre.x - tip, centre.y - reach));
    path.line_to(Point::new(centre.x + tip, centre.y));
    path.line_to(Point::new(centre.x - tip, centre.y + reach));
    list.push(DisplayItem::Stroke {
        style: Stroke::new(1.8),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(if enabled { INK } else { DISABLED }),
        brush_transform: None,
        shape: path,
    });
}

/// The reload button: a circular arrow, drawn rather than typed.
///
/// A glyph would be at the mercy of whichever font the system hands back, and a
/// missing glyph is a hollow box where the button should be. A path is the same
/// on every machine.
fn draw_reload(list: &mut DisplayList, rect: Rect, spinner: Option<f32>) {
    fill(list, rect, TAB_INACTIVE, 6.0);

    let centre = Point::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    let radius = rect.width.min(rect.height) / 2.0 - 4.0;

    // The same arrow either way: while a page is loading it turns, and the turn is
    // the only thing that says the browser is busy rather than stuck. A shorter
    // sweep then, so the gap reads as motion.
    let (start, sweep) = match spinner {
        Some(phase) => (f64::from(phase), 4.2),
        None => (-0.9, 5.2),
    };
    list.push(DisplayItem::Stroke {
        style: Stroke::new(1.6),
        transform: Affine::IDENTITY,
        brush: Brush::Solid(INK),
        brush_transform: None,
        shape: Arc::new(centre, (radius, radius), start, sweep, 0.0).to_path(0.05),
    });

    // The arrowhead sits on the arc's end, pointing along it — computed from the
    // same angle the arc ends at, so the two cannot drift apart when either is
    // adjusted.
    let end = start + sweep;
    let tip = Point::new(centre.x + radius * end.cos(), centre.y + radius * end.sin());
    let along = Point::new(-end.sin(), end.cos());
    let across = Point::new(end.cos(), end.sin());
    let size = 4.0;
    let mut head = BezPath::new();
    head.move_to(Point::new(tip.x + along.x * size, tip.y + along.y * size));
    head.line_to(Point::new(
        tip.x - along.x * size * 0.4 + across.x * size,
        tip.y - along.y * size * 0.4 + across.y * size,
    ));
    head.line_to(Point::new(
        tip.x - along.x * size * 0.4 - across.x * size,
        tip.y - along.y * size * 0.4 - across.y * size,
    ));
    head.close_path();
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(INK),
        brush_transform: None,
        shape: head,
    });
}

/// A filled, optionally rounded rectangle.
fn fill(list: &mut DisplayList, rect: Rect, color: Color, radius: f64) {
    let shape = if radius > 0.0 {
        RoundedRect::from_rect(rect.to_kurbo(), radius).to_path(0.1)
    } else {
        rect.to_kurbo().to_path(0.1)
    };
    list.push(DisplayItem::Fill {
        style: Fill::NonZero,
        transform: Affine::IDENTITY,
        brush: Brush::Solid(color),
        brush_transform: None,
        shape,
    });
}

/// One line of interface text, clipped by shaping it to `max_width`.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    list: &mut DisplayList,
    engine: &mut TextEngine,
    stack: &FontStack,
    content: &str,
    x: f64,
    y: f64,
    max_width: f64,
    color: Color,
) {
    if content.is_empty() || max_width <= 0.0 {
        return;
    }
    let shaped = engine.shape(content, stack, FONT_SIZE, Some(max_width as f32));

    // One line only: a tab title that wrapped would push the address bar down the
    // window.
    for run in shaped.runs.iter().filter(|run| run.line == 0) {
        list.push_glyphs(
            &run.font,
            run.font_size,
            run.normalized_coords.clone(),
            Brush::Solid(color),
            Affine::translate((x, y)),
            true,
            run.glyphs.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(count: usize) -> Vec<TabLabel> {
        (0..count)
            .map(|index| TabLabel {
                title: format!("Tab {index}"),
                loading: false,
            })
            .collect()
    }

    #[test]
    fn pressing_the_reload_button_reloads() {
        let mut ui = BrowserUi::new();
        let layout = UiLayout::new(1000.0, 2);

        ui.pointer_moved(layout.reload.x + 4.0, layout.reload.y + 4.0);
        assert_eq!(ui.pointer_pressed(1000.0, 2), UiAction::Reload);
    }

    /// The button sits before the tabs and must not overlap the first of them —
    /// the bug where reload closes a tab instead.
    #[test]
    fn the_reload_button_is_clear_of_the_first_tab() {
        let layout = UiLayout::new(1000.0, 3);
        assert!(layout.reload.x < layout.tabs[0].x);
        assert!(layout.reload.x + layout.reload.width <= layout.tabs[0].x);
    }

    #[test]
    fn a_press_selects_the_tab_it_is_drawn_over() {
        let mut ui = BrowserUi::new();
        let layout = UiLayout::new(1000.0, 3);
        let second = layout.tabs[1];

        ui.pointer_moved(second.x + 5.0, second.y + 5.0);
        assert_eq!(ui.pointer_pressed(1000.0, 3), UiAction::SelectTab(1));
    }

    #[test]
    fn the_close_target_is_inside_the_tab_and_wins_over_it() {
        let mut ui = BrowserUi::new();
        let layout = UiLayout::new(1000.0, 2);
        let close = layout.closes[1];

        assert!(
            layout.tabs[1].contains(close.x + 1.0, close.y + 1.0),
            "the close target must sit inside its tab"
        );
        ui.pointer_moved(close.x + 2.0, close.y + 2.0);
        assert_eq!(ui.pointer_pressed(1000.0, 2), UiAction::CloseTab(1));
    }

    #[test]
    fn pressing_the_address_bar_focuses_it_and_pressing_the_page_does_not() {
        let mut ui = BrowserUi::new();
        let layout = UiLayout::new(1000.0, 1);

        ui.pointer_moved(layout.address.x + 20.0, layout.address.y + 5.0);
        ui.pointer_pressed(1000.0, 1);
        assert!(ui.address_focused);

        ui.pointer_moved(400.0, UI_HEIGHT + 100.0);
        ui.pointer_pressed(1000.0, 1);
        assert!(!ui.address_focused, "clicking the page takes focus away");
    }

    #[test]
    fn typing_goes_to_the_address_bar_only_when_it_has_focus() {
        let mut ui = BrowserUi::new();
        assert!(!ui.text_input('a'));
        assert_eq!(ui.address.text(), "");

        ui.address_focused = true;
        assert!(ui.text_input('a'));
        assert!(ui.text_input('b'));
        assert_eq!(ui.address.text(), "ab");
    }

    #[test]
    fn enter_navigates_to_what_was_typed_and_drops_focus() {
        let mut ui = BrowserUi::new();
        ui.address_focused = true;
        for character in "example.com".chars() {
            ui.text_input(character);
        }

        let action = ui.key_pressed(Key::Enter, Modifiers::default());
        assert_eq!(action, UiAction::Navigate("example.com".to_owned()));
        assert!(!ui.address_focused);
    }

    #[test]
    fn an_empty_address_navigates_nowhere() {
        let mut ui = BrowserUi::new();
        ui.address_focused = true;
        assert_eq!(
            ui.key_pressed(Key::Enter, Modifiers::default()),
            UiAction::None
        );
    }

    #[test]
    fn editing_keys_move_and_delete_by_character_not_by_byte() {
        // Every one of these steps lands mid-byte-sequence if the field counts
        // bytes: each of these characters is two bytes.
        let mut field = TextField::new("привет");
        field.move_left();
        field.backspace();
        assert_eq!(field.text(), "привт", "backspace deletes before the caret");

        field.move_home();
        field.delete();
        assert_eq!(field.text(), "ривт", "delete removes after it");

        field.move_end();
        field.insert('о');
        assert_eq!(field.text(), "ривто", "the caret survives at the end");
    }

    #[test]
    fn the_caret_never_lands_inside_a_character() {
        let mut field = TextField::new("日本語");
        for _ in 0..5 {
            field.move_left();
        }
        assert_eq!(field.caret(), 0);
        for _ in 0..5 {
            field.move_right();
        }
        assert_eq!(field.caret(), field.text().len());
    }

    #[test]
    fn the_accelerator_opens_a_tab_whatever_has_focus() {
        let mut ui = BrowserUi::new();
        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        assert_eq!(
            ui.key_pressed(Key::Character('t'), accelerator),
            UiAction::NewTab
        );
        assert_eq!(
            ui.key_pressed(Key::Character('l'), accelerator),
            UiAction::None
        );
        assert!(ui.address_focused, "cmd-L focuses the address bar");
    }

    /// Tabs shrink to share the width, down to a floor. Past the floor they run
    /// off the edge, which is a stated gap: a scrolling or collapsing tab strip is
    /// interface work this milestone does not do.
    #[test]
    fn f5_and_the_accelerator_both_reload() {
        let mut ui = BrowserUi::new();
        assert_eq!(
            ui.key_pressed(Key::F5, Modifiers::default()),
            UiAction::Reload
        );

        let accelerator = Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        assert_eq!(
            ui.key_pressed(Key::Character('r'), accelerator),
            UiAction::Reload
        );
    }

    /// F5 while typing an address is still a reload — it types nothing, so there
    /// is nothing for it to mean instead.
    #[test]
    fn f5_reloads_even_with_the_address_bar_focused() {
        let mut ui = BrowserUi::new();
        ui.address_focused = true;
        assert_eq!(
            ui.key_pressed(Key::F5, Modifiers::default()),
            UiAction::Reload
        );
    }

    #[test]
    fn tabs_share_the_width_down_to_a_readable_minimum() {
        let few = UiLayout::new(1000.0, 2);
        let many = UiLayout::new(1000.0, 8);
        assert!(
            many.tabs[0].width < few.tabs[0].width,
            "more tabs must mean narrower tabs"
        );
        assert!(
            many.tabs.last().expect("tabs").x + many.tabs.last().expect("tabs").width <= 1000.0,
            "eight tabs still fit in a thousand pixels"
        );

        let crowded = UiLayout::new(400.0, 8);
        assert_eq!(crowded.tabs[0].width, 60.0, "the floor holds");
    }

    #[test]
    fn an_empty_tab_shows_the_mark_and_a_hint() {
        let mut engine = TextEngine::isolated();
        let mark = otlyra_gfx::decode_image(crate::MARK).expect("the mark decodes");
        let mut list = DisplayList::new();
        paint_blank_page(&mut list, 1000.0, 700.0, None, Some(&mark), &mut engine);

        let images = list
            .items()
            .iter()
            .filter(|item| matches!(item, DisplayItem::Image { .. }))
            .count();
        assert_eq!(images, 1, "the mark");
        assert!(
            list.items()
                .iter()
                .any(|item| matches!(item, DisplayItem::Glyphs { .. })),
            "and the hint under it"
        );
    }

    /// A logo over a failure reads as decoration on bad news.
    #[test]
    fn a_failed_tab_shows_the_error_instead_of_the_mark() {
        let mut engine = TextEngine::isolated();
        let mark = otlyra_gfx::decode_image(crate::MARK).expect("the mark decodes");
        let mut list = DisplayList::new();
        paint_blank_page(
            &mut list,
            1000.0,
            700.0,
            Some("could not fetch"),
            Some(&mark),
            &mut engine,
        );

        assert!(
            !list
                .items()
                .iter()
                .any(|item| matches!(item, DisplayItem::Image { .. }))
        );
    }

    #[test]
    fn the_interface_paints_something_for_every_tab() {
        let ui = BrowserUi::new();
        let mut engine = TextEngine::isolated();
        let mut list = DisplayList::new();
        ui.build_display_list(
            1000.0,
            &labels(3),
            0,
            (true, false),
            None,
            &mut engine,
            &mut list,
        );

        let glyph_runs = list
            .items()
            .iter()
            .filter(|item| matches!(item, DisplayItem::Glyphs { .. }))
            .count();
        assert!(glyph_runs >= 4, "three titles and the new-tab button");
    }
}
