from __future__ import annotations

import csv
import fcntl
import json
import os
import shutil
import socket
import stat
import subprocess
import time
from collections.abc import Sequence
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
FETCHER_DIR = REPO_ROOT / "scripts" / "agent-log-archive"
KNOWN_SSH_BANNER = (
    "Meta authorized users only. Usage is subject to monitoring and recording."
)


def _install_archive(
    tmp_path: Path,
    machine_rows: str,
    root_rows: str,
) -> Path:
    archive = tmp_path / "archive"
    archive.mkdir(mode=0o700)
    shutil.copy2(FETCHER_DIR / "fetch_agent_logs.py", archive)
    shutil.copy2(FETCHER_DIR / "fetch_agent_logs.sh", archive)
    (archive / "machines.tsv").write_text(machine_rows, encoding="utf-8")
    (archive / "log_roots.tsv").write_text(root_rows, encoding="utf-8")
    return archive


def _local_machine_row(home: Path) -> tuple[str, str]:
    short_name = socket.gethostname().split(".", maxsplit=1)[0]
    return short_name, f"{short_name}\t{socket.getfqdn()}\t{home}\n"


def _run(
    archive: Path, *arguments: str, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    environment = dict(os.environ if env is None else env)
    # These subprocess tests exercise fetch behavior, not workstation agent discovery. A stale
    # forwarded socket in tmux otherwise adds the real 10-second liveness ceiling to every run.
    # Dedicated tests below cover dead, live, and tmux-discovered agent sockets explicitly.
    environment.pop("SSH_AUTH_SOCK", None)
    test_bin = archive.parent / "test-bin-no-forwarded-agent"
    test_bin.mkdir(exist_ok=True)
    fake_tmux = test_bin / "tmux"
    if not fake_tmux.exists():
        fake_tmux.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        fake_tmux.chmod(0o755)
    environment["PATH"] = f"{test_bin}{os.pathsep}{environment['PATH']}"
    return subprocess.run(
        [str(archive / "fetch_agent_logs.sh"), *arguments],
        cwd=archive,
        capture_output=True,
        text=True,
        check=False,
        env=environment,
    )


def _fake_ssh_environment(
    tmp_path: Path, *, manifest_stderr: str = "", manifest_exit: int = 0
) -> dict[str, str]:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fake_ssh = fake_bin / "ssh"
    fake_ssh.write_text(
        """#!/usr/bin/env python3
import os
import sys

arguments = sys.argv[1:]
index = 0
while index < len(arguments):
    if arguments[index] == "-n":
        index += 1
    elif arguments[index] == "-o":
        index += 2
    elif arguments[index] == "--":
        index += 1
        break
    else:
        break
index += 1
remaining = arguments[index:]
if len(remaining) == 1:
    remote_command = remaining[0]
    if "find . -printf" in remote_command:
        manifest_stderr = os.environ.get("FAKE_MANIFEST_STDERR", "")
        if manifest_stderr:
            print(manifest_stderr, file=sys.stderr)
        manifest_exit = int(os.environ.get("FAKE_MANIFEST_EXIT", "0"))
        if manifest_exit:
            raise SystemExit(manifest_exit)
    os.execl("/bin/sh", "sh", "-c", remote_command)
os.execvp(remaining[0], remaining)
""",
        encoding="utf-8",
    )
    fake_ssh.chmod(0o755)
    environment = dict(os.environ)
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"
    environment["FAKE_MANIFEST_STDERR"] = manifest_stderr
    environment["FAKE_MANIFEST_EXIT"] = str(manifest_exit)
    return environment


def _latest_receipt(archive: Path) -> Path:
    latest = archive / "_fetch_runs" / "latest"
    assert latest.is_symlink()
    return latest.resolve(strict=True)


def _read_tsv(path: Path) -> list[dict[str, str]]:
    with path.open("r", encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle, delimiter="\t")
        rows: list[dict[str, str]] = []
        for row in reader:
            rows.append(
                {
                    key: value if value is not None else ""
                    for key, value in row.items()
                    if key is not None
                }
            )
        return rows


def _assert_tsv_rectangular(path: Path) -> None:
    with path.open("r", encoding="utf-8", newline="") as handle:
        rows = list(csv.reader(handle, delimiter="\t"))
    assert rows
    assert all(len(row) == len(rows[0]) for row in rows)


def _warning_codes(path: Path) -> set[str]:
    codes: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        raw: object = json.loads(line)
        assert isinstance(raw, dict)
        code = raw.get("code")
        assert isinstance(code, str)
        codes.add(code)
    return codes


def _write_legacy_receipt(
    archive: Path, run_id: str, started: str, finished: str
) -> Path:
    run_dir = archive / "_fetch_runs" / run_id
    run_dir.mkdir(parents=True)
    (run_dir / "run.tsv").write_text(
        (
            f"run_id\t{run_id}\n"
            f"started_utc\t{started}\n"
            "mode\tfetch\n"
            "expected_operations\t1\n"
            f"finished_utc\t{finished}\n"
            "status\tcomplete\n"
            "exit_code\t0\n"
            "attempted_operations\t1\n"
        ),
        encoding="utf-8",
    )
    (run_dir / "results.tsv").write_text(
        (
            "machine\tarchive_name\trequirement\tstatus\texit_code\t"
            "started_utc\tfinished_utc\n"
            f"host\t.codex\trequired\tfetched\t0\t{started}\t{finished}\n"
        ),
        encoding="utf-8",
    )
    (run_dir / "run.tsv").chmod(0o600)
    (run_dir / "results.tsv").chmod(0o600)
    run_dir.chmod(0o700)
    return run_dir


def test_local_fetch_records_metrics_manifest_history_and_sqlite_warnings(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "transcript.jsonl").write_text('{"message":"new"}\n', encoding="utf-8")
    (source / "state.sqlite").write_bytes(b"new database image")
    (source / "state.sqlite-wal").write_bytes(b"live wal")

    short_name, machine_row = _local_machine_row(home)
    archive = _install_archive(
        tmp_path,
        machine_row,
        "*\tlogs\tlogs\trequired\n",
    )
    destination = archive / short_name / "logs"
    destination.mkdir(parents=True)
    old_transcript = destination / "transcript.jsonl"
    old_transcript.write_bytes(b"an older and substantially larger transcript\n")
    newer_timestamp = (source / "transcript.jsonl").stat().st_mtime + 60
    os.utime(old_transcript, (newer_timestamp, newer_timestamp))
    (destination / "stale.sqlite-wal").write_bytes(b"stale wal")
    (destination / "old.sqlite").write_bytes(b"retained old db")

    completed = _run(archive)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert (destination / "transcript.jsonl").read_text(encoding="utf-8") == (
        '{"message":"new"}\n'
    )
    assert (destination / "stale.sqlite-wal").read_bytes() == b"stale wal"
    assert (destination / "old.sqlite").read_bytes() == b"retained old db"

    receipt = _latest_receipt(archive)
    assert (receipt / "machines.tsv").read_bytes() == (archive / "machines.tsv").read_bytes()
    assert (receipt / "log_roots.tsv").read_bytes() == (
        archive / "log_roots.tsv"
    ).read_bytes()
    _assert_tsv_rectangular(receipt / "results.tsv")
    result_rows = _read_tsv(receipt / "results.tsv")
    assert len(result_rows) == 1
    assert result_rows[0]["status"] == "fetched"
    assert int(result_rows[0]["rsync_transferred_file_size"]) > 0
    assert int(result_rows[0]["rsync_literal_data"]) > 0
    assert int(result_rows[0]["rsync_bytes_received"]) > 0
    assert int(result_rows[0]["manifest_entries"]) >= 4
    assert result_rows[0]["stale_sqlite_sidecars"] == "1"
    assert result_rows[0]["retained_sqlite_databases"] == "1"
    assert int(result_rows[0]["rewrite_warnings"]) >= 2

    manifest_text = (receipt / "manifests" / short_name / "logs.jsonl").read_text(
        encoding="utf-8"
    )
    assert '"path": "transcript.jsonl"' in manifest_text
    assert '"path": "state.sqlite"' in manifest_text
    assert '"path": "state.sqlite-wal"' in manifest_text
    assert '"path": "stale.sqlite-wal"' not in manifest_text
    assert '"path": "old.sqlite"' not in manifest_text
    codes = _warning_codes(receipt / "warnings.jsonl")
    assert {
        "source_smaller_than_previous_destination",
        "source_older_than_previous_destination",
        "source_sqlite_sidecar_present",
        "stale_destination_sqlite_sidecar",
        "retained_destination_sqlite_database",
    }.issubset(codes)
    assert "--delete" not in (receipt / "fetch.log").read_text(encoding="utf-8")

    history = archive / "_fetch_runs" / "history.tsv"
    _assert_tsv_rectangular(history)
    history_before = history.read_bytes()
    history_rows = _read_tsv(history)
    assert len(history_rows) == 1
    assert history_rows[0]["status"] == "complete_with_warnings"
    assert history_rows[0]["expected_operations"] == "1"
    assert history_rows[0]["recorded_operations"] == "1"
    assert int(history_rows[0]["rsync_transferred_file_size"]) > 0
    assert int(history_rows[0]["rsync_literal_data"]) > 0
    assert history_rows[0]["rsync_metrics_complete_operations"] == "1"
    assert history_rows[0]["rsync_metrics_unknown_operations"] == "0"
    assert history_rows[0]["metrics_provenance"] == "rsync_stats_exact"
    assert stat.S_IMODE(receipt.stat().st_mode) == 0o500
    assert stat.S_IMODE((receipt / "run.tsv").stat().st_mode) == 0o400

    checked = _run(archive, "--check-sources")
    assert checked.returncode == 0, checked.stdout + checked.stderr
    history_after = history.read_bytes()
    assert history_after.startswith(history_before)
    final_history = _read_tsv(history)
    assert len(final_history) == 2
    assert final_history[1]["metrics_provenance"] == "not_applicable"


def test_check_sources_distinguishes_missing_kinds_and_counts_every_operation(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    (home / "present").mkdir(parents=True)
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(
        tmp_path,
        machine_row,
        (
            "*\tpresent\tpresent\trequired\n"
            "*\trequired-missing\trequired-missing\trequired\n"
            "*\toptional-missing\toptional-missing\toptional\n"
        ),
    )

    completed = _run(archive, "--check-sources")
    assert completed.returncode == 1, completed.stdout + completed.stderr
    rows = _read_tsv(_latest_receipt(archive) / "results.tsv")
    assert {row["status"] for row in rows} == {
        "present",
        "missing_required",
        "missing_optional",
    }
    history = _read_tsv(archive / "_fetch_runs" / "history.tsv")
    assert history[0]["expected_operations"] == "3"
    assert history[0]["recorded_operations"] == "3"
    assert history[0]["successful_operations"] == "2"
    assert history[0]["failed_operations"] == "1"


@pytest.mark.parametrize(
    ("machines", "roots", "message"),
    [
        (
            "one\texample.invalid\t/tmp/one\none\texample2.invalid\t/tmp/two\n",
            "*\tlogs\tlogs\trequired\n",
            "duplicate machine short name",
        ),
        (
            "one\texample.invalid\t/tmp/one\n",
            "*\tlogs\tlogs\trequired\none\tlogs\tother\trequired\n",
            "multiple root rows resolve to destination",
        ),
        (
            "one\texample.invalid\t/tmp/one\n",
            "missing\tlogs\tlogs\trequired\n",
            "unknown machine scope",
        ),
    ],
)
def test_list_rejects_ambiguous_configuration(
    tmp_path: Path, machines: str, roots: str, message: str
) -> None:
    archive = _install_archive(tmp_path, machines, roots)
    completed = _run(archive, "--list")
    assert completed.returncode == 2
    assert message in completed.stderr


def test_configuration_files_may_not_be_symlinks(tmp_path: Path) -> None:
    archive = _install_archive(
        tmp_path,
        "unused\tunused.invalid\t/tmp/unused\n",
        "*\tlogs\tlogs\toptional\n",
    )
    machines = archive / "machines.tsv"
    outside = tmp_path / "outside-machines.tsv"
    outside.write_bytes(machines.read_bytes())
    machines.unlink()
    machines.symlink_to(outside)

    completed = _run(archive, "--list")
    assert completed.returncode == 2
    assert "cannot safely open" in completed.stderr


def test_destination_symlink_escape_is_rejected_before_a_run(tmp_path: Path) -> None:
    home = tmp_path / "home"
    (home / "logs").mkdir(parents=True)
    short_name, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")
    outside = tmp_path / "outside"
    outside.mkdir()
    (archive / short_name).symlink_to(outside, target_is_directory=True)

    completed = _run(archive, "--check-sources")
    assert completed.returncode == 2
    assert "contains a symlink" in completed.stderr
    assert list(outside.iterdir()) == []
    assert not (archive / "_fetch_runs").exists()


def test_source_root_symlink_may_not_escape_configured_home(tmp_path: Path) -> None:
    home = tmp_path / "home"
    home.mkdir()
    outside = tmp_path / "outside-source"
    outside.mkdir()
    (outside / "private.jsonl").write_text("{}\n", encoding="utf-8")
    (home / "logs").symlink_to(outside, target_is_directory=True)
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")

    completed = _run(archive, "--check-sources")
    assert completed.returncode == 1, completed.stdout + completed.stderr
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == "source_outside_home"
    assert not (archive / socket.gethostname().split(".", maxsplit=1)[0]).exists()


def test_local_source_may_not_overlap_archive(tmp_path: Path) -> None:
    short_name, machine_row = _local_machine_row(tmp_path)
    archive = _install_archive(
        tmp_path,
        machine_row,
        "*\tarchive\tarchive\trequired\n",
    )

    completed = _run(archive, "--check-sources")
    assert completed.returncode == 1, completed.stdout + completed.stderr
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == "source_overlaps_archive"
    assert not (archive / short_name / "archive").exists()


def test_local_short_name_with_remote_endpoint_is_rejected(tmp_path: Path) -> None:
    short_name = socket.gethostname().split(".", maxsplit=1)[0]
    archive = _install_archive(
        tmp_path,
        f"{short_name}\tremote.invalid\t/tmp/remote\n",
        "*\tlogs\tlogs\trequired\n",
    )

    completed = _run(archive, "--list")
    assert completed.returncode == 2
    assert "claims the local short name" in completed.stderr


def test_non_private_archive_is_rejected(tmp_path: Path) -> None:
    home = tmp_path / "home"
    (home / "logs").mkdir(parents=True)
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")
    archive.chmod(0o755)

    completed = _run(archive, "--check-sources")
    assert completed.returncode == 2
    assert "must be private" in completed.stderr
    assert not (archive / "_fetch_runs").exists()


def test_capacity_reserve_blocks_transfer_but_records_complete_result(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "event.jsonl").write_text("{}\n", encoding="utf-8")
    short_name, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")

    completed = _run(archive, "--min-free-bytes", str(10**30))
    assert completed.returncode == 1, completed.stdout + completed.stderr
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == "capacity_blocked"
    history = _read_tsv(archive / "_fetch_runs" / "history.tsv")[0]
    assert history["recorded_operations"] == "1"
    assert history["rsync_attempted_operations"] == "0"
    assert not (archive / short_name / "logs").exists()


def test_capacity_estimate_counts_each_hard_link_path(tmp_path: Path) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    payload = source / "first.bin"
    payload.write_bytes(b"x" * (256 * 1024))
    os.link(payload, source / "second.bin")
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")

    completed = _run(archive, "--min-free-bytes", str(10**30))
    assert completed.returncode == 1, completed.stdout + completed.stderr
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert int(row["source_estimated_bytes"]) >= 2 * payload.stat().st_size


def test_dry_run_records_planned_metrics_without_copying_source(tmp_path: Path) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "planned.jsonl").write_text('{"planned":true}\n', encoding="utf-8")
    short_name, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")

    completed = _run(archive, "--dry-run")
    assert completed.returncode == 0, completed.stdout + completed.stderr
    destination = archive / short_name / "logs"
    assert destination.is_dir()
    assert not (destination / "planned.jsonl").exists()
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == "dry_run_complete"
    assert int(row["rsync_transferred_file_size"]) > 0
    assert int(row["manifest_entries"]) >= 2


@pytest.mark.parametrize(
    ("fake_mode", "expected_status"),
    [
        ("fail", "rsync_failed"),
        ("disappear", "source_disappeared_during_rsync"),
        ("success-without-copy", "postcheck_failed"),
    ],
)
def test_rsync_failure_disappearance_and_postcheck_are_distinct(
    tmp_path: Path, fake_mode: str, expected_status: str
) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "event.jsonl").write_text("{}\n", encoding="utf-8")
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")
    fake_bin = tmp_path / "fake-bin"
    fake_bin.mkdir()
    fake_rsync = fake_bin / "rsync"
    fake_rsync.write_text(
        """#!/usr/bin/env python3
import os
import shutil
import sys

mode = os.environ["FAKE_RSYNC_MODE"]
source = os.environ["FAKE_RSYNC_SOURCE"]
print("Number of files: 2")
print("Number of regular files transferred: 1")
print("Total file size: 3 bytes")
print("Total transferred file size: 3 bytes")
if mode == "disappear":
    shutil.rmtree(source)
if mode in {"fail", "disappear"}:
    raise SystemExit(23)
raise SystemExit(0)
""",
        encoding="utf-8",
    )
    fake_rsync.chmod(0o755)
    environment = dict(os.environ)
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"
    environment["FAKE_RSYNC_MODE"] = fake_mode
    environment["FAKE_RSYNC_SOURCE"] = str(source)

    completed = _run(archive, env=environment)
    assert completed.returncode == 1, completed.stdout + completed.stderr
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == expected_status
    history = _read_tsv(archive / "_fetch_runs" / "history.tsv")[0]
    assert history["expected_operations"] == "1"
    assert history["recorded_operations"] == "1"
    assert history["rsync_attempted_operations"] == "1"


