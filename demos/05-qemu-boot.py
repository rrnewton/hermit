#!/usr/bin/env python3
"""Boot Linux under Hermit, save a QEMU snapshot, and auto-verify repeats."""

import os
from pathlib import Path
import shutil
import subprocess
import sys


DEMO_DIR = Path(__file__).resolve().parent
ROOT = DEMO_DIR.parent
sys.path.insert(0, str(DEMO_DIR / "lib"))

from demo_common import (  # noqa: E402
    acquire_demo_lock,
    banner,
    canonicalize_qcow2_snapshot_timestamp,
    check_dependencies,
    compare_runs,
    copy_file,
    extract_info_tail,
    hash_file,
    load_anchor,
    make_run_dir,
    print_comparison,
    print_header,
    release_demo_lock,
    run_checked,
    save_anchor,
    save_metadata,
    stop_process,
    wait_for_process,
)
from qemu_controller import build_qemu_command  # noqa: E402


DEMO_LABEL = "Demo 5: QEMU Linux Snapshot"
HERMIT_REPO = Path(os.environ.get("HERMIT_REPO", ROOT / "hermit"))
HERMIT = Path(os.environ.get("HERMIT_RELEASE", HERMIT_REPO / "target/release/hermit"))
ASSETS = Path(os.environ.get("QEMU_ASSETS", ROOT / "ignored/qemu-linux"))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "600"))
SNAPSHOT_NAME = os.environ.get("QEMU_SNAPSHOT_NAME", "hermit-boot")
SNAPSHOT_DISK = Path(
    os.environ.get("QEMU_SNAPSHOT_DISK", ASSETS / "hermit-snapshot.qcow2")
)
SNAPSHOT_SIZE = os.environ.get("QEMU_SNAPSHOT_SIZE", "64M")
LOG_FILTER = os.environ.get(
    "QEMU_LOG_FILTER",
    "warn,detcore=info,reverie_ptrace::task=info",
)


def snapshot_exists(path: Path, name: str) -> bool:
    result = subprocess.run(
        ["qemu-img", "snapshot", "-l", str(path)],
        stdout=subprocess.PIPE,
        check=True,
        text=True,
    )
    return any(line.split()[1:2] == [name] for line in result.stdout.splitlines())


