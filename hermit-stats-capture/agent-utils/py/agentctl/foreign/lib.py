#!/usr/bin/env python3
"""Portable persistent-worker registry, queues, and presentation lifecycle.

State lives below HERDR_SUBAGENTS_HOME, independently of the installed package.
The runner PID and its start time identify a headless worker; presentation can
be repaired without discarding its durable conversation. Registry updates are
serialized by flock. Consumer scheduling and quota rules enter only through
an explicitly configured policy; they are not discovered from the filesystem.
"""

from __future__ import annotations

import contextlib
import dataclasses
import datetime
import fcntl
import json
import os
import re
import secrets
import signal
import shlex
import shutil
import subprocess
import sys
import time
import uuid
from collections.abc import Sequence
from pathlib import Path
from typing import Iterator, NoReturn, Optional, cast

TMUX_SESSION: str = os.environ.get("SUBAGENTS_TMUX_SESSION", "subagents")
PACKAGE: Path = Path(__file__).resolve().parent
BASE: Path = Path(os.environ.get(
    "HERDR_SUBAGENTS_HOME",
    str(Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state"))) / "herdr-agent/foreign"),
)).expanduser().resolve()
from agentctl.agent import Target as SharedAgentTarget  # noqa: E402
from agentctl.agent import drain as shared_agent_drain  # noqa: E402
from agentctl.agent import read as shared_agent_read  # noqa: E402
from agentctl.agent import send as shared_agent_send  # noqa: E402
from agentctl.agent import status as shared_agent_status  # noqa: E402
from agentctl.client import HerdrClient as SharedHerdrClient  # noqa: E402
from agentctl.errors import HerdrRunError as SharedHerdrError  # noqa: E402
from agentctl.procstat import parse_process_stat  # noqa: E402

STATE: Path = BASE / "state"
ARCHIVE: Path = STATE / "_archive"
REGISTRY: Path = BASE / "registry.json"
LOCKFILE: Path = BASE / ".registry.lock"
RUNNER: Path = PACKAGE / "agent_runner.py"
EVENT_LOG: Path = STATE / "events.jsonl"
EVENT_LOCKFILE: Path = STATE / ".events.lock"
BACKEND_CONFIG: Path = BASE / "backend.json"
# Unlike backend.json, this file is tracked with the project.  It gives a
# checkout a reproducible default without overriding an individual agent or a
# coordinator's environment.
PROJECT_DEFAULTS_CONFIG: Path = Path(os.environ.get("HERDR_SUBAGENTS_PROJECT_DEFAULTS", str(BASE / "project_defaults.json"))).expanduser().resolve()
CODEX_SESSIONS: Path = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "sessions"

DEFAULT_HARNESS: str = "codex"
SUPPORTED_HARNESSES: tuple[str, ...] = ("codex", "agy", "muse")
SUPPORTED_BACKENDS: tuple[str, ...] = ("tmux", "herdr")
HEADLESS_MODE: str = "headless"
TUI_MODE: str = "tui"
SUPPORTED_MODES: tuple[str, ...] = (HEADLESS_MODE, TUI_MODE)
CODEX_BIN: str = os.environ.get("CODEX_BIN", "codex")
AGY_BIN: str = os.environ.get("AGY_BIN", "agy")
MUSE_BIN: str = os.environ.get("MUSE_BIN", "muse")
HERDR_BIN: str = os.environ.get("HERDR_BIN", "herdr")
HERDR_WORKSPACE_LABEL: str = os.environ.get("SUBAGENTS_HERDR_WORKSPACE", "subagents")
HERDR_API_TIMEOUT_S: float = float(os.environ.get("SUBAGENTS_HERDR_TIMEOUT", "2"))
# TUI probes are used by status and gc, so each Herdr request must stay short.
HERDR_TUI_PROBE_TIMEOUT_S: float = min(HERDR_API_TIMEOUT_S, 1.0)
# A coordinator send may wait for an interactive Codex turn to reach its prompt,
# but it leaves the message queued and fails loudly if that bounded wait expires.
TUI_DELIVERY_WAIT_S: float = float(os.environ.get("SUBAGENTS_TUI_DELIVERY_WAIT", "900"))
TUI_READ_LINES: int = int(os.environ.get("SUBAGENTS_TUI_READ_LINES", "500"))
# the interactive TUI backend has no durable turn-boundary capture, so
# it can only ever honor a plain, bounded scrollback read -- it cannot identify
# "the last answer" (mode="last") or "everything since a given turn"
# (since_turn=N) the way the headless backend's transcript-based read can.
# read_agent_output() refuses those modes for a TUI agent with a precise error
# instead of silently substituting arbitrary recent scrollback (the bug this
# constant's caller fixes). This caps the one mode it DOES support (an
# explicit `tail` line count) so an unbounded value can't be passed straight
# through to Herdr.
TUI_MAX_TAIL_LINES: int = int(os.environ.get("SUBAGENTS_TUI_MAX_TAIL_LINES", "5000"))
TUI_SUBMIT_WORKING_TIMEOUT_MS: int = int(
    os.environ.get("SUBAGENTS_TUI_SUBMIT_WORKING_TIMEOUT_MS", "5000")
)
TUI_DELIVERY_MAX_ATTEMPTS: int = int(os.environ.get("SUBAGENTS_TUI_DELIVERY_MAX_ATTEMPTS", "2"))
# Herdr's API key grammar is lower-case. Keeping these protocol spellings in
# one place prevents a rejected key from looking like a generic delivery
# timeout.
HERDR_KEY_ENTER: str = "enter"
TUI_MIGRATION_READY_TIMEOUT_S: float = float(
    os.environ.get("SUBAGENTS_TUI_MIGRATION_READY_TIMEOUT", "300")
)
TUI_MIGRATION_CONFIRM_TIMEOUT_S: float = float(
    os.environ.get("SUBAGENTS_TUI_MIGRATION_CONFIRM_TIMEOUT", "300")
)
# A large historic session can take minutes to rebuild its interactive state.
# Record bounded progress snapshots while migration waits so an eventual timeout
# distinguishes slow reconstruction from a prompt that needs a new handler.
TUI_MIGRATION_PROGRESS_INTERVAL_S: float = float(
    os.environ.get("SUBAGENTS_TUI_MIGRATION_PROGRESS_INTERVAL", "15")
)
# Codex asks this only when a resumed session's latest working directory differs
# from the new TUI pane's directory. The default selection is the session
# directory, which preserves the session's original context.
TUI_RESUME_DIRECTORY_PROMPT: str = "Choose working directory to resume this session"
TUI_RESUME_MODEL_PROMPT: str = "This session was recorded with model "
# Per-turn wall-clock cap; a hung turn is killed and marked rc=timeout so the
# runner survives and the agent can be re-prompted.
TURN_TIMEOUT_S: int = int(os.environ.get("SUBAGENTS_TURN_TIMEOUT", "3600"))
MIGRATION_READY_TIMEOUT_S: float = float(os.environ.get("SUBAGENTS_MIGRATION_READY_TIMEOUT", "10"))
# A brand-new agent whose runner has not yet recorded its pid is not reaped by
# gc for this long, so `up` immediately followed by `status` cannot self-reap.
STARTUP_GRACE_S: int = 20

_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
_HERDR_TAB_ID_RE = re.compile(r"^w[^:]+:t[0-9]+$")


class AgentOperationError(Exception):
    """Structured error for callers that need JSON-friendly failures."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _configured_codex_bypass_permissions() -> bool:
    value = os.environ.get("SUBAGENTS_CODEX_BYPASS_PERMISSIONS", "0")
    if value not in {"0", "1"}:
        raise AgentOperationError(
            "invalid_permission_policy",
            "SUBAGENTS_CODEX_BYPASS_PERMISSIONS must be exactly 0 (native permissions) or 1 (bypass approvals and sandbox)",
        )
    return value == "1"


def die(msg: str, code: int = 1) -> NoReturn:
    """Print an actionable error to stderr and exit non-zero."""
    print(f"error: {msg}", file=sys.stderr)
    raise SystemExit(code)


def now_iso() -> str:
    """Return a timezone-aware timestamp for registry and transcript records."""
    return datetime.datetime.now().astimezone().replace(microsecond=0).isoformat()


def validate_name(name: str) -> str:
    """Validate a worker name for filesystem use, exiting with a CLI diagnostic on failure."""
    if not _NAME_RE.match(name):
        die(
            f"invalid agent name {name!r}: use letters, digits, '-' or '_' "
            "(<=64 chars, must start alphanumeric)"
        )
    return name


def require_valid_name(name: str) -> str:
    """Validate an agent name for structured API callers."""
    if not _NAME_RE.match(name):
        raise AgentOperationError(
            "invalid_name",
            f"invalid agent name {name!r}: use letters, digits, '-' or '_' "
            "(<=64 chars, must start alphanumeric)",
        )
    return name


def require_supported_harness(harness: str) -> str:
    """Return a supported harness identifier or raise a structured operation error."""
    if harness not in SUPPORTED_HARNESSES:
        raise AgentOperationError(
            "unsupported_harness",
            f"unsupported harness {harness!r}; supported harnesses: {', '.join(SUPPORTED_HARNESSES)}",
        )
    return harness


def require_harness_quota(harness: str, model: str | None, purpose: str = "development") -> None:
    """Run the explicitly configured consumer launch policy, if any."""
    from .policy import configured_policy
    configured_policy().check_launch(harness, model, purpose)


def require_supported_backend(backend: str) -> str:
    """Return a supported presentation backend or raise a structured operation error."""
    if backend not in SUPPORTED_BACKENDS:
        raise AgentOperationError(
            "unsupported_backend",
            f"unsupported backend {backend!r}; supported backends: {', '.join(SUPPORTED_BACKENDS)}",
        )
    return backend


def require_supported_mode(mode: str) -> str:
    """Return a supported worker mode or raise a structured operation error."""
    if mode not in SUPPORTED_MODES:
        raise AgentOperationError(
            "unsupported_mode",
            f"unsupported agent mode {mode!r}; supported modes: {', '.join(SUPPORTED_MODES)}",
        )
    return mode


def _load_backend_config() -> dict[str, object]:
    if not BACKEND_CONFIG.exists():
        return {}
    try:
        raw = json.loads(BACKEND_CONFIG.read_text())
    except json.JSONDecodeError as exc:
        raise AgentOperationError(
            "backend_config_invalid",
            f"backend config {BACKEND_CONFIG} is not valid JSON: {exc}",
        ) from exc
    if not isinstance(raw, dict):
        raise AgentOperationError(
            "backend_config_invalid",
            f"backend config {BACKEND_CONFIG} must contain a JSON object",
        )
    return raw


def _save_backend_config(config: dict[str, object]) -> None:
    BASE.mkdir(parents=True, exist_ok=True)
    tmp = BACKEND_CONFIG.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(config, indent=2, sort_keys=True) + "\n")
    os.replace(tmp, BACKEND_CONFIG)


def _load_project_defaults() -> dict[str, object]:
    """Load the versioned, checkout-wide subagent defaults."""
    try:
        raw = json.loads(PROJECT_DEFAULTS_CONFIG.read_text())
    except FileNotFoundError:
        return {}
    except json.JSONDecodeError as exc:
        raise AgentOperationError(
            "project_defaults_invalid",
            f"project defaults {PROJECT_DEFAULTS_CONFIG} is not valid JSON: {exc}",
        ) from exc
    if not isinstance(raw, dict):
        raise AgentOperationError(
            "project_defaults_invalid",
            f"project defaults {PROJECT_DEFAULTS_CONFIG} must contain a JSON object",
        )
    return raw


def herdr_available() -> bool:
    """True only when the configured Herdr socket answers a status request."""
    if not Path(HERDR_BIN).is_file() and shutil.which(HERDR_BIN) is None:
        return False
    try:
        result = subprocess.run(
            [HERDR_BIN, "status"], capture_output=True, text=True, timeout=3
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0 and "status: running" in result.stdout


def auto_detect_backend() -> str:
    """Use configured consumer policy, then a live Herdr environment, otherwise tmux."""
    from .policy import configured_policy
    configured = configured_policy().backend()
    if configured is not None:
        return configured
    socket = os.environ.get("HERDR_SOCKET_PATH")
    if os.environ.get("HERDR_ENV") and socket and Path(socket).exists() and herdr_available():
        return "herdr"
    return "tmux"


def selected_backend(explicit: Optional[str] = None) -> str:
    """Resolve backend with call argument, environment, config, then detection.

    Auto-detection first honors an explicitly installed consumer policy,
    then probes Herdr and finally defaults to tmux.
    """
    if explicit is not None:
        return require_supported_backend(explicit)
    env_backend = os.environ.get("SUBAGENTS_BACKEND")
    if env_backend is not None:
        return require_supported_backend(env_backend)
    configured = _load_backend_config().get("backend")
    if configured is not None:
        if not isinstance(configured, str):
            raise AgentOperationError(
                "backend_config_invalid",
                f"backend config {BACKEND_CONFIG} has a non-string backend",
            )
        return require_supported_backend(configured)
    return auto_detect_backend()


def selected_mode(
    harness: str, explicit: Optional[str] = None, *, backend: Optional[str] = None
) -> str:
    """Resolve mode: per-agent argument, environment, project, then headless.

    Project defaults are keyed by harness so enabling Codex's current TUI does
    not silently select an unimplemented terminal UI for another harness.
    """
    harness = require_supported_harness(harness)
    source = "explicit argument"
    if explicit is not None:
        return require_supported_mode(explicit)
    env_mode = os.environ.get("SUBAGENTS_MODE")
    if env_mode is not None:
        mode = require_supported_mode(env_mode)
        source = "SUBAGENTS_MODE"
    else:
        configured_modes = _load_project_defaults().get("harness_modes")
        if configured_modes is None:
            return HEADLESS_MODE
        if not isinstance(configured_modes, dict):
            raise AgentOperationError(
                "project_defaults_invalid",
                f"project defaults {PROJECT_DEFAULTS_CONFIG} has a non-object harness_modes",
            )
        configured = configured_modes.get(harness)
        if configured is None:
            return HEADLESS_MODE
        if not isinstance(configured, str):
            raise AgentOperationError(
                "project_defaults_invalid",
                f"project defaults {PROJECT_DEFAULTS_CONFIG} has a non-string mode for {harness!r}",
            )
        mode = require_supported_mode(configured)
        source = f"project default for {harness}"
    resolved_backend = selected_backend(backend)
    if mode == TUI_MODE and resolved_backend != "herdr":
        print(
            f"warning: {source} selected mode {TUI_MODE!r}, but backend {resolved_backend!r} "
            f"does not support it; using {HEADLESS_MODE!r}",
            file=sys.stderr,
        )
        return HEADLESS_MODE
    return mode


def set_default_backend(backend: str) -> str:
    """Persist the chosen backend for subsequent commands using this state directory."""
    backend = require_supported_backend(backend)
    config = _load_backend_config()
    config["backend"] = backend
    _save_backend_config(config)
    return backend


def _antigravity_settings_path() -> Path:
    return Path.home() / ".gemini" / "antigravity-cli" / "settings.json"


def require_antigravity_preflight(cwd: Path) -> None:
    """Require unattended Antigravity settings before launching an agy worker."""
    settings_path = _antigravity_settings_path()
    instruction = (
        f"Fix: edit {settings_path} so toolPermission is exactly "
        f'"always-proceed" and trustedWorkspaces contains "{cwd}". '
        "Do not use --dangerously-skip-permissions for agy subagent turns."
    )
    if not settings_path.is_file():
        raise AgentOperationError(
            "agy_preflight_failed",
            f"Antigravity settings file not found at {settings_path}. {instruction}",
        )
    try:
        raw = json.loads(settings_path.read_text())
    except json.JSONDecodeError as exc:
        raise AgentOperationError(
            "agy_preflight_failed",
            f"Antigravity settings file is not valid JSON ({exc}). {instruction}",
        ) from exc
    if not isinstance(raw, dict):
        raise AgentOperationError(
            "agy_preflight_failed",
            f"Antigravity settings file must contain a JSON object. {instruction}",
        )
    if raw.get("toolPermission") != "always-proceed":
        raise AgentOperationError(
            "agy_preflight_failed",
            f'Antigravity toolPermission is not "always-proceed". {instruction}',
        )
    workspaces = raw.get("trustedWorkspaces")
    if not isinstance(workspaces, list) or not all(isinstance(item, str) for item in workspaces):
        raise AgentOperationError(
            "agy_preflight_failed",
            f"Antigravity trustedWorkspaces must be a list of strings. {instruction}",
        )
    trusted = {str(Path(item).expanduser().resolve()) for item in workspaces}
    if str(cwd) not in trusted:
        raise AgentOperationError(
            "agy_preflight_failed",
            f"Antigravity cwd is not trusted. {instruction}",
        )


@dataclasses.dataclass
class AgentRecord:
    """One row of the registry (one live agent)."""

    name: str
    harness: str
    backend: str
    # Kept for registry/API compatibility. It is a tmux target for the tmux
    # backend and a Herdr tab id for the Herdr backend.
    tmux_target: str
    cwd: str
    model: Optional[str]
    session_id: Optional[str]
    status: str  # starting | idle | busy | error | quota_exhausted | auth_error
    runner_pid: Optional[int]
    runner_started_at: Optional[str]
    next_seq: int
    created_at: str
    last_turn_at: Optional[str]
    # Missing on V1/V2 rows: those always hosted the headless turn runner.
    mode: str = HEADLESS_MODE
    # Herdr pane that owns an interactive TUI. Headless rows intentionally omit it.
    presentation_pane: Optional[str] = None
    # New workers use native permissions unless the caller explicitly opts in.
    codex_bypass_permissions: bool = False
    # Literal non-secret Muse options. Profiles reject credential-shaped
    # arguments before these durable launch settings are recorded.
    harness_args: tuple[str, ...] = ()

    @staticmethod
    def from_dict(d: dict[str, object]) -> "AgentRecord":
        """Load a worker record, restoring omitted presentation fields from preserved identity metadata."""
        fields = {f.name for f in dataclasses.fields(AgentRecord)}
        missing = fields - set(d) - {"runner_started_at", "backend", "mode", "presentation_pane", "codex_bypass_permissions", "harness_args"}
        if missing:
            die(f"registry row for {d.get('name')!r} missing keys: {sorted(missing)}")
        preserved = _load_presentation_identity(d)
        raw_backend = d.get("backend")
        raw_mode = d.get("mode")
        raw_pane = d.get("presentation_pane")
        backend = str(
            raw_backend
            or (preserved.backend if preserved is not None else "")
            or ("herdr" if _HERDR_TAB_ID_RE.match(str(d["tmux_target"])) else "tmux")
        )
        mode = require_supported_mode(
            str(raw_mode or (preserved.mode if preserved is not None else HEADLESS_MODE))
        )
        presentation_pane = (
            str(raw_pane)
            if raw_pane is not None
            else (preserved.presentation_pane if preserved is not None and mode == TUI_MODE else None)
        )
        bypass = _load_codex_permission_policy(d)
        raw_harness_args = d.get("harness_args", [])
        if not isinstance(raw_harness_args, list) or any(
            not isinstance(item, str) or not item or "\0" in item for item in raw_harness_args
        ):
            die("registry row has invalid harness_args")
        return AgentRecord(
            name=str(d["name"]),
            harness=str(d["harness"]),
            backend=backend,
            tmux_target=str(d["tmux_target"]),
            cwd=str(d["cwd"]),
            model=(None if d["model"] is None else str(d["model"])),
            session_id=(None if d["session_id"] is None else str(d["session_id"])),
            status=str(d["status"]),
            runner_pid=(None if d["runner_pid"] is None else int(str(d["runner_pid"]))),
            runner_started_at=(
                None if d.get("runner_started_at") is None else str(d["runner_started_at"])
            ),
            next_seq=int(str(d["next_seq"])),
            created_at=str(d["created_at"]),
            last_turn_at=(None if d["last_turn_at"] is None else str(d["last_turn_at"])),
            mode=mode,
            presentation_pane=presentation_pane,
            codex_bypass_permissions=bypass,
            harness_args=tuple(raw_harness_args),
        )

    def to_dict(self) -> dict[str, object]:
        """Serialize this record into a JSON-compatible dictionary."""
        return dataclasses.asdict(self)


@dataclasses.dataclass
class AgentStatus:
    """A worker snapshot combining registry identity, process health, queue counts, and transcript paths."""
    name: str
    harness: str
    backend: str
    tmux_target: str
    cwd: str
    model: Optional[str]
    session_id: Optional[str]
    status: str
    runner_pid: Optional[int]
    runner_started_at: Optional[str]
    next_seq: int
    created_at: str
    last_turn_at: Optional[str]
    window_alive: bool
    runner_alive: bool
    presentation_degraded: bool
    pending: int
    transcript: str
    last_message_preview: str
    mode: str = HEADLESS_MODE
    presentation_pane: Optional[str] = None
    automation_paused: bool = False

    def to_dict(self) -> dict[str, object]:
        """Serialize this record into a JSON-compatible dictionary."""
        return dataclasses.asdict(self)


@dataclasses.dataclass
class UpResult:
    """New worker status, the optional first-turn sequence, and cleanup notes."""
    agent: AgentStatus
    queued_turn: Optional[int]
    gc_notes: list[str]


@dataclasses.dataclass
class SendResult:
    """Queued turn identity, transcript location, and any quarantined delivery attempts."""
    name: str
    seq: int
    model: Optional[str]
    transcript: str
    mode: str = HEADLESS_MODE
    presentation_pane: Optional[str] = None
    quarantined: list[int] = dataclasses.field(default_factory=list)


@dataclasses.dataclass
class TuiInboxResult:
    """Visible outcome of inspecting or draining one interactive TUI inbox."""

    name: str
    pending: list[int]
    quarantined: list[int]
    failed_dir: str


@dataclasses.dataclass
class ResetResult:
    """Result of clearing a live agent's harness conversation context."""

    name: str
    previous_session_id: Optional[str]
    transcript: str


@dataclasses.dataclass
class ReadResult:
    """Worker output with its source path and requested read mode."""
    name: str
    mode: str
    text: str


@dataclasses.dataclass
class DownResult:
    """Worker retirement outcome, including presentation cleanup and preserved archive location."""
    name: str
    killed_window: bool
    archived_to: Optional[str]
    state_path: Optional[str]
    was_registered: bool
    forced: bool = False
    unverified_presentation: Optional[str] = None


@dataclasses.dataclass
class StatusResult:
    """A collection of worker status snapshots and garbage-collection notes."""
    agents: list[AgentStatus]
    gc_notes: list[str]


@dataclasses.dataclass
class RecreateWindowResult:
    """Recovered presentation identity and whether the persistent runner was restarted."""
    name: str
    mode: str
    backend: str
    tmux_target: str
    pane_id: Optional[str]
    window_alive: bool


@dataclasses.dataclass
class MigrationResult:
    """Source and destination presentation details for a completed worker migration."""
    name: str
    from_backend: str
    to_backend: str
    from_mode: str
    to_mode: str
    tmux_target: str
    session_id: Optional[str]
    transcript: str


@dataclasses.dataclass(frozen=True)
class RunnerIdentity:
    """A process ID paired with its kernel start time to distinguish PID reuse."""
    pid: int
    started_at: Optional[str]


@dataclasses.dataclass(frozen=True)
class PresentationIdentity:
    """TUI presentation data that survives old registry writers.

    Older MCP servers rewrite a complete registry row from their older schema,
    dropping fields they do not know. A TUI has no headless runner to recover
    its mode or Herdr pane from, so keep the minimal presentation identity in
    the agent's durable state directory as well. ``created_at`` and tab id
    prevent a stale file from being applied to a later reuse of the same name.
    """

    backend: str
    mode: str
    presentation_pane: str


def presentation_identity_path(name: str) -> Path:
    """Return the durable sidecar path used to recover a worker presentation identity."""
    return STATE / name / "presentation.json"


def _load_presentation_identity(row: dict[str, object]) -> Optional[PresentationIdentity]:
    """Return a matching durable TUI identity, if this is the same live row."""
    raw_name = row.get("name")
    raw_target = row.get("tmux_target")
    raw_created_at = row.get("created_at")
    if (
        not isinstance(raw_name, str)
        or not raw_name
        or not isinstance(raw_target, str)
        or not raw_target
        or not isinstance(raw_created_at, str)
        or not raw_created_at
    ):
        return None
    path = presentation_identity_path(raw_name)
    if not path.exists():
        return None
    try:
        raw = json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        die(f"presentation identity {path} is corrupt ({exc}); inspect it before sending work")
    if not isinstance(raw, dict):
        die(f"presentation identity {path} must contain a JSON object")
    if (
        raw.get("name") != raw_name
        or raw.get("tmux_target") != raw_target
        or raw.get("created_at") != raw_created_at
    ):
        return None
    backend = raw.get("backend")
    mode = raw.get("mode")
    pane = raw.get("presentation_pane")
    if not isinstance(backend, str) or not isinstance(mode, str) or not isinstance(pane, str) or not pane:
        die(f"presentation identity {path} is missing a valid backend, mode, or pane id")
    require_supported_backend(backend)
    if require_supported_mode(mode) != TUI_MODE:
        die(f"presentation identity {path} must describe a TUI agent")
    return PresentationIdentity(backend=backend, mode=mode, presentation_pane=pane)


def _save_presentation_identity(rec: AgentRecord) -> None:
    """Atomically mirror a live TUI's presentation identity outside its row."""
    path = presentation_identity_path(rec.name)
    if rec.mode != TUI_MODE or rec.presentation_pane is None:
        path.unlink(missing_ok=True)
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "name": rec.name,
        "created_at": rec.created_at,
        "backend": rec.backend,
        "tmux_target": rec.tmux_target,
        "mode": rec.mode,
        "presentation_pane": rec.presentation_pane,
    }
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2) + "\n")
    os.replace(tmp, path)


