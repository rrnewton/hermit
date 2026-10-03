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

# The repeat check compares Hermit's INFO log under Hermit's own canonical
# policy, BitwiseInfoV1 (detcore/src/logdiff.rs), and normalizes nothing else:
#
# 1. WALLCLOCK_RE removes the real wall-clock timestamp that starts each
#    tracing line (STRIP_WALL_CLOCK_PREFIX_V1).
# 2. HOST_ADDR_RE matches the one marker Hermit puts around a host-side address
#    it prints, `<hostaddr 0x...>` (`host_addr` in logdiff.rs). Each marked value
#    becomes its first-appearance ordinal within its own log, exactly as
#    `canonicalize_addresses_in_line` does (CANON_ADDRESS_ORDINAL_V1), so which
#    marked values repeat, and in what order they first appear, still has to
#    match.
#
# Every other byte must match, including the bare `0x7f...` guest addresses that
# DETLOG prints for syscall pointer arguments. The two logs come from separate
# `hermit run` invocations with identical inputs: the launchers give the guest
# Hermit's minimal fixed environment (guest_environment_args), fixed guest-side
# paths for the controller, the assets and the run directory, a fixed working
# directory and a fixed --epoch, and the ptrace backend turns off address-space
# randomization in the guest. Guest addresses are then reproducible, and the
# retained logs agree: two demo 5 boot pairs (2026-09-30 and 2026-10-01;
# 2,332,583 lines and 817,167 bare 0x7f... values per log) and one demo 6 resume
# pair (2026-10-01; 1,604,193 lines, 320,244 values) differed in no line once the
# wall-clock prefix was removed. A guest address that still differs means the
# inputs differed or the execution diverged, so the repeat fails.
#
# Logs also carry a file's resource id, `FileContents(DetInode(N))`. Hermit
# derives N deterministically (determinize_inode in detcore/src/tool_global.rs),
# so it too is compared exactly.
HOST_ADDR_RE = re.compile(r"<hostaddr (0[xX][A-Fa-f0-9]+)>")
# Schema 2 made qemu_binary_sha256 required. Schema 3 adds guest_exit_status,
# the guest command's exit status, to qemu-resume rows and requires it there.
RUN_METADATA_SCHEMA_VERSION = 3
SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS = (1, 2, RUN_METADATA_SCHEMA_VERSION)


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
    guest_exit_status: int
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
    guest_exit_status: Optional[int]
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
        "guest_exit_status",
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


def _metadata_optional_exit_status(value: Mapping[str, Any]) -> Optional[int]:
    if "guest_exit_status" not in value:
        return None
    raw = value.get("guest_exit_status")
    if not isinstance(raw, int) or isinstance(raw, bool) or not 0 <= raw <= 255:
        raise _metadata_error(
            "guest_exit_status", "must be an integer from 0 to 255"
        )
    return raw


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
        or schema_version not in SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS
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
    guest_exit_status = _metadata_optional_exit_status(value)
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
        if schema_version >= 3 and guest_exit_status is None:
            raise _metadata_error(
                "guest_exit_status",
                "is required for qemu-resume from schema 3 on",
            )
        if schema_version < 3 and guest_exit_status is not None:
            raise _metadata_error(
                "guest_exit_status",
                "is not part of schema {}".format(schema_version),
            )
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
        guest_exit_status=guest_exit_status,
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
    return Path("/var/tmp") / "hermit-demo-qemu-{}-{}".format(
        os.getuid(), digest
    )


def hermit_tmp_args(root: Path) -> List[str]:
    """Keep checkout-local QEMU controller/runtime paths visible to a traced guest."""
    return ["--tmp=/tmp"] if _under_host_tmp(root) else []


# The files the guest's QEMU controller runs: the script and the one module it
# imports. See stage_guest_controller.
GUEST_CONTROLLER_SOURCES = ("qemu_controller.py", "demo_common.py")
# The modification time given to the staged copies and their directory:
# 2026-01-01T00:00:00Z, the fixed epoch the QEMU demos pass to Hermit.
GUEST_CONTROLLER_MTIME = 1767225600


