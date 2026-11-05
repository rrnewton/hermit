#!/usr/bin/env python3
"""Build a deterministic, standalone HTML explorer for dagrun profile history.

The report reads the authoritative ``step_profiles_*.csv`` files, an authored
DAG (JSON or YAML), and any opt-in ``traces/*.csv`` files in a profile store.
Everything needed to explore the result is embedded in one HTML file: there
are no CDN, JavaScript-package, font, or network dependencies.

The browser recomputes normalized speedup and CPU-work efficiency after every
filter change.  This matters because a baseline belongs to one commit,
machine/container environment, and workload revision; normalizing once while
building the report would silently compare unrelated trials.
"""

from __future__ import annotations

import argparse
import csv
import html
import importlib.resources
import json
import math
import os
import secrets
import stat
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import quote

from dagrun import __version__
from dagrun.io import DagJsonError, dag_from_path
from dagrun.model import DagConfig
from dagrun.perflog import _profile_file_lock


PROG = "dagrun-profile-report"
PROFILE_GLOB = "step_profiles_*.csv"
TRACE_GLOB = "*.csv"
PROFILE_REPORT_FILENAME = "profile_report.html"
REPORT_DATA_MARKER = b'<script id="dagrun-report-data" type="application/json">'


class ReportDataError(ValueError):
    """An input cannot be represented faithfully in the report."""


@dataclass(frozen=True)
class GraphStep:
    """One positioned DAG node embedded in the standalone report."""

    tag: str
    desc: str
    description: str
    deps: tuple[str, ...]
    order: int
    layer: int
    x: float
    y: float

    def to_json(self) -> dict[str, object]:
        """Encode this graph node for the report payload."""

        return {
            "tag": self.tag,
            "desc": self.desc,
            "description": self.description,
            "deps": list(self.deps),
            "order": self.order,
            "layer": self.layer,
            "x": self.x,
            "y": self.y,
        }


@dataclass(frozen=True)
class DagDocument:
    """The positioned graph and canvas dimensions used by the report."""

    description: str
    steps: tuple[GraphStep, ...]
    width: int
    height: int


@dataclass(frozen=True)
class ProfileRecord:
    """One aggregate step measurement normalized for browser consumption."""

    step: str
    commit: str
    timestamp: str
    jobs: int
    elapsed_s: float
    cpu_s: float | None
    peak_bytes: int | None
    effective_cores: float | None
    run_id: str
    machine: str
    container: str
    environment: str
    workload: str
    runner: str
    source: str

    def to_json(self) -> dict[str, object]:
        """Encode this aggregate measurement for the report payload."""

        return {
            "step": self.step,
            "commit": self.commit,
            "timestamp": self.timestamp,
            "jobs": self.jobs,
            "elapsed_s": self.elapsed_s,
            "cpu_s": self.cpu_s,
            "peak_bytes": self.peak_bytes,
            "effective_cores": self.effective_cores,
            "run_id": self.run_id,
            "machine": self.machine,
            "container": self.container,
            "environment": self.environment,
            "workload": self.workload,
            "runner": self.runner,
            "source": self.source,
        }


@dataclass(frozen=True)
class TracePoint:
    """One interval sample from a step parallelism trace."""

    sample_index: int
    sample_kind: str
    elapsed_s: float
    interval_s: float | None
    effective_cores: float | None
    user_cores: float | None
    system_cores: float | None
    thread_count: int | None
    throttled_s: float | None

    def to_json(self) -> dict[str, object]:
        """Encode this interval sample for the report payload."""

        return {
            "sample_index": self.sample_index,
            "sample_kind": self.sample_kind,
            "elapsed_s": self.elapsed_s,
            "interval_s": self.interval_s,
            "effective_cores": self.effective_cores,
            "user_cores": self.user_cores,
            "system_cores": self.system_cores,
            "thread_count": self.thread_count,
            "throttled_s": self.throttled_s,
        }


@dataclass(frozen=True)
class TraceSeries:
    """A complete within-step parallelism trace with cohort provenance."""

    key: str
    step: str
    commit: str
    timestamp: str
    jobs: int
    run_id: str
    machine: str
    container: str
    environment: str
    workload: str
    source: str
    points: tuple[TracePoint, ...]

    def to_json(self) -> dict[str, object]:
        """Encode this trace series for the report payload."""

        return {
            "key": self.key,
            "step": self.step,
            "commit": self.commit,
            "timestamp": self.timestamp,
            "jobs": self.jobs,
            "run_id": self.run_id,
            "machine": self.machine,
            "container": self.container,
            "environment": self.environment,
            "workload": self.workload,
            "source": self.source,
            "points": [point.to_json() for point in self.points],
        }


@dataclass(frozen=True)
class CaptureArtifactView:
    """One profiler artifact and its safe report-relative link."""

    role: str
    path: str
    href: str
    size_bytes: int
    mode: str
    exists: bool

    def to_json(self) -> dict[str, object]:
        """Encode this artifact for the report payload."""

        return {
            "role": self.role,
            "path": self.path,
            "href": self.href,
            "size_bytes": self.size_bytes,
            "mode": self.mode,
            "exists": self.exists,
        }


@dataclass(frozen=True)
class CaptureTrialView:
    """One profiler-instrumented trial shown in the report."""

    trial_id: str
    kind: str
    state: str
    inner_jobs: int
    started_at: str
    finished_at: str
    measured_wall_s: float | None
    workload_returncode: int | None
    profiler_returncode: int | None
    error: str
    included_in_model: bool
    artifacts: tuple[CaptureArtifactView, ...]

    def to_json(self) -> dict[str, object]:
        """Encode this profiler trial for the report payload."""

        return {
            "trial_id": self.trial_id,
            "kind": self.kind,
            "state": self.state,
            "inner_jobs": self.inner_jobs,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "measured_wall_s": self.measured_wall_s,
            "workload_returncode": self.workload_returncode,
            "profiler_returncode": self.profiler_returncode,
            "error": self.error,
            "included_in_model": self.included_in_model,
            "artifacts": [artifact.to_json() for artifact in self.artifacts],
        }


@dataclass(frozen=True)
class CaptureView:
    """One capture session with environment and workload provenance."""

    capture_id: str
    state: str
    created_at: str
    finished_at: str
    step: str
    commit: str
    machine: str
    container: str
    environment: str
    workload: str
    jobs: int
    expected_wall_s: float | None
    speedup: float | None
    manifest_path: str
    errors: tuple[str, ...]
    trials: tuple[CaptureTrialView, ...]

    def to_json(self) -> dict[str, object]:
        """Encode this capture session for the report payload."""

        return {
            "capture_id": self.capture_id,
            "state": self.state,
            "created_at": self.created_at,
            "finished_at": self.finished_at,
            "step": self.step,
            "commit": self.commit,
            "machine": self.machine,
            "container": self.container,
            "environment": self.environment,
            "workload": self.workload,
            "jobs": self.jobs,
            "expected_wall_s": self.expected_wall_s,
            "speedup": self.speedup,
            "manifest_path": self.manifest_path,
            "errors": list(self.errors),
            "trials": [trial.to_json() for trial in self.trials],
        }


def load_dag_config(path: Path) -> DagConfig:
    """Load a DAG through the runner's canonical JSON/YAML schema implementation."""
    try:
        return dag_from_path(path)
    except DagJsonError as exc:
        raise ReportDataError(str(exc)) from exc


def dag_document_from_config(cfg: DagConfig) -> DagDocument:
    """Assign a stable left-to-right report layout to an already validated DAG."""
    if not cfg.steps:
        raise ReportDataError("DAG must contain at least one step for a profile report")
    parsed = [
        (step.tag, step.desc, step.description, tuple(step.deps)) for step in cfg.steps
    ]
    known = {tag for tag, _desc, _long, _deps in parsed}

    order_by_tag = {tag: index for index, (tag, _desc, _long, _deps) in enumerate(parsed)}
    successors: dict[str, list[str]] = {tag: [] for tag in order_by_tag}
    indegree: dict[str, int] = {tag: 0 for tag in order_by_tag}
    for tag, _desc, _long, deps in parsed:
        for dep in deps:
            if dep not in known:
                raise ReportDataError(f"step {tag!r} has unknown dependency {dep!r}")
            successors[dep].append(tag)
            indegree[tag] += 1

    ready = sorted((order_by_tag[tag], tag) for tag, degree in indegree.items() if degree == 0)
    topo: list[str] = []
    while ready:
        _order, tag = ready.pop(0)
        topo.append(tag)
        for successor in sorted(successors[tag], key=order_by_tag.__getitem__):
            indegree[successor] -= 1
            if indegree[successor] == 0:
                ready.append((order_by_tag[successor], successor))
                ready.sort()
    if len(topo) != len(parsed):
        raise ReportDataError("graph contains a dependency cycle")

    deps_by_tag = {tag: deps for tag, _desc, _long, deps in parsed}
    layer: dict[str, int] = {}
    for tag in topo:
        deps = deps_by_tag[tag]
        layer[tag] = 0 if not deps else 1 + max(layer[dep] for dep in deps)

    layer_count = 1 + max(layer.values())
    members: dict[int, list[str]] = {index: [] for index in range(layer_count)}
    for tag in topo:
        members[layer[tag]].append(tag)
    max_members = max(len(tags) for tags in members.values())
    width = max(760, 220 + (layer_count - 1) * 250)
    height = max(320, 130 + max_members * 145)
    coordinates: dict[str, tuple[float, float]] = {}
    for layer_index, tags in members.items():
        x = (
            width / 2.0
            if layer_count == 1
            else 110.0 + layer_index * ((width - 220.0) / (layer_count - 1))
        )
        for member_index, tag in enumerate(tags):
            y = (member_index + 1) * height / (len(tags) + 1)
            coordinates[tag] = (round(x, 3), round(y, 3))

    source = {tag: (desc, long, deps) for tag, desc, long, deps in parsed}
    graph_steps: list[GraphStep] = []
    for topo_order, tag in enumerate(topo):
        desc, long, deps = source[tag]
        x, y = coordinates[tag]
        graph_steps.append(
            GraphStep(tag, desc, long, deps, topo_order, layer[tag], x, y)
        )
    return DagDocument(
        description=cfg.description,
        steps=tuple(graph_steps),
        width=width,
        height=height,
    )


