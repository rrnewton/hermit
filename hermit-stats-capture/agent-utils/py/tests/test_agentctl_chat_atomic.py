"""One locked, bounded staging slot covers every durable Chat write class."""

from __future__ import annotations

import errno
import fcntl
import json
import os
from pathlib import Path
import stat
import threading
import tempfile
from collections.abc import Iterator
from contextlib import AbstractContextManager

import pytest

import agentctl.agent as agent_module
import agentctl.chat as chat_module
from agentctl.agent import AtomicWritePolicy, atomic_write_policy, enqueue, drain
from agentctl.chat import _create_chat_json, _read, _write, submit_reply
from agentctl.errors import AgentDeliveryError
from tests.test_herdr_chat import setup


class _Crash(BaseException):
    pass


_KEY = "a" * 64
_DESTINATIONS = [
    "observer.json", f"requests/{_KEY}.json", f"feedback/{_KEY}.json",
    f"deferred/{_KEY}.json", f"replies/{_KEY}.json",
    f"replies/items/aa/aa/{_KEY}/pending/0000/0.json",
    f"replies/items/aa/aa/{_KEY}/history/0000/0.json",
    f"reply-receipts/aa/{_KEY}.json", f"submissions/{_KEY}.json",
]


def _parents(state: Path, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    current = path.parent
    while current != state.parent:
        current.chmod(0o700)
        if current == state:
            break
        current = current.parent


def _audit(state: Path) -> None:
    descriptor = os.open(state / ".run.lock", os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        chat_module._audit_chat_temporaries(state, descriptor)
    finally:
        os.close(descriptor)


def _hostile_old_destination(
    state: Path, kind: str, monkeypatch: pytest.MonkeyPatch,
) -> tuple[Path, Path]:
    destination = state / "submissions" / "existing.json"
    witness = state / "untouched.txt"
    witness.write_bytes(b"unrelated existing bytes")
    witness.chmod(0o600)
    if kind == "symlink":
        destination.symlink_to(witness)
    elif kind == "fifo":
        os.mkfifo(destination, 0o600)
    elif kind == "directory":
        destination.mkdir(mode=0o700)
    elif kind == "hardlink":
        os.link(witness, destination)
    else:
        destination.write_bytes(b"unrelated destination bytes")
        destination.chmod(0o644 if kind == "public-file" else 0o600)
    if kind == "device-metadata":
        # Exercise device classification without requiring mknod capability or
        # touching a real device. The underlying unrelated inode stays intact.
        original_stat = os.stat

        def device_stat(path: str | os.PathLike[str], *, dir_fd: int | None = None,
                        follow_symlinks: bool = True) -> os.stat_result:
            metadata = original_stat(path, dir_fd=dir_fd, follow_symlinks=follow_symlinks)
            if path == destination.name and dir_fd is not None:
                fields = list(metadata)
                fields[0] = stat.S_IFCHR | 0o600
                return os.stat_result(fields)
            return metadata

        monkeypatch.setattr(os, "stat", device_stat)
    return destination, witness


@pytest.mark.parametrize("kind", ["symlink", "fifo", "directory", "public-file", "hardlink", "device-metadata"])
def test_eexist_discards_only_owned_evidence_without_inspecting_old_destination(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, kind: str,
) -> None:
    setup(tmp_path)
    destination, witness = _hostile_old_destination(tmp_path, kind, monkeypatch)
    before, witness_before = destination.lstat(), witness.read_bytes()
    original_open, original_stat = os.open, os.stat

    def open_file(path: str | os.PathLike[str], flags: int, mode: int = 0o777, *, dir_fd: int | None = None) -> int:
        if path in (destination, str(destination), destination.name):
            raise AssertionError("EEXIST cleanup opened the unrelated destination")
        return original_open(path, flags, mode, dir_fd=dir_fd)

    def stat_file(path: str | os.PathLike[str], *, dir_fd: int | None = None,
                  follow_symlinks: bool = True) -> os.stat_result:
        if path in (destination, str(destination), destination.name):
            raise AssertionError("EEXIST cleanup inspected the unrelated destination")
        return original_stat(path, dir_fd=dir_fd, follow_symlinks=follow_symlinks)

    with monkeypatch.context() as guarded:
        guarded.setattr(os, "open", open_file)
        guarded.setattr(os, "stat", stat_file)
        with pytest.raises(FileExistsError) as failure:
            _create_chat_json(tmp_path, destination, {"text": "rejected body"})
    assert failure.value.errno == errno.EEXIST
    assert destination.lstat() == before and witness.read_bytes() == witness_before
    assert not any((tmp_path / ".atomic" / name).exists() for name in ("staged.json", "publish.json", "intent.json"))


@pytest.mark.parametrize("kind", ["symlink", "fifo", "directory", "public-file", "hardlink", "device-metadata"])
@pytest.mark.parametrize("linked", [False, True])
def test_unpublished_replace_recovery_leaves_hostile_old_destination_untouched(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, kind: str, linked: bool,
) -> None:
    setup(tmp_path)
    destination, witness = _hostile_old_destination(tmp_path, kind, monkeypatch)
    before, witness_before = destination.lstat(), witness.read_bytes()
    original_link = os.link

    def link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
        assert name == "publish.json"
        if linked:
            original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                          follow_symlinks=follow_symlinks)
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(_Crash):
            _write(destination, {"text": "never published"})
    with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
        pass
    assert destination.lstat() == before and witness.read_bytes() == witness_before
    assert not any((tmp_path / ".atomic" / name).exists() for name in ("staged.json", "publish.json", "intent.json"))


@pytest.mark.parametrize("operation", ["eexist", "replace"])
def test_unpublished_intent_only_cleanup_refuses_after_staging_fsync_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, operation: str,
) -> None:
    setup(tmp_path)
    destination, witness = _hostile_old_destination(tmp_path, "symlink", monkeypatch)
    before = destination.lstat()
    policy = chat_module._chat_atomic_policy(tmp_path)
    directory = (tmp_path / ".atomic").stat()
    original_fsync = os.fsync

    def fail_cleanup(descriptor: int) -> None:
        metadata = os.fstat(descriptor)
        if ((metadata.st_dev, metadata.st_ino) == (directory.st_dev, directory.st_ino)
                and (tmp_path / ".atomic" / "intent.json").exists()
                and not (tmp_path / ".atomic" / "staged.json").exists()):
            raise OSError(errno.EIO, "unpublished cleanup fsync failed")
        original_fsync(descriptor)

    if operation == "replace":
        def crash(*args: object, **kwargs: object) -> None:
            raise _Crash()

        with monkeypatch.context() as failing:
            failing.setattr(os, "replace", crash)
            with pytest.raises(_Crash):
                _write(destination, {"text": "never published"})
    with monkeypatch.context() as failing:
        failing.setattr(os, "fsync", fail_cleanup)
        with pytest.raises(OSError, match="unpublished cleanup fsync failed"):
            if operation == "eexist":
                _create_chat_json(tmp_path, destination, {"text": "rejected"})
            else:
                with agent_module.atomic_write_recovery(policy):
                    pass
    assert (tmp_path / ".atomic" / "intent.json").exists()
    assert not (tmp_path / ".atomic" / "staged.json").exists()
    phase = (tmp_path / ".atomic" / "intent.json").read_bytes()
    assert phase.endswith(agent_module._ATOMIC_CLEANUP_PHASE)
    with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
        with agent_module.atomic_write_recovery(policy):
            pass
    assert destination.lstat() == before and witness.read_bytes() == b"unrelated existing bytes"
    assert (tmp_path / ".atomic" / "intent.json").read_bytes() == phase


@pytest.mark.parametrize("failure", ["write-error", "crash"])
@pytest.mark.parametrize("cleanup_fault", [False, True])
def test_partial_unpublished_intent_recovers_with_cleanup_fsync_retry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failure: str, cleanup_fault: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / "never-published.json"
    original_write = os.write
    intent_fd = -1
    original_open = os.open

    def open_file(path: str | os.PathLike[str], flags: int, mode: int = 0o777, *, dir_fd: int | None = None) -> int:
        nonlocal intent_fd
        descriptor = original_open(path, flags, mode, dir_fd=dir_fd)
        if path == "intent.json" and flags & os.O_CREAT:
            intent_fd = descriptor
        return descriptor

    def write(descriptor: int, data: bytes) -> int:
        if descriptor == intent_fd:
            original_write(descriptor, data[:5])
            if failure == "crash":
                raise _Crash()
            raise OSError(errno.EIO, "partial intent write")
        return original_write(descriptor, data)

    with monkeypatch.context() as failing:
        failing.setattr(os, "open", open_file)
        failing.setattr(os, "write", write)
        with pytest.raises(_Crash if failure == "crash" else OSError):
            _create_chat_json(tmp_path, destination, {"text": "never published"})
    slot = tmp_path / ".atomic" / "staged.json"
    intent = tmp_path / ".atomic" / "intent.json"
    assert not destination.exists() and slot.stat().st_nlink == 1 and intent.stat().st_size == 5
    policy = chat_module._chat_atomic_policy(tmp_path)
    if cleanup_fault:
        original_fsync = os.fsync
        directory = (tmp_path / ".atomic").stat()

        def fsync(descriptor: int) -> None:
            metadata = os.fstat(descriptor)
            if (metadata.st_dev, metadata.st_ino) == (directory.st_dev, directory.st_ino):
                assert slot.exists() and intent.exists()
                raise OSError(errno.EIO, "partial intent cleanup fsync")
            original_fsync(descriptor)

        with monkeypatch.context() as failing:
            failing.setattr(os, "fsync", fsync)
            with pytest.raises(OSError, match="partial intent cleanup fsync"):
                with agent_module.atomic_write_recovery(policy):
                    pass
        assert slot.exists() and intent.read_bytes() == b'{\n  "'
    with agent_module.atomic_write_recovery(policy):
        pass
    assert not destination.exists() and not slot.exists() and not intent.exists()


@pytest.mark.parametrize("relative", [".atomic", ".atomic.lock", ".atomic/staged.json", ".atomic/publish.json",
                                      ".atomic/intent.json", ".atomic/future-internal.json", ".atomic/../.atomic.lock"])
def test_atomic_destination_cannot_replace_its_own_domain_authority(
    tmp_path: Path, relative: str,
) -> None:
    setup(tmp_path)
    directory_before = (tmp_path / ".atomic").stat()
    lock_before = (tmp_path / ".atomic.lock").stat()
    with atomic_write_policy(chat_module._chat_atomic_policy(tmp_path)):
        with pytest.raises(AgentDeliveryError, match="overlaps staging or lock authority"):
            agent_module._atomic_json(str(tmp_path / relative), {"overwrite": "forbidden"})
    assert (tmp_path / ".atomic").stat().st_ino == directory_before.st_ino
    assert (tmp_path / ".atomic.lock").stat().st_ino == lock_before.st_ino
    assert list((tmp_path / ".atomic").iterdir()) == []


def test_atomic_domain_alias_is_refused_but_explicit_audit_marker_is_allowed(tmp_path: Path) -> None:
    setup(tmp_path)
    alias = tmp_path / "alias"
    alias.symlink_to(tmp_path / ".atomic", target_is_directory=True)
    with atomic_write_policy(chat_module._chat_atomic_policy(tmp_path)):
        with pytest.raises(OSError):
            agent_module._atomic_json(str(alias / "new-internal.json"), {"escape": True})
        assert list((tmp_path / ".atomic").iterdir()) == []
        agent_module._atomic_json(str(tmp_path / ".atomic" / "audited-v1.json"), {"version": 1})
    assert _read(tmp_path / ".atomic" / "audited-v1.json") == {"version": 1}
    assert [path.name for path in (tmp_path / ".atomic").iterdir()] == ["audited-v1.json"]


