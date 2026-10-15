use super::*;

fn inspect<R: Send + 'static>(
    window: &WindowBrowser,
    read: impl FnOnce(&mut Browser) -> R + Send + 'static,
) -> R {
    let (tx, rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            tx.send(read(browser)).unwrap()
        })))
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap()
}

fn settled(window: &mut WindowBrowser) {
    let serial = window.serial;
    wait(window, |window| {
        window.snapshot.serial >= serial && window.pending.is_empty()
    });
}

fn three_tabs(viewport: Viewport) -> WindowBrowser {
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    window.apply(UiAction::NewTab);
    window.apply(UiAction::NewTab);
    settled(&mut window);
    window.compose(viewport);
    window
}

fn click(window: &mut WindowBrowser, rect: crate::widget::Rect) {
    window.on_event(PlatformEvent::PointerMoved {
        x: rect.x + rect.width / 2.0,
        y: rect.y + rect.height / 2.0,
    });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    window.on_event(PlatformEvent::PointerReleased);
}

fn tab_control(window: &WindowBrowser, close: bool) -> crate::widget::Rect {
    window
        .ui
        .describe()
        .into_iter()
        .filter(|node| {
            if close {
                node.role == crate::widget::Role::Button && node.label == "Close tab"
            } else {
                node.role == crate::widget::Role::Tab
            }
        })
        .nth(1)
        .unwrap()
        .rect
}

#[test]
fn retained_tab_controls_keep_their_target_after_an_owner_reorder() {
    for scale in [1.0, 2.0] {
        for close in [false, true] {
            let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
            let mut window = three_tabs(viewport);
            let target = window.tabs[1].id;
            let rect = tab_control(&window, close);
            inspect(&window, move |browser| browser.move_tab(target, 0));
            wait(&mut window, |window| window.tabs[0].id == target);
            // Receive model changes without rebuilding the controls still on screen.
            click(&mut window, rect);
            if close {
                assert!(!window.tabs.iter().any(|tab| tab.id == target));
                assert_eq!(window.tabs.len(), 2);
            } else {
                assert_eq!(window.tabs[window.active].id, target);
            }
            settled(&mut window);
            assert_eq!(
                inspect(&window, move |browser| browser.tab_index(target).is_none()),
                close
            );
            if !close {
                assert_eq!(inspect(&window, |browser| browser.active_id()), target);
            }
        }
    }
}

#[test]
fn a_retired_tabs_retained_close_button_cannot_close_its_neighbour() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = three_tabs(viewport);
    let target = window.tabs[1].id;
    let rect = tab_control(&window, true);
    inspect(&window, move |browser| {
        browser.close_tab(browser.tab_index(target).unwrap())
    });
    wait(&mut window, |window| {
        !window.tabs.iter().any(|tab| tab.id == target)
    });
    let survivors: Vec<_> = window.tabs.iter().map(|tab| tab.id).collect();
    click(&mut window, rect);
    assert_eq!(
        window.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        survivors
    );
}

#[test]
fn synchronous_browser_tab_controls_keep_their_identity_too() {
    let viewport = Viewport::new(900, 640, 1.0);
    let window = three_tabs(viewport);
    inspect(&window, move |browser| {
        browser.compose(viewport);
        let target = browser.tabs[1].id;
        let described = browser.ui.describe();
        let tab = described
            .iter()
            .filter(|node| node.role == crate::widget::Role::Tab)
            .nth(1)
            .unwrap()
            .rect;
        let close = described
            .iter()
            .filter(|node| node.role == crate::widget::Role::Button && node.label == "Close tab")
            .nth(1)
            .unwrap()
            .rect;
        browser.move_tab(target, 0);
        for rect in [tab, close] {
            browser.on_event(PlatformEvent::PointerMoved {
                x: rect.x + rect.width / 2.0,
                y: rect.y + rect.height / 2.0,
            });
            browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
            browser.on_event(PlatformEvent::PointerReleased);
            if rect == tab {
                assert_eq!(browser.active_id(), target);
            }
        }
        assert!(browser.tab_index(target).is_none());
        assert_eq!(browser.tabs.len(), 2);
    });
}

fn system_window(viewport: Viewport) -> WindowBrowser {
    let mut window = WindowBrowser::spawn(viewport, true, |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Fields), Default::default(), text);
        browser.navigate("about:about");
        browser
    })
    .unwrap();
    ready(&mut window);
    window.compose(viewport);
    window
}

#[test]
fn hidden_system_navigation_retires_the_previous_accessibility_description() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = system_window(viewport);
    assert!(!window.snapshot.described.is_empty());
    let previous = Arc::clone(&window.snapshot.described);
    window.on_event(PlatformEvent::VisibilityChanged(false));
    wait(&mut window, |window| !window.snapshot.window_visible);
    window.apply(UiAction::Navigate("about:cookies".into()));
    settled(&mut window);
    assert_eq!(window.tabs[window.active].system, Some(SystemPage::Cookies));
    assert!(
        window.snapshot.described.is_empty(),
        "a hidden replacement must not expose the old surface's controls"
    );
    assert!(!Arc::ptr_eq(&previous, &window.snapshot.described));
    window.on_event(PlatformEvent::VisibilityChanged(true));
    wait(&mut window, |window| {
        window.snapshot.window_visible && !window.snapshot.described.is_empty()
    });
    assert!(!Arc::ptr_eq(&previous, &window.snapshot.described));
}

