"""Deterministic logical-key index for model-generated timeline artifacts."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    read_json,
    write_json_if_changed,
)
from wrkviz.summary_artifacts import SummaryArtifactProvenance


SUMMARY_CATALOG_SCHEMA_VERSION = 1


def _validate_relative_path(value: str) -> str:
    path = PurePosixPath(value)
    if (
        not value
        or path.is_absolute()
        or value != path.as_posix()
        or any(part in {"", ".", ".."} for part in path.parts)
    ):
        raise ValueError(f"unsafe summary artifact cache path {value!r}")
    return value


@dataclass(frozen=True, slots=True)
class SummaryArtifactReference:
    """One immutable artifact and its cache location inside a team summary tree."""

    provenance: SummaryArtifactProvenance
    cache_path: str

    def __post_init__(self) -> None:
        _validate_relative_path(self.cache_path)
        if Path(self.cache_path).name != f"{self.provenance.input_hash}.json":
            raise ValueError("summary artifact cache filename does not match input hash")

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return a strict persisted catalog record."""

        return {
            "artifact": self.provenance.to_json_obj(),
            "cache_path": self.cache_path,
        }

    @classmethod
    def from_json_obj(
        cls, value: JsonValue, where: str
    ) -> SummaryArtifactReference:
        """Parse and validate one persisted catalog record."""

        obj = as_object(value, where)
        if set(obj) != {"artifact", "cache_path"}:
            raise ValueError(f"{where}: invalid summary artifact reference fields")
        return cls(
            provenance=SummaryArtifactProvenance.from_json_obj(
                obj.get("artifact"), f"{where}.artifact"
            ),
            cache_path=_validate_relative_path(
                as_string(obj.get("cache_path"), f"{where}.cache_path")
            ),
        )


