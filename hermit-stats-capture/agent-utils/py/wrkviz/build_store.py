"""Where a team's build state lives, and how an existing archive moves it there.

Every number below was measured on 2026-08-25 against one archive, read-only.

This is the second half of a separation :mod:`wrkviz.snapshot_store` started. That
module moved the *vendor input* out of the published directory. This one moves the *intermediate
output*: the normalized team, the artifact catalogue it was extracted from, the provenance
manifests, and the payload store. On the measured archive that is **717 MiB of a 1.5 GiB
archive**, none of which any consumer of the archive opens:

===============================================  ========  =====================================
``teams/<slug>/raw/team.json``                    717 MiB  the normalized team, rewritten each ingest
``teams/<slug>/raw/artifacts.json``                        the catalogue the presentation comes from
``teams/<slug>/raw/source-manifest.json``                  which vendor bytes produced the above
``teams/<slug>/raw/normalized-generation.json``            what the normalizer was when it ran
``teams/<slug>/raw/task-notes.jsonl``                      notes promoted out of the snapshot store
``teams/<slug>/payloads/``                          0 B    tool text ``raw/team.json`` points at
===============================================  ========  =====================================

Both tenants are *regenerable*: they are what ingestion computes from the snapshot store, so
losing them costs CPU and nothing else. They are relocated rather than dropped because they are
also what makes an incremental ingest incremental -- the append-only guards compare against
``source-manifest.json``, and the renderer reads ``team.json`` without re-reading a single vendor
byte. Deleting them after each build would turn every rebuild into a full re-ingest.

What stays inside the archive, and why
--------------------------------------

``teams/<slug>/summaries/`` is rendered Markdown -- 13 MiB that the shipped ``timeline`` CLI reads
for ``stats`` and ``search``. It is output, and it is paid for. It stays.

``teams/<slug>/summary_data/`` -- 47 MiB of summary cache, name cache and glossary -- is
intermediate by every other measure and **is not moved here**, for one specific reason worth
recording rather than rediscovering. Nested inside its cache directory is ``_usage/``: 1,878
receipts recording every model call this archive ever paid for, which the shipped ``run_stats.py``
reads to report the 55,267,957 tokens the archive cost. Those receipts are not cache; they are the
record of the spend, they belong with the output, and they are the one thing here that cannot be
regenerated at any price.

So moving ``summary_data`` means first splitting the receipts out of the cache they happen to
live inside, which is a change to how two summarization backends address their own output. That
is its own change with its own risk, and it buys 47 MiB against the 717 MiB this store already
takes. Recorded here so the next person weighing it starts from the reason rather than the
symptom.

Where the default goes
----------------------

``<output>.build/`` -- a sibling of the published directory, named after it::

    /path/to/summary/widget             <- --output, published, served, shipped
    /path/to/summary/widget.sources/    <- the snapshot store, vendor input
    /path/to/summary/widget.build/      <- this, intermediate state

A sibling for the same reason the snapshot store is one: it is on the same filesystem in every
layout anyone actually has, so migrating an existing archive is ``os.replace`` per team -- atomic,
instant, and impossible to half-finish inside one team. See that module's own note for the full
argument; it applies here unchanged.

How a run finds the store
-------------------------

The same four branches, in the same order, for the same reasons:

1. ``--build-root`` on the command line, or ``build_root`` in a project config.
2. otherwise ``<output>/build-root.json``, written by the run that established the layout.
3. otherwise, if ``teams/<slug>/raw/`` is populated, *that* -- an archive built by an older tool
   keeps working, untouched, with no flag and no migration.
4. otherwise ``<output>.build/``.

Nothing here ever moves a file. An archive stays in the layout it has until an operator runs
``migrate-build-state``. Two disagreements are refusals rather than guesses:

* a requested root while **any** team in the archive still keeps its build state inside it. Asked
  of the archive rather than of the team being built, because "one archive, one layout" is a
  property of the archive.
* a pointer that says the store is external while *this team's* in-archive directory is still
  populated -- an interrupted migration, which reads as "finish it" rather than "start again", and
  which is asked per team so the teams already moved keep building while the rest wait.

The pointer file is inside ``--output``
---------------------------------------

``build-root.json`` is roughly two hundred bytes and it is the one thing about the build layout
that the published directory keeps. That looks like a contradiction of this module's own premise
and is not: a pointer is not intermediate progress. It exists so that an archive which has been
*moved* can still find the state belonging to it, which a derived ``<output>.build`` cannot
promise and an operator's memory should not have to. It costs the shipped artifact nothing
measurable, it is what :mod:`wrkviz.snapshot_store` already does for the same reason,
and consistency between the two stores is worth more than two hundred bytes.
"""

