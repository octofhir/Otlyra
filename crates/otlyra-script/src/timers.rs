//! When a page's deferred work is due, and what asks for it.
//!
//! ## Whose clock
//!
//! `setTimeout` is not the engine's to run. The engine holds the callback and
//! the token; *when* the token comes due is a question about the browser's
//! event loop, which is why Otter asks the embedder for a
//! [`TimerScheduler`](otter_runtime::TimerScheduler) and refuses `setTimeout`
//! outright without one. This is that scheduler.
//!
//! It schedules nothing itself: there is no thread here and no sleeping. It
//! records deadlines and answers two questions — *when is the next one*, which
//! the browser turns into how long it may wait for the next event, and *what is
//! due now*, which the browser asks when it wakes. That keeps every callback on
//! the thread the isolate is pinned to, and it keeps the browser in charge of
//! its own loop.
//!
//! ## Ordering
//!
//! Two timers due at the same moment run in the order they were asked for,
//! which is what the specification says and what a page written as
//! `setTimeout(a, 0); setTimeout(b, 0)` depends on. The sequence number is what
//! makes that true when two deadlines land on the same instant, which at
//! millisecond resolution they constantly do.
//!
//! ## Why a `Mutex`
//!
//! The engine requires the handle to be `Send + Sync` because a Layer B
//! embedder's scheduler owns a thread pool. Ours does not — every touch happens
//! on the one thread the page lives on — but the trait is the trait, so the
//! state is behind a lock that is never contended.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otter_runtime::TimerScheduler;

/// One scheduled timer.
#[derive(Debug, Clone, Copy)]
struct Timer {
    /// The engine's token, which is what fires it.
    token: u64,
    /// `Some(period)` for `setInterval`, and what it is re-armed with.
    repeat: Option<Duration>,
}

/// The deadlines a page is waiting on.
///
/// Cheap to clone: every clone is the same wheel.
#[derive(Debug, Clone, Default)]
pub struct TimerWheel {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    /// Due time and sequence number to timer, which is the running order.
    pending: BTreeMap<(Instant, u64), Timer>,
    next_token: u64,
    next_sequence: u64,
}

/// How long a timer may be asked for. Anything longer is clamped rather than
/// refused: a page that asks for a month is a page that will be closed first,
/// and an overflowing `Instant` is a panic.
const LONGEST: Duration = Duration::from_secs(60 * 60 * 24);

impl TimerWheel {
    /// A wheel with nothing on it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// When the browser must wake to run the next one, if there is one.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let state = self.state.lock().expect("the timer wheel is never poisoned");
        state.pending.keys().next().map(|(at, _)| *at)
    }

    /// Whether anything is waiting at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .expect("the timer wheel is never poisoned")
            .pending
            .is_empty()
    }

    /// How many timers are outstanding. For the panel and for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .expect("the timer wheel is never poisoned")
            .pending
            .len()
    }

    /// The tokens due at `now`, in the order they must run.
    ///
    /// Taken off the wheel as they are returned, and a repeating one is put
    /// back with its next deadline before the caller runs it — so a callback
    /// that cancels its own interval cancels the re-armed entry rather than
    /// racing it.
    ///
    /// A repeat whose deadline has already passed by more than a period is not
    /// run once per missed period: it is re-armed from `now`. A browser that
    /// was busy for a second does not owe a `setInterval(f, 1)` a thousand
    /// calls, and every engine coalesces this way.
    #[must_use]
    pub fn due(&self, now: Instant) -> Vec<u64> {
        let mut state = self.state.lock().expect("the timer wheel is never poisoned");
        let mut due = Vec::new();
        while let Some((&key, &timer)) = state.pending.iter().next() {
            if key.0 > now {
                break;
            }
            state.pending.remove(&key);
            if let Some(period) = timer.repeat {
                let sequence = state.next_sequence;
                state.next_sequence += 1;
                let next = now + period.max(Duration::from_millis(1));
                state.pending.insert((next, sequence), timer);
            }
            due.push(timer.token);
        }
        due
    }

    /// Forget everything. For a page that is going away.
    pub fn clear(&self) {
        self.state
            .lock()
            .expect("the timer wheel is never poisoned")
            .pending
            .clear();
    }
}

