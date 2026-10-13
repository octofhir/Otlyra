//! Tab commands through the same event route the window uses.

use otlyra_platform::{Key, Modifiers, Painter, PlatformEvent, Viewport};

use super::*;
use crate::fetcher::{Loaded, Loader};
use crate::menu::Command;
use crate::ui::SystemPage;

struct Pages;

impl Loader for Pages {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            bytes: format!(
                "<!doctype html><title>{url}</title><body>{}",
                "<p>A paragraph long enough to keep a reader's place.</p>".repeat(100)
            )
            .into_bytes(),
            content_type: Some("text/html".to_owned()),
            charset: Some("utf-8".to_owned()),
            final_url: url.to_owned(),
            ..Default::default()
        })
    }
}

fn loaded() -> Browser {
    let mut browser = Browser::new(Pages);
    go(&mut browser, "https://first.example/");
    browser
}

fn go(browser: &mut Browser, url: &str) {
    browser.navigate(url);
    settle(browser);
}

fn settle(browser: &mut Browser) {
    browser.wait_for_load(std::time::Duration::from_secs(5));
}

fn accelerator(shift: bool) -> Modifiers {
    Modifiers {
        command: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        shift,
        ..Modifiers::default()
    }
}

fn key(browser: &mut Browser, key: Key, modifiers: Modifiers) {
    browser.on_event(PlatformEvent::KeyPressed { key, modifiers });
}

fn menu(browser: &mut Browser, command: Command) {
    browser.on_event(PlatformEvent::MenuCommand(command.id()));
}

fn paint(browser: &mut Browser) {
    browser.paint(
        &mut otlyra_gfx::RecordingPainter::new(),
        Viewport::new(800, 600, 1.0),
    );
}

#[test]
fn closing_background_tabs_keeps_the_active_identity_and_address_edit() {
    let mut browser = loaded();
    browser.new_tab();
    go(&mut browser, "https://middle.example/");
    browser.new_tab();
    go(&mut browser, "https://last.example/");
    browser.select_tab(1);
    let active = browser.active_id();
    paint(&mut browser);
    browser.activate_surface(SURFACE_CHROME);
    browser.ui.focus_address();
    browser
        .ui
        .address
        .set_text("an unfinished search".to_owned());

    browser.close_tab(0);
    assert_eq!(browser.active_id(), active);
    assert_eq!(browser.active, 0);
    assert_eq!(browser.ui.address.text(), "an unfinished search");
    assert!(browser.ui.address_focused());
    assert_eq!(browser.keyboard_surface, SURFACE_CHROME);

    browser.close_tab(1);
    assert_eq!(browser.active_id(), active);
    assert_eq!(browser.ui.address.text(), "an unfinished search");
    assert!(browser.ui.address_focused());
}

#[test]
fn closing_the_active_tab_selects_its_right_neighbour_then_its_left() {
    let mut browser = loaded();
    let first = browser.active_id();
    let middle = browser.open_tab();
    let last = browser.open_tab();
    browser.select_tab(1);

    browser.close_tab(1);
    assert_eq!(browser.active_id(), last);
    assert!(browser.tab_index(middle).is_none());
    browser.close_tab(1);
    assert_eq!(browser.active_id(), first);
    assert_eq!(browser.ui.address.text(), "https://first.example/");
}

#[test]
fn reopening_restores_history_forward_entries_and_scroll_with_a_new_identity() {
    let mut browser = loaded();
    go(&mut browser, "https://second.example/");
    go(&mut browser, "https://third.example/");
    browser.go_back();
    settle(&mut browser);
    paint(&mut browser);
    browser
        .active_page_mut()
        .expect("the page")
        .set_scroll(260.0);
    let old = browser.active_id();
    let visits = browser.history.visits().count();

    browser.close_tab(0);
    assert!(browser.world.page(old).is_none());
    assert!(browser.reopen_closed_tab());
    settle(&mut browser);
    paint(&mut browser);

    assert_eq!(browser.tabs.len(), 1, "reuse the blank placeholder");
    assert_ne!(browser.active_id(), old);
    assert!(browser.tab_index(old).is_none());
    assert_eq!(browser.tabs[0].url, "https://second.example/");
    assert_eq!(browser.tabs[0].history.len(), 3);
    assert_eq!(browser.tabs[0].position, 1);
    assert!(browser.can_go_back() && browser.can_go_forward());
    assert_eq!(browser.page(0).expect("restored page").scroll(), 260.0);
    assert_eq!(browser.history.visits().count(), visits);

    browser.go_forward();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://third.example/");
    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.page(0).expect("back again").scroll(), 260.0);
}

