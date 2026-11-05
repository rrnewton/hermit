"""Required real-process end-to-end smoke test."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

SYSTEM_GIT = Path("/usr/bin/git")


def _prefer_system_git(directory: Path) -> dict[str, str]:
    """Return the inherited environment with only Git redirected to the system executable."""

    environment = os.environ.copy()
    # Optional host wrappers may detach telemetry after their foreground Git exits. Inside the
    # PID namespace that child is adopted by the init, which must then reap a process this
    # workload never started within a grace period sized for the workload's own descendants.
    # Where there is no system Git, keep the inherited one rather than skip the check.
    if not SYSTEM_GIT.exists():
        return environment
    directory.mkdir()
    (directory / "git").symlink_to(SYSTEM_GIT)
    inherited_path = environment.get("PATH", "")
    environment["PATH"] = os.pathsep.join((str(directory), inherited_path))
    resolved_git = shutil.which("git", path=environment["PATH"])
    assert resolved_git is not None
    assert Path(resolved_git).resolve() == SYSTEM_GIT.resolve()
    assert shutil.which("python3", path=environment["PATH"]) == shutil.which(
        "python3", path=inherited_path
    )
    return environment


def test_real_process_and_git_invariants(tmp_path: Path) -> None:
    environment = _prefer_system_git(tmp_path / "system-git")
    runner = Path(__file__).with_name("e2e_stress.py")
    command = [
        sys.executable,
        str(runner),
        "--seed",
        "7",
        "--workers",
        "2",
        "--seconds",
        "0.15",
    ]
    namespace = subprocess.run(
        ["unshare", "--user", "--map-root-user", "--pid", "--fork", "--mount-proc", "true"],
        text=True,
        capture_output=True,
        check=False,
        env=environment,
    )
    if namespace.returncode == 0:
        init = Path(__file__).resolve().parents[3] / "scripts" / "pid_namespace_init.py"
        command = [
            "unshare",
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
            str(Path(sys.executable).resolve()),
            str(init),
            "--",
            *command,
        ]
    completed = subprocess.run(
        command,
        text=True,
        capture_output=True,
        check=False,
        timeout=600,
        env=environment,
    )

    assert completed.returncode == 0, f"{completed.stdout}\n{completed.stderr}"
    assert "wrkslots e2e passed; seed=7" in completed.stdout
