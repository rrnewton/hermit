"""Tests for the tick engine (run_tick) with deterministic fake gate/age probes."""

from __future__ import annotations

import os
import shlex
import time
from pathlib import Path

import pytest

from tick_hub.cadence import unresolved_render_state_keys
from tick_hub.engine import NO_RESULT_EXIT, TickResult, parse_kv_lines, render_emit, run_tick
from tick_hub.model import (
    Emit,
    EmitKind,
    Gate,
    GateWhen,
    HealthCheck,
    Reminder,
    TickConfig,
)
from tick_hub.protocols import GateResult
from tick_hub.probes import SubprocessGateRunner
from tick_hub.state import OpsState


class FakeGate:
    """Return a canned GateResult per command; default success + given stdout."""

    def __init__(self, by_cmd: dict[str, GateResult]) -> None:
        self.by_cmd = by_cmd
        self.calls: list[tuple[str, int | None]] = []

    def run(self, cmd: str, *, timeout: int | None = None) -> GateResult:
        self.calls.append((cmd, timeout))
        return self.by_cmd.get(cmd, GateResult(returncode=0, stdout="", ok=True))


class FakeProbe:
    """Return a canned age per glob (None = missing)."""

    def __init__(self, ages: dict[str, int | None]) -> None:
        self.ages = ages

    def newest_age_secs(self, pattern: str, now: int) -> int | None:
        return self.ages.get(pattern)


def _action(name: str, skill: str, *, cadence: int = 0) -> Reminder:
    return Reminder(name, Emit(EmitKind.ACTION, title=f"do {name}", skill=skill), cadence_secs=cadence)


def test_health_status_ok_stale_missing() -> None:
    cfg = TickConfig(
        health_checks=(
            HealthCheck("fresh", "/f", 100, "f"),
            HealthCheck("old", "/o", 100, "o"),
            HealthCheck("gone", "/g", 100, "g"),
        )
    )
    probe = FakeProbe({"/f": 10, "/o": 999, "/g": None})
    result = run_tick(
        cfg, OpsState.default(), now=0, fired={},
        gate_runner=FakeGate({}), age_probe=probe,
    )
    joined = "\n".join(result.lines)
    assert "HEALTH: fresh ok" in joined
    assert "HEALTH: old stale" in joined
    assert "HEALTH: gone missing age_secs=NA" in joined


def test_plain_reminder_fires_and_marks_fired() -> None:
    cfg = TickConfig(reminders=(_action("sync", "git-sync", cadence=3600),))
    result = run_tick(
        cfg, OpsState.default(), now=500, fired={},
        gate_runner=FakeGate({}), age_probe=FakeProbe({}),
    )
    assert any(ln.startswith("ACTION: git-sync") for ln in result.lines)
    assert result.fired["sync"] == 500  # cadence clock reset
    assert result.actions_emitted == 1


def test_not_due_reminder_is_silent_and_unchanged() -> None:
    cfg = TickConfig(reminders=(_action("sync", "git-sync", cadence=3600),))
    result = run_tick(
        cfg, OpsState.default(), now=1000, fired={"sync": 900},
        gate_runner=FakeGate({}), age_probe=FakeProbe({}),
    )
    assert not any("git-sync" in ln for ln in result.lines)
    assert result.fired == {"sync": 900}  # untouched


def test_gate_success_fires_failure_suppresses() -> None:
    rem = Reminder(
        "ci",
        Emit(EmitKind.ACTION, title="ci red", skill="ci-red"),
        gate=Gate(cmd="check", when=GateWhen.FAILURE),
    )
    cfg = TickConfig(reminders=(rem,))
    # gate exits 0 -> when=failure NOT satisfied -> no emit, but the check ran (fired stamped).
    ok = run_tick(
        cfg, OpsState.default(), now=5, fired={},
        gate_runner=FakeGate({"check": GateResult(0, "", True)}), age_probe=FakeProbe({}),
    )
    assert not any("ci-red" in ln for ln in ok.lines)
    assert ok.fired["ci"] == 5
    # gate exits 1 -> when=failure satisfied -> emit.
    red = run_tick(
        cfg, OpsState.default(), now=5, fired={},
        gate_runner=FakeGate({"check": GateResult(1, "", True)}), age_probe=FakeProbe({}),
    )
    assert any("ci-red" in ln for ln in red.lines)


