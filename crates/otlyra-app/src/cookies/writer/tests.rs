use super::*;
use crate::cookies::CookieStore;
use std::sync::mpsc;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "otlyra-cookie-writer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn file(&self) -> PathBuf {
        self.0.join("cookies.dat")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn key() -> Arc<OnceLock<Option<Key>>> {
    Arc::new(OnceLock::from(Some(Key::from_bytes([17; 32]))))
}
fn jar() -> SharedJar {
    Arc::new(Mutex::new(Jar::new()))
}
fn set(jar: &SharedJar, value: u64) -> u64 {
    let mut jar = jar.lock().unwrap();
    jar.set(
        &url::Url::parse("https://cookie.test/").unwrap(),
        &format!("session={value}; Max-Age=600"),
        SystemTime::now(),
    )
    .unwrap();
    jar.kept_revision()
}
fn read(file: &Path) -> Jar {
    let sealed = std::fs::read(file).unwrap();
    assert!(crate::secret::is_sealed(&sealed));
    let plain = Key::from_bytes([17; 32]).open(PURPOSE, &sealed).unwrap();
    store::from_text(
        &String::from_utf8(plain).unwrap(),
        Default::default(),
        SystemTime::now(),
    )
}
fn wait(shared: &Shared, predicate: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut state = shared.state.lock().unwrap();
    while !predicate(&state) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "cookie writer did not progress");
        state = shared.changed.wait_timeout(state, remaining).unwrap().0;
    }
}

#[test]
fn slow_writes_neither_hold_the_jar_nor_queue_obsolete_snapshots() {
    let scratch = Scratch::new();
    let jar = jar();
    let revision = set(&jar, 1);
    let (entered, entry) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let writes = Arc::new(AtomicU64::new(0));
    let calls = Arc::clone(&writes);
    let mut writer = CookieWriter::spawn_with(
        Arc::clone(&jar),
        key(),
        scratch.file(),
        0,
        move |file, key, snapshot| {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            write_atomic(file, key, snapshot)
        },
    )
    .unwrap();
    writer.schedule(revision);
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let began = Instant::now();
    for value in 2..=40 {
        writer.schedule(set(&jar, value));
    }
    assert!(
        began.elapsed() < Duration::from_millis(100),
        "scheduling must not wait for disk"
    );
    assert_eq!(
        writer.shared.state.lock().unwrap().written,
        0,
        "an in-flight write is not acknowledged"
    );
    release.send(()).unwrap();
    assert!(writer.finish());
    assert_eq!(writes.load(Ordering::Relaxed), 2);
    assert_eq!(read(&scratch.file()).all()[0].value, "40");
}

#[test]
fn failed_atomic_replacement_keeps_the_revision_pending_for_the_same_revision_retry() {
    let scratch = Scratch::new();
    let blocked = scratch.0.join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let file = blocked.join("cookies.dat");
    let jar = jar();
    let revision = set(&jar, 1);
    let mut writer = CookieWriter::spawn(jar, key(), file.clone(), 0).unwrap();
    assert!(!writer.flush(revision));
    assert_eq!(writer.shared.state.lock().unwrap().written, 0);
    assert!(!file.exists());
    std::fs::remove_file(&blocked).unwrap();
    assert!(
        writer.flush(revision),
        "a failed revision must not be skipped as already written"
    );
    assert_eq!(read(&file).all()[0].value, "1");
    assert!(writer.finish());
    assert_eq!(
        std::fs::read_dir(&blocked).unwrap().count(),
        1,
        "temporary files are cleaned up"
    );
}

#[test]
fn failed_writes_retry_without_another_browser_pump() {
    let scratch = Scratch::new();
    let jar = jar();
    let revision = set(&jar, 1);
    let mut first = true;
    let mut writer =
        CookieWriter::spawn_with(jar, key(), scratch.file(), 0, move |file, key, snapshot| {
            if std::mem::take(&mut first) {
                Err("injected first-write failure".into())
            } else {
                write_atomic(file, key, snapshot)
            }
        })
        .unwrap();
    writer.schedule(revision);
    wait(&writer.shared, |state| state.attempts == 1);
    assert_eq!(writer.shared.state.lock().unwrap().written, 0);
    wait(&writer.shared, |state| state.written == revision);
    assert_eq!(writer.shared.state.lock().unwrap().attempts, 2);
    assert_eq!(read(&scratch.file()).all()[0].value, "1");
    assert!(writer.finish());
}

#[test]
fn shutdown_drains_the_latest_unscheduled_revision() {
    let scratch = Scratch::new();
    let jar = jar();
    let mut writer = CookieWriter::spawn(Arc::clone(&jar), key(), scratch.file(), 0).unwrap();
    set(&jar, 1);
    let revision = jar.lock().unwrap().kept_revision();
    assert!(writer.flush(revision));
    set(&jar, 2);
    assert!(writer.finish());
    assert_eq!(read(&scratch.file()).all()[0].value, "2");
}

#[test]
fn key_refusal_writes_nothing_and_does_not_start_a_retry_loop() {
    let scratch = Scratch::new();
    let jar = jar();
    let revision = set(&jar, 1);
    let mut writer = CookieWriter::spawn(
        Arc::clone(&jar),
        Arc::new(OnceLock::from(None)),
        scratch.file(),
        0,
    )
    .unwrap();
    assert!(!writer.flush(revision));
    wait(&writer.shared, |state| state.finished);
    for value in 2..=40 {
        writer.schedule(set(&jar, value));
    }
    let revision = jar.lock().unwrap().kept_revision();
    assert!(!writer.flush(revision));
    assert_eq!(writer.shared.state.lock().unwrap().attempts, 1);
    assert!(!writer.finish());
    assert!(!scratch.file().exists());
}

