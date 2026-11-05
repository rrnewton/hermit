"""Where the vendor source snapshots live, and how an existing archive moves them there.

Every number below was measured on 2026-08-24 against one archive, read-only.

The snapshots are the largest thing the tool produces and the only thing it produces that is not
output. On the measured archive they are **995 files and 6,524,876,923 bytes of a 9,662,206,589-byte
archive -- 67.5%** -- and until this module existed all of it sat at
``<output>/teams/<slug>/source_snapshots/``: inside the directory the operator publishes, inside
the directory ``serve``/``serve.py`` roots an HTTP server at, and inside the directory a consumer
clones. The tool already said it did not consider them part of the artifact -- ``pipeline``'s
generated ``.gitignore`` has excluded ``/teams/*/source_snapshots/`` since the tree existed -- so
the intent was settled and only the physical placement was left. This module moves it.

They cannot simply be deleted, which is why this is a relocation. Three separate things still read
them:

* **The append-only guards.** Codex and Claude compare the live rollout against the bytes already
  copied, and Orc's ``_prepare_snapshot_candidate`` opens the *previous* content-addressed SQLite
  object, verifies it against the digest in ``raw/source-manifest.json``, and recomputes logical
  state from it. Take the tree away and the next incremental ingest either refuses or silently
  loses its baseline.
* **The frozen task-note projections** under ``.projections/``. See "What is uniquely here" below;
  on the measured archive these are still the only copy of 38,094 notes.
* **The losslessness audit**, whose whole job is to account for every vendor row against the
  archive, and which is the gate any future *deletion* has to pass.

Where the default goes
----------------------

``<output>.sources/`` -- a sibling of the published directory, named after it, holding one
directory per team slug::

    /path/to/summary/widget            <- --output, published, served, cloned
    /path/to/summary/widget.sources/   <- the snapshot store
    /path/to/summary/widget.sources/<team-slug>/...

A sibling rather than a state directory under ``$XDG_STATE_HOME`` or ``~/.cache``, and the reason
is the migration rather than taste. A sibling is on the same filesystem as the archive in every
layout anyone actually has, so moving an existing tree there is ``os.replace`` per team: atomic,
instant, and impossible to half-finish inside one team. A state directory is on the same
filesystem only by luck, and for the measured archive the unlucky case is a 6.5 GB copy that can
be interrupted -- turning "relocate" into "duplicate, verify, then delete", which is a materially
more dangerous operation to hand an operator by default. The sibling also keeps the pairing legible
(``widget`` and ``widget.sources`` sort next to each other) and survives the operator moving the
enclosing directory, which a path recorded under ``$HOME`` does not.

It is *outside* ``--output``, which is the point, and that has one consequence worth stating
plainly: a command given ``--output X`` writes to a directory that is not under ``X``. That is
unusual enough to be worth announcing, so the first ingest that establishes a store says so on
stderr, and the location is recorded in the archive rather than merely derived, so it can be read
back rather than guessed.

The store ignores itself. ``<store>/.gitignore`` contains ``/*``, which ignores every entry
including the ``.gitignore`` -- so a store that lands inside somebody's repository is invisible to
``git status`` rather than being proposed as a 6.5 GB addition. The archive's own ``.gitignore``
cannot do that job any more, because the store is no longer inside the archive, and writing a
``.gitignore`` into the *parent* of ``--output`` would be the tool editing a directory it was never
given.

How a run finds the store
-------------------------

Resolution is deliberately boring, and every branch of it is a refusal or a documented default:

1. ``--snapshot-root`` on the command line, or ``snapshot_root`` in a project config.
2. otherwise ``<output>/snapshot-root.json``, written by the run that established the layout.
3. otherwise, if ``teams/<slug>/source_snapshots/`` is populated, *that* -- an archive built by an
   older tool keeps working, untouched, with no flag and no migration.
4. otherwise ``<output>.sources/``.

Nothing here ever moves a file. An archive in the old layout stays in the old layout until an
operator runs ``migrate-snapshots``, because a build that silently relocated 6.5 GB as a side
effect of being run for some other reason is precisely the behaviour a "safe and explicit
migration" requirement exists to forbid. The disagreements that could quietly split an archive's
snapshots across two trees, or point a run at the wrong one, are refusals instead:

* a requested root while **any** team in the archive still keeps its snapshots inside it. Asked of
  the archive rather than of the team being ingested, because "one archive, one layout" is a
  property of the archive: a new team given ``--snapshot-root`` beside eleven un-migrated ones
  used to succeed, record ``external`` in the archive-wide pointer, and leave the other eleven
  refusing on the next run.
* a pointer that says the store is external while *this team's* in-archive directory is still
  populated -- an interrupted migration, which reads as "finish it" rather than "start again", and
  which is asked per team on purpose so that the teams already moved keep ingesting while the rest
  wait.
* a recorded store that is not on disk, when snapshots have been copied for this archive before.
  See :func:`_recorded_store`; the one case that is *not* a refusal there is the archive and its
  store having been renamed together, which the sibling's own marker identifies.

What is uniquely here
---------------------

Checked against the measured archive rather than assumed, because the answer decided whether a
move is enough or a copy is required:

* ``.projections/<prefix>/<sha>.json`` -- the frozen task-note history. Its schema-2 records carry
  exactly ``note_id``, ``task_id``, ``content``, ``created_at``, ``server_author``, ``task_owner``
  and ``title``; :class:`wrkviz.model.TaskNote` carries all seven plus provenance, so
  the promotion into tracked ``raw/task-notes.jsonl`` is field-complete. **It has not run on the
  measured archive**: no Orc team there has a ``raw/task-notes.jsonl`` at all, so today those four
  projection files -- 70,572,760 bytes, 38,094 notes, 1,386 of them with no upstream row left --
  are still the only copy. Field-complete code that has not been executed protects nothing, which
  is the whole reason this is a move.
* ``.objects/<prefix>/<sha>.db`` and, in the pre-content-addressed layout, ``.orc/`` and ``.tg/``
  -- the previous-snapshot baseline the Orc append guard reads.
* the vendor Codex/Claude JSONL -- still the only copy of command stdout and patch bodies for any
  team ingested before ``teams/<slug>/payloads/`` existed. On the measured archive that is *every*
  team: it contains 0 payload files.

Every reference into the tree is relative to the snapshot root -- ``OrcSourceCopy.snapshot_path``,
``OrcTaskProjection.path``, ``CodexSourceCopy.source_path`` -- so relocating the root leaves all of
them valid. That is not luck; it is the property that makes a rename a complete migration.
"""

