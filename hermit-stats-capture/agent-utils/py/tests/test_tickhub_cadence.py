"""Tests for the cadence / due-logic engine and the fired-state file.

The due-logic takes an explicit ``now`` so the clock can be pinned at exact cadence boundaries.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from tick_hub.cadence import (
    INTERNAL_STATE_PREFIX,
    due_reminders,
    is_due,
    load_fired_state,
    persist_fired_state,
    unresolved_render_state_keys,
)
from tick_hub.model import Emit, EmitKind, Reminder


def test_every_tick_always_due() -> None:
    assert is_due("r", 0, now=0, last_fired={}) is True
    assert is_due("r", 0, now=100, last_fired={"r": 100}) is True  # fired this very tick


def test_never_fired_is_due() -> None:
    assert is_due("r", 3600, now=50, last_fired={}) is True
    assert is_due("r", 3600, now=50, last_fired={"other": 50}) is True


def test_boundary_exact_and_off_by_one() -> None:
    last = 1000
    cadence = 600
    # elapsed exactly == cadence -> due (>=).
    assert is_due("r", cadence, now=last + cadence, last_fired={"r": last}) is True
    # one second short -> not due.
    assert is_due("r", cadence, now=last + cadence - 1, last_fired={"r": last}) is False
    # well past -> due.
    assert is_due("r", cadence, now=last + cadence * 5, last_fired={"r": last}) is True
    # exactly at last (0 elapsed) -> not due.
    assert is_due("r", cadence, now=last, last_fired={"r": last}) is False


def test_due_reminders_preserves_order() -> None:
    def rem(name: str, cadence: int) -> Reminder:
        return Reminder(name, Emit(EmitKind.NOTE, title=name), cadence_secs=cadence)

    reminders = [rem("a", 0), rem("b", 3600), rem("c", 0)]
    fired = {"b": 1000}
    due = due_reminders(reminders, now=1500, last_fired=fired)  # b not yet due (500 < 3600)
    assert [r.name for r in due] == ["a", "c"]
    due2 = due_reminders(reminders, now=1000 + 3600, last_fired=fired)  # b now due
    assert [r.name for r in due2] == ["a", "b", "c"]


def test_fired_state_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "sub" / "state"
    count_key, first_key = unresolved_render_state_keys("a")
    state = {"a": 100, "b": 200, count_key: 3, first_key: 50}
    persist_fired_state(path, state)
    assert load_fired_state(path) == state
    assert f"# {INTERNAL_STATE_PREFIX}* entries are reserved retry diagnostics" in path.read_text()
    # missing file -> empty map (all reminders treated as never-fired).
    assert load_fired_state(tmp_path / "nope") == {}


def test_fired_state_ignores_garbage_lines(tmp_path: Path) -> None:
    path = tmp_path / "state"
    path.write_text(
        "# comment\n\nvalid=42\nmax=9223372036854775807\n"
        "overflow=9223372036854775808\nbad line\nk=notnum\n",
        encoding="utf-8",
    )
    assert load_fired_state(path) == {"valid": 42, "max": 9223372036854775807}


@pytest.mark.parametrize(
    "state",
    [
        {"": 1},
        {"bad key": 1},
        {"bad=key": 1},
        {"bad": -1},
        {"bad": 9223372036854775808},
        {"bad": True},
    ],
)
def test_fired_state_writer_rejects_unreadable_entries(
    tmp_path: Path, state: dict[str, int]
) -> None:
    with pytest.raises(ValueError):
        persist_fired_state(tmp_path / "state", state)


def test_fired_state_writer_cleans_temporary_file_after_rename_failure(tmp_path: Path) -> None:
    destination = tmp_path / "state"
    destination.mkdir()
    with pytest.raises(OSError):
        persist_fired_state(destination, {"valid": 1})
    assert not (tmp_path / "state.tmp").exists()
