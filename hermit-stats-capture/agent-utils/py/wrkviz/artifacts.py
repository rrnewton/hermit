"""Deterministic, evidence-backed work-output artifact extraction.

The normalized transcript intentionally drops bulky tool inputs and outputs before it is
committed to an archive.  Artifact extraction therefore runs once during ingest, while that
evidence is still available, and stores a compact provenance catalog beside ``team.json``.
No language-model summary participates in this pass.
"""

from __future__ import annotations

import hashlib
import json
import re
import shlex
from bisect import bisect_left
from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from pathlib import PurePosixPath
from urllib.parse import urlsplit, urlunsplit

from wrkviz.archive import JsonValue, as_array, as_int, as_object, as_string
from wrkviz.model import Event, JsonObject, TeamData, ToolCall, source_digest


EXTRACTOR_VERSION = "work-artifacts-v1"


class ArtifactKind(str, Enum):
    """Kinds of durable external objects recognized without model inference."""

    COMMIT = "commit"
    PULL_REQUEST = "pull_request"
    MERGE_REQUEST = "merge_request"
    DIFF = "diff"
    ISSUE = "issue"
    TASK = "task"
    REPOSITORY = "repository"
    GIST = "gist"
    PASTE = "paste"
    UPLOADED_FILE = "uploaded_file"
    BUILD_ARTIFACT = "build_artifact"
    URL = "url"


class EvidenceRelation(str, Enum):
    """What the source evidence establishes about an artifact."""

    PRODUCED = "produced"
    PUBLISHED = "published"
    UPDATED = "updated"
    REFERENCED = "referenced"


class EvidenceConfidence(str, Enum):
    """Whether identity/action was explicit or conservatively inferred."""

    HIGH = "high"
    MEDIUM = "medium"


_OUTPUT_RELATIONS = frozenset(
    {
        EvidenceRelation.PRODUCED,
        EvidenceRelation.PUBLISHED,
        EvidenceRelation.UPDATED,
    }
)


@dataclass(frozen=True, slots=True)
class ArtifactEvidence:
    """One compact pointer back to transcript or tool evidence."""

    evidence_id: str
    source_kind: str
    source_id: str
    source_line: int
    thread_id: str
    turn_id: str | None
    timestamp_ms: int
    relation: EvidenceRelation
    action: str
    confidence: EvidenceConfidence
    matched_text: str
    extractor: str

    def to_json_obj(self) -> JsonObject:
        """Return a JSON-serializable provenance record."""

        return {
            "evidence_id": self.evidence_id,
            "source_kind": self.source_kind,
            "source_id": self.source_id,
            "source_line": self.source_line,
            "thread_id": self.thread_id,
            "turn_id": self.turn_id,
            "timestamp_ms": self.timestamp_ms,
            "relation": self.relation.value,
            "action": self.action,
            "confidence": self.confidence.value,
            "matched_text": self.matched_text,
            "extractor": self.extractor,
        }


@dataclass(frozen=True, slots=True)
class WorkArtifact:
    """One deduplicated work output or evidence-bound external reference."""

    artifact_id: str
    kind: ArtifactKind
    locator: str
    url: str | None
    label: str
    title: str | None
    external_id: str | None
    project_url: str | None
    project_slug: str | None
    producer_thread_id: str | None
    produced_at_ms: int | None
    evidence: tuple[ArtifactEvidence, ...]

    def to_json_obj(self) -> JsonObject:
        """Return a JSON-serializable artifact record."""

        return {
            "artifact_id": self.artifact_id,
            "kind": self.kind.value,
            "locator": self.locator,
            "url": self.url,
            "label": self.label,
            "title": self.title,
            "external_id": self.external_id,
            "project_url": self.project_url,
            "project_slug": self.project_slug,
            "producer_thread_id": self.producer_thread_id,
            "produced_at_ms": self.produced_at_ms,
            "evidence": [item.to_json_obj() for item in self.evidence],
        }


@dataclass(frozen=True, slots=True)
class ProjectIdentity:
    """A repository/project identity derived from exact remote or artifact URLs."""

    project_id: str
    host: str
    slug: str
    url: str
    evidence_ids: tuple[str, ...]

    def to_json_obj(self) -> JsonObject:
        """Return a JSON-serializable project record."""

        return {
            "project_id": self.project_id,
            "host": self.host,
            "slug": self.slug,
            "url": self.url,
            "evidence_ids": list(self.evidence_ids),
        }


@dataclass(frozen=True, slots=True)
class ArtifactCatalog:
    """Versioned artifact extraction result for one exact source snapshot."""

    source_digest: str
    artifacts: tuple[WorkArtifact, ...]
    projects: tuple[ProjectIdentity, ...]
    schema_version: int = 1
    extractor_version: str = EXTRACTOR_VERSION

    def to_json_obj(self) -> JsonObject:
        """Return the complete deterministic catalog."""

        return {
            "schema_version": self.schema_version,
            "extractor_version": self.extractor_version,
            "source_digest": self.source_digest,
            "artifacts": [item.to_json_obj() for item in self.artifacts],
            "projects": [item.to_json_obj() for item in self.projects],
        }


@dataclass(frozen=True, slots=True)
class _ArtifactRangeEntry:
    timestamp_ms: int
    artifact_id: str
    output: bool


@dataclass(frozen=True, slots=True)
class _ArtifactRangeSeries:
    timestamps: tuple[int, ...]
    entries: tuple[_ArtifactRangeEntry, ...]


