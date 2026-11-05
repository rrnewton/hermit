"""Read and update native Codex goals through the app-server JSON-RPC protocol.

The default command attaches to an existing local app-server daemon. An explicit
``command=("codex", "app-server", "--stdio")`` can query saved goals on versions
that expose them without loading the thread. No operation in this module starts,
resumes, or submits input to a thread. Updating a goal through a separate server
does not by itself promise to wake another process that owns that thread.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import select
import selectors
import signal
import subprocess
import sys
import time
from collections.abc import Sequence

from agentctl.jsonx import as_mapping, as_sequence, get_int, get_str

__all__ = ["CodexGoalError", "get_goal", "set_goal", "clear_goal", "main"]

DEFAULT_COMMAND = ("codex", "app-server", "proxy")
DEFAULT_TIMEOUT_SECONDS = 30.0
_MAX_MESSAGE_BYTES = 1024 * 1024
_STATUSES = ("active", "paused", "blocked", "usageLimited", "budgetLimited", "complete")


class CodexGoalError(RuntimeError):
    """The native goal API was unavailable, refused a request, or changed shape."""


class _Rpc:
    """One bounded protocol connection, including launcher process cleanup."""

    def __init__(self, command: Sequence[str], timeout: float) -> None:
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be positive and finite")
        if isinstance(command, (str, bytes)) or not command or any(
            not part or "\0" in part for part in command
        ):
            raise ValueError("command must be a nonempty argument vector")
        self._timeout = timeout
        self._deadline = time.monotonic() + timeout
        self._stdout = bytearray()
        self._stderr = bytearray()
        self._stdout_open = True
        self._selector = selectors.DefaultSelector()
        try:
            self._process = subprocess.Popen(
                list(command),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
        except OSError as exc:
            self._selector.close()
            detail = (
                f"{exc.strerror} (os error {exc.errno})"
                if exc.errno is not None and exc.strerror is not None
                else str(exc)
            )
            raise CodexGoalError(f"cannot start Codex goal transport: {detail}") from exc
        assert self._process.stdin is not None
        assert self._process.stdout is not None
        assert self._process.stderr is not None
        self._input = self._process.stdin
        self._output = self._process.stdout
        self._errors = self._process.stderr
        try:
            for stream in (self._input, self._output, self._errors):
                os.set_blocking(stream.fileno(), False)
            self._selector.register(self._output, selectors.EVENT_READ, "stdout")
            self._selector.register(self._errors, selectors.EVENT_READ, "stderr")
        except OSError:
            self.close()
            raise

    def _remaining(self) -> float:
        remaining = self._deadline - time.monotonic()
        if remaining <= 0:
            raise CodexGoalError(
                f"Codex goal RPC timed out after {self._timeout:g} seconds"
            )
        return remaining

    def send(self, message: dict[str, object]) -> None:
        encoded = (json.dumps(message, ensure_ascii=False) + "\n").encode("utf-8")
        if len(encoded) > _MAX_MESSAGE_BYTES:
            raise CodexGoalError("Codex goal request exceeds the message size limit")
        offset = 0
        try:
            while offset < len(encoded):
                _, writable, _ = select.select(
                    [], [self._input.fileno()], [], self._remaining()
                )
                if writable:
                    try:
                        offset += os.write(self._input.fileno(), encoded[offset:])
                    except BlockingIOError:
                        continue
        except OSError as exc:
            raise CodexGoalError(f"cannot write Codex goal request: {exc}") from exc

    def _read_available(self) -> None:
        for key, _ in self._selector.select(self._remaining()):
            try:
                chunk = os.read(key.fd, 65536)
            except BlockingIOError:
                continue
            if not chunk:
                self._selector.unregister(key.fd)
                if key.data == "stdout":
                    self._stdout_open = False
            elif key.data == "stdout":
                self._stdout.extend(chunk)
                if len(self._stdout) > _MAX_MESSAGE_BYTES:
                    raise CodexGoalError("Codex goal response exceeds the message size limit")
            else:
                self._stderr.extend(chunk)
                del self._stderr[:-4096]

    def receive(self, request_id: int) -> dict[str, object]:
        while True:
            self._remaining()
            end = self._stdout.find(b"\n")
            if end >= 0:
                line = bytes(self._stdout[:end])
                del self._stdout[: end + 1]
                try:
                    document: object = json.loads(line)
                    response = as_mapping(document, "Codex goal response")
                except (ValueError, UnicodeError, TypeError) as exc:
                    raise CodexGoalError(f"invalid Codex goal response: {exc}") from exc
                if "method" in response:
                    if "id" in response:
                        raise CodexGoalError(
                            "Codex requested an interactive action during goal RPC; "
                            "no approval was sent"
                        )
                    continue
                response_id = response.get("id")
                if type(response_id) is not int or response_id != request_id:
                    raise CodexGoalError("Codex returned an unexpected response ID")
                if "error" in response:
                    error = as_mapping(response["error"], "Codex RPC error")
                    raise CodexGoalError(
                        f"Codex goal RPC refused: {error.get('message', error)}"
                    )
                return as_mapping(response.get("result"), "Codex goal result")
            if not self._stdout_open:
                detail = self._stderr.decode("utf-8", errors="replace").strip()
                raise CodexGoalError(
                    "Codex goal transport closed before replying"
                    + (f": {detail}" if detail else "")
                )
            self._read_available()

    def close(self) -> None:
        # A proxy's separately running daemon is not in this new process group.
        # A launcher and its helpers are, and must not survive a timeout.
        try:
            os.killpg(self._process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self._process.wait(timeout=0.5)
        except subprocess.TimeoutExpired:
            pass
        finally:
            try:
                os.killpg(self._process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self._process.wait()
            self._selector.close()
            self._input.close()
            self._output.close()
            self._errors.close()


def _call(
    method: str,
    params: dict[str, object],
    command: Sequence[str] | None,
    timeout: float,
) -> dict[str, object]:
    rpc = _Rpc(DEFAULT_COMMAND if command is None else command, timeout)
    try:
        rpc.send({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "herdr-goal", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        })
        rpc.receive(1)
        rpc.send({"method": "initialized", "params": {}})
        rpc.send({"id": 2, "method": method, "params": params})
        return rpc.receive(2)
    except (TypeError, ValueError, OSError) as exc:
        raise CodexGoalError(f"invalid Codex goal protocol: {exc}") from exc
    finally:
        rpc.close()


def _session_params(session_id: str) -> dict[str, object]:
    if not session_id or "\0" in session_id:
        raise ValueError("session_id must be a nonempty session identifier")
    return {"threadId": session_id}


def _goal(value: object, session_id: str) -> dict[str, object]:
    try:
        goal = as_mapping(value, "native Codex goal")
        if get_str(goal, "threadId", "native Codex goal") != session_id:
            raise TypeError("native Codex goal belongs to another session")
        get_str(goal, "objective", "native Codex goal")
        if get_str(goal, "status", "native Codex goal") not in _STATUSES:
            raise TypeError("native Codex goal has an unknown status")
        for key in ("tokensUsed", "timeUsedSeconds", "createdAt", "updatedAt"):
            get_int(goal, key, "native Codex goal")
        if goal.get("tokenBudget") is not None:
            get_int(goal, "tokenBudget", "native Codex goal")
        return goal
    except TypeError as exc:
        raise CodexGoalError(str(exc)) from exc


def get_goal(
    session_id: str,
    command: Sequence[str] | None = None,
    *,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, object] | None:
    """Return the native goal, or None when absent, without resuming the thread."""
    response = _call("thread/goal/get", _session_params(session_id), command, timeout)
    value = response.get("goal")
    return None if value is None else _goal(value, session_id)


def set_goal(
    session_id: str,
    objective: str | None = None,
    status: str | None = None,
    token_budget: int | None = None,
    command: Sequence[str] | None = None,
    *,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, object]:
    """Update the native goal, sending only the fields explicitly provided.

    Use a transport attached to the owning server for live goal notifications.
    A separate stdio server can update saved state but does not guarantee a wakeup.
    """
    params = _session_params(session_id)
    if objective is not None:
        if not objective.strip():
            raise ValueError("objective must not be blank")
        params["objective"] = objective
    if status is not None:
        if status not in _STATUSES:
            raise ValueError("unsupported native goal status")
        params["status"] = status
    if token_budget is not None:
        if isinstance(token_budget, bool) or token_budget <= 0:
            raise ValueError("token_budget must be a positive integer")
        params["tokenBudget"] = token_budget
    if len(params) == 1:
        raise ValueError("provide an objective, status, or token budget")
    response = _call("thread/goal/set", params, command, timeout)
    return _goal(response.get("goal"), session_id)


def clear_goal(
    session_id: str,
    command: Sequence[str] | None = None,
    *,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> None:
    """Clear the saved goal without submitting a prompt or resuming the thread."""
    _call("thread/goal/clear", _session_params(session_id), command, timeout)


def _main(argv: Sequence[str] | None = None) -> int:
    """Run with ``python -m agentctl.codex_goal [--stdio] get SESSION_ID``."""
    parser = argparse.ArgumentParser(description=__doc__)
    transport = parser.add_mutually_exclusive_group()
    transport.add_argument("--stdio", action="store_true", help="use a separate app-server process")
    transport.add_argument("--command-json", help="explicit transport argv as a JSON string array")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    commands = parser.add_subparsers(dest="action", required=True)
    commands.add_parser("get").add_argument("session_id")
    setter = commands.add_parser("set")
    setter.add_argument("session_id")
    setter.add_argument("--objective")
    setter.add_argument("--status", choices=_STATUSES)
    setter.add_argument("--token-budget", type=int)
    commands.add_parser("clear").add_argument("session_id")
    args = parser.parse_args(argv)
    command: Sequence[str] | None = ("codex", "app-server", "--stdio") if args.stdio else None
    try:
        if args.command_json is not None:
            decoded: object = json.loads(args.command_json)
            parts = as_sequence(decoded, "command")
            if any(not isinstance(part, str) for part in parts):
                raise ValueError("command must contain only strings")
            command = [str(part) for part in parts]
        result: dict[str, object] | None
        if args.action == "get":
            result = get_goal(args.session_id, command, timeout=args.timeout)
        elif args.action == "set":
            result = set_goal(
                args.session_id, args.objective, args.status, args.token_budget,
                command, timeout=args.timeout,
            )
        else:
            clear_goal(args.session_id, command, timeout=args.timeout)
            result = None
        print(json.dumps(result, ensure_ascii=False))
        return 0
    except (CodexGoalError, ValueError, TypeError) as exc:
        print(f"native goal: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(_main())
