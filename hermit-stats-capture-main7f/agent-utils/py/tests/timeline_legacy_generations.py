"""Produce an archive an *older* tool would have written, for the readers that still open one.

`timeline_shards.SCHEMA_2_IS_PUBLISHED` is ``False`` and :func:`write_timeline_shards` refuses to
run while it is. That refusal is not incidental: `archive_gc` offers a whole 1.4 GB generation to
the trash on the strength of the constant, so the constant has to be a fact about the writer and
not a comment beside one.

Two readers nonetheless still open schema 2 -- `query.py`, because an archive built last month has
nothing else, and `static/app.js`, for the same reason -- and a reader with no way to produce its
input cannot be tested. This module is that way, spelled out loudly rather than hidden behind a
helper that merely happens to work:

    with schema_2_writer_enabled():
        write_timeline_shards(root, timeline)

**Why monkeypatching the real writer and not a fixture of its own.** A second, test-only projection
would be free to drift away from what real archives contain, and the moment it did, every "the
legacy reader still works" test would be testing agreement between two pieces of test code. The
writer that made the archives on disk is the only thing that can produce another one of them.

**Why a context manager and not an autouse fixture.** The scope is the point. A test that builds a
schema-2 fixture and then asserts something about a *build* must not have the writer enabled while
the build runs, or it would be asserting against a world this tool no longer produces. Narrowing
the window to the fixture call keeps those two apart.
"""

from __future__ import annotations

import gzip
import json
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

from wrkviz import timeline_shards
from wrkviz.archive import JsonValue, as_object, narrow_json
from wrkviz.timeline_shards import write_timeline_shards
from wrkviz.timeline_v3 import SCHEMA_3_BOOTSTRAP_PATH

from tests.timeline_projection import reconstruct_from_schema_3


@contextmanager
def schema_2_writer_enabled() -> Iterator[None]:
    """Let :func:`timeline_shards.write_timeline_shards` run for the duration of the block."""

    previous = timeline_shards.SCHEMA_2_IS_PUBLISHED
    timeline_shards.SCHEMA_2_IS_PUBLISHED = True
    try:
        yield
    finally:
        timeline_shards.SCHEMA_2_IS_PUBLISHED = previous


def schema_3_search_records(archive: Path) -> list[dict[str, JsonValue]]:
    """The transcript search corpus of *archive*, as the flat record list a writer is handed.

    Read out of the schema-3 ``search`` stream and stripped of the one key the schema-3 envelope
    adds, which recovers the records `search_index.build_search_records` produced -- the same claim
    `timeline_v3` makes about the corpus and `test_timeline_v3_search.py` asserts. Reading them
    back is how :func:`write_legacy_schema_2` can put *this archive's* corpus into the older
    container instead of a plausible-looking substitute.

    ``gzip.open`` reads a multi-member file end to end, so no sidecar is needed here: this is the
    documented fallback path of :mod:`wrkviz.seekable_jsonl`, and using it keeps the
    helper independent of the seeking machinery whose tests it helps set up.
    """

    bootstrap_path = archive / SCHEMA_3_BOOTSTRAP_PATH
    if not bootstrap_path.is_file():
        return []
    bootstrap = json.loads(bootstrap_path.read_text(encoding="utf-8"))
    stream = bootstrap.get("streams", {}).get("search")
    if not stream:
        return []
    records: list[dict[str, JsonValue]] = []
    for shard in stream["shards"]:
        with gzip.open(archive / shard["path"], "rt", encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                record = json.loads(line)
                record.pop("record_kind", None)
                records.append(record)
    records.sort(key=lambda record: (record["at_ms"], record["ref"]))
    return records


def write_legacy_schema_2(archive: Path) -> None:
    """Give an already-built *archive* the schema-2 generation a pre-flip build would have left.

    This is the fixture almost every `archive_gc` test needs, and it is why the schema-2 writer
    outlives its own retirement. `gc`'s whole subject is what an *older* build put on disk, so a
    suite that could only produce what the current build writes could not test the collector at
    all -- and a hand-rolled imitation of the old output would be a fixture asserting agreement
    with itself.

    So the two inputs are taken from the archive rather than invented: the schema-1 projection is
    reconstructed from the schema-3 shards, and the search records are read back out of the
    schema-3 corpus. The result is the same records in the older container, which is exactly what
    an archive that predates the flip contains.
    """

    # Reconstructed from schema 3 rather than through `schema_1_timeline_text`, which prefers a
    # `data/timeline.json` if one is lying about. Several tests plant a *stub* monolith to give the
    # fallback something to lose, and reading that would produce a schema 2 describing an archive
    # nobody has.
    timeline = as_object(narrow_json(reconstruct_from_schema_3(archive)), "timeline")
    with schema_2_writer_enabled():
        write_timeline_shards(
            archive, timeline, search_records=schema_3_search_records(archive)
        )
