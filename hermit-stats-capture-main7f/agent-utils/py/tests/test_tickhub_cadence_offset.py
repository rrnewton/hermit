"""Phased cadences: spreading one cadence period's reminders across its ticks.

Every test pins ``now`` explicitly. The phase is derived from absolute time, so these are
exact-boundary assertions rather than timing-sensitive ones.
"""

from __future__ import annotations

import pytest

from tick_hub.cadence import due_reminders, is_due, scheduled_instant
from tick_hub.io import TickConfigError, config_from_yaml, config_to_yaml
from tick_hub.model import Emit, EmitKind, Reminder

HOUR = 3600
HALF = 1800


def _rem(name: str, offset: int | None) -> Reminder:
    return Reminder(
        name=name,
        emit=Emit(kind=EmitKind.NOTE, title=name),
        cadence_secs=HOUR,
        cadence_offset_secs=offset,
    )


def test_no_offset_keeps_the_elapsed_rule_exactly() -> None:
    """The existing behaviour must be untouched when no offset is configured."""
    for elapsed in (0, 1, HOUR - 1, HOUR, HOUR + 1, 10 * HOUR):
        now = 1_000_000 + elapsed
        assert is_due("r", HOUR, now, {"r": 1_000_000}) == (elapsed >= HOUR)
        assert is_due("r", HOUR, now, {"r": 1_000_000}, None) == (elapsed >= HOUR)


def test_never_fired_is_due_in_both_modes() -> None:
    assert is_due("r", HOUR, 1_000_000, {}) is True
    assert is_due("r", HOUR, 1_000_000, {}, 0) is True
    assert is_due("r", HOUR, 1_000_000, {}, HALF) is True


def test_scheduled_instants_land_on_the_configured_phase() -> None:
    assert scheduled_instant(HOUR, 0, 7 * HOUR + 5) == 7 * HOUR
    assert scheduled_instant(HOUR, HALF, 7 * HOUR + 5) == 6 * HOUR + HALF
    assert scheduled_instant(HOUR, HALF, 7 * HOUR + HALF) == 7 * HOUR + HALF


def test_first_ever_run_fires_immediately_then_settles_onto_its_phase() -> None:
    """"Never fired" stays due regardless of phase, so a brand-new reminder runs at once.

    That first firing is OFF-phase by construction, and it is the only one: from the next
    scheduled instant onwards the reminder keeps its phase. Asserted rather than avoided,
    because a reader counting firings over the first hour will otherwise see one extra.
    """
    b = _rem("phase_b", HALF)
    fired: dict[str, int] = {}
    firings = []
    for tick in range(6):
        now = tick * HALF
        if due_reminders([b], now, fired):
            firings.append(now)
            fired["phase_b"] = now
    assert firings == [0, HALF, HALF + HOUR, HALF + 2 * HOUR]
    assert firings[0] == 0, "the bootstrap firing"
    for later in firings[1:]:
        assert later % HOUR == HALF, "every subsequent firing sits on the configured phase"


def test_each_phase_fires_exactly_once_per_hour_at_steady_state() -> None:
    """48 half-hour ticks from a settled state; each fires 24 times and never together."""
    a, b = _rem("phase_a", 0), _rem("phase_b", HALF)
    fired = {"phase_a": 0, "phase_b": HALF}
    counts = {"phase_a": 0, "phase_b": 0}
    together = 0
    for tick in range(2, 50):
        now = tick * HALF
        due = {r.name for r in due_reminders([a, b], now, fired)}
        if len(due) == 2:
            together += 1
        for name in due:
            counts[name] += 1
            fired[name] = now
            expected = 0 if name == "phase_a" else HALF
            assert now % HOUR == expected, (
                f"{name} fired at {now}, off its configured phase"
            )
    assert counts == {"phase_a": 24, "phase_b": 24}
    assert together == 0, "phased reminders must never come due on the same tick"


def test_two_offsets_are_never_due_on_the_same_tick_once_running() -> None:
    a, b = _rem("phase_a", 0), _rem("phase_b", HALF)
    fired = {"phase_a": 0, "phase_b": HALF}
    for tick in range(2, 48):
        now = tick * HALF
        due = {r.name for r in due_reminders([a, b], now, fired)}
        assert len(due) <= 1, f"both phases due at tick {tick}"
        for name in due:
            fired[name] = now


