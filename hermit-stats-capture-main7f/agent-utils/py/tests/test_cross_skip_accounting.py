"""A skipped differential check is its own result, never a pass.

``cross/differential.py`` cannot run every case everywhere: some need cgroup-v2 boxing, some must
not create a live systemd scope from inside a parent-owned delegated cgroup, some need a HARD
cpuset pin, and some need two usable CPUs or, for the CPA planner's memory cap, a core budget of
eight. Such a check used to be recorded with ``Report.ok``, so a run in which none of those checks
executed printed the same ``OK - N checks`` line and exit status as a run in which they all
passed.

These tests pin the replacement contract without running the minutes-long harness:

* a skip never increments the pass count and is listed by name with a total;
* a run that claims full coverage (no ``--allow-skip``) exits nonzero when anything was skipped;
* a run that declares itself partial prints PARTIAL, not OK, and still lists every skip;
* a boxed leg that cannot be boxed is a skip, and one whose engines never announced an
  established box is a failure, never agreement;
* the cpuset-alloc delegated, HARD-refusal and one-CPU branches list exactly the checks they did
  not run, and a selftest that reports UNTESTABLE is a skip, never a counted mutation verdict;
* the CPA planner's one-core and four-core budgets list the checks they could not run, and an
  eight-core budget runs them all;
* ``--tool all`` ends with one line that is PARTIAL or FAILED whenever any tool was;
* ``scripts/validate.py`` names a partial cross node in its summary, although dagrun shows that
  node as a plain PASS;
* each lane that runs the harness states its coverage policy explicitly.
"""

from __future__ import annotations

import importlib.util
import json
import os
import stat
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from types import ModuleType, SimpleNamespace

import pytest

from dagrun.estimates import Planner, apply_plan_to_config, build_plan
from dagrun.io import dag_from_path
from dagrun.scheduler import cap_config_max_cpus

REPO_ROOT = Path(__file__).resolve().parents[2]
BOXING_LABELS = [
    "profile-timeseries",
    "operator-build-width:stated",
    "operator-build-width:unstated",
    "boxed-cpu-bandwidth",
    "jobs-env:boxed-narrow",
    "jobs-env:boxed-readonly",
]
NO_DELEGABLE_SUBTREE = "the outer scheduler has no cgroup subtree to delegate"
COVERAGE_DIR_ENV = "AGENT_UTILS_CROSS_COVERAGE_DIR"
DELEGATION_ENV = ("DAGRUN_DELEGATED_CGROUP", "DAGRUN_DELEGATED_UNBOXED", "DAGRUN_OUTER_RUN")


@pytest.fixture(autouse=True)
def _outside_github_actions(monkeypatch: pytest.MonkeyPatch) -> None:
    """Run every case as a local run, even when this suite itself runs under GitHub Actions.

    ``scripts/validate.py`` annotates a PARTIAL run and appends it to the job summary under
    Actions. Inherited, that would add this suite's fixtures to the real job's summary. The tests
    that are about the annotation set both variables themselves.
    """

    monkeypatch.delenv("GITHUB_ACTIONS", raising=False)
    monkeypatch.delenv("GITHUB_STEP_SUMMARY", raising=False)