def test_rejected_atomic_lock_destination_cannot_split_a_live_flock(tmp_path: Path) -> None:
    setup(tmp_path)
    lock = tmp_path / ".atomic.lock"
    descriptor = agent_module._open_private_lock(str(lock), "held atomic lock")
    fcntl.flock(descriptor, fcntl.LOCK_EX)
    before = os.fstat(descriptor)
    completed = threading.Event()
    failures: list[BaseException] = []

    def attempt() -> None:
        try:
            with atomic_write_policy(chat_module._chat_atomic_policy(tmp_path)):
                agent_module._atomic_json(str(lock), {"must": "not split the lock"})
        except BaseException as exc:
            failures.append(exc)
        finally:
            completed.set()

    writer = threading.Thread(target=attempt)
    writer.start()
    try:
        assert completed.wait(5), "reserved destination must be rejected before taking its lock"
        assert len(failures) == 1 and isinstance(failures[0], AgentDeliveryError)
        assert lock.stat().st_ino == before.st_ino
        probe = agent_module._open_private_lock(str(lock), "atomic lock probe")
        try:
            with pytest.raises(BlockingIOError):
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            os.close(probe)
    finally:
        os.close(descriptor)
        writer.join(timeout=5)
    assert not writer.is_alive() and list((tmp_path / ".atomic").iterdir()) == []


@pytest.mark.parametrize("operation", ["drain", "send"])
@pytest.mark.parametrize("violation", ["hardlink", "oversize"])
def test_policy_only_queue_target_uses_strict_bounded_reader(
    tmp_path: Path, operation: str, violation: str,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    policy = AtomicWritePolicy(str(tmp_path / ".atomic"), 1024)
    drain(harness, bridge.config.target, str(root), ready_timeout=0, atomic_policy=policy)
    target = root / "target.json"
    if violation == "hardlink":
        os.link(target, tmp_path / "unrelated-binding.json")
        expected = "must not be hard-linked"
    else:
        agent_module._atomic_json(str(target), {"oversized": "x" * 1024})
        expected = "max_artifact_bytes=1024"
    with pytest.raises(AgentDeliveryError, match=expected):
        if operation == "send":
            agent_module.send(harness, bridge.config.target, str(root), "do not send",
                              ready_timeout=0, atomic_policy=policy)
        else:
            drain(harness, bridge.config.target, str(root), ready_timeout=0, atomic_policy=policy)
    assert not harness.prompts and list((root / "inbox").iterdir()) == []


@pytest.mark.parametrize("violation", ["hardlink", "oversize"])
def test_policy_only_queue_message_uses_strict_bounded_reader(tmp_path: Path, violation: str) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    policy = AtomicWritePolicy(str(tmp_path / ".atomic"), 1024)
    enqueue(str(root), "must not deliver unsafe artifact", message_id="unsafe", atomic_policy=policy)
    path = root / "inbox" / "unsafe.json"
    if violation == "hardlink":
        os.link(path, tmp_path / "unrelated-message.json")
        result = drain(harness, bridge.config.target, str(root), ready_timeout=0, atomic_policy=policy)
        assert result.quarantined == ("unsafe",) and result.delivered == ()
        assert (root / "failed" / "unsafe.json.error").is_file()
    else:
        document = _read(path)
        document["text"] = "x" * 1024
        agent_module._atomic_json(str(path), document)
        with pytest.raises(AgentDeliveryError, match="max_artifact_bytes=1024"):
            drain(harness, bridge.config.target, str(root), ready_timeout=0, atomic_policy=policy)
        assert path.exists() and not (root / "inflight" / path.name).exists()
    assert harness.prompts == []


@pytest.mark.parametrize("operation", ["enqueue", "send"])
def test_policy_only_enqueue_refuses_oversized_body_without_partial_artifact(
    tmp_path: Path, operation: str,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    policy = AtomicWritePolicy(str(tmp_path / ".atomic"), 1024)
    with pytest.raises(AgentDeliveryError, match="max_artifact_bytes=1024"):
        if operation == "enqueue":
            enqueue(str(root), "x" * 1024, message_id="too-large", atomic_policy=policy)
        else:
            agent_module.send(harness, bridge.config.target, str(root), "x" * 1024, message_id="too-large",
                              ready_timeout=0, atomic_policy=policy)
    assert not (root / "inbox" / "too-large.json").exists()
    assert not (tmp_path / ".atomic" / "staged.json").exists()
    assert not (tmp_path / ".atomic" / "intent.json").exists()
    assert not harness.prompts


@pytest.mark.parametrize("relative", _DESTINATIONS)
@pytest.mark.parametrize("create", [False, True])
def test_crash_in_every_chat_destination_leaves_only_one_recoverable_slot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, relative: str, create: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / relative
    _parents(tmp_path, destination)
    assert chat_module._chat_write_state(destination) == tmp_path
    operation = "link" if create else "replace"

    def crash(*args: object, **kwargs: object) -> None:
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, operation, crash)
        with pytest.raises(_Crash):
            if create:
                _create_chat_json(tmp_path, destination, {"text": "crash-left body"})
            else:
                _write(destination, {"text": "crash-left body"})
    slot = tmp_path / ".atomic" / "staged.json"
    assert slot.is_file() and stat.S_IMODE(slot.stat().st_mode) == 0o600
    assert slot.stat().st_size <= chat_module._MAX_ATOMIC_STAGE_BYTES
    assert not destination.exists()
    assert not list(destination.parent.glob(".message.*"))
    _write(tmp_path / "recovered.json", {"recovered": True})
    assert not slot.exists() and _read(tmp_path / "recovered.json") == {"recovered": True}


def test_create_crash_after_link_preserves_final_artifact_on_slot_recovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    path = tmp_path / "submissions" / f"{_KEY}.json"
    original = os.link

    def link(source: str, destination: str, *, src_dir_fd: int, dst_dir_fd: int,
             follow_symlinks: bool) -> None:
        original(source, destination, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                 follow_symlinks=follow_symlinks)
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(_Crash):
            _create_chat_json(tmp_path, path, {"text": "committed"})
    assert path.stat().st_nlink == 2
    _write(tmp_path / "observer.json", {"state": "ready"})
    assert _read(path) == {"text": "committed"} and path.stat().st_nlink == 1
    assert not (tmp_path / ".atomic" / "staged.json").exists()


@pytest.mark.parametrize("change_cwd", [False, True])
def test_relative_atomic_policy_is_pinned_once_at_context_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, change_cwd: bool,
) -> None:
    monkeypatch.chdir(tmp_path)
    alternate = tmp_path / "alternate"
    alternate.mkdir()
    with atomic_write_policy(AtomicWritePolicy(".atomic", 512)):
        if change_cwd:
            monkeypatch.chdir(alternate)
        agent_module._atomic_json(str(tmp_path / "saved.json"), {"saved": True})
    assert _read(tmp_path / "saved.json") == {"saved": True}
    assert (tmp_path / ".atomic").is_dir()
    assert (tmp_path / ".atomic.lock").is_file()
    assert not (tmp_path / ".atomic" / "staged.json").exists()
    assert not (alternate / ".atomic").exists()
    assert not (alternate / ".atomic.lock").exists()


def test_create_existing_destination_removes_and_fsyncs_rejected_slot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    _create_chat_json(tmp_path, destination, {"text": "original"})
    before = destination.stat()
    staging_directory = tmp_path / ".atomic"
    staging_identity = staging_directory.stat()
    slot = staging_directory / "staged.json"
    synced_slot_presence: list[bool] = []
    original_fsync = os.fsync

    def fsync(descriptor: int) -> None:
        metadata = os.fstat(descriptor)
        if (metadata.st_dev, metadata.st_ino) == (staging_identity.st_dev, staging_identity.st_ino):
            synced_slot_presence.append(slot.exists())
        original_fsync(descriptor)

    monkeypatch.setattr(os, "fsync", fsync)
    with pytest.raises(FileExistsError):
        _create_chat_json(tmp_path, destination, {"text": "rejected sensitive body"})
    # Intent is durably installed first; rejection cleanup must subsequently
    # fsync the directory with both the slot and intent removed.
    # Publication intent and cleanup phase each precede any payload removal.
    assert synced_slot_presence == [True, True, False, False, False]
    assert not slot.exists() and not (staging_directory / "intent.json").exists()
    assert destination.stat().st_ino == before.st_ino
    assert _read(destination) == {"text": "original"}


@pytest.mark.parametrize("boundary", ["post-link", "destination-fsync"])
def test_ambiguous_create_failure_retains_slot_and_final_link_for_recovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, boundary: str,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    parent_identity = destination.parent.stat()
    original_link, original_fsync = os.link, os.fsync

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int,
             follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        raise OSError(errno.EIO, "ambiguous post-link failure")

    def fsync(descriptor: int) -> None:
        metadata = os.fstat(descriptor)
        if (metadata.st_dev, metadata.st_ino) == (parent_identity.st_dev, parent_identity.st_ino):
            raise OSError(errno.EIO, "destination fsync failure")
        original_fsync(descriptor)

    with monkeypatch.context() as failing:
        failing.setattr(os, "link" if boundary == "post-link" else "fsync",
                        link if boundary == "post-link" else fsync)
        with pytest.raises(OSError, match="failure"):
            _create_chat_json(tmp_path, destination, {"text": "committed"})
    slot = tmp_path / ".atomic" / "staged.json"
    assert slot.stat().st_ino == destination.stat().st_ino
    assert destination.stat().st_nlink == 2
    _write(tmp_path / "observer.json", {"state": "recovered"})
    assert not slot.exists()
    assert destination.stat().st_nlink == 1
    assert _read(destination) == {"text": "committed"}


@pytest.mark.parametrize("recovery", ["retry", "adopt", "startup"])
def test_submission_link_crash_is_recovered_before_strict_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, recovery: str,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    _audit(tmp_path)
    request_path = next((tmp_path / "requests").glob("*.json"))
    record = _read(request_path)
    key = str(record["key"])
    submission = tmp_path / "submissions" / f"{key}.json"
    original_link = os.link

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int,
             follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(_Crash):
            submit_reply(tmp_path, key, "durable final answer")
    slot = tmp_path / ".atomic" / "staged.json"
    assert submission.stat().st_nlink == 2
    assert submission.stat().st_ino == slot.stat().st_ino
    observed_links: list[int] = []
    original_read = chat_module._read

    def read(path: Path) -> dict[str, object]:
        if path == submission:
            observed_links.append(path.stat().st_nlink)
            assert not slot.exists()
        return original_read(path)

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("fixed-slot recovery must not scan directories")

    monkeypatch.setattr(chat_module, "_read", read)
    monkeypatch.setattr(os, "scandir", forbidden)
    if recovery == "retry":
        submit_reply(tmp_path, key, "durable final answer")
    elif recovery == "adopt":
        items: list[dict[str, object]] = []
        assert bridge._adopt_submission(request_path, record, items)
        assert [item["text"] for item in items] == ["durable final answer"]
        assert not submission.exists()
    else:
        _audit(tmp_path)
        assert read(submission)["text"] == "durable final answer"
    assert observed_links == [1]
    assert not slot.exists()
    if recovery != "adopt":
        assert submission.stat().st_nlink == 1


@pytest.mark.parametrize("operation", ["retry", "adopt"])
def test_submission_recovery_does_not_accept_unrelated_hardlink(
    tmp_path: Path, operation: str,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    request_path = next((tmp_path / "requests").glob("*.json"))
    record = _read(request_path)
    key = str(record["key"])
    submit_reply(tmp_path, key, "answer")
    submission = tmp_path / "submissions" / f"{key}.json"
    unrelated = tmp_path / "unrelated-hardlink.json"
    os.link(submission, unrelated)
    with pytest.raises(AgentDeliveryError, match="must not be hard-linked"):
        if operation == "retry":
            submit_reply(tmp_path, key, "answer")
        else:
            bridge._adopt_submission(request_path, record, [])
    assert submission.stat().st_nlink == unrelated.stat().st_nlink == 2
    assert not (tmp_path / ".atomic" / "staged.json").exists()


@pytest.mark.parametrize("relative", [
    f"submissions/{_KEY}.json", f"reply-receipts/aa/{_KEY}.json",
    f"replies/items/aa/aa/{_KEY}/pending/0000/0.json", "queue/inbox/prompt.json",
])
def test_bounded_create_artifact_read_recovers_only_fixed_slot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, relative: str,
) -> None:
    setup(tmp_path)
    destination = tmp_path / relative
    _parents(tmp_path, destination)
    original_link = os.link

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int,
             follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(_Crash):
            _create_chat_json(tmp_path, destination, {"text": "committed"})
    assert destination.stat().st_nlink == 2
    document, _ = chat_module._read_bounded_json(destination, 1024, "recovery test artifact")
    assert document == {"text": "committed"}
    assert destination.stat().st_nlink == 1
    assert not (tmp_path / ".atomic" / "staged.json").exists()