impl TimerScheduler for TimerWheel {
    fn schedule(&self, delay_ms: u64, repeat_ms: Option<u64>) -> u64 {
        let mut state = self.state.lock().expect("the timer wheel is never poisoned");
        state.next_token += 1;
        let token = state.next_token;
        let sequence = state.next_sequence;
        state.next_sequence += 1;

        let delay = Duration::from_millis(delay_ms).min(LONGEST);
        let timer = Timer {
            token,
            // A repeat of zero would be a loop with no gap in it. One
            // millisecond is what every engine clamps it to.
            repeat: repeat_ms.map(|ms| Duration::from_millis(ms).clamp(Duration::from_millis(1), LONGEST)),
        };
        state.pending.insert((Instant::now() + delay, sequence), timer);
        token
    }

    fn cancel(&self, token: u64) -> bool {
        let mut state = self.state.lock().expect("the timer wheel is never poisoned");
        let found = state
            .pending
            .iter()
            .find(|(_, timer)| timer.token == token)
            .map(|(key, _)| *key);
        match found {
            Some(key) => {
                state.pending.remove(&key);
                true
            }
            // Already fired, or never ours. The engine drops its own entry
            // either way, so a late fire is a no-op there too.
            None => false,
        }
    }
}

/// A counter for the frame callbacks a page has asked for, so the browser can
/// tell whether a frame is owed without entering the isolate.
#[derive(Debug, Clone, Default)]
pub struct FrameRequests {
    count: Arc<AtomicU64>,
}

impl FrameRequests {
    /// Nothing asked for yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that the page asked for a frame.
    pub fn requested(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether any are outstanding.
    #[must_use]
    pub fn any(&self) -> bool {
        self.count.load(Ordering::Relaxed) > 0
    }

    /// Take the outstanding count, leaving none.
    pub fn take(&self) -> u64 {
        self.count.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_due_before_its_time() {
        let wheel = TimerWheel::new();
        let token = wheel.schedule(50, None);
        let now = Instant::now();
        assert!(wheel.due(now).is_empty());
        assert_eq!(wheel.due(now + Duration::from_millis(60)), vec![token]);
        assert!(wheel.is_empty(), "a one-shot leaves the wheel when it fires");
    }

    /// Two timers due at once run in the order they were asked for, which is
    /// what `setTimeout(a, 0); setTimeout(b, 0)` means.
    #[test]
    fn timers_due_together_keep_the_order_they_were_asked_in() {
        let wheel = TimerWheel::new();
        let first = wheel.schedule(0, None);
        let second = wheel.schedule(0, None);
        let third = wheel.schedule(0, None);
        assert_eq!(
            wheel.due(Instant::now() + Duration::from_millis(1)),
            vec![first, second, third]
        );
    }

    /// A shorter delay asked for later still runs first.
    #[test]
    fn the_earliest_deadline_runs_first() {
        let wheel = TimerWheel::new();
        let late = wheel.schedule(100, None);
        let soon = wheel.schedule(10, None);
        let due = wheel.due(Instant::now() + Duration::from_millis(200));
        assert_eq!(due, vec![soon, late]);
    }

    #[test]
    fn an_interval_comes_back_and_a_cancel_takes_it_off() {
        let wheel = TimerWheel::new();
        let token = wheel.schedule(10, Some(10));
        let at = Instant::now() + Duration::from_millis(20);
        assert_eq!(wheel.due(at), vec![token]);
        assert_eq!(wheel.len(), 1, "a repeat is re-armed as it fires");
        assert_eq!(wheel.due(at + Duration::from_millis(20)), vec![token]);

        assert!(wheel.cancel(token));
        assert!(wheel.is_empty());
        assert!(!wheel.cancel(token), "cancelling twice says so");
    }

    /// A browser that was busy owes one call, not one per missed period.
    #[test]
    fn a_late_interval_is_not_run_once_per_missed_period() {
        let wheel = TimerWheel::new();
        let token = wheel.schedule(1, Some(1));
        let much_later = Instant::now() + Duration::from_secs(5);
        assert_eq!(wheel.due(much_later), vec![token]);
        assert_eq!(wheel.due(much_later), Vec::<u64>::new());
    }

    #[test]
    fn the_next_deadline_is_the_earliest_one() {
        let wheel = TimerWheel::new();
        assert!(wheel.next_deadline().is_none());
        wheel.schedule(500, None);
        let soon = wheel.next_deadline().expect("one is pending");
        wheel.schedule(10, None);
        assert!(wheel.next_deadline().expect("two are pending") < soon);
    }
}
