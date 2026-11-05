from __future__ import annotations

import json
import os
import sys
from io import StringIO
from pathlib import Path
from typing import Iterator

import pytest

from agentctl.foreign.mcp import adapter, server


@pytest.fixture()
def fake_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    # This suite tests the MCP adapter's durable state transitions, not the
    # host's live Herdr installation.  Make every selection input explicit so
    # a runner's HERDR_ENV/socket cannot turn a fixture into a host probe.
    for variable in (
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "SUBAGENTS_BACKEND",
        "SUBAGENTS_MODE",
    ):
        monkeypatch.delenv(variable, raising=False)
    base = tmp_path / "subagents"
    base.mkdir()
    monkeypatch.delenv("HERDR_SUBAGENTS_POLICY", raising=False)
    state = base / "state"
    monkeypatch.setattr(adapter.lib, "BASE", base)
    monkeypatch.setattr(adapter.lib, "STATE", state)
    monkeypatch.setattr(adapter.lib, "ARCHIVE", state / "_archive")
    monkeypatch.setattr(adapter.lib, "REGISTRY", base / "registry.json")
    monkeypatch.setattr(adapter.lib, "LOCKFILE", base / ".registry.lock")
    monkeypatch.setattr(adapter.lib, "EVENT_LOG", state / "events.jsonl")
    monkeypatch.setattr(adapter.lib, "EVENT_LOCKFILE", state / ".events.lock")
    monkeypatch.setattr(adapter.lib, "RUNNER", base / "agent_runner.py")
    project_defaults = base / "project_defaults.json"
    project_defaults.write_text(json.dumps({"harness_modes": {"codex": "tui"}}))
    monkeypatch.setattr(adapter.lib, "PROJECT_DEFAULTS_CONFIG", project_defaults)
    monkeypatch.setattr(adapter.lib, "BACKEND_CONFIG", base / "backend.json")
    monkeypatch.setattr(adapter.lib, "_herdr_workspace_exists", lambda workspace: True)
    active_windows: set[str] = set()
    monkeypatch.setattr(adapter.lib, "backend_available", lambda backend: True)
    monkeypatch.setattr(adapter.lib, "require_antigravity_preflight", lambda cwd: None)

    def window_exists(rec: adapter.lib.AgentRecord) -> bool:
        return rec.name in active_windows

    def launch_window(backend: str, name: str, cwd: str, wrapper: str) -> str:
        active_windows.add(name)
        return f"subagents:{name}"

    def kill_window(rec: adapter.lib.AgentRecord) -> None:
        active_windows.discard(rec.name)

    def launch_tui(
        name: str, cwd: str, model: str | None, *, session_id: str | None = None,
        bypass_permissions: bool | None = None,
    ) -> tuple[str, str, adapter.lib.TuiProbe]:
        del cwd, model, session_id
        active_windows.add(name)
        return f"wM:{name}", f"wM:p-{name}", adapter.lib.TuiProbe(True, True, "idle", os.getpid())

    def herdr_tab_exists(tab_id: str | None) -> bool:
        return tab_id is not None and tab_id.removeprefix("wM:") in active_windows

    monkeypatch.setattr(adapter.lib, "window_exists", window_exists)
    monkeypatch.setattr(adapter.lib, "launch_window", launch_window)
    monkeypatch.setattr(adapter.lib, "kill_window", kill_window)
    monkeypatch.setattr(adapter.lib, "_launch_herdr_tui", launch_tui)
    monkeypatch.setattr(adapter.lib, "_herdr_tab_exists", herdr_tab_exists)
    monkeypatch.setattr(
        adapter.lib,
        "_herdr_tui_probe",
        lambda pane_id: adapter.lib.TuiProbe(True, True, "idle", os.getpid()),
    )
    monkeypatch.setattr(adapter.lib, "_deliver_tui_messages", lambda rec: [])
    # Status is adapter behavior here.  The transport protocol itself is
    # exercised in agent-utils; do not let this fixture invoke a real binary.
    fake_shared_client = object()
    monkeypatch.setattr(adapter.lib, "_shared_tui_client", lambda: fake_shared_client)
    monkeypatch.setattr(
        adapter.lib,
        "shared_agent_status",
        lambda client, target, agent_dir: {"agent_status": "idle"},
    )
    monkeypatch.setattr(adapter.lib, "shared_agent_read", lambda client, target, *, lines: "FULL TRANSCRIPT\n")
    monkeypatch.setattr(adapter.lib, "orphan_window_exists", lambda backend, name: name in active_windows)
    yield base


def call(name: str, args: dict[str, server.Json]) -> dict[str, server.Json]:
    return server.SubagentTools().call(name, args)


def test_up_send_read_status_down(fake_state: Path) -> None:
    up = call(
        "subagent_up",
        {"name": "alpha", "cwd": str(fake_state), "brief": "Say ready.", "backend": "herdr"},
    )
    assert up["ok"] is True
    assert up["queued_turn"] == 0

    sent = call("subagent_send", {"name": "alpha", "message": "PING"})
    assert sent["ok"] is True
    assert sent["seq"] == 1

    read = call("subagent_read", {"name": "alpha", "mode": "tail", "tail": 20})
    assert read["text"] == "FULL TRANSCRIPT\n"

    status = call("subagent_status", {"name": "alpha"})
    agents = status["agents"]
    assert isinstance(agents, list)
    assert len(agents) == 1
    assert isinstance(agents[0], dict)
    assert agents[0]["name"] == "alpha"
    assert agents[0]["status"] == "idle"

    down = call("subagent_down", {"name": "alpha", "archive": True})
    assert down["ok"] is True
    assert down["archived_to"] is not None


