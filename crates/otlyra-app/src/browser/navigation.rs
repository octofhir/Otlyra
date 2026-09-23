//! Where a tab goes: the address asked for, back and forward, reload and stop.
//!
//! Every way a tab arrives somewhere — typed, followed, sent by a form, stepped to
//! through the history, or one of the browser's own pages — is decided here and
//! ends in a history entry. Once the request is on the wire the rest is the load's
//! business, so this holds the decisions and not the fetching.

use crate::fetcher::Body;
use crate::page::PageScene;
use crate::settings;
use crate::ui::SystemPage;

use super::tabs::HistoryEntry;
use super::{Browser, SURFACE_SYSTEM};

impl Browser {
    /// Load `url` into the active tab, as the reader asking for it.
    ///
    /// Nothing waits here: the request goes to the fetch thread and the answer
    /// arrives as an event, because an event loop that waits on the network is a
    /// window that has stopped painting.
    pub fn navigate(&mut self, url: &str) {
        self.navigate_from(url, true);
    }

    /// Show one of the browser's own pages in the active tab.
    ///
    /// Navigation like any other, so it earns a history entry and back reaches
    /// whatever was there before it.
    pub fn open_system(&mut self, page: SystemPage) {
        self.activate_surface(SURFACE_SYSTEM);
        self.navigate(page.url());
    }

    /// Show one of the browser's own pages in a tab of its own.
    ///
    /// A blank tab is used rather than added to: opening the settings from an
    /// empty new tab should fill it, not leave an empty one behind.
    pub fn open_system_in_new_tab(&mut self, page: SystemPage) {
        let blank = {
            let tab = &self.tabs[self.active];
            tab.url.is_empty() && tab.page.is_none() && tab.system.is_none()
        };
        if !blank {
            self.new_tab();
        }
        self.open_system(page);
    }

    /// Which of the browser's own pages the active tab is showing, if any.
    pub fn system_page(&self) -> Option<SystemPage> {
        self.tabs[self.active].system
    }

    /// Go back one entry in the active tab's history.
    ///
    /// The page is loaded again rather than kept: a document costs what it costs
    /// to hold, and a back button that works is worth more than one that is
    /// instant. Where the reader had got to is restored, which is the part they
    /// actually notice.
    pub fn go_back(&mut self) {
        self.travel(-1);
    }

    /// Go forward one entry.
    pub fn go_forward(&mut self) {
        self.travel(1);
    }

    /// Whether the active tab can go back.
    pub fn can_go_back(&self) -> bool {
        self.tabs[self.active].can_go_back()
    }

    /// Whether the active tab can go forward.
    pub fn can_go_forward(&self) -> bool {
        self.tabs[self.active].can_go_forward()
    }

    /// Move `offset` entries through the active tab's history.
    fn travel(&mut self, offset: isize) {
        let tab = &mut self.tabs[self.active];
        let Some(target) = tab.position.checked_add_signed(offset) else {
            return;
        };
        let Some(entry) = tab.history.get(target).cloned() else {
            return;
        };

        self.remember_scroll();
        self.tabs[self.active].position = target;
        // The entry was reached once, so its scheme was allowed once; going back to
        // it is the reader's own request and not the page's.
        self.start_load(&entry.url, true, false, entry.scroll);
    }

    /// Record where the reader is in the entry they are about to leave.
    pub(super) fn remember_scroll(&mut self) {
        // A browser page keeps its own, on the surface that draws it, so where
        // the number lives depends on what the tab is showing — and the history
        // entry does not care which it was.
        let settings = self.settings.settings.scroll as f32;
        let tab = &mut self.tabs[self.active];
        let scroll = match tab.system {
            Some(SystemPage::Settings) => settings,
            Some(_) => 0.0,
            None => tab.page.as_ref().map_or(0.0, |page| page.scroll()),
        };
        if let Some(entry) = tab.history.get_mut(tab.position) {
            entry.scroll = scroll;
        }
    }

    /// Load the active tab's address again, keeping where the reader had got to.
    ///
    /// Browsers restore the scroll position on reload, and for a page you are
    /// editing that is the whole value of the key: the alternative is finding your
    /// place again after every change.
    pub fn reload(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.url.is_empty() {
            return;
        }

        let url = tab.url.clone();
        let scroll = tab.page.as_ref().map_or(0.0, |page| page.scroll());
        // A reload is the reader saying they think it changed, so the stored copy
        // is not the answer however fresh it still is. It is still worth
        // consulting: the server may say nothing changed, and a reload that costs
        // a header rather than a body is the difference between a page that
        // reappears and a page that loads again.
        self.next_cache_mode = otlyra_net::CacheMode::Revalidate;
        // Reload keeps the entry it is on: a page loaded twice is one place, and
        // going back from it must reach where you were before it, not itself.
        self.start_load(&url, false, false, scroll);
    }

