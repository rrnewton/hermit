"""Named, long-lived interactive agents sharing a Herdr workspace.

Herdr owns the terminal and harness process; this module owns durable names,
launch intent, queue routing, output snapshots, and conservative tab teardown.
It never closes a workspace, silently restarts a conversation, or changes the
harness's permission settings. Coordinators and humans see the same terminal.
"""
from __future__ import annotations

import copy
import builtins
import fcntl
import hashlib
import ctypes
import errno
import json
import math
import os
import re
import shutil
import stat
import subprocess
import sys
import time
import uuid
from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from typing import NoReturn, TypeVar, cast

from agentctl import agent
from agentctl.client import (
    AgentPaneInfo,
    CustomProcessIdentity,
    HerdrClient,
    Pane,
    PaneShellProof,
    herdr_error_code,
    muse_auto_review_idle_composer,
    muse_idle_composer,
    muse_prompt_in_composer,
    muse_prompt_is_exact_composer,
    muse_prompt_transcript_count,
    muse_startup_metadata,
    muse_trust_prompt,
    muse_verified_process_composer,
    muse_verified_process_goal_paused,
    muse_verified_process_idle_composer,
    relay_trust_prompt,
)
from agentctl.errors import (
    AgentDeliveryError, HerdrRunError, HerdrUnavailable, InputExpectationFailed,
    MisrouteRecovered, ProbableMisroute, RecipientChanged, RecoveryAction, _with_recovery,
)
from agentctl.profiles import (
    reasoning_arguments,
    validate_structured_harness_argument_conflicts,
    workspace_for_registry,
)
from agentctl.submission import (
    PromptNotStaged, SubmissionReceipt, _compact, submit_verified,
)

_T = TypeVar("_T")
_NAME = re.compile(r"[a-z][a-z0-9-]{0,31}\Z")
_JOURNAL_ID = re.compile(r"[0-9a-f]{32}\Z")
#: Earlier names one record keeps; rename refuses rather than exceed it.
_MAX_NAME_HISTORY = 256
#: Names evicted from the history that a record still remembers for liveness lookups.
_MAX_FORMER_NAMES = 4096
#: The harness pinning rule: the harness must be the pane's only foreground process.
#: Anchors written under an earlier rule (none, or a launcher-capable one) do not count.
ANCHOR_RULE = 2
_KIND = re.compile(r"[a-z][a-z0-9-]{0,63}\Z")
_ENVIRONMENT_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")
_BRACKETED_PASTE_START = "\x1b[200~"
_BRACKETED_PASTE_END = "\x1b[201~"
_MAX_U64 = (1 << 64) - 1
_MAX_AGENT_RECORD_BYTES = 1 << 20
_MAX_SNAPSHOT_BYTES = 16 << 20
_SHA256 = re.compile(r"[0-9a-f]{64}\Z")
_NESTED_SESSION_SCHEMAS = frozenset({
    "agentctl-session/v2", "agentctl-session/v3",
})
_NESTED_LAUNCH_SCHEMAS = frozenset({
    "agentctl-launch/v1", "agentctl-launch/v2",
})


#: Herdr error codes for a target that no longer exists, as doctor reports them.
_MISSING_TARGETS = {
    "workspace_not_found": "workspace-missing",
    "tab_not_found": "tab-missing",
    "pane_not_found": "pane-missing",
}


def _missing_target(exc: Exception) -> str | None:
    """The doctor finding for a Herdr refusal naming a closed workspace, tab, or pane."""
    code = herdr_error_code(str(exc))
    return _MISSING_TARGETS.get(code) if code is not None else None


def _session_matches(record: AgentRecord, info: AgentPaneInfo) -> bool:
    """Does the pane still report the observed native session, provider and id, this
    record holds? A record without one has nothing to compare."""
    if record.session_value is None or record._session_source == "asserted":
        return True
    return (info.session_value == record.session_value
            and info.session_agent == (record.session_agent or record.harness))


def _native_session(agent_kind: str, value: str, source: str) -> dict[str, str]:
    """Recovery provenance, separate from the pane's observed routing anchor."""
    return {"schema": "agentctl-native-session/v1", "agent": agent_kind,
            "value": value, "source": source}


def _capture_native_session(record: AgentRecord, info: AgentPaneInfo) -> None:
    """Keep a supplied conversation and reject contradictory harness reports."""
    if (info.session_agent is None) != (info.session_value is None):
        raise AgentDeliveryError("started agent reported an incomplete native session identity")
    if any(value is not None and (not value or "\0" in value)
           for value in (info.session_agent, info.session_value)):
        raise AgentDeliveryError("started agent reported an invalid native session identity")
    if info.session_value is None:
        return
    provider = info.session_agent or record.harness
    if provider != record.harness:
        raise AgentDeliveryError("started agent reported a different native session provider")
    if record.native_session is not None:
        if record.native_session["value"] != info.session_value:
            raise AgentDeliveryError("started agent reported a different native conversation")
    else:
        record.native_session = _native_session(provider, info.session_value, "observed")


def _validate_rename_journal(value: object, path: Path) -> dict[str, object]:
    """Validate one rename journal written by either edition."""
    keys = {"schema", "token", "old", "new", "adapter", "pane_id", "tab_id",
            "terminal_id", "workspace_id", "journal_id", "started_at"}
    if not isinstance(value, dict) or set(value) != keys:
        raise AgentDeliveryError(f"invalid rename journal: {path}")
    journal = cast(dict[str, object], value)
    started = journal["started_at"]
    if (journal["schema"] != "agentctl-rename/v1"
            or not isinstance(journal["token"], str) or path.name != f"{journal['token']}.json"
            or re.fullmatch(r"[a-z0-9-]{1,80}", journal["token"]) is None
            or not isinstance(journal["old"], str) or not _NAME.fullmatch(journal["old"])
            or not isinstance(journal["new"], str) or not _NAME.fullmatch(journal["new"])
            or journal["old"] == journal["new"]
            or journal["adapter"] not in ("herdr", "herdr-foreign")
            or not isinstance(journal["pane_id"], str) or not journal["pane_id"]
            or not isinstance(journal["tab_id"], str) or not journal["tab_id"]
            or (journal["terminal_id"] is not None and not isinstance(journal["terminal_id"], str))
            or (journal["workspace_id"] is not None and not isinstance(journal["workspace_id"], str))
            or not isinstance(journal["journal_id"], str)
            or not _JOURNAL_ID.fullmatch(journal["journal_id"])
            or isinstance(started, bool) or not isinstance(started, (int, float))
            or not math.isfinite(started)):
        raise AgentDeliveryError(f"invalid rename journal: {path}")
    return journal


def _name_history(value: object, path: Path) -> list[dict[str, object]]:
    """Validate a record's earlier names: each a valid name with a time and journal id."""
    if not isinstance(value, list) or len(value) > _MAX_NAME_HISTORY:
        raise AgentDeliveryError(f"invalid name history in {path}")
    history: list[dict[str, object]] = []
    journals: set[str] = set()
    for entry in value:
        if not isinstance(entry, dict) or set(entry) != {"name", "renamed_at", "journal_id"}:
            raise AgentDeliveryError(f"invalid name history in {path}")
        name, renamed_at, journal_id = entry["name"], entry["renamed_at"], entry["journal_id"]
        if (not isinstance(name, str) or not _NAME.fullmatch(name)
                or isinstance(renamed_at, bool) or not isinstance(renamed_at, (int, float))
                or not math.isfinite(renamed_at)
                or not isinstance(journal_id, str) or not _JOURNAL_ID.fullmatch(journal_id)
                or journal_id in journals):
            raise AgentDeliveryError(f"invalid name history in {path}")
        journals.add(journal_id)
        history.append({"name": name, "renamed_at": renamed_at, "journal_id": journal_id})
    return history


def _rename_directory_noreplace_at(
    source_parent: int,
    source_name: str,
    destination_parent: int,
    destination_name: str,
) -> None:
    """Atomically move one entry between pinned directories without replacement."""
    try:
        renameat2 = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as exc:
        raise AgentDeliveryError(
            "cannot archive agent: atomic no-replace rename is unavailable"
        ) from exc
    renameat2.argtypes = [
        ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint,
    ]
    renameat2.restype = ctypes.c_int
    ctypes.set_errno(0)
    if renameat2(
        source_parent,
        os.fsencode(source_name),
        destination_parent,
        os.fsencode(destination_name),
        1,
    ) == 0:
        return
    number = ctypes.get_errno()
    if number == errno.EEXIST:
        raise AgentDeliveryError(
            f"refusing to replace existing agent archive {destination_name}"
        )
    if number in (errno.ENOSYS, errno.EINVAL, errno.EOPNOTSUPP):
        raise AgentDeliveryError(
            "cannot archive agent: filesystem lacks atomic no-replace rename support"
        )
    raise AgentDeliveryError(
        f"cannot archive agent {source_name} as {destination_name}: {os.strerror(number)}"
    )


def _fsync_pinned_directory(descriptor: int, label: str) -> None:
    """Durably order one already-pinned directory; ``label`` is diagnostic only."""
    del label
    os.fsync(descriptor)


def _snapshot_json_bytes(document: dict[str, object]) -> bytes:
    """Encode retained terminal text as UTF-8 without ASCII escape expansion."""
    return (
        json.dumps(
            document,
            indent=2,
            sort_keys=True,
            allow_nan=False,
            ensure_ascii=False,
        )
        + "\n"
    ).encode("utf-8")


def _close_descriptor(
    descriptor: int, label: str, *, primary: BaseException | None,
) -> None:
    """Close one recovery fd without leaking a raw OS error or hiding its cause."""
    try:
        os.close(descriptor)
    except OSError as exc:
        if primary is not None:
            error_type = (
                _ArchivePublicationUncertainError
                if isinstance(primary, _ArchivePublicationUncertainError)
                else AgentDeliveryError
            )
            raise error_type(
                f"{primary}; additionally could not close {label}: {exc}"
            ) from primary
        raise AgentDeliveryError(f"cannot close {label}: {exc}") from exc


def _name(value: str) -> str:
    if not _NAME.fullmatch(value) or value == "archive":
        raise AgentDeliveryError("agent name must start with a lowercase letter and contain 1-32 lowercase letters, digits or hyphens; 'archive' is reserved")
    return value


def harness_arguments(
    harness: str, *, model: str | None = None, resume: str | None = None,
    extra: Sequence[str] = (),
) -> tuple[str, ...]:
    """Build harness-specific arguments; explicit extras retain user configuration."""
    if not _KIND.fullmatch(harness):
        raise AgentDeliveryError("harness must be a Herdr agent kind")
    if any(not value or "\0" in value for value in extra):
        raise AgentDeliveryError("harness arguments must be nonempty and contain no NUL")
    args: list[str] = []
    if harness == "codex":
        if resume:
            args.extend(("resume", resume))
        args.append("--no-alt-screen")
        if model:
            args.extend(("--model", model))
    elif harness == "claude":
        if resume:
            args.extend(("--resume", resume))
        if model:
            args.extend(("--model", model))
    elif harness == "muse":
        if model:
            args.extend(("--model", model))
        if resume is not None:
            args.extend(extra)
            args.extend(("resume", resume))
            extra = ()
    elif model is not None or resume is not None:
        raise AgentDeliveryError("model and resume presets support codex/claude/muse; use harness arguments for other kinds")
    if any("\0" in value for value in args):
        raise AgentDeliveryError("harness arguments must contain no NUL")
    return tuple((*args, *extra))


def environment_entries(values: Sequence[str]) -> tuple[str, ...]:
    """Validate literal ``KEY=VALUE`` entries for a newly created terminal."""
    entries: list[str] = []
    for entry in values:
        name, separator, value = entry.partition("=")
        if not separator:
            raise AgentDeliveryError("environment entry must use KEY=VALUE")
        if "\0" in name or _ENVIRONMENT_NAME.fullmatch(name) is None:
            raise AgentDeliveryError(
                "environment variable name must match [A-Za-z_][A-Za-z0-9_]*"
            )
        if "\0" in value:
            raise AgentDeliveryError("environment variable value must contain no NUL")
        entries.append(entry)
    return tuple(entries)


#: Seconds after the newest confirmed prompt delivery during which an idle agent counts as ready
#: only once `wait` has seen it busy. A delivery is confirmed when the prompt has left the composer,
#: which can precede the harness visibly starting its turn.
DELIVERY_SETTLE_SECONDS = 10.0


def _latest_confirmed_delivery(queue: Path) -> float | None:
    """Return the newest `confirmed_at` among processed queue entries, or None."""
    latest: float | None = None
    try:
        entries = list((queue / "processed").iterdir())
    except OSError:
        return None
    for entry in entries:
        if entry.suffix != ".json":
            continue
        try:
            document = json.loads(entry.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        value = document.get("confirmed_at") if isinstance(document, dict) else None
        if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value):
            latest = float(value) if latest is None else max(latest, float(value))
    return latest

@dataclass
class AgentRecord:
    """Versioned launch identity for managed interactive agents."""
    name: str
    token: str
    harness: str
    cwd: str
    created_at: float
    schema: int = 1
    lifecycle: str = "starting"
    workspace_id: str | None = None
    tab_id: str | None = None
    pane_id: str | None = None
    session_agent: str | None = None
    session_value: str | None = None
    model: str | None = None
    resume: str | None = None
    arguments: list[str] = field(default_factory=list)
    startup_warning: str | None = None
    effective_reasoning_effort: str | None = None
    error: str | None = None
    goal: str | None = None
    goal_delivery: str | None = None
    goal_session_id: str | None = None
    goal_command: list[str] | None = None
    goal_messages: dict[str, str] = field(default_factory=dict)
    goal_message_id: str | None = None
    adapter: str = "herdr"
    mode: str = "interactive"
    backend: str = "herdr"
    paused: bool = False
    runtime_home: str | None = None
    pane_reported_by_agentctl: bool = False
    custom_process_identity: CustomProcessIdentity | None = None
    foreign_shell_identity: CustomProcessIdentity | None = None
    #: Herdr terminal the pane held when agentctl anchored this record.
    terminal_id: str | None = None
    #: Kernel identity of the harness process anchored at start, adopt or anchor.
    harness_identity: CustomProcessIdentity | None = None
    #: Earlier names, oldest first: ``{"name", "renamed_at", "journal_id"}``.
    name_history: list[dict[str, object]] = field(default_factory=list)
    #: Which pinning rule produced ``harness_identity``; only ``ANCHOR_RULE`` anchors count.
    anchor_rule: int | None = None
    #: Earlier names evicted from the capped ``name_history``, kept for liveness lookups.
    former_names: list[str] = field(default_factory=list)
    #: A reported or explicitly selected native conversation; never a routing anchor.
    native_session: dict[str, str] | None = None
    profile: str | None = None
    reasoning_effort: str | None = None
    slot: str | None = None
    slot_project: str | None = None
    slot_isolation: str | None = None
    #: Names only; environment values stay in the owner's private profile.
    environment_names: list[str] = field(default_factory=list)

    @property
    def harness_anchor(self) -> CustomProcessIdentity | None:
        """The pinned harness process, if the current pinning rule produced it."""
        return self.harness_identity if self.anchor_rule == ANCHOR_RULE else None
    _unknown: dict[str, object] = field(default_factory=dict, init=False, repr=False)
    _nested_storage: dict[str, object] | None = field(
        default=None, init=False, repr=False,
    )
    _session_source: str | None = field(default=None, init=False, repr=False)
    # A revive candidate is saved through its pinned directory, never through NAME.
    _storage: tuple[Path, int, int] | None = field(default=None, init=False, repr=False)

    def to_document(self) -> dict[str, object]:
        """Serialize in the decoded row's schema without duplicating launch intent."""
        if self._nested_storage is not None:
            return self._nested_document()
        document = asdict(self)
        document.pop("_unknown")
        document.pop("_nested_storage")
        document.pop("_session_source")
        document.pop("_storage")
        if self._unknown.keys() & document.keys():
            raise AgentDeliveryError("unknown agent metadata conflicts with a known schema field")
        document.update(self._unknown)
        return document

    def _nested_document(self) -> dict[str, object]:
        """Update mutable fields while retaining one canonical nested LaunchSpec."""
        assert self._nested_storage is not None
        document = copy.deepcopy(self._nested_storage)
        launch_value = document.get("launch")
        if not isinstance(launch_value, dict):  # pragma: no cover - decode invariant
            raise AgentDeliveryError("nested agent record lost its launch specification")
        launch = cast(dict[str, object], launch_value)
        argv = launch.get("argv")
        projection = {
            "harness": self.harness,
            "cwd": self.cwd,
            "adapter": self.adapter,
            "mode": self.mode,
            "backend": self.backend,
            "model": self.model,
            "resume": self.resume,
            "runtime_home": self.runtime_home,
        }
        if any(launch.get(key) != value for key, value in projection.items()) or (
            not isinstance(argv, list) or list(argv[1:]) != self.arguments
        ):
            raise AgentDeliveryError(
                "decoded nested launch intent changed; migrate through a canonical writer"
            )
        document.update({
            "name": self.name,
            "token": self.token,
            "created_at": self.created_at,
            "lifecycle": self.lifecycle,
            "workspace_id": self.workspace_id,
            "tab_id": self.tab_id,
            "pane_id": self.pane_id,
            "startup_warning": self.startup_warning,
            "effective_reasoning_effort": self.effective_reasoning_effort,
            "error": self.error,
            "paused": self.paused,
            "pane_reported_by_agentctl": self.pane_reported_by_agentctl,
            "foreign_shell_identity": (
                None if self.foreign_shell_identity is None
                else asdict(self.foreign_shell_identity)
            ),
        })
        custom_identity = (
            None if self.custom_process_identity is None
            else asdict(self.custom_process_identity)
        )
        if document["schema"] == "agentctl-session/v2":
            document.update({
                "session_agent": self.session_agent,
                "session_value": self.session_value,
                "goal": self.goal,
                "goal_delivery": self.goal_delivery,
                "goal_session_id": self.goal_session_id,
                "goal_command": self.goal_command,
                "goal_messages": dict(self.goal_messages),
                "goal_message_id": self.goal_message_id,
                "custom_process_identity": custom_identity,
            })
        else:
            document["native_session"] = (
                None if self.session_value is None else {
                    "schema": "agentctl-native-session/v1",
                    "agent": self.session_agent or self.harness,
                    "value": self.session_value,
                    "source": self._session_source or "observed",
                }
            )
            document["goal"] = {
                "schema": "agentctl-goal/v1",
                "objective": self.goal,
                "message_id": self.goal_message_id,
                "native_command": self.goal_command,
            }
            document["custom_process_identity"] = custom_identity
        return document

    @classmethod
    def load(cls, path: Path, name: str) -> AgentRecord:
        """Reject malformed or non-private state before using any recorded identity."""
        value = agent._read_queue_json(str(path), "agent record", require_private=True)
        return cls._from_value(value, path, name)

    @classmethod
    def _from_value(cls, value: object, path: Path, name: str) -> AgentRecord:
        """Validate an already identity-bound record value."""
        if not isinstance(value, dict):
            raise AgentDeliveryError(f"invalid agent record: {path}")
        source = cast(dict[str, object], value)
        nested_storage: dict[str, object] | None = None
        nested_session_source: str | None = None
        if source.get("schema") in _NESTED_SESSION_SCHEMAS:
            nested_storage = copy.deepcopy(source)
            if source.get("schema") == "agentctl-session/v3":
                native = source.get("native_session")
                if isinstance(native, dict):
                    candidate = native.get("source")
                    if isinstance(candidate, str):
                        nested_session_source = candidate
            else:
                nested_session_source = (
                    "observed" if source.get("session_value") is not None
                    else "asserted" if source.get("goal_session_id") is not None
                    else None
                )
            document = cls._normalize_nested_storage(source, path)
        else:
            document = source
        for key in ("name", "token", "harness", "cwd", "lifecycle"):
            if not isinstance(document.get(key), str) or not document[key]:
                raise AgentDeliveryError(f"invalid agent record field {key}: {path}")
        for key in ("workspace_id", "tab_id", "pane_id", "session_agent", "session_value", "model", "resume", "startup_warning", "effective_reasoning_effort", "error", "goal", "goal_delivery", "goal_session_id", "goal_message_id", "profile", "reasoning_effort", "slot", "slot_project", "slot_isolation"):
            if document.get(key) is not None and not isinstance(document[key], str):
                raise AgentDeliveryError(f"invalid agent record field {key}: {path}")
        created = document.get("created_at")
        args = document.get("arguments")
        goal_command = document.get("goal_command")
        if (document.get("schema") != 1 or isinstance(document.get("schema"), bool)
            or document["name"] != name or not isinstance(created, (int, float))
            or isinstance(created, bool) or not math.isfinite(created)
            or re.fullmatch(r"[a-z0-9-]{1,80}", str(document["token"])) is None
            or not isinstance(args, list) or any(not isinstance(item, str) for item in args)):
            raise AgentDeliveryError(f"invalid agent record: {path}")
        if goal_command is not None and (not isinstance(goal_command, list) or not goal_command
            or any(not isinstance(item, str) or not item or "\0" in item for item in goal_command)):
            raise AgentDeliveryError(f"invalid goal command in agent record: {path}")
        goals = document.get("goal_messages", {})
        if not isinstance(goals, dict) or any(not isinstance(key, str) or agent._MESSAGE_ID.fullmatch(key) is None
            or not isinstance(value, str) or not value for key, value in goals.items()):
            raise AgentDeliveryError(f"invalid goal messages in agent record: {path}")
        goal_message_id = document.get("goal_message_id")
        if goal_message_id is not None and (not isinstance(goal_message_id, str) or agent._MESSAGE_ID.fullmatch(goal_message_id) is None):
            raise AgentDeliveryError(f"invalid goal message id in agent record: {path}")
        warning = document.get("startup_warning")
        effective_effort = document.get("effective_reasoning_effort")
        if (isinstance(warning, str)
                and (len(warning) > 256 or not warning.isascii()
                     or any(character in warning for character in "\r\n\0"))):
            raise AgentDeliveryError(f"invalid startup warning in agent record: {path}")
        if (isinstance(effective_effort, str)
                and effective_effort not in (
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
                )):
            raise AgentDeliveryError(f"invalid effective reasoning effort in agent record: {path}")
        known = {key for key, field_info in cls.__dataclass_fields__.items() if field_info.init}
        fields = {key: document[key] for key in known if key in document}
        if document.get("adapter", "herdr") not in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay", "turn-runner"):
            raise AgentDeliveryError(f"unsupported runtime adapter in {path}")
        if document.get("mode", "interactive") not in ("interactive", "headless"):
            raise AgentDeliveryError(f"invalid execution mode in {path}")
        if document.get("backend", "herdr") not in ("herdr", "tmux"):
            raise AgentDeliveryError(f"invalid terminal backend in {path}")
        if not isinstance(document.get("paused", False), bool):
            raise AgentDeliveryError(f"invalid pause state in {path}")
        if not isinstance(document.get("pane_reported_by_agentctl", False), bool):
            raise AgentDeliveryError(f"invalid pane report ownership in {path}")
        def process_identity(key: str) -> CustomProcessIdentity | None:
            raw_identity = document.get(key)
            if raw_identity is None:
                return None
            if (not isinstance(raw_identity, dict)
                    or set(raw_identity) != {
                        "version", "boot_id", "pid", "starttime_ticks",
                        "executable_device", "executable_inode",
                    }):
                raise AgentDeliveryError(f"invalid {key.replace('_', ' ')} in {path}")
            identity = cast(dict[str, object], raw_identity)
            integers = [
                identity.get("version"), identity.get("pid"),
                identity.get("starttime_ticks"), identity.get("executable_device"),
                identity.get("executable_inode"),
            ]
            boot_id = identity.get("boot_id")
            if (any(not isinstance(value, int) or isinstance(value, bool) for value in integers)
                    or identity.get("version") != 1
                    or not isinstance(boot_id, str)
                    or re.fullmatch(
                        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                        boot_id,
                    ) is None
                    or not 1 <= cast(int, identity["pid"]) <= 2_147_483_647
                    or not 1 <= cast(int, identity["starttime_ticks"]) <= _MAX_U64
                    or not 1 <= cast(int, identity["executable_device"]) <= _MAX_U64
                    or not 1 <= cast(int, identity["executable_inode"]) <= _MAX_U64):
                raise AgentDeliveryError(f"invalid {key.replace('_', ' ')} in {path}")
            return CustomProcessIdentity(
                version=1, boot_id=boot_id, pid=cast(int, identity["pid"]),
                starttime_ticks=cast(int, identity["starttime_ticks"]),
                executable_device=cast(int, identity["executable_device"]),
                executable_inode=cast(int, identity["executable_inode"]),
            )
        custom_identity = process_identity("custom_process_identity")
        foreign_shell_identity = process_identity("foreign_shell_identity")
        harness_identity = process_identity("harness_identity")
        if custom_identity is not None:
            fields["custom_process_identity"] = custom_identity
        if foreign_shell_identity is not None:
            fields["foreign_shell_identity"] = foreign_shell_identity
        if harness_identity is not None:
            fields["harness_identity"] = harness_identity
        terminal_id = document.get("terminal_id")
        if terminal_id is not None and (
                not isinstance(terminal_id, str) or not terminal_id
                or len(terminal_id) > 128 or not terminal_id.isascii()):
            raise AgentDeliveryError(f"invalid terminal id in {path}")
        fields["name_history"] = _name_history(document.get("name_history", []), path)
        anchor_rule = document.get("anchor_rule")
        if anchor_rule is not None and (isinstance(anchor_rule, bool) or not isinstance(anchor_rule, int)):
            raise AgentDeliveryError(f"invalid anchor rule in {path}")
        fields["anchor_rule"] = anchor_rule
        former = document.get("former_names", [])
        if (not isinstance(former, list) or len(former) > _MAX_FORMER_NAMES
                or any(not isinstance(item, str) or not _NAME.fullmatch(item) for item in former)
                or len(set(former)) != len(former)):
            raise AgentDeliveryError(f"invalid former names in {path}")
        fields["former_names"] = list(former)
        selected_profile = document.get("profile")
        if selected_profile is not None and (not isinstance(selected_profile, str) or not _NAME.fullmatch(selected_profile)):
            raise AgentDeliveryError(f"invalid profile in {path}")
        slot_name = document.get("slot")
        if slot_name is not None and (not isinstance(slot_name, str) or not slot_name or "\0" in slot_name):
            raise AgentDeliveryError(f"invalid slot in {path}")
        project = document.get("slot_project")
        if project is not None and (not isinstance(project, str) or not Path(project).is_absolute() or "\0" in project):
            raise AgentDeliveryError(f"invalid slot project in {path}")
        if document.get("slot_isolation") not in (None, "userns", "cgroup", "root"):
            raise AgentDeliveryError(f"invalid slot isolation in {path}")
        if document.get("reasoning_effort") not in (None, "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"):
            raise AgentDeliveryError(f"invalid reasoning effort in {path}")
        names = document.get("environment_names", [])
        if (not isinstance(names, list) or any(not isinstance(item, str) or not _ENVIRONMENT_NAME.fullmatch(item) for item in names)
                or len(set(names)) != len(names)):
            raise AgentDeliveryError(f"invalid environment names in {path}")
        fields["environment_names"] = list(names)
        native = document.get("native_session")
        if native is not None:
            if (not isinstance(native, dict) or set(native) != {"schema", "agent", "value", "source"}
                    or native.get("schema") != "agentctl-native-session/v1"
                    or native.get("agent") != document["harness"]
                    or native.get("source") not in ("observed", "asserted")
                    or not isinstance(native.get("value"), str) or not native["value"] or "\0" in native["value"]):
                raise AgentDeliveryError(f"invalid native session in {path}")
            if (document.get("resume") not in (None, native["value"])
                    or document.get("session_value") not in (None, native["value"])
                    or document.get("session_agent") not in (None, document["harness"])):
                raise AgentDeliveryError(f"conflicting native session in {path}")
            fields["native_session"] = dict(native)
        if (custom_identity is not None
                and ((document.get("adapter", "herdr"), document.get("harness"))
                     not in (("herdr-pane", "muse"), ("herdr-relay", "claude"), ("herdr-relay", "codex"))
                     or not isinstance(document.get("pane_id"), str)
                     or not document["pane_id"])):
            raise AgentDeliveryError(
                f"custom process identity requires a Muse herdr-pane or a herdr-relay in {path}"
            )
        if (foreign_shell_identity is not None
                and (document.get("adapter", "herdr") != "herdr-foreign"
                     or not isinstance(document.get("pane_id"), str)
                     or not document["pane_id"])):
            raise AgentDeliveryError(
                f"foreign shell identity requires a herdr-foreign pane in {path}"
            )
        home = document.get("runtime_home")
        if home is not None and (not isinstance(home, str) or not Path(home).is_absolute()):
            raise AgentDeliveryError(f"invalid runtime directory in {path}")
        record = cls(**fields)  # type: ignore[arg-type]
        record._unknown = {key: value for key, value in document.items() if key not in known}
        record._nested_storage = nested_storage
        record._session_source = nested_session_source
        return record

    @classmethod
    def _normalize_nested_storage(
        cls, document: dict[str, object], path: Path,
    ) -> dict[str, object]:
        """Decode v2/v3 rows at the boundary while leaving launch data canonical."""
        schema = document.get("schema")
        common = {
            "schema", "name", "token", "created_at", "lifecycle", "launch",
            "workspace_id", "tab_id", "pane_id", "startup_warning",
            "effective_reasoning_effort", "error", "paused",
            "pane_reported_by_agentctl", "foreign_shell_identity",
            "runner_identity", "extensions",
        }
        expected = (
            common | {
                "session_agent", "session_value", "goal", "goal_delivery",
                "goal_session_id", "goal_command", "goal_messages",
                "goal_message_id", "custom_process_identity",
            }
            if schema == "agentctl-session/v2"
            else common | {"native_session", "goal", "custom_process_identity"}
        )
        if set(document) != expected:
            raise AgentDeliveryError(f"invalid agent record {schema} fields: {path}")
        launch_value = document.get("launch")
        if not isinstance(launch_value, dict):
            raise AgentDeliveryError(f"invalid agent record launch specification: {path}")
        launch = cast(dict[str, object], launch_value)
        launch_fields = {
            "schema", "harness", "cwd", "adapter", "mode", "backend",
            "model", "resume", "profile", "argv", "environment_names",
            "runtime_home", "runtime_ownership", "executable",
        }
        if launch.get("schema") == "agentctl-launch/v2":
            launch_fields.add("permission_mode")
        elif launch.get("schema") not in _NESTED_LAUNCH_SCHEMAS:
            raise AgentDeliveryError(f"invalid agent record launch schema: {path}")
        if set(launch) != launch_fields:
            raise AgentDeliveryError(f"invalid agent record launch fields: {path}")
        lifecycle = document.get("lifecycle")
        if lifecycle not in {
            "starting", "running", "stopping", "stopped",
            "launch_failed", "adopt_failed",
        }:
            raise AgentDeliveryError(f"invalid agent record lifecycle: {path}")
        harness = launch.get("harness")
        cwd = launch.get("cwd")
        adapter = launch.get("adapter")
        mode = launch.get("mode")
        backend = launch.get("backend")
        ownership = launch.get("runtime_ownership")
        profile = launch.get("profile")
        permission_mode = launch.get("permission_mode")
        if (not isinstance(harness, str) or _KIND.fullmatch(harness) is None
                or not isinstance(cwd, str) or not Path(cwd).is_absolute()
                or adapter not in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay")
                or mode != "interactive" or backend != "herdr"
                or profile is not None
                    and (not isinstance(profile, str) or not profile or "\0" in profile)
                or permission_mode is not None
                or launch.get("runtime_home") is not None):
            raise AgentDeliveryError(
                f"invalid nested interactive launch contract: {path}"
            )
        if ((adapter == "herdr-foreign" and ownership != "foreign")
                or (adapter != "herdr-foreign" and ownership != "owned")
                or (adapter == "herdr-pane" and harness != "muse")
                or (adapter == "herdr-relay" and harness not in RELAY_HARNESSES)):
            raise AgentDeliveryError(
                f"inconsistent launch ownership or adapter: {path}"
            )
        for field_name in ("model", "resume"):
            field_value = launch.get(field_name)
            if (field_value is not None
                    and (not isinstance(field_value, str)
                         or not field_value or "\0" in field_value)):
                raise AgentDeliveryError(
                    f"invalid agent record launch {field_name}: {path}"
                )
        argv = launch.get("argv")
        environment_names = launch.get("environment_names")
        if (not isinstance(argv, list)
                or any(not isinstance(item, str) or not item or "\0" in item for item in argv)
                or not isinstance(environment_names, list)
                or any(not isinstance(item, str)
                       or _ENVIRONMENT_NAME.fullmatch(item) is None
                       for item in environment_names)):
            raise AgentDeliveryError(f"invalid agent record launch vector: {path}")
        executable = launch.get("executable")
        if executable is not None and (
            not isinstance(executable, dict)
            or set(executable) != {"path", "device", "inode"}
            or not isinstance(executable.get("path"), str)
            or not os.path.isabs(cast(str, executable["path"]))
            or any(not isinstance(executable.get(key), int)
                   or isinstance(executable.get(key), bool)
                   or cast(int, executable[key]) <= 0
                   for key in ("device", "inode"))
        ):
            raise AgentDeliveryError(
                f"invalid agent record launch executable identity: {path}"
            )
        expected_program = (
            cast(dict[str, object], executable).get("path")
            if isinstance(executable, dict) else harness
        )
        if ((adapter == "herdr-foreign" and argv)
                or (adapter != "herdr-foreign"
                    and (not argv or argv[0] != expected_program))):
            raise AgentDeliveryError(f"invalid agent record launch argv: {path}")
        if adapter in ("herdr", "herdr-pane", "herdr-relay"):
            try:
                structured = harness_arguments(
                    harness,
                    model=cast(str | None, launch.get("model")),
                    resume=cast(str | None, launch.get("resume")),
                )
            except AgentDeliveryError as exc:
                raise AgentDeliveryError(
                    f"invalid nested launch presets in {path}: {exc}"
                ) from exc
            if tuple(argv[1:1 + len(structured)]) != structured:
                raise AgentDeliveryError(
                    f"nested launch argv contradicts model or resume: {path}"
                )
        if document.get("runner_identity") is not None:
            raise AgentDeliveryError(
                f"interactive nested record cannot contain runner identity: {path}"
            )
        if (adapter in ("herdr-pane", "herdr-relay")
                and lifecycle in {"running", "stopping", "stopped"}
                and document.get("custom_process_identity") is None):
            raise AgentDeliveryError(
                f"active nested Muse record has no runtime process identity: {path}"
            )
        extensions = document.get("extensions")
        if not isinstance(extensions, dict) or any(
            not isinstance(key, str) or key in expected or key in launch_fields
            for key in extensions
        ):
            raise AgentDeliveryError(f"invalid agent record extensions: {path}")
        normalized = {
            key: value for key, value in document.items()
            if key not in {"schema", "launch", "extensions", "native_session"}
        }
        normalized.update({
            "schema": 1,
            "harness": launch.get("harness"),
            "cwd": launch.get("cwd"),
            "adapter": launch.get("adapter"),
            "mode": launch.get("mode"),
            "backend": launch.get("backend"),
            "model": launch.get("model"),
            "resume": launch.get("resume"),
            "arguments": list(argv[1:]),
            "runtime_home": launch.get("runtime_home"),
            "profile": launch.get("profile"),
            "environment_names": list(environment_names),
        })
        if schema == "agentctl-session/v3":
            native = document.get("native_session")
            if native is None:
                normalized.update({
                    "session_agent": None,
                    "session_value": None,
                    "goal_session_id": None,
                })
            elif (isinstance(native, dict)
                    and set(native) == {"schema", "agent", "value", "source"}
                    and native.get("schema") == "agentctl-native-session/v1"
                    and native.get("source") in ("observed", "asserted")
                    and native.get("agent") == harness
                    and isinstance(native.get("value"), str)
                    and bool(native.get("value"))):
                normalized.update({
                    "session_agent": native.get("agent"),
                    "session_value": native.get("value"),
                    "goal_session_id": native.get("value"),
                    "native_session": dict(native),
                })
            else:
                raise AgentDeliveryError(f"invalid agent record native session: {path}")
            goal = document.get("goal")
            if (not isinstance(goal, dict)
                    or set(goal) != {
                        "schema", "objective", "message_id", "native_command",
                    }
                    or goal.get("schema") != "agentctl-goal/v1"):
                raise AgentDeliveryError(f"invalid agent record goal state: {path}")
            normalized.update({
                "goal": goal.get("objective"),
                "goal_delivery": None,
                "goal_command": goal.get("native_command"),
                "goal_messages": {},
                "goal_message_id": goal.get("message_id"),
            })
        else:
            observed = normalized.get("session_value")
            asserted = normalized.get("goal_session_id")
            session_agent = normalized.get("session_agent")
            if (observed is not None and asserted is not None
                    and observed != asserted):
                raise AgentDeliveryError(
                    f"contradictory native session identities in {path}"
                )
            if (observed is not None
                    and (session_agent != harness
                         or not isinstance(observed, str) or not observed)):
                raise AgentDeliveryError(
                    f"invalid native session identity in {path}"
                )
        return normalized

    def target(self, expected_workspace: str | None = None) -> agent.Target:
        """Pin the exact pane and, when available at launch, its durable session."""
        if not self.pane_id:
            raise AgentDeliveryError(f"agent {self.name!r} has no confirmed pane; inspect its launch error")
        return agent.Target(
            pane_id=self.pane_id,
            session_agent=(
                self.session_agent if self._session_source != "asserted" else None
            ),
            session_value=(
                self.session_value if self._session_source != "asserted" else None
            ),
            expected_agent=self.harness,
            expected_cwd=self.cwd,
            expected_workspace=expected_workspace,
        )


