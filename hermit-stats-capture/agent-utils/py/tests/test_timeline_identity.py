from __future__ import annotations

from wrkviz.archive import narrow_json
from wrkviz.identity import (
    HostIdentity,
    ProjectIdentity,
    SiteIdentity,
    canonical_repository_url,
    infer_structured_identity,
    merge_site_identity,
    parse_identity_overrides,
    site_identity_from_json_obj,
)


def test_repository_remotes_become_safe_browser_urls() -> None:
    assert canonical_repository_url("git@github.com:example-org/dev-widget.git") == (
        "https://github.com/example-org/dev-widget"
    )
    assert canonical_repository_url(
        "https://GitHub.com/example-org/agent-utils.git?token=secret#fragment"
    ) == "https://github.com/example-org/agent-utils"


def test_structured_codex_metadata_infers_projects_without_prompt_scanning() -> None:
    projects, hosts = infer_structured_identity(
        (
            {
                "cwd": "/home/newton/work/dev-widget",
                "git": {
                    "repository_url": "https://github.com/example-org/dev-widget.git"
                },
            },
            {
                "cwd": "/home/newton/work/agent-utils",
                "git": {
                    "repository_url": "git@github.com:example-org/agent-utils.git"
                },
                "hostname": "devbig014.example.com",
            },
        )
    )

    assert [(item.label, item.primary) for item in projects] == [
        ("dev-widget", True),
        ("agent-utils", False),
    ]
    assert [item.hostname for item in hosts] == ["devbig014.example.com"]
    assert all(item.source == "session_metadata" for item in projects)
    assert all(item.source == "session_metadata" for item in hosts)


def test_identity_merge_accumulates_distinct_projects_and_hosts() -> None:
    previous = SiteIdentity(
        "codex-widget",
        (
            ProjectIdentity(
                "dev-widget",
                "https://github.com/example-org/dev-widget",
                True,
                "session_metadata",
            ),
        ),
        (HostIdentity("devbig014.example.com", "explicit"),),
        "America/New_York",
        "explicit",
    )
    inferred = (
        ProjectIdentity(
            "agent-utils",
            "https://github.com/example-org/agent-utils",
            True,
            "session_metadata",
        ),
    )
    explicit, explicit_hosts = parse_identity_overrides(
        ("Widget=https://github.com/example-org/widget.git",),
        ("devbig015",),
    )

    merged = merge_site_identity(
        "codex-widget",
        "America/New_York",
        "explicit",
        inferred,
        (),
        explicit,
        explicit_hosts,
        previous,
    )

    assert [item.label for item in merged.projects] == [
        "dev-widget",
        "agent-utils",
        "Widget",
    ]
    assert [item.label for item in merged.projects if item.primary] == ["Widget"]
    assert [item.hostname for item in merged.hosts] == [
        "devbig014.example.com",
        "devbig015",
    ]


def test_site_identity_json_round_trip() -> None:
    identity = SiteIdentity(
        "codex-widget",
        (
            ProjectIdentity(
                "dev-widget",
                "https://github.com/example-org/dev-widget",
                True,
                "explicit",
            ),
        ),
        (HostIdentity("devbig014", "explicit"),),
        "America/New_York",
        "explicit",
    )

    parsed = site_identity_from_json_obj(
        narrow_json(identity.to_json_obj()), "site-identity.json"
    )

    assert parsed == identity


def test_default_timezone_rerun_preserves_explicit_provenance() -> None:
    previous = SiteIdentity(
        "codex-widget",
        (),
        (),
        "America/New_York",
        "explicit",
    )

    unchanged = merge_site_identity(
        "codex-widget",
        "America/New_York",
        "default",
        (),
        (),
        (),
        (),
        previous,
    )
    changed = merge_site_identity(
        "codex-widget",
        "UTC",
        "default",
        (),
        (),
        (),
        (),
        previous,
    )

    assert unchanged.display_timezone_source == "explicit"
    assert changed.display_timezone_source == "default"
