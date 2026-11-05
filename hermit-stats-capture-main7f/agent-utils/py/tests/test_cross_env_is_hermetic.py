"""The differential must drive both engines from an environment IT chose.

``cross/differential.py`` builds every child environment from ``os.environ``, so anything the
developer exported is inherited by both engines. That is deliberate for most variables and wrong
for the ones a case is asserting about: an ambient ``CARGO_BUILD_JOBS`` makes the
``operator-build-width:unstated`` leg see "honouring CARGO_BUILD_JOBS=200" where it requires "no
CARGO_BUILD_JOBS in the environment".  More seriously, an outer validation DAG's delegated
cgroup and memory ceiling can turn large synthetic sizing cases into real nested-run refusals.
Both make ``make cross`` go red on a difference that exists in neither engine.  Runner authority
is retained only by the cases explicitly exercising containment; other case controls are stated
by each fixture.

Running the whole differential here would cost minutes, so this pins ``_env`` itself, which is the
single place every child environment is built.
"""

from __future__ import annotations

import importlib
import importlib.util
import os
import sys
from pathlib import Path
from types import ModuleType

import pytest

from agentctl import agent as agentctl_agent
from agentctl.client import HerdrClient

REPO_ROOT = Path(__file__).resolve().parents[2]


def _differential() -> ModuleType:
    # differential.py imports its sibling modules by bare name, as the harness runs it by path.
    cross = str(REPO_ROOT / "cross")
    if cross not in sys.path:
        sys.path.insert(0, cross)
    spec = importlib.util.spec_from_file_location(
        "_cross_differential_under_test", REPO_ROOT / "cross" / "differential.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_an_ambient_build_width_does_not_reach_either_engine(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("CARGO_BUILD_JOBS", "200")
    monkeypatch.setenv("DAGRUN_OPERATOR_BUILD_JOBS", "200")
    env = _differential()._env()
    assert "CARGO_BUILD_JOBS" not in env
    assert "DAGRUN_OPERATOR_BUILD_JOBS" not in env


def test_a_case_that_is_about_intent_can_still_state_it(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The scrubbing must not disarm the `stated` leg: `extra` is applied after the pops, so a case
    # that deliberately asks for a width still gets it.
    monkeypatch.delenv("CARGO_BUILD_JOBS", raising=False)
    env = _differential()._env({"CARGO_BUILD_JOBS": "200"})
    assert env["CARGO_BUILD_JOBS"] == "200"


def test_ambient_runner_authority_and_resource_policy_do_not_reach_synthetic_cases(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    controlled = (
        differential._RUNNER_AUTHORITY_ENV
        | differential._AMBIENT_CASE_CONTROL_ENV
    )
    for name in controlled:
        monkeypatch.setenv(name, "ambient-value-must-not-become-fixture-input")
    monkeypatch.setenv("DAGRUN_STEP", "outer.owner")
    monkeypatch.setenv("DAGRUN_STEP_STARTED_MONOTONIC_NS", "1234")

    env = differential._env()

    assert controlled.isdisjoint(env)
    assert env["DAGRUN_STEP"] == "outer.owner"
    assert env["DAGRUN_STEP_STARTED_MONOTONIC_NS"] == "1234"


def test_boxed_cases_retain_authority_but_not_ambient_policy(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    for name in differential._RUNNER_AUTHORITY_ENV:
        monkeypatch.setenv(name, f"authority:{name}")
    for name in differential._AMBIENT_CASE_CONTROL_ENV:
        monkeypatch.setenv(name, f"policy:{name}")

    env = differential._env(inherit_runner_authority=True)

    assert {
        name: env.get(name) for name in differential._RUNNER_AUTHORITY_ENV
    } == {
        name: f"authority:{name}" for name in differential._RUNNER_AUTHORITY_ENV
    }
    assert differential._AMBIENT_CASE_CONTROL_ENV.isdisjoint(env)


def test_explicit_fixture_values_win_after_the_ambient_scrub(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    monkeypatch.setenv("DAGRUN_DELEGATED_CGROUP", "/ambient/delegation")
    monkeypatch.setenv("DAGRUN_OUTER_MEMORY_MAX_BYTES", "100")

    env = differential._env(
        {
            "DAGRUN_DELEGATED_CGROUP": "/fixture/delegation",
            "DAGRUN_OUTER_MEMORY_MAX_BYTES": "200",
        }
    )

    assert env["DAGRUN_DELEGATED_CGROUP"] == "/fixture/delegation"
    assert env["DAGRUN_OUTER_MEMORY_MAX_BYTES"] == "200"


@pytest.mark.parametrize(
    ("environment", "expected"),
    [
        ({}, 8),
        ({"AGENT_UTILS_VALIDATION_JOBS": "1"}, 1),
        ({"AGENT_UTILS_VALIDATION_JOBS": "4"}, 4),
        ({"AGENT_UTILS_VALIDATION_JOBS": "64"}, 8),
    ],
)
def test_boxed_bandwidth_width_follows_the_outer_validation_budget(
    environment: dict[str, str], expected: int
) -> None:
    assert _differential()._validation_jobs(environment, effective_jobs=16) == expected


@pytest.mark.parametrize("value", ("", "0", "-1", "four", " 4", "+4", "٤"))
def test_invalid_outer_validation_budget_is_refused(value: str) -> None:
    with pytest.raises(ValueError, match="must be a positive integer"):
        _differential()._validation_jobs(
            {"AGENT_UTILS_VALIDATION_JOBS": value}, effective_jobs=16
        )


def test_standalone_validation_width_obeys_effective_affinity_and_quota() -> None:
    differential = _differential()
    assert differential._validation_jobs({}, effective_jobs=4) == 4
    assert differential._validation_jobs({}, effective_jobs=64) == 8


def test_dagrun_main_forwards_the_admitted_validation_width(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    observed: dict[str, int] = {}

    observed_skips: list[frozenset[str]] = []

    def compare(
        _random: int,
        _seed: int,
        *,
        validation_jobs: int = 8,
        allowed_skips: frozenset[str] = frozenset(),
    ) -> int:
        observed["validation_jobs"] = validation_jobs
        observed_skips.append(allowed_skips)
        return 0

    monkeypatch.setenv("AGENT_UTILS_VALIDATION_JOBS", "4")
    monkeypatch.delenv("AGENT_UTILS_CROSS_ALLOW_SKIP", raising=False)
    monkeypatch.setattr(differential, "_effective_validation_jobs", lambda: 16)
    monkeypatch.setattr(differential, "compare_dagrun", compare)

    assert differential.main(["--tool", "dagrun"]) == 0
    assert observed == {"validation_jobs": 4}
    assert observed_skips == [frozenset()]


def test_main_cannot_leave_a_record_in_an_outer_validate_run() -> None:
    """``scripts/validate.py`` exports the coverage directory to this suite's own node.

    ``main`` above writes one record per verdict into that directory, which the outer run would
    summarise as a cross node. The shared conftest removes the variable before every test.
    """

    assert "AGENT_UTILS_CROSS_COVERAGE_DIR" not in os.environ


def test_boxed_cpu_bandwidth_case_scales_every_width_observable() -> None:
    differential = _differential()
    dag, args, expected_facts, expected_quota = differential._boxed_cpu_bandwidth_case(
        4, dag_path="fixture.json", delegated=True
    )
    steps = dag["steps"]

    assert args[args.index("--dag") + 1] == "fixture.json"
    assert "-j4" in args
    assert len(steps) == 2
    assert all(step["hint"]["preferred_inner_jobs"] == 4 for step in steps)
    assert all("--cgroup-parent-levels 2" in step["cmd"] for step in steps)
    assert expected_facts == differential.CpuFootprintFacts(2, (4, 4), 2, 8)
    assert expected_quota == (4.0,)


def test_boxing_only_checks_skip_exact_unboxed_delegation_without_spawning(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    monkeypatch.setenv("DAGRUN_DELEGATED_UNBOXED", "1")
    monkeypatch.setenv("DAGRUN_OUTER_RUN", "outer-run")

    def unexpected_run(*_args: object, **_kwargs: object) -> object:
        raise AssertionError("a boxing-only check spawned under unboxed delegation")

    monkeypatch.setattr(differential, "run", unexpected_run)
    report = differential.Report()
    differential.compare_profile_timeseries_trace(["python"], ["rust"], report)
    differential.compare_operator_build_width(["python"], ["rust"], report)
    differential.compare_boxed_cpu_bandwidth(["python"], ["rust"], report)

    # None of the four checks ran, so none of them may be counted as a pass.
    assert report.checks == 0
    assert report.failures == []
    assert [(skip.label, skip.kind) for skip in report.skipped] == [
        ("profile-timeseries", "boxing"),
        ("operator-build-width:stated", "boxing"),
        ("operator-build-width:unstated", "boxing"),
        ("boxed-cpu-bandwidth", "boxing"),
    ]


def test_delegated_cgroup_does_not_disable_boxing_only_checks(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    monkeypatch.setenv("DAGRUN_DELEGATED_UNBOXED", "1")
    monkeypatch.setenv("DAGRUN_DELEGATED_CGROUP", "/delegated")
    monkeypatch.setenv("DAGRUN_OUTER_RUN", "outer-run")

    assert differential._inside_delegated_harness()
    assert not differential._parent_offers_only_unboxed_delegation()

    monkeypatch.delenv("DAGRUN_DELEGATED_CGROUP")
    monkeypatch.delenv("DAGRUN_OUTER_RUN")
    assert not differential._parent_offers_only_unboxed_delegation()


def test_run_output_normalization_removes_only_terminal_elapsed_time() -> None:
    differential = _differential()
    python_output = (
        "plan: cpa\n"
        "scheduled order: p.bud, p.knee\n"
        "[p.bud] ✓ PASS   budget-capped (0s)\n"
        "[p.knee] ✗ FAIL   sub-linear knee (2s, exit 7)\n"
    )
    rust_output = python_output.replace("(0s)", "(1s)").replace("(2s, exit 7)", "(3s, exit 7)")

    normalized = differential._deterministic_run_output
    assert normalized(python_output) == normalized(rust_output)
    assert normalized(python_output) != normalized(
        rust_output.replace("scheduled order: p.bud, p.knee", "scheduled order: p.knee, p.bud")
    )
    assert normalized(python_output) != normalized(
        rust_output.replace("sub-linear knee", "different step")
    )
    assert normalized(python_output) != normalized(
        rust_output.replace("exit 7", "exit 8")
    )


def test_only_exact_transient_user_bus_refusals_are_retryable() -> None:
    differential = _differential()
    outcome = differential.Outcome

    assert differential._transient_user_bus_refusal(
        outcome(
            1,
            "",
            "Failed to connect to user scope bus via local transport: Connection refused\n",
        )
    )
    assert differential._transient_user_bus_refusal(
        outcome(0, "", ""),
        outcome(1, "", "Failed to connect to user scope bus: Connection refused"),
    )
    assert not differential._transient_user_bus_refusal(
        outcome(1, "", "Failed to connect to user scope bus: Connection refused"),
        outcome(1, "", "a real allocator failure"),
    )
    assert not differential._transient_user_bus_refusal(
        outcome(1, "", "Failed to connect to user scope bus: Permission denied")
    )
    assert not differential._transient_user_bus_refusal(
        outcome(1, "", "Connection refused")
    )
    assert not differential._transient_user_bus_refusal(
        outcome(0, "", "Failed to connect to user scope bus: Connection refused")
    )


# --- Host command-line tools -------------------------------------------------------------------
#
# The agentctl and herdr-agent differentials run both editions with the developer's PATH. A case
# that lets an edition fall back to a tool on PATH, such as the default native goal transport
# `codex app-server proxy`, compares two runs of the host's real tool. Codex prints a
# per-process path, so the two runs differ and `make validate` went red on a machine with Codex
# installed while passing on one without it. The harness puts a guard ahead of PATH and reports
# every call that reaches it. These tests put an ambient tool that prints its own process ID
# first on the caller's PATH, which is the shape that diverged.


def _cross_module(name: str) -> ModuleType:
    cross = str(REPO_ROOT / "cross")
    if cross not in sys.path:
        sys.path.insert(0, cross)
    return importlib.import_module(name)


def _ambient_tool(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, program: str
) -> Path:
    """Put a host tool whose output depends on its process ID first on PATH."""
    directory = tmp_path / "ambient-bin"
    directory.mkdir(exist_ok=True)
    ran = tmp_path / f"ambient-{program}-ran"
    tool = directory / program
    tool.write_text(
        f"#!{sys.executable}\n"
        "import os, sys\n"
        f"with open({str(ran)!r}, 'a', encoding='utf-8') as stream:\n"
        "    stream.write(f'{os.getpid()}\\n')\n"
        "print(f'host state at /tmp/ambient/{os.getpid()}/arcrc', file=sys.stderr)\n"
        "raise SystemExit(1)\n",
        encoding="utf-8",
    )
    tool.chmod(0o700)
    monkeypatch.setenv("PATH", f"{directory}{os.pathsep}{os.environ.get('PATH', '')}")
    return ran




# The Python edition with the host-wide target locks moved to private files. A fresh interpreter
# does not inherit conftest's in-process patch, and `start` would otherwise wait on the account's
# real lock files, which live agents hold. The locks are still real flocks.
_PRIVATE_LOCK_AGENTCTL = """import os, sys
from agentctl import agent
roots = tuple(os.path.join(sys.argv[1], part) for part in ("account", "legacy"))
def private_lock_paths(name):
    lock_name = agent._target_lock_name(name)
    return os.path.join(roots[0], lock_name), os.path.join(roots[1], lock_name)
agent._target_lock_paths = private_lock_paths
sys.argv = ["agentctl", *sys.argv[2:]]
from agentctl.cli import main
raise SystemExit(main())
"""


def _python_agentctl(lock_root: Path) -> list[str]:
    for part in ("account", "legacy"):
        (lock_root / part).mkdir(parents=True, mode=0o700, exist_ok=True)
    lock_root.chmod(0o700)
    # A just-recorded refresh of the legacy lock files, so a lock lookup does not scan.
    (lock_root / "account" / agentctl_agent._LEGACY_REFRESH_MARKER).touch(mode=0o600)
    return [sys.executable, "-c", _PRIVATE_LOCK_AGENTCTL, str(lock_root)]


def _private_locks_taken(lock_root: Path) -> list[str]:
    return sorted(
        path.name for path in (lock_root / "account").iterdir()
        if path.name != agentctl_agent._LEGACY_REFRESH_MARKER
    )


@pytest.mark.parametrize(
    "program", _cross_module("herdr_agent_differential").HOST_CLI_GUARDED
)
def test_an_edition_that_reaches_a_host_cli_is_refused_rather_than_compared(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, program: str
) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    ran = _ambient_tool(tmp_path, monkeypatch, program)
    # Each "edition" runs the named tool from PATH exactly as a default transport would.
    edition = [
        sys.executable, "-c",
        "import subprocess, sys; "
        "done = subprocess.run([sys.argv[1], 'probe'], capture_output=True, text=True); "
        "print(done.returncode, done.stderr, end='')",
    ]
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("reaches-host-cli")
        python, rust = harness.invoke(case, (program, *herdr_agent.FIXTURE_HERDR))
        report = herdr_agent.Report()
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    # The ambient tool never ran, so its process ID cannot make the editions differ ...
    assert not ran.exists()
    assert python == rust
    assert python.stdout.startswith("127 cross harness host CLI guard")
    # ... and the call is a named failure, not a silent agreement.
    calls = harness.host_cli_calls()
    assert [call.split(": ", 1)[0] for call in calls] == [
        "001-reaches-host-cli/python/project", "001-reaches-host-cli/rust/project",
    ]
    assert all(f'"program": "{program}", "argv": ["probe"]' in call for call in calls)
    assert report.checks == 1
    assert len(report.failures) == 1
    assert report.failures[0].startswith("harness/no-host-cli: 2 call(s)")


def test_the_hostile_path_fixture_keeps_the_guard_first(tmp_path: Path) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    edition = [sys.executable, "-c", "import os; print(os.environ['PATH'], end='')"]
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("hostile", {"hostile_path": True})
        python, _ = harness.invoke(case, ())
    finally:
        harness.close()
    assert python.stdout == os.pathsep.join(
        ("<ROOT>/" + herdr_agent.HOST_CLI_GUARD_DIRECTORY, "<ROOT>/hostile-bin")
    )


def test_the_production_client_ignores_path_for_the_default_herdr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Why the harness refuses instead of stubbing Herdr: Python never consults a PATH stub."""
    _ambient_tool(tmp_path, monkeypatch, "herdr")
    home = tmp_path / "home"
    (home / "bin").mkdir(parents=True)
    installed = home / "bin" / "herdr"
    installed.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
    installed.chmod(0o700)
    monkeypatch.setattr(HerdrClient, "_account_home", staticmethod(lambda: str(home)))

    resolved = HerdrClient()._executable()

    on_this_host = {
        os.path.realpath(path) for path in ("/usr/local/bin/herdr", "/usr/bin/herdr")
        if os.path.isfile(path) and os.access(path, os.X_OK)
    }
    assert resolved in on_this_host | {str(installed)}
    assert resolved != str(tmp_path / "ambient-bin" / "herdr")


# A goal command that names a file inside the case directory and still runs a host program.
_RUNTIME_EXECS_CODEX = (
    '["<ROOT>/fake-muse-runtime","-c",'
    '"import os; os.execv(\'/usr/local/bin/codex\', [\'codex\', \'app-server\'])"]'
)


def _recording_edition(tmp_path: Path) -> tuple[list[str], Path]:
    started = tmp_path / "edition-started"
    edition = [
        sys.executable, "-c",
        f"open({str(started)!r}, 'a', encoding='utf-8').write('started\\n')",
    ]
    return edition, started


@pytest.mark.parametrize("arguments", (
    ("start", "worker"),
    ("unknown",),
    ("status", "--pane", "--help"),
    ("--registry", "<ROOT>/registry", "send", "worker", "text"),
    ("start", "worker", "--herdr-bin", "herdr"),
    ("start", "worker", "--herdr-bin=/usr/local/bin/herdr"),
    ("start", "worker", "--herdr-bin", "<ROOT>/../outside/herdr"),
    # After a `--` terminator these are message text and a positional name, not options.
    ("send", "worker", "--", "--herdr-bin=<HERDR>"),
    ("send", "worker", "--", "--herdr-bin", "<HERDR>"),
    ("--", "start", "worker"),
    ("--", "start", "--help"),
    ("start", "--", "--help"),
    # After a root-level `--`, the Python subcommand parser reads options again.
    ("--herdr-bin", "<HERDR>", "--", "start", "worker", "--herdr-bin", "herdr"),
    ("--herdr-bin", "<HERDR>", "--", "start", "worker", "--herdr-bin=/usr/local/bin/herdr"),
    # The last --herdr-bin wins in both parsers.
    ("start", "worker", "--herdr-bin", "<HERDR>", "--herdr-bin", "herdr"),
    # The Python Chat bridge builds a default client, so naming the fixture cannot redirect it.
    ("chat", "launch", "--config", "<ROOT>/chat.json", "--harness-arg", "--herdr-bin=<HERDR> x"),
    ("chat", "init", "--config", "<ROOT>/chat.json", "--herdr-bin", "<HERDR>"),
    ("--herdr-bin", "<HERDR>", "chat", "status"),
    # Rust globals the Python edition lacks still precede the command, so this is Chat to Rust,
    # whose `chat publish` runs the outbound command stored in the bridge state.
    ("--from-session=fixture", "chat", "publish", "--herdr-bin", "<HERDR>"),
    ("--from-session", "fixture", "chat", "status", "--herdr-bin", "<HERDR>"),
    ("--agentcloud-url", "ws://fixture", "chat", "status", "--herdr-bin", "<HERDR>"),
    ("--agentcloud-url=ws://fixture", "chat", "status", "--herdr-bin", "<HERDR>"),
    ("--agentcloudctl-bin", "<HERDR>", "chat", "status", "--herdr-bin", "<HERDR>"),
    ("--agentterm-bin=<HERDR>", "--herdr-bin", "<HERDR>", "chat", "status"),
    # An option the guard does not know hides where the command starts.
    ("--not-a-global", "chat", "status", "--herdr-bin", "<HERDR>"),
    ("--not-a-global=x", "--herdr-bin", "<HERDR>", "chat", "status"),
))
def test_an_invocation_that_could_reach_the_installed_herdr_is_never_started(
    tmp_path: Path, arguments: tuple[str, ...]
) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("installed-herdr")
        python, rust = harness.invoke(case, arguments)
        report = herdr_agent.Report()
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    assert python.stderr.startswith("cross harness host CLI guard: refused to run the editions")
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all('"program": "herdr"' in call and '"refused": ' in call for call in calls)
    assert [failure.split(":", 1)[0] for failure in report.failures] == ["harness/no-host-cli"]


@pytest.mark.parametrize("arguments", (
    (),
    ("--help",),
    ("--version",),
    ("--userguide", "unknown"),
    ("userguide",),
    ("quickstart",),
    ("capabilities",),
    ("profiles", "--cwd", "<ROOT>"),
    ("skill", "install", "--harness", "codex"),
    ("stop", "--help"),
    ("--registry", "--help"),
    ("start", "worker", "--herdr-bin", "<HERDR>"),
    ("start", "worker", "--herdr-bin=<HERDR>"),
    ("start", "worker", "--herdr-bin", "./fake-herdr"),
    ("--herdr-bin", "<HERDR>", "start", "worker"),
    ("send", "worker", "--herdr-bin", "<HERDR>", "--", "--text-that-looks-like-an-option"),
    ("chat", "--help"),
    ("goal", "worker", "--herdr-bin", "<HERDR>", "--goal-command-json", '["<HERDR>","goal-rpc"]'),
    ("goal", "worker", "--goal-command-json=[\"./fake-herdr\",\"goal-rpc\"]", "--herdr-bin",
     "<HERDR>"),
    ("goal", "worker", "--goal-command-json", "[]", "--herdr-bin", "<HERDR>"),
    # The value of a global is not the command, even when it is a command's name.
    ("--from-session", "chat", "--herdr-bin", "<HERDR>", "status", "worker"),
    ("--agentcloud-url=ws://fixture", "--herdr-bin", "<HERDR>", "status", "worker"),
    # A Herdr-free command after a global, spaced or `=`, needs no --herdr-bin.
    ("--registry", "<ROOT>/registry", "quickstart"),
    ("--registry=<ROOT>/registry", "quickstart"),
    ("--herdr-bin", "<HERDR>", "--agentcloudctl-bin", "<HERDR>", "--agentterm-bin=./fake-herdr",
     "status", "worker"),
    ("--herdr-bin", "<HERDR>", "inbox", "watch", "--to", "coord", "--once",
     "--claude-bin", "<ROOT>/fake-herdr"),
    # A registry inside the case directory that does not exist yet stores no command.
    ("goal", "worker", "--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry"),
    ("goal", "worker", "--herdr-bin", "<HERDR>", "--state=./registry"),
))
def test_herdr_free_commands_and_fixture_herdr_invocations_still_run(
    tmp_path: Path, arguments: tuple[str, ...]
) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("fixture-herdr")
        python, rust = harness.invoke(case, arguments)
    finally:
        harness.close()

    assert (python.returncode, rust.returncode) == (0, 0)
    assert started.read_text(encoding="utf-8") == "started\n" * 2
    assert harness.host_cli_calls() == []


@pytest.mark.parametrize(("arguments", "program"), (
    (("goal", "worker", "--herdr-bin", "<HERDR>",
      "--goal-command-json", '["/usr/local/bin/codex","app-server","proxy"]'), "goal-command"),
    (("start", "worker", "--herdr-bin", "<HERDR>",
      "--goal-command-json", '["/usr/local/bin/codex","app-server","proxy"]'), "goal-command"),
    (("goal", "worker", "--herdr-bin", "<HERDR>",
      '--goal-command-json=["codex","app-server","proxy"]'), "goal-command"),
    (("goal", "worker", "--herdr-bin", "<HERDR>",
      "--goal-command-json", '["<ROOT>/../outside/codex"]'), "goal-command"),
    (("goal", "worker", "--herdr-bin", "<HERDR>", "--goal-command-json", "not json"),
     "goal-command"),
    # The herdr-agent Python edition turns each element into a string with str().
    (("goal", "worker", "--herdr-bin", "<HERDR>", "--goal-command-json", "[1]"), "goal-command"),
    # A file inside the case is not enough: `fake-muse-runtime` is a copy of the interpreter.
    (("goal", "worker", "--herdr-bin", "<HERDR>", "--goal-command-json", _RUNTIME_EXECS_CODEX),
     "goal-command"),
    (("goal", "worker", "--herdr-bin", "<HERDR>",
      '--goal-command-json=["./fake-muse-skills","goal-rpc"]'), "goal-command"),
    # The fixture itself, in any mode but the goal transport.
    (("goal", "worker", "--herdr-bin", "<HERDR>", "--goal-command-json", '["<HERDR>"]'),
     "goal-command"),
    (("goal", "worker", "--goal-command-json=[\"./fake-herdr\"]", "--herdr-bin", "<HERDR>"),
     "goal-command"),
    (("goal", "worker", "--herdr-bin", "<HERDR>",
      "--goal-command-json", '["<HERDR>","goal-rpc","extra"]'), "goal-command"),
    (("goal", "worker", "--herdr-bin", "<HERDR>",
      "--goal-command-json", '["<HERDR>","pane","run","w1:p1","x"]'), "goal-command"),
    (("--herdr-bin", "<HERDR>", "--", "goal", "worker",
      "--goal-command-json", '["/usr/local/bin/codex"]'), "goal-command"),
    (("--herdr-bin", "<HERDR>", "--agentcloudctl-bin", "/usr/local/bin/agentcloudctl",
      "status", "worker"), "agentcloudctl"),
    (("--herdr-bin", "<HERDR>", "status", "worker", "--agentterm-bin=agentterm"), "agentterm"),
    # The Rust `inbox watch` runs `<claude-bin> agents --json` (rs/agentctl/src/inbox/watch.rs).
    (("inbox", "watch", "--to", "coord", "--once", "--herdr-bin", "<HERDR>",
      "--claude-bin", "/usr/local/bin/claude"), "claude"),
    (("--herdr-bin", "<HERDR>", "inbox", "watch", "--to", "coord", "--once",
      "--claude-bin=claude"), "claude"),
))
def test_an_option_that_names_an_outside_executable_is_never_started(
    tmp_path: Path, arguments: tuple[str, ...], program: str
) -> None:
    """A PATH stub cannot catch a program named by an explicit path, so the harness refuses it."""
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("outside-executable")
        python, rust = harness.invoke(case, arguments)
        report = herdr_agent.Report()
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    assert python.stderr.startswith("cross harness host CLI guard: refused to run the editions")
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all(f'"program": "{program}"' in call and '"refused": ' in call for call in calls)
    assert [failure.split(":", 1)[0] for failure in report.failures] == ["harness/no-host-cli"]


class _TransportStarted(Exception):
    """Raised in place of starting the native goal transport."""


def test_the_goal_transport_runs_an_explicit_path_as_given_and_the_guard_refuses_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The premise behind refusing an outside `--goal-command-json`, through production code.

    The agentctl parser and its JSON decoding hand the command to the goal transport, which
    passes it to process creation unchanged: an absolute path is executed directly, never looked
    up on PATH where a stub could catch it. Process creation is replaced by a recorder that
    raises, so nothing runs.
    """
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import cli as agentctl_cli
    from agentctl import codex_goal

    root = tmp_path / "case"
    root.mkdir()
    fixture = root / "fake-herdr"
    _executable_file(fixture)
    outside = os.path.join(str(tmp_path), "outside", "codex")
    value = f'["{outside}","app-server","proxy"]'
    started: list[list[str]] = []

    def recording_popen(argv: list[str], **_: object) -> object:
        started.append(list(argv))
        raise _TransportStarted

    monkeypatch.setattr(subprocess, "Popen", recording_popen)
    namespace = agentctl_cli.parser().parse_args(["goal", "worker", "--goal-command-json", value])
    command = agentctl_cli._goal_command(namespace)
    with pytest.raises(_TransportStarted):
        codex_goal.get_goal("thread-1", command)

    assert started == [[outside, "app-server", "proxy"]]
    arguments = ["goal", "worker", "--herdr-bin", str(fixture), "--goal-command-json", value]
    refusal = herdr_agent.host_cli_refusal(root, arguments)
    assert refusal is not None and refusal[0] == "goal-command"
    fixture_value = f'["{fixture}","goal-rpc"]'
    assert herdr_agent.host_cli_refusal(
        root, ["goal", "worker", "--herdr-bin", str(fixture), "--goal-command-json", fixture_value]
    ) is None


@pytest.mark.parametrize("link", ("linked-herdr", "linked-directory/herdr"))
def test_a_case_symlink_to_an_outside_herdr_is_never_started(tmp_path: Path, link: str) -> None:
    """Both production clients canonicalize the executable, so a link names its target."""
    herdr_agent = _cross_module("herdr_agent_differential")
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "herdr").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    (outside / "herdr").chmod(0o700)
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("linked-herdr")
        for root in (case.python_root, case.rust_root):
            (root / "linked-herdr").symlink_to(outside / "herdr")
            (root / "linked-directory").symlink_to(outside, target_is_directory=True)
        python, rust = harness.invoke(case, ("start", "worker", "--herdr-bin", f"<ROOT>/{link}"))
        relative, _ = harness.invoke(case, ("start", "worker", "--herdr-bin", f"./{link}"))
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == relative.returncode == 127
    assert "is not a fixture inside the case directory" in python.stderr
    assert len(harness.host_cli_calls()) == 4


def _executable_file(path: Path) -> None:
    path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    path.chmod(0o700)


@pytest.mark.parametrize("layout", ("python-escapes", "kernel-escapes"))
def test_a_symlink_before_dotdot_must_stay_inside_under_both_readings(
    tmp_path: Path, layout: str
) -> None:
    """`link/../herdr`: the Python client drops `..` before following links, the kernel after.

    Each layout keeps one reading inside the case and sends the other outside, so the guard must
    check both. The Python reading is taken from the production resolver, which only inspects
    the file; nothing is executed.
    """
    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import client as agentctl_client

    outside = tmp_path / "outside"
    (outside / "deeper").mkdir(parents=True)
    _executable_file(outside / "herdr")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("dotdot-herdr")
        for root in (case.python_root, case.rust_root):
            (root / "nested" / "deeper").mkdir(parents=True)
            _executable_file(root / "nested" / "herdr")
            if layout == "python-escapes":
                (root / "link").symlink_to(root / "nested" / "deeper", target_is_directory=True)
                (root / "herdr").symlink_to(outside / "herdr")
            else:
                (root / "link").symlink_to(outside / "deeper", target_is_directory=True)
                _executable_file(root / "herdr")
        root = case.python_root
        python_reading = agentctl_client._validated_executable(
            str(root / "link" / ".." / "herdr"), "Herdr"
        )
        kernel_reading = os.path.realpath(root / "link" / ".." / "herdr")
        python, rust = harness.invoke(
            case, ("start", "worker", "--herdr-bin", "<ROOT>/link/../herdr")
        )
        relative, _ = harness.invoke(case, ("start", "worker", "--herdr-bin", "./link/../herdr"))
    finally:
        harness.close()

    inside = os.path.realpath(root / "nested" / "herdr")
    escaped = os.path.realpath(outside / "herdr")
    if layout == "python-escapes":
        assert (python_reading, kernel_reading) == (escaped, inside)
    else:
        assert (python_reading, kernel_reading) == (os.path.realpath(root / "herdr"), escaped)
    assert not started.exists()
    assert python == rust
    assert python.returncode == relative.returncode == 127
    assert "is not a fixture inside the case directory" in python.stderr
    assert "is not a fixture inside the case directory" in relative.stderr
    assert len(harness.host_cli_calls()) == 4


@pytest.mark.parametrize(
    "variable", sorted(_cross_module("herdr_agent_differential").AMBIENT_EXECUTABLE_OVERRIDES)
)
def test_an_ambient_executable_override_is_not_inherited(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, variable: str
) -> None:
    """An absolute override in the caller's environment would bypass every PATH stub."""
    herdr_agent = _cross_module("herdr_agent_differential")
    program = herdr_agent.AMBIENT_EXECUTABLE_OVERRIDES[variable]
    ran = tmp_path / "override-ran"
    override = tmp_path / "override-bin" / program
    override.parent.mkdir()
    override.write_text(f"#!/bin/sh\necho $$ > {ran}\n", encoding="utf-8")
    override.chmod(0o700)
    monkeypatch.setenv(variable, str(override))
    # Each "edition" resolves the tool as agentctl/foreign/lib.py does: the override, else the name.
    edition = [
        sys.executable, "-c",
        "import os, subprocess, sys; "
        "done = subprocess.run([os.environ.get(sys.argv[1], sys.argv[2]), 'probe'], "
        "capture_output=True, text=True); print(done.returncode, end='')",
    ]
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("ambient-override")
        python, rust = harness.invoke(case, (variable, program, *herdr_agent.FIXTURE_HERDR))
        report = herdr_agent.Report()
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    assert not ran.exists()
    assert python == rust
    assert python.stdout == "127"
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all(f'"program": "{program}", "argv": ["probe"]' in call for call in calls)
    assert [failure.split(":", 1)[0] for failure in report.failures] == ["harness/no-host-cli"]


# The Git both editions run (agentctl/profiles.py, profiles.rs) and the call they make with it.
_SYSTEM_GIT = "/usr/bin/git"
_CHECK_IGNORE = ("check-ignore", "--quiet", "--", ".agentctl/profiles.toml")
# The Git variables every edition gets besides GIT_CEILING_DIRECTORIES: no system or global
# configuration, no fetching of promised objects, and core.fsmonitor turned off at command
# scope, above the repository's own.
_CASE_GIT = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_NO_LAZY_FETCH": "1",
    "GIT_CONFIG_COUNT": "1",
    "GIT_CONFIG_KEY_0": "core.fsmonitor",
    "GIT_CONFIG_VALUE_0": "false",
}
# An "edition" that runs Git as both editions do, in its own repository when given `init`, and
# reports the check-ignore status and the Git variables it was given.
_GIT_PROBE_EDITION = (
    "import json, os, subprocess, sys; "
    f"git = {_SYSTEM_GIT!r}; "
    "init = 'init' in sys.argv; "
    "init and subprocess.run([git, 'init', '-q', '.'], check=True, stdin=subprocess.DEVNULL); "
    "init and open('.gitignore', 'w', encoding='utf-8').write('.agentctl/\\n'); "
    f"done = subprocess.run([git, '-C', os.getcwd(), *{_CHECK_IGNORE!r}], "
    "stdin=subprocess.DEVNULL, capture_output=True); "
    "seen = {key: value for key, value in os.environ.items() if key.startswith('GIT_')}; "
    "ceiling = seen.pop('GIT_CEILING_DIRECTORIES', None); "
    "print(json.dumps({'check_ignore': done.returncode, 'git': seen, "
    "'ceiling_is_parent': ceiling == os.path.dirname(os.path.realpath(os.getcwd()))}))"
)


def _hostile_git_configuration(tmp_path: Path, source: str, helper: Path) -> dict[str, str]:
    """Configure core.fsmonitor to run `helper` through one configuration source."""
    if source == "environment":
        return {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.fsmonitor",
                "GIT_CONFIG_VALUE_0": str(helper)}
    if source == "parameters":
        return {"GIT_CONFIG_PARAMETERS": f"'core.fsmonitor'='{helper}'"}
    if source == "home":
        home = tmp_path / "home"
        home.mkdir()
        (home / ".gitconfig").write_text(f"[core]\n\tfsmonitor = {helper}\n", encoding="utf-8")
        return {"HOME": str(home)}
    raise AssertionError(source)


def _git_runs_the_helper(
    directory: Path, configuration: dict[str, str], marker: Path, init: bool
) -> bool:
    """Whether check-ignore in `directory` runs the helper, given the caller's configuration."""
    import subprocess

    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(configuration)
    directory.mkdir()
    if init:
        subprocess.run([_SYSTEM_GIT, "init", "-q", str(directory)], check=True, env=environment,
                       stdin=subprocess.DEVNULL)
        (directory / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
    marker.unlink(missing_ok=True)
    subprocess.run([_SYSTEM_GIT, "-C", str(directory), *_CHECK_IGNORE], env=environment,
                   stdin=subprocess.DEVNULL, capture_output=True, check=False)
    ran = marker.exists()
    marker.unlink(missing_ok=True)
    return ran


@pytest.mark.parametrize("source", ("environment", "parameters", "home", "enclosing"))
def test_an_edition_runs_git_with_no_configuration_but_the_case_repository(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, source: str
) -> None:
    """Git runs a helper its configuration names, by absolute path where no PATH stub sees it.

    Each source first proves, outside the harness, that check-ignore runs the helper it
    configures; then neither edition may. `enclosing` is a repository around the harness
    directory, which Git would discover from a case that has no repository of its own.
    """
    import json
    import subprocess

    if not os.access(_SYSTEM_GIT, os.X_OK):
        pytest.skip(f"{_SYSTEM_GIT}, which both editions run, is not installed")
    herdr_agent = _cross_module("herdr_agent_differential")
    marker = tmp_path / "helper-ran"
    helper = tmp_path / "fsmonitor-helper"
    helper.write_text(f"#!/bin/sh\necho \"$@\" >> {marker}\nexit 1\n", encoding="utf-8")
    helper.chmod(0o700)
    init = source != "enclosing"
    if init:
        configuration = _hostile_git_configuration(tmp_path, source, helper)
    else:
        clean = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        subprocess.run([_SYSTEM_GIT, "init", "-q", str(tmp_path)], check=True, env=clean,
                       stdin=subprocess.DEVNULL)
        subprocess.run([_SYSTEM_GIT, "-C", str(tmp_path), "config", "core.fsmonitor", str(helper)],
                       check=True, env=clean, stdin=subprocess.DEVNULL)
        configuration = {}
    assert _git_runs_the_helper(tmp_path / "control", configuration, marker, init)

    for variable, value in configuration.items():
        monkeypatch.setenv(variable, value)
    edition = [sys.executable, "-c", _GIT_PROBE_EDITION]
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("git-configuration")
        python, rust = harness.invoke(
            case, ("git-probe", "init" if init else "no-init", *herdr_agent.FIXTURE_HERDR)
        )
    finally:
        harness.close()

    assert not marker.exists()
    assert python == rust
    assert python.returncode == 0, python.stderr
    assert json.loads(python.stdout) == {
        # 128: with no repository of its own, the case is not inside any repository.
        "check_ignore": 0 if init else 128,
        "git": _CASE_GIT,
        "ceiling_is_parent": True,
    }
    assert harness.host_cli_calls() == []


def test_the_serialization_editions_run_git_with_no_configuration_but_the_case_repository(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The cross-process lock check launches its editions itself, so it must set Git up too."""
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    edition, _ = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    launched: list[tuple[object, object]] = []

    class _Launched(Exception):
        pass

    def recording_popen(*_: object, **kwargs: object) -> object:
        launched.append((kwargs.get("env"), kwargs.get("cwd")))
        raise _Launched

    try:
        monkeypatch.setenv("GIT_CONFIG_PARAMETERS", f"'core.fsmonitor'='{tmp_path / 'helper'}'")
        monkeypatch.setenv("GIT_DIR", str(tmp_path / "elsewhere"))
        monkeypatch.setattr(subprocess, "Popen", recording_popen)
        with pytest.raises(_Launched):
            herdr_agent._cross_process_serialization(harness, herdr_agent.Report())
    finally:
        monkeypatch.undo()
        harness.close()

    [(environment, directory)] = launched
    assert isinstance(environment, dict) and isinstance(directory, Path)
    assert {key: value for key, value in environment.items() if key.startswith("GIT_")} == {
        **_CASE_GIT,
        "GIT_CEILING_DIRECTORIES": os.path.dirname(os.path.realpath(directory)),
    }


def _recording_helper(tmp_path: Path) -> tuple[Path, Path]:
    """A helper program that records each run in a marker file, and that marker."""
    marker = tmp_path / "helper-ran"
    helper = tmp_path / "fsmonitor-helper"
    helper.write_text(f"#!/bin/sh\necho \"$@\" >> {marker}\nexit 1\n", encoding="utf-8")
    helper.chmod(0o700)
    return helper, marker


def _hostile_template(tmp_path: Path, helper: Path) -> Path:
    """A Git template directory whose config file names the helper, and with an exclude file."""
    template = tmp_path / "template"
    (template / "info").mkdir(parents=True)
    (template / "config").write_text(f"[core]\n\tfsmonitor = {helper}\n", encoding="utf-8")
    (template / "info" / "exclude").write_text("template-marker\n", encoding="utf-8")
    return template


def _promise_a_missing_gitignore(root: Path, helper: Path, environment: dict[str, str]) -> None:
    """Index a skip-worktree `.gitignore` whose blob only a promisor remote can supply.

    Git reads a skip-worktree `.gitignore` from the index, so check-ignore needs the blob and
    fetches it from the remote, whose upload-pack program is `helper`. The profile directory is
    ignored through info/exclude instead, so check-ignore's answer does not depend on the fetch.
    """
    import subprocess

    def git(*arguments: str, data: bytes = b"") -> str:
        done = subprocess.run([_SYSTEM_GIT, "-C", str(root), *arguments], check=True,
                              env=environment, input=data, capture_output=True)
        return done.stdout.decode("utf-8").strip()

    blob = git("hash-object", "--stdin", data=b".agentctl/\n")
    git("update-index", "--add", "--cacheinfo", f"100644,{blob},.gitignore")
    git("update-index", "--skip-worktree", ".gitignore")
    for key, value in (
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "origin"),
        ("remote.origin.url", str(root.parent / "no-remote")),
        ("remote.origin.promisor", "true"),
        ("remote.origin.uploadpack", str(helper)),
    ):
        git("config", key, value)
    (root / ".git" / "info").mkdir(exist_ok=True)
    (root / ".git" / "info" / "exclude").write_text(".agentctl/\n", encoding="utf-8")


@pytest.mark.parametrize("route", ("local", "include", "template", "lazy-fetch"))
def test_an_edition_runs_no_git_helper_that_the_case_repository_configures(
    tmp_path: Path, route: str
) -> None:
    """Both editions read the case repository's configuration, so it may not run a helper either.

    The repository exists before the editions start, as in every corpus case, and check-ignore
    with no configuration but that repository's must first run the helper. `template` is a
    repository whose `git init` copied a template's config file; `lazy-fetch` is a partial
    clone whose index names a `.gitignore` blob that is missing.
    """
    import json
    import subprocess

    if not os.access(_SYSTEM_GIT, os.X_OK):
        pytest.skip(f"{_SYSTEM_GIT}, which both editions run, is not installed")
    herdr_agent = _cross_module("herdr_agent_differential")
    helper, marker = _recording_helper(tmp_path)
    template = _hostile_template(tmp_path, helper)
    included = tmp_path / "included-config"
    included.write_text(f"[core]\n\tfsmonitor = {helper}\n", encoding="utf-8")
    clean = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    clean.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
    edition = [sys.executable, "-c", _GIT_PROBE_EDITION]
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case(f"git-repository-{route}")
        for root in (case.python_root, case.rust_root):
            if route == "template":
                subprocess.run([_SYSTEM_GIT, "init", "-q", str(root)], check=True,
                               env={**clean, "GIT_TEMPLATE_DIR": str(template)},
                               stdin=subprocess.DEVNULL)
            else:
                herdr_agent.init_case_repository(root)
            if route == "local":
                subprocess.run([_SYSTEM_GIT, "-C", str(root), "config", "core.fsmonitor",
                                str(helper)], check=True, env=clean, stdin=subprocess.DEVNULL)
            if route == "include":
                with (root / ".git" / "config").open("a", encoding="utf-8") as configuration:
                    configuration.write(f"[include]\n\tpath = {included}\n")
            if route == "lazy-fetch":
                _promise_a_missing_gitignore(root, helper, clean)
            else:
                (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
            subprocess.run([_SYSTEM_GIT, "-C", str(root), *_CHECK_IGNORE], env=clean,
                           stdin=subprocess.DEVNULL, capture_output=True, check=False)
            assert marker.exists(), f"the {route} configuration does not run the helper"
            marker.unlink()
        python, rust = harness.invoke(
            case, ("git-probe", "no-init", *herdr_agent.FIXTURE_HERDR)
        )
    finally:
        harness.close()

    assert not marker.exists()
    assert python == rust
    assert python.returncode == 0, python.stderr
    assert json.loads(python.stdout) == {
        "check_ignore": 0, "git": _CASE_GIT, "ceiling_is_parent": True,
    }
    assert harness.host_cli_calls() == []


@pytest.mark.parametrize("source", ("template-dir", "environment", "home"))
def test_a_case_repository_takes_nothing_from_a_template_the_caller_names(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, source: str
) -> None:
    """`git init` copies the caller's template directory, config file and all, into `.git`."""
    import subprocess

    if not os.access(_SYSTEM_GIT, os.X_OK):
        pytest.skip(f"{_SYSTEM_GIT}, which both editions run, is not installed")
    herdr_agent = _cross_module("herdr_agent_differential")
    helper, _ = _recording_helper(tmp_path)
    template = _hostile_template(tmp_path, helper)
    configuration: dict[str, str]
    if source == "template-dir":
        configuration = {"GIT_TEMPLATE_DIR": str(template)}
    elif source == "environment":
        configuration = {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "init.templateDir",
                         "GIT_CONFIG_VALUE_0": str(template)}
    else:
        home = tmp_path / "home"
        home.mkdir()
        (home / ".gitconfig").write_text(f"[init]\n\ttemplateDir = {template}\n",
                                         encoding="utf-8")
        configuration = {"HOME": str(home)}
    caller = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    control = tmp_path / "control"
    subprocess.run([_SYSTEM_GIT, "init", "-q", str(control)], check=True,
                   env={**caller, **configuration}, stdin=subprocess.DEVNULL)
    assert str(helper) in (control / ".git" / "config").read_text(encoding="utf-8")
    assert "template-marker" in (control / ".git" / "info" / "exclude").read_text(encoding="utf-8")

    for variable, value in configuration.items():
        monkeypatch.setenv(variable, value)
    fixture = tmp_path / "fixture"
    herdr_agent.init_case_repository(fixture)
    assert "fsmonitor" not in (fixture / ".git" / "config").read_text(encoding="utf-8")
    exclude = fixture / ".git" / "info" / "exclude"
    assert not exclude.exists() or "template-marker" not in exclude.read_text(encoding="utf-8")


def test_a_case_cannot_empty_the_editions_git_variables(tmp_path: Path) -> None:
    """`empty_environment` is applied after with_case_git, so it may not name a Git variable."""
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, ran = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("empty-git", {"empty_environment": ["GIT_NO_LAZY_FETCH"]})
        with pytest.raises(TypeError, match="Git variables"):
            harness.invoke(case, ("status", *herdr_agent.FIXTURE_HERDR))
    finally:
        harness.close()
    assert not ran.exists()


def test_the_cross_harness_runs_git_only_to_create_a_case_repository() -> None:
    """A fixture `git init` with the caller's environment would take the caller's template."""
    import ast

    def runs_git(argument: ast.expr) -> bool:
        """Whether a call's first argument is an argument vector that starts Git."""
        if not (isinstance(argument, (ast.List, ast.Tuple)) and argument.elts):
            return False
        first = argument.elts[0]
        if isinstance(first, ast.Constant) and isinstance(first.value, str):
            return os.path.basename(first.value) == "git"
        if isinstance(first, ast.Name):
            return first.id.endswith("SYSTEM_GIT")
        return isinstance(first, ast.Attribute) and first.attr.endswith("SYSTEM_GIT")

    found: list[tuple[str, str]] = []
    for path in sorted((REPO_ROOT / "cross").glob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        functions = [node for node in ast.walk(tree)
                     if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))]
        for node in ast.walk(tree):
            if isinstance(node, ast.Call) and node.args and runs_git(node.args[0]):
                enclosing = [function for function in functions
                             if function.lineno <= node.lineno <= (function.end_lineno or 0)]
                innermost = max(enclosing, key=lambda function: function.lineno, default=None)
                found.append((path.name, innermost.name if innermost else "<module>"))
    assert found == [("herdr_agent_differential.py", "init_case_repository")]


# Invocations whose refusal is checked against what the Python parsers actually select.
_PARSER_PROBES: tuple[tuple[str, ...], ...] = (
    ("start", "worker"),
    ("start", "worker", "--herdr-bin", "<HERDR>"),
    ("--herdr-bin", "<HERDR>", "start", "worker"),
    ("--herdr-bin=<HERDR>", "status", "--pane", "w1:p1"),
    ("--registry", "<ROOT>/registry", "send", "worker", "text"),
    ("send", "worker", "--", "--herdr-bin=<HERDR>"),
    ("send", "worker", "--", "--herdr-bin", "<HERDR>"),
    ("send", "--pane", "w1:p1", "--", "--herdr-bin=<HERDR>"),
    ("--", "start", "worker"),
    ("--", "start", "--help"),
    ("start", "--", "--help"),
    ("start", "--help"),
    ("start", "worker", "--herdr-bin", "<HERDR>", "--herdr-bin", "herdr"),
    ("start", "worker", "--herdr-bin", "herdr", "--herdr-bin", "<HERDR>"),
    ("--herdr-bin", "<HERDR>", "--", "start", "worker", "--herdr-bin", "herdr"),
    ("--herdr-bin", "<HERDR>", "--", "start", "worker", "--herdr-bin=/usr/local/bin/herdr"),
    ("--herdr-bin", "<HERDR>", "--", "start", "worker"),
    ("--pane", "w1:p1", "status"),
    ("status", "--pane", "w1:p1", "--herdr-bin", "<HERDR>"),
    ("--queue", "<ROOT>/queue", "userguide"),
    ("userguide", "status"),
    ("status", "userguide"),
    ("--userguide",),
    ("--version",),
    ("capabilities",),
    ("profiles", "--cwd", "<ROOT>"),
    ("quickstart",),
    ("skill", "install", "--harness", "codex"),
    ("goal", "worker", "--herdr-bin", "<HERDR>"),
    ("goal", "worker"),
    ("stop", "--help"),
    (),
)


def _parsed_herdr(parse: object, arguments: list[str]) -> tuple[str | None, bool] | None:
    """The Herdr a parser selects and whether its command is Herdr-free; None if it exits."""
    import argparse
    import contextlib
    import io

    assert callable(parse)
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        try:
            namespace = parse(arguments)
        except SystemExit:
            return None
    assert isinstance(namespace, argparse.Namespace)
    herdr = getattr(namespace, "herdr_bin", None)
    command = getattr(namespace, "command", None)
    free = bool(getattr(namespace, "userguide", False)) or command in (
        None, "capabilities", "profiles", "quickstart", "skill", "userguide",
    )
    return (herdr if isinstance(herdr, str) else None), free


@pytest.mark.parametrize("arguments", _PARSER_PROBES)
def test_an_invocation_the_guard_admits_never_selects_an_installed_herdr(
    tmp_path: Path, arguments: tuple[str, ...]
) -> None:
    """Check the syntactic guard against both Python parsers, without constructing a client.

    The Rust edition searches PATH for a bare `herdr`, where the stub catches it; the Python
    edition does not, so its parsers are the ones the guard must never under-approximate.
    """
    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import cli as agentctl_cli
    from agentctl import legacy_cli

    root = tmp_path / "case"
    root.mkdir()
    (root / "fake-herdr").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    expanded = [
        value.replace("<ROOT>", str(root)).replace("<HERDR>", str(root / "fake-herdr"))
        for value in arguments
    ]
    refusal = herdr_agent.host_cli_refusal(root, expanded)
    parsers = {
        "agentctl": agentctl_cli.parser().parse_args,
        "herdr-agent": legacy_cli._parser().parse_intermixed_args,
    }
    for name, parse in parsers.items():
        parsed = _parsed_herdr(parse, expanded)
        if parsed is None:
            continue  # a usage error, help, or version output exits before any client exists
        herdr, free = parsed
        if free:
            continue
        # Judged independently of the guard, the way client._validated_executable resolves it.
        real_root = os.path.realpath(root)
        installed = herdr is None or os.sep not in herdr or os.path.commonpath(
            (os.path.realpath(os.path.abspath(herdr)), real_root)
        ) != real_root
        assert not (installed and refusal is None), (
            f"{name} would run {herdr!r} for {arguments!r}, and the guard admitted it"
        )


@pytest.mark.parametrize(("parser_name", "arguments"), (
    ("agentctl", ("send", "worker", "--", "--herdr-bin=<HERDR>")),
    ("agentctl", ("--", "start", "worker")),
    ("agentctl", ("start", "--", "--help")),
    ("agentctl", ("--herdr-bin", "<HERDR>", "--", "start", "worker", "--herdr-bin", "herdr")),
    ("herdr-agent", ("send", "--pane", "w1:p1", "--", "--herdr-bin=<HERDR>")),
    ("herdr-agent", ("--", "start", "--help")),
    ("herdr-agent", ("--pane", "w1:p1", "status")),
))
def test_the_parser_probes_include_invocations_that_select_the_installed_herdr(
    tmp_path: Path, parser_name: str, arguments: tuple[str, ...]
) -> None:
    """The probes above are not vacuous: these reach the installed Herdr unless refused."""
    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import cli as agentctl_cli
    from agentctl import legacy_cli

    root = tmp_path / "case"
    root.mkdir()
    expanded = [value.replace("<HERDR>", str(root / "fake-herdr")) for value in arguments]
    parse = (
        agentctl_cli.parser().parse_args if parser_name == "agentctl"
        else legacy_cli._parser().parse_intermixed_args
    )
    assert arguments in _PARSER_PROBES
    assert _parsed_herdr(parse, expanded) == ("herdr", False)
    assert herdr_agent.host_cli_refusal(root, expanded) is not None


class _ClientConstructed(Exception):
    """Raised in place of a production Herdr client, so the command stops before any Herdr use."""


@pytest.mark.parametrize("suffix", (("--herdr-bin", "herdr"), ("--herdr-bin=/usr/local/bin/herdr",)))
def test_a_root_terminator_does_not_hide_a_subcommand_herdr_from_the_guard(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, suffix: tuple[str, ...]
) -> None:
    """`--herdr-bin FIXTURE -- start worker --herdr-bin herdr` reaches the client as `herdr`.

    The CLI runs for real up to the point where it constructs its HerdrClient, which is replaced
    by a recorder that raises. Process creation is blocked as well, so nothing can execute.
    """
    import contextlib
    import io
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import cli as agentctl_cli

    root = tmp_path / "case"
    root.mkdir()
    fixture = root / "fake-herdr"
    _executable_file(fixture)
    constructed: list[str] = []

    def recording_client(*, herdr_bin: str = "herdr", **_: object) -> object:
        constructed.append(herdr_bin)
        raise _ClientConstructed

    def no_process(*_: object, **__: object) -> object:
        raise AssertionError("the CLI tried to start a process")

    monkeypatch.setattr(agentctl_cli, "HerdrClient", recording_client)
    monkeypatch.setattr(subprocess, "Popen", no_process)
    arguments = [
        "--registry", str(root / "registry"), "--herdr-bin", str(fixture),
        "--", "start", "worker", *suffix,
    ]
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        with contextlib.suppress(_ClientConstructed):
            agentctl_cli.main(arguments)

    assert constructed == [suffix[-1].removeprefix("--herdr-bin=")]
    assert not herdr_agent._names_case_file(root, constructed[0])
    assert herdr_agent.host_cli_refusal(root, arguments) is not None


def test_the_chat_bridge_ignores_a_fixture_herdr_the_guard_would_otherwise_see(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Why `chat` is refused outright: its `--herdr-bin`-looking token is a harness argument.

    The bridge's launch builds a default HerdrClient, which runs the installed Herdr (see
    test_the_production_client_ignores_path_for_the_default_herdr), so a fixture path in the
    argument list redirects nothing. The launch itself is replaced, so no client is built here.
    """
    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import chat as agentctl_chat
    from agentctl import cli as agentctl_cli

    root = tmp_path / "case"
    root.mkdir()
    fixture = f"--herdr-bin={root / 'fake-herdr'} x"
    launched: list[tuple[str, ...]] = []

    def record_launch(state: Path, config_path: Path, *, harness_args: tuple[str, ...] | list[str],
                      **_: object) -> int:
        launched.append(tuple(harness_args))
        return 0

    monkeypatch.setattr(agentctl_chat, "_launch_here", record_launch)
    arguments = [
        "--registry", str(root / "registry"), "chat", "launch",
        "--config", str(root / "chat.json"), "--harness-arg", fixture,
    ]

    assert agentctl_cli.main(arguments) == 0
    assert launched == [(fixture,)]
    assert herdr_agent.host_cli_refusal(root, arguments) is not None


def test_the_unfixed_retirement_goal_query_reached_the_host_codex(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The case as it stood: a goal query with no goal command uses `codex` from PATH."""
    agentctl = _cross_module("agentctl_differential")
    ran = _ambient_tool(tmp_path, monkeypatch, "codex")
    lock_root = tmp_path / "target-locks"
    edition = _python_agentctl(lock_root)
    harness = agentctl.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("unfixed-goal-query")
        common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
        started = harness.invoke(case, (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common,
        ))
        assert [outcome.returncode for outcome in started] == [0, 0], started
        harness.invoke(case, ("goal", "worker", *common))
        report = agentctl.Report()
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    assert not ran.exists()
    # `start` held its target lock in the private root, not the account's.
    assert _private_locks_taken(lock_root)
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all('"program": "codex", "argv": ["app-server", "proxy"]' in call for call in calls)
    assert [failure.split(":", 1)[0] for failure in report.failures] == ["harness/no-host-cli"]


def test_the_retirement_goal_query_uses_the_fixture_transport(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The shipped case agrees with a PID-printing `codex` first on PATH and never runs it."""
    agentctl = _cross_module("agentctl_differential")
    ran = _ambient_tool(tmp_path, monkeypatch, "codex")
    lock_root = tmp_path / "target-locks"
    edition = _python_agentctl(lock_root)
    harness = agentctl.Harness(tmp_path / "cross", edition, edition)
    try:
        report = agentctl.Report()
        agentctl._workspace_retirement(harness, report)
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    assert not ran.exists()
    assert _private_locks_taken(lock_root)
    assert harness.host_cli_calls() == []
    assert report.failures == []
    # start, status, attach, goal-query, goal-query-native, wait, stop, and the guard.
    assert report.checks == 8


_OUTSIDE_CODEX = '["/usr/local/bin/codex","app-server","proxy"]'


def _seed(case: object, relative: str, text: str) -> None:
    """Write one file into both roots of a pair case, `<ROOT>` replaced by each root."""
    for root in (case.python_root, case.rust_root):  # type: ignore[attr-defined]
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text.replace("<ROOT>", str(root)), encoding="utf-8")


_GOAL_WORKER = ("goal", "worker", "--herdr-bin", "<HERDR>")


@pytest.mark.parametrize(("relative", "text", "arguments"), (
    ("registry/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
    # Nested storage keeps the command under `goal`.
    ("registry/worker/agent.json", f'{{"goal": {{"native_command": {_OUTSIDE_CODEX}}}}}',
     (*_GOAL_WORKER, "--registry=<ROOT>/registry")),
    ("registry/worker/agent.json", '{"goal_command": ["codex", "app-server", "proxy"]}',
     (*_GOAL_WORKER, "--state", "<ROOT>/registry")),
    ("registry/worker/agent.json", '{"goal_command": "/usr/local/bin/codex"}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
    ("registry/worker/agent.json", '{"goal_command": ["<ROOT>/../outside/codex"]}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
    # The guard reads every duplicate, whichever one an edition keeps.
    ("registry/worker/agent.json",
     f'{{"goal_command": {_OUTSIDE_CODEX}, "goal_command": ["<ROOT>/fake-herdr", "goal-rpc"]}}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
    # With no registry option, an edition reads its default under the working directory.
    (".agentctl/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}', _GOAL_WORKER),
    (".herdr-agents/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}', _GOAL_WORKER),
    # After a `--` these are goal text, so the default registry is still the one read.
    (".agentctl/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}',
     (*_GOAL_WORKER, "--", "--registry=<ROOT>/safe")),
    (".herdr-agents/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}',
     (*_GOAL_WORKER, "--", "--state", "<ROOT>/safe")),
    # A registry outside the case directory could store anything.
    ("unused.txt", "", (*_GOAL_WORKER, "--registry", "<ROOT>/../registry")),
    # A stored command must be the fixture goal transport, not just a file inside the case.
    ("registry/worker/agent.json", f'{{"goal_command": {_RUNTIME_EXECS_CODEX}}}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
    ("registry/worker/agent.json",
     f'{{"goal": {{"native_command": {_RUNTIME_EXECS_CODEX.replace("<ROOT>/", "./")}}}}}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
    ("registry/worker/agent.json", '{"goal_command": ["<ROOT>/fake-herdr", "pane", "list"]}',
     (*_GOAL_WORKER, "--registry", "<ROOT>/registry")),
))
def test_a_stored_goal_command_that_could_run_a_host_program_is_never_started(
    tmp_path: Path, relative: str, text: str, arguments: tuple[str, ...]
) -> None:
    """Without `--goal-command-json` an edition runs the command stored in the agent record."""
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("stored-goal-command")
        _seed(case, relative, text)
        python, rust = harness.invoke(case, arguments)
        report = herdr_agent.Report()
        harness.require_no_host_cli(report)
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    assert python.stderr.startswith("cross harness host CLI guard: refused to run the editions")
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all('"program": "goal-command"' in call and '"refused": ' in call for call in calls)
    assert [failure.split(":", 1)[0] for failure in report.failures] == ["harness/no-host-cli"]


@pytest.mark.parametrize("link", ("registry", "registry/worker", "registry/worker/agent.json"))
def test_a_registry_link_that_leaves_the_case_directory_is_never_started(
    tmp_path: Path, link: str
) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    outside = tmp_path / "outside"
    (outside / "registry" / "worker").mkdir(parents=True)
    (outside / "registry" / "worker" / "agent.json").write_text(
        '{"goal_command": null}', encoding="utf-8"
    )
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("linked-registry")
        for root in (case.python_root, case.rust_root):
            (root / link).parent.mkdir(parents=True, exist_ok=True)
            (root / link).symlink_to(outside / link)
        python, rust = harness.invoke(case, (*_GOAL_WORKER, "--registry", "<ROOT>/registry"))
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    assert "outside the case directory" in python.stderr
    assert len(harness.host_cli_calls()) == 2


@pytest.mark.parametrize("text", (
    '{"goal_command": ["<ROOT>/fake-herdr", "goal-rpc"]}',
    '{"goal": {"native_command": ["./fake-herdr", "goal-rpc"]}}',
    '{"goal_command": null}',
    '{"goal_command": []}',
    'not json {"goal_command": ["/usr/local/bin/codex"]}',
))
def test_a_stored_fixture_goal_command_still_runs(tmp_path: Path, text: str) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("stored-fixture-goal-command")
        _seed(case, "registry/worker/agent.json", text)
        (case.python_root / "registry" / "loop").symlink_to(case.python_root / "registry")
        python, rust = harness.invoke(case, (*_GOAL_WORKER, "--registry", "<ROOT>/registry"))
    finally:
        harness.close()

    assert (python.returncode, rust.returncode) == (0, 0)
    assert started.read_text(encoding="utf-8") == "started\n" * 2
    assert harness.host_cli_calls() == []


def test_the_interpreter_copy_in_every_case_runs_the_program_its_arguments_name(
    tmp_path: Path,
) -> None:
    """Why a goal command must be the fixture transport, not merely a file inside the case."""
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    harness = herdr_agent.Harness(tmp_path / "cross", ["true"], ["true"])
    try:
        case = harness.case("interpreter-copy")
    finally:
        harness.close()
    root = case.python_root
    runtime = root / "fake-muse-runtime"
    marker = tmp_path / "chosen-program-ran"
    completed = subprocess.run(
        [str(runtime), "-c", f"open({str(marker)!r}, 'w', encoding='utf-8').write('ran')"],
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60, check=False,
    )

    assert completed.returncode == 0, completed.stderr
    assert marker.read_text(encoding="utf-8") == "ran"
    assert herdr_agent._names_case_file(root, str(runtime))
    value = _RUNTIME_EXECS_CODEX.replace("<ROOT>", str(root))
    refusal = herdr_agent.host_cli_refusal(
        root, ["goal", "worker", "--herdr-bin", str(root / "fake-herdr"), "--goal-command-json", value]
    )
    assert refusal is not None and refusal[0] == "goal-command"


@pytest.mark.parametrize(("target", "admitted"), (("fake-herdr", True), ("fake-muse-runtime", False)))
def test_a_goal_transport_link_counts_only_when_it_reaches_the_fixture(
    tmp_path: Path, target: str, admitted: bool
) -> None:
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("linked-goal-transport")
        for root in (case.python_root, case.rust_root):
            (root / "linked-transport").symlink_to(root / target)
        explicit = harness.invoke(case, (
            *_GOAL_WORKER, "--goal-command-json", '["<ROOT>/linked-transport","goal-rpc"]',
        ))
        _seed(case, "registry/worker/agent.json",
              '{"goal_command": ["./linked-transport", "goal-rpc"]}')
        stored = harness.invoke(case, (*_GOAL_WORKER, "--registry", "<ROOT>/registry"))
    finally:
        harness.close()

    for python, rust in (explicit, stored):
        assert python == rust
        assert python.returncode == (0 if admitted else 127)
    if admitted:
        assert started.read_text(encoding="utf-8") == "started\n" * 4
        assert harness.host_cli_calls() == []
    else:
        assert not started.exists()
        assert len(harness.host_cli_calls()) == 4


@pytest.mark.parametrize("reaches_fixture", ("kernel", "python"))
def test_a_goal_transport_must_be_the_fixture_under_both_readings(
    tmp_path: Path, reaches_fixture: str
) -> None:
    """`./link/../transport` is `sub/transport` to the kernel and `./transport` to Python.

    One of the two is a link to the fixture and the other a link to the interpreter copy, both
    inside the case directory, so only the rule that both readings reach the fixture refuses it.
    """
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    kernel, python_reading = (
        ("fake-herdr", "fake-muse-runtime") if reaches_fixture == "kernel"
        else ("fake-muse-runtime", "fake-herdr")
    )
    try:
        case = harness.case("goal-transport-dotdot")
        for root in (case.python_root, case.rust_root):
            (root / "sub" / "inner").mkdir(parents=True)
            (root / "link").symlink_to(root / "sub" / "inner")
            (root / "sub" / "transport").symlink_to(root / kernel)
            (root / "transport").symlink_to(root / python_reading)
        python, rust = harness.invoke(case, (
            *_GOAL_WORKER, "--goal-command-json", '["./link/../transport","goal-rpc"]',
        ))
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    assert "is not the case's fixture goal transport" in python.stderr
    assert len(harness.host_cli_calls()) == 2


def test_the_guard_knows_every_option_either_agentctl_edition_accepts_before_the_command() -> None:
    """A global the guard did not know would hide the command (`--from-session=ID chat ...`).

    The Python root parser is read directly. The Rust parser is the clap `struct Cli` in
    rs/agentctl/src/cli.rs, read from source because these tests do not build the Rust edition;
    clap adds `--help`, `-h`, `--version` and `-V` itself.
    """
    import argparse
    import re

    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import cli as agentctl_cli

    values: dict[str, set[str]] = {"python": set(), "rust": set()}
    flags: dict[str, set[str]] = {"python": set(), "rust": {"--help", "-h", "--version", "-V"}}
    for action in agentctl_cli.parser()._actions:
        if isinstance(action, argparse._SubParsersAction) or not action.option_strings:
            continue
        (flags if action.nargs == 0 else values)["python"].update(action.option_strings)
    source = (REPO_ROOT / "rs" / "agentctl" / "src" / "cli.rs").read_text(encoding="utf-8")
    block = source.split("\nstruct Cli {\n", 1)[1].split("\n}\n", 1)[0]
    for match in re.finditer(
        r"#\[arg\((?P<attributes>[^\]]*?)\)\]\s*(?P<field>\w+):\s*(?P<type>[^,\n]+),", block
    ):
        attributes = match["attributes"]
        assert re.search(r"\blong\b", attributes), match.group(0)
        assert not re.search(r"\bshort\b|\baliases\b|long\s*=", attributes), match.group(0)
        names = {"--" + match["field"].replace("_", "-")} | {
            f"--{alias}"
            for alias in re.findall(r'\b(?:visible_)?alias\s*=\s*"([^"]+)"', attributes)
        }
        (flags if match["type"].strip() == "bool" else values)["rust"].update(names)

    assert {"--registry", "--state", "--herdr-bin"} <= values["python"]
    assert {"--state", "--agentcloud-url", "--from-session", "--agentterm-bin"} <= values["rust"]
    assert "--userguide" in flags["python"] & flags["rust"]
    known = set(herdr_agent._GLOBAL_VALUE_OPTIONS)
    assert values["python"] | values["rust"] == known
    assert flags["python"] | flags["rust"] <= herdr_agent.HERDR_FREE_COMMANDS


def test_a_stored_goal_command_reaches_the_transport_as_given_and_the_guard_refuses_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The premise behind scanning registries, through production code.

    The Python edition loads the seeded record with its own validation and, when the command line
    names no goal command, hands the stored one to the transport unchanged. Process creation is
    replaced by a recorder that raises, so nothing runs.
    """
    import json
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    from agentctl import subagents

    root = tmp_path / "case"
    fixture = root / "fake-herdr"
    record_path = root / "registry" / "worker" / "agent.json"
    record_path.parent.mkdir(parents=True)
    _executable_file(fixture)
    outside = [os.path.join(str(tmp_path), "outside", "codex"), "app-server", "proxy"]
    record_path.write_text(json.dumps({
        "schema": 1, "name": "worker", "token": "t0", "harness": "codex", "cwd": str(root),
        "lifecycle": "running", "created_at": 1.0, "arguments": [],
        "session_value": "thread-1", "goal_command": outside,
    }), encoding="utf-8")
    started: list[list[str]] = []

    def recording_popen(argv: list[str], **_: object) -> object:
        started.append(list(argv))
        raise _TransportStarted

    record = subagents.AgentRecord._from_value(
        json.loads(record_path.read_text(encoding="utf-8")), record_path, "worker"
    )
    manager = subagents.ManagedAgents(HerdrClient(herdr_bin=str(fixture)), root / "registry")
    monkeypatch.setattr(subprocess, "Popen", recording_popen)
    with pytest.raises(_TransportStarted):
        manager._goal_result(record, None)

    assert started == [outside]
    arguments = ["goal", "worker", "--herdr-bin", str(fixture), "--registry", str(root / "registry")]
    refusal = herdr_agent.host_cli_refusal(root, arguments)
    assert refusal is not None and refusal[0] == "goal-command"
    assert str(record_path) in refusal[1]


@pytest.mark.parametrize("program", (
    "/nonexistent-cross-guard/runtime",
    "python3",
    "<ROOT>/../nonexistent-cross-guard/runtime",
    "./linked-runtime",
))
def test_the_fixture_herdr_runs_no_custom_program_from_outside_the_case(
    tmp_path: Path, program: str
) -> None:
    """The fixture Herdr refuses to start a program from outside the case directory.

    An edition starts a custom harness through `pane run` on the Herdr it was given, so the program
    named there is an input the pre-spawn guard does not see. A goal command could once ask the
    fixture for it too; only the `goal-rpc` mode is admitted now.
    """
    import json
    import signal
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    edition, _ = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("fixture-child")
        root = case.python_root
        (root / "linked-runtime").symlink_to("/nonexistent-cross-guard/runtime")
        fixture = str(root / "fake-herdr")
        launch = f"{program.replace('<ROOT>', str(root))} --custom-harness"
        goal_command = json.dumps([fixture, "pane", "run", "w1:p1", launch])
        assert herdr_agent.host_cli_refusal(
            root, ["goal", "worker", "--herdr-bin", fixture, "--goal-command-json", goal_command]
        ) is not None
        refused = subprocess.run(
            [fixture, "pane", "run", "w1:p1", launch], cwd=root, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, check=False, timeout=30,
        )
        after_refusal = json.loads((root / "state.json").read_text(encoding="utf-8"))
        inside = subprocess.run(
            [fixture, "pane", "run", "w1:p1", f"{root / 'fake-muse-runtime'} --custom-harness"],
            cwd=root, stdin=subprocess.DEVNULL, capture_output=True, text=True, check=False,
            timeout=30,
        )
        child = json.loads((root / "state.json").read_text(encoding="utf-8")).get("custom_pid")
        if isinstance(child, int):
            os.kill(child, signal.SIGTERM)
    finally:
        harness.close()

    # The fixture refuses the program and records it, as a stub would.
    assert refused.returncode == 127, refused.stderr
    assert "refused to run" in refused.stderr
    calls = harness.host_cli_calls()
    assert len(calls) == 1 and '"refused": ' in calls[0]
    assert "custom_pid" not in after_refusal
    # The fixture's own runtime, inside the case directory, still starts.
    assert inside.returncode == 0, inside.stderr
    assert isinstance(child, int)


def test_no_edition_inherits_the_callers_standard_input(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`agentctl mcp` reads requests from standard input after the command line was checked."""
    import subprocess

    herdr_agent = _cross_module("herdr_agent_differential")
    edition, _ = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    inputs: list[object] = []
    real_run = subprocess.run

    def recording_run(*args: object, **kwargs: object) -> object:
        inputs.append(kwargs.get("stdin", "inherited"))
        return real_run(*args, **kwargs)  # type: ignore[call-overload]

    class _Launched(Exception):
        pass

    def recording_popen(*_: object, **kwargs: object) -> object:
        inputs.append(kwargs.get("stdin", "inherited"))
        raise _Launched

    try:
        case = harness.case("standard-input")
        monkeypatch.setattr(subprocess, "run", recording_run)
        harness.invoke(case, ("mcp", *herdr_agent.FIXTURE_HERDR))
        monkeypatch.setattr(subprocess, "Popen", recording_popen)
        with pytest.raises(_Launched):
            herdr_agent._cross_process_serialization(harness, herdr_agent.Report())
    finally:
        monkeypatch.undo()
        harness.close()

    assert inputs == [subprocess.DEVNULL] * 3


@pytest.mark.parametrize("seeded", ("registry", "nested/registry"))
def test_a_registry_is_read_under_both_readings_of_dotdot(tmp_path: Path, seeded: str) -> None:
    """`./link/../registry` is `nested/registry` to the kernel and `registry` once `..` is removed.

    The Python edition removes `..` first (os.path.abspath in ManagedAgents), so the guard must
    read both directories, each of which is inside the case.
    """
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    try:
        case = harness.case("dotdot-registry")
        for root in (case.python_root, case.rust_root):
            (root / "nested" / "deeper").mkdir(parents=True)
            (root / "link").symlink_to("nested/deeper")
            for directory in ("registry", "nested/registry"):
                (root / directory).mkdir(parents=True)
        _seed(case, f"{seeded}/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}')
        python, rust = harness.invoke(case, (*_GOAL_WORKER, "--registry", "./link/../registry"))
    finally:
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    assert "stores the goal command" in python.stderr
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all('"program": "goal-command"' in call for call in calls)


def test_a_registry_directory_that_cannot_be_listed_is_never_started(tmp_path: Path) -> None:
    """An edition opens `<registry>/<name>/agent.json` by name, which needs no listing.

    A walk that cannot list `worker` (search permission only) would otherwise skip the record.
    """
    herdr_agent = _cross_module("herdr_agent_differential")
    edition, started = _recording_edition(tmp_path)
    harness = herdr_agent.Harness(tmp_path / "cross", edition, edition)
    case = harness.case("unlistable-registry")
    workers = [root / "registry" / "worker" for root in (case.python_root, case.rust_root)]
    try:
        _seed(case, "registry/worker/agent.json", f'{{"goal_command": {_OUTSIDE_CODEX}}}')
        for worker in workers:
            worker.chmod(0o100)
        python, rust = harness.invoke(case, (*_GOAL_WORKER, "--registry", "<ROOT>/registry"))
    finally:
        for worker in workers:
            worker.chmod(0o700)
        harness.close()

    assert not started.exists()
    assert python == rust
    assert python.returncode == 127
    if os.geteuid() != 0:  # root lists the directory anyway, and then reads the command
        assert "could not be read completely" in python.stderr
    calls = harness.host_cli_calls()
    assert len(calls) == 2
    assert all('"program": "goal-command"' in call for call in calls)
