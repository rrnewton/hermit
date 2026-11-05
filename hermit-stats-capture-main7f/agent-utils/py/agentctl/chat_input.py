"""Read bounded JSON event frames from one persistent command process."""

from __future__ import annotations

import json
import math
import os
import selectors
import signal
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Sequence
from pathlib import Path
from typing import IO, cast

from agentctl.chat import (
    _CommandAnchorIdentity,
    _cleanup_unpinned_direct_child,
    _command_identities_gone,
    _command_process_identity,
    _read_command_publication,
    _read_command_result,
    _require_waitable_sigchld_children,
    _same_command_process,
    _validated_supervisor_identity,
    _write_command_control,
)
from agentctl.errors import HerdrUnavailable
from agentctl.jsonx import as_mapping


_CLOSE_GRACE_SECONDS = 2.0
_CONTROL_TIMEOUT_SECONDS = 5.0


def _wait_fds(
    readers: Sequence[int], writers: Sequence[int], timeout: float | None,
) -> tuple[list[int], list[int]]:
    """Wait on arbitrary descriptor values without the select(2) FD ceiling."""
    selector = selectors.DefaultSelector()
    masks: dict[int, int] = {}
    for descriptor in readers:
        masks[descriptor] = masks.get(descriptor, 0) | selectors.EVENT_READ
    for descriptor in writers:
        masks[descriptor] = masks.get(descriptor, 0) | selectors.EVENT_WRITE
    try:
        for descriptor, mask in masks.items():
            selector.register(descriptor, mask)
        events = selector.select(timeout)
        readable = [key.fd for key, mask in events if mask & selectors.EVENT_READ]
        writable = [key.fd for key, mask in events if mask & selectors.EVENT_WRITE]
        return readable, writable
    finally:
        selector.close()


def _descriptor_ready(descriptor: int, timeout: float) -> bool:
    readable, _ = _wait_fds((descriptor,), (), timeout)
    return descriptor in readable


class InputStreamError(HerdrUnavailable):
    """An explicit command or framing failure, distinct from an ordinary timeout."""

    def __init__(self, code: str, detail: str) -> None:
        self.code = code
        self.detail = detail
        super().__init__(f"{code}: {detail}")


def _object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON object key")
        result[key] = value
    return result


def _nonfinite(value: str) -> object:
    raise ValueError("nonfinite JSON value")