@dataclass(frozen=True, slots=True)
class ArtifactRangeIndex:
    """One-pass evidence index for repeated render-time interval queries."""

    all_evidence: _ArtifactRangeSeries
    evidence_by_thread: Mapping[str, _ArtifactRangeSeries]

    @classmethod
    def from_catalog(cls, catalog: ArtifactCatalog) -> ArtifactRangeIndex:
        """Build a timestamp-sorted query index from one immutable catalog."""

        all_entries: list[_ArtifactRangeEntry] = []
        entries_by_thread: dict[str, list[_ArtifactRangeEntry]] = {}
        for artifact in catalog.artifacts:
            for evidence in artifact.evidence:
                entry = _ArtifactRangeEntry(
                    timestamp_ms=evidence.timestamp_ms,
                    artifact_id=artifact.artifact_id,
                    output=evidence.relation in _OUTPUT_RELATIONS,
                )
                all_entries.append(entry)
                entries_by_thread.setdefault(evidence.thread_id, []).append(entry)

        def series(entries: list[_ArtifactRangeEntry]) -> _ArtifactRangeSeries:
            ordered = tuple(
                sorted(entries, key=lambda item: (item.timestamp_ms, item.artifact_id))
            )
            return _ArtifactRangeSeries(
                timestamps=tuple(item.timestamp_ms for item in ordered),
                entries=ordered,
            )

        return cls(
            all_evidence=series(all_entries),
            evidence_by_thread={
                thread_id: series(entries)
                for thread_id, entries in entries_by_thread.items()
            },
        )

    def ids_for_range(
        self,
        start_ms: int,
        end_ms: int,
        thread_id: str | None = None,
        *,
        outputs_only: bool = False,
    ) -> tuple[str, ...]:
        """Return deduplicated IDs from a half-open indexed interval."""

        series = (
            self.all_evidence
            if thread_id is None
            else self.evidence_by_thread.get(thread_id)
        )
        if series is None or end_ms <= start_ms:
            return ()
        left = bisect_left(series.timestamps, start_ms)
        right = bisect_left(series.timestamps, end_ms, lo=left)
        return tuple(
            sorted(
                {
                    entry.artifact_id
                    for entry in series.entries[left:right]
                    if not outputs_only or entry.output
                }
            )
        )


@dataclass(frozen=True, slots=True)
class _UrlIdentity:
    kind: ArtifactKind
    url: str
    label: str
    external_id: str | None
    project_url: str | None
    project_slug: str | None


@dataclass(frozen=True, slots=True)
class _Source:
    source_kind: str
    source_id: str
    source_line: int
    thread_id: str
    turn_id: str | None
    timestamp_ms: int
    text: str
    tool: ToolCall | None


@dataclass
class _ArtifactBuilder:
    identity: _UrlIdentity
    title: str | None
    evidence: dict[str, ArtifactEvidence]


_HTTP_URL = re.compile(r"https?://[^\s<>\"'`\\|${}…]+", re.IGNORECASE)
_SSH_REMOTE = re.compile(
    r"(?<![A-Za-z0-9])git@(?P<host>[A-Za-z0-9.-]+):"
    r"(?P<slug>[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?"
    r"(?=$|[\s'\"),;])"
)
_COMMIT_OUTPUT = re.compile(
    r"^\[(?P<branch>[^]\s]+) (?P<sha>[0-9a-f]{7,40})\] (?P<title>.+)$",
    re.MULTILINE | re.IGNORECASE,
)
_FULL_SHA = re.compile(r"(?<![0-9a-f])([0-9a-f]{40})(?![0-9a-f])", re.IGNORECASE)
_PROPERTY = re.compile(
    r"(?:[\"']?{key}[\"']?)\s*:\s*(?P<value>\"(?:\\.|[^\"\\])*\")"
)
_QUALIFIED_PR = re.compile(
    r"(?<![A-Za-z0-9_./-])(?P<slug>[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)"
    r"#(?P<number>[1-9][0-9]*)(?![0-9])"
)
_CONTEXT_PR = re.compile(r"(?<![A-Za-z0-9_])PR\s+#(?P<number>[1-9][0-9]*)", re.I)
_CONTEXT_ISSUE = re.compile(
    r"(?<![A-Za-z0-9_])(?:issue|task)\s+#(?P<number>[1-9][0-9]*)", re.I
)
_GITHUB_OWNER = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})\Z")
_GITHUB_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]{1,100}\Z")


def _hash_id(prefix: str, material: str) -> str:
    return prefix + hashlib.sha256(material.encode("utf-8")).hexdigest()[:20]


def _safe_match(text: str) -> str:
    """Keep only the matched identity/evidence, never a surrounding command or secret."""

    compact = " ".join(text.split())
    return compact[:256]


def _property(text: str, key: str) -> str | None:
    match = re.compile(_PROPERTY.pattern.format(key=re.escape(key))).search(text)
    if match is None:
        return None
    try:
        value: object = json.loads(match.group("value"))
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, str) else None


def _json_object(text: str) -> dict[str, object]:
    try:
        raw: object = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(raw, dict):
        return {}
    result: dict[str, object] = {}
    for key, value in raw.items():
        if isinstance(key, str):
            result[key] = value
    return result


def _tool_command(tool: ToolCall) -> str:
    text = tool.input_text or ""
    obj = _json_object(text)
    for key in ("cmd", "command", "script"):
        value = obj.get(key)
        if isinstance(value, str):
            return value
    for key in ("cmd", "command", "script"):
        value = _property(text, key)
        if value is not None:
            return value
    # Orc code executions are already the shell/program text rather than a wrapper object.
    return text


def _flatten_output(text: str | None) -> str:
    """Flatten provider tool-result envelopes into the human-visible text fragments."""

    if text is None:
        return ""
    try:
        raw: object = json.loads(text)
    except json.JSONDecodeError:
        return text
    fragments: list[str] = []

    def visit(value: object) -> None:
        if isinstance(value, str):
            fragments.append(value)
        elif isinstance(value, list):
            for item in value:
                visit(item)
        elif isinstance(value, dict):
            # Provider result envelopes use ``text``/``output``. Avoid copying unrelated
            # metadata strings such as MIME types or opaque IDs into evidence scanning.
            for key in ("text", "output"):
                if key in value:
                    visit(value[key])

    visit(raw)
    return "\n".join(fragments) if fragments else text


def _shell_segments(command: str) -> tuple[tuple[str, ...], ...]:
    """Tokenize shell statements without matching command names inside quoted search text."""

    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|\n")
        # Make an unquoted newline a statement separator while shlex still preserves a newline
        # inside a quoted PR body as part of that one argument.
        lexer.whitespace = " \t\r"
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return ()
    result: list[tuple[str, ...]] = []
    current: list[str] = []
    for token in tokens:
        if token and all(character in ";|&\n" for character in token):
            if current:
                result.append(tuple(current))
                current = []
        else:
            current.append(token)
    if current:
        result.append(tuple(current))
    return tuple(result)


def _executable_tokens(segment: tuple[str, ...]) -> tuple[str, ...]:
    tokens = list(segment)
    while tokens and ("=" in tokens[0] and not tokens[0].startswith(("-", "/"))):
        tokens.pop(0)
    while tokens and tokens[0] in ("env", "command", "sudo", "with-proxy", "timeout"):
        wrapper = tokens.pop(0)
        if wrapper == "env":
            while tokens and (tokens[0].startswith("-") or "=" in tokens[0]):
                option = tokens.pop(0)
                if option in ("-u", "--unset") and tokens:
                    tokens.pop(0)
        elif wrapper == "timeout" and tokens:
            tokens.pop(0)
        else:
            while tokens and tokens[0].startswith("-"):
                tokens.pop(0)
    return tuple(tokens)


