"""Idempotent bring-up of the server -> workspace -> tab -> pane chain.

Idempotent means: running this twice in a row does the work once and reuses it thereafter, and
running it against partially-existing state completes only the missing part. Each level is resolved
by LOOKING FOR IT (by label) before creating it, so two agents racing to bring up the same project
converge on at most one extra tab rather than a growing pile.

The cache is a pure optimisation and is never trusted. Cached ids are re-validated against the live
session (the pane still exists, and still belongs to a tab with the expected label) before use; any
mismatch discards the cache and re-resolves from labels. A cache that could go stale and be believed
would be a proxy for the real session state, which is precisely the failure mode to avoid.
"""

from __future__ import annotations

import fcntl
import itertools
import os
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass

from herdr_run.audit import make_private_dirs
from herdr_run.client import HerdrClient
from herdr_run.config import Config, render_tab_name
from herdr_run.errors import HerdrUnavailable
from herdr_run.state import open_lock_file, session_lock_path
from herdr_run.textfmt import json_text, quote, strict_json_loads

__all__ = ["Target", "resolve_target", "tab_label_for", "cache_path"]


@dataclass(frozen=True)
class Target:
    """The resolved pane, plus what had to be created to get there (for the audit record)."""

    workspace_id: str
    tab_id: str
    pane_id: str
    workspace_label: str
    tab_label: str
    created: tuple[str, ...]
    from_cache: bool


def tab_label_for(config: Config, agent: str) -> str:
    """Expand the project's restricted tab-name schema."""
    project = os.path.basename(os.path.abspath(config.project_root)) or "project"
    return render_tab_name(config.tab_name, agent=agent, project=project)


def cache_path(config: Config) -> str:
    """Path of the resolved-session cache. A pure optimisation; always re-validated before use."""
    return os.path.join(config.project_root, config.spool_dir, "session-cache.json")


@dataclass(frozen=True)
class _CacheRecord:
    """One cache entry: the ids it points at and the labels they were resolved from."""

    workspace_id: str
    tab_id: str
    pane_id: str
    workspace_label: str | None
    tab_label: str | None


#: Distinguishes temporary cache files written by one process, so two threads never collide.
_TEMPORARY_SEQUENCE = itertools.count()


def _read_cache_document(path: str) -> dict[str, object] | None:
    try:
        with open(path, "rb") as handle:
            payload = handle.read()
        document = strict_json_loads(payload.decode("utf-8"))
    except (OSError, UnicodeError, ValueError):
        return None
    return document if isinstance(document, dict) else None


def _load_cache(path: str, key: str) -> _CacheRecord | None:
    document = _read_cache_document(path)
    if document is None:
        return None
    entry = document.get(key)
    if not isinstance(entry, dict):
        return None
    ids = [entry.get(name) for name in ("workspace_id", "tab_id", "pane_id")]
    labels = [entry.get(name) for name in ("workspace_label", "tab_label")]
    if not all(isinstance(value, str) for value in ids):
        return None
    if not all(value is None or isinstance(value, str) for value in labels):
        return None
    return _CacheRecord(
        workspace_id=str(ids[0]),
        tab_id=str(ids[1]),
        pane_id=str(ids[2]),
        workspace_label=labels[0] if isinstance(labels[0], str) else None,
        tab_label=labels[1] if isinstance(labels[1], str) else None,
    )


