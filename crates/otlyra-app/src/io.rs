//! The one background runtime, for everything that waits on the world.
//!
//! Network loads and file writes are the same kind of work — they take as long as
//! something outside the process takes — and they must not happen on the thread the
//! window runs on. Before this there were three answers to that: six operating
//! system threads for fetching, a one-thread runtime for saving downloads, and a
//! third runtime hidden inside the network loader that a fetch thread blocked on.
//! Threads are not the scarce thing; runtimes are, because each one is its own
//! reactor, its own timer wheel, and its own set of threads the operating system
//! schedules against the others.
//!
//! So there is one. It is built the first time something asks for it rather than at
//! startup, because a browser that opens `about:blank` and is closed again should
//! not have paid for a reactor — the startup budget in `BROWSER_UI_PLAN.md` is
//! measured to the first frame, and the first frame fetches nothing.
//!
//! Two worker threads: the work is waiting rather than computing, so the count is
//! about how many wakeups can be dispatched at once and not about how many requests
//! can be outstanding. Concurrency limits belong to whoever is asking — the fetcher
//! keeps its own [`crate::fetcher::FETCH_CONCURRENCY`] semaphore — because "how many
//! sockets a page may point at a server" is a browser policy and not a thread count.

use std::sync::OnceLock;

/// The process-wide background runtime, built on first use.
///
/// Deliberately `&'static`: a task spawned here outlives the browser that spawned
/// it, which is what makes a result that arrives after a tab closed a message the
/// receiver can ignore rather than a thread someone has to join.
pub fn shared() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("otlyra-io")
            .enable_all()
            .build()
            .expect("the background I/O runtime must start")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One runtime, however many times it is asked for.
    #[test]
    fn the_runtime_is_built_once() {
        let first = shared();
        let second = shared();
        assert!(std::ptr::eq(first, second));
    }

    /// And it runs work.
    #[test]
    fn the_runtime_runs_a_task() {
        let (sender, receiver) = std::sync::mpsc::channel();
        shared().spawn(async move {
            let _ = sender.send(7u8);
        });
        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .ok(),
            Some(7)
        );
    }
}