def _permission_policy_path(name: str) -> Path:
    return STATE / name / "permissions.json"


def _load_codex_permission_policy(row: dict[str, object]) -> bool:
    value = row.get("codex_bypass_permissions")
    if "codex_bypass_permissions" not in row:
        # Old runners rewrite every row using their original schema. Preserve
        # new workers' choices independently so a dropped field cannot enable
        # bypass. Only genuinely old records retain the historical default.
        value = True
        path = _permission_policy_path(str(row["name"]))
        if path.exists():
            try:
                saved = json.loads(path.read_text())
            except (OSError, json.JSONDecodeError) as exc:
                raise AgentOperationError("invalid_permission_policy", f"cannot read worker permissions {path}: {exc}") from exc
            if not isinstance(saved, dict):
                raise AgentOperationError("invalid_permission_policy", f"worker permissions {path} must contain an object")
            if saved.get("name") == row.get("name") and saved.get("created_at") == row.get("created_at"):
                value = saved.get("codex_bypass_permissions")
    if not isinstance(value, bool):
        raise AgentOperationError("invalid_permission_policy", "recorded codex_bypass_permissions must be a boolean")
    return value


def _save_codex_permission_policy(rec: AgentRecord) -> None:
    path = _permission_policy_path(rec.name)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"name": rec.name, "created_at": rec.created_at, "codex_bypass_permissions": rec.codex_bypass_permissions}
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload) + "\n")
    os.replace(tmp, path)


def _clear_presentation_identity(name: str) -> None:
    presentation_identity_path(name).unlink(missing_ok=True)
    _permission_policy_path(name).unlink(missing_ok=True)


# --------------------------------------------------------------------------- #
# Registry persistence (exclusive-locked read-modify-write)
# --------------------------------------------------------------------------- #


def _load_unlocked() -> tuple[dict[str, AgentRecord], bool]:
    if not REGISTRY.exists():
        return {}, False
    try:
        raw = json.loads(REGISTRY.read_text())
    except json.JSONDecodeError as exc:
        die(f"registry.json is corrupt ({exc}); inspect {REGISTRY} by hand")
    if not isinstance(raw, list):
        die(f"registry.json must be a JSON array, found {type(raw).__name__}")
    out: dict[str, AgentRecord] = {}
    presentation_repair_needed = False
    for row in raw:
        if not isinstance(row, dict):
            die("registry.json rows must be JSON objects")
        rec = AgentRecord.from_dict(row)
        out[rec.name] = rec
        # An old MCP writer can serialize a row without TUI-only fields. The
        # durable presentation identity rehydrates them above; record that the
        # registry itself needs repair so the next read makes the canonical row
        # whole again instead of leaving a live TUI stranded on disk.
        if rec.mode == TUI_MODE and any(
            row.get(field) != rec.to_dict()[field]
            for field in ("backend", "mode", "presentation_pane")
        ):
            presentation_repair_needed = True
    return out, presentation_repair_needed


def _save_unlocked(agents: dict[str, AgentRecord]) -> None:
    # Write TUI identities before the registry. If the process crashes between
    # the two writes, a headless row cannot match the newer Herdr tab and will
    # not be misclassified; a TUI row is never published without its fallback.
    for rec in agents.values():
        _save_presentation_identity(rec)
        _save_codex_permission_policy(rec)
    payload = [agents[name].to_dict() for name in sorted(agents)]
    tmp = REGISTRY.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2) + "\n")
    os.replace(tmp, REGISTRY)


