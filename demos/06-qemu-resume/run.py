#!/usr/bin/env python3
"""Resume the QEMU boot snapshot, run a command, and auto-verify repeats."""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading


DEMO_DIR = Path(__file__).resolve().parent
DEMOS_DIR = DEMO_DIR.parent
ROOT = DEMOS_DIR.parent
sys.path.insert(0, str(DEMOS_DIR / "lib"))

from demo_common import (  # noqa: E402
    acquire_demo_lock,
    banner,
    canonicalize_qcow2_snapshot_timestamp,
    check_dependencies,
    compare_runs,
    copy_file,
    default_qemu_assets,
    display_path,
    extract_info_tail,
    hash_file,
    hermit_binary,
    hermit_tmp_args,
    load_anchor,
    make_run_dir,
    print_comparison,
    print_header,
    release_demo_lock,
    run_checked,
    save_anchor,
    save_metadata,
    stage_guest_controller,
    stop_process,
    wait_for_process,
)
from qemu_controller import (  # noqa: E402
    BEGIN_MARKER,
    END_MARKER,
    build_qemu_command,
)
from signal_33 import settle_signal_33_disposition  # noqa: E402


DEMO_LABEL = "Demo 6: QEMU Snapshot Resume"
ASSETS = Path(os.environ.get("QEMU_ASSETS", default_qemu_assets(ROOT)))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "120"))
# Stop the run if Hermit's info log grows past this many bytes. Healthy resumes
# measured on 2026-09-30 on Hermit 0.2.0 gdc92644f96f4 with QEMU 10.1.2 wrote
# 169,751,813 bytes (`uname -a` without saving a snapshot, 10.0-11.4 s) and
# 258,227,125 to 259,021,561 bytes (three commands with a saved snapshot,
# 15.7-16.5 s); the cap is about 2.1 times the larger figure. (Hermit 0.2.0
# g770b95c505fa wrote 80 to 153 MB.)
MAX_LOG_BYTES = int(os.environ.get("QEMU_MAX_LOG_BYTES", str(512 * 1024 * 1024)))
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
# Without --epoch, `hermit run` starts virtual time at the host's current time
# (and logs that epoch so a run can be reproduced), so two resumes started at
# different times would not match. Every resume starts from the same fixed
# epoch, DetCore's own library default, as demo 5's boot does.
EPOCH = "2026-01-01T00:00:00Z"
# The guest sees the controller script and the assets directory (kernel,
# initramfs, snapshot disk, command image, serial log, QMP socket) at fixed
# paths, the same ones demo 5's boot uses. Their host paths depend on where the
# checkout is, and they appear on the guest's command lines, whose lengths change
# how many branches the controller and QEMU execute: passed as host paths, a
# fresh clone in another directory resumed to a different post-command snapshot
# after 288657 scheduler turns instead of 287149. The controller directory holds
# this run's private copy of the controller's sources (see
# stage_guest_controller), not demos/lib itself.
GUEST_CONTROLLER_DIR = Path("/tmp/hermit-demo-controller")
GUEST_ASSETS_DIR = Path("/tmp/hermit-demo-assets")
# The guest's working directory, instead of the checkout directory.
GUEST_WORKDIR = Path("/tmp")


def guest_path(host_path: Path) -> Path:
    """The path at which the guest sees ``host_path``.

    Files in ASSETS appear under GUEST_ASSETS_DIR; any other path (for example a
    QEMU_SNAPSHOT_DISK outside ASSETS) is passed unchanged.
    """
    try:
        return GUEST_ASSETS_DIR / Path(host_path).relative_to(ASSETS)
    except ValueError:
        return Path(host_path)


def guest_environment_args() -> list:
    """Hermit flags that give the controller the same environment on every run.

    By default the guest inherits the caller's whole environment, and the
    controller's branch count depends on it: a resume started through
    `make -C demos demo6` (which adds MAKEFLAGS and MAKELEVEL) would not match
    one started directly. The guest gets Hermit's minimal fixed environment plus
    only the variables the controller uses.
    """
    arguments = ["--base-env=minimal", "--env", "PYTHONDONTWRITEBYTECODE"]
    if "DEMO_QMP_TIMEOUT" in os.environ:
        arguments += ["--env", "DEMO_QMP_TIMEOUT"]
    return arguments


# Seconds to wait, after Hermit exits, for its last output to reach the log.
OUTPUT_DRAIN_TIMEOUT = 60


