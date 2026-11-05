"""Initial census-key publication survives real interruption without new authority."""

from __future__ import annotations

import contextlib
import errno
import fcntl
import hashlib
import hmac
import json
import os
import select
import signal
import stat
from pathlib import Path

import pytest

from wrkslots import cli


def _key(path: Path) -> bytes:
    parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        return cli._audit_cache_state_key(path, parent_fd)
    finally:
        os.close(parent_fd)


def _assert_key(path: Path, expected: bytes) -> None:
    metadata = path.lstat()
    assert stat.S_ISREG(metadata.st_mode)
    assert stat.S_IMODE(metadata.st_mode) == 0o600
    assert metadata.st_uid == os.geteuid()
    assert metadata.st_nlink == 1
    assert metadata.st_size == 32
    assert path.read_bytes() == expected


@pytest.mark.parametrize("failure", ["partial-write", "zero-write", "file-fsync", "directory-fsync"])
def test_failed_creation_remains_error_then_retry_recovers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failure: str
) -> None:
    path = tmp_path / "census.json"
    final = tmp_path / "census.json.key"
    pending = tmp_path / ".census.json.key.pending"
    original_write = os.write
    original_fsync = os.fsync
    writes = 0

    def failing_write(fd: int, content: bytes) -> int:
        nonlocal writes
        writes += 1
        if failure == "zero-write":
            return 0
        if writes == 1:
            return original_write(fd, content[:7])
        raise OSError(errno.ENOSPC, "injected full storage")

    def failing_fsync(fd: int) -> None:
        directory = stat.S_ISDIR(os.fstat(fd).st_mode)
        if directory == (failure == "directory-fsync"):
            raise OSError(errno.EIO, "injected sync failure")
        original_fsync(fd)

    with monkeypatch.context() as fault:
        if failure.endswith("write"):
            fault.setattr(os, "write", failing_write)
        else:
            fault.setattr(os, "fsync", failing_fsync)
        with pytest.raises(OSError, match="injected|short write"):
            _key(path)
    if failure == "directory-fsync":
        assert not pending.exists()
        before = final.read_bytes()
    else:
        assert not final.exists()
        assert pending.stat().st_size == {
            "partial-write": 7, "zero-write": 0, "file-fsync": 32
        }[failure]
        before = None
    result = _key(path)
    _assert_key(final, result)
    if before is not None:
        assert result == before
    assert sorted(entry.name for entry in tmp_path.iterdir()) == ["census.json.key"]


@pytest.mark.parametrize("phase", ["partial-write", "file-synced", "published"])
def test_sigkill_creation_recovers_with_bounded_pending_storage(
    tmp_path: Path, phase: str
) -> None:
    """No Python exception handler runs after SIGKILL; retries must still work."""

    path = tmp_path / "census.json"
    final = tmp_path / "census.json.key"
    pending = tmp_path / ".census.json.key.pending"
    for _ in range(3):
        read_fd, write_fd = os.pipe()
        child = os.fork()
        if child == 0:
            os.close(read_fd)
            original_write = os.write
            original_fsync = os.fsync
            original_publish = cli._publish_audit_cache_key_noreplace

            def pause() -> None:
                original_write(write_fd, b"ready")
                while True:
                    signal.pause()

            def interrupted_write(fd: int, content: bytes) -> int:
                written = original_write(fd, content[:7])
                pause()
                return written

            def interrupted_fsync(fd: int) -> None:
                original_fsync(fd)
                if stat.S_ISREG(os.fstat(fd).st_mode):
                    pause()

            def interrupted_publish(parent: int, candidate: str, name: str) -> None:
                original_publish(parent, candidate, name)
                pause()

            with pytest.MonkeyPatch.context() as patch:
                if phase == "partial-write":
                    patch.setattr(os, "write", interrupted_write)
                elif phase == "file-synced":
                    patch.setattr(os, "fsync", interrupted_fsync)
                else:
                    patch.setattr(cli, "_publish_audit_cache_key_noreplace", interrupted_publish)
                try:
                    _key(path)
                except BaseException:
                    os._exit(11)
            os._exit(12)
        os.close(write_fd)
        try:
            assert select.select([read_fd], [], [], 10)[0], "writer never reached interruption"
            assert os.read(read_fd, 5) == b"ready"
        finally:
            os.close(read_fd)
            with contextlib.suppress(ProcessLookupError):
                os.kill(child, signal.SIGKILL)
            waited, status = os.waitpid(child, 0)
        assert waited == child
        assert os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL
        if phase == "published":
            assert not pending.exists()
            published = final.read_bytes()
            assert _key(path) == published
            break
        assert not final.exists()
        assert pending.stat().st_size == (7 if phase == "partial-write" else 32)
        assert list(tmp_path.iterdir()) == [pending]
    result = _key(path)
    _assert_key(final, result)
    assert sorted(entry.name for entry in tmp_path.iterdir()) == ["census.json.key"]