def test_queue_link_then_error_recovers_before_bounded_drain_and_delivers_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = str(tmp_path / "queue")
    policy = chat_module._chat_atomic_policy(tmp_path)
    # Pre-bind so no unrelated metadata write happens to recover the slot.
    assert drain(harness, bridge.config.target, root, ready_timeout=0,
                 max_artifact_bytes=512 << 10, atomic_policy=policy).delivered == ()
    original_link = os.link

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int,
             follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        raise OSError(errno.EIO, "post-link failure")

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(OSError, match="post-link failure"):
            enqueue(root, "deliver exactly once", message_id="recoverable",
                    max_artifact_bytes=512 << 10, atomic_policy=policy)
    path = tmp_path / "queue" / "inbox" / "recoverable.json"
    assert path.stat().st_nlink == 2
    result = drain(harness, bridge.config.target, root, ready_timeout=0,
                   max_artifact_bytes=512 << 10, atomic_policy=policy)
    assert result.delivered == ("recoverable",) and result.quarantined == ()
    processed = tmp_path / "queue" / "processed" / path.name
    assert processed.stat().st_nlink == 1
    assert not list((tmp_path / "queue" / "failed").iterdir())
    assert not (tmp_path / ".atomic" / "staged.json").exists()
    assert drain(harness, bridge.config.target, root, ready_timeout=0,
                 max_artifact_bytes=512 << 10, atomic_policy=policy).delivered == ()
    assert harness.prompts == ["deliver exactly once"]


def test_queue_destination_fsync_must_succeed_before_any_delivery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = str(tmp_path / "queue")
    policy = chat_module._chat_atomic_policy(tmp_path)
    drain(harness, bridge.config.target, root, ready_timeout=0,
          max_artifact_bytes=512 << 10, atomic_policy=policy)
    inbox = (tmp_path / "queue" / "inbox").stat()
    original_fsync, original_prompt = os.fsync, harness.prompt_agent
    refusing = True
    events: list[str] = []

    def fsync(descriptor: int) -> None:
        info = os.fstat(descriptor)
        if (info.st_dev, info.st_ino) == (inbox.st_dev, inbox.st_ino):
            if refusing:
                raise OSError(errno.EIO, "queue destination fsync failed")
            events.append("durable")
        original_fsync(descriptor)

    def prompt(pane_id: str, text: str) -> None:
        events.append("delivery")
        original_prompt(pane_id, text)

    monkeypatch.setattr(os, "fsync", fsync)
    monkeypatch.setattr(harness, "prompt_agent", prompt)
    with pytest.raises(OSError, match="queue destination fsync failed"):
        enqueue(root, "durable before delivery", message_id="recoverable",
                max_artifact_bytes=512 << 10, atomic_policy=policy)
    with pytest.raises(OSError, match="queue destination fsync failed"):
        drain(harness, bridge.config.target, root, ready_timeout=0,
              max_artifact_bytes=512 << 10, atomic_policy=policy)
    assert harness.prompts == [] and events == []
    assert not list((tmp_path / "queue" / "failed").iterdir())
    assert (tmp_path / ".atomic" / "intent.json").exists()
    refusing = False
    result = drain(harness, bridge.config.target, root, ready_timeout=0,
                   max_artifact_bytes=512 << 10, atomic_policy=policy)
    assert result.delivered == ("recoverable",) and result.quarantined == ()
    assert events.index("durable") < events.index("delivery")
    assert harness.prompts == ["durable before delivery"]
    assert not (tmp_path / ".atomic" / "intent.json").exists()


@pytest.mark.parametrize("create", [False, True])
@pytest.mark.parametrize("failure", ["destination-fsync", "post-publish-crash"])
def test_initial_queue_binding_recovers_before_its_first_bounded_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, create: bool, failure: str,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    agent_module._prepare(str(root))
    binding = root / "target.json"
    assert not binding.exists()
    root_info = root.stat()
    policy = chat_module._chat_atomic_policy(tmp_path)
    original_fsync, original_link, original_replace = os.fsync, os.link, os.replace

    def fsync(descriptor: int) -> None:
        info = os.fstat(descriptor)
        if (failure == "destination-fsync" and binding.exists() and binding.stat().st_nlink == 2
                and (info.st_dev, info.st_ino) == (root_info.st_dev, root_info.st_ino)):
            raise OSError(errno.EIO, "binding destination fsync failed")
        original_fsync(descriptor)

    def link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
        original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        if failure == "post-publish-crash" and name == "target.json":
            raise _Crash()

    def replace(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int) -> None:
        original_replace(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd)
        if failure == "post-publish-crash" and name == "target.json":
            raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "fsync", fsync)
        failing.setattr(os, "link", link)
        failing.setattr(os, "replace", replace)
        with pytest.raises(OSError if failure == "destination-fsync" else _Crash):
            if create:
                with atomic_write_policy(policy):
                    agent_module._atomic_json_create(
                        str(binding), agent_module._binding(bridge.config.target), max_artifact_bytes=512 << 10)
            else:
                drain(harness, bridge.config.target, str(root), ready_timeout=0,
                      max_artifact_bytes=512 << 10, atomic_policy=policy)
    slot = tmp_path / ".atomic" / "staged.json"
    assert binding.stat().st_nlink == 2 and binding.stat().st_ino == slot.stat().st_ino
    assert (tmp_path / ".atomic" / "intent.json").exists()

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("binding recovery must not scan staging directories")

    monkeypatch.setattr(os, "scandir", forbidden)
    result = drain(harness, bridge.config.target, str(root), ready_timeout=0,
                   max_artifact_bytes=512 << 10, atomic_policy=policy)
    assert result.delivered == () and result.quarantined == ()
    assert binding.stat().st_nlink == 1
    assert not slot.exists() and not (tmp_path / ".atomic" / "intent.json").exists()
    assert harness.prompts == []


@pytest.mark.parametrize("writer_kind", ["binding", "atomic"])
def test_binding_reader_waits_for_live_publication_before_bounded_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, writer_kind: str,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    agent_module._prepare(str(root))
    binding = root / "target.json"
    policy = chat_module._chat_atomic_policy(tmp_path)
    lock_path = root / ".binding.lock" if writer_kind == "binding" else tmp_path / ".atomic.lock"
    descriptor = agent_module._open_private_lock(str(lock_path), "test binding lock")
    identity = os.fstat(descriptor)
    os.close(descriptor)
    published, release, attempted, reader_done = (threading.Event() for _ in range(4))
    failures: list[BaseException] = []
    original_replace, original_flock = os.replace, fcntl.flock

    def replace(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int) -> None:
        original_replace(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd)
        if name == "target.json":
            published.set()
            assert release.wait(5)

    def flock(fd: int, operation: int) -> None:
        info = os.fstat(fd)
        if (threading.current_thread().name == "binding-reader"
                and (info.st_dev, info.st_ino) == (identity.st_dev, identity.st_ino)):
            attempted.set()
        original_flock(fd, operation)

    def run(*, writer: bool) -> None:
        try:
            if writer and writer_kind == "atomic":
                # Also exercise waiting on atomic after acquiring binding,
                # not merely waiting for another _bind_queue caller's lock.
                with atomic_write_policy(policy):
                    agent_module._atomic_json(
                        str(binding), agent_module._binding(bridge.config.target), max_artifact_bytes=512 << 10)
            else:
                result = drain(harness, bridge.config.target, str(root), ready_timeout=0,
                               max_artifact_bytes=512 << 10, atomic_policy=policy)
                assert result.delivered == () and result.quarantined == ()
        except BaseException as exc:
            failures.append(exc)
        finally:
            if not writer:
                reader_done.set()

    monkeypatch.setattr(os, "replace", replace)
    monkeypatch.setattr(fcntl, "flock", flock)
    writer = threading.Thread(target=lambda: run(writer=True), name="binding-writer")
    reader = threading.Thread(target=lambda: run(writer=False), name="binding-reader")
    writer.start()
    try:
        assert published.wait(5)
        assert binding.stat().st_nlink == 2
        reader.start()
        assert attempted.wait(5)
        assert not reader_done.is_set()
        assert (tmp_path / ".atomic" / "staged.json").stat().st_ino == binding.stat().st_ino
    finally:
        release.set()
        writer.join(timeout=5)
        if reader.ident is not None:
            reader.join(timeout=5)
    assert not writer.is_alive() and not reader.is_alive() and not failures
    assert binding.stat().st_nlink == 1 and reader_done.is_set()
    assert not (tmp_path / ".atomic" / "staged.json").exists()


def test_binding_recovery_preserves_refusal_of_unrelated_hardlinks(tmp_path: Path) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    policy = chat_module._chat_atomic_policy(tmp_path)
    drain(harness, bridge.config.target, str(root), ready_timeout=0,
          max_artifact_bytes=512 << 10, atomic_policy=policy)
    binding = root / "target.json"
    unrelated = tmp_path / "unrelated-binding.json"
    os.link(binding, unrelated)
    with pytest.raises(AgentDeliveryError, match="must not be hard-linked"):
        drain(harness, bridge.config.target, str(root), ready_timeout=0,
              max_artifact_bytes=512 << 10, atomic_policy=policy)
    assert binding.stat().st_nlink == unrelated.stat().st_nlink == 2
    assert harness.prompts == []


def test_send_uses_one_policy_for_binding_enqueue_and_delivery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    monkeypatch.chdir(tmp_path)
    destinations: list[str] = []
    original_writer = agent_module._atomic_json_staged

    def staged(path: str, document: dict[str, object], policy: AtomicWritePolicy, *,
               create: bool, max_artifact_bytes: int | None) -> None:
        assert policy.directory == str(tmp_path / ".atomic")
        destinations.append(str(Path(path).relative_to(tmp_path / "queue")))
        original_writer(path, document, policy, create=create, max_artifact_bytes=max_artifact_bytes)

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("send escaped its fixed-slot policy")

    monkeypatch.setattr(agent_module, "_atomic_json_staged", staged)
    monkeypatch.setattr(tempfile, "NamedTemporaryFile", forbidden)
    result = agent_module.send(
        harness, bridge.config.target, str(tmp_path / "queue"), "fixed-domain send",
        message_id="sent", max_artifact_bytes=512 << 10, ready_timeout=0,
        atomic_policy=AtomicWritePolicy(".atomic", 512 << 10))
    assert result.delivered == ("sent",) and harness.prompts == ["fixed-domain send"]
    assert "target.json" in destinations and "inbox/sent.json" in destinations
    assert "inflight/sent.json" in destinations
    assert agent_module._ACTIVE_ATOMIC_POLICY.get() is None
    assert not (tmp_path / ".atomic" / "staged.json").exists()