from __future__ import annotations

import hashlib
import os
import shutil
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Literal

from wrkviz.archive import (
    JsonValue,
    as_object,
    as_string,
    narrow_json,
    read_json,
    validate_team_slug,
    write_json_if_changed,
)

#: The name of the pointer the archive keeps, beside ``snapshot-root.json``.
BUILD_POINTER_FILE = "build-root.json"

#: Appended to the archive's own name to form the default sibling store.
DEFAULT_STORE_SUFFIX = ".build"

#: The per-team directories this store owns, relative to ``teams/<slug>/``. ``summaries`` is
#: deliberately absent: it is rendered output, the shipped CLI reads it, and it stays.
#:
#: ``payloads`` holds the tool ``input_text``/``output_text`` that ``raw/team.json`` keeps a
#: reference to instead of inlining. It is empty on the measured archive -- every team there
#: predates it -- but it is the same category as ``raw``: derived from the snapshot store, read
#: only by a rebuild, and never served.
#:
#: ``summary_data`` is deliberately absent; see "What stays inside the archive" above.
TENANTS = ("raw", "payloads")

#: Tenants whose loss costs money rather than CPU, and which are therefore verified by digest
#: before the source is unlinked on a cross-device migration. Empty today, because the one such
#: tenant is the summary cache and that has not moved. Kept rather than inlined as ``False``
#: because the verify path is the part that must already exist when it does.
PAID_TENANTS: frozenset[str] = frozenset()

#: The store entry for build state that belongs to no single team. A leading underscore, so it
#: cannot collide with a team slug -- `validate_team_slug` rejects one.
SHARED_TENANT = "_shared"

#: Shared-tenant files, by the archive-relative path an un-migrated archive keeps them at. The
#: transcript projection's monotonic baseline is the only one: it is the union across every team
#: the projection has ever represented, so it belongs to no slug, and at 106.3 MiB it is the
#: largest single thing the tool writes that nothing consuming an archive ever opens.
SHARED_FILES = ("extracted/transcripts/occurrences.jsonl",)

Layout = Literal["in-archive-legacy", "external"]


class BuildLocationError(ValueError):
    """A build store that cannot be used without an operator deciding something."""


@dataclass(frozen=True)
class BuildLocation:
    """Where one team's build state is, and how that was decided."""

    archive: Path
    team_slug: str
    #: The directory that used to be ``teams/<slug>``, as far as this store's tenants go. Every
    #: path below it -- ``raw/team.json``, ``summary_data/cache`` -- is addressed relative to
    #: this, which is what makes relocating the root a change to one function.
    root: Path
    #: The store holding every team, or ``None`` in the in-archive layout, which has no such thing.
    store_root: Path | None
    layout: Layout
    #: How resolution arrived here. Not persisted: it describes this run, not the archive.
    origin: Literal["requested", "recorded", "in-archive-legacy", "default"]

    @property
    def inside_archive(self) -> bool:
        """Whether this state is inside the directory the operator publishes and ships."""

        return self.layout == "in-archive-legacy"


def default_store_root(archive: Path) -> Path:
    """Return ``<archive>.build``, the store a fresh archive gets when nobody says otherwise."""

    resolved = archive.resolve()
    return resolved.parent / (resolved.name + DEFAULT_STORE_SUFFIX)


def legacy_team_root(archive: Path, team_slug: str) -> Path:
    """Return the in-archive location an older tool wrote, whether or not it exists."""

    validate_team_slug(team_slug)
    return archive / "teams" / team_slug


def _populated(root: Path) -> bool:
    """Whether *root* holds any of this store's tenants with anything in them."""

    return any((root / tenant).is_dir() and any((root / tenant).iterdir()) for tenant in TENANTS)


def legacy_team_roots(archive: Path) -> tuple[Path, ...]:
    """Every team directory in *archive* still holding build state inside the archive."""

    teams = archive / "teams"
    if not teams.is_dir():
        return ()
    return tuple(
        sorted(path for path in teams.iterdir() if path.is_dir() and _populated(path))
    )


