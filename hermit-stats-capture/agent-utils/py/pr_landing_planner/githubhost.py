"""The one real :class:`~pr_landing_planner.host.VcsHost`: GitHub via ``gh`` + a local ``git`` clone.

Light PR metadata comes from one ``gh pr list`` call. Checks and three explicitly paginated review
event sources (native reviews, issue comments, and inline review comments) are enriched per PR;
conflict / ancestry / freshness operations are plain ``git`` against a local clone. An optional
``--net-wrapper`` prefixes each ``gh`` and ``git fetch`` command, while ``--gh-cmd`` supports
authenticated wrappers. Gate-check names and flaky signatures live in classifier configuration,
not this host.
"""

from __future__ import annotations

import json
import shlex
import subprocess
import sys
from collections.abc import Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace

from pr_landing_planner.classify import parse_rollup
from pr_landing_planner.landing_context import (
    ALLOWED_RETIREMENT_PERMISSIONS,
    retirement_record,
)
from pr_landing_planner.model import (
    CheckRun,
    NATIVE_REVIEW_STATES,
    RawPr,
    ReviewEvidenceEvent,
    ReviewEvidenceSnapshot,
)

GH_FIELDS: tuple[str, ...] = (
    "number",
    "title",
    "author",
    "baseRefName",
    "headRefName",
    "headRefOid",
    "isDraft",
    "mergeable",
    "reviewDecision",
    "createdAt",
    "updatedAt",
    "additions",
    "deletions",
    "labels",
    "statusCheckRollup",
    "reviews",
    "comments",
)

#: The heavy ``statusCheckRollup`` field makes a single ``gh pr list`` over a large open set 504 at
#: the GraphQL layer (measured: fails at 60 PRs on <org>/<repo>). So the light metadata is fetched
#: in one cheap list call (:data:`LIGHT_FIELDS`) and the rollup is enriched per PR, in parallel,
#: below. Each per-PR query is small and bounded; an enrichment that still fails degrades that one
#: PR to "no checks" (classified pending) with a LOUD stderr NOTE rather than aborting the whole plan.
_ENRICHMENT_FIELDS = frozenset(("statusCheckRollup", "reviews", "comments"))
LIGHT_FIELDS: tuple[str, ...] = tuple(f for f in GH_FIELDS if f not in _ENRICHMENT_FIELDS)

#: Concurrency for per-PR checks/review enrichment. Bounded so we never fan out hundreds of ``gh``
#: processes; the work is network-bound so a small pool already hides most latency.
_ENRICHMENT_WORKERS = 8
_PR_LIST_LIMIT = 500

