"""Content-addressed storage for the bulk text a tool call produced.

Until this existed, ingest *deleted* that text. ``pipeline._archive_team`` nulled ``input_text``
and ``output_text`` on every tool call before writing ``raw/team.json``, and its docstring said
plainly what the consequence was: "the original Codex JSONL remains the authority for command
stdout and patch bodies". It was measurably true -- 0 of the 30,921 tool calls in one archived
team retained either field -- and it is the single reason 3.59 GB of vendor JSONL under
``teams/*/source_snapshots/`` cannot be deleted. An archive that needs the vendor bytes to answer
"what did that command print" is not an archive of the run; it is an index into one.

**Why a separate store rather than fields on the tool call.** Command stdout and patch bodies are
at once the largest and the most sensitive content in the archive. On one measured Codex team they
are 290 MB against a 55 MB ``team.json``; across the archive they are the bytes that carry file
contents, credentials that leaked into a log line, and absolute paths under the operator's home
directory. Inlining them would make ``raw/team.json`` an order of magnitude larger *and* would put
that content under version control, which the archive deliberately does not do -- ``pipeline`` has
a test named ``test_ingest_never_persists_cwd_or_repository_credentials`` pinning exactly that
promise. Keeping the text in its own tree keeps both properties independent: the model stays small
and tracked, the bulk stays large and gitignored, and either can be pruned, permissioned,
encrypted or moved to cold storage without the other noticing. ``raw/team.json`` keeps only a
:class:`~wrkviz.model.PayloadRef` -- a digest and a byte count -- so an archive whose
payload tree has been pruned still knows exactly what it no longer has, which is a strictly better
state than the silent ``null`` this replaces.

**Why content-addressed.** The same text recurs: an identical ``{}`` argument, an identical "no
changes" output, the same file read twice. Deduplication is a secondary benefit and a small one
(1.5% on the measured team); the primary one is that the digest is a *name* the model can hold
without holding the bytes, and equality of names is equality of content with no comparison to run.
It also makes the merge below trivially correct: two ingests that observe the same text produce the
same record, so the union has no conflict to resolve.

**Why sharded by digest prefix rather than one file.** A single ``payloads.jsonl`` would be
rewritten in full every time one payload is added -- 290 MB of write amplification for one new
command. Splitting on the first byte of the digest gives 256 shards, so an ingest that observes new
text rewrites the shards it touched and leaves the rest byte-identical, which is what makes a
repeat ingest report zero changed files. The shard is also derivable from the digest alone, so a
reader that wants one payload opens one file rather than scanning the tree. That is deliberately
the same shape the archive is heading for everywhere: chunked, addressable, and never "decompress
everything to read three records".

**The line format is load-bearing, not incidental.** Every record is one canonical JSON object,
keys sorted, no whitespace:

    {"sha256":"<64 hex>","text":<json string>}

``"sha256"`` sorts before ``"text"``, and a digest is 64 characters of hex with nothing JSON has to
escape, so the digest of a record always occupies the same fixed byte range of its line. Building
the index of a shard is therefore a slice per line with no JSON parsing at all -- which matters
because the merge on every ingest has to know which digests are already stored, and parsing 290 MB
of JSON to learn 73,195 hex strings would be the dominant cost of ingest. The reader asserts the
prefix rather than falling back to parsing when it is absent: a line that does not have this shape
was not written by this module, and quietly coping with it would be the beginning of a second,
undeclared format.
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path

from wrkviz.archive import (
    ARCHIVE_MARKER_TOOL,
    LEGACY_ARCHIVE_MARKER_TOOL,
    JsonValue,
    as_int,
    as_object,
    as_string,
    canonical_json,
    canonical_jsonl,
    narrow_json,
    read_json,
    write_text_if_changed,
)
from wrkviz.model import PayloadRef

PAYLOAD_SCHEMA_VERSION = 1

#: The digest occupies this exact slice of every stored line. See the module docstring: the
#: canonical key order makes the offset a property of the format, not a guess about it.
_LINE_PREFIX = '{"sha256":"'
_DIGEST_START = len(_LINE_PREFIX)
_DIGEST_END = _DIGEST_START + 64
_AFTER_DIGEST = '","text":'

_MANIFEST_NAME = "manifest.json"
_SHARD_SUFFIX = ".jsonl"
_HEX = frozenset("0123456789abcdef")


@dataclass(frozen=True)
class PayloadShardRecord:
    """One shard's contribution to the manifest: what it holds and what it hashes to."""

    name: str
    records: int
    text_bytes: int
    sha256: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the shard record as a JSON-serializable object."""

        return {
            "name": self.name,
            "records": self.records,
            "text_bytes": self.text_bytes,
            "sha256": self.sha256,
        }


@dataclass(frozen=True)
class PayloadManifest:
    """The complete inventory of one team's payload tree.

    It is the tree's own record of itself, and it is deliberately *not* bound into
    ``raw/normalized-generation.json`` the way ``team.json`` and the task notes are. That marker
    exists to catch a stale generation, and a payload cannot be stale -- its name is its content
    and the store is a union that never rewrites a record. What the tree can be is incomplete,
    which is precisely the state it is designed to permit, because it is gitignored bulk an
    operator is invited to prune, permission or move to cold storage. Binding it to the marker
    would turn each of those into a refusal on the next build and a re-ingest that recreated the
    bytes they had just removed.

    So the manifest answers a narrower question, asked deliberately rather than on every read: is
    the tree still what it says it is? :func:`verify_payload_store` compares each recorded shard
    hash against the bytes on disk and each stored digest against its own text, and reports a
    pruned shard as the absence it is. Nothing rehashes shards on the read path, because doing
    that on every load would cost far more than the read it was protecting.

    The per-shard records also carry forward across ingests: a shard this ingest did not touch
    keeps the record the previous one wrote for it, which is what makes a repeat ingest cheap.
    """

    records: int
    text_bytes: int
    shards: tuple[PayloadShardRecord, ...]

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the manifest as a JSON-serializable object."""

        return {
            "schema_version": PAYLOAD_SCHEMA_VERSION,
            "tool": ARCHIVE_MARKER_TOOL,
            "records": self.records,
            "text_bytes": self.text_bytes,
            "shards": [shard.to_json_obj() for shard in self.shards],
        }

    def digest(self) -> str:
        """Return the single content address of the whole tree, for the generation marker."""

        return hashlib.sha256(canonical_json(self.to_json_obj()).encode("utf-8")).hexdigest()


