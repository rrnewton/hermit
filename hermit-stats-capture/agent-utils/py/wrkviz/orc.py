"""Read-only Orc SQLite snapshots and provider-neutral timeline normalization."""

from __future__ import annotations

import hashlib
import json
import os
import re
import secrets
import sqlite3
import stat
from collections import Counter, defaultdict
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

from wrkviz.model import (
    Agent,
    Edge,
    Event,
    SourceSnapshot,
    TaskNote,
    TeamData,
    ToolCall,
    Turn,
    task_note_key,
)


_SAFE_COMPONENT = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
_ORC_TOOL = re.compile(r"\borc\.([A-Za-z_][A-Za-z0-9_]*)\s*\(")
_AUXILIARY_POLICY_LEGACY = "legacy-message-prefix-v1"
_AUXILIARY_POLICY_STABLE_SPAWNS = "stable-spawn-subset-v1"
_AUXILIARY_POLICY_NOT_APPLICABLE = "not-applicable"
_AUXILIARY_DEGRADATION_REASON = (
    "conversation-history-rewritten-stable-spawns-preserved"
)
_PREFIX_OVERRIDE_POLICY = "operator-accepted-prefix-rewrite-v1"
# A different event from _AUXILIARY_DEGRADATION_REASON and therefore a different string. That one
# records something this code *detected and tolerated* on its own authority, about the rewritable
# conversation projection. This one records that the durable append prefix -- the thing the archive
# otherwise treats as immutable -- did not match, and that a human passed an explicit flag saying
# so anyway. A reader who greps for one must not find the other, because "the tool decided" and "an
# operator decided" have entirely different follow-up actions.
_PREFIX_OVERRIDE_DEGRADATION_REASON = (
    "append-prefix-rewritten-operator-accepted-rows-preserved"
)
# The exact column lists the append-prefix digest covers, in digest order. These exist as constants
# rather than inline SQL so the guard's digest and the diff that explains a mismatch are provably
# reading the same columns: a diff that omitted a digested column could report "nothing changed"
# about a refusal, which is worse than no diff at all. `_prefix_query` builds both statements, and
# `test_prefix_scopes_reproduce_the_guard_digest` pins the agreement.
_CONTENT_BLOCK_PREFIX_COLUMNS = (
    "rowid",
    "id",
    "message_id",
    "session_id",
    "block_index",
    "created_at_ms",
    "turn_index",
    "role",
    "block_type",
    "content",
    "searchable_text",
    "code_input",
    "code_output",
    "code_exit_code",
    "model",
    "user_source",
    "token_count",
    "extra",
)
_MESSAGE_PREFIX_COLUMNS = (
    "id",
    "session_id",
    "role",
    "created_at_ms",
    "message_json",
    "search_text",
)
# Caps on the recorded diff. The whole point of the override is that it runs against archives whose
# guarded prefix is hundreds of megabytes, so every part of the evidence is bounded and every bound
# is reported as a flag rather than applied silently. Twenty rows is far more than the observed
# incident needs (one) and still small enough to read in a terminal and to store in every future
# run receipt for the same source.
#
# These caps stayed small on purpose after being reconsidered, and the reason is
# `superseded_snapshot_path` below: an accepted override no longer garbage-collects the pre-rewrite
# database, so the recorded diff is a *summary* of an event whose two sides both still exist on
# disk. A reader who needs more than the summary re-runs the comparison against the retained
# object, at whatever fidelity they want, without this code having had to guess in advance how much
# detail would turn out to matter. Raising the caps -- or spilling a complete diff to a
# sidecar file in the archive, which was the other candidate -- would buy a second, still-frozen,
# still-lossy rendering of bytes the archive already keeps, and would do it by writing a
# potentially unbounded file on the one code path that exists to survive pathological inputs. The
# summary is for the operator standing at the terminal; the object store is the evidence.
_PREFIX_DIFF_MAX_ROWS = 20
_PREFIX_DIFF_MAX_KEYS = 8
_PREFIX_DIFF_EXCERPT_CHARS = 160
_PREFIX_DIFF_CONTEXT_CHARS = 24
_PREFIX_DIFF_MAX_JSON_PATHS = 8
_PREFIX_DIFF_MAX_JSON_DEPTH = 6
# A second, tighter bound for the *refusal message*. Twenty rows of eighteen excerpted columns is a
# reasonable thing to store in a receipt and an unreasonable thing to throw at a terminal as one
# exception string, so the refusal spends a few kilobytes and then says how many rows it did not
# name. The full bounded evidence is what gets recorded when the override is actually taken.
_PREFIX_DIFF_MAX_MESSAGE_CHARS = 4000
# Only attempt the structural JSON diff on values small enough that parsing them twice is cheap.
# Above this the character-window excerpt still localizes the change; what is lost is only the
# field *name*, and paying an unbounded parse to recover a name is the wrong trade in a code path
# that exists to survive pathological inputs.
_PREFIX_DIFF_MAX_JSON_CHARS = 1_000_000
_EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
_SNAPSHOT_OBJECT_ROOT = ".objects"
_TASK_PROJECTION_ROOT = ".projections"
_TASK_PROJECTION_POLICY = "frozen-note-provenance-v2"
_TASK_HISTORY_POLICY = "frozen-note-history-v3"
_TASK_PROJECTION_DEGRADATION_REASON = "task-metadata-rewritten-enrichment-preserved"
_TASK_HISTORY_DEGRADATION_REASON = "task-source-rewritten-frozen-history-preserved"
_EMPTY_TASK_REWRITE_SHA256 = hashlib.sha256(b"task-rewrite:none").hexdigest()
_SEMANTIC_IDENTITY_LEGACY = "legacy-raw-v1"
_SEMANTIC_IDENTITY_DETERMINISTIC = "normalized-v2"
_SOURCE_STATE_LIVE = "live"
_SOURCE_STATE_DETACHED = "detached"
_SESSION_STATE_SHIFT = 64
_SESSION_STATE_MASK = (1 << _SESSION_STATE_SHIFT) - 1
_SESSION_STATE_TAG = 1 << (_SESSION_STATE_SHIFT * 2)
# The two tables a continuation boundary's row ordinal can ever have indexed, and the identity
# column each of them numbers rows by. `messages` is the modern append-only transcript and numbers
# its rows with an explicit `id`; `content_blocks` is the older per-block store whose ordinal is its
# `rowid`. The ordinal itself is just an integer and says nothing about which of the two it came
# from, which is exactly the ambiguity `OrcContinuationLink.predecessor_source_table` closes.
#
# Order matters only for the message a refusal prints; resolution never picks by position. See
# `_resolve_continuation_boundary` for why a positional preference would be the wrong rule.
_CONTINUATION_BOUNDARY_IDENTITY: Mapping[str, str] = {
    "messages": "id",
    "content_blocks": "rowid",
}


class OrcParseError(ValueError):
    """Raised when an Orc source or its append-only history is invalid."""


