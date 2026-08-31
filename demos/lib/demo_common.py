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
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from enum import Enum
from typing import Any, Dict, List, Mapping, Optional, Sequence, Tuple, TypedDict, cast


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

# The DETLOG renders raw guest virtual addresses (stack buffers, mmap regions)
# for syscall pointer arguments, e.g. `openat(-100, 0x7fffffffa240 -> "...")`.
# Those canonical-userspace addresses (0x7f...) shift by the size of the guest's
# argv+env block, so two SEPARATE hermit invocations with different inherited
# host environments place the stack at a different base and every such address
# differs by a constant offset even when the guest does bit-identical work. The
# dereferenced string after `-> ` is preserved, so masking only the address
# folds host-physical layout noise without hiding a real path/content change.
# Guest-observable determinism is asserted independently via the qcow2 / serial /
# guest-output SHAs; see compare_runs.
USER_ADDR_RE = re.compile(r"0x7f[0-9a-f]{6,}")
RUN_METADATA_SCHEMA_VERSION = 2


class QemuRunKind(str, Enum):
    BOOT = "qemu-boot"
    RESUME = "qemu-resume"


class QemuRunMetadataRecord(TypedDict, total=False):
    schema_version: int
    created_at: str
    kind: str
    info_log: str
    info_log_sha256: str
    hermit_version: str
    qemu_version: str
    qemu_binary_sha256: str
    qemu_argv: List[str]
    serial_log: str
    serial_sha256: str
    qcow2_path: str
    qcow2_sha256: str
    qcow2_size: int
    snapshot_name: str
    snapshot_date_nsec_canonicalized: bool
    command: str
    command_sha256: str
    guest_output: str
    guest_output_sha256: str
    snapshot_saved: bool


@dataclass(frozen=True)
class QemuRunMetadata:
    schema_version: int
    kind: QemuRunKind
    created_at: str
    info_log: str
    info_log_sha256: str
    hermit_version: str
    qemu_version: str
    qemu_binary_sha256: Optional[str]
    qemu_argv: Tuple[str, ...]
    serial_log: str
    serial_sha256: Optional[str]
    qcow2_path: Optional[str]
    qcow2_sha256: Optional[str]
    qcow2_size: Optional[int]
    snapshot_name: Optional[str]
    snapshot_date_nsec_canonicalized: Optional[bool]
    command: Optional[str]
    command_sha256: Optional[str]
    guest_output: Optional[str]
    guest_output_sha256: Optional[str]
    snapshot_saved: Optional[bool]
    raw: QemuRunMetadataRecord

    def with_info_log(self, path: Path) -> "QemuRunMetadata":
        value = dict(self.raw)
        value["info_log"] = str(Path(path).resolve())
        return parse_run_metadata(value)


COMMON_METADATA_FIELDS = frozenset(
    {
        "schema_version",
        "created_at",
        "kind",
        "info_log",
        "info_log_sha256",
        "hermit_version",
        "qemu_version",
        "qemu_binary_sha256",
        "qemu_argv",
        "serial_log",
    }
)
BOOT_METADATA_FIELDS = COMMON_METADATA_FIELDS | frozenset(
    {
        "serial_sha256",
        "qcow2_path",
        "qcow2_sha256",
        "qcow2_size",
        "snapshot_name",
        "snapshot_date_nsec_canonicalized",
    }
)
RESUME_METADATA_FIELDS = COMMON_METADATA_FIELDS | frozenset(
    {
        "command",
        "command_sha256",
        "guest_output",
        "guest_output_sha256",
        "snapshot_saved",
        "qcow2_path",
        "qcow2_sha256",
        "qcow2_size",
        "snapshot_date_nsec_canonicalized",
    }
)
METADATA_FIELDS_BY_KIND = {
    QemuRunKind.BOOT: BOOT_METADATA_FIELDS,
    QemuRunKind.RESUME: RESUME_METADATA_FIELDS,
}


def _metadata_error(field: str, detail: str) -> ValueError:
    return ValueError("qemu-run-metadata-{}: {}".format(field, detail))


def _metadata_text(value: Mapping[str, Any], field: str) -> str:
    raw = value.get(field)
    if not isinstance(raw, str) or not raw.strip():
        raise _metadata_error(field, "must be a nonempty string")
    return raw