def test_remote_fetch_ignores_known_ssh_banner_during_manifest(
    tmp_path: Path,
) -> None:
    home = tmp_path / "remote-home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "remote.jsonl").write_text('{"remote":true}\n', encoding="utf-8")
    archive = _install_archive(
        tmp_path,
        f"remote-test\tremote.invalid\t{home}\n",
        "*\tlogs\tlogs\trequired\n",
    )
    environment = _fake_ssh_environment(
        tmp_path, manifest_stderr=KNOWN_SSH_BANNER
    )

    completed = _run(archive, env=environment)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert (archive / "remote-test" / "logs" / "remote.jsonl").read_text(
        encoding="utf-8"
    ) == '{"remote":true}\n'
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == "fetched"
    assert int(row["manifest_entries"]) >= 2
    assert "source_manifest_unstable" not in _warning_codes(
        _latest_receipt(archive) / "warnings.jsonl"
    )


def test_remote_manifest_keeps_non_banner_stderr_actionable(tmp_path: Path) -> None:
    home = tmp_path / "remote-home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "remote.jsonl").write_text('{"remote":true}\n', encoding="utf-8")
    archive = _install_archive(
        tmp_path,
        f"remote-test\tremote.invalid\t{home}\n",
        "*\tlogs\tlogs\trequired\n",
    )
    environment = _fake_ssh_environment(
        tmp_path,
        manifest_stderr=f"{KNOWN_SSH_BANNER}\nfind: traversal warning",
    )

    completed = _run(archive, env=environment)
    assert completed.returncode == 1, completed.stdout + completed.stderr
    receipt = _latest_receipt(archive)
    row = _read_tsv(receipt / "results.tsv")[0]
    assert row["status"] == "manifest_unstable"
    assert _warning_codes(receipt / "warnings.jsonl") >= {
        "source_manifest_unstable"
    }
    warnings = (receipt / "warnings.jsonl").read_text(encoding="utf-8")
    assert "remote find reported: find: traversal warning" in warnings
    assert KNOWN_SSH_BANNER not in warnings