from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Literal

from wrkviz.build_store import candidate_store_roots
from wrkviz.archive import (
    archive_marker_path,
    JsonValue,
    as_object,
    as_string,
    narrow_json,
    read_json,
    validate_team_slug,
    write_json_durable,
    write_text_durable,
)


#: Written at the archive root, tracked, and tiny. It exists so that the layout is *recorded*
#: rather than inferred: a run that had to guess would guess the default, and an operator who
#: configured a store elsewhere and then forgot the flag would silently start a second one --
#: which is not an error the filesystem can report, because both trees are individually valid.
SNAPSHOT_POINTER_FILE = "snapshot-root.json"

#: What the directory was called when it lived inside the archive, and still is for an archive
#: nobody has migrated. Kept as the in-archive name rather than renamed on migration so that an
#: unmigrated archive, an in-flight migration, and the generated ``.gitignore`` all keep agreeing.
LEGACY_SNAPSHOT_DIRECTORY = "source_snapshots"

#: ``<output>.sources``. A suffix rather than a fixed sibling name because more than one archive
#: routinely shares a parent directory, and a store called ``sources`` beside two archives would
#: belong to neither.
DEFAULT_STORE_SUFFIX = ".sources"

#: The store's own metadata, dot-prefixed so it can never collide with a team slug -- the slug
#: grammar has no leading dot. The marker records which archive the store belongs to, which is the
#: only way to notice that a second archive has been pointed at somebody else's store before the
#: two collide on a shared slug.
STORE_MARKER_FILE = ".snapshot-store.json"
STORE_GITIGNORE_FILE = ".gitignore"

#: Where a cross-filesystem copy assembles a team before publishing it by rename. Dot-prefixed
#: for the same reason the marker is -- no team slug can collide with it -- and inside the store
#: rather than beside it so that the publishing step is a same-device ``os.replace``. See
#: :func:`_copy_across_devices` for why the copy does not simply write to its target.
_COPY_STAGING_DIRECTORY = ".migrating"

_POINTER_KIND = "source-snapshot-location"
_MARKER_KIND = "source-snapshot-store"
_SCHEMA_VERSION = 1

Layout = Literal["in-archive-legacy", "external"]

_STORE_GITIGNORE_TEXT = (
    "# Written by wrkviz. This directory is the archive's source-snapshot store:\n"
    "# vendor logs and SQLite snapshots that ingestion reads and that no published artifact\n"
    "# contains. `/*` ignores every entry including this file, so a store that lands inside a\n"
    "# repository is invisible to `git status` rather than being offered as a multi-gigabyte\n"
    "# addition. Delete this file if you genuinely want the bulk under version control.\n"
    "/*\n"
)


class SnapshotLocationError(ValueError):
    """A snapshot store that cannot be used without an operator deciding something."""


