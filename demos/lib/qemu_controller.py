#!/usr/bin/env python3
"""Run QEMU plus its serial/QMP controller inside Hermit's boundary."""

import argparse
from pathlib import Path
import subprocess
import sys
import time
from typing import List, Optional

from demo_common import qmp_command, stop_process, wait_for_socket


# Launch QEMU with fork()+execve, never vfork(). CPython 3.11+ uses vfork() by
# default, which suspends the parent until the child execs. This controller runs
# inside Hermit alongside QEMU, so both share one deterministic scheduler. Under
# `hermit run --no-rcb-time`, QEMU's `-icount sleep=off` main loop starts
# busy-polling as soon as it execs and the scheduler never resumes the suspended
# vfork parent, so the controller would wait forever. Plain fork() leaves the
# parent runnable. The attribute exists on CPython 3.11+; older versions already
# use fork().
if hasattr(subprocess, "_USE_VFORK"):
    subprocess._USE_VFORK = False


BOOT_MARKER = "HERMIT-QEMU-BASELINE-BOOT-OK"
# Emitted by the guest once it is polling the command disk; this is where the
# boot snapshot is taken, so the resumed guest has not yet read the device.
COMMAND_DISK_MARKER = "HERMIT-QEMU-COMMAND-DISK-READY"
BEGIN_MARKER = "__HERMIT_COMMAND_BEGIN__"
END_MARKER = "__HERMIT_COMMAND_END__"


# QEMU's initial process state influences the VM snapshot. Do not leak harness
# settings such as QEMU_TIMEOUT or proxy variables into its initial stack.
QEMU_ENV = {
    "LC_ALL": "C",
    "TZ": "UTC",
}


class FileSerial:
    """Read-only serial transport that tails QEMU's `-serial file:` transcript.

    The controller only reads the console, so QEMU writes the serial stream
    straight to a file and this class tails it. Unlike a unix-socket chardev, a
    file sink puts no pollable descriptor in QEMU's main loop, so it cannot
    starve the -icount vCPU under `hermit --no-rcb-time`. There is no internal
    deadline: time is virtual inside Hermit, so the demo process outside the
    container enforces the real wall-clock timeout.
    """

    def __init__(self, transcript: Path) -> None:
        self.path = Path(transcript)
        self.handle = None
        self.buffer = bytearray()

    def _ensure_open(self) -> None:
        # QEMU creates the file when it opens the chardev; wait for it to appear.
        while self.handle is None:
            try:
                self.handle = self.path.open("rb")
            except FileNotFoundError:
                time.sleep(0.05)

    def wait_for(self, marker: str, count: int = 1) -> None:
        self._ensure_open()
        marker_bytes = marker.encode()
        while self.buffer.count(marker_bytes) < count:
            chunk = self.handle.read()
            if chunk:
                self.buffer.extend(chunk)
            else:
                time.sleep(0.05)

    def close(self) -> None:
        if self.handle is not None:
            self.handle.close()


