"""The direct same-UID census keeps evidence it found before a holder exits."""

from __future__ import annotations

import os
import socket
from pathlib import Path

import pytest

from wrkslots import cli as wrkslots

PID = 4242
REFUSAL = (
    rf"^PID {PID} identity changed after direct census matched a selected "
    r"path; the selected path may still be in use$"
)


def _budget() -> wrkslots._ReadOnlyCommandBudget:
    return wrkslots._ReadOnlyCommandBudget.start(
        timeout_seconds=30, stdout_limit=64, stderr_limit=64
    )


def _process() -> wrkslots._AbsentProcessObservation:
    return wrkslots._AbsentProcessObservation(PID, 17, "", "mnt:[1]")


def _proc(tmp_path: Path, **links: Path) -> Path:
    """Build /proc/<PID> with cwd/root/exe and fd entries, defaulting outside."""

    proc_root = tmp_path / "proc"
    base = proc_root / str(PID)
    (base / "fd").mkdir(parents=True)
    outside = tmp_path / "outside"
    outside.mkdir(exist_ok=True)
    for name in ("cwd", "root", "exe"):
        destination = links.pop(name, outside)
        if destination != Path("/missing"):
            (base / name).symlink_to(destination)
    for name, destination in links.items():
        (base / "fd" / name.removeprefix("fd")).symlink_to(destination)
    return proc_root


def _target(tmp_path: Path) -> tuple[Path, Path]:
    target = tmp_path / "slots" / "selected"
    target.mkdir(parents=True)
    held = target / "held"
    held.write_bytes(b"held")
    return target, held


def _patch(monkeypatch: pytest.MonkeyPatch, *, current: bool) -> None:
    monkeypatch.setattr(wrkslots, "_process_filesystem_uid", lambda _path: os.getuid())
    monkeypatch.setattr(
        wrkslots, "_process_generation_is_current", lambda _process: current
    )


def _link_evidence(tmp_path: Path) -> tuple[Path, dict[Path, str], dict[tuple[int, int], tuple[tuple[str, str], ...]]]:
    target, held = _target(tmp_path)
    return _proc(tmp_path, fd0=held), {target: str(target)}, {}


def _inode_evidence(tmp_path: Path) -> tuple[Path, dict[Path, str], dict[tuple[int, int], tuple[tuple[str, str], ...]]]:
    target, held = _target(tmp_path)
    alias = tmp_path / "outside-alias"
    os.link(held, alias)
    identity = held.stat()
    return (
        _proc(tmp_path, fd0=alias),
        {target: str(target)},
        {(identity.st_dev, identity.st_ino): ((str(target), f"{held}"),)},
    )


def _vanished_evidence(tmp_path: Path) -> tuple[Path, dict[Path, str], dict[tuple[int, int], tuple[tuple[str, str], ...]]]:
    # cwd matches, then root is already gone: the exit is seen mid-census.
    target, _held = _target(tmp_path)
    return _proc(tmp_path, cwd=target, root=Path("/missing")), {target: str(target)}, {}


EVIDENCE = {
    "link": _link_evidence,
    "inode": _inode_evidence,
    "vanished-mid-census": _vanished_evidence,
}


@pytest.mark.parametrize("evidence", ("link", "inode"))
def test_current_holder_reports_its_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, evidence: str
) -> None:
    proc_root, targets, inodes = EVIDENCE[evidence](tmp_path)
    _patch(monkeypatch, current=True)

    links, identities, sockets, fallback = wrkslots._direct_process_identity_matches(
        (_process(),), targets, inodes, _budget(), proc_root=proc_root
    )

    assert [(pid, kind) for pid, _slot, kind, _detail in (*links, *identities)] == [
        (PID, evidence)
    ]
    assert sockets == {}
    assert fallback == ()


@pytest.mark.parametrize("evidence", tuple(EVIDENCE))
def test_holder_that_exits_after_matching_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, evidence: str
) -> None:
    proc_root, targets, inodes = EVIDENCE[evidence](tmp_path)
    _patch(monkeypatch, current=False)

    with pytest.raises(wrkslots.Refusal, match=REFUSAL) as refused:
        wrkslots._direct_process_identity_matches(
            (_process(),), targets, inodes, _budget(), proc_root=proc_root
        )
    # Not a retryable evidence change: the next snapshot may miss nothing, yet
    # a child forked before the exit still holds the path.
    assert type(refused.value) is wrkslots.Refusal