def test_selected_unavailable_herdr_fails_loudly(
    fake_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The fixture fake must not weaken the production backend preflight."""
    monkeypatch.setattr(adapter.lib, "backend_available", lambda backend: False)

    with pytest.raises(adapter.lib.AgentOperationError) as excinfo:
        adapter.lib.bring_up_agent(
            "missing-herdr",
            cwd=str(fake_state),
            brief=None,
            backend="herdr",
            mode=adapter.lib.HEADLESS_MODE,
        )

    assert excinfo.value.code == "herdr_missing"
    assert "not available" in excinfo.value.message


def test_read_defaults_to_all_not_last_when_mode_is_omitted(fake_state: Path) -> None:
    """this tool's `mode` default used to silently diverge from
    lib.read_agent_output's own default ("all") and from the CLI's default
    (also "all"), defaulting to "last" instead -- an MCP caller that never
    asked for "last" got it anyway. Write DIFFERENT content to last-message.txt
    and the transcript so a wrong default is directly observable."""
    up = call(
        "subagent_up",
        {"name": "beta", "cwd": str(fake_state), "brief": "Say ready."},
    )
    assert up["ok"] is True

    (fake_state / "state" / "beta" / "last-message.txt").write_text("LAST ONLY\n")
    adapter.transcript_path("beta").write_text("FULL TRANSCRIPT\n")

    read = call("subagent_read", {"name": "beta"})
    assert read["text"] == "FULL TRANSCRIPT\n"
    assert read["mode"] == "all"


def test_up_accepts_agy_harness(fake_state: Path) -> None:
    up = call(
        "subagent_up",
        {"name": "agyalpha", "cwd": str(fake_state), "brief": "Say ready.", "harness": "agy"},
    )
    assert up["ok"] is True
    agent = up["agent"]
    assert isinstance(agent, dict)
    assert agent["harness"] == "agy"


def test_up_honors_explicit_launch_policy(
    fake_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    policy = fake_state / "policy.py"
    policy.write_text(
        "from agentctl.foreign.lib import AgentOperationError\n"
        "def backend(): return None\n"
        "def check_launch(harness, model, purpose):\n"
        "    raise AgentOperationError('harness_not_allowed', 'launch refused by policy')\n"
    )
    monkeypatch.setenv("HERDR_SUBAGENTS_POLICY", str(policy))
    with pytest.raises(adapter.ToolFailure, match="launch refused by policy"):
        call("subagent_up", {"name": "blocked", "cwd": str(fake_state), "brief": "test", "backend": "tmux"})
    assert adapter.lib.read_registry() == {}


def test_precise_unknown_agent_error(fake_state: Path) -> None:
    with pytest.raises(adapter.ToolFailure) as excinfo:
        call("subagent_send", {"name": "missing", "message": "PING"})
    assert excinfo.value.code == "unknown_agent"
    assert "missing" in excinfo.value.message

    with pytest.raises(adapter.ToolFailure) as down_excinfo:
        call("subagent_down", {"name": "missing"})
    assert down_excinfo.value.code == "unknown_agent"


def test_live_tui_pane_without_codex_is_degraded_not_reaped(
    fake_state: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    call("subagent_up", {"name": "degraded", "cwd": str(fake_state), "brief": "ready", "backend": "herdr"})
    monkeypatch.setattr(
        adapter.lib,
        "_herdr_tui_probe",
        lambda pane_id: adapter.lib.TuiProbe(True, False, "idle", None),
    )

    status = call("subagent_status", {"name": "degraded"})
    agents = status["agents"]
    assert isinstance(agents, list)
    assert isinstance(agents[0], dict)
    assert agents[0]["runner_alive"] is False
    assert agents[0]["window_alive"] is True
    assert agents[0]["presentation_degraded"] is True
    assert "degraded" in adapter.lib.read_registry()

    with pytest.raises(adapter.ToolFailure) as send_error:
        call("subagent_send", {"name": "degraded", "message": "PING"})
    assert send_error.value.code == "dead_tui"

    with pytest.raises(adapter.ToolFailure) as recreate_error:
        call("subagent_recreate_window", {"name": "degraded"})
    assert recreate_error.value.code == "tui_recreate_unsupported"


def test_mcp_tools_call_returns_json_error(fake_state: Path) -> None:
    mcp = server.McpServer(stdin=StringIO(), stdout=StringIO(), tools=server.SubagentTools())
    response = mcp._tools_call(7, {"name": "subagent_send", "arguments": {"name": "missing", "message": "PING"}})
    result = response["result"]
    assert isinstance(result, dict)
    assert result["isError"] is True
    content = result["content"]
    assert isinstance(content, list)
    assert isinstance(content[0], dict)
    payload = json.loads(str(content[0]["text"]))
    assert payload["error"]["code"] == "unknown_agent"


def test_mcp_exposes_migration_tool() -> None:
    assert "subagent_migrate" in {schema["name"] for schema in server.tool_schemas()}


def test_websocket_port_default_avoids_browser_test_servers(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv(server.WS_PORT_ENV, raising=False)

    assert server.websocket_port_from_env() == 18765
    assert server.DEFAULT_WS_PORT == 18765


def test_websocket_port_honors_explicit_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(server.WS_PORT_ENV, "18772")

    assert server.websocket_port_from_env() == 18772


def test_mcp_up_schema_exposes_opt_in_tui_mode() -> None:
    up = next(schema for schema in server.tool_schemas() if schema["name"] == "subagent_up")
    input_schema = up["inputSchema"]
    assert isinstance(input_schema, dict)
    properties = input_schema["properties"]
    assert isinstance(properties, dict)
    assert properties["mode"] == {
        "type": ["string", "null"],
        "enum": ["headless", "tui", None],
        "description": "Per-agent override; omitted uses SUBAGENTS_MODE then project_defaults.json.",
    }
