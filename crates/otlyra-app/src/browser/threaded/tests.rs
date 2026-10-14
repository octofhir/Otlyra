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

struct Fields;
impl Loader for Fields {
    fn load(&self, url: &str) -> Result<Loaded, String> {
        Ok(Loaded {
            bytes: br#"<!doctype html><title>Fields</title><body style="margin:0;background:white">
<input id=left value=left style="position:absolute;left:40px;top:40px;width:240px;height:32px">
<button id=right style="position:absolute;left:400px;top:200px;width:240px;height:32px">remote button</button>"#.to_vec(),
            content_type: Some("text/html".into()),
            final_url: url.into(),
            ..Default::default()
        })
    }
}

fn fields_window(viewport: Viewport) -> WindowBrowser {
    let mut window = WindowBrowser::spawn(viewport, true, move |text| {
        let mut browser =
            Browser::with_fetcher_and_text(Fetcher::spawn(Fields), Default::default(), text);
        browser.navigate("https://fields.example/");
        browser
    })
    .unwrap();
    ready(&mut window);
    // Focusing a control may change styles/layout. Establish that state before
    // measuring subsequent field-only edits.
    edit_field(&window, "#left", "prepared");
    wait_pending_field(&window, "prepared");
    window.receive();
    window
}

fn edit_field(window: &WindowBrowser, selector: &'static str, text: &'static str) {
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            let page = browser.active_page_mut().unwrap();
            let node = otlyra_css::stylo_dom::select(page.document(), selector).unwrap()[0];
            page.focus_node(node);
            page.select_all();
            page.typed(text);
            browser.activate_surface(super::super::SURFACE_PAGE);
            browser.accessibility_dirty = true;
        })))
        .unwrap();
}

fn wait_pending_field(window: &WindowBrowser, value: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if window
            .mailbox
            .lock()
            .unwrap()
            .latest
            .as_ref()
            .is_some_and(|snapshot| {
                snapshot
                    .document
                    .nodes
                    .iter()
                    .any(|(_, node)| node.value() == Some(value))
            })
        {
            return;
        }
        assert!(Instant::now() < deadline, "field update was not published");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_pending_epoch(window: &WindowBrowser, previous: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if window
            .mailbox
            .lock()
            .unwrap()
            .latest
            .as_ref()
            .is_some_and(|snapshot| snapshot.scene.layers[0].epoch != previous)
        {
            return;
        }
        assert!(Instant::now() < deadline, "scene update was not published");
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct ResumeOwner(mpsc::Sender<()>);
impl Drop for ResumeOwner {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

fn pause_owner(window: &WindowBrowser) -> ResumeOwner {
    let (started, start_rx) = mpsc::channel();
    let (resume, gate) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |_| {
            started.send(()).unwrap();
            let _ = gate.recv_timeout(Duration::from_secs(10));
        })))
        .unwrap();
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    ResumeOwner(resume)
}

fn assert_full_pixels(window: &mut WindowBrowser, pump: &mut FramePump, viewport: Viewport) {
    let actual = pump.pixels().unwrap();
    let mut whole = FramePump::new(viewport);
    whole.frame(window).unwrap();
    assert_eq!(
        actual,
        whole.pixels().unwrap(),
        "partial raster must match a fresh surface"
    );
}

#[test]
fn field_damage_survives_coalesced_publications_at_both_scales() {
    for scale in [1.0, 2.0] {
        let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
        let mut window = fields_window(viewport);
        let mut pump = FramePump::new(viewport);
        pump.frame(&mut window).unwrap();
        edit_field(&window, "#left", "first change");
        wait_pending_field(&window, "first change");
        // UI intentionally does not receive the first changed publication.
        edit_field(&window, "#left", "second change");
        wait_pending_field(&window, "second change");
        let resume = pause_owner(&window);
        let began = Instant::now();
        let damage = pump.frame(&mut window).unwrap();
        let otlyra_platform::Damage::Region(rect) = damage else {
            panic!("expected partial damage: {damage:?}");
        };
        let pixels = u64::from(rect.width) * u64::from(rect.height);
        assert!(pixels < u64::from(viewport.width) * u64::from(viewport.height) / 2);
        eprintln!(
            "coalesced_fields scale={scale} pixels={pixels} frame_ms={:.2}",
            began.elapsed().as_secs_f64() * 1000.0
        );
        assert_full_pixels(&mut window, &mut pump, viewport);
        assert!(
            window.scenes[&window.tabs[window.active].id]
                .1
                .layers
                .iter()
                .all(|layer| layer.dirty.is_some_and(|rect| rect.is_empty()))
        );
        drop(resume);

        edit_field(&window, "#left", "small update");
        wait_pending_field(&window, "small update");
        let _resume = pause_owner(&window);
        let began = Instant::now();
        let damage = pump.frame(&mut window).unwrap();
        let otlyra_platform::Damage::Region(rect) = damage else {
            panic!("expected field damage: {damage:?}");
        };
        let pixels = u64::from(rect.width) * u64::from(rect.height);
        assert!(pixels < u64::from(viewport.width) * u64::from(viewport.height) / 10);
        eprintln!(
            "single_field scale={scale} pixels={pixels} frame_ms={:.2}",
            began.elapsed().as_secs_f64() * 1000.0
        );
        assert_full_pixels(&mut window, &mut pump, viewport);
    }
}

