use super::*;
use std::time::Duration;

struct Active {
    live: Arc<AtomicUsize>,
    dropped: Sender<()>,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        let _ = self.dropped.send(());
    }
}

struct Pending {
    live: Arc<AtomicUsize>,
    started: Sender<()>,
    dropped: Sender<()>,
}
impl AsyncLoader for Pending {
    fn fetch(
        self: Arc<Self>,
        _: String,
        _: Option<Body>,
        _: CacheMode,
        _: Vec<(String, String)>,
    ) -> Fetching {
        Box::pin(async move {
            self.live.fetch_add(1, Ordering::SeqCst);
            let _active = Active {
                live: Arc::clone(&self.live),
                dropped: self.dropped.clone(),
            };
            self.started.send(()).unwrap();
            std::future::pending().await
        })
    }
}

fn pending() -> (Fetcher, Arc<AtomicUsize>, Receiver<()>, Receiver<()>) {
    let live = Arc::new(AtomicUsize::new(0));
    let (started, starts) = channel();
    let (dropped, drops) = channel();
    (
        Fetcher::spawn_async(Pending {
            live: Arc::clone(&live),
            started,
            dropped,
        }),
        live,
        starts,
        drops,
    )
}

#[test]
fn shutdown_cancels_active_and_queued_requests_without_wakes_or_stale_replies() {
    let (mut fetcher, live, starts, drops) = pending();
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&wakes);
    fetcher.set_waker(Waker::from_callback(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    for n in 0..FETCH_CONCURRENCY * 4 {
        fetcher.request(&format!("https://pending.test/{n}"), ResourceKind::Document);
    }
    for _ in 0..FETCH_CONCURRENCY {
        starts.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(live.load(Ordering::SeqCst), FETCH_CONCURRENCY);
    crate::io::shared().block_on(fetcher.shutdown());
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(drops.try_iter().count(), FETCH_CONCURRENCY);
    assert_eq!(
        starts.try_iter().count(),
        0,
        "queued transports must never start during shutdown"
    );
    assert!(!fetcher.has_tasks());
    assert!(fetcher.poll().is_empty());
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn dropping_one_pool_cancels_its_requests_without_stopping_another_pool() {
    let (mut first, first_live, first_starts, first_drops) = pending();
    let (mut second, second_live, second_starts, _) = pending();
    first.request("https://pending.test/first", ResourceKind::Document);
    second.request("https://pending.test/second", ResourceKind::Document);
    first_starts.recv_timeout(Duration::from_secs(5)).unwrap();
    second_starts.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(first);
    first_drops.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first_live.load(Ordering::SeqCst), 0);
    assert_eq!(second_live.load(Ordering::SeqCst), 1);
    crate::io::shared().block_on(second.shutdown());
    assert_eq!(second_live.load(Ordering::SeqCst), 0);
}

#[test]
fn an_async_transport_panic_delivers_a_failure_and_wakes_the_owner() {
    struct Panicking;
    impl AsyncLoader for Panicking {
        fn fetch(
            self: Arc<Self>,
            _: String,
            _: Option<Body>,
            _: CacheMode,
            _: Vec<(String, String)>,
        ) -> Fetching {
            Box::pin(async { panic!("injected async transport failure") })
        }
    }
    let mut fetcher = Fetcher::spawn_async(Panicking);
    let (woke, wakes) = channel();
    fetcher.set_waker(Waker::from_callback(move || {
        let _ = woke.send(());
    }));
    let id = fetcher.request("https://panic.test/", ResourceKind::Document);
    wakes.recv_timeout(Duration::from_secs(5)).unwrap();
    let replies = fetcher.wait(Duration::from_secs(5));
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].id, id);
    assert_eq!(
        replies[0].result.as_ref().unwrap_err(),
        "the transport task panicked"
    );
    assert!(matches!(fetcher.exchanges()[0].status, Status::Failed(_)));
    assert!(fetcher.poll().is_empty(), "a task has one terminal reply");
}

#[test]
fn completed_task_handles_are_reclaimed_while_the_pool_is_used() {
    struct Immediate;
    impl Loader for Immediate {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                final_url: url.into(),
                ..Default::default()
            })
        }
    }
    let mut fetcher = Fetcher::spawn(Immediate);
    for n in 0..40 {
        fetcher.request(
            &format!("https://immediate.test/{n}"),
            ResourceKind::Document,
        );
        assert_eq!(fetcher.wait(Duration::from_secs(5)).len(), 1);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while fetcher.has_tasks() {
        fetcher.poll();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
}
