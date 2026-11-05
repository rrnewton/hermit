#!/usr/bin/env python3
"""MCP stdio server for the subagents toolkit."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
import re
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Optional, TextIO, TypeAlias

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[3]))
from agentctl.foreign.mcp import adapter

Json: TypeAlias = object

WS_PORT_ENV = "SUBAGENTS_MCP_WS_PORT"
# Keep the per-worker MCP event stream away from browser test servers (8765/8766).
# The MCP launcher increments this configured base when another worker already owns it.
DEFAULT_WS_PORT = 18765
TURN_DONE_RE = re.compile(r"^===TURN-DONE\s+(\d+)\s+rc=([^ ]+)")


def _as_dict(value: Json) -> dict[str, Json]:
    return value if isinstance(value, dict) else {}


def _string(args: dict[str, Json], key: str, *, required: bool = True) -> Optional[str]:
    value = args.get(key)
    if value is None and not required:
        return None
    if not isinstance(value, str):
        raise adapter.ToolFailure("bad_argument", f"{key!r} must be a string")
    return value


def _int(args: dict[str, Json], key: str) -> Optional[int]:
    value = args.get(key)
    if value is None:
        return None
    if not isinstance(value, int):
        raise adapter.ToolFailure("bad_argument", f"{key!r} must be an integer")
    return value


def _bool(args: dict[str, Json], key: str, default: bool) -> bool:
    value = args.get(key)
    if value is None:
        return default
    if not isinstance(value, bool):
        raise adapter.ToolFailure("bad_argument", f"{key!r} must be a boolean")
    return value


def _json_text(value: Json) -> str:
    return json.dumps(value, sort_keys=True)


class SubagentTools:
    """Dispatch validated MCP tool arguments to the worker adapter."""
    def call(self, name: str, args: dict[str, Json]) -> dict[str, Json]:
        """Execute one supported tool call or raise a structured argument or operation failure."""
        if name == "subagent_up":
            return adapter.subagent_up(
                    _string(args, "name") or "",
                    _string(args, "cwd") or "",
                    _string(args, "brief") or "",
                    model=_string(args, "model", required=False),
                    harness=_string(args, "harness", required=False) or "codex",
                    backend=_string(args, "backend", required=False),
                    mode=_string(args, "mode", required=False),
                )
        if name == "subagent_send":
            return adapter.subagent_send(
                    _string(args, "name") or "",
                    _string(args, "message") or "",
                    model=_string(args, "model", required=False),
                )
        if name == "subagent_read":
            # this default silently diverged from lib.read_agent_output's
            # own default ("all") and from agent_read.py's CLI default (also
            # "all") -- an MCP caller that omitted `mode` got mode="last"
            # while every other caller got the full transcript/scrollback.
            # That mismatch became actively harmful once mode="last" started
            # being rejected outright for interactive TUI agents (they have no
            # durable turn-boundary capture to honor it against): a caller
            # that never even asked for "last" would hit that rejection on
            # every default read. Match the other two callers' default.
            mode = _string(args, "mode", required=False) or "all"
            return adapter.subagent_read(
                    _string(args, "name") or "",
                    mode=mode,
                    since_turn=_int(args, "since_turn"),
                    tail=_int(args, "tail"),
                )
        if name == "subagent_status":
            return adapter.subagent_status(_string(args, "name", required=False))
        if name == "subagent_down":
            return adapter.subagent_down(
                    _string(args, "name") or "",
                    archive=_bool(args, "archive", True),
                )
        if name == "subagent_reset":
            return adapter.subagent_reset(_string(args, "name") or "")
        if name == "subagent_recreate_window":
            return adapter.subagent_recreate_window(_string(args, "name") or "")
        if name == "subagent_migrate":
            return adapter.subagent_migrate(
                    _string(args, "name") or "",
                    _string(args, "to_backend", required=False) or "herdr",
                    _string(args, "to_mode", required=False),
                )
        if name == "subagent_list":
            return adapter.subagent_list()
        raise adapter.ToolFailure("unknown_tool", f"unknown MCP tool {name!r}")


def tool_schemas() -> list[dict[str, Json]]:
    """Return MCP tool definitions and their argument schemas."""
    string = {"type": "string"}
    nullable_string = {"type": ["string", "null"]}
    return [
        {
            "name": "subagent_up",
            "description": "Start a named subagent and queue its first brief. TUI mode is interactive Codex in Herdr.",
            "inputSchema": {
                "type": "object",
                "required": ["name", "cwd", "brief"],
                "properties": {
                    "name": string,
                    "cwd": string,
                    "brief": string,
                    "model": nullable_string,
                    "harness": {"type": "string", "enum": ["codex", "agy"], "default": "codex"},
                    "backend": {"type": ["string", "null"], "enum": ["tmux", "herdr", None]},
                    "mode": {
                        "type": ["string", "null"],
                        "enum": ["headless", "tui", None],
                        "description": "Per-agent override; omitted uses SUBAGENTS_MODE then project_defaults.json.",
                    },
                },
            },
        },
        {
            "name": "subagent_send",
            "description": "Queue a plain string message to a running subagent.",
            "inputSchema": {
                "type": "object",
                "required": ["name", "message"],
                "properties": {"name": string, "message": string, "model": nullable_string},
            },
        },
        {
            "name": "subagent_read",
            "description": "Read a subagent transcript or last answer.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": string,
                    "mode": {"type": "string", "enum": ["last", "since_turn", "tail", "all"]},
                    "since_turn": {"type": "integer"},
                    "tail": {"type": "integer"},
                },
            },
        },
        {
            "name": "subagent_status",
            "description": "Show one subagent, or all live subagents when name is omitted.",
            "inputSchema": {"type": "object", "properties": {"name": nullable_string}},
        },
        {
            "name": "subagent_down",
            "description": "Stop a subagent, close its presentation, and optionally archive state.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {"name": string, "archive": {"type": "boolean", "default": True}},
            },
        },
        {
            "name": "subagent_reset",
            "description": "Clear an idle subagent's harness context while keeping its presentation and state.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {"name": string},
            },
        },
        {
            "name": "subagent_recreate_window",
            "description": "Repair presentation for a live runner whose window is missing.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {"name": string},
            },
        },
        {
            "name": "subagent_migrate",
            "description": "Move an idle supported non-Codex agent between presentation backends.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": string,
                    "to_backend": {"type": "string", "enum": ["tmux", "herdr"], "default": "herdr"},
                    "to_mode": {"type": ["string", "null"], "enum": ["headless", "tui", None]},
                },
            },
        },
        {
            "name": "subagent_list",
            "description": "List all live subagents.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ]


class WebSocketBroadcaster:
    """Publish worker lifecycle and turn-completion events over a local WebSocket."""
    def __init__(self, port: int) -> None:
        self.port = port
        self._clients: set[asyncio.StreamWriter] = set()
        self._seen: set[str] = set()
        self._thread: Optional[threading.Thread] = None

    def start(self) -> None:
        """Start the event-stream listener in a background thread."""
        if self.port <= 0:
            return
        self._thread = threading.Thread(target=self._thread_main, daemon=True)
        self._thread.start()

    def _thread_main(self) -> None:
        try:
            asyncio.run(self._run())
        except Exception as exc:  # noqa: BLE001 - broadcaster must fail loudly.
            print(f"[subagents-mcp] websocket broadcaster failed: {exc}", file=sys.stderr)

    async def _run(self) -> None:
        server = await asyncio.start_server(self._handle_client, "127.0.0.1", self.port)
        async with server:
            await asyncio.gather(server.serve_forever(), self._watch_events(), self._watch_transcripts())

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            headers: dict[str, str] = {}
            while True:
                raw = await reader.readline()
                if raw in (b"", b"\r\n", b"\n"):
                    break
                line = raw.decode("utf-8", errors="replace").strip()
                if ":" in line:
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
            ws_key = headers.get("sec-websocket-key")
            if not ws_key:
                writer.close()
                await writer.wait_closed()
                return
            accept = base64.b64encode(
                hashlib.sha1((ws_key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
            ).decode()
            writer.write(
                (
                    "HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\n"
                    "Connection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
                ).encode()
            )
            await writer.drain()
            self._clients.add(writer)
            while not reader.at_eof():
                await reader.read(1024)
        finally:
            self._clients.discard(writer)
            writer.close()
            await writer.wait_closed()

    async def _broadcast(self, event: dict[str, Json]) -> None:
        key = self._dedupe_key(event)
        if key in self._seen:
            return
        self._seen.add(key)
        payload = json.dumps(event, sort_keys=True).encode("utf-8")
        frame = self._text_frame(payload)
        dead: list[asyncio.StreamWriter] = []
        for writer in self._clients:
            try:
                writer.write(frame)
                await writer.drain()
            except (ConnectionError, RuntimeError):
                dead.append(writer)
        for writer in dead:
            self._clients.discard(writer)

    def _dedupe_key(self, event: dict[str, Json]) -> str:
        if event.get("type") == "TURN-DONE":
            return f"TURN-DONE:{event.get('name')}:{event.get('seq')}:{event.get('rc')}"
        event_id = event.get("id")
        return str(event_id) if event_id is not None else json.dumps(event, sort_keys=True)

    def _text_frame(self, payload: bytes) -> bytes:
        size = len(payload)
        if size < 126:
            header = bytes([0x81, size])
        elif size <= 65535:
            header = bytes([0x81, 126]) + size.to_bytes(2, "big")
        else:
            header = bytes([0x81, 127]) + size.to_bytes(8, "big")
        return header + payload

    async def _watch_events(self) -> None:
        path = adapter.event_log_path()
        offset = path.stat().st_size if path.exists() else 0
        while True:
            if path.exists():
                with path.open() as fh:
                    fh.seek(offset)
                    while True:
                        line = fh.readline()
                        if line == "":
                            break
                        offset = fh.tell()
                        try:
                            event = json.loads(line)
                        except json.JSONDecodeError:
                            print(
                                f"[subagents-mcp] bad event JSON ignored: {line[:160]}",
                                file=sys.stderr,
                            )
                            continue
                        if isinstance(event, dict):
                            await self._broadcast(_as_dict(event))
            await asyncio.sleep(0.5)

    async def _watch_transcripts(self) -> None:
        offsets: dict[str, int] = {}
        while True:
            for name in adapter.registry_snapshot():
                path = adapter.transcript_path(name)
                if not path.exists():
                    continue
                if name not in offsets:
                    offsets[name] = path.stat().st_size
                with path.open() as fh:
                    fh.seek(offsets[name])
                    while True:
                        line = fh.readline()
                        if line == "":
                            break
                        offsets[name] = fh.tell()
                        match = TURN_DONE_RE.match(line.strip())
                        if match:
                            await self._broadcast(
                                {
                                    "type": "TURN-DONE",
                                    "name": name,
                                    "seq": int(match.group(1)),
                                    "rc": match.group(2),
                                    "ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                                    "preview": adapter.last_message_preview(name),
                                }
                            )
            await asyncio.sleep(1.0)


class McpServer:
    """Serve newline-delimited MCP JSON-RPC requests over configured text streams."""
    def __init__(self, stdin: TextIO, stdout: TextIO, tools: SubagentTools) -> None:
        self.stdin = stdin
        self.stdout = stdout
        self.tools = tools

    def serve(self) -> None:
        """Read requests until end-of-input and write protocol responses without mixing in diagnostics."""
        for line in self.stdin:
            if not line.strip():
                continue
            try:
                request = json.loads(line)
                response = self._handle(_as_dict(request))
            except Exception as exc:  # noqa: BLE001 - malformed client input.
                response = self._error(None, -32700, f"parse error: {exc}")
            if response is not None:
                self.stdout.write(json.dumps(response, sort_keys=True) + "\n")
                self.stdout.flush()

    def _handle(self, request: dict[str, Json]) -> Optional[dict[str, Json]]:
        method = request.get("method")
        request_id = request.get("id")
        if not isinstance(method, str):
            return self._error(request_id, -32600, "JSON-RPC request missing string method")
        if request_id is None and method.startswith("notifications/"):
            return None
        if method == "initialize":
            return self._result(
                request_id,
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "herdr-foreign-subagents", "version": "0.1.0"},
                },
            )
        if method == "ping":
            return self._result(request_id, {})
        if method == "tools/list":
            return self._result(request_id, {"tools": tool_schemas()})
        if method == "tools/call":
            return self._tools_call(request_id, _as_dict(request.get("params")))
        return self._error(request_id, -32601, f"method not found: {method}")

    def _tools_call(self, request_id: Json, params: dict[str, Json]) -> dict[str, Json]:
        tool_name = params.get("name")
        arguments = _as_dict(params.get("arguments"))
        if not isinstance(tool_name, str):
            return self._error(request_id, -32602, "tools/call requires string params.name")
        try:
            payload = self.tools.call(tool_name, arguments)
        except adapter.ToolFailure as exc:
            error_payload: dict[str, Json] = {
                "ok": False,
                "error": {"code": exc.code, "message": exc.message},
            }
            return self._result(
                request_id,
                {"content": [{"type": "text", "text": _json_text(error_payload)}], "isError": True},
            )
        return self._result(
            request_id,
            {
                "content": [{"type": "text", "text": _json_text(payload)}],
                "structuredContent": payload,
                "isError": False,
            },
        )

    def _result(self, request_id: Json, result: dict[str, Json]) -> dict[str, Json]:
        return {"jsonrpc": "2.0", "id": request_id, "result": result}

    def _error(self, request_id: Json, code: int, message: str) -> dict[str, Json]:
        return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def websocket_port_from_env() -> int:
    """Read the configured local event-stream port, rejecting invalid integer values."""
    raw = os.environ.get(WS_PORT_ENV, str(DEFAULT_WS_PORT))
    try:
        return int(raw)
    except ValueError as exc:
        raise SystemExit(f"{WS_PORT_ENV} must be an integer, got {raw!r}") from exc


def main() -> int:
    """Start the local event stream and serve MCP requests on standard input and output."""
    WebSocketBroadcaster(websocket_port_from_env()).start()
    McpServer(sys.stdin, sys.stdout, SubagentTools()).serve()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
