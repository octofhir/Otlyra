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

use otlyra_app::settings::{Settings, SettingsSurface};
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
/// Preferences that draw the same on every machine.
///
/// The download folder is named rather than left to the platform: the default is
/// the home directory's `Downloads`, so a surface built from the defaults would
/// draw whoever ran the test into the snapshot.
fn fixed_settings() -> Settings {
    let mut settings = Settings::default();
    settings.apply(otlyra_app::settings::Action::SetDownloadDirectory(
        "/downloads".to_owned(),
    ));
    settings
}

fn toolbar() -> (BrowserUi, Vec<TabLabel>, TextEngine) {
    let mut ui = BrowserUi::new();
    ui.address.set_text("https://example.com/some/path");
    ui.bookmark = otlyra_app::ui::Bookmarked::No;
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
    list.append(&ui.build_display_list(900.0, 600.0, tabs, 0, (true, true), spinner, text));
    list
}

#[test]
fn the_toolbar_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    let list = toolbar_list(&mut ui, &tabs, &mut text, Some(1.2));
    insta::assert_snapshot!(outline(&list));
}

/// The star says whether this page is kept, and it says it in a way that survives
/// being looked at rather than read: hollow is a stroke, kept is a filled accent.
/// Without this the two states could converge and every golden would still pass.
#[test]
fn the_bookmark_star_is_hollow_until_the_page_is_kept() {
    let (mut ui, tabs, mut text) = toolbar();
    let accent = hex(&BrowserUi::new().theme.accent);
    // Counted rather than searched for: the accent is also the colour of an active
    // tab's dot, so *whether* it appears says nothing. One more of it does.
    let accents = |outline: &str| {
        outline
            .lines()
            .filter(|line| line.starts_with(&format!("fill {accent}")))
            .count()
    };

    ui.bookmark = otlyra_app::ui::Bookmarked::No;
    let hollow = outline(&toolbar_list(&mut ui, &tabs, &mut text, None));
    ui.bookmark = otlyra_app::ui::Bookmarked::Yes;
    let kept = outline(&toolbar_list(&mut ui, &tabs, &mut text, None));

    assert_eq!(
        accents(&kept),
        accents(&hollow) + 1,
        "keeping the page must fill exactly one more shape in the accent\n\
         hollow:\n{hollow}\nkept:\n{kept}"
    );
    assert!(
        hollow.lines().count() == kept.lines().count(),
        "the two stars must be one item each, not one and two"
    );
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
    ui.open_menu();
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    insta::assert_snapshot!(outline(&list));
}

/// What the omnibox offers under what has been typed: as wide as the field,
/// hanging off its own rectangle rather than off a number written here.
#[test]
fn the_suggestions_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    toolbar_list(&mut ui, &tabs, &mut text, None);
    ui.focus_address();
    ui.set_suggestions(vec![
        otlyra_app::ui::Suggestion {
            title: "Otlyra — a browser engine".to_owned(),
            url: "https://octofhir.github.io/Otlyra/".to_owned(),
            kept: true,
        },
        otlyra_app::ui::Suggestion {
            title: "Search results".to_owned(),
            url: "https://example.com/search?q=widgets".to_owned(),
            kept: false,
        },
    ]);
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    insta::assert_snapshot!(outline(&list));
}

/// The panel that names what the pointer has been resting on.
#[test]
fn the_tooltip_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    // Drawn once so there is a frame to read what the pointer is over from,
    // then rested on the reload button with the clock wound past the pause.
    toolbar_list(&mut ui, &tabs, &mut text, None);
    ui.pointer_moved(80.0, UI_HEIGHT - 20.0, &mut text);
    ui.wind_rest_back(std::time::Duration::from_millis(1_000));
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    insta::assert_snapshot!(outline(&list));
}

