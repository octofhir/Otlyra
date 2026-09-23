//! Taking in what finished while nothing was asking.
//!
//! Finished fetches, a page's timers come due, the animation frames it asked for
//! and attachment writes are all work that arrives rather than being asked for by
//! an event. The loop is woken for them and calls one method, and a frame calls the
//! same one on its way in; this is that method and what it runs.

use super::Browser;

impl Browser {
    /// Take in everything the fetch thread has finished.
    ///
    /// Called when the loop says it was woken, and by anything with no loop to be
    /// woken by. Returns whether a tab changed, which is whether a frame is worth
    /// drawing.
    pub fn pump(&mut self) -> bool {
        let finished = self.fetcher.poll();
        let mut changed = false;
        for fetched in finished {
            changed |= self.receive(fetched);
        }
        changed |= self.run_due_timers();
        changed |= self.run_frame_callbacks();
        // A fetch that finished is the only moment cookies can have changed, so
        // this is where the file catches up. Cheap when nothing did: the store
        // compares a revision before it writes anything.
        self.cookies.flush();
        for saved in self.downloads_writer.poll() {
            changed = true;
            match saved.result {
                Ok(path) => {
                    tracing::info!(file = %path.display(), "attachment saved");
                    self.downloads
                        .mark_saved(saved.id, path.to_string_lossy().into_owned());
                }
                Err(error) => {
                    tracing::warn!(%error, "attachment could not be saved");
                    self.downloads.mark_save_failed(saved.id, error);
                }
            }
        }
        changed
    }

    /// Run every page's timers that have come due.
    ///
    /// Called once a frame, and the frame is scheduled for the next deadline —
    /// see `next_frame` — so a page's `setTimeout` wakes the loop rather than
    /// waiting for something else to.
    ///
    /// Every open tab, not only the visible one: a background tab's clock does
    /// not stop, and a page that polls in one is a page that expects to have
    /// polled when the reader comes back to it.
    fn run_due_timers(&mut self) -> bool {
        let now = std::time::Instant::now();
        let mut changed = false;
        for tab in &mut self.tabs {
            let (Some(scripts), Some(page)) = (tab.scripts.as_mut(), tab.page.as_mut()) else {
                continue;
            };
            if scripts
                .next_deadline()
                .is_none_or(|deadline| deadline > now)
            {
                continue;
            }
            let ran = page.with_document(|document| scripts.run_due_timers(document));
            // The tree may be different now, so everything style and layout
            // made of it is stale. Asked of the page rather than guessed at:
            // a timer that only read the document changes nothing, and a page
            // that polls on an interval is otherwise a page that re-cascades
            // itself several times a second for nothing.
            if scripts.take_mutated() {
                page.document_changed();
            }
            changed |= ran > 0;
        }
        changed
    }

    /// Give every page that asked for an animation frame the one being built.
    ///
    /// `requestAnimationFrame` means *before the next paint*, so this runs on
    /// the way into a frame rather than after one. The timestamp is the page's
    /// own clock — milliseconds since the browser started, which is what
    /// `performance.now` reports and what every animation on the web integrates
    /// against.
    ///
    /// A page with no frame outstanding costs nothing here: the question is
    /// answered from a counter the bindings keep, not by entering the isolate.
    fn run_frame_callbacks(&mut self) -> bool {
        let timestamp = self.started.elapsed().as_secs_f64() * 1000.0;
        let mut changed = false;
        for tab in &mut self.tabs {
            let (Some(scripts), Some(page)) = (tab.scripts.as_mut(), tab.page.as_mut()) else {
                continue;
            };
            if !scripts.frames_pending() {
                continue;
            }
            let ran = page.with_document(|document| scripts.run_frame(document, timestamp));
            // Whether the callback *changed* anything, not whether it ran. An
            // animation frame that only reads — measuring, polling, deciding it
            // has nothing to do this frame — is most of what a
            // `requestAnimationFrame` loop does, and restyling the document for
            // one is how a page that draws nothing costs a whole frame.
            if scripts.take_mutated() {
                page.document_changed();
            }
            changed |= ran > 0;
        }
        changed
    }

    /// When the loop must wake for a page's timers, if it must.
    pub(super) fn next_timer_deadline(&self) -> Option<std::time::Instant> {
        self.tabs
            .iter()
            .filter_map(|tab| tab.scripts.as_ref()?.next_deadline())
            .min()
    }
}
