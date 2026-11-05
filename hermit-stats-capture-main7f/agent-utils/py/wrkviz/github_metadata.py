"""Strict, bounded cache records for GitHub pull-request metadata.

The cache deliberately stores presentation metadata only.  Authentication is
accepted by :func:`fetch_pull_request` for a single request and is never part of
the record or its JSON representation.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime, timezone
import http.client
import json
import math
from pathlib import Path
import re
from typing import Protocol
from urllib.parse import urlsplit

from wrkviz.archive import JsonValue, read_json, write_json_if_changed


SCHEMA_VERSION = 1
MAX_API_RESPONSE_BYTES = 2 * 1024 * 1024
MAX_BODY_EXCERPT_CHARS = 600
MAX_ETAG_CHARS = 1_024
DEFAULT_TIMEOUT_SECONDS = 15.0

_OWNER_TEXT = r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?"
_REPOSITORY_TEXT = r"[A-Za-z0-9_.-]{1,100}"
_REPOSITORY = re.compile(rf"(?P<owner>{_OWNER_TEXT})/(?P<name>{_REPOSITORY_TEXT})\Z")
_API_PATH = re.compile(rf"/repos/{_OWNER_TEXT}/{_REPOSITORY_TEXT}/pulls/[1-9][0-9]*\Z")


class GitHubMetadataError(RuntimeError):
    """Raised when GitHub metadata cannot be fetched or decoded safely."""


class GitHubHttpError(GitHubMetadataError):
    """A non-success response from the GitHub API."""

    def __init__(self, status: int) -> None:
        self.status = status
        super().__init__(f"GitHub API returned HTTP {status}")


@dataclass(frozen=True, slots=True, order=True)
class PullRequestKey:
    """A validated repository-and-number cache key."""

    repository: str
    number: int

    def __post_init__(self) -> None:
        match = _REPOSITORY.fullmatch(self.repository)
        if match is None or match.group("name") in {".", ".."}:
            raise ValueError(
                "GitHub repository must be an owner/repository slug: "
                f"{self.repository!r}"
            )
        if self.number <= 0:
            raise ValueError("a GitHub pull-request number must be positive")

    @property
    def cache_key(self) -> str:
        """Return the stable spelling used as a JSON object key."""

        return f"{self.repository}#{self.number}"

    @property
    def api_url(self) -> str:
        """Return the sole GitHub API endpoint used for this key."""

        return f"https://api.github.com/repos/{self.repository}/pulls/{self.number}"


def _valid_header_value(value: str, name: str, maximum: int) -> str:
    if not value or len(value) > maximum or "\r" in value or "\n" in value:
        raise ValueError(f"invalid {name}")
    return value


@dataclass(frozen=True, slots=True)
class PullRequestMetadata:
    """The bounded subset of a GitHub pull response needed by the UI."""

    key: PullRequestKey
    title: str
    state: str
    draft: bool
    merged_at: str | None
    body_excerpt: str
    base_ref: str
    head_label: str
    author: str | None
    updated_at: str
    etag: str | None
    fetched_at: str

    def __post_init__(self) -> None:
        if not self.title:
            raise ValueError("pull-request title must not be empty")
        if self.state not in {"open", "closed"}:
            raise ValueError("pull-request state must be 'open' or 'closed'")
        if len(self.body_excerpt) > MAX_BODY_EXCERPT_CHARS:
            raise ValueError("pull-request body excerpt is too long")
        for name, value in (
            ("base_ref", self.base_ref),
            ("head_label", self.head_label),
            ("updated_at", self.updated_at),
            ("fetched_at", self.fetched_at),
        ):
            if not value:
                raise ValueError(f"pull-request {name} must not be empty")
        if self.etag is not None:
            _valid_header_value(self.etag, "ETag", MAX_ETAG_CHARS)

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return a stable JSON-compatible object; credentials cannot appear."""

        return {
            "repository": self.key.repository,
            "number": self.key.number,
            "title": self.title,
            "state": self.state,
            "draft": self.draft,
            "merged_at": self.merged_at,
            "body_excerpt": self.body_excerpt,
            "base_ref": self.base_ref,
            "head_label": self.head_label,
            "author": self.author,
            "updated_at": self.updated_at,
            "etag": self.etag,
            "fetched_at": self.fetched_at,
        }

    @classmethod
    def from_json_obj(
        cls, value: object, where: str = "pull metadata"
    ) -> PullRequestMetadata:
        """Load one record while narrowing every untyped JSON value."""

        item = _object(value, where)
        _exact_keys(
            item,
            {
                "repository",
                "number",
                "title",
                "state",
                "draft",
                "merged_at",
                "body_excerpt",
                "base_ref",
                "head_label",
                "author",
                "updated_at",
                "etag",
                "fetched_at",
            },
            where,
        )
        return cls(
            key=PullRequestKey(
                repository=_string(item["repository"], where + ".repository"),
                number=_integer(item["number"], where + ".number"),
            ),
            title=_string(item["title"], where + ".title"),
            state=_string(item["state"], where + ".state"),
            draft=_boolean(item["draft"], where + ".draft"),
            merged_at=_optional_string(item["merged_at"], where + ".merged_at"),
            body_excerpt=_string(item["body_excerpt"], where + ".body_excerpt"),
            base_ref=_string(item["base_ref"], where + ".base_ref"),
            head_label=_string(item["head_label"], where + ".head_label"),
            author=_optional_string(item["author"], where + ".author"),
            updated_at=_string(item["updated_at"], where + ".updated_at"),
            etag=_optional_string(item["etag"], where + ".etag"),
            fetched_at=_string(item["fetched_at"], where + ".fetched_at"),
        )