EMPTY_PAYLOAD_MANIFEST = PayloadManifest(records=0, text_bytes=0, shards=())


@dataclass(frozen=True)
class PayloadWriteReport:
    """What one merge holds afterwards, what this run of it added, and what it found missing."""

    stored: int
    stored_bytes: int
    newly_stored: int
    newly_stored_bytes: int
    files_changed: int
    manifest: PayloadManifest
    #: Shards the previous manifest recorded and this merge did not find. Not an error -- the tree
    #: is prunable by design -- but reported, because the only alternative to reporting it is a
    #: manifest that silently forgets, and then nobody can say what the archive stopped holding.
    pruned_shards: tuple[str, ...] = ()
    #: Shards whose bytes disagreed with the digest recorded for them and were therefore
    #: re-measured. This is the one state a content-addressed union is not supposed to be able to
    #: reach, so it is surfaced rather than repaired in silence.
    damaged_shards: tuple[str, ...] = ()


def payload_digest(text: str) -> str:
    """Return the content address of one payload."""

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def payload_ref(text: str) -> PayloadRef:
    """Return the reference ``raw/team.json`` keeps in place of *text*."""

    return PayloadRef(sha256=payload_digest(text), byte_length=len(text.encode("utf-8")))


def shard_name(sha256: str) -> str:
    """Return the name of the one shard file that can hold this digest."""

    _validate_digest(sha256, "payload digest")
    return f"{sha256[:2]}{_SHARD_SUFFIX}"


def _validate_digest(value: str, where: str) -> None:
    if len(value) != 64 or not _HEX.issuperset(value):
        raise ValueError(f"{where}: not a lowercase sha-256 hex digest: {value!r}")