@pytest.mark.parametrize("phase", ["binding", "enqueue"])
def test_send_fixed_policy_recovers_binding_or_enqueue_publication_crash(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, phase: str,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    root = tmp_path / "queue"
    policy = chat_module._chat_atomic_policy(tmp_path)
    original_link, original_replace = os.link, os.replace

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("send created a legacy random temporary")

    def link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
        original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        if phase == "enqueue" and name == "crash.json":
            raise _Crash()

    def replace(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int) -> None:
        original_replace(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd)
        if phase == "binding" and name == "target.json":
            raise _Crash()

    monkeypatch.setattr(tempfile, "NamedTemporaryFile", forbidden)
    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        failing.setattr(os, "replace", replace)
        with pytest.raises(_Crash):
            agent_module.send(harness, bridge.config.target, str(root), "recovered send", message_id="crash",
                              max_artifact_bytes=512 << 10, ready_timeout=0, atomic_policy=policy)
    final = root / "target.json" if phase == "binding" else root / "inbox" / "crash.json"
    assert final.stat().st_nlink == 2 and not harness.prompts
    assert (tmp_path / ".atomic" / "intent.json").exists()
    if phase == "binding":
        result = agent_module.send(harness, bridge.config.target, str(root), "recovered send", message_id="crash",
                                   max_artifact_bytes=512 << 10, ready_timeout=0, atomic_policy=policy)
    else:
        result = drain(harness, bridge.config.target, str(root), max_artifact_bytes=512 << 10,
                       ready_timeout=0, atomic_policy=policy)
    assert result.delivered == ("crash",) and result.quarantined == ()
    assert harness.prompts == ["recovered send"]
    assert not (tmp_path / ".atomic" / "intent.json").exists()


def test_concurrent_bounded_reader_waits_for_live_post_link_writer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    key = str(record["key"])
    submission = tmp_path / "submissions" / f"{key}.json"
    slot = tmp_path / ".atomic" / "staged.json"
    identity = (tmp_path / ".atomic.lock").stat()
    linked, release, attempted, read_done = (threading.Event() for _ in range(4))
    failures: list[BaseException] = []
    documents: list[dict[str, object]] = []
    original_link, original_flock = os.link, fcntl.flock

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int,
             follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        linked.set()
        assert release.wait(5)

    def flock(descriptor: int, operation: int) -> None:
        metadata = os.fstat(descriptor)
        if (threading.current_thread().name == "bounded-reader"
                and (metadata.st_dev, metadata.st_ino) == (identity.st_dev, identity.st_ino)):
            attempted.set()
        original_flock(descriptor, operation)

    def writer() -> None:
        try:
            submit_reply(tmp_path, key, "concurrent answer")
        except BaseException as exc:
            failures.append(exc)

    def reader() -> None:
        try:
            documents.append(_read(submission))
        except BaseException as exc:
            failures.append(exc)
        finally:
            read_done.set()

    monkeypatch.setattr(os, "link", link)
    monkeypatch.setattr(fcntl, "flock", flock)
    writer_thread = threading.Thread(target=writer, name="submission-writer")
    reader_thread = threading.Thread(target=reader, name="bounded-reader")
    writer_thread.start()
    try:
        assert linked.wait(5)
        assert submission.stat().st_nlink == 2
        reader_thread.start()
        assert attempted.wait(5)
        assert not read_done.is_set()
        assert slot.stat().st_ino == submission.stat().st_ino
        assert slot.stat().st_nlink == 2  # recovery cannot touch an active writer's slot
    finally:
        release.set()
        writer_thread.join(timeout=5)
        if reader_thread.ident is not None:
            reader_thread.join(timeout=5)
    assert not writer_thread.is_alive() and not reader_thread.is_alive()
    assert not failures and documents == [{"request": key, "text": "concurrent answer"}]
    assert submission.stat().st_nlink == 1 and not slot.exists()


@pytest.mark.parametrize(("relative", "create"), [
    (f"requests/{_KEY}.json", False), (f"deferred/{_KEY}.json", False),
    (f"replies/items/aa/aa/{_KEY}/pending/0000/0.json", True),
    (f"submissions/{_KEY}.json", True), (f"reply-receipts/aa/{_KEY}.json", True),
    ("queue/inbox/prompt.json", True),
])
def test_destination_fsync_is_retried_before_adoption_or_checkpoint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, relative: str, create: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / relative
    _parents(tmp_path, destination)
    checkpoint = tmp_path / "input.json"
    _write(checkpoint, {"cursor": "before"})
    destination_directory = destination.parent.stat()
    root_directory = tmp_path.stat()
    original_fsync = os.fsync
    refusing = True
    events: list[str] = []

    def fsync(descriptor: int) -> None:
        metadata = os.fstat(descriptor)
        identity = (metadata.st_dev, metadata.st_ino)
        if identity == (destination_directory.st_dev, destination_directory.st_ino):
            events.append("destination-failed" if refusing else "destination-succeeded")
            if refusing:
                raise OSError(errno.EIO, "destination durability unavailable")
        elif identity == (root_directory.st_dev, root_directory.st_ino):
            events.append("root-fsync")
        original_fsync(descriptor)

    monkeypatch.setattr(os, "fsync", fsync)
    with pytest.raises(OSError, match="destination durability unavailable"):
        if create:
            _create_chat_json(tmp_path, destination, {"text": "durable only after fsync"})
        else:
            _write(destination, {"text": "durable only after fsync"})
    intent = tmp_path / ".atomic" / "intent.json"
    slot = tmp_path / ".atomic" / "staged.json"
    assert intent.is_file() and intent.stat().st_size <= 8192
    assert destination.stat().st_nlink == 2 and slot.stat().st_ino == destination.stat().st_ino
    with pytest.raises(OSError, match="destination durability unavailable"):
        _read(destination)
    with pytest.raises(OSError, match="destination durability unavailable"):
        _write(checkpoint, {"cursor": "after"})
    assert _read(checkpoint) == {"cursor": "before"}
    assert "root-fsync" not in events and intent.exists() and slot.exists()
    refusing = False
    assert _read(destination) == {"text": "durable only after fsync"}
    assert destination.stat().st_nlink == 1 and not intent.exists() and not slot.exists()
    _write(checkpoint, {"cursor": "after"})
    assert events.index("destination-succeeded") < events.index("root-fsync")
    assert _read(checkpoint) == {"cursor": "after"}


@pytest.mark.parametrize("kind", ["request", "deferred"])
def test_real_intake_replay_requires_destination_fsync_before_cursor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, kind: str,
) -> None:
    from tests.test_agentctl_chat_runtime import Rig, _message

    rig = Rig(tmp_path)
    original_fsync = os.fsync
    directory = (tmp_path / ("requests" if kind == "request" else "deferred")).stat()
    root = tmp_path.stat()
    refusing = True
    events: list[str] = []
    message = _message("durability", text="possible echo")
    if kind == "deferred":
        rig.runtime.pending_texts["possible echo"] = 1

    def fsync(descriptor: int) -> None:
        info = os.fstat(descriptor)
        if (info.st_dev, info.st_ino) == (directory.st_dev, directory.st_ino):
            if refusing:
                raise OSError(errno.EIO, "intake destination fsync failed")
            events.append("destination")
        elif (info.st_dev, info.st_ino) == (root.st_dev, root.st_ino):
            events.append("root")
        original_fsync(descriptor)

    try:
        monkeypatch.setattr(os, "fsync", fsync)
        for _ in range(2):
            with pytest.raises(OSError, match="intake destination fsync failed"):
                rig.runtime._input_event({"type": "message", "message": message, "cursor": "committed"})
            assert not (tmp_path / "input.json").exists()
            assert rig.runtime.input_state.get("cursor") is None
        assert events == []
        refusing = False
        rig.runtime._input_event({"type": "message", "message": message, "cursor": "committed"})
        assert events[0] == "destination" and "root" in events
        assert _read(tmp_path / "input.json")["cursor"] == "committed"
    finally:
        rig.finish()


@pytest.mark.parametrize("boundary", ["before-intent", "after-intent", "after-publish", "after-destination-fsync",
                                      "after-payload-unlink", "after-intent-unlink"])
@pytest.mark.parametrize("create", [False, True])
def test_atomic_durable_intent_crash_boundaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, boundary: str, create: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    if not create:
        _write(destination, {"text": "old"})
    staging = (tmp_path / ".atomic").stat()
    target = destination.parent.stat()
    original_open, original_fsync, original_unlink = os.open, os.fsync, os.unlink
    original_link, original_replace = os.link, os.replace

    def open_file(path: str | os.PathLike[str], flags: int, mode: int = 0o777, *, dir_fd: int | None = None) -> int:
        if boundary == "before-intent" and path == "intent.json" and flags & os.O_CREAT:
            raise _Crash()
        return original_open(path, flags, mode, dir_fd=dir_fd)

    def fsync(descriptor: int) -> None:
        info = os.fstat(descriptor)
        original_fsync(descriptor)
        if boundary == "after-intent" and (info.st_dev, info.st_ino) == (staging.st_dev, staging.st_ino):
            raise _Crash()
        if boundary == "after-destination-fsync" and (info.st_dev, info.st_ino) == (target.st_dev, target.st_ino):
            raise _Crash()

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        original_unlink(path, dir_fd=dir_fd)
        if ((boundary == "after-payload-unlink" and path == "staged.json")
                or (boundary == "after-intent-unlink" and path == "intent.json")):
            raise _Crash()

    def link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
        original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        if create and boundary == "after-publish":
            raise _Crash()

    def replace(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int) -> None:
        original_replace(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd)
        if boundary == "after-publish":
            raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "open", open_file)
        failing.setattr(os, "fsync", fsync)
        failing.setattr(os, "unlink", unlink)
        failing.setattr(os, "link", link)
        failing.setattr(os, "replace", replace)
        with pytest.raises(_Crash):
            if create:
                _create_chat_json(tmp_path, destination, {"text": "new"})
            else:
                _write(destination, {"text": "new"})
    with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
        pass
    if boundary in ("before-intent", "after-intent"):
        if create:
            assert not destination.exists()
        else:
            assert _read(destination) == {"text": "old"}
    else:
        assert _read(destination) == {"text": "new"}
        assert destination.stat().st_nlink == 1
    assert not any((tmp_path / ".atomic" / name).exists() for name in ("staged.json", "publish.json"))
    assert not (tmp_path / ".atomic" / "intent.json").exists()


@pytest.mark.parametrize("remove_link", [False, True])
def test_atomic_intent_read_hardlink_race_refuses_and_preserves_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, remove_link: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    original_link = os.link

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(_Crash):
            _create_chat_json(tmp_path, destination, {"text": "retained recovery evidence"})
    intent = tmp_path / ".atomic" / "intent.json"
    payload = tmp_path / ".atomic" / "staged.json"
    intent_before = intent.stat()
    intent_bytes = intent.read_bytes()
    payload_bytes = payload.read_bytes()
    extra_link = tmp_path / "unrelated-intent.json"
    original_read = os.read
    raced = False

    def race_after_read(descriptor: int, count: int) -> bytes:
        nonlocal raced
        block = original_read(descriptor, count)
        metadata = os.fstat(descriptor)
        if not raced and (metadata.st_dev, metadata.st_ino) == (intent_before.st_dev, intent_before.st_ino):
            raced = True
            os.link(intent, extra_link)
            if remove_link:
                extra_link.unlink()
        return block

    monkeypatch.setattr(os, "read", race_after_read)
    with pytest.raises(AgentDeliveryError, match="intent changed while it was read"):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert raced and intent.read_bytes() == intent_bytes and payload.read_bytes() == payload_bytes
    assert intent.stat().st_nlink == (1 if remove_link else 2)
    assert intent.stat().st_mtime_ns == intent_before.st_mtime_ns
    assert intent.stat().st_ctime_ns != intent_before.st_ctime_ns
    assert destination.stat().st_nlink == 2 and destination.stat().st_ino == payload.stat().st_ino


@pytest.mark.parametrize("name", ["staged.json", "publish.json"])
@pytest.mark.parametrize("remove_link", [False, True])
def test_internal_payload_hardlink_race_during_intent_read_retains_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, name: str, remove_link: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"

    def crash(*args: object, **kwargs: object) -> None:
        raise _Crash()

    with monkeypatch.context() as failing:
        # Staged-only or staged+publish prepublication topology; no final name
        # exists to expose the added link through its later lstat.
        failing.setattr(os, "link" if name == "staged.json" else "replace", crash)
        with pytest.raises(_Crash):
            _write(destination, {"text": "private unpublished payload"})
    intent = tmp_path / ".atomic" / "intent.json"
    payload = tmp_path / ".atomic" / name
    intent_before = intent.stat()
    payload_before = payload.stat()
    intent_bytes, payload_bytes = intent.read_bytes(), payload.read_bytes()
    extra_link = tmp_path / "unrelated-payload.json"
    original_read = os.read
    raced = False

    def race_after_read(descriptor: int, count: int) -> bytes:
        nonlocal raced
        block = original_read(descriptor, count)
        metadata = os.fstat(descriptor)
        if not raced and (metadata.st_dev, metadata.st_ino) == (intent_before.st_dev, intent_before.st_ino):
            raced = True
            os.link(payload, extra_link)
            if remove_link:
                extra_link.unlink()
        return block

    monkeypatch.setattr(os, "read", race_after_read)
    with pytest.raises(AgentDeliveryError, match="staging evidence changed during recovery"):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert raced and not destination.exists()
    assert intent.read_bytes() == intent_bytes and payload.read_bytes() == payload_bytes
    assert payload.stat().st_nlink == payload_before.st_nlink + int(not remove_link)
    assert payload.stat().st_mtime_ns == payload_before.st_mtime_ns
    assert payload.stat().st_ctime_ns != payload_before.st_ctime_ns
    assert (tmp_path / ".atomic" / "staged.json").exists()
    assert (tmp_path / ".atomic" / "publish.json").exists() == (name == "publish.json")