#[test]
fn a_refused_key_is_reported_as_in_memory_storage() {
    let scratch = Scratch::new();
    let mut store = CookieStore::sealed_at(scratch.file(), Key::from_bytes([17; 32]));
    store.key = Arc::new(OnceLock::from(None));
    set(&store.jar(), 1);
    assert!(!store.flush());
    assert!(!store.is_persistent());
    assert!(!scratch.file().exists());
}

#[test]
fn a_failed_worker_releases_a_durability_waiter() {
    let scratch = Scratch::new();
    let jar = jar();
    let revision = set(&jar, 1);
    let mut writer = CookieWriter::spawn_with(jar, key(), scratch.file(), 0, |_, _, _| {
        panic!("injected worker failure")
    })
    .unwrap();
    assert!(!writer.flush(revision));
    assert!(!writer.finish());
}

#[test]
fn dropping_the_store_persists_unscheduled_cookies_but_excludes_session_cookies() {
    let scratch = Scratch::new();
    let store = CookieStore::sealed_at(scratch.file(), Key::from_bytes([17; 32]));
    set(&store.jar(), 7);
    store.with(|jar| {
        jar.set(
            &url::Url::parse("https://cookie.test/").unwrap(),
            "temporary=not-persistent",
            SystemTime::now(),
        )
        .unwrap();
    });
    drop(store);
    let kept = read(&scratch.file());
    assert_eq!(kept.len(), 1);
    assert_eq!(kept.all()[0].value, "7");
}

#[test]
fn plain_cookie_migration_removes_the_source_only_after_a_durable_sealed_write() {
    let scratch = Scratch::new();
    let source = scratch.0.join("cookies.tsv");
    let jar = jar();
    set(&jar, 7);
    std::fs::write(
        &source,
        store::to_text(&jar.lock().unwrap(), SystemTime::now()),
    )
    .unwrap();
    let blocked = scratch.0.join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let file = blocked.join("cookies.dat");
    let mut store = CookieStore::sealed_at(file.clone(), Key::from_bytes([17; 32]));
    store.migrate_plain(&source);
    assert!(
        source.exists(),
        "a failed write must preserve the legacy file"
    );
    assert!(!file.exists());
    std::fs::remove_file(&blocked).unwrap();
    store.migrate_plain(&source);
    assert!(!source.exists());
    assert_eq!(read(&file).all()[0].value, "7");
}

#[test]
fn native_window_close_does_not_join_a_blocked_persistence_worker() {
    struct NoNetwork;
    impl crate::fetcher::Loader for NoNetwork {
        fn load(&self, _: &str) -> Result<crate::fetcher::Loaded, String> {
            unreachable!()
        }
    }
    let scratch = Scratch::new();
    let mut store = CookieStore::sealed_at(scratch.file(), Key::from_bytes([17; 32]));
    let (entered, entry) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    store.writer = Some(
        CookieWriter::spawn_with(
            store.jar(),
            Arc::clone(&store.key),
            scratch.file(),
            0,
            move |file, key, snapshot| {
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                write_atomic(file, key, snapshot)
            },
        )
        .unwrap(),
    );
    let shared = Arc::clone(&store.writer.as_ref().unwrap().shared);
    set(&store.jar(), 9);
    let window = crate::browser::WindowBrowser::spawn(
        otlyra_platform::Viewport::new(800, 600, 1.0),
        true,
        move |_text| {
            let mut browser = crate::browser::Browser::new(NoNetwork);
            otlyra_platform::Painter::on_event(
                &mut browser,
                otlyra_platform::PlatformEvent::VisibilityChanged(false),
            );
            browser.set_cookie_store(store);
            browser
        },
    )
    .unwrap();
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let began = Instant::now();
    drop(window);
    assert!(
        began.elapsed() < Duration::from_millis(100),
        "native close must not wait for cookie disk work"
    );
    release.send(()).unwrap();
    wait(&shared, |state| state.finished);
    assert_eq!(read(&scratch.file()).all()[0].value, "9");
}

#[test]
fn explicit_post_loop_shutdown_waits_for_cookie_persistence() {
    struct NoNetwork;
    impl crate::fetcher::Loader for NoNetwork {
        fn load(&self, _: &str) -> Result<crate::fetcher::Loaded, String> {
            unreachable!()
        }
    }
    let scratch = Scratch::new();
    let store = CookieStore::sealed_at(scratch.file(), Key::from_bytes([17; 32]));
    set(&store.jar(), 11);
    let mut window = crate::browser::WindowBrowser::spawn(
        otlyra_platform::Viewport::new(800, 600, 1.0),
        true,
        move |_text| {
            let mut browser = crate::browser::Browser::new(NoNetwork);
            otlyra_platform::Painter::on_event(
                &mut browser,
                otlyra_platform::PlatformEvent::VisibilityChanged(false),
            );
            browser.set_cookie_store(store);
            browser
        },
    )
    .unwrap();
    window.shutdown().unwrap();
    assert_eq!(read(&scratch.file()).all()[0].value, "11");
    window.shutdown().unwrap();
}