def load_dag(path: Path) -> DagDocument:
    """Load a DAG and assign its stable report layout."""
    return dag_document_from_config(load_dag_config(path))


def _cell(row: Mapping[str | None, str | None], key: str) -> str:
    value = row.get(key)
    return "" if value is None else value.strip()


def _finite_float(value: str, *, minimum: float | None = None) -> float | None:
    if not value:
        return None
    try:
        result = float(value)
    except ValueError:
        return None
    if not math.isfinite(result) or (minimum is not None and result < minimum):
        return None
    return result


def _integer(value: str, *, minimum: int | None = None) -> int | None:
    if not value:
        return None
    try:
        result = int(value)
    except ValueError:
        return None
    if minimum is not None and result < minimum:
        return None
    return result


def _truthy(value: str) -> bool:
    return value.lower() in {"true", "1", "yes"}


def _failed_measurement(row: Mapping[str | None, str | None]) -> bool:
    ok = _cell(row, "ok").lower()
    if ok and ok not in {"true", "1", "yes"}:
        return True
    returncode = _integer(_cell(row, "returncode"))
    if returncode is not None and returncode != 0:
        return True
    if _truthy(_cell(row, "timed_out")) or _truthy(_cell(row, "cpu_timed_out")):
        return True
    oom_kills = _integer(_cell(row, "oom_kills"))
    return oom_kills is not None and oom_kills > 0


def _environment(machine: str, container: str) -> str:
    return f"{machine}\u241f{container}"


def _display_environment(machine: str, container: str) -> str:
    machine_label = machine or "unknown machine"
    container_label = container or "unknown container"
    return f"{machine_label} / {container_label}"


def _relative_source(path: Path, profile_dir: Path) -> str:
    try:
        return path.relative_to(profile_dir).as_posix()
    except ValueError:
        return path.as_posix()


def load_profile_records(
    profile_dir: Path, known_steps: frozenset[str]
) -> tuple[tuple[ProfileRecord, ...], tuple[str, ...]]:
    """Read successful, numeric aggregate measurements from every profile shard."""
    records: list[ProfileRecord] = []
    warnings: list[str] = []
    skipped_invalid = 0
    skipped_failed = 0
    skipped_unknown = 0
    paths = sorted(path for path in profile_dir.glob(PROFILE_GLOB) if path.is_file())
    if not paths:
        warnings.append(f"No {PROFILE_GLOB} files were found in the profile store.")
    for path in paths:
        try:
            handle = path.open(newline="", encoding="utf-8")
        except OSError as exc:
            raise ReportDataError(f"cannot read profile CSV {path}: {exc}") from exc
        with handle:
            reader = csv.DictReader(handle)
            fields = set(reader.fieldnames or [])
            required = {"step", "inner_jobs", "elapsed_s"}
            if not required.issubset(fields):
                missing = ", ".join(sorted(required - fields))
                raise ReportDataError(f"{path}: profile CSV is missing columns: {missing}")
            for row in reader:
                step = _cell(row, "step")
                if step not in known_steps:
                    skipped_unknown += 1
                    continue
                if _failed_measurement(row):
                    skipped_failed += 1
                    continue
                jobs = _integer(_cell(row, "inner_jobs"), minimum=1)
                elapsed_s = _finite_float(_cell(row, "elapsed_s"), minimum=0.0)
                if jobs is None or elapsed_s is None or elapsed_s <= 0.0:
                    skipped_invalid += 1
                    continue
                user_s = _finite_float(_cell(row, "user_s"), minimum=0.0)
                sys_s = _finite_float(_cell(row, "sys_s"), minimum=0.0)
                cpu_s = None
                if user_s is not None or sys_s is not None:
                    cpu_s = (user_s or 0.0) + (sys_s or 0.0)
                effective = _finite_float(_cell(row, "effective_cores"), minimum=0.0)
                if cpu_s is None and effective is not None:
                    cpu_s = effective * elapsed_s
                peak = _integer(_cell(row, "peak_bytes"), minimum=0)
                machine = _cell(row, "machine_id")
                container = _cell(row, "container_class")
                commit = _cell(row, "git_sha") or "(unknown)"
                records.append(
                    ProfileRecord(
                        step=step,
                        commit=commit,
                        timestamp=_cell(row, "timestamp"),
                        jobs=jobs,
                        elapsed_s=elapsed_s,
                        cpu_s=cpu_s,
                        peak_bytes=peak,
                        effective_cores=effective,
                        run_id=_cell(row, "run_id"),
                        machine=machine,
                        container=container,
                        environment=_environment(machine, container),
                        workload=_cell(row, "workload_digest"),
                        runner=_cell(row, "runner_name"),
                        source=_relative_source(path, profile_dir),
                    )
                )
    if skipped_failed:
        warnings.append(f"Excluded {skipped_failed} failed or interrupted aggregate sample(s).")
    if skipped_invalid:
        warnings.append(f"Excluded {skipped_invalid} aggregate row(s) without a positive width and wall time.")
    if skipped_unknown:
        warnings.append(
            f"Excluded {skipped_unknown} aggregate row(s) for steps absent from the supplied DAG."
        )
    records.sort(
        key=lambda record: (
            record.step,
            record.timestamp,
            record.commit,
            record.environment,
            record.workload,
            record.jobs,
            record.run_id,
            record.source,
        )
    )
    return tuple(records), tuple(warnings)


def load_trace_series(
    profile_dir: Path, known_steps: frozenset[str]
) -> tuple[TraceSeries, ...]:
    """Read interval traces and group rows into individual step executions."""
    trace_dir = profile_dir / "traces"
    paths = sorted(path for path in trace_dir.glob(TRACE_GLOB) if path.is_file())
    grouped: dict[
        tuple[str, str, int, str, str, str, str],
        list[tuple[TracePoint, str, str]],
    ] = {}
    for path in paths:
        try:
            handle = path.open(newline="", encoding="utf-8")
        except OSError as exc:
            raise ReportDataError(f"cannot read trace CSV {path}: {exc}") from exc
        with handle:
            reader = csv.DictReader(handle)
            fields = set(reader.fieldnames or [])
            required = {"run_id", "step", "inner_jobs", "sample_index", "elapsed_s"}
            if not required.issubset(fields):
                missing = ", ".join(sorted(required - fields))
                raise ReportDataError(f"{path}: trace CSV is missing columns: {missing}")
            for row in reader:
                step = _cell(row, "step")
                if step not in known_steps:
                    continue
                jobs = _integer(_cell(row, "inner_jobs"), minimum=1)
                sample_index = _integer(_cell(row, "sample_index"), minimum=0)
                elapsed_s = _finite_float(_cell(row, "elapsed_s"), minimum=0.0)
                if jobs is None or sample_index is None or elapsed_s is None:
                    continue
                machine = _cell(row, "machine_id")
                container = _cell(row, "container_class")
                commit = _cell(row, "git_sha") or "(unknown)"
                workload = _cell(row, "workload_digest")
                run_id = _cell(row, "run_id") or path.stem
                point = TracePoint(
                    sample_index=sample_index,
                    sample_kind=_cell(row, "sample_kind") or "periodic",
                    elapsed_s=elapsed_s,
                    interval_s=_finite_float(_cell(row, "interval_s"), minimum=0.0),
                    effective_cores=_finite_float(
                        _cell(row, "effective_cores"), minimum=0.0
                    ),
                    user_cores=_finite_float(_cell(row, "user_cores"), minimum=0.0),
                    system_cores=_finite_float(
                        _cell(row, "system_cores"), minimum=0.0
                    ),
                    thread_count=_integer(_cell(row, "thread_count"), minimum=0),
                    throttled_s=_finite_float(_cell(row, "throttled_s"), minimum=0.0),
                )
                key = (run_id, step, jobs, commit, machine, container, workload)
                grouped.setdefault(key, []).append(
                    (point, _cell(row, "timestamp"), _relative_source(path, profile_dir))
                )

    result: list[TraceSeries] = []
    for group_key, rows in grouped.items():
        run_id, step, jobs, commit, machine, container, workload = group_key
        ordered = sorted(rows, key=lambda item: (item[0].sample_index, item[0].elapsed_s))
        timestamps = [timestamp for _point, timestamp, _source in ordered if timestamp]
        source = ordered[0][2]
        trace_key = "\u241f".join(
            (run_id, step, str(jobs), commit, machine, container, workload)
        )
        result.append(
            TraceSeries(
                key=trace_key,
                step=step,
                commit=commit,
                timestamp=max(timestamps) if timestamps else "",
                jobs=jobs,
                run_id=run_id,
                machine=machine,
                container=container,
                environment=_environment(machine, container),
                workload=workload,
                source=source,
                points=tuple(point for point, _timestamp, _source in ordered),
            )
        )
    result.sort(
        key=lambda series: (
            series.step,
            series.timestamp,
            series.commit,
            series.jobs,
            series.run_id,
            series.source,
        )
    )
    return tuple(result)


