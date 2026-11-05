"""Tests for the CLI parsing helpers, the breach-precedence classifier, and the dry plan-round
path (which needs no cgroups). Exercises requirement 4 (a clean kill that NAMES the breach)."""

from __future__ import annotations

import re
from pathlib import Path
from typing import cast

import pytest

from dagrun import StepOutcome

from parallel_experiment_runner.cli import (
    _default_slice,
    _spec_from_args,
    build_parser,
    main,
    parse_seeds,
)
from parallel_experiment_runner.execute import _classify_outcome
from parallel_experiment_runner.model import (
    STATUS_CANCELLED,
    STATUS_CPU_TIMEOUT,
    STATUS_HIT,
    STATUS_MEMORY_CAP,
    STATUS_PIDS_CAP,
    STATUS_TIMEOUT,
    ExperimentSpec,
    HitCondition,
    ResourceSlice,
    WorkerLimits,
)


def test_parse_seeds_ranges_singletons_order() -> None:
    assert parse_seeds("0-4,10,20-22") == (0, 1, 2, 3, 4, 10, 20, 21, 22)


def test_parse_seeds_single() -> None:
    assert parse_seeds("7") == (7,)


def test_parse_seeds_rejects_reversed_range() -> None:
    with pytest.raises(ValueError):
        parse_seeds("5-1")


def test_parse_seeds_rejects_empty() -> None:
    with pytest.raises(ValueError):
        parse_seeds(" , ")


def _spec(**kw: object) -> ExperimentSpec:
    base: dict[str, object] = {
        "name": "s",
        "command": ("run", "{seed}"),
        "worker_limits": WorkerLimits(cpu_cores=1, memory_bytes=4096, cpu_timeout_s=3, wall_timeout_s=60),
        "hit": HitCondition(hit_exit_codes=(0,)),
    }
    base.update(kw)
    return ExperimentSpec(**base)  # type: ignore[arg-type]


def _outcome(returncode: int = 0, *, aborted: bool = False, pids_events: int = 0) -> StepOutcome:
    return StepOutcome(
        tag="seed.3", ok=(returncode == 0 and not aborted), duration_s=1.0,
        summary="", returncode=returncode, oomed=False, oom_kills=0, timed_out=False,
        cpu_timed_out=False, aborted=aborted, pids_events=pids_events,
    )


def test_breach_cpu_timeout_named(tmp_path: Path) -> None:
    row = {"cpu_timed_out": True, "cpu.usage_usec": 3_000_000}
    res = _classify_outcome(_spec(), _outcome(returncode=1), row, tmp_path / "seed-3.log")
    assert res.status == STATUS_CPU_TIMEOUT
    assert res.cpu_s == 3.0
    assert "CPU-TIMEOUT" in res.breach and "budget 3s" in res.breach


def test_breach_memory_cap_named(tmp_path: Path) -> None:
    row = {"oom_kills": 1, "peak_bytes": 5000}
    res = _classify_outcome(_spec(), _outcome(returncode=137), row, tmp_path / "seed-3.log")
    assert res.status == STATUS_MEMORY_CAP
    assert "MEMORY-CAP" in res.breach


def test_breach_wall_timeout_named(tmp_path: Path) -> None:
    row = {"timed_out": True, "elapsed_s": 60.0}
    res = _classify_outcome(_spec(), _outcome(returncode=1), row, tmp_path / "seed-3.log")
    assert res.status == STATUS_TIMEOUT
    assert "TIMEOUT" in res.breach


def test_breach_pids_cap_named(tmp_path: Path) -> None:
    # A fork-bomb worker: StepOutcome.pids_events > 0 means forks were denied at the inner
    # pids.max (carried in-memory, deliberately NOT a CSV column).
    spec = _spec(
        worker_limits=WorkerLimits(cpu_cores=1, cpu_timeout_s=3, wall_timeout_s=60, pids_max=64)
    )
    res = _classify_outcome(
        spec, _outcome(returncode=1, pids_events=12), {}, tmp_path / "seed-3.log"
    )
    assert res.status == STATUS_PIDS_CAP
    assert res.is_breach
    assert "PIDS-CAP" in res.breach
    assert "12 fork/clone(s) denied" in res.breach and "pids.max 64" in res.breach


