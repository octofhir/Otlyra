//! Content execution, publication, and native service replies.

use super::*;

pub(super) struct ContentClipboard {
    pub(super) state: Rc<RefCell<ClipboardState>>,
    pub(super) requests: mpsc::Sender<NativeRequest>,
}

#[derive(Default)]
pub(super) struct ClipboardState {
    text: Option<String>,
    revision: u64,
    acknowledged: u64,
    serial: u64,
}

impl Clipboard for ContentClipboard {
    fn read(&mut self) -> Option<String> {
        self.state.borrow().text.clone()
    }
    fn write(&mut self, text: String) {
        let mut state = self.state.borrow_mut();
        state.text = Some(text.clone());
        state.revision += 1;
        let _ = self.requests.send(NativeRequest::Clipboard {
            text,
            revision: state.revision,
            serial: state.serial,
        });
    }
}

impl Snapshot {
    fn capture(
        browser: &mut Browser,
        viewport: Viewport,
        serial: u64,
        cache: &mut MetadataCache,
    ) -> Self {
        let mut scene = browser.compose_content_scene(viewport);
        browser.update_find();
        scene
            .layers
            .retain(|layer| layer.id != LayerId(LAYER_CHROME));
        // An unconsumed intermediate snapshot may have painted a different
        // region. Full damage is safe even when that snapshot is overwritten.
        for layer in &mut scene.layers {
            layer.dirty = None;
        }
        let tab = &browser.tabs[browser.active];
        if browser.accessibility_dirty || cache.tab != Some(tab.id) {
            cache.document = Arc::new(browser.world.page(tab.id).map_or_else(
                || crate::a11y::empty_tree(&tab.title),
                |page| crate::a11y::tree_for(page, &tab.title),
            ));
            cache.tab = Some(tab.id);
            browser.accessibility_dirty = false;
        }
        let (focused, described) = match tab.system {
            Some(SystemPage::Settings) => (browser.settings.focused(), browser.settings.describe()),
            Some(SystemPage::History) => (
                browser.history_page.focused(),
                browser.history_page.describe(),
            ),
            Some(SystemPage::Downloads) => (
                browser.downloads_page.focused(),
                browser.downloads_page.describe(),
            ),
            Some(SystemPage::Bookmarks) => (
                browser.bookmarks_page.focused(),
                browser.bookmarks_page.describe(),
            ),
            Some(SystemPage::Cookies) => (
                browser.cookies_page.focused(),
                browser.cookies_page.describe(),
            ),
            Some(SystemPage::About) => (browser.about.focused(), browser.about.describe()),
            _ => (None, Vec::new()),
        };
        let revision = (browser.bookmarks.revision(), browser.history.revision());
        if cache.revision != Some(revision) {
            cache.suggestions = Arc::new(
                browser
                    .bookmarks
                    .bookmarks()
                    .map(|entry| Suggestion {
                        title: entry.title.clone(),
                        url: entry.url.clone(),
                        kept: true,
                    })
                    .chain(browser.history.visits().map(|entry| Suggestion {
                        title: entry.title.clone(),
                        url: entry.url.clone(),
                        kept: false,
                    }))
                    .collect(),
            );
            cache.revision = Some(revision);
        }
        Self {
            serial,
            tabs: browser
                .tabs
                .iter()
                .map(|tab| WindowTab::from_tab(tab, browser.world.loading(tab.id)))
                .collect(),
            active: browser.active,
            labels: browser
                .tabs
                .iter()
                .map(|tab| TabLabel {
                    id: tab.id.0,
                    title: tab.title.clone(),
                    loading: browser.world.loading(tab.id),
                })
                .collect(),
            scene,
            viewport,
            cursor: browser.cursor,
            chrome_focused: browser.keyboard_surface == SURFACE_CHROME,
            content_chrome: browser.keyboard_surface == SURFACE_SYSTEM
                || browser.keyboard_surface == super::super::SURFACE_INSPECTOR,
            find_wants_keyboard: browser.ui.find_wants_keyboard(),
            picking: browser.inspector.picking,
            address_focused: browser.ui.address_focused() || browser.ui.address_wants_keyboard(),
            find: browser
                .ui
                .finding()
                .then(|| browser.ui.find.text().to_owned()),
            find_status: browser.ui.find_status,
            bookmark: browser.ui.bookmark,
            zoom: browser.zoom,
            theme: browser.theme.clone(),
            appearance: browser.window_appearance(),
            next: browser.next_frame(),
            work: browser.work_counters(),
            document: Arc::clone(&cache.document),
            described,
            focused: (browser.keyboard_surface == SURFACE_SYSTEM)
                .then_some(focused)
                .flatten(),
            suggestions: Arc::clone(&cache.suggestions),
            context: browser.ui.context_menu(),
        }
    }
}

struct MetadataCache {
    tab: Option<TabId>,
    document: Arc<accesskit::TreeUpdate>,
    revision: Option<(u64, u64)>,
    suggestions: Arc<Vec<Suggestion>>,
}

