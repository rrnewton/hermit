"""Run a command boxed to one slot: resource limits plus a confined file-system view.

``wrkslots run SLOT -- COMMAND`` needs no host configuration. The box is
orthogonal to how the slot is stored: a plain-worktree slot and a disk-image
slot get exactly the same limits and the same view; only the location of the
slot's private state differs (the state image, or ``<control>/slot-state/``).

* **Limits.** Each slot gets its own slice, ``wrkslots-<slot>.slice``, under
  ``wrkslots.slice`` (nested inside whatever slice the caller already occupies).
  Memory, CPU, task, and IO limits apply to the slice, so every command run
  against the same slot shares one budget.
* **Process tree.** The calling process itself is placed in a transient systemd
  *scope* in that slice, then execs, so COMMAND keeps the caller's PID. A
  terminal multiplexer that detects agents by the pane's own shell PID (Herdr)
  still sees the agent.
* **Isolation modes.** ``userns`` builds the view in a new unprivileged user and
  mount namespace. ``root`` builds the identical view through a short-lived
  ``sudo -n`` launcher in a plain mount namespace, then drops to the invoking
  user's uid, gid, and groups before exec, so setuid helpers inside the box
  (``sudo``, a harness launcher that enters a site sandbox) still work.
  ``cgroup`` applies the limits only and leaves the file system alone.
* **The view** (``userns`` and ``root``), built by :func:`build_view`:

  1. ``/tmp`` is a fresh tmpfs for this launch, discarded at exit.
  2. The real ``$HOME`` (and every other mount path that exposes the same
     directory) stays visible **read-only**, submounts included; nothing is
     copied. ``home: hidden`` covers it with an empty tmpfs instead and binds
     back only the ``home_expose`` paths, read-only.
  3. ``home_shared`` paths (agent credentials and transcripts) are bound
     read-write onto themselves; ``home_private`` paths are per-slot,
     persistent, writable directories from the slot's state.
  4. The slot, the Git common directories its checkouts commit into, the
     wrkslots control directory, the project's blessed ``outputs``, and any
     ``read_write`` paths are writable.
  5. ``home_hidden`` entries are masked: directories by an empty read-only
     tmpfs, files by ``/dev/null``.
  6. With ``protect_system`` every remaining mount becomes read-only, except
     ``/proc``, ``/sys``, ``/dev``, and ``/run/user``.

The network is not touched: agent harnesses and site policy own that.
"""

from __future__ import annotations

import contextlib
import ctypes
import dataclasses
import errno as _errno
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Mapping, Sequence

ISOLATIONS = ("userns", "cgroup", "root")
COORDINATOR_SCOPES = ("worktrees", "project")
HOME_MODES = ("ro", "hidden")
#: Agent harness state that must stay shared with the host and writable:
#: credentials, settings, and session transcripts. A private copy per slot
#: would log the agent out and scatter its history.
DEFAULT_HOME_SHARED = (
    ".claude",
    ".codex",
    ".muse",
    ".config/muse",
    ".config/opencode",
    ".local/share/opencode",
    ".local/state/herdr",
    ".local/share/muse",
    # Package caches: a build that fetches a crate it has not cached writes here.
    ".cargo/registry",
    ".cargo/git",
)
#: Per-slot, persistent, writable directories: they live in the slot's private
#: $HOME layer instead of being bound from the real $HOME.
DEFAULT_HOME_PRIVATE = (".cache", ".buck")
#: Top-level $HOME files seeded ONCE into the slot's layer as a private,
#: writable copy. A harness that rewrites its state file with a temporary file
#: and a rename (``~/.claude.json``) needs a writable $HOME directory and a
#: real file, which a bind mount of the host's file cannot provide.
DEFAULT_HOME_PRIVATE_FILES = (".claude.json",)
#: Credentials masked inside the box even though the rest of $HOME is readable.
DEFAULT_HOME_HIDDEN = (
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".pgpass",
    ".arcrc",
    ".config/gh/hosts.yml",
    ".config/gcloud",
)
#: What ``home: hidden`` binds back, read-only.
DEFAULT_HOME_EXPOSE = ("bin", ".local/bin")
#: Project-root-relative directories the box may write in the primary checkout.
DEFAULT_OUTPUTS = ("ai_docs", "experiments")
DEFAULT_ENV: tuple[tuple[str, str], ...] = ()
DEFAULT_TMP_SIZE = "16G"
DEFAULT_TASKS_MAX = 8192
_ENV_DENYLIST = frozenset(
    {
        "INVOCATION_ID",
        "JOURNAL_STREAM",
        "NOTIFY_SOCKET",
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDNAMES",
        "MANAGERPID",
        "SYSTEMD_EXEC_PID",
    }
)
_TEMP_VARIABLES = ("TMPDIR", "TMP", "TEMP")
_LIMIT_RE = re.compile(r"^(infinity|\d+(\.\d+)?[KMGT]?|\d+(\.\d+)?%)$")
_TMP_SIZE_RE = re.compile(r"^(\d+[KMGT]?|\d+%)$")
_ENV_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
#: Keys of earlier drafts, refused with the replacement named.
_RENAMED_KEYS = {
    "home_writable": "home_private",
    "home_paths": "home_expose (with home: hidden)",
    "memory_max": "limits.memory_max",
    "memory_high": "limits.memory_high",
    "cpu_quota": "limits.cpu_quota",
    "tasks_max": "limits.tasks_max",
    "io_weight": "limits.io_weight",
    "all_slots_memory_max": "limits.all_slots_memory_max",
    "all_slots_cpu_quota": "limits.all_slots_cpu_quota",
}


class SandboxError(RuntimeError):
    """A sandbox request is invalid or cannot run on this host."""


class MountError(SandboxError):
    """A mount system call failed; ``errno`` says why."""

    def __init__(self, message: str, error: int) -> None:
        super().__init__(message)
        self.errno = error


# ------------------------------------------------------------------ settings


@dataclasses.dataclass(frozen=True)
class SandboxLimits:
    """systemd limits for the slot's slice and the all-slots slice."""

    memory_max: str | None = None
    memory_high: str | None = None
    cpu_quota: str | None = None
    tasks_max: int | None = DEFAULT_TASKS_MAX
    io_weight: int | None = None
    all_slots_memory_max: str | None = None
    all_slots_cpu_quota: str | None = None


@dataclasses.dataclass(frozen=True)
class SandboxSettings:
    """The ``configuration.sandbox`` section; every field can be overridden per run."""

    isolation: str = "userns"
    home: str = "ro"
    home_shared: tuple[str, ...] = DEFAULT_HOME_SHARED
    home_private: tuple[str, ...] = DEFAULT_HOME_PRIVATE
    home_private_files: tuple[str, ...] = DEFAULT_HOME_PRIVATE_FILES
    home_hidden: tuple[str, ...] = DEFAULT_HOME_HIDDEN
    home_expose: tuple[str, ...] = DEFAULT_HOME_EXPOSE
    outputs: tuple[str, ...] = DEFAULT_OUTPUTS
    read_write: tuple[str, ...] = ()
    env: tuple[tuple[str, str], ...] = DEFAULT_ENV
    tmp_size: str = DEFAULT_TMP_SIZE
    protect_system: bool = True
    coordinator_writable: str = "worktrees"
    limits: SandboxLimits = SandboxLimits()


SETTING_KEYS = tuple(field.name for field in dataclasses.fields(SandboxSettings))
LIMIT_KEYS = tuple(field.name for field in dataclasses.fields(SandboxLimits))

