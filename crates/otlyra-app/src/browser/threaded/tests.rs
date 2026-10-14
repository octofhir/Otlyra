use super::*;
use crate::fetcher::{Fetcher, Loaded, Loader};
use otlyra_platform::FramePump;
use std::time::Duration;

const PAGE: &str = r#"<!doctype html><title>Page</title><body style="margin:0;background:#fff">
<input id=field value=hello style="position:absolute;left:40px;top:40px;width:240px;height:32px">
<p id=result style="margin-top:150px">before callback</p>"#;

struct Pages;
impl Loader for Pages {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            bytes: PAGE.as_bytes().to_vec(),
            content_type: Some("text/html".into()),
            final_url: url.into(),
            ..Default::default()
        })
    }
}

fn window(viewport: Viewport, url: &'static str) -> WindowBrowser {
    WindowBrowser::spawn(viewport, true, move |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Pages), Default::default(), text);
        browser.navigate(url);
        browser
    })
    .unwrap()
}

fn wait(window: &mut WindowBrowser, predicate: impl Fn(&WindowBrowser) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        window.receive();
        if predicate(window) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "content owner did not make progress"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn ready(window: &mut WindowBrowser) {
    wait(window, |window| {
        !window.scenes.is_empty() && !window.snapshot.labels.iter().any(|label| label.loading)
    });
}

fn key(window: &mut WindowBrowser, character: char) {
    window.on_event(PlatformEvent::KeyPressed {
        key: Key::Character(character),
        modifiers: Modifiers {
            command: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Default::default()
        },
    });
}

struct TimedScript {
    inner: Box<dyn otlyra_html::ScriptRunner>,
    started: mpsc::Sender<()>,
    finished: mpsc::Sender<Duration>,
}
impl otlyra_html::ScriptRunner for TimedScript {
    fn run(&mut self, source: &str, node: otlyra_dom::NodeId, doc: &mut otlyra_dom::Document) {
        self.inner.run(source, node, doc);
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.inner.next_deadline()
    }
    fn run_due_timers(&mut self, doc: &mut otlyra_dom::Document) -> usize {
        let due = self.next_deadline().is_some_and(|at| at <= Instant::now());
        let start = Instant::now();
        if due {
            self.started.send(()).unwrap();
        }
        let count = self.inner.run_due_timers(doc);
        if due {
            self.finished.send(start.elapsed()).unwrap();
        }
        count
    }
    fn take_mutated(&mut self) -> bool {
        self.inner.take_mutated()
    }
}

#[test]
fn slow_javascript_does_not_hold_chrome_or_tab_input() {
    for scale in [1.0, 2.0] {
        let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
        let mut window = window(viewport, "https://page.example/");
        ready(&mut window);
        let mut pump = FramePump::new(viewport);
        pump.frame(&mut window).unwrap();
        let original = window.tabs[0].id;
        let (started, start_rx) = mpsc::channel();
        let (finished, finish_rx) = mpsc::channel();
        window.sender.send(Command::Task(Box::new(move |browser| {
            let html = format!("{PAGE}<script>setTimeout(() => {{ const start = Date.now(); while (Date.now() - start < 500) {{}} document.getElementById('result').textContent = 'after callback'; }}, 150);</script>");
            let (parsed, runner) = otlyra_html::parse_with_scripts(html.as_bytes(), None,
                Some(Box::new(otlyra_script::PageScripts::new("https://page.example/"))), Default::default());
            let tab = browser.active_id();
            browser.world.set_page(tab, Some(crate::page::PageScene::at(parsed.document, url::Url::parse("https://page.example/").unwrap())));
            browser.world.set_scripts(tab, Box::new(TimedScript { inner: runner.unwrap(), started, finished }));
        }))).unwrap();
        start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let begin = Instant::now();
        key(&mut window, 'l');
        for character in "typed while JavaScript is busy".chars() {
            window.on_event(PlatformEvent::TextInput(character));
        }
        assert_eq!(window.ui.address.text(), "typed while JavaScript is busy");
        let mut input_elapsed = begin.elapsed();
        pump.frame(&mut window).unwrap();
        let tab_input_started = Instant::now();
        key(&mut window, 't');
        let new = window.tabs[1].id;
        assert_ne!(new, original);
        assert_eq!(window.active, 1);
        window.select(0);
        window.apply(UiAction::CloseTab(1));
        input_elapsed += tab_input_started.elapsed();
        pump.frame(&mut window).unwrap();
        assert_eq!(window.tabs.len(), 1);
        assert_eq!(window.tabs[0].id, original);
        let ui_elapsed = begin.elapsed();
        assert!(
            ui_elapsed < Duration::from_millis(200),
            "UI waited for page JavaScript: {:?}",
            begin.elapsed()
        );
        assert!(
            finish_rx.try_recv().is_err(),
            "the callback must still be running while UI answers"
        );
        let duration = finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        println!(
            "content responsiveness scale={scale}: callback_ms={:.2}, ui_events_and_two_cpu_frames_ms={:.2}, input_batch_ms={:.2}",
            duration.as_secs_f64() * 1000.0,
            ui_elapsed.as_secs_f64() * 1000.0,
            input_elapsed.as_secs_f64() * 1000.0
        );
        assert!(
            duration >= Duration::from_millis(450),
            "the real JavaScript loop did not run: {duration:?}"
        );
        let serial = window.serial;
        wait(&mut window, |window| window.snapshot.serial >= serial);
        assert_eq!(window.tabs.len(), 1);
        assert_eq!(window.tabs[0].id, original);
        let (sender, receiver) = mpsc::channel();
        window
            .sender
            .send(Command::Task(Box::new(move |browser| {
                sender
                    .send(otlyra_dom::dump::serialize(
                        browser.active_page().unwrap().document(),
                    ))
                    .unwrap();
            })))
            .unwrap();
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .contains("after callback")
        );
    }
}