@dataclass(frozen=True)
class _Pointer:
    store_root: Path
    layout: Layout


def _pointer_path(archive: Path) -> Path:
    return archive / BUILD_POINTER_FILE


def read_pointer(archive: Path) -> _Pointer | None:
    """Read the archive's recorded build layout, or ``None`` if it has never recorded one."""

    path = _pointer_path(archive)
    if not path.is_file():
        return None
    record = as_object(read_json(path), str(path))
    layout = as_string(record.get("layout"), f"{path}.layout")
    if layout not in ("in-archive-legacy", "external"):
        raise BuildLocationError(f"{path}: unknown build layout {layout!r}")
    raw_root = as_string(record.get("store_root"), f"{path}.store_root")
    # Relative to the archive, resolved here, so an archive moved together with its store keeps
    # working. See `SnapshotLocation.archive_relative` for the same reasoning at the other store.
    return _Pointer(
        store_root=(archive / raw_root).resolve() if raw_root else archive.resolve(),
        layout="external" if layout == "external" else "in-archive-legacy",
    )


def write_pointer(archive: Path, location: BuildLocation) -> bool:
    """Record the layout this archive uses, so the next run does not have to re-derive it."""

    store_root = location.store_root
    relative = (
        PurePosixPath(os.path.relpath(store_root, archive)).as_posix()
        if store_root is not None
        else ""
    )
    record: dict[str, JsonValue] = {
        "schema_version": 1,
        "layout": location.layout,
        "store_root": relative,
    }
    return write_json_if_changed(_pointer_path(archive), narrow_json(record))


def resolve_build_root(
    archive: Path, team_slug: str, requested: Path | None = None
) -> BuildLocation:
    """Decide where one team's build state lives, without creating or moving anything.

    The four branches and two refusals are the module docstring's; this is that text as code.
    """

    validate_team_slug(team_slug)
    legacy = legacy_team_root(archive, team_slug)
    if requested is not None:
        stragglers = legacy_team_roots(archive)
        if stragglers:
            names = ", ".join(path.name for path in stragglers)
            raise BuildLocationError(
                f"--build-root was given, but {len(stragglers)} team(s) still keep build state "
                f"inside the archive: {names}. One archive, one layout: run "
                f"`timeline migrate-build-state --output {archive}` first, or drop the flag."
            )
        store_root = requested.resolve()
        return BuildLocation(
            archive=archive,
            team_slug=team_slug,
            root=store_root / team_slug,
            store_root=store_root,
            layout="external",
            origin="requested",
        )
    pointer = read_pointer(archive)
    if pointer is not None and pointer.layout == "external":
        if _populated(legacy):
            raise BuildLocationError(
                f"{archive / BUILD_POINTER_FILE} says the build store is external, but "
                f"{legacy} still holds this team's state. That is an interrupted migration: "
                f"run `timeline migrate-build-state --output {archive}` to finish it."
            )
        return BuildLocation(
            archive=archive,
            team_slug=team_slug,
            root=pointer.store_root / team_slug,
            store_root=pointer.store_root,
            layout="external",
            origin="recorded",
        )
    if _populated(legacy):
        return BuildLocation(
            archive=archive,
            team_slug=team_slug,
            root=legacy,
            store_root=None,
            layout="in-archive-legacy",
            origin="in-archive-legacy",
        )
    store_root = default_store_root(archive)
    return BuildLocation(
        archive=archive,
        team_slug=team_slug,
        root=store_root / team_slug,
        store_root=store_root,
        layout="external",
        origin="default",
    )


