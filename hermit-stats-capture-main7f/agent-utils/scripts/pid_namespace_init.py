#!/usr/bin/env python3
"""Reap adopted children around a command in a fresh PID namespace.

Run this with the real interpreter, resolved *before* entering the namespace.
An interpreter launcher that forks would otherwise occupy PID 1 itself. During
the command, waitpid collects only dead children; live adopted processes remain
untouched. Completion or explicit cancellation starts bounded namespace cleanup.
The command's exit code is preserved; a signal is reported as 128 + signal, as
with a shell. Cancellation cannot turn into success if the command catches it.
"""

from __future__ import annotations

import os
import signal
import sys
import time
from types import FrameType


POLL_SECONDS = 0.01
TERM_GRACE_SECONDS = 1.0
KILL_GRACE_SECONDS = 1.0
REAP_BATCH_SIZE = 128
CANCELLATION_SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


def _reap(primary: int, status: int | None) -> tuple[int | None, bool]:
    """Own every wait status, including the primary's; report live children."""
    for _ in range(REAP_BATCH_SIZE):
        try:
            pid, raw_status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return status, False
        except InterruptedError:
            continue
        if pid == 0:
            return status, True
        if pid == primary and status is None:
            status = raw_status
    # A busy producer must not prevent the caller from handling cancellation or
    # checking cleanup deadlines. The next batch discovers whether any remain.
    return status, True


def _signal_namespace(signum: int) -> None:
    # Linux confines this to the caller's PID namespace and excludes PID 1.
    # Keep the guard here too: this operation must never reach host processes.
    if os.getpid() != 1:
        raise RuntimeError("namespace-wide signal requires namespace PID 1")
    try:
        os.kill(-1, signum)
    except ProcessLookupError:
        pass


def _exit_code(status: int | None, cancellation: int | None) -> int:
    if status is None:
        return 128 + cancellation if cancellation is not None else 1
    code = os.waitstatus_to_exitcode(status)
    if code < 0:
        return 128 - code
    if code == 0 and cancellation is not None:
        return 128 + cancellation
    return code


def main(argv: list[str]) -> int:
    if os.getpid() != 1:
        print("pid_namespace_init: refused: must run as namespace PID 1", file=sys.stderr)
        return 2
    if len(argv) < 2 or argv[0] != "--":
        print("usage: pid_namespace_init.py -- COMMAND [ARG ...]", file=sys.stderr)
        return 2
    command = argv[1:]
    pending: list[int] = []

    def record_signal(signum: int, _frame: FrameType | None) -> None:
        if signum not in pending:
            pending.append(signum)

    for signum in CANCELLATION_SIGNALS:
        signal.signal(signum, record_signal)
    # Neither a fork-to-exec handler window nor PEP 475's restarted blocking
    # wait may discard cancellation. The loop below uses nonblocking waits.
    old_mask = signal.pthread_sigmask(signal.SIG_BLOCK, CANCELLATION_SIGNALS)
    try:
        primary = os.fork()
    except OSError as error:
        signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
        print(f"pid_namespace_init: cannot fork command: {error}", file=sys.stderr)
        return 1
    if primary == 0:
        for signum in CANCELLATION_SIGNALS:
            signal.signal(signum, signal.SIG_DFL)
        signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
        try:
            os.execvp(command[0], command)
        except OSError as error:
            print(f"pid_namespace_init: cannot execute {command[0]}: {error}", file=sys.stderr)
            os._exit(127 if isinstance(error, FileNotFoundError) else 126)
    signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)

    status: int | None = None
    cancellation: int | None = None
    shutdown_started: float | None = None
    killed = False
    while True:
        status, has_children = _reap(primary, status)
        # Process a bounded snapshot even when a sender continuously signals us.
        for _ in range(len(pending)):
            cancelled_by = pending.pop(0)
            if cancellation is None:
                cancellation = cancelled_by
                print(f"pid_namespace_init: cancellation signal {cancelled_by}", file=sys.stderr)
            if shutdown_started is None:
                shutdown_started = time.monotonic()
            _signal_namespace(cancelled_by)
        if not has_children:
            # This is the terminal boundary: no child remains to cancel. Do
            # not lose a handler delivery after the bounded signal batch, and
            # stop accepting new cancellation while choosing the final status.
            signal.pthread_sigmask(signal.SIG_BLOCK, CANCELLATION_SIGNALS)
            if cancellation is None and pending:
                cancellation = pending[0]
                print(f"pid_namespace_init: cancellation signal {cancellation}", file=sys.stderr)
            if status is None:
                print("pid_namespace_init: primary wait status is missing", file=sys.stderr)
                return 1
            return _exit_code(status, cancellation)
        if status is not None and shutdown_started is None:
            shutdown_started = time.monotonic()
            _signal_namespace(signal.SIGTERM)
        if shutdown_started is not None:
            elapsed = time.monotonic() - shutdown_started
            if elapsed >= TERM_GRACE_SECONDS and not killed:
                _signal_namespace(signal.SIGKILL)
                killed = True
            if elapsed >= TERM_GRACE_SECONDS + KILL_GRACE_SECONDS:
                print("pid_namespace_init: descendants did not reap after SIGKILL", file=sys.stderr)
                return _exit_code(status, cancellation) or 1
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