def test_a_cut_off_reminder_stays_due_and_does_not_drag_the_other_phase() -> None:
    """A reminder that did not complete records no epoch, so it stays due -- and that
    must not make the other phase's reminder due alongside it."""
    a, b = _rem("phase_a", 0), _rem("phase_b", HALF)
    fired = {"phase_a": 0, "phase_b": HALF}
    # phase_a is due at 1*HOUR and is CUT OFF: no epoch recorded.
    now = HOUR
    assert {r.name for r in due_reminders([a, b], now, fired)} == {"phase_a"}
    # Still due on its next scheduled instant, having never run.
    for tick in (3, 4, 5, 6):
        now = tick * HALF
        due = {r.name for r in due_reminders([a, b], now, fired)}
        assert "phase_a" in due, "a cut-off reminder must remain due"
        assert due != {"phase_a", "phase_b"} or tick % 2 == 1
        if "phase_b" in due:
            fired["phase_b"] = now
    # phase_b kept its own rhythm rather than piling onto phase_a's overdue slot.
    assert fired["phase_b"] > HALF


def test_phase_survives_restart_and_replay() -> None:
    """Dueness is a pure function of (now, last_fired, offset): no tick counter to lose."""
    for now in (12_345, HOUR, HOUR + HALF, 999 * HOUR + 7):
        for offset in (0, HALF):
            first = is_due("r", HOUR, now, {"r": 0}, offset)
            # A "restart" reloads the same persisted epoch and asks again.
            again = is_due("r", HOUR, now, {"r": 0}, offset)
            assert first == again
            assert first == (0 < scheduled_instant(HOUR, offset, now))


def _cfg(extra: str) -> str:
    return (
        "reminders:\n"
        "  - name: r\n"
        "    cadence_secs: 3600\n"
        f"{extra}"
        "    emit:\n"
        "      kind: note\n"
        "      title: t\n"
    )


def test_offset_is_parsed_and_round_trips() -> None:
    cfg = config_from_yaml(_cfg("    cadence_offset_secs: 1800\n"))
    assert cfg.reminders[0].cadence_offset_secs == HALF
    # A round trip must not drop the phase, or re-emitting the config would
    # silently collapse every reminder back onto one tick.
    again = config_from_yaml(config_to_yaml(cfg))
    assert again.reminders[0].cadence_offset_secs == HALF
    # And a reminder without a phase must not gain one.
    plain = config_from_yaml(config_to_yaml(config_from_yaml(_cfg(""))))
    assert plain.reminders[0].cadence_offset_secs is None


def test_offset_at_or_beyond_cadence_is_refused() -> None:
    with pytest.raises(TickConfigError, match="must be less than"):
        config_from_yaml(_cfg("    cadence_offset_secs: 3600\n"))


def test_offset_on_an_every_tick_reminder_is_refused() -> None:
    text = (
        "reminders:\n  - name: r\n    cadence_secs: 0\n    cadence_offset_secs: 5\n"
        "    emit:\n      kind: note\n      title: t\n"
    )
    with pytest.raises(TickConfigError, match="requires a positive"):
        config_from_yaml(text)


def test_an_off_phase_last_fired_is_pulled_back_onto_the_phase() -> None:
    """The case the plain elapsed rule cannot handle, and the reason phasing is not
    merely cosmetic.

    A late tick or a cut-off run leaves a reminder having last fired at an arbitrary
    instant. The elapsed rule then keeps measuring an hour from THAT instant, so the
    reminder drifts and can land back on the same tick as the cohort it was separated
    from. The phased rule re-anchors it to the configured instant.
    """
    off_phase = HOUR + 900  # fired 15 minutes into the wrong half of the hour
    # Phased: not due again until the next instant at :30, then exactly on phase.
    assert is_due("r", HOUR, off_phase + 60, {"r": off_phase}, HALF) is False
    assert is_due("r", HOUR, HOUR + HALF, {"r": off_phase}, HALF) is True
    # Unphased, the same state stays keyed to the drifted instant instead.
    assert is_due("r", HOUR, HOUR + HALF, {"r": off_phase}) is False
    assert is_due("r", HOUR, off_phase + HOUR, {"r": off_phase}) is True


def test_the_window_keeps_a_cut_off_reminder_on_its_own_phase() -> None:
    """Remains due, but is not carried onto the other phase's tick.

    Without a window a reminder that never records an epoch is due at EVERY later tick,
    so a cut-off cohort lands on the tick that was meant to run the other half -- the
    exact pile-up phasing exists to prevent.
    """
    seeded = {"r": 0}
    # Its own instant: due, window open.
    assert is_due("r", HOUR, HOUR, seeded, 0, HALF) is True
    # The other phase's tick, half an hour later: still unfinished, NOT offered.
    assert is_due("r", HOUR, HOUR + HALF, seeded, 0, HALF) is False
    # Its next instant: offered again. Nothing marked it done.
    assert is_due("r", HOUR, 2 * HOUR, seeded, 0, HALF) is True
    # With no window configured the old always-retry behaviour is retained.
    assert is_due("r", HOUR, HOUR + HALF, seeded, 0, None) is True


