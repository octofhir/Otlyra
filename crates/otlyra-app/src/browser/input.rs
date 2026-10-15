//! What a key, a press or a screen reader means to the page.
//!
//! The event route decides which surface an input belongs to; this is what the page
//! does with the inputs that are its own — editing a field, moving a selection,
//! scrolling, copying, taking the keyboard from the chrome, answering a file
//! picker — and the cursor that says what a press there would do.

use otlyra_platform::{Cursor, Key, Modifiers};

use crate::page::PageScene;
use crate::ui::{SystemPage, UI_HEIGHT};

use super::{Browser, SURFACE_PAGE};

impl Browser {
    /// Tab commands belong to the window, including while an inspector filter,
    /// a page field or one of the browser's own pages holds the keyboard.
    pub(super) fn tab_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        if modifiers.control && !modifiers.command && !modifiers.alt {
            match key {
                Key::Tab => {
                    self.cycle_tab(!modifiers.shift);
                    return true;
                }
                Key::PageUp | Key::PageDown if !modifiers.shift => {
                    self.cycle_tab(key == Key::PageDown);
                    return true;
                }
                _ => {}
            }
        }
        if !modifiers.is_accelerator() {
            return false;
        }
        match key {
            Key::Character('t' | 'T') if modifiers.shift => {
                self.reopen_closed_tab();
            }
            Key::Character('w') if !modifiers.shift => self.close_tab(self.active),
            Key::Character('[' | '{') if modifiers.shift => self.cycle_tab(false),
            Key::Character(']' | '}') if modifiers.shift => self.cycle_tab(true),
            Key::Character(digit @ '1'..='9') if !modifiers.shift => {
                let index = if digit == '9' {
                    self.tabs.len() - 1
                } else {
                    (digit as u8 - b'1') as usize
                };
                if index != self.active {
                    self.select_tab(index);
                }
            }
            _ => return false,
        }
        true
    }

    /// The keys that edit a field in the page, while one has the focus.
    ///
    /// Answered before the keys that move a selection: an arrow belongs to the
    /// caret while there is a caret, and to the page's selection otherwise.
    pub(super) fn page_edit_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        use crate::page::EditAction;

        if modifiers.command || modifiers.alt {
            return false;
        }
        // A list that is showing owns the keys that walk it, and the one that puts
        // it away.
        if let Some(page) = self.active_page_mut() {
            match key {
                Key::Escape if page.is_open() => return page.close_open(),
                Key::Enter if page.is_open() => return page.accept_open(),
                Key::Up | Key::Down if page.step_selection(key == Key::Down) => return true,
                _ => {}
            }
        }
        // A slider takes the keys that move it before anything else looks at
        // them: an arrow on a focused slider is a step, not a scroll.
        if let Some(page) = self.active_page_mut() {
            use crate::page::SliderMotion;
            let motion = match key {
                Key::Left | Key::Down => Some(SliderMotion::Down),
                Key::Right | Key::Up => Some(SliderMotion::Up),
                Key::PageUp => Some(SliderMotion::PageUp),
                Key::PageDown => Some(SliderMotion::PageDown),
                Key::Home => Some(SliderMotion::Start),
                Key::End => Some(SliderMotion::End),
                _ => None,
            };
            if let Some(motion) = motion
                && page.step_value(motion)
            {
                return true;
            }
        }
        let extend = modifiers.shift;
        let action = match key {
            Key::Backspace => EditAction::Backspace,
            Key::Delete => EditAction::Delete,
            Key::Left => EditAction::Left,
            Key::Right => EditAction::Right,
            Key::Home => EditAction::Home,
            Key::End => EditAction::End,
            _ => return false,
        };
        self.active_page_mut()
            .is_some_and(|page| page.edit_text(action, extend))
    }

    /// Editing accelerators belong to a field while that field holds the caret.
    pub(super) fn page_clipboard_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        if !modifiers.is_accelerator() || !self.active_page().is_some_and(PageScene::editing_text) {
            return false;
        }
        match key {
            Key::Character('a') => {
                self.active_page_mut().unwrap().select_all();
            }
            Key::Character('c' | 'x') => {
                if let Some(text) = self.active_page().and_then(PageScene::selected_field_text) {
                    self.clipboard.write(text);
                    if key == Key::Character('x') {
                        self.active_page_mut()
                            .unwrap()
                            .edit_text(crate::page::EditAction::Delete, false);
                    }
                }
            }
            Key::Character('v') => {
                if let Some(text) = self.clipboard.read() {
                    self.active_page_mut().unwrap().paste_text(&text);
                }
            }
            _ => return false,
        }
        true
    }

    /// The keys that take a selection on the page, or move the one there is.
    ///
    /// `true` means the key was one of them and the page has answered it.
    pub(super) fn page_selection_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        use otlyra_layout::Motion;

        if key == Key::Character('a') && modifiers.command {
            return self.active_page_mut().is_some_and(PageScene::select_all);
        }

        // Only with shift held. An arrow on a page nobody is editing scrolls it,
        // in every browser and here — turning that into a caret the moment
        // something is selected would take the page's scrolling away for as long
        // as a selection is on screen.
        if !modifiers.shift || !self.page(self.active).is_some_and(PageScene::has_selection) {
            return false;
        }

        // The command key turns a step into a jump: ⇧⌘← reaches the start of the
        // line and ⇧⌘↑ the start of the page.
        let motion = match (key, modifiers.command) {
            (Key::Left, false) => Motion::Back,
            (Key::Right, false) => Motion::Forward,
            (Key::Up, false) => Motion::Up,
            (Key::Down, false) => Motion::Down,
            (Key::Left, true) | (Key::Home, _) => Motion::LineStart,
            (Key::Right, true) | (Key::End, _) => Motion::LineEnd,
            (Key::Up, true) => Motion::Start,
            (Key::Down, true) => Motion::End,
            _ => return false,
        };

        let Some(page) = self.active_page_mut() else {
            return false;
        };
        page.move_selection(motion, true);
        true
    }

    /// Open the dialogue a file picker asked for, and hand the page what came
    /// back.
    ///
    /// The page asked and this answers, which is the whole of the split: what a
    /// reader is shown here is the machine's own dialogue where there is one, and
    /// on a machine with none the request simply goes unanswered and the control
    /// goes on saying that no file was chosen.
    pub(super) fn answer_file_request(&mut self) {
        let Some(request) = self
            .active_page_mut()
            .and_then(PageScene::take_file_request)
        else {
            return;
        };
        if let Some(sender) = &self.native_requests {
            let _ = sender.send(super::threaded::NativeRequest::Files {
                tab: self.active_id(),
                navigation: self.tabs[self.active].navigation,
                request,
            });
            return;
        }
        let chosen = choose_files(&request);
        if chosen.is_empty() {
            return;
        }
        if let Some(page) = self.active_page_mut() {
            page.set_files(request.node, chosen);
        }
    }

    /// Carry out what a screen reader asked for on a node the page owns.
    ///
    /// The identifiers the tree hands out for the page are its box ids, so the
    /// node names a box, the box names an element, and the element is pressed or
    /// focused exactly as the pointer would press or focus it — including a link,
    /// which is followed, and a button, which sends its form.
    pub(super) fn accessibility_request_on_page(
        &mut self,
        node: otlyra_platform::accesskit::NodeId,
        action: otlyra_platform::AccessibilityAction,
    ) {
        let Some(page) = self.active_page_mut() else {
            return;
        };
        // A box for nearly everything on the page, and an element for the few
        // things that generated none — an option of a drop-down nobody has opened.
        let box_id = crate::a11y::box_of(node);
        let element = match box_id {
            Some(box_id) => page.boxes().get(box_id).and_then(|found| found.node),
            None => crate::a11y::element_of(node),
        };
        let Some(element) = element else {
            tracing::debug!(?node, "an accessibility request named nothing on the page");
            return;
        };

        let changed = match action {
            otlyra_platform::AccessibilityAction::Focus => page.focus_node(element),
            // A reader asking a slider to move is the same request an arrow key
            // makes, one step further in: the focus goes to the control first, as
            // it would if the reader had reached it, and then it moves.
            otlyra_platform::AccessibilityAction::Increment
            | otlyra_platform::AccessibilityAction::Decrement => {
                let mut changed = page.focus_node(element);
                changed |= page.step_value(
                    if action == otlyra_platform::AccessibilityAction::Increment {
                        crate::page::SliderMotion::Up
                    } else {
                        crate::page::SliderMotion::Down
                    },
                );
                changed
            }
            otlyra_platform::AccessibilityAction::Activate => {
                // A link is followed rather than activated: there is no control
                // behind it, and what pressing one means is a navigation.
                if let Some(href) = box_id.and_then(|box_id| page.href_of(box_id)) {
                    let target = page.resolve(&href).unwrap_or(href);
                    self.navigate_from(&target, false);
                    return;
                }
                let changed = page.activate_node(element);
                self.follow_submission();
                self.answer_file_request();
                changed
            }
        };
        // Every event asks for a frame; what `changed` says is only whether
        // anything had to be styled again.
        let _ = changed;
    }

    /// Scroll the page by whatever a key means, if it means one.
    ///
    /// The keys every browser scrolls by, and only when nothing is being typed
    /// into: a space bar that pages down while an address is half-written is the
    /// classic way to lose what was typed.
    pub(super) fn scroll_by_key(&mut self, key: Key) {
        /// How far an arrow moves, in logical pixels.
        const LINE: f32 = 48.0;
        /// How much of the window a page key keeps, so the reader has an anchor.
        const PAGE_OVERLAP: f32 = 48.0;

        let screen = (self.last_height as f32 - UI_HEIGHT as f32 - PAGE_OVERLAP).max(LINE);
        let Some(page) = self.active_page_mut() else {
            return;
        };
        if page.editing_text() {
            return;
        }

        match key {
            Key::Down => page.scroll_by(LINE),
            Key::Up => page.scroll_by(-LINE),
            Key::PageDown | Key::Character(' ') => page.scroll_by(screen),
            Key::PageUp => page.scroll_by(-screen),
            Key::Home => page.set_scroll(0.0),
            Key::End => page.scroll_by(f32::MAX / 4.0),
            _ => {}
        }
    }

    /// Put what is selected on the page on the clipboard.
    ///
    /// Returns whether there was anything to copy, which is what decides whether
    /// the key belonged to the page or to whatever else wanted it.
    pub(super) fn copy_selection(&mut self) -> bool {
        let Some(text) = self.page(self.active).and_then(PageScene::selected_text) else {
            return false;
        };
        tracing::debug!(characters = text.len(), "copied the selection");
        self.clipboard.write(text);
        true
    }

    /// The keyboard walked off the end of the chrome: give it to the document.
    ///
    /// Only where there is one to walk. A blank tab and a browser page have
    /// nothing to hand it to, so the chrome keeps it and wraps within itself,
    /// which is what it did before there was anywhere else for it to go.
    pub(super) fn hand_keyboard_to_the_page(&mut self, forward: bool) {
        let entered = self.tabs[self.active].system.is_none()
            && self.active_page_mut().is_some_and(|page| {
                // From the end the reader is coming in at, which is what
                // makes shift-Tab out of the toolbar land on the last thing
                // in the document rather than the first.
                page.blur();
                page.focus_step(forward)
            });
        if entered {
            self.activate_surface(SURFACE_PAGE);
            self.accessibility_dirty = true;
            return;
        }
        // Nowhere to go: the chrome takes the keyboard back at its other end.
        self.ui.focus_edge(forward);
    }

    /// Work out what the pointer should look like where it now is.
    ///
    /// Computed when the pointer moves rather than when the loop asks, because
    /// the loop asks through `&self` and the answer comes from offering the
    /// interface's own tree a press it never applies — which needs the tree.
    /// Asking the tree is what keeps the cursor and the click agreeing: they are
    /// the same question put to the same rectangles.
    pub(super) fn update_cursor(&mut self, x: f64, y: f64) {
        self.cursor = if let Some(interface) = self.ui.cursor_at(x, y, &mut self.text) {
            interface
        } else if y < UI_HEIGHT || self.ui.popup_owns(x, y) {
            // Over the interface but over nothing in it.
            Cursor::Default
        } else if y >= self.dock_top() {
            match self.inspector.action_at(x, y) {
                crate::inspector::Action::None => Cursor::Default,
                _ => Cursor::Pointer,
            }
        } else if self.inspector.picking {
            // Armed, the whole page is a target, and saying so is what tells a
            // person the next click will not follow a link.
            Cursor::Pointer
        } else {
            match self.tabs[self.active].system {
                Some(SystemPage::Settings) => self.settings.cursor_at(x, y),
                Some(SystemPage::History) => self.history_page.cursor_at(x, y),
                Some(SystemPage::Downloads) => self.downloads_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::Bookmarks) => self.bookmarks_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::Cookies) => self.cookies_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::Cache) => self.cache_page.cursor_at(x, y, &mut self.text),
                Some(SystemPage::About) => self.about.cursor_at(x, y, &mut self.text),
                None if self.link_under_pointer().is_some() => Cursor::Pointer,
                None => Cursor::Default,
            }
        };
    }

    /// The link under the pointer, resolved against the document's base URL.
    ///
    /// Resolution happens here rather than at the click, because the cursor has to
    /// know as well, and a link that changes the cursor but goes nowhere — or the
    /// reverse — is worse than neither.
    pub(super) fn link_under_pointer(&self) -> Option<String> {
        let (x, y) = self.pointer;
        if !self.over_page() {
            return None;
        }
        let (x, y) = self.in_page(x, y);
        let page = self.page(self.active)?;
        let href = page.link_at(x, y)?;
        Some(page.resolve(&href).unwrap_or(href))
    }
}