def _store_cache(path: str, key: str, target: Target) -> None:
    parent = os.path.dirname(path)
    make_private_dirs(parent)
    # Serialize the whole read/modify/write. Without this, two agents resolving different tabs can
    # both read the old document and the later rename silently loses the earlier cache entry.
    flags = (
        os.O_CREAT
        | os.O_RDWR
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
    )
    lock_descriptor = os.open(f"{path}.lock", flags, 0o600)
    try:
        os.fchmod(lock_descriptor, 0o600)
    except OSError:
        os.close(lock_descriptor)
        raise
    with os.fdopen(lock_descriptor, "r+b") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        entries: dict[str, object] = _read_cache_document(path) or {}
        entries[key] = {
            "workspace_id": target.workspace_id,
            "tab_id": target.tab_id,
            "pane_id": target.pane_id,
            "workspace_label": target.workspace_label,
            "tab_label": target.tab_label,
        }
        encoded = (
            json_text(entries, indent=2) + "\n"
        ).encode("utf-8")
        # A unique temporary plus rename keeps readers from seeing a torn file, including callers
        # in another thread of this process. The name and the fsync match the native edition.
        temporary = os.path.join(
            parent, f".session-cache.{os.getpid()}.{next(_TEMPORARY_SEQUENCE)}.tmp"
        )
        try:
            descriptor = os.open(
                temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0), 0o600
            )
            try:
                os.fchmod(descriptor, 0o600)
                view = memoryview(encoded)
                while view:
                    view = view[os.write(descriptor, view) :]
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
            os.replace(temporary, path)
        except OSError:
            try:
                os.unlink(temporary)
            except OSError:
                pass
            raise
        os.chmod(path, 0o600)


def _pane_of_tab(client: HerdrClient, workspace_id: str, tab_id: str) -> str:
    panes = [pane for pane in client.panes(workspace_id) if pane.tab_id == tab_id]
    if not panes:
        raise HerdrUnavailable(f"tab {tab_id} has no pane")
    if len(panes) > 1:
        # A split tab is ambiguous: "the" pane is no longer well defined, and picking one silently
        # would mean typing into whichever pane happened to sort first.
        ids = ", ".join(pane.pane_id for pane in panes)
        raise HerdrUnavailable(
            f"tab {tab_id} has {len(panes)} panes ({ids}); herdr-run needs an unsplit tab. "
            "Close the extra panes or point tab_name at a different tab."
        )
    return panes[0].pane_id


def _enforce_pane_cap(
    client: HerdrClient, config: Config, workspace_id: str, agent: str, proc_root: str = "/proc"
) -> None:
    """Make room for ONE more tab once the workspace is already at ``max_panes``, or refuse.

    Agents are coined continuously and every one that runs a command leaves a tab behind; nothing
    closes them, so the command workspace grows without bound until the Herdr server itself becomes
    the bottleneck (measured: 260 panes, >1000% CPU, every control call timing out). A ceiling turns
    that slow collapse into one legible refusal naming the tool that fixes it.

    Checked ONLY on the create path. An agent whose tab already exists must never be locked out of
    it -- a cap that can break work in progress is a cap that gets switched off -- and a cap that
    refused an existing tab would also make the failure arrive at a random later moment rather than
    when a new tab is actually being added.

    At the cap, the least-recently-used tab whose shell is provably idle is closed first (see
    :mod:`herdr_run.evict`); the refusal is reserved for a workspace in which every tab is busy.
    """
    # Imported here, not at module level: evict reaches session through sweep.
    from herdr_run.evict import describe_skipped, evict_one

    if config.max_panes <= 0:
        return
    existing = len(client.panes(workspace_id))
    # Bounded: a close that the listing never reflects must not turn into closing every tab.
    budget = max(existing - config.max_panes, 0) + 1
    skipped: list[tuple[str, str]] = []
    while existing >= config.max_panes and budget > 0:
        budget -= 1
        outcome = evict_one(client, config, workspace_id, agent, proc_root)
        if isinstance(outcome, list):
            skipped = outcome
            break
        existing = len(client.panes(workspace_id))
    if existing < config.max_panes:
        return
    source = config.source_path or "your .herdr-run.yaml"
    message = (
        f"workspace {quote(config.workspace)} already holds {existing} pane(s) and max_panes is "
        f"{config.max_panes}; refusing to open a tab for this agent. Run 'herdr-run reap' to see "
        f"which tabs are provably finished and can be closed, or raise max_panes in {source} "
        "(0 disables the cap)."
    )
    if skipped:
        message += " " + describe_skipped(skipped)
    raise HerdrUnavailable(message)


