"""The tick engine: turn (config + state + fired-state + now) into the emitted lines.

One call to :func:`run_tick` is one heartbeat. It:

1. evaluates every :class:`~tick_hub.model.HealthCheck` (a HEALTH line each),
2. emits the ops-state machine's own lines (summary NOTE, tick-frequency actualization, disabled
   NOTE) via :func:`~tick_hub.state.state_lines`,
3. when the hub is enabled, checks each DUE reminder whose ``requires_flags`` are all truthy: runs
   its optional gate, records that the check ran, and emits its ACTION/NOTE when the gate fires, and
4. appends a trailing NOTE with the count of instructions emitted.

The heavy lifting (running gate commands, measuring file ages) is delegated to the pluggable
:mod:`tick_hub.protocols` boundaries, so the ordering / gating / emission logic here is pure and
deterministic given ``now`` and the injected probes.
"""

from __future__ import annotations

import re
from concurrent.futures import Future, ThreadPoolExecutor
from collections.abc import Callable, Mapping, Sequence, Set as AbstractSet
from dataclasses import dataclass
from enum import Enum

from tick_hub.cadence import (
    MAX_STATE_VALUE,
    UNRESOLVED_RENDER_COUNT_SUFFIX,
    UNRESOLVED_RENDER_FIRST_SUFFIX,
    UNRESOLVED_RENDER_STATE_PREFIX,
    is_due,
    unresolved_render_state_keys,
)
from tick_hub.emit import (
    format_no_result,
    HEALTH_STATUS_MISSING,
    HEALTH_STATUS_OK,
    HEALTH_STATUS_STALE,
    format_action,
    format_clean,
    format_error,
    format_health,
    format_note,
    format_suppressed,
    format_unevaluable,
)
from tick_hub.model import Emit, EmitKind, Gate, GateWhen, HealthCheck, Reminder, TickConfig
from tick_hub.protocols import FileAgeProbe, GateResult, GateRunner, ParallelGateRunner
from tick_hub.state import OpsState, flag_truthy, state_lines

MAX_PARALLEL_GATES = 8


@dataclass(frozen=True)
class TickResult:
    """Everything one tick produced: the emitted lines, the new fired-state, the ACTION count."""

    lines: tuple[str, ...]
    fired: Mapping[str, int]
    actions_emitted: int


class UnresolvedPlaceholderError(ValueError):
    """An emission still contains a template placeholder after rendering."""

    def __init__(
        self, message: str, *, placeholders: Sequence[str] = ()
    ) -> None:
        # Keep the historical ValueError(message) construction valid for callers
        # while giving the engine structured names for its NO-SIGNAL record.
        self.placeholders = tuple(sorted(set(placeholders)))
        super().__init__(message)


_UNRESOLVED_PLACEHOLDER = re.compile(
    r"(?<!\{)\{[A-Za-z_][A-Za-z0-9_.-]*\}(?!\})"
)


