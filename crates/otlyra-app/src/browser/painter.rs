//! The loop's side of the browser: the `Painter` it drives.
//!
//! Events arrive here and go to whichever surface owns them; frames and the
//! accessibility tree leave from here. A trait implementation is one block, so
//! this is the one place the whole route an event takes can be read top to bottom.

use otlyra_gfx::{PaintTarget, render};
use otlyra_platform::{
    Cursor, FrameRequest, Key, LayerId, LayerRect, Modifiers, Painter, PainterWork, PlatformEvent,
    Scene, SceneLayer, Viewport, Waker,
};

use crate::about;
use crate::page::PageScene;
use crate::ui::{SystemPage, UI_HEIGHT, UiAction};

use super::frame::{LAYER_CHROME, LAYER_HIGHLIGHT, LAYER_INSPECTOR, LAYER_PAGE};
use super::{
    Browser, SURFACE_CHROME, SURFACE_INSPECTOR, SURFACE_PAGE, SURFACE_SYSTEM, Tab, ZoomStep,
};

impl Painter for Browser {
    fn set_waker(&mut self, waker: Waker) {
        self.fetcher.set_waker(waker.clone());
        self.downloads_writer.set_waker(waker);
        // Anything that finished before the loop had a waker to be woken by is
        // sitting in the channel: a page asked for on the command line usually
        // arrives before the window exists.
        self.pump();
    }

    /// Continue only visible animation — but a loading background tab *is*
    /// visible: its mark in the strip turns. Everything else about a background
    /// tab wakes the loop when its model changes rather than driving the window
    /// at display pace.
    fn next_frame(&self) -> FrameRequest {
        let Some(tab) = self.tabs.get(self.active) else {
            return FrameRequest::None;
        };
        if self.tabs.iter().any(Tab::loading) {
            return FrameRequest::Vsync;
        }
        // A page mid-animation asks for the next frame at display pace, which
        // is what `requestAnimationFrame` is.
        if tab
            .scripts
            .as_ref()
            .is_some_and(|scripts| scripts.frames_pending())
        {
            return FrameRequest::Vsync;
        }
        // The caret's next half-second and the pause before a control is named:
        // whichever comes first, because one wake serves both and a loop told
        // about the later one would sleep through the earlier.
        let caret = tab
            .page
            .as_ref()
            .and_then(crate::page::PageScene::next_caret_frame);
        let tooltip = self.ui.next_tooltip_frame();
        // And whatever any page put on the clock: a timer is a reason to wake
        // exactly as an animation is, and a browser that only woke for its own
        // interface would run a page's `setTimeout` the next time the reader
        // happened to move the pointer.
        match [caret, tooltip, self.next_timer_deadline()]
            .into_iter()
            .flatten()
            .min()
        {
            Some(at) => FrameRequest::At(at),
            None => FrameRequest::None,
        }
    }

    fn work_counters(&self) -> PainterWork {
        let legacy = self.settings.builds()
            + self.history_page.builds()
            + self.downloads_page.builds()
            + self.bookmarks_page.builds()
            + self.about.builds()
            + self.inspector.builds();
        let chrome_roots = legacy + self.ui.builds();
        let retained_boundaries = self.ui.tab_builds()
            + self.ui.toolbar_builds()
            + self.inspector.header_builds()
            + self.inspector.body_builds();
        PainterWork {
            // Legacy surfaces still perform all three passes on a cache miss.
            // BrowserUi additionally reports work performed behind the retained
            // tab-strip and toolbar boundaries.
            chrome_reconciles: chrome_roots,
            chrome_layouts: chrome_roots + retained_boundaries,
            chrome_paints: chrome_roots + retained_boundaries,
            chrome_semantics: self.ui.tab_semantics_builds()
                + self.ui.toolbar_semantics_builds()
                + self.inspector.header_semantics_builds()
                + self.inspector.body_semantics_builds(),
            page_paints: self
                .tabs
                .iter()
                .filter_map(|tab| tab.page.as_ref())
                .map(PageScene::builds)
                .sum(),
        }
    }

    fn handle_event(&mut self, event: PlatformEvent) -> FrameRequest {
        let previous_pointer = self.pointer;
        let picking_before = self.inspector.picking;
        let selected_before = self.inspector.selected;
        let page_damage = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.page.as_ref())
            .map(PageScene::damage);
        self.on_event(event);

        let PlatformEvent::PointerMoved { x, y } = event else {
            self.accessibility_dirty = true;
            return FrameRequest::Now;
        };
        if previous_pointer == (x, y) {
            return FrameRequest::None;
        }