@dataclass(frozen=True)
class SnapshotLocation:
    """Where one team's snapshots are, and how that was decided."""

    archive: Path
    team_slug: str
    #: The directory that used to be ``teams/<slug>/source_snapshots``. Everything inside it is
    #: addressed relative to this path, so this is the only value the providers need.
    root: Path
    #: The store holding every team, or ``None`` in the in-archive layout, which has no such thing.
    store_root: Path | None
    layout: Layout
    #: How resolution arrived here, for the one stderr line that announces a new store and for the
    #: migration report. Not persisted: it describes this run, not the archive.
    origin: Literal["requested", "recorded", "in-archive-legacy", "default"]

    @property
    def inside_archive(self) -> bool:
        """Whether these bytes are inside the directory the operator publishes and serves."""

        return self.layout == "in-archive-legacy"

    @property
    def archive_relative(self) -> str:
        """The root as a POSIX path relative to the archive, for the source manifest.

        Relative, never absolute, for two reasons that point the same way. The source manifest is
        a *tracked, served* file, so an absolute path would publish somebody's home directory
        layout; and an archive that is copied together with its store keeps working only if what
        it recorded was the relationship rather than the location.
        """

        return PurePosixPath(os.path.relpath(self.root, self.archive)).as_posix()


def default_store_root(archive: Path) -> Path:
    """Return ``<archive>.sources``, the store a fresh archive gets when nobody says otherwise."""

    resolved = archive.resolve()
    return resolved.parent / (resolved.name + DEFAULT_STORE_SUFFIX)


def legacy_team_root(archive: Path, team_slug: str) -> Path:
    """Return the in-archive location an older tool wrote, whether or not it exists."""

    validate_team_slug(team_slug)
    return archive / "teams" / team_slug / LEGACY_SNAPSHOT_DIRECTORY


def legacy_team_roots(archive: Path) -> tuple[Path, ...]:
    """Return every populated in-archive snapshot directory, in slug order.

    Enumerated from the filesystem rather than from a manifest on purpose: the callers are the
    migration, the garbage collector and the audit, and all three are asking "what is physically
    still in the published tree", which is a question only the tree can answer.
    """

    teams = archive / "teams"
    if not teams.is_dir():
        return ()
    found: list[Path] = []
    for team in sorted(teams.iterdir()):
        if not team.is_dir() or team.is_symlink():
            continue
        candidate = team / LEGACY_SNAPSHOT_DIRECTORY
        if _populated(candidate):
            found.append(candidate)
    return tuple(found)


def _populated(path: Path) -> bool:
    """Whether *path* is a real directory with something in it.

    An empty directory reads as absent. It is what an interrupted removal, a fresh ``mkdir`` or a
    completed migration leaves behind, and treating it as "the snapshots are here" would make a
    migrated archive look half-migrated forever.
    """

    if path.is_symlink() or not path.is_dir():
        return False
    return any(path.iterdir())


@dataclass(frozen=True)
class _Pointer:
    layout: Layout
    #: Archive-relative, exactly as recorded; resolved by the caller against the archive so that
    #: moving the pair keeps working.
    root: str | None


def _pointer_path(archive: Path) -> Path:
    return archive / SNAPSHOT_POINTER_FILE


def read_pointer(archive: Path) -> _Pointer | None:
    """Read the recorded layout, or ``None`` when this archive has never recorded one."""

    path = _pointer_path(archive)
    if path.is_symlink():
        raise SnapshotLocationError(f"{path}: snapshot location pointer is a symlink")
    if not path.is_file():
        return None
    obj = as_object(read_json(path), str(path))
    kind = as_string(obj.get("kind"), f"{path}: kind")
    if kind != _POINTER_KIND or obj.get("schema_version") != _SCHEMA_VERSION:
        raise SnapshotLocationError(f"{path}: not a schema-1 {_POINTER_KIND} document")
    layout = as_string(obj.get("layout"), f"{path}: layout")
    if layout == "in-archive-legacy":
        if set(obj) != {"schema_version", "kind", "layout"}:
            raise SnapshotLocationError(f"{path}: unexpected fields for an in-archive layout")
        return _Pointer("in-archive-legacy", None)
    if layout != "external":
        raise SnapshotLocationError(f"{path}: unknown layout {layout!r}")
    if set(obj) != {"schema_version", "kind", "layout", "root"}:
        raise SnapshotLocationError(f"{path}: unexpected fields for an external layout")
    root = as_string(obj.get("root"), f"{path}: root")
    if not root or PurePosixPath(root).is_absolute():
        raise SnapshotLocationError(
            f"{path}: root must be a non-empty path relative to the archive, so that an archive "
            "moved together with its store keeps working"
        )
    return _Pointer("external", root)


def write_pointer(archive: Path, location: SnapshotLocation) -> bool:
    """Record the layout durably; return whether anything changed."""

    value: dict[str, JsonValue] = {
        "schema_version": _SCHEMA_VERSION,
        "kind": _POINTER_KIND,
        "layout": location.layout,
    }
    if location.store_root is not None:
        value["root"] = PurePosixPath(
            os.path.relpath(location.store_root, archive.resolve())
        ).as_posix()
    return write_json_durable(_pointer_path(archive), narrow_json(value))


