//! Find in page: what the reader is looking for, and where it is.
//!
//! Its own module because a search outlives the layout it was made in. Every match
//! is numbered in one layout, so the page is searched again whenever it is laid out
//! again, and the current match is handed to the reader as the selection and
//! brought on screen; that is one piece of state and the handful of things that
//! keep it true.

use otlyra_layout::Damage;

use super::{PageScene, union};

/// A search over the page's text, and where the reader has got to in it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Found {
    /// What is being looked for, as it was typed.
    ///
    /// Kept as typed rather than as it is compared, because it is what the find
    /// bar shows and what the page is searched with again after a relayout.
    query: String,
    /// Every place it occurs, in document order.
    pub(super) at: Vec<otlyra_layout::Selection>,
    /// Which of those the reader is on, counted from zero. Meaningless when there
    /// are none, which is why the count is asked first everywhere it is read.
    pub(super) current: usize,
}

impl PageScene {
    /// Look for `query` in the text of the page, and answer how many times it
    /// occurs.
    ///
    /// A case-insensitive substring of what the page reads as, and nothing more:
    /// no regular expressions, no whole-word option, no matching a letter against
    /// its accented form. The search runs against the layout the reader is looking
    /// at — the last one built — because that is what a match is a place in, and
    /// runs again by itself whenever the page is laid out again.
    ///
    /// The current match goes back to the first, which is what typing another
    /// letter into a find bar means: the answers have changed, so where the reader
    /// had got to among the old ones is not a place among these.
    pub fn find(&mut self, query: &str) -> usize {
        if query.is_empty() {
            self.clear_find();
            return 0;
        }
        let at = self
            .layout
            .as_ref()
            .map(|(_, tree)| otlyra_layout::find::matches(tree, query))
            .unwrap_or_default();
        let found = Found {
            query: query.to_owned(),
            current: 0,
            at,
        };
        let count = found.at.len();
        if self.find.as_ref() != Some(&found) {
            self.find = Some(found);
            self.damage.add(Damage::PAINT);
        }
        // Every search brings the reader to what it found, including one that
        // found what the last one did: asking again after scrolling away means
        // *take me back to it*.
        self.take_current_match();
        count
    }

    /// What is being looked for, if anything is.
    pub fn find_query(&self) -> Option<&str> {
        self.find.as_ref().map(|found| found.query.as_str())
    }

    /// How many times it occurs on the page.
    pub fn match_count(&self) -> usize {
        self.find.as_ref().map_or(0, |found| found.at.len())
    }

    /// Which match the reader is on, counted from zero, when there is one.
    pub fn current_match(&self) -> Option<usize> {
        let found = self.find.as_ref()?;
        (!found.at.is_empty()).then_some(found.current)
    }

    /// Go to the `n`th match, counting from zero and wrapping round the end.
    ///
    /// Wrapping rather than clamping because that is what stepping through a
    /// find bar does at either end, and because it is the only rule under which
    /// *next* and *previous* need no special case of their own.
    pub fn select_match(&mut self, n: usize) -> bool {
        let Some(found) = self.find.as_mut() else {
            return false;
        };
        if found.at.is_empty() {
            return false;
        }
        let current = n % found.at.len();
        if current == found.current {
            return false;
        }
        found.current = current;
        self.damage.add(Damage::PAINT);
        self.take_current_match();
        true
    }

    /// Step to the next match or the one before it, wrapping at either end.
    pub fn step_match(&mut self, forward: bool) -> bool {
        let Some(found) = self.find.as_ref() else {
            return false;
        };
        let count = found.at.len();
        if count == 0 {
            return false;
        }
        let next = if forward {
            found.current + 1
        } else {
            found.current + count - 1
        };
        self.select_match(next % count)
    }

    /// Stop looking, and answer whether anything was being looked for.
    pub fn clear_find(&mut self) -> bool {
        if self.find.take().is_none() {
            return false;
        }
        self.damage.add(Damage::PAINT);
        true
    }

    /// The rectangles every match covers, in page coordinates.
    ///
    /// One walk of the page's runs for all of them: a common word on a long page
    /// is a hundred matches, and asking each of them separately would be a
    /// hundred walks of the document.
    pub fn match_rects(&self) -> Vec<otlyra_layout::Rect> {
        let Some(found) = self.find.as_ref() else {
            return Vec::new();
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return Vec::new();
        };
        otlyra_layout::selection::rects_all(tree, &found.at)
            .into_iter()
            .flatten()
            .collect()
    }

    /// The rectangles the current match covers — more than one where it crosses
    /// the runs a styled word breaks a sentence into.
    pub fn current_match_rects(&self) -> Vec<otlyra_layout::Rect> {
        let Some(found) = self.find.as_ref() else {
            return Vec::new();
        };
        let Some(at) = found.at.get(found.current) else {
            return Vec::new();
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return Vec::new();
        };
        otlyra_layout::selection::rects(tree, *at)
    }

    /// What the current match reads as, which is what a test asserts on and what
    /// a screen reader would be told.
    pub fn current_match_text(&self) -> Option<String> {
        let found = self.find.as_ref()?;
        let at = found.at.get(found.current)?;
        let (_, tree) = self.layout.as_ref()?;
        Some(otlyra_layout::selection::text(tree, *at))
    }

    /// Search the page again, because the page has been laid out again.
    ///
    /// A run's number is a number in one layout, so the matches from the last one
    /// point at whatever now happens to hold those numbers. Where the reader had
    /// got to is kept as far as there is still something there — a resize should
    /// not send a reader back to the top of the page — and is otherwise pulled
    /// back to the last match there is.
    pub(super) fn find_again(&mut self) {
        let Some(mut found) = self.find.take() else {
            return;
        };
        if let Some((_, tree)) = self.layout.as_ref() {
            found.at = otlyra_layout::find::matches(tree, &found.query);
            found.current = found.current.min(found.at.len().saturating_sub(1));
        }
        let at = found.at.get(found.current).copied();
        self.find = Some(found);
        // What was selected was the match, and it is numbered in the layout that
        // has just been replaced. The page is not scrolled to it: a resize is not
        // a reader asking to be taken anywhere.
        if let Some(at) = at {
            self.set_selection(Some(at));
        }
    }

    /// Make the current match what is selected, and bring it on screen.
    ///
    /// Selected as well as washed, because a match and a selection are the same
    /// thing — two places in the page's text — and every browser hands the
    /// reader the current match as the selection. That is what makes ⌘C after a
    /// search copy what was found, and what leaves the last match copyable once
    /// the bar has been closed. It costs nothing to look at: the wash for the
    /// current match is drawn over the selection's, rectangle for rectangle.
    fn take_current_match(&mut self) {
        let at = self
            .find
            .as_ref()
            .and_then(|found| found.at.get(found.current).copied());
        if let Some(at) = at {
            self.set_selection(Some(at));
        }
        self.reveal_current_match();
    }

    /// Bring the current match on screen, and answer whether the page moved.
    ///
    /// The whole match rather than its first line: a phrase found across a line
    /// break is one thing to read, and reaching half of it is not reaching it.
    pub fn reveal_current_match(&mut self) -> bool {
        let Some(bounds) = self.current_match_rects().into_iter().reduce(union) else {
            return false;
        };
        self.scroll_to_rect(bounds)
    }
}