def test_holder_that_exits_without_a_match_is_omitted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    target, _held = _target(tmp_path)
    proc_root = _proc(tmp_path, fd0=tmp_path / "outside")
    _patch(monkeypatch, current=False)

    assert wrkslots._direct_process_identity_matches(
        (_process(),), {target: str(target)}, {}, _budget(), proc_root=proc_root
    ) == ((), (), {}, ())


@pytest.mark.parametrize("current", (True, False))
def test_socket_only_holder_exit_is_left_to_the_unix_association(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, current: bool
) -> None:
    target, _held = _target(tmp_path)
    bound = tmp_path / "s"
    holder = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        holder.bind(str(bound))
        proc_root = _proc(tmp_path, fd0=bound)
        _patch(monkeypatch, current=current)

        result = wrkslots._direct_process_identity_matches(
            (_process(),), {target: str(target)}, {}, _budget(), proc_root=proc_root
        )
    finally:
        holder.close()

    expected = {bound.stat().st_ino: {PID}} if current else {}
    assert result == ((), (), expected, ())


def test_unix_association_refuses_a_target_socket_whose_holder_exited(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Downstream of the omitted holder: its still-live socket is listed bound
    # inside the target with no current holder.
    target, _held = _target(tmp_path)
    representative = wrkslots._AbsentProcessObservation(4343, 18, "", "mnt:[1]")
    table = (
        b"Num RefCount Protocol Flags Type St Inode Path\n"
        + f"0: 2 0 0 1 1 991 {target}/gone.sock\n".encode()
    )
    monkeypatch.setattr(
        wrkslots, "_batch_network_namespaces", lambda *_args: {4343: "net:[1]"}
    )
    monkeypatch.setattr(wrkslots, "_process_generation_is_current", lambda _p: True)
    monkeypatch.setattr(
        wrkslots, "_run_root_owned_command", lambda *_a, **_k: (0, table, b"")
    )

    with pytest.raises(wrkslots.Refusal, match="without a stable observed holder"):
        wrkslots._batch_unix_socket_matches(
            (representative,), {target: str(target)}, {}, _budget()
        )


def test_holder_needing_fallback_keeps_the_evidence_it_found(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # cwd matched before the descriptor table became unreadable; the
    # privileged fallback may then find the process gone.
    target, _held = _target(tmp_path)
    proc_root = _proc(tmp_path, cwd=target)
    descriptors = proc_root / str(PID) / "fd"
    descriptors.chmod(0)
    try:
        if os.access(descriptors, os.R_OK):
            pytest.skip("descriptor permissions are not enforced for this user")
        _patch(monkeypatch, current=True)
        links, identities, sockets, fallback = (
            wrkslots._direct_process_identity_matches(
                (_process(),), {target: str(target)}, {}, _budget(),
                proc_root=proc_root,
            )
        )
    finally:
        descriptors.chmod(0o700)

    assert [(pid, kind, detail) for pid, _slot, kind, detail in links] == [
        (PID, "link", str(target))
    ]
    assert identities == ()
    assert sockets == {}
    assert fallback == (_process(),)


def test_holder_needing_fallback_keeps_an_inode_match(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The cwd lies outside every target but its inode is a selected identity,
    # and the descriptor table is unreadable.
    target, held = _target(tmp_path)
    alias_directory = tmp_path / "alias-cwd"
    alias_directory.mkdir()
    identity = alias_directory.stat()
    proc_root = _proc(tmp_path, cwd=alias_directory)
    descriptors = proc_root / str(PID) / "fd"
    descriptors.chmod(0)
    inodes = {(identity.st_dev, identity.st_ino): ((str(target), f"{held}"),)}
    try:
        if os.access(descriptors, os.R_OK):
            pytest.skip("descriptor permissions are not enforced for this user")
        _patch(monkeypatch, current=True)
        links, identities, sockets, fallback = (
            wrkslots._direct_process_identity_matches(
                (_process(),), {target: str(target)}, inodes, _budget(),
                proc_root=proc_root,
            )
        )
    finally:
        descriptors.chmod(0o700)

    assert links == ()
    assert [(pid, kind) for pid, _slot, kind, _detail in identities] == [
        (PID, "inode")
    ]
    assert sockets == {}
    assert fallback == (_process(),)
