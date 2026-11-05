"""Strict, project-local launch profiles for :mod:`agentctl`.

Profiles are launch input, not session state.  They live in the ignored private
``.agentctl/profiles.json`` file below the selected working directory and are
read without evaluating shell syntax.
"""
from __future__ import annotations

import json
import os
import re
import stat
import subprocess
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from agentctl.errors import AgentDeliveryError

SCHEMA = "agentctl-profiles/v1"
PROFILE_PATH = Path(".agentctl/profiles.json")
_NAME = re.compile(r"[a-z][a-z0-9-]{0,31}\Z")
_ENVIRONMENT_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")
_SECRET_NAME = re.compile(r"(?:TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|API_KEY|PRIVATE_KEY)", re.I)
_SECRET_OPTION = re.compile(r"^--?(?:api[-_]?key|token|secret|password|passwd|credential)(?:=|$)", re.I)
_EFFORTS = frozenset(("none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"))
_HARNESSES = frozenset(("codex", "claude", "muse", "agy", "agentcloud"))
#: Agentcloud profiles are listed and validated here but only the Rust edition launches them.
AGENTCLOUD_HARNESS = "agentcloud"
_CLOUD_DRIVERS = ("native", "claude-code", "codex", "muse-code")
_CLOUD_TEXT_FIELDS = ("envspec", "purpose", "workspace", "node_id")
_CLOUD_FIELDS = frozenset(("harness", "provision", "title", *_CLOUD_TEXT_FIELDS))
#: Placeholders the Rust launcher expands in ``title``, ``workspace``, and ``node_id``.
_CLOUD_PLACEHOLDERS = ("{name}", "{host}", "{fqdn}", "{cwd}")
#: Fixed sample values that check a template's shape, exactly as the Rust validator does.
_CLOUD_SAMPLE = {"{name}": "agent", "{host}": "host", "{fqdn}": "host.example", "{cwd}": "/directory"}
#: Options the launcher sets itself, or that must match every later agentcloud command.
_CLOUD_RESERVED_OPTIONS = frozenset((
    "--prompt", "--title", "--harness", "--model", "--effort", "--provision", "--envspec",
    "--purpose", "--workspace", "--node-id", "--ws-url", "--client-cert", "--client-key",
    "--as-crewmate",
))
_MODES = frozenset(("interactive", "headless"))
_MUSE_EXEC_OWNED_OPTIONS = frozenset(
    ("--", "--api-key-stdin", "--json", "--session-id", "--prompt-file")
)
_MAX_CONFIG_BYTES = 256 * 1024
_SYSTEM_GIT = Path("/usr/bin/git")


class _DuplicateKey(ValueError):
    pass


@dataclass(frozen=True)
class LaunchProfile:
    """One validated profile; environment values are intentionally not printable."""

    name: str
    harness: str
    mode: str
    model: str | None
    reasoning_effort: str | None
    argv: tuple[str, ...]
    environment: tuple[str, ...]
    agentcloud: dict[str, object] | None = None

    def public(self) -> dict[str, object]:
        """Return discovery metadata without argument or environment values."""
        result: dict[str, object] = {
            "name": self.name,
            "harness": self.harness,
            "mode": self.mode,
            "model": self.model,
            "reasoning_effort": self.reasoning_effort,
            "argv_count": len(self.argv),
            "environment": [entry.partition("=")[0] for entry in self.environment],
        }
        if self.agentcloud is not None:
            result["agentcloud"] = dict(self.agentcloud)
            result["requires_edition"] = "rust"
        return result


def profile_path(cwd: str | Path) -> Path:
    """Return the only implicit profile location for *cwd*."""
    return Path(cwd).expanduser().resolve() / PROFILE_PATH


