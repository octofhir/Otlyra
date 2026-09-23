//! One tab, and the strip of them the browser keeps.
//!
//! A tab is where a document, its history and a load in flight live together,
//! named by an identity that outlasts its place in the strip. Opening, closing,
//! reordering and choosing tabs touch only that list and which entry is active, so
//! they are kept apart from what happens inside any one of them.

use crate::page::PageScene;
use crate::ui::SystemPage;

use super::loading::PendingLoad;
use super::{Browser, SURFACE_CHROME};

/// One place a tab has been.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// The address that was loaded, after redirects.
    pub url: String,
    /// How far down the reader had got when they left it. Restored on the way
    /// back, which is the difference between going back and starting over.
    pub scroll: f32,
}

/// One tab.
pub struct Tab {
    /// What this tab is, for as long as it is open.
    ///
    /// A number nobody reuses, handed out on creation. Its position in the strip
    /// is not an identity: closing a tab shifts every tab after it, and anything
    /// holding an index would then be holding a different tab without being told
    /// — which is exactly what a driver does between one command and the next.
    pub id: TabId,
    /// What the address bar shows for it.
    pub url: String,
    /// Its title, or the URL until it has one.
    pub title: String,
    /// The document, absent for a blank tab or one whose load failed.
    pub page: Option<PageScene>,
    /// The page's script world, kept for as long as the page is on screen.
    ///
    /// A page does not stop when its last byte is parsed: its timers come due,
    /// its animation frames are owed, and its listeners are waiting. Dropping
    /// the isolate at the end of the load would be a browser that runs a page
    /// once. It goes when the tab navigates away or closes, which is when the
    /// document it holds goes.
    pub(super) scripts: Option<Box<dyn otlyra_html::ScriptRunner>>,
    /// What went wrong, if anything.
    pub error: Option<String>,
    /// The browser's own page this tab is showing, if it is showing one.
    ///
    /// On the tab rather than on the browser, because `about:settings` is a
    /// place a tab can be — one tab may sit on the preferences while another
    /// reads a document, and going back from it must reach what was there.
    pub system: Option<SystemPage>,
    /// The load in flight, if one is.
    pub(super) pending: Option<PendingLoad>,
    /// What the most recent navigation in this tab is called.
    ///
    /// Kept after the load ends rather than dropped with `pending`, because the
    /// events that report a load *finishing* carry the name of the navigation
    /// that finished — and by then there is no pending load to read it from.
    pub navigation: Option<u64>,
    /// Where this tab has been, oldest first.
    ///
    /// A list and a position rather than two stacks: going back and then somewhere
    /// new drops the forward entries, and that rule is one truncation on a list
    /// instead of a second stack to keep in step.
    pub(super) history: Vec<HistoryEntry>,
    /// Which entry is showing. Meaningless while the history is empty.
    pub(super) position: usize,
    /// The page's build count when its subresources were last swept for.
    ///
    /// Asking a page what pictures and fonts it wants walks its whole box tree
    /// and its whole document, and the answer cannot change while the display
    /// list it was built from is being reused. Without this the walk happened
    /// for every open tab on every frame — sixty full document walks a second,
    /// per tab, to be told nothing new each time.
    pub(super) swept_at_build: Option<u64>,
}

/// What names a tab for as long as it is open.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

/// The next identity to hand out.
///
/// A process-wide counter rather than one per browser: two browsers in one test
/// binary handing out the same names would be two tabs a driver cannot tell
/// apart, and the numbers are cheap.
fn next_tab_id() -> TabId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    TabId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The next navigation's name, from the same kind of counter and for the same
/// reason: a driver holds it across commands and two of them must never collide.
pub(super) fn next_navigation_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// How far a tab's load has got.
///
/// The three states WebDriver lets a caller wait for, and the reason a driver
/// need not wait for the slowest picture on the page before it can act: a
/// document that has been parsed can be clicked, and everything this browser
/// fetches after that is decoration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Readiness {
    /// A load is in flight and its document has not arrived.
    Started,
    /// The document is parsed and laid out; subresources are still coming.
    Interactive,
    /// Nothing is outstanding.
    Complete,
}

