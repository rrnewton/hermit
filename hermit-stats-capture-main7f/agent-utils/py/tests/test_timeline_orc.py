from __future__ import annotations

import hashlib
import json
import shutil
import sqlite3
import stat
from collections.abc import Mapping, Sequence
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path

import wrkviz.orc as orc_module
import wrkviz.pipeline as pipeline_module
import pytest

from wrkviz.build_store import DEFAULT_STORE_SUFFIX, team_build_root
from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    canonical_json,
    canonical_jsonl,
    narrow_json,
    read_json,
    read_jsonl,
    write_json_if_changed,
)
from wrkviz.cli import main as timeline_main
from wrkviz.orc import (
    OrcContinuationLink,
    OrcContinuationSpec,
    OrcParseError,
    OrcSourceCopy,
    load_orc_team,
    snapshot_orc_lineage,
)
from wrkviz.model import TeamData, source_digest
from wrkviz.payloads import load_payload_manifest, verify_payload_store
from wrkviz.phases import build_phases
from wrkviz.pipeline import (
    build_archive,
    ingest_orc,
    load_archived_team,
    summarize_archive,
)
from wrkviz.window import apply_date_window, parse_date_window
from tests.timeline_snapshots import snapshot_root
from tests.timeline_projection import schema_1_timeline_text


ROOT = "11111111-1111-1111-1111-111111111111"
NESTED = "22222222-2222-2222-2222-222222222222"
SUCCESSOR = "33333333-3333-3333-3333-333333333333"
THIRD = "44444444-4444-4444-4444-444444444444"


def _ms(value: str) -> int:
    return int(datetime.fromisoformat(value).astimezone(timezone.utc).timestamp() * 1000)


def _session_database(
    path: Path,
    session_id: str,
    *,
    parent_id: str | None,
    db_name: str | None,
    messages: list[dict[str, object]],
    blocks: Sequence[tuple[object, ...]],
    created_at: str = "2026-07-20T19:00:00+00:00",
    updated_at: str = "2026-07-22T04:00:00+00:00",
    schema_version: int | None = None,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.executescript(
            """
            CREATE TABLE session_meta (
                id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, db_name TEXT,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE conversation_state (
                id INTEGER PRIMARY KEY CHECK (id = 1), conversation_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE associated_dbs (db_name TEXT PRIMARY KEY);
            CREATE TABLE content_blocks (
                id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                block_index INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
                turn_index INTEGER NOT NULL DEFAULT 0, role TEXT NOT NULL,
                block_type TEXT NOT NULL, content TEXT, searchable_text TEXT,
                code_input TEXT, code_output TEXT, code_exit_code INTEGER, model TEXT,
                user_source TEXT, token_count INTEGER, extra TEXT
            );
            """
        )
        connection.execute(
            "INSERT INTO session_meta VALUES (?, ?, ?, ?, ?, ?)",
            (
                session_id,
                parent_id,
                "root" if parent_id is None else "nested",
                db_name,
                created_at,
                updated_at,
            ),
        )
        conversation = json.dumps({"messages": messages}, separators=(",", ":"))
        connection.execute(
            "INSERT INTO conversation_state VALUES (1, ?, ?)",
            (conversation, updated_at),
        )
        connection.executemany(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            blocks,
        )
        if schema_version is not None:
            _write_provider_schema_version(connection, schema_version)
        connection.commit()
    finally:
        connection.close()


def _write_provider_schema_version(
    connection: sqlite3.Connection, version: int
) -> None:
    """Create or update Orc's own in-band schema marker, in Orc's own shape.

    Copied from a real session database rather than invented, including the `CHECK` constraints:
    the point of recording this value is that it is Orc's statement about its storage, so a fixture
    that stored it in some more convenient shape would be testing this repository's imagination.
    """

    connection.executescript(
        """
        CREATE TABLE IF NOT EXISTS schema_version (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            version INTEGER NOT NULL CHECK (version > 0),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        """
    )
    connection.execute(
        "INSERT INTO schema_version(id, version, updated_at) VALUES (1, ?, ?) "
        "ON CONFLICT(id) DO UPDATE SET version = excluded.version",
        (version, "2026-07-21 09:00:00"),
    )


def _set_provider_schema_version(path: Path, version: int) -> None:
    connection = sqlite3.connect(path)
    try:
        _write_provider_schema_version(connection, version)
        connection.commit()
    finally:
        connection.close()


def _task_database(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.executescript(
            """
            CREATE TABLE tasks (
                local_id TEXT PRIMARY KEY, title TEXT NOT NULL, owner TEXT
            );
            CREATE TABLE task_notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
                content TEXT NOT NULL, created_at TEXT NOT NULL,
                server_comment_id TEXT, author_unixname TEXT
            );
            INSERT INTO tasks VALUES ('task-a', 'Audit the scheduler', 'worker');
            """
        )
        connection.executemany(
            "INSERT INTO task_notes(task_id, content, created_at) VALUES (?, ?, ?)",
            (
                ("task-a", "First incarnation finding", "2026-07-21T10:00:00+00:00"),
                ("task-a", "Second incarnation result", "2026-07-21T17:00:00+00:00"),
                ("task-a", "", "2026-07-21T18:00:00+00:00"),
                ("task-a", "Exactly at exclusive end", "2026-07-22T04:00:00+00:00"),
            ),
        )
        connection.commit()
    finally:
        connection.close()


def _index_database(path: Path, sessions: Sequence[tuple[str, str | None]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_id TEXT)"
        )
        connection.executemany("INSERT INTO sessions VALUES (?, ?)", sessions)
        connection.commit()
    finally:
        connection.close()


def _add_messages(
    path: Path,
    messages: Sequence[tuple[int, str, int, dict[str, object]]],
) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "CREATE TABLE messages ("
            "id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, "
            "role TEXT NOT NULL, created_at_ms INTEGER NOT NULL, "
            "message_json TEXT NOT NULL, search_text TEXT)"
        )
        connection.executemany(
            "INSERT INTO messages(id, session_id, role, created_at_ms, "
            "message_json, search_text) VALUES (?, ?, ?, ?, ?, NULL)",
            (
                (
                    row_id,
                    session_id,
                    str(message["role"]).lower(),
                    timestamp_ms,
                    json.dumps(message, separators=(",", ":")),
                )
                for row_id, session_id, timestamp_ms, message in messages
            ),
        )
        connection.commit()
    finally:
        connection.close()


def _append_index_session(path: Path, session_id: str, parent_id: str | None) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "INSERT INTO sessions VALUES (?, ?)", (session_id, parent_id)
        )
        connection.commit()
    finally:
        connection.close()


def _rewrite_conversation(
    path: Path, messages: list[dict[str, object]]
) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "UPDATE conversation_state SET conversation_json = ? WHERE id = 1",
            (json.dumps({"messages": messages}, separators=(",", ":")),),
        )
        connection.commit()
    finally:
        connection.close()


def _snapshot_database(snapshot_root: Path, source: OrcSourceCopy) -> Path:
    return snapshot_root.joinpath(*Path(source.snapshot_path).parts)


def _manifest_snapshot_database(archive: Path, kind: str) -> Path:
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    root = as_object(read_json(manifest_path), str(manifest_path))
    source = next(
        as_object(item, f"{manifest_path}: source")
        for item in as_array(root.get("sources"), f"{manifest_path}: sources")
        if as_object(item, f"{manifest_path}: source").get("kind") == kind
    )
    relative = source.get("snapshot_path")
    if not isinstance(relative, str):
        raise AssertionError("test fixture snapshot path is not a string")
    return snapshot_root(archive, "orc-test") / relative


def _managed_snapshot_objects(archive: Path) -> tuple[Path, ...]:
    root = snapshot_root(archive, "orc-test") / ".objects"
    if not root.is_dir():
        return ()
    return tuple(
        sorted(
            path
            for path in root.glob("[0-9a-f][0-9a-f]/*.db")
            if path.is_file() and not path.is_symlink()
        )
    )


def _managed_task_projections(archive: Path) -> tuple[Path, ...]:
    root = snapshot_root(archive, "orc-test") / ".projections"
    if not root.is_dir():
        return ()
    return tuple(
        sorted(
            path
            for path in root.glob("[0-9a-f][0-9a-f]/*.json")
            if path.is_file() and not path.is_symlink()
        )
    )


def _manifest_task_projection(archive: Path) -> tuple[Path, dict[str, JsonValue]]:
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    root = as_object(read_json(manifest_path), str(manifest_path))
    task = next(
        as_object(item, f"{manifest_path}: source")
        for item in as_array(root.get("sources"), f"{manifest_path}: sources")
        if as_object(item, f"{manifest_path}: source").get("kind") == "task"
    )
    projection = as_object(
        task.get("task_projection"), f"{manifest_path}: task_projection"
    )
    relative = projection.get("path")
    if not isinstance(relative, str):
        raise AssertionError("test fixture projection path is not a string")
    path = snapshot_root(archive, "orc-test") / relative
    return path, projection


def _append_root_message(path: Path, block_id: str, content: str) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                block_id,
                f"message-{block_id}",
                ROOT,
                0,
                _ms("2026-07-21T19:00:00+00:00"),
                3,
                "assistant",
                "text",
                content,
                None,
                None,
                None,
                None,
                "model",
                None,
                None,
                None,
            ),
        )
        connection.commit()
    finally:
        connection.close()


def _append_task_note(path: Path, content: str) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "INSERT INTO task_notes(task_id, content, created_at) VALUES (?, ?, ?)",
            ("task-a", content, "2026-07-21T20:00:00+00:00"),
        )
        connection.commit()
    finally:
        connection.close()


def _non_agent_rewrite_messages(text: str) -> list[dict[str, object]]:
    return [
        {
            "id": 1,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T03:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 10, "agent_id": "worker"}],
        },
        {
            "id": 2,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T16:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 11, "agent_id": "worker"}],
        },
        {
            "id": 30,
            "role": "Assistant",
            "created_at_ms": _ms("2026-07-21T17:30:00+00:00"),
            "blocks": [{"type": "TextBlock", "id": 30, "text": text}],
        },
    ]


def _semantic_team(team: TeamData) -> tuple[object, ...]:
    """Exclude physical source-object provenance from normalized semantics."""

    return (
        team.team_slug,
        team.provider,
        team.root_thread_id,
        team.display_timezone,
        team.agents,
        team.turns,
        team.events,
        team.tool_calls,
        team.edges,
        team.window_start_ms,
        team.window_end_ms,
    )


def _legacy_source_digest(team: TeamData) -> str:
    """Reproduce the exact pre-semantic-provenance cache-key algorithm."""

    snapshots: list[JsonValue] = [
        {
            "thread_id": source.thread_id,
            "complete_bytes": source.complete_bytes,
            "sha256": source.sha256,
        }
        for source in team.sources
    ]
    return hashlib.sha256(canonical_json(snapshots).encode("utf-8")).hexdigest()


def _rewritten_messages(*, first_agent: str = "worker") -> list[dict[str, object]]:
    """Return a shorter conversation projection with all old spawn facts retained."""

    return [
        {
            "id": 1,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T03:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 10, "agent_id": first_agent}],
        },
        {
            "id": 2,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T16:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 11, "agent_id": "worker"}],
        },
        {
            "id": 5,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T18:30:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 12, "agent_id": "reviewer"}],
        },
    ]


def _fixture(tmp_path: Path) -> tuple[Path, Path, Path]:
    source = tmp_path / "source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    task_db = source / ".tg" / "project.db"
    messages = [
        {
            "id": 1,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T03:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 10, "agent_id": "worker"}],
        },
        {
            "id": 2,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T16:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 11, "agent_id": "worker"}],
        },
        {
            "id": 3,
            "role": "Assistant",
            "created_at_ms": _ms("2026-07-21T17:00:00+00:00"),
            "blocks": [{"type": "TextBlock", "id": 20, "text": "temporary"}],
        },
        {
            "id": 4,
            "role": "Assistant",
            "created_at_ms": _ms("2026-07-21T17:01:00+00:00"),
            "blocks": [{"type": "TextBlock", "id": 21, "text": "temporary"}],
        },
    ]
    root_blocks = [
        (
            "root-user",
            "message-1",
            ROOT,
            0,
            _ms("2026-07-21T04:00:00+00:00"),
            1,
            "user",
            "text",
            "Start the complete local day",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            "root-tool",
            "message-2",
            ROOT,
            0,
            _ms("2026-07-21T04:00:01+00:00"),
            1,
            "assistant",
            "code_execution",
            None,
            None,
            "await orc.readFile('x'); await orc.readFile('y'); await orc.sendAgent('w','x')",
            "ok",
            0,
            "model",
            None,
            None,
            None,
        ),
        (
            "root-end",
            "message-3",
            ROOT,
            0,
            _ms("2026-07-22T04:00:00+00:00"),
            2,
            "assistant",
            "text",
            "Outside the half-open window",
            None,
            None,
            None,
            None,
            "model",
            None,
            None,
            None,
        ),
    ]
    nested_blocks = [
        (
            "nested-text",
            "nested-message",
            NESTED,
            0,
            _ms("2026-07-21T12:00:00+00:00"),
            1,
            "assistant",
            "text",
            "Nested coordinator update",
            None,
            None,
            None,
            None,
            "model",
            None,
            None,
            None,
        )
    ]
    _session_database(
        root_db,
        ROOT,
        parent_id=None,
        db_name="project",
        messages=messages,
        blocks=root_blocks,
    )
    _session_database(
        nested_db,
        NESTED,
        parent_id=ROOT,
        db_name=None,
        messages=[],
        blocks=nested_blocks,
    )
    _index_database(
        source / ".orc" / "index.db",
        ((ROOT, None), (NESTED, ROOT)),
    )
    _task_database(task_db)
    return source, root_db, task_db


def _continuation_fixture(tmp_path: Path) -> tuple[Path, Path]:
    source = tmp_path / "continuation-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    successor_db = source / ".orc" / "sessions" / SUCCESSOR / "session.db"
    task_db = source / ".tg" / "project.db"
    root_messages = [
        {
            "id": 1,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T09:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 10, "agent_id": "worker"}],
        }
    ]
    successor_messages = [
        {
            "id": 1,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T16:05:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 10, "agent_id": "worker"}],
        }
    ]
    root_blocks = [
        (
            "shared-text",
            "root-message",
            ROOT,
            0,
            _ms("2026-07-21T15:00:00+00:00"),
            1,
            "assistant",
            "text",
            "Predecessor final status",
            None,
            None,
            None,
            None,
            "model",
            None,
            None,
            None,
        ),
        (
            "shared-tool",
            "root-tool-message",
            ROOT,
            0,
            _ms("2026-07-21T15:01:00+00:00"),
            1,
            "assistant",
            "code_execution",
            None,
            None,
            "await orc.readFile('old')",
            "old",
            0,
            "model",
            None,
            None,
            None,
        ),
    ]
    successor_blocks = [
        (
            "shared-text",
            "successor-message",
            SUCCESSOR,
            0,
            _ms("2026-07-21T16:15:00+00:00"),
            1,
            "assistant",
            "text",
            "Successor first status",
            None,
            None,
            None,
            None,
            "model",
            None,
            None,
            None,
        ),
        (
            "shared-tool",
            "successor-tool-message",
            SUCCESSOR,
            0,
            _ms("2026-07-21T16:16:00+00:00"),
            1,
            "assistant",
            "code_execution",
            None,
            None,
            "await orc.readFile('new')",
            "new",
            0,
            "model",
            None,
            None,
            None,
        ),
    ]
    _session_database(
        root_db,
        ROOT,
        parent_id=None,
        db_name="project",
        messages=root_messages,
        blocks=root_blocks,
    )
    _session_database(
        successor_db,
        SUCCESSOR,
        parent_id=None,
        db_name="project",
        messages=successor_messages,
        blocks=successor_blocks,
        created_at="2026-07-21T16:00:00+00:00",
    )
    _index_database(
        source / ".orc" / "index.db",
        ((ROOT, None), (SUCCESSOR, None)),
    )
    _task_database(task_db)
    return source, task_db


# The first link's frozen boundary and the moment its successor starts, named once so the tampering
# cases below can state the exact value they are replacing rather than nudging whatever is there.
_SPLIT_FIRST_BOUNDARY_MS = _ms("2026-07-21T15:01:00+00:00")
_SPLIT_SECOND_START_MS = _ms("2026-07-21T16:00:00+00:00")


def _boundary_block(
    block_id: str, session_id: str, at_ms: int, text: str
) -> tuple[object, ...]:
    return (
        block_id,
        f"{block_id}-message",
        session_id,
        0,
        at_ms,
        1,
        "assistant",
        "text",
        text,
        None,
        None,
        None,
        None,
        "model",
        None,
        None,
        None,
    )


def _status_message(native_id: str, block_id: int, at_ms: int) -> dict[str, object]:
    return {
        "id": native_id,
        "role": "Assistant",
        "source": None,
        "created_at_ms": at_ms,
        "blocks": [{"type": "StatusBlock", "id": block_id, "message": native_id}],
    }


def _split_table_continuation_fixture(tmp_path: Path) -> Path:
    """Build the one lineage shape that proves a global storage-table answer cannot work.

    Three parentless coordinators, two continuation links, and the two links' boundary ordinals live
    in *different* tables:

    * The first predecessor is a pre-transition session -- `content_blocks` only, no `messages`
      table at all -- so its boundary ordinal is a `content_blocks` rowid.
    * The second predecessor is a post-transition session carrying both tables, so its boundary
      ordinal is a `messages` id. That ordinal *also* exists as a `content_blocks` rowid in the same
      database, with a timestamp days away from the recorded one, which is what makes "just prefer
      one table" indistinguishable from a coin flip.

    Both predecessors are in one lineage and are resolved by one ingest, so any rule that answers
    "which table?" once per run, however it answers, gets exactly one of the two links wrong. This
    is not a hypothetical: it is the shape of a measured archive whose ingest refused with
    "boundary row disappeared" against entirely intact data.
    """

    source = tmp_path / "split-table-source"
    first_db = source / ".orc" / "sessions" / ROOT / "session.db"
    second_db = source / ".orc" / "sessions" / SUCCESSOR / "session.db"
    third_db = source / ".orc" / "sessions" / THIRD / "session.db"
    _session_database(
        first_db,
        ROOT,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=[
            _boundary_block(
                "first-opening",
                ROOT,
                _ms("2026-07-21T15:00:00+00:00"),
                "Predecessor opening",
            ),
            _boundary_block(
                "first-final",
                ROOT,
                _ms("2026-07-21T15:01:00+00:00"),
                "Predecessor final status",
            ),
        ],
        created_at="2026-07-21T09:00:00+00:00",
        updated_at="2026-07-21T15:30:00+00:00",
        schema_version=3,
    )
    # Five content rows and four messages, so the messages ordinal the second link freezes (4) is
    # also a live content_blocks rowid -- carrying a different timestamp. Without that overlap the
    # regression would pass for the wrong reason: any rule at all resolves an ordinal that exists in
    # exactly one table.
    _session_database(
        second_db,
        SUCCESSOR,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=[
            _boundary_block(
                f"second-block-{index}",
                SUCCESSOR,
                _ms("2026-07-21T16:15:00+00:00") + index * 60_000,
                f"Successor block {index}",
            )
            for index in range(1, 6)
        ],
        created_at="2026-07-21T16:00:00+00:00",
        updated_at="2026-07-21T19:30:00+00:00",
        schema_version=5,
    )
    _add_messages(
        second_db,
        tuple(
            (
                index,
                SUCCESSOR,
                at_ms,
                _status_message(f"second-message-{index}", 100 + index, at_ms),
            )
            for index, at_ms in enumerate(
                (
                    _ms("2026-07-21T16:20:00+00:00"),
                    _ms("2026-07-21T17:00:00+00:00"),
                    _ms("2026-07-21T18:00:00+00:00"),
                    _ms("2026-07-21T19:00:00+00:00"),
                ),
                start=1,
            )
        ),
    )
    _session_database(
        third_db,
        THIRD,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=[
            _boundary_block(
                "third-opening",
                THIRD,
                _ms("2026-07-21T20:15:00+00:00"),
                "Second successor opening",
            )
        ],
        created_at="2026-07-21T20:00:00+00:00",
        updated_at="2026-07-21T21:00:00+00:00",
        schema_version=5,
    )
    _index_database(
        source / ".orc" / "index.db",
        ((ROOT, None), (SUCCESSOR, None), (THIRD, None)),
    )
    return source


def _source_manifest(archive: Path) -> dict[str, JsonValue]:
    path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    return as_object(read_json(path), str(path))


def _manifest_links(archive: Path) -> list[dict[str, object]]:
    manifest = _source_manifest(archive)
    return [
        dict(as_object(raw_link, "continuation"))
        for raw_link in as_array(manifest["continuation_sessions"], "continuations")
    ]


def _link_int(link: Mapping[str, object], key: str) -> int:
    return as_int(narrow_json(link[key], key), key)


def _write_manifest_links(
    archive: Path, schema_version: int, links: Sequence[Mapping[str, object]]
) -> None:
    """Put a hand-edited continuation record back, at a stated manifest schema version.

    The version is a required argument rather than something inferred from the records, because
    every use here is deliberately writing a record shape from a *different* generation of the
    writer than the one that produced the archive, and inferring it would quietly repair the very
    mismatch the test exists to create.
    """

    path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    manifest: dict[str, object] = dict(_source_manifest(archive))
    manifest["schema_version"] = schema_version
    manifest["continuation_sessions"] = [dict(link) for link in links]
    path.write_text(json.dumps(narrow_json(manifest)), encoding="utf-8")


def _recorded_boundary_tables(archive: Path) -> list[object]:
    return [link["predecessor_source_table"] for link in _manifest_links(archive)]


def _recorded_provider_schema_versions(archive: Path) -> dict[str, object]:
    manifest = _source_manifest(archive)
    return {
        str(as_object(source, "source")["source_path"]): as_object(
            source, "source"
        )["provider_schema_version"]
        for source in as_array(manifest["sources"], "sources")
    }


def test_two_link_lineage_freezes_each_boundary_table_separately(
    tmp_path: Path,
) -> None:
    source = _split_table_continuation_fixture(tmp_path)
    archive = tmp_path / "archive"

    _, first = ingest_orc(
        archive,
        source,
        ROOT,
        "orc-test",
        "UTC",
        None,
        None,
        (SUCCESSOR, THIRD),
    )
    _, second = ingest_orc(archive, source, ROOT, "orc-test", "UTC")

    assert first.sources > 0
    assert second.files_changed == 0
    manifest = _source_manifest(archive)
    assert manifest["schema_version"] == 5
    # The whole claim in one assertion: two links in one lineage, resolved by one ingest, landing in
    # two different tables.
    assert _recorded_boundary_tables(archive) == ["content_blocks", "messages"]
    links = as_array(manifest["continuation_sessions"], "continuations")
    assert as_object(links[0], "link")["predecessor_source_line"] == 2
    assert as_object(links[1], "link")["predecessor_source_line"] == 4
    assert _recorded_provider_schema_versions(archive) == {
        f".orc/sessions/{ROOT}/session.db": 3,
        f".orc/sessions/{SUCCESSOR}/session.db": 5,
        f".orc/sessions/{THIRD}/session.db": 5,
    }


