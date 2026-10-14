use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const HTML: &str = r#"<!doctype html><title>Deferred</title><body style="margin:0;background:white">
<input id=field value=before style="position:absolute;left:40px;top:40px;width:240px;height:32px">
<p id=state></p><script>
const state = document.getElementById('state');
let updates = 0;
function update() {
  const field = document.getElementById('field');
  field.setAttribute('style', 'position:absolute;left:340px;top:40px;width:240px;height:32px');
  field.setAttribute('value', 'after timer');
  state.setAttribute('updates', String(++updates));
  if (updates < 4) setTimeout(update, 0);
}
document.addEventListener('visibilitychange', () => { if (document.hidden) setTimeout(update, 0); });
</script>"#;

struct Deferred;
impl Loader for Deferred {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            bytes: HTML.as_bytes().to_vec(),
            content_type: Some("text/html".into()),
            final_url: url.into(),
            ..Default::default()
        })
    }
}

fn deferred_window(viewport: Viewport, visible: bool) -> WindowBrowser {
    WindowBrowser::spawn(viewport, true, move |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Deferred), Default::default(), text);
        browser.on_event(PlatformEvent::VisibilityChanged(visible));
        browser.navigate("https://deferred.test/");
        browser
    })
    .unwrap()
}

fn inspect<R: Send + 'static>(
    window: &WindowBrowser,
    read: impl FnOnce(&Browser) -> R + Send + 'static,
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

fn hidden_updates(window: &WindowBrowser) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while inspect(window, |browser| {
        crate::browser::visibility_tests::attr(browser, browser.active_id(), "updates")
    }) != "4"
    {
        assert!(Instant::now() < deadline, "hidden timers did not progress");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn hidden_timers_change_dom_without_preparing_scenes_or_accessibility() {
    for scale in [1.0, 2.0] {
        let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
        let mut window = deferred_window(viewport, true);
        ready(&mut window);
        let tab = window.tabs[0].id;
        let before = inspect(&window, |browser| browser.active_page().unwrap().builds());
        let document = Arc::clone(&window.snapshot.document);
        let list = Arc::clone(&window.scenes[&tab].1.layers[0].list);
        let mut pump = FramePump::new(viewport);
        pump.frame(&mut window).unwrap();
        window.on_event(PlatformEvent::VisibilityChanged(false));
        hidden_updates(&window);
        wait(&mut window, |window| !window.snapshot.window_visible);
        for _ in 0..12 {
            inspect(&window, |_| ());
        }
        window.receive();
        assert_eq!(
            inspect(&window, |browser| browser.active_page().unwrap().builds()),
            before
        );
        assert!(Arc::ptr_eq(&document, &window.snapshot.document));
        assert!(Arc::ptr_eq(&list, &window.scenes[&tab].1.layers[0].list));
        assert!(window.snapshot.scene.is_none());

        window.on_event(PlatformEvent::VisibilityChanged(true));
        wait(&mut window, |window| {
            window.snapshot.window_visible
                && window
                    .snapshot
                    .document
                    .nodes
                    .iter()
                    .any(|(_, node)| node.value() == Some("after timer"))
        });
        assert!(
            window.scenes[&tab]
                .1
                .layers
                .iter()
                .all(|layer| layer.dirty.is_none())
        );
        let _resume = pause_owner(&window);
        pump.frame(&mut window).unwrap();
        assert_full_pixels(&mut window, &mut pump, viewport);
    }
}

#[test]
fn a_hidden_initial_load_stays_unprepared_until_shown_at_the_latest_viewport() {
    let initial = Viewport::new(800, 600, 1.0);
    let resized = Viewport::new(1800, 1280, 2.0);
    let mut window = deferred_window(initial, false);
    wait(&mut window, |window| {
        !window.snapshot.labels.is_empty()
            && !window.snapshot.labels[0].loading
            && window.snapshot.tabs[0].navigation.is_some()
    });
    assert!(window.scenes.is_empty());
    assert!(!window.first_content_received);
    assert_eq!(
        inspect(&window, |browser| browser.active_page().unwrap().builds()),
        0
    );
    window.request_frame(resized);
    inspect(&window, |_| ());
    window.receive();
    assert!(window.scenes.is_empty());
    assert_eq!(
        inspect(&window, |browser| browser.active_page().unwrap().builds()),
        0
    );
    window.on_event(PlatformEvent::VisibilityChanged(true));
    ready(&mut window);
    assert_eq!(window.scenes[&window.tabs[0].id].0, resized);
    assert!(window.first_content_received);
    assert!(
        window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("before"))
    );
}

#[test]
fn restore_and_pointer_input_in_one_batch_use_updated_layout() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = deferred_window(viewport, true);
    ready(&mut window);
    window.compose(viewport);
    window.on_event(PlatformEvent::VisibilityChanged(false));
    hidden_updates(&window);
    wait(&mut window, |window| !window.snapshot.window_visible);
    let resume = pause_owner(&window);
    window.on_event(PlatformEvent::VisibilityChanged(true));
    window.on_event(PlatformEvent::PointerMoved {
        x: 380.0,
        y: UI_HEIGHT + 56.0,
    });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    window.on_event(PlatformEvent::PointerReleased);
    key(&mut window, 'a');
    for character in "fresh coordinates".chars() {
        window.on_event(PlatformEvent::TextInput(character));
    }
    let serial = window.serial;
    drop(resume);
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(
        window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("fresh coordinates"))
    );
}

