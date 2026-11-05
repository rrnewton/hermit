"""Tests for the dagrun CLI surface (in-process, stdlib capture)."""

from __future__ import annotations

import concurrent.futures
import contextlib
import csv
import io
import json
import os
import shlex
import subprocess
import sys
import tempfile
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

import pytest

from dagrun import __version__
import dagrun.cli as cli
from dagrun.attribution import LOG_DIR_ENV
from dagrun.cli import (
    CGROUP_SETUP_ENVIRONMENT_ERROR,
    PROG,
    _load_userguide,
    main,
)
from dagrun.io import dag_from_json
from dagrun.model import DagConfig, Step
from dagrun.protocols import CgroupManager, MetricsSink
from dagrun.profile_capture import (
    CaptureConfig,
    CaptureKind,
    CaptureManifest,
    CaptureState,
    CaptureWindow,
    IsolatedTrialRequest,
    IsolatedTrialResult,
    ProfileCaptureError,
)
from dagrun.scheduler import BudgetUnit
from dagrun.sweep import CpuTopology

_DEMO = '{"steps": [{"group": "g", "job": "j", "cmd": "true", "deps": []}]}'


def test_cgroup_setup_failure_is_environmental_and_pre_node() -> None:
    assert CGROUP_SETUP_ENVIRONMENT_ERROR.startswith("ENVIRONMENT:")
    assert "no DAG node started" in CGROUP_SETUP_ENVIRONMENT_ERROR
    assert "no product build started" in CGROUP_SETUP_ENVIRONMENT_ERROR


def _capture(args: list[str]) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        rc = main(args)
    return rc, out.getvalue(), err.getvalue()


def _demo_path(tmp: str) -> str:
    path = Path(tmp) / "dag.json"
    path.write_text(_DEMO, encoding="utf-8")
    return str(path)


