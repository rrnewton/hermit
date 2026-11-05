"""One-command coordinator launch from an existing Herdr shell pane."""

from __future__ import annotations

import ctypes
import json
import os
import selectors
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Sequence
from pathlib import Path
from typing import IO, cast

import pytest

import agentctl.chat as chat_module
from agentctl.agent import Target
from agentctl.chat import Config, _launch_config, _launch_here
from agentctl.client import AgentPaneInfo


class LaunchClient:
    def __init__(self, *, agent: str | None = None, workspace: str = "w7") -> None:
        self.agent = agent
        self.workspace = workspace

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        assert pane_id == "w7:p3"
        return AgentPaneInfo(pane_id, self.workspace, "/work/project", self.agent,
                             "unknown", None, None)

    def workspace_label(self, workspace_id: str) -> str:
        assert workspace_id == "w7"
        return "project-space"


def _authority() -> dict[str, object]:
    return {
        "space": "spaces/test",
        "allowed_senders": ["users/owner"],
        "transport_command": ["/opt/chat-adapter"],
        "ack_reaction": "🤖",
    }


def _gone(pid: int) -> bool:
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rpartition(") ")[2][:1]
    except (FileNotFoundError, ProcessLookupError):
        return True
    return state == "Z"


def _dispose(child: chat_module._LaunchChild) -> int:
    try:
        return (
            chat_module._stop_launch_group(child)
            if child.group_leader
            else chat_module._stop_launch_child(child)
        )
    finally:
        chat_module._close_launch_child(child)


