"""Deterministic process-liveness checks for timeout teardown."""

from __future__ import annotations

import os
import signal
from dataclasses import dataclass
from pathlib import Path

import pytest

import dagrun.teardown as teardown
from dagrun.procstat import parse_process_stat
from dagrun.teardown import (
    _hard_reap,
    _kill_descendants,
    _live_process_group_from_stat,
    _live_process_groups,
    _proc_descendants,
    _root_can_have_descendants,
)


def _stat_record(
    pid: int, comm: bytes, *, state: bytes = b"S", ppid: int = 1,
    pgrp: int = 777, session: int = 777, starttime: int = 424242,
) -> bytes:
    fields = [state, str(ppid).encode(), str(pgrp).encode(), str(session).encode()]
    fields.extend([b"0"] * 7)
    fields.extend([b"13", b"17"])
    fields.extend([b"0"] * 6)
    fields.append(str(starttime).encode())
    return str(pid).encode() + b" (" + comm + b") " + b" ".join(fields) + b"\n"


def test_proc_stat_parser_excludes_zombies_from_the_term_grace() -> None:
    """An unreaped cooperative child must not be charged the full SIGTERM grace."""
    assert _live_process_group_from_stat(
        _stat_record(123, b"worker ) with parens", state=b"Z")
    ) is None
    assert _live_process_group_from_stat(
        _stat_record(456, b"worker ) with parens", pgrp=888)
    ) == 888
    assert _live_process_group_from_stat("malformed") is None
    assert _live_process_group_from_stat("1 (x) S 0 nope") is None


def test_proc_stat_parser_treats_comm_as_opaque_bytes() -> None:
    """Invalid UTF-8, newlines, and parentheses in comm cannot shift identity fields."""
    raw = _stat_record(
        456,
        b"p\xff)\n(\x80",
        ppid=123,
        pgrp=888,
        session=999,
        starttime=987654321,
    )
    parsed = parse_process_stat(raw)
    assert parsed is not None
    assert (
        parsed.pid,
        parsed.state,
        parsed.ppid,
        parsed.pgrp,
        parsed.session,
        parsed.utime,
        parsed.stime,
        parsed.starttime,
    ) == (456, "S", 123, 888, 999, 13, 17, 987654321)
    assert parse_process_stat(raw.decode("ascii", errors="surrogateescape")) == parsed
    assert _live_process_group_from_stat(raw) == 888


@pytest.mark.parametrize(
    "comm",
    [b"", b") ", b"many ) embedded ) parens", b"line one\nline two"],
)
def test_proc_stat_parser_accepts_every_opaque_comm_shape(comm: bytes) -> None:
    parsed = parse_process_stat(_stat_record(456, comm, pgrp=888, starttime=12345))
    assert parsed is not None
    assert (parsed.pgrp, parsed.starttime) == (888, 12345)


@pytest.mark.parametrize(
    "raw",
    [
        b"456 (unterminated S 1 2 3",
        b"456 (x)S 1 2 3",
        b"456 (x) not-a-state 1 2 3 " + b"0 " * 20,
        b"456 (x) \xff 1 2 3 " + b"0 " * 20,
        b"456 (x) ? 1 2 3 " + b"0 " * 20,
        b"456 (x) S 1 2 3",
        _stat_record(456, b"x", pgrp=-1),
        _stat_record(1 << 31, b"x"),
        _stat_record(456, b"x", starttime=1 << 64),
    ],
)
def test_proc_stat_parser_refuses_malformed_or_out_of_range_fields(raw: bytes) -> None:
    assert parse_process_stat(raw) is None


def test_live_process_group_scan_tolerates_vanished_proc_entries(tmp_path: Path) -> None:
    """A PID may disappear between the directory snapshot and its stat read."""
    (tmp_path / "101").mkdir()
    (tmp_path / "101" / "stat").write_bytes(
        _stat_record(101, b"p\xff)\n(\x80", pgrp=700)
    )
    (tmp_path / "102").mkdir()  # no stat: this process exited after listdir
    (tmp_path / "self").mkdir()

    assert _live_process_groups({700, 800}, proc_root=tmp_path) == {700}


def test_descendant_scan_treats_process_names_as_opaque_bytes(tmp_path: Path) -> None:
    """Even a comm resembling a status field cannot forge the parent relationship."""
    for pid, parent, comm in (
        (101, 100, b"p\xff)\n(\x80"),
        (102, 101, b"x\nPPid:\t999"),
    ):
        directory = tmp_path / str(pid)
        directory.mkdir()
        (directory / "stat").write_bytes(_stat_record(pid, comm, ppid=parent))
    (tmp_path / "103").mkdir()  # vanished before its stat could be opened
    assert _proc_descendants(100, proc_root=tmp_path) == ([102, 101], True)