def shared_build_root(archive: Path) -> Path:
    """Return the store's directory for build state that belongs to no single team.

    The transcript projection's monotonic baseline is the only tenant today. It is archive-wide
    rather than per-team by construction -- it is the union across every team the projection has
    ever represented -- so it cannot live under a slug, and a reserved name is how it gets a home
    in the same store without pretending to be one.

    Named with a leading underscore because the store's other entries are team slugs, and
    :func:`validate_team_slug` rejects a leading underscore -- so this name cannot collide with a
    real team, now or after somebody adds one.
    """

    # Asked of the *archive*, not of a team, and that distinction is the bug this comment
    # exists to stop being reintroduced. `resolve_build_root` decides per team, by asking
    # whether that team's directory is populated -- and no team is named "the baseline", so a
    # per-team question about a shared tenant always falls through to the default store and
    # reports an un-migrated archive as migrated.
    pointer = read_pointer(archive)
    if pointer is not None and pointer.layout == "external":
        return pointer.store_root / SHARED_TENANT
    if legacy_team_roots(archive):
        # No store exists, and an extraction must not bring one into being as a side effect:
        # the baseline stays beside the projection it is the baseline for, exactly where an
        # older tool put it, until `migrate-build-state` is run.
        return archive / "extracted" / "transcripts"
    return default_store_root(archive) / SHARED_TENANT


def shared_build_file(archive: Path, name: str) -> Path:
    """Resolve a shared-tenant file for *reading*, accepting either location.

    The store first, then the place an un-migrated archive keeps it. Read-side only, and
    deliberately asymmetric with :func:`shared_build_root`, which is where a write goes: an
    extraction that quietly relocated a hundred megabytes as a side effect of being run is the
    silent migration this module refuses everywhere else. `migrate-build-state` moves it; this
    only stops the interval before that from being an outage.
    """

    preferred = shared_build_root(archive) / name
    if preferred.is_file():
        return preferred
    legacy = archive / "extracted" / "transcripts" / name
    return legacy if legacy.is_file() else preferred


def team_build_root(archive: Path, team_slug: str) -> Path:
    """Return the directory holding one team's build state, wherever that has been put.

    Every path below it -- the normalized team, the artifact catalogue, the provenance manifests,
    the payload store, the summary cache -- is addressed relative to this, which is what makes
    moving 717 MiB out of the published directory a change to one function rather than to a dozen
    call sites. An archive in the old layout resolves to ``teams/<slug>`` and is untouched.

    Resolution only, with no side effect: this is the accessor the read-only callers use, and it
    must not bring a store into existence merely by being asked where one would be.
    """

    return resolve_build_root(archive, team_slug).root


def candidate_store_roots(archive: Path) -> tuple[Path, ...]:
    """Every directory that could hold a team's build state for *archive*, nearest first.

    Both layouts, always, and in that order, because an archive part-way through a migration has
    teams in each and a discovery that consulted only one would report the other half as
    un-ingested. Resolution for a *named* team is still single-valued -- see
    :func:`resolve_build_root`, which refuses exactly this split rather than papering over it.
    """

    pointer = read_pointer(archive)
    roots: list[Path] = []
    if pointer is not None and pointer.layout == "external":
        roots.append(pointer.store_root)
    else:
        default = default_store_root(archive)
        if default.is_dir():
            roots.append(default)
    roots.append(archive / "teams")
    return tuple(roots)


def ingested_team_slugs(archive: Path) -> tuple[str, ...]:
    """Every team slug in *archive* that has been ingested, in either build-state layout.

    "Ingested" means ``raw/team.json`` exists, which is the same test three call sites used to
    spell inline against ``teams/<slug>/``. It is here now because that path is no longer where
    the answer lives, and a discovery loop that hard-codes a layout is the thing that turns a
    relocation into a silent "no teams found".
    """

    found: set[str] = set()
    for root in candidate_store_roots(archive):
        if not root.is_dir():
            continue
        for path in root.iterdir():
            if (
                path.is_dir()
                and not path.is_symlink()
                and (path / "raw" / "team.json").is_file()
            ):
                found.add(path.name)
    return tuple(sorted(found))


def ensure_build_store(location: BuildLocation) -> int:
    """Create the store and its ``.gitignore``; return the number of files written.

    ``/*`` ignores every entry including the ``.gitignore`` itself, so a store that lands inside
    somebody's repository is invisible to ``git status`` rather than being proposed as a
    three-quarter-gigabyte addition. The archive's own ``.gitignore`` cannot do this job, because
    the store is not inside the archive.
    """

    location.root.mkdir(parents=True, exist_ok=True)
    store_root = location.store_root
    if store_root is None:
        return 0
    marker = store_root / ".gitignore"
    if marker.is_file() and marker.read_text(encoding="utf-8") == "/*\n":
        return 0
    marker.write_text("/*\n", encoding="utf-8")
    return 1