def _command_has(tool: ToolCall, executable: str, *arguments: str) -> bool:
    """Require an actual command boundary, not a policy/search substring."""

    desired = (executable, *arguments)
    for raw_segment in _shell_segments(_tool_command(tool)):
        segment = _executable_tokens(raw_segment)
        if segment[: len(desired)] == desired:
            return True
        if executable == "git" and segment[:1] == ("git",):
            index = 1
            while index < len(segment) and segment[index].startswith("-"):
                if segment[index] == "-C" and index + 1 < len(segment):
                    index += 2
                else:
                    index += 1
            if segment[index : index + len(arguments)] == arguments:
                return True
        if segment[:1] in (("bash",), ("sh",)):
            for index, token in enumerate(segment[:-1]):
                if token in ("-c", "-lc"):
                    nested = ToolCall(
                        call_id=tool.call_id,
                        item_id=tool.item_id,
                        thread_id=tool.thread_id,
                        turn_id=tool.turn_id,
                        name=tool.name,
                        namespace=tool.namespace,
                        started_at_ms=tool.started_at_ms,
                        ended_at_ms=tool.ended_at_ms,
                        status=tool.status,
                        input_text=segment[index + 1],
                        output_text=tool.output_text,
                        nested_tools=tool.nested_tools,
                        source_line=tool.source_line,
                    )
                    if _command_has(nested, executable, *arguments):
                        return True
    return False


def _command_has_option(tool: ToolCall, executable: str, options: tuple[str, ...]) -> bool:
    for raw_segment in _shell_segments(_tool_command(tool)):
        segment = _executable_tokens(raw_segment)
        if segment[:1] == (executable,) and any(option in segment[1:] for option in options):
            return True
    return False


def _command_title(tool: ToolCall) -> str | None:
    """Read a bounded explicit title option from a recognized hosting CLI command."""

    for raw_segment in _shell_segments(_tool_command(tool)):
        segment = _executable_tokens(raw_segment)
        if segment[:1] not in (("gh",), ("glab",)):
            continue
        for index, token in enumerate(segment[1:], start=1):
            value: str | None = None
            if token in ("--title", "-t") and index + 1 < len(segment):
                value = segment[index + 1]
            elif token.startswith("--title="):
                value = token.partition("=")[2]
            if value:
                compact = " ".join(value.split())
                return compact[:300] if compact else None
    return None


def _tool_workdir(tool: ToolCall) -> str | None:
    text = tool.input_text or ""
    obj = _json_object(text)
    for key in ("workdir", "cwd"):
        value = obj.get(key)
        if isinstance(value, str) and value.startswith("/"):
            return value.rstrip("/") or "/"
    for key in ("workdir", "cwd"):
        value = _property(text, key)
        if value is not None and value.startswith("/"):
            return value.rstrip("/") or "/"
    return None


def _trim_url(raw: str) -> str:
    result = raw.rstrip(".,;:!?)]}`*_")
    # JSON-encoded output sometimes leaves a literal escaped newline adjacent to the URL.
    for suffix in (r"\n", r"\r", r"\t"):
        if result.endswith(suffix):
            result = result[: -len(suffix)]
    return result.rstrip(".,;:!?)]}`*_")


def _normalized_http(raw: str) -> str | None:
    candidate = _trim_url(raw)
    try:
        parts = urlsplit(candidate)
    except ValueError:
        # Transcript prose is untrusted input. In particular, strings beginning with an
        # unmatched ``[`` after the authority delimiter make urllib interpret the text as
        # a malformed IPv6 literal. Such text is not evidence for an artifact and must not
        # abort ingestion of the enclosing transcript.
        return None
    if parts.scheme.lower() not in ("http", "https") or not parts.hostname:
        return None
    if parts.username is not None or parts.password is not None:
        return None
    host = parts.hostname.lower().rstrip(".")
    if host in ("localhost", "127.0.0.1", "::1"):
        return None
    try:
        port = parts.port
    except ValueError:
        return None
    netloc = host if port is None else f"{host}:{port}"
    path = re.sub(r"/{2,}", "/", parts.path or "/")
    # Query strings routinely carry signed download credentials, SAS tokens, API keys, and
    # expiring authorization. No v1 artifact identity needs one, so retain neither query nor
    # fragment rather than attempting a brittle denylist.
    return urlunsplit(("https", netloc, path, "", ""))


def _repository_parts(host: str, path_parts: list[str]) -> tuple[str, str] | None:
    if host == "github.com" and len(path_parts) >= 2:
        if (
            _GITHUB_OWNER.fullmatch(path_parts[0]) is None
            or _GITHUB_REPOSITORY.fullmatch(path_parts[1]) is None
            or path_parts[1] in (".", "..")
        ):
            return None
        return "/".join(path_parts[:2]), f"https://github.com/{'/'.join(path_parts[:2])}"
    if host.endswith("gitlab.com") and "-" in path_parts:
        split = path_parts.index("-")
        if split >= 2:
            slug = "/".join(path_parts[:split])
            return slug, f"https://{host}/{slug}"
    if host.endswith("gitlab.com") and len(path_parts) >= 2:
        slug = "/".join(path_parts[:2])
        return slug, f"https://{host}/{slug}"
    return None