def _check_store_root(archive: Path, store_root: Path) -> Path:
    """Refuse a store that would defeat the purpose of having one."""

    resolved_store = store_root.resolve()
    resolved_archive = archive.resolve()
    if resolved_store == resolved_archive:
        raise SnapshotLocationError(
            f"snapshot store {store_root} is the archive itself; the store exists to be outside "
            "the published tree"
        )
    if resolved_archive in resolved_store.parents:
        raise SnapshotLocationError(
            f"snapshot store {store_root} is inside the archive {archive}; the store exists to be "
            "outside the published tree, which is served over HTTP and cloned"
        )
    if resolved_store in resolved_archive.parents:
        raise SnapshotLocationError(
            f"snapshot store {store_root} contains the archive {archive}; that would make the "
            "published tree part of the store"
        )
    return resolved_store


#: What an ingest writes to record which vendor files it copied and what their digests were. Its
#: existence is the archive's own statement that snapshots for that team exist *somewhere*, which
#: is a stronger and more useful fact than the team directory merely being there.
_SOURCE_MANIFEST = ("raw", "source-manifest.json")


def _copied_snapshots_before(archive: Path) -> bool:
    """Whether any team here has ever had vendor bytes copied for it.

    The question this answers is "would creating the recorded store be *establishing* one, or
    silently starting a second one beside snapshots that already exist". A source manifest is the
    right evidence and a team directory is not: a team can be written into an archive without any
    vendor material behind it, and refusing to create a store for that archive would be refusing
    over snapshots that do not exist.
    """

    # Both build-state layouts, because the manifest is no longer necessarily inside the archive:
    # `build_store` moved `raw/` to a sibling directory, and an archive part-way through that
    # migration has manifests in each. Consulting only `teams/` would answer "no snapshots have
    # ever been copied" for a fully migrated archive -- which turns this refusal off exactly where
    # it is needed, since a migrated archive is by definition one that has been ingested before.
    for root in candidate_store_roots(archive):
        if root.is_symlink() or not root.is_dir():
            continue
        if any(
            entry.is_dir()
            and not entry.is_symlink()
            and entry.joinpath(*_SOURCE_MANIFEST).is_file()
            for entry in root.iterdir()
        ):
            return True
    return False


def _recorded_store(archive: Path, store_root: Path) -> Path:
    """The store the pointer means, once the archive may have been moved or renamed.

    The pointer is a *relative* reference so that an archive copied together with its store keeps
    working, and that holds for the case it was designed for: move the enclosing directory and
    both ends move with it. It does not hold for renaming the archive directory itself, which was
    a completely safe operation for as long as the snapshots lived inside it and is the thing an
    operator is most likely to do to a directory called ``summary/widget``.

    Two shapes, and both used to end badly. Rename the archive alone and the pointer still
    resolves -- ``../widget.sources`` is unchanged -- so only the store's marker disagrees, and
    :func:`_foreign_marker` is what stops that from being read as a collision. Rename the archive
    **and** its store, the obvious "keep the pair together" action, and the pointer resolves to a
    directory that no longer exists; :func:`ensure_snapshot_store` would then *create* it, empty,
    in the parent of ``--output``, and the ingest would fail some layers later complaining that a
    previously observed rollout had disappeared -- naming the directory the tool had just
    fabricated while the real store sat beside it under its new name.

    So a recorded store that is not there is not accepted at face value. If the archive's own
    default sibling exists and carries a store marker, that is the pair having moved together,
    and it is adopted -- the marker is the store identifying itself, not a guess from a name. If
    there is no such sibling and this archive has been ingested before, the run refuses and says
    both remedies rather than creating anything. Only a store that was never established yet is
    created from the pointer alone.
    """

    if store_root.is_dir():
        return store_root
    sibling = default_store_root(archive)
    if sibling != store_root and _marker_path(sibling).is_file():
        return _check_store_root(archive, sibling)
    if _copied_snapshots_before(archive):
        raise SnapshotLocationError(
            f"{archive / SNAPSHOT_POINTER_FILE} says this archive's snapshots live in "
            f"{store_root}, and there is no such directory. Snapshots have been copied for this "
            "archive before, so they are somewhere; creating an empty store here would lose "
            "sight of them. Point at where they are with --snapshot-root <path> (or "
            f"`snapshot_root` in the project config), or -- if they really are gone -- delete "
            f"{archive / SNAPSHOT_POINTER_FILE} to start a fresh store, accepting that the "
            "append-only baseline for every team goes with it."
        )
    return store_root