@contextlib.contextmanager
def registry_lock() -> Iterator[dict[str, AgentRecord]]:
    """Exclusive read-modify-write over the registry.

    Yields the mutable agents mapping; on clean exit the (possibly mutated)
    mapping is written back atomically.
    """
    BASE.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(LOCKFILE), os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        agents, _ = _load_unlocked()
        yield agents
        _save_unlocked(agents)
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def read_registry() -> dict[str, AgentRecord]:
    """Read the registry and repair stripped live-TUI presentation fields."""
    BASE.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(LOCKFILE), os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_SH)
        agents, presentation_repair_needed = _load_unlocked()
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)
    if not presentation_repair_needed:
        return agents

    # Do not continuously rewrite ordinary status reads. Only take the
    # exclusive lock when a sidecar proved that a legacy writer dropped a live
    # TUI's mode or pane. Re-load while exclusive so a concurrent repair wins.
    fd = os.open(str(LOCKFILE), os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        agents, presentation_repair_needed = _load_unlocked()
        if presentation_repair_needed:
            _save_unlocked(agents)
        return agents
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


# --------------------------------------------------------------------------- #
# Per-agent filesystem layout
# --------------------------------------------------------------------------- #


def agent_dir(name: str) -> Path:
    """Return the runtime directory for one validated worker name."""
    return STATE / name


def inbox_dir(name: str) -> Path:
    """Return the ordered pending-message directory for one worker."""
    return agent_dir(name) / "inbox"


def processed_dir(name: str) -> Path:
    """Return the directory retaining messages consumed by the worker."""
    return agent_dir(name) / "processed"


def inflight_dir(name: str) -> Path:
    """Return requests claimed before headless execution and awaiting completion."""
    return agent_dir(name) / "inflight"


def failed_dir(name: str) -> Path:
    """Return retained malformed or possibly executed requests requiring inspection."""
    return agent_dir(name) / "failed"


def _sync_directory(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _write_durable_json(path: Path, value: object) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{secrets.token_hex(6)}.tmp")
    try:
        with temporary.open("x") as stream:
            json.dump(value, stream)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        _sync_directory(path.parent)
    finally:
        temporary.unlink(missing_ok=True)


@contextlib.contextmanager
def _harness_lock(name: str) -> Iterator[None]:
    BASE.mkdir(parents=True, exist_ok=True)
    path = BASE / f".harness-{require_valid_name(name)}.lock"
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def record_active_harness(name: str, runner: RunnerIdentity, harness: RunnerIdentity) -> None:
    """Record an owned harness process group for bounded stop and orphan cleanup."""
    if harness.started_at is None or runner.started_at is None:
        raise AgentOperationError("harness_identity_unknown", "cannot establish active harness process identity")
    _write_durable_json(agent_dir(name) / "active-harness.json", {
        "runner_pid": runner.pid, "runner_started_at": runner.started_at,
        "pid": harness.pid, "started_at": harness.started_at,
    })


def terminate_active_harness(rec: AgentRecord) -> bool:
    """Best-effort kill of a harness group after checking its recorded leader."""
    path = agent_dir(rec.name) / "active-harness.json"
    try:
        value = json.loads(path.read_text())
    except FileNotFoundError:
        return False
    except (OSError, ValueError) as exc:
        raise AgentOperationError("harness_identity_unknown", f"cannot read active harness identity: {exc}") from exc
    if not isinstance(value, dict) or value.get("runner_pid") != rec.runner_pid:
        return False
    if rec.runner_started_at is not None and value.get("runner_started_at") != rec.runner_started_at:
        return False
    pid, started_at = value.get("pid"), value.get("started_at")
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 1 or not isinstance(started_at, str):
        raise AgentOperationError("harness_identity_unknown", "malformed active harness identity")
    if pid_start_time(pid) != started_at:
        return False
    try:
        if os.getpgid(pid) != pid:
            raise AgentOperationError("harness_identity_changed", "active harness no longer owns its process group")
        # killpg remains a numeric-PGID operation; the checks narrow but cannot
        # eliminate reuse between this observation and the syscall.
        os.killpg(pid, signal.SIGKILL)
    except ProcessLookupError:
        return False
    return True


def transcript_path(name: str) -> Path:
    """Return the path of the worker transcript used for headless turn reads."""
    return agent_dir(name) / "transcript.log"


def last_message_path(name: str) -> Path:
    """Return the file containing the most recent captured headless answer."""
    return agent_dir(name) / "last-message.txt"


def runner_pid_path(name: str) -> Path:
    """Return the runner process-identity file path."""
    return agent_dir(name) / "runner.pid"


def runner_stderr_path(name: str) -> Path:
    """Return the diagnostic log path for the worker runner."""
    return agent_dir(name) / "runner.stderr.log"


def stop_path(name: str) -> Path:
    """Return the stop-marker path checked between worker turns."""
    return agent_dir(name) / "STOP"


def migration_pause_path(name: str) -> Path:
    """Return the marker requesting that the source runner pause intake."""
    return agent_dir(name) / "MIGRATION_PAUSE"


def migration_pause_ack_path(name: str) -> Path:
    """Return the source-runner acknowledgement path for a migration pause."""
    return agent_dir(name) / "MIGRATION_PAUSE_ACK"


def staged_runner_path(name: str, token: str) -> Path:
    """Return the token-specific identity file for a replacement runner."""
    return agent_dir(name) / f"MIGRATION_STAGED_{token}.json"


def staged_runner_activate_path(name: str, token: str) -> Path:
    """Return the marker permitting a staged runner to begin intake."""
    return agent_dir(name) / f"MIGRATION_ACTIVATE_{token}"


def staged_runner_activated_path(name: str, token: str) -> Path:
    """Return the acknowledgement path proving a staged runner took over intake."""
    return agent_dir(name) / f"MIGRATION_ACTIVATED_{token}"


def write_migration_pause(name: str, old: RunnerIdentity) -> None:
    """Record the source identity whose intake must pause before migration."""
    migration_pause_path(name).write_text(
        json.dumps({"pid": old.pid, "started_at": old.started_at, "created_at": now_iso()}) + "\n"
    )


def acknowledge_migration_pause(name: str, identity: RunnerIdentity) -> None:
    """Record that this runner has stopped consuming pending messages."""
    tmp = migration_pause_ack_path(name).with_suffix(".tmp")
    tmp.write_text(
        json.dumps({"pid": identity.pid, "started_at": identity.started_at, "acknowledged_at": now_iso()})
        + "\n"
    )
    os.replace(tmp, migration_pause_ack_path(name))


def pause_acknowledged(name: str, old: RunnerIdentity) -> bool:
    """Check that the pause acknowledgement belongs to the expected source process."""
    try:
        raw = cast(dict[str, object], json.loads(migration_pause_ack_path(name).read_text()))
    except (OSError, json.JSONDecodeError):
        return False
    return raw.get("pid") == old.pid and raw.get("started_at") == old.started_at


def write_staged_runner(name: str, token: str, identity: RunnerIdentity) -> None:
    """Publish the identity of a replacement runner awaiting activation."""
    path = staged_runner_path(name, token)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(
        json.dumps({"pid": identity.pid, "started_at": identity.started_at, "recorded_at": now_iso()})
        + "\n"
    )
    os.replace(tmp, path)


def read_staged_runner(name: str, token: str) -> Optional[RunnerIdentity]:
    """Read a replacement-runner identity, returning None when no valid record exists."""
    try:
        raw = json.loads(staged_runner_path(name, token).read_text())
        pid = int(raw["pid"])
        started_at = raw.get("started_at")
    except (OSError, json.JSONDecodeError, KeyError, TypeError, ValueError):
        return None
    current_start = pid_start_time(pid)
    if current_start is None:
        return None
    if started_at is not None and started_at != current_start:
        return None
    return RunnerIdentity(pid=pid, started_at=None if started_at is None else str(started_at))


def clear_migration_markers(name: str, token: str) -> None:
    """Remove the pause and staged-activation markers for a completed or rolled-back migration."""
    for path in (
        migration_pause_path(name),
        migration_pause_ack_path(name),
        staged_runner_path(name, token),
        staged_runner_activate_path(name, token),
        staged_runner_activated_path(name, token),
    ):
        try:
            path.unlink()
        except FileNotFoundError:
            pass


def acknowledge_staged_runner_activation(name: str, token: str) -> None:
    """Record that the staged runner has accepted its activation marker."""
    staged_runner_activated_path(name, token).write_text(now_iso() + "\n")


def wait_for_staged_runner_activation(name: str, token: str) -> bool:
    """Wait up to the configured migration bound for replacement-runner acknowledgement."""
    deadline = time.monotonic() + MIGRATION_READY_TIMEOUT_S
    while time.monotonic() < deadline:
        if staged_runner_activated_path(name, token).exists():
            return True
        time.sleep(0.05)
    return staged_runner_activated_path(name, token).exists()


def ensure_agent_dirs(name: str) -> None:
    """Create the pending, inflight, processed, and failed request directories."""
    inbox_dir(name).mkdir(parents=True, exist_ok=True)
    inflight_dir(name).mkdir(parents=True, exist_ok=True)
    processed_dir(name).mkdir(parents=True, exist_ok=True)
    failed_dir(name).mkdir(parents=True, exist_ok=True)


def last_message_preview(name: str, limit: int = 240) -> str:
    """Return a bounded, single-line preview of the latest captured answer."""
    p = last_message_path(name)
    if not p.exists():
        return ""
    text = " ".join(p.read_text(errors="replace").split())
    if len(text) <= limit:
        return text
    return text[: max(0, limit - 3)] + "..."


def write_event(
    event_type: str,
    name: str,
    *,
    seq: Optional[int] = None,
    rc: Optional[str] = None,
    preview: Optional[str] = None,
) -> None:
    """Append a lifecycle event for the MCP WebSocket broadcaster.

    This central log lives outside per-agent dirs so `agent_down` archival cannot
    move a just-written down event away from the watcher.
    """
    STATE.mkdir(parents=True, exist_ok=True)
    event: dict[str, object] = {
        "id": f"{time.time_ns()}-{os.getpid()}",
        "type": event_type,
        "name": name,
        "ts": now_iso(),
    }
    if seq is not None:
        event["seq"] = seq
    if rc is not None:
        event["rc"] = rc
    if preview is not None:
        event["preview"] = preview
    fd = os.open(str(EVENT_LOCKFILE), os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        with EVENT_LOG.open("a") as fh:
            fh.write(json.dumps(event, sort_keys=True) + "\n")
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


# --------------------------------------------------------------------------- #
# Inbox (file-per-message, ordered by zero-padded seq)
# --------------------------------------------------------------------------- #


@dataclasses.dataclass
class Message:
    """A durable queued turn with sequence, prompt, model, reasoning effort, and enqueue time."""
    seq: int
    text: str
    model: Optional[str]
    queued_at: str
    effort: Optional[str] = None

    @staticmethod
    def from_path(path: Path) -> "Message":
        """Read a queued turn, accepting records that omit optional model and effort overrides."""
        d = json.loads(path.read_text())
        return Message(
            seq=int(d["seq"]),
            text=str(d["text"]),
            model=(None if d.get("model") is None else str(d["model"])),
            queued_at=str(d["queued_at"]),
            effort=(None if d.get("effort") is None else str(d["effort"])),
        )


def _msg_filename(seq: int) -> str:
    return f"{seq:012d}.json"


def enqueue_message(name: str, text: str, model: Optional[str]) -> int:
    """Allocate the next seq under lock and drop a message file atomically.

    Raises via die() if the agent is unknown.
    """
    with registry_lock() as agents:
        rec = agents.get(name)
        if rec is None:
            die(f"unknown agent {name!r}; run agent_up first or check agent_status")
        if migration_pause_path(name).exists():
            raise AgentOperationError(
                "agent_migrating",
                f"agent {name!r} is migrating; wait for migration to finish before sending a turn",
            )
        seq = rec.next_seq
        rec.next_seq += 1
    ensure_agent_dirs(name)
    payload = {"seq": seq, "text": text, "model": model, "queued_at": now_iso(),
               "effort": os.environ.get("SUBAGENT_EFFORT")}
    dest = inbox_dir(name) / _msg_filename(seq)
    tmp = dest.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2) + "\n")
    os.replace(tmp, dest)
    return seq


def next_pending_message(name: str) -> Optional[Path]:
    """Return the first sequence-ordered pending message path, or None when idle."""
    box = inbox_dir(name)
    if not box.exists():
        return None
    files = sorted(p for p in box.iterdir() if p.suffix == ".json")
    return files[0] if files else None


def pending_count(name: str) -> int:
    """Count the worker message files still awaiting consumption."""
    box = inbox_dir(name)
    if not box.exists():
        return 0
    return sum(1 for p in box.iterdir() if p.suffix == ".json")


def _record_tui_delivery_failure(path: Path, exc: AgentOperationError) -> int:
    """Persist a failed attempt before retrying or quarantining a TUI message."""
    raw = json.loads(path.read_text())
    attempts = raw.get("tui_delivery_attempts", 0)
    if not isinstance(attempts, int) or attempts < 0:
        raise AgentOperationError(
            "tui_inbox_corrupt", f"TUI inbox message {path} has an invalid delivery-attempt count"
        )
    attempts += 1
    raw["tui_delivery_attempts"] = attempts
    raw["tui_delivery_error"] = exc.message
    raw["tui_delivery_failed_at"] = now_iso()
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(raw, indent=2) + "\n")
    os.replace(tmp, path)
    return attempts


def _quarantine_tui_message(
    name: str, path: Path, exc: AgentOperationError, *, attempts: Optional[int] = None
) -> int:
    """Move one poison message aside so it cannot block later FIFO work."""
    recorded_attempts = _record_tui_delivery_failure(path, exc) if attempts is None else attempts
    message = Message.from_path(path)
    dest = failed_dir(name) / path.name
    os.replace(path, dest)
    write_event(
        "tui_message_quarantined",
        name,
        seq=message.seq,
        preview=(
            f"attempts={recorded_attempts}; moved to {dest}; delivery error: {exc.message[-400:]}"
        ),
    )
    return message.seq


def tui_inbox_snapshot(name: str) -> TuiInboxResult:
    """Inspect queued and quarantined message sequence numbers without delivery."""
    valid_name = require_valid_name(name)
    pending = [Message.from_path(path).seq for path in sorted(inbox_dir(valid_name).glob("*.json"))]
    quarantined = [Message.from_path(path).seq for path in sorted(failed_dir(valid_name).glob("*.json"))]
    return TuiInboxResult(
        name=valid_name,
        pending=pending,
        quarantined=quarantined,
        failed_dir=str(failed_dir(valid_name)),
    )


