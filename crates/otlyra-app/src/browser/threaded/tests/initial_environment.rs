use super::*;
use crate::browser::frame::LAYER_PAGE;

struct Properties;
impl Loader for Properties {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            bytes: br#"<!doctype html><body style="margin:0"><p id=state></p><script>
                const state = document.getElementById('state');
                state.setAttribute('width', String(window.innerWidth));
                state.setAttribute('height', String(window.innerHeight));
                state.setAttribute('ratio', String(window.devicePixelRatio));
                state.setAttribute('visibility', document.visibilityState);
                let resizes = 0;
                state.setAttribute('resizes', '0');
                window.onresize = () => {
                    state.setAttribute('resizes', String(++resizes));
                    state.setAttribute('latest', `${window.innerWidth}|${window.innerHeight}|${window.devicePixelRatio}`);
                    Promise.resolve().then(() => state.setAttribute('microtask', 'done'));
                };
            </script>"#
                .to_vec(),
            content_type: Some("text/html".into()),
            final_url: url.into(),
            ..Default::default()
        })
    }
}

fn property(window: &WindowBrowser, name: &'static str) -> String {
    let (tx, rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            let document = browser.active_page().unwrap().document();
            let node = otlyra_css::stylo_dom::select(document, "#state").unwrap()[0];
            tx.send(document.attr(node, name).unwrap_or("").to_owned())
                .unwrap();
        })))
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap()
}

fn properties_window(viewport: Viewport) -> WindowBrowser {
    WindowBrowser::spawn(viewport, false, |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Properties), Default::default(), text);
        browser.navigate("https://environment.test/");
        browser
    })
    .unwrap()
}

#[test]
fn resize_handlers_observe_current_dimensions_and_dpr_only_changes_do_not_resize() {
    let mut window = properties_window(Viewport::new(900, 600, 1.0));
    ready(&mut window);
    assert_eq!(property(&window, "resizes"), "0");
    let resized = Viewport::new(1600, 1000, 2.0);
    window.on_event(PlatformEvent::Resized(resized));
    wait(&mut window, |window| window.snapshot.viewport == resized);
    assert_eq!(property(&window, "latest"), "800|500|2");
    assert_eq!(property(&window, "microtask"), "done");
    assert_eq!(property(&window, "resizes"), "1");
    let dpr_only = Viewport::new(2400, 1500, 3.0);
    window.on_event(PlatformEvent::Resized(dpr_only));
    wait(&mut window, |window| window.snapshot.viewport == dpr_only);
    assert_eq!(property(&window, "resizes"), "1");
    window.on_event(PlatformEvent::Resized(Viewport::new(3000, 1800, 3.0)));
    wait(&mut window, |window| window.snapshot.viewport.width == 3000);
    assert_eq!(property(&window, "latest"), "1000|600|3");
    assert_eq!(property(&window, "resizes"), "2");
}

#[test]
fn each_tab_retains_its_zoomed_css_viewport() {
    let mut window = properties_window(Viewport::new(900, 600, 2.0));
    ready(&mut window);
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            // Update the active factor without persisting a test preference.
            browser.zoom = 1.5;
            browser.ui.zoom = 1.5;
            browser.sync_world();
        })))
        .unwrap();
    wait(&mut window, |window| {
        (window.snapshot.zoom - 1.5).abs() < f32::EPSILON
    });
    assert_eq!(property(&window, "latest"), "300|200|3");
    let tab = window.tabs[0].id;
    window.apply(UiAction::NewTab);
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial == serial);
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            browser.zoom = 1.0;
            browser.ui.zoom = 1.0;
            browser.sync_world();
            let page = browser.world.page(tab).unwrap();
            let document = page.document();
            let node = otlyra_css::stylo_dom::select(document, "#state").unwrap()[0];
            assert_eq!(document.attr(node, "latest"), Some("300|200|3"));
            assert_eq!(document.attr(node, "resizes"), Some("1"));
        })))
        .unwrap();
    // Order a barrier so assertions on content's owner cannot silently lag.
    let (tx, rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| {
            tx.send(()).unwrap();
        })))
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn inspector_docking_resizes_the_document_viewport() {
    let mut window = properties_window(Viewport::new(900, 600, 1.0));
    ready(&mut window);
    window.apply(UiAction::ToggleInspector);
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial == serial);
    let (tx, rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            let height = (600.0 - browser.inspector.dock_height(600.0)).floor();
            let document = browser.active_page().unwrap().document();
            let node = otlyra_css::stylo_dom::select(document, "#state").unwrap()[0];
            assert_eq!(
                document.attr(node, "latest"),
                Some(format!("900|{height}|1").as_str())
            );
            assert_eq!(document.attr(node, "resizes"), Some("1"));
            tx.send(()).unwrap();
        })))
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    window.apply(UiAction::ToggleInspector);
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial == serial);
    assert_eq!(property(&window, "latest"), "900|600|1");
    assert_eq!(property(&window, "resizes"), "2");
}