/// The menu the reader asks for over the page: the one panel placed where a
/// press landed rather than against a control, so where it lands is the thing
/// worth pinning.
#[test]
fn the_context_menu_outline_is_stable() {
    let (mut ui, tabs, mut text) = toolbar();
    ui.open_context_menu(
        280.0,
        UI_HEIGHT + 60.0,
        vec![
            otlyra_app::ui::ContextRow::Command(
                otlyra_app::ui::ContextCommand::OpenLinkInNewTab,
                true,
            ),
            otlyra_app::ui::ContextRow::Command(
                otlyra_app::ui::ContextCommand::CopyLinkAddress,
                true,
            ),
            otlyra_app::ui::ContextRow::Divider,
            otlyra_app::ui::ContextRow::Command(otlyra_app::ui::ContextCommand::Back, false),
            otlyra_app::ui::ContextRow::Command(otlyra_app::ui::ContextCommand::Reload, true),
            otlyra_app::ui::ContextRow::Command(
                otlyra_app::ui::ContextCommand::InspectElement,
                true,
            ),
        ],
    );
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    insta::assert_snapshot!(outline(&list));
}

/// And the same menu asked for near the bottom right corner, which has to open
/// back onto the window instead of running off it.
#[test]
fn a_context_menu_at_the_corner_opens_back_onto_the_window() {
    let (mut ui, tabs, mut text) = toolbar();
    ui.open_context_menu(
        860.0,
        560.0,
        vec![
            otlyra_app::ui::ContextRow::Command(otlyra_app::ui::ContextCommand::Reload, true),
            otlyra_app::ui::ContextRow::Command(
                otlyra_app::ui::ContextCommand::InspectElement,
                true,
            ),
        ],
    );
    let list = toolbar_list(&mut ui, &tabs, &mut text, None);
    let panel = list
        .items()
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Fill { shape, .. } => Some(shape.bounding_box()),
            _ => None,
        })
        // The panel is the one filled shape as wide as a menu that is drawn
        // below the toolbar.
        .find(|bounds| bounds.width() > 200.0 && bounds.width() < 300.0 && bounds.y0 > UI_HEIGHT)
        .expect("the panel was drawn");
    assert!(
        panel.x1 <= 900.0 && panel.y1 <= 600.0,
        "the panel ran off the window at {panel:?}"
    );
    assert!(
        panel.y1 <= 561.0 && panel.x1 <= 861.0,
        "a menu that does not fit below and right of the press belongs above \
         and left of it, not over the edge: {panel:?}"
    );
}

#[test]
fn the_settings_outline_is_stable() {
    let mut surface = SettingsSurface::with(fixed_settings());
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

#[test]
fn the_bookmarks_page_outline_is_stable() {
    let mut store = otlyra_app::bookmarks::BookmarkStore::default();
    store.add("https://example.com/", "Example Domain");
    store.add(
        "https://doc.rust-lang.org/stable/std/index.html?search=long+enough+to+be+cut",
        "Rust standard library",
    );
    let mut surface = otlyra_app::bookmarks::BookmarksSurface::new();
    let mut text = TextEngine::isolated();
    let mut list = DisplayList::new();
    surface.build_display_list(
        Rect::new(0.0, UI_HEIGHT, 900.0, 700.0 - UI_HEIGHT),
        &store,
        &mut text,
        &mut list,
    );
    insta::assert_snapshot!(outline(&list));
}

#[test]
fn the_downloads_page_outline_is_stable() {
    let mut store = otlyra_app::downloads::DownloadStore::default();
    store.record(
        "people.csv",
        "https://example.test/exports/people.csv",
        Some("text/csv".to_owned()),
        b"id,name\n1,Ada\n".to_vec(),
    );
    let mut surface = otlyra_app::downloads::DownloadsSurface::new();
    let mut text = TextEngine::isolated();
    let mut list = DisplayList::new();
    surface.build_display_list(
        Rect::new(0.0, UI_HEIGHT, 900.0, 700.0 - UI_HEIGHT),
        &store,
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
