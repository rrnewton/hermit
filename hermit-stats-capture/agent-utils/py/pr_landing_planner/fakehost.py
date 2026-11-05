"""A deterministic, network-free :class:`~pr_landing_planner.host.VcsHost` for tests + the demo.

:class:`FakeHost` answers every host operation from an in-memory fixture (loadable from JSON or
YAML), so the whole planner — collection, conflict graph, CI classification, fusion, and all three
output formats — is exercisable with zero network and byte-stable output. The fixture can also
simulate a PR moving mid-collection (``fetched_head_sha`` differing from ``api_head_sha``) to test
the content-identity guard.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    # PyYAML is declared but imported lazily so JSON fixtures and startup diagnostics remain usable
    # if an installation is damaged.
    import yaml

from pr_landing_planner.model import (
    CheckRun,
    NATIVE_REVIEW_STATES,
    RawPr,
    ReviewEvidenceEvent,
    ReviewEvidenceSnapshot,
    edge_key,
)

_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1


class FixtureError(ValueError):
    """Raised when a FakeHost fixture document is malformed."""


# Surfaced (as a FixtureError, so the CLI prints it cleanly) when a YAML fixture is requested but
# the declared PyYAML dependency is not installed.
_MISSING_YAML_MSG = (
    "loading a YAML fixture requires PyYAML, but that package is not installed. "
    "Repair this installation with: python3 -m pip install 'pyyaml>=6'. "
    "JSON fixtures remain available."
)


# --------------------------------------------------------------------------- narrowing helpers
def _as_obj(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise FixtureError(f"{where}: expected an object, got {type(value).__name__}")
    return {str(k): v for k, v in value.items()}


def _opt_str(m: Mapping[str, object], key: str, default: str) -> str:
    val = m.get(key, default)
    if isinstance(val, (int, float)) and not isinstance(val, bool):
        return str(val)
    return val if isinstance(val, str) else default


def _required_review_str(
    m: Mapping[str, object], key: str, where: str, *, allow_empty: bool
) -> str:
    if key not in m or not isinstance(m[key], str):
        raise FixtureError(f"{where}: field {key!r} must be a string")
    value = m[key]
    assert isinstance(value, str)
    if not allow_empty and not value:
        raise FixtureError(f"{where}: field {key!r} must be non-empty")
    return value


def _optional_review_author(
    m: Mapping[str, object], key: str, where: str
) -> str:
    if key not in m or m[key] is None:
        return ""
    value = m[key]
    if not isinstance(value, str):
        raise FixtureError(f"{where}: field {key!r} must be a string or null")
    return value.strip()


def _required_nullable_review_str(
    m: Mapping[str, object], key: str, where: str
) -> str:
    if key not in m:
        raise FixtureError(f"{where}: field {key!r} is required")
    value = m[key]
    if value is None:
        return ""
    if not isinstance(value, str):
        raise FixtureError(f"{where}: field {key!r} must be a string or null")
    return value


def _req_int(
    m: Mapping[str, object], key: str, where: str, *, positive: bool = False
) -> int:
    val = m.get(key)
    if isinstance(val, bool) or not isinstance(val, int):
        raise FixtureError(f"{where}: field {key!r} must be an integer")
    if not _I64_MIN <= val <= _I64_MAX:
        raise FixtureError(f"{where}: field {key!r} must fit a signed 64-bit integer")
    if positive and val <= 0:
        raise FixtureError(f"{where}: field {key!r} must be a positive integer")
    return val


def _opt_int(
    m: Mapping[str, object],
    key: str,
    default: int,
    where: str,
    *,
    nonnegative: bool = False,
) -> int:
    val = m.get(key)
    if val is None:
        return default
    if isinstance(val, bool) or not isinstance(val, int):
        raise FixtureError(f"{where}: field {key!r} must be an integer")
    if not _I64_MIN <= val <= _I64_MAX:
        raise FixtureError(f"{where}: field {key!r} must fit a signed 64-bit integer")
    if nonnegative and val < 0:
        raise FixtureError(f"{where}: field {key!r} must be nonnegative")
    return val


def _opt_bool(m: Mapping[str, object], key: str, default: bool) -> bool:
    val = m.get(key, default)
    return val if isinstance(val, bool) else default

def _strict_opt_bool(m: Mapping[str, object], key: str, default: bool, where: str) -> bool:
    if key not in m:
        return default
    val = m[key]
    if not isinstance(val, bool):
        raise FixtureError(f"{where}: field {key!r} must be a boolean")
    return val



def _opt_str_list(m: Mapping[str, object], key: str) -> tuple[str, ...]:
    val = m.get(key)
    if not isinstance(val, list):
        return ()
    return tuple(str(item) for item in val)


def _checks_from(value: object, where: str) -> tuple[CheckRun, ...]:
    if value is None:
        return ()
    if not isinstance(value, list):
        raise FixtureError(f"{where}: 'checks' must be a list")
    out: list[CheckRun] = []
    for i, entry in enumerate(value):
        obj = _as_obj(entry, f"{where}.checks[{i}]")
        duration = (
            _opt_int(obj, "duration_secs", 0, f"{where}.checks[{i}]", nonnegative=True)
            if obj.get("duration_secs") is not None
            else None
        )
        out.append(
            CheckRun(
                name=_opt_str(obj, "name", ""),
                status=_opt_str(obj, "status", "").upper(),
                conclusion=_opt_str(obj, "conclusion", "").upper(),
                text=_opt_str(obj, "text", ""),
                workflow=_opt_str(obj, "workflow", ""),
                duration_secs=duration,
            )
        )
    return tuple(out)


def _review_snapshot_from(
    obj: Mapping[str, object], where: str, *, default_head: str, default_decision: str
) -> ReviewEvidenceSnapshot | None:
    value = obj.get("review_events")
    if value is None:
        return None
    if not isinstance(value, list):
        raise FixtureError(f"{where}: 'review_events' must be a list")
    events: list[ReviewEvidenceEvent] = []
    for index, entry in enumerate(value):
        event_where = f"{where}.review_events[{index}]"
        event = _as_obj(entry, event_where)
        kind = _required_review_str(event, "kind", event_where, allow_empty=False)
        identity = _required_review_str(
            event, "identity", event_where, allow_empty=False
        )
        author = _optional_review_author(event, "author", event_where)
        state = _required_review_str(event, "state", event_where, allow_empty=False)
        if kind == "review" and state not in NATIVE_REVIEW_STATES:
            raise FixtureError(f"{event_where}: review has unknown state {state!r}")
        head_sha = _required_review_str(event, "head_sha", event_where, allow_empty=True)
        if kind == "review" and not head_sha:
            raise FixtureError(f"{event_where}: review requires non-empty head_sha")
        created_at = _required_review_str(
            event, "created_at", event_where, allow_empty=False
        )
        updated_at = _required_review_str(
            event, "updated_at", event_where, allow_empty=False
        )
        last_edited_at = _required_nullable_review_str(
            event, "last_edited_at", event_where
        )
        body = _required_review_str(event, "body", event_where, allow_empty=True)
        retirement_actor_permission = (
            _required_review_str(
                event,
                "retirement_actor_permission",
                event_where,
                allow_empty=True,
            )
            if "retirement_actor_permission" in event
            else ""
        )
        events.append(
            ReviewEvidenceEvent(
                kind=kind,
                identity=identity,
                state=state,
                head_sha=head_sha,
                created_at=created_at,
                updated_at=updated_at,
                last_edited_at=last_edited_at,
                body=body,
                author=author,
                retirement_actor_permission=retirement_actor_permission,
            )
        )
    if "review_snapshot_head_sha" in obj:
        snapshot_head = _required_review_str(
            obj, "review_snapshot_head_sha", where, allow_empty=False
        )
    else:
        snapshot_head = default_head
    if "review_snapshot_review_decision" not in obj:
        snapshot_decision = default_decision
    else:
        raw_decision = obj["review_snapshot_review_decision"]
        if raw_decision is None:
            snapshot_decision = ""
        elif isinstance(raw_decision, str):
            snapshot_decision = raw_decision
        else:
            raise FixtureError(
                f"{where}: field 'review_snapshot_review_decision' must be a string or null"
            )
    return ReviewEvidenceSnapshot(
        head_sha=snapshot_head,
        review_decision=snapshot_decision,
        events=tuple(events),
    )


# --------------------------------------------------------------------------- internal PR record
@dataclass(frozen=True)
class _FakePr:
    raw: RawPr
    head_sha: str
    fetched_head_sha: str
    changed_files: frozenset[str]
    base_conflict_paths: tuple[str, ...]
    commits_behind: int


def _fake_pr_from(value: object, where: str, *, default_base: str) -> _FakePr:
    obj = _as_obj(value, where)
    number = _req_int(obj, "number", where, positive=True)
    head_sha = _opt_str(obj, "head_sha", f"sha-{number}")
    api_head_sha = _opt_str(obj, "api_head_sha", head_sha)
    fetched_head_sha = _opt_str(obj, "fetched_head_sha", head_sha)
    head_ref = _opt_str(obj, "head_ref", f"feature-{number}")
    base_ref = _opt_str(obj, "base_ref", default_base)
    for field, value in (
        ("head_ref", head_ref),
        ("base_ref", base_ref),
        ("head_sha", head_sha),
        ("api_head_sha", api_head_sha),
        ("fetched_head_sha", fetched_head_sha),
    ):
        if not value:
            raise FixtureError(f"{where}: field {field!r} must be a non-empty string")
    review_decision = _opt_str(obj, "review_decision", "")
    raw = RawPr(
        number=number,
        head_ref=head_ref,
        base_ref=base_ref,
        api_head_sha=api_head_sha,
        title=_opt_str(obj, "title", ""),
        author=_opt_str(obj, "author", ""),
        is_draft=_opt_bool(obj, "is_draft", False),
        mergeable=_opt_str(obj, "mergeable", ""),
        review_decision=review_decision,
        review_evidence_unavailable=_strict_opt_bool(
            obj, "review_evidence_unavailable", False, where
        ),
        created_at=_opt_str(obj, "created_at", ""),
        updated_at=_opt_str(obj, "updated_at", ""),
        additions=_opt_int(obj, "additions", 0, where, nonnegative=True),
        deletions=_opt_int(obj, "deletions", 0, where, nonnegative=True),
        labels=_opt_str_list(obj, "labels"),
        checks=_checks_from(obj.get("checks"), where),
        review_snapshot=_review_snapshot_from(
            obj,
            where,
            default_head=head_sha,
            default_decision=review_decision,
        ),
        mechanism_symbols=_opt_str_list(obj, "mechanism_symbols"),
    )
    return _FakePr(
        raw=raw,
        head_sha=head_sha,
        fetched_head_sha=fetched_head_sha,
        changed_files=frozenset(_opt_str_list(obj, "changed_files")),
        base_conflict_paths=_opt_str_list(obj, "base_conflict_paths"),
        commits_behind=_opt_int(obj, "commits_behind", 0, where, nonnegative=True),
    )


# --------------------------------------------------------------------------- the fake host
class FakeHost:
    """A :class:`~pr_landing_planner.host.VcsHost` backed by an in-memory fixture."""

    def __init__(
        self,
        prs: tuple[_FakePr, ...],
        conflicts: Mapping[tuple[int, int], tuple[str, ...]],
        ancestry: frozenset[tuple[int, int]],
        base_shas: Mapping[str, str],
    ) -> None:
        self._prs = prs
        self._conflicts = dict(conflicts)
        self._ancestry = ancestry
        self._base_shas = dict(base_shas)
        self._by_number = {p.raw.number: p for p in prs}
        self._number_by_sha: dict[str, int] = {}
        for p in prs:
            self._number_by_sha[p.head_sha] = p.raw.number
            self._number_by_sha[p.fetched_head_sha] = p.raw.number
        self._base_ref_by_sha = {sha: ref for ref, sha in self._base_shas.items()}

    # --- VcsHost protocol -------------------------------------------------
    def list_open_prs(self, repo: str, base: str | None) -> tuple[RawPr, ...]:
        """Return fixture PRs in their declared order."""

        return tuple(p.raw for p in self._prs)

    def prefetch_refs(self, refspecs: Sequence[tuple[str, str]]) -> dict[str, str]:
        """Resolve requested fixture refs to their deterministic object IDs."""

        return {dest: self._resolve_source(source) for source, dest in refspecs}

    def _resolve_source(self, source: str) -> str:
        if source.startswith("refs/heads/"):
            ref = source[len("refs/heads/") :]
            return self._base_shas.get(ref, f"basesha-{ref}")
        if source.startswith("refs/pull/") and source.endswith("/head"):
            middle = source[len("refs/pull/") : -len("/head")]
            number = int(middle)
            pr = self._by_number.get(number)
            if pr is None:
                raise FixtureError(f"prefetch_refs: unknown PR in {source!r}")
            return pr.fetched_head_sha
        raise FixtureError(f"prefetch_refs: unrecognized source ref {source!r}")

    def merge_tree(self, left: str, right: str) -> tuple[str, ...]:
        """Return fixture conflict paths for two object IDs."""

        left_pr = self._number_by_sha.get(left)
        right_pr = self._number_by_sha.get(right)
        left_base = left in self._base_ref_by_sha
        right_base = right in self._base_ref_by_sha
        if left_base and right_pr is not None:
            return self._by_number[right_pr].base_conflict_paths
        if right_base and left_pr is not None:
            return self._by_number[left_pr].base_conflict_paths
        if left_pr is not None and right_pr is not None:
            return self._conflicts.get(edge_key(left_pr, right_pr), ())
        return ()

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        """Return whether the fixture declares the requested ancestry relation."""

        a = self._number_by_sha.get(ancestor)
        d = self._number_by_sha.get(descendant)
        if a is None or d is None:
            return False
        return (a, d) in self._ancestry

    def changed_files(self, base_sha: str, head_sha: str) -> frozenset[str]:
        """Return the fixture's changed-file set for *head_sha*."""

        number = self._number_by_sha.get(head_sha)
        return self._by_number[number].changed_files if number is not None else frozenset()

    def commits_behind(self, head_sha: str, base_sha: str) -> int:
        """Return the fixture's base-behind count for *head_sha*."""

        number = self._number_by_sha.get(head_sha)
        return self._by_number[number].commits_behind if number is not None else 0

    # --- construction -----------------------------------------------------
    @classmethod
    def from_fixture(cls, raw: object) -> tuple["FakeHost", str, str]:
        """Build a FakeHost from a parsed fixture. Returns ``(host, repo, base)``."""
        doc = _as_obj(raw, "<root>")
        repo = _opt_str(doc, "repo", "owner/repo")
        base = _opt_str(doc, "base", "main")
        if not base:
            raise FixtureError("<root>: field 'base' must be a non-empty string")
        prs_raw = doc.get("prs")
        if not isinstance(prs_raw, list):
            raise FixtureError("<root>: 'prs' must be a list")
        prs = tuple(
            _fake_pr_from(entry, f"prs[{i}]", default_base=base)
            for i, entry in enumerate(prs_raw)
        )
        numbers = [pr.raw.number for pr in prs]
        if len(set(numbers)) != len(numbers):
            raise FixtureError("<root>: duplicate PR numbers are not allowed")
        head_refs = [pr.raw.head_ref for pr in prs]
        if len(set(head_refs)) != len(head_refs):
            raise FixtureError("<root>: duplicate PR head_ref values are ambiguous")
        fixture_shas: dict[str, int] = {}
        for pr in prs:
            for sha in (pr.head_sha, pr.fetched_head_sha):
                owner = fixture_shas.setdefault(sha, pr.raw.number)
                if owner != pr.raw.number:
                    raise FixtureError(
                        f"<root>: commit identity {sha!r} is shared by PR #{owner} and "
                        f"PR #{pr.raw.number}; fixture host data would be ambiguous"
                    )
        number_set = set(numbers)

        conflicts: dict[tuple[int, int], tuple[str, ...]] = {}
        for i, entry in enumerate(_optional_list(doc, "conflicts")):
            obj = _as_obj(entry, f"conflicts[{i}]")
            a = _req_int(obj, "a", f"conflicts[{i}]", positive=True)
            b = _req_int(obj, "b", f"conflicts[{i}]", positive=True)
            if a == b:
                raise FixtureError(f"conflicts[{i}]: self-conflict for PR #{a} is invalid")
            unknown = sorted({a, b} - number_set)
            if unknown:
                raise FixtureError(
                    f"conflicts[{i}]: unknown PR endpoint(s): "
                    + ", ".join(f"#{number}" for number in unknown)
                )
            key = edge_key(a, b)
            if key in conflicts:
                raise FixtureError(f"conflicts[{i}]: duplicate conflict edge {key}")
            conflicts[key] = _opt_str_list(obj, "paths") or ("<conflict>",)

        ancestry: set[tuple[int, int]] = set()
        for i, entry in enumerate(_optional_list(doc, "ancestry")):
            obj = _as_obj(entry, f"ancestry[{i}]")
            before = _req_int(obj, "before", f"ancestry[{i}]", positive=True)
            after = _req_int(obj, "after", f"ancestry[{i}]", positive=True)
            if before == after:
                raise FixtureError(f"ancestry[{i}]: self-ancestry for PR #{before} is invalid")
            unknown = sorted({before, after} - number_set)
            if unknown:
                raise FixtureError(
                    f"ancestry[{i}]: unknown PR endpoint(s): "
                    + ", ".join(f"#{number}" for number in unknown)
                )
            edge = (before, after)
            if edge in ancestry:
                raise FixtureError(f"ancestry[{i}]: duplicate ancestry edge {edge}")
            ancestry.add(edge)

        base_refs = {p.raw.base_ref for p in prs}
        base_shas = {ref: f"basesha-{ref}" for ref in base_refs}
        return cls(prs, conflicts, frozenset(ancestry), base_shas), repo, base


