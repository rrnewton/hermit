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
    default_qemu_assets,
    display_path,
    extract_info_tail,
    hash_file,
    hermit_tmp_args,
    load_anchor,
    make_run_dir,
    print_comparison,
    print_header,
    report_safehermit,
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
HERMIT_REPO = Path(os.environ.get("HERMIT_REPO", ROOT))
HERMIT = Path(os.environ.get("HERMIT_RELEASE", HERMIT_REPO / "target/release/hermit"))
# Read at import, before anything can put HERMIT_RELEASE back into the environment
# (demo 5 does exactly that), which would otherwise make this always true.
HERMIT_PINNED = "HERMIT_RELEASE" in os.environ
ASSETS = Path(os.environ.get("QEMU_ASSETS", default_qemu_assets(ROOT)))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "120"))
# Run hermit through the bounded entry point rather than bare. The orphan this
# demo's own start_new_session comment describes -- still writing hermit-info.log at
# ~45 GiB/h with ppid=1 after the demo exited -- was reaped by the group kill but was
# never byte-bounded while it lived.
SAFEHERMIT = ROOT / "bin/safehermit"
# MEASURED ON THIS DEMO, NOT GUESSED -- and measured MORE THAN ONCE, which is the
# only reason this number is right. On devbig014 (hermit 0.2.0 g770b95c505fa, QEMU
# 10.1.2) a healthy resume wrote:
#     80,133,297 bytes in 10s   -- resuming one snapshot state
#    153,236,739 bytes in 18-26s -- resuming another, twice, byte-identical
# The same demo, healthy both times, differing by 1.9x depending on the snapshot it
# resumes from. A cap derived from the first measurement alone would have been
# 1.75x a healthy run rather than the 3.3x it appeared to be -- still passing, but
# with a third of the headroom advertised, which is how a cap ends up firing on a
# good run months later and getting blamed on something else. The cap is 3.50x the
# LARGER observed healthy total.
#
# The 64 MiB default is 0.42x what a healthy resume already writes, so wiring this
# demo unchanged would kill every run partway through.
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
    """Build the default phase-5 prerequisite; never guess for custom paths."""
    if BOOT_SNAPSHOT_DISK.is_file():
        return
    default_snapshot = ASSETS / "hermit-boot.qcow2"
    if BOOT_SNAPSHOT_DISK != default_snapshot:
        raise RuntimeError(
            "missing custom boot snapshot: {}; produce it before Demo 6".format(
                BOOT_SNAPSHOT_DISK
            )
        )
    print("Phase-5 snapshot missing; running demo 5 prerequisite...", flush=True)
    run_checked(
        ["make", "--no-print-directory", "-C", str(DEMO_DIR), "demo5"],
        cwd=ROOT,
    )
    if not BOOT_SNAPSHOT_DISK.is_file():
        raise RuntimeError("Demo 5 did not produce {}".format(BOOT_SNAPSHOT_DISK))


