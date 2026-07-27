#!/usr/bin/env python3
"""Shared utilities for the Python QEMU snapshot demos."""

import datetime as dt
import difflib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import stat
import struct
import subprocess
import sys
import threading
import time
from typing import Any, Dict, List, Optional, Sequence, Tuple


WALLCLOCK_RE = re.compile(r"^[0-9T:.Z-]+ +")
RUN_PATH_RE = re.compile(r"(?:boot|resume)-[0-9TZ._-]+(?:-[0-9]+)?")
HERMIT_TMP_RE = re.compile(r"hermit-demo\.[A-Za-z0-9]+")
HEX_ADDRESS_RE = re.compile(r"\b0[xX][0-9A-Fa-f]+\b")
COMMIT_TURN_RE = re.compile(r"(COMMIT turn )\d+")
COMMITTED_TIME_RE = re.compile(r"(previously committed )[0-9_.]+s")
LOGICAL_TIME_RE = re.compile(r"LogicalTime\(\d+\)")
SCHEDULER_TURNS_RE = re.compile(r"(scheduler ran )\d+( turns)")
NUMBER_RE = re.compile(r"\b\d[\d_]*(?:\.\d[\d_]*)?(?:ns|us|µs|ms)?\b")


def hash_file(path: Path) -> str:
    """Return the SHA-256 digest of a file."""
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonicalize_qcow2_snapshot_timestamp(path: Path, snapshot_name: str) -> None:
    """Zero a qcow2 snapshot's non-guest subsecond creation timestamp."""
    path = Path(path)
    with path.open("r+b") as image:
        header = image.read(72)
        if len(header) != 72 or header[:4] != b"QFI\xfb":
            raise ValueError("not a qcow2 image: {}".format(path))
        version = struct.unpack_from(">I", header, 4)[0]
        if version < 3:
            raise ValueError(
                "qcow2 version {} lacks v3 snapshot metadata".format(version)
            )
        snapshot_count = struct.unpack_from(">I", header, 60)[0]
        snapshot_offset = struct.unpack_from(">Q", header, 64)[0]
        image.seek(snapshot_offset)
        for _ in range(snapshot_count):
            entry_offset = image.tell()
            entry = image.read(40)
            if len(entry) != 40:
                raise ValueError("truncated qcow2 snapshot table: {}".format(path))
            _, _, id_size, name_size, _, _, _, _, extra_size = struct.unpack(
                ">QIHHIIQII", entry
            )
            extra_and_strings = image.read(extra_size + id_size + name_size)
            if len(extra_and_strings) != extra_size + id_size + name_size:
                raise ValueError("truncated qcow2 snapshot entry: {}".format(path))
            name_start = extra_size + id_size
            name = extra_and_strings[name_start:].decode("utf-8")
            if name == snapshot_name:
                image.seek(entry_offset + 20)
                image.write(struct.pack(">I", 0))
                image.flush()
                return
            entry_size = 40 + extra_size + id_size + name_size
            image.seek(entry_offset + ((entry_size + 7) // 8) * 8)
    raise ValueError("snapshot {!r} not found in {}".format(snapshot_name, path))


def _tool_version(command: Sequence[str]) -> str:
    try:
        result = subprocess.run(
            list(command),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            text=True,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return "unavailable: {}".format(error)
    first_line = result.stdout.splitlines()
    return first_line[0] if first_line else "unknown"


def _write_json(path: Path, value: Dict[str, Any]) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name("{}.tmp.{}".format(path.name, os.getpid()))
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    os.replace(str(temporary), str(path))


def save_metadata(
    run_dir: Path,
    qcow2_path: Optional[Path],
    info_log: Path,
    extra: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Save machine-readable metadata for one run and return it."""
    run_dir = Path(run_dir)
    info_log = Path(info_log)
    run_dir.mkdir(parents=True, exist_ok=True)
    metadata: Dict[str, Any] = {
        "schema_version": 1,
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "info_log": str(info_log.resolve()),
        "info_log_sha256": hash_file(info_log),
        "hermit_version": _tool_version(
            [os.environ.get("HERMIT_RELEASE", "hermit"), "--version"]
        ),
        "qemu_version": _tool_version(
            [os.environ.get("QEMU_BIN", "qemu-system-x86_64"), "--version"]
        ),
    }
    if qcow2_path is not None:
        qcow2_path = Path(qcow2_path)
        metadata.update(
            {
                "qcow2_path": str(qcow2_path.resolve()),
                "qcow2_sha256": hash_file(qcow2_path),
                "qcow2_size": qcow2_path.stat().st_size,
            }
        )
    if extra:
        metadata.update(extra)
    _write_json(run_dir / "run-metadata.json", metadata)
    return metadata


def load_anchor(run_dir: Path) -> Optional[Dict[str, Any]]:
    """Load the first-run metadata anchor, if present."""
    anchor_path = Path(run_dir) / "run-metadata.json"
    if not anchor_path.is_file():
        return None
    return json.loads(anchor_path.read_text())


def save_anchor(run_dir: Path, metadata: Dict[str, Any]) -> Path:
    """Persist a metadata object as the first-run anchor."""
    anchor_path = Path(run_dir) / "run-metadata.json"
    _write_json(anchor_path, metadata)
    return anchor_path


def _normalize_log_line(line: str) -> str:
    line = WALLCLOCK_RE.sub("", line.rstrip("\n"))
    line = RUN_PATH_RE.sub("<run>", line)
    line = HERMIT_TMP_RE.sub("hermit-demo.<run>", line)
    line = HEX_ADDRESS_RE.sub("<address>", line)
    line = COMMIT_TURN_RE.sub(r"\1<turn>", line)
    line = COMMITTED_TIME_RE.sub(r"\1<virtual-time>", line)
    line = LOGICAL_TIME_RE.sub("LogicalTime(<virtual-time>)", line)
    line = SCHEDULER_TURNS_RE.sub(r"\1<turns>\2", line)
    if line.startswith("Final virtual global (cpu) time:"):
        line = "Final virtual global (cpu) time: <virtual-time>"
    elif line.startswith("Elapsed virtual global (cpu) time:"):
        line = "Elapsed virtual global (cpu) time: <virtual-time>"
    elif line.startswith("Timeslice stats:"):
        line = "Timeslice stats: <normalized>"
    line = NUMBER_RE.sub("<number>", line)
    return line


def hermit_log_diff(log1: Path, log2: Path) -> str:
    """Return a compact normalized diff at the first Hermit log divergence."""
    before: List[Tuple[int, str, str]] = []
    with Path(log1).open(errors="replace") as left, Path(log2).open(
        errors="replace"
    ) as right:
        line_number = 0
        while True:
            left_line = left.readline()
            right_line = right.readline()
            if not left_line and not right_line:
                return ""
            line_number += 1
            normalized_left = _normalize_log_line(left_line)
            normalized_right = _normalize_log_line(right_line)
            if normalized_left != normalized_right:
                left_context = [item[1] for item in before]
                right_context = [item[2] for item in before]
                left_context.append(normalized_left)
                right_context.append(normalized_right)
                diff = difflib.unified_diff(
                    left_context,
                    right_context,
                    fromfile=str(log1),
                    tofile=str(log2),
                    lineterm="",
                )
                return "first normalized divergence at line {}:\n{}".format(
                    line_number, "\n".join(diff)
                )
            before.append((line_number, normalized_left, normalized_right))
            before = before[-3:]


def compare_runs(
    anchor: Dict[str, Any], current: Dict[str, Any]
) -> Tuple[bool, List[str]]:
    """Compare exact artifacts and normalized logs, returning a clear report."""
    passed = True
    report: List[str] = []
    if anchor.get("qemu_argv") == current.get("qemu_argv"):
        report.append("PASS: QEMU argv matches anchor")
    else:
        passed = False
        report.append("WARN: QEMU argv differs from anchor")
    for field, label in (
        ("qcow2_sha256", "qcow2 SHA-256"),
        ("guest_output_sha256", "guest output SHA-256"),
    ):
        if field not in anchor and field not in current:
            continue
        if anchor.get(field) == current.get(field):
            report.append("PASS: {} matches ({})".format(label, current.get(field)))
        else:
            passed = False
            report.append(
                "WARN: {} differs: anchor={} current={}".format(
                    label, anchor.get(field), current.get(field)
                )
            )

    anchor_log = anchor.get("info_log")
    current_log = current.get("info_log")
    if anchor_log and current_log and Path(anchor_log).is_file():
        difference = hermit_log_diff(Path(anchor_log), Path(current_log))
        if difference:
            passed = False
            report.append(
                "WARN: normalized Hermit INFO log differs\n{}".format(difference)
            )
        else:
            report.append("PASS: normalized Hermit INFO log matches")
    else:
        passed = False
        report.append("WARN: anchor INFO log is unavailable")
    return passed, report


def print_comparison(passed: bool, report: Sequence[str]) -> None:
    for line in report:
        print(line)
    print("PASS: run matches anchor" if passed else "WARN: RUN DIVERGED FROM ANCHOR")


def print_header(title: str) -> None:
    width = 42
    title_width = width - 8
    if len(title) > title_width:
        raise ValueError("demo title is too wide: {}".format(title))
    print()
    print("=" * width)
    print("=== {} ===".format(title.center(title_width)))
    print("=" * width)
    print()


def banner(title: str) -> None:
    print("\n=== {} ===".format(title), flush=True)


def run_checked(command: Sequence[str], cwd: Optional[Path] = None) -> None:
    subprocess.run(list(command), cwd=str(cwd) if cwd else None, check=True)


def make_run_dir(parent: Path, prefix: str) -> Path:
    timestamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run_dir = (
        Path(parent) / "run-history" / "{}-{}-{}".format(prefix, timestamp, os.getpid())
    )
    run_dir.mkdir(parents=True, exist_ok=False)
    return run_dir


def wait_for_process(
    process: subprocess.Popen,
    timeout: float,
    stream_path: Optional[Path] = None,
    progress_label: Optional[str] = None,
) -> int:
    """Wait for a process, optionally streaming a growing file or showing progress."""
    deadline = time.monotonic() + timeout
    started = time.monotonic()
    stream = None
    last_progress = -1
    try:
        while True:
            if (
                stream is None
                and stream_path is not None
                and Path(stream_path).is_file()
            ):
                stream = Path(stream_path).open("rb")
            if stream is not None:
                chunk = stream.read()
                if chunk:
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()

            return_code = process.poll()
            if return_code is not None:
                if stream is not None:
                    chunk = stream.read()
                    if chunk:
                        sys.stdout.buffer.write(chunk)
                        sys.stdout.buffer.flush()
                if progress_label is not None:
                    print(
                        "\r{}: done ({:.1f}s)".format(
                            progress_label, time.monotonic() - started
                        )
                    )
                return return_code

            now = time.monotonic()
            if now >= deadline:
                raise TimeoutError("process exceeded timeout of {}s".format(timeout))
            if progress_label is not None:
                elapsed = int(now - started)
                if elapsed != last_progress:
                    print(
                        "\r{}: {}s".format(progress_label, elapsed), end="", flush=True
                    )
                    last_progress = elapsed
            time.sleep(0.1)
    finally:
        if stream is not None:
            stream.close()


def acquire_demo_lock(path: Path) -> Any:
    """Acquire the single-writer lock for fixed QEMU runtime paths."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = path.open("a+")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        handle.close()
        raise RuntimeError("another QEMU demo is already using {}".format(path))
    return handle


def release_demo_lock(handle: Any) -> None:
    if handle is None:
        return
    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
    handle.close()


def wait_for_socket(path: Path, process: subprocess.Popen, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    path = Path(path)
    while time.monotonic() < deadline:
        if path.exists() and stat.S_ISSOCK(path.stat().st_mode):
            return
        if process.poll() is not None:
            raise RuntimeError("Hermit exited before socket appeared: {}".format(path))
        time.sleep(0.1)
    raise TimeoutError("timed out waiting for socket: {}".format(path))


def qmp_command(
    socket_path: Path,
    execute: str,
    argument_name: Optional[str] = None,
    argument_value: Optional[str] = None,
    blocking: bool = False,
) -> Any:
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    if not blocking:
        connection.settimeout(float(os.environ.get("DEMO_QMP_TIMEOUT", "300")))
    connection.connect(str(socket_path))
    stream = connection.makefile("rwb", buffering=0)

    def receive(message_id: str) -> Any:
        while True:
            line = stream.readline()
            if not line:
                raise RuntimeError("QMP disconnected before replying")
            message = json.loads(line)
            if message.get("id") != message_id:
                continue
            if "error" in message:
                raise RuntimeError(str(message["error"]))
            return message.get("return")

    greeting = json.loads(stream.readline())
    if "QMP" not in greeting:
        raise RuntimeError("invalid QMP greeting: {!r}".format(greeting))
    stream.write(
        json.dumps({"execute": "qmp_capabilities", "id": "caps"}).encode() + b"\n"
    )
    receive("caps")
    request: Dict[str, Any] = {"execute": execute, "id": "command"}
    if argument_name:
        request["arguments"] = {argument_name: argument_value}
    stream.write(json.dumps(request).encode() + b"\n")
    result = receive("command")
    stream.close()
    connection.close()
    return result


class SerialSession:
    """Bidirectional Unix serial connection with streaming transcript capture."""

    def __init__(
        self, socket_path: Path, transcript: Path, stream_output: bool
    ) -> None:
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.connect(str(socket_path))
        self.socket.settimeout(0.5)
        self.transcript_path = Path(transcript)
        self.transcript = self.transcript_path.open("wb")
        self.stream_output = stream_output
        self.buffer = bytearray()
        self.condition = threading.Condition()
        self.stopped = False
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def _read(self) -> None:
        try:
            while not self.stopped:
                try:
                    chunk = self.socket.recv(65536)
                except socket.timeout:
                    continue
                except OSError:
                    break
                if not chunk:
                    break
                self.transcript.write(chunk)
                self.transcript.flush()
                if self.stream_output:
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                with self.condition:
                    self.buffer.extend(chunk)
                    self.condition.notify_all()
        finally:
            with self.condition:
                self.condition.notify_all()

    def wait_for(self, marker: str, timeout: float, count: int = 1) -> None:
        marker_bytes = marker.encode()
        deadline = time.monotonic() + timeout
        with self.condition:
            while self.buffer.count(marker_bytes) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        "timed out waiting for serial marker: {!r}".format(marker)
                    )
                self.condition.wait(min(remaining, 0.5))

    def send_line(self, line: str) -> None:
        self.socket.sendall(line.encode() + b"\n")

    def bytes(self) -> bytes:
        with self.condition:
            return bytes(self.buffer)

    def close(self) -> None:
        self.stopped = True
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.socket.close()
        self.thread.join(timeout=5)
        self.transcript.close()


def stop_process(process: Optional[subprocess.Popen]) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)


def extract_info_tail(log_path: Path) -> List[str]:
    """Extract the final COMMIT, shutdown sequence, and compact run report."""
    values: Dict[str, str] = {}
    kills: List[str] = []
    report: List[str] = []
    with Path(log_path).open(errors="replace") as source:
        for raw_line in source:
            line = WALLCLOCK_RE.sub("", raw_line.rstrip("\n"))
            if line.startswith(" COMMIT turn "):
                values["commit"] = line
            elif "Scheduler authorized" in line:
                values["authorized"] = line
            elif "tail_inject of syscall:" in line:
                values["tail_inject"] = line
            elif "logically_kill:" in line:
                kills.append(line)
                kills = kills[-2:]
            elif "scheduler (step2_process_blocked):" in line:
                values["blocked"] = line
            elif "[scheduler] run queue empty" in line:
                values["empty"] = line
            elif "detcore shut down" in line:
                values["shutdown"] = line
            elif (
                "hermit run report" in line
                or line.startswith("Final thread-tree")
                or line.startswith("There were ")
                or line.startswith("Internally,")
                or line.startswith("Final virtual global (cpu) time:")
                or line.startswith("Elapsed virtual global (cpu) time:")
                or line.startswith("Timeslice stats:")
            ):
                report.append(line)
    result = [
        values[key] for key in ("commit", "authorized", "tail_inject") if key in values
    ]
    result.extend(kills)
    result.extend(
        values[key] for key in ("blocked", "empty", "shutdown") if key in values
    )
    result.extend(report)
    return result


def copy_file(source: Path, destination: Path) -> None:
    Path(destination).parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(str(source), str(destination))