def _encode(digest: str, text: str) -> str:
    """Return the stored record for *text*, without its line terminator.

    Terminatorless because every other operation here compares a record against one read back
    through ``splitlines()``, and a trailing newline on one side of that comparison would make an
    already-stored payload look like a conflicting one. The writer adds the terminator when it
    joins a shard, which is also the only place it can be added exactly once.
    """

    line = canonical_jsonl(({"sha256": digest, "text": text},)).rstrip("\n")
    if not line.startswith(_LINE_PREFIX) or not line[_DIGEST_END:].startswith(_AFTER_DIGEST):
        raise ValueError(
            "the canonical payload encoding no longer places the digest at a fixed offset; "
            "the fixed-offset index in this module is invalid"
        )
    return line


def _digest_of_line(line: str, where: str) -> str:
    if not line.startswith(_LINE_PREFIX) or not line[_DIGEST_END:].startswith(_AFTER_DIGEST):
        raise ValueError(f"{where}: payload record does not have the canonical shape")
    digest = line[_DIGEST_START:_DIGEST_END]
    _validate_digest(digest, where)
    return digest


def _text_of_line(line: str, where: str) -> str:
    value = as_object(narrow_json(json.loads(line), where), where)
    return as_string(value.get("text"), f"{where}.text")


def _shard_paths(root: Path) -> tuple[Path, ...]:
    if root.is_symlink() or not root.is_dir():
        return ()
    return tuple(
        sorted(
            path
            for path in root.iterdir()
            if path.is_file()
            and not path.is_symlink()
            and path.name.endswith(_SHARD_SUFFIX)
            and len(path.name) == len(_SHARD_SUFFIX) + 2
            and _HEX.issuperset(path.name[:2])
        )
    )


def _shard_lines(text: str) -> list[str]:
    """Split a shard file into records, on the one character that terminates a record.

    ``str.splitlines()`` would be the obvious call and it is the wrong one, for a reason specific
    to what is stored here. :func:`~wrkviz.archive.canonical_jsonl` writes with
    ``ensure_ascii=False``, and JSON escapes nothing above U+001F -- so a payload containing
    U+2028, U+2029 or U+0085 goes to disk raw, inside the string, and ``splitlines`` treats all
    three as line terminators. One physical line would be read back as two, neither of which has
    the canonical shape, and from then on every read, ingest and audit of that team raises. There
    is no repair short of deleting the shard by hand. Payloads are command stdout: arbitrary text,
    from arbitrary programs, over arbitrary files. Assuming those three characters never appear in
    it is not an assumption this store is entitled to make.

    Blank entries are kept rather than filtered, so that the index the callers enumerate stays the
    physical line number a diagnostic has to name.
    """

    return text.split("\n")


def _index_shard_lines(text: str, path: Path) -> dict[str, str]:
    """Index already-read shard text as digest -> whole line, parsing no JSON at all."""

    lines: dict[str, str] = {}
    for number, line in enumerate(_shard_lines(text), 1):
        if not line:
            continue
        where = f"{path}:{number}"
        digest = _digest_of_line(line, where)
        previous = lines.get(digest)
        if previous is not None and previous != line:
            raise ValueError(f"{where}: two different payloads are stored under digest {digest}")
        lines[digest] = line
    return lines


def _read_shard_lines(path: Path) -> dict[str, str]:
    """Return the shard's records as digest -> whole line, parsing no JSON at all."""

    if path.is_symlink() or not path.is_file():
        return {}
    return _index_shard_lines(path.read_text(encoding="utf-8"), path)


def read_payload_digests(root: Path) -> frozenset[str]:
    """Return every digest the store holds, without materializing any payload text."""

    digests: set[str] = set()
    for path in _shard_paths(root):
        digests.update(_read_shard_lines(path))
    return frozenset(digests)