/// Ask the machine for files, where the machine has a way of asking.
///
/// The dialogue is modal and blocks this thread while it is up, which is what a
/// file dialogue is everywhere: nothing else in the window can be answered until
/// the reader has chosen or dismissed it.
///
/// The bytes are read here rather than remembered as a path, because a form is
/// sent long after the dialogue closed and by then the file may have moved: what
/// was chosen is what is sent. A file too large to hold is not offered at all —
/// this is the one place the browser reads a whole file into memory, and it is
/// worth saying out loud rather than discovering.
pub(super) fn choose_files(
    request: &crate::page::FileRequest,
) -> Vec<otlyra_dom::form::ChosenFile> {
    read_chosen_files(choose_paths(request))
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(super) fn choose_paths(request: &crate::page::FileRequest) -> Vec<std::path::PathBuf> {
    let mut dialogue = rfd::FileDialog::new();
    // Extensions are the only hint the dialogue takes; a media type or a
    // `image/*` is a hint about kinds it has no list for, so those are left to it.
    let extensions: Vec<&str> = request
        .accept
        .iter()
        .filter_map(|hint| hint.strip_prefix('.'))
        .collect();
    if !extensions.is_empty() {
        dialogue = dialogue.add_filter("Accepted", &extensions);
    }
    if request.many {
        dialogue.pick_files().unwrap_or_default()
    } else {
        dialogue.pick_file().into_iter().collect()
    }
}

pub(super) fn read_chosen_files(
    paths: Vec<std::path::PathBuf>,
) -> Vec<otlyra_dom::form::ChosenFile> {
    const LARGEST: u64 = 256 * 1024 * 1024;
    paths
        .into_iter()
        .filter_map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match std::fs::metadata(&path) {
                Ok(about) if about.len() > LARGEST => {
                    tracing::warn!(file = %name, size = about.len(), "the file is too large to send");
                    return None;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(file = %name, %error, "the file could not be read");
                    return None;
                }
            }
            let bytes = std::fs::read(&path)
                .inspect_err(|error| tracing::warn!(file = %name, %error, "the file could not be read"))
                .ok()?;
            Some(otlyra_dom::form::ChosenFile {
                media_type: otlyra_dom::form::media_type_of(&name),
                name,
                bytes,
            })
        })
        .collect()
}

