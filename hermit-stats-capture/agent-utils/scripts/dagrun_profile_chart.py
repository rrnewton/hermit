#!/usr/bin/env python3
"""Render deterministic, dependency-free charts from dagrun profiling data.

``speedup`` reads a dagrun ``scaling_model_*.json`` document. ``timeline``
reads a dagrun interval-profile CSV. Its fixed columns are validated before
any rows are selected; sweep metadata may follow the fixed prefix.

The SVGs are intentionally plain, static XML: they remain reviewable in a
diff, render without JavaScript, and require no plotting package. Optional CSV
outputs contain the exact points sent to the renderer.
"""

from __future__ import annotations

import argparse
import csv
import html
import json
import math
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path


PROG = "dagrun-profile-chart"
SVG_WIDTH = 1000
SVG_HEIGHT = 620


class ChartDataError(ValueError):
    """Input cannot identify one well-formed chart series."""


@dataclass(frozen=True)
class SpeedupPoint:
    inner_jobs: int
    speedup: float
    cpu_s: float | None
    peak_bytes: int | None


@dataclass(frozen=True)
class SpeedupSeries:
    step: str
    baseline_inner_jobs: int
    recommended_inner_jobs: int
    regression_inner_jobs: int | None
    points: tuple[SpeedupPoint, ...]


@dataclass(frozen=True)
class TimelinePoint:
    sample_index: int
    sample_kind: str
    elapsed_s: float
    interval_s: float
    effective_cores: float
    inner_jobs: int
    thread_count: int | None


@dataclass(frozen=True)
class TimelineSeries:
    step: str
    inner_jobs: int
    points: tuple[TimelinePoint, ...]