def test_remote_manifest_nonzero_exit_still_fails_after_banner_filtering(
    tmp_path: Path,
) -> None:
    home = tmp_path / "remote-home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "remote.jsonl").write_text('{"remote":true}\n', encoding="utf-8")
    archive = _install_archive(
        tmp_path,
        f"remote-test\tremote.invalid\t{home}\n",
        "*\tlogs\tlogs\trequired\n",
    )
    environment = _fake_ssh_environment(
        tmp_path,
        manifest_stderr=f"{KNOWN_SSH_BANNER}\nfind: permission denied",
        manifest_exit=1,
    )

    completed = _run(archive, env=environment)
    assert completed.returncode == 1, completed.stdout + completed.stderr
    row = _read_tsv(_latest_receipt(archive) / "results.tsv")[0]
    assert row["status"] == "manifest_failed"
    assert "find: permission denied" in row["detail"]
    assert KNOWN_SSH_BANNER not in row["detail"]


def test_partial_state_survives_failure_and_is_reused_on_next_run(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "resume.jsonl").write_text('{"resume":true}\n', encoding="utf-8")
    short_name, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")
    fake_bin = tmp_path / "fake-bin"
    fake_bin.mkdir()
    fake_rsync = fake_bin / "rsync"
    fake_rsync.write_text(
        """#!/usr/bin/env python3
import os
import pathlib
import sys

partial_arg = next(arg for arg in sys.argv[1:] if arg.startswith("--partial-dir="))
partial = pathlib.Path(partial_arg.split("=", 1)[1])
marker = partial / "retained-partial"
if os.environ["FAKE_RSYNC_PHASE"] == "fail":
    partial.mkdir(parents=True, exist_ok=True)
    marker.write_text("partial payload", encoding="utf-8")
    raise SystemExit(23)
if not marker.is_file():
    raise SystemExit(90)
real_rsync = os.environ["REAL_RSYNC"]
os.execv(real_rsync, [real_rsync, *sys.argv[1:]])
""",
        encoding="utf-8",
    )
    fake_rsync.chmod(0o755)
    real_rsync = shutil.which("rsync")
    assert real_rsync is not None
    environment = dict(os.environ)
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"
    environment["REAL_RSYNC"] = real_rsync
    environment["FAKE_RSYNC_PHASE"] = "fail"

    failed = _run(archive, env=environment)
    assert failed.returncode == 1, failed.stdout + failed.stderr
    assert _read_tsv(_latest_receipt(archive) / "results.tsv")[0]["status"] == (
        "rsync_failed"
    )
    partial = archive / "_fetch_state" / "partials" / short_name / "logs"
    assert (partial / "retained-partial").read_text(encoding="utf-8") == (
        "partial payload"
    )

    environment["FAKE_RSYNC_PHASE"] = "resume"
    resumed = _run(archive, env=environment)
    assert resumed.returncode == 0, resumed.stdout + resumed.stderr
    assert (archive / short_name / "logs" / "resume.jsonl").is_file()
    assert _read_tsv(_latest_receipt(archive) / "results.tsv")[0]["status"] == "fetched"
    assert len(_read_tsv(archive / "_fetch_runs" / "history.tsv")) == 2


