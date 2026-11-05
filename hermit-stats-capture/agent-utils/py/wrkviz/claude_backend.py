"""Strict, non-interactive Claude CLI adapter for structured summary work."""

from __future__ import annotations

import json
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from wrkviz.archive import (
    JsonValue,
    as_object,
    canonical_json,
    narrow_json,
)
from wrkviz.backend_process import BackendProcesses
from wrkviz.token_usage import TokenUsage, parse_claude_json_usage


class ClaudeBackendError(RuntimeError):
    """A Claude invocation failed, retaining any exact usage and raw result."""

    def __init__(
        self,
        message: str,
        *,
        usage: TokenUsage | None = None,
        raw_output: str | None = None,
    ) -> None:
        super().__init__(message)
        self.usage = usage
        self.raw_output = raw_output


@dataclass(frozen=True)
class ClaudeBackendResult:
    """Validated structured output and accounting from one Claude invocation."""

    output: str
    usage: TokenUsage
    raw_output: str


def _parse_root(text: str) -> dict[str, JsonValue]:
    try:
        raw: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise ValueError(f"invalid Claude JSON result: {error}") from error
    return as_object(narrow_json(raw, "Claude JSON result"), "Claude JSON result")


def _failure_detail(root: dict[str, JsonValue] | None, stderr: str) -> str:
    candidates: list[str] = []
    if root is not None:
        for key in ("error", "result"):
            value = root.get(key)
            if isinstance(value, str) and value.strip():
                candidates.append(" ".join(value.split()))
    if stderr.strip():
        candidates.append(" ".join(stderr.split()))
    detail = " — ".join(candidates)
    return detail[:500].rstrip()


def run_claude_json(
    command: Sequence[str],
    *,
    prompt: str,
    schema: dict[str, JsonValue],
    model: str,
    reasoning_effort: str | None,
    cwd: Path,
    processes: BackendProcesses | None = None,
) -> ClaudeBackendResult:
    """Run Claude once with tools/configuration disabled and require schema output.

    There is deliberately no model or backend fallback.  A provider, process,
    schema, or accounting failure aborts the batch before any cache artifact is
    published.
    """

    if not command:
        raise ClaudeBackendError("claude command must not be empty")
    # Claude's ``--json-schema`` parser accepts the schema vocabulary we use,
    # but currently rejects the Draft 2020-12 dialect declaration itself.
    # Keep the complete schema in the summarization layer for validation and
    # remove only this transport-incompatible metadata from the CLI argument.
    cli_schema = {key: value for key, value in schema.items() if key != "$schema"}
    args = [
        *command,
        "--print",
        "--output-format",
        "json",
        "--json-schema",
        canonical_json(cli_schema).strip(),
        "--model",
        model,
        "--safe-mode",
        "--no-session-persistence",
        "--permission-mode",
        "dontAsk",
        "--tools",
        "",
    ]
    if reasoning_effort is not None:
        args.extend(["--effort", reasoning_effort])
    owned_processes = processes or BackendProcesses()
    try:
        completed = owned_processes.run(args, input_text=prompt, cwd=cwd)
    except OSError as error:
        raise ClaudeBackendError(
            f"could not start claude backend: {error}"
        ) from error

    raw_output = completed.stdout.strip()
    root: dict[str, JsonValue] | None = None
    usage: TokenUsage | None = None
    if raw_output:
        try:
            root = _parse_root(raw_output)
            usage = parse_claude_json_usage(raw_output)
        except ValueError as error:
            if completed.returncode == 0:
                raise ClaudeBackendError(
                    f"claude backend returned malformed result: {error}",
                    raw_output=raw_output,
                ) from error
    if completed.returncode != 0:
        detail = _failure_detail(root, completed.stderr)
        suffix = f": {detail}" if detail else ""
        raise ClaudeBackendError(
            f"claude backend failed with exit {completed.returncode}{suffix}",
            usage=usage,
            raw_output=raw_output or None,
        )
    if root is None or usage is None:
        raise ClaudeBackendError("claude backend produced no JSON result")
    if root.get("type") != "result" or root.get("subtype") != "success":
        raise ClaudeBackendError(
            "claude backend did not report a successful result",
            usage=usage,
            raw_output=raw_output,
        )
    is_error = root.get("is_error")
    if not isinstance(is_error, bool) or is_error:
        raise ClaudeBackendError(
            "claude backend marked its result as an error",
            usage=usage,
            raw_output=raw_output,
        )
    try:
        structured = as_object(
            root.get("structured_output"), "Claude JSON result.structured_output"
        )
    except ValueError as error:
        raise ClaudeBackendError(
            f"claude backend returned malformed structured output: {error}",
            usage=usage,
            raw_output=raw_output,
        ) from error
    # Preserve one canonical JSON representation for the existing strict schema
    # parsers and the immutable backend-output audit trail.
    output = canonical_json(structured)
    return ClaudeBackendResult(output=output, usage=usage, raw_output=raw_output)


__all__ = ["ClaudeBackendError", "ClaudeBackendResult", "run_claude_json"]
