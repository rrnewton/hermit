"""Subscription construction races and process-level bridge shutdown cleanup."""

from __future__ import annotations

import fcntl
import json
import os
import queue
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Sequence
from dataclasses import replace
from pathlib import Path

import pytest

import agentctl.chat as chat_module
import agentctl.chat_runtime as runtime_module
from agentctl.chat import Bridge, _OutputSubscription, _run_bridge
from agentctl.chat_output import PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.chat_runtime import _Notice, _OutputPump
from tests.test_herdr_chat import setup


class _Stream(PaneOutputStream):
    def __init__(self) -> None:
        self.waited = threading.Event()
        self.closed = threading.Event()
        self.awakened = threading.Event()

    def wait(self, timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        self.waited.set()
        self.awakened.wait(min(timeout, 5))
        self.awakened.clear()
        return ()

    def wake(self) -> None:
        self.awakened.set()

    def close(self) -> None:
        self.closed.set()
        self.wake()


def test_output_stop_during_subscription_open_closes_without_waiting(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, _ = setup(tmp_path)
    opening, release = threading.Event(), threading.Event()
    stream = _Stream()

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        assert tuple(nonces) == ("first",)
        opening.set()
        assert release.wait(5)
        return stream

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    stop = threading.Event()
    pump = _OutputPump(bridge, queue.Queue[_Notice](), stop)
    pump.update(_OutputSubscription(bridge, ("first",), ("first",), True))
    pump.thread.start()
    try:
        assert opening.wait(2)
        stop.set()
        release.set()
        pump.thread.join(timeout=1)
        assert not pump.thread.is_alive()
        assert stream.closed.is_set()
        assert not stream.waited.is_set()
    finally:
        stop.set()
        release.set()
        stream.wake()
        pump.close()


def test_nonce_change_during_open_replaces_stale_subscription_before_waiting(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, _ = setup(tmp_path)
    opening, release, replacement = threading.Event(), threading.Event(), threading.Event()
    streams = [_Stream(), _Stream()]
    subscriptions: list[tuple[str, ...]] = []

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        subscriptions.append(tuple(nonces))
        if len(subscriptions) == 1:
            opening.set()
            assert release.wait(5)
            return streams[0]
        replacement.set()
        return streams[1]

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    stop = threading.Event()
    pump = _OutputPump(bridge, queue.Queue[_Notice](), stop)
    pump.update(_OutputSubscription(bridge, ("first",), ("first",), True))
    pump.thread.start()
    try:
        assert opening.wait(2)
        pump.update(_OutputSubscription(bridge, ("first", "second"), ("first", "second"), True))
        release.set()
        assert replacement.wait(1)
        assert subscriptions == [("first",), ("first", "second")]
        assert streams[0].closed.is_set()
        assert not streams[0].waited.is_set()
    finally:
        stop.set()
        release.set()
        for stream in streams:
            stream.wake()
        pump.close()
    assert streams[1].closed.is_set()
    assert not pump.thread.is_alive()


def test_sigterm_runs_cleanup_and_restores_previous_handler(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, _ = setup(tmp_path)
    prior = signal.getsignal(signal.SIGTERM)
    cleaned = False

    def polling(current: Bridge, interval: float, prog: str, *,
                observer_write_interval: float) -> None:
        nonlocal cleaned
        assert current is bridge
        assert observer_write_interval == 60
        assert signal.getsignal(signal.SIGTERM) is not prior
        try:
            os.kill(os.getpid(), signal.SIGTERM)
            pytest.fail("SIGTERM did not interrupt the runner")
        finally:
            cleaned = True

    monkeypatch.setattr(chat_module, "_run_polling", polling)
    with pytest.raises(chat_module._ServiceTerminated):
        _run_bridge(bridge, 3, "test-chat")
    assert cleaned
    assert signal.getsignal(signal.SIGTERM) is prior
    with (tmp_path / ".run.lock").open("rb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)


def test_event_bridge_propagates_nondefault_observer_write_interval(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, _ = setup(tmp_path)
    bridge.config = replace(bridge.config, event_command=("fixture-events",))
    calls: list[tuple[float, str, float]] = []

    def streaming(current: Bridge, *, reconcile_interval: float, prog: str,
                  observer_write_interval: float) -> None:
        assert current is bridge
        calls.append((reconcile_interval, prog, observer_write_interval))

    monkeypatch.setattr(runtime_module, "run_streaming", streaming)
    _run_bridge(bridge, 3600, "test-chat", reconcile_interval=123,
                observer_write_interval=777)
    assert calls == [(123, "test-chat", 777)]


def _gone(pid: int) -> bool:
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rpartition(") ")[2][:1]
    except (FileNotFoundError, ProcessLookupError):
        return True
    return state == "Z"


def test_sigterm_stops_real_event_adapter_and_descendant_and_releases_owner(tmp_path: Path) -> None:
    pidfile = tmp_path / "adapter-pids.json"
    child_program = (
        "import json,os,pathlib,subprocess,sys,time\n"
        "request=json.load(sys.stdin)\n"
        "child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])\n"
        "path=pathlib.Path(sys.argv[1])\n"
        "pending=path.with_suffix('.pending')\n"
        "pending.write_text(json.dumps([os.getpid(),child.pid]))\n"
        "pending.replace(path)\n"
        "print('{\"type\":\"heartbeat\"}',flush=True)\n"
        "time.sleep(60)\n"
    )
    program = (
        "import signal,sys\n"
        "from dataclasses import replace\n"
        "from pathlib import Path\n"
        "from agentctl.chat import _ServiceTerminated,_run_bridge\n"
        "from tests.test_herdr_chat import setup\n"
        "bridge,_,_=setup(Path(sys.argv[1]))\n"
        "bridge.config=replace(bridge.config,event_command=(sys.executable,'-u','-c',sys.argv[2],sys.argv[3]))\n"
        "previous=signal.getsignal(signal.SIGTERM)\n"
        "try:\n"
        "    _run_bridge(bridge,3,'test-chat')\n"
        "except _ServiceTerminated:\n"
        "    assert signal.getsignal(signal.SIGTERM) is previous\n"
        "    sys.exit(0)\n"
    )
    process = subprocess.Popen([sys.executable, "-c", program, str(tmp_path / "state"),
                                child_program, str(pidfile)], stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, cwd=Path(__file__).resolve().parents[1])
    pids: list[int] = []
    try:
        deadline = time.monotonic() + 5
        while not pidfile.exists() and time.monotonic() < deadline:
            if process.poll() is not None:
                break
            time.sleep(0.01)
        assert pidfile.exists()
        decoded = json.loads(pidfile.read_text())
        assert isinstance(decoded, list) and len(decoded) == 2
        pids = [int(pid) for pid in decoded]
        assert all(not _gone(pid) for pid in pids)
        process.terminate()
        stdout, stderr = process.communicate(timeout=5)
        assert process.returncode == 0, (stdout, stderr)
        deadline = time.monotonic() + 2
        while not all(_gone(pid) for pid in pids) and time.monotonic() < deadline:
            time.sleep(0.01)
        assert all(_gone(pid) for pid in pids)
        with (tmp_path / "state" / ".run.lock").open("rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
        process.communicate(timeout=3)
        for pid in pids:
            if not _gone(pid):
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
