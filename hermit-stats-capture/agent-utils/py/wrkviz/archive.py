"""Small, deterministic primitives for the version-controllable timeline archive."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import re
import tempfile
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path

JsonScalar = str | int | float | bool | None
JsonValue = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]

#: What makes a directory an archive this tool manages, rather than a directory that happens to
#: hold some JSON. It lives here, in the module every other one imports, because three unrelated
#: places now need the same answer -- the pipeline writes it, `gc` refuses to sweep a directory
#: without it, and the snapshot store asks whether the archive a store claims still exists -- and
#: the import graph gives them no other common home: `pipeline` imports `snapshot_store`, so the
#: name could not simply live in the one that writes it.
#: What marks a directory as an archive this tool manages, and what it says inside.
#:
#: Both changed when the tool was renamed, and BOTH have a legacy spelling that must keep working:
#: an archive built before the rename has `.agent-team-timeline.json` on disk saying
#: `"tool": "agent-team-timeline"`, and refusing it would mean every existing archive stopped
#: being recognised as an archive at all -- the failure would be "refusing non-empty non-archive
#: output directory", which reads as a safety refusal rather than as a rename.
#:
#: Resolution accepts either and a build rewrites the marker to the current spelling, so an
#: archive migrates the first time it is written to and no operator has to do anything. The legacy
#: constants stay because the migration cannot be assumed to have happened: a read-only command --
#: `gc --dry-run`, the losslessness audit -- must recognise an archive it is not allowed to write.
ARCHIVE_MARKER_FILE = ".wrkviz.json"
LEGACY_ARCHIVE_MARKER_FILE = ".agent-team-timeline.json"
ARCHIVE_MARKER_TOOL = "wrkviz"
LEGACY_ARCHIVE_MARKER_TOOL = "agent-team-timeline"


def archive_marker_path(archive: Path) -> Path:
    """Return the marker this archive actually has, preferring the current spelling.

    Returns the CURRENT path when neither exists, so a caller creating an archive creates it under
    the current name rather than perpetuating the old one.
    """

    current = archive / ARCHIVE_MARKER_FILE
    if current.is_file():
        return current
    legacy = archive / LEGACY_ARCHIVE_MARKER_FILE
    return legacy if legacy.is_file() else current


def is_archive_marker(marker: Mapping[str, JsonValue]) -> bool:
    """Whether *marker* is this tool's marker, under either the current or the former name."""

    return (
        marker.get("tool") in (ARCHIVE_MARKER_TOOL, LEGACY_ARCHIVE_MARKER_TOOL)
        and marker.get("schema_version") == 1
    )


def canonical_json(value: JsonValue) -> str:
    """Return the archive's stable JSON representation."""

    return json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def canonical_jsonl(records: Iterable[Mapping[str, JsonValue]]) -> str:
    """Return one canonical JSON object per line, in the order given.

    The indented whole-file form above is what a human diffs; this one is what a reader can
    consume a record at a time without materializing the file. Both are sorted-key and
    ``ensure_ascii=False`` so the same record hashes the same either way, and both end in a
    newline so appending never merges two records into one line.
    """

    return "".join(
        json.dumps(dict(record), ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
        for record in records
    )


def read_jsonl(path: Path) -> list[dict[str, JsonValue]]:
    """Read a line-delimited JSON record file, treating an absent file as empty.

    Absent means "nothing has written this yet", which every caller here handles by starting
    from an empty record set; a symlink or a non-regular file means something other than this
    tool owns the name, and that is refused rather than followed. Empty lines are skipped rather
    than refused, because a blank line carries no record and rejecting one would turn a harmless
    stray newline into an unreadable archive. Every line reports its own number, because "invalid
    JSON" without a line number in a several-hundred-thousand-line record file is not a
    diagnosis.

    **Split on ``"\\n"``, never with ``str.splitlines()``.** This is the reader for files
    :func:`canonical_jsonl` wrote, and that writer uses ``ensure_ascii=False``, so U+2028, U+2029
    and U+0085 -- which JSON does not escape and ``splitlines`` *does* treat as line terminators
    -- go to disk raw inside a string value. With ``splitlines`` one such character in one note's
    text turns one physical line into two JSON fragments, and the file becomes permanently
    unparseable: not a bad read, an archive that can no longer be loaded or re-ingested, poisoned
    by content the archive itself just wrote. There are no such characters in the corpus today;
    there is also nothing stopping the next agent message from containing one.
    """

    if not path.exists():
        return []
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"line-delimited JSON record file is not a regular file: {path}")
    result: list[dict[str, JsonValue]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").split("\n"), 1):
        if not line:
            continue
        value = narrow_json(json.loads(line), f"{path}:{line_number}")
        result.append(as_object(value, f"{path}:{line_number}"))
    return result


def content_hash(text: str) -> str:
    """Hash UTF-8 text for cache keys and provenance."""

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def write_text_if_changed(path: Path, text: str, *, executable: bool = False) -> bool:
    """Atomically replace *path* only when its bytes differ.

    Avoiding an identical rewrite is important for archives checked into Git: a formatting-only
    rebuild neither churns mtimes nor obscures which source or summary actually changed.
    """

    if path.is_file() and path.read_text(encoding="utf-8") == text:
        if executable:
            path.chmod(path.stat().st_mode | 0o111)
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, raw_tmp = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    tmp = Path(raw_tmp)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        if executable:
            tmp.chmod(0o755)
        os.replace(tmp, path)
    finally:
        if tmp.exists():
            tmp.unlink()
    return True


def write_json_if_changed(path: Path, value: JsonValue) -> bool:
    """Write deterministic JSON through :func:`write_text_if_changed`."""

    return write_text_if_changed(path, canonical_json(value))


def write_text_durable(path: Path, text: str) -> bool:
    """Atomically write text and persist the replaced directory entry before returning.

    ``write_text_if_changed`` fsyncs the file's *contents* and then renames, which is enough for
    the bytes but not for the name: after a crash the directory entry can still point at the old
    inode. Every caller that writes a file another run will later treat as authoritative wants
    both, so the parent fsync lives here rather than being restated at each call site.
    """

    changed = write_text_if_changed(path, text)
    if not changed:
        return False
    try:
        parent_fd = os.open(
            path.parent,
            os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW,
        )
    except OSError as exc:
        raise OSError(f"cannot open parent directory for durable write {path}: {exc}") from exc
    try:
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)
    return True


