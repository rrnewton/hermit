#!/usr/bin/env python3
"""Trusted lifetime supervisor for one persistent Chat event command."""

from __future__ import annotations

import json
import os
import selectors
import signal
import subprocess
import sys
import time
from collections.abc import Sequence
from pathlib import Path
from types import FrameType

_PKG_PARENT = str(Path(__file__).resolve().parent.parent)
if _PKG_PARENT not in sys.path:
    sys.path.insert(0, _PKG_PARENT)

from agentctl._command_supervisor import (
    _arm_active_parent_death,
    _arm_parent_death,
    _become_subreaper,
    _identity,
    _join_group,
    _kill_group,
    _new_group,
    _reap_group,
    _wait_control,
)


_CLEANUP_TIMEOUT = 2.0


def _wait_provider(
    pidfd: int,
    cancel_fd: int | None,
    cancelled: list[int],
    timeout: float | None = None,
) -> bool:
    """Return true for provider exit and false for cancellation or timeout."""
    selector = selectors.DefaultSelector()
    selector.register(pidfd, selectors.EVENT_READ, "provider")
    if cancel_fd is not None:
        selector.register(cancel_fd, selectors.EVENT_READ, "cancel")
    deadline = None if timeout is None else time.monotonic() + timeout
    try:
        while True:
            if cancelled:
                return False
            remaining = None if deadline is None else max(0.0, deadline - time.monotonic())
            if deadline is not None and remaining == 0:
                return False
            ready = selector.select(remaining)
            if not ready:
                return False
            if any(key.data == "provider" for key, _ in ready):
                return True
            if cancel_fd is not None and any(key.data == "cancel" for key, _ in ready):
                return False
    finally:
        selector.close()