#[test]
fn completion_preserves_newer_damage_and_a_dropped_frame() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    let mut pump = FramePump::new(viewport);
    pump.frame(&mut window).unwrap();
    edit_field(&window, "#left", "in flight");
    wait_pending_field(&window, "in flight");
    window.compose(viewport).unwrap();
    edit_field(&window, "#left", "newer scene");
    wait_pending_field(&window, "newer scene");
    let _resume = pause_owner(&window);
    window.receive();
    window.frame_complete(true);
    let tab = window.tabs[window.active].id;
    assert!(
        window.scenes[&tab].1.layers[0]
            .dirty
            .is_some_and(|rect| !rect.is_empty())
    );
    let submitted = window.compose(viewport).unwrap();
    window.frame_complete(false);
    assert_eq!(
        window.scenes[&tab].1.layers[0].dirty,
        submitted.layers[0].dirty
    );
    pump.frame(&mut window).unwrap();
    assert_full_pixels(&mut window, &mut pump, viewport);
}

#[test]
fn activation_and_scale_changes_keep_full_content_damage() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    let mut pump = FramePump::new(viewport);
    pump.frame(&mut window).unwrap();
    edit_field(&window, "#left", "pending change");
    wait_pending_field(&window, "pending change");
    let _resume = pause_owner(&window);
    window.receive();
    window.presented_content = Some((TabId(u64::MAX), viewport));
    assert!(window.compose(viewport).unwrap().layers[0].dirty.is_none());
    window.presented_content = Some((window.tabs[window.active].id, viewport));
    let scaled = Viewport::new(1800, 1280, 2.0);
    assert!(window.compose(scaled).unwrap().layers[0].dirty.is_none());
    window.frame_complete(true);
    assert!(
        window.scenes[&window.tabs[window.active].id].1.layers[0]
            .dirty
            .is_some_and(|rect| !rect.is_empty()),
        "scaled cached pixels must not acknowledge source damage"
    );
}

#[test]
fn unchanged_publication_preserves_pending_damage_and_unknown_damage_dominates() {
    let area = LayerRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let partial = LayerRect {
        x: 10,
        y: 20,
        width: 8,
        height: 12,
    };
    let scene = |epoch, dirty| Scene {
        layers: vec![SceneLayer {
            id: LayerId(0),
            rect: area,
            epoch,
            list: Arc::new(DisplayList::new()),
            offsets: Vec::new(),
            background: None,
            dirty,
        }],
    };
    let pending = scene(1, Some(partial));
    let mut unchanged = scene(1, None);
    accumulate_damage(&pending, &mut unchanged);
    assert_eq!(unchanged.layers[0].dirty, Some(partial));
    let mut whole = scene(2, None);
    accumulate_damage(&pending, &mut whole);
    let mut newer = scene(3, Some(partial));
    accumulate_damage(&whole, &mut newer);
    assert!(newer.layers[0].dirty.is_none());
}

struct PublishedScene {
    scene: Arc<Scene>,
    whole: bool,
}

impl Painter for PublishedScene {
    fn compose(&mut self, _: Viewport) -> Option<Scene> {
        Some(Scene {
            layers: self
                .scene
                .layers
                .iter()
                .map(|layer| SceneLayer {
                    id: layer.id,
                    rect: layer.rect,
                    epoch: layer.epoch,
                    list: Arc::clone(&layer.list),
                    offsets: layer.offsets.clone(),
                    background: layer.background,
                    dirty: if self.whole { None } else { layer.dirty },
                })
                .collect(),
        })
    }
    fn paint(&mut self, _: &mut dyn PaintTarget, _: Viewport) {
        unreachable!("retained scene")
    }
}

