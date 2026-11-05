"""Focused contracts for the dependency-free dagrun profile chart utility."""

from __future__ import annotations

import csv
import importlib.util
import json
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path
from types import ModuleType

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
TRACE_COLUMNS = (
    "timestamp",
    "machine_id",
    "container_class",
    "git_sha",
    "outer_jobs",
    "profile_base_sha",
    "enforcement_kind",
    "runner_name",
    "run_id",
    "step",
    "inner_jobs",
    "sample_index",
    "sample_kind",
    "elapsed_s",
    "interval_s",
    "cpu_usage_s",
    "user_s",
    "sys_s",
    "effective_cores",
    "user_cores",
    "system_cores",
    "throttled_s",
    "interval_throttled_s",
    "thread_count",
)


def _load_chart() -> ModuleType:
    path = REPO_ROOT / "scripts" / "dagrun_profile_chart.py"
    spec = importlib.util.spec_from_file_location("_dagrun_profile_chart_test", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _model(step_entries: list[object]) -> dict[str, object]:
    return {"schema": 2, "machine_id": "test", "steps": step_entries}


def _speedup_entry(step: str = "build.main") -> dict[str, object]:
    return {
        "step": step,
        "baseline_inner_jobs": 1,
        "recommended_inner_jobs": 4,
        "regression_inner_jobs": 8,
        "levels": [
            {"inner_jobs": 8, "speedup": "3.200", "cpu_s": "18.0", "peak_bytes": 80 << 20},
            {"inner_jobs": 1, "speedup": "1.000", "cpu_s": "8.0", "peak_bytes": 20 << 20},
            {"inner_jobs": 4, "speedup": "3.500", "cpu_s": "10.0", "peak_bytes": 44 << 20},
            {"inner_jobs": 2, "speedup": "1.900", "cpu_s": "8.4", "peak_bytes": 28 << 20},
        ],
    }


def _write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value), encoding="utf-8")


def _trace_row(
    *,
    step: str = "build.main",
    run_id: str = "run-a",
    inner_jobs: str = "4",
    sample_index: str = "0",
    sample_kind: str = "start",
    elapsed_s: str = "0",
    interval_s: str = "",
    effective_cores: str = "",
    thread_count: str = "1",
) -> dict[str, str]:
    row = {column: "" for column in TRACE_COLUMNS}
    row.update(
        {
            "timestamp": "2026-08-28T00:00:00Z",
            "machine_id": "test-machine",
            "container_class": "test-container",
            "git_sha": "abc123",
            "outer_jobs": "1",
            "profile_base_sha": "abc123",
            "enforcement_kind": "cgroup-v2",
            "runner_name": "dagrun",
            "run_id": run_id,
            "step": step,
            "inner_jobs": inner_jobs,
            "sample_index": sample_index,
            "sample_kind": sample_kind,
            "elapsed_s": elapsed_s,
            "interval_s": interval_s,
            "effective_cores": effective_cores,
            "thread_count": thread_count,
        }
    )
    return row


def _write_trace(path: Path, rows: list[dict[str, str]], *, columns: tuple[str, ...] = TRACE_COLUMNS) -> None:
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(
            handle, fieldnames=columns, extrasaction="ignore", lineterminator="\n"
        )
        writer.writeheader()
        writer.writerows(rows)


def test_speedup_filters_model_and_renders_stable_accessible_svg(tmp_path: Path) -> None:
    chart = _load_chart()
    model_path = tmp_path / "model.json"
    _write_json(model_path, _model([_speedup_entry(), _speedup_entry("other.step")]))

    series = chart.load_speedup_series(model_path, "build.main")
    assert [point.inner_jobs for point in series.points] == [1, 2, 4, 8]
    assert series.recommended_inner_jobs == 4
    assert series.regression_inner_jobs == 8

    first = chart.render_speedup_svg(series)
    second = chart.render_speedup_svg(series)
    assert first == second
    assert ET.fromstring(first).tag == "{http://www.w3.org/2000/svg}svg"
    assert 'role="img" aria-labelledby="chart-title chart-desc"' in first
    assert '<title id="chart-title">Parallel speedup: build.main</title>' in first
    assert 'data-series="observed-speedup"' in first
    assert 'data-series="ideal-speedup" data-clipped="true"' in first
    assert 'data-marker="recommended"' in first
    assert 'data-marker="regression"' in first
    assert "CPU-growth / memory context" in first
    assert "Peak memory:" in first

    positions = [
        float(position)
        for _width, position in re.findall(r'data-width="(\d+)" cx="([0-9.]+)"', first)
    ]
    assert len(positions) == 4
    assert positions[1] - positions[0] == pytest.approx(positions[2] - positions[1], abs=0.02)
    assert positions[2] - positions[1] == pytest.approx(positions[3] - positions[2], abs=0.02)

    csv_path = tmp_path / "speedup.csv"
    chart.write_speedup_csv(csv_path, series)
    rows = list(csv.DictReader(csv_path.open(newline="", encoding="utf-8")))
    assert [row["inner_jobs"] for row in rows] == ["1", "2", "4", "8"]
    assert [row["recommended"] for row in rows] == ["false", "false", "true", "false"]
    assert rows[-1]["ideal_speedup"] == "5"