def start_output_copier(process: subprocess.Popen, log) -> threading.Thread:
    """Copy Hermit's output pipe into ``log`` on a thread; return the thread.

    The guest inherits Hermit's standard streams. When they were the log file
    itself, the controller's start-up fstat(1) reported how many log bytes had
    reached the file so far, which depends on host timing (a wrapper that relays
    Hermit's stderr delivers it late); demo 5's boot hit exactly this. The guest
    now reads /dev/null and writes to a pipe, whose metadata never changes, and
    a thread copies the pipe into the log.
    """

    def copy_output() -> None:
        try:
            while True:
                chunk = os.read(process.stdout.fileno(), 1 << 16)
                if not chunk:
                    break
                log.write(chunk)
        except (OSError, ValueError):
            pass  # the log was closed after a failed run; nothing more to keep
        finally:
            process.stdout.close()

    copier = threading.Thread(target=copy_output, daemon=True)
    copier.start()
    return copier


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--no-save-snapshot",
        action="store_true",
        help="run the command without saving a post-command snapshot",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER, help="guest shell command")
    return parser.parse_args()


COMMAND_IMAGE_BYTES = 4096


def write_command_image(path: Path, command: str) -> None:
    """Fixed-size raw image holding the guest command.

    The size is fixed because the same drive is attached at boot and at resume and
    only its backing file differs; a geometry change between the two would not
    match the device state recorded in the snapshot.
    """
    payload = command.encode() + b"\n"
    if len(payload) > COMMAND_IMAGE_BYTES:
        raise ValueError(
            "guest command is {} bytes, over the {}-byte image".format(
                len(payload), COMMAND_IMAGE_BYTES
            )
        )
    path.write_bytes(payload + b"\0" * (COMMAND_IMAGE_BYTES - len(payload)))


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


def ensure_boot_snapshot() -> None:
    """Build demo 5's default boot snapshot; never guess for custom paths."""
    if BOOT_SNAPSHOT_DISK.is_file():
        return
    default_snapshot = ASSETS / "hermit-boot.qcow2"
    if BOOT_SNAPSHOT_DISK != default_snapshot:
        raise RuntimeError(
            "missing custom boot snapshot: {}; produce it before Demo 6".format(
                BOOT_SNAPSHOT_DISK
            )
        )
    print("Demo 5 boot snapshot missing; running demo 5 first...", flush=True)
    run_checked(
        ["make", "--no-print-directory", "-C", str(DEMOS_DIR), "demo5"],
        cwd=ROOT,
    )
    if not BOOT_SNAPSHOT_DISK.is_file():
        raise RuntimeError("Demo 5 did not produce {}".format(BOOT_SNAPSHOT_DISK))


