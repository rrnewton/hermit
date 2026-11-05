"""Default, production implementations of the :mod:`tick_hub.protocols` boundaries.

:class:`SubprocessGateRunner` runs a reminder's gate via ``bash -c`` with a timeout;
:class:`GlobFileAgeProbe` measures the newest matching file's mtime against the tick's clock. Both
are deliberately side-effectful (subprocess / filesystem) and therefore live outside the pure engine
so tests can swap in deterministic fakes.
"""

from __future__ import annotations

import glob
import os
import signal
import subprocess
import threading
import time
from collections.abc import Callable
from contextlib import AbstractContextManager, nullcontext
from types import FrameType, TracebackType
from typing import Optional

from tick_hub.protocols import GateResult

DEFAULT_GATE_TIMEOUT_SECS = 30
TERMINATE_WAIT_SECS = 1
CANCELLATION_POLL_SECS = 0.1
SignalHandler = int | Callable[[int, FrameType | None], object]


class _GateCancelled(BaseException):
    """Internal checkpoint for cancellation observed before or during gate creation."""


class _ParallelCancellationGuard(AbstractContextManager[None]):
    """Own every process group launched by one concurrent gate scope."""

    def __init__(self, runner: SubprocessGateRunner) -> None:
        self.runner = runner
        self.received_signal: int | None = None
        self.previous_handlers: list[tuple[signal.Signals, SignalHandler]] = []

    def __enter__(self) -> None:
        if os.name == "posix" and threading.current_thread() is threading.main_thread():
            candidates = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)
            try:
                for signum in candidates:
                    previous = signal.getsignal(signum)
                    if previous != signal.SIG_DFL and not (
                        signum == signal.SIGINT
                        and previous is signal.default_int_handler
                    ):
                        continue
                    self.previous_handlers.append((signum, previous))
                    signal.signal(signum, self._cancel)
            except BaseException:
                self._restore_handlers()
                raise
        self.runner._parallel_guard = self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        _exc: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        try:
            if exc_type is not None or self.received_signal is not None:
                self.kill_all()
        finally:
            self.runner._parallel_guard = None
            self._restore_handlers()
        if self.received_signal is not None:
            signal.raise_signal(self.received_signal)
            raise SystemExit(128 + self.received_signal)

    def own(self, proc: subprocess.Popen[str]) -> None:
        """Attach a spawned process and close a cancellation-during-Popen race."""
        with self.runner._active_lock:
            self.runner._active.add(proc)
            cancelled = self.received_signal is not None
        if cancelled:
            _kill_process_group(proc)
            raise _GateCancelled

    def release(self, proc: subprocess.Popen[str]) -> None:
        with self.runner._active_lock:
            self.runner._active.discard(proc)

    def checkpoint(self) -> None:
        if self.received_signal is not None:
            raise _GateCancelled

    def _cancel(self, signum: int, _frame: FrameType | None) -> None:
        if self.received_signal is None:
            self.received_signal = signum
        self.kill_all()

    def kill_all(self) -> None:
        with self.runner._active_lock:
            active = tuple(self.runner._active)
        for proc in active:
            _kill_process_group(proc)

    def _restore_handlers(self) -> None:
        for signum, previous in reversed(self.previous_handlers):
            signal.signal(signum, previous)
        self.previous_handlers.clear()


class _GateCancellationGuard:
    """Own one gate process group across POSIX cancellation and Python unwinding."""

    def __init__(self) -> None:
        self.proc: subprocess.Popen[str] | None = None
        self.received_signal: int | None = None
        self.previous_handlers: list[tuple[signal.Signals, SignalHandler]] = []

    def __enter__(self) -> _GateCancellationGuard:
        if os.name != "posix" or threading.current_thread() is not threading.main_thread():
            return self
        candidates = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)
        try:
            for signum in candidates:
                previous = signal.getsignal(signum)
                # Leave caller-owned custom or ignored dispositions authoritative.
                if previous != signal.SIG_DFL and not (
                    signum == signal.SIGINT and previous is signal.default_int_handler
                ):
                    continue
                self.previous_handlers.append((signum, previous))
                signal.signal(signum, self._cancel)
        except BaseException:
            self._restore_handlers()
            raise
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        _exc: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        try:
            if self.proc is not None and (exc_type is not None or self.received_signal is not None):
                _terminate(self.proc)
        finally:
            self._restore_handlers()
        if self.received_signal is not None:
            signal.raise_signal(self.received_signal)
            raise SystemExit(128 + self.received_signal)

    def own(self, proc: subprocess.Popen[str]) -> None:
        """Attach a spawned gate and close a cancellation-during-Popen race."""
        self.proc = proc
        self.checkpoint()

    def checkpoint(self) -> None:
        """Prevent a gate launch after cancellation reached the owning thread."""
        if self.received_signal is not None:
            raise _GateCancelled

    def _cancel(self, signum: int, _frame: FrameType | None) -> None:
        if self.received_signal is None:
            self.received_signal = signum
        if self.proc is not None:
            _kill_process_group(self.proc)

    def _restore_handlers(self) -> None:
        for signum, previous in reversed(self.previous_handlers):
            signal.signal(signum, previous)
        self.previous_handlers.clear()