/// The same, where there is nothing to ask.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(super) fn choose_paths(_request: &crate::page::FileRequest) -> Vec<std::path::PathBuf> {
    tracing::debug!("no file dialogue on this platform; the picker keeps what it held");
    Vec::new()
}

/// A press on a page's element, as the page was told about it.
pub(super) struct PagePress {
    tab: super::TabId,
    target: otlyra_dom::NodeId,
    clicks: u32,
    /// The link under the press, followed only by an uncancelled click that
    /// lands on the same one.
    link: Option<String>,
}

impl Browser {
    /// Where the pointer is in the page's client coordinates, and the element
    /// under it, when it is over the active page's document.
    fn page_point(&self) -> Option<(f64, f64, otlyra_dom::NodeId)> {
        // A hidden interface owns nothing: the page is the whole window.
        if self.chrome_owns_pointer()
            || self.pointer.1 < f64::from(self.page_top()) * f64::from(self.zoom)
            || self.tabs[self.active].system.is_some()
            || (self.inspector.open && self.pointer.1 >= self.dock_top())
        {
            return None;
        }
        let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
        let target = self.page(self.active)?.element_at(x, y)?;
        Some((x, y - f64::from(self.page_top()), target))
    }

    fn pointer_init(client_x: f64, client_y: f64, buttons: u8, detail: u32) -> String {
        serde_json::json!({
            "clientX": client_x,
            "clientY": client_y,
            "screenX": client_x,
            "screenY": client_y,
            "button": 0,
            "buttons": buttons,
            "detail": detail,
            "pointerId": 1,
            "pointerType": "mouse",
            "isPrimary": true,
            "width": 1,
            "height": 1,
            "pressure": if buttons == 0 { 0.0 } else { 0.5 },
        })
        .to_string()
    }

