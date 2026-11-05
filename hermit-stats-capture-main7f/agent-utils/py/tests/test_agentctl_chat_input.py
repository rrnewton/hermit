"""Persistent command input framing, backpressure, wakeup, and process cleanup."""

from __future__ import annotations

import json
import os
import selectors
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Sequence
from pathlib import Path
from typing import IO, SupportsIndex, cast

import pytest

from agentctl.chat import (
    _CommandAnchorIdentity, _cleanup_unpinned_direct_child,
    _require_waitable_sigchld_children,
)
from agentctl.chat_input import EventCommandStream, InputStreamError


_REQUEST: dict[str, object] = {"action": "subscribe", "space": "spaces/test", "cursor": "opaque+/= cursor"}
_PREAMBLE = "import json,os,sys,time\nrequest=json.load(sys.stdin)\n"


def _stream(program: str, *, max_frame_bytes: int = 1024 * 1024) -> EventCommandStream:
    return EventCommandStream([sys.executable, "-u", "-c", _PREAMBLE + program], _REQUEST,
                              max_frame_bytes=max_frame_bytes)


def _gone(pid: int) -> bool:
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rpartition(") ")[2][:1]
    except (FileNotFoundError, ProcessLookupError):
        return True
    return state == "Z"


def _await_file(path: Path) -> None:
    deadline = time.monotonic() + 5
    while not path.exists() and time.monotonic() < deadline:
        time.sleep(0.005)
    assert path.exists()


def test_one_process_receives_exact_request_and_literal_arguments(tmp_path: Path) -> None:
    marker = tmp_path / "must-not-exist"
    literal = f"$(touch {marker}); spaces 'quotes'\nsecond line"
    program = _PREAMBLE + (
        "print(json.dumps({'type':'message','request':request,'arguments':sys.argv[1:],'pid':os.getpid()}))\n"
        "print(json.dumps({'type':'checkpoint','cursor':'next cursor','pid':os.getpid()}))\n"
        "print(json.dumps({'type':'heartbeat'}))\n"
        "print(json.dumps({'type':'gap','reason':'resume unavailable'}))\n"
        "time.sleep(60)\n"
    )
    stream = EventCommandStream([sys.executable, "-u", "-c", program, literal], _REQUEST)
    try:
        first = stream.wait(5)
        assert len(first) == 1
        assert first[0]["request"] == _REQUEST
        assert first[0]["arguments"] == [literal]
        second = stream.wait(5)
        assert second == [{"type": "checkpoint", "cursor": "next cursor", "pid": first[0]["pid"]}]
        assert stream.wait(5) == [{"type": "heartbeat"}]
        assert stream.wait(5) == [{"type": "gap", "reason": "resume unavailable"}]
        assert not marker.exists()
    finally:
        stream.close()
    assert _gone(int(str(first[0]["pid"])))


def test_timeout_preserves_a_split_frame_and_zero_timeout_reads_buffered_frames(tmp_path: Path) -> None:
    started, release = tmp_path / "partial-written", tmp_path / "release"
    program = (
        "from pathlib import Path\n"
        "os.write(1,b'{\"type\":\"message\",')\n"
        f"Path({str(started)!r}).touch()\n"
        f"while not Path({str(release)!r}).exists(): time.sleep(0.005)\n"
        "os.write(1,b'\"message\":{\"text\":\"complete\"}}\\n{\"type\":\"checkpoint\",\"cursor\":\"done\"}\\n')\n"
        "time.sleep(60)\n"
    )
    stream = _stream(program)
    try:
        _await_file(started)
        assert stream.wait(0.03) == []
        release.touch()
        assert stream.wait(5) == [{"type": "message", "message": {"text": "complete"}}]
        assert stream.wait(0) == [{"type": "checkpoint", "cursor": "done"}]
    finally:
        stream.close()


def test_many_tiny_buffered_frames_advance_a_head_without_front_deletion() -> None:
    count = 2048
    stream = _stream(f"os.write(1,b'{{}}\\n'*{count})\ntime.sleep(60)\n")

    class CountedBuffer(bytearray):
        deletions = 0

        def __delitem__(
            self,
            key: SupportsIndex | slice[
                SupportsIndex | None, SupportsIndex | None, SupportsIndex | None],
        ) -> None:
            self.deletions += 1
            super().__delitem__(key)

    try:
        assert stream.wait(5) == [{}]
        assert len(stream._buffer) - stream._buffer_start >= (count - 1) * 3
        tracked = CountedBuffer(stream._buffer)
        stream._buffer = tracked
        for _ in range(count - 1):
            assert stream.wait(0) == [{}]
        assert tracked.deletions == 0, "tiny frames must not shift the remaining buffer"
        assert stream._buffer_start == 0 and not stream._buffer
    finally:
        stream.close()