impl Tab {
    /// A blank tab.
    pub fn blank() -> Self {
        Self {
            id: next_tab_id(),
            url: String::new(),
            title: "New tab".to_owned(),
            page: None,
            scripts: None,
            error: None,
            system: None,
            pending: None,
            navigation: None,
            history: Vec::new(),
            position: 0,
            swept_at_build: None,
        }
    }

    /// Whether this tab is waiting for something.
    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether there is anywhere to go back to.
    pub fn can_go_back(&self) -> bool {
        self.position > 0
    }

    /// Whether there is anywhere to go forward to.
    pub fn can_go_forward(&self) -> bool {
        self.position + 1 < self.history.len()
    }
}

impl std::fmt::Debug for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tab")
            .field("url", &self.url)
            .field("title", &self.title)
            .field("loaded", &self.page.is_some())
            .finish()
    }
}

impl Browser {
    /// Open a tab and make it active.
    pub fn new_tab(&mut self) {
        self.tabs.push(Tab::blank());
        self.active = self.tabs.len() - 1;
        self.ui.address.clear();
        self.ui.focus_address();
        self.activate_surface(SURFACE_CHROME);
    }

    /// Open a tab and say what it is called, without making it active.
    ///
    /// What a driver asks for: it creates a context and then sends commands to
    /// it by name, and whether the person watching is looking at it is a
    /// separate question with its own command.
    pub fn open_tab(&mut self) -> TabId {
        self.tabs.push(Tab::blank());
        self.tabs[self.tabs.len() - 1].id
    }

    /// Where a tab named `id` sits right now, if it is still open.
    pub fn tab_index(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// What the active tab is called.
    pub fn active_id(&self) -> TabId {
        self.tabs[self.active].id
    }

    /// Close a tab. The last one is never closed; it is emptied instead, because a
    /// window with no tabs has nothing to show and nothing to type into.
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            self.tabs[0] = Tab::blank();
            self.ui.address.clear();
            self.ui.focus_address();
            self.activate_surface(SURFACE_CHROME);
            return;
        }
        self.tabs.remove(index);
        self.active = self.active.min(self.tabs.len() - 1);
        self.sync_address();
        let surface = self.tab_surface();
        self.activate_surface(surface);
    }

    /// Put the tab named `id` at `to`, taking the tabs after it along.
    ///
    /// The strip's order is the browser's, so a drag reports the move rather
    /// than keeping an order of its own to apply on release: dropping is then
    /// letting go, and there is no second answer to what the order is if the
    /// drag is interrupted by anything at all. Which tab is being read moves
    /// with it — a tab dragged somewhere else is still the tab you were on.
    pub fn move_tab(&mut self, id: TabId, to: usize) {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let to = to.min(self.tabs.len().saturating_sub(1));
        if from == to {
            return;
        }
        let active = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .unwrap_or(self.active.min(self.tabs.len() - 1));
    }

    /// Make a tab active.
    pub fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.sync_address();
            let surface = self.tab_surface();
            self.activate_surface(surface);
        }
    }

    /// Put the active tab's URL back in the address bar.
    pub(super) fn sync_address(&mut self) {
        let url = self.tabs[self.active].url.clone();
        // Whether this page is kept is a property of the address, so it is answered
        // wherever the address is: one place, and neither the star nor the menu can
        // end up offering to keep a page that is already kept.
        self.ui.bookmark = if url.trim().is_empty() {
            crate::ui::Bookmarked::Impossible
        } else if self.bookmarks.contains(&url) {
            crate::ui::Bookmarked::Yes
        } else {
            crate::ui::Bookmarked::No
        };
        self.ui.address.set_text(url);
        self.restore_zoom();
        self.sync_find();
    }
}