def _url_identity(raw: str) -> _UrlIdentity | None:
    normalized = _normalized_http(raw)
    if normalized is None:
        return None
    parts = urlsplit(normalized)
    host = parts.hostname or ""
    path_parts = [part for part in parts.path.split("/") if part]
    if host == "github.com" and len(path_parts) >= 2:
        path_parts[1] = path_parts[1].removesuffix(".git")

    if host == "gist.github.com" and len(path_parts) >= 2:
        base = f"https://gist.github.com/{path_parts[0]}/{path_parts[1]}"
        return _UrlIdentity(ArtifactKind.GIST, base, f"Gist {path_parts[1]}", path_parts[1], None, None)

    repository = _repository_parts(host, path_parts)
    project_slug = repository[0] if repository is not None else None
    project_url = repository[1] if repository is not None else None
    tail = path_parts[2:] if host == "github.com" else path_parts
    if host == "github.com" and repository is not None:
        if not tail:
            return _UrlIdentity(
                ArtifactKind.REPOSITORY,
                project_url or normalized,
                project_slug or normalized,
                project_slug,
                project_url,
                project_slug,
            )
        if len(tail) >= 2 and tail[0] == "pull" and tail[1].isdigit():
            number = tail[1]
            url = f"{project_url}/pull/{number}"
            return _UrlIdentity(ArtifactKind.PULL_REQUEST, url, f"{project_slug} PR #{number}", number, project_url, project_slug)
        if len(tail) >= 2 and tail[0] == "issues" and tail[1].isdigit():
            number = tail[1]
            url = f"{project_url}/issues/{number}"
            return _UrlIdentity(ArtifactKind.ISSUE, url, f"{project_slug} issue #{number}", number, project_url, project_slug)
        if len(tail) >= 2 and tail[0] in ("commit", "commits") and re.fullmatch(r"[0-9a-fA-F]{7,40}", tail[1]):
            revision = tail[1].lower()
            url = f"{project_url}/commit/{revision}"
            return _UrlIdentity(ArtifactKind.COMMIT, url, f"{project_slug} commit {revision[:12]}", revision, project_url, project_slug)
        if len(tail) >= 4 and tail[0] == "actions" and tail[1] == "runs" and "artifacts" in tail:
            return _UrlIdentity(ArtifactKind.BUILD_ARTIFACT, normalized, f"{project_slug} build artifact", tail[-1], project_url, project_slug)
        if len(tail) >= 3 and tail[:2] == ["releases", "download"]:
            return _UrlIdentity(ArtifactKind.UPLOADED_FILE, normalized, tail[-1], tail[-1], project_url, project_slug)

    if host.endswith("gitlab.com") and repository is not None:
        marker = path_parts.index("-") if "-" in path_parts else -1
        tail = path_parts[marker + 1 :] if marker >= 0 else []
        if len(tail) >= 2 and tail[0] == "merge_requests" and tail[1].isdigit():
            number = tail[1]
            url = f"{project_url}/-/merge_requests/{number}"
            return _UrlIdentity(ArtifactKind.MERGE_REQUEST, url, f"{project_slug} MR !{number}", number, project_url, project_slug)
        if len(tail) >= 2 and tail[0] == "issues" and tail[1].isdigit():
            number = tail[1]
            url = f"{project_url}/-/issues/{number}"
            return _UrlIdentity(ArtifactKind.ISSUE, url, f"{project_slug} issue #{number}", number, project_url, project_slug)
        if len(tail) >= 2 and tail[0] == "commit" and re.fullmatch(r"[0-9a-fA-F]{7,40}", tail[1]):
            revision = tail[1].lower()
            url = f"{project_url}/-/commit/{revision}"
            return _UrlIdentity(ArtifactKind.COMMIT, url, f"{project_slug} commit {revision[:12]}", revision, project_url, project_slug)

    if re.fullmatch(r"D[1-9][0-9]*", path_parts[-1] if path_parts else ""):
        identifier = path_parts[-1]
        return _UrlIdentity(ArtifactKind.DIFF, normalized, f"Diff {identifier}", identifier, None, None)
    if re.fullmatch(r"T[1-9][0-9]*", path_parts[-1] if path_parts else ""):
        identifier = path_parts[-1]
        return _UrlIdentity(ArtifactKind.TASK, normalized, f"Task {identifier}", identifier, None, None)
    if host in ("paste.rs", "pastebin.com", "pasty.lol", "dpaste.org"):
        identifier = path_parts[-1] if path_parts else host
        return _UrlIdentity(ArtifactKind.PASTE, normalized, f"Paste {identifier}", identifier, None, None)
    return _UrlIdentity(ArtifactKind.URL, normalized, normalized, None, project_url, project_slug)


def _repository_identity(raw: str) -> _UrlIdentity | None:
    value = raw.strip()
    ssh = _SSH_REMOTE.search(value)
    if ssh is not None:
        host = ssh.group("host").lower()
        slug = ssh.group("slug").removesuffix(".git")
        value = f"https://{host}/{slug}"
    normalized = _normalized_http(value)
    if normalized is None:
        return None
    parts = urlsplit(normalized)
    host = parts.hostname or ""
    path_parts = [part for part in parts.path.removesuffix(".git").split("/") if part]
    repository = _repository_parts(host, path_parts)
    if repository is None:
        return None
    slug, url = repository
    return _UrlIdentity(ArtifactKind.REPOSITORY, url, slug, slug, url, slug)


def canonical_repository_url(raw: str | None) -> str | None:
    """Return a credential-free canonical repository URL, if *raw* proves one."""

    if raw is None:
        return None
    identity = _repository_identity(raw)
    return identity.url if identity is not None else None


def _artifact_key(identity: _UrlIdentity) -> str:
    return f"{identity.kind.value}\0{identity.url}"


def _artifact_id(identity: _UrlIdentity) -> str:
    return _hash_id("artifact-", _artifact_key(identity))


def _evidence_id(
    source_kind: str,
    source_id: str,
    source_line: int,
    identity: _UrlIdentity,
    relation: EvidenceRelation,
    action: str,
) -> str:
    material = "\0".join(
        (
            source_kind,
            source_id,
            str(source_line),
            identity.kind.value,
            identity.url,
            relation.value,
            action,
        )
    )
    return _hash_id("evidence-", material)


def _evidence(
    source: _Source,
    identity: _UrlIdentity,
    relation: EvidenceRelation,
    action: str,
    confidence: EvidenceConfidence,
    matched_text: str,
) -> ArtifactEvidence:
    return ArtifactEvidence(
        evidence_id=_evidence_id(
            source.source_kind,
            source.source_id,
            source.source_line,
            identity,
            relation,
            action,
        ),
        source_kind=source.source_kind,
        source_id=source.source_id,
        source_line=source.source_line,
        thread_id=source.thread_id,
        turn_id=source.turn_id,
        timestamp_ms=source.timestamp_ms,
        relation=relation,
        action=action,
        confidence=confidence,
        matched_text=_safe_match(matched_text),
        extractor=EXTRACTOR_VERSION,
    )


def _add(
    builders: dict[str, _ArtifactBuilder],
    identity: _UrlIdentity,
    evidence: ArtifactEvidence,
    title: str | None = None,
) -> None:
    key = _artifact_key(identity)
    builder = builders.get(key)
    if builder is None:
        builder = _ArtifactBuilder(identity, title, {})
        builders[key] = builder
    elif builder.title is None and title:
        builder.title = title
    builder.evidence[evidence.evidence_id] = evidence


