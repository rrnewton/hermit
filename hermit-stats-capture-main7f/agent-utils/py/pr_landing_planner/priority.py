"""Pluggable PR priority sources for landing order (lower number = more urgent).

Ordering among "land-now" PRs is priority -> size -> age. The priority itself is pluggable:

* ``none``   — every PR is priority 0 (order falls back to size then age); the default.
* ``labels`` — parse a priority integer from a PR label matching a caller pattern (e.g. ``p0`` or
  ``priority-2``). This is pure (reads :attr:`RawPr.labels`).
* ``command`` — run a caller-supplied shell command per PR that prints an integer. This is the one
  impure priority source and lives behind the same protocol so the pure core never depends on it.
"""

from __future__ import annotations

import re
import subprocess
from collections.abc import Sequence
from typing import Protocol

DEFAULT_PRIORITY = 100
#: Default label pattern: ``p0``..``p9`` or ``priority-N`` / ``priority:N`` (case-insensitive).
DEFAULT_LABEL_PATTERN = r"^(?:p|priority[:-])(\d+)$"
_I64_TOKEN = re.compile(r"[+-]?[0-9]+\Z")
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1


def _parse_i64(value: str) -> int | None:
    """Parse the shared signed-i64 ASCII grammar."""
    if _I64_TOKEN.fullmatch(value) is None:
        return None
    parsed = int(value)
    return parsed if _I64_MIN <= parsed <= _I64_MAX else None


class PriorityProvider(Protocol):
    """Resolve a landing priority for one PR (lower = more urgent)."""

    last_error: str | None

    def priority(self, pr_number: int, labels: Sequence[str]) -> int:
        """Return the landing priority for one pull request; lower values are more urgent."""

        ...


class NonePriority:
    """Every PR is equal priority; ordering falls back to size then age."""

    last_error: str | None = None

    def priority(self, pr_number: int, labels: Sequence[str]) -> int:
        """Return equal priority for every pull request."""

        return 0


class LabelPriority:
    """Derive priority from a label matching ``pattern`` (first capture group = the integer)."""

    def __init__(self, pattern: str = DEFAULT_LABEL_PATTERN, default: int = DEFAULT_PRIORITY) -> None:
        self._regex = re.compile(pattern, re.IGNORECASE)
        if self._regex.groups < 1:
            raise ValueError("priority label pattern must contain a capture group")
        self._default = default
        self.last_error: str | None = None

    def priority(self, pr_number: int, labels: Sequence[str]) -> int:
        """Return the most urgent matching label priority, or the configured default."""

        best: int | None = None
        for label in labels:
            match = self._regex.match(label)
            if match is not None:
                captured = match.group(1)
                if captured is None:
                    self.last_error = (
                        f"priority label pattern matched without a captured value for #{pr_number}"
                    )
                    continue
                value = _parse_i64(captured)
                if value is None:
                    self.last_error = (
                        f"priority label for #{pr_number} is not a signed 64-bit ASCII integer"
                    )
                    continue
                best = value if best is None else min(best, value)
        return self._default if best is None else best


class CommandPriority:
    """Run ``cmd`` (with ``{pr}`` substituted) per PR; its stdout must be an integer priority.

    On any failure it falls back to ``default`` and does not crash the plan, but the fallback is
    visible via :attr:`last_error` so a caller can surface it.
    """

    def __init__(
        self,
        cmd: str,
        wrapper: Sequence[str] = (),
        default: int = DEFAULT_PRIORITY,
        timeout: int = 20,
    ) -> None:
        self._cmd = cmd
        self._wrapper = tuple(wrapper)
        self._default = default
        self._timeout = timeout
        self.last_error: str | None = None

    def priority(self, pr_number: int, labels: Sequence[str]) -> int:
        """Run the configured command and return its priority or the configured default."""

        rendered = self._cmd.replace("{pr}", str(pr_number))
        argv = [*self._wrapper, "bash", "-c", rendered]
        try:
            proc = subprocess.run(
                argv,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=self._timeout,
            )
        except subprocess.TimeoutExpired:
            self.last_error = f"priority command timed out for #{pr_number}"
            return self._default
        except OSError:
            self.last_error = f"priority command failed to start for #{pr_number}"
            return self._default
        text = proc.stdout.strip()
        if proc.returncode != 0:
            returncode = proc.returncode if proc.returncode >= 0 else -1
            self.last_error = f"priority command for #{pr_number} exited {returncode}"
            return self._default
        if not text:
            self.last_error = f"priority command for #{pr_number} produced empty output"
            return self._default
        value = _parse_i64(text)
        if value is None:
            self.last_error = (
                f"priority command for #{pr_number} did not print a signed 64-bit ASCII integer"
            )
            return self._default
        return value


def make_priority_provider(
    source: str,
    *,
    label_pattern: str = DEFAULT_LABEL_PATTERN,
    command: str = "",
    wrapper: Sequence[str] = (),
) -> PriorityProvider:
    """Build a provider, rejecting missing commands and malformed label expressions."""
    if source == "none":
        return NonePriority()
    if source == "labels":
        return LabelPriority(label_pattern)
    if source == "command":
        if not command.strip():
            raise ValueError("command priority source requires a non-empty command")
        return CommandPriority(command, wrapper)
    raise ValueError(f"unknown priority source {source!r} (want none|labels|command)")


__all__ = [
    "PriorityProvider",
    "NonePriority",
    "LabelPriority",
    "CommandPriority",
    "make_priority_provider",
    "DEFAULT_PRIORITY",
    "DEFAULT_LABEL_PATTERN",
]
