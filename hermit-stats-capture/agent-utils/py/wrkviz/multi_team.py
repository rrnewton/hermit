"""Deterministic, zero-token composition of independently summarized team sites."""

from __future__ import annotations

import hashlib
import re
import tempfile
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    narrow_json,
    read_json,
    read_json_stored,
    write_json_if_changed,
    write_text_if_changed,
)
from wrkviz.pipeline import (
    _archive_writer_locks,
    _build_archive_locked,
    _ensure_archive,
    load_archived_team,
)
from wrkviz.model import TeamData
from wrkviz.periods import DEFAULT_ROLLUP_KINDS
from wrkviz.render import (
    archive_readme,
    prune_retired_query_artifacts,
    retired_generation_files,
)
from wrkviz.search_index import build_search_records
from wrkviz.static_assets import (
    gzip_sidecar_path,
    sync_gzip_sidecar,
    write_gzip_only,
    write_text_with_gzip_invalidation,
)
from wrkviz.timeline_v3 import is_schema_3_path, write_timeline_v3
from wrkviz.window import DateWindow


_ARTIFACT_ID = re.compile(r"artifact-[a-z0-9-]+")
_GLOSSARY_ID = re.compile(r"term-[a-z0-9-]+")
_EXPORT_MANIFEST = "data/export.json"
_COMMON_FILES = (
    "index.html",
    "timeline-core.js",
    "app.js",
    "style.css",
    "serve.py",
    "run_stats.py",
    "timeline",
    "Makefile",
    "vendor/README.md",
    "vendor/markdown-it-15.0.0.min.js",
    "vendor/markdown-it-LICENSE.txt",
)
_RETIRED_COMMON_FILES = frozenset({"query.py"})


@dataclass(frozen=True)
class _RenderedTeam:
    slug: str
    provider: str
    team: TeamData
    root: Path
    timeline: dict[str, JsonValue]
    artifacts: dict[str, JsonValue]


def _namespaced(team_slug: str, value: str) -> str:
    return f"{team_slug}::{value}"


def _artifact_id(team_slug: str, value: str) -> str:
    if _ARTIFACT_ID.fullmatch(value) is None:
        raise ValueError(f"team {team_slug}: invalid artifact ID {value!r}")
    return f"artifact-{team_slug}-{value.removeprefix('artifact-')}"


def _glossary_id(team_slug: str, value: str) -> str:
    if _GLOSSARY_ID.fullmatch(value) is None:
        raise ValueError(f"team {team_slug}: invalid glossary ID {value!r}")
    return f"term-{team_slug}-{value.removeprefix('term-')}"


def _optional_string(value: JsonValue, where: str) -> str | None:
    return None if value is None else as_string(value, where)


def _remap_string_list(
    item: dict[str, JsonValue],
    field: str,
    where: str,
    remap: Callable[[str], str],
) -> None:
    raw = item.get(field)
    if raw is None:
        return
    item[field] = [
        remap(as_string(value, f"{where}.{field}[{index}]"))
        for index, value in enumerate(as_array(raw, f"{where}.{field}"))
    ]


def _team_stats(
    events: Sequence[dict[str, JsonValue]], agent_count: int
) -> dict[str, JsonValue]:
    counts = {
        "user_prompts": 0,
        "agent_responses": 0,
        "inter_agent_messages": 0,
        "external_messages": 0,
        "tool_calls": 0,
    }
    for index, event in enumerate(events):
        kind = as_string(event.get("kind"), f"events[{index}].kind")
        if kind == "user_prompt":
            counts["user_prompts"] += 1
        elif kind == "assistant_message":
            counts["agent_responses"] += 1
        elif kind == "inter_agent_message":
            counts["inter_agent_messages"] += 1
        elif kind == "external_message":
            counts["external_messages"] += 1
        elif kind == "tool_call":
            counts["tool_calls"] += 1
    return {
        **counts,
        "active_agents": agent_count,
        "events": len(events),
    }


