"""Conservative recognition of GitHub pull-request references.

Transcript text contains many numbers prefixed with ``#`` that may denote issues,
milestones, or locally meaningful identifiers.  This module therefore recognizes
only references that carry explicit pull-request evidence.  It does not perform
network access or mutate the supplied text; callers receive character spans and
canonical link metadata that they can render as appropriate.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
import re


_OWNER_TEXT = r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?"
_REPOSITORY_TEXT = r"[A-Za-z0-9_.-]{1,100}"
_OWNER = re.compile(rf"{_OWNER_TEXT}\Z")
_REPOSITORY = re.compile(rf"{_REPOSITORY_TEXT}\Z")

_EXPLICIT_URL = re.compile(
    rf"(?<![A-Za-z0-9])"
    rf"(?P<reference>https://github\.com/"
    rf"(?P<owner>{_OWNER_TEXT})/"
    rf"(?P<repository>{_REPOSITORY_TEXT})/pull/"
    rf"(?P<number>[1-9][0-9]*))"
    rf"(?=$|[/\s?#)\]}}>,.;:!])",
    re.IGNORECASE,
)
_QUALIFIED = re.compile(
    rf"(?<![A-Za-z0-9_./-])"
    rf"(?P<reference>"
    rf"(?P<owner>{_OWNER_TEXT})/"
    rf"(?P<repository>{_REPOSITORY_TEXT})#"
    rf"(?P<number>[1-9][0-9]*))"
    rf"(?![0-9])"
)
_CONTEXTUAL = re.compile(
    r"(?<![A-Za-z0-9_])(?P<reference>PR[ \t]+#(?P<number>[1-9][0-9]*))(?![0-9])",
    re.IGNORECASE,
)


@dataclass(frozen=True, slots=True)
class GitHubRepository:
    """A validated GitHub ``owner/repository`` identity."""

    owner: str
    name: str

    def __post_init__(self) -> None:
        if _OWNER.fullmatch(self.owner) is None:
            raise ValueError(f"invalid GitHub repository owner: {self.owner!r}")
        if _REPOSITORY.fullmatch(self.name) is None or self.name in {".", ".."}:
            raise ValueError(f"invalid GitHub repository name: {self.name!r}")

    @property
    def slug(self) -> str:
        """Return the conventional ``owner/repository`` spelling."""

        return f"{self.owner}/{self.name}"

    @classmethod
    def parse(cls, slug: str) -> GitHubRepository:
        """Parse an exact ``owner/repository`` slug.

        URLs and strings with extra path components are rejected so that a caller
        cannot accidentally supply an ambiguous context.
        """

        parts = slug.split("/")
        if len(parts) != 2:
            raise ValueError(
                f"GitHub repository context must be an owner/repository slug: {slug!r}"
            )
        return cls(owner=parts[0], name=parts[1])


@dataclass(frozen=True, slots=True)
class PullRequestLink:
    """Stable metadata needed to render a GitHub pull-request link."""

    repository: GitHubRepository
    number: int

    def __post_init__(self) -> None:
        if self.number <= 0:
            raise ValueError("a GitHub pull-request number must be positive")

    @property
    def url(self) -> str:
        """Return the canonical GitHub pull-request URL."""

        return f"https://github.com/{self.repository.slug}/pull/{self.number}"


class PullRequestReferenceKind(str, Enum):
    """The evidence that made a transcript reference safe to link."""

    EXPLICIT_URL = "explicit_url"
    QUALIFIED = "qualified"
    REPOSITORY_CONTEXT = "repository_context"


@dataclass(frozen=True, slots=True)
class PullRequestReference:
    """A recognized reference and its half-open character span in source text."""

    start: int
    end: int
    text: str
    kind: PullRequestReferenceKind
    link: PullRequestLink

    def __post_init__(self) -> None:
        if self.start < 0 or self.end <= self.start:
            raise ValueError("a pull-request reference must have a non-empty valid span")


RepositoryContext = GitHubRepository | str


def _repository_context(value: RepositoryContext | None) -> GitHubRepository | None:
    if value is None or isinstance(value, GitHubRepository):
        return value
    return GitHubRepository.parse(value)


def _reference(
    match: re.Match[str],
    repository: GitHubRepository,
    kind: PullRequestReferenceKind,
) -> PullRequestReference:
    reference = match.group("reference")
    return PullRequestReference(
        start=match.start("reference"),
        end=match.end("reference"),
        text=reference,
        kind=kind,
        link=PullRequestLink(
            repository=repository,
            number=int(match.group("number")),
        ),
    )


def find_pull_request_references(
    text: str,
    repository_context: RepositoryContext | None = None,
) -> tuple[PullRequestReference, ...]:
    """Find conservatively linkable pull-request references in ``text``.

    Recognized forms are explicit ``https://github.com/owner/repo/pull/N`` URLs,
    qualified ``owner/repo#N`` references, and ``PR #N`` only when
    ``repository_context`` is supplied.  A naked ``#N`` is deliberately never
    interpreted as a pull request, even when repository context is available.
    """

    context = _repository_context(repository_context)
    references: list[PullRequestReference] = []

    for match in _EXPLICIT_URL.finditer(text):
        references.append(
            _reference(
                match,
                GitHubRepository(
                    owner=match.group("owner"), name=match.group("repository")
                ),
                PullRequestReferenceKind.EXPLICIT_URL,
            )
        )

    for match in _QUALIFIED.finditer(text):
        references.append(
            _reference(
                match,
                GitHubRepository(
                    owner=match.group("owner"), name=match.group("repository")
                ),
                PullRequestReferenceKind.QUALIFIED,
            )
        )

    if context is not None:
        for match in _CONTEXTUAL.finditer(text):
            references.append(
                _reference(match, context, PullRequestReferenceKind.REPOSITORY_CONTEXT)
            )

    references.sort(key=lambda reference: (reference.start, reference.end))
    return tuple(references)


__all__ = [
    "GitHubRepository",
    "PullRequestLink",
    "PullRequestReference",
    "PullRequestReferenceKind",
    "RepositoryContext",
    "find_pull_request_references",
]