def test_breach_precedence_cancel_beats_cpu_timeout(tmp_path: Path) -> None:
    # An eager-cancel outranks a same-round cpu-timeout signal: a killed sibling is CANCELLED.
    row = {"cpu_timed_out": True, "cpu.usage_usec": 3_000_000}
    res = _classify_outcome(_spec(), _outcome(returncode=1, aborted=True), row, tmp_path / "s.log")
    assert res.status == STATUS_CANCELLED


def test_breach_precedence_oom_beats_pids_cap(tmp_path: Path) -> None:
    # A worker that both OOM-killed and hit its pids cap is reported by the kill-based axis (OOM),
    # which is a more definitive cause than a denied fork.
    row = {"oom_kills": 1, "peak_bytes": 5000}
    spec = _spec(
        worker_limits=WorkerLimits(cpu_cores=1, cpu_timeout_s=3, wall_timeout_s=60, pids_max=64)
    )
    res = _classify_outcome(
        spec, _outcome(returncode=137, pids_events=4), row, tmp_path / "seed-3.log"
    )
    assert res.status == STATUS_MEMORY_CAP


def test_breach_precedence_pids_cap_beats_wall_timeout(tmp_path: Path) -> None:
    # A denied fork is a more specific cause than a plain wall hang, so pids-cap outranks timeout.
    # This is the case the reason-string precedence would MASK (it reaps as TIMEOUT); the
    # in-memory pids_events lets the classifier still name the fork-bomb.
    row = {"timed_out": True, "elapsed_s": 60.0}
    spec = _spec(
        worker_limits=WorkerLimits(cpu_cores=1, cpu_timeout_s=3, wall_timeout_s=60, pids_max=64)
    )
    res = _classify_outcome(
        spec, _outcome(returncode=1, pids_events=3), row, tmp_path / "seed-3.log"
    )
    assert res.status == STATUS_PIDS_CAP


def test_clean_zero_is_hit_when_default(tmp_path: Path) -> None:
    res = _classify_outcome(_spec(), _outcome(returncode=0), {}, tmp_path / "seed-3.log")
    assert res.status == STATUS_HIT  # default hit condition is exit 0
    assert res.breach == ""


def test_plan_round_dry_runs_without_cgroups(capsys: pytest.CaptureFixture[str]) -> None:
    code = main([
        "plan-round", "--name", "demo", "--seeds", "0-3",
        "--cpu-cores", "1", "--memory", "1G", "--max-concurrency", "2",
        "--slice-cpu", "8", "--slice-memory", "16G", "--slice-disk", "100G",
        "--", "/bin/true", "{seed}",
    ])
    assert code == 0
    out = capsys.readouterr().out
    assert "resolved width:" in out
    assert "profile key:" in out


def test_leading_double_dash_is_stripped_from_command(tmp_path: Path) -> None:
    # argparse REMAINDER keeps a leading "--"; plan-round must not treat it as argv[0].
    code = main([
        "plan-round", "--seeds", "0", "--slice-cpu", "4", "--slice-memory", "8G",
        "--slice-disk", "50G", "--", "echo", "{seed}",
    ])
    assert code == 0


def _parsed_spec(*extra: str) -> ExperimentSpec:
    ns = build_parser().parse_args(
        ["run", "--name", "s", "--seeds", "0", *extra, "--", "run", "{seed}"]
    )
    spec = _spec_from_args(ns)
    assert isinstance(spec, ExperimentSpec)
    return spec


def test_pids_flag_flows_into_worker_limits() -> None:
    limits = _parsed_spec("--pids", "128", "--cpu-timeout", "10").worker_limits
    assert limits.pids_max == 128