#: The comment written above each ``sandbox`` key in a literate configuration.
#: Kept beside the defaults above so documentation and defaults cannot drift.
SETTING_DOCS: dict[str, str] = {
    "isolation": (
        "How `wrkslots run`, `shell-command`, and agent launchers box a command. userns "
        "(default): limits plus the file-system view below, built in an unprivileged user "
        "namespace. root: the same view built by a short-lived `sudo -n` launcher, then "
        "privileges dropped to your uid, gid, and groups; use it for harness launchers that "
        "perform a setuid step, which a user namespace refuses. Setuid programs (sudo included) "
        "work inside, and your processes outside the box stay reachable through /proc. Needs "
        "passwordless sudo. cgroup: per-slot limits only, no file-system view. The box is an "
        "accident boundary for cooperative agents, not containment of a hostile process: the "
        "user's systemd bus stays reachable, so `systemd-run --user` without --scope starts work "
        "outside it."
    ),
    "home": (
        "ro (default): $HOME inside the box is this slot's persistent private layer, in "
        "which every real top-level entry is bound read-only (nothing is copied). New "
        "top-level files land in the layer; the real $HOME is never written. hidden: only "
        "the home_expose paths are bound into the layer."
    ),
    "home_shared": (
        "$HOME-relative paths bound read-write from the real $HOME: agent credentials, "
        "settings, and transcripts that must stay shared with the host, and package caches "
        "(.cargo/registry and .cargo/git, which cargo writes when it fetches a crate it has "
        "not cached; ~/.cargo/bin and ~/.cargo/config.toml stay read-only on purpose). A "
        "project that pins its toolchain in rust-toolchain.toml may also need .rustup here "
        "so rustup can install it. Missing paths are skipped. Only these paths are writable; "
        "the rest of ~/.config and ~/.local stays read-only. Note: these directories hold "
        "settings and hook files that the harness later runs OUTSIDE any box, so a boxed "
        "agent that edits them affects unboxed runs."
    ),
    "home_private": (
        "$HOME-relative directories that are private to the slot, persistent, and writable "
        "(kept in the slot's state: the state image, or <control>/slot-state/)."
    ),
    "home_private_files": (
        "Top-level $HOME files copied into the slot's layer once and then private and "
        "writable, for tools that rewrite a state file with a temporary file and a rename."
    ),
    "home_hidden": (
        "$HOME-relative credentials masked inside the box: a directory appears empty, a "
        "file reads as empty."
    ),
    "home_expose": "With home: hidden, the $HOME-relative paths bound in, read-only.",
    "outputs": (
        "Directories, relative to the project root (the primary checkout holding this "
        "file), that a boxed command may write. Missing ones are skipped."
    ),
    "read_write": (
        "Extra absolute paths a boxed command may write. A leading ~ and $USER or $HOME "
        "are expanded, so a per-user path can be written as /var/.../$USER/... Missing "
        "paths are skipped."
    ),
    "env": (
        "Extra environment variables for the boxed command, as NAME: value. A leading ~ in "
        "a value is your $HOME."
    ),
    "tmp_size": (
        f"Size of the fresh /tmp tmpfs each launch gets (discarded at exit), such as "
        f"{DEFAULT_TMP_SIZE} or 25%."
    ),
    "protect_system": (
        "true (default): every mount outside the writable paths above is read-only. false "
        "leaves the rest of the file system writable; $HOME stays read-only either way."
    ),
    "coordinator_writable": (
        "What a coordinator box (`wrkslots box`, `agentctl start --project-box`) may write "
        "besides the paths above. worktrees (default): the managed worktrees directory "
        "(every slot, the registry, slot images and state, validate slots) and the Git "
        "directories the project's slots commit into. project: the whole project root. A "
        "coordinator box can write the registry and every slot by design. A project that "
        "keeps validation logs or other coordinator output outside the worktrees directory "
        "chooses project, or lists those paths in read_write."
    ),
    "limits": (
        "systemd limits for the slot's slice, shared by everything run against the slot. "
        "null means no limit."
    ),
}
LIMIT_DOCS: dict[str, str] = {
    "memory_max": "Slot memory ceiling, such as 32G; swap is disabled when set.",
    "memory_high": "Slot memory throttling threshold, such as 24G.",
    "cpu_quota": "Slot CPU ceiling, such as 800% for eight CPUs.",
    "tasks_max": f"Slot process and thread limit (default {DEFAULT_TASKS_MAX}).",
    "io_weight": "Slot IO weight, 1 to 10000.",
    "all_slots_memory_max": "Memory ceiling shared by all slots together.",
    "all_slots_cpu_quota": "CPU ceiling shared by all slots together.",
}