def _successful(tool: ToolCall) -> bool:
    if tool.status not in ("completed", "success", "succeeded"):
        return False
    output = _flatten_output(tool.output_text)
    failures = re.findall(r"(?:exit(?:_code)?|process exited with code)[=: ]+([0-9]+)", output, re.I)
    if failures:
        return all(value == "0" for value in failures)
    return not re.search(
        r"(?:^|\n)(?:fatal:|error:|failed(?:\s|:)|.*permission denied|.*no such file)",
        output,
        re.IGNORECASE,
    )


def _push_succeeded(tool: ToolCall) -> bool:
    if not _successful(tool):
        return False
    output = _flatten_output(tool.output_text)
    return bool(
        re.search(r"(?:^|\n)To\s+(?:https?://|ssh://|git@|[^\s]+:[^\s]+)", output)
        and re.search(r"(?:->|\.\.|new branch|new tag|Everything up-to-date)", output, re.I)
    )


def _url_action(source: _Source, identity: _UrlIdentity) -> tuple[EvidenceRelation, str]:
    if source.tool is None:
        return EvidenceRelation.REFERENCED, "mentioned"
    confirmed = source.source_kind == "tool_output" and _successful(source.tool)
    if confirmed and identity.kind is ArtifactKind.PULL_REQUEST and _command_has(source.tool, "gh", "pr", "create"):
        return EvidenceRelation.PRODUCED, "created_pull_request"
    if confirmed and identity.kind is ArtifactKind.MERGE_REQUEST and _command_has(source.tool, "glab", "mr", "create"):
        return EvidenceRelation.PRODUCED, "created_merge_request"
    if confirmed and identity.kind in (ArtifactKind.ISSUE, ArtifactKind.TASK) and (
        _command_has(source.tool, "gh", "issue", "create")
        or _command_has(source.tool, "glab", "issue", "create")
    ):
        return EvidenceRelation.PRODUCED, "created_issue"
    if confirmed and identity.kind is ArtifactKind.DIFF and (
        _command_has(source.tool, "jf", "submit")
        or _command_has(source.tool, "arc", "diff")
    ):
        return EvidenceRelation.PRODUCED, "created_diff"
    if confirmed and identity.kind is ArtifactKind.GIST and _command_has(source.tool, "gh", "gist", "create"):
        return EvidenceRelation.PRODUCED, "created_gist"
    upload = (
        _command_has(source.tool, "gh", "release", "upload")
        or _command_has(source.tool, "aws", "s3", "cp")
        or _command_has(source.tool, "rclone", "copy")
        or _command_has(source.tool, "rclone", "copyto")
        or _command_has_option(source.tool, "curl", ("-T", "--upload-file"))
    )
    if confirmed and identity.kind in (
        ArtifactKind.URL,
        ArtifactKind.UPLOADED_FILE,
        ArtifactKind.BUILD_ARTIFACT,
    ) and upload:
        return EvidenceRelation.PUBLISHED, "uploaded"
    if confirmed and identity.kind is ArtifactKind.PULL_REQUEST and any(
        _command_has(source.tool, "gh", "pr", action)
        for action in ("edit", "merge", "ready", "close", "reopen")
    ):
        return EvidenceRelation.UPDATED, "updated_pull_request"
    return EvidenceRelation.REFERENCED, "observed" if source.source_kind == "tool_output" else "mentioned"


def _source_records(team: TeamData) -> tuple[_Source, ...]:
    records: list[_Source] = []
    starts = {agent.thread_id: agent.started_at_ms for agent in team.agents}
    for snapshot in team.sources:
        if snapshot.repository_url:
            records.append(
                _Source(
                    "source_metadata",
                    snapshot.path,
                    1,
                    snapshot.thread_id,
                    None,
                    starts.get(snapshot.thread_id, 0),
                    snapshot.repository_url,
                    None,
                )
            )
    for event in team.events:
        if event.text:
            records.append(
                _Source(
                    "event_text",
                    event.event_id,
                    event.source_line,
                    event.thread_id,
                    event.turn_id,
                    event.timestamp_ms,
                    event.text,
                    None,
                )
            )
    for tool in team.tool_calls:
        if tool.input_text:
            records.append(
                _Source(
                    "tool_input",
                    tool.call_id,
                    tool.source_line,
                    tool.thread_id,
                    tool.turn_id,
                    tool.started_at_ms,
                    tool.input_text,
                    tool,
                )
            )
        if tool.output_text:
            records.append(
                _Source(
                    "tool_output",
                    tool.call_id,
                    tool.source_line,
                    tool.thread_id,
                    tool.turn_id,
                    tool.ended_at_ms or tool.started_at_ms,
                    _flatten_output(tool.output_text),
                    tool,
                )
            )
    return tuple(sorted(records, key=lambda item: (item.timestamp_ms, item.source_kind, item.source_id)))


def _repo_from_slug(slug: str) -> _UrlIdentity:
    url = f"https://github.com/{slug}"
    return _UrlIdentity(ArtifactKind.REPOSITORY, url, slug, slug, url, slug)


def _source_repositories(source: _Source) -> tuple[_UrlIdentity, ...]:
    found: dict[str, _UrlIdentity] = {}
    inspect_urls = source.source_kind == "source_metadata"
    if source.tool is not None and source.source_kind == "tool_output":
        inspect_urls = any(
            (
                _command_has(source.tool, "git", "remote", "get-url"),
                _command_has(source.tool, "git", "remote", "-v"),
                _command_has(source.tool, "git", "config", "--get", "remote.origin.url"),
            )
        )
    if inspect_urls:
        for match in _HTTP_URL.finditer(source.text):
            identity = _repository_identity(match.group(0))
            if identity is not None:
                found[identity.url] = identity
        for match in _SSH_REMOTE.finditer(source.text):
            identity = _repository_identity(match.group(0))
            if identity is not None:
                found[identity.url] = identity
    if source.tool is not None and source.source_kind == "tool_input":
        for raw_segment in _shell_segments(_tool_command(source.tool)):
            segment = _executable_tokens(raw_segment)
            if segment[:1] not in (("gh",), ("glab",)):
                continue
            for index, token in enumerate(segment[1:], start=1):
                slug: str | None = None
                if token in ("-R", "--repo") and index + 1 < len(segment):
                    slug = segment[index + 1]
                elif token.startswith("--repo="):
                    slug = token.partition("=")[2]
                if slug is not None and re.fullmatch(
                    r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", slug
                ):
                    identity = _repo_from_slug(slug)
                    found[identity.url] = identity
    return tuple(sorted(found.values(), key=lambda item: item.url))


