"""Import a Claude Code coordinator lineage into the provider-neutral model."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path, PurePosixPath
import re

from wrkviz.archive import write_text_if_changed
from wrkviz.model import (
    Agent,
    Edge,
    Event,
    SourceSnapshot,
    TeamData,
    ToolCall,
    Turn,
)


_AGENT_FILE = re.compile(r"agent-(?P<agent>[A-Za-z0-9_-]+)\.jsonl\Z")
_SAFE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\Z")
_CLASSIFICATION_VERSION = "authorship-v1"
_SCHEDULED_INPUT_PREFIXES = (
    "MISSION RE-READ (",
    "Stay in the triage/rebase/fix/land loop",
    "DISK STOPGAP (",
)


class ClaudeParseError(ValueError):
    """A selected Claude Code lineage is malformed or cannot be imported safely."""


@dataclass(frozen=True)
class ClaudeSourceCopy:
    """One archive-local copy of a Claude transcript or agent sidecar."""

    source_path: str
    original_path: str
    snapshot_path: str
    thread_id: str
    copied_bytes: int
    line_count: int
    sha256: str
    updated_at: str

    def to_json_obj(self) -> dict[str, object]:
        """Return this validated source-copy record as a JSON object."""

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
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> ClaudeSourceCopy:
        """Validate and decode one source-copy record from archive JSON."""

        source_path = _required_string(raw.get("source_path"), where + ".source_path")
        original_path = _required_string(
            raw.get("original_path"), where + ".original_path"
        )
        snapshot_path = _required_string(
            raw.get("snapshot_path"), where + ".snapshot_path"
        )
        thread_id = _required_string(raw.get("thread_id"), where + ".thread_id")
        copied_bytes = _required_int(raw.get("copied_bytes"), where + ".copied_bytes")
        line_count = _required_int(raw.get("line_count"), where + ".line_count")
        sha256 = _required_string(raw.get("sha256"), where + ".sha256")
        updated_at = _required_string(raw.get("updated_at"), where + ".updated_at")
        _safe_relative(source_path)
        if snapshot_path != source_path:
            raise ClaudeParseError(f"{where}: snapshot_path must equal source_path")
        if copied_bytes < 0 or line_count < 0:
            raise ClaudeParseError(f"{where}: source counts must be non-negative")
        if re.fullmatch(r"[0-9a-f]{64}", sha256) is None:
            raise ClaudeParseError(f"{where}: invalid source digest")
        _validate_thread_id(thread_id, where)
        return cls(
            source_path=source_path,
            original_path=original_path,
            snapshot_path=snapshot_path,
            thread_id=thread_id,
            copied_bytes=copied_bytes,
            line_count=line_count,
            sha256=sha256,
            updated_at=updated_at,
        )


@dataclass(frozen=True)
class ClaudeSnapshotResult:
    """Result of copying a complete Claude lineage into an archive."""

    sources: tuple[ClaudeSourceCopy, ...]
    files_changed: int


@dataclass(frozen=True)
class _Record:
    source_path: str
    line: int
    raw: bytes
    value: Mapping[str, object]
    thread_id: str
    timestamp_ms: int | None


@dataclass(frozen=True)
class _Meta:
    agent_id: str
    tool_use_id: str | None
    parent_agent_id: str | None
    description: str | None
    agent_type: str | None
    spawn_depth: int | None


@dataclass
class _ToolBuilder:
    call_id: str
    item_id: str | None
    thread_id: str
    started_at_ms: int
    name: str
    input_text: str | None
    source_line: int
    input_object: Mapping[str, object]


def _mapping(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ClaudeParseError(f"{where}: expected an object")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise ClaudeParseError(f"{where}: object key is not a string")
        result[key] = item
    return result


def _optional_mapping(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        return {}
    result: dict[str, object] = {}
    for key, item in value.items():
        if isinstance(key, str):
            result[key] = item
    return result


def _array(value: object, where: str) -> list[object]:
    if not isinstance(value, list):
        raise ClaudeParseError(f"{where}: expected an array")
    return list(value)


def _optional_string(value: object) -> str | None:
    return value if isinstance(value, str) else None


def _required_string(value: object, where: str) -> str:
    if not isinstance(value, str) or not value:
        raise ClaudeParseError(f"{where}: expected a non-empty string")
    return value


def _required_int(value: object, where: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ClaudeParseError(f"{where}: expected an integer")
    return value


def _optional_int(value: object, where: str) -> int | None:
    if value is None:
        return None
    return _required_int(value, where)


def _textual(value: object) -> str | None:
    if value is None:
        return None
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    except (TypeError, ValueError):
        return str(value)


def _parse_timestamp(value: object, where: str) -> int | None:
    if value is None:
        return None
    text = _required_string(value, where)
    normalized = text[:-1] + "+00:00" if text.endswith("Z") else text
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError as error:
        raise ClaudeParseError(f"{where}: invalid ISO timestamp {text!r}") from error
    if parsed.tzinfo is None:
        raise ClaudeParseError(f"{where}: timestamp has no timezone")
    return int(parsed.timestamp() * 1000)


def _parse_json(raw: bytes, where: str) -> dict[str, object]:
    try:
        value: object = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise ClaudeParseError(f"{where}: invalid JSON ({error})") from error
    return _mapping(value, where)


def _safe_relative(value: str) -> PurePosixPath:
    path = PurePosixPath(value)
    if (
        path.is_absolute()
        or not path.parts
        or any(part in ("", ".", "..") for part in path.parts)
    ):
        raise ClaudeParseError(f"unsafe Claude source path {value!r}")
    return path


def _validate_thread_id(thread_id: str, where: str) -> None:
    if _SAFE_ID.fullmatch(thread_id) is None:
        raise ClaudeParseError(f"{where}: unsafe thread id {thread_id!r}")


def _complete_jsonl(data: bytes) -> bytes:
    if data.endswith(b"\n"):
        return data
    newline = data.rfind(b"\n")
    return data[: newline + 1] if newline >= 0 else b""


def _agent_id_from_path(path: Path) -> str | None:
    match = _AGENT_FILE.fullmatch(path.name)
    return match.group("agent") if match is not None else None


def discover_claude_sources(session_file: Path) -> tuple[Path, ...]:
    """Discover one root JSONL and its Claude-managed subagent files."""

    if session_file.is_symlink() or not session_file.is_file():
        raise ClaudeParseError(f"Claude session is not a regular file: {session_file}")
    if session_file.suffix != ".jsonl":
        raise ClaudeParseError("Claude session filename must end in .jsonl")
    sources = [session_file]
    subagents = session_file.parent / session_file.stem / "subagents"
    if subagents.is_dir():
        for path in sorted(subagents.rglob("agent-*.jsonl")):
            if path.is_symlink() or not path.is_file():
                raise ClaudeParseError(f"Claude subagent source is not regular: {path}")
            sources.append(path)
            meta = path.with_suffix(".meta.json")
            if meta.is_file():
                if meta.is_symlink():
                    raise ClaudeParseError(f"Claude agent metadata is a symlink: {meta}")
                sources.append(meta)
    return tuple(sources)


def _relative_sources(
    session_file: Path, source_paths: Sequence[str] | None
) -> tuple[tuple[str, Path], ...]:
    root = session_file.parent
    if source_paths is None:
        paths = discover_claude_sources(session_file)
        result = [(path.relative_to(root).as_posix(), path) for path in paths]
    else:
        result = []
        for raw in source_paths:
            relative = _safe_relative(raw)
            path = root.joinpath(*relative.parts)
            if path.is_symlink() or not path.is_file():
                raise ClaudeParseError(f"missing Claude source {path}")
            result.append((relative.as_posix(), path))
    root_relative = session_file.name
    if root_relative not in {relative for relative, _ in result}:
        raise ClaudeParseError("Claude source set omits the root transcript")
    return tuple(sorted(result, key=lambda item: (item[0] != root_relative, item[0])))


def _read_records(
    path: Path, source_path: str, root_thread_id: str, is_root: bool
) -> tuple[tuple[_Record, ...], SourceSnapshot, str]:
    data = path.read_bytes()
    complete = _complete_jsonl(data)
    if not complete:
        raise ClaudeParseError(f"{path}: no newline-complete JSON records")
    records: list[_Record] = []
    thread_id: str | None = root_thread_id if is_root else _agent_id_from_path(path)
    for line_number, raw in enumerate(complete.splitlines(), start=1):
        value = _parse_json(raw, f"{path}:{line_number}")
        record_session = _optional_string(value.get("sessionId"))
        if record_session is not None and record_session != root_thread_id:
            raise ClaudeParseError(
                f"{path}:{line_number}: session id {record_session!r} does not match root"
            )
        record_agent = _optional_string(value.get("agentId"))
        if not is_root and record_agent is not None:
            if thread_id is not None and record_agent != thread_id:
                raise ClaudeParseError(
                    f"{path}:{line_number}: inconsistent subagent id {record_agent!r}"
                )
            thread_id = record_agent
        timestamp_ms = _parse_timestamp(
            value.get("timestamp"), f"{path}:{line_number}.timestamp"
        )
        records.append(
            _Record(source_path, line_number, raw, value, thread_id or "", timestamp_ms)
        )
    if thread_id is None:
        raise ClaudeParseError(f"{path}: cannot determine a subagent id")
    _validate_thread_id(thread_id, str(path))
    normalized = tuple(replace(record, thread_id=thread_id) for record in records)
    metadata = path.stat()
    snapshot = SourceSnapshot(
        path=source_path,
        thread_id=thread_id,
        size_bytes=metadata.st_size,
        mtime_ns=metadata.st_mtime_ns,
        sha256=hashlib.sha256(complete).hexdigest(),
        complete_bytes=len(complete),
        line_count=len(records),
    )
    return normalized, snapshot, thread_id


def _read_meta(
    path: Path, source_path: str, agent_id: str
) -> tuple[_Meta, SourceSnapshot]:
    data = path.read_bytes()
    value = _parse_json(data, str(path))
    tool_use_id = _optional_string(value.get("toolUseId"))
    parent_agent_id = _optional_string(value.get("parentAgentId"))
    description = _optional_string(value.get("description"))
    agent_type = _optional_string(value.get("agentType"))
    spawn_depth = _optional_int(value.get("spawnDepth"), str(path) + ".spawnDepth")
    if parent_agent_id is not None:
        _validate_thread_id(parent_agent_id, str(path))
    metadata = path.stat()
    snapshot = SourceSnapshot(
        path=source_path,
        thread_id=agent_id,
        size_bytes=metadata.st_size,
        mtime_ns=metadata.st_mtime_ns,
        sha256=hashlib.sha256(data).hexdigest(),
        complete_bytes=len(data),
        line_count=max(1, len(data.splitlines())),
    )
    return (
        _Meta(
            agent_id=agent_id,
            tool_use_id=tool_use_id,
            parent_agent_id=parent_agent_id,
            description=description,
            agent_type=agent_type,
            spawn_depth=spawn_depth,
        ),
        snapshot,
    )


def _message(record: _Record) -> dict[str, object]:
    return _mapping(record.value.get("message"), f"{record.source_path}:{record.line}.message")


def _blocks(message: Mapping[str, object], where: str) -> tuple[dict[str, object], ...]:
    content = message.get("content")
    if isinstance(content, str):
        return ()
    return tuple(
        _mapping(item, f"{where}.content[{index}]")
        for index, item in enumerate(_array(content, where + ".content"))
    )


def _event_id(record: _Record, suffix: str) -> str:
    uuid = _optional_string(record.value.get("uuid"))
    if uuid is not None:
        return f"{uuid}:{suffix}"
    digest = hashlib.sha256(record.raw).hexdigest()[:20]
    return f"claude-{record.thread_id}-{record.line}-{digest}:{suffix}"


def _source_native_id(record: _Record) -> str | None:
    """Return Claude's stable UUID when this record has one."""

    return _optional_string(record.value.get("uuid"))


