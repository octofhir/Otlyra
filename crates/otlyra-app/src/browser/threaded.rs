//! Native chrome stays on the event-loop thread; pages have one content owner.
//!
//! Only owned metadata and retained display lists cross the boundary. A Browser
//! (including its non-Send script worlds) is constructed and destroyed on its
//! owner thread. Commands are ordered; frame requests and results are coalesced.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use otlyra_gfx::{DisplayList, PaintTarget};
use otlyra_platform::{
    Cursor, FrameRequest, Key, LayerId, LayerRect, Modifiers, Painter, PainterWork, PlatformEvent,
    Scene, SceneLayer, Viewport, Waker, accesskit,
};
use otlyra_text::TextEngine;

use crate::clipboard::Clipboard;
use crate::ui::{BrowserUi, Suggestion, SystemPage, TabLabel, UI_HEIGHT, UiAction};
use crate::widget::{Described, FocusId};

mod owner;
use owner::{ClipboardState, ContentClipboard, content_loop};

use super::frame::{LAYER_CHROME, Scaled, scaled};
use super::{Browser, SURFACE_CHROME, SURFACE_SYSTEM, Tab, TabId};

// Distinct from a content layer so replacing a loading placeholder is full damage.
const LAYER_PLACEHOLDER: u64 = 4;

/// Tab metadata for native chrome; navigation histories stay with the owner.
#[derive(Clone)]
pub struct WindowTab {
    /// Stable context identity.
    pub id: TabId,
    /// The current address.
    pub url: String,
    /// The current title.
    pub title: String,
    /// The current load's failure.
    pub error: Option<String>,
    /// A browser-owned page, when this tab displays one.
    pub system: Option<SystemPage>,
    /// The current document generation.
    pub navigation: Option<u64>,
    /// Whether a load remains in flight.
    pub loading: bool,
    back: bool,
    forward: bool,
}

impl WindowTab {
    fn from_tab(tab: &Tab, loading: bool) -> Self {
        Self {
            id: tab.id,
            url: tab.url.clone(),
            title: tab.title.clone(),
            error: tab.error.clone(),
            system: tab.system,
            navigation: tab.navigation,
            loading,
            back: tab.can_go_back(),
            forward: tab.can_go_forward(),
        }
    }
    /// Whether the owner has a previous history entry.
    pub fn can_go_back(&self) -> bool {
        self.back
    }
    /// Whether the owner has a following history entry.
    pub fn can_go_forward(&self) -> bool {
        self.forward
    }
}

pub(super) enum NativeRequest {
    SaveDownload {
        id: crate::downloads::DownloadId,
        filename: String,
        directory: Option<std::path::PathBuf>,
    },
    DownloadDirectory(Option<std::path::PathBuf>),
    Clipboard {
        text: String,
        revision: u64,
        serial: u64,
    },
    Files {
        tab: TabId,
        navigation: Option<u64>,
        request: crate::page::FileRequest,
    },
}

#[derive(Clone)]
enum Intent {
    Open(Tab),
    Close(TabId, Option<Tab>),
    Select(TabId),
    Move(TabId, usize),
}

impl Intent {
    fn project(&self, tabs: &mut Vec<WindowTab>, active: &mut usize) {
        match self {
            Self::Open(tab) => {
                tabs.push(WindowTab::from_tab(tab, false));
                *active = tabs.len() - 1;
            }
            Self::Close(id, replacement) => {
                if let Some(index) = tabs.iter().position(|tab| tab.id == *id) {
                    tabs.remove(index);
                    if let Some(tab) = replacement {
                        tabs.push(WindowTab::from_tab(tab, false));
                    }
                    if index < *active {
                        *active -= 1;
                    }
                    *active = (*active).min(tabs.len().saturating_sub(1));
                }
            }
            Self::Select(id) => {
                if let Some(index) = tabs.iter().position(|tab| tab.id == *id) {
                    *active = index;
                }
            }
            Self::Move(id, to) => {
                let selected = tabs.get(*active).map(|tab| tab.id);
                if let Some(from) = tabs.iter().position(|tab| tab.id == *id) {
                    let tab = tabs.remove(from);
                    tabs.insert((*to).min(tabs.len()), tab);
                    *active = tabs
                        .iter()
                        .position(|tab| Some(tab.id) == selected)
                        .unwrap_or(0);
                }
            }
        }
    }

    fn apply(&self, browser: &mut Browser) {
        match self {
            Self::Open(tab) => {
                browser.tabs.push(tab.clone());
                browser.select_tab(browser.tabs.len() - 1);
            }
            Self::Close(id, replacement) => {
                if let Some(index) = browser.tab_index(*id) {
                    browser.close_tab(index);
                    if let Some(tab) = replacement {
                        // The shell allocated this identity before the owner
                        // closed its last tab; no document uses the replacement.
                        browser.tabs[0] = tab.clone();
                    }
                }
            }
            Self::Select(id) => {
                if let Some(index) = browser.tab_index(*id) {
                    browser.select_tab(index);
                }
            }
            Self::Move(id, to) => browser.move_tab(*id, *to),
        }
    }
}

struct ChromeInput {
    address: crate::ui::TextField,
    find: Option<String>,
    focused: bool,
    context_open: bool,
}

enum Command {
    SaveDownload {
        id: crate::downloads::DownloadId,
        path: std::path::PathBuf,
    },
    DownloadDirectory(std::path::PathBuf),
    Wake,
    Frame,
    Visibility(bool),
    Input(Box<Input>),
    Pointer(PointerSlot),
    Files {
        tab: TabId,
        navigation: Option<u64>,
        request: crate::page::FileRequest,
        paths: Vec<std::path::PathBuf>,
    },
    Shutdown,
    ClipboardWritten(u64),
    #[cfg(test)]
    Task(Box<dyn FnOnce(&mut Browser) + Send>),
}

type PointerSlot = Arc<Mutex<Option<Box<Input>>>>;