def read_payload(root: Path, sha256: str) -> str | None:
    """Return one stored payload, reading only the shard that can hold it.

    ``None`` means the store does not have it, which is a legitimate state rather than an error:
    the tree is prunable by design. A caller that needs to tell "pruned" from "never stored" has
    the byte count on the :class:`~wrkviz.model.PayloadRef` to say which.
    """

    path = root / shard_name(sha256)
    line = _read_shard_lines(path).get(sha256)
    if line is None:
        return None
    return _text_of_line(line, f"{path}#{sha256}")


def resolve_payloads(root: Path, refs: Sequence[PayloadRef]) -> dict[str, str]:
    """Return the stored text for every reference that resolves, keyed by digest.

    Grouped by shard so that resolving every reference in a team opens each shard once instead of
    once per reference; a per-reference :func:`read_payload` loop over 48,712 tool calls would
    re-read the whole tree roughly 190 times.
    """

    wanted = {ref.sha256 for ref in refs}
    resolved: dict[str, str] = {}
    for name in sorted({shard_name(digest) for digest in wanted}):
        path = root / name
        for digest, line in _read_shard_lines(path).items():
            if digest in wanted:
                resolved[digest] = _text_of_line(line, f"{path}#{digest}")
    return resolved


def load_payload_manifest(root: Path) -> PayloadManifest | None:
    """Read the recorded inventory, or ``None`` for a team that has never stored a payload."""

    path = root / _MANIFEST_NAME
    if path.is_symlink() or not path.is_file():
        return None
    obj = as_object(read_json(path), str(path))
    # BOTH spellings of `tool`, because this manifest is written into the build store and every
    # one already on disk predates the rename. Accepting only the current name made all twelve
    # teams fail `normalize` with "invalid payload manifest" -- a total ingestion outage from a
    # spelling change, on a file whose bytes were perfectly valid.
    #
    # This is the same class of mistake the rename already fixed four times over, and it was
    # missed for a specific reason worth recording: the sweep for stored identifiers grepped the
    # PUBLISHED archive, and the payload store had just been moved out of it into `<output>.build`.
    # A rename must sweep the build store and the snapshot store too, not only what ships.
    if (
        obj.get("schema_version") != PAYLOAD_SCHEMA_VERSION
        or obj.get("tool") not in (ARCHIVE_MARKER_TOOL, LEGACY_ARCHIVE_MARKER_TOOL)
    ):
        raise ValueError(f"invalid payload manifest at {path}")
    raw_shards = obj.get("shards")
    if not isinstance(raw_shards, list):
        raise ValueError(f"{path}: payload manifest shards is not an array")
    shards: list[PayloadShardRecord] = []
    for index, item in enumerate(raw_shards):
        where = f"{path}: shards[{index}]"
        entry = as_object(item, where)
        shards.append(
            PayloadShardRecord(
                name=as_string(entry.get("name"), f"{where}.name"),
                records=as_int(entry.get("records"), f"{where}.records"),
                text_bytes=as_int(entry.get("text_bytes"), f"{where}.text_bytes"),
                sha256=as_string(entry.get("sha256"), f"{where}.sha256"),
            )
        )
    return PayloadManifest(
        records=as_int(obj.get("records"), f"{path}.records"),
        text_bytes=as_int(obj.get("text_bytes"), f"{path}.text_bytes"),
        shards=tuple(shards),
    )