def test_a_first_ingest_derives_the_boundary_from_evidence_not_from_layout(
    tmp_path: Path,
) -> None:
    """A predecessor caught mid-transition must not lose its boundary to the empty new table.

    This is the *derivation* side of the same defect the resolver fixes on the verification
    side, and it was left behind once: `_latest_session_record_before` asked
    `_session_storage_table` which layout the database presents, and that function answers
    "messages" the moment a `messages` table exists, whether or not it holds anything.

    Two failures follow, and the fixture reproduces both at once. The predecessor's transcript
    lives entirely in `content_blocks`; a `messages` table exists beside it holding a single row
    from long before the successor started. Under the old rule the boundary derived to that one
    early row -- skipping every later `content_blocks` record and freezing the wrong pair into
    the manifest as an observation -- and with that row removed entirely the derivation found
    nothing and the ingest refused a lineage whose data is completely intact.
    """

    source = _split_table_continuation_fixture(tmp_path)
    first_db = source / ".orc" / "sessions" / ROOT / "session.db"
    early = _ms("2026-07-21T09:30:00+00:00")
    _add_messages(
        first_db,
        ((1, ROOT, early, _status_message("stray-early-message", 300, early)),),
    )

    archive = tmp_path / "archive"
    ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", None, None, (SUCCESSOR, THIRD)
    )

    links = _manifest_links(archive)
    # The real last predecessor record is `content_blocks` rowid 2 at 15:01, not the stray
    # `messages` row at 09:30.
    assert links[0]["predecessor_source_table"] == "content_blocks"
    assert links[0]["predecessor_source_line"] == 2
    assert _link_int(links[0], "predecessor_at_ms") == _ms(
        "2026-07-21T15:01:00+00:00"
    )


def test_storage_transition_under_a_frozen_link_is_not_a_disappeared_row(
    tmp_path: Path,
) -> None:
    """Orc migrating a predecessor to dual-table storage must not invalidate its frozen boundary."""

    source = _split_table_continuation_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", None, None, (SUCCESSOR, THIRD)
    )
    first_db = source / ".orc" / "sessions" / ROOT / "session.db"

    # Exactly what Orc's in-place migration does: the pre-transition rows stay in `content_blocks`
    # and a `messages` table appears alongside them holding only post-transition traffic. The
    # frozen boundary ordinal (2) is therefore far beyond the new table's single row -- the measured
    # archive's numbers were 11111 and 906 -- so re-deriving the table from what the database now
    # looks like reports the row as gone.
    _add_messages(
        first_db,
        (
            (
                1,
                ROOT,
                _ms("2026-07-21T15:20:00+00:00"),
                _status_message("first-post-migration", 200, _ms("2026-07-21T15:20:00+00:00")),
            ),
        ),
    )
    _set_provider_schema_version(first_db, 8)

    _, migrated = ingest_orc(archive, source, ROOT, "orc-test", "UTC")

    assert migrated.files_changed > 0
    assert _recorded_boundary_tables(archive) == ["content_blocks", "messages"]
    assert _recorded_provider_schema_versions(archive)[
        f".orc/sessions/{ROOT}/session.db"
    ] == 8
    assert load_archived_team(archive, "orc-test").provider == "orc"


def test_table_less_links_are_migrated_by_evidence_not_by_storage_layout(
    tmp_path: Path,
) -> None:
    """A manifest frozen before the table was recorded resolves both links, each in its own table."""

    source = _split_table_continuation_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", None, None, (SUCCESSOR, THIRD)
    )

    # Rewind the manifest to the shape a table-less writer produced, so the migration path runs
    # against the record it actually has to handle rather than a newly written one with a field
    # blanked out.
    links = _manifest_links(archive)
    for link in links:
        del link["predecessor_source_table"]
    _write_manifest_links(archive, 4, links)

    _, migrated = ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    _, repeated = ingest_orc(archive, source, ROOT, "orc-test", "UTC")

    assert migrated.files_changed > 0
    assert repeated.files_changed == 0
    assert _source_manifest(archive)["schema_version"] == 5
    assert _recorded_boundary_tables(archive) == ["content_blocks", "messages"]


def test_table_less_link_whose_evidence_is_nowhere_still_refuses(
    tmp_path: Path,
) -> None:
    source = _split_table_continuation_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", None, None, (SUCCESSOR, THIRD)
    )
    links = _manifest_links(archive)
    for link in links:
        del link["predecessor_source_table"]
        link["predecessor_at_ms"] = _link_int(link, "predecessor_at_ms") - 1
        link["gap_ms"] = _link_int(link, "gap_ms") + 1
    _write_manifest_links(archive, 4, links)

    with pytest.raises(OrcParseError, match="boundary evidence changed"):
        ingest_orc(archive, source, ROOT, "orc-test", "UTC")


@pytest.mark.parametrize(
    ("mutation", "expected"),
    (
        (
            {
                "predecessor_at_ms": _SPLIT_FIRST_BOUNDARY_MS - 1,
                "gap_ms": _SPLIT_SECOND_START_MS - _SPLIT_FIRST_BOUNDARY_MS + 1,
            },
            "boundary evidence changed",
        ),
        ({"predecessor_source_line": 999}, "boundary row disappeared"),
        (
            {"predecessor_source_table": "messages"},
            "boundary table 'messages' is absent",
        ),
    ),
    ids=("evidence-changed", "row-disappeared", "table-absent"),
)
def test_naming_the_boundary_table_narrows_the_guard_without_weakening_it(
    tmp_path: Path, mutation: dict[str, object], expected: str
) -> None:
    """Each way a named boundary can stop being true still refuses, and says which way it was.

    The three failures were one message before the table was recorded, because a table-less link
    could not tell "the row is gone" from "we are reading the wrong table". Splitting them is the
    readable half of the fix: an operator who sees `boundary table 'messages' is absent` knows
    immediately that the manifest and the database disagree about storage layout, which is a very
    different morning from `boundary row disappeared`.
    """

    source = _split_table_continuation_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", None, None, (SUCCESSOR, THIRD)
    )
    links = _manifest_links(archive)
    links[0].update(mutation)
    _write_manifest_links(archive, 5, links)

    with pytest.raises(OrcParseError, match=expected):
        ingest_orc(archive, source, ROOT, "orc-test", "UTC")


def test_unreadable_provider_schema_marker_refuses_rather_than_reading_as_absent(
    tmp_path: Path,
) -> None:
    source = _split_table_continuation_fixture(tmp_path)
    third_db = source / ".orc" / "sessions" / THIRD / "session.db"
    connection = sqlite3.connect(third_db)
    try:
        connection.execute("DELETE FROM schema_version")
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="holds no version row"):
        ingest_orc(
            tmp_path / "archive",
            source,
            ROOT,
            "orc-test",
            "UTC",
            None,
            None,
            (SUCCESSOR, THIRD),
        )


def test_explicit_orc_continuation_unions_lineages_and_partitions_shared_notes(
    tmp_path: Path,
) -> None:
    source, _ = _continuation_fixture(tmp_path)
    snapshot = tmp_path / "continuation-snapshot"

    predecessor_only = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    assert {
        item.owner_session_id
        for item in predecessor_only.sources
        if item.kind == "session"
    } == {ROOT}
    combined = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        predecessor_only.sources,
        "second",
        (SUCCESSOR,),
    )
    repeated = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        combined.sources,
        "third",
        (SUCCESSOR,),
        combined.continuations,
    )

    assert repeated.files_changed == 0
    assert repeated.sources == combined.sources
    assert repeated.continuations == combined.continuations
    assert len(combined.continuations) == 1
    assert len([item for item in combined.sources if item.kind == "task"]) == 1
    link = combined.continuations[0]
    assert link.predecessor_session_id == ROOT
    assert link.session_id == SUCCESSOR
    assert link.started_at_ms == _ms("2026-07-21T16:00:00+00:00")
    assert link.predecessor_source_table == "content_blocks"
    assert OrcContinuationLink.from_json_obj(
        link.to_json_obj(), "continuation"
    ) == link
    # Two earlier record shapes still decode. Each dropped field decodes to `None`, which for the
    # boundary table means "unrecorded" -- the state the resolver is written to migrate out of --
    # so the decoded link is the same link minus exactly the facts that shape never carried.
    bounded_link = link.to_json_obj()
    del bounded_link["predecessor_source_table"]
    assert OrcContinuationLink.from_json_obj(
        bounded_link, "table-less continuation"
    ) == replace(link, predecessor_source_table=None)
    legacy_link = dict(bounded_link)
    del legacy_link["start_message_id"]
    del legacy_link["start_source_line"]
    assert OrcContinuationLink.from_json_obj(
        legacy_link, "legacy continuation"
    ) == replace(link, predecessor_source_table=None)

    team = load_orc_team(
        snapshot,
        ROOT,
        "orc-continuation",
        "UTC",
        combined.sources,
        combined.continuations,
    )
    successor = next(item for item in team.agents if item.thread_id == SUCCESSOR)
    assert successor.parent_thread_id == ROOT
    assert successor.depth == 1
    assert "/continuation-" in successor.agent_path
    continuation = next(item for item in team.edges if item.kind == "continuation")
    assert continuation.from_thread_id == ROOT
    assert continuation.to_thread_id == SUCCESSOR

    predecessor_note = next(
        item for item in team.events if "First incarnation finding" in (item.text or "")
    )
    successor_note = next(
        item for item in team.events if "Second incarnation result" in (item.text or "")
    )
    assert predecessor_note.recipient == ROOT
    assert successor_note.recipient == SUCCESSOR
    assert not predecessor_note.event_id.startswith("orc-cont-")
    assert successor_note.event_id.startswith(f"orc-cont-1-{SUCCESSOR[:8]}-")
    assert len(
        [item for item in team.events if "First incarnation finding" in (item.text or "")]
    ) == 1
    assert len(
        [item for item in team.events if "Second incarnation result" in (item.text or "")]
    ) == 1
    assert len({item.event_id for item in team.events}) == len(team.events)
    assert len({item.turn_id for item in team.turns}) == len(team.turns)
    assert len({item.call_id for item in team.tool_calls}) == len(team.tool_calls)
    assert any(
        item.event_id == "orc-block-shared-text" for item in team.events
    )
    assert any(
        item.event_id
        == f"orc-cont-1-{SUCCESSOR[:8]}-orc-block-shared-text"
        for item in team.events
    )


def test_orc_continuation_rejects_duplicate_or_misordered_roots(
    tmp_path: Path,
) -> None:
    source, _ = _continuation_fixture(tmp_path)
    snapshot = tmp_path / "invalid-continuation-snapshot"
    with pytest.raises(OrcParseError, match="must be unique"):
        snapshot_orc_lineage(
            source, ROOT, snapshot, (), "duplicate", (ROOT,)
        )
    with pytest.raises(OrcParseError, match="strictly increasing"):
        snapshot_orc_lineage(
            source, SUCCESSOR, snapshot, (), "misordered", (ROOT,)
        )


def test_modern_messages_extend_legacy_blocks_without_rewriting_ids(
    tmp_path: Path,
) -> None:
    source = tmp_path / "modern-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    content = [
        (
            "v2-block-10",
            "v2-message-1",
            ROOT,
            0,
            _ms("2026-08-15T10:00:00+00:00"),
            1,
            "user",
            "text",
            "Legacy overlap prompt",
            None,
            None,
            None,
            None,
            None,
            json.dumps({"Submitted": {"source": "Tui"}}),
            None,
            None,
        ),
        (
            "v2-block-11",
            "v2-message-2",
            ROOT,
            0,
            _ms("2026-08-15T10:00:01+00:00"),
            1,
            "assistant",
            "code_execution",
            None,
            None,
            "await orc.readFile('overlap')",
            "overlap",
            0,
            "model",
            None,
            None,
            None,
        ),
    ]
    _session_database(
        root_db,
        ROOT,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=content,
        created_at="2026-08-15T09:00:00+00:00",
        updated_at="2026-08-15T11:00:00+00:00",
    )
    _add_messages(
        root_db,
        (
            (
                1,
                ROOT,
                _ms("2026-08-15T10:00:00+00:00"),
                {
                    "id": "overlap-user",
                    "role": "User",
                    "source": json.dumps({"Submitted": {"source": "Tui"}}),
                    "created_at_ms": _ms("2026-08-15T10:00:00+00:00"),
                    "blocks": [{"type": "text", "id": 10, "text": "Legacy overlap prompt"}],
                },
            ),
            (
                2,
                ROOT,
                _ms("2026-08-15T10:00:01+00:00"),
                {
                    "id": "overlap-tool",
                    "role": "Assistant",
                    "source": None,
                    "created_at_ms": _ms("2026-08-15T10:00:01+00:00"),
                    "blocks": [
                        {
                            "type": "CodeExecutionBlock",
                            "id": 11,
                            "code": "await orc.readFile('overlap')",
                            "output": "overlap",
                            "is_error": False,
                        }
                    ],
                },
            ),
            (
                3,
                ROOT,
                _ms("2026-08-15T10:01:00+00:00"),
                {
                    "id": "noise-interjection",
                    "role": "User",
                    "source": None,
                    "created_at_ms": _ms("2026-08-15T10:01:00+00:00"),
                    "blocks": [
                        {"type": "InterjectionBlock", "id": 14, "text": "Purpose noise"}
                    ],
                },
            ),
            (
                4,
                ROOT,
                _ms("2026-08-15T10:02:00+00:00"),
                {
                    "id": "new-assistant",
                    "role": "Assistant",
                    "source": None,
                    "created_at_ms": _ms("2026-08-15T10:02:00+00:00"),
                    "blocks": [
                        {"type": "NotificationBlock", "id": 12, "text": "New result"},
                        {
                            "type": "CodeExecutionBlock",
                            "id": 13,
                            "code": "await orc.sendAgent('worker','continue')",
                            "output": "sent",
                            "is_error": False,
                        },
                        {
                            "type": "ErrorBlock",
                            "id": 15,
                            "message": "provider connection failed",
                        },
                    ],
                },
            ),
        ),
    )
    _index_database(source / ".orc" / "index.db", ((ROOT, None),))

    snapshot = tmp_path / "modern-snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "modern")
    team = load_orc_team(snapshot, ROOT, "modern", "UTC", copied.sources)

    assert [item.event_id for item in team.events] == [
        "orc-block-v2-block-10",
        "orc-block-v2-block-12",
    ]
    assert [item.call_id for item in team.tool_calls] == [
        "orc-code-v2-block-11",
        "orc-code-v2-block-13",
    ]
    assert {item.turn_id for item in team.turns} == {
        f"orc-turn-{ROOT[:8]}-1",
        f"orc-turn-{ROOT[:8]}-2",
    }
    failed_turn = next(
        item for item in team.turns if item.turn_id == f"orc-turn-{ROOT[:8]}-2"
    )
    assert failed_turn.status == "failed"
    assert failed_turn.error == "provider connection failed"
    assert not any("Purpose noise" in (item.text or "") for item in team.events)

    before_materialization = next(
        item for item in team.events if item.event_id == "orc-block-v2-block-12"
    )
    forged_sources = tuple(
        replace(item, semantic_sha256="0" * 64, semantic_complete_bytes=123)
        if item.kind == "session"
        else item
        for item in copied.sources
    )
    with pytest.raises(OrcParseError, match="semantic alias"):
        snapshot_orc_lineage(
            source, ROOT, snapshot, forged_sources, "forged-semantic-alias"
        )
    materialized = (
        "v2-block-12",
        "v2-message-4",
        ROOT,
        0,
        _ms("2026-08-15T10:02:00+00:00"),
        2,
        "notification",
        "text",
        "New result",
        None,
        None,
        None,
        None,
        "model",
        None,
        None,
        None,
    )
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            materialized,
        )
        connection.commit()
    finally:
        connection.close()
    advanced = snapshot_orc_lineage(
        source, ROOT, snapshot, copied.sources, "materialized"
    )
    after_team = load_orc_team(snapshot, ROOT, "modern", "UTC", advanced.sources)
    after_materialization = next(
        item
        for item in after_team.events
        if item.event_id == "orc-block-v2-block-12"
    )
    assert after_materialization == before_materialization
    assert after_materialization.source_native_id == "new-assistant"
    assert after_materialization.source_line == 4
    assert source_digest(after_team) == source_digest(team)
    assert advanced.sources != copied.sources

    connection = sqlite3.connect(root_db)
    try:
        connection.execute("DELETE FROM content_blocks WHERE id = 'v2-block-12'")
        connection.commit()
    finally:
        connection.close()
    with pytest.raises(OrcParseError, match="append history shrank|prefix"):
        snapshot_orc_lineage(source, ROOT, snapshot, advanced.sources, "deleted")
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            materialized,
        )
        connection.execute(
            "UPDATE content_blocks SET content = 'rewritten' "
            "WHERE id = 'v2-block-10'"
        )
        connection.commit()
    finally:
        connection.close()
    with pytest.raises(OrcParseError, match="prefix was rewritten"):
        snapshot_orc_lineage(source, ROOT, snapshot, advanced.sources, "rewritten")
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "UPDATE content_blocks SET content = 'Legacy overlap prompt' "
            "WHERE id = 'v2-block-10'"
        )
        original_json = connection.execute(
            "SELECT message_json FROM messages WHERE id = 4"
        ).fetchone()[0]
        changed = json.loads(str(original_json))
        changed["blocks"][0]["text"] = "rewritten modern message"
        connection.execute(
            "UPDATE messages SET message_json = ? WHERE id = 4",
            (json.dumps(changed, separators=(",", ":")),),
        )
        connection.commit()
    finally:
        connection.close()
    with pytest.raises(OrcParseError, match="prefix was rewritten"):
        snapshot_orc_lineage(
            source, ROOT, snapshot, advanced.sources, "messages-rewritten"
        )
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "UPDATE messages SET message_json = ? WHERE id = 4", (original_json,)
        )
        status_message = {
            "id": "status-append",
            "role": "Assistant",
            "source": None,
            "created_at_ms": _ms("2026-08-15T10:03:00+00:00"),
            "blocks": [{"type": "StatusBlock", "id": 16, "message": "idle"}],
        }
        connection.execute(
            "INSERT INTO messages(id, session_id, role, created_at_ms, "
            "message_json, search_text) VALUES (5, ?, 'assistant', ?, ?, NULL)",
            (
                ROOT,
                _ms("2026-08-15T10:03:00+00:00"),
                json.dumps(status_message, separators=(",", ":")),
            ),
        )
        connection.commit()
    finally:
        connection.close()
    messages_advanced = snapshot_orc_lineage(
        source, ROOT, snapshot, advanced.sources, "messages-appended"
    )
    assert messages_advanced.sources != advanced.sources
    status_team = load_orc_team(
        snapshot, ROOT, "modern", "UTC", messages_advanced.sources
    )
    assert source_digest(status_team) == source_digest(after_team)
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("DELETE FROM messages WHERE id = 5")
        connection.commit()
    finally:
        connection.close()
    with pytest.raises(OrcParseError, match="append history shrank|prefix"):
        snapshot_orc_lineage(
            source, ROOT, snapshot, messages_advanced.sources, "messages-deleted"
        )