#[test]
fn disjoint_partial_publications_match_full_raster_pixels() {
    use otlyra_gfx::{
        DisplayItem,
        kurbo::{Affine, Rect, Shape},
        peniko::{Brush, Color, Fill},
    };
    let viewport = Viewport::new(32, 16, 1.0);
    let left = LayerRect {
        x: 2,
        y: 2,
        width: 4,
        height: 4,
    };
    let right = LayerRect {
        x: 24,
        y: 8,
        width: 4,
        height: 4,
    };
    let red = Color::from_rgba8(255, 0, 0, 255);
    let blue = Color::from_rgba8(0, 0, 255, 255);
    let green = Color::from_rgba8(0, 255, 0, 255);
    let scene = |epoch, a, b, dirty| {
        let mut list = DisplayList::new();
        for (rect, color) in [(left, a), (right, b)] {
            list.push(DisplayItem::Fill {
                style: Fill::NonZero,
                transform: Affine::IDENTITY,
                brush: Brush::Solid(color),
                brush_transform: None,
                shape: Rect::new(
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.x + rect.width),
                    f64::from(rect.y + rect.height),
                )
                .to_path(0.1),
            });
        }
        Scene {
            layers: vec![SceneLayer {
                id: LayerId(0),
                rect: LayerRect::of_viewport(viewport),
                epoch,
                list: Arc::new(list),
                offsets: Vec::new(),
                background: Some(Color::WHITE),
                dirty,
            }],
        }
    };
    let mut pump = FramePump::new(viewport);
    pump.frame(&mut PublishedScene {
        scene: Arc::new(scene(0, red, red, None)),
        whole: false,
    })
    .unwrap();
    let skipped = scene(1, blue, red, Some(left));
    let mut latest = scene(2, blue, green, Some(right));
    accumulate_damage(&skipped, &mut latest);
    let latest = Arc::new(latest);
    assert_eq!(
        pump.frame(&mut PublishedScene {
            scene: Arc::clone(&latest),
            whole: false
        })
        .unwrap(),
        otlyra_platform::Damage::Region(left.union(&right))
    );
    let mut full = FramePump::new(viewport);
    full.frame(&mut PublishedScene {
        scene: latest,
        whole: true,
    })
    .unwrap();
    assert_eq!(pump.pixels().unwrap(), full.pixels().unwrap());
}

#[test]
fn a_layout_change_dominates_a_coalesced_field_update() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    let mut pump = FramePump::new(viewport);
    pump.frame(&mut window).unwrap();
    edit_field(&window, "#left", "pending field");
    wait_pending_field(&window, "pending field");
    let epoch = window
        .mailbox
        .lock()
        .unwrap()
        .latest
        .as_ref()
        .unwrap()
        .scene
        .layers[0]
        .epoch;
    window
        .sender
        .send(Command::Task(Box::new(|browser| {
            let page = browser.active_page_mut().unwrap();
            let node = otlyra_css::stylo_dom::select(page.document(), "#right").unwrap()[0];
            page.edit(|document| {
                document.set_attr(
                    node,
                    "style",
                    "position:absolute;left:400px;top:400px;width:240px;height:32px",
                )
            });
            browser.accessibility_dirty = true;
        })))
        .unwrap();
    wait_pending_epoch(&window, epoch);
    let _resume = pause_owner(&window);
    window.receive();
    assert!(
        window.scenes[&window.tabs[window.active].id].1.layers[0]
            .dirty
            .is_none()
    );
    pump.frame(&mut window).unwrap();
    assert_full_pixels(&mut window, &mut pump, viewport);
}

#[test]
fn partial_field_raster_matches_full_damage_and_records_cost() {
    let mut samples = Vec::new();
    for scale in [1.0, 2.0] {
        let viewport = Viewport::new((900.0 * scale) as u32, (640.0 * scale) as u32, scale);
        let mut window = fields_window(viewport);
        let initial = Arc::new(window.compose(viewport).unwrap());
        let mut partial = FramePump::new(viewport);
        let mut whole = FramePump::new(viewport);
        for (pump, force) in [(&mut partial, false), (&mut whole, true)] {
            pump.frame(&mut PublishedScene {
                scene: Arc::clone(&initial),
                whole: force,
            })
            .unwrap();
        }
        window.frame_complete(true);
        for index in 0..22 {
            let text = if index % 2 == 0 {
                "sample one"
            } else {
                "sample two"
            };
            edit_field(&window, "#left", text);
            wait_pending_field(&window, text);
            let resume = pause_owner(&window);
            let scene = Arc::new(window.compose(viewport).unwrap());
            let mut elapsed = [0.0; 2];
            let mut areas = [0; 2];
            let order = if index % 2 == 0 { [0, 1] } else { [1, 0] };
            for mode in order {
                let pump = if mode == 0 { &mut partial } else { &mut whole };
                let began = Instant::now();
                let damage = pump
                    .frame(&mut PublishedScene {
                        scene: Arc::clone(&scene),
                        whole: mode == 1,
                    })
                    .unwrap();
                elapsed[mode] = began.elapsed().as_secs_f64() * 1000.0;
                let otlyra_platform::Damage::Region(rect) = damage else {
                    panic!("expected region: {damage:?}");
                };
                areas[mode] = u64::from(rect.width) * u64::from(rect.height);
            }
            assert!(areas[0] < areas[1] / 10);
            assert_eq!(partial.pixels().unwrap(), whole.pixels().unwrap());
            window.frame_complete(true);
            drop(resume);
            if index >= 2 {
                samples.push(serde_json::json!({
                    "scale": scale, "sample": index - 2,
                    "partial_ms": elapsed[0], "full_damage_ms": elapsed[1],
                    "partial_pixels": areas[0], "full_damage_pixels": areas[1],
                }));
            }
        }
    }
    let report = serde_json::json!({
        "schema": 1, "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "logical_width": 900, "logical_height": 640,
        "warmups_per_scale": 2, "sampling": "alternating-order same-scene CPU frame pairs",
        "endpoint": "FramePump compose, raster and damage readback; no native presentation",
        "samples": samples,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/content-damage-cpu-pairs.json");
    std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
}