def stage_guest_controller(destination: Path) -> Path:
    """Copy the controller's sources into a new directory for the guest to run.

    The guest's Python used to import them straight from demos/lib. It then
    also read the bytecode cache that the host's Python leaves in
    demos/lib/__pycache__, and a cached ``.pyc`` records the absolute path of
    its source file: from a checkout whose path was 6 characters shorter, the
    guest read a ``demo_common`` cache 6 bytes shorter, and the boot ended at a
    different virtual time with a different Hermit log. Anything else in
    demos/lib (an editor's swap file, a stale cache) would also change the
    directory listing the guest reads.

    The new directory holds only the two sources, with fixed contents, file
    modes, and modification times, and no cache. The guest runs with
    PYTHONDONTWRITEBYTECODE set, so it compiles demo_common from source and
    writes nothing back. Returns ``destination``.
    """
    destination = Path(destination)
    destination.mkdir(mode=0o755)
    os.chmod(destination, 0o755)
    library = Path(__file__).resolve().parent
    for name in GUEST_CONTROLLER_SOURCES:
        staged = destination / name
        shutil.copyfile(library / name, staged)
        os.chmod(staged, 0o644)
        os.utime(staged, (GUEST_CONTROLLER_MTIME, GUEST_CONTROLLER_MTIME))
    os.utime(destination, (GUEST_CONTROLLER_MTIME, GUEST_CONTROLLER_MTIME))
    return destination


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
        "hermit_version": _tool_version([hermit_binary(), "--version"]),
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


# Linux's sockaddr_un.sun_path is 108 bytes INCLUDING the NUL terminator, so a
# bind or connect fails once the pathname reaches 108. This is a kernel ABI
# constant, not a tunable.
AF_UNIX_PATH_MAX = 107


def make_socket_path(preferred: Path, prefix: str) -> Path:
    """Return a socket pathname for ``preferred``, relocating only if it must.

    Linux rejects a Unix-domain socket path at or over AF_UNIX_PATH_MAX. The
    natural home for these sockets is the per-run working directory, alongside
    the run's other files, and from most checkouts that fits. A deeply nested
    checkout can exceed the bound, and QEMU then refuses to create the socket.

    Relocation is the fallback, not the default. QEMU runs under Hermit, so a
    socket outside the checkout is not necessarily visible to both the guest
    that creates it and the controller that connects to it. Keep the working
    path wherever it fits, and move only when the bound forces it.
    """
    preferred = Path(preferred)
    if len(str(preferred).encode()) <= AF_UNIX_PATH_MAX:
        return preferred

    # Do not use host /tmp here: Hermit normally mounts a private
    # tmpfs over /tmp, which would hide an outer controller's socket from QEMU
    # running under Hermit.  /var/tmp is deliberately also used by
    # default_qemu_assets for this namespace-visibility reason.
    base = Path(os.environ.get("QEMU_SOCKET_DIR", "/var/tmp"))
    if _under_host_tmp(base):
        raise RuntimeError(
            "socket path {} is {} bytes, over the {}-byte AF_UNIX limit, but "
            "QEMU_SOCKET_DIR={} is under host /tmp, which Hermit normally hides "
            "with a private tmpfs. Choose a short host-visible directory outside "
            "/tmp, or set QEMU_ASSETS to a short path so relocation is "
            "unnecessary.".format(
                preferred, len(str(preferred).encode()), AF_UNIX_PATH_MAX, base
            )
        )
    root = base / "hermit-qmp-{}".format(os.getuid())
    try:
        root.mkdir(parents=True, mode=0o700, exist_ok=True)
    except OSError as error:
        raise RuntimeError(
            "socket path {} is {} bytes, over the {}-byte AF_UNIX limit, and the "
            "host-visible fallback root {} is unusable: {}. Set QEMU_SOCKET_DIR "
            "to a short writable directory outside /tmp, or set QEMU_ASSETS to "
            "a short path so relocation is unnecessary.".format(
                preferred, len(str(preferred).encode()), AF_UNIX_PATH_MAX, root, error
            )
        ) from error

    # Hash the complete preferred path.  Demo 5's preferred path includes its
    # unique run directory, so concurrent runs remain isolated.  Demo 6's
    # preferred path is stable and protected by its existing lock, so repeats
    # reuse the same spelling.  Unlike mkdtemp this mapping is deterministic:
    # callers can canonicalize the one known relocated socket without masking
    # any other QEMU argument.
    identity = hashlib.sha256(os.fsencode(str(preferred.resolve()))).hexdigest()[:16]
    path = root / "{}-{}.sock".format(prefix, identity)
    length = len(str(path).encode())
    if length > AF_UNIX_PATH_MAX:
        raise RuntimeError(
            "socket path is {} bytes even after relocating under {}, over the "
            "{}-byte AF_UNIX limit: {}. Set QEMU_SOCKET_DIR to a shorter "
            "host-visible directory outside /tmp.".format(
                length, root, AF_UNIX_PATH_MAX, path
            )
        )
    return path


