//! Acting on what the browser's own interface reports.
//!
//! The toolbar, the find bar, the omnibox's suggestions and the context menu each
//! say what the reader chose and leave the doing to the browser. Every one of those
//! answers is a question about the tab or its document, so it is settled here
//! rather than in the interface that drew the control.

use std::collections::HashSet;

use crate::page::PageScene;
use crate::ui::{ContextCommand, ContextRow, UI_HEIGHT, UiAction};

use super::{Browser, SURFACE_CHROME, SURFACE_INSPECTOR, SURFACE_PAGE, TabId, ZoomStep};

/// What an open context menu was asked about.
pub(super) struct ContextTarget {
    /// Where the press landed, in window logical pixels.
    at: (f64, f64),
    /// The link under it, resolved, if it landed on one.
    link: Option<String>,
}

impl Browser {
    pub(super) fn apply(&mut self, action: UiAction) {
        match action {
            UiAction::None => {}
            // Focus and the menu belong to the interface and are settled there:
            // the press handler applies them and reports `None`, so these arms
            // are only here to keep the match honest about the whole enum.
            UiAction::Focus(_)
            | UiAction::AddressHit(_)
            | UiAction::FindHit(_)
            | UiAction::CloseFind
            | UiAction::ToggleMenu
            | UiAction::DismissPopup
            | UiAction::ScrollTabs(_) => {}
            UiAction::FindStep(forward) => self.step_match(forward),
            UiAction::ResetZoom => self.step_zoom(ZoomStep::Reset),
            UiAction::ToggleInspector => self.toggle_inspector(),
            UiAction::ToggleBookmark => self.toggle_bookmark(),
            // Chosen from the menu, a browser page opens beside what you were
            // reading rather than over it: the menu is reached *while* looking
            // at something, and losing that to check a preference is the whole
            // reason browsers open these in a tab of their own. Typing the same
            // address, which is a decision to leave, still navigates in place.
            UiAction::OpenPage(page) => self.open_system_in_new_tab(page),
            UiAction::Navigate(url) => self.navigate(&url),
            UiAction::Back => self.go_back(),
            UiAction::Forward => self.go_forward(),
            UiAction::Stop => self.stop(),
            UiAction::NewTab => self.new_tab(),
            UiAction::CloseTab(index) => self.close_tab(index),
            UiAction::SelectTab(index) => self.select_tab(index),
            UiAction::MoveTab { id, to } => self.move_tab(TabId(id), to),
            UiAction::Reload => self.reload(),
            UiAction::Context(command) => self.apply_context(command),
            UiAction::LeaveChrome(forward) => self.hand_keyboard_to_the_page(forward),
        }
    }

    /// Make the active page's search agree with the find bar, and the bar's
    /// count agree with the page.
    ///
    /// A pull rather than a push, because the bar stops being open in more ways
    /// than it starts: Escape, its own cross, and a menu opening over it are all
    /// the same answer to *is the reader still looking for something*, and a
    /// route that had to be remembered at each of them is a route that would be
    /// forgotten at the next one added.
    ///
    /// The query is searched again only when it has changed. Asking for the same
    /// one twice would be honest and would also take the reader back to the
    /// first match every time anything at all happened.
    pub(super) fn update_find(&mut self) {
        let wanted = self
            .ui
            .finding()
            .then(|| self.ui.find.text().to_owned())
            .filter(|query| !query.is_empty());
        let Some(page) = self.tabs[self.active].page.as_mut() else {
            self.ui.find_status = crate::ui::FindStatus::default();
            return;
        };
        match wanted {
            Some(query) if page.find_query() != Some(query.as_str()) => {
                page.find(&query);
            }
            Some(_) => {}
            None => {
                page.clear_find();
            }
        }
        self.ui.find_status = crate::ui::FindStatus {
            total: page.match_count(),
            current: page.current_match().map_or(0, |at| at + 1),
        };
    }

    /// Go to the next place the query occurs, or the one before it.
    fn step_match(&mut self, forward: bool) {
        if let Some(page) = self.tabs[self.active].page.as_mut() {
            page.step_match(forward);
        }
        self.update_find();
    }

    /// Show the find bar for whatever the active tab is already searching for.
    ///
    /// The search lives on the page, so a tab coming to the front brings its own
    /// query with it and a tab that is not searching brings no bar. That is what
    /// makes the bar per tab without a second copy of the query to keep in step —
    /// and it is why navigating clears it: the page a search belongs to is gone.
    pub(super) fn sync_find(&mut self) {
        let query = self.tabs[self.active]
            .page
            .as_ref()
            .and_then(PageScene::find_query)
            .map(str::to_owned);
        match query {
            Some(query) => self.ui.restore_find(&query),
            None => self.ui.close_find(),
        }
        self.update_find();
    }