#[test]
fn a_background_tab_reopens_in_its_old_slot_with_its_own_scroll() {
    let mut browser = loaded();
    paint(&mut browser);
    browser.active_page_mut().expect("first").set_scroll(380.0);
    browser.new_tab();
    go(&mut browser, "https://second.example/");
    let second = browser.active_id();
    paint(&mut browser);
    browser.active_page_mut().expect("second").set_scroll(90.0);
    browser.close_tab(0);

    assert!(browser.reopen_closed_tab());
    settle(&mut browser);
    paint(&mut browser);
    assert_eq!(browser.active, 0);
    assert_eq!(browser.tabs[1].id, second);
    assert_eq!(browser.page(0).expect("restored first").scroll(), 380.0);
    assert_eq!(browser.page(1).expect("second stayed put").scroll(), 90.0);
}

#[test]
fn reopening_a_browser_page_keeps_its_history_and_focus_root() {
    let mut browser = loaded();
    browser.open_system(SystemPage::Settings);
    browser.settings.settings.scroll = 72.0;
    browser.close_tab(0);
    menu(&mut browser, Command::ReopenClosedTab);

    assert_eq!(browser.tabs.len(), 1);
    assert_eq!(browser.system_page(), Some(SystemPage::Settings));
    assert_eq!(browser.keyboard_surface, SURFACE_SYSTEM);
    assert_eq!(browser.settings.settings.scroll, 72.0);
    assert!(browser.can_go_back());
    browser.go_back();
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://first.example/");
}

#[test]
fn closed_tabs_are_restored_newest_first_and_the_list_is_bounded() {
    let mut browser = Browser::new(Pages);
    for index in 0..30 {
        browser.open_system(if index % 2 == 0 {
            SystemPage::Settings
        } else {
            SystemPage::About
        });
        browser.close_tab(0);
    }
    assert_eq!(browser.closed_tabs.len(), 25);
    for index in (5..30).rev() {
        assert!(browser.reopen_closed_tab());
        assert_eq!(
            browser.system_page(),
            Some(if index % 2 == 0 {
                SystemPage::Settings
            } else {
                SystemPage::About
            })
        );
    }
    let active = browser.active_id();
    assert!(!browser.reopen_closed_tab());
    browser.close_tab(usize::MAX);
    assert_eq!(browser.active_id(), active);
    assert!(browser.closed_tabs.is_empty());
}

#[test]
fn duplicating_preserves_the_reading_position_and_has_independent_history() {
    let mut browser = loaded();
    go(&mut browser, "https://second.example/");
    browser.go_back();
    settle(&mut browser);
    paint(&mut browser);
    browser
        .active_page_mut()
        .expect("original")
        .set_scroll(240.0);
    let original = browser.active_id();
    let visits = browser.history.visits().count();
    menu(&mut browser, Command::DuplicateTab);
    settle(&mut browser);
    paint(&mut browser);

    assert_eq!(browser.tabs.len(), 2);
    assert_eq!(browser.active, 1);
    assert_eq!(browser.tabs[0].id, original);
    assert_ne!(browser.tabs[1].id, original);
    assert_eq!(browser.page(1).expect("copy").scroll(), 240.0);
    assert!(browser.can_go_forward());
    assert_eq!(browser.history.visits().count(), visits);
    go(&mut browser, "https://different.example/");
    assert!(!browser.can_go_forward());
    browser.select_tab(0);
    assert_eq!(browser.tabs[0].url, "https://first.example/");
    assert!(browser.can_go_forward());
    assert_eq!(browser.page(0).expect("original").scroll(), 240.0);
}