class SubprocessGateRunner:
    """Run a gate command with ``bash -c`` under a timeout, capturing stdout.

    A command that cannot be launched, times out, or otherwise fails to execute returns
    ``ok=False`` with a reason (No Silent Failure): the engine surfaces that as an ``ERROR:`` line
    rather than pretending the gate passed or failed. On POSIX, when ``run`` is called from the
    Python main thread, cancellation kills the gate's independently sessioned process group,
    bounded-waits to reap the direct gate process, closes its capture pipes, restores the original
    disposition, and re-delivers the signal. An outer containment layer remains responsible for a
    descendant that deliberately creates another session."""

    def __init__(self, timeout: int = DEFAULT_GATE_TIMEOUT_SECS) -> None:
        self.timeout = timeout
        self._active: set[subprocess.Popen[str]] = set()
        self._active_lock = threading.Lock()
        self._parallel_guard: _ParallelCancellationGuard | None = None

    def parallel_scope(self) -> AbstractContextManager[None]:
        """Return a main-thread signal scope covering concurrent gate processes."""
        if self._parallel_guard is not None:
            raise RuntimeError("a parallel gate scope is already active")
        return _ParallelCancellationGuard(self)

    def run(self, cmd: str, *, timeout: int | None = None) -> GateResult:
        """Execute ``cmd`` and return its captured completion or launch/timeout error."""
        effective_timeout = self.timeout if timeout is None else timeout
        parallel = self._parallel_guard
        guard: AbstractContextManager[
            _GateCancellationGuard | _ParallelCancellationGuard
        ] = _GateCancellationGuard() if parallel is None else nullcontext(parallel)
        with guard as cancellation:
            cancellation.checkpoint()
            try:
                proc = subprocess.Popen(
                    ["bash", "-c", cmd],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    errors="replace",
                    start_new_session=os.name == "posix",
                )
            except OSError as exc:
                return GateResult(returncode=-1, stdout="", ok=False, error=str(exc))
            try:
                cancellation.own(proc)
                stdout, _stderr = _communicate_with_cancellation(
                    proc,
                    effective_timeout,
                    cancellation,
                )
            except subprocess.TimeoutExpired:
                _terminate(proc)
                return GateResult(
                    returncode=-1,
                    stdout="",
                    ok=False,
                    error=f"timed out after {effective_timeout}s",
                )
            except OSError as exc:
                _terminate(proc)
                return GateResult(returncode=-1, stdout="", ok=False, error=str(exc))
            finally:
                if parallel is not None:
                    parallel.release(proc)
            return GateResult(returncode=proc.returncode, stdout=stdout, ok=True)


def _communicate_with_cancellation(
    proc: subprocess.Popen[str],
    timeout: int,
    cancellation: _GateCancellationGuard | _ParallelCancellationGuard,
) -> tuple[str, str]:
    """Capture output while observing process-level cancellation within a short bound."""
    deadline = time.monotonic() + timeout
    while True:
        cancellation.checkpoint()
        remaining = max(0.0, deadline - time.monotonic())
        try:
            return proc.communicate(timeout=min(CANCELLATION_POLL_SECS, remaining))
        except subprocess.TimeoutExpired:
            cancellation.checkpoint()
            if time.monotonic() >= deadline:
                raise


def _terminate(proc: subprocess.Popen[str]) -> None:
    _kill_process_group(proc)
    try:
        proc.wait(timeout=TERMINATE_WAIT_SECS)
    except (OSError, subprocess.TimeoutExpired):
        pass
    finally:
        for stream in (proc.stdout, proc.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass


def _kill_process_group(proc: subprocess.Popen[str]) -> None:
    if proc.returncode is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError:
            try:
                os.kill(proc.pid, signal.SIGKILL)
            except OSError:
                pass
    else:
        try:
            proc.kill()
        except OSError:
            pass


class GlobFileAgeProbe:
    """Report the age of the newest file matching a glob, relative to ``now``."""

    def newest_age_secs(self, pattern: str, now: int) -> Optional[int]:
        """Return the nonnegative age of the newest match, or ``None`` if unavailable."""
        matches = glob.glob(pattern)
        if not matches:
            return None
        try:
            newest_mtime = max(os.path.getmtime(p) for p in matches)
        except OSError:
            return None
        return max(0, int(now - newest_mtime))


def wall_clock_now() -> int:
    """The real wall-clock epoch (seconds), the tick's default ``now``."""
    return int(time.time())