@dataclass(frozen=True)
class TeamMigration:
    """One team's move: what a dry run would do, and what an applied run did."""

    team_slug: str
    source: Path
    target: Path
    tenants: tuple[str, ...]
    files: int
    bytes: int
    same_device: bool


@dataclass(frozen=True)
class BuildMigrationPlan:
    """Every team that would move, every shared file that would move, and where to."""

    archive: Path
    store_root: Path
    teams: tuple[TeamMigration, ...]
    #: Shared-tenant files still at their in-archive path, as ``(source, target, bytes)``.
    #: The size is captured when the plan is made, not read back later: an applied run formats
    #: its report *after* the move, when the source no longer exists, and a lazily-stat'd size
    #: would quietly report zero bytes moved for the largest file in the migration.
    shared: tuple[tuple[Path, Path, int], ...] = ()

    @property
    def files(self) -> int:
        """Total regular files this move covers, across teams and shared tenants alike."""

        return sum(team.files for team in self.teams) + len(self.shared)

    @property
    def bytes(self) -> int:
        """Total bytes this move covers, across teams and shared tenants alike."""

        return sum(team.bytes for team in self.teams) + sum(
            size for _source, _target, size in self.shared
        )


def _tree_size(root: Path) -> tuple[int, int]:
    files = 0
    total = 0
    for path in root.rglob("*"):
        if path.is_file() and not path.is_symlink():
            files += 1
            total += path.stat().st_size
    return files, total


def _device(path: Path) -> int:
    probe = path
    while not probe.exists():
        parent = probe.parent
        if parent == probe:
            raise BuildLocationError(f"no existing ancestor for {path}")
        probe = parent
    return probe.stat().st_dev


def plan_build_migration(
    archive: Path, requested: Path | None = None
) -> BuildMigrationPlan:
    """Describe the move without performing any part of it."""

    store_root = (requested or default_store_root(archive)).resolve()
    migrations: list[TeamMigration] = []
    for source in legacy_team_roots(archive):
        tenants = tuple(
            tenant
            for tenant in TENANTS
            if (source / tenant).is_dir() and any((source / tenant).iterdir())
        )
        files = 0
        total = 0
        for tenant in tenants:
            tenant_files, tenant_bytes = _tree_size(source / tenant)
            files += tenant_files
            total += tenant_bytes
        target = store_root / source.name
        migrations.append(
            TeamMigration(
                team_slug=source.name,
                source=source,
                target=target,
                tenants=tenants,
                files=files,
                bytes=total,
                same_device=_device(source) == _device(store_root),
            )
        )
    shared: list[tuple[Path, Path, int]] = []
    for relative in SHARED_FILES:
        source = archive / relative
        if source.is_file() and not source.is_symlink():
            shared.append(
                (
                    source,
                    store_root / SHARED_TENANT / Path(relative).name,
                    source.stat().st_size,
                )
            )
    return BuildMigrationPlan(
        archive=archive,
        store_root=store_root,
        teams=tuple(migrations),
        shared=tuple(shared),
    )


def _digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def _fsync_directory(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _copy_tenant_across_devices(source: Path, target: Path, *, verify: bool) -> None:
    """Copy a tenant tree, optionally verifying every file by digest before the source goes.

    Verification is for the tenant whose loss is a bill rather than a recomputation. It costs a
    second read of bytes already in page cache, and it is what makes "copy, then delete" safe to
    hand an operator by default -- without it, an interrupted or silently short copy is
    indistinguishable from a complete one until the next build reports a cache miss for something
    that was already paid for.
    """

    if target.exists():
        raise BuildLocationError(f"refusing to overwrite existing build state: {target}")
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, target, symlinks=True)
    if verify:
        for path in sorted(source.rglob("*")):
            if not path.is_file() or path.is_symlink():
                continue
            mirrored = target / path.relative_to(source)
            if not mirrored.is_file() or _digest(mirrored) != _digest(path):
                raise BuildLocationError(
                    f"copy of paid build state did not verify: {path} -> {mirrored}; "
                    "the source has been left in place"
                )
    _fsync_directory(target)
    shutil.rmtree(source)


@dataclass(frozen=True)
class BuildMigrationResult:
    """What a migration actually did."""

    plan: BuildMigrationPlan
    moved: tuple[TeamMigration, ...]
    pointer_written: bool