def parse_kv_lines(text: str) -> dict[str, str]:
    """Parse a gate command's stdout ``key=value`` lines (blank / ``#`` lines ignored)."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" in line:
            key, _, value = line.partition("=")
            key = key.strip()
            if key:
                out[key] = value.strip()
    return out


def evaluate_health(hc: HealthCheck, probe: FileAgeProbe, now: int) -> str:
    """Evaluate one freshness check and return its formatted health record."""
    age = probe.newest_age_secs(hc.glob, now)
    if age is None:
        status = HEALTH_STATUS_MISSING
    elif age <= hc.threshold_secs:
        status = HEALTH_STATUS_OK
    else:
        status = HEALTH_STATUS_STALE
    return format_health(hc.name, status, age, hc.threshold_secs, hc.detail)


#: Exit code reserved for "I ran, and I could not determine my condition".
#:
#: 75 is EX_TEMPFAIL ("temporary failure, user is invited to retry"), chosen on
#: collision evidence rather than taste: across the scripts behind the live gate
#: set the codes already in use are 0, 1, 2, 3, 124 and 127. Code 2 is the
#: tempting pick because one gate already prints NO_RESULT while exiting 2 --
#: and it is WRONG, because argparse exits 2 on a usage error, so reserving it
#: would render a CRASHED gate as a non-answer. Softening a real failure into
#: NO_RESULT is the opposite of the point.
NO_RESULT_EXIT = 75

# Preserve the ordinary retry signal twice; the third consecutive failure adds a distinct
# escalation without replacing the base ERROR/NO-SIGNAL records.
REPEATED_RENDER_FAILURE_THRESHOLD = 3


class GateOutcome(Enum):
    """What one gate execution concluded."""

    QUIET = "quiet"
    FIRE = "fire"
    NO_RESULT = "no-result"


@dataclass(frozen=True)
class _ReminderEvaluation:
    reminder: Reminder
    outcome: GateOutcome
    captured: dict[str, str]
    error: str | None


def _gate_fires(when: GateWhen, returncode: int, stdout: str) -> bool:
    if when is GateWhen.SUCCESS:
        return returncode == 0
    if when is GateWhen.FAILURE:
        return returncode != 0
    if when is GateWhen.NONEMPTY:
        return stdout.strip() != ""
    return True  # GateWhen.ALWAYS


def _eval_gate(
    gate: Gate | None, runner: GateRunner
) -> tuple[GateOutcome, dict[str, str], str | None]:
    """Return (outcome, captured_fields, error). ``error`` non-None means the gate could not run."""
    if gate is None:
        return GateOutcome.FIRE, {}, None
    # Preserve compatibility with simple runners that implement the original
    # one-argument protocol unless this gate explicitly opts into an override.
    result = (
        runner.run(gate.cmd)
        if gate.timeout_secs is None
        else runner.run(gate.cmd, timeout=gate.timeout_secs)
    )
    return _interpret_gate_result(gate, result)


def _interpret_gate_result(
    gate: Gate, result: GateResult
) -> tuple[GateOutcome, dict[str, str], str | None]:
    """Interpret one already-completed gate process result."""
    if not result.ok:
        return GateOutcome.QUIET, {}, f"gate command could not run ({gate.cmd!r}): {result.error}"
    captured = parse_kv_lines(result.stdout) if gate.capture else {}
    # Checked BEFORE ``when``, so a gate that cannot determine its condition is
    # never reinterpreted through a fire/quiet rule. Under ``when: failure`` a 75
    # would otherwise read as an ordinary failure verdict; under ``when: success``
    # it would read as a CLEAN PASS, which is the silence-means-healthy collapse
    # this exists to remove.
    if result.returncode == NO_RESULT_EXIT:
        return GateOutcome.NO_RESULT, captured, None
    fires = _gate_fires(gate.when, result.returncode, result.stdout)
    return (GateOutcome.FIRE if fires else GateOutcome.QUIET), captured, None


def _interpolate(text: str, captured: Mapping[str, str]) -> str:
    for key, value in captured.items():
        text = text.replace("{" + key + "}", value)
    return text


def render_emit(emit: Emit, captured: Mapping[str, str]) -> str:
    """Render a fired reminder's :class:`~tick_hub.model.Emit` into a line.

    Captured gate values override matching static fields; then ``{key}`` placeholders in the title
    and in each field value are resolved from the merged set (static fields + captured), so a title
    can reference both a static ``{threshold}`` and a captured ``{count}``."""
    merged = dict(emit.fields)
    for key, value in captured.items():
        merged[key] = value
    for key in emit.fields:
        if key not in captured:
            merged[key] = _interpolate(merged[key], merged)
    title = _interpolate(emit.title, merged)
    unresolved: set[str] = set(_UNRESOLVED_PLACEHOLDER.findall(title))
    for value in merged.values():
        unresolved.update(_UNRESOLVED_PLACEHOLDER.findall(value))
    if unresolved:
        names = ", ".join(sorted(unresolved))
        raise UnresolvedPlaceholderError(
            f"refusing emission with unresolved placeholder(s): {names}",
            placeholders=tuple(unresolved),
        )
    if emit.kind is EmitKind.NOTE:
        return format_note(title)
    return format_action(emit.skill, merged, title)


def _no_signal_action(
    reminder: Reminder,
    reason: str,
    *,
    detail: str = "",
    missing_placeholders: Sequence[str] = (),
) -> str:
    """Make a reporting failure visible to ACTION-only tick consumers.

    The original domain action was not emitted, so this record never borrows its
    fields or claims its verdict. It says only that the named gate produced no
    usable signal and why. Reusing the configured skill preserves the caller's
    routing; notes without a skill use a generic reporting handler name.
    """
    fields = {
        "component": "tick-hub-reporting",
        "outcome": "NO-SIGNAL",
        "gate": reminder.name,
        "reason": reason,
    }
    title = f"NO-SIGNAL gate={reminder.name}: {reason}"
    if missing_placeholders:
        missing = ",".join(
            placeholder.removeprefix("{").removesuffix("}")
            for placeholder in missing_placeholders
        )
        fields["missing_placeholders"] = missing
        title += f"; missing placeholder(s)={missing}"
    if detail:
        fields["detail"] = " ".join(detail.split())[:240]
    return format_action(
        reminder.emit.skill or "tick-hub-no-signal",
        fields,
        title,
    )


def _placeholder_names(placeholders: Sequence[str]) -> str:
    return ",".join(
        placeholder.removeprefix("{").removesuffix("}")
        for placeholder in placeholders
    )


def _repeated_render_failure_action(
    reminder: Reminder,
    *,
    consecutive_failures: int,
    first_failure_epoch: int,
    missing_placeholders: Sequence[str],
) -> str:
    """Repeat a render fault with its persisted count without replacing the base alarm."""
    missing = _placeholder_names(missing_placeholders)
    fields = {
        "component": "tick-hub-reporting",
        "outcome": "NO-SIGNAL",
        "gate": reminder.name,
        "reason": "unresolved-placeholder",
        "consecutive_failures": str(consecutive_failures),
        "first_failure_epoch": str(first_failure_epoch),
        "missing_placeholders": missing,
    }
    title = (
        f"NO-SIGNAL gate={reminder.name}: unresolved-placeholder repeated for "
        f"{consecutive_failures} consecutive render failures since first_failure_epoch="
        f"{first_failure_epoch}; missing placeholder(s)={missing}"
    )
    return format_action(
        reminder.emit.skill or "tick-hub-no-signal",
        fields,
        title,
    )


def _clear_render_failure_state(state: dict[str, int], reminder_name: str) -> None:
    count_key, first_key = unresolved_render_state_keys(reminder_name)
    state.pop(count_key, None)
    state.pop(first_key, None)


def _record_render_failure(
    state: dict[str, int], reminder_name: str, now: int
) -> tuple[int, int]:
    count_key, first_key = unresolved_render_state_keys(reminder_name)
    previous_count = state.get(count_key, 0)
    consecutive = min(MAX_STATE_VALUE, max(0, previous_count) + 1)
    first_failure_epoch = (
        max(0, now)
        if previous_count <= 0
        else max(0, state.get(first_key, max(0, now)))
    )
    state[count_key] = consecutive
    state[first_key] = first_failure_epoch
    return consecutive, first_failure_epoch


def _prune_removed_render_failure_state(
    state: dict[str, int], reminder_names: set[str]
) -> None:
    """Drop this feature's malformed keys and records for reminders no longer configured."""
    for key in tuple(state):
        if not key.startswith(UNRESOLVED_RENDER_STATE_PREFIX):
            continue
        if key.endswith(UNRESOLVED_RENDER_COUNT_SUFFIX):
            name = key[
                len(UNRESOLVED_RENDER_STATE_PREFIX) : -len(UNRESOLVED_RENDER_COUNT_SUFFIX)
            ]
        elif key.endswith(UNRESOLVED_RENDER_FIRST_SUFFIX):
            name = key[
                len(UNRESOLVED_RENDER_STATE_PREFIX) : -len(UNRESOLVED_RENDER_FIRST_SUFFIX)
            ]
        else:
            name = ""
        if not name or name not in reminder_names:
            state.pop(key, None)


