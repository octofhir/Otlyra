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
        let fixture = profile.0.join("viewport.html");
        std::fs::write(&fixture, "<!doctype html><title>Native viewport</title><p>Viewport probe</p><script>console.log('OTLYRA_INITIAL_VIEWPORT', innerWidth, innerHeight, devicePixelRatio)</script>").unwrap();
        for (cpu, web) in [(false, false), (true, false), (false, true), (true, true)] {
            let report = profile.0.join(format!(
                "{}-{}.json",
                if cpu { "cpu" } else { "gpu" },
                if web { "web" } else { "about" }
            ));
            let mut command = Command::new(env!("CARGO_BIN_EXE_otlyra"));
            command
                .args(["--no-secrets", "--startup-report"])
                .arg(&report)
                .env("OTLYRA_CONFIG_DIR", profile.0.join("profile"))
                .env("OTLYRA_LOG", "error,page.console=info");
            if web {
                // The screenshot-only scale must never leak into native JS.
                command
                    .arg("--file")
                    .arg(&fixture)
                    .args(["--scale-factor", "3"]);
            } else {
                command.args(["--url", "about:about"]);
            }
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
            if web {
                let scale = value["scale_factor"].as_f64().unwrap();
                let width = (value["physical_width"].as_f64().unwrap() / scale).floor();
                let height = (value["physical_height"].as_f64().unwrap() / scale
                    - otlyra_app::ui::UI_HEIGHT)
                    .floor();
                let logs = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let line = logs
                    .lines()
                    .find_map(|line| {
                        line.split_once("OTLYRA_INITIAL_VIEWPORT ")
                            .map(|(_, metrics)| metrics)
                    })
                    .expect("the first inline script logs its viewport");
                let metrics: Vec<f64> = line
                    .split_whitespace()
                    .map(|metric| metric.parse().unwrap())
                    .collect();
                assert_eq!(
                    metrics,
                    [width, height, scale],
                    "cpu={cpu}: first-script viewport differs from the native surface"
                );
            }
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
                "content_surface_ready",
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
                stages
                    .iter()
                    .find(|stage| stage["name"] == name)
                    .unwrap_or_else(|| panic!("cpu={cpu}, web={web}: missing startup stage {name}"))
                    ["ms"]
                    .as_f64()
                    .unwrap()
            };
            if stages
                .iter()
                .any(|stage| stage["name"] == "bootstrap_redraw_reissued")
            {
                assert!(time("first_loop_idle") <= time("bootstrap_redraw_reissued"));
                assert!(time("bootstrap_redraw_reissued") <= time("chrome_build_started"));
            }
            for (before, after) in [
                ("accesskit_attached", "visibility_requested"),
                ("icon_ready", "visibility_requested"),
                ("menu_ready", "visibility_requested"),
                ("menu_installed", "visibility_requested"),
                ("content_owner_ready", "first_content_scene_ready"),
                ("window_created", "content_surface_ready"),
                ("content_owner_ready", "content_surface_ready"),
                ("content_surface_ready", "first_content_scene_ready"),
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

    /// A presenter that reports a newly shown window occluded must not leave
    /// its first frame unpainted. Needs the injection hook:
    /// `cargo test -p otlyra-app --features fault-injection --test window_startup -- --ignored`
    #[cfg(feature = "fault-injection")]
    #[test]
    #[ignore = "requires a visible macOS desktop"]
    fn a_first_frame_refused_as_occluded_is_redrawn() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile = Profile(std::env::temp_dir().join(format!(
            "otlyra-window-occluded-{}-{nonce}",
            std::process::id(),
        )));
        std::fs::create_dir(&profile.0).unwrap();
        for (cpu, refusals) in [(false, 1), (true, 1), (false, 3), (true, 3)] {
            let report = profile.0.join(format!("{cpu}-{refusals}.json"));
            let mut command = Command::new(env!("CARGO_BIN_EXE_otlyra"));
            command
                .args(["--no-secrets", "--url", "about:about", "--startup-report"])
                .arg(&report)
                .env("OTLYRA_CONFIG_DIR", profile.0.join("profile"))
                .env("OTLYRA_FAULT_OCCLUDED_ACQUIRES", refusals.to_string())
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
                !timed_out && output.status.success(),
                "cpu={cpu}, refusals={refusals}: first frame never presented: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&report).expect("a presented frame writes its report"),
            )
            .unwrap();
            let stages = value["stages"].as_array().unwrap();
            let time = |name: &str| {
                stages
                    .iter()
                    .find(|stage| stage["name"] == name)
                    .and_then(|stage| stage["ms"].as_f64())
            };
            let refused = time("presentation_occluded_while_visible")
                .unwrap_or_else(|| panic!("cpu={cpu}: the injected refusal was not observed"));
            assert!(refused <= time("first_present_complete").unwrap());
        }
    }
}