def _str_tuple(value: object, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise SandboxError(f"{label} must be a list of strings")
    return tuple(str(item) for item in value)


def _optional_str(value: object, label: str) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or not _LIMIT_RE.match(value):
        raise SandboxError(f"{label} must look like 32G, 800%, or infinity, not {value!r}")
    return value


def _optional_int(value: object, label: str) -> int | None:
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise SandboxError(f"{label} must be a positive integer or null")
    return value


def _relative(value: str, label: str) -> str:
    candidate = Path(value)
    if not value or candidate.is_absolute() or ".." in candidate.parts or candidate.as_posix() == ".":
        raise SandboxError(f"{label} entries must be relative paths without '..': {value!r}")
    return candidate.as_posix()


def _relative_list(raw: Mapping[str, object], key: str, default: tuple[str, ...]) -> tuple[str, ...]:
    label = f"configuration.sandbox.{key}"
    values = _str_tuple(raw.get(key, list(default)), label)
    return tuple(dict.fromkeys(_relative(item, label) for item in values))


def _top_level_list(raw: Mapping[str, object], key: str, default: tuple[str, ...]) -> tuple[str, ...]:
    values = _relative_list(raw, key, default)
    for value in values:
        if "/" in value:
            raise SandboxError(f"configuration.sandbox.{key} entries must be top-level $HOME names: {value!r}")
    return values


def _absolute_list(raw: Mapping[str, object], key: str) -> tuple[str, ...]:
    label = f"configuration.sandbox.{key}"
    result: list[str] = []
    for item in _str_tuple(raw.get(key, []), label):
        if not (item.startswith("/") or item == "~" or item.startswith("~/")):
            raise SandboxError(f"{label} entries must be absolute or ~/-relative paths: {item!r}")
        result.append(item)
    return tuple(dict.fromkeys(result))


def _env_pairs(value: object) -> tuple[tuple[str, str], ...]:
    if not isinstance(value, dict):
        raise SandboxError("configuration.sandbox.env must be an object of NAME: value strings")
    pairs: list[tuple[str, str]] = []
    for key, item in value.items():
        if not isinstance(key, str) or not _ENV_NAME_RE.match(key):
            raise SandboxError(f"configuration.sandbox.env has an invalid variable name: {key!r}")
        if not isinstance(item, str) or "\n" in item or "\0" in item:
            raise SandboxError(f"configuration.sandbox.env.{key} must be a one-line string")
        pairs.append((key, item))
    return tuple(pairs)


def limits_from_obj(raw: object) -> SandboxLimits:
    """Validate a ``configuration.sandbox.limits`` mapping."""

    if not isinstance(raw, dict):
        raise SandboxError("configuration.sandbox.limits must be an object")
    unknown = sorted(set(raw) - set(LIMIT_KEYS))
    if unknown:
        raise SandboxError(f"configuration.sandbox.limits has unknown keys: {', '.join(unknown)}")
    defaults = SandboxLimits()
    label = "configuration.sandbox.limits."
    return SandboxLimits(
        memory_max=_optional_str(raw.get("memory_max"), label + "memory_max"),
        memory_high=_optional_str(raw.get("memory_high"), label + "memory_high"),
        cpu_quota=_optional_str(raw.get("cpu_quota"), label + "cpu_quota"),
        tasks_max=_optional_int(raw.get("tasks_max", defaults.tasks_max), label + "tasks_max"),
        io_weight=_optional_int(raw.get("io_weight"), label + "io_weight"),
        all_slots_memory_max=_optional_str(raw.get("all_slots_memory_max"), label + "all_slots_memory_max"),
        all_slots_cpu_quota=_optional_str(raw.get("all_slots_cpu_quota"), label + "all_slots_cpu_quota"),
    )


def settings_from_obj(raw: Mapping[str, object]) -> SandboxSettings:
    """Validate a ``configuration.sandbox`` mapping; absent keys take their defaults."""

    unknown = sorted(set(raw) - set(SETTING_KEYS))
    if unknown:
        hints = [f"{key} (now {_RENAMED_KEYS[key]})" if key in _RENAMED_KEYS else key for key in unknown]
        raise SandboxError(f"configuration.sandbox has unknown keys: {', '.join(hints)}")
    defaults = SandboxSettings()
    isolation = raw.get("isolation", defaults.isolation)
    if isolation not in ISOLATIONS:
        hint = " (namespace was renamed userns)" if isolation == "namespace" else ""
        raise SandboxError(f"configuration.sandbox.isolation must be one of {', '.join(ISOLATIONS)}{hint}")
    home = raw.get("home", defaults.home)
    if home not in HOME_MODES:
        hint = " (use hidden with home_expose)" if home in ("select", "none") else ""
        raise SandboxError(f"configuration.sandbox.home must be one of {', '.join(HOME_MODES)}{hint}")
    protect = raw.get("protect_system", defaults.protect_system)
    if not isinstance(protect, bool):
        raise SandboxError("configuration.sandbox.protect_system must be true or false")
    tmp_size = raw.get("tmp_size", defaults.tmp_size)
    if not isinstance(tmp_size, str) or not _TMP_SIZE_RE.match(tmp_size):
        raise SandboxError(f"configuration.sandbox.tmp_size must look like 16G or 50%, not {tmp_size!r}")
    scope = raw.get("coordinator_writable", defaults.coordinator_writable)
    if scope not in COORDINATOR_SCOPES:
        raise SandboxError(
            f"configuration.sandbox.coordinator_writable must be one of {', '.join(COORDINATOR_SCOPES)}"
        )
    env = _env_pairs(raw["env"]) if "env" in raw else defaults.env
    limits = limits_from_obj(raw["limits"]) if "limits" in raw else defaults.limits
    return SandboxSettings(
        isolation=str(isolation),
        home=str(home),
        home_shared=_relative_list(raw, "home_shared", defaults.home_shared),
        home_private=_relative_list(raw, "home_private", defaults.home_private),
        home_private_files=_top_level_list(raw, "home_private_files", defaults.home_private_files),
        home_hidden=_relative_list(raw, "home_hidden", defaults.home_hidden),
        home_expose=_relative_list(raw, "home_expose", defaults.home_expose),
        outputs=_relative_list(raw, "outputs", defaults.outputs),
        read_write=_absolute_list(raw, "read_write"),
        env=env,
        tmp_size=tmp_size,
        protect_system=protect,
        coordinator_writable=str(scope),
        limits=limits,
    )


def settings_to_obj(settings: SandboxSettings) -> dict[str, object]:
    """The full JSON form of ``settings``, every key spelled out."""

    return {
        "isolation": settings.isolation,
        "home": settings.home,
        "home_shared": list(settings.home_shared),
        "home_private": list(settings.home_private),
        "home_private_files": list(settings.home_private_files),
        "home_hidden": list(settings.home_hidden),
        "home_expose": list(settings.home_expose),
        "outputs": list(settings.outputs),
        "read_write": list(settings.read_write),
        "env": dict(settings.env),
        "tmp_size": settings.tmp_size,
        "protect_system": settings.protect_system,
        "coordinator_writable": settings.coordinator_writable,
        "limits": {key: getattr(settings.limits, key) for key in LIMIT_KEYS},
    }


def default_config_obj() -> dict[str, object]:
    """The ``sandbox`` section ``wrkslots init`` writes for a new project."""

    return settings_to_obj(SandboxSettings())


def merge_defaults(existing: Mapping[str, object]) -> tuple[dict[str, object], list[str]]:
    """Add every missing default key to an existing sandbox section.

    Present keys are never changed (a present ``env`` or ``limits`` object gets
    only its missing limit keys: ``env`` is a value, ``limits`` a record).
    Returns the merged section and the dotted names that were added.
    """

    merged = dict(existing)
    added: list[str] = []
    for key, value in default_config_obj().items():
        if key not in merged:
            merged[key] = value
            added.append(key)
        elif key == "limits" and isinstance(merged[key], dict) and isinstance(value, dict):
            current = merged[key]
            assert isinstance(current, dict)
            limits = {str(name): item for name, item in current.items()}
            for limit_key, limit_value in value.items():
                if limit_key not in limits:
                    limits[limit_key] = limit_value
                    added.append(f"limits.{limit_key}")
            merged[key] = limits
    settings_from_obj(merged)
    return merged, added


def validate_limit(value: str | None, label: str) -> str | None:
    """Validate one systemd limit value such as 32G or 800%."""

    return _optional_str(value, label)


@dataclasses.dataclass(frozen=True)
class SlotView:
    """Everything about a slot that the sandbox must make visible or writable."""

    slot: str
    slot_type: str
    slot_path: Path
    state_directory: Path
    git_directories: tuple[Path, ...]
    control_directory: Path
    representation: str
    project_root: Path | None = None
    #: A coordinator box (``wrkslots box``) rather than one slot's box: the
    #: registry and slots are writable, and ``slot`` is the box name.
    coordinator: bool = False
    #: Extra writable roots (a coordinator box's scope and Git directories).
    writable: tuple[Path, ...] = ()
    #: Roots whose mounts made on the host after the box starts (image slots)
    #: appear inside it; everything else is cut off from host mount events.
    propagate: tuple[Path, ...] = ()


# ------------------------------------------------------------------ slices


def systemd_escape(value: str) -> str:
    """Escape a string for use inside a systemd unit name."""

    result = subprocess.run(
        ["systemd-escape", "--", value], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise SandboxError(f"systemd-escape failed for {value!r}: {result.stderr.strip()}")
    return result.stdout.strip()


def parent_slice(cgroup_file: Path = Path("/proc/self/cgroup")) -> str | None:
    """The innermost user-manager slice enclosing the caller, if any.

    wrkslots never moves a process OUT of an enclosing slice: a caller that site
    policy placed in a sandbox slice (an agent harness's jail, for example) must
    stay inside it, because monitoring and limits are scoped to that cgroup. The
    slot's slices are therefore created as children of the caller's slice.
    """

    try:
        text = cgroup_file.read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        if not line.startswith("0::"):
            continue
        parts = line[3:].split("/")
        if "user@" not in line:
            return None
        below = parts[next(i for i, part in enumerate(parts) if part.startswith("user@")) + 1 :]
        slices = [part for part in below if part.endswith(".slice")]
        return slices[-1] if slices else None
    return None


def root_slice(parent: str | None = None) -> str:
    """The slice holding every slot's slice (the all-slots guard)."""

    enclosing = parent if parent is not None else parent_slice()
    prefix = "" if enclosing is None else enclosing.removesuffix(".slice") + "-"
    return f"{prefix}wrkslots.slice"


def slice_name(slot_type: str, slot: str, parent: str | None = None) -> str:
    """The slice for one slot, nested under the caller's slice unless ``parent`` is given."""

    # A dash in a slice name means nesting, so the slot name is escaped: slot
    # "kvm-foo" becomes one child of the wrkslots slice, not three levels.
    if slot_type == "box":
        # Coordinator boxes nest under <root>-box.slice, apart from the slots.
        return root_slice(parent).removesuffix(".slice") + f"-box-{systemd_escape(slot)}.slice"
    label = slot if slot_type == "agent" else f"{slot_type}:{slot}"
    return root_slice(parent).removesuffix(".slice") + f"-{systemd_escape(label)}.slice"


def slice_limit_properties(limits: SandboxLimits) -> list[str]:
    """systemd properties for the per-slot slice."""

    properties: list[str] = []
    if limits.memory_max:
        properties.append(f"MemoryMax={limits.memory_max}")
        properties.append("MemorySwapMax=0")
    if limits.memory_high:
        properties.append(f"MemoryHigh={limits.memory_high}")
    if limits.cpu_quota:
        properties.append(f"CPUQuota={limits.cpu_quota}")
    if limits.tasks_max:
        properties.append(f"TasksMax={limits.tasks_max}")
    if limits.io_weight:
        properties.append(f"IOWeight={limits.io_weight}")
    return properties


def apply_slice_limits(view: SlotView, limits: SandboxLimits) -> list[str]:
    """Set runtime limits on the slot's slice (and the all-slots slice)."""

    applied: list[str] = []
    root_limits: list[str] = []
    if limits.all_slots_memory_max:
        root_limits += [f"MemoryMax={limits.all_slots_memory_max}", "MemorySwapMax=0"]
    if limits.all_slots_cpu_quota:
        root_limits.append(f"CPUQuota={limits.all_slots_cpu_quota}")
    for unit, properties in (
        (root_slice(), root_limits),
        (slice_name(view.slot_type, view.slot), slice_limit_properties(limits)),
    ):
        if not properties:
            continue
        # A slice has to be loaded before its properties can be set; starting
        # it is idempotent and leaves no process behind.
        subprocess.run(["systemctl", "--user", "start", unit], capture_output=True, check=False)
        result = subprocess.run(
            ["systemctl", "--user", "set-property", "--runtime", unit, *properties],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise SandboxError(f"cannot set limits on {unit}: {result.stderr.strip()}")
        applied.append(f"{unit}: {' '.join(properties)}")
    return applied


# ------------------------------------------------------------ the view spec


def _mount_rows(mountinfo: Path = Path("/proc/self/mountinfo")) -> list[tuple[str, str, Path]]:
    rows: list[tuple[str, str, Path]] = []
    for line in mountinfo.read_text(encoding="utf-8", errors="surrogateescape").splitlines():
        fields = line.split(" - ", 1)[0].split()
        if len(fields) >= 5:
            rows.append((fields[2], _unescape(fields[3]), Path(_unescape(fields[4]))))
    return rows


def _unescape(value: str) -> str:
    return value.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")


def path_aliases(path: Path) -> list[Path]:
    """Return ``path`` plus other mount paths that expose the same directory.

    On hosts where ``$HOME`` is a bind mount of a subdirectory of a larger file
    system, the same files are also reachable through that file system's own
    mount point. Protecting or hiding ``$HOME`` without its alias would protect
    or hide nothing.
    """

    aliases = [path]
    try:
        rows = _mount_rows()
    except OSError:
        return aliases
    home_rows = [row for row in rows if row[2] == path]
    if not home_rows:
        return aliases
    device, root, _mount_point = home_rows[-1]
    if root == "/":
        return aliases
    for other_device, other_root, other_mount in rows:
        if other_device != device or other_mount == path:
            continue
        # Any mount of the same file system whose root is an ancestor of
        # $HOME's root exposes $HOME below it.
        if other_root == "/" or root.startswith(other_root.rstrip("/") + "/"):
            relative = root[len(other_root.rstrip("/")) :].lstrip("/")
            alias = other_mount / relative
            if alias.is_dir() and alias not in aliases:
                aliases.append(alias)
    return aliases


def forwarded_environment(environ: Mapping[str, str], *, private_tmp: bool) -> dict[str, str]:
    """The caller's environment minus service-manager (and, with a private /tmp, temp) variables."""

    drop = _ENV_DENYLIST | (frozenset(_TEMP_VARIABLES) if private_tmp else frozenset())
    return {
        key: value
        for key, value in environ.items()
        if key not in drop and "=" not in key and "\n" not in value and "\0" not in value
    }


_COORDINATOR_ENV_KEYS = ("WRKSLOTS_BOX", "WRKSLOTS_BOX_WRITABLE", "WRKSLOTS_PROJECT_ROOT")
_SLOT_ENV_KEYS = ("WRKSLOTS_SLOT", "WRKSLOTS_SLOT_TYPE", "WRKSLOTS_SLOT_PATH", "WRKSLOTS_SLOT_REPRESENTATION")


def child_environment(view: SlotView, settings: SandboxSettings, environ: Mapping[str, str]) -> dict[str, str]:
    """The complete environment COMMAND starts with."""

    private_tmp = settings.isolation != "cgroup"
    environment = forwarded_environment(environ, private_tmp=private_tmp)
    home = environ.get("HOME", str(Path.home()))
    if private_tmp:
        environment["TMPDIR"] = "/tmp"
    environment["WRKSLOTS_SANDBOX"] = settings.isolation
    # A box launched from inside another box must describe only itself: a slot
    # box started from a coordinator box must not inherit WRKSLOTS_BOX (which
    # would make it look like a coordinator box to image commands), and a
    # coordinator box must not carry a slot identity.
    for key in (*_COORDINATOR_ENV_KEYS, *_SLOT_ENV_KEYS):
        environment.pop(key, None)
    if view.coordinator:
        environment.update(
            {
                "WRKSLOTS_BOX": view.slot,
                "WRKSLOTS_BOX_WRITABLE": view.representation,
                "WRKSLOTS_PROJECT_ROOT": str(view.project_root or ""),
            }
        )
    else:
        environment.update(
            {
                "WRKSLOTS_SLOT": view.slot,
                "WRKSLOTS_SLOT_TYPE": view.slot_type,
                "WRKSLOTS_SLOT_PATH": str(view.slot_path),
                "WRKSLOTS_SLOT_REPRESENTATION": view.representation,
            }
        )
    for key, value in settings.env:
        environment[key] = _expand_home(value, home)
    return environment


def _expand_home(value: str, home: str) -> str:
    if value == "~":
        return home
    if value.startswith("~/"):
        return home.rstrip("/") + value[1:]
    return value


def expand_path(value: str, home: str, user: str) -> str:
    """Expand a leading ``~`` and ``$USER`` / ``${USER}`` / ``$HOME`` / ``${HOME}``.

    Only these, so a project configuration means the same thing whatever else
    is in the caller's environment.
    """

    value = _expand_home(value, home)
    for name, replacement in (("USER", user), ("HOME", home)):
        value = value.replace("${" + name + "}", replacement)
        value = re.sub(r"\$" + name + r"(?![A-Za-z0-9_])", lambda _match: replacement, value)
    return value


def home_layer(view: SlotView) -> Path:
    """The slot's persistent private layer, mounted over ``$HOME`` inside the box."""

    return view.state_directory / "home"


def private_home_directory(view: SlotView, relative: str) -> Path:
    """Where the slot keeps its private ``$HOME/<relative>``: inside its layer."""

    return home_layer(view) / relative


def home_entries(settings: SandboxSettings, home: Path) -> list[tuple[str, str]]:
    """The real ``$HOME`` entries bound read-only over the layer, as (relative path, kind).

    ``home: ro`` binds every top-level entry except private directories and
    private files (kind ``dir`` or ``file``; symbolic links are recreated in the
    layer, kind ``link``). ``home: hidden`` binds only ``home_expose`` paths.
    Sockets and other special files are left out.
    """

    if settings.home == "ro":
        private_tops = {relative for relative in settings.home_private if "/" not in relative}
        skip = private_tops | set(settings.home_private_files)
        try:
            names = sorted(os.listdir(home))
        except OSError as exc:
            raise SandboxError(f"cannot list $HOME {home}: {exc}") from exc
        candidates = [name for name in names if name not in skip]
    else:
        candidates = list(settings.home_expose)
    entries: list[tuple[str, str]] = []
    for relative in candidates:
        path = home / relative
        if path.is_symlink():
            if "/" not in relative:
                entries.append((relative, "link"))
            elif path.exists():
                entries.append((relative, "dir" if path.is_dir() else "file"))
        elif path.is_dir():
            entries.append((relative, "dir"))
        elif path.is_file():
            entries.append((relative, "file"))
    return entries


_PLACEHOLDERS = "home-placeholders.json"


def prepare_state(view: SlotView, settings: SandboxSettings, home: Path) -> None:
    """Build the slot's private $HOME layer: placeholders, seeded files, private directories.

    Runs as the invoking user before any namespace exists, so everything it
    creates is owned by that user in every isolation mode, and nothing is
    created in the real $HOME. Each real entry the view binds gets an empty
    placeholder of the same kind (a mount point) and each real symbolic link is
    recreated; placeholders and links this function created for entries that
    are no longer bound (gone from $HOME, or a different ``home`` mode) are
    removed, placeholders only while still empty.
    """

    layer = home_layer(view)
    layer.mkdir(parents=True, exist_ok=True, mode=0o700)
    record = view.state_directory / _PLACEHOLDERS
    try:
        loaded = json.loads(record.read_text(encoding="utf-8"))
        previous = {str(item) for item in loaded} if isinstance(loaded, list) else set()
    except (OSError, ValueError):
        previous = set()
    created: set[str] = set()

    def placeholder(relative: str, kind: str) -> None:
        target = layer / relative
        if os.path.lexists(target):
            if (kind == "dir") != (target.is_dir() and not target.is_symlink()):
                print(f"wrkslots run: {target} is in the way of a {kind} mount point", file=sys.stderr)
            elif relative in previous:
                created.add(relative)
            return
        target.parent.mkdir(parents=True, exist_ok=True)
        if kind == "dir":
            target.mkdir(mode=0o700)
        else:
            target.touch(mode=0o600)
        created.add(relative)

    for relative, kind in home_entries(settings, home):
        if kind == "link":
            target = layer / relative
            if not os.path.lexists(target):
                os.symlink(os.readlink(home / relative), target)
                created.add(relative)
            elif relative in previous:
                created.add(relative)
        else:
            placeholder(relative, kind)
    for name in settings.home_private_files:
        target, real = layer / name, home / name
        if not os.path.lexists(target) and real.is_file():
            shutil.copy2(real, target)
            os.chmod(target, 0o600)
    for relative in settings.home_private:
        private_home_directory(view, relative).mkdir(parents=True, exist_ok=True, mode=0o700)
    if settings.home == "hidden":
        # Mount points for shared paths; in "ro" mode they exist in the real tree.
        for relative in settings.home_shared:
            real = home / relative
            if real.exists():
                placeholder(relative, "dir" if real.is_dir() else "file")
    for relative in sorted(previous - created, key=len, reverse=True):
        stale = layer / relative
        try:
            if stale.is_symlink():
                stale.unlink()
            elif stale.is_dir():
                stale.rmdir()
            elif stale.is_file() and stale.stat().st_size == 0:
                stale.unlink()
        except OSError:
            created.add(relative)  # not empty: the agent's own content; keep tracking
    temporary = record.with_name(f"{record.name}.tmp.{os.getpid()}")
    temporary.write_text(json.dumps(sorted(created)), encoding="utf-8")
    os.replace(temporary, record)
    # Earlier drafts kept a persistent per-slot /tmp; /tmp is per launch now.
    legacy_tmp = view.state_directory / "tmp"
    if legacy_tmp.is_dir() and not legacy_tmp.is_symlink():
        shutil.rmtree(legacy_tmp, ignore_errors=True)


def build_spec(view: SlotView, settings: SandboxSettings, home: Path, cwd: Path) -> dict[str, object]:
    """Describe the view the helper must build, as plain JSON.

    Pure except for existence checks, so it is shared by the ``userns`` and
    ``root`` modes and by ``--print``.
    """

    if home == Path("/") or not home.is_absolute():
        raise SandboxError(f"refusing to box a command with HOME={home}")
    user = os.environ.get("USER") or home.name
    aliases = path_aliases(home)
    binds: list[list[str]] = []

    def add(source: Path, target: Path) -> None:
        pair = [str(source), str(target)]
        if pair not in binds:
            binds.append(pair)

    for relative in settings.home_shared:
        path = home / relative
        if path.exists():
            add(path, path)
    for relative in settings.home_private:
        if "/" in relative and (settings.home == "hidden" or (home / relative).is_dir()):
            # Top-level private directories are simply part of the layer.
            add(private_home_directory(view, relative), home / relative)
    add(view.slot_path, view.slot_path)
    for directory in (*view.git_directories, *view.writable):
        add(directory, directory)
    # A slot's box never writes the wrkslots control directory (registry,
    # journals, other slots, slot images, private HOME layers): registry commands
    # such as heartbeat, finish, and write-handoff run outside it. A coordinator
    # box is the one exception: its scope includes the control directory.
    if view.project_root is not None:
        for relative in settings.outputs:
            path = view.project_root / relative
            if path.is_dir():
                add(path, path)
    for item in settings.read_write:
        path = Path(expand_path(item, str(home), user))
        if path.exists():
            add(path, path)
    entries = home_entries(settings, home)
    read_only = [] if view.coordinator else [str(view.control_directory)]
    runtime = spec_directory()
    if runtime is not None:
        # Create it now: the view makes it read-only only if it exists, and a
        # box started before any root launch must not be able to create (and
        # then plant files in) the root launcher's spec directory itself.
        ensure_spec_directory(runtime)
        read_only.append(str(runtime))
    masks = [str(base / relative) for base in aliases for relative in settings.home_hidden]
    # Other slots' and boxes' private HOME layers (which hold private copies of
    # credential files), and in a slot's box the image directory too. This box's
    # own layer is bound over $HOME beforehand. A coordinator box keeps
    # slot-images visible: it creates and removes image slots.
    private_state = ("slot-state", "box-state") if view.coordinator else ("slot-images", "slot-state", "box-state")
    masks += [str(view.control_directory / name) for name in private_state]
    return {
        "home": str(home),
        "home_aliases": [str(alias) for alias in aliases[1:]],
        "home_mode": settings.home,
        "home_layer": str(home_layer(view)),
        "home_binds": [relative for relative, kind in entries if kind != "link"],
        "read_only": read_only,
        "binds": binds,
        "masks": masks,
        "tmp_size": settings.tmp_size,
        "protect_system": settings.protect_system,
        "propagate": [str(path) for path in view.propagate],
        "cwd": str(cwd),
    }


# ---------------------------------------------------------- building the view

_CLONE_NEWNS = 0x00020000
_CLONE_NEWUSER = 0x10000000
_MS_RDONLY = 1
_MS_NOSUID = 2
_MS_NODEV = 4
_MS_NOEXEC = 8
_MS_REMOUNT = 32
_MS_NOATIME = 1024
_MS_NODIRATIME = 2048
_MS_BIND = 4096
_MS_REC = 16384
_MS_PRIVATE = 1 << 18
_MS_SLAVE = 1 << 19
_MS_RELATIME = 1 << 21
_MS_STRICTATIME = 1 << 24
_O_PATH = 0o10000000
_OPTION_FLAGS = {
    "nosuid": _MS_NOSUID,
    "nodev": _MS_NODEV,
    "noexec": _MS_NOEXEC,
    "noatime": _MS_NOATIME,
    "nodiratime": _MS_NODIRATIME,
    "relatime": _MS_RELATIME,
    "strictatime": _MS_STRICTATIME,
}
#: Mount trees left writable even under protect_system, as systemd's
#: ProtectSystem=strict does: kernel interfaces and per-user runtime sockets.
_ALWAYS_WRITABLE_PREFIXES = ("/proc", "/sys", "/dev", "/run/user")


def _libc() -> ctypes.CDLL:
    return ctypes.CDLL(None, use_errno=True)


def _check(result: int, what: str) -> None:
    if result != 0:
        error = ctypes.get_errno()
        raise MountError(f"{what}: {os.strerror(error)}", error)


def _mount(source: str | None, target: str, fstype: str | None, flags: int, data: str | None = None) -> None:
    libc = _libc()
    libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_char_p]
    _check(
        libc.mount(
            source.encode() if source is not None else None,
            target.encode(),
            fstype.encode() if fstype is not None else None,
            flags,
            data.encode() if data is not None else None,
        ),
        f"mount {source or fstype} on {target}",
    )


def _write(path: str, text: str) -> None:
    with open(path, "w", encoding="ascii") as handle:
        handle.write(text)


def _under(path: str, roots: Sequence[str]) -> bool:
    return any(path == root or path.startswith(root.rstrip("/") + "/") for root in roots)


def _mounts() -> list[tuple[str, int]]:
    """(mount point, per-mount flags to preserve) for every mount, parents first."""

    rows: list[tuple[str, int]] = []
    with open("/proc/self/mountinfo", encoding="utf-8", errors="surrogateescape") as handle:
        for line in handle:
            fields = line.split(" - ", 1)[0].split()
            if len(fields) < 6:
                continue
            flags = 0
            for option in fields[5].split(","):
                flags |= _OPTION_FLAGS.get(option, 0)
            rows.append((_unescape(fields[4]), flags))
    return rows


def _remount_read_only(point: str, flags: int | None = None) -> None:
    """Make the topmost mount at ``point`` read-only, keeping its other flags.

    The flags must be preserved: inside a user namespace nosuid/nodev/noexec
    and the atime mode are locked, and as root dropping them would weaken the
    host's own policy.
    """

    if flags is None:
        flags = next((value for mount_point, value in reversed(_mounts()) if mount_point == point), 0)
    _mount(None, point, None, _MS_BIND | _MS_REMOUNT | _MS_RDONLY | flags)


def _remount_tree_read_only(base: str, keep: Sequence[str] = (), keep_exact: Sequence[str] = ()) -> list[str]:
    """Remount ``base`` and every mount below it read-only; return real failures.

    Mounts at or below a ``keep`` path, and mounts exactly at a ``keep_exact``
    path, are left alone.
    """

    failures: list[str] = []
    # A remount by path reaches only the TOPMOST mount at that path, so use
    # the flags of the last mountinfo row for each path (a stacked mount's
    # locked flags differ from the one it covers).
    topmost: dict[str, int] = {}
    for point, flags in _mounts():
        topmost[point] = flags
    for point, flags in topmost.items():
        if not _under(point, [base]) or _under(point, keep) or point in keep_exact:
            continue
        if not os.path.lexists(point):
            continue  # covered by a tmpfs: unreachable, nothing to protect
        try:
            _remount_read_only(point, flags)
        except MountError as exc:
            # EINVAL: the path no longer names that mount's root because a
            # layer above shadows it; unreachable. Other failures (another
            # user's FUSE mount, for example) are harmless when this user
            # could not write there anyway.
            if exc.errno == _errno.EINVAL or not os.access(point, os.W_OK):
                continue
            failures.append(str(exc))
    return failures


def _bind_fd(descriptor: int, target: str) -> None:
    """Bind the pinned ``descriptor`` (recursively) at ``target``, creating a mount point."""

    source_path = f"/proc/self/fd/{descriptor}"
    if not os.path.lexists(target):
        if os.path.isdir(source_path):
            os.makedirs(target, exist_ok=True)
        else:
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "a", encoding="utf-8"):
                pass
    _mount(source_path, target, None, _MS_BIND | _MS_REC)


def _mask(path: str) -> None:
    if not os.path.lexists(path):
        return
    # mount(2) follows symbolic links; name the mount point it will really use.
    path = os.path.realpath(path)
    if not os.path.exists(path):
        return
    if os.path.isdir(path):
        # Owned by the user, so it reads as an empty directory, not EACCES.
        _mount(
            "tmpfs",
            path,
            "tmpfs",
            _MS_NOSUID | _MS_NODEV | _MS_NOEXEC | _MS_RDONLY,
            f"mode=0700,uid={os.getuid()},gid={os.getgid()},size=4k",
        )
    else:
        _mount("/dev/null", path, None, _MS_BIND)
        _remount_read_only(path)


def _same_kind(real: str, placeholder: str) -> bool:
    """A real entry can be bound on its placeholder: both exist, both dirs or both not."""

    if not os.path.exists(real) or os.path.islink(placeholder) or not os.path.exists(placeholder):
        return False
    return os.path.isdir(real) == os.path.isdir(placeholder)


def build_view(spec: Mapping[str, object]) -> None:
    """Build the view described by ``spec`` in the CURRENT (new) mount namespace.

    Shared by both isolation modes. The caller has entered a mount namespace in
    which it holds CAP_SYS_ADMIN, and runs as the invoking user (in ``root``
    mode: uid, gid, and groups already dropped, CAP_SYS_ADMIN kept), so every
    path lookup and access check is the user's own and every file created is
    the user's.
    """

    def strings(key: str) -> list[str]:
        value = spec.get(key, [])
        assert isinstance(value, list)
        return [str(item) for item in value]

    propagate = strings("propagate")
    # Slave propagation keeps host mount events flowing in (a coordinator box
    # sees image slots mounted after it started) while nothing mounted in the
    # box ever reaches the host. Without roots to propagate, everything is private.
    _mount(None, "/", None, _MS_REC | (_MS_SLAVE if propagate else _MS_PRIVATE))
    home = str(spec["home"])
    layer = str(spec["home_layer"])
    mode = str(spec.get("home_mode", "ro"))
    aliases = [alias for alias in strings("home_aliases") if os.path.isdir(alias)]
    raw_binds = spec.get("binds", [])
    assert isinstance(raw_binds, list)
    binds = [
        (str(pair[0]), str(pair[1]))
        for pair in raw_binds
        if isinstance(pair, list) and len(pair) == 2 and os.path.exists(str(pair[0]))
    ]
    home_binds = [
        relative
        for relative in strings("home_binds")
        if _same_kind(os.path.join(home, relative), os.path.join(layer, relative))
    ]
    # Pin every source before anything is covered: once /tmp or $HOME is
    # covered, the real paths below it are reachable only through these
    # descriptors, and nothing has to be staged on the host.
    read_only = [path for path in strings("read_only") if os.path.isdir(path)]
    pinned: dict[str, int] = {}
    for path in [
        layer,
        *aliases,
        *(os.path.join(home, rel) for rel in home_binds),
        *read_only,
        *(src for src, _ in binds),
    ]:
        if path not in pinned:
            pinned[path] = os.open(path, _O_PATH)

    # 1. A fresh /tmp for this launch.
    _mount("tmpfs", "/tmp", "tmpfs", _MS_NOSUID | _MS_NODEV, f"mode=1777,size={spec.get('tmp_size', DEFAULT_TMP_SIZE)}")

    # 2. $HOME: the slot's private layer, with the real entries bound read-only
    #    on their placeholders. The layer itself stays writable, so a harness
    #    can create and rename top-level files; they land in the layer.
    _bind_fd(pinned[layer], home)
    for relative in home_binds:
        target = os.path.join(home, relative)
        _bind_fd(pinned[target], target)
        failures = _remount_tree_read_only(target)
        if failures:
            raise SandboxError(f"could not make {target} read-only: " + "; ".join(failures[:3]))
    # Other paths to the same files: read-only ("ro") or empty ("hidden").
    for alias in aliases:
        if mode == "ro":
            _bind_fd(pinned[alias], alias)
            failures = _remount_tree_read_only(alias)
            if failures:
                raise SandboxError(f"could not make {alias} read-only: " + "; ".join(failures[:3]))
        else:
            _mount("tmpfs", alias, "tmpfs", _MS_NOSUID | _MS_NODEV, "mode=0755,size=16m")

    # 3. Trees that stay read-only whatever protect_system says: the wrkslots
    # control directory (registry, other slots and their mounts) and the root
    # launcher's spec directory. A slot below the control directory is bound
    # writable again next.
    for path in read_only:
        _bind_fd(pinned[path], path)
        failures = _remount_tree_read_only(path)
        if failures:
            raise SandboxError(f"could not make {path} read-only: " + "; ".join(failures[:3]))

    # 4. Writable binds, in order: shared $HOME state, nested private
    # directories, the slot, Git directories, outputs, extra paths.
    for source, target in binds:
        _bind_fd(pinned[source], target)

    # 5. Masks last, so they win over anything bound above.
    for path in strings("masks"):
        _mask(path)
    if mode == "hidden":
        for alias in aliases:
            _remount_read_only(alias)

    # 6. Everything else read-only. The layer mount is skipped by its EXACT
    # path only: the read-only binds below it must not be skipped with it.
    if spec.get("protect_system", True):
        keep = [*(target for _source, target in binds), "/tmp", *_ALWAYS_WRITABLE_PREFIXES]
        failures = _remount_tree_read_only("/", keep, keep_exact=[home])
        if failures:
            raise SandboxError(
                "could not make every mount read-only (refusing to run unprotected): "
                + "; ".join(failures[:3])
            )
    if propagate:
        _limit_propagation(propagate)
    for descriptor in pinned.values():
        os.close(descriptor)
    os.chdir(str(spec.get("cwd", "/")))


def _limit_propagation(roots: Sequence[str]) -> None:
    """Keep host mount events only where ``roots`` are visible; make every other mount private.

    The mount each root resolves through in the view (the topmost mount whose
    mount point is the longest prefix of the root) and every mount below a root
    stay slaves, so an image slot mounted on the host appears at its slot path.
    Any other mount, such as ``/`` itself or a read-only bind of $HOME, stops
    receiving host mounts, which would otherwise arrive writable.
    """

    points = list(dict.fromkeys(point for point, _flags in _mounts()))
    keep: set[str] = set()
    for root in roots:
        covering = [point for point in points if _under(root, [point])]
        if covering:
            keep.add(max(covering, key=len))
    for point in points:
        if point in keep or _under(point, roots) or not os.path.lexists(point):
            continue
        try:
            _mount(None, point, None, _MS_PRIVATE)
        except MountError:
            continue  # shadowed by a mount above it: unreachable


# ------------------------------------------------------------- userns helper


def enter_userns(spec: Mapping[str, object], command: Sequence[str]) -> None:
    """Enter a new user + mount namespace, build the view, exec ``command``."""

    uid, gid = os.getuid(), os.getgid()
    _check(_libc().unshare(_CLONE_NEWUSER | _CLONE_NEWNS), "unshare user+mount namespace")
    _write("/proc/self/setgroups", "deny")
    _write("/proc/self/uid_map", f"{uid} {uid} 1\n")
    _write("/proc/self/gid_map", f"{gid} {gid} 1\n")
    build_view(spec)
    os.execvp(command[0], list(command))


# --------------------------------------------------------------- root helper

_PR_SET_KEEPCAPS = 8
_PR_SET_DUMPABLE = 4
_CAP_SYS_ADMIN = 21
_LINUX_CAPABILITY_VERSION_3 = 0x20080522


class _CapHeader(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]


class _CapData(ctypes.Structure):
    _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]