def _cache_still_valid(
    client: HerdrClient,
    cached: _CacheRecord,
    workspace_label: str,
    tab_label: str,
) -> bool:
    # A recorded label that disagrees with the current one fails closed without asking Herdr.
    if cached.workspace_label is not None and cached.workspace_label != workspace_label:
        return False
    if cached.tab_label is not None and cached.tab_label != tab_label:
        return False
    if not client.pane_exists(cached.pane_id):
        return False
    try:
        # IDs may be reused after a server restart. Validate the workspace label as well as the
        # pane/tab relationship before trusting the cached destination. A lookup that fails is a
        # cache MISS, never a failed run: the label-based path asks the same questions again.
        if client.workspace_id_for_label(workspace_label) != cached.workspace_id:
            return False
        if client.tab_id_for_label(cached.workspace_id, tab_label) != cached.tab_id:
            return False
    except HerdrUnavailable:
        return False
    return any(
        pane.pane_id == cached.pane_id
        and pane.tab_id == cached.tab_id
        and pane.workspace_id == cached.workspace_id
        for pane in client.panes(cached.workspace_id)
    )


@contextmanager
def _session_resolution_lock() -> Iterator[None]:
    """Serialize all account-global Herdr session discovery and creation."""
    path = session_lock_path()
    handle = open_lock_file(path)
    try:
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        except OSError as exc:
            raise HerdrUnavailable(
                f"cannot lock Herdr session resolution at {path}: {exc}"
            ) from exc
        yield
    finally:
        # Closing releases `flock` even when the protected operation raised.
        handle.close()


def resolve_target(
    client: HerdrClient, config: Config, agent: str, *, use_cache: bool = True
) -> Target:
    """Bring up (or reuse) the workspace/tab/pane for ``agent`` and return the resolved ids."""
    with _session_resolution_lock():
        return _resolve_target_locked(client, config, agent, use_cache=use_cache)


def _resolve_target_locked(
    client: HerdrClient, config: Config, agent: str, *, use_cache: bool
) -> Target:
    """Resolve a target while the account-global session lock is held."""
    tab_label = tab_label_for(config, agent)
    # An absolute path is passed through exactly as written, and the pane's `cd` resolves it; a
    # relative one is resolved lexically against the project root.
    cwd = config.cwd or config.project_root
    if not os.path.isabs(cwd):
        cwd = os.path.normpath(os.path.join(config.project_root, cwd))
    key = f"{config.workspace}\x00{tab_label}"
    path = cache_path(config)

    created: list[str] = []
    if client.ensure_server():
        created.append("server")

    if use_cache and not created:
        cached = _load_cache(path, key)
        if cached is not None and _cache_still_valid(
            client, cached, config.workspace, tab_label
        ):
            return Target(
                workspace_id=cached.workspace_id,
                tab_id=cached.tab_id,
                pane_id=cached.pane_id,
                workspace_label=config.workspace,
                tab_label=tab_label,
                created=(),
                from_cache=True,
            )

    workspace_id = client.workspace_id_for_label(config.workspace)
    tab_id: str | None = None
    if workspace_id is None:
        # A new workspace arrives with one default tab; rename it into our schema instead of adding
        # a second one, so the common first-run case leaves exactly one tab behind.
        workspace_id, root_tab_id, _root_pane_id = client.create_workspace(
            label=config.workspace, cwd=cwd
        )
        client.rename_tab(root_tab_id, tab_label)
        tab_id = root_tab_id
        created.extend(["workspace", "tab"])
    else:
        tab_id = client.tab_id_for_label(workspace_id, tab_label)
        if tab_id is None:
            _enforce_pane_cap(client, config, workspace_id, agent)
            tab_id = client.create_tab(
                workspace_id=workspace_id, label=tab_label, cwd=cwd
            )
            created.append("tab")

    pane_id = _pane_of_tab(client, workspace_id, tab_id)
    target = Target(
        workspace_id=workspace_id,
        tab_id=tab_id,
        pane_id=pane_id,
        workspace_label=config.workspace,
        tab_label=tab_label,
        created=tuple(created),
        from_cache=False,
    )
    try:
        _store_cache(path, key, target)
    except OSError:
        # The cache is an optimisation; failing to persist it must never fail the run.
        pass
    return target