_CAPTURE_SCHEMA = "dagrun-profile-capture-v1"
_CAPTURE_STATES = frozenset({"running", "complete", "failed", "skipped"})
_CAPTURE_KINDS = frozenset({"perf", "wprof"})


def _capture_object(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ReportDataError(f"{where} must be an object")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise ReportDataError(f"{where} has a non-string key")
        result[key] = item
    return result


def _capture_string(
    value: object, where: str, *, allow_empty: bool = True
) -> str:
    if not isinstance(value, str) or (not allow_empty and not value):
        qualifier = "non-empty " if not allow_empty else ""
        raise ReportDataError(f"{where} must be a {qualifier}string")
    return value


def _capture_int(value: object, where: str, *, minimum: int | None = None) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ReportDataError(f"{where} must be an integer")
    if minimum is not None and value < minimum:
        raise ReportDataError(f"{where} must be >= {minimum}")
    return value


def _capture_optional_int(value: object, where: str) -> int | None:
    if value is None:
        return None
    return _capture_int(value, where)


def _capture_optional_float(
    value: object, where: str, *, minimum: float = 0.0
) -> float | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ReportDataError(f"{where} must be a number or null")
    result = float(value)
    if not math.isfinite(result) or result < minimum:
        raise ReportDataError(f"{where} must be finite and >= {minimum:g}")
    return result


def _capture_strings(value: object, where: str) -> tuple[str, ...]:
    if not isinstance(value, list):
        raise ReportDataError(f"{where} must be a list of strings")
    result: list[str] = []
    for index, item in enumerate(value):
        result.append(_capture_string(item, f"{where}[{index}]"))
    return tuple(result)


def _capture_state(value: object, where: str) -> str:
    state = _capture_string(value, where, allow_empty=False)
    if state not in _CAPTURE_STATES:
        raise ReportDataError(f"{where} has unknown state {state!r}")
    return state


def _capture_artifact(
    raw: object,
    where: str,
    manifest_path: Path,
    profile_dir: Path,
    report_path: Path | None,
) -> CaptureArtifactView:
    artifact = _capture_object(raw, where)
    raw_path = _capture_string(artifact.get("path"), f"{where}.path", allow_empty=False)
    relative = Path(raw_path)
    if relative.is_absolute() or ".." in relative.parts:
        raise ReportDataError(f"{where}.path must stay within its capture directory")
    artifact_path = manifest_path.parent / relative
    link_base = profile_dir if report_path is None else report_path.parent
    try:
        link_path = os.path.relpath(artifact_path.absolute(), link_base.absolute())
        href = quote(Path(link_path).as_posix(), safe="/@:-._~")
    except ValueError:
        href = artifact_path.absolute().as_uri()
    return CaptureArtifactView(
        role=_capture_string(artifact.get("role"), f"{where}.role", allow_empty=False),
        path=_relative_source(artifact_path, profile_dir),
        href=href,
        size_bytes=_capture_int(
            artifact.get("size_bytes"), f"{where}.size_bytes", minimum=0
        ),
        mode=_capture_string(artifact.get("mode", ""), f"{where}.mode"),
        exists=artifact_path.is_file(),
    )


def _capture_trial(
    raw: object,
    where: str,
    manifest_path: Path,
    profile_dir: Path,
    report_path: Path | None,
) -> CaptureTrialView:
    trial = _capture_object(raw, where)
    kind = _capture_string(trial.get("kind"), f"{where}.kind", allow_empty=False)
    if kind not in _CAPTURE_KINDS:
        raise ReportDataError(f"{where}.kind has unknown profiler {kind!r}")
    artifacts_raw = trial.get("artifacts")
    if not isinstance(artifacts_raw, list):
        raise ReportDataError(f"{where}.artifacts must be a list")
    included = trial.get("included_in_model")
    if not isinstance(included, bool):
        raise ReportDataError(f"{where}.included_in_model must be a boolean")
    return CaptureTrialView(
        trial_id=_capture_string(
            trial.get("trial_id"), f"{where}.trial_id", allow_empty=False
        ),
        kind=kind,
        state=_capture_state(trial.get("state"), f"{where}.state"),
        inner_jobs=_capture_int(
            trial.get("inner_jobs"), f"{where}.inner_jobs", minimum=1
        ),
        started_at=_capture_string(trial.get("started_at", ""), f"{where}.started_at"),
        finished_at=_capture_string(
            trial.get("finished_at", ""), f"{where}.finished_at"
        ),
        measured_wall_s=_capture_optional_float(
            trial.get("measured_wall_s"), f"{where}.measured_wall_s"
        ),
        workload_returncode=_capture_optional_int(
            trial.get("workload_returncode"), f"{where}.workload_returncode"
        ),
        profiler_returncode=_capture_optional_int(
            trial.get("profiler_returncode"), f"{where}.profiler_returncode"
        ),
        error=_capture_string(trial.get("error", ""), f"{where}.error"),
        included_in_model=included,
        artifacts=tuple(
            _capture_artifact(
                item,
                f"{where}.artifacts[{index}]",
                manifest_path,
                profile_dir,
                report_path,
            )
            for index, item in enumerate(artifacts_raw)
        ),
    )


def _parse_capture_manifest(
    raw: object, manifest_path: Path, profile_dir: Path, report_path: Path | None
) -> CaptureView:
    where = _relative_source(manifest_path, profile_dir)
    manifest = _capture_object(raw, where)
    schema = _capture_string(manifest.get("schema"), f"{where}.schema", allow_empty=False)
    if schema != _CAPTURE_SCHEMA:
        raise ReportDataError(
            f"{where}.schema must be {_CAPTURE_SCHEMA!r}, got {schema!r}"
        )
    selection = _capture_object(manifest.get("selection"), f"{where}.selection")
    trials_raw = manifest.get("trials")
    if not isinstance(trials_raw, list):
        raise ReportDataError(f"{where}.trials must be a list")
    errors = _capture_strings(manifest.get("errors", []), f"{where}.errors")
    machine = _capture_string(manifest.get("machine_id", ""), f"{where}.machine_id")
    container = _capture_string(
        manifest.get("container_class", ""), f"{where}.container_class"
    )
    return CaptureView(
        capture_id=_capture_string(
            manifest.get("capture_id"), f"{where}.capture_id", allow_empty=False
        ),
        state=_capture_state(manifest.get("state"), f"{where}.state"),
        created_at=_capture_string(
            manifest.get("created_at", ""), f"{where}.created_at"
        ),
        finished_at=_capture_string(
            manifest.get("finished_at", ""), f"{where}.finished_at"
        ),
        step=_capture_string(
            selection.get("step"), f"{where}.selection.step", allow_empty=False
        ),
        commit=_capture_string(
            selection.get("git_sha", ""), f"{where}.selection.git_sha"
        )
        or "(unknown)",
        machine=machine,
        container=container,
        environment=_environment(machine, container) if machine or container else "",
        workload=_capture_string(
            selection.get("workload_digest", ""),
            f"{where}.selection.workload_digest",
        ),
        jobs=_capture_int(
            selection.get("inner_jobs"), f"{where}.selection.inner_jobs", minimum=1
        ),
        expected_wall_s=_capture_optional_float(
            selection.get("expected_wall_s"), f"{where}.selection.expected_wall_s"
        ),
        speedup=_capture_optional_float(
            selection.get("speedup"), f"{where}.selection.speedup"
        ),
        manifest_path=where,
        errors=errors,
        trials=tuple(
            sorted(
                (
                    _capture_trial(
                        item,
                        f"{where}.trials[{index}]",
                        manifest_path,
                        profile_dir,
                        report_path,
                    )
                    for index, item in enumerate(trials_raw)
                ),
                key=lambda trial: (trial.started_at, trial.trial_id, trial.kind),
            )
        ),
    )


def _reject_capture_constant(token: str) -> object:
    raise ReportDataError(f"non-finite JSON value {token!r} is not allowed")


def load_capture_manifests(
    profile_dir: Path,
    known_steps: frozenset[str],
    *,
    report_path: Path | None = None,
) -> tuple[tuple[CaptureView, ...], tuple[str, ...]]:
    """Load profiler manifests without allowing one bad capture to suppress the report."""
    captures: list[CaptureView] = []
    warnings: list[str] = []
    capture_dir = profile_dir / "captures"
    if not capture_dir.is_dir():
        return (), ()
    for manifest_path in sorted(capture_dir.glob("*/manifest.json")):
        relative = _relative_source(manifest_path, profile_dir)
        try:
            raw: object = json.loads(
                manifest_path.read_text(encoding="utf-8"),
                parse_constant=_reject_capture_constant,
            )
            capture = _parse_capture_manifest(
                raw, manifest_path, profile_dir, report_path
            )
        except (OSError, UnicodeError, json.JSONDecodeError, ReportDataError):
            # Keep report payloads interoperable across implementations. Parser and I/O
            # exception text is runtime-specific and must not leak into the serialized data.
            warnings.append(f"Ignored malformed capture manifest {relative}.")
            continue
        if capture.step not in known_steps:
            warnings.append(
                f"Ignored capture manifest {relative} for step {capture.step!r}, "
                "which is absent from the supplied DAG."
            )
            continue
        captures.append(capture)
    captures.sort(
        key=lambda capture: (
            capture.step,
            capture.created_at,
            capture.capture_id,
            capture.manifest_path,
        )
    )
    return tuple(captures), tuple(warnings)


def build_report_data_for_config(
    profile_dir: Path,
    cfg: DagConfig,
    *,
    dag_label: str,
    report_path: Path | None = None,
) -> dict[str, object]:
    """Build the report payload from a DAG already validated by the caller."""
    dag = dag_document_from_config(cfg)
    known_steps = frozenset(step.tag for step in dag.steps)
    records, profile_warnings = load_profile_records(profile_dir, known_steps)
    traces = load_trace_series(profile_dir, known_steps)
    captures, capture_warnings = load_capture_manifests(
        profile_dir, known_steps, report_path=report_path
    )
    warnings = (*profile_warnings, *capture_warnings)

    commit_latest: dict[str, str] = {}
    environment_labels: dict[str, str] = {}
    workloads: set[str] = set()
    for record in records:
        commit_latest[record.commit] = max(
            record.timestamp, commit_latest.get(record.commit, "")
        )
        environment_labels[record.environment] = _display_environment(
            record.machine, record.container
        )
        if record.workload:
            workloads.add(record.workload)
    for trace in traces:
        commit_latest[trace.commit] = max(trace.timestamp, commit_latest.get(trace.commit, ""))
        environment_labels[trace.environment] = _display_environment(
            trace.machine, trace.container
        )
        if trace.workload:
            workloads.add(trace.workload)
    for capture in captures:
        commit_latest[capture.commit] = max(
            capture.created_at, commit_latest.get(capture.commit, "")
        )
        if capture.environment:
            environment_labels[capture.environment] = _display_environment(
                capture.machine, capture.container
            )
        if capture.workload:
            workloads.add(capture.workload)

    commits = [
        {"sha": sha, "timestamp": timestamp}
        for sha, timestamp in sorted(commit_latest.items(), key=lambda item: (item[1], item[0]))
    ]
    environments = [
        {"key": key, "label": environment_labels[key]}
        for key in sorted(environment_labels, key=lambda item: environment_labels[item])
    ]
    graph_steps = [step.to_json() for step in dag.steps]
    edges = [
        {"from": dep, "to": step.tag}
        for step in dag.steps
        for dep in step.deps
    ]
    return {
        "schema": 1,
        "profile_dir": profile_dir.as_posix(),
        "dag_path": dag_label,
        "graph": {
            "description": dag.description,
            "width": dag.width,
            "height": dag.height,
            "steps": graph_steps,
            "edges": edges,
        },
        "commits": commits,
        "environments": environments,
        "workloads": sorted(workloads),
        "records": [record.to_json() for record in records],
        "traces": [trace.to_json() for trace in traces],
        "captures": [capture.to_json() for capture in captures],
        "warnings": list(warnings),
    }


def build_report_data(
    profile_dir: Path, dag_path: Path, *, report_path: Path | None = None
) -> dict[str, object]:
    """Load all report inputs into the stable JSON payload embedded in the page."""
    return build_report_data_for_config(
        profile_dir,
        load_dag_config(dag_path),
        dag_label=dag_path.as_posix(),
        report_path=report_path,
    )


_STYLE = r"""
:root {
  color-scheme: dark;
  --bg: #09111d;
  --panel: #101c2d;
  --panel-2: #14243a;
  --ink: #edf5ff;
  --muted: #9fb1c7;
  --line: #2a405a;
  --cyan: #59d8e6;
  --blue: #6ba7ff;
  --amber: #ffc96b;
  --rose: #ff7e9b;
  --green: #73dfa4;
  --shadow: 0 18px 45px rgba(0, 0, 0, .28);
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background:
    radial-gradient(circle at 12% -10%, rgba(57, 118, 180, .28), transparent 34rem),
    radial-gradient(circle at 90% 5%, rgba(59, 189, 178, .13), transparent 30rem),
    var(--bg);
  color: var(--ink);
  font: 14px/1.45 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
header { padding: 34px clamp(20px, 4vw, 64px) 20px; }
h1 { margin: 0 0 8px; font-size: clamp(28px, 4vw, 48px); letter-spacing: -.035em; }
h2, h3 { margin: 0; }
.lede { color: var(--muted); max-width: 92ch; }
.source { margin-top: 10px; color: #7890aa; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
main { padding: 0 clamp(20px, 4vw, 64px) 64px; }
.panel {
  background: linear-gradient(155deg, rgba(20, 36, 58, .96), rgba(13, 25, 41, .96));
  border: 1px solid rgba(115, 155, 197, .2);
  border-radius: 16px;
  box-shadow: var(--shadow);
  margin: 18px 0;
  overflow: hidden;
}
.panel-head { padding: 20px 22px 12px; }
.panel-head p { color: var(--muted); margin: 6px 0 0; }
.controls {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: 12px;
  padding: 18px 22px 22px;
  border-top: 1px solid rgba(115, 155, 197, .15);
}
label { color: var(--muted); font-size: 12px; font-weight: 650; letter-spacing: .04em; text-transform: uppercase; }
select {
  display: block; width: 100%; margin-top: 6px; padding: 9px 11px;
  color: var(--ink); background: #0b1727; border: 1px solid #34506d; border-radius: 8px;
}
.warnings { margin: 16px 0; padding: 0; list-style: none; }
.warnings li { margin: 7px 0; padding: 10px 13px; border-left: 3px solid var(--amber); background: rgba(255, 201, 107, .08); color: #ffe6b7; }
#dag-wrap { overflow-x: auto; padding: 4px 12px 18px; }
#dag-svg { display: block; width: 100%; min-width: 720px; max-height: 640px; }
.edge { fill: none; stroke: #3a5877; stroke-width: 2; opacity: .8; }
.dag-node { cursor: pointer; outline: none; }
.dag-node circle { fill: #173653; stroke: #5a87b1; stroke-width: 2; transition: r .2s, fill .2s, stroke .2s; }
.dag-node:hover circle, .dag-node:focus circle { fill: #205171; stroke: var(--cyan); }
.dag-node.selected circle { fill: #14546a; stroke: var(--cyan); stroke-width: 4; }
.node-tag { fill: white; font-size: 13px; font-weight: 750; text-anchor: middle; pointer-events: none; }
.node-cpu { fill: #b8cadc; font-size: 10px; text-anchor: middle; pointer-events: none; }
.legend-note { padding: 0 22px 18px; color: var(--muted); font-size: 12px; }
.step-head { display: flex; flex-wrap: wrap; gap: 14px; justify-content: space-between; align-items: start; padding: 22px; }
.step-head p { margin: 4px 0 0; color: var(--muted); max-width: 85ch; }
.step-picker { min-width: min(330px, 100%); }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(145px, 1fr)); gap: 1px; background: var(--line); border-top: 1px solid var(--line); border-bottom: 1px solid var(--line); }
.card { background: var(--panel); padding: 15px 18px; min-height: 82px; }
.card span { display: block; color: var(--muted); font-size: 11px; font-weight: 700; text-transform: uppercase; letter-spacing: .06em; }
.card strong { display: block; margin-top: 7px; font-size: 22px; }
.capture-section { padding: 18px 18px 2px; }
.capture-section > p { color: var(--muted); margin: 4px 0 12px; }
.capture-list { display: grid; gap: 10px; }
.capture {
  border: 1px solid rgba(115, 155, 197, .2); border-radius: 10px;
  background: rgba(5, 14, 25, .5); padding: 13px 15px;
}
.capture-head { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
.capture-head strong { margin-right: auto; }
.status { border: 1px solid currentColor; border-radius: 999px; padding: 2px 8px; font-size: 10px; font-weight: 800; text-transform: uppercase; }
.status.complete { color: var(--green); }
.status.failed { color: var(--rose); }
.status.running { color: var(--amber); }
.status.skipped { color: var(--muted); }
.capture-meta, .capture-path { color: var(--muted); font-size: 12px; margin-top: 5px; }
.capture-path { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }
.capture-errors { color: #ffc2cf; margin: 8px 0 0; padding-left: 20px; }
.capture-trials { display: grid; gap: 7px; margin-top: 10px; }
.capture-trial { border-left: 2px solid #3d5875; padding: 5px 0 5px 11px; }
.capture-artifacts { display: flex; flex-wrap: wrap; gap: 6px 14px; margin-top: 4px; }
.capture-artifacts code, .capture-artifacts a { color: #b9d7ed; font: 11px ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }
.capture-artifacts a:hover, .capture-artifacts a:focus { color: var(--cyan); }
.charts { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 420px), 1fr)); gap: 14px; padding: 18px; }
.chart { background: rgba(5, 14, 25, .58); border: 1px solid rgba(115, 155, 197, .17); border-radius: 12px; padding: 14px; min-width: 0; }
.chart h3 { font-size: 15px; }
.chart p { margin: 3px 0 8px; color: var(--muted); font-size: 12px; min-height: 35px; }
.chart svg { display: block; width: 100%; height: auto; min-height: 270px; }
.axis { stroke: #59718b; stroke-width: 1; }
.grid { stroke: #263b52; stroke-width: 1; }
.tick { fill: #90a6be; font-size: 10px; }
.axis-label { fill: #b9c9da; font-size: 11px; font-weight: 650; }
.point { stroke: rgba(8, 18, 31, .8); stroke-width: 1.5; opacity: .72; }
.fit { fill: none; stroke: var(--cyan); stroke-width: 3; }
.reference { fill: none; stroke: #6c8096; stroke-width: 1.5; stroke-dasharray: 7 5; }
.trace-line { fill: none; stroke: var(--cyan); stroke-width: 2.5; }
.trace-thread { fill: none; stroke: var(--amber); stroke-width: 2; opacity: .82; }
.trace-request { fill: none; stroke: var(--rose); stroke-width: 1.5; stroke-dasharray: 7 5; }
.empty { fill: #9fb1c7; text-anchor: middle; font-size: 13px; }
.trace-controls { padding: 0 18px 18px; display: grid; grid-template-columns: minmax(250px, 1fr) auto; gap: 14px; align-items: end; }
.trace-meta { color: var(--muted); font-size: 12px; }
.footer { color: #71879f; text-align: center; padding-top: 18px; font-size: 12px; }
@media (max-width: 650px) {
  .trace-controls { grid-template-columns: 1fr; }
  .charts { padding: 10px; }
  .panel-head, .step-head { padding-left: 16px; padding-right: 16px; }
}
"""


_SCRIPT = r"""
"use strict";
const DATA = JSON.parse(document.getElementById("dagrun-report-data").textContent);
const NS = "http://www.w3.org/2000/svg";
const state = { step: DATA.graph.steps[0].tag, trace: null, commits: new Set(), environment: "all", workload: "all", recordsByStep: new Map() };
const byId = id => document.getElementById(id);

function svgEl(name, attrs = {}, text = null) {
  const node = document.createElementNS(NS, name);
  Object.entries(attrs).forEach(([key, value]) => node.setAttribute(key, String(value)));
  if (text !== null) node.textContent = String(text);
  return node;
}
function htmlEl(name, className = "", text = null) {
  const node = document.createElement(name);
  if (className) node.className = className;
  if (text !== null) node.textContent = String(text);
  return node;
}
function median(values) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}
function finite(value) { return typeof value === "number" && Number.isFinite(value); }
function minimum(values, fallback = null) { return values.length ? values.reduce((best, value) => value < best ? value : best) : fallback; }
function maximum(values, fallback = null) { return values.length ? values.reduce((best, value) => value > best ? value : best) : fallback; }
function shortSha(sha) { return sha === "(unknown)" ? sha : sha.slice(0, 10); }
function fmt(value, digits = 2) { return finite(value) ? value.toFixed(digits).replace(/\.0+$/, "").replace(/(\.\d*?)0+$/, "$1") : "—"; }
function bytes(value) {
  if (!finite(value)) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let amount = value, index = 0;
  while (amount >= 1024 && index < units.length - 1) { amount /= 1024; index += 1; }
  return `${fmt(amount, amount >= 100 ? 0 : 1)} ${units[index]}`;
}
function selectedCommits() {
  const all = DATA.commits.map(item => item.sha);
  const raw = byId("commit-limit").value;
  return raw === "all" ? new Set(all) : new Set(all.slice(-Number(raw)));
}
function refreshFilterState() {
  state.commits = selectedCommits();
  state.environment = byId("environment-filter").value;
  state.workload = byId("workload-filter").value;
  state.recordsByStep = new Map();
  DATA.records.forEach(row => {
    if (!baseFilter(row)) return;
    if (!state.recordsByStep.has(row.step)) state.recordsByStep.set(row.step, []);
    state.recordsByStep.get(row.step).push(row);
  });
}
function baseFilter(item) {
  if (!state.commits.has(item.commit)) return false;
  if (state.environment !== "all" && item.environment !== state.environment) return false;
  if (state.workload === "legacy" && item.workload !== "") return false;
  if (state.workload !== "all" && state.workload !== "legacy" && item.workload !== state.workload) return false;
  return true;
}
function visibleRecords(step = state.step) { return state.recordsByStep.get(step) || []; }
function cohortKey(row) { return [row.step, row.commit, row.environment, row.workload].join("\u241f"); }
function normalizedRecords(step = state.step) {
  const rows = visibleRecords(step);
  const cohorts = new Map();
  rows.forEach(row => {
    const key = cohortKey(row);
    if (!cohorts.has(key)) cohorts.set(key, []);
    cohorts.get(key).push(row);
  });
  const result = [];
  cohorts.forEach(cohort => {
    const baselineJobs = minimum(cohort.map(row => row.jobs));
    const baselineRows = cohort.filter(row => row.jobs === baselineJobs);
    const baselineWall = median(baselineRows.map(row => row.elapsed_s).filter(finite));
    const baselineCpu = median(baselineRows.map(row => row.cpu_s).filter(finite));
    cohort.forEach(row => result.push({
      ...row,
      baseline_jobs: baselineJobs,
      speedup: finite(baselineWall) ? baselineWall / row.elapsed_s : null,
      cpu_efficiency: finite(baselineCpu) && finite(row.cpu_s) && row.cpu_s > 0 ? 100 * baselineCpu / row.cpu_s : null
    }));
  });
  return result;
}
function groupedMedians(points, field) {
  const groups = new Map();
  points.forEach(point => {
    if (!finite(point[field])) return;
    if (!groups.has(point.jobs)) groups.set(point.jobs, []);
    groups.get(point.jobs).push(point[field]);
  });
  return [...groups.entries()].map(([jobs, values]) => ({jobs, value: median(values)})).sort((a, b) => a.jobs - b.jobs);
}
function commitColor(commit) {
  const commits = DATA.commits.map(item => item.sha);
  const index = Math.max(0, commits.indexOf(commit));
  const t = commits.length <= 1 ? 1 : index / (commits.length - 1);
  const hue = 214 - 165 * t;
  return `hsl(${hue} 80% 64%)`;
}
function activeCommitLabel() {
  const commits = DATA.commits.filter(item => selectedCommits().has(item.sha));
  if (!commits.length) return "no commits";
  if (commits.length === DATA.commits.length) return `all ${commits.length} commits`;
  return `latest ${commits.length} commit${commits.length === 1 ? "" : "s"}`;
}

function buildControls() {
  const commitSelect = byId("commit-limit");
  const choices = [1, 3, 5, 10, 25].filter(number => number < DATA.commits.length);
  const allOption = document.createElement("option");
  allOption.value = "all"; allOption.textContent = `All history (${DATA.commits.length})`;
  commitSelect.append(allOption);
  choices.forEach(number => {
    const option = document.createElement("option");
    option.value = String(number); option.textContent = `Last ${number} commit${number === 1 ? "" : "s"}`;
    commitSelect.append(option);
  });
  const environment = byId("environment-filter");
  environment.append(new Option(`All environments (${DATA.environments.length})`, "all"));
  DATA.environments.forEach(item => environment.append(new Option(item.label, item.key)));
  const workload = byId("workload-filter");
  workload.append(new Option("All workload revisions", "all"));
  if (DATA.records.some(row => row.workload === "") || DATA.captures.some(capture => capture.workload === "")) workload.append(new Option("Legacy data (no digest)", "legacy"));
  DATA.workloads.forEach(digest => workload.append(new Option(digest, digest)));
  const stepSelect = byId("step-select");
  DATA.graph.steps.forEach(step => stepSelect.append(new Option(step.tag, step.tag)));
  stepSelect.value = state.step;
  [commitSelect, environment, workload].forEach(control => control.addEventListener("change", updateAll));
  stepSelect.addEventListener("change", () => selectStep(stepSelect.value));
}

function renderDag() {
  const svg = byId("dag-svg");
  svg.replaceChildren();
  svg.setAttribute("viewBox", `0 0 ${DATA.graph.width} ${DATA.graph.height}`);
  const defs = svgEl("defs");
  const marker = svgEl("marker", {id: "arrow", viewBox: "0 0 10 10", refX: 8, refY: 5, markerWidth: 7, markerHeight: 7, orient: "auto-start-reverse"});
  marker.append(svgEl("path", {d: "M 0 0 L 10 5 L 0 10 z", fill: "#3a5877"}));
  defs.append(marker); svg.append(defs);
  const positions = new Map(DATA.graph.steps.map(step => [step.tag, step]));
  DATA.graph.edges.forEach(edge => {
    const from = positions.get(edge.from), to = positions.get(edge.to);
    if (!from || !to) return;
    const middle = (from.x + to.x) / 2;
    svg.append(svgEl("path", {class: "edge", d: `M ${from.x} ${from.y} C ${middle} ${from.y}, ${middle} ${to.y}, ${to.x} ${to.y}`, "marker-end": "url(#arrow)"}));
  });
  DATA.graph.steps.forEach(step => {
    const group = svgEl("g", {class: "dag-node", transform: `translate(${step.x} ${step.y})`, tabindex: 0, role: "button", "aria-label": `Open ${step.tag}`, "data-step": step.tag});
    const circle = svgEl("circle", {r: 28});
    const title = svgEl("title", {}, step.tag); circle.append(title);
    const label = step.tag.length > 22 ? `${step.tag.slice(0, 20)}…` : step.tag;
    group.append(circle, svgEl("text", {class: "node-tag", y: -2}, label), svgEl("text", {class: "node-cpu", y: 16}, "no samples"));
    group.addEventListener("click", () => selectStep(step.tag));
    group.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); selectStep(step.tag); } });
    svg.append(group);
  });
}

function updateDagWeights() {
  const medians = new Map();
  DATA.graph.steps.forEach(step => {
    const cpu = visibleRecords(step.tag).map(row => row.cpu_s).filter(finite);
    medians.set(step.tag, median(cpu));
  });
  const maxCpu = maximum([...medians.values()].filter(finite), 0);
  document.querySelectorAll(".dag-node").forEach(group => {
    const tag = group.dataset.step;
    const cpu = medians.get(tag);
    const area = finite(cpu) && maxCpu > 0 ? Math.max(1450, 9000 * cpu / maxCpu) : 1200;
    group.querySelector("circle").setAttribute("r", Math.sqrt(area / Math.PI).toFixed(2));
    group.querySelector(".node-cpu").textContent = finite(cpu) ? `${fmt(cpu, 1)} CPU-s` : "no samples";
    group.classList.toggle("selected", tag === state.step);
  });
}

function renderScatter(svg, points, field, options) {
  svg.replaceChildren();
  const width = 640, height = 330, margin = {left: 58, right: 18, top: 18, bottom: 48};
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  const valid = points.filter(point => finite(point[field]));
  if (!valid.length) {
    svg.append(svgEl("text", {class: "empty", x: width / 2, y: height / 2}, "No measurements match these filters"));
    return;
  }
  const jobs = [...new Set(valid.map(point => point.jobs))].sort((a, b) => a - b);
  const logMin = Math.log2(minimum(jobs, 1)), logMax = Math.log2(maximum(jobs, 1));
  const x = value => margin.left + (logMax === logMin ? .5 : (Math.log2(value) - logMin) / (logMax - logMin)) * (width - margin.left - margin.right);
  const values = valid.map(point => point[field]);
  let yMin = options.zero ? 0 : minimum(values, 0);
  let yMax = maximum(values, options.reference || 0);
  if (options.zero) yMax *= 1.12;
  else { const pad = Math.max((yMax - yMin) * .12, yMax * .04, .01); yMin = Math.max(0, yMin - pad); yMax += pad; }
  if (yMax <= yMin) yMax = yMin + 1;
  const y = value => height - margin.bottom - (value - yMin) / (yMax - yMin) * (height - margin.top - margin.bottom);
  for (let tick = 0; tick <= 4; tick += 1) {
    const value = yMin + tick * (yMax - yMin) / 4;
    const py = y(value);
    svg.append(svgEl("line", {class: "grid", x1: margin.left, x2: width - margin.right, y1: py, y2: py}));
    svg.append(svgEl("text", {class: "tick", x: margin.left - 8, y: py + 4, "text-anchor": "end"}, options.format(value)));
  }
  jobs.forEach(job => {
    const px = x(job);
    svg.append(svgEl("line", {class: "grid", x1: px, x2: px, y1: margin.top, y2: height - margin.bottom}));
    svg.append(svgEl("text", {class: "tick", x: px, y: height - margin.bottom + 19, "text-anchor": "middle"}, job));
  });
  svg.append(svgEl("line", {class: "axis", x1: margin.left, x2: width - margin.right, y1: height - margin.bottom, y2: height - margin.bottom}));
  svg.append(svgEl("line", {class: "axis", x1: margin.left, x2: margin.left, y1: margin.top, y2: height - margin.bottom}));
  svg.append(svgEl("text", {class: "axis-label", x: (margin.left + width - margin.right) / 2, y: height - 10, "text-anchor": "middle"}, "Requested inner jobs (log₂ scale)"));
  svg.append(svgEl("text", {class: "axis-label", transform: `translate(14 ${(margin.top + height - margin.bottom) / 2}) rotate(-90)`, "text-anchor": "middle"}, options.axis));
  if (finite(options.reference) && options.reference >= yMin && options.reference <= yMax) {
    svg.append(svgEl("line", {class: "reference", x1: margin.left, x2: width - margin.right, y1: y(options.reference), y2: y(options.reference)}));
  }
  if (options.ideal) {
    const ideal = jobs.map(job => ({jobs: job, value: job / minimum(jobs, 1)})).filter(point => point.value >= yMin && point.value <= yMax);
    if (ideal.length > 1) svg.append(svgEl("path", {class: "reference", d: ideal.map((point, index) => `${index ? "L" : "M"} ${x(point.jobs)} ${y(point.value)}`).join(" ")}));
  }
  const fit = groupedMedians(valid, field);
  if (fit.length > 1) svg.append(svgEl("path", {class: "fit", d: fit.map((point, index) => `${index ? "L" : "M"} ${x(point.jobs)} ${y(point.value)}`).join(" ")}));
  valid.forEach(point => {
    const circle = svgEl("circle", {class: "point", cx: x(point.jobs), cy: y(point[field]), r: 4.5, fill: commitColor(point.commit)});
    circle.append(svgEl("title", {}, [
      `${shortSha(point.commit)} · ${point.timestamp || "unknown time"}`,
      `${point.jobs} jobs · ${options.tooltip(point[field])}`,
      `wall ${fmt(point.elapsed_s, 3)} s · CPU ${fmt(point.cpu_s, 3)} s · RSS ${bytes(point.peak_bytes)}`,
      point.run_id ? `run ${point.run_id}` : point.source
    ].join("\n")));
    svg.append(circle);
  });
}

function sweetSpot(points) {
  const speed = groupedMedians(points, "speedup");
  if (!speed.length) return null;
  const efficiency = new Map(groupedMedians(points, "cpu_efficiency").map(item => [item.jobs, item.value]));
  const best = maximum(speed.map(item => item.value), 0);
  const eligible = speed.filter(item => item.value >= best / 1.1 && (!efficiency.has(item.jobs) || efficiency.get(item.jobs) >= 100 / 1.5));
  return (eligible.length ? eligible : speed.filter(item => item.value >= best / 1.1))[0] || speed[speed.length - 1];
}

function updateCards(points) {
  const commits = new Set(points.map(point => point.commit));
  const best = groupedMedians(points, "speedup").reduce((value, item) => Math.max(value, item.value), 0);
  const spot = sweetSpot(points);
  const spotRows = spot ? points.filter(point => point.jobs === spot.jobs) : [];
  const memory = median(spotRows.map(point => point.peak_bytes).filter(finite));
  byId("card-samples").textContent = String(points.length);
  byId("card-commits").textContent = String(commits.size);
  byId("card-best").textContent = best ? `${fmt(best, 2)}×` : "—";
  byId("card-sweet").textContent = spot ? `${spot.jobs} jobs` : "—";
  byId("card-memory").textContent = bytes(memory);
  byId("history-label").textContent = activeCommitLabel();
  return spot ? spot.jobs : null;
}

function visibleCaptures() {
  return DATA.captures.filter(capture => capture.step === state.step && baseFilter(capture));
}
function renderCaptures() {
  const container = byId("capture-list");
  container.replaceChildren();
  const captures = visibleCaptures().sort((a, b) => b.created_at.localeCompare(a.created_at) || b.capture_id.localeCompare(a.capture_id));
  if (!captures.length) {
    container.append(htmlEl("div", "capture-meta", "No perf or wprof captures match this step and history window."));
    return;
  }
  captures.forEach(capture => {
    const card = htmlEl("article", "capture");
    const head = htmlEl("div", "capture-head");
    head.append(
      htmlEl("strong", "", `${capture.capture_id} · ${capture.jobs} jobs`),
      htmlEl("span", `status ${capture.state}`, capture.state)
    );
    card.append(head);
    card.append(htmlEl("div", "capture-meta", `${shortSha(capture.commit)} · ${capture.created_at || "unknown time"} · selected speedup ${fmt(capture.speedup, 2)}×`));
    card.append(htmlEl("div", "capture-path", `manifest: ${capture.manifest_path}`));
    if (capture.errors.length) {
      const errors = htmlEl("ul", "capture-errors");
      capture.errors.forEach(error => errors.append(htmlEl("li", "", error)));
      card.append(errors);
    }
    const trials = htmlEl("div", "capture-trials");
    capture.trials.forEach(trial => {
      const item = htmlEl("div", "capture-trial");
      const label = `${trial.kind} · ${trial.state} · ${trial.inner_jobs} jobs · ${fmt(trial.measured_wall_s, 3)}s`;
      item.append(htmlEl("strong", "", label));
      if (trial.error) item.append(htmlEl("div", "capture-errors", trial.error));
      const artifacts = htmlEl("div", "capture-artifacts");
      if (!trial.artifacts.length) artifacts.append(htmlEl("span", "capture-meta", "No retained artifacts"));
      trial.artifacts.forEach(artifact => {
        const suffix = artifact.exists ? "" : " (missing)";
        const label = `${artifact.role}: ${artifact.path} · ${bytes(artifact.size_bytes)}${suffix}`;
        if (artifact.exists) {
          const link = htmlEl("a", "", label);
          link.setAttribute("href", artifact.href);
          artifacts.append(link);
        } else {
          artifacts.append(htmlEl("code", "", label));
        }
      });
      item.append(artifacts);
      trials.append(item);
    });
    card.append(trials);
    container.append(card);
  });
}

function updateCharts() {
  const points = normalizedRecords();
  const sweet = updateCards(points);
  renderScatter(byId("speedup-chart"), points, "speedup", {zero: true, ideal: true, axis: "Speedup (×)", format: value => `${fmt(value, 1)}×`, tooltip: value => `${fmt(value, 3)}× speedup`});
  renderScatter(byId("memory-chart"), points, "peak_bytes", {zero: true, axis: "Peak RSS", format: bytes, tooltip: bytes});
  renderScatter(byId("efficiency-chart"), points, "cpu_efficiency", {zero: false, reference: 100, axis: "CPU-work efficiency", format: value => `${fmt(value, 0)}%`, tooltip: value => `${fmt(value, 1)}% CPU-work efficiency`});
  renderCaptures();
  updateTraceSelector(sweet);
}

function selectStep(tag) {
  state.step = tag;
  byId("step-select").value = tag;
  const step = DATA.graph.steps.find(item => item.tag === tag);
  byId("step-title").textContent = tag;
  byId("step-description").textContent = step.description || step.desc || "No authored description.";
  updateDagWeights();
  updateCharts();
}

function traceLabel(trace) {
  return `${shortSha(trace.commit)} · ${trace.jobs} jobs · ${trace.timestamp || trace.run_id}`;
}
function updateTraceSelector(sweet) {
  const select = byId("trace-select");
  const traces = DATA.traces.filter(trace => trace.step === state.step && baseFilter(trace)).sort((a, b) => b.timestamp.localeCompare(a.timestamp) || b.run_id.localeCompare(a.run_id));
  select.replaceChildren();
  if (!traces.length) {
    select.disabled = true; select.append(new Option("No matching traces", "")); state.trace = null; renderTimeline(null); return;
  }
  select.disabled = false;
  traces.forEach(trace => select.append(new Option(traceLabel(trace), trace.key)));
  const prior = traces.find(trace => trace.key === state.trace);
  const preferred = prior || traces.find(trace => sweet !== null && trace.jobs === sweet) || traces[0];
  state.trace = preferred.key; select.value = preferred.key; renderTimeline(preferred);
}

function renderTimeline(trace) {
  const svg = byId("timeline-chart");
  svg.replaceChildren();
  const width = 1120, height = 350, margin = {left: 58, right: 58, top: 18, bottom: 46};
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  if (!trace || !trace.points.length) {
    svg.append(svgEl("text", {class: "empty", x: width / 2, y: height / 2}, "No interval trace matches these filters"));
    byId("trace-meta").textContent = "Enable --profile-timeseries during a run or sweep to collect interval data.";
    return;
  }
  const corePoints = trace.points.filter(point => finite(point.effective_cores));
  const threadPoints = trace.points.filter(point => finite(point.thread_count));
  const maxTime = maximum(trace.points.map(point => point.elapsed_s), 1e-9);
  const maxCore = Math.max(1, trace.jobs, maximum(corePoints.map(point => point.effective_cores), 0)) * 1.12;
  const maxThread = Math.max(1, maximum(threadPoints.map(point => point.thread_count), 0)) * 1.12;
  const x = value => margin.left + value / maxTime * (width - margin.left - margin.right);
  const yCore = value => height - margin.bottom - value / maxCore * (height - margin.top - margin.bottom);
  const yThread = value => height - margin.bottom - value / maxThread * (height - margin.top - margin.bottom);
  for (let tick = 0; tick <= 4; tick += 1) {
    const py = margin.top + tick * (height - margin.top - margin.bottom) / 4;
    const coreValue = maxCore * (1 - tick / 4), threadValue = maxThread * (1 - tick / 4);
    svg.append(svgEl("line", {class: "grid", x1: margin.left, x2: width - margin.right, y1: py, y2: py}));
    svg.append(svgEl("text", {class: "tick", x: margin.left - 8, y: py + 4, "text-anchor": "end"}, fmt(coreValue, 1)));
    svg.append(svgEl("text", {class: "tick", x: width - margin.right + 8, y: py + 4}, fmt(threadValue, 0)));
  }
  for (let tick = 0; tick <= 5; tick += 1) {
    const value = maxTime * tick / 5, px = x(value);
    svg.append(svgEl("line", {class: "grid", x1: px, x2: px, y1: margin.top, y2: height - margin.bottom}));
    svg.append(svgEl("text", {class: "tick", x: px, y: height - margin.bottom + 18, "text-anchor": "middle"}, `${fmt(value, 1)}s`));
  }
  svg.append(svgEl("line", {class: "axis", x1: margin.left, x2: width - margin.right, y1: height - margin.bottom, y2: height - margin.bottom}));
  svg.append(svgEl("text", {class: "axis-label", transform: `translate(14 ${(margin.top + height - margin.bottom) / 2}) rotate(-90)`, "text-anchor": "middle"}, "Effective cores"));
  svg.append(svgEl("text", {class: "axis-label", transform: `translate(${width - 8} ${(margin.top + height - margin.bottom) / 2}) rotate(90)`, "text-anchor": "middle"}, "Threads"));
  const path = (points, getter, scale) => points.map((point, index) => `${index ? "L" : "M"} ${x(point.elapsed_s)} ${scale(getter(point))}`).join(" ");
  if (corePoints.length) svg.append(svgEl("path", {class: "trace-line", d: path(corePoints, point => point.effective_cores, yCore)}));
  if (threadPoints.length) svg.append(svgEl("path", {class: "trace-thread", d: path(threadPoints, point => point.thread_count, yThread)}));
  svg.append(svgEl("line", {class: "trace-request", x1: margin.left, x2: width - margin.right, y1: yCore(trace.jobs), y2: yCore(trace.jobs)}));
  corePoints.forEach(point => {
    const circle = svgEl("circle", {class: "point", cx: x(point.elapsed_s), cy: yCore(point.effective_cores), r: 3.5, fill: "#59d8e6"});
    circle.append(svgEl("title", {}, `${fmt(point.elapsed_s, 3)}s · ${fmt(point.effective_cores, 2)} effective cores · ${point.thread_count ?? "—"} threads · ${point.sample_kind}`));
    svg.append(circle);
  });
  byId("trace-meta").textContent = `${shortSha(trace.commit)} · requested ${trace.jobs} jobs · cyan effective cores · amber threads · dashed requested width · ${trace.source}`;
}

function updateAll() { refreshFilterState(); updateDagWeights(); updateCharts(); }

buildControls();
refreshFilterState();
renderDag();
byId("trace-select").addEventListener("change", event => {
  state.trace = event.target.value;
  renderTimeline(DATA.traces.find(trace => trace.key === state.trace) || null);
});
selectStep(state.step);
"""


def _safe_json(value: object) -> str:
    """Serialize JSON safely inside a script element without changing its data."""
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        .replace("&", "\\u0026")
        .replace("<", "\\u003c")
        .replace(">", "\\u003e")
    )