def test_pids_flag_omitted_is_no_cap() -> None:
    assert _parsed_spec("--cpu-timeout", "10").worker_limits.pids_max is None


def test_wall_timeout_omitted_derives_from_cpu_budget() -> None:
    # No --wall-timeout + a --cpu-timeout -> the derived ~3x backstop, never a hardcoded default.
    limits = _parsed_spec("--cpu-timeout", "10").worker_limits
    assert limits.wall_timeout_s is None
    assert limits.resolved_wall_timeout_s() == 30


def test_wall_timeout_explicit_is_honoured() -> None:
    limits = _parsed_spec("--cpu-timeout", "10", "--wall-timeout", "200").worker_limits
    assert limits.wall_timeout_s == 200
    assert limits.resolved_wall_timeout_s() == 200


def test_memory_reserve_reaches_live_slice(tmp_path: Path) -> None:
    ns = build_parser().parse_args(
        [
            "plan-round",
            "--memory-reserve",
            "2G",
            "--slice-memory",
            "8G",
            "--",
            "run",
            "{seed}",
        ]
    )
    slice_ = cast(ResourceSlice, _default_slice(tmp_path, ns))
    assert slice_.memory_reserve_bytes == 2 * 1024**3


def test_detached_report_flags_reach_spec() -> None:
    spec = _parsed_spec(
        "--memory",
        "1G",
        "--cpu-timeout",
        "5",
        "--pids",
        "10",
        "--detached-resource-report",
        "/tmp/worker-{seed}.txt",
        "--detached-unit-prefix",
        "worker-",
    )
    assert spec.resource_report_path(7) == "/tmp/worker-7.txt"
    assert spec.detached_unit_prefix == "worker-"


def test_quickstart_and_version() -> None:
    assert main(["quickstart"]) == 0
    with pytest.raises(SystemExit) as exc:
        main(["--version"])
    assert exc.value.code == 0


# --- the canonical example says one thing, in three places -------------------------------------
#
# `--help`, the shipped user guide and the agent-facing skill card each print the SAME headline
# invocation, and nothing used to compare them. They drifted: a redaction sweep rewrote the target
# program as `./workload` in two of them and left `target-runner ... ./demo` in the third, so the
# tool's own canonical command had two names and two arities depending on where you read it. A
# reader who copies the wrong one gets a command this project never ran.

_REPO_ROOT = Path(__file__).resolve().parents[2]

_CANONICAL_SOURCES = {
    "cli --help": _REPO_ROOT / "py" / "parallel_experiment_runner" / "cli.py",
    "USER_GUIDE.md": _REPO_ROOT / "common" / "docs" / "parallel-experiment-runner" / "USER_GUIDE.md",
    "SKILL.md": _REPO_ROOT / "skills" / "parallel-experiment-runner" / "SKILL.md",
}


def _canonical_target(text: str) -> str:
    """The `-- <program and args>` tail of the canonical `run` example in one file.

    Anchored on `image=demo5`, which only the canonical example carries, and tolerant of the three
    layouts it appears in: a help string whose continuations are escaped backslashes, a fenced
    shell block, and a single backticked line in Markdown. `{{seed}}` is the help string's
    escaping of `{seed}`.
    """
    match = re.search(r"image=demo5[\s\\]*--\s+([^\n`]+)", text)
    assert match is not None, "the canonical `run` example is gone"
    return match.group(1).strip().replace("{{", "{").replace("}}", "}")


def test_the_canonical_run_example_is_the_same_command_everywhere() -> None:
    found = {
        where: _canonical_target(path.read_text(encoding="utf-8"))
        for where, path in _CANONICAL_SOURCES.items()
    }
    assert len(set(found.values())) == 1, f"the canonical example disagrees with itself: {found}"
    # Named literally, not read from one of the three: a test that only compared them to each other
    # would go green on three copies of the wrong command.
    assert set(found.values()) == {"./workload --chaos --seed {seed}"}