#[test]
fn threaded_about_scene_matches_the_synchronous_compositor() {
    for scale in [1.0, 2.0] {
        let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
        let mut window = window(viewport, "about:about");
        ready(&mut window);
        let mut local = Browser::new(Pages);
        local.navigate("about:about");
        let mut threaded = FramePump::new(viewport);
        let mut synchronous = FramePump::new(viewport);
        threaded.frame(&mut window).unwrap();
        synchronous.frame(&mut local).unwrap();
        assert_eq!(
            threaded.pixels().unwrap(),
            synchronous.pixels().unwrap(),
            "scale={scale}"
        );
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::write(
            directory.join(format!("content-owner-about-{scale}x.png")),
            threaded.png().unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn page_input_find_and_inspector_survive_the_owner_boundary() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    window.compose(viewport);
    window.on_event(PlatformEvent::PointerMoved {
        x: 80.0,
        y: UI_HEIGHT + 56.0,
    });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    window.on_event(PlatformEvent::PointerReleased);
    key(&mut window, 'a');
    for character in "page text".chars() {
        window.on_event(PlatformEvent::TextInput(character));
    }
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(
        window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("page text"))
    );
    key(&mut window, 'f');
    window.compose(viewport);
    for character in "callback".chars() {
        window.on_event(PlatformEvent::TextInput(character));
    }
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(window.ui.finding());
    assert!(window.ui.find_focused());
    assert_eq!(window.ui.find.text(), "callback");
    assert_eq!(window.ui.find_status.total, 1);
    window.apply(UiAction::ToggleInspector);
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(
        window
            .compose(viewport)
            .unwrap()
            .layers
            .iter()
            .any(|layer| layer.id == LayerId(super::super::frame::LAYER_INSPECTOR))
    );
}

#[test]
fn resize_requests_coalesce_while_content_is_busy_and_close_does_not_join() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "about:about");
    ready(&mut window);
    let (started, start_rx) = mpsc::channel();
    let (release, wait_rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| {
            started.send(()).unwrap();
            wait_rx.recv().unwrap();
        })))
        .unwrap();
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    for width in 901..1000 {
        window.on_event(PlatformEvent::Resized(Viewport::new(width, 650, 2.0)));
    }
    assert!(window.mailbox.lock().unwrap().frame_pending);
    let begin = Instant::now();
    drop(window);
    assert!(begin.elapsed() < Duration::from_millis(100));
    release.send(()).unwrap();
}