def test_wait_timeout_does_not_turn_into_an_eof_or_kill_the_command() -> None:
    stream = _stream("print(json.dumps({'pid':os.getpid()}))\ntime.sleep(60)\n")
    try:
        pid = int(str(stream.wait(5)[0]["pid"]))
        assert stream.wait(0) == []
        assert stream.wait(0.02) == []
        assert not _gone(pid)
    finally:
        stream.close()
    assert _gone(pid)


@pytest.mark.parametrize("exit_status", [0, 7])
def test_eof_is_explicit_after_complete_pending_frames(exit_status: int) -> None:
    stream = _stream(f"print('{{\"type\":\"heartbeat\"}}')\nsys.exit({exit_status})\n")
    try:
        assert stream.wait(5) == [{"type": "heartbeat"}]
        with pytest.raises(InputStreamError) as caught:
            stream.wait(5)
        assert caught.value.code == "stream_eof"
        with pytest.raises(InputStreamError, match="stream_closed"):
            stream.wait(0)
    finally:
        stream.close()


def test_eof_mid_frame_is_not_a_message_or_timeout() -> None:
    stream = _stream("os.write(1,b'{\"type\":\"message\"')\n")
    try:
        with pytest.raises(InputStreamError) as caught:
            stream.wait(5)
        assert caught.value.code == "stream_eof"
    finally:
        stream.close()


@pytest.mark.parametrize("payload", [
    b"not-json private-token\n", b"[]\n", b"null\n", b"\xff\n", b"\n",
    b'{"type":"heartbeat","type":"message"}\n', b'{"value":NaN}\n',
    b'{"value":Infinity}\n', b'{"value":"\\ud800"}\n',
])
def test_invalid_frames_close_the_process_without_echoing_provider_bytes(payload: bytes) -> None:
    stream = _stream(f"os.write(2,b'private-token\\n')\nos.write(1,{payload!r})\ntime.sleep(60)\n")
    try:
        with pytest.raises(InputStreamError) as caught:
            stream.wait(5)
        assert caught.value.code == "invalid_frame"
        assert "private-token" not in str(caught.value)
        assert stream._process is not None and stream._process.returncode is not None
    finally:
        stream.close()


@pytest.mark.parametrize("newline", [False, True])
def test_frame_limit_applies_before_json_parsing_even_without_a_newline(newline: bool) -> None:
    payload = b"x" * 129 + (b"\n" if newline else b"")
    stream = _stream(f"os.write(1,{payload!r})\ntime.sleep(60)\n", max_frame_bytes=128)
    try:
        with pytest.raises(InputStreamError) as caught:
            stream.wait(5)
        assert caught.value.code == "frame_limit_exceeded"
        assert stream._process is not None and stream._process.returncode is not None
    finally:
        stream.close()


def test_exact_byte_limit_and_crlf_are_supported() -> None:
    payload = json.dumps({"value": "x" * 115}, separators=(",", ":")).encode()
    assert len(payload) == 127
    stream = _stream(f"os.write(1,{(payload + bytes([13, 10]))!r})\ntime.sleep(60)\n", max_frame_bytes=128)
    try:
        assert stream.wait(5) == [{"value": "x" * 115}]
    finally:
        stream.close()


def test_stderr_larger_than_pipe_capacity_does_not_block_output_or_leak() -> None:
    program = (
        "chunk=b'private-token ' * 5000\n"
        "for _ in range(64): os.write(2,chunk)\n"
        "print('{\"type\":\"heartbeat\"}')\n"
        "time.sleep(60)\n"
    )
    stream = _stream(program)
    try:
        assert stream.wait(5) == [{"type": "heartbeat"}]
        assert len(stream._buffer) <= stream._max_frame + 1
    finally:
        stream.close()