    /// Tell the active page about a press on it. Answers whether the page
    /// cancelled it, in which case the browser neither focuses nor selects.
    pub(super) fn page_pointer_pressed(&mut self, clicks: u32) -> bool {
        self.flush_keyup();
        self.page_press = None;
        let Some((x, y, target)) = self.page_point() else {
            return false;
        };
        let tab = self.tabs[self.active].id;
        self.page_press = Some(PagePress {
            tab,
            target,
            clicks,
            link: self.link_under_pointer(),
        });
        self.sync_world();
        let init = Self::pointer_init(x, y, 1, clicks);
        // A cancelled pointerdown suppresses only the compatibility mouse
        // events, which is not a distinction any default action here makes.
        self.world.dispatch_input(tab, target, "pointerdown", &init);
        let cancelled = self.world.dispatch_input(tab, target, "mousedown", &init);
        self.apply_world_events();
        cancelled
    }

    /// Tell the page its press was let go, and click what both landed in.
    /// Answers `Some(cancelled)` when a click was dispatched; the link the
    /// press began on is followed here, after an uncancelled click.
    pub(super) fn page_pointer_released(&mut self) -> Option<bool> {
        let press = self.page_press.take()?;
        if self.tabs.get(self.active).map(|tab| tab.id) != Some(press.tab) {
            return None;
        }
        let point = self.page_point();
        let (x, y) = match point {
            Some((x, y, _)) => (x, y),
            None => {
                let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                (x, y - f64::from(self.page_top()))
            }
        };
        let released_on = point.map(|(_, _, target)| target);
        let init = Self::pointer_init(x, y, 0, press.clicks);
        self.sync_world();
        let up = released_on.unwrap_or(press.target);
        self.world.dispatch_input(press.tab, up, "pointerup", &init);
        self.world.dispatch_input(press.tab, up, "mouseup", &init);
        let clicked = released_on.and_then(|released| {
            self.page(self.active)?
                .common_ancestor(press.target, released)
        });
        // A checkbox or radio button changes before its click is heard, and
        // changes back if the click is cancelled (HTML's legacy-pre-activation).
        let (page_x, page_y) = self.in_page(self.pointer.0, self.pointer.1);
        let pre_activated = clicked
            .and_then(|_| self.active_page_mut())
            .and_then(|page| page.pre_activate_pressed(page_x, page_y));
        let cancelled =
            clicked.map(|target| self.world.dispatch_input(press.tab, target, "click", &init));
        if let Some((control, before)) = pre_activated.as_ref() {
            if cancelled == Some(true) {
                if let Some(page) = self.active_page_mut() {
                    page.restore_checkedness(before);
                }
            } else {
                self.world
                    .dispatch_input(press.tab, *control, "input", "{}");
                self.world
                    .dispatch_input(press.tab, *control, "change", "{}");
            }
        }
        if press.clicks == 2
            && let Some(target) = clicked
        {
            self.world
                .dispatch_input(press.tab, target, "dblclick", &init);
        }
        self.apply_world_events();
        if cancelled == Some(false)
            && let Some(link) = press.link
            && self.link_under_pointer().as_deref() == Some(link.as_str())
        {
            self.navigate_from(&link, false);
        }
        // The control's own activation has happened here, or must not.
        if pre_activated.is_some() {
            return Some(true);
        }
        cancelled
    }
}