def write_json_durable(path: Path, value: JsonValue) -> bool:
    """Atomically write JSON and persist the replaced directory entry before returning."""

    return write_text_durable(path, canonical_json(value))


#: The archive's team-slug grammar. It lives here, beside the other primitives every layer shares,
#: because three modules now need it -- ingestion, which creates the directory; the snapshot store,
#: which names a directory after it outside the archive; and the migration that moves one to the
#: other. Restating the pattern in each would let them drift, and the one that drifts furthest is
#: the one furthest from the archive, which is exactly the one where a slug that is not a slug
#: turns into a path traversal.
_TEAM_SLUG = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")


def validate_team_slug(team_slug: str) -> None:
    """Refuse anything that is not a bare, filesystem-safe team slug."""

    if len(team_slug) > 64 or _TEAM_SLUG.fullmatch(team_slug) is None:
        raise ValueError(
            "team slug must be 1-64 lowercase letters/digits separated by single hyphens"
        )


def read_json(path: Path) -> JsonValue:
    """Read JSON while narrowing it to the archive's recursive value type."""

    raw: object = json.loads(path.read_text(encoding="utf-8"))
    return narrow_json(raw, str(path))


def read_json_stored(path: Path) -> JsonValue:
    """Read ``path``, accepting a stored ``path.gz`` as the same document.

    Resources whose stored form is the compressed member have no identity file on disk, so a
    reader that opens the ``.json`` name directly finds nothing. This resolves the name the way
    the server does -- identity first, then the gzip member -- so a caller can keep naming the
    logical path without caring which of the two the writer chose.
    """

    if path.is_file():
        return read_json(path)
    sidecar = path.with_name(path.name + ".gz")
    if not sidecar.is_file():
        # The original name, not the sidecar's: the caller asked for a document, and the fact
        # that this looked for a compressed copy too is not the thing that went wrong.
        raise FileNotFoundError(f"no such file: {path}")
    with gzip.open(sidecar, "rt", encoding="utf-8") as handle:
        raw: object = json.load(handle)
    return narrow_json(raw, str(path))


def narrow_json(raw: object, where: str = "JSON") -> JsonValue:
    """Reject non-JSON values and non-string object keys."""

    if raw is None or isinstance(raw, (str, bool, int, float)):
        return raw
    if isinstance(raw, list):
        return [narrow_json(item, where) for item in raw]
    if isinstance(raw, dict):
        result: dict[str, JsonValue] = {}
        for key, item in raw.items():
            if not isinstance(key, str):
                raise ValueError(f"{where}: object key is not a string")
            result[key] = narrow_json(item, where)
        return result
    raise ValueError(f"{where}: unsupported JSON value {type(raw).__name__}")


def as_object(value: JsonValue, where: str) -> dict[str, JsonValue]:
    """Return *value* as a JSON object or raise a contextual error."""

    if not isinstance(value, dict):
        raise ValueError(f"{where}: expected an object")
    return value


def as_array(value: JsonValue, where: str) -> list[JsonValue]:
    """Return *value* as a JSON array or raise a contextual error."""

    if not isinstance(value, list):
        raise ValueError(f"{where}: expected an array")
    return value


def as_string(value: JsonValue, where: str) -> str:
    """Return *value* as a string or raise a contextual error."""

    if not isinstance(value, str):
        raise ValueError(f"{where}: expected a string")
    return value


def as_int(value: JsonValue, where: str) -> int:
    """Return *value* as a non-boolean integer or raise a contextual error."""

    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{where}: expected an integer")
    return value


def string_map(values: Mapping[str, str]) -> dict[str, JsonValue]:
    """Widen a string mapping to the recursive JSON value type."""

    return {key: value for key, value in values.items()}


def json_sequence(values: Sequence[JsonValue]) -> list[JsonValue]:
    """Copy a sequence into a mutable JSON array."""

    return list(values)