def _transform_team(
    rendered: _RenderedTeam,
) -> tuple[dict[str, JsonValue], dict[str, list[dict[str, JsonValue]]]]:
    team_slug = rendered.slug
    timeline = rendered.timeline
    values: dict[str, list[dict[str, JsonValue]]] = {
        key: []
        for key in (
            "agents",
            "phases",
            "activity_bins",
            "edges",
            "events",
            "rollups",
            "summary_files",
            "glossary",
            "project_overviews",
        )
    }

    raw_teams = as_array(timeline.get("teams"), f"{team_slug}.timeline.teams")
    if len(raw_teams) != 1:
        raise ValueError(f"team {team_slug}: rendered timeline must describe exactly one team")
    team = dict(as_object(raw_teams[0], f"{team_slug}.timeline.teams[0]"))
    if as_string(team.get("slug"), f"{team_slug}.timeline.teams[0].slug") != team_slug:
        raise ValueError(f"team {team_slug}: rendered team slug does not match")
    team["provider"] = rendered.provider

    for index, raw in enumerate(
        as_array(timeline.get("agents"), f"{team_slug}.timeline.agents")
    ):
        where = f"{team_slug}.timeline.agents[{index}]"
        item = dict(as_object(raw, where))
        item["id"] = _namespaced(team_slug, as_string(item.get("id"), where + ".id"))
        parent = _optional_string(item.get("parent_id"), where + ".parent_id")
        item["parent_id"] = _namespaced(team_slug, parent) if parent else None
        item["team"] = team_slug
        _remap_string_list(
            item,
            "artifact_ids",
            where,
            lambda value: _artifact_id(team_slug, value),
        )
        _remap_string_list(
            item,
            "output_artifact_ids",
            where,
            lambda value: _artifact_id(team_slug, value),
        )
        values["agents"].append(item)

    for index, raw in enumerate(
        as_array(timeline.get("phases"), f"{team_slug}.timeline.phases")
    ):
        where = f"{team_slug}.timeline.phases[{index}]"
        item = dict(as_object(raw, where))
        phase_id = as_string(item.get("id"), where + ".id")
        item["id"] = _namespaced(team_slug, phase_id)
        item["agent_id"] = _namespaced(
            team_slug, as_string(item.get("agent_id"), where + ".agent_id")
        )
        item["team"] = team_slug
        detail_path = as_string(item.get("detail_path"), where + ".detail_path")
        detail = PurePosixPath(detail_path)
        if (
            detail.parent != PurePosixPath("data/details")
            or detail.name != detail_path.rsplit("/", 1)[-1]
        ):
            raise ValueError(f"{where}.detail_path is not a direct detail file")
        item["detail_path"] = f"data/details/{team_slug}/{detail.name}"
        _remap_string_list(
            item,
            "artifact_ids",
            where,
            lambda value: _artifact_id(team_slug, value),
        )
        _remap_string_list(
            item,
            "output_artifact_ids",
            where,
            lambda value: _artifact_id(team_slug, value),
        )
        values["phases"].append(item)

    for index, raw in enumerate(
        as_array(
            timeline.get("activity_bins", []),
            f"{team_slug}.timeline.activity_bins",
        )
    ):
        where = f"{team_slug}.timeline.activity_bins[{index}]"
        item = dict(as_object(raw, where))
        if as_string(item.get("team"), where + ".team") != team_slug:
            raise ValueError(f"{where}.team does not match its rendered team")
        item["team"] = team_slug
        values["activity_bins"].append(item)

    for index, raw in enumerate(
        as_array(timeline.get("edges"), f"{team_slug}.timeline.edges")
    ):
        where = f"{team_slug}.timeline.edges[{index}]"
        item = dict(as_object(raw, where))
        item["id"] = _namespaced(team_slug, as_string(item.get("id"), where + ".id"))
        for field in ("source_id", "target_id"):
            item[field] = _namespaced(
                team_slug, as_string(item.get(field), f"{where}.{field}")
            )
        item["team"] = team_slug
        values["edges"].append(item)

    for index, raw in enumerate(
        as_array(timeline.get("events"), f"{team_slug}.timeline.events")
    ):
        where = f"{team_slug}.timeline.events[{index}]"
        item = dict(as_object(raw, where))
        item["agent_id"] = _namespaced(
            team_slug, as_string(item.get("agent_id"), where + ".agent_id")
        )
        item["team"] = team_slug
        values["events"].append(item)

    for index, raw in enumerate(
        as_array(timeline.get("rollups"), f"{team_slug}.timeline.rollups")
    ):
        where = f"{team_slug}.timeline.rollups[{index}]"
        item = dict(as_object(raw, where))
        item["team"] = team_slug
        _remap_string_list(
            item,
            "artifact_ids",
            where,
            lambda value: _artifact_id(team_slug, value),
        )
        _remap_string_list(
            item,
            "output_artifact_ids",
            where,
            lambda value: _artifact_id(team_slug, value),
        )
        values["rollups"].append(item)

    for index, raw in enumerate(
        as_array(timeline.get("summary_files"), f"{team_slug}.timeline.summary_files")
    ):
        where = f"{team_slug}.timeline.summary_files[{index}]"
        item = dict(as_object(raw, where))
        item["team"] = team_slug
        values["summary_files"].append(item)

    for index, raw in enumerate(
        as_array(timeline.get("glossary"), f"{team_slug}.timeline.glossary")
    ):
        where = f"{team_slug}.timeline.glossary[{index}]"
        item = dict(as_object(raw, where))
        term_id = _glossary_id(
            team_slug, as_string(item.get("id"), where + ".id")
        )
        item["id"] = term_id
        item["url"] = f"#glossary/{term_id}"
        item["team"] = team_slug
        values["glossary"].append(item)

    overview = timeline.get("project_overview")
    if overview is not None:
        overview_item = dict(
            as_object(overview, f"{team_slug}.timeline.project_overview")
        )
        overview_item["team"] = team_slug
        values["project_overviews"].append(overview_item)

    team["stats"] = _team_stats(values["events"], len(values["agents"]))
    return team, values