/// A key as the `KeyboardEvent` init a page reads: `key`, `code`, the legacy
/// `keyCode`/`which`, and the modifiers.
fn keyboard_init(key: Key, modifiers: Modifiers) -> String {
    let (name, code, key_code): (String, String, u32) = match key {
        Key::Enter => ("Enter".into(), "Enter".into(), 13),
        Key::Backspace => ("Backspace".into(), "Backspace".into(), 8),
        Key::Delete => ("Delete".into(), "Delete".into(), 46),
        Key::Escape => ("Escape".into(), "Escape".into(), 27),
        Key::Tab => ("Tab".into(), "Tab".into(), 9),
        Key::Left => ("ArrowLeft".into(), "ArrowLeft".into(), 37),
        Key::Up => ("ArrowUp".into(), "ArrowUp".into(), 38),
        Key::Right => ("ArrowRight".into(), "ArrowRight".into(), 39),
        Key::Down => ("ArrowDown".into(), "ArrowDown".into(), 40),
        Key::Home => ("Home".into(), "Home".into(), 36),
        Key::End => ("End".into(), "End".into(), 35),
        Key::PageUp => ("PageUp".into(), "PageUp".into(), 33),
        Key::PageDown => ("PageDown".into(), "PageDown".into(), 34),
        Key::F5 => ("F5".into(), "F5".into(), 116),
        Key::Character(character) => {
            let typed = if modifiers.shift {
                character.to_uppercase().collect::<String>()
            } else {
                character.to_string()
            };
            let (code, key_code) = match character {
                'a'..='z' | 'A'..='Z' => (
                    format!("Key{}", character.to_ascii_uppercase()),
                    u32::from(character.to_ascii_uppercase()),
                ),
                '0'..='9' => (format!("Digit{character}"), u32::from(character)),
                ' ' => ("Space".to_owned(), 32),
                _ => (String::new(), 0),
            };
            (typed, code, key_code)
        }
        _ => ("Unidentified".into(), String::new(), 0),
    };
    serde_json::json!({
        "key": name,
        "code": code,
        "keyCode": key_code,
        "which": key_code,
        "ctrlKey": modifiers.control,
        "shiftKey": modifiers.shift,
        "altKey": modifiers.alt,
        "metaKey": modifiers.command,
    })
    .to_string()
}