def _optional_list(doc: Mapping[str, object], key: str) -> list[object]:
    value = doc.get(key)
    if value is None:
        return []
    if not isinstance(value, list):
        raise FixtureError(f"<root>: field {key!r} must be a list")
    return list(value)


def load_fixture_text(text: str, *, as_yaml: bool) -> object:
    """Parse a fixture document from ``text`` (YAML or JSON) into a plain ``object`` (no Any leak).

    If the declared PyYAML dependency is missing, a YAML fixture raises :class:`FixtureError` with
    an actionable repair hint (JSON fixtures remain available)."""
    if as_yaml:
        try:
            import yaml
        except ModuleNotFoundError as exc:
            raise FixtureError(_MISSING_YAML_MSG) from exc

        class _UniqueKeyLoader(yaml.SafeLoader):
            pass

        def construct_unique_mapping(
            loader: yaml.SafeLoader, node: yaml.MappingNode, deep: bool = False
        ) -> dict[str, object]:
            mapping: dict[str, object] = {}
            for key_node, value_node in node.value:
                key: object = loader.construct_object(key_node, deep=deep)
                if not isinstance(key, str):
                    raise FixtureError("invalid YAML fixture: mapping keys must be strings")
                if key == "<<":
                    raise FixtureError("invalid YAML fixture: merge keys are not supported")
                if key in mapping:
                    raise FixtureError(f"invalid YAML fixture: duplicate key {key!r}")
                mapping[key] = loader.construct_object(value_node, deep=deep)
            return mapping

        _UniqueKeyLoader.add_constructor(
            yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG,
            construct_unique_mapping,
        )
        try:
            raw: object = yaml.load(text, Loader=_UniqueKeyLoader)
        except yaml.YAMLError as exc:
            raise FixtureError(f"invalid YAML fixture: {exc}") from exc
    else:
        def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
            obj: dict[str, object] = {}
            for key, value in pairs:
                if key in obj:
                    raise FixtureError(f"invalid JSON fixture: duplicate key {key!r}")
                obj[key] = value
            return obj

        try:
            raw = json.loads(text, object_pairs_hook=unique_object)
        except json.JSONDecodeError as exc:
            raise FixtureError(f"invalid JSON fixture: {exc}") from exc
    return raw


__all__ = ["FakeHost", "FixtureError", "load_fixture_text"]
