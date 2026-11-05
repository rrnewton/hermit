"""MCP stdio access to the same named sessions and operations as the CLI."""
from __future__ import annotations

import contextlib
import io
import json
import sys
from dataclasses import dataclass

from agentctl import __version__
from agentctl.jsonx import as_mapping


@dataclass(frozen=True)
class Operation:
    """One public operation and its accepted MCP arguments."""
    description: str
    required: tuple[str, ...]
    arguments: tuple[str, ...]


OPERATIONS = {
    "start": Operation("Start a persistent agent; interactive Herdr or headless Herdr/tmux.",
        ("name",), ("name", "cwd", "harness", "mode", "backend", "model", "brief", "resume")),
    "adopt": Operation("Register an existing identity-checked Herdr agent without owning its runtime.",
        ("name", "pane", "workspace", "cwd", "harness"),
        ("name", "pane", "workspace", "cwd", "harness", "session")),
    "send": Operation("Submit follow-up text, retaining uncertain delivery for reconciliation.",
        ("name", "text"), ("name", "text", "message_id", "model", "ready_timeout")),
    "list": Operation("List all named sessions and their capabilities.", (), ()),
    "status": Operation("Inspect one session's identity, runtime, and queue.", ("name",), ("name",)),
    "read": Operation("Read terminal text or an explicit headless answer boundary.",
        ("name",), ("name", "output", "lines", "since_turn")),
    "wait": Operation("Wait for readiness; this does not prove goal completion.", ("name",), ("name", "timeout")),
    "stop": Operation("Stop an owned session, or safely unregister an adopted one, and archive state.",
        ("name",), ("name",)),
    "move": Operation("Move a running owned native Herdr session into its configured project workspace.",
        ("name",), ("name",)),
    "pause": Operation("Pause automated input for human interaction.", ("name",), ("name",)),
    "resume": Operation("Resume automated input after human interaction.", ("name",), ("name",)),
    "goal": Operation("Read native goal state when supported, or send a goal instruction.",
        ("name",), ("name", "text", "goal_command_json")),
    "bind-session": Operation("Bind a native conversation ID reported by the harness.",
        ("name", "session"), ("name", "session", "goal_command_json")),
    "drain": Operation("Deliver pending interactive requests without replaying uncertain input.",
        ("name",), ("name", "ready_timeout")),
    "migrate": Operation("Move an idle headless worker while preserving conversation state.",
        ("name", "backend"), ("name", "backend", "mode")),
    "reset": Operation("Reset an idle headless worker's conversation.", ("name",), ("name",)),
    "repair": Operation("Repair a live headless worker's terminal presentation.", ("name",), ("name",)),
    "attach": Operation("Focus the verified terminal for direct inspection.", ("name",), ("name",)),
}
_INTEGERS = {"lines", "since_turn"}
_NUMBERS = {"ready_timeout", "timeout"}


def tool_schemas() -> list[dict[str, object]]:
    """Describe every tool, rejecting unspecified arguments instead of ignoring them."""
    return [{"name": "agent_" + name.replace("-", "_"), "description": operation.description,
        "inputSchema": {"type": "object", "additionalProperties": False,
            "required": list(operation.required), "properties": {key: {
                "type": "integer" if key in _INTEGERS else "number" if key in _NUMBERS else "string"
            } for key in operation.arguments}}} for name, operation in OPERATIONS.items()]


def call_tool(name: str, arguments: dict[str, object], registry: str, herdr_bin: str) -> dict[str, object]:
    """Run the CLI's validation and implementation without introducing another registry."""
    from agentctl.cli import main as cli_main
    command = name.removeprefix("agent_").replace("_", "-")
    if not name.startswith("agent_") or command not in OPERATIONS:
        raise ValueError(f"unknown tool: {name}")
    operation = OPERATIONS[command]
    if set(arguments) - set(operation.arguments):
        raise ValueError("tool arguments contain unknown fields")
    if set(operation.required) - set(arguments):
        raise ValueError("tool arguments are missing required fields")
    argv = ["--registry", registry, "--herdr-bin", herdr_bin, command]
    for key in ("name", "session"):
        if key in arguments:
            value = arguments[key]
            if not isinstance(value, str) or not value or value.startswith("-"):
                raise ValueError(f"{key} must be a nonempty identifier")
            if key == "name" or command == "bind-session":
                argv.append(value)
    for key, value in arguments.items():
        if key in ("name", "text") or (key == "session" and command == "bind-session"):
            continue
        if key in _INTEGERS:
            valid = isinstance(value, int) and not isinstance(value, bool)
        elif key in _NUMBERS:
            valid = isinstance(value, (int, float)) and not isinstance(value, bool)
        else:
            valid = isinstance(value, str)
        if not valid:
            raise ValueError(f"invalid value for {key}")
        argv.append("--" + key.replace("_", "-") + "=" + str(value))
    if "text" in arguments:
        text = arguments["text"]
        if not isinstance(text, str):
            raise ValueError("text must be a string")
        argv.extend(("--", text))
    output, errors = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
        try:
            code = cli_main(argv)
        except SystemExit as exc:
            code = exc.code if isinstance(exc.code, int) else 1
    rendered = output.getvalue() + errors.getvalue()
    return {"content": [{"type": "text", "text": rendered}], "isError": code != 0}


def _serve(registry: str = ".agentctl", herdr_bin: str = "herdr") -> int:
    """Serve bounded JSON-RPC messages on stdio; diagnostics never enter stdout."""
    for line in sys.stdin:
        identifier: object = None
        try:
            if len(line.encode()) > 1_048_576:
                raise ValueError("MCP request exceeds 1 MiB")
            request = as_mapping(json.loads(line), "MCP request")
            identifier = request.get("id")
            method = request.get("method")
            if identifier is None:
                continue
            if request.get("jsonrpc") != "2.0":
                raise ValueError("expected JSON-RPC 2.0")
            result: object
            if method == "initialize":
                result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "agentctl", "version": __version__}}
            elif method == "ping":
                result = {}
            elif method == "tools/list":
                result = {"tools": tool_schemas()}
            elif method == "tools/call":
                params = as_mapping(request.get("params"), "tool call")
                tool = params.get("name")
                if not isinstance(tool, str):
                    raise ValueError("tool name must be a string")
                result = call_tool(tool, as_mapping(params.get("arguments", {}), "tool arguments"), registry, herdr_bin)
            else:
                raise ValueError(f"unsupported method: {method}")
            response: dict[str, object] = {"jsonrpc": "2.0", "id": identifier, "result": result}
        except (ValueError, TypeError, OSError) as exc:
            response = {"jsonrpc": "2.0", "id": identifier,
                "error": {"code": -32602, "message": str(exc)}}
        print(json.dumps(response), flush=True)
    return 0
