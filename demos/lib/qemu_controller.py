#!/usr/bin/env python3
"""Run QEMU plus its serial/QMP controller inside Hermit's boundary."""

import argparse
from pathlib import Path
import socket
import subprocess
import sys
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


def build_qemu_command(
    qemu: str,
    qmp_socket: Path,
    serial_socket: Path,
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
        "-serial",
        "unix:{},server=on,wait=off".format(serial_socket),
        "-qmp",
        "unix:{},server=on,wait=off".format(qmp_socket),
        "-drive",
        "if=none,id=hermit-snapshot-store,file={},format=qcow2".format(disk),
    ]
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
    parser.add_argument("--serial-socket", type=Path, required=True)
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
    command = build_qemu_command(
        arguments.qemu,
        arguments.qmp_socket,
        arguments.serial_socket,
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
        wait_for_socket(arguments.serial_socket, process, arguments.timeout)
        serial = BlockingSerial(arguments.serial_socket, arguments.serial_log)

        if arguments.mode == "boot":
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
