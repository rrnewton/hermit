#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Stress/flakiness SERIES store: append-only history per (cell, tree, run).

WHY A SECOND STORE AND NOT A COLUMN ON THE FIRST. The validation ledger records
one event per validation RUN of a whole tree. This represents one observation
per CELL per REPETITION, with equal observations allowed to share one row, so a
single pressure campaign over 40 cells at N=20 represents 800 runs
against one tree. Those are different cardinalities and different lifetimes,
and folding them into the validate shards would make a month's shard unreadable
for its original purpose.

WHAT IS DELIBERATELY REUSED, not re-invented. The shard/spool/union model is
imported from `ci-hub/ledger/ledger.py` rather than copied:

  * `union_events` — deterministic merge and event-id dedup, so the same row is
    one logical row whether it is read from the live spool, the tracked shard,
    or both during publication.
  * `append_events` — one `os.write` per line in `O_APPEND`, so two producers on
    one shard interleave whole LINES and never tear one. A pressure campaign is
    exactly the concurrent-writer case that needs this.
  * `verify_append_only` — a rewritten or deleted line stays a DETECTABLE
    VIOLATION rather than an ordinary update.
  * `parse_line` — same strictness about what a line is.

Copying those would have produced a second implementation that drifts. Only the
things that are genuinely different here — the path root, the envelope version,
the row identity, and the linter — are new.

⚠️ THE PRODUCER BOUNDARY IS THE SAME AS THE LEDGER'S, FOR THE SAME REASON.
Live producers append to `ignored/ci-hub/series-spool/`; they never write the
tracked shard. The scheduled parent publisher moves those rows into tracked
`series/` shards through the serialized parent-main writer. Readers union the
spool, batches retained until the producing checkout syncs, and every tracked
shard, so a row is visible before, during and after publication. A reader that
consulted only one source would report a campaign as missing while it was being
published.
"""

from __future__ import annotations

import array
import base64
import collections
import copy
import contextlib
import ctypes
import errno
import fcntl
import hashlib
import importlib.util
import json
import locale
import marshal
import math
import os
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys
import threading
import tempfile
import time
import types
from dataclasses import dataclass, field
from datetime import datetime, timezone
from collections.abc import Callable, Generator, Iterable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ledger"))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))

import rust_toolchain  # noqa: E402

from ledger import (  # noqa: E402  (path set above)
    ConflictingEventId,
    LedgerError,
    MalformedEvent,
    append_events,
    parse_line,
    read_shard,
    union_events,
    verify_append_only,
)
import ledger as _ledger  # noqa: E402  (its source is part of _published_scan_inputs)

LEGACY_ENVELOPE_VERSION = "stress-series/v1"
HOST_FACTS_ENVELOPE_VERSION = "stress-series/v2"
ENVELOPE_VERSION = "stress-series/v3"
SCORECARD_SNAPSHOT_SCHEMA = "scorecard-series-snapshot/v1"
SCORECARD_SNAPSHOT_SOURCE = "series"
LEDGER_REPOSITORY = "https://github.com/rrnewton/hermit_test_ledger.git"
PARENT_REPOSITORY = "https://github.com/rrnewton/dev-hermit.git"
LEDGER_ROOT_ENV = "DEV_HERMIT_TEST_LEDGER_ROOT"
#: The immutable operational tool root that admitted this run, exported by
#: ``operational-tool --run-current-tool`` and by the validate/pressure units.
TOOL_ROOT_ENV = "DEV_HERMIT_TOOL_ROOT"
SHARED_CHECKER_RELATIVE = Path("ci-hub") / "series" / "check-series-rows.rs"
#: rust-script 0.36's final stderr line when cargo cannot build the script.
RUST_SCRIPT_BUILD_FAILURE = "error: Could not execute cargo"
SERIES_SPOOL_LOCK_TIMEOUT_SECONDS = 5.0
#: Retaining a refused input waits longer than a row append: the alternative to
#: waiting out a concurrent quarantine publication is losing the input's bytes.
QUARANTINE_LOCK_TIMEOUT_SECONDS = 120.0
#: One `append-parity` call kept in `parity_deferred_dir` until a publication
#: replays it. Local only, never published.
PARITY_DEFERRED_SCHEMA = "parity-deferred/v1"
#: Every field of each kind of deferred call: a row batch, or the
#: `parity.refusal` event of a refused call.
PARITY_DEFERRED_FIELDS = {
    "rows": frozenset({
        "schema", "kind", "deferred_at", "reason", "producer", "run_id", "tree",
        "emitted_at", "source_tree_dirty", "host", "input", "input_sha256", "input_bytes",
    }),
    "refusal": frozenset({"schema", "kind", "deferred_at", "reason", "event"}),
}
SCORECARD_SNAPSHOT_MAX_DIRECTORY_ENTRIES = 4096
SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS = 1.0
SCORECARD_SNAPSHOT_USAGE = """usage:
  series.py snapshot --parent <dev-hermit root> --output <run-scoped.json>

Create one immutable scorecard-series-snapshot/v1 file from the exact parent
origin/main series tree and the bounded local series sources.

options:
  --parent <dev-hermit root>  workspace containing the ledger checkout and local spool
  --output <run-scoped.json>  new snapshot path; an existing different file refuses
  -h, --help                  show this help and exit successfully"""
RETRACT_PARITY_USAGE = """usage:
  series.py retract-parity --parent <dev-hermit root> --reason <text>
      [--emitted-at <YYYY-MM-DDTHH:MM:SSZ>] < event-ids.txt

Spool one parity.retraction withdrawing published parity events, one 64-hex
event id per stdin line, in the parent's UNPUBLISHED parity spool. The
publisher appends it to parity/<team>/<host>/<YYYY-MM>.jsonl beside the rows
it withdraws, only when every id it names is already published and none is a
retraction; every reader then drops those events (parity summary format v4,
section 9). Nothing published is deleted or rewritten.

options:
  --parent <dev-hermit root>           workspace whose local parity spool receives the retraction
  --reason <text>                      why the events are withdrawn; public, no owner paths
  --emitted-at <YYYY-MM-DDTHH:MM:SSZ>  the retraction's emission time in UTC; defaults to now
  -h, --help                           show this help and exit successfully

exit status: 0 when the retraction was spooled, 1 when it was refused (an id
not published, or a malformed id or reason), 2 for a usage error."""
RETRACT_QUARANTINE_USAGE = """usage:
  series.py retract-quarantine --parent <dev-hermit root> --reason <text>
      [--emitted-at <YYYY-MM-DDTHH:MM:SSZ>] < quarantine-event-ids.txt

Spool one series.quarantine-retraction withdrawing published series
quarantine records (refused inputs), one series-quarantine-<64 hex> event id
per stdin line, in the parent's UNPUBLISHED parity spool. The publisher
appends it to quarantine/retractions/<team>/<host>/<YYYY-MM>.jsonl only when
every id it names is a published quarantine record; series readmission and
the website export then drop those records. Nothing published is deleted or
rewritten.

options:
  --parent <dev-hermit root>           workspace whose local parity spool receives the retraction
  --reason <text>                      why the records are withdrawn; printable ASCII, no owner paths
  --emitted-at <YYYY-MM-DDTHH:MM:SSZ>  the retraction's emission time in UTC; defaults to now
  -h, --help                           show this help and exit successfully

exit status: 0 when the retraction was spooled, 1 when it was refused (an id
not published, or a malformed id or reason), 2 for a usage error."""
#: `series.py retract-parity`'s valued flags and the option each sets.
RETRACT_PARITY_FLAGS = {"--parent": "parent", "--reason": "reason", "--emitted-at": "emitted_at"}
#: `series.py append-parity`'s valued flags and the option each sets. Its
#: usage below names every one; a test holds the two together.
APPEND_PARITY_FLAGS = {
    "--parent": "parent", "--producer": "producer", "--run-id": "run_id",
    "--tree": "tree", "--emitted-at": "emitted_at",
    "--source-tree-dirty": "source_tree_dirty",
}
APPEND_PARITY_USAGE = """usage:
  series.py append-parity --parent <dev-hermit root> --producer <validate|pressure-test|parity-all>
      --run-id <id> --tree <40-hex sha> --source-tree-dirty <true|false>
      [--emitted-at <YYYY-MM-DDTHH:MM:SSZ>] < parity-ledger-sources.jsonl

Spool one run's parity rows, one hermit ParityLedgerSource JSON object per
stdin line, as one batch behind a run marker in the parent's UNPUBLISHED
parity spool. Empty stdin records a zero-record marker. When the call names a
valid producer, run and tree, refused rows or a missing or invalid
--source-tree-dirty are spooled as a parity.refusal carrying the message.

options:
  --parent <dev-hermit root>           workspace whose local parity spool receives the batch
  --producer <validate|pressure-test|parity-all>
                                       the producer that measured the rows
  --run-id <id>                        the producing run; a row naming another run is refused
  --tree <40-hex sha>                  the Hermit tree; a row naming another tree is refused
  --source-tree-dirty <true|false>     whether that tree had uncommitted changes; never defaulted
  --emitted-at <YYYY-MM-DDTHH:MM:SSZ>  the batch's emission time in UTC; defaults to now
  -h, --help                           show this help and exit successfully

When the spool lock times out or the spool cannot be written, the call is kept
in the parent's parity-deferred directory and the next series publication
appends it (or records its refusal) at its original --emitted-at.

exit status: 0 when the batch was written, was already recorded identically,
or was deferred, 1 when the rows were refused, 2 for a usage error or for a
call that could be neither written nor deferred.
Help alone writes nothing; help with any other argument is a usage error."""
READABLE_ENVELOPE_VERSIONS = frozenset({
    LEGACY_ENVELOPE_VERSION,
    HOST_FACTS_ENVELOPE_VERSION,
    ENVELOPE_VERSION,
})

#: The four divergence coordinates, stored as RAW MEASURED POSITIONS. A row may
#: count repeated observations only when every stored position is exactly equal.
#:
#: ⚠️ THESE ARE FOUR DIFFERENT KEYSPACES AND MUST NEVER BE COMPARED ACROSS AXES.
#: One real divergence was record 98, syscall 37, scheduler turn 4 -- three
#: different numbers for one event. `ci/compat-envelope/scorecard.rs` already
#: carries that warning on `first_divergent_syscall`; storing bounds per axis is
#: only meaningful because each axis is folded separately.
COORDINATE_KEYS = (
    "first_divergent_record",
    "first_divergent_syscall",
    "first_divergent_scheduler_turn",
    "first_divergent_virtual_nanoseconds",
)

FIRST_DIVERGENT_MESSAGE_KEYS = ("left", "right")
WORKSPACE_TOKEN = "{{WORKSPACE_ROOT}}"
RECORDED_ROOT = "/repo"
# Unicode White_Space (not Python's broader str.isspace set) plus the exact
# punctuation that delimits path tokens in recorded argv/env/shell text.
_EVIDENCE_PATH_DELIMITERS = frozenset(
    "\u0009\u000a\u000b\u000c\u000d\u0020\u0085\u00a0\u1680"
    "\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a"
    "\u2028\u2029\u202f\u205f\u3000"
    "=,:;()[]{}<>\"'"
)

_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
PRESSURE_PROMOTION_REPETITIONS = 10
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_PROJECTION_RESULTS_SCHEMA_VERSION = 1
_WRITE_RESULTS_SCHEMA_VERSION = 1
QUARANTINE_SCHEMA = "series-quarantine/v2"
#: A published v2 record whose input bytes are kept in their own ledger files
#: (quarantine_input_path) instead of in the row; every other field, the
#: event_id included, is the v2 record's. See slim_quarantine_record.
QUARANTINE_REF_SCHEMA = "series-quarantine/v3"
QUARANTINE_INPUTS_ROOT = "quarantine/inputs"
#: The largest input file a v3 record names: 48 MiB, the ceiling the ledger
#: already uses for a series shard (publisher.SERIES_SHARD_CEILING_BYTES) and a
#: scorecard part (scorecard_cells.FILE_CEILING_BYTES), under GitHub's 50 MiB
#: warning. A record with a larger input stays v2, inside its shard and so
#: under the publisher's 100,000,000-byte quarantine shard ceiling
#: (publisher.QUARANTINE_SHARD_CEILING_BYTES). The largest input published by
#: hermit_test_ledger e2bef897 is 1,361,997 bytes.
QUARANTINE_INPUT_FILE_CEILING_BYTES = 48 * 1024 * 1024
# Written by series.py from f44dd42e0d until v2 replaced it. It stored the
# PARSED object and a digest of its canonical re-serialization, never the
# received bytes, so it is upgraded on publication and labelled as such.
LEGACY_QUARANTINE_SCHEMA = "series-quarantine/v1"
QUARANTINE_EVENT_TYPE = "series.quarantine"
QUARANTINE_SUMMARY_SCHEMA = "series-quarantine-summary/v1"
QUARANTINE_STAGES = frozenset({
    "parse", "projection", "shared-checker", "spool-conflict", "batch", "writer-error",
})
#: Exact records whose content cannot be published without an owner path.
QUARANTINE_HELD_SUBDIR = "held"
#: Top-level quarantine files the publisher could not read, moved aside whole so
#: the readable ones still publish. Never republished or deleted; the backlog
#: gate counts every file here as held back from publication and alarms.
QUARANTINE_CORRUPT_SUBDIR = "corrupt"
# Built rather than written literally so this module does not itself contain the
# owner-path needle the portability check greps tracked files for.
_OWNER_PATH_NEEDLE = "/" + "home" + "/"
_GIT_REPO_SCOPED_ENV = (
    "GIT_INDEX_FILE", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE", "GIT_PREFIX", "GIT_INDEX_VERSION",
)


class SeriesError(LedgerError):
    """Any series-store violation."""


class MalformedRow(SeriesError):
    """A row that does not satisfy the schema."""


class _UntypedComparisonCell(MalformedRow):
    """A pressure comparison result predating typed attempt evidence."""


@dataclass(frozen=True)
class PublishedSnapshot:
    """One immutable published-series revision and its canonical rows."""

    commit: str
    rows: list[dict]
    # Older snapshots named the parent repository implicitly.
    repository: str = PARENT_REPOSITORY
    files: Any = field(default=None, repr=False, compare=False)

    def verify(self) -> None:
        if self.files is not None:
            self.files.verify(self.files.listing(("series",)))

    def close(self) -> None:
        if self.files is not None:
            self.files.close()


@dataclass(frozen=True)
class ProjectionResults:
    """Hermit's projection result for every row in one canonical input."""

    row_count: int
    input_jsonl_sha256: str
    results: dict[str, str | None]


def shard_path(team: str, host: str, month: str) -> str:
    """`series/<team>/<host>/<YYYY>-<MM>.jsonl`.

    Beside `ledger/`, never inside it: two roots keep a `git log` over
    validation history readable when a campaign emits 800 rows in a minute.
    The path is a storage index derived from the row's authoritative `host`
    field. Moving or republishing a row cannot change the machine it describes;
    the v2 type also requires `host == series.machine_shortname`. Sharding per
    team and short host is what makes two machines merge trivially.
    """
    if not re.fullmatch(r"\d{4}-\d{2}", month or ""):
        raise MalformedRow(f"month must be YYYY-MM, got {month!r}")
    for name, value in (("team", team), ("host", host)):
        if not value or "/" in value or value in (".", ".."):
            raise MalformedRow(f"{name} must be a single path segment, got {value!r}")
    return f"series/{team}/{host}/{month}.jsonl"


#: The highest part ``shard_part_path`` names. Four digits keep every part name
#: the same length, so path order is part order.
SHARD_PART_LIMIT = 9999
_SHARD_PART_NAME_RE = re.compile(r"(\d{4}-\d{2})(?:\.part-(\d{4}))?\.jsonl")


def shard_part_path(team: str, host: str, month: str, part: int) -> str:
    """`series/<team>/<host>/<YYYY>-<MM>.part-<NNNN>.jsonl`, or `shard_path` for part 0.

    One host's month no longer fits in one file. GitHub refuses a pushed file
    over 104,857,600 bytes (GH001): devbig014's September shard reached
    97,179,293 bytes on 2026-09-30 and was split by hand, and devbig030's
    October shard was 68,594,348 bytes on 2026-10-03, growing about 20 MB a
    day (https://github.com/rrnewton/dev-hermit/issues/453). The publisher
    therefore appends to a month's highest part only while it stays within
    ``publisher.SERIES_SHARD_CEILING_BYTES`` and then starts the next part.

    A part name sorts after the month's own shard (``.p`` after ``.j``) and
    after every lower part, so a reader that reads a month's files in path
    order still reads its rows in publication order. Every series reader
    already reads every ``series/*/*/*.jsonl`` file.
    """
    if type(part) is not int or not 0 <= part <= SHARD_PART_LIMIT:
        raise MalformedRow(f"shard part must be an integer from 0 to {SHARD_PART_LIMIT}, got {part!r}")
    path = shard_path(team, host, month)
    if part == 0:
        return path
    return f"{path.removesuffix('.jsonl')}.part-{part:04d}.jsonl"


def parse_shard_part_path(path: Any) -> tuple[str, str, str, int] | None:
    """``(team, host, month, part)`` for a name ``shard_part_path`` gives, else None.

    Only canonical names: the month's own shard is part 0, and a
    ``.part-NNNN`` suffix names part 1 or later in exactly four digits. A
    hand-split file such as ``2026-09-01-to-12.jsonl`` is not a part. The
    publisher checks new rows against it but never appends to it.
    """
    text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else None
    if not isinstance(text, str):
        return None
    pieces = text.split("/")
    if len(pieces) != 4 or pieces[0] != "series":
        return None
    match = _SHARD_PART_NAME_RE.fullmatch(pieces[3])
    if match is None:
        return None
    month, digits = match.groups()
    part = 0 if digits is None else int(digits)
    try:
        canonical = shard_part_path(pieces[1], pieces[2], month, part)
    except MalformedRow:
        return None
    if canonical != text:
        return None
    return pieces[1], pieces[2], month, part


def is_month_shard_name(name: str, month: str) -> bool:
    """Whether a file in a host's series directory holds rows of ``month``.

    Every part of the month, and any hand-split file named after it, such as
    ``2026-09-01-to-12.jsonl``. The publisher checks each new row's identity
    against all of them, so a row is published at most once in its month.
    """
    if not re.fullmatch(r"\d{4}-\d{2}", month or ""):
        raise MalformedRow(f"month must be YYYY-MM, got {month!r}")
    return re.fullmatch(re.escape(month) + r"(?:[.-][^/]*)?\.jsonl", name) is not None


def shared_parent_root(parent: Any) -> Path:
    """Return the primary checkout root shared by a linked parent worktree.

    A series producer commonly runs from a detached dev-hermit worktree. If its
    spool stayed under that worktree, the scheduled publisher rooted at the
    primary checkout would never see it. The validation ledger already resolves
    the same Git common directory for this reason. Independent clones keep their
    own ``.git`` directory and therefore remain independent producer roots.

    Only a checkout's own top-level directory is redirected. Git discovery
    walks up from any directory, so without that check a directory merely
    nested inside a checkout (a test's ``TemporaryDirectory`` under a scratch
    ``TMPDIR`` in ``ignored/``) resolved to the shared checkout itself. On
    2026-10-07 that put test fixtures (a ``{}`` batch, conflicting
    ``campaign-1`` rows) in the live series spool, and the next validate's
    append-cells refused all 1102 of its cell rows on the ``{}`` line.
    """
    parent = Path(parent).resolve()
    env = {name: value for name, value in os.environ.items() if not name.startswith("GIT_")}
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    try:
        result = subprocess.run(
            [
                "git", "--no-replace-objects", "-C", str(parent), "rev-parse",
                "--path-format=absolute", "--show-toplevel", "--git-common-dir",
            ],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )
    except (OSError, subprocess.TimeoutExpired):
        return parent
    lines = result.stdout.splitlines()
    if result.returncode != 0 or len(lines) != 2 or not all(line.strip() for line in lines):
        return parent
    if Path(lines[0]).resolve() != parent:
        return parent
    common_dir = Path(lines[1]).resolve()
    if common_dir.name != ".git":
        return parent
    shared_root = common_dir.parent
    if not (shared_root / "ci-hub" / "series" / "series.py").is_file():
        return parent
    return shared_root


def spool_dir(parent: Any) -> Path:
    """The live, UNPUBLISHED spool in the shared parent checkout."""
    return shared_parent_root(parent) / "ignored" / "ci-hub" / "series-spool"


def quarantine_dir(parent: Any) -> Path:
    """The live, UNPUBLISHED spool of refused series inputs.

    A cell result the writer refuses is kept here with its exact received bytes
    and a typed reason, instead of being discarded or taking its valid siblings
    with it. The ledger publisher (``publisher.py --store series-quarantine``,
    run by ``mirror.py`` beside every series publication) appends these records
    to ``quarantine/series/<team>/<host>/<YYYY-MM-DDTHH>.jsonl`` in the test ledger,
    outside the ``series/`` tree that every series reader globs. Its records are
    typed ``series-quarantine/v2``, never ``SeriesRow``.
    """
    return shared_parent_root(parent) / "ignored" / "ci-hub" / "series-quarantine"


def quarantine_held_dir(parent: Any) -> Path:
    """Exact refused inputs whose content cannot be published portably.

    The test ledger is public and carries no owner path. A record that cannot
    be made public by owner-home token substitution is published as a
    content-free stub (digest, length, reason). A record published with its
    content can still name a token that no prefix this host knows restores
    (``published_quarantine_restores``). Either way its exact bytes then exist
    only here, on this host: the ledger cannot restore them. Nothing deletes
    this directory, and ``ledger_spool_backlog --store series-quarantine``
    alarms while it holds anything.
    """
    return quarantine_dir(parent) / QUARANTINE_HELD_SUBDIR


def quarantine_bucket(emitted_at: str) -> str:
    """The hour a refusal publishes under: ``YYYY-MM-DDTHH`` of ``emitted_at``.

    A month was too coarse. Each record keeps the exact refused input, about
    25 KB, and in September and October 2026 one host refused thousands of rows
    a day: devbig030's pending 2026-10 shard reached 257.7 MB and GitHub
    refused it (GH001, over 100 MB), which stopped every quarantine publication
    (https://github.com/rrnewton/dev-hermit/issues/499). Over the 9,749 local
    records measured on 2026-10-03, the largest day was 154.3 MB and the
    largest hour 48.1 MB.
    """
    if not isinstance(emitted_at, str) or _QUARANTINE_EMITTED_AT_RE.fullmatch(emitted_at) is None:
        raise MalformedRow(f"quarantine emitted_at must be a timestamp with an hour, got {emitted_at!r}")
    return emitted_at[:13]


def quarantine_shard_path(team: str, host: str, bucket: str) -> str:
    """`quarantine/series/<team>/<host>/<YYYY>-<MM>-<DD>T<HH>.jsonl` in the test ledger.

    Deliberately outside `series/`: the scorecard, the website, and the
    published-snapshot reader all read `series/*/*/*.jsonl` (the scorecard
    recursively), and a quarantine record is not a `SeriesRow`.

    New records publish into hourly shards (``quarantine_bucket``). A
    ``YYYY-MM`` bucket names a monthly shard published before that change;
    those stay readable and are never appended to again, since new records
    always take an hourly bucket.
    """
    if re.fullmatch(r"\d{4}-\d{2}", bucket or ""):
        return "quarantine/" + shard_path(team, host, bucket)
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}", bucket or ""):
        raise MalformedRow(f"quarantine bucket must be YYYY-MM-DDTHH, got {bucket!r}")
    shard_path(team, host, bucket[:7])  # validates the team and host segments
    return f"quarantine/series/{team}/{host}/{bucket}.jsonl"


def quarantine_shard_part_path(team: str, host: str, bucket: str, part: int) -> str:
    """A quarantine bucket's base or a four-digit part after a retired file."""
    if type(part) is not int or not 0 <= part <= SHARD_PART_LIMIT:
        raise MalformedRow(f"quarantine shard part must be from 0 to {SHARD_PART_LIMIT}")
    base = quarantine_shard_path(team, host, bucket)
    return base if part == 0 else f"{base.removesuffix('.jsonl')}.part-{part:04d}.jsonl"


def parse_quarantine_shard_part_path(path: Any) -> tuple[str, str, str, int] | None:
    """The original team, host, bucket and canonical part, or None."""
    if not isinstance(path, str):
        return None
    pieces = path.split("/")
    if len(pieces) != 5 or pieces[:2] != ["quarantine", "series"]:
        return None
    matched = re.fullmatch(
        r"(\d{4}-\d{2}(?:-\d{2}T\d{2})?)(?:\.part-([0-9]{4}))?\.jsonl", pieces[-1])
    if matched is None:
        return None
    bucket, digits = matched.groups()
    part = 0 if digits is None else int(digits)
    try:
        canonical = quarantine_shard_part_path(pieces[2], pieces[3], bucket, part)
    except SeriesError:
        return None
    return (pieces[2], pieces[3], bucket, part) if canonical == path else None


def file_archive_module():
    """The storage reader from this tool tree; captured builds bind this getter."""
    directory = Path(__file__).resolve().parents[1] / "ledger"
    if str(directory) not in sys.path:
        sys.path.insert(0, str(directory))
    import file_archive
    if Path(str(getattr(file_archive, "__file__", ""))).resolve() != directory / "file_archive.py":
        raise SeriesError("file archive reader was imported from a different tool tree")
    return file_archive


def file_archive_snapshot(repo: Path, commit: str, *, files: Any = None):
    """One authenticated original-file population at the exact resolved commit."""
    try:
        if _SHA_RE.fullmatch(commit) is None:
            resolved = _git(Path(repo), ["--no-replace-objects", "rev-parse", "--verify",
                                        f"{commit}^{{commit}}"])
            if resolved.returncode != 0 or _SHA_RE.fullmatch(resolved.stdout.strip()) is None:
                raise SeriesError(f"cannot resolve file archive revision {commit}")
            commit = resolved.stdout.strip()
        snapshot = files if files is not None else file_archive_module().FileArchiveSnapshot(
            Path(repo), commit)
        if snapshot.commit != commit:
            raise SeriesError("file archive snapshot names a different source commit")
        return snapshot
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise SeriesError(f"cannot read file archives at {commit}: {error}") from error


@contextlib.contextmanager
def file_archive_scope(repo: Path, commit: str, *, files: Any = None):
    """Close a private storage handle; leave an explicitly shared one owned by its caller."""
    snapshot = file_archive_snapshot(repo, commit, files=files)
    try:
        yield snapshot
    finally:
        if files is None:
            snapshot.close()


def validate_archive_file(path: str, data: bytes,
                          inputs: Mapping[str, bytes | None]) -> Sequence[Mapping[str, Any]]:
    """Native logical events per original nonempty line, without deduplication.

    The archive builder keeps the raw bytes independently. This callback is
    the existing admission boundary, including v3 quarantine input expansion;
    storage metadata alone never admits a row or changes a refusal's body.
    """
    pieces = Path(path).parts
    if _QUARANTINE_REF_RE.fullmatch(path):
        return ()
    lines = _jsonl_lines(path, data)
    try:
        text_lines = [line.decode("utf-8") for line in lines]
    except UnicodeDecodeError as error:
        raise MalformedEvent(f"{path}: not UTF-8: {error}") from error
    if len(pieces) == 4 and pieces[0] == "series" and path.endswith(".jsonl"):
        return _validate_rows([parse_line(line) for line in text_lines], strict_paths=True,
                              read=True, admitted=_archive_read_admitted)
    if len(pieces) == 5 and pieces[:2] == ("quarantine", "series") and path.endswith(".jsonl"):
        records = validate_quarantine_records([parse_line(line) for line in text_lines], public=True)
        return [expand_quarantine_record(record, inputs) for record in records]
    parity = parity_ledger_module()
    if parity.is_shard_path(path) and pieces[:2] == ("quarantine", "retractions"):
        events = [parity.loads(line, path) for line in text_lines]
        for event in events:
            parity.validate_retraction(event)
            if event["event_type"] != parity.QUARANTINE_RETRACTION_EVENT_TYPE:
                raise MalformedQuarantine(f"{path}: not a quarantine retraction")
        return events
    raise SeriesError(f"{path}: not an original series or quarantine source file")


#: Exact ``--read`` checker inputs that the shared checker admitted inside a
#: larger batch (``prime_archive_file_validation``), keyed by the checker's
#: identity and the SHA-256 of one original series file's complete checker
#: input. Only admissions are recorded; nothing here can refuse a row.
_ARCHIVE_READ_ADMITTED: set[tuple[str, str, str]] = set()

#: Checker input per batched invocation. The scheduled compaction of
#: 2026-10-25 validated 12,420 series files one rust-script process at a time,
#: about 100 files a minute and four times over (archive build, two proposed
#: views, render view). One published scan already sends 3.16 million rows to
#: one invocation; 64 MiB keeps each call far inside its 300 s bound and keeps
#: the bisection that locates a refused file cheap.
ARCHIVE_VALIDATION_BATCH_BYTES = 64 * 1024 * 1024


def _archive_checker_identity() -> tuple[str, str]:
    """The checker a later per-file call would run: its script and tool root."""
    return (str(_shared_checker_script(os.environ)), os.environ.get(TOOL_ROOT_ENV, ""))


def _archive_read_admitted(checker_input: str) -> bool:
    if not _ARCHIVE_READ_ADMITTED:
        return False
    try:
        identity = _archive_checker_identity()
    except SeriesError:
        return False  # the per-file call reports the same refusal itself
    digest = hashlib.sha256(checker_input.encode()).hexdigest()
    return (*identity, digest) in _ARCHIVE_READ_ADMITTED


def _archive_series_checker_input(path: str, data: bytes) -> str | None:
    """The exact checker input ``validate_archive_file`` builds for a series file.

    None for a file that is not an original series shard or has no rows (the
    per-file path never invokes the checker for those). Any Python-side
    refusal raises exactly as it does there.
    """
    pieces = Path(path).parts
    if not (len(pieces) == 4 and pieces[0] == "series" and path.endswith(".jsonl")):
        return None
    lines = _jsonl_lines(path, data)
    try:
        text_lines = [line.decode("utf-8") for line in lines]
    except UnicodeDecodeError as error:
        raise MalformedEvent(f"{path}: not UTF-8: {error}") from error
    checked = _checked_rows([parse_line(line) for line in text_lines], strict_paths=True)
    return _checker_input(checked) if checked else None


def _prime_archive_chunk(chunk: list[tuple[str, bytes]], batch_bytes: int) -> tuple[list[str], dict]:
    """One worker's share of ``prime_archive_file_validation``: admitted digests and counts."""
    before = set(_ARCHIVE_READ_ADMITTED)
    stats = prime_archive_file_validation(chunk, batch_bytes=batch_bytes, workers=1)
    return [digest for (*_identity, digest) in _ARCHIVE_READ_ADMITTED - before], stats


def prime_archive_file_validation(
    files: Iterable[tuple[str, bytes]], *, batch_bytes: int = ARCHIVE_VALIDATION_BATCH_BYTES,
    workers: int = 1,
) -> dict[str, Any]:
    """Run the shared ``--read`` checker over many series files in few invocations.

    ``validate_archive_file`` is unchanged in what it decides. It still parses
    and checks every file in Python, and it still invokes the checker itself
    for any file whose exact checker input is not recorded here. A batch is
    recorded only when the checker admits the whole concatenation. In
    ``--read`` mode the checker judges each nonempty line on its own and keeps
    no state across lines (its only whole-input refusal is an empty input,
    which a per-file call never sends), so admitting the concatenation admits
    each file's own input. A refused batch is split in halves until each
    refused file stands alone and is left unrecorded: its per-file call then
    produces the exact refusal message. A file that Python already refuses is
    skipped for the same reason, and a checker that cannot run stops priming
    without recording anything further.
    """
    stats: dict[str, Any] = {"files": 0, "admitted": 0, "not_admitted": 0,
                             "python_refused": 0, "invocations": 0, "tool_error": None}
    if type(batch_bytes) is not int or batch_bytes < 1:
        raise SeriesError("archive validation batch size must be a positive byte count")
    try:
        identity = _archive_checker_identity()
    except SeriesError as error:
        stats["tool_error"] = str(error)
        return stats
    if type(workers) is not int or workers < 1:
        raise SeriesError("archive validation workers must be a positive count")
    if workers > 1:
        return _prime_concurrently(files, batch_bytes=batch_bytes, workers=workers,
                                   identity=identity, stats=stats)

    def judge(batch: list[tuple[str, str]]) -> None:
        if not batch:
            return
        stats["invocations"] += 1
        result = _run_shared_checker("".join(text for _digest, text in batch), ["--read"])
        if result.returncode == 0:
            for digest, _text in batch:
                _ARCHIVE_READ_ADMITTED.add((*identity, digest))
            stats["admitted"] += len(batch)
        elif len(batch) == 1:
            stats["not_admitted"] += 1
        else:
            middle = len(batch) // 2
            judge(batch[:middle])
            judge(batch[middle:])

    pending: list[tuple[str, str]] = []
    pending_bytes = 0
    try:
        for path, data in files:
            try:
                checker_input = _archive_series_checker_input(path, data)
            except Exception:  # the per-file call raises the identical refusal
                stats["files"] += 1
                stats["python_refused"] += 1
                continue
            if checker_input is None:
                continue
            stats["files"] += 1
            pending.append((hashlib.sha256(checker_input.encode()).hexdigest(), checker_input))
            pending_bytes += len(checker_input)
            if pending_bytes >= batch_bytes:
                judge(pending)
                pending, pending_bytes = [], 0
        judge(pending)
    except SeriesError as error:
        # A tool failure is not a verdict; every unrecorded file falls back to
        # its own per-file call, which raises this failure as before.
        stats["tool_error"] = str(error)
    return stats


def _prime_concurrently(files: Iterable[tuple[str, bytes]], *, batch_bytes: int, workers: int,
                        identity: tuple[str, str], stats: dict[str, Any]) -> dict[str, Any]:
    """Prime in worker processes; the Python parsing is most of the cost.

    Each worker runs the same single-process priming over whole files and
    returns only the digests the checker admitted. Workers are forks of this
    process, so their checker is the one this process's identity names; a
    digest is recorded here under that identity.
    """
    import multiprocessing
    from concurrent.futures import ProcessPoolExecutor
    totals = ("files", "admitted", "not_admitted", "python_refused", "invocations")
    pending: list = []
    try:
        pool = ProcessPoolExecutor(max_workers=workers, mp_context=multiprocessing.get_context("fork"))
    except (OSError, ValueError) as error:
        stats["tool_error"] = f"cannot start priming workers: {error}"
        return stats
    with pool:
        def collect(limit: int) -> None:
            while len(pending) > limit:
                try:
                    digests, part = pending.pop(0).result()
                except Exception as error:  # priming only ever adds admissions
                    stats["tool_error"] = stats["tool_error"] or f"priming worker failed: {error}"
                    continue
                for digest in digests:
                    _ARCHIVE_READ_ADMITTED.add((*identity, digest))
                for name in totals:
                    stats[name] += part[name]
                if part["tool_error"] and not stats["tool_error"]:
                    stats["tool_error"] = part["tool_error"]

        chunk: list[tuple[str, bytes]] = []
        size = 0
        try:
            for path, data in files:
                chunk.append((path, data))
                size += len(data)
                if size >= batch_bytes:
                    pending.append(pool.submit(_prime_archive_chunk, chunk, batch_bytes))
                    chunk, size = [], 0
                    collect(2 * workers)  # bound the bytes in flight
            if chunk:
                pending.append(pool.submit(_prime_archive_chunk, chunk, batch_bytes))
        except RuntimeError as error:  # a broken pool: the unprimed files validate one by one
            stats["tool_error"] = stats["tool_error"] or f"priming workers stopped: {error}"
        collect(0)
    return stats


def clear_archive_file_validation() -> None:
    """Forget every batched admission (tests, and long-lived callers)."""
    _ARCHIVE_READ_ADMITTED.clear()


def archive_file_input_paths(path: str, data: bytes) -> tuple[str, ...]:
    """Strict public v3 input names needed before validating an archive batch."""
    pieces = Path(path).parts
    if not (len(pieces) == 5 and pieces[:2] == ("quarantine", "series")
            and path.endswith(".jsonl")):
        return ()
    try:
        records = [parse_line(line.decode("utf-8")) for line in _jsonl_lines(path, data)]
    except UnicodeDecodeError as error:
        raise MalformedEvent(f"{path}: not UTF-8: {error}") from error
    checked = validate_quarantine_records(records, public=True)
    return tuple(sorted({item["raw_ref"] for record in checked
                         if record["schema"] == QUARANTINE_REF_SCHEMA
                         for item in record["inputs"]}))


PARITY_SCHEMA = "parity-ledger/v1"
_PARITY_LEDGER_MODULE = "ci_hub_series_parity_ledger"


def parity_ledger_module():
    """The parity ledger store (`parity_ledger.py`), loaded once per process.

    Every loader (this module, the ledger publisher, the mirror, the website
    exporter) shares this one module object, so `ParityLedgerError` is one
    class wherever it is caught. The module is registered before it runs:
    a dataclass looks its module up in `sys.modules` while it is defined.
    """
    module = sys.modules.get(_PARITY_LEDGER_MODULE)
    if module is not None:
        return module
    path = Path(__file__).resolve().parent / "parity_ledger.py"
    spec = importlib.util.spec_from_file_location(_PARITY_LEDGER_MODULE, path)
    if spec is None or spec.loader is None:
        raise SeriesError(f"cannot load the parity ledger store from {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[_PARITY_LEDGER_MODULE] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(_PARITY_LEDGER_MODULE, None)
        raise
    return module


_READMISSION_CACHE_MODULE = "ci_hub_series_readmission_cache"


def readmission_cache_module():
    """The readmission cache (`readmission_cache.py`), loaded once per process."""
    module = sys.modules.get(_READMISSION_CACHE_MODULE)
    if module is not None:
        return module
    path = Path(__file__).resolve().parent / "readmission_cache.py"
    spec = importlib.util.spec_from_file_location(_READMISSION_CACHE_MODULE, path)
    if spec is None or spec.loader is None:
        raise SeriesError(f"cannot load the readmission cache from {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[_READMISSION_CACHE_MODULE] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(_READMISSION_CACHE_MODULE, None)
        raise
    return module


def parity_spool_dir(parent: Any) -> Path:
    """The live, UNPUBLISHED spool of parity batches, one file per run.

    `series.py append-parity` writes here; the ledger publisher
    (`publisher.py --store parity`, run by `mirror.py` in the same commit as
    the series rows) appends each batch to `parity/<team>/<host>/<YYYY-MM>.jsonl`
    and its run marker to `parity-runs/<team>/<host>/<YYYY-MM>.jsonl`. Both
    trees are outside the `series/` glob. See `parity_ledger.py` and
    https://github.com/rrnewton/hermit/issues/3301.
    """
    return shared_parent_root(parent) / "ignored" / "ci-hub" / "parity-spool"


def parity_deferred_dir(parent: Any) -> Path:
    """The live, UNPUBLISHED `append-parity` calls the parity spool did not take.

    When the parity spool lock times out or the spool write fails, `series.py
    append-parity` keeps the whole call here, one immutable file per call,
    without taking any lock. The next series publication (`mirror.py`, through
    `replay_deferred_parity`) replays each call into the parity spool before it
    publishes, so a busy spool delays a run's parity instead of losing it.
    """
    return shared_parent_root(parent) / "ignored" / "ci-hub" / "parity-deferred"


def parity_shard_path(team: str, host: str, month: str) -> str:
    """`parity/<team>/<host>/<YYYY-MM>.jsonl`: where a run's parity rows go."""
    return parity_ledger_module().shard_path("parity", team, host, month)


def parity_run_shard_path(team: str, host: str, month: str) -> str:
    """`parity-runs/<team>/<host>/<YYYY-MM>.jsonl`: one marker per appended run."""
    return parity_ledger_module().shard_path("parity-runs", team, host, month)


def ledger_root(parent: Any) -> Path:
    """Resolve the existing cell ledger; never create or silently replace it."""
    state = shared_parent_root(parent)
    configured = _git(state, [
        "config", "--file", str(state / ".gitmodules"), "--get",
        "submodule.hermit_test_ledger.url",
    ])
    if configured.returncode or configured.stdout.strip() != LEDGER_REPOSITORY:
        raise SeriesError("hermit_test_ledger submodule URL is absent or unexpected")
    override = os.environ.get(LEDGER_ROOT_ENV)
    root = Path(override).expanduser().resolve() if override else state / "hermit_test_ledger"
    if not _is_checkout(root):
        raise SeriesError(f"cell ledger checkout is unavailable: {root}")
    # get-url expands transport rewrites; the configured identity remains the
    # ledger URL even when a proxy or a local test remote supplies transport.
    remote = _git(root, ["config", "--get", "remote.origin.url"])
    if remote.returncode or remote.stdout.strip() not in {
        LEDGER_REPOSITORY, "git@github.com:rrnewton/hermit_test_ledger.git",
    }:
        raise SeriesError(f"cell ledger checkout has an unexpected origin: {root}")
    return root


def published_repository(parent: Any, repository: str) -> Path:
    """Keep historical parent SHAs in the repository that originally owned them."""
    if repository == PARENT_REPOSITORY:
        return Path(parent).resolve()
    if repository == LEDGER_REPOSITORY:
        return ledger_root(parent)
    raise SeriesError(f"unsupported series repository identity: {repository!r}")


def row_identity(row: dict) -> tuple[str, str, int, int]:
    """The (cell, tree, run, attempt) key for one execution."""
    payload = row.get("series") or {}
    return (
        str(payload.get("cell", "")),
        str(payload.get("tree", "")),
        int(payload.get("run_index", -1)),
        int(payload.get("attempt", 1)),
    )


def validate_row(row: Any, *, strict_paths: bool = True) -> dict:
    """Refuse a row that cannot be read later. The LINTER, not the reader.

    Hermit's shared Rust type owns the serialized shape and cross-field rules.
    Python retains only the parent-specific owner-path rule.
    """
    return validate_rows([row], strict_paths=strict_paths)[0]


def validate_rows(rows: Iterable[Any], *, strict_paths: bool = True) -> list[dict]:
    """Validate a batch with one invocation of the shared Rust type checker."""
    return _validate_rows(rows, strict_paths=strict_paths, read=False)


def validate_read_rows(rows: Iterable[Any], *, strict_paths: bool = True) -> list[dict]:
    """Validate stored rows, retaining readable v1 but requiring complete v2."""
    return _validate_rows(rows, strict_paths=strict_paths, read=True)


def write_admission(rows: Sequence[Any]) -> list[str | None]:
    """Return Hermit's write-boundary verdict for each row independently.

    ``None`` admits the row; a string is the exact refusal. Unlike
    ``validate_rows``, one refused row does not decide the fate of the rows
    beside it: the shared checker's ``--write-results`` mode judges every row
    in one invocation. A checker that cannot run, or that returns a result
    envelope that does not account for exactly these rows, raises
    ``SeriesError``; that is a tool failure, not a verdict about any row.
    """
    verdicts: list[str | None] = [None] * len(rows)
    candidates: list[tuple[int, dict]] = []
    for index, row in enumerate(rows):
        try:
            copied = _checked_rows([row], strict_paths=True)[0]
            _canonical_row_json(copied)
        except MalformedRow as error:
            verdicts[index] = str(error)
            continue
        candidates.append((index, copied))
    if not candidates:
        return verdicts
    checker_input = _checker_input(row for _, row in candidates)
    expected_sha256 = hashlib.sha256(checker_input.encode()).hexdigest()
    result = _run_shared_checker(checker_input, ["--write-results"])
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise SeriesError(f"shared SeriesRow write checker failed: {detail}")
    try:
        output = json.loads(result.stdout)
    except (json.JSONDecodeError, TypeError, RecursionError) as error:
        raise SeriesError(
            f"shared SeriesRow write checker returned malformed JSON: {error}"
        ) from error
    if not isinstance(output, dict) or set(output) != {
        "input_jsonl_sha256", "input_row_count", "results", "schema_version",
    }:
        raise SeriesError("shared SeriesRow write checker returned an invalid top-level shape")
    if (
        type(output["schema_version"]) is not int
        or output["schema_version"] != _WRITE_RESULTS_SCHEMA_VERSION
    ):
        raise SeriesError("shared SeriesRow write checker returned an unsupported schema_version")
    if type(output["input_row_count"]) is not int or output["input_row_count"] != len(candidates):
        raise SeriesError("shared SeriesRow write checker input_row_count disagrees with rows")
    if output["input_jsonl_sha256"] != expected_sha256:
        raise SeriesError("shared SeriesRow write checker input_jsonl_sha256 disagrees with rows")
    results = output["results"]
    if not isinstance(results, list) or len(results) != len(candidates):
        raise SeriesError("shared SeriesRow write checker did not return one result per row")
    for position, ((index, _row), item) in enumerate(zip(candidates, results), 1):
        if (
            not isinstance(item, dict)
            or set(item) != {"line", "result"}
            or type(item["line"]) is not int
            or item["line"] != position
            or not (item["result"] is None or isinstance(item["result"], str))
        ):
            raise SeriesError(
                f"shared SeriesRow write checker result {position} is malformed"
            )
        verdicts[index] = item["result"]
    return verdicts


def _validate_rows(
    rows: Iterable[Any], *, strict_paths: bool, read: bool,
    admitted: Callable[[str], bool] | None = None,
) -> list[dict]:
    """Apply Hermit's shared SeriesRow checks for one read or write boundary.

    ``admitted`` may report that this exact checker input was already admitted
    by the same checker in a batch (``prime_archive_file_validation``); only
    then is the invocation skipped.
    """
    checked = _checked_rows(rows, strict_paths=strict_paths)
    if not checked:
        return checked

    arguments = ["--read"] if read else []
    checker_input = _checker_input(checked)
    if read and admitted is not None and admitted(checker_input):
        return checked
    result = _run_shared_checker(checker_input, arguments)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise MalformedRow(detail)
    return checked


def projection_results(
    rows: Iterable[Any], *, strict_paths: bool = True,
) -> ProjectionResults:
    """Return Hermit's validate_for_projection result for every event id.

    ``None`` means the shared type admitted the row. A nonempty string is the
    exact rejection returned by ``SeriesRow::validate_for_projection``. Rows
    are sorted by their canonical JSON before invoking the checker so callers
    receive the same result map and input digest for any ordering of one row
    population.
    """
    checked = _checked_rows(rows, strict_paths=strict_paths)
    ordered = sorted(checked, key=_canonical_row_json)
    checker_input = _checker_input(ordered)
    expected_sha256 = hashlib.sha256(checker_input.encode()).hexdigest()
    if not ordered:
        return ProjectionResults(0, expected_sha256, {})

    result = _run_shared_checker(checker_input, ["--projection-results"])
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise MalformedRow(detail)
    return _parse_projection_results(
        result.stdout,
        expected_event_ids={str(row["event_id"]) for row in ordered},
        expected_row_count=len(ordered),
        expected_sha256=expected_sha256,
    )


def _parse_projection_results(
    encoded: str,
    *,
    expected_event_ids: set[str],
    expected_row_count: int,
    expected_sha256: str,
) -> ProjectionResults:
    """Validate the checker envelope before exposing any projection result."""
    try:
        output = json.loads(encoded)
    except (json.JSONDecodeError, TypeError, RecursionError) as error:
        raise SeriesError(
            f"shared SeriesRow projection checker returned malformed JSON: {error}"
        ) from error
    if not isinstance(output, dict) or set(output) != {
        "input_jsonl_sha256", "input_row_count", "results", "schema_version",
    }:
        raise SeriesError(
            "shared SeriesRow projection checker returned an invalid top-level shape"
        )
    if (
        not isinstance(output["schema_version"], int)
        or isinstance(output["schema_version"], bool)
        or output["schema_version"] != _PROJECTION_RESULTS_SCHEMA_VERSION
    ):
        raise SeriesError(
            "shared SeriesRow projection checker returned an unsupported schema_version"
        )
    input_row_count = output["input_row_count"]
    if not isinstance(input_row_count, int) or isinstance(input_row_count, bool):
        raise SeriesError(
            "shared SeriesRow projection checker returned an invalid input_row_count"
        )
    if input_row_count != expected_row_count:
        raise SeriesError(
            "shared SeriesRow projection checker input_row_count disagrees with rows"
        )
    input_jsonl_sha256 = output["input_jsonl_sha256"]
    if (
        not isinstance(input_jsonl_sha256, str)
        or _SHA256_RE.fullmatch(input_jsonl_sha256) is None
    ):
        raise SeriesError(
            "shared SeriesRow projection checker returned an invalid input_jsonl_sha256"
        )
    if input_jsonl_sha256 != expected_sha256:
        raise SeriesError(
            "shared SeriesRow projection checker input_jsonl_sha256 disagrees with rows"
        )
    raw_results = output["results"]
    if not isinstance(raw_results, list):
        raise SeriesError(
            "shared SeriesRow projection checker results must be an array"
        )
    parsed = _parse_projection_result_items(raw_results)
    returned_event_ids = set(parsed)
    missing = sorted(expected_event_ids.difference(returned_event_ids))
    unknown = sorted(returned_event_ids.difference(expected_event_ids))
    if missing:
        raise SeriesError(
            "shared SeriesRow projection checker omitted event_id(s): "
            + ", ".join(missing)
        )
    if unknown:
        raise SeriesError(
            "shared SeriesRow projection checker returned unknown event_id(s): "
            + ", ".join(unknown)
        )
    if len(parsed) != expected_row_count:
        raise SeriesError(
            "shared SeriesRow projection checker result count disagrees with rows"
        )
    return ProjectionResults(
        row_count=input_row_count,
        input_jsonl_sha256=input_jsonl_sha256,
        results=dict(sorted(parsed.items())),
    )


def _parse_projection_result_items(raw_results: list[Any]) -> dict[str, str | None]:
    """Parse one complete event-id-to-result mapping, refusing ambiguity."""
    parsed: dict[str, str | None] = {}
    for index, item in enumerate(raw_results):
        if not isinstance(item, dict) or set(item) != {"event_id", "result"}:
            raise SeriesError(
                f"shared SeriesRow projection checker result {index} has an invalid shape"
            )
        event_id = item["event_id"]
        if not isinstance(event_id, str) or not event_id.strip():
            raise SeriesError(
                f"shared SeriesRow projection checker result {index} has an invalid event_id"
            )
        if event_id in parsed:
            raise SeriesError(
                f"shared SeriesRow projection checker repeated event_id {event_id!r}"
            )
        projection_result = item["result"]
        if projection_result is not None and (
            not isinstance(projection_result, str) or not projection_result.strip()
        ):
            raise SeriesError(
                f"shared SeriesRow projection checker result for {event_id!r} "
                "must be null or a nonempty string"
            )
        parsed[event_id] = projection_result
    return parsed


def _checked_rows(rows: Iterable[Any], *, strict_paths: bool) -> list[dict]:
    """Copy input rows and enforce parent-owned path publication rules."""
    checked = []
    for row in rows:
        if not isinstance(row, dict):
            raise MalformedRow(f"row must be an object, got {type(row).__name__}")
        copied = dict(row)
        if strict_paths and _contains_owner_path(copied):
            raise MalformedRow("row contains a workspace-local owner path")
        checked.append(copied)
    return checked


def _canonical_row_json(row: dict) -> str:
    try:
        return json.dumps(row, sort_keys=True, separators=(",", ":"))
    except (TypeError, ValueError, RecursionError) as error:
        raise MalformedRow(f"row is not canonical JSON: {error}") from error


def _checker_input(rows: Iterable[dict]) -> str:
    return "".join(_canonical_row_json(row) + "\n" for row in rows)


def prepare_shared_checker() -> None:
    """Prepare the same read-only checker before taking publication authority."""
    result = _run_shared_checker("", ["--help"])
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise SeriesError(f"cannot prepare the shared SeriesRow checker: {detail}")


def _shared_checker_script(environment: Mapping[str, str]) -> Path:
    """Select the row checker of the tool that admitted this run.

    check-series-rows.rs compiles ``../../hermit/ci/manifest-plan`` relative to
    itself. Hermit's pressure test always runs the STATE root's series.py
    (``$DEV_HERMIT_PARENT/ci-hub/series/series.py``), so locating the checker
    from ``__file__`` compiled it against the shared primary Hermit checkout,
    which another agent may leave stale or mid-edit. On 2026-10-03 that
    checkout sat detached at a three-day-old commit beside a newer agent-utils
    submodule; the checker failed to compile (E0063, missing
    ``delegated_children``) and every pressure-test cell batch was refused.

    An explicit tool root names immutable code materialized at one parent
    commit with its pinned submodules, the same contract validate.rs and
    ci-hub.rs already apply to ``DEV_HERMIT_TOOL_ROOT``. A set but unusable
    value is refused rather than silently replaced by the mutable checkout.
    Unset or empty keeps the previous meaning: the checker beside this file.
    """
    configured = environment.get(TOOL_ROOT_ENV, "")
    if not configured:
        return Path(__file__).resolve().with_name(SHARED_CHECKER_RELATIVE.name)
    if not os.path.isabs(configured) or not os.path.isdir(configured):
        raise SeriesError(
            f"{TOOL_ROOT_ENV}={configured!r} is not an absolute directory; "
            "refusing to fall back to the shared checkout's SeriesRow checker"
        )
    checker = None
    try:
        # The same resolution __file__ receives on the validate path, so a
        # /proc/<pid>/fd/<n> handle builds under its stable materialized path.
        checker = Path(configured).resolve(strict=True) / SHARED_CHECKER_RELATIVE
        regular = stat.S_ISREG(checker.stat().st_mode)
    except (OSError, RuntimeError) as error:
        regular = False
        reason = str(error)
    else:
        reason = "not a regular file"
    if not regular:
        raise SeriesError(
            f"{TOOL_ROOT_ENV}={configured!r} has no usable SeriesRow checker at "
            f"{checker or SHARED_CHECKER_RELATIVE} ({reason}); refusing to fall back "
            "to the shared checkout's checker"
        )
    return checker


def _checker_build_failed(result: subprocess.CompletedProcess[str]) -> bool:
    """True when rust-script could not build the checker, so no row was judged.

    The checker's own refusals print ``check-series-rows: ...``; only a cargo
    failure ends with rust-script's fixed line and produces no stdout.
    """
    if result.returncode == 0 or (result.stdout or "").strip():
        return False
    lines = [line.strip() for line in (result.stderr or "").splitlines() if line.strip()]
    return bool(lines) and lines[-1] == RUST_SCRIPT_BUILD_FAILURE


#: Per-probe bound for the build-failure diagnostic; it runs at most five probes.
_DIAGNOSTIC_GIT_TIMEOUT_SECONDS = 10


def _git_output(checkout: Path, args: Sequence[str]) -> str | None:
    """Stdout of a successful, bounded git probe, or None; never raises."""
    try:
        result = _git(checkout, args, timeout=_DIAGNOSTIC_GIT_TIMEOUT_SECONDS)
    except (OSError, subprocess.SubprocessError, UnicodeError, ValueError):
        return None
    return result.stdout if result.returncode == 0 else None


def _checker_build_failure(
    checker: Path, tool_hermit: Path, stderr: str, *, from_tool_root: bool,
) -> str:
    """One diagnostic line naming the Hermit tree the checker failed against.

    Best effort by construction: every probe that fails reports ``unknown``
    rather than masking the build failure behind a second exception.
    """
    head = None
    probe = _git_output(tool_hermit, ["rev-parse", "--show-toplevel", "HEAD"])
    parts = (probe or "").splitlines()
    if len(parts) == 2:
        try:
            # git -C walks upward; a directory that is not itself the checkout
            # root would otherwise report its parent's commit as Hermit's.
            if Path(parts[0]).resolve() == tool_hermit.resolve():
                head = parts[1]
        except OSError:
            head = None
    pin = (_git_output(tool_hermit.parent, ["rev-parse", "HEAD:hermit"]) or "").strip()
    if head is None:
        head_note = "HEAD unknown (not a git checkout root)"
    elif pin:
        head_note = f"HEAD {head} (dev-hermit pins {pin}: {'matches' if head == pin else 'DIFFERS'})"
    else:
        head_note = f"HEAD {head} (dev-hermit pin unknown)"
    # Compare each submodule's checked-out commit with Hermit's COMMITTED
    # gitlink. `git submodule status` alone compares with the index, so a
    # moved submodule whose gitlink was also staged would read as clean.
    checked_out: dict[str, str] = {}
    drifted = []
    submodules = _git_output(tool_hermit, ["submodule", "status"]) if head else None
    for line in (submodules or "").splitlines():
        fields = line[1:].split()
        if len(fields) < 2:
            continue
        sha, name = fields[0], fields[1]
        if line.startswith("-"):
            drifted.append(f"{name} not initialized")
        elif line.startswith("U"):
            drifted.append(f"{name} has merge conflicts")
        else:
            checked_out[name] = sha
    pins: dict[str, str] | None = {}
    if checked_out:
        listing = _git_output(tool_hermit, ["ls-tree", "-z", "HEAD", "--", *checked_out])
        if listing is None:
            pins = None
        else:
            for entry in listing.split("\0"):
                meta, _, path = entry.partition("\t")
                parts = meta.split()
                if len(parts) == 3 and parts[1] == "commit":
                    pins[path] = parts[2]
    for name, sha in checked_out.items():
        pinned = None if pins is None else pins.get(name)
        if pinned != sha:
            drifted.append(f"{name} at {sha[:12]} (pin {pinned[:12] if pinned else 'unknown'})")
    if head is None:
        submodule_note = "unknown"
    elif submodules is None:
        submodule_note = "unknown (git submodule status failed)"
    else:
        submodule_note = "; ".join(drifted) or "none"
    dirty = _git_output(
        tool_hermit,
        ["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"],
    ) if head else None
    dirty_count = "unknown" if dirty is None else str(len(dirty.splitlines()))
    lines = stderr.splitlines()
    first_error = "none reported"
    for index, line in enumerate(lines):
        stripped = line.strip()
        if not stripped.startswith("error") or stripped == RUST_SCRIPT_BUILD_FAILURE:
            continue
        if stripped.startswith("error: could not compile"):
            continue
        first_error = stripped
        if index + 1 < len(lines) and lines[index + 1].strip().startswith("-->"):
            first_error += " " + lines[index + 1].strip()
        break
    tail = stderr.strip()[-4000:]
    if from_tool_root:
        remedy = (
            f"the immutable tool root named by {TOOL_ROOT_ENV} does not build its "
            "own checker; fix that dev-hermit commit, not the shared checkout"
        )
    else:
        remedy = (
            f"run under an immutable tool root ({TOOL_ROOT_ENV}) or restore "
            f"{tool_hermit} and its submodules to their pinned commits"
        )
    return (
        f"shared SeriesRow checker did not build against tool Hermit {tool_hermit} "
        f"at {head_note}; submodules off Hermit's pin: "
        f"{submodule_note}; {dirty_count} modified tracked path(s); "
        f"first error: {first_error}; checker {checker}. Remedy: {remedy}\n{tail}"
    )


def _run_shared_checker(
    checker_input: str, arguments: Sequence[str],
) -> subprocess.CompletedProcess[str]:
    """Invoke Hermit's shared row checker without accepting partial output."""
    environment = dict(os.environ)
    # This compiler/checker reads stdin only; it never needs ledger authority.
    environment.pop(LEDGER_ROOT_ENV, None)
    environment.pop("DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD", None)
    checker = _shared_checker_script(environment)
    # The checker compiles the current tool's Hermit dependency, which may use
    # a different channel from an inherited historical measurement environment.
    tool_hermit = checker.parents[2] / "hermit"
    try:
        selected = rust_toolchain.selected_toolchain(tool_hermit, environment=environment)
        environment["RUSTUP_TOOLCHAIN"] = selected
        environment["RUSTUP_AUTO_INSTALL"] = "0"
        result = subprocess.run(
            ["rust-script", "--force", "--toolchain", selected, str(checker), *arguments],
            input=checker_input,
            env=environment,
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
    except (rust_toolchain.ToolchainRefused, OSError, subprocess.TimeoutExpired, UnicodeError) as error:
        raise SeriesError(f"cannot run the shared SeriesRow checker: {error}") from error
    if _checker_build_failed(result):
        # A tool failure, not a verdict on any row: never report it as a
        # MalformedRow, and say which Hermit tree failed in the first line.
        raise SeriesError(_checker_build_failure(
            checker, tool_hermit, result.stderr,
            from_tool_root=bool(environment.get(TOOL_ROOT_ENV)),
        ))
    return result


def _contains_owner_path(value: Any) -> bool:
    if isinstance(value, str):
        return _OWNER_PATH_NEEDLE in value
    if isinstance(value, dict):
        return any(_contains_owner_path(v) for v in value.values())
    if isinstance(value, (list, tuple)):
        return any(_contains_owner_path(v) for v in value)
    return False


def union(paths: Sequence[Any]) -> list[dict]:
    """Deterministic union across spool batches and tracked shards.

    Delegates to the ledger's `union_events`: same ordering rule, same event-id
    dedup, no series-specific branch.
    """
    events: list[dict] = []
    for path in paths:
        events.extend(read_shard(path))
    return union_events(validate_read_rows(events))


def series_for(rows: Iterable[dict], cell: str, tree: str) -> list[dict]:
    """Every repetition of one cell against one tree, in run order.

    This is the READER for steps 4, 5 and 8: a stored row may count exactly equal
    observations, and {earliest,latest} is a DERIVED view over this list rather
    than a stored field.
    """
    out = [
        r for r in rows
        if (r.get("series") or {}).get("cell") == cell
        and (r.get("series") or {}).get("tree") == tree
    ]
    out.sort(key=lambda r: (
        int((r.get("series") or {}).get("run_index", -1)),
        int((r.get("series") or {}).get("attempt", 1)),
    ))
    return out


def observed_range(rows: Iterable[dict], coordinate: str) -> dict | None:
    """`{earliest, latest, samples}` for ONE coordinate, DERIVED not stored.

    Returns None when no row located that coordinate, which is how
    'diverged-but-unlocated' stays distinguishable from 'never-measured' at the
    consumer: the caller knows whether it had rows at all.

    ⚠️ ONE COORDINATE AT A TIME, BY CONSTRUCTION. Taking a min across axes would
    mix record numbers with syscall counts; the signature does not allow it.
    """
    if coordinate not in COORDINATE_KEYS:
        raise SeriesError(f"unknown coordinate {coordinate!r}")
    minimum = None
    maximum = None
    samples = 0
    for row in rows:
        payload = row.get("series") or {}
        if payload.get("outcome") != "diverged":
            continue
        value = (payload.get("coordinates") or {}).get(coordinate)
        if isinstance(value, int) and not isinstance(value, bool):
            minimum = value if minimum is None else min(minimum, value)
            maximum = value if maximum is None else max(maximum, value)
            count = payload.get("num_runs", 1)
            samples += (
                count
                if isinstance(count, int) and not isinstance(count, bool) and count > 0
                else 1
            )
    if minimum is None or maximum is None:
        return None
    return {"earliest": minimum, "latest": maximum, "samples": samples}


def measurement_state(rows: Sequence[dict]) -> str:
    """never-measured | measured-and-passed | diverged-unlocated | diverged.

    Step 6 needs these four readable from the row without consulting another
    file; this is the function that defines them so every consumer agrees.
    """
    if not rows:
        return "never-measured"
    outcomes = {(r.get("series") or {}).get("outcome") for r in rows}
    if "diverged" not in outcomes:
        return "measured-and-passed" if outcomes <= {"passed"} else "measured-no-verdict"
    for coordinate in COORDINATE_KEYS:
        if observed_range(rows, coordinate) is not None:
            return "diverged"
    return "diverged-unlocated"


def _pressure_outer_kind(payload: dict) -> str:
    outcome = payload.get("outcome")
    result = payload.get("result")
    failure_class = payload.get("failure_class")
    if outcome == "passed" and result == "pass" and failure_class is None:
        return "pass"
    if failure_class == "product_failure" and _is_str_in(outcome, {"diverged", "errored"}):
        return "product"
    return {
        "understood_infrastructure_failure": "infrastructure",
        "understood_prerequisite_failure": "prerequisite",
        "no_result": "no_result",
    }.get(failure_class, "unknown")


def _pressure_inner_kinds(payload: dict) -> list[str] | None:
    """Interpret retained, shared-type-validated facts for qualification only."""
    evidence = payload.get("pressure_evidence")
    if evidence is None:
        return None
    mode = payload["cell"].rsplit("/", 2)[-2]
    kinds = []
    for attempt in evidence["attempts"]:
        comparison = attempt.get("comparison")
        non_product = {
            "guest-launch-refused": "prerequisite",
            "backend-unavailable": "prerequisite",
            "infrastructure": "infrastructure",
            "result-publication": "infrastructure",
            "incomplete-verification-evidence": "no_result",
            "invalid-backend-evidence": "no_result",
        }.get(attempt.get("error_kind"))
        if non_product is not None:
            kind = non_product
        elif comparison is not None:
            verdict = comparison["verdict"]
            if comparison["canonical"] and verdict == "matched":
                kind = "pass"
            elif comparison["canonical"] and verdict == "diverged":
                kind = "product"
            elif verdict == "infrastructure_error":
                kind = "infrastructure"
            elif verdict == "no_result" and _is_str_in(comparison.get("no_result_kind"), {
                "first_run_rejected", "container_failed", "run_failed",
            }):
                kind = "product"
            else:
                kind = "no_result"
        elif mode in _COMPARISON_MODES or attempt["timed_out"]:
            kind = "no_result"
        elif attempt["outcome"] == "PASS":
            # Custom modes can deliberately expect a nonzero exit or signal.
            # The shared validator checks their completed process disposition.
            kind = "pass"
        elif attempt["outcome"] == "FAIL":
            if attempt.get("status") is None and attempt.get("signal") is None:
                return None
            kind = "product"
        else:
            kind = "no_result"
        kinds.append(kind)
    return kinds


def classify_pressure_sample(
    rows: Iterable[dict], selected_cells: Sequence[str], campaign_id: str
) -> dict[str, dict[str, Any]]:
    """Classify one fixed ten-repetition pressure campaign by exact cell.

    Rows are the accepted series projection, not website state.  The latter
    intentionally collapses every non-product failure to an unknown result and
    therefore cannot distinguish infrastructure, prerequisites, and no-result.
    Exact duplicate events are harmless publication replay; two distinct events
    claiming the same repetition/attempt make that cell incomplete.
    """
    selected = list(selected_cells)
    if not isinstance(campaign_id, str) or not campaign_id or any(
        not isinstance(cell, str) or not cell for cell in selected
    ):
        raise SeriesError("pressure classification requires a named campaign and unique cells")
    if len(selected) != len(set(selected)):
        raise SeriesError("pressure classification requires a named campaign and unique cells")

    by_cell: dict[str, dict[int, dict[int, dict]]] = {
        cell: {} for cell in selected
    }
    duplicate_attempts = {cell: 0 for cell in selected}
    seen_events: dict[str, dict] = {}
    trees = set()
    campaign_rows = [row for row in rows if row.get("run_id") == campaign_id]
    for row in validate_read_rows(campaign_rows):
        event_id = row.get("event_id")
        if isinstance(event_id, str) and event_id in seen_events:
            if seen_events[event_id] != row:
                raise SeriesError(f"pressure event_id {event_id!r} changed content")
            continue
        if isinstance(event_id, str):
            seen_events[event_id] = row
        producer = row.get("producer")
        producer_name = producer.get("tool") if isinstance(producer, dict) else producer
        if producer_name != "pressure-test":
            raise SeriesError(
                f"campaign {campaign_id!r} contains non-pressure producer {producer_name!r}"
            )
        payload = row.get("series")
        if not isinstance(payload, dict):
            raise SeriesError("pressure campaign row has no series payload")
        cell = payload.get("cell")
        if cell not in by_cell:
            raise SeriesError(
                f"campaign {campaign_id!r} contains unselected cell {cell!r}"
            )
        tree = payload.get("tree")
        if not isinstance(tree, str) or not tree:
            raise SeriesError("pressure campaign row has no source tree")
        trees.add(tree)
        first = payload.get("run_index")
        count = payload.get("num_runs", 1)
        attempt = payload.get("attempt", 1)
        if (
            not isinstance(first, int)
            or isinstance(first, bool)
            or not isinstance(count, int)
            or isinstance(count, bool)
            or count < 1
            or not isinstance(attempt, int)
            or isinstance(attempt, bool)
            or attempt < 1
            or (count > 1 and "attempt" in payload)
        ):
            raise SeriesError("pressure campaign row has invalid repetition/attempt span")
        last = payload.get("last_run_index", first + count - 1)
        if last != first + count - 1:
            raise SeriesError("pressure campaign row has an inconsistent repetition span")
        for repetition in range(first, last + 1):
            attempts = by_cell[cell].setdefault(repetition, {})
            if attempt in attempts:
                duplicate_attempts[cell] += 1
            else:
                attempts[attempt] = payload
    if len(trees) > 1:
        raise SeriesError(f"campaign {campaign_id!r} spans multiple source trees")

    expected = set(range(1, PRESSURE_PROMOTION_REPETITIONS + 1))
    classifications: dict[str, dict[str, Any]] = {}
    for cell in selected:
        repetitions = by_cell[cell]
        counts = {
            "qualifying_passes": 0,
            "terminal_passes": 0,
            "terminal_product_failures": 0,
            "infrastructure_failures": 0,
            "prerequisite_failures": 0,
            "no_results": 0,
            "mixed_repetitions": 0,
            "retried_repetitions": 0,
            "unknown_history_repetitions": 0,
        }
        malformed = duplicate_attempts[cell] > 0 or set(repetitions) - expected != set()
        for repetition in sorted(set(repetitions) & expected):
            attempts = repetitions[repetition]
            ordinals = sorted(attempts)
            if len(ordinals) > 1:
                counts["retried_repetitions"] += 1
            if (
                ordinals != list(range(1, len(ordinals) + 1))
                or len(ordinals) > 2
                or any(
                    attempts[ordinal]["outcome"] == "passed"
                    for ordinal in ordinals[:-1]
                )
            ):
                # The framework caps outer attempts at two and stops on PASS.
                # This history cannot supply a valid terminal observation.
                malformed = True
                counts["mixed_repetitions"] += 1
                continue
            kinds = []
            inner_kinds = []
            history_known = True
            for ordinal in ordinals:
                payload = attempts[ordinal]
                kinds.append(_pressure_outer_kind(payload))
                retained = _pressure_inner_kinds(payload)
                if not retained or payload.get("source_tree_dirty", False):
                    history_known = False
                else:
                    inner_kinds.extend(retained)
            if not history_known:
                counts["unknown_history_repetitions"] += 1
            adverse_kinds = set(kinds) | (set(inner_kinds) - {"pass"})
            # Valid retries select PASS over FAIL over ERROR. Retain every
            # adverse attempt when deciding whether the history is homogeneous.
            if "pass" in kinds:
                counts["terminal_passes"] += 1
                if (
                    history_known
                    and ordinals == [1]
                    and kinds == ["pass"]
                    and set(inner_kinds) == {"pass"}
                ):
                    counts["qualifying_passes"] += 1
            elif history_known and adverse_kinds == {"product"}:
                counts["terminal_product_failures"] += 1
            elif history_known and adverse_kinds == {"infrastructure"}:
                counts["infrastructure_failures"] += 1
            elif (
                history_known
                and adverse_kinds == {"prerequisite"}
            ):
                counts["prerequisite_failures"] += 1
            elif history_known and adverse_kinds == {"no_result"}:
                counts["no_results"] += 1
            else:
                counts["mixed_repetitions"] += 1

        observed = len(set(repetitions) & expected)
        missing = PRESSURE_PROMOTION_REPETITIONS - observed
        complete = not malformed and missing == 0 and counts["unknown_history_repetitions"] == 0
        if complete and counts["qualifying_passes"] == PRESSURE_PROMOTION_REPETITIONS:
            classification = "promotion-candidate"
        elif complete and counts["terminal_passes"] > 0:
            classification = "intermittent"
        elif complete and counts["terminal_product_failures"] == PRESSURE_PROMOTION_REPETITIONS:
            classification = "confirmed-failing"
        elif complete and counts["infrastructure_failures"] == PRESSURE_PROMOTION_REPETITIONS:
            classification = "infrastructure-failure"
        elif complete and counts["prerequisite_failures"] == PRESSURE_PROMOTION_REPETITIONS:
            classification = "prerequisite-failure"
        elif complete and counts["no_results"] == PRESSURE_PROMOTION_REPETITIONS:
            classification = "no-result"
        else:
            classification = "incomplete"
        classifications[cell] = {
            "classification": classification,
            "promotion_candidate": classification == "promotion-candidate",
            "expected_repetitions": PRESSURE_PROMOTION_REPETITIONS,
            "observed_repetitions": observed,
            "missing_repetitions": missing,
            "duplicate_attempts": duplicate_attempts[cell],
            **counts,
        }
    return classifications


def append_rows(path: Any, rows: Iterable[dict]) -> int:
    """Validate then append. Refuses the batch before writing any of it."""
    checked = validate_rows(rows)
    return append_events(path, checked)


def append_cell_rows(
    parent: Any, path: Any, rows: Iterable[dict], *,
    conflicts: list[tuple[int, dict, str]] | None = None,
    published: PublishedSnapshot | None = None,
) -> int:
    """Append projected cell rows once, refusing a changed repeated identity.

    With ``conflicts``, a row whose event_id repeats within the batch or
    already records a different body is set aside there as
    ``(position in rows, row, reason)`` and the remaining rows are still
    appended. Without it, such a row refuses the whole batch.

    ``published`` is the ledger snapshot to check against, for a caller that
    appends several batches and has already read it. By default only the
    published rows of this batch's event ids are read, at the local
    ``refs/remotes/origin/main`` (``published_rows_for_at_repo``): a vouch
    record lets that read open just the month files those rows belong in.
    Reading every published row as Python objects instead took 139 s and
    17 GB on 2026-10-14 (1,946,497 rows), on the verdict path of every
    Hermit validation, to check 2,241 new ids; the lookup took 1.1 s. That
    lookup refuses for every reason the whole read would. It can miss a
    published row filed outside the month files its event names, never
    return a different one; such a row is then appended again and the
    publisher counts it already published, or refuses it if its body
    differs. A local spool row that conflicts with a published row is
    likewise refused by the publisher, no longer here, unless it shares an
    id with this batch.
    """
    path = Path(path)
    checked = validate_rows(rows)
    # Read the locally available immutable ledger ref before taking the spool
    # lock. Publishing/fetching is separate from retaining a producer's result.
    if published is None:
        wanted: dict[str, set[tuple[str, str, str]]] = {}
        for row in checked:
            wanted.setdefault(str(row.get("event_id", "")), set()).add((
                str(row.get("team", "")), str(row.get("host", "")),
                str(row.get("emitted_at", ""))[:7]))
        published_rows = list(published_rows_for_at_repo(
            published_repository(parent, LEDGER_REPOSITORY), "refs/remotes/origin/main",
            wanted, repository=LEDGER_REPOSITORY,
        ).values())
    else:
        published.verify()
        published_rows = published.rows
    path.parent.mkdir(parents=True, exist_ok=True)
    with series_spool_transition_lock(path.parent, exclusive=True):
        # Check both the live spool and published shards. Once a batch has been
        # published and its spool removed, re-emitting it must still be a no-op.
        local = sorted(path.parent.glob("*.jsonl")) + sorted(
            (path.parent / "published").glob("*.jsonl")
        )
        existing = union_events([*published_rows, *union(local)])
        known = {str(row.get("event_id", "")): row for row in existing}
        pending = []
        batch_ids = set()
        for position, row in enumerate(checked):
            event_id = str(row.get("event_id", ""))
            if event_id in batch_ids:
                reason = (
                    f"cell batch repeats event_id {event_id!r}; run_index must identify one run"
                )
                if conflicts is None:
                    raise SeriesError(reason)
                conflicts.append((position, row, reason))
                continue
            batch_ids.add(event_id)
            prior = known.get(event_id)
            if prior is None:
                known[event_id] = row
                pending.append(row)
                continue
            # The observation time belongs to the original append. A later
            # command that reads the same retained run gets today's time by
            # default; restore the recorded time before comparing so an exact
            # replay is a no-op. WRITE_TIME_SERIES_FIELDS are observations of
            # the same kind and are restored the same way.
            replay = json.loads(json.dumps(row))
            replay["emitted_at"] = prior.get("emitted_at")
            prior_series = prior.get("series")
            replay_series = replay.get("series")
            if isinstance(prior_series, dict) and isinstance(replay_series, dict):
                for field in WRITE_TIME_SERIES_FIELDS:
                    if field in prior_series:
                        replay_series[field] = prior_series[field]
                    else:
                        replay_series.pop(field, None)
            if json.dumps(replay, sort_keys=True, separators=(",", ":")) != json.dumps(
                prior, sort_keys=True, separators=(",", ":")
            ):
                reason = (
                    f"event_id {event_id!r} already records a different row; refusing rewrite"
                )
                if conflicts is None:
                    raise SeriesError(reason)
                conflicts.append((position, row, reason))
        return append_events(path, pending) if pending else 0


def _quarantine_name(run_id: str) -> str:
    """One quarantine file per run, safe even for a run id the writer refused."""
    if run_id and "/" not in run_id and run_id not in (".", "..") and "\0" not in run_id:
        return f"{run_id}.jsonl"
    return "invalid-run-id-" + hashlib.sha256(run_id.encode()).hexdigest() + ".jsonl"


def _quarantine_cell(raw: Any) -> str | None:
    """Best-effort cell name for a record whose row may be malformed."""
    if not isinstance(raw, dict):
        return None
    payload = raw.get("series")
    if isinstance(payload, dict) and isinstance(payload.get("cell"), str):
        return payload["cell"]
    parts = [raw.get(name) for name in ("test", "mode", "backend")]
    if all(isinstance(part, str) and part for part in parts):
        return "/".join(parts)
    return None


class MalformedQuarantine(SeriesError):
    """A quarantine record that cannot be retained or published as evidence."""


_QUARANTINE_KEYS = frozenset({
    "schema", "event_id", "event_type", "team", "host", "emitted_at", "run_id",
    "producer", "tree", "stage", "error", "reason", "cell", "row_event_id",
    "inputs", "row", "owner_paths", "withheld", "legacy",
})
_QUARANTINE_INPUT_KEYS = frozenset({"input_row", "raw", "raw_base64", "raw_sha256", "raw_bytes"})
_QUARANTINE_REF_INPUT_KEYS = _QUARANTINE_INPUT_KEYS | {"raw_ref"}
_QUARANTINE_REF_RE = re.compile(re.escape(QUARANTINE_INPUTS_ROOT) + r"/([0-9a-f]{2})/([0-9a-f]{64})")
_QUARANTINE_LEGACY_KEYS = frozenset({
    "schema", "quarantine_id", "input_row", "raw", "raw_sha256", "raw_bytes",
})
_QUARANTINE_ID_RE = re.compile(r"series-quarantine-[0-9a-f]{64}")
#: The hour is required because it names the record's published shard
#: (quarantine_bucket).
_QUARANTINE_EMITTED_AT_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}\S*")
#: A stand-in for one owner-home prefix in a published record. The ledger
#: publishes only the prefix's byte length, so the bytes come back only for a
#: reader who already knows the prefix (see portable_quarantine_record).
_OWNER_HOME_TOKEN_RE = re.compile(r"\{\{OWNER_HOME_[0-9]+\}\}")
#: One owner-home prefix: the needle plus the account segment that follows it.
#: The segment stops at any character that JSON escapes or that delimits a path,
#: so the same substring appears identically in the encoded and decoded record.
_OWNER_HOME_PREFIX_RE = re.compile(
    re.escape(_OWNER_PATH_NEEDLE) + r"[^/\\\"'\s=,:;()\[\]{}<>]+"
)
_QUARANTINE_REDACTED = "[withheld: contained an owner path]"


def _writer_host() -> str:
    """This machine's short name: the shard a refused input is published under."""
    host = socket.gethostname().split(".")[0]
    if not host or "/" in host or host in (".", "..") or "\0" in host:
        raise MalformedQuarantine(f"cannot derive a host path segment from {socket.gethostname()!r}")
    return host


def quarantine_input(input_row: int | None, raw: bytes) -> dict:
    """Describe one received input by its exact bytes.

    ``raw`` holds the bytes as UTF-8 text when they decode strictly (so they
    re-encode to the same bytes), otherwise ``raw_base64`` holds them. The
    digest and length are always over the exact received bytes.
    """
    if not isinstance(raw, (bytes, bytearray)):
        raise MalformedQuarantine(
            f"a quarantine input must be the received bytes, got {type(raw).__name__}"
        )
    raw = bytes(raw)
    try:
        text: str | None = raw.decode("utf-8")
    except UnicodeDecodeError:
        text = None
    return {
        "input_row": input_row,
        "raw": text,
        "raw_base64": None if text is not None else base64.b64encode(raw).decode("ascii"),
        "raw_sha256": hashlib.sha256(raw).hexdigest(),
        "raw_bytes": len(raw),
    }


def _quarantine_event_id(record: Mapping[str, Any]) -> str:
    """Identity of one refusal: what was refused, where, and at which stage.

    It binds the exact input digests, not the observation time, so replaying
    one run's inputs is a no-op rather than a second record.
    """
    legacy = record.get("legacy")
    if legacy is not None:
        identity: dict[str, Any] = {
            "event_id": record.get("row_event_id"),
            "input_row": legacy.get("input_row"),
            "producer": record.get("producer"),
            "raw_sha256": legacy.get("raw_sha256"),
            "run_id": record.get("run_id"),
            "stage": record.get("stage"),
            "tree": record.get("tree"),
        }
    else:
        identity = {
            "inputs": [[item.get("input_row"), item.get("raw_sha256")]
                       for item in record.get("inputs") or []],
            "producer": record.get("producer"),
            "row_event_id": record.get("row_event_id"),
            "run_id": record.get("run_id"),
            "stage": record.get("stage"),
            "tree": record.get("tree"),
        }
    encoded = json.dumps(identity, sort_keys=True, separators=(",", ":"))
    return "series-quarantine-" + hashlib.sha256(encoded.encode()).hexdigest()


def quarantine_record(
    *, stage: str, run_id: str, producer: str, tree: str, emitted_at: str,
    error: str, reason: str, inputs: Sequence[dict], row: dict | None = None,
    row_event_id: str | None = None, cell: str | None = None, host: str | None = None,
) -> dict:
    """Describe one refusal with the exact inputs it refused.

    ``inputs`` come from :func:`quarantine_input`, so each carries the input
    exactly as received, byte for byte. ``row`` is the projected series row
    for a refusal made after projection (shared checker or spool conflict);
    it is derived, never a substitute for the inputs. The local record keeps
    every byte, owner paths included; :func:`portable_quarantine_record` makes
    the published form.
    """
    record = {
        "schema": QUARANTINE_SCHEMA,
        "event_id": "",
        "event_type": QUARANTINE_EVENT_TYPE,
        "team": "hermit",
        "host": host if host is not None else _writer_host(),
        "emitted_at": emitted_at,
        "run_id": run_id,
        "producer": producer,
        "tree": tree,
        "stage": stage,
        "error": error,
        "reason": reason,
        "cell": cell,
        "row_event_id": row_event_id,
        "inputs": [dict(item) for item in inputs],
        "row": copy.deepcopy(row) if row is not None else None,
        "owner_paths": [],
        "withheld": None,
        "legacy": None,
    }
    record["event_id"] = _quarantine_event_id(record)
    validate_quarantine_records([record], public=False)
    return record


def upgrade_legacy_quarantine(record: Mapping[str, Any], *, host: str) -> dict:
    """Carry a ``series-quarantine/v1`` record into the published v2 type.

    v1 stored the parsed object and a digest of its canonical re-serialization,
    never the received bytes, so the upgraded record has no ``inputs``. The v1
    content is kept verbatim under ``legacy`` and labelled as such; it is not
    presented as the received bytes. Its identity is the v1 ``quarantine_id``.
    """
    if not isinstance(record, Mapping) or record.get("schema") != LEGACY_QUARANTINE_SCHEMA:
        raise MalformedQuarantine("not a series-quarantine/v1 record")
    recorded_host = record.get("host")
    if not (isinstance(recorded_host, str) and recorded_host and "/" not in recorded_host
            and recorded_host not in (".", "..")):
        recorded_host = host
    upgraded = {
        "schema": QUARANTINE_SCHEMA,
        "event_id": record.get("quarantine_id"),
        "event_type": QUARANTINE_EVENT_TYPE,
        "team": "hermit",
        "host": recorded_host,
        "emitted_at": record.get("emitted_at"),
        "run_id": record.get("run_id"),
        "producer": record.get("producer"),
        "tree": record.get("tree"),
        "stage": record.get("stage"),
        "error": "legacy-v1-unrecorded",
        "reason": record.get("reason"),
        "cell": record.get("cell"),
        "row_event_id": record.get("event_id"),
        "inputs": [],
        "row": None,
        "owner_paths": [],
        "withheld": None,
        "legacy": {
            "schema": LEGACY_QUARANTINE_SCHEMA,
            "quarantine_id": record.get("quarantine_id"),
            "input_row": record.get("input_row"),
            "raw": copy.deepcopy(record.get("raw")),
            "raw_sha256": record.get("raw_sha256"),
            "raw_bytes": record.get("raw_bytes"),
        },
    }
    validate_quarantine_records([upgraded], public=False)
    return upgraded


def _quarantine_text(record: Mapping[str, Any]) -> str:
    return json.dumps(record, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def quarantine_exposes_owner_path(record: Mapping[str, Any]) -> bool:
    """True when any published form of ``record`` would carry an owner path.

    Stricter than the series row guard: it also reads keys, the decoded
    content of each JSON input line, and base64-held bytes.
    """
    try:
        text = _quarantine_text(record)
    except (TypeError, ValueError, RecursionError):
        return True
    if _OWNER_PATH_NEEDLE in text:
        return True
    needle = _OWNER_PATH_NEEDLE.encode()
    for item in record.get("inputs") or []:
        if not isinstance(item, Mapping):
            continue
        raw = item.get("raw")
        if isinstance(raw, str):
            try:
                decoded = json.dumps(json.loads(raw), ensure_ascii=False)
            except (ValueError, RecursionError):
                decoded = ""
            if _OWNER_PATH_NEEDLE in decoded:
                return True
        encoded = item.get("raw_base64")
        if isinstance(encoded, str):
            try:
                content = base64.b64decode(encoded, validate=True)
            except ValueError:
                return True
            if needle in content:
                return True
    return False


def _withheld_quarantine_stub(record: Mapping[str, Any], detail: str) -> dict:
    """A content-free public stand-in for a record token substitution cannot publish.

    It keeps each input's digest and length and none of its bytes. The ledger
    cannot restore the record from it: the only exact copy is the host-local
    one that the publisher moves into ``held/`` (:func:`quarantine_held_dir`),
    where the health gate alarms until someone deals with it.
    """
    stub = copy.deepcopy(dict(record))
    stub["inputs"] = [
        {
            "input_row": item.get("input_row"),
            "raw": None,
            "raw_base64": None,
            "raw_sha256": item.get("raw_sha256"),
            "raw_bytes": item.get("raw_bytes"),
        }
        for item in record.get("inputs") or []
    ]
    stub["row"] = None
    stub["owner_paths"] = []
    stub["withheld"] = {"reason": "owner-path", "detail": detail}
    if stub.get("legacy") is not None:
        stub["legacy"]["raw"] = None
    for name in ("reason", "cell", "tree", "run_id", "error", "row_event_id", "host"):
        value = stub.get(name)
        if isinstance(value, str) and _OWNER_PATH_NEEDLE in value:
            stub[name] = _QUARANTINE_REDACTED
    if quarantine_exposes_owner_path(stub):
        raise MalformedQuarantine(
            f"quarantine record {record.get('event_id')!r} still exposes an owner path "
            "after withholding its content"
        )
    return stub


def portable_quarantine_record(record: Mapping[str, Any]) -> dict:
    """The deterministic form of ``record`` published to the public test ledger.

    Each distinct owner-home prefix (the needle plus its account segment) is
    replaced by ``{{OWNER_HOME_<n>}}`` and listed in ``owner_paths`` with its
    byte length only. The published record is therefore NOT the received
    bytes: it is exact only after each token is replaced by a prefix that the
    ledger does not contain, and ``raw_sha256``/``raw_bytes`` prove a
    substitution a reader who knows the prefixes makes. The substitution is
    checked to round-trip here, before publication. A record that token
    substitution cannot make public (a token already in the content, a path
    the prefix pattern does not cover, or base64-held bytes) is published as
    a stub with ``withheld`` set, each input's digest and length, and no
    content. Its bytes are then kept only in the host-local ``held/``
    directory (:func:`quarantine_held_dir`). Idempotent.
    """
    text = _quarantine_text(record)
    prefixes: list[str] = []
    for match in _OWNER_HOME_PREFIX_RE.finditer(text):
        if match.group(0) not in prefixes:
            prefixes.append(match.group(0))
    if not prefixes:
        portable = json.loads(text)
    elif _OWNER_HOME_TOKEN_RE.search(text) is not None:
        return _withheld_quarantine_stub(
            record, "the content already contains an owner-home token, so a token "
            "substitution could not be told apart from the content",
        )
    else:
        tokens = {prefix: "{{OWNER_HOME_%d}}" % ordinal
                  for ordinal, prefix in enumerate(prefixes, 1)}
        replaced = _OWNER_HOME_PREFIX_RE.sub(lambda match: tokens[match.group(0)], text)
        restored = replaced
        for prefix, token in tokens.items():
            restored = restored.replace(token, prefix)
        if restored != text:
            return _withheld_quarantine_stub(
                record, "replacing owner paths did not round-trip exactly",
            )
        portable = json.loads(replaced)
        portable["owner_paths"] = [
            *(record.get("owner_paths") or []),
            *({"token": token, "bytes": len(prefix.encode())}
              for prefix, token in tokens.items()),
        ]
    if quarantine_exposes_owner_path(portable):
        return _withheld_quarantine_stub(
            record, "an owner path remains that the owner-home pattern does not "
            "replace",
        )
    return portable


def quarantine_input_path(digest: str) -> str:
    """The test-ledger file that holds the published input bytes with SHA-256 ``digest``.

    ``quarantine/inputs/<first two hex digits>/<digest>``: one file per
    distinct content, holding the bytes themselves, not JSON or base64. The
    digest is of the published bytes, which are the received bytes unless an
    owner-home token stands in them (:func:`portable_quarantine_record`); the
    row's ``raw_sha256`` is always of the received bytes.
    """
    if not isinstance(digest, str) or _SHA256_RE.fullmatch(digest) is None:
        raise MalformedQuarantine(f"a quarantine input digest must be 64-hex, got {digest!r}")
    return f"{QUARANTINE_INPUTS_ROOT}/{digest[:2]}/{digest}"


def _published_input_bytes(item: Any) -> bytes | None:
    """The bytes one published v2 input holds, or None when it holds none."""
    if not isinstance(item, Mapping):
        return None
    raw, encoded = item.get("raw"), item.get("raw_base64")
    if isinstance(raw, str) and encoded is None:
        try:
            return raw.encode("utf-8")
        except UnicodeEncodeError:
            return None
    if raw is None and isinstance(encoded, str):
        try:
            return base64.b64decode(encoded, validate=True)
        except ValueError:
            return None
    return None


def slim_quarantine_record(record: Mapping[str, Any]) -> tuple[dict, dict[str, bytes]]:
    """The v3 form of a published v2 record, and the input files it names.

    Each input's published bytes move to the file :func:`quarantine_input_path`
    names, and the row names that file in ``raw_ref``; the row keeps
    ``input_row``, ``raw_sha256`` and ``raw_bytes``. The event_id binds only
    those, so it is unchanged.

    A record whose bytes cannot move losslessly is returned unchanged, with no
    files: a withheld or upgraded v1 record has no input bytes to move, and any
    other record is checked to come back exactly from
    :func:`expand_quarantine_record`. So is a record with an input over
    :data:`QUARANTINE_INPUT_FILE_CEILING_BYTES`. Deterministic, so one record
    has one published form whichever host writes it.
    """
    if (record.get("schema") != QUARANTINE_SCHEMA or record.get("withheld") is not None
            or record.get("legacy") is not None or not record.get("inputs")):
        return dict(record), {}
    files: dict[str, bytes] = {}
    inputs = []
    for item in record["inputs"]:
        content = _published_input_bytes(item)
        if content is None or len(content) > QUARANTINE_INPUT_FILE_CEILING_BYTES:
            return dict(record), {}
        path = quarantine_input_path(hashlib.sha256(content).hexdigest())
        files[path] = content
        inputs.append({**item, "raw": None, "raw_base64": None, "raw_ref": path})
    slim = copy.deepcopy({name: value for name, value in record.items() if name != "inputs"})
    slim.update(schema=QUARANTINE_REF_SCHEMA, inputs=inputs)
    try:
        # The comparison union_events makes, so True and 1 stay different.
        exact = (json.dumps(expand_quarantine_record(slim, files), sort_keys=True)
                 == json.dumps(record, sort_keys=True))
    except (LedgerError, TypeError, ValueError, RecursionError):
        exact = False
    return (slim, files) if exact else (dict(record), {})


def expand_quarantine_record(record: Mapping[str, Any], files: Mapping[str, bytes | None]) -> dict:
    """The published v2 record that a v3 record stands for.

    ``files`` maps each ``raw_ref`` to the file's bytes; a file the ledger does
    not have is missing or None. A file must hash to the digest its name
    carries. Bytes that decode strictly as UTF-8 go back to ``raw`` and any
    others to ``raw_base64``, as :func:`quarantine_input` stored them, and the
    result must validate as a public v2 record, so a v3 record expands only to
    complete, exact evidence. Any other record is returned unchanged.
    """
    if record.get("schema") != QUARANTINE_REF_SCHEMA:
        return dict(record)
    validate_quarantine_records([record], public=True)
    inputs = []
    for item in record["inputs"]:
        path = item["raw_ref"]
        content = files.get(path)
        if content is None:
            raise MalformedQuarantine(
                f"quarantine record {record['event_id']!r}: input file {path} is missing"
            )
        if hashlib.sha256(content).hexdigest() != path.rpartition("/")[2]:
            raise MalformedQuarantine(
                f"quarantine record {record['event_id']!r}: input file {path} does not "
                "hash to its name"
            )
        try:
            text: str | None = content.decode("utf-8")
        except UnicodeDecodeError:
            text = None
        restored = {name: value for name, value in item.items() if name != "raw_ref"}
        restored["raw"] = text
        restored["raw_base64"] = (None if text is not None
                                  else base64.b64encode(content).decode("ascii"))
        inputs.append(restored)
    expanded = copy.deepcopy({name: value for name, value in record.items() if name != "inputs"})
    expanded.update(schema=QUARANTINE_SCHEMA, inputs=inputs)
    validate_quarantine_records([expanded], public=True)
    return expanded


def _require(condition: bool, index: int, message: str) -> None:
    if not condition:
        raise MalformedQuarantine(f"quarantine record {index}: {message}")


def validate_quarantine_records(records: Iterable[Any], *, public: bool = True) -> list[dict]:
    """Refuse a quarantine record that is not complete, exact evidence.

    Every received input must be present with a digest and length that its
    content proves (base64 always; text whenever no owner-home token stands in
    it). ``public`` additionally refuses any record that would publish an
    owner path.

    A v3 record is checked without its input files, which only
    :func:`expand_quarantine_record` reads: each input names the file that
    holds its bytes and carries none in the row, and with no owner-home token
    in the record that file is the one ``raw_sha256`` names. Whether a listed
    token is used, and the size it restores, are checked on expansion.
    """
    checked = []
    for index, record in enumerate(records, 1):
        _require(isinstance(record, dict), index, "must be an object")
        _require(set(record) == _QUARANTINE_KEYS, index,
                 f"keys differ from the v2 type: {sorted(set(record) ^ _QUARANTINE_KEYS)}")
        _require(record["schema"] in (QUARANTINE_SCHEMA, QUARANTINE_REF_SCHEMA), index,
                 f"schema {record['schema']!r}")
        by_ref = record["schema"] == QUARANTINE_REF_SCHEMA
        _require(record["event_type"] == QUARANTINE_EVENT_TYPE, index, "wrong event_type")
        _require(record["team"] == "hermit", index, "team must be hermit")
        host = record["host"]
        _require(isinstance(host, str) and bool(host) and "/" not in host
                 and host not in (".", "..") and "\0" not in host, index, f"host {host!r}")
        emitted_at = record["emitted_at"]
        _require(isinstance(emitted_at, str)
                 and _QUARANTINE_EMITTED_AT_RE.fullmatch(emitted_at) is not None,
                 index, f"emitted_at {emitted_at!r}")
        for name in ("run_id", "producer", "tree", "reason"):
            _require(isinstance(record[name], str), index, f"{name} must be a string")
        _require(isinstance(record["error"], str) and bool(record["error"].strip()),
                 index, "error must name the refusal type")
        _require(record["stage"] in QUARANTINE_STAGES, index, f"stage {record['stage']!r}")
        for name in ("cell", "row_event_id"):
            _require(record[name] is None or isinstance(record[name], str), index,
                     f"{name} must be a string or null")
        _require(record["row"] is None or isinstance(record["row"], dict), index,
                 "row must be an object or null")
        event_id = record["event_id"]
        _require(isinstance(event_id, str) and _QUARANTINE_ID_RE.fullmatch(event_id) is not None,
                 index, f"event_id {event_id!r}")
        withheld = record["withheld"]
        _require(withheld is None or (
            isinstance(withheld, dict) and set(withheld) == {"reason", "detail"}
            and withheld["reason"] == "owner-path" and isinstance(withheld["detail"], str)
        ), index, "withheld must be null or {reason: owner-path, detail}")
        owner_paths = record["owner_paths"]
        _require(isinstance(owner_paths, list), index, "owner_paths must be an array")
        # The content a token must appear in: everything but the token list itself.
        text = _quarantine_text({name: value for name, value in record.items()
                                 if name != "owner_paths"})
        growth = {}
        for ordinal, entry in enumerate(owner_paths, 1):
            _require(isinstance(entry, dict) and set(entry) == {"token", "bytes"}, index,
                     "owner_paths entries are {token, bytes}")
            _require(entry["token"] == "{{OWNER_HOME_%d}}" % ordinal, index,
                     f"owner_paths token {ordinal} is {entry['token']!r}")
            size = entry["bytes"]
            _require(type(size) is int and size > len(_OWNER_PATH_NEEDLE), index,
                     "owner_paths bytes must exceed the needle length")
            _require(by_ref or entry["token"] in text, index,
                     f"{entry['token']} is listed but unused")
            growth[entry["token"]] = size - len(entry["token"].encode())
        _require(not (withheld is not None and owner_paths), index,
                 "a withheld record carries no owner-home tokens")
        inputs = record["inputs"]
        legacy = record["legacy"]
        _require(isinstance(inputs, list), index, "inputs must be an array")
        _require(bool(inputs) or legacy is not None, index,
                 "a v2 refusal must carry the inputs it refused")
        if by_ref:
            _require(withheld is None and legacy is None and bool(inputs), index,
                     "a v3 record moves the bytes of the inputs it refused")
        input_keys = _QUARANTINE_REF_INPUT_KEYS if by_ref else _QUARANTINE_INPUT_KEYS
        for item in inputs:
            _require(isinstance(item, dict) and set(item) == input_keys, index,
                     f"inputs entries are {{{', '.join(sorted(input_keys))}}}")
            row_number = item["input_row"]
            _require(row_number is None or (type(row_number) is int and row_number >= 1),
                     index, "input_row must be a positive int or null")
            digest, size = item["raw_sha256"], item["raw_bytes"]
            _require(isinstance(digest, str) and _SHA256_RE.fullmatch(digest) is not None,
                     index, "raw_sha256 must be 64-hex")
            _require(type(size) is int and size >= 0, index, "raw_bytes must be a count")
            raw, encoded = item["raw"], item["raw_base64"]
            if by_ref:
                ref = item["raw_ref"]
                named = _QUARANTINE_REF_RE.fullmatch(ref) if isinstance(ref, str) else None
                _require(named is not None and named.group(2).startswith(named.group(1)),
                         index, f"raw_ref {ref!r} is not a quarantine input file")
                _require(raw is None and encoded is None, index,
                         "a v3 input keeps its bytes in its raw_ref file, not in the row")
                _require(bool(owner_paths) or named.group(2) == digest, index,
                         "raw_ref does not name the raw_sha256 file")
                continue
            if withheld is not None:
                _require(raw is None and encoded is None, index,
                         "a withheld record publishes no input content")
                continue
            _require((raw is None) != (encoded is None), index,
                     "exactly one of raw and raw_base64 holds the input")
            if encoded is not None:
                try:
                    content = base64.b64decode(encoded, validate=True) if isinstance(encoded, str) else None
                except ValueError:
                    content = None
                _require(content is not None, index, "raw_base64 is not base64")
                _require(hashlib.sha256(content).hexdigest() == digest and len(content) == size,
                         index, "raw_base64 does not match raw_sha256/raw_bytes")
                continue
            _require(isinstance(raw, str), index, "raw must be text")
            if not owner_paths:
                content = raw.encode("utf-8")
                _require(hashlib.sha256(content).hexdigest() == digest and len(content) == size,
                         index, "raw does not match raw_sha256/raw_bytes")
            else:
                restored = len(raw.encode("utf-8")) + sum(
                    raw.count(token) * delta for token, delta in growth.items()
                )
                _require(restored == size, index,
                         "raw with owner-home tokens restored does not match raw_bytes")
        if withheld is not None:
            _require(record["row"] is None, index, "a withheld record publishes no row")
        if record["stage"] in {"shared-checker", "spool-conflict"} and legacy is None:
            _require(isinstance(record["row_event_id"], str), index,
                     "a post-projection refusal names its row event id")
            _require(record["row"] is not None or withheld is not None, index,
                     "a post-projection refusal keeps its projected row")
        elif legacy is None:
            _require(record["row"] is None, index, "only a post-projection refusal has a row")
        if legacy is not None:
            _require(isinstance(legacy, dict) and set(legacy) == _QUARANTINE_LEGACY_KEYS, index,
                     "legacy has the v1 fields")
            _require(legacy["schema"] == LEGACY_QUARANTINE_SCHEMA, index, "legacy schema")
            _require(legacy["quarantine_id"] == event_id, index,
                     "an upgraded record keeps its v1 identity")
            _require(not inputs, index, "v1 never recorded the received bytes")
            _require(isinstance(legacy["raw_sha256"], str)
                     and _SHA256_RE.fullmatch(legacy["raw_sha256"]) is not None,
                     index, "legacy raw_sha256 must be 64-hex")
            if withheld is None and not owner_paths:
                canonical = json.dumps(
                    legacy["raw"], sort_keys=True, separators=(",", ":"), ensure_ascii=False,
                ).encode()
                _require(hashlib.sha256(canonical).hexdigest() == legacy["raw_sha256"]
                         and len(canonical) == legacy["raw_bytes"],
                         index, "legacy raw does not match its v1 digest")
        _require(_quarantine_event_id(record) == event_id, index,
                 "event_id does not bind the recorded identity")
        if public:
            _require(not quarantine_exposes_owner_path(record), index,
                     "would publish an owner path")
        checked.append(record)
    return checked


def _read_quarantine_file(path: Path) -> list[dict]:
    """Every record in one local quarantine file, strictly parsed.

    Lines are split on ``\\n`` only: v1 files were written without ASCII
    escaping, and ``str.splitlines`` would also split inside a string at
    U+2028 and other Unicode line boundaries.
    """
    if not path.is_file():
        return []
    try:
        text = path.read_bytes().decode("utf-8")
    except UnicodeDecodeError as error:
        raise MalformedEvent(f"{path}: quarantine file is not UTF-8: {error}") from error
    if text and not text.endswith("\n"):
        raise MalformedEvent(
            f"{path}: final line has no newline terminator; a writer died mid-append"
        )
    return [parse_line(line) for line in text.split("\n") if line.strip()]


def quarantine_identity(record: Mapping[str, Any]) -> Any:
    """The identity of one local quarantine line.

    A v2 record is named by its ``event_id``. A v1 line's own ``event_id``
    names the ROW it refused; its refusal identity is ``quarantine_id``.
    """
    if record.get("schema") == LEGACY_QUARANTINE_SCHEMA:
        return record.get("quarantine_id")
    return record.get("event_id")


def _local_quarantine_ids(path: Path) -> set[str]:
    known = set()
    for record in _read_quarantine_file(path):
        for name in ("event_id", "quarantine_id"):
            value = record.get(name)
            if isinstance(value, str) and value:
                known.add(value)
    return known


def _exact_quarantine_copy(record: Any) -> bool:
    """Whether ``record`` is an exact copy of one refusal: a v2 record holding
    every received input, whose ``event_id`` binds them (a valid local record).
    """
    if (not isinstance(record, dict) or record.get("schema") != QUARANTINE_SCHEMA
            or record.get("withheld") is not None or record.get("legacy") is not None
            or not record.get("inputs")):
        return False
    try:
        validate_quarantine_records([record], public=False)
    except (LedgerError, TypeError, ValueError):
        return False
    return True


def _exact_local_quarantine_ids(directory: Path, wanted: set[str]) -> set[str]:
    """The identities in ``wanted`` of which this host keeps an exact copy.

    Reads the pending, ``published/`` and ``held/`` files. Only an exact copy
    (``_exact_quarantine_copy``) counts: a record that does not validate under
    its own ``event_id`` keeps no refusal, so the refusal is retained again.
    Otherwise best effort: an unreadable file is skipped here, not fatal. The
    publisher moves it into ``corrupt/`` and the backlog gate alarms on it.
    """
    found: set[str] = set()
    for folder in (directory, directory / "published", directory / QUARANTINE_HELD_SUBDIR):
        try:
            paths = sorted(folder.glob("*.jsonl")) if folder.is_dir() else []
        except OSError:
            continue
        for path in paths:
            try:
                records = _read_quarantine_file(path)
            except (OSError, LedgerError):
                continue
            for record in records:
                identity = record.get("event_id") if isinstance(record, dict) else None
                if (isinstance(identity, str) and identity in wanted and identity not in found
                        and _exact_quarantine_copy(record)):
                    found.add(identity)
    return found


def _restoring_published_quarantine_ids(parent: Any, records: Sequence[dict]) -> set[str]:
    """The identities of ``records`` whose published form gives them back here.

    A published identity alone proves only that the refusal happened. A
    withheld form carries none of its bytes, and a token for a prefix this host
    does not know carries only the prefix's length. Only a published record
    that expands at the ledger's ``refs/remotes/origin/main``, input files
    included, and restores to the received inputs
    (``published_quarantine_restores``) makes this host's copy redundant.
    """
    commit, published = published_quarantine_at(parent, "refs/remotes/origin/main")
    by_id = {record["event_id"]: record for record in published}
    matched = {record["event_id"]: by_id[record["event_id"]]
               for record in records if record["event_id"] in by_id}
    refs = {item["raw_ref"] for public in matched.values()
            if public.get("schema") == QUARANTINE_REF_SCHEMA for item in public["inputs"]}
    files = (_ledger_files(published_repository(parent, LEDGER_REPOSITORY), commit, refs)
             if refs else {})
    restoring: set[str] = set()
    for record in records:
        public = matched.get(record["event_id"])
        if public is None or public.get("withheld") is not None:
            continue
        try:
            expanded = expand_quarantine_record(public, files)
        except LedgerError:
            continue
        if published_quarantine_restores(expanded, record):
            restoring.add(record["event_id"])
    return restoring


def _fsync_quarantine_directory(directory: Path) -> None:
    descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _make_quarantine_directory(directory: Path) -> None:
    """Create ``directory`` and its missing parents, syncing each new entry.

    A directory whose own entry was never synced can vanish in a crash, with
    every file renamed into it, however carefully those files were synced.
    """
    missing: list[Path] = []
    current = directory
    while not current.exists():
        missing.append(current)
        current = current.parent
    directory.mkdir(parents=True, exist_ok=True)
    for created in reversed(missing):
        _fsync_quarantine_directory(created.parent)


def sync_quarantine_file(path: Path) -> None:
    """Make the existing quarantine file ``path`` durable where it is.

    Its bytes, its entry in its directory and that directory's entry in its
    parent are synced. A caller that finds a file already in place cannot tell
    whether the write that put it there was synced: that write may have
    renamed it into place and then failed to sync the directory.
    """
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    _fsync_quarantine_directory(path.parent)
    _fsync_quarantine_directory(path.parent.parent)


def write_quarantine_file(directory: Path, name: str, text: str, *, replace: bool = False) -> Path:
    """Durably create ``directory/name`` holding exactly ``text``.

    The bytes go to a hidden ``.partial`` file, which no reader globs, and are
    fsynced. The file is then linked (or with ``replace``, renamed) into place
    and the directory is fsynced, as is the entry of any directory created for
    it. A reader therefore sees either the whole file or no file, never a torn
    one. Without ``replace``, an existing name is an error and is never
    overwritten.
    """
    _make_quarantine_directory(directory)
    final = directory / name
    partial = directory / f".{name}.{os.getpid()}-{secrets.token_hex(4)}.partial"
    descriptor = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o644)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        if replace:
            os.replace(partial, final)
        else:
            os.link(partial, final)
    finally:
        partial.unlink(missing_ok=True)
    _fsync_quarantine_directory(directory)
    return final


def append_quarantine(parent: Any, run_id: str, records: Sequence[dict]) -> dict:
    """Retain refused inputs once per identity, exactly, with fsync.

    Each call writes the new records to a NEW immutable file,
    ``<run>.<time_ns>-<pid>-<random>.jsonl``, and never appends to an existing
    one. It takes no lock, so it never waits for a publisher, even one that is
    part way through a network push. The publisher snapshots the complete
    top-level files and moves exactly those files once they are published. A
    file created during a publication is therefore left for the next one.

    A record is a duplicate only when its exact inputs can be had back without
    it: this host keeps an exact copy in a readable local file
    (``_exact_local_quarantine_ids``), or the test ledger's published form
    restores here to them (``_restoring_published_quarantine_ids``). A replayed
    run therefore adds nothing. A record that is published but does not
    restore here, and has no exact local copy, is written again: the publisher
    counts it as already published and holds it in ``held/``.
    Returns ``{"path", "written", "duplicates", "published_check_error"}``, where
    ``path`` is the new file, or None when nothing was new.
    """
    directory = quarantine_dir(parent)
    result: dict[str, Any] = {
        "path": None, "written": 0, "duplicates": 0, "published_check_error": None,
    }
    if not records:
        return result
    validate_quarantine_records(records, public=False)
    restoring: set[str] = set()
    try:
        restoring = _restoring_published_quarantine_ids(parent, records)
    except (LedgerError, OSError, subprocess.SubprocessError) as error:
        # Retaining the refusal matters more than deduplicating it. A duplicate
        # of a published identity is dropped again at publication.
        result["published_check_error"] = str(error)
    _make_quarantine_directory(directory)
    known = restoring | _exact_local_quarantine_ids(
        directory, {record["event_id"] for record in records})
    pending = []
    for record in records:
        if record["event_id"] in known:
            result["duplicates"] += 1
            continue
        known.add(record["event_id"])
        pending.append(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
    if pending:
        stem = _quarantine_name(run_id).removesuffix(".jsonl")
        if len(stem.encode()) > 150:
            # The record carries the run id; the name only has to be unique
            # and fit a 255-byte path component with its suffix.
            stem = "run-id-sha256-" + hashlib.sha256(run_id.encode()).hexdigest()
        name = f"{stem}.{time.time_ns()}-{os.getpid()}-{secrets.token_hex(4)}.jsonl"
        result["path"] = write_quarantine_file(directory, name, "".join(pending))
    result["written"] = len(pending)
    return result


def _published_form(record: Mapping[str, Any], *, host: str) -> dict:
    """The exact public record a local quarantine line publishes as."""
    if record.get("schema") == LEGACY_QUARANTINE_SCHEMA:
        record = upgrade_legacy_quarantine(record, host=host)
    else:
        validate_quarantine_records([record], public=False)
    return portable_quarantine_record(record)


def _quarantine_file_forms(path: Path, host: str) -> list[dict]:
    """The validated public form of every record in one local file, in order."""
    forms = []
    for record in _read_quarantine_file(path):
        portable = _published_form(record, host=host)
        validate_quarantine_records([portable], public=True)
        quarantine_shard_path("hermit", portable["host"], quarantine_bucket(portable["emitted_at"]))
        forms.append(portable)
    return forms


def set_aside_unreadable_quarantine(directory: Any, *, host: str | None = None,
                                    folder: str | None = None) -> list[str]:
    """Move each local file that cannot be published whole into ``corrupt/``.

    Checks every ``*.jsonl`` file in ``directory`` (or in its ``folder``
    subdirectory). A file that is not UTF-8, lacks its final newline, has a line
    that does not parse, or has a record whose public form does not validate is
    renamed to ``corrupt/<folder>--<name>``. The rename is atomic and the bytes
    are unchanged. Nothing republishes or deletes ``corrupt/``, and
    ``ledger_spool_backlog --store series-quarantine`` counts every file there
    as held back and alarms. One bad file therefore no longer stops the others
    publishing. Returns the names it moved.
    """
    root = Path(directory)
    source = root / folder if folder else root
    if not source.is_dir():
        return []
    fallback = host if host is not None else _writer_host()
    corrupt = root / QUARANTINE_CORRUPT_SUBDIR
    moved: list[str] = []
    for path in sorted(source.glob("*.jsonl")):
        try:
            _quarantine_file_forms(path, fallback)
            continue
        except FileNotFoundError:
            continue
        except (OSError, LedgerError):
            pass
        _make_quarantine_directory(corrupt)
        target = corrupt / f"{folder or 'pending'}--{path.name}"
        if target.exists():
            target = corrupt / f"{folder or 'pending'}--{time.time_ns()}-{path.name}"
        os.replace(path, target)
        moved.append(target.name)
    if moved:
        _fsync_quarantine_directory(corrupt)
        _fsync_quarantine_directory(source)
    return moved


def quarantine_publication_snapshot(
    directory: Any, *, host: str | None = None,
) -> tuple[dict[tuple[str, str, str], list[dict]], list[Path]]:
    """Group pending local quarantine files by the ledger shard they publish into.

    Only top-level ``*.jsonl`` files are pending. ``published/`` (proven
    published, awaiting durability), ``held/`` and ``corrupt/`` are never
    republished. Every record is converted to its portable form and validated
    as public. This function stays strict: the publisher first calls
    ``set_aside_unreadable_quarantine``, so a file that fails here is one that
    changed after that check, and the publication refuses. An identity is
    published once. Its identity excludes the observation time, so a later line
    with the same identity is the same refusal seen again, and the first one (in
    file, then line, order) stands.
    """
    root = Path(directory)
    pending: dict[tuple[str, str, str], list[dict]] = {}
    files: list[Path] = []
    if not root.is_dir():
        return pending, files
    fallback = host if host is not None else _writer_host()
    seen: set[str] = set()
    for path in sorted(root.glob("*.jsonl")):
        for portable in _quarantine_file_forms(path, fallback):
            if portable["event_id"] in seen:
                continue
            seen.add(portable["event_id"])
            key = ("hermit", portable["host"], quarantine_bucket(portable["emitted_at"]))
            pending.setdefault(key, []).append(portable)
        files.append(path)
    return {key: union_events(value) for key, value in pending.items()}, files


# ------------------------------------------------- published scan vouching
#
# published_rows_at_repo and published_quarantine_at_repo validated every row
# and every quarantine record ever published, on every call: the owner-path
# predicate, the canonical JSON and the shared Rust checker over 1,474,708
# series rows (9,888 shards, 1.89 GB) and 26,433 quarantine records (74
# shards, 525 MB) at test-ledger main 1149b492 on 2026-10-12. A publication
# calls them at least twice (the publisher's reap view and the mirror's
# snapshot), each under ignored/validate/publication.lock. Published shards
# only grow by appending, and every one of those checks is a pure function of
# one row, so a row whose bytes an earlier call already validated need not be
# validated again.
#
# After a call validates every row and returns, it records, for each shard
# it read, the blob's Git object id, size and SHA-256, keyed by a digest of
# everything the verdict depends on besides those bytes (_published_scan_inputs),
# named by content and by paths under the tool root, never the root's own path.
# The record lives in the repository's own Git directory, one file per commit
# read (the newest _PUBLISHED_SCAN_KEEP), and is also kept in this process. A
# later call takes this commit's own record, else the newest whose commit is
# an ancestor, else the newest, and checks it whole (schema,
# kind, inputs, object format, field shapes, its own digest) and then, for
# each recorded shard, that the shard now starts with exactly the recorded
# bytes: the prefix of the recorded size ends with a newline and has the
# recorded Git object id and SHA-256, both recomputed from the bytes read
# now. Only the rows after those prefixes, and every row of a shard the
# record does not name, are validated. Every row is still parsed, the union
# and its conflicts are recomputed over every row, and a refusal for a new
# row is the full read's refusal: the owner-path refusal carries no row
# position, and a refusal whose message names one (the shared checker's
# stdin line, a quarantine record's index) makes the call run the full read,
# which raises it.
#
# The full read runs instead when there is no usable record (none, corrupt,
# a digest that does not match its content, another kind, inputs or object
# format), when a recorded shard was removed or does not start with its
# recorded bytes, when the inputs cannot be named (a predicate or checker
# replaced by a non-function, an unusable tool root), or when
# CI_HUB_PUBLISHED_SCAN_VOUCH=off. PUBLISHED_SCANS records each call's path
# and why, and each call prints one "published scan:" line to stderr saying
# fast or full and, for full, why. A caller that must not write (a read-only
# health gate) passes record=False: it still uses the records, and writes none.
#
# The record is integrity-checked, not authenticated: an agent able to write
# a self-consistent false record can equally edit this module. What a record
# cannot do is vouch for bytes other than the ones it names.

PUBLISHED_SCAN_VOUCH_ENV = "CI_HUB_PUBLISHED_SCAN_VOUCH"
_PUBLISHED_SCAN_SCHEMA = "ci-hub-published-scan-vouch/1"
_PUBLISHED_SCAN_DIRECTORY = "ci-hub-published-scan"
_SERIES_ROWS_SCAN = "series-rows"
_QUARANTINE_SCAN = "quarantine"
_PUBLISHED_SCAN_RECORD_KEYS = frozenset(
    {"schema", "kind", "inputs", "object_format", "commit", "shards", "digest"})
_PUBLISHED_SCAN_SHARD_KEYS = frozenset({"oid", "size", "sha256"})
_GIT_OBJECT_ID_HEX = {"sha1": 40, "sha256": 64}
# The shared checker compiles these Hermit crates (check-series-rows.rs's
# manifest), relative to the tool's Hermit checkout.
_SERIES_CHECKER_CRATES = ("ci/manifest-plan", "detcore-model")

# Records kept per repository and kind: one per commit read, the newest first,
# so the mirror reading its base and then its head finds each one's own.
_PUBLISHED_SCAN_KEEP = 4

#: Each published scan, newest last: its kind and commit, "delta" (rows an
#: earlier validation covered were not validated again) or "full", why a
#: delta was not used, and how many rows or records it validated and skipped.
PUBLISHED_SCANS: collections.deque = collections.deque(maxlen=256)
# (record directory, kind) -> {commit: (time, record)} this process wrote there.
_PUBLISHED_SCAN_MEMO: dict[tuple[str, str], dict[str, tuple[float, dict]]] = {}


class _VouchRefused(Exception):
    """A delta read cannot answer exactly as the full read would; read in full."""


def published_scan_stats() -> dict[str, int]:
    """How many recorded published scans used a delta and how many read in full."""
    counts = {"delta": 0, "full": 0}
    for item in PUBLISHED_SCANS:
        counts[item["path"]] += 1
    return counts


def _portable_code(code: types.CodeType) -> types.CodeType:
    """``code`` and every code object it holds, without the file name they were loaded from."""
    consts = tuple(_portable_code(item) if isinstance(item, types.CodeType) else item
                   for item in code.co_consts)
    return code.replace(co_filename="", co_consts=consts)


def _code_digest(function: Any) -> str | None:
    """The compiled code of ``function``, or None when it is not a plain function.

    The file name is left out: a publication runs this module from a tool
    root materialized at a new path each time (``/proc/<pid>/fd/<n>``), and
    the same code under another path is the same predicate.
    """
    if not isinstance(function, types.FunctionType):
        return None
    return hashlib.sha256(marshal.dumps(_portable_code(function.__code__))).hexdigest()


# A Rust source naming another file it compiles: a module at an explicit path,
# or a file included as source, text or bytes.
_RUST_FILE_REFERENCE_RE = re.compile(
    r'#\[\s*path\s*=\s*"([^"]+)"\s*\]|\binclude(?:_str|_bytes)?!\s*\(\s*"([^"]+)"')
_RUST_MODULE_RE = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;',
                             re.MULTILINE)


def _checker_source_files(tool: Path) -> list[Path]:
    """Every file the shared checker's crates compile, as far as their sources name them.

    Each crate's Cargo.toml and every file under its ``src``, then, followed
    from every Rust file in the set, each ``#[path = "..."]`` module and
    ``include!``/``include_str!``/``include_bytes!`` file (manifest-plan
    compiles hermit-cli and scripts/lib sources this way) and each plain
    ``mod name;`` beside such a file. Over-inclusion only costs a full read.
    """
    tool_hermit = tool / "hermit"
    files: set[Path] = {tool_hermit / "rust-toolchain.toml"}
    pending: list[Path] = []
    for crate in _SERIES_CHECKER_CRATES:
        root = tool_hermit / crate
        files.add(root / "Cargo.toml")
        pending.extend(path for path in (root / "src").rglob("*") if path.is_file())
    while pending:
        path = pending.pop()
        if path in files and path.suffix != ".rs":
            continue
        new = path not in files
        files.add(path)
        if path.suffix != ".rs" or not new:
            continue
        text = path.read_text(errors="replace")
        named = [os.path.normpath(path.parent / (first or second))
                 for first, second in _RUST_FILE_REFERENCE_RE.findall(text)]
        for module in _RUST_MODULE_RE.findall(text):
            for directory in (path.parent, path.parent / path.stem):
                named.extend(os.path.normpath(directory / candidate)
                             for candidate in (f"{module}.rs", f"{module}/mod.rs"))
        for target in map(Path, named):
            if target.is_file() and target not in files:
                pending.append(target)
    return sorted(files)


def _series_checker_sources() -> list[tuple[str, bytes]] | None:
    """The shared checker and the Hermit sources it compiles, or None if unnamed.

    Each is named by its path under the tool root, never the root's own
    path, which a publication materializes anew each time. A named file
    outside the tool root makes the sources unnameable.
    """
    try:
        checker = _shared_checker_script(dict(os.environ))
        tool = checker.parents[2]
        files = [checker, *_checker_source_files(tool)]
        return [(path.relative_to(tool).as_posix(), path.read_bytes()) for path in files]
    except (OSError, ValueError, SeriesError):
        return None


def _published_scan_inputs(kind: str) -> str | None:
    """Digest of what a published scan's verdict depends on besides the shard bytes.

    This module's source; the source of the ledger module it imports
    (parse_line, union_events and the event expansion every row goes through:
    a change there changes what a vouched row expands to, which must then be
    checked again); the compiled code of the owner-path predicate and
    of the functions that check a row or a record, without the file they
    were loaded from; the needle; for series rows the contents of the shared
    checker, the Hermit crates it compiles and their toolchain file, each
    named by its path under the tool root; for quarantine records
    the text encoding ``git show`` output is decoded with; and the Python
    version. None, so that no record is used or written, when any of them
    cannot be named.
    """
    functions: tuple[Any, ...]
    if kind == _SERIES_ROWS_SCAN:
        functions = (_contains_owner_path, _canonical_row_json, _run_shared_checker,
                     _shared_checker_script)
    elif kind == _QUARANTINE_SCAN:
        functions = (validate_quarantine_records, quarantine_exposes_owner_path,
                     _quarantine_text, _require, _quarantine_event_id)
    else:
        return None
    digest = hashlib.sha256()
    for item in (_PUBLISHED_SCAN_SCHEMA, kind, sys.version, _OWNER_PATH_NEEDLE):
        digest.update(f"{item}\0".encode())
    for function in functions:
        code = _code_digest(function)
        if code is None:
            return None
        digest.update(f"{getattr(function, '__qualname__', '?')}={code}\0".encode())
    try:
        digest.update(hashlib.sha256(Path(__file__).read_bytes()).digest())
        digest.update(hashlib.sha256(Path(_ledger.__file__).read_bytes()).digest())
        directory = Path(__file__).resolve().parents[1] / "ledger"
        for name in ("ledger_retention.py", "archive_store.py", "file_archive.py"):
            digest.update(name.encode() + b"\0")
            digest.update(hashlib.sha256((directory / name).read_bytes()).digest())
    except (OSError, TypeError):
        return None
    if kind == _SERIES_ROWS_SCAN:
        sources = _series_checker_sources()
        if sources is None:
            return None
        for name, content in sources:
            digest.update(f"{name}={hashlib.sha256(content).hexdigest()}\0".encode())
    else:
        encoding = "utf-8" if sys.flags.utf8_mode else locale.getpreferredencoding(False)
        digest.update(f"encoding={encoding}\0".encode())
    return digest.hexdigest()


def _git_object_id(data: Any, object_format: str) -> str:
    """The Git blob object id of ``data`` in ``object_format`` (sha1 or sha256)."""
    hasher = hashlib.new(object_format)
    hasher.update(b"blob %d\0" % len(data))
    hasher.update(data)
    return hasher.hexdigest()


def _published_scan_digest(record: Mapping[str, Any]) -> str:
    body = {key: value for key, value in record.items() if key != "digest"}
    return hashlib.sha256(
        json.dumps(body, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _checked_published_scan_record(
    record: Any, *, kind: str, inputs: str, object_format: str,
) -> tuple[dict | None, str | None]:
    """``record`` if every field has its shape and it matches this scan, else a reason."""
    if not isinstance(record, dict) or set(record) != _PUBLISHED_SCAN_RECORD_KEYS:
        return None, "the record is not a published-scan record"
    if record["schema"] != _PUBLISHED_SCAN_SCHEMA or record["kind"] != kind:
        return None, "the record is of another schema or kind"
    if record["digest"] != _published_scan_digest(record):
        return None, "the record's digest does not match its content"
    if record["inputs"] != inputs:
        return None, "the validation inputs changed"
    if record["object_format"] != object_format:
        return None, "the record is of another object format"
    width = _GIT_OBJECT_ID_HEX[object_format]
    commit, shards = record["commit"], record["shards"]
    if not (isinstance(commit, str) and re.fullmatch(f"[0-9a-f]{{{width}}}", commit)):
        return None, "the record names no commit"
    if not isinstance(shards, dict):
        return None, "the record's shards are not a map"
    for relative, entry in shards.items():
        if not (
            isinstance(relative, str) and isinstance(entry, dict)
            and set(entry) == _PUBLISHED_SCAN_SHARD_KEYS
            and isinstance(entry["oid"], str)
            and re.fullmatch(f"[0-9a-f]{{{width}}}", entry["oid"]) is not None
            and type(entry["size"]) is int and entry["size"] >= 0
            and isinstance(entry["sha256"], str)
            and _SHA256_RE.fullmatch(entry["sha256"]) is not None
        ):
            return None, "a recorded shard is malformed"
    return record, None


class _PublishedScan:
    """Which bytes of one published read an earlier full validation covered.

    ``delta`` is true when a record is in use: only rows outside the bytes
    ``vouched_prefix`` returns are validated. ``directory`` holds the
    repository's records, one per commit read (the newest
    ``_PUBLISHED_SCAN_KEEP``); this read's own record is written there once
    every row validated, unless ``write`` is false or there is no directory.
    """

    def __init__(self, kind: str, commit: str, *, directory: Path | None = None,
                 write: bool = True, inputs: str | None = None, object_format: str = "sha1",
                 record: dict | None = None, reason: str | None = None) -> None:
        self.kind, self.commit = kind, commit
        self.directory, self.write = directory, write
        self.inputs, self.object_format = inputs, object_format
        self.record, self.reason = record, reason
        self.observed: dict[str, dict] = {}
        self.vouched = 0
        self.checked = 0
        self.started = time.monotonic()

    @property
    def delta(self) -> bool:
        return self.record is not None

    @property
    def records(self) -> bool:
        """Whether this read needs its shards' bytes: to check a record, or to write one."""
        return self.record is not None or (self.directory is not None and self.write)

    @classmethod
    def open(cls, repo: Path, kind: str, commit: str, shards: Sequence[str], *,
             write: bool = True) -> "_PublishedScan":
        if os.environ.get(PUBLISHED_SCAN_VOUCH_ENV, "").lower() in {"off", "0", "no"}:
            return cls(kind, commit, reason="disabled")
        inputs = _published_scan_inputs(kind)
        if inputs is None:
            return cls(kind, commit, reason="the validation inputs cannot be named")
        located = _git(repo, ["rev-parse", "--path-format=absolute", "--git-common-dir",
                              "--show-object-format"])
        lines = located.stdout.splitlines()
        if located.returncode != 0 or len(lines) != 2 or lines[1] not in _GIT_OBJECT_ID_HEX:
            return cls(kind, commit, reason="the repository's Git directory cannot be named")
        directory = Path(lines[0]) / _PUBLISHED_SCAN_DIRECTORY / kind
        object_format = lines[1]
        scan = cls(kind, commit, directory=directory, write=write, inputs=inputs,
                   object_format=object_format)
        chosen, candidate = _published_scan_candidate(repo, directory, kind, commit)
        if chosen is None:
            scan.reason = "no record"
            return scan
        if candidate is None:
            scan.reason = "the record is unreadable"
            return scan
        record, reason = _checked_published_scan_record(
            candidate, kind=kind, inputs=inputs, object_format=object_format)
        if record is not None and record["commit"] != chosen:
            record, reason = None, "the record names another commit than its file"
        if record is not None and not set(record["shards"]) <= set(shards):
            record, reason = None, "a recorded shard was removed"
        scan.record, scan.reason = record, reason
        return scan

    def full(self, reason: str) -> "_PublishedScan":
        """The same read with no record: every row is validated."""
        scan = _PublishedScan(self.kind, self.commit, directory=self.directory, write=self.write,
                              inputs=self.inputs, object_format=self.object_format,
                              reason=reason)
        scan.started = self.started
        return scan

    def vouched_prefix(self, relative: str, oid: str, data: bytes) -> int:
        """How many leading bytes of shard RELATIVE (DATA) an earlier validation covered.

        Raises ``_VouchRefused`` when the record names the shard and the shard
        does not start with exactly the recorded bytes.
        """
        if not self.records:
            return 0
        whole = hashlib.sha256(data).hexdigest()
        self.observed[relative] = {"oid": oid, "size": len(data), "sha256": whole}
        entry = None if self.record is None else self.record["shards"].get(relative)
        if entry is None:
            return 0
        size = entry["size"]
        if size > len(data) or (size and data[size - 1:size] != b"\n"):
            raise _VouchRefused(f"{relative} does not start with its vouched bytes")
        if size == len(data):
            # The whole shard: Git read these bytes as object OID, so that is
            # their object id, and hashing them into one again (SHA-1 over
            # every unchanged shard, about 2.1 GB a read) adds nothing.
            # Their SHA-256 is still computed from the bytes read now.
            if whole != entry["sha256"] or oid != entry["oid"]:
                raise _VouchRefused(f"{relative} does not start with its vouched bytes")
            return size
        prefix = memoryview(data)[:size]
        if (hashlib.sha256(prefix).hexdigest() != entry["sha256"]
                or _git_object_id(prefix, self.object_format) != entry["oid"]):
            raise _VouchRefused(f"{relative} does not start with its vouched bytes")
        return size

    def vouch(self) -> None:
        """Every row validated: record this read's shards for a later read."""
        global _PUBLISHED_SCAN_MEMO
        if self.directory is None or not self.write:
            return
        record = {"schema": _PUBLISHED_SCAN_SCHEMA, "kind": self.kind, "inputs": self.inputs,
                  "object_format": self.object_format, "commit": self.commit,
                  "shards": {path: dict(entry) for path, entry in self.observed.items()}}
        record["digest"] = _published_scan_digest(record)
        key = (str(self.directory), self.kind)
        memo = dict(_PUBLISHED_SCAN_MEMO.get(key, {}))
        memo[self.commit] = (time.time(), record)
        for stale in sorted(memo, key=lambda commit: memo[commit][0])[:-_PUBLISHED_SCAN_KEEP]:
            del memo[stale]
        # Website validation keys its memo on reachable global identities.
        # Replace both mappings so recording or eviction invalidates that key,
        # while any previous validation keeps an unchanged snapshot.
        _PUBLISHED_SCAN_MEMO = {**_PUBLISHED_SCAN_MEMO, key: memo}
        try:
            self.directory.mkdir(parents=True, exist_ok=True)
            handle, temporary = tempfile.mkstemp(dir=self.directory, prefix=".tmp-")
            try:
                with os.fdopen(handle, "w") as out:
                    json.dump(record, out, sort_keys=True, separators=(",", ":"))
                os.replace(temporary, self.directory / f"{self.commit}.json")
            except BaseException:
                with contextlib.suppress(OSError):
                    os.unlink(temporary)
                raise
            kept = sorted(_published_scan_files(self.directory).items(),
                          key=lambda item: item[1][0], reverse=True)
            for _commit, (_stamp, path) in kept[_PUBLISHED_SCAN_KEEP:]:
                with contextlib.suppress(OSError):
                    path.unlink()
        except OSError:
            pass  # only costs a later full read

    def note(self) -> None:
        seconds = round(time.monotonic() - self.started, 1)
        PUBLISHED_SCANS.append({
            "kind": self.kind, "commit": self.commit,
            "path": "delta" if self.delta else "full", "reason": self.reason,
            "vouched": self.vouched, "checked": self.checked, "seconds": seconds,
        })
        # One line per scan, so a record that never matches is seen. The
        # "ledger publication:" prefix is what a publication unit repeats from
        # its publisher's stderr (start_unit.relay_publication_notes); without
        # it these lines were discarded with the rest.
        if self.delta:
            print(f"ledger publication: published scan: {self.kind} at {self.commit[:12]} fast: validated "
                  f"{self.checked}, vouched {self.vouched} in {seconds} s", file=sys.stderr)
        else:
            print(f"ledger publication: published scan: {self.kind} at {self.commit[:12]} full ({self.reason}): "
                  f"validated {self.checked} in {seconds} s", file=sys.stderr)


def _published_scan_files(directory: Path) -> dict[str, tuple[float, Path]]:
    """Each record file in DIRECTORY by the commit its name gives, with its mtime."""
    found: dict[str, tuple[float, Path]] = {}
    try:
        paths = list(directory.glob("*.json"))
    except OSError:
        return found
    for path in paths:
        if _SHA_RE.fullmatch(path.stem) is None and _SHA256_RE.fullmatch(path.stem) is None:
            continue
        try:
            found[path.stem] = (path.stat().st_mtime, path)
        except OSError:
            continue
    return found


def _published_scan_candidate(repo: Path, directory: Path, kind: str,
                              commit: str) -> tuple[str | None, Any]:
    """The record to try for a read of COMMIT: (its commit, its parsed content).

    This commit's own record if there is one; else the newest whose commit
    is an ancestor of COMMIT; else the newest. (None, None) when there is
    none; (commit, None) when the chosen file cannot be read.
    """
    found: dict[str, tuple[float, Any]] = dict(_published_scan_files(directory))
    for known, (stamp, record) in _PUBLISHED_SCAN_MEMO.get((str(directory), kind), {}).items():
        found[known] = (stamp, record)
    if not found:
        return None, None
    if commit in found:
        chosen = commit
    else:
        newest = sorted(found, key=lambda known: found[known][0], reverse=True)
        ancestors = [known for known in newest if _git(repo, [
            "--no-replace-objects", "merge-base", "--is-ancestor", known, commit,
        ]).returncode == 0]
        chosen = (ancestors or newest)[0]
    value = found[chosen][1]
    if isinstance(value, Path):
        try:
            value = json.loads(value.read_bytes())
        except (OSError, ValueError, RecursionError):
            return chosen, None
    return chosen, value


def _scanned(read: Callable[["_PublishedScan"], Any], scan: "_PublishedScan") -> Any:
    """``read(scan)``; when the delta cannot vouch for its answer, the full read's."""
    try:
        try:
            return read(scan)
        except _VouchRefused as refused:
            scan = scan.full(str(refused))
        # Outside the handler, so the full read's refusal carries no context.
        return read(scan)
    finally:
        scan.note()


#: Each process's quarantine records by (Git common directory, resolved
#: commit), with whether that read wrote its vouch record. A commit's records
#: cannot change, and one publication read the same commit twice (2026-10-13:
#: two "published scan: quarantine at <same commit>" lines, about 6 s each, in
#: every publication). The key is the common directory, where the vouch
#: records live, not the path: the two reads go through the ledger writer and
#: the parent's hermit_test_ledger submodule, two checkouts of one repository
#: (both .git/modules/hermit_test_ledger), so a path key never matched.
#: Replaced, never mutated, like _PUBLISHED_SCAN_MEMO.
_QUARANTINE_READS: dict[tuple[str, str], tuple[bool, list[dict]]] = {}


def published_quarantine_at_repo(repo: Any, published_ref: str, *,
                                 record: bool = True, files: Any = None) -> tuple[str, list[dict]]:
    """Every published quarantine record at one resolved ledger commit.

    Records an earlier call validated are not validated again; see
    "published scan vouching" above. ``record=False`` writes no record. A
    second read of the same commit in one process returns a copy of the
    first read's records, unless only this one must write a record or
    vouching is off (CI_HUB_PUBLISHED_SCAN_VOUCH).
    """
    global _QUARANTINE_READS
    repo = Path(repo)
    resolved = _git(repo, ["--no-replace-objects", "rev-parse", "--verify",
                           f"{published_ref}^{{commit}}"])
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA_RE.fullmatch(commit) is None:
        raise SeriesError(
            f"cannot resolve published quarantine revision {published_ref}: "
            + (resolved.stderr.strip() or "no output")
        )
    owned_files = files is None
    files = file_archive_snapshot(repo, commit, files=files)
    try:
        files.verify(files.listing(("quarantine",)))
        located = _git(repo, ["rev-parse", "--path-format=absolute", "--git-common-dir"])
        common = located.stdout.strip().splitlines()[0] if located.returncode == 0 and located.stdout.strip() else ""
        key = (common or str(repo.resolve()), commit)
        earlier = _QUARANTINE_READS.get(key)
        # With vouching off every record is validated again: that is the full
        # read the vouched one is checked against, so it never reuses either.
        full = os.environ.get(PUBLISHED_SCAN_VOUCH_ENV, "").lower() in {"off", "0", "no"}
        if earlier is not None and not full and (earlier[0] or not record):
            print(f"ledger publication: published scan: quarantine at {commit[:12]} reused: "
                  f"{len(earlier[1])} record(s) read earlier in this process", file=sys.stderr)
            return commit, copy.deepcopy(earlier[1])
        commit, records = _published_quarantine_at_commit(repo, commit, record=record, files=files)
        _QUARANTINE_READS = {**_QUARANTINE_READS, key: (record, copy.deepcopy(records))}
        return commit, records
    finally:
        if owned_files:
            files.close()


def _published_quarantine_at_commit(repo: Path, commit: str, *,
                                    record: bool, files: Any = None) -> tuple[str, list[dict]]:
    """published_quarantine_at_repo's read of one resolved commit."""
    owned_files = files is None
    files = file_archive_snapshot(repo, commit, files=files)
    try:
        relatives = []
        for relative in files.listing(("quarantine/series",)):
            parts = Path(relative).parts
            if (len(parts) != 5 or parts[:2] != ("quarantine", "series")
                    or not relative.endswith(".jsonl")):
                continue
            relatives.append(relative)
        scan = _PublishedScan.open(repo, _QUARANTINE_SCAN, commit, relatives, write=record)
        return commit, _scanned(
            lambda scan: _published_quarantine_read(repo, commit, relatives, scan, files=files), scan)
    finally:
        if owned_files:
            files.close()


def _published_quarantine_read(repo: Path, commit: str, relatives: Sequence[str],
                               scan: "_PublishedScan", *, files: Any = None) -> list[dict]:
    """published_quarantine_at_repo's records, validating only what SCAN does not vouch for."""
    owned_files = files is None
    files = file_archive_snapshot(repo, commit, files=files)
    try:
        blobs = files.iter_blobs(relatives)
        records: list[dict] = []
        fresh: list[dict] = []
        try:
            for relative, oid, data in blobs:
                text = data.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
                if text and not text.endswith("\n"):
                    raise MalformedEvent(f"{commit}:{relative}: final line has no newline terminator")
                parsed = [parse_line(line) for line in text.split("\n") if line.strip()]
                records.extend(parsed)
                if not scan.records:
                    continue
                size = scan.vouched_prefix(relative, oid, data)
                if size == len(data):
                    kept = len(parsed)
                elif size == 0:
                    kept = 0
                else:
                    # The records of the vouched bytes: the leading lines of the text
                    # git show printed, which must start with those bytes' text.
                    try:
                        prefix = data[:size].decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
                    except UnicodeDecodeError:
                        raise _VouchRefused(f"{relative}: vouched bytes are not UTF-8") from None
                    if not text.startswith(prefix):
                        raise _VouchRefused(f"{relative}: vouched bytes are not the text's start")
                    kept = sum(1 for line in prefix.split("\n") if line.strip())
                if scan.delta:
                    scan.vouched += kept
                    fresh.extend(parsed[kept:])
        finally:
            blobs.close()
        if scan.delta:
            scan.checked = len(fresh)
            try:
                validate_quarantine_records(fresh, public=True)
            except Exception as error:  # noqa: BLE001 - the full read raises it, index and all
                raise _VouchRefused(f"a new record is refused: {error}") from None
            checked = records
        else:
            scan.checked = len(records)
            checked = validate_quarantine_records(records, public=True)
        published = union_events(checked)
        scan.vouch()
        return published
    finally:
        if owned_files:
            files.close()


def published_quarantine_at(parent: Any, published_ref: str) -> tuple[str, list[dict]]:
    return published_quarantine_at_repo(
        published_repository(parent, LEDGER_REPOSITORY), published_ref,
    )


def store_paths(parent: Any) -> list[Path]:
    """Every tracked, live, and published-but-not-yet-synced batch."""
    tracked = sorted((ledger_root(parent) / "series").glob("*/*/*.jsonl"))
    spool = spool_dir(parent)
    live = sorted(spool.glob("*.jsonl"))
    retained = sorted((spool / "published").glob("*.jsonl"))
    return tracked + live + retained


def refresh_origin_main(checkout: Any, repository: str) -> None:
    """Refresh one checkout's remote main without moving its working tree."""
    checkout = Path(checkout)
    if not _is_checkout(checkout):
        raise SeriesError(f"{repository} checkout is unavailable at {checkout}")
    command = [
        "git", "-C", str(checkout), "fetch", "--quiet", "--force", "origin",
        "refs/heads/main:refs/remotes/origin/main",
    ]
    proxy = shutil.which(os.environ.get("WITH_PROXY", "with-proxy"))
    if proxy:
        command.insert(0, proxy)
    env = dict(os.environ)
    for name in _GIT_REPO_SCOPED_ENV:
        env.pop(name, None)
    fetched = subprocess.run(
        command, capture_output=True, text=True, timeout=60, check=False, env=env,
    )
    if fetched.returncode != 0:
        detail = fetched.stderr.strip() or fetched.stdout.strip() or "no output"
        raise SeriesError(f"cannot fetch {repository} origin/main: {detail}")


def published_snapshot(parent: Any) -> PublishedSnapshot:
    """Fetch the ledger, then read one immutable published revision."""
    root = ledger_root(parent)
    refresh_origin_main(root, "cell ledger")
    return published_snapshot_at(
        parent, "refs/remotes/origin/main", repository=LEDGER_REPOSITORY,
    )


def published_snapshot_at(
    parent: Any, published_ref: str, *, repository: str = PARENT_REPOSITORY,
) -> PublishedSnapshot:
    """Resolve ``published_ref`` once and read only that immutable commit.

    Every tree and blob lookup below uses the resolved 40-hex object id.  A
    concurrent fetch may move the caller's ref after resolution, but it cannot
    splice shards from two revisions into one reported snapshot.
    """
    return _published_snapshot_at_repo(
        published_repository(parent, repository), published_ref, repository=repository,
    )


def _published_snapshot_at_repo(
    parent: Path, published_ref: str, *, repository: str, files: Any = None,
) -> PublishedSnapshot:
    resolved = _git(parent, [
        "--no-replace-objects", "rev-parse", "--verify", f"{published_ref}^{{commit}}",
    ])
    if resolved.returncode != 0:
        raise SeriesError(
            f"cannot resolve published series revision {published_ref}: "
            + (resolved.stderr.strip() or "no output")
        )
    commit = resolved.stdout.strip()
    if not _SHA_RE.fullmatch(commit):
        raise SeriesError(
            f"published series revision {published_ref} resolved to invalid commit "
            f"{commit!r}"
        )

    owned_files = files is None
    files = file_archive_snapshot(parent, commit, files=files)
    try:
        shards = [
            relative for relative in files.listing(("series",))
            if len(Path(relative).parts) == 4 and Path(relative).parts[0] == "series"
            and relative.endswith(".jsonl")
        ]
        rows: list[dict] = []
        for relative, _oid, data in files.iter_blobs(shards):
            text = data.decode("utf-8")
            if text and not text.endswith("\n"):
                raise MalformedEvent(
                    f"parent {commit}:{relative}: final line has no newline terminator"
                )
            rows.extend(parse_line(line) for line in text.splitlines() if line.strip())
        return PublishedSnapshot(commit, union_events(validate_read_rows(rows)), repository, files)
    except BaseException:
        if owned_files and files is not None:
            files.close()
        raise


class PublishedRowView(dict):
    """Selected exact bodies with an operation-boundary archive guard."""

    def __init__(self, rows: Mapping[str, dict], files: Any, paths: Iterable[str]):
        super().__init__(rows)
        self.files = files
        self.paths = tuple(paths)

    def verify(self) -> None:
        self.files.verify(self.paths)

    def close(self) -> None:
        self.files.close()

    def __getitem__(self, key):
        self.verify()
        return super().__getitem__(key)

    def get(self, key, default=None):
        self.verify()
        return super().get(key, default)

    def __contains__(self, key):
        self.verify()
        return super().__contains__(key)

    def __iter__(self):
        self.verify()
        return super().__iter__()

    def __len__(self):
        self.verify()
        return super().__len__()

    def items(self):
        self.verify()
        return super().items()

    def keys(self):
        self.verify()
        return super().keys()

    def values(self):
        self.verify()
        return super().values()

    def copy(self):
        self.verify()
        return super().copy()

    def __eq__(self, other):
        self.verify()
        return super().__eq__(other)

    def __ne__(self, other):
        self.verify()
        return super().__ne__(other)


class PublishedRows(Sequence):
    """Every published series row, held as its canonical JSON text.

    A snapshot's rows were every series row of the ledger as Python objects:
    1,140,698 rows, about 8 GB, on 2026-10-11, held from the snapshot through
    the scorecard render, beside the renderer's own memory
    (https://github.com/rrnewton/dev-hermit/issues/554). Here they are the
    ledger's 1.3 GB of text. A row is parsed only when it is read, so code
    that indexes or iterates them is unchanged.

    The texts and the event ids are each ONE bytearray with an array of end
    offsets, not a str per row. A million small strings kept alive while the
    rows they were made from are freed pin the freed memory: measured on the
    real ledger, a str per row held 8.1 GB of RSS for 1.3 GB of text. Each
    text is the exact line _canonical_rows_bytes writes, so the row digest
    and the snapshot bytes are produced from them byte for byte.
    """

    __slots__ = ("_texts", "_ends", "_ids", "_id_ends", "max_emitted_at", "_files", "_paths")

    def __init__(self, rows: Sequence[Mapping[str, Any]]) -> None:
        self._files, self._paths = None, ()
        self._texts, self._ends = bytearray(), array.array("Q")
        self._ids, self._id_ends = bytearray(), array.array("Q")
        newest = None
        for row in rows:
            self._texts += json.dumps(row, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
            self._ends.append(len(self._texts))
            self._ids += row["event_id"].encode()
            self._id_ends.append(len(self._ids))
            if newest is None or row["emitted_at"] > newest:
                newest = row["emitted_at"]
        # A copy, so no string of the rows outlives them.
        self.max_emitted_at = None if newest is None else (newest + "\0")[:-1]

    @classmethod
    def _from_parts(cls, texts: bytearray, ends: "array.array", ids: bytearray,
                    id_ends: "array.array", max_emitted_at: str | None) -> "PublishedRows":
        rows = cls.__new__(cls)
        rows._texts, rows._ends, rows._ids, rows._id_ends = texts, ends, ids, id_ends
        rows.max_emitted_at = max_emitted_at
        rows._files, rows._paths = None, ()
        return rows

    def bind_files(self, files: Any, paths: Iterable[str]) -> "PublishedRows":
        self._files, self._paths = files, tuple(paths)
        return self

    def verify(self) -> None:
        if self._files is not None:
            self._files.verify(self._paths)

    def close(self) -> None:
        if self._files is not None:
            self._files.close()

    def __len__(self) -> int:
        self.verify()
        return len(self._ends)

    def _text(self, index: int) -> memoryview:
        start = self._ends[index - 1] if index else 0
        return memoryview(self._texts)[start:self._ends[index]]

    def __getitem__(self, index):
        self.verify()
        if isinstance(index, slice):
            return [json.loads(bytes(self._text(position)))
                    for position in range(*index.indices(len(self._ends)))]
        if index < 0:
            index += len(self._ends)
        if not 0 <= index < len(self._ends):
            raise IndexError("published row index out of range")
        return json.loads(bytes(self._text(index)))

    def __iter__(self):
        self.verify()
        for index in range(len(self._ends)):
            yield json.loads(bytes(self._text(index)))

    def texts(self) -> Iterator[memoryview]:
        """Each row's canonical text, in order, without copying it."""
        self.verify()
        view, start = memoryview(self._texts), 0
        for end in self._ends:
            yield view[start:end]
            start = end

    def digest(self) -> str:
        """sha256 of _canonical_rows_bytes(rows), without building those bytes."""
        hasher = hashlib.sha256()
        for text in self.texts():
            hasher.update(text)
            hasher.update(b"\n")
        return hasher.hexdigest()

    def rows_for(self, event_ids: set[str]) -> dict[str, dict]:
        """The rows of just EVENT_IDS, parsed, keyed by event id."""
        self.verify()
        wanted = {event_id.encode() for event_id in event_ids}
        found: dict[str, dict] = {}
        ids, start = memoryview(self._ids), 0
        for index, end in enumerate(self._id_ends):
            key = bytes(ids[start:end])
            start = end
            if key in wanted:
                found[key.decode()] = json.loads(bytes(self._text(index)))
        return (PublishedRowView(found, self._files, self._paths)
                if self._files is not None else found)

    def by_event_id(self) -> dict[bytes, bytes]:
        """Each row's canonical text by event id, for comparing two snapshots."""
        ids, start = memoryview(self._ids), 0
        keys = []
        for end in self._id_ends:
            keys.append(bytes(ids[start:end]))
            start = end
        return {key: bytes(text) for key, text in zip(keys, self.texts())}


def published_rows_at_repo(
    parent: Path, published_ref: str, *, repository: str, record: bool = True,
    files: Any = None,
) -> PublishedRows:
    """_published_snapshot_at_repo's rows, read shard by shard into PublishedRows.

    The same rows in the same order, refused for the same reasons with the same
    messages, without ever holding every row as Python objects: on 2026-10-11
    that list was about 8.6 GB of the 1,155,059 published rows, plus a 1.4 GB
    checker string, twice per publication
    (https://github.com/rrnewton/dev-hermit/issues/554). The order of refusals
    is the old one: an unreadable or truncated shard or a malformed line
    first, then the first row with a workspace-local owner path, then the
    shared checker, then the first conflicting event_id in reading order.

    Rows an earlier call validated are not validated again; see "published
    scan vouching" above. ``record=False`` writes no record.
    """
    resolved = _git(parent, [
        "--no-replace-objects", "rev-parse", "--verify", f"{published_ref}^{{commit}}",
    ])
    if resolved.returncode != 0:
        raise SeriesError(
            f"cannot resolve published series revision {published_ref}: "
            + (resolved.stderr.strip() or "no output")
        )
    commit = resolved.stdout.strip()
    if not _SHA_RE.fullmatch(commit):
        raise SeriesError(
            f"published series revision {published_ref} resolved to invalid commit "
            f"{commit!r}"
        )
    owned_files = files is None
    files = file_archive_snapshot(parent, commit, files=files)
    try:
        shards = [
            relative for relative in files.listing(("series",))
            if len(Path(relative).parts) == 4 and Path(relative).parts[0] == "series"
            and relative.endswith(".jsonl")
        ]
        scan = _PublishedScan.open(Path(parent), _SERIES_ROWS_SCAN, commit, shards, write=record)
        return _scanned(lambda scan: _published_rows_read(parent, commit, shards, scan, files=files), scan)
    except BaseException:
        if owned_files and files is not None:
            files.close()
        raise


#: Each published_rows_for_at_repo call, newest last: its commit, "record"
#: (answered from a vouch record, the commit's own or an ancestor's with the
#: same series shards, named in vouched_by, and a read of the wanted rows'
#: month files) or "scan" (published_rows_at_repo), why no record was used,
#: and for "record" how many files it read.
PUBLISHED_ROW_LOOKUPS: collections.deque = collections.deque(maxlen=256)


def published_rows_for_at_repo(
    parent: Path, published_ref: str, wanted: Mapping[str, Iterable[tuple[str, str, str]]], *,
    repository: str, files: Any = None,
) -> dict[str, dict]:
    """A subset of ``published_rows_at_repo(...).rows_for(set(wanted))``; it serves the reap.

    It is exactly that whole read's answer when it reads the whole ledger,
    and otherwise a subset of it, never a different row (see below), which is
    what the publisher's reap view needs: a retained batch is deleted only
    when every one of its events equals its row here, so a missing row keeps
    the batch.

    ``wanted`` maps each event id to the (team, host, month) of every event
    with that id the caller holds: where the publisher places such an event
    (``series/<team>/<host>/`` and the month's files, ``is_month_shard_name``).

    The whole read is skipped only when a published-scan record, under the
    current validation inputs, names exactly the commit's series shards (path,
    object id and size): the commit's own record, or, when it has none, an
    ancestor's whose shard map is exactly the commit's (see
    _series_record_reason). A record is written only after
    a read that validated every row and found no conflicting event id, so the
    whole read of this commit, which cannot change, would return without a
    refusal, and every row of an event id has the same body. Each wanted id
    is then looked up in the month files its events name, the highest part
    first, stopping once every id there is found.

    The answer can hold fewer ids than the whole read's, never different
    rows: an id is missing only when no row of it is in those month files.
    The publisher places a row by its own team, host and emitted_at month and
    publishes it at most once in that month, so a held event whose published
    row has the same body is found unless that row was filed elsewhere (moved
    by hand, or placed by an older rule). Such a batch is kept, not deleted,
    until a reap answered by the whole read: the reap deletes the same
    batches or fewer, never more. One that is unpublished, or published with
    another body, compares unequal either way.
    """
    started = time.monotonic()
    resolved = _git(parent, [
        "--no-replace-objects", "rev-parse", "--verify", f"{published_ref}^{{commit}}",
    ])
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or not _SHA_RE.fullmatch(commit):
        # The whole read reports the failure exactly as it always has.
        return published_rows_at_repo(parent, published_ref, repository=repository).rows_for(
            set(wanted))
    owned_files = files is None
    files = file_archive_snapshot(parent, commit, files=files)
    try:
        shards, reason = _series_shard_listing(parent, commit, files=files)
        if shards is not None:
            # A vouch remembers a verdict about exact original bytes; it does
            # not remember whether the external operands are still available.
            files.verify(shards)
            reason, vouching = _series_record_reason(Path(parent), commit, shards)
        if reason is not None:
            found = published_rows_at_repo(
                parent, commit, repository=repository, files=files).rows_for(set(wanted))
            PUBLISHED_ROW_LOOKUPS.append({"commit": commit, "path": "scan", "reason": reason,
                                          "files": None, "found": len(found),
                                          "seconds": round(time.monotonic() - started, 1)})
            print(f"ledger publication: published rows lookup at {commit[:12]} by full scan "
                  f"({reason}): {len(found)} of {len(wanted)} id(s)", file=sys.stderr)
            return found
        supporting_paths: set[str] = set()
        found, file_count = _rows_from_month_files(
            Path(parent), commit, shards, wanted, files=files, supporting_paths=supporting_paths)
        found = PublishedRowView(found, files, supporting_paths)
        seconds = round(time.monotonic() - started, 1)
        PUBLISHED_ROW_LOOKUPS.append({"commit": commit, "path": "record", "reason": None,
                                      "vouched_by": vouching,
                                      "files": file_count, "found": len(found), "seconds": seconds})
        source = ("its vouch record" if vouching == commit
                  else f"the vouch record of ancestor {vouching[:12]} (the same series shards)")
        print(f"ledger publication: published rows lookup at {commit[:12]} from {source}: "
              f"{len(found)} of {len(wanted)} id(s), {file_count} file(s) read in {seconds} s",
              file=sys.stderr)
        return found
    except BaseException:
        if owned_files and files is not None:
            files.close()
        raise


def _series_shard_listing(parent: Path, commit: str, *, files: Any = None) -> tuple[dict[str, tuple[str, int]] | None,
                                                               str | None]:
    """Every series shard published_rows_at_repo reads at COMMIT: path -> (object id, size)."""
    with file_archive_scope(parent, commit, files=files) as files:
        shards: dict[str, tuple[str, int]] = {}
        for relative, descriptor in files.listing(("series",)).items():
            if not (len(Path(relative).parts) == 4 and Path(relative).parts[0] == "series"
                    and relative.endswith(".jsonl")):
                continue
            if descriptor.mode not in {"100644", "100755"}:
                return None, f"{relative} is not a blob"
            shards[relative] = (descriptor.blob_oid, descriptor.bytes)
        return shards, None


def _series_record_reason(parent: Path, commit: str, shards: Mapping[str, tuple[str, int]],
                          ) -> tuple[str | None, str | None]:
    """(None, the commit whose record vouches) when a series-rows record vouches for
    exactly SHARDS at COMMIT, else (why not, None).

    COMMIT's own record first. When it has none, a record of an ANCESTOR vouches
    only if its shard map equals SHARDS exactly (same paths, object ids and
    sizes): the rows read are a function of those bytes and the inputs, so an
    identical map vouches as the commit's own would, and any append changes it.
    A publication pushes one more commit (quarantine and proofs) on top of the
    last one it scanned, with the same series tree, so the next publication's
    lookup there never finds an own record (2026-10-13: every lookup took the
    full scan, "no record of this commit").
    """
    if os.environ.get(PUBLISHED_SCAN_VOUCH_ENV, "").lower() in {"off", "0", "no"}:
        return "disabled", None
    inputs = _published_scan_inputs(_SERIES_ROWS_SCAN)
    if inputs is None:
        return "the validation inputs cannot be named", None
    located = _git(parent, ["rev-parse", "--path-format=absolute", "--git-common-dir",
                            "--show-object-format"])
    lines = located.stdout.splitlines()
    if located.returncode != 0 or len(lines) != 2 or lines[1] not in _GIT_OBJECT_ID_HEX:
        return "the repository's Git directory cannot be named", None
    directory = Path(lines[0]) / _PUBLISHED_SCAN_DIRECTORY / _SERIES_ROWS_SCAN
    memo = _PUBLISHED_SCAN_MEMO.get((str(directory), _SERIES_ROWS_SCAN), {})
    wanted = dict(shards)

    def load(candidate_commit: str, path: Path | None) -> tuple[Any, str | None]:
        held = memo.get(candidate_commit)
        if held is not None:
            return held[1], None
        try:
            return json.loads((path or directory / f"{candidate_commit}.json").read_bytes()), None
        except FileNotFoundError:
            return None, "no record of this commit"
        except (OSError, ValueError, RecursionError):
            return None, "the record is unreadable"

    def vouches(candidate: Any, candidate_commit: str) -> str | None:
        record, reason = _checked_published_scan_record(
            candidate, kind=_SERIES_ROWS_SCAN, inputs=inputs, object_format=lines[1])
        if record is None:
            return reason
        if record["commit"] != candidate_commit:
            return "the record names another commit than its file"
        if {relative: (entry["oid"], entry["size"])
                for relative, entry in record["shards"].items()} != wanted:
            return "the record's shards are not the commit's"
        return None

    candidate, missing = load(commit, None)
    if candidate is not None:
        reason = vouches(candidate, commit)
        return (None, commit) if reason is None else (reason, None)
    if missing != "no record of this commit":
        return missing, None  # an own record that cannot be read is not absent
    # No own record: an ancestor's whose shard map is exactly this commit's.
    others = {name: (stamp, path) for name, (stamp, path) in _published_scan_files(directory).items()}
    for name, (stamp, _record) in memo.items():
        others.setdefault(name, (stamp, None))
    for other, (_stamp, path) in sorted(others.items(), key=lambda item: item[1][0], reverse=True):
        if other == commit:
            continue
        candidate, _unread = load(other, path)
        if candidate is None or vouches(candidate, other) is not None:
            continue
        ancestry = _git(parent, ["--no-replace-objects", "merge-base", "--is-ancestor", other, commit])
        if ancestry.returncode == 0:
            return None, other
    return missing, None


def _rows_from_month_files(parent: Path, commit: str, shards: Mapping[str, tuple[str, int]],
                           wanted: Mapping[str, Iterable[tuple[str, str, str]]],
                           *, files: Any = None, supporting_paths: set[str] | None = None,
                           ) -> tuple[dict[str, dict], int]:
    """The row of each wanted id found in the month files its events name, and files read."""
    by_month: dict[tuple[str, str, str], set[str]] = {}
    for event_id, routes in wanted.items():
        for team, host, month in routes:
            by_month.setdefault((team, host, month), set()).add(event_id)
    found: dict[str, dict] = {}
    with file_archive_scope(parent, commit, files=files) as snapshot:
        file_count = 0
        for (team, host, month), ids in sorted(by_month.items()):
            directory = f"series/{team}/{host}"
            try:
                month_files = sorted(
                    (relative for relative in shards
                     if relative.rpartition("/")[0] == directory
                     and is_month_shard_name(relative.rpartition("/")[2], month)),
                    reverse=True)  # path order is part order: the highest part first
            except MalformedRow:
                continue  # no month the publisher could place such an event in
            remaining = ids - set(found)
            if not remaining or not month_files:
                continue
            if supporting_paths is not None:
                # The absence of a selected event also depends on the original
                # files searched.  A cached negative answer cannot outlive them.
                supporting_paths.update(month_files)
            with contextlib.closing(snapshot.iter_blobs(month_files)) as blobs:
                for relative, _oid, data in blobs:
                    file_count += 1
                    for line in data.decode().splitlines():
                        if not line.strip():
                            continue
                        row = parse_line(line)
                        event_id = str(row.get("event_id", ""))
                        if event_id in remaining:
                            found[event_id] = row
                            remaining.discard(event_id)
                    if not remaining:
                        break
        return found, file_count


def _published_rows_read(parent: Path, commit: str, shards: Sequence[str],
                         scan: "_PublishedScan", *, files: Any = None) -> PublishedRows:
    """published_rows_at_repo's rows, validating only what SCAN does not vouch for."""
    texts, ends = bytearray(), array.array("Q")
    ids, id_ends = bytearray(), array.array("Q")
    stamps, stamp_ends = bytearray(), array.array("Q")
    checker_input = bytearray()
    checked = 0
    owner_path_row = False
    blobs = (files.iter_blobs(shards) if files is not None
             else _git_blob_bytes(parent, commit, shards))
    with contextlib.closing(blobs) as blobs:
        for relative, oid, data in blobs:
            text = data.decode()
            if text and not text.endswith("\n"):
                raise MalformedEvent(
                    f"parent {commit}:{relative}: final line has no newline terminator"
                )
            size = scan.vouched_prefix(relative, oid, data)
            # Lines numbered below this were validated before (a vouched prefix
            # ends with a newline, so its lines are the text's leading lines).
            vouched_lines = (math.inf if size == len(data)
                             else len(data[:size].decode().splitlines()) if size else 0)
            del data
            for number, line in enumerate(text.splitlines()):
                if not line.strip():
                    continue
                row = parse_line(line)
                if owner_path_row:
                    # Already refused (below); every remaining line is still parsed,
                    # as before, so an unreadable line is still reported first.
                    continue
                if number < vouched_lines:
                    scan.vouched += 1
                else:
                    if _contains_owner_path(row):
                        owner_path_row = True
                        continue
                    checker_input += (_canonical_row_json(row) + "\n").encode()
                    checked += 1
                texts += json.dumps(row, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
                ends.append(len(texts))
                ids += str(row.get("event_id", "")).encode()
                id_ends.append(len(ids))
                stamps += str(row.get("emitted_at", "")).encode()
                stamp_ends.append(len(stamps))
    scan.checked = checked
    if owner_path_row:
        raise MalformedRow("row contains a workspace-local owner path")
    count = len(ends)
    if checked:
        decoded = checker_input.decode()
        del checker_input
        result = _run_shared_checker(decoded, ["--read"])
        del decoded
        if result.returncode != 0:
            if scan.delta:
                # Its message numbers the row among the rows it was given.
                raise _VouchRefused("the shared checker refused a new row")
            detail = result.stderr.strip() or result.stdout.strip() or "no output"
            raise MalformedRow(detail)
    else:
        del checker_input

    def piece(blob: bytearray, offsets: "array.array", index: int) -> bytes:
        return bytes(blob[offsets[index - 1] if index else 0:offsets[index]])

    # union_events: the first body of each event_id is kept; a later identical
    # body collapses; the first later body that differs, in reading order, is
    # refused. ledger.union_events compares json.dumps(event, sort_keys=True);
    # this compares the rows' texts, which are equal exactly when those are.
    # Both encodings write the same tokens for the same row (keys sorted the
    # same way, the same number and literal spellings) and differ only in the
    # separators between tokens and in how a string's characters are escaped,
    # and each string encoding decodes back to its string. So either text
    # determines the row's value and the value determines either text. (A row
    # whose text cannot be UTF-8 encoded, a lone surrogate, still raises where
    # its text is built.) The texts are already held: comparing them saves a
    # second dumps and a SHA-256 per row, about 12.7 us, or 20 of the 83 s a
    # vouched read of 1.6 million rows took on 2026-10-13.
    #
    # Each event id is made into bytes once, here, and used for the map and
    # the final sort; sorting every row by id and walking that order made each
    # one three times, and dropping them saved 3.5 more of those seconds (63
    # to 59.5 s) and 0.2 GB of peak memory. Walking the rows in
    # reading order with a map from each id to its first row meets every
    # repeat after its first body, so the first repeat that differs is the
    # first in reading order, the smallest differing index the sort found.
    view, start = memoryview(ids), 0
    keys: list[bytes] = []
    for end in id_ends:
        keys.append(bytes(view[start:end]))
        start = end
    view.release()
    first_of: dict[bytes, int] = {}
    for index, key in enumerate(keys):
        first = first_of.setdefault(key, index)
        if first != index and piece(texts, ends, index) != piece(texts, ends, first):
            raise ConflictingEventId(
                f"event_id {key.decode()!r} appears twice with different bodies; "
                f"refusing last-writer-wins (§5.2)"
            )
    kept = list(first_of.values())
    del first_of
    kept.sort(key=lambda index: (piece(stamps, stamp_ends, index), keys[index]))
    out_texts, out_ends = bytearray(), array.array("Q")
    out_ids, out_id_ends = bytearray(), array.array("Q")
    newest = None
    view = memoryview(texts)
    for index in kept:
        out_texts += view[ends[index - 1] if index else 0:ends[index]]
        out_ends.append(len(out_texts))
        out_ids += keys[index]
        out_id_ends.append(len(out_ids))
    view.release()
    if kept:
        newest = piece(stamps, stamp_ends, kept[-1]).decode()
    del texts, ids, stamps, kept, keys
    scan.vouch()
    result = PublishedRows._from_parts(out_texts, out_ends, out_ids, out_id_ends, newest)
    return result.bind_files(files, shards) if files is not None else result


def _git_blobs(
    checkout: Path, commit: str, paths: Sequence[str], *, idle_timeout: float = 60,
) -> Iterator[tuple[str, str]]:
    """Yield ``(path, text)`` for each path of ``commit``, in order, from one git.

    One ``git cat-file --batch`` process serves every path, one request at a
    time, where a ``git show`` per path cost a process start each: with 5159
    published series shards (2026-10-07) those starts were most of the 150 s
    a published snapshot took, and a mirror run reads several snapshots
    within the backlog publisher's 600 s bound. A path that is missing, or
    that is not a file, raises ``SeriesError`` as ``git show`` failing did.
    git is killed if a single read makes no progress for ``idle_timeout``
    seconds, the bound each ``git show`` had.
    """
    with contextlib.closing(
            _git_blob_bytes(checkout, commit, paths, idle_timeout=idle_timeout)) as blobs:
        for relative, _oid, data in blobs:
            yield relative, data.decode()


def _git_blob_bytes(
    checkout: Path, commit: str, paths: Sequence[str], *, idle_timeout: float = 60,
) -> Generator[tuple[str, str, bytes], None, None]:
    """``_git_blobs``, yielding ``(path, blob object id, bytes)``."""
    if not paths:
        return
    env = dict(os.environ)
    for name in _GIT_REPO_SCOPED_ENV:
        env.pop(name, None)
    process = subprocess.Popen(
        ["git", "-C", str(checkout), "--no-replace-objects", "cat-file", "--batch"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    assert process.stdin is not None and process.stdout is not None
    progress = [time.monotonic()]
    finished = threading.Event()

    def watchdog() -> None:
        while not finished.wait(1):
            if time.monotonic() - progress[0] > idle_timeout:
                process.kill()
                return

    threading.Thread(target=watchdog, name="git-cat-file-watchdog", daemon=True).start()
    try:
        for relative in paths:
            if "\n" in relative:
                raise SeriesError(f"cannot read parent {commit}:{relative!r}: newline in path")
            progress[0] = time.monotonic()
            process.stdin.write(f"{commit}:{relative}\n".encode())
            process.stdin.flush()
            header = process.stdout.readline().decode(errors="replace").rstrip("\n")
            fields = header.split(" ")
            if len(fields) != 3 or fields[1] != "blob" or not fields[2].isdigit():
                raise SeriesError(
                    f"cannot read parent {commit}:{relative}: "
                    + (header or "git cat-file ended without output")
                )
            size = int(fields[2])
            body = process.stdout.read(size + 1)
            if len(body) != size + 1 or body[-1:] != b"\n":
                raise SeriesError(
                    f"cannot read parent {commit}:{relative}: short read from git cat-file"
                )
            yield relative, fields[0], body[:-1]
    finally:
        finished.set()
        process.stdin.close()
        if process.poll() is None:
            process.kill()
        process.wait()
        process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()


def published_rows(parent: Any) -> list[dict]:
    """Compatibility wrapper returning rows from the current published snapshot."""
    snapshot = published_snapshot(parent)
    try:
        snapshot.verify()
        return snapshot.rows
    finally:
        snapshot.close()


def published_rows_at(
    parent: Any, published_ref: str, *, repository: str = PARENT_REPOSITORY,
) -> list[dict]:
    """Compatibility wrapper returning rows from one resolved revision."""
    snapshot = published_snapshot_at(parent, published_ref, repository=repository)
    try:
        snapshot.verify()
        return snapshot.rows
    finally:
        snapshot.close()


def history_store_rows(parent: Any) -> list[dict]:
    """Published parent rows plus this checkout's unpublished and retained batches."""
    spool = spool_dir(parent)
    local = sorted(spool.glob("*.jsonl")) + sorted((spool / "published").glob("*.jsonl"))
    return union_events([*published_rows(parent), *union(local)])


def _series_tree_at(parent: Path, commit: str) -> str:
    """Resolve the exact tree containing the published series at ``commit``."""
    resolved = _git(parent, [
        "--no-replace-objects",
        "rev-parse",
        "--verify",
        f"{commit}:{SCORECARD_SNAPSHOT_SOURCE}",
    ])
    tree = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA_RE.fullmatch(tree) is None:
        detail = resolved.stderr.strip() or resolved.stdout.strip() or "no output"
        raise SeriesError(
            f"cannot resolve published series tree at {commit}: {detail}"
        )
    object_type = _git(parent, ["--no-replace-objects", "cat-file", "-t", tree])
    if object_type.returncode != 0 or object_type.stdout.strip() != "tree":
        raise SeriesError(
            f"published series object {tree} at {commit} is not a tree"
        )
    return tree


def _canonical_rows_bytes(rows: Sequence[dict]) -> bytes:
    return "".join(
        json.dumps(row, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        + "\n"
        for row in rows
    ).encode()


def _stat_signature(status: os.stat_result) -> tuple[int, ...]:
    """Fields that must remain stable while immutable evidence is consumed."""
    return (
        status.st_dev,
        status.st_ino,
        status.st_mode,
        status.st_nlink,
        status.st_size,
        status.st_mtime_ns,
        status.st_ctime_ns,
    )


def _read_held_fd(descriptor: int) -> bytes:
    """Read from an already-bound descriptor without reopening its pathname."""
    chunks = []
    offset = 0
    while True:
        chunk = os.pread(descriptor, 1024 * 1024, offset)
        if not chunk:
            return b"".join(chunks)
        chunks.append(chunk)
        offset += len(chunk)


def _lstat_at(directory_fd: int, name: str, display: str) -> os.stat_result:
    try:
        return os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
    except OSError as error:
        raise SeriesError(
            f"series snapshot path changed or is unavailable: {display}: {error}"
        ) from error


@dataclass
class _HeldSnapshotSource:
    """One source whose descriptor and directory entry stay bound until publish."""

    kind: str
    relative: str
    directory_fd: int
    name: str
    descriptor: int
    signature: tuple[int, ...]
    sha256: str
    size: int
    rows: list[dict]

    def verify(self) -> None:
        display = self.relative
        current = os.fstat(self.descriptor)
        if (
            not stat.S_ISREG(current.st_mode)
            or _stat_signature(current) != self.signature
        ):
            raise SeriesError(f"series snapshot source changed while held open: {display}")
        current_path = _lstat_at(self.directory_fd, self.name, display)
        if (
            not stat.S_ISREG(current_path.st_mode)
            or _stat_signature(current_path) != self.signature
        ):
            raise SeriesError(
                f"series snapshot source pathname changed while held open: {display}"
            )
        if hashlib.sha256(_read_held_fd(self.descriptor)).hexdigest() != self.sha256:
            raise SeriesError(
                f"series snapshot source content changed while held open: {display}"
            )

    def close(self) -> None:
        os.close(self.descriptor)


def _open_snapshot_source(
    directory_fd: int, name: str, *, kind: str, relative: str,
) -> _HeldSnapshotSource:
    """Open and parse one source through its held parent directory descriptor."""
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NONBLOCK
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(name, flags, dir_fd=directory_fd)
    except OSError as error:
        raise SeriesError(
            f"cannot open regular series snapshot source {relative}: {error}"
        ) from error
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            raise SeriesError(f"series snapshot source is not a regular file: {relative}")
        path_status = _lstat_at(directory_fd, name, relative)
        if (
            not stat.S_ISREG(path_status.st_mode)
            or (path_status.st_dev, path_status.st_ino) != (before.st_dev, before.st_ino)
        ):
            raise SeriesError(
                f"series snapshot source pathname does not name its opened file: {relative}"
            )
        payload = _read_held_fd(descriptor)
        signature = _stat_signature(before)
        after = os.fstat(descriptor)
        if _stat_signature(after) != signature:
            raise SeriesError(f"series snapshot source changed while being read: {relative}")
        digest = hashlib.sha256(payload).hexdigest()
        if payload and not payload.endswith(b"\n"):
            raise MalformedEvent(
                f"{relative}: final line has no newline terminator — the batch is truncated"
            )
        try:
            text = payload.decode("utf-8")
        except UnicodeDecodeError as error:
            raise MalformedEvent(f"{relative}: batch is not UTF-8: {error}") from error
        rows = validate_read_rows(
            [parse_line(line) for line in text.splitlines() if line.strip()]
        )
        source = _HeldSnapshotSource(
            kind=kind,
            relative=relative,
            directory_fd=directory_fd,
            name=name,
            descriptor=descriptor,
            signature=signature,
            sha256=digest,
            size=len(payload),
            rows=rows,
        )
        source.verify()
        return source
    except Exception:
        os.close(descriptor)
        raise


def _bounded_jsonl_names(
    directory_fd: int,
    *,
    display: str,
    deadline: float,
    entries_seen: int,
    context: str,
) -> tuple[list[str], int]:
    """Stream one directory under the snapshot's total entry/time budget."""
    names = []
    try:
        iterator = os.scandir(directory_fd)
    except OSError as error:
        raise SeriesError(
            f"cannot enumerate scorecard snapshot sources in {display}: {error}"
        ) from error
    try:
        while True:
            if time.monotonic() > deadline:
                raise SeriesError(
                    "scorecard source identity phase exceeded "
                    f"{SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS:.3f}s {context}"
                )
            try:
                entry = next(iterator)
            except StopIteration:
                break
            entries_seen += 1
            if entries_seen > SCORECARD_SNAPSHOT_MAX_DIRECTORY_ENTRIES:
                raise SeriesError(
                    "scorecard source identity phase inspected "
                    f"{entries_seen} total directory entries, exceeding the explicit "
                    f"maximum of {SCORECARD_SNAPSHOT_MAX_DIRECTORY_ENTRIES} {context}"
                )
            if time.monotonic() > deadline:
                raise SeriesError(
                    "scorecard source identity phase exceeded "
                    f"{SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS:.3f}s {context}"
                )
            if entry.name.endswith(".jsonl"):
                names.append(entry.name)
    except OSError as error:
        raise SeriesError(
            f"cannot enumerate scorecard snapshot sources in {display}: {error}"
        ) from error
    finally:
        iterator.close()
    names.sort()
    return names, entries_seen


def _bounded_snapshot_source_keys(
    spool_fd: int,
    published_fd: int | None,
    *,
    spool: Path,
    deadline: float,
    context: str,
) -> list[tuple[str, str]]:
    """Return deterministic source keys without first materializing all names."""
    live_names, entries_seen = _bounded_jsonl_names(
        spool_fd,
        display=str(spool),
        deadline=deadline,
        entries_seen=0,
        context=context,
    )
    published_names = []
    if published_fd is not None:
        published_names, entries_seen = _bounded_jsonl_names(
            published_fd,
            display=str(spool / "published"),
            deadline=deadline,
            entries_seen=entries_seen,
            context=context,
        )
    source_keys = [("live", name) for name in live_names]
    source_keys.extend(
        ("published-retained", f"published/{name}") for name in published_names
    )
    return source_keys


@dataclass
class _SeriesSpoolLock:
    """One owner for the series spool lock and its bound directory inode."""

    spool: Path
    spool_fd: int
    spool_identity: tuple[int, int]
    lock_fd: int
    lock_signature: tuple[int, ...]

    def verify(self) -> None:
        held = os.fstat(self.spool_fd)
        try:
            named = os.lstat(self.spool)
        except OSError as error:
            raise SeriesError(f"series spool changed while lock was held: {error}") from error
        identity = (held.st_dev, held.st_ino)
        if (
            not stat.S_ISDIR(held.st_mode)
            or not stat.S_ISDIR(named.st_mode)
            or identity != self.spool_identity
            or (named.st_dev, named.st_ino) != self.spool_identity
        ):
            raise SeriesError("series spool pathname changed while lock was held")

        held_lock = os.fstat(self.lock_fd)
        named_lock = _lstat_at(
            self.spool_fd, ".append.lock", str(self.spool / ".append.lock")
        )
        if (
            not stat.S_ISREG(held_lock.st_mode)
            or not stat.S_ISREG(named_lock.st_mode)
            or _stat_signature(held_lock) != self.lock_signature
            or _stat_signature(named_lock) != self.lock_signature
        ):
            raise SeriesError("series snapshot writer lock changed while held")

    def close(self) -> None:
        try:
            fcntl.flock(self.lock_fd, fcntl.LOCK_UN)
        finally:
            os.close(self.lock_fd)
            os.close(self.spool_fd)


@contextlib.contextmanager
def series_spool_transition_lock(
    spool: Any,
    *,
    exclusive: bool,
    timeout_seconds: float = SERIES_SPOOL_LOCK_TIMEOUT_SECONDS,
    poll_seconds: float = 0.05,
):
    """Serialize series writers/publishers or hold a stable reader frontier.

    This is the sole lock owner. Callers that already hold it must use their
    ``*_locked`` helper instead of recursively opening the same lock pathname.
    """
    if (
        not math.isfinite(timeout_seconds)
        or timeout_seconds < 0
        or not math.isfinite(poll_seconds)
        or poll_seconds <= 0
    ):
        raise SeriesError(
            "series spool lock timeout must be finite and nonnegative and poll "
            "interval must be finite and positive"
        )
    spool = Path(spool)
    try:
        spool.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise SeriesError(
            f"cannot create series spool for lock {spool / '.append.lock'}: {error}"
        ) from error
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        directory_flags |= os.O_NOFOLLOW
    try:
        spool_fd = os.open(spool, directory_flags)
    except OSError as error:
        raise SeriesError(
            f"cannot open regular series spool directory {spool}: {error}"
        ) from error
    lock_fd = -1
    held: _SeriesSpoolLock | None = None
    try:
        spool_status = os.fstat(spool_fd)
        named_spool = os.lstat(spool)
        if (
            not stat.S_ISDIR(spool_status.st_mode)
            or not stat.S_ISDIR(named_spool.st_mode)
            or (spool_status.st_dev, spool_status.st_ino)
            != (named_spool.st_dev, named_spool.st_ino)
        ):
            raise SeriesError(
                f"series spool pathname does not name its opened directory: {spool}"
            )

        lock_flags = os.O_RDWR | os.O_CREAT | os.O_CLOEXEC
        if hasattr(os, "O_NOFOLLOW"):
            lock_flags |= os.O_NOFOLLOW
        try:
            lock_fd = os.open(
                ".append.lock", lock_flags, 0o644, dir_fd=spool_fd,
            )
        except OSError as error:
            raise SeriesError(
                f"cannot open series spool lock {spool / '.append.lock'}: {error}"
            ) from error
        lock_status = os.fstat(lock_fd)
        if not stat.S_ISREG(lock_status.st_mode):
            raise SeriesError(
                f"series snapshot lock is not a regular file: {spool / '.append.lock'}"
            )
        mode = fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH
        deadline = time.monotonic() + timeout_seconds
        while True:
            try:
                fcntl.flock(lock_fd, mode | fcntl.LOCK_NB)
                break
            except BlockingIOError as error:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    kind = "exclusive" if exclusive else "shared"
                    raise SeriesError(
                        f"timed out after {timeout_seconds:.3f}s "
                        f"acquiring {kind} series spool lock {spool / '.append.lock'}"
                    ) from error
                time.sleep(min(poll_seconds, remaining))
            except OSError as error:
                raise SeriesError(
                    f"cannot acquire series spool lock {spool / '.append.lock'}: {error}"
                ) from error
        held = _SeriesSpoolLock(
            spool=spool,
            spool_fd=spool_fd,
            spool_identity=(spool_status.st_dev, spool_status.st_ino),
            lock_fd=lock_fd,
            lock_signature=_stat_signature(lock_status),
        )
        held.verify()
        yield held
        held.verify()
    finally:
        if held is not None:
            held.close()
        else:
            if lock_fd >= 0:
                try:
                    fcntl.flock(lock_fd, fcntl.LOCK_UN)
                finally:
                    os.close(lock_fd)
            os.close(spool_fd)


@dataclass
class _LocalSnapshotCapture:
    """Held local frontier and every source inode through snapshot readback."""

    spool: Path
    spool_fd: int
    spool_identity: tuple[int, int]
    published_fd: int | None
    published_signature: tuple[int, ...] | None
    source_keys: list[tuple[str, str]]
    sources: list[_HeldSnapshotSource]
    published_verify: Callable[[], None] | None = field(default=None, repr=False, compare=False)

    def _verify_directory(self) -> None:
        held = os.fstat(self.spool_fd)
        try:
            named = os.lstat(self.spool)
        except OSError as error:
            raise SeriesError(
                f"series spool changed while snapshot was held: {error}"
            ) from error
        identity = (held.st_dev, held.st_ino)
        if (
            not stat.S_ISDIR(held.st_mode)
            or not stat.S_ISDIR(named.st_mode)
            or identity != self.spool_identity
            or (named.st_dev, named.st_ino) != self.spool_identity
        ):
            raise SeriesError("series spool pathname changed while snapshot was held")

        if self.published_fd is None:
            try:
                os.stat(
                    "published",
                    dir_fd=self.spool_fd,
                    follow_symlinks=False,
                )
            except FileNotFoundError:
                return
            except OSError as error:
                raise SeriesError(
                    f"retained series directory changed while snapshot was held: {error}"
                ) from error
            raise SeriesError("retained series directory appeared while snapshot was held")

        held_published = os.fstat(self.published_fd)
        named_published = _lstat_at(
            self.spool_fd,
            "published",
            str(self.spool / "published"),
        )
        if (
            not stat.S_ISDIR(held_published.st_mode)
            or not stat.S_ISDIR(named_published.st_mode)
            or _stat_signature(held_published) != self.published_signature
            or _stat_signature(named_published) != self.published_signature
        ):
            raise SeriesError("retained series directory changed while snapshot was held")

    def identity_generation(self) -> tuple[Any, ...]:
        """Exact source set and inode metadata, without rereading source bytes."""
        self._verify_directory()
        deadline = time.monotonic() + SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS
        current = _bounded_snapshot_source_keys(
            self.spool_fd,
            self.published_fd,
            spool=self.spool,
            deadline=deadline,
            context=f"while verifying held sources in {self.spool}",
        )
        if current != self.source_keys:
            raise SeriesError("series spool changed while its projection snapshot was held")
        identities = []
        for source in self.sources:
            held = os.fstat(source.descriptor)
            named = _lstat_at(source.directory_fd, source.name, source.relative)
            if (
                not stat.S_ISREG(held.st_mode)
                or not stat.S_ISREG(named.st_mode)
                or _stat_signature(held) != source.signature
                or _stat_signature(named) != source.signature
            ):
                raise SeriesError(
                    f"series snapshot source pathname changed while held open: "
                    f"{source.relative}"
                )
            identities.append(
                (source.kind, source.relative, source.signature)
            )
        return (
            self.spool_identity,
            self.published_signature,
            tuple(identities),
        )

    def verify(self) -> None:
        if self.published_verify is not None:
            self.published_verify()
        self.identity_generation()
        for source in self.sources:
            source.verify()

    def close(self) -> None:
        for source in reversed(self.sources):
            source.close()
        if self.published_fd is not None:
            os.close(self.published_fd)
        os.close(self.spool_fd)


@contextlib.contextmanager
def _local_snapshot_capture(parent: Path):
    """Bind local sources independently of the cooperative writer lock."""
    spool = spool_dir(parent)
    spool.mkdir(parents=True, exist_ok=True)
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        directory_flags |= os.O_NOFOLLOW
    try:
        spool_fd = os.open(spool, directory_flags)
    except OSError as error:
        raise SeriesError(
            f"cannot open regular series spool directory {spool}: {error}"
        ) from error
    published_fd: int | None = None
    sources: list[_HeldSnapshotSource] = []
    capture: _LocalSnapshotCapture | None = None
    try:
        spool_status = os.fstat(spool_fd)
        named_spool = os.lstat(spool)
        if (
            not stat.S_ISDIR(spool_status.st_mode)
            or not stat.S_ISDIR(named_spool.st_mode)
            or (spool_status.st_dev, spool_status.st_ino)
            != (named_spool.st_dev, named_spool.st_ino)
        ):
            raise SeriesError(
                f"series spool pathname does not name its opened directory: {spool}"
            )
        try:
            published_fd = os.open("published", directory_flags, dir_fd=spool_fd)
        except FileNotFoundError:
            published_fd = None
        except OSError as error:
            raise SeriesError(
                f"cannot open regular retained series directory {spool / 'published'}: {error}"
            ) from error

        deadline = time.monotonic() + SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS
        source_keys = _bounded_snapshot_source_keys(
            spool_fd,
            published_fd,
            spool=spool,
            deadline=deadline,
            context=f"while binding held sources in {spool}",
        )
        capture = _LocalSnapshotCapture(
            spool=spool,
            spool_fd=spool_fd,
            spool_identity=(spool_status.st_dev, spool_status.st_ino),
            published_fd=published_fd,
            published_signature=(
                _stat_signature(os.fstat(published_fd))
                if published_fd is not None else None
            ),
            source_keys=source_keys,
            sources=sources,
        )
        for kind, relative in source_keys:
            if kind == "live":
                directory_fd, name = spool_fd, relative
            else:
                assert published_fd is not None
                directory_fd, name = published_fd, Path(relative).name
            sources.append(
                _open_snapshot_source(
                    directory_fd, name, kind=kind, relative=relative,
                )
            )
        capture.verify()
        yield capture
    finally:
        if capture is not None:
            capture.close()
        else:
            for source in reversed(sources):
                source.close()
            if published_fd is not None:
                os.close(published_fd)
            os.close(spool_fd)


def _snapshot_source_generation(lock: _SeriesSpoolLock) -> tuple[Any, ...]:
    """Measure names and inode metadata within one bounded shared-lock phase."""
    started = time.monotonic()
    deadline = started + SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS
    lock_path = lock.spool / ".append.lock"

    def check_deadline() -> None:
        if time.monotonic() > deadline:
            raise SeriesError(
                "scorecard source identity phase exceeded "
                f"{SCORECARD_SNAPSHOT_IDENTITY_TIMEOUT_SECONDS:.3f}s while holding "
                f"shared series spool lock {lock_path}"
            )

    lock.verify()
    check_deadline()
    spool_signature = _stat_signature(os.fstat(lock.spool_fd))
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        directory_flags |= os.O_NOFOLLOW
    published_fd: int | None = None
    published_signature: tuple[int, ...] | None = None
    try:
        try:
            published_status = os.stat(
                "published", dir_fd=lock.spool_fd, follow_symlinks=False,
            )
        except FileNotFoundError:
            published_status = None
        except OSError as error:
            raise SeriesError(
                f"cannot inspect retained series directory {lock.spool / 'published'}: "
                f"{error}"
            ) from error
        if published_status is not None:
            if not stat.S_ISDIR(published_status.st_mode):
                raise SeriesError(
                    f"retained series path is not a directory: {lock.spool / 'published'}"
                )
            try:
                published_fd = os.open(
                    "published", directory_flags, dir_fd=lock.spool_fd,
                )
            except OSError as error:
                raise SeriesError(
                    f"cannot open regular retained series directory "
                    f"{lock.spool / 'published'}: {error}"
                ) from error
            opened = os.fstat(published_fd)
            if _stat_signature(opened) != _stat_signature(published_status):
                raise SeriesError(
                    "retained series directory changed while its identity was measured"
                )
            published_signature = _stat_signature(opened)

        source_keys = _bounded_snapshot_source_keys(
            lock.spool_fd,
            published_fd,
            spool=lock.spool,
            deadline=deadline,
            context=f"while holding shared series spool lock {lock_path}",
        )

        identities = []
        for kind, relative in source_keys:
            if kind == "live":
                directory_fd, name = lock.spool_fd, relative
            else:
                assert published_fd is not None
                directory_fd, name = published_fd, Path(relative).name
            status = _lstat_at(directory_fd, name, relative)
            if not stat.S_ISREG(status.st_mode):
                raise SeriesError(
                    f"series snapshot source is not a regular file: {relative}"
                )
            identities.append((kind, relative, _stat_signature(status)))
            check_deadline()

        for kind, relative, expected in identities:
            if kind == "live":
                directory_fd, name = lock.spool_fd, relative
            else:
                assert published_fd is not None
                directory_fd, name = published_fd, Path(relative).name
            status = _lstat_at(directory_fd, name, relative)
            if not stat.S_ISREG(status.st_mode) or _stat_signature(status) != expected:
                raise SeriesError(
                    f"series snapshot source changed while its identity was measured: "
                    f"{relative}"
                )
            check_deadline()
        held_spool = os.fstat(lock.spool_fd)
        try:
            named_spool = os.lstat(lock.spool)
        except OSError as error:
            raise SeriesError(
                f"series spool changed while its identity was measured: {error}"
            ) from error
        if (
            _stat_signature(held_spool) != spool_signature
            or _stat_signature(named_spool) != spool_signature
        ):
            raise SeriesError(
                "series source set changed while its identity was measured"
            )
        if published_fd is not None:
            held = os.fstat(published_fd)
            named = _lstat_at(
                lock.spool_fd, "published", str(lock.spool / "published")
            )
            if (
                _stat_signature(held) != published_signature
                or _stat_signature(named) != published_signature
            ):
                raise SeriesError(
                    "retained series directory changed while its identity was measured"
                )
        lock.verify()
        check_deadline()
        return (
            lock.spool_identity,
            published_signature,
            tuple(identities),
        )
    finally:
        if published_fd is not None:
            os.close(published_fd)


def _origin_main_commit(parent: Path) -> str:
    """Bind the exact fetched origin/main commit of the supplied repository."""
    origin_ref = "refs/remotes/origin/main"
    resolved = _git(parent, [
        "--no-replace-objects", "rev-parse", "--verify", f"{origin_ref}^{{commit}}",
    ])
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA_RE.fullmatch(commit) is None:
        detail = resolved.stderr.strip() or resolved.stdout.strip() or "no output"
        raise SeriesError(f"cannot bind ledger origin/main for scorecard snapshot: {detail}")
    return commit


@contextlib.contextmanager
def _scorecard_projection_capture(parent: Any):
    """Build a snapshot while retaining every mutable source descriptor."""
    parent = Path(parent).resolve()
    repository = ledger_root(parent)
    # Both network refreshes run outside the bounded writer lock. The snapshot
    # is usable only when the exact published ref and the independently
    # recomputed local source generation stay unchanged across capture.
    refresh_origin_main(repository, "cell ledger")
    published_commit = _origin_main_commit(repository)
    with contextlib.ExitStack() as held_sources:
        published = _published_snapshot_at_repo(
            repository, published_commit, repository=LEDGER_REPOSITORY,
        )
        held_sources.callback(published.close)
        source_tree = _series_tree_at(repository, published.commit)
        with series_spool_transition_lock(
            spool_dir(parent), exclusive=False,
        ) as lock:
            before = _snapshot_source_generation(lock)

        local = held_sources.enter_context(_local_snapshot_capture(parent))
        local.published_verify = published.verify
        captured = local.identity_generation()

        with series_spool_transition_lock(
            spool_dir(parent), exclusive=False,
        ) as lock:
            after = _snapshot_source_generation(lock)
            if before != captured or captured != after:
                raise SeriesError(
                    "series source generation changed during scorecard snapshot capture"
                )

        refresh_origin_main(repository, "cell ledger")
        refreshed_commit = _origin_main_commit(repository)
        if refreshed_commit != published_commit:
            raise SeriesError(
                "ledger origin/main changed during scorecard snapshot capture: "
                f"{published_commit} -> {refreshed_commit}"
            )

        with series_spool_transition_lock(
            spool_dir(parent), exclusive=False,
        ) as lock:
            final_generation = _snapshot_source_generation(lock)
            if final_generation != captured:
                raise SeriesError(
                    "series source generation changed after published-ref verification"
                )
        local.verify()

        local_rows = [row for source in local.sources for row in source.rows]
        try:
            rows = union_events([*published.rows, *local_rows])
        except LedgerError as error:
            raise SeriesError(f"cannot form canonical series snapshot: {error}") from error
        rows_bytes = _canonical_rows_bytes(rows)
        snapshot = {
            "schema": SCORECARD_SNAPSHOT_SCHEMA,
            "source": {
                "repository": LEDGER_REPOSITORY,
                "path": SCORECARD_SNAPSHOT_SOURCE,
                "commit": published.commit,
                "tree": source_tree,
                "published_rows": len(published.rows),
                "local_sources": [
                    {
                        "kind": source.kind,
                        "path": source.relative,
                        "device": source.signature[0],
                        "inode": source.signature[1],
                        "rows": len(source.rows),
                        "size": source.size,
                        "sha256": source.sha256,
                    }
                    for source in local.sources
                ],
            },
            "rows_read": len(rows),
            "rows_sha256": hashlib.sha256(rows_bytes).hexdigest(),
            "rows": rows,
        }
        local.verify()
        yield snapshot, local


def scorecard_projection_snapshot(parent: Any) -> dict[str, Any]:
    """Build one immutable published+local series input for scorecard write-back.

    Failure is terminal for this operation.  In particular, an unavailable
    published revision never falls back to the tracked files in the caller's
    checkout, which may be arbitrarily stale.
    """
    with _scorecard_projection_capture(parent) as (snapshot, local):
        local.verify()
        return snapshot


def _snapshot_bytes(snapshot: Mapping[str, Any]) -> bytes:
    return (
        json.dumps(snapshot, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        + "\n"
    ).encode()


def _validate_snapshot_payload(
    payload: bytes, *, expected_sha256: str,
) -> dict[str, Any]:
    """Validate exact snapshot bytes after a caller binds their inode."""
    actual_sha256 = hashlib.sha256(payload).hexdigest()
    if actual_sha256 != expected_sha256:
        raise SeriesError(
            "scorecard snapshot digest mismatch: "
            f"expected {expected_sha256}, got {actual_sha256}"
        )
    def unique_fields(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        fields: dict[str, Any] = {}
        for key, value in pairs:
            if key in fields:
                raise SeriesError(f"scorecard snapshot has duplicate JSON field {key!r}")
            fields[key] = value
        return fields

    try:
        snapshot = json.loads(payload, object_pairs_hook=unique_fields)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SeriesError(f"scorecard snapshot is not valid JSON: {error}") from error
    if not isinstance(snapshot, dict) or set(snapshot) != {
        "schema", "source", "rows_read", "rows_sha256", "rows",
    }:
        raise SeriesError("scorecard snapshot has an invalid top-level shape")
    source = snapshot.get("source")
    required_source = {"path", "commit", "tree", "published_rows", "local_sources"}
    if not isinstance(source, dict) or set(source) not in (
        required_source, required_source | {"repository"},
    ):
        raise SeriesError("scorecard snapshot has an invalid source identity")
    if not _is_str_in(source.get("repository", PARENT_REPOSITORY), {
        PARENT_REPOSITORY, LEDGER_REPOSITORY,
    }):
        raise SeriesError("scorecard snapshot names an unsupported repository")
    if (
        snapshot.get("schema") != SCORECARD_SNAPSHOT_SCHEMA
        or source.get("path") != SCORECARD_SNAPSHOT_SOURCE
        or _SHA_RE.fullmatch(str(source.get("commit", ""))) is None
        or _SHA_RE.fullmatch(str(source.get("tree", ""))) is None
        or type(source.get("published_rows")) is not int
        or source["published_rows"] < 0
    ):
        raise SeriesError("scorecard snapshot has an invalid source identity")
    local_sources = source.get("local_sources")
    if not isinstance(local_sources, list):
        raise SeriesError("scorecard snapshot local_sources must be an array")
    seen_paths = set()
    ordered_sources = []
    for item in local_sources:
        if not isinstance(item, dict) or set(item) != {
            "kind", "path", "device", "inode", "rows", "size", "sha256",
        }:
            raise SeriesError("scorecard snapshot has an invalid local source")
        relative = item.get("path")
        if (
            not _is_str_in(item.get("kind"), {"live", "published-retained"})
            or not isinstance(relative, str)
            or not relative
            or Path(relative).is_absolute()
            or ".." in Path(relative).parts
            or relative in seen_paths
            or type(item.get("device")) is not int
            or item["device"] < 0
            or type(item.get("inode")) is not int
            or item["inode"] <= 0
            or type(item.get("rows")) is not int
            or item["rows"] < 0
            or type(item.get("size")) is not int
            or item["size"] < 0
            or re.fullmatch(r"[0-9a-f]{64}", str(item.get("sha256", ""))) is None
        ):
            raise SeriesError("scorecard snapshot has an invalid local source")
        if item["kind"] == "live" and len(Path(relative).parts) != 1:
            raise SeriesError("scorecard snapshot live source has an invalid path")
        if item["kind"] == "published-retained" and (
            len(Path(relative).parts) != 2
            or Path(relative).parts[0] != "published"
        ):
            raise SeriesError(
                "scorecard snapshot published-retained source has an invalid path"
            )
        seen_paths.add(relative)
        ordered_sources.append((item["kind"], relative))
    source_order = {"live": 0, "published-retained": 1}
    if ordered_sources != sorted(
        ordered_sources, key=lambda item: (source_order[item[0]], item[1])
    ):
        raise SeriesError("scorecard snapshot local sources are not in canonical order")
    rows = snapshot.get("rows")
    if not isinstance(rows, list):
        raise SeriesError("scorecard snapshot rows must be an array")
    checked = validate_read_rows(rows)
    canonical = union_events(checked)
    if canonical != rows:
        raise SeriesError("scorecard snapshot rows are not a unique canonical event order")
    if type(snapshot.get("rows_read")) is not int or snapshot["rows_read"] != len(rows):
        raise SeriesError("scorecard snapshot rows_read does not match its rows")
    if source["published_rows"] > len(rows):
        raise SeriesError("scorecard snapshot published_rows exceeds its canonical rows")
    rows_sha256 = hashlib.sha256(_canonical_rows_bytes(rows)).hexdigest()
    if snapshot.get("rows_sha256") != rows_sha256:
        raise SeriesError("scorecard snapshot row digest does not match its rows")
    return snapshot


def _verify_named_descriptor(
    directory_fd: int,
    name: str,
    descriptor: int,
    signature: tuple[int, ...],
    display: str,
) -> None:
    current = os.fstat(descriptor)
    named = _lstat_at(directory_fd, name, display)
    if (
        not stat.S_ISREG(current.st_mode)
        or not stat.S_ISREG(named.st_mode)
        or _stat_signature(current) != signature
        or _stat_signature(named) != signature
    ):
        raise SeriesError(f"scorecard snapshot pathname changed while held open: {display}")


def _read_scorecard_projection_snapshot_at(
    directory_fd: int,
    name: str,
    *,
    display: str,
    expected_sha256: str,
) -> dict[str, Any]:
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NONBLOCK
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(name, flags, dir_fd=directory_fd)
    except OSError as error:
        raise SeriesError(
            f"scorecard snapshot is not a regular file: {display}: {error}"
        ) from error
    try:
        status = os.fstat(descriptor)
        if not stat.S_ISREG(status.st_mode):
            raise SeriesError(f"scorecard snapshot is not a regular file: {display}")
        signature = _stat_signature(status)
        _verify_named_descriptor(directory_fd, name, descriptor, signature, display)
        payload = _read_held_fd(descriptor)
        _verify_named_descriptor(directory_fd, name, descriptor, signature, display)
        snapshot = _validate_snapshot_payload(
            payload, expected_sha256=expected_sha256,
        )
        _verify_named_descriptor(directory_fd, name, descriptor, signature, display)
        return snapshot
    finally:
        os.close(descriptor)


def _directory_identity(path: Path, descriptor: int) -> tuple[int, int]:
    held = os.fstat(descriptor)
    try:
        named = os.lstat(path)
    except OSError as error:
        raise SeriesError(f"scorecard snapshot directory changed: {path}: {error}") from error
    if (
        not stat.S_ISDIR(held.st_mode)
        or not stat.S_ISDIR(named.st_mode)
        or (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino)
    ):
        raise SeriesError(f"scorecard snapshot directory pathname changed: {path}")
    return held.st_dev, held.st_ino


def read_scorecard_projection_snapshot(
    path: Any, *, expected_sha256: str
) -> dict[str, Any]:
    """Read one exact snapshot from a held no-follow descriptor."""
    path = Path(path)
    if re.fullmatch(r"[0-9a-f]{64}", expected_sha256 or "") is None:
        raise SeriesError("scorecard snapshot expected_sha256 must be 64 lowercase hex")
    if not path.name:
        raise SeriesError(f"scorecard snapshot path has no filename: {path}")
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        directory_flags |= os.O_NOFOLLOW
    try:
        directory_fd = os.open(path.parent, directory_flags)
    except OSError as error:
        raise SeriesError(
            f"scorecard snapshot parent is not a regular directory: {path.parent}: {error}"
        ) from error
    try:
        identity = _directory_identity(path.parent, directory_fd)
        snapshot = _read_scorecard_projection_snapshot_at(
            directory_fd,
            path.name,
            display=str(path),
            expected_sha256=expected_sha256,
        )
        if _directory_identity(path.parent, directory_fd) != identity:
            raise SeriesError(f"scorecard snapshot directory changed: {path.parent}")
        return snapshot
    finally:
        os.close(directory_fd)


def _write_all(descriptor: int, payload: bytes) -> None:
    offset = 0
    while offset < len(payload):
        written = os.write(descriptor, payload[offset:])
        if written <= 0:
            raise SeriesError("scorecard snapshot staging write made no progress")
        offset += written


def _open_anonymous_snapshot_stage(directory_fd: int) -> int:
    if not hasattr(os, "O_TMPFILE"):
        raise OSError(errno.EOPNOTSUPP, "O_TMPFILE is unavailable")
    return os.open(
        ".", os.O_RDWR | os.O_CLOEXEC | os.O_TMPFILE, 0o600, dir_fd=directory_fd,
    )


def _open_named_snapshot_stage(directory_fd: int, output_name: str) -> tuple[int, str]:
    flags = os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    while True:
        name = f".{output_name}.staging-{os.getpid()}-{secrets.token_hex(16)}"
        try:
            return os.open(name, flags, 0o600, dir_fd=directory_fd), name
        except FileExistsError:
            continue


def _link_anonymous_noreplace(
    descriptor: int, directory_fd: int, output_name: str,
) -> None:
    """Publish an O_TMPFILE inode with linkat(AT_EMPTY_PATH), never replacing."""
    libc = ctypes.CDLL(None, use_errno=True)
    linkat = getattr(libc, "linkat", None)
    if linkat is None:
        raise OSError(errno.ENOSYS, "linkat is unavailable")
    result = linkat(
        ctypes.c_int(descriptor),
        ctypes.c_char_p(b""),
        ctypes.c_int(directory_fd),
        ctypes.c_char_p(os.fsencode(output_name)),
        ctypes.c_int(0x1000),  # AT_EMPTY_PATH
    )
    if result != 0:
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number), output_name)


def _rename_noreplace(
    directory_fd: int, source_name: str, output_name: str,
) -> None:
    """Move a named stage into place atomically without replacing an output."""
    libc = ctypes.CDLL(None, use_errno=True)
    renameat2 = getattr(libc, "renameat2", None)
    if renameat2 is None:
        raise OSError(errno.ENOSYS, "renameat2 is unavailable")
    result = renameat2(
        ctypes.c_int(directory_fd),
        ctypes.c_char_p(os.fsencode(source_name)),
        ctypes.c_int(directory_fd),
        ctypes.c_char_p(os.fsencode(output_name)),
        ctypes.c_uint(1),  # RENAME_NOREPLACE
    )
    if result != 0:
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number), output_name)


def _validate_staged_snapshot(
    descriptor: int,
    payload: bytes,
    digest: str,
    *,
    directory_fd: int,
    stage_name: str | None,
    display: str,
) -> tuple[int, ...]:
    _write_all(descriptor, payload)
    os.fsync(descriptor)
    status = os.fstat(descriptor)
    if not stat.S_ISREG(status.st_mode):
        raise SeriesError("scorecard snapshot staging descriptor is not a regular file")
    signature = _stat_signature(status)
    if stage_name is not None:
        _verify_named_descriptor(
            directory_fd, stage_name, descriptor, signature, display,
        )
    readback = _read_held_fd(descriptor)
    if _stat_signature(os.fstat(descriptor)) != signature:
        raise SeriesError("scorecard snapshot staging file changed during readback")
    _validate_snapshot_payload(readback, expected_sha256=digest)
    if stage_name is not None:
        _verify_named_descriptor(
            directory_fd, stage_name, descriptor, signature, display,
        )
    return signature


def _existing_snapshot(
    directory_fd: int, output_name: str, output: Path, digest: str,
) -> bool:
    try:
        status = os.stat(output_name, dir_fd=directory_fd, follow_symlinks=False)
    except FileNotFoundError:
        return False
    except OSError as error:
        raise SeriesError(f"cannot inspect scorecard snapshot output {output}: {error}") from error
    if not stat.S_ISREG(status.st_mode):
        raise SeriesError(
            f"scorecard snapshot output already exists with different content: {output}"
        )
    try:
        _read_scorecard_projection_snapshot_at(
            directory_fd,
            output_name,
            display=str(output),
            expected_sha256=digest,
        )
    except SeriesError as error:
        raise SeriesError(
            f"scorecard snapshot output already exists with different content: {output}: {error}"
        ) from error
    return True


def _finish_snapshot_publication(
    directory_fd: int, output: Path, digest: str,
) -> dict[str, Any]:
    os.fsync(directory_fd)
    return _read_scorecard_projection_snapshot_at(
        directory_fd,
        output.name,
        display=str(output),
        expected_sha256=digest,
    )


def _publish_snapshot_payload(
    directory_fd: int, output: Path, payload: bytes, digest: str,
) -> dict[str, Any]:
    """Publish exact bytes without any failure-path unlink by pathname."""
    if _existing_snapshot(directory_fd, output.name, output, digest):
        return _finish_snapshot_publication(directory_fd, output, digest)

    unsupported = {
        errno.EINVAL, errno.EISDIR, errno.ENOENT, errno.ENOSYS,
        errno.EOPNOTSUPP, errno.EPERM,
    }
    anonymous_fd: int | None = None
    try:
        try:
            anonymous_fd = _open_anonymous_snapshot_stage(directory_fd)
        except OSError as error:
            if error.errno not in unsupported:
                raise SeriesError(
                    f"cannot create anonymous scorecard snapshot stage: {error}"
                ) from error
        else:
            _validate_staged_snapshot(
                anonymous_fd,
                payload,
                digest,
                directory_fd=directory_fd,
                stage_name=None,
                display="anonymous scorecard snapshot stage",
            )
            try:
                _link_anonymous_noreplace(
                    anonymous_fd, directory_fd, output.name,
                )
            except FileExistsError:
                if not _existing_snapshot(directory_fd, output.name, output, digest):
                    raise AssertionError("existing snapshot disappeared during inspection")
                return _finish_snapshot_publication(directory_fd, output, digest)
            except OSError as error:
                if error.errno not in unsupported:
                    raise
            else:
                return _finish_snapshot_publication(directory_fd, output, digest)
    finally:
        if anonymous_fd is not None:
            os.close(anonymous_fd)

    stage_fd, stage_name = _open_named_snapshot_stage(directory_fd, output.name)
    stage_moved = False
    try:
        signature = _validate_staged_snapshot(
            stage_fd,
            payload,
            digest,
            directory_fd=directory_fd,
            stage_name=stage_name,
            display=str(output.parent / stage_name),
        )
        # Make the fallback's quarantine name durable before any operation
        # that might have to preserve it for inspection.
        os.fsync(directory_fd)
        _verify_named_descriptor(
            directory_fd,
            stage_name,
            stage_fd,
            signature,
            str(output.parent / stage_name),
        )
        try:
            _rename_noreplace(directory_fd, stage_name, output.name)
            stage_moved = True
        except FileExistsError:
            if not _existing_snapshot(directory_fd, output.name, output, digest):
                raise AssertionError("existing snapshot disappeared during inspection")
            raise SeriesError(
                "an identical scorecard snapshot appeared while a named staging "
                "file was held; refusing success because the staging pathname "
                "cannot be removed by inode"
            )
        except OSError as error:
            if error.errno != errno.ENOSYS:
                raise
            try:
                os.link(
                    stage_name,
                    output.name,
                    src_dir_fd=directory_fd,
                    dst_dir_fd=directory_fd,
                    follow_symlinks=False,
                )
            except FileExistsError:
                if not _existing_snapshot(directory_fd, output.name, output, digest):
                    raise AssertionError("existing snapshot disappeared during inspection")
                raise SeriesError(
                    "an identical scorecard snapshot appeared while the no-replace "
                    "move was unavailable; refusing success with named staging residue"
                )
            os.fsync(directory_fd)
            raise SeriesError(
                "scorecard snapshot was linked without replacement, but the atomic "
                "move primitive is unavailable; refusing success with named staging "
                "residue"
            )
        return _finish_snapshot_publication(directory_fd, output, digest)
    except Exception as error:
        if not stage_moved:
            raise SeriesError(
                f"{error}; named staging pathname was left in place: "
                f"{output.parent / stage_name}"
            ) from error
        raise
    finally:
        os.close(stage_fd)


def write_scorecard_projection_snapshot(parent: Any, output: Any) -> dict[str, Any]:
    """Atomically publish one run-scoped snapshot without replacing evidence."""
    output = Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        directory_flags |= os.O_NOFOLLOW
    try:
        directory_fd = os.open(output.parent, directory_flags)
    except OSError as error:
        raise SeriesError(
            f"scorecard snapshot parent is not a regular directory: {output.parent}: {error}"
        ) from error
    try:
        directory_identity = _directory_identity(output.parent, directory_fd)
        with _scorecard_projection_capture(parent) as (snapshot, local):
            payload = _snapshot_bytes(snapshot)
            digest = hashlib.sha256(payload).hexdigest()
            local.verify()
            _publish_snapshot_payload(directory_fd, output, payload, digest)
            local.verify()
            if _directory_identity(output.parent, directory_fd) != directory_identity:
                raise SeriesError(
                    f"scorecard snapshot directory changed during publication: {output.parent}"
                )
            return {
                "state": "available",
                "path": str(output.absolute()),
                "sha256": digest,
                "source_commit": snapshot["source"]["commit"],
                "source_tree": snapshot["source"]["tree"],
                "rows_read": snapshot["rows_read"],
            }
    finally:
        os.close(directory_fd)


def test_id(row: dict) -> str | None:
    """The test part of the existing ``<test>/<mode>/<backend>`` cell id."""
    cell = str((row.get("series") or {}).get("cell", ""))
    parts = cell.rsplit("/", 2)
    return parts[0] if len(parts) == 3 else None


RETIRED_IDS_FILE = "tests/e2e/manifests/inventory/retired-ids.json"
RETIRED_IDS_SCHEMA = 1
_RETIRED_IDS_FIELDS = frozenset({"schema", "description", "retirements"})
_RETIREMENT_FIELDS = frozenset({
    "retired_bucket", "successor_bucket", "last_live_commit", "reason", "ids", "collisions",
})
_RETIRED_TEST_NAME_RE = re.compile(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?")


def _retired_test_name(test: str, bucket: str) -> str | None:
    """``name`` of ``bucket/name``, with Hermit's ``retired_ids::test_name`` rule."""
    prefix = bucket + "/"
    if not test.startswith(prefix):
        return None
    name = test[len(prefix):]
    return name if _RETIRED_TEST_NAME_RE.fullmatch(name) else None


def _retired_string(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise SeriesError(f"retired-id map {label} must be a non-empty string")
    return value


def _retired_string_map(value: object, label: str) -> dict[str, str]:
    if not isinstance(value, dict) or any(
        not isinstance(key, str) or not isinstance(item, str) for key, item in value.items()
    ):
        raise SeriesError(f"retired-id map {label} must map strings to strings")
    return value


def parse_retired_ids(text: str) -> dict[str, str]:
    """Every retired test id and its successor, from Hermit's ``retired-ids.json``.

    When Hermit folds one manifest bucket into another, a program keeps its
    history but changes its test id (the backend-parity-c bucket became
    c-programs, https://github.com/rrnewton/hermit/issues/3301). Hermit records
    each old id and the id that replaced it in :data:`RETIRED_IDS_FILE`, and
    its own reader, ``hermit_manifest_plan::retired_ids::RetiredIds::parse``,
    decides what a valid map is. This reader accepts only the maps that one
    accepts, so the two never resolve one id differently: exactly the schema-1
    fields; non-empty text fields; a full lowercase ``last_live_commit``; every
    retired id a test of its retired bucket and every successor a test of its
    successor bucket; each successor the plain bucket-prefix rename unless the
    old id is listed, with a reason, in ``collisions``; no id retired twice; no
    successor shared by two ids; and no successor that is itself retired.
    Anything else raises :class:`SeriesError`.
    """
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise SeriesError(f"invalid retired-id map: {error}") from error
    if not isinstance(document, dict) or set(document) != _RETIRED_IDS_FIELDS:
        raise SeriesError(
            "retired-id map must hold exactly schema, description and retirements"
        )
    schema = document["schema"]
    if type(schema) is not int or schema != RETIRED_IDS_SCHEMA:
        raise SeriesError(
            f"retired-id map schema must be {RETIRED_IDS_SCHEMA}, found {schema!r}"
        )
    _retired_string(document["description"], "description")
    retirements = document["retirements"]
    if not isinstance(retirements, list) or not retirements:
        raise SeriesError("retired-id map has no retirement")
    successors: dict[str, str] = {}
    shared: set[str] = set()
    buckets: set[str] = set()
    for index, retirement in enumerate(retirements):
        if not isinstance(retirement, dict) or set(retirement) != _RETIREMENT_FIELDS:
            raise SeriesError(
                f"retired-id map retirement {index} must hold exactly "
                + ", ".join(sorted(_RETIREMENT_FIELDS))
            )
        bucket = _retired_string(retirement["retired_bucket"], "retired_bucket")
        successor_bucket = _retired_string(
            retirement["successor_bucket"], "successor_bucket"
        )
        if bucket == successor_bucket:
            raise SeriesError(f"retirement of {bucket} names itself as successor")
        if bucket in buckets:
            raise SeriesError(f"bucket {bucket} is retired twice")
        buckets.add(bucket)
        last_live = retirement["last_live_commit"]
        if not isinstance(last_live, str) or _SHA_RE.fullmatch(last_live) is None:
            raise SeriesError(
                f"retirement of {bucket} needs a full lowercase last_live_commit SHA"
            )
        _retired_string(retirement["reason"], f"retirement of {bucket} reason")
        ids = _retired_string_map(retirement["ids"], f"retirement of {bucket} ids")
        collisions = _retired_string_map(
            retirement["collisions"], f"retirement of {bucket} collisions"
        )
        if not ids:
            raise SeriesError(f"retirement of {bucket} retires no id")
        for old, reason in collisions.items():
            if old not in ids:
                raise SeriesError(
                    f"collision {old!r} is not a retired id of bucket {bucket}"
                )
            _retired_string(reason, f"collision {old!r} reason")
        for old, new in ids.items():
            old_name = _retired_test_name(old, bucket)
            if old_name is None:
                raise SeriesError(f"retired id {old!r} is not a test of bucket {bucket}")
            new_name = _retired_test_name(new, successor_bucket)
            if new_name is None:
                raise SeriesError(
                    f"successor {new!r} of {old!r} is not a test of bucket {successor_bucket}"
                )
            listed = old in collisions
            if old_name == new_name and listed:
                raise SeriesError(
                    f"collision {old!r} maps to the plain prefix rename {new!r}"
                )
            if old_name != new_name and not listed:
                raise SeriesError(
                    f"successor {new!r} of {old!r} is neither the prefix rename "
                    f"{successor_bucket}/{old_name} nor a listed collision"
                )
            if old in successors:
                raise SeriesError(f"retired id {old!r} is listed twice")
            if new in shared:
                raise SeriesError(f"successor {new!r} is shared by two retired ids")
            successors[old] = new
            shared.add(new)
    chained = sorted(shared.intersection(successors))
    if chained:
        raise SeriesError(
            f"{chained[0]!r} is both a retired id and a successor; "
            "record the final id directly"
        )
    return successors


def retired_ids_at(checkout: Path, ref: str) -> tuple[str, dict[str, str]] | None:
    """``(commit, map)``: Hermit's retired-id map as committed at ``ref``.

    ``ref`` is resolved once to a commit and the file is read from that
    immutable commit, never from the working tree. ``None`` means no map
    applies and every test id is joined exactly as recorded, which is what
    every reader did before the map existed: ``checkout`` is not a checkout,
    ``ref`` does not resolve, or the commit predates the file. A file that is
    present but unreadable or malformed raises :class:`SeriesError` rather than
    silently dropping the joins it would have made.
    """
    checkout = Path(checkout)
    if not _is_checkout(checkout):
        return None
    resolved = _git(checkout, ["rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"])
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA_RE.fullmatch(commit) is None:
        return None
    listed = _git(checkout, ["ls-tree", "-z", commit, "--", RETIRED_IDS_FILE])
    if listed.returncode != 0:
        detail = listed.stderr.strip() or "no output"
        raise SeriesError(f"cannot list {RETIRED_IDS_FILE} at Hermit {commit}: {detail}")
    entries = [entry for entry in listed.stdout.split("\0") if entry]
    if not entries:
        return None
    meta, _, path = entries[0].partition("\t")
    fields = meta.split()
    if len(entries) != 1 or path != RETIRED_IDS_FILE or len(fields) != 3 or fields[1] != "blob":
        raise SeriesError(f"{RETIRED_IDS_FILE} at Hermit {commit} is not one file")
    env = dict(os.environ)
    for name in _GIT_REPO_SCOPED_ENV:
        env.pop(name, None)
    shown = subprocess.run(
        ["git", "-C", str(checkout), "cat-file", "blob", fields[2]],
        capture_output=True, timeout=60, check=False, env=env,
    )
    if shown.returncode != 0:
        detail = shown.stderr.decode(errors="replace").strip() or "no output"
        raise SeriesError(f"cannot read {RETIRED_IDS_FILE} at Hermit {commit}: {detail}")
    try:
        text = shown.stdout.decode("utf-8")
    except UnicodeDecodeError as error:
        raise SeriesError(f"{RETIRED_IDS_FILE} at Hermit {commit} is not UTF-8") from error
    try:
        return commit, parse_retired_ids(text)
    except SeriesError as error:
        raise SeriesError(f"{RETIRED_IDS_FILE} at Hermit {commit}: {error}") from error


def resolve_test_id(test: str, retired: Mapping[str, str] | None) -> str:
    """The live test id that carries ``test``'s history: its successor if retired."""
    return retired.get(test, test) if retired else test


def resolve_cell(cell: str, retired: Mapping[str, str] | None) -> str:
    """``<test>/<mode>/<backend>`` with a retired ``test`` replaced by its successor."""
    parts = cell.rsplit("/", 2)
    if len(parts) != 3:
        return cell
    return "/".join([resolve_test_id(parts[0], retired), parts[1], parts[2]])


def history_rows(
    rows: Iterable[dict],
    test: str,
    tree: str | None = None,
    retired: Mapping[str, str] | None = None,
) -> list[dict]:
    """All rows for one test, ordered as they were emitted.

    Without ``retired`` this is one exact test id. With Hermit's retired-id map
    (:func:`retired_ids_at`), a row recorded under a retired id and a row
    recorded under its successor belong to one history, whichever of the two
    ``test`` names. Each row keeps its recorded ``series.cell``.
    """
    wanted = resolve_test_id(test, retired)
    selected = [
        row for row in rows
        if (recorded := test_id(row)) is not None
        and resolve_test_id(recorded, retired) == wanted
        and (tree is None or (row.get("series") or {}).get("tree") == tree)
    ]
    return sorted(
        selected,
        key=lambda row: (
            str(row.get("emitted_at", "")),
            str(row.get("run_id", "")),
            int((row.get("series") or {}).get("run_index", -1)),
            int((row.get("series") or {}).get("attempt", 1)),
            str(row.get("event_id", "")),
        ),
    )


def _git(
    checkout: Path, args: Sequence[str], *, timeout: float = 60,
) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ)
    for name in _GIT_REPO_SCOPED_ENV:
        env.pop(name, None)
    return subprocess.run(
        ["git", "-C", str(checkout), *args],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
        env=env,
    )


def _is_checkout(checkout: Path) -> bool:
    """True only when git resolves *this directory* as the checkout root.

    An uninitialised submodule is an empty directory. ``git -C`` otherwise
    walks upward and would silently report the parent dev-hermit commit as the
    Hermit or Reverie commit.
    """
    if not checkout.is_dir():
        return False
    top = _git(checkout, ["rev-parse", "--show-toplevel"])
    if top.returncode != 0:
        return False
    try:
        return Path(top.stdout.strip()).resolve() == checkout.resolve()
    except OSError:
        return False


def _corpus_tests(document: dict, corpus: Any, path: str, tree: str) -> list[dict]:
    """The tests a manifest's ``corpus:`` section expands into.

    Hermit's ``ci/manifest-plan/src/manifest_corpus.rs`` expands each row into
    one test with id ``<bucket>/<row id, or its label>``, appended after any
    hand-written ``test`` entries. A row carries only ``label``, ``id`` and
    ``argv`` (the section denies unknown keys), so it declares no
    ``expected_guest_exit``. The section's lane-wide ``verify.comparator``
    and ``verify.comparator_reason`` are copied verbatim into every expanded
    test's ``modes.verify``, as ``manifest_corpus.rs`` copies them; nothing
    else of the recipe is needed here.

    Each entry of the section's ``variants`` adds one more test per row whose
    label none of its ``except`` groups names, with id
    ``<bucket>/<id_prefix><row id, or its label>``, after every row's own
    test. A ``verify`` variant (the default ``mode``) shares the corpus's
    comparator, so its test declares it as well; a ``replay`` variant's test
    has no verify cell and declares none.
    """
    bucket = document.get("bucket")
    rows = corpus.get("rows") if isinstance(corpus, dict) else None
    if not isinstance(bucket, str) or not bucket or not isinstance(rows, list) or not rows:
        raise SeriesError(f"{path} at Hermit {tree} has a malformed corpus section")
    lane_verify = corpus.get("verify")
    verify = {
        key: lane_verify[key]
        for key in ("comparator", "comparator_reason")
        if isinstance(lane_verify, dict) and key in lane_verify
    }
    tests = []
    for row in rows:
        suffix = row.get("id", row.get("label")) if isinstance(row, dict) else None
        if not isinstance(suffix, str) or not suffix:
            raise SeriesError(f"{path} at Hermit {tree} has a corpus row without a label")
        test: dict = {"id": f"{bucket}/{suffix}"}
        if verify:
            test["modes"] = {"verify": dict(verify)}
        tests.append(test)
    variants = corpus.get("variants", [])
    if not isinstance(variants, list):
        raise SeriesError(f"{path} at Hermit {tree} has a malformed corpus variants list")
    for variant in variants:
        prefix = variant.get("id_prefix") if isinstance(variant, dict) else None
        mode = variant.get("mode", "verify") if isinstance(variant, dict) else None
        groups = variant.get("except", []) if isinstance(variant, dict) else None
        if (not isinstance(prefix, str) or not prefix or mode not in ("verify", "replay")
                or not isinstance(groups, list)
                or not all(isinstance(group, dict) and isinstance(group.get("rows"), list)
                           for group in groups)):
            raise SeriesError(f"{path} at Hermit {tree} has a malformed corpus variant")
        excepted = {label for group in groups for label in group["rows"]}
        for row in rows:
            if row.get("label") in excepted:
                continue
            test = {"id": f"{bucket}/{prefix}{row.get('id', row.get('label'))}"}
            if verify and mode == "verify":
                test["modes"] = {"verify": dict(verify)}
            tests.append(test)
    return tests


def main_ancestry(checkout: Path, trees: Iterable[str]) -> dict[str, tuple[bool | None, dict | None]]:
    """Resolve current ``origin/main`` ancestry and depth for each requested SHA.

    ``None`` is deliberately distinct from ``False``: an unavailable object or
    checkout did not establish that a tree is off main.
    """
    requested = sorted(set(trees))
    if not requested:
        return {}
    if not _is_checkout(checkout):
        return {tree: (None, None) for tree in requested}

    main = _git(checkout, ["rev-parse", "--verify", "origin/main^{commit}"])
    if main.returncode != 0:
        return {tree: (None, None) for tree in requested}

    resolved: dict[str, tuple[bool | None, dict | None]] = {}
    for tree in requested:
        if _SHA_RE.fullmatch(tree) is None:
            resolved[tree] = (None, None)
            continue
        present = _git(checkout, ["cat-file", "-e", f"{tree}^{{commit}}"])
        if present.returncode != 0:
            resolved[tree] = (None, None)
            continue
        ancestor = _git(checkout, ["merge-base", "--is-ancestor", tree, "origin/main"])
        if ancestor.returncode == 1:
            resolved[tree] = (False, None)
            continue
        if ancestor.returncode != 0:
            resolved[tree] = (None, None)
            continue
        resolved[tree] = (True, commit_depth(checkout, tree))
    return resolved


def commit_depth(checkout: Path, tree: str) -> dict | None:
    """The scorecard's existing ``{commits, first_parent}`` depth shape."""
    counts = {}
    for key, arguments in (
        ("commits", ["rev-list", "--count", tree]),
        ("first_parent", ["rev-list", "--count", "--first-parent", tree]),
    ):
        result = _git(checkout, arguments)
        value = result.stdout.strip()
        if result.returncode != 0 or not value.isdigit() or int(value) < 1:
            return None
        counts[key] = int(value)
    return counts


def source_depths(
    parent: Any, tree: str, *, hermit_checkout: Any | None = None,
) -> dict[str, dict]:
    """The scorecard's per-repository depths at write time."""
    parent = Path(parent)
    hermit = Path(hermit_checkout) if hermit_checkout is not None else parent / "hermit"
    depth = commit_depth(hermit, tree) if _is_checkout(hermit) else None
    if depth is None:
        raise SeriesError(f"cannot resolve Hermit source depth for {tree}")
    depths = {"hermit": depth}

    # Match scorecard.rs: Reverie is a sibling checkout whose presence depends
    # on the surrounding workspace, so omit it when it is unavailable rather
    # than inventing a depth. Its HEAD is the source state available at write
    # time; it is not a path inside the Hermit commit.
    reverie = parent / "reverie"
    if _is_checkout(reverie):
        reverie_depth = commit_depth(reverie, "HEAD")
        if reverie_depth is not None:
            depths["reverie"] = reverie_depth
    return depths


_COMPARISON_MODES = frozenset({"verify", "replay", "chaos"})
_KNOWN_CELL_MODES = _COMPARISON_MODES | {"naked", "custom"}


def _cell_mode(row: dict) -> str:
    mode = row.get("mode")
    if not isinstance(mode, str) or not mode:
        raise MalformedRow("cell result has no nonempty mode")
    if mode not in _KNOWN_CELL_MODES:
        raise MalformedRow(f"cell result has unknown mode {mode!r}")
    return mode


def _has_nonzero_process_disposition(attempt: dict) -> bool:
    status = attempt.get("status")
    signal = attempt.get("signal")
    return (
        isinstance(status, int)
        and not isinstance(status, bool)
        and status > 0
        and signal is None
    ) or (
        status is None
        and isinstance(signal, int)
        and not isinstance(signal, bool)
        and signal > 0
    )


def _validate_container_failed_report(raw: str, attempt: dict, index: int) -> None:
    """Match the shared report's closed container-disposition contract."""
    def unique_fields(pairs):
        fields = {}
        for name, value in pairs:
            if name in fields:
                raise MalformedRow(
                    f"cell attempt {index} container_failed report has duplicate field {name!r}"
                )
            fields[name] = value
        return fields

    def invalid_constant(value):
        raise MalformedRow(
            f"cell attempt {index} container_failed report has non-JSON constant {value}"
        )

    report = json.loads(raw, object_pairs_hook=unique_fields,
                        parse_constant=invalid_constant)
    required = {
        "verified", "bitwise_parity", "verdict", "infrastructure_error",
        "comparison", "no_result_reason", "compared_log_messages",
        "guest_exit_code", "guest_signal", "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds", "first_divergent_record",
        "first_divergent_syscall", "first_divergent_left_message",
        "first_divergent_right_message",
    }
    if not required <= report.keys():
        raise MalformedRow(
            f"cell attempt {index} container_failed report omits current producer fields"
        )
    reason = report["no_result_reason"]
    if (
        not isinstance(reason, dict)
        or set(reason) != {"kind", "run", "disposition"}
        or reason["kind"] != "container_failed"
        or not isinstance(reason["run"], str)
        or reason["run"] not in {"run1", "run2"}
    ):
        raise MalformedRow(f"cell attempt {index} has invalid container_failed reason")
    disposition = reason["disposition"]
    valid = False
    if isinstance(disposition, dict):
        if set(disposition) == {"kind", "code"} and disposition["kind"] == "exited":
            code = disposition["code"]
            valid = (
                type(code) is int and 0 <= code <= 255
                and code not in {122, 124} and not 129 <= code <= 192
            )
        elif (
            set(disposition) == {"kind", "signal", "core_dumped"}
            and disposition["kind"] == "signaled"
        ):
            signal = disposition["signal"]
            valid = (
                type(signal) is int and 1 <= signal <= 64
                and type(disposition["core_dumped"]) is bool
            )
    if not valid:
        raise MalformedRow(f"cell attempt {index} has invalid container_failed disposition")
    if (
        any(report.get(field) is not None for field in (
            "guest_exit_code", "guest_signal", "compared_outputs", "runtime",
            "dbt_counted_branches",
        ))
        or attempt.get("outcome") != "FAIL"
        or type(attempt.get("status")) is not int
        or attempt["status"] != 125
        or attempt.get("signal") is not None
        or attempt.get("timed_out") is not False
        or attempt.get("error_kind") is not None
    ):
        raise MalformedRow(
            f"cell attempt {index} container_failed disposition is inconsistent"
        )


# Hermit's ``runner::STRIPPED_COMPARATOR_RELAXATION_PREFIX``.
STRIPPED_COMPARATOR_RELAXATION_PREFIX = "comparator=stripped: "
# The relaxation ``runner::cell_relaxations`` records for a cell whose recipe
# sets ``compare_io_buffers: false`` (it passes ``--no-detlog-io-buffers``), or
# whose ``hermit_args`` carry that flag. A declared stripped cell carrying it
# is refused: see :func:`_refuse_stripped_io_buffers_off`.
NO_DETLOG_IO_BUFFERS_RELAXATION_PREFIX = "--no-detlog-io-buffers: "
# The characters Rust's ``str::trim`` removes: Unicode White_Space. Python's
# ``str.strip()`` also removes U+001C..U+001F, which Rust keeps, so a reason
# made only of those is blank to Python and substantive to Hermit.
_RUST_WHITE_SPACE = (
    "\u0009\u000a\u000b\u000c\u000d\u0020\u0085\u00a0\u1680"
    "\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a"
    "\u2028\u2029\u202f\u205f\u3000"
)


def _rust_trim(text: str) -> str:
    """``text.trim()`` as Rust computes it."""
    return text.strip(_RUST_WHITE_SPACE)


def _stripped_comparator_reason(row: dict) -> str | None:
    """The reason of the stripped comparator a cell result records, or None.

    A Hermit manifest may declare ``comparator: stripped`` for a verify cell:
    the cell's verdict is then Hermit's default ``--verify`` comparison (exit
    status, output streams and stripped logs), below canonical bitwise
    parity. The runner records the declaration as the relaxation
    ``comparator=stripped: <comparator_reason>`` and applies it only in verify
    mode. This mirrors ``runner::records_stripped_comparator`` and
    ``cell_verdict::declares_stripped_comparator``: a verify row with exactly
    one relaxation carrying the prefix and a reason that is not blank by
    Rust's ``str::trim``. The reason is returned verbatim, as the runner
    copied it from the manifest. The relaxations are part of
    :func:`_cell_result_evidence_sha256`, so the declaration is bound to the
    row's evidence identity, and :func:`_bind_stripped_comparator` binds it
    to the manifest of the row's tree.
    """
    if row.get("mode") != "verify":
        return None
    relaxations = row.get("relaxations")
    if not isinstance(relaxations, list):
        return None
    prefix = STRIPPED_COMPARATOR_RELAXATION_PREFIX
    reasons = [
        relaxation[len(prefix):]
        for relaxation in relaxations
        if isinstance(relaxation, str)
        and relaxation.startswith(prefix)
        and _rust_trim(relaxation[len(prefix):])
    ]
    return reasons[0] if len(reasons) == 1 else None


def _declares_stripped_comparator(row: dict) -> bool:
    """Whether a cell result records Hermit's declared stripped comparator.

    See :func:`_stripped_comparator_reason`.
    """
    return _stripped_comparator_reason(row) is not None


def _bind_stripped_comparator(
    row: dict, tree: str,
    declarations: Callable[[], dict[str, str | None]],
) -> None:
    """Refuse a row whose stripped comparator its tree's manifest does not declare.

    The stripped comparator turns a non-bitwise match from a refusal into a
    pass, so the row's relaxation alone is not authority for it, exactly as
    for a declared guest exit (:func:`_bind_expected_guest_exit`). The
    manifest entry named by the row's ``test`` at the row's Hermit ``tree``
    must declare ``comparator: stripped`` with a ``comparator_reason`` equal
    to the recorded reason: the runner copies that reason verbatim. Any
    difference means the row was not produced under this tree's manifest;
    that includes a row from a dirty working tree whose uncommitted manifest
    declared it. Such a row is refused as :class:`MalformedRow` (quarantined,
    never admitted). An undeclared row never reads the manifests.
    """
    reason = _stripped_comparator_reason(row)
    if reason is None:
        return
    test = row.get("test")
    if not isinstance(test, str) or not test:
        raise MalformedRow("a declared stripped comparator needs the row's manifest test id")
    manifests = declarations()
    if test not in manifests:
        raise MalformedRow(
            f"comparator stripped is recorded for {test!r}, which has no manifest "
            f"entry at Hermit {tree}"
        )
    authority = manifests[test]
    if authority is None:
        raise MalformedRow(
            f"comparator stripped is recorded for {test!r}, whose manifest entry at "
            f"Hermit {tree} declares no stripped comparator"
        )
    if reason != authority:
        raise MalformedRow(
            f"comparator stripped is recorded for {test!r} with a reason that differs "
            f"from its manifest entry's comparator_reason at Hermit {tree}"
        )


def _records_io_buffers_relaxation(relaxations: Any) -> bool:
    """Whether a row's ``relaxations`` record ``--no-detlog-io-buffers``."""
    return isinstance(relaxations, list) and any(
        isinstance(relaxation, str)
        and relaxation.startswith(NO_DETLOG_IO_BUFFERS_RELAXATION_PREFIX)
        for relaxation in relaxations
    )


def _refuse_stripped_io_buffers_off(
    comparison: dict, *, relaxations: Any, index: int, noun: str,
) -> None:
    """Refuse a declared stripped report from a cell with I/O buffers uncompared.

    Hermit's runner passes ``--no-detlog-io-buffers`` when the cell's recipe
    sets ``compare_io_buffers: false``, records the relaxation
    ``--no-detlog-io-buffers: <reason>`` and writes the report with
    ``compare_io_buffers`` false. Nothing here binds that relaxation to the
    manifest of the row's tree, as :func:`_bind_stripped_comparator` binds
    the comparator reason, so the row alone would be its authority. At
    Hermit 853c25fb2ca9 no manifest declares a stripped cell with
    ``compare_io_buffers: false`` (``compat.yaml`` declares the comparator
    for its whole corpus and never sets it), so either signal, the report
    field or the row relaxation, is refused rather than trusted. Admitting
    such a cell needs a manifest binding first.
    """
    if (
        comparison.get("compare_io_buffers") is False
        or _records_io_buffers_relaxation(relaxations)
    ):
        raise MalformedRow(
            f"cell attempt {index} {noun} report is internally inconsistent: its cell "
            "declares the stripped comparator with I/O buffers not compared "
            "(--no-detlog-io-buffers), which is not bound to its manifest and is "
            "refused"
        )


def _is_canonical_comparison(comparison: dict, *, mode: str) -> bool:
    """Whether ``comparison`` is the canonical comparison of a ``mode`` cell.

    Only a matched or diverged report with exactly this comparison is a
    canonical verdict (:func:`_typed_comparison_attempt`). Every field is
    fixed, and virtual time is compared in every mode but replay.
    """
    return (
        comparison.get("strictness") == "canonical"
        and comparison.get("display_name") == "BitwiseInfoV1"
        and comparison.get("compare_logs") is True
        and comparison.get("compare_io_buffers") is True
        and comparison.get("log_scope") == "info"
        and _is_str_in(
            comparison.get("record_envelope"),
            {"all_records_v1", "dbt_evidence_transport_v1"},
        )
        and comparison.get("virtualize_time") is (mode != "replay")
        and comparison.get("strip_lines") is False
        and comparison.get("canonicalize_addresses") is True
        and comparison.get("full_trace") is True
        and comparison.get("exact_remainder") is True
        and comparison.get("stripped_prefixes") == ["real-wall-clock-prefix/v1"]
        and comparison.get("canonicalizations")
        == ["host-address-to-first-appearance-ordinal/v1"]
        and comparison.get("ignore_lines") is False
        and comparison.get("skip_commit") is False
        and comparison.get("skip_detlog") is False
    )


def _is_stripped_verify_comparison(comparison: dict, *, mode: str) -> bool:
    """Whether ``comparison`` is exactly the one Hermit writes for a stripped verify.

    ``verify::ComparisonSpec::new`` fixes every field of a stripped
    comparison but three parameters, and the runner's
    ``require_stripped_comparison`` requires two of them: logs compared, with
    virtual time. The third, ``compare_io_buffers``, is false only when the
    runner passed ``--no-detlog-io-buffers``; that is refused
    (:func:`_refuse_stripped_io_buffers_off`), so it must be true. The record
    envelope is one of the two canonical envelopes
    ``comparison_relaxations`` admits. Hermit's own readers check fewer of
    these fields; a report that differs in any of them was not written by
    Hermit's stripped verify, so it is refused rather than read as one.
    """
    return (
        mode == "verify"
        and comparison.get("strictness") == "stripped"
        and comparison.get("display_name") == "Stripped"
        and comparison.get("compare_logs") is True
        and comparison.get("compare_io_buffers") is True
        and comparison.get("log_scope") == "deterministic"
        and _is_str_in(
            comparison.get("record_envelope"),
            {"all_records_v1", "dbt_evidence_transport_v1"},
        )
        and comparison.get("virtualize_time") is True
        and comparison.get("strip_lines") is True
        and comparison.get("canonicalize_addresses") is False
        and comparison.get("full_trace") is False
        and comparison.get("exact_remainder") is False
        and comparison.get("stripped_prefixes") == [
            "real-wall-clock-prefix/v1",
            "unsafe-numeric-address-and-path-normalization/v1",
        ]
        and comparison.get("canonicalizations") == []
        and comparison.get("ignore_lines") is False
        and comparison.get("skip_commit") is False
        and comparison.get("skip_detlog") is False
    )


def _expected_guest_exit(row: dict) -> tuple[int | None, int | None] | None:
    """Return a verify cell's declared nonzero guest disposition, if any.

    The Hermit manifest may declare that a verify cell's guest exits with one
    exact nonzero code or dies from one exact signal. The runner then passes
    ``--verify-allow=failure`` and passes the attempt only when the canonical
    report and Hermit's own status both show that disposition, and it copies
    the declaration onto the result row. The row's declaration is the only
    authority under which a matched attempt may carry a nonzero status, so
    its shape is checked here exactly as the manifest loader checks it.
    Returns ``(code, signal)`` with exactly one member set.

    A replay cell inherits its verify recipe's declaration and the runner
    passes it only through the verify cell's gate, so a result row may carry
    it in verify or replay mode, as Hermit's
    ``runner::validate_result_expected_guest_exit`` accepts.
    """
    if "expected_guest_exit" not in row:
        return None
    declared = row["expected_guest_exit"]
    if _cell_mode(row) not in ("verify", "replay"):
        raise MalformedRow(
            "expected_guest_exit is valid only on a verify or replay cell result"
        )
    if not isinstance(declared, dict) or set(declared) != {"code", "signal", "reason"}:
        raise MalformedRow(
            "expected_guest_exit must be an object with exactly code, signal and reason"
        )
    code = declared["code"]
    signal = declared["signal"]
    reason = declared["reason"]
    if not isinstance(reason, str) or not reason.strip():
        raise MalformedRow("expected_guest_exit.reason must be substantive")
    if code is not None and signal is None:
        if type(code) is not int or not 1 <= code <= 255:
            raise MalformedRow(
                "expected_guest_exit.code must be a nonzero exit status in 1..=255, "
                f"got {code!r}"
            )
    elif code is None and signal is not None:
        if type(signal) is not int or not 1 <= signal <= 64:
            raise MalformedRow(
                "expected_guest_exit.signal must be a signal number in 1..=64, "
                f"got {signal!r}"
            )
    else:
        raise MalformedRow(
            "expected_guest_exit must name exactly one of code or signal"
        )
    return code, signal


#: Where Hermit's E2E manifests live in a Hermit commit, and the file of
#: shared defaults beside them that declares no test (ci/manifest-plan's
#: DEFAULTS_FILE). The Rust loader reads every other ``*.yaml`` file there.
HERMIT_MANIFEST_DIR = "tests/e2e/manifests"
HERMIT_MANIFEST_DEFAULTS = "defaults.yaml"

# Hermit's manifest loader (ci/manifest-plan/src/manifest_value.rs) parses YAML
# with serde_yaml 0.9.34, whose plain-scalar rules are YAML 1.2 core, not the
# YAML 1.1 rules PyYAML applies by default. Under PyYAML's rules `yes`, `on`,
# `010`, `1_0`, `1:30` and `2001-12-14` are not strings, and `0o7` is not an
# integer. These mirror serde_yaml's de.rs (visit_untagged_scalar, visit_int,
# parse_f64 and digits_but_not_number) so that both read the same values.
_SERDE_YAML_INT_TAG = "tag:hermit.invalid,2026:serde-yaml-int"
_SERDE_YAML_FLOAT_TAG = "tag:hermit.invalid,2026:serde-yaml-float"
_SERDE_YAML_RADIX = (("0x", 16, re.compile(r"[0-9a-fA-F]+")),
                     ("0o", 8, re.compile(r"[0-7]+")),
                     ("0b", 2, re.compile(r"[01]+")))
_SERDE_YAML_DECIMAL = re.compile(r"[0-9]+")
_SERDE_YAML_FLOAT = re.compile(r"-?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?")


def _serde_yaml_leading_zero(scalar: str) -> bool:
    """serde_yaml's ``digits_but_not_number``: ``0`` then digits is a string."""
    bare = scalar[1:] if scalar[:1] in ("+", "-") else scalar
    return len(bare) > 1 and bare[0] == "0" and _SERDE_YAML_DECIMAL.fullmatch(bare[1:]) is not None


def _serde_yaml_int(scalar: str) -> int | None:
    """The integer serde_yaml 0.9.34 resolves a plain scalar to, or None."""
    unpositive = scalar[1:] if scalar.startswith("+") else scalar
    for prefix, base, digits in _SERDE_YAML_RADIX:
        if unpositive.startswith(prefix) and digits.fullmatch(unpositive[2:]):
            value = int(unpositive[2:], base)
            return value if value < 1 << 128 else None
        if scalar.startswith("-" + prefix) and digits.fullmatch(scalar[3:]):
            value = -int(scalar[3:], base)
            return value if value >= -(1 << 127) else None
    if _serde_yaml_leading_zero(scalar):
        return None
    if _SERDE_YAML_DECIMAL.fullmatch(unpositive):
        value = int(unpositive)
        return value if value < 1 << 128 else None
    if scalar.startswith("-") and _SERDE_YAML_DECIMAL.fullmatch(scalar[1:]):
        value = int(scalar)
        return value if value >= -(1 << 127) else None
    return None


def _serde_yaml_float(scalar: str) -> float | None:
    """The float serde_yaml 0.9.34 resolves a plain non-integer scalar to, or None."""
    if _serde_yaml_leading_zero(scalar):
        return None
    unpositive = scalar[1:] if scalar.startswith("+") else scalar
    if unpositive[:1] == "+" or (scalar.startswith("+") and unpositive[:1] == "-"):
        return None
    if unpositive in (".inf", ".Inf", ".INF"):
        return math.inf
    if scalar in ("-.inf", "-.Inf", "-.INF"):
        return -math.inf
    if scalar in (".nan", ".NaN", ".NAN"):
        return math.nan
    if _SERDE_YAML_FLOAT.fullmatch(unpositive) is None:
        return None
    value = float(unpositive)
    return value if math.isfinite(value) else None


def _serde_yaml_plain_tag(scalar: str) -> str:
    if scalar in ("", "~", "null", "Null", "NULL"):
        return "tag:yaml.org,2002:null"
    if scalar in ("true", "True", "TRUE", "false", "False", "FALSE"):
        return "tag:yaml.org,2002:bool"
    if _serde_yaml_int(scalar) is not None:
        return _SERDE_YAML_INT_TAG
    if _serde_yaml_float(scalar) is not None:
        return _SERDE_YAML_FLOAT_TAG
    return "tag:yaml.org,2002:str"


def _manifest_yaml_loader(yaml: Any) -> type:
    """A PyYAML loader that reads a manifest as Hermit's loader does.

    Plain scalars resolve by serde_yaml's rules above; a quoted scalar is a
    string in both. A duplicate or non-string mapping key is refused, as
    ``manifest_value.rs`` refuses it: PyYAML would otherwise keep the last
    duplicate silently.
    """
    base = getattr(yaml, "CSafeLoader", yaml.SafeLoader)

    class ManifestLoader(base):  # type: ignore[misc, valid-type]
        def resolve(self, kind, value, implicit):
            if kind is yaml.ScalarNode and implicit[0]:
                return _serde_yaml_plain_tag(value)
            return super().resolve(kind, value, implicit)

        def construct_mapping(self, node, deep=False):
            seen: set[str] = set()
            for key_node, _ in node.value:
                key = self.construct_object(key_node, deep=True)
                if not isinstance(key, str):
                    raise yaml.constructor.ConstructorError(
                        None, None, f"manifest mapping key {key!r} is not a string",
                        key_node.start_mark,
                    )
                if key in seen:
                    raise yaml.constructor.ConstructorError(
                        None, None, f"duplicate manifest key {key!r}", key_node.start_mark,
                    )
                seen.add(key)
            return super().construct_mapping(node, deep=deep)

    def construct_int(loader, node):
        value = _serde_yaml_int(loader.construct_scalar(node))
        # serde_yaml resolves integers up to 128 bits, but its Value holds
        # only i64 and u64 and refuses the rest.
        if value is None or not -(1 << 63) <= value < 1 << 64:
            raise yaml.constructor.ConstructorError(
                None, None, f"integer {node.value} is outside i64 and u64", node.start_mark,
            )
        return value

    ManifestLoader.add_constructor(_SERDE_YAML_INT_TAG, construct_int)
    ManifestLoader.add_constructor(
        _SERDE_YAML_FLOAT_TAG,
        lambda loader, node: _serde_yaml_float(loader.construct_scalar(node)),
    )
    return ManifestLoader


def manifest_guest_exit_declarations(
    source: Path, tree: str,
) -> dict[str, tuple[int | None, int | None, str] | None]:
    """Every manifest test id at Hermit ``tree`` and its verify guest-exit declaration.

    The first table of :func:`manifest_verify_declarations`.
    """
    return manifest_verify_declarations(source, tree)[0]


def manifest_verify_declarations(
    source: Path, tree: str,
) -> tuple[
    dict[str, tuple[int | None, int | None, str] | None],
    dict[str, str | None],
]:
    """Every manifest test id at Hermit ``tree`` and its verify declarations.

    Returns two tables keyed by the same test ids. The first is the guest-exit
    table described below. The second maps each id to the
    ``modes.verify.comparator_reason`` of an entry whose
    ``modes.verify.comparator`` is exactly ``stripped`` (including a corpus
    row's lane-wide declaration, :func:`_corpus_tests`), or to None when the
    entry declares no stripped comparator or no string reason. Hermit's
    manifest loader refuses any other comparator spelling and a blank reason,
    so None here only ever refuses a row's claim; it never fails the tree.

    Reads ``tests/e2e/manifests/*.yaml`` (not ``defaults.yaml``) from the
    immutable commit, never from a working tree, resolving scalars as Hermit's
    serde_yaml loader does (:func:`_manifest_yaml_loader`). In the guest-exit
    table each test id maps to ``(code, signal, reason)`` from
    ``modes.verify.expected_guest_exit``, or to None when the entry declares
    none. The manifest, not the row, is the authority for both declarations:
    the runner copies each onto the result row verbatim (the stripped
    comparator as a relaxation), so a row that claims one its tree's manifest
    does not make is not evidence of what the cell was meant to be judged by.
    A tree that cannot be read, or whose manifests do not parse into test
    entries with unique ids, raises :class:`SeriesError`: that is a fault in
    the invocation's source, not in any one row.
    """
    listed = _git(source, ["ls-tree", "-z", tree, "--", HERMIT_MANIFEST_DIR + "/"])
    if listed.returncode != 0:
        detail = listed.stderr.strip() or "no output"
        raise SeriesError(f"cannot list Hermit manifests at {tree}: {detail}")
    try:
        import yaml
    except ImportError as error:
        raise SeriesError(
            f"cannot read Hermit manifests at {tree} without PyYAML: {error}"
        ) from error
    loader = _manifest_yaml_loader(yaml)
    env = dict(os.environ)
    for name in _GIT_REPO_SCOPED_ENV:
        env.pop(name, None)
    declarations: dict[str, tuple[int | None, int | None, str] | None] = {}
    stripped: dict[str, str | None] = {}
    for entry in listed.stdout.split("\0"):
        if not entry:
            continue
        meta, _, path = entry.partition("\t")
        fields = meta.split()
        name = path.rsplit("/", 1)[-1]
        if (len(fields) != 3 or fields[1] != "blob" or not name.endswith(".yaml")
                or name == HERMIT_MANIFEST_DEFAULTS):
            continue
        shown = subprocess.run(
            ["git", "-C", str(source), "cat-file", "blob", fields[2]],
            capture_output=True, timeout=60, check=False, env=env,
        )
        if shown.returncode != 0:
            detail = shown.stderr.decode(errors="replace").strip() or "no output"
            raise SeriesError(f"cannot read {path} at Hermit {tree}: {detail}")
        try:
            document = yaml.load(shown.stdout.decode("utf-8"), Loader=loader)
        except (UnicodeDecodeError, yaml.YAMLError) as error:
            raise SeriesError(f"{path} at Hermit {tree} is not UTF-8 YAML: {error}") from error
        tests = document.get("test") if isinstance(document, dict) else None
        corpus = document.get("corpus") if isinstance(document, dict) else None
        if corpus is not None:
            tests = list(tests or []) + _corpus_tests(document, corpus, path, tree)
        if not isinstance(tests, list):
            raise SeriesError(f"{path} at Hermit {tree} has no test list")
        for test in tests:
            identifier = test.get("id") if isinstance(test, dict) else None
            if not isinstance(identifier, str) or not identifier:
                raise SeriesError(f"{path} at Hermit {tree} has a test without a string id")
            if identifier in declarations:
                raise SeriesError(f"Hermit {tree} declares manifest test {identifier!r} twice")
            modes = test.get("modes")
            verify = modes.get("verify") if isinstance(modes, dict) else None
            comparator = verify.get("comparator") if isinstance(verify, dict) else None
            comparator_reason = (
                verify.get("comparator_reason") if isinstance(verify, dict) else None
            )
            stripped[identifier] = (
                comparator_reason
                if type(comparator) is str and comparator == "stripped"
                and type(comparator_reason) is str
                else None
            )
            declared = verify.get("expected_guest_exit") if isinstance(verify, dict) else None
            if declared is None:
                declarations[identifier] = None
                continue
            if not isinstance(declared, dict) or not set(declared) <= {"code", "signal", "reason"}:
                raise SeriesError(
                    f"{path} at Hermit {tree}: {identifier} expected_guest_exit is not "
                    "an object of code, signal and reason"
                )
            declarations[identifier] = (
                declared.get("code"), declared.get("signal"), declared.get("reason"),
            )
    return declarations, stripped


def _bind_expected_guest_exit(
    row: dict, tree: str,
    declarations: Callable[[], dict[str, tuple[int | None, int | None, str] | None]],
) -> None:
    """Refuse a row whose guest-exit declaration its tree's manifest does not make.

    The row's declaration is shape-checked first (:func:`_expected_guest_exit`).
    It must then equal, in code, signal AND reason, the declaration on the
    manifest entry named by the row's ``test`` at the row's Hermit ``tree``.
    The runner copies all three verbatim, so any difference means the row
    was not produced under this tree's manifest; that includes a row from a
    dirty working tree whose uncommitted manifest declared it. Such a row is
    refused as :class:`MalformedRow` (quarantined, never admitted). An
    undeclared row never reads the manifests.
    """
    expected = _expected_guest_exit(row)
    if expected is None:
        return
    test = row.get("test")
    if not isinstance(test, str) or not test:
        raise MalformedRow("a declared guest exit needs the row's manifest test id")
    manifests = declarations()
    if test not in manifests:
        raise MalformedRow(
            f"expected_guest_exit is declared for {test!r}, which has no manifest "
            f"entry at Hermit {tree}"
        )
    authority = manifests[test]
    if authority is None:
        raise MalformedRow(
            f"expected_guest_exit is declared for {test!r}, whose manifest entry at "
            f"Hermit {tree} declares none"
        )
    declared = row["expected_guest_exit"]
    claimed = (declared["code"], declared["signal"], declared["reason"])
    if any(type(have) is not type(want) or have != want
           for have, want in zip(claimed, authority)):
        raise MalformedRow(
            f"expected_guest_exit code={claimed[0]!r} signal={claimed[1]!r} for {test!r} "
            f"disagrees with its manifest entry at Hermit {tree} "
            f"(code={authority[0]!r} signal={authority[1]!r}) or its reason"
        )


# The error kinds Hermit's runner gives an attempt it timed out
# (``ProcessTimeout::error_kind`` in ``ci/manifest-plan/src/runner.rs``).
_TIMEOUT_ERROR_KINDS = frozenset({"cpu-timeout", "wall-timeout"})


def _is_timed_out_match(attempt: dict) -> bool:
    """Whether a matched attempt is one Hermit's runner timed out after its report.

    Mirrors Hermit's ``SeriesNoVerdictKind::TimedOutMatch``
    (``ci/manifest-plan/src/stress_series.rs``). The runner never passes a
    timed-out attempt: it is a FAIL, or an ERROR when the runner also refused
    the report's comparison policy, and the timeout's error kind replaces any
    other. The process ended and was reaped, so exactly one status or signal
    is known: the normal status when a CPU timeout is found at the final
    wait, the kill's signal otherwise. Such a match is that timeout's
    evidence, never credit and never red.
    """
    status = attempt.get("status")
    signal = attempt.get("signal")
    reaped = (type(status) is int and status >= 0 and signal is None) or (
        status is None and type(signal) is int and signal > 0
    )
    return (
        _is_str_in(attempt.get("outcome"), {"FAIL", "ERROR"})
        and attempt.get("timed_out") is True
        and _is_str_in(attempt.get("error_kind"), _TIMEOUT_ERROR_KINDS)
        and reaped
    )


# The reason Hermit's runner gives a cell it failed after every attempt passed,
# because the cell's SaBRe execution path was ineligible
# (``execution_path_ineligible`` in ``ci/manifest-plan/src/runner.rs``).
_SABRE_INELIGIBLE_EXECUTION_PATH_REASON = (
    "SaBRe execution path is incomplete or used fallback/native sites"
)
# The fields of one execution's SaBRe path-evidence record, Hermit's
# ``PathEvidence`` (``detcore-model/src/summary.rs``), which refuses any other.
_SABRE_PATH_EVIDENCE_FIELDS = frozenset({
    "schema",
    "guest_rpc_observed",
    "ptrace_fallback_sites",
    "trusted_shared_object_sites",
    "trusted_shared_objects",
})
# serde_json's default recursion limit admits 127 nested arrays or objects.
_SERDE_JSON_MAX_DEPTH = 127


def _argv_selects_sabre(argv: list[str]) -> bool:
    """Whether a Hermit command line selects the SaBRe backend.

    Mirrors Hermit's ``argv_selects_sabre``: only Hermit's own arguments,
    before the first ``--``, select a backend; a ``--backend sabre`` pair
    after it is an argument of the guest.
    """
    hermit_args = argv[:argv.index("--")] if "--" in argv else argv
    return any(
        first == "--backend" and second == "sabre"
        for first, second in zip(hermit_args, hermit_args[1:])
    )


def _rust_lines(text: str) -> list[str]:
    """Split ``text`` as Rust's ``str::lines`` does.

    A line ends at ``\\n`` or ``\\r\\n``, and a final empty line is not one,
    but a line between two newlines is, even when it is empty.
    """
    pieces = text.split("\n")
    lines = [piece.removesuffix("\r") for piece in pieces[:-1]]
    if pieces[-1]:
        lines.append(pieces[-1])
    return lines


def _json_nesting_depth(text: str) -> int:
    """The deepest nesting of arrays and objects in JSON ``text``."""
    depth = deepest = 0
    in_string = escaped = False
    for character in text:
        if in_string:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                in_string = False
        elif character == '"':
            in_string = True
        elif character in "[{":
            depth += 1
            deepest = max(deepest, depth)
        elif character in "]}":
            depth -= 1
    return deepest


def _serde_value_float(text: str) -> float:
    value = float(text)
    if not math.isfinite(value):
        raise ValueError(f"number out of range: {text}")
    return value


def _serde_value_int(text: str) -> int | float:
    # serde_json reads -0, a negative integer, and one past u64 as a float or
    # an i64, never as the u64 that a site count or the schema must be.
    if text.startswith("-") or int(text) > 2**64 - 1:
        return _serde_value_float(text)
    return int(text)


def _serde_value_constant(text: str):
    raise ValueError(f"{text} is not JSON")


def _serde_value_fields(pairs):
    fields = {}
    for name, value in pairs:
        if name in fields:
            # serde_json keeps the last duplicate, but it has already read the
            # one it replaces, and refuses an unpaired surrogate there too.
            json.dumps(fields[name], ensure_ascii=False).encode()
        fields[name] = value
    return fields


def _sabre_path_evidence_record(line: str) -> dict:
    """Read one line of an attempt's SaBRe path evidence as Hermit does.

    Hermit reads the line as a ``serde_json::Value``, requires its ``schema``
    to be 1, and then reads it as a ``PathEvidence``
    (``summarize_sabre_path_evidence``), so this refuses what either refuses.
    """
    try:
        if _json_nesting_depth(line) > _SERDE_JSON_MAX_DEPTH:
            raise ValueError("recursion limit exceeded")
        value = json.loads(
            line,
            object_pairs_hook=_serde_value_fields,
            parse_constant=_serde_value_constant,
            parse_float=_serde_value_float,
            parse_int=_serde_value_int,
        )
        json.dumps(value, ensure_ascii=False).encode()
    except (ValueError, RecursionError) as error:
        # JSONDecodeError, a refused number or constant, an integer past
        # Python's digit limit, and an unpaired surrogate.
        raise MalformedRow(f"invalid SaBRe path-evidence record: {error}") from error
    schema = value.get("schema") if isinstance(value, dict) else None
    if type(schema) is not int or schema != 1:
        raise MalformedRow("invalid SaBRe path-evidence record: schema must be 1")
    if (
        set(value) != _SABRE_PATH_EVIDENCE_FIELDS
        or type(value["guest_rpc_observed"]) is not bool
        or not _is_u64(value["ptrace_fallback_sites"])
        or not _is_u64(value["trusted_shared_object_sites"])
        or not isinstance(value["trusted_shared_objects"], list)
        or not all(isinstance(name, str) for name in value["trusted_shared_objects"])
    ):
        raise MalformedRow(
            "invalid SaBRe path-evidence record: it is not Hermit's PathEvidence"
        )
    return value


def _summarize_sabre_path_evidence(attempts: list[dict]) -> dict | None:
    """Summarize the attempts' SaBRe path evidence as Hermit's runner does.

    Mirrors Hermit's ``summarize_sabre_path_evidence``: ``None`` when no
    attempt selects SaBRe; otherwise every retained record, how many were
    expected (two for each SaBRe attempt that verifies, one for any other),
    and whether the path was eligible: complete, with the guest's RPC observed
    in every execution and no ptrace-fallback or trusted-shared-object site.
    """
    if not any(_argv_selects_sabre(attempt["argv"]) for attempt in attempts):
        return None
    executions = [
        _sabre_path_evidence_record(line)
        for attempt in attempts
        for line in _rust_lines(attempt.get("sabre_path_evidence") or "")
    ]
    # Two executions for an attempt that verifies, in either of Hermit's forms:
    # `--verify` in one process, or the split `--verify-run` that
    # hermit-verify-pair runs twice (hermit a1c953ebc, 2026-10-13). Counting
    # only `--verify` made every split-verify SaBRe cell incomplete here while
    # Hermit's summary called it complete, so append-cells quarantined it as
    # MalformedRow (pressure run pressure-03c24d69a8e7-20261015T004056Z,
    # compat pathchk and uuidgen on sabre).
    expected = sum(
        2 if any(arg in ("--verify", "--verify-run") for arg in attempt["argv"]) else 1
        for attempt in attempts
        if _argv_selects_sabre(attempt["argv"])
    )
    complete = len(executions) == expected
    guest_rpc_observed = complete and all(
        execution["guest_rpc_observed"] for execution in executions
    )
    ptrace_fallback_sites = sum(
        execution["ptrace_fallback_sites"] for execution in executions
    )
    trusted_shared_object_sites = sum(
        execution["trusted_shared_object_sites"] for execution in executions
    )
    if not (_is_u64(ptrace_fallback_sites) and _is_u64(trusted_shared_object_sites)):
        # Hermit sums the sites as a usize with overflow checks on.
        raise MalformedRow("SaBRe path-evidence site counts overflow a usize")
    return {
        "schema": 1,
        "expected_execution_count": expected,
        "complete": complete,
        "execution_count": len(executions),
        "guest_rpc_observed": guest_rpc_observed,
        "ptrace_fallback_sites": ptrace_fallback_sites,
        "trusted_shared_object_sites": trusted_shared_object_sites,
        "trusted_shared_objects": sorted({
            name for execution in executions for name in execution["trusted_shared_objects"]
        }),
        "eligible": (
            complete
            and guest_rpc_observed
            and ptrace_fallback_sites == 0
            and trusted_shared_object_sites == 0
        ),
        "executions": executions,
    }


def _retained_sabre_path_summary(row: dict) -> dict | None:
    """Re-decide the row's SaBRe execution path from its retained attempts.

    Mirrors Hermit's ``retained_path_evidence_error``: the row's
    ``execution_path`` is a label, so every attempt must select SaBRe exactly
    when the row's backend is ``sabre``, every attempt's retained evidence
    text must be the bytes its ``sabre_path_evidence_sha256`` names (both
    null for an attempt that wrote none), and the summary of those attempts
    must equal the row's ``execution_path``. Returns that summary.
    """
    attempts = row.get("attempts")
    if not isinstance(attempts, list) or not all(isinstance(a, dict) for a in attempts):
        raise MalformedRow("cell result attempts must be an array of objects")
    sabre = row.get("backend") == "sabre"
    for ordinal, attempt in enumerate(attempts, 1):
        argv = attempt.get("argv")
        if not isinstance(argv, list) or not all(isinstance(arg, str) for arg in argv):
            raise MalformedRow(f"cell attempt {ordinal} argv is not a list of strings")
        if _argv_selects_sabre(argv) != sabre:
            raise MalformedRow(
                f"cell attempt {ordinal}'s command line does not select the row's "
                f"backend {row.get('backend')!r}"
            )
        evidence = attempt.get("sabre_path_evidence")
        digest = attempt.get("sabre_path_evidence_sha256")
        if not (_is_optional(evidence, _is_rust_string) and _is_optional(digest, _is_rust_string)):
            raise MalformedRow(
                f"cell attempt {ordinal} SaBRe path evidence and its digest must be "
                "strings or null"
            )
        if digest != (
            None if evidence is None else hashlib.sha256(evidence.encode()).hexdigest()
        ):
            raise MalformedRow(
                f"cell attempt {ordinal}'s retained SaBRe path evidence is not the bytes "
                "its sabre_path_evidence_sha256 names"
            )
    summary = _summarize_sabre_path_evidence(attempts)
    # Compared as JSON text, so a count that is a float or a bool differs from
    # the integer it equals in Python, as serde_json's Value equality has it.
    if json.dumps(summary, sort_keys=True) != json.dumps(
        row.get("execution_path"), sort_keys=True
    ):
        raise MalformedRow(
            "the row's execution_path is not the summary of its retained SaBRe path evidence"
        )
    return summary


def _ineligible_execution_path(row: dict) -> bool:
    """Whether Hermit's runner failed this cell for its SaBRe execution path.

    Hermit's runner passes a verify cell only when every attempt passed, and
    then fails a pass whose SaBRe execution path is ineligible, with
    :data:`_SABRE_INELIGIBLE_EXECUTION_PATH_REASON`: a crash-error ``FAIL``
    whose attempts are all passes. Such a row is told by that reason, and
    then believed only if its retained path evidence re-decides an
    ineligible path (:func:`_retained_sabre_path_summary`); a row that claims
    the reason with evidence that does not is refused. Mirrors Hermit's
    ``SeriesNoVerdictKind::IneligibleExecutionPath``
    (``ci/manifest-plan/src/stress_series.rs``), which accepts only a SaBRe
    verify cell with a crash-error result.
    """
    if (
        row.get("reason") != _SABRE_INELIGIBLE_EXECUTION_PATH_REASON
        or row.get("mode") != "verify"
        or row.get("backend") != "sabre"
        or row.get("outcome") != "FAIL"
        or row.get("result") != "crash-error"
        or row.get("failure_class") != "product_failure"
    ):
        return False
    if row.get("error_kind") is not None:
        raise MalformedRow("a SaBRe execution-path failure carries an error kind")
    summary = _retained_sabre_path_summary(row)
    if summary is None or summary["eligible"]:
        raise MalformedRow(
            "row records a SaBRe execution-path failure, but its retained path evidence "
            "is not ineligible"
        )
    return True


def _invocation_admits_declared_guest_exit(mode: str, hermit_args: list[str]) -> bool:
    """Mirror Hermit's ``runner::invocation_admits_declared_guest_exit``.

    A declared nonzero exit is credited only from the invocation the runner
    writes for it: a verify comparison with ``--verify-allow=failure``, or a
    replay cell's ``record start``, whose comparison takes
    ``--verify-allow=both``.
    """
    if mode == "verify":
        return "--verify-allow=failure" in hermit_args
    if mode == "replay":
        return "--verify-allow=both" in hermit_args and any(
            first == "record" and second == "start"
            for first, second in zip(hermit_args, hermit_args[1:])
        )
    return False


def _require_matched_evidence(
    attempt: dict,
    report: dict,
    *,
    index: int,
    expected_guest_exit: tuple[int | None, int | None] | None,
    stripped: bool = False,
    mode: str = "verify",
) -> tuple[int | None, int | None]:
    """Refuse a matched attempt whose evidence is malformed or contradicts itself.

    This runs before a matched attempt is classified, so a ``failed_match``
    can only be structurally valid evidence that ended differently from what
    its row allows; it can never be a contradiction relabelled as red.

    Present ``compared_outputs`` must satisfy Hermit's
    ``require_exact_output_match`` (``hermit-cli/src/canonical_verdict.rs``):
    Hermit's complete canonical shape on both sides, identical left and
    right, exactly one guest disposition, agreeing with the report's own
    guest disposition. An undeclared report may omit them, as a historical
    receipt does. A declared row additionally requires the outputs,
    ``--verify-allow=failure`` before the guest separator, and a Hermit status
    that reports the guest's disposition (a signal death either as that
    signal or as ``128 + signo``), unless the attempt is a timed-out match
    (:func:`_is_timed_out_match`) the runner killed, whose signal is the
    kill's.

    ``stripped`` is set only for a match in a cell that declares Hermit's
    stripped comparator (see :func:`_declares_stripped_comparator`). Hermit
    writes such a match as verified with ``bitwise_parity`` false, and its
    tier is exit-and-stream equality, so the compared outputs are the whole
    basis of the match and are required even without a declared guest exit.

    Returns the report's guest disposition ``(code, signal)``.
    """
    def refuse(detail: str) -> None:
        raise MalformedRow(
            f"cell attempt {index} matched report is internally inconsistent: {detail}"
        )

    if stripped:
        if report.get("verified") is not True or report.get("bitwise_parity") is not False:
            refuse("a stripped match must be verified with bitwise_parity false")
    elif report.get("verified") is not True or report.get("bitwise_parity") is not True:
        refuse("verified and bitwise_parity must both be true")
    status = attempt.get("status")
    observed_signal = attempt.get("signal")
    for name, value in (("status", status), ("signal", observed_signal)):
        if not _is_optional_i32(value):
            refuse(f"attempt {name} is not an i32")
    guest = (report.get("guest_exit_code"), report.get("guest_signal"))
    if not all(_is_optional_i32(value) for value in guest):
        refuse("report guest disposition is not typed")
    outputs = report.get("compared_outputs")
    if outputs is None:
        if expected_guest_exit is not None:
            refuse("a declared guest exit requires compared_outputs")
        if stripped:
            refuse("a stripped match requires compared_outputs")
    else:
        if not isinstance(outputs, dict) or set(outputs) != {"left", "right"}:
            refuse("compared_outputs must have exactly left and right")
        for side in ("left", "right"):
            output = outputs[side]
            if not _is_canonical_compared_output(output):
                refuse(f"{side} compared output is not Hermit's canonical shape")
            if (output["exit_code"] is None) == (output["signal"] is None):
                refuse(f"{side} compared output must record exactly one guest disposition")
        # Both sides are typed above: Python's == equates 7 with 7.0 and 1
        # with True, which Hermit's typed reader refuses on either side.
        if outputs["left"] != outputs["right"]:
            refuse("left and right compared outputs differ")
        if guest != (outputs["left"]["exit_code"], outputs["left"]["signal"]):
            refuse("compared outputs contradict the report's guest disposition")
    if expected_guest_exit is not None:
        argv = attempt.get("argv")
        if not isinstance(argv, list) or not all(isinstance(arg, str) for arg in argv):
            refuse("attempt argv is not a list of strings")
        hermit_args = argv[:argv.index("--")] if "--" in argv else argv
        if not _invocation_admits_declared_guest_exit(mode, hermit_args):
            if mode == "replay":
                refuse(
                    "a declared guest exit ran without a record start compared "
                    "with --verify-allow=both"
                )
            refuse("a declared guest exit ran without --verify-allow=failure")
        code, signal = guest
        if (code is None) == (signal is None):
            refuse("report must record exactly one guest disposition")
        if code is not None:
            reported = status == code and observed_signal is None
        else:
            reported = (status is None and observed_signal == signal) or (
                status == 128 + signal and observed_signal is None
            )
        # The runner kills a process whose wall timeout, or CPU timeout while
        # it polls, fires after the report was written, so a timed-out match
        # may carry the kill's signal instead of Hermit's status.
        if not reported and not (_is_timed_out_match(attempt) and observed_signal is not None):
            refuse("Hermit's status does not report the guest's disposition")
    return guest


_COMPARED_OUTPUT_FIELDS = frozenset({
    "exit_code", "signal", "stdout_sha256", "stdout_bytes", "stderr_sha256", "stderr_bytes",
})


def _is_u64(value: Any) -> bool:
    return type(value) is int and 0 <= value <= 2**64 - 1


def _is_i64(value: Any) -> bool:
    return type(value) is int and -(2**63) <= value <= 2**63 - 1


_HOST_FILE_IDENTITY_FIELDS = {
    "dev": _is_u64, "ino": _is_u64, "size": _is_i64, "mtime_sec": _is_i64, "mtime_nsec": _is_i64,
}


def _is_typed_infrastructure_error(report: dict) -> bool:
    """Whether ``report`` carries an ``InfrastructureError`` that the pinned
    Hermit ``hermit-cli/src/canonical_verdict.rs`` writes.

    ``skid_overshoot`` carries its integer ``count`` (its range is checked by
    pressure evidence). ``host_input_changed`` names the run that saw a host
    file change (``run1`` or ``run2``), the file's path, and its host identity
    (``HostFileIdentity``: u64 ``dev`` and ``ino``, i64 ``size``,
    ``mtime_sec`` and ``mtime_nsec``) before and after the change. It explains
    a divergence, so, as Hermit's report reader requires, the report keeps the
    comparison that found it.
    """
    reason = report.get("infrastructure_error")
    if not isinstance(reason, dict):
        return False
    kind = reason.get("kind")
    if kind == "skid_overshoot":
        count = reason.get("count")
        return isinstance(count, int) and not isinstance(count, bool)
    if kind == "host_input_changed":
        return (
            report.get("comparison") is not None
            and report.get("compared_log_messages") is not None
            and reason.keys() == {"kind", "run", "path", "before", "after"}
            and _is_str_in(reason.get("run"), {"run1", "run2"})
            and isinstance(reason.get("path"), str)
            and bool(reason["path"])
            and all(
                isinstance(reason.get(side), dict)
                and reason[side].keys() == _HOST_FILE_IDENTITY_FIELDS.keys()
                and all(valid(reason[side][field]) for field, valid in _HOST_FILE_IDENTITY_FIELDS.items())
                for side in ("before", "after")
            )
        )
    return False


def _is_optional_i32(value: Any) -> bool:
    return value is None or (type(value) is int and -(2**31) <= value <= 2**31 - 1)


def _is_typed_runtime(runtime: Any) -> bool:
    """Whether ``runtime`` reads as Hermit's ``Option<VerificationRuntime>``.

    Each present run is a ``RuntimeStats``: ``scheduler_turns`` and
    ``virtual_nanoseconds`` are required u64s and ``syscalls`` an optional u64.
    Neither Rust type denies unknown fields, so other fields are not refused.
    """
    if runtime is None:
        return True
    if not isinstance(runtime, dict):
        return False
    for run in ("run1", "run2"):
        stats = runtime.get(run)
        if stats is None:
            continue
        if (
            not isinstance(stats, dict)
            or not _is_u64(stats.get("scheduler_turns"))
            or not _is_u64(stats.get("virtual_nanoseconds"))
            or not (stats.get("syscalls") is None or _is_u64(stats["syscalls"]))
        ):
            return False
    return True


def _is_canonical_compared_output(output: Any) -> bool:
    """Whether one side of ``compared_outputs`` has exactly Hermit's typed fields."""
    return (
        isinstance(output, dict)
        and set(output) == _COMPARED_OUTPUT_FIELDS
        and _is_optional_i32(output["exit_code"])
        and _is_optional_i32(output["signal"])
        and all(
            isinstance(output[field], str)
            and re.fullmatch(r"[0-9a-f]{64}", output[field]) is not None
            for field in ("stdout_sha256", "stderr_sha256")
        )
        and all(
            _is_u64(output[field]) for field in ("stdout_bytes", "stderr_bytes")
        )
    )


_U64_MAX = 2**64 - 1
# The texts ``expected_stdout_mismatch_reason`` and
# ``expected_stdout_contains_mismatch_reason`` in Hermit's
# ci/manifest-plan/src/runner.rs write. Rust's ``u64`` display has no sign and
# no leading zero, and its digests are lowercase SHA-256.
_EXACT_STDOUT_REASON_RE = re.compile(
    r"verify (?P<run>[a-z]+) run stdout \((?P<observed_bytes>0|[1-9][0-9]*) bytes, "
    r"sha256 (?P<observed_sha256>[0-9a-f]{64})\) differs from the declared "
    r"expected_stdout \((?P<declared_bytes>0|[1-9][0-9]*) bytes, "
    r"sha256 (?P<declared_sha256>[0-9a-f]{64})\)"
)
_CONTAINS_STDOUT_REASON_RE = re.compile(
    r"verify stdout \((?P<observed_bytes>0|[1-9][0-9]*) bytes, "
    r"sha256 (?P<observed_sha256>[0-9a-f]{64})\) does not contain the declared "
    r"expected_stdout_contains text (?P<quoted>\".*\")",
    re.DOTALL,
)


def _rust_debug_ascii(text: str) -> str | None:
    """Rust's ``{:?}`` rendering of an ASCII ``text``, or None for other text.

    Beyond ASCII, Rust's rendering depends on its Unicode printability tables,
    which this reader does not reproduce; such a text is not recognized.
    """
    rendered = ['"']
    for character in text:
        code = ord(character)
        if code > 0x7F:
            return None
        rendered.append({
            "\t": "\\t", "\r": "\\r", "\n": "\\n", "\0": "\\0",
            "\\": "\\\\", '"': '\\"',
        }.get(character) or (
            f"\\u{{{code:x}}}" if code < 0x20 or code == 0x7F else character
        ))
    rendered.append('"')
    return "".join(rendered)


def _parse_rust_debug_ascii(quoted: str) -> str | None:
    """Invert :func:`_rust_debug_ascii`, or None when it could not have written ``quoted``."""
    if len(quoted) < 2 or not quoted.startswith('"') or not quoted.endswith('"'):
        return None
    inner = quoted[1:-1]
    text = []
    position = 0
    while position < len(inner):
        character = inner[position]
        position += 1
        if character != "\\":
            text.append(character)
            continue
        escape = inner[position:position + 1]
        position += 1
        if escape == "u":
            closing = inner.find("}", position)
            digits = inner[position + 1:closing] if closing > position else ""
            if (not inner.startswith("{", position) or not digits
                    or re.fullmatch(r"[0-9a-f]+", digits) is None or int(digits, 16) > 0x7F):
                return None
            text.append(chr(int(digits, 16)))
            position = closing + 1
        elif escape in ("t", "r", "n", "0", "\\", '"'):
            text.append({"t": "\t", "r": "\r", "n": "\n", "0": "\0"}.get(escape, escape))
        else:
            return None
    decoded = "".join(text)
    return decoded if _rust_debug_ascii(decoded) == quoted else None


def _expected_output_failure_reason(
    attempt: dict, report: dict, *, mode: str, index: int,
) -> str | None:
    """The runner's reason for a failed declared stdout on a matched FAIL, or None.

    Hermit's runner checks a verify cell's declared ``expected_stdout`` and
    ``expected_stdout_contains`` after the canonical comparison matched and
    before the guest's exit, and records a failure only as the attempt's
    ``reason``: no timeout, no error kind, and possibly status 0. None: the
    reason is not one of those texts. A string: it is, and the evidence bears
    it out, so the attempt is a failed match. MalformedRow: it claims such a
    failure that the evidence contradicts.

    A replay cell inherits its verify recipe's declarations, exactly as it
    inherits ``expected_guest_exit`` (``runner::expectation_recipe``), so a
    replay attempt may carry this reason and is judged on the same evidence.

    The evidence is what the runner decided from. For an exact
    ``expected_stdout`` that is the matched report's per-run stdout identity,
    so the captured ``stdout`` is not consulted. For
    ``expected_stdout_contains`` the runner searched the capture after
    requiring it to equal both compared runs, so a recorded capture must be
    those bytes and must omit the text. Hermit's runner records a capture it
    cannot read as UTF-8 as empty, so an empty capture of nonempty output is
    absent, not other bytes. The row does not carry the manifest's declared
    stdout, so the declared side is checked only for what it cannot be.
    """
    reason = attempt.get("reason")
    if not isinstance(reason, str):
        return None
    exact = _EXACT_STDOUT_REASON_RE.fullmatch(reason)
    contains = None if exact else _CONTAINS_STDOUT_REASON_RE.fullmatch(reason)
    claim = exact or contains
    if claim is None:
        return None
    text = None
    if contains is not None:
        text = _parse_rust_debug_ascii(contains["quoted"])
        if text is None:
            return None

    def contradiction(why: str) -> MalformedRow:
        return MalformedRow(
            f"cell attempt {index} reason claims a failed declared expected output "
            f"that contradicts its evidence: {why}"
        )

    if mode not in ("verify", "replay"):
        raise contradiction(
            f"only a verify or replay cell declares expected stdout, and this is a {mode} cell"
        )
    status = attempt.get("status")
    signal = attempt.get("signal")
    completed = (
        type(status) is int and status >= 0 and signal is None
    ) or (
        status is None and type(signal) is int and signal > 0
    )
    if (
        attempt.get("timed_out") is not False
        or attempt.get("error_kind") is not None
        or not completed
    ):
        raise contradiction(
            "the runner fails declared stdout only for an attempt that completed "
            "with a status or signal, no timeout and no error kind"
        )
    outputs = report.get("compared_outputs")
    if (
        not isinstance(outputs, dict)
        or set(outputs) != {"left", "right"}
        or not _is_canonical_compared_output(outputs["left"])
        or outputs["left"] != outputs["right"]
        or not all(
            outputs["left"][field] is None or type(outputs["left"][field]) is int
            for field in ("exit_code", "signal")
        )
    ):
        raise contradiction("the matched report has no identical, complete compared outputs")
    observed_bytes = int(claim["observed_bytes"])
    observed_sha256 = claim["observed_sha256"]
    if observed_bytes > _U64_MAX:
        raise contradiction("its observed byte count exceeds u64")
    left = outputs["left"]
    if left["stdout_bytes"] != observed_bytes or left["stdout_sha256"] != observed_sha256:
        raise contradiction(
            f"both compared runs printed {left['stdout_bytes']} bytes with sha256 "
            f"{left['stdout_sha256']}, not the stdout the reason names"
        )
    if exact is not None:
        # The runner names the first run whose stdout differs, and both runs
        # of a matched pair print the same bytes.
        if exact["run"] != "first":
            raise contradiction(
                f"it names the {exact['run']} run, but the first run of a matched "
                "pair printed the same bytes"
            )
        declared_bytes = int(exact["declared_bytes"])
        if declared_bytes > _U64_MAX:
            raise contradiction("its declared byte count exceeds u64")
        if declared_bytes == observed_bytes and exact["declared_sha256"] == observed_sha256:
            raise contradiction("the declared stdout is the observed stdout")
        return reason
    if not text:
        raise contradiction("the runner finds an empty declared text in any stdout")
    stdout = attempt.get("stdout")
    if stdout is None or (stdout == "" and observed_bytes > 0):
        return reason
    if not isinstance(stdout, str):
        raise contradiction("the captured stdout is not text")
    try:
        captured = stdout.encode("utf-8")
    except UnicodeEncodeError:
        raise contradiction("the captured stdout is not UTF-8 text") from None
    if (
        len(captured) != observed_bytes
        or hashlib.sha256(captured).hexdigest() != observed_sha256
    ):
        raise contradiction("the captured stdout is not the stdout the reason names")
    if text in stdout:
        raise contradiction("the captured stdout contains the declared text")
    return reason


#: The top-level fields ``VerificationReport::from_producer_json_value``
#: (``hermit-cli/src/canonical_verdict.rs``) requires present, null or not, in
#: every retained cell receipt. Hermit's scorecard reads cell receipts through
#: ``from_retained_cell_json_slice``, which requires these but not
#: ``compared_outputs``.
_RETAINED_REPORT_FIELDS = (
    "verified",
    "bitwise_parity",
    "verdict",
    "infrastructure_error",
    "comparison",
    "no_result_reason",
    "compared_log_messages",
    "guest_exit_code",
    "guest_signal",
    "first_divergent_scheduler_turn",
    "first_divergent_virtual_nanoseconds",
    "first_divergent_record",
    "first_divergent_syscall",
    "first_divergent_left_message",
    "first_divergent_right_message",
)

#: ``ComparisonReport``'s fields, which a current producer writes every one of.
_COMPARISON_FIELDS = (
    "strictness",
    "display_name",
    "compare_logs",
    "compare_io_buffers",
    "log_scope",
    "record_envelope",
    "virtualize_time",
    "strip_lines",
    "canonicalize_addresses",
    "full_trace",
    "exact_remainder",
    "stripped_prefixes",
    "canonicalizations",
    "ignore_lines",
    "skip_commit",
    "skip_detlog",
)


def _is_rust_string(value: Any) -> bool:
    """Whether serde_json reads ``value`` as a Rust ``String``.

    Python's JSON reader accepts an unpaired surrogate escape, which is not
    valid UTF-8 and which serde_json refuses.
    """
    if not isinstance(value, str):
        return False
    try:
        value.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return True


def _is_optional(value: Any, check) -> bool:
    return value is None or check(value)


def _is_str_in(value: Any, choices) -> bool:
    """Whether ``value`` is one of the string ``choices``.

    Row data may put an array or object where a string belongs; a bare ``in``
    against a set would raise ``TypeError`` for those instead of refusing.
    """
    return isinstance(value, str) and value in choices


def _branch_counter_verdict(value: Any) -> dict | None:
    """``BranchCounterVerdict`` (``VerificationReport::exact_branch_counter``,
    hermit-cli/src/canonical_verdict.rs) as serde reads it, normalised to the
    object Rust writes back, or None when serde would refuse it.

    The derived deserializer takes an object with exactly ``present`` and
    ``evidence`` (it denies unknown fields) or the two-element sequence
    ``[present, evidence]``; ``present`` is a bool and ``evidence`` a Rust
    ``String``, so an unpaired surrogate is refused.
    """
    if isinstance(value, dict):
        if set(value) != {"present", "evidence"}:
            return None
        present, evidence = value["present"], value["evidence"]
    elif isinstance(value, list) and len(value) == 2:
        present, evidence = value
    else:
        return None
    if type(present) is not bool or not _is_rust_string(evidence):
        return None
    return {"present": present, "evidence": evidence}


def _is_branch_counter_verdict(value: Any) -> bool:
    return _branch_counter_verdict(value) is not None


def _is_u64_pair(value: Any) -> bool:
    """``ComparedLogMessages`` or ``DbtCountedBranchComparison``: two u64s.

    Neither Rust type denies unknown fields, so other fields are not refused.
    """
    return (
        isinstance(value, dict)
        and "left" in value
        and "right" in value
        and _is_u64(value["left"])
        and _is_u64(value["right"])
    )


def _is_typed_compared_output(output: Any) -> bool:
    """``ComparedOutput`` as serde reads it: exactly its fields, each typed.

    This is the type alone. ``_is_canonical_compared_output`` additionally
    requires the digest shape that ``require_exact_output_match`` checks for a
    matched report.
    """
    return (
        isinstance(output, dict)
        and set(output) == _COMPARED_OUTPUT_FIELDS
        and _is_optional_i32(output["exit_code"])
        and _is_optional_i32(output["signal"])
        and _is_rust_string(output["stdout_sha256"])
        and _is_rust_string(output["stderr_sha256"])
        and _is_u64(output["stdout_bytes"])
        and _is_u64(output["stderr_bytes"])
    )


def _is_typed_comparison(comparison: Any) -> bool:
    """``ComparisonReport`` as serde reads it. It does not deny unknown fields."""
    if not isinstance(comparison, dict):
        return False
    get = comparison.get
    optional_bools = (
        "compare_io_buffers", "virtualize_time", "strip_lines", "canonicalize_addresses",
        "full_trace", "exact_remainder", "ignore_lines", "skip_commit", "skip_detlog",
    )
    return (
        _is_str_in(get("strictness"), {"stripped", "canonical"})
        and _is_optional(get("display_name"), _is_rust_string)
        and type(get("compare_logs")) is bool
        and all(_is_optional(get(key), lambda v: type(v) is bool) for key in optional_bools)
        and _is_optional(
            get("log_scope"),
            lambda v: _is_str_in(v, {"deterministic", "info", "full_trace"}),
        )
        and _is_str_in(get("record_envelope"), {
            "all_records_v1", "dbt_evidence_transport_v1",
            "cross_backend_detcore_v1", "caller_defined",
        })
        and all(
            _is_optional(
                get(key),
                lambda v: isinstance(v, list) and all(_is_rust_string(item) for item in v),
            )
            for key in ("stripped_prefixes", "canonicalizations")
        )
    )


def _require_retained_comparison_report(report: dict, *, index: int) -> None:
    """Refuse a matched or diverged report Hermit's retained-cell reader refuses.

    Mirrors ``VerificationReport::from_retained_cell_json_slice`` in the pinned
    ``hermit-cli/src/canonical_verdict.rs``: every producer field present, the
    comparison's current fields present, and every present field of the type
    serde reads, whether or not a later check here looks at it. A field this
    writer does not otherwise read is still evidence the row's identity digests,
    so a report Hermit cannot parse must not be projected as if it could.
    Unknown top-level fields are not refused, as serde does not refuse them.
    """
    def refuse(detail: str) -> None:
        raise MalformedRow(
            f"cell attempt {index} verification report is not Hermit's typed "
            f"retained report: {detail}"
        )

    for field in _RETAINED_REPORT_FIELDS:
        if field not in report:
            refuse(f"missing producer field {field!r}")
    comparison = report["comparison"]
    if isinstance(comparison, dict):
        for field in _COMPARISON_FIELDS:
            if field not in comparison:
                refuse(f"comparison omitted {field!r}")
    if type(report["verified"]) is not bool or type(report["bitwise_parity"]) is not bool:
        refuse("verified and bitwise_parity must be booleans")
    if report["no_result_reason"] is not None:
        refuse("a comparison verdict carries no_result_reason")
    if report["infrastructure_error"] is not None:
        refuse("a comparison verdict carries infrastructure_error")
    if not _is_typed_comparison(comparison):
        refuse("comparison is not Hermit's typed ComparisonReport")
    if not _is_optional(report["compared_log_messages"], _is_u64_pair):
        refuse("compared_log_messages is not two u64 counts")
    if not _is_optional(report.get("dbt_counted_branches"), _is_u64_pair):
        refuse("dbt_counted_branches is not two u64 counts")
    if not _is_optional(report.get("exact_branch_counter"), _is_branch_counter_verdict):
        refuse("exact_branch_counter is not Hermit's typed branch-counter verdict")
    if not _is_typed_runtime(report.get("runtime")):
        refuse("runtime is not Hermit's typed runtime")
    outputs = report.get("compared_outputs")
    if outputs is not None and not (
        isinstance(outputs, dict)
        and set(outputs) == {"left", "right"}
        and _is_typed_compared_output(outputs["left"])
        and _is_typed_compared_output(outputs["right"])
    ):
        refuse("compared_outputs is not Hermit's typed ComparedOutputs")
    for field in ("guest_exit_code", "guest_signal"):
        if not _is_optional_i32(report[field]):
            refuse(f"{field} is not an optional i32")
    for field in COORDINATE_KEYS:
        if not _is_optional(report[field], _is_u64):
            refuse(f"{field} is not an optional u64")
    for side in FIRST_DIVERGENT_MESSAGE_KEYS:
        field = f"first_divergent_{side}_message"
        if not _is_optional(report[field], _is_rust_string):
            refuse(f"{field} is not an optional string")


def _typed_comparison_attempt(
    attempt: Any,
    *,
    mode: str,
    index: int,
    expected_guest_exit: tuple[int | None, int | None] | None = None,
    declared_stripped: bool = False,
    relaxations: Any = None,
    ineligible_execution_path: bool = False,
) -> str:
    """Return the attempt's typed disposition.

    One of matched, diverged, no_result, timeout, errored, failed_match,
    timed_out_match, stripped_matched, stripped_diverged,
    stripped_failed_match and ineligible_execution_path.

    ``timed_out_match`` is a match from an attempt the runner timed out after
    the report was written (:func:`_is_timed_out_match`), in any comparison
    tier. It is that timeout's evidence: it supports only the result timeout
    or none, and is never canonical, credit or red.

    ``ineligible_execution_path`` is :func:`_ineligible_execution_path` for
    the attempt's row: Hermit's runner passed every attempt and then failed
    the cell because its SaBRe execution path was ineligible. A match that
    would otherwise be ``matched`` or ``stripped_matched`` is then
    ``ineligible_execution_path``: the crash-error red of that execution
    path, never canonical, credit or a comparison verdict.

    ``declared_stripped`` is :func:`_declares_stripped_comparator` for the
    attempt's row and ``relaxations`` the row's relaxations. Every matched or
    diverged report in such a row must be exactly Hermit's stripped verify
    comparison (:func:`_is_stripped_verify_comparison`): the runner ERRORs a
    declared cell on any other (``runner::require_stripped_comparison``). One
    whose I/O buffers were not compared, by its report or by the row's
    ``--no-detlog-io-buffers`` relaxation, is refused
    (:func:`_refuse_stripped_io_buffers_off`). Its
    dispositions are the three ``stripped_*`` ones, which are never canonical
    or bitwise evidence:

    * ``stripped_matched``: verified with ``bitwise_parity`` false, from a
      completed PASS that ended as its row allows. It supports only a pass.
    * ``stripped_diverged``: the runner's "stripped verification did not
      match", an attempt FAIL with no timeout or error kind and a nonzero
      process disposition. It supports only a determinism-failure.
    * ``stripped_failed_match``: a match the runner FAILed for a failed
      declared stdout, or because it did not end as its row allows, exactly
      as ``failed_match``. It is the crash-error red.

    A canonical or bitwise report in a declared row, or a stripped match in
    an undeclared one, contradicts the row and is refused. Without a declared
    guest exit a stripped pass must have exited 0 with a guest that exited 0.

    ``expected_guest_exit`` is the row's validated declaration from
    ``_expected_guest_exit``. Without one, a matched attempt must have exited
    0 (any status in chaos) and not by a signal. With one, a matched attempt
    must end exactly as declared and nothing else. A matched attempt the
    runner FAILed for a failed declared stdout, which
    :func:`_expected_output_failure_reason` verifies, is ``failed_match``
    however it ended: the runner checks declared stdout before the exit. A
    matched attempt the runner failed because it did not end as its row
    allows is ``failed_match`` too. Either is red evidence, never credit.
    """
    if not isinstance(attempt, dict):
        raise MalformedRow(f"cell attempt {index} must be an object")
    raw = attempt.get("verification_report")
    if raw is None:
        if attempt.get("verification_report_sha256") is not None:
            raise MalformedRow(
                f"cell attempt {index} has a verification report hash without a report"
            )
        if (
            attempt.get("outcome") == "ERROR"
            and attempt.get("timed_out") is True
            and isinstance(attempt.get("error_kind"), str)
            and bool(attempt["error_kind"].strip())
            and _has_nonzero_process_disposition(attempt)
        ):
            return "timeout"
        raise MalformedRow(
            f"cell attempt {index} has no verification report and did not time out"
        )
    if not isinstance(raw, str):
        raise MalformedRow(f"cell attempt {index} verification report must be a string")
    recorded_sha = attempt.get("verification_report_sha256")
    if not isinstance(recorded_sha, str) or hashlib.sha256(raw.encode()).hexdigest() != recorded_sha:
        raise MalformedRow(f"cell attempt {index} verification report hash mismatch")
    def unique_fields(pairs):
        fields = {}
        for name, value in pairs:
            if name in fields:
                raise MalformedRow(
                    f"cell attempt {index} verification report has duplicate field {name!r}"
                )
            fields[name] = value
        return fields

    def invalid_constant(value):
        raise MalformedRow(
            f"cell attempt {index} verification report has non-JSON constant {value}"
        )

    def finite_float(text):
        value = float(text)
        if not math.isfinite(value):
            raise MalformedRow(
                f"cell attempt {index} verification report has out-of-range number {text}"
            )
        return value

    try:
        # Read the evidence from the original bytes: an ordinary dict has
        # already collapsed duplicate keys to whichever came last, and Python
        # accepts NaN and Infinity, and parses 1e999 as infinity, none of which
        # Hermit's serde_json reads. Every later reader of this report relies
        # on this parse.
        report = json.loads(
            raw,
            object_pairs_hook=unique_fields,
            parse_constant=invalid_constant,
            parse_float=finite_float,
        )
    except ValueError as error:
        # JSONDecodeError, and an integer past Python's digit limit.
        raise MalformedRow(
            f"cell attempt {index} verification report is malformed: {error}"
        ) from error
    if not isinstance(report, dict):
        raise MalformedRow(f"cell attempt {index} verification report must be an object")
    if not _is_typed_runtime(report.get("runtime")):
        raise MalformedRow(
            f"cell attempt {index} verification report runtime is not Hermit's typed runtime"
        )
    # Hermit's typed reader reads this field for every verdict, so a no-result
    # or infrastructure-error report carrying a malformed one is refused too.
    if not _is_optional(report.get("exact_branch_counter"), _is_branch_counter_verdict):
        raise MalformedRow(
            f"cell attempt {index} verification report exact_branch_counter is not "
            "Hermit's typed branch-counter verdict"
        )

    verdict = report.get("verdict")
    if _is_str_in(verdict, {"matched", "diverged"}):
        if (
            report.get("no_result_reason") is not None
            or report.get("infrastructure_error") is not None
        ):
            raise MalformedRow(
                f"cell attempt {index} comparison report is internally inconsistent"
            )
        comparison = report.get("comparison")
        counts = report.get("compared_log_messages")
        if not isinstance(comparison, dict):
            raise MalformedRow(f"cell attempt {index} has no comparison object")
        if not isinstance(counts, dict):
            raise MalformedRow(f"cell attempt {index} has no INFO-message counts")
        for field in (
            "strictness",
            "display_name",
            "compare_logs",
            "compare_io_buffers",
            "log_scope",
            "record_envelope",
            "virtualize_time",
            "strip_lines",
            "canonicalize_addresses",
            "full_trace",
            "exact_remainder",
            "stripped_prefixes",
            "canonicalizations",
            "ignore_lines",
            "skip_commit",
            "skip_detlog",
        ):
            if field not in comparison:
                raise MalformedRow(
                    f"cell attempt {index} comparison omitted {field!r}"
                )
        left = counts.get("left")
        right = counts.get("right")
        if (
            not isinstance(left, int)
            or isinstance(left, bool)
            or left <= 0
            or not isinstance(right, int)
            or isinstance(right, bool)
            or right <= 0
        ):
            raise MalformedRow(
                f"cell attempt {index} has no nonzero INFO-message counts"
            )
        if not (_is_u64(left) and _is_u64(right)):
            # Hermit's CompareLogMessageCounts are u64s.
            raise MalformedRow(
                f"cell attempt {index} INFO-message counts exceed u64"
            )
        if verdict == "matched" and declared_stripped:
            # Hermit's runner passes a declared stripped cell only on its
            # stripped verify comparison with virtual time over nonzero counts
            # (checked above), and writes that match with bitwise_parity false.
            if (
                report.get("verified") is not True
                or report.get("bitwise_parity") is not False
            ):
                raise MalformedRow(
                    f"cell attempt {index} matched report is internally inconsistent: "
                    "a stripped match must be verified with bitwise_parity false"
                )
        if verdict == "diverged" and (
            report.get("verified") is not False
            or report.get("bitwise_parity") is not False
        ):
            raise MalformedRow(
                f"cell attempt {index} divergence report is internally inconsistent"
            )
        noun = "matched" if verdict == "matched" else "divergence"
        if declared_stripped:
            _refuse_stripped_io_buffers_off(
                comparison, relaxations=relaxations, index=index, noun=noun,
            )
        if declared_stripped and not _is_stripped_verify_comparison(
            comparison, mode=mode
        ):
            raise MalformedRow(
                f"cell attempt {index} {noun} report is internally inconsistent: "
                "its cell declares the stripped comparator, but the report is not "
                "Hermit's stripped verify comparison"
            )
        if verdict == "matched" and declared_stripped:
            # Hermit's log diff reports a match only when both sides compared
            # the same messages, and sets divergence coordinates and messages
            # only when it found a difference.
            if left != right:
                raise MalformedRow(
                    f"cell attempt {index} matched report is internally inconsistent: "
                    "a stripped match compared unequal INFO-message counts"
                )
            divergence_keys = (
                *COORDINATE_KEYS,
                *(f"first_divergent_{side}_message" for side in FIRST_DIVERGENT_MESSAGE_KEYS),
            )
            if any(report.get(key) is not None for key in divergence_keys) or any(
                attempt.get(key) is not None for key in divergence_keys
            ):
                raise MalformedRow(
                    f"cell attempt {index} matched report is internally inconsistent: "
                    "a stripped match carries a divergence coordinate"
                )
        elif verdict == "matched" and (
            report.get("verified") is not True
            or report.get("bitwise_parity") is not True
        ):
            raise MalformedRow(
                f"cell attempt {index} matched report is internally inconsistent"
            )
        canonical = _is_canonical_comparison(comparison, mode=mode)
        if verdict == "matched":
            # Every structural and integrity check refuses before the attempt
            # is classified, canonical or not; only then can a valid match be
            # judged against the exit its row allows.
            guest = _require_matched_evidence(
                attempt,
                report,
                index=index,
                expected_guest_exit=expected_guest_exit,
                stripped=declared_stripped,
                mode=mode,
            )
        # After the specific checks above, so each keeps its own refusal; this
        # refuses every remaining field Hermit's typed reader would refuse.
        _require_retained_comparison_report(report, index=index)
        if verdict == "matched" and _is_timed_out_match(attempt):
            # The runner checks for a timeout before it reads the report and
            # replaces the attempt's reason with the timeout's, so this match,
            # canonical or not, is that timeout. A reason claiming a failed
            # declared stdout contradicts the timeout and is refused here.
            _expected_output_failure_reason(attempt, report, mode=mode, index=index)
            return "timed_out_match"
        if declared_stripped:
            outcome = attempt.get("outcome")
            completed = (
                attempt.get("timed_out") is False and attempt.get("error_kind") is None
            )
            if verdict == "diverged":
                # The runner's only verdict for a declared stripped report that
                # did not match is the attempt FAIL "stripped verification did
                # not match", which leaves the error kind unset; Hermit's
                # verify exits nonzero when it fails, and the shared checker
                # requires a nonzero disposition of a retained divergence.
                if (
                    outcome != "FAIL"
                    or not completed
                    or not _has_nonzero_process_disposition(attempt)
                ):
                    raise MalformedRow(
                        f"cell attempt {index} divergence report is internally inconsistent: "
                        "a stripped divergence must be an attempt the runner FAILed, "
                        "without a timeout or error kind, with a nonzero status or a signal"
                    )
                return "stripped_diverged"
            status = attempt.get("status")
            signal = attempt.get("signal")
            if expected_guest_exit is None:
                # Undeclared, Hermit's verify exits with the guest's status, so
                # a pass is a status 0 over a guest that exited 0, and only a
                # nonzero status or a signal fails a match by its exit.
                ends_as_allowed = (
                    type(status) is int and status == 0 and signal is None
                    and guest == (0, None)
                )
                failed_by_exit = (type(status) is int and status != 0) or signal is not None
            else:
                ends_as_allowed = guest == expected_guest_exit
                failed_by_exit = not ends_as_allowed
            if outcome == "PASS" and completed and ends_as_allowed:
                if ineligible_execution_path:
                    return "ineligible_execution_path"
                return "stripped_matched"
            if outcome == "FAIL" and completed and (
                _expected_output_failure_reason(attempt, report, mode=mode, index=index)
                is not None
                or failed_by_exit
            ):
                # The runner's red for a stripped match, retained like the
                # canonical failed_match: never credit, never refusing its row.
                return "stripped_failed_match"
            raise MalformedRow(
                f"cell attempt {index} matched report is internally inconsistent: "
                "a stripped match must be a completed PASS that ended as its row allows, "
                "or a completed FAIL that its declared stdout or its exit explains"
            )
        if not canonical:
            return "no_result"
        if verdict == "matched":
            if attempt.get("outcome") == "FAIL" and _expected_output_failure_reason(
                attempt, report, mode=mode, index=index
            ) is not None:
                return "failed_match"
            status = attempt.get("status")
            if expected_guest_exit is None:
                disposition_consistent = (
                    type(status) is int
                    and status >= 0
                    and (mode == "chaos" or status == 0)
                    and attempt.get("signal") is None
                )
            else:
                # A replay cell inherits the declaration (see
                # ``_expected_guest_exit``), so it credits it exactly as
                # verify does; any other mode never reaches here with one.
                disposition_consistent = mode in ("verify", "replay") and guest == expected_guest_exit
            if attempt.get("outcome") == "PASS":
                if (
                    attempt.get("timed_out") is not False
                    or attempt.get("error_kind") is not None
                    or not disposition_consistent
                ):
                    raise MalformedRow(
                        f"cell attempt {index} matched report is internally inconsistent"
                    )
                if ineligible_execution_path:
                    return "ineligible_execution_path"
                return "matched"
            # The runner's other red for a match: a declared cell that ended
            # differently, or an undeclared verify that exited nonzero, is an
            # attempt FAIL with no timeout or error kind inside a crash-error
            # row. It earns no credit but is kept as red evidence rather than
            # refusing every sibling, exactly as Hermit's scorecard reader
            # retains it. Only its contradictions refuse: a divergence
            # coordinate or first-divergent message, or an attempt that in
            # fact ended as its row allows.
            if (
                attempt.get("outcome") != "FAIL"
                or attempt.get("timed_out") is not False
                or attempt.get("error_kind") is not None
            ):
                raise MalformedRow(
                    f"cell attempt {index} matched report is internally inconsistent"
                )
            divergence_keys = (
                *COORDINATE_KEYS,
                *(f"first_divergent_{side}_message" for side in FIRST_DIVERGENT_MESSAGE_KEYS),
            )
            if any(report.get(key) is not None for key in divergence_keys) or any(
                attempt.get(key) is not None for key in divergence_keys
            ):
                raise MalformedRow(
                    f"cell attempt {index} matched report the runner failed carries a divergence coordinate"
                )
            if disposition_consistent:
                raise MalformedRow(
                    f"cell attempt {index} matched report is internally inconsistent: "
                    "the runner failed it without a failed declared stdout, but it ends "
                    "as its row allows"
                )
            return "failed_match"
        if report.get("verified") is not False or report.get("bitwise_parity") is not False:
            raise MalformedRow(
                f"cell attempt {index} divergence report is internally inconsistent"
            )
        return "diverged"

    if verdict == "no_result":
        reason = report.get("no_result_reason")
        contradiction_fields = (
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_left_message",
            "first_divergent_right_message",
            "dbt_counted_branches",
        )
        if (
            report.get("verified") is not False
            or report.get("bitwise_parity") is not False
            or report.get("comparison") is not None
            or report.get("compared_log_messages") is not None
            or report.get("infrastructure_error") is not None
            or "no_result_reason" not in report
            or (reason is not None and not isinstance(reason, dict))
            or any(report.get(field) is not None for field in contradiction_fields)
            or any(attempt.get(field) is not None for field in contradiction_fields[:-1])
            or not _is_str_in(attempt.get("outcome"), {"FAIL", "ERROR"})
        ):
            raise MalformedRow(
                f"cell attempt {index} no_result report is internally inconsistent"
            )
        kind = "unspecified" if reason is None else reason.get("kind")
        no_process_timeout = (
            kind == "not_run"
            and attempt.get("timed_out") is True
            and attempt.get("status") is None
            and attempt.get("signal") is None
            and attempt.get("error_kind")
            in ("incomplete-verification-evidence", "cpu-timeout", "wall-timeout")
        )
        if reason is None or kind == "comparison_refused":
            # These current-producer cases distinguish an explicit null cause
            # from missing evidence. Older typed reasons retain their existing
            # historical-reader rules below.
            def current_fields(pairs):
                fields = {}
                for name, value in pairs:
                    if name in fields:
                        raise MalformedRow(
                            f"cell attempt {index} current no_result report has duplicate field {name!r}"
                        )
                    fields[name] = value
                return fields

            # Inspect the original bytes: an ordinary dict has already lost
            # duplicate keys, including contradictory values and nested detail.
            report = json.loads(raw, object_pairs_hook=current_fields)
            reason = report["no_result_reason"]
            for field in (
                "verified", "bitwise_parity", "verdict", "infrastructure_error",
                "comparison", "no_result_reason", "compared_log_messages",
                "guest_exit_code", "guest_signal",
                "first_divergent_scheduler_turn", "first_divergent_virtual_nanoseconds",
                "first_divergent_record", "first_divergent_syscall",
                "first_divergent_left_message", "first_divergent_right_message",
            ):
                if field not in report:
                    raise MalformedRow(
                        f"cell attempt {index} current no_result report omitted {field!r}"
                    )
            for field, minimum in (("guest_exit_code", 0), ("guest_signal", 1)):
                value = report[field]
                if value is not None and (
                    not isinstance(value, int) or isinstance(value, bool)
                    or not minimum <= value <= 2**31 - 1
                ):
                    raise MalformedRow(
                        f"cell attempt {index} current no_result report has invalid {field}"
                    )
            if report["guest_exit_code"] is not None and report["guest_signal"] is not None:
                raise MalformedRow(
                    f"cell attempt {index} current no_result report has contradictory guest disposition"
                )
            if (
                attempt.get("outcome") != "ERROR"
                or attempt.get("timed_out") is not False
                or not isinstance(attempt.get("error_kind"), str)
                or not attempt["error_kind"].strip()
                or not _has_nonzero_process_disposition(attempt)
            ):
                raise MalformedRow(
                    f"cell attempt {index} {kind} disposition is incomplete"
                )
            if kind == "comparison_refused" and (
                not isinstance(reason.get("detail"), str)
                or not reason["detail"].strip()
                or attempt["error_kind"] != "incomplete-verification-evidence"
            ):
                raise MalformedRow(
                    f"cell attempt {index} comparison_refused detail or error kind is incomplete"
                )
        elif kind == "container_failed":
            _validate_container_failed_report(raw, attempt, index)
        elif kind == "not_run":
            if (
                report.get("guest_exit_code") is not None
                or report.get("guest_signal") is not None
                or attempt.get("outcome") != "ERROR"
                or not isinstance(attempt.get("error_kind"), str)
                or not attempt["error_kind"].strip()
                or not (
                    _has_nonzero_process_disposition(attempt)
                    or no_process_timeout
                )
            ):
                raise MalformedRow(
                    f"cell attempt {index} NotRun report has no complete process or pre-launch timeout disposition"
                )
        elif kind == "run_failed":
            # Hermit started verification run `run` and a typed failure of it
            # stopped the run (hermit-cli canonical_verdict RunFailed): the
            # runner's crash-error red, with no guest disposition and Hermit's
            # own nonzero exit.
            if (
                set(reason) != {"kind", "run", "failure", "detail"}
                or not _is_str_in(reason.get("run"), {"run1", "run2"})
                or not _is_str_in(
                    reason.get("failure"),
                    {"backend_failure", "determinism_loss", "policy_refusal", "deadlock"},
                )
                or not isinstance(reason.get("detail"), str)
                or not reason["detail"].strip()
                or report.get("guest_exit_code") is not None
                or report.get("guest_signal") is not None
                or attempt.get("outcome") != "FAIL"
                or attempt.get("timed_out") is not False
                or not isinstance(attempt.get("status"), int)
                or isinstance(attempt.get("status"), bool)
                or attempt["status"] <= 0
                or attempt.get("signal") is not None
                or attempt.get("error_kind") is not None
            ):
                raise MalformedRow(
                    f"cell attempt {index} run_failed disposition is inconsistent"
                )
        elif kind == "first_run_rejected":
            exit_code = reason.get("exit_code")
            signal = reason.get("signal")
            if (
                (exit_code is None) == (signal is None)
                or report.get("guest_exit_code") != exit_code
                or report.get("guest_signal") != signal
                or attempt.get("outcome") != "FAIL"
                or attempt.get("timed_out") is not False
                or not isinstance(attempt.get("status"), int)
                or isinstance(attempt.get("status"), bool)
                or attempt["status"] <= 0
                or attempt.get("signal") is not None
                or attempt.get("error_kind") is not None
            ):
                raise MalformedRow(
                    f"cell attempt {index} first_run_rejected disposition is inconsistent"
                )
        else:
            raise MalformedRow(
                f"cell attempt {index} has unknown no_result reason {kind!r}"
            )
        if attempt.get("timed_out") is True:
            if (
                attempt.get("outcome") != "ERROR"
                or not isinstance(attempt.get("error_kind"), str)
                or not attempt["error_kind"].strip()
                or not (
                    _has_nonzero_process_disposition(attempt)
                    or no_process_timeout
                )
            ):
                raise MalformedRow(
                    f"cell attempt {index} timed-out no_result disposition is incomplete"
                )
            return "timeout"
        return "no_result"
    if verdict == "infrastructure_error":
        if (
            not _is_typed_infrastructure_error(report)
            or attempt.get("outcome") != "ERROR"
            or attempt.get("timed_out") is not False
            or not _has_nonzero_process_disposition(attempt)
        ):
            raise MalformedRow(
                f"cell attempt {index} infrastructure_error report or process disposition is incomplete"
            )
        return "errored"
    raise MalformedRow(f"cell attempt {index} has unknown verdict {verdict!r}")


def _parity_object(value: Any, name: str, required: set[str],
                   optional: set[str] = frozenset(), *, extra: bool = False) -> dict:
    if (not isinstance(value, dict) or not required <= value.keys()
            or (not extra and value.keys() - required - optional)):
        raise MalformedRow(f"backend parity {name} has missing or unknown fields")
    return value


def _parity_uint(value: Any, name: str, minimum: int = 0) -> int:
    if type(value) is not int or not minimum <= value <= 2**64 - 1:
        raise MalformedRow(f"backend parity {name} is not a valid unsigned count")
    return value


def _parity_digest(value: Any, name: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise MalformedRow(f"backend parity {name} is not a SHA-256 digest")
    return value


def _parity_unique_fields(pairs):
    fields = {}
    for key, value in pairs:
        if key in fields:
            raise MalformedRow(f"backend parity evidence has duplicate field {key!r}")
        fields[key] = value
    return fields


def _parity_nonfinite_constant(value):
    raise MalformedRow(f"backend parity evidence contains non-JSON numeric constant {value}")


def _parity_output(value: Any) -> dict:
    output = dict(_parity_object(value, "output", {
        "stdout_sha256", "stdout_bytes",
        "stderr_sha256", "stderr_bytes",
    }, {"exit_code", "signal"}))
    output.setdefault("exit_code", None)
    output.setdefault("signal", None)
    for key in ("exit_code", "signal"):
        number = output[key]
        if number is not None and (type(number) is not int or not -2**31 <= number < 2**31):
            raise MalformedRow(f"backend parity output has invalid {key}")
    if (output["exit_code"] is None) == (output["signal"] is None):
        raise MalformedRow("backend parity output must have exactly one guest disposition")
    for stream in ("stdout", "stderr"):
        _parity_digest(output[f"{stream}_sha256"], f"{stream} digest")
        _parity_uint(output[f"{stream}_bytes"], f"{stream} bytes")
    return output


def _parity_verification(value: Any, *, current: bool) -> dict:
    """Canonical typed serialization of a matched VerificationReport.

    Keep the Rust optional-field serialization rules for evidence identity;
    invocation reports themselves retain their original bytes and digest.
    """
    nullable = {
        "no_result_reason", "infrastructure_error", "guest_exit_code", "guest_signal",
        *COORDINATE_KEYS, "first_divergent_left_message", "first_divergent_right_message",
    }
    required = {"verified", "bitwise_parity", "verdict", "comparison",
                "compared_log_messages", "compared_outputs"}
    value = _parity_object(value, "verification", required | (nullable if current else set()),
                           extra=True)
    report = {key: value.get(key) for key in required | nullable}
    if (report["verified"] is not True or report["bitwise_parity"] is not True
            or report["verdict"] != "matched"
            or report["no_result_reason"] is not None
            or report["infrastructure_error"] is not None
            or any(report[key] is not None for key in
                   (*COORDINATE_KEYS, "first_divergent_left_message", "first_divergent_right_message"))):
        raise MalformedRow("backend parity operand is not a consistent match")
    comparison = _parity_object(report["comparison"], "operand comparison", {
        "strictness", "display_name", "compare_logs", "compare_io_buffers", "log_scope",
        "record_envelope", "virtualize_time", "strip_lines", "canonicalize_addresses",
        "full_trace", "exact_remainder", "stripped_prefixes", "canonicalizations",
        "ignore_lines", "skip_commit", "skip_detlog",
    }, extra=True)
    # These known fields are exactly the ones serialized by ComparisonReport.
    report["comparison"] = {key: comparison[key] for key in (
        "strictness", "display_name", "compare_logs", "compare_io_buffers", "log_scope",
        "record_envelope", "virtualize_time", "strip_lines", "canonicalize_addresses",
        "full_trace", "exact_remainder", "stripped_prefixes", "canonicalizations",
        "ignore_lines", "skip_commit", "skip_detlog",
    )}
    for key in ("compare_logs", "compare_io_buffers", "virtualize_time", "strip_lines",
                "canonicalize_addresses", "full_trace", "exact_remainder", "ignore_lines",
                "skip_commit", "skip_detlog"):
        if type(comparison[key]) is not bool:
            raise MalformedRow(f"backend parity operand comparison has invalid {key}")
    counts = _parity_object(report["compared_log_messages"], "operand counts", {"left", "right"}, extra=True)
    report["compared_log_messages"] = {
        key: _parity_uint(counts[key], f"operand {key} INFO count", 1) for key in ("left", "right")
    }
    outputs = _parity_object(report["compared_outputs"], "compared outputs", {"left", "right"})
    left, right = (_parity_output(outputs[key]) for key in ("left", "right"))
    if (left != right or report["guest_exit_code"] != left["exit_code"]
            or report["guest_signal"] != left["signal"]
            or any(type(report[key]) is not type(left[key.removeprefix("guest_")])
                   for key in ("guest_exit_code", "guest_signal"))):
        raise MalformedRow("backend parity operand outputs or guest disposition disagree")
    report["compared_outputs"] = {"left": left, "right": right}
    branches = value.get("dbt_counted_branches")
    if branches is not None:
        branches = _parity_object(branches, "DBT counts", {"left", "right"}, extra=True)
        report["dbt_counted_branches"] = {
            key: _parity_uint(branches[key], f"DBT {key} count") for key in ("left", "right")
        }
    runtime = value.get("runtime")
    if runtime is not None:
        runtime = _parity_object(runtime, "runtime", set(), {"run1", "run2"}, extra=True)
        report["runtime"] = {}
        for key in ("run1", "run2"):
            if runtime.get(key) is not None:
                stats = _parity_object(runtime[key], key, {"scheduler_turns", "virtual_nanoseconds"},
                                       {"syscalls"}, extra=True)
                report["runtime"][key] = {
                    name: _parity_uint(stats[name], f"{key} {name}")
                    for name in ("scheduler_turns", "virtual_nanoseconds")
                }
                if stats.get("syscalls") is not None:
                    report["runtime"][key]["syscalls"] = _parity_uint(stats["syscalls"], f"{key} syscalls")
    # Hermit's exact_branch_counter verdict is serialized when present and
    # skipped when null, like the optional fields above, so it is part of the
    # Rust evidence identity whenever the operand carries it.
    counter = value.get("exact_branch_counter")
    if counter is not None:
        normalised = _branch_counter_verdict(counter)
        if normalised is None:
            raise MalformedRow("backend parity operand has an invalid exact_branch_counter")
        report["exact_branch_counter"] = normalised
    return report


def _typed_backend_parity(row: dict, dispositions: list[str]) -> dict:
    """Authenticate BackendParityReport, separately from operand repeatability.

    Mirrors backend_parity.rs::validate, LogDiffReport's cross-backend policy,
    and scorecard.rs's attempt/outer-result binding. Returns typed serialization.
    """
    if row.get("mode") != "verify" or dispositions != ["matched", "matched"]:
        raise MalformedRow("backend parity requires exactly two strict matching verify attempts")
    report = copy.deepcopy(_parity_object(row.get("backend_parity"), "report", {
        "schema", "verdict", "reference", "candidate", "comparison",
    }))
    if _parity_uint(report["schema"], "schema") != 1:
        raise MalformedRow("backend parity report schema must be 1")
    backend = row.get("backend")
    if not isinstance(backend, str) or not backend or backend == "ptrace":
        raise MalformedRow("backend parity candidate must be the non-ptrace cell backend")
    for index, label, expected in ((0, "candidate", backend), (1, "reference", "ptrace")):
        operand = _parity_object(report[label], label, {
            "backend", "verification", "output", "retained_log", "retained_log_sha256",
        })
        attempt = row["attempts"][index]
        raw = json.loads(attempt["verification_report"], object_pairs_hook=_parity_unique_fields,
                         parse_constant=_parity_nonfinite_constant)
        actual = _parity_verification(raw, current=True)
        embedded = _parity_verification(operand["verification"], current=False)
        argv = attempt.get("argv")
        if not isinstance(argv, list) or not all(isinstance(arg, str) for arg in argv):
            raise MalformedRow("backend parity attempt has no argv")
        named = next((argv[i + 1] for i, arg in enumerate(argv[:-1]) if arg == "--backend"), None)
        if operand["backend"] != expected or named != expected or embedded != actual:
            raise MalformedRow("backend parity operand does not match its attempt report or backend")
        output = _parity_output(operand["output"])
        if output != actual["compared_outputs"]["left"]:
            raise MalformedRow("backend parity selected output differs from its verified output")
        if not isinstance(operand["retained_log"], str) or not operand["retained_log"].strip():
            raise MalformedRow("backend parity retained log path is empty")
        _parity_digest(operand["retained_log_sha256"], "retained log digest")
        operand["verification"] = embedded
        operand["output"] = output
    coordinates = {*COORDINATE_KEYS, "first_divergent_left_message", "first_divergent_right_message"}
    comparison = _parity_object(report["comparison"], "log comparison", {
        "schema", "verdict", "selected_messages", "records", "inputs", "comparison",
    }, coordinates | {"refusal", "follow_stopped_because"})
    if (_parity_uint(comparison["schema"], "log comparison schema") != 1
            or comparison["verdict"] not in ("matched", "diverged")
            or comparison.get("refusal") is not None
            or comparison.get("follow_stopped_because") is not None):
        raise MalformedRow("backend parity log comparison has no complete verdict")
    policy = {
        "stream": "info", "record_envelope": "cross_backend_detcore_v1",
        "unsafe_strip_lines": False, "canonicalize_host_addresses": True,
        "require_structured_events": True, "ignored_line_substrings": [],
        "skip_commit": False, "skip_detlog": False,
        "included_detlog_kinds": ["syscall", "syscall_result", "other"], "git_diff": False,
    }
    actual_policy = _parity_object(comparison["comparison"], "log policy", set(policy))
    if any(type(actual_policy[key]) is not type(value) or actual_policy[key] != value
           for key, value in policy.items()):
        raise MalformedRow("backend parity log comparison did not use canonical INFO policy")
    inputs = _parity_object(comparison["inputs"], "captured inputs", {"left", "right"})
    selected = _parity_object(comparison["selected_messages"], "selected messages", {"left", "right"})
    records = _parity_object(comparison["records"], "records", {
        "compared", "available_left", "available_right", "withheld_incomplete_tail",
    })
    for side, label in (("left", "reference"), ("right", "candidate")):
        captured = _parity_object(inputs[side], f"{side} input", {"sha256", "bytes"})
        _parity_uint(captured["bytes"], f"{side} input bytes", 1)
        if _parity_digest(captured["sha256"], f"{side} input hash") != report[label]["retained_log_sha256"]:
            raise MalformedRow("backend parity retained log differs from captured input hash")
        count = _parity_uint(selected[side], f"{side} selected messages", 1)
        if count > _parity_uint(records[f"available_{side}"], f"{side} available records", 1):
            raise MalformedRow("backend parity selected messages exceed available records")
    if (records["withheld_incomplete_tail"] is not False
            or _parity_uint(records["compared"], "compared records", 1)
            != min(records["available_left"], records["available_right"])
            or (comparison["verdict"] == "matched" and selected["left"] != selected["right"])):
        raise MalformedRow("backend parity log comparison is incomplete or has inconsistent counts")
    for key in COORDINATE_KEYS:
        if comparison.get(key) is not None:
            _parity_uint(comparison[key], key)
        if row.get(key) is not None:
            _parity_uint(row[key], f"outer {key}")
    for key in ("first_divergent_left_message", "first_divergent_right_message"):
        if comparison.get(key) is not None and not isinstance(comparison[key], str):
            raise MalformedRow("backend parity divergent message must be a string")
    matched = (report["reference"]["output"] == report["candidate"]["output"]
               and comparison["verdict"] == "matched")
    expected = ("matched", "PASS", "pass", None) if matched else (
        "diverged", "FAIL", "parity-failure", "product_failure")
    if ((report["verdict"], row.get("outcome"), row.get("result"), row.get("failure_class")) != expected
            or any(row.get(key) != (None if matched else comparison.get(key)) for key in coordinates)):
        raise MalformedRow("backend parity witness contradicts the outer result or divergence details")
    # Match serde's optional-field serialization, without rewriting retained
    # paths or any comparator evidence using the invocation's workspace root.
    for key in coordinates | {"refusal", "follow_stopped_because"}:
        if key in {"refusal", "follow_stopped_because", "first_divergent_record"}:
            if comparison.get(key) is None:
                comparison.pop(key, None)
        else:
            comparison.setdefault(key, None)
    return report


def _retype_chaos_divergence(row: dict) -> dict:
    """Return ``row``, with a chaos divergence the runner typed crash-error retyped.

    Hermit's runner (``observed_result_from_typed_evidence`` in
    ci/manifest-plan/src/runner.rs) types a FAILed cell with a diverged
    verification report as a determinism-failure in verify mode and a
    replay-failure in replay mode. It has no chaos arm, so a chaos cell in
    which one seed's two runs diverged falls through to crash-error.
    :func:`_cell_outcome` refuses that row, because a crash-error has no
    supporting diverged attempt, and the divergence never reaches the series,
    although Hermit's scorecard already counts the cell as diverged from the
    same attempt reports.

    Exactly that shape gets the determinism-failure the runner writes for the
    same evidence in verify mode: a chaos FAIL typed crash-error with
    failure_class product_failure, no error kind on the row or any attempt, no
    timed-out attempt, no backend parity witness, and attempts that are each a
    canonical match or a canonical divergence, at least one of them diverged.
    The runner already wrote the row's divergence coordinates and messages;
    only ``result`` changes, and it is not part of the row's evidence
    identity. Every other row is returned unchanged, so :func:`_cell_outcome`
    projects or refuses it exactly as received, and a malformed attempt
    raises the refusal it would raise there.
    """
    if (
        row.get("mode") != "chaos"
        or row.get("result") != "crash-error"
        or row.get("failure_class") != "product_failure"
        or row.get("outcome") != "FAIL"
        or row.get("error_kind") is not None
        or row.get("backend_parity") is not None
    ):
        return row
    attempts = row.get("attempts")
    if not isinstance(attempts, list) or not attempts or not all(
        isinstance(attempt, dict)
        and attempt.get("timed_out") is not True
        and attempt.get("error_kind") is None
        for attempt in attempts
    ):
        return row
    expected_guest_exit = _expected_guest_exit(row)
    declared_stripped = _declares_stripped_comparator(row)
    dispositions = [
        _typed_comparison_attempt(
            attempt,
            mode="chaos",
            index=index,
            expected_guest_exit=expected_guest_exit,
            declared_stripped=declared_stripped,
            relaxations=row.get("relaxations"),
        )
        for index, attempt in enumerate(attempts, 1)
    ]
    if "diverged" not in dispositions or not set(dispositions) <= {"matched", "diverged"}:
        return row
    return {**row, "result": "determinism-failure"}


def _noncanonical_divergence(row: dict, dispositions: list[str]) -> bool:
    """Whether ``row`` is a divergence that no canonical comparison saw.

    A verify recipe may set ``compare_io_buffers: false`` with a reason.
    Hermit's runner then passes ``--no-detlog-io-buffers``, records the
    relaxation ``--no-detlog-io-buffers: <reason>``
    (``runner::cell_relaxations`` in ci/manifest-plan/src/runner.rs), and
    writes the report with the canonical comparison but for
    ``compare_io_buffers``, which is false. Its manifest validation refuses
    the setting in every other mode and the flag in ``hermit_args``, and a
    replay cell does not inherit it, so no other cell writes this
    comparison. A verify cell runs one attempt. When that attempt's report
    diverged, the runner FAILs it ("canonical verification did not match:
    verified=false verdict=diverged bitwise_parity=false") over Hermit's
    nonzero exit status, and ``observed_result_from_typed_evidence`` types
    the cell a determinism-failure, whatever the report's comparison.

    The series counts a divergence only from the canonical comparison, so
    :func:`_typed_comparison_attempt` types that attempt ``no_result``, which
    supports no determinism-failure: :func:`_cell_outcome` would refuse the
    row. Hermit's scorecard counts the same cell as unavailable with no
    result (``comparison_evidence_from`` in ci/compat-envelope/scorecard.rs),
    and Hermit's shared checker admits the attempt, as
    ``noncanonical_divergence`` evidence, only in a row with no verdict and
    only from a FAIL with no error kind, no timeout and exactly one nonzero
    status or signal.

    This recognises exactly that row: a verify determinism-failure with
    failure_class product_failure, no error kind, no backend parity witness,
    no declared stripped comparator, and the ``--no-detlog-io-buffers``
    relaxation, whose single attempt is such a FAIL with a diverged report
    whose comparison is the canonical one but for ``compare_io_buffers``
    false. Every other row keeps its refusal. A verified match the runner
    FAILed below bitwise parity is refused before this is asked, as an
    internally inconsistent matched report. The relaxation is not bound to
    the manifest of the row's tree: the cell gets no verdict either way, so
    the row alone chooses only between no result and a refusal.
    """
    if (
        row.get("mode") != "verify"
        or row.get("result") != "determinism-failure"
        or row.get("failure_class") != "product_failure"
        or row.get("outcome") != "FAIL"
        or row.get("error_kind") is not None
        or row.get("backend_parity") is not None
        or _declares_stripped_comparator(row)
        or not _records_io_buffers_relaxation(row.get("relaxations"))
        or dispositions != ["no_result"]
    ):
        return False
    attempts = row.get("attempts")
    if not isinstance(attempts, list) or len(attempts) != 1:
        return False
    attempt = attempts[0]
    if (
        not isinstance(attempt, dict)
        or attempt.get("outcome") != "FAIL"
        or attempt.get("timed_out") is not False
        or attempt.get("error_kind") is not None
        or not _has_nonzero_process_disposition(attempt)
    ):
        return False
    # _typed_comparison_attempt has read and verified this report already.
    report = json.loads(attempt["verification_report"])
    comparison = report.get("comparison")
    return (
        report.get("verdict") == "diverged"
        and isinstance(comparison, dict)
        and comparison.get("compare_io_buffers") is False
        and _is_canonical_comparison({**comparison, "compare_io_buffers": True}, mode="verify")
    )


def _cell_outcome(row: dict) -> str:
    """Project the framework's exact result without reconstructing it."""
    mode = _cell_mode(row)
    expected_guest_exit = _expected_guest_exit(row)
    result = row.get("result")
    failure_class = row.get("failure_class")
    classification = (result, failure_class)
    try:
        projected, allowed_cell_outcomes = {
            ("pass", None): ("passed", {"PASS"}),
            ("determinism-failure", "product_failure"): ("diverged", {"FAIL"}),
            ("parity-failure", "product_failure"): ("diverged", {"FAIL"}),
            ("replay-failure", "product_failure"): ("diverged", {"FAIL"}),
            ("crash-error", "product_failure"): ("errored", {"FAIL"}),
            # Hermit's runner (ObservedResult::ChaosFloorFailure) names a chaos
            # cell whose population floor failed over attempts that all
            # matched, instead of a crash-error no attempt supports;
            # stress_series admits it as errored, product_failure, chaos only.
            ("chaos-floor-failure", "product_failure"): ("errored", {"FAIL"}),
            ("timeout", "no_result"): ("timeout", {"FAIL", "ERROR"}),
            ("oom", "no_result"): ("no_result", {"FAIL"}),
            (None, "understood_infrastructure_failure"): ("errored", {"ERROR"}),
            # Hermit's runner refines a crash or timeout to these when an
            # attempt's output shows an environmental block
            # (runner::observed_result), and Hermit's SeriesPayload admits
            # them as errored with this class (stress_series.rs). Neither is
            # a product verdict.
            ("infrastructure-error", "understood_infrastructure_failure"): (
                "errored", {"FAIL", "ERROR"},
            ),
            ("sandbox-denied", "understood_infrastructure_failure"): (
                "errored", {"FAIL", "ERROR"},
            ),
            (None, "understood_prerequisite_failure"): (
                "skipped", {"ERROR", "HOST-INAPPLICABLE"},
            ),
            (None, "no_result"): ("no_result", {"ERROR"}),
        }[classification]
    except (KeyError, TypeError) as error:
        raise MalformedRow(
            "cell result has no recognised result/failure_class pair: "
            f"result={result!r} failure_class={failure_class!r}"
        ) from error
    outcome = row.get("outcome")
    # Checked as a string first: a list or object here is malformed data, and
    # set membership would otherwise raise TypeError on the unhashable value.
    if not isinstance(outcome, str) or outcome not in allowed_cell_outcomes:
        raise MalformedRow(
            "cell result outcome disagrees with its recorded classification: "
            f"outcome={outcome!r} result={result!r} failure_class={failure_class!r}"
        )
    has_parity = row.get("backend_parity") is not None
    if (has_parity or result == "parity-failure") and mode != "verify":
        raise MalformedRow("backend parity requires verify mode")
    if result == "chaos-floor-failure" and mode != "chaos":
        raise MalformedRow("chaos-floor-failure is valid only on a chaos cell result")
    if mode not in _COMPARISON_MODES:
        return projected

    attempts = row.get("attempts")
    if not isinstance(attempts, list) or not attempts:
        raise _UntypedComparisonCell("comparison cell result has no typed attempts")
    # The framework-owned result above decides the projection. Attempt reports
    # remain independently validated so malformed evidence cannot hide behind
    # a well-formed terminal classification, but an outer reader must not
    # reconstruct a competing verdict from attempt order.
    declared_stripped = _declares_stripped_comparator(row)
    ineligible_path = _ineligible_execution_path(row)
    dispositions = [
        _typed_comparison_attempt(
            attempt,
            mode=mode,
            index=index,
            expected_guest_exit=expected_guest_exit,
            declared_stripped=declared_stripped,
            relaxations=row.get("relaxations"),
            ineligible_execution_path=ineligible_path,
        )
        for index, attempt in enumerate(attempts, 1)
    ]
    if ineligible_path:
        # Hermit's runner fails a cell for its SaBRe execution path only
        # after every attempt passed, so every attempt must be a match that
        # would otherwise be credit, and the row, which did not diverge,
        # carries no divergence coordinate.
        if any(disposition != "ineligible_execution_path" for disposition in dispositions):
            raise MalformedRow(
                "row records a SaBRe execution-path failure, but not every attempt is "
                f"a match the runner passed: dispositions={dispositions!r}"
            )
        if any(row.get(key) is not None for key in COORDINATE_KEYS):
            raise MalformedRow(
                "row recording a SaBRe execution-path failure carries a divergence coordinate"
            )
    if "stripped_matched" in dispositions and classification != ("pass", None):
        # A stripped match is green evidence below bitwise parity. It may
        # support only the pass Hermit's runner writes for it, never a red, a
        # no-result, or a backend parity witness.
        raise MalformedRow(
            f"row retaining a declared stripped match records result {result!r} "
            f"with failure_class {failure_class!r}, but a stripped match supports "
            "only a pass"
        )
    if declared_stripped and classification == ("pass", None) and any(
        disposition != "stripped_matched" for disposition in dispositions
    ):
        # Hermit's scorecard credits a declared cell only when every attempt
        # it retains is a stripped match; one passing attempt cannot erase a
        # sibling divergence, timeout or other disposition from a pass.
        raise MalformedRow(
            "a declared stripped pass requires every attempt to be a stripped match: "
            f"dispositions={dispositions!r}"
        )
    if "stripped_diverged" in dispositions and classification != (
        "determinism-failure", "product_failure",
    ):
        # Hermit's runner types a verify cell with a diverged report as a
        # determinism-failure, and the stripped comparator exists only in
        # verify mode, so a stripped divergence supports nothing else.
        raise MalformedRow(
            f"row retaining a declared stripped divergence records result {result!r} "
            f"with failure_class {failure_class!r}, but a stripped divergence supports "
            "only a determinism-failure"
        )
    if "failed_match" in dispositions or "stripped_failed_match" in dispositions:
        # A retained failed match is the runner's crash-error red. Hermit's
        # runner types a matched attempt it FAILed as crash-error, and the
        # shared failed_match evidence requires exactly that result, so it
        # cannot sit in any other row, or beside a divergence.
        if classification != ("crash-error", "product_failure"):
            raise MalformedRow(
                f"row retaining a matched attempt the runner failed records result {result!r} "
                f"with failure_class {failure_class!r}, but that attempt supports only a "
                "crash-error result"
            )
        if any(row.get(key) is not None for key in COORDINATE_KEYS):
            raise MalformedRow(
                "row retaining a matched attempt the runner failed carries a divergence coordinate"
            )
        if "diverged" in dispositions or "stripped_diverged" in dispositions:
            # crash-error accepts failed_match as support, so a diverged
            # sibling would otherwise be erased from the row's verdict.
            raise MalformedRow(
                "row retaining a matched attempt the runner failed also carries a diverged attempt"
            )
    if "timed_out_match" in dispositions and result not in (
        None, "timeout", "infrastructure-error", "sandbox-denied",
    ):
        # Hermit's shared checker refuses a row whose timed-out attempt sits
        # under any result but timeout or none: a timeout never passes an
        # attempt, so no result that retains one is a pass or a red.
        raise MalformedRow(
            f"row retaining a timed-out matched attempt records result {result!r} "
            f"with failure_class {failure_class!r}, but a timed-out attempt supports "
            "only the result timeout or none"
        )
    if has_parity:
        _typed_backend_parity(row, dispositions)
        return projected
    if result == "parity-failure":
        raise MalformedRow("parity-failure has no backend parity witness")
    # Two framework classifications intentionally project to ``errored`` but
    # arise at different layers.  A crash-error is a product failure before a
    # comparison verdict exists, so its canonical attempt is ``no_result``;
    # only an understood infrastructure failure carries ``errored`` attempt
    # evidence.  Key this obligation by the full classification rather than
    # collapsing it through the outward projection.
    supporting_dispositions = {
        ("pass", None): {"matched"},
        ("determinism-failure", "product_failure"): {"diverged"},
        ("parity-failure", "product_failure"): {"diverged"},
        ("replay-failure", "product_failure"): {"diverged"},
        ("crash-error", "product_failure"): {
            "no_result", "failed_match", "ineligible_execution_path",
        },
        ("chaos-floor-failure", "product_failure"): {"matched"},
        ("timeout", "no_result"): {"timeout", "timed_out_match"},
        ("oom", "no_result"): {"no_result", "timeout"},
        (None, "understood_infrastructure_failure"): {"errored"},
        # The refinement is read from the attempt's output, over an attempt
        # that ended without a comparison verdict.
        ("infrastructure-error", "understood_infrastructure_failure"): {
            "errored", "no_result", "timeout", "timed_out_match",
        },
        ("sandbox-denied", "understood_infrastructure_failure"): {
            "errored", "no_result", "timeout", "timed_out_match",
        },
        (None, "understood_prerequisite_failure"): {
            "no_result", "timeout", "timed_out_match",
        },
        (None, "no_result"): {"no_result", "timeout", "timed_out_match"},
    }[classification]
    if declared_stripped:
        # A declared cell has no canonical comparison: its matched and diverged
        # reports are typed as the stripped dispositions above. Every other
        # supporting disposition is reached without a comparison verdict.
        supporting_dispositions = {
            {
                "matched": "stripped_matched",
                "diverged": "stripped_diverged",
                "failed_match": "stripped_failed_match",
            }.get(disposition, disposition)
            for disposition in supporting_dispositions
        }
    if result == "chaos-floor-failure" and any(
        disposition != "matched" for disposition in dispositions
    ):
        # Hermit assigns it only when every attempt's report matched; any
        # other attempt keeps the runner's own result for that evidence.
        raise MalformedRow(
            "chaos-floor-failure requires every attempt to be a match: "
            f"dispositions={dispositions!r}"
        )
    if not supporting_dispositions.intersection(dispositions):
        if _noncanonical_divergence(row, dispositions):
            # Not a refusal: Hermit's scorecard counts this cell as
            # unavailable with no result, and the caller writes that.
            return "no_result"
        kind = "stripped" if declared_stripped else "canonical"
        raise MalformedRow(
            f"framework result has no supporting {kind} typed attempt: "
            f"projected={projected!r} dispositions={dispositions!r}"
        )
    return projected


def _cell_result_evidence_sha256(row: dict) -> str:
    """Keep ordinary stored identities; bind parity with the scorecard's typed witness."""
    declared = None
    if _expected_guest_exit(row) is not None:
        declared = row["expected_guest_exit"]
    parity = None
    if row.get("backend_parity") is not None:
        parity = _typed_backend_parity(row, [
            _typed_comparison_attempt(
                attempt,
                mode=_cell_mode(row),
                index=index,
                expected_guest_exit=_expected_guest_exit(row),
                declared_stripped=_declares_stripped_comparator(row),
                relaxations=row.get("relaxations"),
            )
            for index, attempt in enumerate(row.get("attempts", []), 1)
        ])
    row = _normalise_cell_result_for_evidence(row)
    evidence = {
        key: row.get(key)
        for key in (
            "run_id",
            "attempt",
            "hermit_sha",
            "binary_sha256",
            "test_sha256",
            "test",
            "category",
            "lane",
            "mode",
            "backend",
            "classification",
            "outcome",
            "log_level",
            "effective_args",
            "argv",
            "guest_argv",
            "env",
            "cwd",
            "shell_command",
            "relaxations",
            "attempts",
        )
    }
    # ResultRow's Rust type defaults an omitted timeout to zero before hashing,
    # and serde_json writes Unicode as UTF-8 rather than ASCII escapes.
    evidence["timeout_seconds"] = row.get("timeout_seconds", 0)
    # Preserve historical ordinary-row identities. Rust also hashes an absent
    # witness as null; that preexisting no-parity mismatch remains explicit.
    if parity is not None:
        evidence["backend_parity"] = parity
    # A declaration authorizes converting a matched nonzero exit from a
    # refusal into a pass, so the complete checked declaration is part of the
    # identity: removing or changing it cannot keep the accepted row's hash.
    # Undeclared rows keep their historical identity, exactly as ResultRow's
    # Rust identity does.
    if declared is not None:
        evidence["expected_guest_exit"] = {
            "code": declared["code"],
            "reason": declared["reason"],
            "signal": declared["signal"],
        }
    encoded = json.dumps(
        evidence, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    )
    try:
        data = encoded.encode()
    except UnicodeEncodeError as error:
        # A lone surrogate: Python accepts it; serde_json refuses it.
        raise MalformedRow(f"cell result is not valid Unicode: {error}") from error
    return hashlib.sha256(data).hexdigest()


def _rewrite_recorded_root(value: str, root: str) -> str:
    """Rewrite producing-root path tokens without aliasing sibling paths."""
    return _replace_evidence_root_token(value, root, RECORDED_ROOT)


def _replace_evidence_root_token(value: str, old: str, new: str) -> str:
    """Replace invocation path tokens, including option and PATH values."""
    def delimiter(character: str) -> bool:
        return character in _EVIDENCE_PATH_DELIMITERS

    start = 0
    pieces = []
    while True:
        found = value.find(old, start)
        if found < 0:
            pieces.append(value[start:])
            return "".join(pieces)
        end = found + len(old)
        preceding = value[found - 1 : found]
        following = value[end : end + 1]
        starts_at_boundary = not preceding or delimiter(preceding)
        ends_at_boundary = (
            not following or following == "/" or delimiter(following)
        )
        if starts_at_boundary and ends_at_boundary:
            pieces.extend((value[start:found], new))
            start = end
        else:
            pieces.append(value[start:end])
            start = end


def _rewrite_recorded_root_json(value: Any, root: str) -> Any:
    if isinstance(value, str):
        return _rewrite_recorded_root(value, root)
    if isinstance(value, list):
        return [_rewrite_recorded_root_json(item, root) for item in value]
    if isinstance(value, dict):
        return {
            key: _rewrite_recorded_root_json(item, root)
            for key, item in value.items()
        }
    return value


def _recorded_shell_quote(value: str) -> str:
    safe = "_@%+=:,./-"
    if value and all(
        character.isascii() and (character.isalnum() or character in safe)
        for character in value
    ):
        return value
    return "'" + value.replace("'", "'\"'\"'") + "'"


def _literal_shell_command(cwd: str, env: dict[str, str], argv: list[str]) -> str:
    words = ["cd", _recorded_shell_quote(cwd), "&&", "env"]
    words.extend(
        _recorded_shell_quote(f"{name}={value}")
        for name, value in sorted(env.items())
    )
    words.extend(_recorded_shell_quote(argument) for argument in argv)
    return " ".join(words)


def _normalise_cell_result_for_evidence(row: dict) -> dict:
    """Mirror scorecard ingestion before computing ResultRow evidence identity."""
    row = copy.deepcopy(row)
    root = row.get("cwd")
    if (
        not isinstance(root, str)
        or not root
        or root == RECORDED_ROOT
        or not root.startswith("/")
    ):
        return row

    for key in ("argv", "guest_argv", "effective_args"):
        values = row.get(key)
        if isinstance(values, list):
            row[key] = [
                _rewrite_recorded_root(value, root)
                if isinstance(value, str)
                else value
                for value in values
            ]
    env = row.get("env")
    if isinstance(env, dict):
        row["env"] = {
            name: _rewrite_recorded_root(value, root)
            if isinstance(value, str)
            else value
            for name, value in env.items()
        }
    attempts = row.get("attempts")
    if isinstance(attempts, list):
        row["attempts"] = [
            _rewrite_recorded_root_json(attempt, root)
            for attempt in attempts
        ]
        for attempt in row["attempts"]:
            if not isinstance(attempt, dict):
                continue
            attempt_cwd = attempt.get("cwd")
            attempt_argv = attempt.get("argv")
            attempt_env = attempt.get("env")
            if (
                isinstance(attempt_cwd, str)
                and isinstance(attempt_argv, list)
                and all(isinstance(value, str) for value in attempt_argv)
                and isinstance(attempt_env, dict)
                and all(
                    isinstance(name, str) and isinstance(value, str)
                    for name, value in attempt_env.items()
                )
            ):
                attempt["shell_command"] = _literal_shell_command(
                    attempt_cwd, attempt_env, attempt_argv
                )
    row["cwd"] = RECORDED_ROOT
    top_env = row.get("env")
    top_argv = row.get("argv")
    if (
        isinstance(top_env, dict)
        and all(
            isinstance(name, str) and isinstance(value, str)
            for name, value in top_env.items()
        )
        and isinstance(top_argv, list)
        and all(isinstance(value, str) for value in top_argv)
    ):
        row["shell_command"] = _literal_shell_command(
            row["cwd"], top_env, top_argv
        )
    return row


def _no_verdict_kind(attempt: dict, disposition: str) -> str:
    """Name the producer-validated reason without reclassifying the result."""
    raw = attempt.get("verification_report")
    if raw is None:
        if disposition != "timeout":
            raise MalformedRow("only a typed timeout may omit its verification report")
        return "missing_report_timeout"
    report = json.loads(raw)
    verdict = report.get("verdict")
    if verdict == "no_result":
        reason = report["no_result_reason"]
        return "unspecified" if reason is None else reason["kind"]
    if verdict == "infrastructure_error":
        return "infrastructure_error"
    if verdict == "matched" and disposition in {"failed_match", "stripped_failed_match"}:
        return "failed_match"
    if verdict == "matched" and disposition == "timed_out_match":
        return "timed_out_match"
    if verdict == "matched" and disposition == "ineligible_execution_path":
        return "ineligible_execution_path"
    if _is_str_in(verdict, {"matched", "diverged"}) and disposition == "no_result":
        return {
            "matched": "noncanonical_match",
            "diverged": "noncanonical_divergence",
        }[verdict]
    raise MalformedRow(
        f"typed {disposition} attempt has unsupported report verdict {verdict!r}"
    )


def _pressure_evidence(row: dict) -> dict | None:
    """Retain the inner history without replacing the framework's verdict.

    A terminal PASS can contain earlier adverse observations. The outward
    series outcome cannot prove first-attempt qualification after that history
    has been discarded. Each evidence object identifies one outer attempt;
    several declared successful inner subruns are valid and are not themselves
    proof that the outer attempt was retried.
    """
    attempts = row.get("attempts")
    if attempts is None or attempts == []:
        # Historical/noncomparison results may lack this evidence. They remain
        # reportable, but the promotion classifier cannot supply missing facts.
        return None
    if not isinstance(attempts, list):
        raise MalformedRow("pressure cell attempts must be an array")
    mode = _cell_mode(row)
    ineligible_path = _ineligible_execution_path(row)
    retained = []
    for index, attempt in enumerate(attempts, 1):
        if not isinstance(attempt, dict):
            raise MalformedRow(f"pressure inner attempt {index} must be an object")
        comparison = None
        if mode in _COMPARISON_MODES:
            disposition = _typed_comparison_attempt(
                attempt,
                mode=mode,
                index=index,
                expected_guest_exit=_expected_guest_exit(row),
                declared_stripped=_declares_stripped_comparator(row),
                relaxations=row.get("relaxations"),
                ineligible_execution_path=ineligible_path,
            )
            # A failed match (the runner failed a matched attempt by its
            # declared exit or stdout) is retained below as a match that is
            # not canonical; the shared checker admits that only from a FAIL
            # attempt with no error kind, and it never stands for a pass.
            raw = attempt.get("verification_report")
            if raw is not None:
                # The existing reader above verifies the report bytes, hash,
                # complete comparison policy and invocation disposition.
                report = json.loads(raw)
                verdict = report["verdict"]
                for name, minimum in (("guest_exit_code", 0), ("guest_signal", 1)):
                    value = report.get(name)
                    if value is not None and (
                        not isinstance(value, int) or isinstance(value, bool)
                        or not minimum <= value <= 2**31 - 1
                    ):
                        raise MalformedRow(f"pressure inner attempt {index} has invalid {name}")
                if report.get("guest_exit_code") is not None and report.get("guest_signal") is not None:
                    raise MalformedRow(f"pressure inner attempt {index} has contradictory guest disposition")
                if verdict == "infrastructure_error" and (
                    not _is_typed_infrastructure_error(report)
                    or (
                        report["infrastructure_error"]["kind"] == "skid_overshoot"
                        and not 0 < report["infrastructure_error"]["count"] < 2**64
                    )
                    or report.get("verified") is not False
                    or report.get("bitwise_parity") is not False
                    or report.get("no_result_reason") is not None
                ):
                    raise MalformedRow(f"pressure inner attempt {index} has inconsistent infrastructure evidence")
                counts = report.get("compared_log_messages")
                if verdict in {"matched", "diverged"} and any(
                    counts[side] >= 2**64 for side in ("left", "right")
                ):
                    raise MalformedRow(f"pressure inner attempt {index} INFO counts exceed u64")
                # A declared stripped match or divergence (``stripped_matched``,
                # ``stripped_diverged``) is never canonical: its tier is below
                # bitwise parity. Nor is a match whose SaBRe execution path the
                # runner failed (``ineligible_execution_path``).
                canonical = disposition in {"matched", "diverged"}
                if verdict == "matched" and counts["left"] != counts["right"]:
                    # Historical projection accepts this shape, but unequal
                    # counts cannot prove a complete canonical match.
                    canonical = False
                comparison = {
                    "verdict": verdict,
                    "canonical": canonical,
                    "report_sha256": attempt["verification_report_sha256"],
                    "no_result_kind": (
                        _no_verdict_kind(attempt, disposition)
                        if verdict == "no_result" else None
                    ),
                }
        retained.append({
            "index": attempt.get("index"),
            "outcome": attempt.get("outcome"),
            "error_kind": attempt.get("error_kind"),
            "status": attempt.get("status"),
            "signal": attempt.get("signal"),
            "timed_out": attempt.get("timed_out"),
            "comparison": comparison,
        })
    return {
        "evidence_sha256": _cell_result_evidence_sha256(row),
        "attempts": retained,
    }


def _declared_guest_exit_evidence(row: dict) -> dict | None:
    """Retain the declaration a verify cell was judged under, for audit.

    ``expected_guest_exit`` is what lets a matched attempt with a nonzero
    guest exit pass instead of being refused. A published row must therefore
    say which declaration applied, bind it to the exact source evidence
    identity (which includes the declaration), and keep each attempt's
    process disposition so a reader can see what was admitted.
    """
    if _expected_guest_exit(row) is None:
        return None
    declared = row["expected_guest_exit"]
    attempts = row.get("attempts")
    if not isinstance(attempts, list) or not attempts:
        raise _UntypedComparisonCell("comparison cell result has no typed attempts")
    retained = []
    for ordinal, attempt in enumerate(attempts, 1):
        if not isinstance(attempt, dict):
            raise MalformedRow(f"cell attempt {ordinal} must be an object")
        index = attempt.get("index")
        outcome = attempt.get("outcome")
        timed_out = attempt.get("timed_out")
        report_sha = attempt.get("verification_report_sha256")
        if not isinstance(index, str) or not index.strip():
            raise MalformedRow(f"cell attempt {ordinal} has no nonempty typed index")
        if not isinstance(outcome, str) or not outcome.strip():
            raise MalformedRow(f"cell attempt {ordinal} has no nonempty outcome")
        if not isinstance(timed_out, bool):
            raise MalformedRow(f"cell attempt {ordinal} timed_out must be a bool")
        if report_sha is not None and (
            not isinstance(report_sha, str) or _SHA256_RE.fullmatch(report_sha) is None
        ):
            raise MalformedRow(
                f"cell attempt {ordinal} verification_report_sha256 must be 64-hex or null"
            )
        for name in ("status", "signal"):
            value = attempt.get(name)
            if value is not None and (type(value) is not int or value < 0):
                raise MalformedRow(
                    f"cell attempt {ordinal} {name} has invalid value {value!r}"
                )
        retained.append({
            "index": index,
            "outcome": outcome,
            "status": attempt.get("status"),
            "signal": attempt.get("signal"),
            "timed_out": timed_out,
            "verification_report_sha256": report_sha,
        })
    return {
        "code": declared["code"],
        "signal": declared["signal"],
        "reason": declared["reason"],
        "evidence_sha256": _cell_result_evidence_sha256(row),
        "attempts": retained,
    }


def _stripped_comparison_evidence(row: dict, projected_outcome: str) -> dict | None:
    """Name the comparison behind a declared stripped verdict; ``None`` otherwise.

    :func:`_cell_outcome` admits a verdict from a comparison in a cell that
    declares the stripped comparator only on Hermit's stripped comparison,
    which is never bitwise parity: a pass whose every attempt is
    ``stripped_matched``, a determinism-failure on a ``stripped_diverged``
    attempt, or a crash-error on a ``stripped_failed_match`` attempt. The
    outward outcome alone would read like a canonical one, so the row records
    that strictness and that it is not bitwise parity. A declared row whose
    verdict no comparison supports (a timeout, a no-result, a crash-error
    before any comparison, or one for its SaBRe execution path after every
    comparison matched), canonical rows, and every other row keep their
    historical payload unchanged.
    """
    if projected_outcome not in {"passed", "diverged", "errored"}:
        return None
    if not _declares_stripped_comparator(row):
        return None
    attempts = row.get("attempts")
    if not isinstance(attempts, list):
        return None
    mode = _cell_mode(row)
    stripped = {"stripped_matched", "stripped_diverged", "stripped_failed_match"}
    ineligible_path = _ineligible_execution_path(row)
    if not any(
        _typed_comparison_attempt(
            attempt,
            mode=mode,
            index=index,
            expected_guest_exit=_expected_guest_exit(row),
            declared_stripped=True,
            relaxations=row.get("relaxations"),
            ineligible_execution_path=ineligible_path,
        ) in stripped
        for index, attempt in enumerate(attempts, 1)
    ):
        return None
    return {"strictness": "stripped", "bitwise_parity": False}


def _no_verdict_evidence(row: dict, projected_outcome: str) -> dict | None:
    """Retain typed non-comparison dispositions without reconstructing one."""
    mode = row.get("mode")
    if not _is_str_in(mode, _COMPARISON_MODES) or not _is_str_in(projected_outcome, {
        "no_result", "timeout", "errored",
    }):
        return None
    attempts = row.get("attempts")
    if not isinstance(attempts, list) or not attempts:
        raise _UntypedComparisonCell("comparison cell result has no typed attempts")

    dispositions = []
    ineligible_path = _ineligible_execution_path(row)
    for ordinal, attempt in enumerate(attempts, 1):
        kind = _typed_comparison_attempt(
            attempt,
            mode=mode,
            index=ordinal,
            expected_guest_exit=_expected_guest_exit(row),
            declared_stripped=_declares_stripped_comparator(row),
            relaxations=row.get("relaxations"),
            ineligible_execution_path=ineligible_path,
        )
        if kind in {"matched", "diverged"}:
            continue
        index = attempt.get("index")
        if not isinstance(index, str) or not index.strip():
            raise MalformedRow(
                f"cell attempt {ordinal} has no nonempty typed index"
            )
        timed_out = attempt.get("timed_out")
        if not isinstance(timed_out, bool):
            raise MalformedRow(
                f"cell attempt {ordinal} timed_out must be a bool"
            )
        error_kind = attempt.get("error_kind")
        if error_kind is not None and (
            not isinstance(error_kind, str) or not error_kind.strip()
        ):
            raise MalformedRow(
                f"cell attempt {ordinal} error_kind must be nonempty or null"
            )
        status = attempt.get("status")
        signal = attempt.get("signal")
        for name, value, minimum in (("status", status, 0), ("signal", signal, 1)):
            if value is not None and (
                not isinstance(value, int)
                or isinstance(value, bool)
                or value < minimum
            ):
                raise MalformedRow(
                    f"cell attempt {ordinal} {name} has invalid value {value!r}"
                )
        if status is not None and signal is not None:
            raise MalformedRow(
                f"cell attempt {ordinal} records both status and signal"
            )
        report_sha = attempt.get("verification_report_sha256")
        if report_sha is not None and (
            not isinstance(report_sha, str)
            or re.fullmatch(r"[0-9a-f]{64}", report_sha) is None
        ):
            raise MalformedRow(
                f"cell attempt {ordinal} has invalid verification report identity"
            )
        retained = {
            "index": index,
            "kind": _no_verdict_kind(attempt, kind),
            "attempt_outcome": attempt.get("outcome"),
            # A failed match, or a match whose SaBRe execution path the
            # runner failed, is the crash-error red, so its disposition is
            # the errored outcome that result projects to; a timed-out match
            # is its timeout.
            "disposition": (
                "errored"
                if kind in {
                    "failed_match", "stripped_failed_match", "ineligible_execution_path",
                }
                else "timeout" if kind == "timed_out_match"
                else kind
            ),
            "error_kind": error_kind,
            "status": status,
            "signal": signal,
            "timed_out": timed_out,
            "verification_report_sha256": report_sha,
        }
        if retained["kind"] == "comparison_refused":
            report = json.loads(attempt["verification_report"])
            retained["detail"] = report["no_result_reason"]["detail"]
        elif retained["kind"] == "failed_match":
            # The runner's reason for a failed declared stdout is the only
            # record of that failure; _typed_comparison_attempt verified it
            # above. Hermit's shared checker admits no other detail: a match
            # the runner failed by its exit is told by its status or signal
            # and the row's declaration.
            report = json.loads(attempt["verification_report"])
            if _expected_output_failure_reason(
                attempt, report, mode=mode, index=ordinal
            ) is not None:
                retained["detail"] = attempt["reason"]
        dispositions.append(retained)
    if not dispositions:
        if projected_outcome == "errored" and row.get("result") == "crash-error":
            return None
        raise MalformedRow(
            "non-comparison cell result has no typed no-verdict attempt"
        )
    return {
        "evidence_sha256": _cell_result_evidence_sha256(row),
        "attempts": dispositions,
    }


def _cell_name(row: dict) -> str:
    test = row.get("test")
    if not isinstance(test, str) or not test:
        raise MalformedRow("cell result has no nonempty test")
    mode = _cell_mode(row)
    parts = [test, mode]
    backend = row.get("backend")
    if backend is None and mode == "naked":
        backend = "native"
    if not isinstance(backend, str) or not backend:
        raise MalformedRow("cell result has no nonempty backend")
    parts.append(backend)
    return "/".join(parts)


def _cell_coordinates(row: dict) -> dict | None:
    located = {}
    attempts = row.get("attempts")
    attempts = attempts if isinstance(attempts, list) else []
    for key in COORDINATE_KEYS:
        value = row.get(key)
        if not isinstance(value, int) or isinstance(value, bool):
            value = next((
                attempt.get(key)
                for attempt in attempts
                if isinstance(attempt, dict)
                and isinstance(attempt.get(key), int)
                and not isinstance(attempt.get(key), bool)
            ), None)
        if isinstance(value, int) and not isinstance(value, bool):
            located[key] = value
    return located or None


def _replace_at_path_boundary(value: str, old: str, new: str) -> str:
    """Replace a known path without consuming a sibling path's prefix."""
    start = 0
    pieces = []
    while True:
        found = value.find(old, start)
        if found < 0:
            pieces.append(value[start:])
            return "".join(pieces)
        end = found + len(old)
        preceding = value[found - 1 : found]
        following = value[end : end + 1]
        before_preceding = value[found - 2 : found - 1]
        opening_quote = preceding in "\"'" and (
            not before_preceding
            or before_preceding.isspace()
            or before_preceding in "=([{,:>"
        )
        starts_at_boundary = (
            not preceding
            or preceding.isspace()
            or preceding in "=([{,:"
            or opening_quote
        )
        ends_at_boundary = not following or following == "/" or (
            opening_quote and following == preceding
        )
        if starts_at_boundary and ends_at_boundary:
            pieces.extend((value[start:found], new))
            start = end
        else:
            pieces.append(value[start:end])
            start = end


def _cell_first_divergent_messages(row: dict) -> dict | None:
    attempts = row.get("attempts")
    attempts = attempts if isinstance(attempts, list) else []
    for candidate in [row, *attempts]:
        if not isinstance(candidate, dict):
            continue
        left = candidate.get("first_divergent_left_message")
        right = candidate.get("first_divergent_right_message")
        left = left if isinstance(left, str) and left else None
        right = right if isinstance(right, str) and right else None
        if left is not None or right is not None:
            messages = {"left": left, "right": right}
            cwd = row.get("cwd")
            if not isinstance(cwd, str) or _OWNER_PATH_NEEDLE not in cwd:
                return messages

            # The retained harness row is the exact local evidence and keeps
            # its absolute paths.  The published series is portable evidence:
            # retain the diagnostic text, but replace the exact execution cwd
            # with the same workspace token used by the validation ledger.
            # Do not infer a broader workspace or owner-home prefix: only the
            # CellResult cwd is producer-owned evidence, and normalizing a
            # sibling path would let an unknown owner path evade the guard.
            messages = {
                side: (
                    _replace_at_path_boundary(value, cwd, WORKSPACE_TOKEN)
                    if value is not None
                    else None
                )
                for side, value in messages.items()
            }
            return messages
    return None


#: The per-run fields of Hermit's shared ``SeriesRuntimeMeasurement``
#: (ci/manifest-plan/src/stress_series.rs). The struct refuses unknown fields.
SERIES_RUNTIME_MEASUREMENT_FIELDS = ("scheduler_turns", "virtual_nanoseconds", "syscalls")

#: Fields copied from run 1 only. Syscall usage counts one canonical execution
#: per cell, so the shared type refuses a histogram on run 2
#: (``SeriesRuntimeMeasurement::syscall_histogram``); the predecessor design is
#: ai_docs/compatibility-website-vision-and-implementation-plan-20260830.md.
SERIES_RUNTIME_RUN1_FIELDS = ("syscall_histogram",)


def _cell_runtime(row: dict) -> dict:
    """Project a cell result's runtime onto the shared ``SeriesRuntime``.

    The row's runtime is built from the fields the shared type declares, and
    from nothing else: ``run1`` and ``run2``, from each measurement object the
    fields in ``SERIES_RUNTIME_MEASUREMENT_FIELDS``, and the two wall times
    from ``duration_ms``. Whatever else Hermit writes into a verification
    runtime is not part of the series row. Copying the runtime verbatim let
    one new producer field refuse every row that carried it: Hermit
    b180b47b49f2 added ``run1.dispatch`` and ``run2.dispatch`` (the reverie
    dispatch counters), the shared ``SeriesRuntimeMeasurement`` refuses
    unknown fields, and 3,493 verify rows of five validate runs were
    quarantined. A field keeps its value, null included, and an absent field
    stays absent. A measurement that is not an object is copied unchanged for
    the shared checker to judge.
    """
    runtime = row.get("runtime")
    if runtime is None:
        runtime = {}
    elif isinstance(runtime, dict):
        projected = {}
        for run in ("run1", "run2"):
            if run not in runtime:
                continue
            measurement = runtime[run]
            if isinstance(measurement, dict):
                fields = SERIES_RUNTIME_MEASUREMENT_FIELDS
                if run == "run1":
                    fields += SERIES_RUNTIME_RUN1_FIELDS
                measurement = {
                    name: measurement[name]
                    for name in fields
                    if name in measurement
                }
            projected[run] = measurement
        runtime = projected
    else:
        raise MalformedRow("cell result runtime must be an object or null")
    wall = row.get("duration_ms")
    if wall is None:
        wall = None
    elif not isinstance(wall, (int, float)) or isinstance(wall, bool) or wall < 0:
        raise MalformedRow(
            f"cell result duration_ms must be a non-negative number or null, got {wall!r}"
        )
    runtime["wall_time_min_ms"] = wall
    runtime["wall_time_max_ms"] = wall
    return runtime


def _compression_key(payload: dict) -> str:
    stable = dict(payload)
    stable.pop("run_index", None)
    stable.pop("last_run_index", None)
    stable.pop("num_runs", None)
    runtime = stable.get("runtime")
    if isinstance(runtime, dict):
        runtime = dict(runtime)
        runtime.pop("wall_time_min_ms", None)
        runtime.pop("wall_time_max_ms", None)
        stable["runtime"] = runtime
    return json.dumps(stable, sort_keys=True, separators=(",", ":"))


def _last_run_index(payload: dict) -> int:
    first = int(payload["run_index"])
    count = int(payload.get("num_runs", 1))
    return int(payload.get("last_run_index", first + count - 1))


def collapse_rows(rows: Sequence[dict]) -> list[dict]:
    """Fold exact equal executions while preserving retry rows and first-seen order."""
    return collapse_rows_with_sources(rows, [[index] for index in range(len(rows))])[0]


def collapse_rows_with_sources(
    rows: Sequence[dict], sources: Sequence[Sequence[int]],
) -> tuple[list[dict], list[list[int]]]:
    """:func:`collapse_rows`, also returning which ``sources`` each output folds.

    ``sources[i]`` names the inputs behind ``rows[i]``; the second result
    names, for each collapsed row, the union of its folded rows' sources.
    """
    if len(sources) != len(rows):
        raise SeriesError("collapse_rows_with_sources needs one source list per row")
    order = sorted(
        range(len(rows)),
        key=lambda index: (
            str((rows[index].get("series") or {}).get("cell", "")),
            int((rows[index].get("series") or {}).get("run_index", -1)),
            int((rows[index].get("series") or {}).get("attempt", 1)),
        ),
    )
    collapsed: list[dict] = []
    merged: list[list[int]] = []
    collapsible: dict[tuple[str, str], int] = {}
    for index in order:
        row = rows[index]
        payload = row["series"]
        key = (str(row.get("run_id", "")), _compression_key(payload))
        previous_index = None if "attempt" in payload else collapsible.get(key)
        if previous_index is not None:
            previous = collapsed[previous_index]
            previous_payload = previous["series"]
            added_runs = int(payload.get("num_runs", 1))
            previous_last = _last_run_index(previous_payload)
            added_last = _last_run_index(payload)
            previous_payload["num_runs"] = (
                int(previous_payload.get("num_runs", 1)) + added_runs
            )
            previous_payload["last_run_index"] = max(previous_last, added_last)
            previous_runtime = previous_payload.get("runtime") or {}
            runtime = payload.get("runtime") or {}
            walls = [
                previous_runtime.get("wall_time_min_ms"),
                previous_runtime.get("wall_time_max_ms"),
                runtime.get("wall_time_min_ms"),
                runtime.get("wall_time_max_ms"),
            ]
            walls = [
                value for value in walls
                if isinstance(value, (int, float)) and not isinstance(value, bool)
            ]
            if walls:
                previous_runtime["wall_time_min_ms"] = min(walls)
                previous_runtime["wall_time_max_ms"] = max(walls)
                previous_payload["runtime"] = previous_runtime
            merged[previous_index].extend(sources[index])
            continue
        collapsed.append(json.loads(json.dumps(row)))
        merged.append(list(sources[index]))
        if "attempt" not in payload:
            collapsible[key] = len(collapsed) - 1
    return collapsed, merged


#: Series fields that describe the writer's view at append time, not the run:
#: whether the tree was on origin/main as the emitting checkout saw it, and the
#: per-repository source depths (Reverie's from its HEAD at that moment). A
#: row's identity (``_series_event_id``) excludes them, so a later replay of
#: the same retained run may see other values. Measured 2026-10-14: pressure
#: run pressure-03c24d69a8e7-20261015T004056Z appended its cells at 01:03:45Z
#: with main_ancestry false; its emit-series retry at 01:04:59Z saw true, and
#: all 8 rows were quarantined as SpoolConflict with no other field changed.
#: ``append_cell_rows`` keeps the recorded values on a replay, as it keeps
#: ``emitted_at``; every other field must still match.
WRITE_TIME_SERIES_FIELDS = ("main_ancestry", "depth")


def _series_event_id(
    run_id: str, producer: str, cell: str, tree: str, run_index: Any, *attempt: Any,
) -> str:
    """A cell row's identity: its run, producer, cell, tree, run index and,
    when the row names one, its attempt. Never its observation time."""
    identity = json.dumps(
        {
            "run_id": run_id,
            "producer": producer,
            "cell": cell,
            "tree": tree,
            "run_index": run_index,
            **({"attempt": attempt[0]} if attempt else {}),
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    return "series-" + hashlib.sha256(identity.encode()).hexdigest()


#: The longest first-difference message a cell row's divergence carries
#: (hermit's ``stress_series::DIVERGENCE_MESSAGE_MAX_CHARS``).
DIVERGENCE_MESSAGE_MAX_CHARS = 240


def _verification_report(cell_result: dict) -> dict | None:
    """The last attempt's verification report of a verify result, or None."""
    attempts = cell_result.get("attempts")
    if not isinstance(attempts, list):
        return None
    for attempt in reversed(attempts):
        if not isinstance(attempt, dict):
            continue
        report = attempt.get("verification_report")
        if isinstance(report, str):
            try:
                report = json.loads(report)
            except ValueError:
                return None
        if isinstance(report, dict):
            return report
    return None


def _output_digest(side: Any) -> dict | None:
    """One run's guest outputs as the cell row's divergence stores them."""
    if not isinstance(side, dict):
        return None
    streams = {}
    for stream in ("stdout", "stderr"):
        sha, size = side.get(f"{stream}_sha256"), side.get(f"{stream}_bytes")
        if (not isinstance(sha, str) or _SHA256_RE.fullmatch(sha) is None
                or type(size) is not int or size < 0):
            return None
        streams[stream] = f"{sha[:16]}:{size}"
    exit_code, signal = side.get("exit_code"), side.get("signal")
    if (exit_code is not None and type(exit_code) is not int) or (
            signal is not None and type(signal) is not int):
        return None
    return {"exit_code": exit_code, "signal": signal, **streams}


def _report_outputs(cell_result: dict) -> tuple[dict | None, dict | None]:
    report = _verification_report(cell_result) or {}
    outputs = report.get("compared_outputs") or {}
    if not isinstance(outputs, dict):
        return None, None
    return _output_digest(outputs.get("left")), _output_digest(outputs.get("right"))


def _divergence_message(value: Any) -> str | None:
    if not isinstance(value, str) or not value:
        return None
    if len(value) <= DIVERGENCE_MESSAGE_MAX_CHARS:
        return value
    return value[:DIVERGENCE_MESSAGE_MAX_CHARS - 1] + "\u2026"


def _divergence_first_difference(record: dict) -> dict | None:
    """A parity comparison's first difference as the cell row keeps it."""
    difference = record.get("first_difference") or {}
    if not isinstance(difference, dict):
        difference = {}
    if record.get("first_divergent_record") is None and not difference:
        return None
    return {
        "record": record.get("first_divergent_record"),
        "syscall": difference.get("syscall"),
        "scheduler_turn": difference.get("scheduler_turn"),
        "virtual_nanoseconds": difference.get("virtual_nanoseconds"),
        "reference": _divergence_message(difference.get("reference_message")),
        "candidate": _divergence_message(difference.get("candidate_message")),
    }


def _divergence_vs_ptrace(record: dict) -> dict:
    """A parity.jsonl record as the cell row's L3: no log paths, short
    messages, and a red candidate's runs against the golden (hermit's
    ``parity::ParityRunComparison``) when the post-pass measured them."""
    vs = {
        key: record.get(key)
        for key in ("verdict", "unavailable_class", "credit", "unequalized_credit",
                    "matched_prefix", "left_len", "right_len")
    }
    first_difference = _divergence_first_difference(record)
    if first_difference is not None:
        vs["first_difference"] = first_difference
    runs = []
    for run in record.get("runs") or []:
        if not isinstance(run, dict):
            continue
        entry = {key: run.get(key)
                 for key in ("run", "verdict", "matched_prefix", "left_len", "right_len")}
        run_difference = _divergence_first_difference(run)
        if run_difference is not None:
            entry["first_difference"] = run_difference
        runs.append(entry)
    if runs:
        vs["runs"] = runs
    return vs


def _cell_divergence(
    cell_result: dict, *, reference_outputs: dict | None, has_reference: bool,
    parity: dict | None,
) -> dict | None:
    """A verify result's first divergence in all three senses (hermit's
    ``stress_series::SeriesDivergence``; dev-hermit
    ai_docs/2026-10-21-cell-row-first-divergence-design.md): both runs' log
    lengths, the guest's outputs against the same run's ptrace row, and the
    run's parity record for the cell. Only what the run recorded is filled."""
    backend = cell_result.get("backend")
    divergence: dict[str, Any] = {}
    report = _verification_report(cell_result) or {}
    lengths = report.get("compared_log_messages") or {}
    if isinstance(lengths, dict) and all(
            type(lengths.get(side)) is int and lengths[side] >= 0 for side in ("left", "right")):
        divergence["self"] = {"left_len": lengths["left"], "right_len": lengths["right"]}
    run1, run2 = _report_outputs(cell_result)
    if run1 is not None:
        outputs: dict[str, Any] = {"run1": run1, "runs_equal": run2 is None or run2 == run1}
        if run2 is not None and run2 != run1:
            outputs["run2"] = run2
        if backend == "ptrace":
            outputs["vs_ptrace"] = "reference"
        elif not has_reference or reference_outputs is None:
            outputs["vs_ptrace"] = "no-reference"
        else:
            differing = [field for field in ("exit_code", "signal", "stdout", "stderr")
                         if run1[field] != reference_outputs[field]]
            outputs["vs_ptrace"] = "differs" if differing else "equal"
            if differing:
                outputs["vs_ptrace_fields"] = differing
        divergence["outputs"] = outputs
    if parity is not None and backend != "ptrace":
        divergence["vs_ptrace"] = _divergence_vs_ptrace(parity)
    return divergence or None


def load_parity_records(paths: Iterable[Any]) -> dict[tuple[str, str], dict]:
    """``(test_id, backend)`` -> the one parity.jsonl record of a run. A key
    recorded more than once (a run's repetitions) is dropped: which record
    belongs to which cell row would be a guess."""
    found: dict[tuple[str, str], list[dict]] = {}
    for path in paths:
        for line in Path(path).read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            record = json.loads(line)
            if not isinstance(record, dict):
                raise SeriesError(f"{path}: a parity record must be an object")
            key = (str(record.get("test_id")), str(record.get("backend")))
            found.setdefault(key, []).append(record)
    return {key: records[0] for key, records in found.items() if len(records) == 1}


def _rows_from_cell_results(
    cell_results: Sequence[dict], *, parent: Any, producer: str, run_id: str,
    tree: str, emitted_at: str, hermit_checkout: Any | None = None,
    quarantine: list[dict] | None, input_numbers: Sequence[int] | None = None,
    raw_inputs: Sequence[bytes] | None = None, provenance: list[list[int]] | None = None,
    write_context: tuple[bool, dict] | None = None,
    parity_records: Mapping[tuple[str, str], dict] | None = None,
) -> list[dict]:
    """Convert typed harness results into validated, compressed series rows.

    With ``quarantine=None`` any refused cell result refuses the batch. With a
    list, each cell result is judged on its own: one the projection refuses as
    malformed data (:class:`MalformedRow`) is recorded in ``quarantine`` with
    its typed reason and its exact received bytes (``raw_inputs``, required
    then), and the independent valid results are still returned. Any other
    exception is a defect in this code, not in the data, and propagates.
    Refusals that concern the invocation rather than any one row (producer,
    run id, tree, source ancestry and depth, the recorded Detcore tree, or a
    checker that cannot run) still raise, because no row could be written
    correctly. ``input_numbers`` names each cell result's original input line;
    by default it is the 1-based position in the batch. ``provenance``, when
    given, receives for each returned row the indices of the cell results it
    folds.

    ``write_context`` is ``(main_ancestry, depth)`` as an earlier append of
    this batch recorded them. Without it both are read from the checkouts now,
    which is what an append does. Replaying refused inputs
    (:func:`readmit_quarantined`) passes the recorded pair when the batch has
    one: the Reverie depth is the sibling checkout's HEAD at write time, so
    reading it again later would give the replayed rows a different depth
    from the rows the same batch admitted when it ran. A batch with no
    recorded pair was never written, and its replay is its first write.
    """
    if input_numbers is not None and len(input_numbers) != len(cell_results):
        raise SeriesError("input_numbers must name every cell result")
    if quarantine is not None and (raw_inputs is None or len(raw_inputs) != len(cell_results)):
        raise SeriesError("quarantining needs the exact received bytes of every cell result")

    def received(index: int) -> dict:
        number = input_numbers[index] if input_numbers is not None else index + 1
        assert raw_inputs is not None
        return quarantine_input(number, raw_inputs[index])
    if producer not in {"validate", "pressure-test", "hermit-repeat"}:
        raise SeriesError(
            "producer must be validate, pressure-test, or hermit-repeat, "
            f"got {producer!r}"
        )
    if not run_id or "/" in run_id or run_id in (".", ".."):
        raise SeriesError(f"run_id must be a single path segment, got {run_id!r}")
    if _SHA_RE.fullmatch(tree) is None:
        raise MalformedRow(f"tree must be a 40-hex commit sha, got {tree!r}")
    source = Path(hermit_checkout) if hermit_checkout is not None else Path(parent) / "hermit"
    if write_context is not None:
        ancestry, depth = write_context
    else:
        ancestry = main_ancestry(source, [tree]).get(tree, (None, None))[0]
        if ancestry is None:
            raise SeriesError(f"cannot resolve whether Hermit {tree} is on origin/main ancestry")
        depth = source_depths(parent, tree, hermit_checkout=source)
    recorded_detcore_tree = None
    verify_declarations: list[tuple[dict, dict]] = []

    def manifest_declarations() -> tuple[dict, dict]:
        # Read once, and only when some row declares a guest exit or the
        # stripped comparator.
        if not verify_declarations:
            verify_declarations.append(manifest_verify_declarations(source, tree))
        return verify_declarations[0]

    def guest_exit_declarations() -> dict:
        return manifest_declarations()[0]

    def stripped_comparator_declarations() -> dict:
        return manifest_declarations()[1]

    # L2's reference: run 1's outputs of each test's ptrace verify row in this
    # batch, per repetition for a repeated producer and, for validate, from
    # its last outer attempt.
    # L3's row: the one whose logs the parity post-pass compared. A
    # validate node's post-pass runs again with each outer attempt, so its
    # parity.jsonl is the last attempt's; a pressure test's compares each
    # cell's first repetition.
    ptrace_outputs: dict[tuple[str, int], dict | None] = {}
    ptrace_attempts: dict[str, int] = {}
    measured_rows: dict[tuple[str, str], int] = {}
    for result in cell_results:
        if not isinstance(result, dict) or result.get("mode") != "verify":
            continue
        position = result.get("attempt" if producer == "validate" else "run_index")
        if type(position) is int:
            cell = (str(result.get("test")), str(result.get("backend")))
            pick = max if producer == "validate" else min
            measured_rows[cell] = pick(measured_rows.get(cell, position), position)
        if result.get("backend") != "ptrace":
            continue
        test = str(result.get("test"))
        if producer == "validate":
            attempt = result.get("attempt") if type(result.get("attempt")) is int else 0
            if attempt < ptrace_attempts.get(test, -1):
                continue
            ptrace_attempts[test] = attempt
            key = (test, 0)
        else:
            key = (test, result.get("run_index") if type(result.get("run_index")) is int else -1)
        ptrace_outputs[key] = _report_outputs(result)[0]

    rows = []
    row_sources: list[list[int]] = []
    pressure_counts: dict[tuple[str, int], int] = {}
    if producer in {"pressure-test", "hermit-repeat"}:
        for cell_result in cell_results:
            if not isinstance(cell_result, dict):
                continue
            if cell_result.get("outcome") == "HOST-INAPPLICABLE":
                continue
            run_index = cell_result.get("run_index")
            if isinstance(run_index, int) and not isinstance(run_index, bool):
                try:
                    cell = _cell_name(cell_result)
                except MalformedRow:
                    if quarantine is None:
                        raise
                    # Quarantined with its exact reason by the loop below.
                    continue
                key = (cell, run_index)
                pressure_counts[key] = pressure_counts.get(key, 0) + 1
    for position, cell_result in enumerate(cell_results, 1):
        # An unmet prerequisite is recorded by the validate-level ledger. It
        # produced no execution and therefore produces no cell observation.
        if isinstance(cell_result, dict) and cell_result.get("outcome") == "HOST-INAPPLICABLE":
            continue
        try:
            if not isinstance(cell_result, dict):
                raise MalformedRow(
                    f"cell result must be an object, got {type(cell_result).__name__}"
                )
            schema = cell_result.get("schema")
            if not isinstance(schema, int) or isinstance(schema, bool) or schema != 4:
                raise MalformedRow(f"cell result schema must be 4, got {schema!r}")
            source_tree_dirty = cell_result.get("source_tree_dirty")
            if not isinstance(source_tree_dirty, bool):
                raise MalformedRow(
                    "cell result source_tree_dirty must be a bool, "
                    f"got {source_tree_dirty!r}"
                )
            if cell_result.get("hermit_sha") != tree:
                raise MalformedRow(
                    f"cell result Hermit sha {cell_result.get('hermit_sha')!r} does not match {tree}"
                )
            machine_shortname = cell_result.get("machine_shortname")
            if not isinstance(machine_shortname, str) or not machine_shortname:
                raise MalformedRow("cell result has no nonempty machine_shortname")
            kernel_version = cell_result.get("kernel_version")
            if not isinstance(kernel_version, str) or not kernel_version:
                raise MalformedRow("cell result has no nonempty kernel_version")
            host_capabilities = cell_result.get("host_capabilities")
            if not isinstance(host_capabilities, dict):
                raise MalformedRow("cell result has no host_capabilities object")
            if producer == "validate":
                run_index = cell_result.get("attempt")
                if (
                    not isinstance(run_index, int)
                    or isinstance(run_index, bool)
                    or run_index < 1
                ):
                    raise MalformedRow(
                        "validate cell result attempt must be a positive int, "
                        f"got {run_index!r}"
                    )
            else:
                run_index = cell_result.get("run_index")
                if (
                    not isinstance(run_index, int)
                    or isinstance(run_index, bool)
                    or run_index < 0
                ):
                    raise MalformedRow(
                        "pressure-test cell result run_index must be a non-negative int, "
                        f"got {run_index!r}"
                    )
                attempt = cell_result.get("attempt", 1)
                if (
                    not isinstance(attempt, int)
                    or isinstance(attempt, bool)
                    or attempt < 1
                ):
                    raise MalformedRow(
                        "pressure-test cell result attempt must be a positive int, "
                        f"got {attempt!r}"
                    )
            detcore_tree = cell_result.get("detcore_tree")
            if detcore_tree is not None and (
                not isinstance(detcore_tree, str)
                or _SHA_RE.fullmatch(detcore_tree) is None
            ):
                raise MalformedRow(
                    "cell result detcore_tree must be a 40-hex object id when present, "
                    f"got {detcore_tree!r}"
                )
            _bind_expected_guest_exit(cell_result, tree, guest_exit_declarations)
            _bind_stripped_comparator(cell_result, tree, stripped_comparator_declarations)
            runtime = _cell_runtime(cell_result)
            # Only the classification is corrected; every piece of evidence
            # below is read from the cell result as received.
            classified = _retype_chaos_divergence(cell_result)
            projected_outcome = _cell_outcome(classified)
            if (
                projected_outcome == "no_result"
                and classified.get("failure_class") == "product_failure"
            ):
                # Only a divergence that no canonical comparison saw projects
                # a product failure to no_result (_noncanonical_divergence):
                # the row records the no-result Hermit's scorecard counts.
                classified = {**classified, "result": None, "failure_class": "no_result"}
            no_verdict_evidence = _no_verdict_evidence(cell_result, projected_outcome)
            payload = {
                "cell": _cell_name(cell_result),
                "tree": tree,
                "run_index": run_index,
                "num_runs": 1,
                "outcome": projected_outcome,
                "result": classified.get("result"),
                "failure_class": classified.get("failure_class"),
                "main_ancestry": ancestry,
                "depth": depth,
                "runtime": runtime,
                "source_tree_dirty": source_tree_dirty,
                "machine_shortname": machine_shortname,
                "kernel_version": kernel_version,
                "host_capabilities": host_capabilities,
            }
            stripped_comparison = _stripped_comparison_evidence(
                cell_result, projected_outcome
            )
            if stripped_comparison is not None:
                payload["comparison"] = stripped_comparison
            if no_verdict_evidence is not None:
                # A validate result's run_index currently carries the outer attempt
                # ordinal too, but naming it explicitly prevents collapse_rows from
                # combining two equal retry rows and makes the identity unambiguous
                # to readers that do not know producer-specific run_index rules.
                payload["attempt"] = run_index if producer == "validate" else attempt
                payload["no_verdict_evidence"] = no_verdict_evidence
            declared_guest_exit = _declared_guest_exit_evidence(cell_result)
            if declared_guest_exit is not None:
                # The audit binds one outer attempt's evidence identity, so the
                # row names that attempt and is never collapsed with another.
                payload["attempt"] = run_index if producer == "validate" else attempt
                payload["declared_guest_exit"] = declared_guest_exit
            # How the cell ran (https://github.com/rrnewton/hermit/issues/4276):
            # the strictness level and the Hermit flags behind it, which the
            # harness writes on each results row. Copied as written when present;
            # a row without it stays without it, and no reader infers it.
            strictness = cell_result.get("strictness")
            if strictness is not None:
                if not isinstance(strictness, dict):
                    raise MalformedRow(f"cell result strictness must be an object, got {strictness!r}")
                payload["strictness"] = strictness
            if cell_result.get("mode") == "verify":
                test, backend = str(cell_result.get("test")), str(cell_result.get("backend"))
                key = (test, 0) if producer == "validate" else (test, run_index)
                measured = measured_rows.get((test, backend)) == run_index
                divergence = _cell_divergence(
                    cell_result,
                    reference_outputs=ptrace_outputs.get(key),
                    has_reference=key in ptrace_outputs,
                    parity=(parity_records or {}).get((test, backend)) if measured else None,
                )
                if divergence is not None:
                    payload["divergence"] = divergence
            if producer == "pressure-test":
                pressure_evidence = _pressure_evidence(cell_result)
                if pressure_evidence is not None:
                    payload["attempt"] = attempt
                    payload["pressure_evidence"] = pressure_evidence
            if detcore_tree is None and no_verdict_evidence is not None:
                if recorded_detcore_tree is None:
                    resolved = _git(source, ["rev-parse", "--verify", f"{tree}:detcore"])
                    recorded_detcore_tree = resolved.stdout.strip()
                    if (
                        resolved.returncode != 0
                        or _SHA_RE.fullmatch(recorded_detcore_tree) is None
                    ):
                        detail = resolved.stderr.strip() or resolved.stdout.strip() or "no output"
                        raise SeriesError(
                            f"cannot resolve Detcore tree recorded by Hermit {tree}: {detail}"
                        )
                detcore_tree = recorded_detcore_tree
            if detcore_tree is not None:
                payload["detcore_tree"] = detcore_tree
            if (
                producer == "pressure-test"
                and pressure_counts.get((payload["cell"], run_index), 0) > 1
            ):
                payload["attempt"] = attempt
            coordinates = _cell_coordinates(cell_result)
            if coordinates is not None and payload["outcome"] == "diverged":
                payload["coordinates"] = coordinates
            messages = _cell_first_divergent_messages(cell_result)
            if messages is not None and payload["outcome"] == "diverged":
                payload["first_divergent_messages"] = messages
            envelope = {
                "schema": ENVELOPE_VERSION,
                "event_id": "",
                "event_type": "series.observation",
                "emitted_at": emitted_at,
                "team": "hermit",
                "host": machine_shortname,
                "run_id": run_id,
                "producer": producer,
                "series": payload,
            }
            rows.append(envelope)
            row_sources.append([position - 1])
        except MalformedRow as error:
            # Only the typed data refusal is quarantined. A TypeError, KeyError,
            # AttributeError or ValueError escaping the projection is a defect
            # in this code; catching it here would file the bug as bad data.
            if quarantine is None:
                raise
            quarantine.append(quarantine_record(
                stage="projection",
                run_id=run_id,
                producer=producer,
                tree=tree,
                emitted_at=emitted_at,
                error=type(error).__name__,
                reason=str(error),
                inputs=[received(position - 1)],
                cell=_quarantine_cell(cell_result),
            ))

    collapsed, collapsed_sources = collapse_rows_with_sources(rows, row_sources)
    for row in collapsed:
        payload = row["series"]
        row["event_id"] = _series_event_id(
            run_id, producer, payload["cell"], payload["tree"], payload["run_index"],
            *([payload["attempt"]] if "attempt" in payload else []),
        )
    if quarantine is None:
        validate_rows(collapsed)
        if provenance is not None:
            provenance.extend(collapsed_sources)
        return collapsed
    admitted = []
    for row, sources, verdict in zip(collapsed, collapsed_sources, write_admission(collapsed)):
        if verdict is None:
            admitted.append(row)
            if provenance is not None:
                provenance.append(list(sources))
            continue
        quarantine.append(quarantine_record(
            stage="shared-checker",
            run_id=run_id,
            producer=producer,
            tree=tree,
            emitted_at=emitted_at,
            error="SharedCheckerRefusal",
            reason=verdict,
            inputs=[received(index) for index in sources],
            row=row,
            row_event_id=str(row.get("event_id", "")),
            cell=_quarantine_cell(row),
        ))
    return admitted


def rows_from_cell_results(
    cell_results: Sequence[dict], *, parent: Any, producer: str, run_id: str,
    tree: str, emitted_at: str, hermit_checkout: Any | None = None,
    parity_records: Mapping[tuple[str, str], dict] | None = None,
) -> list[dict]:
    """Convert a complete typed harness batch or refuse it before writing."""
    return _rows_from_cell_results(
        cell_results,
        parent=parent,
        producer=producer,
        run_id=run_id,
        tree=tree,
        emitted_at=emitted_at,
        hermit_checkout=hermit_checkout,
        quarantine=None,
        parity_records=parity_records,
    )


_LINE_EVENT_ID_RE = re.compile(rb'"event_id":\s*"(series-[0-9a-f]{64})"')
_LINE_RUN_ID_RE = re.compile(rb'"run_id":\s*"((?:[^"\\]|\\.)*)"')


def _ledger_blobs(repo: Path, commit: str, prefix: str, *,
                  files: Any = None) -> Iterable[tuple[str, bytes]]:
    """Yield ``(path, bytes)`` for each ``.jsonl`` blob under ``prefix`` at ``commit``.

    One ``git cat-file --batch`` serves every blob, one at a time, so the whole
    store is never held in memory at once.
    """
    for relative, _oid, data in _read_ledger_blobs(
            repo, commit, _ledger_blob_entries(repo, commit, prefix, files=files), files=files):
        yield relative, data


def _ledger_blob_entries(repo: Path, commit: str, prefix: str,
                         suffix: str = ".jsonl", *, files: Any = None) -> list[tuple[str, str]]:
    """``(path, blob object id)`` of each blob under ``prefix`` at ``commit``
    whose path ends with ``suffix``."""
    if prefix.split("/", 1)[0] in {"series", "quarantine"}:
        with file_archive_scope(repo, commit, files=files) as snapshot:
            return [(path, descriptor.blob_oid)
                    for path, descriptor in snapshot.listing((prefix,)).items()
                    if path.endswith(suffix)]
    listed = _git(repo, ["--no-replace-objects", "ls-tree", "-r", commit, "--", prefix])
    if listed.returncode != 0:
        raise SeriesError(
            f"cannot list {prefix} at {commit}: " + (listed.stderr.strip() or "no output")
        )
    entries = []
    for line in listed.stdout.splitlines():
        meta, _, relative = line.partition("\t")
        fields = meta.split()
        if len(fields) == 3 and fields[1] == "blob" and relative.endswith(suffix):
            entries.append((relative, fields[2]))
    return entries


def _read_ledger_blobs(repo: Path, commit: str, entries: Sequence[tuple[str, str]],
                       *, files: Any = None,
                       ) -> Iterable[tuple[str, str, bytes]]:
    """Yield ``(path, object id, bytes)`` for each of ``entries``, in order,
    through one ``git cat-file --batch``."""
    if not entries:
        return
    if all(path.split("/", 1)[0] in {"series", "quarantine"} for path, _oid in entries):
        with file_archive_scope(repo, commit, files=files) as snapshot:
            for (expected_path, expected_oid), (path, oid, data) in zip(
                    entries, snapshot.iter_blobs([path for path, _oid in entries]), strict=True):
                if (path, oid) != (expected_path, expected_oid):
                    raise SeriesError(f"original file identity changed at {commit}:{expected_path}")
                yield path, oid, data
        return
    env = dict(os.environ)
    for name in _GIT_REPO_SCOPED_ENV:
        env.pop(name, None)
    process = subprocess.Popen(
        ["git", "-C", str(repo), "--no-replace-objects", "cat-file", "--batch"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env,
    )
    assert process.stdin is not None and process.stdout is not None
    try:
        for relative, oid in entries:
            process.stdin.write(oid.encode() + b"\n")
            process.stdin.flush()
            header = process.stdout.readline().split()
            if len(header) != 3 or header[0].decode() != oid or header[1] != b"blob":
                raise SeriesError(f"cannot read {commit}:{relative}: {header!r}")
            size = int(header[2])
            data = process.stdout.read(size)
            process.stdout.read(1)
            if len(data) != size:
                raise SeriesError(f"short read of {commit}:{relative}")
            yield relative, oid, data
    finally:
        process.stdin.close()
        process.stdout.close()
        process.wait(timeout=60)


def _ledger_files(repo: Path, commit: str, paths: Iterable[str], *,
                  files: Any = None) -> dict[str, bytes | None]:
    """The bytes of each of ``paths`` at ``commit``, or None for one it does not hold.

    One ``git cat-file --batch`` reads them all by ``<commit>:<path>``. Only
    quarantine input file paths (:func:`quarantine_input_path`) are read.
    """
    wanted = sorted({path for path in paths
                     if isinstance(path, str) and _QUARANTINE_REF_RE.fullmatch(path)})
    found: dict[str, bytes | None] = {}
    if not wanted:
        return found
    with file_archive_scope(repo, commit, files=files) as snapshot:
        listing = snapshot.listing(("quarantine",))
        found = dict.fromkeys(wanted)
        present = [path for path in wanted if path in listing]
        for path, _oid, data in snapshot.iter_blobs(present):
            found[path] = data
        return found


def _jsonl_lines(label: str, data: bytes) -> list[bytes]:
    if data and not data.endswith(b"\n"):
        raise MalformedEvent(f"{label}: final line has no newline terminator")
    return [line for line in data.split(b"\n") if line.strip()]


#: Prefixes that the owner-home pattern (``_OWNER_HOME_PREFIX_RE``) also
#: replaces but that are no account's home. Hermit's cell runner sets HOME to
#: a ``home`` directory inside the cell directory, so a cell result that names
#: a retained verify log holds the needle followed by ``.local/state/hermit/``,
#: and the pattern replaces the needle plus ``.local`` as if it were an owner
#: home. The ledger keeps only a token's byte length, so a reader that never
#: tries this prefix cannot restore such an input. 18 refusals published from
#: 2026-10-01 to 2026-10-06 restore only with it. (Written with the needle, not
#: literally, for the reason given at ``_OWNER_PATH_NEEDLE``.)
_NON_OWNER_HOME_PREFIXES = (_OWNER_PATH_NEEDLE + ".local",)

#: The most prefix assignments one record's restoration may try
#: (``_quarantined_input_bytes``); each try hashes every input. Only the tokens
#: in an input's text are assigned, and no prefix stands for two tokens, so
#: with this host's candidates (at most three) no record comes near it: at most
#: three tokens can then be assigned, in at most 3**3 = 27 ways. A record that
#: could take more is not tried. The publisher holds its exact copy
#: (``published_quarantine_restores``) and readmission skips it under its own
#: counted reason, so one record cannot stall every publication. Readmission
#: restores every published refusal before each series publication.
QUARANTINE_RESTORE_ATTEMPTS = 64


class QuarantineRestoreLimit(SeriesError):
    """Restoring a record's inputs could take more than QUARANTINE_RESTORE_ATTEMPTS tries."""


def _owner_home_candidates() -> list[str]:
    """The prefixes a published owner-home token may stand for, for restoring tokens.

    The owner-home prefixes of the account running this code, then
    ``_NON_OWNER_HOME_PREFIXES``. Trying a wrong prefix is harmless: a
    restoration counts only when every input's SHA-256 and length match the
    received bytes.
    """
    candidates = [str(Path.home())]
    try:
        import pwd
        candidates.append(_OWNER_PATH_NEEDLE + pwd.getpwuid(os.getuid()).pw_name)
    except (ImportError, KeyError):
        pass
    candidates.extend(_NON_OWNER_HOME_PREFIXES)
    return [c for i, c in enumerate(candidates) if c and c not in candidates[:i]]


def _quarantined_input_bytes(record: Mapping[str, Any]) -> list[tuple[int, bytes]] | None:
    """The exact received bytes of every input of ``record``, or None.

    A published record names each owner-home prefix by a token and its byte
    length. A token is restored only with a prefix of this account whose
    restoration makes every input's SHA-256 and length match the received
    bytes, so a restored input is the received input or nothing.

    Only the tokens in an input's text are assigned: one found only elsewhere
    in the record, such as its row or its error, changes no input. Distinct
    tokens stand for distinct prefixes (``portable_quarantine_record``), so no
    prefix is tried for two tokens, and a record with more tokens than this
    host has prefixes to give them does not restore. Raises
    :class:`QuarantineRestoreLimit`, having tried nothing, when the prefixes
    that fit could be assigned in more than ``QUARANTINE_RESTORE_ATTEMPTS``
    ways.
    """
    inputs = record.get("inputs") or []
    texts = [item["raw"] for item in inputs if item.get("raw") is not None]
    entries = [entry for entry in record.get("owner_paths") or []
               if any(entry["token"] in text for text in texts)]
    tokens = [entry["token"] for entry in entries]
    choices: list[list[str]] = []
    for entry in entries:
        fitting = [c for c in _owner_home_candidates() if len(c.encode()) == entry["bytes"]]
        if not fitting:
            return None
        choices.append(fitting)
    if len(tokens) > len({prefix for fitting in choices for prefix in fitting}):
        return None
    ways = math.prod(len(fitting) for fitting in choices)
    if ways > QUARANTINE_RESTORE_ATTEMPTS:
        raise QuarantineRestoreLimit(
            f"restoring {record.get('event_id')} could take {ways} prefix assignments, "
            f"more than {QUARANTINE_RESTORE_ATTEMPTS}")

    def restore(assignment: Sequence[str]) -> list[tuple[int, bytes]] | None:
        restored = []
        for item in inputs:
            if item.get("raw") is not None:
                text = item["raw"]
                for token, prefix in zip(tokens, assignment):
                    text = text.replace(token, prefix)
                raw = text.encode("utf-8")
            elif item.get("raw_base64") is not None:
                raw = base64.b64decode(item["raw_base64"], validate=True)
            else:
                return None
            if len(raw) != item.get("raw_bytes") or (
                hashlib.sha256(raw).hexdigest() != item.get("raw_sha256")
            ):
                return None
            restored.append((item.get("input_row"), raw))
        return restored

    def assignments(index: int, chosen: list[str]) -> Iterable[list[str]]:
        if index == len(choices):
            yield list(chosen)
            return
        for prefix in choices[index]:
            if prefix not in chosen:
                yield from assignments(index + 1, [*chosen, prefix])

    for assignment in assignments(0, []):
        restored = restore(assignment)
        if restored is not None:
            return restored
    return None


def published_quarantine_restores(public: Mapping[str, Any], exact: Mapping[str, Any]) -> bool:
    """Whether a reader on this host gets back every input ``exact`` received.

    ``exact`` is this host's copy of a refusal and ``public`` its published
    record, expanded (``expand_quarantine_record``). The published inputs must
    restore here (``_quarantined_input_bytes``) to the received inputs, in
    order: the same input rows, SHA-256 digests and lengths. A v1 line
    recorded no received bytes (``upgrade_legacy_quarantine``), so it has
    none to give back. A record whose restoration could take more tries than
    ``QUARANTINE_RESTORE_ATTEMPTS`` does not restore here.
    """
    if exact.get("schema") == LEGACY_QUARANTINE_SCHEMA:
        return True
    try:
        restored = _quarantined_input_bytes(public)
    except (KeyError, TypeError, ValueError, QuarantineRestoreLimit):
        return False
    if restored is None:
        return False
    return [(row, hashlib.sha256(raw).hexdigest(), len(raw)) for row, raw in restored] == [
        (item.get("input_row"), item.get("raw_sha256"), item.get("raw_bytes"))
        for item in exact.get("inputs") or []
    ]


def _replayed_input_counted(
    raw: bytes, run_id: str, producer: str, tree: str, counted: set[str],
) -> bool:
    """Whether the row one refused cell result would make is already counted.

    The row's identity names its attempt only for some outcomes, so both
    forms are checked; one cell attempt has only one row. An input whose
    identity cannot be read here is not counted and is replayed.
    """
    try:
        cell_result = _parse_cell_result_line(raw)
        if not isinstance(cell_result, dict):
            return False
        cell = _cell_name(cell_result)
    except (ValueError, RecursionError, MalformedRow):
        return False
    if producer == "validate":
        run_index = attempt = cell_result.get("attempt")
    else:
        run_index, attempt = cell_result.get("run_index"), cell_result.get("attempt", 1)
    return (
        _series_event_id(run_id, producer, cell, tree, run_index) in counted
        or _series_event_id(run_id, producer, cell, tree, run_index, attempt) in counted
    )


def _replayed_input_event_ids(
    raw: bytes, run_id: str, producer: str, tree: str,
) -> list[str] | None:
    """What :func:`_replayed_input_counted` decides by, apart from ``counted``.

    ``[]`` when the input's identity cannot be read here (never counted), the
    two event ids it checks otherwise, and None when deriving them raises, so
    the caller keeps nothing and asks :func:`_replayed_input_counted` itself.
    """
    try:
        cell_result = _parse_cell_result_line(raw)
        if not isinstance(cell_result, dict):
            return []
        cell = _cell_name(cell_result)
    except (ValueError, RecursionError, MalformedRow):
        return []
    if producer == "validate":
        run_index = attempt = cell_result.get("attempt")
    else:
        run_index, attempt = cell_result.get("run_index"), cell_result.get("attempt", 1)
    try:
        return [_series_event_id(run_id, producer, cell, tree, run_index),
                _series_event_id(run_id, producer, cell, tree, run_index, attempt)]
    except Exception:  # noqa: BLE001 -- the caller then takes the uncached path
        return None


#: The classes of an input that readmission leaves refused (``left_refused``
#: in the :func:`readmission_plan` summary). ``counted`` has its row anyway,
#: and a ``no_attempts`` result records no attempt for a row to describe.
#: ``missing_with_evidence`` records at least one attempt that has no row, and
#: ``unclassified`` could not be read here; the health gate alarms on both.
READMISSION_INPUT_CLASSES = ("counted", "no_attempts", "missing_with_evidence", "unclassified")
#: The most reasons ``left_refused`` names for one class. The rest are summed
#: under one entry, so the summary stays small whatever the refusals say.
READMISSION_REASON_LIMIT = 20
READMISSION_SUMMARY_SCHEMA = "series-readmission-summary/v1"
#: Not ``*.jsonl``: no reader of the quarantine directory takes it for records.
READMISSION_SUMMARY_NAME = "readmission-summary.json"
_READMISSION_HEX_RE = re.compile(r"[0-9a-f]{12,}")
_READMISSION_ATTEMPT_RE = re.compile(r"attempt \d+")
_READMISSION_REASON_CHARS = 150


def _refused_input_class(
    raw: bytes, run_id: str, producer: str, tree: str, counted: set[str],
) -> str:
    """The ``left_refused`` class of one input that readmission did not count.

    ``counted`` when its row is counted anyway (:func:`_replayed_input_counted`),
    ``unclassified`` when it is not a JSON cell result, and otherwise
    ``missing_with_evidence`` when it carries a nonempty ``attempts`` list and
    ``no_attempts`` when it does not.
    """
    if _replayed_input_counted(raw, run_id, producer, tree, counted):
        return "counted"
    try:
        cell_result = _parse_cell_result_line(raw)
    except (ValueError, RecursionError, MalformedRow):
        return "unclassified"
    if not isinstance(cell_result, dict):
        return "unclassified"
    attempts = cell_result.get("attempts")
    if not isinstance(attempts, list) or not attempts:
        return "no_attempts"
    return "missing_with_evidence"


def _readmission_reason(text: Any) -> str:
    """``text`` without the object ids and attempt numbers that differ from one
    refused input to the next, so equal reasons are counted together."""
    text = _READMISSION_HEX_RE.sub("<hex>", str(text))
    return _READMISSION_ATTEMPT_RE.sub("attempt N", text)[:_READMISSION_REASON_CHARS]


def _bounded_reasons(counts: Mapping[str, int]) -> dict[str, int]:
    """The ``READMISSION_REASON_LIMIT`` most frequent reasons, and one total for the rest."""
    ranked = sorted(counts.items(), key=lambda item: (-item[1], item[0]))
    kept = dict(ranked[:READMISSION_REASON_LIMIT])
    rest = ranked[READMISSION_REASON_LIMIT:]
    if rest:
        kept[f"({len(rest)} other reasons)"] = sum(count for _reason, count in rest)
    return kept


def _left_refused_summary(left: Mapping[str, Mapping[str, int]]) -> dict:
    return {
        **{name: sum(left[name].values()) for name in READMISSION_INPUT_CLASSES},
        "reasons": {name: _bounded_reasons(left[name]) for name in READMISSION_INPUT_CLASSES},
    }


#: Reviewed refusals. A refused input that records an attempt and has no row
#: (``missing_with_evidence``) fails the series-quarantine gate until it is
#: re-admitted, or until a reviewer records here that the refusal stands and
#: what stopped its producer. Each line acknowledges one input by its
#: ``raw_sha256``; readmission then counts it as ``acknowledged``, not as a
#: loss. Nothing is withdrawn: the refusal stays published and refused. This is
#: not a ``series.quarantine-retraction``, which asserts that a record should
#: never have been a refusal. A line naming an input that readmission does not
#: leave refused (counted, re-admitted, retracted or never published) is an
#: acknowledgement error, which the gate reports.
QUARANTINE_ACKNOWLEDGEMENT_SCHEMA = "series-quarantine-acknowledgement/v1"
QUARANTINE_ACKNOWLEDGEMENT_EVENT_TYPE = "series.quarantine-acknowledgement"
QUARANTINE_ACKNOWLEDGEMENT_VERDICT = "refusal-stands"
QUARANTINE_ACKNOWLEDGEMENTS_PATH = Path(__file__).resolve().with_name(
    "quarantine-acknowledgements.jsonl")
_QUARANTINE_ACK_FIELDS = {
    "schema", "event_type", "raw_sha256", "quarantine_event_id", "run_id", "cell", "host",
    "verdict", "producer_fix", "evidence", "reviewer", "reviewed_at",
}
_QUARANTINE_EVENT_ID_RE = re.compile(r"series-quarantine-[0-9a-f]{64}")
_QUARANTINE_ACK_TEXT_RE = re.compile(r"[ -~]{1,600}")
_QUARANTINE_ACK_TIME_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
#: How many errors, and how many acknowledged inputs by reason, a summary names.
QUARANTINE_ACK_ERROR_LIMIT = 20


def load_quarantine_acknowledgements(path: Path | None = None) -> dict[str, dict]:
    """The reviewed refusals in ``path`` (default ``QUARANTINE_ACKNOWLEDGEMENTS_PATH``),
    by acknowledged ``raw_sha256``. An absent file acknowledges nothing; a line that
    is not a complete, public acknowledgement, or a second line for one input,
    raises ``SeriesError``, so a gate never reads a half-valid review as clean."""
    path = QUARANTINE_ACKNOWLEDGEMENTS_PATH if path is None else Path(path)
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return {}
    except (OSError, UnicodeDecodeError) as error:
        raise SeriesError(f"cannot read quarantine acknowledgements {path}: {error}") from error
    acks: dict[str, dict] = {}
    for number, line in enumerate(text.split("\n"), 1):
        if not line.strip():
            continue
        where = f"{path.name}:{number}"
        try:
            ack = json.loads(line)
        except (ValueError, RecursionError) as error:
            raise SeriesError(f"{where}: not JSON: {error}") from error
        if not isinstance(ack, dict) or set(ack) != _QUARANTINE_ACK_FIELDS:
            raise SeriesError(f"{where}: an acknowledgement has exactly the fields "
                              f"{sorted(_QUARANTINE_ACK_FIELDS)}")
        if (ack["schema"] != QUARANTINE_ACKNOWLEDGEMENT_SCHEMA
                or ack["event_type"] != QUARANTINE_ACKNOWLEDGEMENT_EVENT_TYPE
                or ack["verdict"] != QUARANTINE_ACKNOWLEDGEMENT_VERDICT):
            raise SeriesError(f"{where}: schema, event_type or verdict is not "
                              f"{QUARANTINE_ACKNOWLEDGEMENT_SCHEMA}, "
                              f"{QUARANTINE_ACKNOWLEDGEMENT_EVENT_TYPE}, {QUARANTINE_ACKNOWLEDGEMENT_VERDICT}")
        if not isinstance(ack["raw_sha256"], str) or _SHA256_RE.fullmatch(ack["raw_sha256"]) is None:
            raise SeriesError(f"{where}: raw_sha256 is not 64 lowercase hex digits")
        if (not isinstance(ack["quarantine_event_id"], str)
                or _QUARANTINE_EVENT_ID_RE.fullmatch(ack["quarantine_event_id"]) is None):
            raise SeriesError(f"{where}: quarantine_event_id is not series-quarantine-<64 hex>")
        if not isinstance(ack["reviewed_at"], str) or _QUARANTINE_ACK_TIME_RE.fullmatch(ack["reviewed_at"]) is None:
            raise SeriesError(f"{where}: reviewed_at is not a UTC time YYYY-MM-DDTHH:MM:SSZ")
        for name in ("run_id", "cell", "host", "producer_fix", "evidence", "reviewer"):
            value = ack[name]
            if (not isinstance(value, str) or _QUARANTINE_ACK_TEXT_RE.fullmatch(value) is None
                    or _OWNER_PATH_NEEDLE in value):
                raise SeriesError(f"{where}: {name} is not 1-600 printable ASCII characters "
                                  "without an owner path")
        if ack["raw_sha256"] in acks:
            raise SeriesError(f"{where}: a second acknowledgement of input {ack['raw_sha256']}")
        acks[ack["raw_sha256"]] = ack
    return acks


def _local_exact_quarantine(parent: Any) -> tuple[dict[str, dict], int]:
    """This host's retained refusals with their exact bytes, by event_id, and
    how many of its v2 records with inputs are not exact copies.

    Only an exact copy (``_exact_quarantine_copy``) is used. A record that does
    not validate under its own ``event_id`` can carry another input's bytes,
    so the published record is replayed in its place.
    """
    directory = quarantine_dir(parent)
    exact: dict[str, dict] = {}
    malformed = 0
    for folder in (directory, directory / "published", directory / QUARANTINE_HELD_SUBDIR):
        if not folder.is_dir():
            continue
        for path in sorted(folder.glob("*.jsonl")):
            for record in _read_quarantine_file(path):
                if (not isinstance(record, dict) or record.get("schema") != QUARANTINE_SCHEMA
                        or not record.get("inputs")):
                    continue
                if not _exact_quarantine_copy(record):
                    malformed += 1
                    continue
                exact.setdefault(record["event_id"], record)
    return exact, malformed


def quarantine_retracted_ids(shards: Iterable[tuple[str, bytes]]) -> dict[str, str]:
    """Each series quarantine record a well-formed `series.quarantine-retraction`
    in ``shards`` (``(path, bytes)`` of ``quarantine/retractions/`` files)
    withdraws, mapped to the retraction's event id.

    The one rule both readers of published refusals use: this module's
    readmission and the website export. A line that is not JSON, or not a
    well-formed retraction (``parity_ledger.validate_retraction``), withdraws
    nothing.
    """
    parity = parity_ledger_module()
    events: list[dict] = []
    for label, data in shards:
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            continue
        for number, line in enumerate(text.split("\n"), 1):
            if not line.strip():
                continue
            try:
                event = parity.loads(line, f"{label}:{number}")
            except (parity.ParityLedgerError, RecursionError):
                continue
            if isinstance(event, dict):
                events.append(event)
    return parity.retracted_events(events, parity.QUARANTINE_RETRACTION_EVENT_TYPE)


def published_quarantine_retractions(repo: Path, commit: str, *, files: Any = None) -> dict[str, str]:
    """``quarantine_retracted_ids`` over the canonical
    ``quarantine/retractions/<team>/<host>/<YYYY-MM>[.part-NNNN].jsonl`` shards at
    ``commit``, the same files the website export captures."""
    parity = parity_ledger_module()
    return quarantine_retracted_ids(
        (relative, data)
        for relative, data in _ledger_blobs(
            repo, commit, parity.QUARANTINE_RETRACTION_ROOT, files=files)
        if parity.is_shard_path(relative)
    )


def readmission_plan(
    parent: Any, *, published_ref: str = "refs/remotes/origin/main",
    host: str | None = None, hermit_checkout: Any | None = None,
    cache_root: Path | None = None,
) -> tuple[str, dict[str, list[dict]], dict]:
    """Rows the current writer makes from the published refused cell results.

    Returns ``(ledger commit, rows by run_id, summary)`` and writes nothing.
    Every refused input published at the resolved ledger commit, by any host,
    or by ``host`` alone when it is given, is replayed through
    :func:`_rows_from_cell_results`, grouped by the batch it
    arrived in (run, producer, tree and observation time), with the batch's
    recorded ``main_ancestry`` and ``depth`` and its recorded ``emitted_at``,
    so an admitted row is the row the writer would have appended when the
    input arrived. The recorded context is the refused row's own (a
    shared-checker refusal records the row), or else that of the rows the
    same batch had admitted. A batch that admitted nothing and recorded no
    row never had a write: its rows are written now, and both values are
    read from the checkouts as any append reads them (the Reverie depth is
    defined as the source state at write time). A batch whose recorded
    contexts disagree is skipped. A row whose event_id is already published
    or spooled is already counted and is dropped; the event_id does not
    include either value, so a later replay cannot count a row twice. Inputs the writer still refuses are left where they are:
    their refusal is already recorded, and no new record is written.

    Every host's refusals are replayed, not only this host's. Replaying only
    the publishing host's own left the refusals of a host that stopped
    publishing uncounted for good, however many of them the writer admits:
    3,136 refused attempts of twelve validate runs whose host stopped
    publishing on 2026-10-03. Another host's bytes are used only when they
    restore exactly (:func:`_quarantined_input_bytes`), and each row keeps the
    host of the run that produced it, so it is published in that host's shard.

    The summary's ``left_refused`` classifies every input the plan leaves
    without a row: those the writer still refuses, those that do not parse,
    those of a skipped batch, and those whose bytes are not readable here.
    It counts each class (``READMISSION_INPUT_CLASSES``) and its most frequent
    reasons. A refusal counted in ``already_counted`` is not in it. The
    health gate reads it (:func:`write_readmission_summary`).

    What it derives from one published blob alone, the series blobs' event
    ids and batch contexts and each published refusal's validation,
    expansion, restoration and row identities, is cached under the blob's
    object id in ``cache_root`` (``readmission_cache.py``); whether a row is
    counted and the replay itself are decided on every run.
    """
    commit, by_run, summary, _published = _readmission_plan(
        parent, published_ref=published_ref, host=host, hermit_checkout=hermit_checkout,
        cache_root=cache_root,
    )
    return commit, by_run, summary


def _series_scan(label: str, data: bytes, runs: set[str]) -> dict:
    """The event ids of the series lines in ``data``, and the batch contexts
    of the runs in ``runs``, which readmission reads from every series blob.

    ``runs`` maps every run id a line names to the distinct ``[batch key,
    context]`` pairs of its rows when it is in ``runs``, and to None when it
    is not, so a cached scan says which runs it would have to be redone for.
    """
    event_ids: list[str] = []
    batches: dict[str, set[str]] = {}
    named: set[str] = set()
    for line in _jsonl_lines(label, data):
        found = _LINE_EVENT_ID_RE.search(line)
        if found is not None:
            event_ids.append(found.group(1).decode())
        run = _LINE_RUN_ID_RE.search(line)
        if run is None:
            continue
        value = run.group(1)
        name = (json.loads(b'"' + value + b'"') if b"\\" in value
                else value.decode("utf-8"))
        named.add(name)
        if name not in runs:
            continue
        row = parse_line(line.decode("utf-8"))
        payload = row.get("series") or {}
        key = [row.get("run_id"), row.get("producer"), payload.get("tree"),
               row.get("emitted_at")]
        context = json.dumps([payload.get("main_ancestry"), payload.get("depth")],
                             sort_keys=True)
        batches.setdefault(name, set()).add(json.dumps([key, context]))
    return {
        "event_ids": event_ids,
        "runs": {name: ([json.loads(pair) for pair in sorted(batches[name])]
                        if name in runs else None)
                 for name in sorted(named)},
    }


def _usable_series_scan(entry: Mapping[str, Any], runs: set[str]) -> bool:
    """Whether a cached :func:`_series_scan` holds the batch contexts of every
    run in ``runs`` that its blob names."""
    if not isinstance(entry.get("event_ids"), list) or not isinstance(entry.get("runs"), dict):
        return False
    return all(isinstance(batches, list)
               for name, batches in entry["runs"].items() if name in runs)


def _readmission_plan(
    parent: Any, *, published_ref: str, host: str | None, hermit_checkout: Any | None,
    cache_root: Path | None = None,
) -> tuple[str, dict[str, list[dict]], dict, set[str] | None]:
    """:func:`readmission_plan`, and the event ids the ledger commit carries
    (``None`` when no refusal is published, as the ledger is then not read)."""
    started = time.monotonic()
    repo = published_repository(parent, LEDGER_REPOSITORY)
    resolved = _git(repo, ["--no-replace-objects", "rev-parse", "--verify",
                           f"{published_ref}^{{commit}}"])
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA_RE.fullmatch(commit) is None:
        raise SeriesError(
            f"cannot resolve published ledger revision {published_ref}: "
            + (resolved.stderr.strip() or "no output")
        )
    # Open/register the storage authorities before cache code_key(), and
    # authenticate operands before any counted/settled/cache-fact shortcut.
    archive_snapshot = file_archive_snapshot(repo, commit)
    try:
        archive_snapshot.verify(archive_snapshot.listing(("series", "quarantine")))
        summary: dict[str, Any] = {
            "ledger_commit": commit, "host": host, "refusals": 0, "retracted": 0, "already_counted": 0,
            "inputs_replayed": 0, "inputs_still_refused": 0, "rows_already_counted": 0,
            "rows": 0, "skipped": {}, "malformed_local_copies": 0,
        }

        def skip(reason: str, count: int = 1) -> None:
            summary["skipped"][reason] = summary["skipped"].get(reason, 0) + count

        left: dict[str, dict[str, int]] = {name: {} for name in READMISSION_INPUT_CLASSES}
        acknowledgements = load_quarantine_acknowledgements()
        acknowledged: dict[str, int] = {}
        acknowledged_digests: set[str] = set()

        def leave(category: str, reason: str, count: int = 1, digest: str | None = None) -> None:
            reason = _readmission_reason(reason)
            # A reviewed refusal is still refused, but no longer an unreviewed loss.
            if digest is not None and digest in acknowledgements and category != "counted":
                acknowledged_digests.add(digest)
                acknowledged[reason] = acknowledged.get(reason, 0) + count
                return
            reasons = left[category]
            reasons[reason] = reasons.get(reason, 0) + count

        def leave_inputs(received: Mapping[tuple, bytes], key: tuple, reason: str) -> None:
            run_id, producer, tree, _emitted_at = key
            for (_input_row, digest), raw in received.items():
                leave(_refused_input_class(raw, run_id, producer, tree, counted), reason,
                      digest=digest)

        def unreadable(record: Mapping[str, Any], reason: str) -> None:
            # Without the bytes nothing says whether an attempt is missing, unless
            # the row the refusal recorded is counted.
            skip(reason)
            inputs = record.get("inputs")
            row_event_id = record.get("row_event_id")
            category = ("counted" if isinstance(row_event_id, str) and row_event_id in counted
                        else "unclassified")
            if isinstance(inputs, list) and inputs:
                for item in inputs:
                    digest = item.get("raw_sha256") if isinstance(item, dict) else None
                    leave(category, reason, digest=digest if isinstance(digest, str) else None)
            else:
                leave(category, reason)

        # Facts derived from one published blob's bytes are cached under its
        # object id (readmission_cache.py); every decision that also depends on
        # what is counted is still made here, on every run.
        cache_module = readmission_cache_module()
        caches = cache_module.open_caches(
            parent, [f"owner-home={json.dumps(_owner_home_candidates())}"], cache_root)
        series_cache, quarantine_cache = caches if caches is not None else (None, None)
        records: list[dict] = []
        # Where each parsed record came from, by id: the record itself (so no id
        # is reused while this runs), its blob's object id and its index there.
        origin: dict[int, tuple[dict, str, int]] = {}
        prefix = "quarantine/series/hermit" + (f"/{host}" if host else "")
        for relative, oid, data in _read_ledger_blobs(
                repo, commit, _ledger_blob_entries(repo, commit, prefix, files=archive_snapshot),
                files=archive_snapshot):
            try:
                text = data.decode("utf-8")
            except UnicodeDecodeError as error:
                raise MalformedEvent(f"{commit}:{relative}: not UTF-8: {error}") from error
            _jsonl_lines(f"{commit}:{relative}", data)
            for index, line in enumerate(line for line in text.split("\n") if line.strip()):
                record = parse_line(line)
                origin[id(record)] = (record, oid, index)
                records.append(record)
        records = [r for r in union_events(records) if r.get("legacy") is None]
        # A record a published quarantine retraction withdraws is not replayed,
        # classified or counted: it should never have been published.
        retracted = published_quarantine_retractions(repo, commit, files=archive_snapshot)
        summary["retracted"] = sum(str(r.get("event_id")) in retracted for r in records)
        records = [r for r in records if str(r.get("event_id")) not in retracted]
        summary["refusals"] = len(records)
        counted: set[str] = set()

        def acknowledgement_summary() -> None:
            summary["acknowledged"] = {"count": sum(acknowledged.values()),
                                       "reasons": _bounded_reasons(acknowledged)}
            # When one host is replayed, another host's acknowledgements are not checked here.
            errors = sorted(digest for digest, ack in acknowledgements.items()
                            if digest not in acknowledged_digests
                            and (host is None or ack["host"] == host))
            summary["acknowledgement_errors"] = {
                "count": len(errors),
                "inputs": [f"{digest} ({acknowledgements[digest]['quarantine_event_id']}): "
                           "readmission does not leave this input refused"
                           for digest in errors[:QUARANTINE_ACK_ERROR_LIMIT]],
            }

        if not records:
            summary["left_refused"] = _left_refused_summary(left)
            acknowledgement_summary()
            summary["seconds"] = round(time.monotonic() - started, 2)
            return commit, {}, summary, None

        needed_runs = {str(r.get("run_id")) for r in records}
        batch_context: dict[tuple, set[str]] = {}

        def fold(scanned: Mapping[str, Any]) -> None:
            counted.update(scanned["event_ids"])
            for run, batches in scanned["runs"].items():
                if run in needed_runs:
                    for key, context in batches:
                        batch_context.setdefault(tuple(key), set()).add(context)

        pending: list[tuple[str, str]] = []
        computed_runs: dict[str, list[str]] = {}
        for relative, oid in _ledger_blob_entries(repo, commit, "series", files=archive_snapshot):
            if len(Path(relative).parts) != 4:
                continue
            entry = series_cache.load(oid) if series_cache is not None else None
            if entry is not None and _usable_series_scan(entry, needed_runs):
                fold(entry)
                continue
            pending.append((relative, oid))
            if entry is not None and isinstance(entry.get("runs"), dict):
                computed_runs[oid] = [run for run, batches in entry["runs"].items()
                                      if isinstance(batches, list)]
        for relative, oid, data in _read_ledger_blobs(repo, commit, pending, files=archive_snapshot):
            scanned = _series_scan(f"{commit}:{relative}", data,
                                   needed_runs.union(computed_runs.get(oid, ())))
            if series_cache is not None:
                series_cache.store(oid, scanned)
            fold(scanned)
        published = set(counted)
        spool = spool_dir(parent)
        for path in sorted(spool.glob("*.jsonl")) + sorted((spool / "published").glob("*.jsonl")):
            fold(_series_scan(str(path), path.read_bytes(), needed_runs))

        exact, summary["malformed_local_copies"] = _local_exact_quarantine(parent)
        entries: dict[str, dict] = {}
        changed: set[str] = set()

        def facts_of(refusal: Mapping[str, Any] | None) -> dict:
            """The cached facts of a published record (empty for a local copy)."""
            if refusal is None or quarantine_cache is None:
                return {}
            _record, oid, index = origin[id(refusal)]
            entry = entries.get(oid)
            if entry is None:
                entry = quarantine_cache.load(oid) or {}
                if not isinstance(entry.get("records"), dict):
                    entry = {"records": {}}
                entries[oid] = entry
            facts = entry["records"].get(str(index))
            return facts if isinstance(facts, dict) else {}

        def learn(refusal: Mapping[str, Any] | None, **facts: Any) -> None:
            if refusal is None or quarantine_cache is None:
                return
            _record, oid, index = origin[id(refusal)]
            facts_of(refusal)
            known = entries[oid]["records"].setdefault(str(index), {})
            if any(known.get(name) != value for name, value in facts.items()):
                known.update(facts)
                changed.add(oid)

        # (record, the published record it is, or None for this host's copy)
        candidates: list[tuple[dict, dict | None]] = []
        for refusal in records:
            # A withheld record is published without its content; this host's
            # retained copy, when it has one, is the exact record.
            local = exact.get(str(refusal.get("event_id")))
            record = local if local is not None else refusal
            if record.get("withheld"):
                unreadable(record, "content withheld and not retained on this host")
                continue
            if record.get("row_event_id") in counted:
                summary["already_counted"] += 1
                continue
            if local is None and record.get("schema") == QUARANTINE_SCHEMA:
                # A local copy was validated when read, and a v3 record is
                # validated when it is expanded. A published v2 record is not
                # expanded: one whose event_id does not bind its inputs would be
                # replayed with whatever bytes it carries. Whether it would
                # publish an owner path does not matter to a replay, so the
                # private check is enough; the public one costs 0.3 ms a record.
                valid = facts_of(refusal).get("valid")
                if not isinstance(valid, bool):
                    try:
                        validate_quarantine_records([record], public=False)
                        valid = True
                    except (LedgerError, TypeError, ValueError):
                        valid = False
                    learn(refusal, valid=valid)
                if not valid:
                    unreadable(record, "published record is not a valid refusal")
                    continue
            candidates.append((record, refusal if local is None else None))

        # A v3 record's input bytes are in their own files at the same commit, so
        # its cached expansion holds only while those files are the same blobs.
        input_oids: dict[str, str] | None = None

        def file_oids(record: Mapping[str, Any]) -> dict[str, str | None]:
            nonlocal input_oids
            if input_oids is None:
                input_oids = {
                    relative: oid for relative, oid
                    in _ledger_blob_entries(repo, commit, QUARANTINE_INPUTS_ROOT,
                                            suffix="", files=archive_snapshot)
                    if _QUARANTINE_REF_RE.fullmatch(relative)
                }
            return {item["raw_ref"]: input_oids.get(item["raw_ref"])
                    for item in record.get("inputs") or []
                    if isinstance(item, dict) and isinstance(item.get("raw_ref"), str)}

        def settled(record: Mapping[str, Any], refusal: Mapping[str, Any] | None) -> str | None:
            """What the cached facts decide for ``record`` without its bytes:
            "missing", "limit", "unrecoverable" or "counted"; None when only the
            bytes can (a replayed member, or facts not cached)."""
            facts = facts_of(refusal)
            if not facts:
                return None
            if record.get("schema") == QUARANTINE_REF_SCHEMA:
                if facts.get("files") != file_oids(record) or not isinstance(
                        facts.get("expanded"), bool):
                    return None
                if not facts["expanded"]:
                    return "missing"
            restore = facts.get("restore")
            if restore == "limit":
                return "limit"
            if restore == "none":
                return "unrecoverable"
            identities = facts.get("identities")
            if (record.get("row_event_id") is None and restore == 1
                    and isinstance(identities, list)
                    and any(identity in counted for identity in identities)):
                return "counted"
            return None

        decided = [settled(record, refusal) for record, refusal in candidates]
        files = _ledger_files(repo, commit, (
            item.get("raw_ref")
            for (record, _refusal), decision in zip(candidates, decided) if decision is None
            if record.get("schema") == QUARANTINE_REF_SCHEMA
            for item in record.get("inputs") or [] if isinstance(item, dict)
        ), files=archive_snapshot)
        groups: dict[tuple, list[tuple[dict, dict | None, str | None]]] = {}
        for (record, refusal), decision in zip(candidates, decided):
            if decision == "missing":
                unreadable(record, "published input file missing or not matching its record")
                continue
            if decision is None:
                try:
                    expanded = expand_quarantine_record(record, files)
                except MalformedQuarantine:
                    if record.get("schema") == QUARANTINE_REF_SCHEMA:
                        learn(refusal, files=file_oids(record), expanded=False)
                    unreadable(record, "published input file missing or not matching its record")
                    continue
                if record.get("schema") == QUARANTINE_REF_SCHEMA:
                    learn(refusal, files=file_oids(record), expanded=True)
                record = expanded
            key = (record.get("run_id"), record.get("producer"), record.get("tree"),
                   record.get("emitted_at"))
            groups.setdefault(key, []).append((record, refusal, decision))

        source = Path(hermit_checkout) if hermit_checkout is not None else Path(parent) / "hermit"
        by_run: dict[str, list[dict]] = {}
        planned: set[str] = set()
        for key in sorted(groups, key=lambda k: tuple(str(part) for part in k)):
            run_id, producer, tree, emitted_at = key
            members = []
            received: dict[tuple[int, str], bytes] = {}
            for record, refusal, decision in groups[key]:
                if decision is None:
                    try:
                        restored = _quarantined_input_bytes(record)
                    except QuarantineRestoreLimit:
                        learn(refusal, restore="limit")
                        decision = "limit"
                    else:
                        learn(refusal, restore="none" if restored is None else len(restored))
                        if restored is None:
                            decision = "unrecoverable"
                if decision == "limit":
                    unreadable(record, "restoring the exact input bytes could take more than "
                               f"{QUARANTINE_RESTORE_ATTEMPTS} prefix assignments")
                    continue
                if decision == "unrecoverable":
                    unreadable(record, "exact input bytes not recoverable on this host")
                    continue
                if decision is None and record.get("row_event_id") is None and len(restored) == 1:
                    identities = _replayed_input_event_ids(restored[0][1], run_id, producer, tree)
                    if identities is None:
                        if _replayed_input_counted(restored[0][1], run_id, producer, tree, counted):
                            decision = "counted"
                    else:
                        learn(refusal, identities=identities)
                        if any(identity in counted for identity in identities):
                            decision = "counted"
                if decision == "counted":
                    # Refused before it had a row, and its row has been counted
                    # since. Saves projecting it again on every publication.
                    summary["already_counted"] += 1
                    continue
                members.append(record)
                for input_row, raw in restored:
                    received[(input_row, hashlib.sha256(raw).hexdigest())] = raw
            if not members:
                continue
            contexts = {
                json.dumps([r["row"]["series"].get("main_ancestry"),
                            r["row"]["series"].get("depth")], sort_keys=True)
                for r in members
                if isinstance(r.get("row"), dict) and isinstance(r["row"].get("series"), dict)
            } or batch_context.get(key, set())
            if len(contexts) > 1:
                skip("recorded write-time contexts disagree", len(members))
                leave_inputs(received, key, "recorded write-time contexts disagree")
                continue
            # No recorded context: nothing from this batch was ever written, so
            # this is its first write and the writer measures the context now.
            write_context = tuple(json.loads(next(iter(contexts)))) if contexts else None
            numbers: list[int] = []
            raws: list[bytes] = []
            values: list[Any] = []
            unparsed: list[tuple[str, str]] = []
            for (input_row, digest), raw in sorted(received.items(),
                                                   key=lambda item: (item[0][0] or 0, item[0][1])):
                try:
                    values.append(_parse_cell_result_line(raw))
                except (ValueError, RecursionError, MalformedRow) as error:
                    summary["inputs_still_refused"] += 1
                    unparsed.append(
                        (f"the input is not a JSON cell result ({type(error).__name__})", digest))
                    continue
                numbers.append(input_row)
                raws.append(raw)
            summary["inputs_replayed"] += len(received)
            refused: list[dict] = []
            try:
                rows = _rows_from_cell_results(
                    values, parent=parent, producer=producer, run_id=run_id, tree=tree,
                    emitted_at=emitted_at, hermit_checkout=source, quarantine=refused,
                    input_numbers=numbers, raw_inputs=raws, write_context=write_context,
                )
            except SeriesError as error:
                skip(f"the writer refuses the batch: {error}", len(members))
                leave_inputs(received, key, f"the writer refuses the batch: {error}")
                continue
            summary["inputs_still_refused"] += sum(len(r.get("inputs") or []) for r in refused)
            for reason, digest in unparsed:
                leave("unclassified", reason, digest=digest)
            for refused_record in refused:
                reason = f"{refused_record.get('stage')}: {refused_record.get('reason')}"
                for item in refused_record.get("inputs") or []:
                    digest = item.get("raw_sha256")
                    raw = received.get((item.get("input_row"), digest))
                    if raw is None:
                        leave("unclassified", reason, digest=digest)
                    else:
                        leave(_refused_input_class(raw, run_id, producer, tree, counted), reason,
                              digest=digest)
            for row in rows:
                event_id = str(row.get("event_id"))
                if event_id in counted:
                    summary["rows_already_counted"] += 1
                elif event_id not in planned:
                    planned.add(event_id)
                    by_run.setdefault(run_id, []).append(row)
        if quarantine_cache is not None:
            for oid in sorted(changed):
                quarantine_cache.store(oid, entries[oid])
            cache_module.report(series_cache, quarantine_cache, time.monotonic() - started)
        summary["rows"] = sum(len(rows) for rows in by_run.values())
        summary["left_refused"] = _left_refused_summary(left)
        acknowledgement_summary()
        summary["seconds"] = round(time.monotonic() - started, 2)
        return commit, by_run, summary, published
    finally:
        archive_snapshot.close()


#: The spool batch of the rows readmitted for one run: apart from the batch the
#: run appended itself, because these rows are derived from published records
#: and may be dropped again (:func:`_drop_published_readmissions`).
_READMITTED_BATCH_PREFIX = "readmitted-"


def _drop_published_readmissions(
    parent: Any, published: set[str], *, verify: Callable[[], None],
) -> int:
    """Drop from the readmitted spool batches each row the ledger now carries.

    Two hosts can replay the same refusal, and their writers can differ, so a
    spooled copy may have a body other than the one the ledger now carries;
    publishing it would be refused, and every later publication with it. The
    ledger's row is counted whatever its body, so the spooled copy is dropped.
    Rows of a run's own batch are never touched. Returns the rows dropped.
    """
    verify()
    spool = spool_dir(parent)
    dropped = 0
    if not spool.is_dir():
        return 0
    with series_spool_transition_lock(spool, exclusive=True):
        for path in sorted(spool.glob(f"{_READMITTED_BATCH_PREFIX}*.jsonl")):
            data = path.read_bytes()
            lines = [line for line in data.split(b"\n") if line.strip()]
            kept = []
            for line in lines:
                found = _LINE_EVENT_ID_RE.search(line)
                if found is None or found.group(1).decode() not in published:
                    kept.append(line)
            if len(kept) == len(lines):
                continue
            dropped += len(lines) - len(kept)
            if kept:
                verify()
                write_quarantine_file(
                    spool, path.name, b"".join(line + b"\n" for line in kept).decode("utf-8"),
                    replace=True,
                )
            else:
                verify()
                path.unlink()
    return dropped


def readmit_quarantined(
    parent: Any, *, published_ref: str = "refs/remotes/origin/main",
    host: str | None = None, hermit_checkout: Any | None = None,
) -> dict:
    """Append to the series spool every refused row the current writer admits.

    Runs before each series publication (``mirror.publish_series``), so a
    refused input counts from the first publication after the writer that
    refused it is fixed, with no replay command and no new record type. See
    :func:`readmission_plan` for what is replayed and how. Must be called
    without the series spool lock held. Returns the plan's summary, with its
    ``left_refused`` classification, plus ``appended`` (rows written to the
    spool), ``conflicts`` and ``dropped``.

    A run's rows go to its own ``readmitted-<run_id>.jsonl`` batch. A row of
    such a batch that the ledger now carries, published by another host's
    replay, is first dropped from it (``dropped``), so a copy that lost the
    race to publication cannot hold back this host's publications.
    """
    commit, by_run, summary, published = _readmission_plan(
        parent, published_ref=published_ref, host=host, hermit_checkout=hermit_checkout,
    )
    summary["appended"] = 0
    summary["conflicts"] = []
    summary["dropped"] = 0
    if published is None and not by_run:
        return summary
    repo = published_repository(parent, LEDGER_REPOSITORY)
    with file_archive_scope(repo, commit) as files:
        def verify_published() -> None:
            files.verify(files.listing(("series", "quarantine")))

        if published is not None:
            summary["dropped"] = _drop_published_readmissions(
                parent, published, verify=verify_published)
        if not by_run:
            return summary
        snapshot = _published_snapshot_at_repo(
            repo, commit, repository=LEDGER_REPOSITORY, files=files)
        try:
            for run_id, rows in sorted(by_run.items()):
                conflicts: list[tuple[int, dict, str]] = []
                summary["appended"] += append_cell_rows(
                    parent, spool_dir(parent) / (_READMITTED_BATCH_PREFIX + _batch_name(rows)), rows,
                    conflicts=conflicts, published=snapshot,
                )
                summary["conflicts"].extend(
                    {"run_id": run_id, "event_id": row.get("event_id"), "reason": reason}
                    for _position, row, reason in conflicts
                )
        finally:
            snapshot.close()
    return summary


def readmission_summary_path(parent: Any) -> Path:
    """Where :func:`write_readmission_summary` keeps the parent's last summary."""
    return quarantine_dir(parent) / READMISSION_SUMMARY_NAME


def write_readmission_summary(parent: Any, summary: Mapping[str, Any]) -> Path:
    """Keep what the last readmission left refused, for the health gate.

    ``summary`` is what :func:`readmit_quarantined` returned. The file holds
    its ``left_refused`` classification, the ledger commit it was computed
    at, and when it was written; each call replaces it whole
    (:func:`write_quarantine_file`). ``mirror.publish_series`` calls this
    only after a readmission succeeds, so a readmission that keeps failing
    leaves the file to age, and the gate reports that it cannot tell.
    """
    left = summary.get("left_refused")
    if not isinstance(left, Mapping):
        raise SeriesError("a readmission summary without left_refused classifies nothing")
    document = {
        "schema": READMISSION_SUMMARY_SCHEMA,
        "written_at": _utc_now(),
        "ledger_commit": summary.get("ledger_commit"),
        "left_refused": left,
    }
    # Reviewed refusals, and acknowledgements naming an input that is not refused.
    for name in ("acknowledged", "acknowledgement_errors"):
        if isinstance(summary.get(name), Mapping):
            document[name] = summary[name]
    path = readmission_summary_path(parent)
    return write_quarantine_file(
        path.parent, path.name, json.dumps(document, sort_keys=True) + "\n", replace=True,
    )


__all__ = [
    "COORDINATE_KEYS",
    "ENVELOPE_VERSION",
    "FIRST_DIVERGENT_MESSAGE_KEYS",
    "HOST_FACTS_ENVELOPE_VERSION",
    "LEGACY_ENVELOPE_VERSION",
    "READABLE_ENVELOPE_VERSIONS",
    "SERIES_SPOOL_LOCK_TIMEOUT_SECONDS",
    "SHARD_PART_LIMIT",
    "MalformedRow",
    "ProjectionResults",
    "SeriesError",
    "append_rows",
    "collapse_rows",
    "classify_pressure_sample",
    "commit_depth",
    "history_rows",
    "history_store_rows",
    "is_month_shard_name",
    "main_ancestry",
    "measurement_state",
    "observed_range",
    "parse_line",
    "parse_shard_part_path",
    "published_rows",
    "read_scorecard_projection_snapshot",
    "projection_results",
    "row_identity",
    "rows_from_cell_results",
    "scorecard_projection_snapshot",
    "series_for",
    "series_spool_transition_lock",
    "shared_parent_root",
    "shard_part_path",
    "shard_path",
    "spool_dir",
    "source_depths",
    "store_paths",
    "test_id",
    "union",
    "validate_row",
    "validate_read_rows",
    "validate_rows",
    "verify_append_only",
    "write_scorecard_projection_snapshot",
]


# --------------------------------------------------------------------------
# Write-boundary CLI.
#
# Step 3 left this module a library with no entry point. The commands below are
# the Python projection and spool front door, while Hermit's shared `SeriesRow`
# is the write-boundary type invoked by `_validated_rows`.
#
# This is deliberately the only thing the CLI does. It does not publish, it does
# not read, and it never touches the tracked shard -- a producer writes the
# spool and publication stays a separate, reviewed step.
# --------------------------------------------------------------------------


def _batch_name(rows: Sequence[dict]) -> str:
    """Name the spool batch after the run that produced it.

    Deliberately NOT a timestamp or a pid. A campaign re-run against the same
    tree must land in the same file so `union_events` can dedup it by
    `event_id`; a clock-derived name would turn one logical batch into two and
    defeat the dedup this store already relies on.
    """
    run_ids = {str(r.get("run_id", "")) for r in rows}
    if len(run_ids) != 1:
        raise SeriesError(
            f"a batch must carry exactly one run_id, got {len(run_ids)}: "
            f"{sorted(run_ids)}"
        )
    run_id = run_ids.pop()
    if not run_id or "/" in run_id or run_id in (".", ".."):
        raise SeriesError(f"run_id must be a single path segment, got {run_id!r}")
    return f"{run_id}.jsonl"


def _stdin_unique_fields(pairs):
    fields = {}
    for key, value in pairs:
        if key in fields:
            raise MalformedRow(f"duplicate field {key!r}")
        fields[key] = value
    return fields


def _stdin_nonjson_constant(value):
    raise MalformedRow(f"non-JSON numeric constant {value}")


def _stdin_finite_float(text):
    value = float(text)
    if not math.isfinite(value):
        raise MalformedRow(f"out-of-range number {text}")
    return value


def _stdin_rows() -> tuple[list[dict] | None, int]:
    rows = []
    for number, line in enumerate(sys.stdin.read().splitlines(), 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
            if isinstance(row, dict) and "backend_parity" in row:
                row = json.loads(line, object_pairs_hook=_parity_unique_fields,
                                 parse_constant=_parity_nonfinite_constant,
                                 parse_float=_stdin_finite_float)
            # serde_json refuses a lone surrogate anywhere in a row.
            json.dumps(row, ensure_ascii=False).encode()
            rows.append(row)
        except (ValueError, MalformedRow) as error:
            # ValueError covers JSONDecodeError, an integer past Python's digit
            # limit and UnicodeEncodeError.
            print(f"series: stdin line {number} is not JSON: {error}", file=sys.stderr)
            return None, 2
    return rows, 0


def _parse_cell_result_line(raw: bytes) -> Any:
    """Parse one received cell-result line the way Hermit's serde_json reads it.

    Refuses duplicate fields, non-JSON constants, numbers that overflow to
    infinity, and lone surrogates, raising ``ValueError``, ``RecursionError``
    or ``MalformedRow``. Shared by append-cells and by the replay of refused
    inputs, so a replayed line is read exactly as it was when it arrived.
    """
    line = raw.decode("utf-8")
    row = json.loads(line, object_pairs_hook=_stdin_unique_fields,
                     parse_constant=_stdin_nonjson_constant,
                     parse_float=_stdin_finite_float)
    # serde_json refuses a lone surrogate anywhere in a row.
    json.dumps(row, ensure_ascii=False).encode()
    return row


def _stdin_cell_results() -> tuple[
    list[tuple[int, Any, bytes]], list[tuple[int, bytes, str, str]]
]:
    """Read stdin as bytes, separating parsed lines from lines that are not JSON.

    Returns ``(parsed, failures)``: ``parsed`` holds ``(line_number, value,
    raw)`` and ``failures`` holds ``(line_number, raw, error_type, reason)``.
    ``raw`` is the exact received line, without only its ``\\n`` terminator.
    Lines are split on ``\\n`` alone: ``str.splitlines`` would also split a
    JSON string at U+2028, U+0085 or a form feed, which a JSON writer may emit
    unescaped. Lines of ASCII whitespace are skipped. Nothing is refused here;
    the caller quarantines failures.

    Every line is read refusing duplicate fields, non-JSON constants, and
    numbers that overflow to infinity (``1e999``) in every field of every row,
    as Hermit's serde_json does. A duplicate ``expected_guest_exit`` or
    ``attempts`` would otherwise be collapsed before validation sees it.
    """
    buffer = getattr(sys.stdin, "buffer", None)
    data = buffer.read() if buffer is not None else sys.stdin.read().encode("utf-8")
    lines = data.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    parsed: list[tuple[int, Any, bytes]] = []
    failures: list[tuple[int, bytes, str, str]] = []
    for number, raw in enumerate(lines, 1):
        if not raw.strip():
            continue
        try:
            row = _parse_cell_result_line(raw)
        except (ValueError, RecursionError, MalformedRow) as error:
            # ValueError covers UnicodeDecodeError, JSONDecodeError, an integer
            # past Python's digit limit and UnicodeEncodeError.
            failures.append((
                number, raw, type(error).__name__, f"stdin line {number} is not JSON: {error}",
            ))
            continue
        parsed.append((number, row, raw))
    return parsed, failures


def _quarantine_summary(record: dict) -> str:
    """One refusal without its content: identity, reason, and input digests."""
    summary = {
        key: value for key, value in record.items()
        if key not in {"inputs", "row", "legacy"}
    }
    summary["inputs"] = [
        {name: item.get(name) for name in ("input_row", "raw_sha256", "raw_bytes")}
        for item in record.get("inputs") or []
    ]
    return json.dumps(summary, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _cmd_append(argv: list[str]) -> int:
    parent = None
    i = 0
    while i < len(argv):
        if argv[i] == "--parent" and i + 1 < len(argv):
            parent = argv[i + 1]
            i += 2
            continue
        return _usage(f"unknown argument {argv[i]!r}")
    if not parent:
        return _usage("append requires --parent <dev-hermit root>")

    rows, error = _stdin_rows()
    if rows is None:
        return error
    if not rows:
        # Not an error, and deliberately not silent: a campaign that measured
        # nothing must not read as a campaign that was never run.
        print("series: no rows on stdin; nothing appended", file=sys.stderr)
        return 0

    try:
        target = spool_dir(parent) / _batch_name(rows)
        target.parent.mkdir(parents=True, exist_ok=True)
        written = append_rows(target, rows)
    except (MalformedRow, SeriesError) as error:
        # The batch is refused WHOLE by append_rows. Say which row so the
        # producer can fix it rather than bisecting its own output.
        print(f"series: REFUSED the batch, nothing was written: {error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"series: cannot write the spool: {error}", file=sys.stderr)
        return 2

    print(f"series: appended {written} row(s) to {target}")
    return 0


def _parity_stdin_bytes() -> bytes:
    buffer = getattr(sys.stdin, "buffer", None)
    return buffer.read() if buffer is not None else sys.stdin.read().encode("utf-8")


def _parity_stdin_lines(data: bytes) -> int | None:
    """How many lines stdin held, as `_parity_stdin_sources` splits it; None if not UTF-8."""
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return None
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return len(lines)


def _parity_stdin_sources(parity, data: bytes) -> list[Any]:
    """One `ParityLedgerSource` per stdin line, in order; strict JSON.

    Unlike `append-cells`, nothing is quarantined: parity rows are derived from
    files hermit already retains, so a refused batch is re-exportable
    (`test-harness parity export`). It is refused whole, loudly, and the
    refusal is recorded as a `parity.refusal`.
    """
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise parity.ParityLedgerError(f"stdin is not UTF-8: {error}") from error
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    sources = []
    for number, line in enumerate(lines, 1):
        if not line.strip():
            raise parity.ParityLedgerError(f"stdin line {number} is blank")
        sources.append(parity.loads(line, f"stdin line {number}"))
    return sources


def _published_parity(parity, parent: Any, run: Sequence[Mapping[str, Any]] | None = None,
                      ) -> tuple[dict[str, dict], str]:
    """The parity events on the fetched ledger, and a sentence saying what was read.

    With RUN (one run's marker, then its rows, as ``build_batch`` returns
    them) only what ``parity.published_parity_for_runs`` reads for it, which
    answers every lookup of RUN's events as the whole index would.

    An unreadable ledger is named, not fatal: the publisher repeats every
    check against the ledger it pushes to.
    """
    try:
        if run is None:
            at = parity.published_parity_at_repo(ledger_root(parent), "refs/remotes/origin/main")
        else:
            at = parity.published_parity_for_runs(
                ledger_root(parent), "refs/remotes/origin/main", [run])
    except (SeriesError, parity.ParityLedgerError, OSError, subprocess.SubprocessError) as error:
        return {}, f"published ledger not consulted: {error}"
    check = f"read {len(at.events)} published parity event(s) at {at.commit}"
    if at.invalid:
        check += f"; {len(at.invalid)} published entr(ies) could not be indexed"
    return at.events, check


def _utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def defer_parity_call(parent: Any, record: Mapping[str, Any]) -> Path:
    """Keep one `append-parity` call that the parity spool did not take.

    The file is written like a quarantine file (`write_quarantine_file`):
    without a lock, whole or not at all, and never over another. It is named
    `<run event id>.<time_ns>-<pid>-<random>.jsonl`, so a run's deferred calls
    are found by name without reading them. ``record`` is checked first, so
    a call is never kept in a form the replay would refuse to read.
    """
    parity = parity_ledger_module()
    producer, run_id = _check_deferred_parity(parity, record, "deferred parity call")
    name = (f"{parity.run_event_id(producer, run_id)}.{time.time_ns()}-{os.getpid()}-"
            f"{secrets.token_hex(4)}.jsonl")
    return write_quarantine_file(
        parity_deferred_dir(parent), name,
        json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n",
    )


def _check_deferred_parity(parity, record: Any, label: str) -> tuple[str, str]:
    """Refuse a deferred call that is not exactly `parity-deferred/v1`; its producer and run."""
    if not isinstance(record, dict) or record.get("schema") != PARITY_DEFERRED_SCHEMA:
        raise parity.ParityLedgerError(f"{label}: not a {PARITY_DEFERRED_SCHEMA} record")
    kind = record.get("kind")
    fields = PARITY_DEFERRED_FIELDS.get(kind) if isinstance(kind, str) else None
    if fields is None or set(record) != fields:
        raise parity.ParityLedgerError(
            f"{label}: a {record.get('kind')!r} call does not carry exactly its fields")
    for name in ("deferred_at", "reason"):
        if not isinstance(record[name], str) or not record[name]:
            raise parity.ParityLedgerError(f"{label}: {name} must be a nonempty string")
    if record["kind"] == "refusal":
        event = parity.validate_refusal(record["event"])
        return event["producer"], event["run_id"]
    for name in ("producer", "run_id", "tree", "emitted_at", "host", "input", "input_sha256"):
        if not isinstance(record[name], str):
            raise parity.ParityLedgerError(f"{label}: {name} must be a string")
    if not isinstance(record["source_tree_dirty"], bool):
        raise parity.ParityLedgerError(f"{label}: source_tree_dirty must be a boolean")
    data = record["input"].encode("utf-8")
    if (type(record["input_bytes"]) is not int or record["input_bytes"] != len(data)
            or record["input_sha256"] != hashlib.sha256(data).hexdigest()):
        raise parity.ParityLedgerError(
            f"{label}: its input is not the {record['input_bytes']!r} byte(s) it names")
    return record["producer"], record["run_id"]


def _read_deferred_parity(parity, path: Path) -> dict:
    """One deferred call: a single JSON line naming the run its file is named for."""
    try:
        text = path.read_bytes().decode("utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise parity.ParityLedgerError(f"{path.name}: cannot read: {error}") from error
    if not text.endswith("\n") or "\n" in text[:-1]:
        raise parity.ParityLedgerError(f"{path.name}: is not exactly one line")
    record = parity.loads(text[:-1], path.name)
    producer, run_id = _check_deferred_parity(parity, record, path.name)
    if not path.name.startswith(parity.run_event_id(producer, run_id) + "."):
        raise parity.ParityLedgerError(
            f"{path.name}: names a different run than its {producer} run {run_id}")
    return record


def _replay_deferred_call(parity, spool: Path, record: Mapping[str, Any], *,
                          published: Mapping[str, dict], published_check: str) -> tuple[str, str | None]:
    """Decide one deferred call as `append-parity` would have; caller holds the lock.

    Returns the outcome (``appended``, ``already_recorded`` or ``refused``)
    and the batch written, if any. Rows the writer now refuses, at any step,
    are recorded as that call's `parity.refusal`.
    """
    if record["kind"] == "refusal":
        return "refused", parity.append_refusal(spool, record["event"])["batch"]
    data = record["input"].encode("utf-8")
    identity = {
        "producer": record["producer"], "run_id": record["run_id"],
        "hermit_sha": record["tree"], "emitted_at": record["emitted_at"],
        "source_tree_dirty": record["source_tree_dirty"], "host": record["host"],
    }
    try:
        result = parity.append_parity(
            spool, _parity_stdin_sources(parity, data), **identity,
            published=published, published_check=published_check,
        )
    except parity.ParityLedgerError as error:
        event = parity.build_refusal(
            **identity, input_bytes=data, input_lines=_parity_stdin_lines(data), error=error,
        )
        return "refused", parity.append_refusal(spool, event)["batch"]
    if result["already_recorded"]:
        return "already_recorded", None
    return "appended", result["batch"]


def replay_deferred_parity(
    parent: Any, *, timeout_seconds: float = SERIES_SPOOL_LOCK_TIMEOUT_SECONDS,
) -> dict:
    """Replay every deferred `append-parity` call into the parity spool.

    Runs before each series publication (``mirror.publish_series``), which
    then publishes what it spooled. Must be called without the parity spool
    lock held; it takes the lock once, only for the spool writes. Each call is
    decided as `append-parity` would have decided it, under its own
    `emitted_at` and host: its batch is spooled, it is found already
    recorded, or it is refused and the refusal spooled. Only then, and only
    once the spool directory is synced under the lock, is its file deleted:
    `parity_ledger._write_batch` renames a batch into place without syncing
    the directory, so until then a crash could lose the batch and keep the
    deletion. A call the spool still cannot take, a file that cannot be read,
    and every decided call when that sync fails stay where they are, listed in
    ``kept`` with the reason; nothing is ever discarded here. The next replay
    of a kept call whose batch did reach the spool writes nothing new.

    Returns ``{"replayed", "appended", "already_recorded", "refused",
    "batches", "kept"}``: counts of decided calls, the batches written, and
    ``{"file", "reason"}`` for each call kept.
    """
    summary: dict[str, Any] = {"replayed": 0, "appended": 0, "already_recorded": 0,
                               "refused": 0, "batches": [], "kept": []}
    directory = parity_deferred_dir(parent)
    files = sorted(directory.glob("*.jsonl"))
    if not files:
        return summary
    parity = parity_ledger_module()
    calls: list[tuple[Path, dict]] = []
    for path in files:
        try:
            calls.append((path, _read_deferred_parity(parity, path)))
        except parity.ParityLedgerError as error:
            summary["kept"].append({"file": path.name, "reason": f"unreadable: {error}"})
    if not calls:
        return summary
    published, published_check = _published_parity(parity, parent)
    spool = parity_spool_dir(parent)
    decided: list[tuple[Path, str, str | None]] = []
    try:
        with series_spool_transition_lock(spool, exclusive=True, timeout_seconds=timeout_seconds):
            for path, record in calls:
                try:
                    outcome, batch = _replay_deferred_call(
                        parity, spool, record, published=published, published_check=published_check)
                except (parity.ParityLedgerError, OSError) as error:
                    summary["kept"].append({"file": path.name, "reason": str(error)})
                    continue
                decided.append((path, outcome, batch))
            if decided:
                try:
                    _fsync_quarantine_directory(spool)
                except OSError as error:
                    summary["kept"].extend(
                        {"file": path.name, "reason": f"the parity spool was not synced: {error}"}
                        for path, _outcome, _batch in decided)
                    decided = []
    except SeriesError as error:
        # Nothing is deleted unless the lock was held, and released, cleanly.
        kept = {item["file"] for item in summary["kept"]}
        summary["kept"].extend(
            {"file": path.name, "reason": f"the parity spool lock failed: {error}"}
            for path, _record in calls if path.name not in kept)
        return summary
    for path, outcome, batch in decided:
        path.unlink(missing_ok=True)
        summary["replayed"] += 1
        summary[outcome] += 1
        if batch is not None:
            summary["batches"].append(batch)
    if decided:
        _fsync_quarantine_directory(directory)
    return summary


def _record_parity_refusal(parity, parent: Any, spool: Path, *, producer: str, run_id: str,
                           tree: str, emitted_at: str, dirty: bool | None, data: bytes,
                           error: Any) -> str:
    """Spool a `parity.refusal` for this call; a sentence naming where it went.

    A refusal that the spool does not take (its lock timed out, its write
    failed) is deferred like a batch of rows, and the next series publication
    records it. A refusal that itself cannot be recorded (an identity the
    ledger cannot hold) is named, never raised: the caller's own refusal is
    what the exit status reports.
    """
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", emitted_at) is None:
        # A malformed --emitted-at is itself a refusal; the refusal is dated now.
        emitted_at = _utc_now()
    try:
        event = parity.build_refusal(
            producer=producer, run_id=run_id, hermit_sha=tree, emitted_at=emitted_at,
            source_tree_dirty=dirty, host=_writer_host(), input_bytes=data,
            input_lines=_parity_stdin_lines(data), error=error,
        )
    except (parity.ParityLedgerError, MalformedQuarantine, OSError) as failure:
        return f"the refusal could NOT be recorded: {failure}"
    try:
        with series_spool_transition_lock(spool, exclusive=True):
            outcome = parity.append_refusal(spool, event)
    except parity.ParityLedgerError as failure:
        return f"the refusal could NOT be recorded: {failure}"
    except (OSError, SeriesError) as failure:
        try:
            kept = defer_parity_call(parent, {
                "schema": PARITY_DEFERRED_SCHEMA, "kind": "refusal", "deferred_at": _utc_now(),
                "reason": str(failure), "event": event,
            })
        except (OSError, parity.ParityLedgerError) as deferral:
            return (f"the refusal could NOT be recorded ({failure}) and could not be "
                    f"deferred either: {deferral}")
        return (f"the refusal is deferred -> {kept} (the parity spool did not take it: "
                f"{failure}); the next series publication records it")
    if outcome["batch"] is None:
        return f"the refusal is already recorded ({outcome['recorded_in']})"
    return f"the refusal is recorded -> {spool / outcome['batch']}"


def _cmd_append_parity(argv: list[str]) -> int:
    """Spool one run's parity rows: `parity: ...` on stdout, a refusal on stderr.

    Exit 0 when the batch was written, was already recorded identically, or
    was deferred, 1 when the rows were refused, 2 for a usage error or a call
    that could be neither written nor deferred. A call whose rows the writer
    admits but whose spool lock timed out, or whose spool write failed, is
    kept in `parity_deferred_dir` (`defer_parity_call`) and replayed by the
    next series publication, so hermit, which reads exit 0 as acceptance,
    never leaves rows that the ledger then loses.
    When the call names a valid producer, run and tree, refused rows (exit 1)
    and a missing or invalid `--source-tree-dirty` (exit 2) are spooled as a
    `parity.refusal` (see parity_ledger.py), so the run reads as refused
    rather than as never attempted. Every other usage error, such as an
    unknown or repeated flag or help with other arguments, exits 2 before
    anything is recorded.

    `--help` or `-h` alone prints APPEND_PARITY_USAGE on stdout, exits 0 and
    reads and writes nothing: hermit's parity export probes the writer that
    way before it sends rows. Help with any other argument is a usage error
    (exit 2), and nothing is read or written either.
    """
    if argv in (["--help"], ["-h"]):
        print(APPEND_PARITY_USAGE)
        return 0
    if any(argument in {"-h", "--help"} for argument in argv):
        return _append_parity_usage_error("help cannot be combined with other arguments")
    options: dict[str, str] = {}
    flags = APPEND_PARITY_FLAGS
    i = 0
    while i < len(argv):
        name = flags.get(argv[i])
        if name is None or i + 1 >= len(argv):
            return _usage(f"unknown append-parity argument {argv[i]!r}")
        if name in options:
            return _usage(f"append-parity supplies {argv[i]} more than once")
        options[name] = argv[i + 1]
        i += 2
    missing = [
        flag for flag, name in flags.items()
        if name in {"parent", "producer", "run_id", "tree"} and not options.get(name)
    ]
    if missing:
        return _usage(f"append-parity requires {', '.join(missing)}")
    emitted_at = options.get("emitted_at") or datetime.now(timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    parity = parity_ledger_module()
    parent = options["parent"]
    spool = parity_spool_dir(parent)
    try:
        data = _parity_stdin_bytes()
    except OSError as error:
        print(f"series: cannot read the parity rows on stdin: {error}", file=sys.stderr)
        return 2
    # Where no producer, run and tree can be named, no refusal can say whose it is.
    identified = (
        options["producer"] in parity.PRODUCERS
        and options["run_id"].strip() and "\0" not in options["run_id"]
        and re.fullmatch(r"[0-9a-f]{40}", options["tree"]) is not None
    )

    def refuse(error: Any, dirty: bool | None, status: int) -> int:
        recorded = (
            _record_parity_refusal(
                parity, parent, spool, producer=options["producer"], run_id=options["run_id"],
                tree=options["tree"], emitted_at=emitted_at, dirty=dirty, data=data, error=error,
            )
            if identified else "the refusal is not recorded: no valid producer, run and tree name it"
        )
        if status == 2:
            _usage(str(error))
            print(f"series: {recorded}", file=sys.stderr)
            return 2
        print(f"series: REFUSED the parity batch, no row was written; {recorded}: {error}",
              file=sys.stderr)
        return status

    # Like append-cells, the tree state is evidence and is never defaulted:
    # a row stamped clean without a caller saying so would be admitted as
    # clean evidence by every scorecard that excludes dirty rows.
    dirty_text = options.get("source_tree_dirty")
    if dirty_text is None:
        return refuse("append-parity requires --source-tree-dirty true|false", None, 2)
    if dirty_text not in {"true", "false"}:
        return refuse(
            f"append-parity --source-tree-dirty must be true or false, got {dirty_text!r}", None, 2
        )
    dirty = dirty_text == "true"
    identity = {
        "producer": options["producer"], "run_id": options["run_id"],
        "hermit_sha": options["tree"], "emitted_at": emitted_at, "source_tree_dirty": dirty,
    }
    try:
        identity["host"] = _writer_host()
        sources = _parity_stdin_sources(parity, data)
        # The whole batch is checked before the lock is taken, so the lock
        # covers only the spool's own check and write, and a call that has to
        # be deferred is one this writer admits.
        events = parity.build_batch(sources, **identity)
        marker = events[0]
    except (MalformedQuarantine, parity.ParityLedgerError) as error:
        return refuse(error, dirty, 1)
    counts = ", ".join(
        f"{verdict} {count}" for verdict, count in marker["verdicts"].items() if count
    ) or "no rows"
    subject = f"{options['producer']} run {options['run_id']}"
    # What the ledger already holds for this run, so a replay is recognised
    # and a changed record is refused even after its batch was drained.
    published, published_check = _published_parity(parity, parent, events)
    try:
        with series_spool_transition_lock(spool, exclusive=True):
            result = parity.append_parity(
                spool, sources, **identity,
                published=published, published_check=published_check,
            )
    except parity.ParityLedgerError as error:
        return refuse(error, dirty, 1)
    except (OSError, SeriesError) as error:
        try:
            kept = defer_parity_call(parent, {
                "schema": PARITY_DEFERRED_SCHEMA, "kind": "rows", "deferred_at": _utc_now(),
                "reason": str(error), "producer": options["producer"],
                "run_id": options["run_id"], "tree": options["tree"], "emitted_at": emitted_at,
                "source_tree_dirty": dirty, "host": identity["host"],
                "input": data.decode("utf-8"), "input_sha256": hashlib.sha256(data).hexdigest(),
                "input_bytes": len(data),
            })
        except (OSError, parity.ParityLedgerError) as deferral:
            print(f"series: cannot write the parity spool {spool}: {error}; and the call "
                  f"could not be deferred either: {deferral}", file=sys.stderr)
            return 2
        print(
            f"parity: deferred {marker['records']} record(s) ({counts}) for {subject}: the "
            f"parity spool did not take them ({error}); the call is kept -> {kept}, and the "
            "next series publication appends it"
        )
        print(f"parity: {published_check}", file=sys.stderr)
        return 0
    if result["already_recorded"]:
        print(
            f"parity: {result['records']} record(s) for {subject} are already recorded "
            f"({', '.join(result['recorded_in'])}); nothing written"
        )
    elif result["records"] == 0:
        print(
            f"parity: recorded an empty parity selection for {subject} "
            f"-> {spool / result['batch']}"
        )
    else:
        print(
            f"parity: appended {result['records']} record(s) ({counts}) for {subject} "
            f"-> {spool / result['batch']}"
        )
    print(f"parity: {published_check}", file=sys.stderr)
    return 0


def _cmd_append_cells(argv: list[str]) -> int:
    parent = None
    hermit_checkout = None
    producer = None
    run_id = None
    tree = None
    emitted_at = None
    parity_files: list[str] = []
    i = 0
    while i < len(argv):
        if argv[i] == "--parent" and i + 1 < len(argv):
            parent = argv[i + 1]
        elif argv[i] == "--parity-file" and i + 1 < len(argv):
            parity_files.append(argv[i + 1])
        elif argv[i] == "--checkout" and i + 1 < len(argv):
            hermit_checkout = argv[i + 1]
        elif argv[i] == "--producer" and i + 1 < len(argv):
            producer = argv[i + 1]
        elif argv[i] == "--run-id" and i + 1 < len(argv):
            run_id = argv[i + 1]
        elif argv[i] == "--tree" and i + 1 < len(argv):
            tree = argv[i + 1]
        elif argv[i] == "--emitted-at" and i + 1 < len(argv):
            emitted_at = argv[i + 1]
        else:
            return _usage(f"unknown append-cells argument {argv[i]!r}")
        i += 2
    missing = [
        name for name, value in (
            ("--parent", parent),
            ("--producer", producer),
            ("--run-id", run_id),
            ("--tree", tree),
        ) if not value
    ]
    if missing:
        return _usage(f"append-cells requires {', '.join(missing)}")
    producer, run_id, tree = str(producer), str(run_id), str(tree)
    if emitted_at is not None and _QUARANTINE_EMITTED_AT_RE.fullmatch(emitted_at) is None:
        return _usage(f"append-cells --emitted-at must be a UTC timestamp, got {emitted_at!r}")
    try:
        host = _writer_host()
    except MalformedQuarantine as error:
        print(f"series: REFUSED the cell batch, nothing was written: {error}", file=sys.stderr)
        return 2
    try:
        parity_records = load_parity_records(parity_files)
    except (OSError, ValueError, SeriesError) as error:
        print(f"series: REFUSED the cell batch, nothing was written: cannot read --parity-file: {error}",
              file=sys.stderr)
        return 2
    parsed, failures = _stdin_cell_results()
    if not parsed and not failures:
        print("series: no cell results on stdin; nothing appended", file=sys.stderr)
        return 1
    emitted_at = emitted_at or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    total = len(parsed) + len(failures)

    def record(stage: str, error: str, reason: str, inputs: list[dict], *,
               row: dict | None = None) -> dict:
        return quarantine_record(
            stage=stage, run_id=run_id, producer=producer, tree=tree,
            emitted_at=emitted_at, error=error, reason=reason, inputs=inputs,
            row=row, row_event_id=None if row is None else str(row.get("event_id", "")),
            cell=_quarantine_cell(row), host=host,
        )

    def parse_records() -> list[dict]:
        return [
            record("parse", error_type, reason, [quarantine_input(number, raw)])
            for number, raw, error_type, reason in failures
        ]

    # Each cell result is judged on its own. A refused one is quarantined with
    # its exact reason and received bytes; the independent valid results beside
    # it are still appended. Only a refusal about the invocation itself (not
    # any one row) keeps every row out, and then every row is quarantined too.
    quarantined = parse_records()
    series_rows: list[dict] = []
    provenance: list[list[int]] = []
    written = 0
    target = None
    batch_error = None
    exit_on_error = 1
    try:
        if parsed:
            series_rows = _rows_from_cell_results(
                [value for _, value, _ in parsed],
                parent=parent,
                producer=producer,
                run_id=run_id,
                tree=tree,
                emitted_at=emitted_at,
                hermit_checkout=hermit_checkout,
                quarantine=quarantined,
                input_numbers=[number for number, _, _ in parsed],
                raw_inputs=[raw for _, _, raw in parsed],
                provenance=provenance,
                parity_records=parity_records,
            )
        if series_rows:
            conflicts: list[tuple[int, dict, str]] = []
            target = spool_dir(parent) / _batch_name(series_rows)
            target.parent.mkdir(parents=True, exist_ok=True)
            written = append_cell_rows(parent, target, series_rows, conflicts=conflicts)
            conflicted = {position for position, _, _ in conflicts}
            for position, row, reason in conflicts:
                quarantined.append(record(
                    "spool-conflict", "SpoolConflict", reason,
                    [quarantine_input(parsed[index][0], parsed[index][2])
                     for index in provenance[position]],
                    row=row,
                ))
            series_rows = [
                row for position, row in enumerate(series_rows) if position not in conflicted
            ]
    except (MalformedEvent, SeriesError) as error:
        batch_error = f"series: REFUSED the cell batch, nothing was written: {error}"
        batch_error_type = type(error).__name__
    except OSError as error:
        batch_error = f"series: cannot write the spool: {error}"
        batch_error_type = type(error).__name__
        exit_on_error = 2
    except Exception as error:
        # A defect in this writer, not a refusal of the data. Retain every
        # received input exactly, so the run's cells survive the bug, and then
        # let the exception propagate with its traceback and a nonzero exit.
        detail = (
            f"series writer failed with {type(error).__name__}: {error}; "
            f"{written} row(s) had been appended to {target}"
        )
        retained = parse_records() + [
            record("writer-error", type(error).__name__, detail,
                   [quarantine_input(number, raw)])
            for number, _, raw in parsed
        ]
        try:
            outcome = append_quarantine(parent, run_id, retained)
            print(
                f"series: WRITER ERROR; retained {len(retained)} input(s) at "
                f"{outcome['path']}: {detail}",
                file=sys.stderr,
            )
        except (OSError, LedgerError) as retention_error:
            print(
                f"series: WRITER ERROR and FAILED to retain {len(retained)} input(s): "
                f"{retention_error}; {detail}",
                file=sys.stderr,
            )
        raise
    if batch_error is not None:
        series_rows = []
        written = 0
        quarantined = parse_records() + [
            record("batch", batch_error_type, batch_error.removeprefix("series: "),
                   [quarantine_input(number, raw)])
            for number, _, raw in parsed
        ]

    retention: dict[str, Any] = {
        "path": None, "written": 0, "duplicates": 0, "published_check_error": None,
    }
    quarantine_error = None
    if quarantined:
        try:
            retention = append_quarantine(parent, run_id, quarantined)
        except (OSError, LedgerError) as error:
            quarantine_error = error

    if batch_error is not None:
        exit_code = exit_on_error
    elif quarantine_error is not None:
        exit_code = 1
    elif not quarantined:
        exit_code = 0
    elif not series_rows:
        # Nothing independent survived: a batch whose every row was refused
        # must not read as a successful append.
        exit_code = 1
    elif producer == "validate":
        # The valid rows are appended and every refused input is retained,
        # byte for byte, in the quarantine spool (fsynced above). That spool
        # is published to the test ledger beside the series at finalize, its
        # count is recorded in the run's ledger_mirror.raw_publication, and
        # ledger_spool_backlog --store series-quarantine alarms while the last
        # readmission leaves a refused input that records an attempt without
        # a row, or one it cannot read. Failing the validate here would
        # instead make a data refusal in one cell fail the tests of every
        # other cell.
        exit_code = 0
    else:
        exit_code = 1

    stream = sys.stdout if exit_code == 0 else sys.stderr
    for item in quarantined:
        print("series: QUARANTINED " + _quarantine_summary(item), file=stream)
    if batch_error is not None:
        print(batch_error, file=sys.stderr)
    if series_rows:
        represented = sum(int(row["series"].get("num_runs", 1)) for row in series_rows)
        print(
            f"series: appended {written} row(s) representing {represented} run(s) to {target}"
        )
    elif batch_error is None and not quarantined:
        print(
            "series: no completed cell rows; unmet prerequisites remain in the validate record",
            file=sys.stderr,
        )
    if quarantined:
        where = str(retention["path"]) if quarantine_error is None else "NOWHERE"
        print(
            f"series: quarantined {len(quarantined)} record(s) covering "
            f"{len({item['input_row'] for record_ in quarantined for item in record_['inputs']})}"
            f" of {total} input(s) to {where}",
            file=stream,
        )
    if quarantine_error is not None:
        print(
            f"series: FAILED to retain {len(quarantined)} quarantined record(s): "
            f"{quarantine_error}",
            file=sys.stderr,
        )
    stages: dict[str, int] = {}
    for item in quarantined:
        stages[item["stage"]] = stages.get(item["stage"], 0) + 1
    print("series: QUARANTINE_SUMMARY " + json.dumps({
        "schema": QUARANTINE_SUMMARY_SCHEMA,
        "run_id": run_id,
        "producer": producer,
        "tree": tree,
        "inputs": total,
        "rows_appended": written,
        "rows_admitted": len(series_rows),
        "quarantined_records": len(quarantined),
        "quarantined_inputs": len({
            item["input_row"] for record_ in quarantined for item in record_["inputs"]
        }),
        "stages": stages,
        "retained": quarantine_error is None,
        "written": retention["written"],
        "duplicates": retention["duplicates"],
        "published_check_error": retention["published_check_error"],
        "retention_error": None if quarantine_error is None else str(quarantine_error),
        "exit_code": exit_code,
    }, sort_keys=True), file=stream)
    return exit_code


def _depth_text(depth: Any) -> str:
    if not isinstance(depth, dict):
        return "-"
    repositories = []
    for name in sorted(depth):
        counts = depth.get(name)
        if not isinstance(counts, dict):
            continue
        commits = counts.get("commits")
        first_parent = counts.get("first_parent")
        if isinstance(commits, int) and isinstance(first_parent, int):
            repositories.append(f"{name}:{commits}/{first_parent}")
    return ",".join(repositories) or "-"


def _runtime_value(runtime: Any, key: str) -> str:
    if not isinstance(runtime, dict):
        return "-"
    values = []
    for run in ("run1", "run2"):
        part = runtime.get(run)
        value = part.get(key) if isinstance(part, dict) else None
        if isinstance(value, int) and not isinstance(value, bool):
            values.append(str(value))
        elif run == "run1":
            values.append("-")
    return "/".join(values) if values else "-"


def _wall_text(runtime: Any) -> str:
    if not isinstance(runtime, dict):
        return "-"
    minimum = runtime.get("wall_time_min_ms")
    maximum = runtime.get("wall_time_max_ms")
    if not isinstance(minimum, (int, float)) or isinstance(minimum, bool):
        return "-"
    if not isinstance(maximum, (int, float)) or isinstance(maximum, bool):
        return "-"
    return str(minimum) if minimum == maximum else f"{minimum}-{maximum}"


def _producer_text(producer: Any) -> str:
    if isinstance(producer, dict):
        tool = producer.get("tool")
        return str(tool) if tool else "-"
    return str(producer) if producer else "-"


def _run_span(payload: dict) -> str:
    first = payload.get("run_index")
    count = payload.get("num_runs", 1)
    if not isinstance(first, int) or not isinstance(count, int) or count < 1:
        return "-"
    attempt = payload.get("attempt")
    if isinstance(attempt, int) and not isinstance(attempt, bool):
        return f"{first} attempt {attempt}"
    if count == 1:
        return str(first)
    last = payload.get("last_run_index", first + count - 1)
    return f"{first}-{last} ({count})"


def _coordinate_text(payload: dict, coordinate: str) -> str:
    value = (payload.get("coordinates") or {}).get(coordinate)
    if isinstance(value, int) and not isinstance(value, bool):
        return str(value)
    return "-"


def _first_divergent_messages_text(payload: dict) -> str:
    messages = payload.get("first_divergent_messages")
    if not isinstance(messages, dict):
        return "-"
    return json.dumps(
        {side: messages.get(side) for side in FIRST_DIVERGENT_MESSAGE_KEYS},
        sort_keys=True,
        separators=(",", ":"),
    )


def _cmd_history(argv: list[str]) -> int:
    parent = Path(__file__).resolve().parents[2]
    wanted_test = None
    wanted_tree = None
    i = 0
    while i < len(argv):
        if argv[i] == "--parent" and i + 1 < len(argv):
            parent = Path(argv[i + 1])
            i += 2
            continue
        if argv[i] == "--test" and i + 1 < len(argv):
            wanted_test = argv[i + 1]
            i += 2
            continue
        if argv[i] == "--tree" and i + 1 < len(argv):
            wanted_tree = argv[i + 1]
            i += 2
            continue
        return _usage(f"unknown history argument {argv[i]!r}")
    if not wanted_test:
        return _usage("history requires --test <test id>")
    if wanted_tree is not None and _SHA_RE.fullmatch(wanted_tree) is None:
        return _usage("history --tree must be a full lowercase 40-hex commit sha")

    try:
        stored = history_store_rows(parent)
    except (MalformedEvent, SeriesError, OSError, subprocess.SubprocessError) as error:
        print(f"series: cannot read history: {error}", file=sys.stderr)
        return 2
    # The retired-id map is read from the same refreshed Hermit origin/main
    # that labels current ancestry below, so a test whose older rows carry a
    # retired id is joined by the map Hermit main publishes now.
    hermit_checkout = parent / "hermit"
    if _is_checkout(hermit_checkout):
        refresh_origin_main(hermit_checkout, "Hermit")
    try:
        retired_source = retired_ids_at(hermit_checkout, "origin/main")
    except (SeriesError, OSError, subprocess.SubprocessError) as error:
        print(f"series: cannot read Hermit's retired-id map: {error}", file=sys.stderr)
        return 2
    retired = retired_source[1] if retired_source is not None else None
    rows = history_rows(stored, wanted_test, wanted_tree, retired)
    if not rows:
        suffix = f" at {wanted_tree}" if wanted_tree else ""
        print(
            f"series: no runs recorded for test {wanted_test!r}{suffix}",
            file=sys.stderr,
        )
        return 1

    trees = [str((row.get("series") or {}).get("tree", "")) for row in rows]
    current = main_ancestry(hermit_checkout, trees)
    represented = sum(int((row.get("series") or {}).get("num_runs", 1)) for row in rows)
    print(
        f"test={wanted_test} rows={len(rows)} runs={represented} "
        "source=test-ledger-origin-main+local-spool"
    )
    resolved_test = resolve_test_id(wanted_test, retired)
    joined = sorted(
        {recorded for row in rows if (recorded := test_id(row)) is not None}
        | {wanted_test}
    )
    if retired_source is not None and joined != [resolved_test]:
        print(
            f"joined test ids {' + '.join(joined)} as {resolved_test} "
            f"(retired-id map {RETIRED_IDS_FILE} at Hermit {retired_source[0]})"
        )
    by_cell: dict[str, dict[str, Any]] = {}
    for row in rows:
        payload = row.get("series") or {}
        cell = resolve_cell(str(payload.get("cell", "-")), retired)
        totals = by_cell.setdefault(cell, {"runs": 0, "minimum": None, "maximum": None})
        totals["runs"] += int(payload.get("num_runs", 1))
        runtime = payload.get("runtime") or {}
        minimum = runtime.get("wall_time_min_ms")
        maximum = runtime.get("wall_time_max_ms")
        if isinstance(minimum, (int, float)) and not isinstance(minimum, bool):
            totals["minimum"] = (
                minimum if totals["minimum"] is None else min(totals["minimum"], minimum)
            )
        if isinstance(maximum, (int, float)) and not isinstance(maximum, bool):
            totals["maximum"] = (
                maximum if totals["maximum"] is None else max(totals["maximum"], maximum)
            )
    for cell, totals in sorted(by_cell.items()):
        minimum = totals["minimum"] if totals["minimum"] is not None else "-"
        maximum = totals["maximum"] if totals["maximum"] is not None else "-"
        print(
            f"cell={cell}\truns={totals['runs']}\t"
            f"wall_time_min_ms={minimum}\twall_time_max_ms={maximum}"
        )
    print(
        "day\tsha\tmain(current/recorded)\tdepth(repo:commits/first-parent)\tcell\toutcome\t"
        "result\tfailure_class\truns\t"
        "turns(run1/run2)\tvirtual_ns(run1/run2)\tsyscalls(run1/run2)\t"
        "wall_ms(min-max)\tfirst_divergent_record\tfirst_divergent_syscall\t"
        "first_divergent_scheduler_turn\tfirst_divergent_virtual_nanoseconds\t"
        "first_divergent_messages\tproducer\trun_id"
    )
    for row in rows:
        payload = row.get("series") or {}
        tree = str(payload.get("tree", ""))
        on_main, current_depth = current.get(tree, (None, None))
        recorded = payload.get("main_ancestry")
        current_text = "yes" if on_main is True else "no" if on_main is False else "unknown"
        recorded_text = (
            "yes" if recorded is True else "no" if recorded is False else "unknown"
        )
        main_text = f"{current_text}/{recorded_text}"
        depth = dict(payload.get("depth") or {})
        if current_depth is not None:
            depth["hermit"] = current_depth
        runtime = payload.get("runtime")
        emitted_at = str(row.get("emitted_at", ""))
        day = emitted_at[:10] if len(emitted_at) >= 10 else "-"
        print(
            "\t".join([
                day,
                tree or "-",
                main_text,
                _depth_text(depth),
                str(payload.get("cell", "-")),
                str(payload.get("outcome", "-")),
                str(payload.get("result") or "-"),
                str(payload.get("failure_class") or "-"),
                _run_span(payload),
                _runtime_value(runtime, "scheduler_turns"),
                _runtime_value(runtime, "virtual_nanoseconds"),
                _runtime_value(runtime, "syscalls"),
                _wall_text(runtime),
                _coordinate_text(payload, "first_divergent_record"),
                _coordinate_text(payload, "first_divergent_syscall"),
                _coordinate_text(payload, "first_divergent_scheduler_turn"),
                _coordinate_text(payload, "first_divergent_virtual_nanoseconds"),
                _first_divergent_messages_text(payload),
                _producer_text(row.get("producer")),
                str(row.get("run_id", "-")),
            ])
        )
    return 0


def _cmd_snapshot(argv: list[str]) -> int:
    if argv in (["--help"], ["-h"]):
        print(SCORECARD_SNAPSHOT_USAGE)
        return 0
    if any(argument in {"-h", "--help"} for argument in argv):
        return _snapshot_usage_error("help cannot be combined with other arguments")

    parent = None
    output = None
    i = 0
    while i < len(argv):
        if argv[i] == "--parent":
            if i + 1 >= len(argv):
                return _snapshot_usage_error("--parent requires a value")
            parent = argv[i + 1]
            i += 2
            continue
        if argv[i] == "--output":
            if i + 1 >= len(argv):
                return _snapshot_usage_error("--output requires a value")
            output = argv[i + 1]
            i += 2
            continue
        return _snapshot_usage_error(f"unknown snapshot argument {argv[i]!r}")
    missing = [
        name
        for name, value in (("--parent", parent), ("--output", output))
        if not value
    ]
    if missing:
        return _snapshot_usage_error(f"snapshot requires {', '.join(missing)}")
    output_path = Path(output)
    try:
        report = write_scorecard_projection_snapshot(parent, output_path)
    except (MalformedEvent, SeriesError, OSError, subprocess.SubprocessError) as error:
        staging_glob = output_path.parent / f".{output_path.name}.staging-*"
        print(
            f"series: snapshot failed: {error}\n"
            "series: output state is not asserted after failure; inspect "
            f"{output_path} and {staging_glob}. Preserve any file found, correct "
            "the named failure, then retry with a new run-scoped --output. Run "
            "`series.py snapshot --help` for usage.",
            file=sys.stderr,
        )
        return 1
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


def _snapshot_usage_error(message: str) -> int:
    print(f"series: {message}", file=sys.stderr)
    print("Run `series.py snapshot --help` for the supported flags.", file=sys.stderr)
    print(SCORECARD_SNAPSHOT_USAGE, file=sys.stderr)
    return 2


def _append_parity_usage_error(message: str) -> int:
    print(f"series: {message}", file=sys.stderr)
    print("Run `series.py append-parity --help` for the supported flags.", file=sys.stderr)
    print(APPEND_PARITY_USAGE, file=sys.stderr)
    return 2


def _usage(message: str) -> int:
    print(f"series: {message}", file=sys.stderr)
    print(
        "usage:\n"
        "  series.py append --parent <dev-hermit root>  < rows.jsonl\n"
        "  series.py append-cells --parent <root> "
        "--producer <validate|pressure-test|hermit-repeat>\n"
        "      --run-id <id> --tree <sha> [--checkout <Hermit checkout>]\n"
        "      [--emitted-at <UTC>] [--parity-file <run parity.jsonl>]... < cell-results.jsonl\n"
        "  series.py append-parity --parent <root> --producer <validate|pressure-test|parity-all>\n"
        "      --run-id <id> --tree <sha> --source-tree-dirty <true|false>\n"
        "      [--emitted-at <UTC>] < parity-ledger-sources.jsonl\n"
        "  series.py retract-parity --parent <root> --reason <text> [--emitted-at <UTC>]\n"
        "      < event-ids.txt\n"
        "  series.py retract-quarantine --parent <root> --reason <text> [--emitted-at <UTC>]\n"
        "      < quarantine-event-ids.txt\n"
        "  series.py history --test <test id> [--tree <sha>] [--parent <root>]\n"
        "  series.py snapshot --parent <root> --output <run-scoped.json>\n"
        "append validates complete series rows. append-cells converts typed harness\n"
        "results, compresses exact equal runs, and writes the UNPUBLISHED spool.\n"
        "append-parity spools one run's parity rows (hermit's ParityLedgerSource) with\n"
        "a run marker; empty stdin records a zero-record marker. A refused call is\n"
        "spooled as a parity.refusal carrying its message. retract-parity spools one\n"
        "parity.retraction withdrawing published parity events, one id per stdin line;\n"
        "retract-quarantine does the same for published series quarantine records.\n"
        "history freshly reads tracked shards from test-ledger origin/main plus that local spool.\n"
        "snapshot atomically writes that canonical union with immutable source identities;\n"
        "an unavailable source is refused and never replaced by stale tracked output.",
        file=sys.stderr,
    )
    return 2


def _cmd_retract_parity(argv: list[str]) -> int:
    """Spool one `parity.retraction` of published parity events (`RETRACT_PARITY_USAGE`)."""
    return _cmd_retract(argv, quarantine=False)


def _cmd_retract_quarantine(argv: list[str]) -> int:
    """Spool one `series.quarantine-retraction` of published quarantine records
    (`RETRACT_QUARANTINE_USAGE`)."""
    return _cmd_retract(argv, quarantine=True)


def _cmd_retract(argv: list[str], *, quarantine: bool) -> int:
    command, usage = (("retract-quarantine", RETRACT_QUARANTINE_USAGE) if quarantine
                      else ("retract-parity", RETRACT_PARITY_USAGE))
    if argv in (["--help"], ["-h"]):
        print(usage)
        return 0
    options: dict[str, str] = {}
    i = 0
    while i < len(argv):
        name = RETRACT_PARITY_FLAGS.get(argv[i])
        if name is None or i + 1 >= len(argv):
            return _usage(f"unknown {command} argument {argv[i]!r}")
        if name in options:
            return _usage(f"{command} supplies {argv[i]} more than once")
        options[name] = argv[i + 1]
        i += 2
    missing = [flag for flag, name in RETRACT_PARITY_FLAGS.items()
               if name in {"parent", "reason"} and not options.get(name)]
    if missing:
        return _usage(f"{command} requires {', '.join(missing)}")
    parity = parity_ledger_module()
    parent = options["parent"]
    ids = [line.strip() for line in sys.stdin.read().splitlines() if line.strip()]
    kind = "quarantine" if quarantine else "parity"
    try:
        event = parity.build_retraction(
            ids, options["reason"], team=parity.TEAM, host=_writer_host(),
            emitted_at=options.get("emitted_at") or _utc_now(),
            event_type=(parity.QUARANTINE_RETRACTION_EVENT_TYPE if quarantine
                        else parity.RETRACTION_EVENT_TYPE),
        )
        if quarantine:
            commit, records = published_quarantine_at(parent, "refs/remotes/origin/main")
            published = {str(record["event_id"]): record for record in records}
            published_check = f"read {len(published)} published quarantine record(s) at {commit}"
        else:
            published, published_check = _published_parity(parity, parent)
        unknown = [item for item in event["retracts"] if item not in published]
        if unknown:
            raise parity.ParityLedgerError(
                f"{len(unknown)} of {len(event['retracts'])} event id(s) are not published {kind} "
                f"{'records' if quarantine else 'events'} ({published_check}), first {unknown[0]}"
            )
        withdrawals = [item for item in event["retracts"]
                       if published[item].get("event_type") in parity.RETRACTION_EVENT_TYPES]
        if withdrawals:
            raise parity.ParityLedgerError(
                f"{len(withdrawals)} event id(s) name a published retraction, which is never withdrawn, "
                f"first {withdrawals[0]}"
            )
        spool = parity_spool_dir(parent)
        with series_spool_transition_lock(spool, exclusive=True):
            name = parity.spool_retraction(spool, event)
    except (SeriesError, parity.ParityLedgerError) as error:
        print(f"series: REFUSED the {kind} retraction, nothing was spooled: {error}", file=sys.stderr)
        return 1
    print(f"{kind}: spooled retraction {event['event_id']} of {len(event['retracts'])} event(s) as "
          f"{name}; {published_check}")
    return 0


def main(argv: list[str]) -> int:
    if not argv:
        return _usage("a subcommand is required")
    if argv[0] == "append":
        return _cmd_append(argv[1:])
    if argv[0] == "append-cells":
        return _cmd_append_cells(argv[1:])
    if argv[0] == "append-parity":
        return _cmd_append_parity(argv[1:])
    if argv[0] == "retract-parity":
        return _cmd_retract_parity(argv[1:])
    if argv[0] == "retract-quarantine":
        return _cmd_retract_quarantine(argv[1:])
    if argv[0] == "history":
        return _cmd_history(argv[1:])
    if argv[0] == "snapshot":
        return _cmd_snapshot(argv[1:])
    return _usage(f"unknown subcommand {argv[0]!r}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