def test_gate_specific_timeout_is_forwarded_without_changing_default() -> None:
    default_runner = FakeGate({})
    override_runner = FakeGate({})
    default = Reminder(
        "default",
        Emit(EmitKind.ACTION, title="default", skill="default"),
        gate=Gate(cmd="default-check"),
    )
    override = Reminder(
        "override",
        Emit(EmitKind.ACTION, title="override", skill="override"),
        gate=Gate(cmd="long-check", timeout_secs=195),
    )
    run_tick(
        TickConfig(reminders=(default,)), OpsState.default(), now=0, fired={},
        gate_runner=default_runner, age_probe=FakeProbe({}),
    )
    run_tick(
        TickConfig(reminders=(override,)), OpsState.default(), now=0, fired={},
        gate_runner=override_runner, age_probe=FakeProbe({}),
    )
    assert default_runner.calls == [("default-check", None)]
    assert override_runner.calls == [("long-check", 195)]


def test_gate_capture_interpolates_fields_and_title() -> None:
    rem = Reminder(
        "backlog",
        Emit(
            EmitKind.ACTION,
            title="{count} ready (>{threshold})",
            skill="triage",
            fields={"threshold": "20"},
        ),
        gate=Gate(cmd="count", when=GateWhen.ALWAYS, capture=True),
    )
    cfg = TickConfig(reminders=(rem,))
    result = run_tick(
        cfg, OpsState.default(), now=0, fired={},
        gate_runner=FakeGate({"count": GateResult(0, "count=42\n", True)}),
        age_probe=FakeProbe({}),
    )
    line = next(ln for ln in result.lines if ln.startswith("ACTION: triage"))
    assert "count=42" in line
    assert 'title="42 ready (>20)"' in line


def test_captured_summary_renders_and_still_emits() -> None:
    rem = Reminder(
        "obligation",
        Emit(
            EmitKind.ACTION,
            title="speculative land requires attention: {summary}",
            skill="hard-warn",
            fields={"component": "speculative-land-obligation"},
        ),
        gate=Gate(cmd="check", when=GateWhen.FAILURE, capture=True),
    )
    result = run_tick(
        TickConfig(reminders=(rem,)), OpsState.default(), now=7, fired={},
        gate_runner=FakeGate({"check": GateResult(1, "summary=obligation abc is red\n", True)}),
        age_probe=FakeProbe({}),
    )
    assert any('title="speculative land requires attention: obligation abc is red"' in line
               for line in result.lines)
    assert result.fired["obligation"] == 7
    assert result.actions_emitted == 1


def test_unresolved_placeholder_is_refused_loudly_and_retried() -> None:
    rem = Reminder(
        "obligation",
        Emit(
            EmitKind.ACTION,
            title="speculative land requires attention: {summary}",
            skill="hard-warn",
            fields={"component": "speculative-land-obligation"},
        ),
        gate=Gate(cmd="check", when=GateWhen.FAILURE, capture=True),
    )
    result = run_tick(
        TickConfig(reminders=(rem,)), OpsState.default(), now=7, fired={},
        gate_runner=FakeGate({"check": GateResult(1, "", True)}),
        age_probe=FakeProbe({}),
    )
    actions = [line for line in result.lines if line.startswith("ACTION: ")]
    assert len(actions) == 1
    assert "outcome=NO-SIGNAL" in actions[0]
    assert "gate=obligation" in actions[0]
    assert "reason=unresolved-placeholder" in actions[0]
    assert "missing_placeholders=summary" in actions[0]
    assert "obligation abc is red" not in actions[0]
    assert any(
        line == (
            "ERROR: reminder obligation: refusing emission with unresolved "
            "placeholder(s): {summary}"
        )
        for line in result.lines
    )
    assert "obligation" not in result.fired
    count_key, first_key = unresolved_render_state_keys("obligation")
    assert result.fired[count_key] == 1
    assert result.fired[first_key] == 7
    assert result.actions_emitted == 1