@pytest.mark.parametrize("matched_final", [False, True])
@pytest.mark.parametrize("cleanup_retry", [False, True])
def test_publish_only_crash_state_recovers_with_matching_or_unrelated_final(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, matched_final: bool, cleanup_retry: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    _write(destination, {"text": "old unrelated final"})
    old_identity = destination.stat()

    def crash(*args: object, **kwargs: object) -> None:
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "replace", crash)
        with pytest.raises(_Crash):
            _write(destination, {"text": "new payload"})
    stage = tmp_path / ".atomic"
    payload, publish, intent = (stage / name for name in ("staged.json", "publish.json", "intent.json"))
    # Model persisted directory state after a crash: without the cleanup dir
    # fsync, disappearance of these two internal names need not be a prefix.
    payload.unlink()
    if matched_final:
        destination.unlink()
        os.link(publish, destination)
    assert publish.stat().st_nlink == (2 if matched_final else 1)
    policy = chat_module._chat_atomic_policy(tmp_path)
    original_fsync = os.fsync
    target_info, stage_info = destination.parent.stat(), stage.stat()
    destination_syncs = 0
    fail_cleanup = cleanup_retry

    def fsync(descriptor: int) -> None:
        nonlocal destination_syncs
        info = os.fstat(descriptor)
        if (info.st_dev, info.st_ino) == (target_info.st_dev, target_info.st_ino):
            destination_syncs += 1
        if (fail_cleanup and (info.st_dev, info.st_ino) == (stage_info.st_dev, stage_info.st_ino)
                and not publish.exists()):
            raise OSError(errno.EIO, "publish-only cleanup fsync failed")
        original_fsync(descriptor)

    monkeypatch.setattr(os, "fsync", fsync)
    if cleanup_retry:
        with pytest.raises(OSError, match="publish-only cleanup fsync failed"):
            with agent_module.atomic_write_recovery(policy):
                pass
        assert intent.exists() and not payload.exists() and not publish.exists()
        fail_cleanup = False
        evidence = intent.read_bytes()
        if matched_final:
            with agent_module.atomic_write_recovery(policy):
                pass
        else:
            with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
                with agent_module.atomic_write_recovery(policy):
                    pass
            assert intent.read_bytes() == evidence
    else:
        with agent_module.atomic_write_recovery(policy):
            pass
    assert _read(destination) == {"text": "new payload" if matched_final else "old unrelated final"}
    assert destination.stat().st_nlink == 1
    assert bool(destination_syncs) == matched_final
    if not matched_final:
        assert destination.stat().st_ino == old_identity.st_ino
    assert not payload.exists() and not publish.exists() and intent.exists() == (cleanup_retry and not matched_final)


@pytest.mark.parametrize("remove_link", [False, True])
def test_eexist_cleanup_revalidates_payload_after_checking_intent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, remove_link: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    _create_chat_json(tmp_path, destination, {"text": "existing"})
    before = destination.read_bytes()
    payload = tmp_path / ".atomic" / "staged.json"
    intent = tmp_path / ".atomic" / "intent.json"
    extra_link = tmp_path / "unrelated-rejected-body.json"
    original_entry = agent_module._atomic_entry
    raced = False

    def entry(directory: int, name: str, limit: int) -> tuple[int, os.stat_result] | None:
        nonlocal raced
        opened = original_entry(directory, name, limit)
        if not raced and name == "intent.json" and opened is not None:
            raced = True
            os.link(payload, extra_link)
            if remove_link:
                extra_link.unlink()
        return opened

    monkeypatch.setattr(agent_module, "_atomic_entry", entry)
    with pytest.raises(AgentDeliveryError, match="staging evidence changed during recovery"):
        _create_chat_json(tmp_path, destination, {"text": "rejected private body"})
    assert raced and destination.read_bytes() == before
    assert json.loads(payload.read_bytes()) == {"text": "rejected private body"}
    assert intent.exists() and payload.stat().st_nlink == (1 if remove_link else 2)


@pytest.mark.parametrize("phase", ["writer-create", "writer-replace", "recovery"])
@pytest.mark.parametrize("remove_link", [False, True])
def test_destination_fsync_hardlink_race_refuses_without_discarding_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, phase: str, remove_link: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    policy = chat_module._chat_atomic_policy(tmp_path)
    if phase == "recovery":
        original_link = os.link

        def link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
            original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                          follow_symlinks=follow_symlinks)
            raise _Crash()

        with monkeypatch.context() as failing:
            failing.setattr(os, "link", link)
            with pytest.raises(_Crash):
                _create_chat_json(tmp_path, destination, {"text": "sensitive"})
    directory = destination.parent.stat()
    original_fsync = os.fsync
    raced = False
    extra_link = tmp_path / "unrelated-published-body.json"

    def fsync(descriptor: int) -> None:
        nonlocal raced
        original_fsync(descriptor)
        metadata = os.fstat(descriptor)
        if not raced and (metadata.st_dev, metadata.st_ino) == (directory.st_dev, directory.st_ino):
            raced = True
            os.link(destination, extra_link)
            if remove_link:
                extra_link.unlink()

    monkeypatch.setattr(os, "fsync", fsync)
    with pytest.raises(AgentDeliveryError, match="destination changed during publication or recovery"):
        if phase == "recovery":
            with agent_module.atomic_write_recovery(policy):
                pass
        elif phase == "writer-create":
            _create_chat_json(tmp_path, destination, {"text": "sensitive"})
        else:
            _write(destination, {"text": "sensitive"})
    assert raced and (tmp_path / ".atomic" / "intent.json").exists()
    payload = tmp_path / ".atomic" / "staged.json"
    assert payload.stat().st_ino == destination.stat().st_ino
    assert payload.stat().st_nlink == (2 if remove_link else 3)


@pytest.mark.parametrize("recovering", [False, True])
@pytest.mark.parametrize("remove_link", [False, True])
def test_post_anchor_cleanup_fsync_race_keeps_intent_until_final_is_revalidated(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, recovering: bool, remove_link: bool,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    policy = chat_module._chat_atomic_policy(tmp_path)
    if recovering:
        original_link = os.link

        def link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
            original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                          follow_symlinks=follow_symlinks)
            raise _Crash()

        with monkeypatch.context() as failing:
            failing.setattr(os, "link", link)
            with pytest.raises(_Crash):
                _create_chat_json(tmp_path, destination, {"text": "final"})
    stage = tmp_path / ".atomic"
    directory = stage.stat()
    original_fsync = os.fsync
    raced = False
    extra_link = tmp_path / "late-alias.json"

    def fsync(descriptor: int) -> None:
        nonlocal raced
        original_fsync(descriptor)
        metadata = os.fstat(descriptor)
        if (not raced and (metadata.st_dev, metadata.st_ino) == (directory.st_dev, directory.st_ino)
                and (stage / "intent.json").exists() and not (stage / "staged.json").exists()):
            raced = True
            os.link(destination, extra_link)
            if remove_link:
                extra_link.unlink()

    monkeypatch.setattr(os, "fsync", fsync)
    with pytest.raises(AgentDeliveryError, match="destination changed during publication or recovery"):
        if recovering:
            with agent_module.atomic_write_recovery(policy):
                pass
        else:
            _create_chat_json(tmp_path, destination, {"text": "final"})
    assert raced and (stage / "intent.json").exists()
    assert not (stage / "staged.json").exists()
    assert destination.stat().st_nlink == (1 if remove_link else 2)


@pytest.mark.parametrize("corruption", ["partial", "escape", "absolute", "mode", "oversized"])
def test_invalid_atomic_intent_refuses_without_discarding_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, corruption: str,
) -> None:
    setup(tmp_path)
    destination = tmp_path / "submissions" / f"{_KEY}.json"
    original_link = os.link

    def link(source: str, target: str, *, src_dir_fd: int, dst_dir_fd: int, follow_symlinks: bool) -> None:
        original_link(source, target, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                      follow_symlinks=follow_symlinks)
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "link", link)
        with pytest.raises(_Crash):
            _create_chat_json(tmp_path, destination, {"text": "evidence"})
    intent = tmp_path / ".atomic" / "intent.json"
    if corruption in ("escape", "absolute"):
        document = json.loads(intent.read_bytes())
        document["destination"] = "../escape.json" if corruption == "escape" else "/escape.json"
        intent.write_text(json.dumps(document))
    elif corruption == "mode":
        intent.chmod(0o644)
    else:
        intent.write_bytes(b"{" if corruption == "partial" else b"x" * 8193)
    before = intent.read_bytes()
    with pytest.raises((AgentDeliveryError, OSError)):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert intent.read_bytes() == before
    assert destination.stat().st_nlink == 2
    assert (tmp_path / ".atomic" / "staged.json").stat().st_ino == destination.stat().st_ino


@pytest.mark.parametrize("phase", ["eexist", "unmatched-staged", "unmatched-publish", "publish-only",
                                  "invalid-intent", "no-intent"])
def test_unpublished_payload_alias_at_actual_unlink_keeps_terminal_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, phase: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    payload, publish, intent = (stage / name for name in ("staged.json", "publish.json", "intent.json"))
    destination = tmp_path / "submissions" / "unlink-race.json"
    secret: dict[str, object] = {"text": "private rejected payload"}
    policy = chat_module._chat_atomic_policy(tmp_path)
    if phase == "eexist":
        _create_chat_json(tmp_path, destination, {"text": "unchanged final"})
    elif phase in ("invalid-intent", "no-intent"):
        payload.write_text(json.dumps(secret))
        payload.chmod(0o600)
        if phase == "invalid-intent":
            intent.write_bytes(b"{")
            intent.chmod(0o600)
    else:
        def crash(*args: object, **kwargs: object) -> None:
            raise _Crash()

        with monkeypatch.context() as failing:
            failing.setattr(os, "replace", crash)
            with pytest.raises(_Crash):
                _write(destination, secret)
        if phase == "publish-only":
            payload.unlink()
    boundary = "publish.json" if phase in ("unmatched-publish", "publish-only") else "staged.json"
    alias = tmp_path / "unexpected-private-alias.json"
    original_unlink = os.unlink
    raced = False

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        nonlocal raced
        if not raced and path == boundary and dir_fd is not None:
            raced = True
            os.link(stage / boundary, alias)
        original_unlink(path, dir_fd=dir_fd)

    monkeypatch.setattr(os, "unlink", unlink)
    with pytest.raises(AgentDeliveryError, match="changed during publication or recovery cleanup"):
        if phase == "eexist":
            _create_chat_json(tmp_path, destination, secret)
        else:
            with agent_module.atomic_write_recovery(policy):
                pass
    assert raced and json.loads(alias.read_bytes()) == secret
    marker = intent.read_bytes()
    assert marker.startswith(agent_module._ATOMIC_REFUSAL_PREFIX)
    record = json.loads(marker[len(agent_module._ATOMIC_REFUSAL_PREFIX):])
    assert record["type"] == "atomic-cleanup-refusal" and record["version"] == 1
    assert len(record["intent_sha256"]) == 64 and b"private rejected payload" not in marker
    assert stat.S_IMODE(intent.stat().st_mode) == 0o600 and intent.stat().st_nlink == 1
    assert len(marker) <= 8192
    if phase == "eexist":
        assert _read(destination) == {"text": "unchanged final"}
    else:
        assert not destination.exists()
    with pytest.raises(AgentDeliveryError, match=r"cleanup interference; evidence retained \(sha256=[a-f0-9]{64}\)"):
        with agent_module.atomic_write_recovery(policy):
            pass
    assert intent.read_bytes() == marker and alias.exists()