def _flags_satisfied(required: Sequence[str], flags: Mapping[str, object]) -> bool:
    typed_flags = {k: v for k, v in flags.items() if isinstance(v, (bool, int, str))}
    return all(flag_truthy(typed_flags, name) for name in required)


def run_tick(
    config: TickConfig,
    state: OpsState,
    *,
    now: int,
    fired: Mapping[str, int],
    gate_runner: GateRunner,
    age_probe: FileAgeProbe,
    current_tick_min: int | None = None,
    report_pending: bool = False,
    emit: Callable[[str], None] | None = None,
) -> TickResult:
    """Run one tick and return the emitted lines plus the advanced fired-state (pure w.r.t. I/O,
    which is confined to the injected ``gate_runner`` / ``age_probe``).

    ``emit`` receives each report line at the moment it is produced, in the same
    order as ``TickResult.lines``, and is the difference between a tick that is
    stopped partway reporting what it had already found and one that reports
    nothing at all. A caller that runs the tick under an external time bound --
    a service manager, a supervisor, an agent runtime -- gets no output when the
    bound fires unless the lines have already left this function, because the
    return value never arrives. Passing ``emit`` makes an interrupted run
    self-describing; omitting it keeps the previous collect-then-return
    behaviour exactly.
    """
    lines: list[str] = []
    actions = 0

    def record(line: str) -> None:
        lines.append(line)
        if emit is not None:
            emit(line)

    for hc in config.health_checks:
        record(evaluate_health(hc, age_probe, now))

    for line in state_lines(state, current_tick_min):
        record(line)
        if line.startswith("ACTION: "):
            actions += 1

    new_fired = dict(fired)
    _prune_removed_render_failure_state(
        new_fired, {reminder.name for reminder in config.reminders}
    )
    if state.enabled:
        planned: list[Reminder | str] = []
        for rem in config.reminders:
            if not is_due(
                rem.name,
                rem.cadence_secs,
                now,
                fired,
                rem.cadence_offset_secs,
                rem.cadence_window_secs,
            ):
                continue
            if not _flags_satisfied(rem.requires_flags, state.flags):
                # Flag-suppressed: do NOT consume the cadence, so it fires promptly once enabled.
                if report_pending:
                    # In a pending report this must be VISIBLE. Skipping silently
                    # is how a suppressed gate becomes indistinguishable from a
                    # clean one, which is the whole reason this mode exists.
                    missing = tuple(
                        flag
                        for flag in rem.requires_flags
                        if not state.flags.get(flag, False)
                    )
                    planned.append(format_suppressed(rem.name, missing))
                continue
            planned.append(rem)

        # A dependency-free prefix is final as each gate finishes and can be
        # streamed immediately. Once a dependency-bearing reminder appears we
        # retain that reminder and everything after it: its QUIET verdict can be
        # downgraded after a later dependency runs, and emitting a later final
        # reminder around it would change config order. This lets mixed configs
        # report the maximal safe prefix without publishing a verdict that may
        # subsequently change.
        stream_prefix_len = 0
        if emit is not None:
            stream_prefix_len = next(
                (
                    index
                    for index, entry in enumerate(planned)
                    if isinstance(entry, Reminder) and entry.depends_on
                ),
                len(planned),
            )

        no_dependencies: set[str] = set()
        reports: list[_ReminderEvaluation | str] = []

        def evaluate(reminder: Reminder) -> _ReminderEvaluation:
            outcome, captured, error = _eval_gate(reminder.gate, gate_runner)
            return _ReminderEvaluation(reminder, outcome, captured, error)

        def collect(futures: Mapping[int, Future[_ReminderEvaluation]]) -> None:
            nonlocal actions
            for index, entry in enumerate(planned):
                if isinstance(entry, str):
                    report: _ReminderEvaluation | str = entry
                elif index in futures:
                    report = futures[index].result()
                else:
                    report = evaluate(entry)
                reports.append(report)
                if index < stream_prefix_len:
                    if isinstance(report, str):
                        record(report)
                    else:
                        actions += _render_evaluation(
                            report,
                            no_dependencies,
                            new_fired,
                            now,
                            record,
                            report_pending,
                        )

        parallel_indices = tuple(
            index
            for index, entry in enumerate(planned)
            if isinstance(entry, Reminder)
            and entry.gate is not None
            and entry.gate.parallel
        )
        if len(parallel_indices) >= 2 and isinstance(gate_runner, ParallelGateRunner):
            # The executor owns the shared cancellation scope. A failed output
            # write therefore exits the scope and kills outstanding gates
            # before the executor waits for its worker threads.
            with ThreadPoolExecutor(
                max_workers=min(MAX_PARALLEL_GATES, len(parallel_indices))
            ) as executor:
                with gate_runner.parallel_scope():
                    futures: dict[int, Future[_ReminderEvaluation]] = {}
                    for index in parallel_indices:
                        reminder = planned[index]
                        if isinstance(reminder, Reminder):
                            futures[index] = executor.submit(evaluate, reminder)
                    try:
                        collect(futures)
                    except BaseException:
                        for future in futures.values():
                            future.cancel()
                        raise
        else:
            collect({})
        if stream_prefix_len < len(reports):
            actions += _render_after_dependency_pass(
                reports,
                new_fired,
                now,
                record,
                report_pending,
                render_from=stream_prefix_len,
            )

    record(format_note(f"emitted {actions} instruction(s) this tick"))
    return TickResult(tuple(lines), new_fired, actions)


