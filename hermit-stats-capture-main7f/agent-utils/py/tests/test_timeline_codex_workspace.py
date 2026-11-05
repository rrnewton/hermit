"""Contracts for isolated repository-backed Codex workspaces."""

from __future__ import annotations

import subprocess
from collections.abc import Sequence
from pathlib import Path

import pytest

from wrkviz.codex_workspace import (
    CodexWorkspaceError,
    codex_failure_detail,
    initialize_codex_workspace,
)


def _completed(
    command: Sequence[str], returncode: int = 0, stderr: str = ""
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(command, returncode, "", stderr)


def test_workspace_initializes_git_and_available_hg(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[tuple[str, ...]] = []

    def fake_run(
        command: Sequence[str],
        *,
        cwd: Path,
        text: bool,
        capture_output: bool,
        check: bool,
    ) -> subprocess.CompletedProcess[str]:
        assert cwd == tmp_path
        assert text and capture_output and not check
        normalized = tuple(command)
        calls.append(normalized)
        return _completed(normalized)

    def fake_which(command: str) -> str | None:
        return "/tools/hg" if command == "hg" else None

    monkeypatch.setattr("wrkviz.codex_workspace.subprocess.run", fake_run)
    monkeypatch.setattr("wrkviz.codex_workspace.shutil.which", fake_which)

    initialize_codex_workspace(tmp_path)

    assert calls == [("git", "init", "--quiet"), ("/tools/hg", "init")]


def test_workspace_remains_portable_without_hg_or_sl(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[tuple[str, ...]] = []

    def fake_run(
        command: Sequence[str],
        *,
        cwd: Path,
        text: bool,
        capture_output: bool,
        check: bool,
    ) -> subprocess.CompletedProcess[str]:
        del cwd, text, capture_output, check
        normalized = tuple(command)
        calls.append(normalized)
        return _completed(normalized)

    monkeypatch.setattr("wrkviz.codex_workspace.subprocess.run", fake_run)
    monkeypatch.setattr(
        "wrkviz.codex_workspace.shutil.which", lambda _command: None
    )

    initialize_codex_workspace(tmp_path)

    assert calls == [("git", "init", "--quiet")]


@pytest.mark.parametrize(
    ("failing_command", "expected"),
    [
        ("git", "git init failed with exit 5"),
        ("/tools/hg", "hg init failed with exit 5"),
    ],
)
def test_workspace_reports_initializer_failure(
    failing_command: str,
    expected: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fake_run(
        command: Sequence[str],
        *,
        cwd: Path,
        text: bool,
        capture_output: bool,
        check: bool,
    ) -> subprocess.CompletedProcess[str]:
        del cwd, text, capture_output, check
        normalized = tuple(command)
        if normalized[0] == failing_command:
            return _completed(normalized, 5, "initializer diagnostic")
        return _completed(normalized)

    monkeypatch.setattr("wrkviz.codex_workspace.subprocess.run", fake_run)
    monkeypatch.setattr(
        "wrkviz.codex_workspace.shutil.which",
        lambda command: "/tools/hg" if command == "hg" else None,
    )

    with pytest.raises(CodexWorkspaceError, match=expected):
        initialize_codex_workspace(tmp_path)


def test_failure_detail_prefers_final_structured_provider_error() -> None:
    provider_error = (
        "unexpected status 404 Not Found: The API deployment for this resource "
        "does not exist. If you created the deployment within the last 5 minutes, "
        "please wait a moment and try again."
    )
    stdout = "\n".join(
        (
            '{"type":"thread.started","thread_id":"test"}',
            '{"type":"error","message":"Reconnecting... 1/5 ('
            + provider_error
            + ')"}',
            '{"type":"error","message":"Reconnecting... 5/5 ('
            + provider_error
            + ')"}',
            '{"type":"turn.failed","error":{"message":"'
            + provider_error
            + '"}}',
        )
    )

    detail = codex_failure_detail(
        stdout,
        "Codex CLI at Meta. Using AI Gateway. Promotional launcher banner.",
    )

    assert "404 Not Found" in detail
    assert "does not exist" in detail
    assert "turn.failed" in detail
    assert "Promotional launcher banner" not in detail
    assert len(detail) <= 320


def test_failure_detail_retains_stderr_when_jsonl_has_no_diagnostic() -> None:
    detail = codex_failure_detail(
        '{"type":"thread.started","thread_id":"test"}\n',
        "backend exited before the first response",
    )
    assert detail == "backend exited before the first response"