/// Whether the platform follows this key with the text it typed.
pub(super) fn types_text(key: Key, modifiers: Modifiers) -> bool {
    matches!(key, Key::Character(_)) && !modifiers.command && !modifiers.control
}

impl Browser {
    /// The focused field of the active page and what it holds, while the page
    /// has the keyboard.
    pub(super) fn page_field_value(&self) -> Option<(super::TabId, otlyra_dom::NodeId, String)> {
        if self.keyboard_surface != SURFACE_PAGE || self.tabs[self.active].system.is_some() {
            return None;
        }
        let page = self.page(self.active)?;
        let node = page.focused_element()?;
        let value = page.focused_value()?.to_owned();
        Some((self.tabs[self.active].id, node, value))
    }

    /// Tell the active page about a key before the browser acts on it, and
    /// answer whether the page cancelled it. The browser's own accelerators
    /// are not the page's to cancel.
    pub(super) fn page_key_down(&mut self, key: Key, modifiers: Modifiers) -> bool {
        self.flush_keyup();
        self.text_cancelled = false;
        if self.keyboard_surface != SURFACE_PAGE
            || self.tabs[self.active].system.is_some()
            || modifiers.is_accelerator()
        {
            return false;
        }
        let tab = self.tabs[self.active].id;
        let Some(target) = self
            .page(self.active)
            .and_then(|page| page.focused_element().or_else(|| page.body_element()))
        else {
            return false;
        };
        let init = keyboard_init(key, modifiers);
        self.sync_world();
        let cancelled = self.world.dispatch_input(tab, target, "keydown", &init);
        self.pending_keyup = Some((tab, target, init));
        self.apply_world_events();
        cancelled
    }

