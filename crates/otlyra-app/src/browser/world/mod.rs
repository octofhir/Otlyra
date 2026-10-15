//! The pages: everything the browser keeps and does for the documents in its
//! tabs, apart from showing them.
//!
//! A load from the request to the last thing the page asked for, the page's
//! scripts and their timers, the pictures and fonts its rules name, and the
//! documents themselves with everything style and layout made of them. None of
//! it is the window's: the window has tabs, a toolbar and a pointer, and asks
//! this for pictures of pages and tells it what the reader did to them.
//!
//! Kept apart so that it can be somewhere else. A page's script can run for a
//! second and its layout for a tenth of one, and neither is a reason for the
//! tab strip to stop answering: what is here is what goes to the page thread,
//! and what the window needs to hear back from it comes back as a
//! [`WorldEvent`] rather than as a reach into the window's own state.

use std::collections::{HashMap, HashSet};

use otlyra_dom::NodeId;
use otlyra_text::TextEngine;

use crate::fetcher::{Fetched, Fetcher};
use crate::page::PageScene;

use super::TabId;

mod fonts;
mod load;
mod subresources;

#[cfg(test)]
mod timer_tests;

#[cfg(test)]
pub(super) use subresources::IMAGE_CACHE_BUDGET;
pub(super) use subresources::ImageCache;
use subresources::PictureFetch;

use load::PendingLoad;

/// How long a caller with no event loop waits between checks for a finished fetch.
const FETCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

const TIMER_PUMP: std::time::Duration = std::time::Duration::from_millis(8);

/// One tab's document and what is in flight for it.
#[derive(Default)]
struct PageSlot {
    /// Each document keeps its zoom while another tab is selected.
    viewport_zoom: Option<f32>,
    /// The document, absent for a blank tab, a browser page, or a load that failed.
    page: Option<PageScene>,
    /// The page's script world, kept for as long as the page is on screen.
    ///
    /// A page does not stop when its last byte is parsed: its timers come due,
    /// its animation frames are owed, and its listeners are waiting. Dropping
    /// the isolate at the end of the load would be a browser that runs a page
    /// once. It goes when the tab navigates away or closes, which is when the
    /// document it holds goes.
    scripts: Option<Box<dyn otlyra_html::ScriptRunner>>,
    /// The load in flight, if one is.
    pending: Option<PendingLoad>,
    /// The page's build count when its subresources were last swept for.
    ///
    /// Asking a page what pictures and fonts it wants walks its whole box tree
    /// and its whole document, and the answer cannot change while the display
    /// list it was built from is being reused. Without this the walk happened
    /// for every open tab on every frame.
    swept_at_build: Option<u64>,
    /// Which document `page` is, so a picture fetched for the one before it is
    /// never handed to an element of this one that happens to share a node id.
    generation: u64,
    /// The pictures elements of this document have been asked for, by element
    /// and address as the markup spells it. A picture script adds or changes
    /// after the load is asked for once, whether or not it arrives.
    pictures_asked: HashSet<(NodeId, String)>,
}

/// What the pages are loaded into: the window's preferences and size as they
/// stand, which the window keeps and hands over rather than this reaching for.
#[derive(Clone, Copy, Debug)]
pub(super) struct Environment {
    pub(super) page_zoom: f32,
    /// Whether pictures are fetched at all.
    pub(super) load_images: bool,
    /// Whether the browser's interface is drawn. Without it the page is going to
    /// be compared with another browser's, and draws no scrollbars.
    pub(super) interface: bool,
    /// The window a page chooses its pictures against and holds its first frame
    /// for: how big it is and how many device pixels go to one of its pixels.
    pub(super) viewport: otlyra_css::cascade::Viewport,
    /// The tab the reader is looking at, which is the only one whose script may
    /// send it somewhere else.
    pub(super) active: Option<TabId>,
    pub(super) window_visible: bool,
}

impl Environment {
    fn visible_tab(self) -> Option<TabId> {
        self.active.filter(|_| self.window_visible)
    }
}

/// Something the pages did that the window has to act on.
#[derive(Debug)]
pub(super) enum WorldEvent {
    /// The document never came.
    Failed { tab: TabId, error: String },
    /// The document arrived and was parsed: what the tab is now called and where
    /// it is. `recorded` is where it was before, when arriving earns an entry in
    /// the history.
    Arrived {
        tab: TabId,
        title: String,
        url: String,
        recorded: Option<String>,
    },
    /// What came back was an attachment, which is a download and not a page.
    Attachment {
        tab: TabId,
        filename: String,
        loaded: crate::fetcher::Loaded,
        recorded: Option<String>,
    },
    /// The page's script asked to go somewhere, and the hop is allowed.
    Navigate { tab: TabId, to: ScriptNavigation },
}

