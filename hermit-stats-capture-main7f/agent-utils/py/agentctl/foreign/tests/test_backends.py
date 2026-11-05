from __future__ import annotations

import json
import os
import shlex
import signal
import subprocess
import sys
from pathlib import Path
from typing import Iterator

import pytest

from agentctl.foreign import agent_keeper, agent_runner, lib
from agentctl.client import AgentPaneInfo
from agentctl.errors import HerdrUnavailable


@pytest.fixture()
def fake_backend_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    base = tmp_path / "subagents"
    state = base / "state"
    monkeypatch.setattr(lib, "BASE", base)
    monkeypatch.setattr(lib, "STATE", state)
    monkeypatch.setattr(lib, "ARCHIVE", state / "_archive")
    monkeypatch.setattr(lib, "REGISTRY", base / "registry.json")
    monkeypatch.setattr(lib, "LOCKFILE", base / ".registry.lock")
    monkeypatch.setattr(lib, "EVENT_LOG", state / "events.jsonl")
    monkeypatch.setattr(lib, "EVENT_LOCKFILE", state / ".events.lock")
    monkeypatch.setattr(lib, "BACKEND_CONFIG", base / "backend.json")
    monkeypatch.setattr(lib, "PROJECT_DEFAULTS_CONFIG", base / "project_defaults.json")
    yield base


def test_backend_selection_prefers_call_then_environment_then_config(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    lib.set_default_backend("tmux")
    monkeypatch.setenv("SUBAGENTS_BACKEND", "herdr")
    assert lib.selected_backend() == "herdr"
    assert lib.selected_backend("tmux") == "tmux"
    monkeypatch.delenv("SUBAGENTS_BACKEND")
    assert lib.selected_backend() == "tmux"


def test_mode_selection_prefers_call_then_environment_then_project(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    fake_backend_state.mkdir()
    lib.PROJECT_DEFAULTS_CONFIG.write_text('{"harness_modes": {"codex": "tui"}}\n')
    monkeypatch.setenv("SUBAGENTS_MODE", "headless")
    assert lib.selected_mode("codex", backend="tmux") == lib.HEADLESS_MODE
    assert lib.selected_mode("codex", "tui") == lib.TUI_MODE
    monkeypatch.delenv("SUBAGENTS_MODE")
    assert lib.selected_mode("codex", backend="herdr") == lib.TUI_MODE
    assert lib.selected_mode("codex", backend="tmux") == lib.HEADLESS_MODE
    assert "project default for codex selected mode 'tui'" in capsys.readouterr().err
    assert lib.selected_mode("agy", backend="tmux") == lib.HEADLESS_MODE


@pytest.mark.parametrize(("setting", "expected"), [(None, False), ("0", False), ("1", True)])
def test_codex_permissions_are_selected_at_launch_and_retained_for_later_turns(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch, setting: str | None, expected: bool,
) -> None:
    fake_backend_state.mkdir(parents=True)
    if setting is None:
        monkeypatch.delenv("SUBAGENTS_CODEX_BYPASS_PERMISSIONS", raising=False)
    else:
        monkeypatch.setenv("SUBAGENTS_CODEX_BYPASS_PERMISSIONS", setting)
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "orphan_window_exists", lambda backend, name: False)
    monkeypatch.setattr(lib, "gc", lambda: [])
    monkeypatch.setattr(lib, "launch_window", lambda *args: "original:worker")
    lib.bring_up_agent("worker", cwd=str(fake_backend_state), brief=None, backend="tmux", mode="headless")
    rec = lib.read_registry()["worker"]
    assert rec.codex_bypass_permissions is expected
    monkeypatch.setenv("SUBAGENTS_CODEX_BYPASS_PERMISSIONS", "0" if expected else "1")
    rec.session_id = "continued-session"
    argv = agent_runner._build_codex_argv(rec, lib.Message(0, "next task", None, lib.now_iso()))
    flag = "--dangerously-bypass-approvals-and-sandbox"
    assert (flag in argv) is expected
    assert (flag in lib._codex_tui_argv(None, bypass_permissions=rec.codex_bypass_permissions)) is expected


@pytest.mark.parametrize("setting", ["", "true", "yes", "2"])
def test_malformed_codex_permission_setting_fails_before_backend_access(
    monkeypatch: pytest.MonkeyPatch, setting: str,
) -> None:
    monkeypatch.setenv("SUBAGENTS_CODEX_BYPASS_PERMISSIONS", setting)
    monkeypatch.setattr(lib, "backend_available", lambda backend: pytest.fail("backend must remain untouched"))
    with pytest.raises(lib.AgentOperationError, match="must be exactly 0"):
        lib.bring_up_agent("worker", cwd="/work", brief=None, backend="tmux")


def test_legacy_registry_writer_cannot_enable_bypass_on_a_native_worker(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rec = _tui_record("native-worker", fake_backend_state)
    rec.mode = lib.HEADLESS_MODE
    rec.codex_bypass_permissions = False
    with lib.registry_lock() as agents:
        agents[rec.name] = rec
    original_rows = json.loads(lib.REGISTRY.read_text())
    for row in original_rows:
        row.pop("codex_bypass_permissions")
    # A still-running pre-upgrade process serializes only fields it knows.
    lib.REGISTRY.write_text(json.dumps(original_rows))
    monkeypatch.setenv("SUBAGENTS_CODEX_BYPASS_PERMISSIONS", "1")
    restored = lib.read_registry()[rec.name]
    assert restored.codex_bypass_permissions is False
    argv = agent_runner._build_codex_argv(restored, lib.Message(0, "next task", None, lib.now_iso()))
    assert "--dangerously-bypass-approvals-and-sandbox" not in argv
    lib._permission_policy_path(rec.name).write_text("invalid sidecar")
    with pytest.raises(lib.AgentOperationError, match="cannot read worker permissions"):
        lib.read_registry()


def test_auto_detection_requires_herdr_environment_socket_and_live_server(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_backend_state.mkdir()
    socket = fake_backend_state / "herdr.sock"
    socket.touch()
    monkeypatch.setenv("HERDR_ENV", "1")
    monkeypatch.setenv("HERDR_SOCKET_PATH", str(socket))
    monkeypatch.setattr(lib, "herdr_available", lambda: True)
    assert lib.auto_detect_backend() == "herdr"
    socket.unlink()
    assert lib.auto_detect_backend() == "tmux"


def test_herdr_probe_timeout_is_loud_and_bounded(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        subprocess,
        "run",
        lambda *args, **kwargs: (_ for _ in ()).throw(subprocess.TimeoutExpired("herdr", 2)),
    )
    with pytest.raises(lib.AgentOperationError, match="within 2s"):
        lib._herdr("tab", "get", "wX:t1")


def test_herdr_action_accepts_empty_success_response(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        subprocess,
        "run",
        lambda *args, **kwargs: subprocess.CompletedProcess(args=[], returncode=0, stdout="", stderr=""),
    )

    lib._herdr_action("pane", "send-keys", "wX:p1", lib.HERDR_KEY_ENTER)


def test_herdr_accepts_successful_agent_wait_result(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        subprocess,
        "run",
        lambda *args, **kwargs: subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=(
                '{"result":{"agent":{"pane_id":"wX:p1","agent_status":"working"}}}\n'
            ),
            stderr="",
        ),
    )

    assert lib._herdr("agent", "wait", "wX:p1", "--until", "working") == {
        "agent": {"pane_id": "wX:p1", "agent_status": "working"},
    }


def test_herdr_agent_tab_is_always_fresh_not_reused_from_another_checkout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, ...]] = []

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del timeout_s
        calls.append(args)
        return {"tab": {"tab_id": "wT:t9"}, "root_pane": {"pane_id": "wT:p9"}}

    monkeypatch.setattr(lib, "_herdr", herdr)

    assert lib._herdr_tab_for_new_agent("wT", "/work") == ("wT:t9", "wT:p9")
    assert calls == [("tab", "create", "--workspace", "wT", "--cwd", "/work", "--label", "subagent", "--no-focus")]