class PullRequestMetadataCache:
    """In-memory records keyed by :class:`PullRequestKey`."""

    def __init__(self, records: tuple[PullRequestMetadata, ...] = ()) -> None:
        self._records: dict[PullRequestKey, PullRequestMetadata] = {}
        for record in records:
            self.put(record)

    def get(self, key: PullRequestKey) -> PullRequestMetadata | None:
        """Return the cached record for ``key``, if present."""

        return self._records.get(key)

    def put(self, record: PullRequestMetadata) -> None:
        """Insert or replace one record by its validated key."""

        self._records[record.key] = record

    @property
    def records(self) -> tuple[PullRequestMetadata, ...]:
        """Return every record in deterministic key order."""

        return tuple(self._records[key] for key in sorted(self._records))

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the complete versioned cache as a JSON-compatible object."""

        records: dict[str, JsonValue] = {
            record.key.cache_key: record.to_json_obj() for record in self.records
        }
        return {"schema_version": SCHEMA_VERSION, "records": records}

    @classmethod
    def from_json_obj(cls, value: object) -> PullRequestMetadataCache:
        """Validate and load a complete versioned cache object."""

        root = _object(value, "GitHub metadata cache")
        _exact_keys(root, {"schema_version", "records"}, "GitHub metadata cache")
        version = _integer(
            root["schema_version"], "GitHub metadata cache.schema_version"
        )
        if version != SCHEMA_VERSION:
            raise ValueError(f"unsupported GitHub metadata cache schema {version}")
        raw_records = _object(root["records"], "GitHub metadata cache.records")
        records: list[PullRequestMetadata] = []
        for cache_key in sorted(raw_records):
            record = PullRequestMetadata.from_json_obj(
                raw_records[cache_key], f"GitHub metadata cache.records[{cache_key!r}]"
            )
            if cache_key != record.key.cache_key:
                raise ValueError(
                    "GitHub metadata cache key does not match its record: "
                    f"{cache_key!r} != {record.key.cache_key!r}"
                )
            records.append(record)
        return cls(tuple(records))


def load_pull_request_metadata_cache(path: Path) -> PullRequestMetadataCache:
    """Load a cache, treating an absent file as an empty first run."""

    if not path.is_file():
        return PullRequestMetadataCache()
    return PullRequestMetadataCache.from_json_obj(read_json(path))


def save_pull_request_metadata_cache(
    path: Path, cache: PullRequestMetadataCache
) -> bool:
    """Atomically save deterministic JSON, returning whether bytes changed."""

    return write_json_if_changed(path, cache.to_json_obj())


@dataclass(frozen=True, slots=True)
class HttpResponse:
    """Small transport-neutral HTTP response used by the fetcher."""

    status: int
    headers: Mapping[str, str]
    body: bytes


class HttpTransport(Protocol):
    """Injectable HTTP seam for deterministic tests."""

    def get(
        self, *, url: str, headers: Mapping[str, str], timeout_seconds: float
    ) -> HttpResponse:
        """Fetch one URL without following it to another host."""


def _validate_api_url(url: str) -> str:
    parsed = urlsplit(url)
    if (
        parsed.scheme != "https"
        or parsed.netloc != "api.github.com"
        or _API_PATH.fullmatch(parsed.path) is None
        or parsed.query
        or parsed.fragment
        or parsed.username is not None
        or parsed.password is not None
    ):
        raise ValueError(
            "GitHub metadata requests must use https://api.github.com/repos/"
        )
    return parsed.path


class StandardLibraryHttpTransport:
    """Direct HTTPS transport with a hard response-size limit."""

    def get(
        self, *, url: str, headers: Mapping[str, str], timeout_seconds: float
    ) -> HttpResponse:
        """Fetch one bounded response from the exact GitHub pull endpoint."""

        path = _validate_api_url(url)
        connection = http.client.HTTPSConnection(
            "api.github.com", timeout=timeout_seconds
        )
        try:
            try:
                connection.request("GET", path, headers=dict(headers))
                response = connection.getresponse()
                body = response.read(MAX_API_RESPONSE_BYTES + 1)
                if len(body) > MAX_API_RESPONSE_BYTES:
                    raise GitHubMetadataError("GitHub API response exceeds size limit")
                response_headers = {
                    name.lower(): value for name, value in response.getheaders()
                }
                return HttpResponse(
                    status=response.status, headers=response_headers, body=body
                )
            except http.client.HTTPException as error:
                raise GitHubMetadataError("GitHub API HTTP protocol failure") from error
        finally:
            connection.close()


@dataclass(frozen=True, slots=True)
class PullRequestFetchResult:
    """A fetched record plus whether GitHub returned HTTP 304."""

    metadata: PullRequestMetadata
    not_modified: bool


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def fetch_pull_request(
    key: PullRequestKey,
    *,
    cached: PullRequestMetadata | None = None,
    token: str | None = None,
    fetched_at: str | None = None,
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
    transport: HttpTransport | None = None,
) -> PullRequestFetchResult:
    """Fetch one PR, conditionally using a cached ETag when available.

    A 304 response returns the cached record unchanged, including ``fetched_at``,
    so a conditional rerun is byte-idempotent.  The optional bearer token is
    placed solely in the request headers and is not retained by the result.
    """

    if cached is not None and cached.key != key:
        raise ValueError("cached pull-request metadata has a different key")
    if not math.isfinite(timeout_seconds) or timeout_seconds <= 0.0:
        raise ValueError("HTTP timeout must be finite and positive")
    fetched = fetched_at if fetched_at is not None else _utc_now()
    if not fetched:
        raise ValueError("fetched_at must not be empty")

    headers: dict[str, str] = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "wrkviz",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if cached is not None and cached.etag is not None:
        headers["If-None-Match"] = cached.etag
    if token is not None:
        headers["Authorization"] = "Bearer " + _valid_header_value(
            token, "GitHub bearer token", 4_096
        )

    url = key.api_url
    _validate_api_url(url)
    client = transport if transport is not None else StandardLibraryHttpTransport()
    response = client.get(url=url, headers=headers, timeout_seconds=timeout_seconds)
    if len(response.body) > MAX_API_RESPONSE_BYTES:
        raise GitHubMetadataError("GitHub API response exceeds size limit")
    if response.status == 304:
        if cached is None:
            raise GitHubMetadataError("GitHub returned 304 without a cached record")
        return PullRequestFetchResult(metadata=cached, not_modified=True)
    if response.status != 200:
        raise GitHubHttpError(response.status)

    try:
        metadata = _metadata_from_response(key, response, fetched)
    except ValueError as error:
        raise GitHubMetadataError(
            "GitHub API returned malformed pull metadata"
        ) from error
    return PullRequestFetchResult(metadata=metadata, not_modified=False)


def _metadata_from_response(
    key: PullRequestKey, response: HttpResponse, fetched_at: str
) -> PullRequestMetadata:
    try:
        text = response.body.decode("utf-8")
        raw: object = json.loads(text)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GitHubMetadataError("GitHub API returned invalid JSON") from error
    root = _object(raw, "GitHub pull response")
    body_value = root.get("body")
    body = (
        "" if body_value is None else _string(body_value, "GitHub pull response.body")
    )
    base = _object(root.get("base"), "GitHub pull response.base")
    head = _object(root.get("head"), "GitHub pull response.head")
    user_value = root.get("user")
    author = (
        None
        if user_value is None
        else _string(
            _object(user_value, "GitHub pull response.user").get("login"),
            "GitHub pull response.user.login",
        )
    )
    return PullRequestMetadata(
        key=key,
        title=_string(root.get("title"), "GitHub pull response.title"),
        state=_string(root.get("state"), "GitHub pull response.state"),
        draft=_boolean(root.get("draft"), "GitHub pull response.draft"),
        merged_at=_optional_string(
            root.get("merged_at"), "GitHub pull response.merged_at"
        ),
        body_excerpt=body[:MAX_BODY_EXCERPT_CHARS],
        base_ref=_string(base.get("ref"), "GitHub pull response.base.ref"),
        head_label=_string(head.get("label"), "GitHub pull response.head.label"),
        author=author,
        updated_at=_string(root.get("updated_at"), "GitHub pull response.updated_at"),
        etag=_response_header(response.headers, "etag"),
        fetched_at=fetched_at,
    )


def _response_header(headers: Mapping[str, str], name: str) -> str | None:
    for header_name, value in headers.items():
        if header_name.lower() == name:
            return _valid_header_value(value, name, MAX_ETAG_CHARS)
    return None


def _object(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ValueError(f"{where}: expected an object")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise ValueError(f"{where}: object key is not a string")
        result[key] = item
    return result


def _string(value: object, where: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{where}: expected a string")
    return value


def _optional_string(value: object, where: str) -> str | None:
    if value is None:
        return None
    return _string(value, where)


def _integer(value: object, where: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{where}: expected an integer")
    return value


def _boolean(value: object, where: str) -> bool:
    if not isinstance(value, bool):
        raise ValueError(f"{where}: expected a boolean")
    return value


def _exact_keys(value: Mapping[str, object], expected: set[str], where: str) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        details: list[str] = []
        if missing:
            details.append("missing " + ", ".join(missing))
        if extra:
            details.append("extra " + ", ".join(extra))
        raise ValueError(f"{where}: " + "; ".join(details))


__all__ = [
    "DEFAULT_TIMEOUT_SECONDS",
    "GitHubHttpError",
    "GitHubMetadataError",
    "HttpResponse",
    "HttpTransport",
    "MAX_API_RESPONSE_BYTES",
    "MAX_BODY_EXCERPT_CHARS",
    "PullRequestFetchResult",
    "PullRequestKey",
    "PullRequestMetadata",
    "PullRequestMetadataCache",
    "StandardLibraryHttpTransport",
    "fetch_pull_request",
    "load_pull_request_metadata_cache",
    "save_pull_request_metadata_cache",
]
