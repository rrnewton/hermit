#!/usr/bin/env python3
"""Resume the QEMU boot snapshot, run a command, and auto-verify repeats."""

import argparse
import hashlib
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
from qemu_controller import (  # noqa: E402
    BEGIN_MARKER,
    END_MARKER,
    build_qemu_command,
)


DEMO_LABEL = "Demo 6: QEMU Snapshot Resume"
HERMIT_REPO = Path(os.environ.get("HERMIT_REPO", ROOT / "hermit"))
HERMIT = Path(os.environ.get("HERMIT_RELEASE", HERMIT_REPO / "target/release/hermit"))
ASSETS = Path(os.environ.get("QEMU_ASSETS", ROOT / "ignored/qemu-linux"))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "120"))
SNAPSHOT_NAME = os.environ.get("QEMU_SNAPSHOT_NAME", "hermit-boot")
SNAPSHOT_DISK = Path(
    os.environ.get("QEMU_SNAPSHOT_DISK", ASSETS / "hermit-snapshot.qcow2")
)
BOOT_SNAPSHOT_DISK = Path(
    os.environ.get("QEMU_BOOT_SNAPSHOT_DISK", ASSETS / "hermit-boot.qcow2")
)
LOG_FILTER = os.environ.get(
    "QEMU_LOG_FILTER",
    "warn,detcore=info,reverie_ptrace::task=info",
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--no-save-snapshot",
        action="store_true",
        help="run the command without saving a post-command snapshot",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER, help="guest shell command")
    return parser.parse_args()


def command_output(transcript: bytes, begin: str, end: str) -> bytes:
    output = []
    active = False
    for line in transcript.decode(errors="replace").splitlines():
        stripped = line.strip()
        if stripped == begin:
            active = True
            continue
        if active and end in line:
            break
        if active and begin not in line:
            output.append(line)
    return ("\n".join(output) + "\n").encode()


def main() -> int:
    arguments = parse_args()
    guest_command = " ".join(arguments.command).strip() or "uname -a"
    if "\n" in guest_command or "\r" in guest_command:
        raise ValueError("guest command must be a single line")

    os.environ["HERMIT_RELEASE"] = str(HERMIT)
    os.environ["QEMU_BIN"] = QEMU
    dependency = check_dependencies(ROOT)
    print_header(
        DEMO_LABEL,
        (
            "QEMU restores the live shell, runs one command, saves a post-command",
            "snapshot by default, and compares repeats keyed by the command string.",
        ),
        dependency,
    )
    run_checked(["make", "--no-print-directory", "-s", "build-hermit"], cwd=ROOT)
    if not QEMU:
        raise RuntimeError("qemu-system-x86_64 is required")
    if not BOOT_SNAPSHOT_DISK.is_file():
        raise RuntimeError("missing boot snapshot; run ./demos/05-qemu-boot.py first")

    command_digest = hashlib.sha256(guest_command.encode()).hexdigest()
    command_root = ASSETS / "resume-metadata" / command_digest
    lock = acquire_demo_lock(ASSETS / ".qemu-demo.lock")
    run_dir = make_run_dir(command_root, "resume")
    serial_socket = ASSETS / "serial.sock"
    qmp_socket = ASSETS / "qmp.sock"
    serial_log = ASSETS / "serial.log"
    archived_serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    output_path = run_dir / "guest-output.txt"
    archived_disk = run_dir / "post-command.qcow2"
    copy_file(BOOT_SNAPSHOT_DISK, SNAPSHOT_DISK)
    process = None
    saved_snapshot = not arguments.no_save_snapshot

    try:
        for runtime_path in (qmp_socket, serial_socket, serial_log):
            runtime_path.unlink(missing_ok=True)
        qemu_argv = build_qemu_command(
            QEMU,
            qmp_socket,
            serial_socket,
            SNAPSHOT_DISK,
            ASSETS / "bzImage",
            ASSETS / "initramfs.cpio.gz",
            SNAPSHOT_NAME,
        )
        post_name = "command-{}".format(command_digest[:16])
        command = [
            str(HERMIT),
            "run",
            "--strict",
            "--no-rcb-time",
            "--target-timeslice",
            "100000",
            "--max-timeslice",
            "disabled",
            "--",
            sys.executable,
            str(DEMO_DIR / "lib/qemu_controller.py"),
            "resume",
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
            "--guest-command",
            guest_command,
            "--post-snapshot-name",
            post_name,
        ]
        if not saved_snapshot:
            command.append("--no-save-snapshot")
        environment = os.environ.copy()
        environment["RUST_LOG"] = LOG_FILTER
        banner("Resume {} and run: {}".format(SNAPSHOT_NAME, guest_command))
        print("Restoring snapshot (timeout: {}s)...".format(TIMEOUT), flush=True)
        with info_log.open("wb") as log:
            process = subprocess.Popen(
                command,
                stdout=log,
                stderr=subprocess.STDOUT,
                env=environment,
                cwd=str(ROOT),
            )
            return_code = wait_for_process(
                process, TIMEOUT, progress_label="Hermit/QEMU resume"
            )
        transcript = serial_log.read_bytes()
        if return_code != 0:
            raise RuntimeError("Hermit/QEMU exited with status {}".format(return_code))

        copy_file(serial_log, archived_serial_log)
        guest_output = command_output(transcript, BEGIN_MARKER, END_MARKER)
        output_path.write_bytes(guest_output)
        banner("Guest serial output")
        sys.stdout.buffer.write(guest_output)
        sys.stdout.buffer.flush()

        banner("Hermit INFO tail (wall-clock timestamps stripped)")
        for line in extract_info_tail(info_log):
            print(line)

        extra = {
            "kind": "qemu-resume",
            "command": guest_command,
            "command_sha256": command_digest,
            "guest_output": str(output_path.resolve()),
            "guest_output_sha256": hashlib.sha256(guest_output).hexdigest(),
            "qemu_argv": qemu_argv,
            "serial_log": str(archived_serial_log.resolve()),
            "snapshot_saved": saved_snapshot,
        }
        if saved_snapshot:
            canonicalize_qcow2_snapshot_timestamp(SNAPSHOT_DISK, post_name)
            copy_file(SNAPSHOT_DISK, archived_disk)
            extra["snapshot_date_nsec_canonicalized"] = True
        current = save_metadata(
            run_dir, archived_disk if saved_snapshot else None, info_log, extra
        )
        anchor = load_anchor(command_root)
        banner("Automatic repeat verification")
        result = "FIRST RUN SAVED"
        if anchor is None:
            anchor_path = save_anchor(command_root, current)
            print(
                "PASS: saved first run for this command at {}".format(
                    anchor_path.relative_to(ROOT)
                )
            )
        else:
            passed, report = compare_runs(anchor, current)
            print_comparison(
                passed,
                report,
                current.get("qcow2_sha256"),
                "Resume",
            )
            result = "SUCCESS" if passed else "PARTIAL"

        if saved_snapshot:
            print("Post-command snapshot: {}".format(archived_disk.relative_to(ROOT)))
            print("Post-command SHA-256: {}".format(hash_file(archived_disk)))
        print(
            "Run metadata: {}".format((run_dir / "run-metadata.json").relative_to(ROOT))
        )
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return 1 if result == "PARTIAL" else 0
    finally:
        stop_process(process)
        for socket_path in (qmp_socket, serial_socket):
            socket_path.unlink(missing_ok=True)
        release_demo_lock(lock)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