def run(
    expected_parent: int,
    identity_fd: int,
    acknowledge_fd: int,
    lifeline_fd: int,
    result_fd: int,
    provider_ready_fd: int,
    command: Sequence[str],
) -> int:
    """Activate an event provider only after its anchored group is acknowledged."""
    if not command:
        raise ValueError("event command must not be empty")
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    signal.pthread_sigmask(
        signal.SIG_UNBLOCK,
        {signal.SIGCHLD, signal.SIGTERM, signal.SIGINT, signal.SIGCONT},
    )
    cancelled: list[int] = []
    cancel_read, cancel_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)

    def cancel(signum: int, _frame: FrameType | None) -> None:
        if not cancelled:
            cancelled.append(signal.SIGTERM if signum == signal.SIGCONT else signum)
        try:
            os.write(cancel_write, b"1")
        except (BlockingIOError, OSError):
            pass

    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGINT, cancel)
    signal.signal(signal.SIGCONT, cancel)
    if not _arm_parent_death(expected_parent):
        return 128 + signal.SIGTERM
    _become_subreaper()
    if cancelled:
        return 128 + cancelled[0]

    activate_read, activate_write = os.pipe2(os.O_CLOEXEC)
    ready_read, ready_write = os.pipe2(os.O_CLOEXEC)
    supervisor_pidfd = os.pidfd_open(os.getpid())
    anchor_program = Path(__file__).with_name("_command_anchor.py")
    anchor: subprocess.Popen[bytes] | None = None
    provider: subprocess.Popen[bytes] | None = None
    provider_pidfd: int | None = None
    provider_returncode = 125
    failure: BaseException | None = None
    try:
        anchor = subprocess.Popen(
            (
                sys.executable,
                str(anchor_program),
                str(os.getpid()),
                str(activate_read),
                str(ready_write),
                str(lifeline_fd),
                str(supervisor_pidfd),
            ),
            close_fds=True,
            pass_fds=(activate_read, ready_write, lifeline_fd, supervisor_pidfd),
            preexec_fn=_new_group,
        )
        os.close(activate_read)
        activate_read = -1
        os.close(ready_write)
        ready_write = -1
        os.close(lifeline_fd)
        lifeline_fd = -1
        os.close(supervisor_pidfd)
        supervisor_pidfd = -1

        anchor_identity = _identity(anchor.pid)
        if (
            anchor_identity["ppid"] != os.getpid()
            or anchor_identity["pgrp"] != anchor.pid
            or anchor_identity["session"] != os.getsid(0)
        ):
            raise ValueError("event command anchor did not enter its expected process group")
        publication = dict(anchor_identity)
        publication.update(version=1, supervisor_pid=os.getpid())
        os.write(identity_fd, json.dumps(publication, sort_keys=True).encode("ascii") + b"\n")
        os.close(identity_fd)
        identity_fd = -1
        if _wait_control(acknowledge_fd, cancel_read, cancelled) != b"1":
            raise ValueError("event command anchor publication was not acknowledged")
        os.close(acknowledge_fd)
        acknowledge_fd = -1
        os.write(activate_write, b"1")
        os.close(activate_write)
        activate_write = -1
        if _wait_control(ready_read, cancel_read, cancelled) != b"1":
            raise ValueError("event command anchor did not assume bridge lifeline ownership")
        os.close(ready_read)
        ready_read = -1
        if not cancelled and _arm_active_parent_death(expected_parent):
            # This trusted helper is single-threaded. The provider inherits the
            # supervisor's already-bounded stdio and the live anchor's pinned PGID.
            provider = subprocess.Popen(
                command,
                close_fds=True,
                preexec_fn=_join_group(anchor.pid),
            )
            provider_pidfd = os.pidfd_open(provider.pid)
            os.write(provider_ready_fd, b"1")
            os.close(provider_ready_fd)
            provider_ready_fd = -1
            if not _wait_provider(provider_pidfd, cancel_read, cancelled):
                try:
                    signal.pidfd_send_signal(provider_pidfd, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                _wait_provider(provider_pidfd, None, [], _CLEANUP_TIMEOUT)
        elif not cancelled:
            cancelled.append(signal.SIGTERM)
    except BaseException as exc:
        failure = exc
    finally:
        try:
            for descriptor in (
                identity_fd,
                acknowledge_fd,
                lifeline_fd,
                activate_read,
                activate_write,
                ready_read,
                ready_write,
                supervisor_pidfd,
                cancel_read,
                cancel_write,
                provider_pidfd if provider_pidfd is not None else -1,
                provider_ready_fd,
            ):
                if descriptor >= 0:
                    os.close(descriptor)
            if anchor is not None:
                cleanup_deadline = time.monotonic() + _CLEANUP_TIMEOUT
                # The trusted live/unreaped anchor reserves this numeric PGID.
                _kill_group(anchor.pid)
                if provider is not None:
                    provider_returncode = provider.wait(
                        timeout=max(0.0, cleanup_deadline - time.monotonic()),
                    )
                anchor.wait(timeout=max(0.0, cleanup_deadline - time.monotonic()))
                _reap_group(anchor.pid, cleanup_deadline)
        except BaseException as exc:
            if failure is None:
                failure = exc

    try:
        os.write(
            result_fd,
            json.dumps(
                {"version": 1, "returncode": provider_returncode}, sort_keys=True,
            ).encode("ascii")
            + b"\n",
        )
    except BrokenPipeError:
        if not cancelled:
            raise
    finally:
        os.close(result_fd)
    if failure is not None:
        raise failure
    if cancelled:
        return 128 + cancelled[0]
    if provider_returncode >= 0:
        return provider_returncode
    return 128 + -provider_returncode


def main(argv: Sequence[str] | None = None) -> int:
    """Run the private persistent supervisor from inherited descriptor arguments."""
    try:
        arguments = tuple(sys.argv[1:] if argv is None else argv)
        if len(arguments) < 7:
            raise ValueError(
                "event command supervisor requires parent and four control descriptors",
            )
        expected_parent = int(arguments[0])
        identity_fd, acknowledge_fd, lifeline_fd, result_fd, provider_ready_fd = map(
            int, arguments[1:6],
        )
        if expected_parent <= 0 or min(
            identity_fd, acknowledge_fd, lifeline_fd, result_fd, provider_ready_fd,
        ) < 0:
            raise ValueError("event command supervisor control identities must be positive")
        return run(
            expected_parent,
            identity_fd,
            acknowledge_fd,
            lifeline_fd,
            result_fd,
            provider_ready_fd,
            arguments[6:],
        )
    except (OSError, ValueError, subprocess.SubprocessError, TimeoutError) as exc:
        print(f"event command supervisor: {exc}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