def _origin_kind(value: object) -> str | None:
    """Read the explicit producer marker used by recent Claude transcripts."""

    return _optional_string(_optional_mapping(value).get("kind"))


def _is_slash_command(text: str) -> bool:
    return "<command-name>" in text or "<command-message>" in text


def _is_synthetic_user_text(record: _Record, text: str) -> bool:
    """Recognize only source-marked or exact, durable Claude synthetic inputs."""

    if (
        record.value.get("isCompactSummary") is True
        or record.value.get("isMeta") is True
    ):
        return True
    if text.startswith(
        (
            "<local-command-caveat>",
            "<local-command-stdout>",
            "A session-scoped Stop hook is now active",
            "Stop hook feedback:",
            "[Request interrupted by user",
            *_SCHEDULED_INPUT_PREFIXES,
        )
    ):
        return True
    return False


def _claude_input_provenance(
    record: _Record,
    text: str,
    *,
    origin_kind: str | None,
    queued: bool,
) -> tuple[str, str, str, str]:
    """Classify a Claude input without inferring a human from the ``user`` role."""

    if origin_kind == "human":
        ingress = "claude_queued" if queued else "claude_typed"
        return "user_prompt", "user", "owner_human", ingress
    if _is_slash_command(text):
        return "user_prompt", "user", "owner_human", "claude_slash"
    if origin_kind is not None or _is_synthetic_user_text(record, text):
        return "system_input", "system", "system", "claude_system"
    # Older Claude logs do not carry origin metadata. Preserve their plain
    # user messages, but do not claim that authorship is proven.
    return "user_prompt", "user", "unknown", "claude_legacy"