def resolve_snapshot_root(
    archive: Path, team_slug: str, requested: Path | None = None
) -> SnapshotLocation:
    """Decide where one team's snapshots are. Reads the filesystem; writes nothing.

    The write-nothing property is load-bearing rather than incidental. This is called from inside
    every ingest, and the requirement it serves is that an archive is never relocated as a side
    effect of a build. Anything that would need to move bytes raises instead, naming
    ``migrate-snapshots``.
    """

    validate_team_slug(team_slug)
    legacy = legacy_team_root(archive, team_slug)
    legacy_populated = _populated(legacy)
    recorded = read_pointer(archive)

    if requested is not None:
        store_root = _check_store_root(archive, requested)
        # Archive-wide, not team-wide, and the difference is the whole refusal. Asking only about
        # *this* team let a new team be given a store while its siblings were still in the
        # archive -- and the run that succeeded then recorded ``external`` in the archive-wide
        # pointer, after which every already-ingested team hit the interrupted-migration refusal
        # below and was told to finish a migration nobody had started. One archive, one layout,
        # is a property of the archive, so it has to be asked of the archive.
        occupied = legacy_team_roots(archive)
        if legacy_populated or occupied:
            where = (
                str(legacy)
                if legacy_populated
                else ", ".join(str(path) for path in occupied)
            )
            whose = "this team's" if legacy_populated else "this archive's"
            raise SnapshotLocationError(
                f"{where} still holds {whose} snapshots, so --snapshot-root "
                f"{requested} would split this archive across two layouts. Move them first, all "
                f"of them together: wrkviz migrate-snapshots --output {archive} "
                f"--snapshot-root {requested} --move"
            )
        if recorded is not None and recorded.layout == "external":
            existing = (archive.resolve() / recorded.root).resolve() if recorded.root else None
            if existing is not None and existing != store_root and _populated(existing):
                raise SnapshotLocationError(
                    f"{archive / SNAPSHOT_POINTER_FILE} records the store at {existing}, which is "
                    f"populated, but --snapshot-root says {store_root}. Move it deliberately: "
                    f"wrkviz migrate-snapshots --output {archive} "
                    f"--snapshot-root {store_root} --move"
                )
        return SnapshotLocation(
            archive=archive,
            team_slug=team_slug,
            root=store_root / team_slug,
            store_root=store_root,
            layout="external",
            origin="requested",
        )

    if recorded is not None and recorded.layout == "external":
        assert recorded.root is not None
        store_root = _check_store_root(archive, archive.resolve() / recorded.root)
        # Asked before :func:`_recorded_store`, because a half-moved archive has the more
        # specific diagnosis and the resumable remedy, and it would otherwise be reported as the
        # store having gone missing. Asked about *this team* rather than the archive -- unlike
        # the ``--snapshot-root`` branch above -- because a migration in progress is a state the
        # archive is allowed to be in: the teams already moved keep ingesting while the rest
        # refuse by name, which is what makes an interrupted migration a pause rather than an
        # outage.
        if legacy_populated:
            raise SnapshotLocationError(
                f"{archive / SNAPSHOT_POINTER_FILE} says this archive's snapshots live in "
                f"{store_root}, but {legacy} is still populated. That is an interrupted "
                "migration, not a choice: finish it with "
                f"wrkviz migrate-snapshots --output {archive} --move"
                " (add --copy if the store is on another filesystem)"
            )
        store_root = _recorded_store(archive, store_root)
        return SnapshotLocation(
            archive=archive,
            team_slug=team_slug,
            root=store_root / team_slug,
            store_root=store_root,
            layout="external",
            origin="recorded",
        )

    if recorded is not None and recorded.layout == "in-archive-legacy":
        return SnapshotLocation(
            archive=archive,
            team_slug=team_slug,
            root=legacy,
            store_root=None,
            layout="in-archive-legacy",
            origin="recorded",
        )

    if legacy_populated or legacy_team_roots(archive):
        # One archive, one layout. A team added to an unmigrated archive joins the layout its
        # siblings are in rather than becoming the archive's first external team, because an
        # archive whose snapshots are half in and half out is the state every refusal above exists
        # to prevent, and there is no reason to create it deliberately here.
        return SnapshotLocation(
            archive=archive,
            team_slug=team_slug,
            root=legacy,
            store_root=None,
            layout="in-archive-legacy",
            origin="in-archive-legacy",
        )

    store_root = _check_store_root(archive, default_store_root(archive))
    return SnapshotLocation(
        archive=archive,
        team_slug=team_slug,
        root=store_root / team_slug,
        store_root=store_root,
        layout="external",
        origin="default",
    )


def _marker_path(store_root: Path) -> Path:
    return store_root / STORE_MARKER_FILE


def _read_marker_archive(store_root: Path) -> str | None:
    path = _marker_path(store_root)
    if path.is_symlink():
        raise SnapshotLocationError(f"{path}: store marker is a symlink")
    if not path.is_file():
        return None
    obj = as_object(read_json(path), str(path))
    if obj.get("kind") != _MARKER_KIND or obj.get("schema_version") != _SCHEMA_VERSION:
        raise SnapshotLocationError(f"{path}: not a schema-1 {_MARKER_KIND} document")
    return as_string(obj.get("archive"), f"{path}: archive")