def canonicalize_qemu_runtime_path(text: str, run_dir: Path, qmp_socket: Path) -> str:
    """Fold only the private runtime paths whose spelling varies between runs."""
    run_dir = Path(run_dir)
    qmp_socket = Path(qmp_socket)
    normalized = text.replace(str(run_dir), "<run-dir>")
    try:
        qmp_socket.relative_to(run_dir)
    except ValueError:
        normalized = normalized.replace(str(qmp_socket), "<qmp-socket>")
    return normalized


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


class _HostAddressOrdinals:
    """Number each Hermit-marked host address by first appearance in one log.

    This is the canonical policy's only address rule (CANON_ADDRESS_ORDINAL_V1,
    `canonicalize_addresses_in_line` in detcore/src/logdiff.rs): a value inside
    `<hostaddr ...>` becomes `<addrN>`, where N counts distinct marked values in
    order of first appearance. Use one instance per log, never shared between
    the two logs being compared. Which marked values repeat, and their order,
    must still agree. Bare hex values, including every guest address, are not
    touched.
    """

    def __init__(self) -> None:
        self._seen: Dict[str, int] = {}

    def substitute(self, line: str) -> str:
        def _ordinal(match: "re.Match[str]") -> str:
            raw = match.group(1)
            ordinal = self._seen.get(raw)
            if ordinal is None:
                ordinal = len(self._seen) + 1
                self._seen[raw] = ordinal
            return "<addr{}>".format(ordinal)

        return HOST_ADDR_RE.sub(_ordinal, line)


def _canonical_log_line(line: str, host_addresses: _HostAddressOrdinals) -> str:
    """Apply the canonical policy to one log line; see HOST_ADDR_RE.

    Only the leading wall-clock timestamp is removed and only Hermit-marked host
    addresses are renumbered. Anything else that differs is a real divergence.
    """
    return host_addresses.substitute(_strip_wallclock_prefix(line))


def hermit_log_diff(log1: Path, log2: Path) -> str:
    """Return the first log divergence under the canonical policy, or ""."""
    before: List[Tuple[int, str, str]] = []
    with Path(log1).open(errors="replace") as left, Path(log2).open(
        errors="replace"
    ) as right:
        line_number = 0
        left_addresses = _HostAddressOrdinals()
        right_addresses = _HostAddressOrdinals()
        while True:
            left_line = left.readline()
            right_line = right.readline()
            if not left_line and not right_line:
                return ""
            line_number += 1
            canonical_left = _canonical_log_line(left_line, left_addresses)
            canonical_right = _canonical_log_line(right_line, right_addresses)
            if canonical_left != canonical_right:
                context = ["  {!r}".format(item[1]) for item in before]
                context.extend(
                    (
                        "- {!r}".format(canonical_left),
                        "+ {!r}".format(canonical_right),
                    )
                )
                return (
                    "first divergence at line {} (only the wall-clock prefix "
                    "removed and Hermit-marked host addresses numbered):\n{}"
                ).format(line_number, "\n".join(context))
            before.append((line_number, canonical_left, canonical_right))
            before = before[-3:]