@dataclass(frozen=True, slots=True)
class SummaryArtifactCatalog:
    """All indexed model artifacts for one team, ordered deterministically."""

    team_slug: str
    records: tuple[SummaryArtifactReference, ...]

    def __post_init__(self) -> None:
        if not self.team_slug:
            raise ValueError("summary artifact catalog team slug must not be empty")
        artifact_ids = tuple(item.provenance.artifact_id for item in self.records)
        if len(artifact_ids) != len(set(artifact_ids)):
            raise ValueError("summary artifact catalog contains duplicate artifact IDs")
        if any(item.provenance.team_slug != self.team_slug for item in self.records):
            raise ValueError("summary artifact catalog mixes team slugs")
        if self.records != _ordered_records(self.records):
            raise ValueError("summary artifact catalog records are not canonical")

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the complete catalog plus inspectable aggregate counts."""

        versions: dict[str, int] = {}
        models: dict[str, int] = {}
        frontier_statuses: dict[str, int] = {}
        for record in self.records:
            provenance = record.provenance
            version_key = (
                f"{provenance.summarizer_id}@{provenance.summarizer_version}"
                f"/schema-{provenance.output_schema_version}"
            )
            versions[version_key] = versions.get(version_key, 0) + 1
            models[provenance.model] = models.get(provenance.model, 0) + 1
            frontier = provenance.context_coverage.frontier_status
            frontier_statuses[frontier] = frontier_statuses.get(frontier, 0) + 1
        logical_keys = len(
            {record.provenance.logical_key for record in self.records}
        )
        return {
            "schema_version": SUMMARY_CATALOG_SCHEMA_VERSION,
            "team_slug": self.team_slug,
            "artifact_count": len(self.records),
            "logical_key_count": logical_keys,
            "version_counts": dict(sorted(versions.items())),
            "model_counts": dict(sorted(models.items())),
            "frontier_status_counts": dict(sorted(frontier_statuses.items())),
            "artifacts": [record.to_json_obj() for record in self.records],
        }

    @classmethod
    def from_json_obj(
        cls, value: JsonValue, where: str
    ) -> SummaryArtifactCatalog:
        """Parse a catalog and verify its derived aggregate counts."""

        obj = as_object(value, where)
        expected = {
            "schema_version",
            "team_slug",
            "artifact_count",
            "logical_key_count",
            "version_counts",
            "model_counts",
            "frontier_status_counts",
            "artifacts",
        }
        if set(obj) != expected:
            raise ValueError(f"{where}: invalid summary artifact catalog fields")
        if as_int(obj.get("schema_version"), f"{where}.schema_version") != (
            SUMMARY_CATALOG_SCHEMA_VERSION
        ):
            raise ValueError(f"{where}: unsupported summary artifact catalog schema")
        catalog = cls(
            team_slug=as_string(obj.get("team_slug"), f"{where}.team_slug"),
            records=_ordered_records(
                tuple(
                    SummaryArtifactReference.from_json_obj(
                        item, f"{where}.artifacts[{index}]"
                    )
                    for index, item in enumerate(
                        as_array(obj.get("artifacts"), f"{where}.artifacts")
                    )
                )
            ),
        )
        rendered = catalog.to_json_obj()
        for key in (
            "artifact_count",
            "logical_key_count",
            "version_counts",
            "model_counts",
            "frontier_status_counts",
        ):
            if obj.get(key) != rendered[key]:
                raise ValueError(f"{where}.{key}: derived catalog value mismatch")
        return catalog


def _record_order(record: SummaryArtifactReference) -> tuple[object, ...]:
    provenance = record.provenance
    return (
        provenance.logical_key,
        provenance.summarizer_id,
        provenance.summarizer_version,
        provenance.output_schema_version,
        provenance.model,
        provenance.generated_at,
        provenance.artifact_id,
    )


def _ordered_records(
    records: tuple[SummaryArtifactReference, ...],
) -> tuple[SummaryArtifactReference, ...]:
    return tuple(sorted(records, key=_record_order))


def empty_summary_catalog(team_slug: str) -> SummaryArtifactCatalog:
    """Create an empty canonical catalog for one team."""

    return SummaryArtifactCatalog(team_slug=team_slug, records=())


def load_summary_catalog(path: Path, team_slug: str) -> SummaryArtifactCatalog:
    """Load a catalog or return an empty catalog when the file is absent."""

    if not path.is_file():
        return empty_summary_catalog(team_slug)
    catalog = SummaryArtifactCatalog.from_json_obj(read_json(path), str(path))
    if catalog.team_slug != team_slug:
        raise ValueError(f"{path}: summary artifact catalog team mismatch")
    return catalog


def merge_summary_catalog(
    path: Path,
    team_slug: str,
    additions: tuple[SummaryArtifactReference, ...],
) -> tuple[SummaryArtifactCatalog, bool]:
    """Merge validated references by artifact ID and persist canonical JSON."""

    existing = load_summary_catalog(path, team_slug)
    by_id = {item.provenance.artifact_id: item for item in existing.records}
    for addition in additions:
        artifact_id = addition.provenance.artifact_id
        current = by_id.get(artifact_id)
        if current is not None and (
            current.cache_path != addition.cache_path
            or current.provenance.identity_json_obj()
            != addition.provenance.identity_json_obj()
        ):
            raise ValueError(f"artifact {artifact_id!r} has conflicting catalog identity")
        by_id[artifact_id] = addition
    catalog = SummaryArtifactCatalog(
        team_slug=team_slug,
        records=_ordered_records(tuple(by_id.values())),
    )
    changed = write_json_if_changed(path, catalog.to_json_obj())
    return catalog, changed


def select_summary_artifact(
    catalog: SummaryArtifactCatalog,
    logical_key: str,
    summarizer_id: str,
    *,
    minimum_output_schema: int = 1,
    preferred_model: str | None = None,
) -> SummaryArtifactReference | None:
    """Select the strongest compatible artifact for one logical key."""

    if minimum_output_schema < 1:
        raise ValueError("minimum output schema must be positive")
    candidates = [
        record
        for record in catalog.records
        if record.provenance.logical_key == logical_key
        and record.provenance.summarizer_id == summarizer_id
        and record.provenance.output_schema_version >= minimum_output_schema
    ]
    if preferred_model is not None:
        preferred = [
            record
            for record in candidates
            if record.provenance.model == preferred_model
        ]
        if preferred:
            candidates = preferred
    if not candidates:
        return None

    def quality(record: SummaryArtifactReference) -> tuple[object, ...]:
        provenance = record.provenance
        coverage = provenance.context_coverage.coverage_basis_points
        return (
            provenance.summarizer_version,
            provenance.output_schema_version,
            -1 if coverage is None else coverage,
            provenance.generated_at,
            provenance.artifact_id,
        )

    return max(candidates, key=quality)


__all__ = [
    "SUMMARY_CATALOG_SCHEMA_VERSION",
    "SummaryArtifactCatalog",
    "SummaryArtifactReference",
    "empty_summary_catalog",
    "load_summary_catalog",
    "merge_summary_catalog",
    "select_summary_artifact",
]