#[test]
fn tab_shortcuts_work_from_every_keyboard_surface_and_wrap() {
    let mut browser = loaded();
    browser.open_tab();
    browser.open_tab();
    for surface in [
        SURFACE_PAGE,
        SURFACE_CHROME,
        SURFACE_SYSTEM,
        SURFACE_INSPECTOR,
    ] {
        browser.select_tab(0);
        browser.keyboard_surface = surface;
        key(
            &mut browser,
            Key::Tab,
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(browser.active, 1, "next from {surface:?}");
        key(&mut browser, Key::Character('9'), accelerator(false));
        assert_eq!(browser.active, 2);
        menu(&mut browser, Command::NextTab);
        assert_eq!(browser.active, 0);
        menu(&mut browser, Command::PreviousTab);
        assert_eq!(browser.active, 2);
        key(&mut browser, Key::Character('1'), accelerator(false));
        assert_eq!(browser.active, 0);
        key(
            &mut browser,
            Key::Tab,
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(browser.active, 2);
        key(
            &mut browser,
            Key::PageUp,
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(browser.active, 1);
        key(
            &mut browser,
            Key::PageDown,
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(browser.active, 2);
        key(&mut browser, Key::Character('['), accelerator(true));
        assert_eq!(browser.active, 1);
        key(&mut browser, Key::Character('}'), accelerator(true));
        assert_eq!(browser.active, 2);
    }
}

#[test]
fn closing_and_reopening_from_the_keyboard_releases_the_old_context() {
    let mut browser = loaded();
    let original = browser.active_id();
    key(&mut browser, Key::Character('w'), accelerator(false));
    assert!(browser.tab_index(original).is_none());
    assert!(browser.world.page(original).is_none());
    key(&mut browser, Key::Character('T'), accelerator(true));
    settle(&mut browser);
    assert_eq!(browser.tabs.len(), 1);
    assert_eq!(browser.tabs[0].url, "https://first.example/");
    assert_ne!(browser.active_id(), original);
}

#[test]
fn reopening_an_unfinished_navigation_records_the_arrival_and_rejects_old_replies() {
    let mut browser = Browser::new(Pages);
    browser.open_system(SystemPage::About);
    browser.navigate("https://pending.example/");
    let old = browser.active_id();
    assert_eq!(browser.tabs[0].history.len(), 1);
    browser.close_tab(0);
    assert!(browser.reopen_closed_tab());
    settle(&mut browser);

    assert_ne!(browser.active_id(), old);
    assert!(browser.world.page(old).is_none());
    assert_eq!(browser.tabs[0].url, "https://pending.example/");
    assert_eq!(browser.tabs[0].history.len(), 2);
    assert_eq!(browser.tabs[0].position, 1);
    browser.go_back();
    assert_eq!(browser.system_page(), Some(SystemPage::About));
}

#[test]
fn option_enter_accepts_a_marked_suggestion_and_empty_input_opens_nothing() {
    let mut browser = loaded();
    go(&mut browser, "https://second.example/");
    paint(&mut browser);
    browser.activate_surface(SURFACE_CHROME);
    browser.ui.focus_address();
    browser.ui.address.set_text("first".to_owned());
    browser.refresh_suggestions();
    assert!(browser.ui.suggesting());
    key(&mut browser, Key::Down, Modifiers::default());
    key(
        &mut browser,
        Key::Enter,
        Modifiers {
            alt: true,
            ..Modifiers::default()
        },
    );
    settle(&mut browser);
    assert_eq!(browser.tabs[0].url, "https://second.example/");
    assert_eq!(browser.tabs[1].url, "https://first.example/");

    paint(&mut browser);
    browser.activate_surface(SURFACE_CHROME);
    browser.ui.focus_address();
    browser.ui.address.set_text("  ".to_owned());
    key(
        &mut browser,
        Key::Enter,
        Modifiers {
            alt: true,
            ..Modifiers::default()
        },
    );
    assert_eq!(browser.tabs.len(), 2);
    assert_eq!(browser.active, 1);
}

#[test]
fn option_enter_opens_the_typed_address_beside_the_current_page() {
    let mut browser = loaded();
    let original = browser.active_id();
    paint(&mut browser);
    browser.activate_surface(SURFACE_CHROME);
    browser.ui.focus_address();
    browser
        .ui
        .address
        .set_text("https://new.example/".to_owned());
    key(
        &mut browser,
        Key::Enter,
        Modifiers {
            alt: true,
            ..Modifiers::default()
        },
    );
    settle(&mut browser);

    assert_eq!(browser.tabs.len(), 2);
    assert_eq!(browser.active, 1);
    assert_eq!(browser.tabs[0].id, original);
    assert_eq!(browser.tabs[0].url, "https://first.example/");
    assert_eq!(browser.tabs[1].url, "https://new.example/");
    assert_eq!(browser.keyboard_surface, SURFACE_PAGE);
}