#[test]
fn hidden_navigation_discards_retired_accessibility_and_scene_until_restore() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = deferred_window(viewport, true);
    ready(&mut window);
    let tab = window.tabs[0].id;
    let list = Arc::clone(&window.scenes[&tab].1.layers[0].list);
    let generation = window.tabs[0].navigation;
    window.on_event(PlatformEvent::VisibilityChanged(false));
    window.apply(UiAction::Navigate("https://deferred.test/next".into()));
    wait(&mut window, |window| {
        window.tabs[0].navigation != generation && !window.tabs[0].loading
    });
    assert!(!window.scenes.contains_key(&tab));
    assert!(
        !window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value().is_some())
    );
    assert_eq!(
        inspect(&window, |browser| browser.active_page().unwrap().builds()),
        0
    );
    window.on_event(PlatformEvent::VisibilityChanged(true));
    wait(&mut window, |window| window.snapshot.window_visible);
    assert!(!Arc::ptr_eq(&list, &window.scenes[&tab].1.layers[0].list));
    assert!(
        window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("before"))
    );
}

#[test]
fn hidden_metadata_cannot_lose_damage_from_an_unconsumed_visible_scene() {
    for scale in [1.0, 2.0] {
        let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
        let mut window = fields_window(viewport);
        let mut pump = FramePump::new(viewport);
        pump.frame(&mut window).unwrap();
        edit_field(&window, "#left", "unconsumed visible edit");
        wait_pending_field(&window, "unconsumed visible edit");
        // Publish hidden metadata over the edited scene before the UI receives it.
        window.window_visible = false;
        window.sender.send(Command::Visibility(false)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while window
            .mailbox
            .lock()
            .unwrap()
            .latest
            .as_ref()
            .is_none_or(|snapshot| snapshot.window_visible)
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        window.receive();
        window.on_event(PlatformEvent::VisibilityChanged(true));
        wait(&mut window, |window| window.snapshot.window_visible);
        assert!(
            window.scenes[&window.tabs[0].id]
                .1
                .layers
                .iter()
                .all(|layer| layer.dirty.is_none())
        );
        let _resume = pause_owner(&window);
        pump.frame(&mut window).unwrap();
        assert_full_pixels(&mut window, &mut pump, viewport);
    }
}

#[test]
fn unchanged_hidden_work_does_not_publish_or_wake_native_ui() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = deferred_window(viewport, true);
    ready(&mut window);
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&wakes);
    window.set_waker(Waker::from_callback(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    window.on_event(PlatformEvent::VisibilityChanged(false));
    hidden_updates(&window);
    wait(&mut window, |window| !window.snapshot.window_visible);
    let publication = Arc::clone(&window.snapshot.metadata);
    let before = wakes.load(Ordering::SeqCst);
    for _ in 0..100 {
        inspect(&window, |_| ());
    }
    let resume = pause_owner(&window);
    assert_eq!(wakes.load(Ordering::SeqCst), before);
    assert!(window.mailbox.lock().unwrap().latest.is_none());
    window.receive();
    assert!(Arc::ptr_eq(&publication, &window.snapshot.metadata));
    drop(resume);
    window.on_event(PlatformEvent::VisibilityChanged(true));
    wait(&mut window, |window| window.snapshot.window_visible);
    assert!(wakes.load(Ordering::SeqCst) > before);
}

#[test]
fn hidden_metadata_changes_and_command_acknowledgements_still_publish() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = deferred_window(viewport, true);
    ready(&mut window);
    window.on_event(PlatformEvent::VisibilityChanged(false));
    hidden_updates(&window);
    wait(&mut window, |window| !window.snapshot.window_visible);
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            browser.tabs[browser.active].title = "background title".into();
        })))
        .unwrap();
    wait(&mut window, |window| {
        window.tabs[0].title == "background title"
    });
    window.send(None, UiAction::None, None, false, None);
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial == serial);
    let resized = Viewport::new(1800, 1280, 2.0);
    window.request_frame(resized);
    wait(&mut window, |window| window.snapshot.viewport == resized);
    window.apply(UiAction::NewTab);
    let serial = window.serial;
    wait(&mut window, |window| {
        window.snapshot.serial == serial && window.pending.is_empty()
    });
    assert_eq!(window.tabs.len(), 2);
    assert_eq!(window.active, 1);
    assert!(!window.snapshot.window_visible);
}

#[test]
fn hidden_context_menu_payload_changes_publish_without_repeating_unchanged_wakes() {
    let mut window = deferred_window(Viewport::new(900, 640, 1.0), true);
    ready(&mut window);
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&wakes);
    window.set_waker(Waker::from_callback(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    window.on_event(PlatformEvent::VisibilityChanged(false));
    hidden_updates(&window);
    wait(&mut window, |window| !window.snapshot.window_visible);
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            browser.ui.open_context_menu(
                300.0,
                170.0,
                vec![crate::ui::ContextRow::Command(
                    crate::ui::ContextCommand::CopySelection,
                    true,
                )],
            );
        })))
        .unwrap();
    wait(&mut window, |window| window.snapshot.context.is_some());
    let before = wakes.load(Ordering::SeqCst);
    for _ in 0..100 {
        inspect(&window, |_| ());
    }
    let resume = pause_owner(&window);
    assert_eq!(wakes.load(Ordering::SeqCst), before);
    drop(resume);
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            browser.ui.open_context_menu(
                300.0,
                170.0,
                vec![crate::ui::ContextRow::Command(
                    crate::ui::ContextCommand::CopySelection,
                    false,
                )],
            );
        })))
        .unwrap();
    wait(&mut window, |window| {
        window.snapshot.context.as_ref().is_some_and(|(_, rows)| {
            rows == &[crate::ui::ContextRow::Command(
                crate::ui::ContextCommand::CopySelection,
                false,
            )]
        })
    });
    assert!(wakes.load(Ordering::SeqCst) > before);
}