def test_subscription_write_drains_early_output_and_stderr_while_input_is_blocked() -> None:
    request: dict[str, object] = {"action": "subscribe", "cursor": "x" * 196608}
    program = (
        "import json,os,sys,time\n"
        "print('{\"type\":\"heartbeat\"}',flush=True)\n"
        "for _ in range(32): os.write(2,b'private-token '*5000)\n"
        "request=json.load(sys.stdin)\n"
        "print(json.dumps({'type':'checkpoint','cursor_length':len(request['cursor'])}),flush=True)\n"
        "time.sleep(60)\n"
    )
    stream = EventCommandStream([sys.executable, "-u", "-c", program], request)
    try:
        assert stream.wait(5) == [{"type": "heartbeat"}]
        assert stream.wait(5) == [{"type": "checkpoint", "cursor_length": 196608}]
    finally:
        stream.close()


def test_continuous_stderr_obeys_wait_deadline() -> None:
    stream = _stream("print('{\"type\":\"heartbeat\"}')\nwhile True: os.write(2,b'x'*65536)\n")
    try:
        assert stream.wait(5) == [{"type": "heartbeat"}]
        start = time.monotonic()
        assert stream.wait(0.03) == []
        assert time.monotonic() - start < 1
    finally:
        stream.close()


def test_wake_interrupts_wait_without_discarding_subsequent_event(tmp_path: Path) -> None:
    release = tmp_path / "release"
    stream = _stream("from pathlib import Path\nprint('{\"type\":\"heartbeat\"}')\n"
                     f"while not Path({str(release)!r}).exists(): time.sleep(0.005)\n"
                     "print('{\"type\":\"checkpoint\",\"cursor\":\"after-wake\"}')\ntime.sleep(60)\n")
    entered = threading.Event()
    result: list[list[dict[str, object]]] = []

    def wait() -> None:
        entered.set()
        result.append(stream.wait(30))

    try:
        assert stream.wait(5) == [{"type": "heartbeat"}]
        worker = threading.Thread(target=wait)
        worker.start()
        assert entered.wait(1)
        stream.wake()
        worker.join(2)
        assert not worker.is_alive()
        assert result == [[]]
        release.touch()
        assert stream.wait(5) == [{"type": "checkpoint", "cursor": "after-wake"}]
    finally:
        stream.close()


def test_close_from_another_thread_interrupts_wait_and_is_idempotent() -> None:
    stream = _stream("print(json.dumps({'pid':os.getpid()}))\ntime.sleep(60)\n")
    pid = int(str(stream.wait(5)[0]["pid"]))
    entered = threading.Event()
    finished = threading.Event()
    errors: list[BaseException] = []

    def wait() -> None:
        entered.set()
        try:
            assert stream.wait(30) == []
        except InputStreamError as exc:
            if exc.code != "stream_closed":
                errors.append(exc)
        except BaseException as exc:
            errors.append(exc)
        finally:
            finished.set()

    worker = threading.Thread(target=wait)
    worker.start()
    assert entered.wait(1)
    stream.close()
    worker.join(2)
    assert finished.is_set() and not worker.is_alive()
    assert not errors
    assert _gone(pid)
    stream.close()
    stream.wake()


def test_close_allows_durable_unsubscribe_and_drains_shutdown_output(
    tmp_path: Path,
) -> None:
    marker = tmp_path / "unsubscribed"
    program = (
        "import signal\n"
        "def unsubscribe(signum, frame):\n"
        "    for descriptor in (1,2):\n"
        "        for _ in range(8): os.write(descriptor,b'private shutdown output '*4096)\n"
        f"    with open({str(marker)!r},'w') as target:\n"
        "        target.write('unsubscribed')\n"
        "        target.flush()\n"
        "        os.fsync(target.fileno())\n"
        "    sys.exit(0)\n"
        "signal.signal(signal.SIGTERM,unsubscribe)\n"
        "print(json.dumps({'pid':os.getpid()}))\n"
        "while True: time.sleep(60)\n"
    )
    stream = _stream(program, max_frame_bytes=128)
    try:
        pid = int(str(stream.wait(5)[0]["pid"]))
        stream.close()
        assert marker.read_text() == "unsubscribed"
        assert stream._returncode == 0
        assert _gone(pid)
    finally:
        stream.close()