#[test]
fn the_first_inline_script_uses_the_shell_viewport_and_chrome_mode() {
    for scale in [1.0, 2.0] {
        for interface in [true, false] {
            let viewport = Viewport::new((919.0 * scale) as u32, (661.0 * scale) as u32, scale);
            let mut window = WindowBrowser::spawn(viewport, interface, move |text| {
                let mut browser = Browser::with_fetcher_and_text(
                    Fetcher::spawn(Properties),
                    Default::default(),
                    text,
                );
                // Deliberately leave the factory environment inconsistent with UI.
                if interface {
                    browser.hide_interface();
                }
                browser.navigate("https://environment.test/");
                browser
            })
            .unwrap();
            ready(&mut window);
            let (tx, rx) = mpsc::channel();
            window
                .sender
                .send(Command::Task(Box::new(move |browser| {
                    let document = browser.active_page().unwrap().document();
                    let node = otlyra_css::stylo_dom::select(document, "#state").unwrap()[0];
                    let properties = ["width", "height", "ratio", "visibility"]
                        .map(|name| document.attr(node, name).unwrap_or("").to_owned());
                    tx.send(properties).unwrap();
                })))
                .unwrap();
            let properties = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(
                properties,
                [
                    "919".into(),
                    (661.0 - if interface { UI_HEIGHT } else { 0.0 }).to_string(),
                    scale.to_string(),
                    "visible".into()
                ],
                "scale={scale} interface={interface}"
            );
            let scene = &window.scenes[&window.tabs[0].id].1;
            let page = scene
                .layers
                .iter()
                .find(|layer| layer.id == LayerId(LAYER_PAGE))
                .unwrap();
            assert_eq!(
                page.rect.y,
                if interface {
                    (UI_HEIGHT * scale) as u32
                } else {
                    0
                }
            );
        }
    }
}

#[test]
fn aligning_the_factory_environment_preserves_initial_hidden_state() {
    let viewport = Viewport::new(1838, 1322, 2.0);
    let mut window = WindowBrowser::spawn(viewport, false, |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Properties), Default::default(), text);
        browser.on_event(PlatformEvent::VisibilityChanged(false));
        browser.navigate("https://environment.test/hidden");
        browser
    })
    .unwrap();
    wait(&mut window, |window| {
        !window.snapshot.labels.is_empty() && !window.snapshot.labels[0].loading
    });
    let (tx, rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            let document = browser.active_page().unwrap().document();
            let node = otlyra_css::stylo_dom::select(document, "#state").unwrap()[0];
            tx.send((
                document.attr(node, "ratio").unwrap().to_owned(),
                document.attr(node, "visibility").unwrap().to_owned(),
            ))
            .unwrap();
        })))
        .unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ("2".into(), "hidden".into())
    );
    assert!(window.scenes.is_empty());
}

#[test]
fn native_initial_chrome_exposes_pinned_appearance_before_surface_confirmation() {
    for (appearance, color, theme) in [
        (
            crate::settings::Appearance::Light,
            otlyra_platform::ColorScheme::Light,
            crate::widget::theme::Theme::light(),
        ),
        (
            crate::settings::Appearance::Dark,
            otlyra_platform::ColorScheme::Dark,
            crate::widget::theme::Theme::dark(),
        ),
    ] {
        let viewport = Viewport::new(900, 600, 1.0);
        let mut window = WindowBrowser::spawn_native(viewport, true, None, move |text| {
            let mut settings = crate::settings::Settings::default();
            settings.appearance = appearance;
            let mut browser =
                Browser::with_fetcher_and_text(Fetcher::spawn(Properties), settings, text);
            // The blank tab has no origin, so this writes no saved preference.
            browser.set_zoom(1.5);
            browser
        })
        .unwrap();
        assert_eq!(window.window_appearance(), Some(color));
        assert_eq!(window.snapshot.theme, theme);
        assert_eq!(window.snapshot.zoom, 1.5);
        assert_eq!(window.ui.zoom, 1.5);
        assert!(window.scenes.is_empty());
        assert!(window.mailbox.lock().unwrap().latest.is_none());
        window.shutdown().unwrap();
    }
}

