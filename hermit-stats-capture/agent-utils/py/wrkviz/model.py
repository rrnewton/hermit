"""Typed, JSON-serializable data model for agent-team timelines."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass

from wrkviz.archive import JsonValue, canonical_json


JsonObject = dict[str, object]


@dataclass(frozen=True)
class SourceSnapshot:
    """The exact complete-line snapshot consumed from one live rollout file."""

    path: str
    thread_id: str
    size_bytes: int
    mtime_ns: int
    sha256: str
    complete_bytes: int
    line_count: int
    working_directory: str | None = None
    repository_url: str | None = None
    semantic_sha256: str | None = None
    semantic_complete_bytes: int | None = None

    def to_json_obj(self) -> JsonObject:
        """Return the snapshot as a JSON-serializable object."""

        return {
            "path": self.path,
            "thread_id": self.thread_id,
            "size_bytes": self.size_bytes,
            "mtime_ns": self.mtime_ns,
            "sha256": self.sha256,
            "complete_bytes": self.complete_bytes,
            "line_count": self.line_count,
            "working_directory": self.working_directory,
            "repository_url": self.repository_url,
            "semantic_sha256": self.semantic_sha256,
            "semantic_complete_bytes": self.semantic_complete_bytes,
        }


@dataclass(frozen=True)
class Agent:
    """One coordinator or subagent thread in a Codex team lineage."""

    thread_id: str
    parent_thread_id: str | None
    agent_path: str
    nickname: str | None
    role: str | None
    depth: int
    started_at_ms: int
    ended_at_ms: int | None
    status: str
    source_path: str

    def to_json_obj(self) -> JsonObject:
        """Return the agent record as a JSON-serializable object."""

        return {
            "thread_id": self.thread_id,
            "parent_thread_id": self.parent_thread_id,
            "agent_path": self.agent_path,
            "nickname": self.nickname,
            "role": self.role,
            "depth": self.depth,
            "started_at_ms": self.started_at_ms,
            "ended_at_ms": self.ended_at_ms,
            "status": self.status,
            "source_path": self.source_path,
        }


@dataclass(frozen=True)
class Turn:
    """A model turn, including its authoritative lifecycle boundary."""

    turn_id: str
    thread_id: str
    started_at_ms: int
    ended_at_ms: int | None
    status: str
    first_token_ms: int | None
    error: str | None
    last_agent_message: str | None

    def to_json_obj(self) -> JsonObject:
        """Return the turn as a JSON-serializable object."""

        return {
            "turn_id": self.turn_id,
            "thread_id": self.thread_id,
            "started_at_ms": self.started_at_ms,
            "ended_at_ms": self.ended_at_ms,
            "status": self.status,
            "first_token_ms": self.first_token_ms,
            "error": self.error,
            "last_agent_message": self.last_agent_message,
        }


@dataclass(frozen=True)
class Event:
    """Canonical transcript or lifecycle event (UI duplicate records excluded)."""

    event_id: str
    thread_id: str
    turn_id: str | None
    timestamp_ms: int
    kind: str
    role: str | None
    phase: str | None
    text: str | None
    content_availability: str
    encrypted_content: str | None
    author: str | None
    recipient: str | None
    source_line: int
    ingress_kind: str | None = None
    author_kind: str | None = None
    source_native_id: str | None = None
    classification_version: str | None = None

    def to_json_obj(self) -> JsonObject:
        """Return the event as a JSON-serializable object."""

        return {
            "event_id": self.event_id,
            "thread_id": self.thread_id,
            "turn_id": self.turn_id,
            "timestamp_ms": self.timestamp_ms,
            "kind": self.kind,
            "role": self.role,
            "phase": self.phase,
            "text": self.text,
            "content_availability": self.content_availability,
            "encrypted_content": self.encrypted_content,
            "author": self.author,
            "recipient": self.recipient,
            "source_line": self.source_line,
            "ingress_kind": self.ingress_kind,
            "author_kind": self.author_kind,
            "source_native_id": self.source_native_id,
            "classification_version": self.classification_version,
        }


@dataclass(frozen=True)
class PayloadRef:
    """Where a tool call's bulk text went, and how much of it there was.

    The archived tool call does not carry its command arguments or its stdout: they are the
    largest and most sensitive content in the archive, they live in the gitignored payload tree
    described in :mod:`wrkviz.payloads`, and the whole point of putting them there is
    that they can be pruned, permissioned or moved independently of the model.

    So this is what stays behind, and the byte count is the load-bearing half of it. A digest
    alone would let a reader *find* the text but say nothing when the text is gone; with the
    length, an archive whose payload tree has been pruned still reports exactly what it no longer
    holds. That is strictly more than the silent ``null`` this replaces, which was indistinguishable
    from a tool call that genuinely produced no output.

    **It is not free, and the price was chosen deliberately.** In the archive's indented canonical
    JSON a reference costs about 148 bytes, so a team with 38,130 tool calls pays 11.3 MB and its
    ``team.json`` grows 27%, from 41.6 MB to 52.9 MB. Two cheaper shapes were considered and
    rejected. Packing the pair into one string, ``"<sha256>:<length>"``, would save roughly 60% --
    and would be the same mistake ``TaskNote`` exists to undo, where a rendering packed two fields
    into one and the archive lost the ability to read either back cleanly; the saving is 6 MB
    against a 1.07 GB corpus and not worth reintroducing a parse. Truncating the digest would save
    about as much and would make the content address no longer the content address, so a payload
    could not be looked up by hashing it and the store's shard-per-digest-prefix layout would need
    a second, weaker key. 27% of a small tracked file to stop losing 210 MB of command output per
    team is the right side of that trade, and it is stated here so nobody has to rediscover it.
    """

    sha256: str
    byte_length: int

    def to_json_obj(self) -> JsonObject:
        """Return the payload reference as a JSON-serializable object."""

        return {"sha256": self.sha256, "byte_length": self.byte_length}


@dataclass(frozen=True)
class ToolCall:
    """One model tool invocation joined to its output by Codex ``call_id``."""

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
    # Set by the ingest writer when it detaches the text above into the payload tree, and unset on
    # a tool call still holding its text in memory. The two are deliberately not mutually
    # exclusive in the type: a provider reader produces text and no reference, the archived copy
    # carries a reference and no text, and `pipeline.rehydrate_tool_payloads` produces both.
    input_payload: PayloadRef | None = None
    output_payload: PayloadRef | None = None

    def to_json_obj(self) -> JsonObject:
        """Return the tool call as a JSON-serializable object."""

        nested: list[JsonObject] = [
            {"name": name, "count": count} for name, count in self.nested_tools
        ]
        result: JsonObject = {
            "call_id": self.call_id,
            "item_id": self.item_id,
            "thread_id": self.thread_id,
            "turn_id": self.turn_id,
            "name": self.name,
            "namespace": self.namespace,
            "started_at_ms": self.started_at_ms,
            "ended_at_ms": self.ended_at_ms,
            "status": self.status,
            "input_text": self.input_text,
            "output_text": self.output_text,
            "nested_tools": nested,
            "source_line": self.source_line,
        }
        # Emitted only when set, for the same reason `TeamData.task_notes` is: an archive written
        # before payloads existed stays byte-identical until its next ingest, so introducing this
        # record does not rewrite every `team.json` in every archive for no information.
        if self.input_payload is not None:
            result["input_payload"] = self.input_payload.to_json_obj()
        if self.output_payload is not None:
            result["output_payload"] = self.output_payload.to_json_obj()
        return result


@dataclass(frozen=True)
class Edge:
    """A spawn or later coordinator/subagent interaction."""

    edge_id: str
    call_id: str
    from_thread_id: str
    to_thread_id: str
    kind: str
    timestamp_ms: int
    message_text: str | None
    content_availability: str
    encrypted_content: str | None
    source_line: int

    def to_json_obj(self) -> JsonObject:
        """Return the interaction edge as a JSON-serializable object."""

        return {
            "edge_id": self.edge_id,
            "call_id": self.call_id,
            "from_thread_id": self.from_thread_id,
            "to_thread_id": self.to_thread_id,
            "kind": self.kind,
            "timestamp_ms": self.timestamp_ms,
            "message_text": self.message_text,
            "content_availability": self.content_availability,
            "encrypted_content": self.encrypted_content,
            "source_line": self.source_line,
        }


@dataclass(frozen=True)
class TaskNote:
    """One task-tracker note, kept verbatim instead of only as the message it renders into.

    A note already reaches the timeline as an :class:`Event` -- ``kind`` ``external_message`` or
    ``inter_agent_message``, with ``text`` set to ``[<task_id> · <title>]\\n\\n<content>`` and the
    note id in ``source_line``. That rendering is for reading, and it is lossy in the ways
    renderings are: the task id and title are packed into a text prefix that cannot be unpacked
    when a title itself contains the separator, ``task_owner`` survives only as the synthetic
    thread the event was attributed to, ``created_at`` survives only as milliseconds, and a note
    outside the ingest date window produces no event at all. None of that matters for display and
    all of it matters for an archive that is the last copy of the text.

    So this is the record, and the Event stays the rendering. The two are joined on
    ``(source_path, note_id)`` against the event's ``source_line``, which is deliberately not a
    stored foreign key: one note becomes one event *per coordinator* whose lifetime covers it, so
    the relation is one-to-many across a continued lineage and a single id field would have to
    lie about that.

    ``projection_policy`` and ``projection_sha256`` are recorded once, when the note is first
    promoted, and never rewritten. The frozen projection they name is content-addressed and is
    replaced by a new generation every time any note changes, so re-deriving them on each ingest
    would restamp every note in the archive with today's generation -- churning the whole file for
    one appended note, and asserting something false about where the other records came from.

    ``upstream_present`` is the one field here that no later run can reconstruct, and it is the
    reason this record family is worth its bytes. Orc's ``task_notes`` table is mutable and rows
    are genuinely deleted from it: measured against the archive that prompted this, one Orc team
    has 1,311 of 7,826 frozen notes with no live counterpart and another has 75 of 5,079 --
    1,386 notes for which this file is the only copy
    in existence. The projection has always counted them (``OrcTaskProjection.missing_note_count``)
    and hashed their ids, but a count is not an answer to "which ones", and the moment
    ``source_snapshots/`` is deleted -- the entire purpose of the promotion -- nothing can ever
    recompute it. An earlier draft recorded an ``origin`` string here instead, which was
    ``frozen_projection`` exactly when ``projection_policy`` was the history policy: a restatement
    of the field beside it, carrying no information, in the one place where the information that
    mattered was being thrown away.

    It is therefore the one field that is *not* frozen at first promotion. It latches one way --
    true to false, never back, since Orc never reuses a note id below its frozen high-water mark
    -- so a note that upstream deletes next year is recorded as deleted on the next ingest that
    can still see the table. Freezing it would have made the field answer the question correctly
    only for notes already deleted the first time this archive ran, which is precisely the
    population that stops growing. False means "this archive is the last copy"; true means it was
    still upstream at the most recent ingest, and says nothing about after that.
    """

    note_id: int
    source_path: str
    task_source_ordinal: int
    task_id: str
    title: str
    content: str
    created_at: str
    server_author: str | None
    task_owner: str | None
    upstream_present: bool
    projection_policy: str | None
    projection_sha256: str | None

    def to_json_obj(self) -> JsonObject:
        """Return the task note as a JSON-serializable object."""

        return {
            "note_id": self.note_id,
            "source_path": self.source_path,
            "task_source_ordinal": self.task_source_ordinal,
            "task_id": self.task_id,
            "title": self.title,
            "content": self.content,
            "created_at": self.created_at,
            "server_author": self.server_author,
            "task_owner": self.task_owner,
            "upstream_present": self.upstream_present,
            "projection_policy": self.projection_policy,
            "projection_sha256": self.projection_sha256,
        }


def task_note_key(note: TaskNote) -> tuple[str, int]:
    """Return the identity a promoted note is merged and ordered on.

    The note id is only unique within one task database, and an Orc lineage can carry several --
    one team in the measured archive carries two -- so the source path is part of the identity, not
    decoration. ``task_source_ordinal`` is deliberately *not* in the key: it is an ordinal
    assigned per ingest and a source that changes ordinal is still the same source.
    """

    return (note.source_path, note.note_id)


@dataclass(frozen=True)
class TeamData:
    """A deterministic, provider-neutral snapshot ready for JSON emission."""

    team_slug: str
    provider: str
    root_thread_id: str
    display_timezone: str
    sources: tuple[SourceSnapshot, ...]
    agents: tuple[Agent, ...]
    turns: tuple[Turn, ...]
    events: tuple[Event, ...]
    tool_calls: tuple[ToolCall, ...]
    edges: tuple[Edge, ...]
    task_notes: tuple[TaskNote, ...] = ()
    window_start_ms: int | None = None
    window_end_ms: int | None = None

    def to_json_obj(self) -> JsonObject:
        """Return the complete team snapshot as a JSON-serializable object."""

        result: JsonObject = {
            "team_slug": self.team_slug,
            "provider": self.provider,
            "root_thread_id": self.root_thread_id,
            "display_timezone": self.display_timezone,
            "sources": [item.to_json_obj() for item in self.sources],
            "agents": [item.to_json_obj() for item in self.agents],
            "turns": [item.to_json_obj() for item in self.turns],
            "events": [item.to_json_obj() for item in self.events],
            "tool_calls": [item.to_json_obj() for item in self.tool_calls],
            "edges": [item.to_json_obj() for item in self.edges],
            "window_start_ms": self.window_start_ms,
            "window_end_ms": self.window_end_ms,
        }
        # Emitted only when there is something to emit, so that every archive written before task
        # notes were a record family stays byte-identical until its next ingest -- the same
        # bargain `DateWindow.to_json_obj` already makes for its exact-instant bounds. This is not
        # cosmetic: `raw/team.json` reaches 220 MB on this archive, it is version-controlled, and
        # adding `"task_notes": []` to every one of them would be a rewrite of every team for no
        # information. The archive's own storage of these records is
        # `raw/task-notes.jsonl`, which is why in practice the ingest writer strips them here
        # rather than duplicating tens of megabytes into two files; see
        # `pipeline._write_ingested_team`.
        if self.task_notes:
            result["task_notes"] = [item.to_json_obj() for item in self.task_notes]
        return result


def source_digest(team: TeamData) -> str:
    """Return the compatibility-shaped cache digest for normalized source prefixes."""

    snapshots: list[JsonValue] = [
        {
            "thread_id": source.thread_id,
            "complete_bytes": (
                source.semantic_complete_bytes
                if source.semantic_complete_bytes is not None
                else source.complete_bytes
            ),
            "sha256": source.semantic_sha256 or source.sha256,
        }
        for source in team.sources
    ]
    material = canonical_json(snapshots)
    return hashlib.sha256(material.encode("utf-8")).hexdigest()


__all__ = [
    "Agent",
    "Edge",
    "Event",
    "JsonObject",
    "PayloadRef",
    "SourceSnapshot",
    "TaskNote",
    "TeamData",
    "ToolCall",
    "Turn",
    "source_digest",
    "task_note_key",
]
