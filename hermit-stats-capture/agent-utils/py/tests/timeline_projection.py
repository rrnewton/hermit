"""Read a built archive's timeline as the schema-1 object the tests were written against.

A published build no longer writes ``data/timeline.json``. Dozens of assertions across this
suite were nonetheless written against that object, and they are the right assertions: they say
things about *the projection* -- that a reused subagent gets exactly one structural join, that a
backfilled phase invalidates its dependents, that an out-of-window overview is suppressed --
which have nothing to do with which container the projection is stored in.

Rewriting each of them against schema 3's shards would have retyped a hundred assertions in the
vocabulary of a storage format, and would have made every one of them a test of the reader as
well as of the thing it was about. So the container moves and the assertions do not: this
module reconstructs the schema-1 collections from whatever generation the archive actually has.

That reconstruction is also the *claim schema 3 makes about itself*, executed. `timeline_v3`
states that removing ``record_kind``, and removing ``at_ms`` from every kind but ``event``,
recovers the schema-1 record byte for byte, and that no record is duplicated across shards. This
function is that sentence as code, so a change that quietly broke it would take most of the
suite with it rather than one test named after the property.

**What is deliberately not reproduced is order.** Schema 3 sorts a timeline shard by instant and
a spine group by the kind's identifier, and neither is schema 1's insertion order. Collections
come back sorted by a stable key, so an assertion that indexes positionally into one of them is
asserting something this file cannot promise -- and, since the on-disk order is now a property of
the writer's sort rather than of the renderer, was asserting something it should not have.
"""

from __future__ import annotations

import gzip
import json
from pathlib import Path

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_object,
    as_string,
    read_json,
)
from wrkviz.timeline_v3 import (
    SCHEMA_3_BOOTSTRAP_PATH,
    SCHEMA_3_RECORD_KIND_KEY,
    SCHEMA_3_TIMESTAMP_KEY,
)

#: Which schema-1 collection each schema-3 record kind belongs to. ``phase_card`` and
#: ``activity_bounds`` are absent on purpose: both are derived views schema 1 never carried, and
#: folding them back in would invent records rather than recover them.
_COLLECTION_OF_KIND: dict[str, str] = {
    "team": "teams",
    "agent": "agents",
    "phase": "phases",
    "event": "events",
    "edge": "edges",
    "structural_edge": "edges",
    "rollup": "rollups",
    "project": "projects",
    "summary_file": "summary_files",
    "glossary_term": "glossary",
    "activity_bin": "activity_bins",
}

#: The field each collection is put back in order by, tiebroken on the encoded record so the
#: result is a function of the archive and not of dictionary iteration.
_SORT_FIELD: dict[str, str] = {
    "teams": "slug",
    "agents": "id",
    "phases": "start_ms",
    "events": "at_ms",
    "edges": "id",
    "rollups": "start_ms",
    "projects": "project_id",
    "summary_files": "path",
    "glossary": "id",
    "activity_bins": "start_ms",
}

#: Kinds that exist in schema 3 and in no schema-1 collection. The four search kinds are here for
#: the same reason ``phase_card`` is: the transcript search corpus is a *projection* of the events
#: and tool calls, built by `search_index.build_search_records` rather than carried in
#: ``data/timeline.json``, so reconstructing schema 1 from schema 3 must walk past it rather than
#: try to place it in a collection that has never existed.
_DERIVED_KINDS = frozenset(
    {
        "phase_card",
        "activity_bounds",
        "search_record",
        "search_bloom",
        "search_prompt",
        "search_response",
    }
)


def schema_1_timeline_text(archive: Path) -> str:
    """The schema-1 timeline of *archive* as JSON text, read or reconstructed.

    Reads ``data/timeline.json`` when it is there -- an archive written by an older tool, or the
    combined export's per-team intermediate, which still uses schema 1 as its handoff format --
    and otherwise rebuilds it from the schema-3 shards.

    **Text, not a parsed object,** so that a caller writes ``json.loads(schema_1_timeline_text(x))``
    and keeps exactly the types it had when it wrote ``json.loads(path.read_text())``. Returning a
    narrowed ``dict[str, JsonValue]`` would be the more honest signature and the wrong one here:
    every one of the hundreds of assertions this feeds indexes into nested JSON, `json.loads` is
    what made that typecheck, and re-narrowing at each of them would be several hundred lines of
    ceremony added to tests whose subject is the projection rather than its type.
    """

    path = archive / "data" / "timeline.json"
    if path.is_file():
        return path.read_text(encoding="utf-8")
    return json.dumps(reconstruct_from_schema_3(archive), sort_keys=True)