def main() -> int:
    arguments = parse_args()
    guest_command = " ".join(arguments.command).strip() or "uname -a"
    if "\n" in guest_command or "\r" in guest_command:
        raise ValueError("guest command must be a single line")

    os.environ["HERMIT_RELEASE"] = str(HERMIT)
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
    # A pinned binary is NOT rebuilt, matching demo 5 (05-qemu-boot.py) and for the
    # reason recorded there: `make release-core` verifies the pinned submodules
    # runs `git submodule update --init --recursive`, which DETACHES attached
    # primaries and would build a different Hermit than the operator asked for. Demo
    # 6 was calling it unconditionally, so a pinned run aborted on any checkout the
    # submodule could not switch -- including a submodule another agent has left
    # dirty, which is not a fact about this demo at all.
    if HERMIT_PINNED and HERMIT.is_file() and os.access(str(HERMIT), os.X_OK):
        print("Pinned Hermit (skipping release-core): {}".format(HERMIT))
    else:
        run_checked(["make", "--no-print-directory", "-s", "release-core"], cwd=ROOT)
    if not QEMU:
        raise RuntimeError("qemu-system-x86_64 is required")
    ensure_boot_snapshot()

    command_digest = hashlib.sha256(guest_command.encode()).hexdigest()
    command_root = ASSETS / "resume-metadata" / command_digest
    lock = acquire_demo_lock(ASSETS / ".qemu-demo.lock")
    run_dir = make_run_dir(command_root, "resume")
    # Resume serial is a bidirectional `-serial pipe:` FIFO pair (base path plus
    # .in/.out), not a unix socket: a socket chardev's poll fd starves the vCPU
    # under `hermit --no-rcb-time` (same class as the demo-5 boot bug).
    serial_pipe = ASSETS / "serial-pipe"
    serial_pipe_in = Path(str(serial_pipe) + ".in")
    serial_pipe_out = Path(str(serial_pipe) + ".out")
    qmp_socket = ASSETS / "qmp.sock"
    serial_log = ASSETS / "serial.log"
    archived_serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    safehermit_report = run_dir / "safehermit-report.txt"
    output_path = run_dir / "guest-output.txt"
    archived_disk = run_dir / "post-command.qcow2"
    copy_file(BOOT_SNAPSHOT_DISK, SNAPSHOT_DISK)
    process = None
    saved_snapshot = not arguments.no_save_snapshot

    try:
        for runtime_path in (
            qmp_socket,
            serial_pipe_in,
            serial_pipe_out,
            serial_log,
        ):
            runtime_path.unlink(missing_ok=True)
        # The workload is a DISK input, not something typed into the console. Write
        # the fixed-size image before QEMU launches so its contents are already
        # present the instant the guest resumes; nothing then depends on when the
        # host gets around to sending bytes.
        # A STABLE path, not a per-run one. The image path appears in the recorded
        # QEMU argv that the repeat check compares, and the anchor stores that argv
        # raw, so a per-run path made every repeat report "QEMU argv differs" for a
        # difference that is only the directory name. The serial pipe this replaced
        # lived at a stable path under ASSETS for the same reason; the demo lock
        # serialises runs, so sharing the path is no worse than it was.
        command_image = ASSETS / "guest-command.img"
        write_command_image(command_image, guest_command)
        qemu_argv = build_qemu_command(
            QEMU,
            qmp_socket,
            serial_log,
            SNAPSHOT_DISK,
            ASSETS / "bzImage",
            ASSETS / "initramfs.cpio.gz",
            SNAPSHOT_NAME,
            command_image,
        )
        post_name = "command-{}".format(command_digest[:16])
        command = [
            str(SAFEHERMIT),
            # Keep safehermit's report out of info_log: it is hashed into
            # run-metadata.json and line-diffed against the anchor run, and six
            # report lines embed a per-run id (UTC timestamp plus pid). Leaving them
            # in fails repeat verification on every run.
            "--sh-report",
            str(safehermit_report),
            "--sh-max-log-bytes",
            str(MAX_LOG_BYTES),
            "--sh-deadline",
            str(TIMEOUT + 60),
            str(HERMIT),
            "run",
            *hermit_tmp_args(ROOT),
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
            "--command-image",
            str(command_image),
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
            "--post-snapshot-name",
            post_name,
        ]
        if not saved_snapshot:
            command.append("--no-save-snapshot")
        environment = os.environ.copy()
        environment["RUST_LOG"] = LOG_FILTER
        # Keep safehermit's own capped copy inside this run's directory so it is
        # pruned with the run rather than accumulating in a shared location. A
        # healthy run therefore stores the log twice, about 76 MiB each.
        environment["SAFEHERMIT_LOG_ROOT"] = str(run_dir / "safehermit")
        # The guest controller imports demo_common. Suppress CPython's bytecode
        # write so the controller cannot mutate checkout bytes during a run.
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        banner("Resume {} and run: {}".format(SNAPSHOT_NAME, guest_command))
        print("Restoring snapshot (timeout: {}s)...".format(TIMEOUT), flush=True)
        with info_log.open("wb") as log:
            process = subprocess.Popen(
                command,
                stdout=log,
                stderr=subprocess.STDOUT,
                env=environment,
                cwd=str(ROOT),
                # Own process group, so stop_process can take the whole tree down.
                # hermit forks a second process that survives a signal to the pid
                # alone; measured, it kept writing hermit-info.log at ~45 GB/h with
                # ppid=1 after the demo exited. drgn_hermit.py has done this since
                # demo 7 was written.
                start_new_session=True,
            )
            return_code = wait_for_process(
                process, TIMEOUT, progress_label="Hermit/QEMU resume"
            )
        # CHECK THE STATUS BEFORE TOUCHING THE ARTIFACTS. This read used to come
        # first, and when the run was killed early the serial log did not exist yet,
        # so a FileNotFoundError pre-empted the informative error below. Measured on a
        # review run with the cap set to 1 MiB: this demo reported
        #   FAILURE: [Errno 2] No such file or directory: .../serial.log
        # while demo 5, which already checked the status first, reported
        #   FAILURE: Hermit/QEMU exited with status 125
        # The missing file is a SYMPTOM of the kill, and reporting it instead of the
        # kill sends the reader to debug the serial log.
        report_safehermit(safehermit_report, return_code)
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
                "PASS: saved first run for this command at {}".format(
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
        return 1 if result == "PARTIAL" else 0
    finally:
        stop_process(process)
        for runtime_path in (qmp_socket, serial_pipe_in, serial_pipe_out):
            runtime_path.unlink(missing_ok=True)
        release_demo_lock(lock)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
