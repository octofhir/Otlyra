#!/usr/bin/env python3
"""Measure real window startup distributions and enforce Otlyra's budgets."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import os
import pathlib
import platform
import subprocess
import sys
import tempfile
from typing import Any


DEFAULT_BUDGETS = {
    "process_to_visible_ms": {"p50": 50.0, "p95": 100.0},
    "process_to_first_frame_ms": {"p50": 100.0, "p95": 150.0},
}

# These intervals can overlap. Adjacent marks alone cannot attribute work once
# GPU discovery runs alongside native window creation.
PHASES = {
    "content_owner_setup": ("preferences_ready", "content_owner_ready"),
    "first_content_scene": ("content_owner_ready", "first_content_scene_ready"),
    "native_surface_delivery": ("window_created", "content_surface_ready"),
    "confirmed_content_scene": ("content_surface_ready", "first_content_scene_ready"),
    "first_content_delivery": ("first_content_scene_ready", "first_content_scene_received"),
    "first_content_to_frame": ("first_content_scene_received", "first_content_scene_composed"),
    "event_loop_creation": ("browser_ready", "event_loop_created"),
    "native_launch": ("event_loop_created", "event_loop_resumed"),
    "event_loop_preparation": ("browser_ready", "event_loop_build_started"),
    "native_event_loop_build": ("event_loop_build_started", "event_loop_created"),
    "icon_decode": ("icon_decode_started", "icon_image_created"),
    "icon_install": ("icon_image_created", "icon_ready"),
    "menu_validation": ("menu_validation_started", "menu_validation_complete"),
    "menu_root_creation": ("menu_validation_complete", "menu_root_created"),
    "menu_first_submenu_creation": ("menu_root_created", "menu_first_submenu_created"),
    "menu_first_item_creation": ("menu_first_submenu_created", "menu_first_item_created"),
    "menu_first_submenu_build": ("menu_first_submenu_created", "menu_first_submenu_built"),
    "menu_first_submenu_attach": ("menu_first_submenu_built", "menu_first_submenu_attached"),
    "menu_item_creation": ("menu_root_created", "menu_items_created"),
    "menu_install": ("menu_items_created", "menu_installed"),
    "window_attribute_preparation": ("menu_ready", "window_attributes_ready"),
    "native_window_creation": ("window_attributes_ready", "window_created"),
    "gpu_backend_discovery": ("wgpu_instance_started", "wgpu_instance_created"),
    "gpu_device_setup": ("surface_attached", "gpu_presenter_created"),
    "gpu_ready_delivery": ("gpu_presenter_created", "gpu_ready"),
    "gpu_ready_posting": ("gpu_presenter_created", "gpu_ready_report_posted"),
    "gpu_ready_report_delivery": ("gpu_ready_report_posted", "gpu_ready"),
    "gpu_ready_wake_call": ("gpu_ready_wake_requested", "gpu_ready_wake_returned"),
    "first_native_event_batch": ("visibility_requested", "first_loop_idle"),
    "redraw_dispatch": ("gpu_ready", "chrome_build_started"),
    "chrome_build": ("chrome_build_started", "chrome_build_complete"),
    "gpu_raster_setup": ("gpu_raster_setup_started", "gpu_raster_setup_complete"),
    "chrome_raster": ("chrome_raster_started", "chrome_raster_complete"),
    "first_present": ("chrome_raster_complete", "first_present_complete"),
    "render_present_scheduling": ("chrome_raster_complete", "render_present_scheduled"),
    "render_report_delivery": ("render_report_posted", "first_frame_report_received"),
    "render_wake_call": ("render_wake_requested", "render_wake_returned"),
    "first_frame_ui_accounting": ("first_frame_report_received", "first_present_complete"),
}


def percentile(values: list[float], fraction: float) -> float:
    """Nearest-rank percentile, stable for the small CI sample."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def git(repo: pathlib.Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def hardware_name() -> str:
    if platform.system() != "Darwin":
        return platform.machine()
    result = subprocess.run(
        ["sysctl", "-n", "machdep.cpu.brand_string"],
        check=False,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip() or platform.machine()


def one_run(
    binary: pathlib.Path,
    report: pathlib.Path,
    width: int,
    height: int,
    timeout: float,
    cpu_raster: bool,
    workload_url: str = "about:about",
) -> dict[str, Any]:
    environment = os.environ.copy()
    environment["OTLYRA_LOG"] = "error"
    environment["OTLYRA_CONFIG_DIR"] = str(report.parent / "profile")
    command = [
        str(binary),
        "--no-secrets",
        "--url",
        workload_url,
        "--width",
        str(width),
        "--height",
        str(height),
        "--startup-report",
        str(report),
    ]
    if cpu_raster:
        command.append("--cpu-raster")
    try:
        subprocess.run(
            command,
            check=True,
            timeout=timeout,
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"browser exited with {error.returncode}: {error.stderr.strip()}") from error
    if not report.exists():
        raise RuntimeError("browser exited without writing its startup report")
    value = json.loads(report.read_text(encoding="utf-8"))
    if value.get("schema") != 2:
        raise RuntimeError(f"unsupported startup report schema: {value.get('schema')!r}")
    for name in (
        "process_to_visible_ms",
        "process_to_first_frame_ms",
        "visible_to_first_frame_ms",
        "physical_width",
        "physical_height",
        "scale_factor",
    ):
        if not isinstance(value.get(name), (int, float)):
            raise RuntimeError(f"startup report has no numeric {name}: {value.get(name)!r}")
    stages = value.get("stages")
    if not isinstance(stages, list):
        raise RuntimeError(f"startup report has no stage array: {stages!r}")
    for entry in stages:
        if not isinstance(entry, dict) or not isinstance(entry.get("name"), str) \
                or not isinstance(entry.get("ms"), (int, float)):
            raise RuntimeError(f"malformed stage entry: {entry!r}")
    return value


def stage_metrics(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate per-milestone cumulative times and per-stage durations.

    Milestones are cumulative milliseconds from the process origin; the duration
    of a stage is the gap to its predecessor in that run's launch order. Worker
    completion can change that order; aggregate each observed transition rather
    than subtracting timestamps from unrelated turns of the event loop.
    """
    # Representative milestone order; transitions below use each run's order.
    order: list[str] = [entry["name"] for entry in runs[0]["stages"]]
    seen = set(order)
    for run in runs:
        for entry in run["stages"]:
            if entry["name"] not in seen:
                seen.add(entry["name"])
                order.append(entry["name"])

    cumulative: dict[str, dict[str, Any]] = {}
    for name in order:
        samples = [
            float(entry["ms"])
            for run in runs
            for entry in run["stages"]
            if entry["name"] == name
        ]
        if samples:
            cumulative[name] = metric(samples)

    transitions: dict[str, list[float]] = {}
    for run in runs:
        previous: str | None = None
        previous_ms = 0.0
        for entry in run["stages"]:
            name, ms = entry["name"], float(entry["ms"])
            label = name if previous is None else f"{previous}__to__{name}"
            transitions.setdefault(label, []).append(ms - previous_ms)
            previous, previous_ms = name, ms
    durations = {label: metric(samples) for label, samples in transitions.items()}

    return {"order": order, "cumulative": cumulative, "durations": durations}


def metric(values: list[float]) -> dict[str, Any]:
    return {
        "min": min(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "max": max(values),
        "samples": values,
    }


def phase_metrics(runs: list[dict[str, Any]]) -> dict[str, Any]:
    result = {}
    timelines = [
        {entry["name"]: float(entry["ms"]) for entry in run["stages"]}
        for run in runs
    ]
    for phase, (start, end) in PHASES.items():
        samples = [marks[end] - marks[start] for marks in timelines
                   if start in marks and end in marks]
        if samples:
            result[phase] = metric(samples)
    return result


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default="target/release/otlyra")
    parser.add_argument(
        "--baseline-binary", type=pathlib.Path,
        help="alternate candidate and baseline launches to control system load drift",
    )
    parser.add_argument("--output", type=pathlib.Path, default="target/startup-benchmark.json")
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--width", type=int, default=1024)
    parser.add_argument("--height", type=int, default=768)
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--url", default="about:about", help="startup workload URL, including file:// fixtures")
    renderer = parser.add_mutually_exclusive_group()
    renderer.add_argument(
        "--cpu-raster",
        action="store_true",
        help="measure CPU rasterization instead of the default direct GPU path",
    )
    renderer.add_argument(
        "--direct-gpu",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    parser.add_argument("--check", action="store_true", help="fail when a budget is exceeded")
    return parser.parse_args()


def main() -> int:
    args = arguments()
    if args.samples < 20:
        raise SystemExit("--samples must be at least 20 so p95 is meaningful")
    if args.warmups < 0:
        raise SystemExit("--warmups cannot be negative")

    repo = pathlib.Path(__file__).resolve().parents[1]
    binary = (repo / args.binary).resolve() if not args.binary.is_absolute() else args.binary
    output = (repo / args.output).resolve() if not args.output.is_absolute() else args.output
    if not binary.is_file():
        raise SystemExit(f"release binary does not exist: {binary}")
    baseline = None
    if args.baseline_binary is not None:
        baseline = (repo / args.baseline_binary).resolve()
        if not baseline.is_file():
            raise SystemExit(f"baseline binary does not exist: {baseline}")

    runs: list[dict[str, Any]] = []
    baseline_runs: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="otlyra-startup-") as directory:
        temporary = pathlib.Path(directory)
        total = args.warmups + args.samples
        for index in range(total):
            launches = [("candidate", binary, runs)]
            if baseline is not None:
                launches.append(("baseline", baseline, baseline_runs))
                if index % 2 == 0:
                    launches.reverse()
            for label, executable, collected in launches:
                profile = temporary / label
                profile.mkdir(exist_ok=True)
                report = profile / f"run-{index:03}.json"
                value = one_run(
                    executable, report, args.width, args.height, args.timeout, args.cpu_raster,
                    args.url,
                )
                if index >= args.warmups:
                    collected.append(value)
                phase = "warmup" if index < args.warmups else "sample"
                print(
                    f"{phase} {index + 1:02}/{total} {label}: "
                    f"visible {value['process_to_visible_ms']:.2f} ms, "
                    f"frame {value['process_to_first_frame_ms']:.2f} ms"
                )

    metric_names = (
        "process_to_visible_ms",
        "process_to_first_frame_ms",
        "visible_to_first_frame_ms",
    )
    geometries = {
        (
            run["physical_width"],
            run["physical_height"],
            run["scale_factor"],
        )
        for run in runs + baseline_runs
    }
    if len(geometries) != 1:
        raise RuntimeError(f"window geometry changed during the benchmark: {geometries!r}")
    metrics = {
        name: metric([float(run[name]) for run in runs])
        for name in metric_names
    }
    stages = stage_metrics(runs)
    failures = []
    for name, percentiles in DEFAULT_BUDGETS.items():
        for percentile_name, budget in percentiles.items():
            actual = metrics[name][percentile_name]
            if actual > budget:
                failures.append(
                    {
                        "metric": name,
                        "percentile": percentile_name,
                        "actual_ms": actual,
                        "budget_ms": budget,
                    }
                )

    first = runs[0]
    summary = {
        "schema": 1,
        "recorded_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "commit": git(repo, "rev-parse", "HEAD"),
        "dirty": bool(git(repo, "status", "--porcelain")),
        "profile": "release",
        "workload_url": args.url,
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "renderer": "cpu" if args.cpu_raster else "direct_gpu",
        "runner": os.environ.get("RUNNER_NAME"),
        "os": platform.platform(),
        "hardware": hardware_name(),
        "logical_width": args.width,
        "logical_height": args.height,
        "physical_width": first["physical_width"],
        "physical_height": first["physical_height"],
        "scale_factor": first["scale_factor"],
        "warmups": args.warmups,
        "sample_count": args.samples,
        "percentile_method": "nearest-rank",
        "budgets_ms": DEFAULT_BUDGETS,
        "metrics": metrics,
        "stages": stages,
        "phases": phase_metrics(runs),
        "failures": failures,
    }
    if baseline is not None:
        summary["comparison"] = {
            "sampling": "alternating-order matched process pairs",
            "baseline": {
                "binary": str(baseline),
                "binary_sha256": hashlib.sha256(baseline.read_bytes()).hexdigest(),
                "sample_count": len(baseline_runs),
                "metrics": {
                    name: metric([float(run[name]) for run in baseline_runs])
                    for name in metric_names
                },
                "stages": stage_metrics(baseline_runs),
                "phases": phase_metrics(baseline_runs),
            },
            "paired_delta_ms": {
                name: metric([after[name] - before[name]
                              for before, after in zip(baseline_runs, runs)])
                for name in metric_names
            },
        }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

    for name in metric_names:
        value = metrics[name]
        print(
            f"{name}: p50 {value['p50']:.2f} ms, "
            f"p95 {value['p95']:.2f} ms "
            f"(min {value['min']:.2f}, max {value['max']:.2f})"
        )

    print("adjacent milestone gaps (p50 / p95 ms; overlapping work uses phases below):")
    ranked = sorted(
        stages["durations"].items(),
        key=lambda item: item[1]["p50"],
        reverse=True,
    )
    largest = ranked[0][0] if ranked else None
    for label, value in stages["durations"].items():
        flag = "  <- largest" if label == largest else ""
        print(f"  {label}: p50 {value['p50']:.2f}, p95 {value['p95']:.2f}{flag}")
    print("phase durations (overlap allowed; p50 / p95 ms):")
    for name, value in summary["phases"].items():
        print(f"  {name}: {value['p50']:.2f} / {value['p95']:.2f}")
    if baseline is not None:
        for name, value in summary["comparison"]["baseline"]["metrics"].items():
            delta = summary["comparison"]["paired_delta_ms"][name]["p50"]
            print(f"baseline {name}: p50 {value['p50']:.2f}, p95 {value['p95']:.2f}; "
                  f"paired candidate-baseline p50 {delta:+.2f} ms")
    print(f"wrote {output}")

    if args.check and failures:
        for failure in failures:
            print(
                "budget exceeded: "
                f"{failure['metric']} {failure['percentile']} "
                f"{failure['actual_ms']:.2f} ms > {failure['budget_ms']:.2f} ms",
                file=sys.stderr,
            )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
