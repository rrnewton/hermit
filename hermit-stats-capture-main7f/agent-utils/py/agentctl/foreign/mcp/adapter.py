#!/usr/bin/env python3
"""Typed adapter from MCP requests to the persistent-worker runtime."""

from __future__ import annotations

import dataclasses
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Optional, ParamSpec, TypeAlias

from agentctl.foreign import lib as lib

Json: TypeAlias = object
P = ParamSpec("P")


class ToolFailure(Exception):
    """A structured worker error that the MCP boundary reports as a failed tool result."""
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _convert(value: object) -> Json:
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return _convert(dataclasses.asdict(value))
    if isinstance(value, dict):
        return {str(k): _convert(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_convert(v) for v in value]
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    return str(value)


def _call(fn: Callable[P, object], *args: P.args, **kwargs: P.kwargs) -> dict[str, Json]:
    try:
        result = fn(*args, **kwargs)
    except lib.AgentOperationError as exc:
        raise ToolFailure(exc.code, exc.message) from exc
    payload = _convert(result)
    if not isinstance(payload, dict):
        raise ToolFailure("internal_error", "adapter returned a non-object result")
    payload["ok"] = True
    return payload


def subagent_up(
    name: str,
    cwd: str,
    brief: str,
    model: Optional[str] = None,
    harness: str = "codex",
    backend: Optional[str] = None,
    mode: Optional[str] = None,
) -> dict[str, Json]:
    """Create a named worker and return a JSON-compatible status and first-turn result."""
    return _call(
        lib.bring_up_agent,
        name,
        cwd=cwd,
        brief=brief,
        model=model,
        harness=harness,
        backend=backend,
        mode=mode,
    )


def subagent_send(name: str, message: str, model: Optional[str] = None) -> dict[str, Json]:
    """Queue a follow-up prompt and report its sequence and delivery outcome."""
    return _call(lib.send_message_to_agent, name, message, model=model)


def subagent_read(
    name: str,
    mode: str = "last",
    since_turn: Optional[int] = None,
    tail: Optional[int] = None,
) -> dict[str, Json]:
    """Return requested transcript output or bounded interactive scrollback."""
    return _call(lib.read_agent_output, name, mode=mode, since_turn=since_turn, tail=tail)


def subagent_status(name: Optional[str] = None) -> dict[str, Json]:
    """Return process, presentation, and queue state for one worker or the full registry."""
    return _call(lib.status_snapshot, name)


def subagent_list() -> dict[str, Json]:
    """Return status snapshots for every registered worker."""
    return _call(lib.status_snapshot, None)


def subagent_down(name: str, archive: bool = True) -> dict[str, Json]:
    """Retire a worker and preserve its archive unless the caller explicitly disables archiving."""
    result = lib.bring_down_agent(name, archive=archive)
    if (
        not result.was_registered
        and not result.killed_window
        and result.archived_to is None
        and result.state_path is None
    ):
        raise ToolFailure("unknown_agent", f"unknown agent {name!r}")
    payload = _convert(result)
    if not isinstance(payload, dict):
        raise ToolFailure("internal_error", "adapter returned a non-object result")
    payload["ok"] = True
    return payload


def subagent_reset(name: str) -> dict[str, Json]:
    """Start a fresh harness conversation without changing the named worker identity."""
    return _call(lib.reset_agent_context, name)


def subagent_recreate_window(name: str) -> dict[str, Json]:
    """Restore missing presentation for an existing persistent worker."""
    return _call(lib.recreate_window, name)


def subagent_migrate(
    name: str, to_backend: str = "herdr", to_mode: Optional[str] = None
) -> dict[str, Json]:
    """Move an idle worker to the requested presentation backend or mode."""
    return _call(lib.migrate_agent, name, to_backend=to_backend, to_mode=to_mode)


def event_log_path() -> Path:
    """Return the shared lifecycle-event log consumed by event-stream subscribers."""
    return lib.EVENT_LOG


def transcript_path(name: str) -> Path:
    """Return the path of the worker transcript used for headless turn reads."""
    return lib.transcript_path(name)


def registry_snapshot() -> dict[str, lib.AgentRecord]:
    """Serialize the current worker registry for MCP event-stream monitoring."""
    return lib.read_registry()


def last_message_preview(name: str) -> str:
    """Return a bounded, single-line preview of the latest captured answer."""
    return lib.last_message_preview(name)


def emit_event(
    event_type: str,
    name: str,
    *,
    seq: Optional[int] = None,
    rc: Optional[str] = None,
    preview: Optional[str] = None,
) -> None:
    """Append a named worker lifecycle event with optional turn and output metadata."""
    lib.write_event(event_type, name, seq=seq, rc=rc, preview=preview)