    /// Load the page again without consulting the cache at all.
    ///
    /// What ⌘⇧R means, and the difference from an ordinary reload is the whole
    /// reason there are two: this one does not ask whether anything changed, it
    /// fetches. What comes back is still kept — the point is a new copy, not the
    /// end of having one — and every subresource the page then asks for is
    /// fetched afresh too, which is what makes it the answer to a stylesheet a
    /// server is serving stale.
    pub fn reload_ignoring_cache(&mut self) {
        if let Some(cache) = self.cache.as_ref() {
            cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
        self.next_cache_mode = otlyra_net::CacheMode::Bypass;
        self.reload_keeping_the_mode();
    }

    /// The body of [`Browser::reload`], without setting a mode of its own.
    fn reload_keeping_the_mode(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.url.is_empty() {
            return;
        }
        let url = tab.url.clone();
        let scroll = tab.page.as_ref().map_or(0.0, |page| page.scroll());
        self.start_load(&url, false, false, scroll);
    }

    /// Stop the active navigation and reject every response that still arrives.
    ///
    /// Fetch work may already be inside a platform syscall and finish in the
    /// background. Removing its `PendingLoad` is the cancellation boundary:
    /// `receive` accepts only request ids a live tab still owns, so a late
    /// document, stylesheet, or image cannot replace what the reader kept.
    pub fn stop(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.pending.take().is_some() {
            tracing::info!(url = %tab.url, "navigation stopped");
        }
    }

    /// Go wherever a form the reader has just sent points.
    ///
    /// A form that submits is a form that navigates, and that is the whole of it
    /// without a script. An action of nothing at all means the page's own address,
    /// which is what reloads a page with its answers in the query.
    pub(super) fn follow_submission(&mut self) {
        let Some(sent) = self.tabs[self.active]
            .page
            .as_mut()
            .and_then(PageScene::take_submission)
        else {
            return;
        };
        if sent.method == otlyra_dom::Method::Dialog {
            return;
        }
        // The action is spelled as the markup spells it, so an empty one is the
        // page itself and a relative one is resolved against it.
        let here = self.tabs[self.active].url.clone();
        let target = if sent.url.is_empty() {
            here.clone()
        } else {
            otlyra_net::url::resolve(&here, &sent.url).unwrap_or_else(|| sent.url.clone())
        };
        // A form is the page acting, not the reader, so the same scheme policy that
        // holds for a link holds here: a page from the network may not aim a form
        // at a file.
        if sent.method == otlyra_dom::Method::Post {
            self.remember_scroll();
            self.start_send(
                &target,
                false,
                true,
                0.0,
                Some(Body {
                    content_type: sent.content_type,
                    bytes: sent.body,
                }),
            );
            return;
        }
        self.navigate_from(&target, false);
    }

    /// Load `url` into the active tab.
    ///
    /// `user_initiated` says whether the address came from the person rather than
    /// from the page: it is what decides whether a `file:` URL may be reached at
    /// all, and a page from the internet must never be able to claim it.
    pub(super) fn navigate_from(&mut self, url: &str, user_initiated: bool) {
        if user_initiated {
            self.script_hops = 0;
        }
        self.remember_scroll();
        self.start_load(url, user_initiated, true, 0.0);
    }

    /// Put one of the browser's own pages in the active tab.
    ///
    /// Everything a finished load does, minus the loading: the tab's address and
    /// title change, whatever was there is dropped, and the arrival earns a
    /// history entry if this navigation was the kind that earns one.
    pub(super) fn show_system(&mut self, page: SystemPage, record: bool, restore_scroll: f32) {
        if !page.available() {
            let tab = &mut self.tabs[self.active];
            tab.error = Some(format!("{} is not built yet.", page.label()));
            tracing::info!(?page, "system page requested before it exists");
            return;
        }

        let index = self.active;
        let previous_url = self.tabs[index].url.clone();
        let tab = &mut self.tabs[index];
        tab.system = Some(page);
        tab.url = page.url().to_owned();
        tab.title = page.label().to_owned();
        tab.error = None;
        tab.page = None;
        tab.pending = None;

        if record {
            self.record_history(index, &previous_url);
        }
        // A browser page is scrolled like any other, so coming back to one lands
        // where the reader left it. Set rather than added, because the surface is
        // the browser's and the last tab to use it left its own position there.
        self.settings.settings.scroll = f64::from(restore_scroll);
        self.sync_address();
        self.activate_surface(SURFACE_SYSTEM);
    }

    /// Add the entry this load earned, if it earned one.
    pub(super) fn record_history(&mut self, index: usize, previous_url: &str) {
        let tab = &mut self.tabs[index];

        // A load that did not move is not a second place: reloading a page, or
        // typing the address it is already on, adds nothing.
        if tab.url == previous_url && !tab.history.is_empty() {
            return;
        }

        // Going somewhere new after going back drops what was ahead: the forward
        // entries describe a future that did not happen.
        if !tab.history.is_empty() {
            tab.position += 1;
            tab.history.truncate(tab.position);
        }
        tab.history.push(HistoryEntry {
            url: tab.url.clone(),
            scroll: 0.0,
        });
        tab.position = tab.history.len() - 1;

        // The browser-wide record, beside the tab's own: same seam, so it is
        // once per navigation by construction — a redirect chain arrived here
        // as one final URL, and a reload returned before this line.
        let (url, title) = (tab.url.clone(), tab.title.clone());
        self.history.record(url, title, jiff::Timestamp::now());
    }

    /// Where the home button and a new window go.
    pub fn home(&self) -> String {
        self.settings.settings.home.text().to_owned()
    }

    /// What the preferences say should happen when the browser opens.
    pub fn settings_on_start(&self) -> settings::OnStart {
        self.settings.settings.on_start
    }

    /// Go where the preferences say home is.
    pub fn go_home(&mut self) {
        let home = self.home();
        if home.trim().is_empty() {
            return;
        }
        self.navigate(&home);
    }
}