def _foreign_marker(archive: Path, store_root: Path) -> str | None:
    """The archive a store claims, when that is a *different* archive that still exists.

    ``None`` means the store is this archive's to use. Three ways to get there: it names this
    archive, it has no marker at all, or it names a path that no longer holds an archive.

    That last clause is the one worth arguing for, because it deliberately weakens a check.
    The marker exists to catch one specific accident -- two archives pointed at one store, which
    collide the moment they share a team slug -- and that accident requires two archives. A
    marker naming a path where no archive is any more is what renaming the archive directory
    leaves behind, and refusing there told an operator with exactly one archive that two of them
    were fighting over a store, then suggested a remedy (``--snapshot-root``) that starts an
    empty second tree and fails again on the append guard. Adopting the new name instead is
    correct and is also the only reading that can be right: there is no other claimant.
    """

    existing = _read_marker_archive(store_root)
    if existing is None:
        return None
    expected = PurePosixPath(os.path.relpath(archive.resolve(), store_root)).as_posix()
    if existing == expected:
        return None
    rival = (store_root / existing).resolve()
    if not archive_marker_path(rival).is_file():
        return None
    return existing


def ensure_snapshot_store(location: SnapshotLocation) -> int:
    """Create the store and the team directory; return how many files this changed.

    Idempotent, and on the steady state it changes nothing: the marker and the ``.gitignore`` are
    written through :func:`write_text_durable`, which compares before it writes.
    """

    if location.store_root is not None:
        # Before the first `mkdir`, not after: everything this creates is created outside
        # ``--output``, so a refusal that arrives after the directory exists has already left a
        # store behind that nobody asked for.
        changed = ensure_store_root(location.archive, location.store_root)
        location.root.mkdir(parents=True, exist_ok=True)
        return changed
    location.root.parent.mkdir(parents=True, exist_ok=True)
    location.root.mkdir(parents=True, exist_ok=True)
    return 0


def ensure_store_root(archive: Path, store_root: Path) -> int:
    """Create the store's own metadata -- its marker and its self-ignoring ``.gitignore``."""

    relative_archive = PurePosixPath(
        os.path.relpath(archive.resolve(), store_root)
    ).as_posix()
    foreign = _foreign_marker(archive, store_root)
    if foreign is not None:
        raise SnapshotLocationError(
            f"{_marker_path(store_root)} says this store belongs to {foreign}, not to "
            f"{relative_archive}, and that archive is still there. Two archives sharing one "
            "store would collide the moment they share a team slug; give this archive its own "
            "--snapshot-root."
        )
    store_root.mkdir(parents=True, exist_ok=True)
    changed = int(
        write_json_durable(
            _marker_path(store_root),
            narrow_json(
                {
                    "schema_version": _SCHEMA_VERSION,
                    "kind": _MARKER_KIND,
                    "archive": relative_archive,
                }
            ),
        )
    )
    changed += int(
        write_text_durable(store_root / STORE_GITIGNORE_FILE, _STORE_GITIGNORE_TEXT)
    )
    return changed


@dataclass(frozen=True)
class TeamMigration:
    """One team's move, described before anything has been touched."""

    team_slug: str
    source: Path
    target: Path
    files: int
    bytes: int
    same_device: bool
    refusal: str | None