def _shared_tui_client() -> SharedHerdrClient:
    def run(command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        # Let SharedHerdrClient normalize OSError/TimeoutExpired into HerdrUnavailable so every
        # adapter observes the shared typed failure contract.
        return subprocess.run(
            list(command), capture_output=True, text=True, timeout=max(35, HERDR_API_TIMEOUT_S)
        )

    # Supplying the runtime's runner preserves HERDR_BIN overrides and keeps tests at the
    # subprocess boundary; the shared client still owns protocol validation and all messaging.
    return SharedHerdrClient(herdr_bin=HERDR_BIN, run=run)


def _shared_tui_target(rec: AgentRecord) -> SharedAgentTarget:
    """Bind a named registry row to the generic transport target."""
    return SharedAgentTarget(
        pane_id=rec.presentation_pane,
        session_agent=rec.harness if rec.session_id is not None else None,
        session_value=rec.session_id,
        expected_agent=rec.harness,
        expected_workspace=HERDR_WORKSPACE_LABEL,
        expected_cwd=rec.cwd,
    )


# --------------------------------------------------------------------------- #
# Process + presentation backend helpers
# --------------------------------------------------------------------------- #


def pid_alive(pid: Optional[int]) -> bool:
    """Check whether a process ID currently exists without sending it a signal."""
    if pid is None:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def pid_start_time(pid: int) -> Optional[str]:
    """Return Linux start ticks, or ``None`` only for positive disappearance.

    Unreadable or malformed procfs evidence is not a death certificate and must
    not authorize later process signaling.
    """
    stat = Path(f"/proc/{pid}/stat")
    try:
        raw = stat.read_bytes()
    except (FileNotFoundError, ProcessLookupError):
        return None
    except OSError as exc:
        raise AgentOperationError(
            "runner_identity_unknown",
            f"cannot read runner process {pid} identity: {exc}",
        ) from exc
    parsed = parse_process_stat(raw)
    if parsed is None or parsed.pid != pid:
        raise AgentOperationError(
            "runner_identity_unknown",
            f"cannot parse runner process {pid} identity",
        )
    return str(parsed.starttime)


def runner_identity_alive(rec: AgentRecord) -> bool:
    """True only when the registry's runner PID still names the same process."""
    if rec.runner_pid is None:
        return False
    current = pid_start_time(rec.runner_pid)
    if current is None:
        return False
    if rec.runner_started_at is None:
        # Backward-compatible for rows created before runner starttime existed.
        return True
    return current == rec.runner_started_at


def wait_for_pause_ack(name: str, old: RunnerIdentity) -> bool:
    """Wait within the migration deadline for the source runner to acknowledge paused intake."""
    deadline = time.monotonic() + MIGRATION_READY_TIMEOUT_S
    while time.monotonic() < deadline:
        if not pid_alive(old.pid):
            return False
        if pause_acknowledged(name, old):
            return True
        time.sleep(0.05)
    return pause_acknowledged(name, old) and pid_alive(old.pid)


def wait_for_staged_runner(name: str, token: str) -> Optional[RunnerIdentity]:
    """Wait within the migration deadline for a live replacement-runner identity."""
    deadline = time.monotonic() + MIGRATION_READY_TIMEOUT_S
    while time.monotonic() < deadline:
        identity = read_staged_runner(name, token)
        if identity is not None:
            return identity
        time.sleep(0.05)
    return read_staged_runner(name, token)


def wait_for_restarted_runner(name: str, previous: RunnerIdentity) -> Optional[RunnerIdentity]:
    """Wait for a normal replacement runner to publish a new registry identity."""
    deadline = time.monotonic() + MIGRATION_READY_TIMEOUT_S
    while time.monotonic() < deadline:
        rec = read_registry().get(name)
        if rec is not None and rec.runner_pid is not None:
            identity = RunnerIdentity(rec.runner_pid, rec.runner_started_at)
            if identity != previous and runner_identity_alive(rec):
                return identity
        time.sleep(0.05)
    rec = read_registry().get(name)
    if rec is None or rec.runner_pid is None:
        return None
    identity = RunnerIdentity(rec.runner_pid, rec.runner_started_at)
    return identity if identity != previous and runner_identity_alive(rec) else None


def _pid_is_descendant(pid: int, ancestor: int) -> bool:
    cur = pid
    seen: set[int] = set()
    while cur > 1 and cur not in seen:
        if cur == ancestor:
            return True
        seen.add(cur)
        stat = Path(f"/proc/{cur}/stat")
        try:
            parsed = parse_process_stat(stat.read_bytes())
            if parsed is None or parsed.pid != cur:
                return False
            cur = parsed.ppid
        except OSError:
            return False
    return False


_TMUX_MISSING_WARNED = False


def _tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run tmux. With ``check=False``, a MISSING tmux binary is a failed call.

    Every ``check=False`` caller already reads a nonzero return code as "no
    session / no such window", and a host with no tmux at all is the strongest
    possible form of that. Previously the missing binary raised
    ``FileNotFoundError`` from ``Popen`` instead -- ``check=False`` suppresses
    exit codes, not a missing executable -- so on such a host
    ``bring_down_agent(force=True)`` crashed on its way to archiving rather than
    completing the retirement it was asked to perform. That is the
    backend-loss retirement gap in a different disguise, and it is why a CI
    runner without tmux failed a test that passes on developer boxes.

    Not silent: the first occurrence warns, so "tmux is not installed" is
    visible rather than looking like an empty tmux session. ``check=True``
    callers still get the exception, because they genuinely depend on tmux.
    """
    global _TMUX_MISSING_WARNED
    try:
        return subprocess.run(
            ["tmux", *args], capture_output=True, text=True, check=check
        )
    except FileNotFoundError:
        if check:
            raise
        if not _TMUX_MISSING_WARNED:
            _TMUX_MISSING_WARNED = True
            print(
                "WARN: tmux is not installed; treating every tmux query as "
                "'no such session/window'. Agent windows cannot be created or "
                "inspected on this host.",
                file=sys.stderr,
            )
        return subprocess.CompletedProcess(
            args=["tmux", *args], returncode=127, stdout="", stderr="tmux: not found"
        )


def tmux_available() -> bool:
    """Check whether tmux is installed and can report its version."""
    try:
        subprocess.run(["tmux", "-V"], capture_output=True, check=True)
    except (OSError, subprocess.CalledProcessError):
        return False
    return True


def session_exists(session: Optional[str] = None) -> bool:
    """Check whether a recorded or newly configured tmux session exists."""
    return _tmux("has-session", "-t", f"={session or TMUX_SESSION}", check=False).returncode == 0


def _tmux_window_exists(name: str) -> bool:
    return _tmux_target_exists(f"{TMUX_SESSION}:{name}")


def _tmux_target_exists(target: str) -> bool:
    session, separator, name = target.partition(":")
    if not separator or not session or not name:
        return False
    res = _tmux("list-windows", "-t", f"={session}", "-F", "#{window_name}", check=False)
    if res.returncode != 0:
        return False
    return name in res.stdout.splitlines()


def _exact_tmux_target(target: str) -> str:
    session, separator, name = target.partition(":")
    if not separator or not session or not name:
        raise AgentOperationError("invalid_tmux_target", f"invalid recorded tmux window target {target!r}")
    return f"={session}:={name}"


def _kill_tmux_window(name: str) -> None:
    _tmux("kill-window", "-t", _exact_tmux_target(f"{TMUX_SESSION}:{name}"), check=False)


def create_legacy_tmux_guard(name: str, cwd: str, *, target: Optional[str] = None) -> None:
    """Keep a tmux-only MCP garbage collector from reaping a Herdr runner."""
    target = target or f"{TMUX_SESSION}:{name}"
    _exact_tmux_target(target)
    if _tmux_target_exists(target):
        return
    session, _, window = target.partition(":")
    _launch_tmux_window(window, cwd, "exec sleep infinity", session=session)
    marker = agent_dir(name) / "legacy-tmux-guard"
    marker.write_text(target + "\n")


def remove_legacy_tmux_guard(name: str) -> None:
    """Remove the compatibility window that reserves a migrating worker name."""
    marker = agent_dir(name) / "legacy-tmux-guard"
    # Guards made before target metadata existed always used this fixed
    # session. A new default must never redirect their teardown elsewhere.
    target = marker.read_text().strip() if marker.exists() else f"subagents:{name}"
    _tmux("kill-window", "-t", _exact_tmux_target(target), check=False)
    marker.unlink(missing_ok=True)


def _launch_tmux_window(name: str, cwd: str, wrapper_cmd: str, *, session: Optional[str] = None) -> str:
    """Create the agent's tmux window running ``wrapper_cmd`` via bash -lc."""
    session = session or TMUX_SESSION
    if not session_exists(session):
        _tmux(
            "new-session", "-d", "-s", session, "-n", name,
            "-c", cwd, "bash", "-lc", wrapper_cmd,
        )
    else:
        _tmux(
            "new-window", "-t", f"={session}:", "-n", name,
            "-c", cwd, "bash", "-lc", wrapper_cmd,
        )
    return f"{session}:{name}"


def _herdr_stdout(*args: str, timeout_s: Optional[float] = None) -> str:
    """Run one bounded Herdr request and return successful stdout verbatim."""
    timeout = HERDR_API_TIMEOUT_S if timeout_s is None else timeout_s
    try:
        result = subprocess.run(
            [HERDR_BIN, *args], capture_output=True, text=True, check=False, timeout=timeout
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise AgentOperationError(
            "herdr_unavailable",
            f"Herdr probe {' '.join(args)} failed within {timeout:g}s: {exc}",
        ) from exc
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
        raise AgentOperationError("herdr_api_failed", f"herdr {' '.join(args)} failed: {detail}")
    return result.stdout


def _herdr(*args: str, timeout_s: Optional[float] = None) -> dict[str, object]:
    """Run one bounded Herdr request and return its result or wait-event envelope.

    Ordinary Herdr commands respond with ``{"result": ...}``. A successful
    ``wait agent-status`` transition instead responds with a top-level event,
    so preserve that envelope for its caller rather than misclassifying the
    confirmed transition as malformed JSON.
    """
    stdout = _herdr_stdout(*args, timeout_s=timeout_s)
    try:
        raw = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise AgentOperationError(
            "herdr_api_invalid",
            f"herdr {' '.join(args)} returned invalid API JSON: {stdout.strip()}",
        ) from exc
    if not isinstance(raw, dict):
        raise AgentOperationError(
            "herdr_api_invalid",
            f"herdr {' '.join(args)} returned invalid API JSON: {stdout.strip()}",
        )
    payload = raw.get("result")
    if payload is None:
        event = raw.get("event")
        data = raw.get("data")
        if isinstance(event, str) and isinstance(data, dict):
            return {"event": event, "data": data}
        raise AgentOperationError(
            "herdr_api_invalid",
            f"herdr {' '.join(args)} returned invalid API JSON: {stdout.strip()}",
        )
    if not isinstance(payload, dict):
        raise AgentOperationError("herdr_api_invalid", f"herdr {' '.join(args)} returned no object")
    return payload


def _herdr_action(*args: str, timeout_s: Optional[float] = None) -> None:
    """Run a mutating Herdr command, including commands whose success is empty."""
    _herdr_stdout(*args, timeout_s=timeout_s)


def _herdr_items(payload: dict[str, object], key: str) -> list[dict[str, object]]:
    items = payload.get(key)
    if not isinstance(items, list) or not all(isinstance(item, dict) for item in items):
        raise AgentOperationError("herdr_api_invalid", f"herdr response lacks a {key} list")
    return items


def _herdr_not_found(exc: AgentOperationError) -> bool:
    message = exc.message.lower()
    return "not_found" in message or "not found" in message


def _configured_herdr_workspace() -> Optional[str]:
    raw = _load_backend_config().get("herdr_workspace_id")
    return raw if isinstance(raw, str) and raw else None


def _remember_herdr_workspace(workspace_id: str) -> None:
    config = _load_backend_config()
    config["herdr_workspace_id"] = workspace_id
    _save_backend_config(config)


def _herdr_workspace_id(cwd: str) -> str:
    configured = _configured_herdr_workspace()
    if configured is not None:
        try:
            candidate = _herdr("workspace", "get", configured).get("workspace")
            workspace = candidate if isinstance(candidate, dict) else None
        except AgentOperationError as exc:
            if not _herdr_not_found(exc):
                raise
            workspace = None
        if isinstance(workspace, dict) and workspace.get("label") == HERDR_WORKSPACE_LABEL:
            return configured
    for workspace in _herdr_items(_herdr("workspace", "list"), "workspaces"):
        workspace_id = workspace.get("workspace_id")
        if workspace.get("label") == HERDR_WORKSPACE_LABEL and isinstance(workspace_id, str):
            _remember_herdr_workspace(workspace_id)
            return workspace_id
    created = _herdr(
        "workspace", "create", "--cwd", cwd, "--label", HERDR_WORKSPACE_LABEL, "--no-focus"
    )
    candidate = created.get("workspace")
    workspace = candidate if isinstance(candidate, dict) else None
    if not isinstance(workspace, dict) or not isinstance(workspace.get("workspace_id"), str):
        raise AgentOperationError("herdr_api_invalid", "Herdr did not return its new workspace id")
    workspace_id = str(workspace["workspace_id"])
    _remember_herdr_workspace(workspace_id)
    return workspace_id


def _herdr_tab_exists(tab_id: str) -> bool:
    try:
        _herdr("tab", "get", tab_id)
    except AgentOperationError as exc:
        if _herdr_not_found(exc):
            return False
        raise
    return True


def _herdr_workspace_from_tab(tab_id: str) -> Optional[str]:
    workspace_id, separator, _tab = tab_id.partition(":")
    return workspace_id if separator and workspace_id else None


def _herdr_workspace_exists(workspace_id: str) -> Optional[bool]:
    """Return False only for a confirmed missing workspace; preserve on probe errors."""
    try:
        _herdr("workspace", "get", workspace_id, timeout_s=HERDR_TUI_PROBE_TIMEOUT_S)
    except AgentOperationError as exc:
        if _herdr_not_found(exc):
            return False
        return None
    return True


def _herdr_tab_for_new_agent(workspace_id: str, cwd: str) -> tuple[str, str]:
    """Create a fresh one-pane tab for an agent command.

    The Herdr workspace is shared by multiple checkouts, while each checkout
    has its own local registry. Therefore an unrecognized existing tab may
    belong to another live worker and must never be reused.
    """
    created = _herdr(
        "tab", "create", "--workspace", workspace_id, "--cwd", cwd,
        "--label", "subagent", "--no-focus"
    )
    raw_tab = created.get("tab")
    raw_root = created.get("root_pane")
    created_tab = raw_tab if isinstance(raw_tab, dict) else None
    created_root = raw_root if isinstance(raw_root, dict) else None
    if (
        not isinstance(created_tab, dict)
        or not isinstance(created_root, dict)
        or not isinstance(created_tab.get("tab_id"), str)
        or not isinstance(created_root.get("pane_id"), str)
    ):
        raise AgentOperationError("herdr_api_invalid", "Herdr did not return a tab and root pane")
    return str(created_tab["tab_id"]), str(created_root["pane_id"])


def _launch_herdr_window(name: str, cwd: str, wrapper_cmd: str) -> str:
    workspace_id = _herdr_workspace_id(cwd)
    tab_id, root_pane_id = _herdr_tab_for_new_agent(workspace_id, cwd)
    try:
        _herdr_action(
            "pane", "run", root_pane_id,
            f"/bin/bash -lc {shlex.quote(wrapper_cmd)}",
        )
        _herdr("tab", "rename", tab_id, name)
    except Exception as exc:
        if _herdr_tab_exists(tab_id):
            try:
                _herdr("tab", "close", tab_id)
            except AgentOperationError as cleanup_exc:
                raise AgentOperationError(
                    "herdr_cleanup_failed",
                    f"failed to launch Herdr tab {tab_id} ({exc}); cleanup also failed: {cleanup_exc}",
                ) from cleanup_exc
        raise
    return tab_id


@dataclasses.dataclass(frozen=True)
class TuiProbe:
    """Bounded health/state snapshot for one interactive Herdr-hosted TUI."""

    pane_alive: bool
    codex_alive: bool
    status: str
    pid: Optional[int]
    error: Optional[str] = None


def _herdr_tui_probe(pane_id: str) -> TuiProbe:
    """Inspect a TUI pane without allowing status/gc to block indefinitely."""
    try:
        process_info = _herdr(
            "pane", "process-info", "--pane", pane_id, timeout_s=HERDR_TUI_PROBE_TIMEOUT_S
        ).get("process_info")
        if not isinstance(process_info, dict):
            raise AgentOperationError("herdr_api_invalid", "Herdr did not return pane process information")
        raw_processes = process_info.get("foreground_processes")
        processes = (
            raw_processes
            if isinstance(raw_processes, list) and all(isinstance(item, dict) for item in raw_processes)
            else []
        )
        pane_alive = bool(processes)
        codex_name = Path(CODEX_BIN).name
        codex_process = next(
            (
                process
                for process in processes
                if process.get("name") == codex_name
                or (
                    isinstance(process.get("argv"), list)
                    and bool(process["argv"])
                    and str(process["argv"][0]) == CODEX_BIN
                )
            ),
            None,
        )
        raw_pid = None if codex_process is None else codex_process.get("pid")
        pid = raw_pid if isinstance(raw_pid, int) else None
        agent = _herdr(
            "agent", "get", pane_id, timeout_s=HERDR_TUI_PROBE_TIMEOUT_S
        ).get("agent")
        raw_status = agent.get("agent_status") if isinstance(agent, dict) else "unknown"
        status = raw_status if isinstance(raw_status, str) else "unknown"
        return TuiProbe(
            pane_alive=pane_alive,
            codex_alive=pid is not None and pid_alive(pid),
            status=status,
            pid=pid,
        )
    except AgentOperationError as exc:
        return TuiProbe(False, False, "probe_error", None, exc.message)


def _session_recorded_model(session_id: str) -> Optional[str]:
    """Read the initial interactive model from a local persisted Codex session.

    Codex does not preserve a session model when ``resume`` omits ``-m``: it
    instead loads the caller's current default. A matching explicit model avoids
    its resume-time warning. The session file is local best-effort metadata, so
    unavailable or partially-written files deliberately fall back to no model
    override; the startup prompt handler below remains the safety net.
    """
    for path in sorted(CODEX_SESSIONS.glob(f"**/*-{session_id}.jsonl")):
        try:
            lines = path.read_text().splitlines()
        except OSError:
            continue
        for line in lines:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                # A Codex session can be observed while its last JSONL line is
                # being appended. Earlier complete events remain authoritative.
                continue
            if not isinstance(event, dict) or event.get("type") != "turn_context":
                continue
            payload = event.get("payload")
            model = payload.get("model") if isinstance(payload, dict) else None
            if isinstance(model, str) and model:
                return model
    return None


def _codex_tui_argv(
    model: Optional[str], session_id: Optional[str] = None, *, bypass_permissions: Optional[bool] = None
) -> list[str]:
    """Build the visible Codex command, preserving a resumed session's model."""
    argv = [CODEX_BIN]
    launch_model = model
    if session_id is not None:
        argv.extend(["resume", session_id])
        # Do not force the headless row's possibly stale/default model onto a
        # durable session. Codex otherwise warns (and some versions block) at
        # startup. If local metadata is unavailable, omit -m rather than force
        # a known-mismatching value.
        launch_model = _session_recorded_model(session_id)
    argv.append("--no-alt-screen")
    bypass = _configured_codex_bypass_permissions() if bypass_permissions is None else bypass_permissions
    if bypass:
        argv.append("--dangerously-bypass-approvals-and-sandbox")
    if launch_model:
        argv.extend(["-m", launch_model])
    return argv


def _launch_herdr_tui(
    name: str, cwd: str, model: Optional[str], *, session_id: Optional[str] = None,
    bypass_permissions: Optional[bool] = None,
) -> tuple[str, str, TuiProbe]:
    """Launch one visible Codex TUI in its own Herdr tab and prove its pane."""
    workspace_id = _herdr_workspace_id(cwd)
    tab_id, root_pane_id = _herdr_tab_for_new_agent(workspace_id, cwd)
    argv = _codex_tui_argv(model, session_id, bypass_permissions=bypass_permissions)
    try:
        # Migration has its own longer readiness barrier, including known
        # resume prompts. Herdr's synchronous agent-start readiness timeout
        # would close a live pane before that barrier can inspect it.
        _herdr_action("pane", "run", root_pane_id, shlex.join(argv))
        pane_id = root_pane_id
        _herdr("tab", "rename", tab_id, name)
        deadline = time.monotonic() + 5.0
        probe = _herdr_tui_probe(pane_id)
        while not probe.codex_alive and probe.error is None and time.monotonic() < deadline:
            time.sleep(0.05)
            probe = _herdr_tui_probe(pane_id)
        if not probe.codex_alive:
            detail = probe.error or "Codex was not the foreground TUI process"
            raise AgentOperationError("tui_launch_unconfirmed", f"Herdr TUI pane {pane_id} was not live: {detail}")
    except Exception as exc:
        if _herdr_tab_exists(tab_id):
            try:
                _herdr("tab", "close", tab_id)
            except AgentOperationError as cleanup_exc:
                raise AgentOperationError(
                    "herdr_cleanup_failed",
                    f"failed to launch Herdr TUI tab {tab_id} ({exc}); cleanup also failed: {cleanup_exc}",
                ) from cleanup_exc
        raise
    return tab_id, pane_id, probe


def _herdr_window_exists(name: str) -> bool:
    workspace_id = _configured_herdr_workspace()
    if workspace_id is None:
        return False
    try:
        tabs = _herdr_items(_herdr("tab", "list", "--workspace", workspace_id), "tabs")
    except AgentOperationError as exc:
        if _herdr_not_found(exc):
            return False
        raise
    return any(tab.get("label") == name for tab in tabs)


def _kill_herdr_window(rec: AgentRecord) -> None:
    # The Herdr `subagents` workspace is shared by independently checked-out
    # harnesses. A local registry cannot prove that it has no other tabs, so
    # never close the workspace while tearing down one agent.
    if rec.tmux_target:
        _herdr("tab", "close", rec.tmux_target)


def backend_available(backend: str) -> bool:
    """Probe the selected presentation backend for startup availability."""
    return tmux_available() if backend == "tmux" else herdr_available()


def window_exists(rec: AgentRecord) -> bool:
    """Check the recorded presentation, using process-aware probes for interactive TUIs."""
    if rec.mode == TUI_MODE:
        return rec.presentation_pane is not None and _herdr_tui_probe(rec.presentation_pane).pane_alive
    if rec.backend == "tmux":
        return _tmux_target_exists(rec.tmux_target)
    return bool(rec.tmux_target) and _herdr_tab_exists(rec.tmux_target)


def orphan_window_exists(backend: str, name: str) -> bool:
    """Check whether a named presentation already exists without a local registry owner."""
    return _tmux_window_exists(name) if backend == "tmux" else _herdr_window_exists(name)


def kill_window(rec: AgentRecord) -> None:
    """Close the presentation belonging to this record without closing its workspace."""
    if rec.backend == "tmux":
        _tmux("kill-window", "-t", _exact_tmux_target(rec.tmux_target), check=False)
    else:
        _kill_herdr_window(rec)


def launch_window(backend: str, name: str, cwd: str, wrapper_cmd: str) -> str:
    """Start a visible headless runner in the requested backend and return its target."""
    if backend == "tmux":
        return _launch_tmux_window(name, cwd, wrapper_cmd)
    return _launch_herdr_window(name, cwd, wrapper_cmd)


def find_runner_pane(rec: AgentRecord) -> Optional[str]:
    """Locate the tmux pane containing the recorded live runner process."""
    if rec.runner_pid is None:
        return None
    res = _tmux(
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{pane_pid}",
        check=False,
    )
    if res.returncode != 0:
        return None
    for line in res.stdout.splitlines():
        parts = line.split("\t")
        if len(parts) != 2:
            continue
        pane_id, raw_pid = parts
        try:
            pane_pid = int(raw_pid)
        except ValueError:
            continue
        if _pid_is_descendant(rec.runner_pid, pane_pid):
            return pane_id
    return None


def break_runner_pane_to_window(rec: AgentRecord) -> Optional[str]:
    """Recover a displaced tmux runner pane into its own named window."""
    pane_id = find_runner_pane(rec)
    if pane_id is None:
        return None
    session = rec.tmux_target.partition(":")[0]
    _exact_tmux_target(rec.tmux_target)
    res = _tmux("break-pane", "-d", "-s", pane_id, "-t", f"={session}:", "-n", rec.name, check=False)
    if res.returncode != 0:
        raise AgentOperationError(
            "presentation_recreate_failed",
            f"failed to break pane {pane_id} into window {rec.name!r}: {res.stderr.strip()}",
        )
    return pane_id


def terminate_runner_identity(identity: RunnerIdentity, grace: float = 2.0) -> bool:
    """Best-effort stop of a recorded PID with start-time checks around shutdown."""
    current_start = pid_start_time(identity.pid)
    if current_start is None:
        return False
    if identity.started_at is not None and current_start != identity.started_at:
        return False
    # These compatibility signals use a numeric PID. The surrounding checks
    # narrow but cannot eliminate reuse between an observation and a syscall.
    try:
        os.kill(identity.pid, signal.SIGTERM)
    except ProcessLookupError:
        return False
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline:
        if not pid_alive(identity.pid):
            return True
        time.sleep(0.1)
    if not pid_alive(identity.pid):
        return True
    final_start = pid_start_time(identity.pid)
    if final_start is None or (
        identity.started_at is not None and final_start != identity.started_at
    ):
        return True
    try:
        os.kill(identity.pid, signal.SIGKILL)
    except ProcessLookupError:
        return True
    return True


def terminate_runner(rec: AgentRecord, grace: float = 2.0) -> bool:
    """Best-effort stop of the process recorded for a worker."""
    # Spawn and identity publication use this same lock, so even a zero-grace
    # stop observes the detached child before killing its runner.
    with _harness_lock(rec.name):
        harness_stopped = terminate_active_harness(rec)
        if rec.runner_pid is None or not runner_identity_alive(rec):
            return harness_stopped
        return terminate_runner_identity(RunnerIdentity(rec.runner_pid, rec.runner_started_at), grace)


# --------------------------------------------------------------------------- #
# Archival + garbage collection of dead agents
# --------------------------------------------------------------------------- #


def archive_state(name: str) -> Optional[Path]:
    """Move the agent's state dir under state/_archive/ (transcripts kept)."""
    src = agent_dir(name)
    if not src.exists():
        return None
    ARCHIVE.mkdir(parents=True, exist_ok=True)
    stamp = datetime.datetime.now().strftime("%Y%m%dT%H%M%S")
    dest = ARCHIVE / f"{name}-{stamp}"
    n = 1
    while dest.exists():
        dest = ARCHIVE / f"{name}-{stamp}-{n}"
        n += 1
    os.replace(src, dest)
    return dest


def _write_workspace_loss_snapshot(rec: AgentRecord, workspace_id: str) -> Path:
    """Preserve identity/context before archiving a hard Herdr workspace loss."""
    ensure_agent_dirs(rec.name)
    path = agent_dir(rec.name) / "WORKSPACE_LOST.json"
    payload: dict[str, object] = {
        "detected_at": now_iso(),
        "workspace_id": workspace_id,
        "agent": rec.to_dict(),
        "recovery": (
            "The Herdr workspace disappeared. Read transcript.log and this saved registry row before "
            "starting a replacement agent. A TUI's visible-only scrollback may be unavailable after "
            "a hard workspace death."
        ),
    }
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2) + "\n")
    os.replace(tmp, path)
    return path