@dataclass(frozen=True)
class _DeadPaneProof:
    """One exact absent-agent, one-pane-tab, idle-shell observation."""

    info: AgentPaneInfo
    presentation: Pane
    shell: PaneShellProof


@dataclass(frozen=True)
class _LegacyRecordSnapshot:
    """Exact record bytes, parsed meaning, and containing directory generation."""

    record: AgentRecord
    content: bytes
    digest: str
    directory_device: int
    directory_inode: int


@dataclass(frozen=True)
class _ManagedRecordSnapshot:
    """Parsed managed record and its containing directory generation."""

    record: AgentRecord
    content: bytes
    directory_device: int
    directory_inode: int


@dataclass(frozen=True)
class _PinnedAgentDirectory:
    """Open directory generation held from first proof through publication."""

    name: str
    path: Path
    descriptor: int
    device: int
    inode: int


@dataclass(frozen=True)
class _PinnedParentDirectory:
    """Open parent-directory generation used by one namespace transaction."""

    path: Path
    descriptor: int
    device: int
    inode: int


@dataclass(frozen=True)
class _InstalledArtifact:
    """Exact private file generation installed under a pinned directory."""

    name: str
    device: int
    inode: int
    size: int
    digest: str


class _ArchivePublicationUncertainError(AgentDeliveryError):
    """The held directory cannot safely be restored through the active name."""


class _ArtifactNotInstalledError(AgentDeliveryError):
    """An artifact write failed while the prior named generation stayed intact."""


class _ArtifactInstalledError(AgentDeliveryError):
    """An exact artifact was installed before a later bounded operation failed."""

    def __init__(self, message: str, installed: _InstalledArtifact) -> None:
        super().__init__(message)
        self.installed = installed


class _ArtifactPublicationUncertainError(_ArchivePublicationUncertainError):
    """An artifact name no longer has a safely reversible generation."""


def _goal_replacement_selected(screen: str, objective: str) -> bool:
    normalized = " ".join(screen.split())
    wanted = " ".join(objective.split())
    return ("Replace goal?" in normalized
            and any(f"New objective: {wanted} {marker} 1. Replace current goal Set the new objective and start it now" in normalized
                    for marker in ("›", "❯"))
            and "2. Cancel Keep the current goal" in normalized
            and "Press enter to confirm or esc to go back" in normalized)


#: Longest wait for a sent prompt to appear in the target pane's scrollback.
READBACK_SECONDS = 5.0
#: Shorter prompts are too common to attribute to another pane by their text.
_ATTRIBUTABLE = 12
#: Inline Markdown markup a harness may drop when it prints a submitted prompt (Claude
#: shows `code` without its backticks); read-back compares the prompt and the scrollback
#: with it removed from both.
_RENDERED_MARKUP = frozenset("`*_~\\")
#: Lines of scrollback each read-back reads; a read this long may have lost old lines.
SCROLLBACK_LINES = 400
#: Opt-in for countermanding a misroute; by default a misroute is only quarantined.
COUNTERMAND_ENV = "AGENTCTL_MISROUTE_COUNTERMAND"
#: Longest wait for another pane's input lock before a countermand gives up.
_COUNTERMAND_LOCK_SECONDS = 2.0
#: Receipt evidence that already shows the prompt text in the target pane itself.
_PRINTED_EVIDENCE = ("prompt text appeared above the composer",
                     "pasted prompt appeared above the composer")
#: What a program that received someone else's prompt is told after being interrupted.
MISROUTE_NOTE = "Ignore the previous message: it was sent to the wrong agent by agentctl."
_NOTE_HARNESSES = ("claude", "codex", "muse")


def _readback_form(text: str) -> str:
    """Text as read-back compares it: no whitespace and no droppable inline markup."""
    return "".join(char for char in _compact(text) if char not in _RENDERED_MARKUP)


def _readback_tail(text: str) -> str:
    """The end of a prompt that read-back looks for in a pane."""
    return _readback_form(text)[-40:]


class RecipientUnanchored(RecipientChanged):
    """The record pins no harness process or observed session, so input is refused."""


class _GuardedTerminal:
    """Verify the recipient immediately before and immediately after every input effect.

    When the server advertises ``input-expect`` and the record pins a terminal, each
    write also carries that terminal, and Herdr refuses it atomically if the pane
    holds another terminal. Without that capability Herdr writes by pane id alone: a
    replacement that takes the pane between the last check and Herdr's write still
    receives that one effect. Either way a program exiting or exec-ing inside the
    same terminal is outside Herdr's pane table, so the process check runs before
    and after every effect, and a mismatch after a write becomes a quarantined
    probable misroute.
    """

    def __init__(
        self, client: HerdrClient, verify: Callable[[str], None],
        expect_terminal: str | None = None,
    ) -> None:
        self.client, self.verify = client, verify
        self.expect_terminal = expect_terminal
        self.effects = 0

    def read_screen(self, pane_id: str) -> str:
        return self.client.read_screen(pane_id)

    def effect(self, pane_id: str, action: Callable[[], _T]) -> _T:
        try:
            self.verify(pane_id)
        except (HerdrUnavailable, AgentDeliveryError) as exc:
            if self.effects == 0:
                raise PromptNotStaged(f"{exc}; nothing was typed") from exc
            raise RecipientChanged(
                f"{exc}; input already typed into pane {pane_id} stopped here"
            ) from exc
        try:
            result = action()
        except InputExpectationFailed as exc:
            if self.effects == 0:
                raise PromptNotStaged(f"{exc}; nothing was typed") from exc
            raise RecipientChanged(
                f"{exc}; input already typed into pane {pane_id} stopped here"
            ) from exc
        except Exception as exc:
            # The write may have been accepted before the transport failed: check where
            # it could have gone before reporting an ambiguous outcome.
            self.effects += 1
            try:
                self.verify(pane_id)
            except RecipientChanged as check:
                raise ProbableMisroute(
                    f"pane {pane_id} failed its recipient check after an input whose "
                    f"outcome is unknown ({exc}): {check}"
                ) from exc
            raise
        self.effects += 1
        try:
            self.verify(pane_id)
        except RecipientChanged as exc:
            raise ProbableMisroute(
                f"pane {pane_id} failed its recipient check immediately after input: {exc}"
            ) from exc
        return result

    def send_text(self, pane_id: str, text: str) -> None:
        if self.expect_terminal is None:
            self.effect(pane_id, lambda: self.client.send_text(pane_id, text))
        else:
            self.effect(pane_id, lambda: self.client.send_text(
                pane_id, text, expect_terminal=self.expect_terminal))

    def send_keys(self, pane_id: str, keys: str) -> None:
        if self.expect_terminal is None:
            self.effect(pane_id, lambda: self.client.send_keys(pane_id, keys))
        else:
            self.effect(pane_id, lambda: self.client.send_keys(
                pane_id, keys, expect_terminal=self.expect_terminal))

    def native_prompt(self, pane_id: str, text: str) -> None:
        self.effect(pane_id, lambda: self.client.agent_prompt(
            pane_id, text, expect_terminal=self.expect_terminal))


