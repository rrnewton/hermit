"""Import a Codex coordinator lineage from append-only rollout JSONL files."""

from __future__ import annotations

from collections import Counter, defaultdict
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
import errno
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import stat
from urllib.parse import quote
from zoneinfo import ZoneInfo

from wrkviz.model import (
    Agent,
    Edge,
    Event,
    SourceSnapshot,
    TeamData,
    ToolCall,
    Turn,
)


_NESTED_TOOL_RE = re.compile(r"\btools\.([A-Za-z_][A-Za-z0-9_]*)\s*\(")
_CLASSIFICATION_VERSION = "authorship-v1"


class CodexParseError(ValueError):
    """A selected Codex rollout is malformed or the requested lineage is absent."""


@dataclass(frozen=True)
class CodexSourceCopy:
    """One append-only rollout copy and its versioned provenance record."""

    source_path: str
    original_path: str
    snapshot_path: str
    thread_id: str
    copied_bytes: int
    line_count: int
    sha256: str
    updated_at: str

    def to_json_obj(self) -> dict[str, object]:
        """Return the copied-source record as a JSON-serializable object."""

        return {
            "source_path": self.source_path,
            "original_path": self.original_path,
            "snapshot_path": self.snapshot_path,
            "thread_id": self.thread_id,
            "copied_bytes": self.copied_bytes,
            "line_count": self.line_count,
            "sha256": self.sha256,
            "updated_at": self.updated_at,
        }

    @classmethod
    def from_json_obj(cls, raw: Mapping[str, object], where: str) -> CodexSourceCopy:
        """Parse and validate a copied-source record from a JSON object."""

        source_path = _string(raw.get("source_path"))
        original_path = _string(raw.get("original_path"))
        snapshot_path = _string(raw.get("snapshot_path"))
        thread_id = _string(raw.get("thread_id"))
        copied_bytes = _integer(raw.get("copied_bytes"))
        line_count = _integer(raw.get("line_count"))
        sha256 = _string(raw.get("sha256"))
        updated_at = _string(raw.get("updated_at"))
        if (
            source_path is None
            or original_path is None
            or snapshot_path is None
            or thread_id is None
            or copied_bytes is None
            or copied_bytes < 0
            or line_count is None
            or line_count < 0
            or sha256 is None
            or re.fullmatch(r"[0-9a-f]{64}", sha256) is None
            or updated_at is None
        ):
            raise CodexParseError(f"{where}: malformed source-copy record")
        _safe_snapshot_relative(source_path)
        if snapshot_path != source_path:
            raise CodexParseError(
                f"{where}: snapshot_path must match the safe relative source_path"
            )
        return cls(
            source_path,
            original_path,
            snapshot_path,
            thread_id,
            copied_bytes,
            line_count,
            sha256,
            updated_at,
        )