def _reap_workspace_loss(rec: AgentRecord, workspace_id: str) -> str:
    snapshot = _write_workspace_loss_snapshot(rec, workspace_id)
    if rec.mode == HEADLESS_MODE:
        terminate_active_harness(rec)
    dest = archive_state(rec.name)
    saved_snapshot = snapshot if dest is None else dest / snapshot.name
    tail = f"; state archived to {dest}" if dest else ""
    write_event("herdr_workspace_lost", rec.name, preview=last_message_preview(rec.name))
    return (
        f"reaped {rec.name} (Herdr workspace {workspace_id} is gone; hard workspace death may have "
        f"interrupted a turn; recovery snapshot {saved_snapshot}{tail})"
    )


def _within_startup_grace(rec: AgentRecord) -> bool:
    """True if the agent was created so recently its runner may not have
    recorded its pid yet — do not reap it as dead in that window."""
    if rec.runner_pid is not None:
        return False
    try:
        created = datetime.datetime.fromisoformat(rec.created_at)
    except ValueError:
        return False
    age = (datetime.datetime.now().astimezone() - created).total_seconds()
    return age < STARTUP_GRACE_S


def gc() -> list[str]:
    """Reap registry entries whose runner process is truly gone.

    Returns human-readable notes (one per reaped agent) so callers can print a
    visible record — silent reaping would be a No-Silent-Failure violation.
    """
    notes: list[str] = []
    workspace_state: dict[str, Optional[bool]] = {}
    with registry_lock() as agents:
        for name in list(agents):
            rec = agents[name]
            if _within_startup_grace(rec):
                continue
            if rec.backend == "herdr":
                workspace_id = _herdr_workspace_from_tab(rec.tmux_target)
                if workspace_id is not None:
                    if workspace_id not in workspace_state:
                        workspace_state[workspace_id] = _herdr_workspace_exists(workspace_id)
                    exists = workspace_state[workspace_id]
                    if exists is None:
                        notes.append(
                            f"{name} Herdr workspace {workspace_id} probe failed; preserving state "
                            "instead of assuming a workspace death"
                        )
                        continue
                    if not exists:
                        process_alive = (
                            rec.mode == HEADLESS_MODE and runner_identity_alive(rec)
                        )
                        if process_alive:
                            notes.append(
                                f"{name} degraded: Herdr workspace {workspace_id} is gone but runner pid "
                                f"{rec.runner_pid} remains alive; recover its presentation before sending work"
                            )
                            write_event("herdr_workspace_lost", name, preview=last_message_preview(name))
                            continue
                        notes.append(_reap_workspace_loss(rec, workspace_id))
                        del agents[name]
                        continue
            if rec.mode == TUI_MODE:
                if rec.presentation_pane is None:
                    notes.append(f"{name} degraded: TUI registry row has no pane id; preserving state")
                    continue
                probe = _herdr_tui_probe(rec.presentation_pane)
                # A live pane is intentionally never reaped. It may be showing a
                # human's unsaved draft, and a temporary API timeout is not proof
                # that the interactive Codex process died.
                if probe.error is not None:
                    notes.append(f"{name} TUI probe failed; preserving state: {probe.error}")
                    continue
                if probe.pane_alive:
                    if not probe.codex_alive:
                        notes.append(
                            f"{name} degraded: Herdr TUI pane {rec.presentation_pane} is alive but "
                            "Codex is not its foreground process; preserving it for inspection"
                        )
                    continue
                if window_exists(rec):
                    notes.append(
                        f"{name} degraded: Herdr tab {rec.tmux_target} remains but its TUI pane is gone; "
                        "preserving state for inspection"
                    )
                    continue
                dest = archive_state(name)
                del agents[name]
                tail = f"; state archived to {dest}" if dest else ""
                notes.append(f"reaped {name} (Herdr TUI pane is gone){tail}")
                continue
            win = window_exists(rec)
            runner_alive = runner_identity_alive(rec)
            if runner_alive:
                if not win:
                    notes.append(
                        f"{name} degraded: runner pid {rec.runner_pid} is alive but "
                        f"{rec.backend} presentation {rec.tmux_target or name} is missing; "
                        "recreate presentation"
                    )
                continue
            if rec.runner_pid is None:
                reason = "runner pid missing after startup grace"
            else:
                reason = f"runner pid {rec.runner_pid} not alive or reused"
            terminate_active_harness(rec)
            if win:
                kill_window(rec)
            dest = archive_state(name)
            del agents[name]
            tail = f"; state archived to {dest}" if dest else ""
            notes.append(f"reaped {name} ({reason}){tail}")
    return notes


# --------------------------------------------------------------------------- #
# Shared operations used by both V1 scripts and the MCP server
# --------------------------------------------------------------------------- #


def _agent_status(rec: AgentRecord) -> AgentStatus:
    if rec.mode == TUI_MODE:
        if rec.presentation_pane is None:
            probe = TuiProbe(False, False, "probe_error", None, "registry row has no TUI pane id")
        else:
            probe = _herdr_tui_probe(rec.presentation_pane)
            try:
                shared = shared_agent_status(
                    _shared_tui_client(), _shared_tui_target(rec), str(agent_dir(rec.name))
                )
                shared_state = shared.get("agent_status")
                if isinstance(shared_state, str):
                    probe = dataclasses.replace(probe, status=shared_state)
            except SharedHerdrError as exc:
                probe = dataclasses.replace(probe, error=str(exc))
        if probe.error is not None:
            status = f"probe_error: {probe.error}"
        elif probe.status == "working":
            status = "busy"
        elif probe.status in ("idle", "done"):
            status = "idle"
        elif probe.status == "blocked":
            status = "blocked"
        else:
            status = "unknown"
        return AgentStatus(
            name=rec.name,
            harness=rec.harness,
            backend=rec.backend,
            tmux_target=rec.tmux_target,
            cwd=rec.cwd,
            model=rec.model,
            session_id=rec.session_id,
            status=status,
            runner_pid=probe.pid,
            runner_started_at=None if probe.pid is None else pid_start_time(probe.pid),
            next_seq=rec.next_seq,
            created_at=rec.created_at,
            last_turn_at=rec.last_turn_at,
            window_alive=probe.pane_alive,
            runner_alive=probe.codex_alive,
            presentation_degraded=probe.pane_alive and not probe.codex_alive,
            pending=pending_count(rec.name),
            transcript=f"herdr pane {rec.presentation_pane or '<missing>'}",
            last_message_preview="",
            mode=rec.mode,
            presentation_pane=rec.presentation_pane,
        )
    runner_alive = runner_identity_alive(rec)
    win = window_exists(rec)
    return AgentStatus(
        name=rec.name,
        harness=rec.harness,
        backend=rec.backend,
        tmux_target=rec.tmux_target,
        cwd=rec.cwd,
        model=rec.model,
        session_id=rec.session_id,
        status=rec.status,
        runner_pid=rec.runner_pid,
        runner_started_at=rec.runner_started_at,
        next_seq=rec.next_seq,
        created_at=rec.created_at,
        last_turn_at=rec.last_turn_at,
        window_alive=win,
        runner_alive=runner_alive,
        presentation_degraded=runner_alive and not win,
        pending=pending_count(rec.name),
        transcript=str(transcript_path(rec.name)),
        last_message_preview=last_message_preview(rec.name),
        mode=rec.mode,
        presentation_pane=rec.presentation_pane,
        automation_paused=(agent_dir(rec.name) / "automation-paused").exists(),
    )


def status_snapshot(name: Optional[str] = None, *, run_gc: bool = True) -> StatusResult:
    """Return one or all named worker statuses, optionally collecting confirmed dead workers first."""
    notes = gc() if run_gc else []
    reg = read_registry()
    if name is not None:
        require_valid_name(name)
        rec = reg.get(name)
        if rec is None:
            raise AgentOperationError("unknown_agent", f"unknown agent {name!r}")
        reg = {name: rec}
    return StatusResult(agents=[_agent_status(reg[n]) for n in sorted(reg)], gc_notes=notes)


def _child_python_command(script: Path, *args: str) -> str:
    """Carry this runtime's state and policy into presentation-server children."""
    environment = {
        "HERDR_SUBAGENTS_HOME": str(BASE),
        "HERDR_SUBAGENTS_PROJECT_DEFAULTS": str(PROJECT_DEFAULTS_CONFIG),
        "SUBAGENTS_CODEX_BYPASS_PERMISSIONS": "1" if _configured_codex_bypass_permissions() else "0",
    }
    if os.environ.get("HERDR_SUBAGENTS_POLICY"):
        environment["HERDR_SUBAGENTS_POLICY"] = str(Path(os.environ["HERDR_SUBAGENTS_POLICY"]).expanduser().resolve())
    for key in ("CODEX_BIN", "AGY_BIN", "MUSE_BIN", "HERDR_BIN", "CODEX_HOME", "SUBAGENT_EFFORT", "SUBAGENTS_TMUX_SESSION", "SUBAGENTS_HERDR_WORKSPACE", "SUBAGENTS_TURN_TIMEOUT"):
        if key in os.environ:
            environment[key] = os.environ[key]
    prefix = " ".join(f"{key}={shlex.quote(value)}" for key, value in environment.items())
    return f"env {prefix} {shlex.join([sys.executable, str(script), *args])}"


def runner_wrapper(name: str, *, stage_token: Optional[str] = None) -> str:
    """Shell command used by every backend to host a runner visibly."""
    inner = _child_python_command(RUNNER, name)
    if stage_token is not None:
        inner = f"SUBAGENTS_MIGRATION_STAGE={shlex.quote(stage_token)} {inner}"
    stderr = shlex.quote(str(runner_stderr_path(name)))
    return (
        f"{inner} 2>>{stderr}; ec=$?; echo '[subagents] runner exited rc='$ec >>{stderr}; echo; "
        f"echo '[subagents] runner for {name} exited rc='$ec' - presentation kept for "
        f"inspection; run agent_down.py {name} to reap'; exec sleep infinity"
    )


def bring_up_agent(
    name: str,
    *,
    cwd: str,
    brief: Optional[str],
    model: Optional[str] = None,
    harness: str = DEFAULT_HARNESS,
    backend: Optional[str] = None,
    mode: Optional[str] = None,
    purpose: str = "development",
    harness_args: Sequence[str] = (),
) -> UpResult:
    """Validate policy and launch a named worker, optionally enqueueing its first task."""
    harness = require_supported_harness(harness)
    if any(not isinstance(item, str) or not item or "\0" in item for item in harness_args):
        raise AgentOperationError(
            "invalid_harness_arguments",
            "harness arguments must be nonempty NUL-free strings",
        )
    if harness != "muse" and harness_args:
        raise AgentOperationError(
            "unsupported_harness_arguments",
            "headless harness arguments currently apply only to Muse",
        )
    bypass_permissions = _configured_codex_bypass_permissions()
    backend = selected_backend(backend)
    mode = selected_mode(harness, mode, backend=backend)
    if mode == TUI_MODE and backend != "herdr":
        raise AgentOperationError("tui_requires_herdr", "--mode tui requires the herdr backend")
    if mode == TUI_MODE and harness != "codex":
        raise AgentOperationError(
            "tui_harness_unsupported",
            "--mode tui currently supports the codex harness only; the registry seam reserves other TUIs",
        )
    if not backend_available(backend):
        raise AgentOperationError(
            f"{backend}_missing",
            f"{backend} is selected but is not available; check its binary and running server",
        )
    valid_name = require_valid_name(name)
    root = Path(cwd).expanduser().resolve()
    if not root.is_dir():
        raise AgentOperationError("bad_cwd", f"cwd is not a directory: {root}")
    require_harness_quota(harness, model, purpose)
    if harness == "agy":
        require_antigravity_preflight(root)

    notes = gc()
    with registry_lock() as agents:
        if valid_name in agents:
            raise AgentOperationError(
                "agent_exists",
                f"agent {valid_name!r} already active; subagent_down it first",
            )
        if orphan_window_exists(backend, valid_name):
            raise AgentOperationError(
                "orphan_window",
                f"{backend} presentation for {valid_name!r} already exists with no registry row",
            )
        agents[valid_name] = AgentRecord(
            name=valid_name,
            harness=harness,
            backend=backend,
            tmux_target=f"{TMUX_SESSION}:{valid_name}",
            cwd=str(root),
            model=model,
            session_id=str(uuid.uuid4()) if harness == "muse" and mode == HEADLESS_MODE else None,
            status="starting",
            runner_pid=None,
            runner_started_at=None,
            next_seq=0,
            created_at=now_iso(),
            last_turn_at=None,
            mode=mode,
            codex_bypass_permissions=bypass_permissions,
            harness_args=tuple(harness_args),
        )

    ensure_agent_dirs(valid_name)
    try:
        if mode == TUI_MODE:
            target, pane_id, probe = _launch_herdr_tui(valid_name, str(root), model, bypass_permissions=bypass_permissions)
        else:
            target = launch_window(backend, valid_name, str(root), runner_wrapper(valid_name))
            pane_id = None
            probe = None
    except Exception as exc:
        with registry_lock() as agents:
            agents.pop(valid_name, None)
        raise AgentOperationError(
            f"{backend}_launch_failed",
            f"failed to launch {backend} presentation for {valid_name!r}: {exc}",
        ) from exc
    with registry_lock() as agents:
        rec = agents.get(valid_name)
        if rec is not None:
            rec.tmux_target = target
            rec.presentation_pane = pane_id
            if probe is not None:
                rec.runner_pid = probe.pid
                rec.runner_started_at = None if probe.pid is None else pid_start_time(probe.pid)
                rec.status = "idle" if probe.status in ("idle", "done") else "busy"

    queued_turn: Optional[int] = None
    if brief is not None:
        queued_turn = enqueue_message(valid_name, brief, model=None)
        if mode == TUI_MODE:
            _deliver_tui_messages(read_registry()[valid_name])
    write_event("agent_up", valid_name, preview=last_message_preview(valid_name))
    rec = read_registry()[valid_name]
    return UpResult(agent=_agent_status(rec), queued_turn=queued_turn, gc_notes=notes)


