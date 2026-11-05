"""Tests for the CSV-backed metrics sink (perflog.CsvMetricsSink).

Guards the fix that made ``run_dag(cfg, metrics=CsvMetricsSink(dir))`` actually write
its per-step and whole-run CSVs instead of raising: the scheduler's per-step row keys did
not match the writer's fieldnames (a ``ValueError``) and the whole-run appender opened a
not-yet-existent file for reading (a ``FileNotFoundError``).
"""

from __future__ import annotations

from dagrun import perflog

import csv
import tempfile
import time
from pathlib import Path

import pytest

from dagrun import DagConfig, Step, run_dag, run_dag_limited
from dagrun.perflog import (
    CsvMetricsSink,
    append_step_profiles,
    machine_id,
)

# The standard per-step columns the scheduler always emits (via the stamped run context and
# the row it builds in _run_step). Dynamic ``cpu.*`` columns may be appended after these.
_EXPECTED_STEP_COLUMNS = {
    "timestamp", "machine_id", "container_class", "git_sha", "outer_jobs",
    "profile_base_sha", "enforcement_kind", "runner_name",
    "step", "classification", "inner_jobs", "elapsed_s", "returncode", "ok",
    "timed_out", "oom_kills", "peak_bytes", "thread_peak",
}


def _tiny_dag() -> DagConfig:
    return DagConfig(
        steps=(
            Step("build", "app", "compile", "true"),
            Step("test", "unit", "unit tests", "true", deps=["build.app"]),
            Step("lint", "fmt", "format check", "true"),
        )
    )


def test_profile_timestamp_uses_utc(monkeypatch: pytest.MonkeyPatch) -> None:
    sentinel = object()
    seen: list[object | None] = []

    monkeypatch.setattr(time, "gmtime", lambda: sentinel)

    def fake_strftime(fmt: str, value: object | None = None) -> str:
        assert fmt == "%Y-%m-%dT%H:%M:%SZ"
        seen.append(value)
        return "2026-08-31T12:34:56Z"

    monkeypatch.setattr(time, "strftime", fake_strftime)
    assert perflog._timestamp() == "2026-08-31T12:34:56Z"
    assert seen == [sentinel]


def test_csv_metrics_sink_writes_per_step_and_whole_run() -> None:
    """A run with a CsvMetricsSink writes a non-empty per-step CSV (one row per step, with
    the expected columns) plus a whole-run CSV, and raises nothing."""
    with tempfile.TemporaryDirectory() as d:
        sink = CsvMetricsSink(d, git_sha="deadbeef")
        result = run_dag(_tiny_dag(), jobs=4, metrics=sink, verbosity=0)
        assert result.ok

        step_csvs = list(Path(d).glob("step_profiles_*.csv"))
        assert len(step_csvs) == 1, f"expected exactly one per-step CSV, got {step_csvs}"
        step_csv = step_csvs[0]
        assert step_csv.stat().st_size > 0

        with step_csv.open(newline="") as f:
            reader = csv.DictReader(f)
            header = set(reader.fieldnames or [])
            rows = list(reader)
        assert _EXPECTED_STEP_COLUMNS <= header, (
            f"missing columns: {_EXPECTED_STEP_COLUMNS - header}"
        )
        assert len(rows) == 3  # one row per step
        assert {row["step"] for row in rows} == {"build.app", "test.unit", "lint.fmt"}
        assert all(row["git_sha"] == "deadbeef" for row in rows)
        assert all(row["outer_jobs"] == "4" for row in rows)

        whole_run_csv = Path(d) / f"{machine_id()}.csv"
        assert whole_run_csv.exists() and whole_run_csv.stat().st_size > 0
        with whole_run_csv.open(newline="") as f:
            whole_rows = list(csv.DictReader(f))
        assert len(whole_rows) == 1
        assert whole_rows[0]["result"] == "pass"
        assert whole_rows[0]["n_steps"] == "3"

        # Stable hidden lock inodes are shared with the Rust writer. They stay out of the visible
        # profile-artifact namespace and must not use the old racy `*.csv.lock` location.
        assert not list(Path(d).glob("*.lock"))
        assert (Path(d) / ".locks" / f"{step_csv.name}.lock").is_file()
        assert (Path(d) / ".locks" / f"{whole_run_csv.name}.lock").is_file()

        # New rows use the canonical Rust-compatible scalar spellings. Readers remain tolerant of
        # historical Python rows that used title-case booleans or shorter decimals.
        assert all(row["ok"] == "true" for row in rows)
        assert all(row["timed_out"] == "false" for row in rows)
        assert all(len(row["elapsed_s"].partition(".")[2]) == 3 for row in rows)


def test_independent_cpu_budget_keeps_legacy_jobs_columns_as_max_steps() -> None:
    """The existing profile schema keeps ``jobs``/``outer_jobs`` as active-step ceiling."""
    with tempfile.TemporaryDirectory() as d:
        result = run_dag_limited(
            _tiny_dag(),
            max_steps=2,
            max_cpus=8,
            metrics=CsvMetricsSink(d, git_sha="limits"),
            verbosity=0,
        )
        assert result.ok
        step_csv = next(Path(d).glob("step_profiles_*.csv"))
        with step_csv.open(newline="") as f:
            step_rows = list(csv.DictReader(f))
        assert step_rows and all(row["outer_jobs"] == "2" for row in step_rows)
        with (Path(d) / f"{machine_id()}.csv").open(newline="") as f:
            whole_rows = list(csv.DictReader(f))
        assert whole_rows[-1]["jobs"] == "2"