class _WorkspaceClient:
    """Add exact workspace checks to every queue readiness probe, after enqueue."""

    def __init__(
        self, client: HerdrClient, record: AgentRecord, *,
        queue: str | None = None, check_prompt: bool = True,
        expected_workspace: str | None = None,
        peer_panes: Callable[[], builtins.list[str]] | None = None,
        claims: Callable[[], builtins.list[tuple[str, str, str | None]]] | None = None,
        readback_seconds: float | None = None,
    ) -> None:
        self.client, self.record = client, record
        self.goal_objective: str | None = None
        self.queue = queue
        self.check_prompt = check_prompt
        self.expected_workspace = expected_workspace
        self.custom_submission: tuple[str, str, int] | None = None
        #: Panes of the other agents in this registry, searched when a prompt is missing.
        self.peer_panes = peer_panes
        #: Other registered agents' pane and terminal claims, checked before input.
        self.claims = claims
        #: Whether a misroute is countermanded (Esc and a note) instead of only quarantined.
        self.countermand_enabled = os.environ.get(COUNTERMAND_ENV) == "1"
        #: A longer read-back limit than ``READBACK_SECONDS``, for a just-started harness.
        self.readback_seconds = readback_seconds

    def _snapshot(self, pane_id: str, text: str) -> dict[str, int]:
        """Before sending: how often the prompt shows in the target and, for a prompt long
        enough to attribute, in every other registered agent's pane. Only occurrences beyond these are evidence about this send.

        A failure here happens before anything is typed, so it leaves the message pending.
        A peer pane Herdr reports as not found cannot receive the prompt: it is logged and
        skipped. Any other failure, for the target or a peer, is a failure.
        """
        panes = [pane_id]
        if len(_readback_tail(text)) >= _ATTRIBUTABLE and self.peer_panes is not None:
            panes += [peer for peer in self.peer_panes() if peer != pane_id]
        windows: dict[str, int] = {}
        for pane in panes:
            try:
                windows[pane] = self._window(pane, text)
            except (HerdrUnavailable, AgentDeliveryError) as exc:
                missing = _missing_target(exc) if pane != pane_id else None
                if missing is None:
                    raise PromptNotStaged(
                        f"cannot read scrollback before sending: {exc}; nothing was typed"
                    ) from exc
                self._log_readback(pane_id, text, "peer-skipped", {
                    "peer": pane, "finding": missing, "detail": str(exc),
                })
        return windows

    def _window(self, pane: str, text: str) -> int:
        """How often the text shows in the pane's scrollback."""
        tail = _readback_tail(text)
        if not tail:
            return 0
        return _readback_form(self.client.read_scrollback(pane)).count(tail)

    def _observe(self, pane: str, text: str, before: dict[str, int]) -> str:
        """``fresh`` when the text shows more often than before. When it does not, a pane
        that already showed it is ``uncertain``: an old occurrence may have scrolled or
        redrawn out as a new one came in, which equal counts cannot tell apart, whatever
        the window's length and even when the bytes read are unchanged. Otherwise
        ``absent``."""
        count = self._window(pane, text)
        old_count = before.get(pane, 0)
        if count > old_count:
            return "fresh"
        if old_count > 0:
            return "uncertain"
        return "absent"

    def _readback_seconds(self) -> float:
        """How long the target has to show a sent prompt."""
        return max(READBACK_SECONDS, self.readback_seconds or 0.0)

    def _seen_in_target(self, pane_id: str, text: str, before: dict[str, int]) -> bool:
        deadline = time.monotonic() + self._readback_seconds()
        while True:
            if self._observe(pane_id, text, before) == "fresh":
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(min(0.25, max(0.0, deadline - time.monotonic())))

    def _peer_states(self, pane_id: str, text: str, before: dict[str, int]) -> dict[str, str]:
        return {peer: self._observe(peer, text, before) for peer in before if peer != pane_id}

    def _deliver_checked(
        self, pane_id: str, command: str, guarded: _GuardedTerminal,
        submit: Callable[[], SubmissionReceipt | None],
    ) -> SubmissionReceipt | None:
        """Submit, then attribute the prompt to where it actually appeared."""
        before = self._snapshot(pane_id, command)
        try:
            receipt = submit()
        except ProbableMisroute as exc:
            self._countermand(pane_id, command, "identity-changed-after-write", str(exc))
        except PromptNotStaged:
            raise
        except Exception as exc:
            if guarded.effects == 0:
                raise
            # The outcome of a write is unknown (lost acknowledgement, composer
            # failure). Text in the target is no proof of submission (an unsubmitted
            # paste shows in the composer), so it stays possibly submitted; the read-back
            # only looks for the prompt in another agent's pane.
            self._located(pane_id, command, before, str(exc))
            raise
        self._read_back(pane_id, command, receipt, before)
        return receipt

    def _located(
        self, pane_id: str, text: str, before: dict[str, int], detail: str,
    ) -> None:
        """After an unknown outcome: countermand a prompt that newly shows in exactly one
        other agent's pane and not in the target; otherwise only record what was seen.
        The caller reports the write as possibly submitted either way."""
        in_target = self._seen_in_target(pane_id, text, before)
        states = self._peer_states(pane_id, text, before)
        fresh = [peer for peer, state in states.items() if state == "fresh"]
        if len(fresh) == 1 and not in_target:
            self._countermand(fresh[0], text, "prompt-in-another-pane", detail)
        self._log_readback(pane_id, text, "unknown-outcome", {
            "in_target": in_target, "peers": states, "detail": detail,
        })

    def _read_back(
        self, pane_id: str, text: str, receipt: SubmissionReceipt | None,
        before: dict[str, int],
    ) -> None:
        """Prove the prompt reached this record's pane, or quarantine it.

        Evidence for the target: a receipt that saw the prompt printed there, or the
        prompt newly in its scrollback within the read-back limit (``READBACK_SECONDS``,
        or the working timeout for a start brief), each followed by a
        passing recipient check. Every other registered agent's pane is inspected as
        well. The prompt newly in exactly one of them, and not in the target, is a
        misroute (quarantined; countermanded only when that is enabled). Newly in the
        target and a peer, in several peers, or a peer whose full window cannot rule
        out a new occurrence: quarantined without a note. A prompt the target never
        shows is unproven and quarantined, whatever confirmed the submission.
        """
        try:
            self.verify_recipient(pane_id)
        except RecipientChanged as exc:
            self._countermand(pane_id, text, "identity-changed-after-write", str(exc))
        in_target = (receipt is not None and receipt.evidence in _PRINTED_EVIDENCE) or (
            self._seen_in_target(pane_id, text, before))
        try:
            self.verify_recipient(pane_id)
        except RecipientChanged as exc:
            self._countermand(pane_id, text, "identity-changed-after-write", str(exc))
        states = self._peer_states(pane_id, text, before)
        fresh = [peer for peer, state in states.items() if state == "fresh"]
        uncertain = [peer for peer, state in states.items() if state == "uncertain"]
        if len(fresh) == 1 and not in_target:
            self._countermand(fresh[0], text, "prompt-in-another-pane",
                              f"not newly in pane {pane_id} after {self._readback_seconds():g}s")
        if fresh or uncertain:
            self._log_readback(pane_id, text, "ambiguous", {"in_target": in_target, "peers": states})
            raise ProbableMisroute(
                f"prompt for agent {self.record.name!r} cannot be attributed: target "
                f"{'shows' if in_target else 'does not show'} it, other panes {states}; "
                "quarantined without a note"
            )
        if in_target:
            return
        # A confirmation without target evidence proves nothing about the recipient.
        logged = self._log_readback(pane_id, text, "not-seen", {"receipt": receipt is not None})
        raise AgentDeliveryError(
            f"prompt for agent {self.record.name!r} was submitted but never showed in pane "
            f"{pane_id} within {self._readback_seconds():g}s; delivery is unproven"
            + ("" if logged else " (the read-back log could not be written)")
        )

    def _occupant(self, pane: str) -> tuple[str | None, str | None, CustomProcessIdentity | None]:
        info = self.client.pane_info(pane)
        harness = (self.client.harness_identity(pane, info.agent)
                   if info.agent in _NOTE_HARNESSES else None)
        return info.agent, info.terminal_id, harness

    def _countermand(self, wrong_pane: str, text: str, detection: str, detail: str) -> NoReturn:
        """Interrupt the program that got this prompt and tell it to ignore it, once.

        The wrong pane's input lock is taken (within a bounded wait; this sender already
        holds its own target's lock) so no other agentctl input interleaves. The occupant
        seen now (terminal and harness process) is re-verified before the interrupt,
        before the note, and after it, and the note must newly show in that pane.
        ``MisrouteRecovered`` (one retry) needs all of that; anything less raises
        ``ProbableMisroute`` (quarantine, no retry).
        """
        observed_agent = observed_terminal = None
        occupant: CustomProcessIdentity | None = None
        interrupted = note_sent = note_confirmed = False
        skipped: str | None = None
        lock = None
        if not self.countermand_enabled:
            logged = self._log_misroute({
                "at": time.time(), "agent": self.record.name, "token": self.record.token,
                "detection": detection, "detail": detail,
                "intended_pane": self.record.pane_id, "intended_terminal": self.record.terminal_id,
                "observed_pane": wrong_pane, "interrupted": False, "note_sent": False,
                "note_confirmed": False,
                "skipped": f"automatic countermand is off ({COUNTERMAND_ENV}=1 enables it); nothing typed",
                "message_id": self._inflight_message_id(text),
            })
            raise ProbableMisroute(
                f"prompt for agent {self.record.name!r} probably reached pane {wrong_pane} "
                f"({detection}); quarantined, nothing typed into any pane: {detail}"
                + ("" if logged else " (the misroute log could not be written)")
            )
        if wrong_pane != self.record.pane_id:
            try:
                lock = agent._lock_target_within(wrong_pane, "pane input lock", _COUNTERMAND_LOCK_SECONDS)
            except AgentDeliveryError as exc:
                skipped = f"{exc}; nothing typed"
        try:
            if skipped is None:
                try:
                    observed_agent, observed_terminal, occupant = self._occupant(wrong_pane)
                except HerdrUnavailable as exc:
                    skipped = f"pane unavailable: {exc}"
            if skipped is None and occupant is None:
                skipped = (f"pane shows {observed_agent!r} without one verifiable harness "
                           "process; nothing typed")
            expect = (observed_terminal
                      if observed_terminal is not None and self.client.input_expect_supported()
                      else None)

            def same_occupant() -> bool:
                try:
                    return self._occupant(wrong_pane)[1:] == (observed_terminal, occupant)
                except HerdrUnavailable:
                    return False

            if skipped is None:
                try:
                    if not same_occupant():
                        skipped = "occupant changed before the interrupt; nothing typed"
                    else:
                        self.client.send_keys(wrong_pane, "esc", expect_terminal=expect)
                        interrupted = True
                        time.sleep(0.5)
                        note_before = {wrong_pane: self._window(wrong_pane, MISROUTE_NOTE)}
                        if not same_occupant():
                            skipped = "occupant changed after the interrupt; note not sent"
                        else:
                            self.client.agent_prompt(wrong_pane, MISROUTE_NOTE, expect_terminal=expect)
                            note_sent = True
                            deadline = time.monotonic() + READBACK_SECONDS
                            while not note_confirmed:
                                note_confirmed = (
                                    self._observe(wrong_pane, MISROUTE_NOTE, note_before) == "fresh"
                                    and same_occupant())
                                if note_confirmed or time.monotonic() >= deadline:
                                    break
                                time.sleep(0.25)
                            if not note_confirmed:
                                skipped = ("the note did not show in that pane for the same "
                                           "occupant; its recipient is unproven")
                except (HerdrUnavailable, AgentDeliveryError) as exc:
                    skipped = f"recovery input failed: {exc}"
        finally:
            if lock is not None:
                lock.close()
        logged = self._log_misroute({
            "at": time.time(), "agent": self.record.name, "token": self.record.token,
            "detection": detection, "detail": detail,
            "intended_pane": self.record.pane_id, "intended_terminal": self.record.terminal_id,
            "observed_pane": wrong_pane, "observed_terminal": observed_terminal,
            "observed_agent": observed_agent, "interrupted": interrupted,
            "note_sent": note_sent, "note_confirmed": note_confirmed, "skipped": skipped,
            "message_id": self._inflight_message_id(text),
        })
        suffix = "" if logged else " (the misroute log could not be written)"
        if note_confirmed and logged:
            raise MisrouteRecovered(
                f"prompt for agent {self.record.name!r} reached pane {wrong_pane} "
                f"({detection}); that program was interrupted and told to ignore it: {detail}"
            )
        raise ProbableMisroute(
            f"prompt for agent {self.record.name!r} probably reached pane {wrong_pane} "
            f"({detection}) and was not proven countermanded ({skipped}): {detail}{suffix}"
        )

    def _inflight_message_id(self, text: str) -> str | None:
        """The message being delivered: its inflight file, matched by text or, since
        the inflight barrier holds one message at a time, the only one present."""
        if self.queue is None:
            return None
        paths = sorted((Path(self.queue) / "inflight").glob("*.json"))
        for path in paths:
            try:
                document = agent._read_queue_json(str(path), "queued message", require_private=True)
            except AgentDeliveryError:
                continue
            if isinstance(document, dict) and document.get("text") == text:
                return path.name[:-5]
        return paths[0].name[:-5] if len(paths) == 1 else None

    def _append_log(self, filename: str, entry: dict[str, object]) -> bool:
        """Append one fsynced line to a log beside the queue; False when it failed."""
        if self.queue is None:
            return False
        try:
            descriptor = os.open(
                Path(self.queue).parent / filename,
                os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600,
            )
            try:
                os.write(descriptor, (json.dumps(entry, sort_keys=True) + "\n").encode("utf-8"))
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        except OSError:
            return False
        return True

    def _log_misroute(self, entry: dict[str, object]) -> bool:
        return self._append_log("misroutes.jsonl", entry)

    def _log_readback(
        self, pane_id: str, text: str, result: str, evidence: dict[str, object],
    ) -> bool:
        return self._append_log("readback.jsonl", {
            "at": time.time(), "agent": self.record.name, "pane": pane_id,
            "result": result, "evidence": evidence,
            "message_id": self._inflight_message_id(text),
        })

    def _guarded(self) -> _GuardedTerminal:
        expect = None
        if self.record.terminal_id is not None:
            try:
                supported = self.client.input_expect_supported()
            except HerdrUnavailable as exc:
                raise PromptNotStaged(
                    f"cannot read the Herdr server's input capabilities: {exc}; nothing was typed"
                ) from exc
            expect = self.record.terminal_id if supported else None
        return _GuardedTerminal(self.client, self.verify_recipient, expect)

    def verify_recipient(self, pane_id: str, *, presentation: bool = True) -> None:
        """Prove that the pane still holds this record's recipient; called around each effect.

        A definite mismatch raises ``RecipientChanged``; a check that cannot be made
        raises another error, so a post-write check never mistakes an outage for a
        misroute. ``presentation=False`` skips only the tab label and Herdr name, for
        repairing them.
        """
        record = self.record
        if pane_id != record.pane_id:
            raise RecipientChanged(f"refusing input to pane {pane_id}: agent {record.name!r} owns {record.pane_id}")
        failures: list[str] = []
        if record.adapter == "herdr" and presentation:
            identity = self.client.agent_identity(record.name)
            if identity.pane_id != record.pane_id:
                failures.append(f"Herdr agent {record.name!r} is in pane {identity.pane_id}")
        info = self.client.pane_info(pane_id)
        if info.workspace_id != record.workspace_id:
            failures.append(f"workspace is {info.workspace_id!r}, recorded {record.workspace_id!r}")
        if os.path.realpath(info.cwd) != os.path.realpath(record.cwd):
            failures.append(f"cwd is {info.cwd!r}, recorded {record.cwd!r}")
        if record.terminal_id is not None and info.terminal_id != record.terminal_id:
            failures.append(f"terminal is {info.terminal_id!r}, recorded {record.terminal_id!r}")
        if record.adapter != "herdr-foreign" and record.tab_id is not None:
            if info.tab_id is not None and info.tab_id != record.tab_id:
                failures.append(f"tab is {info.tab_id!r}, recorded {record.tab_id!r}")
            label = self.client.tab_label(record.tab_id) if presentation else record.name
            if label != record.name:
                failures.append(f"tab label is {label!r}, expected {record.name!r}")
        if not _session_matches(record, info):
            failures.append(
                f"native session is {info.session_agent!r}/{info.session_value!r}, recorded "
                f"{record.session_agent or record.harness!r}/{record.session_value!r}"
            )
        if failures:
            raise RecipientChanged(
                f"refusing input to agent {record.name!r}: " + "; ".join(failures)
                + "; run `agentctl doctor`"
            )
        if self.claims is not None:
            for name, pane, terminal in self.claims():
                if pane == pane_id or (terminal is not None and terminal == info.terminal_id):
                    raise RecipientChanged(
                        f"refusing input to agent {record.name!r}: registered agent {name!r} "
                        f"also claims pane {pane_id}; run `agentctl doctor`"
                    )
        if record.adapter == "herdr-pane":
            self.client.verify_custom_harness(
                pane_id, record.harness, record.custom_process_identity
            )
            return
        if record.adapter == "herdr-relay":
            if (record.custom_process_identity is None
                    or self.client.relay_process(pane_id) != record.custom_process_identity):
                raise RecipientChanged(
                    f"refusing input to agent {record.name!r}: its relayed harness process changed"
                )
            return
        if record.adapter == "herdr-foreign" and record.harness == "muse":
            self._verify_muse_harness(pane_id)
            return
        if record.harness_anchor is not None:
            if not self.client.verify_harness_identity(pane_id, record.harness_anchor):
                raise RecipientChanged(
                    f"refusing input to agent {record.name!r}: the anchored {record.harness} "
                    f"process (pid {record.harness_anchor.pid}) is no longer a foreground "
                    f"process of pane {pane_id}; run `agentctl doctor`"
                )
            return
        if record.session_value is not None and record._session_source != "asserted":
            return
        raise RecipientUnanchored(
            f"refusing input to agent {record.name!r}: its record pins no harness process or "
            "observed native session, so a replacement in the same pane could not be told "
            f"apart; check that pane {pane_id} runs the intended agent, then run "
            f"`agentctl anchor {record.name}`"
        )

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        # Sessions started by this manager have a second lifecycle identity in
        # Herdr's named-agent registry.  Adopted sessions deliberately do not:
        # assigning or replacing a native name would mutate a runtime this
        # registry does not own.  Their exact pane/session/workspace/cwd/harness
        # assertions remain the authority instead.
        if (self.record.adapter == "herdr"
                and self.client.agent_pane(self.record.name) != self.record.pane_id):
            raise HerdrUnavailable(f"agent {self.record.name!r} no longer owns its recorded pane")
        info = self.client.pane_info(pane_id)
        if info.workspace_id != self.record.workspace_id:
            raise HerdrUnavailable(f"agent {self.record.name!r} workspace identity changed")
        if (self.expected_workspace is not None
                and self.client.workspace_label(info.workspace_id)
                != self.expected_workspace):
            raise HerdrUnavailable(
                f"refusing pane {pane_id}: workspace is "
                f"{self.client.workspace_label(info.workspace_id)!r}, expected "
                f"{self.expected_workspace!r}"
            )
        if pane_id == self.record.pane_id:
            if self.record.goal_session_id is not None and info.session_value is not None and info.session_value != self.record.goal_session_id:
                raise HerdrUnavailable(f"agent {self.record.name!r} native session identity changed")
            if self.check_prompt and info.status in ("idle", "done") and info.agent == "claude":
                screen = self.client.read(pane_id, source="visible", lines=200)
                if ("Quick safety check: Is this a project you created or one you trust?" in screen
                    and "No, exit" in screen and "Yes, I trust this folder" in screen):
                    raise HerdrUnavailable("Claude workspace trust prompt requires human attention; no input was submitted")
            if self.record.adapter == "herdr-relay":
                return self._relay_pane_info(info)
            if self._uses_muse_composer():
                self._verify_muse_harness(pane_id)
                reported_status = info.status
                screen = self.client.read(pane_id, source="visible", lines=200)
                self._verify_muse_harness(pane_id)
                if muse_trust_prompt(screen):
                    raise HerdrUnavailable(
                        "Muse workspace trust prompt requires human attention; no input was submitted"
                    )
                info = AgentPaneInfo(
                    pane_id=info.pane_id,
                    workspace_id=info.workspace_id,
                    cwd=info.cwd,
                    agent=info.agent,
                    status=(
                        "paused"
                        if (reported_status in ("idle", "done")
                            and muse_verified_process_goal_paused(screen))
                        else "idle"
                        if (
                            muse_idle_composer(screen)
                            or (
                                reported_status in ("idle", "done")
                                and muse_verified_process_idle_composer(screen)
                            )
                        )
                        else "staged"
                        if (reported_status in ("idle", "done")
                            and muse_verified_process_composer(screen))
                        else "working"
                    ),
                    session_agent=info.session_agent,
                    session_value=info.session_value,
                    terminal_id=info.terminal_id,
                    tab_id=info.tab_id,
                )
        return info

    def _uses_muse_composer(self) -> bool:
        """Foreign Muse retains its ownership class and shares guarded editor input."""
        return (self.record.harness == "muse"
                and self.record.adapter in ("herdr-pane", "herdr-foreign"))

    def _verify_muse_harness(self, pane_id: str) -> None:
        """Check the appropriate stored process pin without acquiring a new one."""
        if self.record.adapter == "herdr-pane":
            self.client.verify_custom_harness(
                pane_id, self.record.harness, self.record.custom_process_identity
            )
        else:
            if (self.record.foreign_shell_identity is None
                    or self.client.pane_shell_identity(pane_id) != self.record.foreign_shell_identity):
                raise RecipientChanged(
                    f"adopted Muse in pane {pane_id} no longer holds its pinned shell process"
                )
            if (self.record.harness_anchor is None
                    or not self.client.verify_harness_identity(pane_id, self.record.harness_anchor)):
                raise RecipientChanged(
                    f"adopted Muse in pane {pane_id} no longer holds its pinned foreground process"
                )

    def codex_idle_ready(self, info: AgentPaneInfo) -> bool:
        """Add Codex readiness only between existing record recipient checks."""
        if (self.record.harness != "codex" or info.agent != "codex"
                or info.status != "unknown" or info.pane_id != self.record.pane_id):
            return False
        checker = getattr(self.client, "codex_idle_ready", None)
        if not callable(checker):
            return False
        self.verify_recipient(info.pane_id)
        if not checker(info):
            return False
        self.verify_recipient(info.pane_id)
        return True

    def _relay_pane_info(self, info: AgentPaneInfo) -> AgentPaneInfo:
        """State of a harness behind a root slot relay, from Herdr's live screen rules.

        Herdr reports the state agentctl last reported for such a pane, so the
        live verdict comes from ``agent explain``, and a changed verdict is
        reported back so Herdr's own listing stays current.
        """
        self.client.verify_relay_harness(info.pane_id, self.record.custom_process_identity)
        agent_kind, state = self.client.explain_agent(info.pane_id)
        if agent_kind != self.record.harness:
            raise HerdrUnavailable(
                f"pane {info.pane_id} no longer shows a {self.record.harness} screen"
            )
        if state == "blocked":
            raise HerdrUnavailable(
                f"{self.record.harness} behind the slot relay is waiting for human attention; "
                "no input was submitted"
            )
        if state != "working":
            # Claude keeps its prompt box on screen while it works, and Herdr's
            # rules then read that box as idle; the interrupt hint is the tell.
            screen = self.client.read(info.pane_id, source="visible", lines=200)
            if relay_trust_prompt(screen):
                raise HerdrUnavailable(
                    f"{self.record.harness} workspace trust prompt requires human attention; "
                    "no input was submitted"
                )
            if RELAY_WORKING_MARKER in screen:
                state = "working"
        if (state != info.status and not (state == "idle" and info.status == "done")
                and state in ("idle", "working", "unknown")):
            self.client.report_pane_agent(info.pane_id, self.record.harness, state)
        return AgentPaneInfo(
            pane_id=info.pane_id, workspace_id=info.workspace_id, cwd=info.cwd,
            agent=self.record.harness, status=state,
            session_agent=info.session_agent, session_value=info.session_value,
            terminal_id=info.terminal_id, tab_id=info.tab_id,
        )

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        del workspace_id
        return self.client.panes(self.record.workspace_id)

    def workspace_label(self, workspace_id: str) -> str:
        return self.client.workspace_label(workspace_id)

    def prompt_agent(self, pane_id: str, command: str) -> SubmissionReceipt | None:
        self.goal_objective = None
        if self.queue is not None:
            for identifier, objective in self.record.goal_messages.items():
                path = Path(self.queue) / "inflight" / f"{identifier}.json"
                if command == f"/goal {objective}" and os.path.lexists(path):
                    document = agent._read_queue_json(str(path), "goal message", require_private=True)
                    if isinstance(document, dict) and document.get("text") == command:
                        self.goal_objective = objective
                        break
        if self.record.adapter == "herdr-relay":
            if command.lstrip().startswith("/"):
                raise HerdrUnavailable(
                    "slash commands cannot be delivered to a harness behind a root slot relay"
                )
            if self.pane_info(pane_id).status != "idle":
                raise HerdrUnavailable(f"relayed {self.record.harness} in pane {pane_id} is not idle")
            relay = self._guarded()
            return self._deliver_checked(
                pane_id, command, relay,
                lambda: submit_verified(relay, pane_id, self.record.harness, command),
            )
        if not self._uses_muse_composer():
            native = self._guarded()
            return self._deliver_checked(
                pane_id, command, native,
                lambda: self.client.prompt_agent(pane_id, command, terminal=native),
            )
        if "\0" in command or "\x1b" in command:
            raise HerdrUnavailable(
                "Muse pane prompts cannot contain NUL or terminal escape characters"
            )
        info = self.pane_info(pane_id)
        if info.status != "idle":
            raise HerdrUnavailable(
                f"custom pane {pane_id} is not at a verified idle Muse composer"
            )
        before = self.client.read(pane_id, source="visible", lines=200)
        if not muse_auto_review_idle_composer(before):
            raise HerdrUnavailable(
                "Muse input requires the legacy reviewed Auto-review composer; "
                "current YOLO input remains pending until Herdr provides an "
                "effect-coupled process-generation guard"
            )
        guarded = self._guarded()
        guarded.send_text(
            pane_id, f"{_BRACKETED_PASTE_START}{command}{_BRACKETED_PASTE_END}"
        )
        deadline = time.monotonic() + 2.0
        staged = ""
        while time.monotonic() < deadline:
            self._verify_muse_harness(pane_id)
            staged = self.client.read(pane_id, source="visible", lines=200)
            self._verify_muse_harness(pane_id)
            if staged != before and muse_prompt_is_exact_composer(staged, command):
                break
            time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
        else:
            raise HerdrUnavailable(
                "literal text insertion did not produce exact visible Muse editor evidence; Enter was not sent"
            )
        guarded.send_keys(pane_id, "Enter")
        self.custom_submission = (
            staged, command, muse_prompt_transcript_count(staged, command),
        )
        return None

    def wait_agent_status(self, pane_id: str, status: str, timeout_ms: int) -> None:
        """Wait for ``status``; a delivery confirmation ("working") also needs the pinned
        recipient to hold afterwards, so a replacement's state never confirms delivery."""
        self._wait_agent_status_unchecked(pane_id, status, timeout_ms)
        if status == "working":
            self.verify_recipient(pane_id)

    def _wait_agent_status_unchecked(self, pane_id: str, status: str, timeout_ms: int) -> None:
        if self.record.adapter == "herdr-relay":
            deadline = time.monotonic() + timeout_ms / 1000
            while True:
                observed = self.pane_info(pane_id).status
                if observed == status:
                    return
                if time.monotonic() >= deadline:
                    raise HerdrUnavailable(
                        f"relayed {self.record.harness} in pane {pane_id} did not become {status} "
                        f"within {timeout_ms} ms (last {observed})"
                    )
                time.sleep(min(0.2, max(0.0, deadline - time.monotonic())))
        if self._uses_muse_composer() and status == "working":
            if self.custom_submission is None:
                raise HerdrUnavailable(
                    "custom pane harness has no pending submission receipt"
                )
            staged, command, prior_count = self.custom_submission
            deadline = time.monotonic() + timeout_ms / 1000
            while time.monotonic() < deadline:
                self._verify_muse_harness(pane_id)
                screen = self.client.read(pane_id, source="visible", lines=200)
                self._verify_muse_harness(pane_id)
                if (screen != staged
                        and muse_prompt_transcript_count(screen, command) > prior_count
                        and not muse_prompt_in_composer(screen, command)):
                    self.custom_submission = None
                    return
                time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
            raise HerdrUnavailable(
                "Muse did not show a verified post-Enter screen transition"
            )
        if self.goal_objective is None or status != "working":
            self.client.wait_agent_status(pane_id, status, timeout_ms)
            return
        started = time.monotonic()
        try:
            self.client.wait_agent_status(pane_id, status, min(1000, timeout_ms))
            return
        except HerdrUnavailable:
            self.pane_info(pane_id)
            screen = self.client.read(pane_id, source="visible", lines=200)
            if _goal_replacement_selected(screen, self.goal_objective):
                try:
                    self._guarded().send_keys(pane_id, "Enter")
                except ProbableMisroute as exc:
                    self._countermand(pane_id, "Enter", "identity-changed-after-write",
                                      f"goal confirmation: {exc}")
            remaining = max(1, timeout_ms - int((time.monotonic() - started) * 1000))
            self.client.wait_agent_status(pane_id, status, remaining)

    def read(self, pane_id: str, *, source: str, lines: int) -> str:
        return self.client.read(pane_id, source=source, lines=lines)