def _set_capabilities(capabilities: Sequence[int]) -> None:
    """Set the effective and permitted sets to exactly ``capabilities``."""

    header = _CapHeader(_LINUX_CAPABILITY_VERSION_3, 0)
    data = (_CapData * 2)()
    for capability in capabilities:
        index, bit = divmod(capability, 32)
        data[index].effective |= 1 << bit
        data[index].permitted |= 1 << bit
    _check(_libc().capset(ctypes.byref(header), data), "capset")


def _prctl(option: int, value: int) -> None:
    _check(_libc().prctl(option, ctypes.c_ulong(value), 0, 0, 0), f"prctl({option})")


_SPEC_BYTES_LIMIT = 16 * 1024 * 1024
def _sudo_identity() -> tuple[int, int, list[int], str]:
    """The invoking user, from what sudo itself recorded (never from the spec)."""

    import pwd

    try:
        uid = int(os.environ["SUDO_UID"])
        gid = int(os.environ["SUDO_GID"])
    except (KeyError, ValueError) as exc:
        raise SandboxError("enter-root must run through sudo (SUDO_UID/SUDO_GID are missing)") from exc
    if uid == 0:
        raise SandboxError("isolation root boxes a non-root user; the invoking user is root")
    try:
        name = pwd.getpwuid(uid).pw_name
    except KeyError as exc:
        raise SandboxError(f"invoking uid {uid} has no passwd entry") from exc
    groups = sorted(set(os.getgrouplist(name, gid)))
    return uid, gid, groups, name