def render_report(data: Mapping[str, object], title: str) -> str:
    """Render one byte-stable report document from a prepared payload."""
    escaped_title = html.escape(title, quote=True)
    warnings_raw = data.get("warnings")
    warnings = warnings_raw if isinstance(warnings_raw, list) else []
    warning_items = "".join(
        f"<li>{html.escape(item)}</li>" for item in warnings if isinstance(item, str)
    )
    warnings_html = (
        f'<ul class="warnings" aria-label="Input notices">{warning_items}</ul>'
        if warning_items
        else ""
    )
    profile_dir = html.escape(str(data.get("profile_dir", "")))
    dag_path = html.escape(str(data.get("dag_path", "")))
    payload = _safe_json(dict(data))
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{escaped_title}</title>
<style>{_STYLE}</style>
</head>
<body>
<header>
  <h1>{escaped_title}</h1>
  <div class="lede">Explore scaling behavior across profiling generations. Points are individual successful trials; cyan fitted lines are per-width medians recomputed from the active filters.</div>
  <div class="source">DAG: {dag_path}<br>Profile store: {profile_dir}</div>
</header>
<main>
{warnings_html}
  <section class="panel" aria-labelledby="filter-title">
    <div class="panel-head"><h2 id="filter-title">Historical window</h2><p>Commit, environment, and workload controls update every graph and fitted curve together.</p></div>
    <div class="controls">
      <label>Commit history<select id="commit-limit"></select></label>
      <label>Machine / container<select id="environment-filter"></select></label>
      <label>Workload revision<select id="workload-filter"></select></label>
    </div>
  </section>
  <section class="panel" aria-labelledby="graph-title">
    <div class="panel-head"><h2 id="graph-title">DAG CPU-work map</h2><p>Click a node to drill down. Circle area scales with median CPU-seconds for the active historical window (with a small legibility floor).</p></div>
    <div id="dag-wrap"><svg id="dag-svg" role="img" aria-label="Interactive DAG profile map"></svg></div>
    <div class="legend-note">Arrows follow dependency order. Node weights and labels change when the historical filters change.</div>
  </section>
  <section class="panel" aria-labelledby="step-title">
    <div class="step-head">
      <div><h2 id="step-title"></h2><p id="step-description"></p></div>
      <label class="step-picker">Step<select id="step-select"></select></label>
    </div>
    <div class="cards">
      <div class="card"><span>Samples</span><strong id="card-samples">—</strong></div>
      <div class="card"><span>Commits in view</span><strong id="card-commits">—</strong></div>
      <div class="card"><span>Best median speedup</span><strong id="card-best">—</strong></div>
      <div class="card"><span>Economic sweet spot</span><strong id="card-sweet">—</strong></div>
      <div class="card"><span>RSS at sweet spot</span><strong id="card-memory">—</strong></div>
      <div class="card"><span>Window</span><strong id="history-label">—</strong></div>
    </div>
    <div class="capture-section">
      <h3>Profiler captures</h3>
      <p>Perf and wprof follow-up trials taken at a selected scaling width. Artifact paths are relative to the profile store.</p>
      <div id="capture-list" class="capture-list" aria-live="polite"></div>
    </div>
    <div class="charts">
      <article class="chart"><h3>Parallel speedup</h3><p>Each trial is normalized to its own commit/environment/workload baseline. Dashed gray is ideal scaling.</p><svg id="speedup-chart" role="img" aria-label="Parallel speedup scatterplot"></svg></article>
      <article class="chart"><h3>Memory response</h3><p>Peak resident memory as requested inner parallelism changes.</p><svg id="memory-chart" role="img" aria-label="Peak memory scatterplot"></svg></article>
      <article class="chart"><h3>CPU-work efficiency</h3><p>Baseline CPU-seconds divided by measured CPU-seconds. 100% conserves work; lower values expose parallel overhead.</p><svg id="efficiency-chart" role="img" aria-label="CPU work efficiency scatterplot"></svg></article>
    </div>
    <div class="legend-note">Point color moves from blue for older commits to gold for newer commits. Hover a point for commit, run, wall, CPU, and memory details.</div>
  </section>
  <section class="panel" aria-labelledby="trace-title">
    <div class="panel-head"><h2 id="trace-title">Parallelism over time</h2><p>Inspect sequential startup/shutdown regions and whether requested workers became effective CPU occupancy.</p></div>
    <div class="trace-controls"><label>Trace run<select id="trace-select"></select></label><div id="trace-meta" class="trace-meta"></div></div>
    <div class="chart" style="margin:0 18px 18px"><svg id="timeline-chart" role="img" aria-label="Effective parallelism over time"></svg></div>
  </section>
  <div class="footer">Standalone dagrun profile report · no network access or external assets required</div>