TIMELINE_COLUMNS = (
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


def _as_object(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ChartDataError(f"{where} must be an object")
    return {str(key): item for key, item in value.items()}


def _required_string(data: Mapping[str, object], key: str, where: str) -> str:
    value = data.get(key)
    if not isinstance(value, str) or not value:
        raise ChartDataError(f"{where}.{key} must be a non-empty string")
    return value


def _required_int(data: Mapping[str, object], key: str, where: str, *, minimum: int) -> int:
    value = data.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ChartDataError(f"{where}.{key} must be an integer >= {minimum}")
    return value


def _optional_int(data: Mapping[str, object], key: str, where: str, *, minimum: int) -> int | None:
    value = data.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ChartDataError(f"{where}.{key} must be null or an integer >= {minimum}")
    return value


def _finite_number(value: object, where: str, *, minimum: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        raise ChartDataError(f"{where} must be a finite number >= {minimum:g}")
    try:
        result = float(value)
    except ValueError as exc:
        raise ChartDataError(f"{where} is not a number ({value!r})") from exc
    if not math.isfinite(result) or result < minimum:
        raise ChartDataError(f"{where} must be a finite number >= {minimum:g}")
    return result


def _optional_number(
    data: Mapping[str, object], key: str, where: str, *, minimum: float
) -> float | None:
    value = data.get(key)
    if value is None:
        return None
    return _finite_number(value, f"{where}.{key}", minimum=minimum)


def _optional_nonnegative_int(
    data: Mapping[str, object], key: str, where: str
) -> int | None:
    value = data.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ChartDataError(f"{where}.{key} must be null or a non-negative integer")
    return value


def _reject_json_constant(token: str) -> object:
    raise ChartDataError(f"model contains non-finite JSON constant {token!r}")


def load_speedup_series(path: Path, step: str) -> SpeedupSeries:
    """Load exactly one step's speedup curve from a scaling-model document."""
    try:
        raw: object = json.loads(
            path.read_text(encoding="utf-8"), parse_constant=_reject_json_constant
        )
    except json.JSONDecodeError as exc:
        raise ChartDataError(f"{path}: invalid JSON: {exc}") from exc
    document = _as_object(raw, str(path))
    steps_raw = document.get("steps")
    if not isinstance(steps_raw, list):
        raise ChartDataError(f"{path}: top-level 'steps' must be a list")

    entries: list[tuple[int, dict[str, object]]] = []
    known_steps: list[str] = []
    for index, item in enumerate(steps_raw):
        entry = _as_object(item, f"{path}: steps[{index}]")
        tag = _required_string(entry, "step", f"{path}: steps[{index}]")
        known_steps.append(tag)
        if tag == step:
            entries.append((index, entry))
    if not entries:
        known = ", ".join(repr(tag) for tag in sorted(set(known_steps))) or "(none)"
        raise ChartDataError(f"{path}: no model for step {step!r}; available steps: {known}")
    if len(entries) != 1:
        raise ChartDataError(
            f"{path}: found {len(entries)} model entries for step {step!r}; selection is ambiguous"
        )

    index, entry = entries[0]
    where = f"{path}: steps[{index}]"
    baseline = _required_int(entry, "baseline_inner_jobs", where, minimum=1)
    recommended = _required_int(entry, "recommended_inner_jobs", where, minimum=1)
    regression = _optional_int(entry, "regression_inner_jobs", where, minimum=1)
    levels_raw = entry.get("levels")
    if not isinstance(levels_raw, list) or not levels_raw:
        raise ChartDataError(f"{where}.levels must be a non-empty list")

    points: list[SpeedupPoint] = []
    seen_widths: set[int] = set()
    for level_index, item in enumerate(levels_raw):
        level_where = f"{where}.levels[{level_index}]"
        level = _as_object(item, level_where)
        width = _required_int(level, "inner_jobs", level_where, minimum=1)
        if width in seen_widths:
            raise ChartDataError(f"{where}.levels has duplicate inner_jobs={width}")
        seen_widths.add(width)
        points.append(
            SpeedupPoint(
                inner_jobs=width,
                speedup=_finite_number(level.get("speedup"), f"{level_where}.speedup", minimum=0.0),
                cpu_s=_optional_number(level, "cpu_s", level_where, minimum=0.0),
                peak_bytes=_optional_nonnegative_int(level, "peak_bytes", level_where),
            )
        )
    points.sort(key=lambda point: point.inner_jobs)
    if baseline not in seen_widths:
        raise ChartDataError(f"{where}.baseline_inner_jobs={baseline} has no level")
    if recommended not in seen_widths:
        raise ChartDataError(f"{where}.recommended_inner_jobs={recommended} has no level")
    if regression is not None and regression not in seen_widths:
        raise ChartDataError(f"{where}.regression_inner_jobs={regression} has no level")
    return SpeedupSeries(step, baseline, recommended, regression, tuple(points))


def _csv_cell(row: Mapping[str, str | None], key: str, row_number: int) -> str:
    value = row.get(key)
    if value is None or not value.strip():
        raise ChartDataError(f"trace row {row_number}: {key} is missing")
    return value.strip()


def _csv_float(
    row: Mapping[str, str | None], key: str, row_number: int, *, minimum: float
) -> float:
    value = _csv_cell(row, key, row_number)
    try:
        parsed = float(value)
    except ValueError as exc:
        raise ChartDataError(f"trace row {row_number}: {key} is not a number ({value!r})") from exc
    if not math.isfinite(parsed) or parsed < minimum:
        raise ChartDataError(
            f"trace row {row_number}: {key} must be a finite number >= {minimum:g}"
        )
    return parsed


def _csv_int(row: Mapping[str, str | None], key: str, row_number: int, *, minimum: int) -> int:
    value = _csv_cell(row, key, row_number)
    try:
        parsed = int(value)
    except ValueError as exc:
        raise ChartDataError(f"trace row {row_number}: {key} is not an integer ({value!r})") from exc
    if parsed < minimum:
        raise ChartDataError(f"trace row {row_number}: {key} must be >= {minimum}")
    return parsed


def _csv_optional_int(
    row: Mapping[str, str | None], key: str, row_number: int, *, minimum: int
) -> int | None:
    value = row.get(key)
    if value is None or not value.strip():
        return None
    return _csv_int(row, key, row_number, minimum=minimum)


def load_timeline_series(path: Path, step: str) -> TimelineSeries:
    """Load the plottable interval rows from one unambiguous step run."""
    try:
        with path.open(newline="", encoding="utf-8") as handle:
            reader = csv.DictReader(handle)
            if reader.fieldnames is None:
                raise ChartDataError(f"{path}: trace has no header")
            fieldnames = tuple(reader.fieldnames)
            rows = list(reader)
    except csv.Error as exc:
        raise ChartDataError(f"{path}: invalid CSV: {exc}") from exc

    actual_prefix = fieldnames[: len(TIMELINE_COLUMNS)]
    if actual_prefix != TIMELINE_COLUMNS:
        expected = ",".join(TIMELINE_COLUMNS)
        actual = ",".join(fieldnames) or "(none)"
        raise ChartDataError(
            f"{path}: unexpected trace columns; expected fixed prefix {expected}; got {actual}"
        )

    selected: list[tuple[int, Mapping[str, str | None]]] = []
    known_steps: set[str] = set()
    for row_number, row in enumerate(rows, start=2):
        raw_tag = row.get("step")
        if raw_tag is None or not raw_tag.strip():
            raise ChartDataError(f"trace row {row_number}: step is missing")
        tag = raw_tag.strip()
        known_steps.add(tag)
        if tag == step:
            selected.append((row_number, row))
    if not selected:
        known = ", ".join(repr(tag) for tag in sorted(known_steps)) or "(none)"
        raise ChartDataError(f"{path}: no trace rows for step {step!r}; available steps: {known}")

    run_ids = {_csv_cell(row, "run_id", row_number) for row_number, row in selected}
    if len(run_ids) != 1:
        rendered = ", ".join(repr(run_id) for run_id in sorted(run_ids))
        raise ChartDataError(
            f"{path}: step {step!r} has multiple run_id values ({rendered}); selection is ambiguous"
        )

    points: list[TimelinePoint] = []
    widths: set[int] = set()
    for row_number, selected_row in selected:
        width = _csv_int(selected_row, "inner_jobs", row_number, minimum=1)
        widths.add(width)
        interval_cell = selected_row.get("interval_s")
        effective_cell = selected_row.get("effective_cores")
        if (
            interval_cell is None
            or not interval_cell.strip()
            or effective_cell is None
            or not effective_cell.strip()
        ):
            continue
        sample_kind = _csv_cell(selected_row, "sample_kind", row_number)
        interval_s = _csv_float(selected_row, "interval_s", row_number, minimum=0.0)
        if interval_s <= 0.0:
            raise ChartDataError(f"trace row {row_number}: interval_s must be > 0")
        points.append(
            TimelinePoint(
                sample_index=_csv_int(selected_row, "sample_index", row_number, minimum=0),
                sample_kind=sample_kind,
                elapsed_s=_csv_float(selected_row, "elapsed_s", row_number, minimum=0.0),
                interval_s=interval_s,
                effective_cores=_csv_float(
                    selected_row, "effective_cores", row_number, minimum=0.0
                ),
                inner_jobs=width,
                thread_count=_csv_optional_int(
                    selected_row, "thread_count", row_number, minimum=0
                ),
            )
        )
    if len(widths) != 1:
        rendered = ", ".join(str(width) for width in sorted(widths))
        raise ChartDataError(
            f"{path}: step {step!r} has multiple inner_jobs values ({rendered}); selection is ambiguous"
        )
    if not points:
        raise ChartDataError(
            f"{path}: step {step!r} has no rows with numeric interval_s and effective_cores"
        )

    for previous, current in zip(points, points[1:]):
        if current.elapsed_s <= previous.elapsed_s:
            raise ChartDataError(
                f"{path}: elapsed_s must increase strictly within step {step!r}"
            )
        if current.sample_index <= previous.sample_index:
            raise ChartDataError(
                f"{path}: sample_index must increase strictly within step {step!r}"
            )
    return TimelineSeries(step=step, inner_jobs=next(iter(widths)), points=tuple(points))


def _escape(text: object) -> str:
    return html.escape(str(text), quote=True)


def _number_text(value: float) -> str:
    return format(value, ".12g")


def _axis_text(value: float) -> str:
    if abs(value) >= 100.0:
        return f"{value:.0f}"
    if abs(value) >= 10.0:
        return f"{value:.1f}".rstrip("0").rstrip(".")
    return f"{value:.2f}".rstrip("0").rstrip(".")


def _nice_ceiling(value: float) -> float:
    if value <= 0.0:
        return 1.0
    magnitude = 10.0 ** math.floor(math.log10(value))
    normalized = value / magnitude
    for candidate in (1.0, 2.0, 5.0, 10.0):
        if normalized <= candidate:
            return candidate * magnitude
    return 10.0 * magnitude


def _polyline(points: Sequence[tuple[float, float]]) -> str:
    return " ".join(f"{x:.2f},{y:.2f}" for x, y in points)


def _step_path(points: Sequence[tuple[float, float]]) -> str:
    if not points:
        return ""
    commands = [f"M {points[0][0]:.2f} {points[0][1]:.2f}"]
    for x_value, y_value in points[1:]:
        commands.append(f"H {x_value:.2f} V {y_value:.2f}")
    return " ".join(commands)


def _interval_path(segments: Sequence[tuple[float, float, float]]) -> str:
    """Draw each measured interval over the time window that produced its rate."""
    commands: list[str] = []
    previous_end: float | None = None
    for start, end, y_value in segments:
        if previous_end is None or not math.isclose(start, previous_end, abs_tol=0.01):
            commands.append(f"M {start:.2f} {y_value:.2f}")
        else:
            commands.append(f"V {y_value:.2f}")
        commands.append(f"H {end:.2f}")
        previous_end = end
    return " ".join(commands)


def _base_svg(title: str, description: str, body: Sequence[str]) -> str:
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SVG_WIDTH} {SVG_HEIGHT}" '
            'role="img" aria-labelledby="chart-title chart-desc">'
        ),
        f'  <title id="chart-title">{_escape(title)}</title>',
        f'  <desc id="chart-desc">{_escape(description)}</desc>',
        "  <style>",
        "    text { font-family: ui-sans-serif, system-ui, sans-serif; fill: #172033; }",
        "    .title { font-size: 22px; font-weight: 700; }",
        "    .subtitle { font-size: 12px; fill: #526077; }",
        "    .axis { stroke: #667085; stroke-width: 1; }",
        "    .grid { stroke: #d7dde8; stroke-width: 1; }",
        "    .tick { font-size: 11px; fill: #526077; }",
        "    .legend { font-size: 12px; }",
        "    .context { font-size: 11px; }",
        "  </style>",
        *body,
        "</svg>",
        "",
    ]
    return "\n".join(lines)


