"""Focused contracts for the standalone dagrun history report."""

from __future__ import annotations

import csv
import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path
from types import ModuleType

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]


def _load_report() -> ModuleType:
    path = REPO_ROOT / "py" / "dagrun" / "profile_report.py"
    spec = importlib.util.spec_from_file_location("_dagrun_profile_report_test", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _write_csv(path: Path, rows: list[dict[str, str]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fields = sorted({key for row in rows for key in row})
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields, lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def _profile_row(
    *,
    step: str,
    commit: str,
    jobs: int,
    elapsed: float,
    cpu: float,
    peak: int,
    run_id: str,
    ok: str = "true",
) -> dict[str, str]:
    return {
        "timestamp": f"2026-08-{20 + int(commit[-1])}T00:00:00Z",
        "machine_id": "machine-a",
        "container_class": "affinity-8_quota-8",
        "git_sha": commit,
        "runner_name": "dagrun-py",
        "run_id": run_id,
        "step": step,
        "inner_jobs": str(jobs),
        "elapsed_s": str(elapsed),
        "user_s": str(cpu - 0.1),
        "sys_s": "0.1",
        "effective_cores": str(cpu / elapsed),
        "peak_bytes": str(peak),
        "workload_digest": "digest-a",
        "returncode": "0",
        "ok": ok,
        "timed_out": "false",
        "cpu_timed_out": "false",
        "oom_kills": "0",
    }


def _trace_rows() -> list[dict[str, str]]:
    common = {
        "timestamp": "2026-08-22T00:00:00Z",
        "machine_id": "machine-a",
        "container_class": "affinity-8_quota-8",
        "git_sha": "commit2",
        "run_id": "run-p4-c2",
        "step": "build.compile",
        "inner_jobs": "4",
        "workload_digest": "digest-a",
    }
    return [
        {**common, "sample_index": "0", "sample_kind": "start", "elapsed_s": "0", "interval_s": "", "effective_cores": "", "user_cores": "", "system_cores": "", "thread_count": "1", "throttled_s": "0"},
        {**common, "sample_index": "1", "sample_kind": "periodic", "elapsed_s": "0.5", "interval_s": "0.5", "effective_cores": "3.2", "user_cores": "3.0", "system_cores": "0.2", "thread_count": "5", "throttled_s": "0"},
        {**common, "sample_index": "2", "sample_kind": "final", "elapsed_s": "1", "interval_s": "0.5", "effective_cores": "1.1", "user_cores": "1.0", "system_cores": "0.1", "thread_count": "2", "throttled_s": "0"},
    ]


def _write_capture_manifests(profile_dir: Path) -> None:
    capture = profile_dir / "captures" / "capture-001"
    artifact = capture / "perf-001" / "perf data#1.data"
    artifact.parent.mkdir(parents=True)
    artifact.write_bytes(b"PERFILE2")
    (capture / "manifest.json").write_text(
        json.dumps(
            {
                "schema": "dagrun-profile-capture-v1",
                "capture_id": "capture-001",
                "state": "complete",
                "machine_id": "capture-machine",
                "container_class": "capture-container",
                "created_at": "2026-08-22T00:01:00Z",
                "finished_at": "2026-08-22T00:01:03Z",
                "artifact_root": ".",
                "selection": {
                    "step": "build.compile",
                    "workload_digest": "digest-a",
                    "inner_jobs": 4,
                    "expected_wall_s": 2.5,
                    "speedup": 3.2,
                    "git_sha": "commit2",
                },
                "preflight": [],
                "trials": [
                    {
                        "trial_id": "perf-001",
                        "kind": "perf",
                        "state": "complete",
                        "inner_jobs": 4,
                        "started_at": "2026-08-22T00:01:00Z",
                        "finished_at": "2026-08-22T00:01:03Z",
                        "measured_wall_s": 2.6,
                        "workload_returncode": 0,
                        "profiler_returncode": 0,
                        "included_in_model": False,
                        "artifacts": [
                            {
                                "role": "perf-data",
                                "path": "perf-001/perf data#1.data",
                                "size_bytes": 8,
                                "mode": "0o600",
                            },
                            {
                                "role": "perf-log",
                                "path": "perf-001/perf.stderr.log",
                                "size_bytes": 0,
                                "mode": "0o600",
                            },
                        ],
                        "error": "",
                    }
                ],
                "errors": [],
            }
        ),
        encoding="utf-8",
    )
    malformed = profile_dir / "captures" / "bad-json"
    malformed.mkdir()
    (malformed / "manifest.json").write_text("{not json", encoding="utf-8")
    wrong_schema = profile_dir / "captures" / "bad-schema"
    wrong_schema.mkdir()
    (wrong_schema / "manifest.json").write_text(
        json.dumps({"schema": "future-capture-v9"}), encoding="utf-8"
    )


def _fixture(tmp_path: Path) -> tuple[Path, Path]:
    profile_dir = tmp_path / "profiles"
    dag_path = tmp_path / "pipeline.json"
    dag_path.write_text(
        json.dumps(
            {
                "description": "Test build graph",
                "steps": [
                    {"group": "fetch", "job": "source", "cmd": "true", "desc": "fetch"},
                    {"group": "build", "job": "compile", "cmd": "true", "deps": ["fetch.source"], "description": "Compile everything"},
                    {"group": "test", "job": "unit", "cmd": "true", "deps": ["build.compile"]},
                ],
            }
        ),
        encoding="utf-8",
    )
    rows = [
        _profile_row(step="build.compile", commit="commit1", jobs=1, elapsed=10, cpu=9, peak=1000, run_id="run-p1-c1"),
        _profile_row(step="build.compile", commit="commit1", jobs=4, elapsed=3, cpu=11, peak=1800, run_id="run-p4-c1"),
        _profile_row(step="build.compile", commit="commit2", jobs=1, elapsed=8, cpu=8, peak=1100, run_id="run-p1-c2"),
        _profile_row(step="build.compile", commit="commit2", jobs=4, elapsed=2.5, cpu=10, peak=1900, run_id="run-p4-c2"),
        _profile_row(step="test.unit", commit="commit2", jobs=1, elapsed=4, cpu=3.5, peak=800, run_id="run-test"),
        _profile_row(step="build.compile", commit="commit2", jobs=8, elapsed=1, cpu=4, peak=2000, run_id="failed", ok="false"),
        _profile_row(step="old.removed", commit="commit2", jobs=1, elapsed=1, cpu=1, peak=1, run_id="old"),
    ]
    _write_csv(profile_dir / "step_profiles_machine-a_affinity-8.csv", rows)
    _write_csv(profile_dir / "traces" / "run-p4-c2.csv", _trace_rows())
    _write_capture_manifests(profile_dir)
    return profile_dir, dag_path


def _embedded_payload(document: str) -> dict[str, object]:
    marker = '<script id="dagrun-report-data" type="application/json">'
    start = document.index(marker) + len(marker)
    end = document.index("</script>", start)
    raw: object = json.loads(document[start:end])
    assert isinstance(raw, dict)
    return {str(key): value for key, value in raw.items()}


def test_report_embeds_history_graph_traces_and_no_external_assets(tmp_path: Path) -> None:
    report = _load_report()
    profile_dir, dag_path = _fixture(tmp_path)
    output = tmp_path / "out" / "report.html"

    data = report.generate_report(profile_dir, dag_path, output, title="Scaling <study>")
    first = output.read_text(encoding="utf-8")
    report.generate_report(profile_dir, dag_path, output, title="Scaling <study>")
    assert output.read_text(encoding="utf-8") == first

    assert "Scaling &lt;study&gt;" in first
    assert "https://" not in first
    assert "http://" not in first.replace("http://www.w3.org/2000/svg", "")
    assert "<script src=" not in first
    assert 'id="commit-limit"' in first
    assert 'id="dag-svg"' in first
    assert 'id="speedup-chart"' in first
    assert 'id="memory-chart"' in first
    assert 'id="efficiency-chart"' in first
    assert 'id="timeline-chart"' in first
    assert "per-width medians" in first
    assert "Math.sqrt(area / Math.PI)" in first

    payload = _embedded_payload(first)
    records = payload["records"]
    traces = payload["traces"]
    captures = payload["captures"]
    graph = payload["graph"]
    warnings = payload["warnings"]
    assert isinstance(records, list)
    assert isinstance(traces, list)
    assert isinstance(captures, list)
    assert isinstance(graph, dict)
    assert isinstance(warnings, list)
    assert len(records) == 5
    assert len(traces) == 1
    assert len(captures) == 1
    capture = captures[0]
    assert capture["step"] == "build.compile"
    assert capture["state"] == "complete"
    assert capture["machine"] == "capture-machine"
    assert capture["container"] == "capture-container"
    assert capture["environment"] == "capture-machine␟capture-container"
    assert capture["manifest_path"] == "captures/capture-001/manifest.json"
    artifacts = capture["trials"][0]["artifacts"]
    assert artifacts[0] == {
        "exists": True,
        "href": "../profiles/captures/capture-001/perf-001/perf%20data%231.data",
        "mode": "0o600",
        "path": "captures/capture-001/perf-001/perf data#1.data",
        "role": "perf-data",
        "size_bytes": 8,
    }
    assert artifacts[1]["exists"] is False
    assert 'htmlEl("a", "", label)' in first
    assert 'link.setAttribute("href", artifact.href)' in first
    assert "capture.step === state.step && baseFilter(capture)" in first
    environments = payload["environments"]
    assert isinstance(environments, list)
    assert {item["key"] for item in environments if isinstance(item, dict)} == {
        "machine-a␟affinity-8_quota-8",
        "capture-machine␟capture-container",
    }
    assert [step["tag"] for step in graph["steps"]] == [
        "fetch.source",
        "build.compile",
        "test.unit",
    ]
    assert graph["edges"] == [
        {"from": "fetch.source", "to": "build.compile"},
        {"from": "build.compile", "to": "test.unit"},
    ]
    assert any("failed or interrupted" in warning for warning in warnings)
    assert any("absent from the supplied DAG" in warning for warning in warnings)
    assert {
        warning
        for warning in warnings
        if warning.startswith("Ignored malformed capture manifest")
    } == {
        "Ignored malformed capture manifest captures/bad-json/manifest.json.",
        "Ignored malformed capture manifest captures/bad-schema/manifest.json.",
    }
    assert 'id="capture-list"' in first
    assert "Perf and wprof follow-up trials" in first
    assert data["schema"] == 1

    cfg = report.load_dag_config(dag_path)
    canonical = report.write_profile_report(
        profile_dir, cfg, dag_label=dag_path.as_posix(), title="Canonical report"
    )
    assert canonical == profile_dir / "profile_report.html"
    assert canonical.is_file()
    assert (profile_dir / ".locks" / "profile_report.html.lock").is_file()
    canonical_payload = _embedded_payload(canonical.read_text(encoding="utf-8"))
    canonical_captures = canonical_payload["captures"]
    assert isinstance(canonical_captures, list)
    canonical_capture = canonical_captures[0]
    assert isinstance(canonical_capture, dict)
    canonical_trials = canonical_capture["trials"]
    assert isinstance(canonical_trials, list)
    canonical_trial = canonical_trials[0]
    assert isinstance(canonical_trial, dict)
    canonical_artifacts = canonical_trial["artifacts"]
    assert isinstance(canonical_artifacts, list)
    canonical_artifact = canonical_artifacts[0]
    assert isinstance(canonical_artifact, dict)
    assert canonical_artifact["href"] == (
        "captures/capture-001/perf-001/perf%20data%231.data"
    )


def test_legacy_capture_has_blank_environment_provenance(tmp_path: Path) -> None:
    report = _load_report()
    profile_dir, dag_path = _fixture(tmp_path)
    source = profile_dir / "captures" / "capture-001" / "manifest.json"
    manifest = json.loads(source.read_text(encoding="utf-8"))
    manifest.pop("machine_id")
    manifest.pop("container_class")
    manifest["capture_id"] = "legacy-capture"
    manifest["created_at"] = "2026-08-23T00:01:00Z"
    legacy = profile_dir / "captures" / "legacy-capture"
    legacy.mkdir()
    (legacy / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")

    payload = report.build_report_data(profile_dir, dag_path)
    captures = payload["captures"]
    assert isinstance(captures, list)
    capture = next(item for item in captures if item["capture_id"] == "legacy-capture")
    assert capture["machine"] == ""
    assert capture["container"] == ""
    assert capture["environment"] == ""
    assert all(
        item["key"] != "␟"
        for item in payload["environments"]
        if isinstance(item, dict)
    )


def test_report_destination_guard_preserves_inputs_and_allows_reports(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    report = _load_report()
    profile_dir, dag_path = _fixture(tmp_path)
    cfg = report.load_dag_config(dag_path)

    output = tmp_path / "site" / "new-report.html"
    report.generate_report(profile_dir, dag_path, output, title="First")
    first = output.read_bytes()
    report.generate_report(profile_dir, dag_path, output, title="Second")
    assert output.read_bytes() != first
    assert report.REPORT_DATA_MARKER in output.read_bytes()
    assert output.stat().st_mode & 0o777 == 0o644
    assert not list(output.parent.glob(f".{output.name}.*.tmp"))

    dag_before = dag_path.read_bytes()
    with pytest.raises(report.ReportDataError, match="report output resolves to DAG input"):
        report.generate_report(profile_dir, dag_path, dag_path, title="Unsafe")
    assert dag_path.read_bytes() == dag_before

    hardlink = tmp_path / "dag-hardlink.json"
    hardlink.hardlink_to(dag_path)
    with pytest.raises(report.ReportDataError, match="report output resolves to DAG input"):
        report.generate_report(profile_dir, dag_path, hardlink, title="Unsafe")
    assert dag_path.read_bytes() == dag_before

    arbitrary = tmp_path / "keep.txt"
    arbitrary.write_bytes(b"do not overwrite")
    protected = [
        next(profile_dir.glob("step_profiles_*.csv")),
        profile_dir / "traces" / "run-p4-c2.csv",
        profile_dir / "captures" / "capture-001" / "manifest.json",
        profile_dir / "captures" / "capture-001" / "perf-001" / "perf data#1.data",
        arbitrary,
    ]
    for destination in protected:
        before = destination.read_bytes()
        with pytest.raises(
            report.ReportDataError,
            match="refusing to replace existing file without dagrun report marker",
        ):
            report.generate_report(profile_dir, dag_path, destination, title="Unsafe")
        assert destination.read_bytes() == before

    link = tmp_path / "report-link.html"
    link.symlink_to(output)
    with pytest.raises(
        report.ReportDataError, match="refusing to replace report destination symlink"
    ):
        report.generate_report(profile_dir, dag_path, link, title="Unsafe")
    assert link.is_symlink()

    directory = tmp_path / "existing-directory"
    directory.mkdir()
    with pytest.raises(
        report.ReportDataError, match="refusing to replace non-regular report destination"
    ):
        report.generate_report(profile_dir, dag_path, directory, title="Unsafe")

    guarded = tmp_path / "attacker-controlled" / "report.html"
    guarded.parent.mkdir()
    victim = tmp_path / "temp-victim"
    victim.write_bytes(b"must survive")
    planted = guarded.with_name(f".{guarded.name}.{os.getpid()}.planted.tmp")
    planted.symlink_to(victim)
    tokens = iter(("planted", "fresh"))
    monkeypatch.setattr(report.secrets, "token_hex", lambda _size: next(tokens))
    report.generate_report(profile_dir, dag_path, guarded, title="Safe temporary")
    assert victim.read_bytes() == b"must survive"
    assert planted.is_symlink()
    assert report.REPORT_DATA_MARKER in guarded.read_bytes()


def test_cli_accepts_yaml_and_reports_counts(tmp_path: Path) -> None:
    profile_dir = tmp_path / "profiles"
    profile_dir.mkdir()
    dag = tmp_path / "pipeline.yaml"
    dag.write_text(
        """description: tiny graph
steps:
  - group: on
    job: yes
    cmd: "true"
""",
        encoding="utf-8",
    )
    output = tmp_path / "report.html"
    result = subprocess.run(
        [
            sys.executable,
            str(REPO_ROOT / "scripts" / "dagrun_profile_report.py"),
            "--profile-dir",
            str(profile_dir),
            "--dag",
            str(dag),
            "--output",
            str(output),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert "0 aggregate samples, 0 time-series runs" in result.stdout
    assert output.is_file()
    payload = _embedded_payload(output.read_text(encoding="utf-8"))
    assert payload["warnings"] == [
        "No step_profiles_*.csv files were found in the profile store."
    ]
    graph = payload["graph"]
    assert isinstance(graph, dict)
    steps = graph["steps"]
    assert isinstance(steps, list)
    first_step = steps[0]
    assert isinstance(first_step, dict)
    assert first_step["tag"] == "on.yes"


def test_dag_layout_rejects_unknown_dependency_and_cycle(tmp_path: Path) -> None:
    report = _load_report()
    unknown = tmp_path / "unknown.json"
    unknown.write_text(
        json.dumps(
            {"steps": [{"group": "a", "job": "one", "cmd": "true", "deps": ["gone.step"]}]}
        ),
        encoding="utf-8",
    )
    try:
        report.load_dag(unknown)
    except report.ReportDataError as exc:
        assert "depends on 'gone.step'" in str(exc)
    else:
        raise AssertionError("unknown dependency was accepted")

    cycle = tmp_path / "cycle.json"
    cycle.write_text(
        json.dumps(
            {
                "steps": [
                    {"group": "a", "job": "one", "cmd": "true", "deps": ["b.two"]},
                    {"group": "b", "job": "two", "cmd": "true", "deps": ["a.one"]},
                ]
            }
        ),
        encoding="utf-8",
    )
    try:
        report.load_dag(cycle)
    except report.ReportDataError as exc:
        assert "dependency cycle" in str(exc)
    else:
        raise AssertionError("dependency cycle was accepted")
