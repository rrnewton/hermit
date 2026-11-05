"""Reclaim what a built archive no longer produces, reversibly and under the writer's lock.

An archive is append-heavy by construction. Every build publishes a new generation of the
presentation timeline beside the last, every format generation that was ever written stays
written until something removes it, and the two mechanisms that already remove things --
`render._remove_stale_presentation_files` and `timeline_shards._finish_object_generation` --
only ever reap what the *current* build's own manifest stopped naming. Neither can see a whole
format generation that a newer tool stopped emitting, because a file nobody writes is a file no
manifest lists, and "absent from the manifest" is exactly the signal those two use to mean
"delete". A retired generation therefore falls into the one gap between them: too dead to be
rewritten, too unnamed to be reaped.

This module is that gap, made explicit. It is a **third** user of the same mark-and-sweep idiom
rather than a second idiom:

* :func:`timeline_shards._finish_object_generation` marks with `current_objects`, sweeps
  `previous - current - retained`, and holds a `retained_objects` set: the objects the *previous*
  generation named and this one no longer does, kept alive one more generation so that a browser
  which already loaded the previous bootstrap can still fetch what it points at. That grace is
  gated by `_scope_allows_retention`, which grants it only when the new build covered the same
  teams and a window no narrower -- that is, only when the new generation genuinely supersedes
  the old one. A narrowed re-export gets no grace and takes the older objects with it, which is
  consistent rather than surprising: the archive it leaves behind describes exactly what was
  re-exported. The precedent to carry across is the shape, not the scope rule: **something is
  released only once a named successor has demonstrably taken over its job, and never merely
  because the last build did not mention it.**
* :func:`render.prune_retired_query_artifacts` removes one specific retired output -- the
  generated `query.py` launcher -- and only when it can prove the file is the one the tool
  wrote, either because a manifest claimed it or because its bytes still match the bundled
  source.

`gc` generalises both across *format generations*, and inherits both of their refusals. Nothing
is reclaimed on the strength of "the current build did not write it". Something is reclaimed only
when a **named superseding artefact is present and complete**, checked by the reader's own
acceptance rule, or when a manifest that is authoritative over a directory says the file is in
neither its current nor its retained set.

Retiring a whole generation needs one thing more
------------------------------------------------
A superseding artefact says the archive can still be read without the old generation. It does not
say the old generation has *stopped being produced*, and for a format that is still written those
are different: reclaiming this build's own output is churn, and the operator who runs `gc` after
every build would pay for it every time. The schema-1 monolith did not have to ask, because it
was already unwritten when it was given a category. Schema 2 does, because it is 1.4 GB and it is
still the website's only format.

The answer cannot come from the archive. Objects are content addressed and the bootstrap goes
through `write_text_if_changed`, so a build that republishes an identical schema 2 moves no mtime
at all -- on a measured archive the newest schema-2 object was thirteen hours *older* than the
schema-3 bootstrap while that same build's `data/export.json` named all 661 schema-2 files as its
own output. So the answer comes from the code: `timeline_shards.SCHEMA_2_IS_PUBLISHED`, which
`timeline_shards.write_timeline_shards` enforces by refusing to run while it is false. "No build
did" is an observation and this module refuses to act on one; "no build can" is a property, and
that is what a category worth 1.4 GB is gated on. See :func:`_schema_2_retirement_refusal`.

…and one thing that had no successor until it did
-------------------------------------------------
"Schema 3 replaces schema 2" used to be true of the *presentation timeline* and false of the
archive: the transcript search corpus was a set of schema-2 content-addressed day shards with
trigram blooms, catalogued in the ``search`` section of ``data/timeline-v2.json``, and a search
under schema 3 read phases and agents from the spine and *messages from schema 2*. That is 500.5
MiB of the 1,434.2 MiB this module sees in the generation on the measured archive -- 35% -- and it
was not a duplicate copy of anything, so reclaiming it would not have made the archive slower, it
would have deleted `timeline search` and `timeline show` outright.

Schema 3 now publishes a corpus of its own: the ``search``, ``search_bloom`` and ``search_links``
streams of `timeline_v3`, read by ``query.TimelineQuery._iter_search_records``, which opens
``data/timeline-v2.json`` only for an archive built before they existed. So the corpus finally has
the thing every other category here requires -- a *named, present* successor -- and
:func:`_schema_2_search_corpus_category` asks the archive whether it has one instead of holding
unconditionally.

The two facts stay separate, which is the part worth keeping. Retiring the *format* and superseding
the *corpus* are still different events with different evidence: the format is retired by a
constant in the writer (:data:`timeline_shards.SCHEMA_2_IS_PUBLISHED`) and the corpus is superseded
by shards this archive can be seen to hold. An archive can be in either state without the other --
the common one today is a schema 3 that carries the corpus while the format is still published for
the website -- and in that state the corpus bytes join the whole-generation category and are held
there with the format's own reason, rather than being held twice under two.

Two more questions the retirement has to ask
--------------------------------------------
`query.schema_3_completeness` is the reader's acceptance rule and it is entirely
*intra-generation*: all five clauses compare the schema-3 bootstrap against itself and against
its own tree. It cannot see that the generation beside it is **newer**, and one exists --
`render.render_archive` publishes schema 2 and then schema 3, so a build killed in between leaves
a new schema 2 next to an untouched old schema 3, self-consistent and a team short. Reclaiming
on completeness alone would then take the newer generation and keep the older one, which is the
one outcome worse than reclaiming nothing. The reader already treats that mismatch as a refusal
where it can see it -- ``_search_bootstrap`` compares ``source_digest`` across the two bootstraps
and declines rather than papering over it -- so :func:`_cross_generation_refusal` makes the same
comparison *before* the sweep, because the reader only makes it when somebody searches and by
then the bytes are gone. See :func:`_cross_generation_refusal` for why this is asked of schema 2
and not of the monolith.

The other question is the browser. `gc` reclaims without rebuilding -- that is the point of it --
but the archive carries its **own copy** of ``app.js``, written by the build that made it, and
`gc` does not rewrite it. A copy from before the flip has no schema-3 mode at all: it loads
``data/timeline-v2.json`` as its timeline and falls back to ``data/timeline.json``, and this pass
offers both. So the graphical surface would go silently, at the moment the operator was told they
were reclaiming a superseded copy. :func:`_website_refusal` asks the positive question instead --
does this archive's own ``app.js`` name the schema-3 bootstrap -- and holds the generation until a
build has put one there. That is the same shape as every other precondition here: a named
successor has to be *present*, not merely conceivable.

The rejected design: sweeping everything ``data/export.json`` does not name
-------------------------------------------------------------------------
The export manifest is a complete positive inventory of one build's presentation output, so
"delete everything under the archive that is not in it" looks like the whole feature in one
line. It is refused, for two reasons that are different failures.

The first is scope. The manifest covers the *presentation* tree and nothing else, while the
archive it lives in also holds `teams/`, `runs/`, `extracted/`, `qa/`, the marker, the lock, and
whatever an operator put there -- none of which any manifest names and all of which that rule
would propose deleting. The negation of a partial inventory is not a garbage set.

The second is that it is the very inference this module exists to *avoid*. "The last build did
not write it" is what `_remove_stale_presentation_files` already acts on, correctly, within the
one directory its manifest is authoritative over -- and it is precisely why the schema-1 monolith
had to be given a `retired_files` seat rather than being left to that sweep. Rebuilding the same
inference at archive scope, where nothing is authoritative, would generalise the hazard instead
of the mechanism. So `gc` reasons only forwards: a file goes when something *else*, named and
verified, has taken over its job.

Why the first pass moves rather than unlinks
--------------------------------------------
The measured archive costs hours of model time to rebuild and holds 9.1 GB across roughly forty
thousand files; the single largest thing `gc` can reclaim is a 246,973,399-byte JSON document.
A wrong `unlink` there is not a slow afternoon, it is a re-ingest. So the first destructive
operation is `os.replace` into ``.wrkviz-trash/<generation>/files/<original path>``,
which preserves the archive-relative path exactly, so undoing a sweep is copying one directory
tree back over the archive. The generation directory carries a ``receipt.json`` naming every
file, its category, its size and the reason it was judged dead -- written *before* the first move
as a statement of intent and rewritten after the last one as a record, so that a sweep which dies
partway still leaves the operator a list rather than a traceback. See :func:`_receipt`.

Emptying the trash is a **separate invocation with a separate flag**. That is the whole design:
`--delete` is reversible and `--empty-trash` is not, and no single command does both, because a
command that swept and emptied in one pass would have exactly the failure mode the trash exists
to prevent.

What `gc` will not touch, and why
---------------------------------
* **`teams/*/source_snapshots/`** -- 6,524,876,923 bytes across 995 files on the measured
  archive, 72% of it, and genuinely not output: it is the vendor material ingest reads. It is
  already gitignored and already excluded from every manifest. `gc` reports it, at its full size,
  because an operator asking "what is in my output that is not output" deserves that number as
  the first line of the answer -- and then declines to reclaim it, because the archive has a
  purpose-built gate for exactly that deletion (`audit-losslessness --require-lossless`, whose
  own help says "run this before deleting source snapshots"). A second, weaker gate beside a
  strong one is not a convenience.
* **`retained_objects`** -- the schema-2 objects the last build deliberately kept alive for one
  more generation, so that a reader holding the previous bootstrap can still fetch what it
  names. Reclaiming them would collapse that grace period to zero at exactly the moment it is
  being used, since the operator most likely to run `gc` is the one who has just rebuilt and is
  looking at what the rebuild left behind -- which is when a browser tab from before the rebuild
  is still open. The next build supersedes them and they go on their own. That reasoning holds
  only while something still *writes* schema 2; when the format itself is retired the grace
  period has nobody left to protect, and the whole generation moves into one category. See
  :func:`_schema_2_categories`.
* **The transcript search corpus, when this archive's schema 3 does not carry one.** The
  schema-2 bootstrap and the day shards and linkage sidecars its ``search`` section names -- 500.5
  MiB and 144 objects on the measured archive. Held at full size while it is the only copy, and
  held *even once the schema-2 presentation format is retired*, because retiring a format does not
  by itself give a corpus a successor. An archive whose schema 3 publishes the ``search`` streams
  has one, and there these bytes stop being a capability and become a duplicate like everything
  else in the generation. See :func:`_schema_2_search_corpus_category`.
* **Schema-3 shards the bootstrap does not name.** Reported, never reclaimed. Absence from the
  catalogue does not say whether the shard is a retired team's or a live team's: the answer turns
  entirely on whether the bootstrap beside it is newer or older than the shard, which nothing on
  disk states. So the publisher settles it instead -- `timeline_v3.write_timeline_v3` clears its
  own root of everything outside its plan once its bootstrap is published -- and any leftover
  `gc` can see is therefore residue from a build that did not finish. The remedy is to run one,
  not to sweep. See :func:`_schema_3_category`.
* **Anything at all, when the artefact that would supersede it is absent or incomplete.** The
  measured archive today has no schema-3 generation, so a `gc` run against it reclaims nothing
  from the schema-1 monolith and says so in a sentence. That is the intended behaviour on a
  first run and the reason the report distinguishes "reclaimable" from "held (<reason>)" instead
  of printing one total.

Concurrency
-----------
Both the dry run and the sweep take :func:`pipeline.archive_writer_lock`. The dry run takes it
too -- despite reading nothing -- because its output is the input to a human's decision, and a
listing computed beside a running build names files the build is halfway through replacing.

A reader mid-flight is a different question, and the answer is POSIX rather than a lock: readers
of a built archive take no lock (that is the point of the format), so `gc` can move a file out
from under one. A reader that already has the file open keeps reading it, because the rename
does not disturb the open description; a reader that has not opened it yet gets `ENOENT` on a
file it learned about from a manifest that `gc` has, by construction, already established does
not name it. The residual case -- a reader that read a stale manifest before the sweep and opens
the path after it -- degrades to a missing file rather than a wrong answer, and unlike the
existing stale-file removal it is recoverable from the trash.
"""