@dataclass(frozen=True)
class OrcAuxiliaryStatus:
    """Bounded provenance for Orc's rewritable conversation-state projection."""

    policy: str
    message_count: int
    message_sha256: str
    stable_spawn_count: int
    stable_spawn_sha256: str
    rewrite_count: int
    last_rewrite_at: str | None
    degraded: bool
    degradation_reason: str | None

    def to_json_obj(self) -> dict[str, object]:
        """Return the manifest representation without raw conversation payloads."""

        return {
            "policy": self.policy,
            "message_count": self.message_count,
            "message_sha256": self.message_sha256,
            "stable_spawn_count": self.stable_spawn_count,
            "stable_spawn_sha256": self.stable_spawn_sha256,
            "rewrite_count": self.rewrite_count,
            "last_rewrite_at": self.last_rewrite_at,
            "degraded": self.degraded,
            "degradation_reason": self.degradation_reason,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcAuxiliaryStatus:
        """Strictly decode schema-v2 auxiliary provenance."""

        _require_exact_keys(
            raw,
            {
                "policy",
                "message_count",
                "message_sha256",
                "stable_spawn_count",
                "stable_spawn_sha256",
                "rewrite_count",
                "last_rewrite_at",
                "degraded",
                "degradation_reason",
            },
            where,
        )
        policy = _required_string(raw.get("policy"), f"{where}.policy")
        if policy not in (
            _AUXILIARY_POLICY_STABLE_SPAWNS,
            _AUXILIARY_POLICY_NOT_APPLICABLE,
        ):
            raise OrcParseError(f"{where}.policy: unsupported policy {policy!r}")
        message_sha256 = _sha256_string(
            raw.get("message_sha256"), f"{where}.message_sha256"
        )
        stable_spawn_sha256 = _sha256_string(
            raw.get("stable_spawn_sha256"), f"{where}.stable_spawn_sha256"
        )
        rewrite_count = _nonnegative_integer(
            raw.get("rewrite_count"), f"{where}.rewrite_count"
        )
        last_rewrite_at = _optional_string(
            raw.get("last_rewrite_at"), f"{where}.last_rewrite_at"
        )
        degraded = _boolean(raw.get("degraded"), f"{where}.degraded")
        degradation_reason = _optional_string(
            raw.get("degradation_reason"), f"{where}.degradation_reason"
        )
        if rewrite_count == 0 and (
            last_rewrite_at is not None or degraded or degradation_reason is not None
        ):
            raise OrcParseError(
                f"{where}: zero rewrites cannot carry degradation metadata"
            )
        if rewrite_count > 0 and (
            last_rewrite_at is None
            or not degraded
            or degradation_reason != _AUXILIARY_DEGRADATION_REASON
        ):
            raise OrcParseError(
                f"{where}: rewrite history requires complete degradation metadata"
            )
        if policy == _AUXILIARY_POLICY_NOT_APPLICABLE and (
            rewrite_count != 0
            or _nonnegative_integer(
                raw.get("message_count"), f"{where}.message_count"
            )
            != 0
            or _nonnegative_integer(
                raw.get("stable_spawn_count"), f"{where}.stable_spawn_count"
            )
            != 0
            or message_sha256 != _EMPTY_SHA256
            or stable_spawn_sha256 != _EMPTY_SHA256
        ):
            raise OrcParseError(
                f"{where}: not-applicable auxiliary provenance must be empty"
            )
        return cls(
            policy=policy,
            message_count=_nonnegative_integer(
                raw.get("message_count"), f"{where}.message_count"
            ),
            message_sha256=message_sha256,
            stable_spawn_count=_nonnegative_integer(
                raw.get("stable_spawn_count"), f"{where}.stable_spawn_count"
            ),
            stable_spawn_sha256=stable_spawn_sha256,
            rewrite_count=rewrite_count,
            last_rewrite_at=last_rewrite_at,
            degraded=degraded,
            degradation_reason=degradation_reason,
        )


@dataclass(frozen=True)
class OrcPrefixColumnChange:
    """One guarded column whose stored value differs from the recorded append prefix.

    ``previous`` and ``observed`` are *excerpts*, not values: a rendered window centred on the first
    and last character that differ, so a one-field edit inside a megabyte of ``message_json`` reads
    as ``"token_count":null`` against ``"token_count":445`` instead of two identical truncated
    prefixes. ``bounded`` says the window is a window; ``json_paths`` names the changed field when
    both sides parse as JSON, which is the answer an operator actually wants.
    """

    column: str
    previous: str
    observed: str
    bounded: bool
    json_paths: tuple[str, ...]
    json_paths_bounded: bool

    def to_json_obj(self) -> dict[str, object]:
        """Return this column change as bounded manifest and receipt evidence."""

        return {
            "column": self.column,
            "previous": self.previous,
            "observed": self.observed,
            "bounded": self.bounded,
            "json_paths": list(self.json_paths),
            "json_paths_bounded": self.json_paths_bounded,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcPrefixColumnChange:
        """Strictly decode one recorded column change."""

        _require_exact_keys(
            raw,
            {
                "column",
                "previous",
                "observed",
                "bounded",
                "json_paths",
                "json_paths_bounded",
            },
            where,
        )
        return cls(
            column=_required_string(raw.get("column"), f"{where}.column"),
            previous=_string_value(raw.get("previous"), f"{where}.previous"),
            observed=_string_value(raw.get("observed"), f"{where}.observed"),
            bounded=_boolean(raw.get("bounded"), f"{where}.bounded"),
            json_paths=tuple(
                _required_string(item, f"{where}.json_paths[{index}]")
                for index, item in enumerate(
                    _array(raw.get("json_paths"), f"{where}.json_paths")
                )
            ),
            json_paths_bounded=_boolean(
                raw.get("json_paths_bounded"), f"{where}.json_paths_bounded"
            ),
        )

    def describe(self) -> str:
        """Return one operator-readable line naming the column, its fields, and the excerpts."""

        fields = ""
        if self.json_paths:
            fields = " ." + ", .".join(self.json_paths)
            if self.json_paths_bounded:
                fields += ", ..."
        return f"{self.column}{fields}: {self.previous} -> {self.observed}"


@dataclass(frozen=True)
class OrcPrefixRowChange:
    """One row at or below the append watermark whose guarded columns were rewritten."""

    table: str
    row_id: int
    columns: tuple[OrcPrefixColumnChange, ...]

    def to_json_obj(self) -> dict[str, object]:
        """Return this row change as bounded manifest and receipt evidence."""

        return {
            "table": self.table,
            "row_id": self.row_id,
            "columns": [column.to_json_obj() for column in self.columns],
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcPrefixRowChange:
        """Strictly decode one recorded row change."""

        _require_exact_keys(raw, {"table", "row_id", "columns"}, where)
        columns = tuple(
            OrcPrefixColumnChange.from_json_obj(
                _mapping(item, f"{where}.columns[{index}]"),
                f"{where}.columns[{index}]",
            )
            for index, item in enumerate(
                _array(raw.get("columns"), f"{where}.columns")
            )
        )
        if not columns:
            raise OrcParseError(f"{where}: a recorded row change must name a column")
        return cls(
            table=_required_string(raw.get("table"), f"{where}.table"),
            # Signed, because SQLite rowids legitimately are. Narrowing this to non-negative would
            # make a real archive undecodable to buy a validation nobody asked for.
            row_id=_integer(raw.get("row_id"), f"{where}.row_id"),
            columns=columns,
        )

    def describe(self) -> str:
        """Return one operator-readable line naming the row and each changed column."""

        return (
            f"{self.table} row {self.row_id}: "
            + "; ".join(column.describe() for column in self.columns)
        )


@dataclass(frozen=True)
class OrcAppendPrefixOverride:
    """An operator's recorded decision to re-baseline one source's append-prefix digest.

    This is provenance, not permission: the record is written *after* the fact and is sticky, so an
    archive that was ever re-baselined says so forever, in the manifest, next to the source it
    happened to. ``override_count`` accumulates; ``changed_rows`` keeps the most recent event's
    bounded evidence, because a counter alone cannot tell a later reader whether the accepted change
    was a metadata backfill or something that should have been refused. Earlier events remain in the
    run receipts written by the runs that made them.

    ``superseded_snapshot_path`` is what makes any of that checkable rather than merely readable. It
    names the pre-rewrite database -- the exact object the manifest pointed at before this override
    re-baselined the digest -- and naming it in the manifest is what keeps it alive: object GC
    retains everything the manifest still references, so the bytes survive the command that made
    them unverifiable instead of being reclaimed by it. With both sides on disk the bounded
    ``changed_rows`` summary stops being the only account of the event; an operator who accepted a
    rewrite and later regretted it can diff the two databases directly, at full fidelity, with
    nothing but ``sqlite3``.

    Retention is one deep, per source, and it is reclaimed by the next *accepted* override on that
    same source, which supersedes this pointer and lets GC take the older object. That is the
    deliberate choice over keeping every superseded generation: these are whole session databases,
    an archive is not a backup system, and every reclamation is therefore the direct consequence of
    a second explicit operator action carrying the same warning as the first -- never of a routine
    run. Nothing here is load-bearing for a later ingest, so a missing object is tolerated rather
    than fatal: see :func:`prune_orc_snapshot_objects`.

    A schema-v1 source keeps its snapshot at its own mirrored source path instead of in the managed
    object store, where GC never reaches it. The pointer then records that path as a plain note
    saying where the bytes are, and it is the only case in which it is not a content-addressed
    object name.
    """

    policy: str
    source_path: str
    accepted_at: str
    override_count: int
    previous_append_prefix_sha256: str
    observed_append_prefix_sha256: str
    superseded_snapshot_path: str
    superseded_sha256: str
    changed_row_count: int
    changed_rows: tuple[OrcPrefixRowChange, ...]
    changed_rows_bounded: bool
    degraded: bool
    degradation_reason: str

    def to_json_obj(self) -> dict[str, object]:
        """Return the bounded override record stored beside its source."""

        return {
            "policy": self.policy,
            "source_path": self.source_path,
            "accepted_at": self.accepted_at,
            "override_count": self.override_count,
            "previous_append_prefix_sha256": self.previous_append_prefix_sha256,
            "observed_append_prefix_sha256": self.observed_append_prefix_sha256,
            "superseded_snapshot_path": self.superseded_snapshot_path,
            "superseded_sha256": self.superseded_sha256,
            "changed_row_count": self.changed_row_count,
            "changed_rows": [row.to_json_obj() for row in self.changed_rows],
            "changed_rows_bounded": self.changed_rows_bounded,
            "degraded": self.degraded,
            "degradation_reason": self.degradation_reason,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcAppendPrefixOverride:
        """Strictly decode one recorded operator override."""

        _require_exact_keys(
            raw,
            {
                "policy",
                "source_path",
                "accepted_at",
                "override_count",
                "previous_append_prefix_sha256",
                "observed_append_prefix_sha256",
                "superseded_snapshot_path",
                "superseded_sha256",
                "changed_row_count",
                "changed_rows",
                "changed_rows_bounded",
                "degraded",
                "degradation_reason",
            },
            where,
        )
        policy = _required_string(raw.get("policy"), f"{where}.policy")
        if policy != _PREFIX_OVERRIDE_POLICY:
            raise OrcParseError(f"{where}.policy: unsupported policy {policy!r}")
        override_count = _nonnegative_integer(
            raw.get("override_count"), f"{where}.override_count"
        )
        if override_count < 1:
            raise OrcParseError(
                f"{where}: a recorded override must have happened at least once"
            )
        degraded = _boolean(raw.get("degraded"), f"{where}.degraded")
        degradation_reason = _required_string(
            raw.get("degradation_reason"), f"{where}.degradation_reason"
        )
        # An override that does not carry its degradation is the failure mode this record exists to
        # prevent, so it is rejected on read rather than repaired: a manifest claiming a clean
        # source while holding override evidence is corrupt, not merely stale.
        if not degraded or degradation_reason != _PREFIX_OVERRIDE_DEGRADATION_REASON:
            raise OrcParseError(
                f"{where}: an accepted prefix rewrite must record its degradation"
            )
        previous_digest = _sha256_string(
            raw.get("previous_append_prefix_sha256"),
            f"{where}.previous_append_prefix_sha256",
        )
        observed_digest = _sha256_string(
            raw.get("observed_append_prefix_sha256"),
            f"{where}.observed_append_prefix_sha256",
        )
        if previous_digest == observed_digest:
            raise OrcParseError(
                f"{where}: an accepted prefix rewrite must record two different digests"
            )
        # The retained pointer is checked for shape here rather than for existence, and the two are
        # different promises on purpose. Shape is an archive invariant this code controls, while
        # existence is a property of a directory an operator can and sometimes must clean up by
        # hand: refusing to decode an otherwise valid manifest because a piece of evidence was
        # deleted would make a routine future ingest fail for a reason that has nothing to do with
        # it.
        #
        # The shape rule binds only inside the managed object store, because only there does the
        # pointer *do* anything -- it is what keeps GC's hands off those bytes, so it must be the
        # content-addressed name of the digest recorded beside it and cannot be aimed elsewhere.
        # Outside it the pointer is a note to a human, and it has to stay one: a schema-v1 source
        # is stored at its own mirrored source path rather than in the object store, GC does not
        # reach that path at all, and rejecting the honest note would be the only thing standing
        # between such an archive and a decodable manifest.
        superseded_sha256 = _sha256_string(
            raw.get("superseded_sha256"), f"{where}.superseded_sha256"
        )
        superseded_snapshot_path = _required_string(
            raw.get("superseded_snapshot_path"), f"{where}.superseded_snapshot_path"
        )
        _safe_relative(superseded_snapshot_path)
        expected_superseded = _snapshot_object_relative(superseded_sha256)
        if (
            superseded_snapshot_path.startswith(f"{_SNAPSHOT_OBJECT_ROOT}/")
            and superseded_snapshot_path != expected_superseded
        ):
            raise OrcParseError(
                f"{where}.superseded_snapshot_path: expected content-addressed path "
                f"{expected_superseded!r}"
            )
        changed_rows = tuple(
            OrcPrefixRowChange.from_json_obj(
                _mapping(item, f"{where}.changed_rows[{index}]"),
                f"{where}.changed_rows[{index}]",
            )
            for index, item in enumerate(
                _array(raw.get("changed_rows"), f"{where}.changed_rows")
            )
        )
        changed_row_count = _nonnegative_integer(
            raw.get("changed_row_count"), f"{where}.changed_row_count"
        )
        changed_rows_bounded = _boolean(
            raw.get("changed_rows_bounded"), f"{where}.changed_rows_bounded"
        )
        if (
            changed_row_count < 1
            or changed_row_count < len(changed_rows)
            or changed_rows_bounded != (len(changed_rows) < changed_row_count)
        ):
            raise OrcParseError(
                f"{where}: recorded row evidence disagrees with its own row count"
            )
        return cls(
            policy=policy,
            source_path=_required_string(
                raw.get("source_path"), f"{where}.source_path"
            ),
            accepted_at=_required_string(
                raw.get("accepted_at"), f"{where}.accepted_at"
            ),
            override_count=override_count,
            previous_append_prefix_sha256=previous_digest,
            observed_append_prefix_sha256=observed_digest,
            superseded_snapshot_path=superseded_snapshot_path,
            superseded_sha256=superseded_sha256,
            changed_row_count=changed_row_count,
            changed_rows=changed_rows,
            changed_rows_bounded=changed_rows_bounded,
            degraded=degraded,
            degradation_reason=degradation_reason,
        )

    def describe(self) -> tuple[str, ...]:
        """Return the operator-readable report: one headline, then one line per changed row."""

        headline = (
            f"accepted append-prefix rewrite for {self.source_path}: "
            f"{self.changed_row_count} row(s) changed at or below the append watermark "
            f"(override #{self.override_count}, {self.degradation_reason})"
        )
        lines = [headline, *(row.describe() for row in self.changed_rows)]
        if self.changed_rows_bounded:
            lines.append(
                f"... {self.changed_row_count - len(self.changed_rows)} further changed "
                "row(s) not shown"
            )
        # Last, because it is the line an operator comes back for hours later: where the bytes this
        # override stopped trusting actually are. Printing the path is the whole affordance -- the
        # rows above are a summary, and this is how to check it.
        lines.append(
            f"pre-rewrite snapshot retained for comparison: {self.superseded_snapshot_path} "
            "(reclaimed by the next accepted override on this source)"
        )
        return tuple(lines)


@dataclass(frozen=True)
class OrcTaskProjection:
    """Pointer and provenance for frozen per-note task enrichment."""

    policy: str
    path: str
    note_count: int
    sha256: str
    observed_enrichment_sha256: str
    observed_note_rewrite_sha256: str
    missing_note_count: int
    missing_note_ids_sha256: str
    observed_note_sequence: int | None
    unobserved_note_id_gap_count: int | None
    unobserved_note_id_gap_sha256: str | None
    rewrite_count: int
    last_rewrite_at: str | None
    degraded: bool
    degradation_reason: str | None

    def to_json_obj(self) -> dict[str, object]:
        """Return the bounded schema-v2 task projection."""

        return {
            "policy": self.policy,
            "path": self.path,
            "note_count": self.note_count,
            "sha256": self.sha256,
            "observed_enrichment_sha256": self.observed_enrichment_sha256,
            "observed_note_rewrite_sha256": self.observed_note_rewrite_sha256,
            "missing_note_count": self.missing_note_count,
            "missing_note_ids_sha256": self.missing_note_ids_sha256,
            "observed_note_sequence": self.observed_note_sequence,
            "unobserved_note_id_gap_count": self.unobserved_note_id_gap_count,
            "unobserved_note_id_gap_sha256": self.unobserved_note_id_gap_sha256,
            "rewrite_count": self.rewrite_count,
            "last_rewrite_at": self.last_rewrite_at,
            "degraded": self.degraded,
            "degradation_reason": self.degradation_reason,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcTaskProjection:
        """Strictly decode schema-v2 task evidence."""

        legacy_fields = {
            "policy",
            "path",
            "note_count",
            "sha256",
            "observed_enrichment_sha256",
            "rewrite_count",
            "last_rewrite_at",
            "degraded",
            "degradation_reason",
        }
        prior_current_fields = legacy_fields | {
            "observed_note_rewrite_sha256",
            "missing_note_count",
            "missing_note_ids_sha256",
        }
        sequence_fields = prior_current_fields | {"observed_note_sequence"}
        current_fields = sequence_fields | {
            "unobserved_note_id_gap_count",
            "unobserved_note_id_gap_sha256",
        }
        if set(raw) not in (
            legacy_fields,
            prior_current_fields,
            sequence_fields,
            current_fields,
        ):
            _require_exact_keys(raw, current_fields, where)
        policy = _required_string(raw.get("policy"), f"{where}.policy")
        if policy not in (_TASK_PROJECTION_POLICY, _TASK_HISTORY_POLICY):
            raise OrcParseError(f"{where}.policy: unsupported policy {policy!r}")
        sha256 = _sha256_string(raw.get("sha256"), f"{where}.sha256")
        path = _required_string(raw.get("path"), f"{where}.path")
        if path != _task_projection_relative(sha256):
            raise OrcParseError(f"{where}.path: invalid content-addressed projection path")
        rewrite_count = _nonnegative_integer(
            raw.get("rewrite_count"), f"{where}.rewrite_count"
        )
        last_rewrite_at = _optional_string(
            raw.get("last_rewrite_at"), f"{where}.last_rewrite_at"
        )
        degraded = _boolean(raw.get("degraded"), f"{where}.degraded")
        degradation_reason = _optional_string(
            raw.get("degradation_reason"), f"{where}.degradation_reason"
        )
        if rewrite_count == 0 and (
            last_rewrite_at is not None or degraded or degradation_reason is not None
        ):
            raise OrcParseError(
                f"{where}: zero rewrites cannot carry degradation metadata"
            )
        if rewrite_count > 0 and (
            last_rewrite_at is None
            or not degraded
            or degradation_reason
            not in (
                _TASK_PROJECTION_DEGRADATION_REASON,
                _TASK_HISTORY_DEGRADATION_REASON,
            )
        ):
            raise OrcParseError(
                f"{where}: rewrite history requires complete degradation metadata"
            )
        return cls(
            policy=policy,
            path=path,
            note_count=_nonnegative_integer(
                raw.get("note_count"), f"{where}.note_count"
            ),
            sha256=sha256,
            observed_enrichment_sha256=_sha256_string(
                raw.get("observed_enrichment_sha256"),
                f"{where}.observed_enrichment_sha256",
            ),
            observed_note_rewrite_sha256=(
                _EMPTY_TASK_REWRITE_SHA256
                if "observed_note_rewrite_sha256" not in raw
                else _sha256_string(
                    raw.get("observed_note_rewrite_sha256"),
                    f"{where}.observed_note_rewrite_sha256",
                )
            ),
            missing_note_count=(
                0
                if "missing_note_count" not in raw
                else _nonnegative_integer(
                    raw.get("missing_note_count"), f"{where}.missing_note_count"
                )
            ),
            missing_note_ids_sha256=(
                hashlib.sha256(b"[]").hexdigest()
                if "missing_note_ids_sha256" not in raw
                else _sha256_string(
                    raw.get("missing_note_ids_sha256"),
                    f"{where}.missing_note_ids_sha256",
                )
            ),
            observed_note_sequence=(
                None
                if "observed_note_sequence" not in raw
                else _nonnegative_integer(
                    raw.get("observed_note_sequence"),
                    f"{where}.observed_note_sequence",
                )
            ),
            unobserved_note_id_gap_count=(
                None
                if "unobserved_note_id_gap_count" not in raw
                else _nonnegative_integer(
                    raw.get("unobserved_note_id_gap_count"),
                    f"{where}.unobserved_note_id_gap_count",
                )
            ),
            unobserved_note_id_gap_sha256=(
                None
                if "unobserved_note_id_gap_sha256" not in raw
                else _sha256_string(
                    raw.get("unobserved_note_id_gap_sha256"),
                    f"{where}.unobserved_note_id_gap_sha256",
                )
            ),
            rewrite_count=rewrite_count,
            last_rewrite_at=last_rewrite_at,
            degraded=degraded,
            degradation_reason=degradation_reason,
        )


@dataclass(frozen=True)
class OrcSourceCopy:
    """One consistent SQLite backup and its logical append-prefix evidence."""

    source_path: str
    snapshot_path: str
    kind: str
    owner_session_id: str
    lineage_root_session_id: str | None
    source_size: int
    snapshot_size: int
    sha256: str
    append_count: int
    append_max_id: int
    append_prefix_sha256: str
    semantic_identity_mode: str
    semantic_sha256: str
    semantic_complete_bytes: int
    canonical_semantic_sha256: str | None
    canonical_semantic_complete_bytes: int | None
    semantic_alias_baseline_path: str | None
    semantic_baseline_path: str | None
    source_state: str
    task_source_ordinal: int | None
    auxiliary: OrcAuxiliaryStatus
    task_projection: OrcTaskProjection | None
    captured_at: str
    # Sticky: once an operator has re-baselined this source's append prefix, the archive says so
    # for the rest of its life, even across later clean ingests. Kept separate from `auxiliary`
    # because that field's degradation describes Orc's rewritable conversation projection, and the
    # two events can and do occur together -- one `degradation_reason` string cannot hold both.
    append_prefix_override: OrcAppendPrefixOverride | None = None
    # What Orc itself says its storage schema is, read from the in-band `schema_version` table of
    # the snapshotted database and copied here verbatim. `None` means the database has no such
    # table -- a genuinely older Orc that predates it -- and never means "we did not look".
    #
    # This field exists because an audit found that no SQL anywhere in this module selected it: every
    # decision about Orc's storage layout was being made by duck-typing which tables happened to
    # exist (`_session_storage_table`), and duck-typing is exactly what fails on a database caught
    # mid-transition, where the *old* table still holds the rows and the *new* table already exists
    # but is nearly empty. The recorded version does not by itself decide anything -- the boundary
    # resolver deliberately uses evidence, not version numbers -- but it is the one fact that lets a
    # human reading a manifest months later say "this source was schema 3 when we froze it and is
    # schema 8 now", which is the sentence nobody could write while diagnosing the false positive
    # that motivated it.
    provider_schema_version: int | None = None

    def to_json_obj(self) -> dict[str, object]:
        """Return this validated source-copy record as a JSON object."""

        return {
            "source_path": self.source_path,
            "snapshot_path": self.snapshot_path,
            "kind": self.kind,
            "owner_session_id": self.owner_session_id,
            "lineage_root_session_id": self.lineage_root_session_id,
            "source_size": self.source_size,
            "snapshot_size": self.snapshot_size,
            "sha256": self.sha256,
            "append_count": self.append_count,
            "append_max_id": self.append_max_id,
            "append_prefix_sha256": self.append_prefix_sha256,
            "semantic_identity_mode": self.semantic_identity_mode,
            "semantic_sha256": self.semantic_sha256,
            "semantic_complete_bytes": self.semantic_complete_bytes,
            "canonical_semantic_sha256": self.canonical_semantic_sha256,
            "canonical_semantic_complete_bytes": self.canonical_semantic_complete_bytes,
            "semantic_alias_baseline_path": self.semantic_alias_baseline_path,
            "semantic_baseline_path": self.semantic_baseline_path,
            "source_state": self.source_state,
            "task_source_ordinal": self.task_source_ordinal,
            "auxiliary": self.auxiliary.to_json_obj(),
            "task_projection": (
                self.task_projection.to_json_obj()
                if self.task_projection is not None
                else None
            ),
            "captured_at": self.captured_at,
            "append_prefix_override": (
                self.append_prefix_override.to_json_obj()
                if self.append_prefix_override is not None
                else None
            ),
            "provider_schema_version": self.provider_schema_version,
        }

    @classmethod
    def from_json_obj(
        cls,
        raw: Mapping[str, object],
        where: str,
        manifest_schema_version: int,
    ) -> OrcSourceCopy:
        """Validate and decode one source-copy record from archive JSON."""

        source_path = _required_string(raw.get("source_path"), f"{where}.source_path")
        snapshot_path = _required_string(
            raw.get("snapshot_path"), f"{where}.snapshot_path"
        )
        _safe_relative(source_path)
        _safe_relative(snapshot_path)
        if manifest_schema_version == 2:
            legacy_fields = {
                "source_path",
                "snapshot_path",
                "kind",
                "owner_session_id",
                "source_size",
                "snapshot_size",
                "sha256",
                "append_count",
                "append_max_id",
                "append_prefix_sha256",
                "semantic_identity_mode",
                "semantic_sha256",
                "semantic_complete_bytes",
                "semantic_baseline_path",
                "source_state",
                "task_source_ordinal",
                "auxiliary",
                "task_projection",
                "captured_at",
            }
            lineage_fields = legacy_fields | {"lineage_root_session_id"}
            canonical_fields = lineage_fields | {
                "canonical_semantic_sha256",
                "canonical_semantic_complete_bytes",
            }
            alias_fields = canonical_fields | {
                "semantic_alias_baseline_path",
            }
            override_fields = alias_fields | {"append_prefix_override"}
            current_fields = override_fields | {"provider_schema_version"}
            if set(raw) not in (
                legacy_fields,
                lineage_fields,
                canonical_fields,
                alias_fields,
                override_fields,
                current_fields,
            ):
                _require_exact_keys(raw, current_fields, where)
        kind = _required_string(raw.get("kind"), f"{where}.kind")
        if kind not in ("session", "task"):
            raise OrcParseError(f"{where}.kind: unsupported Orc database kind {kind!r}")
        sha256 = _required_string(raw.get("sha256"), f"{where}.sha256")
        append_digest = _required_string(
            raw.get("append_prefix_sha256"), f"{where}.append_prefix_sha256"
        )
        _sha256_string(sha256, f"{where}.sha256")
        _sha256_string(append_digest, f"{where}.append_prefix_sha256")
        if manifest_schema_version == 1:
            if source_path != snapshot_path:
                raise OrcParseError(
                    f"{where}: schema-v1 source and snapshot paths must match"
                )
            auxiliary = OrcAuxiliaryStatus(
                policy=_AUXILIARY_POLICY_LEGACY,
                message_count=_nonnegative_integer(
                    raw.get("auxiliary_count"), f"{where}.auxiliary_count"
                ),
                message_sha256=_sha256_string(
                    raw.get("auxiliary_prefix_sha256"),
                    f"{where}.auxiliary_prefix_sha256",
                ),
                stable_spawn_count=0,
                stable_spawn_sha256=_EMPTY_SHA256,
                rewrite_count=0,
                last_rewrite_at=None,
                degraded=False,
                degradation_reason=None,
            )
            task_projection: OrcTaskProjection | None = None
            semantic_identity_mode = _SEMANTIC_IDENTITY_LEGACY
            semantic_sha256 = sha256
            semantic_complete_bytes = _nonnegative_integer(
                raw.get("snapshot_size"), f"{where}.snapshot_size"
            )
            canonical_semantic_sha256: str | None = None
            canonical_semantic_complete_bytes: int | None = None
            semantic_baseline_path: str | None = snapshot_path
            source_state = _SOURCE_STATE_LIVE
            task_source_ordinal: int | None = 0 if kind == "task" else None
            append_prefix_override: OrcAppendPrefixOverride | None = None
            provider_schema_version: int | None = None
        elif manifest_schema_version == 2:
            expected_snapshot_path = _snapshot_object_relative(sha256)
            if snapshot_path != expected_snapshot_path:
                raise OrcParseError(
                    f"{where}.snapshot_path: expected content-addressed path "
                    f"{expected_snapshot_path!r}"
                )
            auxiliary = OrcAuxiliaryStatus.from_json_obj(
                _mapping(raw.get("auxiliary"), f"{where}.auxiliary"),
                f"{where}.auxiliary",
            )
            raw_task_projection = raw.get("task_projection")
            task_projection = (
                None
                if raw_task_projection is None
                else OrcTaskProjection.from_json_obj(
                    _mapping(raw_task_projection, f"{where}.task_projection"),
                    f"{where}.task_projection",
                )
            )
            semantic_sha256 = _sha256_string(
                raw.get("semantic_sha256"), f"{where}.semantic_sha256"
            )
            semantic_identity_mode = _required_string(
                raw.get("semantic_identity_mode"),
                f"{where}.semantic_identity_mode",
            )
            if semantic_identity_mode not in (
                _SEMANTIC_IDENTITY_LEGACY,
                _SEMANTIC_IDENTITY_DETERMINISTIC,
            ):
                raise OrcParseError(
                    f"{where}.semantic_identity_mode: unsupported mode "
                    f"{semantic_identity_mode!r}"
                )
            semantic_complete_bytes = _nonnegative_integer(
                raw.get("semantic_complete_bytes"),
                f"{where}.semantic_complete_bytes",
            )
            raw_canonical_sha256 = raw.get("canonical_semantic_sha256")
            canonical_semantic_sha256 = (
                None
                if raw_canonical_sha256 is None
                else _sha256_string(
                    raw_canonical_sha256,
                    f"{where}.canonical_semantic_sha256",
                )
            )
            raw_canonical_bytes = raw.get("canonical_semantic_complete_bytes")
            canonical_semantic_complete_bytes = (
                None
                if raw_canonical_bytes is None
                else _nonnegative_integer(
                    raw_canonical_bytes,
                    f"{where}.canonical_semantic_complete_bytes",
                )
            )
            if (canonical_semantic_sha256 is None) != (
                canonical_semantic_complete_bytes is None
            ):
                raise OrcParseError(
                    f"{where}: canonical semantic identity must be complete"
                )
            semantic_alias_baseline_path = _optional_string(
                raw.get("semantic_alias_baseline_path"),
                f"{where}.semantic_alias_baseline_path",
            )
            if semantic_alias_baseline_path is not None:
                _safe_relative(semantic_alias_baseline_path)
            semantic_baseline_path = _optional_string(
                raw.get("semantic_baseline_path"),
                f"{where}.semantic_baseline_path",
            )
            if semantic_baseline_path is not None:
                _safe_relative(semantic_baseline_path)
            source_state = _required_string(
                raw.get("source_state"), f"{where}.source_state"
            )
            if source_state not in (_SOURCE_STATE_LIVE, _SOURCE_STATE_DETACHED):
                raise OrcParseError(
                    f"{where}.source_state: unsupported state {source_state!r}"
                )
            raw_ordinal = raw.get("task_source_ordinal")
            task_source_ordinal = (
                None
                if raw_ordinal is None
                else _nonnegative_integer(
                    raw_ordinal, f"{where}.task_source_ordinal"
                )
            )
            raw_prefix_override = raw.get("append_prefix_override")
            append_prefix_override = (
                None
                if raw_prefix_override is None
                else OrcAppendPrefixOverride.from_json_obj(
                    _mapping(
                        raw_prefix_override, f"{where}.append_prefix_override"
                    ),
                    f"{where}.append_prefix_override",
                )
            )
            # The override names its own source so that it stays readable when a receipt or a bug
            # report quotes it alone; that makes it forgeable in isolation, so the pairing is
            # checked here, where both halves are in hand.
            if (
                append_prefix_override is not None
                and append_prefix_override.source_path != source_path
            ):
                raise OrcParseError(
                    f"{where}.append_prefix_override.source_path: recorded override "
                    f"belongs to {append_prefix_override.source_path!r}"
                )
            # A rewrite changes the guarded prefix, so it changes the file, so the superseded object
            # and the current one cannot be the same object. If they are, the manifest is claiming
            # to retain a pre-rewrite copy while pointing at the post-rewrite bytes -- retention
            # that reads as satisfied and holds nothing. Caught here because this is the only place
            # both paths are in hand.
            if (
                append_prefix_override is not None
                and append_prefix_override.superseded_snapshot_path == snapshot_path
            ):
                raise OrcParseError(
                    f"{where}.append_prefix_override.superseded_snapshot_path: the "
                    "pre-rewrite snapshot cannot be the current snapshot"
                )
            raw_provider_schema = raw.get("provider_schema_version")
            provider_schema_version = (
                None
                if raw_provider_schema is None
                else _positive_integer(
                    raw_provider_schema, f"{where}.provider_schema_version"
                )
            )
        else:
            raise OrcParseError(
                f"{where}: unsupported Orc manifest schema {manifest_schema_version}"
            )
        if manifest_schema_version == 2 and (
            (kind == "session" and auxiliary.policy != _AUXILIARY_POLICY_STABLE_SPAWNS)
            or (kind == "task" and auxiliary.policy != _AUXILIARY_POLICY_NOT_APPLICABLE)
            or (kind == "session" and task_projection is not None)
            or (kind == "task" and task_projection is None)
            or (kind == "session" and source_state != _SOURCE_STATE_LIVE)
            or (kind == "session" and task_source_ordinal is not None)
            or (kind == "task" and task_source_ordinal is None)
            or (
                kind == "task"
                and raw.get("lineage_root_session_id") is not None
            )
            or (
                semantic_identity_mode == _SEMANTIC_IDENTITY_LEGACY
                and semantic_baseline_path is None
            )
            or (
                semantic_identity_mode == _SEMANTIC_IDENTITY_DETERMINISTIC
                and semantic_baseline_path is not None
            )
            # Task sources never reach the append-prefix guard: their history is protected by the
            # frozen-note projection instead, which has its own rewrite policy and its own
            # degradation reasons. An override recorded against a task source would therefore be
            # describing a comparison that never happened.
            or (kind == "task" and append_prefix_override is not None)
        ):
            raise OrcParseError(
                f"{where}: invalid schema-v2 projections for {kind} source"
            )
        return cls(
            source_path=source_path,
            snapshot_path=snapshot_path,
            kind=kind,
            owner_session_id=_required_string(
                raw.get("owner_session_id"), f"{where}.owner_session_id"
            ),
            lineage_root_session_id=_optional_string(
                raw.get("lineage_root_session_id"),
                f"{where}.lineage_root_session_id",
            ),
            source_size=_nonnegative_integer(
                raw.get("source_size"), f"{where}.source_size"
            ),
            snapshot_size=_nonnegative_integer(
                raw.get("snapshot_size"), f"{where}.snapshot_size"
            ),
            sha256=sha256,
            append_count=_nonnegative_integer(
                raw.get("append_count"), f"{where}.append_count"
            ),
            append_max_id=_nonnegative_integer(
                raw.get("append_max_id"), f"{where}.append_max_id"
            ),
            append_prefix_sha256=append_digest,
            semantic_identity_mode=semantic_identity_mode,
            semantic_sha256=semantic_sha256,
            semantic_complete_bytes=semantic_complete_bytes,
            canonical_semantic_sha256=canonical_semantic_sha256,
            canonical_semantic_complete_bytes=canonical_semantic_complete_bytes,
            semantic_alias_baseline_path=(
                semantic_alias_baseline_path
                if manifest_schema_version == 2
                else None
            ),
            semantic_baseline_path=semantic_baseline_path,
            source_state=source_state,
            task_source_ordinal=task_source_ordinal,
            auxiliary=auxiliary,
            task_projection=task_projection,
            captured_at=_required_string(raw.get("captured_at"), f"{where}.captured_at"),
            append_prefix_override=append_prefix_override,
            provider_schema_version=provider_schema_version,
        )


@dataclass(frozen=True)
class OrcContinuationSpec:
    """One explicitly configured whole-root or native-message-bounded successor."""

    session_id: str
    start_message_id: str | None = None

    def to_json_obj(self) -> dict[str, object]:
        """Return the strict project-config representation of this continuation."""

        return {
            "session_id": self.session_id,
            "start_message_id": self.start_message_id,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcContinuationSpec:
        """Decode one strict bounded-continuation configuration object."""

        _require_exact_keys(raw, {"session_id", "start_message_id"}, where)
        session_id = _safe_component(
            _required_string(raw.get("session_id"), f"{where}.session_id"),
            f"{where}.session_id",
        )
        start_message_id = _optional_string(
            raw.get("start_message_id"), f"{where}.start_message_id"
        )
        return cls(session_id=session_id, start_message_id=start_message_id)

    @classmethod
    def from_value(cls, raw: object, where: str) -> OrcContinuationSpec:
        """Normalize a whole-root string or bounded continuation object."""

        if isinstance(raw, cls):
            return cls.from_json_obj(raw.to_json_obj(), where)
        if isinstance(raw, str):
            return cls(
                session_id=_safe_component(
                    _required_string(raw, where), where
                )
            )
        return cls.from_json_obj(_mapping(raw, where), where)


@dataclass(frozen=True)
class OrcContinuationLink:
    """One explicit, frozen transition between parentless Orc coordinators."""

    predecessor_session_id: str
    session_id: str
    predecessor_source_path: str
    predecessor_source_line: int
    predecessor_at_ms: int
    source_path: str
    start_message_id: str | None
    start_source_line: int | None
    started_at_ms: int
    gap_ms: int
    # Which table `predecessor_source_line` is a row ordinal *in*. It belongs beside that field and
    # is only last because a dataclass wants its defaulted fields last; the default exists so links
    # frozen before this field was added decode without a migration pass over every archive.
    #
    # `None` means the table is not recorded, and that is a live state rather than purely a legacy
    # one: `_resolve_continuation_boundary` declines to record a table when the ordinal resolves to
    # the recorded timestamp in more than one candidate table, because writing a guess there would
    # be indistinguishable from writing a fact.
    predecessor_source_table: str | None = None

    def to_json_obj(self) -> dict[str, object]:
        """Return the exact durable continuation-boundary record."""

        return {
            "predecessor_session_id": self.predecessor_session_id,
            "session_id": self.session_id,
            "predecessor_source_path": self.predecessor_source_path,
            "predecessor_source_line": self.predecessor_source_line,
            "predecessor_source_table": self.predecessor_source_table,
            "predecessor_at_ms": self.predecessor_at_ms,
            "source_path": self.source_path,
            "start_message_id": self.start_message_id,
            "start_source_line": self.start_source_line,
            "started_at_ms": self.started_at_ms,
            "gap_ms": self.gap_ms,
        }

    @classmethod
    def from_json_obj(
        cls, raw: Mapping[str, object], where: str
    ) -> OrcContinuationLink:
        """Strictly decode one frozen Orc continuation boundary."""

        legacy_fields = {
            "predecessor_session_id",
            "session_id",
            "predecessor_source_path",
            "predecessor_source_line",
            "predecessor_at_ms",
            "source_path",
            "started_at_ms",
            "gap_ms",
        }
        bounded_fields = legacy_fields | {"start_message_id", "start_source_line"}
        current_fields = bounded_fields | {"predecessor_source_table"}
        if set(raw) not in (legacy_fields, bounded_fields, current_fields):
            _require_exact_keys(raw, current_fields, where)
        predecessor_session_id = _safe_component(
            _required_string(
                raw.get("predecessor_session_id"),
                f"{where}.predecessor_session_id",
            ),
            f"{where}.predecessor_session_id",
        )
        session_id = _safe_component(
            _required_string(raw.get("session_id"), f"{where}.session_id"),
            f"{where}.session_id",
        )
        predecessor_source_path = _required_string(
            raw.get("predecessor_source_path"),
            f"{where}.predecessor_source_path",
        )
        source_path = _required_string(
            raw.get("source_path"), f"{where}.source_path"
        )
        _safe_relative(predecessor_source_path)
        _safe_relative(source_path)
        start_message_id = _optional_string(
            raw.get("start_message_id"), f"{where}.start_message_id"
        )
        raw_start_source_line = raw.get("start_source_line")
        start_source_line = (
            None
            if raw_start_source_line is None
            else _nonnegative_integer(
                raw_start_source_line, f"{where}.start_source_line"
            )
        )
        predecessor_source_line = _nonnegative_integer(
            raw.get("predecessor_source_line"),
            f"{where}.predecessor_source_line",
        )
        predecessor_source_table = _optional_string(
            raw.get("predecessor_source_table"),
            f"{where}.predecessor_source_table",
        )
        if (
            predecessor_source_table is not None
            and predecessor_source_table not in _CONTINUATION_BOUNDARY_IDENTITY
        ):
            raise OrcParseError(
                f"{where}.predecessor_source_table: unsupported Orc boundary table "
                f"{predecessor_source_table!r}"
            )
        predecessor_at_ms = _nonnegative_integer(
            raw.get("predecessor_at_ms"), f"{where}.predecessor_at_ms"
        )
        started_at_ms = _nonnegative_integer(
            raw.get("started_at_ms"), f"{where}.started_at_ms"
        )
        gap_ms = _nonnegative_integer(raw.get("gap_ms"), f"{where}.gap_ms")
        if (
            predecessor_session_id == session_id
            or predecessor_source_line == 0
            or predecessor_at_ms == 0
            or started_at_ms == 0
            or gap_ms == 0
            or gap_ms != started_at_ms - predecessor_at_ms
            or (start_message_id is None) != (start_source_line is None)
            or start_source_line == 0
        ):
            raise OrcParseError(f"{where}: malformed Orc continuation record")
        return cls(
            predecessor_session_id=predecessor_session_id,
            session_id=session_id,
            predecessor_source_path=predecessor_source_path,
            predecessor_source_line=predecessor_source_line,
            predecessor_at_ms=predecessor_at_ms,
            source_path=source_path,
            start_message_id=start_message_id,
            start_source_line=start_source_line,
            started_at_ms=started_at_ms,
            gap_ms=gap_ms,
            predecessor_source_table=predecessor_source_table,
        )


@dataclass(frozen=True)
class OrcSnapshotResult:
    """Summary of source copies retained by one snapshot pass."""

    sources: tuple[OrcSourceCopy, ...]
    files_changed: int
    continuations: tuple[OrcContinuationLink, ...] = ()
    # Only the overrides this pass actually accepted, not every override the manifest remembers.
    # Callers use this to decide whether the *current* run must shout; the sticky record on each
    # source answers the different question of whether the archive was ever re-baselined.
    prefix_overrides: tuple[OrcAppendPrefixOverride, ...] = ()


@dataclass(frozen=True)
class _DiscoveryPlan:
    """Validated live/frozen source set and stable task namespaces for one ingest."""

    sources: tuple[_DiscoveredSource, ...]
    previous_by_path: Mapping[str, OrcSourceCopy]
    selected_session_ids: frozenset[str]
    task_ordinals: Mapping[str, int]
    session_task_paths: Mapping[str, frozenset[str]]


@dataclass(frozen=True)
class _PreparedCandidate:
    """One consistent live/frozen backup plus its validated prior state."""

    discovered: _DiscoveredSource
    previous: OrcSourceCopy | None
    owner_session_id: str
    source_path: Path
    previous_path: Path | None
    temporary_path: Path
    state: _LogicalState
    previous_state: _LogicalState | None
    prefix_override: OrcAppendPrefixOverride | None


@dataclass(frozen=True)
class _SourceAdvance:
    """Provider-specific semantic/projection result for a prepared source."""

    auxiliary: OrcAuxiliaryStatus
    task_projection: OrcTaskProjection | None
    task_source_ordinal: int | None
    previous_identity: _SemanticIdentity | None
    current_identity: _SemanticIdentity
    staged_objects: tuple[tuple[Path, Path, str], ...]
    temporary_paths: tuple[Path, ...]


@dataclass(frozen=True)
class _SessionMeta:
    session_id: str
    parent_id: str | None
    name: str
    db_name: str | None
    created_at_ms: int
    updated_at_ms: int
    source_path: str


@dataclass(frozen=True)
class _LogicalState:
    append_count: int
    append_max_id: int
    append_prefix_sha256: str


@dataclass(frozen=True)
class _SessionGeometry:
    """Row counts and watermarks for the tables a session's append prefix is digested over."""

    storage_table: str
    content_count: int
    content_max_id: int
    message_count: int
    message_max_id: int


@dataclass(frozen=True)
class _PrefixScope:
    """One guarded table, the columns the digest covers in order, and the watermark applied."""

    table: str
    columns: tuple[str, ...]
    limit: int

    @property
    def query(self) -> str:
        """Return the single statement both the digest and the row diff read rows through."""

        key = self.columns[0]
        return (
            f"SELECT {', '.join(self.columns)} FROM {self.table} "
            f"WHERE {key} <= ? ORDER BY {key}"
        )


@dataclass(frozen=True)
class _DiscoveredSource:
    """One live Orc database and every session that currently references it."""

    source_path: str
    kind: str
    owner_candidates: tuple[str, ...]
    source_state: str = _SOURCE_STATE_LIVE
    lineage_root_session_id: str | None = None


@dataclass(frozen=True)
class _StableSpawn:
    """The immutable projection of one AgentBlock needed by normalization."""

    session_id: str
    parent_session_id: str | None
    message_id: int
    block_id: int
    timestamp_ms: int
    agent_id: str

    @property
    def key(self) -> tuple[str, int]:
        """Return the stable identity used for monotonic subset validation."""

        return (self.session_id, self.block_id)


@dataclass(frozen=True)
class _AuxiliaryObservation:
    """One bounded observation of mutable conversation history."""

    messages: tuple[object, ...]
    message_sha256: str
    stable_spawns: tuple[_StableSpawn, ...]
    stable_spawn_sha256: str


@dataclass(frozen=True)
class _ModernBlockOrigin:
    source_line: int
    native_message_id: str


@dataclass(frozen=True)
class _TaskNoteEnrichment:
    """Frozen normalization metadata for one append-guarded task note."""

    note_id: int
    task_id: str
    server_author: str | None
    task_owner: str | None
    title: str


@dataclass(frozen=True)
class _TaskEnrichmentObservation:
    """Current task-note enrichment derived from a task database."""

    records: tuple[_TaskNoteEnrichment, ...]
    sha256: str


@dataclass(frozen=True)
class _FrozenTaskNote:
    note_id: int
    task_id: str
    content: str
    created_at: str
    server_author: str | None
    task_owner: str | None
    title: str

    @property
    def enrichment(self) -> _TaskNoteEnrichment:
        return _TaskNoteEnrichment(
            self.note_id,
            self.task_id,
            self.server_author,
            self.task_owner,
            self.title,
        )


@dataclass(frozen=True)
class _ObservedTaskNote:
    note_id: int
    task_id: str
    content: str
    created_at: str
    server_author: str | None
    task_owner: str | None
    title: str | None


@dataclass(frozen=True)
class _SemanticIdentity:
    """Deterministic cache identity for one normalized Orc source state."""

    sha256: str
    complete_bytes: int


@dataclass(frozen=True)
class _Spawn:
    thread_id: str
    parent_thread_id: str
    official_name: str
    timestamp_ms: int
    source_line: int
    source_path: str


_CLASSIFICATION_VERSION = "authorship-v2"
_ORC_PERIODIC_REMINDER = (
    "This is your periodic reminder to make sure your running state is aligned "
    "with your overarching goals"
)
_ORC_TIME_ORIENTATION = re.compile(
    r"\AWe're working in eastern time and the current date/time is "
    r"`\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} (?:EDT|EST)`, use that to orient "
)


def _required_string(value: object, where: str) -> str:
    if not isinstance(value, str) or not value:
        raise OrcParseError(f"{where}: expected a non-empty string")
    return value


def _optional_string(value: object, where: str) -> str | None:
    if value is None or value == "":
        return None
    if not isinstance(value, str):
        raise OrcParseError(f"{where}: expected a string or null")
    return value


def _string_value(value: object, where: str) -> str:
    if not isinstance(value, str):
        raise OrcParseError(f"{where}: expected a string")
    return value


def _boolean(value: object, where: str) -> bool:
    if not isinstance(value, bool):
        raise OrcParseError(f"{where}: expected a boolean")
    return value


def _nonnegative_integer(value: object, where: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise OrcParseError(f"{where}: expected a non-negative integer")
    return value


def _integer(value: object, where: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise OrcParseError(f"{where}: expected an integer")
    return value


def _positive_integer(value: object, where: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise OrcParseError(f"{where}: expected a positive integer")
    return value


def _sha256_string(value: object, where: str) -> str:
    text = _required_string(value, where)
    if re.fullmatch(r"[0-9a-f]{64}", text) is None:
        raise OrcParseError(f"{where}: expected a SHA-256 digest")
    return text


def _mapping(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise OrcParseError(f"{where}: expected an object")
    return {str(key): item for key, item in value.items()}


def _optional_json_mapping(value: object, where: str) -> dict[str, object]:
    """Decode one optional JSON object stored in an Orc SQLite text column."""

    text = _optional_string(value, where)
    if text is None:
        return {}
    try:
        decoded: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise OrcParseError(f"{where}: invalid JSON ({error})") from error
    return _mapping(decoded, where)


def _modern_source_mapping(value: object, where: str) -> dict[str, object]:
    """Decode structured modern message provenance; ignore named system sources."""

    if value is None or value == "":
        return {}
    if not isinstance(value, str):
        raise OrcParseError(f"{where}: expected a string or null")
    if not value.startswith("{"):
        return {}
    return _optional_json_mapping(value, where)


def _nested_mapping(raw: Mapping[str, object], key: str) -> dict[str, object]:
    value = raw.get(key)
    return _mapping(value, key) if isinstance(value, dict) else {}


def _first_string(*values: object) -> str | None:
    for value in values:
        if isinstance(value, str) and value:
            return value
    return None


def _is_scheduled_orc_input(text: str) -> bool:
    """Recognize only two exact scheduled-message families seen in Orc logs."""

    return text.startswith(_ORC_PERIODIC_REMINDER) or bool(
        _ORC_TIME_ORIENTATION.match(text)
    )


def _orc_input_provenance(
    user_source: Mapping[str, object],
    extra: Mapping[str, object],
    text: str,
    message_id: str,
    owner_gchat_senders: frozenset[str] = frozenset(),
) -> tuple[str, str | None, str, str, str]:
    """Return event kind, author, ingress, author kind, and native identity."""

    orc = _nested_mapping(user_source, "Orc")
    if "Orc" in user_source:
        author = _first_string(orc.get("sender_session"), extra.get("sender_session"))
        return "inter_agent_message", author, "orc", "agent", message_id

    gchat = _nested_mapping(user_source, "GChat")
    if "GChat" in user_source:
        explicit_owner = gchat.get("is_owner")
        if not isinstance(explicit_owner, bool):
            explicit_owner = extra.get("is_owner")
        author = _first_string(
            gchat.get("sender_unixname"),
            extra.get("sender_unixname"),
            gchat.get("sender_display_name"),
            extra.get("sender_display_name"),
            gchat.get("sender_name"),
            extra.get("sender_name"),
        )
        if not isinstance(explicit_owner, bool) and author is not None:
            if author in owner_gchat_senders:
                explicit_owner = True
            elif owner_gchat_senders:
                explicit_owner = False
        author_kind = "owner_human" if explicit_owner is True else "unknown"
        native_id = _first_string(
            gchat.get("message_name"), extra.get("message_name"), message_id
        )
        if native_id is None:
            raise OrcParseError("GChat input lacks a native message identity")
        event_kind = "external_message" if explicit_owner is False else "user_prompt"
        if explicit_owner is False:
            author_kind = "other_human"
        return event_kind, author, "gchat", author_kind, native_id

    submitted = _nested_mapping(user_source, "Submitted")
    submitted_source = submitted.get("source")
    is_tui = submitted_source == "Tui"
    is_web = isinstance(submitted_source, dict) and "Web" in submitted_source
    if (is_tui or is_web) and _is_scheduled_orc_input(text):
        return "system_input", "system", "scheduled", "system", message_id
    if is_tui:
        return "user_prompt", None, "tui", "unknown", message_id
    if is_web:
        return (
            "user_prompt",
            None,
            "submitted_web",
            "external_or_unknown",
            message_id,
        )
    return "user_prompt", None, "orc_unknown", "unknown", message_id


def _require_exact_keys(
    raw: Mapping[str, object], expected: set[str], where: str
) -> None:
    actual = set(raw)
    if actual == expected:
        return
    details: list[str] = []
    missing = sorted(expected - actual)
    unknown = sorted(actual - expected)
    if missing:
        details.append("missing " + ", ".join(missing))
    if unknown:
        details.append("unknown " + ", ".join(unknown))
    raise OrcParseError(f"{where}: invalid fields ({'; '.join(details)})")


def _array(value: object, where: str) -> list[object]:
    if not isinstance(value, list):
        raise OrcParseError(f"{where}: expected an array")
    return list(value)


def _row(value: object, where: str) -> tuple[object, ...]:
    if not isinstance(value, tuple):
        raise OrcParseError(f"{where}: SQLite returned an invalid row")
    return tuple(value)


def _one(connection: sqlite3.Connection, sql: str, where: str) -> tuple[object, ...]:
    raw: object = connection.execute(sql).fetchone()
    if raw is None:
        raise OrcParseError(f"{where}: expected one row")
    return _row(raw, where)


def _iso_ms(value: object, where: str) -> int:
    text = _required_string(value, where)
    normalized = text[:-1] + "+00:00" if text.endswith("Z") else text
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError as error:
        raise OrcParseError(f"{where}: invalid ISO timestamp {text!r}") from error
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return int(parsed.timestamp() * 1000)


def _safe_component(value: str, where: str) -> str:
    if len(value) > 255 or _SAFE_COMPONENT.fullmatch(value) is None:
        raise OrcParseError(f"{where}: unsafe path component {value!r}")
    return value


def _safe_relative(value: str) -> PurePosixPath:
    relative = PurePosixPath(value)
    if (
        relative.is_absolute()
        or not relative.parts
        or any(part in ("", ".", "..") for part in relative.parts)
    ):
        raise OrcParseError(f"unsafe Orc snapshot path {value!r}")
    return relative


def _snapshot_path(snapshot_root: Path, relative: str) -> Path:
    safe = _safe_relative(relative)
    current = snapshot_root
    for index, part in enumerate(safe.parts):
        try:
            mode = current.lstat().st_mode
        except FileNotFoundError:
            mode = None
        if mode is not None and (
            stat.S_ISLNK(mode)
            or (index > 0 and not stat.S_ISDIR(mode))
            or (index == 0 and not stat.S_ISDIR(mode))
        ):
            raise OrcParseError(
                f"snapshot path component is a symlink or non-directory: {current}"
            )
        current = current / part
    return current


def _live_source_path(source_root: Path, relative: str) -> Path:
    """Resolve a source-relative path without following intermediate symlinks."""

    safe = _safe_relative(relative)
    current = source_root
    for index, part in enumerate(safe.parts):
        current = current / part
        try:
            mode = current.lstat().st_mode
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(mode):
            raise OrcParseError(f"symlink in Orc live source path: {current}")
        if index < len(safe.parts) - 1 and not stat.S_ISDIR(mode):
            raise OrcParseError(f"non-directory in Orc live source path: {current}")
    return current


def _snapshot_object_relative(sha256: str) -> str:
    _sha256_string(sha256, "snapshot object digest")
    return f"{_SNAPSHOT_OBJECT_ROOT}/{sha256[:2]}/{sha256}.db"


def _task_projection_relative(sha256: str) -> str:
    _sha256_string(sha256, "task projection digest")
    return f"{_TASK_PROJECTION_ROOT}/{sha256[:2]}/{sha256}.json"


def _read_only(path: Path) -> sqlite3.Connection:
    if path.is_symlink() or not path.is_file():
        raise OrcParseError(f"Orc SQLite source is missing or not a regular file: {path}")
    try:
        resolved = path.resolve(strict=True)
        immutable_object = (
            resolved.parent.parent.name == _SNAPSHOT_OBJECT_ROOT
            and re.fullmatch(r"[0-9a-f]{2}", resolved.parent.name) is not None
            and re.fullmatch(r"[0-9a-f]{64}\.db", resolved.name) is not None
        )
        query = "?mode=ro&immutable=1" if immutable_object else "?mode=ro"
        connection = sqlite3.connect(resolved.as_uri() + query, uri=True)
        connection.execute("PRAGMA query_only = ON")
        return connection
    except sqlite3.Error as error:
        raise OrcParseError(f"cannot open Orc SQLite source read-only at {path}: {error}") from error


def _require_tables(
    connection: sqlite3.Connection, names: Sequence[str], where: str
) -> None:
    found = _table_names(connection, where)
    missing = sorted(set(names) - found)
    if missing:
        raise OrcParseError(f"{where}: missing required tables: {', '.join(missing)}")


def _table_names(connection: sqlite3.Connection, where: str) -> frozenset[str]:
    rows = connection.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table'"
    ).fetchall()
    return frozenset(
        _required_string(_row(raw, where)[0], where) for raw in rows
    )


def _session_storage_table(
    connection: sqlite3.Connection, where: str
) -> str:
    tables = _table_names(connection, where)
    if "messages" in tables:
        return "messages"
    if "conversation_state" in tables:
        return "content_blocks"
    raise OrcParseError(
        f"{where}: missing both modern messages and legacy conversation_state"
    )


def _provider_schema_version(path: Path, where: str) -> int | None:
    """Read what Orc itself says this database's storage schema is.

    Orc keeps a single-row `schema_version` table -- `id = 1`, a positive `version`, and the time it
    was last migrated. Nothing in this module used to select it, which is how a whole family of bugs
    got in: every question about storage layout was answered by looking at which tables exist, and
    that answer is a *current* observation being applied to *historical* records.

    Absent table means ``None`` and nothing else. That is a real, benign state -- Orc predates the
    table -- and it is the only state that reads as "no version". A table that is present but does
    not answer `SELECT version WHERE id = 1` is refused rather than recorded as ``None``, because
    the two would then be indistinguishable and the second one is precisely the signal that Orc's
    storage has changed shape again. Recording it as absent would reproduce, in the field intended
    to prevent it, the silent-misread failure that motivated the field. A refusal here is one line
    to fix once someone reads it; a silent ``None`` is another year of nobody noticing.
    """

    connection = _read_only(path)
    try:
        if "schema_version" not in _table_names(connection, where):
            return None
        raw = connection.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
    except sqlite3.Error as error:
        raise OrcParseError(
            f"{where}: Orc schema_version table is present but unreadable: {error}"
        ) from error
    finally:
        connection.close()
    if raw is None:
        raise OrcParseError(
            f"{where}: Orc schema_version table holds no version row"
        )
    return _positive_integer(
        _row(raw, where)[0], f"{where}: schema_version.version"
    )


def _session_meta(path: Path, source_path: str) -> _SessionMeta:
    connection = _read_only(path)
    try:
        _require_tables(
            connection,
            ("session_meta", "content_blocks"),
            str(path),
        )
        _session_storage_table(connection, str(path))
        row = _one(
            connection,
            "SELECT id, parent_id, name, db_name, created_at, updated_at "
            "FROM session_meta LIMIT 1",
            str(path),
        )
    except sqlite3.Error as error:
        raise OrcParseError(
            f"failed to inspect Orc session metadata at {path}: {error}"
        ) from error
    finally:
        connection.close()
    session_id = _safe_component(_required_string(row[0], f"{path}: id"), f"{path}: id")
    parent_id = _optional_string(row[1], f"{path}: parent_id")
    if parent_id is not None:
        _safe_component(parent_id, f"{path}: parent_id")
    db_name = _optional_string(row[3], f"{path}: db_name")
    if db_name is not None:
        _safe_component(db_name, f"{path}: db_name")
    return _SessionMeta(
        session_id=session_id,
        parent_id=parent_id,
        name=_required_string(row[2], f"{path}: name"),
        db_name=db_name,
        created_at_ms=_iso_ms(row[4], f"{path}: created_at"),
        updated_at_ms=_iso_ms(row[5], f"{path}: updated_at"),
        source_path=source_path,
    )


def _associated_db_names(path: Path) -> tuple[str, ...]:
    connection = _read_only(path)
    try:
        table = connection.execute(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' "
            "AND name = 'associated_dbs'"
        ).fetchone()
        if table is None:
            return ()
        rows = connection.execute(
            "SELECT db_name FROM associated_dbs ORDER BY db_name"
        ).fetchall()
    except sqlite3.Error as error:
        raise OrcParseError(
            f"failed to inspect Orc associated task databases at {path}: {error}"
        ) from error
    finally:
        connection.close()
    names: list[str] = []
    for index, raw in enumerate(rows):
        row = _row(raw, f"{path}: associated_dbs[{index}]")
        names.append(
            _safe_component(
                _required_string(row[0], f"{path}: associated_dbs[{index}].db_name"),
                f"{path}: associated_dbs[{index}].db_name",
            )
        )
    return tuple(names)


def _session_task_relatives(
    database: Path, meta: _SessionMeta, _root_session_id: str
) -> tuple[tuple[str, bool, bool], ...]:
    """Return TaskGraph paths and whether their persisted reference requires a file."""

    if meta.parent_id is None and meta.db_name is not None:
        primary_name = meta.db_name
        primary_required = False
    else:
        primary_name = meta.session_id
        primary_required = False
    required_by_path: dict[str, bool] = {
        f".tg/{primary_name}.db": primary_required
    }
    for name in _associated_db_names(database):
        required_by_path[f".tg/{name}.db"] = False
    primary_path = f".tg/{primary_name}.db"
    return tuple(
        (path, required, path == primary_path)
        for path, required in sorted(required_by_path.items())
    )


def _lineage_roots(
    root_session_id: str, continuation_session_ids: Sequence[str]
) -> tuple[str, ...]:
    roots = tuple(
        _safe_component(item, "Orc root or continuation session id")
        for item in (root_session_id, *continuation_session_ids)
    )
    if len(set(roots)) != len(roots):
        raise OrcParseError("Orc root and continuation session IDs must be unique")
    return roots


def _continuation_specs(
    values: Sequence[str | OrcContinuationSpec],
) -> tuple[OrcContinuationSpec, ...]:
    return tuple(
        OrcContinuationSpec.from_value(value, f"continuation_specs[{index}]")
        for index, value in enumerate(values)
    )


def _resolve_continuation_start(
    path: Path, meta: _SessionMeta, spec: OrcContinuationSpec
) -> tuple[int | None, int]:
    if spec.start_message_id is None:
        return None, meta.created_at_ms
    connection = _read_only(path)
    try:
        if _session_storage_table(connection, str(path)) != "messages":
            raise OrcParseError(
                f"bounded Orc continuation {spec.session_id!r} requires the "
                "append-only messages table"
            )
        matches: list[tuple[int, int]] = []
        for raw in connection.execute(
            "SELECT id, created_at_ms, message_json FROM messages ORDER BY id"
        ):
            row = _row(raw, str(path))
            source_line = _nonnegative_integer(row[0], f"{path}: message id")
            timestamp_ms = _nonnegative_integer(
                row[1], f"{path}: message created_at_ms"
            )
            raw_json = _required_string(row[2], f"{path}: message_json")
            try:
                decoded: object = json.loads(raw_json)
            except json.JSONDecodeError as error:
                raise OrcParseError(
                    f"{path}: invalid message_json at row {source_line}: {error}"
                ) from error
            message = _mapping(decoded, f"{path}: message_json[{source_line}]")
            if message.get("id") == spec.start_message_id:
                matches.append((source_line, timestamp_ms))
        if len(matches) != 1 or matches[0][1] <= 0:
            raise OrcParseError(
                f"bounded Orc continuation {spec.session_id!r} start message "
                f"{spec.start_message_id!r} resolved to {len(matches)} valid rows"
            )
        return matches[0]
    except sqlite3.Error as error:
        raise OrcParseError(
            f"failed to resolve Orc continuation start in {path}: {error}"
        ) from error
    finally:
        connection.close()


def _session_has_activity_at_or_after(
    source_root: Path,
    path: Path,
    meta: _SessionMeta,
    lineage_root_session_id: str,
    start_at_ms: int,
) -> bool:
    if meta.created_at_ms >= start_at_ms:
        return True
    connection = _read_only(path)
    try:
        table = _session_storage_table(connection, str(path))
        maximum = connection.execute(
            f"SELECT MAX(created_at_ms) FROM {table}"
        ).fetchone()
    finally:
        connection.close()
    if maximum is not None:
        row = _row(maximum, str(path))
        value = row[0]
        if isinstance(value, int) and not isinstance(value, bool) and value >= start_at_ms:
            return True
    if any(
        spawn.timestamp_ms >= start_at_ms
        for spawn in _auxiliary_observation(path, meta).stable_spawns
    ):
        return True
    for relative, _, _ in _session_task_relatives(
        path, meta, lineage_root_session_id
    ):
        task_path = _live_source_path(source_root, relative)
        if not task_path.is_file() or task_path.is_symlink():
            continue
        connection = _read_only(task_path)
        try:
            _require_tables(connection, ("task_notes",), str(task_path))
            maximum = connection.execute(
                "SELECT MAX(created_at) FROM task_notes"
            ).fetchone()
        finally:
            connection.close()
        if maximum is None:
            continue
        value = _row(maximum, str(task_path))[0]
        if value is not None and _iso_ms(
            value, f"{task_path}: latest task note created_at"
        ) >= start_at_ms:
            return True
    return False


def _discover_sources(
    source_root: Path, root_session_id: str
) -> tuple[_DiscoveredSource, ...]:
    root_id = _safe_component(root_session_id, "root session id")
    sessions_root = _live_source_path(source_root, ".orc/sessions")
    if not sessions_root.is_dir() or sessions_root.is_symlink():
        raise OrcParseError(f"missing Orc sessions directory: {sessions_root}")
    index_path = _live_source_path(source_root, ".orc/index.db")
    selected: set[str] | None = None
    index_parents: dict[str, str | None] | None = None
    if index_path.is_file() and not index_path.is_symlink():
        connection = _read_only(index_path)
        try:
            _require_tables(connection, ("sessions",), str(index_path))
            rows = connection.execute(
                "SELECT id, parent_id FROM sessions ORDER BY id"
            ).fetchall()
        except sqlite3.Error as error:
            raise OrcParseError(
                f"failed to inspect Orc session index at {index_path}: {error}"
            ) from error
        finally:
            connection.close()
        parents: dict[str, str | None] = {}
        for index, raw in enumerate(rows):
            row = _row(raw, f"{index_path}: sessions[{index}]")
            session_id = _safe_component(
                _required_string(row[0], f"{index_path}: sessions[{index}].id"),
                f"{index_path}: sessions[{index}].id",
            )
            parent = _optional_string(
                row[1], f"{index_path}: sessions[{index}].parent_id"
            )
            parents[session_id] = parent
        if root_id not in parents:
            raise OrcParseError(f"root Orc session {root_id!r} is absent from {index_path}")
        selected = {root_id}
        index_parents = parents
        changed = True
        while changed:
            changed = False
            for session_id, parent in parents.items():
                if parent in selected and session_id not in selected:
                    selected.add(session_id)
                    changed = True

    metas: dict[str, _SessionMeta] = {}
    candidate_names = sorted(selected) if selected is not None else [root_id]
    for session_name in candidate_names:
        session_dir = sessions_root / session_name
        if session_dir.is_symlink() or not session_dir.is_dir():
            if selected is not None:
                raise OrcParseError(f"selected Orc session directory is missing: {session_dir}")
            continue
        database = _live_source_path(
            source_root, f".orc/sessions/{session_name}/session.db"
        )
        if not database.is_file() or database.is_symlink():
            if selected is not None or session_name == root_id:
                raise OrcParseError(f"selected Orc session database is missing: {database}")
            continue
        relative = database.relative_to(source_root).as_posix()
        try:
            meta = _session_meta(database, relative)
        except OrcParseError:
            if selected is not None or session_name == root_id:
                raise
            continue
        if meta.session_id != session_name:
            raise OrcParseError(
                f"Orc session directory {session_name!r} contains session {meta.session_id!r}"
            )
        if index_parents is not None and meta.parent_id != index_parents[session_name]:
            raise OrcParseError(
                f"Orc session parent differs between index and session DB for "
                f"{session_name!r}"
            )
        metas[meta.session_id] = meta
    if root_id not in metas:
        raise OrcParseError(f"root Orc session {root_id!r} was not found under {sessions_root}")
    if selected is None:
        selected = {root_id}

    result: list[_DiscoveredSource] = []
    task_owner_evidence: dict[str, list[tuple[str, bool]]] = defaultdict(list)
    for session_id in sorted(selected):
        meta = metas[session_id]
        result.append(_DiscoveredSource(meta.source_path, "session", (session_id,)))
        session_database = _live_source_path(source_root, meta.source_path)
        for task_relative, required, is_primary in _session_task_relatives(
            session_database, meta, root_id
        ):
            task_path = _live_source_path(source_root, task_relative)
            if not task_path.is_file() or task_path.is_symlink():
                if required:
                    raise OrcParseError(
                        f"session {session_id!r} references task database "
                        f"{task_relative!r}, but {task_path} is missing"
                    )
                continue
            task_owner_evidence[task_relative].append((session_id, is_primary))

    def session_depth(session_id: str) -> int:
        depth = 0
        seen = {session_id}
        parent = metas[session_id].parent_id
        while parent is not None and parent in selected and parent not in seen:
            seen.add(parent)
            depth += 1
            parent = metas[parent].parent_id
        return depth

    result.extend(
        _DiscoveredSource(
            path,
            "task",
            tuple(
                session_id
                for session_id, _ in sorted(
                    evidence,
                    key=lambda item: (
                        not item[1],
                        metas[item[0]].parent_id is not None,
                        session_depth(item[0]),
                        metas[item[0]].created_at_ms,
                        item[0],
                    ),
                )
            ),
        )
        for path, evidence in sorted(task_owner_evidence.items())
    )
    return tuple(sorted(result, key=lambda source: source.source_path))


def _discover_continuation_sources(
    source_root: Path,
    root_session_id: str,
    continuation_specs: Sequence[OrcContinuationSpec],
    previously_selected_session_ids: frozenset[str] = frozenset(),
) -> tuple[_DiscoveredSource, ...]:
    """Discover disjoint session lineages while coalescing shared TaskGraph DBs."""

    merged: dict[str, _DiscoveredSource] = {}
    root_start: int | None = None
    selected_sessions: set[str] = set()
    specs = (OrcContinuationSpec(root_session_id), *continuation_specs)
    for spec in specs:
        root = spec.session_id
        root_relative = f".orc/sessions/{root}/session.db"
        root_path = _live_source_path(source_root, root_relative)
        root_meta = _session_meta(root_path, root_relative)
        if root_meta.session_id != root or root_meta.parent_id is not None:
            raise OrcParseError(
                f"configured Orc continuation {root!r} is not a parentless root session"
            )
        _, effective_start = _resolve_continuation_start(
            root_path, root_meta, spec
        )
        if root_start is not None and effective_start <= root_start:
            raise OrcParseError(
                "Orc root and continuation sessions must be ordered by strictly "
                "increasing start time"
            )
        root_start = effective_start
        lineage_sources = _discover_sources(source_root, root)
        if spec.start_message_id is not None:
            retained_sessions = {root}
            allowed_task_paths = {
                path
                for path, _, is_primary in _session_task_relatives(
                    root_path, root_meta, root
                )
                if is_primary
            }
            for session_source in lineage_sources:
                if session_source.kind != "session":
                    continue
                session_path = _live_source_path(
                    source_root, session_source.source_path
                )
                session_meta = _session_meta(
                    session_path, session_source.source_path
                )
                if session_meta.session_id == root:
                    continue
                if (
                    session_meta.session_id not in previously_selected_session_ids
                    and not _session_has_activity_at_or_after(
                        source_root,
                        session_path,
                        session_meta,
                        root,
                        effective_start,
                    )
                ):
                    continue
                retained_sessions.add(session_meta.session_id)
                allowed_task_paths.update(
                    path
                    for path, _, _ in _session_task_relatives(
                        session_path, session_meta, root
                    )
                )
            lineage_sources = tuple(
                source
                for source in lineage_sources
                if (
                    source.kind == "session"
                    and source.owner_candidates[0] in retained_sessions
                )
                or source.source_path in allowed_task_paths
            )
        for source in lineage_sources:
            if source.kind == "session":
                source = replace(source, lineage_root_session_id=root)
            prior = merged.get(source.source_path)
            if source.kind == "session":
                session_id = source.owner_candidates[0]
                if session_id in selected_sessions or prior is not None:
                    raise OrcParseError(
                        "configured Orc root lineages overlap at session "
                        f"{session_id!r}"
                    )
                selected_sessions.add(session_id)
                merged[source.source_path] = source
                continue
            if prior is None:
                merged[source.source_path] = source
                continue
            if prior.kind != "task":
                raise OrcParseError(
                    f"Orc source path has conflicting database kinds: {source.source_path}"
                )
            owners = tuple(
                dict.fromkeys((*prior.owner_candidates, *source.owner_candidates))
            )
            merged[source.source_path] = replace(prior, owner_candidates=owners)
    return tuple(sorted(merged.values(), key=lambda source: source.source_path))


def _update_digest(digest: hashlib._Hash, value: object) -> None:
    if value is None:
        digest.update(b"N")
        return
    if isinstance(value, bool):
        digest.update(b"I1" if value else b"I0")
        return
    if isinstance(value, int):
        payload = str(value).encode("ascii")
        prefix = b"I"
    elif isinstance(value, float):
        payload = value.hex().encode("ascii")
        prefix = b"F"
    elif isinstance(value, str):
        payload = value.encode("utf-8")
        prefix = b"S"
    elif isinstance(value, bytes):
        payload = value
        prefix = b"B"
    else:
        raise OrcParseError(f"unsupported SQLite value type {type(value).__name__}")
    digest.update(prefix)
    digest.update(len(payload).to_bytes(8, "big"))
    digest.update(payload)


def _query_digest(
    connection: sqlite3.Connection, sql: str, parameters: tuple[object, ...] = ()
) -> tuple[int, str]:
    digest = hashlib.sha256()
    count = 0
    for raw in connection.execute(sql, parameters):
        row = _row(raw, "SQLite digest query")
        digest.update(b"R")
        for value in row:
            _update_digest(digest, value)
        count += 1
    return count, digest.hexdigest()


def _conversation_messages(connection: sqlite3.Connection) -> list[object]:
    if _session_storage_table(connection, "Orc session") == "messages":
        result: list[object] = []
        for index, raw_row in enumerate(
            connection.execute(
                "SELECT id, created_at_ms, message_json FROM messages ORDER BY id"
            )
        ):
            row = _row(raw_row, f"messages[{index}]")
            message_id = _nonnegative_integer(row[0], f"messages[{index}].id")
            created_at_ms = _nonnegative_integer(
                row[1], f"messages[{index}].created_at_ms"
            )
            raw = _required_string(
                row[2], f"messages[{index}].message_json"
            )
            try:
                parsed_message: object = json.loads(raw)
            except json.JSONDecodeError as error:
                raise OrcParseError(
                    f"invalid messages.message_json at row {message_id}: {error}"
                ) from error
            message = _mapping(
                parsed_message, f"messages[{index}].message_json"
            )
            embedded_at = _nonnegative_integer(
                message.get("created_at_ms"),
                f"messages[{index}].message_json.created_at_ms",
            )
            if embedded_at != created_at_ms:
                raise OrcParseError(
                    f"messages[{index}] timestamp differs from message_json"
                )
            normalized = dict(message)
            normalized["id"] = message_id
            normalized["created_at_ms"] = created_at_ms
            result.append(normalized)
        return result
    row = _one(
        connection,
        "SELECT conversation_json FROM conversation_state WHERE id = 1",
        "conversation_state",
    )
    raw = _required_string(row[0], "conversation_state.conversation_json")
    try:
        parsed: object = json.loads(raw)
    except json.JSONDecodeError as error:
        raise OrcParseError(f"invalid conversation_state JSON: {error}") from error
    root = _mapping(parsed, "conversation_state")
    return _array(root.get("messages"), "conversation_state.messages")


def _messages_digest(messages: Sequence[object]) -> str:
    try:
        encoded = json.dumps(
            list(messages),
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise OrcParseError(f"conversation state is not canonical JSON: {error}") from error
    return hashlib.sha256(encoded).hexdigest()


def _stable_spawn_records(
    messages: Sequence[object], meta: _SessionMeta, where: str
) -> tuple[_StableSpawn, ...]:
    records: list[_StableSpawn] = []
    seen_keys: set[tuple[str, int]] = set()
    for message_index, raw_message in enumerate(messages):
        message_where = f"{where}: messages[{message_index}]"
        message = _mapping(raw_message, message_where)
        message_id = _integer(message.get("id"), f"{message_where}.id")
        timestamp_ms = _integer(
            message.get("created_at_ms"), f"{message_where}.created_at_ms"
        )
        blocks = _array(message.get("blocks"), f"{message_where}.blocks")
        for block_index, raw_block in enumerate(blocks):
            block_where = f"{message_where}.blocks[{block_index}]"
            block = _mapping(raw_block, block_where)
            if block.get("type") != "AgentBlock":
                continue
            block_id = _integer(block.get("id"), f"{block_where}.id")
            record = _StableSpawn(
                session_id=meta.session_id,
                parent_session_id=meta.parent_id,
                message_id=message_id,
                block_id=block_id,
                timestamp_ms=timestamp_ms,
                agent_id=_required_string(
                    block.get("agent_id"), f"{block_where}.agent_id"
                ),
            )
            if record.key in seen_keys:
                raise OrcParseError(
                    f"{where}: duplicate AgentBlock identity {record.key!r}"
                )
            seen_keys.add(record.key)
            records.append(record)
    return tuple(sorted(records, key=lambda record: record.key))


def _stable_spawn_digest(records: Sequence[_StableSpawn]) -> str:
    digest = hashlib.sha256()
    for record in records:
        digest.update(b"R")
        for value in (
            record.session_id,
            record.parent_session_id,
            record.message_id,
            record.block_id,
            record.timestamp_ms,
            record.agent_id,
        ):
            _update_digest(digest, value)
    return digest.hexdigest()


def _auxiliary_observation(
    path: Path, meta: _SessionMeta
) -> _AuxiliaryObservation:
    connection = _read_only(path)
    try:
        messages = tuple(_conversation_messages(connection))
    finally:
        connection.close()
    records = _stable_spawn_records(messages, meta, str(path))
    return _AuxiliaryObservation(
        messages=messages,
        message_sha256=_messages_digest(messages),
        stable_spawns=records,
        stable_spawn_sha256=_stable_spawn_digest(records),
    )


def _conversation_is_append_extension(
    previous: _AuxiliaryObservation, current: _AuxiliaryObservation
) -> bool:
    previous_count = len(previous.messages)
    return len(current.messages) >= previous_count and _messages_digest(
        current.messages[:previous_count]
    ) == previous.message_sha256


def _validate_stable_spawn_extension(
    previous: _AuxiliaryObservation,
    current: _AuxiliaryObservation,
    relative: str,
) -> None:
    previous_by_key = {record.key: record for record in previous.stable_spawns}
    current_by_key = {record.key: record for record in current.stable_spawns}
    missing = sorted(set(previous_by_key) - set(current_by_key))
    if missing:
        raise OrcParseError(
            f"Orc stable spawn evidence disappeared for {relative}: {missing[0]!r}"
        )
    changed = sorted(
        key
        for key, record in previous_by_key.items()
        if current_by_key[key] != record
    )
    if changed:
        raise OrcParseError(
            f"Orc stable spawn evidence was rewritten for {relative}: {changed[0]!r}"
        )


def _validate_stable_spawn_storage_migration(
    previous: _AuxiliaryObservation,
    current: _AuxiliaryObservation,
    relative: str,
) -> None:
    """Allow only the message-row renumbering caused by Orc's schema-v5 migration."""

    previous_by_key = {record.key: record for record in previous.stable_spawns}
    current_by_key = {record.key: record for record in current.stable_spawns}
    missing = sorted(set(previous_by_key) - set(current_by_key))
    if missing:
        raise OrcParseError(
            f"Orc stable spawn evidence disappeared during storage migration for "
            f"{relative}: {missing[0]!r}"
        )
    for key, prior in previous_by_key.items():
        current_record = current_by_key[key]
        if (
            prior.session_id,
            prior.parent_session_id,
            prior.block_id,
            prior.timestamp_ms,
            prior.agent_id,
        ) != (
            current_record.session_id,
            current_record.parent_session_id,
            current_record.block_id,
            current_record.timestamp_ms,
            current_record.agent_id,
        ):
            raise OrcParseError(
                f"Orc stable spawn evidence was rewritten during storage migration "
                f"for {relative}: {key!r}"
            )


def _validate_session_meta_extension(
    previous: _SessionMeta, current: _SessionMeta, relative: str
) -> None:
    immutable_previous = (
        previous.session_id,
        previous.parent_id,
        previous.created_at_ms,
    )
    immutable_current = (
        current.session_id,
        current.parent_id,
        current.created_at_ms,
    )
    if immutable_current != immutable_previous:
        raise OrcParseError(
            f"Orc immutable session metadata was rewritten for {relative}"
        )
    if current.updated_at_ms < previous.updated_at_ms:
        raise OrcParseError(
            f"Orc session updated_at moved backwards for {relative}: "
            f"{previous.updated_at_ms} to {current.updated_at_ms}"
        )


def _task_projection_text(records: Sequence[_TaskNoteEnrichment]) -> str:
    value = {
        "schema_version": 1,
        "records": [
            {
                "note_id": record.note_id,
                "task_id": record.task_id,
                "server_author": record.server_author,
                "task_owner": record.task_owner,
                "title": record.title,
            }
            for record in records
        ],
    }
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ) + "\n"


def _frozen_task_projection_text(records: Sequence[_FrozenTaskNote]) -> str:
    value = {
        "schema_version": 2,
        "records": [
            {
                "note_id": record.note_id,
                "task_id": record.task_id,
                "content": record.content,
                "created_at": record.created_at,
                "server_author": record.server_author,
                "task_owner": record.task_owner,
                "title": record.title,
            }
            for record in records
        ],
    }
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ) + "\n"


def _task_enrichment_observation(path: Path) -> _TaskEnrichmentObservation:
    connection = _read_only(path)
    try:
        rows = connection.execute(
            "SELECT n.id, n.task_id, n.author_unixname, t.owner, t.title "
            "FROM task_notes n LEFT JOIN tasks t ON t.local_id = n.task_id "
            "ORDER BY n.id"
        ).fetchall()
    except sqlite3.Error as error:
        raise OrcParseError(f"failed to inspect Orc tasks at {path}: {error}") from error
    finally:
        connection.close()
    records: list[_TaskNoteEnrichment] = []
    seen: set[int] = set()
    for raw in rows:
        row = _row(raw, str(path))
        note_id = _nonnegative_integer(row[0], f"{path}: note id")
        author = _optional_string(row[2], f"{path}: note author")
        task_owner = _optional_string(row[3], f"{path}: task owner")
        record = _TaskNoteEnrichment(
            note_id=note_id,
            task_id=_required_string(row[1], f"{path}: note task_id"),
            server_author=author,
            task_owner=task_owner,
            title=_required_string(row[4], f"{path}: task title"),
        )
        if note_id in seen:
            raise OrcParseError(f"{path}: duplicate task note id {note_id}")
        seen.add(note_id)
        records.append(record)
    frozen = tuple(records)
    digest = hashlib.sha256(_task_projection_text(frozen).encode("utf-8")).hexdigest()
    return _TaskEnrichmentObservation(frozen, digest)


def _observed_task_notes(path: Path) -> tuple[_ObservedTaskNote, ...]:
    connection = _read_only(path)
    try:
        rows = connection.execute(
            "SELECT n.id, n.task_id, n.content, n.created_at, n.author_unixname, "
            "t.owner, t.title FROM task_notes n LEFT JOIN tasks t "
            "ON t.local_id = n.task_id ORDER BY n.id"
        ).fetchall()
    except sqlite3.Error as error:
        raise OrcParseError(f"failed to inspect Orc tasks at {path}: {error}") from error
    finally:
        connection.close()
    records = tuple(
        _ObservedTaskNote(
            _nonnegative_integer(row[0], f"{path}: note id"),
            _required_string(row[1], f"{path}: note task_id"),
            _string_value(row[2], f"{path}: note content"),
            _required_string(row[3], f"{path}: note created_at"),
            _optional_string(row[4], f"{path}: note author"),
            _optional_string(row[5], f"{path}: task owner"),
            _optional_string(row[6], f"{path}: task title"),
        )
        for raw in rows
        for row in [_row(raw, str(path))]
    )
    if len({record.note_id for record in records}) != len(records):
        raise OrcParseError(f"{path}: duplicate task note id")
    return records


def _task_note_sequence(path: Path) -> int:
    """Return Orc's durable task-note allocation high-water mark."""

    connection = _read_only(path)
    try:
        tables = _table_names(connection, str(path))
        if "sqlite_sequence" in tables:
            raw = connection.execute(
                "SELECT seq FROM sqlite_sequence WHERE name = 'task_notes'"
            ).fetchone()
            if raw is not None:
                return _nonnegative_integer(
                    _row(raw, str(path))[0], f"{path}: task note sequence"
                )
        raw = _one(
            connection,
            "SELECT COALESCE(MAX(id), 0) FROM task_notes",
            str(path),
        )
        return _nonnegative_integer(raw[0], f"{path}: task note highwater")
    except sqlite3.Error as error:
        raise OrcParseError(
            f"failed to inspect Orc task-note sequence at {path}: {error}"
        ) from error
    finally:
        connection.close()


def _load_legacy_task_projection(
    snapshot_root: Path, status: OrcTaskProjection
) -> tuple[_TaskNoteEnrichment, ...]:
    path = _snapshot_path(snapshot_root, status.path)
    if path.is_symlink() or not path.is_file():
        raise OrcParseError(f"task projection is missing or unsafe: {path}")
    raw_text = path.read_text(encoding="utf-8")
    if hashlib.sha256(raw_text.encode("utf-8")).hexdigest() != status.sha256:
        raise OrcParseError(f"task projection hash mismatch: {path}")
    try:
        parsed: object = json.loads(raw_text)
    except json.JSONDecodeError as error:
        raise OrcParseError(f"invalid task projection JSON at {path}: {error}") from error
    root = _mapping(parsed, str(path))
    _require_exact_keys(root, {"schema_version", "records"}, str(path))
    if root.get("schema_version") != 1:
        raise OrcParseError(f"unsupported task projection schema at {path}")
    records: list[_TaskNoteEnrichment] = []
    prior_id = -1
    for index, raw_record in enumerate(_array(root.get("records"), f"{path}.records")):
        record_obj = _mapping(raw_record, f"{path}.records[{index}]")
        _require_exact_keys(
            record_obj,
            {"note_id", "task_id", "server_author", "task_owner", "title"},
            f"{path}.records[{index}]",
        )
        record = _TaskNoteEnrichment(
            note_id=_nonnegative_integer(
                record_obj.get("note_id"), f"{path}.records[{index}].note_id"
            ),
            task_id=_required_string(
                record_obj.get("task_id"), f"{path}.records[{index}].task_id"
            ),
            server_author=_optional_string(
                record_obj.get("server_author"),
                f"{path}.records[{index}].server_author",
            ),
            task_owner=_optional_string(
                record_obj.get("task_owner"),
                f"{path}.records[{index}].task_owner",
            ),
            title=_required_string(
                record_obj.get("title"), f"{path}.records[{index}].title"
            ),
        )
        if record.note_id <= prior_id:
            raise OrcParseError(f"task projection note IDs are not strictly ordered: {path}")
        prior_id = record.note_id
        records.append(record)
    frozen = tuple(records)
    if len(frozen) != status.note_count:
        raise OrcParseError(f"task projection count mismatch: {path}")
    if _task_projection_text(frozen) != raw_text:
        raise OrcParseError(f"task projection is not canonical: {path}")
    return frozen


def _load_frozen_task_projection(
    snapshot_root: Path, status: OrcTaskProjection
) -> tuple[_FrozenTaskNote, ...]:
    path = _snapshot_path(snapshot_root, status.path)
    if path.is_symlink() or not path.is_file():
        raise OrcParseError(f"task projection is missing or unsafe: {path}")
    raw_text = path.read_text(encoding="utf-8")
    if hashlib.sha256(raw_text.encode("utf-8")).hexdigest() != status.sha256:
        raise OrcParseError(f"task projection hash mismatch: {path}")
    try:
        parsed: object = json.loads(raw_text)
    except json.JSONDecodeError as error:
        raise OrcParseError(f"invalid task projection JSON at {path}: {error}") from error
    root = _mapping(parsed, str(path))
    _require_exact_keys(root, {"schema_version", "records"}, str(path))
    if root.get("schema_version") != 2:
        raise OrcParseError(f"unsupported frozen task projection schema at {path}")
    records: list[_FrozenTaskNote] = []
    prior_id = -1
    for index, raw_record in enumerate(_array(root.get("records"), f"{path}.records")):
        obj = _mapping(raw_record, f"{path}.records[{index}]")
        _require_exact_keys(
            obj,
            {
                "note_id",
                "task_id",
                "content",
                "created_at",
                "server_author",
                "task_owner",
                "title",
            },
            f"{path}.records[{index}]",
        )
        record = _FrozenTaskNote(
            _nonnegative_integer(obj.get("note_id"), f"{path}.records[{index}].note_id"),
            _required_string(obj.get("task_id"), f"{path}.records[{index}].task_id"),
            _string_value(obj.get("content"), f"{path}.records[{index}].content"),
            _required_string(obj.get("created_at"), f"{path}.records[{index}].created_at"),
            _optional_string(obj.get("server_author"), f"{path}.records[{index}].server_author"),
            _optional_string(obj.get("task_owner"), f"{path}.records[{index}].task_owner"),
            _required_string(obj.get("title"), f"{path}.records[{index}].title"),
        )
        if record.note_id <= prior_id:
            raise OrcParseError(f"task projection note IDs are not strictly ordered: {path}")
        prior_id = record.note_id
        records.append(record)
    frozen = tuple(records)
    if len(frozen) != status.note_count or _frozen_task_projection_text(frozen) != raw_text:
        raise OrcParseError(f"frozen task projection mismatch: {path}")
    return frozen


def _freeze_current_task_note(
    record: _ObservedTaskNote, relative: str
) -> _FrozenTaskNote:
    if record.title is None:
        raise OrcParseError(
            f"new task note {record.note_id} lacks its task row in {relative}"
        )
    return _FrozenTaskNote(
        record.note_id,
        record.task_id,
        record.content,
        record.created_at,
        record.server_author,
        record.task_owner,
        record.title,
    )


def _bootstrap_frozen_task_projection(
    snapshot_root: Path,
    source: OrcSourceCopy,
    source_path: Path,
) -> tuple[_FrozenTaskNote, ...]:
    """Load v3 history or losslessly upgrade one older immutable snapshot."""

    if source.task_projection is not None and (
        source.task_projection.policy == _TASK_HISTORY_POLICY
    ):
        return _load_frozen_task_projection(snapshot_root, source.task_projection)
    observed = _observed_task_notes(source_path)
    if source.task_projection is None:
        return tuple(
            _freeze_current_task_note(record, source.source_path)
            for record in observed
        )
    enrichments = _load_legacy_task_projection(
        snapshot_root, source.task_projection
    )
    enrichment_by_id = {record.note_id: record for record in enrichments}
    if len(observed) != len(enrichments):
        raise OrcParseError(
            f"legacy task projection count does not match its snapshot for "
            f"{source.source_path}"
        )
    frozen: list[_FrozenTaskNote] = []
    for record in observed:
        enrichment = enrichment_by_id.get(record.note_id)
        if enrichment is None or enrichment.task_id != record.task_id:
            raise OrcParseError(
                f"legacy task projection does not match note {record.note_id} in "
                f"{source.source_path}"
            )
        frozen.append(
            _FrozenTaskNote(
                record.note_id,
                record.task_id,
                record.content,
                record.created_at,
                enrichment.server_author,
                enrichment.task_owner,
                enrichment.title,
            )
        )
    return tuple(frozen)


def _task_enrichment_sha256(records: Sequence[_FrozenTaskNote]) -> str:
    return hashlib.sha256(
        _task_projection_text(tuple(record.enrichment for record in records)).encode(
            "utf-8"
        )
    ).hexdigest()


def _stage_task_projection(
    records: Sequence[_FrozenTaskNote], snapshot_root: Path
) -> tuple[Path, Path, str, str]:
    text = _frozen_task_projection_text(records)
    sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()
    relative = _task_projection_relative(sha256)
    target = _snapshot_path(snapshot_root, relative)
    temporary = snapshot_root / ".staging" / (
        f"task-projection-{os.getpid()}-{secrets.token_hex(8)}.json"
    )
    _ensure_snapshot_parent(temporary, snapshot_root)
    with temporary.open("x", encoding="utf-8", newline="") as handle:
        handle.write(text)
        handle.flush()
    return temporary, target, sha256, relative


def _semantic_identity(payload: Mapping[str, object]) -> _SemanticIdentity:
    try:
        encoded = json.dumps(
            dict(payload),
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise OrcParseError(f"semantic identity is not canonical JSON: {error}") from error
    return _SemanticIdentity(hashlib.sha256(encoded).hexdigest(), len(encoded))


def _session_semantic_identity(
    source_path: str,
    owner_session_id: str,
    state: _LogicalState,
    meta: _SessionMeta,
    auxiliary: _AuxiliaryObservation,
    schema_version: int = 2,
) -> _SemanticIdentity:
    if schema_version not in (2, 3, 4):
        raise OrcParseError(
            f"unsupported Orc session semantic schema {schema_version}"
        )
    return _semantic_identity(
        {
            "schema_version": schema_version,
            "kind": "session",
            "source_path": source_path,
            "owner_session_id": owner_session_id,
            "append_count": state.append_count,
            "append_max_id": state.append_max_id,
            "append_prefix_sha256": state.append_prefix_sha256,
            "stable_spawn_sha256": auxiliary.stable_spawn_sha256,
            "session_id": meta.session_id,
            "parent_id": meta.parent_id,
            "name": meta.name,
            "created_at_ms": meta.created_at_ms,
        }
    )


def _session_canonical_state(path: Path, meta: _SessionMeta) -> _LogicalState:
    """Hash normalized transcript semantics independently of Orc storage layout."""

    events, tools, turns = _content_records(path, meta)
    ordered_events = tuple(
        sorted(events, key=lambda item: (item.timestamp_ms, item.event_id))
    )
    ordered_tools = tuple(
        sorted(tools, key=lambda item: (item.started_at_ms, item.call_id))
    )
    ordered_turns = tuple(
        sorted(turns, key=lambda item: (item.started_at_ms, item.turn_id))
    )
    payload = json.dumps(
        {
            "events": [event.to_json_obj() for event in ordered_events],
            "tool_calls": [tool.to_json_obj() for tool in ordered_tools],
            "turns": [turn.to_json_obj() for turn in ordered_turns],
        },
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return _LogicalState(
        append_count=len(ordered_events) + len(ordered_tools) + len(ordered_turns),
        append_max_id=0,
        append_prefix_sha256=hashlib.sha256(payload).hexdigest(),
    )


def _task_semantic_identity(
    source_path: str,
    owner_session_id: str,
    task_source_ordinal: int,
    state: _LogicalState,
    projection_sha256: str,
    schema_version: int = 2,
) -> _SemanticIdentity:
    if schema_version not in (2, 3):
        raise OrcParseError(
            f"unsupported Orc task semantic schema {schema_version}"
        )
    return _semantic_identity(
        {
            "schema_version": schema_version,
            "kind": "task",
            "source_path": source_path,
            "owner_session_id": owner_session_id,
            "task_source_ordinal": task_source_ordinal,
            "append_count": state.append_count,
            "append_max_id": state.append_max_id,
            "append_prefix_sha256": state.append_prefix_sha256,
            "task_projection_sha256": projection_sha256,
        }
    )


def _frozen_task_logical_state(
    records: Sequence[_FrozenTaskNote],
) -> _LogicalState:
    digest = hashlib.sha256()
    for record in records:
        digest.update(b"R")
        for value in (
            record.note_id,
            record.task_id,
            record.content,
            record.created_at,
        ):
            _update_digest(digest, value)
    return _LogicalState(
        len(records),
        max((record.note_id for record in records), default=0),
        digest.hexdigest(),
    )


def _task_rewrite_observation(
    frozen: Sequence[_FrozenTaskNote],
    current: Mapping[int, _ObservedTaskNote],
) -> tuple[str, int, str]:
    missing_ids = tuple(record.note_id for record in frozen if record.note_id not in current)
    facts: list[dict[str, object]] = []
    for record in frozen:
        observed = current.get(record.note_id)
        if observed is None:
            facts.append({"note_id": record.note_id, "state": "missing"})
            continue
        observed_enrichment = (
            observed.server_author,
            observed.task_owner,
            observed.title,
        )
        frozen_enrichment = (
            record.server_author,
            record.task_owner,
            record.title,
        )
        if observed_enrichment != frozen_enrichment:
            facts.append(
                {
                    "note_id": record.note_id,
                    "state": "enrichment-changed",
                    "server_author": observed.server_author,
                    "task_owner": observed.task_owner,
                    "title": observed.title,
                }
            )
    payload = json.dumps(facts, sort_keys=True, separators=(",", ":")).encode()
    rewrite_sha = (
        _EMPTY_TASK_REWRITE_SHA256
        if not facts
        else hashlib.sha256(payload).hexdigest()
    )
    missing_payload = json.dumps(missing_ids, separators=(",", ":")).encode()
    return rewrite_sha, len(missing_ids), hashlib.sha256(missing_payload).hexdigest()


def _task_unobserved_id_gaps(
    frozen: Sequence[_FrozenTaskNote], sequence: int
) -> tuple[int, str]:
    observed = {record.note_id for record in frozen}
    gaps = tuple(note_id for note_id in range(1, sequence + 1) if note_id not in observed)
    payload = json.dumps(gaps, separators=(",", ":")).encode()
    return len(gaps), hashlib.sha256(payload).hexdigest()


def _validate_manifest_logical_state(
    source: OrcSourceCopy, state: _LogicalState, where: str
) -> None:
    if (
        state.append_count != source.append_count
        or state.append_max_id != source.append_max_id
        or state.append_prefix_sha256 != source.append_prefix_sha256
    ):
        raise OrcParseError(f"Orc logical state does not match its manifest: {where}")


def _validate_recorded_semantic_identity(
    source: OrcSourceCopy, identity: _SemanticIdentity, where: str
) -> None:
    if source.semantic_identity_mode != _SEMANTIC_IDENTITY_DETERMINISTIC:
        return
    if (
        source.semantic_sha256 != identity.sha256
        or source.semantic_complete_bytes != identity.complete_bytes
    ):
        raise OrcParseError(
            f"Orc deterministic semantic identity does not match artifacts: {where}"
        )


def _validate_legacy_semantic_identity(
    snapshot_root: Path,
    source: OrcSourceCopy,
    current_identity: _SemanticIdentity,
    projection_sha256: str | None,
) -> None:
    if source.semantic_identity_mode != _SEMANTIC_IDENTITY_LEGACY:
        return
    if source.semantic_baseline_path is None:
        raise OrcParseError(
            f"legacy Orc semantic identity lacks a baseline for {source.source_path}"
        )
    baseline_path = _snapshot_path(snapshot_root, source.semantic_baseline_path)
    if baseline_path.is_symlink() or not baseline_path.is_file():
        raise OrcParseError(f"legacy Orc semantic baseline is missing or unsafe: {baseline_path}")
    if (
        _sha256_file(baseline_path) != source.semantic_sha256
        or baseline_path.stat().st_size != source.semantic_complete_bytes
    ):
        raise OrcParseError(
            f"legacy Orc semantic baseline does not match its cache identity: {baseline_path}"
        )
    baseline_state = _logical_state(baseline_path, source.kind)
    if source.kind == "session":
        baseline_meta = _session_meta(baseline_path, source.source_path)
        baseline_auxiliary = _auxiliary_observation(baseline_path, baseline_meta)
        baseline_identity = _session_semantic_identity(
            source.source_path,
            source.owner_session_id,
            _session_canonical_state(baseline_path, baseline_meta),
            baseline_meta,
            baseline_auxiliary,
            4,
        )
    else:
        if projection_sha256 is None:
            raise OrcParseError(
                f"legacy task semantic identity lacks a projection for {source.source_path}"
            )
        baseline_identity = _task_semantic_identity(
            source.source_path,
            source.owner_session_id,
            source.task_source_ordinal or 0,
            baseline_state,
            projection_sha256,
        )
    if baseline_identity != current_identity:
        raise OrcParseError(
            f"legacy Orc semantic state changed without an identity transition: "
            f"{source.source_path}"
        )


def _validate_semantic_alias(
    snapshot_root: Path,
    source: OrcSourceCopy,
    canonical: _SemanticIdentity,
) -> None:
    recorded = (source.semantic_sha256, source.semantic_complete_bytes)
    expected = (canonical.sha256, canonical.complete_bytes)
    if source.kind == "task":
        if recorded != expected or source.semantic_alias_baseline_path is not None:
            raise OrcParseError(
                f"task semantic alias differs from canonical history: "
                f"{source.source_path}"
            )
        return
    if recorded == expected:
        if source.semantic_alias_baseline_path is not None:
            raise OrcParseError(
                f"canonical Orc semantic identity has an unnecessary alias baseline: "
                f"{source.source_path}"
            )
        return
    relative = source.semantic_alias_baseline_path
    if relative is None:
        relative = source.snapshot_path
    match = re.fullmatch(
        rf"{re.escape(_SNAPSHOT_OBJECT_ROOT)}/([0-9a-f]{{2}})/"
        r"([0-9a-f]{64})\.db",
        relative,
    )
    if match is None or match.group(1) != match.group(2)[:2]:
        raise OrcParseError(
            f"Orc semantic alias baseline is not a managed object: {relative}"
        )
    path = _snapshot_path(snapshot_root, relative)
    if (
        path.is_symlink()
        or not path.is_file()
        or _sha256_file(path) != match.group(2)
    ):
        raise OrcParseError(f"Orc semantic alias baseline is missing or unsafe: {path}")
    meta = _session_meta(path, source.source_path)
    auxiliary = _auxiliary_observation(path, meta)
    baseline_canonical = _session_semantic_identity(
        source.source_path,
        source.owner_session_id,
        _session_canonical_state(path, meta),
        meta,
        auxiliary,
        4,
    )
    if baseline_canonical != canonical:
        raise OrcParseError(
            f"Orc semantic alias baseline has different canonical semantics: "
            f"{source.source_path}"
        )
    states = [_logical_state(path, "session")]
    if states[0].append_max_id & _SESSION_STATE_TAG:
        states.extend(
            _logical_state(path, "session", session_state_mode=mode)
            for mode in ("content-only", "messages-only")
        )
    identities = {
        (
            identity.sha256,
            identity.complete_bytes,
        )
        for state in states
        for identity in (
            _session_semantic_identity(
                source.source_path,
                source.owner_session_id,
                state,
                meta,
                auxiliary,
                3 if state.append_max_id & _SESSION_STATE_TAG else 2,
            ),
        )
    }
    if recorded not in identities:
        raise OrcParseError(
            f"Orc semantic alias does not match its authenticated baseline: "
            f"{source.source_path}"
        )


def _validate_source_semantic_identity(
    snapshot_root: Path, source: OrcSourceCopy
) -> _SemanticIdentity:
    path = _snapshot_path(snapshot_root, source.snapshot_path)
    if path.is_symlink() or not path.is_file() or _sha256_file(path) != source.sha256:
        raise OrcParseError(f"Orc snapshot does not match its manifest: {path}")
    state = _logical_state(path, source.kind)
    _validate_manifest_logical_state(source, state, str(path))
    projection_sha256: str | None = None
    if source.kind == "session":
        meta = _session_meta(path, source.source_path)
        auxiliary = _auxiliary_observation(path, meta)
        if (
            source.auxiliary.message_count != len(auxiliary.messages)
            or source.auxiliary.message_sha256 != auxiliary.message_sha256
            or source.auxiliary.stable_spawn_count != len(auxiliary.stable_spawns)
            or source.auxiliary.stable_spawn_sha256
            != auxiliary.stable_spawn_sha256
        ):
            raise OrcParseError(
                f"Orc auxiliary evidence does not match its snapshot: {path}"
            )
        legacy_identity = _session_semantic_identity(
            source.source_path,
            source.owner_session_id,
            state,
            meta,
            auxiliary,
            3 if state.append_max_id & _SESSION_STATE_TAG else 2,
        )
        accepted_legacy_identities = {
            (legacy_identity.sha256, legacy_identity.complete_bytes)
        }
        if state.append_max_id & _SESSION_STATE_TAG:
            for storage_mode in ("content-only", "messages-only"):
                storage_state = _logical_state(
                    path, "session", session_state_mode=storage_mode
                )
                storage_identity = _session_semantic_identity(
                    source.source_path,
                    source.owner_session_id,
                    storage_state,
                    meta,
                    auxiliary,
                    2,
                )
                accepted_legacy_identities.add(
                    (storage_identity.sha256, storage_identity.complete_bytes)
                )
        identity = _session_semantic_identity(
            source.source_path,
            source.owner_session_id,
            _session_canonical_state(path, meta),
            meta,
            auxiliary,
            4,
        )
        if (
            source.canonical_semantic_sha256 is None
            and source.semantic_identity_mode == _SEMANTIC_IDENTITY_DETERMINISTIC
            and (
            source.semantic_sha256,
            source.semantic_complete_bytes,
            )
            not in accepted_legacy_identities
            | {(identity.sha256, identity.complete_bytes)}
        ):
            raise OrcParseError(
                f"Orc deterministic semantic identity does not match artifacts: "
                f"{source.source_path}"
            )
    else:
        if source.task_projection is None:
            raise OrcParseError(f"task source lacks frozen enrichment: {source.source_path}")
        if source.task_projection.policy == _TASK_HISTORY_POLICY:
            frozen = _load_frozen_task_projection(
                snapshot_root, source.task_projection
            )
            current_records = _observed_task_notes(path)
            current_sequence = _task_note_sequence(path)
            if (
                source.task_projection.observed_note_sequence is not None
                and current_sequence
                != source.task_projection.observed_note_sequence
            ):
                raise OrcParseError(
                    f"task note allocation sequence does not match its "
                    f"snapshot: {path}"
                )
            current = {record.note_id: record for record in current_records}
            frozen_by_id = {record.note_id: record for record in frozen}
            unexpected = sorted(set(current) - set(frozen_by_id))
            if unexpected:
                raise OrcParseError(
                    f"task snapshot contains an unfrozen note for "
                    f"{source.source_path}: {unexpected[0]}"
                )
            for note_id, observed in current.items():
                prior = frozen_by_id[note_id]
                if (
                    observed.task_id,
                    observed.content,
                    observed.created_at,
                ) != (prior.task_id, prior.content, prior.created_at):
                    raise OrcParseError(
                        f"task note immutable core was rewritten for note "
                        f"{note_id} in {source.source_path}"
                    )
            enrichment_sha256 = _task_enrichment_sha256(frozen)
            if (
                enrichment_sha256
                != source.task_projection.observed_enrichment_sha256
            ):
                raise OrcParseError(
                    f"frozen task enrichment does not match its manifest: {path}"
                )
            rewrite_sha256, missing_count, missing_sha256 = (
                _task_rewrite_observation(frozen, current)
            )
            gap_count, gap_sha256 = _task_unobserved_id_gaps(
                frozen, current_sequence
            )
            if (
                rewrite_sha256
                != source.task_projection.observed_note_rewrite_sha256
                or missing_count != source.task_projection.missing_note_count
                or missing_sha256
                != source.task_projection.missing_note_ids_sha256
                or (
                    source.task_projection.unobserved_note_id_gap_count is not None
                    and (
                        gap_count
                        != source.task_projection.unobserved_note_id_gap_count
                        or gap_sha256
                        != source.task_projection.unobserved_note_id_gap_sha256
                    )
                )
            ):
                raise OrcParseError(
                    f"task rewrite observation does not match its snapshot: {path}"
                )
            projection_sha256 = enrichment_sha256
            identity = _task_semantic_identity(
                source.source_path,
                source.owner_session_id,
                source.task_source_ordinal or 0,
                _frozen_task_logical_state(frozen),
                enrichment_sha256,
            )
        else:
            _load_legacy_task_projection(snapshot_root, source.task_projection)
            observation = _task_enrichment_observation(path)
            if (
                observation.sha256
                != source.task_projection.observed_enrichment_sha256
            ):
                raise OrcParseError(
                    f"task enrichment observation does not match its snapshot: {path}"
                )
            projection_sha256 = source.task_projection.sha256
            identity = _task_semantic_identity(
                source.source_path,
                source.owner_session_id,
                source.task_source_ordinal or 0,
                state,
                projection_sha256,
            )
    if source.canonical_semantic_sha256 is not None and (
        source.canonical_semantic_sha256 != identity.sha256
        or source.canonical_semantic_complete_bytes != identity.complete_bytes
    ):
        raise OrcParseError(
            f"Orc canonical semantic identity does not match artifacts: "
            f"{source.source_path}"
        )
    if (
        source.canonical_semantic_sha256 is not None
        and source.semantic_identity_mode == _SEMANTIC_IDENTITY_DETERMINISTIC
    ):
        _validate_semantic_alias(snapshot_root, source, identity)
    if source.kind != "session" and source.canonical_semantic_sha256 is None:
        _validate_recorded_semantic_identity(source, identity, source.source_path)
    _validate_legacy_semantic_identity(
        snapshot_root, source, identity, projection_sha256
    )
    return identity


def _session_geometry(
    connection: sqlite3.Connection, path: Path, session_state_mode: str | None
) -> _SessionGeometry:
    """Measure the tables a session's append prefix is digested over, honouring legacy modes."""

    storage_table = _session_storage_table(connection, str(path))
    if session_state_mode == "messages-only":
        if storage_table != "messages":
            raise OrcParseError(
                f"{path}: messages-only legacy state lacks messages table"
            )
    elif session_state_mode == "content-only":
        storage_table = "content_blocks"
    elif session_state_mode is not None:
        raise OrcParseError(
            f"unsupported legacy Orc session state mode {session_state_mode!r}"
        )
    content_count = 0
    content_max_id = 0
    if session_state_mode != "messages-only":
        content_row = _one(
            connection,
            "SELECT COUNT(*), COALESCE(MAX(rowid), 0) FROM content_blocks",
            str(path),
        )
        content_count = _nonnegative_integer(content_row[0], f"{path}: content count")
        content_max_id = _nonnegative_integer(
            content_row[1], f"{path}: max content rowid"
        )
    message_count = 0
    message_max_id = 0
    if storage_table == "messages":
        message_row = _one(
            connection,
            "SELECT COUNT(*), COALESCE(MAX(id), 0) FROM messages",
            str(path),
        )
        message_count = _nonnegative_integer(message_row[0], f"{path}: message count")
        message_max_id = _nonnegative_integer(
            message_row[1], f"{path}: max message id"
        )
    return _SessionGeometry(
        storage_table=storage_table,
        content_count=content_count,
        content_max_id=content_max_id,
        message_count=message_count,
        message_max_id=message_max_id,
    )


def _session_prefix_scopes(
    geometry: _SessionGeometry,
    session_state_mode: str | None,
    prefix_max_id: int | None,
) -> tuple[tuple[_PrefixScope, ...], bool]:
    """Return exactly what the append-prefix digest covers, plus whether it is a legacy prefix.

    Both the digest in :func:`_logical_state` and the row diff that explains a mismatch derive
    their SQL from this one answer, so the two can never disagree about which rows were compared.
    That matters because the diff is what an operator reads before deciding whether to override a
    refusal: a diff scoped differently from the digest would be evidence about a different question.
    """

    if session_state_mode == "messages-only":
        limit = geometry.message_max_id if prefix_max_id is None else prefix_max_id
        return ((_PrefixScope("messages", _MESSAGE_PREFIX_COLUMNS, limit),), False)
    if geometry.storage_table == "messages":
        if prefix_max_id is None:
            content_limit = geometry.content_max_id
            message_limit = geometry.message_max_id
            legacy_prefix = False
        elif prefix_max_id & _SESSION_STATE_TAG:
            packed = prefix_max_id ^ _SESSION_STATE_TAG
            content_limit = packed >> _SESSION_STATE_SHIFT
            message_limit = packed & _SESSION_STATE_MASK
            legacy_prefix = False
        else:
            content_limit = prefix_max_id
            message_limit = 0
            legacy_prefix = True
        content_scope = _PrefixScope(
            "content_blocks", _CONTENT_BLOCK_PREFIX_COLUMNS, content_limit
        )
        if legacy_prefix:
            return ((content_scope,), True)
        return (
            (
                content_scope,
                _PrefixScope("messages", _MESSAGE_PREFIX_COLUMNS, message_limit),
            ),
            False,
        )
    limit = geometry.content_max_id if prefix_max_id is None else prefix_max_id
    return (
        (_PrefixScope("content_blocks", _CONTENT_BLOCK_PREFIX_COLUMNS, limit),),
        False,
    )


def _logical_state(
    path: Path,
    kind: str,
    *,
    prefix_max_id: int | None = None,
    legacy_task_fields: bool = False,
    session_state_mode: str | None = None,
) -> _LogicalState:
    connection = _read_only(path)
    try:
        check = _one(connection, "PRAGMA quick_check", str(path))
        if check != ("ok",):
            raise OrcParseError(f"SQLite quick_check failed for {path}: {check!r}")
        if kind == "session":
            _require_tables(
                connection,
                ("session_meta", "content_blocks"),
                str(path),
            )
            geometry = _session_geometry(connection, path, session_state_mode)
            scopes, legacy_prefix = _session_prefix_scopes(
                geometry, session_state_mode, prefix_max_id
            )
            if session_state_mode == "messages-only":
                scope = scopes[0]
                prefix_count, prefix_digest = _query_digest(
                    connection, scope.query, (scope.limit,)
                )
                return _LogicalState(
                    append_count=(
                        geometry.message_count
                        if prefix_max_id is None
                        else prefix_count
                    ),
                    append_max_id=geometry.message_max_id,
                    append_prefix_sha256=prefix_digest,
                )
            if geometry.storage_table == "messages":
                if (
                    geometry.content_max_id > _SESSION_STATE_MASK
                    or geometry.message_max_id > _SESSION_STATE_MASK
                ):
                    raise OrcParseError(f"{path}: session row identity exceeds 64 bits")
                packed_max_id = (
                    _SESSION_STATE_TAG
                    | (geometry.content_max_id << _SESSION_STATE_SHIFT)
                    | geometry.message_max_id
                )
                content_scope = scopes[0]
                content_prefix_count, content_prefix_digest = _query_digest(
                    connection, content_scope.query, (content_scope.limit,)
                )
                if legacy_prefix:
                    return _LogicalState(
                        append_count=content_prefix_count,
                        append_max_id=packed_max_id,
                        append_prefix_sha256=content_prefix_digest,
                    )
                message_scope = scopes[1]
                message_prefix_count, message_prefix_digest = _query_digest(
                    connection, message_scope.query, (message_scope.limit,)
                )
                combined = hashlib.sha256()
                for value in (
                    "content_blocks",
                    content_prefix_count,
                    content_scope.limit,
                    content_prefix_digest,
                    "messages",
                    message_prefix_count,
                    message_scope.limit,
                    message_prefix_digest,
                ):
                    _update_digest(combined, value)
                return _LogicalState(
                    append_count=(
                        geometry.content_count + geometry.message_count
                        if prefix_max_id is None
                        else content_prefix_count + message_prefix_count
                    ),
                    append_max_id=packed_max_id,
                    append_prefix_sha256=combined.hexdigest(),
                )
            scope = scopes[0]
            prefix_count, prefix_digest = _query_digest(
                connection, scope.query, (scope.limit,)
            )
            return _LogicalState(
                append_count=(
                    geometry.content_count if prefix_max_id is None else prefix_count
                ),
                append_max_id=geometry.content_max_id,
                append_prefix_sha256=prefix_digest,
            )
        if kind == "task":
            _require_tables(connection, ("tasks", "task_notes"), str(path))
            count_row = _one(
                connection,
                "SELECT COUNT(*), COALESCE(MAX(id), 0) FROM task_notes",
                str(path),
            )
            append_count = _nonnegative_integer(count_row[0], f"{path}: note count")
            append_max_id = _nonnegative_integer(count_row[1], f"{path}: max note id")
            limit = append_max_id if prefix_max_id is None else prefix_max_id
            selected_fields = (
                "id, task_id, content, created_at, server_comment_id, author_unixname"
                if legacy_task_fields
                else "id, task_id, content, created_at"
            )
            prefix_count, prefix_digest = _query_digest(
                connection,
                f"SELECT {selected_fields} FROM task_notes WHERE id <= ? ORDER BY id",
                (limit,),
            )
            return _LogicalState(
                append_count=append_count if prefix_max_id is None else prefix_count,
                append_max_id=append_max_id,
                append_prefix_sha256=prefix_digest,
            )
        raise OrcParseError(f"unsupported Orc source kind {kind!r}")
    except sqlite3.Error as error:
        raise OrcParseError(f"failed to inspect Orc SQLite source {path}: {error}") from error
    finally:
        connection.close()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                return digest.hexdigest()
            digest.update(chunk)


def _fsync_regular_file(path: Path) -> None:
    try:
        file_fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    except OSError as error:
        raise OrcParseError(f"cannot open snapshot object for fsync {path}: {error}") from error
    try:
        mode = os.fstat(file_fd).st_mode
        if not stat.S_ISREG(mode):
            raise OrcParseError(f"snapshot fsync target is not a regular file: {path}")
        os.fsync(file_fd)
    finally:
        os.close(file_fd)


def _fsync_directory(path: Path) -> None:
    try:
        directory_fd = os.open(
            path,
            os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW,
        )
    except OSError as error:
        raise OrcParseError(f"cannot open snapshot directory for fsync {path}: {error}") from error
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def _ensure_snapshot_parent(path: Path, snapshot_root: Path) -> None:
    snapshot_root_existed = snapshot_root.exists()
    snapshot_root.mkdir(parents=True, exist_ok=True)
    if snapshot_root.is_symlink() or not snapshot_root.is_dir():
        raise OrcParseError(f"snapshot root is a symlink or non-directory: {snapshot_root}")
    if not snapshot_root_existed:
        _fsync_directory(snapshot_root.parent)
    relative = path.parent.relative_to(snapshot_root)
    current = snapshot_root
    for part in relative.parts:
        current = current / part
        if current.exists():
            if current.is_symlink() or not current.is_dir():
                raise OrcParseError(
                    f"snapshot path component is a symlink or non-directory: {current}"
                )
        else:
            current.mkdir(mode=0o700)
            _fsync_directory(current.parent)


def _backup_database(source: Path, destination: Path, snapshot_root: Path) -> None:
    _ensure_snapshot_parent(destination, snapshot_root)
    if destination.exists() and (destination.is_symlink() or not destination.is_file()):
        raise OrcParseError(f"snapshot target is a symlink or non-file: {destination}")
    source_connection = _read_only(source)
    try:
        destination_connection = sqlite3.connect(destination)
        try:
            source_connection.backup(destination_connection)
            destination_connection.commit()
            journal_mode = destination_connection.execute(
                "PRAGMA journal_mode = DELETE"
            ).fetchone()
            if journal_mode is None or str(journal_mode[0]).casefold() != "delete":
                raise OrcParseError(
                    f"failed to normalize Orc snapshot journal mode for {source}"
                )
            destination_connection.commit()
        finally:
            destination_connection.close()
    except sqlite3.Error as error:
        raise OrcParseError(f"failed to back up Orc SQLite source {source}: {error}") from error
    finally:
        source_connection.close()


def _publish_snapshot_candidate(
    temporary: Path,
    target: Path,
    expected_sha256: str,
    snapshot_root: Path,
) -> bool:
    """Atomically publish one immutable object, or reuse an identical orphan."""

    _ensure_snapshot_parent(target, snapshot_root)
    _fsync_regular_file(temporary)
    try:
        mode = target.lstat().st_mode
    except FileNotFoundError:
        mode = None
    if mode is not None:
        if not stat.S_ISREG(mode) or stat.S_ISLNK(mode):
            raise OrcParseError(
                f"snapshot object target is a symlink or non-file: {target}"
            )
        if _sha256_file(target) != expected_sha256:
            raise OrcParseError(
                f"preexisting snapshot object has the wrong hash: {target}"
            )
        _fsync_regular_file(target)
        _fsync_directory(target.parent)
        temporary.unlink()
        return False
    try:
        os.link(temporary, target)
    except FileExistsError:
        if target.is_symlink() or not target.is_file():
            raise OrcParseError(
                f"snapshot object target is a symlink or non-file: {target}"
            )
        if _sha256_file(target) != expected_sha256:
            raise OrcParseError(
                f"raced snapshot object has the wrong hash: {target}"
            )
        _fsync_regular_file(target)
        _fsync_directory(target.parent)
        temporary.unlink()
        return False
    if _sha256_file(target) != expected_sha256:
        raise OrcParseError(f"published snapshot object failed hash verification: {target}")
    _fsync_directory(target.parent)
    temporary.unlink()
    return True


def _prune_managed_objects(
    snapshot_root: Path,
    root_name: str,
    extension: str,
    retained: set[str],
) -> int:
    object_root = snapshot_root / root_name
    try:
        root_mode = object_root.lstat().st_mode
    except FileNotFoundError:
        return 0
    if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
        raise OrcParseError(f"Orc snapshot object root is unsafe: {object_root}")
    removed = 0
    for prefix in sorted(object_root.iterdir(), key=lambda path: path.name):
        prefix_mode = prefix.lstat().st_mode
        if stat.S_ISLNK(prefix_mode):
            raise OrcParseError(f"symlink in Orc snapshot object store: {prefix}")
        if not stat.S_ISDIR(prefix_mode) or re.fullmatch(r"[0-9a-f]{2}", prefix.name) is None:
            continue
        prefix_changed = False
        for candidate in sorted(prefix.iterdir(), key=lambda path: path.name):
            candidate_mode = candidate.lstat().st_mode
            if stat.S_ISLNK(candidate_mode):
                raise OrcParseError(f"symlink in Orc snapshot object store: {candidate}")
            sidecar_match = (
                re.fullmatch(r"([0-9a-f]{64})\.db-(?:wal|shm)", candidate.name)
                if extension == ".db"
                else None
            )
            if (
                stat.S_ISREG(candidate_mode)
                and sidecar_match is not None
                and sidecar_match.group(1)[:2] == prefix.name
            ):
                candidate.unlink()
                removed += 1
                prefix_changed = True
                continue
            match = re.fullmatch(
                rf"([0-9a-f]{{64}}){re.escape(extension)}", candidate.name
            )
            if (
                not stat.S_ISREG(candidate_mode)
                or match is None
                or match.group(1)[:2] != prefix.name
            ):
                continue
            relative = candidate.relative_to(snapshot_root).as_posix()
            if relative in retained:
                continue
            candidate.unlink()
            removed += 1
            prefix_changed = True
        if prefix_changed:
            _fsync_directory(prefix)
    return removed


def prune_orc_snapshot_objects(
    snapshot_root: Path, retained_sources: Sequence[OrcSourceCopy]
) -> int:
    """Remove unreferenced managed objects after a durable manifest commit.

    Two classes of retention, deliberately not treated alike. Everything a source *needs* -- its
    own snapshot, a preserved raw-byte semantic baseline, an alias baseline, a frozen task
    projection -- is retained *and* verified below: missing or wrong-hashed, and this raises,
    because a later ingest would otherwise compute against bytes that are not the bytes it
    recorded.

    An accepted append-prefix override's ``superseded_snapshot_path`` is retained without being
    verified. It is evidence, not an input: nothing downstream reads it, and the override record
    itself -- both digests, the row count, the bounded rows -- is what a future run validates. So
    the pointer earns the object protection from GC, which is the entire point of naming it, but a
    hand-deleted or hand-pruned evidence object must not be able to fail an unrelated ingest six
    months later. Failing closed on a *need* prevents silent corruption; failing closed on a
    *keepsake* only strands the archive.
    """

    retained_databases: set[str] = set()
    retained_projections: set[str] = set()
    for source in retained_sources:
        expected = _snapshot_object_relative(source.sha256)
        if source.snapshot_path != expected:
            raise OrcParseError(
                f"refusing Orc object GC for unmanaged snapshot path {source.snapshot_path!r}"
            )
        retained_databases.add(source.snapshot_path)
        if source.append_prefix_override is not None:
            retained_databases.add(
                source.append_prefix_override.superseded_snapshot_path
            )
        retained_items: tuple[tuple[str, str], ...] = (
            (source.snapshot_path, source.sha256),
        )
        if source.semantic_identity_mode == _SEMANTIC_IDENTITY_LEGACY:
            if source.semantic_baseline_path is None:
                raise OrcParseError(
                    f"legacy Orc source lacks semantic baseline: {source.source_path}"
                )
            retained_items = (
                *retained_items,
                (source.semantic_baseline_path, source.semantic_sha256),
            )
            if source.semantic_baseline_path.startswith(
                f"{_SNAPSHOT_OBJECT_ROOT}/"
            ):
                retained_databases.add(source.semantic_baseline_path)
        if source.semantic_alias_baseline_path is not None:
            alias_match = re.fullmatch(
                rf"{re.escape(_SNAPSHOT_OBJECT_ROOT)}/[0-9a-f]{{2}}/"
                r"([0-9a-f]{64})\.db",
                source.semantic_alias_baseline_path,
            )
            if alias_match is None:
                raise OrcParseError(
                    f"invalid Orc semantic alias baseline: "
                    f"{source.semantic_alias_baseline_path}"
                )
            retained_databases.add(source.semantic_alias_baseline_path)
            retained_items = (
                *retained_items,
                (source.semantic_alias_baseline_path, alias_match.group(1)),
            )
        if source.task_projection is not None:
            retained_projections.add(source.task_projection.path)
            retained_items = (
                *retained_items,
                (source.task_projection.path, source.task_projection.sha256),
            )
        for relative, expected_sha256 in retained_items:
            path = _snapshot_path(snapshot_root, relative)
            try:
                mode = path.lstat().st_mode
            except FileNotFoundError as error:
                raise OrcParseError(
                    f"retained Orc snapshot artifact is missing: {path}"
                ) from error
            if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
                raise OrcParseError(f"retained Orc snapshot artifact is unsafe: {path}")
            if _sha256_file(path) != expected_sha256:
                raise OrcParseError(
                    f"retained Orc snapshot artifact has wrong hash: {path}"
                )
            if (
                relative == source.semantic_baseline_path
                and path.stat().st_size != source.semantic_complete_bytes
            ):
                raise OrcParseError(
                    f"retained Orc semantic baseline has wrong size: {path}"
                )
    return _prune_managed_objects(
        snapshot_root,
        _SNAPSHOT_OBJECT_ROOT,
        ".db",
        retained_databases,
    ) + _prune_managed_objects(
        snapshot_root,
        _TASK_PROJECTION_ROOT,
        ".json",
        retained_projections,
    )


def prune_orc_staging(snapshot_root: Path) -> int:
    """Remove managed candidates left by a terminated ingest transaction."""

    staging = _snapshot_path(snapshot_root, ".staging")
    try:
        mode = staging.lstat().st_mode
    except FileNotFoundError:
        return 0
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise OrcParseError(f"Orc staging root is unsafe: {staging}")
    managed = re.compile(
        r"(?:orc-[0-9]+-[0-9a-f]{16}\.db(?:-(?:wal|shm))?"
        r"|task-projection-[0-9]+-[0-9a-f]{16}\.json)"
    )
    removed = 0
    for candidate in sorted(staging.iterdir(), key=lambda item: item.name):
        candidate_mode = candidate.lstat().st_mode
        if stat.S_ISLNK(candidate_mode) or not stat.S_ISREG(candidate_mode):
            raise OrcParseError(f"unsafe entry in Orc staging directory: {candidate}")
        if managed.fullmatch(candidate.name) is None:
            continue
        candidate.unlink()
        removed += 1
    if removed:
        _fsync_directory(staging)
    return removed


def _plan_snapshot_sources(
    source_root: Path,
    root_session_id: str,
    snapshot_root: Path,
    previous_sources: Sequence[OrcSourceCopy],
    continuation_specs: Sequence[OrcContinuationSpec],
) -> _DiscoveryPlan:
    roots = _lineage_roots(
        root_session_id, tuple(spec.session_id for spec in continuation_specs)
    )
    live_discovered = list(
        _discover_continuation_sources(
            source_root,
            root_session_id,
            continuation_specs,
            frozenset(
                source.owner_session_id
                for source in previous_sources
                if source.kind == "session"
            ),
        )
        if continuation_specs
        else _discover_sources(source_root, root_session_id)
    )
    discovered_paths = {source.source_path for source in live_discovered}
    if len(discovered_paths) != len(live_discovered):
        raise OrcParseError("Orc source discovery produced duplicate source records")
    previous_by_path: dict[str, OrcSourceCopy] = {}
    for source in previous_sources:
        if source.source_path in previous_by_path:
            raise OrcParseError(
                f"duplicate previous Orc source record: {source.source_path}"
            )
        previous_by_path[source.source_path] = source
    selected_session_ids = frozenset(
        source.owner_candidates[0]
        for source in live_discovered
        if source.kind == "session"
    )
    selected_task_paths = frozenset(
        source.source_path for source in live_discovered if source.kind == "task"
    )
    restricted_task_sources = any(
        spec.start_message_id is not None for spec in continuation_specs
    )
    newly_detached_paths: set[str] = set()
    session_task_paths: dict[str, frozenset[str]] = {}
    for discovered_source in live_discovered:
        if discovered_source.kind != "session":
            continue
        live_path = _live_source_path(source_root, discovered_source.source_path)
        live_meta = _session_meta(live_path, discovered_source.source_path)
        live_task_paths = frozenset(
            path
            for path, _, _ in _session_task_relatives(
                live_path, live_meta, root_session_id
            )
            if not restricted_task_sources or path in selected_task_paths
        )
        session_task_paths[discovered_source.source_path] = live_task_paths
        previous = previous_by_path.get(discovered_source.source_path)
        if previous is None:
            continue
        previous_path = _snapshot_path(snapshot_root, previous.snapshot_path)
        if (
            previous_path.is_symlink()
            or not previous_path.is_file()
            or _sha256_file(previous_path) != previous.sha256
        ):
            raise OrcParseError(
                f"previous Orc snapshot does not match its manifest: {previous_path}"
            )
        prior_meta = _session_meta(previous_path, discovered_source.source_path)
        prior_task_paths = {
            path
            for path, _, _ in _session_task_relatives(
                previous_path, prior_meta, root_session_id
            )
            if not restricted_task_sources or path in selected_task_paths
        }
        newly_detached_paths.update(prior_task_paths - live_task_paths)

    planned_live_task_paths = {
        path
        for paths in session_task_paths.values()
        for path in paths
        if _live_source_path(source_root, path).is_file()
    }
    discovered_live_task_paths = {
        item.source_path for item in live_discovered if item.kind == "task"
    }
    if planned_live_task_paths != discovered_live_task_paths:
        raise OrcParseError(
            "Orc task database references changed during source discovery; retry ingest"
        )

    frozen_paths = {
        source.source_path
        for source in previous_sources
        if source.kind == "task"
        and source.source_path not in discovered_paths
        and (
            source.source_state == _SOURCE_STATE_DETACHED
            or source.source_path in newly_detached_paths
        )
    }
    disappeared = sorted(set(previous_by_path) - discovered_paths - frozen_paths)
    if disappeared:
        raise OrcParseError(
            "previously observed Orc source disappeared: " + ", ".join(disappeared)
        )
    for path in sorted(frozen_paths):
        previous = previous_by_path[path]
        live_discovered.append(
            _DiscoveredSource(
                path,
                "task",
                (previous.owner_session_id,),
                _SOURCE_STATE_DETACHED,
            )
        )
    discovered = tuple(sorted(live_discovered, key=lambda item: item.source_path))
    used_task_ordinals: dict[str, set[int]] = defaultdict(set)
    for prior_source in previous_sources:
        if (
            prior_source.kind == "task"
            and prior_source.task_source_ordinal is not None
        ):
            used_task_ordinals[prior_source.owner_session_id].add(
                prior_source.task_source_ordinal
            )
    task_ordinals: dict[str, int] = {}
    for discovered_item in discovered:
        if discovered_item.kind != "task":
            continue
        previous = previous_by_path.get(discovered_item.source_path)
        owner = (
            previous.owner_session_id
            if previous is not None
            else discovered_item.owner_candidates[0]
        )
        if previous is not None and previous.task_source_ordinal is not None:
            ordinal = previous.task_source_ordinal
        else:
            ordinal = 0
            while ordinal in used_task_ordinals[owner]:
                ordinal += 1
        task_ordinals[discovered_item.source_path] = ordinal
        used_task_ordinals[owner].add(ordinal)
    return _DiscoveryPlan(
        discovered,
        previous_by_path,
        selected_session_ids,
        task_ordinals,
        session_task_paths,
    )


def _prefix_value(value: object) -> tuple[str, str]:
    """Render one SQLite value as the (kind, body) pair the append-prefix digest distinguishes.

    The kinds mirror :func:`_update_digest`'s type tags exactly, which is the point: the diff must
    call two values different on precisely the occasions the digest does. Python's ``==`` does not
    -- ``1 == 1.0`` and ``True == 1`` -- so a diff built on ``==`` alone could report "no rows
    changed" about a digest that really did move, and an operator would be shown an empty
    explanation for a real refusal.
    """

    if value is None:
        return "null", ""
    if isinstance(value, bool):
        return "int", "1" if value else "0"
    if isinstance(value, int):
        return "int", str(value)
    if isinstance(value, float):
        return "real", value.hex()
    if isinstance(value, str):
        return "text", value
    if isinstance(value, bytes):
        return "blob", value.hex()
    # _update_digest raises for anything else, but this function runs while explaining a failure,
    # and a diff that raises replaces the operator's evidence with a second, less informative error.
    return "unsupported", type(value).__name__


def _prefix_values_equal(previous: object, observed: object) -> bool:
    """Compare two SQLite values the way the digest does, without rendering either.

    ``type(...) is type(...)`` is the cheap stand-in for the tag comparison in
    :func:`_prefix_value`: it separates ``1`` from ``1.0`` and ``True`` from ``1``, which plain
    ``==`` does not, and it costs nothing on the overwhelming majority of rows that are identical.
    """

    return previous == observed and type(previous) is type(observed)


def _prefix_rows_equal(
    previous: Sequence[object], observed: Sequence[object]
) -> bool:
    """Compare two prefix rows the way the digest does, cheaply, without rendering them."""

    if len(previous) != len(observed):
        return False
    return all(
        _prefix_values_equal(left, right) for left, right in zip(previous, observed)
    )


def _prefix_window(body: str, start: int, end: int) -> tuple[str, bool]:
    """Return a bounded slice of *body* with ellipses marking whatever was cut."""

    start = max(0, min(start, len(body)))
    end = max(start, min(end, len(body)))
    if end - start > _PREFIX_DIFF_EXCERPT_CHARS:
        end = start + _PREFIX_DIFF_EXCERPT_CHARS
    text = body[start:end]
    if start > 0:
        text = "..." + text
    if end < len(body):
        text = text + "..."
    return text, start > 0 or end < len(body)


def _prefix_excerpts(
    previous_body: str, observed_body: str
) -> tuple[str, str, bool]:
    """Return matching windows centred on where two bodies first and last disagree.

    Truncating from character zero would be useless here: the incident this exists for is a single
    field edited nine minutes after capture inside a large ``message_json`` blob, where two
    head-truncated renderings are byte-identical and say nothing. Anchoring the window on the
    differing region instead makes the report read as ``"token_count":null`` against
    ``"token_count":445`` no matter how far into the value the edit sits.
    """

    shared = min(len(previous_body), len(observed_body))
    head = 0
    while head < shared and previous_body[head] == observed_body[head]:
        head += 1
    tail = 0
    while (
        tail < shared - head
        and previous_body[len(previous_body) - 1 - tail]
        == observed_body[len(observed_body) - 1 - tail]
    ):
        tail += 1
    start = head - _PREFIX_DIFF_CONTEXT_CHARS
    previous_excerpt, previous_bounded = _prefix_window(
        previous_body,
        start,
        len(previous_body) - tail + _PREFIX_DIFF_CONTEXT_CHARS,
    )
    observed_excerpt, observed_bounded = _prefix_window(
        observed_body,
        start,
        len(observed_body) - tail + _PREFIX_DIFF_CONTEXT_CHARS,
    )
    return previous_excerpt, observed_excerpt, previous_bounded or observed_bounded


def _json_object(value: object) -> dict[str, object] | None:
    if not isinstance(value, dict):
        return None
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            return None
        result[key] = item
    return result


def _json_array(value: object) -> list[object] | None:
    if not isinstance(value, list):
        return None
    return list(value)


def _collect_json_changes(
    previous: object, observed: object, path: str, depth: int, found: list[str]
) -> bool:
    """Append the paths whose values differ; return whether a cap stopped the walk early."""

    if len(found) >= _PREFIX_DIFF_MAX_JSON_PATHS:
        return True
    if _prefix_values_equal(previous, observed):
        return False
    if depth >= _PREFIX_DIFF_MAX_JSON_DEPTH:
        found.append(path or ".")
        return True
    previous_object = _json_object(previous)
    observed_object = _json_object(observed)
    if previous_object is not None and observed_object is not None:
        bounded = False
        for key in sorted(set(previous_object) | set(observed_object)):
            child = f"{path}.{key}" if path else key
            if key not in previous_object or key not in observed_object:
                if len(found) >= _PREFIX_DIFF_MAX_JSON_PATHS:
                    return True
                found.append(child)
                continue
            bounded = (
                _collect_json_changes(
                    previous_object[key], observed_object[key], child, depth + 1, found
                )
                or bounded
            )
        return bounded
    previous_array = _json_array(previous)
    observed_array = _json_array(observed)
    if (
        previous_array is not None
        and observed_array is not None
        and len(previous_array) == len(observed_array)
    ):
        bounded = False
        for index, (left, right) in enumerate(zip(previous_array, observed_array)):
            bounded = (
                _collect_json_changes(
                    left, right, f"{path}[{index}]", depth + 1, found
                )
                or bounded
            )
        return bounded
    found.append(path or ".")
    return False


def _json_change_paths(
    previous_body: str, observed_body: str
) -> tuple[tuple[str, ...], bool]:
    """Name the JSON fields that changed inside one text column, or nothing if it is not JSON."""

    if (
        len(previous_body) > _PREFIX_DIFF_MAX_JSON_CHARS
        or len(observed_body) > _PREFIX_DIFF_MAX_JSON_CHARS
    ):
        return (), False
    try:
        previous_value: object = json.loads(previous_body)
        observed_value: object = json.loads(observed_body)
    except (json.JSONDecodeError, RecursionError, UnicodeDecodeError, ValueError):
        return (), False
    if _json_object(previous_value) is None and _json_array(previous_value) is None:
        return (), False
    if _json_object(observed_value) is None and _json_array(observed_value) is None:
        return (), False
    found: list[str] = []
    try:
        bounded = _collect_json_changes(previous_value, observed_value, "", 0, found)
    except RecursionError:
        return (), True
    return tuple(found), bounded


def _prefix_column_change(
    column: str, previous_value: object, observed_value: object
) -> OrcPrefixColumnChange:
    """Describe one changed column as a bounded, self-explaining before/after pair."""

    previous_kind, previous_body = _prefix_value(previous_value)
    observed_kind, observed_body = _prefix_value(observed_value)
    json_paths: tuple[str, ...] = ()
    json_paths_bounded = False
    if previous_kind == observed_kind:
        previous_excerpt, observed_excerpt, bounded = _prefix_excerpts(
            previous_body, observed_body
        )
        if previous_kind == "text":
            json_paths, json_paths_bounded = _json_change_paths(
                previous_body, observed_body
            )
    else:
        previous_excerpt, previous_bounded = _prefix_window(
            previous_body, 0, len(previous_body)
        )
        observed_excerpt, observed_bounded = _prefix_window(
            observed_body, 0, len(observed_body)
        )
        bounded = previous_bounded or observed_bounded
    return OrcPrefixColumnChange(
        column=column,
        previous=_prefix_display(previous_kind, previous_excerpt),
        observed=_prefix_display(observed_kind, observed_excerpt),
        bounded=bounded,
        json_paths=json_paths,
        json_paths_bounded=json_paths_bounded,
    )


def _prefix_display(kind: str, excerpt: str) -> str:
    return "null" if kind == "null" else f"{kind}:{excerpt}"


@dataclass(frozen=True)
class _PrefixDiff:
    """Bounded evidence about how a live source's append prefix differs from its snapshot."""

    changed_rows: tuple[OrcPrefixRowChange, ...]
    changed_row_count: int
    missing_rows: tuple[str, ...]
    missing_row_count: int
    added_rows: tuple[str, ...]
    added_row_count: int

    @property
    def changed_rows_bounded(self) -> bool:
        """Return whether more rows changed than the record names."""

        return len(self.changed_rows) < self.changed_row_count


def _append_prefix_diff(
    previous_path: Path, current_path: Path, scopes: Sequence[_PrefixScope]
) -> _PrefixDiff:
    """Walk both prefixes in key order and report, boundedly, exactly how they differ.

    Both sides are streamed rather than loaded: the guarded prefix of a real coordinator archive is
    hundreds of megabytes, and this runs on the failure path of a run that is already slow. Only
    rows that actually differ are ever rendered, and only the first `_PREFIX_DIFF_MAX_ROWS` of
    those are kept -- the counts are exact regardless.
    """

    changed: list[OrcPrefixRowChange] = []
    changed_count = 0
    missing: list[str] = []
    missing_count = 0
    added: list[str] = []
    added_count = 0
    previous_connection = _read_only(previous_path)
    try:
        current_connection = _read_only(current_path)
        try:
            for scope in scopes:
                previous_cursor = previous_connection.execute(
                    scope.query, (scope.limit,)
                )
                current_cursor = current_connection.execute(
                    scope.query, (scope.limit,)
                )
                previous_raw = next(previous_cursor, None)
                current_raw = next(current_cursor, None)
                where = f"{scope.table} prefix diff"
                while previous_raw is not None or current_raw is not None:
                    previous_row = (
                        None if previous_raw is None else _row(previous_raw, where)
                    )
                    current_row = (
                        None if current_raw is None else _row(current_raw, where)
                    )
                    previous_key = (
                        None
                        if previous_row is None
                        else _integer(previous_row[0], f"{where}: key")
                    )
                    current_key = (
                        None
                        if current_row is None
                        else _integer(current_row[0], f"{where}: key")
                    )
                    if current_key is None or (
                        previous_key is not None and previous_key < current_key
                    ):
                        missing_count += 1
                        if len(missing) < _PREFIX_DIFF_MAX_KEYS:
                            missing.append(f"{scope.table} row {previous_key}")
                        previous_raw = next(previous_cursor, None)
                        continue
                    if previous_key is None or current_key < previous_key:
                        added_count += 1
                        if len(added) < _PREFIX_DIFF_MAX_KEYS:
                            added.append(f"{scope.table} row {current_key}")
                        current_raw = next(current_cursor, None)
                        continue
                    if previous_row is not None and current_row is not None:
                        if not _prefix_rows_equal(previous_row, current_row):
                            changed_count += 1
                            if len(changed) < _PREFIX_DIFF_MAX_ROWS:
                                changed.append(
                                    OrcPrefixRowChange(
                                        table=scope.table,
                                        row_id=previous_key,
                                        columns=tuple(
                                            _prefix_column_change(
                                                name,
                                                previous_row[index],
                                                current_row[index],
                                            )
                                            for index, name in enumerate(scope.columns)
                                            if not _prefix_values_equal(
                                                previous_row[index],
                                                current_row[index],
                                            )
                                        ),
                                    )
                                )
                    previous_raw = next(previous_cursor, None)
                    current_raw = next(current_cursor, None)
        finally:
            current_connection.close()
    except sqlite3.Error as error:
        raise OrcParseError(
            f"failed to compare Orc append prefixes for {current_path}: {error}"
        ) from error
    finally:
        previous_connection.close()
    return _PrefixDiff(
        changed_rows=tuple(changed),
        changed_row_count=changed_count,
        missing_rows=tuple(missing),
        missing_row_count=missing_count,
        added_rows=tuple(added),
        added_row_count=added_count,
    )


def _prefix_scopes_for(
    path: Path, session_state_mode: str | None, prefix_max_id: int
) -> tuple[_PrefixScope, ...]:
    """Return the scopes the guard digested for *path* at the recorded watermark."""

    connection = _read_only(path)
    try:
        geometry = _session_geometry(connection, path, session_state_mode)
    except sqlite3.Error as error:
        raise OrcParseError(
            f"failed to inspect Orc SQLite source {path}: {error}"
        ) from error
    finally:
        connection.close()
    scopes, _ = _session_prefix_scopes(geometry, session_state_mode, prefix_max_id)
    return scopes


def _resolve_prefix_rewrite(
    relative: str,
    kind: str,
    session_id: str,
    previous: OrcSourceCopy,
    previous_state: _LogicalState,
    observed: _LogicalState,
    diff: _PrefixDiff,
    accept_prefix_rewrite: frozenset[str],
    captured_at: str,
) -> OrcAppendPrefixOverride:
    """Refuse a prefix rewrite, or record an operator's explicit acceptance of it.

    Acceptance is scoped to the one session the operator was shown, which is why this takes a set
    of authorized session ids and not a boolean. A lineage is a tree -- a root coordinator plus
    every nested session the index reaches -- and the guard runs once per session in it, so a bare
    boolean would extend one session's diagnosed backfill to every other session in the tree. That
    is the same argument :func:`wrkviz.project_config._prefix_rewrite_selection` makes
    one level up for teams, and it bites harder here: the operator is *structurally* incapable of
    having seen the other sessions, because the first mismatching source raises and ends the run,
    so a second genuinely rewritten session has never been printed at the moment the flag is
    passed. Naming the session makes the authorization say only what was actually inspected.

    Refusals therefore arrive one per run rather than all at once, and that is deliberate. Batching
    them would mean preparing every candidate in the lineage -- a full consistent backup of every
    session and task database -- on a run that is going to fail anyway, purely to enrich a message.
    The order is deterministic (sources are visited sorted by path), so the sequence terminates:
    each re-run surfaces the next rewritten session with its own evidence, and the operator adds
    one more session id having actually read that session's diff.

    The diff is computed and reported whether or not the override is in play, because the operator
    has to decide *before* passing the flag and the only alternative would be enabling the override
    in order to discover what it would accept -- exactly backwards for a safety valve. The extra
    scan costs a pass over a prefix the run has already digested twice, on a run that is failing
    anyway.

    For the same reason the refusal states the *price* of the flag it recommends, not just its
    name. The refusal is where the decision is actually made; a caveat that lives only in the user
    guide is a caveat the operator reads after acting, if at all. So the message says which snapshot
    object acceptance supersedes and how long that copy is kept, and it says the recorded diff is a
    summary by quoting the two caps that apply -- rows kept out of rows changed, and characters per
    column value. An operator who reads "1 row(s) changed" and nothing else cannot tell that the
    evidence in exchange is lossy; one who reads "20 of 25" and "160 characters" can.

    Two things the override deliberately does not cover, both raised regardless of the flag. The
    first is the prefix gaining or losing a row: a row that disappeared is record loss, which is
    the whole reason the guard exists, and a row that appeared behind the watermark is history
    inserted after the fact -- a different and more serious event than a metadata backfill. These
    are reported together rather than in sequence because they arrive together: the count guard
    above already refuses an unbalanced prefix, so the only way either reaches here is a deletion
    and an insertion cancelling out. The second is a digest that moved with no row to show for it,
    which means this code's own evidence disagrees with its own digest; accepting a rewrite nobody
    can describe would make the recorded override worthless.
    """

    prefix_error = (
        f"Orc {kind} existing append prefix was rewritten for {relative}"
    )
    if diff.missing_row_count or diff.added_row_count:
        raise OrcParseError(
            f"{prefix_error}: the recorded prefix lost {diff.missing_row_count} row(s) "
            f"({', '.join(diff.missing_rows) or 'none'}) and gained "
            f"{diff.added_row_count} row(s) "
            f"({', '.join(diff.added_rows) or 'none'}) below its watermark; accepting a "
            "prefix rewrite covers changed column values only, never rows appearing or "
            "disappearing"
        )
    if diff.changed_row_count == 0:
        raise OrcParseError(
            f"{prefix_error}: the digest moved but no row differs, so the archive cannot "
            "describe what changed"
        )
    if session_id not in accept_prefix_rewrite:
        described: list[str] = []
        budget = _PREFIX_DIFF_MAX_MESSAGE_CHARS
        for row in diff.changed_rows:
            line = row.describe()
            if budget - len(line) < 0 and described:
                break
            described.append(line)
            budget -= len(line)
        remaining = diff.changed_row_count - len(described)
        evidence = "; ".join(described)
        if remaining:
            evidence += f"; ... {remaining} further changed row(s) not shown"
        raise OrcParseError(
            f"{prefix_error}: {diff.changed_row_count} row(s) changed at or below the "
            f"append watermark [{evidence}]; re-run with "
            f"--accept-orc-prefix-rewrite {session_id} to record this rewrite and "
            "re-baseline the digest -- that authorizes this one session and no other, so "
            "another rewritten session in this lineage refuses again with its own evidence "
            "-- accepting supersedes the pre-rewrite snapshot "
            f"{previous.snapshot_path} (kept for comparison until the next accepted "
            "override on this source, then reclaimed) and records only a bounded summary "
            f"-- at most {_PREFIX_DIFF_MAX_ROWS} of those {diff.changed_row_count} changed "
            f"row(s), at most {_PREFIX_DIFF_EXCERPT_CHARS} characters per column value"
        )
    prior = previous.append_prefix_override
    return OrcAppendPrefixOverride(
        policy=_PREFIX_OVERRIDE_POLICY,
        source_path=relative,
        accepted_at=captured_at,
        override_count=1 if prior is None else prior.override_count + 1,
        previous_append_prefix_sha256=previous_state.append_prefix_sha256,
        observed_append_prefix_sha256=observed.append_prefix_sha256,
        # The object the manifest is about to stop pointing at. Recording it *is* retaining it:
        # object GC keeps what the manifest references, so this assignment is the difference
        # between an operator having a route back and not having one.
        superseded_snapshot_path=previous.snapshot_path,
        superseded_sha256=previous.sha256,
        changed_row_count=diff.changed_row_count,
        changed_rows=diff.changed_rows,
        changed_rows_bounded=diff.changed_rows_bounded,
        degraded=True,
        degradation_reason=_PREFIX_OVERRIDE_DEGRADATION_REASON,
    )


def _prepare_snapshot_candidate(
    source_root: Path,
    snapshot_root: Path,
    discovered: _DiscoveredSource,
    previous: OrcSourceCopy | None,
    selected_session_ids: frozenset[str],
    temporary: Path,
    captured_at: str,
    accept_prefix_rewrite: frozenset[str],
) -> _PreparedCandidate:
    relative = discovered.source_path
    if previous is None:
        owner_session_id = discovered.owner_candidates[0]
        previous_path: Path | None = None
        source_path = _live_source_path(source_root, relative)
    else:
        if previous.kind != discovered.kind:
            raise OrcParseError(
                f"Orc source kind changed for {relative}: "
                f"{previous.kind!r} to {discovered.kind!r}"
            )
        if (
            previous.owner_session_id not in discovered.owner_candidates
            and previous.owner_session_id not in selected_session_ids
        ):
            raise OrcParseError(
                f"Orc source ownership changed for {relative}: prior owner "
                f"{previous.owner_session_id!r} no longer references the source"
            )
        owner_session_id = previous.owner_session_id
        previous_path = _snapshot_path(snapshot_root, previous.snapshot_path)
        if (
            previous_path.is_symlink()
            or not previous_path.is_file()
            or _sha256_file(previous_path) != previous.sha256
        ):
            raise OrcParseError(
                f"previous Orc snapshot does not match its manifest: {previous_path}"
            )
        source_path = (
            previous_path
            if discovered.source_state == _SOURCE_STATE_DETACHED
            else _live_source_path(source_root, relative)
        )
    _backup_database(source_path, temporary, snapshot_root)
    state = _logical_state(temporary, discovered.kind)
    previous_state: _LogicalState | None = None
    previous_session_mode: str | None = None
    prefix_override: OrcAppendPrefixOverride | None = None
    if previous is not None:
        if previous_path is None:
            raise AssertionError("validated previous snapshot path is missing")
        if (
            discovered.kind == "session"
            and previous.append_max_id & _SESSION_STATE_TAG == 0
        ):
            candidates: list[tuple[str, _LogicalState]] = [
                (
                    "content-only",
                    _logical_state(
                        previous_path,
                        discovered.kind,
                        session_state_mode="content-only",
                    ),
                )
            ]
            connection = _read_only(previous_path)
            try:
                has_messages = "messages" in _table_names(
                    connection, str(previous_path)
                )
            finally:
                connection.close()
            if has_messages:
                candidates.append(
                    (
                        "messages-only",
                        _logical_state(
                            previous_path,
                            discovered.kind,
                            session_state_mode="messages-only",
                        ),
                    )
                )
            matches = [
                (mode, candidate)
                for mode, candidate in candidates
                if (
                    candidate.append_count == previous.append_count
                    and candidate.append_max_id == previous.append_max_id
                    and candidate.append_prefix_sha256
                    == previous.append_prefix_sha256
                )
            ]
            if not matches:
                raise OrcParseError(
                    f"legacy Orc session prefix does not match its manifest: {relative}"
                )
            previous_session_mode, previous_state = matches[-1]
        elif discovered.kind == "task" and previous.task_projection is None:
            legacy_state = _logical_state(
                previous_path, discovered.kind, legacy_task_fields=True
            )
            if (
                legacy_state.append_count != previous.append_count
                or legacy_state.append_max_id != previous.append_max_id
                or legacy_state.append_prefix_sha256 != previous.append_prefix_sha256
            ):
                raise OrcParseError(
                    f"legacy Orc task prefix does not match its manifest: {relative}"
                )
            previous_state = _logical_state(previous_path, discovered.kind)
        else:
            previous_state = _logical_state(previous_path, discovered.kind)
            _validate_manifest_logical_state(previous, previous_state, relative)
        if discovered.kind == "session":
            prefix = _logical_state(
                temporary,
                discovered.kind,
                prefix_max_id=previous.append_max_id,
                session_state_mode=previous_session_mode,
            )
            if state.append_count < previous.append_count:
                raise OrcParseError(
                    f"Orc {discovered.kind} append history shrank for {relative}: "
                    f"{previous.append_count} to {state.append_count} rows"
                )
            if prefix.append_count != previous.append_count:
                raise OrcParseError(
                    f"Orc {discovered.kind} append prefix lost rows for {relative}"
                )
            # Only the *digest* comparison is overridable. The two guards above stay absolute
            # because both of them mean rows are gone or unaccounted for, and no operator flag in
            # this tool is allowed to wave through record loss -- the override exists for the
            # narrow case of an upstream metadata backfill rewriting a column in place.
            if prefix.append_prefix_sha256 != previous_state.append_prefix_sha256:
                prefix_override = _resolve_prefix_rewrite(
                    relative,
                    discovered.kind,
                    # The owner session id, not the directory name parsed out of the path: for a
                    # session source with a previous record these are the same string (discovery
                    # refuses a session directory whose database says otherwise), and taking it
                    # from the manifest keeps the authorization keyed to the identity the archive
                    # already committed to rather than to a path the operator could rename.
                    owner_session_id,
                    previous,
                    previous_state,
                    prefix,
                    _append_prefix_diff(
                        previous_path,
                        temporary,
                        _prefix_scopes_for(
                            temporary, previous_session_mode, previous.append_max_id
                        ),
                    ),
                    accept_prefix_rewrite,
                    captured_at,
                )
    return _PreparedCandidate(
        discovered,
        previous,
        owner_session_id,
        source_path,
        previous_path,
        temporary,
        state,
        previous_state,
        prefix_override,
    )


def _advance_session_candidate(
    candidate: _PreparedCandidate,
    snapshot_root: Path,
    captured_at: str,
) -> _SourceAdvance:
    relative = candidate.discovered.source_path
    current_meta = _session_meta(candidate.temporary_path, relative)
    if current_meta.session_id != candidate.owner_session_id:
        raise OrcParseError(
            f"Orc session identity changed for {relative}: "
            f"{candidate.owner_session_id!r} to {current_meta.session_id!r}"
        )
    current_auxiliary = _auxiliary_observation(candidate.temporary_path, current_meta)
    rewrite_detected = False
    rewrite_count = 0
    last_rewrite_at: str | None = None
    degraded = False
    previous_identity: _SemanticIdentity | None = None
    previous = candidate.previous
    if previous is not None:
        if candidate.previous_path is None or candidate.previous_state is None:
            raise AssertionError("validated previous session state is missing")
        previous_meta = _session_meta(candidate.previous_path, relative)
        _validate_session_meta_extension(previous_meta, current_meta, relative)
        previous_auxiliary = _auxiliary_observation(
            candidate.previous_path, previous_meta
        )
        if previous.auxiliary.policy == _AUXILIARY_POLICY_LEGACY:
            if (
                previous.auxiliary.message_count != len(previous_auxiliary.messages)
                or previous.auxiliary.message_sha256
                != previous_auxiliary.message_sha256
            ):
                raise OrcParseError(
                    "previous Orc auxiliary message evidence does not match "
                    f"its snapshot for {relative}"
                )
            previous_identity = _session_semantic_identity(
                relative,
                candidate.owner_session_id,
                _session_canonical_state(candidate.previous_path, previous_meta),
                previous_meta,
                previous_auxiliary,
                4,
            )
            _validate_legacy_semantic_identity(
                snapshot_root, previous, previous_identity, None
            )
        else:
            if previous.append_max_id & _SESSION_STATE_TAG:
                previous_identity = _validate_source_semantic_identity(
                    snapshot_root, previous
                )
            else:
                previous_legacy_identity = _session_semantic_identity(
                    relative,
                    candidate.owner_session_id,
                    candidate.previous_state,
                    previous_meta,
                    previous_auxiliary,
                    2,
                )
                previous_identity = _session_semantic_identity(
                    relative,
                    candidate.owner_session_id,
                    _session_canonical_state(
                        candidate.previous_path, previous_meta
                    ),
                    previous_meta,
                    previous_auxiliary,
                    4,
                )
                if (
                    previous.semantic_identity_mode
                    == _SEMANTIC_IDENTITY_DETERMINISTIC
                    and (
                        previous.semantic_sha256,
                        previous.semantic_complete_bytes,
                    )
                    != (
                        previous_identity.sha256,
                        previous_identity.complete_bytes,
                    )
                ):
                    _validate_recorded_semantic_identity(
                        previous, previous_legacy_identity, relative
                    )
                _validate_legacy_semantic_identity(
                    snapshot_root, previous, previous_identity, None
                )
        if (
            candidate.previous_state.append_max_id & _SESSION_STATE_TAG == 0
            and candidate.state.append_max_id & _SESSION_STATE_TAG
        ):
            _validate_stable_spawn_storage_migration(
                previous_auxiliary, current_auxiliary, relative
            )
        else:
            _validate_stable_spawn_extension(
                previous_auxiliary, current_auxiliary, relative
            )
        rewrite_detected = not _conversation_is_append_extension(
            previous_auxiliary, current_auxiliary
        )
        rewrite_count = previous.auxiliary.rewrite_count + int(rewrite_detected)
        last_rewrite_at = (
            captured_at if rewrite_detected else previous.auxiliary.last_rewrite_at
        )
        degraded = previous.auxiliary.degraded or rewrite_detected
    auxiliary = OrcAuxiliaryStatus(
        policy=_AUXILIARY_POLICY_STABLE_SPAWNS,
        message_count=len(current_auxiliary.messages),
        message_sha256=current_auxiliary.message_sha256,
        stable_spawn_count=len(current_auxiliary.stable_spawns),
        stable_spawn_sha256=current_auxiliary.stable_spawn_sha256,
        rewrite_count=rewrite_count,
        last_rewrite_at=last_rewrite_at,
        degraded=degraded,
        degradation_reason=_AUXILIARY_DEGRADATION_REASON if degraded else None,
    )
    current_identity = _session_semantic_identity(
        relative,
        candidate.owner_session_id,
        _session_canonical_state(candidate.temporary_path, current_meta),
        current_meta,
        current_auxiliary,
        4,
    )
    return _SourceAdvance(
        auxiliary,
        None,
        None,
        previous_identity,
        current_identity,
        (),
        (),
    )


def _advance_task_candidate(
    candidate: _PreparedCandidate,
    snapshot_root: Path,
    captured_at: str,
    task_source_ordinal: int,
) -> _SourceAdvance:
    relative = candidate.discovered.source_path
    auxiliary = OrcAuxiliaryStatus(
        policy=_AUXILIARY_POLICY_NOT_APPLICABLE,
        message_count=0,
        message_sha256=_EMPTY_SHA256,
        stable_spawn_count=0,
        stable_spawn_sha256=_EMPTY_SHA256,
        rewrite_count=0,
        last_rewrite_at=None,
        degraded=False,
        degradation_reason=None,
    )
    current_records = _observed_task_notes(candidate.temporary_path)
    current_sequence = _task_note_sequence(candidate.temporary_path)
    if len(current_records) != candidate.state.append_count:
        raise OrcParseError(f"task note count does not match its source for {relative}")
    if current_sequence < max((record.note_id for record in current_records), default=0):
        raise OrcParseError(f"task note allocation sequence is invalid for {relative}")
    current_by_id = {record.note_id: record for record in current_records}
    previous_identity: _SemanticIdentity | None = None
    previous = candidate.previous
    if previous is None:
        previous_records: tuple[_FrozenTaskNote, ...] = ()
        prior_allocation_highwater = 0
        prior_rewrite_sha256 = _EMPTY_TASK_REWRITE_SHA256
        prior_rewrite_count = 0
        prior_last_rewrite_at: str | None = None
        prior_degraded = False
    else:
        if candidate.previous_path is None or candidate.previous_state is None:
            raise AssertionError("validated previous task state is missing")
        previous_records = _bootstrap_frozen_task_projection(
            snapshot_root, previous, candidate.previous_path
        )
        previous_sequence = (
            previous.task_projection.observed_note_sequence
            if previous.task_projection is not None
            and previous.task_projection.observed_note_sequence is not None
            else _task_note_sequence(candidate.previous_path)
        )
        prior_allocation_highwater = previous_sequence
        if current_sequence < max(
            previous_sequence,
            max((record.note_id for record in previous_records), default=0),
        ):
            raise OrcParseError(
                f"task note allocation sequence regressed for {relative}: "
                f"{previous_sequence} to {current_sequence}"
            )
        if previous.task_projection is None:
            previous_projection_sha256 = _task_enrichment_sha256(
                previous_records
            )
            previous_identity = _task_semantic_identity(
                relative,
                candidate.owner_session_id,
                task_source_ordinal,
                candidate.previous_state,
                previous_projection_sha256,
            )
            _validate_legacy_semantic_identity(
                snapshot_root,
                previous,
                previous_identity,
                previous_projection_sha256,
            )
        else:
            previous_identity = _validate_source_semantic_identity(
                snapshot_root, previous
            )
        if previous.task_projection is None:
            prior_rewrite_sha256 = _EMPTY_TASK_REWRITE_SHA256
            prior_rewrite_count = 0
            prior_last_rewrite_at = None
            prior_degraded = False
        else:
            prior_rewrite_count = previous.task_projection.rewrite_count
            prior_last_rewrite_at = previous.task_projection.last_rewrite_at
            prior_degraded = previous.task_projection.degraded
            if previous.task_projection.policy == _TASK_HISTORY_POLICY:
                prior_rewrite_sha256 = (
                    previous.task_projection.observed_note_rewrite_sha256
                )
            else:
                prior_current = {
                    record.note_id: record
                    for record in _observed_task_notes(candidate.previous_path)
                }
                prior_rewrite_sha256, _, _ = _task_rewrite_observation(
                    previous_records, prior_current
                )

    previous_by_id = {record.note_id: record for record in previous_records}
    highwater = max(max(previous_by_id, default=0), prior_allocation_highwater)
    merged_by_id = dict(previous_by_id)
    for note_id, observed in current_by_id.items():
        prior = previous_by_id.get(note_id)
        if prior is not None:
            if (
                observed.task_id,
                observed.content,
                observed.created_at,
            ) != (prior.task_id, prior.content, prior.created_at):
                raise OrcParseError(
                    f"task note immutable core was rewritten for note {note_id} "
                    f"in {relative}"
                )
            continue
        if note_id <= highwater:
            raise OrcParseError(
                f"task note ID {note_id} was reused below frozen highwater "
                f"{highwater} in {relative}"
            )
        merged_by_id[note_id] = _freeze_current_task_note(observed, relative)
    merged_records = tuple(merged_by_id[note_id] for note_id in sorted(merged_by_id))
    enrichment_sha256 = _task_enrichment_sha256(merged_records)
    rewrite_sha256, missing_count, missing_ids_sha256 = (
        _task_rewrite_observation(merged_records, current_by_id)
    )
    gap_count, gap_sha256 = _task_unobserved_id_gaps(
        merged_records, current_sequence
    )
    rewrite_detected = previous is not None and rewrite_sha256 != prior_rewrite_sha256
    projection_rewrite_count = prior_rewrite_count + int(rewrite_detected)
    projection_last_rewrite_at = (
        captured_at if rewrite_detected else prior_last_rewrite_at
    )
    projection_degraded = prior_degraded or (
        rewrite_sha256 != _EMPTY_TASK_REWRITE_SHA256
    )
    (
        projection_temporary,
        projection_target,
        projection_sha256,
        projection_relative,
    ) = _stage_task_projection(merged_records, snapshot_root)
    task_projection = OrcTaskProjection(
        policy=_TASK_HISTORY_POLICY,
        path=projection_relative,
        note_count=len(merged_records),
        sha256=projection_sha256,
        observed_enrichment_sha256=enrichment_sha256,
        observed_note_rewrite_sha256=rewrite_sha256,
        missing_note_count=missing_count,
        missing_note_ids_sha256=missing_ids_sha256,
        observed_note_sequence=current_sequence,
        unobserved_note_id_gap_count=gap_count,
        unobserved_note_id_gap_sha256=gap_sha256,
        rewrite_count=projection_rewrite_count,
        last_rewrite_at=projection_last_rewrite_at,
        degraded=projection_degraded,
        degradation_reason=(
            _TASK_HISTORY_DEGRADATION_REASON
            if projection_degraded
            else None
        ),
    )
    current_identity = _task_semantic_identity(
        relative,
        candidate.owner_session_id,
        task_source_ordinal,
        _frozen_task_logical_state(merged_records),
        enrichment_sha256,
    )
    return _SourceAdvance(
        auxiliary,
        task_projection,
        task_source_ordinal,
        previous_identity,
        current_identity,
        ((projection_temporary, projection_target, projection_sha256),),
        (projection_temporary,),
    )


def _publish_staged_objects(
    staged_objects: Sequence[tuple[Path, Path, str]], snapshot_root: Path
) -> int:
    changed = 0
    for temporary, target, sha256 in staged_objects:
        changed += int(
            _publish_snapshot_candidate(
                temporary, target, sha256, snapshot_root
            )
        )
    return changed


def _session_record_timestamp(
    connection: sqlite3.Connection, source_line: int, where: str, table: str
) -> int | None:
    """Read one row's timestamp from *table*, or ``None`` when that ordinal has no row there.

    The table is a parameter rather than something this function decides. It used to decide, by
    asking `_session_storage_table` which storage layout the database currently presents, and that
    is precisely the bug this signature exists to make unrepresentable: a stored ordinal was written
    against whichever table Orc was using *then*, and re-deriving the table *now* silently answers a
    different question the moment the database has been migrated underneath the recorded link.
    """

    identity = _CONTINUATION_BOUNDARY_IDENTITY.get(table)
    if identity is None:
        raise OrcParseError(f"{where}: unsupported Orc boundary table {table!r}")
    raw = connection.execute(
        f"SELECT created_at_ms FROM {table} WHERE {identity} = ?",
        (source_line,),
    ).fetchone()
    if raw is None:
        return None
    return _integer(_row(raw, where)[0], f"{where}: created_at_ms")


def _resolve_continuation_boundary(
    connection: sqlite3.Connection,
    link: OrcContinuationLink,
    index: int,
    where: str,
) -> str | None:
    """Confirm a recorded boundary still holds, and say which table its ordinal indexes.

    A link written since `predecessor_source_table` existed names its own table, so this is a single
    lookup and any disagreement is real. A link written before it names no table, and the ordinal
    alone cannot be resolved -- so this refuses to *choose* a table and instead makes the recorded
    evidence do the choosing: try the ordinal in every candidate table the database actually has and
    keep the ones whose row carries exactly the recorded `predecessor_at_ms`.

    Three things were considered and rejected for that migration case:

    * **Ask `_session_storage_table` which layout the database presents.** This is the behaviour
      being fixed. It is not merely imprecise, it is unfixable in principle: one measured lineage
      spans Orc's storage transition and has two links whose ordinals resolve in *different* tables
      -- one in `content_blocks`, one in `messages` -- with both tables present in both predecessor
      databases. Whichever table a single global answer picks, the other link is wrong.
    * **Prefer one table and fall back to the other.** A preference silently picks a table whenever
      the ordinal happens to exist in both, and "the row exists" is a far weaker coincidence than it
      sounds: `content_blocks` and `messages` both start at 1 and both grow monotonically, so small
      ordinals exist in both essentially always. On the measured lineage, ordinal 1762 exists in both
      tables of the same database with *different* timestamps -- 1786759893720 in `messages`, which
      is the recorded one, and 1786432513136 in `content_blocks`, which is nearly four days off.
      Preference would have taken whichever table it was told to like and reported a false failure or,
      worse, a false success.
    * **Refuse the ingest until an operator names the table.** This turns a resolvable, evidence-backed
      question into an interactive one, on a path an operator reaches only because their data is
      intact. The evidence is already in the manifest; asking a human to retype it adds a way to be
      wrong and removes none.

    Exactly one agreeing table is adopted and returned so the next run is unambiguous. Zero agreeing
    tables is a real refusal: the recorded evidence is nowhere in this database. More than one
    agreeing table means both readings assert the same thing, so the boundary itself is confirmed --
    but which table the ordinal *came from* is genuinely undetermined, and this returns ``None``
    rather than freeze a coin flip into the manifest as though it had been observed.

    **What the adopted table is, and is not, evidence of.** "Exactly one table holds this ordinal at
    this instant" is a reading of the database in front of us, not a proof of provenance. A
    predecessor whose true table was compacted -- the ordinal no longer present there -- while the
    other table happens to carry that instant at that ordinal would be adopted under the other
    table's name. The alternative rule, refusing whenever provenance cannot be *proved*, refuses
    every un-tabled link in existence, which is the entire migration population and the exact false
    refusal this function replaces. What is not weakened either way is the thing the field is used
    for: `predecessor_at_ms` is confirmed against a real row before anything is adopted, and it is
    the instant -- not the table -- that the boundary means. `predecessor_source_table` earns its
    place by making the *next* read a single unambiguous lookup, and the append-prefix digest guard
    refuses the predecessor mutations that would be needed to reach the compacted state at all.
    """

    tables = _table_names(connection, where)
    recorded_table = link.predecessor_source_table
    if recorded_table is not None:
        if recorded_table not in tables:
            raise OrcParseError(
                f"recorded Orc continuation {index} boundary table "
                f"{recorded_table!r} is absent from {where}"
            )
        observed = _session_record_timestamp(
            connection, link.predecessor_source_line, where, recorded_table
        )
        if observed is None:
            raise OrcParseError(
                f"recorded Orc continuation {index} boundary row disappeared from "
                f"{recorded_table}"
            )
        if observed != link.predecessor_at_ms:
            raise OrcParseError(
                f"recorded Orc continuation {index} boundary evidence changed"
            )
        return recorded_table
    observations = tuple(
        (
            table,
            _session_record_timestamp(
                connection, link.predecessor_source_line, where, table
            ),
        )
        for table in _CONTINUATION_BOUNDARY_IDENTITY
        if table in tables
    )
    agreeing = tuple(
        table
        for table, observed in observations
        if observed == link.predecessor_at_ms
    )
    if not agreeing:
        detail = ", ".join(
            f"{table}={'absent' if observed is None else observed}"
            for table, observed in observations
        )
        raise OrcParseError(
            f"recorded Orc continuation {index} boundary evidence changed: row "
            f"{link.predecessor_source_line} was recorded at "
            f"{link.predecessor_at_ms} ms and the link names no table, but no "
            f"candidate table in {where} still holds that row "
            f"({detail or 'no candidate table present'})"
        )
    if len(agreeing) > 1:
        return None
    return agreeing[0]


def _latest_session_record_before(
    connection: sqlite3.Connection, before_ms: int, where: str
) -> tuple[str, int, int] | None:
    """Return the table, ordinal and timestamp of the last content record before *before_ms*.

    The table travels with the ordinal from the moment the boundary is first derived, because this
    is the only point in the program where the pairing is known for free. Deriving the ordinal here
    and re-deriving the table at every later read is what produced a guard that refused intact data.

    **Every candidate table is asked, not the one the database currently looks like.** This used to
    call `_session_storage_table`, which answers "does a `messages` table exist?" -- and existing is
    not holding rows. A database caught mid-transition has an empty or nearly-empty `messages`
    beside a `content_blocks` that still holds the entire transcript, so the single-table question
    returned `messages`, found nothing before the successor's start, and the caller refused a
    lineage whose predecessor content was sitting untouched in the other table. The weaker variant
    is worse because it does not announce itself: `messages` holding *one* early row made the
    boundary resolve to that row, silently skipping every later record in `content_blocks` and then
    freezing the wrong pair into the manifest as though it had been observed. Duck-typing the
    layout is the same mistake `_resolve_continuation_boundary` was written to stop making; there is
    no reason for the derivation side to keep making it.

    A tie -- two tables whose latest qualifying row shares the maximum instant -- is broken by the
    declared order in :data:`_CONTINUATION_BOUNDARY_IDENTITY`, and that is *not* the coin flip the
    resolver refuses to make. The resolver is asked an unobservable historical question ("which
    table was this recorded ordinal written against?") and declines to guess. This function is
    asked a present-tense one ("which record is the last one before the successor started?") and
    both candidates are true answers, observed now, in a table named now. Choosing between two
    correct observations is a choice; inventing provenance for one is not.
    """

    tables = _table_names(connection, where)
    candidates = [name for name in _CONTINUATION_BOUNDARY_IDENTITY if name in tables]
    if not candidates:
        # A backstop, not a live path: `_require_tables` refuses a session database missing its
        # transcript table long before a continuation boundary is derived from it. It is here
        # anyway because the alternative -- falling out of the loop with `best is None` -- would
        # report "no predecessor content record before its start" and send a reader looking for a
        # missing message when what is missing is the schema. The two diagnoses lead to different
        # places, so they are different refusals even when only one of them can fire today.
        raise OrcParseError(
            f"{where}: holds none of the Orc transcript tables "
            f"({', '.join(_CONTINUATION_BOUNDARY_IDENTITY)})"
        )
    best: tuple[str, int, int] | None = None
    for table in candidates:
        identity = _CONTINUATION_BOUNDARY_IDENTITY[table]
        raw = connection.execute(
            f"SELECT {identity}, created_at_ms FROM {table} "
            "WHERE created_at_ms > 0 AND created_at_ms < ? "
            f"ORDER BY created_at_ms DESC, {identity} DESC LIMIT 1",
            (before_ms,),
        ).fetchone()
        if raw is None:
            continue
        row = _row(raw, where)
        candidate = (
            table,
            _integer(row[0], f"{where}: source line"),
            _integer(row[1], f"{where}: created_at_ms"),
        )
        # Strictly greater, so the declared order breaks a tie and the first table listed wins.
        if best is None or candidate[2] > best[2]:
            best = candidate
    return best


def _continuation_links(
    root_session_id: str,
    continuation_specs: Sequence[OrcContinuationSpec],
    session_databases: Mapping[str, Path],
    previous: Sequence[OrcContinuationLink],
) -> tuple[OrcContinuationLink, ...]:
    """Freeze one reproducible transcript boundary for each explicit successor."""

    roots = (root_session_id, *(spec.session_id for spec in continuation_specs))
    expected_pairs = tuple(zip(roots, continuation_specs))
    if len(previous) > len(expected_pairs):
        raise OrcParseError(
            "source manifest records more Orc continuations than were configured"
        )
    result: list[OrcContinuationLink] = []
    prior_effective_start = _session_meta(
        session_databases[f".orc/sessions/{root_session_id}/session.db"],
        f".orc/sessions/{root_session_id}/session.db",
    ).created_at_ms
    for index, (predecessor_id, spec) in enumerate(expected_pairs):
        successor_id = spec.session_id
        predecessor_source_path = f".orc/sessions/{predecessor_id}/session.db"
        successor_source_path = f".orc/sessions/{successor_id}/session.db"
        predecessor_path = session_databases.get(predecessor_source_path)
        successor_path = session_databases.get(successor_source_path)
        if predecessor_path is None or successor_path is None:
            raise OrcParseError(
                "configured Orc continuation lacks its root session snapshot"
            )
        successor_meta = _session_meta(successor_path, successor_source_path)
        start_source_line, started_at_ms = _resolve_continuation_start(
            successor_path, successor_meta, spec
        )
        if started_at_ms <= prior_effective_start:
            raise OrcParseError(
                "Orc root and continuation sessions are not chronologically ordered"
            )
        prior_effective_start = started_at_ms
        connection = _read_only(predecessor_path)
        try:
            if index < len(previous):
                link = previous[index]
                if (
                    link.predecessor_session_id != predecessor_id
                    or link.session_id != successor_id
                    or link.predecessor_source_path != predecessor_source_path
                    or link.source_path != successor_source_path
                    or link.start_message_id != spec.start_message_id
                    or link.start_source_line != start_source_line
                    or link.started_at_ms != started_at_ms
                ):
                    raise OrcParseError(
                        f"recorded Orc continuation {index} no longer matches "
                        "its configured sessions"
                    )
                resolved_table = _resolve_continuation_boundary(
                    connection, link, index, str(predecessor_path)
                )
                # Adopting the resolved table here is what makes the migration happen exactly once.
                # The link is otherwise reused byte for byte, so a lineage whose links already name
                # their tables rewrites nothing and the ingest stays idempotent.
                result.append(
                    link
                    if resolved_table == link.predecessor_source_table
                    else replace(link, predecessor_source_table=resolved_table)
                )
                continue
            boundary = _latest_session_record_before(
                connection, started_at_ms, str(predecessor_path)
            )
            if boundary is None:
                raise OrcParseError(
                    f"Orc continuation {successor_id!r} has no predecessor "
                    "content record before its start"
                )
            (
                predecessor_source_table,
                predecessor_source_line,
                predecessor_at_ms,
            ) = boundary
        except sqlite3.Error as error:
            raise OrcParseError(
                f"failed to inspect Orc continuation boundary at "
                f"{predecessor_path}: {error}"
            ) from error
        finally:
            connection.close()
        gap_ms = started_at_ms - predecessor_at_ms
        if predecessor_source_line <= 0 or gap_ms <= 0:
            raise OrcParseError(
                f"Orc continuation {successor_id!r} does not follow its predecessor"
            )
        result.append(
            OrcContinuationLink(
                predecessor_session_id=predecessor_id,
                session_id=successor_id,
                predecessor_source_path=predecessor_source_path,
                predecessor_source_line=predecessor_source_line,
                predecessor_source_table=predecessor_source_table,
                predecessor_at_ms=predecessor_at_ms,
                source_path=successor_source_path,
                start_message_id=spec.start_message_id,
                start_source_line=start_source_line,
                started_at_ms=started_at_ms,
                gap_ms=gap_ms,
            )
        )
    return tuple(result)


def _accepted_prefix_rewrite_sessions(
    requested: Sequence[str], lineage_session_ids: frozenset[str]
) -> frozenset[str]:
    """Validate the per-session append-prefix override list against this lineage's sessions.

    Every near miss is rejected here, before a single database is copied, rather than quietly
    authorizing nothing: a repeated id, an id that is not a legal session component at all, and an
    id for a session outside the lineage this run snapshots. The reasoning is the same one
    :func:`wrkviz.project_config._prefix_rewrite_selection` records for teams -- an
    override that looks accepted but had no effect is the worst outcome available to a safety
    valve, because the operator reads their own command line back and believes it did something.

    A session that *is* in the lineage but turns out to have a clean prefix is deliberately not an
    error. The operator names a session having read that session's refusal, and between that
    refusal and the re-run the upstream writer may well have been rolled back, or the flag may be
    carried across two runs of a lineage where only one of them needed it. Refusing that would
    punish the honest case; the run receipt records what was *permitted* separately from what
    actually fired, which is what makes an unused authorization visible.
    """

    if len(set(requested)) != len(requested):
        raise OrcParseError(
            "--accept-orc-prefix-rewrite selection contains duplicate session ids"
        )
    for session_id in requested:
        _safe_component(session_id, "accepted Orc prefix-rewrite session id")
    unknown = sorted(set(requested) - lineage_session_ids)
    if unknown:
        raise OrcParseError(
            "--accept-orc-prefix-rewrite names sessions outside this lineage: "
            + ", ".join(unknown)
        )
    return frozenset(requested)


def snapshot_orc_lineage(
    source_root: Path,
    root_session_id: str,
    snapshot_root: Path,
    previous_sources: Sequence[OrcSourceCopy],
    captured_at: str,
    continuation_specs: Sequence[str | OrcContinuationSpec] = (),
    previous_continuations: Sequence[OrcContinuationLink] = (),
    accept_prefix_rewrite: Sequence[str] = (),
) -> OrcSnapshotResult:
    """Publish immutable SQLite objects after validating provider-specific monotonicity.

    ``accept_prefix_rewrite`` is an operator override, empty by default, and it relaxes exactly one
    check for exactly the sessions it names: a named session whose recorded append prefix digests
    differently is re-baselined and recorded as degraded instead of refusing the whole lineage. A
    session the list does not name is untouched by the flag and still refuses. Rows that vanished
    from or appeared inside the prefix still refuse either way; see :func:`_resolve_prefix_rewrite`
    for why the authorization is per session rather than per run.
    """

    specs = _continuation_specs(continuation_specs)
    _lineage_roots(root_session_id, tuple(spec.session_id for spec in specs))
    plan = _plan_snapshot_sources(
        source_root,
        root_session_id,
        snapshot_root,
        previous_sources,
        specs,
    )
    discovered = plan.sources
    previous_by_path = plan.previous_by_path
    selected_session_ids = plan.selected_session_ids
    task_ordinals = plan.task_ordinals
    # Validated against the discovered lineage, so a mistyped id is caught here rather than being
    # silently inert; the plan does no writing, so this still happens before anything is staged.
    accepted_sessions = _accepted_prefix_rewrite_sessions(
        accept_prefix_rewrite, selected_session_ids
    )

    staged_objects: list[tuple[Path, Path, str]] = []
    result_sources: list[OrcSourceCopy] = []
    temporary_paths: list[Path] = []
    temporary_by_source: dict[str, Path] = {}
    accepted_overrides: list[OrcAppendPrefixOverride] = []
    try:
        for discovered_source in discovered:
            relative = discovered_source.source_path
            kind = discovered_source.kind
            previous = previous_by_path.get(relative)
            staging_root = snapshot_root / ".staging"
            temporary = staging_root / (
                f"orc-{os.getpid()}-{secrets.token_hex(8)}.db"
            )
            temporary_paths.append(temporary)
            temporary_by_source[relative] = temporary
            candidate = _prepare_snapshot_candidate(
                source_root,
                snapshot_root,
                discovered_source,
                previous,
                selected_session_ids,
                temporary,
                captured_at,
                accepted_sessions,
            )
            if candidate.prefix_override is not None:
                accepted_overrides.append(candidate.prefix_override)
            # Sticky across later clean ingests: a source that was ever re-baselined keeps saying
            # so. Dropping the record the moment the source went back to appending cleanly would
            # erase exactly the fact a reader needs when the normalized data looks odd later.
            append_prefix_override = candidate.prefix_override or (
                previous.append_prefix_override if previous is not None else None
            )
            owner_session_id = candidate.owner_session_id
            previous_target = candidate.previous_path
            source_path = candidate.source_path
            full = candidate.state
            previous_state = candidate.previous_state
            if kind == "session":
                staged_meta = _session_meta(temporary, relative)
                staged_task_paths = frozenset(
                    path
                    for path, _, _ in _session_task_relatives(
                        temporary, staged_meta, root_session_id
                    )
                    if not any(
                        spec.start_message_id is not None for spec in specs
                    )
                    or path in task_ordinals
                )
                if staged_task_paths != plan.session_task_paths[relative]:
                    raise OrcParseError(
                        "Orc task database references changed during snapshot; "
                        "retry ingest"
                    )
            advance = (
                _advance_session_candidate(candidate, snapshot_root, captured_at)
                if kind == "session"
                else _advance_task_candidate(
                    candidate,
                    snapshot_root,
                    captured_at,
                    task_ordinals[relative],
                )
            )
            temporary_paths.extend(advance.temporary_paths)
            staged_objects.extend(advance.staged_objects)
            task_source_ordinal = advance.task_source_ordinal
            auxiliary = advance.auxiliary
            task_projection = advance.task_projection
            previous_identity = advance.previous_identity
            current_identity = advance.current_identity
            snapshot_size = temporary.stat().st_size
            snapshot_sha256 = _sha256_file(temporary)
            snapshot_relative = _snapshot_object_relative(snapshot_sha256)
            target = _snapshot_path(snapshot_root, snapshot_relative)
            effective_captured_at = (
                previous.captured_at
                if previous is not None and previous.sha256 == snapshot_sha256
                else captured_at
            )
            semantic_changed = (
                previous_identity is None or previous_identity != current_identity
            )
            if (
                previous is not None
                and not semantic_changed
            ):
                semantic_identity_mode = previous.semantic_identity_mode
                semantic_sha256 = previous.semantic_sha256
                semantic_complete_bytes = previous.semantic_complete_bytes
                semantic_baseline_path = previous.semantic_baseline_path
                semantic_alias_baseline_path = (
                    previous.semantic_alias_baseline_path
                    or previous.snapshot_path
                    if previous.semantic_identity_mode
                    == _SEMANTIC_IDENTITY_DETERMINISTIC
                    and (
                        previous.semantic_sha256,
                        previous.semantic_complete_bytes,
                    )
                    != (current_identity.sha256, current_identity.complete_bytes)
                    else None
                )
            else:
                semantic_identity_mode = _SEMANTIC_IDENTITY_DETERMINISTIC
                semantic_sha256 = current_identity.sha256
                semantic_complete_bytes = current_identity.complete_bytes
                semantic_baseline_path = None
                semantic_alias_baseline_path = None
            staged_objects.append((temporary, target, snapshot_sha256))
            result_sources.append(
                OrcSourceCopy(
                    source_path=relative,
                    snapshot_path=snapshot_relative,
                    kind=kind,
                    owner_session_id=owner_session_id,
                    lineage_root_session_id=(
                        candidate.discovered.lineage_root_session_id
                        if kind == "session"
                        else None
                    ),
                    source_size=source_path.stat().st_size,
                    snapshot_size=snapshot_size,
                    sha256=snapshot_sha256,
                    append_count=full.append_count,
                    append_max_id=full.append_max_id,
                    append_prefix_sha256=full.append_prefix_sha256,
                    semantic_identity_mode=semantic_identity_mode,
                    semantic_sha256=semantic_sha256,
                    semantic_complete_bytes=semantic_complete_bytes,
                    canonical_semantic_sha256=current_identity.sha256,
                    canonical_semantic_complete_bytes=current_identity.complete_bytes,
                    semantic_alias_baseline_path=semantic_alias_baseline_path,
                    semantic_baseline_path=semantic_baseline_path,
                    source_state=discovered_source.source_state,
                    task_source_ordinal=task_source_ordinal,
                    auxiliary=auxiliary,
                    task_projection=task_projection,
                    captured_at=effective_captured_at,
                    append_prefix_override=append_prefix_override,
                    # Read from the staged copy, not the live file: the manifest must describe the
                    # bytes it points at, and the live database can migrate between the backup and
                    # this line.
                    provider_schema_version=_provider_schema_version(
                        temporary, relative
                    ),
                )
            )

        continuations = _continuation_links(
            root_session_id, specs, temporary_by_source, previous_continuations
        )
        changed = _publish_staged_objects(staged_objects, snapshot_root)
        return OrcSnapshotResult(
            sources=tuple(result_sources),
            files_changed=changed,
            continuations=continuations,
            prefix_overrides=tuple(accepted_overrides),
        )
    finally:
        for temporary in temporary_paths:
            if temporary.exists():
                temporary.unlink()


def _conversation_spawns(
    path: Path,
    meta: _SessionMeta,
    start_at_ms: int | None = None,
    start_source_line: int | None = None,
) -> tuple[_Spawn, ...]:
    observation = _auxiliary_observation(path, meta)
    result = [
        _Spawn(
            thread_id=f"orc-agent-{meta.session_id[:8]}-{record.block_id}",
            parent_thread_id=meta.session_id,
            official_name=record.agent_id,
            timestamp_ms=record.timestamp_ms,
            source_line=record.block_id,
            source_path=meta.source_path,
        )
        for record in observation.stable_spawns
        if (
            record.message_id >= start_source_line
            if start_source_line is not None
            else start_at_ms is None or record.timestamp_ms >= start_at_ms
        )
    ]
    return tuple(
        sorted(result, key=lambda item: (item.timestamp_ms, item.source_line))
    )


def _modern_block_origins(path: Path) -> dict[str, _ModernBlockOrigin]:
    connection = _read_only(path)
    try:
        if _session_storage_table(connection, str(path)) != "messages":
            return {}
        rows = connection.execute(
            "SELECT id, message_json FROM messages ORDER BY id"
        ).fetchall()
    finally:
        connection.close()
    result: dict[str, _ModernBlockOrigin] = {}
    for raw in rows:
        row = _row(raw, str(path))
        source_line = _nonnegative_integer(row[0], f"{path}: message id")
        raw_json = _required_string(row[1], f"{path}: message_json")
        try:
            decoded: object = json.loads(raw_json)
        except json.JSONDecodeError as error:
            raise OrcParseError(
                f"{path}: invalid message_json at row {source_line}: {error}"
            ) from error
        message = _mapping(decoded, f"{path}: message_json[{source_line}]")
        native_message_id = _required_string(
            message.get("id"), f"{path}: message_json[{source_line}].id"
        )
        for raw_block in _array(
            message.get("blocks"), f"{path}: message_json[{source_line}].blocks"
        ):
            block = _mapping(raw_block, f"{path}: message block")
            block_id = block.get("id")
            if not isinstance(block_id, int) or isinstance(block_id, bool):
                continue
            identity = f"v2-block-{block_id}"
            origin = _ModernBlockOrigin(source_line, native_message_id)
            prior = result.get(identity)
            if prior is not None and prior != origin:
                raise OrcParseError(
                    f"{path}: modern block identity {identity!r} is duplicated"
                )
            result[identity] = origin
    return result


def _legacy_content_records(
    path: Path,
    meta: _SessionMeta,
    id_namespace: str = "",
    start_at_ms: int | None = None,
    start_source_line: int | None = None,
    modern_origins: Mapping[str, _ModernBlockOrigin] | None = None,
) -> tuple[tuple[Event, ...], tuple[ToolCall, ...], tuple[Turn, ...]]:
    connection = _read_only(path)
    try:
        rows = connection.execute(
            "SELECT rowid, id, message_id, created_at_ms, turn_index, role, block_type, "
            "content, code_input, code_output, code_exit_code, user_source, extra "
            "FROM content_blocks "
            "ORDER BY created_at_ms, rowid"
        ).fetchall()
    finally:
        connection.close()
    origins = modern_origins or {}

    def row_is_included(row: tuple[object, ...]) -> bool:
        timestamp_ms = _integer(row[3], f"{path}: created_at_ms")
        if start_source_line is not None:
            block_id = _required_string(row[1], f"{path}: block id")
            origin = origins.get(block_id)
            return origin is not None and origin.source_line >= start_source_line
        return start_at_ms is None or timestamp_ms >= start_at_ms

    owner_gchat_senders: set[str] = set()
    for raw in rows:
        row = _row(raw, str(path))
        if not row_is_included(row):
            continue
        role = _required_string(row[5], f"{path}: role")
        if role != "user":
            continue
        user_source = _optional_json_mapping(
            row[11], f"{path}: content block user_source"
        )
        gchat = _nested_mapping(user_source, "GChat")
        if "GChat" not in user_source:
            continue
        extra = _optional_json_mapping(
            row[12], f"{path}: content block extra"
        )
        explicit_owner = gchat.get("is_owner")
        if not isinstance(explicit_owner, bool):
            explicit_owner = extra.get("is_owner")
        if explicit_owner is not True:
            continue
        sender_aliases = (
            gchat.get("sender_unixname"),
            extra.get("sender_unixname"),
            gchat.get("sender_display_name"),
            extra.get("sender_display_name"),
            gchat.get("sender_name"),
            extra.get("sender_name"),
        )
        owner_gchat_senders.update(
            value for value in sender_aliases if isinstance(value, str) and value
        )
    events: list[Event] = []
    tools: list[ToolCall] = []
    turn_bounds: dict[int, tuple[int, int, str | None]] = {}
    for raw in rows:
        row = _row(raw, str(path))
        rowid = _integer(row[0], f"{path}: rowid")
        block_id = _required_string(row[1], f"{path}: block id")
        message_id = _required_string(row[2], f"{path}: message id")
        timestamp_ms = _integer(row[3], f"{path}: created_at_ms")
        if not row_is_included(row):
            continue
        modern_origin = origins.get(block_id)
        turn_index = _integer(row[4], f"{path}: turn_index")
        role = _required_string(row[5], f"{path}: role")
        block_type = _required_string(row[6], f"{path}: block_type")
        content = _optional_string(row[7], f"{path}: content")
        turn_id = f"{id_namespace}orc-turn-{meta.session_id[:8]}-{turn_index}"
        prior = turn_bounds.get(turn_index)
        if prior is None:
            turn_bounds[turn_index] = (
                timestamp_ms,
                timestamp_ms + 1,
                content if role in ("assistant", "notification") else None,
            )
        else:
            last_message = (
                content
                if role in ("assistant", "notification") and content
                else prior[2]
            )
            turn_bounds[turn_index] = (
                min(prior[0], timestamp_ms),
                max(prior[1], timestamp_ms + 1),
                last_message,
            )
        event_kind: str | None = None
        event_role: str | None = None
        event_author: str | None = None
        event_recipient: str | None = None
        ingress_kind: str | None = None
        author_kind: str | None = None
        source_native_id: str | None = message_id
        if block_type == "text" and role == "user":
            user_source = _optional_json_mapping(
                row[11], f"{path}: content block {block_id} user_source"
            )
            extra = _optional_json_mapping(
                row[12], f"{path}: content block {block_id} extra"
            )
            (
                event_kind,
                event_author,
                ingress_kind,
                author_kind,
                source_native_id,
            ) = _orc_input_provenance(
                user_source,
                extra,
                content or "",
                message_id,
                frozenset(owner_gchat_senders),
            )
            if event_kind == "inter_agent_message":
                event_role = None
                event_recipient = meta.session_id
            elif event_kind == "system_input":
                event_role = "system"
            else:
                event_role = "user"
        elif block_type == "text" and role in ("assistant", "notification"):
            event_kind = "assistant_message"
            event_role = "assistant"
            event_author = meta.session_id
            ingress_kind = "orc"
            author_kind = "agent"
        if event_kind is not None and content:
            events.append(
                Event(
                    event_id=f"{id_namespace}orc-block-{block_id}",
                    thread_id=meta.session_id,
                    turn_id=turn_id,
                    timestamp_ms=timestamp_ms,
                    kind=event_kind,
                    role=event_role,
                    phase=None,
                    text=content,
                    content_availability="plaintext",
                    encrypted_content=None,
                    author=event_author,
                    recipient=event_recipient,
                    source_line=(
                        modern_origin.source_line
                        if modern_origin is not None
                        else rowid
                    ),
                    ingress_kind=ingress_kind,
                    author_kind=author_kind,
                    source_native_id=(
                        modern_origin.native_message_id
                        if modern_origin is not None
                        else source_native_id
                    ),
                    classification_version=_CLASSIFICATION_VERSION,
                )
            )
        if block_type == "code_execution":
            code_input = _optional_string(row[8], f"{path}: code_input")
            code_output = _optional_string(row[9], f"{path}: code_output")
            exit_code = row[10]
            counts = Counter(_ORC_TOOL.findall(code_input or ""))
            status = (
                "failed"
                if isinstance(exit_code, int) and not isinstance(exit_code, bool) and exit_code != 0
                else "completed"
            )
            tools.append(
                ToolCall(
                    call_id=f"{id_namespace}orc-code-{block_id}",
                    item_id=f"{id_namespace}{block_id}",
                    thread_id=meta.session_id,
                    turn_id=turn_id,
                    name="code_execution",
                    namespace="orc",
                    started_at_ms=timestamp_ms,
                    ended_at_ms=timestamp_ms + 1,
                    status=status,
                    input_text=code_input,
                    output_text=code_output,
                    nested_tools=tuple(sorted(counts.items())),
                    source_line=(
                        modern_origin.source_line
                        if modern_origin is not None
                        else rowid
                    ),
                )
            )
    turns = tuple(
        Turn(
            turn_id=f"{id_namespace}orc-turn-{meta.session_id[:8]}-{turn_index}",
            thread_id=meta.session_id,
            started_at_ms=bounds[0],
            ended_at_ms=bounds[1],
            status="completed",
            first_token_ms=None,
            error=None,
            last_agent_message=bounds[2],
        )
        for turn_index, bounds in sorted(turn_bounds.items())
    )
    return tuple(events), tuple(tools), turns


def _modern_content_records(
    path: Path,
    meta: _SessionMeta,
    id_namespace: str,
    start_at_ms: int | None,
    start_source_line: int | None,
) -> tuple[tuple[Event, ...], tuple[ToolCall, ...], tuple[Turn, ...]]:
    """Normalize the append-only schema-v5 Orc messages table."""

    connection = _read_only(path)
    try:
        rows = connection.execute(
            "SELECT id, session_id, role, created_at_ms, message_json "
            "FROM messages ORDER BY id"
        ).fetchall()
        content_rows = connection.execute(
            "SELECT id, turn_index FROM content_blocks ORDER BY rowid"
        ).fetchall()
    finally:
        connection.close()
    existing_block_turns: dict[int, int] = {}
    for raw in content_rows:
        row = _row(raw, f"{path}: content block identity")
        block_identity = _required_string(row[0], f"{path}: content block id")
        match = re.fullmatch(r"v2-block-(\d+)", block_identity)
        if match is None:
            continue
        existing_block_turns[int(match.group(1))] = _integer(
            row[1], f"{path}: content block turn_index"
        )
    parsed: list[tuple[int, int, str, dict[str, object]]] = []
    owner_gchat_senders: set[str] = set()

    def message_is_included(message_id: int, timestamp_ms: int) -> bool:
        if start_source_line is not None:
            return message_id >= start_source_line
        return start_at_ms is None or timestamp_ms >= start_at_ms

    for index, raw in enumerate(rows):
        row = _row(raw, f"{path}: messages[{index}]")
        message_id = _nonnegative_integer(row[0], f"{path}: message id")
        session_id = _required_string(row[1], f"{path}: message session_id")
        if session_id != meta.session_id:
            raise OrcParseError(
                f"{path}: message {message_id} belongs to {session_id!r}, "
                f"not {meta.session_id!r}"
            )
        stored_role = _required_string(row[2], f"{path}: message role")
        timestamp_ms = _nonnegative_integer(
            row[3], f"{path}: message created_at_ms"
        )
        raw_json = _required_string(row[4], f"{path}: message_json")
        try:
            decoded: object = json.loads(raw_json)
        except json.JSONDecodeError as error:
            raise OrcParseError(
                f"{path}: invalid message_json for row {message_id}: {error}"
            ) from error
        message = _mapping(decoded, f"{path}: message_json[{message_id}]")
        role = _required_string(
            message.get("role"), f"{path}: message_json[{message_id}].role"
        )
        if role.casefold() != stored_role.casefold():
            raise OrcParseError(
                f"{path}: role differs for message row {message_id}"
            )
        embedded_at = _nonnegative_integer(
            message.get("created_at_ms"),
            f"{path}: message_json[{message_id}].created_at_ms",
        )
        if embedded_at != timestamp_ms:
            raise OrcParseError(
                f"{path}: timestamp differs for message row {message_id}"
            )
        source = _modern_source_mapping(
            message.get("source"), f"{path}: message_json[{message_id}].source"
        )
        if (
            message_is_included(message_id, timestamp_ms)
            and role.casefold() == "user"
            and "GChat" in source
        ):
            gchat = _nested_mapping(source, "GChat")
            explicit_owner = gchat.get("is_owner")
            if explicit_owner is True:
                owner_gchat_senders.update(
                    value
                    for value in (
                        gchat.get("sender_unixname"),
                        gchat.get("sender_display_name"),
                        gchat.get("sender_name"),
                    )
                    if isinstance(value, str) and value
                )
        parsed.append((message_id, timestamp_ms, role, message))

    events: list[Event] = []
    tools: list[ToolCall] = []
    turns: list[Turn] = []
    current_turn_index = 0
    for message_id, timestamp_ms, role, message in parsed:
        if timestamp_ms == 0:
            continue
        blocks = _array(
            message.get("blocks"), f"{path}: message_json[{message_id}].blocks"
        )
        known_turns = {
            existing_block_turns[raw_id]
            for raw_block in blocks
            for block in [_mapping(raw_block, f"{path}: message block")]
            for raw_id in [block.get("id")]
            if isinstance(raw_id, int)
            and not isinstance(raw_id, bool)
            and raw_id in existing_block_turns
        }
        if len(known_turns) > 1:
            raise OrcParseError(
                f"{path}: message row {message_id} spans multiple legacy turns"
            )
        if known_turns:
            turn_index = next(iter(known_turns))
            current_turn_index = turn_index
        else:
            if role.casefold() == "user" or current_turn_index == 0:
                current_turn_index += 1
            turn_index = current_turn_index
        if not message_is_included(message_id, timestamp_ms):
            continue
        native_message_id = _required_string(
            message.get("id"), f"{path}: message_json[{message_id}].id"
        )
        turn_id = f"{id_namespace}orc-turn-{meta.session_id[:8]}-{turn_index}"
        last_agent_message: str | None = None
        error_text: str | None = None
        emitted = False
        source = _modern_source_mapping(
            message.get("source"), f"{path}: message_json[{message_id}].source"
        )
        for block_index, raw_block in enumerate(blocks):
            block = _mapping(
                raw_block,
                f"{path}: message_json[{message_id}].blocks[{block_index}]",
            )
            block_type = _required_string(
                block.get("type"),
                f"{path}: message_json[{message_id}].blocks[{block_index}].type",
            )
            raw_block_id = block.get("id")
            if not isinstance(raw_block_id, int) or isinstance(raw_block_id, bool):
                continue
            block_id = raw_block_id
            if block_id in existing_block_turns:
                continue
            if block_type == "ErrorBlock":
                error_text = _optional_string(
                    block.get("message"),
                    f"{path}: message_json[{message_id}].blocks[{block_index}].message",
                )
                emitted = True
                continue
            if block_type == "CodeExecutionBlock":
                code_input = _optional_string(
                    block.get("code"),
                    f"{path}: message_json[{message_id}].blocks[{block_index}].code",
                )
                code_output = _optional_string(
                    block.get("output"),
                    f"{path}: message_json[{message_id}].blocks[{block_index}].output",
                )
                is_error = block.get("is_error")
                if not isinstance(is_error, bool):
                    raise OrcParseError(
                        f"{path}: CodeExecutionBlock {block_id} lacks is_error"
                    )
                counts = Counter(_ORC_TOOL.findall(code_input or ""))
                identity = f"v2-block-{block_id}"
                tools.append(
                    ToolCall(
                        call_id=f"{id_namespace}orc-code-{identity}",
                        item_id=f"{id_namespace}{identity}",
                        thread_id=meta.session_id,
                        turn_id=turn_id,
                        name="code_execution",
                        namespace="orc",
                        started_at_ms=timestamp_ms,
                        ended_at_ms=timestamp_ms + 1,
                        status="failed" if is_error else "completed",
                        input_text=code_input,
                        output_text=code_output,
                        nested_tools=tuple(sorted(counts.items())),
                        source_line=message_id,
                    )
                )
                emitted = True
                continue
            text_key = (
                "text"
                if block_type in ("text", "NotificationBlock")
                else None
            )
            if text_key is None:
                continue
            content = _optional_string(
                block.get(text_key),
                f"{path}: message_json[{message_id}].blocks[{block_index}].text",
            )
            if content is None:
                continue
            role_key = role.casefold()
            event_kind: str
            event_role: str | None
            event_author: str | None
            event_recipient: str | None
            ingress_kind: str
            author_kind: str
            source_native_id: str
            if role_key == "user":
                (
                    event_kind,
                    event_author,
                    ingress_kind,
                    author_kind,
                    source_native_id,
                ) = _orc_input_provenance(
                    source,
                    {},
                    content,
                    native_message_id,
                    frozenset(owner_gchat_senders),
                )
                if event_kind == "inter_agent_message":
                    event_role = None
                    event_recipient = meta.session_id
                elif event_kind == "system_input":
                    event_role = "system"
                    event_recipient = None
                else:
                    event_role = "user"
                    event_recipient = None
            elif role_key == "system":
                event_kind = "system_input"
                event_role = "system"
                event_author = "system"
                event_recipient = None
                ingress_kind = "orc"
                author_kind = "system"
                source_native_id = native_message_id
            else:
                event_kind = "assistant_message"
                event_role = "assistant"
                event_author = meta.session_id
                event_recipient = None
                ingress_kind = "orc"
                author_kind = "agent"
                source_native_id = native_message_id
                last_agent_message = content
            events.append(
                Event(
                    event_id=(
                        f"{id_namespace}orc-block-v2-block-{block_id}"
                    ),
                    thread_id=meta.session_id,
                    turn_id=turn_id,
                    timestamp_ms=timestamp_ms,
                    kind=event_kind,
                    role=event_role,
                    phase=None,
                    text=content,
                    content_availability="plaintext",
                    encrypted_content=None,
                    author=event_author,
                    recipient=event_recipient,
                    source_line=message_id,
                    ingress_kind=ingress_kind,
                    author_kind=author_kind,
                    source_native_id=source_native_id,
                    classification_version=_CLASSIFICATION_VERSION,
                )
            )
            emitted = True
        if emitted:
            turns.append(
                Turn(
                    turn_id=turn_id,
                    thread_id=meta.session_id,
                    started_at_ms=timestamp_ms,
                    ended_at_ms=timestamp_ms + 1,
                    status="failed" if error_text is not None else "completed",
                    first_token_ms=None,
                    error=error_text,
                    last_agent_message=last_agent_message,
                )
            )
    return tuple(events), tuple(tools), tuple(turns)


def _content_records(
    path: Path,
    meta: _SessionMeta,
    id_namespace: str = "",
    start_at_ms: int | None = None,
    start_source_line: int | None = None,
) -> tuple[tuple[Event, ...], tuple[ToolCall, ...], tuple[Turn, ...]]:
    connection = _read_only(path)
    try:
        modern = _session_storage_table(connection, str(path)) == "messages"
    finally:
        connection.close()
    modern_origins = _modern_block_origins(path) if modern else {}
    legacy = _legacy_content_records(
        path,
        meta,
        id_namespace,
        start_at_ms,
        start_source_line,
        modern_origins,
    )
    if not modern:
        return legacy
    additions = _modern_content_records(
        path, meta, id_namespace, start_at_ms, start_source_line
    )
    turns_by_id = {turn.turn_id: turn for turn in legacy[2]}
    for turn in additions[2]:
        prior = turns_by_id.get(turn.turn_id)
        if prior is None:
            turns_by_id[turn.turn_id] = turn
            continue
        turns_by_id[turn.turn_id] = replace(
            prior,
            started_at_ms=min(prior.started_at_ms, turn.started_at_ms),
            ended_at_ms=max(
                prior.ended_at_ms or prior.started_at_ms,
                turn.ended_at_ms or turn.started_at_ms,
            ),
            status=(
                "failed"
                if prior.status == "failed" or turn.status == "failed"
                else prior.status
            ),
            error=prior.error or turn.error,
            last_agent_message=(
                turn.last_agent_message or prior.last_agent_message
            ),
        )
    return (
        (*legacy[0], *additions[0]),
        (*legacy[1], *additions[1]),
        tuple(
            sorted(
                turns_by_id.values(),
                key=lambda item: (item.started_at_ms, item.turn_id),
            )
        ),
    )


def _select_spawn(
    spawns: Mapping[tuple[str, str], Sequence[_Spawn]],
    coordinator_id: str,
    owner: str,
    timestamp_ms: int,
) -> _Spawn | None:
    eligible = [
        spawn
        for spawn in spawns.get((coordinator_id, owner), ())
        if spawn.timestamp_ms <= timestamp_ms
    ]
    return eligible[-1] if eligible else None


def _task_records(
    path: Path,
    source_path: str,
    coordinator_id: str,
    task_source_ordinal: int,
    spawns: Mapping[tuple[str, str], Sequence[_Spawn]],
    records: Sequence[_FrozenTaskNote],
    *,
    start_at_ms: int | None = None,
    end_at_ms: int | None = None,
    id_namespace: str = "",
) -> tuple[tuple[Event, ...], tuple[Turn, ...], tuple[_Spawn, ...]]:
    events: list[Event] = []
    turns: list[Turn] = []
    inferred: dict[str, _Spawn] = {}
    for record in sorted(records, key=lambda item: (item.created_at, item.note_id)):
        note_id = record.note_id
        task_id = record.task_id
        content = record.content
        timestamp_ms = _iso_ms(record.created_at, f"{path}: note created_at")
        if start_at_ms is not None and timestamp_ms < start_at_ms:
            continue
        if end_at_ms is not None and timestamp_ms >= end_at_ms:
            continue
        title = record.title
        namespace = "" if task_source_ordinal == 0 else f"-s{task_source_ordinal}"
        turn_id = (
            f"{id_namespace}orc-note-turn-"
            f"{coordinator_id[:8]}{namespace}-{note_id}"
        )
        event_id = (
            f"{id_namespace}orc-note-{coordinator_id[:8]}{namespace}-{note_id}"
        )
        if record.server_author is not None:
            text = (
                f"[{task_id} · {title} · external author: "
                f"{record.server_author}]\n\n{content}"
            )
            events.append(
                Event(
                    event_id=event_id,
                    thread_id=coordinator_id,
                    turn_id=turn_id,
                    timestamp_ms=timestamp_ms,
                    kind="external_message",
                    role="user",
                    phase=None,
                    text=text,
                    content_availability="plaintext",
                    encrypted_content=None,
                    author=record.server_author,
                    recipient=None,
                    source_line=note_id,
                )
            )
            turns.append(
                Turn(
                    turn_id=turn_id,
                    thread_id=coordinator_id,
                    started_at_ms=timestamp_ms,
                    ended_at_ms=timestamp_ms + 1000,
                    status="completed",
                    first_token_ms=None,
                    error=None,
                    last_agent_message=None,
                )
            )
            continue
        owner = record.task_owner
        inferred_key = owner or "__unattributed__"
        spawn = (
            None
            if owner is None
            else _select_spawn(spawns, coordinator_id, owner, timestamp_ms)
        )
        if spawn is None:
            spawn = inferred.get(inferred_key)
        if spawn is None:
            if owner is None:
                official_name = "Unattributed Task Work"
                thread_id = f"orc-unattributed-{coordinator_id[:8]}{namespace or '-s0'}"
            else:
                official_name = owner
                owner_slug = (
                    re.sub(r"[^A-Za-z0-9._-]+", "-", owner).strip("-") or "agent"
                )
                digest = hashlib.sha256(
                    f"{coordinator_id}\0{owner}".encode("utf-8")
                ).hexdigest()[:12]
                thread_id = f"orc-owner-{owner_slug[:80]}-{digest}"
            spawn = _Spawn(
                thread_id=thread_id,
                parent_thread_id=coordinator_id,
                official_name=official_name,
                timestamp_ms=timestamp_ms,
                source_line=note_id,
                source_path=source_path,
            )
            inferred[inferred_key] = spawn
        provenance = " · unattributed local task work" if owner is None else ""
        text = f"[{task_id} · {title}{provenance}]\n\n{content}"
        events.append(
            Event(
                event_id=event_id,
                thread_id=spawn.thread_id,
                turn_id=turn_id,
                timestamp_ms=timestamp_ms,
                kind="inter_agent_message",
                role="agent",
                phase=None,
                text=text,
                content_availability="plaintext",
                encrypted_content=None,
                author=spawn.thread_id,
                recipient=coordinator_id,
                source_line=note_id,
            )
        )
        turns.append(
            Turn(
                turn_id=turn_id,
                thread_id=spawn.thread_id,
                started_at_ms=timestamp_ms,
                ended_at_ms=timestamp_ms + 1000,
                status="completed",
                first_token_ms=None,
                error=None,
                last_agent_message=text,
            )
        )
    return tuple(events), tuple(turns), tuple(inferred.values())


def _promoted_task_notes(
    source: OrcSourceCopy,
    source_path: Path,
    records: Sequence[_FrozenTaskNote],
) -> tuple[TaskNote, ...]:
    """Lift one task source's frozen note history into normalized model records.

    This is the whole content of the frozen projection, verbatim and unwindowed. ``_task_records``
    below renders the same notes into events and drops the ones outside the ingest window, which
    is right for a timeline and wrong for the last surviving copy of a deleted note -- so the two
    deliberately disagree, and this one keeps everything.

    ``upstream_present`` costs one query against the snapshotted database and is the only thing
    here that could not be recovered later. The projection already knows the *number* of frozen
    notes with no live counterpart and the digest of their ids -- ``missing_note_count`` and
    ``missing_note_ids_sha256``, computed by ``_task_rewrite_observation`` at snapshot time -- and
    neither answers "which ones", which is the only form of the question a reader ever has. Once
    ``source_snapshots/`` is deleted, which is the entire point of promoting these notes, the
    comparison can never be made again; recording it per note is what turns "1,386 notes are the
    archive's only copy" from a statistic into something you can open.

    The comparison is against the *snapshot* database, not the live one: the snapshot is the state
    the frozen records were frozen against, so the two sides of the comparison are contemporaries.
    ``pipeline._merge_promoted_task_notes`` is what carries the answer forward, and it lets this
    field -- alone among the provenance -- latch from true to false as upstream deletes rows.
    """

    projection = source.task_projection
    live_note_ids = {record.note_id for record in _observed_task_notes(source_path)}
    return tuple(
        TaskNote(
            note_id=record.note_id,
            source_path=source.source_path,
            task_source_ordinal=source.task_source_ordinal or 0,
            task_id=record.task_id,
            title=record.title,
            content=record.content,
            created_at=record.created_at,
            server_author=record.server_author,
            task_owner=record.task_owner,
            upstream_present=record.note_id in live_note_ids,
            projection_policy=None if projection is None else projection.policy,
            projection_sha256=None if projection is None else projection.sha256,
        )
        for record in records
    )


def _agent_depth(
    thread_id: str, parents: Mapping[str, str | None]
) -> int:
    depth = 0
    seen = {thread_id}
    parent = parents.get(thread_id)
    while parent is not None and parent not in seen:
        seen.add(parent)
        depth += 1
        parent = parents.get(parent)
    return depth


def _validated_continuation_roots(
    snapshot_root: Path,
    root_session_id: str,
    source_copies: Sequence[OrcSourceCopy],
    metas: Mapping[str, _SessionMeta],
    continuation_links: Sequence[OrcContinuationLink],
) -> tuple[str, ...]:
    continuation_ids = tuple(link.session_id for link in continuation_links)
    roots = _lineage_roots(root_session_id, continuation_ids)
    source_by_path = {source.source_path: source for source in source_copies}
    prior_start: int | None = None
    for index, root in enumerate(roots):
        meta = metas.get(root)
        if meta is None:
            raise OrcParseError(
                f"snapshot set does not contain configured Orc root {root!r}"
            )
        if meta.parent_id is not None:
            raise OrcParseError(
                f"configured Orc continuation {root!r} is not a parentless root"
            )
        if index == 0:
            prior_start = meta.created_at_ms
            continue
        link = continuation_links[index - 1]
        predecessor = roots[index - 1]
        predecessor_source_path = f".orc/sessions/{predecessor}/session.db"
        successor_source_path = f".orc/sessions/{root}/session.db"
        validated = OrcContinuationLink.from_json_obj(
            link.to_json_obj(), f"continuation_links[{index - 1}]"
        )
        if (
            validated.predecessor_session_id != predecessor
            or validated.session_id != root
            or validated.predecessor_source_path != predecessor_source_path
            or validated.source_path != successor_source_path
        ):
            raise OrcParseError(
                f"Orc continuation {index - 1} does not follow the configured order"
            )
        predecessor_source = source_by_path.get(predecessor_source_path)
        successor_source = source_by_path.get(successor_source_path)
        if predecessor_source is None or successor_source is None:
            raise OrcParseError(
                f"Orc continuation {index - 1} refers to an absent source snapshot"
            )
        successor_path = _snapshot_path(
            snapshot_root, successor_source.snapshot_path
        )
        resolved_line, resolved_at = _resolve_continuation_start(
            successor_path,
            meta,
            OrcContinuationSpec(
                session_id=root,
                start_message_id=validated.start_message_id,
            ),
        )
        if (
            resolved_line != validated.start_source_line
            or resolved_at != validated.started_at_ms
            or prior_start is None
            or resolved_at <= prior_start
        ):
            raise OrcParseError(
                f"Orc continuation {index - 1} start boundary changed or is unordered"
            )
        prior_start = resolved_at
        path = _snapshot_path(snapshot_root, predecessor_source.snapshot_path)
        connection = _read_only(path)
        try:
            # Same resolver as the snapshot path, deliberately. This read is against the frozen
            # snapshot object rather than the live database, so the two can legitimately be at
            # different Orc storage schemas at the same instant -- which is exactly the situation
            # in which two independently written "which table is this?" rules would drift apart and
            # only one of them would be believed. Nothing is adopted here: this path validates, and
            # the snapshot path is the only writer of `predecessor_source_table`.
            _resolve_continuation_boundary(
                connection, validated, index - 1, str(path)
            )
        except sqlite3.Error as error:
            raise OrcParseError(
                f"failed to validate Orc continuation boundary at {path}: {error}"
            ) from error
        finally:
            connection.close()
    return roots


def _continuation_lineages(
    metas: Mapping[str, _SessionMeta],
    roots: Sequence[str],
    root_start_ms: Mapping[str, int],
    recorded_roots: Mapping[str, str],
) -> dict[str, str]:
    root_set = set(roots)
    result: dict[str, str] = {}

    def resolve(session_id: str, visiting: frozenset[str]) -> str:
        known = result.get(session_id)
        if known is not None:
            return known
        recorded = recorded_roots.get(session_id)
        if recorded is not None:
            if recorded not in root_set:
                raise OrcParseError(
                    f"Orc session {session_id!r} records unknown lineage root "
                    f"{recorded!r}"
                )
            result[session_id] = recorded
            return recorded
        if session_id in root_set:
            result[session_id] = session_id
            return session_id
        if session_id in visiting:
            raise OrcParseError(f"cycle in Orc session lineage at {session_id!r}")
        meta = metas[session_id]
        if meta.parent_id is None:
            raise OrcParseError(
                f"Orc session {session_id!r} is outside the configured root lineages"
            )
        if meta.parent_id not in metas:
            candidates = [
                root
                for root in roots
                if root_start_ms[root] <= meta.updated_at_ms
            ]
            if not candidates:
                raise OrcParseError(
                    f"Orc session {session_id!r} has no retained lineage root"
                )
            root = max(candidates, key=root_start_ms.__getitem__)
            result[session_id] = root
            return root
        root = resolve(meta.parent_id, visiting | {session_id})
        result[session_id] = root
        return root

    for session_id in metas:
        resolve(session_id, frozenset())
    for session_id, meta in metas.items():
        parent_id = meta.parent_id
        if parent_id is not None and parent_id in metas and result[parent_id] != result[session_id]:
            raise OrcParseError(
                f"Orc retained parent/child sessions disagree on lineage root: "
                f"{parent_id!r} -> {session_id!r}"
            )
    return result


def _continuation_namespace(
    lineage_root: str, roots: Sequence[str]
) -> str:
    index = roots.index(lineage_root)
    return "" if index == 0 else f"orc-cont-{index}-{lineage_root[:8]}-"


def load_orc_team(
    snapshot_root: Path,
    root_session_id: str,
    team_slug: str,
    display_timezone: str,
    source_copies: Sequence[OrcSourceCopy],
    continuation_links: Sequence[OrcContinuationLink] = (),
) -> TeamData:
    """Normalize validated archive-local Orc SQLite backups into ``TeamData``."""

    session_sources = [source for source in source_copies if source.kind == "session"]
    task_sources = [source for source in source_copies if source.kind == "task"]
    source_paths = [source.source_path for source in source_copies]
    if len(set(source_paths)) != len(source_paths):
        raise OrcParseError("snapshot set contains duplicate Orc source records")
    for source in source_copies:
        if source.auxiliary.policy != _AUXILIARY_POLICY_LEGACY:
            _validate_source_semantic_identity(snapshot_root, source)
    metas: dict[str, _SessionMeta] = {}
    session_snapshot_paths: dict[str, str] = {}
    for source in session_sources:
        path = _snapshot_path(snapshot_root, source.snapshot_path)
        meta = _session_meta(path, source.source_path)
        if meta.session_id != source.owner_session_id:
            raise OrcParseError(
                f"session snapshot {source.source_path!r} contains {meta.session_id!r}, "
                f"not manifest owner {source.owner_session_id!r}"
            )
        if meta.session_id in metas:
            raise OrcParseError(f"duplicate Orc session id {meta.session_id!r}")
        metas[meta.session_id] = meta
        session_snapshot_paths[meta.session_id] = source.snapshot_path
    if root_session_id not in metas:
        raise OrcParseError(f"snapshot set does not contain root session {root_session_id!r}")
    prefixes: dict[str, str] = {}
    for session_id in sorted(metas):
        prefix = session_id[:8]
        prior = prefixes.get(prefix)
        if prior is not None and prior != session_id:
            raise OrcParseError(
                f"Orc session-id prefix collision {prefix!r}: {prior!r} and {session_id!r}"
            )
        prefixes[prefix] = session_id
    for source in task_sources:
        if source.owner_session_id not in metas:
            raise OrcParseError(
                f"task source {source.source_path!r} has unknown owner "
                f"{source.owner_session_id!r}"
            )
    roots = (
        _validated_continuation_roots(
            snapshot_root,
            root_session_id,
            source_copies,
            metas,
            continuation_links,
        )
        if continuation_links
        else (root_session_id,)
    )
    root_start_ms = {root_session_id: metas[root_session_id].created_at_ms}
    root_start_ms.update(
        {link.session_id: link.started_at_ms for link in continuation_links}
    )
    root_start_source_line = {
        link.session_id: link.start_source_line for link in continuation_links
    }
    lineage_by_session = (
        _continuation_lineages(
            metas,
            roots,
            root_start_ms,
            {
                source.owner_session_id: source.lineage_root_session_id
                for source in session_sources
                if source.lineage_root_session_id is not None
            },
        )
        if continuation_links
        else {session_id: root_session_id for session_id in metas}
    )

    session_events: list[Event] = []
    tools: list[ToolCall] = []
    turns: list[Turn] = []
    all_spawns: list[_Spawn] = []
    for session_id in sorted(metas):
        meta = metas[session_id]
        path = _snapshot_path(snapshot_root, session_snapshot_paths[session_id])
        lineage_root = lineage_by_session[session_id]
        lineage_start = root_start_ms[lineage_root]
        source_line_start = (
            root_start_source_line.get(lineage_root)
            if session_id == lineage_root
            else None
        )
        content_events, session_tools, session_turns = _content_records(
            path,
            meta,
            _continuation_namespace(lineage_by_session[session_id], roots),
            lineage_start if lineage_by_session[session_id] != root_session_id else None,
            source_line_start,
        )
        session_events.extend(content_events)
        tools.extend(session_tools)
        turns.extend(session_turns)
        all_spawns.extend(
            _conversation_spawns(
                path,
                meta,
                lineage_start
                if lineage_by_session[session_id] != root_session_id
                else None,
                source_line_start,
            )
        )

    spawns_by_name: dict[tuple[str, str], list[_Spawn]] = defaultdict(list)
    for spawn in all_spawns:
        spawns_by_name[(spawn.parent_thread_id, spawn.official_name)].append(spawn)
    for values in spawns_by_name.values():
        values.sort(key=lambda item: (item.timestamp_ms, item.source_line))

    task_roots_by_path: dict[str, tuple[str, ...]] = {}
    if continuation_links:
        referenced_roots: dict[str, set[str]] = defaultdict(set)
        task_source_paths = {source.source_path for source in task_sources}
        for session_id, meta in metas.items():
            path = _snapshot_path(snapshot_root, session_snapshot_paths[session_id])
            for task_path, _, _ in _session_task_relatives(
                path, meta, lineage_by_session[session_id]
            ):
                if task_path in task_source_paths:
                    referenced_roots[task_path].add(
                        lineage_by_session[session_id]
                    )
        root_order = {session_id: index for index, session_id in enumerate(roots)}
        task_roots_by_path = {
            path: tuple(sorted(values, key=root_order.__getitem__))
            for path, values in referenced_roots.items()
        }

    task_events: list[Event] = []
    task_notes: list[TaskNote] = []
    inferred_spawns: list[_Spawn] = []
    for source in task_sources:
        path = _snapshot_path(snapshot_root, source.snapshot_path)
        frozen_records = _bootstrap_frozen_task_projection(
            snapshot_root, source, path
        )
        task_notes.extend(_promoted_task_notes(source, path, frozen_records))
        task_roots = task_roots_by_path.get(source.source_path, ())
        owner_root = lineage_by_session[source.owner_session_id]
        if task_roots and owner_root not in task_roots:
            root_order = {session_id: index for index, session_id in enumerate(roots)}
            task_roots = tuple(
                sorted((owner_root, *task_roots), key=root_order.__getitem__)
            )
        if len(task_roots) <= 1:
            assignments: tuple[tuple[str, int | None, int | None], ...] = (
                (
                    source.owner_session_id,
                    (
                        root_start_ms[owner_root]
                        if owner_root != root_session_id
                        else None
                    ),
                    None,
                ),
            )
        else:
            assignments = tuple(
                (
                    coordinator_id,
                    (
                        None
                        if coordinator_id == root_session_id
                        else root_start_ms[coordinator_id]
                    ),
                    (
                        root_start_ms[task_roots[index + 1]]
                        if index + 1 < len(task_roots)
                        else None
                    ),
                )
                for index, coordinator_id in enumerate(task_roots)
            )
        for coordinator_id, start_at_ms, end_at_ms in assignments:
            note_events, task_turns, inferred = _task_records(
                path,
                source.source_path,
                coordinator_id,
                source.task_source_ordinal or 0,
                spawns_by_name,
                frozen_records,
                start_at_ms=start_at_ms,
                end_at_ms=end_at_ms,
                id_namespace=_continuation_namespace(
                    lineage_by_session[coordinator_id], roots
                ),
            )
            task_events.extend(note_events)
            turns.extend(task_turns)
            inferred_spawns.extend(inferred)
    all_spawns.extend(inferred_spawns)
    spawn_ids: dict[str, _Spawn] = {}
    for spawn in all_spawns:
        prior_spawn = spawn_ids.get(spawn.thread_id)
        if prior_spawn is not None:
            if (
                spawn.thread_id.startswith("orc-owner-")
                and spawn.parent_thread_id == prior_spawn.parent_thread_id
                and spawn.official_name == prior_spawn.official_name
            ):
                if (spawn.timestamp_ms, spawn.source_path, spawn.source_line) < (
                    prior_spawn.timestamp_ms,
                    prior_spawn.source_path,
                    prior_spawn.source_line,
                ):
                    spawn_ids[spawn.thread_id] = spawn
                continue
            raise OrcParseError(
                f"Orc agent id collision {spawn.thread_id!r} between "
                f"{prior_spawn.source_path!r} and {spawn.source_path!r}"
            )
        if spawn.thread_id in metas:
            raise OrcParseError(
                f"Orc generated agent id collides with session id {spawn.thread_id!r}"
            )
        spawn_ids[spawn.thread_id] = spawn
    all_spawns = list(spawn_ids.values())

    normalized_events = sorted(
        [*session_events, *task_events],
        key=lambda item: (item.timestamp_ms, item.event_id),
    )
    events_by_thread: dict[str, list[Event]] = defaultdict(list)
    for event in normalized_events:
        events_by_thread[event.thread_id].append(event)

    bounded_roots = {
        link.session_id
        for link in continuation_links
        if link.start_message_id is not None
    }
    included_sessions: set[str] = set()
    evidence_sessions = {
        event.thread_id
        for event in (*session_events, *task_events)
        if event.thread_id in metas
    }
    evidence_sessions.update(
        tool.thread_id for tool in tools if tool.thread_id in metas
    )
    evidence_sessions.update(
        turn.thread_id for turn in turns if turn.thread_id in metas
    )
    evidence_sessions.update(
        spawn.parent_thread_id
        for spawn in all_spawns
        if spawn.parent_thread_id in metas
    )
    for session_id, meta in metas.items():
        lineage_root = lineage_by_session[session_id]
        if (
            lineage_root not in bounded_roots
            or session_id == lineage_root
            or meta.created_at_ms >= root_start_ms[lineage_root]
            or session_id in evidence_sessions
        ):
            included_sessions.add(session_id)

    continuation_parents = {
        link.session_id: link.predecessor_session_id
        for link in continuation_links
    }
    def included_parent(session_id: str) -> str | None:
        if session_id in continuation_parents:
            return continuation_parents[session_id]
        parent = metas[session_id].parent_id
        seen = {session_id}
        while parent is not None and parent in metas and parent not in included_sessions:
            if parent in seen:
                raise OrcParseError(f"cycle in Orc session parents at {parent!r}")
            seen.add(parent)
            parent = metas[parent].parent_id
        if parent is not None and parent in included_sessions:
            return parent
        lineage_root = lineage_by_session[session_id]
        return lineage_root if lineage_root != session_id else None

    parents: dict[str, str | None] = {
        session_id: included_parent(session_id)
        for session_id in included_sessions
    }
    all_spawns = [
        spawn
        for spawn in all_spawns
        if spawn.parent_thread_id not in metas
        or spawn.parent_thread_id in included_sessions
    ]
    for spawn in all_spawns:
        parents[spawn.thread_id] = spawn.parent_thread_id

    session_agent_paths: dict[str, str] = {}

    def session_agent_path(session_id: str) -> str:
        known = session_agent_paths.get(session_id)
        if known is not None:
            return known
        if session_id == root_session_id:
            result = "/root"
        else:
            meta = metas[session_id]
            if not continuation_links:
                result = f"/root/{meta.name}"
            else:
                parent = parents.get(session_id)
                parent_path = (
                    session_agent_path(parent)
                    if parent is not None and parent in included_sessions
                    else "/root"
                )
                component = (
                    f"continuation-{meta.name}-{session_id[:8]}"
                    if session_id in continuation_parents
                    else meta.name
                )
                result = f"{parent_path.rstrip('/')}/{component}"
        session_agent_paths[session_id] = result
        return result

    agents: list[Agent] = []
    for session_id in sorted(included_sessions):
        meta = metas[session_id]
        raw_parent = parents.get(session_id)
        parent = raw_parent if raw_parent in metas else None
        agent_path = session_agent_path(session_id)
        own = events_by_thread.get(session_id, [])
        activity_ends = [meta.created_at_ms + 1]
        activity_ends.extend(event.timestamp_ms + 1 for event in own)
        activity_ends.extend(
            (tool.ended_at_ms or tool.started_at_ms) + 1
            for tool in tools
            if tool.thread_id == session_id
        )
        activity_ends.extend(
            (turn.ended_at_ms or turn.started_at_ms) + 1
            for turn in turns
            if turn.thread_id == session_id
        )
        activity_ends.extend(
            spawn.timestamp_ms + 1
            for spawn in all_spawns
            if spawn.parent_thread_id == session_id
        )
        ended = max(activity_ends)
        agents.append(
            Agent(
                thread_id=session_id,
                parent_thread_id=parent,
                agent_path=agent_path,
                nickname=meta.name if session_id != root_session_id else None,
                role="coordinator",
                depth=_agent_depth(session_id, parents),
                started_at_ms=max(
                    meta.created_at_ms,
                    root_start_ms[lineage_by_session[session_id]],
                ),
                ended_at_ms=max(
                    max(
                        meta.created_at_ms,
                        root_start_ms[lineage_by_session[session_id]],
                    )
                    + 1,
                    ended,
                ),
                status="completed",
                source_path=meta.source_path,
            )
        )

    spawns_by_official: dict[tuple[str, str], list[_Spawn]] = defaultdict(list)
    for spawn in all_spawns:
        spawns_by_official[(spawn.parent_thread_id, spawn.official_name)].append(spawn)
    for values in spawns_by_official.values():
        values.sort(key=lambda item: (item.timestamp_ms, item.source_line))
    for spawn_key in sorted(spawns_by_official):
        values = spawns_by_official[spawn_key]
        official_name = spawn_key[1]
        for index, spawn in enumerate(values):
            own = events_by_thread.get(spawn.thread_id, [])
            activity_end = max(
                (event.timestamp_ms + 1000 for event in own),
                default=spawn.timestamp_ms + 1000,
            )
            next_start = values[index + 1].timestamp_ms if index + 1 < len(values) else None
            ended = min(activity_end, next_start) if next_start is not None else activity_end
            parent_path = next(
                (
                    agent.agent_path
                    for agent in agents
                    if agent.thread_id == spawn.parent_thread_id
                ),
                "/root",
            )
            agents.append(
                Agent(
                    thread_id=spawn.thread_id,
                    parent_thread_id=spawn.parent_thread_id,
                    agent_path=f"{parent_path.rstrip('/')}/{official_name}",
                    nickname=None,
                    role="worker",
                    depth=_agent_depth(spawn.thread_id, parents),
                    started_at_ms=spawn.timestamp_ms,
                    ended_at_ms=max(spawn.timestamp_ms + 1, ended),
                    status="completed",
                    source_path=spawn.source_path,
                )
            )

    propagated_ends = {
        agent.thread_id: agent.ended_at_ms or agent.started_at_ms + 1
        for agent in agents
    }
    for agent in sorted(agents, key=lambda item: item.depth, reverse=True):
        if agent.parent_thread_id is None:
            continue
        child_end = propagated_ends[agent.thread_id]
        propagated_ends[agent.parent_thread_id] = max(
            propagated_ends.get(agent.parent_thread_id, child_end), child_end
        )
    agents = [
        replace(agent, ended_at_ms=propagated_ends[agent.thread_id])
        for agent in agents
    ]

    agent_ids = {agent.thread_id for agent in agents}
    filtered_events = tuple(
        event for event in normalized_events if event.thread_id in agent_ids
    )
    edges = [
        Edge(
            edge_id=f"orc-continuation-{link.session_id}",
            call_id=f"orc-continuation-{link.session_id}",
            from_thread_id=link.predecessor_session_id,
            to_thread_id=link.session_id,
            kind="continuation",
            timestamp_ms=link.predecessor_at_ms,
            message_text=(
                "Explicit Orc session continuation. Predecessor source "
                f"{link.predecessor_source_path} row "
                f"{link.predecessor_source_line} was recorded at "
                f"{link.predecessor_at_ms} ms UTC; successor source "
                f"{link.source_path} started at {link.started_at_ms} ms UTC "
                f"({link.gap_ms} ms later)."
            ),
            content_availability="plaintext",
            encrypted_content=None,
            source_line=link.predecessor_source_line,
        )
        for link in continuation_links
    ]
    edges.extend(
        [
        Edge(
            edge_id=f"orc-spawn-{spawn.thread_id}",
            call_id=f"orc-spawn-{spawn.source_line}",
            from_thread_id=spawn.parent_thread_id,
            to_thread_id=spawn.thread_id,
            kind="spawn",
            timestamp_ms=spawn.timestamp_ms,
            message_text=None,
            content_availability="none",
            encrypted_content=None,
            source_line=spawn.source_line,
        )
        for spawn in sorted(
            all_spawns, key=lambda item: (item.timestamp_ms, item.thread_id)
        )
        if spawn.parent_thread_id in agent_ids
        ]
    )
    edges.extend(
        Edge(
            edge_id=f"orc-message-{event.event_id}",
            call_id=event.event_id,
            from_thread_id=event.thread_id,
            to_thread_id=event.recipient,
            kind="message",
            timestamp_ms=event.timestamp_ms,
            message_text=event.text,
            content_availability=event.content_availability,
            encrypted_content=event.encrypted_content,
            source_line=event.source_line,
        )
        for event in task_events
        if event.kind == "inter_agent_message"
        and event.thread_id in agent_ids
        and event.thread_id != event.recipient
        and event.recipient is not None
        and event.recipient in agent_ids
    )
    source_snapshots = tuple(
        SourceSnapshot(
            path=source.source_path,
            thread_id=source.owner_session_id,
            size_bytes=source.snapshot_size,
            mtime_ns=_snapshot_path(snapshot_root, source.snapshot_path).stat().st_mtime_ns,
            sha256=source.sha256,
            complete_bytes=source.snapshot_size,
            line_count=(
                source.task_projection.note_count
                if source.kind == "task" and source.task_projection is not None
                else source.append_count
            ),
            semantic_sha256=source.semantic_sha256,
            semantic_complete_bytes=source.semantic_complete_bytes,
        )
        for source in sorted(source_copies, key=lambda item: item.source_path)
    )
    return TeamData(
        team_slug=team_slug,
        provider="orc",
        root_thread_id=root_session_id,
        display_timezone=display_timezone,
        sources=source_snapshots,
        agents=tuple(sorted(agents, key=lambda item: (item.started_at_ms, item.thread_id))),
        turns=tuple(sorted(turns, key=lambda item: (item.started_at_ms, item.turn_id))),
        events=filtered_events,
        tool_calls=tuple(
            sorted(tools, key=lambda item: (item.started_at_ms, item.call_id))
        ),
        edges=tuple(sorted(edges, key=lambda item: (item.timestamp_ms, item.edge_id))),
        task_notes=tuple(sorted(task_notes, key=task_note_key)),
    )


__all__ = [
    "OrcAppendPrefixOverride",
    "OrcContinuationLink",
    "OrcContinuationSpec",
    "OrcParseError",
    "OrcPrefixColumnChange",
    "OrcPrefixRowChange",
    "OrcSnapshotResult",
    "OrcSourceCopy",
    "load_orc_team",
    "prune_orc_snapshot_objects",
    "snapshot_orc_lineage",
]
