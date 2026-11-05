"""Adversarial tests for tick-hub's subprocess boundary."""

from __future__ import annotations

import os
import select
import shlex
import signal
import subprocess
import sys
import textwrap
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

from tick_hub.probes import SubprocessGateRunner


def _process_is_live(pid: int) -> bool:
    try:
        stat = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
    except OSError:
        return False
    return stat[stat.rfind(")") + 2 :].split()[0] != "Z"


def _read_pid(path: Path) -> int | None:
    try:
        return int(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _wait_for_pids(*paths: Path) -> tuple[int, ...]:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        pids = tuple(_read_pid(path) for path in paths)
        if all(pid is not None for pid in pids):
            return tuple(pid for pid in pids if pid is not None)
        time.sleep(0.01)
    raise AssertionError(f"gate did not publish PIDs: {paths}")


def _kill_pinned_process(pidfd: int | None) -> None:
    if pidfd is None:
        return
    try:
        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        select.select([pidfd], [], [], 2)
    except ProcessLookupError:
        pass
    finally:
        os.close(pidfd)


def _wait_for_process_reaped(pid: int) -> None:
    deadline = time.monotonic() + 2
    process_path = Path(f"/proc/{pid}")
    while process_path.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert not process_path.exists()


def test_gate_runner_replaces_invalid_utf8() -> None:
    result = SubprocessGateRunner(timeout=2).run("printf '\\377'")
    assert result.ok is True
    assert result.returncode == 0
    assert result.stdout == "\ufffd"


@pytest.mark.skipif(os.name != "posix", reason="signal-handler ownership is POSIX-specific")
@pytest.mark.parametrize(("command", "timeout"), [("true", 2), ("sleep 30", 0)])
def test_gate_runner_restores_signal_handlers(command: str, timeout: int) -> None:
    watched = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)
    before = tuple(signal.getsignal(signum) for signum in watched)
    try:
        SubprocessGateRunner(timeout=timeout).run(command)
        after = tuple(signal.getsignal(signum) for signum in watched)
    finally:
        for signum, previous in zip(watched, before):
            signal.signal(signum, previous)

    assert after == before


def test_gate_runner_remains_usable_from_a_worker_thread() -> None:
    with ThreadPoolExecutor(max_workers=1) as pool:
        result = pool.submit(SubprocessGateRunner(timeout=2).run, "true").result(timeout=3)

    assert result.ok is True
    assert result.returncode == 0


@pytest.mark.skipif(sys.platform != "linux", reason="process liveness is read from Linux /proc")
def test_gate_timeout_kills_background_descendants(tmp_path: Path) -> None:
    pid_file = tmp_path / "child.pid"
    command = f"sleep 30 & echo $! > {shlex.quote(str(pid_file))}; sleep 30"
    started = time.monotonic()
    result = SubprocessGateRunner(timeout=1).run(command)
    elapsed = time.monotonic() - started

    assert result.ok is False
    assert result.error == "timed out after 1s"
    assert elapsed < 3
    child_pid = int(pid_file.read_text(encoding="utf-8"))

    deadline = time.monotonic() + 1
    while _process_is_live(child_pid) and time.monotonic() < deadline:
        time.sleep(0.01)
    if _process_is_live(child_pid):
        os.kill(child_pid, signal.SIGKILL)
        raise AssertionError("timed-out gate left a live background descendant")


@pytest.mark.skipif(sys.platform != "linux", reason="process liveness is read from Linux /proc")
def test_external_sigterm_returns_promptly_when_descendant_escapes_gate_group(
    tmp_path: Path,
) -> None:
    gate_pid_file = tmp_path / "gate.pid"
    child_pid_file = tmp_path / "child.pid"
    reaped_file = tmp_path / "reaped"
    result_file = tmp_path / "result"
    escaped_command = f"echo $$ > {shlex.quote(str(child_pid_file))}; exec sleep 30"
    command = (
        f"echo $$ > {shlex.quote(str(gate_pid_file))}; "
        f"setsid sh -c {shlex.quote(escaped_command)} & wait"
    )
    harness = textwrap.dedent(
        f"""
        import os
        import signal
        from pathlib import Path
        from tick_hub.probes import SubprocessGateRunner

        real_raise_signal = signal.raise_signal

        def assert_gate_reaped(signum):
            gate_pid = int(Path({str(gate_pid_file)!r}).read_text(encoding="utf-8"))
            try:
                waited_pid, _status = os.waitpid(gate_pid, os.WNOHANG)
            except ChildProcessError:
                outcome = "reaped"
            else:
                outcome = f"waited:{{waited_pid}}"
            Path({str(reaped_file)!r}).write_text(outcome, encoding="utf-8")
            real_raise_signal(signum)

        signal.raise_signal = assert_gate_reaped
        result = SubprocessGateRunner(timeout=30).run({command!r})
        Path({str(result_file)!r}).write_text(repr(result), encoding="utf-8")
        """
    )
    process = subprocess.Popen(
        [sys.executable, "-c", harness],
        env={**os.environ, "PYTHONPATH": str(Path(__file__).resolve().parents[1])},
        start_new_session=True,
    )
    gate_pid: int | None = None
    child_pid: int | None = None
    gate_pidfd: int | None = None
    child_pidfd: int | None = None
    try:
        gate_pid, child_pid = _wait_for_pids(gate_pid_file, child_pid_file)
        gate_pidfd = os.pidfd_open(gate_pid)
        child_pidfd = os.pidfd_open(child_pid)

        process.send_signal(signal.SIGTERM)
        assert process.wait(timeout=3) == -signal.SIGTERM

        _wait_for_process_reaped(gate_pid)
        assert reaped_file.read_text(encoding="utf-8") == "reaped"
        assert not result_file.exists()
        assert _process_is_live(child_pid)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=3)
        _kill_pinned_process(gate_pidfd)
        _kill_pinned_process(child_pidfd)


@pytest.mark.skipif(sys.platform != "linux", reason="process liveness is read from Linux /proc")
def test_parallel_gate_scope_reaps_every_gate_on_sigterm(tmp_path: Path) -> None:
    first_pid_file = tmp_path / "first.pid"
    second_pid_file = tmp_path / "second.pid"
    first = f"echo $$ > {shlex.quote(str(first_pid_file))}; exec sleep 30"
    second = f"echo $$ > {shlex.quote(str(second_pid_file))}; exec sleep 30"
    harness = textwrap.dedent(
        f"""
        from tick_hub.probes import SubprocessGateRunner
        from concurrent.futures import ThreadPoolExecutor
        runner = SubprocessGateRunner(timeout=30)
        with ThreadPoolExecutor(max_workers=2) as executor:
            with runner.parallel_scope():
                futures = [executor.submit(runner.run, command) for command in ({first!r}, {second!r})]
                [future.result() for future in futures]
        """
    )
    process = subprocess.Popen(
        [sys.executable, "-c", harness],
        env={**os.environ, "PYTHONPATH": str(Path(__file__).resolve().parents[1])},
        start_new_session=True,
    )
    pids: tuple[int, ...] = ()
    pidfds: tuple[int, ...] = ()
    try:
        pids = _wait_for_pids(first_pid_file, second_pid_file)
        pidfds = tuple(os.pidfd_open(pid) for pid in pids)
        process.send_signal(signal.SIGTERM)
        assert process.wait(timeout=3) == -signal.SIGTERM
        for pid in pids:
            _wait_for_process_reaped(pid)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=3)
        for pidfd in pidfds:
            _kill_pinned_process(pidfd)
