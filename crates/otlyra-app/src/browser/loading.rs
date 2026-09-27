//! The window's side of a load: what a tab shows while it waits, and what it
//! does when the pages report back.
//!
//! The load itself — the request, the parse, the scripts, everything the page
//! asks for — is the pages' (see `world`). What is here is the rest: the tab's
//! address and title, the history entry an arrival earns, a download that turned
//! up instead of a page, and a page's script asking to go somewhere else.

use crate::downloads;
use crate::fetcher::Body;
#[cfg(test)]
use crate::fetcher::Fetched;
use crate::page::PageScene;
use crate::ui::SystemPage;

use super::tabs::next_navigation_id;
use super::world::{Environment, ScriptNavigation, WorldEvent};
use super::{Browser, SURFACE_PAGE, SURFACE_SYSTEM};

impl Browser {
    /// Ask for `url` and leave the tab waiting for it.
    ///
    /// Nothing here waits: the request goes to the fetch thread and the answer
    /// arrives as an event like any other. `record` says whether reaching it should
    /// become a history entry, and `restore_scroll` where the reader should be put
    /// once it has.
    pub(super) fn start_load(
        &mut self,
        url: &str,
        user_initiated: bool,
        record: bool,
        restore_scroll: f32,
    ) {
        self.start_send(url, user_initiated, record, restore_scroll, None);
    }

    /// What the cache is allowed to do about the navigation now being started.
    ///
    /// Set for the one navigation and cleared by it, because a reload is an
    /// instruction about *this* fetch: leaving it on would make every later
    /// click behave like a reload, and a browser that never answers from its
    /// cache is a browser with no cache.
    fn take_cache_mode(&mut self) -> otlyra_net::CacheMode {
        std::mem::take(&mut self.next_cache_mode)
    }

    /// Ask for `url` with a body, and leave the tab waiting for it.
    ///
    /// The same navigation as any other in every respect but the method: the same
    /// scheme policy, the same history entry, the same pending state. What a form
    /// sends is bytes on the request rather than a different way of getting there.
    pub(super) fn start_send(
        &mut self,
        url: &str,
        user_initiated: bool,
        record: bool,
        restore_scroll: f32,
        body: Option<Body>,
    ) {
        let _span = tracing::info_span!("navigation", url).entered();
        // The document a context menu was asked about is being replaced, and
        // its rows name things in it.
        self.ui.dismiss_context_menu();
        self.context_target = None;

        // A browser's own page is fetched from nothing and parsed from nothing:
        // it is a surface this program draws. Catching it in the one place every
        // navigation passes through — the menu, the address bar, the command
        // line, and a step through the history — is what makes it a place a tab
        // can be rather than a mode the window is in.
        if let Some(page) = SystemPage::from_url(url) {
            self.show_system(page, record, restore_scroll);
            return;
        }
        self.activate_surface(SURFACE_PAGE);
        self.tabs[self.active].system = None;
        let tab = self.tabs[self.active].id;

        if !user_initiated && let Ok(target) = otlyra_net::normalize(url) {
            let from = self.tabs[self.active].url.clone();
            if !otlyra_net::may_navigate(Some(&from), &target) {
                tracing::warn!(%url, %from, "navigation refused by scheme policy");
                self.tabs[self.active].error = Some(format!("Refused to open {url} from {from}"));
                self.world.set_page(tab, None);
                return;
            }
        }

        let previous_url = self.tabs[self.active].url.clone();
        let cache = self.take_cache_mode();
        self.sync_world();
        self.world
            .start(tab, url, body, cache, previous_url, record, restore_scroll);
        self.load_started = std::time::Instant::now();

        let tab = &mut self.tabs[self.active];
        tab.url = url.to_owned();
        tab.error = None;
        tab.title = url.to_owned();
        tab.navigation = Some(next_navigation_id());
        // Through `sync_address` rather than straight into the field: the address
        // and whether this page is one the reader kept are two things the toolbar
        // draws from the same fact, and setting one without the other is how the
        // star ends up describing the page before this one.
        self.sync_address();
    }

    /// What the pages are loaded into, as the window stands now.
    pub(super) fn environment(&self) -> Environment {
        Environment {
            load_images: self.settings.settings.load_images,
            interface: self.interface,
            viewport: self.picture_viewport(),
            active: self.tabs.get(self.active).map(|tab| tab.id),
        }
    }

    /// Tell the pages what they are loaded into, before they are asked to do
    /// anything with it.
    pub(super) fn sync_world(&mut self) {
        let environment = self.environment();
        self.world.set_environment(environment);
    }

    /// One finished fetch, taken in as the loop would take it in. Returns
    /// whether it changed anything on screen.
    #[cfg(test)]
    pub(super) fn receive(&mut self, fetched: Fetched) -> bool {
        self.sync_world();
        let changed = self.world.receive(fetched);
        self.apply_world_events();
        changed
    }