@pytest.mark.parametrize("bad_stat", [b"malformed", None])
def test_proc_scans_retain_incompleteness_for_bad_entries(
    tmp_path: Path, bad_stat: bytes | None,
) -> None:
    """Malformed data and EISDIR are not evidence that a listed PID vanished."""
    stat_path = tmp_path / "101" / "stat"
    stat_path.parent.mkdir()
    if bad_stat is None:
        stat_path.mkdir()
    else:
        stat_path.write_bytes(bad_stat)

    assert _live_process_groups({700}, proc_root=tmp_path) is None
    assert _proc_descendants(100, proc_root=tmp_path) == ([], False)


def test_live_process_group_scan_rejects_a_mismatched_pid_record(tmp_path: Path) -> None:
    (tmp_path / "101").mkdir()
    (tmp_path / "101" / "stat").write_bytes(
        _stat_record(999, b"replacement", pgrp=700)
    )
    assert _live_process_groups({700}, proc_root=tmp_path) is None


def test_live_process_group_scan_distinguishes_unavailable_proc_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Failure to enumerate procfs remains unknown, not an empty liveness proof."""
    original_listdir = os.listdir

    def disappearing(path: os.PathLike[str] | str) -> list[str]:
        if Path(path) == tmp_path:
            raise FileNotFoundError(path)
        return original_listdir(path)

    monkeypatch.setattr(os, "listdir", disappearing)
    assert _live_process_groups({700}, proc_root=tmp_path) is None
    assert _proc_descendants(100, proc_root=tmp_path) == ([], False)


def test_descendant_kill_requires_a_complete_empty_sweep(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Known descendants are retried until a later complete scan proves emptiness."""
    scans = iter([([919191], False), ([919191], True), ([], True)])
    signals: list[tuple[int, signal.Signals]] = []
    monkeypatch.setattr(teardown, "_proc_descendants", lambda _root: next(scans))
    monkeypatch.setattr(os, "kill", lambda pid, sig: signals.append((pid, sig)))

    assert _kill_descendants(818181) == 1
    assert signals == [
        (919191, signal.SIGKILL),
        (919191, signal.SIGKILL),
    ]


def test_only_a_running_root_can_have_descendants_to_sweep(tmp_path: Path) -> None:
    """A zombie or vanished root has no children left to reach, so its sweep is skipped."""
    for pid, record, tasks in (
        (100, _stat_record(100, b"runner"), (100,)),
        (200, _stat_record(200, b"exited", state=b"Z"), (200,)),
        (300, b"malformed", (300,)),
        (500, _stat_record(500, b"leader", state=b"Z"), (500, 501)),
    ):
        (tmp_path / str(pid)).mkdir()
        (tmp_path / str(pid) / "stat").write_bytes(record)
        for task in tasks:
            (tmp_path / str(pid) / "task" / str(task)).mkdir(parents=True)

    assert _root_can_have_descendants(100, proc_root=tmp_path)
    assert not _root_can_have_descendants(200, proc_root=tmp_path)
    assert not _root_can_have_descendants(400, proc_root=tmp_path)
    # An unreadable record proves nothing, so it keeps the sweep.
    assert _root_can_have_descendants(300, proc_root=tmp_path)
    # A leader that called pthread_exit is a zombie whose other threads still
    # hold its children.
    assert _root_can_have_descendants(500, proc_root=tmp_path)


@dataclass
class _Leader:
    pid: int


def test_hard_reap_sweeps_descendants_only_from_a_running_root(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The unboxed parentage sweep runs for a running root and is skipped for a dead one."""
    swept: list[int] = []
    running = {818181: True, 828282: False}

    def kill_descendants(root: int) -> int:
        swept.append(root)
        return 0

    monkeypatch.setattr(os, "killpg", lambda _pgid, _sig: None)
    monkeypatch.setattr(teardown, "_kill_descendants", kill_descendants)
    monkeypatch.setattr(
        teardown, "_root_can_have_descendants", lambda root: running[root]
    )
    for pid in running:
        _hard_reap(_Leader(pid), None, "g.step", None)

    assert swept == [818181]


def test_descendant_kill_warns_without_a_complete_empty_sweep(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    """A bounded, incomplete procfs scan must fail visibly rather than report success."""
    monkeypatch.setattr(teardown, "_DESCENDANT_KILL_SWEEPS", 2)
    monkeypatch.setattr(teardown, "_proc_descendants", lambda _root: ([], False))

    assert _kill_descendants(818181) == 0
    assert "could not confirm an empty descendant tree" in capsys.readouterr().err