struct NativeClipboard {
    owner: std::thread::ThreadId,
    text: Arc<Mutex<Option<String>>>,
}
impl Clipboard for NativeClipboard {
    fn read(&mut self) -> Option<String> {
        assert_eq!(std::thread::current().id(), self.owner);
        self.text.lock().unwrap().clone()
    }
    fn write(&mut self, text: String) {
        assert_eq!(std::thread::current().id(), self.owner);
        *self.text.lock().unwrap() = Some(text);
    }
}

#[test]
fn copy_and_paste_use_the_native_clipboard_only_on_ui() {
    let mut window = window(Viewport::new(900, 640, 1.0), "https://page.example/");
    ready(&mut window);
    let text = Arc::new(Mutex::new(None));
    window.set_clipboard(Box::new(NativeClipboard {
        owner: std::thread::current().id(),
        text: Arc::clone(&text),
    }));
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            browser.clipboard.write("from content".into());
        })))
        .unwrap();
    wait(&mut window, |_| {
        text.lock().unwrap().as_deref() == Some("from content")
    });
    window.on_event(PlatformEvent::PointerMoved {
        x: 80.0,
        y: UI_HEIGHT + 56.0,
    });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    window.on_event(PlatformEvent::PointerReleased);
    key(&mut window, 'a');
    key(&mut window, 'v');
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(
        window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("from content"))
    );
    key(&mut window, 'l');
    window.compose(Viewport::new(900, 640, 1.0));
    key(&mut window, 'c');
    assert_eq!(
        text.lock().unwrap().as_deref(),
        Some("https://page.example/")
    );
}

#[test]
fn consecutive_pointer_moves_coalesce_without_crossing_key_events() {
    let mut window = window(Viewport::new(900, 640, 1.0), "https://page.example/");
    ready(&mut window);
    let (started, start_rx) = mpsc::channel();
    let (release, wait_rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| {
            started.send(()).unwrap();
            wait_rx.recv().unwrap();
        })))
        .unwrap();
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    for x in 1..1000 {
        window.on_event(PlatformEvent::PointerMoved {
            x: f64::from(x),
            y: 200.0,
        });
    }
    let slot = Arc::clone(window.pointer_slot.as_ref().unwrap());
    assert!(matches!(
        slot.lock().unwrap().as_ref().unwrap().event,
        Some(PlatformEvent::PointerMoved { x: 999.0, .. })
    ));
    window.on_event(PlatformEvent::KeyPressed {
        key: Key::PageDown,
        modifiers: Default::default(),
    });
    window.on_event(PlatformEvent::PointerMoved {
        x: 1000.0,
        y: 200.0,
    });
    assert!(!Arc::ptr_eq(&slot, window.pointer_slot.as_ref().unwrap()));
    release.send(()).unwrap();
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
}

#[test]
fn queued_input_does_not_reach_a_replaced_document() {
    let mut window = window(Viewport::new(900, 640, 1.0), "https://page.example/");
    ready(&mut window);
    let (started, start_rx) = mpsc::channel();
    let (release, wait_rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            started.send(()).unwrap();
            wait_rx.recv().unwrap();
            browser.navigate("https://replacement.example/");
        })))
        .unwrap();
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    window.on_event(PlatformEvent::PointerMoved {
        x: 80.0,
        y: UI_HEIGHT + 56.0,
    });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    window.on_event(PlatformEvent::PointerReleased);
    key(&mut window, 'a');
    for character in "stale input".chars() {
        window.on_event(PlatformEvent::TextInput(character));
    }
    let serial = window.serial;
    release.send(()).unwrap();
    wait(&mut window, |window| {
        window.snapshot.serial >= serial
            && window.tabs[window.active].url == "https://replacement.example/"
            && !window.snapshot.labels.iter().any(|label| label.loading)
    });
    assert!(
        window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("hello"))
    );
    assert!(
        !window
            .snapshot
            .document
            .nodes
            .iter()
            .any(|(_, node)| node.value() == Some("stale input"))
    );
}

#[test]
fn a_page_pointer_move_with_no_visible_change_does_not_request_a_frame() {
    let mut window = window(Viewport::new(900, 640, 1.0), "https://page.example/");
    ready(&mut window);
    window.compose(Viewport::new(900, 640, 1.0));
    window.handle_event(PlatformEvent::PointerMoved { x: 800.0, y: 500.0 });
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    window.handle_event(PlatformEvent::Woken);
    assert_eq!(
        window.handle_event(PlatformEvent::PointerMoved { x: 801.0, y: 500.0 }),
        FrameRequest::None
    );
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert_eq!(
        window.handle_event(PlatformEvent::Woken),
        FrameRequest::None
    );
}

