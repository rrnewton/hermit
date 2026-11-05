"""The cadence / due-logic engine and the per-reminder fired-state store.

The due-logic is PURE and deterministic: given the reminder set, the last-fired epochs, and an
explicit ``now``, it decides which reminders should be CHECKED this tick. ``now`` is a parameter (not
``time.time()``) so tests can pin the clock at exact cadence boundaries.

The fired-state store is a tiny ``key=epoch-or-count`` text file, the same crash-safe,
human-readable format the tool generalizes. Reminder-name keys hold last-fired epochs; a reserved
internal namespace holds retry diagnostics such as repeated render failures. A missing reminder key
means "never fired" = due.
"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from pathlib import Path

from tick_hub.model import EVERY_TICK, Reminder

_LINE_RE = re.compile(r"^([^=\s]+)=([0-9]+)$")
MAX_STATE_VALUE = 2**63 - 1

# Reminder names may never enter this namespace. Keeping internal diagnostics in the existing
# atomic state file makes one flushed tick update cadence and retry accounting together.
INTERNAL_STATE_PREFIX = "__tick_hub_internal__."
UNRESOLVED_RENDER_STATE_PREFIX = INTERNAL_STATE_PREFIX + "unresolved_render."
UNRESOLVED_RENDER_COUNT_SUFFIX = ".count"
UNRESOLVED_RENDER_FIRST_SUFFIX = ".first_failure_epoch"


def unresolved_render_state_keys(name: str) -> tuple[str, str]:
    """Return the reserved ``(count, first-failure-epoch)`` keys for ``name``."""
    base = UNRESOLVED_RENDER_STATE_PREFIX + name
    return base + UNRESOLVED_RENDER_COUNT_SUFFIX, base + UNRESOLVED_RENDER_FIRST_SUFFIX


def scheduled_instant(cadence_secs: int, offset_secs: int, now: int) -> int:
    """The most recent absolute instant at or before ``now`` for this phased cadence.

    The instants are ``... , offset, offset + cadence, offset + 2*cadence, ...``. Derived from the
    clock alone, so a restart mid-cycle and a replay at a pinned ``now`` both land on the same
    instant -- there is no tick counter to lose."""
    return ((now - offset_secs) // cadence_secs) * cadence_secs + offset_secs


def is_due(
    name: str,
    cadence_secs: int,
    now: int,
    last_fired: Mapping[str, int],
    offset_secs: int | None = None,
    window_secs: int | None = None,
) -> bool:
    """True iff the named reminder should be checked this tick.

    A cadence of :data:`~tick_hub.model.EVERY_TICK` (0) is always due. Otherwise the reminder is due
    when at least ``cadence_secs`` have elapsed since it last fired; a reminder with no recorded
    last-fired epoch has never run and is due.

    ``offset_secs`` PHASES the cadence against absolute time. When it is ``None`` the elapsed rule
    above applies exactly as before -- callers that configure no offset see no behaviour change.
    When it is set, the reminder is due once per ``cadence_secs`` window, at the first check at or
    after :func:`scheduled_instant`. Two reminders sharing a cadence but given different offsets
    therefore never come due on the same tick, which is the point: it lets an expensive cohort be
    spread across the ticks inside one cadence period without any reminder running less often.

    ⚠️ A REMINDER THAT WAS CUT OFF IS STILL DUE. Dueness is decided from the last-fired epoch, and
    a reminder that did not complete never records one. Phasing does not and must not change that:
    it moves WHEN a reminder is checked, never whether a check that did not happen counts as done.
    """
    if cadence_secs <= EVERY_TICK:
        return True
    last = last_fired.get(name)
    if last is None:
        # ⚠️ NEVER FIRED IS DUE, PHASE OR NO PHASE, AND THAT IS LOAD-BEARING ELSEWHERE.
        # The pending report calls this with an EMPTY fired-state precisely so every
        # reminder answers "due", which is what makes it a complete inventory rather than
        # a view of one phase. Narrowing it here would quietly shrink that report.
        # RESIDUAL, ACCEPTED AND NOT HIDDEN: a reminder that has never once completed --
        # wrkslots_audit ends NO_RESULT every tick and records no epoch -- is therefore
        # offered on both phases until it completes once. That is one cheap gate, not the
        # cohort pile-up this phasing exists to stop; a reminder that HAS completed and is
        # later cut off is bounded by the window below.
        return True
    if offset_secs is None:
        return (now - last) >= cadence_secs
    instant = scheduled_instant(cadence_secs, offset_secs, now)
    if last >= instant:
        return False
    if window_secs is not None and (now - instant) >= window_secs:
        # Past this phase's window. The reminder is still UNFINISHED -- no epoch was
        # recorded and its next instant will offer it again -- but it is not carried
        # onto the following phase's tick, which exists to run the other half.
        return False
    return True


def due_reminders(
    reminders: Sequence[Reminder], now: int, last_fired: Mapping[str, int]
) -> list[Reminder]:
    """The subset of ``reminders`` due this tick, in registration order (pure)."""
    return [
        r
        for r in reminders
        if is_due(
            r.name,
            r.cadence_secs,
            now,
            last_fired,
            r.cadence_offset_secs,
            r.cadence_window_secs,
        )
    ]


def load_fired_state(path: Path) -> dict[str, int]:
    """Load the ``key=epoch-or-count`` store.

    Missing files and malformed lines yield an empty or partial map. A reminder absent from the map
    is treated as never-fired (= due); reserved internal entries never participate in cadence.
    """
    state: dict[str, int] = {}
    if not path.is_file():
        return state
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return state
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = _LINE_RE.match(line)
        if m:
            epoch = int(m.group(2))
            if epoch <= MAX_STATE_VALUE:
                state[m.group(1)] = epoch
    return state


def persist_fired_state(path: Path, state: Mapping[str, int]) -> None:
    """Atomically write the fired-state store (temp file + rename)."""
    for key, value in state.items():
        if (
            not isinstance(key, str)
            or not key
            or "=" in key
            or any(char.isspace() for char in key)
            or isinstance(value, bool)
            or not isinstance(value, int)
            or value < 0
            or value > MAX_STATE_VALUE
        ):
            raise ValueError(f"invalid fired-state entry {key!r}={value!r}")
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    lines = [
        "# tick-hub fired-state — reminder=last_fired_epoch (managed by tick-hub)",
        f"# {INTERNAL_STATE_PREFIX}* entries are reserved retry diagnostics",
    ]
    for key in sorted(state):
        lines.append(f"{key}={state[key]}")
    try:
        tmp.write_text("\n".join(lines) + "\n", encoding="utf-8")
        tmp.replace(path)
    except OSError:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise
