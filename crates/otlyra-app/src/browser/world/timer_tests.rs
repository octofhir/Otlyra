use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

struct UnusedLoader;
impl crate::fetcher::Loader for UnusedLoader {
    fn load(&self, _: &str) -> Result<crate::fetcher::Loaded, String> {
        unreachable!("no resources requested")
    }
}

struct BusyClock {
    calls: Arc<AtomicUsize>,
    due: Instant,
}
impl otlyra_html::ScriptRunner for BusyClock {
    fn run(&mut self, _: &str, _: NodeId, _: &mut otlyra_dom::Document) {}
    fn next_deadline(&self) -> Option<Instant> {
        Some(self.due)
    }
    fn run_due_timers(&mut self, _: &mut otlyra_dom::Document) -> usize {
        std::thread::sleep(Duration::from_millis(6));
        self.calls.fetch_add(1, Ordering::SeqCst);
        1
    }
}

#[test]
fn busy_tabs_share_the_timer_deadline_without_starving_later_slots() {
    let mut browser = crate::browser::Browser::with_fetcher_and_text(
        Fetcher::spawn(UnusedLoader),
        Default::default(),
        TextEngine::isolated(),
    );
    let mut counters = Vec::new();
    for index in 0..5 {
        let calls = Arc::new(AtomicUsize::new(0));
        let tab = TabId(index);
        let page = PageScene::new(otlyra_html::parse(b"<p>clock", Some("utf-8")).document);
        browser.world.set_page(tab, Some(page));
        browser.world.set_scripts(
            tab,
            Box::new(BusyClock {
                calls: Arc::clone(&calls),
                due: Instant::now(),
            }),
        );
        counters.push(calls);
    }
    for _ in 0..5 {
        let before: usize = counters
            .iter()
            .map(|counter| counter.load(Ordering::SeqCst))
            .sum();
        assert!(browser.world.run_due_timers());
        let after: usize = counters
            .iter()
            .map(|counter| counter.load(Ordering::SeqCst))
            .sum();
        assert!(after - before <= 2, "one pump exhausted every busy tab");
    }
    assert!(
        counters
            .iter()
            .all(|counter| counter.load(Ordering::SeqCst) > 0),
        "a busy prefix starved another tab"
    );
    // A changed map size must not leave the cursor outside the collection.
    browser.world.slots.clear();
    assert!(!browser.world.run_due_timers());
}