def compare_runs(
    anchor: QemuRunMetadata, current: QemuRunMetadata
) -> Tuple[bool, List[str]]:
    """Compare exact artifacts and the INFO logs under the canonical policy."""
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
        (
            anchor.guest_exit_status,
            current.guest_exit_status,
            "guest command exit status",
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

    # Compare the logs under the canonical policy only (see HOST_ADDR_RE). Any
    # remaining INFO difference, a guest address included, is execution
    # evidence and must fail the repeat, even when the VM artifacts happen to be
    # byte-identical. In particular, a difference that begins during Python
    # startup can propagate into virtual clock values and the QEMU execution;
    # its origin does not make the later guest-visible log evidence optional.
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
                "WARN: Hermit INFO log differs from first run with only the "
                "wall-clock prefix removed and Hermit-marked host addresses "
                "numbered; canonical repeat verification failed\n{}".format(
                    difference
                )
            )
        else:
            report.append(
                "PASS: Hermit INFO log matches first run exactly apart from the "
                "wall-clock prefix (Hermit-marked host addresses compared by "
                "first appearance)"
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
        print("DETERMINISTIC: snapshot SHA-256 matches the previous run:")
        print("   {}".format(snapshot_sha256))
        print("   {} is bitwise-reproducible under Hermit.".format(subject))


HERMIT_NOT_ON_PATH = (
    "hermit is not on PATH. Build it from this checkout with `make release-core` "
    "and run `export PATH=\"$PWD/target/release:$PATH\"` (see demos/README.md)."
)


def hermit_binary() -> str:
    """Return the `hermit` found on PATH, which is the binary every demo runs."""
    found = shutil.which("hermit")
    if found is None:
        raise RuntimeError(HERMIT_NOT_ON_PATH)
    return found


def check_dependencies(root: Path) -> str:
    """Confirm that a working `hermit` is on PATH and return a summary line.

    ``root`` is the checkout the demo belongs to; it is accepted for symmetry with
    check_qemu_dependencies and is not otherwise needed.
    """
    del root
    hermit = hermit_binary()
    result = subprocess.run(
        [hermit, "--version"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.stderr.write(result.stdout)
        raise subprocess.CalledProcessError(result.returncode, result.args)
    lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    if not lines or not lines[0].startswith("hermit "):
        raise RuntimeError(
            "unexpected `hermit --version` output: {!r}".format(result.stdout)
        )
    return "Dependency check passed: {} ({})".format(lines[0], hermit)


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
    first_output_label: Optional[str] = None,
    log_path: Optional[Path] = None,
    max_log_bytes: Optional[int] = None,
) -> int:
    """Wait for a process, optionally streaming a growing file or showing progress.

    ``timeout`` bounds the wall time. When ``log_path`` and ``max_log_bytes`` are
    both set, the file is also watched: if it grows past the cap, the process
    group is stopped and RuntimeError names the cap, so a runaway log cannot fill
    the disk. The caller is expected to have started the process with
    ``start_new_session=True``.

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

            if log_path is not None and max_log_bytes is not None:
                try:
                    log_size = Path(log_path).stat().st_size
                except FileNotFoundError:
                    log_size = 0
                if log_size > max_log_bytes:
                    stop_process(process)
                    raise RuntimeError(
                        "{} grew to {} bytes, past the {}-byte log cap; the run "
                        "was stopped".format(log_path, log_size, max_log_bytes)
                    )

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

    Signalling one PID is not enough for a Hermit run: Hermit supervises a traced
    process tree, and a surviving Hermit process can keep writing its log after
    the one we were handed has exited. Killing the child's process group reaches
    the whole tree. That is only safe when the child leads a group of its own
    (otherwise the group is ours and we would kill the demo), so callers launch
    long-running children with ``start_new_session=True``.
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