#[test]
fn synchronous_fields_share_cut_copy_paste_and_preserve_multiline_utf8() {
    let parsed = otlyra_html::parse(
        b"<textarea id=text>hello</textarea><input id=readonly readonly value=kept>",
        None,
    );
    let field = otlyra_css::stylo_dom::select(&parsed.document, "#text").unwrap()[0];
    let readonly = otlyra_css::stylo_dom::select(&parsed.document, "#readonly").unwrap()[0];
    let mut browser = Browser::new(Pages);
    browser.world.set_page(
        browser.active_id(),
        Some(crate::page::PageScene::new(parsed.document)),
    );
    browser.compose_scene(Viewport::new(900, 640, 1.0));
    browser.activate_surface(super::super::SURFACE_PAGE);
    browser.active_page_mut().unwrap().focus_node(field);
    browser.clipboard.write("Привет\r\nмир\t!".into());
    let press = |browser: &mut Browser, character| {
        browser.on_event(PlatformEvent::KeyPressed {
            key: Key::Character(character),
            modifiers: Modifiers {
                command: cfg!(target_os = "macos"),
                control: !cfg!(target_os = "macos"),
                ..Default::default()
            },
        })
    };
    press(&mut browser, 'a');
    press(&mut browser, 'v');
    assert_eq!(
        browser
            .active_page()
            .unwrap()
            .control_facts(field)
            .unwrap()
            .value
            .as_deref(),
        Some("Привет\nмир\t!")
    );
    press(&mut browser, 'a');
    press(&mut browser, 'c');
    assert_eq!(browser.clipboard.read().as_deref(), Some("Привет\nмир\t!"));
    press(&mut browser, 'x');
    assert_eq!(
        browser
            .active_page()
            .unwrap()
            .control_facts(field)
            .unwrap()
            .value
            .as_deref(),
        Some("")
    );
    press(&mut browser, 'v');
    assert_eq!(
        browser
            .active_page()
            .unwrap()
            .control_facts(field)
            .unwrap()
            .value
            .as_deref(),
        Some("Привет\nмир\t!")
    );
    browser.active_page_mut().unwrap().focus_node(readonly);
    press(&mut browser, 'v');
    assert_eq!(
        browser
            .active_page()
            .unwrap()
            .control_facts(readonly)
            .unwrap()
            .value
            .as_deref(),
        Some("kept")
    );
}

#[test]
fn address_and_find_claim_text_before_their_first_frame() {
    let mut ui = BrowserUi::new();
    ui.focus_address();
    assert!(ui.text_input('a'));
    ui.open_find();
    assert!(ui.text_input('f'));
    assert_eq!(ui.address.text(), "a");
    assert_eq!(ui.find.text(), "f");
    ui.focus_address();
    assert!(ui.text_input('b'));
    assert_eq!(ui.address.text(), "b");
    assert_eq!(ui.find.text(), "f");
    ui.blur();
    assert!(!ui.text_input('x'));
}

