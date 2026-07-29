#!/usr/bin/env python3
"""Shared utilities for the Python QEMU snapshot demos."""

import ctypes
import datetime as dt
import errno
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
import tempfile
import threading
import time
from typing import Any, Dict, List, Optional, Sequence, Tuple


WALLCLOCK_RE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z[ \t]+"
)

# detcore logs the host-filesystem inode of a touched file as its resource id,
# e.g. `FileContents(263701387)`. That number is a host-physical identifier the
# kernel hands out afresh whenever a file is (re)created, so it varies run to run
# even when guest execution is bit-identical (recreated fixed-path files churn it
# just as per-run private directories do). It is nondeterministic in exactly the
# same sense as the wallclock prefix, so the exact-log comparison folds it to a
# stable token. Guest-observable determinism is still asserted independently via
# the qcow2 / serial / guest-output SHAs.
FILE_INODE_RE = re.compile(r"FileContents\(\d+\)")


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


def _tool_sha256(executable: str) -> str:
    path = Path(shutil.which(executable) or executable)
    if not path.is_file():
        return "unavailable: {} is not a file".format(path)
    try:
        return hash_file(path)
    except OSError as error:
        return "unavailable: {}".format(error)


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
    qemu = os.environ.get("QEMU_BIN", "qemu-system-x86_64")
    metadata: Dict[str, Any] = {
        "schema_version": 1,
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "info_log": str(info_log.resolve()),
        "info_log_sha256": hash_file(info_log),
        "hermit_version": _tool_version(
            [os.environ.get("HERMIT_RELEASE", "hermit"), "--version"]
        ),
        "qemu_version": _tool_version([qemu, "--version"]),
        "qemu_binary_sha256": _tool_sha256(qemu),
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


# ---------------------------------------------------------------------------
# Concurrent-safe anchor claim.
#
# Multiple demo runs may execute simultaneously. Each builds its whole result in
# a private working directory (make_temp_result_dir) and then races to publish it
# as THE anchor with a single atomic, no-clobber rename. Exactly one run wins and
# becomes the first-run anchor; every other run loses cleanly (EEXIST), archives
# its own result, and compares against the fully-committed anchor. Because the
# entire result directory is moved in one step, a loser never observes a
# half-written anchor.
# ---------------------------------------------------------------------------

# renameat2(2) with RENAME_NOREPLACE is the atomic, no-clobber primitive.
_AT_FDCWD = -100
_RENAME_NOREPLACE = 1
# x86-64 renameat2 syscall number; used only if the glibc wrapper is missing.
_SYS_RENAMEAT2 = 316


def _rename_noreplace(src: Path, dst: Path) -> None:
    """Atomically rename ``src`` to ``dst``, refusing to clobber an existing dst.

    Wraps Linux ``renameat2(RENAME_NOREPLACE)``: the move either creates ``dst``
    atomically or raises ``OSError(EEXIST)`` because ``dst`` already exists. It
    never overwrites ``dst``, and there is no check-then-move window (no TOCTOU).
    Raises ``OSError(ENOSYS)``/``OSError(EINVAL)`` when the kernel or filesystem
    lacks RENAME_NOREPLACE, so callers can fall back to a lock-guarded rename.
    """
    libc = ctypes.CDLL(None, use_errno=True)
    src_b = os.fsencode(str(src))
    dst_b = os.fsencode(str(dst))
    wrapper = getattr(libc, "renameat2", None)
    if wrapper is not None:
        wrapper.restype = ctypes.c_int
        wrapper.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        result = wrapper(
            _AT_FDCWD, src_b, _AT_FDCWD, dst_b, ctypes.c_uint(_RENAME_NOREPLACE)
        )
    else:
        result = libc.syscall(
            ctypes.c_long(_SYS_RENAMEAT2),
            ctypes.c_int(_AT_FDCWD),
            ctypes.c_char_p(src_b),
            ctypes.c_int(_AT_FDCWD),
            ctypes.c_char_p(dst_b),
            ctypes.c_uint(_RENAME_NOREPLACE),
        )
    if result != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(dst))


