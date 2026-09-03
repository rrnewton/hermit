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
    canonicalize_qemu_runtime_path,
    check_dependencies,
    check_qemu_dependencies,
    compare_runs,
    copy_file,
    default_qemu_assets,
    display_path,
    extract_info_tail,
    hash_file,
    hermit_tmp_args,
    load_committed_anchor,
    make_socket_path,
    make_temp_result_dir,
    print_comparison,
    print_header,
    publish_anchor,
    publish_file_atomic,
    report_safehermit,
    run_checked,
    save_metadata,
    stop_process,
    wait_for_process,
)
from qemu_controller import build_qemu_command  # noqa: E402


DEMO_LABEL = "Demo 5: QEMU Linux Snapshot"
HERMIT_REPO = Path(os.environ.get("HERMIT_REPO", ROOT))
HERMIT = Path(os.environ.get("HERMIT_RELEASE", HERMIT_REPO / "target/release/hermit"))
# Whether the caller pinned the binary. Must be read BEFORE main() writes
# HERMIT_RELEASE back into the environment, which would make this always true.
HERMIT_PINNED = "HERMIT_RELEASE" in os.environ
ASSETS = Path(os.environ.get("QEMU_ASSETS", default_qemu_assets(ROOT)))
QEMU = os.environ.get("QEMU_BIN", shutil.which("qemu-system-x86_64") or "")
TIMEOUT = int(os.environ.get("QEMU_TIMEOUT", "600"))
# Run hermit through the bounded entry point rather than bare. Three runs at
# ~45 GiB/h once wrote 38 GiB of orphaned hermit-info.log in a single session and
# tripped the disk headroom alarm twice; start_new_session below reaps the tree the
# demo knows about, but nothing bounded the bytes.
SAFEHERMIT = ROOT / "bin/safehermit"
# MEASURED ON THIS DEMO, NOT GUESSED. Four healthy full boots on the Demo 5 QEMU
# measurement host recorded in docs/TESTING_ENVIRONMENTS.md under "Named measurement
# hosts" (hermit 0.2.0 g770b95c505fa, QEMU 10.1.2) wrote 253,386,127 / 253,585,587 / 253,643,026 /
# 253,643,032 bytes of hermit-info.log in 50-54s of wall -- about 5.4 MiB/s, and a
# spread of 0.1%, so unlike demo 6 this figure is stable across runs. The cap is
# 3.17x the largest of them.
#
# THE DEFAULT WOULD HAVE BEEN LETHAL AND IT IS WORTH SAYING BY HOW MUCH:
# safehermit's built-in cap is 64 MiB, which this demo passes roughly a quarter of
# the way through the boot, so wiring it unchanged would kill every run and the
# likely reaction would be to unwire the bound rather than raise it. A cap below
# what a healthy run needs is worse than no cap, because it converts a working demo
# into an unexplained kill.
MAX_LOG_BYTES = int(os.environ.get("QEMU_MAX_LOG_BYTES", str(768 * 1024 * 1024)))
SNAPSHOT_NAME = os.environ.get("QEMU_SNAPSHOT_NAME", "hermit-boot")
# By default each concurrent run keeps its snapshot disk inside its own private
# working directory (computed in main); QEMU_SNAPSHOT_DISK forces a fixed path.
SNAPSHOT_DISK_OVERRIDE = os.environ.get("QEMU_SNAPSHOT_DISK")
SNAPSHOT_SIZE = os.environ.get("QEMU_SNAPSHOT_SIZE", "64M")
LOG_FILTER = os.environ.get(
    "QEMU_LOG_FILTER",
    "warn,detcore=info,reverie_ptrace::task=info",
)