def _prepare_launch(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> tuple[Path, Config, list[tuple[Path, Config, str | None]]]:
    config_path = tmp_path / "chat.json"
    config_path.write_text(json.dumps(_authority()), encoding="utf-8")
    config_path.chmod(0o600)
    target = Target(
        pane_id="w7:p3", expected_agent="codex",
        expected_cwd="/work/project", expected_workspace="project-space",
    )
    config = Config("spaces/test", ("users/owner",), target, "coordinator")
    monkeypatch.setattr(chat_module, "_launch_config", lambda *args, **kwargs: config)
    (tmp_path / "state").mkdir()
    initialized: list[tuple[Path, Config, str | None]] = []
    monkeypatch.setattr(
        chat_module.Bridge, "initialize",
        lambda state, value, after=None: initialized.append((state, value, after)),
    )
    return config_path, config, initialized


def test_launch_config_binds_current_pane_and_uses_model_label(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("HERDR_ENV", "1")
    monkeypatch.setenv("HERDR_PANE_ID", "w7:p3")
    monkeypatch.setenv("HERDR_WORKSPACE_ID", "w7")
    monkeypatch.setattr("agentctl.chat.shutil.which", lambda command: "/bin/" + command)
    document = _authority()
    document.update({"agent_name": "stale-name", "target": {"pane_id": "stale"},
                     "agent_label": "stale-label"})

    config = _launch_config(document, LaunchClient(), harness="codex",
                            model="gpt-6-astra", agent_label=None)

    assert config.agent_name is None
    assert config.agent_label == "gpt-6-astra"
    assert config.target == Target(pane_id="w7:p3", expected_agent="codex",
                                   expected_cwd="/work/project", expected_workspace="project-space")
    assert document["agent_name"] == "stale-name"


@pytest.mark.parametrize(("environment", "agent", "message"), [
    ({}, None, "inside the Herdr shell pane"),
    ({"HERDR_ENV": "1", "HERDR_PANE_ID": "w7:p3", "HERDR_WORKSPACE_ID": "wrong"}, None,
     "do not match"),
    ({"HERDR_ENV": "1", "HERDR_PANE_ID": "w7:p3", "HERDR_WORKSPACE_ID": "w7"}, "codex",
     "already hosts an agent"),
])
def test_launch_config_refuses_unsafe_pane_identity(
    monkeypatch: pytest.MonkeyPatch, environment: dict[str, str], agent: str | None, message: str,
) -> None:
    for key in ("HERDR_ENV", "HERDR_PANE_ID", "HERDR_WORKSPACE_ID"):
        monkeypatch.delenv(key, raising=False)
    for key, value in environment.items():
        monkeypatch.setenv(key, value)
    monkeypatch.setattr("agentctl.chat.shutil.which", lambda command: "/bin/" + command)
    with pytest.raises(ValueError, match=message):
        _launch_config(_authority(), LaunchClient(agent=agent), harness="codex",
                       model=None, agent_label=None)


def test_launch_here_owns_bridge_for_coordinator_lifetime(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    config_path, config, initialized = _prepare_launch(monkeypatch, tmp_path)
    original_spawn = chat_module._spawn_launch_child
    children: list[chat_module._LaunchChild] = []
    requested: list[tuple[str, tuple[str, ...]]] = []
    bridge_marker = tmp_path / "bridge-started"

    def spawn(
        name: str, command: Sequence[str], *, group_leader: bool,
        stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
    ) -> chat_module._LaunchChild:
        requested.append((name, tuple(command)))
        fixture = (
            [
                sys.executable, "-c",
                f"from pathlib import Path;import time;Path({str(bridge_marker)!r}).touch();time.sleep(60)",
            ]
            if name == "bridge"
            else [
                sys.executable, "-c",
                f"import os,time;path={str(bridge_marker)!r};"
                "\nwhile not os.path.exists(path): time.sleep(.001)"
                "\nraise SystemExit(0)",
            ]
        )
        child = original_spawn(
            name, fixture, group_leader=group_leader,
            stdin=stdin, stdout=stdout, stderr=stderr,
        )
        children.append(child)
        return child

    monkeypatch.setattr(chat_module, "_spawn_launch_child", spawn)
    result = _launch_here(
        tmp_path / "state", config_path, harness="codex", model="gpt-6-astra",
        resume=None, harness_args=("--personality", "pragmatic"), agent_label=None,
        after=None, interval=3, reconcile_interval=300, prog="agentctl chat",
    )
    assert result == 0
    assert initialized == [(tmp_path / "state", config, None)]
    assert requested[0][0] == "bridge" and list(requested[0][1]) == [
        sys.executable, str(Path(chat_module.__file__).resolve()), "run",
        "--state", str((tmp_path / "state").absolute()), "--interval", "3",
        "--reconcile-interval", "300", "--observer-write-interval", "60.0",
    ]
    assert requested[1] == (
        "coordinator",
        ("codex", "--no-alt-screen", "--model", "gpt-6-astra", "--personality", "pragmatic"),
    )
    assert bridge_marker.exists()
    assert len(children) == 2 and all(child.reaped and _gone(child.process.pid) for child in children)
    for child in children:
        with pytest.raises(OSError):
            os.fstat(child.pidfd)


def test_launch_pidfd_wait_reports_bridge_exit_without_process_polling(
) -> None:
    bridge = chat_module._spawn_launch_child(
        "bridge", [sys.executable, "-c", "raise SystemExit(7)"], group_leader=True,
    )
    coordinator = chat_module._spawn_launch_child(
        "coordinator", [sys.executable, "-c", "import time;time.sleep(60)"],
        group_leader=False,
    )
    terminate_read, terminate_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        chat_module._activate_launch_child(bridge)
        chat_module._activate_launch_child(coordinator)
        completed, result = chat_module._wait_launch_processes(
            bridge, coordinator, terminate_read,
        )
        assert (completed, result) == ("bridge", 7)
        assert not bridge.reaped
    finally:
        os.close(terminate_read)
        os.close(terminate_write)
        assert _dispose(coordinator) in (-signal.SIGTERM, -signal.SIGKILL)
        assert _dispose(bridge) == 7


def test_launch_real_children_coalesce_batch_boundary_in_favour_of_coordinator(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    bridge_release = tmp_path / "bridge-release"
    coordinator_release = tmp_path / "coordinator-release"
    program = (
        "import os,sys,time;path=sys.argv[1];"
        "\nwhile not os.path.exists(path): time.sleep(.001)"
        "\nraise SystemExit(int(sys.argv[2]))"
    )
    bridge = chat_module._spawn_launch_child(
        "bridge", [sys.executable, "-c", program, str(bridge_release), "7"],
        group_leader=True,
    )
    coordinator = chat_module._spawn_launch_child(
        "coordinator", [sys.executable, "-c", program, str(coordinator_release), "13"],
        group_leader=False,
    )
    chat_module._activate_launch_child(bridge)
    chat_module._activate_launch_child(coordinator)
    terminate_read, terminate_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
    original_selector = selectors.DefaultSelector

    class BoundarySelector:
        def __init__(self) -> None:
            self.inner = cast(selectors.BaseSelector, original_selector())
            self.descriptors: dict[str, int] = {}
            self.first = True

        def register(
            self, descriptor: int, events: int, data: object = None,
        ) -> object:
            assert isinstance(data, str)
            self.descriptors[data] = descriptor
            return self.inner.register(descriptor, events, data)

        def unregister(self, descriptor: int) -> object:
            return self.inner.unregister(descriptor)

        def select(
            self, timeout: float | None = None,
        ) -> list[tuple[object, int]]:
            if self.first:
                self.first = False
                bridge_release.touch()
                ready = self.inner.select(timeout)
                assert [key.data for key, _ in ready] == ["bridge"]
                coordinator_release.touch()
                proof = original_selector()
                try:
                    proof.register(self.descriptors["coordinator"], selectors.EVENT_READ)
                    assert proof.select(5)
                finally:
                    proof.close()
                return cast(list[tuple[object, int]], ready)
            return cast(list[tuple[object, int]], self.inner.select(timeout))

        def close(self) -> None:
            self.inner.close()

    try:
        with monkeypatch.context() as patch:
            patch.setattr(selectors, "DefaultSelector", BoundarySelector)
            completed, result = chat_module._wait_launch_processes(
                bridge, coordinator, terminate_read,
            )
            assert (completed, result) == ("coordinator", 13)
    finally:
        os.close(terminate_read)
        os.close(terminate_write)
        _dispose(coordinator)
        _dispose(bridge)


def test_launch_selector_registration_failure_closes_every_pidfd(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    before = len(list(Path("/proc/self/fd").iterdir()))
    bridge = chat_module._spawn_launch_child(
        "bridge", [sys.executable, "-c", "import time;time.sleep(60)"], group_leader=True,
    )
    coordinator = chat_module._spawn_launch_child(
        "coordinator", [sys.executable, "-c", "import time;time.sleep(60)"],
        group_leader=False,
    )
    chat_module._activate_launch_child(bridge)
    chat_module._activate_launch_child(coordinator)
    terminate_read, terminate_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
    original_selector = selectors.DefaultSelector

    class FailingSelector:
        def __init__(self) -> None:
            self.inner = cast(selectors.BaseSelector, original_selector())
            self.registrations = 0

        def register(
            self, descriptor: int, events: int, data: object = None,
        ) -> object:
            self.registrations += 1
            if self.registrations == 2:
                raise OSError("synthetic registration exhaustion")
            return self.inner.register(descriptor, events, data)

        def select(self, timeout: float | None = None) -> list[tuple[object, int]]:
            return cast(list[tuple[object, int]], self.inner.select(timeout))

        def close(self) -> None:
            self.inner.close()

    try:
        with monkeypatch.context() as patch:
            patch.setattr(selectors, "DefaultSelector", FailingSelector)
            with pytest.raises(OSError, match="registration exhaustion"):
                chat_module._wait_launch_processes(bridge, coordinator, terminate_read)
    finally:
        os.close(terminate_read)
        os.close(terminate_write)
        _dispose(coordinator)
        _dispose(bridge)
    assert len(list(Path("/proc/self/fd").iterdir())) == before


@pytest.mark.parametrize("popen_window", [1, 2])
def test_launch_sigterm_after_os_spawn_before_popen_return_is_deferred_until_tracked(
    popen_window: int, monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    config_path, _, _ = _prepare_launch(monkeypatch, tmp_path)
    prior = signal.getsignal(signal.SIGTERM)
    original_popen = subprocess.Popen
    original_spawn = chat_module._spawn_launch_child
    spawned: list[subprocess.Popen[bytes]] = []
    tracked: list[chat_module._LaunchChild] = []
    bridge_marker = tmp_path / "bridge-provider-started"
    coordinator_marker = tmp_path / "coordinator-provider-started"

    def popen(
        args: Sequence[str], *, stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
        start_new_session: bool = False, pass_fds: Sequence[int] = (),
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, pass_fds=pass_fds,
        )
        spawned.append(process)
        if len(spawned) == popen_window:
            os.kill(os.getpid(), signal.SIGTERM)
        return process

    def spawn(
        name: str, command: Sequence[str], *, group_leader: bool,
        stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
    ) -> chat_module._LaunchChild:
        marker = bridge_marker if name == "bridge" else coordinator_marker
        fixture = [
            sys.executable, "-c",
            f"from pathlib import Path;import time;Path({str(marker)!r}).touch();time.sleep(60)",
        ]
        child = original_spawn(
            name, fixture, group_leader=group_leader,
            stdin=stdin, stdout=stdout, stderr=stderr,
        )
        tracked.append(child)
        return child

    monkeypatch.setattr(subprocess, "Popen", popen)
    monkeypatch.setattr(chat_module, "_spawn_launch_child", spawn)
    with pytest.raises(chat_module._ServiceTerminated):
        _launch_here(
            tmp_path / "state", config_path, harness="codex", model=None,
            resume=None, harness_args=(), agent_label=None, after=None,
            interval=3600, reconcile_interval=300, prog="agentctl chat",
        )
    assert len(spawned) == popen_window and len(tracked) == popen_window
    assert all(process.returncode is not None and _gone(process.pid) for process in spawned)
    assert not (bridge_marker if popen_window == 1 else coordinator_marker).exists()
    assert signal.getsignal(signal.SIGTERM) is prior


def test_launch_steady_sigterm_wakes_pidfd_selector_through_self_pipe(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    config_path, _, _ = _prepare_launch(monkeypatch, tmp_path)
    original_spawn = chat_module._spawn_launch_child
    children: list[chat_module._LaunchChild] = []

    def spawn(
        name: str, command: Sequence[str], *, group_leader: bool,
        stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
    ) -> chat_module._LaunchChild:
        child = original_spawn(
            name, [sys.executable, "-c", "import time;time.sleep(60)"],
            group_leader=group_leader, stdin=stdin, stdout=stdout, stderr=stderr,
        )
        children.append(child)
        return child

    monkeypatch.setattr(chat_module, "_spawn_launch_child", spawn)
    timer = threading.Timer(0.1, os.kill, args=(os.getpid(), signal.SIGTERM))
    timer.start()
    try:
        with pytest.raises(chat_module._ServiceTerminated):
            _launch_here(
                tmp_path / "state", config_path, harness="codex", model=None,
                resume=None, harness_args=(), agent_label=None, after=None,
                interval=3600, reconcile_interval=300, prog="agentctl chat",
            )
    finally:
        timer.cancel()
        timer.join()
    assert len(children) == 2
    assert all(child.reaped and _gone(child.process.pid) for child in children)


def test_launch_pidfd_failure_closes_gate_before_product_exec(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    config_path, _, _ = _prepare_launch(monkeypatch, tmp_path)
    marker = tmp_path / "product-command-started"
    original_open = os.pidfd_open
    original_popen = subprocess.Popen
    original_spawn = chat_module._spawn_launch_child
    spawned: list[subprocess.Popen[bytes]] = []

    def open_descriptor(pid: int, flags: int = 0) -> int:
        if pid == os.getpid():
            return original_open(pid, flags)
        raise OSError("descriptor exhausted after gate spawn")

    def popen(
        args: Sequence[str], *, stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
        start_new_session: bool = False, pass_fds: Sequence[int] = (),
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, pass_fds=pass_fds,
        )
        spawned.append(process)
        return process

    def spawn(
        name: str, command: Sequence[str], *, group_leader: bool,
        stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
    ) -> chat_module._LaunchChild:
        fixture = [
            sys.executable, "-c",
            f"from pathlib import Path;Path({str(marker)!r}).touch()",
        ]
        return original_spawn(
            name, fixture, group_leader=group_leader,
            stdin=stdin, stdout=stdout, stderr=stderr,
        )

    monkeypatch.setattr(os, "pidfd_open", open_descriptor)
    monkeypatch.setattr(subprocess, "Popen", popen)
    monkeypatch.setattr(chat_module, "_spawn_launch_child", spawn)
    with pytest.raises(ValueError, match="bridge process descriptor"):
        _launch_here(
            tmp_path / "state", config_path, harness="codex", model=None,
            resume=None, harness_args=(), agent_label=None, after=None,
            interval=3600, reconcile_interval=300, prog="agentctl chat",
        )
    assert len(spawned) == 1 and spawned[0].returncode == -signal.SIGKILL
    assert not Path(f"/proc/{spawned[0].pid}").exists()
    assert not marker.exists()


def test_launch_waits_for_gate_lifeline_readiness_before_return(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    original_popen = subprocess.Popen
    marker = tmp_path / "product-started"
    delay = 0.2
    delay_program = (
        "import runpy,sys,time;"
        "time.sleep(float(sys.argv[1]));"
        "path=sys.argv[2];"
        "sys.argv=[path,*sys.argv[3:]];"
        "runpy.run_path(path,run_name='__main__')"
    )

    def delayed_popen(
        args: Sequence[str], *, stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
        start_new_session: bool = False, pass_fds: Sequence[int] = (),
    ) -> subprocess.Popen[bytes]:
        delayed = (
            args[0], "-c", delay_program, str(delay), args[1], *args[2:],
        )
        return original_popen(
            delayed, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, pass_fds=pass_fds,
        )

    monkeypatch.setattr(subprocess, "Popen", delayed_popen)
    started = time.monotonic()
    child = chat_module._spawn_launch_child(
        "coordinator",
        [
            sys.executable, "-c",
            f"from pathlib import Path;Path({str(marker)!r}).touch()",
        ],
        group_leader=False,
    )
    elapsed = time.monotonic() - started
    try:
        assert elapsed >= delay * 0.75
        assert not marker.exists()
        chat_module._activate_launch_child(child)
        assert chat_module._wait_launch_fd(child.pidfd, 5)
        assert chat_module._reap_launch_child(child) == 0
        assert marker.exists()
    finally:
        if not child.reaped:
            chat_module._stop_launch_child(child)
        chat_module._close_launch_child(child)


def test_launch_repeated_pidfd_exhaustion_reaps_stopped_gates_without_leaks(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    original_popen = subprocess.Popen
    before_fds = len(list(Path("/proc/self/fd").iterdir()))
    before_tasks = len(list(Path("/proc/self/task").iterdir()))
    marker = tmp_path / "must-not-start"
    processes: list[subprocess.Popen[bytes]] = []
    pidfd_calls: list[int] = []
    signals: list[tuple[int, int]] = []
    original_kill = os.kill

    def unavailable(pid: int, flags: int = 0) -> int:
        pidfd_calls.append(pid)
        raise OSError(24, "injected process descriptor exhaustion")

    def record_signal(pid: int, signum: int) -> None:
        signals.append((pid, signum))
        original_kill(pid, signum)

    def stopped_popen(
        args: Sequence[str], *, stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
        start_new_session: bool = False, pass_fds: Sequence[int] = (),
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, pass_fds=pass_fds,
        )
        processes.append(process)
        os.kill(process.pid, signal.SIGSTOP)
        deadline = time.monotonic() + 2
        while not _gone(process.pid):
            state = Path(f"/proc/{process.pid}/stat").read_text().rpartition(") ")[2][:1]
            if state in ("T", "t"):
                break
            if time.monotonic() >= deadline:
                pytest.fail("launch gate did not stop before pidfd exhaustion")
            time.sleep(0.005)
        return process

    monkeypatch.setattr(os, "pidfd_open", unavailable)
    monkeypatch.setattr(os, "kill", record_signal)
    monkeypatch.setattr(subprocess, "Popen", stopped_popen)
    for _ in range(20):
        with pytest.raises(ValueError, match="process descriptor"):
            chat_module._spawn_launch_child(
                "coordinator",
                [
                    sys.executable, "-c",
                    f"from pathlib import Path;Path({str(marker)!r}).touch()",
                ],
                group_leader=False,
            )
    assert pidfd_calls == [pid for process in processes for pid in (process.pid, process.pid)]
    assert signals == [
        item for process in processes for item in (
            (process.pid, signal.SIGSTOP),
            (process.pid, signal.SIGCONT),
            (process.pid, signal.SIGKILL),
        )
    ]
    assert all(process.returncode == -signal.SIGKILL for process in processes)
    assert all(not Path(f"/proc/{process.pid}").exists() for process in processes)
    assert not marker.exists()
    assert len(list(Path("/proc/self/fd").iterdir())) == before_fds
    assert len(list(Path("/proc/self/task").iterdir())) == before_tasks


def test_launch_armed_stopped_gate_disappears_when_caller_dies(tmp_path: Path) -> None:
    record = tmp_path / "armed-gate.pid"
    marker = tmp_path / "must-not-start"
    package_parent = Path(chat_module.__file__).resolve().parent.parent
    probe = r'''
import ctypes
import os
import pathlib
import signal
import subprocess
import sys
import time

libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
record = pathlib.Path(sys.argv[1])
marker = pathlib.Path(sys.argv[2])
package_parent = sys.argv[3]
caller_program = r"""
import os
import pathlib
import signal
import sys
import time
sys.path.insert(0, sys.argv[3])
from agentctl.chat import _spawn_launch_child
record = pathlib.Path(sys.argv[1])
marker = pathlib.Path(sys.argv[2])
gate = _spawn_launch_child(
    "coordinator",
    [sys.executable, "-c", "import pathlib,sys;pathlib.Path(sys.argv[1]).touch()", str(marker)],
    group_leader=False,
)
os.kill(gate.process.pid, signal.SIGSTOP)
deadline = time.monotonic() + 2
while pathlib.Path(f"/proc/{gate.process.pid}/stat").read_bytes().rpartition(b") ")[2][:1] != b"T":
    if time.monotonic() >= deadline:
        raise RuntimeError("armed launch gate did not stop")
    time.sleep(0.005)
pending = record.with_suffix(".pending")
pending.write_text(str(gate.process.pid))
pending.replace(record)
time.sleep(60)
"""
caller = subprocess.Popen(
    (sys.executable, "-c", caller_program, str(record), str(marker), package_parent),
)
deadline = time.monotonic() + 5
while not record.exists() and time.monotonic() < deadline:
    if caller.poll() is not None:
        break
    time.sleep(0.01)
if not record.exists():
    outcome = caller.poll()
    if outcome is None:
        caller.kill()
        caller.wait(timeout=2)
    raise SystemExit(f"caller did not publish an armed stopped gate: {outcome}")
gate_pid = int(record.read_text())
os.kill(caller.pid, signal.SIGKILL)
caller.wait(timeout=2)
status = None
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    try:
        reaped, candidate = os.waitpid(gate_pid, os.WNOHANG)
    except ChildProcessError:
        time.sleep(0.01)
        continue
    if reaped == gate_pid:
        status = candidate
        break
    time.sleep(0.01)
if status is None:
    try:
        os.kill(gate_pid, signal.SIGCONT)
        os.kill(gate_pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    raise SystemExit("armed stopped gate survived caller death")
if os.waitstatus_to_exitcode(status) != -signal.SIGKILL:
    raise SystemExit(f"armed gate parent-death status was {os.waitstatus_to_exitcode(status)}")
remaining = [pid for pid in (caller.pid, gate_pid) if pathlib.Path(f"/proc/{pid}").exists()]
if remaining:
    raise SystemExit(f"caller-death cleanup left /proc entries: {remaining}")
if marker.exists():
    raise SystemExit("launch command gained authority before activation")
'''
    completed = subprocess.run(
        (sys.executable, "-c", probe, str(record), str(marker), str(package_parent)),
        text=True, capture_output=True, timeout=12, check=False,
    )
    assert completed.returncode == 0, completed.stderr


def test_launch_post_spawn_validation_failure_kills_stopped_gate_by_pidfd(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_identity = chat_module._command_process_identity
    calls = 0
    gate_pid = 0

    def identity(pid: int) -> chat_module._CommandAnchorIdentity | None:
        nonlocal calls, gate_pid
        observed = original_identity(pid)
        calls += 1
        if calls == 2 and observed is not None:
            gate_pid = pid
            os.kill(pid, signal.SIGSTOP)
            return chat_module._CommandAnchorIdentity(
                pid=observed.pid, starttime=observed.starttime + 1,
                ppid=observed.ppid, pgrp=observed.pgrp,
                session=observed.session, state=observed.state,
            )
        return observed

    monkeypatch.setattr(chat_module, "_command_process_identity", identity)
    with pytest.raises(ValueError, match="identity changed before activation"):
        chat_module._spawn_launch_child(
            "coordinator", [sys.executable, "-c", "import time;time.sleep(60)"],
            group_leader=False,
        )
    assert gate_pid > 0 and _gone(gate_pid)


def test_launch_second_preflight_pidfd_failure_closes_first_without_spawning(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_open = os.pidfd_open
    calls = 0
    before = len(list(Path("/proc/self/fd").iterdir()))

    def open_descriptor(pid: int, flags: int = 0) -> int:
        nonlocal calls
        calls += 1
        if calls == 2:
            raise OSError("synthetic second preflight failure")
        return original_open(pid, flags)

    monkeypatch.setattr(os, "pidfd_open", open_descriptor)
    monkeypatch.setattr(
        subprocess, "Popen",
        lambda *args, **kwargs: pytest.fail("preflight failure must precede every spawn"),
    )
    with pytest.raises(ValueError, match="descriptors are unavailable"):
        chat_module._require_waitable_launch_children()
    assert len(list(Path("/proc/self/fd").iterdir())) == before


@pytest.mark.parametrize("disposition", ["ignored", "no-cldwait"])
def test_launch_refuses_child_autoreap_before_spawning(
    disposition: str, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        subprocess, "Popen",
        lambda *args, **kwargs: pytest.fail("unsafe SIGCHLD state must precede every spawn"),
    )
    prior_handler = signal.getsignal(signal.SIGCHLD)
    libc = ctypes.CDLL(None, use_errno=True)
    libc.sigaction.argtypes = [
        ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(chat_module._LinuxSigaction),
    ]
    libc.sigaction.restype = ctypes.c_int
    previous_action = chat_module._LinuxSigaction()
    assert libc.sigaction(signal.SIGCHLD, None, ctypes.byref(previous_action)) == 0
    try:
        if disposition == "ignored":
            signal.signal(signal.SIGCHLD, signal.SIG_IGN)
        else:
            action = chat_module._LinuxSigaction()
            ctypes.memmove(
                ctypes.byref(action), ctypes.byref(previous_action), ctypes.sizeof(action),
            )
            action.handler = None
            action.flags |= 2
            assert libc.sigaction(signal.SIGCHLD, ctypes.byref(action), None) == 0
        with pytest.raises(ValueError, match="SIGCHLD|SA_NOCLDWAIT"):
            chat_module._require_waitable_launch_children()
    finally:
        assert libc.sigaction(signal.SIGCHLD, ctypes.byref(previous_action), None) == 0
        signal.signal(signal.SIGCHLD, prior_handler)


def test_launch_exact_pidfd_status_reports_exit_and_signal() -> None:
    exited = chat_module._spawn_launch_child(
        "coordinator", [sys.executable, "-c", "raise SystemExit(7)"],
        group_leader=False,
    )
    killed = chat_module._spawn_launch_child(
        "coordinator", [sys.executable, "-c", "import time;time.sleep(60)"],
        group_leader=False,
    )
    try:
        chat_module._activate_launch_child(exited)
        chat_module._activate_launch_child(killed)
        assert chat_module._wait_launch_fd(exited.pidfd, 5)
        assert chat_module._reap_launch_child(exited) == 7
        chat_module._signal_launch_child(killed, signal.SIGKILL)
        assert chat_module._wait_launch_fd(killed.pidfd, 5)
        assert chat_module._reap_launch_child(killed) == -signal.SIGKILL
    finally:
        for child in (exited, killed):
            if not child.reaped:
                chat_module._stop_launch_child(child)
            chat_module._close_launch_child(child)


def test_launch_group_cleanup_refuses_changed_numeric_identity(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    child = chat_module._spawn_launch_child(
        "bridge", [sys.executable, "-c", "import time;time.sleep(60)"],
        group_leader=True,
    )
    chat_module._activate_launch_child(child)
    changed = chat_module._CommandAnchorIdentity(
        pid=child.identity.pid, starttime=child.identity.starttime + 1,
        ppid=child.identity.ppid, pgrp=child.identity.pgrp,
        session=child.identity.session, state=child.identity.state,
    )
    monkeypatch.setattr(chat_module, "_command_process_identity", lambda pid: changed)
    monkeypatch.setattr(
        os, "killpg",
        lambda pid, signum: pytest.fail("changed identity must never receive a numeric PGID signal"),
    )
    try:
        with pytest.raises(RuntimeError, match="identity changed"):
            chat_module._stop_launch_group(child)
        assert child.reaped and child.observed_returncode == -signal.SIGKILL
    finally:
        chat_module._close_launch_child(child)


def test_launch_group_kill_removes_forked_descendant(tmp_path: Path) -> None:
    identities = tmp_path / "identities.json"
    program = (
        "import json,os,pathlib,signal,time;"
        "child=os.fork();"
        "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
        f"path=pathlib.Path({str(identities)!r});"
        "pending=path.with_suffix('.pending');"
        "pending.write_text(json.dumps({'parent':os.getpid(),'child':child})) if child else None;"
        "pending.replace(path) if child else None;"
        "time.sleep(60)"
    )
    bridge = chat_module._spawn_launch_child(
        "bridge", [sys.executable, "-c", program], group_leader=True,
    )
    try:
        chat_module._activate_launch_child(bridge)
        deadline = time.monotonic() + 5
        while not identities.exists() and time.monotonic() < deadline:
            time.sleep(0.005)
        record = json.loads(identities.read_text())
        provider, descendant = int(record["parent"]), int(record["child"])
        chat_module._signal_launch_group(bridge, signal.SIGKILL)
        assert chat_module._stop_launch_group(bridge) == -signal.SIGKILL
        deadline = time.monotonic() + 2
        while not _gone(descendant) and time.monotonic() < deadline:
            time.sleep(0.005)
        assert _gone(provider) and _gone(descendant)
    finally:
        if not bridge.reaped:
            chat_module._stop_launch_group(bridge)
        chat_module._close_launch_child(bridge)


def test_launch_repeated_children_leave_no_fds_tasks_or_survivors() -> None:
    before_fds = len(list(Path("/proc/self/fd").iterdir()))
    before_tasks = len(list(Path("/proc/self/task").iterdir()))
    identifiers: list[int] = []
    for _ in range(20):
        child = chat_module._spawn_launch_child(
            "coordinator", [sys.executable, "-c", "raise SystemExit(0)"],
            group_leader=False,
        )
        identifiers.append(child.process.pid)
        chat_module._activate_launch_child(child)
        assert chat_module._wait_launch_fd(child.pidfd, 5)
        assert chat_module._reap_launch_child(child) == 0
        chat_module._close_launch_child(child)
    assert all(_gone(pid) for pid in identifiers)
    assert len(list(Path("/proc/self/fd").iterdir())) == before_fds
    assert len(list(Path("/proc/self/task").iterdir())) == before_tasks


def test_launch_help_describes_current_pane_and_every_option(capsys: pytest.CaptureFixture[str]) -> None:
    with pytest.raises(SystemExit) as result:
        chat_module.run_cli(["launch", "--help"])
    assert result.value.code == 0
    output = capsys.readouterr().out
    for text in ("this Herdr pane", "--config", "--harness", "--model", "--resume",
                 "--harness-arg", "--agent-label", "--after", "--interval",
                 "--reconcile-interval", "--observer-write-interval", "--state",
                 "0.1–86400", "60–3600"):
        assert text in output


def test_launch_accepts_one_day_poll_interval_and_rejects_larger(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str],
) -> None:
    intervals: list[float] = []

    def launch(*args: object, **kwargs: object) -> int:
        interval = kwargs["interval"]
        assert isinstance(interval, float)
        intervals.append(interval)
        return 0

    monkeypatch.setattr(chat_module, "_launch_here", launch)
    assert chat_module.run_cli([
        "launch", "--config", str(tmp_path / "chat.json"), "--interval", "86400",
    ]) == 0
    assert intervals == [86400]
    assert chat_module.run_cli([
        "launch", "--config", str(tmp_path / "chat.json"), "--interval", "86400.1",
    ]) == 1
    assert "between 0.1 and 86400 seconds" in capsys.readouterr().err


def test_launch_cli_reports_sigterm_as_clean_service_shutdown(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    def terminated(*args: object, **kwargs: object) -> int:
        raise chat_module._ServiceTerminated

    monkeypatch.setattr(chat_module, "_launch_here", terminated)
    assert chat_module.run_cli([
        "launch", "--config", str(tmp_path / "chat.json"),
    ]) == 0


def test_launch_defaults_to_hourly_polling(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    intervals: list[float] = []

    def launch(*args: object, **kwargs: object) -> int:
        interval = kwargs["interval"]
        assert isinstance(interval, float)
        intervals.append(interval)
        return 0

    monkeypatch.setattr(chat_module, "_launch_here", launch)
    assert chat_module.run_cli([
        "launch", "--config", str(tmp_path / "chat.json"),
    ]) == 0
    assert intervals == [3600.0]


def test_launch_validates_observer_write_interval(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str],
) -> None:
    intervals: list[float] = []

    def launch(*args: object, **kwargs: object) -> int:
        interval = kwargs["observer_write_interval"]
        assert isinstance(interval, float)
        intervals.append(interval)
        return 0

    monkeypatch.setattr(chat_module, "_launch_here", launch)
    for value in (60.0, 300.0, 3600.0):
        assert chat_module.run_cli([
            "launch", "--config", str(tmp_path / "chat.json"),
            "--observer-write-interval", str(value),
        ]) == 0
    assert intervals == [60, 300, 3600]
    for invalid_value in (59.9, 3600.1):
        assert chat_module.run_cli([
            "launch", "--config", str(tmp_path / "chat.json"),
            "--observer-write-interval", str(invalid_value),
        ]) == 1
    errors = capsys.readouterr().err
    assert errors.count("between 60 and 3600 seconds") == 2