def test_sigterm_stops_rsync_and_seals_an_interrupted_receipt(tmp_path: Path) -> None:
    home = tmp_path / "home"
    source = home / "logs"
    source.mkdir(parents=True)
    (source / "event.jsonl").write_text("{}\n", encoding="utf-8")
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")
    fake_bin = tmp_path / "fake-bin"
    fake_bin.mkdir()
    ready = tmp_path / "rsync-ready"
    fake_rsync = fake_bin / "rsync"
    fake_rsync.write_text(
        """#!/usr/bin/env python3
import os
import pathlib
import time

pathlib.Path(os.environ["RSYNC_READY"]).write_text(str(os.getpid()), encoding="utf-8")
time.sleep(60)
""",
        encoding="utf-8",
    )
    fake_rsync.chmod(0o755)
    environment = dict(os.environ)
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"
    environment["RSYNC_READY"] = str(ready)
    process = subprocess.Popen(
        [str(archive / "fetch_agent_logs.sh")],
        cwd=archive,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    # 60s, not 10s. This waits for a locally spawned process to write one marker file, which
    # takes milliseconds when the machine is idle -- so the bound is not measuring anything, it is
    # only distinguishing "started" from "never will". It was 10s, and that was observed to fail
    # inside a full suite run while an unrelated archive rebuild saturated the machine: process
    # spawn plus interpreter startup does not fit in ten seconds on a box under real load.
    #
    # Raising it costs nothing when the marker appears (the loop exits on the file, not the
    # clock) and removes a flake from a gate somebody else is trying to land through.
    deadline = time.monotonic() + 60
    while not ready.exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    assert ready.exists(), (
        "the fake rsync never started: it writes its marker immediately, so 60s without one "
        "means the process was never spawned rather than that it was slow"
    )
    child_pid = int(ready.read_text(encoding="utf-8"))
    process.terminate()
    stdout, stderr = process.communicate(timeout=10)
    assert process.returncode == 1, stdout + stderr
    child_stopped = False
    for _ in range(100):
        try:
            os.kill(child_pid, 0)
        except ProcessLookupError:
            child_stopped = True
            break
        time.sleep(0.02)
    assert child_stopped
    receipt = _latest_receipt(archive)
    row = _read_tsv(receipt / "results.tsv")[0]
    assert row["status"] == "interrupted"
    assert stat.S_IMODE(receipt.stat().st_mode) == 0o500
    history = _read_tsv(archive / "_fetch_runs" / "history.tsv")[0]
    assert history["rsync_attempted_operations"] == "1"
    assert history["rsync_metrics_unknown_operations"] == "1"
    assert history["metrics_provenance"] == "partial_rsync_stats"


def test_existing_lock_prevents_receipt_creation(tmp_path: Path) -> None:
    home = tmp_path / "home"
    (home / "logs").mkdir(parents=True)
    _, machine_row = _local_machine_row(home)
    archive = _install_archive(tmp_path, machine_row, "*\tlogs\tlogs\trequired\n")
    state = archive / "_fetch_state"
    state.mkdir(mode=0o700)
    lock = (state / "fetch.lock").open("a+", encoding="utf-8")
    try:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        completed = _run(archive, "--check-sources")
    finally:
        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
        lock.close()
    assert completed.returncode == 2
    assert "another fetch is already running" in completed.stderr
    runs = archive / "_fetch_runs"
    assert runs.is_dir()
    assert not any(path.name != "history.tsv" for path in runs.iterdir())


def test_history_command_idempotently_migrates_and_backfills_legacy_receipts(
    tmp_path: Path,
) -> None:
    archive = _install_archive(
        tmp_path,
        "unused\tunused.invalid\t/tmp/unused\n",
        "*\tlogs\tlogs\toptional\n",
    )
    first_run = "20260807T142602Z-2907142"
    second_run = "20260807T144544Z-3681718"
    first_start = "2026-08-07T14:26:02Z"
    first_finish = "2026-08-07T14:27:51Z"
    second_start = "2026-08-07T14:45:44Z"
    second_finish = "2026-08-07T14:47:06Z"
    _write_legacy_receipt(archive, first_run, first_start, first_finish)
    _write_legacy_receipt(archive, second_run, second_start, second_finish)
    history = archive / "_fetch_runs" / "history.tsv"
    history.write_text(
        (
            "run_id\tstarted_utc\tfinished_utc\tduration_seconds\tmode\tstatus\t"
            "exit_code\texpected_operations\tattempted_operations\t"
            "transferred_file_bytes_approx\n"
            f"{first_run}\t{first_start}\t{first_finish}\t109\tfetch\tcomplete\t"
            "0\t1\t1\t43844000000\n"
        ),
        encoding="utf-8",
    )

    viewed = _run(archive, "--history")
    assert viewed.returncode == 0
    assert "legacy history schema" in viewed.stderr
    assert second_run not in viewed.stdout

    migrated = _run(archive, "--backfill-history")
    assert migrated.returncode == 0, migrated.stdout + migrated.stderr
    assert "added 1 missing run(s); legacy migration=yes" in migrated.stderr
    rows = _read_tsv(history)
    assert [row["run_id"] for row in rows] == [first_run, second_run]
    first = rows[0]
    second = rows[1]
    assert first["legacy_transferred_file_bytes_approx"] == "43844000000"
    assert first["rsync_transferred_file_size"] == ""
    assert first["metrics_provenance"] == "legacy_approximate"
    assert second["rsync_transferred_file_size"] == ""
    assert second["metrics_provenance"] == "unknown"
    assert second["recorded_operations"] == "1"
    backups = list((archive / "_fetch_runs").glob("history.legacy-*.tsv"))
    assert len(backups) == 1
    assert stat.S_IMODE(backups[0].stat().st_mode) == 0o400
    assert stat.S_IMODE((archive / "_fetch_runs" / first_run).stat().st_mode) == 0o500
    assert stat.S_IMODE(
        (archive / "_fetch_runs" / second_run / "run.tsv").stat().st_mode
    ) == 0o400

    repeated = _run(archive, "--backfill-history")
    assert repeated.returncode == 0, repeated.stdout + repeated.stderr
    assert "added 0 missing run(s); legacy migration=no" in repeated.stderr
    assert len(_read_tsv(history)) == 2
    assert len(list((archive / "_fetch_runs").glob("history.legacy-*.tsv"))) == 1


@pytest.mark.parametrize("symlink_kind", ["runs_directory", "history_file"])
def test_history_rejects_symlinked_state_paths(
    tmp_path: Path, symlink_kind: str
) -> None:
    archive = _install_archive(
        tmp_path,
        "unused\tunused.invalid\t/tmp/unused\n",
        "*\tlogs\tlogs\toptional\n",
    )
    outside = tmp_path / "outside"
    if symlink_kind == "runs_directory":
        outside.mkdir()
        (archive / "_fetch_runs").symlink_to(outside, target_is_directory=True)
    else:
        runs = archive / "_fetch_runs"
        runs.mkdir(mode=0o700)
        outside.write_text("\t".join(("run_id", "unsafe")) + "\n", encoding="utf-8")
        (runs / "history.tsv").symlink_to(outside)

    viewed = _run(archive, "--history")
    assert viewed.returncode == 2
    assert "symlink" in viewed.stderr or "safe owned regular file" in viewed.stderr


# --- a fetch must FAIL, never hang -------------------------------------------------------------
#
# The defect these pin was observed, not imagined: a fetch ran for eight minutes, transferred
# nothing, printed one line ("Using forwarded SSH agent from the tmux environment"), and would
# have waited indefinitely. Three separate things had to be true for that, and each gets a test.


def _fetcher_module() -> object:
    """Import the fetcher as a module, so its internals can be exercised directly."""

    import importlib.util
    import sys

    name = "_fetcher_under_test"
    cached = sys.modules.get(name)
    if cached is not None:
        return cached
    path = FETCHER_DIR / "fetch_agent_logs.py"
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    # Registered BEFORE exec: the fetcher defines dataclasses, and `@dataclass` resolves the
    # defining module out of `sys.modules` while the class body is being built. Executing an
    # unregistered module makes that lookup return None and fail inside `dataclasses`, which
    # reports as an unrelated AttributeError.
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def test_a_socket_nothing_is_listening_on_is_not_an_ssh_agent(tmp_path: Path) -> None:
    """``S_ISSOCK`` is true for an abandoned agent socket, and that is the whole bug.

    A forwarded agent's socket outlives its session: the inode stays, the listener does not. The
    old check accepted it on existence alone, exported ``SSH_AUTH_SOCK``, announced that it had
    found an agent, and left every later ssh blocked in AUTHENTICATION -- past ``ConnectTimeout``,
    which a successful TCP connection had already satisfied.
    """

    module = _fetcher_module()
    dead = tmp_path / "dead.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(dead))
    listener.close()  # the socket FILE remains; nothing is behind it
    assert stat.S_ISSOCK(dead.stat().st_mode), "fixture must be a real socket file"

    assert module._agent_socket_answers(str(dead)) is False  # type: ignore[attr-defined]


def test_a_path_that_is_not_a_socket_is_rejected_without_probing(tmp_path: Path) -> None:
    module = _fetcher_module()
    ordinary = tmp_path / "not-a-socket"
    ordinary.write_text("", encoding="utf-8")
    assert module._agent_socket_answers(str(ordinary)) is False  # type: ignore[attr-defined]
    assert module._agent_socket_answers(str(tmp_path / "absent")) is False  # type: ignore[attr-defined]


def test_an_agent_holding_no_identities_still_counts_as_an_agent(tmp_path: Path) -> None:
    """Exit 1 from ``ssh-add -l`` means "no keys", not "no agent".

    Rejecting it would silently skip a perfectly usable agent over a condition ssh itself reports
    clearly, and would send the fetch down the no-agent path for the wrong reason.
    """

    module = _fetcher_module()
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    ssh_add = fake_bin / "ssh-add"
    ssh_add.write_text(
        "#!/bin/sh\necho 'The agent has no identities.'\nexit 1\n", encoding="utf-8"
    )
    ssh_add.chmod(0o755)
    live = tmp_path / "live.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(live))
    try:
        previous = os.environ["PATH"]
        os.environ["PATH"] = f"{fake_bin}{os.pathsep}{previous}"
        try:
            assert module._agent_socket_answers(str(live)) is True  # type: ignore[attr-defined]
        finally:
            os.environ["PATH"] = previous
    finally:
        listener.close()


def test_a_live_agent_from_tmux_is_discovered_and_adopted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Generic fetch tests suppress ambient tmux, so this pins discovery itself."""

    module = _fetcher_module()
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    live = tmp_path / "live.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(live))
    ssh_add = fake_bin / "ssh-add"
    ssh_add.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
    ssh_add.chmod(0o755)
    tmux = fake_bin / "tmux"
    tmux.write_text(
        f"#!/bin/sh\nprintf '%s\\n' 'SSH_AUTH_SOCK={live}'\n", encoding="utf-8"
    )
    tmux.chmod(0o755)

    class _Logger:
        def __init__(self) -> None:
            self.lines: list[str] = []

        def line(self, text: str) -> None:
            self.lines.append(text)

    monkeypatch.delenv("SSH_AUTH_SOCK", raising=False)
    monkeypatch.setenv("PATH", f"{fake_bin}{os.pathsep}{os.environ['PATH']}")
    logger = _Logger()
    try:
        module.discover_forwarded_ssh_agent(logger)  # type: ignore[attr-defined]
        assert os.environ["SSH_AUTH_SOCK"] == str(live)
        assert logger.lines == ["Using forwarded SSH agent from the tmux environment."]
    finally:
        listener.close()


def test_a_child_that_never_returns_becomes_a_failure_rather_than_a_hang() -> None:
    """``_run_bounded`` converts a stall into the failure path every caller already handles.

    Exit 124 matches :manpage:`timeout(1)` rather than being a number this tool invented, and the
    detail says "stall, not refusal" because that distinction is what points at the real cause.
    """

    module = _fetcher_module()
    started = time.monotonic()
    completed = module._run_bounded(["sleep", "30"], timeout_s=2.0)  # type: ignore[attr-defined]
    elapsed = time.monotonic() - started

    assert completed.returncode == 124
    assert elapsed < 20.0, f"the ceiling did not fire: took {elapsed:.1f}s"
    assert "timed out after 2s" in completed.stderr
    assert "stall rather than a refusal" in completed.stderr

    # The bytes-returning twin must behave identically; it exists only because the return type
    # differs, and a ceiling that fired on one but not the other would be worse than neither.
    raw = module._run_bounded_bytes(["sleep", "30"], timeout_s=2.0)  # type: ignore[attr-defined]
    assert raw.returncode == module.TIMED_OUT  # type: ignore[attr-defined]
    assert b"timed out after 2s" in raw.stderr


def test_every_remote_host_is_smoke_tested_once_before_any_source_is_probed(
    tmp_path: Path,
) -> None:
    """One question per HOST, not per source, and a failing host skips its sources.

    Twenty-four operations across a handful of hosts is the real configuration. Learning that a
    host cannot authenticate by probing each of its sources in turn costs one probe ceiling per
    source to establish a fact one ssh could have settled -- which is the difference between
    failing in seconds and failing in an hour and a half.

    A failed host is recorded and SKIPPED rather than aborting the run: a fetch spanning several
    machines should still collect from the ones that are up.
    """

    module = _fetcher_module()

    class _Machine:
        def __init__(self, host: str) -> None:
            self.host = host

    class _Operation:
        def __init__(self, host: str) -> None:
            self.machine = _Machine(host)
            self.local = False

    asked: list[list[str]] = []

    def _fake_run(
        command: Sequence[str], *, timeout_s: float, text: bool = True
    ) -> subprocess.CompletedProcess[str]:
        recorded = list(command)
        asked.append(recorded)
        ok = recorded[-2] != "down.example"
        return subprocess.CompletedProcess(
            recorded,
            0 if ok else 255,
            "",
            "" if ok else "Permission denied (publickey).",
        )

    class _Logger:
        def __init__(self) -> None:
            self.lines: list[str] = []

        def line(self, text: str) -> None:
            self.lines.append(text)

    module._run_bounded = _fake_run  # type: ignore[attr-defined]
    logger = _Logger()
    operations = [
        _Operation("up.example"),
        _Operation("up.example"),
        _Operation("down.example"),
        _Operation("down.example"),
        _Operation("down.example"),
    ]

    failures = module.preflight_hosts(operations, logger)  # type: ignore[attr-defined]

    assert set(failures) == {"down.example"}
    assert "Permission denied" in failures["down.example"]
    # Two hosts, five operations: asked once per host.
    assert len(asked) == 2, f"expected one probe per host, got {len(asked)}"
    assert any("did not answer" in line for line in logger.lines)


def test_a_source_on_an_unreachable_host_is_failed_without_being_probed() -> None:
    """The synthesised probe carries the preflight's reason, so the receipt says WHY."""

    module = _fetcher_module()
    probe = module._unreachable_probe("Permission denied (publickey).")  # type: ignore[attr-defined]

    assert probe.kind is module.ProbeKind.FAILED  # type: ignore[attr-defined]
    assert probe.exit_code == module.REMOTE_UNREACHABLE  # type: ignore[attr-defined]
    assert probe.duration_seconds == 0.0
    assert "did not answer preflight" in probe.detail
    assert "Permission denied" in probe.detail