def _transform_artifacts(
    rendered: _RenderedTeam,
) -> tuple[list[dict[str, JsonValue]], list[dict[str, JsonValue]]]:
    team_slug = rendered.slug
    artifacts: list[dict[str, JsonValue]] = []
    projects: list[dict[str, JsonValue]] = []
    for index, raw in enumerate(
        as_array(rendered.artifacts.get("artifacts"), f"{team_slug}.artifacts.artifacts")
    ):
        where = f"{team_slug}.artifacts.artifacts[{index}]"
        item = dict(as_object(raw, where))
        item["artifact_id"] = _artifact_id(
            team_slug, as_string(item.get("artifact_id"), where + ".artifact_id")
        )
        producer = _optional_string(
            item.get("producer_thread_id"), where + ".producer_thread_id"
        )
        item["producer_thread_id"] = (
            _namespaced(team_slug, producer) if producer else None
        )
        evidence_values: list[JsonValue] = []
        for evidence_index, raw_evidence in enumerate(
            as_array(item.get("evidence"), where + ".evidence")
        ):
            evidence_where = f"{where}.evidence[{evidence_index}]"
            evidence = dict(as_object(raw_evidence, evidence_where))
            for field in ("evidence_id", "source_id", "thread_id"):
                evidence[field] = _namespaced(
                    team_slug,
                    as_string(evidence.get(field), f"{evidence_where}.{field}"),
                )
            turn = _optional_string(
                evidence.get("turn_id"), evidence_where + ".turn_id"
            )
            evidence["turn_id"] = _namespaced(team_slug, turn) if turn else None
            evidence_values.append(evidence)
        item["evidence"] = evidence_values
        item["team"] = team_slug
        artifacts.append(item)

    for index, raw in enumerate(
        as_array(rendered.artifacts.get("projects"), f"{team_slug}.artifacts.projects")
    ):
        where = f"{team_slug}.artifacts.projects[{index}]"
        item = dict(as_object(raw, where))
        item["project_id"] = _namespaced(
            team_slug, as_string(item.get("project_id"), where + ".project_id")
        )
        _remap_string_list(
            item,
            "evidence_ids",
            where,
            lambda value: _namespaced(team_slug, value),
        )
        item["team"] = team_slug
        projects.append(item)
    return artifacts, projects


def _transform_detail(
    team_slug: str, detail: dict[str, JsonValue], where: str
) -> dict[str, JsonValue]:
    item = dict(detail)
    item["team"] = team_slug
    _remap_string_list(
        item,
        "artifact_ids",
        where,
        lambda value: _artifact_id(team_slug, value),
    )
    _remap_string_list(
        item,
        "output_artifact_ids",
        where,
        lambda value: _artifact_id(team_slug, value),
    )
    return item


def _combined_digest(rendered_teams: Sequence[_RenderedTeam]) -> str:
    source_values: list[JsonValue] = []
    for rendered in rendered_teams:
        source_values.append(
            {
                "team": rendered.slug,
                "source_digest": as_string(
                    rendered.timeline.get("source_digest"),
                    f"{rendered.slug}.timeline.source_digest",
                ),
            }
        )
    return hashlib.sha256(canonical_json(source_values).encode("utf-8")).hexdigest()