class ManagedAgents:
    """Registry-backed API for a coordinator's visible foreign-harness workers."""

    def __init__(self, client: HerdrClient, registry: str | Path = ".herdr-agents") -> None:
        self.client = client
        self.registry = Path(os.path.abspath(registry))
        # Tests may set this explicit override. Production policy is read for
        # each operation that needs it, so a long-running manager cannot retain
        # a stale placement label.
        self.project_workspace: str | None = None

    def _project_workspace(self) -> str | None:
        return self.project_workspace or workspace_for_registry(self.registry)

    def _target(self, record: AgentRecord) -> agent.Target:
        return record.target()

    def _policy_target(self, record: AgentRecord) -> agent.Target:
        return record.target(self._project_workspace())

    def _prepare(self) -> None:
        self.registry.mkdir(mode=0o700, parents=True, exist_ok=True)
        agent._validate_private_directory(str(self.registry), "agent registry", tighten=True)

    @contextmanager
    def _lock(self, name: str) -> Iterator[None]:
        self._prepare()
        # Locks live outside agent directories, so stop/reuse cannot replace a held inode.
        path = self.registry / f".{_name(name)}.lock"
        descriptor = agent._open_private_lock(str(path), "agent lifecycle lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            yield
        finally:
            os.close(descriptor)

    @contextmanager
    def _identity_transaction(self) -> Iterator[None]:
        """Serialize live native-session claims across start and adoption."""
        self._prepare()
        descriptor = agent._open_private_lock(
            str(self.registry / ".identity.lock"), "agent identity lock"
        )
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            yield
        finally:
            os.close(descriptor)

    def _directory(self, name: str) -> Path:
        return self.registry / _name(name)

    def _load(self, name: str) -> AgentRecord:
        directory = self._directory(name)
        agent._validate_private_directory(str(self.registry), "agent registry")
        self._refuse_pending_rename(name)
        if not directory.exists():
            raise AgentDeliveryError(f"unknown agent {name!r}; use list to inspect the registry")
        agent._validate_private_directory(str(directory), "agent directory")
        return AgentRecord.load(directory / "agent.json", name)

    def _load_expected(self, name: str, expected_token: str | None = None) -> AgentRecord:
        record = self._load(name)
        if expected_token is not None and record.token != expected_token:
            raise AgentDeliveryError(f"agent {name!r} was replaced before this operation")
        return record

    @contextmanager
    def _pinned_agent_directory(self, name: str) -> Iterator[_PinnedAgentDirectory]:
        """Hold one private agent-directory inode across proof and publication."""
        path = self._directory(name)
        agent._validate_private_directory(str(path), "agent directory")
        try:
            before = os.stat(path, follow_symlinks=False)
            if (not stat.S_ISDIR(before.st_mode) or before.st_uid != os.getuid()
                    or stat.S_IMODE(before.st_mode) & 0o077):
                raise AgentDeliveryError("agent directory is not a private directory")
            descriptor = os.open(
                path,
                os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
            )
        except AgentDeliveryError:
            raise
        except OSError as exc:
            raise AgentDeliveryError(f"cannot pin agent directory: {exc}") from exc
        try:
            try:
                opened = os.fstat(descriptor)
            except OSError as exc:
                raise AgentDeliveryError(
                    f"cannot inspect pinned agent directory: {exc}"
                ) from exc
            if (not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.getuid()
                    or stat.S_IMODE(opened.st_mode) & 0o077
                    or (opened.st_dev, opened.st_ino)
                    != (before.st_dev, before.st_ino)):
                raise AgentDeliveryError(
                    "agent directory changed or is not private while being pinned"
                )
            pinned = _PinnedAgentDirectory(
                name=name,
                path=path,
                descriptor=descriptor,
                device=before.st_dev,
                inode=before.st_ino,
            )
            self._verify_pinned_agent_directory(pinned)
            yield pinned
        finally:
            _close_descriptor(
                descriptor, "pinned agent directory", primary=sys.exc_info()[1],
            )

    @staticmethod
    def _verify_pinned_agent_directory(pinned: _PinnedAgentDirectory) -> None:
        """Require the registry pathname to name the held directory generation."""
        try:
            path = os.stat(pinned.path, follow_symlinks=False)
            opened = os.fstat(pinned.descriptor)
        except OSError as exc:
            raise AgentDeliveryError(
                f"agent {pinned.name!r} registry directory changed: {exc}"
            ) from exc
        if (not stat.S_ISDIR(path.st_mode) or path.st_uid != os.getuid()
                or stat.S_IMODE(path.st_mode) & 0o077
                or not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.getuid()
                or stat.S_IMODE(opened.st_mode) & 0o077
                or (path.st_dev, path.st_ino) != (pinned.device, pinned.inode)
                or (opened.st_dev, opened.st_ino) != (pinned.device, pinned.inode)):
            raise AgentDeliveryError(
                f"agent {pinned.name!r} registry directory changed"
            )

    @staticmethod
    @contextmanager
    def _pinned_parent_directory(
        path: Path, *, label: str,
    ) -> Iterator[_PinnedParentDirectory]:
        agent._validate_private_directory(str(path), label)
        try:
            before = os.stat(path, follow_symlinks=False)
            if (not stat.S_ISDIR(before.st_mode) or before.st_uid != os.getuid()
                    or stat.S_IMODE(before.st_mode) & 0o077):
                raise AgentDeliveryError(f"{label} is not a private directory")
            descriptor = os.open(
                path,
                os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
            )
        except AgentDeliveryError:
            raise
        except OSError as exc:
            raise AgentDeliveryError(f"cannot pin {label}: {exc}") from exc
        try:
            try:
                opened = os.fstat(descriptor)
            except OSError as exc:
                raise AgentDeliveryError(f"cannot inspect pinned {label}: {exc}") from exc
            if (not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.getuid()
                    or stat.S_IMODE(opened.st_mode) & 0o077
                    or (opened.st_dev, opened.st_ino)
                    != (before.st_dev, before.st_ino)):
                raise AgentDeliveryError(
                    f"{label} changed or is not private while being pinned"
                )
            pinned = _PinnedParentDirectory(
                path=path,
                descriptor=descriptor,
                device=before.st_dev,
                inode=before.st_ino,
            )
            ManagedAgents._verify_pinned_parent_directory(pinned, label=label)
            yield pinned
        finally:
            _close_descriptor(
                descriptor, f"pinned {label}", primary=sys.exc_info()[1],
            )

    @staticmethod
    def _verify_pinned_parent_directory(
        pinned: _PinnedParentDirectory, *, label: str,
    ) -> None:
        try:
            path = os.stat(pinned.path, follow_symlinks=False)
            opened = os.fstat(pinned.descriptor)
        except OSError as exc:
            raise AgentDeliveryError(f"{label} directory changed: {exc}") from exc
        if (not stat.S_ISDIR(path.st_mode) or path.st_uid != os.getuid()
                or stat.S_IMODE(path.st_mode) & 0o077
                or not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.getuid()
                or stat.S_IMODE(opened.st_mode) & 0o077
                or (path.st_dev, path.st_ino) != (pinned.device, pinned.inode)
                or (opened.st_dev, opened.st_ino) != (pinned.device, pinned.inode)):
            raise AgentDeliveryError(f"{label} directory changed")

    def _record_bytes(
        self, pinned: _PinnedAgentDirectory, *, require_active_name: bool = True,
        file_name: str = "agent.json",
    ) -> bytes:
        """Read one bounded record relative to the held directory generation."""
        if file_name not in ("agent.json", "stopped.json") and re.fullmatch(r"[a-z0-9-]{1,80}\.json", file_name) is None:
            raise AgentDeliveryError("unsupported pinned record artifact")
        path = pinned.path / file_name
        if require_active_name:
            self._verify_pinned_agent_directory(pinned)
        flags = (
            os.O_RDONLY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_NONBLOCK", 0)
        )
        descriptor = -1
        try:
            descriptor = os.open(file_name, flags, dir_fd=pinned.descriptor)
            before = os.fstat(descriptor)
            if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
                    or stat.S_IMODE(before.st_mode) & 0o077 or before.st_nlink != 1
                    or before.st_size > _MAX_AGENT_RECORD_BYTES):
                raise AgentDeliveryError(f"unsafe agent record for recovery: {path}")
            content = bytearray()
            while True:
                remaining = _MAX_AGENT_RECORD_BYTES + 1 - len(content)
                if remaining <= 0:
                    raise AgentDeliveryError(
                        f"agent record exceeds {_MAX_AGENT_RECORD_BYTES} bytes: {path}"
                    )
                block = os.read(descriptor, min(64 << 10, remaining))
                if not block:
                    break
                content.extend(block)
            after = os.fstat(descriptor)
            named = os.stat(file_name, dir_fd=pinned.descriptor, follow_symlinks=False)
            if (before.st_size != len(content)
                    or (named.st_dev, named.st_ino) != (before.st_dev, before.st_ino)
                    or (after.st_dev, after.st_ino, after.st_mode, after.st_uid,
                        after.st_nlink, after.st_size, after.st_mtime_ns,
                        after.st_ctime_ns) != (
                            before.st_dev, before.st_ino, before.st_mode, before.st_uid,
                            before.st_nlink, before.st_size, before.st_mtime_ns,
                            before.st_ctime_ns,
                        )):
                raise AgentDeliveryError(f"agent record changed while hashing: {path}")
            if require_active_name:
                self._verify_pinned_agent_directory(pinned)
            return bytes(content)
        except AgentDeliveryError:
            raise
        except OSError as exc:
            raise AgentDeliveryError(f"cannot hash agent record {path}: {exc}") from exc
        finally:
            if descriptor >= 0:
                _close_descriptor(
                    descriptor, "agent record", primary=sys.exc_info()[1],
                )

    def _legacy_record_snapshot(
        self, pinned: _PinnedAgentDirectory, *, expected_token: str,
        expected_digest: str,
    ) -> _LegacyRecordSnapshot:
        """Bind exact recovery bytes, meaning, digest, and directory generation."""
        name = pinned.name
        path = self._directory(name) / "agent.json"
        if _SHA256.fullmatch(expected_digest) is None:
            raise AgentDeliveryError(
                "expected-record-sha256 must be exactly 64 lowercase hexadecimal characters"
            )
        content = self._record_bytes(pinned)
        digest = hashlib.sha256(content).hexdigest()
        try:
            document = json.loads(content)
        except (UnicodeError, json.JSONDecodeError) as exc:
            raise AgentDeliveryError(
                f"cannot inspect legacy agent record {path}: {exc}"
            ) from exc
        if not isinstance(document, dict):
            raise AgentDeliveryError(f"invalid legacy agent record: {path}")
        if "foreign_shell_identity" in document:
            raise AgentDeliveryError(
                "--recover-legacy-adoption requires foreign_shell_identity to be absent, "
                "not null or populated"
            )
        record = AgentRecord._from_value(document, path, name)
        if record.token != expected_token or digest != expected_digest:
            raise AgentDeliveryError(
                f"agent {name!r} record changed before adoption recovery"
            )
        return _LegacyRecordSnapshot(
            record=record,
            content=content,
            digest=digest,
            directory_device=pinned.device,
            directory_inode=pinned.inode,
        )

    def _managed_record_snapshot(
        self, pinned: _PinnedAgentDirectory, *, expected_token: str,
    ) -> _ManagedRecordSnapshot:
        """Bind a managed record read to one containing directory generation."""
        content = self._record_bytes(pinned)
        path = pinned.path / "agent.json"
        try:
            document = json.loads(content)
        except (UnicodeError, json.JSONDecodeError) as exc:
            raise AgentDeliveryError(f"cannot inspect agent record {path}: {exc}") from exc
        record = AgentRecord._from_value(document, path, pinned.name)
        if record.token != expected_token:
            raise AgentDeliveryError(
                f"agent {pinned.name!r} was replaced before this operation"
            )
        return _ManagedRecordSnapshot(
            record=record,
            content=content,
            directory_device=pinned.device,
            directory_inode=pinned.inode,
        )

    @staticmethod
    def _dead_adoption_eligible(record: AgentRecord) -> bool:
        """Only an anchored running local adoption has kernel retirement authority."""
        return (record.adapter == "herdr-foreign" and record.lifecycle == "running"
                and record.mode == "interactive" and record.backend == "herdr"
                and record.harness_anchor is not None)

    def _require_dead_adoption(self, record: AgentRecord) -> None:
        """Prove the stored harness generation dead without querying Herdr."""
        if not self._dead_adoption_eligible(record):
            raise AgentDeliveryError(
                "--retire-dead-adoption requires a running interactive herdr-foreign "
                "record with a current harness process anchor"
            )
        anchor = record.harness_anchor
        assert anchor is not None
        checker = getattr(self.client, "process_liveness", None)
        if not callable(checker):
            raise AgentDeliveryError("cannot prove the recorded adopted harness process dead")
        try:
            liveness = checker(anchor)
        except (HerdrRunError, OSError, TypeError, ValueError) as exc:
            raise AgentDeliveryError(
                f"cannot prove the recorded adopted harness process dead: {exc}"
            ) from exc
        if liveness != "dead":
            observed = "alive" if liveness == "alive" else "unknown"
            raise AgentDeliveryError(
                f"refusing dead-adoption retirement: recorded harness process is {observed}"
            )

    def _dead_adoption_pending(self, record: AgentRecord) -> None:
        """Preserve existing transaction authority before registry retirement."""
        try:
            self._refuse_pending_rename(record.name)
            if self._pending_move_destination(record) is not None:
                raise _with_recovery(AgentDeliveryError(
                    f"refusing to stop {record.name!r}: move is incomplete"
                ), RecoveryAction("move", name=record.name, token=record.token))
        except HerdrRunError as exc:
            if exc.recovery_action is None:
                _with_recovery(exc, RecoveryAction("doctor"))
            raise

    def _dead_adoption_stop_recovery(self, record: AgentRecord) -> RecoveryAction | None:
        """Capture supplemental recovery before probing the ordinary foreign runtime."""
        if not self._dead_adoption_eligible(record):
            return None
        try:
            with self._identity_transaction():
                self._dead_adoption_pending(record)
                with self._pinned_agent_directory(record.name) as pinned:
                    with self._dead_adoption_queue_locks(pinned) as verify_queue:
                        snapshot = self._managed_record_snapshot(pinned, expected_token=record.token)
                        if snapshot.record.to_document() != record.to_document():
                            return None
                        self._require_dead_adoption(snapshot.record)
                        verify_queue()
                        if self._record_bytes(pinned) != snapshot.content:
                            return None
                        return RecoveryAction(
                            "retire-dead-adoption", name=record.name, token=record.token,
                            record_sha256=hashlib.sha256(snapshot.content).hexdigest(),
                        )
        except (HerdrRunError, OSError, TypeError, ValueError) as exc:
            if isinstance(exc, HerdrRunError) and exc.recovery_action is not None:
                # A pending transaction has its own captured authority.
                raise
            # Supplemental proof must not change ordinary stop's result or reason.
            return None

    @contextmanager
    def _dead_adoption_queue_locks(
        self, pinned: _PinnedAgentDirectory,
    ) -> Iterator[Callable[[], None]]:
        """Hold generation-checked queue locks without preparing delivery artifacts."""
        self._verify_pinned_agent_directory(pinned)
        try:
            before = os.stat("queue", dir_fd=pinned.descriptor, follow_symlinks=False)
        except FileNotFoundError:
            def verify_absent() -> None:
                self._verify_pinned_agent_directory(pinned)
                try:
                    os.stat("queue", dir_fd=pinned.descriptor, follow_symlinks=False)
                except FileNotFoundError:
                    return
                except OSError as exc:
                    raise AgentDeliveryError(f"cannot verify absent adopted queue: {exc}") from exc
                raise AgentDeliveryError("adopted queue appeared during retirement")

            verify_absent()
            yield verify_absent
            return
        except OSError as exc:
            raise AgentDeliveryError(f"cannot inspect adopted queue: {exc}") from exc
        queue = pinned.path / "queue"
        with self._pinned_parent_directory(queue, label="adopted queue") as held:
            if (before.st_dev, before.st_ino) != (held.device, held.inode):
                raise AgentDeliveryError("adopted queue changed while being pinned")
            descriptors: list[tuple[str, int]] = []

            def verify_queue() -> None:
                self._verify_pinned_agent_directory(pinned)
                self._verify_pinned_parent_directory(held, label="adopted queue")
                try:
                    current = os.stat("queue", dir_fd=pinned.descriptor, follow_symlinks=False)
                    if (current.st_dev, current.st_ino) != (held.device, held.inode):
                        raise AgentDeliveryError("adopted queue directory changed")
                    for lock_name, descriptor in descriptors:
                        agent._verify_queue_lock(
                            str(queue), (held.device, held.inode),
                            str(queue / lock_name), descriptor, "adopted queue lock",
                        )
                        named = os.stat(lock_name, dir_fd=held.descriptor, follow_symlinks=False)
                        opened = os.fstat(descriptor)
                        if (named.st_nlink != 1 or opened.st_nlink != 1
                                or (named.st_dev, named.st_ino) != (opened.st_dev, opened.st_ino)):
                            raise AgentDeliveryError("adopted queue lock generation changed")
                except OSError as exc:
                    raise AgentDeliveryError(f"cannot verify adopted queue locks: {exc}") from exc

            try:
                verify_queue()
                for lock_name in (".delivery.lock", ".binding.lock"):
                    # Valid queues acquire these coordination files lazily. Open
                    # only in the pinned queue; never prepare or bind its messages.
                    descriptor = os.open(
                        lock_name,
                        os.O_RDWR | os.O_CREAT | getattr(os, "O_CLOEXEC", 0)
                        | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0),
                        0o600, dir_fd=held.descriptor,
                    )
                    descriptors.append((lock_name, descriptor))
                    verify_queue()
                    fcntl.flock(descriptor, fcntl.LOCK_EX)
                    verify_queue()
                yield verify_queue
            except OSError as exc:
                raise AgentDeliveryError(f"cannot lock adopted queue: {exc}") from exc
            finally:
                primary = sys.exc_info()[1]
                cleanup: HerdrRunError | None = None
                for _lock_name, descriptor in reversed(descriptors):
                    try:
                        _close_descriptor(descriptor, "adopted queue lock", primary=primary)
                    except HerdrRunError as exc:
                        if cleanup is None:
                            cleanup = exc
                if cleanup is not None:
                    raise cleanup

    def _retire_dead_adoption(
        self, record: AgentRecord, *, expected_token: str,
        expected_record_sha256: str,
    ) -> dict[str, object]:
        """Archive exact adopted registry artifacts using only a stored dead process pin."""
        with self._identity_transaction():
            self._dead_adoption_pending(record)
            with self._pinned_agent_directory(record.name) as pinned:
                with self._dead_adoption_queue_locks(pinned) as verify_queue:
                    initial = self._managed_record_snapshot(pinned, expected_token=expected_token)
                    digest = hashlib.sha256(initial.content).hexdigest()
                    if (initial.record.to_document() != record.to_document()
                            or digest != expected_record_sha256):
                        raise AgentDeliveryError(
                            f"agent {record.name!r} record changed before dead-adoption retirement"
                        )
                    self._require_dead_adoption(initial.record)
                    _archive, destination = self._archive_destination(initial.record)

                    def before_publish() -> None:
                        self._dead_adoption_pending(initial.record)
                        verify_queue()
                        final = self._managed_record_snapshot(pinned, expected_token=expected_token)
                        if final != initial:
                            raise AgentDeliveryError(
                                f"agent {record.name!r} record changed before dead-adoption publication"
                            )
                        self._require_dead_adoption(final.record)
                        if self._record_bytes(pinned) != initial.content:
                            raise AgentDeliveryError(
                                f"agent {record.name!r} record changed during final dead-harness proof"
                            )
                        verify_queue()

                    self._publish_pinned_directory(
                        pinned, destination, expected_record=initial.content,
                        before_publish=before_publish,
                    )
        return {
            "name": record.name, "archive": str(destination), "pane_closed": False,
            "tab_closed": False, "runtime_preserved": True, "retired_dead_adoption": True,
            "record_sha256": digest,
        }

    def _legacy_stop_recovery(self, record: AgentRecord) -> RecoveryAction | None:
        """Offer legacy retirement only for exact bytes and a proved dead pane."""
        if (record.adapter != "herdr-foreign" or record.lifecycle != "running"
                or record.mode != "interactive" or record.backend != "herdr"):
            return None
        try:
            self._dead_pane_proof(record, operation="inspect legacy adoption recovery")
        except HerdrRunError:
            return None
        with self._pinned_agent_directory(record.name) as pinned:
            snapshot = self._managed_record_snapshot(pinned, expected_token=record.token)
            if snapshot.record.to_document() != record.to_document():
                raise AgentDeliveryError(
                    f"agent {record.name!r} record changed before adoption recovery advice"
                )
            document = json.loads(snapshot.content)
            if "foreign_shell_identity" in document:
                return None
            return RecoveryAction(
                "stop", name=record.name, token=snapshot.record.token,
                record_sha256=hashlib.sha256(snapshot.content).hexdigest(),
            )

    @contextmanager
    def _pane_lock(self, pane_id: str) -> Iterator[None]:
        """Serialize cooperative control across registries for one exact pane."""
        with agent._lock_target(pane_id, "host-wide target lock"):
            yield

    def _save(self, record: AgentRecord) -> None:
        if record._storage is None:
            agent._atomic_json(str(self._directory(record.name) / "agent.json"), record.to_document())
            return
        path, device, inode = record._storage
        with self._pinned_parent_directory(path, label="revive candidate") as parent:
            if (parent.device, parent.inode) != (device, inode):
                raise AgentDeliveryError("revive candidate directory changed")
            pinned = _PinnedAgentDirectory(record.name, path, parent.descriptor, device, inode)
            self._atomic_snapshot_bytes(
                pinned, agent._json_text(record.to_document()).encode("utf-8"), name="agent.json",
            )
            self._verify_pinned_parent_directory(parent, label="revive candidate")

    def _queue(self, name: str) -> str:
        return str(self._directory(name) / "queue")

    def _move_intent_path(self, name: str) -> Path:
        return self._directory(name) / "move.json"

    def _write_move_intent(self, record: AgentRecord, destination: str) -> None:
        agent._atomic_json(
            str(self._move_intent_path(record.name)),
            {
                "schema": "agentctl-move/v1",
                "token": record.token,
                "source_pane_id": record.pane_id,
                "source_tab_id": record.tab_id,
                "source_workspace_id": record.workspace_id,
                "destination_workspace_id": destination,
                "harness": record.harness,
                "cwd": record.cwd,
                "session_agent": record.session_agent,
                "session_value": record.session_value,
            },
        )

    def _require_move_intent(self, record: AgentRecord, destination: str) -> None:
        actual = self._read_move_intent(record)
        if actual is None:
            raise AgentDeliveryError(
                f"refusing to recover move of {record.name!r}: no durable move intent"
            )
        expected = {
            "schema": "agentctl-move/v1",
            "token": record.token,
            "source_pane_id": record.pane_id,
            "source_tab_id": record.tab_id,
            "source_workspace_id": record.workspace_id,
            "destination_workspace_id": destination,
            "harness": record.harness,
            "cwd": record.cwd,
            "session_agent": record.session_agent,
            "session_value": record.session_value,
        }
        if actual != expected:
            raise AgentDeliveryError(
                f"refusing to recover move of {record.name!r}: durable move intent "
                f"changed; rerun `agentctl move {record.name}`"
            )

    def _read_move_intent(self, record: AgentRecord) -> dict[str, object] | None:
        """Read and validate the record-independent authority in a move intent."""
        path = self._move_intent_path(record.name)
        if not path.exists():
            return None
        try:
            actual = agent._read_queue_json(
                str(path), "move intent", require_private=True,
            )
        except HerdrRunError as exc:
            raise _with_recovery(AgentDeliveryError(
                f"move of {record.name!r} has an unreadable durable intent; "
                f"rerun `agentctl move {record.name}`: {exc}"
            ), RecoveryAction("doctor"), stop_reason=(
                f"move of {record.name!r} has an unreadable durable intent: {exc}"
            )) from exc
        invariant = {
            "schema": "agentctl-move/v1",
            "token": record.token,
            "harness": record.harness,
            "cwd": record.cwd,
            "session_agent": record.session_agent,
            "session_value": record.session_value,
        }
        if (not isinstance(actual, dict)
                or any(actual.get(key) != value for key, value in invariant.items())
                or not all(isinstance(actual.get(key), str) for key in (
                    "source_pane_id", "source_tab_id", "source_workspace_id",
                    "destination_workspace_id",
                ))):
            raise _with_recovery(AgentDeliveryError(
                f"move of {record.name!r} has an invalid durable intent; "
                f"rerun `agentctl move {record.name}`"
            ), RecoveryAction("doctor"), stop_reason=(
                f"move of {record.name!r} has an invalid durable intent"
            ))
        return actual

    def _pending_move_destination(self, record: AgentRecord) -> str | None:
        """Return a valid pending destination, refusing ambiguous intent state."""
        actual = self._read_move_intent(record)
        if actual is None:
            return None
        destination = cast(str, actual["destination_workspace_id"])
        expected = {
            "schema": "agentctl-move/v1",
            "token": record.token,
            "source_pane_id": record.pane_id,
            "source_tab_id": record.tab_id,
            "source_workspace_id": record.workspace_id,
            "destination_workspace_id": destination,
            "harness": record.harness,
            "cwd": record.cwd,
            "session_agent": record.session_agent,
            "session_value": record.session_value,
        }
        completed = (record.workspace_id == destination
                     and actual["source_workspace_id"] != destination)
        if actual != expected and not completed:
            raise _with_recovery(AgentDeliveryError(
                f"move of {record.name!r} has a durable intent that does not match "
                f"its record; rerun `agentctl move {record.name}`"
            ), RecoveryAction("doctor"), stop_reason=(
                f"move of {record.name!r} has a durable intent that does not match its record"
            ))
        return destination

    def _source_unchanged_after_failed_move(self, record: AgentRecord) -> bool:
        """Prove a failed Herdr move made no externally visible placement change."""
        if record.pane_id is None or self.client.agent_pane(record.name) != record.pane_id:
            return False
        presentations = [
            pane for pane in self.client.panes()
            if pane.pane_id == record.pane_id
        ]
        if (len(presentations) != 1
                or presentations[0].tab_id != record.tab_id
                or presentations[0].workspace_id != record.workspace_id):
            return False
        info = self.client.pane_info(record.pane_id)
        return (info.pane_id == record.pane_id
                and info.workspace_id == record.workspace_id
                and os.path.realpath(info.cwd) == os.path.realpath(record.cwd))

    def _clear_move_intent(self, name: str) -> None:
        path = self._move_intent_path(name)
        try:
            path.unlink()
        except FileNotFoundError:
            return
        agent._fsync_dir(str(path.parent))

    def get(self, name: str) -> AgentRecord:
        """Read durable metadata without requiring Herdr to be reachable."""
        return self._load(name)

    def _anchor_fresh(self, record: AgentRecord, info: AgentPaneInfo) -> None:
        """Pin the terminal and harness process of a pane this command just verified.

        Used only where agentctl itself launched or explicitly adopted the program, so
        the pinned identity is the intended recipient rather than whatever is visible now.
        """
        record.terminal_id = info.terminal_id
        if record.adapter in ("herdr", "herdr-foreign") and record.pane_id is not None:
            record.harness_identity = self.client.harness_identity(record.pane_id, record.harness)
            record.anchor_rule = ANCHOR_RULE if record.harness_identity is not None else None
        if record.pane_id is not None:
            owner = self._claim_owner(record.pane_id, record.terminal_id, exclude=record.name)
            if owner is not None:
                raise AgentDeliveryError(
                    f"pane {record.pane_id} is already registered as {owner!r}"
                )

    def _peer_claims(
        self, name: str,
    ) -> Callable[[], builtins.list[tuple[str, str, str | None]]]:
        """Pane and terminal claims of every other active record, read conservatively.

        Each record is read as plain JSON, so a record either edition cannot fully
        decode still counts; one that is not readable JSON refuses input outright.
        """
        def claims() -> builtins.list[tuple[str, str, str | None]]:
            found: builtins.list[tuple[str, str, str | None]] = []
            for path in sorted(self.registry.iterdir()):
                if not _NAME.fullmatch(path.name) or path.name in ("archive", name):
                    continue
                try:
                    raw = json.loads((path / "agent.json").read_text(encoding="utf-8"))
                except FileNotFoundError:
                    continue
                except (OSError, ValueError) as exc:
                    raise AgentDeliveryError(
                        f"cannot prove that pane ownership is unique: record {path.name!r} "
                        f"is unreadable ({exc}); run `agentctl doctor`"
                    ) from exc
                if not isinstance(raw, dict):
                    raise AgentDeliveryError(
                        f"cannot prove that pane ownership is unique: record {path.name!r} "
                        "is not a JSON object; run `agentctl doctor`"
                    )
                if raw.get("lifecycle") in ("stopped", "launch_failed"):
                    continue
                pane, terminal = raw.get("pane_id"), raw.get("terminal_id")
                if isinstance(pane, str):
                    found.append((path.name, pane, terminal if isinstance(terminal, str) else None))
            from agentctl.revive import candidate_records
            for candidate in candidate_records(self):
                if candidate.name != name and candidate.pane_id is not None:
                    found.append((candidate.name, candidate.pane_id, candidate.terminal_id))
            return found
        return claims

    def _peer_panes(self, name: str) -> Callable[[], builtins.list[str]]:
        """Panes of every other readable record, for locating a misrouted prompt."""
        def panes() -> builtins.list[str]:
            found: builtins.list[str] = []
            for path in sorted(self.registry.iterdir()):
                if not _NAME.fullmatch(path.name) or path.name in ("archive", name):
                    continue
                try:
                    other = AgentRecord.load(path / "agent.json", path.name)
                except AgentDeliveryError:
                    continue
                if other.pane_id is not None and other.lifecycle == "running":
                    found.append(other.pane_id)
            return found
        return panes

    def _repin_moved_terminal(self, record: AgentRecord) -> None:
        """After a verified move, keep the terminal anchor only while the harness still matches.

        A cross-workspace move may give the pane a new terminal id. The move itself
        proved the managed name followed the pane; the pinned harness process must
        also still be its foreground program before the new terminal is recorded.
        """
        if record.pane_id is None or record.terminal_id is None:
            return
        terminal = self.client.pane_info(record.pane_id).terminal_id
        if terminal == record.terminal_id:
            return
        if (record.harness_anchor is None
                or not self.client.verify_harness_identity(record.pane_id, record.harness_anchor)):
            record.terminal_id = None
            return
        record.terminal_id = terminal

    def _claim_owner(
        self, pane_id: str, terminal_id: str | None, *, exclude: str | None = None,
    ) -> str | None:
        """Name another active record that claims this pane or terminal.

        Read conservatively: an unreadable record refuses rather than being skipped.
        """
        if not self.registry.exists():
            return None
        for name, pane, terminal in self._peer_claims(exclude or "")():
            if pane == pane_id or (terminal_id is not None and terminal == terminal_id):
                return name
        return None

    def _identity_owner(
        self, session_agent: str, session_value: str, *, exclude: str | None = None,
    ) -> AgentRecord | None:
        """Return an active record holding this exact provider-local session."""
        if not self.registry.exists():
            return None
        agent._validate_private_directory(str(self.registry), "agent registry")
        for path in self.registry.iterdir():
            if (not _NAME.fullmatch(path.name) or path.name == "archive"
                    or path.name == exclude):
                continue
            agent._validate_private_directory(str(path), "agent directory")
            other = AgentRecord.load(path / "agent.json", path.name)
            if ((other.session_agent or other.harness) == session_agent
                    and session_value in (other.resume, other.session_value, other.goal_session_id,
                                          None if other.native_session is None else other.native_session["value"])):
                return other
        from agentctl.revive import candidate_records
        for other in candidate_records(self):
            if (other.name != exclude and other.harness == session_agent
                    and session_value in (other.resume, other.session_value,
                        None if other.native_session is None else other.native_session["value"])):
                return other
        return None

    def revive(
        self, name: str, *, dry_run: bool = False,
        expected_token: str | None = None, startup_timeout: float = 30.0,
    ) -> dict[str, object]:
        """Resume one positively dead owned harness in a fresh tab."""
        from agentctl.revive import revive
        return revive(self, name, dry_run=dry_run, expected_token=expected_token,
                      startup_timeout=startup_timeout)

    def revive_all(
        self, *, dry_run: bool = False, startup_timeout: float = 30.0,
    ) -> dict[str, object]:
        """Plan or resume each recoverable generation, preserving failed plans."""
        from agentctl.revive import revive_all
        return revive_all(self, dry_run=dry_run, startup_timeout=startup_timeout)

    def start(
        self, name: str, *, cwd: str, workspace_id: str | None = None,
        harness: str = "codex", model: str | None = None, resume: str | None = None,
        reasoning_effort: str | None = None,
        harness_args: Sequence[str] = (), environment: Sequence[str] = (),
        brief: str | None = None,
        startup_timeout: float = 30.0, ready_timeout: float = 900.0,
        working_timeout: float = 30.0, max_attempts: int = 3,
        slot: str | None = None, slot_isolation: str | None = None,
        slot_project: str | None = None,
        profile: str | None = None,
    ) -> dict[str, object]:
        """Create one new tab and start its interactive harness without stealing focus.

        With ``slot``, the new pane's shell is first replaced by a shell boxed
        to that slot (per-slot resource limits and, unless the isolation is
        ``cgroup``, a confined file-system view), so the harness and everything
        it runs stay inside the slot's box. ``slot_isolation`` None uses the
        project's configured isolation.

        Failed launches retain their record and terminal for diagnosis. Stop the
        named agent after inspecting it to archive its artifacts and release its name.
        """
        _name(name)
        root = str(Path(cwd).expanduser().resolve())
        if not Path(root).is_dir():
            raise AgentDeliveryError(f"cwd is not a directory: {root}")
        if not math.isfinite(startup_timeout) or not 0 < startup_timeout <= 300:
            raise AgentDeliveryError("startup timeout must be between 0 and 300 seconds")
        if profile is not None and not _NAME.fullmatch(profile):
            raise AgentDeliveryError("invalid launch profile name")
        if resume is not None and (not resume or "\0" in resume):
            raise AgentDeliveryError("native session id must be nonempty and contain no NUL")
        if isinstance(harness_args, (str, bytes)) or any(
            not isinstance(item, str) or not item or "\0" in item
            for item in harness_args
        ):
            raise AgentDeliveryError(
                "harness arguments must be a sequence of nonempty NUL-free strings"
            )
        if harness == "claude" and any(
            item.split("=", 1)[0] in ("--session-id", "--resume", "--continue", "--fork-session", "-r", "-c")
            or item.startswith(("-r", "-c")) and not item.startswith("--")
            for item in harness_args
        ):
            raise AgentDeliveryError(
                "Claude conversation selectors must use --resume; agentctl assigns --session-id for new conversations"
            )
        if harness in ("codex", "claude", "muse"):
            validate_structured_harness_argument_conflicts(
                harness, list(harness_args), label=f"{harness} launch",
                structured_model=model is not None,
                structured_effort=reasoning_effort is not None,
                structured_resume=resume is not None,
            )
        structured_effort = reasoning_arguments(harness, reasoning_effort)
        arguments = harness_arguments(
            harness, model=model, resume=resume,
            extra=(*structured_effort, *harness_args),
        )
        conversation = resume
        if harness == "claude" and resume is None:
            conversation = str(uuid.uuid4())
            arguments = ("--session-id", conversation, *arguments)
        environment = environment_entries(environment)
        if brief is not None and not brief:
            raise AgentDeliveryError("brief must not be empty")
        project_workspace = self._project_workspace()
        if project_workspace is not None and workspace_id is not None:
            actual_label = self.client.workspace_label(workspace_id)
            if actual_label != project_workspace:
                raise AgentDeliveryError(
                    f"workspace {workspace_id!r} is labelled {actual_label!r}, "
                    f"but project configuration requires {project_workspace!r}"
                )
        slot_command: str | None = None
        relay_command: str | None = None
        launch_project: str | None = None
        effective_isolation: str | None = None
        if slot is not None:
            # The agent works in the slot: its record and pane cwd are the slot
            # directory, which is where the boxed shell starts.
            project = slot_project or root
            launch_project = str(Path(project).expanduser().resolve())
            slot_command, root, effective_isolation = _slot_shell_command(
                slot, isolation=slot_isolation, project=project,
                explicit_project=slot_project is not None,
            )
            if effective_isolation == "root":
                # sudo stays the pane's foreground process under root isolation,
                # so Herdr cannot start the harness; the boxed harness line runs
                # in the pane directly and agentctl owns its lifecycle state.
                if harness not in RELAY_HARNESSES:
                    raise AgentDeliveryError(
                        f"--slot with root isolation supports {', '.join(RELAY_HARNESSES)}, not {harness!r}"
                    )
                try:
                    executable = self.client._harness_executable(harness)
                except HerdrRunError as exc:
                    raise AgentDeliveryError(str(exc)) from exc
                relay_command, _path, _isolation = _slot_shell_command(
                    slot, isolation=slot_isolation, project=project,
                    explicit_project=slot_project is not None,
                    command=(executable, *arguments),
                )
                slot_command = None
        with self._lock(name):
            with self._identity_transaction():
                if resume is not None:
                    owner = self._identity_owner(harness, resume, exclude=name)
                    if owner is not None:
                        raise AgentDeliveryError(
                            f"native session is already registered as {owner.name!r}"
                        )
                directory = self._directory(name)
                self._refuse_pending_rename(name)
                if os.path.lexists(directory):
                    raise AgentDeliveryError(f"agent {name!r} already registered; stop it before reusing the name")
                directory.mkdir(mode=0o700)
                agent._fsync_dir(str(self.registry))
                record = AgentRecord(name, uuid.uuid4().hex, harness, root, time.time(),
                                     model=model, resume=resume, arguments=list(arguments),
                                     native_session=None if conversation is None else _native_session(harness, conversation, "asserted"),
                                     profile=profile, reasoning_effort=reasoning_effort,
                                     slot=slot, slot_project=launch_project,
                                     slot_isolation=effective_isolation,
                                     environment_names=sorted({entry.partition("=")[0] for entry in environment}),
                                     adapter="herdr-relay" if relay_command is not None
                                     else "herdr-pane" if harness == "muse" else "herdr")
                self._save(record)
                try:
                    self._launch_interactive(
                        record, workspace_id, environment, project_workspace,
                        startup_timeout, slot_command, relay_command,
                    )
                except (HerdrRunError, ValueError, OSError) as exc:
                    record.lifecycle = "launch_failed"
                    record.error = (
                        "launch failed with caller-supplied environment; "
                        "details omitted from status"
                        if environment else str(exc)
                    )
                    self._save(record)
                    raise AgentDeliveryError(
                        f"launch of {name!r} failed: {exc}; record and any created "
                        f"tab retained at {directory}"
                    ) from exc
            if brief is not None:
                client = cast(HerdrClient, _WorkspaceClient(
                    self.client, record, queue=self._queue(name),
                    expected_workspace=project_workspace,
                    peer_panes=self._peer_panes(name), claims=self._peer_claims(name),
                    # A just-started harness may take longer than READBACK_SECONDS to print
                    # its first prompt; it gets as long as it has to start working.
                    readback_seconds=working_timeout,
                ))
                agent.send(client, self._target(record), self._queue(name), brief,
                           ready_timeout=ready_timeout, working_timeout=working_timeout,
                           max_attempts=max_attempts,
                           allow_legacy_workspace_binding=True)
            # Keep the generation lock until its initial instruction and result
            # are captured; a replacement must never receive this launch's brief.
            return self.status(name)

    def _launch_interactive(
        self, record: AgentRecord, workspace_id: str | None,
        environment: Sequence[str], project_workspace: str | None,
        startup_timeout: float, slot_command: str | None, relay_command: str | None,
    ) -> None:
        """Launch into the record's storage destination and capture independent anchors."""
        name, harness, arguments = record.name, record.harness, tuple(record.arguments)
        self._create_presentation(
            record, workspace_id, environment, project_workspace
        )
        assert record.pane_id is not None
        if slot_command is not None:
            self.client.enter_slot_sandbox(
                record.pane_id, slot_command, timeout=startup_timeout
            )
        if relay_command is not None:
            def persist_relay(identity: CustomProcessIdentity) -> None:
                record.custom_process_identity = identity
                self._save(record)

            self.client.start_relay_agent(
                harness, record.pane_id, relay_command,
                timeout=startup_timeout, on_observed=persist_relay,
            )
            record.pane_reported_by_agentctl = True
            self._save(record)
        elif record.adapter == "herdr-pane":
            def persist_identity(identity: CustomProcessIdentity) -> None:
                record.custom_process_identity = identity
                self._save(record)

            self.client.start_pane_agent(
                name, harness, record.pane_id, arguments,
                timeout=startup_timeout, on_observed=persist_identity,
            )
            record.pane_reported_by_agentctl = True
            self._save(record)
            screen = self.client.read(
                record.pane_id, source="visible", lines=200
            )
            (record.startup_warning,
             record.effective_reasoning_effort) = muse_startup_metadata(screen)
        else:
            self.client.start_agent(name, harness, record.pane_id, arguments, timeout=startup_timeout)
        info = self._checked(record, ready=True, enforce_policy=True)
        if info.workspace_id != record.workspace_id:
            raise AgentDeliveryError("started agent moved to another workspace")
        _capture_native_session(record, info)
        record.session_agent = info.session_agent
        record.session_value = info.session_value
        if info.session_agent is not None and info.session_value is not None:
            owner = self._identity_owner(
                info.session_agent, info.session_value, exclude=name
            )
            if owner is not None:
                try:
                    self.client.close_pane(record.pane_id)
                except HerdrRunError as close_error:
                    record.session_agent = record.session_value = None
                    raise AgentDeliveryError(
                        f"native session is already registered as {owner.name!r}; "
                        f"could not close the conflicting new pane: {close_error}"
                    ) from close_error
                record.session_agent = record.session_value = None
                raise AgentDeliveryError(
                    f"native session is already registered as {owner.name!r}; "
                    "closed the conflicting new pane"
                )
            try:
                agent.resolve_target(self.client, self._target(record))
            except HerdrRunError as identity_error:
                record.session_agent = record.session_value = None
                raise AgentDeliveryError(
                    "started native session is not globally unique; "
                    "the failed owned pane remains available for stop"
                ) from identity_error
        self._anchor_fresh(record, info)
        record.lifecycle = "running"
        self._save(record)
        try:
            agent.resolve_target(self.client, self._target(record))
            final_info = self._checked(record, ready=True)
        except HerdrRunError:
            record.session_agent = record.session_value = None
            raise
        if (final_info.session_agent != record.session_agent
                or final_info.session_value != record.session_value):
            record.session_agent = record.session_value = None
            raise AgentDeliveryError(
                "started agent native session changed during identity commit"
            )
        _capture_native_session(record, final_info)

    def adopt(
        self, name: str, *, pane_id: str, expected_workspace: str,
        expected_cwd: str, harness: str, session: str | None = None,
    ) -> dict[str, object]:
        """Register an existing interactive Herdr agent without owning its runtime.

        Adoption is intentionally narrower than startup: every live identity
        assertion is explicit, no pane/name/process is changed, and the saved
        adapter marks the runtime as foreign so retirement cannot close it.
        """
        _name(name)
        if not pane_id or "\0" in pane_id:
            raise AgentDeliveryError("adopt needs a nonempty pane id without NUL")
        if not expected_workspace or "\0" in expected_workspace:
            raise AgentDeliveryError("adopt needs a nonempty expected workspace label without NUL")
        project_workspace = self._project_workspace()
        if (project_workspace is not None
                and expected_workspace != project_workspace):
            raise AgentDeliveryError(
                f"adopt workspace {expected_workspace!r} does not match project "
                f"workspace {project_workspace!r}"
            )
        if not _KIND.fullmatch(harness):
            raise AgentDeliveryError("harness must be a Herdr agent kind")
        root = str(Path(expected_cwd).expanduser().resolve())
        if not Path(root).is_dir():
            raise AgentDeliveryError(f"cwd is not a directory: {root}")
        if session is not None and (not session or "\0" in session):
            raise AgentDeliveryError("native session id must be nonempty and contain no NUL")
        target = agent.Target(
            pane_id=pane_id,
            session_agent=harness if session is not None else None,
            session_value=session,
            expected_agent=harness,
            expected_workspace=expected_workspace,
            expected_cwd=root,
        )
        with self._lock(name):
            directory = self._directory(name)
            if os.path.lexists(directory):
                raise AgentDeliveryError(
                    f"agent {name!r} already registered; stop it before reusing the name"
                )
            # Names have independent lifecycle locks.  Serialize adoption as a
            # registry-wide identity transaction as well, so two callers cannot
            # register different panes that report the same native session.
            with self._identity_transaction():
                self._refuse_pending_rename(name)
                target_lock, _locked_pane, info = agent._lock_resolved_target(self.client, target)
                try:
                    return self._adopt_locked(
                        name, directory, root, harness, pane_id, info
                    )
                finally:
                    target_lock.close()

    def _adopt_locked(
        self, name: str, directory: Path, root: str, harness: str,
        pane_id: str, info: AgentPaneInfo,
    ) -> dict[str, object]:
        """Commit one adoption while its registry and live pane are locked."""
        if info.agent is None:
            raise AgentDeliveryError(f"refusing pane {pane_id}: no live agent is detected")
        if (info.session_agent is None) != (info.session_value is None):
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: native session identity is incomplete"
            )
        if (info.session_agent is not None
                and (not info.session_agent or not info.session_value
                     or "\0" in info.session_agent or "\0" in info.session_value)):
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: native session identity is invalid"
            )
        if info.session_agent is not None and info.session_agent != harness:
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: native session agent is "
                f"{info.session_agent!r}, expected {harness!r}"
            )
        if harness == "muse" and (not info.terminal_id or "\0" in info.terminal_id):
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: adopting Muse requires a terminal identity"
            )
        muse_identity = self.client.harness_identity(info.pane_id, harness) if harness == "muse" else None
        if harness == "muse" and muse_identity is None:
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: cannot pin the existing Muse foreground process"
            )
        if info.session_value is not None:
            # A reported native session becomes the durable queue authority.
            # Prove now that it resolves uniquely back to this exact pane.
            agent.resolve_target(self.client, agent.Target(
                pane_id=info.pane_id, session_agent=info.session_agent,
                session_value=info.session_value, expected_agent=harness,
                expected_cwd=root,
            ))
        presentations = [pane for pane in self.client.panes()
                         if pane.pane_id == info.pane_id]
        if len(presentations) != 1:
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: expected one live presentation, "
                f"found {len(presentations)}"
            )
        presentation = presentations[0]
        if presentation.workspace_id != info.workspace_id:
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: presentation workspace identity changed"
            )
        shell_identity = self.client.pane_shell_identity(info.pane_id)
        owner = self._claim_owner(info.pane_id, info.terminal_id)
        if owner is not None:
            raise AgentDeliveryError(f"pane {pane_id!r} is already registered as {owner!r}")
        if info.session_agent is not None and info.session_value is not None:
            session_owner = self._identity_owner(info.session_agent, info.session_value)
            if session_owner is not None:
                raise AgentDeliveryError(f"pane {pane_id!r} is already registered as {session_owner.name!r}")
        if self.registry.exists():
            agent._validate_private_directory(str(self.registry), "agent registry")
            for path in self.registry.iterdir():
                if not _NAME.fullmatch(path.name) or path.name == "archive":
                    continue
                agent._validate_private_directory(str(path), "agent directory")
                other = AgentRecord.load(path / "agent.json", path.name)
                same_session = (info.session_value is not None
                                and (other.session_agent or other.harness)
                                    == info.session_agent
                                and info.session_value in (
                                    other.resume, other.session_value, other.goal_session_id,
                                    None if other.native_session is None else other.native_session["value"],
                                ))
                same_terminal = (info.terminal_id is not None
                                 and other.terminal_id == info.terminal_id)
                if other.pane_id == info.pane_id or same_session or same_terminal:
                    raise AgentDeliveryError(
                        f"pane {pane_id!r} is already registered as {other.name!r}"
                    )
        confirmed = agent.resolve_target(self.client, agent.Target(
            pane_id=info.pane_id, session_agent=info.session_agent,
            session_value=info.session_value, expected_agent=harness,
            expected_cwd=root,
        ))
        if (confirmed.workspace_id != info.workspace_id
                or confirmed.session_agent != info.session_agent
                or confirmed.session_value != info.session_value):
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: live identity changed before adoption"
            )
        final_presentations = [pane for pane in self.client.panes()
                               if pane.pane_id == confirmed.pane_id]
        if (len(final_presentations) != 1
                or final_presentations[0].tab_id != presentation.tab_id
                or final_presentations[0].workspace_id != presentation.workspace_id):
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: live presentation changed before adoption"
            )
        if self.client.pane_shell_identity(confirmed.pane_id) != shell_identity:
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: pane shell process changed before adoption"
            )
        # Adoption is the operator's explicit assertion about this program, so its
        # harness process and terminal become the record's anchors.
        harness_identity = (muse_identity if harness == "muse"
                            else self.client.harness_identity(confirmed.pane_id, harness))
        if (muse_identity is not None
                and not self.client.verify_harness_identity(confirmed.pane_id, muse_identity)):
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: Muse foreground process changed before adoption"
            )
        if confirmed.terminal_id != info.terminal_id:
            raise AgentDeliveryError(
                f"refusing pane {pane_id}: terminal changed before adoption"
            )
        directory.mkdir(mode=0o700)
        agent._fsync_dir(str(self.registry))
        record = AgentRecord(
            name, uuid.uuid4().hex, harness, root, time.time(),
            lifecycle="running", workspace_id=info.workspace_id,
            tab_id=presentation.tab_id, pane_id=info.pane_id,
            session_agent=info.session_agent,
            session_value=info.session_value,
            native_session=None if info.session_value is None else _native_session(harness, info.session_value, "observed"),
            adapter="herdr-foreign", mode="interactive", backend="herdr",
            foreign_shell_identity=shell_identity,
            terminal_id=info.terminal_id, harness_identity=harness_identity,
            anchor_rule=ANCHOR_RULE if harness_identity is not None else None,
        )
        self._save(record)
        try:
            result = self._status_record(record)
            if result.get("probe_error") is not None:
                raise AgentDeliveryError(
                    f"final live-identity verification failed: {result['probe_error']}"
                )
            final_info = self._checked(record)
            if (final_info.session_agent != record.session_agent
                    or final_info.session_value != record.session_value):
                raise AgentDeliveryError(
                    "final live native-session identity changed"
                )
            live = [pane for pane in self.client.panes()
                    if pane.pane_id == record.pane_id]
            if (len(live) != 1 or live[0].tab_id != record.tab_id
                    or live[0].workspace_id != record.workspace_id):
                raise AgentDeliveryError("final live-presentation verification failed")
            if self.client.pane_shell_identity(confirmed.pane_id) != shell_identity:
                raise AgentDeliveryError("final pane shell process identity changed")
            if (muse_identity is not None
                    and not self.client.verify_harness_identity(confirmed.pane_id, muse_identity)):
                raise AgentDeliveryError("final Muse foreground process identity changed")
            return result
        except (HerdrRunError, OSError) as exc:
            try:
                failed = self._archive_failed_adoption(record, str(exc))
            except (HerdrRunError, OSError) as cleanup:
                raise AgentDeliveryError(
                    f"adoption failed final verification ({exc}); could not finish "
                    f"failed-record archival from {directory}: {cleanup}"
                ) from exc
            raise AgentDeliveryError(
                f"adoption failed final verification and was not registered; "
                f"diagnostic record archived at {failed}: {exc}"
            ) from exc

    def _archive_failed_adoption(self, record: AgentRecord, error: str) -> Path:
        """Atomically remove a failed generation from the active namespace."""
        archive = self.registry / "archive"
        archive.mkdir(mode=0o700, exist_ok=True)
        agent._validate_private_directory(str(archive), "agent archive")
        destination = archive / f"{record.name}-{record.token}-adopt-failed"
        os.rename(self._directory(record.name), destination)
        agent._fsync_dir(str(archive))
        agent._fsync_dir(str(self.registry))
        record.lifecycle = "adopt_failed"
        record.error = error
        agent._atomic_json(str(destination / "agent.json"), record.to_document())
        return destination

    def _create_presentation(
        self, record: AgentRecord, workspace_id: str | None,
        environment: Sequence[str], project_workspace: str | None,
    ) -> None:
        # Independent registries can share the default workspace. Serialize label
        # resolution and creation host-wide, releasing before any harness startup.
        default_label = project_workspace or "subagents"
        lock = agent._lock_target(
            f"managed-workspace:{default_label}", "workspace allocation lock"
        )
        try:
            selected = workspace_id
            if selected is None and project_workspace is None:
                selected = os.environ.get("HERDR_WORKSPACE_ID")
            if selected:
                actual_label = self.client.workspace_label(selected)
                if (project_workspace is not None
                        and actual_label != project_workspace):
                    raise AgentDeliveryError(
                        f"workspace {selected!r} is labelled {actual_label!r}, "
                        f"but project configuration requires {default_label!r}"
                    )
            else:
                selected = self.client.workspace_id_for_label(default_label)
            if selected is None:
                selected, tab, pane = self.client.create_workspace(
                    label=default_label, cwd=record.cwd, environment=environment
                )
                record.workspace_id, record.tab_id, record.pane_id = selected, tab, pane
                self._save(record)
                self.client.rename_tab(tab, record.name)
            else:
                record.workspace_id = selected
                record.tab_id, record.pane_id = self.client.create_tab_with_pane(
                    workspace_id=selected, label=record.name, cwd=record.cwd,
                    environment=environment,
                )
                self._save(record)
        finally:
            lock.close()

    def _checked(
        self, record: AgentRecord, *, ready: bool = False,
        enforce_policy: bool = False,
    ) -> AgentPaneInfo:
        if record.adapter not in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            raise AgentDeliveryError("this operation requires the interactive Herdr adapter")
        client = cast(HerdrClient, _WorkspaceClient(self.client, record, check_prompt=ready))
        target = self._policy_target(record) if enforce_policy else self._target(record)
        info = agent.resolve_target(client, target)
        if info.workspace_id != record.workspace_id:
            raise AgentDeliveryError(f"agent {record.name!r} workspace identity changed")
        return info

    def _require_policy(self, record: AgentRecord) -> None:
        agent.resolve_target(self.client, self._policy_target(record))

    def _checked_or_failed_pane_report(self, record: AgentRecord, pane_id: str) -> None:
        """Prove a failed custom launch is still ours or has returned to its shell."""
        if (record.lifecycle in ("starting", "launch_failed")
                and record.adapter == "herdr-relay"):
            # The pane is the one agentctl created; its shell exec'd into the
            # slot relay agentctl ran. A relay still running there with the
            # slot as its cwd is that launch, pinned or not.
            info = self.client.pane_info(pane_id)
            if (info.pane_id == pane_id and info.workspace_id == record.workspace_id
                    and os.path.realpath(info.cwd) == os.path.realpath(record.cwd)):
                observed = self.client.relay_process(pane_id)
                if observed is not None and record.custom_process_identity in (None, observed):
                    return
        if (record.lifecycle in ("starting", "launch_failed")
                and record.adapter == "herdr-pane"
                and record.custom_process_identity is not None):
            info = self.client.pane_info(pane_id)
            if (info.pane_id == pane_id and info.workspace_id == record.workspace_id
                    and os.path.realpath(info.cwd) == os.path.realpath(record.cwd)):
                try:
                    self.client.verify_custom_harness(
                        pane_id, record.harness, record.custom_process_identity
                    )
                    return
                except HerdrUnavailable:
                    pass
        # Preserve the original failed-launch fallback exactly. A stale report plus
        # return to the original shell is sufficient only after agentctl itself
        # reported that pane; an unreported pre-readiness pane remains unowned.
        if (record.lifecycle == "launch_failed" and record.adapter == "herdr-pane"
                and record.pane_reported_by_agentctl):
            info = self.client.pane_info(pane_id)
            if (info.pane_id == pane_id and info.workspace_id == record.workspace_id
                    and os.path.realpath(info.cwd) == os.path.realpath(record.cwd)
                    and info.agent == record.harness
                    and self.client.pane_is_idle_shell(pane_id)):
                return
        if (record.lifecycle == "starting" and record.adapter == "herdr-pane"
                and record.custom_process_identity is None):
            raise HerdrUnavailable(
                f"cannot prove starting custom harness ownership in pane {pane_id}"
            )
        self._checked(record)

    def status(self, name: str) -> dict[str, object]:
        """Report live state or a visible probe error, preserving every durable record."""
        return self._status_record(self._load(name))

    def move_to_project_workspace(self, name: str) -> dict[str, object]:
        """Move one owned interactive pane to the configured project workspace.

        A cross-workspace Herdr move changes the public pane id. If that move
        completed before the registry write, a repeat invocation recovers the
        new identity through the globally unique managed agent name.
        """
        with self._lock(name):
            record = self._load(name)
            if (record.adapter != "herdr" or record.lifecycle != "running"
                    or record.mode != "interactive" or record.backend != "herdr"):
                raise AgentDeliveryError(
                    "move supports only a running agentctl-owned native Herdr agent"
                )
            expected_workspace = self._project_workspace()
            if expected_workspace is None:
                pending_destination = self._pending_move_destination(record)
                if pending_destination is None:
                    raise AgentDeliveryError(
                        "move requires a workspace field in .agentctl/profiles.json "
                        "or a durable pending move"
                    )
                destination = pending_destination
                expected_workspace = self.client.workspace_label(destination)
            else:
                resolved_destination = self.client.workspace_id_for_label(expected_workspace)
                if resolved_destination is None:
                    raise AgentDeliveryError(
                        f"configured project workspace {expected_workspace!r} does not exist"
                    )
                destination = resolved_destination
                if self.client.workspace_label(destination) != expected_workspace:
                    raise AgentDeliveryError(
                        "configured project workspace changed during resolution"
                    )
            if record.pane_id is None:
                raise AgentDeliveryError(f"agent {name!r} has no confirmed pane")
            recorded_pane = record.pane_id
            previous_target = record.target()
            named_pane = self.client.agent_pane(name)
            if named_pane != recorded_pane:
                self._require_move_intent(record, destination)
                if any(pane.pane_id == recorded_pane for pane in self.client.panes()):
                    raise AgentDeliveryError(
                        f"refusing to recover move of {name!r}: both recorded and named panes are live"
                    )
                target = agent.Target(
                    pane_id=named_pane,
                    session_agent=record.session_agent,
                    session_value=record.session_value,
                    expected_agent=record.harness,
                    expected_workspace=expected_workspace,
                    expected_cwd=record.cwd,
                )
                binding_target = replace(record.target(), pane_id=named_pane)
                def recover() -> tuple[agent.Target, object]:
                    with self._pane_lock(named_pane):
                        if self.client.agent_pane(name) != named_pane:
                            raise AgentDeliveryError(
                                f"refusing to recover move of {name!r}: managed name changed panes"
                            )
                        if any(pane.pane_id == recorded_pane for pane in self.client.panes()):
                            raise AgentDeliveryError(
                                f"refusing to recover move of {name!r}: both recorded and named panes are live"
                            )
                        info = agent.resolve_target(self.client, target)
                        presentations = [
                            pane for pane in self.client.panes(destination)
                            if pane.pane_id == info.pane_id
                        ]
                        if len(presentations) != 1:
                            raise AgentDeliveryError(
                                f"refusing to recover move of {name!r}: expected one "
                                f"destination presentation, found {len(presentations)}"
                            )
                        presentation = presentations[0]
                        if sum(
                            pane.tab_id == presentation.tab_id
                            for pane in self.client.panes(destination)
                        ) != 1:
                            raise AgentDeliveryError(
                                f"refusing to recover move of {name!r}: its tab contains another pane"
                            )
                        return binding_target, (info, presentation)

                recovered = agent.rebind_queue_after(
                    self._queue(name), previous_target, recover,
                    already_replacement=binding_target,
                    allow_legacy_workspace_binding=True,
                )
                assert isinstance(recovered, tuple)
                info, presentation = recovered
                record.workspace_id = destination
                record.tab_id = presentation.tab_id
                record.pane_id = info.pane_id
                self._repin_moved_terminal(record)
                self._save(record)
                self._clear_move_intent(name)
                result = self._status_record(record)
                result.update({
                    "moved": True,
                    "recovered": True,
                    "previous_pane_id": recorded_pane,
                })
                return result

            def perform() -> tuple[agent.Target, object]:
                with self._pane_lock(recorded_pane):
                    client = cast(HerdrClient, _WorkspaceClient(self.client, record))
                    source_info = agent.resolve_target(client, previous_target)
                    if self.client.agent_pane(name) != source_info.pane_id:
                        raise AgentDeliveryError(
                            f"refusing to move {name!r}: managed agent name changed panes"
                        )
                    presentations = [
                        pane for pane in self.client.panes(source_info.workspace_id)
                        if pane.pane_id == source_info.pane_id
                    ]
                    if len(presentations) != 1:
                        raise AgentDeliveryError(
                            f"refusing to move {name!r}: expected one source "
                            f"presentation, found {len(presentations)}"
                        )
                    source = presentations[0]
                    if (source.tab_id != record.tab_id
                            or source.workspace_id != record.workspace_id):
                        raise AgentDeliveryError(
                            f"refusing to move {name!r}: recorded tab or workspace "
                            "identity changed"
                        )
                    if sum(
                        pane.tab_id == source.tab_id
                        for pane in self.client.panes(source.workspace_id)
                    ) != 1:
                        raise AgentDeliveryError(
                            f"refusing to move {name!r}: its tab contains another pane"
                        )
                    if source.workspace_id == destination:
                        target = agent.Target(
                            pane_id=source.pane_id,
                            session_agent=record.session_agent,
                            session_value=record.session_value,
                            expected_agent=record.harness,
                            expected_workspace=expected_workspace,
                            expected_cwd=record.cwd,
                        )
                        return replace(target, expected_workspace=None), (False, None)
                    self._write_move_intent(record, destination)
                    try:
                        moved = self.client.move_pane_to_new_tab(
                            source.pane_id,
                            workspace_id=destination,
                            tab_label=name,
                        )
                    except HerdrRunError:
                        try:
                            unchanged = self._source_unchanged_after_failed_move(record)
                        except HerdrRunError:
                            unchanged = False
                        if unchanged:
                            self._clear_move_intent(name)
                        raise
                    if self.client.agent_pane(name) != moved.pane_id:
                        raise AgentDeliveryError(
                            f"move of {name!r} completed but the managed name did not "
                            "follow it; rerun move only after inspecting Herdr"
                        )
                    target = agent.Target(
                        pane_id=moved.pane_id,
                        session_agent=record.session_agent,
                        session_value=record.session_value,
                        expected_agent=record.harness,
                        expected_workspace=expected_workspace,
                        expected_cwd=record.cwd,
                    )
                    info = agent.resolve_target(self.client, target)
                    final_presentations = [
                        pane for pane in self.client.panes(destination)
                        if pane.pane_id == info.pane_id
                    ]
                    if (len(final_presentations) != 1
                            or final_presentations[0].tab_id != moved.tab_id
                            or final_presentations[0].workspace_id != moved.workspace_id):
                        raise AgentDeliveryError(
                            f"move of {name!r} completed but final presentation "
                            "verification failed; rerun move to recover"
                        )
                    return replace(target, expected_workspace=None), (True, moved)

            outcome = agent.rebind_queue_after(
                self._queue(name), previous_target, perform,
                allow_legacy_workspace_binding=True,
            )
            assert isinstance(outcome, tuple)
            did_move, moved = outcome
            if not did_move:
                self._clear_move_intent(name)
                result = self._status_record(record)
                result.update({"moved": False, "recovered": False})
                return result
            assert moved is not None
            record.workspace_id = moved.workspace_id
            record.tab_id = moved.tab_id
            record.pane_id = moved.pane_id
            self._repin_moved_terminal(record)
            self._save(record)
            self._clear_move_intent(name)
            result = self._status_record(record)
            result.update({
                "moved": True,
                "recovered": False,
                "previous_pane_id": recorded_pane,
            })
            return result

    def _status_record(self, record: AgentRecord) -> dict[str, object]:
        """Probe one pinned record without resolving its name a second time."""
        name = record.name
        result: dict[str, object] = record.to_document()
        result["queue"] = self._queue(name)
        result["output"] = str(self._directory(name) / "output.json")
        result["goal_source"] = "requested" if record.goal is not None else None
        result["goal_delivery"] = self._goal_delivery(record)
        try:
            destination = self._pending_move_destination(record)
            if destination is not None:
                result.update({
                    "agent_status": "unknown",
                    "move_pending": True,
                    "move_destination_workspace_id": destination,
                    "probe_error": (
                        f"move of {name!r} is incomplete; rerun `agentctl move {name}`"
                    ),
                })
                return result
            client = cast(
                HerdrClient,
                _WorkspaceClient(self.client, record, check_prompt=False),
            )
            agent.resolve_target(client, self._target(record))
            result.update(agent.status(
                client, self._target(record), self._queue(name),
                allow_legacy_workspace_binding=True,
            ))
            result["probe_error"] = None
        except HerdrRunError as exc:
            result["agent_status"], result["probe_error"] = "unknown", str(exc)
        return result

    def list(self) -> list[dict[str, object]]:
        """List each row independently; one malformed row cannot hide the fleet."""
        if not self.registry.exists():
            return []
        agent._validate_private_directory(str(self.registry), "agent registry")
        rows: list[dict[str, object]] = []
        for path in sorted(self.registry.iterdir()):
            if not _NAME.fullmatch(path.name) or path.name == "archive":
                continue
            try:
                rows.append(self.status(path.name))
            except HerdrRunError as exc:
                rows.append({
                    "name": path.name,
                    "agent_status": "unknown",
                    "probe_error": str(exc),
                    "record_error": True,
                })
        return rows

    def send(self, name: str, text: str, *, message_id: str | None = None, expected_token: str | None = None, **options: object) -> agent.QueueResult:
        """Serialize against stop, then use the existing durable submission transport."""
        with self._lock(name):
            record = self._load_expected(name, expected_token)
            self._require_automation(record)
            client = cast(HerdrClient, _WorkspaceClient(
                self.client, record, queue=self._queue(name),
                expected_workspace=self._project_workspace(),
                peer_panes=self._peer_panes(name), claims=self._peer_claims(name),
            ))
            return agent.send(
                client, self._target(record), self._queue(name), text,
                message_id=message_id, allow_legacy_workspace_binding=True,
                **options,
            )

    def drain(self, name: str, **options: object) -> agent.QueueResult:
        """Retry only messages that the shared queue knows were never submitted."""
        with self._lock(name):
            record = self._load(name)
            self._require_automation(record)
            client = cast(HerdrClient, _WorkspaceClient(
                self.client, record, queue=self._queue(name),
                expected_workspace=self._project_workspace(),
                peer_panes=self._peer_panes(name), claims=self._peer_claims(name),
            ))
            return agent.drain(
                client, self._target(record), self._queue(name),
                allow_legacy_workspace_binding=True,
                **options,  # type: ignore[arg-type]
            )

    def read(self, name: str, *, lines: int = 500) -> str:
        """Read human and coordinator turns together; persist the latest bounded snapshot."""
        with self._lock(name):
            record = self._load(name)
            self._require_policy(record)
            self._checked(record)
            text = agent.read(self.client, self._target(record), lines=lines)
            agent._atomic_json(str(self._directory(name) / "output.json"),
                               {"text": text, "captured_at": time.time(), "pane_id": record.pane_id})
            return text

    def wait(
        self, name: str, *, timeout: float = 900.0,
        sleep: Callable[[float], None] = time.sleep,
        monotonic: Callable[[], float] = time.monotonic,
        wall: Callable[[], float] = time.time,
        expected_token: str | None = None,
    ) -> dict[str, object]:
        """Wait for idle/done, fail visibly on blocked/unknown identity or a deadline.

        This is readiness, not task completion: an active native goal may continue
        after a turn. Callers should inspect the conversation and goal separately.

        A screen-verified delivery proves the prompt left the composer, but the
        terminal server can report the agent idle until the harness visibly starts
        the turn. So an idle state within DELIVERY_SETTLE_SECONDS of the newest
        confirmed delivery counts only after this wait has seen the agent busy.
        """
        if not math.isfinite(timeout) or not 0 <= timeout <= 31_536_000:
            raise AgentDeliveryError("wait timeout must be finite and between 0 and 31536000 seconds")
        token = self._load_expected(name, expected_token).token
        delivered_at = _latest_confirmed_delivery(Path(self._queue(name)))
        seen_busy = False
        deadline = monotonic() + timeout
        while True:
            with self._lock(name):
                record = self._load_expected(name, token)
                info = self._checked(record)
                if info.status in ("working", "starting"):
                    seen_busy = True
                if info.status in ("idle", "done"):
                    if (seen_busy or delivered_at is None
                            or wall() - delivered_at >= DELIVERY_SETTLE_SECONDS):
                        return self._status_record(record)
                elif info.status not in ("working", "starting", "unknown"):
                    raise AgentDeliveryError(f"agent {name!r} requires attention (state {info.status!r}); read its pane")
            remaining = deadline - monotonic()
            if remaining <= 0:
                if info.status in ("idle", "done"):
                    raise AgentDeliveryError(
                        f"timed out waiting for agent {name!r}: it reads {info.status!r}, but a prompt "
                        f"was delivered {max(0.0, wall() - cast(float, delivered_at)):.1f}s ago and the "
                        f"agent has not been seen working since"
                    )
                raise AgentDeliveryError(f"timed out waiting for agent {name!r} (state {info.status!r})")
            sleep(min(0.25, remaining))

    def bind_session(self, name: str, session_id: str, *, goal_command: Sequence[str] | None = None) -> dict[str, object]:
        """Bind an explicitly known native session; never guess by cwd or change a queue binding."""
        if not session_id or "\0" in session_id:
            raise AgentDeliveryError("native session id must be nonempty and contain no NUL")
        if goal_command is not None and (not goal_command or any(not item or "\0" in item for item in goal_command)):
            raise AgentDeliveryError("goal command must be a nonempty argument vector")
        with self._lock(name):
            with self._identity_transaction():
                record = self._load(name)
                info = self._checked(record)
                for existing in (record.resume, record.goal_session_id, record.session_value, info.session_value,
                                 None if record.native_session is None else record.native_session["value"]):
                    if existing is not None and existing != session_id:
                        raise AgentDeliveryError("refusing to replace an already bound native session")
                owner = self._identity_owner(record.harness, session_id, exclude=name)
                if owner is not None:
                    raise AgentDeliveryError(
                        f"native session is already registered as {owner.name!r}"
                    )
                reported: list[str] = []
                for pane in self.client.panes():
                    live = self.client.pane_info(pane.pane_id)
                    if (live.session_agent == record.harness
                            and live.session_value == session_id):
                        reported.append(pane.pane_id)
                if reported and (len(reported) != 1
                                 or reported[0] != record.pane_id):
                    raise AgentDeliveryError(
                        "native session is reported by another or ambiguous live pane"
                    )
                # Some Herdr detection sources do not expose session metadata. The
                # caller explicitly asserts the native session; the independently
                # verified live name and pane remain the lifecycle authority.
                if (record._nested_storage is not None
                        and record._nested_storage.get("schema")
                        == "agentctl-session/v3"):
                    record.session_agent = record.harness
                    record.session_value = session_id
                    record.goal_session_id = session_id
                    record._session_source = "asserted"
                else:
                    record.goal_session_id = session_id
                if record.native_session is None:
                    record.native_session = _native_session(record.harness, session_id, "asserted")
                if goal_command is not None:
                    record.goal_command = list(goal_command)
                self._save(record)
                return {"name": name, "session_id": session_id, "source": "explicit"}

    def _goal_delivery(self, record: AgentRecord) -> str | None:
        if record.goal_message_id is not None:
            queue = Path(self._queue(record.name))
            filename = f"{record.goal_message_id}.json"
            for folder, outcome in (("processed", "delivered"), ("failed", "possibly_submitted"), ("inflight", "possibly_submitted"), ("inbox", "pending")):
                if os.path.lexists(queue / folder / filename):
                    return outcome
        return record.goal_delivery

    def _goal_result(self, record: AgentRecord, command: Sequence[str] | None) -> dict[str, object]:
        result: dict[str, object] = {"name": record.name, "goal": record.goal,
            "delivery": self._goal_delivery(record), "source": "requested", "native_status": "unverified"}
        if record.harness != "codex":
            return result
        session = record.goal_session_id or record.session_value
        if not session:
            result["native_error"] = "native session unknown; bind-session with the session id reported by this agent"
            return result
        from agentctl.codex_goal import CodexGoalError, get_goal
        try:
            native = get_goal(session, command or record.goal_command)
        except CodexGoalError as exc:
            result["native_error"] = str(exc)
        else:
            result["native"] = native
            result["source"] = "native"
            result["native_status"] = "absent" if native is None else native["status"]
            result["goal"] = None if native is None else native["objective"]
        return result

    def _dead_pane_proof(
        self, record: AgentRecord, *, operation: str, allow_reported_dead: bool = False,
    ) -> _DeadPaneProof:
        """Prove one exact absent-agent, one-pane tab and idle shell generation."""
        pane_id = record.pane_id
        tab_id = record.tab_id
        workspace_id = record.workspace_id
        if not pane_id or not tab_id or not workspace_id:
            raise AgentDeliveryError(
                f"refusing to {operation} {record.name!r}: record lacks pane, tab, or workspace identity"
            )
        def snapshot() -> tuple[Pane, AgentPaneInfo]:
            panes = self.client.panes()
            presentations = [pane for pane in panes if pane.pane_id == pane_id]
            if len(presentations) != 1:
                raise AgentDeliveryError(
                    f"refusing to {operation} {record.name!r}: expected one recorded pane, "
                    f"found {len(presentations)}"
                )
            presentation = presentations[0]
            if (presentation.tab_id != tab_id
                    or presentation.workspace_id != workspace_id):
                raise AgentDeliveryError(
                    f"refusing to {operation} {record.name!r}: recorded pane, tab, or workspace changed"
                )
            tab_panes = [pane for pane in panes if pane.tab_id == tab_id]
            if len(tab_panes) != 1 or tab_panes[0] != presentation:
                raise AgentDeliveryError(
                    f"refusing to {operation} {record.name!r}: recorded tab is not the exact one-pane tab"
                )
            info = self.client.pane_info(pane_id)
            if (info.pane_id != pane_id or info.workspace_id != workspace_id
                    or os.path.realpath(info.cwd) != os.path.realpath(record.cwd)):
                raise AgentDeliveryError(
                    f"refusing to {operation} {record.name!r}: recorded pane, workspace, or cwd changed"
                )
            stale_report = (allow_reported_dead and record.adapter in ("herdr-pane", "herdr-relay")
                and record.pane_reported_by_agentctl and info.agent in (None, record.harness)
                and (info.session_agent, info.session_value) in (
                    (None, None), (record.session_agent, record.session_value),
                    (record.harness, record.resume),
                    (record.harness, None if record.native_session is None else record.native_session["value"])))
            if info.agent is not None and not stale_report:
                raise AgentDeliveryError(
                    f"refusing to {operation} {record.name!r}: pane still reports agent {info.agent!r}"
                )
            if (info.session_agent is not None or info.session_value is not None) and not stale_report:
                raise AgentDeliveryError(
                    f"refusing to {operation} {record.name!r}: absent agent has native session identity"
                )
            return presentation, info

        presentation, info = snapshot()
        try:
            shell = self.client.pane_idle_shell_identity(pane_id)
        except HerdrRunError as exc:
            raise AgentDeliveryError(
                f"refusing to {operation} {record.name!r}: cannot prove supported "
                f"idle shell generation: {exc}"
            ) from exc
        if shell is None:
            raise AgentDeliveryError(
                f"refusing to {operation} {record.name!r}: pane is not a supported idle shell "
                "generation without descendants"
            )
        final_presentation, final_info = snapshot()
        if final_presentation != presentation or final_info != info:
            raise AgentDeliveryError(
                f"refusing to {operation} {record.name!r}: pane membership or agent state "
                "changed during shell proof"
            )
        return _DeadPaneProof(
            info=final_info, presentation=final_presentation, shell=shell,
        )

    def _bounded_terminal_text(self, pane_id: str, *, operation: str) -> str:
        try:
            output = self.client.read(
                pane_id, source="recent-unwrapped", lines=5000
            )
            if not output:
                output = self.client.read(pane_id, source="recent", lines=5000)
            return output
        except HerdrRunError as exc:
            raise AgentDeliveryError(
                f"cannot preserve terminal output before {operation}: {exc}"
            ) from exc

    def _archive_destination(self, record: AgentRecord) -> tuple[Path, Path]:
        archive = self.registry / "archive"
        try:
            archive.mkdir(mode=0o700, exist_ok=True)
        except OSError as exc:
            raise AgentDeliveryError(f"cannot prepare agent archive: {exc}") from exc
        agent._validate_private_directory(str(archive), "agent archive")
        destination = archive / f"{record.name}-{record.token}"
        if os.path.lexists(destination):
            raise AgentDeliveryError(
                f"refusing to overwrite existing agent archive {destination}"
            )
        return archive, destination

    @staticmethod
    def _optional_snapshot_bytes(pinned: _PinnedAgentDirectory) -> bytes | None:
        """Read an existing private snapshot exactly so publication can roll back."""
        path = pinned.path / "output.json"
        flags = (
            os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        )
        try:
            descriptor = os.open("output.json", flags, dir_fd=pinned.descriptor)
        except FileNotFoundError:
            return None
        except OSError as exc:
            raise AgentDeliveryError(
                f"cannot open existing output snapshot {path}: {exc}"
            ) from exc
        try:
            before = os.fstat(descriptor)
            if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
                    or stat.S_IMODE(before.st_mode) & 0o077 or before.st_nlink != 1
                    or before.st_size > _MAX_SNAPSHOT_BYTES):
                raise AgentDeliveryError(f"unsafe existing output snapshot: {path}")
            content = bytearray()
            while len(content) <= _MAX_SNAPSHOT_BYTES:
                block = os.read(descriptor, min(64 << 10, _MAX_SNAPSHOT_BYTES + 1 - len(content)))
                if not block:
                    break
                content.extend(block)
            after = os.fstat(descriptor)
            if (len(content) > _MAX_SNAPSHOT_BYTES or before.st_size != len(content)
                    or (after.st_dev, after.st_ino, after.st_mode, after.st_uid,
                        after.st_nlink, after.st_size, after.st_mtime_ns,
                        after.st_ctime_ns) != (
                            before.st_dev, before.st_ino, before.st_mode, before.st_uid,
                            before.st_nlink, before.st_size, before.st_mtime_ns,
                            before.st_ctime_ns,
                        )):
                raise AgentDeliveryError(f"existing output snapshot changed while reading: {path}")
            return bytes(content)
        except AgentDeliveryError:
            raise
        except OSError as exc:
            raise AgentDeliveryError(
                f"cannot inspect existing output snapshot {path}: {exc}"
            ) from exc
        finally:
            _close_descriptor(
                descriptor, "existing output snapshot", primary=sys.exc_info()[1],
            )

    @staticmethod
    def _atomic_snapshot_bytes(
        pinned: _PinnedAgentDirectory, content: bytes, *, name: str = "output.json",
    ) -> _InstalledArtifact:
        """Replace one private snapshot with exact bytes and durable directory metadata."""
        if name not in {"agent.json", "output.json", "stopped.json"} and re.fullmatch(r"[a-z0-9-]{1,80}\.json", name) is None:
            raise AgentDeliveryError("unsupported pinned registry artifact name")
        limit = _MAX_SNAPSHOT_BYTES if name == "output.json" else _MAX_AGENT_RECORD_BYTES
        if len(content) > limit:
            raise AgentDeliveryError(
                f"refusing {name} larger than {limit} bytes"
            )
        temporary = f".{name}-recovery-{os.getpid()}-{uuid.uuid4().hex}"
        descriptor = -1
        temporary_owned = False
        created: os.stat_result | None = None
        rename_attempted = False
        installed_verified = False
        failure: BaseException | None = None
        try:
            descriptor = os.open(
                temporary,
                os.O_RDWR | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0),
                0o600,
                dir_fd=pinned.descriptor,
            )
            temporary_owned = True
            created = os.fstat(descriptor)
            if (not stat.S_ISREG(created.st_mode)
                    or created.st_uid != os.getuid()
                    or stat.S_IMODE(created.st_mode) & 0o077
                    or created.st_nlink != 1):
                raise AgentDeliveryError(
                    f"unsafe pinned registry artifact staging file for {name}"
                )
            offset = 0
            while offset < len(content):
                written = os.write(descriptor, content[offset:])
                if written <= 0:
                    raise OSError("output snapshot restoration made no progress")
                offset += written
            os.fsync(descriptor)
            held = os.fstat(descriptor)
            held_content = os.pread(descriptor, len(content) + 1, 0)
            staged = os.stat(
                temporary, dir_fd=pinned.descriptor, follow_symlinks=False,
            )
            if (not stat.S_ISREG(held.st_mode)
                    or not stat.S_ISREG(staged.st_mode)
                    or held.st_uid != os.getuid() or staged.st_uid != os.getuid()
                    or stat.S_IMODE(held.st_mode) & 0o077
                    or stat.S_IMODE(staged.st_mode) & 0o077
                    or held.st_nlink != 1 or staged.st_nlink != 1
                    or held.st_size != len(content) or staged.st_size != len(content)
                    or held_content != content
                    or (held.st_dev, held.st_ino) != (created.st_dev, created.st_ino)
                    or (staged.st_dev, staged.st_ino) != (
                        created.st_dev, created.st_ino,
                    )):
                raise AgentDeliveryError(
                    f"registry artifact staging generation changed before installing {name}"
                )
            rename_attempted = True
            try:
                os.replace(
                    temporary, name,
                    src_dir_fd=pinned.descriptor, dst_dir_fd=pinned.descriptor,
                )
            except OSError as exc:
                raise AgentDeliveryError(
                    f"cannot install pinned registry artifact {name}: {exc}"
                ) from exc
            # The staging name no longer belongs to this operation.  Never
            # remove a new entry that appears there after the rename.
            temporary_owned = False
            installed = os.stat(
                name, dir_fd=pinned.descriptor, follow_symlinks=False,
            )
            held = os.fstat(descriptor)
            held_content = os.pread(descriptor, len(content) + 1, 0)
            if (not stat.S_ISREG(installed.st_mode)
                    or installed.st_uid != os.getuid()
                    or stat.S_IMODE(installed.st_mode) & 0o077
                    or installed.st_nlink != 1
                    or installed.st_size != len(content)
                    or held_content != content
                    or (installed.st_dev, installed.st_ino) != (
                        created.st_dev, created.st_ino,
                    )
                    or (held.st_dev, held.st_ino, held.st_size) != (
                        created.st_dev, created.st_ino, len(content),
                    )):
                raise AgentDeliveryError(
                    f"installed registry artifact {name} was not the staged generation"
                )
            installed_verified = True
            os.fsync(pinned.descriptor)
        except AgentDeliveryError as exc:
            failure = exc
        except OSError as exc:
            failure = AgentDeliveryError(
                f"cannot write pinned registry artifact {name}: {exc}"
            )

        # If replace(2) committed but its wrapper reported an error, classify
        # the namespace before any cleanup. A known installed generation may
        # be rolled back by a caller; a replacement or missing generation may
        # not be overwritten.
        if (failure is not None and rename_attempted and not installed_verified
                and created is not None):
            try:
                held = os.fstat(descriptor)
                held_content = os.pread(descriptor, len(content) + 1, 0)
                reconciled_staging: os.stat_result | None
                try:
                    reconciled_staging = os.stat(
                        temporary,
                        dir_fd=pinned.descriptor,
                        follow_symlinks=False,
                    )
                except FileNotFoundError:
                    reconciled_staging = None
                reconciled_install: os.stat_result | None
                try:
                    reconciled_install = os.stat(
                        name,
                        dir_fd=pinned.descriptor,
                        follow_symlinks=False,
                    )
                except FileNotFoundError:
                    reconciled_install = None
                held_exact = (
                    stat.S_ISREG(held.st_mode)
                    and held.st_uid == os.getuid()
                    and not stat.S_IMODE(held.st_mode) & 0o077
                    and held.st_nlink == 1
                    and held.st_size == len(content)
                    and held_content == content
                    and (held.st_dev, held.st_ino) == (created.st_dev, created.st_ino)
                )
                staged_exact = (
                    reconciled_staging is not None
                    and stat.S_ISREG(reconciled_staging.st_mode)
                    and reconciled_staging.st_uid == os.getuid()
                    and not stat.S_IMODE(reconciled_staging.st_mode) & 0o077
                    and reconciled_staging.st_nlink == 1
                    and (reconciled_staging.st_dev, reconciled_staging.st_ino) == (
                        created.st_dev, created.st_ino,
                    )
                )
                installed_exact = (
                    reconciled_install is not None
                    and stat.S_ISREG(reconciled_install.st_mode)
                    and reconciled_install.st_uid == os.getuid()
                    and not stat.S_IMODE(reconciled_install.st_mode) & 0o077
                    and reconciled_install.st_nlink == 1
                    and reconciled_install.st_size == len(content)
                    and (reconciled_install.st_dev, reconciled_install.st_ino) == (
                        created.st_dev, created.st_ino,
                    )
                )
                if held_exact and installed_exact and not staged_exact:
                    temporary_owned = False
                    installed_verified = True
                elif not (held_exact and staged_exact):
                    temporary_owned = False
            except OSError:
                temporary_owned = False

        cleanup_failures: list[tuple[str, BaseException]] = []
        if descriptor >= 0:
            try:
                os.close(descriptor)
            except OSError as exc:
                cleanup_failures.append(("close staging file", exc))
        if temporary_owned:
            try:
                staged = os.stat(
                    temporary,
                    dir_fd=pinned.descriptor,
                    follow_symlinks=False,
                )
                if (created is None
                        or (staged.st_dev, staged.st_ino) != (
                            created.st_dev, created.st_ino,
                        )
                        or not stat.S_ISREG(staged.st_mode)
                        or staged.st_uid != os.getuid()
                        or stat.S_IMODE(staged.st_mode) & 0o077
                        or staged.st_nlink != 1):
                    raise AgentDeliveryError(
                        "registry staging name no longer denotes the owned file; "
                        "replacement was preserved"
                    )
                # A same-uid process ignoring the cooperative registry lock can
                # still race this final identity check and unlink.
                os.unlink(temporary, dir_fd=pinned.descriptor)
            except FileNotFoundError:
                pass
            except (OSError, AgentDeliveryError) as exc:
                cleanup_failures.append(("remove staging file", exc))
        if cleanup_failures:
            detail = "; ".join(
                f"{label}: {error}" for label, error in cleanup_failures
            )
            cleanup_error = AgentDeliveryError(
                f"registry artifact cleanup failed: {detail}"
            )
            if failure is None:
                failure = cleanup_error
            else:
                failure = AgentDeliveryError(f"{failure}; {cleanup_error}")

        installed_artifact = (
            _InstalledArtifact(
                name,
                created.st_dev,
                created.st_ino,
                len(content),
                hashlib.sha256(content).hexdigest(),
            )
            if created is not None else None
        )
        if failure is not None:
            if installed_verified and installed_artifact is not None:
                raise _ArtifactInstalledError(
                    str(failure), installed_artifact,
                ) from failure
            if rename_attempted and not temporary_owned:
                raise _ArtifactPublicationUncertainError(str(failure)) from failure
            raise _ArtifactNotInstalledError(str(failure)) from failure
        if installed_artifact is None or not installed_verified:
            raise _ArtifactPublicationUncertainError(
                f"cannot prove installed registry artifact {name}"
            )
        return installed_artifact

    def _publish_pinned_directory(
        self,
        pinned: _PinnedAgentDirectory,
        destination: Path,
        *,
        expected_record: bytes,
        before_publish: Callable[[], None] | None = None,
    ) -> None:
        """Atomically publish the exact held generation and its exact record."""
        archive_path = destination.parent
        if archive_path != self.registry / "archive" or not destination.name:
            raise AgentDeliveryError("invalid agent archive destination")
        publication_state = False
        try:
            with self._pinned_parent_directory(
                self.registry, label="agent registry",
            ) as registry_parent, self._pinned_parent_directory(
                archive_path, label="agent archive",
            ) as archive_parent:
                self._verify_pinned_agent_directory(pinned)
                try:
                    source = os.stat(
                        pinned.name,
                        dir_fd=registry_parent.descriptor,
                        follow_symlinks=False,
                    )
                except OSError as exc:
                    raise AgentDeliveryError(
                        f"cannot inspect active agent directory: {exc}"
                    ) from exc
                if (source.st_dev, source.st_ino) != (pinned.device, pinned.inode):
                    raise AgentDeliveryError(
                        f"agent {pinned.name!r} registry directory changed before publication"
                    )
                if before_publish is not None:
                    before_publish()
                    self._verify_pinned_parent_directory(
                        registry_parent, label="agent registry",
                    )
                    self._verify_pinned_parent_directory(
                        archive_parent, label="agent archive",
                    )
                    self._verify_pinned_agent_directory(pinned)
                try:
                    _rename_directory_noreplace_at(
                        registry_parent.descriptor,
                        pinned.name,
                        archive_parent.descriptor,
                        destination.name,
                    )
                except Exception as rename_error:
                    # A successful rename may be reported as an error by the
                    # filesystem or wrapper.  Reconcile both names before the
                    # caller decides whether restoring output is safe.
                    try:
                        try:
                            active = os.stat(
                                pinned.name,
                                dir_fd=registry_parent.descriptor,
                                follow_symlinks=False,
                            )
                        except FileNotFoundError:
                            active = None
                        try:
                            archived = os.stat(
                                destination.name,
                                dir_fd=archive_parent.descriptor,
                                follow_symlinks=False,
                            )
                        except FileNotFoundError:
                            archived = None
                        opened = os.fstat(pinned.descriptor)
                    except OSError as proof_error:
                        publication_state = True
                        raise _ArchivePublicationUncertainError(
                            f"archive rename reported failure ({rename_error}) and its "
                            f"namespace outcome could not be proved: {proof_error}"
                        ) from proof_error
                    active_exact = (
                        active is not None
                        and stat.S_ISDIR(active.st_mode)
                        and active.st_uid == os.getuid()
                        and not stat.S_IMODE(active.st_mode) & 0o077
                        and (active.st_dev, active.st_ino)
                        == (pinned.device, pinned.inode)
                    )
                    archived_exact = (
                        archived is not None
                        and stat.S_ISDIR(archived.st_mode)
                        and archived.st_uid == os.getuid()
                        and not stat.S_IMODE(archived.st_mode) & 0o077
                        and (archived.st_dev, archived.st_ino)
                        == (pinned.device, pinned.inode)
                    )
                    opened_exact = (
                        stat.S_ISDIR(opened.st_mode)
                        and opened.st_uid == os.getuid()
                        and not stat.S_IMODE(opened.st_mode) & 0o077
                        and (opened.st_dev, opened.st_ino)
                        == (pinned.device, pinned.inode)
                    )
                    if active_exact and opened_exact and not archived_exact:
                        raise
                    publication_state = True
                    if active is None and archived_exact and opened_exact:
                        try:
                            published_record = self._record_bytes(
                                pinned, require_active_name=False,
                            )
                        except Exception as proof_error:
                            raise _ArchivePublicationUncertainError(
                                f"archive rename reported failure ({rename_error}); the "
                                "generation was published but its record could not be "
                                f"proved: {proof_error}"
                            ) from proof_error
                        if published_record == expected_record:
                            raise _ArchivePublicationUncertainError(
                                f"archive rename reported failure ({rename_error}) after the "
                                "proved generation was published"
                            ) from rename_error
                        raise _ArchivePublicationUncertainError(
                            f"archive rename reported failure ({rename_error}); the "
                            "generation was published but its record changed"
                        ) from rename_error
                    raise _ArchivePublicationUncertainError(
                        f"archive rename reported failure ({rename_error}) with an "
                        "ambiguous namespace outcome"
                    ) from rename_error
                publication_state = True
                try:
                    self._verify_pinned_parent_directory(
                        registry_parent, label="agent registry",
                    )
                    self._verify_pinned_parent_directory(
                        archive_parent, label="agent archive",
                    )
                    try:
                        os.stat(
                            pinned.name,
                            dir_fd=registry_parent.descriptor,
                            follow_symlinks=False,
                        )
                    except FileNotFoundError:
                        pass
                    except OSError as exc:
                        raise AgentDeliveryError(
                            f"cannot inspect active agent directory after publication: {exc}"
                        ) from exc
                    else:
                        raise AgentDeliveryError(
                            f"agent {pinned.name!r} active name reappeared during publication"
                        )
                    try:
                        published = os.stat(
                            destination.name,
                            dir_fd=archive_parent.descriptor,
                            follow_symlinks=False,
                        )
                        opened = os.fstat(pinned.descriptor)
                    except OSError as exc:
                        raise AgentDeliveryError(
                            f"cannot inspect published agent directory: {exc}"
                        ) from exc
                    if (not stat.S_ISDIR(published.st_mode)
                            or published.st_uid != os.getuid()
                            or stat.S_IMODE(published.st_mode) & 0o077
                            or not stat.S_ISDIR(opened.st_mode)
                            or opened.st_uid != os.getuid()
                            or stat.S_IMODE(opened.st_mode) & 0o077
                            or (published.st_dev, published.st_ino)
                            != (pinned.device, pinned.inode)
                            or (opened.st_dev, opened.st_ino)
                            != (pinned.device, pinned.inode)):
                        raise AgentDeliveryError(
                            f"agent {pinned.name!r} published directory was not the proved generation"
                        )
                    if self._record_bytes(
                        pinned, require_active_name=False,
                    ) != expected_record:
                        raise AgentDeliveryError(
                            f"agent {pinned.name!r} record changed during archival publication"
                        )
                except Exception as cause:
                    # All agentctl participants hold the name lock.  Reprove both
                    # namespace entries immediately before the inverse rename so a
                    # replacement at either name is never promoted to active state.
                    # renameat2 cannot compare an inode atomically, so an actor that
                    # ignores the cooperative lock can still race this last check;
                    # the no-replace rename at least refuses an occupied active name.
                    try:
                        os.stat(
                            pinned.name,
                            dir_fd=registry_parent.descriptor,
                            follow_symlinks=False,
                        )
                    except FileNotFoundError:
                        pass
                    except OSError as proof_error:
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}); cannot prove the "
                            f"active name absent before rollback: {proof_error}"
                        ) from proof_error
                    else:
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}); active name reappeared, "
                            "so rollback was not attempted"
                        ) from cause
                    try:
                        rollback_source = os.stat(
                            destination.name,
                            dir_fd=archive_parent.descriptor,
                            follow_symlinks=False,
                        )
                    except OSError as proof_error:
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}); cannot reprove the "
                            f"published generation before rollback: {proof_error}"
                        ) from proof_error
                    if (not stat.S_ISDIR(rollback_source.st_mode)
                            or rollback_source.st_uid != os.getuid()
                            or stat.S_IMODE(rollback_source.st_mode) & 0o077
                            or (rollback_source.st_dev, rollback_source.st_ino) != (
                                pinned.device, pinned.inode,
                            )):
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}); archive destination "
                            "was replaced, or became unsafe, so rollback was not attempted"
                        ) from cause
                    try:
                        _rename_directory_noreplace_at(
                            archive_parent.descriptor,
                            destination.name,
                            registry_parent.descriptor,
                            pinned.name,
                        )
                    except Exception as rollback:
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}) and rollback was incomplete: "
                            f"{rollback}"
                        ) from rollback
                    try:
                        self._verify_pinned_parent_directory(
                            registry_parent, label="agent registry",
                        )
                        self._verify_pinned_parent_directory(
                            archive_parent, label="agent archive",
                        )
                        self._verify_pinned_agent_directory(pinned)
                        restored = os.stat(
                            pinned.name,
                            dir_fd=registry_parent.descriptor,
                            follow_symlinks=False,
                        )
                        if (restored.st_dev, restored.st_ino) != (
                            pinned.device, pinned.inode,
                        ):
                            raise AgentDeliveryError(
                                "restored active name is not the proved generation"
                            )
                        try:
                            os.stat(
                                destination.name,
                                dir_fd=archive_parent.descriptor,
                                follow_symlinks=False,
                            )
                        except FileNotFoundError:
                            pass
                        else:
                            raise AgentDeliveryError(
                                "archive destination remained after rollback"
                            )
                        if self._record_bytes(pinned) != expected_record:
                            raise AgentDeliveryError(
                                "restored agent record is not the proved content"
                            )
                    except Exception as rollback_proof:
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}); inverse rename completed "
                            f"but rollback state could not be proved: {rollback_proof}"
                        ) from rollback_proof
                    sync_failures: list[str] = []
                    for parent, label in (
                        (archive_parent, "agent archive rollback"),
                        (registry_parent, "agent registry rollback"),
                    ):
                        try:
                            _fsync_pinned_directory(parent.descriptor, label)
                        except OSError as exc:
                            sync_failures.append(f"{label}: {exc}")
                    if sync_failures:
                        raise _ArchivePublicationUncertainError(
                            f"archive identity check failed ({cause}); rollback completed but "
                            f"directory durability is uncertain: {'; '.join(sync_failures)}"
                        ) from cause
                    publication_state = False
                    raise
                sync_failures = []
                for parent, label in (
                    (archive_parent, "published agent archive"),
                    (registry_parent, "published agent registry"),
                ):
                    try:
                        _fsync_pinned_directory(parent.descriptor, label)
                    except OSError as exc:
                        sync_failures.append(f"{label}: {exc}")
                if sync_failures:
                    raise _ArchivePublicationUncertainError(
                        "agent archive was published but directory durability is uncertain: "
                        f"{'; '.join(sync_failures)}"
                    )
        except _ArchivePublicationUncertainError:
            raise
        except Exception as error:
            if publication_state:
                raise _ArchivePublicationUncertainError(
                    f"agent archive publication state is uncertain after publication: {error}"
                ) from error
            raise

    @staticmethod
    def _restore_output_snapshot(
        pinned: _PinnedAgentDirectory, previous: bytes | None,
    ) -> None:
        if previous is None:
            try:
                os.unlink("output.json", dir_fd=pinned.descriptor)
            except FileNotFoundError:
                pass
            os.fsync(pinned.descriptor)
        else:
            ManagedAgents._atomic_snapshot_bytes(pinned, previous)

    @staticmethod
    def _verify_installed_output_snapshot(
        pinned: _PinnedAgentDirectory,
        installed: _InstalledArtifact,
    ) -> None:
        """Require output to retain the exact generation and bytes we installed."""
        descriptor = -1
        try:
            descriptor = os.open(
                installed.name,
                os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0),
                dir_fd=pinned.descriptor,
            )
            before = os.fstat(descriptor)
            content = bytearray()
            while len(content) <= installed.size:
                block = os.read(
                    descriptor,
                    min(64 << 10, installed.size + 1 - len(content)),
                )
                if not block:
                    break
                content.extend(block)
            after = os.fstat(descriptor)
        except OSError as exc:
            raise _ArchivePublicationUncertainError(
                f"cannot reprove installed output before rollback: {exc}"
            ) from exc
        finally:
            if descriptor >= 0:
                _close_descriptor(
                    descriptor,
                    "installed output rollback proof",
                    primary=sys.exc_info()[1],
                )
        if (not stat.S_ISREG(before.st_mode)
                or before.st_uid != os.getuid()
                or stat.S_IMODE(before.st_mode) & 0o077
                or before.st_nlink != 1
                or before.st_size != installed.size
                or len(content) != installed.size
                or hashlib.sha256(content).hexdigest() != installed.digest
                or (before.st_dev, before.st_ino) != (
                    installed.device, installed.inode,
                )
                or (after.st_dev, after.st_ino, after.st_mode, after.st_uid,
                    after.st_nlink, after.st_size, after.st_mtime_ns,
                    after.st_ctime_ns) != (
                        before.st_dev, before.st_ino, before.st_mode, before.st_uid,
                        before.st_nlink, before.st_size, before.st_mtime_ns,
                        before.st_ctime_ns,
                    )):
            raise _ArchivePublicationUncertainError(
                "installed output generation changed before use; replacement was preserved"
            )

    @staticmethod
    def _restore_installed_output_snapshot(
        pinned: _PinnedAgentDirectory,
        previous: bytes | None,
        installed: _InstalledArtifact,
    ) -> None:
        """Restore only while output still names the generation we installed."""
        ManagedAgents._verify_installed_output_snapshot(pinned, installed)
        # Registry participants hold the per-name lock; a same-uid process that
        # ignores it can still race this final check and replacement operation.
        ManagedAgents._restore_output_snapshot(pinned, previous)

    def _publish_archive(
        self,
        pinned: _PinnedAgentDirectory,
        destination: Path,
        snapshot: dict[str, object],
        *,
        expected_record: bytes,
        before_publish: Callable[[], None],
    ) -> None:
        """Prepare output, reprove state, then publish in one namespace move."""
        previous = self._optional_snapshot_bytes(pinned)
        installed: _InstalledArtifact | None = None
        try:
            installed = self._atomic_snapshot_bytes(
                pinned, _snapshot_json_bytes(snapshot)
            )
            self._verify_installed_output_snapshot(pinned, installed)
            before_publish()
            self._publish_pinned_directory(
                pinned, destination, expected_record=expected_record,
            )
        except _ArchivePublicationUncertainError:
            # The held directory was moved and could not be safely returned to
            # the active name.  Restoring through its fd would silently mutate a
            # retained or displaced generation rather than roll back publication.
            raise
        except _ArtifactNotInstalledError:
            # The previous named output was never replaced.
            raise
        except _ArtifactInstalledError as error:
            try:
                self._restore_installed_output_snapshot(
                    pinned, previous, error.installed,
                )
            except Exception as rollback:
                raise _ArchivePublicationUncertainError(
                    f"archive preparation failed ({error}) and exact output rollback "
                    f"was unsafe or incomplete: {rollback}"
                ) from rollback
            raise
        except Exception:
            if installed is None:
                raise
            try:
                self._restore_installed_output_snapshot(pinned, previous, installed)
            except Exception as rollback:
                raise _ArchivePublicationUncertainError(
                    "archive publication failed and exact output rollback was unsafe or "
                    f"incomplete: {rollback}"
                ) from rollback
            raise

    def _recover_legacy_adoption(
        self, record: AgentRecord, *, expected_token: str | None,
        expected_record_sha256: str | None,
        expected_token_explicit: bool,
    ) -> dict[str, object]:
        """Explicitly retire an identity-less adopted record without touching its pane."""
        if not record.pane_id:
            raise AgentDeliveryError(
                f"refusing to recover legacy adoption {record.name!r}: record lacks pane identity"
            )
        with self._pane_lock(record.pane_id):
            with self._pinned_agent_directory(record.name) as pinned:
                return self._recover_legacy_adoption_locked(
                    record, pinned=pinned, expected_token=expected_token,
                    expected_record_sha256=expected_record_sha256,
                    expected_token_explicit=expected_token_explicit,
                )

    def _recover_legacy_adoption_locked(
        self, record: AgentRecord, *, pinned: _PinnedAgentDirectory,
        expected_token: str | None,
        expected_record_sha256: str | None,
        expected_token_explicit: bool,
    ) -> dict[str, object]:
        if (not expected_token_explicit or expected_token is None
                or expected_record_sha256 is None):
            raise AgentDeliveryError(
                "legacy adoption recovery requires --expected-token and "
                "--expected-record-sha256"
            )
        initial = self._legacy_record_snapshot(
            pinned,
            expected_token=expected_token,
            expected_digest=expected_record_sha256,
        )
        if initial.record.to_document() != record.to_document():
            raise AgentDeliveryError(
                f"refusing to recover adoption {record.name!r}: registry record changed"
            )
        record = initial.record
        if (record.adapter != "herdr-foreign"
                or record.lifecycle != "running"
                or record.mode != "interactive"
                or record.backend != "herdr"
                or record.foreign_shell_identity is not None):
            raise AgentDeliveryError(
                "--recover-legacy-adoption applies only to a running herdr-foreign "
                "record missing foreign_shell_identity"
            )
        before = self._dead_pane_proof(record, operation="recover legacy adoption")
        text = self._bounded_terminal_text(
            before.info.pane_id, operation="legacy adoption recovery"
        )
        current_snapshot = self._legacy_record_snapshot(
            pinned,
            expected_token=expected_token,
            expected_digest=expected_record_sha256,
        )
        if current_snapshot != initial:
            raise AgentDeliveryError(
                f"refusing to recover legacy adoption {record.name!r}: registry record changed"
            )
        current = current_snapshot.record
        after = self._dead_pane_proof(current, operation="recover legacy adoption")
        if after != before:
            raise AgentDeliveryError(
                f"refusing to recover legacy adoption {record.name!r}: "
                "runtime identity changed during output capture"
            )
        _archive, destination = self._archive_destination(current)

        def reprove_before_publish() -> None:
            final_snapshot = self._legacy_record_snapshot(
                pinned,
                expected_token=expected_token,
                expected_digest=expected_record_sha256,
            )
            if final_snapshot != initial:
                raise AgentDeliveryError(
                    f"refusing to recover adoption {record.name!r}: registry record changed"
                )
            if self._dead_pane_proof(
                final_snapshot.record, operation="recover legacy adoption"
            ) != before:
                raise AgentDeliveryError(
                    f"refusing to recover legacy adoption {record.name!r}: "
                    "runtime identity changed before archival"
                )

        self._publish_archive(
            pinned,
            destination,
            {"text": text, "captured_at": time.time(), "pane_id": record.pane_id,
             "recovery_shell_identity": {
                 **asdict(before.shell.identity),
                 "executable_path": before.shell.executable_path,
             }},
            expected_record=initial.content,
            before_publish=reprove_before_publish,
        )
        return {
            "name": record.name,
            "archive": str(destination),
            "pane_closed": False,
            "tab_closed": False,
            "runtime_preserved": True,
            "recovered_legacy_adoption": True,
            "record_sha256": expected_record_sha256,
            "recovery_shell_identity": {
                **asdict(before.shell.identity),
                "executable_path": before.shell.executable_path,
            },
        }

    def _retire_managed_dead(
        self, record: AgentRecord, *, expected_token: str | None,
        expected_token_explicit: bool,
    ) -> dict[str, object]:
        """Retire one owned pane only after proving a stable returned shell."""
        if not record.pane_id:
            raise AgentDeliveryError(
                f"refusing to retire dead managed agent {record.name!r}: record lacks pane identity"
            )
        with self._pane_lock(record.pane_id):
            with self._pinned_agent_directory(record.name) as pinned:
                return self._retire_managed_dead_locked(
                    record, pinned=pinned, expected_token=expected_token,
                    expected_token_explicit=expected_token_explicit,
                )

    def _retire_managed_dead_locked(
        self, record: AgentRecord, *, pinned: _PinnedAgentDirectory,
        expected_token: str | None,
        expected_token_explicit: bool,
    ) -> dict[str, object]:
        initial = self._managed_record_snapshot(
            pinned, expected_token=expected_token if expected_token is not None else record.token,
        )
        if initial.record.to_document() != record.to_document():
            raise AgentDeliveryError(
                f"refusing to retire dead managed agent {record.name!r}: registry record changed"
            )
        record = initial.record
        if (record.adapter != "herdr" or record.lifecycle != "running"
                or record.mode != "interactive" or record.backend != "herdr"):
            raise AgentDeliveryError(
                "managed-dead retirement requires a running herdr record in "
                "interactive/herdr mode"
            )
        if not expected_token_explicit or expected_token is None:
            raise _with_recovery(AgentDeliveryError(
                f"retiring dead managed agent {record.name!r} requires --expected-token"
            ), RecoveryAction("stop", name=record.name, token=record.token))
        before = self._dead_pane_proof(record, operation="retire dead managed agent")
        text = self._bounded_terminal_text(
            before.info.pane_id, operation="managed-dead retirement"
        )
        current_snapshot = self._managed_record_snapshot(
            pinned, expected_token=expected_token,
        )
        if current_snapshot != initial:
            raise AgentDeliveryError(
                f"refusing to retire dead managed agent {record.name!r}: registry record changed"
            )
        current = current_snapshot.record
        after = self._dead_pane_proof(current, operation="retire dead managed agent")
        if after != before:
            raise AgentDeliveryError(
                f"refusing to retire dead managed agent {record.name!r}: "
                "runtime identity changed during output capture"
            )
        _archive, destination = self._archive_destination(current)
        final_snapshot = self._managed_record_snapshot(
            pinned, expected_token=expected_token,
        )
        if final_snapshot != initial:
            raise AgentDeliveryError(
                f"refusing to retire dead managed agent {record.name!r}: registry record changed"
            )
        final = final_snapshot.record
        if self._dead_pane_proof(final, operation="retire dead managed agent") != before:
            raise AgentDeliveryError(
                f"refusing to retire dead managed agent {record.name!r}: "
                "runtime identity changed before close"
            )
        final.lifecycle = "stopped"
        stopped_bytes = agent._json_text(final.to_document()).encode("utf-8")
        if len(stopped_bytes) > _MAX_AGENT_RECORD_BYTES:
            raise AgentDeliveryError(
                f"refusing stopped agent record larger than {_MAX_AGENT_RECORD_BYTES} bytes"
            )
        previous_output = self._optional_snapshot_bytes(pinned)
        installed: _InstalledArtifact | None = None
        try:
            installed = self._atomic_snapshot_bytes(
                pinned,
                _snapshot_json_bytes({
                    "text": text, "captured_at": time.time(), "pane_id": record.pane_id,
                    "retirement_shell_identity": {
                        **asdict(before.shell.identity),
                        "executable_path": before.shell.executable_path,
                    },
                }),
            )
        except _ArtifactNotInstalledError:
            raise
        except _ArtifactPublicationUncertainError:
            raise
        except _ArtifactInstalledError as error:
            try:
                self._restore_installed_output_snapshot(
                    pinned, previous_output, error.installed,
                )
            except Exception as rollback:
                raise _ArchivePublicationUncertainError(
                    f"managed retirement preparation failed ({error}) and exact output "
                    f"rollback was unsafe or incomplete: {rollback}"
                ) from rollback
            raise
        except Exception:
            if installed is None:
                raise
            try:
                self._restore_installed_output_snapshot(
                    pinned, previous_output, installed,
                )
            except Exception as rollback:
                raise _ArchivePublicationUncertainError(
                    "managed retirement preparation failed and exact output rollback was "
                    f"unsafe or incomplete: {rollback}"
                ) from rollback
            raise
        try:
            assert installed is not None
            self._verify_installed_output_snapshot(pinned, installed)
            if self._record_bytes(pinned) != initial.content:
                raise AgentDeliveryError(
                    f"refusing to retire dead managed agent {record.name!r}: "
                    "registry record changed immediately before close"
                )
            if self._dead_pane_proof(
                final, operation="retire dead managed agent"
            ) != before:
                raise AgentDeliveryError(
                    f"refusing to retire dead managed agent {record.name!r}: "
                    "runtime identity changed immediately before close"
                )
        except Exception:
            try:
                assert installed is not None
                self._restore_installed_output_snapshot(
                    pinned, previous_output, installed,
                )
            except Exception as rollback:
                raise _ArchivePublicationUncertainError(
                    "managed retirement final runtime proof failed and rollback "
                    f"was unsafe or incomplete: {rollback}"
                ) from rollback
            raise
        assert final.pane_id is not None
        self.client.close_pane(final.pane_id)
        self._atomic_snapshot_bytes(pinned, stopped_bytes, name="agent.json")
        self._publish_pinned_directory(
            pinned, destination, expected_record=stopped_bytes,
        )
        try:
            tab_closed: bool | None = not any(
                pane.tab_id == final.tab_id for pane in self.client.panes()
            )
        except HerdrRunError:
            tab_closed = None
        return {
            "name": record.name,
            "archive": str(destination),
            "pane_closed": True,
            "tab_closed": tab_closed,
            "managed_dead": True,
        }

    def goal(self, name: str, text: str | None = None, *, goal_command: Sequence[str] | None = None, **options: object) -> dict[str, object]:
        """Read native goal state when bound, or submit a goal to the visible conversation.

        Codex receives its native /goal command. Other harnesses receive a plain
        goal instruction. Delivery proves submission, not native goal completion.
        """
        if text is None:
            record = self._load(name)
            self._checked(record)
            return self._goal_result(record, goal_command)
        if not text.strip() or "\n" in text or "\r" in text:
            raise AgentDeliveryError("goal must be a nonempty single line")
        with self._lock(name):
            record = self._load(name)
            self._require_automation(record)
            record.goal, record.goal_delivery = text, "pending"
            identifier = f"{time.time_ns():020d}-{os.getpid()}"
            record.goal_message_id = identifier
            if record.harness == "codex":
                record.goal_messages[identifier] = text
            self._save(record)
            prompt = f"/goal {text}" if record.harness == "codex" else f"Your ongoing goal: {text}\nWork toward this goal and report completion or blockers."
            try:
                client = cast(HerdrClient, _WorkspaceClient(
                    self.client, record, queue=self._queue(name),
                    expected_workspace=self._project_workspace(),
                    peer_panes=self._peer_panes(name), claims=self._peer_claims(name),
                ))
                result = agent.send(
                    client, self._target(record), self._queue(name), prompt,
                    message_id=identifier, allow_legacy_workspace_binding=True,
                    **options,
                )
            except HerdrRunError as exc:
                record.goal_delivery = str(getattr(exc, "outcome", "failed"))
                self._save(record)
                raise
            record.goal_delivery = result.outcome
            self._save(record)
            return self._goal_result(record, goal_command)

    def stop(
        self, name: str, *, expected_token: str | None = None,
        recover_legacy_adoption: bool = False,
        expected_record_sha256: str | None = None,
        retire_dead_adoption: bool = False,
    ) -> dict[str, object]:
        """Retire one exact registered generation or recover an adopted runtime."""
        return self._stop(
            name,
            expected_token=expected_token,
            recover_legacy_adoption=recover_legacy_adoption,
            expected_record_sha256=expected_record_sha256,
            expected_token_explicit=expected_token is not None,
            retire_dead_adoption=retire_dead_adoption,
        )

    def _stop(
        self, name: str, *, expected_token: str | None,
        recover_legacy_adoption: bool,
        expected_record_sha256: str | None,
        expected_token_explicit: bool,
        retire_dead_adoption: bool = False,
    ) -> dict[str, object]:
        """Close only the owned single-pane tab and archive the complete record/queue.

        A confirmed missing pane permits archival. A probe failure, changed agent
        identity, or extra human-created panes refuses teardown and keeps state.
        """
        if retire_dead_adoption and recover_legacy_adoption:
            raise AgentDeliveryError(
                "--retire-dead-adoption and --recover-legacy-adoption are mutually exclusive"
            )
        if retire_dead_adoption:
            if (not expected_token_explicit or expected_token is None
                    or expected_record_sha256 is None):
                raise AgentDeliveryError(
                    "dead-adoption retirement requires --expected-token and --expected-record-sha256"
                )
            if re.fullmatch(r"[a-z0-9-]{1,80}", expected_token) is None:
                raise AgentDeliveryError("expected-token must be a valid recorded generation token")
            if _SHA256.fullmatch(expected_record_sha256) is None:
                raise AgentDeliveryError(
                    "expected-record-sha256 must be exactly 64 lowercase hexadecimal characters"
                )
        with self._lock(name):
            record = self._load_expected(name, expected_token)
            confirmed_record = self._load_expected(name, record.token)
            if confirmed_record.to_document() != record.to_document():
                raise AgentDeliveryError(f"agent {name!r} record changed before stop")
            record = confirmed_record
            pending_destination = self._pending_move_destination(record)
            if pending_destination is not None:
                raise _with_recovery(AgentDeliveryError(
                    f"refusing to stop {name!r}: move is incomplete"
                ), RecoveryAction("move", name=name, token=record.token))
            if retire_dead_adoption:
                assert expected_token is not None and expected_record_sha256 is not None
                return self._retire_dead_adoption(
                    record, expected_token=expected_token,
                    expected_record_sha256=expected_record_sha256,
                )
            if recover_legacy_adoption:
                return self._recover_legacy_adoption(
                    record, expected_token=expected_token,
                    expected_record_sha256=expected_record_sha256,
                    expected_token_explicit=expected_token_explicit,
                )
            if expected_record_sha256 is not None:
                raise AgentDeliveryError(
                    "--expected-record-sha256 requires --recover-legacy-adoption or --retire-dead-adoption"
                )
            recovery = self._dead_adoption_stop_recovery(record)
            try:
                return self._stop_runtime(
                    record, expected_token=expected_token,
                    expected_token_explicit=expected_token_explicit,
                )
            except HerdrRunError as exc:
                if recovery is not None and exc.recovery_action is None:
                    _with_recovery(exc, recovery)
                raise

    def _stop_runtime(
        self, record: AgentRecord, *, expected_token: str | None,
        expected_token_explicit: bool,
    ) -> dict[str, object]:
        """Run ordinary runtime retirement under the caller's name lock."""
        name = record.name
        if (record.adapter == "herdr-foreign" and record.pane_id is not None
                and not any(pane.pane_id == record.pane_id for pane in self.client.panes())):
            closed = self._closed_foreign_target(record)
            if closed is not None:
                return self._archive_closed_foreign(record, *closed)
        if record.adapter == "herdr-foreign":
            # This registry owns only delivery state.  Revalidate and retain
            # one final snapshot, but never close, rename, signal, or
            # otherwise mutate the adopted runtime.
            def inspect_foreign() -> tuple[str, AgentPaneInfo, Pane]:
                presentations = [
                    pane for pane in self.client.panes()
                    if pane.pane_id == record.pane_id
                ]
                if len(presentations) != 1:
                    raise AgentDeliveryError(
                        f"refusing to unregister adopted agent {name!r}: "
                        f"expected one recorded pane, found {len(presentations)}"
                    )
                presentation = presentations[0]
                if presentation.workspace_id != record.workspace_id:
                    raise AgentDeliveryError(
                        f"refusing to unregister adopted agent {name!r}: "
                        "recorded presentation workspace changed"
                    )
                info = self.client.pane_info(presentation.pane_id)
                if (info.pane_id != record.pane_id
                        or info.workspace_id != record.workspace_id
                        or os.path.realpath(info.cwd) != os.path.realpath(record.cwd)):
                    raise AgentDeliveryError(
                        f"refusing to unregister adopted agent {name!r}: "
                        "recorded pane, workspace, or cwd changed"
                    )
                shell_identity = record.foreign_shell_identity
                if shell_identity is None:
                    error = AgentDeliveryError(
                        f"refusing to unregister adopted agent {name!r}: "
                        "legacy record has no identity-bound pane shell"
                    )
                    if (presentation.tab_id == record.tab_id and info.agent is None
                            and info.session_agent is None and info.session_value is None):
                        recovery = self._legacy_stop_recovery(record)
                        if recovery is not None:
                            error = _with_recovery(error, recovery)
                    raise error
                if self.client.pane_shell_identity(info.pane_id) != shell_identity:
                    raise AgentDeliveryError(
                        f"refusing to unregister adopted agent {name!r}: "
                        "recorded pane shell generation changed"
                    )
                if info.agent is None:
                    # A live agent is pinned by its harness and, when one was
                    # reported, native session.  A returned shell has no such
                    # process identity, so its recorded tab remains part of
                    # the fallback proof.  This still lets an operator
                    # unregister a fully revalidated live agent after moving
                    # its pane between tabs in the same workspace.
                    if presentation.tab_id != record.tab_id:
                        raise AgentDeliveryError(
                            f"refusing to unregister adopted agent {name!r}: "
                            "recorded tab changed while the agent was absent"
                        )
                    if info.session_agent is not None or info.session_value is not None:
                        raise AgentDeliveryError(
                            f"refusing pane {info.pane_id}: absent agent has "
                            "native session identity"
                        )
                    if not self.client.pane_is_same_idle_shell(
                        info.pane_id, shell_identity
                    ):
                        raise AgentDeliveryError(
                            f"refusing pane {info.pane_id}: absent agent is not "
                            "at the recorded identity-bound idle shell process group"
                        )
                    return "absent", info, presentation
                return "live", self._checked(record), presentation

            state, info, presentation = inspect_foreign()
            output = self._bounded_terminal_text(
                info.pane_id, operation="unregistering"
            )
            try:
                final_state, final_info, final_presentation = inspect_foreign()
            except (AgentDeliveryError, HerdrRunError) as exc:
                raise AgentDeliveryError(
                    f"refusing to unregister adopted agent {name!r}: "
                    "runtime identity could not be reverified after output capture"
                ) from exc
            if (
                final_state,
                final_info.pane_id,
                final_info.workspace_id,
                os.path.realpath(final_info.cwd),
                final_info.agent,
                final_info.session_agent,
                final_info.session_value,
                final_presentation.tab_id,
                final_presentation.workspace_id,
            ) != (
                state,
                info.pane_id,
                info.workspace_id,
                os.path.realpath(info.cwd),
                info.agent,
                info.session_agent,
                info.session_value,
                presentation.tab_id,
                presentation.workspace_id,
            ):
                raise AgentDeliveryError(
                    f"refusing to unregister adopted agent {name!r}: "
                    "runtime identity changed during output capture"
                )
            archive, destination = self._archive_destination(record)
            agent._atomic_json(str(self._directory(name) / "output.json"),
                               {"text": output, "captured_at": time.time(),
                                "pane_id": record.pane_id})
            try:
                persisted_state, persisted_info, persisted_presentation = inspect_foreign()
            except (AgentDeliveryError, HerdrRunError) as exc:
                raise AgentDeliveryError(
                    f"refusing to unregister adopted agent {name!r}: "
                    "runtime identity could not be reverified before archival"
                ) from exc
            if (
                persisted_state,
                persisted_info.pane_id,
                persisted_info.workspace_id,
                os.path.realpath(persisted_info.cwd),
                persisted_info.agent,
                persisted_info.session_agent,
                persisted_info.session_value,
                persisted_presentation.tab_id,
                persisted_presentation.workspace_id,
            ) != (
                state,
                info.pane_id,
                info.workspace_id,
                os.path.realpath(info.cwd),
                info.agent,
                info.session_agent,
                info.session_value,
                presentation.tab_id,
                presentation.workspace_id,
            ):
                raise AgentDeliveryError(
                    f"refusing to unregister adopted agent {name!r}: "
                    "runtime identity changed before archival"
                )
            record.lifecycle = "stopped"
            self._save(record)
            os.rename(self._directory(name), destination)
            agent._fsync_dir(str(archive))
            agent._fsync_dir(str(self.registry))
            return {"name": name, "archive": str(destination),
                    "pane_closed": False, "tab_closed": False,
                    "runtime_preserved": True}
        panes = self.client.panes()
        if (record.adapter == "herdr" and record.lifecycle in ("running", "stopping")
                and record.pane_id is not None):
            recorded = [pane for pane in panes if pane.pane_id == record.pane_id]
            if len(recorded) == 1:
                observed = self.client.pane_info(record.pane_id)
                if observed.agent is None:
                    if record.lifecycle != "running":
                        raise AgentDeliveryError(
                            "managed-dead retirement requires a running herdr record"
                        )
                    return self._retire_managed_dead(
                        record, expected_token=expected_token,
                        expected_token_explicit=expected_token_explicit,
                    )
        tab_closed: bool | None = False
        owned = [pane for pane in panes if pane.tab_id == record.tab_id]
        if record.pane_id is None and record.lifecycle == "launch_failed" and len(owned) == 1:
            # Recover an older partial allocation only when its unique root
            # still identifies an unclaimed shell in the recorded directory.
            info = self.client.pane_info(owned[0].pane_id)
            if (info.agent is None and info.workspace_id == record.workspace_id
                    and os.path.realpath(info.cwd) == os.path.realpath(record.cwd)):
                record.pane_id = owned[0].pane_id
                self._save(record)
        if any(pane.pane_id == record.pane_id and pane.tab_id != record.tab_id for pane in panes):
            raise AgentDeliveryError("refusing to archive an agent whose pane moved to another tab")
        if owned:
            if len(owned) != 1 or owned[0].pane_id != record.pane_id or owned[0].workspace_id != record.workspace_id:
                raise AgentDeliveryError("refusing to close a tab whose pane ownership changed")
        archive, destination = self._archive_destination(record)
        if owned:
            if (record.adapter in ("herdr-pane", "herdr-relay") or record.lifecycle == "running"
                    or self.client.pane_info(owned[0].pane_id).agent is not None):
                self._checked_or_failed_pane_report(record, owned[0].pane_id)
            try:
                output = self.client.read(owned[0].pane_id, source="recent-unwrapped", lines=5000)
                if not output:
                    output = self.client.read(owned[0].pane_id, source="recent", lines=5000)
                agent._atomic_json(str(self._directory(name) / "output.json"),
                                   {"text": output, "captured_at": time.time(), "pane_id": record.pane_id})
            except HerdrRunError as exc:
                raise AgentDeliveryError(f"cannot preserve terminal output before stop: {exc}") from exc
            # Output capture can involve another control round trip. Recheck
            # the owned native identity before acting on that pane again.
            if (record.adapter in ("herdr-pane", "herdr-relay") or record.lifecycle == "running"
                    or self.client.pane_info(owned[0].pane_id).agent is not None):
                self._checked_or_failed_pane_report(record, owned[0].pane_id)
            record.lifecycle = "stopping"
            self._save(record)
            assert record.pane_id is not None
            self.client.close_pane(record.pane_id)
            try:
                tab_closed = not any(pane.tab_id == record.tab_id for pane in self.client.panes())
            except HerdrRunError:
                tab_closed = None
        record.lifecycle = "stopped"
        self._save(record)
        os.rename(self._directory(name), destination)
        agent._fsync_dir(str(archive))
        agent._fsync_dir(str(self.registry))
        return {"name": name, "archive": str(destination), "pane_closed": bool(owned), "tab_closed": tab_closed}

    def _closed_foreign_target(self, record: AgentRecord) -> tuple[str, str] | None:
        """The doctor finding and Herdr's refusal when Herdr positively reports the
        adopted record's workspace or pane as not found; ``None`` when it may exist."""
        probes: builtins.list[Callable[[], object]] = []
        if record.workspace_id is not None:
            workspace = record.workspace_id
            probes.append(lambda: self.client.tab_labels(workspace))
        if record.pane_id is not None:
            pane = record.pane_id
            probes.append(lambda: self.client.pane_info(pane))
        for probe in probes:
            try:
                probe()
            except HerdrUnavailable as exc:
                missing = _missing_target(exc)
                if missing is not None:
                    return missing, str(exc)
        return None

    def _archive_closed_foreign(
        self, record: AgentRecord, missing: str, detail: str,
    ) -> dict[str, object]:
        """Archive an adopted record whose runtime Herdr reports closed; nothing is closed.

        There is no pane left to capture or revalidate, so the reason is kept beside the
        record as ``stop.json``.
        """
        name = record.name
        archive, destination = self._archive_destination(record)
        agent._atomic_json(str(self._directory(name) / "stop.json"), {
            "source": missing, "detail": detail, "stopped_at": time.time(),
            "pane_id": record.pane_id, "workspace_id": record.workspace_id,
        })
        record.lifecycle = "stopped"
        self._save(record)
        os.rename(self._directory(name), destination)
        agent._fsync_dir(str(archive))
        agent._fsync_dir(str(self.registry))
        return {"name": name, "archive": str(destination),
                "pane_closed": False, "tab_closed": False, "source": missing}

    # Rename journals reserve both names until the rename is complete.

    def _renames_dir(self) -> Path:
        return self.registry / ".renames"

    def _rename_journals(self) -> builtins.list[dict[str, object]]:
        """Read every pending rename journal; an unreadable one refuses."""
        directory = self._renames_dir()
        if not directory.exists():
            return []
        agent._validate_private_directory(str(directory), "rename journal directory")
        journals: list[dict[str, object]] = []
        for path in sorted(directory.iterdir()):
            if path.name.startswith("."):
                continue  # an atomic writer's temporary file
            if not path.name.endswith(".json"):
                raise AgentDeliveryError(f"unexpected entry in rename journal directory: {path}")
            value = agent._read_queue_json(str(path), "rename journal", require_private=True)
            journals.append(_validate_rename_journal(value, path))
        return journals

    def _refuse_pending_rename(self, *names: str) -> None:
        from agentctl.revive import refuse_pending
        refuse_pending(self, names)
        for journal in self._rename_journals():
            if journal["old"] in names or journal["new"] in names:
                raise _with_recovery(AgentDeliveryError(
                    f"rename of {journal['old']!r} to {journal['new']!r} is incomplete; "
                    f"rerun `agentctl rename {journal['old']} {journal['new']}`"
                ), RecoveryAction(
                    "rename", name=cast(str, journal["old"]),
                    rename_to=cast(str, journal["new"]), token=cast(str, journal["token"]),
                ), stop_reason=f"rename of {journal['old']!r} to {journal['new']!r} is incomplete")

    def _write_rename_journal(self, journal: dict[str, object]) -> None:
        directory = self._renames_dir()
        if not directory.exists():
            directory.mkdir(mode=0o700)
            # Publish the new directory entry before any runtime change relies on it.
            agent._fsync_dir(str(self.registry))
        agent._validate_private_directory(str(directory), "rename journal directory")
        path = directory / f"{journal['token']}.json"
        if os.path.lexists(path):
            raise AgentDeliveryError(f"a rename journal already exists: {path}")
        agent._atomic_json(str(path), journal)

    def _remove_rename_journal(self, journal: dict[str, object]) -> None:
        directory = self._renames_dir()
        os.unlink(directory / f"{journal['token']}.json")
        agent._fsync_dir(str(directory))

    @contextmanager
    def _queue_locks(self, directory_name: str) -> Iterator[None]:
        """Hold one existing queue's delivery and binding locks, in drain's order.

        A queue is created by the first send, under the name lock the caller holds, so
        a missing queue has nothing to exclude; it is never created here.
        """
        queue = self._directory(directory_name) / "queue"
        if not queue.is_dir():
            yield
            return
        descriptors: list[int] = []
        try:
            for lock_name, purpose in ((".delivery.lock", "queue delivery lock"),
                                       (".binding.lock", "queue binding lock")):
                descriptor = agent._open_private_lock(str(queue / lock_name), purpose)
                descriptors.append(descriptor)
                fcntl.flock(descriptor, fcntl.LOCK_EX)
            yield
        finally:
            for descriptor in reversed(descriptors):
                os.close(descriptor)

    def anchor(self, name: str, *, replace: bool = False) -> dict[str, object]:
        """Pin the terminal and harness process an operator has confirmed for NAME."""
        with self._lock(name):
            with self._identity_transaction():
                record = self._load(name)
                migrated_from = None
                if record._nested_storage is not None:
                    # Anchors live only in flat schema-1 records, so the record is rewritten
                    # in that format. An asserted session must not become an observed one.
                    migrated_from = record._nested_storage.get("schema")
                    if record._session_source == "asserted":
                        record.goal_session_id = record.session_value or record.goal_session_id
                        record.session_agent = record.session_value = None
                    record._nested_storage = None
                    record._session_source = None
                if record.adapter not in ("herdr", "herdr-foreign"):
                    raise AgentDeliveryError(
                        "anchor applies to native and adopted Herdr agents; custom panes "
                        "pin their process at launch"
                    )
                if record.pane_id is None:
                    raise AgentDeliveryError(f"agent {name!r} has no confirmed pane")
                with self._pane_lock(record.pane_id):
                    info = self._checked(record)
                    harness = self.client.harness_identity(record.pane_id, record.harness)
                    if harness is None:
                        raise AgentDeliveryError(
                            f"cannot pin the {record.harness} process of pane {record.pane_id}: "
                            "an anchor needs the harness to be the pane's only foreground "
                            "process; wait until it is idle at its prompt and run anchor "
                            "again, or stop it and start a new agent"
                        )
                    changed = (record.terminal_id not in (None, info.terminal_id)
                               or record.harness_anchor not in (None, harness))
                    if changed and not replace:
                        raise AgentDeliveryError(
                            f"agent {name!r} is anchored to a different terminal or harness "
                            "process, so the pane may hold another program; inspect it, "
                            "then rerun with --replace"
                        )
                    owner = self._claim_owner(record.pane_id, info.terminal_id, exclude=name)
                    if owner is not None:
                        raise AgentDeliveryError(
                            f"pane {record.pane_id} is already registered as {owner!r}"
                        )
                    previous = {
                        "terminal_id": record.terminal_id,
                        "harness_pid": (None if record.harness_identity is None
                                        else record.harness_identity.pid),
                    }
                    record.terminal_id = info.terminal_id
                    record.harness_identity = harness
                    record.anchor_rule = ANCHOR_RULE
                    self._save(record)
                    return {
                        "name": name, "pane_id": record.pane_id,
                        "terminal_id": record.terminal_id, "harness_pid": harness.pid,
                        "replaced": changed, "previous": previous,
                        "migrated_from": migrated_from,
                    }

    def rename(self, old: str, new: str) -> dict[str, object]:
        """Rename a live agent: registry entry, Herdr agent name and tab label together."""
        _name(old)
        _name(new)
        if old == new:
            raise AgentDeliveryError("rename needs two different names")
        first, second = sorted((old, new))
        with self._lock(first), self._lock(second):
            with self._identity_transaction():
                pending = [journal for journal in self._rename_journals()
                           if {journal["old"], journal["new"]} & {old, new}]
                if pending:
                    found = pending[0]
                    if len(pending) != 1 or (found["old"], found["new"]) != (old, new):
                        raise AgentDeliveryError(
                            f"rename of {found['old']!r} to {found['new']!r} is incomplete; "
                            f"rerun exactly `agentctl rename {found['old']} {found['new']}`"
                        )
                    return self._finish_rename(found, recovered=True)
                record = self._load(old)
                if record._nested_storage is not None:
                    raise AgentDeliveryError(
                        f"agent {old!r} uses a nested record format; run `agentctl anchor {old}`, "
                        "which rewrites it as schema 1, then rename"
                    )
                if len(record.name_history) >= _MAX_NAME_HISTORY:
                    raise AgentDeliveryError(
                        f"agent {old!r} has been renamed {len(record.name_history)} times, the "
                        "most a record keeps; start a new agent instead"
                    )
                if record.adapter not in ("herdr", "herdr-foreign") or record.lifecycle != "running":
                    raise AgentDeliveryError(
                        "rename supports running native and adopted Herdr agents"
                    )
                if record.pane_id is None or record.tab_id is None:
                    raise AgentDeliveryError(f"agent {old!r} has no confirmed pane and tab")
                if record.harness_anchor is None and not (
                        record.session_value is not None and record._session_source != "asserted"):
                    raise AgentDeliveryError(
                        f"agent {old!r} pins no harness process or observed session; "
                        f"check its pane, run `agentctl anchor {old}`, then rename"
                    )
                if self._read_move_intent(record) is not None:
                    raise AgentDeliveryError(f"move of {old!r} is incomplete; rerun `agentctl move {old}`")
                if os.path.lexists(self._directory(new)):
                    raise AgentDeliveryError(f"agent {new!r} is already registered")
                journal: dict[str, object] = {
                    "schema": "agentctl-rename/v1", "token": record.token,
                    "old": old, "new": new, "adapter": record.adapter,
                    "pane_id": record.pane_id, "tab_id": record.tab_id,
                    "terminal_id": record.terminal_id, "workspace_id": record.workspace_id,
                    "journal_id": uuid.uuid4().hex, "started_at": time.time(),
                }
                with self._queue_locks(old), self._pane_lock(record.pane_id):
                    self._checked(record)
                    _WorkspaceClient(self.client, record).verify_recipient(record.pane_id)
                    owner = self._claim_owner(record.pane_id, record.terminal_id, exclude=old)
                    if owner is not None:
                        raise AgentDeliveryError(
                            f"pane {record.pane_id} is also claimed by registered agent "
                            f"{owner!r}; run `agentctl doctor`"
                        )
                    if new in self.client.agent_names():
                        raise AgentDeliveryError(f"a Herdr agent is already named {new!r}")
                    if (record.adapter == "herdr" and record.workspace_id is not None
                            and new in self.client.tab_labels(record.workspace_id).values()):
                        raise AgentDeliveryError(f"a tab in the workspace is already labelled {new!r}")
                    self._write_rename_journal(journal)
                    return self._complete_rename(journal, old, record, recovered=False)

    def _finish_rename(self, journal: dict[str, object], *, recovered: bool) -> dict[str, object]:
        """Complete a journalled rename after a crash, refusing any state it did not create."""
        old, new = str(journal["old"]), str(journal["new"])
        old_exists = os.path.lexists(self._directory(old))
        new_exists = os.path.lexists(self._directory(new))
        if old_exists == new_exists:
            raise AgentDeliveryError(
                f"rename journal for {old!r} -> {new!r} found "
                f"{'both' if old_exists else 'neither'} agent directories; refusing to guess"
            )
        current = old if old_exists else new
        path = self._directory(current) / "agent.json"
        agent._validate_private_directory(str(self._directory(current)), "agent directory")
        raw = agent._read_queue_json(str(path), "agent record", require_private=True)
        allowed = (old, new) if current == old else (new,)
        if (not isinstance(raw, dict) or raw.get("token") != journal["token"]
                or raw.get("name") not in allowed):
            raise AgentDeliveryError(
                f"agent record {path} does not match the rename journal; refusing to guess"
            )
        record = AgentRecord._from_value(raw, path, str(raw["name"]))
        with self._queue_locks(current), self._pane_lock(str(journal["pane_id"])):
            return self._complete_rename(journal, current, record, recovered=recovered)

    def _complete_rename(
        self, journal: dict[str, object], current: str, record: AgentRecord, *, recovered: bool,
    ) -> dict[str, object]:
        """Run the remaining rename steps; every step checks its own state first."""
        old, new = str(journal["old"]), str(journal["new"])
        pane_id, tab_id = str(journal["pane_id"]), str(journal["tab_id"])
        live = any(pane.pane_id == pane_id for pane in self.client.panes())
        herdr_steps = "skipped-pane-missing"
        verified = False
        if live:
            try:
                self._verify_rename_recipient(record, journal)
                verified = True
            except AgentDeliveryError as exc:
                # The harness exited or another program holds the pane: its name and
                # label are not ours to change, but the registry rename still completes.
                herdr_steps = f"skipped-recipient-changed: {exc}"
        if verified:
            if journal["adapter"] == "herdr":
                names = self.client.agent_names()
                if names.get(new) != pane_id:
                    if names.get(old) != pane_id:
                        raise AgentDeliveryError(
                            f"pane {pane_id} is named neither {old!r} nor {new!r} in Herdr; refusing"
                        )
                    self.client.rename_agent(pane_id, new)
                label = self.client.tab_label(tab_id)
                if label != new:
                    if label != old:
                        raise AgentDeliveryError(
                            f"tab {tab_id} is labelled {label!r}, neither {old!r} nor {new!r}; refusing"
                        )
                    self.client.rename_tab(tab_id, new)
            herdr_steps = "done"
        if current == old:
            record.name = new
            if not any(entry["journal_id"] == journal["journal_id"] for entry in record.name_history):
                # A journal from before the history limit may find it full: keep the newest
                # names rather than publish a record no edition can read.
                evicted = record.name_history[:max(0, len(record.name_history) - _MAX_NAME_HISTORY + 1)]
                del record.name_history[:len(evicted)]
                for entry in evicted:
                    if entry["name"] not in record.former_names:
                        record.former_names.append(str(entry["name"]))
                record.name_history.append({
                    "name": old, "renamed_at": journal["started_at"],
                    "journal_id": journal["journal_id"],
                })
            agent._atomic_json(str(self._directory(old) / "agent.json"), record.to_document())
            registry = os.open(
                self.registry, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0),
            )
            try:
                _rename_directory_noreplace_at(registry, old, registry, new)
                os.fsync(registry)
            finally:
                os.close(registry)
        elif not any(entry["journal_id"] == journal["journal_id"] for entry in record.name_history):
            raise AgentDeliveryError(
                f"agent {new!r} was moved without its rename history; refusing to guess"
            )
        self._remove_rename_journal(journal)
        return {
            "name": new, "previous_name": old, "token": record.token, "pane_id": pane_id,
            "recovered": recovered, "herdr_steps": herdr_steps,
            "external_references": (
                f"wrkslots slots, chat bindings and scheduled prompts that name {old!r} "
                "are not changed by agentctl"
            ),
        }

    def _verify_rename_recipient(self, record: AgentRecord, journal: dict[str, object]) -> None:
        """Every non-presentation anchor must hold; only the name and label may be OLD or NEW."""
        pane_id = str(journal["pane_id"])
        info = self.client.pane_info(pane_id)
        failures: list[str] = []
        if info.workspace_id != record.workspace_id:
            failures.append(f"workspace is {info.workspace_id!r}, recorded {record.workspace_id!r}")
        if os.path.realpath(info.cwd) != os.path.realpath(record.cwd):
            failures.append(f"cwd is {info.cwd!r}, recorded {record.cwd!r}")
        if info.agent != record.harness:
            failures.append(f"harness is {info.agent!r}, recorded {record.harness!r}")
        if journal["terminal_id"] is not None and info.terminal_id != journal["terminal_id"]:
            failures.append(f"terminal is {info.terminal_id!r}, recorded {journal['terminal_id']!r}")
        if info.tab_id is not None and info.tab_id != journal["tab_id"]:
            failures.append(f"tab is {info.tab_id!r}, recorded {journal['tab_id']!r}")
        if not _session_matches(record, info):
            failures.append("the observed native session changed")
        if record.harness_anchor is not None:
            if not self.client.verify_harness_identity(pane_id, record.harness_anchor):
                failures.append("the anchored harness process is no longer in the foreground")
        elif record.session_value is None:
            failures.append("no anchored harness process or observed session")
        if failures:
            raise AgentDeliveryError(
                f"pane {pane_id} no longer holds this agent: " + "; ".join(failures)
            )

    def doctor(self, *, repair_labels: bool = False) -> dict[str, object]:
        """Compare every registry record with live Herdr state; read-only unless repairing labels."""
        rows: list[dict[str, object]] = []
        registry_findings: list[dict[str, object]] = []
        try:
            journals = self._rename_journals()
        except AgentDeliveryError as exc:
            journals = []
            registry_findings.append({"finding": "rename-journal-unreadable", "detail": str(exc)})
        journal_names = {str(name) for journal in journals for name in (journal["old"], journal["new"])}
        records: list[AgentRecord] = []
        if self.registry.exists():
            for path in sorted(self.registry.iterdir()):
                if not _NAME.fullmatch(path.name) or path.name == "archive":
                    continue
                try:
                    records.append(AgentRecord.load(path / "agent.json", path.name))
                except AgentDeliveryError as exc:
                    if path.name in journal_names:
                        rows.append({"name": path.name, "findings": ["rename-incomplete"],
                                     "detail": str(exc)})
                    else:
                        rows.append({"name": path.name, "findings": ["record-unreadable"],
                                     "detail": str(exc)})
        panes = {pane.pane_id: pane for pane in self.client.panes()}
        agent_names = self.client.agent_names()
        workspaces = sorted({record.workspace_id for record in records if record.workspace_id})
        labels: dict[str, str] = {}
        missing_workspaces: set[str] = set()
        for workspace in workspaces:
            try:
                labels.update(self.client.tab_labels(workspace))
            except HerdrUnavailable as exc:
                if _missing_target(exc) != "workspace-missing":
                    raise
                missing_workspaces.add(workspace)
        claims: dict[str, list[str]] = {}
        for record in records:
            for key in (record.pane_id, record.terminal_id):
                if key is not None:
                    claims.setdefault(key, []).append(record.name)
        for record in records:
            if record.workspace_id in missing_workspaces:
                findings = ["workspace-missing"]
            else:
                try:
                    findings = self._doctor_findings(record, panes, agent_names, labels)
                except HerdrUnavailable as exc:
                    # Closed between the pane list and this record's checks.
                    missing = _missing_target(exc)
                    if missing is None:
                        raise
                    findings = [missing]
            if record.name in journal_names:
                findings.append("rename-incomplete")
            if any(len(claims.get(key, [])) > 1 for key in (record.pane_id, record.terminal_id)
                   if key is not None):
                findings.append("duplicate-claim")
            row: dict[str, object] = {"name": record.name, "adapter": record.adapter,
                                      "pane_id": record.pane_id, "findings": findings}
            if (repair_labels and findings
                    and set(findings) <= {"label-mismatch", "herdr-name-mismatch"}
                    and record.adapter == "herdr" and not journals):
                row["repaired"] = self._repair_presentation(record.name)
            rows.append(row)
        registered = {record.name for record in records}
        claimed_tabs = {record.tab_id for record in records}
        workspace_findings: list[dict[str, object]] = []
        for tab_id, label in sorted(labels.items()):
            if tab_id in claimed_tabs:
                owner = next(record for record in records if record.tab_id == tab_id)
                if label in registered and label != owner.name:
                    workspace_findings.append({"finding": "label-collision", "tab_id": tab_id,
                                               "label": label})
            elif label in registered:
                workspace_findings.append({"finding": "label-collision", "tab_id": tab_id,
                                           "label": label})
            else:
                workspace_findings.append({"finding": "unmanaged-tab", "tab_id": tab_id,
                                           "label": label})
        clean = (not registry_findings
                 and all(not row["findings"] or row.get("repaired") is True for row in rows)
                 and all(item["finding"] == "unmanaged-tab" for item in workspace_findings))
        return {"clean": clean, "records": rows, "registry": registry_findings,
                "workspace": workspace_findings,
                "journals": [{"old": journal["old"], "new": journal["new"]} for journal in journals]}

    def _doctor_findings(
        self, record: AgentRecord, panes: dict[str, Pane], agent_names: dict[str, str],
        labels: dict[str, str],
    ) -> builtins.list[str]:
        findings: list[str] = []
        if record.pane_id is None or record.pane_id not in panes:
            return ["pane-missing"]
        info = self.client.pane_info(record.pane_id)
        if info.workspace_id != record.workspace_id:
            findings.append("workspace-mismatch")
        if os.path.realpath(info.cwd) != os.path.realpath(record.cwd):
            findings.append("cwd-mismatch")
        if record.terminal_id is not None and info.terminal_id != record.terminal_id:
            findings.append("terminal-mismatch")
        if record.adapter != "herdr-foreign":
            if record.tab_id is not None and panes[record.pane_id].tab_id != record.tab_id:
                findings.append("tab-moved")
            if record.tab_id is not None and labels.get(record.tab_id) != record.name:
                findings.append("label-mismatch")
        if record.adapter == "herdr" and agent_names.get(record.name) != record.pane_id:
            findings.append("herdr-name-mismatch")
        if not _session_matches(record, info):
            findings.append("session-mismatch")
        if info.agent is None:
            findings.append("harness-exited")
        elif info.agent != record.harness:
            findings.append("harness-kind-mismatch")
        if record.harness_anchor is not None:
            if not self.client.verify_harness_identity(record.pane_id, record.harness_anchor):
                findings.append("harness-replaced")
        elif record.adapter in ("herdr", "herdr-foreign") and not (
                record.session_value is not None and record._session_source != "asserted"):
            findings.append("unanchored")
        return findings

    def _repair_presentation(self, name: str) -> bool:
        """Restore one owned agent's label and Herdr name when every other anchor holds."""
        with self._lock(name):
            with self._identity_transaction():
                record = self._load(name)
                if record.pane_id is None or record.tab_id is None:
                    return False
                with self._queue_locks(name), self._pane_lock(record.pane_id):
                    guard = _WorkspaceClient(self.client, record, claims=self._peer_claims(name))
                    try:
                        guard.verify_recipient(record.pane_id, presentation=False)
                        if self.client.pane_info(record.pane_id).agent != record.harness:
                            return False
                    except (HerdrUnavailable, AgentDeliveryError):
                        return False
                    names = self.client.agent_names()
                    if names.get(name) not in (None, record.pane_id):
                        return False
                    if record.workspace_id is not None and any(
                            label == name and tab != record.tab_id
                            for tab, label in self.client.tab_labels(record.workspace_id).items()):
                        return False
                    if names.get(name) != record.pane_id:
                        self.client.rename_agent(record.pane_id, name)
                    if self.client.tab_label(record.tab_id) != name:
                        self.client.rename_tab(record.tab_id, name)
                    guard.verify_recipient(record.pane_id)
                    return True

    @staticmethod
    def _require_automation(record: AgentRecord) -> None:
        if record.paused:
            raise AgentDeliveryError(f"agent {record.name!r} is paused for human input; resume it before automated submission")

    def pause(self, name: str, *, paused: bool = True) -> dict[str, object]:
        """Pause automated input without interrupting an active harness turn."""
        with self._lock(name):
            record = self._load(name)
            record.paused = paused
            self._save(record)
            return {"name": name, "token": record.token, "paused": paused}

    def attach(self, name: str, *, expected_token: str | None = None) -> dict[str, object]:
        """Focus the verified native terminal; input ownership changes separately."""
        with self._lock(name):
            record = self._load_expected(name, expected_token)
            info = self._checked(record)
            self.client.focus_pane(info.pane_id)
            return {"name": name, "pane_id": info.pane_id, "paused": record.paused}