def reconstruct_from_schema_3(archive: Path) -> dict[str, JsonValue]:
    """Rebuild the schema-1 collections from a schema-3 generation."""

    bootstrap_path = archive / SCHEMA_3_BOOTSTRAP_PATH
    if not bootstrap_path.is_file():
        raise AssertionError(
            f"{archive} has neither data/timeline.json nor {SCHEMA_3_BOOTSTRAP_PATH}"
        )
    bootstrap = as_object(read_json(bootstrap_path), str(bootstrap_path))
    timeline: dict[str, JsonValue] = {"schema_version": 1}
    for field in (
        "generated_at",
        "source_digest",
        "display_timezone",
        "display_timezone_source",
        "range",
        "stats",
        "artifact_catalog_path",
        "glossary_path",
    ):
        if field in bootstrap:
            timeline[field] = bootstrap[field]

    collections: dict[str, list[dict[str, JsonValue]]] = {
        name: [] for name in set(_COLLECTION_OF_KIND.values())
    }
    overviews: list[dict[str, JsonValue]] = []
    streams = as_object(bootstrap.get("streams"), "timeline-v3.streams")
    for stream, raw_section in sorted(streams.items()):
        section = as_object(raw_section, f"streams.{stream}")
        for index, raw in enumerate(
            as_array(section.get("shards"), f"streams.{stream}.shards")
        ):
            entry = as_object(raw, f"streams.{stream}.shards[{index}]")
            relative = as_string(entry.get("path"), "shard.path")
            with gzip.open(archive / relative, "rb") as handle:
                for line in handle:
                    if not line.strip():
                        continue
                    record = as_object(
                        json.loads(line.decode("utf-8")), f"{relative} record"
                    )
                    kind = as_string(
                        record.pop(SCHEMA_3_RECORD_KIND_KEY), "record_kind"
                    )
                    if kind != "event":
                        record.pop(SCHEMA_3_TIMESTAMP_KEY, None)
                    if kind in _DERIVED_KINDS:
                        continue
                    if kind == "project_overview":
                        overviews.append(record)
                        continue
                    collections[_COLLECTION_OF_KIND[kind]].append(record)

    for name, records in collections.items():
        field = _SORT_FIELD[name]
        records.sort(key=lambda item: (_key(item, field), json.dumps(item, sort_keys=True)))
        timeline[name] = [record for record in records]
    # The single-team render emits one unlabelled overview object and the combined export emits
    # a labelled list; schema 3 stores both as one kind, and the label is what tells them apart.
    if len(overviews) == 1 and "team" not in overviews[0]:
        timeline["project_overview"] = overviews[0]
    elif overviews:
        overviews.sort(key=lambda item: _key(item, "team"))
        timeline["project_overviews"] = [record for record in overviews]
    return timeline


def _key(record: dict[str, JsonValue], field: str) -> tuple[int, str]:
    """A total order over a field that may be an int, a string, or missing."""

    value = record.get(field)
    if isinstance(value, bool) or value is None:
        return (2, "")
    if isinstance(value, int):
        return (0, f"{value:020d}")
    if isinstance(value, str):
        return (1, value)
    return (2, "")


__all__ = ["reconstruct_from_schema_3", "schema_1_timeline_text"]


def stored_path(path: Path) -> Path:
    """Resolve a logical archive path to the file that actually holds it.

    The archive stores some browser-facing documents as ``<name>.gz`` and nothing else -- the
    per-phase details and the artifact catalogue -- and the server materialises the identity
    bytes from that on demand. A test asserting the document *exists* is asserting about the
    resource, not about which of the two spellings the writer chose, so it asks here.
    """

    return path if path.is_file() else path.with_name(path.name + ".gz")


def detail_documents(details_root: Path) -> dict[Path, str]:
    """Every per-phase detail document under *details_root*, as text, keyed by its stored path.

    Details are stored as ``<phase>.json.gz`` with no identity twin, so a test that globs
    ``*.json`` finds an empty directory and reports it as "no details were written". Both
    spellings are accepted here because the tests are about the documents. Text, for the reason
    :func:`read_stored_text` gives.
    """

    found: dict[Path, str] = {}
    for path in sorted(details_root.glob("*.json.gz")):
        with gzip.open(path, "rt", encoding="utf-8") as handle:
            found[path] = handle.read()
    for path in sorted(details_root.glob("*.json")):
        found[path] = path.read_text(encoding="utf-8")
    return found


def read_stored_text(path: Path) -> str:
    """Return a stored archive document's text, whether it is stored plain or as a gzip member.

    Text rather than a parsed object, matching :func:`schema_1_timeline_text` above and for the
    same reason: the caller writes ``json.loads(...)`` and gets the loose typing a test assertion
    needs. Returning the archive's strict ``JsonValue`` instead would make every ``detail[...]``
    in this suite a type error about a recursive union, which is retyping a hundred assertions in
    the vocabulary of a reader they are not about.
    """

    if path.is_file():
        return path.read_text(encoding="utf-8")
    sidecar = path.with_name(path.name + ".gz")
    if not sidecar.is_file():
        raise FileNotFoundError(f"no such file: {path}")
    with gzip.open(sidecar, "rt", encoding="utf-8") as handle:
        return handle.read()