def _safe_generated_path(raw: str) -> PurePosixPath:
    path = PurePosixPath(raw)
    if (
        path.is_absolute()
        or not path.parts
        or any(part in ("", ".", "..") for part in path.parts)
    ):
        raise ValueError(f"unsafe generated path in export manifest: {raw!r}")
    # Schema 3 before the `.gz` rule below, which reads a trailing `.gz` as the compressed
    # sidecar of a plain file and recurses on the base. A schema-3 shard has no plain base --
    # existing once is the point -- so recursing would ask whether a file that is never written
    # is a legal export path.
    if is_schema_3_path(raw):
        return path
    if raw.endswith(".gz"):
        if raw.removesuffix(".gz").endswith(".gz"):
            raise ValueError(f"nested gzip sidecar in export manifest: {raw!r}")
        _safe_generated_path(raw.removesuffix(".gz"))
        return path
    if raw in _COMMON_FILES or raw in _RETIRED_COMMON_FILES or raw in {
        "README.md",
        _EXPORT_MANIFEST,
    }:
        return path
    if (
        len(path.parts) >= 4
        and path.parts[0] == "teams"
        and path.parts[2] == "summaries"
        and path.suffix == ".md"
    ):
        return path
    if (
        len(path.parts) == 4
        and path.parts[:2] == ("data", "details")
        and path.suffix == ".json"
    ):
        return path
    if raw in {
        "data/timeline.json",
        "data/timeline-v2.json",
        "data/timeline-v2/manifest.json",
        "data/artifacts.json",
    }:
        return path
    if (
        len(path.parts) == 4
        and path.parts[:3] == ("data", "timeline-v2", "objects")
        and re.fullmatch(r"[0-9a-f]{64}\.json", path.name) is not None
    ):
        return path
    raise ValueError(f"unrecognized generated path in export manifest: {raw!r}")


def _output_path(output: Path, raw: str) -> Path:
    relative = _safe_generated_path(raw)
    root = output.resolve()
    cursor = output
    for part in relative.parts[:-1]:
        cursor /= part
        if cursor.is_symlink():
            raise ValueError(f"generated output path crosses a symlink: {raw!r}")
    candidate = output.joinpath(*relative.parts)
    try:
        candidate.parent.resolve().relative_to(root)
    except ValueError as error:
        raise ValueError(f"generated output path escapes through a symlink: {raw!r}") from error
    if candidate.is_symlink():
        raise ValueError(f"refusing generated output symlink: {candidate}")
    return candidate


def _previous_generated_files(output: Path) -> set[str]:
    path = _output_path(output, _EXPORT_MANIFEST)
    if not path.is_file():
        return set()
    root = as_object(read_json(path), str(path))
    if root.get("schema_version") != 1 or root.get("kind") != "multi-team-export":
        raise ValueError(f"unsupported multi-team export manifest at {path}")
    values: set[str] = set()
    raw_files = as_array(root.get("generated_files"), str(path) + ".generated_files")
    for index, raw in enumerate(raw_files):
        value = as_string(raw, f"{path}.generated_files[{index}]")
        _safe_generated_path(value)
        values.add(value)
    return values


def _remove_stale_files(output: Path, previous: set[str], current: set[str]) -> int:
    changed = 0
    for raw in sorted(previous - current):
        relative = _safe_generated_path(raw)
        path = _output_path(output, relative.as_posix())
        if path.is_file() or path.is_symlink():
            path.unlink()
            changed += 1
    return changed


def _copy_text_file(source: Path, target: Path) -> int:
    if not source.is_file():
        raise ValueError(f"rendered team output is missing {source}")
    return write_text_with_gzip_invalidation(
        target,
        source.read_text(encoding="utf-8"),
        executable=bool(source.stat().st_mode & 0o111),
    )


def _write_compressible_json(path: Path, value: JsonValue) -> int:
    return write_text_with_gzip_invalidation(
        path, canonical_json(value)
    )


def _write_gzip_only_json(path: Path, value: JsonValue) -> int:
    """Store a browser-facing document as ``<path>.gz`` with no identity twin.

    The combiner's counterpart to ``render._write_gzip_only_json``; see that docstring for
    why the two families this is used for are stored compressed and nothing else.
    """

    return write_gzip_only(path, canonical_json(value))