def test_third_consecutive_unresolved_placeholder_adds_persistent_escalation() -> None:
    # Mutation controls: changing >=3 to >3 fails the third-tick assertion; resetting the first
    # epoch on each failure fails the retained first_failure_epoch=100 assertions.
    rem = Reminder(
        "obligation",
        Emit(
            EmitKind.ACTION,
            title="speculative land requires attention: {summary}",
            skill="hard-warn",
            fields={"component": "speculative-land-obligation"},
        ),
        gate=Gate(cmd="check", when=GateWhen.FAILURE, capture=True),
    )
    cfg = TickConfig(reminders=(rem,))
    count_key, first_key = unresolved_render_state_keys(rem.name)
    fired: dict[str, int] = {}

    for consecutive, now in enumerate((100, 200, 300), start=1):
        result = run_tick(
            cfg,
            OpsState.default(),
            now=now,
            fired=fired,
            gate_runner=FakeGate({"check": GateResult(1, "", True)}),
            age_probe=FakeProbe({}),
        )
        actions = [line for line in result.lines if line.startswith("ACTION: ")]
        assert any("reason=unresolved-placeholder" in line for line in actions)
        assert (len(actions), result.actions_emitted) == (
            (1, 1) if consecutive < 3 else (2, 2)
        )
        if consecutive < 3:
            assert not any("consecutive_failures=" in line for line in actions)
        else:
            repeated = next(line for line in actions if "consecutive_failures=" in line)
            assert "consecutive_failures=3" in repeated
            assert "first_failure_epoch=100" in repeated
            assert "missing_placeholders=summary" in repeated
        assert result.fired[count_key] == consecutive
        assert result.fired[first_key] == 100
        assert rem.name not in result.fired
        fired = dict(result.fired)


def test_any_later_non_render_failure_outcome_clears_render_failure_state() -> None:
    # Mutation control: deleting any branch's clear call leaves one of these four seeded keys live.
    rem = Reminder(
        "obligation",
        Emit(EmitKind.ACTION, title="problem: {summary}", skill="hard-warn"),
        gate=Gate(cmd="check", when=GateWhen.FAILURE, capture=True),
    )
    cfg = TickConfig(reminders=(rem,))
    count_key, first_key = unresolved_render_state_keys(rem.name)
    prior = {count_key: 4, first_key: 10}
    outcomes = (
        GateResult(-1, "", False, error="not found"),
        GateResult(NO_RESULT_EXIT, "", True),
        GateResult(0, "", True),
        GateResult(1, "summary=rendered\n", True),
    )

    for outcome in outcomes:
        result = run_tick(
            cfg,
            OpsState.default(),
            now=20,
            fired=prior,
            gate_runner=FakeGate({"check": outcome}),
            age_probe=FakeProbe({}),
        )
        assert count_key not in result.fired
        assert first_key not in result.fired


def test_removed_reminder_render_failure_state_is_pruned_without_touching_cadence() -> None:
    count_key, first_key = unresolved_render_state_keys("removed")
    result = run_tick(
        TickConfig(),
        OpsState.default(),
        now=20,
        fired={"still-config-independent": 7, count_key: 4, first_key: 10},
        gate_runner=FakeGate({}),
        age_probe=FakeProbe({}),
    )
    assert result.fired == {"still-config-independent": 7}


def test_no_evaluation_keeps_active_reminder_render_failure_state() -> None:
    rem = Reminder("obligation", Emit(EmitKind.NOTE, title="{summary}"))
    count_key, first_key = unresolved_render_state_keys(rem.name)
    prior = {count_key: 2, first_key: 10}
    result = run_tick(
        TickConfig(reminders=(rem,)),
        OpsState(enabled=False, tick_frequency_min=30),
        now=20,
        fired=prior,
        gate_runner=FakeGate({}),
        age_probe=FakeProbe({}),
    )
    assert result.fired == prior


def test_gate_run_failure_emits_no_signal_and_no_fired_stamp() -> None:
    rem = Reminder("x", Emit(EmitKind.ACTION, title="x", skill="x"), gate=Gate(cmd="boom"))
    cfg = TickConfig(reminders=(rem,))
    result = run_tick(
        cfg, OpsState.default(), now=9, fired={},
        gate_runner=FakeGate({"boom": GateResult(-1, "", False, error="not found")}),
        age_probe=FakeProbe({}),
    )
    assert any(ln.startswith("ERROR: reminder x") for ln in result.lines)
    actions = [line for line in result.lines if line.startswith("ACTION: ")]
    assert len(actions) == 1
    assert "outcome=NO-SIGNAL" in actions[0]
    assert "gate=x" in actions[0]
    assert "reason=gate-execution-error" in actions[0]
    assert "x" not in result.fired  # not marked -> retried next tick
    assert result.actions_emitted == 1