def main() -> int:
    os.environ["HERMIT_RELEASE"] = str(HERMIT)
    os.environ["QEMU_BIN"] = QEMU
    dependency = check_dependencies(ROOT)
    print_header(
        DEMO_LABEL,
        (
            "Hermit boots QEMU/Linux, streams the serial console, saves a live snapshot,",
            "and compares every repeat run with the first run.",
        ),
        dependency,
    )
    run_checked(["make", "--no-print-directory", "-s", "build-hermit"], cwd=ROOT)
    banner("Verify QEMU kernel and initramfs")
    run_checked([str(DEMO_DIR / "lib/qemu-assets.sh")], cwd=ROOT)
    if not HERMIT.is_file() or not os.access(str(HERMIT), os.X_OK):
        raise RuntimeError("missing release Hermit binary: {}".format(HERMIT))
    if not QEMU:
        raise RuntimeError("qemu-system-x86_64 is required")

    ASSETS.mkdir(parents=True, exist_ok=True)
    lock = acquire_demo_lock(ASSETS / ".qemu-demo.lock")
    run_dir = make_run_dir(ASSETS, "boot")
    qmp_socket = ASSETS / "qmp.sock"
    serial_socket = ASSETS / "serial.sock"
    serial_log = ASSETS / "serial.log"
    archived_serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    temporary_disk = SNAPSHOT_DISK.with_name(
        "{}.tmp.{}".format(SNAPSHOT_DISK.name, os.getpid())
    )
    process = None

    try:
        for runtime_path in (qmp_socket, serial_socket, serial_log):
            runtime_path.unlink(missing_ok=True)
        run_checked(
            [
                "qemu-img",
                "create",
                "-q",
                "-f",
                "qcow2",
                str(temporary_disk),
                SNAPSHOT_SIZE,
            ]
        )
        os.replace(str(temporary_disk), str(SNAPSHOT_DISK))

        qemu_argv = build_qemu_command(
            QEMU,
            qmp_socket,
            serial_socket,
            SNAPSHOT_DISK,
            ASSETS / "bzImage",
            ASSETS / "initramfs.cpio.gz",
        )
        command = [
            str(HERMIT),
            "run",
            "--strict",
            "--target-timeslice",
            "100000",
            "--max-timeslice",
            "2000000000",
            "--",
            sys.executable,
            str(DEMO_DIR / "lib/qemu_controller.py"),
            "boot",
            "--qemu",
            QEMU,
            "--qmp-socket",
            str(qmp_socket),
            "--serial-socket",
            str(serial_socket),
            "--serial-log",
            str(serial_log),
            "--disk",
            str(SNAPSHOT_DISK),
            "--kernel",
            str(ASSETS / "bzImage"),
            "--initrd",
            str(ASSETS / "initramfs.cpio.gz"),
            "--snapshot-name",
            SNAPSHOT_NAME,
            "--timeout",
            str(TIMEOUT),
        ]
        environment = os.environ.copy()
        environment["RUST_LOG"] = LOG_FILTER
        banner("Boot Linux to its serial shell (1st line takes a while to appear)")
        with info_log.open("wb") as log:
            process = subprocess.Popen(
                command,
                stdout=log,
                stderr=subprocess.STDOUT,
                env=environment,
                cwd=str(ROOT),
            )
            return_code = wait_for_process(process, TIMEOUT, stream_path=serial_log)
        if return_code != 0:
            raise RuntimeError("Hermit/QEMU exited with status {}".format(return_code))
        if not snapshot_exists(SNAPSHOT_DISK, SNAPSHOT_NAME):
            raise RuntimeError("snapshot {} was not saved".format(SNAPSHOT_NAME))
        canonicalize_qcow2_snapshot_timestamp(SNAPSHOT_DISK, SNAPSHOT_NAME)

        snapshot_sha = hash_file(SNAPSHOT_DISK)
        (SNAPSHOT_DISK.with_suffix(SNAPSHOT_DISK.suffix + ".id")).write_text(
            snapshot_sha + "\n"
        )
        baseline_disk = ASSETS / "hermit-boot.qcow2"
        copy_file(SNAPSHOT_DISK, baseline_disk)
        archived_disk = run_dir / "boot-snapshot.qcow2"
        copy_file(SNAPSHOT_DISK, archived_disk)
        copy_file(serial_log, archived_serial_log)
        serial_text = serial_log.read_text(errors="replace")
        if "2022-01-01T" not in serial_text:
            raise RuntimeError("serial transcript lacks the fixed RTC epoch")

        banner("Snapshot ready")
        display_path = os.path.relpath(str(SNAPSHOT_DISK), str(ROOT))
        print(
            "Snapshot disk: {} (internal tag: {})".format(display_path, SNAPSHOT_NAME)
        )
        run_checked(["qemu-img", "snapshot", "-l", str(SNAPSHOT_DISK)])
        print("Snapshot SHA-256: {}".format(snapshot_sha))

        banner("Hermit INFO tail (wall-clock timestamps stripped)")
        for line in extract_info_tail(info_log):
            print(line)

        anchor = load_anchor(ASSETS)
        current = save_metadata(
            run_dir,
            archived_disk,
            info_log,
            {
                "kind": "qemu-boot",
                "snapshot_name": SNAPSHOT_NAME,
                "snapshot_date_nsec_canonicalized": True,
                "qemu_argv": qemu_argv,
                "serial_log": str(archived_serial_log.resolve()),
                "serial_sha256": hash_file(archived_serial_log),
            },
        )
        banner("Automatic repeat verification")
        result = "FIRST RUN SAVED"
        if anchor is None:
            anchor_path = save_anchor(ASSETS, current)
            print(
                "PASS: saved first run metadata at {}".format(
                    anchor_path.relative_to(ROOT)
                )
            )
        else:
            passed, report = compare_runs(anchor, current)
            print_comparison(passed, report, current["qcow2_sha256"], "Boot")
            result = "SUCCESS" if passed else "PARTIAL"
        print(
            "Run metadata: {}".format((run_dir / "run-metadata.json").relative_to(ROOT))
        )
        print("Archived snapshot: {}".format(archived_disk.relative_to(ROOT)))
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return 1 if result == "PARTIAL" else 0
    finally:
        stop_process(process)
        temporary_disk.unlink(missing_ok=True)
        for socket_path in (qmp_socket, serial_socket):
            socket_path.unlink(missing_ok=True)
        release_demo_lock(lock)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