def _build_combined_archive_locked(
    archive: Path,
    team_slugs: Sequence[str],
    *,
    output: Path,
    display_timezone: str,
    phase_minutes: int = 30,
    display_window: DateWindow | None = None,
    rollup_kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
) -> dict[str, int]:
    """Build one collision-safe static site from two or more cached team archives."""

    ordered_slugs = tuple(sorted(team_slugs))
    if len(ordered_slugs) < 2:
        raise ValueError("combined export requires at least two teams")
    if len(ordered_slugs) != len(set(ordered_slugs)):
        raise ValueError("combined export team slugs must be unique")
    _ensure_archive(output, ordered_slugs[0], create=True)
    previous_files = _previous_generated_files(output)

    with tempfile.TemporaryDirectory(prefix="wrkviz-combine-") as raw_tmp:
        temporary_root = Path(raw_tmp)
        rendered_teams: list[_RenderedTeam] = []
        for team_slug in ordered_slugs:
            team = load_archived_team(archive, team_slug)
            team_root = temporary_root / team_slug
            _build_archive_locked(
                archive,
                team_slug,
                phase_minutes=phase_minutes,
                display_window=display_window,
                rollup_kinds=rollup_kinds,
                output=team_root,
                _precompress=False,
                _published=False,
            )
            timeline_path = team_root / "data" / "timeline.json"
            artifacts_path = team_root / "data" / "artifacts.json"
            rendered_teams.append(
                _RenderedTeam(
                    slug=team_slug,
                    provider=team.provider,
                    team=team,
                    root=team_root,
                    timeline=as_object(read_json(timeline_path), str(timeline_path)),
                    artifacts=as_object(
                        read_json_stored(artifacts_path), str(artifacts_path)
                    ),
                )
            )

        merged: dict[str, list[dict[str, JsonValue]]] = {
            key: []
            for key in (
                "teams",
                "agents",
                "phases",
                "activity_bins",
                "edges",
                "events",
                "rollups",
                "summary_files",
                "glossary",
                "project_overviews",
                "artifacts",
                "projects",
            )
        }
        generated_files = set(_COMMON_FILES)
        generated_files.update(
            {
                "README.md",
                _EXPORT_MANIFEST,
            }
        )
        compressible_files = {
            relative
            for relative in _COMMON_FILES
            if PurePosixPath(relative).suffix in {".css", ".html", ".js"}
        }
        #: Relatives whose stored form is ``<relative>.gz`` alone. Disjoint from
        #: `compressible_files`, which keeps an identity file and gains a companion.
        gzip_only_files: set[str] = set()
        changed = 0

        first_root = rendered_teams[0].root
        for relative in _COMMON_FILES:
            changed += int(
                _copy_text_file(
                    first_root / relative, _output_path(output, relative)
                )
            )

        for rendered in rendered_teams:
            team_record, values = _transform_team(rendered)
            merged["teams"].append(team_record)
            for key, items in values.items():
                merged[key].extend(items)
            artifacts, projects = _transform_artifacts(rendered)
            merged["artifacts"].extend(artifacts)
            merged["projects"].extend(projects)

            source_team_root = rendered.root / "teams" / rendered.slug
            for source in sorted(source_team_root.rglob("*")):
                if not source.is_file():
                    continue
                relative = source.relative_to(rendered.root).as_posix()
                if relative.endswith(".gz"):
                    continue
                _safe_generated_path(relative)
                generated_files.add(relative)
                if PurePosixPath(relative).suffix == ".md":
                    compressible_files.add(relative)
                changed += int(
                    _copy_text_file(source, _output_path(output, relative))
                )

            source_phases = as_array(
                rendered.timeline.get("phases"), f"{rendered.slug}.timeline.phases"
            )
            for index, raw_phase in enumerate(source_phases):
                where = f"{rendered.slug}.timeline.phases[{index}]"
                phase = as_object(raw_phase, where)
                source_relative = as_string(
                    phase.get("detail_path"), where + ".detail_path"
                )
                source_path = rendered.root / source_relative
                detail = as_object(read_json_stored(source_path), str(source_path))
                target_relative = (
                    f"data/details/{rendered.slug}/{PurePosixPath(source_relative).name}"
                )
                _safe_generated_path(target_relative)
                # Named by the file that exists, and only that -- the `.gz` entry is added from
                # `gzip_only_files` below. Recording the plain relative here as well put 11,899
                # names in the export manifest for files this build deliberately does not write,
                # which makes the manifest a false inventory of the archive it describes.
                gzip_only_files.add(target_relative)
                changed += _write_gzip_only_json(
                    _output_path(output, target_relative),
                    _transform_detail(rendered.slug, detail, str(source_path)),
                )

        ranges = [
            as_object(rendered.timeline.get("range"), f"{rendered.slug}.timeline.range")
            for rendered in rendered_teams
        ]
        start_ms = min(
            as_int(value.get("start_ms"), "timeline.range.start_ms")
            for value in ranges
        )
        end_ms = max(
            as_int(value.get("end_ms"), "timeline.range.end_ms")
            for value in ranges
        )
        generated_at = max(
            as_string(
                rendered.timeline.get("generated_at"),
                f"{rendered.slug}.timeline.generated_at",
            )
            for rendered in rendered_teams
        )
        total_stats = _team_stats(merged["events"], len(merged["agents"]))
        timeline = as_object(
            narrow_json(
                {
                    "schema_version": 1,
                    "generated_at": generated_at,
                    "source_digest": _combined_digest(rendered_teams),
                    "display_timezone": display_timezone,
                    "display_timezone_source": "combined_export",
                    "range": {"start_ms": start_ms, "end_ms": end_ms},
                    "teams": merged["teams"],
                    "agents": merged["agents"],
                    "phases": merged["phases"],
                    "activity_bins": merged["activity_bins"],
                    "edges": merged["edges"],
                    "events": merged["events"],
                    "rollups": merged["rollups"],
                    "glossary": merged["glossary"],
                    "glossary_path": "",
                    "project_overviews": merged["project_overviews"],
                    "summary_files": merged["summary_files"],
                    "artifact_catalog_path": "data/artifacts.json",
                    "projects": merged["projects"],
                    "stats": total_stats,
                }
            ),
            "combined timeline",
        )
        artifact_catalog = as_object(
            narrow_json(
                {
                    "schema_version": 1,
                    "extractor_version": "multi-team-export-v1",
                    "source_digest": timeline["source_digest"],
                    "teams": list(ordered_slugs),
                    "artifacts": merged["artifacts"],
                    "projects": merged["projects"],
                }
            ),
            "combined artifact catalog",
        )
        # No `data/timeline.json` here. The merged monolith was 243,001,813 bytes plus a
        # 33,562,114-byte twin on the measured archive, and every reader of a combined export
        # reaches schema 1 only after schema 3 and schema 2 have both failed -- both of which
        # this function writes, a few lines below, before it names anything. See
        # `render.retired_generation_files` for why an older one already on disk is left where it
        # is rather than swept.
        changed += _write_gzip_only_json(
            _output_path(output, "data/artifacts.json"), artifact_catalog
        )
        gzip_only_files.add("data/artifacts.json")
        search_records: list[dict[str, JsonValue]] = []
        for rendered in rendered_teams:
            rendered_range = as_object(
                rendered.timeline.get("range"),
                f"{rendered.slug}.timeline.range",
            )
            visible_agent_ids = frozenset(
                as_string(
                    as_object(raw_agent, "timeline agent").get("id"),
                    "timeline agent.id",
                )
                for raw_agent in as_array(
                    rendered.timeline.get("agents"),
                    f"{rendered.slug}.timeline.agents",
                )
            )
            search_records.extend(
                build_search_records(
                    rendered.team,
                    visible_agent_ids,
                    namespace_agents=True,
                    start_ms=as_int(
                        rendered_range.get("start_ms"),
                        f"{rendered.slug}.timeline.range.start_ms",
                    ),
                    end_ms=as_int(
                        rendered_range.get("end_ms"),
                        f"{rendered.slug}.timeline.range.end_ms",
                    ),
                )
            )
        readme = archive_readme(
            "Combined agent-team timeline",
            "Teams: "
            + ", ".join(f"`{slug}`" for slug in ordered_slugs)
            + "\n\nThis directory is a self-contained, zero-token export of cached summaries.",
            "The requested export slice is recorded in `data/export.json` under "
            "`display_window`; `./timeline` reports the actual team and record intervals. "
            "Do not infer the slice from file modification times.",
        )
        changed += int(
            write_text_if_changed(_output_path(output, "README.md"), readme)
        )

        for relative in sorted(compressible_files):
            source_path = _output_path(output, relative)
            changed += int(sync_gzip_sidecar(source_path))
            sidecar = gzip_sidecar_path(source_path)
            if sidecar.is_file():
                sidecar_relative = relative + ".gz"
                _safe_generated_path(sidecar_relative)
                generated_files.add(sidecar_relative)
        # A gzip-only relative is named by the file that exists. Naming the plain
        # relative would point stale-file removal at something never written and
        # leave the `.gz` unnamed, which is the shape that gets swept as an orphan.
        for relative in sorted(gzip_only_files):
            sidecar_relative = relative + ".gz"
            _safe_generated_path(sidecar_relative)
            generated_files.add(sidecar_relative)

        # Schema 3, and only schema 3. `write_timeline_shards` used to run here first, and
        # stopped the day `static/app.js` learned to read schema 3 -- item 2 of the checklist on
        # `timeline_shards.SCHEMA_2_IS_PUBLISHED`, the other half of which is the call site in
        # `render._render_team`. Its paths go into the same manifest as everything else, which is
        # what makes the caller's existing stale-file removal retire a shard whose day or team has
        # gone -- schema 3 needs no reachability manifest of its own because its names say what
        # they are.
        v3_report = write_timeline_v3(output, timeline, search_records=search_records)
        changed += v3_report.files_changed
        for relative in v3_report.generated_files:
            _safe_generated_path(relative)
            generated_files.add(relative)
        retired_files = retired_generation_files(output, generated_files)
        for relative in retired_files:
            _safe_generated_path(relative)
        export_manifest = as_object(
            narrow_json(
                {
                    "schema_version": 1,
                    "kind": "multi-team-export",
                    "teams": list(ordered_slugs),
                    "display_timezone": display_timezone,
                    "display_window": (
                        narrow_json(display_window.to_json_obj())
                        if display_window is not None
                        else None
                    ),
                    "rollup_kinds": list(rollup_kinds),
                    "source_digest": timeline["source_digest"],
                    "generated_files": sorted(generated_files),
                    "retired_files": sorted(retired_files),
                }
            ),
            "combined export manifest",
        )
        retired_query_was_managed = "query.py" in previous_files
        changed += _remove_stale_files(
            output, previous_files - set(retired_files), generated_files
        )
        changed += prune_retired_query_artifacts(
            output,
            manifest_owned=retired_query_was_managed,
        )
        changed += int(
            write_json_if_changed(
                _output_path(output, _EXPORT_MANIFEST), export_manifest
            )
        )

    return {
        "files_changed": changed,
        "teams": len(ordered_slugs),
        "phases": len(merged["phases"]),
        "activity_bins": len(merged["activity_bins"]),
        "agents": len(merged["agents"]),
        "edges": len(merged["edges"]),
        "events": len(merged["events"]),
        "rollups": len(merged["rollups"]),
        "summary_files": len(merged["summary_files"]),
        "artifacts": len(merged["artifacts"]),
        "projects": len(merged["projects"]),
        # Repointed at schema 3 with their names unchanged; see the same six keys in
        # `render._render_team` for what each one now means and why the two transfer figures are
        # equal to the two byte figures.
        "detail_shards": v3_report.timeline_shards,
        "search_records": len(search_records),
        "search_shards": v3_report.search_shards,
        "bootstrap_bytes": v3_report.bootstrap_bytes,
        "bootstrap_transfer_bytes": v3_report.bootstrap_bytes,
        "shard_object_bytes": v3_report.compressed_bytes + v3_report.index_bytes,
        "shard_transfer_bytes": v3_report.compressed_bytes + v3_report.index_bytes,
    }


def build_combined_archive(
    archive: Path,
    team_slugs: Sequence[str],
    *,
    output: Path,
    display_timezone: str,
    phase_minutes: int = 30,
    display_window: DateWindow | None = None,
    rollup_kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
) -> dict[str, int]:
    """Build a combined site while serializing its source snapshot and target."""

    with _archive_writer_locks(archive, output):
        return _build_combined_archive_locked(
            archive,
            team_slugs,
            output=output,
            display_timezone=display_timezone,
            phase_minutes=phase_minutes,
            display_window=display_window,
            rollup_kinds=rollup_kinds,
        )


__all__ = ["build_combined_archive"]
