"""Security and resource bounds for managed agentctl skill installation."""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

from agentctl import skill_install
from agentctl.errors import AgentDeliveryError


def _executable(path: Path, script: str) -> Path:
    path.write_text(f"#!/bin/sh\n{script}\n", encoding="utf-8")
    path.chmod(0o700)
    return path


def test_muse_executable_does_not_search_hostile_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    planted = _executable(tmp_path / "muse", f"touch {tmp_path / 'executed'}")
    monkeypatch.setenv("PATH", str(tmp_path))
    monkeypatch.delenv("AGENTCTL_MUSE_BIN", raising=False)
    monkeypatch.setattr(skill_install, "_muse_executable_candidates", lambda: ())

    with pytest.raises(AgentDeliveryError, match="Muse executable not found"):
        skill_install._muse_executable()

    assert planted.is_file()
    assert not (tmp_path / "executed").exists()


def test_muse_installer_kills_a_process_group_that_exceeds_output_bound(
    tmp_path: Path,
) -> None:
    executable = _executable(
        tmp_path / "muse-output",
        f"head -c {skill_install._INSTALL_OUTPUT_BYTES + 1} /dev/zero",
    )

    with pytest.raises(AgentDeliveryError, match="output exceeds"):
        skill_install._run_muse_installer([str(executable)], dict(os.environ))


def test_muse_installer_kills_a_process_group_at_deadline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    executable = _executable(tmp_path / "muse-sleep", "sleep 10")
    monkeypatch.setattr(skill_install, "_INSTALL_TIMEOUT", 0.05)

    with pytest.raises(subprocess.TimeoutExpired):
        skill_install._run_muse_installer([str(executable)], dict(os.environ))