struct Input {
    serial: u64,
    tab: TabId,
    navigation: Option<Option<u64>>,
    intent: Option<Intent>,
    chrome: Option<ChromeInput>,
    action: UiAction,
    event: Option<PlatformEvent>,
    clipboard: Option<Option<String>>,
    system_index: Option<usize>,
}

struct Mailbox {
    latest: Option<Snapshot>,
    viewport: Viewport,
    frame_pending: bool,
    wake_pending: bool,
    waker: Option<Waker>,
    startup: Option<otlyra_platform::StartupTrace>,
}

struct Snapshot {
    serial: u64,
    tabs: Vec<WindowTab>,
    active: usize,
    window_visible: bool,
    labels: Vec<TabLabel>,
    scene: Scene,
    viewport: Viewport,
    cursor: Cursor,
    chrome_focused: bool,
    content_chrome: bool,
    find_wants_keyboard: bool,
    picking: bool,
    address_focused: bool,
    find: Option<String>,
    find_status: crate::ui::FindStatus,
    bookmark: crate::ui::Bookmarked,
    zoom: f32,
    theme: crate::widget::theme::Theme,
    appearance: Option<otlyra_platform::ColorScheme>,
    next: FrameRequest,
    work: PainterWork,
    document: Arc<accesskit::TreeUpdate>,
    described: Vec<Described>,
    focused: Option<FocusId>,
    suggestions: Arc<Vec<Suggestion>>,
    context: Option<((f64, f64), Vec<crate::ui::ContextRow>)>,
}

struct BlankScene {
    viewport: Viewport,
    theme: crate::widget::theme::Theme,
    error: Option<String>,
    list: Arc<DisplayList>,
}

struct ContentFrame {
    tab: Option<TabId>,
    viewport: Viewport,
    layers: Vec<(LayerId, u64, LayerRect)>,
}

/// A native window's chrome and the channel to its content owner.
pub struct WindowBrowser {
    ui: BrowserUi,
    text: TextEngine,
    clipboard: Box<dyn Clipboard>,
    sender: mpsc::Sender<Command>,
    pointer_slot: Option<PointerSlot>,
    native: mpsc::Receiver<NativeRequest>,
    mailbox: Arc<Mutex<Mailbox>>,
    snapshot: Snapshot,
    tabs: Vec<WindowTab>,
    active: usize,
    pending: VecDeque<(u64, Intent)>,
    serial: u64,
    focus_serial: u64,
    clipboard_serial: u64,
    chrome_focused: bool,
    content_chrome: bool,
    pointer: (f64, f64),
    content_pressed: bool,
    cursor: Cursor,
    traversal_forward: bool,
    interface: bool,
    chrome_device: Option<Scaled>,
    mark: Option<otlyra_gfx::peniko::ImageData>,
    blank: Option<BlankScene>,
    blank_epoch: u64,
    scenes: HashMap<TabId, (Viewport, Scene)>,
    scene_device: HashMap<(TabId, LayerId), Option<Scaled>>,
    composed_content: Option<ContentFrame>,
    presented_content: Option<(TabId, Viewport)>,
    invalidated: HashMap<TabId, u64>,
    accessibility_dirty: bool,
    accessibility_tree: crate::a11y::WindowTree,
    chrome_description: Vec<Described>,
    chrome_description_builds: Option<u64>,
    placeholder_document: Option<(TabId, Option<u64>, String, Arc<accesskit::TreeUpdate>)>,
    visible_dirty: bool,
    window_visible: bool,
    first_content_received: bool,
    first_content_composed: bool,
    frame_has_content: bool,
    context_serial: Option<u64>,
    started: Instant,
}

impl WindowBrowser {
    /// The factory must construct its Browser on this new thread. It must not
    /// run scripts or wait for a load: initialization has a short handoff to UI.
    pub fn spawn(
        viewport: Viewport,
        interface: bool,
        factory: impl FnOnce(TextEngine) -> Browser + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::spawn_with_trace(viewport, interface, None, factory)
    }