def merge_payloads(root: Path, texts: Iterable[str]) -> PayloadWriteReport:
    """Union the stored payloads with the observed ones and rewrite only what changed.

    A union for the same reason ``pipeline._merge_promoted_task_notes`` is one: this tree is the
    archive's copy, so an ingest that no longer observes a payload -- a narrower date window, a
    lineage re-ingested under a different team selection, a vendor log rotated away -- must not be
    able to delete it. Nothing here can shrink the store.

    The content address makes "first promotion wins" degenerate rather than a policy: two ingests
    that observe the same text produce the same line, so there is no older copy to prefer and no
    provenance to restamp. That is exactly why a payload carries none of the
    ``origin``/``projection_*`` provenance a task note carries -- a task note's identity is a
    mutable upstream row id and its body can be edited beneath it, while a payload's identity *is*
    its body, so "where did this come from" is answered by whichever tool calls reference it.

    Two different payloads under one digest is a refusal, not a merge. In practice that means the
    file was edited outside this module, because the alternative is a sha-256 collision.

    **A pruned shard is expected input, not tampering.** This module, ``ARCHITECTURE.md`` and the
    user guide all promise that ``payloads/`` can be pruned, permissioned or moved to cold storage
    and the archive still says precisely which text is missing. An earlier draft refused outright
    when the manifest's shard set differed from the tree's, which withdrew that promise in the
    worst possible way: the refusal happened on the *next ingest*, every subsequent ingest, with no
    documented escape, so exercising the supported operation permanently froze the team -- and it
    fired on an ingest that was in the act of re-observing the very bytes that had been removed and
    would have restored them. A missing shard is therefore dropped from the manifest and reported,
    which is the same treatment :func:`verify_payload_store` gives it.

    **A shrunken shard is reported, never laundered.** For a shard this merge is going to rewrite,
    the recorded digest is checked against the bytes actually on disk before its ``text_bytes`` is
    carried forward. Without that check a torn write or a partial prune was silently absorbed: the
    merge rebuilt the shard from what it could read, kept the stale byte count, and stamped a fresh
    hash over it, after which ``verify_payload_store`` reported the tree clean. The store's own
    tamper-evidence was erased by the next ingest. Untouched shards are still not re-hashed -- that
    is the branch that makes a repeat ingest cheap, and the whole-tree pass has a name and a caller
    of its own -- so what this adds is bounded by the shards the ingest was rewriting anyway.
    """

    incoming: dict[str, str] = {}
    incoming_bytes: dict[str, int] = {}
    for text in texts:
        digest = payload_digest(text)
        if digest not in incoming:
            incoming[digest] = _encode(digest, text)
            incoming_bytes[digest] = len(text.encode("utf-8"))
    by_shard: dict[str, dict[str, str]] = {}
    for digest, line in incoming.items():
        by_shard.setdefault(shard_name(digest), {})[digest] = line

    previous = load_payload_manifest(root)
    previous_shards = {shard.name: shard for shard in previous.shards} if previous else {}
    present = {path.name for path in _shard_paths(root)}
    pruned = tuple(sorted(set(previous_shards) - present))
    if not present and not by_shard and previous is None:
        return PayloadWriteReport(0, 0, 0, 0, 0, EMPTY_PAYLOAD_MANIFEST)

    files_changed = 0
    newly_stored = 0
    newly_stored_bytes = 0
    damaged: list[str] = []
    shard_records: list[PayloadShardRecord] = []
    for name in sorted(present | set(by_shard)):
        # A record for a shard that is no longer on disk describes bytes this tree does not have,
        # so it is not carried: `pruned` above reports the absence and the manifest stops claiming
        # it. Note this also covers the awkward case of a pruned shard that today's ingest has new
        # text for -- carrying the old byte count into a file rebuilt from scratch would invent an
        # inventory nobody can substantiate.
        carried = previous_shards.get(name) if name in present else None
        additions = by_shard.get(name, {})
        path = root / name
        if not additions and carried is not None:
            # Untouched: keep the previous ingest's record rather than re-reading and re-hashing
            # bytes that provably did not change. This is the branch that makes a repeat ingest
            # cheap, and on the steady state it is every shard but a handful.
            shard_records.append(carried)
            continue
        raw = path.read_text(encoding="utf-8") if name in present else ""
        on_disk = hashlib.sha256(raw.encode("utf-8")).hexdigest()
        if carried is not None and on_disk != carried.sha256:
            # The bytes are not the bytes this manifest describes. Whatever did that -- a torn
            # write, a partial prune, an editor -- the recorded `text_bytes` is now a claim about
            # content that may no longer be here, so it is discarded and the shard is measured from
            # what is actually present. The discrepancy goes into the report because a rewritten
            # manifest is a rewritten alibi: after this merge nothing else would ever notice.
            damaged.append(
                f"{name}: shard bytes did not match the digest recorded for them; "
                "the shard was re-measured from what is on disk"
            )
            carried = None
        stored = _index_shard_lines(raw, path)
        # A shard with no manifest record has to be measured; that happens on the first merge into
        # an existing tree, after a prune, and after the check above rejected a stale record.
        base_bytes = (
            carried.text_bytes
            if carried is not None
            else sum(
                len(_text_of_line(line, f"{path}#{digest}").encode("utf-8"))
                for digest, line in stored.items()
            )
        )
        added = 0
        added_bytes = 0
        for digest, line in additions.items():
            held = stored.get(digest)
            if held is None:
                stored[digest] = line
                added += 1
                added_bytes += incoming_bytes[digest]
            elif held != line:
                raise ValueError(
                    f"{path}: two different payloads are stored under digest {digest}"
                )
        text = "".join(f"{stored[digest]}\n" for digest in sorted(stored))
        files_changed += int(write_text_if_changed(path, text))
        newly_stored += added
        newly_stored_bytes += added_bytes
        shard_records.append(
            PayloadShardRecord(
                name=name,
                records=len(stored),
                text_bytes=base_bytes + added_bytes,
                sha256=hashlib.sha256(text.encode("utf-8")).hexdigest(),
            )
        )

    manifest = PayloadManifest(
        records=sum(shard.records for shard in shard_records),
        text_bytes=sum(shard.text_bytes for shard in shard_records),
        shards=tuple(shard_records),
    )
    files_changed += int(
        write_text_if_changed(root / _MANIFEST_NAME, canonical_json(manifest.to_json_obj()))
    )
    return PayloadWriteReport(
        stored=manifest.records,
        stored_bytes=manifest.text_bytes,
        newly_stored=newly_stored,
        newly_stored_bytes=newly_stored_bytes,
        files_changed=files_changed,
        manifest=manifest,
        pruned_shards=pruned,
        damaged_shards=tuple(damaged),
    )