    /// Offer the omnibox somewhere to go, from what has been typed.
    ///
    /// Kept pages first and then where the reader has been, newest first,
    /// because a page somebody kept is a page they meant to come back to and
    /// one they visited once may not be. Matching is on the address and on the
    /// title, without case: a person typing "otl" is as likely to be reaching
    /// for the words in the title as for the host.
    pub(super) fn refresh_suggestions(&mut self) {
        const OFFERED: usize = 6;

        if !self.ui.address_focused() {
            self.ui.set_suggestions(Vec::new());
            return;
        }
        let typed = self.ui.address.text().trim().to_lowercase();
        if typed.is_empty() {
            self.ui.set_suggestions(Vec::new());
            return;
        }
        let matches = |url: &str, title: &str| {
            url.to_lowercase().contains(&typed) || title.to_lowercase().contains(&typed)
        };

        let mut rows: Vec<crate::ui::Suggestion> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for bookmark in self.bookmarks.bookmarks() {
            if rows.len() == OFFERED {
                break;
            }
            if matches(&bookmark.url, &bookmark.title) && seen.insert(bookmark.url.clone()) {
                rows.push(crate::ui::Suggestion {
                    title: bookmark.title.clone(),
                    url: bookmark.url.clone(),
                    kept: true,
                });
            }
        }
        for visit in self.history.visits() {
            if rows.len() == OFFERED {
                break;
            }
            if matches(&visit.url, &visit.title) && seen.insert(visit.url.clone()) {
                rows.push(crate::ui::Suggestion {
                    title: visit.title.clone(),
                    url: visit.url.clone(),
                    kept: false,
                });
            }
        }
        self.ui.set_suggestions(rows);
    }

    /// Offer a menu for whatever the reader asked over.
    ///
    /// The rows are decided here rather than in the interface because every one
    /// of them is a question about the document: is there a link under the
    /// pointer, is anything selected, is there anywhere to go back to. What is
    /// under the pointer is also remembered here, so the row says *what* and
    /// this says *to what* — a menu row that carried a URL would be a second
    /// copy of a fact the browser already holds.
    pub(super) fn context_menu_requested(&mut self) {
        let (x, y) = self.pointer;
        // The interface has its own menu and the panel has none: a press for a
        // menu that lands on either dismisses whatever is open and stops there,
        // rather than offering the page's rows over something that is not a page.
        self.ui.dismiss_popup();
        if !self.interface && y < UI_HEIGHT {
            return;
        }
        if y < UI_HEIGHT || y >= self.dock_top() {
            return;
        }
        if self.tabs[self.active].page.is_none() {
            return;
        }

        let link = self.link_under_pointer();
        let selection = self.tabs[self.active]
            .page
            .as_ref()
            .is_some_and(PageScene::has_selection);
        let mut rows = Vec::new();
        if link.is_some() {
            rows.push(ContextRow::Command(ContextCommand::OpenLinkInNewTab, true));
            rows.push(ContextRow::Command(ContextCommand::CopyLinkAddress, true));
            rows.push(ContextRow::Divider);
        }
        if selection {
            rows.push(ContextRow::Command(ContextCommand::CopySelection, true));
            rows.push(ContextRow::Divider);
        }
        rows.push(ContextRow::Command(
            ContextCommand::Back,
            self.can_go_back(),
        ));
        rows.push(ContextRow::Command(
            ContextCommand::Forward,
            self.can_go_forward(),
        ));
        rows.push(ContextRow::Command(ContextCommand::Reload, true));
        rows.push(ContextRow::Divider);
        rows.push(ContextRow::Command(ContextCommand::SelectAll, true));
        rows.push(ContextRow::Command(ContextCommand::InspectElement, true));

        self.context_target = Some(ContextTarget { at: (x, y), link });
        self.ui.open_context_menu(x, y, rows);
        self.activate_surface(SURFACE_CHROME);
    }

    /// Do what a row of the context menu says, to what it was asked about.
    fn apply_context(&mut self, command: ContextCommand) {
        // Where the press landed, as it was when the menu opened. A menu that
        // read the pointer now would act on wherever the pointer drifted to
        // between opening the menu and choosing a row.
        let Some(target) = self.context_target.take() else {
            return;
        };
        match command {
            ContextCommand::OpenLinkInNewTab => {
                if let Some(url) = target.link {
                    self.new_tab();
                    self.navigate(&url);
                }
            }
            ContextCommand::CopyLinkAddress => {
                if let Some(url) = target.link {
                    self.clipboard.write(url);
                }
            }
            ContextCommand::CopySelection => {
                self.copy_selection();
            }
            ContextCommand::SelectAll => {
                if let Some(page) = self.tabs[self.active].page.as_mut() {
                    page.select_all();
                }
                self.activate_surface(SURFACE_PAGE);
            }
            ContextCommand::Back => self.go_back(),
            ContextCommand::Forward => self.go_forward(),
            ContextCommand::Reload => self.reload(),
            ContextCommand::InspectElement => {
                if !self.inspector.open {
                    self.toggle_inspector();
                }
                self.pick_at(target.at.0, target.at.1);
                self.activate_surface(SURFACE_INSPECTOR);
            }
        }
    }
}