@pytest.mark.parametrize("recovering", [False, True])
@pytest.mark.parametrize(("boundary", "remove_alias"), [
    ("payload", False), ("final-check", False), ("final-check", True),
    ("cleanup-fsync", False), ("cleanup-fsync", True),
])
def test_matched_final_precommit_boundaries_retain_durable_refusal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, recovering: bool, boundary: str, remove_alias: bool,
) -> None:
    # Own payload unlink also changes ctime: no claim is made about a transient
    # alias wholly inside that syscall. Retained payload aliases are covered.
    setup(tmp_path)
    destination = tmp_path / "submissions" / "unlink-final.json"
    secret: dict[str, object] = {"text": "published private payload"}
    if recovering:
        original_link = os.link

        def crash_link(source: str, name: str, *, src_dir_fd: int, dst_dir_fd: int,
                       follow_symlinks: bool) -> None:
            original_link(source, name, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd,
                          follow_symlinks=follow_symlinks)
            raise _Crash()

        with monkeypatch.context() as failing:
            failing.setattr(os, "link", crash_link)
            with pytest.raises(_Crash):
                _create_chat_json(tmp_path, destination, secret)
    stage = tmp_path / ".atomic"
    intent = stage / "intent.json"
    stage_info = stage.stat()
    alias = tmp_path / "unexpected-final-alias.json"
    original_unlink, original_fsync = os.unlink, os.fsync
    original_verify = agent_module._verify_atomic_final
    raced = False

    def race() -> None:
        nonlocal raced
        raced = True
        os.link(destination, alias)
        if remove_alias:
            original_unlink(alias)

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        if not raced and dir_fd is not None and boundary == "payload" and path == "staged.json":
            race()
        original_unlink(path, dir_fd=dir_fd)

    def fsync(descriptor: int) -> None:
        original_fsync(descriptor)
        metadata = os.fstat(descriptor)
        if (not raced and boundary == "cleanup-fsync" and intent.exists()
                and destination.exists() and not (stage / "staged.json").exists()
                and (metadata.st_dev, metadata.st_ino) == (stage_info.st_dev, stage_info.st_ino)):
            race()

    def verify(target: int, name: str, descriptor: int, expected: os.stat_result, *,
               links: int, stable_ctime: bool = True) -> os.stat_result:
        if not raced and boundary == "final-check" and intent.exists() and not (stage / "staged.json").exists():
            race()
        return original_verify(target, name, descriptor, expected, links=links, stable_ctime=stable_ctime)

    monkeypatch.setattr(os, "unlink", unlink)
    monkeypatch.setattr(os, "fsync", fsync)
    monkeypatch.setattr(agent_module, "_verify_atomic_final", verify)
    with pytest.raises(AgentDeliveryError, match="destination changed during publication or recovery"):
        if recovering:
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
        else:
            _create_chat_json(tmp_path, destination, secret)
    assert raced and json.loads(destination.read_bytes()) == secret
    assert intent.read_bytes().startswith(agent_module._ATOMIC_REFUSAL_PREFIX)
    marker = intent.read_bytes()
    with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert intent.read_bytes() == marker


