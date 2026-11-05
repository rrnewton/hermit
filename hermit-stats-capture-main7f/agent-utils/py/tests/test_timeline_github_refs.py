"""Tests for conservative GitHub pull-request reference recognition."""

from __future__ import annotations

import pytest

from wrkviz.github_refs import (
    GitHubRepository,
    PullRequestReferenceKind,
    find_pull_request_references,
)


def test_explicit_pull_url_produces_canonical_link_metadata() -> None:
    text = "Merged https://github.com/sched-ext/scx/pull/3668 after review."

    (reference,) = find_pull_request_references(text)

    assert reference.text == "https://github.com/sched-ext/scx/pull/3668"
    assert text[reference.start : reference.end] == reference.text
    assert reference.kind is PullRequestReferenceKind.EXPLICIT_URL
    assert reference.link.repository.slug == "sched-ext/scx"
    assert reference.link.number == 3668
    assert reference.link.url == "https://github.com/sched-ext/scx/pull/3668"


def test_explicit_pull_url_uses_base_pr_url_before_files_suffix() -> None:
    text = "Inspect https://github.com/example-org/dev-widget/pull/42/files."

    (reference,) = find_pull_request_references(text)

    assert reference.text.endswith("/pull/42")
    assert text[reference.end :] == "/files."


def test_qualified_owner_repository_reference_needs_no_context() -> None:
    text = "Follow up in example-org/dev-widget#1087, then report back."

    (reference,) = find_pull_request_references(text)

    assert reference.kind is PullRequestReferenceKind.QUALIFIED
    assert reference.text == "example-org/dev-widget#1087"
    assert reference.link.repository == GitHubRepository("example-org", "dev-widget")
    assert reference.link.url == "https://github.com/example-org/dev-widget/pull/1087"


def test_pr_number_uses_explicit_string_repository_context() -> None:
    text = "PR #38 fixes the scheduling regression."

    (reference,) = find_pull_request_references(text, "example-org/dev-widget")

    assert reference.kind is PullRequestReferenceKind.REPOSITORY_CONTEXT
    assert reference.text == "PR #38"
    assert reference.link.repository.slug == "example-org/dev-widget"
    assert reference.link.number == 38


def test_pr_number_uses_validated_repository_object_context() -> None:
    repository = GitHubRepository(owner="sched-ext", name="scx")

    (reference,) = find_pull_request_references("Reviewed pr #3668.", repository)

    assert reference.link.repository is repository
    assert reference.link.url == "https://github.com/sched-ext/scx/pull/3668"


def test_pr_number_without_repository_context_remains_plain_text() -> None:
    assert find_pull_request_references("PR #38 may refer to several repositories.") == ()


def test_naked_number_remains_plain_text_even_with_repository_context() -> None:
    text = "Compare #38 with issue #7 and milestone #4."

    assert find_pull_request_references(text, "example-org/dev-widget") == ()


def test_unrelated_urls_and_github_issues_are_not_pull_requests() -> None:
    text = (
        "See https://example.com/owner/repo/pull/3 and "
        "https://github.com/owner/repo/issues/3."
    )

    assert find_pull_request_references(text) == ()


def test_multiple_forms_are_returned_in_source_order() -> None:
    text = (
        "PR #9 followed owner/second#10 and "
        "https://github.com/owner/third/pull/11."
    )

    references = find_pull_request_references(text, "owner/first")

    assert [reference.link.repository.slug for reference in references] == [
        "owner/first",
        "owner/second",
        "owner/third",
    ]
    assert [reference.link.number for reference in references] == [9, 10, 11]


@pytest.mark.parametrize(
    "context",
    [
        "owner",
        "owner/repository/extra",
        "https://github.com/owner/repository",
        "-owner/repository",
        "owner/",
    ],
)
def test_invalid_repository_context_is_rejected(context: str) -> None:
    with pytest.raises(ValueError, match="GitHub repository"):
        find_pull_request_references("PR #1", context)


@pytest.mark.parametrize(
    "text",
    [
        "owner/repository#0",
        "https://github.com/owner/repository/pull/0",
        "PR #0",
        "path/owner/repository#3",
    ],
)
def test_malformed_or_embedded_references_are_not_linked(text: str) -> None:
    assert find_pull_request_references(text, "owner/context") == ()