@dataclass(frozen=True)
class CodexContinuationLink:
    """One explicit, evidence-backed transition between coordinator sessions."""

    predecessor_thread_id: str
    thread_id: str
    predecessor_source_path: str
    predecessor_source_line: int
    predecessor_at_ms: int
    source_path: str
    started_at_ms: int
    gap_ms: int

    def to_json_obj(self) -> dict[str, object]:
        """Return the exact durable continuation-boundary record."""

        return {
            "predecessor_thread_id": self.predecessor_thread_id,
            "thread_id": self.thread_id,
            "predecessor_source_path": self.predecessor_source_path,
            "predecessor_source_line": self.predecessor_source_line,
            "predecessor_at_ms": self.predecessor_at_ms,
            "source_path": self.source_path,
            "started_at_ms": self.started_at_ms,
            "gap_ms": self.gap_ms,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> CodexContinuationLink:
        """Parse one exact continuation record from a source manifest."""

        expected = {
            "predecessor_thread_id",
            "thread_id",
            "predecessor_source_path",
            "predecessor_source_line",
            "predecessor_at_ms",
            "source_path",
            "started_at_ms",
            "gap_ms",
        }
        if set(raw) != expected:
            missing = sorted(expected - set(raw))
            unknown = sorted(set(raw) - expected)
            raise CodexParseError(
                f"{where}: invalid continuation fields; "
                f"missing={missing!r}, unknown={unknown!r}"
            )
        predecessor_thread_id = _string(raw.get("predecessor_thread_id"))
        thread_id = _string(raw.get("thread_id"))
        predecessor_source_path = _string(raw.get("predecessor_source_path"))
        predecessor_source_line = _integer(raw.get("predecessor_source_line"))
        predecessor_at_ms = _integer(raw.get("predecessor_at_ms"))
        source_path = _string(raw.get("source_path"))
        started_at_ms = _integer(raw.get("started_at_ms"))
        gap_ms = _integer(raw.get("gap_ms"))
        if (
            predecessor_thread_id is None
            or not predecessor_thread_id
            or thread_id is None
            or not thread_id
            or predecessor_thread_id == thread_id
            or predecessor_source_path is None
            or source_path is None
            or predecessor_source_line is None
            or predecessor_source_line < 1
            or predecessor_at_ms is None
            or predecessor_at_ms < 0
            or started_at_ms is None
            or started_at_ms < 0
            or gap_ms is None
            or gap_ms <= 0
            or gap_ms != started_at_ms - predecessor_at_ms
        ):
            raise CodexParseError(f"{where}: malformed continuation record")
        _safe_snapshot_relative(predecessor_source_path)
        _safe_snapshot_relative(source_path)
        return cls(
            predecessor_thread_id,
            thread_id,
            predecessor_source_path,
            predecessor_source_line,
            predecessor_at_ms,
            source_path,
            started_at_ms,
            gap_ms,
        )


@dataclass(frozen=True)
class CodexSnapshotResult:
    """Result of copying a lineage into an archive-local source directory."""

    sources: tuple[CodexSourceCopy, ...]
    files_changed: int
    continuations: tuple[CodexContinuationLink, ...] = ()


@dataclass(frozen=True)
class _Record:
    line: int
    raw: bytes
    value: Mapping[str, object]


@dataclass(frozen=True)
class _Rollout:
    path: Path
    source_path: str
    snapshot: SourceSnapshot
    metadata: Mapping[str, object]
    records: tuple[_Record, ...]
    canonical_records: tuple[_Record, ...]
    lineage_root_id: str


@dataclass(frozen=True)
class _AgentSeed:
    thread_id: str
    parent_thread_id: str | None
    agent_path: str
    nickname: str | None
    role: str | None
    depth: int
    started_at_ms: int
    source_path: str
    lineage_root_id: str
    raw_agent_path: str


@dataclass
class _TurnBuilder:
    turn_id: str
    thread_id: str
    started_at_ms: int
    ended_at_ms: int | None = None
    status: str = "running"
    first_token_ms: int | None = None
    error: str | None = None
    last_agent_message: str | None = None


@dataclass
class _ToolBuilder:
    call_id: str
    item_id: str | None
    thread_id: str
    turn_id: str | None
    name: str
    namespace: str | None
    started_at_ms: int
    ended_at_ms: int | None
    status: str
    input_text: str | None
    output_text: str | None
    nested_tools: tuple[tuple[str, int], ...]
    source_line: int
    lineage_root_id: str


@dataclass(frozen=True)
class _Activity:
    owner_thread_id: str
    timestamp_ms: int
    payload: Mapping[str, object]
    source_line: int
    lineage_root_id: str


@dataclass(frozen=True)
class _PendingCopy:
    source: CodexSourceCopy
    complete: bytes
    baseline: bytes | None


def _mapping(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        return {}
    return {str(key): item for key, item in value.items()}


def _string(value: object) -> str | None:
    return value if isinstance(value, str) else None


def _integer(value: object) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value


def _textual(value: object) -> str | None:
    if value is None:
        return None
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, sort_keys=True, separators=(",", ":"))
    except (TypeError, ValueError):
        return str(value)


def _parse_json_object(raw: bytes, where: str) -> dict[str, object]:
    try:
        value: object = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        raise CodexParseError(f"{where}: invalid JSON: {exc}") from exc
    result = _mapping(value)
    if not result:
        raise CodexParseError(f"{where}: expected a non-empty JSON object")
    return result


def _iso_ms(value: object) -> int:
    text = _string(value)
    if text is None:
        return 0
    normalized = text[:-1] + "+00:00" if text.endswith("Z") else text
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return 0
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return int(parsed.timestamp() * 1000)


def _numeric_epoch_ms(value: object, fallback_ms: int) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return fallback_ms
    numeric = float(value)
    if numeric > 10_000_000_000:
        return int(numeric)
    return int(numeric * 1000)


def _record_payload(record: _Record) -> dict[str, object]:
    return _mapping(record.value.get("payload"))


def _record_timestamp_ms(record: _Record) -> int:
    return _iso_ms(record.value.get("timestamp"))


def _fallback_id(thread_id: str, record: _Record, kind: str) -> str:
    digest = hashlib.sha256()
    digest.update(thread_id.encode("utf-8"))
    digest.update(b"\0")
    digest.update(kind.encode("utf-8"))
    digest.update(b"\0")
    digest.update(str(record.line).encode("ascii"))
    digest.update(b"\0")
    digest.update(record.raw)
    return f"evt_{digest.hexdigest()[:24]}"


def _relative_path(path: Path, sessions_root: Path) -> str:
    try:
        return path.relative_to(sessions_root).as_posix()
    except ValueError:
        return str(path)


def _safe_snapshot_relative(source_path: str) -> PurePosixPath:
    relative = PurePosixPath(source_path)
    if (
        relative.is_absolute()
        or not relative.parts
        or any(part in ("", ".", "..") for part in relative.parts)
    ):
        raise CodexParseError(f"unsafe source snapshot path {source_path!r}")
    return relative


def _snapshot_target(snapshot_root: Path, source_path: str) -> Path:
    relative = _safe_snapshot_relative(source_path)
    return snapshot_root.joinpath(*relative.parts)


def _complete_prefix(data: bytes) -> bytes:
    if data.endswith(b"\n"):
        return data
    newline = data.rfind(b"\n")
    return data[: newline + 1] if newline >= 0 else b""


def _open_directory_no_symlinks(path: Path, *, create: bool) -> int | None:
    """Open an absolute directory path component-by-component without following symlinks."""

    absolute = path.absolute()
    parts = absolute.parts
    if not parts:
        raise CodexParseError(f"invalid empty snapshot directory path: {path}")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW
    try:
        current = os.open(parts[0], flags)
    except OSError as exc:
        raise CodexParseError(f"cannot open snapshot path root {parts[0]}: {exc}") from exc
    try:
        for part in parts[1:]:
            if create:
                try:
                    os.mkdir(part, mode=0o700, dir_fd=current)
                except FileExistsError:
                    pass
                except OSError as exc:
                    raise CodexParseError(
                        f"cannot create snapshot directory component {part!r}: {exc}"
                    ) from exc
            try:
                following = os.open(part, flags, dir_fd=current)
            except FileNotFoundError:
                if not create:
                    os.close(current)
                    current = -1
                    return None
                raise
            except OSError as exc:
                message = (
                    "symlink or non-directory"
                    if exc.errno in (errno.ELOOP, errno.ENOTDIR)
                    else "invalid"
                )
                raise CodexParseError(
                    f"{message} snapshot directory component {part!r} in {absolute}: {exc}"
                ) from exc
            os.close(current)
            current = following
        result = current
        current = -1
        return result
    finally:
        if current >= 0:
            os.close(current)


def _open_snapshot_file(
    snapshot_root: Path, source_path: str
) -> tuple[int, os.stat_result] | None:
    relative = _safe_snapshot_relative(source_path)
    parent = snapshot_root.joinpath(*relative.parts[:-1])
    parent_fd = _open_directory_no_symlinks(parent, create=False)
    if parent_fd is None:
        return None
    name = relative.parts[-1]
    try:
        try:
            metadata = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            return None
        if stat.S_ISLNK(metadata.st_mode):
            raise CodexParseError(
                f"source snapshot target must not be a symlink: {snapshot_root / relative}"
            )
        if not stat.S_ISREG(metadata.st_mode):
            raise CodexParseError(
                f"source snapshot target is not a regular file: {snapshot_root / relative}"
            )
        try:
            file_fd = os.open(
                name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=parent_fd
            )
        except OSError as exc:
            raise CodexParseError(
                f"cannot safely open source snapshot {snapshot_root / relative}: {exc}"
            ) from exc
        opened = os.fstat(file_fd)
        if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
            os.close(file_fd)
            raise CodexParseError(
                f"source snapshot changed while opening: {snapshot_root / relative}"
            )
        return file_fd, opened
    finally:
        os.close(parent_fd)


def _read_snapshot_file(
    snapshot_root: Path, source_path: str
) -> tuple[bytes, os.stat_result] | None:
    opened = _open_snapshot_file(snapshot_root, source_path)
    if opened is None:
        return None
    file_fd, metadata = opened
    with os.fdopen(file_fd, "rb") as handle:
        return handle.read(), metadata


def _first_snapshot_metadata(
    snapshot_root: Path, source_path: str
) -> dict[str, object] | None:
    opened = _open_snapshot_file(snapshot_root, source_path)
    if opened is None:
        return None
    file_fd, _ = opened
    with os.fdopen(file_fd, "rb") as handle:
        line = handle.readline()
    if not line:
        return None
    try:
        value: object = json.loads(line)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    record = _mapping(value)
    if record.get("type") != "session_meta":
        return None
    return _mapping(record.get("payload"))


def _lineage_roots(
    root_thread_id: str, continuation_thread_ids: Sequence[str]
) -> tuple[str, ...]:
    roots = (root_thread_id, *continuation_thread_ids)
    if any(not item for item in roots):
        raise CodexParseError("Codex root and continuation session IDs must not be empty")
    if len(set(roots)) != len(roots):
        raise CodexParseError("Codex root and continuation session IDs must be unique")
    return roots


def _metadata_lineage_root(
    metadata: Mapping[str, object], lineage_roots: Sequence[str], where: str
) -> str:
    thread_id = _string(metadata.get("id"))
    session_id = _string(metadata.get("session_id"))
    matches = [
        root
        for root in lineage_roots
        if thread_id == root or session_id == root
    ]
    if len(matches) != 1:
        raise CodexParseError(
            f"{where}: source belongs to {len(matches)} configured Codex lineages"
        )
    return matches[0]


def codex_identity_metadata(
    snapshot_root: Path,
    source_paths: Sequence[str],
    root_thread_id: str,
    continuation_thread_ids: Sequence[str] = (),
) -> tuple[Mapping[str, object], ...]:
    """Return structured session metadata, root first, for identity inference.

    The source paths have already been copied into the archive, but this helper still applies the
    snapshot path and metadata validation used by the transcript parser. Free-form transcript text
    is deliberately excluded from identity inference.
    """

    roots = _lineage_roots(root_thread_id, continuation_thread_ids)
    root_order = {thread_id: index for index, thread_id in enumerate(roots)}
    records: list[tuple[Mapping[str, object], str]] = []
    for source_path in source_paths:
        metadata = _first_snapshot_metadata(snapshot_root, source_path)
        if metadata is None:
            raise CodexParseError(
                f"source snapshot lacks valid session metadata: {source_path!r}"
            )
        records.append(
            (
                metadata,
                _metadata_lineage_root(metadata, roots, source_path),
            )
        )
    return tuple(
        item[0]
        for item in sorted(
            records,
            key=lambda item: (
                root_order[item[1]],
                0 if _string(item[0].get("id")) == item[1] else 1,
                _string(item[0].get("id")) or "",
            ),
        )
    )


def _read_at(parent_fd: int, name: str, display_path: Path) -> bytes | None:
    try:
        metadata = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except FileNotFoundError:
        return None
    if stat.S_ISLNK(metadata.st_mode):
        raise CodexParseError(f"source snapshot target must not be a symlink: {display_path}")
    if not stat.S_ISREG(metadata.st_mode):
        raise CodexParseError(f"source snapshot target is not a regular file: {display_path}")
    try:
        file_fd = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=parent_fd)
    except OSError as exc:
        raise CodexParseError(f"cannot safely open source snapshot {display_path}: {exc}") from exc
    with os.fdopen(file_fd, "rb") as handle:
        return handle.read()