def _metadata_sha256(value: Mapping[str, Any], field: str) -> str:
    raw = _metadata_text(value, field)
    if len(raw) != 64 or any(character not in "0123456789abcdef" for character in raw):
        raise _metadata_error(field, "must be a lowercase 64-hex SHA-256")
    return raw


def _metadata_optional_text(value: Mapping[str, Any], field: str) -> Optional[str]:
    if field not in value:
        return None
    return _metadata_text(value, field)


def _metadata_optional_sha256(value: Mapping[str, Any], field: str) -> Optional[str]:
    if field not in value:
        return None
    return _metadata_sha256(value, field)


def _metadata_qemu_argv(value: Mapping[str, Any]) -> Tuple[str, ...]:
    raw = value.get("qemu_argv")
    if (
        not isinstance(raw, list)
        or not raw
        or any(not isinstance(argument, str) or not argument for argument in raw)
    ):
        raise _metadata_error("qemu_argv", "must be a nonempty list of strings")
    return tuple(raw)


def _metadata_optional_size(value: Mapping[str, Any]) -> Optional[int]:
    if "qcow2_size" not in value:
        return None
    raw = value.get("qcow2_size")
    if not isinstance(raw, int) or isinstance(raw, bool) or raw < 0:
        raise _metadata_error("qcow2_size", "must be a nonnegative integer")
    return raw


