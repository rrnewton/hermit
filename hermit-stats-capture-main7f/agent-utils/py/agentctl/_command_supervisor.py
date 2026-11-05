#!/usr/bin/env python3
"""Private subprocess supervisor for bounded command-adapter execution."""

from __future__ import annotations

import ctypes
import json
import os
import selectors
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Sequence
from pathlib import Path
from types import FrameType

_PKG_PARENT = str(Path(__file__).resolve().parent.parent)
if _PKG_PARENT not in sys.path:
    sys.path.insert(0, _PKG_PARENT)

from agentctl.procstat import parse_process_stat


_PR_SET_PDEATHSIG = 1
_PR_SET_CHILD_SUBREAPER = 36
_CLEANUP_TIMEOUT = 2.0
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


def _arm_parent_death(expected_parent: int) -> bool:
    """Before command authority exists, parent death may kill this process."""
    if os.getppid() != expected_parent:
        return False
    _prctl(_PR_SET_PDEATHSIG, signal.SIGKILL)
    if os.getppid() != expected_parent:
        os.kill(os.getpid(), signal.SIGKILL)
        return False
    return True


def _arm_active_parent_death(expected_parent: int) -> bool:
    """SIGCONT wakes even a stopped supervisor so it can kill and reap the group."""
    if os.getppid() != expected_parent:
        return False
    _prctl(_PR_SET_PDEATHSIG, signal.SIGCONT)
    if os.getppid() != expected_parent:
        os.kill(os.getpid(), signal.SIGCONT)
        return False
    return True


def _become_subreaper() -> None:
    """Adopt orphaned adapter descendants instead of leaving them to PID 1."""
    _prctl(_PR_SET_CHILD_SUBREAPER, 1)


def _kill_group(pgid: int) -> None:
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def _reap_group(pgid: int, deadline: float) -> None:
    """Reap every adopted process which stayed in the adapter's original PGID."""
    while True:
        try:
            pid, _ = os.waitpid(-pgid, os.WNOHANG)
        except ChildProcessError:
            return
        if pid != 0:
            continue
        _kill_group(pgid)
        if time.monotonic() >= deadline:
            raise TimeoutError("adapter descendants did not exit before the cleanup deadline")
        time.sleep(0.005)


def _exit_like_adapter(returncode: int) -> int:
    if returncode >= 0:
        return returncode
    signum = -returncode
    if signum not in (signal.SIGKILL, signal.SIGSTOP):
        signal.signal(signum, signal.SIG_DFL)
    os.kill(os.getpid(), signum)
    return 128 + signum


def _identity(pid: int) -> dict[str, int | str]:
    raw = Path(f"/proc/{pid}/stat").read_bytes()
    parsed = parse_process_stat(raw)
    if parsed is None or parsed.pid != pid:
        raise ValueError("malformed process identity record")
    return {
        "pid": pid,
        "state": parsed.state,
        "ppid": parsed.ppid,
        "pgrp": parsed.pgrp,
        "session": parsed.session,
        "starttime": parsed.starttime,
    }


def _new_group() -> None:
    os.setpgid(0, 0)


def _join_group(pgid: int) -> Callable[[], None]:
    def join() -> None:
        os.setpgid(0, pgid)
    return join


def _wait_control(descriptor: int, cancel_fd: int, cancelled: list[int]) -> bytes:
    os.set_blocking(descriptor, False)
    selector = selectors.DefaultSelector()
    selector.register(descriptor, selectors.EVENT_READ)
    selector.register(cancel_fd, selectors.EVENT_READ)
    deadline = time.monotonic() + _CONTROL_TIMEOUT
    try:
        while not cancelled:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("command supervisor control acknowledgement timed out")
            ready = selector.select(remaining)
            if not ready:
                continue
            if any(key.fd == cancel_fd for key, _ in ready):
                return b""
            try:
                return os.read(descriptor, 1)
            except BlockingIOError:
                continue
        return b""
    finally:
        selector.close()


def _wait_child(process: subprocess.Popen[bytes], cancel_fd: int,
                cancelled: list[int]) -> bool:
    pidfd = os.pidfd_open(process.pid)
    selector = selectors.DefaultSelector()
    selector.register(pidfd, selectors.EVENT_READ)
    selector.register(cancel_fd, selectors.EVENT_READ)
    try:
        while not cancelled:
            for key, _ in selector.select():
                if key.fd == pidfd:
                    return True
                if key.fd == cancel_fd:
                    return False
        return False
    finally:
        selector.close()
        os.close(pidfd)