    /// After the browser's own work for a key: an `input` if it changed the
    /// focused field, then the key's `keyup` unless text is still to come.
    pub(super) fn after_page_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        before: Option<(super::TabId, otlyra_dom::NodeId, String)>,
    ) {
        let input_type = match key {
            Key::Backspace => "deleteContentBackward",
            Key::Delete => "deleteContentForward",
            Key::Enter => "insertLineBreak",
            _ => "insertText",
        };
        self.field_input(before, input_type, None);
        // A character's keyup follows the text it typed, which arrives next.
        let typed = types_text(key, modifiers) && self.pending_keyup.is_some();
        if !typed {
            self.flush_keyup();
        }
    }

    /// After typed text reached the page: its `input`, then the `keyup`.
    pub(super) fn after_page_text(
        &mut self,
        character: char,
        before: Option<(super::TabId, otlyra_dom::NodeId, String)>,
    ) {
        self.field_input(before, "insertText", Some(character));
        self.flush_keyup();
    }

    fn field_input(
        &mut self,
        before: Option<(super::TabId, otlyra_dom::NodeId, String)>,
        input_type: &str,
        data: Option<char>,
    ) {
        let Some((tab, node, value)) = before else {
            return;
        };
        let changed = self
            .page_field_value()
            .is_some_and(|(now_tab, now_node, now)| {
                now_tab == tab && now_node == node && now != value
            });
        if !changed {
            return;
        }
        let init = serde_json::json!({
            "inputType": input_type,
            "data": data.map(String::from),
        })
        .to_string();
        self.sync_world();
        self.world.dispatch_input(tab, node, "input", &init);
        self.apply_world_events();
    }

    /// Send the `keyup` the last `keydown` is owed, if it still is.
    pub(super) fn flush_keyup(&mut self) {
        if let Some((tab, target, init)) = self.pending_keyup.take() {
            self.sync_world();
            self.world.dispatch_input(tab, target, "keyup", &init);
            self.apply_world_events();
        }
    }
}

impl Browser {
    /// The active page's focused element.
    pub(super) fn page_focus(&self) -> Option<(super::TabId, otlyra_dom::NodeId)> {
        let tab = self.tabs.get(self.active)?;
        if tab.system.is_some() {
            return None;
        }
        let node = self.page(self.active)?.focused_element()?;
        Some((tab.id, node))
    }

    fn control_value(&self, tab: super::TabId, node: otlyra_dom::NodeId) -> Option<String> {
        let index = self.tabs.iter().position(|candidate| candidate.id == tab)?;
        let document = self.page(index)?.document();
        otlyra_dom::form::Control::of(document, node)
            .is_some_and(otlyra_dom::form::Control::is_text_entry)
            .then(|| document.form().value(document, node).to_owned())
    }

    /// Tell the page its focus moved: `change` for a field left with a value
    /// other than the one it was given the focus with, then blur/focusout on
    /// what lost it and focus/focusin on what gained it.
    pub(super) fn sync_page_focus(&mut self) {
        let before = self.page_focus_told;
        let after = self.page_focus();
        if before == after {
            return;
        }
        self.page_focus_told = after;
        self.sync_world();
        if let Some((tab, node)) = before {
            if let Some((focused_tab, focused, value)) = self.focus_value.take()
                && (focused_tab, focused) == (tab, node)
                && self
                    .control_value(tab, node)
                    .is_some_and(|now| now != value)
            {
                self.world.dispatch_input(tab, node, "change", "{}");
            }
            self.world.dispatch_input(tab, node, "blur", "{}");
            self.world.dispatch_input(tab, node, "focusout", "{}");
        }
        if let Some((tab, node)) = after {
            self.focus_value = self
                .control_value(tab, node)
                .map(|value| (tab, node, value));
            self.world.dispatch_input(tab, node, "focus", "{}");
            self.world.dispatch_input(tab, node, "focusin", "{}");
        }
        self.apply_world_events();
    }
}