def _differential() -> ModuleType:
    # differential.py imports its sibling modules by bare name, as the harness runs it by path.
    cross = str(REPO_ROOT / "cross")
    if cross not in sys.path:
        sys.path.insert(0, cross)
    spec = importlib.util.spec_from_file_location(
        "_cross_differential_skip_accounting", REPO_ROOT / "cross" / "differential.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _unexpected_run(*_args: object, **_kwargs: object) -> object:
    raise AssertionError("a check that must be skipped spawned an engine")


def _noop(*_args: object, **_kwargs: object) -> None:
    return None


_SHELL_STARTUP_CHANNELS = frozenset(
    {
        "BASH_COMPAT",
        "BASH_ENV",
        "BASH_XTRACEFD",
        "CDPATH",
        "ENV",
        "EXECIGNORE",
        "GLOBIGNORE",
        "PATH",
        "POSIXLY_CORRECT",
    }
)


def _jobs_env_engine(differential: ModuleType, *, boxed_allowed: bool, banner: bool) -> object:
    """Fake both engines for ``compare_jobs_env_width``; nothing else may spawn through it.

    The fake grants ``min(-j, preferred 4)`` through ``CARGO_BUILD_JOBS``, refuses the same
    channels the engines refuse, and prints the boxing banner on a boxed leg only when
    ``banner`` is set. With ``boxed_allowed`` unset, running any boxed leg fails the test.
    """

    outcome = differential.Outcome

    def fake_run(
        _cmd: object, args: tuple[str, ...] | list[str], extra_env: dict[str, str] | None = None,
        **_kw: object,
    ) -> object:
        env = dict(extra_env or {})
        if "OBSERVED_PATH" not in env or "dagrun-cross-jobs-env-" not in args[2]:
            raise AssertionError(f"only the jobs-env comparison may spawn here: {args!r}")
        boxed = "--unsafe-no-cgroups" not in args
        if boxed:
            if not boxed_allowed:
                raise AssertionError(f"a boxed jobs-env leg ran where no box can exist: {args!r}")
            assert env.get("DAGRUN_FORCE_SCOPE_ATTEMPT") == "1", env
        announced = (
            "dagrun: cgroup boxing ACTIVE in parent-owned delegated step root /fake\n"
            if boxed and banner
            else ""
        )
        channel = env.get("DAGRUN_JOBS_ENV")
        if channel is None:
            return outcome(2, "", "dagrun: no width channel is configured")
        if channel == "NOT=A=NAME":
            return outcome(2, "", "DAGRUN_JOBS_ENV must be a valid environment variable name")
        if channel in _SHELL_STARTUP_CHANNELS:
            return outcome(2, "", f"DAGRUN_JOBS_ENV={channel} is a shell startup/control variable")
        if channel != "CARGO_BUILD_JOBS":
            return outcome(1, "", f"{announced}{channel} did not retain assigned width 1")
        width = int(next(arg for arg in args if arg.startswith("-j"))[2:])
        Path(env["OBSERVED_PATH"]).write_text(str(min(width, 4)), encoding="utf-8")
        return outcome(0, "", announced)

    return fake_run


def _set_delegation(monkeypatch: pytest.MonkeyPatch, environment: dict[str, str]) -> None:
    for name in DELEGATION_ENV:
        monkeypatch.delenv(name, raising=False)
    for name, value in environment.items():
        monkeypatch.setenv(name, value)


def _dagrun_with_only_boxing_checks(
    differential: ModuleType, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Drive the real ``main`` -> ``compare_dagrun`` summary with every non-boxing case stubbed.

    The outer scheduler is made to say that it has no cgroup subtree to delegate, which is the
    state of a hosted runner started with ``--allow-cgroup-failure``. The boxing-only comparisons
    then take their real skip paths without spawning an engine. The jobs-env comparison runs its
    17 unboxed cases through a fake engine and records its two boxed legs as skips.
    """

    kept = {
        "compare_dagrun",
        "compare_profile_timeseries_trace",
        "compare_operator_build_width",
        "compare_boxed_cpu_bandwidth",
        "compare_jobs_env_width",
    }
    for name in dir(differential):
        if name.startswith("compare_") and name not in kept:
            monkeypatch.setattr(differential, name, _noop)
    for name in (
        "representative_fixtures",
        "randomized_fixtures",
        "example_fixtures",
        "yaml_fixture_paths",
    ):
        monkeypatch.setattr(differential, name, lambda *_args: [])
    monkeypatch.setattr(differential, "rs_command", lambda _tool: ["rust"])
    monkeypatch.setattr(
        differential, "run", _jobs_env_engine(differential, boxed_allowed=False, banner=False)
    )
    monkeypatch.setattr(differential, "_effective_validation_jobs", lambda: 4)
    monkeypatch.delenv("AGENT_UTILS_VALIDATION_JOBS", raising=False)
    monkeypatch.delenv("AGENT_UTILS_CROSS_ALLOW_SKIP", raising=False)
    monkeypatch.delenv("DAGRUN_DELEGATED_CGROUP", raising=False)
    monkeypatch.setenv("DAGRUN_DELEGATED_UNBOXED", "1")
    monkeypatch.setenv("DAGRUN_OUTER_RUN", "outer-run")


def test_a_skipped_check_is_not_counted_as_a_pass() -> None:
    differential = _differential()
    report = differential.Report()

    report.ok("ran")
    report.skip("did-not-run", "boxing", "no scope")

    assert report.checks == 1
    assert report.failures == []
    assert [(skip.label, skip.kind, skip.reason) for skip in report.skipped] == [
        ("did-not-run", "boxing", "no scope")
    ]
    with pytest.raises(ValueError, match="unknown skip kind"):
        report.skip("typo", "boxng", "misspelled kind")


@pytest.mark.parametrize(
    ("allowed", "status", "verdict"),
    [
        (frozenset(), 1, "cross[herdr-run]: INCOMPLETE - 3 paired checks agree"),
        (frozenset({"c-compiler"}), 0, "cross[herdr-run]: PARTIAL - 3 paired checks agree"),
    ],
)
def test_a_herdr_run_skip_counts_like_any_other_tool(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    allowed: frozenset[str],
    status: int,
    verdict: str,
) -> None:
    """The herdr-run end-to-end group without a C compiler is a listed, required skip."""
    differential = _differential()
    herdr = SimpleNamespace(
        checks=3,
        failures=[],
        notes=["a note"],
        skipped=[("e2e/all", "c-compiler", "no C compiler (cc, gcc, or clang) is on PATH")],
    )
    monkeypatch.setattr(differential, "build_herdr_run_report", lambda _py, _rs: herdr)

    assert differential.compare_herdr_run(["py"], ["rs"], allowed_skips=allowed) == status
    out = capsys.readouterr().out
    assert "NOTE [herdr-run] a note" in out
    assert "e2e/all: no C compiler (cc, gcc, or clang) is on PATH" in out
    assert verdict in out


def test_full_coverage_verdict_refuses_any_skip(capsys: pytest.CaptureFixture[str]) -> None:
    differential = _differential()
    report = differential.Report()
    report.ok("ran")
    report.skip("did-not-run", "boxing", "no scope")

    status = differential.coverage_verdict("demo", report, "1 checks agree", frozenset())
    out = capsys.readouterr().out

    assert status == 1
    assert "cross[demo]: SKIPPED/UNVERIFIED 1 check(s), not counted as passes:" in out
    assert "SKIPPED [boxing; REQUIRED] did-not-run: no scope" in out
    assert "cross[demo]: INCOMPLETE - 1 checks agree, but 1 required check(s)" in out
    assert "OK -" not in out


def test_an_allowance_covers_only_its_own_kind(capsys: pytest.CaptureFixture[str]) -> None:
    differential = _differential()
    report = differential.Report()
    report.skip("boxed", "boxing", "no scope")
    report.skip("pinned", "hard-cpuset", "refused")

    status = differential.coverage_verdict("demo", report, "0 checks agree", frozenset({"boxing"}))
    out = capsys.readouterr().out

    assert status == 1
    assert "SKIPPED [boxing; allowed] boxed: no scope" in out
    assert "SKIPPED [hard-cpuset; REQUIRED] pinned: refused" in out
    assert "did not run: pinned." in out


def test_a_run_without_skips_is_still_ok(capsys: pytest.CaptureFixture[str]) -> None:
    differential = _differential()
    report = differential.Report()
    report.ok("ran")

    assert differential.coverage_verdict("demo", report, "1 checks agree", frozenset()) == 0
    assert capsys.readouterr().out == "cross[demo]: OK - 1 checks agree\n"


def test_full_dagrun_run_with_skipped_boxing_checks_exits_nonzero(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    differential = _differential()
    _dagrun_with_only_boxing_checks(differential, monkeypatch)

    status = differential.main(["--tool", "dagrun"])
    out = capsys.readouterr().out

    assert status == 1
    assert "cross[dagrun]: SKIPPED/UNVERIFIED 6 check(s), not counted as passes:" in out
    for label in BOXING_LABELS:
        assert f"SKIPPED [boxing; REQUIRED] {label}:" in out
    assert "cross[dagrun]: INCOMPLETE - 17 checks across 0 fixtures agree" in out
    assert "cross[dagrun]: OK" not in out


@pytest.mark.parametrize(
    ("argv", "environment", "source"),
    [
        (["--allow-skip", "boxing"], {}, "--allow-skip"),
        (["--allow-skip", "multi-cpu,boxing"], {}, "--allow-skip"),
        ([], {"AGENT_UTILS_CROSS_ALLOW_SKIP": "boxing"}, "AGENT_UTILS_CROSS_ALLOW_SKIP"),
    ],
)
def test_partial_dagrun_run_says_partial_and_prints_its_skips(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    argv: list[str],
    environment: dict[str, str],
    source: str,
) -> None:
    differential = _differential()
    _dagrun_with_only_boxing_checks(differential, monkeypatch)
    for key, value in environment.items():
        monkeypatch.setenv(key, value)

    status = differential.main(["--tool", "dagrun", *argv])
    out = capsys.readouterr().out

    assert status == 0
    assert out.startswith("cross: PARTIAL-coverage run declared;")
    assert f"boxing (from {source})" in out
    assert "cross[dagrun]: SKIPPED/UNVERIFIED 6 check(s), not counted as passes:" in out
    for label in BOXING_LABELS:
        assert f"SKIPPED [boxing; allowed] {label}:" in out
    assert "cross[dagrun]: PARTIAL - 17 checks across 0 fixtures agree" in out
    assert "6 check(s) were skipped and are UNVERIFIED (allowed skips by kind: boxing 6)" in out
    assert "cross[dagrun]: OK" not in out


@pytest.mark.parametrize(
    ("argv", "environment"),
    [
        (["--allow-skip", "boxng"], {}),
        ([], {"AGENT_UTILS_CROSS_ALLOW_SKIP": "boxing,everything"}),
    ],
)
def test_an_unknown_skip_kind_is_refused(
    monkeypatch: pytest.MonkeyPatch, argv: list[str], environment: dict[str, str]
) -> None:
    differential = _differential()
    _dagrun_with_only_boxing_checks(differential, monkeypatch)
    for key, value in environment.items():
        monkeypatch.setenv(key, value)

    with pytest.raises(SystemExit) as refused:
        differential.main(["--tool", "dagrun", *argv])
    assert refused.value.code == 2


def _pin_run_engine(
    differential: ModuleType, *, hard_refusal: bool
) -> Callable[[object, tuple[str, ...]], object]:
    """Fake both engines for ``compare_pin_run``, optionally refusing every HARD pin."""

    outcome = differential.Outcome

    def fake_run(_cmd: object, args: tuple[str, ...], *_rest: object, **_kw: object) -> object:
        command = args[args.index("--") + 1 :] if "--" in args else ()
        if "--" not in args:
            return outcome(2, "", "a command is required")
        if args[2] == "0":
            return outcome(2, "", "--cores must be >= 1")
        if command[:1] == ("/definitely/missing/command",):
            return outcome(3, "", "cannot execute")
        if hard_refusal:
            return outcome(3, "", "HARD pin unavailable")
        return outcome(143 if "os.kill" in " ".join(command) else 0, "", "")

    return fake_run


def test_a_refused_hard_pin_is_parity_but_the_pin_itself_is_skipped(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    monkeypatch.delenv("DAGRUN_DELEGATED_CGROUP", raising=False)
    monkeypatch.delenv("DAGRUN_DELEGATED_UNBOXED", raising=False)
    monkeypatch.setattr(differential, "run", _pin_run_engine(differential, hard_refusal=True))
    report = differential.Report()

    differential.compare_pin_run(["python"], ["rust"], report)

    # missing-command, nonpositive, identical HARD refusal x2, missing-executable.
    assert report.checks == 5
    assert report.failures == []
    assert [(skip.label, skip.kind) for skip in report.skipped] == [
        ("pin-run:reserve-apply-release", "hard-cpuset"),
        ("pin-run:signal-status", "hard-cpuset"),
    ]


def test_a_working_hard_pin_records_no_skip(monkeypatch: pytest.MonkeyPatch) -> None:
    differential = _differential()
    monkeypatch.delenv("DAGRUN_DELEGATED_CGROUP", raising=False)
    monkeypatch.delenv("DAGRUN_DELEGATED_UNBOXED", raising=False)
    monkeypatch.setattr(differential, "run", _pin_run_engine(differential, hard_refusal=False))
    report = differential.Report()

    differential.compare_pin_run(["python"], ["rust"], report)

    assert report.checks == 5
    assert report.failures == []
    assert report.skipped == []


def test_delegated_pin_run_lists_both_live_checks_it_did_not_run(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    differential = _differential()
    monkeypatch.setenv("DAGRUN_DELEGATED_CGROUP", "/delegated")
    monkeypatch.setattr(differential, "run", _pin_run_engine(differential, hard_refusal=False))
    report = differential.Report()

    differential.compare_pin_run(["python"], ["rust"], report)

    # missing-command, nonpositive, missing-executable: the only cases that actually ran.
    assert report.checks == 3
    assert [(skip.label, skip.kind) for skip in report.skipped] == [
        ("pin-run:reserve-apply-release", "delegated-live-scope"),
        ("pin-run:signal-status", "delegated-live-scope"),
    ]


# ------------------------------------------------------------------ boxed jobs-env legs


def test_boxed_jobs_env_legs_are_skipped_without_a_delegable_subtree(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Under an unboxed outer scheduler both boxed jobs-env legs are boxing skips, not passes.

    A nested engine then honours ``DAGRUN_DELEGATED_UNBOXED=1`` and runs the step uncontained,
    so running these legs would compare two unboxed runs and count them as boxed agreement.
    """

    differential = _differential()
    _set_delegation(
        monkeypatch, {"DAGRUN_DELEGATED_UNBOXED": "1", "DAGRUN_OUTER_RUN": "outer-run"}
    )
    monkeypatch.setattr(
        differential, "run", _jobs_env_engine(differential, boxed_allowed=False, banner=False)
    )
    report = differential.Report()

    differential.compare_jobs_env_width(["python"], ["rust"], report)
    out = capsys.readouterr().out

    # 2 unboxed width legs, 3 unboxed readonly legs, 12 pre-spawn refusals.
    assert report.checks == 17
    assert report.failures == []
    assert [(skip.label, skip.kind, skip.reason) for skip in report.skipped] == [
        ("jobs-env:boxed-narrow", "boxing", NO_DELEGABLE_SUBTREE),
        ("jobs-env:boxed-readonly", "boxing", NO_DELEGABLE_SUBTREE),
    ]
    assert (
        "cross[dagrun]: SKIP boxed jobs-env differential (jobs-env:boxed-narrow): "
        f"{NO_DELEGABLE_SUBTREE}"
    ) in out
    assert (
        "cross[dagrun]: SKIP boxed readonly jobs-env differential (jobs-env:boxed-readonly): "
        f"{NO_DELEGABLE_SUBTREE}"
    ) in out


@pytest.mark.parametrize(
    "environment",
    [
        pytest.param({}, id="top-level"),
        pytest.param(
            {"DAGRUN_DELEGATED_CGROUP": "/delegated", "DAGRUN_OUTER_RUN": "outer-run"},
            id="delegated-cgroup",
        ),
    ],
)
def test_boxed_jobs_env_legs_count_only_when_both_engines_boxed(
    monkeypatch: pytest.MonkeyPatch, environment: dict[str, str]
) -> None:
    """Where a box can exist the boxed legs run, and agreement counts only with the banner."""

    differential = _differential()
    _set_delegation(monkeypatch, environment)

    monkeypatch.setattr(
        differential, "run", _jobs_env_engine(differential, boxed_allowed=True, banner=True)
    )
    boxed = differential.Report()
    differential.compare_jobs_env_width(["python"], ["rust"], boxed)
    assert (boxed.checks, boxed.failures, boxed.skipped) == (19, [], [])

    monkeypatch.setattr(
        differential, "run", _jobs_env_engine(differential, boxed_allowed=True, banner=False)
    )
    unboxed = differential.Report()
    differential.compare_jobs_env_width(["python"], ["rust"], unboxed)
    assert unboxed.checks == 19
    assert unboxed.skipped == []
    assert [failure.split(":", 2)[:2] for failure in unboxed.failures] == [
        ["jobs-env", "boxed-narrow"],
        ["jobs-env", "boxed-readonly"],
    ]
    for failure in unboxed.failures:
        assert "a boxed leg must run boxed; no 'cgroup boxing ACTIVE' from ['py', 'rs']" in failure


@pytest.mark.parametrize("banner", [True, False])
def test_boxed_build_width_legs_count_only_when_both_engines_boxed(
    monkeypatch: pytest.MonkeyPatch, banner: bool
) -> None:
    differential = _differential()
    _set_delegation(monkeypatch, {})
    outcome = differential.Outcome
    announced = "dagrun: cgroup boxing ACTIVE (two-level cgroup-v2 scope)\n" if banner else ""

    def fake_run(
        _cmd: object, _args: object, extra_env: dict[str, str] | None = None, **_kw: object
    ) -> object:
        stated = (extra_env or {}).get("CARGO_BUILD_JOBS")
        sentence = (
            f"honouring CARGO_BUILD_JOBS={stated}"
            if stated
            else "no CARGO_BUILD_JOBS in the environment; derived 16"
        )
        return outcome(0, f"WIDTH={stated or 2}\n", f"{announced}build width: {sentence}\n")

    monkeypatch.setattr(differential, "run", fake_run)
    report = differential.Report()

    differential.compare_operator_build_width(["python"], ["rust"], report)

    assert report.checks == 2
    assert report.skipped == []
    if banner:
        assert report.failures == []
    else:
        assert [failure.split(":", 2)[:2] for failure in report.failures] == [
            ["operator-build-width", "stated"],
            ["operator-build-width", "unstated"],
        ]


# ------------------------------------------------------------------ cpuset-alloc branches

_CPUSET_HELP = "run status reclaim selftest --cores --tag --sample-s --max-irq-rate --ledger\n"
_CPUSET_SAMPLE_REFUSAL = "--sample-s must be > 0 when --max-irq-rate is set"
_CPUSET_HARD_REFUSAL = "cpuset-alloc: HARD cpuset pin is unavailable on this host"
_CPUSET_COMMON_CHECKS = 53
_CPUSET_LIVE_LABELS = [
    "selftest:mutation-verdict",
    "interop:py-then-rs",
    "interop:rs-then-py",
    "run:wrapped-help-passthrough",
    "run:signal-status",
]


_SELFTEST_NO_SPARE_CORE = {"verdict": "UNTESTABLE", "reason": "no core outside the reserved set"}
_SELFTEST_NO_SCOPE = {"verdict": "UNTESTABLE", "reason": "systemd-run --user --scope unavailable"}


def _cpuset_engine(
    differential: ModuleType,
    *,
    hard_refusal: bool,
    selftest: tuple[int, dict[str, str]] | None = None,
) -> object:
    """Fake both cpuset-alloc engines identically, optionally refusing every HARD pin.

    ``selftest`` prints the verdicts the real engines print. By default a host that refuses HARD
    pins reports SOFT_OR_INERT, a one-CPU host has no core to escape to and reports UNTESTABLE,
    and any other host reports HARD; ``selftest`` overrides that with an exit status and a verdict.

    The ledger cases read the real files the comparison writes; a FIFO is recognised by ``stat``
    and never opened, so the non-blocking refusal case cannot hang the test.
    """

    outcome = differential.Outcome

    def selftest_verdict() -> tuple[int, dict[str, str]]:
        if selftest is not None:
            return selftest
        if hard_refusal:
            return 1, {"verdict": "SOFT_OR_INERT"}
        if len(os.sched_getaffinity(0)) < 2:
            return 3, _SELFTEST_NO_SPARE_CORE
        return 0, {"verdict": "HARD"}

    def ledger(subcommand: str, path: str) -> object:
        if not os.path.lexists(path):
            return outcome(0, json.dumps({"reservations": []}), "")
        if stat.S_ISFIFO(os.stat(path).st_mode):
            return outcome(3, "", "refusing a ledger that is not a regular file")
        try:
            payload = json.loads(Path(path).read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            return outcome(3, "", "corrupt ledger")
        reservations = payload.get("reservations", [])
        if [record.get("tag") for record in reservations] not in (["cross-live"], ["dead"]):
            return outcome(3, "", "invalid reservation record")
        key = "reclaimed" if subcommand == "reclaim" else "reservations"
        return outcome(0, json.dumps({key: reservations}), "")

    def fake_run(_cmd: object, args: tuple[str, ...], *_rest: object, **_kw: object) -> object:
        args = tuple(args)
        if args == ("--version",):
            return outcome(0, "cpuset-alloc 1.0.0\n", "")
        if args in ((), ("--help",)) or args[1:] == ("--help",):
            return outcome(0, _CPUSET_HELP, "")
        if args[0] in ("status", "reclaim"):
            if len(args) != 3 or args[1] != "--ledger":
                return outcome(2, "", "usage error")
            return ledger(args[0], args[2])
        if args[0] == "selftest":
            if "--tag" in args:
                return outcome(2, "", "unrecognized arguments: --tag")
            if "--max-irq-rate" in args:
                return outcome(2, "", _CPUSET_SAMPLE_REFUSAL)
            returncode, verdict = selftest_verdict()
            return outcome(returncode, json.dumps(verdict), "")
        if args[0] == "run":
            if "--" not in args:
                return outcome(2, "", "a command after -- is required")
            options = args[1 : args.index("--")]
            command = args[args.index("--") + 1 :]
            if "--max-irq-rate" in options:
                return outcome(2, "", _CPUSET_SAMPLE_REFUSAL)
            if options[:2] == ("--cores", "0"):
                return outcome(2, "", "--cores must be >= 1")
            if "--sample-s" in options:
                return outcome(2, "", "--sample-s must be finite and >= 0")
            if command == ("/definitely/missing/command",):
                return outcome(3, "", "cpuset-alloc: cannot execute /definitely/missing/command")
            if hard_refusal:
                return outcome(3, "", _CPUSET_HARD_REFUSAL)
            if command[-1:] == ("--help",):
                return outcome(0, "--help\n", "")
            if "os.kill" in " ".join(command):
                return outcome(143, "", "")
            return outcome(0, "", 'cpuset-alloc: reserved {"cores":[1],"count":1}')
        if args == ("not-a-command",):
            return outcome(2, "", "invalid choice")
        raise AssertionError(f"unexpected cpuset-alloc invocation {args!r}")

    return fake_run


class _HardRefusingPopen:
    """The interop pair's first command, launched directly, refusing its HARD pin."""

    def __init__(self, _argv: object, **_kw: object) -> None:
        self.returncode: int | None = None

    def poll(self) -> int:
        self.returncode = 3
        return 3

    def communicate(self, timeout: float | None = None) -> tuple[str, str]:
        self.returncode = 3
        return "", _CPUSET_HARD_REFUSAL

    def kill(self) -> None:
        return None


def _cpuset_alloc_run(
    differential: ModuleType,
    monkeypatch: pytest.MonkeyPatch,
    *,
    hard_refusal: bool,
    cpus: int,
    environment: dict[str, str],
    allowed: frozenset[str],
    selftest: tuple[int, dict[str, str]] | None = None,
) -> tuple[int, str, int | None, list[tuple[str, str]]]:
    """Run the real ``compare_cpuset_alloc`` against fake engines.

    Returns the exit status, the recorded verdict, the number of checks that ran, and each skipped
    check's label and kind, in order.
    """

    _set_delegation(monkeypatch, environment)
    monkeypatch.setattr(differential, "py_command_for", lambda _tool: ["python-engine"])
    monkeypatch.setattr(differential, "rs_command", lambda _tool: ["rust-engine"])
    monkeypatch.setattr(
        differential,
        "run",
        _cpuset_engine(differential, hard_refusal=hard_refusal, selftest=selftest),
    )
    monkeypatch.setattr(
        differential,
        "subprocess",
        SimpleNamespace(
            Popen=_HardRefusingPopen if hard_refusal else _unexpected_run,
            PIPE=subprocess.PIPE,
            TimeoutExpired=subprocess.TimeoutExpired,
        ),
    )
    monkeypatch.setattr(os, "sched_getaffinity", lambda _pid: set(range(cpus)))

    status = differential.compare_cpuset_alloc(allowed_skips=allowed)

    assert len(differential._COVERAGE) == 1
    recorded = differential._COVERAGE[0]
    skipped = [(str(skip.label), str(skip.kind)) for skip in recorded.skipped]
    return int(status), str(recorded.verdict), recorded.checks, skipped


@pytest.mark.parametrize("cpus", [1, 4])
@pytest.mark.parametrize(
    "environment",
    [
        pytest.param({"DAGRUN_DELEGATED_CGROUP": "/delegated"}, id="delegated-cgroup"),
        pytest.param(
            {"DAGRUN_DELEGATED_UNBOXED": "1", "DAGRUN_OUTER_RUN": "outer-run"},
            id="delegated-unboxed",
        ),
    ],
)
def test_delegated_cpuset_alloc_lists_its_five_live_scope_checks(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    cpus: int,
    environment: dict[str, str],
) -> None:
    """Inside a delegated root no live scope runs, on one CPU or many; nothing else is skipped."""

    differential = _differential()
    status, verdict, checks, skipped = _cpuset_alloc_run(
        differential,
        monkeypatch,
        hard_refusal=False,
        cpus=cpus,
        environment=environment,
        allowed=frozenset({"delegated-live-scope"}),
    )
    out = capsys.readouterr().out

    assert status == 0
    assert "DIVERGENCE" not in out
    assert verdict == "PARTIAL"
    assert checks == _CPUSET_COMMON_CHECKS
    assert skipped == [(label, "delegated-live-scope") for label in _CPUSET_LIVE_LABELS]
    assert (
        "cross[cpuset-alloc]: PARTIAL - 53 behavioral and ledger-schema checks agree; "
        "5 check(s) were skipped and are UNVERIFIED (allowed skips by kind: delegated-live-scope 5)"
    ) in out


def test_a_refused_hard_cpuset_pin_skips_the_four_pinned_checks(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Identical HARD refusals are parity, but the pinned behaviour itself did not run."""

    differential = _differential()
    status, verdict, checks, skipped = _cpuset_alloc_run(
        differential,
        monkeypatch,
        hard_refusal=True,
        cpus=4,
        environment={},
        allowed=frozenset(),
    )
    out = capsys.readouterr().out

    assert status == 1
    assert "DIVERGENCE" not in out
    assert verdict == "INCOMPLETE"
    # The common checks, the selftest verdict, and one identical-refusal check for each of the two
    # interop pairs, the wrapped --help and the signal status.
    assert checks == _CPUSET_COMMON_CHECKS + 5
    assert skipped == [
        ("interop:py-then-rs", "hard-cpuset"),
        ("interop:rs-then-py", "hard-cpuset"),
        ("run:wrapped-help-passthrough", "hard-cpuset"),
        ("run:signal-status", "hard-cpuset"),
    ]
    assert (
        "cross[cpuset-alloc]: INCOMPLETE - 58 behavioral and ledger-schema checks agree, but 4 "
        "required check(s) did not run: interop:py-then-rs, interop:rs-then-py, "
        "run:wrapped-help-passthrough, run:signal-status."
    ) in out


def test_one_cpu_cpuset_alloc_skips_the_selftest_and_the_two_interop_pairs(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The mutation probe and two disjoint reservations need two CPUs; the rest runs on one.

    On one CPU both engines' selftest reports UNTESTABLE because no core is left to escape to.
    That agreement is one parity check, and the mutation verdict itself is a multi-cpu skip.
    """

    differential = _differential()
    status, verdict, checks, skipped = _cpuset_alloc_run(
        differential,
        monkeypatch,
        hard_refusal=False,
        cpus=1,
        environment={},
        allowed=frozenset({"multi-cpu"}),
    )
    out = capsys.readouterr().out

    assert status == 0
    assert "DIVERGENCE" not in out
    assert verdict == "PARTIAL"
    # The common checks, the agreed UNTESTABLE refusal, the wrapped --help and the signal status.
    assert checks == _CPUSET_COMMON_CHECKS + 3
    assert skipped == [
        ("selftest:mutation-verdict", "multi-cpu"),
        ("interop:py-then-rs", "multi-cpu"),
        ("interop:rs-then-py", "multi-cpu"),
    ]
    assert (
        "cross[cpuset-alloc]: PARTIAL - 56 behavioral and ledger-schema checks agree; "
        "3 check(s) were skipped and are UNVERIFIED (allowed skips by kind: multi-cpu 3)"
    ) in out


def test_a_host_without_a_user_scope_skips_the_selftest_as_boxing(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Without ``systemd-run --user --scope`` the mutation probe never runs and no pin is HARD.

    Both engines' selftest reports UNTESTABLE for that reason, so the mutation verdict is a boxing
    skip beside the four hard-cpuset skips, and a full-coverage run ends INCOMPLETE naming it.
    """

    differential = _differential()
    status, verdict, checks, skipped = _cpuset_alloc_run(
        differential,
        monkeypatch,
        hard_refusal=True,
        cpus=4,
        environment={},
        allowed=frozenset(),
        selftest=(3, _SELFTEST_NO_SCOPE),
    )
    out = capsys.readouterr().out

    assert status == 1
    assert "DIVERGENCE" not in out
    assert verdict == "INCOMPLETE"
    # The common checks, the agreed UNTESTABLE refusal, and one identical-refusal check for each
    # of the two interop pairs, the wrapped --help and the signal status.
    assert checks == _CPUSET_COMMON_CHECKS + 5
    assert skipped == [
        ("selftest:mutation-verdict", "boxing"),
        ("interop:py-then-rs", "hard-cpuset"),
        ("interop:rs-then-py", "hard-cpuset"),
        ("run:wrapped-help-passthrough", "hard-cpuset"),
        ("run:signal-status", "hard-cpuset"),
    ]
    assert (
        "cross[cpuset-alloc]: INCOMPLETE - 58 behavioral and ledger-schema checks agree, but 5 "
        "required check(s) did not run: selftest:mutation-verdict, interop:py-then-rs, "
        "interop:rs-then-py, run:wrapped-help-passthrough, run:signal-status."
    ) in out


def _selftest_outcome(differential: ModuleType, returncode: int, result: object) -> object:
    """One engine's ``selftest`` outcome: its exit status and the JSON it printed, if any."""

    stdout = "" if result is None else json.dumps(result, indent=2)
    return differential.Outcome(returncode, stdout, "")


@pytest.mark.parametrize(
    ("result", "kind"),
    [
        pytest.param(_SELFTEST_NO_SCOPE, "boxing", id="no-user-scope"),
        pytest.param(_SELFTEST_NO_SPARE_CORE, "multi-cpu", id="no-spare-core"),
    ],
)
def test_an_agreed_untestable_selftest_is_a_skip_not_a_check(
    capsys: pytest.CaptureFixture[str], result: dict[str, str], kind: str
) -> None:
    """UNTESTABLE means the mutation probe never ran: parity of the refusal, and a skip."""

    differential = _differential()
    report = differential.Report()
    engine = _selftest_outcome(differential, 3, result)

    differential._record_selftest_verdict(report, engine, engine)

    assert report.failures == []
    assert report.checks == 1
    assert [(skip.label, skip.kind) for skip in report.skipped] == [
        ("selftest:mutation-verdict", kind)
    ]
    assert report.skipped[0].reason == f"both engines reported UNTESTABLE: {result['reason']}"

    status = differential.coverage_verdict("cpuset-alloc", report, "1 checks agree", frozenset())
    out = capsys.readouterr().out
    assert status == 1
    assert (
        "cross[cpuset-alloc]: INCOMPLETE - 1 checks agree, but 1 required check(s) did not run: "
        "selftest:mutation-verdict."
    ) in out

    allowed = frozenset({kind})
    assert differential.coverage_verdict("cpuset-alloc", report, "1 checks agree", allowed) == 0
    assert "cross[cpuset-alloc]: PARTIAL - 1 checks agree;" in capsys.readouterr().out


@pytest.mark.parametrize(
    ("returncode", "verdict"), [pytest.param(0, "HARD"), pytest.param(1, "SOFT_OR_INERT")]
)
def test_an_agreed_probe_verdict_is_a_check(returncode: int, verdict: str) -> None:
    """HARD and SOFT_OR_INERT come from a probe that ran; agreement on either is a check."""

    differential = _differential()
    report = differential.Report()
    engine = _selftest_outcome(differential, returncode, {"verdict": verdict})

    differential._record_selftest_verdict(report, engine, engine)

    assert (report.checks, report.failures, report.skipped) == (1, [], [])


@pytest.mark.parametrize(
    ("py", "rs", "message"),
    [
        pytest.param(
            (3, {"verdict": "ERROR", "reason": "scope probe produced no result"}),
            (3, {"verdict": "ERROR", "reason": "scope probe produced no result"}),
            "both engines reported ERROR",
            id="agreed-error",
        ),
        pytest.param(
            (3, {"verdict": "ERROR", "reason": "scope probe timed out"}),
            (3, {"verdict": "ERROR", "reason": "scope probe produced no result"}),
            "both engines reported ERROR",
            id="agreed-error-different-reasons",
        ),
        pytest.param(
            (3, _SELFTEST_NO_SCOPE),
            (3, _SELFTEST_NO_SPARE_CORE),
            "UNTESTABLE with a differing or unrecognised reason",
            id="untestable-reasons-differ",
        ),
        pytest.param(
            (3, {"verdict": "UNTESTABLE", "reason": "cosmic rays"}),
            (3, {"verdict": "UNTESTABLE", "reason": "cosmic rays"}),
            "UNTESTABLE with a differing or unrecognised reason",
            id="untestable-reason-unrecognised",
        ),
        pytest.param(
            (3, {"verdict": "UNTESTABLE"}),
            (3, {"verdict": "UNTESTABLE"}),
            "UNTESTABLE with a differing or unrecognised reason",
            id="untestable-without-reason",
        ),
        pytest.param(
            (0, {"verdict": "HARD"}),
            (1, {"verdict": "SOFT_OR_INERT"}),
            "py=0",
            id="verdicts-differ",
        ),
        pytest.param(
            (3, {"verdict": "HARD"}), (3, {"verdict": "HARD"}), "py=3", id="hard-with-exit-3"
        ),
        pytest.param(
            (0, _SELFTEST_NO_SPARE_CORE),
            (0, _SELFTEST_NO_SPARE_CORE),
            "py=0",
            id="untestable-with-exit-0",
        ),
        pytest.param((3, None), (3, None), "py=3", id="reservation-failed-before-the-probe"),
        pytest.param(
            (0, {"verdict": "MAYBE"}), (0, {"verdict": "MAYBE"}), "py=0", id="unknown-verdict"
        ),
    ],
)
def test_a_selftest_without_an_agreed_result_is_a_failure(
    py: tuple[int, object], rs: tuple[int, object], message: str
) -> None:
    """An ERROR, a mismatched exit status, or an UNTESTABLE the harness cannot classify fails."""

    differential = _differential()
    report = differential.Report()

    differential._record_selftest_verdict(
        report, _selftest_outcome(differential, *py), _selftest_outcome(differential, *rs)
    )

    assert report.skipped == []
    assert report.checks == 1
    assert len(report.failures) == 1
    assert report.failures[0].startswith(f"selftest:mutation-verdict: {message}")


# ------------------------------------------------------------------ CPA planner core budgets

_CPA_ALL_CHECKS = [
    "cpa:text",
    "cpa:json",
    "cpa:widened",
    "cpa:modeled-ge-lower-bound",
    "cpa:beats-fixed",
    "cpa:mem-byte-identical",
    "cpa:mem-capped",
    "cpa:infeasible-fixed-width",
    "cpa:intentional-skip-zero-demand",
    "cpa:self-managed-curve-source",
]
_CPA_ONE_CORE_SKIPS = [
    ("cpa:widened", "multi-cpu"),
    ("cpa:beats-fixed", "multi-cpu"),
    ("cpa:mem-capped", "eight-cpu"),
]


def _cpa_engine(
    differential: ModuleType,
    *,
    core_budget: int,
    capped_budget: int,
    throttled: bool,
    capped_stdout: str | None,
) -> object:
    """Fake both engines identically for ``compare_cpa_planner``; nothing else may spawn.

    ``core_budget`` is the budget the main plan reports and ``capped_budget`` the one the two
    memory-store plans report, so each budget gate can be driven on its own. A plan widens only
    as far as its budget allows. With ``throttled`` set, a capped plan with at least eight cores
    narrows ``m.heavy`` and reports ``mem-capped``; unset, the cap has no effect, which the
    comparison must report as a failure. ``capped_stdout`` replaces the capped plan's output.
    """

    outcome = differential.Outcome

    def plan(dag: str, args: tuple[str, ...]) -> str:
        if dag == "dag.json":
            if "text" in args:
                return f"cpa plan, core budget {core_budget}\n"
            wide = core_budget >= 2
            return json.dumps(
                {
                    "allocation": {
                        "core_budget": core_budget,
                        "modeled_makespan_s": 40.0 if wide else 58.0,
                        "lower_bound_s": 30.0 if wide else 58.0,
                    },
                    "steps": [{"tag": "c.build", "alloc_inner_jobs": min(core_budget, 4)}],
                }
            )
        if dag == "mdag.json":
            free = min(capped_budget, 8)
            if "--max-mem" not in args:
                width, reason = free, "core-budget"
            elif capped_stdout is not None:
                return capped_stdout
            elif throttled and capped_budget >= 8:
                width, reason = 4, "mem-capped"
            else:
                width, reason = free, "core-budget"
            return json.dumps(
                {
                    "allocation": {"core_budget": capped_budget, "stop_reason": reason},
                    "steps": [{"tag": "m.heavy", "alloc_inner_jobs": width}],
                }
            )
        if dag == "fixed-width.json":
            return json.dumps(
                {
                    "allocation": {
                        "stop_reason": "infeasible-fixed-width",
                        "modeled_makespan_s": "inf",
                    },
                    "steps": [{"tag": "f.fixed", "alloc_inner_jobs": None}],
                }
            )
        live = {"tag": "c.build", "alloc_inner_jobs": 1}
        if dag == "skip-control.json":
            return json.dumps({"steps": [live]})
        if dag == "skip-present.json":
            skipped = {
                "tag": "c.skipped",
                "est_duration_s": "0.000",
                "est_source": "skip",
                "alloc_inner_jobs": None,
            }
            return json.dumps({"steps": [skipped, live]})
        if dag == "fixed-curve-source.json":
            return json.dumps(
                {
                    "steps": [
                        {
                            "tag": "c.build",
                            "est_duration_s": "20.000",
                            "est_source": "store",
                            "alloc_inner_jobs": None,
                        },
                        {
                            "tag": "c.test",
                            "est_duration_s": "8.000",
                            "est_source": "store",
                            "alloc_inner_jobs": None,
                        },
                    ]
                }
            )
        raise AssertionError(f"unexpected CPA plan DAG {dag!r}")

    def fake_run(
        _cmd: object, args: tuple[str, ...] | list[str], *_rest: object, **_kw: object
    ) -> object:
        args = tuple(args)
        if args[:1] != ("plan",) or "--dag" not in args:
            raise AssertionError(f"only the CPA planner comparison may spawn here: {args!r}")
        return outcome(0, plan(Path(args[args.index("--dag") + 1]).name, args), "")

    return fake_run


def _cpa_run(
    monkeypatch: pytest.MonkeyPatch,
    argv: list[str],
    *,
    core_budget: int,
    capped_budget: int,
    throttled: bool = True,
    capped_stdout: str | None = None,
    environment: dict[str, str] | None = None,
) -> tuple[int, str, int | None, list[tuple[str, str]]]:
    """Drive the real ``main`` -> ``compare_dagrun`` path with only the CPA comparison live.

    The run is a top-level one with no outer scheduler and, unless ``environment`` or ``argv``
    declares one, no allowance, so only the CPA planner's own budget gates can produce a skip.
    Returns the exit status, the recorded verdict, the checks that ran, and each skip's label
    and kind, in order.
    """

    differential = _differential()
    for name in dir(differential):
        if name.startswith("compare_") and name not in {"compare_dagrun", "compare_cpa_planner"}:
            monkeypatch.setattr(differential, name, _noop)
    for name in (
        "representative_fixtures",
        "randomized_fixtures",
        "example_fixtures",
        "yaml_fixture_paths",
    ):
        monkeypatch.setattr(differential, name, lambda *_args: [])
    monkeypatch.setattr(differential, "rs_command", lambda _tool: ["rust"])
    monkeypatch.setattr(
        differential,
        "run",
        _cpa_engine(
            differential,
            core_budget=core_budget,
            capped_budget=capped_budget,
            throttled=throttled,
            capped_stdout=capped_stdout,
        ),
    )
    monkeypatch.setattr(differential, "_effective_validation_jobs", lambda: 4)
    monkeypatch.delenv("AGENT_UTILS_VALIDATION_JOBS", raising=False)
    monkeypatch.delenv("AGENT_UTILS_CROSS_ALLOW_SKIP", raising=False)
    _set_delegation(monkeypatch, {})
    for key, value in (environment or {}).items():
        monkeypatch.setenv(key, value)

    status = differential.main(["--tool", "dagrun", *argv])

    assert len(differential._COVERAGE) == 1
    recorded = differential._COVERAGE[0]
    skipped = [(str(skip.label), str(skip.kind)) for skip in recorded.skipped]
    return int(status), str(recorded.verdict), recorded.checks, skipped


@pytest.mark.parametrize("capped_budget", [1, 4])
def test_a_one_core_cpa_budget_skips_the_three_checks_it_cannot_run(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], capped_budget: int
) -> None:
    """With one core no plan can widen, so the widening, beats-fixed and memory-cap checks did
    not run. They used to be counted as passes; without an allowance the run is INCOMPLETE."""

    status, verdict, checks, skipped = _cpa_run(
        monkeypatch, [], core_budget=1, capped_budget=capped_budget
    )
    out = capsys.readouterr().out

    assert (status, verdict) == (1, "INCOMPLETE")
    assert checks == len(_CPA_ALL_CHECKS) - 3 == 7
    assert skipped == _CPA_ONE_CORE_SKIPS
    assert "cross[dagrun]: SKIPPED/UNVERIFIED 3 check(s), not counted as passes:" in out
    assert "SKIPPED [multi-cpu; REQUIRED] cpa:widened: the core budget is 1;" in out
    assert "SKIPPED [multi-cpu; REQUIRED] cpa:beats-fixed: the core budget is 1;" in out
    assert (
        f"SKIPPED [eight-cpu; REQUIRED] cpa:mem-capped: the core budget is {capped_budget};"
    ) in out
    assert "cross[dagrun]: INCOMPLETE - 7 checks across 0 fixtures agree" in out
    assert "3 required check(s) did not run: cpa:widened, cpa:beats-fixed, cpa:mem-capped." in out
    assert "DIVERGENCE" not in out
    assert "cross[dagrun]: OK" not in out


def test_a_one_core_cpa_run_is_partial_only_when_both_kinds_are_declared(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    result = _cpa_run(
        monkeypatch, ["--allow-skip", "multi-cpu,eight-cpu"], core_budget=1, capped_budget=1
    )
    out = capsys.readouterr().out

    assert result == (0, "PARTIAL", 7, _CPA_ONE_CORE_SKIPS)
    assert "cross[dagrun]: PARTIAL - 7 checks across 0 fixtures agree" in out
    assert (
        "3 check(s) were skipped and are UNVERIFIED "
        "(allowed skips by kind: eight-cpu 1, multi-cpu 2)"
    ) in out

    # One of the two kinds is not enough: the multi-cpu skips stay required.
    result = _cpa_run(monkeypatch, ["--allow-skip", "eight-cpu"], core_budget=1, capped_budget=1)
    out = capsys.readouterr().out

    assert result == (1, "INCOMPLETE", 7, _CPA_ONE_CORE_SKIPS)
    assert "SKIPPED [eight-cpu; allowed] cpa:mem-capped:" in out
    assert "2 required check(s) did not run: cpa:widened, cpa:beats-fixed." in out


def test_a_four_core_cpa_budget_skips_only_the_memory_cap(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Four cores can widen, but the 5G cap binds only once m.heavy could reach eight."""

    result = _cpa_run(monkeypatch, [], core_budget=4, capped_budget=4)
    out = capsys.readouterr().out

    assert result == (1, "INCOMPLETE", 9, [("cpa:mem-capped", "eight-cpu")])
    assert "SKIPPED [eight-cpu; REQUIRED] cpa:mem-capped: the core budget is 4;" in out
    assert "cross[dagrun]: INCOMPLETE - 9 checks across 0 fixtures agree" in out
    assert "1 required check(s) did not run: cpa:mem-capped." in out

    # The hosted lanes declare the kind through the environment.
    result = _cpa_run(
        monkeypatch,
        [],
        core_budget=4,
        capped_budget=4,
        environment={"AGENT_UTILS_CROSS_ALLOW_SKIP": "boxing,eight-cpu"},
    )
    out = capsys.readouterr().out

    assert result == (0, "PARTIAL", 9, [("cpa:mem-capped", "eight-cpu")])
    assert "cross[dagrun]: PARTIAL - 9 checks across 0 fixtures agree" in out
    assert "1 check(s) were skipped and are UNVERIFIED (allowed skips by kind: eight-cpu 1)" in out


def test_an_eight_core_cpa_budget_runs_every_check(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    result = _cpa_run(monkeypatch, [], core_budget=8, capped_budget=8)
    out = capsys.readouterr().out

    assert result == (0, "OK", len(_CPA_ALL_CHECKS), [])
    assert "cross[dagrun]: OK - 10 checks across 0 fixtures agree" in out
    assert "SKIPPED" not in out


def test_an_eight_core_memory_cap_that_does_not_bind_is_a_failure(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    result = _cpa_run(monkeypatch, [], core_budget=8, capped_budget=8, throttled=False)
    out = capsys.readouterr().out

    # A failed check is counted among the checks that ran; nothing was skipped.
    assert result == (1, "FAILED", len(_CPA_ALL_CHECKS), [])
    assert "DIVERGENCE [cpa:mem-capped: expected --max-mem to throttle m.heavy below the " in out
    assert "cross[dagrun]: 1 divergence(s) out of 10 checks" in out


def test_an_unreadable_capped_allocation_is_a_failure_not_a_skip(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """An allocation that cannot be read says nothing about the core budget, so it cannot be
    excused as a small-budget skip, and it used to read as budget 0 and pass."""

    result = _cpa_run(
        monkeypatch, [], core_budget=8, capped_budget=8, capped_stdout="not a plan\n"
    )
    out = capsys.readouterr().out

    assert result == (1, "FAILED", len(_CPA_ALL_CHECKS), [])
    assert (
        "DIVERGENCE [cpa:mem-capped: unparseable --max-mem allocation (py exit 0, rs exit 0)"
    ) in out
    assert "cross[dagrun]: 1 divergence(s) out of 10 checks" in out


# ------------------------------------------------------------------ multi-tool final line


def _coverage(
    differential: ModuleType, tool: str, verdict: str, skips: int, kind: str = "boxing"
) -> object:
    skipped = tuple(
        differential.Skip(f"{tool}-skip-{index}", kind, "no box") for index in range(skips)
    )
    return differential.ToolCoverage(tool, verdict, 10, skipped)


def test_the_multi_tool_line_is_ok_only_with_no_skip_and_no_failure() -> None:
    differential = _differential()
    clean = [(0, _coverage(differential, tool, "OK", 0)) for tool in ("a", "b", "c")]
    partial = [
        (0, _coverage(differential, "a", "OK", 0)),
        (0, _coverage(differential, "b", "PARTIAL", 2)),
        (0, _coverage(differential, "c", "PARTIAL", 1, "multi-cpu")),
    ]
    failed = [
        (1, _coverage(differential, "a", "INCOMPLETE", 3)),
        (0, _coverage(differential, "b", "OK", 0)),
        (1, _coverage(differential, "c", "FAILED", 0)),
    ]

    assert differential.coverage_aggregate(clean) == (
        "cross: OK - all 3 tools agree with no skipped checks",
        0,
    )
    assert differential.coverage_aggregate(partial) == (
        "cross: PARTIAL - 3 skipped across tools (b 2, c 1; by kind: boxing 2, multi-cpu 1); "
        "no tool diverged, but the skipped checks are UNVERIFIED and listed above",
        0,
    )
    assert differential.coverage_aggregate(failed) == (
        "cross: FAILED - 2 of 3 tool(s) did not pass: a INCOMPLETE, c FAILED; "
        "3 skipped across tools (a 3; by kind: boxing 3)",
        1,
    )


def _all_tools_stubbed(
    differential: ModuleType,
    monkeypatch: pytest.MonkeyPatch,
    *,
    dagrun_skips: int = 0,
    diverging: str | None = None,
) -> list[str]:
    """Replace every tool's differential with a fast one that reaches the real verdict code."""

    ran: list[str] = []

    def verdict(tool: str, allowed_skips: frozenset[str], skips: int = 0) -> int:
        ran.append(tool)
        report = differential.Report()
        report.ok("ran")
        for index in range(skips):
            report.skip(f"boxed-{index}", "boxing", "no box")
        if diverging == tool:
            report.bad("case", "engines differ")
            return int(differential.divergence_verdict(tool, report, "2 checks", allowed_skips))
        return int(differential.coverage_verdict(tool, report, "1 checks agree", allowed_skips))

    def own_count(tool: str) -> Callable[[object, object], int]:
        def compare(_py: object, _rs: object, **_options: object) -> int:
            ran.append(tool)
            return 1 if diverging == tool else 0

        return compare

    def compare_dagrun(
        _rand: int, _seed: int, *, validation_jobs: int, allowed_skips: frozenset[str]
    ) -> int:
        assert validation_jobs == 4
        return verdict("dagrun", allowed_skips, dagrun_skips)

    def compare_cpuset_alloc(*, allowed_skips: frozenset[str]) -> int:
        return verdict("cpuset-alloc", allowed_skips)

    monkeypatch.setattr(differential, "compare_dagrun", compare_dagrun)
    monkeypatch.setattr(differential, "compare_cpuset_alloc", compare_cpuset_alloc)
    monkeypatch.setattr(
        differential, "compare_tick_hub", lambda _rand, _seed: verdict("tick-hub", frozenset())
    )
    monkeypatch.setattr(
        differential,
        "compare_pr_landing_planner",
        lambda _rand, _seed: verdict("pr-landing-planner", frozenset()),
    )
    monkeypatch.setattr(differential, "compare_herdr_run", own_count("herdr-run"))
    monkeypatch.setattr(differential, "compare_herdr_agent", own_count("herdr-agent"))
    monkeypatch.setattr(differential, "compare_agentctl", own_count("agentctl"))
    monkeypatch.setattr(differential, "py_command_for", lambda tool: [f"python-{tool}"])
    monkeypatch.setattr(differential, "rs_command", lambda tool: [f"rust-{tool}"])
    monkeypatch.setattr(differential, "_effective_validation_jobs", lambda: 4)
    monkeypatch.delenv("AGENT_UTILS_VALIDATION_JOBS", raising=False)
    monkeypatch.delenv("AGENT_UTILS_CROSS_ALLOW_SKIP", raising=False)
    return ran


@pytest.mark.parametrize(
    ("argv", "dagrun_skips", "diverging", "status", "final"),
    [
        pytest.param([], 0, None, 0, "cross: OK - all 7 tools agree with no skipped checks", id="ok"),
        pytest.param(
            ["--allow-skip", "boxing"],
            2,
            None,
            0,
            "cross: PARTIAL - 2 skipped across tools (dagrun 2; by kind: boxing 2); no tool "
            "diverged, but the skipped checks are UNVERIFIED and listed above",
            id="partial",
        ),
        pytest.param(
            [],
            2,
            None,
            1,
            "cross: FAILED - 1 of 7 tool(s) did not pass: dagrun INCOMPLETE; "
            "2 skipped across tools (dagrun 2; by kind: boxing 2)",
            id="incomplete",
        ),
        pytest.param(
            ["--allow-skip", "boxing"],
            1,
            "tick-hub",
            1,
            "cross: FAILED - 1 of 7 tool(s) did not pass: tick-hub FAILED; "
            "1 skipped across tools (dagrun 1; by kind: boxing 1)",
            id="diverged",
        ),
        pytest.param(
            [],
            0,
            "herdr-run",
            1,
            "cross: FAILED - 1 of 7 tool(s) did not pass: herdr-run FAILED; 0 skipped across tools",
            id="own-count-tool-failed",
        ),
    ],
)
def test_tool_all_ends_with_one_line_for_the_whole_run(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    argv: list[str],
    dagrun_skips: int,
    diverging: str | None,
    status: int,
    final: str,
) -> None:
    differential = _differential()
    ran = _all_tools_stubbed(
        differential, monkeypatch, dagrun_skips=dagrun_skips, diverging=diverging
    )

    assert differential.main(["--tool", "all", *argv]) == status
    out = capsys.readouterr().out

    assert ran == list(differential.DIFFERENTIAL_TOOLS)
    assert out.splitlines()[-1] == final


def test_a_single_tool_run_prints_no_multi_tool_line(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    differential = _differential()
    ran = _all_tools_stubbed(differential, monkeypatch)

    assert differential.main(["--tool", "tick-hub"]) == 0
    out = capsys.readouterr().out

    assert ran == ["tick-hub"]
    assert out == "cross[tick-hub]: OK - 1 checks agree\n"


# ------------------------------------------------------------------ validate.py summary


def _validate() -> ModuleType:
    scripts = str(REPO_ROOT / "scripts")
    if scripts not in sys.path:
        sys.path.insert(0, scripts)
    spec = importlib.util.spec_from_file_location(
        "_validate_cross_coverage_under_test", REPO_ROOT / "scripts" / "validate.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_each_verdict_leaves_a_record_naming_the_node_that_ran_it(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    differential = _differential()
    monkeypatch.setenv(COVERAGE_DIR_ENV, str(tmp_path))
    monkeypatch.setenv("DAGRUN_OUTER_RUN", "cross.dagrun.differential")
    report = differential.Report()
    report.ok("ran")
    report.skip("boxed", "boxing", "no box")
    report.skip("pinned", "hard-cpuset", "refused")

    assert differential.coverage_verdict("demo", report, "1 checks", frozenset({"boxing"})) == 1

    records = sorted(tmp_path.iterdir())
    assert [path.name for path in records] == [f"demo-{os.getpid()}-1.json"]
    assert json.loads(records[0].read_text(encoding="utf-8")) == {
        "schema": 1,
        "tool": "demo",
        "node": "cross.dagrun.differential",
        "verdict": "INCOMPLETE",
        "checks": 1,
        "skipped": [
            {"label": "boxed", "kind": "boxing", "reason": "no box", "allowed": True},
            {"label": "pinned", "kind": "hard-cpuset", "reason": "refused", "allowed": False},
        ],
    }


def test_a_record_that_cannot_be_written_ends_the_run(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    differential = _differential()
    monkeypatch.setenv(COVERAGE_DIR_ENV, str(tmp_path / "missing"))
    report = differential.Report()
    report.ok("ran")

    with pytest.raises(SystemExit, match="cannot write the coverage record"):
        differential.coverage_verdict("demo", report, "1 checks agree", frozenset())


def test_validate_summarises_only_records_that_skipped(tmp_path: Path) -> None:
    validate = _validate()
    (tmp_path / "complete.json").write_text(
        json.dumps(
            {
                "tool": "tick-hub",
                "node": "cross.tick-hub.differential",
                "verdict": "OK",
                "checks": 9,
                "skipped": [],
            }
        ),
        encoding="utf-8",
    )
    (tmp_path / "partial.json").write_text(
        json.dumps(
            {
                "tool": "cpuset-alloc",
                "node": "cross.dagrun.cpuset-differential",
                "verdict": "PARTIAL",
                "checks": 53,
                "skipped": [
                    {"label": "run:signal-status", "kind": "delegated-live-scope"},
                    {"label": "interop:py-then-rs", "kind": "delegated-live-scope"},
                ],
            }
        ),
        encoding="utf-8",
    )
    (tmp_path / "truncated.json").write_text('{"tool": ', encoding="utf-8")
    (tmp_path / "wrong-shape.json").write_text("[]", encoding="utf-8")
    (tmp_path / "zz-mixed.json").write_text(
        json.dumps(
            {
                "tool": "dagrun",
                "node": "cross.dagrun.differential",
                "verdict": "PARTIAL",
                "checks": 17,
                "skipped": [{"label": "profile-timeseries", "kind": "boxing"}, "not-a-dict"],
            }
        ),
        encoding="utf-8",
    )

    lines, skipped, unreadable, missing, by_kind = validate.cross_coverage_report(
        tmp_path,
        [
            "cross.tick-hub.differential",
            "cross.dagrun.cpuset-differential",
            "cross.dagrun.differential",
        ],
    )

    assert skipped == 4
    assert by_kind == (("boxing", 1), ("delegated-live-scope", 2), ("malformed", 1))
    assert unreadable == 2
    assert missing == []
    assert len(lines) == 4
    assert lines[0] == (
        "  cross.dagrun.cpuset-differential (cpuset-alloc, PARTIAL): 53 check(s) ran; 2 skipped "
        "[delegated-live-scope 2]: run:signal-status, interop:py-then-rs"
    )
    assert lines[1].startswith("  unreadable cross coverage record truncated.json:")
    assert lines[2] == "  malformed cross coverage record wrong-shape.json: []"
    assert lines[3] == (
        "  cross.dagrun.differential (dagrun, PARTIAL): 17 check(s) ran; 2 skipped "
        "[boxing 1, malformed 1]: profile-timeseries, 'not-a-dict'"
    )


def test_a_selected_cross_node_without_a_record_is_not_full_coverage(tmp_path: Path) -> None:
    """A node that left no record may have been the PARTIAL one, exactly like a corrupt record.

    A record from a top-level run, or one that names a different node, does not stand in for it.
    """

    validate = _validate()
    for name, node in (("ran.json", "cross.tick-hub.differential"), ("top.json", None)):
        (tmp_path / name).write_text(
            json.dumps({"tool": "t", "node": node, "verdict": "OK", "checks": 1, "skipped": []}),
            encoding="utf-8",
        )
    expected = [
        "cross.dagrun.differential",
        "cross.tick-hub.differential",
        "cross.planner.differential",
    ]

    coverage = validate.cross_coverage_report(tmp_path, expected)

    assert (coverage.skipped, coverage.unreadable) == (0, 0)
    assert coverage.missing == ["cross.dagrun.differential", "cross.planner.differential"]
    assert coverage.lines == [
        "  cross.dagrun.differential: no readable coverage record, so its coverage could not be "
        "confirmed",
        "  cross.planner.differential: no readable coverage record, so its coverage could not be "
        "confirmed",
    ]

    # An empty directory confirms nothing, and no expected node confirms everything.
    empty = tmp_path / "empty"
    empty.mkdir()
    assert validate.cross_coverage_report(empty, expected).missing == expected
    assert validate.cross_coverage_report(empty, []) == validate.CrossCoverage([], 0, 0, [])


def test_validate_expects_a_record_from_exactly_the_selected_cross_nodes() -> None:
    validate = _validate()
    by_tag = dag_from_path(REPO_ROOT / "validation.dag.yaml").by_tag()
    every_cross_node = [tag for tag, step in by_tag.items() if "cross/differential.py" in step.cmd]

    assert len(every_cross_node) == 7
    assert validate.selected_cross_nodes(frozenset(), frozenset(), all_contract=True) == (
        every_cross_node
    )
    # A component-narrowed run expects only the cross node that component owns.
    assert validate.selected_cross_nodes(
        frozenset(), frozenset({"tick-hub"}), all_contract=False
    ) == ["cross.tick-hub.differential"]


def _validate_with_one_cross_node(
    monkeypatch: pytest.MonkeyPatch,
    *,
    skips: int,
    expected: list[str] | None = None,
) -> tuple[ModuleType, list[Path]]:
    """Make ``validate.main`` run a fake graph whose one cross node writes a real record.

    The node is ``cross.dagrun.cpuset-differential`` and it skips ``skips`` checks. ``expected``
    replaces the selected cross nodes, which otherwise are just that one node.
    """

    differential = _differential()
    validate = _validate()
    seen: list[Path] = []

    def fake_run(
        _selected: object,
        _components: object,
        *,
        all_contract: bool,
        coverage_dir: Path,
        host: object = None,
    ) -> int:
        assert all_contract
        assert host == {}
        seen.append(coverage_dir)
        monkeypatch.setenv(COVERAGE_DIR_ENV, str(coverage_dir))
        monkeypatch.setenv("DAGRUN_OUTER_RUN", "cross.dagrun.cpuset-differential")
        report = differential.Report()
        report.ok("ran")
        for _ in range(skips):
            report.skip("run:signal-status", "delegated-live-scope", "delegated root")
        allowed = frozenset({"delegated-live-scope"})
        assert differential.coverage_verdict("cpuset-alloc", report, "1 checks", allowed) == 0
        return 0

    nodes = ["cross.dagrun.cpuset-differential"] if expected is None else expected
    monkeypatch.setattr(validate, "run", fake_run)
    monkeypatch.setattr(validate, "selected_cross_nodes", lambda *_args, **_kw: nodes)
    # Hermetic: no host settings file from the machine running the tests.
    monkeypatch.setattr(validate, "host_env", lambda _environ: {})
    return validate, seen


@pytest.mark.parametrize("skips", [0, 1])
def test_validate_says_partial_when_a_passing_cross_node_skipped_checks(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    skips: int,
) -> None:
    """The records the differential writes are the ones ``validate.py`` reads."""

    validate, seen = _validate_with_one_cross_node(monkeypatch, skips=skips)

    assert validate.main(["--all"]) == 0
    out = capsys.readouterr().out

    assert len(seen) == 1 and not seen[0].exists()
    if skips:
        assert (
            "  cross.dagrun.cpuset-differential (cpuset-alloc, PARTIAL): 1 check(s) ran; "
            "1 skipped [delegated-live-scope 1]: run:signal-status"
        ) in out
        assert out.rstrip().endswith(
            "validate: PARTIAL - every selected node passed, but 1 cross check(s) were skipped "
            "and are UNVERIFIED (delegated-live-scope 1)"
        )
        assert "validate: OK" not in out
    else:
        assert out.rstrip().endswith("validate: OK")
        assert "PARTIAL" not in out
    assert "::warning" not in out


def test_validate_cannot_confirm_a_selected_cross_node_that_left_no_record(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    validate, _seen = _validate_with_one_cross_node(
        monkeypatch,
        skips=0,
        expected=["cross.dagrun.differential", "cross.dagrun.cpuset-differential"],
    )

    assert validate.main(["--all"]) == 0
    out = capsys.readouterr().out

    assert "validate: cross-language coverage was PARTIAL or could not be confirmed:" in out
    assert (
        "  cross.dagrun.differential: no readable coverage record, so its coverage could not be "
        "confirmed"
    ) in out
    assert out.rstrip().endswith(
        "validate: PARTIAL - every selected node passed, but 1 selected cross node(s) left no "
        "readable coverage record"
    )
    assert "validate: OK" not in out


def test_a_partial_run_under_github_actions_is_annotated_and_summarised(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], tmp_path: Path
) -> None:
    """A PARTIAL run exits 0, which a hosted job shows as a green tick; it must not read as OK."""

    validate, _seen = _validate_with_one_cross_node(monkeypatch, skips=1)
    summary = tmp_path / "step-summary.md"
    summary.write_text("earlier step\n", encoding="utf-8")
    monkeypatch.setenv("GITHUB_ACTIONS", "true")
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))

    assert validate.main(["--all"]) == 0
    out = capsys.readouterr().out

    verdict = (
        "validate: PARTIAL - every selected node passed, but 1 cross check(s) were skipped and "
        "are UNVERIFIED (delegated-live-scope 1)"
    )
    node = (
        "  cross.dagrun.cpuset-differential (cpuset-alloc, PARTIAL): 1 check(s) ran; 1 skipped "
        "[delegated-live-scope 1]: run:signal-status"
    )
    assert f"\n::warning title=validate PARTIAL::{verdict}%0A{node}\n" in out
    assert out.rstrip().endswith(verdict)
    assert summary.read_text(encoding="utf-8") == (
        f"earlier step\n### validate: PARTIAL\n\n{verdict}\n\n```text\n{node}\n```\n"
    )


def test_a_complete_run_under_github_actions_is_not_annotated(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], tmp_path: Path
) -> None:
    validate, _seen = _validate_with_one_cross_node(monkeypatch, skips=0)
    summary = tmp_path / "step-summary.md"
    monkeypatch.setenv("GITHUB_ACTIONS", "true")
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))

    assert validate.main(["--all"]) == 0
    out = capsys.readouterr().out

    assert out.rstrip().endswith("validate: OK")
    assert "::warning" not in out
    assert not summary.exists()


def test_a_workflow_annotation_escapes_its_message() -> None:
    validate = _validate()

    assert validate._workflow_command_data("100% done\r\nnext") == "100%25 done%0D%0Anext"


def test_an_unwritable_job_summary_does_not_change_the_verdict(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], tmp_path: Path
) -> None:
    validate, _seen = _validate_with_one_cross_node(monkeypatch, skips=1)
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(tmp_path / "missing" / "summary.md"))

    assert validate.main(["--all"]) == 0
    captured = capsys.readouterr()

    assert "validate: cannot append the PARTIAL verdict to GITHUB_STEP_SUMMARY:" in captured.err
    assert captured.out.rstrip().endswith(
        "validate: PARTIAL - every selected node passed, but 1 cross check(s) were skipped and "
        "are UNVERIFIED (delegated-live-scope 1)"
    )


def test_validate_prints_no_summary_after_a_failed_graph(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    validate = _validate()
    monkeypatch.setattr(validate, "run", lambda *_args, **_kw: 3)
    monkeypatch.setattr(
        validate, "selected_cross_nodes", lambda *_args, **_kw: ["cross.dagrun.differential"]
    )

    assert validate.main(["--all"]) == 3
    out = capsys.readouterr().out

    assert "validate: OK" not in out
    assert "validate: PARTIAL" not in out


def test_each_validation_lane_states_its_coverage_policy() -> None:
    """The delegated validation lanes are partial only for the structural live-scope skip.

    Both run inside a parent-owned delegated cgroup, where the harness must not create a live
    systemd scope. Boxing stays required: a boxed local run that loses boxing fails. A lane
    that cannot box must say so itself through ``AGENT_UTILS_CROSS_ALLOW_SKIP``.
    """

    by_tag = dag_from_path(REPO_ROOT / "validation.dag.yaml").by_tag()
    expected = {
        "cross.dagrun.differential": "--tool dagrun --allow-skip delegated-live-scope",
        "cross.dagrun.cpuset-differential": (
            "--tool cpuset-alloc --allow-skip delegated-live-scope"
        ),
    }
    for tag, arguments in expected.items():
        assert by_tag[tag].cmd == f"python3 cross/differential.py {arguments}"
    for tag, step in by_tag.items():
        if "cross/differential.py" in step.cmd and tag not in expected:
            assert "--allow-skip" not in step.cmd, tag
            assert "AGENT_UTILS_CROSS_ALLOW_SKIP" not in step.cmd, tag

    makefile = (REPO_ROOT / "Makefile").read_text(encoding="utf-8")
    assert "python3 cross/differential.py --tool all\n" in makefile
    assert "--allow-skip" not in makefile


def test_hosted_lanes_that_cannot_box_declare_themselves_partial() -> None:
    """Hosted runners cannot box and have fewer than eight CPUs; they declare exactly that."""

    workflows = REPO_ROOT / ".github" / "workflows"
    for name in ("cross-dagrun.yml", "nightly-all.yml"):
        text = (workflows / name).read_text(encoding="utf-8")
        assert "BOX_FLAGS: --allow-cgroup-failure" in text, name
        allowances = [
            line.split(":", 1)[1].strip()
            for line in text.splitlines()
            if line.strip().startswith("AGENT_UTILS_CROSS_ALLOW_SKIP:")
        ]
        assert allowances == ["boxing,eight-cpu"], name


def test_the_memory_cap_node_gets_fewer_than_eight_cores_only_from_explicit_inputs(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``cpa:mem-capped`` needs eight cores in ``cross.dagrun.differential``, which does not
    allow ``eight-cpu``. Pin the parts of the contract that keep that node at eight on a local run
    of at least eight CPUs: the node asks for eight through a resizable width, validate plans with
    the critical-path planner, and that planner does not change widths. Only a run budget below
    eight narrows it, and then visibly.
    """

    cfg = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    node = cfg.by_tag()["cross.dagrun.differential"]
    assert node.hint.preferred_inner_jobs == 8
    assert node.jobs_env == "AGENT_UTILS_VALIDATION_JOBS"
    assert "eight-cpu" not in node.cmd
    assert "AGENT_UTILS_CROSS_ALLOW_SKIP" not in node.env

    validate = _validate()
    commands: list[list[str]] = []

    def capture(command: list[str], **_kw: object) -> SimpleNamespace:
        commands.append(list(command))
        return SimpleNamespace(returncode=0)

    for name in ("VALIDATE_DAGRUN_FLAGS", "BOX_FLAGS", "VALIDATE_MAX_CPUS"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setattr(validate.subprocess, "run", capture)
    assert validate.run(frozenset(), frozenset(), all_contract=True) == 0
    [command] = commands
    assert command[command.index("--planner") + 1] == "critical-path"
    assert command.count("--planner") == 1
    assert command[command.index("--max-cpus") + 1] == str(min(32, os.cpu_count() or 1))

    # The critical-path plan publishes no width allocation, so applying it keeps eight.
    plan = build_plan(cfg, {}, planner=Planner.CRITICAL_PATH, core_budget=8)
    assert all(entry.alloc_inner_jobs is None for entry in plan.entries)
    planned = apply_plan_to_config(cfg, plan).by_tag()["cross.dagrun.differential"]
    assert planned.hint.preferred_inner_jobs == 8

    # A run budget of eight keeps the width; one below eight narrows it.
    assert cap_config_max_cpus(cfg, 8).by_tag()[node.tag].hint.preferred_inner_jobs == 8
    assert cap_config_max_cpus(cfg, 7).by_tag()[node.tag].hint.preferred_inner_jobs == 7
