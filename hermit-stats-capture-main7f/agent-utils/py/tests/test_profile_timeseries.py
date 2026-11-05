"""Opt-in per-step CPU time-series profiling contracts."""

from __future__ import annotations

import csv
import threading
from collections.abc import Mapping
from pathlib import Path

import pytest

import dagrun.cli as cli
from dagrun.cli import _parse_profile_timeseries_duration, build_parser, main
from dagrun.model import DagConfig, ResourceHint, Step
from dagrun.perflog import STEP_TIMESERIES_COLUMNS, CsvMetricsSink
from dagrun.scheduler import _step_timeseries_sample, run_dag_limited


class _TracingCgroups:
    """Enabled fake whose cumulative counters advance on every observation."""

    enabled = True

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._read = 0
        self.cleaned = False

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        return cmd

    def prepare_delegated_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> tuple[str, str | None]:
        return cmd, None

    def kill(self, tag: str) -> bool:
        return False

    def cleanup(self, tag: str) -> None:
        self.cleaned = True

    def set_worker_pids_max(self, limit: int | None) -> None:
        return None

    def pids_events(self, tag: str) -> int:
        return 0

    def oom_kills(self, tag: str) -> int:
        return 0

    def memory_events(self, tag: str) -> Mapping[str, int] | None:
        return {"low": 0, "high": 0, "max": 0, "oom": 0, "oom_kill": 0}

    def applied_memory_max(self, tag: str) -> str | None:
        return "max"

    def peak_bytes(self, tag: str) -> int | None:
        return 4096

    def cpu_stats(self, tag: str) -> Mapping[str, int] | None:
        assert not self.cleaned, "the final sample must precede cgroup cleanup"
        with self._lock:
            self._read += 1
            tick = self._read
        return {
            "usage_usec": tick * 100_000,
            "user_usec": tick * 80_000,
            "system_usec": tick * 20_000,
            "nr_throttled": tick,
            "throttled_usec": tick * 5_000,
        }

    def cpu_pressure(self, tag: str) -> Mapping[str, float] | None:
        return {"avg10": 0.0, "avg60": 0.0}

    def thread_count(self, tag: str) -> int | None:
        return 4

    def kill_all_remaining(self) -> int:
        return 0


def _cfg(command: str = "sleep 0.13") -> DagConfig:
    return DagConfig(
        steps=(
            Step(
                "build",
                "main",
                "build",
                command,
                hint=ResourceHint(preferred_inner_jobs=4),
                jobs_flag="",
            ),
        )
    )


@pytest.mark.parametrize(
    ("raw", "seconds"),
    (("50ms", 0.05), ("0.25s", 0.25), ("2", 2.0), ("10s", 10.0)),
)
def test_profile_timeseries_duration_accepts_bounded_units(raw: str, seconds: float) -> None:
    assert _parse_profile_timeseries_duration(raw) == pytest.approx(seconds)


@pytest.mark.parametrize("raw", ("49ms", "10.001s", "0", "-1s", "nan", "oops"))
def test_profile_timeseries_duration_rejects_out_of_range_or_invalid_values(raw: str) -> None:
    with pytest.raises(ValueError, match="--profile-timeseries"):
        _parse_profile_timeseries_duration(raw)


def test_run_and_sweep_parsers_expose_the_opt_in_interval() -> None:
    parser = build_parser()
    run = parser.parse_args(["run", "--dag", "dag.json", "--profile-timeseries", "250ms"])
    sweep = parser.parse_args(
        [
            "sweep",
            "--dag",
            "dag.json",
            "--step",
            "g.j",
            "--jobs",
            "1",
            "--profile-timeseries",
            "1s",
        ]
    )
    assert run.profile_timeseries == "250ms"
    assert sweep.profile_timeseries == "1s"


def test_sample_math_uses_consecutive_deltas_and_blanks_resets() -> None:
    start = _step_timeseries_sample(
        step="build.main",
        inner_jobs=4,
        sample_index=0,
        sample_kind="start",
        elapsed_s=0.0,
        cpu_stats={
            "usage_usec": 1_000_000,
            "user_usec": 800_000,
            "system_usec": 200_000,
            "throttled_usec": 50_000,
        },
        thread_count=1,
        previous_elapsed_s=None,
        previous_cpu_stats=None,
    )
    assert start["elapsed_s"] == "0.000000"
    assert start["cpu_usage_s"] == "1.000000"
    assert start["interval_s"] == ""
    assert start["effective_cores"] == ""

    periodic = _step_timeseries_sample(
        step="build.main",
        inner_jobs=4,
        sample_index=1,
        sample_kind="periodic",
        elapsed_s=0.25,
        cpu_stats={
            "usage_usec": 1_500_000,
            "user_usec": 1_200_000,
            "system_usec": 300_000,
            "throttled_usec": 100_000,
        },
        thread_count=8,
        previous_elapsed_s=0.0,
        previous_cpu_stats={
            "usage_usec": 1_000_000,
            "user_usec": 800_000,
            "system_usec": 200_000,
            "throttled_usec": 50_000,
        },
    )
    assert periodic["interval_s"] == "0.250000"
    assert periodic["effective_cores"] == "2.0000"
    assert periodic["user_cores"] == "1.6000"
    assert periodic["system_cores"] == "0.4000"
    assert periodic["interval_throttled_s"] == "0.050000"
    assert periodic["thread_count"] == 8

    reset = _step_timeseries_sample(
        step="build.main",
        inner_jobs=4,
        sample_index=2,
        sample_kind="final",
        elapsed_s=0.5,
        cpu_stats={"usage_usec": 1, "user_usec": 1},
        thread_count=None,
        previous_elapsed_s=0.25,
        previous_cpu_stats={"usage_usec": 1_500_000, "user_usec": 1_200_000},
    )
    assert reset["interval_s"] == "0.250000"
    assert reset["cpu_usage_s"] == "0.000001"
    assert reset["effective_cores"] == ""
    assert reset["user_cores"] == ""
    assert reset["system_cores"] == ""
    assert reset["thread_count"] == ""


