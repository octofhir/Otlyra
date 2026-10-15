use super::*;
use std::path::PathBuf;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn native_close_does_not_block_ui_but_owner_exit_finishes_accepted_downloads() {
    for native in [false, true] {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "otlyra-download-shutdown-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&scratch.0).unwrap();
        let path = scratch.0.join("saved.bin");
        let destination = path.clone();
        let (started, entered) = mpsc::channel();
        let (allow, gate) = tokio::sync::oneshot::channel();
        let factory = move |text| {
            let mut browser =
                Browser::with_fetcher_and_text(Fetcher::spawn(Fields), Default::default(), text);
            browser.on_event(PlatformEvent::VisibilityChanged(false));
            let id = browser
                .downloads
                .record(
                    "saved.bin",
                    "https://download.test/saved.bin",
                    None,
                    b"completed before exit".to_vec(),
                )
                .unwrap();
            browser.downloads.mark_saving(
                id,
                destination.display().to_string(),
                Some(destination.clone()),
            );
            let bytes = browser.downloads.get(id).unwrap().payload();
            browser.downloads_writer.save_with_gate(
                id,
                crate::downloads::Destination::Exact(destination),
                bytes,
                started,
                gate,
            );
            browser
        };
        let viewport = Viewport::new(900, 640, 1.0);
        let mut window = if native {
            WindowBrowser::spawn_native(viewport, true, None, factory)
        } else {
            WindowBrowser::spawn(viewport, true, factory)
        }
        .unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (release, release_rx) = mpsc::channel();
        let controller = std::thread::spawn(move || {
            // Release a broken blocking Drop too, so a regression fails rather
            // than leaving the test harness stuck behind its own gate.
            match release_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = allow.send(());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        });
        let owner = window.owner.take().unwrap();
        let start = Instant::now();
        drop(window);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "native close waited for filesystem work"
        );
        let (done, finished) = mpsc::channel();
        let joiner = std::thread::spawn(move || {
            owner.join().unwrap();
            done.send(()).unwrap();
        });
        assert!(
            matches!(
                finished.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "content exited with an accepted save still pending"
        );
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        controller.join().unwrap();
        joiner.join().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"completed before exit");
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 1);
    }
}