def render_speedup_svg(series: SpeedupSeries) -> str:
    """Render one speedup curve with a clipped ideal line and resource context."""
    left, right, top, bottom = 82.0, 950.0, 84.0, 400.0
    plot_width = right - left
    plot_height = bottom - top
    min_width = min(point.inner_jobs for point in series.points)
    max_width = max(point.inner_jobs for point in series.points)
    log_min = math.log2(min_width)
    log_span = math.log2(max_width) - log_min

    def x_position(width: int) -> float:
        if log_span == 0.0:
            return (left + right) / 2.0
        return left + (math.log2(width) - log_min) / log_span * plot_width

    observed_max = max(point.speedup for point in series.points)
    y_ceiling = _nice_ceiling(max(1.0, observed_max) * 1.12)

    def y_position(value: float) -> float:
        return bottom - min(max(value, 0.0), y_ceiling) / y_ceiling * plot_height

    ideal_raw = [point.inner_jobs / series.baseline_inner_jobs for point in series.points]
    ideal_clipped = [min(value, y_ceiling) for value in ideal_raw]
    ideal_was_clipped = any(raw > clipped for raw, clipped in zip(ideal_raw, ideal_clipped))
    observed_points = [(x_position(point.inner_jobs), y_position(point.speedup)) for point in series.points]
    ideal_points = [
        (x_position(point.inner_jobs), y_position(ideal))
        for point, ideal in zip(series.points, ideal_clipped)
    ]

    body = [
        f'  <text x="{left:.0f}" y="38" class="title">Parallel speedup: {_escape(series.step)}</text>',
        (
            f'  <text x="{left:.0f}" y="59" class="subtitle">'
            "Observed wall-time speedup; widths are spaced by log2(inner_jobs).</text>"
        ),
    ]
    for tick_index in range(6):
        value = y_ceiling * tick_index / 5.0
        y_value = y_position(value)
        body.extend(
            [
                f'  <line x1="{left:.2f}" y1="{y_value:.2f}" x2="{right:.2f}" y2="{y_value:.2f}" class="grid"/>',
                f'  <text x="{left - 10:.2f}" y="{y_value + 4:.2f}" text-anchor="end" class="tick">{_escape(_axis_text(value))}×</text>',
            ]
        )
    body.extend(
        [
            f'  <line x1="{left:.2f}" y1="{top:.2f}" x2="{left:.2f}" y2="{bottom:.2f}" class="axis"/>',
            f'  <line x1="{left:.2f}" y1="{bottom:.2f}" x2="{right:.2f}" y2="{bottom:.2f}" class="axis"/>',
        ]
    )
    for point in series.points:
        x_value = x_position(point.inner_jobs)
        body.extend(
            [
                f'  <line x1="{x_value:.2f}" y1="{bottom:.2f}" x2="{x_value:.2f}" y2="{bottom + 5:.2f}" class="axis"/>',
                f'  <text x="{x_value:.2f}" y="{bottom + 22:.2f}" text-anchor="middle" class="tick">{point.inner_jobs}</text>',
            ]
        )
    body.append(
        f'  <text x="{(left + right) / 2:.2f}" y="{bottom + 43:.2f}" text-anchor="middle" class="tick">inner_jobs (log2 spacing)</text>'
    )
    body.append(
        f'  <polyline data-series="ideal-speedup" data-clipped="{str(ideal_was_clipped).lower()}" points="{_polyline(ideal_points)}" fill="none" stroke="#98a2b3" stroke-width="2" stroke-dasharray="7 5"/>'
    )
    body.append(
        f'  <polyline data-series="observed-speedup" points="{_polyline(observed_points)}" fill="none" stroke="#1769aa" stroke-width="3"/>'
    )
    for point, (x_value, y_value) in zip(series.points, observed_points):
        body.append(
            f'  <circle data-width="{point.inner_jobs}" cx="{x_value:.2f}" cy="{y_value:.2f}" r="4.5" fill="#1769aa" stroke="#ffffff" stroke-width="1.5"/>'
        )

    recommended_x = x_position(series.recommended_inner_jobs)
    body.extend(
        [
            f'  <line data-marker="recommended" x1="{recommended_x:.2f}" y1="{top:.2f}" x2="{recommended_x:.2f}" y2="{bottom:.2f}" stroke="#16825d" stroke-width="2" stroke-dasharray="3 4"/>',
            f'  <text x="{recommended_x:.2f}" y="{top + 15:.2f}" text-anchor="middle" class="legend" fill="#116149">recommended: j{series.recommended_inner_jobs}</text>',
        ]
    )
    if series.regression_inner_jobs is not None:
        regression_x = x_position(series.regression_inner_jobs)
        body.extend(
            [
                f'  <line data-marker="regression" x1="{regression_x:.2f}" y1="{top:.2f}" x2="{regression_x:.2f}" y2="{bottom:.2f}" stroke="#c2413b" stroke-width="2" stroke-dasharray="3 4"/>',
                f'  <text x="{regression_x:.2f}" y="{top + 32:.2f}" text-anchor="middle" class="legend" fill="#9f302b">regression: j{series.regression_inner_jobs}</text>',
            ]
        )
    ideal_label = "Ideal linear (clipped at chart ceiling)" if ideal_was_clipped else "Ideal linear"
    body.extend(
        [
            f'  <line x1="{right - 260:.2f}" y1="55" x2="{right - 225:.2f}" y2="55" stroke="#98a2b3" stroke-width="2" stroke-dasharray="7 5"/>',
            f'  <text x="{right - 215:.2f}" y="59" class="legend">{ideal_label}</text>',
            f'  <line x1="{right - 260:.2f}" y1="35" x2="{right - 225:.2f}" y2="35" stroke="#1769aa" stroke-width="3"/>',
            f'  <text x="{right - 215:.2f}" y="39" class="legend">Observed speedup</text>',
        ]
    )

    by_width = {point.inner_jobs: point for point in series.points}
    context_widths = [series.baseline_inner_jobs, series.recommended_inner_jobs]
    if series.regression_inner_jobs is not None:
        context_widths.append(series.regression_inner_jobs)
    context_widths.append(max_width)
    unique_context = list(dict.fromkeys(context_widths))
    baseline_cpu = by_width[series.baseline_inner_jobs].cpu_s
    panel_y = 468.0
    body.extend(
        [
            f'  <rect data-context="cpu-memory" x="{left:.2f}" y="{panel_y:.2f}" width="{plot_width:.2f}" height="115" rx="5" fill="#f5f7fa" stroke="#d7dde8"/>',
            f'  <text x="{left + 14:.2f}" y="{panel_y + 22:.2f}" class="legend" font-weight="700">CPU-growth / memory context</text>',
        ]
    )
    column_width = (plot_width - 28.0) / len(unique_context)
    for index, width in enumerate(unique_context):
        point = by_width[width]
        x_value = left + 14.0 + index * column_width
        cpu_growth = (
            point.cpu_s / baseline_cpu
            if point.cpu_s is not None and baseline_cpu is not None and baseline_cpu > 0.0
            else None
        )
        cpu_text = "n/a" if cpu_growth is None else f"{cpu_growth:.2f}× base"
        memory_text = "n/a" if point.peak_bytes is None else f"{point.peak_bytes / (1024.0 ** 2):.1f} MiB"
        labels: list[str] = []
        if width == series.baseline_inner_jobs:
            labels.append("baseline")
        if width == series.recommended_inner_jobs:
            labels.append("recommended")
        if width == series.regression_inner_jobs:
            labels.append("regression")
        if width == max_width:
            labels.append("widest")
        suffix = f" ({', '.join(labels)})" if labels else ""
        body.extend(
            [
                f'  <text x="{x_value:.2f}" y="{panel_y + 47:.2f}" class="context" font-weight="700">j{width}{_escape(suffix)}</text>',
                f'  <text x="{x_value:.2f}" y="{panel_y + 69:.2f}" class="context">CPU work: {_escape(cpu_text)}</text>',
                f'  <text x="{x_value:.2f}" y="{panel_y + 90:.2f}" class="context">Peak memory: {_escape(memory_text)}</text>',
            ]
        )

    description = (
        f"Observed parallel speedup for {series.step}. Recommended inner_jobs is "
        f"{series.recommended_inner_jobs}. "
        + (
            f"Regression begins at {series.regression_inner_jobs}. "
            if series.regression_inner_jobs is not None
            else "No regression point is recorded. "
        )
        + "A compact panel reports CPU work growth and peak memory."
    )
    return _base_svg(f"Parallel speedup: {series.step}", description, body)