pub(super) fn content_loop(
    mut browser: Browser,
    receiver: mpsc::Receiver<Command>,
    mailbox: Arc<Mutex<Mailbox>>,
    clipboard_state: Rc<RefCell<ClipboardState>>,
    mut viewport: Viewport,
) {
    let mut serial = 0;
    let mut cache = MetadataCache {
        tab: None,
        document: Arc::new(crate::a11y::empty_tree("")),
        revision: None,
        suggestions: Arc::new(Vec::new()),
    };
    let mut first = true;
    loop {
        let snapshot = Snapshot::capture(&mut browser, viewport, serial, &mut cache);
        let next = snapshot.next;
        let (previous, waker) = {
            let mut mailbox = mailbox.lock().unwrap();
            if first {
                if let Some(trace) = &mailbox.startup {
                    trace.mark("first_content_scene_ready");
                }
                first = false;
            }
            let previous = mailbox.latest.replace(snapshot);
            (previous, mailbox.waker.clone())
        };
        drop(previous);
        if let Some(waker) = waker {
            waker.wake();
        }
        let command = match next {
            FrameRequest::At(at) => {
                match receiver.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => Command::Wake,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            _ => match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        // Limit input batching so a stream of pointer events cannot indefinitely
        // postpone a content result. This preserves ordered keys/navigation.
        for command in std::iter::once(command).chain(receiver.try_iter()).take(64) {
            let command = match command {
                Command::Pointer(slot) => match slot.lock().unwrap().take() {
                    Some(input) => Command::Input(input),
                    None => continue,
                },
                command => command,
            };
            match command {
                Command::Pointer(_) => unreachable!("pointer slot was expanded"),
                Command::SaveDownload { id, path } => {
                    if let Some(bytes) = browser
                        .downloads
                        .get(id)
                        .map(crate::downloads::Download::payload)
                    {
                        browser.start_save(id, crate::downloads::Destination::Exact(path), bytes);
                    }
                }
                Command::DownloadDirectory(path) => {
                    browser
                        .settings
                        .settings
                        .apply(crate::settings::Action::SetDownloadDirectory(
                            path.to_string_lossy().into_owned(),
                        ));
                    crate::preferences::save(&browser.settings.settings);
                }
                Command::Shutdown => return,
                Command::Wake => {
                    mailbox.lock().unwrap().wake_pending = false;
                }
                Command::ClipboardWritten(revision) => {
                    clipboard_state.borrow_mut().acknowledged = revision;
                }
                #[cfg(test)]
                Command::Task(task) => task(&mut browser),
                Command::Frame => {
                    {
                        let mut mailbox = mailbox.lock().unwrap();
                        viewport = mailbox.viewport;
                        mailbox.frame_pending = false;
                    }
                    browser.handle_event(PlatformEvent::Resized(viewport));
                }
                Command::Input(input) => {
                    let Input {
                        serial: received,
                        tab,
                        navigation,
                        intent,
                        chrome,
                        action,
                        event,
                        clipboard,
                        system_index,
                    } = *input;
                    serial = received;
                    if navigation.is_some_and(|expected| {
                        browser
                            .tab_index(tab)
                            .is_none_or(|index| browser.tabs[index].navigation != expected)
                    }) {
                        continue;
                    }
                    if let Some(intent) = &intent {
                        intent.apply(&mut browser);
                    } else if browser.active_id() != tab {
                        let Some(index) = browser.tab_index(tab) else {
                            continue;
                        };
                        browser.select_tab(index);
                    }
                    {
                        let mut state = clipboard_state.borrow_mut();
                        state.serial = serial;
                        if let Some(text) = clipboard
                            && state.revision <= state.acknowledged
                        {
                            state.text = text;
                        }
                    }
                    if let Some(chrome) = chrome {
                        if !chrome.context_open {
                            browser.ui.dismiss_context_menu();
                            if !matches!(action, UiAction::Context(_)) {
                                browser.context_target = None;
                            }
                        }
                        if chrome.focused {
                            browser.activate_surface(SURFACE_CHROME);
                            browser.ui.focus_address();
                        }
                        browser.ui.address = chrome.address;
                        match chrome.find {
                            Some(query) => browser.ui.restore_find(&query),
                            None => browser.ui.close_find(),
                        }
                        browser.update_find();
                    }
                    match action {
                        UiAction::NewTab
                        | UiAction::CloseTab(_)
                        | UiAction::SelectTab(_)
                        | UiAction::MoveTab { .. } => {}
                        UiAction::NavigateInNewTab(url) => browser.navigate(&url),
                        UiAction::OpenPage(page) => browser.open_system(page),
                        action => browser.apply(action),
                    }
                    if let Some(mut event) = event {
                        if let (Some(index), PlatformEvent::AccessibilityRequest { node, .. }) =
                            (system_index, &mut event)
                        {
                            *node =
                                crate::a11y::described_node(browser.ui.describe().len() + index);
                        }
                        browser.handle_event(event);
                    }
                }
                Command::Files {
                    tab,
                    navigation,
                    request,
                    paths,
                } => {
                    if let Some(index) = browser.tab_index(tab)
                        && browser.tabs[index].navigation == navigation
                        && !paths.is_empty()
                    {
                        let files = super::super::input::read_chosen_files(paths);
                        if let Some(page) = browser.world.page_mut(tab) {
                            page.set_files(request.node, files);
                            browser.accessibility_dirty = true;
                        }
                    }
                }
            }
        }
    }
}