/// Where a page's script asked to go, resolved against the page.
#[derive(Debug)]
pub(super) enum ScriptNavigation {
    /// `location.reload()`.
    Reload,
    /// `location = …`, already resolved; `replace` says whether it takes the
    /// place of the entry it leaves rather than following it.
    Url { url: String, replace: bool },
    /// `form.submit()`, already staged on the page.
    Submit,
}

/// The pages, and everything that is fetched and run for them.
pub(super) struct PageWorld {
    environment_changed: bool,
    timer_cursor: usize,
    /// The engine pages are shaped with: a fork of the window's, so that the
    /// fonts a page brings are the pages' and the window's text is not held up
    /// by a page's layout.
    pub(super) text: TextEngine,
    pub(super) fetcher: Fetcher,
    /// Pictures that have already been decoded.
    images: ImageCache,
    /// Each open tab's document, by the tab's name.
    slots: HashMap<TabId, PageSlot>,
    environment: Environment,
    /// Background pictures asked for, so none is asked for twice.
    background_requests: HashMap<String, TabId>,
    /// How many navigations in a row page script has asked for.
    ///
    /// A redirector page sends the reader on, and the page it sends them to may
    /// send them on again — that is what a sign-in bounce is. A page that does
    /// it forever is a loop, and this is where it stops being our problem.
    script_hops: u8,
    /// Background fetches in flight, by request number.
    background_fetches: HashMap<u64, (TabId, String)>,
    /// Fetches for a picture an element chose again, by request number, with the
    /// tab, the element and the address as its markup spells it.
    picture_fetches: HashMap<u64, PictureFetch>,
    /// The last document generation handed to a slot.
    generations: u64,
    /// The window the pictures on screen were last chosen against.
    ///
    /// The choice is a question about the window, so it is put again only when
    /// the window is a different one — which keeps a walk of every document off
    /// the ordinary frame.
    picture_window: Option<(f32, f32)>,
    /// The `@font-face` faces pages have started loading, so none is loaded
    /// twice: the same rule on two pages, or seen again on the next frame.
    requested_faces: Vec<otlyra_css::cascade::FontFace>,
    /// Face fetches in flight, by request number.
    font_fetches: HashMap<u64, fonts::FaceLoad>,
    /// Legacy elapsed clock for runners without their own document origin.
    started: std::time::Instant,
    /// What has happened that the window has not been told.
    events: Vec<WorldEvent>,
}

impl PageWorld {
    /// No pages yet, fetching through `fetcher` and shaping with `text`.
    pub(super) fn new(fetcher: Fetcher, text: TextEngine, environment: Environment) -> Self {
        Self {
            environment_changed: false,
            timer_cursor: 0,
            text,
            fetcher,
            images: ImageCache::default(),
            slots: HashMap::new(),
            environment,
            background_requests: HashMap::new(),
            script_hops: 0,
            background_fetches: HashMap::new(),
            picture_fetches: HashMap::new(),
            generations: 0,
            picture_window: None,
            requested_faces: Vec::new(),
            font_fetches: HashMap::new(),
            started: std::time::Instant::now(),
            events: Vec::new(),
        }
    }

    /// Take the window's preferences and size as they stand now.
    pub(super) fn set_environment(&mut self, environment: Environment) {
        let mut viewport_changed = self.environment.viewport.width != environment.viewport.width
            || self.environment.viewport.height != environment.viewport.height
            || self.environment.viewport.scale != environment.viewport.scale;
        let previous = self.environment.visible_tab();
        self.environment = environment;
        if let Some(tab) = environment.active
            && let Some(slot) = self.slots.get_mut(&tab)
            && slot.viewport_zoom != Some(environment.page_zoom)
        {
            slot.viewport_zoom = Some(environment.page_zoom);
            viewport_changed = true;
        }
        let visible = environment.visible_tab();
        if previous != visible {
            // Hide the old page before exposing the new one. This runs between
            // turns on content's owner, including when input is batched.
            if let Some(tab) = previous {
                self.set_hidden(tab, true);
            }
            if let Some(tab) = visible {
                self.set_hidden(tab, false);
            }
        }
        if viewport_changed {
            for slot in self.slots.values_mut() {
                let Some(page) = slot.page.as_mut() else {
                    continue;
                };
                let mut viewport = environment.viewport;
                let zoom = slot.viewport_zoom.unwrap_or(1.0);
                viewport.width /= zoom;
                viewport.height /= zoom;
                viewport.scale *= zoom;
                let mutated = if let Some(scripts) = slot.scripts.as_mut() {
                    page.with_document(|document| {
                        scripts.set_viewport(
                            document,
                            f64::from(viewport.width),
                            f64::from(viewport.height),
                            f64::from(viewport.scale),
                        )
                    });
                    scripts.take_mutated()
                } else if let Some(pending) = slot.pending.as_mut() {
                    page.with_document(|document| pending.set_viewport(document, viewport))
                } else {
                    false
                };
                if mutated {
                    page.document_changed();
                    self.environment_changed = true;
                }
            }
        }
    }