def test_close_bounds_uncooperative_anchored_group(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    program = (
        "import signal\n"
        "signal.signal(signal.SIGTERM,signal.SIG_IGN)\n"
        "child=os.fork()\n"
        "if child: print(json.dumps({'parent':os.getpid(),'child':child}))\n"
        "while True: time.sleep(60)\n"
    )
    stream = _stream(program)
    try:
        identifiers = stream.wait(5)[0]
        parent, child = int(str(identifiers["parent"])), int(str(identifiers["child"]))
        assert stream._process is not None
        monkeypatch.setattr(
            os, "killpg",
            lambda pid, signum: pytest.fail(
                "outer bridge must not signal a numeric process group",
            ),
        )
        monkeypatch.setattr(
            stream._process, "poll",
            lambda: pytest.fail(
                "outer bridge cleanup must use exact readiness, not Popen.poll",
            ),
        )
        started = time.monotonic()
        stream.close()
        assert time.monotonic() - started < 3
        assert stream._returncode == -signal.SIGKILL
        deadline = time.monotonic() + 1
        while not _gone(child) and time.monotonic() < deadline:
            time.sleep(0.005)
        assert _gone(parent) and _gone(child)
        stream.close()
    finally:
        stream.close()


@pytest.mark.parametrize("parent_exits", [False, True])
def test_cleanup_kills_descendants_even_if_parent_exits_with_output_pipe_open(parent_exits: bool) -> None:
    program = (
        "import subprocess\n"
        "child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])\n"
        "print(json.dumps({'parent':os.getpid(),'child':child.pid}))\n"
        + ("sys.exit(7)\n" if parent_exits else "time.sleep(60)\n")
    )
    stream = _stream(program)
    try:
        process = stream.wait(5)[0]
        parent, child = int(str(process["parent"])), int(str(process["child"]))
        if parent_exits:
            started = time.monotonic()
            with pytest.raises(InputStreamError) as caught:
                stream.wait(5)
            assert caught.value.code == "stream_eof"
            assert "exit status 7" in str(caught.value)
            assert time.monotonic() - started < 2
        else:
            stream.close()
        assert _gone(parent)
        deadline = time.monotonic() + 2
        while not _gone(child) and time.monotonic() < deadline:
            time.sleep(0.005)
        assert _gone(child)
    finally:
        stream.close()


def test_subscription_write_is_bounded_if_command_never_reads_stdin(tmp_path: Path) -> None:
    pidfile = tmp_path / "pid"
    program = (
        "import os,time\nfrom pathlib import Path\n"
        f"Path({str(pidfile)!r}).write_text(str(os.getpid()))\ntime.sleep(60)\n"
    )
    start = time.monotonic()
    with pytest.raises(InputStreamError) as caught:
        EventCommandStream([sys.executable, "-u", "-c", program], {"cursor": "x" * 524288},
                           start_timeout=0.5)
    assert caught.value.code == "subscription_timeout"
    assert time.monotonic() - start < 3
    assert pidfile.exists()
    assert _gone(int(pidfile.read_text()))


def test_missing_command_has_explicit_redacted_failure(tmp_path: Path) -> None:
    command = tmp_path / "private-token-missing"
    with pytest.raises(InputStreamError) as caught:
        EventCommandStream([str(command)], _REQUEST)
    assert caught.value.code == "command_failed"
    assert "private-token" not in str(caught.value)


@pytest.mark.parametrize("timeout", [-1.0, float("inf"), float("nan")])
def test_invalid_wait_deadline_does_not_close_a_live_stream(timeout: float) -> None:
    stream = _stream("print('{\"type\":\"heartbeat\"}')\ntime.sleep(60)\n")
    try:
        with pytest.raises(ValueError, match="timeout"):
            stream.wait(timeout)
        assert stream.wait(5) == [{"type": "heartbeat"}]
    finally:
        stream.close()


@pytest.mark.parametrize("options", [
    {"max_frame_bytes": 0}, {"max_frame_bytes": True}, {"start_timeout": 0.0},
    {"start_timeout": float("nan")}, {"start_timeout": float("inf")},
])
def test_invalid_settings_fail_before_starting_a_command(
    options: dict[str, object], monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fail(*args: object, **kwargs: object) -> None:
        pytest.fail("invalid configuration must not launch a process")

    monkeypatch.setattr(subprocess, "Popen", fail)
    if "max_frame_bytes" in options:
        value = options["max_frame_bytes"]
        assert isinstance(value, int)
        with pytest.raises(ValueError):
            EventCommandStream(["unused"], _REQUEST, max_frame_bytes=value)
    else:
        timeout = options["start_timeout"]
        assert isinstance(timeout, float)
        with pytest.raises(ValueError):
            EventCommandStream(["unused"], _REQUEST, start_timeout=timeout)


def test_nonfinite_or_oversized_subscription_is_rejected_before_spawn() -> None:
    with pytest.raises(ValueError, match="valid JSON"):
        EventCommandStream(["must-not-run"], {"value": float("nan")})
    with pytest.raises(ValueError, match="byte limit"):
        EventCommandStream(["must-not-run"], {"value": "x" * 200}, max_frame_bytes=128)


def test_missing_process_descriptors_refuse_before_spawning(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def unavailable(pid: int, flags: int = 0) -> int:
        raise OSError("unsupported")

    def spawn(*args: object, **kwargs: object) -> None:
        pytest.fail("pidfd preflight must precede event command spawn")

    monkeypatch.setattr(os, "pidfd_open", unavailable)
    monkeypatch.setattr(subprocess, "Popen", spawn)
    with pytest.raises(InputStreamError) as caught:
        _stream("time.sleep(60)\n")
    assert caught.value.code == "process_descriptor_unavailable"


def test_process_descriptor_race_after_spawn_kills_and_reaps_without_waitid_polling(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    original_open = os.pidfd_open
    original_popen = subprocess.Popen
    spawned: list[subprocess.Popen[bytes]] = []

    def open_descriptor(pid: int, flags: int = 0) -> int:
        if pid == os.getpid():
            return original_open(pid, flags)
        raise OSError("descriptor exhausted after spawn")

    def popen(
        args: Sequence[str], *, stdin: int, stdout: int, stderr: int,
        start_new_session: bool, bufsize: int, pass_fds: Sequence[int],
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, bufsize=bufsize, pass_fds=pass_fds,
        )
        spawned.append(process)
        return process

    monkeypatch.setattr(os, "pidfd_open", open_descriptor)
    monkeypatch.setattr(os, "waitid", lambda *args, **kwargs: pytest.fail("waitid polling fallback"))
    monkeypatch.setattr(subprocess, "Popen", popen)
    with pytest.raises(InputStreamError) as caught:
        _stream(f"open({str(tmp_path / 'provider-started')!r},'w').close()\ntime.sleep(60)\n")
    assert caught.value.code == "process_descriptor_unavailable"
    assert len(spawned) == 1 and spawned[0].returncode is not None
    assert not (tmp_path / "provider-started").exists()


def test_repeated_double_pidfd_exhaustion_reaps_stopped_event_supervisors(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    _require_waitable_sigchld_children("event cleanup regression")
    original_open = os.pidfd_open
    original_pipe = os.pipe2
    original_popen = subprocess.Popen
    original_kill = os.kill
    before_fds = len(list(Path("/proc/self/fd").iterdir()))
    before_tasks = len(list(Path("/proc/self/task").iterdir()))
    marker = tmp_path / "provider-must-not-start"
    processes: list[subprocess.Popen[bytes]] = []
    anchors: list[int] = []
    pipe_reads: dict[int, int] = {}
    pidfd_calls: list[int] = []
    signals: list[tuple[int, int]] = []

    def record_pipe(flags: int) -> tuple[int, int]:
        read_descriptor, write_descriptor = original_pipe(flags)
        pipe_reads[write_descriptor] = read_descriptor
        return read_descriptor, write_descriptor

    def exhaust_twice(pid: int, flags: int = 0) -> int:
        if pid == os.getpid():
            return original_open(pid, flags)
        pidfd_calls.append(pid)
        message = (
            "original event pidfd exhaustion"
            if len(pidfd_calls) % 2 else "cleanup retry event pidfd exhaustion"
        )
        raise OSError(24, message)

    def record_signal(pid: int, signum: int) -> None:
        signals.append((pid, signum))
        original_kill(pid, signum)

    def stopped_popen(
        args: Sequence[str], *, stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
        start_new_session: bool = False, bufsize: int = -1,
        pass_fds: Sequence[int] = (),
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, bufsize=bufsize, pass_fds=pass_fds,
        )
        processes.append(process)
        identity_read = pipe_reads[int(args[3])]
        selector = selectors.DefaultSelector()
        try:
            selector.register(identity_read, selectors.EVENT_READ)
            if not selector.select(2):
                pytest.fail("event supervisor did not publish its pre-ACK anchor")
            publication = json.loads(os.read(identity_read, 4096).splitlines()[0])
        finally:
            selector.close()
        assert isinstance(publication, dict) and isinstance(publication.get("pid"), int)
        anchors.append(int(publication["pid"]))
        os.kill(process.pid, signal.SIGSTOP)
        deadline = time.monotonic() + 2
        while True:
            state = Path(f"/proc/{process.pid}/stat").read_text().rpartition(") ")[2][:1]
            if state in ("T", "t"):
                return process
            if time.monotonic() >= deadline:
                pytest.fail("event supervisor did not stop before pidfd exhaustion")
            time.sleep(0.005)

    monkeypatch.setattr(os, "pidfd_open", exhaust_twice)
    monkeypatch.setattr(os, "pipe2", record_pipe)
    monkeypatch.setattr(os, "kill", record_signal)
    monkeypatch.setattr(subprocess, "Popen", stopped_popen)
    for _ in range(10):
        with pytest.raises(InputStreamError) as caught:
            _stream(
                f"open({str(marker)!r},'w').close()\ntime.sleep(60)\n",
            )
        assert caught.value.code == "process_descriptor_unavailable"
        assert isinstance(caught.value.__cause__, OSError)
        assert "original event pidfd exhaustion" in str(caught.value.__cause__)
    assert pidfd_calls == [pid for process in processes for pid in (process.pid, process.pid)]
    assert signals == [
        item for process in processes for item in (
            (process.pid, signal.SIGSTOP),
            (process.pid, signal.SIGCONT),
            (process.pid, signal.SIGKILL),
        )
    ]
    assert all(process.returncode == -signal.SIGKILL for process in processes)
    assert all(not Path(f"/proc/{process.pid}").exists() for process in processes)
    assert len(anchors) == len(processes)
    deadline = time.monotonic() + 2
    while any(Path(f"/proc/{pid}").exists() for pid in anchors) and time.monotonic() < deadline:
        time.sleep(0.01)
    assert all(not Path(f"/proc/{pid}").exists() for pid in anchors)
    assert not marker.exists()
    assert len(list(Path("/proc/self/fd").iterdir())) == before_fds
    assert len(list(Path("/proc/self/task").iterdir())) == before_tasks


def test_event_preauthority_cleanup_fault_is_not_excused(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_open = os.pidfd_open
    original_cleanup = _cleanup_unpinned_direct_child

    def exhaust(pid: int, flags: int = 0) -> int:
        if pid == os.getpid():
            return original_open(pid, flags)
        raise OSError(24, "original event pidfd exhaustion")

    def cleanup_then_fail(
        process: subprocess.Popen[bytes], expected: _CommandAnchorIdentity,
        context: str, *, timeout: float = 5.0,
    ) -> None:
        original_cleanup(process, expected, context, timeout=timeout)
        raise RuntimeError("synthetic preauthority cleanup proof failure")

    monkeypatch.setattr(os, "pidfd_open", exhaust)
    monkeypatch.setattr("agentctl.chat_input._cleanup_unpinned_direct_child", cleanup_then_fail)
    with pytest.raises(RuntimeError, match="cleanup proof failure") as caught:
        _stream("time.sleep(60)\n")
    assert isinstance(caught.value.__cause__, InputStreamError)


@pytest.mark.parametrize("failure", ["supervisor_pidfd", "anchor_pidfd", "registration"])
def test_pre_ack_failure_never_starts_provider_or_leaks(
    failure: str, monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    marker = tmp_path / "provider-started"
    original_open = os.pidfd_open
    original_popen = subprocess.Popen
    original_selector = selectors.DefaultSelector
    nonself_opens = 0
    spawned: list[subprocess.Popen[bytes]] = []
    before_fds = len(list(Path("/proc/self/fd").iterdir()))

    def open_descriptor(pid: int, flags: int = 0) -> int:
        nonlocal nonself_opens
        if pid == os.getpid():
            return original_open(pid, flags)
        nonself_opens += 1
        if (failure == "supervisor_pidfd" and nonself_opens == 1) or (
            failure == "anchor_pidfd" and nonself_opens == 2
        ):
            raise OSError("synthetic pidfd exhaustion")
        return original_open(pid, flags)

    def popen(
        args: Sequence[str], *, stdin: int, stdout: int, stderr: int,
        start_new_session: bool, bufsize: int, pass_fds: Sequence[int],
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, bufsize=bufsize, pass_fds=pass_fds,
        )
        spawned.append(process)
        return process

    selector_instances = 0

    class RegistrationFailure:
        def __init__(self) -> None:
            nonlocal selector_instances
            selector_instances += 1
            self.number = selector_instances
            self.inner = cast(selectors.BaseSelector, original_selector())

        def register(
            self, fileobj: int, events: int, data: object = None,
        ) -> object:
            if self.number == 2:
                self.number = -1
                raise OSError("synthetic selector registration failure")
            return self.inner.register(fileobj, events, data)

        def select(
            self, timeout: float | None = None,
        ) -> list[tuple[object, int]]:
            return cast(list[tuple[object, int]], self.inner.select(timeout))

        def close(self) -> None:
            self.inner.close()

    monkeypatch.setattr(os, "pidfd_open", open_descriptor)
    monkeypatch.setattr(subprocess, "Popen", popen)
    if failure == "registration":
        monkeypatch.setattr(selectors, "DefaultSelector", RegistrationFailure)
    with pytest.raises(InputStreamError) as caught:
        _stream(f"open({str(marker)!r},'w').close()\ntime.sleep(60)\n")
    assert caught.value.code == (
        "command_failed" if failure == "registration" else "process_descriptor_unavailable"
    )
    assert len(spawned) == 1 and spawned[0].returncode is not None
    assert not marker.exists()
    assert len(list(Path("/proc/self/fd").iterdir())) == before_fds


@pytest.mark.parametrize("disposition", ["ignored", "no-cldwait"])
def test_event_refuses_autoreap_before_stopped_double_pidfd_race(
    tmp_path: Path, disposition: str,
) -> None:
    python_path = str(Path(__file__).resolve().parents[1])
    setup = (
        "signal.signal(signal.SIGCHLD,signal.SIG_IGN)"
        if disposition == "ignored"
        else """
class SigSet(ctypes.Structure):
    _fields_=[('values',ctypes.c_ulong*16)]
class SigAction(ctypes.Structure):
    _fields_=[('handler',ctypes.c_void_p),('mask',SigSet),('flags',ctypes.c_int),('restorer',ctypes.c_void_p)]
action=SigAction()
action.handler=ctypes.c_void_p(0)
libc=ctypes.CDLL(None,use_errno=True)
assert libc.sigemptyset(ctypes.byref(action.mask)) == 0
action.flags=2
assert libc.sigaction(signal.SIGCHLD,ctypes.byref(action),None) == 0
"""
    )
    marker = tmp_path / f"event-autoreap-{disposition}"
    script = f"""
import ctypes,os,pathlib,signal,subprocess,sys,time
{setup}
import agentctl.chat_input as chat_input
original_open=os.pidfd_open
original_popen=subprocess.Popen
spawned=[]
handles=[]
nonself_opens=[]
def stopped_popen(*args,**kwargs):
    process=original_popen(*args,**kwargs)
    descriptor=original_open(process.pid)
    spawned.append(process)
    handles.append(descriptor)
    signal.pidfd_send_signal(descriptor,signal.SIGSTOP)
    deadline=time.monotonic()+2
    while pathlib.Path(f'/proc/{{process.pid}}/stat').read_text().rpartition(') ')[2][:1] not in ('T','t'):
        if time.monotonic() >= deadline:
            raise AssertionError('supervisor did not stop')
        time.sleep(.005)
    return process
def exhaust_after_spawn(pid,flags=0):
    if pid == os.getpid():
        return original_open(pid,flags)
    nonself_opens.append(pid)
    raise OSError(24,'combined stopped-supervisor descriptor exhaustion')
chat_input.subprocess.Popen=stopped_popen
chat_input.os.pidfd_open=exhaust_after_spawn
marker=pathlib.Path({str(marker)!r})
provider="import json,pathlib,sys,time;json.load(sys.stdin);pathlib.Path(sys.argv[1]).touch();time.sleep(60)"
try:
    try:
        chat_input.EventCommandStream(
            [sys.executable,'-u','-c',provider,str(marker)],{{'action':'subscribe'}},
        )
    except chat_input.InputStreamError as exc:
        assert exc.code=='process_status_unavailable',exc.code
        assert 'SIGCHLD=SIG_DFL' in str(exc),str(exc)
    else:
        raise AssertionError('unsafe SIGCHLD state was accepted')
    assert not spawned,spawned
    assert not nonself_opens,nonself_opens
    assert not marker.exists()
finally:
    chat_input.os.pidfd_open=original_open
    chat_input.subprocess.Popen=original_popen
    for process,descriptor in zip(spawned,handles):
        try:
            signal.pidfd_send_signal(descriptor,signal.SIGCONT)
            signal.pidfd_send_signal(descriptor,signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=2)
        except (ChildProcessError,subprocess.TimeoutExpired):
            pass
        os.close(descriptor)
print('pre-spawn-refusal-ok')
"""
    environment = dict(os.environ)
    environment["PYTHONPATH"] = python_path + os.pathsep + environment.get("PYTHONPATH", "")
    result = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True,
        env=environment, timeout=15, check=False,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "pre-spawn-refusal-ok"
    assert not marker.exists()


def test_stopped_supervisor_and_anchor_are_woken_for_exact_cleanup() -> None:
    stream = _stream(
        "child=os.fork()\n"
        "if child: print(json.dumps({'parent':os.getpid(),'child':child}))\n"
        "while True: time.sleep(60)\n",
    )
    try:
        identities = stream.wait(5)[0]
        provider = int(str(identities["parent"]))
        child = int(str(identities["child"]))
        assert stream._process is not None and stream._anchor is not None
        supervisor = stream._process.pid
        anchor = stream._anchor.pid
        os.kill(supervisor, signal.SIGSTOP)
        os.kill(anchor, signal.SIGSTOP)
        stream.close()
        assert stream._returncode in (-signal.SIGTERM, -signal.SIGKILL)
        assert all(_gone(pid) for pid in (provider, child, supervisor, anchor))
    finally:
        stream.close()


def test_owner_sigkill_closes_lifeline_and_removes_provider_group() -> None:
    python_path = str(Path(__file__).resolve().parents[1])
    provider = (
        "import json,os,sys,time;json.load(sys.stdin);child=os.fork();"
        "print(json.dumps({'type':'heartbeat','provider':os.getpid(),'child':child}),flush=True) "
        "if child else None;time.sleep(60)"
    )
    owner = f"""
import json,os,sys,time
from agentctl.chat_input import EventCommandStream
stream=EventCommandStream([sys.executable,'-u','-c',{provider!r}],{{'action':'subscribe'}})
event=stream.wait(5)[0]
print(json.dumps({{'owner':os.getpid(),'supervisor':stream._process.pid,'anchor':stream._anchor.pid,
                  'provider':event['provider'],'child':event['child']}}),flush=True)
time.sleep(60)
"""
    environment = dict(os.environ)
    environment["PYTHONPATH"] = python_path + os.pathsep + environment.get("PYTHONPATH", "")
    process = subprocess.Popen(
        [sys.executable, "-u", "-c", owner], stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, env=environment,
    )
    try:
        assert process.stdout is not None
        selector = selectors.DefaultSelector()
        try:
            selector.register(process.stdout, selectors.EVENT_READ)
            assert selector.select(10), "event stream owner did not publish its identities"
        finally:
            selector.close()
        identities = json.loads(process.stdout.readline())
        os.kill(process.pid, signal.SIGKILL)
        process.wait(timeout=5)
        deadline = time.monotonic() + 5
        watched = tuple(int(identities[key]) for key in ("supervisor", "anchor", "provider", "child"))
        while not all(_gone(pid) for pid in watched) and time.monotonic() < deadline:
            time.sleep(0.01)
        assert all(_gone(pid) for pid in watched)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        assert process.stdout is not None and process.stderr is not None
        process.stdout.close()
        process.stderr.close()


def test_repeated_supervised_streams_leave_no_descriptors_or_processes() -> None:
    before = len(list(Path("/proc/self/fd").iterdir()))
    identifiers: list[int] = []
    for _ in range(10):
        stream = _stream(
            "print(json.dumps({'type':'heartbeat','pid':os.getpid()}))\ntime.sleep(60)\n",
        )
        identifiers.append(int(str(stream.wait(5)[0]["pid"])))
        stream.close()
    assert all(_gone(pid) for pid in identifiers)
    assert len(list(Path("/proc/self/fd").iterdir())) == before