def parse_run_metadata(value: Mapping[str, Any]) -> QemuRunMetadata:
    """Read the version before requiring its complete kind-specific shape."""
    schema_version = value.get("schema_version")
    if (
        not isinstance(schema_version, int)
        or isinstance(schema_version, bool)
        or schema_version not in (1, RUN_METADATA_SCHEMA_VERSION)
    ):
        raise _metadata_error(
            "schema_version", "unsupported value {!r}".format(schema_version)
        )
    try:
        kind = QemuRunKind(_metadata_text(value, "kind"))
    except ValueError as error:
        if str(error).startswith("qemu-run-metadata-"):
            raise
        raise _metadata_error(
            "kind", "unsupported value {!r}".format(value.get("kind"))
        )

    allowed = METADATA_FIELDS_BY_KIND.get(kind)
    if allowed is None:
        raise _metadata_error(
            "kind", "has no field contract for {!r}".format(kind.value)
        )
    unknown = sorted(set(value) - allowed)
    if unknown:
        raise _metadata_error(
            "field",
            "unknown field(s) for kind {!r}: {}".format(kind.value, ", ".join(unknown)),
        )

    created_at = _metadata_text(value, "created_at")
    info_log = _metadata_text(value, "info_log")
    info_log_sha256 = _metadata_sha256(value, "info_log_sha256")
    hermit_version = _metadata_text(value, "hermit_version")
    qemu_version = _metadata_text(value, "qemu_version")
    qemu_binary_sha256 = _metadata_optional_sha256(value, "qemu_binary_sha256")
    qemu_argv = _metadata_qemu_argv(value)
    serial_log = _metadata_text(value, "serial_log")

    serial_sha256 = _metadata_optional_sha256(value, "serial_sha256")
    qcow2_path = _metadata_optional_text(value, "qcow2_path")
    qcow2_sha256 = _metadata_optional_sha256(value, "qcow2_sha256")
    qcow2_size = _metadata_optional_size(value)
    snapshot_name = _metadata_optional_text(value, "snapshot_name")
    canonicalized = value.get("snapshot_date_nsec_canonicalized")
    if canonicalized is not None and not isinstance(canonicalized, bool):
        raise _metadata_error("snapshot_date_nsec_canonicalized", "must be a boolean")
    command = _metadata_optional_text(value, "command")
    command_sha256 = _metadata_optional_sha256(value, "command_sha256")
    guest_output = _metadata_optional_text(value, "guest_output")
    guest_output_sha256 = _metadata_optional_sha256(value, "guest_output_sha256")
    snapshot_saved = value.get("snapshot_saved")
    if snapshot_saved is not None and not isinstance(snapshot_saved, bool):
        raise _metadata_error("snapshot_saved", "must be a boolean")

    if kind is QemuRunKind.BOOT:
        if qemu_binary_sha256 is None:
            raise _metadata_error("qemu_binary_sha256", "is required for qemu-boot")
        for field, parsed in (
            ("serial_sha256", serial_sha256),
            ("qcow2_path", qcow2_path),
            ("qcow2_sha256", qcow2_sha256),
            ("qcow2_size", qcow2_size),
            ("snapshot_name", snapshot_name),
        ):
            if parsed is None:
                raise _metadata_error(field, "is required for qemu-boot")
        if canonicalized is not True:
            raise _metadata_error(
                "snapshot_date_nsec_canonicalized", "must be true for qemu-boot"
            )
    elif kind is QemuRunKind.RESUME:
        for field, parsed in (
            ("command", command),
            ("command_sha256", command_sha256),
            ("guest_output", guest_output),
            ("guest_output_sha256", guest_output_sha256),
        ):
            if parsed is None:
                raise _metadata_error(field, "is required for qemu-resume")
        if snapshot_saved is None:
            raise _metadata_error("snapshot_saved", "is required for qemu-resume")
        if qemu_binary_sha256 is None and not (
            schema_version == 1 and snapshot_saved is False
        ):
            raise _metadata_error(
                "qemu_binary_sha256",
                "is required except on schema-1 rows without a saved snapshot",
            )
        snapshot_fields = {
            "qcow2_path": qcow2_path,
            "qcow2_sha256": qcow2_sha256,
            "qcow2_size": qcow2_size,
            "snapshot_date_nsec_canonicalized": canonicalized,
        }
        if snapshot_saved:
            for field, parsed in snapshot_fields.items():
                if parsed is None:
                    raise _metadata_error(
                        field, "is required when snapshot_saved is true"
                    )
            if canonicalized is not True:
                raise _metadata_error(
                    "snapshot_date_nsec_canonicalized",
                    "must be true when snapshot_saved is true",
                )
        else:
            present = sorted(
                field for field, parsed in snapshot_fields.items() if parsed is not None
            )
            if present:
                raise _metadata_error(
                    "snapshot_saved",
                    "is false but snapshot field(s) are present: {}".format(
                        ", ".join(present)
                    ),
                )
    else:
        raise _metadata_error(
            "kind", "has no value contract for {!r}".format(kind.value)
        )

    return QemuRunMetadata(
        schema_version=schema_version,
        kind=kind,
        created_at=created_at,
        info_log=info_log,
        info_log_sha256=info_log_sha256,
        hermit_version=hermit_version,
        qemu_version=qemu_version,
        qemu_binary_sha256=qemu_binary_sha256,
        qemu_argv=qemu_argv,
        serial_log=serial_log,
        serial_sha256=serial_sha256,
        qcow2_path=qcow2_path,
        qcow2_sha256=qcow2_sha256,
        qcow2_size=qcow2_size,
        snapshot_name=snapshot_name,
        snapshot_date_nsec_canonicalized=canonicalized,
        command=command,
        command_sha256=command_sha256,
        guest_output=guest_output,
        guest_output_sha256=guest_output_sha256,
        snapshot_saved=snapshot_saved,
        raw=cast(QemuRunMetadataRecord, dict(value)),
    )


def _under_host_tmp(root: Path) -> bool:
    """Whether ``root`` resolves inside the host's ``/tmp`` tree."""
    try:
        Path(root).resolve().relative_to("/tmp")
    except ValueError:
        return False
    return True


def default_qemu_assets(root: Path) -> Path:
    """Return a host-visible, checkout-scoped default for persistent QEMU assets."""
    root = Path(root).resolve()
    if not _under_host_tmp(root):
        return root / "ignored/qemu-linux"
    # Hermit mounts a private tmpfs over /tmp. Keep persistent QEMU inputs outside it,
    # and include the canonical checkout identity so concurrent clones cannot share or
    # clean each other's snapshots.
    digest = hashlib.sha256(str(root).encode("utf-8")).hexdigest()[:12]
    return Path("/var/tmp") / "hermit-qemu-strict-l2-{}-{}".format(
        os.getuid(), digest
    )