def test_bounded_reused_root_is_hermetic_and_excludes_unrelated_task_db(
    tmp_path: Path,
) -> None:
    bounded = "44444444-4444-4444-4444-444444444444"
    inactive_child = "55555555-5555-5555-5555-555555555555"
    active_child = "66666666-6666-6666-6666-666666666666"
    task_only_child = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    source = tmp_path / "bounded-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    bounded_db = source / ".orc" / "sessions" / bounded / "session.db"
    inactive_db = source / ".orc" / "sessions" / inactive_child / "session.db"
    active_db = source / ".orc" / "sessions" / active_child / "session.db"
    task_only_db = source / ".orc" / "sessions" / task_only_child / "session.db"
    project_db = source / ".tg" / f"{bounded}.db"
    hatch_db = source / ".tg" / "hatch2.db"
    task_only_task_db = source / ".tg" / f"{task_only_child}.db"
    root_blocks = [
        (
            "root-boundary",
            "root-boundary-message",
            ROOT,
            0,
            _ms("2026-08-15T02:11:00+00:00"),
            1,
            "assistant",
            "text",
            "Predecessor ending",
            None,
            None,
            None,
            None,
            "model",
            None,
            None,
            None,
        )
    ]
    _session_database(
        root_db,
        ROOT,
        parent_id=None,
        db_name="project",
        messages=[],
        blocks=root_blocks,
        created_at="2026-08-14T00:00:00+00:00",
        updated_at="2026-08-15T04:00:00+00:00",
    )
    _session_database(
        bounded_db,
        bounded,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=[],
        created_at="2026-08-07T00:00:00+00:00",
        updated_at="2026-08-15T04:00:00+00:00",
    )
    _session_database(
        inactive_db,
        inactive_child,
        parent_id=bounded,
        db_name=None,
        messages=[],
        blocks=[
            (
                "inactive-old",
                "inactive-old-message",
                inactive_child,
                0,
                _ms("2026-08-10T10:00:00+00:00"),
                1,
                "assistant",
                "text",
                "Inactive old child",
                None,
                None,
                None,
                None,
                "model",
                None,
                None,
                None,
            )
        ],
        created_at="2026-08-08T00:00:00+00:00",
        updated_at="2026-08-10T11:00:00+00:00",
    )
    _session_database(
        active_db,
        active_child,
        parent_id=inactive_child,
        db_name=None,
        messages=[],
        blocks=[
            (
                "active-new",
                "active-new-message",
                active_child,
                0,
                _ms("2026-08-15T03:30:00+00:00"),
                1,
                "assistant",
                "text",
                "Active bounded child",
                None,
                None,
                None,
                None,
                "model",
                None,
                None,
                None,
            )
        ],
        created_at="2026-08-08T01:00:00+00:00",
        updated_at="2026-08-15T04:00:00+00:00",
    )
    _session_database(
        task_only_db,
        task_only_child,
        parent_id=bounded,
        db_name=None,
        messages=[],
        blocks=[
            (
                "task-only-old",
                "task-only-old-message",
                task_only_child,
                0,
                _ms("2026-08-10T12:00:00+00:00"),
                1,
                "assistant",
                "text",
                "Task-only child old conversation",
                None,
                None,
                None,
                None,
                "model",
                None,
                None,
                None,
            )
        ],
        created_at="2026-08-08T02:00:00+00:00",
        updated_at="2026-08-10T13:00:00+00:00",
    )
    connection = sqlite3.connect(bounded_db)
    try:
        connection.execute("INSERT INTO associated_dbs VALUES ('hatch2')")
        connection.commit()
    finally:
        connection.close()
    purpose_at = _ms("2026-08-15T02:14:48+00:00")
    owner_at = _ms("2026-08-15T02:16:40+00:00")
    _add_messages(
        bounded_db,
        (
            (
                0,
                bounded,
                _ms("2026-08-15T02:20:00+00:00"),
                {
                    "id": "earlier-row-later-clock",
                    "role": "User",
                    "source": json.dumps({"Submitted": {"source": "Tui"}}),
                    "created_at_ms": _ms("2026-08-15T02:20:00+00:00"),
                    "blocks": [
                        {
                            "type": "text",
                            "id": 99,
                            "text": "Earlier row must not cross the boundary",
                        }
                    ],
                },
            ),
            (
                1,
                bounded,
                _ms("2026-08-10T10:00:00+00:00"),
                {
                    "id": "unrelated-old",
                    "role": "User",
                    "source": json.dumps({"Submitted": {"source": "Tui"}}),
                    "created_at_ms": _ms("2026-08-10T10:00:00+00:00"),
                    "blocks": [{"type": "text", "id": 100, "text": "Old Hatch work"}],
                },
            ),
            (
                2,
                bounded,
                purpose_at,
                {
                    "id": "purpose-boundary",
                    "role": "User",
                    "source": None,
                    "created_at_ms": purpose_at,
                    "blocks": [
                        {"type": "InterjectionBlock", "id": 101, "text": "# Purpose Widget"}
                    ],
                },
            ),
            (
                3,
                bounded,
                owner_at,
                {
                    "id": "owner-restart",
                    "role": "User",
                    "source": json.dumps(
                        {"Submitted": {"source": {"Web": {"view": "Transcript"}}}}
                    ),
                    "created_at_ms": owner_at,
                    "blocks": [{"type": "text", "id": 102, "text": "Recover Widget agents"}],
                },
            ),
        ),
    )
    connection = sqlite3.connect(bounded_db)
    try:
        connection.execute(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                "v2-block-100",
                "v2-message-1",
                bounded,
                0,
                _ms("2026-08-10T10:00:00+00:00"),
                48,
                "user",
                "text",
                "Old Hatch work",
                None,
                None,
                None,
                None,
                None,
                json.dumps({"Submitted": {"source": "Tui"}}),
                None,
                None,
            ),
        )
        connection.commit()
    finally:
        connection.close()
    _index_database(
        source / ".orc" / "index.db",
        (
            (ROOT, None),
            (bounded, None),
            (inactive_child, bounded),
            (active_child, inactive_child),
            (task_only_child, bounded),
        ),
    )
    _task_database(project_db)
    _task_database(hatch_db)
    _task_database(task_only_task_db)
    for path, text in (
        (project_db, "Widget result after restart"),
        (hatch_db, "Unrelated Hatch result after restart"),
        (task_only_task_db, "Task-only child result after restart"),
    ):
        connection = sqlite3.connect(path)
        try:
            connection.execute("DELETE FROM task_notes")
            connection.execute(
                "INSERT INTO task_notes(task_id, content, created_at) VALUES (?, ?, ?)",
                ("task-a", text, "2026-08-15T03:00:00+00:00"),
            )
            connection.commit()
        finally:
            connection.close()

    spec = OrcContinuationSpec(bounded, "purpose-boundary")
    assert OrcContinuationSpec.from_value(spec, "spec") == spec
    snapshot = tmp_path / "bounded-snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "bounded", (spec,))
    copied_paths = {item.source_path for item in copied.sources}
    assert f".tg/{bounded}.db" in copied_paths
    assert ".tg/hatch2.db" not in copied_paths
    assert f".orc/sessions/{inactive_child}/session.db" not in copied_paths
    assert f".orc/sessions/{active_child}/session.db" in copied_paths
    assert f".orc/sessions/{task_only_child}/session.db" in copied_paths
    assert f".tg/{task_only_child}.db" in copied_paths
    active_source = next(
        item for item in copied.sources if item.owner_session_id == active_child
    )
    assert active_source.lineage_root_session_id == bounded
    link = copied.continuations[0]
    assert link.start_message_id == "purpose-boundary"
    assert link.start_source_line == 2
    assert link.started_at_ms == purpose_at

    team = load_orc_team(
        snapshot, ROOT, "bounded", "UTC", copied.sources, copied.continuations
    )
    bounded_agent = next(item for item in team.agents if item.thread_id == bounded)
    assert bounded_agent.started_at_ms == purpose_at
    assert not any("Old Hatch work" in (item.text or "") for item in team.events)
    assert not any(
        "Earlier row must not cross" in (item.text or "") for item in team.events
    )
    assert not any("# Purpose Widget" in (item.text or "") for item in team.events)
    owner = next(item for item in team.events if item.source_native_id == "owner-restart")
    assert owner.kind == "user_prompt"
    assert owner.timestamp_ms == owner_at
    assert owner.turn_id == f"orc-cont-1-{bounded[:8]}-orc-turn-{bounded[:8]}-50"
    active_agent = next(item for item in team.agents if item.thread_id == active_child)
    assert active_agent.parent_thread_id == bounded
    assert not any(item.thread_id == inactive_child for item in team.agents)
    task_only_agent = next(
        item for item in team.agents if item.thread_id == task_only_child
    )
    assert task_only_agent.parent_thread_id == bounded
    assert task_only_agent.started_at_ms == purpose_at
    assert any("Widget result after restart" in (item.text or "") for item in team.events)
    assert not any(
        "Unrelated Hatch result" in (item.text or "") for item in team.events
    )
    assert any(
        "Task-only child result after restart" in (item.text or "")
        for item in team.events
    )

    connection = sqlite3.connect(bounded_db)
    try:
        connection.execute(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                "v2-block-102",
                "v2-message-3",
                bounded,
                0,
                owner_at,
                50,
                "user",
                "text",
                "Recover Widget agents",
                None,
                None,
                None,
                None,
                None,
                json.dumps(
                    {"Submitted": {"source": {"Web": {"view": "Transcript"}}}}
                ),
                None,
                None,
            ),
        )
        connection.commit()
    finally:
        connection.close()
    materialized = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        copied.sources,
        "materialized",
        (spec,),
        copied.continuations,
    )
    materialized_team = load_orc_team(
        snapshot,
        ROOT,
        "bounded",
        "UTC",
        materialized.sources,
        materialized.continuations,
    )
    materialized_owner = next(
        item
        for item in materialized_team.events
        if item.source_native_id == "owner-restart"
    )
    assert materialized_owner == owner
    assert source_digest(materialized_team) == source_digest(team)

    connection = sqlite3.connect(task_only_task_db)
    try:
        connection.execute("DELETE FROM task_notes")
        connection.commit()
    finally:
        connection.close()
    task_note_deleted = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        materialized.sources,
        "task-note-deleted",
        (spec,),
        materialized.continuations,
    )
    deleted_team = load_orc_team(
        snapshot,
        ROOT,
        "bounded",
        "UTC",
        task_note_deleted.sources,
        task_note_deleted.continuations,
    )
    deleted_paths = {item.source_path for item in task_note_deleted.sources}
    assert f".orc/sessions/{task_only_child}/session.db" in deleted_paths
    assert f".tg/{task_only_child}.db" in deleted_paths
    assert any(
        "Task-only child result after restart" in (item.text or "")
        for item in deleted_team.events
    )
    deleted_projection = next(
        item.task_projection
        for item in task_note_deleted.sources
        if item.source_path == f".tg/{task_only_child}.db"
    )
    assert deleted_projection is not None
    assert deleted_projection.missing_note_count == 1
    task_note_repeat = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        task_note_deleted.sources,
        "task-note-deleted-repeat",
        (spec,),
        task_note_deleted.continuations,
    )
    assert task_note_repeat.files_changed == 0
    assert task_note_repeat.sources == task_note_deleted.sources


def test_recorded_lineage_root_beats_latest_root_time_fallback() -> None:
    roots = (ROOT, SUCCESSOR, "77777777-7777-7777-7777-777777777777")
    child = "88888888-8888-8888-8888-888888888888"
    missing_parent = "99999999-9999-9999-9999-999999999999"
    root_starts = {ROOT: 10, SUCCESSOR: 20, roots[2]: 30}
    metas = {
        root: orc_module._SessionMeta(root, None, root, None, start, start, root)
        for root, start in root_starts.items()
    }
    metas[child] = orc_module._SessionMeta(
        child,
        missing_parent,
        "active-grandchild",
        None,
        15,
        40,
        child,
    )

    resolved = orc_module._continuation_lineages(
        metas, roots, root_starts, {child: SUCCESSOR}
    )

    assert resolved[child] == SUCCESSOR


def test_session_state_migrates_content_only_snapshot_to_dual_table(
    tmp_path: Path,
) -> None:
    source = tmp_path / "storage-migration-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    timestamp = _ms("2026-08-15T10:00:00+00:00")
    block = (
        "v2-block-1",
        "v2-message-1",
        ROOT,
        0,
        timestamp,
        1,
        "user",
        "text",
        "Migrated prompt",
        None,
        None,
        None,
        None,
        None,
        json.dumps({"Submitted": {"source": "Tui"}}),
        None,
        None,
    )
    _session_database(
        root_db,
        ROOT,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=[block],
        created_at="2026-08-15T09:00:00+00:00",
        updated_at="2026-08-15T11:00:00+00:00",
    )
    _index_database(source / ".orc" / "index.db", ((ROOT, None),))
    snapshot = tmp_path / "storage-migration-snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "content-only")

    _add_messages(
        root_db,
        (
            (
                1,
                ROOT,
                timestamp,
                {
                    "id": "native-migrated-prompt",
                    "role": "User",
                    "source": json.dumps({"Submitted": {"source": "Tui"}}),
                    "created_at_ms": timestamp,
                    "blocks": [{"type": "text", "id": 1, "text": "Migrated prompt"}],
                },
            ),
        ),
    )
    second = snapshot_orc_lineage(
        source, ROOT, snapshot, first.sources, "dual-table"
    )
    repeated = snapshot_orc_lineage(
        source, ROOT, snapshot, second.sources, "dual-table-repeat"
    )
    assert second.files_changed > 0
    assert repeated.files_changed == 0
    assert repeated.sources == second.sources
    team = load_orc_team(snapshot, ROOT, "migration", "UTC", second.sources)
    event = next(item for item in team.events if item.event_id == "orc-block-v2-block-1")
    assert event.source_native_id == "native-migrated-prompt"
    assert event.source_line == 1


def test_session_state_migrates_old_messages_only_manifest(
    tmp_path: Path,
) -> None:
    source = tmp_path / "messages-state-migration-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    _session_database(
        root_db,
        ROOT,
        parent_id=None,
        db_name=None,
        messages=[],
        blocks=[],
        created_at="2026-08-15T09:00:00+00:00",
        updated_at="2026-08-15T11:00:00+00:00",
    )
    _add_messages(
        root_db,
        (
            (
                1,
                ROOT,
                _ms("2026-08-15T10:00:00+00:00"),
                {
                    "id": "status-only",
                    "role": "Assistant",
                    "source": None,
                    "created_at_ms": _ms("2026-08-15T10:00:00+00:00"),
                    "blocks": [{"type": "StatusBlock", "id": 1, "message": "idle"}],
                },
            ),
        ),
    )
    _index_database(source / ".orc" / "index.db", ((ROOT, None),))
    snapshot = tmp_path / "messages-state-migration-snapshot"
    captured = snapshot_orc_lineage(source, ROOT, snapshot, (), "captured")
    session_source = next(item for item in captured.sources if item.kind == "session")
    snapshot_path = _snapshot_database(snapshot, session_source)
    old_state = orc_module._logical_state(
        snapshot_path, "session", session_state_mode="messages-only"
    )
    old_meta = orc_module._session_meta(snapshot_path, session_source.source_path)
    old_auxiliary = orc_module._auxiliary_observation(snapshot_path, old_meta)
    old_identity = orc_module._session_semantic_identity(
        session_source.source_path,
        session_source.owner_session_id,
        old_state,
        old_meta,
        old_auxiliary,
        2,
    )
    old_source = replace(
        session_source,
        append_count=old_state.append_count,
        append_max_id=old_state.append_max_id,
        append_prefix_sha256=old_state.append_prefix_sha256,
        semantic_identity_mode="normalized-v2",
        semantic_sha256=old_identity.sha256,
        semantic_complete_bytes=old_identity.complete_bytes,
        canonical_semantic_sha256=None,
        canonical_semantic_complete_bytes=None,
        semantic_baseline_path=None,
    )
    previous = tuple(
        old_source if item.source_path == old_source.source_path else item
        for item in captured.sources
    )
    migrated = snapshot_orc_lineage(
        source, ROOT, snapshot, previous, "migrated"
    )
    repeated = snapshot_orc_lineage(
        source, ROOT, snapshot, migrated.sources, "repeat"
    )
    assert repeated.files_changed == 0
    assert repeated.sources == migrated.sources
    before_digest = next(
        item.semantic_sha256 for item in migrated.sources if item.kind == "session"
    )
    connection = sqlite3.connect(root_db)
    try:
        ignored = {
            "id": "second-status-only",
            "role": "Assistant",
            "source": None,
            "created_at_ms": _ms("2026-08-15T10:01:00+00:00"),
            "blocks": [{"type": "StatusBlock", "id": 2, "message": "still idle"}],
        }
        connection.execute(
            "INSERT INTO messages(id, session_id, role, created_at_ms, "
            "message_json, search_text) VALUES (2, ?, 'assistant', ?, ?, NULL)",
            (
                ROOT,
                _ms("2026-08-15T10:01:00+00:00"),
                json.dumps(ignored, separators=(",", ":")),
            ),
        )
        connection.commit()
    finally:
        connection.close()
    ignored_advanced = snapshot_orc_lineage(
        source, ROOT, snapshot, repeated.sources, "ignored-append"
    )
    ignored_repeated = snapshot_orc_lineage(
        source, ROOT, snapshot, ignored_advanced.sources, "ignored-repeat"
    )
    assert next(
        item.semantic_sha256
        for item in ignored_advanced.sources
        if item.kind == "session"
    ) == before_digest
    assert ignored_repeated.files_changed == 0


def test_snapshot_and_parse_orc_lineage_read_only_and_idempotently(
    tmp_path: Path,
) -> None:
    source, root_db, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    root_db.chmod(0o444)
    task_db.chmod(0o444)

    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    team = load_orc_team(snapshot, ROOT, "orc-test", "America/New_York", first.sources)
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")

    assert len(first.sources) == 3
    assert second.files_changed == 0
    assert second.sources == first.sources
    snapshot_databases = tuple(
        _snapshot_database(snapshot, source_copy) for source_copy in first.sources
    )
    for snapshot_database in snapshot_databases:
        connection = sqlite3.connect(
            snapshot_database.resolve().as_uri() + "?mode=ro&immutable=1",
            uri=True,
        )
        try:
            assert connection.execute("PRAGMA journal_mode").fetchone() == ("delete",)
        finally:
            connection.close()
        assert not snapshot_database.with_name(snapshot_database.name + "-wal").exists()
        assert not snapshot_database.with_name(snapshot_database.name + "-shm").exists()
    assert stat.S_IMODE(root_db.stat().st_mode) & 0o222 == 0
    assert stat.S_IMODE(task_db.stat().st_mode) & 0o222 == 0
    nested = next(agent for agent in team.agents if agent.thread_id == NESTED)
    assert nested.parent_thread_id == ROOT
    assert nested.depth == 1
    workers = [agent for agent in team.agents if agent.agent_path.endswith("/worker")]
    assert len(workers) == 2
    first_event = next(event for event in team.events if "First incarnation" in (event.text or ""))
    second_event = next(
        event for event in team.events if "Second incarnation" in (event.text or "")
    )
    assert first_event.thread_id != second_event.thread_id
    assert first_event.kind == "inter_agent_message"
    assert first_event.phase is None
    assert first_event.author == first_event.thread_id
    assert first_event.recipient == ROOT
    tools = team.tool_calls
    assert len(tools) == 1
    assert tools[0].nested_tools == (("readFile", 2), ("sendAgent", 1))


def test_classifies_orc_inputs_from_user_source_and_extra(tmp_path: Path) -> None:
    source, root_db, _ = _fixture(tmp_path)
    inputs: list[tuple[object, ...]] = [
        (
            "gchat-legacy-owner",
            "message-gchat-legacy-owner",
            ROOT,
            0,
            _ms("2026-07-21T04:59:00+00:00"),
            9,
            "user",
            "text",
            "Legacy owner message",
            None,
            None,
            None,
            None,
            None,
            json.dumps(
                {
                    "GChat": {
                        "message_name": "spaces/x/messages/legacy-owner",
                        "sender_name": "users/owner-id",
                    }
                }
            ),
            None,
            None,
        ),
        (
            "gchat-owner",
            "message-gchat-owner",
            ROOT,
            0,
            _ms("2026-07-21T05:00:00+00:00"),
            10,
            "user",
            "text",
            "Owner message",
            None,
            None,
            None,
            None,
            None,
            json.dumps(
                {
                    "GChat": {
                        "message_name": "spaces/x/messages/owner",
                        "sender_unixname": "newton",
                        "sender_name": "users/owner-id",
                        "is_owner": True,
                    }
                }
            ),
            None,
            json.dumps({"sender_display_name": "Alice Doe"}),
        ),
        (
            "gchat-other",
            "message-gchat-other",
            ROOT,
            0,
            _ms("2026-07-21T05:01:00+00:00"),
            11,
            "user",
            "text",
            "Another person's message",
            None,
            None,
            None,
            None,
            None,
            json.dumps(
                {
                    "GChat": {
                        "message_name": "spaces/x/messages/other",
                        "sender_display_name": "A Collaborator",
                        "is_owner": False,
                    }
                }
            ),
            None,
            None,
        ),
        (
            "orc-child",
            "message-orc-child",
            ROOT,
            0,
            _ms("2026-07-21T05:02:00+00:00"),
            12,
            "user",
            "text",
            "Child Orc report",
            None,
            None,
            None,
            None,
            None,
            json.dumps({"Orc": {"sender_session": "child-session"}}),
            None,
            None,
        ),
        (
            "submitted-web",
            "message-submitted-web",
            ROOT,
            0,
            _ms("2026-07-21T05:03:00+00:00"),
            13,
            "user",
            "text",
            "Web submission",
            None,
            None,
            None,
            None,
            None,
            json.dumps({"Submitted": {"source": {"Web": {"view": "Inbox"}}}}),
            None,
            None,
        ),
        (
            "tui-unknown",
            "message-tui-unknown",
            ROOT,
            0,
            _ms("2026-07-21T05:04:00+00:00"),
            14,
            "user",
            "text",
            "Unattributed terminal input",
            None,
            None,
            None,
            None,
            None,
            json.dumps({"Submitted": {"source": "Tui"}}),
            None,
            None,
        ),
        (
            "scheduled-reminder",
            "message-scheduled-reminder",
            ROOT,
            0,
            _ms("2026-07-21T05:05:00+00:00"),
            15,
            "user",
            "text",
            "This is your periodic reminder to make sure your running state is aligned "
            "with your overarching goals, listed below.",
            None,
            None,
            None,
            None,
            None,
            json.dumps({"Submitted": {"source": "Tui"}}),
            None,
            None,
        ),
        (
            "scheduled-web-reminder",
            "message-scheduled-web-reminder",
            ROOT,
            0,
            _ms("2026-07-21T05:06:00+00:00"),
            16,
            "user",
            "text",
            "This is your periodic reminder to make sure your running state is aligned "
            "with your overarching goals, listed below.",
            None,
            None,
            None,
            None,
            None,
            json.dumps({"Submitted": {"source": {"Web": {"view": "Inbox"}}}}),
            None,
            None,
        ),
    ]
    connection = sqlite3.connect(root_db)
    try:
        connection.executemany(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            inputs,
        )
        connection.commit()
    finally:
        connection.close()

    snapshot = tmp_path / "snapshot"
    captured = snapshot_orc_lineage(source, ROOT, snapshot, (), "captured")
    team = load_orc_team(snapshot, ROOT, "orc-authorship", "UTC", captured.sources)
    events = {event.event_id: event for event in team.events}

    owner = events["orc-block-gchat-owner"]
    assert owner.kind == "user_prompt"
    assert owner.author == "newton"
    assert owner.author_kind == "owner_human"
    assert owner.ingress_kind == "gchat"
    assert owner.source_native_id == "spaces/x/messages/owner"
    assert events["orc-block-gchat-legacy-owner"].author_kind == "owner_human"
    other = events["orc-block-gchat-other"]
    assert other.kind == "external_message"
    assert other.author_kind == "other_human"
    child = events["orc-block-orc-child"]
    assert child.kind == "inter_agent_message"
    assert child.author == "child-session"
    assert child.recipient == ROOT
    assert events["orc-block-submitted-web"].author_kind == "external_or_unknown"
    assert events["orc-block-tui-unknown"].author_kind == "unknown"
    scheduled = events["orc-block-scheduled-reminder"]
    assert scheduled.kind == "system_input"
    assert scheduled.ingress_kind == "scheduled"
    assert scheduled.author_kind == "system"
    assert events["orc-block-scheduled-web-reminder"].kind == "system_input"
    assert all(
        event.classification_version == "authorship-v2"
        for event in events.values()
        if event.event_id.startswith("orc-block-")
    )


def test_rewritten_auxiliary_history_accepts_stable_spawns_and_is_idempotent(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )

    _append_root_message(root_db, "root-appended", "Authoritative appended result")
    _rewrite_conversation(root_db, _rewritten_messages())
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    root_source = next(
        source_copy
        for source_copy in second.sources
        if source_copy.kind == "session" and source_copy.owner_session_id == ROOT
    )

    assert {event.event_id for event in before.events} < {
        event.event_id for event in after.events
    }
    assert {agent.thread_id for agent in before.agents} < {
        agent.thread_id for agent in after.agents
    }
    assert root_source.auxiliary.message_count == 3
    assert root_source.auxiliary.stable_spawn_count == 3
    assert root_source.auxiliary.rewrite_count == 1
    assert root_source.auxiliary.last_rewrite_at == "second"
    assert root_source.auxiliary.degraded is True
    assert root_source.auxiliary.degradation_reason == (
        "conversation-history-rewritten-stable-spawns-preserved"
    )

    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    assert third.files_changed == 0
    assert third.sources == second.sources


