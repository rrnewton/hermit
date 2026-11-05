"""Tests for GitHubHost's two-phase collection: light list + parallel evidence enrichment.

The heavy ``statusCheckRollup`` field makes a single ``gh pr list`` 504 on a large open set (measured
at 60 PRs). GitHubHost therefore fetches the light metadata in one list call and enriches each PR's
checks and review evidence with bounded per-PR calls, degrading one failed enrichment to "no
checks/no authority" rather than aborting the whole plan. These tests pin that fail-closed seam.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Callable, Sequence

import pytest

from pr_landing_planner import githubhost
from pr_landing_planner.githubhost import GitHubHost, HostCommandError
from pr_landing_planner.landing_context import review_evidence_digest

_FakeRun = Callable[
    [Sequence[str], "str | None", tuple[int, ...]], subprocess.CompletedProcess[str]
]


def _completed(stdout: str) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(args=[], returncode=0, stdout=stdout, stderr="")


def _fake_run_factory(
    *,
    fail_view_for: frozenset[int] = frozenset(),
    missing_identity_for: frozenset[int] = frozenset(),
    missing_inline_identity_for: frozenset[int] = frozenset(),
    retired_inline_for: frozenset[int] = frozenset(),
    two_page_for: frozenset[int] = frozenset(),
    truncate_reviews_for: frozenset[int] = frozenset(),
    retirement_for: frozenset[int] = frozenset(),
    retirement_actor: str = "release-authority",
    retirement_role: str = "observer",
    retirement_event_author: str | None = "release-authority",
    permission: str = "write",
    permission_login: str = "release-authority",
    fail_permission: bool = False,
) -> _FakeRun:
    """Build a fake ``_run`` that answers the light list and each per-PR ``gh pr view``."""

    def fake_run(
        cmd: Sequence[str], cwd: str | None, allowed: tuple[int, ...] = (0,)
    ) -> subprocess.CompletedProcess[str]:
        parts = list(cmd)
        if "list" in parts:
            fields = ",".join(parts)
            assert "statusCheckRollup" not in fields, "light list must omit rollup"
            assert "reviews" not in fields and "comments" not in fields
            return _completed(
                json.dumps(
                    [
                        {"number": 1, "headRefName": "a", "baseRefName": "main"},
                        {"number": 2, "headRefName": "b", "baseRefName": "main"},
                    ]
                )
            )
        if "view" in parts:
            number = int(parts[parts.index("view") + 1])
            if number in fail_view_for:
                raise HostCommandError(cmd, 1, "boom")
            rollup = [{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED",
                       "conclusion": "SUCCESS"}]
            return _completed(
                json.dumps(
                    {
                        "number": number,
                        "headRefOid": "a" * 40,
                        "reviewDecision": "CHANGES_REQUESTED",
                        "statusCheckRollup": rollup,
                    }
                )
            )
        if "graphql" in parts:
            assert "--paginate" in parts and "--slurp" in parts
            number = int(next(part.split("=", 1)[1] for part in parts if part.startswith("number=")))
            query = next(part.split("=", 1)[1] for part in parts if part.startswith("query="))
            is_reviews = "reviews(first:" in query
            connection = "reviews" if is_reviews else "comments"
            if is_reviews:
                assert "submittedAt" in query
                assert "updatedAt" in query
                assert "lastEditedAt" in query
                node: dict[str, object] = {
                    "id": "" if number in missing_identity_for else f"review-{number}",
                    "author": {"login": "reviewer"},
                    "state": "CHANGES_REQUESTED",
                    "commit": {"oid": "a" * 40},
                    "submittedAt": "2026-09-04T12:00:00Z",
                    "updatedAt": "2026-09-04T12:00:00Z",
                    "lastEditedAt": None,
                    "body": "review body",
                }
            else:
                assert "createdAt" in query and "updatedAt" in query
                body = "resolution"
                if number in retirement_for:
                    body = (
                        f"[team, {retirement_actor}, session, model, role={retirement_role}]\n"
                        f"CHANGES-REQUESTED-WITHDRAWN-AT: codex {'a' * 40} "
                        f"BY {retirement_actor}\n"
                        "RETIRES 123456"
                    )
                event_author = (
                    retirement_event_author
                    if number in retirement_for
                    else "release-authority"
                )
                node = {
                    "id": f"comment-{number}",
                    "author": (
                        {"login": event_author} if event_author is not None else None
                    ),
                    "body": body,
                    "createdAt": "2026-09-04T12:00:00Z",
                    "updatedAt": "2026-09-04T12:00:00Z",
                    "isMinimized": False,
                    "minimizedReason": None,
                }
            pages = [
                {
                    "data": {"repository": {"pullRequest": {
                        "headRefOid": "a" * 40,
                        connection: {
                            "nodes": [node],
                            "pageInfo": {
                                "hasNextPage": number in two_page_for,
                                "endCursor": "cursor-1" if number in two_page_for else None,
                            },
                        },
                    }}}
                }
            ]
            if number in two_page_for and not (
                is_reviews and number in truncate_reviews_for
            ):
                second = dict(node)
                second["id"] = f"{node['id']}-page-2"
                pages.append(
                    {
                        "data": {"repository": {"pullRequest": {
                            "headRefOid": "a" * 40,
                            connection: {
                                "nodes": [second],
                                "pageInfo": {
                                    "hasNextPage": False,
                                    "endCursor": None,
                                },
                            },
                        }}}
                    }
                )
            return _completed(json.dumps(pages))
        if "api" in parts and "/collaborators/" in parts[-1]:
            assert "--paginate" not in parts and "--slurp" not in parts
            assert retirement_event_author is not None
            assert parts[-1].endswith(
                f"/collaborators/{retirement_event_author}/permission"
            )
            if fail_permission:
                raise HostCommandError(cmd, 1, "permission unavailable")
            return _completed(
                json.dumps(
                    {
                        "permission": permission,
                        "role_name": permission,
                        "user": {"login": permission_login},
                    }
                )
            )
        if "api" in parts:
            assert "--paginate" in parts and "--slurp" in parts
            endpoint = parts[-1]
            number = int(endpoint.split("/pulls/", 1)[1].split("/", 1)[0])
            position = None if number in retired_inline_for else 7
            identity: int | None = (
                None if number in missing_inline_identity_for else 1000 + number
            )
            return _completed(
                json.dumps(
                    [[
                        {
                            "id": identity,
                            "user": {"login": "reviewer"},
                            "pull_request_review_id": 500 + number,
                            "in_reply_to_id": None,
                            "body": "inline objection",
                            "path": "src/lib.rs",
                            "position": position,
                            "original_position": 7,
                            "line": position,
                            "original_line": 7,
                            "original_start_line": None,
                            "side": "RIGHT",
                            "start_line": None,
                            "start_side": None,
                            "subject_type": "line",
                            "commit_id": None,
                            "original_commit_id": "a" * 40,
                            "created_at": "2026-09-04T12:00:00Z",
                            "updated_at": "2026-09-04T12:00:00Z",
                        }
                    ]]
                )
            )
        raise AssertionError(f"unexpected command: {parts}")

    return fake_run


def test_list_open_prs_enriches_rollup_per_pr(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(githubhost, "_run", _fake_run_factory())
    prs = GitHubHost().list_open_prs("owner/repo", "main")
    assert [p.number for p in prs] == [1, 2]  # order follows the light list, not completion order
    assert all(len(p.checks) == 1 for p in prs)  # every PR got its rollup
    assert all(p.review_snapshot is not None for p in prs)
    assert all(not p.review_evidence_unavailable for p in prs)
    snapshot = prs[0].review_snapshot
    assert snapshot is not None
    assert {event.kind for event in snapshot.events} == {
        "review",
        "issue-comment",
        "review-comment",
    }
    assert all(event.head_sha == "" for event in snapshot.events if event.kind != "review")
@pytest.mark.parametrize(
    ("stdout", "message"),
    (
        ("", "empty stdout"),
        (json.dumps([{"number": 1}, "not-an-object"]), "entry 1 is not an object"),
        (
            json.dumps([{"number": number} for number in range(500)]),
            "500-item limit",
        ),
    ),
)
def test_live_list_refuses_when_pr_availability_is_unknown(
    monkeypatch: pytest.MonkeyPatch, stdout: str, message: str
) -> None:
    def list_only(
        cmd: Sequence[str], cwd: str | None, allowed: tuple[int, ...] = (0,)
    ) -> subprocess.CompletedProcess[str]:
        assert "list" in cmd
        return _completed(stdout)

    monkeypatch.setattr(githubhost, "_run", list_only)
    with pytest.raises(ValueError, match=message):
        GitHubHost().list_open_prs("owner/repo", "main")


def test_live_list_accepts_an_explicit_empty_array(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(githubhost, "_run", lambda *_args, **_kwargs: _completed("[]"))
    assert GitHubHost().list_open_prs("owner/repo", "main") == ()




def test_native_reviews_and_issue_comments_include_every_paginated_page(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        githubhost, "_run", _fake_run_factory(two_page_for=frozenset({1}))
    )
    snapshot = GitHubHost().list_open_prs("owner/repo", "main")[0].review_snapshot
    assert snapshot is not None
    assert [event.kind for event in snapshot.events].count("review") == 2
    assert [event.kind for event in snapshot.events].count("issue-comment") == 2


def test_graphql_partial_data_with_errors_fails_closed() -> None:
    raw = [
        {
            "errors": [{"message": "review evidence is incomplete"}],
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": "a" * 40,
                        "reviews": {
                            "nodes": [],
                            "pageInfo": {"hasNextPage": False, "endCursor": None},
                        },
                    }
                }
            },
        }
    ]
    with pytest.raises(ValueError, match="contains GraphQL errors"):
        githubhost._graphql_connection_from_slurp(
            raw, number=1, connection="reviews"
        )


def test_incomplete_native_review_pagination_fails_closed(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(
        githubhost,
        "_run",
        _fake_run_factory(
            two_page_for=frozenset({2}), truncate_reviews_for=frozenset({2})
        ),
    )
    prs = GitHubHost().list_open_prs("owner/repo", "main")
    by = {pr.number: pr for pr in prs}
    assert by[1].review_snapshot is not None
    assert by[2].review_snapshot is None
    assert by[2].review_evidence_unavailable
    assert by[2].checks == ()
    assert "#2" in capsys.readouterr().err


def test_list_open_prs_degrades_failed_rollup(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(githubhost, "_run", _fake_run_factory(fail_view_for=frozenset({2})))
    prs = GitHubHost().list_open_prs("owner/repo", "main")
    by = {p.number: p for p in prs}
    assert len(by[1].checks) == 1  # healthy PR still enriched
    assert by[2].checks == ()  # failed rollup degrades to no checks, PR is NOT dropped
    assert by[2].review_evidence_unavailable
    err = capsys.readouterr().err
    assert "#2" in err and "evidence enrichment failed" in err  # LOUD note, not silent


def test_missing_stable_review_identity_fails_closed(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(
        githubhost, "_run", _fake_run_factory(missing_identity_for=frozenset({2}))
    )
    prs = GitHubHost().list_open_prs("owner/repo", "main")
    by = {pr.number: pr for pr in prs}
    assert by[1].review_snapshot is not None
    assert by[2].review_snapshot is None
    assert by[2].review_evidence_unavailable
    assert by[2].checks == ()
    assert "#2" in capsys.readouterr().err


def test_missing_stable_inline_identity_fails_closed(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(
        githubhost,
        "_run",
        _fake_run_factory(missing_inline_identity_for=frozenset({2})),
    )
    prs = GitHubHost().list_open_prs("owner/repo", "main")
    by = {pr.number: pr for pr in prs}
    assert by[1].review_snapshot is not None
    assert by[2].review_snapshot is None
    assert by[2].review_evidence_unavailable
    assert by[2].checks == ()
    assert "#2" in capsys.readouterr().err


def test_same_timestamp_inline_retirement_changes_production_digest(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(githubhost, "_run", _fake_run_factory())
    active = GitHubHost().list_open_prs("owner/repo", "main")[0].review_snapshot
    assert active is not None

    monkeypatch.setattr(
        githubhost,
        "_run",
        _fake_run_factory(retired_inline_for=frozenset({1})),
    )
    retired = GitHubHost().list_open_prs("owner/repo", "main")[0].review_snapshot
    assert retired is not None

    assert {event.updated_at for event in active.events} == {
        "2026-09-04T12:00:00Z"
    }
    assert {event.updated_at for event in retired.events} == {
        "2026-09-04T12:00:00Z"
    }
    assert review_evidence_digest(active) != review_evidence_digest(retired)


@pytest.mark.parametrize("permission", ["triage", "write", "maintain", "admin"])
def test_exact_retirement_binds_current_actor_permission(
    monkeypatch: pytest.MonkeyPatch, permission: str
) -> None:
    monkeypatch.setattr(
        githubhost,
        "_run",
        _fake_run_factory(retirement_for=frozenset({1}), permission=permission),
    )
    snapshot = GitHubHost().list_open_prs("owner/repo", "main")[0].review_snapshot
    assert snapshot is not None
    retirement = next(event for event in snapshot.events if "RETIRES" in event.body)
    assert retirement.author == "release-authority"
    assert retirement.retirement_actor_permission == permission


def test_retirement_permission_change_changes_production_digest(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        githubhost,
        "_run",
        _fake_run_factory(retirement_for=frozenset({1}), permission="write"),
    )
    write = GitHubHost().list_open_prs("owner/repo", "main")[0].review_snapshot
    assert write is not None
    monkeypatch.setattr(
        githubhost,
        "_run",
        _fake_run_factory(retirement_for=frozenset({1}), permission="maintain"),
    )
    maintain = GitHubHost().list_open_prs("owner/repo", "main")[0].review_snapshot
    assert maintain is not None
    assert review_evidence_digest(write) != review_evidence_digest(maintain)


def test_repository_triage_role_is_authority_despite_read_base_permission() -> None:
    assert githubhost._repository_permission(
        {
            "permission": "read",
            "role_name": "triage",
            "user": {"login": "release-authority"},
        },
        "release-authority",
    ) == "triage"


@pytest.mark.parametrize(
    (
        "permission",
        "permission_login",
        "fail_permission",
        "retirement_actor",
        "retirement_event_author",
        "expected_permission_calls",
        "expected_permission",
        "expected_note",
    ),
    [
        ("", "release-authority", False, "release-authority", "release-authority", 1, "", True),
        ("read", "release-authority", False, "release-authority", "release-authority", 1, "", True),
        ("write", "different-actor", False, "release-authority", "release-authority", 1, "", True),
        ("write", "release-authority", True, "release-authority", "release-authority", 1, "", True),
        ("write", "release-authority", False, "different-actor", "release-authority", 1, "write", False),
        ("write", "devbig014", False, "departed", "devbig014", 1, "write", False),
        ("write", "release-authority", False, "departed", None, 0, "", False),
    ],
)
def test_unavailable_retirement_permission_keeps_snapshot_without_authority(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    permission: str,
    permission_login: str,
    fail_permission: bool,
    retirement_actor: str,
    retirement_event_author: str | None,
    expected_permission_calls: int,
    expected_permission: str,
    expected_note: bool,
) -> None:
    permission_calls = 0
    fake_run = _fake_run_factory(
        retirement_for=frozenset({2}),
        permission=permission,
        permission_login=permission_login,
        fail_permission=fail_permission,
        retirement_actor=retirement_actor,
        retirement_event_author=retirement_event_author,
        retirement_role="reviewer",
    )

    def counting_run(
        cmd: Sequence[str], cwd: str | None, allowed: tuple[int, ...] = (0,)
    ) -> subprocess.CompletedProcess[str]:
        nonlocal permission_calls
        if "/collaborators/" in cmd[-1]:
            permission_calls += 1
        return fake_run(cmd, cwd, allowed)

    monkeypatch.setattr(githubhost, "_run", counting_run)
    prs = GitHubHost().list_open_prs("owner/repo", "main")
    by_number = {pr.number: pr for pr in prs}
    assert by_number[1].review_snapshot is not None
    snapshot = by_number[2].review_snapshot
    assert snapshot is not None
    assert len(snapshot.events) == 3
    retirement = next(event for event in snapshot.events if "RETIRES" in event.body)
    assert retirement.author == (retirement_event_author or "")
    assert retirement.retirement_actor_permission == expected_permission
    assert by_number[2].checks
    assert permission_calls == expected_permission_calls
    has_note = "retirement permission unavailable" in capsys.readouterr().err
    assert has_note is expected_note


def test_production_snapshot_rejects_missing_authority_fields() -> None:
    head = "a" * 40
    view = {"headRefOid": head, "reviewDecision": "CHANGES_REQUESTED"}
    review = {
        "id": "review-1",
        "author": {"login": "reviewer"},
        "state": "CHANGES_REQUESTED",
        "commit": {"oid": head},
        "submittedAt": "2026-09-04T12:00:00Z",
        "updatedAt": "2026-09-04T12:00:01Z",
        "lastEditedAt": None,
        "body": "review body",
    }
    issue = {
        "id": "comment-1",
        "author": {"login": "release-authority"},
        "body": "resolution",
        "createdAt": "2026-09-04T12:00:00Z",
        "updatedAt": "2026-09-04T12:00:01Z",
        "isMinimized": False,
        "minimizedReason": None,
    }
    inline = {
        "id": 1001,
        "user": {"login": "reviewer"},
        "pull_request_review_id": 501,
        "in_reply_to_id": None,
        "body": "inline objection",
        "path": "src/lib.rs",
        "position": 7,
        "original_position": 7,
        "line": 7,
        "original_line": 7,
        "original_start_line": None,
        "side": "RIGHT",
        "start_line": None,
        "start_side": None,
        "subject_type": "line",
        "commit_id": None,
        "original_commit_id": head,
        "created_at": "2026-09-04T12:00:00Z",
        "updated_at": "2026-09-04T12:00:01Z",
    }
    missing_decision = dict(view)
    missing_decision.pop("reviewDecision")
    with pytest.raises(ValueError, match="reviewDecision"):
        githubhost._review_snapshot(missing_decision, [review], [issue], [inline])

    alien_review = dict(review)
    alien_review["state"] = "ALIEN_STATE"
    with pytest.raises(ValueError, match="unknown state"):
        githubhost._review_snapshot(view, [alien_review], [issue], [inline])
    for source, key in ((review, "updatedAt"), (issue, "createdAt"), (inline, "created_at")):
        malformed = dict(source)
        malformed.pop(key)
        with pytest.raises(ValueError, match="lacks"):
            githubhost._review_snapshot(
                view,
                [malformed] if source is review else [review],
                [malformed] if source is issue else [issue],
                [malformed] if source is inline else [inline],
            )

    anonymous_review = dict(review)
    anonymous_review["author"] = None
    snapshot = githubhost._review_snapshot(
        view, [anonymous_review], [issue], [inline]
    )
    assert snapshot.events[0].author == ""
    assert review_evidence_digest(snapshot)