def build_qemu_command(
    qemu: str,
    qmp_socket: Path,
    serial_endpoint: Path,
    disk: Path,
    kernel: Path,
    initrd: Path,
    load_snapshot: Optional[str] = None,
    command_image: Optional[Path] = None,
) -> List[str]:
    command = [
        qemu,
        "-machine",
        "q35",
        "-cpu",
        "max",
        "-smp",
        "1",
        "-m",
        "512M",
        "-display",
        "none",
        "-monitor",
        "none",
    ]
    # The console is a plain file chardev: QEMU only writes to it, and nothing is
    # ever typed into the guest (the command arrives on a disk). A listening
    # unix-socket chardev would add a descriptor whose readiness depends on host
    # timing to QEMU's main-loop poll set; under `hermit run --no-rcb-time` that
    # lets the main loop monopolize the scheduler and starve the vCPU thread.
    command.extend(["-serial", "file:{}".format(serial_endpoint)])
    command.extend(
        [
            "-qmp",
            "unix:{},server=on,wait=off".format(qmp_socket),
            "-drive",
            "if=none,id=hermit-snapshot-store,file={},format=qcow2".format(disk),
        ]
    )
    if command_image is not None:
        # Present at BOOT as well as resume: vmstate records the device model, so a
        # device absent at snapshot cannot appear at resume. Only the backing FILE
        # differs between the two, and its size is fixed so the geometry matches.
        # readonly=on is required, not cosmetic: a writable raw drive is not
        # snapshottable and `savevm` refuses the save.
        command.extend(
            [
                "-drive",
                "if=none,id=hermit-command,file={},format=raw,readonly=on".format(
                    command_image
                ),
                "-device",
                "virtio-blk-pci,drive=hermit-command",
            ]
        )
    if load_snapshot is not None:
        command.extend(["-loadvm", load_snapshot])
    command.extend(
        [
            "-icount",
            "shift=0,sleep=off",
            "-rtc",
            "base=2022-01-01T00:00:00,clock=vm",
            "-kernel",
            str(kernel),
            "-initrd",
            str(initrd),
            "-append",
            "console=ttyS0 reboot=t",
        ]
    )
    return command


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("boot", "resume"))
    parser.add_argument("--qemu", required=True)
    parser.add_argument("--qmp-socket", type=Path, required=True)
    # Fixed-size raw image holding the guest command; see build_qemu_command.
    parser.add_argument("--command-image", type=Path)
    parser.add_argument("--serial-log", type=Path, required=True)
    parser.add_argument("--disk", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--snapshot-name", default="hermit-boot")
    parser.add_argument("--timeout", type=float, required=True)
    parser.add_argument("--post-snapshot-name")
    parser.add_argument("--no-save-snapshot", action="store_true")
    return parser.parse_args()


def run_controller(arguments: argparse.Namespace) -> int:
    load_snapshot = arguments.snapshot_name if arguments.mode == "resume" else None
    serial_endpoint = arguments.serial_log
    if arguments.mode == "resume":
        if arguments.command_image is None:
            raise ValueError("resume mode requires --command-image")
    command = build_qemu_command(
        arguments.qemu,
        arguments.qmp_socket,
        serial_endpoint,
        arguments.disk,
        arguments.kernel,
        arguments.initrd,
        load_snapshot,
        arguments.command_image,
    )
    process = None
    serial = None
    try:
        process = subprocess.Popen(command, env=QEMU_ENV)
        wait_for_socket(arguments.qmp_socket, process, arguments.timeout)

        if arguments.mode == "boot":
            # QEMU writes the console straight to arguments.serial_log; tail it.
            serial = FileSerial(arguments.serial_log)
            serial.wait_for(BOOT_MARKER)
            # Snapshot the guest while it is polling the command disk, NOT at a
            # shell prompt: the prompt only existed to be typed into.
            serial.wait_for(COMMAND_DISK_MARKER)
            qmp_command(
                arguments.qmp_socket,
                "human-monitor-command",
                "command-line",
                "savevm {}".format(arguments.snapshot_name),
                blocking=True,
            )
            qmp_command(arguments.qmp_socket, "quit", blocking=True)
        else:
            # Nothing is written into the guest. The command image was populated
            # before QEMU launched, so the guest reads it and emits the markers
            # itself; the controller only tails the transcript.
            serial = FileSerial(arguments.serial_log)
            serial.wait_for(END_MARKER)
            if arguments.no_save_snapshot:
                # There is no input channel to tell the guest to shut down, so
                # stop QEMU from the control side.
                qmp_command(arguments.qmp_socket, "quit", blocking=True)
            else:
                if not arguments.post_snapshot_name:
                    raise ValueError("resume snapshot requires --post-snapshot-name")
                qmp_command(
                    arguments.qmp_socket,
                    "human-monitor-command",
                    "command-line",
                    "savevm {}".format(arguments.post_snapshot_name),
                    blocking=True,
                )
                qmp_command(arguments.qmp_socket, "quit", blocking=True)

        return process.wait(timeout=arguments.timeout)
    finally:
        if serial is not None:
            serial.close()
        stop_process(process)


if __name__ == "__main__":
    try:
        sys.exit(run_controller(parse_args()))
    except Exception as error:
        print("QEMU controller failed: {}".format(error), file=sys.stderr)
        sys.exit(1)
