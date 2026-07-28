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
    archive_result_dir,
    banner,
    canonicalize_qcow2_snapshot_timestamp,
    check_dependencies,
    compare_runs,
    copy_file,
    extract_info_tail,
    hash_file,
    load_committed_anchor,
    make_temp_result_dir,
    print_comparison,
    print_header,
    publish_anchor,
    publish_file_atomic,
    run_checked,
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
# By default each concurrent run keeps its snapshot disk inside its own private
# working directory (computed in main); QEMU_SNAPSHOT_DISK forces a fixed path.
SNAPSHOT_DISK_OVERRIDE = os.environ.get("QEMU_SNAPSHOT_DISK")
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
    anchor_dir = ASSETS / "boot-anchor"
    # Everything for this run lives in a private working directory so any number
    # of runs can boot QEMU concurrently without sharing sockets, disks, or logs.
    run_dir = make_temp_result_dir(ASSETS, "boot")
    qmp_socket = run_dir / "qmp.sock"
    serial_socket = run_dir / "serial.sock"
    serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    snapshot_disk = (
        Path(SNAPSHOT_DISK_OVERRIDE)
        if SNAPSHOT_DISK_OVERRIDE
        else run_dir / "hermit-snapshot.qcow2"
    )
    process = None

    try:
        run_checked(
            [
                "qemu-img",
                "create",
                "-q",
                "-f",
                "qcow2",
                str(snapshot_disk),
                SNAPSHOT_SIZE,
            ]
        )

        qemu_argv = build_qemu_command(
            QEMU,
            qmp_socket,
            serial_socket,
            snapshot_disk,
            ASSETS / "bzImage",
            ASSETS / "initramfs.cpio.gz",
        )
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
            str(snapshot_disk),
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
        # The guest controller imports demo_common; letting CPython write its
        # bytecode cache into the shared demos/lib/__pycache__ makes concurrent
        # runs race on the same .pyc (one wins openat(O_CREAT|O_EXCL), the other
        # sees EEXIST), diverging the deterministic trace. Suppress the write so
        # every concurrent guest executes the identical syscall sequence.
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
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
        if not snapshot_exists(snapshot_disk, SNAPSHOT_NAME):
            raise RuntimeError("snapshot {} was not saved".format(SNAPSHOT_NAME))
        canonicalize_qcow2_snapshot_timestamp(snapshot_disk, SNAPSHOT_NAME)

        snapshot_sha = hash_file(snapshot_disk)
        (snapshot_disk.with_suffix(snapshot_disk.suffix + ".id")).write_text(
            snapshot_sha + "\n"
        )
        # Shared handoff artifact consumed by demo 6; publish atomically so a
        # concurrent reader never sees a half-written qcow2.
        baseline_disk = ASSETS / "hermit-boot.qcow2"
        publish_file_atomic(snapshot_disk, baseline_disk)
        archived_disk = run_dir / "boot-snapshot.qcow2"
        copy_file(snapshot_disk, archived_disk)
        # serial_log already lives inside run_dir, so it is published with the
        # anchor/archive directly; no separate copy step is needed.
        serial_text = serial_log.read_text(errors="replace")
        if "2022-01-01T" not in serial_text:
            raise RuntimeError("serial transcript lacks the fixed RTC epoch")

        banner("Snapshot ready")
        display_path = os.path.relpath(str(snapshot_disk), str(ROOT))
        print(
            "Snapshot disk: {} (internal tag: {})".format(display_path, SNAPSHOT_NAME)
        )
        run_checked(["qemu-img", "snapshot", "-l", str(snapshot_disk)])
        print("Snapshot SHA-256: {}".format(snapshot_sha))

        banner("Hermit INFO tail (wall-clock timestamps stripped)")
        for line in extract_info_tail(info_log):
            print(line)

        # The qemu argv and the Hermit INFO log both embed this run's private
        # working directory (sockets and snapshot disk), which differs between
        # concurrent runs by design. Fold that path to a stable token so the
        # anchor comparison reflects genuine differences, not the per-run temp
        # directory name.
        canonical_argv = [arg.replace(str(run_dir), "<run-dir>") for arg in qemu_argv]
        info_log.write_text(
            info_log.read_text(errors="replace").replace(str(run_dir), "<run-dir>")
        )
        current = save_metadata(
            run_dir,
            archived_disk,
            info_log,
            {
                "kind": "qemu-boot",
                "snapshot_name": SNAPSHOT_NAME,
                "snapshot_date_nsec_canonicalized": True,
                "qemu_argv": canonical_argv,
                "serial_log": str(serial_log.resolve()),
                "serial_sha256": hash_file(serial_log),
            },
        )
        # Remove the run's sockets before publishing so they never pollute the
        # anchor (or the archived run) directory.
        for socket_path in (qmp_socket, serial_socket):
            socket_path.unlink(missing_ok=True)
        # Drop the working snapshot copy before publishing: the archived
        # boot-snapshot.qcow2 plus the metadata's qcow2_sha256 retain everything
        # needed, so keeping it too would double the per-run disk footprint. An
        # explicitly overridden QEMU_SNAPSHOT_DISK is left in place.
        if not SNAPSHOT_DISK_OVERRIDE:
            snapshot_disk.unlink(missing_ok=True)
            snapshot_disk.with_suffix(snapshot_disk.suffix + ".id").unlink(
                missing_ok=True
            )

        banner("Automatic repeat verification")
        # Race to claim the anchor with a single atomic, no-clobber rename. The
        # winner becomes the first-run anchor; every loser compares against the
        # fully-committed anchor.
        won_anchor = publish_anchor(run_dir, anchor_dir)
        if won_anchor:
            final_dir = anchor_dir
            result = "FIRST RUN SAVED"
            print(
                "PASS: this run won the anchor claim; saved first run at {}".format(
                    anchor_dir.relative_to(ROOT)
                )
            )
        else:
            anchor = load_committed_anchor(anchor_dir)
            # Compare while the run dir is still in place (its info_log path is
            # valid), then archive it into run-history.
            passed, report = compare_runs(anchor, current)
            final_dir = archive_result_dir(run_dir, ASSETS, "boot")
            print("Anchor already claimed by a concurrent/earlier run; comparing.")
            print_comparison(passed, report, current["qcow2_sha256"], "Boot")
            result = "SUCCESS" if passed else "PARTIAL"
        print(
            "Run metadata: {}".format(
                (final_dir / "run-metadata.json").relative_to(ROOT)
            )
        )
        print(
            "Archived snapshot: {}".format(
                (final_dir / "boot-snapshot.qcow2").relative_to(ROOT)
            )
        )
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return 1 if result == "PARTIAL" else 0
    finally:
        stop_process(process)
        for socket_path in (qmp_socket, serial_socket):
            socket_path.unlink(missing_ok=True)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
