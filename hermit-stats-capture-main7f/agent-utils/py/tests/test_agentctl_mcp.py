"""Protocol-level checks for a coordinator using the canonical registry through MCP."""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

from agentctl.mcp import call_tool, tool_schemas
import pytest


def test_stdio_initialize_list_and_empty_registry(tmp_path: Path) -> None:
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "agent_list", "arguments": {}}},
    ]
    process = subprocess.run([sys.executable, "-m", "agentctl", "--registry", str(tmp_path / "registry"),
        "--herdr-bin", "/unavailable-herdr", "mcp"], input="".join(json.dumps(item) + "\n" for item in requests),
        text=True, capture_output=True, timeout=10)
    assert process.returncode == 0, process.stderr
    replies = [json.loads(line) for line in process.stdout.splitlines()]
    assert [reply["id"] for reply in replies] == [1, 2, 3]
    assert replies[0]["result"]["serverInfo"]["name"] == "agentctl"
    names = {tool["name"] for tool in replies[1]["result"]["tools"]}
    assert {"agent_start", "agent_adopt", "agent_send", "agent_goal", "agent_pause", "agent_stop"} <= names
    assert replies[2]["result"]["isError"] is False
    assert json.loads(replies[2]["result"]["content"][0]["text"]) == []
    assert not (tmp_path / "registry").exists()


@pytest.mark.parametrize("arguments", [{"name": "worker", "unexpected": True}, {"name": True}, {}])
def test_malformed_calls_refuse_before_runtime(tmp_path: Path, arguments: dict[str, object]) -> None:
    with pytest.raises(ValueError):
        call_tool("agent_status", arguments, str(tmp_path / "registry"), "/missing")
    assert not (tmp_path / "registry").exists()


def test_every_mcp_operation_describes_and_closes_its_argument_schema() -> None:
    for tool in tool_schemas():
        assert tool["description"]
        schema = tool["inputSchema"]
        assert isinstance(schema, dict)
        assert schema["additionalProperties"] is False


def test_adopt_mcp_routes_session_as_an_assertion_option(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    arguments: list[str] = []

    def invoke(argv: list[str]) -> int:
        arguments.extend(argv)
        return 0

    monkeypatch.setattr("agentctl.cli.main", invoke)
    result = call_tool("agent_adopt", {
        "name": "reviewer", "pane": "w1:p2", "workspace": "project",
        "cwd": "/work/project", "harness": "codex", "session": "thread-1",
    }, str(tmp_path / "registry"), "/opt/herdr")
    assert result["isError"] is False
    assert arguments == [
        "--registry", str(tmp_path / "registry"), "--herdr-bin", "/opt/herdr",
        "adopt", "reviewer", "--pane=w1:p2", "--workspace=project",
        "--cwd=/work/project", "--harness=codex", "--session=thread-1",
    ]