def _skip_if_parent_validation_is_unboxed(
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    if inherited_runner_authority_env.get("DAGRUN_DELEGATED_UNBOXED") == "1":
        pytest.skip(
            "parent validation is explicitly unboxed; no kernel subtree exists to verify"
        )


def test_no_args_prints_help() -> None:
    rc, out, _ = _capture([])
    assert rc == 0
    assert PROG in out and "quickstart" in out


def test_quickstart_is_self_contained() -> None:
    rc, out, _ = _capture(["quickstart"])
    assert rc == 0
    for marker in ("Install", "Write a DAG", "run", "DAG schema", "Exit codes"):
        assert marker in out


def _capture_help(args: list[str]) -> tuple[int, str, str]:
    """Capture a subcommand's --help, which argparse emits via SystemExit(0)."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        with pytest.raises(SystemExit) as exc:
            main(args)
    code = exc.value.code if isinstance(exc.value.code, int) else 0
    return code, out.getvalue(), err.getvalue()


def test_run_help_lists_flags_and_cores_pinning() -> None:
    """`run --help` exits 0, lists its flags, and surfaces --cores CPU pinning discoverably."""
    code, out, _ = _capture_help(["run", "--help"])
    assert code == 0
    for flag in (
        "--dag",
        "--max-steps",
        "--max-cpus",
        "--cores",
        "--selected",
        "--labels",
        "--ignore-selected-deps",
        "--planner",
        "--keep-going",
        "--allow-unwise-nest-dagruns",
    ):
        assert flag in out, flag
    assert "--jobs" not in out
    assert "--only" not in out
    # Discoverable pinning aliases + intent keywords (greppable).
    assert "--cpuset" in out and "--pin" in out
    # argparse line-wraps help, so collapse whitespace before substring checks.
    low = " ".join(out.lower().split())
    assert "pinning" in low or "cpuset" in low or "affinity" in low
    assert "opt-in" in low and "off by default" in low
    assert "every dependency they require" in low
    assert "run only the named steps" in low
    for cmdtype in (
        "unknown",
        "make",
        "cargo-build",
        "cargo-test",
        "cargo-nextest",
        "generic-dash-j-command",
        "generic-with-flag",
    ):
        assert cmdtype in out
    assert "$DAGRUN_EXTRA_ARGS" in out


def test_run_help_has_no_rust_binary_mention() -> None:
    """The Python help must not direct the reader to a Rust executable."""
    _, out, _ = _capture_help(["run", "--help"])
    low = out.lower()
    assert "rs/target" not in low
    assert "rust binary" not in low

    _, top, _ = _capture_help(["--help"])
    assert "rust" not in top.lower()
    normalized = " ".join(top.split())
    assert "sweep: --output-dir" in normalized
    assert "--perf-dir as an alias" in normalized
    assert "other commands: --perf-dir" in normalized


@pytest.mark.parametrize("command", ["run", "pin-run"])
def test_core_count_must_be_positive_usage_error(command: str) -> None:
    argv = [command, "--cores", "0"]
    if command == "run":
        argv.extend(["--dag", "missing.json"])
    else:
        argv.extend(["--", "true"])
    with pytest.raises(SystemExit) as raised:
        main(argv)
    assert raised.value.code == 2


@pytest.mark.parametrize(
    ("sub", "expected"),
    [
        ("sweep", "--step"),
        ("plan", "--planner"),
        ("list", "--dag"),
        ("ascii", "--dag"),
        ("json", "--dag"),
        ("yaml", "--dag"),
    ],
)
def test_subcommand_help_exits_zero_and_lists_flags(sub: str, expected: str) -> None:
    code, out, _ = _capture_help([sub, "--help"])
    assert code == 0
    assert expected in out


def test_sweep_help_explains_modes_widths_and_profile_artifacts() -> None:
    code, out, _ = _capture_help(["sweep", "--help"])
    normalized = " ".join(out.lower().split())

    assert code == 0
    for phrase in (
        "single-step dense mode",
        "target-time graph mode",
        "one at a time in stable topological order",
        "pass 1 always completes",
        "every started pass finishes",
        "powers of two through the physical-core count",
        "physical-core and logical-thread counts",
        "1,2,4,8,16,32,64,128,158,316",
        "integer midpoints",
        "cmdtype, jobs_flag, and/or jobs_env",
        "./.dagrun/profiles/",
        "does not redirect terminal or child-process output",
        "scaling_model_*.json",
        "profile_report.html",
        "traces/<run_id>.csv",
        "persist every sample and show the fastest wall time",
        "-vv streams child stdout/stderr",
        "separate perf-record trial",
        "not added to the scaling model",
        "acknowledged fifo control interface",
        "repeatable",
        "typically 300ms..500ms",
        "sudo -n",
        "this sweep's own uninstrumented data",
        "under captures/",
    ):
        assert phrase in normalized
    assert "--output-dir" in out
    assert "--report-html" in out
    assert "--no-report" in out
    assert "--perf-record" in out
    assert "--perf-window" in out
    assert "--wprof-window" in out
    assert "--profiler-sudo" in out
    assert "backward-compatible alias for --output-dir" in normalized
    assert "--max-cpus" not in out


def test_summary_top_help_describes_local_plan_and_single_input_merge() -> None:
    code, out, _ = _capture_help(["summary", "--help"])
    normalized = " ".join(out.lower().split())

    assert code == 0
    assert "build a plan from a summary json and dag" in normalized
    assert "merge one or more summary json files" in normalized
    assert "plan a summary sync from a backend spec" not in normalized


@pytest.mark.parametrize(
    "args",
    [
        ["summary", "build", "unexpected"],
        [
            "summary",
            "plan",
            "unexpected",
            "--summary",
            "missing-summary.json",
            "--dag",
            "missing-dag.json",
        ],
        ["summary", "stats", "one.json", "two.json"],
        ["summary", "merge"],
        ["summary", "build", "--reservoir-cap", "nope"],
        ["summary", "build", "--reservoir-cap", "1_0"],
        ["summary", "build", "--reservoir-cap", " 10"],
        ["summary", "build", "--reservoir-cap", "10 "],
        ["summary", "build", "--reservoir-cap", "١٠"],
        ["summary", "build", "--reservoir-cap", "0"],
        ["summary", "build", "--reservoir-cap", "9223372036854775808"],
        ["summary", "merge", "missing.json", "--reservoir-cap", "-1"],
        [
            "summary",
            "plan",
            "--summary",
            "missing-summary.json",
            "--dag",
            "missing-dag.json",
            "--planner",
            "unknown",
        ],
        [
            "summary",
            "plan",
            "--summary",
            "missing-summary.json",
            "--dag",
            "missing-dag.json",
            "--format",
            "xml",
        ],
        ["summary", "build", "--perf-dir"],
        ["summary", "build", "--", "--looks-like-an-option"],
        ["summary", "plan", "--summary", "--dag", "missing-dag.json"],
    ],
)
def test_summary_action_schema_rejects_invalid_invocations(args: list[str]) -> None:
    with pytest.raises(SystemExit) as raised:
        main(args)

    assert raised.value.code == 2


def test_python_quickstart_mentions_python_install_only() -> None:
    """The Python quickstart install step must not direct the reader to the Rust binary."""
    rc, out, _ = _capture(["quickstart"])
    assert rc == 0
    low = out.lower()
    assert "pip install" in low
    assert "cargo install" not in low
    assert "rs/target" not in low


def test_userguide_prints_embedded_guide() -> None:
    """`--userguide` prints the full embedded guide VERBATIM (byte-for-byte, no coloring)."""
    rc, out, _ = _capture(["--userguide"])
    assert rc == 0
    embedded = _load_userguide()
    assert out == embedded
    # A real, substantial guide (not a stub) with recognizable content.
    assert len(out) > 5000
    assert "dagrun" in out


def test_list_and_ascii() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, out, _ = _capture(["list", "--dag", dag])
        assert rc == 0 and "g.j" in out
        rc, out, _ = _capture(["ascii", "--dag", dag])
        assert rc == 0 and "layer 0:" in out and "g.j" in out


def test_dot_and_json() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, out, _ = _capture(["dot", "--dag", dag])
        assert rc == 0 and out.startswith("digraph")
        rc, out, _ = _capture(["json", "--dag", dag])
        assert rc == 0 and '"steps"' in out


# Cgroup boxing is ON by default; the in-process CLI tests use the explicit unsafe opt-out so `run`
# does NOT re-exec into a systemd scope (which would replace the pytest process). Use the
# deliberate opt-out rather than merely allowing setup failure: under a parent-owned delegated
# test step, setup succeeds by design and each in-process CLI unit test would otherwise allocate
# a real nested cgroup. The pytest process itself remains inside the outer step's containment.
# Default require-boxing and unavailable-host behavior are covered by subprocess tests instead.
_ACF = "--unsafe-no-cgroups"


def test_run_exit_codes() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        ok = Path(tmp) / "ok.json"
        ok.write_text('{"steps": [{"group": "g", "job": "j", "cmd": "true"}]}', encoding="utf-8")
        assert _capture(["run", "--dag", str(ok), "-q", _ACF])[0] == 0
        bad = Path(tmp) / "bad.json"
        bad.write_text('{"steps": [{"group": "g", "job": "j", "cmd": "false"}]}', encoding="utf-8")
        assert _capture(["run", "--dag", str(bad), "-q", _ACF])[0] == 1


@pytest.mark.parametrize("schema", [2, 3])
def test_structured_result_descriptor_lists_but_run_refuses_before_execution(
    tmp_path: Path, schema: int,
) -> None:
    marker = tmp_path / "ran"
    dag = tmp_path / "structured.json"
    dag.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "test",
                        "job": "counts",
                        "cmd": f"touch {shlex.quote(str(marker))}",
                        "result_manifests": [
                            {
                                "kind": "structured-test-results",
                                "schema": schema,
                                "path_env": "DAGRUN_TEST_COUNTS_PATH",
                                "owner": "test.counts",
                            }
                        ],
                    }
                ]
            }
        ),
        encoding="utf-8",
    )

    list_rc, listed, list_error = _capture(["list", "--dag", str(dag)])
    assert list_rc == 0, list_error
    assert "test.counts" in listed

    run_rc, _, run_error = _capture(["run", "--dag", str(dag), "-q", _ACF])
    assert run_rc == 1
    assert "REFUSING to run before any node starts" in run_error
    assert "Python runner does not implement structured test-result capture" in run_error
    assert not marker.exists()


def test_nested_run_refuses_by_outer_run_and_override_is_explicit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The validation shard itself may be an opted-in delegated child. This test constructs each
    # nested authority state explicitly while remaining physically inside that outer cgroup.
    monkeypatch.delenv("DAGRUN_DELEGATED_CGROUP", raising=False)
    monkeypatch.delenv("DAGRUN_DELEGATED_UNBOXED", raising=False)
    marker = tmp_path / "seen-outer-run"
    dag = tmp_path / "inner.json"
    dag.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": f"printf '%s' \"$DAGRUN_OUTER_RUN\" > {shlex.quote(str(marker))}",
                        # Runner policy must replace a manifest-authored value on this key.
                        "env": {"DAGRUN_OUTER_RUN": "forged"},
                    }
                ]
            }
        ),
        encoding="utf-8",
    )

    monkeypatch.setenv("DAGRUN_OUTER_RUN", "--dag outer.json")
    rc, out, err = _capture(["run", "--dag", "/does/not/need/to/exist.json"])
    assert rc == 2
    assert out == ""
    assert "refusing nested invocation" in err
    assert "--dag outer.json" in err
    assert "--allow-unwise-nest-dagruns" in err
    assert "No such file" not in err

    monkeypatch.setenv("DAGRUN_DELEGATED_UNBOXED", "1")
    monkeypatch.setenv("DAGRUN_DELEGATED_CGROUP", str(tmp_path / "forged"))
    rc, _, err = _capture(["run", "--dag", "/does/not/need/to/exist.json"])
    assert rc == 2
    assert "was present but invalid" in err
    assert "delegated cgroup path" in err
    monkeypatch.delenv("DAGRUN_DELEGATED_CGROUP")

    monkeypatch.setenv("DAGRUN_DELEGATED_UNBOXED", "not-authorized")
    rc, _, err = _capture(["run", "--dag", "/does/not/need/to/exist.json"])
    assert rc == 2
    assert "DAGRUN_DELEGATED_UNBOXED" in err
    assert "exactly '1'" in err

    monkeypatch.setenv("DAGRUN_DELEGATED_UNBOXED", "1")
    rc, _, err = _capture(
        ["run", "--dag", str(dag), "--no-profile", "--no-profile-feedback", "-q"]
    )
    assert rc == 0, err
    assert "reviewed uncontained nested execution" in err
    assert marker.read_text(encoding="utf-8") == "g.j"
    marker.unlink()
    monkeypatch.delenv("DAGRUN_DELEGATED_UNBOXED")

    rc, _, err = _capture(
        [
            "run",
            "--dag",
            str(dag),
            "--allow-unwise-nest-dagruns",
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ]
    )
    assert rc == 0, err
    assert marker.read_text(encoding="utf-8") == "g.j"

    marker.unlink()
    monkeypatch.delenv("DAGRUN_OUTER_RUN")
    rc, _, err = _capture(
        [
            "run",
            "--dag",
            str(dag),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ]
    )
    assert rc == 0, err
    assert marker.read_text(encoding="utf-8") == "g.j"


def test_delegated_nested_run_keeps_descendants_in_outer_owned_subtree(
    tmp_path: Path,
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    import os
    import time

    _skip_if_parent_validation_is_unboxed(inherited_runner_authority_env)

    pid_marker = tmp_path / "descendant.pid"
    cgroup_marker = tmp_path / "descendant.cgroup"
    coordinator_cgroup_marker = tmp_path / "coordinator.cgroup"
    evidence_dir = tmp_path / "evidence"
    # The budget starts before a second Python runner has imported and established its nested
    # cgroups. Three seconds raced that setup under whole-repository load and could tear the run
    # down before the worker wrote the very markers this test needs to inspect. Ten seconds is
    # still a tight timeout smoke, while leaving startup outside the condition under test.
    outer_timeout_s = 10
    inner = tmp_path / "inner.json"
    inner.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "inner",
                        "job": "worker",
                        "cmd": (
                            f"cat /proc/$PPID/cgroup > {coordinator_cgroup_marker} && "
                            f"cat /proc/self/cgroup > {cgroup_marker} && "
                            f"exec setsid --wait sh -c 'echo $$ > {pid_marker}; exec sleep 60'"
                        ),
                        "timeout": 60,
                    }
                ]
            }
        )
    )
    outer = tmp_path / "outer.json"
    outer.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "outer",
                        "job": "delegating",
                        "cmd": shlex.join(
                            [
                                sys.executable,
                                "-m",
                                "dagrun",
                                "run",
                                "--dag",
                                str(inner),
                                "--max-cpus",
                                "1",
                                "--no-profile",
                                "--no-profile-feedback",
                                "-q",
                            ]
                        ),
                        "delegated_children": True,
                        "timeout": outer_timeout_s,
                        "cpu_timeout": 60,
                        # Keep the smoke's own envelope small enough to nest below the canonical
                        # validation shard; inherited admission clamps the modeling floor.
                        "hint": {"hard_mem_max_bytes": 1024**3},
                    }
                ]
            }
        )
    )
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "dagrun",
            "run",
            "--dag",
            str(outer),
            "--max-cpus",
            "1",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ],
        cwd=Path(__file__).parents[1],
        env={
            **os.environ,
            **inherited_runner_authority_env,
            # Assert the durable timeout event, not a console line emitted later in teardown.
            # Under load the process snapshot can be visible even when that terminal rendering
            # loses its race with nested shutdown; the journal record is flushed before reaping.
            LOG_DIR_ENV: str(evidence_dir),
        },
        capture_output=True,
        text=True,
        timeout=30,
    )
    output = result.stdout + result.stderr
    if result.returncode == 3 and (
        "cgroup boxing could not be established" in output
        or "cgroup setup failed" in output
        or "cgroup boxing was NOT ESTABLISHED" in output
    ):
        pytest.skip(f"cgroup delegation unavailable: {output}")
    assert result.returncode == 1, output
    journal = evidence_dir / "journal.jsonl"
    assert journal.exists(), output
    records = [
        json.loads(line)
        for line in journal.read_text().splitlines()
        if line.strip()
    ]
    breaches = [
        record
        for record in records
        if record.get("event") == "step_timeout"
        and record.get("step") == "outer.delegating"
    ]
    assert len(breaches) == 1, f"expected one outer wall-timeout record, got {breaches}\n{output}"
    breach = breaches[0]
    assert breach["unit"] == BudgetUnit.WALL_SECONDS.value
    assert breach["limit_s"] == str(outer_timeout_s)
    assert float(breach["measured_s"]) >= outer_timeout_s
    assert float(breach["measured_s"]) == float(breach["wall_elapsed_s"])
    assert cgroup_marker.exists(), output
    cgroup_path = cgroup_marker.read_text()
    assert "step-outer.delegating/nested-run-" in cgroup_path
    assert "/step-inner.worker" in cgroup_path
    assert coordinator_cgroup_marker.exists(), output
    coordinator_cgroup_path = coordinator_cgroup_marker.read_text()
    assert "step-outer.delegating/nested-run-" in coordinator_cgroup_path
    assert coordinator_cgroup_path.rstrip().endswith("/supervisor")
    assert pid_marker.exists(), output
    pid = int(pid_marker.read_text().strip())
    deadline = time.monotonic() + 5
    while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
        time.sleep(0.02)
    assert not Path(f"/proc/{pid}").exists(), (
        f"outer timeout left delegated setsid descendant {pid} alive\n{output}"
    )


def test_delegated_process_can_run_twice_in_process_after_aggregate_migration(
    tmp_path: Path,
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    _skip_if_parent_validation_is_unboxed(inherited_runner_authority_env)

    coordinator_cgroups = tmp_path / "coordinator-cgroups"
    returned_cgroups = tmp_path / "returned-cgroups"
    inner = tmp_path / "inner.json"
    inner.write_text(
        json.dumps(
            {
                "mem_cap_floor_bytes": 268435456,
                "steps": [
                    {
                        "group": "inner",
                        "job": "worker",
                        "cmd": f"cat /proc/$PPID/cgroup >> {coordinator_cgroups}",
                        "hint": {"hard_mem_max_bytes": 268435456},
                    }
                ],
            }
        )
    )
    driver = tmp_path / "twice.py"
    inner_args = [
        "run",
        "--dag",
        str(inner),
        "--max-cpus",
        "1",
        "--max-mem",
        "1G",
        "--no-profile",
        "--no-profile-feedback",
        "-q",
    ]
    driver.write_text(
        "import sys\n"
        f"sys.path.insert(0, {str(Path(__file__).parents[1])!r})\n"
        "from dagrun.cli import main\n"
        f"args = {inner_args!r}\n"
        "first = main(args)\n"
        f"open({str(returned_cgroups)!r}, 'a').write(open('/proc/self/cgroup').read())\n"
        "second = main(args)\n"
        f"open({str(returned_cgroups)!r}, 'a').write(open('/proc/self/cgroup').read())\n"
        "raise SystemExit(first or second)\n"
    )
    outer = tmp_path / "outer.json"
    outer.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "outer",
                        "job": "delegating",
                        "cmd": shlex.join([sys.executable, str(driver)]),
                        "delegated_children": True,
                        "hint": {
                            "preferred_inner_jobs": 2,
                            "hard_mem_max_bytes": 2 * 1024**3,
                        },
                    }
                ]
            }
        )
    )

    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "dagrun",
            "run",
            "--dag",
            str(outer),
            "--max-cpus",
            "2",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ],
        cwd=Path(__file__).parents[1],
        env={**os.environ, **inherited_runner_authority_env},
        capture_output=True,
        text=True,
        timeout=30,
    )
    output = result.stdout + result.stderr
    if result.returncode == 3 and (
        "cgroup boxing could not be established" in output
        or "cgroup setup failed" in output
        or "cgroup boxing was NOT ESTABLISHED" in output
    ):
        pytest.skip(f"cgroup delegation unavailable: {output}")
    assert result.returncode == 0, output
    recorded = coordinator_cgroups.read_text().splitlines()
    assert len(recorded) == 2
    assert all("step-outer.delegating/nested-run-" in line for line in recorded)
    assert all(line.endswith("/supervisor") for line in recorded)
    assert recorded[0] != recorded[1]
    returned = returned_cgroups.read_text().splitlines()
    assert len(returned) == 2
    assert all("step-outer.delegating/supervisor" in line for line in returned)


def test_selected_run_memory_sizing_excludes_omitted_steps_but_keeps_dependencies(
    tmp_path: Path,
) -> None:
    marker = tmp_path / "selected-ran"
    label_marker = tmp_path / "label-selected-ran"
    dag = tmp_path / "selected-memory.json"
    dag.write_text(
        json.dumps(
            {
                "mem_cap_floor_bytes": 8 * 1024**3,
                "steps": [
                    {
                        "group": "g",
                        "job": "large-dependency",
                        "cmd": "true",
                        "labels": ["all"],
                        "hint": {"hard_mem_max_bytes": 8 * 1024**3},
                    },
                    {
                        "group": "g",
                        "job": "small",
                        "cmd": f"touch {shlex.quote(str(marker))}",
                        "deps": ["g.large-dependency"],
                        "labels": ["all"],
                        "hint": {"rss_baseline_bytes": 256 * 1024**2},
                    },
                    {
                        "group": "g",
                        "job": "label-small",
                        "cmd": f"touch {shlex.quote(str(label_marker))}",
                        "labels": ["all", "small-only"],
                        "hint": {"rss_baseline_bytes": 256 * 1024**2},
                    },
                ],
            }
        ),
        encoding="utf-8",
    )
    common = [
        "run",
        "--dag",
        str(dag),
        "--selected",
        "g.small",
        "--max-cpus",
        "2",
        "--max-mem",
        "1G",
        "--unsafe-no-cgroups",
        "--no-profile",
        "--no-profile-feedback",
        "-q",
    ]

    rc, _, err = _capture([*common, "--ignore-selected-deps"])
    assert rc == 0, err
    assert marker.exists()

    marker.unlink()
    rc, _, err = _capture(common)
    assert rc == 2
    assert not marker.exists()
    assert "minimum runnable footprint 8589934592" in err

    all_tags = [*common, "--ignore-selected-deps"]
    all_tags[4] = "g.large-dependency,g.small,g.label-small"
    rc, _, err = _capture(all_tags)
    assert rc == 2
    assert not marker.exists()
    assert not label_marker.exists()
    assert "minimum runnable footprint 8589934592" in err

    all_labels = [
        "run",
        "--dag",
        str(dag),
        "--labels",
        "all",
        "--max-cpus",
        "2",
        "--max-mem",
        "1G",
        "--unsafe-no-cgroups",
        "--no-profile",
        "--no-profile-feedback",
        "-q",
    ]
    rc, _, err = _capture(all_labels)
    assert rc == 2
    assert not marker.exists()
    assert not label_marker.exists()
    assert "minimum runnable footprint 8589934592" in err

    partial_label = [*all_labels]
    partial_label[4] = "small-only"
    rc, _, err = _capture(partial_label)
    assert rc == 0, err
    assert label_marker.exists()


def test_allow_cgroup_failure_outer_delegates_a_named_unboxed_nested_run(
    tmp_path: Path,
) -> None:
    """Hosted CI may be unboxed, but an explicit graph delegation still reaches its child."""
    marker = tmp_path / "nested-ran"
    inner = tmp_path / "inner.json"
    inner.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "inner",
                        "job": "worker",
                        "cmd": f"touch {shlex.quote(str(marker))}",
                    }
                ]
            }
        )
    )
    outer = tmp_path / "outer.json"
    outer.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "outer",
                        "job": "delegating",
                        "cmd": shlex.join(
                            [
                                sys.executable,
                                "-m",
                                "dagrun",
                                "run",
                                "--dag",
                                str(inner),
                                "--no-profile",
                                "--no-profile-feedback",
                                "-q",
                            ]
                        ),
                        "delegated_children": True,
                    }
                ]
            }
        )
    )
    env = dict(os.environ, CI="1")
    for inherited in (
        "DAGRUN_DELEGATED_CGROUP",
        "DAGRUN_DELEGATED_UNBOXED",
        "DAGRUN_IN_SCOPE",
        "DAGRUN_OUTER_RUN",
    ):
        env.pop(inherited, None)
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "dagrun",
            "run",
            "--dag",
            str(outer),
            "--allow-cgroup-failure",
            "--no-profile",
            "--no-profile-feedback",
            "-vv",
        ],
        env=env,
        capture_output=True,
        text=True,
        timeout=20,
    )
    output = result.stdout + result.stderr
    assert result.returncode == 0, output
    assert marker.exists()
    assert "reviewed uncontained nested execution" in output


def test_missing_and_malformed_dag_exit_2() -> None:
    assert _capture(["run", "--dag", "/nonexistent/nope.json", "-q"])[0] == 2
    with tempfile.TemporaryDirectory() as tmp:
        junk = Path(tmp) / "junk.json"
        junk.write_text("not json", encoding="utf-8")
        assert _capture(["list", "--dag", str(junk)])[0] == 2


def test_wrong_document_error_names_the_path_contents_and_next_action(tmp_path: Path) -> None:
    path = tmp_path / "not-a-dag.yaml"
    path.write_text("schema: 2\nbucket: example\ntest: []\n", encoding="utf-8")

    rc, out, err = _capture(["list", "--dag", str(path)])

    assert rc == 2
    assert out == ""
    assert err == (
        f"dagrun: {path}: expected a dagrun DAG document with a top-level 'steps' list; "
        "found no 'steps' key (top-level keys: 'bucket', 'schema', 'test'). This may be a "
        "different document type. Pass a dagrun DAG file, or run `dagrun quickstart` for the "
        "schema.\n"
    )


def test_run_max_mem_exits_0() -> None:
    # --max-mem picks a memory-aware active-step ceiling; a passing DAG still exits 0.
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, _, err = _capture(["run", "--max-mem", "8G", "--dag", dag, "-q", _ACF])
        assert rc == 0
        assert "--max-mem 8G" in err  # the sizing decision is surfaced


def test_run_max_mem_counts_the_undeclared_step_default() -> None:
    # A bare runnable step still receives the positive default memory cap, so --max-mem models
    # that real enforced cap rather than pretending the footprint collapsed to zero/the floor.
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        dag.write_text(
            '{"mem_cap_floor_bytes": 0, "steps": '
            '[{"group": "g", "job": "j", "cmd": "true"}]}',
            encoding="utf-8",
        )
        rc, _, err = _capture(
            ["run", "--max-mem", "16G", "--dag", str(dag), "-q", _ACF]
        )
        assert rc == 0
        assert "worst-case 1073741824 bytes" in err
        assert "no runnable step has a positive" not in err


def test_run_max_mem_with_baseline_no_note() -> None:
    # A DAG whose step carries an rss_baseline_bytes participates in the memory model, so the
    # no-throttle note is NOT emitted even when the chosen -j lands at the CPU count.
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        dag.write_text(
            '{"steps": [{"group": "g", "job": "j", "cmd": "true",'
            ' "hint": {"rss_baseline_bytes": 1073741824}}]}',
            encoding="utf-8",
        )
        rc, _, err = _capture(["run", "--max-mem", "64G", "--dag", str(dag), "-q", _ACF])
        assert rc == 0
        assert "no runnable step has a positive" not in err


def test_run_max_cpus_and_max_mem_control_independent_limits() -> None:
    # --max-cpus sets total CPU capacity while --max-mem independently derives max active steps.
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, _, err = _capture(
            ["run", "--max-cpus", "2", "--max-mem", "8G", "--dag", dag, "-q", _ACF]
        )
        assert rc == 0
        assert "--max-mem 8G -> modeled memory ceiling" in err
        assert "base active-step ceiling 2; final --max-steps 2" in err


def test_run_perf_dir_writes_csv() -> None:
    # --perf-dir writes per-step + whole-run CSVs (the CsvMetricsSink path).
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        perf = Path(tmp) / "perf"
        rc, _, err = _capture(["run", "--perf-dir", str(perf), "--dag", dag, "-q", _ACF])
        assert rc == 0
        csvs = list(perf.glob("*.csv"))
        assert csvs, "expected at least one perf CSV to be written"
        assert any(p.stat().st_size > 0 for p in csvs)
        assert "perf CSVs written under" in err
        # Lock coordination stays in the hidden .locks directory, not beside data files.
        assert not list(perf.glob("*.lock"))


def test_run_selected_runs_full_dependency_ancestry_in_order() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        order = Path(tmp) / "order"
        path.write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "package",
                            "job": "tarball",
                            "cmd": f"printf 'package\\n' >> {order}",
                            "deps": ["test.unit"],
                        },
                        {
                            "group": "test",
                            "job": "unit",
                            "cmd": f"printf 'test\\n' >> {order}",
                            "deps": ["build.app"],
                        },
                        {
                            "group": "build",
                            "job": "app",
                            "cmd": f"printf 'build\\n' >> {order}",
                        },
                    ]
                }
            ),
            encoding="utf-8",
        )
        rc, _, err = _capture(
            ["run", "--dag", str(path), "--selected", "package.tarball", "-q", _ACF]
        )
        assert rc == 0
        assert order.read_text(encoding="utf-8") == "build\ntest\npackage\n"
        assert (
            "3 passed, 0 failed, 0 aborted, 0 intentionally skipped, "
            "0 dependency-skipped, 0 not launched"
        ) in err


def test_run_selected_multiple_steps() -> None:
    # A comma-separated selection runs those steps and any dependencies they require.
    dag = (
        '{"steps": ['
        '{"group": "a", "job": "x", "cmd": "true"},'
        '{"group": "b", "job": "y", "cmd": "true"},'
        '{"group": "c", "job": "z", "cmd": "true"}'
        "]}"
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        rc, _, err = _capture(
            ["run", "--dag", str(path), "--selected", "a.x,c.z", "-q", _ACF]
        )
        assert rc == 0
        assert (
            "2 passed, 0 failed, 0 aborted, 0 intentionally skipped, "
            "0 dependency-skipped, 0 not launched"
        ) in err


def test_run_labels_select_a_union_and_include_dependencies() -> None:
    dag = (
        '{"steps": ['
        '{"group":"build","job":"app","cmd":"true"},'
        '{"group":"test","job":"unit","cmd":"true","labels":["quick","full"],"deps":["build.app"]},'
        '{"group":"test","job":"slow","cmd":"true","labels":["full"]},'
        '{"group":"privileged","job":"host","cmd":"true","labels":["super"]}'
        "]}"
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        rc, _, err = _capture(["run", "--dag", str(path), "--labels", "quick,super", "-q", _ACF])
        assert rc == 0
        assert (
            "3 passed, 0 failed, 0 aborted, 0 intentionally skipped, "
            "0 dependency-skipped, 0 not launched"
        ) in err

        rc, _, err = _capture(["run", "--dag", str(path), "--labels", "test.unit", "-q", _ACF])
        assert rc == 2
        assert "unknown label(s): test.unit" in err
        assert "Known labels: full, quick, super" in err


def test_run_selected_and_labels_are_mutually_exclusive() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(
            '{"steps":[{"group":"g","job":"j","cmd":"true","labels":["quick"]}]}',
            encoding="utf-8",
        )
        rc, _, err = _capture(
            ["run", "--dag", str(path), "--selected", "g.j", "--labels", "quick", "-q", _ACF]
        )
        assert rc == 2
        assert "--selected and --labels cannot be combined" in err


def test_keep_going_launches_later_independent_work_and_reports_full_accounting() -> None:
    dag = (
        '{"steps": ['
        '{"group":"g","job":"fail","cmd":"exit 1","hint":{"est_duration_s":100}},'
        '{"group":"g","job":"dependent","cmd":"true","deps":["g.fail"],"hint":{"est_duration_s":90}},'
        '{"group":"g","job":"independent","cmd":"true","hint":{"est_duration_s":80}}'
        "]}"
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")

        rc, _, err = _capture(["run", "--dag", str(path), "-q", "-s", "1", _ACF])
        assert rc == 1
        assert "0 passed, 1 failed, 0 aborted" in err
        assert "1 dependency-skipped, 1 not launched" in err
        assert "not launched: g.independent" in err

        rc, _, err = _capture(
            ["run", "--dag", str(path), "-q", "-s", "1", "--keep-going", _ACF]
        )
        assert rc == 1
        assert "1 passed, 1 failed, 0 aborted" in err
        assert "1 dependency-skipped, 0 not launched" in err
        assert "not launched:" not in err


def test_run_selected_can_ignore_dependencies() -> None:
    dag = (
        '{"steps": ['
        '{"group": "build", "job": "app", "cmd": "true"},'
        '{"group": "test", "job": "unit", "cmd": "true", "deps": ["build.app"]}'
        "]}"
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        rc, _, err = _capture(
            [
                "run",
                "--dag",
                str(path),
                "--selected",
                "test.unit",
                "--ignore-selected-deps",
                "-q",
                _ACF,
            ]
        )
        assert rc == 0
        assert (
            "1 passed, 0 failed, 0 aborted, 0 intentionally skipped, "
            "0 dependency-skipped, 0 not launched"
        ) in err


def test_ignore_selected_deps_requires_selected() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, _, err = _capture(["run", "--dag", dag, "--ignore-selected-deps", "-q", _ACF])
        assert rc == 2
        assert "--ignore-selected-deps requires --selected" in err


def test_run_selected_unknown_tag_exits_2() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, _, err = _capture(["run", "--dag", dag, "--selected", "no.pe", "-q", _ACF])
        assert rc == 2
        assert "unknown step tag" in err


def test_retired_only_flag_and_boolean_value_are_rejected() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        with pytest.raises(SystemExit) as only_error:
            main(["run", "--dag", dag, "--only", "test.unit"])
        assert only_error.value.code == 2
        with pytest.raises(SystemExit) as boolean_value_error:
            main(
                [
                    "run",
                    "--dag",
                    dag,
                    "--selected",
                    "test.unit",
                    "--ignore-selected-deps=true",
                ]
            )
        assert boolean_value_error.value.code == 2


def test_run_profile_prints_table() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, out, _ = _capture(["run", "--dag", dag, "--profile", "-q", _ACF])
        assert rc == 0
        assert "per-step profile:" in out
        for column in ("step", "wall_s", "user_s", "sys_s", "rss_hwm", "oom", "inner_jobs"):
            assert column in out


def test_run_default_profile_store(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # With NEITHER --perf-dir NOR $DAGRUN_PROFILE_DIR set, a run auto-logs to the repo-local
    # default ./.dagrun/profiles/ (relative to CWD) and says where.
    monkeypatch.delenv("DAGRUN_PROFILE_DIR", raising=False)
    monkeypatch.chdir(tmp_path)
    dag = _demo_path(str(tmp_path))
    rc, _, err = _capture(["run", "--dag", dag, "-q", _ACF])
    assert rc == 0
    store = tmp_path / ".dagrun" / "profiles"
    csvs = list(store.glob("*.csv"))
    assert csvs, "expected the default profile store to be created and written"
    assert "default profile store" in err


def test_run_no_profile_writes_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("DAGRUN_PROFILE_DIR", raising=False)
    monkeypatch.chdir(tmp_path)
    dag = _demo_path(str(tmp_path))
    rc, _, err = _capture(["run", "--dag", dag, "--no-profile", "-q", _ACF])
    assert rc == 0
    assert not (tmp_path / ".dagrun").exists()
    assert "profile data appended" not in err


def test_sweep_prints_table_and_writes_store() -> None:
    # sweep runs the ONE step at inner -j1..-j2 and prints a speedup table; each run auto-appends
    # to the profile store (redirected to a temp dir by the autouse conftest fixture).
    dag = '{"steps": [{"group": "g", "job": "j", "cmd": "true"}]}'
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        store = Path(tmp) / "perf"
        rc, out, err = _capture(
            ["sweep", "--dag", str(path), "--step", "g.j", "--jobs", "1..2",
             "--perf-dir", str(store), _ACF]
        )
        assert rc == 0, err
        assert "parallel-speedup sweep: g.j" in out
        for column in ("jobs", "wall_s", "user_s", "sys_s", "rss_hwm", "speedup(vs j1)"):
            assert column in out
        profile = next(store.glob("step_profiles_*.csv"))
        with profile.open(newline="", encoding="utf-8") as source:
            rows = list(csv.DictReader(source))
        assert rows
        assert {row["enforcement_kind"] for row in rows} == {"unboxed"}
        assert {row["runner_name"] for row in rows} == {"sweep"}


def test_sweep_sink_labels_a_present_cgroup_manager_as_cgroup_v2(tmp_path: Path) -> None:
    # The sink only needs manager presence to describe how the enclosing scheduler run is boxed;
    # it does not call the manager itself.
    manager = cast(CgroupManager, object())

    sink = cli._sweep_sink(str(tmp_path), "deadbee", {}, manager)

    assert sink is not None
    assert getattr(sink, "enforcement_kind") == "cgroup-v2"
    assert getattr(sink, "runner_name") == "sweep"


def test_sweep_unknown_step_exits_2() -> None:
    dag = '{"steps": [{"group": "g", "job": "j", "cmd": "true"}]}'
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        rc, _, err = _capture(
            ["sweep", "--dag", str(path), "--step", "no.pe", "--jobs", "1..2", _ACF]
        )
        assert rc == 2
        assert "unknown --step tag" in err


def test_sweep_bad_range_exits_2() -> None:
    dag = '{"steps": [{"group": "g", "job": "j", "cmd": "true"}]}'
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        rc, _, err = _capture(
            ["sweep", "--dag", str(path), "--step", "g.j", "--jobs", "5..2", _ACF]
        )
        assert rc == 2
        assert "invalid --jobs range" in err


@pytest.mark.parametrize(
    "extra",
    (
        ["--step", "g.j", "--jobs", "abc"],
        ["--target-time", "0", "--jobs", "1,,2"],
    ),
)
def test_sweep_usage_errors_precede_cgroup_and_profile_setup(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, extra: list[str]
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )

    def unexpected(*_args: object, **_kwargs: object) -> object:
        raise AssertionError("usage validation must run before environment/profile setup")

    monkeypatch.setattr(cli, "_resolve_cgroup_manager", unexpected)
    monkeypatch.setattr(cli, "_resolve_profile_dir", unexpected)

    rc, _, err = _capture(["sweep", "--dag", str(dag), *extra])

    assert rc == 2
    assert "invalid --jobs" in err


@pytest.mark.parametrize(
    ("extra", "message"),
    (
        (["--no-profile", "--report-html", "report.html"], "cannot be combined with --no-profile"),
        (["--no-report", "--report-html", "report.html"], "cannot be combined with --no-report"),
        (["--output-dir", "one", "--perf-dir", "two"], "--output-dir and --perf-dir disagree"),
    ),
)
def test_sweep_report_output_conflicts_are_usage_errors_before_setup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    extra: list[str],
    message: str,
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )

    def unexpected(*_args: object, **_kwargs: object) -> object:
        raise AssertionError("report/output validation must precede cgroup setup")

    monkeypatch.setattr(cli, "_resolve_cgroup_manager", unexpected)
    rc, _, err = _capture(
        ["sweep", "--dag", str(dag), "--step", "g.j", "--jobs", "1", *extra]
    )

    assert rc == 2
    assert message in err


@pytest.mark.parametrize(
    ("extra", "message"),
    (
        (
            ["--wprof-window", "300ms", "--wprof-window", "400ms"],
            "durations must match",
        ),
        (["--perf-window", "0"], "duration must be greater than zero"),
        (["--profiler-sudo"], "requires --perf-record"),
        (
            ["--perf-record", "--no-profile"],
            "profiler capture options cannot be combined with --no-profile",
        ),
    ),
)
def test_sweep_profiler_usage_errors_precede_cgroup_setup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    extra: list[str],
    message: str,
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )

    def unexpected(*_args: object, **_kwargs: object) -> object:
        raise AssertionError("profiler validation must precede cgroup setup")

    monkeypatch.setattr(cli, "_resolve_cgroup_manager", unexpected)
    rc, _, err = _capture(
        ["sweep", "--dag", str(dag), "--step", "g.j", "--jobs", "1", *extra]
    )

    assert rc == 2
    assert message in err


def test_repeated_equivalent_wprof_windows_request_separate_trials() -> None:
    ns = cli.build_parser().parse_args(
        [
            "sweep",
            "--dag",
            "dag.json",
            "--step",
            "g.j",
            "--jobs",
            "1..2",
            "--wprof-window",
            "400ms",
            "--wprof-window",
            "0.4s",
        ]
    )

    request = cli._parse_profiler_request(ns)

    assert request.wprof_windows == 2
    assert request.wprof_window_s == pytest.approx(0.4)
    assert not request.capture_perf


def test_legacy_sweep_still_requires_step_and_jobs() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        rc, _, err = _capture(["sweep", "--dag", dag, "--no-profile"])
    assert rc == 2
    assert "without --target-time, --step and --jobs are required" in err


def test_runtime_sweep_topology_falls_back_to_logical_when_physical_is_unknown(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        cli,
        "detect_cpu_topology",
        lambda: CpuTopology(tuple(range(6)), physical_core_count=None),
    )
    monkeypatch.setattr(cli, "container_core_budget", lambda: 4)

    topology = cli._effective_sweep_topology()

    assert topology.logical_thread_count == 4
    assert topology.physical_core_count == 4


def test_target_sweep_completes_pass_one_in_topological_order(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A zero target still runs the entire mandatory first pass, one node at a time."""
    dag = tmp_path / "dag.json"
    dag.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "leaf",
                        "cmd": "true",
                        "deps": ["g.root"],
                        "jobs_flag": "--workers=",
                    },
                    {
                        "group": "g",
                        "job": "root",
                        "cmd": "true",
                        "jobs_flag": "--workers=",
                    },
                ]
            }
        ),
        encoding="utf-8",
    )
    calls: list[tuple[str, int, bool]] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del cfg, cgroups, metrics, verbosity, profile_timeseries_interval_s
        calls.append((step.tag, inner_jobs, vary_width))
        return cli._SweepMeasure(1.0, 1.0, 0.0, 1024, True)

    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda: CpuTopology(tuple(range(4)), physical_core_count=2),
    )
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))

    rc, out, err = _capture(
        ["sweep", "--dag", str(dag), "--target-time", "0", "--no-profile"]
    )

    assert rc == 0
    assert calls == [
        ("g.root", 1, True),
        ("g.root", 2, True),
        ("g.root", 4, True),
        ("g.leaf", 1, True),
        ("g.leaf", 2, True),
        ("g.leaf", 4, True),
    ]
    assert "parallel-scaling sweep: 2 node(s), target 0.000s" in out
    assert "sweep pass 1 starting: cumulative widths [1,2,4] (mandatory minimum pass)" in out
    assert "target-time sweep complete: 1 pass(es), 6 sample(s)" in out
    assert "overrun " in out
    assert err == ""