def test_requires_flags_gating() -> None:
    rem = Reminder(
        "bench",
        Emit(EmitKind.ACTION, title="run bench", skill="run-benchmark"),
        requires_flags=("benchmark_enabled",),
    )
    cfg = TickConfig(reminders=(rem,))
    off = OpsState(enabled=True, tick_frequency_min=30, flags={"benchmark_enabled": False})
    r_off = run_tick(
        cfg, off, now=0, fired={}, gate_runner=FakeGate({}), age_probe=FakeProbe({})
    )
    assert not any("run-benchmark" in ln for ln in r_off.lines)
    assert "bench" not in r_off.fired  # suppressed reminders do not consume the cadence
    on = OpsState(enabled=True, tick_frequency_min=30, flags={"benchmark_enabled": True})
    r_on = run_tick(
        cfg, on, now=0, fired={}, gate_runner=FakeGate({}), age_probe=FakeProbe({})
    )
    assert any("run-benchmark" in ln for ln in r_on.lines)


def test_disabled_state_runs_health_but_no_reminders() -> None:
    cfg = TickConfig(
        reminders=(_action("sync", "git-sync"),),
        health_checks=(HealthCheck("db", "/db", 100, "d"),),
    )
    state = OpsState(enabled=False, tick_frequency_min=30)
    result = run_tick(
        cfg, state, now=0, fired={},
        gate_runner=FakeGate({}), age_probe=FakeProbe({"/db": 5}),
    )
    assert any(ln.startswith("HEALTH: db ok") for ln in result.lines)
    assert not any("git-sync" in ln for ln in result.lines)
    assert any("disabled" in ln for ln in result.lines)
    assert result.actions_emitted == 0


def test_trailing_summary_counts_actions() -> None:
    cfg = TickConfig(reminders=(_action("a", "sa"), _action("b", "sb")))
    result = run_tick(
        cfg, OpsState.default(), now=0, fired={},
        gate_runner=FakeGate({}), age_probe=FakeProbe({}),
    )
    assert result.actions_emitted == 2
    assert result.lines[-1] == "NOTE: emitted 2 instruction(s) this tick"


def test_note_reminder_is_note_not_action() -> None:
    cfg = TickConfig(
        reminders=(Reminder("n", Emit(EmitKind.NOTE, title="just a note")),)
    )
    result = run_tick(
        cfg, OpsState.default(), now=0, fired={},
        gate_runner=FakeGate({}), age_probe=FakeProbe({}),
    )
    assert "NOTE: just a note" in result.lines
    assert result.actions_emitted == 0


def test_parse_kv_lines() -> None:
    assert parse_kv_lines("a=1\n# c\n\nb = two words \nno-eq\n") == {"a": "1", "b": "two words"}


def test_render_emit_note_interpolates() -> None:
    line = render_emit(Emit(EmitKind.NOTE, title="hi {who}"), {"who": "there"})
    assert line == "NOTE: hi there"


def test_static_fields_interpolate_static_and_captured_values() -> None:
    emit = Emit(
        EmitKind.ACTION,
        title="{copy}/{live}",
        skill="triage",
        fields={"base": "7", "copy": "{base}", "live": "{count}"},
    )
    assert render_emit(emit, {"count": "3"}) == (
        'ACTION: triage base=7 copy=7 live=3 count=3 title="7/3"'
    )


# --- COULD-NOT-DETERMINE, BOTH DIRECTIONS ------------------------------------
#
# The negative half is the point: a gate that cannot determine its condition
# must be visibly distinct from one that checked and found nothing. The
# positive halves prove the change is not inert -- a gate that CAN determine
# still reports exactly what it reported before.