def _publish_anchor_locked(work_dir: Path, anchor_dir: Path) -> bool:
    """Portable fallback claim: serialize with an exclusive lock, then rename.

    Used only when the filesystem lacks ``renameat2(RENAME_NOREPLACE)``. The lock
    removes the check-then-rename race: a loser cannot rename while the winner
    holds the lock, so the plain ``os.rename`` here is safe and never clobbers a
    published anchor.
    """
    lock_path = anchor_dir.with_name(anchor_dir.name + ".claim.lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    handle = lock_path.open("a+")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        if anchor_dir.exists():
            return False
        os.rename(str(work_dir), str(anchor_dir))
        return True
    finally:
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()


def make_temp_result_dir(assets: Path, prefix: str) -> Path:
    """Create a private, unique per-run working directory under ``<assets>/.work``.

    Everything for one concurrent run (sockets, snapshot, logs, metadata) is
    built here in isolation so simultaneous runs never share a path. The
    directory is later published atomically as the anchor (winner) or archived
    into run-history (loser).
    """
    work_root = Path(assets) / ".work"
    work_root.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix="{}-".format(prefix), dir=str(work_root)))


def publish_anchor(work_dir: Path, anchor_dir: Path) -> bool:
    """Atomically publish ``work_dir`` as THE anchor directory.

    Returns ``True`` if this run won the anchor (``work_dir`` became
    ``anchor_dir``), or ``False`` if an anchor already existed, in which case the
    caller is a loser and must compare against the committed anchor. The whole
    result directory is moved in one atomic, no-clobber step, so no reader ever
    observes a half-written anchor.
    """
    work_dir = Path(work_dir)
    anchor_dir = Path(anchor_dir)
    anchor_dir.parent.mkdir(parents=True, exist_ok=True)
    try:
        _rename_noreplace(work_dir, anchor_dir)
        return True
    except OSError as error:
        if error.errno == errno.EEXIST:
            return False
        if error.errno in (errno.ENOSYS, errno.EINVAL):
            return _publish_anchor_locked(work_dir, anchor_dir)
        raise


def load_committed_anchor(anchor_dir: Path) -> Optional[Dict[str, Any]]:
    """Load the committed anchor metadata, resolving its bundled INFO log path.

    Returns ``None`` when no anchor exists yet. The anchor's Hermit INFO log is
    bundled inside the anchor directory; the ``info_log`` field recorded before
    publication points at the pre-publish working path, so rewrite it to the
    bundled copy for log comparison.
    """
    anchor_dir = Path(anchor_dir)
    anchor_meta = anchor_dir / "run-metadata.json"
    if not anchor_meta.is_file():
        return None
    metadata = json.loads(anchor_meta.read_text())
    bundled_log = anchor_dir / "hermit-info.log"
    if bundled_log.is_file():
        metadata["info_log"] = str(bundled_log.resolve())
    return metadata


def archive_result_dir(work_dir: Path, assets: Path, prefix: str) -> Path:
    """Move a completed non-anchor run into run-history under a unique name."""
    work_dir = Path(work_dir)
    timestamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    history = Path(assets) / "run-history"
    history.mkdir(parents=True, exist_ok=True)
    # work_dir.name carries the mkdtemp random suffix, guaranteeing uniqueness.
    destination = history / "{}-{}-{}".format(prefix, timestamp, work_dir.name)
    os.rename(str(work_dir), str(destination))
    return destination


def publish_file_atomic(src: Path, dst: Path) -> None:
    """Copy ``src`` onto ``dst`` atomically (temp copy + ``os.replace``).

    Concurrent-safe replacement for a plain copy of a shared handoff artifact:
    readers always see either the old or the new complete file, never a partial.
    """
    src = Path(src)
    dst = Path(dst)
    dst.parent.mkdir(parents=True, exist_ok=True)
    temporary = dst.with_name("{}.tmp.{}".format(dst.name, os.getpid()))
    shutil.copy2(str(src), str(temporary))
    os.replace(str(temporary), str(dst))


def _strip_wallclock_prefix(line: str) -> str:
    """Strip only the nondeterministic tracing wallclock prefix."""
    return WALLCLOCK_RE.sub("", line, count=1)


def _normalize_log_line(line: str) -> str:
    """Fold out host-physical nondeterminism (wallclock prefix, inode numbers).

    See WALLCLOCK_RE and FILE_INODE_RE for why each token is nondeterministic
    even when the guest execution is bit-identical. Anything else surviving here
    is a real divergence.
    """
    line = _strip_wallclock_prefix(line)
    line = FILE_INODE_RE.sub("FileContents(<inode>)", line)
    return line


