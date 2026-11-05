"""Strict loading of the normalized provider-neutral archive snapshot."""

from __future__ import annotations

from collections.abc import Mapping, Sequence

from wrkviz.model import (
    Agent,
    Edge,
    Event,
    PayloadRef,
    SourceSnapshot,
    TaskNote,
    TeamData,
    ToolCall,
    Turn,
)


def _object(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ValueError(f"{where}: expected an object")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise ValueError(f"{where}: object key is not a string")
        result[key] = item
    return result


def _array(value: object, where: str) -> list[object]:
    if not isinstance(value, list):
        raise ValueError(f"{where}: expected an array")
    return list(value)


def _string(value: object, where: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{where}: expected a string")
    return value


def _optional_string(value: object, where: str) -> str | None:
    if value is None:
        return None
    return _string(value, where)


def _integer(value: object, where: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{where}: expected an integer")
    return value


def _optional_integer(value: object, where: str) -> int | None:
    if value is None:
        return None
    return _integer(value, where)


def _items(root: Mapping[str, object], key: str) -> list[dict[str, object]]:
    return [
        _object(value, f"team.{key}[{index}]")
        for index, value in enumerate(_array(root.get(key), f"team.{key}"))
    ]


def _payload_ref(value: object, where: str) -> PayloadRef | None:
    """Decode a tool call's payload reference, absent in every pre-payload archive.

    Strict about the field set for the same reason ``task_note_from_json_obj`` is: unlike the rest
    of a tool call, the thing this points at is *not* re-derivable from ``raw/team.json``, and
    once the vendor snapshots are gone it is not re-derivable at all. An unknown key means the
    writer recorded something about the payload that this reader would carry forward without
    understanding, and for a reference into a store that is the archive's only copy, that is worth
    a refusal rather than a shrug.
    """

    if value is None:
        return None
    item = _object(value, where)
    if set(item) != {"sha256", "byte_length"}:
        raise ValueError(f"{where}: invalid payload reference fields: {sorted(item)!r}")
    byte_length = _integer(item.get("byte_length"), f"{where}.byte_length")
    if byte_length < 0:
        raise ValueError(f"{where}.byte_length: expected a non-negative integer")
    return PayloadRef(
        sha256=_string(item.get("sha256"), f"{where}.sha256"), byte_length=byte_length
    )


def task_note_from_json_obj(value: object, where: str) -> TaskNote:
    """Strictly decode one promoted task note.

    Stricter than its neighbours in this module on purpose. The other record families are
    rewritten wholesale from their source snapshots on every ingest, so a decode that quietly
    accepted something wrong would be corrected by the next run. A task note is not rewritten:
    once promoted it is carried forward from this file and nowhere else, so a field this decoder
    lets through unexamined is a field nothing downstream will ever re-derive. Hence the exact
    key set -- an unknown key means the writer knew something this reader does not, and merging
    that record forward would silently drop it.
    """

    item = _object(value, where)
    expected = {
        "note_id",
        "source_path",
        "task_source_ordinal",
        "task_id",
        "title",
        "content",
        "created_at",
        "server_author",
        "task_owner",
        "upstream_present",
        "projection_policy",
        "projection_sha256",
    }
    if set(item) != expected:
        missing = sorted(expected - set(item))
        unknown = sorted(set(item) - expected)
        raise ValueError(
            f"{where}: invalid task note fields: missing={missing!r}, unknown={unknown!r}"
        )
    upstream_present = item.get("upstream_present")
    if not isinstance(upstream_present, bool):
        raise ValueError(f"{where}.upstream_present: expected a boolean")
    note_id = _integer(item.get("note_id"), f"{where}.note_id")
    if note_id < 0:
        raise ValueError(f"{where}.note_id: expected a non-negative integer")
    ordinal = _integer(item.get("task_source_ordinal"), f"{where}.task_source_ordinal")
    if ordinal < 0:
        raise ValueError(f"{where}.task_source_ordinal: expected a non-negative integer")
    return TaskNote(
        note_id=note_id,
        source_path=_string(item.get("source_path"), f"{where}.source_path"),
        task_source_ordinal=ordinal,
        task_id=_string(item.get("task_id"), f"{where}.task_id"),
        title=_string(item.get("title"), f"{where}.title"),
        content=_string(item.get("content"), f"{where}.content"),
        created_at=_string(item.get("created_at"), f"{where}.created_at"),
        server_author=_optional_string(
            item.get("server_author"), f"{where}.server_author"
        ),
        task_owner=_optional_string(item.get("task_owner"), f"{where}.task_owner"),
        upstream_present=upstream_present,
        projection_policy=_optional_string(
            item.get("projection_policy"), f"{where}.projection_policy"
        ),
        projection_sha256=_optional_string(
            item.get("projection_sha256"), f"{where}.projection_sha256"
        ),
    )


def team_from_json_obj(value: object) -> TeamData:
    """Reconstruct :class:`TeamData` written by ``TeamData.to_json_obj``."""

    root = _object(value, "team")
    sources = tuple(
        SourceSnapshot(
            path=_string(item.get("path"), "source.path"),
            thread_id=_string(item.get("thread_id"), "source.thread_id"),
            size_bytes=_integer(item.get("size_bytes"), "source.size_bytes"),
            mtime_ns=_integer(item.get("mtime_ns"), "source.mtime_ns"),
            sha256=_string(item.get("sha256"), "source.sha256"),
            complete_bytes=_integer(item.get("complete_bytes"), "source.complete_bytes"),
            line_count=_integer(item.get("line_count"), "source.line_count"),
            working_directory=_optional_string(
                item.get("working_directory"), "source.working_directory"
            ),
            repository_url=_optional_string(
                item.get("repository_url"), "source.repository_url"
            ),
            semantic_sha256=_optional_string(
                item.get("semantic_sha256"), "source.semantic_sha256"
            ),
            semantic_complete_bytes=_optional_integer(
                item.get("semantic_complete_bytes"),
                "source.semantic_complete_bytes",
            ),
        )
        for item in _items(root, "sources")
    )
    agents = tuple(
        Agent(
            thread_id=_string(item.get("thread_id"), "agent.thread_id"),
            parent_thread_id=_optional_string(
                item.get("parent_thread_id"), "agent.parent_thread_id"
            ),
            agent_path=_string(item.get("agent_path"), "agent.agent_path"),
            nickname=_optional_string(item.get("nickname"), "agent.nickname"),
            role=_optional_string(item.get("role"), "agent.role"),
            depth=_integer(item.get("depth"), "agent.depth"),
            started_at_ms=_integer(item.get("started_at_ms"), "agent.started_at_ms"),
            ended_at_ms=_optional_integer(item.get("ended_at_ms"), "agent.ended_at_ms"),
            status=_string(item.get("status"), "agent.status"),
            source_path=_string(item.get("source_path"), "agent.source_path"),
        )
        for item in _items(root, "agents")
    )
    turns = tuple(
        Turn(
            turn_id=_string(item.get("turn_id"), "turn.turn_id"),
            thread_id=_string(item.get("thread_id"), "turn.thread_id"),
            started_at_ms=_integer(item.get("started_at_ms"), "turn.started_at_ms"),
            ended_at_ms=_optional_integer(item.get("ended_at_ms"), "turn.ended_at_ms"),
            status=_string(item.get("status"), "turn.status"),
            first_token_ms=_optional_integer(item.get("first_token_ms"), "turn.first_token_ms"),
            error=_optional_string(item.get("error"), "turn.error"),
            last_agent_message=_optional_string(
                item.get("last_agent_message"), "turn.last_agent_message"
            ),
        )
        for item in _items(root, "turns")
    )
    events = tuple(
        Event(
            event_id=_string(item.get("event_id"), "event.event_id"),
            thread_id=_string(item.get("thread_id"), "event.thread_id"),
            turn_id=_optional_string(item.get("turn_id"), "event.turn_id"),
            timestamp_ms=_integer(item.get("timestamp_ms"), "event.timestamp_ms"),
            kind=_string(item.get("kind"), "event.kind"),
            role=_optional_string(item.get("role"), "event.role"),
            phase=_optional_string(item.get("phase"), "event.phase"),
            text=_optional_string(item.get("text"), "event.text"),
            content_availability=_string(
                item.get("content_availability"), "event.content_availability"
            ),
            encrypted_content=_optional_string(
                item.get("encrypted_content"), "event.encrypted_content"
            ),
            author=_optional_string(item.get("author"), "event.author"),
            recipient=_optional_string(item.get("recipient"), "event.recipient"),
            source_line=_integer(item.get("source_line"), "event.source_line"),
            ingress_kind=_optional_string(
                item.get("ingress_kind"), "event.ingress_kind"
            ),
            author_kind=_optional_string(item.get("author_kind"), "event.author_kind"),
            source_native_id=_optional_string(
                item.get("source_native_id"), "event.source_native_id"
            ),
            classification_version=_optional_string(
                item.get("classification_version"), "event.classification_version"
            ),
        )
        for item in _items(root, "events")
    )
    tools: list[ToolCall] = []
    for item in _items(root, "tool_calls"):
        nested = tuple(
            (
                _string(entry.get("name"), "tool.nested_tools.name"),
                _integer(entry.get("count"), "tool.nested_tools.count"),
            )
            for entry in (
                _object(value, "tool.nested_tools[]")
                for value in _array(item.get("nested_tools"), "tool.nested_tools")
            )
        )
        tools.append(
            ToolCall(
                call_id=_string(item.get("call_id"), "tool.call_id"),
                item_id=_optional_string(item.get("item_id"), "tool.item_id"),
                thread_id=_string(item.get("thread_id"), "tool.thread_id"),
                turn_id=_optional_string(item.get("turn_id"), "tool.turn_id"),
                name=_string(item.get("name"), "tool.name"),
                namespace=_optional_string(item.get("namespace"), "tool.namespace"),
                started_at_ms=_integer(item.get("started_at_ms"), "tool.started_at_ms"),
                ended_at_ms=_optional_integer(item.get("ended_at_ms"), "tool.ended_at_ms"),
                status=_string(item.get("status"), "tool.status"),
                input_text=_optional_string(item.get("input_text"), "tool.input_text"),
                output_text=_optional_string(item.get("output_text"), "tool.output_text"),
                nested_tools=nested,
                source_line=_integer(item.get("source_line"), "tool.source_line"),
                input_payload=_payload_ref(
                    item.get("input_payload"), "tool.input_payload"
                ),
                output_payload=_payload_ref(
                    item.get("output_payload"), "tool.output_payload"
                ),
            )
        )
    edges = tuple(
        Edge(
            edge_id=_string(item.get("edge_id"), "edge.edge_id"),
            call_id=_string(item.get("call_id"), "edge.call_id"),
            from_thread_id=_string(item.get("from_thread_id"), "edge.from_thread_id"),
            to_thread_id=_string(item.get("to_thread_id"), "edge.to_thread_id"),
            kind=_string(item.get("kind"), "edge.kind"),
            timestamp_ms=_integer(item.get("timestamp_ms"), "edge.timestamp_ms"),
            message_text=_optional_string(item.get("message_text"), "edge.message_text"),
            content_availability=_string(
                item.get("content_availability"), "edge.content_availability"
            ),
            encrypted_content=_optional_string(
                item.get("encrypted_content"), "edge.encrypted_content"
            ),
            source_line=_integer(item.get("source_line"), "edge.source_line"),
        )
        for item in _items(root, "edges")
    )
    # Absent, not empty, in every archive written before task notes became a record family, and
    # absent again in every archive written after -- the ingest writer keeps them in
    # `raw/task-notes.jsonl` and reattaches them. Decoding the key when it *is* present keeps
    # `TeamData.to_json_obj` round-trippable, which the model tests rely on and which a caller
    # serializing a complete team in memory has every right to expect.
    task_notes = tuple(
        task_note_from_json_obj(item, f"team.task_notes[{index}]")
        for index, item in enumerate(
            _array(root.get("task_notes", []), "team.task_notes")
        )
    )
    return TeamData(
        team_slug=_string(root.get("team_slug"), "team.team_slug"),
        provider=_string(root.get("provider"), "team.provider"),
        root_thread_id=_string(root.get("root_thread_id"), "team.root_thread_id"),
        display_timezone=_string(root.get("display_timezone"), "team.display_timezone"),
        sources=sources,
        agents=agents,
        turns=turns,
        events=events,
        tool_calls=tuple(tools),
        edges=edges,
        task_notes=task_notes,
        window_start_ms=_optional_integer(
            root.get("window_start_ms"), "team.window_start_ms"
        ),
        window_end_ms=_optional_integer(root.get("window_end_ms"), "team.window_end_ms"),
    )


__all__ = ["task_note_from_json_obj", "team_from_json_obj"]