def _probe(when: GateWhen = GateWhen.FAILURE) -> TickConfig:
    return TickConfig(
        reminders=(
            Reminder(
                "probe",
                Emit(EmitKind.ACTION, title="probe says {summary}", skill="warn"),
                gate=Gate(cmd="check", when=when, capture=True),
            ),
        )
    )


def _tick(cfg: TickConfig, code: int, stdout: str = "") -> TickResult:
    return run_tick(
        cfg, OpsState.default(), now=5, fired={},
        gate_runner=FakeGate({"check": GateResult(code, stdout, True)}),
        age_probe=FakeProbe({}),
    )


def test_could_not_determine_renders_no_result_not_a_pass() -> None:
    res = _tick(_probe(), NO_RESULT_EXIT, "summary=backend unreachable\n")
    no_result = [ln for ln in res.lines if ln.startswith("NO_RESULT: ")]
    assert len(no_result) == 1, res.lines
    assert "probe" in no_result[0]
    assert "this is not a pass" in no_result[0]
    assert "backend unreachable" in no_result[0]
    # the domain verdict must NOT also be emitted
    assert not any("probe says" in ln for ln in res.lines)


def test_no_result_reaches_action_only_consumers() -> None:
    # A consumer that forwards ACTION lines only must still see it, or the
    # whole point is lost for exactly the readers that matter.
    res = _tick(_probe(), NO_RESULT_EXIT, "summary=x\n")
    actions = [ln for ln in res.lines if ln.startswith("ACTION: ")]
    assert any("could-not-determine" in ln for ln in actions), res.lines
    assert res.actions_emitted >= 1


def test_no_result_is_emittable_when_the_gate_printed_nothing() -> None:
    # THE CORRELATED-FAILURE CASE. A gate that cannot determine its condition is
    # the one least likely to produce a usable summary=, so the line must not
    # depend on captured fields or go through render_emit's placeholder check.
    res = _tick(_probe(), NO_RESULT_EXIT, "")
    no_result = [ln for ln in res.lines if ln.startswith("NO_RESULT: ")]
    assert len(no_result) == 1, res.lines
    assert "{" not in no_result[0]
    assert not any("unresolved-placeholder" in ln for ln in res.lines), res.lines


def test_no_result_does_not_consume_cadence() -> None:
    res = _tick(_probe(), NO_RESULT_EXIT, "")
    assert "probe" not in res.fired, "must re-announce next tick"


def test_no_result_is_not_reinterpreted_by_when_success() -> None:
    # Under when=success a nonzero code means "quiet". Without the pre-check, 75
    # would read as a CLEAN PASS -- the exact collapse this removes.
    res = _tick(_probe(GateWhen.SUCCESS), NO_RESULT_EXIT, "")
    assert any(ln.startswith("NO_RESULT: ") for ln in res.lines), res.lines


def test_positive_control_a_determinable_gate_is_unchanged() -> None:
    fired = _tick(_probe(), 1, "summary=real problem\n")
    assert any("probe says real problem" in ln for ln in fired.lines), fired.lines
    assert not any(ln.startswith("NO_RESULT: ") for ln in fired.lines)
    assert fired.fired["probe"] == 5, "a real verdict still consumes cadence"

    quiet = _tick(_probe(), 0, "")
    assert not any(ln.startswith("NO_RESULT: ") for ln in quiet.lines)
    assert not any("probe says" in ln for ln in quiet.lines)
    assert quiet.fired["probe"] == 5


def _dependency_probe(name: str, *depends_on: str) -> Reminder:
    return Reminder(
        name,
        Emit(EmitKind.ACTION, title=f"{name} found a problem", skill="warn"),
        gate=Gate(cmd=name, when=GateWhen.FAILURE),
        depends_on=depends_on,
    )


