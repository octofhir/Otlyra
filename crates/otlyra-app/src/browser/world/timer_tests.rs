use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// Run `run` on a thread with the content owner's 8 MiB stack: a test thread's
/// default stack is not enough to bootstrap a page isolate reliably.
fn on_content_stack(run: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name("content-stack-test".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

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

struct ClockPage;
impl crate::fetcher::Loader for ClockPage {
    fn load(&self, url: &str) -> Result<crate::fetcher::Loaded, String> {
        Ok(crate::fetcher::Loaded {
            bytes: br#"<p id=clock></p><script>
              document.getElementById('clock').setAttribute('initial', String(performance.now()));
              function tick(timestamp) {
                const node = document.getElementById('clock');
                if (!node.hasAttribute('first')) node.setAttribute('first', String(timestamp));
                node.setAttribute('frame', String(timestamp));
                node.setAttribute('now', String(performance.now()));
                requestAnimationFrame(tick);
              }
              requestAnimationFrame(tick);
            </script>"#
                .to_vec(),
            content_type: Some("text/html".into()),
            final_url: url.into(),
            ..Default::default()
        })
    }
}

#[test]
fn page_clock_frames_use_the_documents_origin_in_an_older_world() {
    // A page isolate needs the content owner's stack, not a test thread's.
    on_content_stack(|| {
        let mut browser = crate::browser::Browser::with_fetcher_and_text(
            Fetcher::spawn(ClockPage),
            Default::default(),
            TextEngine::isolated(),
        );
        browser.world.started = Instant::now() - Duration::from_secs(600);
        browser.navigate("https://clock.test/");
        browser.wait_for_load(Duration::from_secs(5));
        browser.compose_content_scene(otlyra_platform::Viewport::new(800, 600, 1.0));
        let document = browser.active_page().unwrap().document();
        let node = otlyra_css::stylo_dom::select(document, "#clock").unwrap()[0];
        let frame: f64 = document
            .attr(node, "frame")
            .expect("frame ran")
            .parse()
            .unwrap();
        let now: f64 = document.attr(node, "now").unwrap().parse().unwrap();
        assert!(
            frame >= 0.0 && now >= frame && now - frame < 1000.0,
            "page performance.now={now}, frame timestamp={frame}"
        );
    });
}

#[test]
fn page_clock_first_script_includes_the_navigation_wait() {
    // A page isolate needs the content owner's stack, not a test thread's.
    on_content_stack(|| {
        let mut browser = crate::browser::Browser::with_fetcher_and_text(
            Fetcher::spawn(ClockPage),
            Default::default(),
            TextEngine::isolated(),
        );
        browser.navigate("https://clock.test/");
        let tab = browser.active_id();
        browser.world.pending_mut(tab).unwrap().started = Instant::now() - Duration::from_secs(10);
        browser.wait_for_load(Duration::from_secs(5));
        let document = browser.active_page().unwrap().document();
        let node = otlyra_css::stylo_dom::select(document, "#clock").unwrap()[0];
        let initial: f64 = document.attr(node, "initial").unwrap().parse().unwrap();
        let first: f64 = document.attr(node, "first").unwrap().parse().unwrap();
        assert!(
            first >= 10000.0,
            "deferred first frame used a zero timestamp: {first}"
        );
        assert!(
            initial >= 10000.0,
            "initial performance.now ignored navigation wait: {initial}"
        );
    });
}