def _require_live_agent(name: str) -> AgentRecord:
    valid_name = require_valid_name(name)
    reg = read_registry()
    rec = reg.get(valid_name)
    if rec is None:
        raise AgentOperationError(
            "unknown_agent",
            f"unknown agent {valid_name!r}; run subagent_status to list live agents",
        )
    if rec.mode == TUI_MODE:
        if rec.presentation_pane is None:
            raise AgentOperationError("tui_pane_missing", f"TUI agent {valid_name!r} has no recorded Herdr pane")
        probe = _herdr_tui_probe(rec.presentation_pane)
        if probe.error is not None:
            raise AgentOperationError(
                "tui_probe_failed",
                f"could not confirm TUI agent {valid_name!r} within {HERDR_TUI_PROBE_TIMEOUT_S:g}s: {probe.error}",
            )
        if not probe.codex_alive:
            raise AgentOperationError(
                "dead_tui",
                f"TUI agent {valid_name!r} has no live Codex process in pane {rec.presentation_pane}",
            )
        return rec
    if runner_identity_alive(rec):
        return rec
    if rec.runner_pid is None and _within_startup_grace(rec) and window_exists(rec):
        return rec
    if rec.runner_pid is None:
        raise AgentOperationError(
            "dead_runner",
            f"agent {valid_name!r} has no runner pid after startup grace",
        )
    if not runner_identity_alive(rec):
        raise AgentOperationError(
            "dead_runner",
            f"agent {valid_name!r} runner pid {rec.runner_pid} is not alive or was reused",
        )
    return rec


def _read_tui_recent_scrollback(pane_id: str) -> str:
    """Return the bounded scrollback used for startup-prompt diagnosis."""
    payload = _herdr(
        "agent",
        "read",
        pane_id,
        "--source",
        "recent",
        "--lines",
        str(TUI_READ_LINES),
        "--format",
        "text",
        timeout_s=HERDR_TUI_PROBE_TIMEOUT_S,
    )
    raw_read = payload.get("read")
    text = raw_read.get("text") if isinstance(raw_read, dict) else None
    if not isinstance(text, str):
        raise AgentOperationError("herdr_api_invalid", "Herdr did not return TUI scrollback text")
    return text


def _tui_scrollback_tail(text: str, limit: int = 480) -> str:
    """Keep timeout diagnostics useful without putting a whole TUI in events."""
    compact = " ".join(line.strip() for line in text.splitlines() if line.strip())
    return compact[-limit:] if compact else "<empty scrollback>"


def _accept_tui_resume_startup_prompt(
    pane_id: str, *, accepted_directory: bool, accepted_model: bool
) -> Optional[str]:
    """Accept one known resumed-session startup prompt without human input.

    A session id alone does not suppress this prompt when Codex's recorded
    working directory differs from the newly hosted pane. Reading scrollback
    on each bounded readiness pass lets a long resume reach the prompt at any
    time, instead of timing out after a fixed startup delay. A model mismatch
    warning may follow it on Codex versions that render the warning as a modal;
    Enter accepts its default continuation too.
    """
    text = _read_tui_recent_scrollback(pane_id)
    if not accepted_directory and TUI_RESUME_DIRECTORY_PROMPT in text:
        _herdr_action("pane", "send-keys", pane_id, HERDR_KEY_ENTER)
        return "directory"
    if not accepted_model and TUI_RESUME_MODEL_PROMPT in text:
        _herdr_action("pane", "send-keys", pane_id, HERDR_KEY_ENTER)
        return "model"
    return None


def _wait_for_tui_prompt(
    rec: AgentRecord,
    *,
    timeout_s: float = TUI_DELIVERY_WAIT_S,
    handle_resume_startup: bool = False,
) -> None:
    """Wait, in bounded Herdr slices, until the interactive composer is safe to submit."""
    assert rec.presentation_pane is not None
    deadline = time.monotonic() + timeout_s
    started_at = time.monotonic()
    next_progress_at = started_at
    resume_directory_accepted = False
    resume_model_accepted = False
    while True:
        probe = _herdr_tui_probe(rec.presentation_pane)
        if probe.error is not None:
            raise AgentOperationError("tui_probe_failed", probe.error)
        if not probe.codex_alive:
            raise AgentOperationError(
                "dead_tui", f"TUI agent {rec.name!r} no longer has a live Codex process"
            )
        startup_prompt = (
            _accept_tui_resume_startup_prompt(
                rec.presentation_pane,
                accepted_directory=resume_directory_accepted,
                accepted_model=resume_model_accepted,
            )
            if handle_resume_startup
            else None
        )
        if startup_prompt is not None:
            if startup_prompt == "directory":
                resume_directory_accepted = True
                write_event("tui_resume_directory_accepted", rec.name)
            else:
                resume_model_accepted = True
                write_event("tui_resume_model_warning_accepted", rec.name)
            # Give Codex a moment to redraw the now-unblocked TUI before the
            # next status probe. The normal deadline still bounds this path.
            time.sleep(0.1)
            continue
        now = time.monotonic()
        if handle_resume_startup and now >= next_progress_at:
            try:
                scrollback_tail = _tui_scrollback_tail(_read_tui_recent_scrollback(rec.presentation_pane))
            except AgentOperationError as exc:
                scrollback_tail = f"<scrollback unavailable: {exc.message}>"
            write_event(
                "tui_resume_wait_progress",
                rec.name,
                preview=(
                    f"elapsed={now - started_at:.1f}s status={probe.status}; "
                    f"scrollback={scrollback_tail}"
                ),
            )
            next_progress_at = now + TUI_MIGRATION_PROGRESS_INTERVAL_S
        # Herdr reports an unseen completion in a background tab as `done`.
        # That is a safe composer just like `idle`; waiting only for idle here
        # strands a successfully resumed background TUI until this deadline.
        if probe.status in ("idle", "done"):
            return
        if probe.status == "blocked":
            try:
                scrollback_tail = _tui_scrollback_tail(_read_tui_recent_scrollback(rec.presentation_pane))
            except AgentOperationError as exc:
                scrollback_tail = f"<scrollback unavailable: {exc.message}>"
            raise AgentOperationError(
                "tui_blocked",
                f"TUI agent {rec.name!r} is blocked; resolve its visible prompt before coordinator delivery; "
                f"scrollback tail: {scrollback_tail}",
            )
        remaining = deadline - now
        if remaining <= 0:
            try:
                scrollback_tail = _tui_scrollback_tail(_read_tui_recent_scrollback(rec.presentation_pane))
            except AgentOperationError as exc:
                scrollback_tail = f"<scrollback unavailable: {exc.message}>"
            write_event(
                "tui_resume_readiness_timeout",
                rec.name,
                preview=(
                    f"elapsed={now - started_at:.1f}s status={probe.status}; "
                    f"scrollback={scrollback_tail}"
                ),
            )
            raise AgentOperationError(
                "tui_not_idle",
                f"TUI agent {rec.name!r} did not become idle within {timeout_s:g}s; "
                "the message remains queued for this bounded delivery attempt; "
                f"last Herdr status={probe.status}; scrollback tail: {scrollback_tail}",
            )
        wait_ms = max(1, min(1000, int(remaining * 1000)))
        try:
            _herdr(
                "agent",
                "wait",
                rec.presentation_pane,
                "--until",
                "idle",
                "--timeout",
                str(wait_ms),
                timeout_s=(wait_ms / 1000) + HERDR_TUI_PROBE_TIMEOUT_S,
            )
        except AgentOperationError:
            # Herdr returns non-zero for a timeout. Re-probe above to distinguish
            # that normal race from a real lost pane or blocked prompt.
            pass


def _deliver_tui_messages(rec: AgentRecord) -> list[int]:
    """Thin named-agent adapter around agent-utils' one delivery state machine."""
    assert rec.presentation_pane is not None
    # Per-message model selection is a named-subagent feature the TUI cannot honor. Handle that
    # adapter-only policy before handing the remaining FIFO to the shared transport.
    quarantined: list[int] = []
    while True:
        pending = next_pending_message(rec.name)
        if pending is None:
            return quarantined
        message = Message.from_path(pending)
        if message.model is None:
            break
        exc = AgentOperationError(
            "tui_model_override_unsupported",
            "per-message --model is not supported for an interactive TUI; use /model in its tab",
        )
        quarantined.append(_quarantine_tui_message(rec.name, pending, exc))

    try:
        result = shared_agent_drain(
            _shared_tui_client(),
            _shared_tui_target(rec),
            str(agent_dir(rec.name)),
            ready_timeout=TUI_DELIVERY_WAIT_S,
            working_timeout=TUI_SUBMIT_WORKING_TIMEOUT_MS / 1000,
            max_attempts=TUI_DELIVERY_MAX_ATTEMPTS,
        )
    except SharedHerdrError as exc:
        raise AgentOperationError("tui_delivery_failed", str(exc)) from exc
    for identifier in result.delivered:
        seq = int(identifier) if identifier.isdigit() else -1
        with registry_lock() as agents:
            current = agents.get(rec.name)
            if current is not None:
                current.status = "busy"
                current.last_turn_at = now_iso()
        write_event("tui_turn_submitted", rec.name, seq=seq)
    for identifier in result.quarantined:
        seq = int(identifier) if identifier.isdigit() else -1
        quarantined.append(seq)
        write_event(
            "tui_message_quarantined",
            rec.name,
            seq=seq,
            preview="shared delivery marked the prompt possibly submitted; inspect failed artifact",
        )
    if result.blocked is not None:
        raise AgentOperationError(
            "tui_not_idle",
            f"TUI agent {rec.name!r} delivery remains queued without consuming a retry: {result.blocked}",
        )
    return quarantined


def _clear_tui_composer(rec: AgentRecord) -> None:
    """Reject unsafe automatic clearing of a human-owned TUI draft.

    Herdr exposes no input-state API. ``delete`` is unsupported, while
    ``ctrl+c`` can terminate an unseen/background Codex TUI instead of merely
    clearing its draft. Coordinator delivery therefore never destroys a draft
    it cannot prove is empty.
    """
    raise AgentOperationError(
        "tui_composer_clear_unsupported",
        f"cannot safely clear TUI agent {rec.name!r}'s visible composer; Herdr has no "
        "draft-state API. Ask the human to clear or submit their draft, then retry the inbox.",
    )


def _submit_tui_text(
    rec: AgentRecord,
    text: str,
    *,
    ready_timeout_s: float = TUI_DELIVERY_WAIT_S,
    working_timeout_ms: int = TUI_SUBMIT_WORKING_TIMEOUT_MS,
    handle_resume_startup: bool = False,
) -> None:
    """Inject and submit one message only after the TUI is at a safe prompt."""
    assert rec.presentation_pane is not None
    if handle_resume_startup:
        _wait_for_tui_prompt(rec, timeout_s=ready_timeout_s, handle_resume_startup=True)
    try:
        shared_agent_send(
            _shared_tui_client(),
            _shared_tui_target(rec),
            str(agent_dir(rec.name)),
            text,
            ready_timeout=ready_timeout_s,
            working_timeout=working_timeout_ms / 1000,
            max_attempts=TUI_DELIVERY_MAX_ATTEMPTS,
        )
    except SharedHerdrError as exc:
        raise AgentOperationError("tui_delivery_failed", str(exc)) from exc


def drain_tui_inbox(name: str, *, clear_composer: bool = False) -> TuiInboxResult:
    """Retry a TUI inbox after optionally clearing a stale human-visible draft."""
    rec = _require_live_agent(require_valid_name(name))
    if rec.mode != TUI_MODE:
        raise AgentOperationError("not_tui_agent", f"agent {rec.name!r} is not an interactive TUI")
    if clear_composer:
        _wait_for_tui_prompt(rec)
        _clear_tui_composer(rec)
        write_event("tui_composer_cleared", rec.name)
    _deliver_tui_messages(rec)
    return tui_inbox_snapshot(rec.name)


def send_message_to_agent(name: str, text: str, *, model: Optional[str] = None) -> SendResult:
    """Queue a prompt for a live worker and attempt bounded delivery for an interactive TUI."""
    if text == "":
        raise AgentOperationError("empty_message", "message must not be empty")
    rec = _require_live_agent(name)
    if rec.mode == TUI_MODE and model is not None:
        raise AgentOperationError(
            "tui_model_override_unsupported",
            "per-message --model is not supported for an interactive TUI; use /model in its tab",
        )
    seq = enqueue_message(rec.name, text, model=model)
    quarantined: list[int] = []
    if rec.mode == TUI_MODE:
        quarantined = _deliver_tui_messages(rec)
    if seq in quarantined:
        raise AgentOperationError(
            "tui_message_quarantined",
            f"message {seq} for TUI agent {rec.name!r} failed after {TUI_DELIVERY_MAX_ATTEMPTS} "
            f"attempts and was moved to {failed_dir(rec.name)}; later queued messages were allowed to continue",
        )
    return SendResult(
        name=rec.name,
        seq=seq,
        model=model,
        transcript=str(transcript_path(rec.name)),
        mode=rec.mode,
        presentation_pane=rec.presentation_pane,
        quarantined=quarantined,
    )


def reset_agent_context(name: str) -> ResetResult:
    """Clear one idle agent's harness session without disturbing its warm slot.

    Resetting during a running or queued turn could let that turn write its old
    session id back after the reset, so callers must reset at a task boundary.
    """
    valid_name = require_valid_name(name)
    with registry_lock() as agents:
        rec = agents.get(valid_name)
        if rec is None:
            raise AgentOperationError("unknown_agent", f"unknown agent {valid_name!r}")
        if rec.mode == TUI_MODE:
            raise AgentOperationError(
                "tui_reset_unsupported",
                "interactive TUI context is owned by its visible Codex process; use /new or /resume in that tab",
            )
        if rec.status == "busy" or pending_count(valid_name) > 0:
            raise AgentOperationError(
                "agent_busy",
                f"agent {valid_name!r} has an active or queued turn; reset it at an idle task boundary",
            )
        previous_session_id = rec.session_id
        rec.session_id = str(uuid.uuid4()) if rec.harness == "muse" else None
        transcript = transcript_path(valid_name)
        transcript.parent.mkdir(parents=True, exist_ok=True)
        with transcript.open("a") as fh:
            fh.write("=== CONTEXT RESET ===\n")
    return ResetResult(
        name=valid_name,
        previous_session_id=previous_session_id,
        transcript=str(transcript),
    )


_READ_MODES: tuple[str, ...] = ("all", "last", "since_turn", "tail")


def _validate_read_request(mode: str, since_turn: Optional[int], tail: Optional[int]) -> None:
    """Validate `mode` and its required parameter pairing ONCE, before the
    TUI/headless backend branch in `read_agent_output`.

    the original bug was the TUI backend silently substituting
    arbitrary scrollback for an unsupported mode while still reporting the
    CALLER's requested mode back -- a silent contract violation. The fix
    added backend-specific rejections (mode="last" and since_turn=N are
    unsupported for TUI), but those checks ran only after this validation
    would have happened for the headless path -- so a malformed call the
    headless path already rejects loudly (e.g. mode="since_turn" with
    since_turn omitted, or an unrecognized mode string) fell through the
    TUI branch's narrower checks and hit the exact same silent-substitution
    bug in a corner the backend-specific guards don't cover. Centralizing
    the enum + pairing check here, shared by both backends, closes that
    corner instead of leaving it to whichever backend happens to validate
    harder.
    """
    if mode not in _READ_MODES:
        raise AgentOperationError(
            "bad_read_mode",
            f"mode must be one of: {', '.join(_READ_MODES)}",
        )
    if mode == "since_turn" and since_turn is None:
        raise AgentOperationError("bad_read_mode", "since_turn mode requires since_turn")
    if mode == "tail" and tail is None:
        raise AgentOperationError("bad_read_mode", "tail mode requires tail")


