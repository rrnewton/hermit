#!/usr/bin/env python3
"""Trusted process-group anchor for one bounded Chat adapter invocation."""

from __future__ import annotations

import ctypes
import os
import selectors
import signal
import sys
import time
from collections.abc import Sequence
from types import FrameType


_PR_SET_PDEATHSIG = 1
_CONTROL_TIMEOUT = 5.0


def _prctl(option: int, argument: int) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
    ]
    libc.prctl.restype = ctypes.c_int
    if int(libc.prctl(option, argument, 0, 0, 0)) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))


def _kill_anchored_group(_signum: int = signal.SIGTERM,
                         _frame: FrameType | None = None) -> None:
    """The anchor can always signal its own numeric group without a reuse race."""
    os.killpg(os.getpgrp(), signal.SIGKILL)


def _outer_lost(supervisor_pidfd: int) -> None:
    """Wake the exact supervisor so it can reap; retain self-kill as the bound."""
    if hasattr(signal, "pidfd_send_signal"):
        try:
            signal.pidfd_send_signal(supervisor_pidfd, signal.SIGTERM)
            signal.pidfd_send_signal(supervisor_pidfd, signal.SIGCONT)
        except OSError:
            pass
        else:
            # A healthy or merely stopped supervisor kills this anchor during
            # its group cleanup. The short bound covers a crashed/unresponsive
            # supervisor without letting the adapter group survive.
            time.sleep(0.25)
    _kill_anchored_group()


def _read_byte(descriptor: int, expected_parent: int) -> bytes:
    os.set_blocking(descriptor, False)
    selector = selectors.DefaultSelector()
    selector.register(descriptor, selectors.EVENT_READ)
    deadline = time.monotonic() + _CONTROL_TIMEOUT
    try:
        while True:
            if os.getppid() != expected_parent:
                _kill_anchored_group()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("command anchor activation timed out")
            if not selector.select(min(remaining, 0.1)):
                continue
            try:
                return os.read(descriptor, 1)
            except BlockingIOError:
                continue
    finally:
        selector.close()


def run(expected_parent: int, activate_fd: int, ready_fd: int, lifeline_fd: int,
        supervisor_pidfd: int) -> int:
    """Pin one PGID until its supervisor or owning bridge proves cleanup."""
    if expected_parent <= 0 or os.getppid() != expected_parent:
        return 125
    signal.signal(signal.SIGTERM, _kill_anchored_group)
    signal.signal(signal.SIGINT, _kill_anchored_group)
    _prctl(_PR_SET_PDEATHSIG, signal.SIGKILL)
    if os.getppid() != expected_parent:
        _kill_anchored_group()

    activated = _read_byte(activate_fd, expected_parent)
    os.close(activate_fd)
    if activated != b"1":
        _kill_anchored_group()

    # Publication has been validated and acknowledged. From this point the
    # bridge-owned lifeline, rather than the supervisor's lifetime, owns the
    # anchor. The after-prctl PPid check closes the disarm race.
    # If the supervisor crashes while this anchor is externally stopped,
    # SIGCONT wakes it. The bridge-owned lifeline then decides whether the
    # group remains owned or must be killed.
    try:
        _prctl(_PR_SET_PDEATHSIG, signal.SIGCONT)
        if os.getppid() != expected_parent:
            _kill_anchored_group()
        os.write(ready_fd, b"1")
        os.close(ready_fd)

        # No bytes are valid on the lifeline. EOF means the bridge died or
        # entered emergency cleanup, so kill the entire original adapter group.
        while True:
            os.read(lifeline_fd, 1)
            _outer_lost(supervisor_pidfd)
    finally:
        # Read errors, failed supervisor signaling, and every other active
        # exception have the same fail-safe outcome as lifeline EOF.
        _kill_anchored_group()


def main(argv: Sequence[str] | None = None) -> int:
    """Run the private anchor protocol from inherited descriptor arguments."""
    try:
        arguments = tuple(sys.argv[1:] if argv is None else argv)
        if len(arguments) != 5:
            raise ValueError(
                "command anchor requires parent, activation, readiness, lifeline, and supervisor descriptors")
        expected_parent, activate_fd, ready_fd, lifeline_fd, supervisor_pidfd = (
            int(value) for value in arguments)
        return run(expected_parent, activate_fd, ready_fd, lifeline_fd, supervisor_pidfd)
    except (OSError, TimeoutError, ValueError) as exc:
        print(f"chat command anchor: {exc}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