def test_concurrent_creator_is_bounded_and_both_successes_use_final_authority(
    tmp_path: Path,
) -> None:
    path = tmp_path / "census.json"
    ready_read, ready_write = os.pipe()
    resume_read, resume_write = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(ready_read)
        os.close(resume_write)
        original_publish = cli._publish_audit_cache_key_noreplace

        def paused_publish(parent: int, candidate: str, name: str) -> None:
            os.write(ready_write, b"ready")
            if os.read(resume_read, 1) != b"x":
                os._exit(13)
            original_publish(parent, candidate, name)

        with pytest.MonkeyPatch.context() as patch:
            patch.setattr(cli, "_publish_audit_cache_key_noreplace", paused_publish)
            try:
                result = _key(path)
                os.write(ready_write, result)
            except BaseException:
                os._exit(14)
        os._exit(0)
    os.close(ready_write)
    os.close(resume_read)
    try:
        assert select.select([ready_read], [], [], 10)[0]
        assert os.read(ready_read, 5) == b"ready"
        with pytest.raises(cli.Refusal, match="already in progress"):
            _key(path)
        assert not (tmp_path / "census.json.key").exists()
        os.write(resume_write, b"x")
        assert select.select([ready_read], [], [], 10)[0]
        winner = os.read(ready_read, 32)
        assert len(winner) == 32
    finally:
        os.close(ready_read)
        os.close(resume_write)
        with contextlib.suppress(ProcessLookupError):
            os.kill(child, signal.SIGKILL)
        os.waitpid(child, 0)
    retry = _key(path)
    assert retry == winner
    _assert_key(tmp_path / "census.json.key", winner)


def test_nonreplacing_publication_adopts_a_different_complete_winner(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "census.json"
    final = tmp_path / "census.json.key"
    winner = b"w" * 32
    original_publish = cli._publish_audit_cache_key_noreplace

    def other_winner(parent: int, candidate: str, name: str) -> None:
        final.write_bytes(winner)
        final.chmod(0o600)
        original_publish(parent, candidate, name)

    monkeypatch.setattr(cli, "_publish_audit_cache_key_noreplace", other_winner)
    key = _key(path)
    assert key == winner
    _assert_key(final, winner)
    parent_fd = os.open(tmp_path, os.O_RDONLY | os.O_DIRECTORY)
    payload: dict[str, object] = {"progress": "current"}
    try:
        cli._write_audit_cache_state(path, payload, key, parent_fd)
    finally:
        os.close(parent_fd)
    envelope = json.loads(path.read_bytes())
    mac = envelope.pop("hmac_sha256")
    canonical = json.dumps(envelope, sort_keys=True, separators=(",", ":")).encode()
    assert mac == hmac.new(final.read_bytes(), canonical, hashlib.sha256).hexdigest()


def test_candidate_renamed_before_lock_is_read_as_final_never_truncated(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "census.json"
    original_flock = fcntl.flock
    running_competitor = False
    winner: bytes | None = None

    def competitor_before_lock(fd: int, operation: int) -> None:
        nonlocal running_competitor, winner
        if not running_competitor:
            running_competitor = True
            winner = _key(path)
        original_flock(fd, operation)

    monkeypatch.setattr(fcntl, "flock", competitor_before_lock)
    result = _key(path)
    assert winner is not None
    assert result == winner
    _assert_key(tmp_path / "census.json.key", winner)
    assert sorted(entry.name for entry in tmp_path.iterdir()) == ["census.json.key"]


@pytest.mark.parametrize("kind", ["symlink", "hardlink", "mode", "owner", "oversize"])
def test_hostile_pending_is_refused_without_changing_its_contents(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, kind: str
) -> None:
    path = tmp_path / "census.json"
    pending = tmp_path / ".census.json.key.pending"
    target = tmp_path / "untouched"
    target.write_bytes(b"original content")
    target.chmod(0o600)
    if kind == "symlink":
        pending.symlink_to(target)
    elif kind == "hardlink":
        os.link(target, pending)
    else:
        pending.write_bytes(b"p" * (33 if kind == "oversize" else 7))
        pending.chmod(0o644 if kind == "mode" else 0o600)
    before = pending.read_bytes()
    original_fstat = os.fstat

    def foreign_owner(fd: int) -> os.stat_result:
        values = list(original_fstat(fd))
        values[4] = os.geteuid() + 1
        return os.stat_result(values)

    if kind == "owner":
        monkeypatch.setattr(os, "fstat", foreign_owner)
    with pytest.raises((OSError, cli.Refusal)):
        _key(path)
    assert not (tmp_path / "census.json.key").exists()
    assert pending.read_bytes() == before
    assert target.read_bytes() == b"original content"


def test_replaced_pending_name_never_truncates_the_previously_bound_inode(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "census.json"
    pending = tmp_path / ".census.json.key.pending"
    displaced = tmp_path / "displaced"
    pending.write_bytes(b"partial")
    pending.chmod(0o600)
    original_stat = os.stat

    def swap_before_stat(
        name: str | os.PathLike[str], *, dir_fd: int | None = None,
        follow_symlinks: bool = True,
    ) -> os.stat_result:
        if name == pending.name:
            pending.rename(displaced)
            pending.write_bytes(b"replacement")
            pending.chmod(0o600)
        return original_stat(name, dir_fd=dir_fd, follow_symlinks=follow_symlinks)

    with monkeypatch.context() as patch:
        patch.setattr(os, "stat", swap_before_stat)
        with pytest.raises(cli.Refusal) as refused:
            _key(path)
    assert displaced.read_bytes() == b"partial"
    assert pending.read_bytes() == b"replacement"
    assert not (tmp_path / "census.json.key").exists()
    assert "changed while binding" in str(refused.value)


@pytest.mark.parametrize("contents", [b"", b"p" * 7, b"p" * 33])
def test_malformed_final_key_is_never_regenerated(tmp_path: Path, contents: bytes) -> None:
    path = tmp_path / "census.json"
    final = tmp_path / "census.json.key"
    final.write_bytes(contents)
    final.chmod(0o600)
    with pytest.raises(cli.Refusal, match="exactly 32"):
        _key(path)
    assert final.read_bytes() == contents
    assert list(tmp_path.iterdir()) == [final]