def _write_snapshot_file(
    snapshot_root: Path,
    source_path: str,
    data: bytes,
    expected: bytes | None,
) -> bool:
    relative = _safe_snapshot_relative(source_path)
    parent = snapshot_root.joinpath(*relative.parts[:-1])
    parent_fd = _open_directory_no_symlinks(parent, create=True)
    if parent_fd is None:  # create=True always returns a descriptor
        raise CodexParseError(f"cannot create source snapshot parent for {source_path!r}")
    name = relative.parts[-1]
    display_path = snapshot_root / relative
    temporary = f".{name}.{os.getpid()}.{secrets.token_hex(8)}"
    try:
        current = _read_at(parent_fd, name, display_path)
        if current != expected:
            raise CodexParseError(
                f"source snapshot changed after validation: {display_path}"
            )
        if current == data:
            return False
        temp_fd = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW,
            0o600,
            dir_fd=parent_fd,
        )
        try:
            with os.fdopen(temp_fd, "wb") as handle:
                handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(
                temporary,
                name,
                src_dir_fd=parent_fd,
                dst_dir_fd=parent_fd,
            )
            os.fsync(parent_fd)
        finally:
            try:
                os.unlink(temporary, dir_fd=parent_fd)
            except FileNotFoundError:
                pass
        return True
    finally:
        os.close(parent_fd)


def _first_metadata(path: Path) -> dict[str, object] | None:
    try:
        with path.open("rb") as source:
            line = source.readline()
    except OSError:
        return None
    if not line:
        return None
    try:
        value: object = json.loads(line)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    record = _mapping(value)
    if record.get("type") != "session_meta":
        return None
    return _mapping(record.get("payload"))


def _discover_codex_lineage(
    sessions_root: Path,
    root_thread_id: str,
    *,
    exclude_root: Path | None = None,
    continuation_thread_ids: Sequence[str] = (),
) -> tuple[tuple[Path, Mapping[str, object]], ...]:
    roots = frozenset(_lineage_roots(root_thread_id, continuation_thread_ids))
    candidates: list[tuple[Path, Mapping[str, object]]] = []
    excluded = exclude_root.resolve() if exclude_root is not None else None
    try:
        paths = sorted(sessions_root.rglob("rollout-*.jsonl"))
    except OSError as exc:
        raise CodexParseError(f"cannot scan Codex sessions root {sessions_root}: {exc}") from exc
    for path in paths:
        if excluded is not None:
            try:
                path.resolve().relative_to(excluded)
            except ValueError:
                pass
            else:
                continue
        metadata = _first_metadata(path)
        if metadata is None:
            continue
        thread_id = _string(metadata.get("id"))
        session_id = _string(metadata.get("session_id"))
        if thread_id in roots or session_id in roots:
            candidates.append((path, metadata))
    return tuple(candidates)


def _complete_records(complete: bytes, where: str) -> tuple[_Record, ...]:
    records: list[_Record] = []
    for line_number, raw_line in enumerate(complete.splitlines(), start=1):
        records.append(
            _Record(
                line_number,
                raw_line,
                _parse_json_object(raw_line, f"{where}:{line_number}"),
            )
        )
    return tuple(records)


def _continuation_links(
    roots: Sequence[str],
    root_sources: Mapping[str, str],
    complete_by_path: Mapping[str, bytes],
    metadata_by_path: Mapping[str, Mapping[str, object]],
    previous: Sequence[CodexContinuationLink],
) -> tuple[CodexContinuationLink, ...]:
    expected_pairs = tuple(zip(roots, roots[1:]))
    if len(previous) > len(expected_pairs):
        raise CodexParseError(
            "source manifest records more continuations than were configured"
        )
    result: list[CodexContinuationLink] = []
    for index, (predecessor_id, successor_id) in enumerate(expected_pairs):
        predecessor_path = root_sources[predecessor_id]
        successor_path = root_sources[successor_id]
        successor_started_ms = _iso_ms(
            metadata_by_path[successor_path].get("timestamp")
        )
        if successor_started_ms <= 0:
            raise CodexParseError(
                f"continuation root {successor_id!r} lacks a valid start timestamp"
            )
        predecessor_records = _complete_records(
            complete_by_path[predecessor_path], predecessor_path
        )
        if index < len(previous):
            link = previous[index]
            if (
                link.predecessor_thread_id != predecessor_id
                or link.thread_id != successor_id
                or link.predecessor_source_path != predecessor_path
                or link.source_path != successor_path
                or link.started_at_ms != successor_started_ms
                or link.predecessor_source_line > len(predecessor_records)
            ):
                raise CodexParseError(
                    f"recorded continuation {index} no longer matches its configured sessions"
                )
            recorded = predecessor_records[link.predecessor_source_line - 1]
            if _record_timestamp_ms(recorded) != link.predecessor_at_ms:
                raise CodexParseError(
                    f"recorded continuation {index} boundary evidence changed"
                )
            result.append(link)
            continue
        eligible = [
            record
            for record in predecessor_records
            if 0 < _record_timestamp_ms(record) < successor_started_ms
        ]
        if not eligible:
            raise CodexParseError(
                f"continuation {successor_id!r} has no predecessor record before its start"
            )
        boundary = max(
            eligible, key=lambda record: (_record_timestamp_ms(record), record.line)
        )
        predecessor_at_ms = _record_timestamp_ms(boundary)
        result.append(
            CodexContinuationLink(
                predecessor_thread_id=predecessor_id,
                thread_id=successor_id,
                predecessor_source_path=predecessor_path,
                predecessor_source_line=boundary.line,
                predecessor_at_ms=predecessor_at_ms,
                source_path=successor_path,
                started_at_ms=successor_started_ms,
                gap_ms=successor_started_ms - predecessor_at_ms,
            )
        )
    return tuple(result)


