"""Regression tests for interrupting token-spending timeline operations."""

from __future__ import annotations

import fcntl
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import NoReturn

import pytest

from wrkviz import cli as timeline_cli
from wrkviz import pipeline as timeline_pipeline
from wrkviz.archive import as_object, read_json


def _process_is_dead(process_id: int) -> bool:
    stat_path = Path("/proc") / str(process_id) / "stat"
    try:
        stat = stat_path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return True
    suffix = stat.split(") ", 1)
    return len(suffix) == 2 and suffix[1].split()[0] == "Z"


def _wait_for_files(directory: Path, count: int, timeout_seconds: float) -> list[Path]:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        paths = sorted(directory.glob("*.pids"))
        if len(paths) >= count:
            return paths
        time.sleep(0.02)
    return sorted(directory.glob("*.pids"))


def _wait_for_processes_to_die(
    process_ids: list[int], timeout_seconds: float
) -> bool:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        if all(_process_is_dead(process_id) for process_id in process_ids):
            return True
        time.sleep(0.02)
    return all(_process_is_dead(process_id) for process_id in process_ids)


def test_sigint_stops_backend_process_groups_without_executor_lock_error(
    tmp_path: Path,
) -> None:
    pid_dir = tmp_path / "pids"
    pid_dir.mkdir()
    fake_claude = tmp_path / "fake-claude"
    fake_claude.write_text(
        """#!/usr/bin/env python3
import os
import subprocess
import sys
import time
from pathlib import Path

sys.stdin.read()
child = subprocess.Popen(
    [sys.executable, "-c", "import time; time.sleep(60)"],
)
pid_path = Path(os.environ["TIMELINE_TEST_PID_DIR"]) / f"{os.getpid()}.pids"
pid_path.write_text(f"{os.getpid()} {child.pid}\\n", encoding="utf-8")
while True:
    time.sleep(60)
""",
        encoding="utf-8",
    )
    fake_claude.chmod(0o755)
    harness = tmp_path / "interrupt-harness.py"
    harness.write_text(
        """from pathlib import Path
import sys

from wrkviz.summarize import SummaryJob, summarize_jobs

jobs = [
    SummaryJob(
        key=f"phase-{index}",
        team_slug="test-team",
        agent_label=f"agent-{index}",
        start_ms=1_800_000_000_000 + index,
        end_ms=1_800_000_060_000 + index,
        prior_context="The user requested an interrupt-safe summary pipeline.",
        transcript="The agent tested backend cleanup after interruption.",
        glossary="",
        stats={"messages": 2},
    )
    for index in range(2)
]
try:
    summarize_jobs(
        jobs,
        Path(sys.argv[1]),
        backend="claude",
        model="claude-test",
        max_workers=2,
        batch_size=1,
        claude_command=(sys.argv[2],),
    )
except KeyboardInterrupt:
    raise SystemExit(130)
""",
        encoding="utf-8",
    )
    environment = os.environ.copy()
    package_root = str(Path(__file__).parents[1])
    old_python_path = environment.get("PYTHONPATH")
    environment["PYTHONPATH"] = (
        package_root
        if old_python_path is None
        else package_root + os.pathsep + old_python_path
    )
    environment["TIMELINE_TEST_PID_DIR"] = str(pid_dir)
    process = subprocess.Popen(
        [
            sys.executable,
            str(harness),
            str(tmp_path / "cache"),
            str(fake_claude),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
        start_new_session=True,
    )
    process_ids: list[int] = []
    try:
        pid_paths = _wait_for_files(pid_dir, 2, 10.0)
        assert len(pid_paths) == 2
        for pid_path in pid_paths:
            process_ids.extend(
                int(value) for value in pid_path.read_text(encoding="utf-8").split()
            )
        process.send_signal(signal.SIGINT)
        _, stderr = process.communicate(timeout=10.0)
        assert process.returncode == 130
        assert "release unlocked lock" not in stderr
        assert "Traceback" not in stderr
        assert _wait_for_processes_to_die(process_ids, 5.0)
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5.0)
        for process_id in process_ids[::2]:
            try:
                os.killpg(process_id, signal.SIGKILL)
            except ProcessLookupError:
                pass


def test_cli_interrupt_receipt_is_written_after_archive_lock_release(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    archive = tmp_path / "archive"

    def interrupt_summary(*args: object, **kwargs: object) -> NoReturn:
        del args, kwargs
        raise KeyboardInterrupt

    monkeypatch.setattr(
        timeline_pipeline, "_summarize_archive_locked", interrupt_summary
    )

    result = timeline_cli.main(
        (
            "summarize",
            "--output",
            str(archive),
            "--team",
            "test-team",
            "--backend",
            "heuristic",
            "--model",
            "offline",
        )
    )

    assert result == 130
    stderr = capsys.readouterr().err
    assert "interrupted by user" in stderr
    run_paths = tuple((archive / "runs").glob("*.json"))
    assert len(run_paths) == 1
    run = as_object(read_json(run_paths[0]), str(run_paths[0]))
    assert run["status"] == "interrupted"
    assert run["error"] == "interrupted by user"
    manifest_path = archive / "manifest.json"
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    assert manifest["last_run_status"] == "interrupted"

    descriptor = os.open(
        archive / ".wrkviz.lock",
        os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW,
    )
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(descriptor, fcntl.LOCK_UN)
    finally:
        os.close(descriptor)