def test_old_registry_row_defaults_to_tmux_backend(fake_backend_state: Path) -> None:
    record = lib.AgentRecord.from_dict(
        {
            "name": "old-agent",
            "harness": "codex",
            "tmux_target": "subagents:old-agent",
            "cwd": "/tmp",
            "model": None,
            "session_id": None,
            "status": "idle",
            "runner_pid": None,
            "next_seq": 0,
            "created_at": "2026-07-12T00:00:00+00:00",
            "last_turn_at": None,
        }
    )
    assert record.backend == "tmux"
    assert record.mode == lib.HEADLESS_MODE
    assert record.codex_bypass_permissions is True


def test_herdr_headless_runner_uses_the_owned_root_pane(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    actions: list[tuple[str, ...]] = []
    monkeypatch.setattr(lib, "_herdr_workspace_id", lambda cwd: "wT")
    monkeypatch.setattr(lib, "_herdr_tab_for_new_agent", lambda workspace, cwd: ("wT:t9", "wT:p9"))
    monkeypatch.setattr(lib, "_herdr_action", lambda *args: actions.append(args))
    monkeypatch.setattr(lib, "_herdr", lambda *args: {})
    assert lib._launch_herdr_window("worker", "/work", "python3 '/path with spaces/runner.py'") == "wT:t9"
    assert actions[0][:3] == ("pane", "run", "wT:p9")
    assert all("close" not in action for action in actions)


def test_herdr_tui_launch_preserves_live_resume_prompt_for_readiness_barrier(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    actions: list[tuple[str, ...]] = []
    probe = lib.TuiProbe(True, True, "starting", os.getpid())
    monkeypatch.setattr(lib, "CODEX_BIN", "codex")
    monkeypatch.setattr(lib, "_herdr_action", lambda *args: actions.append(args))
    monkeypatch.setattr(lib, "_session_recorded_model", lambda session: "model-a")
    monkeypatch.setattr(lib, "_herdr_workspace_id", lambda cwd: "wT")
    monkeypatch.setattr(lib, "_herdr_tab_for_new_agent", lambda workspace, cwd: ("wT:t9", "wT:p9"))
    monkeypatch.setattr(lib, "_herdr", lambda *args: {})
    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane: probe)
    assert lib._launch_herdr_tui("worker", "/work", None, session_id="session-a") == ("wT:t9", "wT:p9", probe)
    assert actions[0][:3] == ("pane", "run", "wT:p9")
    argv = shlex.split(actions[0][3])
    assert argv[:3] == ["codex", "resume", "session-a"]
    assert argv[-2:] == ["-m", "model-a"]


def test_tmux_record_operations_ignore_changed_default_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rec = _tui_record("worker", tmp_path)
    rec.mode = lib.HEADLESS_MODE
    rec.backend = "tmux"
    rec.tmux_target = "original:worker"
    monkeypatch.setattr(lib, "TMUX_SESSION", "unrelated")
    windows = {"original": {"worker"}, "unrelated": {"worker"}}
    calls: list[tuple[str, ...]] = []

    def tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        del check
        calls.append(args)
        target = args[args.index("-t") + 1] if "-t" in args else ""
        output = ""
        if args[0] == "list-windows":
            output = "\n".join(windows[target.removeprefix("=")])
        elif args[0] == "kill-window":
            session, window = target.split(":")
            windows[session.removeprefix("=")].remove(window.removeprefix("="))
        elif args[0] == "list-panes":
            output = "%17\t456\n"
        return subprocess.CompletedProcess(args=args, returncode=0, stdout=output, stderr="")

    monkeypatch.setattr(lib, "_tmux", tmux)
    monkeypatch.setattr(lib, "_pid_is_descendant", lambda child, parent: parent == 456)
    assert lib.window_exists(rec)
    lib.kill_window(rec)
    assert windows == {"original": set(), "unrelated": {"worker"}}
    assert not lib.window_exists(rec)
    assert lib.break_runner_pane_to_window(rec) == "%17"
    assert calls[-1] == ("break-pane", "-d", "-s", "%17", "-t", "=original:", "-n", "worker")
    assert all("unrelated" not in str(call) for call in calls)


def test_keeper_status_child_inherits_selected_state_and_policy(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_backend_state.mkdir(parents=True)
    policy = fake_backend_state / "policy with spaces.py"
    monkeypatch.chdir(fake_backend_state)
    monkeypatch.setenv("HERDR_SUBAGENTS_POLICY", policy.name)
    monkeypatch.setenv("SUBAGENTS_HERDR_WORKSPACE", "custom workspace")
    script = fake_backend_state / "agent_status.py"
    script.write_text("import json, os, sys\nprint(json.dumps({'env': {k: v for k, v in os.environ.items() if k.startswith(('HERDR_SUBAGENTS_', 'SUBAGENTS_HERDR_'))}, 'python': sys.executable}))\n")
    monkeypatch.setattr(agent_keeper, "__file__", str(fake_backend_state / "agent_keeper.py"))
    monkeypatch.setattr(agent_keeper, "_workspace_id", lambda: "wT")
    tabs = iter([None, "wT:t1"])
    monkeypatch.setattr(agent_keeper, "_keeper_tab_id", lambda wid: next(tabs))
    loops: list[str] = []

    def herdr(*args: str) -> dict[str, object]:
        if args[:2] == ("pane", "send-text"):
            loops.append(args[-1])
        return {"tab": {"active_pane_id": "wT:p1"}}

    monkeypatch.setattr(lib, "_herdr", herdr)
    assert agent_keeper.ensure(interval=7) == 0
    command = loops[0].replace("sleep 7; done", "break; done")
    result = subprocess.run(["/bin/bash", "-c", command], cwd=fake_backend_state.parent, env={"PATH": "/usr/bin:/bin"}, check=True, text=True, capture_output=True)
    child = json.loads(result.stdout)
    assert child["env"]["HERDR_SUBAGENTS_HOME"] == str(fake_backend_state)
    assert child["env"]["HERDR_SUBAGENTS_PROJECT_DEFAULTS"] == str(lib.PROJECT_DEFAULTS_CONFIG)
    assert child["env"]["HERDR_SUBAGENTS_POLICY"] == str(policy)
    assert child["env"]["SUBAGENTS_HERDR_WORKSPACE"] == "custom workspace"
    assert child["python"] == sys.executable


def test_relative_project_defaults_are_bound_before_worker_changes_directory(tmp_path: Path) -> None:
    env = os.environ.copy()
    env["PYTHONPATH"] = str(Path(lib.__file__).resolve().parents[2])
    env["HERDR_SUBAGENTS_PROJECT_DEFAULTS"] = "config/project.json"
    result = subprocess.run(
        [sys.executable, "-c", "from agentctl.foreign import lib; print(lib.PROJECT_DEFAULTS_CONFIG)"],
        cwd=tmp_path, env=env, check=True, text=True, capture_output=True,
    )
    assert result.stdout.strip() == str(tmp_path / "config/project.json")


def test_tmux_migration_guard_retains_original_target_after_default_changes(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    lib.ensure_agent_dirs("worker")
    launches: list[tuple[str, str | None]] = []
    closes: list[tuple[str, ...]] = []

    def launch(name: str, cwd: str, wrapper: str, *, session: str | None = None) -> str:
        del cwd, wrapper
        launches.append((name, session))
        return f"{session}:{name}"

    monkeypatch.setattr(lib, "_tmux_target_exists", lambda target: False)
    monkeypatch.setattr(lib, "_launch_tmux_window", launch)
    monkeypatch.setattr(lib, "_tmux", lambda *args, **kwargs: closes.append(args))
    monkeypatch.setattr(lib, "TMUX_SESSION", "unrelated")
    lib.create_legacy_tmux_guard("worker", "/work", target="original:worker")
    assert launches == [("worker", "original")]
    lib.remove_legacy_tmux_guard("worker")
    assert closes == [("kill-window", "-t", "=original:=worker")]
    lib.remove_legacy_tmux_guard("old-worker")
    assert closes[-1] == ("kill-window", "-t", "=subagents:=old-worker")


def _tui_record(name: str, cwd: Path) -> lib.AgentRecord:
    return lib.AgentRecord(
        name=name,
        harness="codex",
        backend="herdr",
        tmux_target="wT:t1",
        cwd=str(cwd),
        model="model-a",
        session_id=None,
        status="idle",
        runner_pid=os.getpid(),
        runner_started_at=lib.pid_start_time(os.getpid()),
        next_seq=0,
        created_at="2020-01-01T00:00:00+00:00",
        last_turn_at=None,
        mode=lib.TUI_MODE,
        presentation_pane="wT:p2",
    )


@pytest.mark.parametrize("failure", ["permission", "malformed"])
def test_unknown_runner_identity_never_authorizes_a_signal(
    fake_backend_state: Path,
    monkeypatch: pytest.MonkeyPatch,
    failure: str,
) -> None:
    rec = _tui_record("identity-unknown", fake_backend_state)
    assert rec.runner_pid is not None and rec.runner_started_at is not None
    lib.ensure_agent_dirs(rec.name)
    token = "staged"
    lib.write_staged_runner(
        rec.name,
        token,
        lib.RunnerIdentity(rec.runner_pid, rec.runner_started_at),
    )
    (lib.agent_dir(rec.name) / "active-harness.json").write_text(json.dumps({
        "runner_pid": rec.runner_pid,
        "runner_started_at": rec.runner_started_at,
        "pid": rec.runner_pid,
        "started_at": rec.runner_started_at,
    }))

    def unreadable(_path: Path) -> bytes:
        if failure == "permission":
            raise PermissionError("denied")
        return b"malformed"

    signals: list[tuple[str, int, int]] = []
    monkeypatch.setattr(Path, "read_bytes", unreadable)
    monkeypatch.setattr(
        os,
        "kill",
        lambda pid, signum: signals.append(("kill", pid, signum)),
    )
    monkeypatch.setattr(
        os,
        "killpg",
        lambda pid, signum: signals.append(("killpg", pid, signum)),
    )

    operations = (
        lambda: lib.runner_identity_alive(rec),
        lambda: lib.read_staged_runner(rec.name, token),
        lambda: lib.terminate_runner_identity(
            lib.RunnerIdentity(rec.runner_pid or 0, rec.runner_started_at)
        ),
        lambda: lib.terminate_active_harness(rec),
    )
    for operation in operations:
        with pytest.raises(lib.AgentOperationError) as excinfo:
            operation()
        assert excinfo.value.code == "runner_identity_unknown"
    assert signals == []


def test_unbound_runner_identity_preserves_legacy_signaling(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    signals: list[tuple[int, int]] = []
    monkeypatch.setattr(os, "kill", lambda pid, signum: signals.append((pid, signum)))
    monkeypatch.setattr(lib, "pid_alive", lambda _pid: True)
    monkeypatch.setattr(lib, "pid_start_time", lambda _pid: "current")
    assert lib.terminate_runner_identity(lib.RunnerIdentity(4242, None), grace=0)
    assert signals == [(4242, signal.SIGTERM), (4242, signal.SIGKILL)]


class _FakeSharedAgentClient:
    """Protocol fake for the agent-utils transport used by thin project adapters."""

    def __init__(self, cwd: Path, states: list[str] | None = None) -> None:
        self.cwd = str(cwd)
        self.states = states or ["idle"]
        self.index = 0
        self.runs: list[str] = []
        self.waits: list[tuple[str, str, int]] = []
        self.read_sources: list[str] = []
        self.read_values: dict[str, str] = {"recent-unwrapped": "scrollback\n", "recent": "fallback\n"}
        self.fail_confirmation = False

    def panes(self, workspace_id: str | None = None) -> tuple[object, ...]:
        del workspace_id
        return ()

    def pane_info(self, pane_id: str) -> object:
        state = self.states[min(self.index, len(self.states) - 1)]
        self.index += 1
        return AgentPaneInfo(pane_id, "wT", self.cwd, "codex", state, None, None)

    def workspace_label(self, workspace_id: str) -> str:
        assert workspace_id == "wT"
        return lib.HERDR_WORKSPACE_LABEL

    def prompt_agent(self, pane_id: str, text: str) -> None:
        assert pane_id == "wT:p2"
        self.runs.append(text)

    def wait_agent_status(self, pane_id: str, state: str, timeout_ms: int) -> None:
        self.waits.append((pane_id, state, timeout_ms))
        if self.fail_confirmation:
            raise HerdrUnavailable("synthetic confirmation loss")

    def read(self, pane_id: str, *, source: str, lines: int) -> str:
        del lines
        assert pane_id == "wT:p2"
        self.read_sources.append(source)
        return self.read_values[source]


def test_tui_mode_requires_herdr_and_codex(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    with pytest.raises(lib.AgentOperationError, match="requires the herdr backend"):
        lib.bring_up_agent("bad-backend", cwd=str(cwd), brief=None, backend="tmux", mode="tui")
    with pytest.raises(lib.AgentOperationError, match="codex harness only"):
        lib.bring_up_agent(
            "bad-harness", cwd=str(cwd), brief=None, backend="herdr", harness="agy", mode="tui"
        )


def test_codex_tui_resume_omits_stale_registry_model_when_session_metadata_is_missing(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(lib, "CODEX_SESSIONS", tmp_path / "sessions")
    assert lib._codex_tui_argv("model-a", "session-kept") == [
        lib.CODEX_BIN,
        "resume",
        "session-kept",
        "--no-alt-screen",
    ]


def test_codex_tui_resume_uses_recorded_session_model(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    session_id = "session-kept"
    sessions = tmp_path / "sessions" / "2026" / "07" / "13"
    sessions.mkdir(parents=True)
    (sessions / f"rollout-2026-07-13T00-00-00-{session_id}.jsonl").write_text(
        '{"type":"turn_context","payload":{"model":"model-b"}}\n'
    )
    monkeypatch.setattr(lib, "CODEX_SESSIONS", tmp_path / "sessions")

    assert lib._codex_tui_argv("model-c", session_id)[-2:] == [
        "-m",
        "model-b",
    ]


def test_tui_mode_records_pane_and_delivers_initial_brief(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    delivered: list[str] = []
    probe = lib.TuiProbe(True, True, "idle", os.getpid())
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "_launch_herdr_tui", lambda name, cwd, model, **kwargs: ("wT:t1", "wT:p2", probe))
    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane_id: probe)
    monkeypatch.setattr(lib, "_deliver_tui_messages", lambda rec: delivered.append(rec.name))

    result = lib.bring_up_agent(
        "tui-agent", cwd=str(cwd), brief="greet", backend="herdr", mode=lib.TUI_MODE
    )

    record = lib.read_registry()["tui-agent"]
    assert record.mode == lib.TUI_MODE
    assert record.presentation_pane == "wT:p2"
    assert result.queued_turn == 0
    assert delivered == ["tui-agent"]


def test_tui_delivery_waits_for_idle_then_submits_one_message(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-delivery"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    record = _tui_record(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = record
    lib.enqueue_message(name, "reply exactly READY", model=None)
    shared = _FakeSharedAgentClient(cwd, ["working", "idle"])
    monkeypatch.setattr(lib, "_shared_tui_client", lambda: shared)

    lib._deliver_tui_messages(record)

    assert shared.runs == ["reply exactly READY"]
    assert shared.waits == [("wT:p2", "working", lib.TUI_SUBMIT_WORKING_TIMEOUT_MS)]
    assert lib.pending_count(name) == 0
    assert (lib.processed_dir(name) / "000000000000.json").exists()


def test_tui_delivery_accepts_a_long_multiline_message_without_echo_matching(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-long-message"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    record = _tui_record(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = record
    message = "\n".join(["Long coordinator brief:", *("line " + "x" * 300 for _ in range(8))])
    lib.enqueue_message(name, message, model=None)
    shared = _FakeSharedAgentClient(cwd, ["working", "idle"])
    monkeypatch.setattr(lib, "_shared_tui_client", lambda: shared)

    assert lib._deliver_tui_messages(record) == []

    assert shared.runs == [message]
    assert shared.waits == [("wT:p2", "working", lib.TUI_SUBMIT_WORKING_TIMEOUT_MS)]
    assert (lib.processed_dir(name) / "000000000000.json").exists()


def test_tui_poison_message_is_quarantined_and_does_not_block_fifo(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-poison"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    record = _tui_record(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = record
    lib.enqueue_message(name, "poison", model=None)
    lib.enqueue_message(name, "healthy follow-up", model=None)
    shared = _FakeSharedAgentClient(cwd)
    confirmations = {"count": 0}

    def confirm(pane_id: str, state: str, timeout_ms: int) -> None:
        del pane_id, state, timeout_ms
        confirmations["count"] += 1
        if confirmations["count"] == 1:
            raise HerdrUnavailable("synthetic confirmation loss")

    shared.wait_agent_status = confirm  # type: ignore[method-assign]
    monkeypatch.setattr(lib, "_shared_tui_client", lambda: shared)

    assert lib._deliver_tui_messages(record) == [0]
    assert shared.runs == ["poison", "healthy follow-up"]
    assert not (lib.inbox_dir(name) / "000000000000.json").exists()
    assert (lib.failed_dir(name) / "000000000000.json").exists()
    assert (lib.processed_dir(name) / "000000000001.json").exists()
    failed = json.loads((lib.failed_dir(name) / "000000000000.json").read_text())
    assert failed["tui_delivery_attempts"] == 1
    assert failed["possibly_submitted"] is True
    assert "tui_message_quarantined" in lib.EVENT_LOG.read_text()


def test_drain_tui_inbox_refuses_to_destroy_a_stuck_composer(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-drain"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    record = _tui_record(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = record
    lib.enqueue_message(name, "after-clear", model=None)
    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane: lib.TuiProbe(True, True, "idle", os.getpid()))

    with pytest.raises(lib.AgentOperationError, match="cannot safely clear TUI agent"):
        lib.drain_tui_inbox(name, clear_composer=True)

    assert (lib.inbox_dir(name) / "000000000000.json").exists()


def test_tui_readiness_accepts_resumed_session_directory_prompt(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-resume-directory"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    record = _tui_record(name, cwd)
    states = iter(
        [
            lib.TuiProbe(True, True, "working", os.getpid()),
            lib.TuiProbe(True, True, "idle", os.getpid()),
        ]
    )
    reads = iter([lib.TUI_RESUME_DIRECTORY_PROMPT, "", ""])
    calls: list[tuple[str, ...]] = []

    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane_id: next(states))

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del timeout_s
        calls.append(args)
        if args[:2] == ("agent", "read"):
            return {"read": {"text": next(reads)}}
        return {}

    monkeypatch.setattr(lib, "_herdr", herdr)
    monkeypatch.setattr(lib, "_herdr_action", herdr)

    lib._wait_for_tui_prompt(record, timeout_s=1, handle_resume_startup=True)

    assert ("pane", "send-keys", "wT:p2", lib.HERDR_KEY_ENTER) in calls
    assert "tui_resume_directory_accepted" in lib.EVENT_LOG.read_text()


def test_tui_readiness_accepts_resumed_session_model_warning(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-resume-model"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    record = _tui_record(name, cwd)
    states = iter(
        [
            lib.TuiProbe(True, True, "working", os.getpid()),
            lib.TuiProbe(True, True, "idle", os.getpid()),
        ]
    )
    calls: list[tuple[str, ...]] = []

    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane_id: next(states))

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del timeout_s
        calls.append(args)
        if args[:2] == ("agent", "read"):
            return {"read": {"text": lib.TUI_RESUME_MODEL_PROMPT}}
        return {}

    monkeypatch.setattr(lib, "_herdr", herdr)
    monkeypatch.setattr(lib, "_herdr_action", herdr)

    lib._wait_for_tui_prompt(record, timeout_s=1, handle_resume_startup=True)

    assert ("pane", "send-keys", "wT:p2", lib.HERDR_KEY_ENTER) in calls
    assert "tui_resume_model_warning_accepted" in lib.EVENT_LOG.read_text()


def test_tui_readiness_accepts_background_done_without_waiting_for_idle(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    record = _tui_record("tui-background-done", fake_backend_state)
    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane_id: lib.TuiProbe(True, True, "done", 123))
    monkeypatch.setattr(
        lib,
        "_herdr",
        lambda *args, **kwargs: pytest.fail("a done TUI must not wait only for idle"),
    )

    lib._wait_for_tui_prompt(record, timeout_s=0)


def test_gc_preserves_live_tui_pane_even_without_headless_runner(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-gc"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)
        agents[name].runner_pid = 999_991
        agents[name].runner_started_at = "stale"
    monkeypatch.setattr(
        lib, "_herdr_tui_probe", lambda pane_id: lib.TuiProbe(True, True, "idle", os.getpid())
    )
    monkeypatch.setattr(lib, "_herdr_workspace_exists", lambda workspace_id: True)

    assert lib.gc() == []
    assert name in lib.read_registry()


def test_herdr_teardown_closes_only_its_tab_not_the_shared_workspace(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    calls: list[tuple[str, ...]] = []

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del timeout_s
        calls.append(args)
        return {}

    monkeypatch.setattr(lib, "_herdr", herdr)

    lib._kill_herdr_window(_tui_record("tui-close", cwd))

    assert calls == [("tab", "close", "wT:t1")]


def test_workspace_death_is_loud_and_preserves_recovery_snapshot(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "workspace-lost"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    lib.transcript_path(name).write_text("unfinished turn\n")
    rec = _tui_record(name, cwd)
    rec.mode = lib.HEADLESS_MODE
    rec.tmux_target = "wLost:t1"
    rec.presentation_pane = None
    rec.session_id = "session-to-recover"
    rec.runner_pid = 999_990
    rec.runner_started_at = "stale"
    with lib.registry_lock() as agents:
        agents[name] = rec
    monkeypatch.setattr(lib, "_herdr_workspace_exists", lambda workspace_id: False)

    notes = lib.gc()

    assert len(notes) == 1
    assert "Herdr workspace wLost is gone" in notes[0]
    assert name not in lib.read_registry()
    archive = next(lib.ARCHIVE.iterdir())
    snapshot = json.loads((archive / "WORKSPACE_LOST.json").read_text())
    assert snapshot["workspace_id"] == "wLost"
    assert snapshot["agent"]["session_id"] == "session-to-recover"


def test_workspace_probe_failure_preserves_agent_instead_of_guessing_death(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "workspace-probe-failed"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    rec = _tui_record(name, cwd)
    rec.mode = lib.HEADLESS_MODE
    rec.tmux_target = "wLost:t1"
    rec.presentation_pane = None
    rec.runner_pid = 999_989
    rec.runner_started_at = "stale"
    with lib.registry_lock() as agents:
        agents[name] = rec
    monkeypatch.setattr(lib, "_herdr_workspace_exists", lambda workspace_id: None)

    notes = lib.gc()

    assert notes == [
        "workspace-probe-failed Herdr workspace wLost probe failed; preserving state "
        "instead of assuming a workspace death"
    ]
    assert name in lib.read_registry()


def test_tui_read_uses_recent_unwrapped_scrollback(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-read"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)
    shared = _FakeSharedAgentClient(cwd)
    shared.read_values["recent-unwrapped"] = "human and coordinator conversation\n"
    monkeypatch.setattr(lib, "_shared_tui_client", lambda: shared)

    # "all"/"tail" are the two modes the TUI backend can honestly
    # support (both just mean "recent scrollback" here); "last" and
    # since_turn are covered by the dedicated rejection tests below.
    result = lib.read_agent_output(name, mode="tail", tail=77)

    assert result.text == "human and coordinator conversation\n"
    assert shared.read_sources == ["recent-unwrapped"]


def test_tui_read_rejects_last_mode_instead_of_silently_substituting_scrollback(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """mode="last" used to silently return generic recent scrollback
    while still claiming mode="last" in the result -- a coordinator asking
    for "the last answer" got an arbitrary recent screen with no indication
    the requested mode wasn't honored. The TUI backend has no durable
    turn-boundary capture to identify "the last answer" against, so it must
    fail loudly instead."""
    name = "tui-read-last-unsupported"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del args, timeout_s
        raise AssertionError("Herdr must not be called once mode=last is rejected")

    monkeypatch.setattr(lib, "_herdr", herdr)

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.read_agent_output(name, mode="last")
    assert excinfo.value.code == "tui_last_unsupported"


def test_tui_read_rejects_since_turn_instead_of_silently_substituting_scrollback(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """since_turn=N used to be ignored entirely for a TUI agent."""
    name = "tui-read-since-turn-unsupported"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del args, timeout_s
        raise AssertionError("Herdr must not be called once since_turn is rejected")

    monkeypatch.setattr(lib, "_herdr", herdr)

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.read_agent_output(name, mode="all", since_turn=12)
    assert excinfo.value.code == "tui_since_turn_unsupported"


def test_tui_read_rejects_tail_above_the_cap(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """an unbounded `tail` used to pass straight through to Herdr."""
    name = "tui-read-tail-too-large"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del args, timeout_s
        raise AssertionError("Herdr must not be called once an over-cap tail is rejected")

    monkeypatch.setattr(lib, "_herdr", herdr)

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.read_agent_output(name, mode="tail", tail=lib.TUI_MAX_TAIL_LINES + 1)
    assert excinfo.value.code == "tui_tail_too_large"


def test_tui_read_rejects_since_turn_mode_with_parameter_omitted(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """the TUI branch's
    since_turn rejection keyed on the `since_turn` PARAMETER being non-None,
    and its "last" rejection keyed on the mode STRING -- so mode="since_turn"
    called with the parameter omitted matched neither guard and fell through
    to the exact silent-substitution bug this issue exists to remove,
    labeled with the caller's bogus mode. The shared _validate_read_request
    check (mode/parameter pairing) now catches this before either backend
    branch runs."""
    name = "tui-read-since-turn-mode-no-param"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del args, timeout_s
        raise AssertionError("Herdr must not be called once a malformed mode/parameter pairing is rejected")

    monkeypatch.setattr(lib, "_herdr", herdr)

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.read_agent_output(name, mode="since_turn")
    assert excinfo.value.code == "bad_read_mode"


def test_tui_read_rejects_unknown_mode_string(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """an unrecognized mode string hit neither backend
    branch's specific guard and fell through to silent scrollback
    substitution, labeled with the bogus mode."""
    name = "tui-read-unknown-mode"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)

    def herdr(*args: str, timeout_s: float | None = None) -> dict[str, object]:
        del args, timeout_s
        raise AssertionError("Herdr must not be called once an unknown mode is rejected")

    monkeypatch.setattr(lib, "_herdr", herdr)

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.read_agent_output(name, mode="bogus")
    assert excinfo.value.code == "bad_read_mode"


def test_tui_read_falls_back_to_recent_when_unwrapped_is_empty(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "tui-read-fallback"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    with lib.registry_lock() as agents:
        agents[name] = _tui_record(name, cwd)
    shared = _FakeSharedAgentClient(cwd)
    shared.read_values = {"recent-unwrapped": "", "recent": "fallback text\n"}
    monkeypatch.setattr(lib, "_shared_tui_client", lambda: shared)

    assert lib.read_agent_output(name).text == "fallback text\n"
    assert shared.read_sources == ["recent-unwrapped", "recent"]


def test_old_server_rewrite_preserves_herdr_identity_through_tab_target() -> None:
    record = lib.AgentRecord.from_dict(
        {"name":"herdr","harness":"codex","tmux_target":"wJ:t1","cwd":"/tmp","model":None,"session_id":None,"status":"idle","runner_pid":1,"next_seq":0,"created_at":"2026-07-12T00:00:00+00:00","last_turn_at":None}
    )
    assert record.backend == "herdr"


def test_old_server_rewrite_recovers_tui_mode_and_pane_from_durable_identity(
    fake_backend_state: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    name = "tui-old-writer"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    original = _tui_record(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = original

    stripped = original.to_dict()
    for key in ("backend", "mode", "presentation_pane"):
        del stripped[key]
    lib.REGISTRY.write_text(json.dumps([stripped]) + "\n")

    recovered = lib.read_registry()[name]
    assert recovered.backend == "herdr"
    assert recovered.mode == lib.TUI_MODE
    assert recovered.presentation_pane == "wT:p2"
    assert lib.presentation_identity_path(name).exists()
    repaired = json.loads(lib.REGISTRY.read_text())[0]
    assert repaired["backend"] == "herdr"
    assert repaired["mode"] == lib.TUI_MODE
    assert repaired["presentation_pane"] == "wT:p2"

    delivered_to: list[str] = []
    monkeypatch.setattr(
        lib, "_herdr_tui_probe", lambda pane: lib.TuiProbe(True, True, "idle", os.getpid())
    )
    def deliver(rec: lib.AgentRecord) -> list[int]:
        delivered_to.append(rec.presentation_pane or "")
        return []

    monkeypatch.setattr(lib, "_deliver_tui_messages", deliver)
    result = lib.send_message_to_agent(name, "still reaches the TUI")

    assert result.mode == lib.TUI_MODE
    assert result.presentation_pane == "wT:p2"
    assert delivered_to == ["wT:p2"]


def test_stale_presentation_identity_does_not_apply_to_reused_name(
    fake_backend_state: Path,
) -> None:
    name = "tui-stale-identity"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    original = _tui_record(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = original

    stale_row = original.to_dict()
    stale_row["created_at"] = "2026-07-13T00:00:00+00:00"
    for key in ("backend", "mode", "presentation_pane"):
        del stale_row[key]
    lib.REGISTRY.write_text(json.dumps([stale_row]) + "\n")

    recovered = lib.read_registry()[name]
    assert recovered.backend == "herdr"
    assert recovered.mode == lib.HEADLESS_MODE
    assert recovered.presentation_pane is None


def test_migration_preserves_session_and_updates_presentation(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "migrating"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    with lib.registry_lock() as agents:
        agents[name] = lib.AgentRecord(
            name=name,
            harness="codex",
            backend="tmux",
            tmux_target="subagents:migrating",
            cwd=str(cwd),
            model=None,
            session_id="session-kept",
            status="idle",
            runner_pid=os.getpid(),
            runner_started_at=lib.pid_start_time(os.getpid()),
            next_seq=1,
            created_at=lib.now_iso(),
            last_turn_at=lib.now_iso(),
        )

    killed: list[lib.AgentRecord] = []
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: True)
    monkeypatch.setattr(
        lib,
        "wait_for_staged_runner",
        lambda name, token: lib.RunnerIdentity(pid=999_999, started_at="new-start"),
    )
    monkeypatch.setattr(lib, "terminate_runner", lambda rec: True)
    monkeypatch.setattr(lib, "wait_for_staged_runner_activation", lambda name, token: True)
    guards: list[tuple[str, str]] = []
    monkeypatch.setattr(lib, "create_legacy_tmux_guard", lambda name, cwd, **kwargs: guards.append((name, cwd)))
    monkeypatch.setattr(lib, "launch_window", lambda backend, name, cwd, wrapper: "wB:t9")
    monkeypatch.setattr(lib, "kill_window", lambda rec: killed.append(rec))

    result = lib.migrate_agent(name)

    assert result.from_backend == "tmux"
    assert result.to_backend == "herdr"
    assert result.session_id == "session-kept"
    migrated = lib.read_registry()[name]
    assert migrated.backend == "herdr"
    assert migrated.tmux_target == "wB:t9"
    assert killed[0].backend == "tmux"
    assert guards == [(name, str(cwd))]


def test_migration_destination_runner_dies_keeps_tmx_source_intact(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "destination-dies"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    old_pid = os.getpid()
    old_start = lib.pid_start_time(old_pid)
    with lib.registry_lock() as agents:
        agents[name] = lib.AgentRecord(
            name=name,
            harness="codex",
            backend="tmux",
            tmux_target="subagents:destination-dies",
            cwd=str(cwd),
            model=None,
            session_id="session-kept",
            status="idle",
            runner_pid=old_pid,
            runner_started_at=old_start,
            next_seq=1,
            created_at=lib.now_iso(),
            last_turn_at=lib.now_iso(),
        )

    killed: list[lib.AgentRecord] = []
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: True)
    monkeypatch.setattr(lib, "wait_for_staged_runner", lambda name, token: None)
    monkeypatch.setattr(lib, "launch_window", lambda backend, name, cwd, wrapper: "wB:t10")
    monkeypatch.setattr(lib, "kill_window", lambda rec: killed.append(rec))
    monkeypatch.setattr(lib, "terminate_runner", lambda rec: pytest.fail("source must not be terminated"))

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.migrate_agent(name)

    assert excinfo.value.code == "migration_failed_before_commit"
    assert "no live staged runner identity" in excinfo.value.message
    source = lib.read_registry()[name]
    assert source.backend == "tmux"
    assert source.tmux_target == "subagents:destination-dies"
    assert source.runner_pid == old_pid
    assert [rec.backend for rec in killed] == ["herdr"]
    assert not lib.migration_pause_path(name).exists()


@pytest.mark.parametrize("legacy", [False, True])
def test_activation_timeout_restores_source_registry_after_commit(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch, legacy: bool,
) -> None:
    name = "activation-timeout"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    old_pid, old_start = os.getpid(), lib.pid_start_time(os.getpid())
    with lib.registry_lock() as agents:
        agents[name] = lib.AgentRecord(name=name, harness="codex", backend="tmux",
            tmux_target=f"subagents:{name}", cwd=str(cwd), model=None, session_id="session-kept",
            status="idle", runner_pid=old_pid, runner_started_at=old_start, next_seq=1,
            created_at=lib.now_iso(), last_turn_at=lib.now_iso())
    destination = lib.RunnerIdentity(999_998, "destination-start")
    killed: list[str] = []
    stopped: list[int] = []
    monkeypatch.setattr(lib, "backend_available", lambda _backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda _name, _old: not legacy)
    monkeypatch.setattr(lib, "wait_for_staged_runner", lambda _name, _token: destination)
    monkeypatch.setattr(lib, "launch_window", lambda *_args: "wZ:t1")
    monkeypatch.setattr(lib, "terminate_runner", lambda _rec: True)
    monkeypatch.setattr(lib, "terminate_runner_identity", lambda identity: stopped.append(identity.pid))
    monkeypatch.setattr(lib, "kill_window", lambda rec: killed.append(rec.tmux_target))
    def activation(_name: str, _token: str) -> bool:
        with lib.registry_lock() as agents:
            assert agents[name].runner_pid == destination.pid
            assert agents[name].backend == "herdr"
            agents[name].next_seq = 42
        return False
    monkeypatch.setattr(lib, "wait_for_staged_runner_activation", activation)
    def restart(_name: str, old: lib.AgentRecord) -> lib.RunnerIdentity:
        current = lib.read_registry()[name]
        assert current.backend == old.backend and current.tmux_target == old.tmux_target
        return lib.RunnerIdentity(old_pid, old_start)
    monkeypatch.setattr(lib, "_restore_legacy_tmux_runner", restart)
    with pytest.raises(lib.AgentOperationError) as caught:
        lib.migrate_agent(name)
    assert caught.value.code == ("legacy_migration_destination_failed" if legacy else "migration_activation_rolled_back")
    restored = lib.read_registry()[name]
    assert (restored.backend, restored.tmux_target, restored.runner_pid, restored.runner_started_at) == (
        "tmux", f"subagents:{name}", old_pid, old_start)
    assert restored.next_seq == 42 and restored.session_id == "session-kept"
    assert stopped == [destination.pid] and killed == ["wZ:t1"]
    assert not lib.migration_pause_path(name).exists()


def test_legacy_tmux_source_without_pause_ack_stops_only_after_idle_check(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "legacy-success"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    old_pid = os.getpid()
    old_start = lib.pid_start_time(old_pid)
    with lib.registry_lock() as agents:
        agents[name] = lib.AgentRecord(
            name=name,
            harness="codex",
            backend="tmux",
            tmux_target="subagents:legacy-success",
            cwd=str(cwd),
            model=None,
            session_id="session-kept",
            status="idle",
            runner_pid=old_pid,
            runner_started_at=old_start,
            next_seq=1,
            created_at=lib.now_iso(),
            last_turn_at=lib.now_iso(),
        )

    stopped: list[lib.AgentRecord] = []
    killed: list[lib.AgentRecord] = []
    wrappers: list[str] = []
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: False)
    monkeypatch.setattr(
        lib,
        "wait_for_staged_runner",
        lambda name, token: lib.RunnerIdentity(pid=999_998, started_at="legacy-new"),
    )
    def stop_runner(rec: lib.AgentRecord) -> bool:
        stopped.append(rec)
        return True

    monkeypatch.setattr(lib, "terminate_runner", stop_runner)
    monkeypatch.setattr(lib, "wait_for_staged_runner_activation", lambda name, token: True)
    monkeypatch.setattr(lib, "create_legacy_tmux_guard", lambda name, cwd, **kwargs: None)

    def launch(backend: str, name: str, cwd: str, wrapper: str) -> str:
        assert lib.migration_pause_path(name).exists()
        wrappers.append(wrapper)
        return "wB:t11"

    monkeypatch.setattr(lib, "launch_window", launch)
    monkeypatch.setattr(lib, "kill_window", lambda rec: killed.append(rec))

    result = lib.migrate_agent(name)

    assert result.to_backend == "herdr"
    assert [rec.backend for rec in stopped] == ["tmux"]
    assert [rec.backend for rec in killed] == ["tmux"]
    assert "SUBAGENTS_MIGRATION_STAGE=" in wrappers[0]
    assert lib.read_registry()[name].backend == "herdr"
    assert not lib.migration_pause_path(name).exists()


def test_legacy_tmux_destination_failure_restores_tmux_runner(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "legacy-restore"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    old_pid = os.getpid()
    old_start = lib.pid_start_time(old_pid)
    with lib.registry_lock() as agents:
        agents[name] = lib.AgentRecord(
            name=name,
            harness="codex",
            backend="tmux",
            tmux_target="subagents:legacy-restore",
            cwd=str(cwd),
            model=None,
            session_id="session-kept",
            status="idle",
            runner_pid=old_pid,
            runner_started_at=old_start,
            next_seq=1,
            created_at=lib.now_iso(),
            last_turn_at=lib.now_iso(),
        )

    stopped: list[lib.AgentRecord] = []
    killed: list[lib.AgentRecord] = []
    launches: list[str] = []
    restored = lib.RunnerIdentity(pid=999_997, started_at="restored")
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: False)
    monkeypatch.setattr(lib, "wait_for_staged_runner", lambda name, token: None)
    def stop_runner(rec: lib.AgentRecord) -> bool:
        stopped.append(rec)
        return True

    monkeypatch.setattr(lib, "terminate_runner", stop_runner)
    monkeypatch.setattr(lib, "window_exists", lambda rec: True)

    def launch(backend: str, name: str, cwd: str, wrapper: str) -> str:
        launches.append(backend)
        return "wB:t12" if backend == "herdr" else f"subagents:{name}"

    def publish_restored_runner(name: str, previous: lib.RunnerIdentity) -> lib.RunnerIdentity:
        with lib.registry_lock() as agents:
            agents[name].runner_pid = restored.pid
            agents[name].runner_started_at = restored.started_at
            agents[name].status = "idle"
        return restored

    monkeypatch.setattr(lib, "launch_window", launch)
    monkeypatch.setattr(lib, "kill_window", lambda rec: killed.append(rec))
    monkeypatch.setattr(lib, "wait_for_restarted_runner", publish_restored_runner)

    with pytest.raises(lib.AgentOperationError) as excinfo:
        lib.migrate_agent(name)

    assert excinfo.value.code == "legacy_migration_destination_failed"
    assert "tmux runner pid 999997 was restored" in excinfo.value.message
    assert [rec.backend for rec in stopped] == ["tmux"]
    assert launches == ["herdr", "tmux"]
    assert [rec.backend for rec in killed] == ["herdr", "tmux"]
    source = lib.read_registry()[name]
    assert source.backend == "tmux"
    assert source.runner_pid == restored.pid
    assert not lib.migration_pause_path(name).exists()


def _headless_conversion_source(name: str, cwd: Path) -> lib.AgentRecord:
    return lib.AgentRecord(
        name=name,
        harness="codex",
        backend="tmux",
        tmux_target=f"subagents:{name}",
        cwd=str(cwd),
        model="model-a",
        session_id="session-kept",
        status="idle",
        runner_pid=os.getpid(),
        runner_started_at=lib.pid_start_time(os.getpid()),
        next_seq=1,
        created_at=lib.now_iso(),
        last_turn_at=lib.now_iso(),
    )


def test_headless_to_tui_conversion_confirms_response_before_retiring_source(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "convert-tui"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    source = _headless_conversion_source(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = source
    calls: list[str] = []
    probe = lib.TuiProbe(True, True, "idle", 999_991)

    def launch_tui(
        name: str, cwd: str, model: str | None, *, session_id: str | None = None,
        bypass_permissions: bool | None = None,
    ) -> tuple[str, str, lib.TuiProbe]:
        del name, cwd, model
        calls.append(f"launch:{session_id}")
        return "wT:t8", "wT:p8", probe

    def confirm_tui(rec: lib.AgentRecord) -> None:
        del rec
        calls.append("confirmed")

    def retire_source(rec: lib.AgentRecord) -> bool:
        del rec
        calls.append("retired")
        return True

    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: True)
    monkeypatch.setattr(lib, "_launch_herdr_tui", launch_tui)
    monkeypatch.setattr(lib, "_confirm_resumed_tui", confirm_tui)
    monkeypatch.setattr(lib, "terminate_runner", retire_source)
    monkeypatch.setattr(lib, "kill_window", lambda rec: calls.append(f"close:{rec.backend}"))
    monkeypatch.setattr(lib, "create_legacy_tmux_guard", lambda name, cwd, **kwargs: calls.append("guard"))

    result = lib.migrate_agent(name, to_backend="herdr", to_mode="tui")

    assert result.from_mode == lib.HEADLESS_MODE
    assert result.to_mode == lib.TUI_MODE
    assert calls[:3] == ["launch:session-kept", "confirmed", "retired"]
    converted = lib.read_registry()[name]
    assert converted.backend == "herdr"
    assert converted.mode == lib.TUI_MODE
    assert converted.presentation_pane == "wT:p8"
    persisted = json.loads(lib.REGISTRY.read_text())[0]
    assert persisted["mode"] == lib.TUI_MODE
    assert persisted["presentation_pane"] == "wT:p8"
    assert not lib.migration_pause_path(name).exists()


def test_headless_to_tui_failure_keeps_paused_source_and_removes_destination(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "convert-tui-failure"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    source = _headless_conversion_source(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = source
    killed: list[lib.AgentRecord] = []
    probe = lib.TuiProbe(True, True, "idle", 999_992)
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: True)
    monkeypatch.setattr(
        lib, "_launch_herdr_tui", lambda name, cwd, model, *, session_id=None, bypass_permissions=None: ("wT:t9", "wT:p9", probe)
    )
    monkeypatch.setattr(
        lib,
        "_confirm_resumed_tui",
        lambda rec: (_ for _ in ()).throw(lib.AgentOperationError("no_reply", "health check failed")),
    )
    monkeypatch.setattr(lib, "terminate_runner", lambda rec: pytest.fail("source must not be retired"))
    monkeypatch.setattr(lib, "kill_window", lambda rec: killed.append(rec))

    with pytest.raises(lib.AgentOperationError, match="health check failed"):
        lib.migrate_agent(name, to_backend="herdr", to_mode="tui")

    assert [rec.backend for rec in killed] == ["herdr"]
    assert lib.read_registry()[name].mode == lib.HEADLESS_MODE
    assert not lib.migration_pause_path(name).exists()
    assert not lib.migration_pause_ack_path(name).exists()
    assert lib.runner_identity_alive(lib.read_registry()[name])


def test_legacy_headless_to_tui_failure_restores_tmux_source(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "legacy-convert-tui-failure"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    source = _headless_conversion_source(name, cwd)
    with lib.registry_lock() as agents:
        agents[name] = source
    restored = lib.RunnerIdentity(999_993, "restored")
    probe = lib.TuiProbe(True, True, "idle", 999_994)
    launches: list[str] = []
    monkeypatch.setattr(lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(lib, "wait_for_pause_ack", lambda name, old: False)
    monkeypatch.setattr(lib, "terminate_runner", lambda rec: True)
    monkeypatch.setattr(lib, "window_exists", lambda rec: False)
    monkeypatch.setattr(
        lib, "_launch_herdr_tui", lambda name, cwd, model, *, session_id=None, bypass_permissions=None: ("wT:t10", "wT:p10", probe)
    )
    monkeypatch.setattr(
        lib,
        "_confirm_resumed_tui",
        lambda rec: (_ for _ in ()).throw(lib.AgentOperationError("no_reply", "health check failed")),
    )
    monkeypatch.setattr(lib, "kill_window", lambda rec: None)

    def launch_headless(backend: str, name: str, cwd: str, wrapper: str) -> str:
        del cwd, wrapper
        launches.append(backend)
        return f"subagents:{name}"

    monkeypatch.setattr(
        lib,
        "launch_window",
        launch_headless,
    )

    def publish_restore(name: str, previous: lib.RunnerIdentity) -> lib.RunnerIdentity:
        with lib.registry_lock() as agents:
            agents[name].runner_pid = restored.pid
            agents[name].runner_started_at = restored.started_at
        return restored

    monkeypatch.setattr(lib, "wait_for_restarted_runner", publish_restore)

    with pytest.raises(lib.AgentOperationError, match="tmux source restored as pid 999993"):
        lib.migrate_agent(name, to_backend="herdr", to_mode="tui")

    assert launches == ["tmux"]
    restored_row = lib.read_registry()[name]
    assert restored_row.backend == "tmux"
    assert restored_row.mode == lib.HEADLESS_MODE


def test_resume_source_intake_removes_pause_and_ack_markers(
    fake_backend_state: Path,
) -> None:
    name = "resume-intake"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    source = _headless_conversion_source(name, cwd)
    lib.write_migration_pause(name, lib.RunnerIdentity(source.runner_pid or -1, source.runner_started_at))
    lib.acknowledge_migration_pause(
        name, lib.RunnerIdentity(source.runner_pid or -1, source.runner_started_at)
    )

    lib._resume_source_intake(name, source)

    assert not lib.migration_pause_path(name).exists()
    assert not lib.migration_pause_ack_path(name).exists()


def test_tui_migration_health_check_uses_generous_ready_and_working_timeouts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    rec = _tui_record("health-check", Path("/tmp"))
    submitted: list[tuple[float, int]] = []
    expected = "abc"[::-1]

    def submit(
        rec: lib.AgentRecord,
        text: str,
        *,
        ready_timeout_s: float,
        working_timeout_ms: int,
        handle_resume_startup: bool,
    ) -> None:
        del rec, text, handle_resume_startup
        submitted.append((ready_timeout_s, working_timeout_ms))

    monkeypatch.setattr(lib, "secrets", type("Secrets", (), {"token_hex": staticmethod(lambda n: "abc")}))
    monkeypatch.setattr(lib, "_submit_tui_text", submit)
    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane: lib.TuiProbe(True, True, "idle", os.getpid()))
    monkeypatch.setattr(lib, "_herdr", lambda *args, **kwargs: {"read": {"text": expected}})

    lib._confirm_resumed_tui(rec)

    assert submitted == [
        (lib.TUI_MIGRATION_READY_TIMEOUT_S, int(lib.TUI_MIGRATION_READY_TIMEOUT_S * 1000))
    ]


def test_tui_migration_default_readiness_budget_allows_heavy_sessions() -> None:
    assert lib.TUI_MIGRATION_READY_TIMEOUT_S == 300
    assert lib.TUI_MIGRATION_CONFIRM_TIMEOUT_S == 300


def test_tui_migration_timeout_records_status_and_scrollback(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rec = _tui_record("slow-resume", fake_backend_state)
    monkeypatch.setattr(lib, "_herdr_tui_probe", lambda pane: lib.TuiProbe(True, True, "working", 123))
    monkeypatch.setattr(
        lib,
        "_herdr",
        lambda *args, **kwargs: {"read": {"text": "Context compacted; rebuilding session"}},
    )

    with pytest.raises(lib.AgentOperationError, match="last Herdr status=working"):
        lib._wait_for_tui_prompt(rec, timeout_s=0, handle_resume_startup=True)

    events = lib.EVENT_LOG.read_text()
    assert "tui_resume_wait_progress" in events
    assert "tui_resume_readiness_timeout" in events
    assert "Context compacted; rebuilding session" in events


def test_force_retirement_archives_without_probing_unavailable_backend(
    fake_backend_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    name = "lost-herdr-agent"
    cwd = fake_backend_state / "cwd"
    cwd.mkdir(parents=True)
    lib.ensure_agent_dirs(name)
    with lib.registry_lock() as agents:
        agents[name] = lib.AgentRecord(
            name=name,
            harness="codex",
            backend="herdr",
            tmux_target="wMissing:t1",
            cwd=str(cwd),
            model=None,
            session_id=None,
            status="idle",
            runner_pid=None,
            runner_started_at=None,
            next_seq=0,
            created_at=lib.now_iso(),
            last_turn_at=None,
        )

    def backend_probe_must_not_run(*_args: object, **_kwargs: object) -> bool:
        raise AssertionError("forced retirement must not probe Herdr")

    monkeypatch.setattr(lib, "_herdr_tab_exists", backend_probe_must_not_run)
    monkeypatch.setattr(lib, "window_exists", backend_probe_must_not_run)

    result = lib.bring_down_agent(name, grace=0, force=True)

    assert result.forced
    assert result.killed_window is False
    assert result.unverified_presentation == "herdr presentation wMissing:t1"
    assert result.archived_to is not None
    assert not lib.read_registry()
    assert "agent_down_forced" in lib.EVENT_LOG.read_text()


def test_tmux_absent_is_a_failed_call_not_a_crash(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A host with no tmux must not crash the check=False callers.

    Regression for the CI failure this suite exposed on the day it was first
    gated: the runner has no tmux, and `bring_down_agent(force=True)` died with
    FileNotFoundError on its way to archiving instead of completing the
    retirement. `check=False` suppresses exit codes, not a missing executable.
    Every check=False caller already reads nonzero as "no session/window", and
    no tmux at all is the strongest form of that.
    """
    monkeypatch.setattr(lib, "_TMUX_MISSING_WARNED", False)

    def no_tmux(*_args: object, **_kwargs: object) -> object:
        raise FileNotFoundError(2, "No such file or directory", "tmux")

    monkeypatch.setattr("agentctl.foreign.lib.subprocess.run", no_tmux)

    result = lib._tmux("kill-window", "-t", "subagents:whatever", check=False)
    assert result.returncode != 0, "a missing tmux must read as a failed call"

    # Not silent: the first occurrence says tmux is absent, so an empty tmux
    # session and an uninstalled tmux are distinguishable in the logs.
    assert "tmux is not installed" in capsys.readouterr().err

    # The callers that genuinely depend on tmux must still see the exception.
    with pytest.raises(FileNotFoundError):
        lib._tmux("list-windows", check=True)


def test_tmux_absent_keeps_window_queries_answerable(
    monkeypatch: pytest.MonkeyPatch
) -> None:
    """Session/window predicates must answer False rather than raise."""
    monkeypatch.setattr(lib, "_TMUX_MISSING_WARNED", True)

    def no_tmux(*_args: object, **_kwargs: object) -> object:
        raise FileNotFoundError(2, "No such file or directory", "tmux")

    monkeypatch.setattr("agentctl.foreign.lib.subprocess.run", no_tmux)

    assert lib.session_exists() is False
    assert lib._tmux_window_exists("anything") is False
    lib._kill_tmux_window("anything")  # must not raise


def test_shared_tui_runner_timeout_is_normalized_by_shared_client(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def timeout(*_args: object, **_kwargs: object) -> subprocess.CompletedProcess[str]:
        raise subprocess.TimeoutExpired(["herdr", "pane", "list"], 30)

    monkeypatch.setattr(subprocess, "run", timeout)
    client = lib._shared_tui_client()
    with pytest.raises(HerdrUnavailable, match="timed out"):
        client.panes()