def test_repeated_auxiliary_rewrites_increment_once_per_distinct_observation(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    _rewrite_conversation(root_db, _non_agent_rewrite_messages("first rewrite"))
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    _rewrite_conversation(root_db, _non_agent_rewrite_messages("second rewrite"))
    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    fourth = snapshot_orc_lineage(source, ROOT, snapshot, third.sources, "fourth")
    root_second = next(item for item in second.sources if item.kind == "session")
    root_third = next(item for item in third.sources if item.kind == "session")
    root_fourth = next(item for item in fourth.sources if item.kind == "session")

    assert root_second.auxiliary.rewrite_count == 1
    assert root_second.auxiliary.last_rewrite_at == "second"
    assert root_third.auxiliary.rewrite_count == 2
    assert root_third.auxiliary.last_rewrite_at == "third"
    assert root_fourth.auxiliary.rewrite_count == 2
    assert root_fourth.auxiliary.last_rewrite_at == "third"
    assert fourth.files_changed == 0


@pytest.mark.parametrize(
    ("rewrite_kind", "message"),
    (
        ("agent", "stable spawn evidence was rewritten"),
        ("message", "stable spawn evidence was rewritten"),
        ("timestamp", "stable spawn evidence was rewritten"),
        ("missing", "stable spawn evidence disappeared"),
    ),
)
def test_rewritten_stable_spawn_is_rejected_and_snapshot_is_preserved(
    tmp_path: Path, rewrite_kind: str, message: str
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    root_source = next(
        item
        for item in first.sources
        if item.kind == "session" and item.owner_session_id == ROOT
    )
    snapshot_db = _snapshot_database(snapshot, root_source)
    prior_bytes = snapshot_db.read_bytes()
    _append_root_message(root_db, "root-appended", "Must not replace the snapshot")
    rewritten = _rewritten_messages(
        first_agent="renamed-worker" if rewrite_kind == "agent" else "worker"
    )
    if rewrite_kind == "message":
        rewritten[0]["id"] = 99
    if rewrite_kind == "timestamp":
        rewritten[0]["created_at_ms"] = _ms("2026-07-21T03:00:01+00:00")
    if rewrite_kind == "missing":
        rewritten = rewritten[1:]
    _rewrite_conversation(root_db, rewritten)

    with pytest.raises(OrcParseError, match=message):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    assert snapshot_db.read_bytes() == prior_bytes


def test_window_excludes_exact_end_but_retains_pre_window_spawn_context(
    tmp_path: Path,
) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    team = load_orc_team(snapshot, ROOT, "orc-test", "America/New_York", copied.sources)
    window = parse_date_window("2026-07-21", "2026-07-22", "America/New_York")
    assert window is not None
    assert window.start_ms is not None
    phases = build_phases(apply_date_window(team, window))
    transcript = "\n".join(phase.transcript_text for phase in phases)

    assert "Start the complete local day" in transcript
    assert "First incarnation finding" in transcript
    assert "Second incarnation result" in transcript
    assert "Exactly at exclusive end" not in transcript
    assert "Outside the half-open window" not in transcript
    assert any(agent.started_at_ms < window.start_ms for agent in team.agents)
    assert sum(phase.stats.inter_agent_messages for phase in phases) == 3


def test_rewritten_task_note_prefix_is_rejected(tmp_path: Path) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE task_notes SET content = 'rewritten' WHERE id = 1"
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="immutable core was rewritten"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_forged_task_semantic_alias_is_rejected(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    forged = tuple(
        replace(item, semantic_sha256="0" * 64, semantic_complete_bytes=123)
        if item.kind == "task"
        else item
        for item in first.sources
    )

    with pytest.raises(OrcParseError, match="task semantic alias"):
        snapshot_orc_lineage(source, ROOT, snapshot, forged, "forged")


def test_stale_session_semantic_alias_baseline_is_rejected(tmp_path: Path) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    first_root = next(item for item in first.sources if item.owner_session_id == ROOT)
    first_path = _snapshot_database(snapshot, first_root)
    first_meta = orc_module._session_meta(first_path, first_root.source_path)
    first_auxiliary = orc_module._auxiliary_observation(first_path, first_meta)
    old_raw_identity = orc_module._session_semantic_identity(
        first_root.source_path,
        first_root.owner_session_id,
        orc_module._logical_state(first_path, "session"),
        first_meta,
        first_auxiliary,
        2,
    )
    _append_root_message(root_db, "new-semantic-event", "New semantic event")
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    forged = tuple(
        replace(
            item,
            semantic_sha256=old_raw_identity.sha256,
            semantic_complete_bytes=old_raw_identity.complete_bytes,
            semantic_alias_baseline_path=first_root.snapshot_path,
        )
        if item.owner_session_id == ROOT
        else item
        for item in second.sources
    )

    with pytest.raises(OrcParseError, match="different canonical semantics"):
        load_orc_team(snapshot, ROOT, "orc-test", "UTC", forged)


@pytest.mark.parametrize("field", ("owner", "title"))
def test_rewritten_task_metadata_preserves_frozen_note_enrichment(
    tmp_path: Path, field: str
) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            f"UPDATE tasks SET {field} = ? WHERE local_id = 'task-a'",
            (f"changed-{field}",),
        )
        connection.commit()
    finally:
        connection.close()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    task_source = next(item for item in second.sources if item.kind == "task")

    assert _semantic_team(after) == _semantic_team(before)
    assert source_digest(after) == source_digest(before)
    assert task_source.task_projection is not None
    assert task_source.task_projection.rewrite_count == 1
    assert task_source.task_projection.degraded is True


def test_growing_mutable_task_metadata_preserves_semantic_source_digest(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    first_task = next(item for item in first.sources if item.kind == "task")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE tasks SET title = ? WHERE local_id = 'task-a'",
            ("expanded-" + "x" * 100_000,),
        )
        connection.commit()
    finally:
        connection.close()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    second_task = next(item for item in second.sources if item.kind == "task")

    assert second_task.snapshot_size > first_task.snapshot_size
    assert _semantic_team(after) == _semantic_team(before)
    assert source_digest(after) == source_digest(before)


def test_task_note_server_sync_id_preserves_team_digest_and_summary_cache(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    before, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    summarize_archive(archive, "orc-test", "heuristic", "fixture")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE task_notes SET server_comment_id = 'server-1' WHERE id = 1"
        )
        connection.commit()
    finally:
        connection.close()

    after, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    cached = summarize_archive(archive, "orc-test", "heuristic", "fixture")
    task = next(item for item in after.sources if item.path == ".tg/project.db")
    task_manifest = next(
        item
        for item in as_array(
            as_object(
                read_json(
                    team_build_root(archive, "orc-test")
                    / "raw"
                    / "source-manifest.json"
                ),
                "manifest",
            ).get("sources"),
            "manifest.sources",
        )
        if as_object(item, "manifest.source").get("kind") == "task"
    )
    projection = as_object(
        as_object(task_manifest, "manifest.task").get("task_projection"),
        "manifest.task.task_projection",
    )

    assert _semantic_team(after) == _semantic_team(before)
    assert source_digest(after) == source_digest(before)
    assert task.semantic_sha256 is not None
    assert projection.get("rewrite_count") == 0
    assert cached.cache_misses == 0
    assert cached.cache_hits > 0


def test_task_note_author_fill_and_change_preserve_frozen_attribution_and_cache(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    before, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    summarize_archive(archive, "orc-test", "heuristic", "fixture")

    for author, expected_rewrites in (("reviewer", 1), ("third-owner", 2)):
        connection = sqlite3.connect(task_db)
        try:
            connection.execute(
                "UPDATE task_notes SET author_unixname = ? WHERE id = 1",
                (author,),
            )
            connection.commit()
        finally:
            connection.close()
        after, _ = ingest_orc(
            archive, source, ROOT, "orc-test", "America/New_York"
        )
        cached = summarize_archive(
            archive, "orc-test", "heuristic", "fixture"
        )
        manifest_path = (
            team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
        )
        manifest = as_object(read_json(manifest_path), str(manifest_path))
        task = next(
            as_object(item, "manifest.source")
            for item in as_array(manifest.get("sources"), "manifest.sources")
            if as_object(item, "manifest.source").get("kind") == "task"
        )
        projection = as_object(
            task.get("task_projection"), "manifest.task.task_projection"
        )

        assert _semantic_team(after) == _semantic_team(before)
        assert source_digest(after) == source_digest(before)
        assert projection.get("rewrite_count") == expected_rewrites
        assert cached.cache_misses == 0
        assert cached.cache_hits > 0


def test_external_task_note_is_user_provenance_without_agent_or_edge(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE task_notes SET author_unixname = 'external-reviewer' WHERE id = 1"
        )
        connection.commit()
    finally:
        connection.close()

    copied = snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")
    team = load_orc_team(
        tmp_path / "snapshot", ROOT, "orc-test", "America/New_York", copied.sources
    )
    event = next(item for item in team.events if item.event_id.endswith("-1"))
    phases = build_phases(team)

    assert event.kind == "external_message"
    assert event.role == "user"
    assert event.thread_id == ROOT
    assert event.author == "external-reviewer"
    assert event.recipient is None
    assert "external author: external-reviewer" in (event.text or "")
    assert all("external-reviewer" not in agent.agent_path for agent in team.agents)
    assert all(edge.call_id != event.event_id for edge in team.edges)
    assert sum(phase.stats.external_messages for phase in phases) == 1
    assert sum(phase.stats.user_prompts for phase in phases) == 1


def test_lone_external_task_note_creates_a_visible_phase(tmp_path: Path) -> None:
    source, root_db, task_db = _fixture(tmp_path)
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    for session_db in (root_db, nested_db):
        connection = sqlite3.connect(session_db)
        try:
            connection.execute("DELETE FROM content_blocks")
            connection.commit()
        finally:
            connection.close()
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes WHERE id != 1")
        connection.execute(
            "UPDATE task_notes SET author_unixname = 'external-reviewer' WHERE id = 1"
        )
        connection.commit()
    finally:
        connection.close()

    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    team = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", copied.sources
    )
    phases = build_phases(team)

    assert len(team.events) == 1
    assert len(phases) == 1
    assert phases[0].stats.external_messages == 1
    assert "EXTERNAL:" in phases[0].transcript_text
    assert "First incarnation finding" in phases[0].transcript_text


def test_unattributed_local_task_notes_are_preserved_on_synthetic_worker(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("UPDATE tasks SET owner = NULL WHERE local_id = 'task-a'")
        connection.commit()
    finally:
        connection.close()

    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    team = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", copied.sources
    )
    notes = [event for event in team.events if event.event_id.startswith("orc-note-")]
    unattributed = next(
        agent for agent in team.agents if agent.agent_path.endswith("/Unattributed Task Work")
    )

    assert {event.source_line for event in notes} == {1, 2, 3, 4}
    assert {event.thread_id for event in notes} == {unattributed.thread_id}
    assert all(event.kind == "inter_agent_message" for event in notes)
    assert all("unattributed local task work" in (event.text or "") for event in notes)
    assert sum(
        edge.kind == "message" and edge.from_thread_id == unattributed.thread_id
        for edge in team.edges
    ) == 4


def test_new_task_identity_is_a_monotonic_addition(tmp_path: Path) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "INSERT INTO tasks VALUES ('task-b', 'New task', 'reviewer')"
        )
        connection.commit()
    finally:
        connection.close()
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    first_task = next(item for item in first.sources if item.kind == "task")
    task_source = next(item for item in second.sources if item.kind == "task")

    assert task_source.task_projection is not None
    assert first_task.task_projection is not None
    assert task_source.task_projection.note_count == 4
    assert task_source.task_projection.path == first_task.task_projection.path
    assert task_source.semantic_sha256 == first_task.semantic_sha256


def test_new_note_captures_current_enrichment_then_freezes_it(tmp_path: Path) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE tasks SET owner = 'reviewer', title = 'Reassigned task title' "
            "WHERE local_id = 'task-a'"
        )
        connection.commit()
    finally:
        connection.close()
    _append_task_note(task_db, "Finding after reassignment")
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after_append = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    new_event = next(
        event for event in after_append.events if event.event_id.endswith("-5")
    )
    prior_events = {event.event_id: event for event in before.events}

    assert new_event.text is not None
    assert new_event.text.startswith("[task-a · Reassigned task title]")
    assert all(
        event == prior_events[event.event_id]
        for event in after_append.events
        if event.event_id in prior_events
    )
    second_task = next(item for item in second.sources if item.kind == "task")
    assert second_task.task_projection is not None
    assert second_task.task_projection.note_count == 5
    assert second_task.task_projection.rewrite_count == 1

    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE tasks SET owner = 'third-owner', title = 'Third mutable title' "
            "WHERE local_id = 'task-a'"
        )
        connection.commit()
    finally:
        connection.close()
    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    after_rewrite = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", third.sources
    )
    third_task = next(item for item in third.sources if item.kind == "task")

    assert _semantic_team(after_rewrite) == _semantic_team(after_append)
    assert source_digest(after_rewrite) == source_digest(after_append)
    assert third_task.task_projection is not None
    assert third_task.task_projection.rewrite_count == 2
    assert third_task.task_projection.last_rewrite_at == "third"


def test_semantic_identity_is_equal_for_one_shot_and_two_step_ingest(
    tmp_path: Path,
) -> None:
    one_source, one_root, one_task = _fixture(tmp_path / "one")
    two_source, two_root, two_task = _fixture(tmp_path / "two")
    _append_root_message(one_root, "root-appended", "deterministic final state")
    _append_task_note(one_task, "deterministic task state")
    one_snapshot = tmp_path / "one-snapshot"
    one = snapshot_orc_lineage(one_source, ROOT, one_snapshot, (), "one")
    one_team = load_orc_team(
        one_snapshot, ROOT, "orc-test", "America/New_York", one.sources
    )

    two_snapshot = tmp_path / "two-snapshot"
    two_initial = snapshot_orc_lineage(two_source, ROOT, two_snapshot, (), "first")
    _append_root_message(two_root, "root-appended", "deterministic final state")
    _append_task_note(two_task, "deterministic task state")
    two = snapshot_orc_lineage(
        two_source, ROOT, two_snapshot, two_initial.sources, "second"
    )
    two_team = load_orc_team(
        two_snapshot, ROOT, "orc-test", "America/New_York", two.sources
    )

    one_identities = {
        item.source_path: (
            item.semantic_identity_mode,
            item.semantic_sha256,
            item.semantic_complete_bytes,
        )
        for item in one.sources
    }
    two_identities = {
        item.source_path: (
            item.semantic_identity_mode,
            item.semantic_sha256,
            item.semantic_complete_bytes,
        )
        for item in two.sources
    }
    assert one_identities == two_identities
    assert source_digest(one_team) == source_digest(two_team)
    assert _semantic_team(one_team) == _semantic_team(two_team)


