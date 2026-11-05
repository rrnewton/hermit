"""Isolated repository-backed workspaces for non-interactive Codex calls."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Final


_DIAGNOSTIC_HINTS: Final = (
    "error",
    "fail",
    "abort",
    "404",
    "421",
    "unavailable",
    "not found",
    "does not exist",
    "repository",
    "deployment",
)


class CodexWorkspaceError(RuntimeError):
    """A temporary Codex workspace could not be made repository-backed."""


def _one_line(text: str) -> str:
    return " ".join(text.strip().split())


def _bounded_head_tail(text: str, limit: int) -> str:
    if limit < 5:
        raise ValueError("failure detail limit must be at least 5")
    if len(text) <= limit:
        return text
    marker = " … "
    available = limit - len(marker)
    head_length = available // 2
    tail_length = available - head_length
    return (
        text[:head_length].rstrip()
        + marker
        + text[-tail_length:].lstrip()
    )


def codex_failure_detail(stdout: str, stderr: str, *, limit: int = 320) -> str:
    """Return the most actionable bounded backend diagnostic.

    Codex JSONL commonly repeats reconnect errors before ending with a
    ``turn.failed`` record, while launcher banners are written to stderr.  Keep
    the final structured diagnostic by itself when one exists so a banner and
    repeated retries cannot displace the HTTP status and provider message.
    """

    stderr_detail = _one_line(stderr)
    stdout_lines = tuple(_one_line(line) for line in stdout.splitlines() if line.strip())
    diagnostic_stdout = tuple(
        line
        for line in stdout_lines
        if any(hint in line.casefold() for hint in _DIAGNOSTIC_HINTS)
    )
    if diagnostic_stdout:
        return _bounded_head_tail(f"stdout: {diagnostic_stdout[-1]}", limit)
    if stderr_detail:
        return _bounded_head_tail(stderr_detail, limit)
    if stdout_lines:
        return _bounded_head_tail(stdout_lines[-1], limit)
    return ""


def _run_initializer(command: tuple[str, ...], work_dir: Path, label: str) -> None:
    try:
        completed = subprocess.run(
            command,
            cwd=work_dir,
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError as error:
        raise CodexWorkspaceError(
            f"could not start {label} while preparing {work_dir}: {error}"
        ) from error
    if completed.returncode == 0:
        return
    detail = _one_line(completed.stderr or completed.stdout)
    suffix = f": {_bounded_head_tail(detail, 240)}" if detail else ""
    raise CodexWorkspaceError(
        f"{label} init failed with exit {completed.returncode}{suffix}"
    )


def initialize_codex_workspace(work_dir: Path) -> None:
    """Initialize ``work_dir`` as an isolated repository-backed workspace.

    Some Codex launchers require their process working directory to be inside a
    repository even when ``--skip-git-repo-check`` is present.  Initializing the
    already-temporary directory as Git plus the first installed ``hg``/``sl``
    compatibility repository satisfies both public and wrapped launchers.  No
    caller checkout is modified, and ``TemporaryDirectory`` still removes the
    repository with the schema and output files.  Hosts without ``hg`` or
    ``sl`` retain the ordinary Git-only behavior.
    """

    _run_initializer(("git", "init", "--quiet"), work_dir, "git")
    for compatibility_command in ("hg", "sl"):
        executable = shutil.which(compatibility_command)
        if executable is None:
            continue
        _run_initializer((executable, "init"), work_dir, compatibility_command)
        break