    /// The same owner with first-content milestones in the native startup trace.
    pub fn spawn_with_trace(
        viewport: Viewport,
        interface: bool,
        startup: Option<otlyra_platform::StartupTrace>,
        factory: impl FnOnce(TextEngine) -> Browser + Send + 'static,
    ) -> std::io::Result<Self> {
        let text = TextEngine::deferred();
        if let Err(error) = text.preload_system_fonts() {
            tracing::warn!(%error, "no font discovery worker");
        }
        let content_text = text.fork();
        let (sender, receiver) = mpsc::channel();
        let (native_sender, native) = mpsc::channel();
        let (initial_sender, initial) = mpsc::sync_channel(1);
        let mailbox = Arc::new(Mutex::new(Mailbox {
            latest: None,
            viewport,
            frame_pending: false,
            wake_pending: false,
            waker: None,
            startup,
        }));
        let owner_mailbox = Arc::clone(&mailbox);
        let wake_mailbox = Arc::clone(&mailbox);
        let wake_sender = sender.clone();
        std::thread::Builder::new()
            .name("otlyra-content".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let mut browser = factory(content_text);
                if let Some(trace) = &owner_mailbox.lock().unwrap().startup {
                    trace.mark("content_owner_ready");
                }
                browser.native_requests = Some(native_sender.clone());
                let clipboard = Rc::new(RefCell::new(ClipboardState::default()));
                browser.set_clipboard(Box::new(ContentClipboard {
                    state: Rc::clone(&clipboard),
                    requests: native_sender.clone(),
                }));
                // Transfer only initial metadata before installing the fetch waker:
                // set_waker pumps ready responses and may execute arbitrary script.
                if initial_sender
                    .send((
                        browser
                            .tabs
                            .iter()
                            .map(|tab| WindowTab::from_tab(tab, browser.world.loading(tab.id)))
                            .collect::<Vec<_>>(),
                        browser.active,
                        browser.mark.clone(),
                        browser.window_visible,
                    ))
                    .is_err()
                {
                    return;
                }
                browser.set_waker(Waker::from_callback(move || {
                    let mut mailbox = wake_mailbox.lock().unwrap();
                    if !mailbox.wake_pending {
                        mailbox.wake_pending = true;
                        let _ = wake_sender.send(Command::Wake);
                    }
                }));
                content_loop(browser, receiver, owner_mailbox, clipboard, viewport);
            })?;
        let (tabs, active, mark, window_visible) = initial.recv().map_err(std::io::Error::other)?;
        let mut ui = BrowserUi::new();
        ui.address.set_text(tabs[active].url.clone());
        if tabs[active].url.is_empty() {
            ui.focus_address();
        }
        let snapshot = Snapshot {
            serial: 0,
            tabs: tabs.clone(),
            active,
            window_visible,
            labels: Vec::new(),
            scene: Scene { layers: Vec::new() },
            viewport,
            cursor: Cursor::Default,
            chrome_focused: tabs[active].url.is_empty(),
            content_chrome: tabs[active].system.is_some(),
            find_wants_keyboard: false,
            picking: false,
            address_focused: tabs[active].url.is_empty(),
            find: None,
            find_status: Default::default(),
            bookmark: crate::ui::Bookmarked::Impossible,
            zoom: 1.0,
            theme: crate::widget::theme::Theme::light(),
            appearance: None,
            next: FrameRequest::None,
            work: Default::default(),
            document: Arc::new(crate::a11y::empty_tree(&tabs[active].title)),
            described: Vec::new(),
            focused: None,
            suggestions: Arc::new(Vec::new()),
            context: None,
        };
        Ok(Self {
            ui,
            text,
            clipboard: Box::new(crate::clipboard::InMemory::default()),
            sender,
            pointer_slot: None,
            native,
            mailbox,
            chrome_focused: snapshot.chrome_focused,
            content_chrome: snapshot.content_chrome,
            snapshot,
            tabs,
            active,
            pending: VecDeque::new(),
            serial: 0,
            focus_serial: 0,
            clipboard_serial: 0,
            pointer: (0.0, 0.0),
            content_pressed: false,
            cursor: Cursor::Default,
            traversal_forward: true,
            interface,
            chrome_device: None,
            mark,
            blank: None,
            blank_epoch: 0,
            scenes: HashMap::new(),
            scene_device: HashMap::new(),
            composed_content: None,
            presented_content: None,
            invalidated: HashMap::new(),
            accessibility_dirty: true,
            accessibility_tree: Default::default(),
            chrome_description: Vec::new(),
            chrome_description_builds: None,
            placeholder_document: None,
            visible_dirty: true,
            window_visible,
            first_content_received: false,
            first_content_composed: false,
            frame_has_content: false,
            context_serial: None,
            started: Instant::now(),
        })
    }

    /// Install a native clipboard on the window owner, never on content.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    /// Current visible tab metadata, including ordered unacknowledged intents.
    pub fn tabs(&self) -> &[WindowTab] {
        &self.tabs
    }
    /// The visible active tab's position.
    pub fn active(&self) -> usize {
        self.active
    }
    /// The native chrome's input and focus state.
    pub fn ui(&self) -> &BrowserUi {
        &self.ui
    }

    fn send(
        &mut self,
        intent: Option<Intent>,
        action: UiAction,
        event: Option<PlatformEvent>,
        chrome: bool,
        system_index: Option<usize>,
    ) {
        self.serial += 1;
        self.focus_serial = self.serial;
        let tab = self.tabs[self.active].id;
        if let Some(intent) = &intent {
            self.pending.push_back((self.serial, intent.clone()));
        }
        let paste = matches!(event, Some(PlatformEvent::KeyPressed { key: Key::Character('v'), modifiers }) if modifiers.is_accelerator());
        let clipboard = paste.then(|| self.clipboard.read());
        let page_event = matches!(
            action,
            UiAction::ToggleBookmark | UiAction::ResetZoom | UiAction::FindStep(_)
        ) || !chrome
            && event.is_some_and(|event| match event {
                PlatformEvent::KeyPressed { key, modifiers } => {
                    key != Key::F5
                        && !(key == Key::Character('i')
                            && modifiers.alt
                            && (modifiers.command || modifiers.control))
                        && (!modifiers.is_accelerator()
                            || matches!(key, Key::Character('a' | 'c' | 'x' | 'v')))
                }
                PlatformEvent::Scroll { modifiers, .. } => !modifiers.is_accelerator(),
                PlatformEvent::TextInput(_)
                | PlatformEvent::PointerMoved { .. }
                | PlatformEvent::PointerPressed { .. }
                | PlatformEvent::PointerReleased
                | PlatformEvent::ContextMenuRequested
                | PlatformEvent::AccessibilityRequest { .. } => true,
                _ => false,
            });
        let input = Box::new(Input {
            serial: self.serial,
            tab,
            navigation: page_event.then_some(self.tabs[self.active].navigation),
            intent,
            chrome: chrome.then(|| ChromeInput {
                address: self.ui.address.clone(),
                find: self.ui.finding().then(|| self.ui.find.text().to_owned()),
                focused: self.chrome_focused,
                context_open: self.ui.context_menu().is_some(),
            }),
            action,
            event,
            clipboard,
            system_index,
        });
        if matches!(event, Some(PlatformEvent::PointerMoved { .. })) {
            if let Some(slot) = &self.pointer_slot {
                let mut previous = slot.lock().unwrap();
                if previous.is_some() {
                    *previous = Some(input);
                    return;
                }
            }
            let slot = Arc::new(Mutex::new(Some(input)));
            let _ = self.sender.send(Command::Pointer(Arc::clone(&slot)));
            self.pointer_slot = Some(slot);
        } else {
            self.pointer_slot = None;
            let _ = self.sender.send(Command::Input(input));
        }
    }

    fn apply(&mut self, action: UiAction) {
        let intent = match &action {
            UiAction::NewTab | UiAction::NavigateInNewTab(_) | UiAction::OpenPage(_) => {
                Some(Intent::Open(Tab::blank()))
            }
            UiAction::CloseTab(index) => self
                .tabs
                .get(*index)
                .map(|tab| Intent::Close(tab.id, (self.tabs.len() == 1).then(Tab::blank))),
            UiAction::SelectTab(index) => self.tabs.get(*index).map(|tab| Intent::Select(tab.id)),
            UiAction::MoveTab { id, to } => Some(Intent::Move(TabId(*id), *to)),
            _ => None,
        };
        let old_active = self.tabs[self.active].id;
        if let Some(intent) = &intent {
            intent.project(&mut self.tabs, &mut self.active);
            if old_active != self.tabs[self.active].id {
                self.ui.dismiss_popup();
                self.ui.close_find();
                self.ui.address.set_text(self.tabs[self.active].url.clone());
                self.chrome_focused = self.tabs[self.active].url.is_empty();
                self.content_chrome = self.tabs[self.active].system.is_some();
                if self.chrome_focused {
                    self.ui.focus_address();
                } else {
                    self.ui.blur();
                }
            }
        }
        match &action {
            UiAction::Navigate(_) | UiAction::NavigateInNewTab(_) | UiAction::LeaveChrome(_) => {
                self.chrome_focused = false;
                self.content_chrome = self.tabs[self.active].system.is_some();
                self.ui.blur();
            }
            UiAction::ToggleInspector
            | UiAction::Context(crate::ui::ContextCommand::InspectElement) => {
                self.chrome_focused = false;
                self.content_chrome = true;
                self.ui.blur();
            }
            _ => {}
        }
        self.send(intent, action.clone(), None, true, None);
        if !matches!(action, UiAction::Context(_)) && self.ui.context_menu().is_none() {
            self.context_serial = None;
        }
        self.visible_dirty = true;
        self.accessibility_dirty = true;
        if matches!(
            action,
            UiAction::Navigate(_)
                | UiAction::NavigateInNewTab(_)
                | UiAction::Back
                | UiAction::Forward
                | UiAction::Reload
                | UiAction::OpenPage(_)
        ) {
            let tab = self.tabs[self.active].id;
            self.invalidated.insert(tab, self.serial);
            self.scenes.remove(&tab);
        }
        self.refresh_suggestions();
    }

    fn refresh_suggestions(&mut self) {
        let typed = self.ui.address.text().trim().to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let rows = if self.ui.address_focused() && !typed.is_empty() {
            self.snapshot
                .suggestions
                .iter()
                .filter(|row| {
                    (row.url.to_lowercase().contains(&typed)
                        || row.title.to_lowercase().contains(&typed))
                        && seen.insert(row.url.clone())
                })
                .take(6)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        self.ui.set_suggestions(rows);
    }

    fn receive(&mut self) {
        let latest = self.mailbox.lock().unwrap().latest.take();
        if let Some(mut snapshot) = latest {
            let tab = snapshot.tabs[snapshot.active].id;
            self.visible_dirty |= self.snapshot.theme != snapshot.theme
                || self.snapshot.active != snapshot.active
                || !self
                    .snapshot
                    .labels
                    .iter()
                    .map(|tab| (tab.id, &tab.title, tab.loading))
                    .eq(snapshot
                        .labels
                        .iter()
                        .map(|tab| (tab.id, &tab.title, tab.loading)))
                || !Arc::ptr_eq(&self.snapshot.document, &snapshot.document)
                || self.snapshot.zoom != snapshot.zoom
                || self.snapshot.bookmark != snapshot.bookmark
                || self.snapshot.find_status != snapshot.find_status;
            if self.tabs[self.active].id == tab {
                self.visible_dirty |= self.scenes.get(&tab).is_none_or(|(viewport, scene)| {
                    *viewport != snapshot.viewport || !same_scene(scene, &snapshot.scene)
                });
            }
            if self
                .invalidated
                .get(&tab)
                .is_none_or(|serial| snapshot.serial >= *serial)
            {
                if let Some((viewport, previous)) = self.scenes.get(&tab)
                    && *viewport == snapshot.viewport
                {
                    accumulate_damage(previous, &mut snapshot.scene);
                } else {
                    full_damage(&mut snapshot.scene);
                }
                self.scene_device.retain(|(id, _), _| *id != tab);
                self.scenes.insert(
                    tab,
                    (
                        snapshot.viewport,
                        std::mem::replace(&mut snapshot.scene, Scene { layers: Vec::new() }),
                    ),
                );
            }
            self.pending.retain(|(serial, _)| *serial > snapshot.serial);
            self.tabs = snapshot.tabs.clone();
            self.active = snapshot.active;
            for (_, intent) in &self.pending {
                intent.project(&mut self.tabs, &mut self.active);
            }
            self.scenes
                .retain(|id, _| self.tabs.iter().any(|tab| tab.id == *id));
            self.scene_device
                .retain(|(id, _), _| self.tabs.iter().any(|tab| tab.id == *id));
            self.invalidated
                .retain(|id, _| self.tabs.iter().any(|tab| tab.id == *id));
            self.ui.set_theme(snapshot.theme.clone());
            if snapshot.serial >= self.focus_serial {
                let entering = !self.chrome_focused && snapshot.chrome_focused;
                self.chrome_focused = snapshot.chrome_focused;
                self.content_chrome = snapshot.content_chrome;
                if entering {
                    if snapshot.find_wants_keyboard {
                        self.ui.open_find();
                    } else if snapshot.address_focused {
                        self.ui.focus_address();
                    } else {
                        self.ui.focus_edge(self.traversal_forward);
                    }
                } else if !self.chrome_focused {
                    self.ui.blur();
                }
                if !self.ui.address_focused() && !self.ui.address_wants_keyboard() {
                    self.ui.address.set_text(self.tabs[self.active].url.clone());
                }
                match &snapshot.find {
                    Some(query) if !self.ui.finding() || self.ui.find.text() != query => {
                        self.ui.restore_find(query)
                    }
                    None => self.ui.close_find(),
                    _ => {}
                }
                if let Some((at, rows)) = &snapshot.context
                    && self
                        .context_serial
                        .is_some_and(|serial| serial <= snapshot.serial)
                {
                    self.context_serial = None;
                    self.ui.open_context_menu(at.0, at.1, rows.clone());
                }
            }
            let current = self.tabs[self.active].id == tab;
            self.ui.find_status = if current
                && snapshot.find.as_deref() == self.ui.finding().then_some(self.ui.find.text())
            {
                snapshot.find_status
            } else {
                Default::default()
            };
            self.ui.bookmark = if current {
                snapshot.bookmark
            } else if self.tabs[self.active].url.is_empty() {
                crate::ui::Bookmarked::Impossible
            } else if snapshot
                .suggestions
                .iter()
                .any(|row| row.kept && row.url == self.tabs[self.active].url)
            {
                crate::ui::Bookmarked::Yes
            } else {
                crate::ui::Bookmarked::No
            };
            self.ui.zoom = if current { snapshot.zoom } else { 1.0 };
            self.snapshot = snapshot;
            if !self.first_content_received && !self.scenes.is_empty() {
                if let Some(trace) = &self.mailbox.lock().unwrap().startup {
                    trace.mark("first_content_scene_received");
                }
                self.first_content_received = true;
            }
            self.refresh_suggestions();
            self.accessibility_dirty = true;
        }
        while let Ok(request) = self.native.try_recv() {
            match request {
                NativeRequest::DownloadDirectory(current) => {
                    if let Some(path) =
                        super::system_pages::choose_download_directory(current.as_deref())
                    {
                        let _ = self.sender.send(Command::DownloadDirectory(path));
                    }
                }
                NativeRequest::SaveDownload {
                    id,
                    filename,
                    directory,
                } => {
                    if let Some(path) =
                        super::system_pages::choose_download_path(&filename, directory.as_deref())
                    {
                        let _ = self.sender.send(Command::SaveDownload { id, path });
                    }
                }
                NativeRequest::Clipboard {
                    text,
                    revision,
                    serial,
                } => {
                    if serial >= self.clipboard_serial {
                        self.clipboard.write(text);
                    }
                    let _ = self.sender.send(Command::ClipboardWritten(revision));
                }
                NativeRequest::Files {
                    tab,
                    navigation,
                    request,
                } => {
                    if !self
                        .tabs
                        .iter()
                        .any(|entry| entry.id == tab && entry.navigation == navigation)
                    {
                        continue;
                    }
                    let paths = super::input::choose_paths(&request);
                    let _ = self.sender.send(Command::Files {
                        tab,
                        navigation,
                        request,
                        paths,
                    });
                }
            }
        }
    }

    fn request_frame(&self, viewport: Viewport) {
        let mut mailbox = self.mailbox.lock().unwrap();
        let changed = mailbox.viewport != viewport;
        mailbox.viewport = viewport;
        if (changed || self.snapshot.next == FrameRequest::Vsync) && !mailbox.frame_pending {
            mailbox.frame_pending = true;
            let _ = self.sender.send(Command::Frame);
        }
    }

    fn select(&mut self, index: usize) {
        self.apply(UiAction::SelectTab(index));
    }
    fn cycle(&mut self, forward: bool) {
        let len = self.tabs.len();
        self.select(if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        });
    }
}