def _record_time(record: _Record) -> int:
    if record.timestamp_ms is None:
        raise ClaudeParseError(
            f"{record.source_path}:{record.line}: message record lacks a timestamp"
        )
    return record.timestamp_ms


def _deduplicate(records: Sequence[_Record]) -> tuple[_Record, ...]:
    seen: dict[tuple[str, str], bytes] = {}
    result: list[_Record] = []
    for record in records:
        uuid = _optional_string(record.value.get("uuid"))
        if uuid is None:
            result.append(record)
            continue
        key = (record.thread_id, uuid)
        previous = seen.get(key)
        if previous is None:
            seen[key] = record.raw
            result.append(record)
        elif previous != record.raw:
            raise ClaudeParseError(
                f"{record.source_path}:{record.line}: conflicting duplicate UUID {uuid}"
            )
    return tuple(result)


def _extract(
    records: Sequence[_Record],
) -> tuple[list[Event], list[_ToolBuilder], dict[str, tuple[int, str, str | None]], list[int]]:
    events: list[Event] = []
    tools: list[_ToolBuilder] = []
    results: dict[str, tuple[int, str, str | None]] = {}
    assistant_times: list[int] = []
    for record in _deduplicate(records):
        record_type = _optional_string(record.value.get("type"))
        if record_type == "attachment":
            attachment = _optional_mapping(record.value.get("attachment"))
            if _optional_string(attachment.get("type")) != "queued_command":
                continue
            prompt = (_optional_string(attachment.get("prompt")) or "").strip()
            attachment_origin = _origin_kind(attachment.get("origin"))
            # Non-human queued commands are also materialized as ``user``
            # records when delivered. Human commands can otherwise exist only
            # in this attachment while the agent is busy.
            if not prompt or attachment_origin != "human":
                continue
            kind, author, author_kind, ingress_kind = _claude_input_provenance(
                record,
                prompt,
                origin_kind=attachment_origin,
                queued=True,
            )
            events.append(
                Event(
                    event_id=_event_id(record, "queued-user"),
                    thread_id=record.thread_id,
                    turn_id=None,
                    timestamp_ms=_record_time(record),
                    kind=kind,
                    role="user",
                    phase=None,
                    text=prompt,
                    content_availability="plain",
                    encrypted_content=None,
                    author=author,
                    recipient=record.thread_id,
                    source_line=record.line,
                    ingress_kind=ingress_kind,
                    author_kind=author_kind,
                    source_native_id=_source_native_id(record),
                    classification_version=_CLASSIFICATION_VERSION,
                )
            )
            continue
        if record_type not in ("user", "assistant"):
            continue
        at_ms = _record_time(record)
        message = _message(record)
        role = _optional_string(message.get("role"))
        where = f"{record.source_path}:{record.line}.message"
        content = message.get("content")
        if record_type == "user":
            raw_origin = _origin_kind(record.value.get("origin"))
            if isinstance(content, str):
                text = content.strip()
                if text:
                    kind, author, author_kind, ingress_kind = (
                        _claude_input_provenance(
                            record,
                            text,
                            origin_kind=raw_origin,
                            queued=False,
                        )
                    )
                    events.append(
                        Event(
                            event_id=_event_id(record, "user"),
                            thread_id=record.thread_id,
                            turn_id=None,
                            timestamp_ms=at_ms,
                            kind=kind,
                            role=role or "user",
                            phase=None,
                            text=text,
                            content_availability="plain",
                            encrypted_content=None,
                            author=author,
                            recipient=record.thread_id,
                            source_line=record.line,
                            ingress_kind=ingress_kind,
                            author_kind=author_kind,
                            source_native_id=_source_native_id(record),
                            classification_version=_CLASSIFICATION_VERSION,
                        )
                    )
                continue
            for index, block in enumerate(_blocks(message, where)):
                block_type = _optional_string(block.get("type"))
                if block_type == "tool_result":
                    call_id = _required_string(
                        block.get("tool_use_id"), where + ".tool_result.tool_use_id"
                    )
                    if call_id in results:
                        raise ClaudeParseError(f"{where}: duplicate result for {call_id}")
                    failed = block.get("is_error") is True
                    results[call_id] = (
                        at_ms,
                        "failed" if failed else "completed",
                        _textual(block.get("content")),
                    )
                elif block_type == "text":
                    text = (_optional_string(block.get("text")) or "").strip()
                    if text:
                        kind, author, author_kind, ingress_kind = (
                            _claude_input_provenance(
                                record,
                                text,
                                origin_kind=raw_origin,
                                queued=False,
                            )
                        )
                        events.append(
                            Event(
                                event_id=_event_id(record, f"user-{index}"),
                                thread_id=record.thread_id,
                                turn_id=None,
                                timestamp_ms=at_ms,
                                kind=kind,
                                role=role or "user",
                                phase=None,
                                text=text,
                                content_availability="plain",
                                encrypted_content=None,
                                author=author,
                                recipient=record.thread_id,
                                source_line=record.line,
                                ingress_kind=ingress_kind,
                                author_kind=author_kind,
                                source_native_id=_source_native_id(record),
                                classification_version=_CLASSIFICATION_VERSION,
                            )
                        )
            continue

        assistant_times.append(at_ms)
        stop_reason = _optional_string(message.get("stop_reason"))
        for index, block in enumerate(_blocks(message, where)):
            block_type = _optional_string(block.get("type"))
            if block_type == "text":
                text = (_optional_string(block.get("text")) or "").strip()
                if text:
                    events.append(
                        Event(
                            event_id=_event_id(record, f"assistant-{index}"),
                            thread_id=record.thread_id,
                            turn_id=None,
                            timestamp_ms=at_ms,
                            kind="assistant_message",
                            role=role or "assistant",
                            phase=(
                                "final_answer"
                                if stop_reason in ("end_turn", "stop_sequence")
                                else "commentary"
                            ),
                            text=text,
                            content_availability="plain",
                            encrypted_content=None,
                            author=record.thread_id,
                            recipient=None,
                            source_line=record.line,
                            ingress_kind="claude",
                            author_kind="agent",
                            source_native_id=_source_native_id(record),
                            classification_version=_CLASSIFICATION_VERSION,
                        )
                    )
            elif block_type == "tool_use":
                call_id = _required_string(block.get("id"), where + ".tool_use.id")
                name = _required_string(block.get("name"), where + ".tool_use.name")
                input_object = _optional_mapping(block.get("input"))
                tools.append(
                    _ToolBuilder(
                        call_id=call_id,
                        item_id=_optional_string(record.value.get("uuid")),
                        thread_id=record.thread_id,
                        started_at_ms=at_ms,
                        name=name,
                        input_text=_textual(block.get("input")),
                        source_line=record.line,
                        input_object=input_object,
                    )
                )
            # Thinking blocks are deliberately excluded from the durable transcript.
    return events, tools, results, assistant_times


