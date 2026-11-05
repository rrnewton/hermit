"""Real namespace controls for the lifecycle suite's init process."""

from __future__ import annotations

import os
from dataclasses import dataclass
import importlib.util
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
from typing import cast

import pytest


ROOT = Path(__file__).resolve().parents[2]
INIT = ROOT / "scripts" / "pid_namespace_init.py"
PYTHON = str(Path(sys.executable).resolve())
NAMESPACE_FLAGS = ["--user", "--map-root-user", "--pid", "--fork", "--mount-proc"]


@pytest.fixture(scope="module")
def namespace_argv() -> list[str]:
    unshare = shutil.which("unshare")
    if unshare is None:
        pytest.skip("unshare is unavailable; the Makefile retains the ordinary full-suite path")
    available = subprocess.run(
        [unshare, *NAMESPACE_FLAGS, "true"], capture_output=True, text=True, timeout=10
    )
    if available.returncode != 0:
        pytest.skip(f"PID namespaces unavailable: {available.stderr.strip()}")
    return [unshare, *NAMESPACE_FLAGS, PYTHON, str(INIT), "--", PYTHON, "-c"]


def run_namespace(namespace_argv: list[str], code: str) -> subprocess.CompletedProcess[str]:
    argv = [*namespace_argv, code]
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        stdout, stderr = process.communicate(timeout=10)
        return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
    finally:
        finish_owned(process, None)


def direct_children(parent: int) -> list[int]:
    # /proc/PID/task/PID/children depends on a kernel configuration option.
    result = subprocess.run(
        ["ps", "--ppid", str(parent), "-o", "pid="], capture_output=True, text=True, timeout=5
    )
    assert result.returncode in (0, 1), result.stderr
    return [int(pid) for pid in result.stdout.split()]


@dataclass(frozen=True)
class NamespaceInit:
    pid: int
    pidfd: int


def bind_direct_child(parent: int, pid: int) -> NamespaceInit | None:
    try:
        pidfd = os.pidfd_open(pid)
    except ProcessLookupError:
        return None
    try:
        # Verify after opening: a recycled number cannot bind a foreign
        # process merely because it appeared in an earlier child census.
        status = Path(f"/proc/{pid}/status").read_text()
        if f"PPid:\t{parent}\n" not in status:
            os.close(pidfd)
            return None
    except (FileNotFoundError, ProcessLookupError):
        os.close(pidfd)
        return None
    except BaseException:
        os.close(pidfd)
        raise
    return NamespaceInit(pid, pidfd)


def namespace_init_process(process: subprocess.Popen[str]) -> NamespaceInit:
    children = direct_children(process.pid)
    assert len(children) == 1, children
    child = bind_direct_child(process.pid, children[0])
    assert child is not None, "namespace init exited before identity binding"
    try:
        assert Path(f"/proc/{child.pid}/exe").resolve() == Path(PYTHON)
        signal.pidfd_send_signal(child.pidfd, 0)
    except BaseException:
        os.close(child.pidfd)
        raise
    return child


def wait_ready(process: subprocess.Popen[str], ready: Path) -> None:
    deadline = time.monotonic() + 5
    while not ready.exists():
        assert process.poll() is None, process.communicate()
        assert time.monotonic() < deadline, "namespace command did not become ready"
        time.sleep(0.01)


