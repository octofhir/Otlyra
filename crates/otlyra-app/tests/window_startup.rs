//! Opt-in smoke check for the real AppKit/Metal startup boundary.
//! `cargo test -p otlyra-app --test window_startup -- --ignored`

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct Profile(PathBuf);

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore = "requires a visible macOS desktop"]
    fn gpu_and_cpu_windows_present_their_first_frame() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile = Profile(std::env::temp_dir().join(format!(
            "otlyra-window-startup-{}-{nonce}",
            std::process::id(),
        )));
        std::fs::create_dir(&profile.0).unwrap();
        for cpu in [false, true] {
            let report = profile.0.join(if cpu { "cpu.json" } else { "gpu.json" });
            let mut command = Command::new(env!("CARGO_BIN_EXE_otlyra"));
            command
                .args(["--no-secrets", "--url", "about:about", "--startup-report"])
                .arg(&report)
                .env("OTLYRA_CONFIG_DIR", profile.0.join("profile"))
                .env("OTLYRA_LOG", "error");
            if cpu {
                command.arg("--cpu-raster");
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn the browser");
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut timed_out = false;
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                !timed_out,
                "cpu={cpu}: startup timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "cpu={cpu}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&report).expect("a presented frame writes its report"),
            )
            .unwrap();
            assert_eq!(value["schema"], 2);
            let visible = value["process_to_visible_ms"].as_f64().unwrap();
            let frame = value["process_to_first_frame_ms"].as_f64().unwrap();
            assert!(frame >= visible && visible > 0.0);
            let stages = value["stages"].as_array().unwrap();
            let attached = stages
                .iter()
                .find(|stage| stage["name"] == "surface_attached")
                .unwrap()["ms"]
                .as_f64()
                .unwrap();
            assert!(
                attached <= visible,
                "attach Metal before showing the window"
            );
            for name in [
                "content_owner_ready",
                "first_content_scene_ready",
                "first_content_scene_received",
                "first_content_scene_composed",
                "wgpu_instance_created",
                "visibility_requested",
                "surface_attached",
                "gpu_ready",
                "gpu_ready_report_posted",
                "render_present_scheduled",
                "render_report_posted",
                "first_frame_report_received",
                "first_present_complete",
            ] {
                assert!(
                    stages.iter().any(|stage| stage["name"] == name),
                    "cpu={cpu}: missing {name}"
                );
            }
            let time = |name| {
                stages.iter().find(|stage| stage["name"] == name).unwrap()["ms"]
                    .as_f64()
                    .unwrap()
            };
            for (before, after) in [
                ("content_owner_ready", "first_content_scene_ready"),
                ("first_content_scene_ready", "first_content_scene_received"),
                (
                    "first_content_scene_received",
                    "first_content_scene_composed",
                ),
                ("first_content_scene_composed", "chrome_build_complete"),
                ("gpu_presenter_created", "gpu_ready_report_posted"),
                ("gpu_ready_report_posted", "gpu_ready"),
                ("chrome_raster_complete", "render_present_scheduled"),
                ("render_present_scheduled", "render_report_posted"),
                ("render_report_posted", "first_frame_report_received"),
                ("first_frame_report_received", "first_present_complete"),
            ] {
                assert!(
                    time(before) <= time(after),
                    "cpu={cpu}: {before} must precede {after}"
                );
            }
        }
    }
}