def test_foundation_no_result_marks_quiet_dependents_unevaluable() -> None:
    # Put the dependency last to prove config order does not control whether
    # the relationship is observed. Every gate must still run.
    cfg = TickConfig(
        reminders=(
            _dependency_probe("dependent", "foundation"),
            _dependency_probe("independent"),
            _dependency_probe("foundation"),
        )
    )
    runner = FakeGate(
        {
            "foundation": GateResult(NO_RESULT_EXIT, "", True),
            "dependent": GateResult(0, "", True),
            "independent": GateResult(0, "", True),
        }
    )
    result = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=runner,
        age_probe=FakeProbe({}),
    )
    assert [cmd for cmd, _ in runner.calls] == [
        "dependent",
        "independent",
        "foundation",
    ]
    assert any(
        line.startswith("NO_RESULT: dependent is unevaluable because dependency foundation")
        for line in result.lines
    ), result.lines
    assert any("reason=dependency-could-not-determine" in line for line in result.lines)
    assert "dependent" not in result.fired, "unevaluable silence must retry"
    assert "foundation" not in result.fired, "direct NO_RESULT must retry"
    assert result.fired["independent"] == 5, "independent silence stays determinable"


def test_dependency_never_suppresses_a_real_finding() -> None:
    cfg = TickConfig(
        reminders=(
            _dependency_probe("foundation"),
            _dependency_probe("dependent", "foundation"),
        )
    )
    result = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(
            {
                "foundation": GateResult(NO_RESULT_EXIT, "", True),
                "dependent": GateResult(1, "", True),
            }
        ),
        age_probe=FakeProbe({}),
    )
    assert any("dependent found a problem" in line for line in result.lines), result.lines
    assert not any("dependent is unevaluable" in line for line in result.lines)
    assert result.fired["dependent"] == 5


def test_dependency_propagates_through_quiet_chain_only() -> None:
    cfg = TickConfig(
        reminders=(
            _dependency_probe("foundation"),
            _dependency_probe("middle", "foundation"),
            _dependency_probe("leaf", "middle"),
        )
    )
    result = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(
            {
                "foundation": GateResult(NO_RESULT_EXIT, "", True),
                "middle": GateResult(0, "", True),
                "leaf": GateResult(0, "", True),
            }
        ),
        age_probe=FakeProbe({}),
    )
    assert any("middle is unevaluable" in line for line in result.lines), result.lines
    assert any("leaf is unevaluable" in line for line in result.lines), result.lines


def test_empty_success_is_not_inferred_to_be_no_result() -> None:
    cfg = TickConfig(
        reminders=(
            _dependency_probe("foundation"),
            _dependency_probe("dependent", "foundation"),
        )
    )
    result = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(
            {
                "foundation": GateResult(0, "", True),
                "dependent": GateResult(0, "", True),
            }
        ),
        age_probe=FakeProbe({}),
    )
    assert not any(line.startswith("NO_RESULT: ") for line in result.lines), result.lines
    assert result.fired == {"foundation": 5, "dependent": 5}


class RecordingGate:
    """A gate runner that records what the tick had already reported when it ran.

    The interesting property of incremental emission is not that the same lines
    come out -- it is WHEN they come out. This runner captures the emitted
    report as each subsequent gate starts, so a test can prove that an earlier
    gate's verdict had already left the engine before a later, slower gate was
    even invoked.
    """

    def __init__(self, emitted: list[str], by_cmd: dict[str, GateResult]) -> None:
        self.emitted = emitted
        self.by_cmd = by_cmd
        self.seen_at_call: list[tuple[str, tuple[str, ...]]] = []

    def run(self, cmd: str, *, timeout: int | None = None) -> GateResult:
        self.seen_at_call.append((cmd, tuple(self.emitted)))
        return self.by_cmd.get(cmd, GateResult(returncode=0, stdout="", ok=True))


def test_emit_delivers_a_gate_verdict_before_the_next_gate_runs() -> None:
    cfg = TickConfig(
        health_checks=(HealthCheck("fresh", "/f", 100, "f"),),
        reminders=(
            _dependency_probe("first"),
            _dependency_probe("second"),
            _dependency_probe("third"),
        ),
    )
    emitted: list[str] = []
    runner = RecordingGate(
        emitted,
        {
            "first": GateResult(1, "summary=first is unhappy", True),
            "second": GateResult(1, "summary=second is unhappy", True),
            "third": GateResult(0, "", True),
        },
    )
    result = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=runner,
        age_probe=FakeProbe({"/f": 10}),
        emit=emitted.append,
    )

    # Streaming must not change what the report says, only when it is written.
    assert tuple(emitted) == result.lines

    seen = dict(runner.seen_at_call)
    assert any("HEALTH: fresh ok" in line for line in seen["first"]), (
        "the health lines precede every gate and must already be out"
    )
    assert any("first found a problem" in line for line in seen["second"]), (
        "the first gate's finding must be readable before the second gate runs"
    )
    assert any("second found a problem" in line for line in seen["third"])
    assert not any("second found a problem" in line for line in seen["second"]), (
        "a gate's own line cannot exist before that gate has run"
    )