</main>
<script id="dagrun-report-data" type="application/json">{payload}</script>
<script>{_SCRIPT}</script>
</body>
</html>
"""


def generate_report(
    profile_dir: Path, dag_path: Path, output_path: Path, *, title: str
) -> dict[str, object]:
    """Build and write the report, returning its embedded data for callers/tests."""
    _validate_report_destination(output_path, dag_path)
    if not profile_dir.is_dir():
        raise ReportDataError(f"profile store is not a directory: {profile_dir}")
    _prepare_report_destination(output_path)
    try:
        with _profile_file_lock(output_path):
            _validate_report_destination(output_path, dag_path)
            data = build_report_data(profile_dir, dag_path, report_path=output_path)
            _write_report_document(
                output_path, render_report(data, title), dag_path=dag_path
            )
    except OSError as exc:
        raise ReportDataError(f"cannot lock report {output_path}: {exc}") from exc
    return data


def generate_report_for_config(
    profile_dir: Path,
    cfg: DagConfig,
    output_path: Path,
    *,
    dag_label: str,
    title: str,
) -> dict[str, object]:
    """Build a report from an in-memory DAG, as the sweep command can do without reparsing."""
    dag_path = Path(dag_label)
    _validate_report_destination(output_path, dag_path)
    if not profile_dir.is_dir():
        raise ReportDataError(f"profile store is not a directory: {profile_dir}")
    _prepare_report_destination(output_path)
    try:
        with _profile_file_lock(output_path):
            _validate_report_destination(output_path, dag_path)
            data = build_report_data_for_config(
                profile_dir, cfg, dag_label=dag_label, report_path=output_path
            )
            _write_report_document(
                output_path, render_report(data, title), dag_path=dag_path
            )
    except OSError as exc:
        raise ReportDataError(f"cannot lock report {output_path}: {exc}") from exc
    return data


def _prepare_report_destination(output_path: Path) -> None:
    """Create the report directory before acquiring its persistent sidecar lock."""
    try:
        output_path.parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise ReportDataError(f"cannot create report directory {output_path.parent}: {exc}") from exc


def _paths_resolve_same(left: Path, right: Path) -> bool:
    """Compare lexical, symlink-resolved, and existing-file identities."""
    try:
        if left.resolve(strict=False) == right.resolve(strict=False):
            return True
    except (OSError, RuntimeError):
        if os.path.normcase(os.path.abspath(left)) == os.path.normcase(
            os.path.abspath(right)
        ):
            return True
    try:
        return os.path.samefile(left, right)
    except OSError:
        return False


def _has_report_marker(path: Path) -> bool:
    """Recognize a dagrun-generated report without loading a large report at once."""
    overlap = b""
    try:
        with path.open("rb") as source:
            while chunk := source.read(64 * 1024):
                candidate = overlap + chunk
                if REPORT_DATA_MARKER in candidate:
                    return True
                overlap = candidate[-(len(REPORT_DATA_MARKER) - 1) :]
    except OSError as exc:
        raise ReportDataError(
            f"cannot inspect existing report destination {path}: {exc}"
        ) from exc
    return False


def _validate_report_destination(output_path: Path, dag_path: Path) -> None:
    """Refuse collisions with source/artifact data or arbitrary existing files."""
    if _paths_resolve_same(output_path, dag_path):
        raise ReportDataError(f"report output resolves to DAG input: {output_path}")
    try:
        metadata = output_path.lstat()
    except FileNotFoundError:
        return
    except OSError as exc:
        raise ReportDataError(
            f"cannot inspect existing report destination {output_path}: {exc}"
        ) from exc
    if stat.S_ISLNK(metadata.st_mode):
        raise ReportDataError(
            f"refusing to replace report destination symlink: {output_path}"
        )
    if not stat.S_ISREG(metadata.st_mode):
        raise ReportDataError(
            f"refusing to replace non-regular report destination: {output_path}"
        )
    if not _has_report_marker(output_path):
        raise ReportDataError(
            "refusing to replace existing file without dagrun report marker: "
            f"{output_path}"
        )


def _write_report_document(
    output_path: Path, document: str, *, dag_path: Path
) -> None:
    """Atomically replace a rebuildable report while the caller holds its lock."""
    temporary: Path | None = None
    try:
        _validate_report_destination(output_path, dag_path)
        for _attempt in range(128):
            candidate = output_path.with_name(
                f".{output_path.name}.{os.getpid()}.{secrets.token_hex(8)}.tmp"
            )
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
            try:
                descriptor = os.open(candidate, flags, 0o600)
            except FileExistsError:
                continue
            temporary = candidate
            with os.fdopen(descriptor, "w", encoding="utf-8") as destination:
                destination.write(document)
                destination.flush()
                os.fchmod(destination.fileno(), 0o644)
                os.fsync(destination.fileno())
            break
        if temporary is None:
            raise ReportDataError(
                f"cannot create a unique temporary report beside {output_path}"
            )
        _validate_report_destination(output_path, dag_path)
        os.replace(temporary, output_path)
    except OSError as exc:
        raise ReportDataError(f"cannot write report {output_path}: {exc}") from exc
    finally:
        if temporary is not None:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass


def write_profile_report(
    profile_dir: Path,
    cfg: DagConfig,
    *,
    dag_label: str,
    title: str = "dagrun profile history",
) -> Path:
    """Refresh the canonical report sidecar used by a successful profiling sweep."""
    output_path = profile_dir / PROFILE_REPORT_FILENAME
    generate_report_for_config(
        profile_dir, cfg, output_path, dag_label=dag_label, title=title
    )
    return output_path


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog=PROG,
        description=(
            "Build one deterministic, standalone interactive HTML report from a dagrun "
            "profile store, its interval traces, and the DAG that names the graph."
        ),
    )
    parser.add_argument("--version", action="version", version=f"{PROG} {__version__}")
    parser.add_argument(
        "--userguide",
        action="store_true",
        help="print dagrun's complete embedded user guide and exit",
    )
    parser.add_argument("--profile-dir", type=Path, required=True, help="dagrun profile store")
    parser.add_argument("--dag", type=Path, required=True, help="authored DAG in JSON or YAML")
    parser.add_argument("--output", type=Path, required=True, help="standalone HTML path to write")
    parser.add_argument("--title", default="dagrun profile history", help="report heading")
    return parser


def _main(argv: Sequence[str] | None = None) -> int:
    raw_args = list(argv) if argv is not None else sys.argv[1:]
    # Keep the package-wide command contract: every installed command can print the exact bundled
    # user guide without requiring unrelated command arguments.
    if raw_args == ["--userguide"]:
        guide = (importlib.resources.files("dagrun") / "USER_GUIDE.md").read_text(
            encoding="utf-8"
        )
        sys.stdout.write(guide)
        return 0
    args = _parser().parse_args(raw_args)
    if bool(args.userguide):
        guide = (importlib.resources.files("dagrun") / "USER_GUIDE.md").read_text(
            encoding="utf-8"
        )
        sys.stdout.write(guide)
        return 0
    try:
        data = generate_report(args.profile_dir, args.dag, args.output, title=args.title)
    except ReportDataError as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    records = data.get("records")
    traces = data.get("traces")
    record_count = len(records) if isinstance(records, list) else 0
    trace_count = len(traces) if isinstance(traces, list) else 0
    print(
        f"{PROG}: wrote {args.output} ({record_count} aggregate samples, "
        f"{trace_count} time-series runs)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