from __future__ import annotations

import errno
import os
import re
import stat as stat_module
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

from wrkviz.build_store import candidate_store_roots
from wrkviz.archive import (
    archive_marker_path,
    JsonValue,
    as_array,
    as_object,
    as_string,
    canonical_json,
    read_json,
    write_text_if_changed,
)
from wrkviz.pipeline import (
    ARCHIVE_TRASH_ROOT,
    LEGACY_ARCHIVE_TRASH_ROOT,
    archive_writer_lock,
)
from wrkviz.query import schema_3_completeness
from wrkviz.timeline_shards import (
    SCHEMA_2_BOOTSTRAP_PATH,
    SCHEMA_2_ROOT,
    schema_2_is_published,
)
from wrkviz.timeline_v3 import SCHEMA_3_BOOTSTRAP_PATH, SCHEMA_3_ROOT

#: Where a swept file goes. Dot-prefixed and at the archive root, beside the marker and the lock,
#: because it is control state rather than content; :func:`pipeline._ensure_bulk_content_ignored`
#: adds it to the archive's own ``.gitignore`` for the same reason it adds the lock, and the name
#: is declared there so that the ignore rule and the directory cannot drift apart.
TRASH_ROOT = ARCHIVE_TRASH_ROOT
LEGACY_TRASH_ROOT = LEGACY_ARCHIVE_TRASH_ROOT

#: The receipt's own name, kept inside a ``files/`` sibling rather than beside the reclaimed
#: tree, so that reclaiming an archive path that happens to be called ``receipt.json`` can never
#: overwrite the record of why it was reclaimed.
_RECEIPT_NAME = "receipt.json"
_TRASH_PAYLOAD = "files"

_SCHEMA_1_TIMELINE = "data/timeline.json"
_SCHEMA_2_OBJECT_ROOT = "data/timeline-v2/objects"
_SCHEMA_2_MANIFEST = "data/timeline-v2/manifest.json"
_SOURCE_SNAPSHOT_DIRECTORY = "source_snapshots"
_OBJECT_NAME = re.compile(r"[0-9a-f]{64}\.json(?:\.gz)?")
_OBJECT_DIGEST = re.compile(r"[0-9a-f]{64}")
_GENERATION_STAMP = "%Y%m%dT%H%M%SZ"

#: The archive's own copy of the browser bundle. Named here rather than imported from `render`
#: because what this module needs is not "the file a build writes" but "the file a *past* build
#: left in this archive", and those are the same path and deliberately not the same claim -- see
#: :func:`_website_refusal`.
_WEBSITE_APP = "app.js"


class ArchiveGcError(ValueError):
    """A collection this module refuses to plan or perform.

    Distinct from a bare ``ValueError`` so the CLI can tell "the archive is not in a state this
    can reason about" from "a narrowing helper found the wrong type", and so a refusal is never
    mistaken for an empty report -- reclaiming nothing and *failing* to decide are opposite
    outcomes that would otherwise print the same zero.
    """