impl Drop for WindowBrowser {
    fn drop(&mut self) {
        // A running callback must not turn closing the native window into a
        // blocking join. Its world still drops on its content owner afterward.
        let _ = self.sender.send(Command::Shutdown);
    }
}

impl Painter for WindowBrowser {
    fn handle_event(&mut self, event: PlatformEvent) -> FrameRequest {
        let pointer = self.pointer;
        let popup = self.ui.popup_open() || self.ui.pointer_captured();
        let before = (
            self.chrome_focused,
            self.ui.focused(),
            self.ui.address.clone(),
            self.ui.find.clone(),
            self.ui.popup_open(),
        );
        self.visible_dirty = false;
        self.on_event(event);
        let after = (
            self.chrome_focused,
            self.ui.focused(),
            self.ui.address.clone(),
            self.ui.find.clone(),
            self.ui.popup_open(),
        );
        let hover = matches!(event, PlatformEvent::PointerMoved { x, y } if pointer != (x, y) && (popup || self.interface && (pointer.1 < UI_HEIGHT || y < UI_HEIGHT)));
        if self.visible_dirty
            || before != after
            || hover
            || matches!(
                event,
                PlatformEvent::SurfaceReady(_) | PlatformEvent::Resized(_)
            )
        {
            FrameRequest::Now
        } else {
            FrameRequest::None
        }
    }
    fn set_waker(&mut self, waker: Waker) {
        self.mailbox.lock().unwrap().waker = Some(waker.clone());
        // A first result may have arrived before native initialization finished.
        self.receive();
        waker.wake();
    }