def _hermit_version(binary: Path) -> str:
    """Return the binary's self-reported version, or a marker if it will not run."""
    try:
        result = subprocess.run(
            [str(SAFEHERMIT), str(binary), "--version"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return "version unavailable"
    return result.stdout.strip() or "version unavailable"


def snapshot_exists(path: Path, name: str) -> bool:
    result = subprocess.run(
        ["qemu-img", "snapshot", "-l", str(path)],
        stdout=subprocess.PIPE,
        check=True,
        text=True,
    )
    return any(line.split()[1:2] == [name] for line in result.stdout.splitlines())


COMMAND_IMAGE_BYTES = 4096
PLACEHOLDER_COMMAND = "WAIT"


def write_placeholder_command_image(path: Path) -> None:
    """The image the guest polls before resume supplies a real command.

    Same fixed size as the resume image (demo 6 writes that one): the drive is
    attached at boot and at resume and only the backing file differs, so a geometry
    change between the two would not match the device state in the snapshot.
    """
    payload = PLACEHOLDER_COMMAND.encode() + b"\n"
    path.write_bytes(payload + b"\0" * (COMMAND_IMAGE_BYTES - len(payload)))


def main() -> int:
    os.environ["HERMIT_RELEASE"] = str(HERMIT)
    os.environ["QEMU_BIN"] = QEMU
    os.environ["QEMU_DEMO_PYTHON"] = sys.executable
    qemu_dependency = check_qemu_dependencies(ROOT)
    dependency = check_dependencies(ROOT)
    print_header(
        DEMO_LABEL,
        (
            "Hermit boots QEMU/Linux, streams the serial console, saves a live snapshot,",
            "and compares every repeat run with the first run.",
        ),
        dependency + "\n" + qemu_dependency,
    )
    # A pinned binary is NOT rebuilt. `make release-core` checks submodules
    # -> `checkout-all`, which runs `git submodule update --init --recursive`; the
    # Makefile warns that this DETACHES attached primaries. When the parent gitlink
    # and the primary's HEAD differ, that MOVES the primary checkout and then builds
    # a different Hermit than the operator asked for -- so the run would be captured
    # against a binary nobody chose. That is exactly the "golden captured wrong"
    # failure this demo's anchor is supposed to be immune to, so when HERMIT_RELEASE
    # names an existing executable we skip the build and report the binary's
    # baked-in version instead (it carries a `-dirty` marker, so it attests the
    # source state as well as the SHA).
    if HERMIT_PINNED and HERMIT.is_file() and os.access(str(HERMIT), os.X_OK):
        print(
            "Pinned Hermit (skipping release-core): {} [{}]".format(
                HERMIT, _hermit_version(HERMIT)
            )
        )
    else:
        run_checked(["make", "--no-print-directory", "-s", "release-core"], cwd=ROOT)
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
    # Keep the socket with the run when that fits AF_UNIX; make_socket_path moves
    # only an overlong path to its short, host-visible fallback.
    qmp_socket = make_socket_path(run_dir / "qmp.sock", "boot")
    # Boot backs the serial console with a `-serial file:` transcript, not a unix
    # socket: a socket chardev adds a host-timing-driven pollable fd that can
    # starve the -icount vCPU under the deterministic scheduler (the 600s boot
    # timeout). QEMU writes this file directly and the controller tails it for
    # boot markers.
    serial_log = run_dir / "serial.log"
    info_log = run_dir / "hermit-info.log"
    safehermit_report = run_dir / "safehermit-report.txt"
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

        # Boot attaches the command drive with a PLACEHOLDER, so the device exists in
        # the snapshot. Resume swaps only its backing file; a device absent here could
        # not appear there, because vmstate records the device model.
        command_image = run_dir / "guest-command.img"
        write_placeholder_command_image(command_image)
        qemu_argv = build_qemu_command(
            QEMU,
            qmp_socket,
            serial_log,
            snapshot_disk,
            ASSETS / "bzImage",
            ASSETS / "initramfs.cpio.gz",
            None,
            command_image,
        )
        command = [
            str(SAFEHERMIT),
            # Keep safehermit's own report out of info_log. info_log is hashed into
            # run-metadata.json (info_log_sha256) and compare_runs line-diffs two
            # runs of it, and six report lines embed a run id built from a UTC
            # timestamp and safehermit's pid. Verified against the real comparator:
            # hermit_log_diff on two logs identical apart from those lines reports
            # "first divergence at line 1 ... run_id". Without this the repeat
            # verification below fails on every run, for a reason that is not hermit.
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
            # Keep RCB/PMU branch-count preemption ARMED with a large-but-finite
            # --max-timeslice so the deterministic scheduler makes fine-grained
            # virtual-time progress. Setting --max-timeslice disabled together
            # with --no-rcb-time (parent commit 0591104) removed all timer
            # preemption, so unproductive SleepUntil(0) poll-yields kept the run
            # queue non-empty, the step-2d vtime jump never fired, and the vCPU
            # was starved -> boot wedge at HPET calibration. See
            # debug/demo5-regression (H11) and tag demo5-lastgood.
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
            "--serial-log",
            str(serial_log),
            "--disk",
            str(snapshot_disk),
            "--kernel",
            str(ASSETS / "bzImage"),
            "--initrd",
            str(ASSETS / "initramfs.cpio.gz"),
            "--command-image",
            str(command_image),
            "--snapshot-name",
            SNAPSHOT_NAME,
            "--timeout",
            str(TIMEOUT),
        ]
        environment = os.environ.copy()
        environment["RUST_LOG"] = LOG_FILTER
        # safehermit keeps its own capped copy of hermit's stderr. Point it inside
        # this run's directory so it is archived and pruned with the run, instead of
        # accumulating under a shared ignored/safehermit that nothing prunes. This
        # does mean a healthy run now stores the log TWICE -- about 242 MiB each --
        # which is the price of the cap being enforced on a stream the demo would
        # otherwise write unbounded.
        environment["SAFEHERMIT_LOG_ROOT"] = str(run_dir / "safehermit")
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
                # Own process group, so stop_process can take the whole tree down.
                # hermit forks a second process that survives a signal to the pid
                # alone; measured, it kept writing hermit-info.log at ~45 GB/h with
                # ppid=1 after the demo exited. drgn_hermit.py has done this since
                # demo 7 was written.
                start_new_session=True,
            )
            return_code = wait_for_process(
                process,
                TIMEOUT,
                stream_path=serial_log,
                first_output_label="Waiting for first serial line",
            )
        # Surface what the wrapper concluded. --sh-report keeps its lines out of the
        # hashed and line-diffed hermit log, which means nothing shows them unless the
        # caller does; and "status 125" on its own does not say a byte cap fired.
        report_safehermit(safehermit_report, return_code)
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
        snapshot_display = display_path(snapshot_disk, ROOT)
        print(
            "Snapshot disk: {} (internal tag: {})".format(snapshot_display, SNAPSHOT_NAME)
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
        canonical_argv = [
            canonicalize_qemu_runtime_path(arg, run_dir, qmp_socket)
            for arg in qemu_argv
        ]
        info_log.write_text(
            canonicalize_qemu_runtime_path(
                info_log.read_text(errors="replace"), run_dir, qmp_socket
            )
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
        # Remove the run's QMP socket before publishing so it never pollutes the
        # anchor (or the archived run) directory. (Serial is a file, not a
        # socket, and lives on as the transcript.)
        qmp_socket.unlink(missing_ok=True)
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
                    display_path(anchor_dir, ROOT)
                )
            )
        else:
            anchor = load_committed_anchor(anchor_dir)
            if anchor is None:
                raise RuntimeError(
                    "published boot anchor has no readable run-metadata.json"
                )
            # Compare while the run dir is still in place (its info_log path is
            # valid), then archive it into run-history.
            passed, report = compare_runs(anchor, current)
            final_dir = archive_result_dir(run_dir, ASSETS, "boot")
            print("Anchor already claimed by a concurrent/earlier run; comparing.")
            print_comparison(passed, report, current.qcow2_sha256, "Boot")
            result = "SUCCESS" if passed else "PARTIAL"
        print(
            "Run metadata: {}".format(
                display_path(final_dir / "run-metadata.json", ROOT)
            )
        )
        print(
            "Archived snapshot: {}".format(
                display_path(final_dir / "boot-snapshot.qcow2", ROOT)
            )
        )
        print("\n=== {}: {} ===".format(DEMO_LABEL, result))
        return 1 if result == "PARTIAL" else 0
    finally:
        stop_process(process)
        qmp_socket.unlink(missing_ok=True)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print("WARN: {}: FAILURE: {}".format(DEMO_LABEL, error), file=sys.stderr)
        sys.exit(1)