#[test]
fn a_system_navigation_retires_document_accessibility_with_a_retained_navigation_id() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    let document = Arc::clone(&window.snapshot.document);
    assert!(
        document
            .nodes
            .iter()
            .any(|(_, node)| node.role() == accesskit::Role::TextInput)
    );
    let navigation = window.tabs[window.active].navigation;
    window.apply(UiAction::Navigate("about:cookies".into()));
    settled(&mut window);
    assert_eq!(window.tabs[window.active].navigation, navigation);
    assert!(!Arc::ptr_eq(&document, &window.snapshot.document));
    assert!(
        !window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.role() == accesskit::Role::TextInput)
    );
}

#[test]
fn queued_content_input_cannot_cross_a_system_page_replacement() {
    let viewport = Viewport::new(900, 640, 1.0);
    for document in [false, true] {
        let mut window = if document {
            fields_window(viewport)
        } else {
            system_window(viewport)
        };
        window.on_event(PlatformEvent::PointerMoved { x: 800.0, y: 200.0 });
        settled(&mut window);
        assert_eq!(inspect(&window, |browser| browser.pointer), (800.0, 200.0));
        let resume = pause_owner(&window);
        window
            .sender
            .send(Command::Task(Box::new(move |browser| {
                browser.navigate("about:cookies");
                browser.compose_content_scene(viewport);
            })))
            .unwrap();
        window.on_event(PlatformEvent::PointerMoved { x: 850.0, y: 500.0 });
        drop(resume);
        settled(&mut window);
        assert_eq!(window.tabs[window.active].system, Some(SystemPage::Cookies));
        assert_eq!(inspect(&window, |browser| browser.pointer), (800.0, 200.0));
        window.on_event(PlatformEvent::PointerMoved { x: 810.0, y: 550.0 });
        settled(&mut window);
        assert_eq!(inspect(&window, |browser| browser.pointer), (810.0, 550.0));
    }
}

fn open_page_menu(window: &mut WindowBrowser, viewport: Viewport) {
    window.compose(viewport);
    window.on_event(PlatformEvent::PointerMoved {
        x: 100.0,
        y: UI_HEIGHT + 180.0,
    });
    window.on_event(PlatformEvent::ContextMenuRequested);
    wait(window, |window| window.ui.context_menu().is_some());
}

#[test]
fn owner_navigation_retires_the_native_content_menu() {
    let viewport = Viewport::new(900, 640, 1.0);
    for destination in ["https://replacement.example/", "about:cookies"] {
        let mut window = window(viewport, "https://page.example/");
        ready(&mut window);
        open_page_menu(&mut window, viewport);
        let old = window.tabs[window.active].navigation;
        inspect(&window, move |browser| browser.navigate(destination));
        wait(&mut window, |window| {
            window.tabs[window.active].url == destination
        });
        if destination.starts_with("about:") {
            assert_eq!(window.tabs[window.active].navigation, old);
        }
        assert!(
            window.ui.context_menu().is_none(),
            "retired document left native menu open"
        );
    }
}

#[test]
fn owner_dismissal_retires_a_native_menu_without_navigation() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    open_page_menu(&mut window, viewport);
    let resume = pause_owner(&window);
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            browser.ui.dismiss_context_menu();
            browser.context_target = None;
        })))
        .unwrap();
    window.send(None, UiAction::None, None, false, None);
    drop(resume);
    settled(&mut window);
    assert!(window.ui.context_menu().is_none());
}

#[test]
fn queued_context_command_cannot_use_a_replacements_menu_target() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    open_page_menu(&mut window, viewport);
    let resume = pause_owner(&window);
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            browser.navigate("https://replacement.example/");
            browser.wait_for_load(Duration::from_secs(5));
            browser.compose_content_scene(viewport);
            browser.context_menu_requested();
            assert!(browser.context_target.is_some());
            assert!(!browser.active_page().unwrap().has_selection());
        })))
        .unwrap();
    // A retained row has already closed its native popup when it returns the action.
    window.ui.dismiss_context_menu();
    window.apply(UiAction::Context(crate::ui::ContextCommand::SelectAll));
    drop(resume);
    settled(&mut window);
    assert!(
        !inspect(&window, |browser| browser
            .active_page()
            .unwrap()
            .has_selection()),
        "old menu selected the replacement document"
    );
}

#[test]
fn current_context_commands_still_execute_after_native_popup_closes() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    open_page_menu(&mut window, viewport);
    window.ui.dismiss_context_menu();
    window.apply(UiAction::Context(crate::ui::ContextCommand::SelectAll));
    settled(&mut window);
    assert!(inspect(&window, |browser| browser
        .active_page()
        .unwrap()
        .has_selection()));
    assert!(window.ui.context_menu().is_none());
}

#[test]
fn an_unoffered_context_menu_ack_does_not_remain_pending() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    window.on_event(PlatformEvent::PointerMoved { x: 100.0, y: 20.0 });
    window.on_event(PlatformEvent::ContextMenuRequested);
    settled(&mut window);
    assert!(window.ui.context_menu().is_none());
    assert!(window.context_serial.is_none());
    assert!(window.context_source.is_none());
}

#[test]
fn retiring_content_menus_preserves_the_native_browser_menu() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    open_page_menu(&mut window, viewport);
    window.ui.open_menu();
    window.apply(UiAction::None);
    settled(&mut window);
    inspect(&window, |browser| {
        browser.navigate("https://replacement.example/")
    });
    wait(&mut window, |window| {
        window.tabs[window.active].url == "https://replacement.example/"
    });
    assert!(window.ui.menu_open());
}