def render_timeline_svg(series: TimelineSeries) -> str:
    """Render achieved cores and requested jobs with thread count on a secondary axis."""
    left, right, top, bottom = 82.0, 910.0, 88.0, 475.0
    plot_width = right - left
    plot_height = bottom - top
    max_elapsed = max(point.elapsed_s for point in series.points)
    if max_elapsed <= 0.0:
        raise ChartDataError(f"step {series.step!r}: elapsed_s never advances beyond zero")
    core_ceiling = _nice_ceiling(
        max(float(series.inner_jobs), max(point.effective_cores for point in series.points)) * 1.08
    )
    observed_threads = [
        point.thread_count for point in series.points if point.thread_count is not None
    ]
    thread_ceiling = _nice_ceiling(
        max(1.0, float(max(observed_threads))) * 1.08 if observed_threads else 1.0
    )

    def x_position(elapsed: float) -> float:
        return left + elapsed / max_elapsed * plot_width

    def core_y(value: float) -> float:
        return bottom - min(max(value, 0.0), core_ceiling) / core_ceiling * plot_height

    def thread_y(value: int) -> float:
        return bottom - min(max(float(value), 0.0), thread_ceiling) / thread_ceiling * plot_height

    core_segments = [
        (
            x_position(max(0.0, point.elapsed_s - point.interval_s)),
            x_position(point.elapsed_s),
            core_y(point.effective_cores),
        )
        for point in series.points
    ]
    threads = [
        (x_position(point.elapsed_s), thread_y(point.thread_count))
        for point in series.points
        if point.thread_count is not None
    ]
    requested_y = core_y(float(series.inner_jobs))
    body = [
        f'  <text x="{left:.0f}" y="38" class="title">Parallelism over time: {_escape(series.step)}</text>',
        f'  <text x="{left:.0f}" y="60" class="subtitle">Interval CPU use, requested inner_jobs, and observed process thread count.</text>',
    ]
    for tick_index in range(6):
        fraction = tick_index / 5.0
        y_value = bottom - fraction * plot_height
        cores_value = core_ceiling * fraction
        threads_value = thread_ceiling * fraction
        body.extend(
            [
                f'  <line x1="{left:.2f}" y1="{y_value:.2f}" x2="{right:.2f}" y2="{y_value:.2f}" class="grid"/>',
                f'  <text x="{left - 10:.2f}" y="{y_value + 4:.2f}" text-anchor="end" class="tick">{_escape(_axis_text(cores_value))}</text>',
                f'  <text x="{right + 10:.2f}" y="{y_value + 4:.2f}" class="tick">{_escape(_axis_text(threads_value))}</text>',
            ]
        )
    body.extend(
        [
            f'  <line x1="{left:.2f}" y1="{top:.2f}" x2="{left:.2f}" y2="{bottom:.2f}" class="axis"/>',
            f'  <line x1="{right:.2f}" y1="{top:.2f}" x2="{right:.2f}" y2="{bottom:.2f}" class="axis"/>',
            f'  <line x1="{left:.2f}" y1="{bottom:.2f}" x2="{right:.2f}" y2="{bottom:.2f}" class="axis"/>',
            f'  <text x="20" y="{(top + bottom) / 2:.2f}" text-anchor="middle" class="tick" transform="rotate(-90 20 {(top + bottom) / 2:.2f})">effective cores</text>',
            f'  <text x="982" y="{(top + bottom) / 2:.2f}" text-anchor="middle" class="tick" transform="rotate(90 982 {(top + bottom) / 2:.2f})">thread count</text>',
        ]
    )
    for tick_index in range(5):
        elapsed = max_elapsed * tick_index / 4.0
        x_value = x_position(elapsed)
        body.extend(
            [
                f'  <line x1="{x_value:.2f}" y1="{bottom:.2f}" x2="{x_value:.2f}" y2="{bottom + 5:.2f}" class="axis"/>',
                f'  <text x="{x_value:.2f}" y="{bottom + 22:.2f}" text-anchor="middle" class="tick">{_escape(_axis_text(elapsed))}</text>',
            ]
        )
    body.append(
        f'  <text x="{(left + right) / 2:.2f}" y="{bottom + 44:.2f}" text-anchor="middle" class="tick">elapsed seconds</text>'
    )
    body.extend(
        [
            f'  <line data-series="requested-inner-jobs" x1="{left:.2f}" y1="{requested_y:.2f}" x2="{right:.2f}" y2="{requested_y:.2f}" stroke="#8f5bd7" stroke-width="2" stroke-dasharray="8 5"/>',
            f'  <path data-series="effective-cores" d="{_interval_path(core_segments)}" fill="none" stroke="#1769aa" stroke-width="3"/>',
            f'  <path data-series="thread-count" d="{_step_path(threads)}" fill="none" stroke="#d97706" stroke-width="2.5"/>',
        ]
    )
    for point, (_start_x, x_value, y_value) in zip(series.points, core_segments):
        body.append(
            f'  <circle data-elapsed-s="{_escape(_number_text(point.elapsed_s))}" cx="{x_value:.2f}" cy="{y_value:.2f}" r="3.5" fill="#1769aa"/>'
        )
    legend_y = 557.0
    body.extend(
        [
            f'  <line x1="{left:.2f}" y1="{legend_y:.2f}" x2="{left + 35:.2f}" y2="{legend_y:.2f}" stroke="#1769aa" stroke-width="3"/>',
            f'  <text x="{left + 44:.2f}" y="{legend_y + 4:.2f}" class="legend">Interval effective cores</text>',
            f'  <line x1="{left + 255:.2f}" y1="{legend_y:.2f}" x2="{left + 290:.2f}" y2="{legend_y:.2f}" stroke="#8f5bd7" stroke-width="2" stroke-dasharray="8 5"/>',
            f'  <text x="{left + 299:.2f}" y="{legend_y + 4:.2f}" class="legend">Requested inner_jobs: {series.inner_jobs}</text>',
            f'  <line x1="{left + 535:.2f}" y1="{legend_y:.2f}" x2="{left + 570:.2f}" y2="{legend_y:.2f}" stroke="#d97706" stroke-width="2.5"/>',
            f'  <text x="{left + 579:.2f}" y="{legend_y + 4:.2f}" class="legend">Thread count (right axis)</text>',
        ]
    )
    description = (
        f"Parallelism trace for {series.step} at requested inner_jobs {series.inner_jobs}. "
        "The blue line is interval effective CPU cores, the dashed purple line is requested "
        "inner_jobs, and the orange line is thread count on the right axis."
    )
    return _base_svg(f"Parallelism over time: {series.step}", description, body)