def _turns_for_thread(
    thread_id: str,
    events: Sequence[Event],
    tools: Sequence[_ToolBuilder],
    assistant_times: Sequence[int],
) -> tuple[Turn, ...]:
    prompts = sorted(
        (
            event
            for event in events
            if event.kind in ("user_prompt", "system_input")
        ),
        key=lambda event: (event.timestamp_ms, event.event_id),
    )
    activity = [event.timestamp_ms for event in events]
    activity.extend(tool.started_at_ms for tool in tools)
    activity.extend(assistant_times)
    if not prompts:
        return ()
    last_activity = max(activity, default=prompts[-1].timestamp_ms)
    turns: list[Turn] = []
    for index, prompt in enumerate(prompts):
        next_start = (
            prompts[index + 1].timestamp_ms if index + 1 < len(prompts) else None
        )
        upper = next_start - 1 if next_start is not None else last_activity
        responses = sorted(
            (
                event
                for event in events
                if event.kind == "assistant_message"
                and prompt.timestamp_ms <= event.timestamp_ms <= upper
            ),
            key=lambda event: (event.timestamp_ms, event.event_id),
        )
        first_token = min(
            (
                timestamp
                for timestamp in assistant_times
                if prompt.timestamp_ms <= timestamp <= upper
            ),
            default=None,
        )
        ended = max(
            [prompt.timestamp_ms]
            + [event.timestamp_ms for event in responses]
            + [
                tool.started_at_ms
                for tool in tools
                if prompt.timestamp_ms <= tool.started_at_ms <= upper
            ]
        )
        final = next(
            (event for event in reversed(responses) if event.phase == "final_answer"),
            None,
        )
        turns.append(
            Turn(
                turn_id=prompt.event_id,
                thread_id=thread_id,
                started_at_ms=prompt.timestamp_ms,
                ended_at_ms=max(prompt.timestamp_ms + 1, ended),
                status="completed" if final is not None else "unknown",
                first_token_ms=first_token,
                error=None,
                last_agent_message=(responses[-1].text if responses else None),
            )
        )
    return tuple(turns)