def test_scheduler_collects_start_periodic_final_and_persists_exact_trace(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    cgroups = _TracingCgroups()
    sink = CsvMetricsSink(
        tmp_path,
        git_sha="deadbeef",
        enforcement_kind="cgroup-v2",
        runner_name="dagrun",
        run_id="trace-run",
    )
    result = run_dag_limited(
        _cfg(),
        max_steps=1,
        max_cpus=4,
        cgroups=cgroups,
        metrics=sink,
        verbosity=0,
        profile_timeseries_interval_s=0.05,
    )

    assert result.ok
    assert cgroups.cleaned
    assert len(result.step_timeseries_rows) >= 3
    assert result.step_timeseries_rows[0]["sample_kind"] == "start"
    assert result.step_timeseries_rows[-1]["sample_kind"] == "final"
    assert [row["sample_index"] for row in result.step_timeseries_rows] == list(
        range(len(result.step_timeseries_rows))
    )
    assert any(row["sample_kind"] == "periodic" for row in result.step_timeseries_rows)
    assert all(row["thread_count"] == 4 for row in result.step_timeseries_rows)

    trace = tmp_path / "traces" / "trace-run.csv"
    assert trace.exists()
    with trace.open(newline="", encoding="utf-8") as handle:
        reader = csv.DictReader(handle)
        assert tuple(reader.fieldnames or ()) == tuple(STEP_TIMESERIES_COLUMNS)
        rows = list(reader)
    assert len(rows) == len(result.step_timeseries_rows)
    assert {row["run_id"] for row in rows} == {"trace-run"}
    assert {row["enforcement_kind"] for row in rows} == {"cgroup-v2"}
    assert rows[0]["elapsed_s"].count(".") == 1
    assert len(rows[0]["elapsed_s"].partition(".")[2]) == 6
    assert len(rows[-1]["effective_cores"].partition(".")[2]) == 4
    assert str(trace) in capsys.readouterr().err


def test_timeseries_is_off_by_default(tmp_path: Path) -> None:
    result = run_dag_limited(
        _cfg("true"),
        max_steps=1,
        max_cpus=4,
        cgroups=_TracingCgroups(),
        metrics=CsvMetricsSink(tmp_path, git_sha="deadbeef", run_id="off"),
        verbosity=0,
    )
    assert result.ok
    assert result.step_timeseries_rows == ()
    assert not (tmp_path / "traces").exists()


def test_cli_rejects_no_profile_and_unboxed_modes_before_starting_step(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    marker = tmp_path / "started"
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps":[{"group":"g","job":"j","cmd":"touch '
        + str(marker)
        + '","jobs_flag":"--jobs"}]}',
        encoding="utf-8",
    )

    assert main(
        [
            "run",
            "--dag",
            str(dag),
            "--profile-timeseries",
            "250ms",
            "--no-profile",
            "--unsafe-no-cgroups",
        ]
    ) == 2
    assert "cannot be combined with --no-profile" in capsys.readouterr().err
    assert not marker.exists()

    assert main(
        [
            "run",
            "--dag",
            str(dag),
            "--profile-timeseries",
            "250ms",
            "--unsafe-no-cgroups",
        ]
    ) == 3
    assert "requires active cgroup-v2 containment" in capsys.readouterr().err
    assert not marker.exists()

    assert main(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1",
            "--profile-timeseries",
            "250ms",
            "--unsafe-no-cgroups",
        ]
    ) == 3
    assert "requires active cgroup-v2 containment" in capsys.readouterr().err
    assert not marker.exists()


def test_timeseries_dynamic_sweep_metadata_follows_the_fixed_prefix(tmp_path: Path) -> None:
    sink = cli._SweepMetricsSink(
        CsvMetricsSink(tmp_path, git_sha="abc", run_id="metadata"),
        {"sweep_z": "last", "sweep_a": "first"},
    )
    path = sink.record_step_timeseries(
        [
            {
                "step": "g.j",
                "inner_jobs": 2,
                "sample_index": 0,
                "sample_kind": "start",
                "elapsed_s": "0.000000",
            }
        ],
        jobs=1,
    )
    assert path is not None
    with Path(path).open(newline="", encoding="utf-8") as handle:
        header = next(csv.reader(handle))
    assert tuple(header[: len(STEP_TIMESERIES_COLUMNS)]) == tuple(STEP_TIMESERIES_COLUMNS)
    assert header[len(STEP_TIMESERIES_COLUMNS) :] == ["sweep_a", "sweep_z"]