def write_speedup_csv(path: Path, series: SpeedupSeries) -> None:
    """Write the exact speedup points, including the ideal values clipped for display."""
    observed_max = max(point.speedup for point in series.points)
    y_ceiling = _nice_ceiling(max(1.0, observed_max) * 1.12)
    baseline_cpu = next(
        point.cpu_s for point in series.points if point.inner_jobs == series.baseline_inner_jobs
    )
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(
            (
                "step",
                "inner_jobs",
                "speedup",
                "ideal_speedup",
                "cpu_s",
                "cpu_growth",
                "peak_bytes",
                "recommended",
                "regression",
            )
        )
        for point in series.points:
            cpu_growth = (
                point.cpu_s / baseline_cpu
                if point.cpu_s is not None and baseline_cpu is not None and baseline_cpu > 0.0
                else None
            )
            writer.writerow(
                (
                    series.step,
                    point.inner_jobs,
                    _number_text(point.speedup),
                    _number_text(
                        min(point.inner_jobs / series.baseline_inner_jobs, y_ceiling)
                    ),
                    "" if point.cpu_s is None else _number_text(point.cpu_s),
                    "" if cpu_growth is None else _number_text(cpu_growth),
                    "" if point.peak_bytes is None else point.peak_bytes,
                    str(point.inner_jobs == series.recommended_inner_jobs).lower(),
                    str(point.inner_jobs == series.regression_inner_jobs).lower(),
                )
            )