def _render_after_dependency_pass(
    reports: Sequence[_ReminderEvaluation | str],
    new_fired: dict[str, int],
    now: int,
    record: Callable[[str], None],
    report_pending: bool,
    *,
    render_from: int = 0,
) -> int:
    """Interpret dependency edges over completed evaluations, then render in config order."""
    evaluations = tuple(report for report in reports if not isinstance(report, str))
    # Evaluate every due gate before interpreting dependency edges. A
    # dependency never decides whether another gate runs, and config order
    # therefore cannot turn a forward reference into a false clean.
    unavailable = {
        evaluation.reminder.name
        for evaluation in evaluations
        if evaluation.error is None and evaluation.outcome is GateOutcome.NO_RESULT
    }
    changed = True
    while changed:
        changed = False
        for evaluation in evaluations:
            if evaluation.error is not None or evaluation.outcome is not GateOutcome.QUIET:
                continue
            if evaluation.reminder.name in unavailable:
                continue
            if any(
                dependency in unavailable
                for dependency in evaluation.reminder.depends_on
            ):
                unavailable.add(evaluation.reminder.name)
                changed = True

    actions = 0
    for report in reports[render_from:]:
        if isinstance(report, str):
            record(report)
        else:
            actions += _render_evaluation(
                report, unavailable, new_fired, now, record, report_pending
            )
    return actions