def snapshot_codex_lineage(
    sessions_root: Path,
    root_thread_id: str,
    snapshot_root: Path,
    previous_sources: Sequence[CodexSourceCopy],
    updated_at: str,
    continuation_thread_ids: Sequence[str] = (),
    previous_continuations: Sequence[CodexContinuationLink] = (),
) -> CodexSnapshotResult:
    """Copy the newline-complete lineage into *snapshot_root* after monotonic checks.

    Every source is validated before any destination is replaced. A disappeared, shortened, or
    rewritten rollout therefore leaves all prior snapshots untouched. Destinations are replaced
    atomically only when a complete JSONL line has been appended.
    """

    previous_by_path: dict[str, CodexSourceCopy] = {}
    for source in previous_sources:
        if source.source_path in previous_by_path:
            raise CodexParseError(
                f"source manifest contains duplicate path {source.source_path!r}"
            )
        previous_by_path[source.source_path] = source

    roots = _lineage_roots(root_thread_id, continuation_thread_ids)
    candidates = (
        _discover_codex_lineage(
            sessions_root,
            root_thread_id,
            exclude_root=snapshot_root,
            continuation_thread_ids=continuation_thread_ids,
        )
        if continuation_thread_ids
        else _discover_codex_lineage(
            sessions_root, root_thread_id, exclude_root=snapshot_root
        )
    )
    candidate_paths: dict[str, tuple[Path, Mapping[str, object]]] = {}
    lineage_by_thread: dict[str, str] = {}
    root_sources: dict[str, str] = {}
    for path, metadata in candidates:
        try:
            relative = path.relative_to(sessions_root).as_posix()
        except ValueError as exc:
            raise CodexParseError(
                f"rollout {path} is outside sessions root {sessions_root}"
            ) from exc
        _safe_snapshot_relative(relative)
        if relative in candidate_paths:
            raise CodexParseError(f"duplicate Codex source path {relative!r}")
        lineage_root_id = _metadata_lineage_root(metadata, roots, relative)
        thread_id = _string(metadata.get("id"))
        if thread_id is None:
            raise CodexParseError(f"rollout {path} metadata lacks a thread id")
        prior_lineage = lineage_by_thread.get(thread_id)
        if prior_lineage is not None and prior_lineage != lineage_root_id:
            raise CodexParseError(
                f"Codex thread {thread_id!r} occurs in multiple configured lineages"
            )
        lineage_by_thread[thread_id] = lineage_root_id
        if thread_id == lineage_root_id:
            if (
                lineage_root_id != root_thread_id
                and _string(metadata.get("session_id")) != lineage_root_id
            ):
                raise CodexParseError(
                    f"configured continuation {lineage_root_id!r} is not a root session"
                )
            if lineage_root_id in root_sources:
                raise CodexParseError(
                    f"configured Codex root {lineage_root_id!r} has multiple rollouts"
                )
            root_sources[lineage_root_id] = relative
        candidate_paths[relative] = (path, metadata)

    missing = sorted(set(previous_by_path) - set(candidate_paths))
    if missing:
        rendered = ", ".join(repr(path) for path in missing[:3])
        suffix = " ..." if len(missing) > 3 else ""
        raise CodexParseError(
            "append-only source violation: previously observed rollout disappeared: "
            f"{rendered}{suffix}"
        )
    missing_roots = [root for root in roots if root not in root_sources]
    if missing_roots:
        raise CodexParseError(
            "Codex configured root sessions were not found: "
            + ", ".join(repr(root) for root in missing_roots)
        )
    if not candidate_paths:
        raise CodexParseError(f"Codex root thread {root_thread_id!r} was not found")

    pending: list[_PendingCopy] = []
    complete_by_path: dict[str, bytes] = {}
    metadata_by_path: dict[str, Mapping[str, object]] = {}
    for source_path in sorted(candidate_paths):
        path, metadata = candidate_paths[source_path]
        try:
            data = path.read_bytes()
        except OSError as exc:
            raise CodexParseError(f"cannot read rollout {path}: {exc}") from exc
        complete = _complete_prefix(data)
        if not complete:
            raise CodexParseError(f"rollout {path} has no newline-complete metadata record")
        copied_record = _parse_json_object(
            complete.split(b"\n", 1)[0], f"{path}: copied metadata"
        )
        if copied_record.get("type") != "session_meta":
            raise CodexParseError(f"rollout {path} copied prefix lacks session metadata")
        copied_metadata = _mapping(copied_record.get("payload"))
        if copied_metadata != _mapping(metadata):
            raise CodexParseError(
                f"rollout {path} metadata changed between discovery and snapshot read"
            )
        thread_id = _string(copied_metadata.get("id"))
        if thread_id is None:
            raise CodexParseError(f"rollout {path} metadata lacks a thread id")
        _metadata_lineage_root(copied_metadata, roots, source_path)
        complete_by_path[source_path] = complete
        metadata_by_path[source_path] = copied_metadata

        target = _snapshot_target(snapshot_root, source_path)
        previous = previous_by_path.get(source_path)
        opened_snapshot = _read_snapshot_file(snapshot_root, source_path)
        existing = opened_snapshot[0] if opened_snapshot is not None else None
        if previous is not None:
            if previous.thread_id != thread_id:
                raise CodexParseError(
                    "append-only source violation: thread identity changed for "
                    f"{source_path!r}"
                )
            if existing is None:
                raise CodexParseError(
                    f"source snapshot for previously observed rollout is missing: {target}"
                )
            if len(existing) < previous.copied_bytes:
                raise CodexParseError(
                    f"source snapshot is shorter than its manifest record: {target}"
                )
            previous_prefix = existing[: previous.copied_bytes]
            if hashlib.sha256(previous_prefix).hexdigest() != previous.sha256:
                raise CodexParseError(
                    f"source snapshot does not match its manifest record: {target}"
                )

        baseline = existing or b""
        if len(complete) < len(baseline):
            raise CodexParseError(
                "append-only source violation: newline-complete prefix shrank for "
                f"{source_path!r} ({len(baseline)} -> {len(complete)} bytes)"
            )
        if complete[: len(baseline)] != baseline:
            raise CodexParseError(
                "append-only source violation: existing prefix was rewritten for "
                f"{source_path!r}"
            )

        content_changed = complete != baseline
        manifest_advanced = previous is None or len(complete) != previous.copied_bytes
        source_updated_at = (
            updated_at
            if content_changed or manifest_advanced or previous is None
            else previous.updated_at
        )
        source = CodexSourceCopy(
            source_path=source_path,
            original_path=str(path.resolve()),
            snapshot_path=source_path,
            thread_id=thread_id,
            copied_bytes=len(complete),
            line_count=complete.count(b"\n"),
            sha256=hashlib.sha256(complete).hexdigest(),
            updated_at=source_updated_at,
        )
        pending.append(_PendingCopy(source, complete, existing))

    continuations = _continuation_links(
        roots,
        root_sources,
        complete_by_path,
        metadata_by_path,
        previous_continuations,
    )

    changed = sum(
        int(
            _write_snapshot_file(
                snapshot_root,
                item.source.snapshot_path,
                item.complete,
                item.baseline,
            )
        )
        for item in pending
    )
    return CodexSnapshotResult(
        tuple(item.source for item in pending), changed, continuations
    )


def _content(payload: Mapping[str, object]) -> tuple[str | None, str | None, str]:
    raw_content = payload.get("content")
    if not isinstance(raw_content, list):
        return None, None, "none"
    plain_parts: list[str] = []
    encrypted_parts: list[str] = []
    for raw_part in raw_content:
        part = _mapping(raw_part)
        part_type = _string(part.get("type"))
        if part_type in ("input_text", "output_text"):
            text = _string(part.get("text"))
            if text is not None:
                plain_parts.append(text)
        elif part_type == "encrypted_content":
            encrypted = _string(part.get("encrypted_content"))
            if encrypted is not None:
                encrypted_parts.append(encrypted)
    plain = "".join(plain_parts) or None
    encrypted = "\n".join(encrypted_parts) or None
    if encrypted is not None:
        return plain, encrypted, "encrypted"
    if plain is not None:
        return plain, None, "plaintext"
    return None, None, "none"