def test_a_dependent_reminder_holds_the_report_until_every_gate_has_run() -> None:
    # A QUIET gate is downgraded when something it depends on could not
    # determine anything, and that something may run LATER. Emitting its clean
    # line early would publish a verdict the tick is about to withdraw, so this
    # configuration must fall back to reporting once at the end.
    cfg = TickConfig(
        reminders=(
            _dependency_probe("dependent", "foundation"),
            _dependency_probe("foundation"),
        )
    )
    emitted: list[str] = []
    runner = RecordingGate(
        emitted,
        {
            "foundation": GateResult(NO_RESULT_EXIT, "", True),
            "dependent": GateResult(0, "", True),
        },
    )
    result = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=runner,
        age_probe=FakeProbe({}),
        emit=emitted.append,
    )
    before_foundation = dict(runner.seen_at_call)["foundation"]
    assert not any(
        "found a problem" in line or line.startswith("NO_RESULT: ")
        for line in before_foundation
    ), (
        "no gate verdict may be published before the dependency verdict is known: "
        f"{before_foundation}"
    )
    assert tuple(emitted) == result.lines
    assert any(
        line.startswith("NO_RESULT: dependent is unevaluable because dependency foundation")
        for line in result.lines
    ), result.lines


def test_independent_prefix_streams_before_a_later_dependency_group() -> None:
    cfg = TickConfig(
        reminders=(
            _dependency_probe("independent"),
            _dependency_probe("dependent", "foundation"),
            _dependency_probe("foundation"),
        )
    )
    gate_results = {
        "independent": GateResult(1, "", True),
        "dependent": GateResult(0, "", True),
        "foundation": GateResult(NO_RESULT_EXIT, "", True),
    }

    collected = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(dict(gate_results)),
        age_probe=FakeProbe({}),
        report_pending=True,
    )
    emitted: list[str] = []
    runner = RecordingGate(emitted, dict(gate_results))
    streamed = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=runner,
        age_probe=FakeProbe({}),
        report_pending=True,
        emit=emitted.append,
    )

    seen = dict(runner.seen_at_call)
    assert any("independent found a problem" in line for line in seen["dependent"]), (
        "a final independent verdict must not wait for a later dependency group"
    )
    assert not any("CLEAN: dependent" in line for line in seen["foundation"]), (
        "the quiet dependent may still be downgraded after its dependency runs"
    )
    assert tuple(emitted) == collected.lines == streamed.lines
    assert streamed.fired == collected.fired
    assert streamed.actions_emitted == collected.actions_emitted


def test_suppressed_verdict_keeps_config_order_between_runnable_gates() -> None:
    cfg = TickConfig(
        reminders=(
            _dependency_probe("first"),
            Reminder(
                "suppressed",
                Emit(EmitKind.ACTION, title="suppressed fired", skill="warn"),
                requires_flags=("enabled",),
            ),
            _dependency_probe("third"),
        )
    )
    gate_results = {
        "first": GateResult(1, "", True),
        "third": GateResult(1, "", True),
    }
    emitted: list[str] = []
    runner = RecordingGate(emitted, dict(gate_results))
    streamed = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=runner,
        age_probe=FakeProbe({}),
        report_pending=True,
        emit=emitted.append,
    )
    collected = run_tick(
        cfg,
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(dict(gate_results)),
        age_probe=FakeProbe({}),
        report_pending=True,
    )

    verdicts = tuple(
        line
        for line in streamed.lines
        if line.startswith(("ACTION: warn", "SUPPRESSED: "))
    )
    assert verdicts == (
        'ACTION: warn title="first found a problem"',
        "SUPPRESSED: suppressed did not run; required flag(s) not set: enabled",
        'ACTION: warn title="third found a problem"',
    )
    assert tuple(emitted) == collected.lines == streamed.lines
    assert any("first found a problem" in line for line in dict(runner.seen_at_call)["third"])