def _read_root_spec(spec_file: str, uid: int) -> dict[str, object]:
    """Read and delete the launcher's spec, trusting it only if the user alone could write it."""

    try:
        directory = os.stat(os.path.dirname(spec_file) or ".", follow_symlinks=False)
        descriptor = os.open(spec_file, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    except OSError as exc:
        with contextlib.suppress(OSError):
            os.unlink(spec_file)
        raise SandboxError(f"cannot open the root launcher spec: {exc}") from exc
    try:
        info = os.fstat(descriptor)
        problems = []
        if not stat.S_ISDIR(directory.st_mode) or directory.st_uid != uid or directory.st_mode & 0o077:
            problems.append("its directory is not private to the invoking user")
        if not stat.S_ISREG(info.st_mode):
            problems.append("it is not a regular file")
        if info.st_uid != uid:
            problems.append("it is not owned by the invoking user")
        if stat.S_IMODE(info.st_mode) != 0o600:
            problems.append("its mode is not 0600")
        if info.st_nlink != 1:
            problems.append("it has other links")
        if info.st_size > _SPEC_BYTES_LIMIT:
            problems.append("it is too large")
        if problems:
            raise SandboxError("refusing the root launcher spec: " + "; ".join(problems))
        chunks: list[bytes] = []
        while True:
            chunk = os.read(descriptor, 1 << 20)
            if not chunk:
                break
            chunks.append(chunk)
            if sum(len(part) for part in chunks) > _SPEC_BYTES_LIMIT:
                raise SandboxError("refusing the root launcher spec: it is too large")
    finally:
        os.close(descriptor)
        with contextlib.suppress(OSError):
            os.unlink(spec_file)
    try:
        document = json.loads(b"".join(chunks))
    except ValueError as exc:
        raise SandboxError(f"the root launcher spec is not JSON: {exc}") from exc
    if not isinstance(document, dict):
        raise SandboxError("root sandbox spec must be a JSON object")
    return document


def enter_root(spec_file: str) -> None:
    """``sudo -n`` side of ``isolation: root``: build the view, drop to the user, run the command.

    The identity comes from sudo (SUDO_UID, SUDO_GID, and the user's groups from
    the group database); the spec must agree with it. The spec, which carries
    the complete child environment because sudo scrubbed ours, is accepted only
    from a 0600 file owned by the user in a directory only the user can write,
    and is deleted as soon as it is read. Privilege is shed in two steps: uid,
    gid, and groups become the user's immediately, keeping only CAP_SYS_ADMIN,
    so the view is built with the user's own path access (a FUSE mount the user
    owns stays reachable); then that capability is dropped too, and the command
    replaces this process.

    No user namespace is created, so setuid programs inside the box work. No
    PID namespace either: harness launchers that talk to the user's systemd
    manager fail inside one (the bus cannot identify a peer in another PID
    namespace), so host processes stay visible (see the user guide's "Limits of
    the box").
    """

    if os.geteuid() != 0:
        with contextlib.suppress(OSError):
            os.unlink(spec_file)
        raise SandboxError("enter-root must run as root (through sudo -n)")
    try:
        uid, gid, groups, name = _sudo_identity()
    except SandboxError:
        with contextlib.suppress(OSError):
            os.unlink(spec_file)
        raise
    document = _read_root_spec(spec_file, uid)
    view = document["view"]
    assert isinstance(view, dict)
    raw_command = document["command"]
    raw_environment = document["environment"]
    raw_groups = document.get("groups", [])
    if not isinstance(raw_command, list) or not raw_command or not isinstance(raw_environment, dict):
        raise SandboxError("the root launcher spec has no command or environment")
    if not isinstance(raw_groups, list) or not all(isinstance(item, int) for item in raw_groups):
        raise SandboxError("the root launcher spec has invalid groups")
    command = [str(item) for item in raw_command]
    environment = {str(key): str(value) for key, value in raw_environment.items()}
    claimed = (document.get("uid"), document.get("gid"), sorted(set(raw_groups)))
    if claimed != (uid, gid, groups):
        raise SandboxError(
            f"the root launcher spec names uid/gid/groups {claimed[0]}/{claimed[1]}/{claimed[2]}, "
            f"but sudo was invoked by {name} ({uid}/{gid}/{groups}); refusing"
        )
    os.setgroups(groups)
    os.setresgid(gid, gid, gid)
    _prctl(_PR_SET_KEEPCAPS, 1)
    os.setresuid(uid, uid, uid)
    _set_capabilities([_CAP_SYS_ADMIN])
    _prctl(_PR_SET_KEEPCAPS, 0)
    _prctl(_PR_SET_DUMPABLE, 1)
    os.umask(int(str(document.get("umask", 0o022))))
    _check(_libc().unshare(_CLONE_NEWNS), "unshare mount namespace")
    build_view(view)
    _set_capabilities([])
    if os.getuid() != uid or os.geteuid() != uid or os.getgid() != gid:
        raise SandboxError("privilege drop did not take effect")
    os.execvpe(command[0], command, environment)


def _main(argv: Sequence[str] | None = None) -> int:
    """Internal entry points.

    ``sandbox.py enter SPEC-JSON -- COMMAND...``   (userns, as the user)
    ``sandbox.py enter-root SPEC-FILE``            (root, through sudo -n)
    """

    values = list(sys.argv[1:] if argv is None else argv)
    try:
        if len(values) == 2 and values[0] == "enter-root":
            enter_root(values[1])
        elif len(values) >= 4 and values[0] == "enter" and values[2] == "--":
            loaded: object = json.loads(values[1])
            if not isinstance(loaded, dict):
                raise SandboxError("sandbox spec must be a JSON object")
            enter_userns({str(key): value for key, value in loaded.items()}, values[3:])
        else:
            print(
                "usage: sandbox.py enter SPEC-JSON -- COMMAND... | sandbox.py enter-root SPEC-FILE",
                file=sys.stderr,
            )
            return 2
    except (SandboxError, OSError, KeyError, ValueError) as exc:
        print(f"wrkslots sandbox: {exc}", file=sys.stderr)
        return 126
    return 127


# --------------------------------------------------------------- the launcher


def _interpreter() -> str:
    # The real interpreter, not a wrapper script that forks: the PID must not change.
    return os.path.realpath(sys.executable)


def userns_helper(spec: Mapping[str, object], command: Sequence[str]) -> list[str]:
    """Argv that builds the view in a user namespace, then execs ``command``."""

    if not command:
        raise SandboxError("no command to run")
    return [
        _interpreter(),
        "-I",
        str(Path(__file__).resolve()),
        "enter",
        json.dumps(dict(spec), sort_keys=True),
        "--",
        *command,
    ]


def root_helper(spec_file: str) -> list[str]:
    """Argv that builds the view as root through ``sudo -n``, then drops privilege."""

    return ["sudo", "-n", "--", _interpreter(), "-I", str(Path(__file__).resolve()), "enter-root", spec_file]


def check_sudo() -> None:
    """Fail clearly, never prompt, when ``sudo -n`` cannot run without a password."""

    if shutil.which("sudo") is None:
        raise SandboxError("isolation root needs sudo, which is not installed")
    result = subprocess.run(["sudo", "-n", "true"], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.strip() or f"exit {result.returncode}"
        raise SandboxError(
            f"isolation root needs passwordless sudo, and `sudo -n true` failed: {detail}; "
            "use isolation userns or cgroup instead"
        )


def spec_directory() -> Path | None:
    """Private directory for root-launcher specs: in the per-user runtime tmpfs, never on disk."""

    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime or not os.path.isdir(runtime):
        return None
    return Path(runtime) / "wrkslots"


def ensure_spec_directory(directory: Path) -> None:
    """Create the root-launcher spec directory (0700) and check it is a real directory we own."""

    try:
        directory.mkdir(mode=0o700, exist_ok=True)
        info = os.lstat(directory)
    except OSError as exc:
        raise SandboxError(f"cannot create the root launcher spec directory {directory}: {exc}") from exc
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise SandboxError(
            f"root launcher spec directory {directory} must be a directory owned by uid "
            f"{os.getuid()} with mode 0700; remove it and retry"
        )


_STALE_SPEC_SECONDS = 300


def write_root_spec(
    spec: Mapping[str, object], command: Sequence[str], environment: Mapping[str, str]
) -> str:
    """Write the root helper's input to a 0600 file in the user's runtime tmpfs.

    The directory is private (0700) and read-only inside every box, so a boxed
    process cannot tamper with another launch's spec. The helper deletes the
    file as soon as it has read it; specs a failed sudo left behind are removed
    by the next launch.
    """

    directory = spec_directory()
    if directory is None:
        raise SandboxError("isolation root needs XDG_RUNTIME_DIR (a per-user tmpfs) for its launch spec")
    directory.mkdir(mode=0o700, exist_ok=True)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise SandboxError(f"{directory} is not a directory owned by this user")
    os.chmod(directory, 0o700)
    now = time.time()
    for stale in directory.glob("sandbox-*.json"):
        with contextlib.suppress(OSError):
            if now - stale.lstat().st_mtime > _STALE_SPEC_SECONDS:
                stale.unlink()
    umask = os.umask(0o077)
    os.umask(umask)
    document = {
        "view": dict(spec),
        "command": list(command),
        "environment": dict(environment),
        "uid": os.getuid(),
        "gid": os.getgid(),
        "groups": sorted(set(os.getgroups())),
        "umask": umask,
    }
    descriptor, path = tempfile.mkstemp(prefix="sandbox-", suffix=".json", dir=directory)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(document, handle)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(path)
        raise
    return path


def scope_in_place_available() -> bool:
    """Whether a process can join a transient scope without forking (the D-Bus path)."""

    if shutil.which("busctl") is None:
        return False
    result = subprocess.run(
        ["busctl", "--user", "status", "org.freedesktop.systemd1"],
        capture_output=True,
        text=True,
        check=False,
        timeout=10,
    )
    return result.returncode == 0


def place_in_scope(unit: str, slice_unit: str) -> bool:
    """Move THIS process into a new transient scope without forking.

    Keeping the PID matters: a terminal multiplexer that checks whether a pane's
    own shell is at its prompt (Herdr) must still see the same process after
    ``exec wrkslots run ...`` replaces the pane's shell. Returns False when the
    user manager's D-Bus API is unavailable, so the caller can fall back to
    ``systemd-run --scope`` (which forks).
    """

    if shutil.which("busctl") is None:
        return False
    result = subprocess.run(
        [
            "busctl",
            "--user",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
            unit,
            "fail",
            "2",
            "PIDs",
            "au",
            "1",
            str(os.getpid()),
            "Slice",
            "s",
            slice_unit,
            "0",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return False
    for _ in range(100):
        try:
            if f"/{unit}" in Path("/proc/self/cgroup").read_text(encoding="utf-8"):
                return True
        except OSError:
            break
        time.sleep(0.01)
    return False


def run(
    view: SlotView,
    settings: SandboxSettings,
    command: Sequence[str],
    *,
    cwd: Path | None = None,
    print_only: bool = False,
    unit: str | None = None,
) -> int:
    """Replace this process with ``command`` boxed to the slot (or coordinator box).

    Returns only for ``print_only``.
    """

    isolation = settings.isolation
    if isolation not in ISOLATIONS:
        raise SandboxError(f"isolation must be one of {', '.join(ISOLATIONS)}")
    if not command:
        raise SandboxError("no command to run")
    if shutil.which("systemd-run") is None:
        raise SandboxError("systemd-run is not available; the sandbox needs a systemd user manager")
    home = Path(os.environ.get("HOME", str(Path.home())))
    kind = "box" if view.coordinator else "run"
    unit_name = unit or f"wrkslots-{kind}-{systemd_escape(view.slot)}-{os.getpid()}.scope"
    slot_slice = slice_name(view.slot_type, view.slot)
    workdir = cwd or view.slot_path
    if print_only:
        spec = build_spec(view, settings, home, workdir)
        print(f"# isolation {isolation}; scope {unit_name} in {slot_slice}")
        for line in slice_limit_properties(settings.limits):
            print(f"# slice {slot_slice}: {line}")
        for key, value in settings.env:
            print(f"# env {key}={_expand_home(value, str(home))}")
        if isolation == "userns":
            print(" \\\n  ".join(_quote(part) for part in userns_helper(spec, command)))
        elif isolation == "root":
            print(" ".join(_quote(part) for part in root_helper("SPEC-FILE")))
            print(json.dumps({"view": spec, "command": list(command)}, indent=2, sort_keys=True))
        else:
            print(" ".join(_quote(part) for part in command))
        return 0
    if not view.coordinator and isolation != "cgroup" and os.environ.get("WRKSLOTS_BOX"):
        # The coordinator box masks slot-state (other slots' private $HOME
        # layers, which hold seeded credential copies), so a slot's layer cannot
        # be prepared from inside it.
        raise SandboxError(
            f"cannot box a command into slot {view.slot} from inside a coordinator box: the "
            "coordinator box hides every slot's private state. Launch slot agents with "
            "`agentctl start ... --slot` (their terminal panes start outside this box), or run "
            "`wrkslots run` outside the box"
        )
    if isolation == "root":
        check_sudo()
    if isolation != "cgroup":
        try:
            prepare_state(view, settings, home)
        except OSError as exc:
            raise SandboxError(f"cannot prepare the private state of {view.slot}: {exc}") from exc
    for line in apply_slice_limits(view, settings.limits):
        print(f"wrkslots run: limits {line}", file=sys.stderr)
    environment = child_environment(view, settings, os.environ)
    if isolation == "userns":
        argv = userns_helper(build_spec(view, settings, home, workdir), command)
    elif isolation == "root":
        spec_file = write_root_spec(build_spec(view, settings, home, workdir), command, environment)
        argv = root_helper(spec_file)
    else:
        os.chdir(workdir)
        argv = list(command)
    # The scope is established BEFORE sudo in root mode, so the root helper and
    # the command inherit the slot's cgroup.
    try:
        if place_in_scope(unit_name, slot_slice):
            os.execvpe(argv[0], argv, environment)
    except OSError:
        if isolation == "root":
            with contextlib.suppress(OSError):
                os.unlink(argv[-1])
        raise
    print(
        "wrkslots run: the user manager's D-Bus API is unavailable, so the command runs under "
        "`systemd-run --scope`; its process ID differs from the caller's",
        file=sys.stderr,
    )
    fallback = [
        "systemd-run",
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        f"--unit={unit_name}",
        f"--slice={slot_slice}",
        "--",
        *argv,
    ]
    os.execvpe(fallback[0], fallback, environment)


def _quote(value: str) -> str:
    import shlex

    return shlex.quote(value)


if __name__ == "__main__":
    raise SystemExit(_main())