@dataclass(frozen=True)
class SnapshotMigrationPlan:
    """What ``migrate-snapshots`` would do, and what it will refuse to do."""

    archive: Path
    store_root: Path
    teams: tuple[TeamMigration, ...]
    already_external: bool

    @property
    def refusals(self) -> tuple[str, ...]:
        """Return every reason this plan declines to move a team, in team order."""

        return tuple(team.refusal for team in self.teams if team.refusal is not None)

    @property
    def movable(self) -> tuple[TeamMigration, ...]:
        """Return the teams this plan would actually relocate."""

        return tuple(team for team in self.teams if team.refusal is None)

    @property
    def total_bytes(self) -> int:
        """Return the bytes that would leave the published tree."""

        return sum(team.bytes for team in self.movable)

    @property
    def total_files(self) -> int:
        """Return the file count that would leave the published tree."""

        return sum(team.files for team in self.movable)

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the plan as a JSON-serializable object."""

        return {
            "archive": str(self.archive),
            "store_root": str(self.store_root),
            "already_external": self.already_external,
            "teams": [
                {
                    "team": team.team_slug,
                    "source": str(team.source),
                    "target": str(team.target),
                    "files": team.files,
                    "bytes": team.bytes,
                    "same_device": team.same_device,
                    "refusal": team.refusal,
                }
                for team in self.teams
            ],
        }


def _tree_size(root: Path) -> tuple[int, int]:
    files = 0
    total = 0
    for dirpath, dirnames, filenames in os.walk(root):
        del dirnames
        for name in filenames:
            path = Path(dirpath) / name
            try:
                info = path.lstat()
            except OSError:
                continue
            files += 1
            total += info.st_size
    return files, total


def _device(path: Path) -> int:
    probe = path
    while not probe.exists():
        parent = probe.parent
        if parent == probe:
            break
        probe = parent
    return probe.stat().st_dev


def plan_snapshot_migration(
    archive: Path, requested: Path | None = None
) -> SnapshotMigrationPlan:
    """Describe the move without performing any part of it."""

    recorded = read_pointer(archive)
    if requested is not None:
        store_root = _check_store_root(archive, requested)
    elif recorded is not None and recorded.layout == "external":
        assert recorded.root is not None
        store_root = _check_store_root(archive, archive.resolve() / recorded.root)
    else:
        store_root = _check_store_root(archive, default_store_root(archive))
    foreign = _foreign_marker(archive, store_root) if store_root.is_dir() else None
    expected_archive = PurePosixPath(
        os.path.relpath(archive.resolve(), store_root)
    ).as_posix()
    store_refusal = (
        f"{_marker_path(store_root)} says this store belongs to {foreign}, not to "
        f"{expected_archive}, and that archive is still there"
        if foreign is not None
        else None
    )
    store_device = _device(store_root)
    teams: list[TeamMigration] = []
    for source in legacy_team_roots(archive):
        team_slug = source.parent.name
        target = store_root / team_slug
        files, total = _tree_size(source)
        refusal = store_refusal
        if refusal is None and _populated(target):
            refusal = (
                f"{target} already holds snapshots; merging two trees is not something this "
                "command will guess at"
            )
        if refusal is None:
            try:
                validate_team_slug(team_slug)
            except ValueError as error:
                refusal = f"{source}: {error}"
        teams.append(
            TeamMigration(
                team_slug=team_slug,
                source=source,
                target=target,
                files=files,
                bytes=total,
                same_device=source.stat().st_dev == store_device,
                refusal=refusal,
            )
        )
    return SnapshotMigrationPlan(
        archive=archive,
        store_root=store_root,
        teams=tuple(teams),
        already_external=recorded is not None and recorded.layout == "external",
    )


@dataclass(frozen=True)
class SnapshotMigrationResult:
    """What ``migrate-snapshots --move`` actually did."""

    plan: SnapshotMigrationPlan
    #: Teams relocated by rename, and teams relocated by copy-verify-delete. Two fields rather
    #: than one, because the second is the interruptible path and a reader of a receipt needs to
    #: know which teams went through it.
    moved: tuple[str, ...]
    copied: tuple[str, ...]
    pointer_changed: bool

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the result as a JSON-serializable object."""

        return {
            "plan": self.plan.to_json_obj(),
            "moved": list(self.moved),
            "copied": list(self.copied),
            "pointer_changed": self.pointer_changed,
        }


def _fsync_directory(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _copy_across_devices(source: Path, target: Path) -> None:
    """Copy into a staging directory, verify it, publish it by rename, then remove the original.

    Only reachable behind ``--copy``, and separate from the rename path because it is a different
    operation with a different failure mode: interrupt a rename and nothing has happened;
    interrupt this and the bytes exist twice. Verification is file count and total size rather
    than digests, and that is a deliberate ceiling -- re-hashing 6.5 GB to confirm a copy the
    kernel already reported success for costs more than the risk it retires, and the *content* is
    verified where it matters anyway, by the per-object digest check the next ingest performs
    against ``raw/source-manifest.json``.

    **The staging directory is what makes an interruption survivable**, and the version without
    it wedged the archive. Copying straight into ``<store>/<slug>`` and dying partway -- a
    ``KeyboardInterrupt``, or the ENOSPC that is the likeliest reason a cross-filesystem move was
    needed in the first place -- left a half tree at the target. From there the two commands
    pointed at each other and neither named the way out: :func:`plan_snapshot_migration` refused
    because "``<target>`` already holds snapshots; merging two trees is not something this
    command will guess at", and every ingest refused because the pointer said external while the
    in-archive directory was still populated, telling the operator to finish a migration that
    could not be re-run. The plausible wrong guess out of that -- delete the *source*, because
    the pointer declares the store authoritative and the copy looks done -- destroys the only
    copy of the vendor logs and the frozen note projections.

    With staging there is no such state. The target is created by an ``os.replace`` of a fully
    verified tree, which is same-device and atomic because the staging directory lives inside the
    store; the source is removed only after that. Anything left behind on failure is a directory
    under ``.staging/``, which is unambiguously this command's own scratch -- ``.staging`` cannot
    be a team slug, the grammar has no leading dot -- so the next run deletes it and starts that
    team again. Re-running the command really does finish the job, on both paths.
    """

    staging = target.parent / _COPY_STAGING_DIRECTORY / target.name
    if staging.exists():
        # Residue from an interrupted run. It is incomplete by construction -- a complete one is
        # renamed away in the same breath as it is verified -- so it is removed, not resumed:
        # deciding which of its files are whole is exactly the guess this module refuses to make
        # about two trees anywhere else.
        shutil.rmtree(staging)
    staging.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, staging, symlinks=True, dirs_exist_ok=False)
    before = _tree_size(source)
    after = _tree_size(staging)
    if before != after:
        shutil.rmtree(staging)
        raise SnapshotLocationError(
            f"copy of {source} to {target} does not match: {before} files/bytes became {after}; "
            "the original has been left alone"
        )
    os.replace(staging, target)
    _fsync_directory(target.parent)
    shutil.rmtree(source)