    fn next_frame(&self) -> FrameRequest {
        if !self.window_visible {
            // The owner schedules timers itself. Native UI has nothing to draw.
            return FrameRequest::None;
        }
        if (self.tabs[self.active].id == self.snapshot.tabs[self.snapshot.active].id
            && self.snapshot.window_visible
            && self.snapshot.next == FrameRequest::Vsync)
            || self.tabs.iter().any(|tab| tab.loading)
        {
            FrameRequest::Vsync
        } else {
            self.ui
                .next_tooltip_frame()
                .map_or(FrameRequest::None, FrameRequest::At)
        }
    }

    fn work_counters(&self) -> PainterWork {
        let mut work = self.snapshot.work;
        work.chrome_reconciles += self.ui.builds();
        work.chrome_layouts += self.ui.builds() + self.ui.tab_builds() + self.ui.toolbar_builds();
        work.chrome_paints += self.ui.builds() + self.ui.tab_builds() + self.ui.toolbar_builds();
        work.chrome_semantics +=
            self.ui.tab_semantics_builds() + self.ui.toolbar_semantics_builds();
        work
    }

    fn on_event(&mut self, event: PlatformEvent) {
        self.receive();
        self.accessibility_dirty = true;
        match event {
            PlatformEvent::VisibilityChanged(visible) => {
                if self.window_visible != visible {
                    self.window_visible = visible;
                    self.visible_dirty = true;
                    self.pointer_slot = None;
                    let _ = self.sender.send(Command::Visibility(visible));
                }
            }
            PlatformEvent::Woken => {}
            PlatformEvent::SurfaceReady(viewport) | PlatformEvent::Resized(viewport) => {
                self.ui.dismiss_context_menu();
                self.context_serial = None;
                self.request_frame(viewport);
            }
            PlatformEvent::PointerMoved { x, y } => {
                self.pointer = (x, y);
                let owned = self.ui.owns_pointer() || self.ui.pointer_captured();
                let action = self.ui.pointer_moved(x, y, &mut self.text);
                if action != UiAction::None {
                    self.apply(action);
                }
                if self.content_pressed || !owned || !self.ui.owns_pointer() {
                    self.send(None, UiAction::None, Some(event), false, None);
                }
            }
            PlatformEvent::PointerPressed { clicks }
                if self.interface && self.ui.owns_pointer() && !self.content_pressed =>
            {
                self.chrome_focused = true;
                let action = self.ui.pointer_pressed(&mut self.text, clicks);
                self.apply(action);
            }
            PlatformEvent::PointerReleased => {
                let owned = self.ui.pointer_captured();
                self.ui.pointer_released();
                if self.content_pressed || !owned {
                    self.send(None, UiAction::None, Some(event), false, None);
                }
                self.content_pressed = false;
            }
            PlatformEvent::ContextMenuRequested => {
                self.ui.dismiss_popup();
                self.send(None, UiAction::None, Some(event), false, None);
                self.context_serial = Some(self.serial);
            }
            PlatformEvent::TextInput(character) if self.chrome_focused && self.interface => {
                if self.ui.text_input(character) {
                    self.apply(UiAction::None);
                }
            }
            PlatformEvent::KeyPressed { key, modifiers } => {
                if self.chrome_focused
                    && modifiers.is_accelerator()
                    && matches!(key, Key::Character('c' | 'x'))
                {
                    self.clipboard_serial = self.serial + 1;
                }
                if key == Key::Tab {
                    self.traversal_forward = !modifiers.shift;
                }
                if modifiers.control
                    && !modifiers.command
                    && !modifiers.alt
                    && matches!(key, Key::Tab | Key::PageUp | Key::PageDown)
                    && (key == Key::Tab || !modifiers.shift)
                {
                    self.cycle(if key == Key::Tab {
                        !modifiers.shift
                    } else {
                        key == Key::PageDown
                    });
                } else if modifiers.is_accelerator()
                    && key == Key::Character('t')
                    && !modifiers.shift
                {
                    self.apply(UiAction::NewTab);
                } else if modifiers.is_accelerator()
                    && key == Key::Character('w')
                    && !modifiers.shift
                {
                    self.apply(UiAction::CloseTab(self.active));
                } else if modifiers.is_accelerator()
                    && matches!(key, Key::Character('[' | '{' | ']' | '}'))
                    && modifiers.shift
                {
                    self.cycle(matches!(key, Key::Character(']' | '}')));
                } else if modifiers.is_accelerator()
                    && matches!(key, Key::Character('1'..='9'))
                    && !modifiers.shift
                {
                    let Key::Character(digit) = key else {
                        unreachable!()
                    };
                    self.select(if digit == '9' {
                        self.tabs.len() - 1
                    } else {
                        (digit as u8 - b'1') as usize
                    });
                } else if (modifiers.is_accelerator()
                    && (modifiers.shift && matches!(key, Key::Character('t' | 'T'))
                        || matches!(key, Key::Character('=' | '+' | '-' | '_' | '0'))))
                    || (modifiers.alt
                        && (modifiers.command || modifiers.control)
                        && key == Key::Character('i'))
                {
                    self.send(None, UiAction::None, Some(event), false, None);
                } else if self.interface
                    && (self.chrome_focused
                        || key == Key::F5
                        || (!self.content_chrome || key == Key::Character('l'))
                            && matches!(
                                key,
                                Key::Character('r' | 'f' | 'g' | '[' | ']' | 'l' | 't')
                                    | Key::Left
                                    | Key::Right
                            )
                            && modifiers.is_accelerator()
                            && (self.chrome_focused
                                || cfg!(target_os = "macos")
                                || !matches!(key, Key::Left | Key::Right))
                        || key == Key::Escape && self.ui.popup_open())
                {
                    let action = self.ui.key_pressed(
                        key,
                        modifiers,
                        &mut self.text,
                        self.clipboard.as_mut(),
                    );
                    if self.ui.address_focused()
                        || self.ui.address_wants_keyboard()
                        || self.ui.find_focused()
                        || self.ui.find_wants_keyboard()
                    {
                        self.chrome_focused = true;
                    }
                    self.apply(action);
                } else {
                    self.send(None, UiAction::None, Some(event), false, None);
                }
            }
            PlatformEvent::Scroll {
                x, y, modifiers, ..
            } if !modifiers.is_accelerator() && self.ui.owns_pointer() && self.interface => {
                if self.pointer.1 < crate::ui::TAB_STRIP_HEIGHT && !self.ui.popup_open() {
                    self.ui
                        .scroll_tabs_by(if x.abs() > y.abs() { x } else { y });
                }
            }
            PlatformEvent::AccessibilityRequest { node, .. } => {
                if let Some(index) = crate::a11y::described_index(node) {
                    let chrome = self.ui.describe().len();
                    if index < chrome {
                        self.chrome_focused = true;
                        let action = self.ui.activate_described(index, &mut self.text);
                        self.apply(action);
                    } else {
                        self.send(
                            None,
                            UiAction::None,
                            Some(event),
                            false,
                            Some(index - chrome),
                        );
                    }
                } else {
                    self.send(None, UiAction::None, Some(event), false, None);
                }
            }
            PlatformEvent::MenuCommand(id) => {
                use crate::menu::Command as Menu;
                match Menu::from_id(id) {
                    Some(Menu::NewTab) => self.apply(UiAction::NewTab),
                    Some(Menu::CloseTab) => self.apply(UiAction::CloseTab(self.active)),
                    Some(Menu::NextTab) => self.cycle(true),
                    Some(Menu::PreviousTab) => self.cycle(false),
                    Some(Menu::Settings) => self.apply(UiAction::OpenPage(SystemPage::Settings)),
                    Some(Menu::ShowHistory) => self.apply(UiAction::OpenPage(SystemPage::History)),
                    Some(Menu::ShowDownloads) => {
                        self.apply(UiAction::OpenPage(SystemPage::Downloads))
                    }
                    Some(Menu::ShowBookmarks) => {
                        self.apply(UiAction::OpenPage(SystemPage::Bookmarks))
                    }
                    Some(Menu::ShowCookies) => self.apply(UiAction::OpenPage(SystemPage::Cookies)),
                    Some(Menu::ShowCache) => self.apply(UiAction::OpenPage(SystemPage::Cache)),
                    Some(command @ (Menu::Cut | Menu::Copy | Menu::Paste | Menu::SelectAll)) => {
                        self.on_event(PlatformEvent::KeyPressed {
                            key: Key::Character(match command {
                                Menu::Cut => 'x',
                                Menu::Copy => 'c',
                                Menu::Paste => 'v',
                                _ => 'a',
                            }),
                            modifiers: Modifiers {
                                command: cfg!(target_os = "macos"),
                                control: !cfg!(target_os = "macos"),
                                ..Default::default()
                            },
                        })
                    }
                    _ => self.send(None, UiAction::None, Some(event), false, None),
                }
            }
            PlatformEvent::CloseRequested => {
                let _ = self.sender.send(Command::Shutdown);
            }
            _ => {
                if matches!(event, PlatformEvent::PointerPressed { .. }) {
                    self.content_pressed = true;
                    self.content_chrome = self.tabs[self.active].system.is_some()
                        || self.snapshot.picking
                        || self.scenes.get(&self.tabs[self.active].id).is_some_and(
                            |(viewport, scene)| {
                                scene.layers.iter().any(|layer| {
                                    layer.id == LayerId(super::frame::LAYER_INSPECTOR)
                                        && self.pointer.1
                                            >= f64::from(layer.rect.y) / viewport.scale_factor
                                })
                            },
                        );
                    self.chrome_focused = false;
                    self.ui.dismiss_popup();
                    self.context_serial = None;
                    self.ui.blur();
                }
                self.send(None, UiAction::None, Some(event), false, None);
            }
        }
        self.cursor = if self.interface && self.ui.owns_pointer() && !self.content_pressed {
            self.ui
                .cursor_at(self.pointer.0, self.pointer.1, &mut self.text)
                .unwrap_or(Cursor::Default)
        } else {
            self.snapshot.cursor
        };
    }

