"""Evidence-bounded project and execution-host identity extraction."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import PurePath
import re
from urllib.parse import urlsplit, urlunsplit
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from wrkviz.archive import JsonValue, as_array, as_object, as_string


JsonObject = dict[str, JsonValue]


_SCP_REMOTE = re.compile(
    r"(?:[^@/\s]+@)?(?P<host>[A-Za-z0-9.-]+):(?P<path>[^\s]+)\Z"
)
_HOSTNAME = re.compile(
    r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}))*\Z"
)


@dataclass(frozen=True)
class ProjectIdentity:
    """One repository/project associated with an agent team."""

    label: str
    repository_url: str | None
    primary: bool
    source: str

    def to_json_obj(self) -> JsonObject:
        """Return this project identity as a JSON object."""

        return {
            "label": self.label,
            "repository_url": self.repository_url,
            "primary": self.primary,
            "source": self.source,
        }


@dataclass(frozen=True)
class HostIdentity:
    """One machine on which an agent team ran."""

    hostname: str
    source: str

    def to_json_obj(self) -> JsonObject:
        """Return this host identity as a JSON object."""

        return {"hostname": self.hostname, "source": self.source}


@dataclass(frozen=True)
class SiteIdentity:
    """Durable display identity kept separately from transcript normalization."""

    team_slug: str
    projects: tuple[ProjectIdentity, ...]
    hosts: tuple[HostIdentity, ...]
    display_timezone: str
    display_timezone_source: str

    def to_json_obj(self) -> JsonObject:
        """Return the versioned site identity record."""

        return {
            "schema_version": 1,
            "team_slug": self.team_slug,
            "projects": [item.to_json_obj() for item in self.projects],
            "hosts": [item.to_json_obj() for item in self.hosts],
            "display_timezone": self.display_timezone,
            "display_timezone_source": self.display_timezone_source,
        }


@dataclass(frozen=True)
class IdentityOverrides:
    """Explicit ingest identity values and timezone-setting provenance."""

    projects: tuple[ProjectIdentity, ...] = ()
    hosts: tuple[HostIdentity, ...] = ()
    display_timezone_source: str = "api"


def _string(value: object) -> str | None:
    if not isinstance(value, str):
        return None
    stripped = value.strip()
    return stripped or None


def _mapping(value: object) -> Mapping[str, object]:
    if not isinstance(value, dict):
        return {}
    return {key: item for key, item in value.items() if isinstance(key, str)}


def _boolean(value: JsonValue, where: str) -> bool:
    if not isinstance(value, bool):
        raise ValueError(f"{where}: expected a boolean")
    return value


def canonical_repository_url(raw: str) -> str:
    """Normalize an HTTP(S) or Git SCP-style repository remote to a browser URL."""

    value = raw.strip()
    scp = _SCP_REMOTE.fullmatch(value)
    if scp is not None and "://" not in value:
        value = f"https://{scp.group('host')}/{scp.group('path')}"
    parsed = urlsplit(value)
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise ValueError(f"repository URL is not an HTTP(S) or Git SSH remote: {raw!r}")
    if parsed.username is not None or parsed.password is not None:
        raise ValueError("repository URL must not contain credentials")
    path = parsed.path.rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    if not path or path == "/" or any(part in (".", "..") for part in path.split("/")):
        raise ValueError(f"repository URL has no safe project path: {raw!r}")
    return urlunsplit((parsed.scheme, parsed.netloc.lower(), path, "", ""))


def project_label_from_url(repository_url: str) -> str:
    """Return a compact default label for a canonical repository URL."""

    label = urlsplit(repository_url).path.rstrip("/").rsplit("/", 1)[-1]
    if not label:
        raise ValueError(f"repository URL has no project label: {repository_url!r}")
    return label


def _validate_label(raw: str, what: str, *, maximum: int) -> str:
    value = raw.strip()
    if not value or len(value) > maximum or any(ord(char) < 32 for char in value):
        raise ValueError(f"invalid {what} {raw!r}")
    return value


def explicit_project(raw: str, *, primary: bool) -> ProjectIdentity:
    """Parse ``LABEL=REPOSITORY_URL`` supplied by a user."""

    label, separator, remote = raw.partition("=")
    if not separator:
        remote = raw
        canonical = canonical_repository_url(remote)
        label = project_label_from_url(canonical)
    else:
        label = _validate_label(label, "project label", maximum=120)
        canonical = canonical_repository_url(remote)
    return ProjectIdentity(label, canonical, primary, "explicit")


def explicit_host(raw: str) -> HostIdentity:
    """Validate one explicitly supplied execution hostname."""

    hostname = raw.strip().rstrip(".").lower()
    if len(hostname) > 253 or _HOSTNAME.fullmatch(hostname) is None:
        raise ValueError(f"invalid hostname {raw!r}")
    return HostIdentity(hostname, "explicit")


def parse_identity_overrides(
    project_values: Sequence[str], host_values: Sequence[str]
) -> tuple[tuple[ProjectIdentity, ...], tuple[HostIdentity, ...]]:
    """Parse repeatable CLI identity overrides, preserving their display order."""

    projects: list[ProjectIdentity] = []
    seen_projects: set[str] = set()
    for raw in project_values:
        project = explicit_project(raw, primary=not projects)
        key = project.repository_url or project.label.casefold()
        if key in seen_projects:
            continue
        seen_projects.add(key)
        projects.append(project)
    hosts: list[HostIdentity] = []
    seen_hosts: set[str] = set()
    for raw in host_values:
        host = explicit_host(raw)
        if host.hostname in seen_hosts:
            continue
        seen_hosts.add(host.hostname)
        hosts.append(host)
    return tuple(projects), tuple(hosts)


def _inferred_project(metadata: Mapping[str, object], *, primary: bool) -> ProjectIdentity | None:
    git = _mapping(metadata.get("git"))
    remote = _string(git.get("repository_url")) or _string(
        metadata.get("repository_url")
    )
    if remote is not None:
        try:
            canonical = canonical_repository_url(remote)
        except ValueError:
            canonical = None
        if canonical is not None:
            return ProjectIdentity(
                project_label_from_url(canonical),
                canonical,
                primary,
                "session_metadata",
            )
    cwd = _string(metadata.get("cwd"))
    if cwd is None:
        return None
    label = PurePath(cwd).name
    if not label or label in (".", "/"):
        return None
    return ProjectIdentity(label, None, primary, "session_metadata")


def _inferred_host(metadata: Mapping[str, object]) -> HostIdentity | None:
    environment = _mapping(metadata.get("environment"))
    raw = (
        _string(metadata.get("hostname"))
        or _string(metadata.get("host_name"))
        or _string(environment.get("hostname"))
    )
    if raw is None:
        return None
    try:
        validated = explicit_host(raw)
    except ValueError:
        return None
    return HostIdentity(validated.hostname, "session_metadata")


def infer_structured_identity(
    records: Sequence[Mapping[str, object]],
) -> tuple[tuple[ProjectIdentity, ...], tuple[HostIdentity, ...]]:
    """Infer identity only from structured, provider-owned metadata fields."""

    projects: list[ProjectIdentity] = []
    project_keys: set[str] = set()
    hosts: list[HostIdentity] = []
    host_keys: set[str] = set()
    for metadata in records:
        project = _inferred_project(metadata, primary=not projects)
        if project is not None:
            key = project.repository_url or project.label.casefold()
            label_match = next(
                (
                    index
                    for index, prior in enumerate(projects)
                    if prior.label.casefold() == project.label.casefold()
                    and (prior.repository_url is None or project.repository_url is None)
                ),
                None,
            )
            if label_match is not None:
                prior = projects[label_match]
                if prior.repository_url is None and project.repository_url is not None:
                    project_keys.discard(prior.label.casefold())
                    projects[label_match] = replace(
                        project, primary=prior.primary or project.primary
                    )
                    project_keys.add(key)
            elif key not in project_keys:
                project_keys.add(key)
                projects.append(project)
        host = _inferred_host(metadata)
        if host is not None and host.hostname not in host_keys:
            host_keys.add(host.hostname)
            hosts.append(host)
    return tuple(projects), tuple(hosts)


def site_identity_from_json_obj(raw: JsonValue, where: str) -> SiteIdentity:
    """Validate a standalone ``site-identity.json`` record."""

    root = as_object(raw, where)
    if root.get("schema_version") != 1:
        raise ValueError(f"{where}: unsupported site identity schema")
    project_values = as_array(root.get("projects"), where + ".projects")
    projects: list[ProjectIdentity] = []
    for index, value in enumerate(project_values):
        item_where = f"{where}.projects[{index}]"
        item = as_object(value, item_where)
        label = _validate_label(
            as_string(item.get("label"), item_where + ".label"),
            "project label",
            maximum=120,
        )
        remote_value = item.get("repository_url")
        remote = None
        if remote_value is not None:
            remote = canonical_repository_url(
                as_string(remote_value, item_where + ".repository_url")
            )
        projects.append(
            ProjectIdentity(
                label,
                remote,
                _boolean(item.get("primary"), item_where + ".primary"),
                as_string(item.get("source"), item_where + ".source"),
            )
        )
    host_values = as_array(root.get("hosts"), where + ".hosts")
    hosts: list[HostIdentity] = []
    for index, value in enumerate(host_values):
        item_where = f"{where}.hosts[{index}]"
        item = as_object(value, item_where)
        validated = explicit_host(
            as_string(item.get("hostname"), item_where + ".hostname")
        )
        hosts.append(
            HostIdentity(
                validated.hostname,
                as_string(item.get("source"), item_where + ".source"),
            )
        )
    timezone_name = as_string(
        root.get("display_timezone"), where + ".display_timezone"
    )
    try:
        ZoneInfo(timezone_name)
    except ZoneInfoNotFoundError as error:
        raise ValueError(f"{where}: unknown IANA timezone {timezone_name!r}") from error
    primary_count = sum(project.primary for project in projects)
    if projects and primary_count != 1:
        raise ValueError(f"{where}: projects must contain exactly one primary entry")
    return SiteIdentity(
        team_slug=as_string(root.get("team_slug"), where + ".team_slug"),
        projects=tuple(projects),
        hosts=tuple(hosts),
        display_timezone=timezone_name,
        display_timezone_source=as_string(
            root.get("display_timezone_source"),
            where + ".display_timezone_source",
        ),
    )


def merge_site_identity(
    team_slug: str,
    display_timezone: str,
    display_timezone_source: str,
    inferred_projects: Sequence[ProjectIdentity],
    inferred_hosts: Sequence[HostIdentity],
    explicit_projects: Sequence[ProjectIdentity],
    explicit_hosts: Sequence[HostIdentity],
    previous: SiteIdentity | None,
) -> SiteIdentity:
    """Resolve explicit, newly inferred, and prior identity without losing evidence."""

    try:
        ZoneInfo(display_timezone)
    except ZoneInfoNotFoundError as error:
        raise ValueError(f"unknown IANA timezone {display_timezone!r}") from error
    prior_projects = previous.projects if previous is not None else ()
    project_order: list[str] = []
    projects_by_key: dict[str, ProjectIdentity] = {}
    for project in (*prior_projects, *inferred_projects, *explicit_projects):
        key = project.repository_url or "label:" + project.label.casefold()
        if key not in projects_by_key:
            project_order.append(key)
        projects_by_key[key] = project
    explicit_primary = next(
        (project for project in explicit_projects if project.primary), None
    )
    prior_primary = next((project for project in prior_projects if project.primary), None)
    inferred_primary = next(
        (project for project in inferred_projects if project.primary), None
    )
    selected_primary = explicit_primary or prior_primary or inferred_primary
    selected_key = (
        selected_primary.repository_url
        if selected_primary is not None and selected_primary.repository_url is not None
        else (
            "label:" + selected_primary.label.casefold()
            if selected_primary is not None
            else None
        )
    )
    projects = tuple(
        replace(projects_by_key[key], primary=key == selected_key)
        for key in project_order
    )

    prior_hosts = previous.hosts if previous is not None else ()
    host_order: list[str] = []
    hosts_by_key: dict[str, HostIdentity] = {}
    for host in (*prior_hosts, *inferred_hosts, *explicit_hosts):
        if host.hostname not in hosts_by_key:
            host_order.append(host.hostname)
        hosts_by_key[host.hostname] = host
    hosts = tuple(hosts_by_key[key] for key in host_order)
    timezone_source = display_timezone_source
    if (
        previous is not None
        and previous.display_timezone == display_timezone
        and display_timezone_source in ("api", "default")
    ):
        timezone_source = previous.display_timezone_source
    return SiteIdentity(
        team_slug=team_slug,
        projects=projects,
        hosts=hosts,
        display_timezone=display_timezone,
        display_timezone_source=timezone_source,
    )


__all__ = [
    "canonical_repository_url",
    "HostIdentity",
    "IdentityOverrides",
    "infer_structured_identity",
    "merge_site_identity",
    "parse_identity_overrides",
    "ProjectIdentity",
    "project_label_from_url",
    "SiteIdentity",
    "site_identity_from_json_obj",
]
