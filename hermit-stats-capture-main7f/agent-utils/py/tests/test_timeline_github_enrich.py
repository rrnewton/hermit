"""Tests for archive-level GitHub pull metadata enrichment."""

from __future__ import annotations

from collections.abc import Mapping
import json
from pathlib import Path

import pytest

from wrkviz.github_enrich import (
    discover_pull_request_keys,
    enrich_pull_request_metadata,
    pull_metadata_path,
)
from wrkviz.github_metadata import (
    HttpResponse,
    load_pull_request_metadata_cache,
)


class MetadataTransport:
    """Return deterministic metadata or conditional 304 responses."""

    def __init__(self, *, fail_number: int | None = None) -> None:
        self.fail_number = fail_number
        self.calls: list[tuple[str, dict[str, str]]] = []

    def get(
        self, *, url: str, headers: Mapping[str, str], timeout_seconds: float
    ) -> HttpResponse:
        del timeout_seconds
        request_headers = dict(headers)
        self.calls.append((url, request_headers))
        number = int(url.rsplit("/", 1)[-1])
        if number == self.fail_number:
            return HttpResponse(status=500, headers={}, body=b"failure")
        if "If-None-Match" in request_headers:
            return HttpResponse(status=304, headers={}, body=b"")
        body: dict[str, object] = {
            "title": f"Pull request {number}",
            "state": "closed",
            "draft": False,
            "merged_at": "2026-08-05T10:00:00Z",
            "body": f"Details for pull request {number}.",
            "base": {"ref": "main"},
            "head": {"label": f"author:pr-{number}"},
            "user": {"login": "author"},
            "updated_at": "2026-08-05T10:00:00Z",
        }
        return HttpResponse(
            status=200,
            headers={"etag": f'W/"pull-{number}"'},
            body=json.dumps(body).encode("utf-8"),
        )


def _write_details(archive: Path) -> None:
    root = archive / "data" / "details"
    root.mkdir(parents=True)
    detail = {
        "work_summary": [
            {
                "text": "Reviewed https://github.com/owner/first/pull/7.",
                "pull_requests": [{"repository": "owner/first", "number": 7}],
            }
        ],
        "transcript": [
            {"text": "Compared owner/second#9 with naked #12."},
            {
                "text": "Repeated https://github.com/owner/first/pull/7.",
                "pull_requests": [],
            },
        ],
    }
    (root / "phase.json").write_text(json.dumps(detail), encoding="utf-8")


def test_discovery_deduplicates_keys_but_counts_evidenced_occurrences(
    tmp_path: Path,
) -> None:
    _write_details(tmp_path)

    references, keys = discover_pull_request_keys(tmp_path)

    assert references == 3
    assert [(key.repository, key.number) for key in keys] == [
        ("owner/first", 7),
        ("owner/second", 9),
    ]


def test_enrichment_caches_titles_and_second_conditional_run_is_byte_stable(
    tmp_path: Path,
) -> None:
    _write_details(tmp_path)
    first_transport = MetadataTransport()

    first = enrich_pull_request_metadata(
        tmp_path,
        "team",
        transport=first_transport,
        fetched_at="2026-08-05T11:00:00Z",
    )

    assert first.fetched == 2
    assert first.not_modified == 0
    assert first.failures == ()
    path = pull_metadata_path(tmp_path, "team")
    before = path.read_bytes()
    cache = load_pull_request_metadata_cache(path)
    assert [record.title for record in cache.records] == [
        "Pull request 7",
        "Pull request 9",
    ]

    second_transport = MetadataTransport()
    second = enrich_pull_request_metadata(
        tmp_path,
        "team",
        transport=second_transport,
        fetched_at="2026-08-05T12:00:00Z",
    )

    assert second.fetched == 0
    assert second.not_modified == 2
    assert second.failures == ()
    assert path.read_bytes() == before
    assert all("If-None-Match" in headers for _, headers in second_transport.calls)


def test_partial_failure_preserves_each_successful_record(tmp_path: Path) -> None:
    _write_details(tmp_path)

    report = enrich_pull_request_metadata(
        tmp_path,
        "team",
        transport=MetadataTransport(fail_number=9),
        fetched_at="2026-08-05T11:00:00Z",
    )

    assert report.fetched == 1
    assert len(report.failures) == 1
    assert "owner/second#9" in report.failures[0]
    cache = load_pull_request_metadata_cache(pull_metadata_path(tmp_path, "team"))
    assert [record.key.number for record in cache.records] == [7]


@pytest.mark.parametrize(
    "team_slug", ["../outside", "team/other", "Uppercase", "double--hyphen"]
)
def test_metadata_cache_path_rejects_unsafe_team_slugs(
    tmp_path: Path, team_slug: str
) -> None:
    with pytest.raises(ValueError, match="team slug"):
        pull_metadata_path(tmp_path, team_slug)