def test_speedup_refuses_missing_or_ambiguous_step(tmp_path: Path) -> None:
    chart = _load_chart()
    model_path = tmp_path / "model.json"
    _write_json(model_path, _model([_speedup_entry(), _speedup_entry()]))
    with pytest.raises(chart.ChartDataError, match="selection is ambiguous"):
        chart.load_speedup_series(model_path, "build.main")
    with pytest.raises(chart.ChartDataError, match="available steps"):
        chart.load_speedup_series(model_path, "missing.step")


def test_timeline_filters_step_skips_baseline_and_preserves_sample_kind(tmp_path: Path) -> None:
    chart = _load_chart()
    trace_path = tmp_path / "trace.csv"
    rows = [
        _trace_row(step="other.step"),
        _trace_row(),
        _trace_row(
            sample_index="1",
            sample_kind="periodic",
            elapsed_s="0.5",
            interval_s="0.5",
            effective_cores="2.25",
            thread_count="5",
        ),
        _trace_row(
            sample_index="2",
            sample_kind="final",
            elapsed_s="1.0",
            interval_s="0.5",
            effective_cores="3.5",
            thread_count="7",
        ),
    ]
    _write_trace(trace_path, rows, columns=(*TRACE_COLUMNS, "sweep_pass"))

    series = chart.load_timeline_series(trace_path, "build.main")
    assert [point.sample_kind for point in series.points] == ["periodic", "final"]
    assert [point.effective_cores for point in series.points] == [2.25, 3.5]
    assert [point.thread_count for point in series.points] == [5, 7]

    first = chart.render_timeline_svg(series)
    assert first == chart.render_timeline_svg(series)
    assert ET.fromstring(first).tag == "{http://www.w3.org/2000/svg}svg"
    assert '<title id="chart-title">Parallelism over time: build.main</title>' in first
    assert 'data-series="effective-cores"' in first
    assert 'data-series="requested-inner-jobs"' in first
    assert 'stroke-dasharray="8 5"' in first
    assert 'data-series="thread-count"' in first
    assert "Thread count (right axis)" in first

    csv_path = tmp_path / "timeline.csv"
    chart.write_timeline_csv(csv_path, series)
    plotted = list(csv.DictReader(csv_path.open(newline="", encoding="utf-8")))
    assert [row["sample_kind"] for row in plotted] == ["periodic", "final"]
    assert [row["effective_cores"] for row in plotted] == ["2.25", "3.5"]
    assert [row["inner_jobs"] for row in plotted] == ["4", "4"]


def test_timeline_refuses_wrong_schema_and_ambiguous_runs(tmp_path: Path) -> None:
    chart = _load_chart()
    malformed = tmp_path / "malformed.csv"
    _write_trace(malformed, [_trace_row()], columns=TRACE_COLUMNS[1:])
    with pytest.raises(chart.ChartDataError, match="expected fixed prefix"):
        chart.load_timeline_series(malformed, "build.main")

    ambiguous = tmp_path / "ambiguous.csv"
    rows = [
        _trace_row(
            run_id="run-a",
            sample_index="1",
            sample_kind="periodic",
            elapsed_s="0.5",
            interval_s="0.5",
            effective_cores="2",
        ),
        _trace_row(
            run_id="run-b",
            sample_index="1",
            sample_kind="periodic",
            elapsed_s="0.5",
            interval_s="0.5",
            effective_cores="2",
        ),
    ]
    _write_trace(ambiguous, rows)
    with pytest.raises(chart.ChartDataError, match="selection is ambiguous"):
        chart.load_timeline_series(ambiguous, "build.main")


def test_timeline_refuses_a_step_without_plottable_intervals(tmp_path: Path) -> None:
    chart = _load_chart()
    trace_path = tmp_path / "trace.csv"
    _write_trace(trace_path, [_trace_row()])
    with pytest.raises(chart.ChartDataError, match="no rows with numeric interval_s"):
        chart.load_timeline_series(trace_path, "build.main")


def test_timeline_keeps_effective_cores_when_thread_count_is_missing(tmp_path: Path) -> None:
    chart = _load_chart()
    trace_path = tmp_path / "trace.csv"
    _write_trace(
        trace_path,
        [
            _trace_row(),
            _trace_row(
                sample_index="1",
                sample_kind="final",
                elapsed_s="0.5",
                interval_s="0.5",
                effective_cores="2.25",
                thread_count="",
            ),
        ],
    )

    series = chart.load_timeline_series(trace_path, "build.main")
    assert series.points[0].effective_cores == 2.25
    assert series.points[0].thread_count is None
    assert 'data-series="effective-cores"' in chart.render_timeline_svg(series)

    csv_path = tmp_path / "timeline.csv"
    chart.write_timeline_csv(csv_path, series)
    rows = list(csv.DictReader(csv_path.open(newline="", encoding="utf-8")))
    assert rows[0]["thread_count"] == ""