def write_timeline_csv(path: Path, series: TimelineSeries) -> None:
    """Write the exact timeline points sent to the renderer."""
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(
            (
                "step",
                "inner_jobs",
                "sample_index",
                "sample_kind",
                "elapsed_s",
                "interval_s",
                "effective_cores",
                "thread_count",
            )
        )
        for point in series.points:
            writer.writerow(
                (
                    series.step,
                    point.inner_jobs,
                    point.sample_index,
                    point.sample_kind,
                    _number_text(point.elapsed_s),
                    _number_text(point.interval_s),
                    _number_text(point.effective_cores),
                    "" if point.thread_count is None else point.thread_count,
                )
            )


def _write_text(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog=PROG, description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    speedup = subparsers.add_parser("speedup", help="chart one step from a scaling model")
    speedup.add_argument("--model", type=Path, required=True)
    speedup.add_argument("--step", required=True)
    speedup.add_argument("--output", type=Path, required=True, metavar="FILE.svg")
    speedup.add_argument("--csv-out", type=Path, metavar="FILE.csv")

    timeline = subparsers.add_parser("timeline", help="chart one step from a sampled CSV trace")
    timeline.add_argument("--trace", type=Path, required=True)
    timeline.add_argument("--step", required=True)
    timeline.add_argument("--output", type=Path, required=True, metavar="FILE.svg")
    timeline.add_argument("--csv-out", type=Path, metavar="FILE.csv")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        command = str(args.command)
        step = str(args.step)
        output = Path(args.output)
        csv_out = Path(args.csv_out) if args.csv_out is not None else None
        if command == "speedup":
            series = load_speedup_series(Path(args.model), step)
            _write_text(output, render_speedup_svg(series))
            if csv_out is not None:
                write_speedup_csv(csv_out, series)
        elif command == "timeline":
            timeline = load_timeline_series(Path(args.trace), step)
            _write_text(output, render_timeline_svg(timeline))
            if csv_out is not None:
                write_timeline_csv(csv_out, timeline)
        else:  # pragma: no cover - argparse constrains this value
            raise ChartDataError(f"unknown command {command!r}")
    except (ChartDataError, OSError) as exc:
        print(f"{PROG}: error: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