def test_target_sweep_starts_a_second_cumulative_pass_and_finishes_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The target is checked between passes; a started refinement pass is atomic."""
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"cmdtype": "generic-with-flag", "jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    calls: list[int] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del step, cfg, cgroups, metrics, verbosity, vary_width, profile_timeseries_interval_s
        calls.append(inner_jobs)
        return cli._SweepMeasure(1.0, 1.0, 0.0, None, True)

    ticks = iter((0.0, 0.0, 0.5, 0.5, 0.5, 2.0, 2.0, 2.0))
    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr("dagrun.cli.time.monotonic", lambda: next(ticks))
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda: CpuTopology(tuple(range(4)), physical_core_count=2),
    )
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))

    rc, out, err = _capture(
        ["sweep", "--dag", str(dag), "--target-time", "1", "--no-profile"]
    )

    assert rc == 0
    assert calls == [1, 2, 4, 1, 2, 3, 4]
    assert "sweep pass 2 starting: cumulative widths [1,2,3,4]" in out
    assert "target-time sweep complete: 2 pass(es), 7 sample(s)" in out
    assert "overrun 1.000s" in out
    assert err == ""


def test_legacy_profiler_capture_uses_current_sweep_sweet_spot_and_no_metrics(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "echo build", '
        '"cmdtype": "generic-with-flag", "jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    ordinary_widths: list[int] = []
    instrumented: list[tuple[int, str, MetricsSink | None, float | None]] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del cfg, cgroups, verbosity, vary_width
        if metrics is None:
            instrumented.append(
                (inner_jobs, step.cmd, metrics, profile_timeseries_interval_s)
            )
        else:
            ordinary_widths.append(inner_jobs)
        walls = {1: 10.0, 2: 5.0, 3: 4.8}
        return cli._SweepMeasure(
            walls[inner_jobs], 9.0, 1.0, 1024, True, returncode=0
        )

    def fake_capture(
        selection: object,
        config: CaptureConfig,
        run_trial: object,
    ) -> CaptureManifest:
        from dagrun.profile_capture import RunIsolatedTrial, SweetSpotSelection

        assert isinstance(selection, SweetSpotSelection)
        assert selection.inner_jobs == 2
        assert selection.expected_wall_s == pytest.approx(5.0)
        assert selection.source == "scaling-model-economic-plateau"
        assert config.capture_perf
        assert config.perf_window_s == pytest.approx(0.02)
        assert config.sudo == ("sudo", "-n")
        callback = cast(RunIsolatedTrial, run_trial)
        result = callback(
            IsolatedTrialRequest(
                trial_id="perf-001",
                step="g.j",
                inner_jobs=2,
                kind=CaptureKind.PERF,
                output_dir=tmp_path,
                expected_wall_s=5.0,
                window=CaptureWindow(0.02, 2.49, 0.02, False),
                argv_prefix=("sudo", "-n", "fake-perf", "record", "--"),
            )
        )
        assert isinstance(result, IsolatedTrialResult)
        assert result.ok
        return CaptureManifest(
            path=tmp_path / "manifest.json",
            capture_id="capture-test",
            state=CaptureState.COMPLETE,
            created_at="2026-01-01T00:00:00Z",
            finished_at="2026-01-01T00:00:01Z",
            selection=selection,
            preflight=(),
            trials=(),
            errors=(),
        )

    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr(cli, "capture_at_sweet_spot", fake_capture)
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))
    monkeypatch.setattr(cli, "_report_profile_written", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_scaling_model", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_profile_report", lambda *_args: None)

    rc, out, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1..3",
            "--output-dir",
            str(tmp_path / "profiles"),
            "--perf-window",
            "20ms",
            "--profiler-sudo",
        ]
    )

    assert rc == 0, err
    assert ordinary_widths == [1, 2, 3]
    assert len(instrumented) == 1
    width, command, metrics, trace_interval = instrumented[0]
    assert width == 2
    assert metrics is None
    assert trace_interval is None
    assert "sudo -n fake-perf record --" in command
    assert "guest-launch-ready.fifo" in command
    assert "guest-launch-release.fifo" in command
    assert command.index("fake-perf record --") < command.index("guest-launch-ready.fifo")
    assert "echo build --workers=2" in command
    assert "sweet-spot width 2" in out
    assert f"profiler capture manifest: {tmp_path / 'manifest.json'}" in out


def test_single_step_measure_preserves_exact_child_returncode() -> None:
    step = Step("g", "status", "", "exit 37")
    measure = cli._run_single_step(
        step,
        DagConfig(steps=(step,)),
        1,
        None,
        None,
        0,
        vary_width=False,
    )

    assert not measure.ok
    assert measure.returncode == 37


@pytest.mark.parametrize(("channel", "width", "refused", "observe_startup"), (
    ("CARGO_BUILD_JOBS", 1, False, False),
    ("CARGO_BUILD_JOBS", 4, False, False),
    ("LINENO", 1, True, False),
    ("IFS", 1, True, False),
    ("WORKER_JOBS", 2, False, True),
))
def test_jobs_env_profiled_guest_boundary(
    tmp_path: Path, channel: str, width: int, refused: bool, observe_startup: bool,
) -> None:
    from dagrun.model import ResourceHint

    profiler = tmp_path / "profiler"
    guest = tmp_path / "guest"
    startup = tmp_path / "startup"
    hook = tmp_path / "startup.sh"
    hook.write_text(f"printf '%s\\n' \"${{{channel}-unset}}\" >> {shlex.quote(str(startup))}\n")
    step = Step("g", "j", "", f"printf '%s' \"${{{channel}-}}\" > {shlex.quote(str(guest))}",
                jobs_flag="", hint=ResourceHint(preferred_inner_jobs=width),
                env={channel: "99"})
    request = IsolatedTrialRequest(
        trial_id="jobs-env", step=step.tag, inner_jobs=width, kind=CaptureKind.PERF,
        output_dir=tmp_path, expected_wall_s=1.0,
        window=CaptureWindow(0.02, 0.0, 0.02, False),
        argv_prefix=("/bin/bash", "-c", 'printf started > "$1"; shift; exec "$@"',
                     "profiler", str(profiler)),
        env={channel: "88", **({"BASH_ENV": str(hook)} if observe_startup else {})},
    )
    # Exercise the real shell handshake, using local regular files rather than a capture
    # controller. This tests inner command execution, not FIFO/controller timing or real perf.
    request.guest_launch_release_path.write_text("go\n")
    cfg = DagConfig(steps=(step,), default_jobs_env=channel)
    wrapped = cli._profiled_step(step, cfg, request)
    assert wrapped.jobs_env == "" and wrapped.jobs_flag == ""
    result = cli._run_single_step(wrapped, cfg, width, None, None, 0)
    assert profiler.read_text() == "started"
    assert int(request.guest_launch_ready_path.read_text()) > 0
    if refused:
        assert not result.ok
        assert result.returncode == 125
        assert not guest.exists(), "profiler startup is not proof that the guest was protected"
    else:
        assert result.ok and result.returncode == 0
        assert guest.read_text() == str(width)
        if observe_startup:
            assert startup.read_text().splitlines()[0] == "unset"


def test_target_profiler_capture_runs_after_pass_one_and_consumes_target_time(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    clock = [0.0]
    widths: list[int] = []
    captures: list[tuple[str, tuple[int, ...]]] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del step, cfg, cgroups, metrics, verbosity, vary_width, profile_timeseries_interval_s
        widths.append(inner_jobs)
        clock[0] += 0.1
        return cli._SweepMeasure(1.0, 1.0, 0.0, None, True)

    def fake_captures(
        cfg: DagConfig,
        step_samples: Sequence[
            tuple[Step, Mapping[int, Sequence[cli._SweepMeasure]]]
        ],
        config: CaptureConfig,
        cgroups: CgroupManager | None,
        sweep_git_sha: str,
        verbosity: int,
    ) -> int:
        del cfg, cgroups, sweep_git_sha, verbosity
        assert config.capture_perf
        assert len(step_samples) == 1
        step, samples = step_samples[0]
        captures.append((step.tag, tuple(samples)))
        clock[0] += 0.6
        return 0

    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr(cli, "_run_profiler_captures", fake_captures)
    monkeypatch.setattr("dagrun.cli.time.monotonic", lambda: clock[0])
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda: CpuTopology(tuple(range(4)), physical_core_count=2),
    )
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))
    monkeypatch.setattr(cli, "_report_profile_written", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_scaling_model", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_profile_report", lambda *_args: None)

    rc, out, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--target-time",
            "0.5",
            "--jobs",
            "1,4",
            "--output-dir",
            str(tmp_path / "profiles"),
            "--perf-record",
        ]
    )

    assert rc == 0, err
    assert widths == [1, 4]
    assert captures == [("g.j", (1, 4))]
    assert "target-time sweep complete: 1 pass(es), 2 sample(s)" in out
    assert "elapsed 0.800s, target 0.500s, overrun 0.300s" in out


def test_target_profiler_capture_includes_a_fixed_width_step(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "fixed", "cmd": "true", '
        '"jobs_flag": "", "hint": {"preferred_inner_jobs": 2}}]}',
        encoding="utf-8",
    )
    captured: list[tuple[str, tuple[int, ...]]] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del step, cfg, cgroups, metrics, verbosity, profile_timeseries_interval_s
        assert not vary_width
        assert inner_jobs == 2
        return cli._SweepMeasure(1.0, 0.8, 0.1, 1024, True)

    def fake_captures(
        cfg: DagConfig,
        step_samples: Sequence[
            tuple[Step, Mapping[int, Sequence[cli._SweepMeasure]]]
        ],
        config: CaptureConfig,
        cgroups: CgroupManager | None,
        sweep_git_sha: str,
        verbosity: int,
    ) -> int:
        del cfg, config, cgroups, sweep_git_sha, verbosity
        step, samples = step_samples[0]
        captured.append((step.tag, tuple(samples)))
        return 0

    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr(cli, "_run_profiler_captures", fake_captures)
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))
    monkeypatch.setattr(cli, "_report_profile_written", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_scaling_model", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_profile_report", lambda *_args: None)

    rc, _, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--target-time",
            "0",
            "--output-dir",
            str(tmp_path / "profiles"),
            "--perf-record",
        ]
    )

    assert rc == 0, err
    assert captured == [("g.fixed", (2,))]


def test_profiler_failure_is_loud_nonzero_and_names_retained_manifest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    step = Step("g", "j", "", "true", jobs_flag="--workers=")
    cfg = DagConfig(steps=(step,))
    manifest_path = tmp_path / "captures" / "failed" / "manifest.json"
    manifest_path.parent.mkdir(parents=True)
    manifest_path.write_text("{}\n", encoding="utf-8")

    def fail_capture(
        selection: object,
        config: CaptureConfig,
        run_trial: object,
    ) -> CaptureManifest:
        from dagrun.profile_capture import SweetSpotSelection

        del config, run_trial
        assert isinstance(selection, SweetSpotSelection)
        manifest = CaptureManifest(
            path=manifest_path,
            capture_id="failed",
            state=CaptureState.FAILED,
            created_at="2026-01-01T00:00:00Z",
            finished_at="2026-01-01T00:00:01Z",
            selection=selection,
            preflight=(),
            trials=(),
            errors=("wprof denied",),
        )
        raise ProfileCaptureError("wprof denied", manifest)

    monkeypatch.setattr(cli, "capture_at_sweet_spot", fail_capture)
    code = cli._run_profiler_captures(
        cfg,
        ((step, {1: (cli._SweepMeasure(1.0, 1.0, 0.0, None, True),)}),),
        CaptureConfig(output_dir=tmp_path, capture_perf=True),
        None,
        "deadbeef",
        0,
    )

    captured = capsys.readouterr()
    assert code == cli._SWEEP_CAPTURE_FAILURE
    assert "profiler capture FAILED" in captured.err
    assert str(manifest_path) in captured.err
    assert manifest_path.exists()


def test_capture_failure_still_refreshes_model_and_report_before_public_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    store = tmp_path / "profiles"
    manifest = store / "captures" / "failed" / "manifest.json"
    refreshed: list[str] = []

    def failed_sweep(*args: object, **kwargs: object) -> int:
        del args, kwargs
        manifest.parent.mkdir(parents=True)
        manifest.write_text("{}\n", encoding="utf-8")
        return cli._SWEEP_CAPTURE_FAILURE

    def refresh_model(perf_dir: str, cfg: DagConfig) -> None:
        del cfg
        assert Path(perf_dir) == store
        assert manifest.exists()
        refreshed.append("model")

    def refresh_report(
        perf_dir: str, cfg: DagConfig, dag_label: str, report_html: str | None
    ) -> bool:
        del cfg, dag_label, report_html
        assert Path(perf_dir) == store
        assert manifest.exists()
        refreshed.append("report")
        return True

    monkeypatch.setattr(cli, "_run_legacy_sweep", failed_sweep)
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))
    monkeypatch.setattr(cli, "_report_profile_written", lambda *_args: None)
    monkeypatch.setattr(cli, "_refresh_scaling_model", refresh_model)
    monkeypatch.setattr(cli, "_refresh_profile_report", refresh_report)

    rc, _, _ = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1..2",
            "--output-dir",
            str(store),
            "--perf-record",
        ]
    )

    assert rc == 1
    assert refreshed == ["model", "report"]


def test_target_sweep_characterizes_a_fixed_node_only_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "fixed",
                        "cmd": "true",
                        "jobs_flag": "",
                        "hint": {"preferred_inner_jobs": 2},
                    },
                    {
                        "group": "g",
                        "job": "scaling",
                        "cmd": "true",
                        "deps": ["g.fixed"],
                        "jobs_flag": "--workers=",
                    },
                ]
            }
        ),
        encoding="utf-8",
    )
    calls: list[tuple[str, int, bool]] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del cfg, cgroups, metrics, verbosity, profile_timeseries_interval_s
        calls.append((step.tag, inner_jobs, vary_width))
        return cli._SweepMeasure(1.0, 1.0, 0.0, None, True)

    ticks = iter((0.0, 0.0, 0.5, 0.5, 0.5, 2.0, 2.0, 2.0))
    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr("dagrun.cli.time.monotonic", lambda: next(ticks))
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda: CpuTopology(tuple(range(4)), physical_core_count=2),
    )
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))

    rc, out, err = _capture(
        ["sweep", "--dag", str(dag), "--target-time", "1", "--no-profile"]
    )

    assert rc == 0
    assert calls == [
        ("g.fixed", 2, False),
        ("g.scaling", 1, True),
        ("g.scaling", 2, True),
        ("g.scaling", 4, True),
        ("g.scaling", 1, True),
        ("g.scaling", 2, True),
        ("g.scaling", 3, True),
        ("g.scaling", 4, True),
    ]
    assert "characterizing its configured width once (width 2)" in out
    assert err == ""


def test_target_sweep_refuses_selected_fixed_width_above_delegated_cpu_budget(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cfg = dag_from_json(
        '{"steps": [{"group": "g", "job": "fixed", "cmd": "true", '
        '"jobs_flag": "", "hint": {"preferred_inner_jobs": 8}}]}'
    )
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda _max_cpus=None: CpuTopology(tuple(range(2)), physical_core_count=2),
    )
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        rc = cli._run_target_sweep(
            cfg,
            step_tag="g.fixed",
            jobs_spec=None,
            target_s=0.0,
            repeat=1,
            cgroups=None,
            perf_dir=None,
            sweep_git_sha="unknown",
            verbosity=0,
            c=cli.Palette(enabled=False),
            inherited_max_cpus=2,
        )

    assert rc == 2
    assert out.getvalue() == ""
    assert "selected fixed-width step 'g.fixed' requests 8 CPUs" in err.getvalue()
    assert "refusing an empty successful sweep" in err.getvalue()


def test_target_sweep_does_not_charge_an_intentionally_skipped_steps_memory(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cfg = dag_from_json(
        '{"steps": [{"group": "g", "job": "skipped", "cmd": "true", '
        '"skip_reason": "empty-manifest-bucket", '
        '"hint": {"hard_mem_max_bytes": 8589934592}}]}'
    )
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda _max_cpus=None: CpuTopology((0, 1), physical_core_count=2),
    )
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        rc = cli._run_target_sweep(
            cfg,
            step_tag=None,
            jobs_spec=None,
            target_s=0.0,
            repeat=1,
            cgroups=None,
            perf_dir=None,
            sweep_git_sha="unknown",
            verbosity=0,
            c=cli.Palette(enabled=False),
            inherited_max_mem_bytes=1024**3,
        )

    assert rc == 0
    assert "intentionally skipped by the DAG" in out.getvalue()
    assert "exceeding inherited delegated memory ceiling" not in err.getvalue()


def test_target_sweep_persists_pass_metadata(tmp_path: Path) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"cmdtype": "generic-with-flag", "jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    store = tmp_path / "profiles"

    rc, _, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--target-time",
            "0",
            "--jobs",
            "1,2",
            "--perf-dir",
            str(store),
            "--unsafe-no-cgroups",
        ]
    )

    assert rc == 0, err
    profile = next(store.glob("step_profiles_*.csv"))
    with profile.open(newline="", encoding="utf-8") as source:
        rows = list(csv.DictReader(source))
    assert len(rows) == 2
    assert rows[0]["sweep_id"]
    assert rows[0]["sweep_pass"] == "1"
    assert rows[0]["sweep_sample"] == "1"
    assert rows[0]["sweep_width_source"] == "explicit"
    assert rows[0]["sweep_mode"] == "target-time"
    assert int(rows[0]["sweep_physical_cores"]) >= 1
    assert int(rows[0]["sweep_logical_cpus"]) >= 1
    assert len(rows[0]["workload_digest"]) == 16
    assert all(row["user_s"] != "" for row in rows)
    assert all(row["sys_s"] != "" for row in rows)
    assert {row["enforcement_kind"] for row in rows} == {"unboxed"}
    assert {row["runner_name"] for row in rows} == {"sweep"}
    models = list(store.glob("scaling_model_*.json"))
    assert len(models) == 1
    saved_model = json.loads(models[0].read_text(encoding="utf-8"))
    assert saved_model["schema"] == 2
    assert saved_model["steps"][0]["step"] == "g.j"
    assert saved_model["steps"][0]["workload_digest"] == rows[0]["workload_digest"]
    report = store / "profile_report.html"
    assert report.is_file()
    assert 'id="dagrun-report-data"' in report.read_text(encoding="utf-8")
    assert f"interactive profile report refreshed at {report}" in err


def test_sweep_output_dir_report_override_and_opt_out(tmp_path: Path) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    store = tmp_path / "artifacts"
    custom = tmp_path / "reports" / "latest.html"

    rc, _, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1",
            "--output-dir",
            str(store),
            "--report-html",
            str(custom),
            "--unsafe-no-cgroups",
        ]
    )
    assert rc == 0, err
    assert custom.is_file()
    assert not (store / "profile_report.html").exists()
    assert "profile artifacts written under" in err
    assert f"interactive profile report refreshed at {custom}" in err

    no_report_store = tmp_path / "no-report"
    rc, _, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1",
            "--output-dir",
            str(no_report_store),
            "--no-report",
            "--unsafe-no-cgroups",
        ]
    )
    assert rc == 0, err
    assert next(no_report_store.glob("step_profiles_*.csv")).is_file()
    assert not (no_report_store / "profile_report.html").exists()
    assert "interactive profile report refreshed" not in err


def test_explicit_report_collision_fails_after_preserving_measurements(
    tmp_path: Path,
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    original = dag.read_bytes()
    store = tmp_path / "artifacts"

    rc, _, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1",
            "--output-dir",
            str(store),
            "--report-html",
            str(dag),
            "--unsafe-no-cgroups",
        ]
    )

    assert rc == 1
    assert dag.read_bytes() == original
    assert next(store.glob("step_profiles_*.csv")).is_file()
    assert "WARNING could not refresh interactive profile report" in err
    assert "report output resolves to DAG input" in err
    assert "interactive profile report refreshed at" not in err


def test_default_report_collision_warns_without_failing_sweep(tmp_path: Path) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    store = tmp_path / "artifacts"
    store.mkdir()
    report = store / "profile_report.html"
    report.write_bytes(b"do not overwrite")

    rc, _, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.j",
            "--jobs",
            "1",
            "--output-dir",
            str(store),
            "--unsafe-no-cgroups",
        ]
    )

    assert rc == 0
    assert report.read_bytes() == b"do not overwrite"
    assert next(store.glob("step_profiles_*.csv")).is_file()
    assert "WARNING could not refresh interactive profile report" in err
    assert "without dagrun report marker" in err
    assert "interactive profile report refreshed at" not in err


def test_target_sweep_refines_sparse_explicit_widths(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "true", '
        '"jobs_flag": "--workers="}]}',
        encoding="utf-8",
    )
    calls: list[int] = []

    def fake_run(
        step: Step,
        cfg: DagConfig,
        inner_jobs: int,
        cgroups: CgroupManager | None,
        metrics: MetricsSink | None,
        verbosity: int,
        *,
        vary_width: bool = True,
        profile_timeseries_interval_s: float | None = None,
    ) -> cli._SweepMeasure:
        del step, cfg, cgroups, metrics, verbosity, vary_width, profile_timeseries_interval_s
        calls.append(inner_jobs)
        return cli._SweepMeasure(1.0, 1.0, 0.0, None, True)

    ticks = iter((0.0, 0.0, 0.5, 0.5, 0.5, 2.0, 2.0, 2.0))
    monkeypatch.setattr(cli, "_run_single_step", fake_run)
    monkeypatch.setattr("dagrun.cli.time.monotonic", lambda: next(ticks))
    monkeypatch.setattr(
        cli,
        "_effective_sweep_topology",
        lambda: CpuTopology(tuple(range(8)), physical_core_count=4),
    )
    monkeypatch.setattr(cli, "_resolve_cgroup_manager", lambda *_args, **_kwargs: (None, 0))

    rc, out, err = _capture(
        [
            "sweep",
            "--dag",
            str(dag),
            "--target-time",
            "1",
            "--jobs",
            "1,4",
            "--no-profile",
        ]
    )

    assert rc == 0
    assert calls == [1, 4, 1, 2, 4]
    assert "cumulative widths [1,2,4]" in out
    assert err == ""


def test_sync_upload_rows_reuse_the_local_run_identity() -> None:
    rows = [{"step": "g.a"}, {"step": "g.a"}]

    stamped = cli._rows_with_observation_ids(rows, "run-local")

    assert stamped[0]["observation_id"] == "run-local"
    assert stamped[1]["observation_id"] == "run-local"


def test_sync_uploaded_and_local_copies_deduplicate() -> None:
    from dagrun import summary as summarylib

    raw = {"step": "g.a", "inner_jobs": "2", "elapsed_s": "1.0"}
    local = summarylib.summary_from_rows(
        [{**raw, "run_id": "run-local"}], "m", "c", None
    )
    uploaded = summarylib.summary_from_rows(
        cli._rows_with_observation_ids([raw], "run-local"), "m", "c", None
    )

    merged = summarylib.merge(local, uploaded)

    assert summarylib.summary_stats(merged) == (1, 1, 1)


def test_version_via_module() -> None:
    # argparse --version exits(0); run as a subprocess so it doesn't kill the test process.
    result = subprocess.run(
        [sys.executable, "-m", "dagrun", "--version"],
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout.strip() == f"{PROG} {__version__}"


def test_run_default_requires_cgroups_or_flag() -> None:
    # Cgroup boxing is ON by default. Run in a SUBPROCESS (so a re-exec cannot replace pytest)
    # with CI=1 set, which makes the boxing bring-up skip the re-exec -> boxing is "unavailable".
    # Default (no flag): the run must ERROR with a distinct nonzero exit (3). With
    # --allow-cgroup-failure it must downgrade to a best-effort UNBOXED run and exit 0.
    import os

    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        env = dict(os.environ, CI="1")  # force the boxing re-exec to be skipped
        # This test models a fresh CI entrypoint even when pytest itself is a delegated child.
        for inherited in (
            "DAGRUN_DELEGATED_CGROUP",
            "DAGRUN_DELEGATED_UNBOXED",
            "DAGRUN_IN_SCOPE",
            "DAGRUN_OUTER_RUN",
        ):
            env.pop(inherited, None)
        required = subprocess.run(
            [sys.executable, "-m", "dagrun", "run", "--dag", dag, "-q"],
            capture_output=True,
            text=True,
            env=env,
        )
        assert required.returncode == 3, required.stderr
        assert "cgroup boxing" in required.stderr.lower()

        allowed = subprocess.run(
            [sys.executable, "-m", "dagrun", "run", "--dag", dag, "-q",
             "--allow-cgroup-failure"],
            capture_output=True,
            text=True,
            env=env,
        )
        assert allowed.returncode == 0, allowed.stderr
        assert "UNBOXED" in allowed.stderr


def test_unsafe_no_cgroups_deliberately_skips_boxing() -> None:
    # --unsafe-no-cgroups is the DELIBERATE opt-out: it skips scope bring-up entirely and runs
    # unboxed even where boxing IS available (distinct from --allow-cgroup-failure's
    # capability fallback). It must exit 0, emit a LOUD reviewable warning naming the flag, and
    # never claim boxing is ACTIVE. Run in a SUBPROCESS so any (absent) re-exec is real.
    import os

    with tempfile.TemporaryDirectory() as tmp:
        dag = _demo_path(tmp)
        # Do NOT force the re-exec skip: the point is that the deliberate opt-out short-circuits
        # regardless of whether boxing could have been established.
        env = dict(os.environ)
        result = subprocess.run(
            [sys.executable, "-m", "dagrun", "run", "--dag", dag, "-q",
             "--unsafe-no-cgroups"],
            capture_output=True,
            text=True,
            env=env,
        )
        assert result.returncode == 0, result.stderr
        assert "DELIBERATELY UNBOXED via --unsafe-no-cgroups" in result.stderr
        assert "cgroup boxing ACTIVE" not in result.stderr


def test_boxed_run_enforces_cpu_timeout(
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    # Behavioral parity anchor for the Rust `cpu_timeout_smoke.rs`: prove the DEFAULT boxed run
    # actually ENFORCES a per-step CPU-time budget. A step that busy-loops forever with a tiny
    # cpu_timeout must be reaped as a CPU-TIMEOUT well before its (much larger) wall timeout.
    #
    # Run boxed (no --allow-cgroup-failure) in a SUBPROCESS so the boxing re-exec is real; do NOT
    # set CI=1 here (that would skip boxing). Cgroup boxing is environment-dependent, so when it
    # genuinely cannot be established the default run exits 3 and we skip LOUDLY, never silently.
    import os

    _skip_if_parent_validation_is_unboxed(inherited_runner_authority_env)

    dag = (
        '{"steps": [{"group": "cpu", "job": "burn", "desc": "burn CPU past budget",'
        ' "cmd": "while :; do :; done", "cpu_timeout": 1, "timeout": 30}]}'
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "cpu.json"
        path.write_text(dag, encoding="utf-8")
        proc = subprocess.run(
            [sys.executable, "-m", "dagrun", "run", "--dag", str(path),
             "-q", "--no-profile"],
            capture_output=True,
            text=True,
            env={**os.environ, **inherited_runner_authority_env},
        )
        if proc.returncode == 3:
            pytest.skip(
                "cgroup boxing unavailable (need cgroup-v2 + a working systemd --user scope); "
                f"cannot verify CPU-timeout enforcement here. Details:\n{proc.stderr}"
            )
        combined = proc.stdout + proc.stderr
        assert proc.returncode == 1, (
            "boxed run should FAIL (exit 1) when a step exceeds its CPU-time budget; "
            f"got {proc.returncode}\n{combined}"
        )
        assert "CPU-TIMEOUT" in combined, (
            "expected a CPU-TIMEOUT report proving the per-step CPU-time budget fired "
            f"(not the wall TIMEOUT):\n{combined}"
        )


def test_default_small_cpu_cap_is_enforced_and_allows_compliant_work(
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    # Plant both sides of the forcing default through the real cgroup path. Each command first
    # reads its own cpu.max, so a pass/failure cannot be attributed to merely configuring the
    # model without applying the one-core quota in the kernel.
    import os

    _skip_if_parent_validation_is_unboxed(inherited_runner_authority_env)

    command = (
        "python3 -c 'import pathlib,time; "
        "cg=next(x.split(\":\",2)[2].strip() for x in open(\"/proc/self/cgroup\") "
        "if x.startswith(\"0::\")); "
        "quota=pathlib.Path(\"/sys/fs/cgroup\"+cg+\"/cpu.max\").read_text().strip(); "
        "assert quota==\"100000 100000\",quota; "
        "start=time.process_time(); exec(\"while time.process_time()-start < {seconds}: pass\")'"
    )

    def run_cpu_burn(tmp: str, label: str, seconds: int) -> subprocess.CompletedProcess[str]:
        dag = (
            '{"steps": [{"group": "cpu", "job": "' + label + '", '
            '"desc": "undeclared one-core CPU burn", "cmd": '
            + json.dumps(command.format(seconds=seconds))
            + "}]}"
        )
        path = Path(tmp) / f"{label}.json"
        path.write_text(dag, encoding="utf-8")
        env = {
            **{k: v for k, v in os.environ.items() if k not in ("CI", "GITHUB_ACTIONS")},
            **inherited_runner_authority_env,
        }
        return subprocess.run(
            [sys.executable, "-m", "dagrun", "run", "--dag", str(path),
             "-q", "--no-profile"],
            capture_output=True,
            text=True,
            env=env,
        )

    with tempfile.TemporaryDirectory() as tmp:
        compliant = run_cpu_burn(tmp, "compliant-default", 1)
        if compliant.returncode == 3:
            pytest.skip(
                "cgroup boxing unavailable (need cgroup-v2 + a working systemd --user scope); "
                f"cannot verify default CPU caps here. Details:\n{compliant.stderr}"
            )
        compliant_output = compliant.stdout + compliant.stderr
        assert compliant.returncode == 0, (
            "an undeclared workload below the 10-second default must pass after reading back "
            f"the one-core kernel quota; got {compliant.returncode}\n{compliant_output}"
        )

        breach = run_cpu_burn(tmp, "breach-default", 12)
        breach_output = breach.stdout + breach.stderr
        assert breach.returncode == 1, (
            "an undeclared workload exceeding 10 CPU-seconds must fail; "
            f"got {breach.returncode}\n{breach_output}"
        )
        assert "CPU-TIMEOUT >10s cpu" in breach_output, (
            "expected the named default CPU-time cap, not a generic process failure:\n"
            f"{breach_output}"
        )


def test_boxed_reexec_via_symlink_imports_package(
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    # Regression guard for the fleet-wide local-validate breakage: a DEFAULT boxed run invoked
    # through the py/bin symlink (NOT `python -m`, NOT pip-installed) must re-exec a child that
    # can still import the package. The old code re-exec'd `python -m dagrun`, whose fresh child
    # had py/ off sys.path -> `No module named dagrun`, so every non-CI `run` (i.e. validate.sh)
    # died. The fix re-execs __main__.py by absolute path, which does its own sys.path fixup. Run
    # from a CWD *outside* py/ so an accidental cwd-relative import can't mask the bug. Boxing is
    # environment-dependent, so exit 3 (boxing genuinely unavailable) is a valid LOUD skip; the
    # one thing that must NEVER appear is the import failure.
    import os

    if inherited_runner_authority_env:
        pytest.skip(
            "fresh top-level systemd re-exec could escape the parent-owned "
            "validation cgroup; run this integration test standalone; "
            "tracked by #29 delegated-scope-smokes"
        )

    symlink = Path(__file__).resolve().parent.parent / "bin" / "dagrun"
    assert symlink.exists(), f"expected the console symlink at {symlink}"
    dag = (
        '{"steps": [{"group": "cpu", "job": "burn", "desc": "burn",'
        ' "cmd": "while :; do :; done", "cpu_timeout": 1, "timeout": 30}]}'
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "cpu.json"
        path.write_text(dag, encoding="utf-8")
        # Scrub CI markers so boxing is NOT skipped: this must exercise the real re-exec.
        env = {
            k: v
            for k, v in os.environ.items()
            if k
            not in (
                "CI",
                "GITHUB_ACTIONS",
                "DAGRUN_DIRECT_CGROUP",
                "DAGRUN_FORCE_SCOPE_ATTEMPT",
            )
        }
        proc = subprocess.run(
            [sys.executable, str(symlink), "run", "--dag", str(path), "-q", "--no-profile"],
            capture_output=True,
            text=True,
            cwd=tmp,
            env=env,
        )
        combined = proc.stdout + proc.stderr
        assert "No module named dagrun" not in combined, (
            "boxed re-exec child failed to import the package (the regression this guards): "
            f"rc={proc.returncode}\n{combined}"
        )
        if proc.returncode == 3:
            pytest.skip(
                "cgroup boxing unavailable here; re-exec child imported OK (no import error), "
                f"which is what this test guards.\n{proc.stderr}"
            )
        assert proc.returncode == 1 and "CPU-TIMEOUT" in combined, (
            "boxed symlink run should enforce the CPU budget once boxing is active; "
            f"got rc={proc.returncode}\n{combined}"
        )


def test_cores_flag_refuses_unboxed_soft_affinity() -> None:
    # `--unsafe-no-cgroups` deliberately leaves dagrun without an owned child scope.
    # `--cores` must therefore fail closed before launching the step; inherited process affinity
    # is escapable and cannot enforce a collision-free reservation.
    with tempfile.TemporaryDirectory() as tmp:
        dag = _one_step_dag(tmp, "echo SHOULD_NOT_RUN")
        rc, out, err = _capture(
            ["run", "--dag", dag, "--cores", "1", "-q", "--no-profile", _ACF]
        )
        assert rc == 3
        assert "SHOULD_NOT_RUN" not in out
        assert "hard cgroup cpuset unavailable; refusing to run" in err


def test_boxed_stdin_dag_survives_scope_reexec(
    inherited_runner_authority_env: Mapping[str, str],
) -> None:
    import os

    if inherited_runner_authority_env:
        pytest.skip(
            "fresh top-level systemd re-exec could escape the parent-owned "
            "validation cgroup; run this integration test standalone; "
            "tracked by #29 delegated-scope-smokes"
        )

    symlink = Path(__file__).resolve().parent.parent / "bin" / "dagrun"
    dag = (
        '{"steps":[{"group":"stress","job":"singleton","cmd":"sleep 1",'
        '"hint":{"hard_mem_max_bytes":67108864}}]}'
    )
    env = {
        k: v
        for k, v in os.environ.items()
        if k
        not in (
            "CI",
            "GITHUB_ACTIONS",
            "DAGRUN_DIRECT_CGROUP",
            "DAGRUN_FORCE_SCOPE_ATTEMPT",
        )
    }
    proc = subprocess.run(
        [
            sys.executable,
            str(symlink),
            "run",
            "--dag",
            "-",
            "--stress",
            "3",
            "--max-cpus",
            "3",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ],
        input=dag,
        capture_output=True,
        text=True,
        env=env,
    )
    combined = proc.stdout + proc.stderr
    if proc.returncode == 3:
        pytest.skip(
            "cgroup boxing unavailable here; boxed stdin ordering cannot run on this host.\n"
            + combined
        )
    assert proc.returncode == 0, combined
    assert (
        "containment OBSERVED" in combined
        or "cgroup boxing ACTIVE in parent-owned delegated step root" in combined
        or "reviewed uncontained nested execution" in combined
    )
    assert "invalid JSON" not in combined
    assert "stress.singleton: 3/3 passed" in proc.stdout
    assert (
        "maximum concurrent steps: 3 "
        "(--max-steps 3; --max-cpus 3 CPU target/per-step ceiling)"
    ) in proc.stdout


# --------------------------------------------------------------------------- --stress
def _one_step_dag(tmp: str, cmd: str = "true") -> str:
    path = Path(tmp) / "dag.json"
    path.write_text(
        '{"steps": [{"group": "g", "job": "j", "cmd": "%s"}]}' % cmd, encoding="utf-8"
    )
    return str(path)


def test_stress_reports_ratio_all_pass() -> None:
    # --stress N duplicates the step N times, runs the copies in parallel, and prints the
    # per-copy PASS/FAIL ratio (the finding). A passing step gives N/N.
    with tempfile.TemporaryDirectory() as tmp:
        dag = _one_step_dag(tmp, "sleep 0.3")
        rc, out, err = _capture(
            [
                "run",
                "--dag",
                dag,
                "--stress",
                "3",
                "--max-steps",
                "3",
                "--max-cpus",
                "3",
                "-q",
                _ACF,
            ]
        )
        assert rc == 0
        assert "stress results (3 generated graph copies):" in out
        assert "g.j: 3/3 passed" in out
        assert "maximum concurrent steps: 3 (--max-steps" in out
        # All 3 copies actually ran (the run summary counts every copy).
        assert "3 passed, 0 failed" in err
        # The memory-safe OK line names the derivation.
        assert "--stress 3: OK" in err and "max safe" in err


def test_stress_single_node_while_ignoring_selected_dependencies() -> None:
    # A focused experiment can stress one selected node after explicitly dropping its dependencies.
    dag = (
        '{"steps": ['
        '{"group": "build", "job": "app", "cmd": "true"},'
        '{"group": "test", "job": "unit", "cmd": "sleep 0.3", "deps": ["build.app"]}'
        "]}"
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "dag.json"
        path.write_text(dag, encoding="utf-8")
        rc, out, err = _capture(
            [
                "run",
                "--dag",
                str(path),
                "--selected",
                "test.unit",
                "--ignore-selected-deps",
                "--stress",
                "4",
                "-q",
                _ACF,
            ]
        )
        assert rc == 0
        assert "test.unit: 4/4 passed" in out
        # Only the selected node was stressed (build.app was not run).
        assert "build.app" not in out
        assert "4 passed, 0 failed" in err


def test_stress_lists_failing_copies() -> None:
    # A failing step fails every copy; --stress reports the ratio AND names the failed copies
    # (it implies --keep-going, so a failure does NOT eager-cancel the siblings' verdicts).
    with tempfile.TemporaryDirectory() as tmp:
        dag = _one_step_dag(tmp, "false")
        rc, out, _ = _capture(["run", "--dag", dag, "--stress", "3", "-q", _ACF])
        assert rc == 1
        assert "g.j: 0/3 passed" in out
        assert "3 FAILED: #1, #2, #3" in out


def test_stress_refuses_when_exceeds_box_memory() -> None:
    # A step whose per-copy footprint is enormous makes even a small N exceed the box budget:
    # the run REFUSES LOUDLY (exit 2) rather than silently OOMing sibling work.
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        # 500 TiB per-copy hard cap — larger than any real box budget.
        dag.write_text(
            '{"steps": [{"group": "g", "job": "j", "cmd": "true",'
            ' "hint": {"hard_mem_max_bytes": 549755813888000}}]}',
            encoding="utf-8",
        )
        rc, _, err = _capture(["run", "--dag", str(dag), "--stress", "2", "-q", _ACF])
        assert rc == 2
        assert "REFUSED" in err
        assert "per-copy footprint" in err
        assert "max safe --stress" in err


def test_stress_generation_removes_named_resource_serialization() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        dag.write_text(
            '{"resource_caps":{"exclusive":1},"steps":['
            '{"group":"g","job":"j","cmd":"sleep 0.3",'
            '"hint":{"resources":{"exclusive":1}}}]}',
            encoding="utf-8",
        )
        rc, out, err = _capture(
            [
                "run",
                "--dag",
                str(dag),
                "--stress",
                "4",
                "--max-steps",
                "4",
                "--max-cpus",
                "4",
                "-q",
                _ACF,
            ]
        )
        assert rc == 0, err
        assert "g.j: 4/4 passed" in out
        assert (
            "maximum concurrent steps: 4 "
            "(--max-steps 4; --max-cpus 4 CPU target/per-step ceiling)"
        ) in out


def test_stress_generated_graph_has_no_named_resource_scheduling() -> None:
    from dagrun.cli import _expand_stress
    from dagrun.io import dag_from_json
    from dagrun.scheduler import run_dag

    cfg = dag_from_json(
        '{"resource_caps":{"exclusive":1},"steps":['
        '{"group":"g","job":"j","cmd":"sleep 0.15",'
        '"hint":{"resources":{"exclusive":1}}}]}'
    )
    result = run_dag(_expand_stress(cfg, 4), jobs=4, keep_going=True, verbosity=0)
    assert len(result.outcomes) == 4
    assert result.max_concurrent_steps == 4


def test_stress_defaults_max_steps_to_max_cpus() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _one_step_dag(tmp, "sleep 1")
        rc, out, err = _capture(
            ["run", "--dag", dag, "--stress", "4", "--max-cpus", "3", "-q", _ACF]
        )
        assert rc == 0, err
        assert "g.j: 4/4 passed" in out
        assert (
            "maximum concurrent steps: 3 "
            "(--max-steps 3; --max-cpus 3 CPU target/per-step ceiling)"
        ) in out


def test_stress_n_must_be_positive() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag = _one_step_dag(tmp, "true")
        rc, _, err = _capture(["run", "--dag", dag, "--stress", "0", "-q", _ACF])
        assert rc == 2
        assert "--stress N must be >= 1" in err


def test_expand_stress_replicates_steps_and_rewires_deps() -> None:
    # Unit test of the fan-out: N shards, distinct #NN suffixes, intra-shard deps rewired.
    from dagrun.cli import _expand_stress
    from dagrun.io import dag_from_json

    cfg = dag_from_json(
        '{"steps": ['
        '{"group": "build", "job": "app", "cmd": "true"},'
        '{"group": "test", "job": "unit", "cmd": "true", "deps": ["build.app"]}'
        "]}"
    )
    expanded = _expand_stress(cfg, 3)
    tags = sorted(s.tag for s in expanded.steps)
    assert tags == [
        "build.app#1", "build.app#2", "build.app#3",
        "test.unit#1", "test.unit#2", "test.unit#3",
    ]
    by_tag = expanded.by_tag()
    # Each shard's test depends on the SAME shard's build, not a cross-shard build.
    assert by_tag["test.unit#2"].deps == ["build.app#2"]
    assert expanded.resource_caps == {}
    assert all(not step.hint.resources for step in expanded.steps)
    # n <= 1 is a no-op.
    assert _expand_stress(cfg, 1).steps == cfg.steps


def test_expand_stress_gives_each_copy_a_stable_index_in_its_environment() -> None:
    # Every copy runs the SAME `cmd`, so without a per-copy value in the environment the
    # command cannot choose a distinct output path: N copies write one file, the last writer
    # wins, and nothing errors. The `#NN` suffix does not help -- it is part of the job NAME,
    # which the command never sees.
    from dagrun.cli import STRESS_COPIES_ENV
    from dagrun.cli import STRESS_COPY_ENV
    from dagrun.cli import _expand_stress
    from dagrun.io import dag_from_json

    cfg = dag_from_json(
        '{"steps": [{"group": "demo", "job": "run", "cmd": "true", "env": {"KEEP": "me"}}]}'
    )
    expanded = _expand_stress(cfg, 10)
    indices = [s.env[STRESS_COPY_ENV] for s in expanded.steps]
    # Distinct, so a path built from the index cannot collide...
    assert len(set(indices)) == 10
    # ...zero-padded to the job suffix's width, so those paths sort in copy order...
    assert sorted(indices) == [f"{i:02d}" for i in range(1, 11)]
    # ...and every copy is told the total, so a split does not have to be given N twice.
    assert {s.env[STRESS_COPIES_ENV] for s in expanded.steps} == {"10"}
    # A step's own environment survives the expansion.
    assert all(s.env["KEEP"] == "me" for s in expanded.steps)
    # Unmultiplied graphs are untouched, so a command can tell one run from a copy.
    assert STRESS_COPY_ENV not in _expand_stress(cfg, 1).steps[0].env


# --------------------------------------------------------------------------- --args passthrough
def test_args_passthrough_substitutes_placeholder() -> None:
    # A step DECLARES it accepts args via the {args} token; --args is forwarded there verbatim.
    # The command echoes its args, so the captured detail proves substitution happened.
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        dag.write_text(
            '{"steps": [{"group": "t", "job": "unit",'
            ' "cmd": "echo GOT: {args}; test \\"{args}\\" = \\"-k foo\\""}]}',
            encoding="utf-8",
        )
        # -v (not -q) so the passing step's one-line summary is printed.
        # Value starts with '-', so the --args=VALUE form is required (argparse).
        rc, out, err = _capture(
            ["run", "--dag", str(dag), "--args=-k foo", _ACF]
        )
        assert rc == 0, err
        assert "GOT: -k foo" in out


def test_args_passthrough_errors_when_no_step_declares() -> None:
    # --args given but no selected step has {args}: refuse loudly (never silently drop the args).
    with tempfile.TemporaryDirectory() as tmp:
        dag = _one_step_dag(tmp, "true")
        rc, _, err = _capture(["run", "--dag", dag, "--args=-k foo", "-q", _ACF])
        assert rc == 2
        assert "no selected step declares" in err


def test_args_declared_but_not_passed_removes_token() -> None:
    # A step declaring {args} but run WITHOUT --args has the token removed so it still runs.
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        dag.write_text(
            '{"steps": [{"group": "t", "job": "unit", "cmd": "echo done {args}"}]}',
            encoding="utf-8",
        )
        rc, out, err = _capture(["run", "--dag", str(dag), _ACF])
        assert rc == 0, err
        assert "done" in out
        assert "{args}" not in out


def test_args_composes_with_selected_ignored_dependencies_and_stress() -> None:
    # Scope to one node, explicitly omit its dependencies, pass a test case, and multiply it.
    with tempfile.TemporaryDirectory() as tmp:
        dag = Path(tmp) / "dag.json"
        dag.write_text(
            '{"steps": ['
            '{"group": "build", "job": "app", "cmd": "true"},'
            '{"group": "dbi", "job": "file_metadata",'
            ' "cmd": "sleep 0.3; test \\"{args}\\" = \\"--case xyz\\"", "deps": ["build.app"]}'
            "]}",
            encoding="utf-8",
        )
        rc, out, err = _capture(
            [
                "run",
                "--dag",
                str(dag),
                "--selected",
                "dbi.file_metadata",
                "--ignore-selected-deps",
                "--args=--case xyz",
                "--stress",
                "5",
                "-q",
                _ACF,
            ]
        )
        assert rc == 0, err
        assert "dbi.file_metadata: 5/5 passed" in out
        assert "build.app" not in out


def test_unboxed_run_enforces_a_lower_bound_and_exposes_its_escape() -> None:
    # THE REGRESSION THIS FILE DID NOT HAVE. `test_boxed_run_enforces_cpu_timeout` above only
    # anchors the BOXED path. Unboxed, `CgroupManager.cpu_stats` returns None and the scheduler's
    # guard used to be skipped entirely, so every `cpu_timeout` was inert on exactly the lane that
    # matters most: any caller passing --allow-cgroup-failure, which an originating CI wrapper does
    # unconditionally under GITHUB_ACTIONS/CI. Measured before the procfs fallback existed, the
    # spinner below burned 60 CPU-seconds against a 3-second budget and exited GREEN.
    #
    # Unlike the boxed test this one can NEVER skip: it deliberately runs with boxing off, so it
    # is a real guard on every machine including boxing-less CI.
    #
    # Four brackets, run as separate invocations where needed so eager-exit cannot abort a
    # control: one breach, two concurrent breaches, an idle step, and a setsid escapee.
    # The sleeper is the discriminator, not decoration: it runs 6.7x its budget in WALL terms, so
    # a wall timeout mislabelled as a CPU timeout would kill it. Only a real CPU bound lets it
    # through, and only the breach proves the bound is enforced at all. Assert both or neither.
    import os

    spin = (
        '{"steps": [{"group": "cpu", "job": "burn", "desc": "burn CPU past budget",'
        ' "cmd": "python3 -c \\"import time\\nt=time.time()\\nwhile time.time()-t<60: pass\\"",'
        ' "cpu_timeout": 3, "timeout": 300}]}'
    )
    sleeper = (
        '{"steps": [{"group": "cpu", "job": "idle", "desc": "sleep well past the budget",'
        ' "cmd": "sleep 20", "cpu_timeout": 3, "timeout": 300}]}'
    )
    burn_command = "python3 -c 'import time\nt=time.time()\nwhile time.time()-t<60: pass'"
    multi = json.dumps(
        {
            "steps": [
                {
                    "group": "cpu",
                    "job": job,
                    "cmd": burn_command,
                    "cpu_timeout": 3,
                    "timeout": 300,
                }
                for job in ("burn-a", "burn-b")
            ]
        }
    )
    escapee = json.dumps(
        {
            "steps": [
                {
                    "group": "cpu",
                    "job": "escape",
                    "desc": "leave the measured group",
                    "cmd": (
                        "python3 -c 'import os,time\n"
                        "pid=os.fork()\n"
                        "if pid == 0:\n"
                        " os.setsid()\n"
                        " if os.fork() == 0:\n"
                        "  end=time.time()+4\n"
                        "  while time.time()<end: pass\n"
                        "  os._exit(0)\n"
                        " os._exit(0)\n"
                        "os.waitpid(pid,0)\n"
                        "time.sleep(5)'"
                    ),
                    "cpu_timeout": 1,
                    "timeout": 30,
                }
            ]
        }
    )
    env = dict(os.environ)
    # The procfs fallback is the subject of this test. Opt out explicitly so a self-hosting
    # validation shard with a delegated parent remains physically contained while dagrun itself
    # does not create or account through per-step cgroups.
    env["GITHUB_ACTIONS"] = "1"
    env["CI"] = "1"

    def _run(dag_text: str, name: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / f"{name}.json"
            path.write_text(dag_text, encoding="utf-8")
            return subprocess.run(
                [sys.executable, "-m", "dagrun", "run", "--dag", str(path),
                 "--unsafe-no-cgroups", "--keep-going", "--max-steps", "2", "-q",
                 "--no-profile"],
                capture_output=True,
                text=True,
                env=env,
            )

    # The idle control has its own temporary directory and process group. Start it first so its
    # required 20 seconds of wall time overlaps the three independent enforcement invocations.
    # Every invocation and every assertion below is unchanged; only their waiting overlaps.
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        idle_future = executor.submit(_run, sleeper, "idle")

        breach = _run(spin, "burn")
        breach_out = breach.stdout + breach.stderr
        assert "UNBOXED" in breach_out, (
            "this test is only meaningful with boxing OFF; if the run was boxed it is exercising the "
            f"cgroup path and proves nothing about the fallback:\n{breach_out}"
        )
        assert breach.returncode == 1, (
            "an UNBOXED step that burns 20x its CPU budget must FAIL; exit 0 here is the original "
            f"defect (declared budget, no enforcement). got {breach.returncode}\n{breach_out}"
        )
        assert "CPU-TIMEOUT >3s cpu" in breach_out, (
            f"expected the CPU budget to fire, not the (300s) wall timeout:\n{breach_out}"
        )
        assert "PROCFS SUBTREE" in breach_out, (
            "an unboxed breach must NAME the degraded accounting that produced it, so a reader can "
            f"weigh its known blind spots:\n{breach_out}"
        )

        parallel = _run(multi, "multi")
        parallel_out = parallel.stdout + parallel.stderr
        assert parallel.returncode == 1, parallel_out
        assert parallel_out.count("CPU-TIMEOUT >3s cpu") == 2, parallel_out
        assert parallel_out.count("PROCFS SUBTREE") == 2, parallel_out

        escaped = _run(escapee, "escape")
        idle = idle_future.result()

    idle_out = idle.stdout + idle.stderr
    assert idle.returncode == 0, (
        "a step that sleeps 20s on a 3s CPU budget burns ~no CPU and must SURVIVE. Killing it "
        "would mean the guard is measuring WALL time while calling itself a CPU timeout — the "
        f"exact confusion this bound exists to avoid. got {idle.returncode}\n{idle_out}"
    )

    escaped_out = escaped.stdout + escaped.stderr
    assert escaped.returncode == 0, (
        "the procfs process-group floor unexpectedly claimed cgroup-equivalent coverage; a "
        f"setsid escape must remain visible as the reason capabilities stays false:\n{escaped_out}"
    )
    assert "CPU-TIMEOUT" not in escaped_out, escaped_out
