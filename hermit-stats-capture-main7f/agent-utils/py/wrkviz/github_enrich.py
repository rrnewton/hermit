"""Discover evidenced pull references and cache bounded GitHub metadata."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re

from wrkviz.build_store import team_build_root as _build_root
from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    read_json,
)
from wrkviz.github_metadata import (
    DEFAULT_TIMEOUT_SECONDS,
    GitHubMetadataError,
    HttpTransport,
    PullRequestKey,
    fetch_pull_request,
    load_pull_request_metadata_cache,
    save_pull_request_metadata_cache,
)
from wrkviz.github_refs import find_pull_request_references


_TEAM_SLUG = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\Z")


@dataclass(frozen=True, slots=True)
class PullMetadataReport:
    """Observable result of one conditional metadata refresh."""

    references: int
    distinct_pulls: int
    fetched: int
    not_modified: int
    failures: tuple[str, ...]
    cache_path: str


def pull_metadata_path(archive: Path, team_slug: str) -> Path:
    """Return the durable per-team pull metadata cache path."""

    if len(team_slug) > 64 or _TEAM_SLUG.fullmatch(team_slug) is None:
        raise ValueError(
            "team slug must be 1-64 lowercase letters/digits separated by single hyphens"
        )
    return archive / "teams" / team_slug / "summary_data" / "github" / "pulls.json"


def _structured_key(value: JsonValue, where: str) -> PullRequestKey:
    item = as_object(value, where)
    return PullRequestKey(
        repository=as_string(item.get("repository"), where + ".repository"),
        number=as_int(item.get("number"), where + ".number"),
    )


def _entry_keys(value: JsonValue, where: str) -> tuple[PullRequestKey, ...]:
    item = as_object(value, where)
    text_value = item.get("text")
    text = text_value if isinstance(text_value, str) else ""
    keys = {
        PullRequestKey(reference.link.repository.slug, reference.link.number)
        for reference in find_pull_request_references(text)
    }
    raw_references = item.get("pull_requests")
    if raw_references is not None:
        for index, reference in enumerate(
            as_array(raw_references, where + ".pull_requests")
        ):
            keys.add(_structured_key(reference, f"{where}.pull_requests[{index}]"))
    return tuple(sorted(keys))


def discover_pull_request_keys(archive: Path) -> tuple[int, tuple[PullRequestKey, ...]]:
    """Read generated phase details and return occurrence and unique-key counts."""

    details_root = archive / "data" / "details"
    if not details_root.is_dir():
        raise ValueError(
            f"no generated phase details at {details_root}; run build before GitHub enrichment"
        )
    occurrences = 0
    keys: set[PullRequestKey] = set()
    for path in sorted(details_root.glob("*.json")):
        detail = as_object(read_json(path), str(path))
        for field in ("work_summary", "transcript"):
            for index, raw_entry in enumerate(
                as_array(detail.get(field), f"{path}.{field}")
            ):
                entry_keys = _entry_keys(raw_entry, f"{path}.{field}[{index}]")
                occurrences += len(entry_keys)
                keys.update(entry_keys)
    return occurrences, tuple(sorted(keys))


def enrich_pull_request_metadata(
    archive: Path,
    team_slug: str,
    *,
    token: str | None = None,
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
    transport: HttpTransport | None = None,
    fetched_at: str | None = None,
) -> PullMetadataReport:
    """Conditionally refresh every evidenced pull and retain successful progress."""

    references, keys = discover_pull_request_keys(archive)
    cache_path = pull_metadata_path(archive, team_slug)
    cache = load_pull_request_metadata_cache(cache_path)
    fetched = 0
    not_modified = 0
    failures: list[str] = []
    for key in keys:
        try:
            result = fetch_pull_request(
                key,
                cached=cache.get(key),
                token=token,
                fetched_at=fetched_at,
                timeout_seconds=timeout_seconds,
                transport=transport,
            )
            if result.not_modified:
                not_modified += 1
                continue
            cache.put(result.metadata)
            save_pull_request_metadata_cache(cache_path, cache)
            fetched += 1
        except (GitHubMetadataError, OSError) as error:
            failures.append(f"{key.cache_key}: {error}")
    return PullMetadataReport(
        references=references,
        distinct_pulls=len(keys),
        fetched=fetched,
        not_modified=not_modified,
        failures=tuple(failures),
        cache_path=str(cache_path),
    )


__all__ = [
    "PullMetadataReport",
    "discover_pull_request_keys",
    "enrich_pull_request_metadata",
    "pull_metadata_path",
]