def migrate_snapshots(
    archive: Path,
    requested: Path | None = None,
    *,
    move: bool = False,
    copy_across_devices: bool = False,
) -> SnapshotMigrationResult:
    """Relocate an existing archive's snapshots out of the published tree.

    The caller holds the archive writer lock. The order inside is the part that matters: the
    pointer is written **first**, before a single byte moves. A crash after that leaves the
    pointer saying "external" with some teams still in the archive, which
    :func:`resolve_snapshot_root` recognises by name as an interrupted migration and refuses to
    build on, and which re-running this command finishes. The other order -- move, then record --
    would leave a crashed run looking like an archive whose teams have simply never been ingested,
    which is the one wrong answer that is indistinguishable from a right one.
    """

    plan = plan_snapshot_migration(archive, requested)
    if not move:
        return SnapshotMigrationResult(plan, (), (), False)
    blocked = [team for team in plan.teams if team.refusal is not None]
    if blocked:
        raise SnapshotLocationError(
            "refusing to migrate; nothing was moved:\n  "
            + "\n  ".join(f"{team.team_slug}: {team.refusal}" for team in blocked)
        )
    cross = [team for team in plan.movable if not team.same_device]
    if cross and not copy_across_devices:
        raise SnapshotLocationError(
            f"{plan.store_root} is on a different filesystem from "
            f"{', '.join(team.team_slug for team in cross)}, so this cannot be a rename. Pass "
            "--copy to accept a copy-verify-delete instead: unlike a rename it is interruptible, "
            "and while it runs the bytes exist twice."
        )
    ensure_store_root(archive, plan.store_root)
    pointer_changed = write_pointer(
        archive,
        SnapshotLocation(
            archive=archive,
            team_slug="",
            root=plan.store_root,
            store_root=plan.store_root,
            layout="external",
            origin="requested",
        ),
    )
    moved: list[str] = []
    copied: list[str] = []
    for team in plan.movable:
        if team.same_device:
            os.replace(team.source, team.target)
            moved.append(team.team_slug)
        else:
            _copy_across_devices(team.source, team.target)
            copied.append(team.team_slug)
        _fsync_directory(team.source.parent)
        _fsync_directory(plan.store_root)
    return SnapshotMigrationResult(plan, tuple(moved), tuple(copied), pointer_changed)


def format_migration_report(result: SnapshotMigrationResult, *, moved: bool) -> str:
    """Render the plan or the outcome as text an operator can act on."""

    plan = result.plan
    lines: list[str] = []
    lines.append(f"archive:    {plan.archive}")
    lines.append(f"store root: {plan.store_root}")
    lines.append("")
    if not plan.teams:
        lines.append(
            "No team keeps snapshots inside the archive."
            + (
                ""
                if plan.already_external
                else " Recording the external layout is all this would do."
            )
        )
    for team in plan.teams:
        verb = "would move"
        if moved:
            verb = "moved" if team.team_slug in result.moved else "copied"
        if team.refusal is not None:
            verb = "REFUSED"
        lines.append(
            f"{team.team_slug}: {verb} {team.files} file(s), {team.bytes} byte(s)\n"
            f"    from {team.source}\n"
            f"      to {team.target}"
            + ("" if team.same_device else "\n    (different filesystem: copy, not rename)")
            + (f"\n    refusal: {team.refusal}" if team.refusal is not None else "")
        )
    lines.append("")
    if moved:
        lines.append(
            f"moved {len(result.moved)} team(s), copied {len(result.copied)}; "
            f"{plan.archive / SNAPSHOT_POINTER_FILE} now records the external layout"
        )
    else:
        lines.append(
            f"dry run: {plan.total_files} file(s), {plan.total_bytes} byte(s) would leave the "
            "published tree. Re-run with --move to do it."
        )
    return "\n".join(lines) + "\n"



def pointer_summary(archive: Path) -> dict[str, JsonValue]:
    """Describe an archive's snapshot layout for ``inspect``, without resolving a team."""

    recorded = read_pointer(archive)
    legacy = legacy_team_roots(archive)
    layout: str
    if recorded is not None:
        layout = recorded.layout
    elif legacy:
        layout = "in-archive-legacy"
    else:
        layout = "external"
    root: JsonValue = None
    if layout == "external":
        root = str(
            (archive.resolve() / recorded.root).resolve()
            if recorded is not None and recorded.root is not None
            else default_store_root(archive)
        )
    files = 0
    total = 0
    for path in legacy:
        count, size = _tree_size(path)
        files += count
        total += size
    return {
        "layout": layout,
        "store_root": root,
        "recorded": recorded is not None,
        "in_archive_teams": [str(path.parent.name) for path in legacy],
        "in_archive_files": files,
        "in_archive_bytes": total,
    }