def _turn_id(turns: Sequence[Turn], at_ms: int) -> str | None:
    candidates = [turn for turn in turns if turn.started_at_ms <= at_ms]
    if not candidates:
        return None
    return max(candidates, key=lambda turn: turn.started_at_ms).turn_id


def _slug(value: str, fallback: str) -> str:
    normalized = re.sub(r"[^a-z0-9]+", "_", value.lower()).strip("_")
    return (normalized or fallback)[:80]


def _inside(at_ms: int, start_ms: int | None, end_ms: int | None) -> bool:
    return (start_ms is None or at_ms >= start_ms) and (
        end_ms is None or at_ms < end_ms
    )


def _overlaps(
    start: int, end: int | None, window_start: int | None, window_end: int | None
) -> bool:
    effective_end = end if end is not None else start + 1
    return (window_end is None or start < window_end) and (
        window_start is None or effective_end >= window_start
    )


def load_claude_team(
    session_file: Path,
    team_slug: str,
    display_timezone: str,
    *,
    start_ms: int | None = None,
    end_ms: int | None = None,
    source_paths: Sequence[str] | None = None,
) -> TeamData:
    """Normalize one Claude session and its subagents.

    ``start_ms`` is inclusive and ``end_ms`` is exclusive.  Filtering clips
    lifetimes and active intervals while retaining ancestors needed to explain
    an in-window nested agent.
    """

    if start_ms is not None and end_ms is not None and end_ms <= start_ms:
        raise ClaudeParseError("Claude import end_ms must be greater than start_ms")
    root_thread_id = session_file.stem
    _validate_thread_id(root_thread_id, str(session_file))
    relative_sources = _relative_sources(session_file, source_paths)
    records_by_thread: dict[str, list[_Record]] = {}
    snapshots: list[SourceSnapshot] = []
    agent_by_log: dict[str, str] = {}
    meta_paths: list[tuple[str, Path]] = []
    for relative, path in relative_sources:
        if path.name.endswith(".meta.json"):
            meta_paths.append((relative, path))
            continue
        is_root = relative == session_file.name
        records, snapshot, thread_id = _read_records(
            path, relative, root_thread_id, is_root
        )
        records_by_thread.setdefault(thread_id, []).extend(records)
        snapshots.append(snapshot)
        agent_by_log[path.with_suffix("").as_posix()] = thread_id
    if root_thread_id not in records_by_thread:
        raise ClaudeParseError("Claude root transcript did not identify the root session")

    metas: dict[str, _Meta] = {}
    for relative, path in meta_paths:
        log_key = path.with_suffix("").with_suffix("").as_posix()
        agent_id = agent_by_log.get(log_key)
        if agent_id is None:
            candidate = path.name.removeprefix("agent-").removesuffix(".meta.json")
            agent_id = candidate
        meta, snapshot = _read_meta(path, relative, agent_id)
        metas[agent_id] = meta
        snapshots.append(snapshot)

    all_events: list[Event] = []
    tool_builders: list[_ToolBuilder] = []
    result_by_call: dict[str, tuple[int, str, str | None]] = {}
    assistant_by_thread: dict[str, list[int]] = {}
    for thread_id, thread_records in records_by_thread.items():
        events, tools, results, assistant_times = _extract(thread_records)
        all_events.extend(events)
        tool_builders.extend(tools)
        assistant_by_thread[thread_id] = assistant_times
        for call_id, extracted_result in results.items():
            if call_id in result_by_call:
                raise ClaudeParseError(f"duplicate Claude tool result {call_id}")
            result_by_call[call_id] = extracted_result
    if len({tool.call_id for tool in tool_builders}) != len(tool_builders):
        raise ClaudeParseError("duplicate Claude tool-use id")

    turns_by_thread: dict[str, tuple[Turn, ...]] = {}
    for thread_id in records_by_thread:
        turns_by_thread[thread_id] = _turns_for_thread(
            thread_id,
            [event for event in all_events if event.thread_id == thread_id],
            [tool for tool in tool_builders if tool.thread_id == thread_id],
            assistant_by_thread.get(thread_id, ()),
        )
    all_events = [
        replace(
            event,
            turn_id=_turn_id(turns_by_thread.get(event.thread_id, ()), event.timestamp_ms),
        )
        for event in all_events
    ]
    all_tools: list[ToolCall] = []
    tool_inputs: dict[str, Mapping[str, object]] = {}
    for tool_builder in tool_builders:
        tool_result = result_by_call.get(tool_builder.call_id)
        namespace = (
            tool_builder.name.split("__", 1)[0]
            if "__" in tool_builder.name
            else None
        )
        all_tools.append(
            ToolCall(
                call_id=tool_builder.call_id,
                item_id=tool_builder.item_id,
                thread_id=tool_builder.thread_id,
                turn_id=_turn_id(
                    turns_by_thread.get(tool_builder.thread_id, ()),
                    tool_builder.started_at_ms,
                ),
                name=tool_builder.name,
                namespace=namespace,
                started_at_ms=tool_builder.started_at_ms,
                ended_at_ms=tool_result[0] if tool_result is not None else None,
                status=tool_result[1] if tool_result is not None else "running",
                input_text=tool_builder.input_text,
                output_text=tool_result[2] if tool_result is not None else None,
                nested_tools=(),
                source_line=tool_builder.source_line,
            )
        )
        tool_inputs[tool_builder.call_id] = tool_builder.input_object

    spawn_by_agent: dict[str, _ToolBuilder] = {}
    meta_by_tool = {
        meta.tool_use_id: meta
        for meta in metas.values()
        if meta.tool_use_id is not None
    }
    for tool in tool_builders:
        agent_meta = meta_by_tool.get(tool.call_id)
        if tool.name == "Agent" and agent_meta is not None:
            spawn_by_agent[agent_meta.agent_id] = tool

    parent_by_agent: dict[str, str | None] = {root_thread_id: None}
    description_by_agent: dict[str, str] = {root_thread_id: "root"}
    role_by_agent: dict[str, str | None] = {root_thread_id: "coordinator"}
    for agent_id in records_by_thread:
        if agent_id == root_thread_id:
            continue
        agent_meta = metas.get(agent_id)
        spawn = spawn_by_agent.get(agent_id)
        parent = agent_meta.parent_agent_id if agent_meta is not None else None
        if parent is None and spawn is not None:
            parent = spawn.thread_id
        if parent not in records_by_thread:
            parent = root_thread_id
        parent_by_agent[agent_id] = parent
        description = agent_meta.description if agent_meta is not None else None
        if description is None and spawn is not None:
            description = _optional_string(spawn.input_object.get("description"))
        description_by_agent[agent_id] = description or agent_id
        role_by_agent[agent_id] = (
            agent_meta.agent_type if agent_meta is not None else None
        )

    # Subagent transcript files present parent instructions as user records and
    # completed work as assistant records. Normalize both sides of that route so
    # archive statistics count only root-thread prompts as human input.
    routed_events: list[Event] = []
    for event in all_events:
        parent = parent_by_agent.get(event.thread_id)
        if parent is None:
            routed_events.append(event)
        elif event.kind == "user_prompt":
            routed_events.append(
                replace(
                    event,
                    kind="inter_agent_message",
                    role=None,
                    phase="instruction",
                    author=parent,
                    recipient=event.thread_id,
                    ingress_kind="claude_subagent",
                    author_kind="agent",
                )
            )
        elif event.kind == "assistant_message" and event.phase == "final_answer":
            routed_events.append(
                replace(
                    event,
                    kind="inter_agent_message",
                    role=None,
                    author=event.thread_id,
                    recipient=parent,
                    ingress_kind="claude_subagent",
                    author_kind="agent",
                )
            )
        else:
            routed_events.append(event)
    for tool in tool_builders:
        if tool.name != "SendMessage":
            continue
        recipient = _optional_string(
            tool.input_object.get("recipient")
        ) or _optional_string(tool.input_object.get("to"))
        message = _optional_string(
            tool.input_object.get("message")
        ) or _optional_string(tool.input_object.get("prompt"))
        if recipient not in records_by_thread or message is None:
            continue
        already_recorded = any(
            event.kind == "inter_agent_message"
            and event.author == tool.thread_id
            and event.recipient == recipient
            and (event.text or "").strip() == message.strip()
            and abs(event.timestamp_ms - tool.started_at_ms) <= 5_000
            for event in routed_events
        )
        if already_recorded:
            continue
        routed_events.append(
            Event(
                event_id=f"claude-message-{tool.call_id}",
                thread_id=tool.thread_id,
                turn_id=_turn_id(
                    turns_by_thread.get(tool.thread_id, ()), tool.started_at_ms
                ),
                timestamp_ms=tool.started_at_ms,
                kind="inter_agent_message",
                role=None,
                phase="instruction",
                text=message,
                content_availability="plain",
                encrypted_content=None,
                author=tool.thread_id,
                recipient=recipient,
                source_line=tool.source_line,
                ingress_kind="claude_send_message",
                author_kind="agent",
                source_native_id=tool.call_id,
                classification_version=_CLASSIFICATION_VERSION,
            )
        )
    all_events = routed_events

    raw_activity: dict[str, list[int]] = {thread_id: [] for thread_id in records_by_thread}
    for event in all_events:
        raw_activity[event.thread_id].append(event.timestamp_ms)
    for normalized_tool in all_tools:
        raw_activity[normalized_tool.thread_id].append(normalized_tool.started_at_ms)
        if normalized_tool.ended_at_ms is not None:
            raw_activity[normalized_tool.thread_id].append(normalized_tool.ended_at_ms)
    for agent_id, spawn in spawn_by_agent.items():
        raw_activity.setdefault(agent_id, []).append(spawn.started_at_ms)

    included = {
        thread_id
        for thread_id, times in raw_activity.items()
        if any(_inside(timestamp, start_ms, end_ms) for timestamp in times)
    }
    if root_thread_id not in included and any(included):
        included.add(root_thread_id)
    for agent_id in tuple(included):
        parent = parent_by_agent.get(agent_id)
        seen: set[str] = set()
        while parent is not None:
            if parent in seen:
                raise ClaudeParseError("cycle in Claude subagent parent metadata")
            seen.add(parent)
            included.add(parent)
            parent = parent_by_agent.get(parent)
    if not included:
        raise ClaudeParseError("requested Claude window contains no transcript activity")

    segment_by_agent: dict[str, str] = {}
    used_paths: set[str] = set()

    def agent_path(agent_id: str) -> str:
        existing = segment_by_agent.get(agent_id)
        if existing is not None:
            return existing
        parent = parent_by_agent.get(agent_id)
        if parent is None:
            segment_by_agent[agent_id] = "/root"
            return "/root"
        base = agent_path(parent)
        leaf = _slug(description_by_agent[agent_id], agent_id)
        candidate = f"{base}/{leaf}"
        if candidate in used_paths:
            candidate = f"{candidate}_{agent_id[-6:]}"
        used_paths.add(candidate)
        segment_by_agent[agent_id] = candidate
        return candidate

    agents: list[Agent] = []
    for thread_id in sorted(included, key=lambda item: (item != root_thread_id, item)):
        times = raw_activity.get(thread_id, [])
        if not times:
            inherited = start_ms if start_ms is not None else 0
            times = [inherited]
        original_start = min(times)
        # Agent lifetimes are half-open. Include activity at the final observed
        # millisecond instead of clipping the terminal event out of its phase.
        original_end = max(times) + 1
        clipped_start = max(original_start, start_ms) if start_ms is not None else original_start
        clipped_end = min(original_end, end_ms) if end_ms is not None else original_end
        if clipped_end <= clipped_start:
            clipped_end = clipped_start + 1
        depth = agent_path(thread_id).count("/") - 1
        source_path = next(
            snapshot.path
            for snapshot in snapshots
            if snapshot.thread_id == thread_id and snapshot.path.endswith(".jsonl")
        )
        agents.append(
            Agent(
                thread_id=thread_id,
                parent_thread_id=parent_by_agent.get(thread_id),
                agent_path=agent_path(thread_id),
                nickname=None,
                role=role_by_agent.get(thread_id),
                depth=depth,
                started_at_ms=clipped_start,
                ended_at_ms=clipped_end,
                status=(
                    "completed"
                    if any(
                        turn.status == "completed"
                        for turn in turns_by_thread.get(thread_id, ())
                    )
                    else "unknown"
                ),
                source_path=source_path,
            )
        )

    filtered_events = tuple(
        sorted(
            (
                event
                for event in all_events
                if event.thread_id in included
                and _inside(event.timestamp_ms, start_ms, end_ms)
            ),
            key=lambda event: (event.timestamp_ms, event.event_id),
        )
    )
    filtered_tools = tuple(
        sorted(
            (
                replace(
                    tool,
                    started_at_ms=(
                        max(tool.started_at_ms, start_ms)
                        if start_ms is not None
                        else tool.started_at_ms
                    ),
                    ended_at_ms=(
                        min(tool.ended_at_ms, end_ms)
                        if tool.ended_at_ms is not None and end_ms is not None
                        else tool.ended_at_ms
                    ),
                )
                for tool in all_tools
                if tool.thread_id in included
                and _overlaps(tool.started_at_ms, tool.ended_at_ms, start_ms, end_ms)
            ),
            key=lambda tool: (tool.started_at_ms, tool.call_id),
        )
    )
    turns = tuple(
        sorted(
            (
                replace(
                    turn,
                    started_at_ms=(
                        max(turn.started_at_ms, start_ms)
                        if start_ms is not None
                        else turn.started_at_ms
                    ),
                    ended_at_ms=(
                        min(turn.ended_at_ms, end_ms)
                        if turn.ended_at_ms is not None and end_ms is not None
                        else turn.ended_at_ms
                    ),
                )
                for thread_id, thread_turns in turns_by_thread.items()
                if thread_id in included
                for turn in thread_turns
                if _overlaps(turn.started_at_ms, turn.ended_at_ms, start_ms, end_ms)
            ),
            key=lambda turn: (turn.started_at_ms, turn.turn_id),
        )
    )

    edges: list[Edge] = []
    for agent_id in sorted(included):
        if agent_id == root_thread_id:
            continue
        parent = parent_by_agent.get(agent_id) or root_thread_id
        spawn = spawn_by_agent.get(agent_id)
        raw_at = spawn.started_at_ms if spawn is not None else min(raw_activity[agent_id])
        if end_ms is not None and raw_at >= end_ms:
            continue
        at_ms = max(raw_at, start_ms) if start_ms is not None else raw_at
        call_id = spawn.call_id if spawn is not None else f"claude-spawn-{agent_id}"
        prompt = (
            _optional_string(spawn.input_object.get("prompt"))
            if spawn is not None
            else description_by_agent[agent_id]
        )
        edges.append(
            Edge(
                edge_id=f"claude-spawn-{call_id}",
                call_id=call_id,
                from_thread_id=parent,
                to_thread_id=agent_id,
                kind="spawn",
                timestamp_ms=at_ms,
                message_text=prompt,
                content_availability="plain",
                encrypted_content=None,
                source_line=spawn.source_line if spawn is not None else 1,
            )
        )
    for tool in tool_builders:
        if tool.name != "SendMessage" or tool.thread_id not in included:
            continue
        recipient = _optional_string(tool.input_object.get("recipient")) or _optional_string(
            tool.input_object.get("to")
        )
        if recipient not in included or not _inside(tool.started_at_ms, start_ms, end_ms):
            continue
        message = _optional_string(tool.input_object.get("message")) or _optional_string(
            tool.input_object.get("prompt")
        )
        edges.append(
            Edge(
                edge_id=f"claude-message-{tool.call_id}",
                call_id=tool.call_id,
                from_thread_id=tool.thread_id,
                to_thread_id=recipient,
                kind="followup",
                timestamp_ms=tool.started_at_ms,
                message_text=message,
                content_availability="plain",
                encrypted_content=None,
                source_line=tool.source_line,
            )
        )

    if not events and not tools:
        raise ClaudeParseError("requested Claude window contains no renderable activity")
    return TeamData(
        team_slug=team_slug,
        provider="claude",
        root_thread_id=root_thread_id,
        display_timezone=display_timezone,
        sources=tuple(sorted(snapshots, key=lambda source: source.path)),
        agents=tuple(sorted(agents, key=lambda agent: (agent.depth, agent.started_at_ms, agent.thread_id))),
        turns=turns,
        events=filtered_events,
        tool_calls=filtered_tools,
        edges=tuple(sorted(edges, key=lambda edge: (edge.timestamp_ms, edge.edge_id))),
    )