    fn accessibility(&mut self) -> Option<accesskit::TreeUpdate> {
        let chrome_changed = self.chrome_description_builds != Some(self.ui.builds());
        if !std::mem::take(&mut self.accessibility_dirty) && !chrome_changed {
            return None;
        }
        if chrome_changed {
            self.chrome_description = self.ui.describe();
            self.chrome_description_builds = Some(self.ui.builds());
        }
        let tab = &self.tabs[self.active];
        let current = tab.id == self.snapshot.tabs[self.snapshot.active].id
            && self
                .invalidated
                .get(&tab.id)
                .is_none_or(|serial| self.snapshot.serial >= *serial);
        let document = if current {
            Arc::clone(&self.snapshot.document)
        } else {
            if self
                .placeholder_document
                .as_ref()
                .is_none_or(|(id, navigation, title, _)| {
                    *id != tab.id || *navigation != tab.navigation || *title != tab.title
                })
            {
                self.placeholder_document = Some((
                    tab.id,
                    tab.navigation,
                    tab.title.clone(),
                    Arc::new(crate::a11y::empty_tree(&tab.title)),
                ));
            }
            Arc::clone(&self.placeholder_document.as_ref().unwrap().3)
        };
        let described = if current {
            self.snapshot.described.as_slice()
        } else {
            &[]
        };
        let focus = if self.chrome_focused {
            self.ui
                .focused()
                .and_then(|focus| crate::a11y::focused_index(&self.chrome_description, focus))
        } else if current {
            self.snapshot
                .focused
                .and_then(|focus| crate::a11y::focused_index(described, focus))
                .map(|index| self.chrome_description.len() + index)
        } else {
            None
        };
        self.accessibility_tree.update(
            &self.chrome_description,
            described,
            focus,
            &document,
            &tab.title,
            self.interface,
        )
    }