impl Browser {
    /// Tell the page the pointer moved: over/out and enter/leave when the
    /// element under it changed, and the move itself if the page listens.
    pub(super) fn page_pointer_moved(&mut self) {
        let point = self.page_point();
        let tab = self.tabs[self.active].id;
        let now = point.map(|(_, _, target)| (tab, target));
        let (x, y) = point.map_or((0.0, 0.0), |(x, y, _)| (x, y));
        let buttons = u8::from(self.page_press.is_some() || self.selecting);
        let init = Self::pointer_init(x, y, buttons, 0);
        if now != self.hovered {
            self.sync_world();
            let before = self.hovered.take();
            let ancestors = |browser: &Self, (tab, node): (super::TabId, otlyra_dom::NodeId)| {
                browser
                    .tabs
                    .iter()
                    .position(|candidate| candidate.id == tab)
                    .and_then(|index| browser.page(index))
                    .map(|page| page.element_ancestors(node))
                    .unwrap_or_default()
            };
            let left = before.map(|old| ancestors(self, old)).unwrap_or_default();
            let entered = now.map(|new| ancestors(self, new)).unwrap_or_default();
            if let Some((old_tab, old)) = before {
                self.world.dispatch_input(old_tab, old, "pointerout", &init);
                self.world.dispatch_input(old_tab, old, "mouseout", &init);
                for &node in left.iter().filter(|node| !entered.contains(node)) {
                    self.world
                        .dispatch_input(old_tab, node, "pointerleave", &init);
                    self.world
                        .dispatch_input(old_tab, node, "mouseleave", &init);
                }
            }
            if let Some((new_tab, new)) = now {
                self.world
                    .dispatch_input(new_tab, new, "pointerover", &init);
                self.world.dispatch_input(new_tab, new, "mouseover", &init);
                for &node in entered.iter().rev().filter(|node| !left.contains(node)) {
                    self.world
                        .dispatch_input(new_tab, node, "pointerenter", &init);
                    self.world
                        .dispatch_input(new_tab, node, "mouseenter", &init);
                }
            }
            self.hovered = now;
            self.apply_world_events();
        }
        if let Some((_, _, target)) = point
            && self.world.wants_pointer_moves(tab)
        {
            self.sync_world();
            self.world.dispatch_input(tab, target, "pointermove", &init);
            self.world.dispatch_input(tab, target, "mousemove", &init);
            self.apply_world_events();
        }
    }
}

impl Browser {
    /// Run an automation client's script in the page of the tab at `index`:
    /// WebDriver BiDi's `script.evaluate` and `script.callFunction`. Answers
    /// the BiDi result object as JSON text.
    pub(crate) fn automation_script(
        &mut self,
        index: usize,
        source: &str,
        call: Option<(&str, &str)>,
        await_promise: bool,
    ) -> Result<String, String> {
        let tab = self
            .tabs
            .get(index)
            .ok_or_else(|| "no such tab".to_owned())?
            .id;
        self.sync_world();
        let answer = self
            .world
            .automation_script(tab, source, call, await_promise);
        self.apply_world_events();
        answer
    }
}

impl Browser {
    /// Whether the pointer is below the browser's own interface, which is
    /// everywhere when the interface is hidden.
    pub(super) fn over_page(&self) -> bool {
        !self.interface || self.pointer.1 >= UI_HEIGHT
    }

    /// Whether the browser's interface claims the pointer: never while it is
    /// hidden, when the page is the whole window.
    pub(super) fn chrome_owns_pointer(&self) -> bool {
        self.interface && self.ui.owns_pointer()
    }
}

impl Browser {
    /// The active page and how far down it is scrolled.
    pub(super) fn page_scroll(&self) -> Option<(super::TabId, f32)> {
        let tab = self.tabs.get(self.active)?;
        Some((tab.id, self.page(self.active)?.scroll()))
    }

    /// Tell a page that listens that it scrolled: once per event, however
    /// far, which is the coalescing a frame-paced scroll event has.
    pub(super) fn page_scrolled(&mut self, before: Option<(super::TabId, f32)>) {
        let after = self.page_scroll();
        let Some((tab, _)) = after else {
            return;
        };
        if before == after || before.is_none_or(|(old, _)| old != tab) {
            return;
        }
        if !self.world.wants_scroll_events(tab) {
            return;
        }
        let Some(root) = self.page(self.active).map(|page| page.document().root()) else {
            return;
        };
        self.sync_world();
        self.world.dispatch_input(tab, root, "scroll", "{}");
        self.apply_world_events();
    }
}