SLOT_ISOLATIONS = ("userns", "cgroup", "root")
#: Harnesses agentctl can drive behind a root slot relay: Herdr has screen rules
#: for them and agentctl has verified composer models.
RELAY_HARNESSES = ("claude", "codex")
#: Both relayed harnesses show this hint only while a turn is running.
RELAY_WORKING_MARKER = "esc to interrupt"


def _slot_shell_command(
    slot: str, *, isolation: str | None, project: str, explicit_project: bool = False,
    command: Sequence[str] = (),
) -> tuple[str, str, str]:
    """Ask the slot manager for the exec-only line that boxes a pane shell (or ``command``).

    Returns the line, the slot directory, and the effective isolation.
    """
    if isolation is not None and isolation not in SLOT_ISOLATIONS:
        raise AgentDeliveryError("--slot-isolation must be userns, cgroup, or root")
    executable = os.environ.get("AGENTCTL_WRKSLOTS_BIN") or shutil.which("wrkslots")
    if not executable:
        raise AgentDeliveryError(
            "--slot needs wrkslots on PATH (or AGENTCTL_WRKSLOTS_BIN naming it)"
        )
    argv = [executable, "shell-command", slot, "--format", "json"]
    if isolation is not None:
        argv[3:3] = ["--isolation", isolation]
    if explicit_project:
        argv[1:1] = ["--project-root", project]
    if command:
        argv += ["--", *command]
    completed = subprocess.run(
        argv, cwd=project, capture_output=True, text=True, timeout=60, check=False,
    )
    if completed.returncode != 0 or not completed.stdout.strip():
        detail = (completed.stderr or completed.stdout).strip() or f"exit {completed.returncode}"
        raise AgentDeliveryError(f"wrkslots shell-command {slot!r} failed: {detail}")
    try:
        document: object = json.loads(completed.stdout)
        if not isinstance(document, dict):
            raise ValueError("not an object")
        line, slot_path = document["command"], document["slot_path"]
        if not isinstance(line, str) or not isinstance(slot_path, str) or not line:
            raise ValueError("missing fields")
    except (ValueError, KeyError) as exc:
        raise AgentDeliveryError(f"wrkslots shell-command {slot!r} returned invalid JSON: {exc}") from exc
    effective = document.get("isolation", isolation or "userns")
    if effective not in SLOT_ISOLATIONS:
        raise AgentDeliveryError(
            f"wrkslots shell-command {slot!r} returned invalid JSON: unknown isolation {effective!r}"
        )
    return line, slot_path, str(effective)