def _canonical_records(
    records: Sequence[_Record], metadata: Mapping[str, object], root_thread_id: str
) -> tuple[_Record, ...]:
    thread_id = _string(metadata.get("id")) or ""
    if thread_id == root_thread_id:
        return tuple(records[1:])

    agent_path = _string(metadata.get("agent_path"))
    initial_turn_id: str | None = None
    incoming_line = 0
    if agent_path is not None:
        for record in records[1:]:
            if record.value.get("type") != "response_item":
                continue
            payload = _record_payload(record)
            if payload.get("type") != "agent_message":
                continue
            if _string(payload.get("recipient")) != agent_path:
                continue
            passthrough = _mapping(payload.get("internal_chat_message_metadata_passthrough"))
            initial_turn_id = _string(passthrough.get("turn_id"))
            incoming_line = record.line
            if initial_turn_id is not None:
                break

    if initial_turn_id is not None:
        for index, record in enumerate(records):
            if record.line > incoming_line or record.value.get("type") != "event_msg":
                continue
            payload = _record_payload(record)
            if (
                payload.get("type") == "task_started"
                and _string(payload.get("turn_id")) == initial_turn_id
            ):
                return tuple(records[index:])

    # Fallback for older logs lacking recipient/turn metadata: imported parent turns
    # started before the child's own session timestamp, while the child's turn starts at
    # or after it.
    session_started_ms = _iso_ms(metadata.get("timestamp"))
    for index, record in enumerate(records[1:], start=1):
        if record.value.get("type") != "event_msg":
            continue
        payload = _record_payload(record)
        if payload.get("type") != "task_started":
            continue
        started_ms = _numeric_epoch_ms(
            payload.get("started_at"), _record_timestamp_ms(record)
        )
        if started_ms >= session_started_ms:
            return tuple(records[index:])
    return ()


def _read_rollout(
    path: Path,
    sessions_root: Path,
    metadata: Mapping[str, object],
    lineage_root_id: str,
    secure_source_path: str | None = None,
) -> _Rollout:
    if secure_source_path is not None:
        opened = _read_snapshot_file(sessions_root, secure_source_path)
        if opened is None:
            raise CodexParseError(f"source snapshot disappeared before parsing: {path}")
        data, file_stat = opened
    else:
        try:
            data = path.read_bytes()
            file_stat = path.stat()
        except OSError as exc:
            raise CodexParseError(f"cannot read rollout {path}: {exc}") from exc

    complete = _complete_prefix(data)
    complete_bytes = len(complete)
    raw_lines = complete.splitlines()
    records: list[_Record] = []
    for line_number, raw_line in enumerate(raw_lines, start=1):
        value = _parse_json_object(raw_line, f"{path}:{line_number}")
        records.append(_Record(line_number, raw_line, value))

    thread_id = _string(metadata.get("id")) or ""
    source_path = _relative_path(path, sessions_root)
    snapshot = SourceSnapshot(
        path=source_path,
        thread_id=thread_id,
        size_bytes=len(data),
        mtime_ns=file_stat.st_mtime_ns,
        sha256=hashlib.sha256(data).hexdigest(),
        complete_bytes=complete_bytes,
        line_count=len(raw_lines),
        working_directory=_string(metadata.get("cwd")),
        repository_url=_string(_mapping(metadata.get("git")).get("repository_url")),
    )
    canonical = _canonical_records(records, metadata, lineage_root_id)
    return _Rollout(
        path=path,
        source_path=source_path,
        snapshot=snapshot,
        metadata=metadata,
        records=tuple(records),
        canonical_records=canonical,
        lineage_root_id=lineage_root_id,
    )


def _agent_seed(rollout: _Rollout) -> _AgentSeed:
    metadata = rollout.metadata
    thread_id = _string(metadata.get("id")) or ""
    source = _mapping(metadata.get("source"))
    subagent = _mapping(source.get("subagent"))
    spawn = _mapping(subagent.get("thread_spawn"))
    parent = _string(metadata.get("parent_thread_id")) or _string(
        spawn.get("parent_thread_id")
    )
    nickname = _string(metadata.get("agent_nickname")) or _string(
        spawn.get("agent_nickname")
    )
    role = _string(metadata.get("agent_role")) or _string(spawn.get("agent_role"))
    depth = _integer(spawn.get("depth"))
    if depth is None:
        depth = 0 if thread_id == rollout.lineage_root_id else 1
    path = _string(metadata.get("agent_path")) or _string(spawn.get("agent_path"))
    if path is None:
        path = "/root" if thread_id == rollout.lineage_root_id else f"/root/{thread_id}"
    return _AgentSeed(
        thread_id=thread_id,
        parent_thread_id=parent,
        agent_path=path,
        nickname=nickname,
        role=role,
        depth=depth,
        started_at_ms=_iso_ms(metadata.get("timestamp")),
        source_path=rollout.source_path,
        lineage_root_id=rollout.lineage_root_id,
        raw_agent_path=path,
    )


def _scoped_id(lineage_root_id: str, root_thread_id: str, raw_id: str) -> str:
    if lineage_root_id == root_thread_id:
        return raw_id
    # Codex IDs are provider strings, not guaranteed UUIDs. Length-prefix both variable
    # components so pairs such as ("a-b", "c") and ("a", "b-c") cannot collapse to the
    # same normalized identity. The canonical lineage deliberately keeps its historical IDs.
    return (
        f"codex-continuation-{len(lineage_root_id)}-{lineage_root_id}-"
        f"{len(raw_id)}-{raw_id}"
    )


def _continuation_path(lineage_root_id: str) -> str:
    # Keep the provider ID in one injective path component. In particular, a root ID ``a/b``
    # must not occupy the same path as child ``b`` beneath continuation root ``a``; quoting ``%``
    # as well as ``/`` makes the encoding reversible and collision-free.
    return f"/root/continuation-{quote(lineage_root_id, safe='')}"


def _normalize_seed(
    seed: _AgentSeed,
    root_thread_id: str,
    continuation_thread_ids: Sequence[str],
) -> _AgentSeed:
    if seed.lineage_root_id == root_thread_id:
        return seed
    try:
        continuation_index = continuation_thread_ids.index(seed.lineage_root_id)
    except ValueError as error:
        raise CodexParseError(
            f"agent {seed.thread_id!r} belongs to an unknown continuation"
        ) from error
    predecessor = (
        root_thread_id
        if continuation_index == 0
        else continuation_thread_ids[continuation_index - 1]
    )
    prefix = _continuation_path(seed.lineage_root_id)
    raw_path = seed.raw_agent_path.rstrip("/") or "/root"
    if raw_path == "/root":
        normalized_path = prefix
    elif raw_path.startswith("/root/"):
        normalized_path = prefix + raw_path[len("/root") :]
    else:
        normalized_path = prefix + "/" + raw_path.strip("/")
    is_lineage_root = seed.thread_id == seed.lineage_root_id
    return _AgentSeed(
        thread_id=seed.thread_id,
        parent_thread_id=predecessor if is_lineage_root else seed.parent_thread_id,
        agent_path=normalized_path,
        nickname=seed.nickname,
        role="coordinator" if is_lineage_root else seed.role,
        depth=(continuation_index + 1 if is_lineage_root else seed.depth + continuation_index + 1),
        started_at_ms=seed.started_at_ms,
        source_path=seed.source_path,
        lineage_root_id=seed.lineage_root_id,
        raw_agent_path=seed.raw_agent_path,
    )


def _prompt_pairs(records: Sequence[_Record]) -> dict[tuple[int, str], list[str]]:
    pairs: dict[tuple[int, str], list[str]] = defaultdict(list)
    for record in records:
        if record.value.get("type") != "response_item":
            continue
        payload = _record_payload(record)
        if payload.get("type") != "message" or payload.get("role") != "user":
            continue
        text, _, _ = _content(payload)
        item_id = _string(payload.get("id"))
        if text is not None and item_id is not None:
            pairs[(_record_timestamp_ms(record), text)].append(item_id)
    return pairs


def _claim_prompt_id(
    pairs: Mapping[tuple[int, str], list[str]], timestamp_ms: int, text: str
) -> str | None:
    exact = pairs.get((timestamp_ms, text))
    if exact:
        return exact.pop(0)
    candidates = [
        (abs(candidate_ms - timestamp_ms), ids)
        for (candidate_ms, candidate_text), ids in pairs.items()
        if candidate_text == text and ids and abs(candidate_ms - timestamp_ms) <= 10
    ]
    if not candidates:
        return None
    _, ids = min(candidates, key=lambda item: item[0])
    return ids.pop(0)