def hermit_tmp_args(root: Path) -> List[str]:
    """Keep checkout-local QEMU controller/runtime paths visible to a traced guest."""
    return ["--tmp=/tmp"] if _under_host_tmp(root) else []


def display_path(path: Path, root: Path) -> str:
    """Render a repo-relative path when possible, otherwise a stable absolute path."""
    path = Path(path).resolve()
    try:
        return str(path.relative_to(Path(root).resolve()))
    except ValueError:
        return str(path)


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
        raise _metadata_error(
            "qemu_binary_sha256", "cannot hash {}: not a file".format(path)
        )
    try:
        return hash_file(path)
    except OSError as error:
        raise _metadata_error(
            "qemu_binary_sha256", "cannot hash {}: {}".format(path, error)
        ) from error


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
) -> QemuRunMetadata:
    """Save machine-readable metadata for one run and return it."""
    run_dir = Path(run_dir)
    info_log = Path(info_log)
    run_dir.mkdir(parents=True, exist_ok=True)
    qemu = os.environ.get("QEMU_BIN", "qemu-system-x86_64")
    metadata: Dict[str, Any] = {
        "schema_version": RUN_METADATA_SCHEMA_VERSION,
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
        overlap = sorted(set(metadata) & set(extra))
        if overlap:
            raise _metadata_error(
                "field",
                "extra fields replace common field(s): {}".format(", ".join(overlap)),
            )
        metadata.update(extra)
    typed = parse_run_metadata(metadata)
    _write_json(run_dir / "run-metadata.json", dict(typed.raw))
    return typed


def load_anchor(run_dir: Path) -> Optional[QemuRunMetadata]:
    """Load the first-run metadata anchor, if present."""
    anchor_path = Path(run_dir) / "run-metadata.json"
    if not anchor_path.is_file():
        return None
    return parse_run_metadata(json.loads(anchor_path.read_text()))


def save_anchor(run_dir: Path, metadata: QemuRunMetadata) -> Path:
    """Persist a metadata object as the first-run anchor."""
    anchor_path = Path(run_dir) / "run-metadata.json"
    _write_json(anchor_path, dict(metadata.raw))
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


def load_committed_anchor(anchor_dir: Path) -> Optional[QemuRunMetadata]:
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
    metadata = parse_run_metadata(json.loads(anchor_meta.read_text()))
    bundled_log = anchor_dir / "hermit-info.log"
    if bundled_log.is_file():
        metadata = metadata.with_info_log(bundled_log)
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
    line = USER_ADDR_RE.sub("0x<uaddr>", line)
    return line


def hermit_log_diff(log1: Path, log2: Path) -> str:
    """Return the first exact log divergence after documented normalization."""
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
                return (
                    "first divergence at line {} "
                    "(wallclock + inode + userspace address normalized):\n{}"
                ).format(line_number, "\n".join(context))
            before.append((line_number, normalized_left, normalized_right))
            before = before[-3:]


def compare_runs(
    anchor: QemuRunMetadata, current: QemuRunMetadata
) -> Tuple[bool, List[str]]:
    """Compare exact artifacts and timestamp-stripped logs."""
    passed = True
    report: List[str] = []
    if anchor.kind is not current.kind:
        passed = False
        report.append(
            "WARN: run kind differs from first run: first={} current={}".format(
                anchor.kind.value, current.kind.value
            )
        )
    if anchor.qemu_argv == current.qemu_argv:
        report.append("PASS: QEMU argv matches first run")
    else:
        passed = False
        report.append(
            "WARN: QEMU argv differs from first run; executable path or arguments changed"
        )
    for anchor_value, current_value, label in (
        (anchor.qemu_version, current.qemu_version, "QEMU version"),
        (
            anchor.qemu_binary_sha256,
            current.qemu_binary_sha256,
            "QEMU binary SHA-256",
        ),
        (anchor.qcow2_sha256, current.qcow2_sha256, "qcow2 SHA-256"),
        (anchor.serial_sha256, current.serial_sha256, "serial output SHA-256"),
        (
            anchor.guest_output_sha256,
            current.guest_output_sha256,
            "guest output SHA-256",
        ),
    ):
        if anchor_value is None and current_value is None:
            continue
        if anchor_value == current_value:
            report.append("PASS: {} matches ({})".format(label, current_value))
        else:
            passed = False
            report.append(
                "WARN: {} differs from first run: first={} current={}".format(
                    label, anchor_value, current_value
                )
            )

    # Normalize only the documented host-physical fields above. Any remaining
    # INFO difference is canonical execution evidence and must fail the repeat,
    # even when the VM artifacts happen to be byte-identical. In particular, a
    # difference that begins during Python startup can propagate into virtual
    # clock values and the QEMU execution; its origin does not make the later
    # guest-visible log evidence optional.
    anchor_log = anchor.info_log
    current_log = current.info_log
    if (
        anchor_log
        and current_log
        and Path(anchor_log).is_file()
        and Path(current_log).is_file()
    ):
        difference = hermit_log_diff(Path(anchor_log), Path(current_log))
        if difference:
            passed = False
            report.append(
                "WARN: Hermit INFO log differs from first run after normalizing "
                "wallclock timestamps, host inode numbers, and env-dependent guest "
                "addresses; canonical repeat verification failed\n{}".format(
                    difference
                )
            )
        else:
            report.append(
                "PASS: exact Hermit log matches first run after normalizing "
                "wallclock timestamps, host inode numbers, and env-dependent "
                "guest addresses"
            )
    else:
        passed = False
        report.append(
            "WARN: Hermit INFO logs not compared because the first-run or current "
            "log is unavailable; canonical repeat verification requires both logs"
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
        ["make", "--no-print-directory", "-s", "check-demo-deps"],
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


# safehermit's exit codes for the two bounds it enforces lethally. The wrapper
# documents these; a caller that only prints the number teaches nobody what happened.
SAFEHERMIT_EXIT_REASON = {
    124: "the WALL DEADLINE fired: safehermit killed the run through its cgroup",
    125: "the LOG BYTE CAP fired: safehermit killed the run through its cgroup",
}


def report_safehermit(report: Path, return_code: int) -> None:
    """Surface the wrapper's verdict, because --sh-report puts it in a file.

    Moving safehermit's report off stderr is what keeps its per-run `run_id` out of
    the hermit log these demos hash and line-diff. The cost is that the report is no
    longer in front of anyone, so the caller has to take over the job of showing it,
    and this is that job.

    TWO THINGS ARE PRINTED, FOR TWO DIFFERENT REASONS.

    Any bound the wrapper could NOT apply is printed on EVERY run, pass or fail. That
    is safehermit's own stated discipline -- "on the day of the incident several
    mechanisms were present and inert, and a wrapper that only speaks up when
    something breaks becomes another one of them" -- and redirecting the report would
    have quietly reintroduced exactly that.

    On a failure the exit code is TRANSLATED. Measured on a review run with the cap
    set to 1 MiB: demo 5 reported `FAILURE: Hermit/QEMU exited with status 125` and
    demo 6 reported `FAILURE: [Errno 2] No such file or directory: .../serial.log`.
    Neither says a cap fired. The second is worse than silence -- it sends the reader
    to look at the serial log, which is missing only because the run was killed before
    it existed.
    """
    lines = []
    try:
        lines = Path(report).read_text(errors="replace").splitlines()
    except OSError as error:
        # A missing report is itself worth saying: it means the run may not have been
        # bounded at all, which is the one thing this must never pass over in silence.
        print(
            "WARN: safehermit report unreadable ({}): cannot confirm which bounds "
            "were applied to this run".format(error),
            file=sys.stderr,
        )
        return

    unapplied = [l for l in lines if "=NOT_APPLIED:" in l]
    if unapplied:
        print("Bounds safehermit could NOT apply to this run:", file=sys.stderr)
        for line in unapplied:
            print("  {}".format(line), file=sys.stderr)

    if return_code != 0:
        reason = SAFEHERMIT_EXIT_REASON.get(return_code)
        if reason is not None:
            print("safehermit: exit {} means {}".format(return_code, reason),
                  file=sys.stderr)
        # The wrapper's own one-line explanations, which name the cap size or the
        # deadline that fired.
        for line in lines:
            if line.startswith("safehermit: LOG CAP") or line.startswith(
                "safehermit: DEADLINE"
            ):
                print("  {}".format(line), file=sys.stderr)
        print("  full wrapper report: {}".format(report), file=sys.stderr)


def wait_for_process(
    process: subprocess.Popen,
    timeout: float,
    stream_path: Optional[Path] = None,
    progress_label: Optional[str] = None,
    first_output_label: Optional[str] = None,
) -> int:
    """Wait for a process, optionally streaming a growing file or showing progress.

    When ``first_output_label`` is set (used with ``stream_path``), a live
    seconds-counter ticks until the very first byte of streamed output appears,
    then freezes as ``(N.Ns to first output)``. For the QEMU boot demo this makes
    the healthy ~10-20s time-to-first-serial-line obvious at a glance and turns a
    wedged boot (counter climbing toward the timeout with no output) into an
    immediately visible symptom.
    """
    deadline = time.monotonic() + timeout
    started = time.monotonic()
    stream = None
    last_progress = -1
    last_wait_tick = -1.0
    first_output_at: Optional[float] = None

    def note_first_output() -> None:
        nonlocal first_output_at
        if first_output_label is not None and first_output_at is None:
            first_output_at = time.monotonic() - started
            # Freeze the ticking counter on its own line, then let the streamed
            # output follow on the next line.
            print(
                "\r{}: {:.1f}s  ({:.1f}s to first output)".format(
                    first_output_label, first_output_at, first_output_at
                ),
                flush=True,
            )
            # Stable, greppable marker so timing harnesses can recover the
            # frozen time-to-first-output from a log without parsing the
            # human-facing counter line (which carries a leading '\r').
            print(
                "FIRST_OUTPUT_ELAPSED={:.1f}s".format(first_output_at),
                flush=True,
            )

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
                    note_first_output()
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()

            return_code = process.poll()
            if return_code is not None:
                if stream is not None:
                    chunk = stream.read()
                    if chunk:
                        note_first_output()
                        sys.stdout.buffer.write(chunk)
                        sys.stdout.buffer.flush()
                if progress_label is not None:
                    done_elapsed = time.monotonic() - started
                    print(
                        "\r{}: done ({:.1f}s)".format(
                            progress_label, done_elapsed
                        )
                    )
                    # Stable, greppable end-of-timer marker.
                    print(
                        "TIMER_DONE label={} elapsed={:.1f}s".format(
                            progress_label, done_elapsed
                        ),
                        flush=True,
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
            if first_output_label is not None and first_output_at is None:
                waited = now - started
                if waited - last_wait_tick >= 0.1:
                    print(
                        "\r{}: {:.1f}s".format(first_output_label, waited),
                        end="",
                        flush=True,
                    )
                    last_wait_tick = waited
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
    """Stop a launched child, and its descendants when it leads its own group.

    SIGNALLING ONE PID IS NOT ENOUGH FOR A HERMIT RUN. hermit supervises a ptraced
    tree and the guest runs as a PID-namespace init, where the kernel discards
    SIGTERM. Measured on demo 6 with QEMU_TIMEOUT=25: two hermit processes existed
    during the run, this function killed the one it was handed, and the second
    survived with ppid=1 -- still holding hermit-info.log and growing it from 704 MB
    to 830 MB in ten seconds, about 45 GB/h. Three such orphans tripped the disk
    headroom alarm and left 38 GiB behind on 2026-08-20.

    The survivor was in the CHILD's process group, so killing that group reaches it.
    That is only safe when the child leads a group of its own -- otherwise the group
    is ours and we would kill the demo. Callers that launch long-running children
    pass `start_new_session=True` to make that true; `drgn_hermit.py` has done this
    since demo 7 was written and this brings the rest of the demos in line with it.
    """
    if process is None or process.poll() is not None:
        return
    group: Optional[int] = None
    try:
        if os.getpgid(process.pid) == process.pid:
            group = process.pid
    except (OSError, ProcessLookupError):
        group = None

    def signal_all(sig: int) -> None:
        if group is not None:
            try:
                os.killpg(group, sig)
                return
            except (ProcessLookupError, PermissionError):
                pass
        try:
            process.send_signal(sig)
        except (ProcessLookupError, OSError):
            pass

    signal_all(signal.SIGTERM)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    signal_all(signal.SIGKILL)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass


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