def test_window_without_an_offset_is_refused() -> None:
    text = (
        "reminders:\n  - name: r\n    cadence_secs: 3600\n    cadence_window_secs: 60\n"
        "    emit:\n      kind: note\n      title: t\n"
    )
    with pytest.raises(TickConfigError, match="requires 'cadence_offset_secs'"):
        config_from_yaml(text)


def test_window_outside_the_cadence_is_refused() -> None:
    for bad in (0, HOUR + 1):
        text = (
            f"reminders:\n  - name: r\n    cadence_secs: 3600\n"
            f"    cadence_offset_secs: 0\n    cadence_window_secs: {bad}\n"
            "    emit:\n      kind: note\n      title: t\n"
        )
        with pytest.raises(TickConfigError, match="must be greater"):
            config_from_yaml(text)


def test_never_fired_stays_due_on_both_phases_and_that_is_deliberate() -> None:
    """The accepted residual, asserted so it cannot drift silently.

    A reminder that has never once completed is due regardless of phase, because the
    pending report relies on an empty fired-state meaning "ask every reminder". The cost
    is that a perpetually-NO_RESULT reminder is offered on both phases until it completes
    once -- one cheap gate, not the cohort pile-up phasing exists to stop.
    """
    empty: dict[str, int] = {}
    assert is_due("r", HOUR, HOUR, empty, 0, HALF) is True
    assert is_due("r", HOUR, HOUR + HALF, empty, 0, HALF) is True
    # Once it HAS completed, the window confines its retries to its own phase.
    assert is_due("r", HOUR, HOUR + HALF, {"r": 0}, 0, HALF) is False


def test_the_engine_itself_honours_the_phase() -> None:
    """The engine calls is_due directly rather than through due_reminders. A phase that
    only due_reminders respects is a phase the real tick ignores -- which is exactly what
    happened on 2026-09-06: the config was phased, a due_reminders simulation agreed, and
    the running tick still put every hourly gate on one tick."""
    from tick_hub.engine import run_tick
    from tick_hub.protocols import GateResult
    from tick_hub.model import TickConfig
    from tick_hub.state import OpsState

    # Typed to the real protocols rather than loosely: a stub whose signature drifts from
    # GateRunner stops proving the engine is being driven the way production drives it, and
    # strict typing is what catches that.
    class _Gate:
        def run(self, cmd: str, *, timeout: int | None = None) -> GateResult:
            raise AssertionError(f"no gate should run in this test, got {cmd!r}")

    class _Probe:
        def newest_age_secs(self, pattern: str, now: int) -> int | None:
            return None

    def _windowed(name: str, offset: int) -> Reminder:
        return Reminder(
            name=name,
            emit=Emit(kind=EmitKind.NOTE, title=name),
            cadence_secs=HOUR,
            cadence_offset_secs=offset,
            cadence_window_secs=HALF,
        )

    # Both last fired at 0. At now=HOUR the PLAIN elapsed rule calls both due (3600
    # elapsed >= 3600). The phase rule calls only phase_a due: phase_b's instant is 1800
    # and now is a full window past it. So this state discriminates, where a state whose
    # two rules happen to agree would not.
    cfg = TickConfig(reminders=(_windowed("phase_a", 0), _windowed("phase_b", HALF)))
    result = run_tick(
        cfg,
        OpsState.default(),
        now=HOUR,
        fired={"phase_a": 0, "phase_b": 0},
        gate_runner=_Gate(),
        age_probe=_Probe(),
    )
    assert result.fired.get("phase_a") == HOUR
    assert result.fired.get("phase_b") == 0, "the other phase must not run on this tick"


def test_positional_construction_is_unchanged_by_the_new_fields() -> None:
    """The new fields must be APPENDED. Inserted before requires_flags/gate/depends_on they
    silently rebind existing positional callers -- the flags tuple would land in
    cadence_offset_secs and nothing would complain."""
    from tick_hub.model import Gate

    gate = Gate(cmd="true")
    rem = Reminder(
        "positional",
        Emit(kind=EmitKind.NOTE, title="t"),
        3600,
        ("some_flag",),
        gate,
        ("other",),
    )
    assert rem.requires_flags == ("some_flag",)
    assert rem.gate is gate
    assert rem.depends_on == ("other",)
    assert rem.cadence_offset_secs is None
    assert rem.cadence_window_secs is None