def _render_evaluation(
    evaluation: _ReminderEvaluation,
    unavailable: AbstractSet[str],
    new_fired: dict[str, int],
    now: int,
    record: Callable[[str], None],
    report_pending: bool,
) -> int:
    """Write one completed gate's report lines and return the actions they carry."""
    actions = 0
    rem = evaluation.reminder
    outcome = evaluation.outcome
    captured = evaluation.captured
    error = evaluation.error
    if error is not None:
        _clear_render_failure_state(new_fired, rem.name)
        # The check did not complete. Keep the ERROR for full diagnostic
        # readers and emit a counted NO-SIGNAL action because production
        # consumers may intentionally forward ACTION lines only. Retry
        # next tick by leaving the cadence unconsumed.
        record(format_error(f"reminder {rem.name}: {error}"))
        record(_no_signal_action(rem, "gate-execution-error", detail=error))
        return actions + 1
    if outcome is GateOutcome.NO_RESULT:
        _clear_render_failure_state(new_fired, rem.name)
        # A legitimate verdict, not a reporting fault: the gate ran and
        # said it could not tell. Emitted as BOTH the explicit
        # NO_RESULT diagnostic and a counted ACTION, because
        # ACTION-only consumers would otherwise see nothing -- and
        # "consumer sees nothing" is precisely the failure this code
        # exists to remove. Cadence is left unconsumed so it
        # re-announces every tick until it can determine something.
        detail = captured.get("summary", "")
        record(format_no_result(rem.name, detail))
        record(_no_signal_action(rem, "could-not-determine", detail=detail))
        return actions + 1
    if outcome is GateOutcome.QUIET:
        _clear_render_failure_state(new_fired, rem.name)
        unavailable_dependencies = tuple(
            dependency for dependency in rem.depends_on if dependency in unavailable
        )
        if unavailable_dependencies:
            record(format_unevaluable(rem.name, unavailable_dependencies))
            record(
                _no_signal_action(
                    rem,
                    "dependency-could-not-determine",
                    detail=",".join(unavailable_dependencies),
                )
            )
            return actions + 1
        if report_pending:
            record(format_clean(rem.name))
        new_fired[rem.name] = now  # the check ran; the cadence clock resets
        return actions
    try:
        line = render_emit(rem.emit, captured)
    except UnresolvedPlaceholderError as exc:
        # A templated hole is not the domain warning, so never emit the
        # malformed action. Surface a separate, counted NO-SIGNAL action
        # and leave cadence unconsumed so the reminder retries.
        record(format_error(f"reminder {rem.name}: {exc}"))
        record(
            _no_signal_action(
                rem,
                "unresolved-placeholder",
                missing_placeholders=exc.placeholders,
            )
        )
        actions += 1
        consecutive, first_failure_epoch = _record_render_failure(
            new_fired, rem.name, now
        )
        if consecutive >= REPEATED_RENDER_FAILURE_THRESHOLD:
            record(
                _repeated_render_failure_action(
                    rem,
                    consecutive_failures=consecutive,
                    first_failure_epoch=first_failure_epoch,
                    missing_placeholders=exc.placeholders,
                )
            )
            actions += 1
        return actions
    _clear_render_failure_state(new_fired, rem.name)
    new_fired[rem.name] = now
    record(line)
    if line.startswith("ACTION: "):
        actions += 1
    return actions


__all__ = [
    "TickResult",
    "run_tick",
    "evaluate_health",
    "render_emit",
    "UnresolvedPlaceholderError",
    "parse_kv_lines",
    "HEALTH_STATUS_OK",
    "HEALTH_STATUS_STALE",
    "HEALTH_STATUS_MISSING",
]