    fn cursor(&self) -> Cursor {
        self.cursor
    }
    fn startup_ready(&self) -> bool {
        self.frame_has_content
    }

    fn frame_complete(&mut self, presented: bool) {
        let Some(frame) = self.composed_content.take() else {
            return;
        };
        if !presented {
            return;
        }
        self.presented_content = frame.tab.map(|tab| (tab, frame.viewport));
        if let Some(tab) = frame.tab
            && let Some((viewport, scene)) = self.scenes.get_mut(&tab)
            && *viewport == frame.viewport
        {
            for layer in &mut scene.layers {
                if frame.layers.contains(&(layer.id, layer.epoch, layer.rect)) {
                    // A newer cached epoch may have arrived while this frame
                    // rendered. Only clear changes confirmed by this report.
                    layer.dirty = Some(LayerRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                }
            }
        }
    }
    fn window_appearance(&self) -> Option<otlyra_platform::ColorScheme> {
        self.snapshot.appearance
    }

    fn compose(&mut self, viewport: Viewport) -> Option<Scene> {
        self.receive();
        self.frame_has_content = self.scenes.contains_key(&self.tabs[self.active].id);
        if !self.first_content_composed && self.frame_has_content {
            if let Some(trace) = &self.mailbox.lock().unwrap().startup {
                trace.mark("first_content_scene_composed");
            }
            self.first_content_composed = true;
        }
        self.request_frame(viewport);
        let tab_id = self.tabs[self.active].id;
        let mut layers = self
            .scenes
            .get(&self.tabs[self.active].id)
            .map(|(old, scene)| {
                scene
                    .layers
                    .iter()
                    .map(|layer| {
                        copy_layer(
                            layer,
                            *old,
                            viewport,
                            self.scene_device.entry((tab_id, layer.id)).or_default(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if self.presented_content != Some((tab_id, viewport)) {
            for layer in &mut layers {
                layer.dirty = None;
            }
        }
        self.composed_content = Some(ContentFrame {
            tab: self.frame_has_content.then_some(tab_id),
            viewport,
            layers: layers
                .iter()
                .map(|layer| (layer.id, layer.epoch, layer.rect))
                .collect(),
        });
        if layers.is_empty() {
            let error = self.tabs[self.active].error.clone();
            if self.blank.as_ref().is_none_or(|cached| {
                cached.viewport != viewport
                    || cached.theme != self.snapshot.theme
                    || cached.error != error
            }) {
                let mut list = DisplayList::new();
                crate::ui::paint_blank_page(
                    &mut list,
                    &self.snapshot.theme,
                    viewport.logical_width(),
                    viewport.logical_height(),
                    error.as_deref(),
                    self.mark.as_ref(),
                    &mut self.text,
                );
                list.transform(otlyra_gfx::kurbo::Affine::scale(viewport.scale_factor));
                self.blank = Some(BlankScene {
                    viewport,
                    theme: self.snapshot.theme.clone(),
                    error,
                    list: Arc::new(list),
                });
                self.blank_epoch = self.blank_epoch.wrapping_add(1);
            }
            let top = if self.interface {
                (UI_HEIGHT * viewport.scale_factor).round() as u32
            } else {
                0
            }
            .min(viewport.height);
            layers.push(SceneLayer {
                id: LayerId(LAYER_PLACEHOLDER),
                rect: LayerRect {
                    x: 0,
                    y: top,
                    width: viewport.width,
                    height: viewport.height.saturating_sub(top),
                },
                epoch: self.blank_epoch,
                list: Arc::clone(&self.blank.as_ref().unwrap().list),
                offsets: Vec::new(),
                background: None,
                dirty: None,
            });
        }
        if self.interface {
            let labels = self
                .tabs
                .iter()
                .map(|tab| TabLabel {
                    id: tab.id.0,
                    title: tab.title.clone(),
                    loading: tab.loading,
                })
                .collect::<Vec<_>>();
            let tab = &self.tabs[self.active];
            let spinner = labels
                .iter()
                .any(|tab| tab.loading)
                .then(|| self.started.elapsed().as_secs_f32() * 4.0);
            let logical = self.ui.build_display_list(
                viewport.logical_width(),
                viewport.logical_height(),
                &labels,
                self.active,
                (tab.can_go_back(), tab.can_go_forward()),
                spinner,
                &mut self.text,
            );
            let list = scaled(&mut self.chrome_device, logical, viewport.scale_factor);
            layers.push(SceneLayer {
                id: LayerId(LAYER_CHROME),
                rect: LayerRect {
                    x: 0,
                    y: 0,
                    width: viewport.width,
                    height: if self.ui.popup_open() {
                        viewport.height
                    } else {
                        ((UI_HEIGHT * viewport.scale_factor).round() as u32).min(viewport.height)
                    },
                },
                epoch: self
                    .ui
                    .builds()
                    .wrapping_add(self.ui.tab_builds())
                    .wrapping_add(self.ui.toolbar_builds()),
                list,
                offsets: Vec::new(),
                background: None,
                dirty: None,
            });
        }
        Some(Scene { layers })
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let scene = self.compose(viewport).unwrap();
        for layer in scene.layers {
            otlyra_gfx::render_scrolled(&layer.list, target, &layer.offsets);
        }
    }
}

fn copy_layer(
    layer: &SceneLayer,
    old: Viewport,
    viewport: Viewport,
    cache: &mut Option<Scaled>,
) -> SceneLayer {
    let ratio = viewport.scale_factor / old.scale_factor;
    let list = if ratio == 1.0 {
        Arc::clone(&layer.list)
    } else {
        scaled(cache, Arc::clone(&layer.list), ratio)
    };
    let x = ((f64::from(layer.rect.x) * ratio).round() as u32).min(viewport.width);
    let y = ((f64::from(layer.rect.y) * ratio).round() as u32).min(viewport.height);
    let rect = LayerRect {
        x,
        y,
        width: viewport.width.saturating_sub(x),
        height: ((f64::from(layer.rect.height) * ratio).round() as u32)
            .min(viewport.height.saturating_sub(y)),
    };
    SceneLayer {
        id: layer.id,
        rect,
        epoch: layer.epoch,
        list,
        offsets: layer.offsets.iter().map(|offset| *offset * ratio).collect(),
        background: layer.background,
        dirty: (old == viewport && rect == layer.rect)
            .then_some(layer.dirty)
            .flatten(),
    }
}

fn full_damage(scene: &mut Scene) {
    for layer in &mut scene.layers {
        layer.dirty = None;
    }
}

/// Keep every unacknowledged change when replacing a retained publication.
/// Unknown damage dominates; an unchanged epoch adds no new damage.
fn accumulate_damage(previous: &Scene, next: &mut Scene) {
    if previous.layers.len() != next.layers.len()
        || previous
            .layers
            .iter()
            .zip(&next.layers)
            .any(|(old, new)| old.id != new.id)
    {
        full_damage(next);
        return;
    }
    for (old, new) in previous.layers.iter().zip(&mut next.layers) {
        new.dirty = if old.rect != new.rect || old.background != new.background {
            None
        } else if old.epoch == new.epoch {
            old.dirty
        } else {
            old.dirty.zip(new.dirty).map(|(a, b)| a.union(&b))
        };
    }
}

fn same_scene(left: &Scene, right: &Scene) -> bool {
    left.layers.len() == right.layers.len()
        && left.layers.iter().zip(&right.layers).all(|(a, b)| {
            a.id == b.id
                && a.rect == b.rect
                && a.epoch == b.epoch
                && a.offsets == b.offsets
                && a.background == b.background
        })
}

#[cfg(test)]
mod tests;