@dataclass(frozen=True)
class GcFile:
    """One classified file: where it is, and how much reclaiming it would return."""

    relative_path: str
    bytes: int

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Render this file for the JSON report and for the trash receipt."""

        return {"path": self.relative_path, "bytes": self.bytes}


@dataclass(frozen=True)
class GcCategory:
    """One reason a set of files is dead, or one reason it only looks dead.

    ``reclaimable`` and ``reason`` are both always populated, and that is deliberate: a category
    with no files still carries its reason, so a dry run that reclaims nothing explains itself
    rather than printing a bare zero the operator has to interpret.
    """

    name: str
    reclaimable: bool
    reason: str
    files: tuple[GcFile, ...]

    @property
    def bytes(self) -> int:
        """How much disk this category holds."""

        return sum(item.bytes for item in self.files)

    @property
    def count(self) -> int:
        """How many files it holds."""

        return len(self.files)

    def to_json_obj(self, *, include_files: bool) -> dict[str, JsonValue]:
        """Render this category, with its paths only when the caller wants the long form."""

        result: dict[str, JsonValue] = {
            "category": self.name,
            "reclaimable": self.reclaimable,
            "reason": self.reason,
            "files": self.count,
            "bytes": self.bytes,
        }
        if include_files:
            listed: list[JsonValue] = [item.to_json_obj() for item in self.files]
            result["paths"] = listed
        return result


@dataclass(frozen=True)
class GcReport:
    """What one ``gc`` invocation found, and what -- if anything -- it did about it."""

    root: str
    action: str
    total_bytes: int
    total_files: int
    categories: tuple[GcCategory, ...]
    trash_relative_path: str
    trash_generation: str | None
    moved: tuple[GcFile, ...]
    removed_directories: tuple[str, ...]
    emptied_bytes: int

    @property
    def reclaimable_bytes(self) -> int:
        """How much this pass would return to the filesystem."""

        return sum(item.bytes for item in self.categories if item.reclaimable)

    @property
    def reclaimable_files(self) -> int:
        """How many files that is."""

        return sum(item.count for item in self.categories if item.reclaimable)

    @property
    def held_bytes(self) -> int:
        """How much is classified but deliberately not reclaimed, with a reason per category."""

        return sum(item.bytes for item in self.categories if not item.reclaimable)

    def category(self, name: str) -> GcCategory:
        """The named category, which always exists -- an absent one would be an unread reason."""

        for item in self.categories:
            if item.name == name:
                return item
        raise KeyError(name)

    def to_json_obj(self, *, include_files: bool = False) -> dict[str, JsonValue]:
        """Render the whole report as the JSON the `--format json` caller parses."""

        categories: list[JsonValue] = [
            item.to_json_obj(include_files=include_files) for item in self.categories
        ]
        moved: list[JsonValue] = [item.to_json_obj() for item in self.moved]
        directories: list[JsonValue] = [value for value in self.removed_directories]
        return {
            "schema_version": 1,
            "kind": "archive-gc-report",
            "archive": self.root,
            "action": self.action,
            "archive_bytes": self.total_bytes,
            "archive_files": self.total_files,
            "reclaimable_bytes": self.reclaimable_bytes,
            "reclaimable_files": self.reclaimable_files,
            "held_bytes": self.held_bytes,
            "trash": self.trash_relative_path,
            "trash_generation": self.trash_generation,
            "moved": moved,
            "removed_directories": directories,
            "emptied_bytes": self.emptied_bytes,
            "categories": categories,
        }


def _archive_path(root: Path, relative: str) -> Path:
    """Resolve one archive-relative path, refusing to leave the archive or cross a symlink.

    The same shape as `render._generated_path` and `timeline_shards._safe_output_path`, repeated
    here for the reason those two are repeated in each other: this module's whole job is to
    *remove* things, so it is the one place where borrowing a path helper whose guarantees are
    stated for a writer would be a mistake worth making explicit rather than importing.
    """

    parts = PurePosixPath(relative)
    if parts.is_absolute() or not parts.parts or any(
        part in ("", ".", "..") for part in parts.parts
    ):
        raise ArchiveGcError(f"unsafe archive path: {relative!r}")
    resolved_root = root.resolve()
    cursor = root
    for part in parts.parts[:-1]:
        cursor = cursor / part
        if cursor.is_symlink():
            raise ArchiveGcError(f"archive path crosses a symlink: {relative!r}")
    candidate = root.joinpath(*parts.parts)
    if candidate.is_symlink():
        raise ArchiveGcError(f"refusing to collect through a symlink: {relative!r}")
    try:
        candidate.parent.resolve().relative_to(resolved_root)
    except ValueError as error:
        raise ArchiveGcError(f"archive path escapes the archive: {relative!r}") from error
    return candidate


def _size_of(root: Path, relative: str) -> int | None:
    """The size of a regular, non-symlinked file, or ``None`` if it is not one."""

    path = root / relative
    try:
        stat_result = path.lstat()
    except OSError:
        return None
    if not stat_module.S_ISREG(stat_result.st_mode):
        return None
    return stat_result.st_size


def _walk_files(root: Path, relative_root: str) -> Iterator[tuple[str, int]]:
    """Every regular file under *relative_root*, as ``(archive-relative path, bytes)``.

    Symlinked directories are not descended into and symlinked files are not reported, so a
    listing can never grow a path that leaves the archive -- which is the property that lets
    everything downstream treat a reported path as collectable.
    """

    start = root / relative_root if relative_root else root
    if not start.is_dir() or start.is_symlink():
        return
    for directory, subdirectories, names in os.walk(start, followlinks=False):
        subdirectories[:] = sorted(
            name
            for name in subdirectories
            if not Path(directory, name).is_symlink()
        )
        for name in sorted(names):
            path = Path(directory, name)
            try:
                stat_result = path.lstat()
            except OSError:
                continue
            if not stat_module.S_ISREG(stat_result.st_mode):
                continue
            yield path.relative_to(root).as_posix(), stat_result.st_size


def _files_with_sizes(root: Path, relatives: Iterable[str]) -> tuple[GcFile, ...]:
    found: list[GcFile] = []
    for relative in sorted(set(relatives)):
        size = _size_of(root, relative)
        if size is not None:
            found.append(GcFile(relative, size))
    return tuple(found)


def _schema_1_category(root: Path) -> GcCategory:
    """The schema-1 monolith, reclaimable only once both later generations can answer for it.

    Two conditions, not one, because the archive has two readers and they retired schema 1 at
    different times. ``./timeline`` reads schema 3 first and schema 2 second and reaches schema 1
    only when neither is present, so schema 3 alone would satisfy it. The **website** is the
    second reader, and the one that needs asking about, because a build *copies* `app.js` into
    the archive: a bundle written before schema 3 existed loads `data/timeline-v2.json` and falls
    back to `data/timeline.json` when that load throws, and it will go on doing so forever, since
    no tool release reaches back into an archive somebody built last month. Schema 3's presence is
    judged by :func:`query.schema_3_completeness` -- the reader's own five-clause rule, not a
    looser restatement of it that could say yes where the reader says no.

    **The second condition is asked of this archive, not of this release**, and that is a change
    from how it was first written. It used to read ``schema_2_is_published() and the bootstrap is
    absent``, on the reasoning that the flip is only legal beside a browser that reads schema 3,
    so the condition could lapse with the constant. That reasoning is true about the *tool* and
    false about a *directory*: the constant is now ``False`` and there are still archives on disk
    whose own bundle has never heard of schema 3, and for one of those with no schema 2 beside it
    the monolith is the last thing its graphical surface can open. Reclaiming it there blanks the
    page -- silently, hours later, in somebody else's session, which is the failure mode
    :func:`_website_refusal` exists to prevent and which does not become acceptable because a
    different file was being deleted.

    So the question is the same per-archive one, asked with the same function: does *this*
    archive's bundle name the schema-3 bootstrap. If it does, the monolith has no reader left and
    goes. If it does not, it goes anyway as long as schema 2 is there to catch that bundle -- and
    is held only when neither is. The remedy is the one `_website_refusal` names: rebuild, which
    costs no tokens and republishes the bundle.
    """

    files = _files_with_sizes(root, (_SCHEMA_1_TIMELINE, _SCHEMA_1_TIMELINE + ".gz"))
    complete, declined = schema_3_completeness(root)
    if not complete:
        return GcCategory(
            "schema-1-monolith",
            False,
            f"held: no complete schema-3 generation supersedes it ({declined})",
            files,
        )
    if (
        _website_refusal(root) is not None
        and _size_of(root, SCHEMA_2_BOOTSTRAP_PATH) is None
    ):
        return GcCategory(
            "schema-1-monolith",
            False,
            f"held: this archive's own {_WEBSITE_APP} has no schema-3 mode, so it reads schema 2 "
            f"and falls back to schema 1, and {SCHEMA_2_BOOTSTRAP_PATH} is absent. Re-run the "
            "build against this archive, which republishes the bundle",
            files,
        )
    beside = (
        f" beside {SCHEMA_2_BOOTSTRAP_PATH}"
        if _size_of(root, SCHEMA_2_BOOTSTRAP_PATH) is not None
        else ""
    )
    return GcCategory(
        "schema-1-monolith",
        True,
        f"superseded by a complete {SCHEMA_3_BOOTSTRAP_PATH}{beside}; no build writes it any "
        "more",
        files,
    )


def _schema_2_manifest_sets(root: Path) -> tuple[frozenset[str], frozenset[str]] | None:
    """The v2 manifest's ``(current, retained)`` sets, or ``None`` when it cannot be trusted.

    ``None`` is returned rather than an empty pair for an absent or unrecognised manifest,
    because an empty pair would make every object on disk look like an orphan. That is exactly
    the misreading `timeline_shards._previous_objects` refuses on the same file, where an absent
    manifest means "an older tool wrote these and recorded no reachability", and the safe
    interpretation is that everything is live.
    """

    path = root / _SCHEMA_2_MANIFEST
    if path.is_symlink() or not path.is_file():
        return None
    try:
        manifest = as_object(read_json(path), str(path))
        if (
            manifest.get("schema_version") != 1
            or manifest.get("kind") != "timeline-shard-files"
        ):
            return None
        current = frozenset(
            as_string(value, f"{path}.current_objects[{index}]")
            for index, value in enumerate(
                as_array(manifest.get("current_objects"), f"{path}.current_objects")
            )
        )
        retained = frozenset(
            as_string(value, f"{path}.retained_objects[{index}]")
            for index, value in enumerate(
                as_array(manifest.get("retained_objects"), f"{path}.retained_objects")
            )
        )
    except (OSError, ValueError):
        return None
    return current, retained


#: Named in the reasons below rather than paraphrased, because an operator looking at 1.4 GB held
#: by a constant is entitled to know which constant, and because the string is what a future
#: reader greps for when they want to know what makes this category fire.
_SCHEMA_2_CAPABILITY = "timeline_shards.SCHEMA_2_IS_PUBLISHED"


def _bootstrap(
    root: Path, relative: str, version: int, kind: str
) -> dict[str, JsonValue] | None:
    """One generation's bootstrap, or ``None`` when this reader cannot vouch for it.

    ``None`` collapses "absent" and "unreadable", which every caller here then has to separate
    for itself, and that is on purpose: the two mean opposite things to a collector. An absent
    bootstrap is a generation that is not there, and a generation that is not there holds
    nothing; an unreadable one is a generation whose contents cannot be enumerated, and a
    collector that treated it as empty would classify every file it names as unclaimed. So the
    narrowing is done here and the interpretation is done at each call site, in the open.
    """

    path = root / relative
    if path.is_symlink() or not path.is_file():
        return None
    try:
        document = as_object(read_json(path), str(path))
    except (OSError, ValueError):
        return None
    if document.get("schema_version") != version or document.get("kind") != kind:
        return None
    return document


def _schema_2_search_paths(root: Path) -> frozenset[str] | None:
    """Every schema-2 file the transcript search corpus is made of, or ``None`` if unknowable.

    An empty set is a real answer and means "this archive has no transcript search corpus": either
    there is no schema-2 bootstrap at all, or the one there is carries no ``search`` section --
    which is what an archive predating transcript search looks like, and what an archive built
    after the corpus finds a schema-3 home will look like. ``None`` is the other answer, "there is
    a bootstrap and it cannot be parsed", and the caller turns that into a refusal rather than
    into an empty set, because a corpus whose members cannot be listed is a corpus whose members
    cannot be protected.

    The set is taken from the catalogue rather than from the shape of the filenames, because the
    two halves of the generation are stored in the same content-addressed directory and are
    indistinguishable on disk: ``data/timeline-v2/objects/<sha256>.json`` is a day of transcript
    or a page of presentation depending only on which section of the bootstrap points at it.
    Names cannot answer that. The catalogue can, exactly, and it is the same list
    ``query._iter_search_records`` walks -- ``search.shards[].sha256`` for the day shard and
    ``search.shards[].linkage.sha256`` for the prompt/response sidecar
    ``query._search_link_context_from_sidecars`` reads.

    The ``.gz`` twin of each object is included even though the reader only ever opens the plain
    ``.json`` -- the digest is over the uncompressed bytes -- because the twin is what a browser
    fetches, and leaving the compressed half of a held corpus behind to be swept would keep the
    corpus readable by exactly one of its two readers.
    """

    path = root / SCHEMA_2_BOOTSTRAP_PATH
    if path.is_symlink() or not path.is_file():
        return frozenset()
    bootstrap = _bootstrap(root, SCHEMA_2_BOOTSTRAP_PATH, 2, "timeline-bootstrap")
    if bootstrap is None:
        return None
    raw_search = bootstrap.get("search")
    if raw_search is None:
        return frozenset()
    named: set[str] = {SCHEMA_2_BOOTSTRAP_PATH, SCHEMA_2_BOOTSTRAP_PATH + ".gz"}
    try:
        search = as_object(raw_search, "timeline-v2.search")
        for index, raw_shard in enumerate(
            as_array(search.get("shards"), "timeline-v2.search.shards")
        ):
            where = f"timeline-v2.search.shards[{index}]"
            shard = as_object(raw_shard, where)
            references = [(shard, where)]
            raw_linkage = shard.get("linkage")
            if raw_linkage is not None:
                references.append((as_object(raw_linkage, where + ".linkage"), where + ".linkage"))
            for reference, source in references:
                digest = as_string(reference.get("sha256"), source + ".sha256")
                if _OBJECT_DIGEST.fullmatch(digest) is None:
                    return None
                named.add(f"{_SCHEMA_2_OBJECT_ROOT}/{digest}.json")
                named.add(f"{_SCHEMA_2_OBJECT_ROOT}/{digest}.json.gz")
    except ValueError:
        return None
    return frozenset(named)


def _cross_generation_refusal(root: Path) -> str | None:
    """Whether the schema-3 generation describes the *same build* as the schema 2 beside it.

    `query.schema_3_completeness` cannot answer this and is not defective for not answering it:
    all five of its clauses are about one generation's internal agreement -- the bootstrap parses,
    the codec is one this reader implements, the spine covers the declared teams, every named
    shard is present at its declared length, and nothing on disk is unnamed. A generation can
    satisfy every one of those and still be **older than the generation it is being used to
    retire**, and one specific accident produces exactly that: `render.render_archive` writes
    schema 2 and then schema 3, in that order, with no atomicity across the pair, so a build that
    dies in between -- OOM, ENOSPC, a validation error inside `write_timeline_v3` -- leaves a new
    schema 2 next to the previous build's schema 3, untouched and perfectly self-consistent. The
    writer's lock is released by process death, so nothing stops `gc` running next.

    Reclaiming there would take the newer generation and keep the older one. On a three-team
    export whose schema 3 is a build behind, that is a team whose presentation data exists in no
    readable generation at all -- and the pass would say ``superseded``, because from inside
    schema 3 everything is in order.

    ``source_digest`` settles it, and this is not a new rule invented for a collector: it is the
    reader's, from ``query.TimelineQuery._search_bootstrap``, which refuses outright when the two
    generations disagree because "the phases would come from one build and the messages from
    another". The reader makes that comparison lazily -- only when something searches -- which is
    correct for a reader and useless here, because by the time anybody searches `gc` has already
    moved the bytes. So the same comparison is made eagerly, once, before the sweep.

    **Asked of schema 2 and not of the schema-1 monolith**, deliberately. The monolith is not
    written by a published build at all, so it cannot be the newest generation in an archive; the
    worst it can be is stale, and a stale fallback behind two live generations loses nothing when
    it goes. Schema 2 is written by *every* build, which is what makes "it might be newer than
    schema 3" a state an archive can actually be in.
    """

    schema_3 = _bootstrap(root, SCHEMA_3_BOOTSTRAP_PATH, 3, "timeline-v3-bootstrap")
    schema_2 = _bootstrap(root, SCHEMA_2_BOOTSTRAP_PATH, 2, "timeline-bootstrap")
    if schema_3 is None or schema_2 is None:
        return None
    theirs = schema_3.get("source_digest")
    ours = schema_2.get("source_digest")
    if theirs is None or ours is None or theirs == ours:
        return None
    return (
        f"held: {SCHEMA_3_BOOTSTRAP_PATH} and {SCHEMA_2_BOOTSTRAP_PATH} describe different "
        f"builds -- source_digest {theirs!r} against {ours!r} -- so the schema-3 generation is "
        "not a successor to this schema 2, it is a bystander from another one. Publication is "
        "schema 2 and then schema 3, so a build that died between them leaves exactly this: a "
        "complete, self-consistent schema 3 that is a build behind, and reclaiming on its "
        "strength would take the newer generation and keep the older. The reader refuses the "
        "same mismatch when a transcript search makes it look at both. Re-run the build; it "
        "republishes schema 3 against this source and the two agree again"
    )


def _website_refusal(root: Path) -> str | None:
    """Whether this archive's own browser bundle has anywhere to go once schema 2 is reclaimed.

    A subtle asymmetry, and the one thing about `gc` that a tool release cannot fix on its own.
    Every other precondition here is about the *tool*: flip a constant, ship a reader, and every
    archive in the world is ready at once. The website is not shipped, it is **copied** -- a build
    writes ``app.js`` into the archive it builds, and that copy then stays exactly as it was until
    another build overwrites it. `gc` does not rewrite it and should not: rewriting published
    assets is a build's job, and a collector that started editing JavaScript would be doing
    something nobody could audit from a receipt.

    So an archive built before the flip carries a bundle with no schema-3 mode -- it loads
    ``data/timeline-v2.json`` and falls back to ``data/timeline.json`` -- and this pass offers
    both of those. Sweeping without rebuilding, which is the whole convenience `gc` sells, would
    leave the operator an ``index.html`` that fails its first fetch, falls through to a file that
    is also in the trash, and shows an error. Silently, hours later, in somebody else's session.

    The test is the *positive* one: does this archive's ``app.js`` name the schema-3 bootstrap.
    Asking the negative -- "does it still mention ``data/timeline-v2.json``" -- would be wrong
    rather than merely fragile, because a post-flip bundle still fetches that file: it is the
    transcript search catalogue, which the flip does not retire. Presence of the schema-3 URL is
    the thing that actually distinguishes a bundle that can read this archive without schema 2's
    presentation objects from one that cannot.

    An archive with no ``app.js`` gets no refusal. There is no graphical surface there to protect,
    and inventing one would hold bytes on behalf of a reader that does not exist -- the failure
    mode :func:`_schema_1_category` describes for the fallback condition it lets lapse.
    """

    path = root / _WEBSITE_APP
    if path.is_symlink() or not path.is_file():
        return None
    try:
        source = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    if SCHEMA_3_BOOTSTRAP_PATH in source:
        return None
    return (
        f"held: this archive's own {_WEBSITE_APP} has no schema-3 mode -- it does not mention "
        f"{SCHEMA_3_BOOTSTRAP_PATH} -- so it still loads {SCHEMA_2_BOOTSTRAP_PATH} as its "
        f"timeline and falls back to {_SCHEMA_1_TIMELINE}, and this pass offers both. A build "
        f"copies {_WEBSITE_APP} into the archive and `gc` does not rewrite it, so a newer tool "
        "is not enough on its own: re-run the build against this archive, which costs no tokens "
        "and republishes the bundle, and the generation becomes collectable"
    )


def _schema_2_generation_files(root: Path) -> tuple[GcFile, ...]:
    """Every file the schema-2 generation owns: the bootstrap, its gzip twin, and its whole tree.

    The tree is taken wholesale rather than filtered to object-shaped names, which departs from
    :func:`_schema_2_categories`' orphan rule deliberately. ``data/timeline-v2/`` belongs to one
    format generation and to nothing else -- no other writer puts anything there and no reader
    looks there for anything else -- so once that generation is retired an unrecognised file
    inside it is residue of the same dead format, and skipping it would leave the directory
    standing for no reason anybody could later reconstruct. This set is only ever reached with
    the retirement preconditions already satisfied, and reclaiming is a rename into
    ``.wrkviz-trash/``, so the price of being wrong about a stray is one copy back.

    "Wholesale" is about *recognising* names, not about fate: :func:`_schema_2_categories`
    subtracts the transcript search corpus from whatever this returns, in both worlds, because
    those objects live in this tree without belonging to the format that is being retired.
    """

    return _files_with_sizes(
        root,
        (
            SCHEMA_2_BOOTSTRAP_PATH,
            SCHEMA_2_BOOTSTRAP_PATH + ".gz",
            *(relative for relative, _size in _walk_files(root, SCHEMA_2_ROOT)),
        ),
    )


def _schema_2_retirement_refusal(root: Path) -> str | None:
    """Why the schema-2 generation may not go yet, or ``None`` when it may.

    Four preconditions, and they answer four different questions. **Is there anything left to
    read the archive with**, which is the same question the schema-1 monolith asks and is answered
    the same way -- by `query.schema_3_completeness`, the reader's own five-clause acceptance
    rule, called rather than restated so that `gc` can never say yes where the reader says no.
    **Will anything write this generation again**, which the monolith never had to ask, because
    when the monolith was retired the tool had already stopped emitting it and schema 2 was there
    to catch both readers. **Is that schema 3 a successor to this schema 2 or a bystander from
    another build**, which completeness structurally cannot see -- :func:`_cross_generation_refusal`.
    And **does the browser in this archive know about schema 3** -- :func:`_website_refusal`,
    the one precondition a tool release cannot satisfy on an operator's behalf.

    They are checked in that order because that is the order in which they stop being worth
    printing: an operator whose tool still writes schema 2 does not need to hear about their
    ``app.js``, and one whose schema 3 is a shard short does not need to hear about digests.

    The second question is answered from the *code* -- :data:`timeline_shards.SCHEMA_2_IS_PUBLISHED`
    via :func:`timeline_shards.schema_2_is_published` -- and never from the archive, because every
    archive-side signal for it is either wrong or circular:

    * **Modification times lie, and this tool is why.** Schema-2 objects are content addressed and
      the bootstrap goes through `write_text_if_changed`, so a build that republishes an identical
      schema 2 writes no byte and moves no mtime. A measured archive showed its newest schema-2
      object thirteen hours older than its schema-3 bootstrap while the most recent build's
      ``data/export.json`` named all 661 schema-2 files as its own ``generated_files`` and both
      bootstraps carried the same ``source_digest`` and ``generated_at``. An mtime rule would have
      released 1.4 GB that the next build would have written straight back -- and worse, it would
      have been the "the last build did not write it" inference this whole module refuses.
    * **The export manifest is the opposite mistake.** It does name the generation, exactly, and
      that is why it cannot be the gate: the manifest of an archive built before the retirement
      names schema 2 forever, so requiring its silence would hold the bytes until a rebuild that
      the operator is trying to avoid, and requiring its *presence* to hold would be the mtime
      rule with more steps. It is corroboration for a human -- the measured archive above -- not a
      precondition.

    So the writer declares it, once, in the place a future edit has to go through anyway, and
    :func:`timeline_shards.write_timeline_shards` refuses to run while the declaration says
    retired. That is the property mtimes cannot have: not "no build did", but "no build can".
    """

    if schema_2_is_published():
        return (
            f"held: this build still writes schema 2 -- {_SCHEMA_2_CAPABILITY} is True, and "
            "write_timeline_shards refuses to run while it is False -- so every byte reclaimed "
            "here would be written again by the next build, which is churn rather than "
            "collection. The website used to be why that constant was True, and is not any "
            f"more: static/app.js reads {SCHEMA_3_BOOTSTRAP_PATH} and the schema-3 transcript "
            "search streams, and keeps schema 2 only as a fallback for an archive an older tool "
            "built. So a tree reaching this line is one that has turned the constant back on -- "
            "a released build sets it False -- and the reason to look for is in that tree's own "
            "timeline_shards.py, not here. Nothing an operator can do to this archive changes "
            "this line; a release of the tool does"
        )
    complete, declined = schema_3_completeness(root)
    if not complete:
        return f"held: no complete schema-3 generation supersedes it ({declined})"
    mismatch = _cross_generation_refusal(root)
    if mismatch is not None:
        return mismatch
    return _website_refusal(root)


def _holds_raw_turns(root: Path) -> bool:
    """Whether the material a rebuild reads is in *this* directory.

    The distinction is ingest archive against combined export, and it is not cosmetic: both carry
    ``.agent-team-timeline.json`` and both are things `gc` runs on, but only the first can be
    rebuilt from itself. Asked as "is there a ``<store>/<slug>/raw/``" rather than by looking for
    a marker that says which kind of archive this is, because no such marker exists and inventing
    one to answer a sentence in a reason string would be the tail wagging the dog -- the question
    the sentence is actually making a promise about is literally "are the raw turns reachable".

    *Reachable*, not *inside*. `build_store` moved ``raw/`` to a sibling of the archive, and an
    ingest archive that has migrated can still be rebuilt without a network or a token: the
    material is beside the archive rather than within it. Asking only about ``teams/`` would call
    such an archive a combined export and tell its operator the way back is on another machine,
    at the exact moment ``--empty-trash`` is about to make the question permanent.
    """

    for store in candidate_store_roots(root):
        if not store.is_dir() or store.is_symlink():
            continue
        for team in store.iterdir():
            raw = team / "raw"
            if raw.is_dir() and not raw.is_symlink():
                return True
    return False


def _schema_2_reclaim_reason(root: Path) -> str:
    """The reclaim reason, which is mostly a warning, and is a warning on purpose.

    This category removes the **last fallback**. The reader tries schema 3, then schema 2, then
    ``data/timeline.json``; the monolith is already retired as output and is offered for
    collection by :func:`_schema_1_category` under conditions that are satisfied whenever these
    are, so one ``--delete`` can take the whole chain below schema 3 in a single pass. After that
    a schema-3 generation the reader *declines* is not a slow archive, it is an unreadable one --
    and clause 5 declines a whole generation over one shard-shaped file under
    ``data/timeline-v3/`` that the catalogue does not name, which is the exact residue an
    interrupted build leaves.

    That is recoverable, and **how** recoverable depends on which archive this is, which is why
    the sentence is computed rather than written. An ingest archive holds ``teams/*/raw`` and a
    rebuild there costs no tokens and reads material that is right here. A **combined export**
    does not: `multi_team.build_combined_archive` writes ``teams/<slug>/summaries/`` and no
    ``raw/`` at all, while carrying the archive marker, so `gc --output <export>` runs on it
    perfectly happily -- and an export is the archive most likely to be collected, being the one
    an operator keeps and serves. Promising that operator a local rebuild would be false at the
    exact moment they are deciding something that ``--empty-trash`` makes permanent, so the reason
    says instead where the raw turns actually are: in the ingest archive this was built from,
    which may be on another machine, or may itself have been collected.

    The alternative considered was a doc note plus a shorter reason; it was rejected because the
    dry run is the only artefact an operator is guaranteed to read before typing ``--delete``.
    """

    monolith_gone = _size_of(root, _SCHEMA_1_TIMELINE) is None
    chain = (
        f"{_SCHEMA_1_TIMELINE} is already gone, so schema 3 becomes the only generation this "
        "archive has"
        if monolith_gone
        else f"{_SCHEMA_1_TIMELINE} is still on disk, but the schema-1-monolith category above "
        "offers it under conditions these already satisfy, so one --delete takes both and "
        "schema 3 becomes the only generation this archive has"
    )
    rebuild = (
        "the way back is a rebuild, which costs no tokens and reads the raw turns already "
        "reachable from here -- in <output>.build/*/raw, or teams/*/raw on an archive that has "
        "not migrated"
        if _holds_raw_turns(root)
        else "the way back is a rebuild, and it cannot be run here: this archive has no raw "
        "turns in either place, which is what a combined export looks like -- they are in the "
        "ingest archive it was built from, wherever that is"
    )
    return (
        f"superseded by a complete {SCHEMA_3_BOOTSTRAP_PATH} beside {SCHEMA_2_BOOTSTRAP_PATH}; "
        f"no build writes it any more ({_SCHEMA_2_CAPABILITY} is False, and "
        "write_timeline_shards refuses to run while it is). Whether this includes the transcript "
        "search corpus that shares the tree depends on whether the schema 3 here publishes one; "
        "schema-2-search-corpus says which, and subtracts itself from this category when it does "
        "not. WARNING, read before --delete: "
        f"this removes the last fallback. {chain}. A schema-3 generation the reader declines then "
        "leaves the archive unreadable rather than slow, and it declines the whole generation "
        f"over a single shard-shaped file under {SCHEMA_3_ROOT}/ that the catalogue does not "
        f"name -- exactly what an interrupted build leaves behind. Then {rebuild}; the way back "
        f"from a mistake in the next ten minutes is {TRASH_ROOT}/, until `gc --empty-trash`"
    )


def _schema_3_search_corpus(root: Path) -> tuple[int, int] | None:
    """``(shards, teams)`` of this archive's schema-3 transcript search corpus, or ``None``.

    ``None`` means there is no corpus here to supersede the schema-2 one -- no schema-3 bootstrap,
    an unparsable one, or one written before the ``search`` streams existed. The distinction that
    matters is not "can this tool write a corpus" but "does *this archive* hold one", the same
    per-archive question :func:`_website_refusal` asks about ``app.js`` and for the same reason: a
    tool upgrade does not reach into an archive somebody built last month, and the bytes being
    offered are that archive's.

    Read out of the catalogue rather than by walking ``data/timeline-v3/search/``, because the
    catalogue is what the reader acts on: `query._SchemaThreeArchive` will not open a shard the
    bootstrap does not name, so a shard on disk and absent from the bootstrap is not a corpus, it
    is residue. The stricter half of the question -- is the whole generation complete, is it the
    *same build* as this schema 2 -- is already asked, unchanged, by
    :func:`_schema_2_retirement_refusal` and :func:`_cross_generation_refusal` before any of these
    bytes can move. This function only has to answer whether the successor exists.
    """

    bootstrap = _bootstrap(root, SCHEMA_3_BOOTSTRAP_PATH, 3, "timeline-v3-bootstrap")
    if bootstrap is None:
        return None
    try:
        streams = as_object(bootstrap.get("streams"), "timeline-v3.streams")
        raw_search = streams.get("search")
        if raw_search is None:
            return None
        section = as_object(raw_search, "timeline-v3.streams.search")
        shards = as_array(section.get("shards"), "timeline-v3.streams.search.shards")
        teams = {
            as_object(raw, f"timeline-v3.streams.search.shards[{index}]").get("team")
            for index, raw in enumerate(shards)
        }
    except ValueError:
        return None
    if not shards:
        return None
    return len(shards), len(teams)


def _schema_2_search_corpus_category(
    root: Path, named: frozenset[str] | None, unknowable: str, successor: tuple[int, int] | None
) -> GcCategory:
    """The half of the schema-2 generation whose successor arrived last, held until it did.

    Everything else `gc` offers is a *copy* of something a newer artefact also has: the monolith
    against schema 2, the schema-2 presentation objects against schema 3. For most of this
    module's life the transcript search corpus was not, and this category existed to say so: it
    had exactly one implementation -- content-addressed day shards with trigram blooms, catalogued
    in ``search`` in the schema-2 bootstrap -- and the reader reached into it from *under schema
    3*. Reclaiming it would not have slowed anything down, it would have deleted `timeline search`
    and `timeline show` from an archive whose schema 3 was in perfect order, and the pass would
    have said "superseded" while doing it.

    Schema 3 now publishes a corpus, so the condition this category always implied can finally be
    written down, and it is written as a question about **this archive** rather than about this
    tool: does the schema-3 bootstrap here name ``search`` shards. When it does, these bytes are a
    duplicate like every other byte in the generation, they stop being subtracted from
    ``superseded-schema-2``, and they are held or offered by whatever that category decides --
    which today still holds them, because the format is still published for the website. When it
    does not, the old reason stands unchanged and unconditionally.

    Deciding it per archive rather than by a constant in the writer is the whole point. The two
    facts are genuinely independent: a build can carry the schema-3 corpus while an archive on
    disk predates it, which is the state every existing archive is in until it is rebuilt once, and
    an archive in that state must keep its only copy no matter what version of the tool is asking.
    The rejected alternative -- gate on `timeline_v3` being *able* to write a corpus, the way the
    format's retirement gates on :data:`timeline_shards.SCHEMA_2_IS_PUBLISHED` -- is exactly the
    mistake :func:`_website_refusal` was written to avoid, and it would offer 500.5 MiB of the only
    copy an unrebuilt archive has.

    The other rejected alternative, from before there was a successor at all, was to gate the whole
    generation on the corpus and refuse all 1.4 GB until search was ported. It was rejected because
    two thirds of those bytes genuinely *were* superseded, and holding a superseded copy on account
    of an unrelated tenant of the same directory is the mirror image of the mistake this exists to
    prevent.
    """

    if named is None:
        return GcCategory("schema-2-search-corpus", False, unknowable, ())
    if not named:
        return GcCategory(
            "schema-2-search-corpus",
            False,
            f"nothing to hold: {SCHEMA_2_BOOTSTRAP_PATH} names no transcript search corpus, so "
            "there is nothing here that schema 3 has not superseded",
            (),
        )
    if successor is not None:
        shards, teams = successor
        return GcCategory(
            "schema-2-search-corpus",
            False,
            f"nothing to hold: {SCHEMA_3_BOOTSTRAP_PATH} publishes a transcript search corpus of "
            f"its own -- {shards} search shard(s) across {teams} team(s), with the trigram "
            "prefilter and the prompt/response sidecar beside them -- and "
            "query.TimelineQuery._iter_search_records reads it, opening "
            f"{SCHEMA_2_BOOTSTRAP_PATH} only for an archive that has no such streams. These "
            "objects are therefore a duplicate like the rest of the generation and are accounted "
            "for by superseded-schema-2, under whatever condition that category is subject to",
            (),
        )
    files = _files_with_sizes(root, named)
    objects = sum(
        1 for item in files if item.relative_path.startswith(_SCHEMA_2_OBJECT_ROOT + "/")
    )
    return GcCategory(
        "schema-2-search-corpus",
        False,
        f"held: not superseded in this archive. {SCHEMA_2_BOOTSTRAP_PATH} and the "
        f"{objects} content-addressed day shards and linkage sidecars its `search` section "
        f"names under {_SCHEMA_2_OBJECT_ROOT}/ are the transcript search corpus, and the "
        f"{SCHEMA_3_BOOTSTRAP_PATH} here publishes no `search` stream to replace them -- so "
        "query.TimelineQuery._iter_search_records reads this generation even when schema 3 "
        "answers everything else, which is what `timeline search` and `timeline show` run on. "
        "Reclaiming these would remove a capability rather than a duplicate. A rebuild against "
        "this archive publishes the schema-3 corpus and costs no tokens; then they are offered "
        "with the rest of the generation",
        files,
    )


def _schema_2_categories(
    root: Path,
) -> tuple[GcCategory, GcCategory, GcCategory, GcCategory]:
    """The whole schema-2 generation, split four ways that never overlap.

    ``(superseded, search, orphans, retained)``. The four are computed together, and *partition*
    the generation -- every schema-2 file appears in exactly one of them -- because they are the
    only categories in this module that describe the same bytes from two angles, and a file listed
    twice would be counted twice in ``held_bytes`` and, far worse, moved twice by a sweep: the
    second `os.replace` would fail on a file that is no longer there and abort the pass with a
    traceback. So the partition is structural rather than remembered. ``superseded`` is the
    generation *minus* whatever the other three claim.

    The search corpus is subtracted **in both worlds when it is subtracted at all**, and it is the
    only one of the three that is. Reachability and the grace period are properties of a live
    format and stop meaning anything once it is retired; being unsuperseded is not, and an archive
    whose schema 3 carries no ``search`` stream is in that state whether or not the format is
    retired. Taking it out of ``superseded`` while the generation is live changes no byte's fate --
    everything there is held regardless -- and it makes the report say the true thing early: of the
    1,434.2 MiB in this tree on the measured archive, 500.5 MiB was a capability with no successor
    and 933.7 MiB a copy of something schema 3 already had.

    Once an archive is rebuilt with the schema-3 search streams the subtraction stops, and it stops
    in both worlds too, for the symmetric reason: the corpus is then a duplicate, and a duplicate
    reported in a category whose name says "not superseded" would be the report lying in the
    direction that costs an operator disk rather than data.

    Which way the rest of the split falls depends on :func:`_schema_2_retirement_refusal`.

    **While the generation is live**, reachability is the only thing that can retire a file
    inside it, and the existing two categories do that job unchanged. The manifest's accounting
    is exact today -- on the measured archive 353 current plus 305 retained is 658, which is
    every one of the 334 ``.json`` and 324 ``.gz`` files on disk, with no orphan and nothing
    missing, and all 283 search objects are inside ``current_objects``, so the corpus never
    overlaps either of them -- and the orphan set is *derived* as ``on disk - current -
    retained``, so a non-empty one is a real finding about a crashed build rather than an artefact
    of `gc` counting differently. ``superseded`` then holds the remainder -- the manifest and the
    70 current objects that are not the corpus -- and holds it with the reason it cannot go. That
    remainder used to be reported by nobody: an operator asking where 1.4 GB went was shown 305
    retained objects and left to discover the rest themselves. A category that exists to explain a
    refusal has to name the bytes it is refusing.

    **Once it is retired**, the reachability split stops meaning anything and the presentation
    half moves into one category. ``retained`` in particular has no residual claim: its grace
    period exists so a browser holding the previous bootstrap can still fetch what that bootstrap
    names, and the precondition for retirement is that no browser reads this format at all. Both
    are reported as empty with a reason pointing at the category that took them, rather than
    dropped, because this module's rule is that a category keeps its reason even when it has no
    files.
    """

    on_disk = {
        relative: size
        for relative, size in _walk_files(root, _SCHEMA_2_OBJECT_ROOT)
        if _OBJECT_NAME.fullmatch(PurePosixPath(relative).name) is not None
    }
    generation = _schema_2_generation_files(root)
    search_paths = _schema_2_search_paths(root)
    unknowable = (
        f"held: {SCHEMA_2_BOOTSTRAP_PATH} is present and cannot be parsed, so the objects the "
        "transcript search corpus needs cannot be told apart from the presentation objects "
        "schema 3 supersedes -- they share one content-addressed directory and only the "
        "catalogue distinguishes them. Nothing in this generation is offered until it reads"
    )
    refusal = _schema_2_retirement_refusal(root)
    if refusal is None and search_paths is None:
        refusal = unknowable
    search = _schema_2_search_corpus_category(
        root, search_paths, unknowable, _schema_3_search_corpus(root)
    )
    held_by_search = {item.relative_path for item in search.files}
    generation = tuple(
        item for item in generation if item.relative_path not in held_by_search
    )
    on_disk = {
        relative: size
        for relative, size in on_disk.items()
        if relative not in held_by_search
    }
    if refusal is None and not generation:
        # An archive built after the retirement has no schema 2 to retire, and one already swept
        # has nothing but the corpus left. Saying so beats printing the reclaim reason -- which is
        # mostly a warning about losing the last fallback -- against zero files, where it would
        # frighten an operator about a decision they are not being offered.
        refusal = (
            "nothing to collect: the presentation half of this generation is gone and what "
            f"remains under {SCHEMA_2_ROOT}/ is the transcript search corpus, held above"
            if held_by_search
            else f"nothing to collect: this archive has no {SCHEMA_2_BOOTSTRAP_PATH} and no "
            f"{SCHEMA_2_ROOT}/ tree"
        )
    if refusal is None:
        subsumed = (
            "subsumed by superseded-schema-2: the presentation half of the generation is being "
            "reclaimed in this pass, so which of its objects the manifest still names decides "
            "nothing"
        )
        return (
            GcCategory(
                "superseded-schema-2", True, _schema_2_reclaim_reason(root), generation
            ),
            search,
            GcCategory("schema-2-orphan-objects", False, subsumed, ()),
            GcCategory(
                "schema-2-retained-objects",
                False,
                "subsumed by superseded-schema-2: the grace period keeps these fetchable for a "
                "browser that already loaded the previous bootstrap, and no browser reads this "
                "format any more -- which is the precondition the whole generation went on",
                (),
            ),
        )
    sets = _schema_2_manifest_sets(root)
    if sets is None:
        orphans = GcCategory(
            "schema-2-orphan-objects",
            False,
            f"held: {_SCHEMA_2_MANIFEST} is absent or unrecognised, so nothing on disk can "
            "be shown to be unreachable",
            (),
        )
        retained_category = GcCategory(
            "schema-2-retained-objects",
            False,
            f"held: {_SCHEMA_2_MANIFEST} is absent or unrecognised",
            (),
        )
    else:
        current, retained = sets
        orphans = GcCategory(
            "schema-2-orphan-objects",
            True,
            f"named by neither current_objects nor retained_objects in {_SCHEMA_2_MANIFEST}",
            tuple(
                GcFile(relative, on_disk[relative])
                for relative in sorted(set(on_disk) - current - retained)
            ),
        )
        retained_category = GcCategory(
            "schema-2-retained-objects",
            False,
            "held: the previous generation's objects, kept alive one more generation so a "
            "browser that already loaded the previous bootstrap can still fetch what it names; "
            "the next build that supersedes them reclaims them",
            tuple(
                GcFile(relative, on_disk[relative])
                for relative in sorted(retained)
                if relative in on_disk
            ),
        )
    claimed = {item.relative_path for item in orphans.files} | {
        item.relative_path for item in retained_category.files
    }
    return (
        GcCategory(
            "superseded-schema-2",
            False,
            refusal,
            tuple(item for item in generation if item.relative_path not in claimed),
        ),
        search,
        orphans,
        retained_category,
    )


def _schema_3_named_paths(root: Path) -> frozenset[str] | None:
    """Every path the schema-3 bootstrap names, or ``None`` if it cannot be read.

    Deliberately parsed here rather than obtained from `query._SchemaThreeArchive`: the reader
    keeps the catalogue for the questions it answers and exposes it as shard *entries*, while the
    only question here is the flat set of names, and reaching into a private structure to
    reconstruct it would couple a sweep to a reader's field layout.
    """

    bootstrap = _bootstrap(root, SCHEMA_3_BOOTSTRAP_PATH, 3, "timeline-v3-bootstrap")
    if bootstrap is None:
        return None
    try:
        named: set[str] = {SCHEMA_3_BOOTSTRAP_PATH}
        streams = as_object(bootstrap.get("streams"), "timeline-v3.streams")
        for stream, raw_section in sorted(streams.items()):
            where = f"timeline-v3.streams.{stream}"
            section = as_object(raw_section, where)
            for index, raw in enumerate(as_array(section.get("shards"), where + ".shards")):
                entry = as_object(raw, f"{where}.shards[{index}]")
                named.add(as_string(entry.get("path"), f"{where}.shards[{index}].path"))
                named.add(
                    as_string(
                        entry.get("index_path"), f"{where}.shards[{index}].index_path"
                    )
                )
    except (OSError, ValueError):
        return None
    return frozenset(named)


def _schema_3_category(root: Path) -> GcCategory:
    """Shards under the schema-3 root that the current bootstrap does not name. Never swept.

    This category was written to be reclaimable, on the reasoning that schema 3 needs no
    reachability manifest because the bootstrap *is* the reachable set, so a file under
    ``data/timeline-v3/`` and absent from the catalogue must have been reachable from a
    generation that no longer exists. **That reasoning is wrong, and the counterexample costs a
    team.** Publication is shards, then the bootstrap, with no atomicity across the set. A build
    that adds a team and dies after writing its shards and before writing the bootstrap leaves
    the *previous* bootstrap in place -- complete, self-consistent, naming files that are all
    present at their declared lengths -- with the new team's shards beside it, named by nobody.
    Absence from the catalogue therefore means "retired" if the bootstrap is newer than the
    shard and "live, and the newest thing here" if it is older, and nothing on disk says which.
    A rule that guessed would eventually sweep the better half of a rebuild, and would in the
    same pass declare the schema-1 monolith superseded and reclaim the archive's fallback.

    Two changes retire the question rather than answer it. The reader now checks the converse of
    its presence rule -- clause 5 of :class:`query._SchemaThreeArchive`, "nothing on disk is
    unnamed" -- so the interrupted generation above is declined rather than read as complete,
    which is what already keeps this category, and the monolith beside it, held. And the
    publisher clears its own root of everything outside its plan once its bootstrap is on disk,
    so a completed build always leaves the tree equal to its catalogue. Between them, a non-empty
    orphan set has exactly one meaning left: a build was interrupted. That is a finding to report
    and a build to re-run, not garbage to collect -- and `gc` is not the tool that should be
    deciding it, because it is the one tool here with no way to tell the two cases apart.

    So the files are still listed, at their full size, with the reason saying what they are.
    Reporting a thing and reclaiming it are different jobs, and this module already separates
    them for the largest thing in the archive.
    """

    named = _schema_3_named_paths(root)
    on_disk = tuple(_walk_files(root, SCHEMA_3_ROOT))
    if named is None:
        return GcCategory(
            "schema-3-orphan-shards",
            False,
            f"held: {SCHEMA_3_BOOTSTRAP_PATH} is absent or unreadable, so no shard can be shown "
            "to be unreachable",
            (),
        )
    orphans = tuple(
        GcFile(relative, size) for relative, size in on_disk if relative not in named
    )
    if not orphans:
        complete, declined = schema_3_completeness(root)
        return GcCategory(
            "schema-3-orphan-shards",
            False,
            f"nothing under {SCHEMA_3_ROOT}/ is unaccounted for; the tree matches "
            f"{SCHEMA_3_BOOTSTRAP_PATH}"
            + ("" if complete else f", though the generation is incomplete ({declined})"),
            (),
        )
    return GcCategory(
        "schema-3-orphan-shards",
        False,
        f"held: under {SCHEMA_3_ROOT}/ and named by no entry in {SCHEMA_3_BOOTSTRAP_PATH}. A "
        "completed build leaves this set empty -- it removes its own strays -- so these are the "
        "residue of a build that was interrupted before it published its bootstrap, and there "
        "is no way from here to tell a retired team's shard from a live team's. Until they are "
        "gone the reader declines schema 3 entirely and this archive is being read through the "
        "slower generation behind it. Re-run the build; it will clear them and this category "
        "goes back to empty",
        orphans,
    )


def _source_snapshot_category(root: Path) -> GcCategory:
    """The ingest inputs still inside the archive, measured and left alone.

    This is the largest single thing in the archive and it is reported for that reason alone: the
    question `gc` exists to answer is "what is in the output that is not output", and an answer
    that omitted 72% of the bytes because they are out of scope would be a technically true
    answer to a different question.

    On an archive whose snapshots have been relocated by `migrate-snapshots` this category is
    empty, and that is the correct report rather than a blind spot: `gc` classifies the files in
    the directory it was given, and after a migration there are none of these there. The store
    outside the archive is deliberately not walked -- reaching outside `--output` to enumerate,
    let alone reclaim, is exactly the authority a garbage collector should not have.
    """

    teams = root / "teams"
    relatives: list[str] = []
    if teams.is_dir() and not teams.is_symlink():
        for team in sorted(teams.iterdir()):
            if not team.is_dir() or team.is_symlink():
                continue
            relatives.append(f"teams/{team.name}/{_SOURCE_SNAPSHOT_DIRECTORY}")
    found: list[GcFile] = []
    for relative in relatives:
        found.extend(GcFile(path, size) for path, size in _walk_files(root, relative))
    return GcCategory(
        "ingest-source-snapshots",
        False,
        "held: vendor input rather than published output, and its deletion has a purpose-built "
        "gate -- run `audit-losslessness --require-lossless` and act on that, not on this. To "
        "get these bytes out of the published tree without deleting anything, run "
        "`migrate-snapshots`",
        tuple(found),
    )


def _trash_category(root: Path) -> GcCategory:
    # BOTH trash roots. The directory was renamed with the tool, and an archive swept before that
    # still holds its deleted files under the former name. Walking only the current one would
    # report those bytes as absent and leave `--empty-trash` unable to reach them -- a rename
    # that silently strands whatever an operator already deleted is the one outcome a reversible
    # delete exists to prevent.
    held = tuple(
        GcFile(path, size)
        for trash_root in (TRASH_ROOT, LEGACY_TRASH_ROOT)
        for path, size in _walk_files(root, trash_root)
    )
    return GcCategory(
        "trash",
        False,
        f"held: already swept and reversible; `gc --empty-trash` deletes {TRASH_ROOT}/ for good",
        held,
    )


def _empty_directories(root: Path, pending: frozenset[str]) -> tuple[str, ...]:
    """Directories under ``data/`` that hold nothing, innermost first.

    *pending* is the set of files this pass is about to reclaim, counted as already gone. That
    is what makes the dry run's number honest: the directory a retired team leaves behind still
    contains its shards at the moment the plan is computed, and a plan that only reported
    already-empty directories would report zero and then remove two.

    The known source is a retired team: stale-file removal unlinks a team's shards and details
    and never `rmdir`s what held them, so `data/timeline-v3/timeline/<team>/` and
    `data/details/<team>/` survive their contents. They cost no bytes, which is why they are
    reported as a count rather than a size and removed by `rmdir` rather than moved to the trash
    -- an empty directory is restored by `mkdir`, so a trash copy of one would be ceremony.

    Innermost first so that removing a leaf makes its parent collectable in the same pass without
    a second walk.
    """

    data = root / "data"
    if not data.is_dir() or data.is_symlink():
        return ()
    collected: set[str] = set()
    empty: list[str] = []
    for directory, subdirectories, names in os.walk(data, topdown=False, followlinks=False):
        path = Path(directory)
        if path == data or path.is_symlink():
            continue
        if any(
            Path(directory, name).relative_to(root).as_posix() not in pending
            for name in names
        ):
            continue
        if any(
            Path(directory, name).as_posix() not in collected for name in subdirectories
        ):
            continue
        collected.add(path.as_posix())
        empty.append(path.relative_to(root).as_posix())
    return tuple(empty)


def plan_collection(root: Path) -> GcReport:
    """Classify the archive without touching it. The caller holds the lock."""

    total_bytes = 0
    total_files = 0
    for _relative, size in _walk_files(root, ""):
        total_bytes += size
        total_files += 1
    superseded_schema_2, search_corpus, orphan_objects, retained_objects = (
        _schema_2_categories(root)
    )
    categories = (
        _schema_1_category(root),
        superseded_schema_2,
        search_corpus,
        orphan_objects,
        _schema_3_category(root),
        retained_objects,
        _source_snapshot_category(root),
        _trash_category(root),
    )
    pending = frozenset(
        item.relative_path
        for category in categories
        if category.reclaimable
        for item in category.files
    )
    return GcReport(
        root=str(root.resolve()),
        action="dry-run",
        total_bytes=total_bytes,
        total_files=total_files,
        categories=categories,
        trash_relative_path=TRASH_ROOT,
        trash_generation=None,
        moved=(),
        removed_directories=_empty_directories(root, pending),
        emptied_bytes=0,
    )


def _generation_directory(root: Path, now: datetime) -> str:
    """A fresh trash generation, named for the instant it was opened.

    The suffix loop exists because two sweeps in the same second are ordinary in a test and
    possible in a script, and a generation that silently merged with an earlier one would make
    its receipt describe files it did not move.
    """

    stamp = now.strftime(_GENERATION_STAMP)
    for attempt in range(1, 1000):
        candidate = stamp if attempt == 1 else f"{stamp}-{attempt}"
        relative = f"{TRASH_ROOT}/{candidate}"
        if not (root / relative).exists():
            return relative
    raise ArchiveGcError(f"cannot open a trash generation under {TRASH_ROOT}/")


def _move_to_trash(root: Path, generation: str, relative: str) -> None:
    source = _archive_path(root, relative)
    target = _archive_path(root, f"{generation}/{_TRASH_PAYLOAD}/{relative}")
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.replace(source, target)
    except OSError as error:
        if error.errno == errno.EXDEV:
            raise ArchiveGcError(
                f"cannot move {relative} into {TRASH_ROOT}/ because they are on different "
                "filesystems; the trash has to sit on the archive's own device for the first "
                "pass to be both fast and reversible"
            ) from error
        raise ArchiveGcError(f"cannot reclaim {relative}: {error}") from error


def _receipt(
    root: Path,
    generation: str,
    report: GcReport,
    files: Sequence[tuple[str, GcFile]],
    *,
    complete: bool,
) -> None:
    """Write the generation's receipt, before the sweep as intent and after it as record.

    Written **twice**, and the first write is the one that matters. A receipt produced only after
    every move succeeded is absent in precisely the case it was invented for: `os.replace` raises
    on the fifth of ten files -- EACCES under a read-only subtree, ENOSPC or EDQUOT creating the
    trash subdirectory, EPERM on a sticky directory -- and the operator is left with a traceback
    and a generation directory holding four files and no statement of what they are or how to put
    them back. The payload tree still mirrors the archive, so the restore would have worked; it
    just was not written down anywhere, which was the receipt's entire job.

    So the intent is recorded first, listing everything the sweep is about to move, and rewritten
    afterwards with what it actually moved. ``complete`` distinguishes the two, so a reader can
    tell "these files are in the trash" from "each of these is in the trash *or* still in the
    archive" -- and the restore command is correct either way, because copying the payload tree
    back over the archive is idempotent and a file that never moved simply is not in it.
    """

    entries: list[JsonValue] = [
        {
            "path": item.relative_path,
            "bytes": item.bytes,
            "category": category,
            "restore_to": item.relative_path,
        }
        for category, item in files
    ]
    reasons: list[JsonValue] = [
        {"category": item.name, "reason": item.reason}
        for item in report.categories
        if item.reclaimable
    ]
    receipt: dict[str, JsonValue] = {
        "schema_version": 1,
        "kind": "archive-gc-trash-receipt",
        "generation": generation,
        "archive": report.root,
        "complete": complete,
        "reasons": reasons,
        "files": entries,
        "restore": (
            f"from the archive root, run: cp -a {generation}/{_TRASH_PAYLOAD}/. ./ -- the "
            "payload tree mirrors the archive, so every file lands back where it was taken from"
        ),
    }
    if not complete:
        receipt["note"] = (
            "written before the sweep started. Every file listed is either in this generation's "
            "payload tree or still at its original path in the archive; the restore command "
            "above is correct in both cases. If the sweep finished, this file was replaced by "
            "one with complete=true."
        )
    write_text_if_changed(
        _archive_path(root, f"{generation}/{_RECEIPT_NAME}"), canonical_json(receipt)
    )


def _remove_tree(path: Path) -> int:
    """Delete a directory tree that this module created, returning the bytes freed."""

    freed = 0
    for directory, subdirectories, names in os.walk(path, topdown=False, followlinks=False):
        for name in names:
            entry = Path(directory, name)
            try:
                freed += entry.lstat().st_size
            except OSError:
                pass
            entry.unlink()
        for name in subdirectories:
            candidate = Path(directory, name)
            if candidate.is_symlink():
                candidate.unlink()
            else:
                candidate.rmdir()
    path.rmdir()
    return freed


def collect(
    root: Path, *, delete: bool = False, empty_trash: bool = False
) -> GcReport:
    """Report what is dead, and -- only when asked -- move it or destroy the trash.

    ``delete`` and ``empty_trash`` are refused together rather than sequenced. Sweeping and then
    emptying in one command would reintroduce exactly the irreversibility the trash exists to
    remove, and an operator who wants both has lost nothing by typing two commands: the second
    one is the one they should have had to think about.
    """

    if delete and empty_trash:
        raise ArchiveGcError(
            "--delete and --empty-trash are separate passes on purpose: the first is "
            "reversible and the second is not, so no single invocation does both"
        )
    if not archive_marker_path(root).is_file():
        raise ArchiveGcError(f"not a wrkviz archive: {root}")
    with archive_writer_lock(root):
        report = plan_collection(root)
        if empty_trash:
            freed = 0
            for trash_name in (TRASH_ROOT, LEGACY_TRASH_ROOT):
                trash = root / trash_name
                if trash.is_dir() and not trash.is_symlink():
                    freed += _remove_tree(trash)
            return GcReport(
                root=report.root,
                action="emptied",
                total_bytes=report.total_bytes - freed,
                total_files=report.total_files - report.category("trash").count,
                categories=tuple(
                    item if item.name != "trash" else GcCategory(
                        item.name, False, "emptied", ()
                    )
                    for item in report.categories
                ),
                trash_relative_path=TRASH_ROOT,
                trash_generation=None,
                moved=(),
                removed_directories=(),
                emptied_bytes=freed,
            )
        if not delete:
            return report
        moved: list[tuple[str, GcFile]] = []
        planned: list[tuple[str, GcFile]] = [
            (category.name, item)
            for category in report.categories
            if category.reclaimable
            for item in category.files
        ]
        generation = _generation_directory(root, datetime.now(tz=timezone.utc))
        if planned:
            _receipt(root, generation, report, planned, complete=False)
        for category_name, item in planned:
            _move_to_trash(root, generation, item.relative_path)
            moved.append((category_name, item))
        removed: list[str] = []
        for relative in report.removed_directories:
            candidate = _archive_path(root, relative)
            try:
                candidate.rmdir()
            except OSError:
                continue
            removed.append(relative)
        swept = GcReport(
            root=report.root,
            action="swept",
            total_bytes=report.total_bytes,
            total_files=report.total_files,
            categories=tuple(
                GcCategory(item.name, item.reclaimable, item.reason, ())
                if item.reclaimable
                else item
                for item in report.categories
            ),
            trash_relative_path=TRASH_ROOT,
            trash_generation=generation,
            moved=tuple(item for _category, item in moved),
            removed_directories=tuple(removed),
            emptied_bytes=0,
        )
        if moved:
            _receipt(root, generation, report, moved, complete=True)
        return swept


def _mib(value: int) -> str:
    return f"{value / (1024 * 1024):,.1f} MiB"


def format_gc_report(report: GcReport, output_format: str) -> str:
    """Render a report for a human or for a machine.

    The text form leads with the archive total and the reclaimable fraction, because "how much
    of this 9.1 GB is dead" is the question, and a table of categories that never adds up to
    anything is how a report about size fails to be about size.
    """

    if output_format == "json":
        return canonical_json(report.to_json_obj(include_files=True)) + "\n"
    if output_format != "text":
        raise ArchiveGcError(f"unsupported gc report format {output_format!r}")
    lines = [
        f"archive: {report.root}",
        f"  {report.total_files:,} files, {_mib(report.total_bytes)}",
        "",
    ]
    for category in report.categories:
        state = "reclaimable" if category.reclaimable else "held"
        lines.append(
            f"{category.name}: {category.count:,} files, {_mib(category.bytes)} [{state}]"
        )
        lines.append(f"  {category.reason}")
    lines.append("")
    lines.append(
        f"reclaimable now: {report.reclaimable_files:,} files, "
        f"{_mib(report.reclaimable_bytes)}"
    )
    if report.action == "dry-run":
        if report.removed_directories:
            lines.append(
                f"empty directories to remove: {len(report.removed_directories):,}"
            )
        lines.append("")
        lines.append(
            "this was a dry run; nothing was moved or deleted. To reclaim, add --delete: "
            f"every file above is moved into {report.trash_relative_path}/<generation>/ with "
            "its archive path preserved, and a receipt beside it says how to put it back. "
            "Emptying that trash for good is a separate command, `gc --empty-trash`."
        )
    elif report.action == "swept":
        lines.append(
            f"moved {len(report.moved):,} file(s) into {report.trash_generation}/"
            f"{_TRASH_PAYLOAD}/"
        )
        if report.removed_directories:
            lines.append(f"removed {len(report.removed_directories):,} empty director(ies)")
        lines.append(
            f"to undo, from the archive root: "
            f"cp -a {report.trash_generation}/{_TRASH_PAYLOAD}/. ./"
        )
        lines.append("to make it permanent: gc --empty-trash")
    else:
        lines.append(f"emptied the trash: {_mib(report.emptied_bytes)} freed permanently")
    return "\n".join(lines) + "\n"


__all__ = [
    "ArchiveGcError",
    "GcCategory",
    "GcFile",
    "GcReport",
    "TRASH_ROOT",
    "collect",
    "format_gc_report",
    "plan_collection",
]