def read_agent_output(
    name: str,
    *,
    mode: str = "all",
    since_turn: Optional[int] = None,
    tail: Optional[int] = None,
) -> ReadResult:
    """Read durable headless turn output or bounded TUI scrollback, rejecting unsupported boundaries."""
    valid_name = require_valid_name(name)
    _validate_read_request(mode, since_turn, tail)
    reg = read_registry()
    rec = reg.get(valid_name)
    if rec is not None and rec.mode == TUI_MODE:
        if rec.presentation_pane is None:
            raise AgentOperationError("tui_pane_missing", f"TUI agent {valid_name!r} has no recorded Herdr pane")
        # mode="last" and since_turn=N both claim a precision the TUI
        # backend cannot deliver -- there is no durable turn-boundary capture
        # to identify "the last answer" or "everything since turn N" against,
        # only a plain scrollback tail. Reject loudly rather than silently
        # substituting arbitrary recent scrollback while still reporting the
        # requested mode back to the caller (the exact contract violation
        # this issue reports: a coordinator asking for "since turn 12" got a
        # generic 500-line screen instead, with no indication it wasn't
        # honored). "all" and "tail" are the only modes the TUI backend can
        # honestly support, since both just mean "recent scrollback" here.
        if since_turn is not None:
            raise AgentOperationError(
                "tui_since_turn_unsupported",
                f"TUI agent {valid_name!r} has no durable turn-boundary capture; "
                "since_turn cannot be honored for an interactive TUI backend. "
                "Use mode=\"tail\" (optionally with an explicit `tail` line count) instead.",
            )
        if mode == "last":
            raise AgentOperationError(
                "tui_last_unsupported",
                f"TUI agent {valid_name!r} cannot identify \"the last answer\" distinctly from "
                "scrollback; mode=\"last\" is not supported for an interactive TUI backend. "
                "Use mode=\"tail\" (optionally with an explicit `tail` line count) instead.",
            )
        read_lines = TUI_READ_LINES if tail is None else tail
        if read_lines < 0:
            raise AgentOperationError("bad_read_mode", "tail must be non-negative")
        if read_lines > TUI_MAX_TAIL_LINES:
            raise AgentOperationError(
                "tui_tail_too_large",
                f"tail={read_lines} exceeds the TUI backend's cap of {TUI_MAX_TAIL_LINES} lines "
                "(SUBAGENTS_TUI_MAX_TAIL_LINES); request a smaller tail.",
            )
        try:
            text = shared_agent_read(
                _shared_tui_client(), _shared_tui_target(rec), lines=read_lines
            )
        except SharedHerdrError as exc:
            raise AgentOperationError("herdr_api_invalid", str(exc)) from exc
        if text:
            return ReadResult(name=valid_name, mode=mode, text=text)
        fallback = _herdr(
            "agent",
            "read",
            rec.presentation_pane,
            "--source",
            "recent",
            "--lines",
            str(read_lines),
            "--format",
            "text",
            timeout_s=HERDR_TUI_PROBE_TIMEOUT_S,
        )
        fallback_read = fallback.get("read")
        fallback_text = fallback_read.get("text") if isinstance(fallback_read, dict) else None
        if not isinstance(fallback_text, str):
            raise AgentOperationError(
                "herdr_api_invalid", "Herdr did not return fallback TUI scrollback text"
            )
        write_event("tui_scrollback_fallback", valid_name, preview="recent-unwrapped empty; used recent")
        return ReadResult(name=valid_name, mode=mode, text=fallback_text)
    if rec is None and not transcript_path(valid_name).exists():
        raise AgentOperationError(
            "unknown_agent",
            f"unknown agent {valid_name!r} and no archived transcript",
        )
    if mode == "last":
        p = last_message_path(valid_name)
        if not p.exists():
            raise AgentOperationError(
                "no_completed_turn",
                f"no completed turn yet for {valid_name!r} (no last-message.txt)",
            )
        return ReadResult(name=valid_name, mode=mode, text=p.read_text())
    if mode == "since_turn":
        if since_turn is None:
            raise AgentOperationError("bad_read_mode", "since_turn mode requires since_turn")
        p = transcript_path(valid_name)
        if not p.exists():
            raise AgentOperationError("no_transcript", f"no transcript yet for {valid_name!r}")
        marker = f"===TURN {since_turn} START"
        lines = p.read_text().splitlines()
        for i, line in enumerate(lines):
            if line.startswith(marker):
                return ReadResult(name=valid_name, mode=mode, text="\n".join(lines[i:]) + "\n")
        raise AgentOperationError(
            "turn_not_found",
            f"turn {since_turn} not found in {valid_name!r} transcript",
        )
    if mode == "tail":
        if tail is None:
            raise AgentOperationError("bad_read_mode", "tail mode requires tail")
        if tail < 0:
            raise AgentOperationError("bad_read_mode", "tail must be non-negative")
        p = transcript_path(valid_name)
        if not p.exists():
            raise AgentOperationError("no_transcript", f"no transcript yet for {valid_name!r}")
        lines = p.read_text().splitlines()
        selected = [] if tail == 0 else lines[-tail:]
        return ReadResult(name=valid_name, mode=mode, text="\n".join(selected) + "\n")
    if mode != "all":
        raise AgentOperationError(
            "bad_read_mode",
            "mode must be one of: all, last, since_turn, tail",
        )
    p = transcript_path(valid_name)
    if not p.exists():
        raise AgentOperationError("no_transcript", f"no transcript yet for {valid_name!r}")
    return ReadResult(name=valid_name, mode=mode, text=p.read_text())


def bring_down_agent(
    name: str, *, grace: float = 10.0, archive: bool = True, force: bool = False
) -> DownResult:
    """Retire an agent, optionally without probing an unavailable backend.

    Force mode remains deliberately narrow: it can stop a local headless runner,
    but never contacts or claims to close the presentation backend. The caller
    receives the unverified presentation identity so the lost verification is
    visible in both the command output and the event log.
    """
    valid_name = require_valid_name(name)
    reg = read_registry()
    rec = reg.get(valid_name)
    was_registered = rec is not None
    if rec is not None:
        if rec.mode == HEADLESS_MODE:
            stop_path(valid_name).write_text(now_iso() + "\n")
            deadline = time.monotonic() + grace
            while time.monotonic() < deadline:
                cur = read_registry().get(valid_name)
                if cur is None or not runner_identity_alive(cur):
                    break
                time.sleep(0.2)
            cur = read_registry().get(valid_name)
            if cur is not None:
                terminate_runner(cur)

    unverified_presentation: Optional[str] = None
    if force and rec is not None:
        presentation = rec.presentation_pane if rec.mode == TUI_MODE else rec.tmux_target
        unverified_presentation = f"{rec.backend} presentation {presentation or rec.name}"
        killed_window = False
    else:
        killed_window = rec is not None and (
            _herdr_tab_exists(rec.tmux_target) if rec.mode == TUI_MODE else window_exists(rec)
        )
        if killed_window:
            assert rec is not None
            kill_window(rec)
    if rec is not None and rec.backend == "herdr" and (
        rec.mode == HEADLESS_MODE or (agent_dir(rec.name) / "legacy-tmux-guard").exists()
    ):
        remove_legacy_tmux_guard(rec.name)

    preview = last_message_preview(valid_name)
    archived_to: Optional[str] = None
    state_path: Optional[str] = None
    if archive:
        dest = archive_state(valid_name)
        archived_to = None if dest is None else str(dest)
    else:
        p = agent_dir(valid_name)
        state_path = str(p) if p.exists() else None
    with registry_lock() as agents:
        agents.pop(valid_name, None)
    if not archive:
        _clear_presentation_identity(valid_name)
    if was_registered or killed_window or archived_to is not None or state_path is not None:
        event = "agent_down_forced" if force else "agent_down"
        write_event(event, valid_name, preview=preview)
    return DownResult(
        name=valid_name,
        killed_window=killed_window,
        archived_to=archived_to,
        state_path=state_path,
        was_registered=was_registered,
        forced=force,
        unverified_presentation=unverified_presentation,
    )


def _restore_legacy_tmux_runner(name: str, old: AgentRecord) -> RunnerIdentity:
    """Replace a stopped pre-handshake tmux runner and prove its registry update."""
    if old.backend != "tmux":
        raise AgentOperationError(
            "legacy_restore_unsupported",
            f"legacy rollback only supports tmux sources, not {old.backend}",
        )
    if window_exists(old):
        kill_window(old)
    target = launch_window("tmux", name, old.cwd, runner_wrapper(name))
    restored = wait_for_restarted_runner(
        name, RunnerIdentity(old.runner_pid or -1, old.runner_started_at)
    )
    if restored is None:
        raise AgentOperationError(
            "legacy_restore_unconfirmed",
            f"tmux presentation {target} was created but its replacement runner did not publish "
            f"a live identity within {MIGRATION_READY_TIMEOUT_S:g}s",
        )
    return restored


def _restore_migration_record(
    name: str, old: AgentRecord, destination: Optional[RunnerIdentity],
    backend: str, target: Optional[str],
) -> bool:
    with registry_lock() as agents:
        current = agents.get(name)
        if current is None:
            raise AgentOperationError("migration_source_changed", "worker disappeared during migration rollback")
        actual = (current.backend, current.tmux_target, current.runner_pid, current.runner_started_at)
        source = (old.backend, old.tmux_target, old.runner_pid, old.runner_started_at)
        if actual == source:
            return False
        expected = None if destination is None else (backend, target, destination.pid, destination.started_at)
        if actual != expected:
            raise AgentOperationError("migration_source_changed", "registry identity changed during migration rollback")
        current.backend = old.backend
        current.tmux_target = old.tmux_target
        current.runner_pid = old.runner_pid
        current.runner_started_at = old.runner_started_at
        # Preserve sequence allocation and other metadata written while activation waited.
        return True


def _migrate_legacy_tmux_source(name: str, old: AgentRecord, to_backend: str) -> MigrationResult:
    """Migrate a pre-pause-handshake tmux runner with restart-on-failure safety."""
    assert old.runner_pid is not None
    old_identity = RunnerIdentity(old.runner_pid, old.runner_started_at)
    with registry_lock() as agents:
        current = agents.get(name)
        if current is None or not runner_identity_alive(current):
            raise AgentOperationError("dead_runner", f"legacy source runner for {name!r} is no longer alive")
        if current.status != "idle" or pending_count(name) > 0:
            raise AgentOperationError(
                "agent_busy",
                f"legacy source {name!r} is no longer idle with an empty inbox; refusing to stop it",
            )
        if (
            current.backend != old.backend
            or current.tmux_target != old.tmux_target
            or current.runner_pid != old_identity.pid
            or current.runner_started_at != old_identity.started_at
        ):
            raise AgentOperationError(
                "migration_source_changed",
                f"legacy source {name!r} changed before it could be stopped",
            )
    if not terminate_runner(old):
        raise AgentOperationError(
            "legacy_source_stop_failed",
            f"legacy source tmux runner pid {old_identity.pid} did not stop; no destination was started",
        )

    token = secrets.token_hex(16)
    new_target: Optional[str] = None
    staged_identity: Optional[RunnerIdentity] = None
    try:
        new_target = launch_window(
            to_backend, name, old.cwd, runner_wrapper(name, stage_token=token)
        )
        staged_identity = wait_for_staged_runner(name, token)
        if staged_identity is None:
            raise AgentOperationError(
                "migration_destination_unconfirmed",
                f"{to_backend} presentation {new_target} was created, but no live staged runner identity "
                f"was recorded within {MIGRATION_READY_TIMEOUT_S:g}s",
            )
        with registry_lock() as agents:
            current = agents.get(name)
            if current is None:
                raise AgentOperationError("unknown_agent", f"agent {name!r} disappeared during migration")
            if pending_count(name) > 0:
                raise AgentOperationError(
                    "migration_message_queued",
                    "a message was queued after the legacy source stopped; destination was not committed",
                )
            if (
                current.backend != old.backend
                or current.tmux_target != old.tmux_target
                or current.runner_pid != old_identity.pid
                or current.runner_started_at != old_identity.started_at
            ):
                raise AgentOperationError(
                    "migration_source_changed",
                    "legacy source registry changed before destination commit; refusing the migration",
                )
            current.backend = to_backend
            current.tmux_target = new_target
            current.runner_pid = staged_identity.pid
            current.runner_started_at = staged_identity.started_at
            staged_runner_activate_path(name, token).write_text(now_iso() + "\n")
            session_id = current.session_id
        if not wait_for_staged_runner_activation(name, token):
            raise AgentOperationError(
                "migration_activation_unconfirmed",
                "staged destination did not acknowledge activation before marker cleanup",
            )
    except Exception as exc:
        cleanup_errors: list[str] = []
        if staged_identity is not None:
            terminate_runner_identity(staged_identity)
        if new_target is not None:
            try:
                kill_window(dataclasses.replace(old, backend=to_backend, tmux_target=new_target))
            except Exception as cleanup_exc:
                cleanup_errors.append(f"destination cleanup failed: {cleanup_exc}")
        try:
            _restore_migration_record(name, old, staged_identity, to_backend, new_target)
        except Exception as restore_exc:
            raise AgentOperationError(
                "legacy_migration_rollback_failed", f"destination failed ({exc}); source registry restoration failed: {restore_exc}"
            ) from exc
        clear_migration_markers(name, token)
        try:
            restored = _restore_legacy_tmux_runner(name, old)
        except Exception as restore_exc:
            cleanup_errors.append(f"tmux restoration failed: {restore_exc}")
            restored = None
        if cleanup_errors:
            raise AgentOperationError(
                "legacy_migration_rollback_failed",
                f"legacy source was stopped and destination failed ({exc}); "
                + "; ".join(cleanup_errors),
            ) from exc
        assert restored is not None
        raise AgentOperationError(
            "legacy_migration_destination_failed",
            f"legacy source was stopped, destination was rolled back ({exc}), and tmux runner pid "
            f"{restored.pid} was restored",
        ) from exc

    try:
        kill_window(old)
    except Exception as exc:
        raise AgentOperationError(
            "migration_source_cleanup_failed",
            f"destination runner pid {staged_identity.pid} was committed, but closing legacy tmux "
            f"presentation failed: {exc}",
        ) from exc
    if to_backend == "herdr":
        create_legacy_tmux_guard(name, old.cwd, target=old.tmux_target)
    clear_migration_markers(name, token)
    write_event("agent_migrated", name, preview=last_message_preview(name))
    return MigrationResult(
        name=name,
        from_backend=old.backend,
        to_backend=to_backend,
        from_mode=old.mode,
        to_mode=old.mode,
        tmux_target=new_target,
        session_id=session_id,
        transcript=str(transcript_path(name)),
    )


def _confirm_resumed_tui(rec: AgentRecord) -> None:
    """Prove a resumed TUI is alive and can answer before retiring its runner."""
    token = secrets.token_hex(12)
    expected = token[::-1]
    prompt = (
        "Migration health check. Reply with only the reverse of this token: "
        f"{token}"
    )
    # Large historic sessions can spend substantial time rebuilding before the
    # composer is genuinely usable. Do not reuse the normal coordinator submit
    # confirmation timeout: wait for Herdr's idle barrier, then allow a
    # generous bounded working-state transition before declaring migration
    # unsafe.
    _submit_tui_text(
        rec,
        prompt,
        ready_timeout_s=TUI_MIGRATION_READY_TIMEOUT_S,
        working_timeout_ms=int(TUI_MIGRATION_READY_TIMEOUT_S * 1000),
        handle_resume_startup=True,
    )
    assert rec.presentation_pane is not None
    deadline = time.monotonic() + TUI_MIGRATION_CONFIRM_TIMEOUT_S
    while time.monotonic() < deadline:
        probe = _herdr_tui_probe(rec.presentation_pane)
        if probe.error is not None:
            raise AgentOperationError("tui_migration_probe_failed", probe.error)
        if not probe.codex_alive:
            raise AgentOperationError(
                "tui_migration_destination_died",
                "resumed Codex TUI died before it answered the migration health check",
            )
        payload = _herdr(
            "agent",
            "read",
            rec.presentation_pane,
            "--source",
            "recent",
            "--lines",
            str(TUI_READ_LINES),
            "--format",
            "text",
            timeout_s=HERDR_TUI_PROBE_TIMEOUT_S,
        )
        raw_read = payload.get("read")
        text = raw_read.get("text") if isinstance(raw_read, dict) else None
        if isinstance(text, str) and expected in text:
            return
        time.sleep(0.2)
    raise AgentOperationError(
        "tui_migration_response_unconfirmed",
        f"resumed Codex TUI stayed live but did not answer its health check within "
        f"{TUI_MIGRATION_CONFIRM_TIMEOUT_S:g}s",
    )


def _resume_source_intake(name: str, old: AgentRecord) -> None:
    """Remove pause markers and prove a surviving headless runner can poll again."""
    clear_migration_markers(name, "")
    markers = (migration_pause_path(name), migration_pause_ack_path(name))
    remaining = [str(path) for path in markers if path.exists()]
    if remaining:
        raise AgentOperationError(
            "migration_intake_resume_failed",
            f"source {old.backend} runner intake could not be resumed; pause marker(s) remain: "
            + ", ".join(remaining),
        )
    if not runner_identity_alive(old):
        raise AgentOperationError(
            "migration_intake_resume_failed",
            f"source {old.backend} runner pid {old.runner_pid} is not live after rollback; "
            "cannot prove intake resumed",
        )


def _restore_headless_runner(name: str, old: AgentRecord) -> RunnerIdentity:
    """Restore a stopped source runner using its durable registry session id."""
    if window_exists(old):
        kill_window(old)
    target = launch_window(old.backend, name, old.cwd, runner_wrapper(name))
    restored = wait_for_restarted_runner(
        name, RunnerIdentity(old.runner_pid or -1, old.runner_started_at)
    )
    if restored is None:
        raise AgentOperationError(
            "migration_source_restore_unconfirmed",
            f"{old.backend} presentation {target} was created but its replacement runner did not "
            f"publish a live identity within {MIGRATION_READY_TIMEOUT_S:g}s",
        )
    return restored


def _tui_conversion_result(old: AgentRecord, current: AgentRecord) -> MigrationResult:
    return MigrationResult(
        name=current.name,
        from_backend=old.backend,
        to_backend=current.backend,
        from_mode=old.mode,
        to_mode=current.mode,
        tmux_target=current.tmux_target,
        session_id=current.session_id,
        transcript=str(transcript_path(current.name)),
    )


def _commit_headless_to_tui(
    name: str, old: AgentRecord, destination: AgentRecord
) -> AgentRecord:
    """Commit a previously confirmed TUI only while the old row is unchanged."""
    with registry_lock() as agents:
        current = agents.get(name)
        if current is None:
            raise AgentOperationError("unknown_agent", f"agent {name!r} disappeared during TUI conversion")
        if pending_count(name) > 0:
            raise AgentOperationError(
                "migration_message_queued",
                "a message was queued during TUI conversion; destination was not committed",
            )
        if (
            current.backend != old.backend
            or current.mode != HEADLESS_MODE
            or current.tmux_target != old.tmux_target
            or current.runner_pid != old.runner_pid
            or current.runner_started_at != old.runner_started_at
        ):
            raise AgentOperationError(
                "migration_source_changed",
                "source registry changed before TUI conversion could be committed",
            )
        agents[name] = destination
    return destination