_REVIEWS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      reviews(first: 100, after: $endCursor) {
        nodes { id author { login } state commit { oid } submittedAt updatedAt lastEditedAt body }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
""".strip()

_ISSUE_COMMENTS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      comments(first: 100, after: $endCursor) {
        nodes { id author { login } body createdAt updatedAt isMinimized minimizedReason }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
""".strip()


@dataclass(frozen=True)
class _Enrichment:
    checks: tuple[CheckRun, ...]
    review_snapshot: ReviewEvidenceSnapshot


class HostCommandError(RuntimeError):
    """A ``gh`` / ``git`` command failed; carries the command + captured stderr for debugging."""

    def __init__(self, cmd: Sequence[str], returncode: int, stderr: str) -> None:
        super().__init__(f"command failed ({returncode}): {shlex.join(cmd)}\n{stderr.strip()}")
        self.returncode = returncode

def _light_pr_entries(stdout: str) -> tuple[dict[str, object], ...]:
    if not stdout.strip():
        raise ValueError(
            "open PR list returned empty stdout; review evidence is unavailable"
        )
    try:
        raw: object = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise ValueError(
            f"open PR list returned invalid JSON; review evidence is unavailable: {exc}"
        ) from exc
    if not isinstance(raw, list):
        raise ValueError(
            "open PR list response is not an array; review evidence is unavailable"
        )
    if len(raw) >= _PR_LIST_LIMIT:
        raise ValueError(
            f"open PR list reached the {_PR_LIST_LIMIT}-item limit; "
            "review evidence may be incomplete"
        )
    entries: list[dict[str, object]] = []
    for index, entry in enumerate(raw):
        if not isinstance(entry, dict):
            raise ValueError(
                f"open PR list entry {index} is not an object; review evidence is unavailable"
            )
        entries.append({str(key): value for key, value in entry.items()})
    return tuple(entries)



def _run(
    cmd: Sequence[str], cwd: str | None, allowed: tuple[int, ...] = (0,)
) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(list(cmd), cwd=cwd, capture_output=True, text=True)
    if proc.returncode not in allowed:
        raise HostCommandError(cmd, proc.returncode, proc.stderr)
    return proc


def _str(m: Mapping[str, object], key: str) -> str:
    val = m.get(key)
    if isinstance(val, str):
        return val
    if isinstance(val, (int, float, bool)):
        return str(val)
    return ""


def _int(m: Mapping[str, object], key: str) -> int:
    val = m.get(key)
    return val if isinstance(val, int) and not isinstance(val, bool) else 0


def _bool(m: Mapping[str, object], key: str) -> bool:
    return bool(m.get(key) is True)


def _author_login(value: object) -> str:
    if isinstance(value, dict):
        login = value.get("login")
        if isinstance(login, str):
            return login
    return "unknown"


def _labels(value: object) -> tuple[str, ...]:
    if not isinstance(value, list):
        return ()
    out: list[str] = []
    for entry in value:
        if isinstance(entry, dict):
            name = entry.get("name")
            if isinstance(name, str):
                out.append(name)
    return tuple(out)


def _event_object(value: object, role: str) -> Mapping[str, object]:
    if not isinstance(value, dict):
        raise ValueError(f"{role} is not an object")
    return {str(key): item for key, item in value.items()}


def _stable_identity(event: Mapping[str, object], role: str) -> str:
    identity = event.get("id")
    if not isinstance(identity, str) or not identity:
        raise ValueError(f"{role} lacks a stable id")
    return identity


def _body(event: Mapping[str, object], role: str) -> str:
    body = event.get("body")
    if not isinstance(body, str):
        raise ValueError(f"{role} lacks a string body")
    return body


def _required_string(event: Mapping[str, object], role: str, key: str) -> str:
    value = event.get(key)
    if not isinstance(value, str) or not value:
        raise ValueError(f"{role} lacks a non-empty string {key}")
    return value


def _event_author(event: Mapping[str, object], role: str, key: str) -> str:
    value = event.get(key)
    if value is None:
        return ""
    if not isinstance(value, dict):
        raise ValueError(f"{role} author is not an object or null")
    login = value.get("login")
    if login is None:
        return ""
    if not isinstance(login, str):
        raise ValueError(f"{role} author login is not a string or null")
    return login.strip()


_INLINE_STATE_FIELDS = (
    "in_reply_to_id",
    "line",
    "original_commit_id",
    "original_line",
    "original_position",
    "original_start_line",
    "path",
    "position",
    "pull_request_review_id",
    "side",
    "start_line",
    "start_side",
    "subject_type",
)


def _numeric_identity(event: Mapping[str, object], role: str) -> str:
    identity = event.get("id")
    if not isinstance(identity, int) or isinstance(identity, bool) or identity <= 0:
        raise ValueError(f"{role} lacks a stable positive numeric id")
    return str(identity)


def _required_nullable_string(
    event: Mapping[str, object], role: str, key: str
) -> str:
    if key not in event:
        raise ValueError(f"{role} lacks promised field {key}")
    value = event[key]
    if value is None:
        return ""
    if not isinstance(value, str) or not value:
        raise ValueError(f"{role} field {key} is not a non-empty string or null")
    return value


def _optional_head(event: Mapping[str, object], role: str) -> str:
    if "commit_id" not in event:
        raise ValueError(f"{role} lacks promised commit_id")
    value = event["commit_id"]
    if value is None:
        return ""
    if not isinstance(value, str):
        raise ValueError(f"{role} commit_id is not a string or null")
    return value


def _inline_state(event: Mapping[str, object], role: str) -> str:
    state: dict[str, object] = {}
    for key in _INLINE_STATE_FIELDS:
        if key not in event:
            raise ValueError(f"{role} lacks promised field {key}")
        value = event[key]
        if isinstance(value, bool) or not isinstance(value, (str, int, type(None))):
            raise ValueError(f"{role} field {key} is not a string, integer, or null")
        state[key] = value
    if not isinstance(state["path"], str) or not state["path"]:
        raise ValueError(f"{role} lacks a non-empty path")
    return json.dumps(state, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _graphql_connection_from_slurp(
    raw: object, *, number: int, connection: str
) -> tuple[str, tuple[object, ...]]:
    if not isinstance(raw, list) or not raw:
        raise ValueError(f"{connection} pagination for PR #{number} is not a non-empty array")
    head = ""
    flattened: list[object] = []
    for page_index, page_value in enumerate(raw):
        page = _event_object(page_value, f"{connection} page[{page_index}]")
        if "errors" in page:
            errors = page["errors"]
            if not isinstance(errors, list):
                raise ValueError(
                    f"{connection} page {page_index} GraphQL errors is not an array"
                )
            if errors:
                raise ValueError(
                    f"{connection} page {page_index} contains GraphQL errors"
                )
        data = _event_object(page.get("data"), f"{connection} page[{page_index}].data")
        repository = _event_object(
            data.get("repository"), f"{connection} page[{page_index}].repository"
        )
        pull_request = _event_object(
            repository.get("pullRequest"),
            f"{connection} page[{page_index}].pullRequest",
        )
        page_head = _required_string(
            pull_request, f"{connection} page {page_index}", "headRefOid"
        )
        if head and page_head != head:
            raise ValueError(
                f"{connection} PR head changed during pagination: {head} -> {page_head}"
            )
        head = page_head
        value = _event_object(
            pull_request.get(connection), f"{connection} page[{page_index}].{connection}"
        )
        nodes = value.get("nodes")
        page_info = value.get("pageInfo")
        if not isinstance(nodes, list) or not isinstance(page_info, dict):
            raise ValueError(f"{connection} page {page_index} lacks nodes or pageInfo")
        has_next = page_info.get("hasNextPage")
        end_cursor = page_info.get("endCursor")
        if not isinstance(has_next, bool):
            raise ValueError(f"{connection} page {page_index} lacks hasNextPage")
        if has_next and (not isinstance(end_cursor, str) or not end_cursor):
            raise ValueError(f"{connection} page {page_index} lacks a next-page cursor")
        if page_index + 1 < len(raw) and not has_next:
            raise ValueError(f"{connection} pagination continued after its terminal page")
        if page_index + 1 == len(raw) and has_next:
            raise ValueError(f"{connection} pagination ended before its terminal page")
        flattened.extend(nodes)
    return head, tuple(flattened)


def _review_snapshot(
    obj: Mapping[str, object],
    reviews: Sequence[object],
    issue_comments: Sequence[object],
    inline_comments: Sequence[object],
) -> ReviewEvidenceSnapshot:
    head = _required_string(obj, "review snapshot", "headRefOid")
    if "reviewDecision" not in obj:
        raise ValueError("review snapshot lacks promised reviewDecision")
    raw_decision = obj.get("reviewDecision")
    if raw_decision is not None and not isinstance(raw_decision, str):
        raise ValueError("review snapshot decision is not a string or null")
    decision = raw_decision if isinstance(raw_decision, str) else ""
    events: list[ReviewEvidenceEvent] = []
    for index, value in enumerate(reviews):
        role = f"review[{index}]"
        event = _event_object(value, role)
        state = _required_string(event, role, "state")
        if state not in NATIVE_REVIEW_STATES:
            raise ValueError(f"{role} has unknown state {state!r}")
        commit = event.get("commit")
        event_head = _required_string(
            _event_object(commit, f"{role}.commit"), f"{role}.commit", "oid"
        )
        events.append(
            ReviewEvidenceEvent(
                kind="review",
                identity=_stable_identity(event, role),
                author=_event_author(event, role, "author"),
                state=state,
                head_sha=event_head,
                created_at=_required_string(event, role, "submittedAt"),
                updated_at=_required_string(event, role, "updatedAt"),
                last_edited_at=_required_nullable_string(
                    event, role, "lastEditedAt"
                ),
                body=_body(event, role),
            )
        )
    for index, value in enumerate(issue_comments):
        role = f"issue-comment[{index}]"
        event = _event_object(value, role)
        minimized = event.get("isMinimized")
        if not isinstance(minimized, bool):
            raise ValueError(f"{role} isMinimized is not a boolean")
        if "minimizedReason" not in event:
            raise ValueError(f"{role} lacks promised minimizedReason")
        minimized_reason = event["minimizedReason"]
        if minimized_reason is not None and not isinstance(minimized_reason, str):
            raise ValueError(f"{role} minimizedReason is not a string or null")
        if minimized and not minimized_reason:
            raise ValueError(f"{role} is minimized without a reason")
        state = f"MINIMIZED:{minimized_reason}" if minimized else "ACTIVE"
        events.append(
            ReviewEvidenceEvent(
                kind="issue-comment",
                identity=_stable_identity(event, role),
                author=_event_author(event, role, "author"),
                state=state,
                # GitHub issue comments do not carry a commit identity. Empty is the
                # canonical absent value; the enclosing snapshot remains exact-head bound.
                head_sha="",
                created_at=_required_string(event, role, "createdAt"),
                updated_at=_required_string(event, role, "updatedAt"),
                last_edited_at="",
                body=_body(event, role),
            )
        )
    for index, value in enumerate(inline_comments):
        role = f"review-comment[{index}]"
        event = _event_object(value, role)
        events.append(
            ReviewEvidenceEvent(
                kind="review-comment",
                identity=_numeric_identity(event, role),
                author=_event_author(event, role, "user"),
                state=_inline_state(event, role),
                # REST review comments may omit commit_id. Preserve that absence rather
                # than attributing the comment to the enclosing snapshot head.
                head_sha=_optional_head(event, role),
                created_at=_required_string(event, role, "created_at"),
                updated_at=_required_string(event, role, "updated_at"),
                last_edited_at="",
                body=_body(event, role),
            )
        )
    return ReviewEvidenceSnapshot(head, decision, tuple(events))


def _repository_permission(raw: object, expected_author: str) -> str:
    response = _event_object(raw, "repository permission response")
    user = _event_object(response.get("user"), "repository permission response.user")
    actual_actor = _required_string(
        user, "repository permission response.user", "login"
    )
    if actual_actor.lower() != expected_author.lower():
        raise ValueError(
            "repository permission response actor differs from GitHub event author"
        )
    role = response.get("role_name")
    permission = response.get("permission")
    candidates = [
        value.lower()
        for value in (role, permission)
        if isinstance(value, str) and value
    ]
    for candidate in candidates:
        if candidate in ALLOWED_RETIREMENT_PERMISSIONS:
            return candidate
    raise ValueError(
        "GitHub event author lacks current triage-or-higher repository permission"
    )


class GitHubHost:
    """Talk to GitHub through ``gh`` and to a local clone through ``git``."""

    def __init__(
        self,
        *,
        git_dir: str = ".",
        remote: str = "origin",
        net_wrapper: Sequence[str] = (),
        gh_cmd: str = "gh",
    ) -> None:
        self._git_dir = git_dir
        self._remote = remote
        self._wrapper = tuple(net_wrapper)
        self._gh = gh_cmd

    def _net(self, cmd: Sequence[str]) -> list[str]:
        return [*self._wrapper, *cmd]

    def _retirement_permission(self, repo: str, author: str) -> str:
        proc = _run(
            self._net(
                [
                    self._gh,
                    "api",
                    f"repos/{repo}/collaborators/{author}/permission",
                ]
            ),
            cwd=None,
        )
        raw: object = json.loads(proc.stdout) if proc.stdout.strip() else None
        return _repository_permission(raw, author)

    def _bind_retirement_permissions(
        self, repo: str, snapshot: ReviewEvidenceSnapshot
    ) -> ReviewEvidenceSnapshot:
        authors: set[str] = set()
        for event in snapshot.events:
            retirement = retirement_record(event.body)
            if (
                retirement is None
                or retirement.head_sha != snapshot.head_sha
                or event.state != "ACTIVE"
                or not event.author
            ):
                continue
            authors.add(event.author)
        permissions: dict[str, str] = {}
        unavailable: list[str] = []
        for author in sorted(authors):
            try:
                permissions[author] = self._retirement_permission(repo, author)
            except (HostCommandError, ValueError):
                permissions[author] = ""
                unavailable.append(author)
        if unavailable:
            print(
                "pr-landing-planner: NOTE: retirement permission unavailable for "
                + ",".join(unavailable)
                + "; retaining the events without retirement authority",
                file=sys.stderr,
            )

        def permission(event: ReviewEvidenceEvent) -> str:
            retirement = retirement_record(event.body)
            if (
                retirement is None
                or retirement.head_sha != snapshot.head_sha
                or event.state != "ACTIVE"
            ):
                return ""
            return permissions.get(event.author, "")

        return replace(
            snapshot,
            events=tuple(
                replace(
                    event,
                    retirement_actor_permission=permission(event),
                )
                for event in snapshot.events
            ),
        )

    def list_open_prs(self, repo: str, base: str | None) -> tuple[RawPr, ...]:
        """List open pull requests and attach each request's latest check rollup."""

        # One cheap list for the light metadata (no rollup -> no GraphQL 504 on a large open set)...
        proc = _run(
            self._net(
                [
                    self._gh, "pr", "list",
                    "--repo", repo,
                    "--state", "open",
                    "--limit", str(_PR_LIST_LIMIT),
                    "--json", ",".join(LIGHT_FIELDS),
                ]
            ),
            cwd=None,
        )
        entries = _light_pr_entries(proc.stdout)
        # ...then enrich each PR's rollup with a small per-PR ``gh pr view``, in parallel.
        numbers = [_int(obj, "number") for obj in entries]
        enrichments = self._fetch_enrichments(repo, numbers)
        prs: list[RawPr] = []
        for obj in entries:
            number = _int(obj, "number")
            enrichment = enrichments.get(number)
            prs.append(
                RawPr(
                    number=number,
                    head_ref=_str(obj, "headRefName"),
                    base_ref=_str(obj, "baseRefName"),
                    api_head_sha=_str(obj, "headRefOid"),
                    title=_str(obj, "title"),
                    author=_author_login(obj.get("author")),
                    is_draft=_bool(obj, "isDraft"),
                    mergeable=_str(obj, "mergeable"),
                    # Preserve the independently observed light-list decision. Collection
                    # compares it with the later evidence snapshot instead of silently
                    # replacing a non-empty decision with missing or contradictory data.
                    review_decision=_str(obj, "reviewDecision"),
                    review_evidence_unavailable=enrichment is None,
                    created_at=_str(obj, "createdAt"),
                    updated_at=_str(obj, "updatedAt"),
                    additions=_int(obj, "additions"),
                    deletions=_int(obj, "deletions"),
                    labels=_labels(obj.get("labels")),
                    checks=enrichment.checks if enrichment is not None else (),
                    review_snapshot=(
                        enrichment.review_snapshot if enrichment is not None else None
                    ),
                )
            )
        return tuple(prs)

    def _fetch_enrichments(
        self, repo: str, numbers: Sequence[int]
    ) -> Mapping[int, _Enrichment]:
        """Fetch checks and exact review/comment evidence per PR, in parallel.

        A single per-PR failure degrades that PR to no checks (classified pending) with a LOUD stderr
        NOTE — No Silent Failure — instead of aborting the whole plan. The returned mapping is by PR
        number, so the caller's order is unaffected by completion order (result stays deterministic).
        """
        enrichments: dict[int, _Enrichment] = {}
        failed: list[int] = []

        def inline_comments(number: int) -> tuple[object, ...]:
            proc = _run(
                self._net(
                    [
                        self._gh,
                        "api",
                        "--paginate",
                        "--slurp",
                        f"repos/{repo}/pulls/{number}/comments?per_page=100",
                    ]
                ),
                cwd=None,
            )
            raw: object = json.loads(proc.stdout) if proc.stdout.strip() else []
            if not isinstance(raw, list) or not raw:
                raise ValueError(
                    "inline review-comment pagination is not a non-empty array"
                )
            flattened: list[object] = []
            for page_index, page in enumerate(raw):
                if not isinstance(page, list):
                    raise ValueError(
                        f"inline review-comment page {page_index} is not an array"
                    )
                flattened.extend(page)
            return tuple(flattened)

        def graphql_connection(
            number: int, connection: str, query: str
        ) -> tuple[str, tuple[object, ...]]:
            repo_parts = repo.split("/")
            if len(repo_parts) < 2 or not repo_parts[-2] or not repo_parts[-1]:
                raise ValueError(f"repository {repo!r} lacks owner/name")
            proc = _run(
                self._net(
                    [
                        self._gh,
                        "api",
                        "graphql",
                        "--paginate",
                        "--slurp",
                        "-f",
                        f"query={query}",
                        "-F",
                        f"owner={repo_parts[-2]}",
                        "-F",
                        f"name={repo_parts[-1]}",
                        "-F",
                        f"number={number}",
                    ]
                ),
                cwd=None,
            )
            raw: object = json.loads(proc.stdout) if proc.stdout.strip() else []
            return _graphql_connection_from_slurp(
                raw, number=number, connection=connection
            )

        def one(number: int) -> tuple[int, _Enrichment | None]:
            try:
                proc = _run(
                    self._net(
                        [
                            self._gh, "pr", "view", str(number),
                            "--repo", repo,
                            "--json",
                            "number,headRefOid,reviewDecision,statusCheckRollup",
                        ]
                    ),
                    cwd=None,
                )
                obj = json.loads(proc.stdout) if proc.stdout.strip() else {}
                if not isinstance(obj, dict):
                    raise ValueError("per-PR enrichment is not an object")
                review_head, reviews = graphql_connection(
                    number, "reviews", _REVIEWS_QUERY
                )
                comment_head, issue_comments = graphql_connection(
                    number, "comments", _ISSUE_COMMENTS_QUERY
                )
                view_head = _str(obj, "headRefOid")
                if not view_head or {view_head, review_head, comment_head} != {view_head}:
                    raise ValueError("PR head changed during review evidence enrichment")
                snapshot = _review_snapshot(
                    obj, reviews, issue_comments, inline_comments(number)
                )
                snapshot = self._bind_retirement_permissions(repo, snapshot)
                checks = parse_rollup(
                    obj.get("statusCheckRollup"), head_sha=snapshot.head_sha
                )
            except (HostCommandError, ValueError, json.JSONDecodeError):
                return number, None
            return number, _Enrichment(checks, snapshot)

        if numbers:
            with ThreadPoolExecutor(max_workers=_ENRICHMENT_WORKERS) as pool:
                for number, enrichment in pool.map(one, numbers):
                    if enrichment is None:
                        failed.append(number)
                    else:
                        enrichments[number] = enrichment
        if failed:
            listed = ",".join(f"#{n}" for n in sorted(failed))
            print(
                "pr-landing-planner: NOTE: check/review evidence enrichment failed for "
                f"{len(failed)} PR(s) ({listed}); treating checks as pending and "
                "review-resolution authority as unavailable",
                file=sys.stderr,
            )
        return enrichments

    def prefetch_refs(self, refspecs: Sequence[tuple[str, str]]) -> dict[str, str]:
        """Fetch all requested refs in one operation and return their object IDs by destination."""

        # ONE `git fetch` for every (source, dest) — a single remote round-trip instead of a per-PR
        # fan-out. Measured cost of the two shapes (2026-08-04, warm, 25 PR heads into a local
        # clone of a consuming repository): N separate `git fetch` = 21.5 s wall / 14.3 s sys
        # (≈0.86 s/PR, almost all process-spawn + round-trip overhead); one batched `git fetch` =
        # 0.85 s wall / 0.57 s sys — ~25× faster and O(1) in round-trips. This is why the planner's
        # default conflict detector is `merge-tree`: once the graph is local, each merge-tree probe
        # is ~37 ms, so conflict-analysing the whole open set costs seconds, not the "expensive
        # fan-out" the per-PR model implied.
        if not refspecs:
            return {}
        pairs = [f"+{source}:{dest}" for source, dest in refspecs]
        _run(
            self._net(["git", "fetch", "--quiet", "--no-tags", self._remote, *pairs]),
            cwd=self._git_dir,
        )
        resolved: dict[str, str] = {}
        for _source, dest in refspecs:
            resolved[dest] = _run(["git", "rev-parse", dest], cwd=self._git_dir).stdout.strip()
        return resolved

    def merge_tree(self, left: str, right: str) -> tuple[str, ...]:
        """Return paths that conflict when merging the two object IDs."""

        proc = _run(
            ["git", "merge-tree", "--write-tree", "--name-only", "--messages", left, right],
            cwd=self._git_dir,
            allowed=(0, 1),
        )
        if proc.returncode == 0:
            return ()
        lines = proc.stdout.splitlines()
        paths: list[str] = []
        for line in lines[1:]:  # first line is the tree oid
            candidate = line.strip()
            if not candidate:
                break
            paths.append(candidate)
        return tuple(sorted(set(paths)))

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        """Return whether *ancestor* is reachable from *descendant*."""

        proc = _run(
            ["git", "merge-base", "--is-ancestor", ancestor, descendant],
            cwd=self._git_dir,
            allowed=(0, 1),
        )
        return proc.returncode == 0

    def changed_files(self, base_sha: str, head_sha: str) -> frozenset[str]:
        """Return paths changed between the base and head object IDs."""

        merge_base = _run(
            ["git", "merge-base", base_sha, head_sha], cwd=self._git_dir
        ).stdout.strip()
        out = _run(
            ["git", "diff", "--name-only", f"{merge_base}...{head_sha}"], cwd=self._git_dir
        ).stdout
        return frozenset(line for line in out.splitlines() if line)

    def commits_behind(self, head_sha: str, base_sha: str) -> int:
        """Return how many commits *head_sha* is behind *base_sha*."""

        out = _run(
            ["git", "rev-list", "--count", f"{head_sha}..{base_sha}"], cwd=self._git_dir
        ).stdout.strip()
        try:
            return int(out)
        except ValueError:
            return 0


__all__ = ["GitHubHost", "HostCommandError", "GH_FIELDS", "LIGHT_FIELDS"]