    fn set_hidden(&mut self, tab: TabId, hidden: bool) {
        let Some(slot) = self.slots.get_mut(&tab) else {
            return;
        };
        let Some(page) = slot.page.as_mut() else {
            return;
        };
        let mutated = if let Some(scripts) = slot.scripts.as_mut() {
            page.with_document(|document| scripts.set_hidden(document, hidden));
            scripts.take_mutated()
        } else if let Some(pending) = slot.pending.as_mut() {
            page.with_document(|document| pending.set_hidden(document, hidden))
        } else {
            false
        };
        if mutated {
            page.document_changed();
            self.environment_changed = true;
        }
    }

    /// What has happened since the window was last told.
    pub(super) fn take_events(&mut self) -> Vec<WorldEvent> {
        std::mem::take(&mut self.events)
    }

    /// The document in `tab`, if it has one.
    pub(super) fn page(&self, tab: TabId) -> Option<&PageScene> {
        self.slots.get(&tab)?.page.as_ref()
    }

    /// The same, to change.
    pub(super) fn page_mut(&mut self, tab: TabId) -> Option<&mut PageScene> {
        self.slots.get_mut(&tab)?.page.as_mut()
    }

    /// The document in `tab` and the engine it is shaped with, together — what
    /// laying it out needs.
    pub(super) fn page_and_text(
        &mut self,
        tab: TabId,
    ) -> Option<(&mut PageScene, &mut TextEngine)> {
        let page = self.slots.get_mut(&tab)?.page.as_mut()?;
        Some((page, &mut self.text))
    }

    /// Put `page` in `tab`, in place of whatever was there and of any load in
    /// flight for it.
    pub(super) fn set_page(&mut self, tab: TabId, page: Option<PageScene>) {
        self.generations += 1;
        let slot = self.slots.entry(tab).or_default();
        slot.page = page;
        slot.scripts = None;
        slot.pending = None;
        slot.swept_at_build = None;
        slot.generation = self.generations;
        slot.pictures_asked.clear();
    }

    /// Install a runner on its owner for content-boundary regression tests.
    #[cfg(test)]
    pub(super) fn set_scripts(&mut self, tab: TabId, scripts: Box<dyn otlyra_html::ScriptRunner>) {
        self.slots.entry(tab).or_default().scripts = Some(scripts);
    }

    /// Forget `tab`: its document, its scripts and whatever it was waiting for.
    pub(super) fn close(&mut self, tab: TabId) {
        self.slots.remove(&tab);
    }

    /// Stop `tab`'s load, and answer whether there was one. Whatever it asked
    /// for that still arrives is let fall: a response is only taken for a load
    /// that is still waiting for it.
    pub(super) fn stop(&mut self, tab: TabId) -> bool {
        self.slots
            .get_mut(&tab)
            .is_some_and(|slot| slot.pending.take().is_some())
    }

    /// Whether `tab` is waiting for something.
    pub(super) fn loading(&self, tab: TabId) -> bool {
        self.slots
            .get(&tab)
            .is_some_and(|slot| slot.pending.is_some())
    }

    /// Whether any tab is.
    pub(super) fn any_loading(&self) -> bool {
        self.slots.values().any(|slot| slot.pending.is_some())
    }

    /// How far `tab`'s load has got.
    pub(super) fn readiness(&self, tab: TabId) -> super::Readiness {
        match self.slots.get(&tab).and_then(|slot| slot.pending.as_ref()) {
            None => super::Readiness::Complete,
            Some(pending) if pending.document_arrived => super::Readiness::Interactive,
            Some(_) => super::Readiness::Started,
        }
    }