def _convert_headless_to_tui(name: str, old: AgentRecord) -> MigrationResult:
    """Replace an idle headless runner with a health-checked resumed Codex TUI.

    New runners pause first, so they cannot consume a coordinator message while
    the TUI answers its health check. Legacy tmux runners cannot pause; their
    separate path below stops only at an idle, empty-inbox boundary and restores
    the same headless session if the TUI does not prove responsive.
    """
    if old.harness != "codex":
        raise AgentOperationError(
            "tui_harness_unsupported", "headless-to-TUI conversion currently supports codex only"
        )
    if old.session_id is None:
        raise AgentOperationError(
            "tui_session_unavailable",
            "cannot convert a headless agent before it has recorded a Codex session id",
        )
    assert old.runner_pid is not None
    old_identity = RunnerIdentity(old.runner_pid, old.runner_started_at)
    with registry_lock() as agents:
        current = agents.get(name)
        if current is None or not runner_identity_alive(current):
            raise AgentOperationError("dead_runner", f"agent {name!r} runner stopped before conversion")
        if current.status != "idle" or pending_count(name) > 0:
            raise AgentOperationError("agent_busy", f"agent {name!r} is busy or has queued work")
        write_migration_pause(name, old_identity)
    if not wait_for_pause_ack(name, old_identity):
        if old.backend == "tmux":
            return _convert_legacy_tmux_headless_to_tui(name, old)
        clear_migration_markers(name, "")
        raise AgentOperationError(
            "migration_pause_unconfirmed",
            f"could not confirm that source {old.backend} runner pid {old_identity.pid} paused intake; "
            "no TUI destination was started and the registry remains unchanged",
        )

    destination: Optional[AgentRecord] = None
    source_retired = False
    try:
        target, pane_id, probe = _launch_herdr_tui(
            name, old.cwd, old.model, session_id=old.session_id,
            bypass_permissions=old.codex_bypass_permissions,
        )
        destination = dataclasses.replace(
            old,
            backend="herdr",
            tmux_target=target,
            mode=TUI_MODE,
            presentation_pane=pane_id,
            runner_pid=probe.pid,
            runner_started_at=None if probe.pid is None else pid_start_time(probe.pid),
            status="idle",
        )
        _confirm_resumed_tui(destination)
        if not terminate_runner(old):
            raise AgentOperationError(
                "migration_source_cleanup_failed",
                f"resumed TUI was confirmed, but source runner pid {old_identity.pid} did not stop",
            )
        source_retired = True
        committed = _commit_headless_to_tui(name, old, destination)
    except Exception as exc:
        rollback_errors: list[str] = []
        if destination is not None:
            try:
                kill_window(destination)
            except Exception as cleanup_exc:
                rollback_errors.append(f"TUI destination cleanup failed: {cleanup_exc}")
        resume_source = old
        if source_retired:
            try:
                restored = _restore_headless_runner(name, old)
            except Exception as restore_exc:
                rollback_errors.append(f"headless source restoration failed: {restore_exc}")
            else:
                rollback_errors.append(f"headless source restored as pid {restored.pid}")
                resume_source = read_registry().get(name) or old
        try:
            _resume_source_intake(name, resume_source)
        except AgentOperationError as resume_exc:
            rollback_errors.append(f"intake resume proof failed: {resume_exc.message}")
        else:
            rollback_errors.append(f"source intake resumed for pid {resume_source.runner_pid}")
        if rollback_errors:
            raise AgentOperationError(
                "tui_conversion_rollback",
                f"TUI destination was not committed ({exc}); " + "; ".join(rollback_errors),
            ) from exc
        raise AgentOperationError(
            "tui_conversion_failed_before_retiring_source",
            f"TUI destination was not committed ({exc}); source {old.backend} runner pid "
            f"{old_identity.pid} has its pause marker removed and registry unchanged",
        ) from exc

    try:
        kill_window(old)
    except Exception as exc:
        raise AgentOperationError(
            "migration_source_cleanup_failed",
            f"TUI pane {committed.presentation_pane} was committed, but closing the old {old.backend} "
            f"presentation failed: {exc}",
        ) from exc
    if old.backend == "tmux":
        create_legacy_tmux_guard(name, old.cwd, target=old.tmux_target)
    clear_migration_markers(name, "")
    write_event("agent_converted_to_tui", name, preview=last_message_preview(name))
    return _tui_conversion_result(old, committed)


def _convert_legacy_tmux_headless_to_tui(name: str, old: AgentRecord) -> MigrationResult:
    """Convert a pre-handshake tmux runner, restoring it on any TUI failure."""
    assert old.runner_pid is not None
    with registry_lock() as agents:
        current = agents.get(name)
        if current is None or not runner_identity_alive(current):
            raise AgentOperationError("dead_runner", f"legacy source runner for {name!r} is no longer alive")
        if current.status != "idle" or pending_count(name) > 0:
            raise AgentOperationError(
                "agent_busy", f"legacy source {name!r} is no longer idle with an empty inbox"
            )
    if not terminate_runner(old):
        raise AgentOperationError(
            "legacy_source_stop_failed",
            f"legacy source tmux runner pid {old.runner_pid} did not stop; no TUI destination was started",
        )

    destination: Optional[AgentRecord] = None
    try:
        target, pane_id, probe = _launch_herdr_tui(
            name, old.cwd, old.model, session_id=old.session_id,
            bypass_permissions=old.codex_bypass_permissions,
        )
        destination = dataclasses.replace(
            old,
            backend="herdr",
            tmux_target=target,
            mode=TUI_MODE,
            presentation_pane=pane_id,
            runner_pid=probe.pid,
            runner_started_at=None if probe.pid is None else pid_start_time(probe.pid),
            status="idle",
        )
        _confirm_resumed_tui(destination)
        committed = _commit_headless_to_tui(name, old, destination)
    except Exception as exc:
        cleanup_errors: list[str] = []
        if destination is not None:
            try:
                kill_window(destination)
            except Exception as cleanup_exc:
                cleanup_errors.append(f"TUI destination cleanup failed: {cleanup_exc}")
        try:
            restored = _restore_headless_runner(name, old)
        except Exception as restore_exc:
            cleanup_errors.append(f"tmux source restoration failed: {restore_exc}")
            resume_source = old
        else:
            cleanup_errors.append(f"tmux source restored as pid {restored.pid}")
            resume_source = read_registry().get(name) or old
        try:
            _resume_source_intake(name, resume_source)
        except AgentOperationError as resume_exc:
            cleanup_errors.append(f"intake resume proof failed: {resume_exc.message}")
        else:
            cleanup_errors.append(f"source intake resumed for pid {resume_source.runner_pid}")
        raise AgentOperationError(
            "legacy_tui_conversion_rolled_back",
            f"legacy source was stopped and TUI destination was not committed ({exc}); "
            + "; ".join(cleanup_errors),
        ) from exc

    try:
        kill_window(old)
    except Exception as exc:
        raise AgentOperationError(
            "migration_source_cleanup_failed",
            f"TUI pane {committed.presentation_pane} was committed, but closing old tmux presentation failed: {exc}",
        ) from exc
    create_legacy_tmux_guard(name, old.cwd, target=old.tmux_target)
    clear_migration_markers(name, "")
    write_event("agent_converted_to_tui", name, preview=last_message_preview(name))
    return _tui_conversion_result(old, committed)


def migrate_agent(
    name: str, *, to_backend: str = "herdr", to_mode: Optional[str] = None
) -> MigrationResult:
    """Atomically move one idle runner without losing its durable session.

    The source runner pauses intake first. A staged destination runner proves
    its pid/start-time identity before the registry changes or the source
    presentation is retired, so a failed destination cannot strand the source.
    """
    valid_name = require_valid_name(name)
    to_backend = require_supported_backend(to_backend)
    rec = _require_live_agent(valid_name)
    destination_mode = rec.mode if to_mode is None else require_supported_mode(to_mode)
    if rec.mode == TUI_MODE:
        raise AgentOperationError(
            "tui_migration_unsupported",
            "interactive TUI sessions cannot be migrated yet; keep their Herdr tab open or start a new TUI",
        )
    if destination_mode == TUI_MODE:
        if to_backend != "herdr":
            raise AgentOperationError("tui_requires_herdr", "--mode tui requires --to herdr")
        if not backend_available("herdr"):
            raise AgentOperationError(
                "herdr_missing", "herdr is selected but is not available; check its binary and running server"
            )
        return _convert_headless_to_tui(valid_name, dataclasses.replace(rec))
    if rec.backend == to_backend:
        raise AgentOperationError(
            "already_on_backend", f"agent {valid_name!r} already uses the {to_backend} backend"
        )
    if rec.status != "idle" or pending_count(valid_name) > 0:
        raise AgentOperationError(
            "agent_busy",
            f"agent {valid_name!r} is busy or has queued work; migrate only at an idle boundary",
        )
    if not backend_available(to_backend):
        raise AgentOperationError(
            f"{to_backend}_missing",
            f"{to_backend} is selected but is not available; check its binary and running server",
        )

    old_rec = dataclasses.replace(rec)
    assert old_rec.runner_pid is not None
    old_identity = RunnerIdentity(old_rec.runner_pid, old_rec.runner_started_at)
    token = secrets.token_hex(16)
    with registry_lock() as agents:
        current = agents.get(valid_name)
        if current is None or not runner_identity_alive(current):
            raise AgentOperationError(
                "dead_runner", f"agent {valid_name!r} runner stopped before migration could begin"
            )
        if current.status != "idle" or pending_count(valid_name) > 0:
            raise AgentOperationError(
                "agent_busy", f"agent {valid_name!r} became busy before migration could pause intake"
            )
        write_migration_pause(valid_name, old_identity)
    if not wait_for_pause_ack(valid_name, old_identity):
        if old_rec.backend == "tmux":
            return _migrate_legacy_tmux_source(valid_name, old_rec, to_backend)
        clear_migration_markers(valid_name, token)
        raise AgentOperationError(
            "migration_pause_unconfirmed",
            f"could not confirm that source {old_rec.backend} runner pid {old_identity.pid} paused intake; "
            "no destination was started and the registry remains unchanged",
        )

    new_target: Optional[str] = None
    staged_identity: Optional[RunnerIdentity] = None
    try:
        new_target = launch_window(
            to_backend, valid_name, rec.cwd, runner_wrapper(valid_name, stage_token=token)
        )
        staged_identity = wait_for_staged_runner(valid_name, token)
        if staged_identity is None:
            raise AgentOperationError(
                "migration_destination_unconfirmed",
                f"{to_backend} presentation {new_target} was created, but no live staged runner identity "
                f"was recorded within {MIGRATION_READY_TIMEOUT_S:g}s",
            )
        if staged_identity.pid == old_identity.pid:
            raise AgentOperationError(
                "migration_destination_unconfirmed",
                f"destination runner reused source pid {old_identity.pid}; refusing to replace the source",
            )
        with registry_lock() as agents:
            current = agents.get(valid_name)
            if current is None or not runner_identity_alive(current):
                raise AgentOperationError(
                    "migration_source_lost",
                    "source runner stopped before the destination could be committed",
                )
            if (
                current.backend != old_rec.backend
                or current.tmux_target != old_rec.tmux_target
                or current.runner_pid != old_identity.pid
                or current.runner_started_at != old_identity.started_at
            ):
                raise AgentOperationError(
                    "migration_source_changed",
                    "source registry identity changed before destination commit; refusing the migration",
                )
            if pending_count(valid_name) > 0:
                raise AgentOperationError(
                    "migration_message_queued",
                    "a message was queued during migration; destination was not committed and source remains paused",
                )
            current.backend = to_backend
            current.tmux_target = new_target
            current.runner_pid = staged_identity.pid
            current.runner_started_at = staged_identity.started_at
            staged_runner_activate_path(valid_name, token).write_text(now_iso() + "\n")
            session_id = current.session_id
        if not wait_for_staged_runner_activation(valid_name, token):
            raise AgentOperationError(
                "migration_activation_unconfirmed",
                "staged destination did not acknowledge activation before marker cleanup",
            )
    except Exception as exc:
        rollback_errors: list[str] = []
        if staged_identity is not None:
            terminate_runner_identity(staged_identity)
        if new_target is not None:
            try:
                kill_window(dataclasses.replace(old_rec, backend=to_backend, tmux_target=new_target))
            except Exception as cleanup_exc:
                rollback_errors.append(f"destination cleanup failed: {cleanup_exc}")
        try:
            restored = _restore_migration_record(valid_name, old_rec, staged_identity, to_backend, new_target)
        except Exception as restore_exc:
            raise AgentOperationError(
                "migration_rollback_failed", f"destination failed ({exc}); source registry restoration failed: {restore_exc}; intake remains paused"
            ) from exc
        clear_migration_markers(valid_name, token)
        source_state = "alive" if runner_identity_alive(old_rec) else "not alive"
        if rollback_errors:
            raise AgentOperationError(
                "migration_rollback_failed",
                f"destination failed ({exc}); source registry restored; {old_rec.backend} runner is {source_state}; "
                + "; ".join(rollback_errors),
            ) from exc
        raise AgentOperationError(
            "migration_activation_rolled_back" if restored else "migration_failed_before_commit",
            f"destination failed: {exc}; source {old_rec.backend} runner pid "
            f"{old_identity.pid} is {source_state}, its window is retained and its registry identity is restored",
        ) from exc

    if not terminate_runner(old_rec):
        raise AgentOperationError(
            "migration_source_cleanup_failed",
            f"destination runner pid {staged_identity.pid} was committed, but source runner pid "
            f"{old_identity.pid} did not stop; intake remains paused on the source",
        )
    try:
        kill_window(old_rec)
    except Exception as exc:
        raise AgentOperationError(
            "migration_source_cleanup_failed",
            f"destination runner pid {staged_identity.pid} was committed, but closing source "
            f"{old_rec.backend} presentation failed: {exc}",
        ) from exc
    if to_backend == "herdr":
        create_legacy_tmux_guard(valid_name, old_rec.cwd, target=old_rec.tmux_target)
    clear_migration_markers(valid_name, token)
    write_event("agent_migrated", valid_name, preview=last_message_preview(valid_name))
    return MigrationResult(
        name=valid_name,
        from_backend=old_rec.backend,
        to_backend=to_backend,
        from_mode=old_rec.mode,
        to_mode=old_rec.mode,
        tmux_target=new_target,
        session_id=session_id,
        transcript=str(transcript_path(valid_name)),
    )


def recreate_window(name: str) -> RecreateWindowResult:
    """Restore a worker presentation while retaining the conversation and queued prompts."""
    valid_name = require_valid_name(name)
    rec = read_registry().get(valid_name)
    if rec is None:
        raise AgentOperationError("unknown_agent", f"unknown agent {valid_name!r}")
    if rec.mode == TUI_MODE:
        raise AgentOperationError(
            "tui_recreate_unsupported",
            "recreating an interactive TUI would lose its visible conversation; inspect or restore its Herdr tab manually",
        )
    if not runner_identity_alive(rec):
        raise AgentOperationError(
            "dead_runner",
            f"agent {valid_name!r} runner pid {rec.runner_pid} is not alive or was reused",
        )
    if window_exists(rec):
        return RecreateWindowResult(
            name=valid_name,
            mode="already_present",
            backend=rec.backend,
            tmux_target=rec.tmux_target,
            pane_id=None,
            window_alive=True,
        )
    pane_id: Optional[str] = None
    if rec.backend == "tmux":
        pane_id = break_runner_pane_to_window(rec)
    mode = "break_pane" if pane_id is not None else "transcript_view"
    if pane_id is None:
        transcript = shlex.quote(str(transcript_path(valid_name)))
        cmd = (
            f"echo '[subagents] presentation view for live runner pid {rec.runner_pid}'; "
            f"echo '[subagents] input still goes through inbox/MCP, not this pane'; "
            f"touch {transcript}; tail -n +1 -F {transcript}"
        )
        try:
            if rec.backend == "tmux":
                _exact_tmux_target(rec.tmux_target)
                target = _launch_tmux_window(valid_name, rec.cwd, cmd, session=rec.tmux_target.partition(":")[0])
            else:
                target = launch_window(rec.backend, valid_name, rec.cwd, cmd)
        except Exception as exc:
            raise AgentOperationError(
                "presentation_recreate_failed",
                f"failed to create transcript-view window for {valid_name!r}: {exc}",
            ) from exc
        with registry_lock() as agents:
            current = agents.get(valid_name)
            if current is not None:
                current.tmux_target = target
                rec = current
    write_event("presentation_restored", valid_name, preview=last_message_preview(valid_name))
    return RecreateWindowResult(
        name=valid_name,
        mode=mode,
        backend=rec.backend,
        tmux_target=rec.tmux_target,
        pane_id=pane_id,
        window_alive=window_exists(rec),
    )
