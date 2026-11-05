#!/usr/bin/env python3
"""Hold one ``chat launch`` child inert until its parent owns an exact pidfd."""

from __future__ import annotations

import ctypes
import os
import signal
import sys
from collections.abc import Sequence


_PR_SET_PDEATHSIG = 1


def _arm_parent_death(expected_parent: int) -> bool:
    if os.getppid() != expected_parent:
        return False
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
    ]
    libc.prctl.restype = ctypes.c_int
    if int(libc.prctl(_PR_SET_PDEATHSIG, signal.SIGKILL, 0, 0, 0)) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    if os.getppid() != expected_parent:
        os.kill(os.getpid(), signal.SIGKILL)
        return False
    return True


def _write_ready(descriptor: int) -> None:
    while True:
        try:
            written = os.write(descriptor, b"1")
            break
        except InterruptedError:
            continue
    if written != 1:
        raise OSError("launch gate could not publish readiness")


def run(
    expected_parent: int, activation_fd: int, readiness_fd: int, command: Sequence[str],
) -> int:
    """Publish readiness, await exact activation, then replace this process."""
    if expected_parent <= 0 or activation_fd < 0 or readiness_fd < 0 or not command:
        raise ValueError(
            "launch gate requires a parent, activation and readiness descriptors, and command",
        )
    if not _arm_parent_death(expected_parent):
        return 125
    try:
        _write_ready(readiness_fd)
    finally:
        os.close(readiness_fd)
    while True:
        try:
            activation = os.read(activation_fd, 2)
            break
        except InterruptedError:
            continue
    os.close(activation_fd)
    if activation != b"1" or os.getppid() != expected_parent:
        return 125
    os.execvp(command[0], list(command))


def main(argv: Sequence[str] | None = None) -> int:
    """Run the private launch-gate protocol from inherited descriptor arguments."""
    try:
        arguments = tuple(sys.argv[1:] if argv is None else argv)
        if len(arguments) < 4:
            raise ValueError(
                "launch gate requires parent, activation and readiness descriptors, and command",
            )
        return run(
            int(arguments[0]), int(arguments[1]), int(arguments[2]), arguments[3:],
        )
    except (OSError, ValueError) as exc:
        print(f"chat launch gate: {exc}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