def _open_private_regular_file(path: Path) -> int:
    """Open and pin a same-user, single-link file below a private directory."""
    try:
        parent = path.parent.lstat()
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect profile config {path}: {exc}") from exc
    if stat.S_ISLNK(parent.st_mode) or not stat.S_ISDIR(parent.st_mode):
        raise AgentDeliveryError(f"profile config parent must be a real directory: {path.parent}")
    uid = os.getuid()
    if parent.st_uid != uid or parent.st_mode & 0o077:
        raise AgentDeliveryError(f"profile config must be private (directory 0700, file 0600): {path}")
    try:
        descriptor = os.open(
            path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
        )
    except OSError as exc:
        raise AgentDeliveryError(f"cannot open profile config {path}: {exc}") from exc
    metadata = os.fstat(descriptor)
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
            or metadata.st_uid != uid or metadata.st_mode & 0o077):
        os.close(descriptor)
        raise AgentDeliveryError(
            f"profile config must be a same-user private single-link regular file: {path}"
        )
    return descriptor


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise _DuplicateKey(f"duplicate key {key!r}")
        result[key] = value
    return result


def _require_ignored(cwd: Path, path: Path) -> None:
    try:
        relative = path.relative_to(cwd)
    except ValueError as exc:
        raise AgentDeliveryError("profile config must remain below the selected working directory") from exc
    try:
        git = Path(os.path.realpath(_SYSTEM_GIT))
        metadata = git.stat()
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0
                or metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH)
                or not os.access(git, os.X_OK)):
            raise AgentDeliveryError(f"refusing unsafe Git executable: {git}")
        checked = subprocess.run(
            [str(git), "-C", str(cwd), "check-ignore", "--quiet", "--", str(relative)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5,
        )
    except AgentDeliveryError:
        raise
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise AgentDeliveryError(f"cannot verify that profile config is ignored: {exc}") from exc
    if checked.returncode != 0:
        raise AgentDeliveryError(
            f"profile config is not ignored by Git: {relative}; add .agentctl/ to .gitignore"
        )


def _text(value: object, field: str, *, optional: bool = False) -> str | None:
    if value is None and optional:
        return None
    if not isinstance(value, str) or not value or "\0" in value:
        raise AgentDeliveryError(f"profile field {field} must be a nonempty NUL-free string")
    return value


def _value_option(value: str, short: str, long: str) -> bool:
    """Recognize both split and attached spellings of a value-taking option."""
    return (value == short or value == long or value.startswith(f"{long}=")
            or (not value.startswith("--") and value.startswith(short)
                and len(value) > len(short)))


def _codex_config_key(argv: tuple[str, ...], index: int) -> str | None:
    """Return the key assigned by one Codex ``-c``/``--config`` occurrence."""
    item = argv[index]
    assignment: str | None = None
    if item in ("-c", "--config"):
        if index + 1 < len(argv):
            assignment = argv[index + 1]
    elif item.startswith("--config="):
        assignment = item.removeprefix("--config=")
    elif not item.startswith("--") and item.startswith("-c") and len(item) > 2:
        assignment = item[2:].removeprefix("=")
    if assignment is None or "=" not in assignment:
        return None
    key = assignment.split("=", 1)[0].strip()
    if len(key) >= 2 and key[0] == key[-1] and key[0] in ('"', "'"):
        return key[1:-1]
    return key


def validate_raw_harness_arguments(
    harness: str, argv: tuple[str, ...] | list[str], *, label: str,
    structured_model: bool = False, structured_effort: bool = False,
    structured_resume: bool = False,
) -> None:
    """Keep structured model and effort settings authoritative over raw argv."""
    validate_structured_harness_argument_conflicts(
        harness, argv, label=label,
        structured_model=structured_model,
        structured_effort=structured_effort,
        structured_resume=structured_resume,
    )
    arguments = tuple(argv)
    option_keys = [item.split("=", 1)[0] for item in arguments if item.startswith("-")]
    config_keys = {
        key for index in range(len(arguments))
        if (key := _codex_config_key(arguments, index)) is not None
    } if harness == "codex" else set()
    has_codex_config = harness == "codex" and any(
        _value_option(item, "-c", "--config") for item in arguments
    )
    if (any(_value_option(item, "-m", "--model") for item in arguments)
            or "model" in config_keys):
        raise AgentDeliveryError(f"{label} must set model with the model field")
    if (any(key in ("--reasoning-effort", "--effort") for key in option_keys)
            or any(item.startswith("model_reasoning_effort=") for item in arguments)
            or "model_reasoning_effort" in config_keys):
        raise AgentDeliveryError(
            f"{label} must set reasoning effort with the reasoning_effort field"
        )
    if (harness == "codex" and (structured_model or structured_effort)
            and (has_codex_config
                 or any(_value_option(item, "-p", "--profile") for item in arguments))):
        raise AgentDeliveryError(
            f"{label} cannot combine structured model or reasoning effort "
            "with raw Codex config or profile arguments"
        )


def validate_structured_harness_argument_conflicts(
    harness: str, argv: tuple[str, ...] | list[str], *, label: str,
    structured_model: bool = False, structured_effort: bool = False,
    structured_resume: bool = False,
) -> None:
    """Reject only raw arguments that duplicate an active structured selector.

    The primary CLI and profiles apply the stricter raw-policy validator above.
    This narrower check protects direct API callers while leaving raw arguments
    unchanged when the corresponding structured field is absent.
    """
    arguments = tuple(argv)
    option_keys = [item.split("=", 1)[0] for item in arguments if item.startswith("-")]
    config_keys = {
        key for index in range(len(arguments))
        if (key := _codex_config_key(arguments, index)) is not None
    } if harness == "codex" else set()
    if structured_model and (
        any(_value_option(item, "-m", "--model") for item in arguments)
        or "model" in config_keys
    ):
        raise AgentDeliveryError(f"{label} must set model with the model field")
    if structured_effort and (
        any(key in ("--reasoning-effort", "--effort") for key in option_keys)
        or any(item.startswith("model_reasoning_effort=") for item in arguments)
        or "model_reasoning_effort" in config_keys
    ):
        raise AgentDeliveryError(
            f"{label} must set reasoning effort with the reasoning_effort field"
        )
    if (harness == "codex" and (structured_model or structured_effort)
            and ("profile" in config_keys
                 or any(_value_option(item, "-p", "--profile") for item in arguments))):
        raise AgentDeliveryError(
            f"{label} cannot combine structured model or reasoning effort "
            "with a raw Codex profile selector"
        )
    duplicate_resume = (
        harness in ("codex", "muse") and "resume" in arguments
    ) or (
        harness == "claude" and any(
            _value_option(item, "-r", "--resume")
            or item in ("-c", "--continue")
            for item in arguments
        )
    )
    if structured_resume and duplicate_resume:
        raise AgentDeliveryError(
            f"{label} cannot repeat the structured resume selector in raw arguments"
        )


def _single_line(value: object) -> bool:
    return (isinstance(value, str) and bool(value.strip())
            and not any(character in value for character in "\0\n\r"))


def _expand_cloud_template(label: str, field: str, template: str) -> str:
    """Expand ``template`` with the sample values; refuse any unknown placeholder."""
    expanded: list[str] = []
    rest = template
    while (start := rest.find("{")) >= 0:
        expanded.append(rest[:start])
        end = rest.find("}", start)
        placeholder = rest[start:end + 1] if end >= 0 else ""
        if placeholder not in _CLOUD_PLACEHOLDERS:
            raise AgentDeliveryError(
                f"{label} agentcloud {field} {template!r} has an unknown placeholder; "
                f"use {', '.join(_CLOUD_PLACEHOLDERS)}"
            )
        expanded.append(_CLOUD_SAMPLE[placeholder])
        rest = rest[start + len(placeholder):]
    expanded.append(rest)
    return "".join(expanded)


def _agentcloud_settings(name: str, raw: object) -> dict[str, object]:
    """Validate an ``agentcloud`` block with the same rules as the Rust launcher."""
    label = f"profile {name!r}"
    if raw is None:
        raw = {}
    if not isinstance(raw, dict):
        raise AgentDeliveryError(f"{label} agentcloud must be an object")
    unknown = sorted(set(raw) - _CLOUD_FIELDS)
    if unknown:
        raise AgentDeliveryError(f"{label} agentcloud has unknown fields: {', '.join(unknown)}")
    driver = raw.get("harness")
    if driver is not None and driver not in _CLOUD_DRIVERS:
        raise AgentDeliveryError(
            f"{label} has unsupported agentcloud driver {driver!r}; use one of {', '.join(_CLOUD_DRIVERS)}"
        )
    provision = raw.get("provision", False)
    if not isinstance(provision, bool):
        raise AgentDeliveryError(f"{label} agentcloud provision must be true or false")
    settings: dict[str, object] = {"harness": driver, "provision": provision}
    expanded: dict[str, object] = {}
    for field in (*_CLOUD_TEXT_FIELDS, "title"):
        value = raw.get(field)
        sample = value
        if isinstance(value, str) and field in ("workspace", "node_id", "title"):
            sample = _expand_cloud_template(label, field, value)
        if sample is not None and not _single_line(sample):
            raise AgentDeliveryError(
                f"{label} agentcloud {field} must be a nonempty single-line string"
            )
        expanded[field] = sample
        # Like the Rust edition, an absent title is omitted rather than listed as null.
        if field != "title" or value is not None:
            settings[field] = value
    if not provision and (settings["envspec"] is not None or settings["purpose"] is not None):
        raise AgentDeliveryError(
            f"{label} sets an agentcloud envspec or purpose without provision; "
            "enable provision (--provision) or remove them"
        )
    if provision and settings["node_id"] is not None:
        raise AgentDeliveryError(
            f"{label} cannot both provision a fresh node and bind node_id; choose one"
        )
    workspace = expanded["workspace"]
    if isinstance(workspace, str) and not workspace.startswith("/"):
        raise AgentDeliveryError(
            f"{label} agentcloud workspace must be an absolute path on the session's node"
        )
    return settings


def validate_agentcloud_create_arguments(argv: tuple[str, ...], *, label: str) -> None:
    """Allow only literal long ``agentcloudctl create`` options that the launcher does not own."""
    for item in argv:
        key = item.split("=", 1)[0]
        if not item.startswith("--") or key == "--":
            raise AgentDeliveryError(
                f"{label} agentcloud argument {item!r} must be a long option; join values as "
                "--option=value because positional values and short options are reserved"
            )
        if key in _CLOUD_RESERVED_OPTIONS:
            raise AgentDeliveryError(
                f"{label} cannot pass {key} through raw agentcloud arguments; agentctl sets it "
                "from the name, brief, model, reasoning effort, or structured agentcloud "
                "settings, and endpoint or identity options must match every later agentctl verb"
            )


def _profile(name: str, raw: object) -> LaunchProfile:
    if not _NAME.fullmatch(name):
        raise AgentDeliveryError(f"invalid profile name {name!r}; use lowercase letters, digits, and hyphens")
    if not isinstance(raw, dict):
        raise AgentDeliveryError(f"profile {name!r} must be an object")
    allowed = {"harness", "mode", "model", "reasoning_effort", "argv", "env", "agentcloud"}
    unknown = sorted(set(raw) - allowed)
    if unknown:
        raise AgentDeliveryError(f"profile {name!r} has unknown fields: {', '.join(unknown)}")
    harness = _text(raw.get("harness"), f"{name}.harness")
    mode = _text(raw.get("mode"), f"{name}.mode")
    assert harness is not None and mode is not None
    if harness not in _HARNESSES:
        raise AgentDeliveryError(f"profile {name!r} has unsupported harness {harness!r}")
    if mode not in _MODES:
        raise AgentDeliveryError(f"profile {name!r} has unsupported mode {mode!r}")
    supported = (
        harness in ("codex", "claude", "muse", AGENTCLOUD_HARNESS)
        if mode == "interactive"
        else harness in ("codex", "agy", "muse")
    )
    if not supported:
        raise AgentDeliveryError(
            f"profile {name!r} has unsupported harness/mode combination "
            f"{harness!r}/{mode!r}"
        )
    model = _text(raw.get("model"), f"{name}.model", optional=True)
    effort = _text(raw.get("reasoning_effort"), f"{name}.reasoning_effort", optional=True)
    if effort is not None and effort not in _EFFORTS:
        raise AgentDeliveryError(f"profile {name!r} has unsupported reasoning effort {effort!r}")
    raw_argv = raw.get("argv", [])
    if not isinstance(raw_argv, list) or any(not isinstance(item, str) or not item or "\0" in item for item in raw_argv):
        raise AgentDeliveryError(f"profile {name!r} argv must be an array of nonempty NUL-free strings")
    argv = tuple(cast(list[str], raw_argv))
    if mode == "headless" and harness != "muse" and argv:
        raise AgentDeliveryError(
            f"profile {name!r} headless {harness} does not support raw argv"
        )
    if mode == "headless" and harness != "muse" and effort is not None:
        raise AgentDeliveryError(
            f"profile {name!r} headless {harness} does not support reasoning_effort"
        )
    for index, item in enumerate(argv):
        if _SECRET_OPTION.match(item) or (index and _SECRET_OPTION.match(argv[index - 1])):
            raise AgentDeliveryError(f"profile {name!r} argv appears to contain a secret; use the harness credential store")
    agentcloud: dict[str, object] | None = None
    if harness == AGENTCLOUD_HARNESS:
        validate_agentcloud_create_arguments(argv, label=f"profile {name!r}")
        agentcloud = _agentcloud_settings(name, raw.get("agentcloud"))
    elif raw.get("agentcloud") is not None:
        # An explicit null means absent, as in the Rust edition.
        raise AgentDeliveryError(
            f"profile {name!r} has an agentcloud block but harness {harness!r}; "
            'use harness "agentcloud" or remove the block'
        )
    else:
        # Structured and runner-owned settings never enter raw argv. This removes
        # order-dependent precedence and keeps the headless session/prompt binding
        # under agentctl's control.
        validate_raw_harness_arguments(
            harness, argv, label=f"profile {name!r}",
            structured_model=model is not None, structured_effort=effort is not None,
        )
    if harness == "muse" and mode == "headless":
        validate_muse_headless_arguments(argv, profile=name)
    raw_env = raw.get("env", {})
    if not isinstance(raw_env, dict):
        raise AgentDeliveryError(f"profile {name!r} env must be an object")
    if mode == "headless" and raw_env:
        raise AgentDeliveryError(
            f"profile {name!r} headless mode does not support environment entries"
        )
    environment: list[str] = []
    for key in sorted(raw_env):
        value = raw_env[key]
        if not isinstance(key, str) or _ENVIRONMENT_NAME.fullmatch(key) is None or "\0" in key:
            raise AgentDeliveryError(f"profile {name!r} has an invalid environment variable name")
        if _SECRET_NAME.search(key):
            raise AgentDeliveryError(f"profile {name!r} environment {key!r} appears secret; use the harness credential store")
        if not isinstance(value, str) or "\0" in value:
            raise AgentDeliveryError(f"profile {name!r} environment values must be NUL-free strings")
        environment.append(f"{key}={value}")
    return LaunchProfile(name, harness, mode, model, effort, argv, tuple(environment), agentcloud)


def _workspace(value: object) -> str | None:
    if value is None:
        return None
    try:
        encoded_length = len(value.encode()) if isinstance(value, str) else 0
    except UnicodeEncodeError:
        encoded_length = 129
    if (not isinstance(value, str) or not value or encoded_length > 128
            or value.strip() != value
            or any(unicodedata.category(character) == "Cc" for character in value)):
        raise AgentDeliveryError(
            "profile config workspace must be 1-128 bytes, have no surrounding "
            "whitespace, and contain no control characters"
        )
    return value


def load_configuration(
    cwd: str | Path, *, absent_ok: bool = False,
) -> tuple[Path, dict[str, LaunchProfile], str | None]:
    """Load the ignored private project configuration for *cwd*."""
    root = Path(cwd).expanduser().resolve()
    path = profile_path(root)
    try:
        path.lstat()
    except FileNotFoundError:
        if absent_ok:
            return path, {}, None
        raise AgentDeliveryError(f"profile config does not exist: {path}") from None
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect profile config {path}: {exc}") from exc
    descriptor = _open_private_regular_file(path)
    try:
        _require_ignored(root, path)
        chunks: list[bytes] = []
        remaining = _MAX_CONFIG_BYTES + 1
        while remaining:
            chunk = os.read(descriptor, remaining)
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        data = b"".join(chunks)
    except Exception:
        os.close(descriptor)
        raise
    os.close(descriptor)
    if len(data) > _MAX_CONFIG_BYTES:
        raise AgentDeliveryError(
            f"profile config exceeds {_MAX_CONFIG_BYTES} bytes: {path}"
        )
    try:
        raw: object = json.loads(data.decode("utf-8"), object_pairs_hook=_unique_object)
    except (UnicodeError, json.JSONDecodeError, RecursionError, _DuplicateKey) as exc:
        raise AgentDeliveryError(f"cannot read profile config {path}: {exc}") from exc
    if (not isinstance(raw, dict)
            or not {"schema", "profiles"}.issubset(raw)
            or set(raw) - {"schema", "profiles", "workspace"}):
        raise AgentDeliveryError(
            "profile config must contain schema and profiles, with optional workspace"
        )
    if raw.get("schema") != SCHEMA or not isinstance(raw.get("profiles"), dict):
        raise AgentDeliveryError(f"profile config must use schema {SCHEMA!r} and an object of profiles")
    profiles = {name: _profile(name, value) for name, value in cast(dict[str, object], raw["profiles"]).items()}
    return path, dict(sorted(profiles.items())), _workspace(raw.get("workspace"))


def load_profiles(cwd: str | Path, *, absent_ok: bool = False) -> tuple[Path, dict[str, LaunchProfile]]:
    """Load and validate the ignored private profile file for *cwd*."""
    path, profiles, _project_workspace = load_configuration(cwd, absent_ok=absent_ok)
    return path, profiles


def configuration_root(
    cwd: str | Path, registry: str | Path, *, absent_ok: bool = False,
) -> Path:
    """Choose the directory whose ``.agentctl/profiles.json`` governs a start in *cwd*.

    *cwd* wins when it has a profile file. Otherwise a conventional registry's
    project (the parent of a registry directory named ``.agentctl``) is used,
    the same project workspace policy is read from, so an agent started in a
    worktree outside that project still finds the project's profiles. When
    neither has one, the refusal names both paths, or with *absent_ok* *cwd* is
    returned.
    """
    root = Path(cwd).expanduser().resolve()
    candidates = [root]
    registry_root = Path(registry).expanduser().resolve()
    if registry_root.name == ".agentctl" and registry_root.parent != root:
        candidates.append(registry_root.parent)
    for candidate in candidates:
        try:
            profile_path(candidate).lstat()
        except FileNotFoundError:
            continue
        except OSError as exc:
            raise AgentDeliveryError(
                f"cannot inspect profile config {profile_path(candidate)}: {exc}"
            ) from exc
        return candidate
    if len(candidates) > 1 and not absent_ok:
        raise AgentDeliveryError(
            f"profile config does not exist: {profile_path(candidates[0])} "
            f"(nor beside the registry: {profile_path(candidates[1])})"
        )
    return root


def workspace_for_registry(registry: str | Path) -> str | None:
    """Read only workspace policy for a conventional project registry.

    Profile validation belongs to ``profiles`` and ``start --profile``.  A typo
    in an unrelated launch profile must not disable ordinary managed-agent
    operations that only need the optional placement label.
    """
    root = Path(registry).expanduser().resolve()
    if root.name != ".agentctl":
        return None
    project = root.parent
    path = profile_path(project)
    try:
        path.lstat()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect profile config {path}: {exc}") from exc
    descriptor = _open_private_regular_file(path)
    try:
        _require_ignored(project, path)
        data = os.read(descriptor, _MAX_CONFIG_BYTES + 1)
    finally:
        os.close(descriptor)
    if len(data) > _MAX_CONFIG_BYTES:
        raise AgentDeliveryError(
            f"profile config exceeds {_MAX_CONFIG_BYTES} bytes: {path}"
        )
    try:
        raw: object = json.loads(data.decode("utf-8"), object_pairs_hook=_unique_object)
    except (UnicodeError, json.JSONDecodeError, RecursionError, _DuplicateKey) as exc:
        raise AgentDeliveryError(f"cannot read profile config {path}: {exc}") from exc
    if not isinstance(raw, dict):
        raise AgentDeliveryError("profile config must be an object")
    return _workspace(raw.get("workspace"))


def validate_muse_headless_arguments(
    argv: tuple[str, ...] | list[str], *, profile: str | None = None,
) -> None:
    """Keep Muse exec's subcommand, session identity, and prompt runner-owned."""
    label = f"profile {profile!r}" if profile is not None else "Muse headless launch"
    # Sessions compiles structured settings to two-token pairs before this
    # final validation. Other option values must remain in one literal
    # ``--key=value`` element, so an unowned positional can never replace the
    # runner's prompt.
    seen_singletons: set[str] = set()
    index = 0
    while index < len(argv):
        item = argv[index]
        key = item.split("=", 1)[0]
        if key in _MUSE_EXEC_OWNED_OPTIONS or item in ("exec", "resume"):
            raise AgentDeliveryError(f"{label} cannot override runner-owned option {key!r}")
        if item in ("--model", "--reasoning-effort", "--effort"):
            if item in seen_singletons:
                raise AgentDeliveryError(
                    f"{label} repeats singleton option {item!r}; precedence must be explicit"
                )
            if index + 1 >= len(argv) or argv[index + 1].startswith("-"):
                raise AgentDeliveryError(f"{label} option {item!r} requires one value")
            seen_singletons.add(item)
            index += 2
            continue
        if not item.startswith("-"):
            raise AgentDeliveryError(
                f"{label} argv must use --option=value for options with values; positional arguments are reserved for the prompt"
            )
        if key in ("--model", "--reasoning-effort", "--effort"):
            if key in seen_singletons:
                raise AgentDeliveryError(
                    f"{label} repeats singleton option {key!r}; precedence must be explicit"
                )
            seen_singletons.add(key)
        index += 1


def reasoning_arguments(harness: str, effort: str | None) -> tuple[str, ...]:
    """Translate one explicit effort without inventing permission policy."""
    if effort is None:
        return ()
    if effort not in _EFFORTS:
        raise AgentDeliveryError(f"unsupported reasoning effort {effort!r}")
    if harness == "codex":
        return ("--config", f"model_reasoning_effort={effort}")
    if harness == "claude":
        return ("--effort", effort)
    if harness == "muse":
        return ("--reasoning-effort", effort)
    raise AgentDeliveryError(f"reasoning effort is not supported for harness {harness!r}")
