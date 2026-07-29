#!/usr/bin/env python3
"""Run QEMU plus its serial/QMP controller inside Hermit's boundary."""

import argparse
from pathlib import Path
import socket
import subprocess
import sys
import time
from typing import List, Optional

from demo_common import qmp_command, stop_process, wait_for_socket


BOOT_MARKER = "HERMIT-QEMU-BASELINE-BOOT-OK"
SHELL_PROMPT = "~ #"
BEGIN_MARKER = "__HERMIT_COMMAND_BEGIN__"
END_MARKER = "__HERMIT_COMMAND_END__"


# QEMU's initial process state influences the VM snapshot. Do not leak harness
# settings such as QEMU_TIMEOUT or proxy variables into its initial stack.
QEMU_ENV = {
    "LC_ALL": "C",
    "TZ": "UTC",
}


class BlockingSerial:
    """Single-threaded serial transport used inside Hermit's scheduler."""

    def __init__(self, socket_path: Path, transcript: Path) -> None:
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.connect(str(socket_path))
        self.transcript = Path(transcript).open("wb")
        self.buffer = bytearray()

    def wait_for(self, marker: str, count: int = 1) -> None:
        marker_bytes = marker.encode()
        while self.buffer.count(marker_bytes) < count:
            chunk = self.socket.recv(65536)
            if not chunk:
                raise RuntimeError(
                    "serial disconnected before marker {!r}".format(marker)
                )
            self.buffer.extend(chunk)
            self.transcript.write(chunk)
            self.transcript.flush()

    def send_line(self, line: str) -> None:
        self.socket.sendall(line.encode() + b"\n")

    def close(self) -> None:
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.socket.close()
        self.transcript.close()


class FileSerial:
    """Read-only serial transport that tails QEMU's `-serial file:` transcript.

    Boot mode only reads the console (waiting for boot/prompt markers), so QEMU
    writes the serial stream straight to a file and this class tails it. Unlike a
    unix-socket chardev, a file sink puts no pollable fd in QEMU's main loop, so
    it cannot starve the -icount vCPU under `hermit --no-rcb-time`. There is no
    internal deadline: the out-of-container demo process enforces the real
    wall-clock timeout, because time is virtualized inside Hermit.
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
    if load_snapshot is None:
        # Boot reads the serial console but never writes to it, so back it with a
        # plain file chardev instead of a listening unix socket. A file sink is a
        # pure write() target with NO descriptor in QEMU's glib main-loop poll
        # set. A `unix:...,server=on` chardev, by contrast, adds a pollable
        # socket fd whose readiness is driven by host timing; under
        # `hermit run --no-rcb-time`, that host-timing-dependent poll() lets the
        # QEMU main-loop thread monopolize the deterministic scheduler and starve
        # the -icount vCPU thread, so the guest never advances and no serial line
        # is ever emitted (the observed 600s "boot timeout"). The file backend
        # removes that channel and boots deterministically. `serial_endpoint` is
        # the transcript file that QEMU writes and the controller tails.
        command.extend(["-serial", "file:{}".format(serial_endpoint)])
    else:
        # Resume must type a command into the console, so it needs a
        # bidirectional transport and keeps the unix-socket serial chardev.
        # `serial_endpoint` is the unix socket path.
        command.extend(
            ["-serial", "unix:{},server=on,wait=off".format(serial_endpoint)]
        )
    command.extend(
        [
            "-qmp",
            "unix:{},server=on,wait=off".format(qmp_socket),
            "-drive",
            "if=none,id=hermit-snapshot-store,file={},format=qcow2".format(disk),
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
    # Boot uses a `-serial file:` transcript (--serial-log) and needs no socket;
    # resume keeps the bidirectional unix-socket serial transport.
    parser.add_argument("--serial-socket", type=Path)
    parser.add_argument("--serial-log", type=Path, required=True)
    parser.add_argument("--disk", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--snapshot-name", default="hermit-boot")
    parser.add_argument("--timeout", type=float, required=True)
    parser.add_argument("--guest-command")
    parser.add_argument("--post-snapshot-name")
    parser.add_argument("--no-save-snapshot", action="store_true")
    return parser.parse_args()


def run_controller(arguments: argparse.Namespace) -> int:
    load_snapshot = arguments.snapshot_name if arguments.mode == "resume" else None
    # Boot backs serial with a file (deterministic, no socket poll); resume backs
    # it with the bidirectional unix socket. build_qemu_command selects the
    # backend from load_snapshot, so pass the matching endpoint.
    serial_endpoint = (
        arguments.serial_log if arguments.mode == "boot" else arguments.serial_socket
    )
    command = build_qemu_command(
        arguments.qemu,
        arguments.qmp_socket,
        serial_endpoint,
        arguments.disk,
        arguments.kernel,
        arguments.initrd,
        load_snapshot,
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
            serial.wait_for(SHELL_PROMPT)
            qmp_command(
                arguments.qmp_socket,
                "human-monitor-command",
                "command-line",
                "savevm {}".format(arguments.snapshot_name),
                blocking=True,
            )
            qmp_command(arguments.qmp_socket, "quit", blocking=True)
        else:
            if not arguments.guest_command:
                raise ValueError("resume mode requires --guest-command")
            if arguments.serial_socket is None:
                raise ValueError("resume mode requires --serial-socket")
            wait_for_socket(arguments.serial_socket, process, arguments.timeout)
            serial = BlockingSerial(arguments.serial_socket, arguments.serial_log)
            serial.send_line("")
            serial.wait_for(SHELL_PROMPT)
            serial.send_line("echo {}".format(BEGIN_MARKER))
            serial.send_line(arguments.guest_command)
            serial.send_line("echo {}".format(END_MARKER))
            serial.wait_for(END_MARKER, count=2)
            if arguments.no_save_snapshot:
                serial.send_line("poweroff -f")
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
