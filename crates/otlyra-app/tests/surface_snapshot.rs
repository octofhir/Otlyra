//! Outline snapshots of what each surface draws, in the order it draws it.
//!
//! One line per display item — kind, colour, geometry — with glyph runs
//! summarized to a count and an origin. This is what a golden PNG cannot say:
//! *the order things are painted in*, whether a hover wash was emitted at all,
//! and exactly which rectangle moved when one does. A control shifting by a
//! pixel changes a coordinate here and fails the snapshot, without the weight
//! of a PNG per state.
//!
//! Shaped with [`TextEngine::isolated`], so every coordinate is the vendored
//! font's answer and holds on any machine.
//!
//! Review and accept changes with `cargo insta review`.

use otlyra_app::settings::SettingsSurface;
use otlyra_app::ui::{BrowserUi, Rect, TabLabel, UI_HEIGHT};
use otlyra_gfx::kurbo::Shape;
use otlyra_gfx::peniko::{Brush, Color};
use otlyra_gfx::{DisplayItem, DisplayList};
use otlyra_text::TextEngine;

fn tabs(titles: &[(&str, bool)]) -> Vec<TabLabel> {
    titles
        .iter()
        .enumerate()
        .map(|(index, (title, loading))| TabLabel {
            id: index as u64 + 1,
            title: (*title).to_owned(),
            loading: *loading,
        })
        .collect()
}

fn hex(color: &Color) -> String {
    let rgba = color.to_rgba8();
    format!("#{:02x}{:02x}{:02x}{:02x}", rgba.r, rgba.g, rgba.b, rgba.a)
}

fn paint(brush: &Brush) -> String {
    match brush {
        Brush::Solid(color) => hex(color),
        other => format!("{other:?}"),
    }
}

fn rect(rect: otlyra_gfx::kurbo::Rect) -> String {
    format!(
        "({:.1}, {:.1}) {:.1}x{:.1}",
        rect.x0,
        rect.y0,
        rect.width(),
        rect.height()
    )
}

/// One line per item: what was drawn, where, in what, in this order.
fn outline(list: &DisplayList) -> String {
    let mut lines = Vec::new();
    for item in list.items() {
        lines.push(match item {
            DisplayItem::PushLayer {
                alpha,
                transform,
                clip,
                ..
            } => {
                format!(
                    "layer alpha={alpha} clip {}",
                    rect(transform.transform_rect_bbox(clip.bounding_box()))
                )
            }
            DisplayItem::PopLayer => "end layer".to_owned(),
            DisplayItem::Blurred {
                transform,
                brush,
                blur,
                shape,
            } => {
                format!(
                    "shadow {} blur={blur} {}",
                    paint(brush),
                    rect(transform.transform_rect_bbox(shape.bounding_box()))
                )
            }
            DisplayItem::Fill {
                transform,
                brush,
                shape,
                ..
            } => {
                format!(
                    "fill {} {}",
                    paint(brush),
                    rect(transform.transform_rect_bbox(shape.bounding_box()))
                )
            }
            DisplayItem::Stroke {
                style,
                transform,
                brush,
                shape,
                ..
            } => {
                format!(
                    "stroke {} width={} {}",
                    paint(brush),
                    style.width,
                    rect(transform.transform_rect_bbox(shape.bounding_box()))
                )
            }
            DisplayItem::Glyphs {
                font_size,
                brush,
                transform,
                glyphs,
                ..
            } => {
                let origin = transform.translation();
                format!(
                    "glyphs n={} size={font_size} {} at ({:.1}, {:.1})",
                    glyphs.len(),
                    paint(brush),
                    origin.x,
                    origin.y
                )
            }
            DisplayItem::Image {
                image, transform, ..
            } => {
                let origin = transform.translation();
                format!(
                    "image {}x{} at ({:.1}, {:.1})",
                    image.width, image.height, origin.x, origin.y
                )
            }
            DisplayItem::HitTest {
                rect: region,
                transform,
                id,
            } => {
                format!(
                    "hit {id:?} {}",
                    rect(transform.transform_rect_bbox(*region))
                )
            }
        });
    }
    lines.join("\n")
}

/// The busy toolbar: tabs, history, a spinner, an address.
fn toolbar() -> (BrowserUi, Vec<TabLabel>, TextEngine) {
    let mut ui = BrowserUi::new();
    ui.address.set_text("https://example.com/some/path");
    let tabs = tabs(&[
        ("CSS support — Otlyra", false),
        ("A title long enough that it has to be cut short", true),
        ("Otlyra", false),
    ]);
    (ui, tabs, TextEngine::isolated())
}

fn toolbar_list(
    ui: &mut BrowserUi,
    tabs: &[TabLabel],
    text: &mut TextEngine,
    spinner: Option<f32>,
) -> DisplayList {
    let mut list = DisplayList::new();
    ui.build_display_list(
        900.0,
        600.0,
        tabs,
        0,
        (true, true),
        spinner,
        text,
        &mut list,
    );
    list
}

#[test]
fn the_toolbar_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    let list = toolbar_list(&mut ui, &tabs, &mut text, Some(1.2));
    insta::assert_snapshot!(outline(&list));
}

#[test]
fn the_focused_selected_address_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    // Drawn once so the field claims its focus id, then focused — ⌘L's path,
    // which selects the whole address — and drawn again for the snapshot.
    toolbar_list(&mut ui, &tabs, &mut text, None);
    ui.focus_address();
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    insta::assert_snapshot!(outline(&list));
}

#[test]
fn the_open_menu_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    ui.menu_open = true;
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    insta::assert_snapshot!(outline(&list));
}

#[test]
fn the_settings_outline_is_stable() {
    let mut surface = SettingsSurface::new();
    let mut text = TextEngine::isolated();
    let mut list = DisplayList::new();
    surface.build_display_list(
        Rect::new(0.0, UI_HEIGHT, 900.0, 700.0 - UI_HEIGHT),
        &mut text,
        &mut list,
    );
    insta::assert_snapshot!(outline(&list));
}

#[test]
fn the_about_page_outline_is_stable() {
    let mut surface = otlyra_app::about::AboutSurface::new();
    let mut text = TextEngine::isolated();
    let mut list = DisplayList::new();
    surface.build_display_list(
        Rect::new(0.0, UI_HEIGHT, 900.0, 700.0 - UI_HEIGHT),
        &mut text,
        &mut list,
    );
    insta::assert_snapshot!(outline(&list));
}

/// The wash under the pointer is emitted, and emitted before the mark it sits
/// under — the two facts a PNG can blur together and a unit test over state
/// cannot see at all.
#[test]
fn hovering_a_button_emits_the_wash_under_its_mark() {
    let (mut ui, tabs, mut text) = toolbar();
    let plain = outline(&toolbar_list(&mut ui, &tabs, &mut text, None));

    // Over the reload button, mid-toolbar.
    ui.pointer_moved(80.0, UI_HEIGHT - 20.0, &mut text);
    let hovered = outline(&toolbar_list(&mut ui, &tabs, &mut text, None));

    let wash = hex(&ui.theme.hover);
    assert!(
        !plain.contains(&wash),
        "with the pointer nowhere, nothing is washed"
    );
    let wash_at = hovered
        .find(&wash)
        .expect("the hovered frame draws the wash");
    let mark_at = hovered[wash_at..]
        .find("stroke")
        .expect("the mark is drawn after its wash");
    assert!(
        mark_at > 0,
        "the wash goes down before the mark on top of it"
    );
}
