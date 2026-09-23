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
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            match key {
                Key::Escape if page.is_open() => return page.close_open(),
                Key::Enter if page.is_open() => return page.accept_open(),
                Key::Up | Key::Down if page.step_selection(key == Key::Down) => return true,
                _ => {}
            }
        }
        // A slider takes the keys that move it before anything else looks at
        // them: an arrow on a focused slider is a step, not a scroll.
        if let Some(page) = self.tabs[self.active].page.as_mut() {
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
        self.tabs[self.active]
            .page
            .as_mut()
            .is_some_and(|page| page.edit_text(action, extend))
    }

    /// The keys that take a selection on the page, or move the one there is.
    ///
    /// `true` means the key was one of them and the page has answered it.
    pub(super) fn page_selection_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        use otlyra_layout::Motion;

        if key == Key::Character('a') && modifiers.command {
            return self.tabs[self.active]
                .page
                .as_mut()
                .is_some_and(PageScene::select_all);
        }

        // Only with shift held. An arrow on a page nobody is editing scrolls it,
        // in every browser and here — turning that into a caret the moment
        // something is selected would take the page's scrolling away for as long
        // as a selection is on screen.
        if !modifiers.shift
            || !self.tabs[self.active]
                .page
                .as_ref()
                .is_some_and(PageScene::has_selection)
        {
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

        let Some(page) = self.tabs[self.active].page.as_mut() else {
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
        let Some(request) = self.tabs[self.active]
            .page
            .as_mut()
            .and_then(PageScene::take_file_request)
        else {
            return;
        };
        let chosen = choose_files(&request);
        if chosen.is_empty() {
            return;
        }
        if let Some(page) = self.tabs[self.active].page.as_mut() {
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
        let Some(page) = self.tabs[self.active].page.as_mut() else {
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
                    let here = self.tabs[self.active].url.clone();
                    let target =
                        otlyra_net::url::resolve(&here, &href).unwrap_or_else(|| href.clone());
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

        let Some(page) = self.tabs[self.active].page.as_mut() else {
            return;
        };
        if page.editing_text() {
            return;
        }
        let screen = (self.last_height as f32 - UI_HEIGHT as f32 - PAGE_OVERLAP).max(LINE);

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
        let Some(text) = self.tabs[self.active]
            .page
            .as_ref()
            .and_then(PageScene::selected_text)
        else {
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
            && self.tabs[self.active].page.as_mut().is_some_and(|page| {
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

    /// The link under the pointer, resolved against the tab's own address.
    ///
    /// Resolution happens here rather than at the click, because the cursor has to
    /// know as well, and a link that changes the cursor but goes nowhere — or the
    /// reverse — is worse than neither.
    pub(super) fn link_under_pointer(&self) -> Option<String> {
        let (x, y) = self.pointer;
        if y < UI_HEIGHT {
            return None;
        }
        let (x, y) = self.in_page(x, y);
        let tab = self.tabs.get(self.active)?;
        let href = tab.page.as_ref()?.link_at(x, y)?;
        Some(otlyra_net::resolve(&tab.url, &href).unwrap_or(href))
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
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn choose_files(request: &crate::page::FileRequest) -> Vec<otlyra_dom::form::ChosenFile> {
    /// The most of one file the browser will hold.
    const LARGEST: u64 = 256 * 1024 * 1024;

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
    let paths = if request.many {
        dialogue.pick_files().unwrap_or_default()
    } else {
        dialogue.pick_file().into_iter().collect()
    };

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
fn choose_files(_request: &crate::page::FileRequest) -> Vec<otlyra_dom::form::ChosenFile> {
    tracing::debug!("no file dialogue on this platform; the picker keeps what it held");
    Vec::new()
}