def verify_payload_store(root: Path) -> tuple[str, ...]:
    """Recompute every digest from its own text and return the discrepancies found.

    The merge trusts the stored digest as a key, because recomputing the sha-256 of the whole tree
    on every ingest to confirm what the format already guarantees would dominate the cost of
    ingest for no information in the overwhelmingly common case. This is where that confirmation
    lives instead: a deliberate, occasional, whole-tree pass. The losslessness audit calls it
    before it is willing to say the vendor bytes are redundant, because "the archive has the text"
    is only true if the text the archive has is the text it claims to have.
    """

    problems: list[str] = []
    manifest = load_payload_manifest(root)
    recorded = {shard.name: shard for shard in manifest.shards} if manifest else {}
    present = {path.name: path for path in _shard_paths(root)}
    for name in sorted(set(recorded) | set(present)):
        path = present.get(name)
        if path is None:
            problems.append(f"{name}: recorded in the manifest but absent from the tree")
            continue
        text = path.read_text(encoding="utf-8")
        shard = recorded.get(name)
        if shard is None:
            problems.append(f"{name}: present in the tree but absent from the manifest")
        elif hashlib.sha256(text.encode("utf-8")).hexdigest() != shard.sha256:
            problems.append(f"{name}: shard bytes do not match the digest recorded for them")
        for number, line in enumerate(_shard_lines(text), 1):
            if not line:
                continue
            where = f"{name}:{number}"
            claimed = _digest_of_line(line, where)
            actual = payload_digest(_text_of_line(line, where))
            if actual != claimed:
                problems.append(f"{where}: text hashes to {actual}, stored under {claimed}")
            elif shard_name(claimed) != name:
                problems.append(f"{where}: digest {claimed} belongs in a different shard")
    return tuple(problems)


__all__ = [
    "EMPTY_PAYLOAD_MANIFEST",
    "PAYLOAD_SCHEMA_VERSION",
    "PayloadManifest",
    "PayloadShardRecord",
    "PayloadWriteReport",
    "load_payload_manifest",
    "merge_payloads",
    "payload_digest",
    "payload_ref",
    "read_payload",
    "read_payload_digests",
    "resolve_payloads",
    "shard_name",
    "verify_payload_store",
]
