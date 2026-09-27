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
        self.sync_world();
        let mut changed = self.world.pump();
        self.apply_world_events();
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

    /// When the loop must wake for a page's timers, if it must.
    pub(super) fn next_timer_deadline(&self) -> Option<std::time::Instant> {
        self.world.next_timer_deadline()
    }
}