def _nested_tools(name: str, input_text: str | None) -> tuple[tuple[str, int], ...]:
    if name != "exec" or input_text is None:
        return ()
    counts = Counter(_NESTED_TOOL_RE.findall(input_text))
    return tuple(sorted(counts.items()))


def _call_arguments(tool: _ToolBuilder) -> dict[str, object]:
    if tool.input_text is None:
        return {}
    try:
        value: object = json.loads(tool.input_text)
    except json.JSONDecodeError:
        return {}
    return _mapping(value)


def _message_fields(arguments: Mapping[str, object]) -> tuple[str | None, str | None, str]:
    message = _string(arguments.get("message"))
    if message is None:
        return None, None, "none"
    if message.startswith("gAAAA"):
        return None, message, "encrypted"
    return message, None, "plaintext"


def _resolve_target(
    target: str | None,
    path_to_thread: Mapping[str, str],
    known_threads: set[str],
) -> str | None:
    if target is None:
        return None
    if target in known_threads:
        return target
    if target in path_to_thread:
        return path_to_thread[target]
    candidates = [target]
    if not target.startswith("/"):
        candidates.append(f"/root/{target}")
    for candidate in candidates:
        if candidate in path_to_thread:
            return path_to_thread[candidate]
    suffix = "/" + target.strip("/")
    matches = [thread_id for path, thread_id in path_to_thread.items() if path.endswith(suffix)]
    return matches[0] if len(matches) == 1 else None


def _turn_id_from_payload(
    payload: Mapping[str, object],
    current: str | None,
    lineage_root_id: str,
    root_thread_id: str,
) -> str | None:
    passthrough = _mapping(payload.get("internal_chat_message_metadata_passthrough"))
    raw = _string(passthrough.get("turn_id"))
    return _scoped_id(lineage_root_id, root_thread_id, raw) if raw is not None else current