def migrate_build_state(
    archive: Path, requested: Path | None = None, *, apply: bool = False
) -> BuildMigrationResult:
    """Move every team's build state into the store; report the whole move and what ran.

    Dry-run by default, matching ``migrate-snapshots``: the caller opts into the write.
    """

    plan = plan_build_migration(archive, requested)
    if not apply:
        return BuildMigrationResult(plan=plan, moved=(), pointer_written=False)
    moved: list[TeamMigration] = []
    for team in plan.teams:
        team.target.mkdir(parents=True, exist_ok=True)
        for tenant in team.tenants:
            source = team.source / tenant
            target = team.target / tenant
            if team.same_device:
                if target.exists():
                    raise BuildLocationError(
                        f"refusing to overwrite existing build state: {target}"
                    )
                os.replace(source, target)
            else:
                _copy_tenant_across_devices(
                    source, target, verify=tenant in PAID_TENANTS
                )
        _fsync_directory(team.target)
        moved.append(team)
    for source, target, _size in plan.shared:
        if target.exists():
            raise BuildLocationError(
                f"refusing to overwrite existing build state: {target}"
            )
        target.parent.mkdir(parents=True, exist_ok=True)
        if _device(source) == _device(target.parent):
            os.replace(source, target)
        else:
            shutil.copyfile(source, target)
            source.unlink()
        _fsync_directory(target.parent)
    location = BuildLocation(
        archive=archive,
        team_slug=plan.teams[0].team_slug if plan.teams else "placeholder",
        root=plan.store_root,
        store_root=plan.store_root,
        layout="external",
        origin="requested" if requested is not None else "default",
    )
    (plan.store_root).mkdir(parents=True, exist_ok=True)
    gitignore = plan.store_root / ".gitignore"
    if not gitignore.is_file():
        gitignore.write_text("/*\n", encoding="utf-8")
    return BuildMigrationResult(
        plan=plan, moved=tuple(moved), pointer_written=write_pointer(archive, location)
    )


def format_migration_report(result: BuildMigrationResult, *, applied: bool) -> str:
    """Render a migration as the operator-facing summary the CLI prints."""

    plan = result.plan
    verb = "Moved" if applied else "Would move"
    if not plan.teams and not plan.shared:
        return (
            "No build state inside the archive; nothing to migrate.\n"
            f"Store: {plan.store_root}\n"
        )
    lines = [
        f"{verb} build state for {len(plan.teams)} team(s) "
        f"({plan.files:,} files, {plan.bytes:,} bytes) into {plan.store_root}",
    ]
    for source, _target, size in plan.shared:
        lines.append(
            f"  {SHARED_TENANT}: {source.relative_to(plan.archive)} ({size:,} bytes)"
        )
    for team in plan.teams:
        crossing = "" if team.same_device else "  [cross-device copy+verify]"
        lines.append(
            f"  {team.team_slug}: {'+'.join(team.tenants)} "
            f"({team.files:,} files, {team.bytes:,} bytes){crossing}"
        )
    if not applied:
        lines.append("")
        lines.append("Dry run. Re-run with --move to perform the move.")
    return "\n".join(lines) + "\n"


def pointer_summary(archive: Path) -> dict[str, JsonValue]:
    """Describe the recorded layout for a status command, without deciding anything."""

    pointer = read_pointer(archive)
    stragglers = legacy_team_roots(archive)
    return narrow_json(
        {
            "recorded": pointer is not None,
            "layout": pointer.layout if pointer is not None else "",
            "store_root": str(pointer.store_root) if pointer is not None else "",
            "teams_inside_archive": [path.name for path in stragglers],
        }
    )  # type: ignore[return-value]


__all__ = [
    "BUILD_POINTER_FILE",
    "BuildLocation",
    "BuildLocationError",
    "BuildMigrationPlan",
    "BuildMigrationResult",
    "PAID_TENANTS",
    "TENANTS",
    "TeamMigration",
    "candidate_store_roots",
    "default_store_root",
    "ensure_build_store",
    "format_migration_report",
    "ingested_team_slugs",
    "legacy_team_root",
    "legacy_team_roots",
    "migrate_build_state",
    "plan_build_migration",
    "pointer_summary",
    "read_pointer",
    "SHARED_TENANT",
    "resolve_build_root",
    "shared_build_root",
    "team_build_root",
    "write_pointer",
]
