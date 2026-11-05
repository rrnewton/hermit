"""Interrupt-safe ownership of model-backend subprocess groups."""

from __future__ import annotations

import os
import signal
import subprocess
import threading
import time
from collections.abc import Sequence
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator


class BackendProcessStopped(RuntimeError):
    """A backend launch raced with cancellation of its owning operation."""


class BackendProcesses:
    """Track backend process groups so the main thread can stop worker-owned calls."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._processes: set[subprocess.Popen[str]] = set()
        self._stopped = False

    def run(
        self,
        command: Sequence[str],
        *,
        input_text: str,
        cwd: Path,
    ) -> subprocess.CompletedProcess[str]:
        """Run one backend in its own process group and retain captured text output."""

        args = tuple(command)
        with self._lock:
            if self._stopped:
                raise BackendProcessStopped("backend operation was interrupted")
            process = subprocess.Popen(
                args,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                cwd=cwd,
                start_new_session=True,
            )
            self._processes.add(process)
        try:
            stdout, stderr = process.communicate(input=input_text)
        except BaseException:
            self.terminate_all()
            raise
        finally:
            with self._lock:
                self._processes.discard(process)
        return subprocess.CompletedProcess(
            args=args,
            returncode=process.returncode,
            stdout=stdout,
            stderr=stderr,
        )

    @staticmethod
    def _signal_group(process: subprocess.Popen[str], signum: int) -> None:
        if process.poll() is not None:
            return
        try:
            os.killpg(process.pid, signum)
        except ProcessLookupError:
            pass

    def terminate_all(self, timeout_seconds: float = 2.0) -> None:
        """Prevent new launches, then terminate and finally kill every live group."""

        with self._lock:
            self._stopped = True
            processes = tuple(self._processes)
        for process in processes:
            self._signal_group(process, signal.SIGTERM)
        deadline = time.monotonic() + timeout_seconds
        while any(process.poll() is None for process in processes):
            if time.monotonic() >= deadline:
                break
            time.sleep(0.01)
        for process in processes:
            self._signal_group(process, signal.SIGKILL)


@contextmanager
def defer_sigint_during_cleanup() -> Iterator[None]:
    """Ignore a repeated Ctrl-C while bounded executor/process cleanup completes."""

    if threading.current_thread() is not threading.main_thread():
        yield
        return
    previous = signal.getsignal(signal.SIGINT)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        yield
    finally:
        signal.signal(signal.SIGINT, previous)


__all__ = [
    "BackendProcessStopped",
    "BackendProcesses",
    "defer_sigint_during_cleanup",
]