def run(expected_parent: int, identity_fd: int, acknowledge_fd: int,
        lifeline_fd: int, result_fd: int, command: Sequence[str]) -> int:
    """Run one adapter while a persistent trusted process pins its original PGID."""
    if not command:
        raise ValueError("chat helper command must not be empty")
    # Signal dispositions and masks survive exec. The supervisor's zombie
    # anchor is an identity/PGID pin, so inherited SIG_IGN/SA_NOCLDWAIT would
    # destroy a safety invariant; blocked teardown signals would defeat the
    # parent-death and cancellation bounds.
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
    process: subprocess.Popen[bytes] | None = None
    leader_returncode = 125
    try:
        anchor = subprocess.Popen(
            (sys.executable, str(anchor_program), str(os.getpid()),
             str(activate_read), str(ready_write), str(lifeline_fd), str(supervisor_pidfd)),
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
        if (anchor_identity["ppid"] != os.getpid()
                or anchor_identity["pgrp"] != anchor.pid
                or anchor_identity["session"] != os.getsid(0)):
            raise ValueError("command anchor did not enter its expected process group")
        publication = dict(anchor_identity)
        publication.update(version=1, supervisor_pid=os.getpid())
        os.write(identity_fd, json.dumps(publication, sort_keys=True).encode("ascii") + b"\n")
        os.close(identity_fd)
        identity_fd = -1
        if _wait_control(acknowledge_fd, cancel_read, cancelled) != b"1":
            raise ValueError("command anchor publication was not acknowledged")
        os.close(acknowledge_fd)
        acknowledge_fd = -1
        os.write(activate_write, b"1")
        os.close(activate_write)
        activate_write = -1
        if _wait_control(ready_read, cancel_read, cancelled) != b"1":
            raise ValueError("command anchor did not assume bridge lifeline ownership")
        os.close(ready_read)
        ready_read = -1
        if cancelled or not _arm_active_parent_death(expected_parent):
            return 128 + (cancelled[0] if cancelled else signal.SIGTERM)

        # This helper is private and single-threaded; preexec_fn is used only
        # for Python 3.10 compatibility, where Popen(process_group=...) is not
        # available. The real adapter inherits the anchor's already-pinned PGID.
        process = subprocess.Popen(
            command,
            close_fds=True,
            preexec_fn=_join_group(anchor.pid),
        )
        _wait_child(process, cancel_read, cancelled)
    finally:
        for descriptor in (
            identity_fd, acknowledge_fd, lifeline_fd,
            activate_read, activate_write, ready_read, ready_write, supervisor_pidfd,
            cancel_read, cancel_write,
        ):
            if descriptor >= 0:
                os.close(descriptor)
        if anchor is not None:
            cleanup_deadline = time.monotonic() + _CLEANUP_TIMEOUT
            # The live/unreaped anchor reserves the numeric PGID until after
            # every same-group member has received SIGKILL.
            _kill_group(anchor.pid)
            if process is not None:
                leader_returncode = process.wait(
                    timeout=max(0.0, cleanup_deadline - time.monotonic()))
            anchor.wait(timeout=max(0.0, cleanup_deadline - time.monotonic()))
            _reap_group(anchor.pid, cleanup_deadline)
    try:
        os.write(result_fd, json.dumps(
            {"version": 1, "returncode": leader_returncode}, sort_keys=True,
        ).encode("ascii") + b"\n")
    except BrokenPipeError:
        if not cancelled:
            raise
    finally:
        os.close(result_fd)
    if cancelled:
        return 128 + cancelled[0]
    return _exit_like_adapter(leader_returncode)


def main(argv: Sequence[str] | None = None) -> int:
    """Run the private one-shot supervisor from inherited descriptor arguments."""
    try:
        arguments = tuple(sys.argv[1:] if argv is None else argv)
        if len(arguments) < 6:
            raise ValueError(
                "chat command supervisor requires parent and four control descriptors")
        expected_parent = int(arguments[0])
        identity_fd, acknowledge_fd, lifeline_fd, result_fd = map(int, arguments[1:5])
        if expected_parent <= 0 or min(
            identity_fd, acknowledge_fd, lifeline_fd, result_fd,
        ) < 0:
            raise ValueError("chat command supervisor control identities must be positive")
        return run(
            expected_parent, identity_fd, acknowledge_fd, lifeline_fd, result_fd,
            arguments[5:],
        )
    except (OSError, ValueError, subprocess.SubprocessError, TimeoutError) as exc:
        print(f"chat command supervisor: {exc}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