        let current_damage = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.page.as_ref())
            .map(PageScene::damage);
        let dock_top = self.dock_top();
        let chrome_changed = previous_pointer.1 < UI_HEIGHT
            || y < UI_HEIGHT
            || self.ui.popup_open()
            || self.ui.pointer_captured();
        let inspector_changed =
            self.inspector.open && (previous_pointer.1 >= dock_top || y >= dock_top);
        let picker_highlight_changed = picking_before && selected_before != self.inspector.selected;
        let system_page_changed = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.system.is_some())
            && (previous_pointer.1 >= UI_HEIGHT || y >= UI_HEIGHT);
        let request = if chrome_changed
            || inspector_changed
            || picker_highlight_changed
            || system_page_changed
            || page_damage != current_damage
        {
            FrameRequest::Now
        } else {
            FrameRequest::None
        };
        if page_damage != current_damage
            && current_damage.is_some_and(|damage| damage.contains(otlyra_layout::Damage::LAYOUT))
        {
            self.accessibility_dirty = true;
        }
        request
    }

    fn on_event(&mut self, event: PlatformEvent) {
        match event {
            // Background work finished: a fetch or an attachment write. What it
            // was is the browser's business; the loop only knows it should ask.
            PlatformEvent::Woken => {
                self.pump();
            }

            PlatformEvent::AppearanceChanged(scheme) => {
                self.scheme = scheme;
                self.apply_theme();
            }

            // A context menu hangs off a point in a window that has just
            // stopped being that window: the page reflows under it and its rows
            // go on describing what used to be there.
            PlatformEvent::Resized(viewport) => {
                if viewport.logical_width() != self.last_width
                    || viewport.logical_height() != self.last_height
                {
                    self.ui.dismiss_context_menu();
                    self.context_target = None;
                }
                self.set_viewport(viewport);
            }

            PlatformEvent::PointerMoved { x, y } => {
                self.pointer = (x, y);
                // A move can be a drag carrying something: a tab being dragged
                // along the strip reports where it should sit now.
                let action = self.ui.pointer_moved(x, y, &mut self.text);
                self.apply(action);
                self.update_cursor(x, y);
                self.inspector.pointer_moved(x, y);
                // While the picker is armed, moving over the page is enough to
                // show what would be chosen: an overlay that only appeared after
                // a click would be an overlay nobody could aim.
                if self.inspector.picking && y >= UI_HEIGHT && y < self.dock_top() {
                    self.pick_at(x, y);
                    return;
                }

                // A selection being made keeps the pointer the same way a scrollbar
                // does: what is between where the press landed and where the
                // pointer is now is what is selected, wherever it wanders.
                if self.selecting {
                    let top = self.page_top();
                    let (x, y) = self.in_page(x, y);
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        page.select_to(x as f32, y as f32, top);
                        return;
                    }
                }

                // A scrollbar being dragged keeps the pointer until it is let go,
                // wherever the pointer wanders.
                let (width, height) = self.in_page(self.last_width, self.last_height - UI_HEIGHT);
                let (_, page_y) = self.in_page(0.0, y - UI_HEIGHT);
                if let Some(page) = self.tabs[self.active].page.as_mut()
                    && page.dragging_scrollbar()
                {
                    page.drag_scrollbar(page_y as f32, width as f32, height.max(0.0) as f32);
                    return;
                }
                // The page follows the pointer: `:hover` on what it is over, and
                // the widget under it drawn as hovered. Nothing is repainted unless
                // something depends on it, which for most pages is never.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let over_page = y >= UI_HEIGHT && y < self.dock_top();
                    let (page_x, page_y) = self.in_page(x, y);
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        let _ = if over_page {
                            page.pointer_moved(page_x, page_y)
                        } else {
                            page.pointer_left()
                        };
                    }
                }
                match self.tabs[self.active].system {
                    // Moves matter to a surface that has a slider on it: that is
                    // what a drag is made of.
                    Some(SystemPage::Settings) => {
                        let action = self.settings.pointer_moved(x, y);
                        self.handle_settings_action(&action);
                    }
                    Some(SystemPage::History) => self.history_page.pointer_moved(x, y),
                    Some(SystemPage::Downloads) => self.downloads_page.pointer_moved(x, y),
                    Some(SystemPage::Bookmarks) => self.bookmarks_page.pointer_moved(x, y),
                    Some(SystemPage::Cookies) => self.cookies_page.pointer_moved(x, y),
                    Some(SystemPage::Cache) => self.cache_page.pointer_moved(x, y),
                    Some(SystemPage::About) => self.about.pointer_moved(x, y),
                    _ => {}
                }
            }

            PlatformEvent::PointerPressed { clicks } => {
                // A popup owns the press only where it is drawn — a menu's
                // sheet is drawn everywhere, a list of suggestions only under
                // the field. Outside it, the press is the page's and the list
                // goes away on the way past.
                let owned = self.ui.popup_owns(self.pointer.0, self.pointer.1);
                if !owned {
                    self.ui.dismiss_popup();
                }
                let surface = if owned || self.pointer.1 < UI_HEIGHT {
                    SURFACE_CHROME
                } else if self.inspector.open && self.pointer.1 >= self.dock_top() {
                    SURFACE_INSPECTOR
                } else if self.tabs[self.active].system.is_some() {
                    SURFACE_SYSTEM
                } else {
                    SURFACE_PAGE
                };
                self.activate_surface(surface);
                // A press below the toolbar takes the focus off the address field,
                // wherever it lands — a page, a control, a link, a scrollbar, a
                // system page, the inspector. Every one of those paths answers the
                // press and returns before the toolbar's own press handler runs, so
                // without this the caret and its selection would sit in a field the
                // reader has plainly clicked away from.
                if self.pointer.1 >= UI_HEIGHT && !owned {
                    self.ui.blur();
                }
                // The panel owns everything below its own top edge.
                if self.pointer.1 >= self.dock_top() && !owned {
                    let action = self.inspector.pointer_pressed();
                    self.apply_inspector(action);
                    return;
                }
                // Armed, a press on the page chooses an element instead of
                // following whatever is under it — which is the whole point of
                // arming it, and why the picker disarms itself afterwards.
                if self.inspector.picking && self.pointer.1 >= UI_HEIGHT {
                    self.pick_at(self.pointer.0, self.pointer.1);
                    // One press, one element: staying armed would make the next
                    // click on a link choose a node instead of following it.
                    self.inspector.picking = false;
                    return;
                }
                // A press on a scrollbar belongs to it rather than to the page
                // behind it.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let (width, height) =
                        self.in_page(self.last_width, self.last_height - UI_HEIGHT);
                    let (x, y) = self.in_page(self.pointer.0, self.pointer.1 - UI_HEIGHT);
                    if let Some(page) = self.tabs[self.active].page.as_mut()
                        && page.grab_scrollbar(
                            x as f32,
                            y as f32,
                            width as f32,
                            height.max(0.0) as f32,
                        )
                    {
                        return;
                    }
                }

                // The settings surface owns everything below the toolbar while it
                // is showing, so a press there never reaches the document behind
                // it — there is no document behind it.
                if self.pointer.1 >= UI_HEIGHT && !owned {
                    match self.tabs[self.active].system {
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            let action = self.settings.pointer_pressed(clicks);
                            self.save_preferences_if_changed(&before);
                            self.handle_settings_action(&action);
                            return;
                        }
                        Some(SystemPage::History) => {
                            let action = self.history_page.pointer_pressed(clicks);
                            self.handle_history_action(action);
                            return;
                        }
                        Some(SystemPage::Downloads) => {
                            let action = self.downloads_page.pointer_pressed(&mut self.text);
                            self.handle_downloads_action(action);
                            return;
                        }
                        Some(SystemPage::Bookmarks) => {
                            let action = self.bookmarks_page.pointer_pressed(&mut self.text);
                            self.handle_bookmarks_action(action);
                            return;
                        }
                        Some(SystemPage::Cookies) => {
                            let action = self.cookies_page.pointer_pressed(&mut self.text);
                            self.handle_cookies_action(action);
                            return;
                        }
                        Some(SystemPage::Cache) => {
                            let action = self.cache_page.pointer_pressed(&mut self.text);
                            self.handle_cache_action(action);
                            return;
                        }
                        Some(SystemPage::About) => {
                            if self.about.pointer_pressed(&mut self.text)
                                == about::Action::OpenSettings
                            {
                                self.open_system(SystemPage::Settings);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                // A link takes the press before the interface sees it, because the
                // interface has nothing in the page area to claim it — except an
                // open menu, which is drawn over the page and owns every press
                // that lands on it.
                if !self.ui.owns_pointer()
                    && let Some(url) = self.link_under_pointer()
                {
                    self.navigate_from(&url, false);
                    return;
                }
                // A control takes the press before the text under it does: pressing
                // a checkbox is not the start of selecting the word beside it.
                if !self.ui.owns_pointer()
                    && self.tabs[self.active].system.is_none()
                    && self.pointer.1 >= UI_HEIGHT
                {
                    let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                    let pressed = self.tabs[self.active]
                        .page
                        .as_mut()
                        .is_some_and(|page| page.control_under(x, y));
                    if pressed {
                        if let Some(page) = self.tabs[self.active].page.as_mut() {
                            page.clear_selection();
                            page.pointer_pressed_times(x, y, clicks);
                        }
                        return;
                    }
                    // Nothing was pressed but the page itself, so the field the
                    // reader was typing in stops being where their typing goes —
                    // the same statement the toolbar's blur above answers, made
                    // about the document instead. The press then goes on to start
                    // a selection where it landed.
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        page.blur();
                    }
                }
                // A press on the page starts a selection where it landed, and takes
                // away whatever was selected before — which is what a press on a
                // page means everywhere else.
                if !self.ui.owns_pointer() && self.tabs[self.active].system.is_none() {
                    let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                    let top = self.page_top();
                    if let Some(page) = self.tabs[self.active].page.as_mut() {
                        let (x, y) = (x as f32, y as f32);
                        // A second click takes the word and a third the block it
                        // is in; a fourth starts over, which is what the count
                        // running past three means.
                        match clicks % 3 {
                            2 => {
                                page.select_word_at(x, y, top);
                            }
                            0 if clicks > 0 => {
                                page.select_paragraph_at(x, y, top);
                            }
                            _ => {
                                page.select_from(x, y, top);
                            }
                        }
                        // A drag after a second or third click extends what that
                        // click took, from wherever it put the far end.
                        self.selecting = true;
                        return;
                    }
                }
                // The press is tested against the geometry of the last frame —
                // which is the frame the user was looking at when they pressed.
                let action = self.ui.pointer_pressed(&mut self.text, clicks);
                self.apply(action);
                // The bar's own cross closes it, and a menu opening over it
                // displaces it. Neither says so — the page's search is asked
                // about rather than told.
                self.update_find();
            }

            PlatformEvent::ContextMenuRequested => self.context_menu_requested(),

            PlatformEvent::PointerReleased => {
                self.selecting = false;
                let (x, y) = self.in_page(self.pointer.0, self.pointer.1);
                if let Some(page) = self.tabs[self.active].page.as_mut() {
                    page.release_scrollbar();
                    // A control is activated on the release and only where the
                    // press landed: a press that wanders off the checkbox before it
                    // is let go does not tick it, which is what every platform does
                    // and what makes a press a thing a reader can take back.
                    page.pointer_released(x, y);
                }
                self.follow_submission();
                self.answer_file_request();
                self.settings.pointer_released();
                self.history_page.pointer_released();
                self.ui.pointer_released();
            }

            PlatformEvent::KeyPressed { key, modifiers } => {
                // The one accelerator that is the inspector's. Alt as well as
                // the platform's own modifier, which is what every browser uses
                // and what keeps it clear of ⌘I.
                if key == Key::Character('i') && modifiers.alt && inspector_modifier(modifiers) {
                    self.toggle_inspector();
                    return;
                }
                // The zoom, before anything else reads the key: it belongs to
                // the page whatever holds the keyboard, which is the whole point
                // of being able to reach it while reading.
                if modifiers.is_accelerator()
                    && let Some(step) = match key {
                        Key::Character('=' | '+') => Some(ZoomStep::In),
                        Key::Character('-' | '_') => Some(ZoomStep::Out),
                        Key::Character('0') => Some(ZoomStep::Reset),
                        _ => None,
                    }
                {
                    self.step_zoom(step);
                    return;
                }
                // The panel takes the keys that walk its tree, but only while it
                // is the thing being looked at — and a caret in the address
                // field means the field is, however open the panel may be.
                //
                // With or without a document: a tab whose load failed still has
                // a console to filter and clear, and gating the panel's keys on
                // a page would take them away exactly when they are wanted.
                if self.keyboard_surface == SURFACE_INSPECTOR
                    && self.inspector.open
                    && self
                        .inspector
                        .key_pressed(
                            key,
                            modifiers,
                            self.tabs[self.active]
                                .page
                                .as_ref()
                                .map(PageScene::document),
                            self.clipboard.as_mut(),
                        )
                        .is_some()
                {
                    if !self.inspector.open {
                        let surface = self.tab_surface();
                        self.activate_surface(surface);
                    }
                    return;
                }
                // A browser page shown in the tab gets the key first: it is what
                // the reader is looking at, and Tab on it walks its own controls
                // rather than the toolbar's.
                if self.keyboard_surface == SURFACE_SYSTEM {
                    match self.tabs[self.active].system {
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            if let Some(action) =
                                self.settings
                                    .key_pressed(key, modifiers, self.clipboard.as_mut())
                            {
                                self.save_preferences_if_changed(&before);
                                self.handle_settings_action(&action);
                                return;
                            }
                        }
                        Some(SystemPage::History) => {
                            if let Some(action) = self.history_page.key_pressed(
                                key,
                                modifiers,
                                self.clipboard.as_mut(),
                            ) {
                                self.handle_history_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Downloads) => {
                            if let Some(action) =
                                self.downloads_page
                                    .key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_downloads_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Bookmarks) => {
                            if let Some(action) =
                                self.bookmarks_page
                                    .key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_bookmarks_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Cookies) => {
                            if let Some(action) =
                                self.cookies_page
                                    .key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_cookies_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::Cache) => {
                            if let Some(action) =
                                self.cache_page.key_pressed(key, modifiers, &mut self.text)
                            {
                                self.handle_cache_action(action);
                                return;
                            }
                        }
                        Some(SystemPage::About) => {
                            match self.about.key_pressed(key, modifiers, &mut self.text) {
                                Some(about::Action::OpenSettings) => {
                                    self.open_system(SystemPage::Settings);
                                    return;
                                }
                                Some(_) => return,
                                None => {}
                            }
                        }
                        _ => {}
                    }
                }
                // Walking the document with Tab, before anything else reads the
                // key: it is never a character, and it is what a reader without
                // a pointer moves through a page with.
                if key == Key::Tab
                    && !modifiers.is_accelerator()
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                {
                    let forward = !modifiers.shift;
                    let walked = self.tabs[self.active]
                        .page
                        .as_mut()
                        .is_some_and(|page| page.focus_step(forward));
                    if walked {
                        self.accessibility_dirty = true;
                        return;
                    }
                    // Off the end of the document, so the keyboard goes on to
                    // the browser around it — which is the whole of what a
                    // document not trapping the keyboard means.
                    self.activate_surface(SURFACE_CHROME);
                    self.ui.focus_edge(forward);
                    return;
                }

                // Return on a link the keyboard reached follows it, which is
                // the same navigation a click on it is — one route, so the two
                // cannot come to disagree about what following a link means.
                if key == Key::Enter
                    && !modifiers.is_accelerator()
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && let Some(href) = self.tabs[self.active]
                        .page
                        .as_ref()
                        .and_then(PageScene::focused_link)
                {
                    let url =
                        otlyra_net::resolve(&self.tabs[self.active].url, &href).unwrap_or(href);
                    self.navigate_from(&url, false);
                    return;
                }

                // Copying what is selected on the page, before the interface reads
                // the key: the address bar takes ⌘C for its own text only while it
                // holds the caret, and the page's selection is the one on screen.
                if key == Key::Character('c')
                    && modifiers.command
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.copy_selection()
                {
                    return;
                }

                // Selecting the page, and moving what is selected. Both go to the
                // page only while the interface does not hold the caret, for the
                // same reason ⌘C does: the address bar's own text is a selection
                // too, and there is one keyboard between them.
                // Return in a field sends the form it is in, which is why a search
                // box with nothing but a field in it works at all.
                if key == Key::Enter
                    && self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && self.tabs[self.active]
                        .page
                        .as_mut()
                        .is_some_and(PageScene::implicit_submit)
                {
                    self.follow_submission();
                    return;
                }
                // Editing what is in a field in the page, before the keys that
                // move a selection: an arrow in a focused field moves the caret
                // and not the page's selection.
                if self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && self.page_edit_key(key, modifiers)
                {
                    return;
                }
                if self.keyboard_surface == SURFACE_PAGE
                    && self.tabs[self.active].system.is_none()
                    && self.page_selection_key(key, modifiers)
                {
                    return;
                }

                let global = key == Key::F5
                    || modifiers.is_accelerator()
                    || (key == Key::Escape && self.ui.popup_open());
                if self.keyboard_surface == SURFACE_CHROME || global {
                    let typed = self.ui.address.text().to_owned();
                    let action = self.ui.key_pressed(
                        key,
                        modifiers,
                        &mut self.text,
                        self.clipboard.as_mut(),
                    );
                    let none = action == UiAction::None;
                    // Which keyboard the chrome now claims. ⌘F claims it before
                    // the field it claims it for exists — the bar is built by
                    // the next frame — so the wish counts as much as the fact.
                    let chrome = self.ui.address_focused()
                        || self.ui.find_focused()
                        || self.ui.find_wants_keyboard();
                    self.apply(action);
                    // Only when the key changed what is in the field: Escape
                    // puts the list away, and a refresh that ran anyway would
                    // put it straight back.
                    if typed != self.ui.address.text() {
                        self.refresh_suggestions();
                    }
                    // What the page is searching for is whatever the bar says,
                    // and the key may have changed either — ⌘F opened it, a
                    // letter narrowed it, Escape put it away.
                    self.update_find();
                    if chrome {
                        self.activate_surface(SURFACE_CHROME);
                    }
                    if none && self.keyboard_surface == SURFACE_PAGE {
                        self.scroll_by_key(key);
                    }
                } else if self.keyboard_surface == SURFACE_PAGE {
                    self.scroll_by_key(key);
                }
            }

            PlatformEvent::TextInput(character) => {
                // Text/IME is exclusive: a stale caret retained by a background
                // root must never get a chance after the active root declines.
                match self.keyboard_surface {
                    SURFACE_INSPECTOR => {
                        let _ = self.inspector.text_input(character);
                    }
                    SURFACE_PAGE => {
                        if self.tabs[self.active].system.is_none()
                            && let Some(page) = self.tabs[self.active].page.as_mut()
                        {
                            let _ = page.typed(&character.to_string());
                        }
                    }
                    SURFACE_SYSTEM => match self.tabs[self.active].system {
                        Some(SystemPage::History) => {
                            let _ = self.history_page.text_input(character);
                        }
                        Some(SystemPage::Settings) => {
                            let before = self.settings.settings.clone();
                            if self.settings.text_input(character) {
                                // Typing in the home field is a preference
                                // changing, one character at a time.
                                self.save_preferences_if_changed(&before);
                            }
                        }
                        _ => {}
                    },
                    SURFACE_CHROME if self.ui.text_input(character) => {
                        // What is offered is a function of what is typed, so it
                        // is settled here rather than remembered: one place
                        // that can go stale instead of two.
                        self.refresh_suggestions();
                        // And so is what the page is searching for.
                        self.update_find();
                    }
                    _ => {}
                }
            }

            // Scrolling belongs to the page unless the pointer is over the
            // interface, where there is nothing to scroll.
            //
            // Every one of these adds the delta to an offset and none of them
            // negates it. The event already says which way the reader went, and
            // a consumer that decided that for itself is how the settings came
            // to scroll the opposite way from a document.
            PlatformEvent::Scroll {
                x, y, modifiers, ..
            } => {
                // The wheel with the platform's own modifier held is a zoom
                // rather than a scroll, everywhere. One notch a step, so that a
                // hand on a trackpad does not run the whole ladder in a flick:
                // the delta says how far, and what a reader wants from this is
                // which way.
                if modifiers.is_accelerator() {
                    if y.abs() > f64::EPSILON {
                        self.step_zoom(if y < 0.0 { ZoomStep::In } else { ZoomStep::Out });
                    }
                    return;
                }
                if self.ui.owns_pointer() {
                    // The tab strip is a thing under the pointer like any other,
                    // and a strip with more tabs than it can show is a strip the
                    // wheel should move. Whichever axis the wheel reported the
                    // more of: a mouse with one wheel says `y` and a trackpad
                    // swiped sideways says `x`, and both mean the same thing to a
                    // strip that only runs one way.
                    if self.pointer.1 < crate::ui::TAB_STRIP_HEIGHT && !self.ui.popup_open() {
                        let delta = if x.abs() > y.abs() { x } else { y };
                        self.ui.scroll_tabs_by(delta);
                    }
                    return;
                }
                // The wheel goes to whatever is under the pointer, and the panel
                // is a thing under the pointer like any other.
                if self.pointer.1 >= self.dock_top() {
                    self.inspector.scroll_by(y);
                    return;
                }
                if self.tabs[self.active].system == Some(SystemPage::Settings) {
                    self.settings.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::History) {
                    self.history_page.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::Downloads) {
                    self.downloads_page.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::Bookmarks) {
                    self.bookmarks_page.scroll_by(y);
                } else if self.tabs[self.active].system == Some(SystemPage::Cookies) {
                    self.cookies_page.scroll_by(y);
                } else if let Some(page) = self.tabs[self.active].page.as_mut() {
                    // The wheel goes to whatever is under the pointer: a box that
                    // scrolls takes it first, and the page takes it once that box
                    // has reached its end.
                    let (x, pointer_y) = self.pointer;
                    // In the page's own pixels, like every other question a
                    // pointer asks it. Read from the field rather than through
                    // `in_page`, which wants a borrow the page already has.
                    let zoom = f64::from(self.zoom);
                    page.scroll_at(
                        (x / zoom) as f32,
                        ((pointer_y - UI_HEIGHT) / zoom) as f32,
                        y as f32,
                    );
                }
            }

            // The menu and the keyboard reach the same commands: one definition of
            // what each means, invoked from wherever the user found it.
            PlatformEvent::MenuCommand(id) => match crate::menu::Command::from_id(id) {
                Some(crate::menu::Command::Reload) => self.reload(),
                Some(crate::menu::Command::ReloadIgnoringCache) => self.reload_ignoring_cache(),
                Some(crate::menu::Command::Stop) => self.stop(),
                Some(crate::menu::Command::Back) => self.go_back(),
                Some(crate::menu::Command::Forward) => self.go_forward(),
                Some(crate::menu::Command::Home) => self.go_home(),
                Some(crate::menu::Command::Settings) => {
                    self.open_system_in_new_tab(SystemPage::Settings);
                }
                Some(crate::menu::Command::ShowHistory) => {
                    self.open_system_in_new_tab(SystemPage::History);
                }
                Some(crate::menu::Command::ShowDownloads) => {
                    self.open_system_in_new_tab(SystemPage::Downloads);
                }
                Some(crate::menu::Command::ShowBookmarks) => {
                    self.open_system_in_new_tab(SystemPage::Bookmarks);
                }
                Some(crate::menu::Command::ShowCookies) => {
                    self.open_system_in_new_tab(SystemPage::Cookies);
                }
                Some(crate::menu::Command::ShowCache) => {
                    self.open_system_in_new_tab(SystemPage::Cache);
                }
                Some(crate::menu::Command::ToggleBookmark) => self.toggle_bookmark(),
                Some(crate::menu::Command::ToggleDevTools) => self.toggle_inspector(),
                Some(crate::menu::Command::ZoomIn) => self.step_zoom(ZoomStep::In),
                Some(crate::menu::Command::ZoomOut) => self.step_zoom(ZoomStep::Out),
                Some(crate::menu::Command::ActualSize) => self.step_zoom(ZoomStep::Reset),
                Some(crate::menu::Command::NewTab) => self.new_tab(),
                Some(crate::menu::Command::CloseTab) => self.close_tab(self.active),
                // The editing four are the keystroke they carry, delivered as
                // one. A menu item that did the copying itself would be a second
                // answer to *what does ⌘C mean here* — and the answer depends on
                // which surface holds the keyboard, which the key path already
                // knows and this would have to learn again.
                Some(
                    command @ (crate::menu::Command::Cut
                    | crate::menu::Command::Copy
                    | crate::menu::Command::Paste
                    | crate::menu::Command::SelectAll),
                ) => {
                    let character = match command {
                        crate::menu::Command::Cut => 'x',
                        crate::menu::Command::Copy => 'c',
                        crate::menu::Command::Paste => 'v',
                        _ => 'a',
                    };
                    self.on_event(PlatformEvent::KeyPressed {
                        key: Key::Character(character),
                        modifiers: Modifiers {
                            command: cfg!(target_os = "macos"),
                            control: !cfg!(target_os = "macos"),
                            ..Modifiers::default()
                        },
                    });
                }
                Some(command) => tracing::info!(?command, "command not implemented yet"),
                None => tracing::warn!(?id, "menu reported an id no command claims"),
            },

            PlatformEvent::AccessibilityRequest { node, action } => {
                self.accessibility_dirty = true;
                // A node the page owns rather than the interface. It takes the
                // route the pointer takes — the focus first and the activation
                // behaviour after — because a reader pressing a control means what
                // a click on it means, down to the form it sends.
                let Some(index) = crate::a11y::described_index(node) else {
                    self.activate_surface(SURFACE_PAGE);
                    self.accessibility_request_on_page(node, action);
                    return;
                };

                // The description is the toolbar's controls followed by the
                // browser page's, so an index past the toolbar belongs to the
                // page. Counting rather than tagging, because the two lists are
                // built one after the other in the same frame and the count is
                // what the identifiers were handed out from.
                let toolbar = self.ui.describe().len();
                if index < toolbar {
                    self.activate_surface(SURFACE_CHROME);
                    let action = self.ui.activate_described(index, &mut self.text);
                    self.apply(action);
                    return;
                }

                let index = index - toolbar;
                self.activate_surface(SURFACE_SYSTEM);
                match self.tabs.get(self.active).and_then(|tab| tab.system) {
                    Some(SystemPage::Settings) => {
                        let before = self.settings.settings.clone();
                        let action = self.settings.activate_described(index);
                        self.save_preferences_if_changed(&before);
                        self.handle_settings_action(&action);
                    }
                    Some(SystemPage::History) => {
                        let action = self.history_page.activate_described(index);
                        self.handle_history_action(action);
                    }
                    Some(SystemPage::Downloads) => {
                        let action = self
                            .downloads_page
                            .activate_described(index, &mut self.text);
                        self.handle_downloads_action(action);
                    }
                    Some(SystemPage::Bookmarks) => {
                        let action = self
                            .bookmarks_page
                            .activate_described(index, &mut self.text);
                        self.handle_bookmarks_action(action);
                    }
                    Some(SystemPage::Cookies) => {
                        let action = self.cookies_page.activate_described(index, &mut self.text);
                        self.handle_cookies_action(action);
                    }
                    Some(SystemPage::About)
                        if self.about.activate_described(index, &mut self.text)
                            == about::Action::OpenSettings =>
                    {
                        self.open_system(SystemPage::Settings);
                    }
                    _ => {}
                }
            }

            PlatformEvent::CloseRequested => tracing::info!("close requested"),
            _ => {}
        }
    }

    fn accessibility(&mut self) -> Option<otlyra_platform::accesskit::TreeUpdate> {
        if !self.accessibility_dirty {
            return None;
        }
        self.accessibility_dirty = false;

        // Rebuilt only after something that can change semantics, geometry or
        // focus. Paint-only animation leaves the last tree valid.
        let tab = self.tabs.get(self.active)?;
        let document = match tab.page.as_ref() {
            Some(page) => crate::a11y::tree_for(page, &tab.title),
            None => crate::a11y::empty_tree(&tab.title),
        };

        // With the interface hidden there is nothing over the page, so the page
        // is the whole window and wrapping it would add a level describing a
        // toolbar that was never drawn.
        if !self.interface {
            return Some(document);
        }

        let title = tab.title.clone();
        let system = tab.system;

        // The toolbar, and then whatever is under it. A browser page is drawn by
        // its own surface, so its controls come from that surface rather than
        // from the document tree, which for an `about:` page has nothing in it.
        let mut described = self.ui.describe();
        let (page_focus, page_described) = match system {
            Some(SystemPage::Settings) => (self.settings.focused(), self.settings.describe()),
            Some(SystemPage::History) => {
                (self.history_page.focused(), self.history_page.describe())
            }
            Some(SystemPage::Downloads) => (
                self.downloads_page.focused(),
                self.downloads_page.describe(),
            ),
            Some(SystemPage::Bookmarks) => (
                self.bookmarks_page.focused(),
                self.bookmarks_page.describe(),
            ),
            Some(SystemPage::Cookies) => {
                (self.cookies_page.focused(), self.cookies_page.describe())
            }
            Some(SystemPage::About) => (self.about.focused(), self.about.describe()),
            // The pages that are still a placeholder draw no controls, so they
            // describe none.
            _ => (None, Vec::new()),
        };
        described.extend(page_described);

        // Only the active root may publish keyboard focus. Background surfaces
        // can retain render state, but never a second accessibility focus.
        let focused = match self.keyboard_surface {
            SURFACE_CHROME => self.ui.focused(),
            SURFACE_SYSTEM => page_focus,
            SURFACE_PAGE | SURFACE_INSPECTOR => None,
            _ => None,
        };

        Some(crate::a11y::window_tree(
            &described, focused, document, &title,
        ))
    }

    fn cursor(&self) -> Cursor {
        self.cursor
    }

    fn window_appearance(&self) -> Option<otlyra_platform::ColorScheme> {
        match self.settings.settings.appearance {
            crate::settings::Appearance::System => None,
            crate::settings::Appearance::Light => Some(otlyra_platform::ColorScheme::Light),
            crate::settings::Appearance::Dark => Some(otlyra_platform::ColorScheme::Dark),
        }
    }

    fn paint(&mut self, target: &mut dyn PaintTarget, viewport: Viewport) {
        let geom = self.frame_geom(viewport);

        // The page first, then the interface over it. The page is inset by the
        // interface's height and culled to what is visible, so it cannot paint
        // underneath it — but painting in this order means a future translucent
        // toolbar composites correctly rather than needing a clip.
        let page = self.page_list(&geom);
        render(&page, target);

        // The highlight goes over the page whether or not the panel is open. They
        // are two things: the overlay says *this element*, the panel says
        // everything about it.
        if let Some(highlight) = self.highlight_list(&geom) {
            render(&highlight, target);
        }

        if !self.interface {
            self.after_frame();
            return;
        }

        // The panel under the overlay, so a box that reaches the bottom of the
        // content area is covered by the dock rather than drawn over it.
        if geom.dock > 0.0 {
            let inspector = self.inspector_list(&geom);
            render(&inspector, target);
        }

        let chrome = self.chrome_list(&geom);
        render(&chrome, target);

        self.after_frame();
    }

    /// Publish the interface as retained layers so the compositor re-rasterizes
    /// and re-uploads only what moved.
    ///
    /// The layers are built by the same helpers `paint` renders, in the same
    /// order, so a full composite is pixel-for-pixel what `paint` would draw; the
    /// only addition is a device rectangle and content epoch per layer for the
    /// compositor's damage. Interface-less frames — a screenshot, `--no-interface`
    /// — keep the whole-surface path.
    fn compose(&mut self, viewport: Viewport) -> Option<Scene> {
        if !self.interface {
            return None;
        }
        let geom = self.frame_geom(viewport);

        // Device bands that tile the surface top to bottom with no seam: the
        // chrome above the content, the content, and the dock filling the rest.
        // An open browser menu is an overlay rather than a band, so its chrome
        // layer temporarily covers the viewport instead of clipping at the
        // toolbar's bottom edge.
        let dev = |value: f64| (value * geom.scale_factor).round() as u32;
        let content_top = dev(geom.top).min(viewport.height);
        let content_bottom = dev(geom.top + geom.content_height).min(viewport.height);
        let page_rect = LayerRect {
            x: 0,
            y: content_top,
            width: viewport.width,
            height: content_bottom.saturating_sub(content_top),
        };

        let mut layers = Vec::with_capacity(4);

        let page = self.page_list(&geom);
        // What the page says it changed, if it changed only part of itself: a
        // keystroke re-shapes one field, and the paragraphs around it keep the
        // pixels they have. Taken after the list is built, because building it is
        // what settles the answer.
        let page_dirty = self.page_dirty(&geom);
        layers.push(SceneLayer {
            id: LayerId(LAYER_PAGE),
            rect: page_rect,
            epoch: self.page_epoch(&geom),
            list: page,
            dirty: page_dirty,
        });

        if let Some(highlight) = self.highlight_list(&geom) {
            // The overlay draws within the content area and can spill a little
            // past a box's edges (labels, handles), so it claims the whole content
            // rect; a highlight move re-rasterizes the page under it, which only
            // happens while a person is walking the tree with the inspector.
            layers.push(SceneLayer {
                id: LayerId(LAYER_HIGHLIGHT),
                rect: page_rect,
                epoch: self.highlight_epoch(),
                list: highlight,
                dirty: None,
            });
        }

        if geom.dock > 0.0 {
            let inspector = self.inspector_list(&geom);
            layers.push(SceneLayer {
                id: LayerId(LAYER_INSPECTOR),
                rect: LayerRect {
                    x: 0,
                    y: content_bottom,
                    width: viewport.width,
                    height: viewport.height.saturating_sub(content_bottom),
                },
                epoch: self.inspector_epoch(),
                list: inspector,
                dirty: None,
            });
        }

        let chrome = self.chrome_list(&geom);
        let chrome_dirty = self.chrome_dirty(&geom);
        layers.push(SceneLayer {
            id: LayerId(LAYER_CHROME),
            rect: LayerRect {
                x: 0,
                y: 0,
                width: viewport.width,
                height: if self.ui.popup_open() {
                    viewport.height
                } else {
                    content_top
                },
            },
            epoch: self.chrome_epoch(),
            list: chrome,
            dirty: chrome_dirty,
        });

        self.after_frame();
        Some(Scene { layers })
    }
}

/// Whether these modifiers are the platform's "open the inspector" pair.
///
/// Alt and the platform's own accelerator: ⌥⌘I on macOS, Ctrl-Alt-I elsewhere,
/// which is what a person's fingers already know.
fn inspector_modifier(modifiers: Modifiers) -> bool {
    #[cfg(target_os = "macos")]
    {
        modifiers.command
    }
    #[cfg(not(target_os = "macos"))]
    {
        modifiers.control
    }
}
