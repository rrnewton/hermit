"""Installed command surface tests for wrkslots."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
COMPATIBILITY_COMMAND = PACKAGE_ROOT.parent / "wrkslots.py"


def _run(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "wrkslots", *args],
        cwd=PACKAGE_ROOT.parent,
        text=True,
        capture_output=True,
        check=False,
    )


def test_quickstart_teaches_the_normal_lifecycle() -> None:
    completed = _run("quickstart")

    assert completed.returncode == 0, completed.stderr
    for command in ("init", "create", "heartbeat", "finish", "remove", "recover"):
        assert f"wrkslots {command}" in completed.stdout
    assert "--remote product=upstream" in completed.stdout
    assert "--remote-url product=URL" in completed.stdout


def test_pre_packaging_repository_command_still_runs() -> None:
    completed = subprocess.run(
        [sys.executable, str(COMPATIBILITY_COMMAND), "--version"],
        cwd=PACKAGE_ROOT.parent,
        text=True,
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    assert completed.stdout.strip() == "wrkslots 0.6.0"


def test_every_command_help_explains_effect_and_inputs() -> None:
    commands = (
        "init",
        "status",
        "create",
        "register",
        "import-existing",
        "adopt",
        "recover-unbound-owner",
        "relocate-repository",
        "heartbeat",
        "finish",
        "remove",
        "recover",
    )
    for command in commands:
        completed = _run(command, "--help")
        assert completed.returncode == 0, completed.stderr
        assert "options:" in completed.stdout
        assert len(completed.stdout.splitlines()) >= 8

    create = _run("create", "--help").stdout
    assert "linked Git worktree" in create
    assert "configured origin" in create
    assert "--remote NAME=REMOTE" in create
    assert "expected fetch URL" in create
    assert "--remote-url NAME=URL" in create
    normalized_create = " ".join(create.split())
    assert "another child of the invoking coordinator" in normalized_create
    assert "even when its assigned coordinator runs separately" in normalized_create
    assert "unless the owner is in this command's ancestry" in normalized_create
    assert "whose lifetime represents this one agent" in normalized_create
    assert "do not use a shared multiplexing supervisor" in normalized_create
    assert "or ../NAME[/PATH] path components inside one direct sibling" in normalized_create
    assert (
        "absolute paths, other parent traversal, and symlink components are refused"
        in normalized_create
    )

    imported = _run("import-existing", "--help").stdout
    assert "--purpose 'continue task-123'" in imported
    assert "--coordinator-pid PID" in imported
    assert "--slot-type agent --coordinator-authorized" in imported

    init = _run("init", "--help").stdout
    assert "called as PATH AGENT" in init
    assert "0 dead, 1 alive, or 2 unverifiable" in init


def test_userguide_is_the_packaged_document() -> None:
    completed = _run("--userguide")

    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == (PACKAGE_ROOT / "USER_GUIDE.md").read_text(encoding="utf-8")