def finish_owned(process: subprocess.Popen[str], init: NamespaceInit | None) -> None:
    try:
        if process.poll() is None:
            if init is not None:
                try:
                    signal.pidfd_send_signal(init.pidfd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                children = direct_children(process.pid)
                for pid in children:
                    child = bind_direct_child(process.pid, pid)
                    if child is None:
                        continue
                    try:
                        signal.pidfd_send_signal(child.pidfd, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    finally:
                        os.close(child.pidfd)
                if not children:
                    process.kill()
            process.communicate(timeout=5)
    finally:
        if init is not None:
            os.close(init.pidfd)


class PendingParent:
    """Model unshare after reaping init, before publishing its own exit."""

    pid = -1

    def __init__(self, fail_communicate: bool = False) -> None:
        self.fail_communicate = fail_communicate
        self.communicated = False

    def poll(self) -> None:
        return None

    def communicate(self, timeout: float) -> tuple[str, str]:
        assert timeout == 5
        self.communicated = True
        if self.fail_communicate:
            raise RuntimeError("retained communicate failure")
        return "", ""


@pytest.mark.parametrize("fail_communicate", [False, True])
def test_cleanup_uses_retained_pidfd_and_closes_it_on_every_path(
    monkeypatch: pytest.MonkeyPatch, fail_communicate: bool
) -> None:
    parent = PendingParent(fail_communicate)
    signals: list[tuple[int, int]] = []
    closed: list[int] = []

    def no_numeric_signal(_pid: int, _signal: int) -> None:
        pytest.fail("cached numeric PID may now refer to a foreign process")

    def signal_descriptor(pidfd: int, signum: int) -> None:
        signals.append((pidfd, signum))

    monkeypatch.setattr(os, "kill", no_numeric_signal)
    monkeypatch.setattr(signal, "pidfd_send_signal", signal_descriptor)
    monkeypatch.setattr(os, "close", closed.append)
    identity = NamespaceInit(pid=4711, pidfd=91)
    if fail_communicate:
        with pytest.raises(RuntimeError, match="retained communicate failure"):
            finish_owned(cast(subprocess.Popen[str], parent), identity)
    else:
        finish_owned(cast(subprocess.Popen[str], parent), identity)
    assert signals == [(91, signal.SIGKILL)]
    assert closed == [91]
    assert parent.communicated


def test_reaped_pidfd_does_not_fall_back_to_numeric_signalling(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    child = subprocess.Popen(
        [PYTHON, "-c", "import sys; sys.stdin.buffer.read(1)"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    pidfd: int | None = None
    try:
        pidfd = os.pidfd_open(child.pid)
        child.communicate(input="x", timeout=5)
        assert child.returncode == 0
        with pytest.raises(ProcessLookupError):
            signal.pidfd_send_signal(pidfd, 0)

        def no_numeric_signal(_pid: int, _signal: int) -> None:
            pytest.fail("an exited pidfd must never fall back to a recycled numeric PID")

        parent = PendingParent()
        retained_fd = pidfd
        # Transfer ownership before the call: finish_owned closes even when
        # its communicate path raises, so this finally must not close it twice.
        pidfd = None
        with monkeypatch.context() as context:
            context.setattr(os, "kill", no_numeric_signal)
            finish_owned(cast(subprocess.Popen[str], parent), NamespaceInit(child.pid, retained_fd))
        assert parent.communicated
        with pytest.raises(OSError, match="Bad file descriptor"):
            os.fstat(retained_fd)
    finally:
        if pidfd is not None:
            os.close(pidfd)
        # Popen owns this child's unreaped wait status. Its kill method checks
        # poll first; once reaped, no numeric signal is sent on this path.
        if child.poll() is None:
            child.kill()
            child.communicate(timeout=5)


@pytest.mark.parametrize("mismatch", ["parent", "executable"])
def test_identity_mismatch_closes_pidfd_without_signalling(
    monkeypatch: pytest.MonkeyPatch, mismatch: str
) -> None:
    opened: list[int] = []
    real_open = os.pidfd_open

    def track_open(pid: int) -> int:
        fd = real_open(pid)
        opened.append(fd)
        return fd

    def no_signal(_pidfd: int, _signal: int) -> None:
        pytest.fail("an unverified process must not be signalled")

    monkeypatch.setattr(os, "pidfd_open", track_open)
    monkeypatch.setattr(signal, "pidfd_send_signal", no_signal)
    if mismatch == "parent":
        assert bind_direct_child(-1, os.getpid()) is None
    else:
        parent = PendingParent()
        parent.pid = os.getppid()
        monkeypatch.setattr(sys.modules[__name__], "direct_children", lambda _parent: [os.getpid()])
        monkeypatch.setattr(sys.modules[__name__], "PYTHON", "/not-the-verified-interpreter")
        with pytest.raises(AssertionError):
            namespace_init_process(cast(subprocess.Popen[str], parent))
    assert len(opened) == 1
    with pytest.raises(OSError, match="Bad file descriptor"):
        os.fstat(opened[0])


def test_primary_wait_status_is_immutable_after_pid_reuse_across_batches(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    spec = importlib.util.spec_from_file_location("namespace_init_status_control", INIT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    primary = 2
    failure = 7 << 8
    waits = iter([(primary, failure)] + [(99, 0)] * (module.REAP_BATCH_SIZE - 1) + [(primary, 0), None])

    class Waits:
        WNOHANG = os.WNOHANG

        @staticmethod
        def waitpid(pid: int, options: int) -> tuple[int, int]:
            assert pid == -1 and options == os.WNOHANG
            event = next(waits)
            if event is None:
                raise ChildProcessError
            return event

    monkeypatch.setattr(module, "os", Waits)
    first = module._reap(primary, None)
    assert first == (failure, True)
    assert module._reap(primary, first[0]) == (failure, False)


def test_refuses_outside_namespace_pid_one(tmp_path: Path) -> None:
    marker = tmp_path / "must-not-run"
    result = subprocess.run(
        [PYTHON, str(INIT), "--", PYTHON, "-c", f"open({str(marker)!r}, 'w').close()"],
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.returncode == 2
    assert "refused: must run as namespace PID 1" in result.stderr
    assert not marker.exists()


@pytest.mark.parametrize("exit_code", [0, 7, 42])
def test_preserves_primary_status(namespace_argv: list[str], exit_code: int) -> None:
    result = run_namespace(namespace_argv, f"raise SystemExit({exit_code})")
    assert result.returncode == exit_code, result.stderr


def test_preserves_primary_signal_failure(namespace_argv: list[str]) -> None:
    result = run_namespace(namespace_argv, "import os, signal; os.kill(os.getpid(), signal.SIGTERM)")
    assert result.returncode == 128 + signal.SIGTERM, result.stderr


def test_adopted_status_does_not_replace_primary_failure(namespace_argv: list[str]) -> None:
    result = run_namespace(
        namespace_argv,
        """
import os, time
parent = os.fork()
if parent == 0:
    if os.fork() == 0:
        os._exit(12)
    os._exit(0)
os.waitpid(parent, 0)
time.sleep(0.05)
raise SystemExit(7)
""",
    )
    assert result.returncode == 7, result.stderr


def test_reaps_adopted_orphans_during_primary_execution(namespace_argv: list[str]) -> None:
    result = run_namespace(
        namespace_argv,
        """
from pathlib import Path
import os, time
ancestors = set()
ancestor = os.getpid()
while ancestor != 1:
    ancestors.add(ancestor)
    status = (Path('/proc') / str(ancestor) / 'status').read_text()
    parent_line = next(line for line in status.splitlines() if line.startswith('PPid:'))
    ancestor = int(parent_line.split()[1])
for index in range(300):
    parent = os.fork()
    if parent == 0:
        if os.fork() == 0:
            os._exit(0)
        os._exit(0)
    _, status = os.waitpid(parent, 0)
    assert os.waitstatus_to_exitcode(status) == 0, ('intermediate child failed', index, status)
deadline = time.monotonic() + 3
while True:
    adopted = []
    for entry in Path('/proc').iterdir():
        if not entry.name.isdecimal() or int(entry.name) in ancestors:
            continue
        try:
            status = (entry / 'status').read_text()
        except (FileNotFoundError, ProcessLookupError):
            continue
        if 'PPid:\\t1\\n' in status:
            adopted.append(entry.name)
    if not adopted:
        break
    assert time.monotonic() < deadline, adopted
    time.sleep(0.01)
print('300 adopted children reaped while primary remained alive')
""",
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "300 adopted children reaped while primary remained alive"


def test_live_setsid_orphan_survives_until_primary_exits(
    namespace_argv: list[str], tmp_path: Path
) -> None:
    stopped = tmp_path / "orphan-stopped"
    result = run_namespace(
        namespace_argv,
        f"""
from pathlib import Path
import os, signal, time
read_fd, write_fd = os.pipe()
parent = os.fork()
if parent == 0:
    os.close(read_fd)
    if os.fork() == 0:
        os.setsid()
        def stop(_signum, _frame):
            Path({str(stopped)!r}).write_text('terminated after primary exit')
            os._exit(0)
        signal.signal(signal.SIGTERM, stop)
        os.write(write_fd, str(os.getpid()).encode())
        os.close(write_fd)
        while True:
            time.sleep(1)
    os.close(write_fd)
    os._exit(0)
os.close(write_fd)
orphan = int(os.read(read_fd, 64))
os.close(read_fd)
os.waitpid(parent, 0)
time.sleep(0.15)
os.kill(orphan, 0)
assert not Path({str(stopped)!r}).exists(), 'init killed a live fixture'
print('live setsid orphan survived until primary exit')
""",
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "live setsid orphan survived until primary exit"
    assert stopped.read_text() == "terminated after primary exit"


def test_immediate_primary_cancellation_cannot_become_success(namespace_argv: list[str]) -> None:
    result = run_namespace(namespace_argv, "import os, signal; os.kill(1, signal.SIGTERM)")
    assert result.returncode == 128 + signal.SIGTERM, result.stderr
    assert "cancellation signal 15" in result.stderr


@pytest.mark.parametrize("catches_as_zero", [False, True])
def test_cancellation_reaches_quiet_primary_and_cannot_become_success(
    namespace_argv: list[str], tmp_path: Path, catches_as_zero: bool
) -> None:
    ready = tmp_path / "ready"
    code = f"""
from pathlib import Path
import os, signal, time
signal.signal(signal.SIGTERM, (lambda _s, _f: os._exit(0)) if {catches_as_zero!r} else signal.SIG_IGN)
Path({str(ready)!r}).touch()
while True:
    time.sleep(1)
"""
    process = subprocess.Popen([*namespace_argv, code], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    init: NamespaceInit | None = None
    try:
        wait_ready(process, ready)
        init = namespace_init_process(process)
        # Repeated cancellation must not restart the grace period. A quiet
        # process must terminate while signals continue, not after they stop.
        deadline = time.monotonic() + 5
        while process.poll() is None and time.monotonic() < deadline:
            try:
                signal.pidfd_send_signal(init.pidfd, signal.SIGTERM)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        stdout, stderr = process.communicate(timeout=1)
        expected = 128 + (signal.SIGTERM if catches_as_zero else signal.SIGKILL)
        assert process.returncode == expected, (stdout, stderr)
        assert "cancellation signal 15" in stderr
    finally:
        finish_owned(process, init)


def test_later_cancellation_preserves_primary_failure_during_orphan_cleanup(
    namespace_argv: list[str], tmp_path: Path
) -> None:
    ready = tmp_path / "ready"
    cleanup_started = tmp_path / "cleanup-started"
    code = f"""
from pathlib import Path
import os, signal, time
parent = os.fork()
if parent == 0:
    if os.fork() == 0:
        os.setsid()
        notified = False
        def cancel_after_cleanup(_signum, _frame):
            global notified
            if notified:
                return
            notified = True
            Path({str(cleanup_started)!r}).touch()
            os.kill(1, signal.SIGTERM)
        signal.signal(signal.SIGTERM, cancel_after_cleanup)
        Path({str(ready)!r}).touch()
        while True:
            time.sleep(1)
    os._exit(0)
os.waitpid(parent, 0)
while not Path({str(ready)!r}).exists():
    time.sleep(0.01)
raise SystemExit(7)
"""
    # The orphan can receive this SIGTERM only after init has collected the
    # primary failure and entered orphan cleanup. Its handler then delivers the
    # later cancellation without relying on load-sensitive host-side polling.
    result = run_namespace(namespace_argv, code)
    assert cleanup_started.exists(), result.stderr
    assert result.returncode == 7, (result.stdout, result.stderr)
    assert "cancellation signal 15" in result.stderr