#[test]
fn initial_native_environment_events_precede_the_first_script() {
    let viewport = Viewport::new(900, 600, 1.0);
    let mut window = WindowBrowser::spawn_native(viewport, false, None, |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Properties), Default::default(), text);
        browser.navigate("https://environment.test/native-hidden");
        browser
    })
    .unwrap();
    window.on_event(PlatformEvent::AppearanceChanged(
        otlyra_platform::ColorScheme::Dark,
    ));
    window.on_event(PlatformEvent::VisibilityChanged(false));
    let confirmed = Viewport::new(1800, 1200, 2.0);
    window.on_event(PlatformEvent::SurfaceReady(confirmed));
    wait(&mut window, |window| {
        !window.snapshot.labels.is_empty() && !window.snapshot.labels[0].loading
    });
    assert_eq!(property(&window, "visibility"), "hidden");
    assert_eq!(property(&window, "ratio"), "2");
    assert_eq!(window.snapshot.theme, crate::widget::theme::Theme::dark());
    assert!(!window.snapshot.window_visible);
    assert!(window.scenes.is_empty());
}

#[test]
fn native_first_script_waits_for_confirmed_surface_dimensions_and_scale() {
    for scale in [1.0, 2.0] {
        for interface in [false, true] {
            let nominal = Viewport::new(2757, 1983, 3.0);
            let mut window = WindowBrowser::spawn_native(nominal, interface, None, |text| {
                let mut browser = Browser::with_fetcher_and_text(
                    Fetcher::spawn(Properties),
                    Default::default(),
                    text,
                );
                browser.navigate("https://environment.test/native");
                browser
            })
            .unwrap();
            let (tx, rx) = mpsc::channel();
            window
                .sender
                .send(Command::Task(Box::new(move |_| tx.send(()).unwrap())))
                .unwrap();
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            window.receive();
            assert!(window.mailbox.lock().unwrap().latest.is_none());
            assert!(window.scenes.is_empty());
            let confirmed = Viewport::new((919.0 * scale) as u32, (661.0 * scale) as u32, scale);
            window.on_event(PlatformEvent::SurfaceReady(confirmed));
            ready(&mut window);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(property(&window, "width"), "919");
            assert_eq!(
                property(&window, "height"),
                (661.0 - if interface { UI_HEIGHT } else { 0.0 }).to_string()
            );
            assert_eq!(property(&window, "ratio"), scale.to_string());
            assert_eq!(property(&window, "resizes"), "0");
            assert_eq!(window.snapshot.viewport, confirmed);
        }
    }
}

#[test]
fn native_chrome_projects_ordered_tab_input_before_surface_confirmation() {
    let viewport = Viewport::new(900, 600, 1.0);
    let mut window = WindowBrowser::spawn_native(viewport, true, None, |text| {
        Browser::with_fetcher_and_text(Fetcher::spawn(Properties), Default::default(), text)
    })
    .unwrap();
    key(&mut window, 't');
    let kept = window.tabs[window.active].id;
    key(&mut window, 't');
    key(&mut window, 'w');
    assert_eq!(window.tabs.len(), 2);
    assert_eq!(window.tabs[window.active].id, kept);
    assert_eq!(window.pending.len(), 3);
    assert!(window.scenes.is_empty());
    window.on_event(PlatformEvent::SurfaceReady(viewport));
    let serial = window.serial;
    wait(&mut window, |window| {
        window.snapshot.serial >= serial && window.pending.is_empty()
    });
    assert_eq!(window.tabs.len(), 2);
    assert_eq!(window.tabs[window.active].id, kept);
}

#[test]
fn native_close_before_surface_confirmation_discards_waiting_page_work() {
    let viewport = Viewport::new(900, 600, 1.0);
    let mut window = WindowBrowser::spawn_native(viewport, true, None, |text| {
        Browser::with_fetcher_and_text(Fetcher::spawn(Properties), Default::default(), text)
    })
    .unwrap();
    let (tx, rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| tx.send(()).unwrap())))
        .unwrap();
    window.shutdown().unwrap();
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    assert!(window.mailbox.lock().unwrap().latest.is_none());
}