#[test]
fn file_answers_are_rejected_after_navigation() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    let (sender, receiver) = mpsc::channel();
    window.sender.send(Command::Task(Box::new(move |browser| {
        let parsed = otlyra_html::parse(b"<input type=file style='position:absolute;left:40px;top:40px;width:240px;height:32px'>", None);
        let node = otlyra_css::stylo_dom::select(&parsed.document, "input").unwrap()[0];
        let tab = browser.active_id();
        browser.world.set_page(tab, Some(crate::page::PageScene::new(parsed.document)));
        browser.compose_content_scene(viewport);
        browser.on_event(PlatformEvent::PointerMoved { x: 80.0, y: UI_HEIGHT + 56.0 });
        browser.on_event(PlatformEvent::PointerPressed { clicks: 1 });
        browser.on_event(PlatformEvent::PointerReleased);
        sender.send(node).unwrap();
    }))).unwrap();
    let node = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    let NativeRequest::Files {
        tab,
        navigation,
        request,
    } = window.native.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("file picker was not forwarded to UI");
    };
    assert_eq!(request.node, node);
    let path = std::env::temp_dir().join(format!("otlyra-content-file-{}.txt", std::process::id()));
    std::fs::write(&path, b"chosen bytes").unwrap();
    let (sender, receiver) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            browser.tabs[browser.active].navigation =
                Some(super::super::tabs::next_navigation_id());
            sender
                .send(std::thread::current().name().map(str::to_owned))
                .unwrap();
        })))
        .unwrap();
    assert_eq!(
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .as_deref(),
        Some("otlyra-content")
    );
    window
        .sender
        .send(Command::Files {
            tab,
            navigation,
            request,
            paths: vec![path.clone()],
        })
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            // Reusing a node number in another document cannot install old files.
            let page = browser.active_page_mut().unwrap();
            page.focus_node(node);
            sender
                .send(page.focused_value().unwrap_or_default().to_owned())
                .unwrap();
        })))
        .unwrap();
    assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), "");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn closing_a_content_menu_clears_its_owner_state() {
    let mut window = window(Viewport::new(900, 640, 1.0), "https://page.example/");
    ready(&mut window);
    window.compose(Viewport::new(900, 640, 1.0));
    window.on_event(PlatformEvent::PointerMoved {
        x: 100.0,
        y: UI_HEIGHT + 180.0,
    });
    window.on_event(PlatformEvent::ContextMenuRequested);
    wait(&mut window, |window| window.ui.context_menu().is_some());
    window.on_event(PlatformEvent::KeyPressed {
        key: Key::Escape,
        modifiers: Default::default(),
    });
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(window.ui.context_menu().is_none());
    let (sender, receiver) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            sender
                .send(browser.ui.context_menu().is_none() && browser.context_target.is_none())
                .unwrap();
        })))
        .unwrap();
    assert!(receiver.recv_timeout(Duration::from_secs(5)).unwrap());
}

#[test]
fn a_queued_page_release_cannot_capture_the_next_chrome_press() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    window.compose(viewport);
    window.on_event(PlatformEvent::PointerMoved {
        x: 80.0,
        y: UI_HEIGHT + 56.0,
    });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    let (started, start_rx) = mpsc::channel();
    let (release, wait_rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| {
            started.send(()).unwrap();
            wait_rx.recv().unwrap();
        })))
        .unwrap();
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    window.on_event(PlatformEvent::PointerReleased);
    window.on_event(PlatformEvent::PointerMoved { x: 300.0, y: 57.0 });
    window.on_event(PlatformEvent::PointerPressed { clicks: 1 });
    window.on_event(PlatformEvent::PointerReleased);
    key(&mut window, 'a');
    for character in "independent chrome".chars() {
        window.on_event(PlatformEvent::TextInput(character));
    }
    assert!(window.chrome_focused);
    assert_eq!(window.ui.address.text(), "independent chrome");
    release.send(()).unwrap();
}

#[test]
fn unchanged_content_is_rescaled_once_while_the_owner_is_busy() {
    let mut window = window(Viewport::new(900, 640, 1.0), "about:about");
    ready(&mut window);
    let (started, start_rx) = mpsc::channel();
    let (release, wait_rx) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| {
            started.send(()).unwrap();
            wait_rx.recv().unwrap();
        })))
        .unwrap();
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let viewport = Viewport::new(1800, 1280, 2.0);
    let first = window.compose(viewport).unwrap();
    let second = window.compose(viewport).unwrap();
    assert!(Arc::ptr_eq(&first.layers[0].list, &second.layers[0].list));
    assert_eq!(first.layers[0].rect, second.layers[0].rect);
    release.send(()).unwrap();
}

#[test]
fn inspector_search_keeps_its_keyboard_shortcut() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    window.compose(viewport);
    window.apply(UiAction::ToggleInspector);
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    key(&mut window, 'f');
    for character in "needle".chars() {
        window.on_event(PlatformEvent::TextInput(character));
    }
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(!window.ui.finding());
    let (sender, receiver) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            sender
                .send(browser.inspector.search.text().to_owned())
                .unwrap();
        })))
        .unwrap();
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        "needle"
    );
    key(&mut window, 't');
    assert_eq!(window.tabs.len(), 2);
    assert_eq!(window.active, 1);
}