def hermit_log_diff(log1: Path, log2: Path) -> str:
    """Return the first exact log divergence after normalizing host-physical noise."""
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
                context = ["  {!r}".format(item[1]) for item in before]
                context.extend(
                    (
                        "- {!r}".format(normalized_left),
                        "+ {!r}".format(normalized_right),
                    )
                )
                return "first divergence at line {} (wallclock + inode normalized):\n{}".format(
                    line_number, "\n".join(context)
                )
            before.append((line_number, normalized_left, normalized_right))
            before = before[-3:]


def compare_runs(
    anchor: Dict[str, Any], current: Dict[str, Any]
) -> Tuple[bool, List[str]]:
    """Compare exact artifacts and timestamp-stripped logs."""
    passed = True
    report: List[str] = []
    if anchor.get("qemu_argv") == current.get("qemu_argv"):
        report.append("PASS: QEMU argv matches first run")
    else:
        passed = False
        report.append(
            "WARN: QEMU argv differs from first run; executable path or arguments changed"
        )
    for field, label in (
        ("qemu_version", "QEMU version"),
        ("qemu_binary_sha256", "QEMU binary SHA-256"),
        ("qcow2_sha256", "qcow2 SHA-256"),
        ("serial_sha256", "serial output SHA-256"),
        ("guest_output_sha256", "guest output SHA-256"),
    ):
        if field not in anchor and field not in current:
            continue
        if anchor.get(field) == current.get(field):
            report.append("PASS: {} matches ({})".format(label, current.get(field)))
        else:
            passed = False
            report.append(
                "WARN: {} differs from first run: first={} current={}".format(
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
                "WARN: exact Hermit log differs from first run after normalizing only wallclock timestamps and host inode numbers\n{}".format(
                    difference
                )
            )
        else:
            report.append(
                "PASS: exact Hermit log matches first run after normalizing wallclock timestamps and host inode numbers"
            )
    else:
        passed = False
        report.append(
            "WARN: cannot compare Hermit INFO logs because the first-run log is unavailable"
        )
    return passed, report


def print_comparison(
    passed: bool,
    report: Sequence[str],
    snapshot_sha256: Optional[str] = None,
    subject: str = "Run",
) -> None:
    for line in report:
        print(line)
    if passed:
        print("PASS: all repeat checks match the first run")
    else:
        print(
            "PARTIAL: workload completed, but repeat verification differs from the first run."
        )
        print("Review the WARN lines above before sharing this artifact.")
    if passed and snapshot_sha256 is not None:
        print()
        print("🎉 DETERMINISTIC! Snapshot SHA-256 matches previous run:")
        print("   {}".format(snapshot_sha256))
        print("   {} is bitwise-reproducible under Hermit.".format(subject))


def check_dependencies(root: Path) -> str:
    """Run the shared dependency check and return its one-line result."""
    result = subprocess.run(
        ["make", "--no-print-directory", "-s", "check-deps"],
        cwd=str(root),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.stderr.write(result.stdout)
        raise subprocess.CalledProcessError(result.returncode, result.args)
    lines = [line for line in result.stdout.splitlines() if line]
    if len(lines) != 1 or not lines[0].startswith("Dependency check passed:"):
        raise RuntimeError(
            "unexpected dependency-check output: {!r}".format(result.stdout)
        )
    return lines[0]


def check_qemu_dependencies(root: Path) -> str:
    """Run the zero-build QEMU demo preflight and return its summary."""
    result = subprocess.run(
        [str(root / "demos/lib/qemu-assets.sh"), "--check"],
        cwd=str(root),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.stderr.write(result.stdout)
        raise subprocess.CalledProcessError(result.returncode, result.args)
    lines = [line for line in result.stdout.splitlines() if line]
    if len(lines) != 1 or not lines[0].startswith(
        "QEMU dependency check passed:"
    ):
        raise RuntimeError(
            "unexpected QEMU dependency-check output: {!r}".format(result.stdout)
        )
    return lines[0]


def print_header(title: str, description: Sequence[str], dependency: str) -> None:
    width = 80
    title_width = width - 10
    if len(title) > title_width:
        raise ValueError("demo title is too wide: {}".format(title))
    print("=" * width)
    print("=====" + title.center(title_width) + "=====")
    print()
    for line in description:
        if len(line) > width:
            raise ValueError("demo description is too wide: {}".format(line))
        print(line)
    print(dependency)
    print()
    print("=" * width)


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