def resume_once(guest_command: str, save_snapshot: bool) -> str:
    """Resume once, compare with this command's reference run, return the verdict."""

    os.environ["QEMU_BIN"] = QEMU
    os.environ["QEMU_ASSETS"] = str(ASSETS)
    dependency = check_dependencies(ROOT)
    print_header(
        DEMO_LABEL,
        (
            "QEMU restores the live shell, runs one command, saves a post-command",
            "snapshot by default, and compares repeats keyed by the command string.",
        ),
        dependency,
    )
    hermit = hermit_binary()
    if not QEMU:
        raise RuntimeError("qemu-system-x86_64 is required")
    ensure_boot_snapshot()

    command_digest = hashlib.sha256(guest_command.encode()).hexdigest()
    # A --no-save-snapshot run writes no post-command snapshot and runs a
    # different controller command line (so a different Hermit log), so it can
    # never match a reference that saved one. It keeps its own reference.
    reference_key = command_digest if save_snapshot else command_digest + "-no-save-snapshot"
    command_root = ASSETS / "resume-metadata" / reference_key
    lock = acquire_demo_lock(ASSETS / ".qemu-demo.lock")
    run_dir = make_run_dir(command_root, "resume")
    controller_dir = stage_guest_controller(run_dir / "controller")
    # Only the guest uses the QMP socket, through GUEST_ASSETS_DIR, so its path is
    # always short enough for AF_UNIX; the host path is kept for cleanup.
    qmp_socket = ASSETS / "qmp.sock"
    serial_log = ASSETS / "serial.log"
    archived_serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    output_path = run_dir / "guest-output.txt"
    archived_disk = run_dir / "post-command.qcow2"
    copy_file(BOOT_SNAPSHOT_DISK, SNAPSHOT_DISK)
    process = None
    saved_snapshot = save_snapshot

    try:
        for runtime_path in (qmp_socket, serial_log):
            runtime_path.unlink(missing_ok=True)
        # The command reaches the guest on a disk, not through the console. The
        # fixed-size image is written before QEMU starts, so its contents are
        # present the instant the guest resumes and nothing depends on when the
        # host sends bytes. The path is stable rather than per-run because it
        # appears in the QEMU argv that the repeat check compares; the demo lock
        # serializes runs, so sharing it is safe.
        command_image = ASSETS / "guest-command.img"
        write_command_image(command_image, guest_command)
        qemu_argv = build_qemu_command(
            QEMU,
            guest_path(qmp_socket),
            guest_path(serial_log),
            guest_path(SNAPSHOT_DISK),
            guest_path(ASSETS / "bzImage"),
            guest_path(ASSETS / "initramfs.cpio.gz"),
            SNAPSHOT_NAME,
            guest_path(command_image),
        )
        post_name = "command-{}".format(command_digest[:16])
        command = [
            hermit,
            "run",
            *hermit_tmp_args(ROOT),
            "--bind",
            "{}:{}".format(controller_dir, GUEST_CONTROLLER_DIR),
            "--bind",
            "{}:{}".format(ASSETS, GUEST_ASSETS_DIR),
            "--workdir",
            str(GUEST_WORKDIR),
            *guest_environment_args(),
            "--strict",
            "--epoch",
            EPOCH,
            "--no-rcb-time",
            "--target-timeslice",
            "100000",
            "--max-timeslice",
            "disabled",
            "--",
            sys.executable,
            str(GUEST_CONTROLLER_DIR / "qemu_controller.py"),
            "resume",
            "--qemu",
            QEMU,
            "--qmp-socket",
            str(guest_path(qmp_socket)),
            "--command-image",
            str(guest_path(command_image)),
            "--serial-log",
            str(guest_path(serial_log)),
            "--disk",
            str(guest_path(SNAPSHOT_DISK)),
            "--kernel",
            str(guest_path(ASSETS / "bzImage")),
            "--initrd",
            str(guest_path(ASSETS / "initramfs.cpio.gz")),
            "--snapshot-name",
            SNAPSHOT_NAME,
            "--timeout",
            str(TIMEOUT),
            "--post-snapshot-name",
            post_name,
        ]
        if not saved_snapshot:
            command.append("--no-save-snapshot")
        environment = os.environ.copy()
        environment["RUST_LOG"] = LOG_FILTER
        # The guest controller imports demo_common. Suppress CPython's bytecode
        # write: the guest compiles demo_common from source on every run and
        # leaves the staged copy exactly as stage_guest_controller wrote it.
        # (guest_environment_args passes this variable through to the guest.)
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        banner("Resume {} and run: {}".format(SNAPSHOT_NAME, guest_command))
        print("Restoring snapshot (timeout: {}s)...".format(TIMEOUT), flush=True)
        with info_log.open("wb", buffering=0) as log:
            process = subprocess.Popen(
                command,
                # The guest inherits these streams; see start_output_copier.
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                env=environment,
                cwd=str(ROOT),
                # Own process group, so stop_process can stop Hermit and every
                # process it started, not only the first one.
                start_new_session=True,
            )
            copier = start_output_copier(process, log)
            try:
                return_code = wait_for_process(
                    process,
                    TIMEOUT,
                    progress_label="Hermit/QEMU resume",
                    log_path=info_log,
                    max_log_bytes=MAX_LOG_BYTES,
                )
            finally:
                copier.join(OUTPUT_DRAIN_TIMEOUT)
            if copier.is_alive():
                raise RuntimeError(
                    "Hermit's output was still open {}s after it exited".format(
                        OUTPUT_DRAIN_TIMEOUT
                    )
                )
        # Check the exit status before reading any artifact: a run stopped early
        # may not have written the serial log yet.
        if return_code != 0:
            raise RuntimeError("Hermit/QEMU exited with status {}".format(return_code))
        transcript = serial_log.read_bytes()

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
                "Saved this run as the reference run for this command at {}".format(
                    display_path(anchor_path, ROOT)
                )
            )
        else:
            passed, report = compare_runs(anchor, current)
            print_comparison(
                passed,
                report,
                current.qcow2_sha256,
                "Resume",
            )
            result = "SUCCESS" if passed else "PARTIAL"

        if saved_snapshot:
            print("Post-command snapshot: {}".format(display_path(archived_disk, ROOT)))
            print("Post-command SHA-256: {}".format(hash_file(archived_disk)))
        print(
            "Run metadata: {}".format(display_path(run_dir / "run-metadata.json", ROOT))
        )
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return result
    finally:
        stop_process(process)
        qmp_socket.unlink(missing_ok=True)
        release_demo_lock(lock)


def main() -> int:
    # Before the first `hermit`; see settle_signal_33_disposition.
    settle_signal_33_disposition()
    arguments = parse_args()
    guest_command = " ".join(arguments.command).strip() or "uname -a"
    if "\n" in guest_command or "\r" in guest_command:
        raise ValueError("guest command must be a single line")
    save_snapshot = not arguments.no_save_snapshot
    result = resume_once(guest_command, save_snapshot)
    # The first run of a command only records its reference run. Resume a second
    # time so a single invocation always performs a comparison. Set
    # QEMU_RESUME_REPEAT=0 to skip the second resume.
    if result == "FIRST RUN SAVED" and os.environ.get("QEMU_RESUME_REPEAT", "1") != "0":
        banner("Resume again and compare with the reference run just saved")
        result = resume_once(guest_command, save_snapshot)
    return 1 if result == "PARTIAL" else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