class EventCommandStream:
    """Send one JSON request and read newline-delimited JSON event objects.

    The command runs with literal arguments in its own process group. Its stdin
    receives the request followed by a newline and then EOF. Output is bounded
    by max_frame_bytes per frame; the subscription write must finish within
    start_timeout seconds. Provider message and cursor semantics belong to
    the caller, which must persist accepted input before advancing its cursor.

    The wait method returns at most one object. No background queue accumulates
    events: pipe backpressure bounds producers while the caller processes a
    result. Stderr is drained separately and discarded, so diagnostics cannot
    block the event pipe or expose credentials through exception messages.

    One thread owns wait. Other threads may call wake or close.
    Closing or a protocol failure gives the command up to two seconds after
    SIGTERM to unsubscribe, then kills and reaps its remaining process group.
    Partial frames survive ordinary timeouts; EOF is always an explicit error.
    """

    def __init__(
        self,
        command: Sequence[str],
        request: dict[str, object],
        *,
        max_frame_bytes: int = 1024 * 1024,
        start_timeout: float = 5.0,
    ) -> None:
        if (isinstance(command, (str, bytes)) or not command
                or any(not isinstance(argument, str) or "\0" in argument for argument in command)
                or not command[0]):
            raise ValueError("event command must be a nonempty argument sequence without NUL bytes")
        if isinstance(max_frame_bytes, bool) or not isinstance(max_frame_bytes, int) or max_frame_bytes <= 0:
            raise ValueError("event frame byte limit must be a positive integer")
        if not math.isfinite(start_timeout) or start_timeout <= 0:
            raise ValueError("event command startup timeout must be finite and positive")
        try:
            payload = (json.dumps(as_mapping(request, "subscription request"),
                                  ensure_ascii=False, allow_nan=False) + "\n").encode("utf-8")
        except (TypeError, ValueError, UnicodeError, RecursionError) as exc:
            raise ValueError("subscription request must be a valid JSON object") from exc
        if len(payload) - 1 > max_frame_bytes:
            raise ValueError("subscription request exceeds the event frame byte limit")
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise InputStreamError(
                "process_descriptor_unavailable",
                "event command supervision requires Linux process descriptors",
            )
        try:
            _require_waitable_sigchld_children("event command containment")
        except (OSError, ValueError) as exc:
            raise InputStreamError(
                "process_status_unavailable",
                "event command supervision requires SIGCHLD=SIG_DFL with SA_NOCLDWAIT clear",
            ) from exc
        try:
            probe_pidfd = os.pidfd_open(os.getpid())
        except OSError as exc:
            raise InputStreamError(
                "process_descriptor_unavailable",
                "event command supervision requires Linux process descriptors",
            ) from exc
        os.close(probe_pidfd)
        self._max_frame = max_frame_bytes
        self._buffer = bytearray()
        self._buffer_start = 0
        self._process: subprocess.Popen[bytes] | None = None
        self._stdin: IO[bytes] | None = None
        self._stdout: IO[bytes] | None = None
        self._stderr: IO[bytes] | None = None
        self._pidfd: int | None = None
        self._anchor_pidfd: int | None = None
        self._anchor: _CommandAnchorIdentity | None = None
        self._supervisor_identity: _CommandAnchorIdentity | None = None
        self._identity_read: int | None = None
        self._acknowledge_write: int | None = None
        self._lifeline_write: int | None = None
        self._result_read: int | None = None
        self._provider_ready_read: int | None = None
        self._returncode: int | None = None
        self._authority_granted = False
        self._exited = False
        self._closed = threading.Event()
        self._released = False
        self._io_lock = threading.RLock()
        self._wake_read, self._wake_write = socket.socketpair()
        self._wake_read.setblocking(False)
        self._wake_write.setblocking(False)
        try:
            deadline = time.monotonic() + start_timeout
            self._start_supervisor(command, deadline)
            pipes = (
                cast(IO[bytes], self._stdin),
                cast(IO[bytes], self._stdout),
                cast(IO[bytes], self._stderr),
            )
            for pipe in pipes:
                os.set_blocking(pipe.fileno(), False)
            self._send(payload, deadline)
        except BaseException as exc:
            try:
                self.close()
            except BaseException as cleanup_exc:
                raise cleanup_exc from exc
            if isinstance(exc, OSError):
                raise InputStreamError("command_failed", "could not start or subscribe to the event command") from exc
            raise

    def _start_supervisor(self, command: Sequence[str], deadline: float) -> None:
        controls: list[int] = []

        def control_pipe() -> tuple[int, int]:
            pair = os.pipe2(os.O_CLOEXEC)
            controls.extend(pair)
            return pair

        supervisor = Path(__file__).with_name("_event_command_supervisor.py")
        try:
            identity_read, identity_write = control_pipe()
            acknowledge_read, acknowledge_write = control_pipe()
            lifeline_read, lifeline_write = control_pipe()
            result_read, result_write = control_pipe()
            provider_ready_read, provider_ready_write = control_pipe()
            process = subprocess.Popen(
                (
                    sys.executable,
                    str(supervisor),
                    str(os.getpid()),
                    str(identity_write),
                    str(acknowledge_read),
                    str(lifeline_read),
                    str(result_write),
                    str(provider_ready_write),
                    *command,
                ),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
                bufsize=0,
                pass_fds=(
                    identity_write,
                    acknowledge_read,
                    lifeline_read,
                    result_write,
                    provider_ready_write,
                ),
            )
        except BaseException:
            for descriptor in controls:
                os.close(descriptor)
            raise

        self._process = process
        self._stdin, self._stdout, self._stderr = process.stdin, process.stdout, process.stderr
        for descriptor in (
            identity_write, acknowledge_read, lifeline_read, result_write, provider_ready_write,
        ):
            os.close(descriptor)
            controls.remove(descriptor)
        self._identity_read = identity_read
        self._acknowledge_write = acknowledge_write
        self._lifeline_write = lifeline_write
        self._result_read = result_read
        self._provider_ready_read = provider_ready_read
        try:
            try:
                expected_supervisor = _validated_supervisor_identity(
                    process, "event command supervisor",
                )
            except ValueError as exc:
                raise InputStreamError(
                    "containment_failed",
                    "event command supervisor vanished before descriptor acquisition",
                ) from exc
            self._supervisor_identity = expected_supervisor
            try:
                self._pidfd = os.pidfd_open(process.pid)
            except OSError as original_pidfd_error:
                os.close(identity_read)
                self._identity_read = None
                try:
                    self._pidfd = os.pidfd_open(process.pid)
                except OSError:
                    self._pidfd = None
                raise InputStreamError(
                    "process_descriptor_unavailable",
                    "event command supervision could not pin the trusted supervisor",
                ) from original_pidfd_error
            observed = _command_process_identity(process.pid)
            if (
                not _same_command_process(expected_supervisor, observed)
                or observed is None
                or observed.ppid != os.getpid()
                or observed.pgrp != process.pid
                or observed.session != process.pid
                or observed.state in ("X", "Z")
            ):
                os.close(self._pidfd)
                self._pidfd = None
                raise InputStreamError(
                    "containment_failed",
                    "event command supervisor identity changed before activation",
                )
            self._supervisor_identity = observed
            self._anchor = _read_command_publication(identity_read, process.pid, deadline)
            try:
                self._anchor_pidfd = os.pidfd_open(self._anchor.pid)
            except OSError as exc:
                raise InputStreamError(
                    "process_descriptor_unavailable",
                    "event command supervision could not pin the trusted anchor",
                ) from exc
            current_anchor = _command_process_identity(self._anchor.pid)
            if (
                not _same_command_process(self._anchor, current_anchor)
                or current_anchor is None
                or current_anchor.ppid != process.pid
                or current_anchor.pgrp != self._anchor.pid
                or current_anchor.session != process.pid
                or current_anchor.state in ("X", "Z")
            ):
                os.close(self._anchor_pidfd)
                self._anchor_pidfd = None
                raise InputStreamError(
                    "containment_failed",
                    "event command anchor identity changed before activation",
                )

            # Registration is part of the pre-authority proof. A descriptor
            # backend failure must not start the operator-selected provider.
            probe = selectors.DefaultSelector()
            try:
                probe.register(self._pidfd, selectors.EVENT_READ)
                probe.register(self._anchor_pidfd, selectors.EVENT_READ)
                assert self._stdin is not None and self._stdout is not None and self._stderr is not None
                probe.register(self._stdin, selectors.EVENT_WRITE)
                probe.register(self._stdout, selectors.EVENT_READ)
                probe.register(self._stderr, selectors.EVENT_READ)
                probe.register(self._wake_read, selectors.EVENT_READ)
            finally:
                probe.close()
            _write_command_control(acknowledge_write, b"1", deadline)
            self._authority_granted = True
            os.close(acknowledge_write)
            self._acknowledge_write = None
            os.close(identity_read)
            self._identity_read = None
            self._wait_provider_ready(deadline)
        except BaseException:
            raise

    def _wait_provider_ready(self, deadline: float) -> None:
        descriptor = self._provider_ready_read
        assert descriptor is not None
        os.set_blocking(descriptor, False)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise InputStreamError(
                    "command_failed", "event command did not start before its deadline",
                )
            readable, _ = _wait_fds((descriptor,), (), remaining)
            if descriptor not in readable:
                continue
            try:
                value = os.read(descriptor, 2)
            except BlockingIOError:
                continue
            if value == b"1":
                os.close(descriptor)
                self._provider_ready_read = None
                return
            raise InputStreamError(
                "command_failed", "could not start the configured event command",
            )

    def _drain_wake(self) -> None:
        try:
            self._wake_read.recv(65536)
        except BlockingIOError:
            pass

    def _drain_stderr(self) -> None:
        pipe = self._stderr
        if pipe is None:
            return
        try:
            chunk = os.read(pipe.fileno(), 65536)
        except BlockingIOError:
            return
        if not chunk:
            pipe.close()
            self._stderr = None

    def _read_stdout(self) -> None:
        pipe = self._stdout
        if pipe is None:
            raise InputStreamError("stream_closed", "the event command output is closed")
        # Consumed frames retain a head offset so a burst of tiny frames does
        # not shift the remaining bytearray once per wait call. Compact once
        # before the next read, when those bytes would otherwise consume the
        # per-frame capacity.
        if self._buffer_start:
            del self._buffer[:self._buffer_start]
            self._buffer_start = 0
        capacity = self._max_frame + 1 - len(self._buffer)
        if capacity <= 0:
            raise InputStreamError("frame_limit_exceeded", "event frame exceeds the byte limit")
        try:
            chunk = os.read(pipe.fileno(), min(65536, capacity))
        except BlockingIOError:
            return
        if not chunk:
            detail = "the event command closed its output"
            if len(self._buffer) > self._buffer_start:
                detail += " in the middle of a frame"
            raise InputStreamError("stream_eof", detail)
        self._buffer.extend(chunk)

    def _readers(self, *, include_stdout: bool = True) -> list[int]:
        readers = [self._wake_read.fileno()]
        if include_stdout and self._stdout is not None:
            readers.append(self._stdout.fileno())
        if self._stderr is not None:
            readers.append(self._stderr.fileno())
        if self._pidfd is not None and not self._exited:
            readers.append(self._pidfd)
        return readers

    def _read_ready(self, readable: list[int]) -> bool:
        if self._wake_read.fileno() in readable:
            self._drain_wake()
            return False
        if self._stderr is not None and self._stderr.fileno() in readable:
            self._drain_stderr()
        if self._stdout is not None and self._stdout.fileno() in readable:
            self._read_stdout()
        if self._pidfd is not None and self._pidfd in readable:
            self._exited = True
        return True

    def _child_exited(self) -> bool:
        return self._exited

    def _send(self, payload: bytes, deadline: float) -> None:
        pipe = self._stdin
        assert pipe is not None
        pending = memoryview(payload)
        while pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise InputStreamError("subscription_timeout", "event command did not accept its subscription request")
            readers = self._readers(
                include_stdout=self._buffer.find(b"\n", self._buffer_start) < 0)
            try:
                readable, writable = _wait_fds(readers, (pipe.fileno(),), remaining)
            except InterruptedError:
                continue
            if not self._read_ready(readable):
                raise InputStreamError("stream_closed", "event command subscription was interrupted")
            if writable:
                try:
                    sent = os.write(pipe.fileno(), pending)
                except BlockingIOError:
                    continue
                pending = pending[sent:]
        pipe.close()
        self._stdin = None

    def _next_frame(self, deadline: float) -> bytes | None:
        polled = False
        while True:
            newline = self._buffer.find(b"\n", self._buffer_start)
            if newline >= 0:
                if newline - self._buffer_start > self._max_frame:
                    raise InputStreamError("frame_limit_exceeded", "event frame exceeds the byte limit")
                frame = bytes(self._buffer[self._buffer_start:newline])
                self._buffer_start = newline + 1
                if self._buffer_start == len(self._buffer):
                    self._buffer.clear()
                    self._buffer_start = 0
                elif (self._buffer_start >= 65536
                      and self._buffer_start * 2 >= len(self._buffer)):
                    # Geometric compaction makes total copied bytes linear in
                    # the input size even for one-byte frames.
                    del self._buffer[:self._buffer_start]
                    self._buffer_start = 0
                return frame
            if len(self._buffer) - self._buffer_start > self._max_frame:
                raise InputStreamError("frame_limit_exceeded", "event frame exceeds the byte limit")
            if self._child_exited():
                # Deliver complete output already written by the command, but
                # do not mistake a descendant holding its pipe open for a live
                # subscription after the owning command has exited.
                assert self._stdout is not None
                readable, _ = _wait_fds((self._stdout.fileno(),), (), 0)
                if readable:
                    self._read_stdout()
                    continue
                detail = "the event command exited"
                if len(self._buffer) > self._buffer_start:
                    detail += " in the middle of a frame"
                raise InputStreamError("stream_eof", detail)
            remaining = max(0.0, deadline - time.monotonic())
            if polled and remaining == 0:
                return None
            try:
                readable, _ = _wait_fds(self._readers(), (), remaining)
            except InterruptedError:
                continue
            polled = True
            if not readable:
                return None
            if not self._read_ready(readable):
                return None

    @staticmethod
    def _decode(frame: bytes) -> dict[str, object]:
        try:
            document = as_mapping(
                json.loads(frame.decode("utf-8"), object_pairs_hook=_object_pairs, parse_constant=_nonfinite),
                "input event",
            )
            # Escaped lone surrogates can pass the JSON decoder but cannot be
            # persisted or forwarded as UTF-8 source messages.
            json.dumps(document, ensure_ascii=False, allow_nan=False).encode("utf-8")
            return document
        except (UnicodeError, ValueError, TypeError, RecursionError) as exc:
            raise InputStreamError("invalid_frame", "event command emitted an invalid UTF-8 JSON object") from exc

    def wait(self, timeout: float) -> list[dict[str, object]]:
        """Wait up to timeout seconds for one event, or return an empty list.

        Timeout and explicit wakeup preserve buffered input. EOF and malformed
        frames close the stream and raise InputStreamError. Reconnect by
        creating a new stream with the caller's last durably accepted cursor.
        """
        if not math.isfinite(timeout) or timeout < 0:
            raise ValueError("event wait timeout must be finite and nonnegative")
        with self._io_lock:
            if self._closed.is_set():
                raise InputStreamError("stream_closed", "cannot wait on a closed input stream")
            try:
                frame = self._next_frame(time.monotonic() + timeout)
                return [] if frame is None else [self._decode(frame)]
            except InputStreamError as exc:
                self.close()
                if exc.code == "stream_eof" and self._returncode is not None:
                    raise InputStreamError("stream_eof",
                                           f"{exc.detail}; exit status {self._returncode}") from exc
                raise
            except OSError as exc:
                self.close()
                raise InputStreamError("command_io_failed", "event command stream I/O failed") from exc

    def wake(self) -> None:
        """Interrupt a wait from another thread without discarding event frames."""
        try:
            self._wake_write.send(b"x")
        except (BlockingIOError, OSError):
            pass

    @staticmethod
    def _signal_exact(descriptor: int | None, signum: int) -> None:
        if descriptor is None:
            return
        try:
            signal.pidfd_send_signal(descriptor, signum)
        except ProcessLookupError:
            pass

    def _wait_supervisor(self, deadline: float) -> bool:
        if self._pidfd is None:
            return False
        while not self._exited:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            readers = [pipe.fileno() for pipe in (self._stdout, self._stderr) if pipe is not None]
            readers.append(self._pidfd)
            try:
                readable, _ = _wait_fds(readers, (), remaining)
            except InterruptedError:
                continue
            for attribute in ("_stdout", "_stderr"):
                pipe = self._stdout if attribute == "_stdout" else self._stderr
                if pipe is not None and pipe.fileno() in readable:
                    try:
                        chunk = os.read(pipe.fileno(), 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        pipe.close()
                        if attribute == "_stdout":
                            self._stdout = None
                        else:
                            self._stderr = None
            if self._pidfd in readable:
                self._exited = True
        return True

    def _finish_supervisor(self) -> None:
        process = self._process
        cleanup_fault: BaseException | None = None
        result_confirmed = False
        try:
            if process is not None:
                if self._pidfd is not None:
                    if not self._exited:
                        self._signal_exact(self._pidfd, signal.SIGTERM)
                        self._signal_exact(self._pidfd, signal.SIGCONT)
                    if not self._wait_supervisor(time.monotonic() + _CLOSE_GRACE_SECONDS + 1):
                        self._signal_exact(self._pidfd, signal.SIGKILL)
                        self._wait_supervisor(time.monotonic() + _CLOSE_GRACE_SECONDS)
                else:
                    # No provider can exist before the mandatory pidfd proof
                    # and ACK. Close authority and lifeline channels before
                    # validated numeric cleanup of the still-unreaped direct
                    # supervisor; a stopped helper cannot consume EOF alone.
                    if self._acknowledge_write is not None:
                        os.close(self._acknowledge_write)
                        self._acknowledge_write = None
                    if self._lifeline_write is not None:
                        os.close(self._lifeline_write)
                        self._lifeline_write = None
                    if self._supervisor_identity is None:
                        try:
                            process.wait(timeout=_CONTROL_TIMEOUT_SECONDS)
                        except subprocess.TimeoutExpired as exc:
                            raise InputStreamError(
                                "cleanup_timeout",
                                "pre-authority event supervisor lacked a cleanup identity",
                            ) from exc
                        if _command_process_identity(process.pid) is not None:
                            raise InputStreamError(
                                "containment_failed",
                                "pre-authority event supervisor remained after reap",
                            )
                    else:
                        _cleanup_unpinned_direct_child(
                            process,
                            self._supervisor_identity,
                            "event command supervisor",
                            timeout=_CONTROL_TIMEOUT_SECONDS,
                        )
                    self._exited = True

                if self._lifeline_write is not None:
                    os.close(self._lifeline_write)
                    self._lifeline_write = None

                if self._anchor_pidfd is not None and not _descriptor_ready(
                    self._anchor_pidfd, _CLOSE_GRACE_SECONDS,
                ):
                    # The exact live anchor handles SIGTERM by killing its own
                    # still-pinned group; SIGCONT wakes a stopped anchor.
                    self._signal_exact(self._anchor_pidfd, signal.SIGTERM)
                    self._signal_exact(self._anchor_pidfd, signal.SIGCONT)
                    if not _descriptor_ready(self._anchor_pidfd, _CLOSE_GRACE_SECONDS):
                        raise InputStreamError(
                            "cleanup_timeout",
                            "event command anchor did not terminate its process group",
                        )

                if self._pidfd is not None and not self._exited:
                    raise InputStreamError(
                        "cleanup_timeout",
                        "event command supervisor did not exit after termination",
                    )
                try:
                    process.wait(timeout=1)
                except subprocess.TimeoutExpired as exc:
                    raise InputStreamError(
                        "cleanup_timeout", "event command supervisor was not reapable",
                    ) from exc

                if self._result_read is not None and self._exited:
                    try:
                        self._returncode = _read_command_result(self._result_read)
                    except (OSError, TypeError, ValueError) as exc:
                        if self._authority_granted:
                            raise InputStreamError(
                                "containment_failed",
                                "event command supervisor did not publish an exact final status",
                            ) from exc
                    else:
                        result_confirmed = True

                identities = tuple(
                    identity
                    for identity in (self._supervisor_identity, self._anchor)
                    if identity is not None
                )
                if identities and not _command_identities_gone(identities, 1):
                    raise InputStreamError(
                        "containment_failed",
                        "event command containment identities survived cleanup",
                    )
                if self._authority_granted and not result_confirmed:
                    raise InputStreamError(
                        "containment_failed",
                        "event command cleanup completed without an exact final status",
                    )
        except BaseException as exc:
            cleanup_fault = exc
        finally:
            for attribute in (
                "_identity_read", "_acknowledge_write", "_lifeline_write", "_result_read",
                "_provider_ready_read",
            ):
                descriptor = getattr(self, attribute)
                if descriptor is not None:
                    try:
                        os.close(descriptor)
                    except OSError:
                        pass
                    setattr(self, attribute, None)
            for attribute in ("_pidfd", "_anchor_pidfd"):
                descriptor = getattr(self, attribute)
                if descriptor is not None:
                    os.close(descriptor)
                    setattr(self, attribute, None)
        if cleanup_fault is not None:
            raise cleanup_fault

    def close(self) -> None:
        """Allow bounded command cleanup, then kill its remaining group and reap."""
        self._closed.set()
        self.wake()
        with self._io_lock:
            if self._released:
                return
            self._released = True
            try:
                self._finish_supervisor()
            finally:
                for pipe in (self._stdin, self._stdout, self._stderr):
                    if pipe is not None:
                        pipe.close()
                self._stdin = self._stdout = self._stderr = None
                self._buffer.clear()
                self._buffer_start = 0
                self._wake_read.close()
                self._wake_write.close()