def test_tampered_semantic_identity_is_rejected(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    session = next(item for item in first.sources if item.kind == "session")
    tampered = replace(session, semantic_sha256="0" * 64)
    sources = tuple(tampered if item == session else item for item in first.sources)

    with pytest.raises(OrcParseError, match="deterministic semantic identity"):
        snapshot_orc_lineage(source, ROOT, snapshot, sources, "second")


@pytest.mark.parametrize(
    ("field", "value", "message"),
    (
        (
            "created_at",
            "2026-07-20T20:00:00+00:00",
            "immutable session metadata was rewritten",
        ),
        (
            "updated_at",
            "2026-07-19T20:00:00+00:00",
            "session updated_at moved backwards",
        ),
    ),
)
def test_session_metadata_rewrite_is_rejected(
    tmp_path: Path, field: str, value: str, message: str
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(f"UPDATE session_meta SET {field} = ?", (value,))
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match=message):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_session_name_change_is_accepted_as_semantic_change(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    connection = sqlite3.connect(nested_db)
    try:
        connection.execute("UPDATE session_meta SET name = 'renamed-worker'")
        connection.commit()
    finally:
        connection.close()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )

    assert source_digest(after) != source_digest(before)
    assert any(agent.nickname == "renamed-worker" for agent in after.agents)


def test_rewritten_task_history_is_frozen_and_extended_idempotently(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes WHERE id = 4")
        connection.execute("DELETE FROM tasks WHERE local_id = 'task-a'")
        connection.execute(
            "INSERT INTO tasks VALUES ('task-b', 'Replacement task', 'reviewer')"
        )
        connection.execute(
            "INSERT INTO task_notes(task_id, content, created_at) VALUES "
            "('task-b', 'New work after rewrite', '2026-07-22T05:00:00+00:00')"
        )
        connection.commit()
    finally:
        connection.close()
    live_bytes = task_db.read_bytes()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    task_source = next(item for item in second.sources if item.kind == "task")
    source_snapshot = next(item for item in after.sources if item.path == ".tg/project.db")

    assert task_db.read_bytes() == live_bytes
    assert task_source.append_count == 4
    assert task_source.task_projection is not None
    assert task_source.task_projection.note_count == 5
    assert task_source.task_projection.missing_note_count == 1
    assert task_source.task_projection.rewrite_count == 1
    assert task_source.task_projection.degraded is True
    assert source_snapshot.line_count == 5
    assert {event.source_line for event in after.events if event.event_id.startswith("orc-note-")} == {
        1,
        2,
        3,
        4,
        5,
    }
    assert all(event in after.events for event in before.events)

    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    third_task = next(item for item in third.sources if item.kind == "task")
    assert third.files_changed == 0
    assert third_task.task_projection == task_source.task_projection
    assert task_db.read_bytes() == live_bytes


def test_new_task_note_without_current_task_row_is_rejected(tmp_path: Path) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "INSERT INTO task_notes(task_id, content, created_at) VALUES "
            "('missing-task', 'Cannot enrich me', '2026-07-22T05:00:00+00:00')"
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="lacks its task row"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_task_note_id_reuse_below_frozen_highwater_is_rejected(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes WHERE id = 2")
        connection.commit()
    finally:
        connection.close()
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    first_task = next(item for item in first.sources if item.kind == "task")
    assert first_task.task_projection is not None
    assert first_task.task_projection.unobserved_note_id_gap_count == 1
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "INSERT INTO task_notes(id, task_id, content, created_at) VALUES "
            "(2, 'task-a', 'Reused identity', '2026-07-22T05:00:00+00:00')"
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="reused below frozen highwater"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_task_note_id_reuse_in_prior_trailing_allocation_gap_is_rejected(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes WHERE id = 4")
        connection.commit()
    finally:
        connection.close()
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    first_task = next(item for item in first.sources if item.kind == "task")
    assert first_task.task_projection is not None
    assert first_task.task_projection.observed_note_sequence == 4
    assert first_task.task_projection.unobserved_note_id_gap_count == 1
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "INSERT INTO task_notes(id, task_id, content, created_at) VALUES "
            "(4, 'task-a', 'Reused trailing allocation', "
            "'2026-07-22T05:00:00+00:00')"
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="reused below frozen highwater"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_task_note_allocation_sequence_rollback_is_rejected(tmp_path: Path) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes WHERE id IN (3, 4)")
        connection.execute(
            "UPDATE sqlite_sequence SET seq = 2 WHERE name = 'task_notes'"
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="allocation sequence regressed"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_disappeared_task_database_is_rejected(tmp_path: Path) -> None:
    source, _, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    task_db.unlink()

    with pytest.raises(OrcParseError, match="source disappeared"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")


def test_provider_initial_session_without_name_uses_session_task_database(
    tmp_path: Path,
) -> None:
    source, root_db, task_db = _fixture(tmp_path)
    fallback = source / ".tg" / f"{ROOT}.db"
    task_db.replace(fallback)
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("UPDATE session_meta SET db_name = NULL")
        connection.commit()
    finally:
        connection.close()

    copied = snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")

    assert {item.source_path for item in copied.sources if item.kind == "task"} == {
        f".tg/{ROOT}.db"
    }


def test_delegated_session_uses_its_id_even_when_db_name_is_inherited(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    delegated_task = source / ".tg" / f"{NESTED}.db"
    shutil.copy2(task_db, delegated_task)
    connection = sqlite3.connect(nested_db)
    try:
        connection.execute("UPDATE session_meta SET db_name = 'project'")
        connection.commit()
    finally:
        connection.close()

    copied = snapshot_orc_lineage(
        source, NESTED, tmp_path / "snapshot", (), "first"
    )

    assert {item.source_path for item in copied.sources if item.kind == "task"} == {
        f".tg/{NESTED}.db"
    }


def test_missing_never_observed_task_references_are_lazy(tmp_path: Path) -> None:
    source, root_db, _, = _fixture(tmp_path)
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("UPDATE session_meta SET db_name = 'not-created'")
        connection.execute(
            "INSERT INTO associated_dbs(db_name) VALUES ('also-not-created')"
        )
        connection.commit()
    finally:
        connection.close()

    copied = snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")

    assert all(item.kind == "session" for item in copied.sources)


def test_no_index_ignores_unrelated_session_debris(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    (source / ".orc" / "index.db").unlink()
    unrelated = source / ".orc" / "sessions" / "unrelated" / "session.db"
    unrelated.parent.mkdir(parents=True)
    unrelated.write_bytes(b"not a sqlite database")

    copied = snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")

    assert {item.owner_session_id for item in copied.sources if item.kind == "session"} == {
        ROOT
    }
    assert {item.source_path for item in copied.sources if item.kind == "task"} == {
        ".tg/project.db"
    }


def test_indexed_selected_corrupt_child_fails_closed(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    child = "33333333-3333-3333-3333-333333333333"
    _append_index_session(source / ".orc" / "index.db", child, ROOT)
    database = source / ".orc" / "sessions" / child / "session.db"
    database.parent.mkdir(parents=True)
    database.write_bytes(b"not a sqlite database")

    with pytest.raises(OrcParseError, match="failed to inspect Orc session metadata"):
        snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")


def test_associated_database_reference_change_during_snapshot_retries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    original_backup = orc_module._backup_database
    changed = False

    def mutate_then_backup(
        source_path: Path, destination: Path, snapshot_root: Path
    ) -> None:
        nonlocal changed
        if not changed and source_path == root_db:
            connection = sqlite3.connect(root_db)
            try:
                connection.execute(
                    "INSERT INTO associated_dbs(db_name) VALUES ('during-copy')"
                )
                connection.commit()
            finally:
                connection.close()
            changed = True
        original_backup(source_path, destination, snapshot_root)

    monkeypatch.setattr(orc_module, "_backup_database", mutate_then_backup)
    with pytest.raises(OrcParseError, match="references changed during snapshot"):
        snapshot_orc_lineage(source, ROOT, snapshot, (), "first")

    assert not tuple(snapshot.glob(".objects/*/*.db"))


def test_parent_lifetime_contains_nested_session_activity(tmp_path: Path) -> None:
    source, root_db, task_db = _fixture(tmp_path)
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("DELETE FROM content_blocks")
        connection.execute(
            "UPDATE conversation_state SET conversation_json = '{\"messages\":[]}'"
        )
        connection.commit()
    finally:
        connection.close()
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes")
        connection.commit()
    finally:
        connection.close()

    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    team = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", copied.sources
    )
    root = next(agent for agent in team.agents if agent.thread_id == ROOT)
    nested = next(agent for agent in team.agents if agent.thread_id == NESTED)

    assert nested.ended_at_ms is not None
    assert root.ended_at_ms is not None
    assert root.ended_at_ms >= nested.ended_at_ms
    assert root.ended_at_ms > root.started_at_ms + 1


def test_lifetime_end_propagates_across_two_nested_hops(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    child = "33333333-3333-3333-3333-333333333333"
    child_block = (
        "child-late",
        "child-message",
        child,
        0,
        _ms("2026-07-23T12:00:00+00:00"),
        1,
        "assistant",
        "text",
        "Grandchild coordinator activity",
        None,
        None,
        None,
        None,
        "model",
        None,
        None,
        None,
    )
    _session_database(
        source / ".orc" / "sessions" / child / "session.db",
        child,
        parent_id=NESTED,
        db_name="inherited-name",
        messages=[],
        blocks=[child_block],
    )
    _append_index_session(source / ".orc" / "index.db", child, NESTED)

    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    team = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", copied.sources
    )
    agents = {agent.thread_id: agent for agent in team.agents}

    child_end = agents[child].ended_at_ms
    nested_end = agents[NESTED].ended_at_ms
    root_end = agents[ROOT].ended_at_ms
    assert child_end is not None
    assert nested_end is not None
    assert root_end is not None
    assert nested_end >= child_end
    assert root_end >= nested_end


def test_shared_task_database_keeps_its_recorded_owner(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    earlier = "00000000-0000-0000-0000-000000000001"
    _session_database(
        source / ".orc" / "sessions" / earlier / "session.db",
        earlier,
        parent_id=ROOT,
        db_name="project",
        messages=[],
        blocks=[],
    )
    connection = sqlite3.connect(
        source / ".orc" / "sessions" / earlier / "session.db"
    )
    try:
        connection.execute(
            "INSERT INTO associated_dbs(db_name) VALUES ('project')"
        )
        connection.commit()
    finally:
        connection.close()
    _append_index_session(source / ".orc" / "index.db", earlier, ROOT)
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    task_source = next(item for item in second.sources if item.kind == "task")

    assert task_source.owner_session_id == ROOT


def test_shared_task_database_keeps_owner_after_its_attachment_is_removed(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    for path, db_name in ((root_db, None), (nested_db, "ignored-inherited-name")):
        connection = sqlite3.connect(path)
        try:
            connection.execute("UPDATE session_meta SET db_name = ?", (db_name,))
            if path == nested_db:
                connection.execute(
                    "INSERT INTO associated_dbs(db_name) VALUES ('project')"
                )
            connection.commit()
        finally:
            connection.close()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    task = next(item for item in second.sources if item.kind == "task")

    assert task.owner_session_id == ROOT
    assert task.source_state == "live"


def test_late_task_database_attachment_is_discovered(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    late_task_db = source / ".tg" / "late.db"
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    _task_database(late_task_db)
    connection = sqlite3.connect(nested_db)
    try:
        connection.execute(
            "INSERT INTO associated_dbs(db_name) VALUES ('late')"
        )
        connection.commit()
    finally:
        connection.close()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")

    assert {item.source_path for item in second.sources} == {
        *{item.source_path for item in first.sources},
        ".tg/late.db",
    }


def test_detached_task_database_retains_archived_history(tmp_path: Path) -> None:
    source, root_db, task_db = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("UPDATE session_meta SET db_name = NULL")
        connection.commit()
    finally:
        connection.close()
    task_db.unlink()

    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    task = next(item for item in second.sources if item.kind == "task")

    assert task.source_state == "detached"
    assert {event.event_id for event in before.events} <= {
        event.event_id for event in after.events
    }

    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    assert third.sources == second.sources


def test_task_database_replacement_retains_old_and_adds_namespaced_new_history(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    replacement = source / ".tg" / "replacement.db"
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    _task_database(replacement)
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("UPDATE session_meta SET db_name = 'replacement'")
        connection.commit()
    finally:
        connection.close()

    before = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", first.sources
    )
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    after = load_orc_team(
        snapshot, ROOT, "orc-test", "America/New_York", second.sources
    )
    tasks = {item.source_path: item for item in second.sources if item.kind == "task"}

    assert tasks[".tg/project.db"].source_state == "detached"
    assert tasks[".tg/project.db"].task_source_ordinal == 0
    assert tasks[".tg/replacement.db"].source_state == "live"
    assert tasks[".tg/replacement.db"].task_source_ordinal == 1
    assert {event.event_id for event in before.events} <= {
        event.event_id for event in after.events
    }
    assert any("-s1-" in event.event_id for event in after.events)


def test_duplicate_and_changed_previous_source_records_are_rejected(
    tmp_path: Path,
) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    with pytest.raises(OrcParseError, match="duplicate previous Orc source"):
        snapshot_orc_lineage(
            source, ROOT, snapshot, (*first.sources, first.sources[0]), "second"
        )
    session_source = next(item for item in first.sources if item.kind == "session")
    changed_kind = replace(session_source, kind="task")
    with pytest.raises(OrcParseError, match="source kind changed"):
        snapshot_orc_lineage(
            source,
            ROOT,
            snapshot,
            tuple(
                changed_kind if item == session_source else item
                for item in first.sources
            ),
            "second",
        )
    task_source = next(item for item in first.sources if item.kind == "task")
    changed_owner = replace(task_source, owner_session_id="unrelated-session")
    with pytest.raises(OrcParseError, match="source ownership changed"):
        snapshot_orc_lineage(
            source,
            ROOT,
            snapshot,
            tuple(
                changed_owner if item == task_source else item for item in first.sources
            ),
            "second",
        )


def test_session_prefix_collision_is_rejected_before_normalization(
    tmp_path: Path,
) -> None:
    source, _, _ = _fixture(tmp_path)
    collision = "11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    _session_database(
        source / ".orc" / "sessions" / collision / "session.db",
        collision,
        parent_id=ROOT,
        db_name=None,
        messages=[],
        blocks=[],
    )
    _append_index_session(source / ".orc" / "index.db", collision, ROOT)
    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")

    with pytest.raises(OrcParseError, match="session-id prefix collision"):
        load_orc_team(snapshot, ROOT, "orc-test", "America/New_York", copied.sources)


def test_duplicate_agent_block_identity_is_rejected(tmp_path: Path) -> None:
    source, root_db, _ = _fixture(tmp_path)
    messages = _non_agent_rewrite_messages("duplicate follows")
    messages.append(
        {
            "id": 31,
            "role": "System",
            "created_at_ms": _ms("2026-07-21T18:00:00+00:00"),
            "blocks": [{"type": "AgentBlock", "id": 10, "agent_id": "other"}],
        }
    )
    _rewrite_conversation(root_db, messages)

    with pytest.raises(OrcParseError, match="duplicate AgentBlock identity"):
        snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")


def _downgrade_orc_manifest_to_v1(path: Path) -> tuple[OrcSourceCopy, ...]:
    root = as_object(read_json(path), str(path))
    # `path` is `<store>/<slug>/raw/source-manifest.json`, and `<store>` is either the archive's
    # own `teams/` or the sibling build store named after it -- so the archive is no longer
    # simply four parents up. Both layouts, because a fixture may establish either.
    store_root = path.parent.parent.parent
    archive = (
        store_root.parent / store_root.name.removesuffix(DEFAULT_STORE_SUFFIX)
        if store_root.name.endswith(DEFAULT_STORE_SUFFIX)
        else store_root.parent
    )
    snapshots = snapshot_root(archive, path.parent.parent.name)
    legacy_sources: list[JsonValue] = []
    object_paths: set[Path] = set()
    for index, raw_source in enumerate(
        as_array(root.get("sources"), f"{path}: sources")
    ):
        source = as_object(raw_source, f"{path}: sources[{index}]")
        auxiliary = as_object(
            source.get("auxiliary"), f"{path}: sources[{index}].auxiliary"
        )
        legacy_source = dict(source)
        del legacy_source["auxiliary"]
        del legacy_source["task_projection"]
        del legacy_source["semantic_identity_mode"]
        del legacy_source["semantic_sha256"]
        del legacy_source["semantic_complete_bytes"]
        del legacy_source["semantic_baseline_path"]
        del legacy_source["source_state"]
        del legacy_source["task_source_ordinal"]
        legacy_source["auxiliary_count"] = as_int(
            auxiliary.get("message_count"),
            f"{path}: sources[{index}].auxiliary.message_count",
        )
        message_sha256 = auxiliary.get("message_sha256")
        if not isinstance(message_sha256, str):
            raise AssertionError("test fixture auxiliary digest is not a string")
        legacy_source["auxiliary_prefix_sha256"] = message_sha256
        source_path = legacy_source.get("source_path")
        snapshot_path = legacy_source.get("snapshot_path")
        if not isinstance(source_path, str) or not isinstance(snapshot_path, str):
            raise AssertionError("test fixture source paths are not strings")
        object_path = snapshots.joinpath(*Path(snapshot_path).parts)
        legacy_path = snapshots.joinpath(*Path(source_path).parts)
        legacy_path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(object_path, legacy_path)
        if legacy_source.get("kind") == "task":
            legacy_state = orc_module._logical_state(
                legacy_path, "task", legacy_task_fields=True
            )
            legacy_source["append_count"] = legacy_state.append_count
            legacy_source["append_max_id"] = legacy_state.append_max_id
            legacy_source["append_prefix_sha256"] = (
                legacy_state.append_prefix_sha256
            )
        object_paths.add(object_path)
        legacy_source["snapshot_path"] = source_path
        legacy_sources.append(legacy_source)
    legacy = dict(root)
    legacy["schema_version"] = 1
    legacy["sources"] = legacy_sources
    assert write_json_if_changed(path, legacy)
    for object_path in object_paths:
        object_path.unlink()
    return tuple(
        OrcSourceCopy.from_json_obj(
            as_object(item, f"{path}: sources[{index}]"),
            f"{path}: sources[{index}]",
            1,
        )
        for index, item in enumerate(legacy_sources)
    )


def test_pipeline_migrates_v1_manifest_across_auxiliary_rewrite_idempotently(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    before, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    _downgrade_orc_manifest_to_v1(manifest_path)
    _append_root_message(root_db, "root-appended", "Authoritative appended result")
    _rewrite_conversation(root_db, _rewritten_messages())

    after, migrated = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    manifest_sources = as_array(manifest.get("sources"), f"{manifest_path}: sources")
    root_source = next(
        as_object(item, f"{manifest_path}: source")
        for item in manifest_sources
        if as_object(item, f"{manifest_path}: source").get("kind") == "session"
    )
    auxiliary = as_object(
        root_source.get("auxiliary"), f"{manifest_path}: source.auxiliary"
    )

    assert manifest.get("schema_version") == 2
    assert auxiliary.get("policy") == "stable-spawn-subset-v1"
    assert auxiliary.get("rewrite_count") == 1
    assert auxiliary.get("degraded") is True
    assert {event.event_id for event in before.events} < {
        event.event_id for event in after.events
    }
    assert {agent.thread_id for agent in before.agents} < {
        agent.thread_id for agent in after.agents
    }
    assert migrated.files_changed > 0

    migrated_bytes = manifest_path.read_bytes()
    same, repeated = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    assert same == after
    assert repeated.files_changed == 0
    assert manifest_path.read_bytes() == migrated_bytes


def test_v1_unchanged_migration_preserves_semantics_digest_and_summary_cache(
    tmp_path: Path,
) -> None:
    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    initial, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    legacy_sources = _downgrade_orc_manifest_to_v1(manifest_path)
    snapshots = snapshot_root(archive, "orc-test")
    legacy_team = load_orc_team(
        snapshots, ROOT, "orc-test", "America/New_York", legacy_sources
    )
    before = replace(initial, sources=legacy_team.sources)
    raw_team_path = team_build_root(archive, "orc-test") / "raw" / "team.json"
    assert write_json_if_changed(raw_team_path, narrow_json(before.to_json_obj()))
    (team_build_root(archive, "orc-test") / "raw" / "artifacts.json").unlink()
    before_digest = _legacy_source_digest(before)
    assert source_digest(before) == before_digest
    before_paths = tuple(item.path for item in before.sources)
    summarize_archive(archive, "orc-test", "heuristic", "fixture")
    connection = sqlite3.connect(task_db)
    try:
        connection.execute(
            "UPDATE tasks SET owner = 'reassigned', title = 'Rewritten mutable title' "
            "WHERE local_id = 'task-a'"
        )
        connection.commit()
    finally:
        connection.close()

    after, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    cached = summarize_archive(archive, "orc-test", "heuristic", "fixture")
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    manifest_sources = [
        as_object(item, f"{manifest_path}: source")
        for item in as_array(manifest.get("sources"), f"{manifest_path}: sources")
    ]

    assert _semantic_team(after) == _semantic_team(before)
    assert tuple(item.path for item in after.sources) == before_paths
    assert all(path.startswith((".orc/", ".tg/")) for path in before_paths)
    assert source_digest(after) == before_digest
    assert manifest.get("schema_version") == 2
    assert all(
        item.get("source_path") != item.get("snapshot_path")
        for item in manifest_sources
    )
    task_source = next(item for item in manifest_sources if item.get("kind") == "task")
    task_projection = as_object(
        task_source.get("task_projection"), f"{manifest_path}: task_projection"
    )
    assert task_projection.get("rewrite_count") == 1
    assert cached.cache_misses == 0
    assert cached.cache_hits > 0


def test_non_agent_conversation_rewrite_preserves_semantics_and_summary_cache(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    before, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    summarize_archive(archive, "orc-test", "heuristic", "fixture")
    _rewrite_conversation(root_db, _non_agent_rewrite_messages("replacement"))
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "UPDATE session_meta SET updated_at = '2026-07-23T04:00:00+00:00'"
        )
        connection.commit()
    finally:
        connection.close()

    after, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    cached = summarize_archive(archive, "orc-test", "heuristic", "fixture")

    assert _semantic_team(after) == _semantic_team(before)
    assert source_digest(after) == source_digest(before)
    assert tuple(item.path for item in after.sources) == tuple(
        item.path for item in before.sources
    )
    assert cached.cache_misses == 0
    assert cached.cache_hits > 0


def test_partial_object_publication_failure_keeps_manifest_and_retries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source, root_db, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    prior_manifest = manifest_path.read_bytes()
    prior_root = _manifest_snapshot_database(archive, "session")
    prior_root_bytes = prior_root.read_bytes()
    _append_root_message(root_db, "root-appended", "publish candidate")
    _append_task_note(task_db, "publish second candidate")
    original_publish = orc_module._publish_snapshot_candidate
    new_targets = 0

    def fail_second_new_object(
        temporary: Path, target: Path, sha256: str, snapshot_root: Path
    ) -> bool:
        nonlocal new_targets
        if not target.exists():
            new_targets += 1
            if new_targets == 2:
                raise OSError("injected publication failure")
        return original_publish(temporary, target, sha256, snapshot_root)

    monkeypatch.setattr(
        orc_module, "_publish_snapshot_candidate", fail_second_new_object
    )
    with pytest.raises(OSError, match="injected publication failure"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert manifest_path.read_bytes() == prior_manifest
    assert prior_root.read_bytes() == prior_root_bytes
    assert len(_managed_snapshot_objects(archive)) > 3

    monkeypatch.setattr(orc_module, "_publish_snapshot_candidate", original_publish)
    after, retried = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    assert any(event.event_id == "orc-block-root-appended" for event in after.events)
    assert retried.files_changed > 0
    assert not prior_root.exists()
    assert len(_managed_snapshot_objects(archive)) == len(after.sources)


def test_task_projection_publication_failure_keeps_manifest_and_retries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    prior_manifest = manifest_path.read_bytes()
    prior_projection, _ = _manifest_task_projection(archive)
    prior_projection_bytes = prior_projection.read_bytes()
    _append_task_note(task_db, "projection publication candidate")
    original_publish = orc_module._publish_snapshot_candidate

    def fail_projection(
        temporary: Path, target: Path, sha256: str, snapshot_root: Path
    ) -> bool:
        if target.suffix == ".json" and not target.exists():
            raise OSError("injected projection publication failure")
        return original_publish(temporary, target, sha256, snapshot_root)

    monkeypatch.setattr(orc_module, "_publish_snapshot_candidate", fail_projection)
    with pytest.raises(OSError, match="injected projection publication failure"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert manifest_path.read_bytes() == prior_manifest
    assert prior_projection.read_bytes() == prior_projection_bytes

    monkeypatch.setattr(orc_module, "_publish_snapshot_candidate", original_publish)
    after, retried = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    current_projection, _ = _manifest_task_projection(archive)

    assert any(
        event.event_id == "orc-note-11111111-5" for event in after.events
    )
    assert retried.files_changed > 0
    assert current_projection != prior_projection
    assert not prior_projection.exists()
    assert _managed_task_projections(archive) == (current_projection,)


def test_task_projection_hash_corruption_fails_closed(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    projection, _ = _manifest_task_projection(archive)
    projection.write_text("{}\n", encoding="utf-8")

    with pytest.raises(OrcParseError, match="task projection hash mismatch"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_task_projection_symlink_fails_closed(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    projection, _ = _manifest_task_projection(archive)
    outside = tmp_path / "outside.json"
    outside.write_bytes(projection.read_bytes())
    projection.unlink()
    projection.symlink_to(outside)

    with pytest.raises(OrcParseError, match="task projection is missing or unsafe"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


@pytest.mark.parametrize("component", (".orc", ".tg"))
def test_live_source_intermediate_symlink_fails_closed(
    tmp_path: Path, component: str
) -> None:
    source, _, _ = _fixture(tmp_path)
    original = source / component
    relocated = tmp_path / f"relocated-{component[1:]}"
    original.replace(relocated)
    original.symlink_to(relocated, target_is_directory=True)

    with pytest.raises(OrcParseError, match="symlink in Orc live source path"):
        snapshot_orc_lineage(source, ROOT, tmp_path / "snapshot", (), "first")


@pytest.mark.parametrize("artifact_kind", ("object", "projection"))
def test_snapshot_intermediate_symlink_fails_closed(
    tmp_path: Path, artifact_kind: str
) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    if artifact_kind == "object":
        source_copy = next(item for item in copied.sources if item.kind == "session")
        artifact = _snapshot_database(snapshot, source_copy)
    else:
        source_copy = next(item for item in copied.sources if item.kind == "task")
        assert source_copy.task_projection is not None
        artifact = snapshot.joinpath(
            *Path(source_copy.task_projection.path).parts
        )
    prefix = artifact.parent
    relocated = tmp_path / f"relocated-{artifact_kind}-prefix"
    prefix.replace(relocated)
    prefix.symlink_to(relocated, target_is_directory=True)

    with pytest.raises(OrcParseError, match="snapshot path component"):
        load_orc_team(
            snapshot, ROOT, "orc-test", "America/New_York", copied.sources
        )


def test_noncanonical_task_projection_fails_closed(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    root = as_object(read_json(manifest_path), str(manifest_path))
    task = next(
        as_object(item, f"{manifest_path}: source")
        for item in as_array(root.get("sources"), f"{manifest_path}: sources")
        if as_object(item, f"{manifest_path}: source").get("kind") == "task"
    )
    projection = as_object(
        task.get("task_projection"), f"{manifest_path}: task_projection"
    )
    prior_path, _ = _manifest_task_projection(archive)
    value = as_object(json.loads(prior_path.read_text(encoding="utf-8")), "projection")
    records = as_array(value.get("records"), "projection.records")
    records.append(records[-1])
    text = json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
    sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()
    relative = f".projections/{sha256[:2]}/{sha256}.json"
    replacement = (
        snapshot_root(archive, "orc-test") / relative
    )
    replacement.parent.mkdir(parents=True, exist_ok=True)
    replacement.write_text(text, encoding="utf-8")
    projection["path"] = relative
    projection["sha256"] = sha256
    projection["note_count"] = len(records)
    assert write_json_if_changed(manifest_path, root)

    with pytest.raises(OrcParseError, match="note IDs are not strictly ordered"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_directory_fsync_failure_leaves_reusable_orphan(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    prior_manifest = manifest_path.read_bytes()
    _append_root_message(root_db, "root-appended", "fsync candidate")
    original_fsync_directory = orc_module._fsync_directory
    failed = False

    def fail_object_directory(path: Path) -> None:
        nonlocal failed
        if not failed and path.parent.name == ".objects":
            failed = True
            raise OSError("injected object-directory fsync failure")
        original_fsync_directory(path)

    monkeypatch.setattr(orc_module, "_fsync_directory", fail_object_directory)
    with pytest.raises(OSError, match="injected object-directory fsync failure"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert manifest_path.read_bytes() == prior_manifest
    assert len(_managed_snapshot_objects(archive)) > 3

    monkeypatch.setattr(orc_module, "_fsync_directory", original_fsync_directory)
    after, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    assert any(event.event_id == "orc-block-root-appended" for event in after.events)
    assert len(_managed_snapshot_objects(archive)) == len(after.sources)


def test_preexisting_content_address_must_match_its_hash(tmp_path: Path) -> None:
    source, root_db, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    first_root = next(item for item in first.sources if item.kind == "session")
    first_root_path = _snapshot_database(snapshot, first_root)
    first_root_bytes = first_root_path.read_bytes()
    _append_root_message(root_db, "root-appended", "new immutable object")
    second = snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    second_root = next(item for item in second.sources if item.kind == "session")
    second_root_path = _snapshot_database(snapshot, second_root)
    second_root_path.write_bytes(b"corrupt preexisting object")

    with pytest.raises(OrcParseError, match="preexisting snapshot object has the wrong hash"):
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "retry")
    assert first_root_path.read_bytes() == first_root_bytes


def test_managed_object_gc_rejects_symlinks(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    prefix = snapshot_root(archive, "orc-test") / ".objects" / "ff"
    prefix.mkdir()
    unsafe = prefix / ("f" * 64 + ".db")
    unsafe.symlink_to(tmp_path / "outside.db")

    with pytest.raises(OrcParseError, match="symlink in Orc snapshot object store"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_managed_object_gc_removes_legacy_sqlite_sidecars(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    snapshot = tmp_path / "snapshot"
    copied = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    database = _snapshot_database(snapshot, copied.sources[0])
    wal = database.with_name(database.name + "-wal")
    shm = database.with_name(database.name + "-shm")
    wal.write_bytes(b"legacy read sidecar")
    shm.write_bytes(b"legacy read sidecar")

    removed = orc_module.prune_orc_snapshot_objects(snapshot, copied.sources)

    assert removed == 2
    assert not wal.exists()
    assert not shm.exists()


def test_post_snapshot_parse_failure_keeps_old_manifest_and_raw_team(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    raw_team_path = team_build_root(archive, "orc-test") / "raw" / "team.json"
    prior_manifest = manifest_path.read_bytes()
    prior_raw_team = raw_team_path.read_bytes()
    prior_root = _manifest_snapshot_database(archive, "session")
    prior_root_bytes = prior_root.read_bytes()
    _append_root_message(root_db, "malformed", "candidate parse must fail")
    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "UPDATE content_blocks SET role = ? WHERE id = 'malformed'",
            (sqlite3.Binary(b"invalid-role"),),
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError, match="role: expected a non-empty string"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert manifest_path.read_bytes() == prior_manifest
    assert raw_team_path.read_bytes() == prior_raw_team
    assert prior_root.read_bytes() == prior_root_bytes

    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "UPDATE content_blocks SET role = 'assistant' WHERE id = 'malformed'"
        )
        connection.commit()
    finally:
        connection.close()
    after, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    assert any(event.event_id == "orc-block-malformed" for event in after.events)
    assert not prior_root.exists()
    assert len(_managed_snapshot_objects(archive)) == len(after.sources)


def test_malformed_v2_projection_is_rejected_before_snapshot(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    root = as_object(read_json(manifest_path), str(manifest_path))
    sources = as_array(root.get("sources"), f"{manifest_path}: sources")
    task = next(
        as_object(item, f"{manifest_path}: source")
        for item in sources
        if as_object(item, f"{manifest_path}: source").get("kind") == "task"
    )
    task["task_projection"] = None
    assert write_json_if_changed(manifest_path, root)

    with pytest.raises(OrcParseError, match="invalid schema-v2 projections"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_v2_manifest_rejects_unknown_top_level_field(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    manifest["unexpected"] = True
    assert write_json_if_changed(manifest_path, manifest)

    with pytest.raises(OrcParseError, match="unknown=.*unexpected"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_v2_manifest_rejects_missing_nested_field(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    session = next(
        as_object(item, "manifest.source")
        for item in as_array(manifest.get("sources"), "manifest.sources")
        if as_object(item, "manifest.source").get("kind") == "session"
    )
    auxiliary = as_object(session.get("auxiliary"), "manifest.source.auxiliary")
    del auxiliary["message_sha256"]
    assert write_json_if_changed(manifest_path, manifest)

    with pytest.raises(OrcParseError, match="missing message_sha256"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_v2_manifest_rejects_invalid_observed_enrichment_digest(
    tmp_path: Path,
) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    task = next(
        as_object(item, "manifest.source")
        for item in as_array(manifest.get("sources"), "manifest.sources")
        if as_object(item, "manifest.source").get("kind") == "task"
    )
    projection = as_object(
        task.get("task_projection"), "manifest.task.task_projection"
    )
    projection["observed_enrichment_sha256"] = "f" * 63 + "z"
    assert write_json_if_changed(manifest_path, manifest)

    with pytest.raises(OrcParseError, match="expected a SHA-256 digest"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_v1_migration_failure_preserves_snapshot_and_manifest(tmp_path: Path) -> None:
    source, root_db, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    _downgrade_orc_manifest_to_v1(manifest_path)
    snapshot_db = _manifest_snapshot_database(archive, "session")
    prior_manifest = manifest_path.read_bytes()
    prior_snapshot = snapshot_db.read_bytes()
    _append_root_message(root_db, "root-appended", "Must not be ingested")
    _rewrite_conversation(root_db, _rewritten_messages(first_agent="renamed-worker"))

    with pytest.raises(OrcParseError, match="stable spawn evidence was rewritten"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert manifest_path.read_bytes() == prior_manifest
    assert snapshot_db.read_bytes() == prior_snapshot


def test_unknown_orc_manifest_schema_is_rejected_before_snapshot(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    manifest_path = (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    )
    root = as_object(read_json(manifest_path), str(manifest_path))
    snapshot_db = _manifest_snapshot_database(archive, "session")
    root["schema_version"] = 99
    assert write_json_if_changed(manifest_path, root)
    prior_snapshot = snapshot_db.read_bytes()

    with pytest.raises(OrcParseError, match="invalid Orc source manifest"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert snapshot_db.read_bytes() == prior_snapshot


@pytest.mark.parametrize(
    "failure_name", ("source-manifest.json", "artifacts.json", "team.json")
)
def test_normalized_generation_marker_fails_closed_and_recovers(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    failure_name: str,
) -> None:
    source, root_db, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    _append_root_message(root_db, "generation-two", "new committed generation")
    original_write = pipeline_module._write_json_durable

    def fail_after_write(path: Path, value: JsonValue) -> bool:
        changed = original_write(path, value)
        if path.name == failure_name:
            raise OSError(f"injected failure after {failure_name}")
        return changed

    monkeypatch.setattr(pipeline_module, "_write_json_durable", fail_after_write)
    with pytest.raises(OSError, match="injected failure"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    with pytest.raises(ValueError, match="normalized generation"):
        load_archived_team(archive, "orc-test")

    monkeypatch.setattr(
        pipeline_module, "_write_json_durable", original_write
    )
    repaired, _ = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    assert load_archived_team(archive, "orc-test") == repaired


def test_stale_managed_staging_candidate_is_pruned(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    staging = snapshot_root(archive, "orc-test") / ".staging"
    stale = staging / "orc-123-0123456789abcdef.db"
    stale_wal = staging / "orc-123-0123456789abcdef.db-wal"
    stale_shm = staging / "orc-123-0123456789abcdef.db-shm"
    unmanaged = staging / "unmanaged.db-wal"
    stale.write_bytes(b"interrupted candidate")
    stale_wal.write_bytes(b"interrupted WAL")
    stale_shm.write_bytes(b"interrupted shared memory")
    unmanaged.write_bytes(b"do not prune")

    _, report = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )

    assert not stale.exists()
    assert not stale_wal.exists()
    assert not stale_shm.exists()
    assert unmanaged.read_bytes() == b"do not prune"
    assert report.files_changed == 3


@pytest.mark.parametrize("unsafe_kind", ("symlink", "directory"))
def test_unsafe_staging_entry_is_rejected(
    tmp_path: Path, unsafe_kind: str
) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    staging = snapshot_root(archive, "orc-test") / ".staging"
    unsafe = staging / "unsafe"
    if unsafe_kind == "symlink":
        unsafe.symlink_to(tmp_path / "outside")
    else:
        unsafe.mkdir()

    with pytest.raises(OrcParseError, match="unsafe entry in Orc staging"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_orc_pipeline_builds_one_day_archive_idempotently(tmp_path: Path) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    window = parse_date_window("2026-07-21", "2026-07-22", "America/New_York")
    assert window is not None
    assert window.start_ms is not None
    assert window.end_ms is not None

    team, first = ingest_orc(
        archive,
        source,
        ROOT,
        "orc-test",
        "America/New_York",
        window,
    )
    _, second = ingest_orc(
        archive,
        source,
        ROOT,
        "orc-test",
        "America/New_York",
        window,
    )
    summaries = summarize_archive(archive, "orc-test", "heuristic", "fixture")
    built = build_archive(archive, "orc-test")
    timeline = json.loads(schema_1_timeline_text(archive))

    assert team.provider == "orc"
    assert first.sources == 3
    assert second.files_changed == 0
    assert summaries.cache_misses > 0
    assert built["agents"] == 4
    assert timeline["range"] == {
        "start_ms": window.start_ms,
        "end_ms": window.end_ms,
    }
    assert len(timeline["rollups"]) == 4
    message_edges = [edge for edge in timeline["edges"] if edge["kind"] == "message"]
    assert len(message_edges) == 3
    assert {
        (edge["source_id"], edge["target_id"])
        for edge in message_edges
    } == {
        (event.thread_id, ROOT)
        for event in team.events
        if event.kind == "inter_agent_message"
        and window.start_ms <= event.timestamp_ms < window.end_ms
    }
    assert [event["kind"] for event in timeline["events"]].count(
        "inter_agent_message"
    ) == 3
    manifest = json.loads(
        (team_build_root(archive, "orc-test") / "raw" / "source-manifest.json").read_text(
            encoding="utf-8"
        )
    )
    assert manifest["provider"] == "orc"
    assert manifest["schema_version"] == 2
    assert manifest["date_window"]["end_date"] == "2026-07-22"


def test_orc_pipeline_allows_narrowing_but_rejects_widening_ingest_window(
    tmp_path: Path,
) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    window = parse_date_window(
        "2026-07-21", "2026-07-22", "America/New_York"
    )
    assert window is not None

    narrowed, _ = ingest_orc(
        archive,
        source,
        ROOT,
        "orc-test",
        "America/New_York",
        window,
    )

    assert narrowed.window_start_ms == window.start_ms
    assert narrowed.window_end_ms == window.end_ms
    manifest = json.loads(
        (
            team_build_root(archive, "orc-test")
            / "raw"
            / "source-manifest.json"
        ).read_text(encoding="utf-8")
    )
    assert manifest["date_window"]["start_ms"] == window.start_ms
    assert manifest["date_window"]["end_ms"] == window.end_ms

    with pytest.raises(OrcParseError, match="only stay unchanged or become narrower"):
        ingest_orc(
            archive,
            source,
            ROOT,
            "orc-test",
            "America/New_York",
        )


# --- operator override for an in-place append-prefix rewrite -----------------------------------
#
# The incident these cover: an upstream backfill wrote "token_count" into one already-captured
# `messages` row about nine minutes after capture. The append-prefix guard is byte-exact over 18
# content_blocks columns and 6 messages columns, so it refused the whole lineage -- correctly, on
# the evidence it had, and uselessly, because it could not say which row or column moved.


def _override_prefix(parent_id: str | None) -> str:
    return "ov" if parent_id is None else "nested-ov"


def _override_block(prefix: str, session_id: str, index: int) -> tuple[object, ...]:
    """Return one `content_blocks` row of an override fixture session."""

    return (
        f"{prefix}-block-{index}",
        f"{prefix}-message-{index}",
        session_id,
        0,
        _ms("2026-08-15T10:00:00+00:00") + index * 1000,
        1,
        "user" if index % 2 == 0 else "assistant",
        "text",
        f"Recorded line {index}",
        None,
        None,
        None,
        None,
        None if index % 2 == 0 else "model",
        None,
        None,
        None,
    )


def _override_message(
    prefix: str, session_id: str, row_id: int, offset: int
) -> tuple[int, str, int, dict[str, object]]:
    """Return one `messages` row of an override fixture session."""

    created_at_ms = _ms("2026-08-15T10:00:00+00:00") + offset
    return (
        row_id,
        session_id,
        created_at_ms,
        {
            "id": f"{prefix}-native-{row_id}",
            "role": "User" if row_id == 1 else "Assistant",
            "source": None,
            "created_at_ms": created_at_ms,
            "blocks": [{"type": "AgentBlock", "id": 90 + row_id, "agent_id": "worker"}],
            "token_count": None,
        },
    )


def _override_session(
    path: Path,
    session_id: str,
    *,
    parent_id: str | None,
    db_name: str | None,
    content_blocks: int,
) -> None:
    """Write one modern Orc session (dual content_blocks/messages storage) at *path*.

    Message ids deliberately skip 3 and 4. That gap is not decoration: it is the only way to test
    a row *appearing* inside an already-recorded prefix, because a straight insert past the
    watermark is caught earlier by the row-count guard instead.

    The rows come from :func:`_override_block` and :func:`_override_message` rather than being
    written inline, because :func:`_receive_live_traffic` has to append rows this fixture would
    have produced itself had capture happened a minute later. Two generators would eventually
    diverge in some column nobody is looking at, and the append-prefix digest covers every column.
    """

    prefix = _override_prefix(parent_id)
    _session_database(
        path,
        session_id,
        parent_id=parent_id,
        db_name=db_name,
        messages=[],
        blocks=[
            _override_block(prefix, session_id, index) for index in range(content_blocks)
        ],
        created_at="2026-08-15T09:00:00+00:00",
        updated_at="2026-08-15T11:00:00+00:00",
    )
    _add_messages(
        path,
        tuple(
            _override_message(prefix, session_id, row_id, offset)
            for row_id, offset in ((1, 0), (2, 1000), (5, 5000))
        ),
    )


def _receive_live_traffic(
    path: Path,
    session_id: str,
    *,
    block_indices: Sequence[int],
    messages: Sequence[tuple[int, int]],
    updated_at: str,
) -> None:
    """Append rows to a session that is still running, the way Orc itself would.

    This is the half of the incident every other override fixture leaves out. The observed
    backfill landed on the *watermark row* of a session that was still receiving messages, about
    nine minutes after capture -- so by the time the operator re-ran the ingest, the database had
    both the rewritten row and rows that simply had not existed before. A fixture frozen between
    the two runs makes the re-baselined digest and the append count come out unchanged, which is
    an artifact of the fixture and not a property of the feature.

    `session_meta.updated_at` moves forward with the rows because a real session's does, and the
    meta-extension guard reads it: leaving it behind would test appending against a session that
    claims not to have changed.
    """

    prefix = _override_prefix(_session_parent_id(path))
    connection = sqlite3.connect(path)
    try:
        connection.executemany(
            "INSERT INTO content_blocks VALUES "
            "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            tuple(_override_block(prefix, session_id, index) for index in block_indices),
        )
        connection.executemany(
            "INSERT INTO messages(id, session_id, role, created_at_ms, message_json, "
            "search_text) VALUES (?, ?, ?, ?, ?, NULL)",
            tuple(
                (
                    row_id,
                    row_session_id,
                    str(message["role"]).lower(),
                    timestamp_ms,
                    json.dumps(message, separators=(",", ":")),
                )
                for row_id, row_session_id, timestamp_ms, message in (
                    _override_message(prefix, session_id, row_id, offset)
                    for row_id, offset in messages
                )
            ),
        )
        connection.execute(
            "UPDATE session_meta SET updated_at = ? WHERE id = ?",
            (updated_at, session_id),
        )
        connection.commit()
    finally:
        connection.close()


def _session_parent_id(path: Path) -> str | None:
    connection = sqlite3.connect(path)
    try:
        row = connection.execute("SELECT parent_id FROM session_meta").fetchone()
    finally:
        connection.close()
    parent_id = row[0]
    return None if parent_id is None else str(parent_id)


def _override_fixture(
    tmp_path: Path, *, content_blocks: int = 2
) -> tuple[Path, Path, Path]:
    """Build a single-session override source plus its task database."""

    source = tmp_path / "override-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    _override_session(
        root_db, ROOT, parent_id=None, db_name="project", content_blocks=content_blocks
    )
    _task_database(source / ".tg" / "project.db")
    _index_database(source / ".orc" / "index.db", ((ROOT, None),))
    return source, root_db, tmp_path / "override-snapshot"


def _nested_override_fixture(tmp_path: Path) -> tuple[Path, Path, Path, Path]:
    """Build the two-session lineage the override's scope is actually about.

    A single-session fixture cannot see the failure this exists to pin, because with one session
    "authorize the run" and "authorize the session" are the same statement. The shape is the one
    the rest of this suite already uses for a lineage -- a root coordinator plus one nested session
    reached through the index -- so the guard runs twice over sources that fail independently.
    """

    source = tmp_path / "nested-override-source"
    root_db = source / ".orc" / "sessions" / ROOT / "session.db"
    nested_db = source / ".orc" / "sessions" / NESTED / "session.db"
    _override_session(
        root_db, ROOT, parent_id=None, db_name="project", content_blocks=2
    )
    _override_session(
        nested_db, NESTED, parent_id=ROOT, db_name=None, content_blocks=2
    )
    _task_database(source / ".tg" / "project.db")
    _index_database(
        source / ".orc" / "index.db", ((ROOT, None), (NESTED, ROOT))
    )
    return source, root_db, nested_db, tmp_path / "nested-override-snapshot"


def _backfill_token_count(path: Path, message_id: int, token_count: int) -> None:
    """Reproduce the observed upstream edit: one JSON field, in place, after capture."""

    connection = sqlite3.connect(path)
    try:
        raw = connection.execute(
            "SELECT message_json FROM messages WHERE id = ?", (message_id,)
        ).fetchone()[0]
        message = json.loads(str(raw))
        message["token_count"] = token_count
        connection.execute(
            "UPDATE messages SET message_json = ? WHERE id = ?",
            (json.dumps(message, separators=(",", ":")), message_id),
        )
        connection.commit()
    finally:
        connection.close()


def _override_snapshot_objects(snapshot_root: Path) -> tuple[str, ...]:
    root = snapshot_root / ".objects"
    if not root.is_dir():
        return ()
    return tuple(sorted(path.name for path in root.glob("[0-9a-f][0-9a-f]/*.db")))


def _session_source(sources: Sequence[OrcSourceCopy]) -> OrcSourceCopy:
    return next(item for item in sources if item.kind == "session")


def _task_source(sources: Sequence[OrcSourceCopy]) -> OrcSourceCopy:
    return next(item for item in sources if item.kind == "task")


def test_prefix_scopes_reproduce_the_guard_digest(tmp_path: Path) -> None:
    """Pin the diff and the digest to one definition of "the prefix"."""

    source, _, snapshot = _override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    session = _session_source(first.sources)
    database = _snapshot_database(snapshot, session)

    connection = orc_module._read_only(database)
    try:
        geometry = orc_module._session_geometry(connection, database, None)
        scopes, legacy = orc_module._session_prefix_scopes(geometry, None, None)
        digests = [
            orc_module._query_digest(connection, scope.query, (scope.limit,))
            for scope in scopes
        ]
    finally:
        connection.close()

    assert not legacy
    assert [scope.table for scope in scopes] == ["content_blocks", "messages"]
    combined = hashlib.sha256()
    for value in (
        "content_blocks",
        digests[0][0],
        scopes[0].limit,
        digests[0][1],
        "messages",
        digests[1][0],
        scopes[1].limit,
        digests[1][1],
    ):
        orc_module._update_digest(combined, value)
    assert combined.hexdigest() == session.append_prefix_sha256
    assert digests[0][0] + digests[1][0] == session.append_count


def test_append_prefix_rewrite_is_refused_by_default_and_names_row_column_and_field(
    tmp_path: Path,
) -> None:
    source, root_db, snapshot = _override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before_objects = _override_snapshot_objects(snapshot)
    _backfill_token_count(root_db, 2, 445)

    with pytest.raises(OrcParseError) as raised:
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")

    message = str(raised.value)
    assert "Orc session existing append prefix was rewritten for" in message
    assert "1 row(s) changed" in message
    assert "messages row 2" in message
    assert "message_json .token_count" in message
    assert '"token_count":null' in message
    assert '"token_count":445' in message
    # The recommended command is complete: the flag with the session id it authorizes, so the
    # operator cannot accidentally re-baseline anything else by following the instruction.
    assert f"--accept-orc-prefix-rewrite {ROOT} " in message
    assert "authorizes this one session and no other" in message
    # The refusal is where the operator decides, so the refusal -- not the user guide -- has to
    # state what accepting costs: which object stops being the baseline, how long it is kept, and
    # that the diff kept in exchange is a bounded summary, with the two numbers that bound it.
    session = _session_source(first.sources)
    assert f"supersedes the pre-rewrite snapshot {session.snapshot_path}" in message
    assert "until the next accepted override on this source, then reclaimed" in message
    assert "at most 20 of those 1 changed row(s)" in message
    assert "at most 160 characters per column value" in message
    # Refusing is still the default, and refusing still publishes nothing.
    assert _override_snapshot_objects(snapshot) == before_objects


def _prefix_digest_at(
    snapshot_root: Path, source: OrcSourceCopy, watermark: int | None
) -> str:
    """Re-digest a published session snapshot at *watermark*, or in full when it is ``None``.

    Two different digests share the name "append prefix sha256" and the difference only becomes
    visible on a live session, which is exactly why it is worth naming here. The manifest's
    `append_prefix_sha256` covers everything the source held at capture; an override's
    `observed_append_prefix_sha256` covers only the rows at or below the *previous* watermark,
    because that is the span the guard compared and the span the operator was shown a diff of.
    """

    return orc_module._logical_state(
        _snapshot_database(snapshot_root, source),
        "session",
        prefix_max_id=watermark,
    ).append_prefix_sha256


def _recorded_lines(team: TeamData) -> set[str]:
    """Return just the session transcript lines of an override fixture team.

    The fixture's task database contributes events too, and they are irrelevant to whether an
    appended `content_blocks` row was ingested -- comparing whole event sets would drag them into
    an assertion about session traffic and make the failure unreadable.
    """

    return {
        event.text
        for event in team.events
        if event.text is not None and event.text.startswith("Recorded line ")
    }


def _unpack_session_watermark(append_max_id: int) -> tuple[int, int]:
    """Return the (content rowid, message id) watermarks packed into a modern append_max_id."""

    assert append_max_id & orc_module._SESSION_STATE_TAG
    packed = append_max_id ^ orc_module._SESSION_STATE_TAG
    return (
        packed >> orc_module._SESSION_STATE_SHIFT,
        packed & orc_module._SESSION_STATE_MASK,
    )


def test_accepted_prefix_rewrite_on_a_live_session_rebaselines_and_keeps_every_record(
    tmp_path: Path,
) -> None:
    """The production shape: the rewrite lands on a session that is still receiving messages.

    This is what actually happened. Orc backfilled `token_count` from null to 445 on the *watermark
    row* about nine minutes after capture, and in those nine minutes the session went on running,
    so the re-ingest saw a rewritten row and four rows that had not existed at capture. The frozen
    sibling below holds the database still between the two runs, and holding it still quietly makes
    three unrelated things come out equal -- the re-baselined digest equals the digest the override
    recorded, the append count does not move, and neither does the watermark. All three are
    artifacts of the fixture. On a live session all three legitimately differ, for reasons that
    have nothing to do with the rewrite, so a suite that only ever pinned the equalities was
    pinning the fixture rather than the feature.

    What has to be true instead is stated below in the terms that survive traffic: no record is
    lost, the rows that arrived are ingested, the rewrite is recorded against the span it was
    diagnosed on, and the re-baseline describes the database as it now is -- which the next
    unflagged run is the real proof of, since a re-baseline that described the frozen past would
    refuse immediately.
    """

    source, root_db, snapshot = _override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(snapshot, ROOT, "override", "UTC", first.sources)
    session_before = _session_source(first.sources)
    assert _unpack_session_watermark(session_before.append_max_id) == (2, 5)

    # Nine minutes pass. One already-captured row is backfilled in place, and the session keeps
    # doing what a live session does: two more content blocks, two more messages, past the
    # watermark. Both edits are present in the same re-ingest, because that is how the operator
    # met them.
    _backfill_token_count(root_db, 2, 445)
    _receive_live_traffic(
        root_db,
        ROOT,
        block_indices=(2, 3),
        messages=((6, 6000), (7, 7000)),
        updated_at="2026-08-15T11:20:00+00:00",
    )

    second = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        first.sources,
        "second",
        accept_prefix_rewrite=(ROOT,),
    )

    # The override still describes the one row that was rewritten, and only it. Appended rows are
    # not "changes" -- they are past the watermark, outside the span the guard digests at all --
    # so traffic must not inflate the evidence an operator is asked to judge.
    assert len(second.prefix_overrides) == 1
    override = second.prefix_overrides[0]
    assert override.changed_row_count == 1
    assert override.changed_rows_bounded is False
    row = override.changed_rows[0]
    assert (row.table, row.row_id) == ("messages", 2)
    assert [column.column for column in row.columns] == ["message_json"]
    assert '"token_count":null' in row.columns[0].previous
    assert '"token_count":445' in row.columns[0].observed

    session_after = _session_source(second.sources)
    # The two digests the override pairs are both taken at the *old* watermark, one on each side.
    # That pairing is what makes the record checkable against the snapshot it superseded, and it
    # is unaffected by anything that arrived afterwards.
    assert override.previous_append_prefix_sha256 == session_before.append_prefix_sha256
    assert override.previous_append_prefix_sha256 == _prefix_digest_at(
        snapshot, session_before, None
    )
    assert override.observed_append_prefix_sha256 == _prefix_digest_at(
        snapshot, session_after, session_before.append_max_id
    )
    assert override.observed_append_prefix_sha256 != session_before.append_prefix_sha256
    assert override.superseded_snapshot_path == session_before.snapshot_path
    assert override.superseded_sha256 == session_before.sha256

    # The re-baseline, by contrast, is taken over everything the source now holds -- so on a live
    # session it is a third distinct digest, equal to neither side of the override. The frozen
    # fixture's `session_after.append_prefix_sha256 == override.observed_append_prefix_sha256` is
    # true there only because "the old watermark" and "everything" name the same rows when nothing
    # arrived; asserting it as a general property would forbid the source from having grown.
    assert session_after.append_prefix_sha256 == _prefix_digest_at(
        snapshot, session_after, None
    )
    assert session_after.append_prefix_sha256 != override.observed_append_prefix_sha256
    assert session_after.append_prefix_sha256 != override.previous_append_prefix_sha256

    # Likewise the count and the watermark: they move by exactly the traffic, and by nothing else.
    # An override that silently dropped the appended rows would leave both frozen, which is what
    # the frozen fixture cannot distinguish from correct behaviour.
    assert session_after.append_count == session_before.append_count + 4
    assert _unpack_session_watermark(session_after.append_max_id) == (4, 7)
    assert session_after.owner_session_id == session_before.owner_session_id
    assert session_after.append_prefix_override == override

    after = load_orc_team(snapshot, ROOT, "override", "UTC", second.sources)
    # No record is lost: everything ingested before the rewrite is still there, byte for byte. The
    # backfilled column is metadata the timeline does not project, so not one event is disturbed
    # by it -- which is the entire reason this rewrite was acceptable to accept.
    before_events = {event.event_id: event for event in before.events}
    after_events = {event.event_id: event for event in after.events}
    assert before_events.keys() <= after_events.keys()
    assert all(after_events[key] == event for key, event in before_events.items())
    # ...and the rows that arrived after capture were ingested rather than being cut off at the
    # re-baselined watermark.
    assert _recorded_lines(before) == {"Recorded line 0", "Recorded line 1"}
    assert _recorded_lines(after) == {
        "Recorded line 0",
        "Recorded line 1",
        "Recorded line 2",
        "Recorded line 3",
    }

    # The proof that the re-baseline describes the database as it now is: the next run needs no
    # flag. A digest re-baselined to the pre-traffic prefix would refuse here instead.
    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    assert third.prefix_overrides == ()
    # And the record stays sticky across that clean run -- the archive says it was re-baselined
    # once, forever, even though the run that observed it did nothing unusual.
    assert _session_source(third.sources).append_prefix_override == override

    # Traffic keeps arriving after the accepted run, and is still ordinary appending.
    _receive_live_traffic(
        root_db,
        ROOT,
        block_indices=(4,),
        messages=((8, 8000),),
        updated_at="2026-08-15T11:40:00+00:00",
    )
    fourth = snapshot_orc_lineage(source, ROOT, snapshot, third.sources, "fourth")
    assert fourth.prefix_overrides == ()
    session_fourth = _session_source(fourth.sources)
    assert session_fourth.append_count == session_after.append_count + 2
    assert _unpack_session_watermark(session_fourth.append_max_id) == (5, 8)
    assert session_fourth.append_prefix_override == override


def test_accepted_prefix_rewrite_rebaselines_and_discards_no_records(
    tmp_path: Path,
) -> None:
    """The degenerate shape: nothing arrives between capture and re-ingest.

    Kept because it isolates what the rewrite alone does -- with no traffic in the way, the
    semantic cache key, the task projection and the whole normalized team must come out identical,
    and any difference is attributable to the override and nothing else. It is the *weaker* test of
    the two, though, and the equalities it can state are read in the light of
    :func:`test_accepted_prefix_rewrite_on_a_live_session_rebaselines_and_keeps_every_record`,
    which is the shape the incident actually had.
    """

    source, root_db, snapshot = _override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before = load_orc_team(snapshot, ROOT, "override", "UTC", first.sources)
    before_task = _task_source(first.sources)
    _backfill_token_count(root_db, 2, 445)

    second = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        first.sources,
        "second",
        accept_prefix_rewrite=(ROOT,),
    )

    assert len(second.prefix_overrides) == 1
    override = second.prefix_overrides[0]
    assert override.degraded is True
    assert (
        override.degradation_reason
        == "append-prefix-rewritten-operator-accepted-rows-preserved"
    )
    assert override.policy == "operator-accepted-prefix-rewrite-v1"
    assert override.override_count == 1
    assert override.source_path == _session_source(first.sources).source_path
    assert override.changed_row_count == 1
    assert override.changed_rows_bounded is False
    row = override.changed_rows[0]
    assert (row.table, row.row_id) == ("messages", 2)
    assert [column.column for column in row.columns] == ["message_json"]
    assert row.columns[0].json_paths == ("token_count",)
    assert row.columns[0].json_paths_bounded is False
    assert '"token_count":null' in row.columns[0].previous
    assert '"token_count":445' in row.columns[0].observed

    session_before = _session_source(first.sources)
    session_after = _session_source(second.sources)
    # Re-baselined, and only re-baselined: the digest moves to what was actually observed and
    # nothing else about the record is rewritten or dropped.
    assert override.previous_append_prefix_sha256 == session_before.append_prefix_sha256
    assert override.observed_append_prefix_sha256 != session_before.append_prefix_sha256
    # Stated the way it stays true once rows start arriving. The override's observed digest covers
    # the span at or below the *previous* watermark -- the span the guard compared -- while the
    # manifest's covers everything the source now holds. Here those are the same rows, so the
    # tempting `session_after.append_prefix_sha256 == override.observed_append_prefix_sha256`
    # passes; it passes only because this fixture is frozen, and it would forbid the source from
    # having grown. The live-session test above pins the general form.
    assert session_after.append_prefix_sha256 == _prefix_digest_at(
        snapshot, session_after, None
    )
    assert override.observed_append_prefix_sha256 == _prefix_digest_at(
        snapshot, session_after, session_before.append_max_id
    )
    # No traffic, so -- and only so -- the count and the watermark are also unmoved. These two are
    # a statement about this fixture, not about accepting a rewrite.
    assert session_after.append_count == session_before.append_count
    assert session_after.append_max_id == session_before.append_max_id
    assert session_after.owner_session_id == session_before.owner_session_id
    assert session_after.append_prefix_override == override
    # Both degradations are recorded, separately, because both really happened: rewriting
    # message_json also moves Orc's conversation projection, and one `degradation_reason` string
    # could not have held the two events at once.
    assert session_after.auxiliary.degraded is True
    assert (
        session_after.auxiliary.degradation_reason
        == "conversation-history-rewritten-stable-spawns-preserved"
    )
    assert (
        session_after.auxiliary.degradation_reason
        != override.degradation_reason
    )
    # The paid-summary cache key survives, because nothing normalized changed.
    assert session_after.semantic_sha256 == session_before.semantic_sha256
    # The unrelated task source keeps its .projections pointer untouched.
    after_task = _task_source(second.sources)
    assert after_task.task_projection == before_task.task_projection
    assert (
        snapshot / Path(str(before_task.task_projection and before_task.task_projection.path))
    ).is_file()

    after = load_orc_team(snapshot, ROOT, "override", "UTC", second.sources)
    assert _semantic_team(after) == _semantic_team(before)

    # Having re-baselined, the next run needs no override at all.
    third = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "third")
    assert third.prefix_overrides == ()


def test_accepting_one_session_does_not_authorize_another_in_the_same_lineage(
    tmp_path: Path,
) -> None:
    """The scope defect, reproduced: one acceptance must not re-baseline a whole session tree.

    Before this the flag was a single boolean threaded to every discovered source, so one
    invocation re-baselined every rewritten session in the lineage at once -- the same
    blanket-switch failure the team-level flag was already designed against, one level down. What
    made it invisible is reproduced here too: the guard raises on the first mismatching source, so
    the operator was only ever shown the *root* session's diff before passing the flag. The nested
    session was authorized having never been printed.
    """

    source, root_db, nested_db, snapshot = _nested_override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before_objects = _override_snapshot_objects(snapshot)
    _backfill_token_count(root_db, 2, 445)
    _backfill_token_count(nested_db, 2, 445)

    # What the operator actually sees with no flag: the root session, and only the root session.
    with pytest.raises(OrcParseError) as unflagged:
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "second")
    assert f"sessions/{ROOT}/session.db" in str(unflagged.value)
    assert NESTED not in str(unflagged.value)

    # Acting on exactly that: the refusal named the root session, so the root session is what gets
    # authorized -- and the nested session, which nobody has looked at, still refuses with its own
    # evidence rather than being swept along.
    with pytest.raises(OrcParseError) as raised:
        snapshot_orc_lineage(
            source,
            ROOT,
            snapshot,
            first.sources,
            "second",
            accept_prefix_rewrite=(ROOT,),
        )
    message = str(raised.value)
    assert f"sessions/{NESTED}/session.db" in message
    assert "messages row 2" in message
    assert f"--accept-orc-prefix-rewrite {NESTED} " in message
    # Refused means refused for the whole lineage: the run that would have quietly re-baselined
    # both publishes nothing at all.
    assert _override_snapshot_objects(snapshot) == before_objects

    accepted = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        first.sources,
        "third",
        accept_prefix_rewrite=(ROOT, NESTED),
    )

    assert sorted(
        override.source_path for override in accepted.prefix_overrides
    ) == [
        f".orc/sessions/{ROOT}/session.db",
        f".orc/sessions/{NESTED}/session.db",
    ]


def test_authorizing_one_session_leaves_an_unrewritten_sibling_unmarked(
    tmp_path: Path,
) -> None:
    """An acceptance is a statement about one session, so no other source may acquire the mark."""

    source, root_db, _, snapshot = _nested_override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    _backfill_token_count(root_db, 2, 445)

    second = snapshot_orc_lineage(
        source,
        ROOT,
        snapshot,
        first.sources,
        "second",
        accept_prefix_rewrite=(ROOT, NESTED),
    )

    # Both were authorized; only the one that was actually rewritten is recorded as degraded. An
    # authorization is permission, never an assertion that something happened.
    assert [override.source_path for override in second.prefix_overrides] == [
        f".orc/sessions/{ROOT}/session.db"
    ]
    nested = next(
        item
        for item in second.sources
        if item.owner_session_id == NESTED and item.kind == "session"
    )
    assert nested.append_prefix_override is None


def test_an_accepted_session_id_outside_the_lineage_is_refused_up_front(
    tmp_path: Path,
) -> None:
    """A safety override that silently authorizes nothing is the worst outcome available to it."""

    source, root_db, _, snapshot = _nested_override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    before_objects = _override_snapshot_objects(snapshot)
    _backfill_token_count(root_db, 2, 445)

    stranger = "33333333-3333-3333-3333-333333333333"
    with pytest.raises(OrcParseError) as raised:
        snapshot_orc_lineage(
            source,
            ROOT,
            snapshot,
            first.sources,
            "second",
            accept_prefix_rewrite=(ROOT, stranger),
        )
    assert "names sessions outside this lineage" in str(raised.value)
    assert stranger in str(raised.value)
    # Raised from the plan, before any database was copied, so a typo costs nothing and -- more to
    # the point -- cannot half-apply by re-baselining the sessions that did match.
    assert _override_snapshot_objects(snapshot) == before_objects

    with pytest.raises(OrcParseError, match="duplicate session ids"):
        snapshot_orc_lineage(
            source,
            ROOT,
            snapshot,
            first.sources,
            "second",
            accept_prefix_rewrite=(ROOT, ROOT),
        )


def test_accepted_prefix_rewrite_retains_the_pre_rewrite_snapshot(
    tmp_path: Path,
) -> None:
    """The reviewer's scenario, end to end: accepting must not GC the bytes it stopped trusting.

    Before this, the accepting run pruned the previous object in the same call that re-baselined
    the digest, so an operator who read the refusal, judged it benign, and was wrong had no route
    back -- only a 20-row, 160-character-per-column summary. This drives the real `ingest_orc`
    entry point rather than `snapshot_orc_lineage`, because the GC that destroyed the object runs
    in the pipeline, after the manifest commits, and never ran in the unit-level fixture at all.

    The session keeps receiving messages across the rewrite, matching the incident: retention has
    to hold while the source is growing, which is the only state it is ever exercised in. A frozen
    source would also hide the manifest round trip that matters here -- the recorded override is
    decoded on the next run against a `append_count` and watermark that have since moved.
    """

    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    snapshots = snapshot_root(archive, "orc-test")
    manifest_path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    pre_rewrite = _manifest_snapshot_database(archive, "session")
    pre_rewrite_sha256 = orc_module._sha256_file(pre_rewrite)
    _backfill_token_count(root_db, 2, 445)
    _receive_live_traffic(
        root_db,
        ROOT,
        block_indices=(2,),
        messages=((6, 6000),),
        updated_at="2026-08-15T11:20:00+00:00",
    )

    _, report = ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,)
    )

    override = report.orc_prefix_overrides[0]
    assert override.superseded_sha256 == pre_rewrite_sha256
    assert (
        override.superseded_snapshot_path
        == pre_rewrite.relative_to(snapshots).as_posix()
    )
    # The object GC ran in this same call and left the superseded bytes alone.
    assert pre_rewrite.is_file()
    assert orc_module._sha256_file(pre_rewrite) == pre_rewrite_sha256
    current = _manifest_snapshot_database(archive, "session")
    assert current != pre_rewrite
    assert current.is_file()

    # Not merely present: still the pre-rewrite content, so the accepted diff is checkable at full
    # fidelity against the object the archive now points at.
    def _token_count(path: Path) -> object:
        connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        try:
            raw = connection.execute(
                "SELECT message_json FROM messages WHERE id = 2"
            ).fetchone()[0]
        finally:
            connection.close()
        return json.loads(str(raw))["token_count"]

    assert _token_count(pre_rewrite) is None
    assert _token_count(current) == 445

    stored = as_object(
        as_object(
            next(
                item
                for item in as_array(
                    as_object(
                        read_json(manifest_path), str(manifest_path)
                    ).get("sources"),
                    "sources",
                )
                if as_object(item, "source").get("kind") == "session"
            ),
            "source",
        ).get("append_prefix_override"),
        "override",
    )
    assert stored.get("superseded_sha256") == pre_rewrite_sha256

    # Sticky retention: a later ordinary ingest still names the object, so routine runs cannot
    # quietly reclaim what the override run deliberately kept -- including the routine ingests that
    # keep happening because the session is still producing rows, which is what "later" means for a
    # live session and which publish a new current object each time.
    _receive_live_traffic(
        root_db,
        ROOT,
        block_indices=(3,),
        messages=((7, 7000),),
        updated_at="2026-08-15T11:40:00+00:00",
    )
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    assert pre_rewrite.is_file()
    assert orc_module._sha256_file(pre_rewrite) == pre_rewrite_sha256
    assert _manifest_snapshot_database(archive, "session") not in (pre_rewrite, current)


def test_a_schema_v1_source_records_where_its_pre_rewrite_bytes_actually_are(
    tmp_path: Path,
) -> None:
    """A v1 snapshot lives outside the object store, so the pointer is a note, not a GC anchor.

    The retention pointer is normally a content-addressed object name, and that shape is enforced
    on read so a manifest cannot aim object retention somewhere else. A schema-v1 source is stored
    at its own mirrored source path instead, which GC never scans; recording that path honestly is
    the only way the override stays decodable *and* still tells a human where the bytes are.
    """

    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    manifest_path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    _downgrade_orc_manifest_to_v1(manifest_path)
    snapshots = snapshot_root(archive, "orc-test")
    mirrored = snapshots / ".orc" / "sessions" / ROOT / "session.db"
    assert mirrored.is_file()
    _backfill_token_count(root_db, 2, 445)

    _, report = ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,)
    )

    override = report.orc_prefix_overrides[0]
    assert override.superseded_snapshot_path == f".orc/sessions/{ROOT}/session.db"
    assert not override.superseded_snapshot_path.startswith(".objects/")
    assert mirrored.is_file()
    assert orc_module._sha256_file(mirrored) == override.superseded_sha256

    # The whole point of tolerating the shape: the migrated schema-2 manifest still decodes, so a
    # v1 archive that took an override is not quietly bricked on its next ordinary run.
    _, again = ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    assert again.orc_prefix_overrides == ()


def test_a_second_accepted_override_reclaims_the_first_retained_snapshot(
    tmp_path: Path,
) -> None:
    """Retention is one deep and only another explicit acceptance collects it."""

    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    first_pre_rewrite = _manifest_snapshot_database(archive, "session")
    _backfill_token_count(root_db, 2, 445)
    ingest_orc(archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,))
    second_pre_rewrite = _manifest_snapshot_database(archive, "session")
    assert first_pre_rewrite.is_file()

    _backfill_token_count(root_db, 1, 17)
    _, report = ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,)
    )

    override = report.orc_prefix_overrides[0]
    assert override.override_count == 2
    assert (
        override.superseded_sha256 == orc_module._sha256_file(second_pre_rewrite)
    )
    assert second_pre_rewrite.is_file()
    # The first override's copy is gone, reclaimed by a second deliberate acceptance rather than
    # by any routine run -- which is the documented retention policy, not an accident.
    assert not first_pre_rewrite.exists()


def test_a_hand_deleted_retained_snapshot_does_not_break_a_later_ingest(
    tmp_path: Path,
) -> None:
    """Retention is evidence, not an input, so losing it must not fail an unrelated run."""

    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    pre_rewrite = _manifest_snapshot_database(archive, "session")
    _backfill_token_count(root_db, 2, 445)
    ingest_orc(archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,))

    pre_rewrite.unlink()

    _, report = ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    assert report.orc_prefix_overrides == ()
    # The fact of the override survives its evidence: the manifest still records that this source
    # was re-baselined, which is the part a later reader must not be able to lose.
    manifest_path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    session = next(
        as_object(item, "source")
        for item in as_array(manifest.get("sources"), "sources")
        if as_object(item, "source").get("kind") == "session"
    )
    stored = as_object(session.get("append_prefix_override"), "override")
    assert stored.get("override_count") == 1
    assert not pre_rewrite.exists()


def test_accepted_prefix_rewrite_is_sticky_and_counts_repeat_events(
    tmp_path: Path,
) -> None:
    source, root_db, snapshot = _override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")
    _backfill_token_count(root_db, 2, 445)
    second = snapshot_orc_lineage(
        source, ROOT, snapshot, first.sources, "second", accept_prefix_rewrite=(ROOT,)
    )

    clean = snapshot_orc_lineage(source, ROOT, snapshot, second.sources, "clean")

    # No new event, but the archive still says it happened.
    assert clean.prefix_overrides == ()
    carried = _session_source(clean.sources).append_prefix_override
    assert carried is not None
    assert carried.override_count == 1
    assert carried.accepted_at == "second"

    _backfill_token_count(root_db, 1, 17)
    fourth = snapshot_orc_lineage(
        source, ROOT, snapshot, clean.sources, "fourth", accept_prefix_rewrite=(ROOT,)
    )

    assert len(fourth.prefix_overrides) == 1
    assert fourth.prefix_overrides[0].override_count == 2
    assert fourth.prefix_overrides[0].accepted_at == "fourth"
    assert fourth.prefix_overrides[0].changed_rows[0].row_id == 1


def test_accepted_prefix_rewrite_never_covers_rows_appearing_or_disappearing(
    tmp_path: Path,
) -> None:
    source, root_db, snapshot = _override_fixture(tmp_path)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")

    # One content block deleted and one message inserted into the id gap: the totals cancel, so
    # the row-count guard is satisfied and only the digest notices.
    connection = sqlite3.connect(root_db)
    try:
        connection.execute("DELETE FROM content_blocks WHERE id = 'ov-block-0'")
        connection.execute(
            "INSERT INTO messages(id, session_id, role, created_at_ms, message_json, "
            "search_text) VALUES (3, ?, 'assistant', ?, ?, NULL)",
            (
                ROOT,
                _ms("2026-08-15T10:00:03+00:00"),
                json.dumps(
                    {
                        "id": "ov-native-3",
                        "role": "Assistant",
                        "source": None,
                        "created_at_ms": _ms("2026-08-15T10:00:03+00:00"),
                        "blocks": [
                            {"type": "AgentBlock", "id": 93, "agent_id": "worker"}
                        ],
                        "token_count": None,
                    },
                    separators=(",", ":"),
                ),
            ),
        )
        connection.commit()
    finally:
        connection.close()

    for accept in ((), (ROOT,)):
        with pytest.raises(OrcParseError) as raised:
            snapshot_orc_lineage(
                source,
                ROOT,
                snapshot,
                first.sources,
                "second",
                accept_prefix_rewrite=accept,
            )
        message = str(raised.value)
        assert "lost 1 row(s) (content_blocks row 1)" in message
        assert "gained 1 row(s) (messages row 3)" in message
        assert "never rows appearing or disappearing" in message


def test_accepted_prefix_rewrite_bounds_a_large_diff(tmp_path: Path) -> None:
    source, root_db, snapshot = _override_fixture(tmp_path, content_blocks=30)
    first = snapshot_orc_lineage(source, ROOT, snapshot, (), "first")

    connection = sqlite3.connect(root_db)
    try:
        connection.execute(
            "UPDATE content_blocks SET searchable_text = ? WHERE rowid <= 25",
            ("x" * 5000,),
        )
        connection.commit()
    finally:
        connection.close()

    with pytest.raises(OrcParseError) as raised:
        snapshot_orc_lineage(source, ROOT, snapshot, first.sources, "refused")

    # The refusal is bounded more tightly than the record: 20 rows of excerpted columns belong in a
    # receipt, not in one exception string thrown at a terminal.
    refusal = str(raised.value)
    assert "25 row(s) changed" in refusal
    assert "further changed row(s) not shown" in refusal
    assert len(refusal) < 6000

    second = snapshot_orc_lineage(
        source, ROOT, snapshot, first.sources, "second", accept_prefix_rewrite=(ROOT,)
    )

    override = second.prefix_overrides[0]
    assert override.changed_row_count == 25
    assert len(override.changed_rows) == 20
    assert override.changed_rows_bounded is True
    column = override.changed_rows[0].columns[0]
    assert column.column == "searchable_text"
    assert column.bounded is True
    assert len(column.observed) < 300
    lines = override.describe()
    assert "5 further changed row(s) not shown" in lines[-2]
    # The caps are a summary, not the record: the report's last word is where the unabridged
    # pre-rewrite bytes are, so the 5 unnamed rows remain recoverable by hand.
    assert lines[-1] == (
        "pre-rewrite snapshot retained for comparison: "
        f"{override.superseded_snapshot_path} (reclaimed by the next accepted override "
        "on this source)"
    )
    assert (snapshot / override.superseded_snapshot_path).is_file()


def test_accepted_prefix_rewrite_round_trips_through_the_source_manifest(
    tmp_path: Path,
) -> None:
    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    _, clean_report = ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    assert clean_report.orc_prefix_overrides == ()
    _backfill_token_count(root_db, 2, 445)

    with pytest.raises(OrcParseError, match="existing append prefix was rewritten"):
        ingest_orc(archive, source, ROOT, "orc-test", "UTC")

    _, report = ingest_orc(
        archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,)
    )

    assert len(report.orc_prefix_overrides) == 1
    recorded = as_array(
        as_object(report.to_json_obj(), "report").get("orc_prefix_overrides"),
        "report.orc_prefix_overrides",
    )
    assert len(recorded) == 1
    manifest_path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    session = next(
        as_object(item, "source")
        for item in as_array(manifest.get("sources"), "sources")
        if as_object(item, "source").get("kind") == "session"
    )
    stored = as_object(session.get("append_prefix_override"), "override")
    assert stored.get("degraded") is True
    assert (
        stored.get("degradation_reason")
        == "append-prefix-rewritten-operator-accepted-rows-preserved"
    )
    assert stored.get("changed_row_count") == 1
    task = next(
        as_object(item, "source")
        for item in as_array(manifest.get("sources"), "sources")
        if as_object(item, "source").get("kind") == "task"
    )
    assert task.get("append_prefix_override") is None

    # The manifest decodes on the next run, which is the only proof that matters for a sticky
    # record: an override that could not be read back would fail every future ingest.
    _, again = ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    assert again.orc_prefix_overrides == ()
    reread = as_object(read_json(manifest_path), str(manifest_path))
    reread_session = next(
        as_object(item, "source")
        for item in as_array(reread.get("sources"), "sources")
        if as_object(item, "source").get("kind") == "session"
    )
    assert reread_session.get("append_prefix_override") == stored


@pytest.mark.parametrize(
    ("mutation", "match"),
    (
        ({"degraded": False}, "must record its degradation"),
        (
            {"degradation_reason": "conversation-history-rewritten-stable-spawns-preserved"},
            "must record its degradation",
        ),
        ({"override_count": 0}, "must have happened at least once"),
        ({"policy": "something-else"}, "unsupported policy"),
        ({"changed_row_count": 9}, "disagrees with its own row count"),
        ({"source_path": "elsewhere.db"}, "belongs to"),
        # A retention pointer that is not the managed content-addressed name of the digest beside
        # it would aim object GC at an arbitrary path, so the two are checked against each other
        # rather than trusted.
        (
            {"superseded_snapshot_path": ".objects/aa/aa.db"},
            "expected content-addressed path",
        ),
        ({"superseded_sha256": "0" * 64}, "expected content-addressed path"),
    ),
)
def test_forged_prefix_override_records_are_rejected(
    tmp_path: Path, mutation: dict[str, JsonValue], match: str
) -> None:
    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    _backfill_token_count(root_db, 2, 445)
    ingest_orc(archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,))
    manifest_path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    sources = as_array(manifest.get("sources"), "sources")
    for index, item in enumerate(sources):
        entry = as_object(item, "source")
        if entry.get("kind") != "session":
            continue
        override = as_object(entry.get("append_prefix_override"), "override")
        override.update(mutation)
        entry["append_prefix_override"] = override
        sources[index] = entry
    manifest["sources"] = sources
    write_json_if_changed(manifest_path, narrow_json(manifest))

    with pytest.raises(OrcParseError, match=match):
        ingest_orc(archive, source, ROOT, "orc-test", "UTC")


def test_a_retention_pointer_aimed_at_the_current_snapshot_is_rejected(
    tmp_path: Path,
) -> None:
    """Retention that names the post-rewrite object reads as satisfied and holds nothing."""

    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")
    _backfill_token_count(root_db, 2, 445)
    ingest_orc(archive, source, ROOT, "orc-test", "UTC", accept_prefix_rewrite=(ROOT,))
    manifest_path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    sources = as_array(manifest.get("sources"), "sources")
    for index, item in enumerate(sources):
        entry = as_object(item, "source")
        if entry.get("kind") != "session":
            continue
        override = as_object(entry.get("append_prefix_override"), "override")
        override["superseded_snapshot_path"] = entry.get("snapshot_path")
        override["superseded_sha256"] = entry.get("sha256")
        entry["append_prefix_override"] = override
        sources[index] = entry
    manifest["sources"] = sources
    write_json_if_changed(manifest_path, narrow_json(manifest))

    with pytest.raises(
        OrcParseError, match="pre-rewrite snapshot cannot be the current snapshot"
    ):
        ingest_orc(archive, source, ROOT, "orc-test", "UTC")


def test_orc_ingest_cli_refuses_then_records_an_accepted_prefix_rewrite(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    source, root_db, _ = _override_fixture(tmp_path)
    archive = tmp_path / "cli-archive"
    command = [
        "ingest-orc",
        "--output",
        str(archive),
        "--team",
        "orc-test",
        "--source-root",
        str(source),
        "--root-session",
        ROOT,
        "--timezone",
        "UTC",
    ]
    assert timeline_main(command) == 0
    capsys.readouterr()
    _backfill_token_count(root_db, 2, 445)

    assert timeline_main(command) == 2
    refused = capsys.readouterr()
    assert "existing append prefix was rewritten" in refused.err
    assert "messages row 2" in refused.err
    assert "--accept-orc-prefix-rewrite" in refused.err
    # The cost of the flag reaches the terminal the operator is standing at, not only the guide.
    assert "supersedes the pre-rewrite snapshot .objects/" in refused.err
    assert "at most 160 characters per column value" in refused.err

    # Copied out of the refusal verbatim: the message names the flag *and* the session, so the
    # operator's next command is what they were just shown rather than a reconstruction.
    assert f"--accept-orc-prefix-rewrite {ROOT}" in refused.err
    assert timeline_main([*command, "--accept-orc-prefix-rewrite", ROOT]) == 0
    accepted = capsys.readouterr()
    # Loud, on stderr, so it survives the operator redirecting stdout to a log file.
    assert "accepted append-prefix rewrite" in accepted.err
    assert "append-prefix-rewritten-operator-accepted-rows-preserved" in accepted.err
    assert "messages row 2: message_json .token_count" in accepted.err
    assert "pre-rewrite snapshot retained for comparison: .objects/" in accepted.err
    assert "accepted append-prefix rewrite" not in accepted.out

    runs = sorted((archive / "runs").glob("*.json"))
    latest = as_object(read_json(runs[-1]), str(runs[-1]))
    assert latest.get("status") == "completed"
    assert "--accept-orc-prefix-rewrite" in as_array(
        latest.get("command"), "command"
    )
    overrides = as_array(
        as_object(latest.get("ingest"), "ingest").get("orc_prefix_overrides"),
        "orc_prefix_overrides",
    )
    assert len(overrides) == 1
    recorded = as_object(overrides[0], "override")
    assert (
        recorded.get("degradation_reason")
        == "append-prefix-rewritten-operator-accepted-rows-preserved"
    )
    row = as_object(
        as_array(recorded.get("changed_rows"), "changed_rows")[0], "row"
    )
    assert row.get("table") == "messages"
    assert row.get("row_id") == 2
    column = as_object(as_array(row.get("columns"), "columns")[0], "column")
    assert column.get("column") == "message_json"
    assert as_array(column.get("json_paths"), "json_paths") == ["token_count"]


def test_orc_ingest_cli_authorizes_a_second_session_only_when_named(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """The whole operator loop on a real lineage: one refusal, one flag, then the next one.

    Two rewritten sessions cost two round trips, on purpose. Each refusal prints one session's
    evidence and the exact repeat of the flag that authorizes it, so the operator arrives at a
    two-session acceptance having read two diffs -- which is the property a single boolean, or a
    single flag that happened to cover the lineage, could not give.
    """

    source, root_db, nested_db, _ = _nested_override_fixture(tmp_path)
    archive = tmp_path / "nested-cli-archive"
    command = [
        "ingest-orc",
        "--output",
        str(archive),
        "--team",
        "orc-test",
        "--source-root",
        str(source),
        "--root-session",
        ROOT,
        "--timezone",
        "UTC",
    ]
    assert timeline_main(command) == 0
    capsys.readouterr()
    _backfill_token_count(root_db, 2, 445)
    _backfill_token_count(nested_db, 2, 445)

    assert timeline_main(command) == 2
    first = capsys.readouterr().err
    assert f"--accept-orc-prefix-rewrite {ROOT}" in first
    assert NESTED not in first

    # The obvious next command -- and it is still refused, because the nested session's rewrite
    # has not been shown to anyone yet.
    assert timeline_main([*command, "--accept-orc-prefix-rewrite", ROOT]) == 2
    second = capsys.readouterr().err
    assert f"sessions/{NESTED}/session.db" in second
    assert f"--accept-orc-prefix-rewrite {NESTED}" in second

    assert (
        timeline_main(
            [
                *command,
                "--accept-orc-prefix-rewrite",
                ROOT,
                "--accept-orc-prefix-rewrite",
                NESTED,
            ]
        )
        == 0
    )
    accepted = capsys.readouterr().err
    assert accepted.count("accepted append-prefix rewrite") == 2
    assert f"sessions/{ROOT}/session.db" in accepted
    assert f"sessions/{NESTED}/session.db" in accepted

    # A mistyped id is rejected up front rather than being silently inert, and the archive the
    # previous command left behind is still clean afterwards.
    assert (
        timeline_main(
            [*command, "--accept-orc-prefix-rewrite", "not-a-session-in-this-lineage"]
        )
        == 2
    )
    assert "names sessions outside this lineage" in capsys.readouterr().err


def _promoted_task_note_records(archive: Path) -> tuple[dict[str, JsonValue], ...]:
    path = team_build_root(archive, "orc-test") / "raw" / "task-notes.jsonl"
    return tuple(read_jsonl(path))


def test_task_notes_are_promoted_into_the_normalized_model(tmp_path: Path) -> None:
    """The notes become model records with provenance, stored once and not inside team.json."""

    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    team, report = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    _, projection = _manifest_task_projection(archive)
    notes_path = team_build_root(archive, "orc-test") / "raw" / "task-notes.jsonl"
    raw_team = as_object(
        read_json(team_build_root(archive, "orc-test") / "raw" / "team.json"),
        "team.json",
    )

    assert report.task_notes == 4
    assert report.newly_promoted_task_notes == 4
    assert [note.note_id for note in team.task_notes] == [1, 2, 3, 4]
    assert [note.content for note in team.task_notes] == [
        "First incarnation finding",
        "Second incarnation result",
        "",
        "Exactly at exclusive end",
    ]
    assert {note.source_path for note in team.task_notes} == {".tg/project.db"}
    assert {note.title for note in team.task_notes} == {"Audit the scheduler"}
    assert {note.task_owner for note in team.task_notes} == {"worker"}
    assert {note.upstream_present for note in team.task_notes} == {True}
    assert {note.projection_policy for note in team.task_notes} == {
        "frozen-note-history-v3"
    }
    assert {note.projection_sha256 for note in team.task_notes} == {
        projection.get("sha256")
    }
    # One storage location, not two: the bytes live in the record file, and `team.json` keeps only
    # the events they render into.
    assert len(_promoted_task_note_records(archive)) == 4
    assert "task_notes" not in raw_team
    assert notes_path.read_text(encoding="utf-8").count("\n") == 4


def test_task_note_deleted_upstream_survives_without_source_snapshots(
    tmp_path: Path,
) -> None:
    """The point of the whole exercise: the archive outlives the row and the snapshot both.

    Note 4 is deleted from the live task table, the way Orc really does delete notes -- 74 of the
    4,583 in one real projection have no counterpart left upstream. Then `source_snapshots/` is
    removed outright, standing in for the relocation this promotion is a prerequisite for. What
    must remain readable afterwards is the note's exact text, from the version-controlled part of
    the archive alone.
    """

    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    first, _ = ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    first_generation = {
        note.note_id: note.projection_sha256 for note in first.task_notes
    }
    connection = sqlite3.connect(task_db)
    try:
        connection.execute("DELETE FROM task_notes WHERE id = 4")
        connection.execute(
            "INSERT INTO task_notes(task_id, content, created_at) VALUES "
            "('task-a', 'Written after the deletion', '2026-07-22T05:00:00+00:00')"
        )
        connection.commit()
    finally:
        connection.close()

    second, report = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )
    shutil.rmtree(snapshot_root(archive, "orc-test"))
    reloaded = load_archived_team(archive, "orc-test")
    survivor = next(note for note in reloaded.task_notes if note.note_id == 4)
    appended = next(note for note in reloaded.task_notes if note.note_id == 5)

    assert report.newly_promoted_task_notes == 1
    assert report.task_notes == 5
    assert [note.note_id for note in second.task_notes] == [1, 2, 3, 4, 5]
    assert survivor.content == "Exactly at exclusive end"
    assert survivor.created_at == "2026-07-22T04:00:00+00:00"
    assert appended.content == "Written after the deletion"
    # And the archive says, per note, that it is now the only copy of this one. The first ingest
    # saw note 4 alive and recorded it as such; the second saw it gone and latched the field over.
    # This is the fact `OrcTaskProjection.missing_note_count` counts and cannot name, and the only
    # thing here that stops being computable the moment the snapshots are deleted -- which the
    # `rmtree` two lines above has already done.
    assert survivor.upstream_present is False
    assert report.task_notes_upstream_deleted == 1
    assert {
        note.note_id: note.upstream_present
        for note in reloaded.task_notes
        if note.note_id != 4
    } == {1: True, 2: True, 3: True, 5: True}
    assert appended.upstream_present is True
    # Provenance is recorded once and never restamped: the four original notes still name the
    # generation they were promoted from, and only the new note names the current one.
    assert {
        note.note_id: note.projection_sha256
        for note in reloaded.task_notes
        if note.note_id <= 4
    } == first_generation
    assert appended.projection_sha256 not in set(first_generation.values())


def test_promoted_task_notes_are_not_reimported_on_a_repeat_ingest(
    tmp_path: Path,
) -> None:
    """An ingest that observes nothing new must promote nothing and rewrite no bytes."""

    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    notes_path = team_build_root(archive, "orc-test") / "raw" / "task-notes.jsonl"
    after_first = notes_path.read_bytes()

    _, repeated = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )

    assert repeated.newly_promoted_task_notes == 0
    assert repeated.task_notes == 4
    assert repeated.files_changed == 0
    assert notes_path.read_bytes() == after_first

    _append_task_note(task_db, "One more note")
    _, extended = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )

    assert extended.newly_promoted_task_notes == 1
    assert extended.task_notes == 5
    # Append-only in the literal sense: the previously promoted records are byte-identical, so a
    # version-controlled archive sees a one-line diff rather than a whole-file restamp.
    assert notes_path.read_bytes().startswith(after_first)


def test_promoted_task_note_disagreeing_about_its_core_is_refused(
    tmp_path: Path,
) -> None:
    """Two copies of one note that differ in the fields Orc never edits is a refusal.

    Reachable only when the promoted file and the frozen projection have been made to disagree --
    the snapshot layer refuses a rewritten note before this ever sees it. That is exactly why the
    check belongs here too: this file is the copy nothing else can reconstruct, so the one place
    that must never quietly choose a winner is the merge that carries it forward.
    """

    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    notes_path = team_build_root(archive, "orc-test") / "raw" / "task-notes.jsonl"
    promoted = pipeline_module._load_promoted_task_notes(archive, "orc-test")
    tampered = tuple(
        replace(note, content="not what was promoted") if note.note_id == 2 else note
        for note in promoted
    )
    notes_path.write_text(
        canonical_jsonl(
            as_object(narrow_json(note.to_json_obj()), "note") for note in tampered
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="disagrees with the current source"):
        ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")


def test_promoted_task_notes_out_of_order_are_refused(tmp_path: Path) -> None:
    """A duplicated or misordered key would let one record hide another; refuse instead."""

    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    notes_path = team_build_root(archive, "orc-test") / "raw" / "task-notes.jsonl"
    promoted = pipeline_module._load_promoted_task_notes(archive, "orc-test")
    notes_path.write_text(
        canonical_jsonl(
            as_object(narrow_json(note.to_json_obj()), "note")
            for note in reversed(promoted)
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="not strictly ordered"):
        load_archived_team(archive, "orc-test")


def test_orc_ingest_stores_tool_payloads_outside_the_generation_marker(tmp_path: Path) -> None:
    """The payload tree is provider-neutral, and the Orc marker deliberately does not bind it.

    The marker exists to catch a *stale* generation, and a payload cannot be stale: its name is
    its content and the store is a union that never rewrites a record. What it can be is
    incomplete, which is the state the tree is designed to permit -- gitignored bulk an operator
    may prune, permission or move. Binding it here would turn every one of those into a refusal on
    the next build. This pins that: the tree can lose a whole shard and the archive still loads.
    """

    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    _, report = ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    payload_root = team_build_root(archive, "orc-test") / "payloads"

    assert report.tool_payloads > 0
    assert report.newly_stored_tool_payloads == report.tool_payloads
    assert sorted(payload_root.glob("*.jsonl"))
    assert verify_payload_store(payload_root) == ()

    marker_path = team_build_root(archive, "orc-test") / "raw" / "normalized-generation.json"
    marker = as_object(read_json(marker_path), str(marker_path))
    assert "payloads_sha256" not in marker

    shutil.rmtree(payload_root)
    load_archived_team(archive, "orc-test")


def test_a_tampered_payload_shard_is_caught_by_verification(tmp_path: Path) -> None:
    """Nothing checks shard bytes on the read path, so this is where that check lives."""

    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    payload_root = team_build_root(archive, "orc-test") / "payloads"
    manifest = load_payload_manifest(payload_root)
    assert manifest is not None
    shard = sorted(payload_root.glob("*.jsonl"))[0]
    shard.write_text("", encoding="utf-8")

    # Still loads, because the model and its digests are intact and the tree is prunable by
    # design -- at load time an emptied shard is indistinguishable from a pruned one.
    load_archived_team(archive, "orc-test")

    problems = verify_payload_store(payload_root)
    assert any("do not match the digest recorded for them" in problem for problem in problems)


def test_pre_promotion_generation_marker_still_builds(tmp_path: Path) -> None:
    """An archive ingested before promotion existed keeps loading until its next ingest."""

    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    marker_path = (
        team_build_root(archive, "orc-test") / "raw" / "normalized-generation.json"
    )
    marker = as_object(read_json(marker_path), str(marker_path))
    del marker["task_notes_sha256"]
    (team_build_root(archive, "orc-test") / "raw" / "task-notes.jsonl").unlink()
    write_json_if_changed(marker_path, narrow_json(marker))

    team = load_archived_team(archive, "orc-test")

    assert team.task_notes == ()

    restored, report = ingest_orc(
        archive, source, ROOT, "orc-test", "America/New_York"
    )

    assert report.newly_promoted_task_notes == 4
    assert len(restored.task_notes) == 4
    assert "task_notes_sha256" in as_object(
        read_json(marker_path), str(marker_path)
    )


def test_a_task_note_holding_a_unicode_line_separator_stays_readable(
    tmp_path: Path,
) -> None:
    """`raw/task-notes.jsonl` is the last copy of 1,386 notes; one character must not break it.

    U+2028, U+2029 and U+0085 are line terminators to `str.splitlines` and ordinary characters to
    JSON, which writes them raw because `canonical_jsonl` uses `ensure_ascii=False`. A reader that
    split on them turned one record into two fragments, and since `load_archived_team` reads this
    file on every load, one such note made the team permanently unloadable *and* un-reingestable
    -- poisoned by content the archive itself had just written.
    """

    hostile = "line one\u2028line two\u2029ends\u0085here"
    source, _, task_db = _fixture(tmp_path)
    archive = tmp_path / "archive"
    _append_task_note(task_db, hostile)
    ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")

    reloaded = load_archived_team(archive, "orc-test")
    survivor = next(note for note in reloaded.task_notes if note.content == hostile)

    assert survivor.content == hostile
    assert len(_promoted_task_note_records(archive)) == 5
    # And the file is still the accumulator the next ingest reads, which is where it broke before.
    _, repeated = ingest_orc(archive, source, ROOT, "orc-test", "America/New_York")
    assert repeated.newly_promoted_task_notes == 0
    assert repeated.task_notes == 5


def test_the_record_file_primitives_round_trip_every_json_string(tmp_path: Path) -> None:
    """`canonical_jsonl` and `read_jsonl` are one contract and are tested as one."""

    path = tmp_path / "records.jsonl"
    records: tuple[dict[str, JsonValue], ...] = (
        {"text": "plain"},
        {"text": "sep\u2028para\u2029nel\u0085end"},
        {"text": "tab\tcarriage\rnewline\nquote\""},
        {"text": ""},
    )
    path.write_text(canonical_jsonl(records), encoding="utf-8")

    assert path.read_text(encoding="utf-8").count("\n") == 4
    assert tuple(read_jsonl(path)) == records