    /// Act on everything the pages have done that the window has to know about.
    pub(super) fn apply_world_events(&mut self) {
        for event in self.world.take_events() {
            self.apply_world_event(event);
        }
    }

    pub(super) fn apply_world_event(&mut self, event: WorldEvent) {
        match event {
            WorldEvent::Failed { tab, error } => {
                let Some(index) = self.tab_index(tab) else {
                    return;
                };
                let tab = &mut self.tabs[index];
                tab.title = "Failed".to_owned();
                tab.error = Some(error);
            }
            WorldEvent::Arrived {
                tab,
                title,
                url,
                recorded,
            } => {
                let Some(index) = self.tab_index(tab) else {
                    return;
                };
                self.tabs[index].title = title;
                self.tabs[index].url = url;
                if index == self.active {
                    self.sync_address();
                }
                if let Some(previous) = recorded {
                    self.record_history(index, &previous);
                }
            }
            WorldEvent::Attachment {
                tab,
                filename,
                loaded,
                recorded,
            } => {
                let Some(index) = self.tab_index(tab) else {
                    return;
                };
                self.attachment_arrived(index, filename, loaded, recorded);
            }
            WorldEvent::Navigate { tab, to } => {
                if self.tab_index(tab) != Some(self.active) {
                    return;
                }
                match to {
                    ScriptNavigation::Reload => {
                        let here = self.tabs[self.active].url.clone();
                        self.start_load(&here, false, false, 0.0);
                    }
                    ScriptNavigation::Url { url, replace } => {
                        self.remember_scroll();
                        // A replacing navigation is the same fetch without the
                        // history entry, which is exactly what a redirector
                        // wants: back should go past it, not to it.
                        self.start_load(&url, false, !replace, 0.0);
                    }
                    ScriptNavigation::Submit => self.follow_submission(),
                }
            }
        }
    }

    /// What came back for a tab was an attachment: keep it as a download and
    /// show the page that names it.
    fn attachment_arrived(
        &mut self,
        index: usize,
        filename: String,
        loaded: crate::fetcher::Loaded,
        recorded: Option<String>,
    ) {
        let final_url = loaded.final_url;
        let size = loaded.bytes.len();
        let recorded_id = self.downloads.record(
            filename.clone(),
            final_url.clone(),
            loaded.content_type,
            loaded.bytes,
        );

        // The preference decides here rather than on the page: an automatic
        // download is one nobody pressed anything for, so the write has to
        // start where the bytes arrive.
        if let Some(id) = recorded_id
            && !self.settings.settings.asks_where_to_save()
            && let Some(directory) = self.settings.settings.download_directory()
            && let Some(bytes) = self.downloads.get(id).map(downloads::Download::payload)
        {
            self.start_save(
                id,
                downloads::Destination::Into {
                    directory,
                    filename: filename.clone(),
                },
                bytes,
            );
        }

        let tab = &mut self.tabs[index];
        tab.system = Some(SystemPage::Downloads);
        tab.url = SystemPage::Downloads.url().to_owned();
        tab.title = SystemPage::Downloads.label().to_owned();
        tab.error = None;
        if let Some(previous) = recorded {
            self.record_history(index, &previous);
        }
        if index == self.active {
            self.sync_address();
            self.activate_surface(SURFACE_SYSTEM);
        }
        tracing::info!(%filename, %final_url, size, "attachment downloaded");
    }

    /// The document in the tab at `index`, if it has one.
    pub(super) fn page(&self, index: usize) -> Option<&PageScene> {
        self.world.page(self.tabs.get(index)?.id)
    }

    /// The same, to change.
    pub(super) fn page_mut(&mut self, index: usize) -> Option<&mut PageScene> {
        let tab = self.tabs.get(index)?.id;
        self.world.page_mut(tab)
    }

    /// The document in the active tab, to change.
    pub(super) fn active_page_mut(&mut self) -> Option<&mut PageScene> {
        self.page_mut(self.active)
    }

    /// Whether a tab is still waiting for a stylesheet it cannot be drawn
    /// without — see `PageWorld::blocked_on_style`.
    pub(super) fn blocked_on_style(&mut self, index: usize) -> bool {
        let Some(tab) = self.tabs.get(index).map(|tab| tab.id) else {
            return false;
        };
        self.sync_world();
        self.world.blocked_on_style(tab)
    }

    /// The picture and font work that follows a frame, once the rules that name
    /// them have been computed on the way to one.
    pub(super) fn after_frame(&mut self) {
        self.sync_world();
        self.world.after_frame();
        self.apply_world_events();
    }
}
