"""Private process boundary for an isolated persistent-turn runtime.

Each invocation inherits one explicit runtime home. No process-global module
paths are changed while an MCP server or another session is using them.
"""
from __future__ import annotations

import json
import sys
from dataclasses import asdict, is_dataclass
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from agentctl.foreign import lib
from agentctl.jsonx import as_mapping, get_str


def _optional_text(request: dict[str, object], key: str) -> str | None:
    value = request.get(key)
    if value is not None and not isinstance(value, str):
        raise ValueError(f"{key} must be a string")
    return value


def _optional_int(request: dict[str, object], key: str) -> int | None:
    value = request.get(key)
    if value is not None and (not isinstance(value, int) or isinstance(value, bool)):
        raise ValueError(f"{key} must be an integer")
    return value


def _string_array(request: dict[str, object], key: str) -> list[str]:
    value = request.get(key, [])
    if not isinstance(value, list) or any(
        not isinstance(item, str) or not item or "\0" in item for item in value
    ):
        raise ValueError(f"{key} must be an array of nonempty NUL-free strings")
    return value


def dispatch(request: dict[str, object]) -> dict[str, object]:
    """Execute one explicitly supported runtime operation."""
    action = get_str(request, "action", "runtime request")
    name = get_str(request, "name", "runtime request")
    result: object
    if action == "start":
        result = lib.bring_up_agent(name, cwd=get_str(request, "cwd", "start"),
            harness=get_str(request, "harness", "start"),
            model=_optional_text(request, "model"), brief=_optional_text(request, "brief"),
            backend=get_str(request, "backend", "start"), mode="headless",
            harness_args=_string_array(request, "harness_args"))
    elif action == "status":
        result = lib.status_snapshot(name, run_gc=False)
    elif action == "send":
        result = lib.send_message_to_agent(name, get_str(request, "text", "send"),
            model=_optional_text(request, "model"))
    elif action == "read":
        result = lib.read_agent_output(name, mode=get_str(request, "mode", "read"),
            since_turn=_optional_int(request, "since_turn"), tail=_optional_int(request, "tail"))
    elif action == "stop":
        result = lib.bring_down_agent(name, archive=True)
    elif action == "reset":
        result = lib.reset_agent_context(name)
    elif action == "migrate":
        mode = _optional_text(request, "mode")
        result = lib.migrate_agent(name, to_backend=get_str(request, "backend", "migrate"),
            to_mode="tui" if mode == "interactive" else mode)
    elif action == "repair":
        result = lib.recreate_window(name)
    elif action in ("pause", "resume"):
        marker = lib.agent_dir(name) / "automation-paused"
        if action == "pause":
            from agentctl.agent import _atomic_json
            _atomic_json(str(marker), {"paused": True})
        else:
            marker.unlink(missing_ok=True)
        result = {"paused": action == "pause"}
    else:
        raise ValueError(f"unsupported runtime operation: {action}")
    value: object = asdict(result) if is_dataclass(result) and not isinstance(result, type) else result
    record = lib.read_registry().get(name)
    return {"result": value, "record": asdict(record) if record is not None else None}


def _main() -> int:
    """Exchange exactly one JSON request and response over stdin/stdout."""
    try:
        result = dispatch(as_mapping(json.load(sys.stdin), "runtime request"))
    except (lib.AgentOperationError, ValueError, TypeError, OSError) as exc:
        json.dump({"error": str(exc), "code": getattr(exc, "code", "runtime_failure")}, sys.stdout)
        sys.stdout.write("\n")
        return 1
    json.dump(result, sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
