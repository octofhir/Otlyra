#!/usr/bin/env python3
"""Measure real window startup distributions and enforce Otlyra's budgets."""

from __future__ import annotations

import argparse
import datetime as dt
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
) -> dict[str, Any]:
    environment = os.environ.copy()
    environment["OTLYRA_LOG"] = "error"
    command = [
        str(binary),
        "--url",
        "about:about",
        "--width",
        str(width),
        "--height",
        str(height),
        "--startup-report",
        str(report),
    ]
    subprocess.run(
        command,
        check=True,
        timeout=timeout,
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
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
    of a stage is the gap to its predecessor in launch order. A run whose trace
    lacks a milestone contributes to neither that milestone nor the two durations
    it bounds, so a stage that only some runs record still gets an honest p50/p95
    from the runs that have it. The largest-duration stage is the one to optimize.
    """
    # Launch order comes from the first run; every run marks in the same order.
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

    durations: dict[str, dict[str, Any]] = {}
    for index, name in enumerate(order):
        previous = order[index - 1] if index > 0 else None
        samples: list[float] = []
        for run in runs:
            marks = {entry["name"]: float(entry["ms"]) for entry in run["stages"]}
            if name not in marks:
                continue
            base = marks[previous] if previous is not None else 0.0
            if previous is not None and previous not in marks:
                continue
            samples.append(marks[name] - base)
        if samples:
            label = name if previous is None else f"{previous}__to__{name}"
            durations[label] = metric(samples)

    return {"order": order, "cumulative": cumulative, "durations": durations}


def metric(values: list[float]) -> dict[str, Any]:
    return {
        "min": min(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "max": max(values),
        "samples": values,
    }


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default="target/release/otlyra")
    parser.add_argument("--output", type=pathlib.Path, default="target/startup-benchmark.json")
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--width", type=int, default=1024)
    parser.add_argument("--height", type=int, default=768)
    parser.add_argument("--timeout", type=float, default=30.0)
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

    runs: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="otlyra-startup-") as directory:
        temporary = pathlib.Path(directory)
        total = args.warmups + args.samples
        for index in range(total):
            report = temporary / f"run-{index:03}.json"
            value = one_run(binary, report, args.width, args.height, args.timeout)
            if index >= args.warmups:
                runs.append(value)
            phase = "warmup" if index < args.warmups else "sample"
            print(
                f"{phase} {index + 1:02}/{total}: "
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
        for run in runs
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
        "failures": failures,
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

    print("per-stage duration (p50 / p95 ms), launch order:")
    ranked = sorted(
        stages["durations"].items(),
        key=lambda item: item[1]["p50"],
        reverse=True,
    )
    largest = ranked[0][0] if ranked else None
    for index, name in enumerate(stages["order"]):
        previous = stages["order"][index - 1] if index > 0 else None
        label = name if previous is None else f"{previous}__to__{name}"
        value = stages["durations"].get(label)
        if value is None:
            continue
        flag = "  <- largest" if label == largest else ""
        print(f"  {label}: p50 {value['p50']:.2f}, p95 {value['p95']:.2f}{flag}")
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