def _source_thread_id(path: Path, root_id: str) -> str:
    if path.name == f"{root_id}.jsonl":
        return root_id
    candidate = path.name.removeprefix("agent-")
    if candidate.endswith(".meta.json"):
        candidate = candidate.removesuffix(".meta.json")
    elif candidate.endswith(".jsonl"):
        candidate = candidate.removesuffix(".jsonl")
    _validate_thread_id(candidate, str(path))
    return candidate


def snapshot_claude_lineage(
    session_file: Path,
    snapshot_root: Path,
    previous_sources: Sequence[ClaudeSourceCopy],
    updated_at: str,
) -> ClaudeSnapshotResult:
    """Copy the latest append-only Claude lineage before parsing it."""

    source_root = session_file.parent
    paths = discover_claude_sources(session_file)
    relative_to_path = {
        path.relative_to(source_root).as_posix(): path for path in paths
    }
    previous_by_path = {source.source_path: source for source in previous_sources}
    missing = sorted(set(previous_by_path) - set(relative_to_path))
    if missing:
        raise ClaudeParseError(
            "append-only source violation: previously observed Claude source disappeared: "
            + ", ".join(missing[:3])
        )
    changed = 0
    copies: list[ClaudeSourceCopy] = []
    root_id = session_file.stem
    for relative, source in sorted(relative_to_path.items()):
        _safe_relative(relative)
        data = source.read_bytes()
        complete = _complete_jsonl(data) if source.suffix == ".jsonl" else data
        target = snapshot_root.joinpath(*PurePosixPath(relative).parts)
        if target.is_symlink():
            raise ClaudeParseError(f"Claude snapshot target is a symlink: {target}")
        existing = target.read_bytes() if target.is_file() else b""
        previous = previous_by_path.get(relative)
        if not complete:
            if previous is not None or existing:
                raise ClaudeParseError(
                    "append-only source violation: Claude transcript was truncated or "
                    f"rewritten: {relative}"
                )
            raise ClaudeParseError(f"Claude source has no complete JSON: {source}")
        if previous is not None:
            if len(existing) < previous.copied_bytes:
                raise ClaudeParseError(f"Claude snapshot is shorter than its manifest: {target}")
            if hashlib.sha256(existing[: previous.copied_bytes]).hexdigest() != previous.sha256:
                raise ClaudeParseError(f"Claude snapshot differs from its manifest: {target}")
        if source.name.endswith(".meta.json"):
            if existing and existing != complete:
                raise ClaudeParseError(
                    f"append-only source violation: Claude agent metadata changed: {relative}"
                )
        elif len(complete) < len(existing) or complete[: len(existing)] != existing:
            raise ClaudeParseError(
                f"append-only source violation: Claude transcript was truncated or rewritten: {relative}"
            )
        text = complete.decode("utf-8")
        changed += int(write_text_if_changed(target, text))
        digest = hashlib.sha256(complete).hexdigest()
        copy_updated = (
            previous.updated_at
            if previous is not None
            and previous.copied_bytes == len(complete)
            and previous.sha256 == digest
            else updated_at
        )
        copies.append(
            ClaudeSourceCopy(
                source_path=relative,
                original_path=str(source.resolve()),
                snapshot_path=relative,
                thread_id=_source_thread_id(source, root_id),
                copied_bytes=len(complete),
                line_count=max(1, complete.count(b"\n")),
                sha256=digest,
                updated_at=copy_updated,
            )
        )
    return ClaudeSnapshotResult(tuple(copies), changed)


__all__ = [
    "ClaudeParseError",
    "ClaudeSnapshotResult",
    "ClaudeSourceCopy",
    "discover_claude_sources",
    "load_claude_team",
    "snapshot_claude_lineage",
]