def _path_repository(
    workdir: str | None, repositories_by_path: dict[str, set[str]]
) -> str | None:
    if workdir is None:
        return None
    path = PurePosixPath(workdir)
    candidates: list[tuple[int, str]] = []
    for base, urls in repositories_by_path.items():
        base_path = PurePosixPath(base)
        if path == base_path or base_path in path.parents:
            candidates.extend((len(base_path.parts), url) for url in urls)
    if not candidates:
        return None
    max_depth = max(depth for depth, _ in candidates)
    urls = {url for depth, url in candidates if depth == max_depth}
    return next(iter(urls)) if len(urls) == 1 else None


def _context_maps(
    records: tuple[_Source, ...], team: TeamData
) -> tuple[dict[str, set[str]], dict[str, set[str]]]:
    repositories_by_path: dict[str, set[str]] = {}
    repositories_by_thread: dict[str, set[str]] = {}
    for snapshot in team.sources:
        if snapshot.repository_url is None:
            continue
        identity = _repository_identity(snapshot.repository_url)
        if identity is None:
            continue
        repositories_by_thread.setdefault(snapshot.thread_id, set()).add(identity.url)
        if snapshot.working_directory:
            workdir = snapshot.working_directory.rstrip("/") or "/"
            repositories_by_path.setdefault(workdir, set()).add(identity.url)
    for source in records:
        if not (
            source.tool is not None
            and source.source_kind == "tool_output"
            and any(
                (
                    _command_has(source.tool, "git", "remote", "get-url"),
                    _command_has(source.tool, "git", "remote", "-v"),
                    _command_has(
                        source.tool,
                        "git",
                        "config",
                        "--get",
                        "remote.origin.url",
                    ),
                )
            )
        ):
            continue
        repositories = _source_repositories(source)
        if not repositories:
            continue
        urls = {item.url for item in repositories}
        if source.tool is not None:
            tool_workdir = _tool_workdir(source.tool)
            if (
                tool_workdir is not None
                and tool_workdir not in repositories_by_path
                and len(urls) == 1
            ):
                repositories_by_path[tool_workdir] = set(urls)
    return repositories_by_path, repositories_by_thread


def _context_repository(
    source: _Source,
    by_path: dict[str, set[str]],
    by_thread: dict[str, set[str]],
) -> str | None:
    if source.tool is not None:
        repository = _path_repository(_tool_workdir(source.tool), by_path)
        if repository is not None:
            return repository
    urls = by_thread.get(source.thread_id, set())
    return next(iter(urls)) if len(urls) == 1 else None


def _commit_identity(repository_url: str | None, revision: str) -> _UrlIdentity:
    revision = revision.lower()
    if repository_url is None:
        url = f"urn:git-commit:{revision}"
        return _UrlIdentity(ArtifactKind.COMMIT, url, f"Git commit {revision[:12]}", revision, None, None)
    repository = _repository_identity(repository_url)
    if repository is None:
        url = f"urn:git-commit:{revision}"
        return _UrlIdentity(ArtifactKind.COMMIT, url, f"Git commit {revision[:12]}", revision, None, None)
    url = f"{repository.url}/commit/{revision}"
    return _UrlIdentity(
        ArtifactKind.COMMIT,
        url,
        f"{repository.project_slug} commit {revision[:12]}",
        revision,
        repository.url,
        repository.project_slug,
    )


def _produced_commits(
    records: tuple[_Source, ...],
    builders: dict[str, _ArtifactBuilder],
    by_path: dict[str, set[str]],
    by_thread: dict[str, set[str]],
) -> None:
    last_commit: dict[tuple[str, str | None], _UrlIdentity] = {}
    for source in records:
        tool = source.tool
        if tool is None or source.source_kind != "tool_output":
            continue
        workdir = _tool_workdir(tool)
        context_key = (source.thread_id, workdir)
        repository_url = _context_repository(source, by_path, by_thread)
        if _command_has(tool, "git", "commit") and _successful(tool):
            for match in _COMMIT_OUTPUT.finditer(source.text):
                revision = match.group("sha").lower()
                full_matches = {
                    value.lower()
                    for value in _FULL_SHA.findall(source.text)
                    if value.lower().startswith(revision)
                }
                if len(full_matches) == 1:
                    revision = next(iter(full_matches))
                identity = _commit_identity(repository_url, revision)
                evidence = _evidence(
                    source,
                    identity,
                    EvidenceRelation.PRODUCED,
                    "committed",
                    EvidenceConfidence.HIGH if repository_url else EvidenceConfidence.MEDIUM,
                    f"{revision} {match.group('title')}",
                )
                _add(builders, identity, evidence, match.group("title").strip())
                last_commit[context_key] = identity
        if _command_has(tool, "git", "push") and _push_succeeded(tool):
            pushed_identity = last_commit.get(context_key)
            if pushed_identity is None:
                full = _FULL_SHA.findall(source.text)
                if len(set(value.lower() for value in full)) == 1:
                    pushed_identity = _commit_identity(repository_url, full[0])
            if pushed_identity is not None:
                evidence = _evidence(
                    source,
                    pushed_identity,
                    EvidenceRelation.PUBLISHED,
                    "pushed",
                    EvidenceConfidence.HIGH if repository_url else EvidenceConfidence.MEDIUM,
                    pushed_identity.external_id or pushed_identity.url,
                )
                _add(builders, pushed_identity, evidence)


def _contextual_references(
    source: _Source,
    repository_url: str | None,
) -> tuple[_UrlIdentity, ...]:
    found: dict[str, _UrlIdentity] = {}
    for match in _QUALIFIED_PR.finditer(source.text):
        repo = _repo_from_slug(match.group("slug"))
        number = match.group("number")
        identity = _UrlIdentity(
            ArtifactKind.PULL_REQUEST,
            f"{repo.url}/pull/{number}",
            f"{repo.project_slug} PR #{number}",
            number,
            repo.url,
            repo.project_slug,
        )
        found[_artifact_key(identity)] = identity
    repository = _repository_identity(repository_url) if repository_url is not None else None
    if repository is not None:
        for match in _CONTEXT_PR.finditer(source.text):
            number = match.group("number")
            identity = _UrlIdentity(
                ArtifactKind.PULL_REQUEST,
                f"{repository.url}/pull/{number}",
                f"{repository.project_slug} PR #{number}",
                number,
                repository.url,
                repository.project_slug,
            )
            found[_artifact_key(identity)] = identity
        for match in _CONTEXT_ISSUE.finditer(source.text):
            number = match.group("number")
            identity = _UrlIdentity(
                ArtifactKind.ISSUE,
                f"{repository.url}/issues/{number}",
                f"{repository.project_slug} issue #{number}",
                number,
                repository.url,
                repository.project_slug,
            )
            found[_artifact_key(identity)] = identity
    return tuple(found.values())