    /// Whether `tab`'s page has asked for an animation frame.
    pub(super) fn frames_pending(&self, tab: TabId) -> bool {
        self.environment.visible_tab() == Some(tab)
            && self
                .slots
                .get(&tab)
                .and_then(|slot| slot.scripts.as_ref())
                .is_some_and(|scripts| scripts.frames_pending())
    }

    /// When a page's timer next comes due, if one is waiting.
    pub(super) fn next_timer_deadline(&self) -> Option<std::time::Instant> {
        self.slots
            .values()
            .filter_map(|slot| slot.scripts.as_ref()?.next_deadline())
            .min()
    }

    /// A user navigation starts the count of script hops again: the reader has
    /// chosen where to be, and a redirect chain from there is a new one.
    pub(super) fn reset_script_hops(&mut self) {
        self.script_hops = 0;
    }

    /// Take in what has finished, run the timers that have come due and give
    /// the visible page its requested animation frame. Answers
    /// whether anything changed.
    pub(super) fn pump(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.environment_changed);
        for fetched in self.fetcher.poll() {
            changed |= self.receive(fetched);
        }
        changed |= self.run_due_timers();
        changed |= self.run_frame_callbacks();
        changed
    }

    /// Share a soft timer deadline across pages, resuming after the last served.
    ///
    /// Every open tab, not only the visible one: a background tab's clock does
    /// not stop, and a page that polls in one is a page that expects to have
    /// polled when the reader comes back to it.
    fn run_due_timers(&mut self) -> bool {
        let now = std::time::Instant::now();
        let count = self.slots.len();
        if count == 0 {
            self.timer_cursor = 0;
            return false;
        }
        let start = self.timer_cursor % count;
        let deadline = now + TIMER_PUMP;
        let mut changed = false;
        for wrapped in [false, true] {
            if wrapped && start == 0 {
                break;
            }
            for (index, slot) in self.slots.values_mut().enumerate() {
                if (index < start) != wrapped {
                    continue;
                }
                let (Some(scripts), Some(page)) = (slot.scripts.as_mut(), slot.page.as_mut())
                else {
                    continue;
                };
                if scripts
                    .next_deadline()
                    .is_none_or(|deadline| deadline > now)
                {
                    continue;
                }
                let ran =
                    page.with_document(|document| scripts.run_due_timers_until(document, deadline));
                // The tree may be different now, so everything style and layout
                // made of it is stale. Asked of the page rather than guessed at:
                // a timer that only read the document changes nothing, and a page
                // that polls on an interval is otherwise a page that re-cascades
                // itself several times a second for nothing.
                if scripts.take_mutated() {
                    page.document_changed();
                }
                changed |= ran > 0;
                self.timer_cursor = (index + 1) % count;
                if std::time::Instant::now() >= deadline {
                    return changed;
                }
            }
        }
        self.timer_cursor = 0;
        changed
    }

    /// Dispatch a user-input event at an element of `tab`'s document, as its
    /// own script turn, and answer whether the page cancelled it. A page with
    /// no script, or one still loading, cancels nothing.
    pub(super) fn dispatch_input(
        &mut self,
        tab: TabId,
        target: NodeId,
        kind: &str,
        init: &str,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(&tab) else {
            return false;
        };
        if slot.pending.is_some() {
            return false;
        }
        let (Some(scripts), Some(page)) = (slot.scripts.as_mut(), slot.page.as_mut()) else {
            return false;
        };
        let cancelled =
            page.with_document(|document| scripts.dispatch_input(document, target, kind, init));
        if scripts.take_mutated() {
            page.document_changed();
        }
        self.follow_script_navigation(tab);
        cancelled
    }

    /// Whether `tab`'s page listens for pointer movement.
    pub(super) fn wants_pointer_moves(&self, tab: TabId) -> bool {
        self.slots.get(&tab).is_some_and(|slot| {
            slot.pending.is_none()
                && slot
                    .scripts
                    .as_ref()
                    .is_some_and(|scripts| scripts.wants_pointer_moves())
        })
    }

    /// Give only the visible page its requested animation frame.
    ///
    /// `requestAnimationFrame` means *before the next paint*, so this runs on
    /// the way into a frame rather than after one. Clock-aware runners convert
    /// the shared frame instant to their own document's origin; other runners
    /// retain the legacy world-relative elapsed timestamp.
    ///
    /// A page with no frame outstanding costs nothing here: the question is
    /// answered from a counter the bindings keep, not by entering the isolate.
    fn run_frame_callbacks(&mut self) -> bool {
        let at = std::time::Instant::now();
        let timestamp = at.saturating_duration_since(self.started).as_secs_f64() * 1000.0;
        let Some(slot) = self
            .environment
            .visible_tab()
            .and_then(|tab| self.slots.get_mut(&tab))
        else {
            return false;
        };
        let (Some(scripts), Some(page)) = (slot.scripts.as_mut(), slot.page.as_mut()) else {
            return false;
        };
        if !scripts.frames_pending() {
            return false;
        }
        let ran = page.with_document(|document| scripts.run_frame_at(document, timestamp, at));
        // A callback that only reads owes a frame, not a restyle.
        if scripts.take_mutated() {
            page.document_changed();
        }
        ran > 0
    }

    /// Wait for every load to finish, for callers with no event loop.
    pub(super) fn wait_for_load(&mut self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.any_loading() {
            // The document itself is stopped at the gate. Only a driver can let it
            // go, and a driver is answered by whichever thread called this — so
            // the wait would be for a command that cannot be read until it ends.
            let held = self.slots.values().all(|slot| {
                slot.pending
                    .as_ref()
                    .is_none_or(|pending| self.fetcher.is_held(pending.document))
            });
            if held {
                tracing::debug!("a driver is holding the document this wait was for");
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("gave up waiting for a load");
                return;
            }
            for fetched in self.fetcher.wait(remaining.min(FETCH_POLL)) {
                self.receive(fetched);
            }
            // A script asked to go somewhere: that load is the window's to start,
            // so the window has to be asked before there is anything to wait for.
            if self
                .events
                .iter()
                .any(|event| matches!(event, WorldEvent::Navigate { .. }))
            {
                return;
            }
        }
    }

    /// Wait for the pictures and fonts a frame asked for, for callers with no
    /// event loop.
    pub(super) fn wait_for_frame_resources(&mut self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while !self.background_fetches.is_empty()
            || !self.font_fetches.is_empty()
            || !self.picture_fetches.is_empty()
        {
            // Everything left is stopped at the gate, waiting for a driver — and
            // the driver is answered by this thread. Waiting here would be waiting
            // for a command nobody is left to read, which is a deadlock and not a
            // slow page. The frame goes out with what arrived.
            if self.only_held_remains() {
                tracing::debug!("a driver is holding everything this frame was waiting for");
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("gave up waiting for a background picture or a font");
                return;
            }
            for fetched in self.fetcher.wait(remaining.min(FETCH_POLL)) {
                self.receive(fetched);
            }
        }
    }

    /// Whether everything still outstanding is stopped at the gate.
    ///
    /// `false` when nothing is outstanding at all, because *nothing to wait for*
    /// is the loop's own condition and not this one's.
    fn only_held_remains(&self) -> bool {
        let mut any = false;
        for id in self
            .background_fetches
            .keys()
            .chain(self.font_fetches.keys())
            .chain(self.picture_fetches.keys())
        {
            any = true;
            if !self.fetcher.is_held(*id) {
                return false;
            }
        }
        any
    }

    /// One finished fetch. Returns whether it changed anything on screen.
    pub(super) fn receive(&mut self, fetched: Fetched) -> bool {
        if let Some(load) = self.font_fetches.remove(&fetched.id) {
            return self.face_arrived(fetched, load);
        }
        if let Some((tab, url)) = self.background_fetches.remove(&fetched.id) {
            return self.background_arrived(tab, url, fetched);
        }
        if let Some(fetch) = self.picture_fetches.remove(&fetched.id) {
            return self.picture_arrived(fetch, fetched);
        }

        let owner = self.slots.iter().find_map(|(tab, slot)| {
            slot.pending
                .as_ref()
                .is_some_and(|pending| {
                    pending.document == fetched.id || pending.outstanding.contains_key(&fetched.id)
                })
                .then_some(*tab)
        });
        let Some(tab) = owner else {
            // A load nobody is waiting for any more: the tab moved on, or closed.
            return false;
        };
        match fetched.kind {
            crate::fetcher::ResourceKind::Document => self.receive_document(tab, fetched),
            crate::fetcher::ResourceKind::Stylesheet
            | crate::fetcher::ResourceKind::Image
            | crate::fetcher::ResourceKind::Script
            | crate::fetcher::ResourceKind::Font => {
                self.receive_subresource(tab, fetched);
                true
            }
        }
    }
}