def test_partial_refusal_record_never_becomes_recoverable_unpublished_intent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    payload, intent = stage / "staged.json", stage / "intent.json"
    payload.write_bytes(b"private payload")
    payload.chmod(0o600)
    alias = tmp_path / "unexpected-alias"
    original_unlink, original_write = os.unlink, os.write

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        if path == "staged.json":
            os.link(payload, alias)
        original_unlink(path, dir_fd=dir_fd)

    def write(descriptor: int, data: bytes) -> int:
        if data.startswith(agent_module._ATOMIC_REFUSAL_PREFIX):
            original_write(descriptor, data[:1])
            raise OSError(errno.EIO, "partial refusal write")
        return original_write(descriptor, data)

    with monkeypatch.context() as failing:
        failing.setattr(os, "unlink", unlink)
        failing.setattr(os, "write", write)
        with pytest.raises(OSError, match="partial refusal write"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
    partial_refusal = intent.read_bytes()
    assert partial_refusal.startswith(b"!") and alias.read_bytes() == b"private payload"
    with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert intent.read_bytes() == partial_refusal


@pytest.mark.parametrize("state", ["valid", "partial", "absent"])
@pytest.mark.parametrize("failure", ["before-byte", "partial-write", "intent-fsync", "directory-fsync"])
def test_cleanup_phase_preparation_failure_preserves_original_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, state: str, failure: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    payload, intent = stage / "staged.json", stage / "intent.json"
    destination = tmp_path / "submissions" / "phase-preparation.json"
    if state == "valid":
        def crash(*args: object, **kwargs: object) -> None:
            raise _Crash()

        with monkeypatch.context() as failing:
            failing.setattr(os, "replace", crash)
            with pytest.raises(_Crash):
                _write(destination, {"text": "original private body"})
    else:
        payload.write_bytes(b"original private body")
        payload.chmod(0o600)
        if state == "partial":
            intent.write_bytes(b"{")
            intent.chmod(0o600)
    original = intent.read_bytes() if intent.exists() else None
    payload_bytes, payload_info = payload.read_bytes(), payload.stat()
    stage_info = stage.stat()
    original_write, original_fsync = os.write, os.fsync
    started = failed = False
    phase_fd = -1

    def write(descriptor: int, data: bytes) -> int:
        nonlocal started, failed, phase_fd
        if data == agent_module._ATOMIC_CLEANUP_PHASE:
            started, phase_fd = True, descriptor
            if failure in ("before-byte", "partial-write") and not failed:
                failed = True
                if failure == "partial-write":
                    original_write(descriptor, data[:5])
                raise OSError(errno.EIO, "phase preparation fault")
        return original_write(descriptor, data)

    def fsync(descriptor: int) -> None:
        nonlocal failed
        metadata = os.fstat(descriptor)
        if started and not failed and (
            (failure == "intent-fsync" and descriptor == phase_fd)
            or (failure == "directory-fsync" and (metadata.st_dev, metadata.st_ino)
                == (stage_info.st_dev, stage_info.st_ino))
        ):
            failed = True
            raise OSError(errno.EIO, "phase preparation fault")
        original_fsync(descriptor)

    with monkeypatch.context() as failing:
        failing.setattr(os, "write", write)
        failing.setattr(os, "fsync", fsync)
        with pytest.raises(OSError, match="phase preparation fault"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
    assert started and failed and payload.read_bytes() == payload_bytes
    assert (payload.stat().st_ino, payload.stat().st_nlink) == (payload_info.st_ino, payload_info.st_nlink)
    assert (intent.read_bytes() if intent.exists() else None) == original
    assert not destination.exists()
    with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
        pass
    assert not payload.exists() and not intent.exists()


@pytest.mark.parametrize("partial_bytes", [1, 5])
def test_phase_partial_append_and_failed_rollback_never_authorize_unsafe_cleanup(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, partial_bytes: int,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    payload, intent = stage / "staged.json", stage / "intent.json"
    destination = tmp_path / "submissions" / "partial-phase.json"

    def crash(*args: object, **kwargs: object) -> None:
        raise _Crash()

    with monkeypatch.context() as failing:
        failing.setattr(os, "replace", crash)
        with pytest.raises(_Crash):
            _write(destination, {"text": "payload"})
    original = intent.read_bytes()
    original_write = os.write

    def write(descriptor: int, data: bytes) -> int:
        if data == agent_module._ATOMIC_CLEANUP_PHASE:
            original_write(descriptor, data[:partial_bytes])
            raise OSError(errno.EIO, "phase write fault")
        return original_write(descriptor, data)

    def truncate(descriptor: int, length: int) -> None:
        raise OSError(errno.EIO, "rollback fault")

    with monkeypatch.context() as failing:
        failing.setattr(os, "write", write)
        failing.setattr(os, "ftruncate", truncate)
        with pytest.raises(OSError, match="rollback fault"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
    assert intent.read_bytes() == original + agent_module._ATOMIC_CLEANUP_PHASE[:partial_bytes]
    assert payload.exists() and (stage / "publish.json").exists() and not destination.exists()
    if partial_bytes == 1:
        # A newline is JSON whitespace, but cannot authorize any unlink: the
        # interrupted preparer never fsynced a complete phase or began cleanup.
        # Restart must prepare a complete phase before touching either link.
        original_unlink = os.unlink

        def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
            if path in ("staged.json", "publish.json"):
                assert intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_PHASE)
            original_unlink(path, dir_fd=dir_fd)

        monkeypatch.setattr(os, "unlink", unlink)
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
        assert not payload.exists() and not intent.exists()
    else:
        evidence = intent.read_bytes()
        with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
        assert intent.read_bytes() == evidence and payload.exists()


@pytest.mark.parametrize("failure", ["before-byte", "partial-write", "refusal-fsync"])
def test_durable_phase_survives_refusal_record_failure_after_payload_alias_race(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failure: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    payload, intent = stage / "staged.json", stage / "intent.json"
    payload.write_bytes(b"sensitive body")
    payload.chmod(0o600)
    alias = tmp_path / "unrelated-alias"
    original_write, original_unlink, original_fsync = os.write, os.unlink, os.fsync
    refusal_fd = -1

    def write(descriptor: int, data: bytes) -> int:
        nonlocal refusal_fd
        if data.startswith(agent_module._ATOMIC_REFUSAL_PREFIX):
            refusal_fd = descriptor
            if failure in ("before-byte", "partial-write"):
                if failure == "partial-write":
                    original_write(descriptor, data[:1])
                raise OSError(errno.EIO, "refusal persistence fault")
        return original_write(descriptor, data)

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        if path == "staged.json":
            assert intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_PHASE)
            os.link(payload, alias)
        original_unlink(path, dir_fd=dir_fd)

    def fsync(descriptor: int) -> None:
        if failure == "refusal-fsync" and descriptor == refusal_fd:
            raise OSError(errno.EIO, "refusal persistence fault")
        original_fsync(descriptor)

    with monkeypatch.context() as failing:
        failing.setattr(os, "write", write)
        failing.setattr(os, "unlink", unlink)
        failing.setattr(os, "fsync", fsync)
        with pytest.raises(OSError, match="refusal persistence fault"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
    evidence = intent.read_bytes()
    assert len(evidence) <= 8192 and alias.read_bytes() == b"sensitive body" and not payload.exists()
    with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert intent.read_bytes() == evidence and alias.exists()


@pytest.mark.parametrize("failure", ["unlink-before", "unlink-after", "verify-oserror", "verify-missing", "baseexception"])
def test_cleanup_operational_failures_keep_durable_phase_before_commit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failure: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    payload, intent = stage / "staged.json", stage / "intent.json"
    destination = tmp_path / "submissions" / "operational-fault.json"
    original_unlink, original_verify = os.unlink, agent_module._verify_atomic_final
    phase_synced = directory_synced = False
    original_fsync = os.fsync
    stage_info = stage.stat()

    def fsync(descriptor: int) -> None:
        nonlocal phase_synced, directory_synced
        original_fsync(descriptor)
        metadata = os.fstat(descriptor)
        if intent.exists() and intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_PHASE):
            if (metadata.st_dev, metadata.st_ino) == (intent.stat().st_dev, intent.stat().st_ino):
                phase_synced = True
            elif (metadata.st_dev, metadata.st_ino) == (stage_info.st_dev, stage_info.st_ino):
                assert phase_synced
                directory_synced = True

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        if path == "staged.json":
            assert phase_synced and directory_synced
            if failure == "unlink-before":
                raise OSError(errno.EIO, "cleanup fault")
        original_unlink(path, dir_fd=dir_fd)
        if path == "staged.json":
            if failure == "unlink-after":
                raise OSError(errno.EIO, "cleanup fault")
            if failure == "baseexception":
                raise _Crash()

    def verify(target: int, name: str, descriptor: int, expected: os.stat_result, *,
               links: int, stable_ctime: bool = True) -> os.stat_result:
        if not payload.exists() and intent.exists():
            if failure == "verify-missing":
                raise FileNotFoundError(errno.ENOENT, "cleanup fault")
            if failure == "verify-oserror":
                raise OSError(errno.EIO, "cleanup fault")
        return original_verify(target, name, descriptor, expected, links=links, stable_ctime=stable_ctime)

    with monkeypatch.context() as failing:
        failing.setattr(os, "fsync", fsync)
        failing.setattr(os, "unlink", unlink)
        failing.setattr(agent_module, "_verify_atomic_final", verify)
        with pytest.raises(_Crash if failure == "baseexception" else OSError):
            _create_chat_json(tmp_path, destination, {"text": "committed payload"})
    evidence = intent.read_bytes()
    assert phase_synced and directory_synced and evidence.endswith(agent_module._ATOMIC_CLEANUP_PHASE)
    # The matching final is still a trusted anchor, so ordinary operational
    # interruption must remain recoverable, unlike a proven alias violation.
    with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
        pass
    assert not intent.exists() and destination.exists() and destination.stat().st_nlink == 1


@pytest.mark.parametrize("failure", ["none", "unlink-after", "directory-fsync"])
def test_phase_unlink_is_commit_without_later_semantic_revalidation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failure: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    destination = tmp_path / "submissions" / "commit-boundary.json"
    moved = tmp_path / "moved-after-commit.json"
    original_unlink, original_verify, original_fsync = os.unlink, agent_module._verify_atomic_final, os.fsync
    committed = False
    checks = 0

    def verify(target: int, name: str, descriptor: int, expected: os.stat_result, *,
               links: int, stable_ctime: bool = True) -> os.stat_result:
        nonlocal checks
        assert not committed, "no semantic operation may follow the evidence-removal commit point"
        checks += 1
        return original_verify(target, name, descriptor, expected, links=links, stable_ctime=stable_ctime)

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        nonlocal committed
        original_unlink(path, dir_fd=dir_fd)
        if path == "intent.json":
            assert checks > 0 and not (stage / "staged.json").exists()
            committed = True
            # Deliberate same-UID modification AFTER the final check is outside
            # the protocol. No later check may depend on recreating evidence.
            os.rename(destination, moved)
            if failure == "unlink-after":
                raise OSError(errno.EIO, "post-commit fault")

    def fsync(descriptor: int) -> None:
        if committed and failure == "directory-fsync":
            raise OSError(errno.EIO, "post-commit fault")
        original_fsync(descriptor)

    with monkeypatch.context() as failing:
        failing.setattr(os, "unlink", unlink)
        failing.setattr(os, "fsync", fsync)
        failing.setattr(agent_module, "_verify_atomic_final", verify)
        if failure == "none":
            _create_chat_json(tmp_path, destination, {"text": "safe before commit"})
        else:
            with pytest.raises(OSError, match="post-commit fault"):
                _create_chat_json(tmp_path, destination, {"text": "safe before commit"})
    assert committed and not destination.exists() and json.loads(moved.read_bytes()) == {"text": "safe before commit"}
    assert not (stage / "intent.json").exists() and moved.stat().st_nlink == 1
    with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
        pass


@pytest.mark.parametrize("matched_final", [False, True])
@pytest.mark.parametrize("failure", ["commit-before-byte", "commit-partial", "commit-file-fsync",
                                     "commit-truncate", "commit-directory-fsync",
                                     "intent-unlink-before", "intent-unlink-after"])
def test_cleanup_commit_faults_resume_only_with_anchor_or_complete_certificate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, matched_final: bool, failure: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    intent = stage / "intent.json"
    destination = tmp_path / "submissions" / "commit-fault.json"
    if not matched_final:
        _create_chat_json(tmp_path, destination, {"text": "unrelated old final"})
    original_write, original_fsync, original_unlink = os.write, os.fsync, os.unlink
    original_truncate = os.ftruncate
    commit_fd = -1
    failed = False
    stage_info = stage.stat()

    def fail() -> None:
        nonlocal failed
        failed = True
        raise OSError(errno.EIO, "cleanup commit fault")

    def write(descriptor: int, data: bytes) -> int:
        nonlocal commit_fd
        if data == agent_module._ATOMIC_CLEANUP_COMMIT:
            commit_fd = descriptor
            assert not (stage / "staged.json").exists() and not (stage / "publish.json").exists()
            if failure == "commit-before-byte":
                fail()
            if failure == "commit-partial":
                original_write(descriptor, data[:5])
                fail()
        return original_write(descriptor, data)

    def fsync(descriptor: int) -> None:
        metadata = os.fstat(descriptor)
        if commit_fd >= 0 and not failed and (
            (failure == "commit-file-fsync" and descriptor == commit_fd)
            or (failure == "commit-directory-fsync" and (metadata.st_dev, metadata.st_ino)
                == (stage_info.st_dev, stage_info.st_ino))
        ):
            fail()
        original_fsync(descriptor)

    def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
        if path == "intent.json":
            assert intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_COMMIT)
            if failure == "intent-unlink-before":
                fail()
        original_unlink(path, dir_fd=dir_fd)
        if path == "intent.json" and failure == "intent-unlink-after":
            fail()

    def truncate(descriptor: int, length: int) -> None:
        if failure == "commit-truncate" and descriptor == commit_fd:
            fail()
        original_truncate(descriptor, length)

    with monkeypatch.context() as failing:
        failing.setattr(os, "write", write)
        failing.setattr(os, "fsync", fsync)
        failing.setattr(os, "unlink", unlink)
        failing.setattr(os, "ftruncate", truncate)
        with pytest.raises(OSError, match="cleanup commit fault"):
            _create_chat_json(tmp_path, destination, {"text": "new body"})
    assert failed and not (stage / "staged.json").exists()
    ambiguous = not matched_final and failure in ("commit-before-byte", "commit-partial")
    if ambiguous:
        evidence = intent.read_bytes()
        with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
        assert intent.read_bytes() == evidence
    else:
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
        assert not intent.exists()
    assert json.loads(destination.read_bytes()) == {"text": "new body" if matched_final else "unrelated old final"}
    assert destination.stat().st_nlink == 1


@pytest.mark.parametrize("matched_final", [False, True])
@pytest.mark.parametrize("boundary", ["after-last-unlink", "after-commit-fsync"])
def test_abrupt_process_death_leaves_provable_cleanup_topology(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, matched_final: bool, boundary: str,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    intent = stage / "intent.json"
    destination = tmp_path / "submissions" / "process-crash.json"
    if not matched_final:
        _create_chat_json(tmp_path, destination, {"text": "old unrelated"})
    child = os.fork()
    if child == 0:
        original_unlink, original_fsync = os.unlink, os.fsync

        def unlink(path: str | os.PathLike[str], *, dir_fd: int | None = None) -> None:
            original_unlink(path, dir_fd=dir_fd)
            if path == "staged.json" and boundary == "after-last-unlink":
                os._exit(23)  # No exception handler/finally can save evidence.

        def fsync(descriptor: int) -> None:
            original_fsync(descriptor)
            metadata = os.fstat(descriptor)
            if (boundary == "after-commit-fsync" and intent.exists()
                    and intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_COMMIT)
                    and (metadata.st_dev, metadata.st_ino) == (intent.stat().st_dev, intent.stat().st_ino)):
                os._exit(23)

        monkeypatch.setattr(os, "unlink", unlink)
        monkeypatch.setattr(os, "fsync", fsync)
        try:
            _create_chat_json(tmp_path, destination, {"text": "new body"})
        except BaseException:
            os._exit(91)
        os._exit(92)
    waited, status = os.waitpid(child, 0)
    assert waited == child and os.WIFEXITED(status) and os.WEXITSTATUS(status) == 23
    assert not (stage / "staged.json").exists()
    evidence = intent.read_bytes()
    assert len(evidence) <= 8192 and b"new body" not in evidence
    if not matched_final and boundary == "after-last-unlink":
        with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
            with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
                pass
        assert intent.read_bytes() == evidence
    else:
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
        assert not intent.exists()
    assert json.loads(destination.read_bytes()) == {"text": "new body" if matched_final else "old unrelated"}


@pytest.mark.parametrize("boundary", ["phase-read", "commit-fsync"])
@pytest.mark.parametrize("remove_alias", [False, True])
def test_cleanup_certificate_revalidates_identity_and_ctime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, boundary: str, remove_alias: bool,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    intent, payload = stage / "intent.json", stage / "staged.json"
    destination = tmp_path / "submissions" / "certificate-race.json"
    alias = tmp_path / "unrelated-intent-alias"
    original_pread, original_fsync = os.pread, os.fsync
    raced = False

    def race() -> None:
        nonlocal raced
        raced = True
        os.link(intent, alias)
        if remove_alias:
            alias.unlink()

    def pread(descriptor: int, length: int, offset: int) -> bytes:
        data = original_pread(descriptor, length, offset)
        if (not raced and boundary == "phase-read" and not payload.exists()
                and data.endswith(agent_module._ATOMIC_CLEANUP_PHASE)):
            race()
        return data

    def fsync(descriptor: int) -> None:
        original_fsync(descriptor)
        metadata = os.fstat(descriptor)
        if (not raced and boundary == "commit-fsync" and intent.exists()
                and intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_COMMIT)
                and (metadata.st_dev, metadata.st_ino) == (intent.stat().st_dev, intent.stat().st_ino)):
            race()

    with monkeypatch.context() as failing:
        failing.setattr(os, "pread", pread)
        failing.setattr(os, "fsync", fsync)
        with pytest.raises(AgentDeliveryError):
            _create_chat_json(tmp_path, destination, {"text": "preserved final"})
    assert raced and intent.exists() and not payload.exists()
    evidence = intent.read_bytes()
    with pytest.raises(AgentDeliveryError):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass
    assert intent.read_bytes() == evidence and json.loads(destination.read_bytes()) == {"text": "preserved final"}


def test_cleanup_certificate_requires_exact_bytes_after_fsync(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    stage = tmp_path / ".atomic"
    intent = stage / "intent.json"
    destination = tmp_path / "submissions" / "certificate-content.json"
    original_fsync = os.fsync
    corrupted = False

    def fsync(descriptor: int) -> None:
        nonlocal corrupted
        original_fsync(descriptor)
        metadata = os.fstat(descriptor)
        if (not corrupted and intent.exists() and intent.read_bytes().endswith(agent_module._ATOMIC_CLEANUP_COMMIT)
                and (metadata.st_dev, metadata.st_ino) == (intent.stat().st_dev, intent.stat().st_ino)):
            corrupted = True
            original = intent.read_bytes()
            intent.write_bytes(original[:-2] + b"X\n")

    with monkeypatch.context() as failing:
        failing.setattr(os, "fsync", fsync)
        with pytest.raises(AgentDeliveryError, match="certificate contents changed"):
            _create_chat_json(tmp_path, destination, {"text": "unchanged"})
    assert corrupted and intent.read_bytes().startswith(agent_module._ATOMIC_REFUSAL_PREFIX)
    with pytest.raises(AgentDeliveryError, match="cleanup interference; evidence retained"):
        with agent_module.atomic_write_recovery(chat_module._chat_atomic_policy(tmp_path)):
            pass


@pytest.mark.parametrize("kind", ["symlink", "mode", "fifo", "directory", "oversized"])
def test_unsafe_fixed_slot_is_refused_without_deleting_it(
    tmp_path: Path, kind: str,
) -> None:
    setup(tmp_path)
    slot = tmp_path / ".atomic" / "staged.json"
    outside = tmp_path / "outside.txt"
    outside.write_text("preserve me")
    if kind == "symlink":
        slot.symlink_to(outside)
    elif kind == "fifo":
        os.mkfifo(slot, 0o600)
    elif kind == "directory":
        slot.mkdir(mode=0o700)
    else:
        slot.write_bytes(b"x" * (chat_module._MAX_ATOMIC_STAGE_BYTES + 1 if kind == "oversized" else 1))
        slot.chmod(0o644 if kind == "mode" else 0o600)
    before = slot.lstat()
    with pytest.raises((OSError, AgentDeliveryError)):
        _write(tmp_path / "observer.json", {"state": "ready"})
    assert slot.lstat().st_ino == before.st_ino
    assert outside.read_text() == "preserve me"
    assert not (tmp_path / "observer.json").exists()


@pytest.mark.parametrize("relative", _DESTINATIONS)
def test_old_scattered_temps_are_reported_and_preserved_not_glob_deleted(
    tmp_path: Path, relative: str,
) -> None:
    setup(tmp_path)
    destination = tmp_path / relative
    _parents(tmp_path, destination)
    stale = destination.parent / ".message.crash-left"
    stale.write_bytes(b"old partial JSON")
    stale.chmod(0o600)
    with pytest.raises(ValueError, match="offline cleanup") as failure:
        _audit(tmp_path)
    assert str(stale) in str(failure.value)
    assert "16 bytes" in str(failure.value)
    assert stale.read_bytes() == b"old partial JSON"
    assert not (tmp_path / ".atomic" / "audited-v1.json").exists()


@pytest.mark.parametrize("cap", ["count", "bytes"])
def test_scattered_temp_audit_refuses_at_bounded_count_and_bytes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, cap: str,
) -> None:
    setup(tmp_path)
    for index in range(3):
        path = tmp_path / "requests" / f".message.{index}"
        path.write_bytes(b"1234")
        path.chmod(0o600)
    monkeypatch.setattr(chat_module, "_MAX_LEGACY_ATOMIC_TEMP_FILES", 2 if cap == "count" else 10)
    monkeypatch.setattr(chat_module, "_MAX_LEGACY_ATOMIC_TEMP_BYTES", 8 if cap == "bytes" else 1024)
    with pytest.raises(ValueError, match="budget exceeded: 3 files, 12 bytes"):
        _audit(tmp_path)
    assert len(list((tmp_path / "requests").glob(".message.*"))) == 3


@pytest.mark.parametrize("domain", ["submission", "binding", "delivery"])
@pytest.mark.parametrize("leaves_stale", [False, True])
def test_owner_migration_waits_for_each_legacy_writer_before_scanning(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, domain: str, leaves_stale: bool,
) -> None:
    setup(tmp_path)
    directory = tmp_path / ("submissions" if domain == "submission" else "queue")
    lock_path = (tmp_path / ".submissions.lock" if domain == "submission"
                 else directory / f".{domain}.lock")
    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
    fcntl.flock(descriptor, fcntl.LOCK_EX)
    metadata = os.fstat(descriptor)
    identity = (metadata.st_dev, metadata.st_ino)
    attempted, scanned = threading.Event(), threading.Event()
    failures: list[BaseException] = []
    original_flock, original_scandir = fcntl.flock, os.scandir
    live = directory / ".message.live"
    live.write_bytes(b"live writer")
    live.chmod(0o600)

    def flock(fd: int, operation: int) -> None:
        info = os.fstat(fd)
        if threading.current_thread().name == "atomic-audit" and (info.st_dev, info.st_ino) == identity:
            attempted.set()
        original_flock(fd, operation)

    def scandir(path: int | str | os.PathLike[str]) -> AbstractContextManager[Iterator[os.DirEntry[str]]]:
        if path == directory:
            scanned.set()
        return original_scandir(path)

    def audit() -> None:
        try:
            _audit(tmp_path)
        except BaseException as exc:
            failures.append(exc)

    monkeypatch.setattr(fcntl, "flock", flock)
    monkeypatch.setattr(os, "scandir", scandir)
    worker = threading.Thread(target=audit, name="atomic-audit")
    worker.start()
    try:
        assert attempted.wait(5)
        assert not scanned.is_set() and live.read_bytes() == b"live writer"
        if not leaves_stale:
            live.unlink()  # the live writer finishes while retaining its lock
        os.close(descriptor)
        descriptor = -1
        worker.join(timeout=5)
        assert not worker.is_alive()
        assert scanned.is_set()
        if leaves_stale:
            assert len(failures) == 1 and isinstance(failures[0], ValueError)
            assert "offline cleanup" in str(failures[0]) and str(live) in str(failures[0])
            assert live.read_bytes() == b"live writer"
            assert not (tmp_path / ".atomic" / "audited-v1.json").exists()
        else:
            assert not failures
            assert (tmp_path / ".atomic" / "audited-v1.json").exists()
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        worker.join(timeout=5)


def test_migration_lock_order_and_queue_release_have_no_reverse_edge(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    acquired: list[str] = []
    names: dict[int, str] = {}
    original_open, original_flock = agent_module._open_private_lock, fcntl.flock

    def open_lock(path: str, purpose: str) -> int:
        descriptor = original_open(path, purpose)
        names[descriptor] = Path(path).name
        return descriptor

    def flock(fd: int, operation: int) -> None:
        if fd in names:
            acquired.append(names[fd])
        original_flock(fd, operation)

    with monkeypatch.context() as tracing:
        tracing.setattr(chat_module, "_open_private_lock", open_lock)
        tracing.setattr(fcntl, "flock", flock)
        _audit(tmp_path)
    assert acquired[:4] == [".submissions.lock", ".binding.lock", ".delivery.lock", ".atomic.lock"]
    delivery = (tmp_path / "queue" / ".delivery.lock").stat()
    binding_free = False

    def check_delivery(fd: int, operation: int) -> None:
        nonlocal binding_free
        info = os.fstat(fd)
        if (info.st_dev, info.st_ino) == (delivery.st_dev, delivery.st_ino):
            binding = original_open(str(tmp_path / "queue" / ".binding.lock"), "test binding lock")
            try:
                original_flock(binding, fcntl.LOCK_EX | fcntl.LOCK_NB)
                binding_free = True
            finally:
                os.close(binding)
        original_flock(fd, operation)

    monkeypatch.setattr(fcntl, "flock", check_delivery)
    drain(harness, bridge.config.target, str(tmp_path / "queue"), ready_timeout=0,
          max_artifact_bytes=512 << 10, atomic_policy=chat_module._chat_atomic_policy(tmp_path))
    assert binding_free


def test_submitted_reply_waits_for_global_slot_lock_before_recovery(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    lock = os.open(tmp_path / ".atomic.lock", os.O_RDWR | os.O_CLOEXEC)
    fcntl.flock(lock, fcntl.LOCK_EX)
    metadata = os.fstat(lock)
    identity = (metadata.st_dev, metadata.st_ino)
    slot = tmp_path / ".atomic" / "staged.json"
    slot.write_bytes(b"a live bounded writer")
    slot.chmod(0o600)
    attempted = threading.Event()
    failures: list[BaseException] = []
    original = fcntl.flock

    def flock(fd: int, operation: int) -> None:
        info = os.fstat(fd)
        if threading.current_thread().name == "reply-writer" and (info.st_dev, info.st_ino) == identity:
            attempted.set()
        original(fd, operation)

    def reply() -> None:
        try:
            submit_reply(tmp_path, str(record["key"]), "answer")
        except BaseException as exc:
            failures.append(exc)

    monkeypatch.setattr(fcntl, "flock", flock)
    worker = threading.Thread(target=reply, name="reply-writer")
    worker.start()
    try:
        assert attempted.wait(5)
        assert slot.read_bytes() == b"a live bounded writer"
        os.close(lock)
        lock = -1
        worker.join(timeout=5)
        assert not worker.is_alive() and not failures
        assert not slot.exists()
        assert _read(tmp_path / "submissions" / f"{record['key']}.json")["text"] == "answer"
    finally:
        if lock >= 0:
            os.close(lock)
        worker.join(timeout=5)


def test_future_chat_and_queue_writes_never_scan_or_create_random_temps(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    _audit(tmp_path)

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("steady atomic write scanned a directory or created a random temp")

    monkeypatch.setattr(os, "scandir", forbidden)
    monkeypatch.setattr(tempfile, "NamedTemporaryFile", forbidden)
    for index in range(20):
        _write(tmp_path / "observer.json", {"sequence": index})
    policy = chat_module._chat_atomic_policy(tmp_path)
    enqueue(str(tmp_path / "queue"), "bounded", message_id="queued", atomic_policy=policy)
    result = drain(harness, bridge.config.target, str(tmp_path / "queue"), ready_timeout=0,
                   atomic_policy=policy)
    assert result.delivered == ("queued",)
    _audit(tmp_path)  # the durable marker also prevents repeated migration walks


def test_fixed_slot_serialization_cap_refuses_before_creating_a_temp(tmp_path: Path) -> None:
    setup(tmp_path)
    policy = AtomicWritePolicy(str(tmp_path / ".atomic"), 32)
    with atomic_write_policy(policy), pytest.raises(AgentDeliveryError, match="max_artifact_bytes"):
        agent_module._atomic_json(str(tmp_path / "oversized.json"), {"text": "x" * 100})
    assert not (tmp_path / ".atomic" / "staged.json").exists()
    assert not (tmp_path / "oversized.json").exists()


def test_owner_restart_recovers_fixed_slot_without_repeating_migration_scan(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    _audit(tmp_path)
    slot = tmp_path / ".atomic" / "staged.json"
    slot.write_bytes(b"crash after first owner startup")
    slot.chmod(0o600)

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("owner restart repeated a completed migration walk")

    monkeypatch.setattr(os, "scandir", forbidden)
    _audit(tmp_path)
    assert not slot.exists()
    assert _read(tmp_path / ".atomic" / "audited-v1.json") == {"version": 1}


@pytest.mark.parametrize("kind", ["symlink", "mode"])
def test_unsafe_legacy_temp_refuses_migration_without_removing_it(tmp_path: Path, kind: str) -> None:
    setup(tmp_path)
    stale = tmp_path / "requests" / ".message.unsafe"
    if kind == "symlink":
        stale.symlink_to(tmp_path / "bridge.json")
    else:
        stale.write_bytes(b"nonprivate legacy temporary")
        stale.chmod(0o644)
    before = stale.lstat()
    with pytest.raises(ValueError, match="unsafe legacy atomic temporary retained") as failure:
        _audit(tmp_path)
    assert str(stale) in str(failure.value)
    assert stale.lstat().st_ino == before.st_ino
    assert not (tmp_path / ".atomic" / "audited-v1.json").exists()


@pytest.mark.parametrize("state_name", ["requests", "feedback", "deferred", "replies", "submissions", "queue"])
def test_root_artifacts_do_not_confuse_state_directory_name_with_artifact_class(
    tmp_path: Path, state_name: str,
) -> None:
    state = tmp_path / state_name
    setup(state)
    for name in ("bridge.json", "input.json", "output.json", "request-limit.json", "population-limit.json"):
        assert chat_module._chat_write_state(state / name) == state
    _write(state / "input.json", {"cursor": "next"})
    assert (state / ".atomic.lock").exists()
    assert not (tmp_path / ".atomic.lock").exists()


@pytest.mark.parametrize("condition", ["busy", "unconfirmed", "inflight", "malformed"])
def test_queue_failure_and_recovery_writes_share_the_fixed_slot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, condition: str,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    policy = chat_module._chat_atomic_policy(tmp_path)
    enqueue(str(tmp_path / "queue"), "bounded", message_id="queued", atomic_policy=policy)
    path = tmp_path / "queue" / "inbox" / "queued.json"
    if condition == "busy":
        harness.state = "working"
    elif condition == "unconfirmed":
        harness.fail_confirmation = True
    elif condition == "inflight":
        path.rename(tmp_path / "queue" / "inflight" / path.name)
    else:
        path.write_text("invalid JSON")

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError("queue policy escaped to random temporary creation")

    monkeypatch.setattr(tempfile, "NamedTemporaryFile", forbidden)
    result = drain(harness, bridge.config.target, str(tmp_path / "queue"), ready_timeout=0,
                   atomic_policy=policy)
    if condition == "busy":
        assert result.pending == ("queued",)
        assert _read(path)["delivery_state"] == "pending"
    else:
        assert result.quarantined == ("queued",)
        assert (tmp_path / "queue" / "failed" / "queued.json.error").exists()
    assert not (tmp_path / ".atomic" / "staged.json").exists()