def extract_artifacts(team: TeamData) -> ArtifactCatalog:
    """Extract a deterministic catalog from normalized events and unredacted tool records."""

    records = _source_records(team)
    by_path, by_thread = _context_maps(records, team)
    builders: dict[str, _ArtifactBuilder] = {}

    for source in records:
        for repository in _source_repositories(source):
            evidence = _evidence(
                source,
                repository,
                EvidenceRelation.REFERENCED,
                "repository_observed",
                EvidenceConfidence.HIGH,
                repository.url,
            )
            _add(builders, repository, evidence)

        seen_urls: set[str] = set()
        for match in _HTTP_URL.finditer(source.text):
            identity = _url_identity(match.group(0))
            if identity is None or identity.url in seen_urls:
                continue
            seen_urls.add(identity.url)
            relation, action = _url_action(source, identity)
            # Ordinary prompts and command output contain large numbers of documentation and
            # policy links. Those remain linkifiable in transcripts, but they are not work
            # artifacts. Admit a generic URL only when successful upload evidence establishes it
            # as an output.
            if identity.kind is ArtifactKind.URL and relation not in (
                EvidenceRelation.PRODUCED,
                EvidenceRelation.PUBLISHED,
            ):
                continue
            evidence = _evidence(
                source,
                identity,
                relation,
                action,
                EvidenceConfidence.HIGH,
                identity.url,
            )
            title = (
                _command_title(source.tool)
                if source.tool is not None
                and relation in (EvidenceRelation.PRODUCED, EvidenceRelation.UPDATED)
                else None
            )
            _add(builders, identity, evidence, title)
            if (
                relation is not EvidenceRelation.REFERENCED
                and identity.project_url is not None
            ):
                project_identity = _repository_identity(identity.project_url)
                if project_identity is not None:
                    project_evidence = _evidence(
                        source,
                        project_identity,
                        EvidenceRelation.REFERENCED,
                        "project_of_output",
                        EvidenceConfidence.HIGH,
                        project_identity.url,
                    )
                    _add(builders, project_identity, project_evidence)

        context = _context_repository(source, by_path, by_thread)
        for identity in _contextual_references(source, context):
            evidence = _evidence(
                source,
                identity,
                EvidenceRelation.REFERENCED,
                "mentioned",
                EvidenceConfidence.MEDIUM,
                identity.label,
            )
            _add(builders, identity, evidence)

    _produced_commits(records, builders, by_path, by_thread)

    artifacts: list[WorkArtifact] = []
    for builder in builders.values():
        ordered_evidence = tuple(
            sorted(
                builder.evidence.values(),
                key=lambda item: (item.timestamp_ms, item.source_kind, item.evidence_id),
            )
        )
        produced = [
            item
            for item in ordered_evidence
            if item.relation in (EvidenceRelation.PRODUCED, EvidenceRelation.PUBLISHED)
        ]
        producer = min(produced, key=lambda item: (item.timestamp_ms, item.evidence_id)) if produced else None
        identity = builder.identity
        link_url: str | None = None if identity.url.startswith("urn:") else identity.url
        if identity.kind is ArtifactKind.COMMIT:
            link_proven = any(
                item.relation is EvidenceRelation.PUBLISHED
                or (
                    item.relation is EvidenceRelation.REFERENCED
                    and item.matched_text == identity.url
                )
                for item in ordered_evidence
            )
            if not link_proven:
                link_url = None
        artifacts.append(
            WorkArtifact(
                artifact_id=_artifact_id(identity),
                kind=identity.kind,
                locator=identity.url,
                url=link_url,
                label=identity.label,
                title=builder.title,
                external_id=identity.external_id,
                project_url=identity.project_url,
                project_slug=identity.project_slug,
                producer_thread_id=producer.thread_id if producer is not None else None,
                produced_at_ms=producer.timestamp_ms if producer is not None else None,
                evidence=ordered_evidence,
            )
        )
    artifacts.sort(
        key=lambda item: (
            item.produced_at_ms if item.produced_at_ms is not None else 2**63 - 1,
            item.kind.value,
            item.locator,
        )
    )

    project_evidence_by_url: dict[str, set[str]] = {}
    for artifact in artifacts:
        if artifact.kind is not ArtifactKind.REPOSITORY or artifact.project_url is None:
            continue
        strong_evidence = {
            item.evidence_id
            for item in artifact.evidence
            if item.action in ("repository_observed", "project_of_output")
        }
        if strong_evidence:
            project_evidence_by_url.setdefault(artifact.project_url, set()).update(
                strong_evidence
            )
    projects: list[ProjectIdentity] = []
    for url, evidence_ids in project_evidence_by_url.items():
        identity = _repository_identity(url)
        if identity is None or identity.project_slug is None:
            continue
        host = urlsplit(url).hostname or ""
        projects.append(
            ProjectIdentity(
                project_id=_hash_id("project-", url),
                host=host,
                slug=identity.project_slug,
                url=url,
                evidence_ids=tuple(sorted(evidence_ids)),
            )
        )
    projects.sort(key=lambda item: (item.host, item.slug))
    return ArtifactCatalog(
        source_digest=source_digest(team),
        artifacts=tuple(artifacts),
        projects=tuple(projects),
    )


def _optional_string(value: JsonValue, where: str) -> str | None:
    if value is None:
        return None
    return as_string(value, where)