def test_dynamic_cpu_columns_are_sorted_alphabetically() -> None:
    """The dynamic ``cpu.*`` columns must be appended in ALPHABETICAL order (not the cgroup
    ``cpu.stat`` read order), so the CSV header is byte-identical to the Rust build (which sorts
    the same set via a BTreeSet). Feed the keys in a deliberately non-sorted order and assert the
    header tail comes back sorted."""
    with tempfile.TemporaryDirectory() as d:
        # cpu.stat native order (usage/user/system/nice/...) is NOT alphabetical.
        row: dict[str, object] = {
            "step": "g.j",
            "cpu.usage_usec": 5,
            "cpu.user_usec": 4,
            "cpu.system_usec": 3,
            "cpu.nice_usec": 2,
        }
        path = append_step_profiles(d, [row], git_sha="abc", outer_jobs=1)
        assert path is not None
        with open(path, newline="") as f:
            header = next(csv.reader(f))
        cpu_cols = [col for col in header if col.startswith("cpu.")]
        assert cpu_cols == sorted(cpu_cols), f"cpu.* columns not sorted: {cpu_cols}"
        assert cpu_cols == [
            "cpu.nice_usec",
            "cpu.system_usec",
            "cpu.usage_usec",
            "cpu.user_usec",
        ]


def test_profile_csvs_use_lf_line_endings() -> None:
    """Profile-store CSVs must be written with LF (``\\n``) line endings, never the csv module's
    default CRLF (``\\r\\n``), so the store is byte-identical to the Rust build (which writes LF)
    and a checked-in store does not churn every line when regenerated by the other build."""
    with tempfile.TemporaryDirectory() as d:
        run_dag(_tiny_dag(), jobs=2, metrics=CsvMetricsSink(d, git_sha="deadbeef"), verbosity=0)
        for name in ("step_profiles_*.csv", f"{machine_id()}.csv"):
            for csv_path in Path(d).glob(name):
                raw = csv_path.read_bytes()
                assert b"\r\n" not in raw, f"{csv_path} has CRLF line endings (expected LF)"
                assert b"\n" in raw, f"{csv_path} has no LF line endings"


def test_csv_metrics_sink_appends_across_runs() -> None:
    """A second run into the same directory appends (does not clobber): the per-step CSV
    accumulates rows and the header stays intact."""
    with tempfile.TemporaryDirectory() as d:
        for _ in range(2):
            run_dag(
                _tiny_dag(),
                jobs=2,
                metrics=CsvMetricsSink(d, git_sha="cafef00d"),
                verbosity=0,
            )
        step_csv = next(iter(Path(d).glob("step_profiles_*.csv")))
        with step_csv.open(newline="") as f:
            rows = list(csv.DictReader(f))
        assert len(rows) == 6  # 3 steps x 2 runs, single header


def test_effective_cpu_quota_reports_max_when_no_ancestor_constrains_cpu(tmp_path: Path) -> None:
    """An unconstrained chain reports ``max``, which is distinct from an unreadable one."""
    root = tmp_path / "cgroup"
    (root / "a" / "b").mkdir(parents=True)
    for node in (root, root / "a", root / "a" / "b"):
        (node / "cpu.max").write_text("max 100000\n")
    assert perflog.effective_cpu_quota(mount_root=root, relpath="/a/b") == "max"


def test_effective_cpu_quota_takes_the_binding_ancestor_limit(tmp_path: Path) -> None:
    """A quota on a slice several levels up still binds the leaf, so the key must report it."""
    root = tmp_path / "cgroup"
    (root / "slice" / "scope").mkdir(parents=True)
    (root / "cpu.max").write_text("max 100000\n")
    (root / "slice" / "cpu.max").write_text("1440000 100000\n")   # 14.4 cores, the binding cap
    (root / "slice" / "scope" / "cpu.max").write_text("max 100000\n")
    assert perflog.effective_cpu_quota(mount_root=root, relpath="/slice/scope") == "1440000_100000"


def test_effective_cpu_quota_prefers_the_most_restrictive_of_several(tmp_path: Path) -> None:
    """With caps at more than one level the smallest cores-per-period wins."""
    root = tmp_path / "cgroup"
    (root / "a" / "b").mkdir(parents=True)
    (root / "cpu.max").write_text("max 100000\n")
    (root / "a" / "cpu.max").write_text("3200000 100000\n")       # 32 cores
    (root / "a" / "b" / "cpu.max").write_text("400000 100000\n")  # 4 cores, binding
    assert perflog.effective_cpu_quota(mount_root=root, relpath="/a/b") == "400000_100000"


def test_effective_cpu_quota_is_unknown_when_the_hierarchy_is_unreadable(tmp_path: Path) -> None:
    """No readable cpu.max anywhere is reported as unknown, never as unconstrained."""
    root = tmp_path / "cgroup"
    (root / "a").mkdir(parents=True)
    assert perflog.effective_cpu_quota(mount_root=root, relpath="/a") == "unknown"


def test_container_class_names_both_the_width_and_the_quota() -> None:
    """The key must carry both constraints; a quota-blind key merges unlike containers."""
    key = perflog.container_class()
    width, _, quota = key.partition("_cpu-max-")
    assert width.startswith("affinity") and width[len("affinity"):].isdigit()
    assert quota