def load_codex_team(
    sessions_root: Path,
    root_thread_id: str,
    team_slug: str,
    display_timezone: str,
    source_paths: Sequence[str] | None = None,
    continuation_links: Sequence[CodexContinuationLink] = (),
) -> TeamData:
    """Load one Codex root and every rollout belonging to its session lineage.

    The importer consumes only newline-complete records. When *source_paths* is supplied, only
    those safe relative paths are opened, without following symlinks; unrelated files beneath the
    root cannot enter the parse. Forked parent-history prefixes and duplicate UI message events are
    deliberately excluded from canonical transcript data.
    """

    ZoneInfo(display_timezone)  # validate now; conversion remains a presentation concern
    continuation_thread_ids = tuple(link.thread_id for link in continuation_links)
    roots = _lineage_roots(root_thread_id, continuation_thread_ids)
    for index, link in enumerate(continuation_links):
        expected_predecessor = (
            root_thread_id
            if index == 0
            else continuation_links[index - 1].thread_id
        )
        if link.predecessor_thread_id != expected_predecessor:
            raise CodexParseError(
                f"continuation {index} does not follow the configured predecessor"
            )
    secure_paths: dict[Path, str] = {}
    if source_paths is None:
        candidates = _discover_codex_lineage(
            sessions_root,
            root_thread_id,
            continuation_thread_ids=continuation_thread_ids,
        )
    else:
        seen: set[str] = set()
        explicit: list[tuple[Path, Mapping[str, object]]] = []
        for source_path in source_paths:
            if source_path in seen:
                raise CodexParseError(f"duplicate explicit source path {source_path!r}")
            seen.add(source_path)
            path = _snapshot_target(sessions_root, source_path)
            metadata = _first_snapshot_metadata(sessions_root, source_path)
            if metadata is None:
                raise CodexParseError(
                    f"explicit source snapshot lacks valid session metadata: {source_path!r}"
                )
            _metadata_lineage_root(metadata, roots, source_path)
            explicit.append((path, metadata))
            secure_paths[path] = source_path
        candidates = tuple(explicit)
    if not candidates:
        raise CodexParseError(f"Codex root thread {root_thread_id!r} was not found")

    rollouts = tuple(
        _read_rollout(
            path,
            sessions_root,
            metadata,
            _metadata_lineage_root(metadata, roots, str(path)),
            secure_paths.get(path),
        )
        for path, metadata in candidates
    )
    if source_paths is not None:
        expected = set(source_paths)
        parsed = {rollout.source_path for rollout in rollouts}
        if parsed != expected:
            raise CodexParseError(
                "parsed source set does not match the validated source manifest: "
                f"expected {sorted(expected)!r}, parsed {sorted(parsed)!r}"
            )
    seeds: dict[str, _AgentSeed] = {}
    lineage_by_thread: dict[str, str] = {}
    for rollout in rollouts:
        seed = _normalize_seed(
            _agent_seed(rollout), root_thread_id, continuation_thread_ids
        )
        prior_lineage = lineage_by_thread.get(seed.thread_id)
        if prior_lineage is not None and prior_lineage != seed.lineage_root_id:
            raise CodexParseError(
                f"Codex thread {seed.thread_id!r} occurs in multiple configured lineages"
            )
        lineage_by_thread[seed.thread_id] = seed.lineage_root_id
        previous = seeds.get(seed.thread_id)
        if previous is None or (seed.started_at_ms, seed.source_path) < (
            previous.started_at_ms,
            previous.source_path,
        ):
            seeds[seed.thread_id] = seed
    if root_thread_id not in seeds:
        raise CodexParseError(f"lineage lacks root metadata for {root_thread_id!r}")
    missing_continuations = [
        thread_id for thread_id in continuation_thread_ids if thread_id not in seeds
    ]
    if missing_continuations:
        raise CodexParseError(
            "lineage lacks continuation root metadata for "
            + ", ".join(repr(item) for item in missing_continuations)
        )

    turns: dict[tuple[str, str], _TurnBuilder] = {}
    tools: dict[str, _ToolBuilder] = {}
    events: dict[tuple[str, str], Event] = {}
    activities: list[_Activity] = []

    for rollout in sorted(rollouts, key=lambda item: item.source_path):
        thread_id = _string(rollout.metadata.get("id")) or ""
        lineage_root_id = rollout.lineage_root_id
        prompt_pairs = _prompt_pairs(rollout.canonical_records)
        current_turn: str | None = None
        for record in rollout.canonical_records:
            top_type = _string(record.value.get("type"))
            payload = _record_payload(record)
            payload_type = _string(payload.get("type"))
            timestamp_ms = _record_timestamp_ms(record)

            if top_type == "turn_context":
                raw_turn_id = _string(payload.get("turn_id"))
                current_turn = (
                    _scoped_id(lineage_root_id, root_thread_id, raw_turn_id)
                    if raw_turn_id is not None
                    else current_turn
                )
                continue

            if top_type == "event_msg" and payload_type == "task_started":
                raw_turn_id = _string(payload.get("turn_id"))
                if raw_turn_id is None:
                    continue
                turn_id = _scoped_id(lineage_root_id, root_thread_id, raw_turn_id)
                current_turn = turn_id
                key = (thread_id, turn_id)
                started_at_ms = _numeric_epoch_ms(payload.get("started_at"), timestamp_ms)
                existing = turns.get(key)
                if existing is None:
                    turns[key] = _TurnBuilder(turn_id, thread_id, started_at_ms)
                else:
                    existing.started_at_ms = min(existing.started_at_ms, started_at_ms)
                continue

            if top_type == "event_msg" and payload_type in ("task_complete", "turn_aborted"):
                raw_turn_id = _string(payload.get("turn_id"))
                if raw_turn_id is None:
                    continue
                turn_id = _scoped_id(lineage_root_id, root_thread_id, raw_turn_id)
                key = (thread_id, turn_id)
                started_at_ms = _numeric_epoch_ms(payload.get("started_at"), timestamp_ms)
                builder = turns.get(key)
                if builder is None:
                    builder = _TurnBuilder(turn_id, thread_id, started_at_ms)
                    turns[key] = builder
                builder.ended_at_ms = _numeric_epoch_ms(
                    payload.get("completed_at"), timestamp_ms
                )
                builder.status = "completed" if payload_type == "task_complete" else "aborted"
                builder.first_token_ms = _integer(payload.get("time_to_first_token_ms"))
                builder.error = _textual(payload.get("error") or payload.get("reason"))
                builder.last_agent_message = _string(payload.get("last_agent_message"))
                if current_turn == turn_id:
                    current_turn = None
                continue

            if top_type == "event_msg" and payload_type == "user_message":
                text = _string(payload.get("message"))
                if text is None:
                    continue
                item_id = _claim_prompt_id(prompt_pairs, timestamp_ms, text)
                raw_event_id = item_id or _fallback_id(
                    thread_id, record, "user_prompt"
                )
                event_id = _scoped_id(
                    lineage_root_id, root_thread_id, raw_event_id
                )
                events[(thread_id, event_id)] = Event(
                    event_id=event_id,
                    thread_id=thread_id,
                    turn_id=current_turn,
                    timestamp_ms=timestamp_ms,
                    kind="user_prompt",
                    role="user",
                    phase=None,
                    text=text,
                    content_availability="plaintext",
                    encrypted_content=None,
                    author="user",
                    recipient=None,
                    source_line=record.line,
                    ingress_kind="codex",
                    author_kind="owner_human",
                    source_native_id=item_id,
                    classification_version=_CLASSIFICATION_VERSION,
                )
                continue

            if top_type == "event_msg" and payload_type == "sub_agent_activity":
                raw_event_id = _string(payload.get("event_id")) or _fallback_id(
                    thread_id, record, "subagent_activity"
                )
                event_id = _scoped_id(
                    lineage_root_id, root_thread_id, raw_event_id
                )
                kind = _string(payload.get("kind")) or "interaction"
                occurred_ms = _integer(payload.get("occurred_at_ms")) or timestamp_ms
                events[(thread_id, event_id)] = Event(
                    event_id=event_id,
                    thread_id=thread_id,
                    turn_id=current_turn,
                    timestamp_ms=occurred_ms,
                    kind=f"subagent_{kind}",
                    role=None,
                    phase=None,
                    text=_string(payload.get("agent_path")),
                    content_availability="none",
                    encrypted_content=None,
                    author=None,
                    recipient=None,
                    source_line=record.line,
                )
                activities.append(
                    _Activity(
                        thread_id,
                        occurred_ms,
                        payload,
                        record.line,
                        lineage_root_id,
                    )
                )
                continue

            if top_type == "event_msg" and payload_type == "context_compacted":
                event_id = _scoped_id(
                    lineage_root_id,
                    root_thread_id,
                    _fallback_id(thread_id, record, "context_compacted"),
                )
                events[(thread_id, event_id)] = Event(
                    event_id,
                    thread_id,
                    current_turn,
                    timestamp_ms,
                    "context_compacted",
                    None,
                    None,
                    None,
                    "none",
                    None,
                    None,
                    None,
                    record.line,
                )
                continue

            if top_type == "event_msg" and payload_type == "thread_goal_updated":
                goal = _mapping(payload.get("goal"))
                status = _string(goal.get("status"))
                objective = _string(goal.get("objective"))
                text = " | ".join(value for value in (status, objective) if value) or None
                event_id = _scoped_id(
                    lineage_root_id,
                    root_thread_id,
                    _fallback_id(thread_id, record, "goal_updated"),
                )
                events[(thread_id, event_id)] = Event(
                    event_id,
                    thread_id,
                    current_turn,
                    timestamp_ms,
                    "goal_updated",
                    None,
                    None,
                    text,
                    "plaintext" if text is not None else "none",
                    None,
                    None,
                    None,
                    record.line,
                )
                continue

            if top_type != "response_item":
                continue

            if payload_type == "message" and payload.get("role") == "assistant":
                text, encrypted, availability = _content(payload)
                raw_item_id = _string(payload.get("id")) or _fallback_id(
                    thread_id, record, "assistant_message"
                )
                item_id = _scoped_id(
                    lineage_root_id, root_thread_id, raw_item_id
                )
                events[(thread_id, item_id)] = Event(
                    event_id=item_id,
                    thread_id=thread_id,
                    turn_id=_turn_id_from_payload(
                        payload, current_turn, lineage_root_id, root_thread_id
                    ),
                    timestamp_ms=timestamp_ms,
                    kind="assistant_message",
                    role="assistant",
                    phase=_string(payload.get("phase")),
                    text=text,
                    content_availability=availability,
                    encrypted_content=encrypted,
                    author=None,
                    recipient=None,
                    source_line=record.line,
                )
                continue

            if payload_type == "agent_message":
                text, encrypted, availability = _content(payload)
                raw_item_id = _string(payload.get("id")) or _fallback_id(
                    thread_id, record, "inter_agent_message"
                )
                item_id = _scoped_id(
                    lineage_root_id, root_thread_id, raw_item_id
                )
                events[(thread_id, item_id)] = Event(
                    event_id=item_id,
                    thread_id=thread_id,
                    turn_id=_turn_id_from_payload(
                        payload, current_turn, lineage_root_id, root_thread_id
                    ),
                    timestamp_ms=timestamp_ms,
                    kind="inter_agent_message",
                    role=None,
                    phase=None,
                    text=text,
                    content_availability=availability,
                    encrypted_content=encrypted,
                    author=_string(payload.get("author")),
                    recipient=_string(payload.get("recipient")),
                    source_line=record.line,
                )
                continue

            if payload_type in ("custom_tool_call", "function_call"):
                raw_call_id = _string(payload.get("call_id"))
                name = _string(payload.get("name"))
                if raw_call_id is None or name is None:
                    continue
                call_id = _scoped_id(
                    lineage_root_id, root_thread_id, raw_call_id
                )
                input_text = _string(
                    payload.get("input")
                    if payload_type == "custom_tool_call"
                    else payload.get("arguments")
                )
                raw_tool_item_id = _string(payload.get("id"))
                existing_tool = tools.get(call_id)
                if (
                    existing_tool is not None
                    and existing_tool.lineage_root_id != lineage_root_id
                ):
                    raise CodexParseError(
                        f"tool call {raw_call_id!r} collides across Codex lineages"
                    )
                tools[call_id] = _ToolBuilder(
                    call_id=call_id,
                    item_id=(
                        _scoped_id(
                            lineage_root_id, root_thread_id, raw_tool_item_id
                        )
                        if raw_tool_item_id is not None
                        else None
                    ),
                    thread_id=thread_id,
                    turn_id=_turn_id_from_payload(
                        payload, current_turn, lineage_root_id, root_thread_id
                    ),
                    name=name,
                    namespace=_string(payload.get("namespace")),
                    started_at_ms=timestamp_ms,
                    ended_at_ms=None,
                    status=_string(payload.get("status")) or "running",
                    input_text=input_text,
                    output_text=None,
                    nested_tools=_nested_tools(name, input_text),
                    source_line=record.line,
                    lineage_root_id=lineage_root_id,
                )
                continue

            if payload_type in ("custom_tool_call_output", "function_call_output"):
                raw_call_id = _string(payload.get("call_id"))
                if raw_call_id is None:
                    continue
                call_id = _scoped_id(
                    lineage_root_id, root_thread_id, raw_call_id
                )
                if call_id not in tools:
                    continue
                tool_builder = tools[call_id]
                tool_builder.ended_at_ms = timestamp_ms
                tool_builder.status = "completed"
                tool_builder.output_text = _textual(payload.get("output"))

    turn_values = tuple(
        sorted(
            (
                Turn(
                    item.turn_id,
                    item.thread_id,
                    item.started_at_ms,
                    item.ended_at_ms,
                    item.status,
                    item.first_token_ms,
                    item.error,
                    item.last_agent_message,
                )
                for item in turns.values()
            ),
            key=lambda item: (item.started_at_ms, item.thread_id, item.turn_id),
        )
    )
    tool_values = tuple(
        sorted(
            (
                ToolCall(
                    item.call_id,
                    item.item_id,
                    item.thread_id,
                    item.turn_id,
                    item.name,
                    item.namespace,
                    item.started_at_ms,
                    item.ended_at_ms,
                    item.status,
                    item.input_text,
                    item.output_text,
                    item.nested_tools,
                    item.source_line,
                )
                for item in tools.values()
            ),
            key=lambda item: (item.started_at_ms, item.thread_id, item.call_id),
        )
    )

    path_to_thread_by_lineage: dict[str, dict[str, str]] = defaultdict(dict)
    known_threads_by_lineage: dict[str, set[str]] = defaultdict(set)
    for seed in seeds.values():
        path_map = path_to_thread_by_lineage[seed.lineage_root_id]
        previous_thread = path_map.get(seed.raw_agent_path)
        if previous_thread is not None and previous_thread != seed.thread_id:
            raise CodexParseError(
                f"duplicate agent path {seed.raw_agent_path!r} in Codex lineage "
                f"{seed.lineage_root_id!r}"
            )
        path_map[seed.raw_agent_path] = seed.thread_id
        known_threads_by_lineage[seed.lineage_root_id].add(seed.thread_id)
    edges_by_id: dict[str, Edge] = {}
    for link in continuation_links:
        edge_id = f"codex-continuation-{link.thread_id}"
        edges_by_id[edge_id] = Edge(
            edge_id=edge_id,
            call_id=edge_id,
            from_thread_id=link.predecessor_thread_id,
            to_thread_id=link.thread_id,
            kind="continuation",
            timestamp_ms=link.predecessor_at_ms,
            message_text=(
                "Explicit Codex session continuation. Predecessor source "
                f"{link.predecessor_source_path} line {link.predecessor_source_line} "
                f"was recorded at {link.predecessor_at_ms} ms UTC; successor source "
                f"{link.source_path} started at {link.started_at_ms} ms UTC "
                f"({link.gap_ms} ms later)."
            ),
            content_availability="plaintext",
            encrypted_content=None,
            source_line=link.predecessor_source_line,
        )
    interrupted_at: dict[str, int] = {}
    for activity in sorted(
        activities, key=lambda item: (item.timestamp_ms, item.owner_thread_id, item.source_line)
    ):
        raw_call_id = _string(activity.payload.get("event_id"))
        activity_kind = _string(activity.payload.get("kind"))
        agent_thread_id = _string(activity.payload.get("agent_thread_id"))
        if raw_call_id is None or activity_kind is None:
            continue
        call_id = _scoped_id(
            activity.lineage_root_id, root_thread_id, raw_call_id
        )
        known_threads = known_threads_by_lineage[activity.lineage_root_id]
        if agent_thread_id not in known_threads:
            agent_thread_id = None
        tool = tools.get(call_id)
        if tool is None:
            if activity_kind == "interrupted" and agent_thread_id is not None:
                interrupted_at[agent_thread_id] = activity.timestamp_ms
            continue
        arguments = _call_arguments(tool)
        target_name = _string(arguments.get("target"))
        target_thread = _resolve_target(
            target_name,
            path_to_thread_by_lineage[activity.lineage_root_id],
            known_threads,
        )
        if activity_kind == "started":
            target_thread = agent_thread_id
        if target_thread is None and agent_thread_id != tool.thread_id:
            target_thread = agent_thread_id
        if target_thread is None and activity.owner_thread_id != tool.thread_id:
            target_thread = activity.owner_thread_id
        if target_thread is None or target_thread == tool.thread_id:
            continue
        kind_by_call = {
            "spawn_agent": "spawn",
            "send_message": "message",
            "followup_task": "followup",
            "interrupt_agent": "interrupt",
        }
        edge_kind = kind_by_call.get(tool.name, activity_kind)
        message, encrypted, availability = _message_fields(arguments)
        existing_edge = edges_by_id.get(call_id)
        if existing_edge is not None and existing_edge.kind == "continuation":
            raise CodexParseError(
                f"collaboration call {raw_call_id!r} collides with a continuation edge"
            )
        edges_by_id[call_id] = Edge(
            edge_id=call_id,
            call_id=call_id,
            from_thread_id=tool.thread_id,
            to_thread_id=target_thread,
            kind=edge_kind,
            timestamp_ms=activity.timestamp_ms,
            message_text=message,
            content_availability=availability,
            encrypted_content=encrypted,
            source_line=activity.source_line,
        )
        if edge_kind == "interrupt":
            interrupted_at[target_thread] = activity.timestamp_ms

    turns_by_thread: dict[str, list[Turn]] = defaultdict(list)
    for turn in turn_values:
        turns_by_thread[turn.thread_id].append(turn)
    agents: list[Agent] = []
    for seed in seeds.values():
        own_turns = sorted(
            turns_by_thread.get(seed.thread_id, []), key=lambda item: item.started_at_ms
        )
        interrupted = interrupted_at.get(seed.thread_id)
        # Interrupts terminate one incarnation, not the thread forever: coordinators may
        # legitimately restart the same subagent. Resolve the latest lifecycle transition in
        # chronological order so a later turn start supersedes a historical interrupt.
        transitions: list[tuple[int, int, str, int | None]] = [
            (seed.started_at_ms, -1, "created", None)
        ]
        for turn_index, turn in enumerate(own_turns):
            # Codex lifecycle payloads are only second-granular. A completed turn and its
            # successor can therefore share a timestamp; preserve turn chronology as the tie
            # breaker so the later start wins over the earlier completion. Within one zero-length
            # turn, its completion still wins over its own start.
            start_order = turn_index * 2
            transitions.append((turn.started_at_ms, start_order, "running", None))
            if turn.ended_at_ms is not None:
                transitions.append(
                    (turn.ended_at_ms, start_order + 1, turn.status, turn.ended_at_ms)
                )
        if interrupted is not None:
            transitions.append(
                (interrupted, len(own_turns) * 2 + 1, "interrupted", interrupted)
            )
        _, _, status, ended_at_ms = max(transitions)
        agents.append(
            Agent(
                seed.thread_id,
                seed.parent_thread_id,
                seed.agent_path,
                seed.nickname,
                seed.role,
                seed.depth,
                seed.started_at_ms,
                ended_at_ms,
                status,
                seed.source_path,
            )
        )

    return TeamData(
        team_slug=team_slug,
        provider="codex",
        root_thread_id=root_thread_id,
        display_timezone=display_timezone,
        sources=tuple(sorted((item.snapshot for item in rollouts), key=lambda item: item.path)),
        agents=tuple(
            sorted(agents, key=lambda item: (item.started_at_ms, item.depth, item.agent_path))
        ),
        turns=turn_values,
        events=tuple(
            sorted(
                events.values(),
                key=lambda item: (
                    item.timestamp_ms,
                    item.thread_id,
                    item.source_line,
                    item.event_id,
                ),
            )
        ),
        tool_calls=tool_values,
        edges=tuple(
            sorted(
                edges_by_id.values(),
                key=lambda item: (item.timestamp_ms, item.kind, item.edge_id),
            )
        ),
    )


__all__ = [
    "CodexContinuationLink",
    "CodexParseError",
    "CodexSnapshotResult",
    "CodexSourceCopy",
    "codex_identity_metadata",
    "load_codex_team",
    "snapshot_codex_lineage",
]
