"""Regression checks for the startup measurement protocol."""

import contextlib
import importlib.util
import io
import json
import pathlib
import tempfile
import sys
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "startup_benchmark", pathlib.Path(__file__).with_name("startup-benchmark.py")
)
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class StartupBenchmarkTests(unittest.TestCase):
    def test_renderer_flags_and_profile_are_explicit(self):
        for cpu_raster in (False, True):
            with self.subTest(cpu_raster=cpu_raster), tempfile.TemporaryDirectory() as directory:
                report = pathlib.Path(directory) / "run.json"
                report.write_text(json.dumps({
                    "schema": 2,
                    "process_to_visible_ms": 10,
                    "process_to_first_frame_ms": 20,
                    "visible_to_first_frame_ms": 10,
                    "physical_width": 2048,
                    "physical_height": 1536,
                    "scale_factor": 2,
                    "stages": [],
                }))
                with patch.object(benchmark.subprocess, "run") as run:
                    benchmark.one_run(pathlib.Path("otlyra"), report, 1024, 768, 30, cpu_raster,
                                      "file:///tmp/timer-fixture.html")
                command = run.call_args.args[0]
                self.assertEqual("--cpu-raster" in command, cpu_raster)
                self.assertNotIn("--direct-gpu", command)
                self.assertIn("--no-secrets", command)
                self.assertEqual(command[command.index("--url") + 1], "file:///tmp/timer-fixture.html")
                self.assertEqual(
                    run.call_args.kwargs["env"]["OTLYRA_CONFIG_DIR"],
                    str(report.parent / "profile"),
                )

    def test_stage_durations_follow_worker_completion_order(self):
        def marks(*entries):
            return {"stages": [{"name": name, "ms": ms} for name, ms in entries]}

        stages = benchmark.stage_metrics([
            marks(("visible", 10), ("chrome", 20), ("gpu", 30)),
            marks(("visible", 12), ("gpu", 18), ("chrome", 22)),
            marks(("visible", 11), ("gpu", 19)),
        ])
        durations = stages["durations"]
        self.assertEqual(durations["chrome__to__gpu"]["samples"], [10])
        self.assertEqual(durations["gpu__to__chrome"]["samples"], [4])
        self.assertEqual(durations["visible__to__gpu"]["samples"], [6, 8])
        self.assertEqual(stages["cumulative"]["chrome"]["samples"], [20, 22])

    def test_p95_uses_the_nineteenth_of_twenty_samples(self):
        self.assertEqual(benchmark.percentile(list(range(20, 0, -1)), 0.95), 19)

    def test_phases_include_overlapping_work_and_skip_missing_pairs(self):
        runs = [{"stages": [
            {"name": "wgpu_instance_started", "ms": 3},
            {"name": "window_created", "ms": 5},
            {"name": "wgpu_instance_created", "ms": 9},
            {"name": "chrome_build_started", "ms": 12},
            {"name": "chrome_build_complete", "ms": 15},
        ]}]
        phases = benchmark.phase_metrics(runs)
        self.assertEqual(phases["gpu_backend_discovery"]["samples"], [6])
        self.assertEqual(phases["chrome_build"]["samples"], [3])
        self.assertNotIn("gpu_raster_setup", phases)

    def test_comparison_alternates_binaries_and_keeps_budget_enforcement(self):
        for check in (False, True):
            with self.subTest(check=check), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                candidate, baseline = root / "candidate", root / "baseline"
                candidate.write_bytes(b"new")
                baseline.write_bytes(b"old")
                output = root / "result.json"
                launches = []

                def fake_run(binary, report, width, height, timeout, cpu_raster, workload_url):
                    self.assertEqual(workload_url, "file:///tmp/timer-fixture.html")
                    launches.append((binary.name, report.parent.name))
                    old = binary.name == baseline.name
                    return {
                        "process_to_visible_ms": 200 if old else 160,
                        "process_to_first_frame_ms": 300 if old else 220,
                        "visible_to_first_frame_ms": 100 if old else 60,
                        "physical_width": 2048, "physical_height": 1536,
                        "scale_factor": 2, "stages": [],
                    }

                arguments = ["startup", "--binary", str(candidate),
                             "--baseline-binary", str(baseline), "--samples", "20",
                             "--warmups", "0", "--output", str(output)]
                arguments.extend(["--url", "file:///tmp/timer-fixture.html"])
                if check:
                    arguments.append("--check")
                with patch.object(sys, "argv", arguments), \
                        patch.object(benchmark, "one_run", side_effect=fake_run), \
                        patch.object(benchmark, "git", return_value=""), \
                        patch.object(benchmark, "hardware_name", return_value="test"), \
                        contextlib.redirect_stdout(io.StringIO()), \
                        contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(benchmark.main(), int(check))
                self.assertEqual(len(launches), 40)
                self.assertEqual(launches[:4], [("baseline", "baseline"),
                                               ("candidate", "candidate"),
                                               ("candidate", "candidate"),
                                               ("baseline", "baseline")])
                summary = json.loads(output.read_text())
                comparison = summary["comparison"]
                self.assertEqual(summary["workload_url"], "file:///tmp/timer-fixture.html")
                self.assertEqual(comparison["baseline"]["sample_count"], 20)
                self.assertEqual(comparison["paired_delta_ms"][
                    "process_to_first_frame_ms"]["p50"], -80)
                self.assertEqual(len(summary["failures"]), 4)

    def test_report_delivery_is_separate_from_present_scheduling(self):
        entries = [
            ("gpu_presenter_created", 10), ("gpu_ready_report_posted", 11),
            ("gpu_ready_wake_requested", 12), ("gpu_ready_wake_returned", 13),
            ("gpu_ready", 60), ("chrome_raster_complete", 80),
            ("render_present_scheduled", 82), ("render_report_posted", 83),
            ("first_frame_report_received", 95), ("first_present_complete", 96),
        ]
        phases = benchmark.phase_metrics([{"stages": [
            {"name": name, "ms": ms} for name, ms in entries
        ]}])
        self.assertEqual(phases["gpu_ready_posting"]["samples"], [1])
        self.assertEqual(phases["gpu_ready_report_delivery"]["samples"], [49])
        self.assertEqual(phases["gpu_ready_wake_call"]["samples"], [1])
        self.assertEqual(phases["render_present_scheduling"]["samples"], [2])
        self.assertEqual(phases["render_report_delivery"]["samples"], [12])
        self.assertEqual(phases["first_frame_ui_accounting"]["samples"], [1])
        self.assertEqual(phases["first_present"]["samples"], [16])


    def test_content_delivery_and_frame_adoption_are_separate(self):
        entries = [("preferences_ready", 10), ("content_owner_ready", 12),
                   ("first_content_scene_ready", 30),
                   ("first_content_scene_received", 45),
                   ("first_content_scene_composed", 55)]
        phases = benchmark.phase_metrics([{"stages": [
            {"name": name, "ms": ms} for name, ms in entries
        ]}])
        self.assertEqual(phases["content_owner_setup"]["samples"], [2])
        self.assertEqual(phases["first_content_scene"]["samples"], [18])
        self.assertEqual(phases["first_content_delivery"]["samples"], [15])
        self.assertEqual(phases["first_content_to_frame"]["samples"], [10])

    def test_native_bootstrap_costs_keep_their_own_intervals(self):
        entries = [("browser_ready", 1), ("event_loop_build_started", 2),
                   ("event_loop_created", 80), ("icon_decode_started", 100),
                   ("icon_image_created", 102), ("icon_ready", 130),
                   ("menu_validation_started", 131), ("menu_validation_complete", 132),
                   ("menu_root_created", 135), ("menu_first_submenu_created", 140),
                   ("menu_first_item_created", 145), ("menu_first_submenu_built", 150),
                   ("menu_first_submenu_attached", 155), ("menu_items_created", 160),
                   ("menu_installed", 180), ("menu_ready", 181),
                   ("window_attributes_started", 181.5), ("window_attributes_ready", 182), ("window_created", 220)]
        phases = benchmark.phase_metrics([{"stages": [
            {"name": name, "ms": ms} for name, ms in entries
        ]}])
        expected = {"event_loop_preparation": 1, "native_event_loop_build": 78,
                    "icon_decode": 2, "icon_install": 28, "menu_validation": 1,
                    "menu_root_creation": 3, "menu_first_submenu_creation": 5,
                    "menu_first_item_creation": 5, "menu_first_submenu_build": 10,
                    "menu_first_submenu_attach": 5, "menu_item_creation": 25,
                    "menu_install": 20, "window_attribute_preparation": 0.5,
                    "native_window_creation": 38}
        for name, duration in expected.items():
            self.assertEqual(phases[name]["samples"], [duration])


if __name__ == "__main__":
    unittest.main()