def test_omitting_emit_reports_exactly_what_streaming_reports() -> None:
    def build() -> TickConfig:
        return TickConfig(
            health_checks=(HealthCheck("fresh", "/f", 100, "f"),),
            reminders=(
                _dependency_probe("noisy"),
                _dependency_probe("quiet"),
                _dependency_probe("undetermined"),
            ),
        )

    results = {
        "noisy": GateResult(1, "summary=noisy is unhappy", True),
        "quiet": GateResult(0, "", True),
        "undetermined": GateResult(NO_RESULT_EXIT, "summary=cannot tell", True),
    }
    collected = run_tick(
        build(),
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(dict(results)),
        age_probe=FakeProbe({"/f": 10}),
    )
    emitted: list[str] = []
    streamed = run_tick(
        build(),
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=FakeGate(dict(results)),
        age_probe=FakeProbe({"/f": 10}),
        emit=emitted.append,
    )
    assert collected.lines == streamed.lines
    assert tuple(emitted) == collected.lines
    assert collected.fired == streamed.fired
    assert collected.actions_emitted == streamed.actions_emitted


def test_explicit_parallel_gates_overlap_and_keep_config_order(tmp_path: Path) -> None:
    marker = tmp_path / "second-started"
    marker_arg = shlex.quote(str(marker))
    first = Reminder(
        "first",
        Emit(EmitKind.ACTION, title="first", skill="warn"),
        gate=Gate(
            cmd=(
                f"for unused in $(seq 1 200); do [ -e {marker_arg} ] && "
                "printf 'summary=first\\n' && exit 1; sleep 0.01; done; exit 75"
            ),
            when=GateWhen.FAILURE,
            capture=True,
            timeout_secs=4,
            parallel=True,
        ),
    )
    second = Reminder(
        "second",
        Emit(EmitKind.ACTION, title="second", skill="warn"),
        gate=Gate(
            cmd=f"sleep 0.1; : > {marker_arg}; printf 'summary=second\\n'; exit 1",
            when=GateWhen.FAILURE,
            capture=True,
            timeout_secs=4,
            parallel=True,
        ),
    )

    result = run_tick(
        TickConfig(reminders=(first, second)),
        OpsState.default(),
        now=5,
        fired={},
        gate_runner=SubprocessGateRunner(timeout=4),
        age_probe=FakeProbe({}),
    )

    actions = tuple(line for line in result.lines if line.startswith("ACTION: warn"))
    assert actions == (
        'ACTION: warn summary=first title="first"',
        'ACTION: warn summary=second title="second"',
    )
    assert result.fired == {"first": 5, "second": 5}


@pytest.mark.skipif(os.name != "posix", reason="process liveness is POSIX-specific")
def test_failed_stream_write_kills_outstanding_parallel_gate(tmp_path: Path) -> None:
    pid_file = tmp_path / "second.pid"
    pid_arg = shlex.quote(str(pid_file))
    first = Reminder(
        "first",
        Emit(EmitKind.ACTION, title="first", skill="warn"),
        gate=Gate(
            cmd=(
                f"for unused in $(seq 1 200); do [ -e {pid_arg} ] && exit 1; "
                "sleep 0.01; done; exit 75"
            ),
            when=GateWhen.FAILURE,
            parallel=True,
        ),
    )
    second = Reminder(
        "second",
        Emit(EmitKind.ACTION, title="second", skill="warn"),
        gate=Gate(
            cmd=f"echo $$ > {pid_arg}; exec sleep 30",
            when=GateWhen.FAILURE,
            timeout_secs=30,
            parallel=True,
        ),
    )

    def fail_on_verdict(line: str) -> None:
        if line.startswith("ACTION: warn"):
            raise OSError("reader closed")

    with pytest.raises(OSError, match="reader closed"):
        run_tick(
            TickConfig(reminders=(first, second)),
            OpsState.default(),
            now=5,
            fired={},
            gate_runner=SubprocessGateRunner(timeout=30),
            age_probe=FakeProbe({}),
            emit=fail_on_verdict,
        )

    pid = int(pid_file.read_text(encoding="utf-8"))
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.01)
    else:
        os.kill(pid, 9)
        raise AssertionError("failed output left a parallel gate running")