def artifact_catalog_from_json(value: JsonValue) -> ArtifactCatalog:
    """Strictly load a catalog and reject malformed or unstable identifiers."""

    root = as_object(value, "artifact catalog")
    schema_version = as_int(root.get("schema_version"), "artifact catalog.schema_version")
    if schema_version != 1:
        raise ValueError(f"unsupported artifact catalog schema {schema_version}")
    extractor_version = as_string(
        root.get("extractor_version"), "artifact catalog.extractor_version"
    )
    artifacts: list[WorkArtifact] = []
    for index, raw_artifact in enumerate(
        as_array(root.get("artifacts"), "artifact catalog.artifacts")
    ):
        where = f"artifact catalog.artifacts[{index}]"
        item = as_object(raw_artifact, where)
        evidence: list[ArtifactEvidence] = []
        for evidence_index, raw_evidence in enumerate(
            as_array(item.get("evidence"), f"{where}.evidence")
        ):
            evidence_where = f"{where}.evidence[{evidence_index}]"
            evidence_item = as_object(raw_evidence, evidence_where)
            evidence.append(
                ArtifactEvidence(
                    evidence_id=as_string(evidence_item.get("evidence_id"), f"{evidence_where}.evidence_id"),
                    source_kind=as_string(evidence_item.get("source_kind"), f"{evidence_where}.source_kind"),
                    source_id=as_string(evidence_item.get("source_id"), f"{evidence_where}.source_id"),
                    source_line=as_int(evidence_item.get("source_line"), f"{evidence_where}.source_line"),
                    thread_id=as_string(evidence_item.get("thread_id"), f"{evidence_where}.thread_id"),
                    turn_id=_optional_string(evidence_item.get("turn_id"), f"{evidence_where}.turn_id"),
                    timestamp_ms=as_int(evidence_item.get("timestamp_ms"), f"{evidence_where}.timestamp_ms"),
                    relation=EvidenceRelation(as_string(evidence_item.get("relation"), f"{evidence_where}.relation")),
                    action=as_string(evidence_item.get("action"), f"{evidence_where}.action"),
                    confidence=EvidenceConfidence(as_string(evidence_item.get("confidence"), f"{evidence_where}.confidence")),
                    matched_text=as_string(evidence_item.get("matched_text"), f"{evidence_where}.matched_text"),
                    extractor=as_string(evidence_item.get("extractor"), f"{evidence_where}.extractor"),
                )
            )
        kind = ArtifactKind(as_string(item.get("kind"), f"{where}.kind"))
        locator = as_string(item.get("locator"), f"{where}.locator")
        identity = _UrlIdentity(
            kind=kind,
            url=locator,
            label=as_string(item.get("label"), f"{where}.label"),
            external_id=_optional_string(item.get("external_id"), f"{where}.external_id"),
            project_url=_optional_string(item.get("project_url"), f"{where}.project_url"),
            project_slug=_optional_string(item.get("project_slug"), f"{where}.project_slug"),
        )
        if len({record.evidence_id for record in evidence}) != len(evidence):
            raise ValueError(f"{where}: duplicate evidence IDs")
        for record in evidence:
            expected_evidence_id = _evidence_id(
                record.source_kind,
                record.source_id,
                record.source_line,
                identity,
                record.relation,
                record.action,
            )
            if record.evidence_id != expected_evidence_id:
                raise ValueError(
                    f"{where}: evidence_id does not match its source and artifact"
                )
        artifact_id = as_string(item.get("artifact_id"), f"{where}.artifact_id")
        if artifact_id != _artifact_id(identity):
            raise ValueError(f"{where}: artifact_id does not match kind and URL")
        link_url = _optional_string(item.get("url"), f"{where}.url")
        allowed_urls: tuple[str | None, ...] = (
            (None, locator)
            if kind is ArtifactKind.COMMIT and not locator.startswith("urn:")
            else (None,)
            if locator.startswith("urn:")
            else (locator,)
        )
        if link_url not in allowed_urls:
            raise ValueError(f"{where}: URL does not match its stable locator")
        artifacts.append(
            WorkArtifact(
                artifact_id=artifact_id,
                kind=kind,
                locator=identity.url,
                url=link_url,
                label=identity.label,
                title=_optional_string(item.get("title"), f"{where}.title"),
                external_id=identity.external_id,
                project_url=identity.project_url,
                project_slug=identity.project_slug,
                producer_thread_id=_optional_string(item.get("producer_thread_id"), f"{where}.producer_thread_id"),
                produced_at_ms=(None if item.get("produced_at_ms") is None else as_int(item.get("produced_at_ms"), f"{where}.produced_at_ms")),
                evidence=tuple(evidence),
            )
        )
    projects: list[ProjectIdentity] = []
    for index, raw_project in enumerate(
        as_array(root.get("projects"), "artifact catalog.projects")
    ):
        where = f"artifact catalog.projects[{index}]"
        item = as_object(raw_project, where)
        url = as_string(item.get("url"), f"{where}.url")
        project_id = as_string(item.get("project_id"), f"{where}.project_id")
        if project_id != _hash_id("project-", url):
            raise ValueError(f"{where}: project_id does not match URL")
        projects.append(
            ProjectIdentity(
                project_id=project_id,
                host=as_string(item.get("host"), f"{where}.host"),
                slug=as_string(item.get("slug"), f"{where}.slug"),
                url=url,
                evidence_ids=tuple(
                    as_string(raw_id, f"{where}.evidence_ids[]")
                    for raw_id in as_array(item.get("evidence_ids"), f"{where}.evidence_ids")
                ),
            )
        )
    return ArtifactCatalog(
        schema_version=schema_version,
        extractor_version=extractor_version,
        source_digest=as_string(root.get("source_digest"), "artifact catalog.source_digest"),
        artifacts=tuple(artifacts),
        projects=tuple(projects),
    )


def artifact_ids_for_range(
    catalog: ArtifactCatalog,
    start_ms: int,
    end_ms: int,
    thread_id: str | None = None,
) -> tuple[str, ...]:
    """Return stable, deduplicated IDs evidenced in a half-open time range."""

    result = {
        artifact.artifact_id
        for artifact in catalog.artifacts
        if any(
            start_ms <= evidence.timestamp_ms < end_ms
            and (thread_id is None or evidence.thread_id == thread_id)
            for evidence in artifact.evidence
        )
    }
    return tuple(sorted(result))


def output_artifact_ids_for_range(
    catalog: ArtifactCatalog,
    start_ms: int,
    end_ms: int,
    thread_id: str | None = None,
) -> tuple[str, ...]:
    """Return only artifacts with output-changing evidence in a half-open range."""

    result = {
        artifact.artifact_id
        for artifact in catalog.artifacts
        if any(
            evidence.relation in _OUTPUT_RELATIONS
            and start_ms <= evidence.timestamp_ms < end_ms
            and (thread_id is None or evidence.thread_id == thread_id)
            for evidence in artifact.evidence
        )
    }
    return tuple(sorted(result))


__all__ = [
    "ArtifactCatalog",
    "ArtifactEvidence",
    "ArtifactKind",
    "ArtifactRangeIndex",
    "EvidenceConfidence",
    "EvidenceRelation",
    "ProjectIdentity",
    "WorkArtifact",
    "artifact_catalog_from_json",
    "artifact_ids_for_range",
    "canonical_repository_url",
    "extract_artifacts",
    "output_artifact_ids_for_range",
]
