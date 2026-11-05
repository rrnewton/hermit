"""Token-bound recovery of an owned interactive generation, without queue replay."""
from __future__ import annotations

import copy
import hashlib
import json
import math
import os
import re
import stat
import time
import uuid
from contextlib import nullcontext
from dataclasses import asdict, dataclass, replace
from pathlib import Path
from typing import cast

from agentctl import agent
from agentctl.client import AgentPaneInfo, CustomProcessIdentity, HerdrClient, Pane, herdr_error_code
from agentctl.errors import AgentDeliveryError, HerdrRunError, HerdrUnavailable, RecoveryAction, _with_recovery
from agentctl.profiles import configuration_root, load_profiles, reasoning_arguments, validate_structured_harness_argument_conflicts
from agentctl.subagents import (
    AgentRecord, ManagedAgents, _MAX_AGENT_RECORD_BYTES, _NAME, _SHA256,
    _PinnedAgentDirectory, _missing_target, _name, _native_session,
    _rename_directory_noreplace_at, _slot_shell_command, harness_arguments,
)

_TOKEN = re.compile(r"[a-z0-9-]{1,80}\Z")
_JOURNAL_KEYS = {
    "schema", "name", "old_token", "new_token", "phase", "started_at",
    "old_directory_device", "old_directory_inode", "new_directory_device", "new_directory_inode",
    "old_record_sha256", "stopped_record_sha256", "ready_record_sha256", "proof",
}
_PROOF_KEYS = {
    "kind", "pane_id", "tab_id", "workspace_id", "cwd", "terminal_id",
    "reported_agent", "reported_session_agent", "reported_session_value", "shell", "shell_executable",
}


def _digest(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def journals(manager: ManagedAgents) -> list[dict[str, object]]:
    """Read both editions' journals without creating or tightening anything."""
    directory = manager.registry / ".revives"
    if not os.path.lexists(directory):
        return []
    agent._validate_private_directory(str(directory), "revive journal directory")
    found: list[dict[str, object]] = []
    for path in sorted(directory.iterdir()):
        if path.name.startswith("."):
            continue
        if _TOKEN.fullmatch(path.name):
            agent._validate_private_directory(str(path), "revive operation directory")
            continue
        if not path.name.endswith(".json"):
            raise AgentDeliveryError("unexpected entry in revive journal directory")
        with manager._pinned_parent_directory(directory, label="revive journal directory") as parent:
            pinned = _PinnedAgentDirectory("revive", directory, parent.descriptor, parent.device, parent.inode)
            value = json.loads(manager._record_bytes(pinned, file_name=path.name))
        if not isinstance(value, dict) or set(value) != _JOURNAL_KEYS:
            raise AgentDeliveryError("invalid revive journal")
        journal = cast(dict[str, object], value)
        name, old, new, started, proof = (journal[key] for key in ("name", "old_token", "new_token", "started_at", "proof"))
        try:
            finite_start = not isinstance(started, bool) and isinstance(started, (int, float)) and math.isfinite(started)
        except OverflowError:
            finite_start = False
        if (journal["schema"] != "agentctl-revive/v1"
                or not isinstance(name, str) or not _NAME.fullmatch(name) or name == "archive"
                or not isinstance(old, str) or not _TOKEN.fullmatch(old) or path.name != f"{old}.json"
                or not isinstance(new, str) or not _TOKEN.fullmatch(new) or old == new
                or journal["phase"] not in ("launching", "ready", "published")
                or not finite_start
                or any(not isinstance(journal[key], int) or isinstance(journal[key], bool)
                       or not 0 < cast(int, journal[key]) < 1 << 64 for key in (
                           "old_directory_device", "old_directory_inode", "new_directory_device", "new_directory_inode"))
                or any(not isinstance(journal[key], str) or not _SHA256.fullmatch(cast(str, journal[key]))
                       for key in ("old_record_sha256", "stopped_record_sha256"))
                or (journal["ready_record_sha256"] is not None and (
                    not isinstance(journal["ready_record_sha256"], str)
                    or not _SHA256.fullmatch(journal["ready_record_sha256"])))
                or (journal["phase"] != "launching" and journal["ready_record_sha256"] is None)
                or not isinstance(proof, dict) or set(proof) != _PROOF_KEYS):
            raise AgentDeliveryError("invalid revive journal")
        _validate_proof(cast(dict[str, object], proof))
        found.append(journal)
    if len({journal["name"] for journal in found}) != len(found):
        raise AgentDeliveryError("multiple revive journals claim one name")
    return found


def _validate_proof(proof: dict[str, object]) -> None:
    if (proof["kind"] not in ("idle_shell", "missing")
            or any(not isinstance(proof[key], str) or not proof[key] or "\0" in cast(str, proof[key])
                   for key in ("pane_id", "tab_id", "workspace_id", "cwd"))
            or not Path(cast(str, proof["cwd"])).is_absolute()
            or any(proof[key] is not None and (not isinstance(proof[key], str) or not proof[key]
                                               or "\0" in cast(str, proof[key]))
                   for key in ("terminal_id", "reported_agent", "reported_session_agent", "reported_session_value", "shell_executable"))):
        raise AgentDeliveryError("invalid revive presentation proof")
    if proof["shell_executable"] is not None and not Path(cast(str, proof["shell_executable"])).is_absolute():
        raise AgentDeliveryError("invalid revive shell executable")
    if proof["kind"] == "missing":
        if any(proof[key] is not None for key in ("terminal_id", "reported_agent", "reported_session_agent", "reported_session_value", "shell", "shell_executable")):
            raise AgentDeliveryError("invalid missing revive presentation proof")
    else:
        shell = proof["shell"]
        if not isinstance(shell, dict) or set(shell) != {"version", "boot_id", "pid", "starttime_ticks", "executable_device", "executable_inode"}:
            raise AgentDeliveryError("invalid revive shell proof")
        # The record decoder is the shared strict identity validator.
        identity = AgentRecord._from_value({
            "schema": 1, "name": "proof", "token": "proof", "harness": "muse", "cwd": "/",
            "adapter": "herdr-pane", "pane_id": proof["pane_id"],
            "created_at": 0, "lifecycle": "running", "arguments": [], "custom_process_identity": shell,
        }, Path("revive-proof"), "proof").custom_process_identity
        if identity is None or proof["shell_executable"] is None:
            raise AgentDeliveryError("invalid revive shell proof")


def refuse_pending(manager: ManagedAgents, names: tuple[str, ...]) -> None:
    """Refuse control of any name reserved by an incomplete revive."""
    for journal in journals(manager):
        name = journal["name"]
        if isinstance(name, str) and name in names:
            raise _with_recovery(AgentDeliveryError(
                f"revive of {name!r} is incomplete; rerun `agentctl revive {name}`"
            ), RecoveryAction(
                "revive", name=name, token=cast(str, journal["old_token"]),
            ), stop_reason=f"revive of {name!r} is incomplete")


def _paths(manager: ManagedAgents, journal: dict[str, object]) -> tuple[Path, Path, Path]:
    operation = manager.registry / ".revives" / cast(str, journal["old_token"])
    return operation, operation / "new", manager.registry / "archive" / f"{journal['name']}-{journal['old_token']}"


def _read_record(manager: ManagedAgents, path: Path, name: str, *, identity: tuple[int, int] | None = None,
                 digest: str | None = None) -> tuple[AgentRecord, bytes, tuple[int, int]]:
    with manager._pinned_parent_directory(path, label="revive record directory") as parent:
        if identity is not None and (parent.device, parent.inode) != identity:
            raise AgentDeliveryError("revive record directory generation changed")
        pinned = _PinnedAgentDirectory(name, path, parent.descriptor, parent.device, parent.inode)
        content = manager._record_bytes(pinned)
        if digest is not None and _digest(content) != digest:
            raise AgentDeliveryError("revive record bytes changed")
        try:
            value: object = json.loads(content)
        except (ValueError, UnicodeError) as exc:
            raise AgentDeliveryError("invalid revive record") from exc
        record = AgentRecord._from_value(value, path / "agent.json", name)
        return record, content, (parent.device, parent.inode)


def candidate_records(manager: ManagedAgents) -> list[AgentRecord]:
    """Both generations retain identity claims until stale cleanup finishes."""
    records: list[AgentRecord] = []
    for journal in journals(manager):
        _operation, candidate, archive = _paths(manager, journal)
        name = cast(str, journal["name"])
        old_path = archive if os.path.lexists(archive) else manager._directory(name)
        old, old_bytes, _held = _read_record(manager, old_path, name, identity=_identity(journal, "old"))
        if (old.token != journal["old_token"]
                or _digest(old_bytes) not in (journal["old_record_sha256"], journal["stopped_record_sha256"])
                or old_path == archive and _digest(old_bytes) != journal["stopped_record_sha256"]):
            raise AgentDeliveryError("old revive generation changed while reserving identities")
        records.append(old)
        path = candidate if os.path.lexists(candidate) else manager._directory(cast(str, journal["name"]))
        record, _content, _candidate_identity = _read_record(manager, path, cast(str, journal["name"]), identity=(
            cast(int, journal["new_directory_device"]), cast(int, journal["new_directory_inode"]),
        ))
        if record.token != journal["new_token"]:
            raise AgentDeliveryError("revive candidate token changed")
        if journal["ready_record_sha256"] is not None and _digest(_content) != journal["ready_record_sha256"]:
            raise AgentDeliveryError("ready revive record changed while reserving identities")
        records.append(record)
    return records


def _conversation(record: AgentRecord) -> str:
    if record._session_source != "asserted" and (
            (record.session_agent is None) != (record.session_value is None)
            or record.session_agent not in (None, record.harness)):
        raise AgentDeliveryError("recorded native conversation provider is incomplete or conflicts with the harness")
    values = [value for value in (
        None if record.native_session is None else record.native_session["value"], record.resume,
        record.session_value if record._session_source != "asserted" else None,
    ) if value is not None]
    if not values or any(not value or "\0" in value for value in values):
        raise AgentDeliveryError("record has no usable native conversation; supply its reported ID when starting a replacement")
    if len(set(values)) != 1:
        raise AgentDeliveryError("recorded native conversation identities conflict")
    return values[0]


def _extras(record: AgentRecord) -> tuple[str, ...]:
    arguments = tuple(record.arguments)
    if record.harness == "claude" and arguments[:1] == ("--session-id",):
        if len(arguments) < 2 or arguments[1] != _conversation(record) or record.resume is not None:
            raise AgentDeliveryError("recorded Claude conversation selector conflicts")
        arguments = arguments[2:]
    old_resume = record.resume
    if record.harness == "muse" and old_resume is not None:
        if arguments[-2:] != ("resume", old_resume):
            raise AgentDeliveryError("recorded Muse conversation selector conflicts")
        arguments, old_resume = arguments[:-2], None
    prefix = (*harness_arguments(record.harness, model=record.model, resume=old_resume),
              *reasoning_arguments(record.harness, record.reasoning_effort))
    if arguments[:len(prefix)] != prefix:
        raise AgentDeliveryError("recorded structured launch policy conflicts with its arguments")
    extra = arguments[len(prefix):]
    validate_structured_harness_argument_conflicts(record.harness, extra, label="recorded revive policy",
        structured_model=record.model is not None, structured_effort=record.reasoning_effort is not None, structured_resume=True)
    if record.harness == "claude" and any(
        argument.split("=", 1)[0] in ("--session-id", "--resume", "--continue", "--fork-session", "-r", "-c")
        or argument.startswith(("-r", "-c")) and not argument.startswith("--") for argument in extra
    ) or record.harness == "muse" and "resume" in extra:
        raise AgentDeliveryError("recorded raw conversation selector conflicts with revive")
    return extra


@dataclass(frozen=True)
class _LaunchPolicy:
    arguments: tuple[str, ...]
    environment: tuple[str, ...]
    slot_command: str | None
    relay_command: str | None
    workspace_id: str | None


def _policy(manager: ManagedAgents, record: AgentRecord) -> _LaunchPolicy:
    if (record.lifecycle != "running" or record.mode != "interactive" or record.backend != "herdr"
            or record.adapter not in ("herdr", "herdr-pane", "herdr-relay")
            or record.harness not in ("claude", "codex", "muse")):
        raise AgentDeliveryError("revive requires a running owned Claude, Codex, or Muse interactive Herdr record")
    if record._unknown.get("project_box") is not None:
        raise AgentDeliveryError("this edition cannot reconstruct the recorded project box")
    if manager._pending_move_destination(record) is not None:
        raise AgentDeliveryError("record has an incomplete move; finish that move before revive")
    if not Path(record.cwd).is_dir() or str(Path(record.cwd).resolve()) != record.cwd:
        raise AgentDeliveryError("recorded working directory is missing or changed")
    extra = _extras(record)
    environment: tuple[str, ...] = ()
    if record.profile is not None:
        root = configuration_root(record.cwd, manager.registry)
        _path, profiles = load_profiles(root)
        selected = profiles.get(record.profile)
        if selected is None:
            raise AgentDeliveryError("recorded launch profile is missing")
        if (selected.harness != record.harness or selected.mode != record.mode
                or selected.model != record.model or selected.reasoning_effort != record.reasoning_effort
                or selected.argv != extra):
            raise AgentDeliveryError("recorded launch profile policy changed (harness, mode, model, effort, or arguments)")
        environment = selected.environment
    if sorted({entry.partition("=")[0] for entry in environment}) != sorted(record.environment_names):
        raise AgentDeliveryError("recorded environment names cannot be reconstructed from the private launch profile")
    arguments = harness_arguments(record.harness, model=record.model, resume=_conversation(record),
        extra=(*reasoning_arguments(record.harness, record.reasoning_effort), *extra))
    slot_command = relay_command = None
    if record.slot is not None:
        if record.slot_project is None or record.slot_isolation is None:
            raise AgentDeliveryError("recorded slot lacks its project or isolation policy")
        slot_command, cwd, isolation = _slot_shell_command(record.slot, isolation=record.slot_isolation,
            project=record.slot_project, explicit_project=True)
        if str(Path(cwd).resolve()) != record.cwd or isolation != record.slot_isolation:
            raise AgentDeliveryError("recorded slot mapping or isolation changed")
        if isolation == "root":
            executable = manager.client._harness_executable(record.harness)
            relay_command, relay_cwd, relay_isolation = _slot_shell_command(record.slot, isolation=isolation,
                project=record.slot_project, explicit_project=True, command=(executable, *arguments))
            if str(Path(relay_cwd).resolve()) != record.cwd or relay_isolation != isolation:
                raise AgentDeliveryError("recorded slot relay policy changed")
            slot_command = None
    elif record.adapter == "herdr-relay":
        raise AgentDeliveryError("relay record lacks the slot boxing policy")
    project_workspace = manager._project_workspace()
    workspace = record.workspace_id
    if workspace is not None:
        try:
            label = manager.client.workspace_label(workspace)
            if project_workspace is not None and label != project_workspace:
                workspace = None
        except HerdrUnavailable as exc:
            if _missing_target(exc) != "workspace-missing":
                raise
            workspace = None
    return _LaunchPolicy(arguments, environment, slot_command, relay_command, workspace)


def _old_process(record: AgentRecord) -> CustomProcessIdentity:
    identity = record.harness_anchor if record.adapter == "herdr" else record.custom_process_identity
    if identity is None:
        raise AgentDeliveryError("record lacks an independently pinned harness process; inspect it before replacing its generation")
    return identity


def _validate_census(panes: tuple[Pane, ...]) -> None:
    if (len({pane.pane_id for pane in panes}) != len(panes)
            or any(not value or "\0" in value for pane in panes for value in (pane.pane_id, pane.tab_id, pane.workspace_id))):
        raise AgentDeliveryError("revive pane census contains duplicate identities")


def _census(manager: ManagedAgents) -> tuple[Pane, ...]:
    panes = manager.client.panes()
    _validate_census(panes)
    return panes


def _proof(manager: ManagedAgents, record: AgentRecord) -> dict[str, object]:
    if manager.client.process_liveness(_old_process(record)) != "dead":
        raise AgentDeliveryError("recorded harness is still alive or its process death cannot be proved")
    if not record.pane_id or not record.tab_id or not record.workspace_id:
        raise AgentDeliveryError("record lacks its owned pane, tab, or workspace")
    panes = _census(manager)
    matches = [pane for pane in panes if pane.pane_id == record.pane_id]
    result: dict[str, object] = {
        "kind": "missing", "pane_id": record.pane_id, "tab_id": record.tab_id,
        "workspace_id": record.workspace_id, "cwd": record.cwd, "terminal_id": None,
        "reported_agent": None, "reported_session_agent": None, "reported_session_value": None,
        "shell": None, "shell_executable": None,
    }
    if not matches:
        if any(pane.tab_id == record.tab_id for pane in panes):
            raise AgentDeliveryError("recorded tab contains a different pane")
        try:
            manager.client.pane_info(record.pane_id)
        except HerdrUnavailable as exc:
            if _missing_target(exc) not in ("pane-missing", "workspace-missing", "tab-missing"):
                raise
        else:
            raise AgentDeliveryError("missing pane census conflicts with the pane probe")
        if any(pane.pane_id == record.pane_id or pane.tab_id == record.tab_id for pane in _census(manager)):
            raise AgentDeliveryError("recorded presentation changed during the missing-pane proof")
        return result
    proof = manager._dead_pane_proof(record, operation="revive", allow_reported_dead=True)
    _census(manager)
    if manager.client.process_liveness(_old_process(record)) != "dead":
        raise AgentDeliveryError("harness process death changed during revive proof")
    owner = manager._claim_owner(record.pane_id, proof.info.terminal_id, exclude=record.name)
    if owner is not None:
        raise AgentDeliveryError(f"revive pane is claimed by {owner!r}")
    result.update({
        "kind": "idle_shell", "terminal_id": proof.info.terminal_id,
        "reported_agent": proof.info.agent, "reported_session_agent": proof.info.session_agent,
        "reported_session_value": proof.info.session_value, "shell": asdict(proof.shell.identity),
        "shell_executable": proof.shell.executable_path,
    })
    return result


def _name_available(manager: ManagedAgents, record: AgentRecord) -> None:
    try:
        pane = manager.client.agent_pane(record.name)
    except HerdrUnavailable as exc:
        if herdr_error_code(str(exc)) not in ("agent_not_found", "pane_not_found", "workspace_not_found"):
            raise
    else:
        if pane != record.pane_id:
            raise AgentDeliveryError("managed name points to a different live presentation")
    owner = manager._identity_owner(record.harness, _conversation(record), exclude=record.name)
    if owner is not None:
        raise AgentDeliveryError(f"native conversation is already registered as {owner.name!r}")


def _plan(record: AgentRecord, action: str, reason: str | None) -> dict[str, object]:
    return {"name": record.name, "token": record.token, "harness": record.harness, "profile": record.profile,
        "model": record.model, "cwd": record.cwd, "slot": record.slot, "native_session": record.native_session,
        "action": action, "reason": reason}


def plan(manager: ManagedAgents, name: str, *, expected_token: str | None = None) -> dict[str, object]:
    """Inspect one generation's recovery eligibility without changing state."""
    _name(name)
    pending = [journal for journal in journals(manager) if journal["name"] == name]
    if pending:
        journal = pending[0]
        _operation, _candidate, archived = _paths(manager, journal)
        path = manager._directory(name)
        if os.path.lexists(archived):
            path = archived
        record, old_content, _held = _read_record(manager, path, name, identity=_identity(journal, "old"))
        if record.token != journal["old_token"]:
            raise AgentDeliveryError("old revive generation changed")
        if expected_token not in (None, journal["old_token"]):
            raise AgentDeliveryError("revive token changed before this operation")
        try:
            if _digest(old_content) not in (journal["old_record_sha256"], journal["stopped_record_sha256"]):
                raise AgentDeliveryError("old revive record bytes changed")
            if path == archived and (record.lifecycle != "stopped"
                                     or _digest(old_content) != journal["stopped_record_sha256"]):
                raise AgentDeliveryError("archived revive generation lacks the prepared stopped record")
            original = copy.copy(record)
            original.lifecycle = "running"
            _prepared_stopped(manager, journal, original)
            published = not os.path.lexists(_candidate)
            candidate, ready_content, _held = _read_record(manager, manager._directory(name) if published else _candidate,
                name, identity=_identity(journal, "new"))
            if candidate.token != journal["new_token"]:
                raise AgentDeliveryError("revive candidate token changed")
            if journal["ready_record_sha256"] is not None and _digest(ready_content) != journal["ready_record_sha256"]:
                raise AgentDeliveryError("ready revive record bytes changed")
            if published:
                if journal["ready_record_sha256"] is None:
                    raise AgentDeliveryError("published candidate lacks durable ready evidence")
            else:
                prospective = copy.copy(candidate)
                if prospective.lifecycle in ("starting", "launch_failed") and journal["phase"] == "launching":
                    prospective.lifecycle, prospective.error = "running", None
                try:
                    _ready(manager, prospective, original, cast(dict[str, object], journal["proof"]))
                except HerdrRunError as exc:
                    raise AgentDeliveryError(
                        f"retained revive candidate cannot be verified: {exc}; inspect {_candidate / 'agent.json'} "
                        "and its recorded terminal; no automatic relaunch"
                    ) from exc
            current = _proof(manager, original)
            if current != journal["proof"] and not (published and current["kind"] == "missing"):
                raise AgentDeliveryError("stale revive presentation changed")
            result = _plan(record, "recover", "incomplete revive transaction; rerun the same command")
        except (HerdrRunError, OSError, ValueError) as exc:
            result = _plan(record, "blocked", str(exc))
        result["transaction_phase"] = journal["phase"]
        return result
    record, content, identity = _read_record(manager, manager._directory(name), name)
    if expected_token is not None and record.token != expected_token:
        raise AgentDeliveryError("agent was replaced before revive")
    try:
        if record.lifecycle != "running":
            return _plan(record, "skip", "record lifecycle is not running")
        if manager.client.process_liveness(_old_process(record)) == "alive":
            return _plan(record, "skip", "recorded harness is still alive")
        policy = _policy(manager, record)
        _name_available(manager, record)
        _proof(manager, record)
        destination = manager.registry / "archive" / f"{record.name}-{record.token}"
        if os.path.lexists(destination):
            raise AgentDeliveryError("revive archive destination already exists")
        _read_record(manager, manager._directory(name), name, identity=identity, digest=_digest(content))
        result = _plan(record, "revive", None)
        result["workspace_id"] = policy.workspace_id
        return result
    except (HerdrRunError, OSError, ValueError) as exc:
        return _plan(record, "blocked", str(exc))


def _write_journal(manager: ManagedAgents, journal: dict[str, object]) -> None:
    directory = manager.registry / ".revives"
    with manager._pinned_parent_directory(directory, label="revive journal directory") as parent:
        pinned = _PinnedAgentDirectory(cast(str, journal["name"]), directory, parent.descriptor, parent.device, parent.inode)
        manager._atomic_snapshot_bytes(pinned, agent._json_text(journal).encode("utf-8"), name=f"{journal['old_token']}.json")
        manager._verify_pinned_parent_directory(parent, label="revive journal directory")


def _new_candidate(old: AgentRecord, token: str, policy: _LaunchPolicy) -> AgentRecord:
    candidate = replace(old, token=token, created_at=time.time(), lifecycle="starting",
        resume=_conversation(old), arguments=list(policy.arguments), native_session=_native_session(old.harness, _conversation(old), "asserted"),
        workspace_id=None, tab_id=None, pane_id=None, session_agent=None, session_value=None,
        terminal_id=None, harness_identity=None, anchor_rule=None, custom_process_identity=None, foreign_shell_identity=None,
        pane_reported_by_agentctl=False, startup_warning=None, effective_reasoning_effort=None, error=None, runtime_home=None,
        goal_delivery=None, goal_message_id=None, goal_messages={}, goal_session_id=None, goal_command=None)
    candidate._unknown = copy.deepcopy(old._unknown)
    candidate._unknown["revived_from"] = old.token
    candidate._nested_storage = None
    candidate._session_source = None
    return candidate


def _same_policy(candidate: AgentRecord, old: AgentRecord) -> bool:
    return not (candidate.name != old.name
            or candidate.token == old.token or candidate.harness != old.harness
            or candidate.cwd != old.cwd or candidate.adapter != old.adapter
            or candidate.mode != old.mode or candidate.backend != old.backend
            or candidate.model != old.model or candidate.profile != old.profile
            or candidate.reasoning_effort != old.reasoning_effort or candidate.environment_names != old.environment_names
            or candidate.slot != old.slot or candidate.slot_project != old.slot_project
            or candidate.slot_isolation != old.slot_isolation or candidate.paused != old.paused
            or candidate.resume != _conversation(old) or _conversation(candidate) != _conversation(old)
            or candidate.arguments != list(harness_arguments(old.harness, model=old.model, resume=_conversation(old),
                extra=(*reasoning_arguments(old.harness, old.reasoning_effort), *_extras(old)))))


class _SessionCensusClient:
    """Exclude one proven dead presentation only during revive verification."""

    def __init__(self, manager: ManagedAgents, old: AgentRecord, proof: dict[str, object]) -> None:
        self.manager, self.old, self.proof = manager, old, proof

    def __getattr__(self, name: str) -> object:
        return cast(object, getattr(self.manager.client, name))

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        """Keep the dead generation proof around each candidate identity probe."""
        if pane_id == self.proof["pane_id"]:
            raise AgentDeliveryError("revive verification cannot target the retired presentation")
        if _proof(self.manager, self.old) != self.proof:
            raise AgentDeliveryError("dead presentation changed before revive identity probe")
        info = self.manager.client.pane_info(pane_id)
        if _proof(self.manager, self.old) != self.proof:
            raise AgentDeliveryError("dead presentation changed during revive identity probe")
        return info

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        """Return a census excluding only the journal-bound dead presentation."""
        if _proof(self.manager, self.old) != self.proof:
            raise AgentDeliveryError("dead presentation changed before revive session census")
        panes = self.manager.client.panes() if workspace_id is None else self.manager.client.panes(workspace_id)
        if _proof(self.manager, self.old) != self.proof:
            raise AgentDeliveryError("dead presentation changed during revive session census")
        _validate_census(panes)
        old = [pane for pane in panes if pane.pane_id == self.proof["pane_id"]]
        if (not old and workspace_id in (None, self.proof["workspace_id"])
                or old and (old[0].tab_id != self.proof["tab_id"]
                            or old[0].workspace_id != self.proof["workspace_id"])):
            raise AgentDeliveryError("excluded dead presentation changed in revive session census")
        return tuple(pane for pane in panes if pane.pane_id != self.proof["pane_id"])


def _candidate_manager(manager: ManagedAgents, old: AgentRecord, proof: dict[str, object]) -> ManagedAgents:
    if proof["reported_session_value"] is None:
        return manager
    candidate_manager = copy.copy(manager)
    candidate_manager.client = cast(HerdrClient, _SessionCensusClient(manager, old, proof))
    return candidate_manager


def _ready(manager: ManagedAgents, candidate: AgentRecord, old: AgentRecord, proof: dict[str, object]) -> None:
    if (candidate.lifecycle != "running" or not _same_policy(candidate, old)
            or candidate.terminal_id is None or (candidate.harness_anchor is None if candidate.adapter == "herdr"
                                               else candidate.custom_process_identity is None)):
        raise AgentDeliveryError("revive candidate lacks the recorded policy or independent runtime anchors")
    verifier = _candidate_manager(manager, old, proof)
    agent.resolve_target(verifier.client, manager._target(candidate))
    verifier._checked(candidate)


def _prepared_stopped(manager: ManagedAgents, journal: dict[str, object], old: AgentRecord) -> bytes:
    operation, _candidate, _archive = _paths(manager, journal)
    with manager._pinned_parent_directory(operation, label="revive operation") as parent:
        pinned = _PinnedAgentDirectory(old.name, operation, parent.descriptor, parent.device, parent.inode)
        content = manager._record_bytes(pinned, file_name="stopped.json")
    value: object = json.loads(content)
    stopped = AgentRecord._from_value(value, operation / "stopped.json", old.name)
    expected = old.to_document()
    expected["lifecycle"] = "stopped"
    if stopped.to_document() != expected:
        raise AgentDeliveryError("prepared stopped record differs from the old generation")
    if len(content) > _MAX_AGENT_RECORD_BYTES or _digest(content) != journal["stopped_record_sha256"]:
        raise AgentDeliveryError("prepared stopped bytes changed")
    return content


def _identity(journal: dict[str, object], prefix: str) -> tuple[int, int]:
    return cast(int, journal[f"{prefix}_directory_device"]), cast(int, journal[f"{prefix}_directory_inode"])


def _complete(manager: ManagedAgents, journal: dict[str, object]) -> dict[str, object]:
    _operation, _stage, archive = _paths(manager, journal)
    name = cast(str, journal["name"])
    queue_lock = nullcontext() if os.path.lexists(archive) else manager._queue_locks(name)
    try:
        with queue_lock, manager._pane_lock(cast(str, cast(dict[str, object], journal["proof"])["pane_id"])):
            return _complete_locked(manager, journal)
    except (HerdrRunError, OSError, ValueError) as exc:
        raise AgentDeliveryError(f"{exc}; revive state retained; rerun `agentctl revive {name}`") from exc


def _complete_locked(manager: ManagedAgents, journal: dict[str, object]) -> dict[str, object]:
    name = cast(str, journal["name"])
    operation, stage, archive = _paths(manager, journal)
    active = manager._directory(name)
    old_path = archive if os.path.lexists(archive) else active
    old, old_bytes, old_identity = _read_record(manager, old_path, name, identity=_identity(journal, "old"))
    if old.token != journal["old_token"] or _digest(old_bytes) not in (journal["old_record_sha256"], journal["stopped_record_sha256"]):
        raise AgentDeliveryError("old revive generation changed")
    if old.lifecycle not in ("running", "stopped"):
        raise AgentDeliveryError("old revive lifecycle changed")
    if old_path == archive and (old.lifecycle != "stopped" or _digest(old_bytes) != journal["stopped_record_sha256"]):
        raise AgentDeliveryError("archived revive generation lacks the prepared stopped record")
    original = copy.copy(old)
    original.lifecycle = "running"
    stopped_bytes = _prepared_stopped(manager, journal, original)
    published = not os.path.lexists(stage)
    candidate_path = active if published else stage
    candidate, ready_bytes, new_identity = _read_record(manager, candidate_path, name, identity=_identity(journal, "new"))
    if candidate.token != journal["new_token"]:
        raise AgentDeliveryError("revive candidate token changed")
    if not published:
        if candidate.lifecycle in ("starting", "launch_failed") and journal["phase"] == "launching":
            recovered = copy.copy(candidate)
            recovered.lifecycle, recovered.error = "running", None
            try:
                _ready(manager, recovered, original, cast(dict[str, object], journal["proof"]))
            except HerdrRunError as exc:
                raise AgentDeliveryError(
                    f"revive launch is incomplete; inspect {stage / 'agent.json'} and its recorded terminal. "
                    f"Use `agentctl revive {name} --dry-run`, then rerun `agentctl revive {name}` "
                    "after the saved process and terminal anchors can be verified; no second harness was launched"
                ) from exc
            recovered._storage = stage, new_identity[0], new_identity[1]
            manager._save(recovered)
            candidate, ready_bytes, new_identity = _read_record(manager, stage, name, identity=new_identity)
        _ready(manager, candidate, original, cast(dict[str, object], journal["proof"]))
    if journal["ready_record_sha256"] is not None and _digest(ready_bytes) != journal["ready_record_sha256"]:
        raise AgentDeliveryError("ready revive record bytes changed")
    if published and journal["ready_record_sha256"] is None:
        raise AgentDeliveryError("published candidate lacks durable ready evidence")
    if not published and journal["phase"] == "launching":
        journal["ready_record_sha256"] = _digest(ready_bytes)
        journal["phase"] = "ready"
        _write_journal(manager, journal)
    # Name and identity reservations cover both directory moves. Existing queue locks
    # are held only while the old active directory still exists, never via reused NAME.
    if old_path == active:
        with manager._pinned_agent_directory(name) as pinned:
            if _proof(manager, original) != journal["proof"]:
                raise AgentDeliveryError("stale revive presentation changed before retirement")
            current = manager._managed_record_snapshot(pinned, expected_token=original.token)
            if (current.directory_device, current.directory_inode) != old_identity or current.content != old_bytes:
                raise AgentDeliveryError("old revive generation changed before retirement")
            if not published:
                _read_record(manager, stage, name, identity=new_identity, digest=_digest(ready_bytes))
                _ready(manager, candidate, original, cast(dict[str, object], journal["proof"]))
            _archive_parent, destination = manager._archive_destination(original)
            if journal["proof"] != _proof(manager, original):
                raise AgentDeliveryError("stale revive presentation changed before output capture")
            proof = cast(dict[str, object], journal["proof"])
            if proof["kind"] == "idle_shell":
                text = manager._bounded_terminal_text(cast(str, proof["pane_id"]), operation="revive")
                if journal["proof"] != _proof(manager, original):
                    raise AgentDeliveryError("stale revive presentation changed during output capture")
                manager._atomic_snapshot_bytes(pinned, agent._json_text({
                    "text": text, "captured_at": time.time(), "pane_id": proof["pane_id"],
                    "retirement_shell_identity": {**cast(dict[str, object], proof["shell"]), "executable_path": proof["shell_executable"]},
                }).encode("utf-8"))
            manager._atomic_snapshot_bytes(pinned, stopped_bytes, name="agent.json")
            if journal["proof"] != _proof(manager, original):
                raise AgentDeliveryError("stale revive presentation changed before archival")
            manager._publish_pinned_directory(pinned, destination, expected_record=stopped_bytes)
    if not published:
        _read_record(manager, stage, name, identity=new_identity, digest=_digest(ready_bytes))
        _ready(manager, candidate, original, cast(dict[str, object], journal["proof"]))
        with manager._pinned_parent_directory(operation, label="revive operation") as parent, manager._pinned_parent_directory(manager.registry, label="agent registry") as registry:
            staged = os.stat("new", dir_fd=parent.descriptor, follow_symlinks=False)
            if (staged.st_dev, staged.st_ino) != new_identity:
                raise AgentDeliveryError("revive candidate directory changed before publication")
            manager._verify_pinned_parent_directory(parent, label="revive operation")
            manager._verify_pinned_parent_directory(registry, label="agent registry")
            _rename_directory_noreplace_at(parent.descriptor, "new", registry.descriptor, name)
            failures: list[str] = []
            for descriptor in (parent.descriptor, registry.descriptor):
                try:
                    os.fsync(descriptor)
                except OSError as exc:
                    failures.append(str(exc))
            if failures:
                raise AgentDeliveryError(f"revive directory publication durability failed: {'; '.join(failures)}")
            manager._verify_pinned_parent_directory(parent, label="revive operation")
            manager._verify_pinned_parent_directory(registry, label="agent registry")
        _read_record(manager, active, name, identity=new_identity, digest=_digest(ready_bytes))
    journal["phase"] = "published"
    _write_journal(manager, journal)
    # Publishing is final even if the replacement later exits. Finish only cleanup.
    current_proof = _proof(manager, original)
    pane_closed = False
    if current_proof["kind"] != "missing":
        if current_proof != journal["proof"]:
            raise AgentDeliveryError("stale revive shell or tab ownership changed; replacement and journal retained")
        _read_record(manager, active, name, identity=new_identity, digest=_digest(ready_bytes))
        if _proof(manager, original) != journal["proof"]:
            raise AgentDeliveryError("stale revive shell changed immediately before close")
        try:
            manager.client.close_pane(cast(str, current_proof["pane_id"]))
        except HerdrRunError:
            if _proof(manager, original)["kind"] != "missing":
                raise
        pane_closed = True
        if _proof(manager, original)["kind"] != "missing":
            raise AgentDeliveryError("stale revive pane closure could not be confirmed")
    _read_record(manager, active, name, identity=new_identity, digest=_digest(ready_bytes))
    _read_record(manager, archive, name, identity=old_identity, digest=cast(str, journal["stopped_record_sha256"]))
    directory = manager.registry / ".revives"
    with manager._pinned_parent_directory(directory, label="revive journal directory") as parent:
        pinned = _PinnedAgentDirectory(name, directory, parent.descriptor, parent.device, parent.inode)
        journal_name = f"{journal['old_token']}.json"
        if json.loads(manager._record_bytes(pinned, file_name=journal_name)) != journal:
            raise AgentDeliveryError("revive journal changed before cleanup")
        os.unlink(journal_name, dir_fd=parent.descriptor)
        os.fsync(parent.descriptor)
        manager._verify_pinned_parent_directory(parent, label="revive journal directory")
    candidate._storage = None
    result = manager._status_record(candidate)
    result.update({"revived": True, "previous_token": original.token, "archive": str(archive),
                   "pane_closed": pane_closed, "tab_closed": True})
    return result


def _preserve_unlaunched_setup(
    manager: ManagedAgents, old: AgentRecord, operation: Path, content: bytes,
    identity: tuple[int, int], proof: dict[str, object],
) -> None:
    """Quarantine pre-journal setup; launching always requires a durable journal."""
    if not os.path.lexists(operation):
        return
    if any(journal["old_token"] == old.token for journal in journals(manager)):
        raise AgentDeliveryError("revive setup acquired a journal before recovery")
    parent_path = operation.parent
    with manager._pinned_parent_directory(parent_path, label="revive journal directory") as parent, manager._pinned_parent_directory(operation, label="orphaned revive setup") as held:
        entries = set(os.listdir(held.descriptor))
        for entry in entries:
            if entry in ("new", "stopped.json"):
                continue
            if re.fullmatch(r"\.stopped\.json-recovery-[0-9]+-(?:[0-9a-f]{32}|[0-9]+-[0-9]+)", entry) is None:
                raise AgentDeliveryError("orphaned revive setup has unexpected contents; inspect it before recovery")
            metadata = os.stat(entry, dir_fd=held.descriptor, follow_symlinks=False)
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                    or metadata.st_mode & 0o077 or metadata.st_nlink != 1 or metadata.st_size > _MAX_AGENT_RECORD_BYTES):
                raise AgentDeliveryError("orphaned revive staging artifact is unsafe")
        if "stopped.json" in entries:
            pinned = _PinnedAgentDirectory(old.name, operation, held.descriptor, held.device, held.inode)
            stopped = AgentRecord._from_value(json.loads(manager._record_bytes(pinned, file_name="stopped.json")), operation / "stopped.json", old.name)
            expected = old.to_document()
            expected["lifecycle"] = "stopped"
            if stopped.to_document() != expected:
                raise AgentDeliveryError("orphaned revive setup belongs to a different old generation")
        if "new" in entries:
            stage = operation / "new"
            with manager._pinned_parent_directory(stage, label="orphaned revive candidate") as staged:
                candidate_entries = set(os.listdir(staged.descriptor))
                for entry in candidate_entries - {"agent.json"}:
                    metadata = os.stat(entry, dir_fd=staged.descriptor, follow_symlinks=False)
                    if (re.fullmatch(r"\.agent\.json-recovery-[0-9]+-(?:[0-9a-f]{32}|[0-9]+-[0-9]+)", entry) is None
                            or not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                            or metadata.st_mode & 0o077 or metadata.st_nlink != 1 or metadata.st_size > _MAX_AGENT_RECORD_BYTES):
                        raise AgentDeliveryError("orphaned revive candidate has unexpected contents")
                if "agent.json" in candidate_entries:
                    candidate, _bytes, _held = _read_record(manager, stage, old.name, identity=(staged.device, staged.inode))
                    if (candidate.lifecycle != "starting" or not _same_policy(candidate, old)
                            or candidate._unknown.get("revived_from") != old.token
                            or candidate.pane_reported_by_agentctl
                            or any(value is not None for value in (
                                candidate.workspace_id, candidate.tab_id, candidate.pane_id, candidate.terminal_id,
                                candidate.harness_identity, candidate.anchor_rule, candidate.custom_process_identity,
                                candidate.foreign_shell_identity, candidate.session_agent, candidate.session_value, candidate.runtime_home))):
                        raise AgentDeliveryError("orphaned revive candidate may have been launched; no automatic relaunch")
                manager._verify_pinned_parent_directory(staged, label="orphaned revive candidate")
        _read_record(manager, manager._directory(old.name), old.name, identity=identity, digest=_digest(content))
        if _proof(manager, old) != proof or set(os.listdir(held.descriptor)) != entries:
            raise AgentDeliveryError("old generation or orphaned setup changed before recovery")
        manager._verify_pinned_parent_directory(held, label="orphaned revive setup")
        manager._verify_pinned_parent_directory(parent, label="revive journal directory")
        destination = f".orphan-{old.token}-{uuid.uuid4().hex}"
        _rename_directory_noreplace_at(parent.descriptor, old.token, parent.descriptor, destination)
        os.fsync(parent.descriptor)
        saved = os.stat(destination, dir_fd=parent.descriptor, follow_symlinks=False)
        if (saved.st_dev, saved.st_ino) != (held.device, held.inode) or os.path.lexists(operation):
            raise AgentDeliveryError("orphaned revive setup publication changed generation")
        manager._verify_pinned_parent_directory(parent, label="revive journal directory")
        _read_record(manager, manager._directory(old.name), old.name, identity=identity, digest=_digest(content))
        if _proof(manager, old) != proof:
            raise AgentDeliveryError("old presentation changed while preserving revive setup")


def revive(manager: ManagedAgents, name: str, *, dry_run: bool = False, expected_token: str | None = None,
           startup_timeout: float = 30.0) -> dict[str, object]:
    """Resume one verified dead generation, or inspect its immutable recovery plan."""
    _name(name)
    if not math.isfinite(startup_timeout) or not 0 < startup_timeout <= 300:
        raise AgentDeliveryError("startup timeout must be between 0 and 300 seconds")
    if expected_token is not None and not _TOKEN.fullmatch(expected_token):
        raise AgentDeliveryError("invalid expected revive token")
    if dry_run:
        return plan(manager, name, expected_token=expected_token)
    with manager._lock(name), manager._identity_transaction():
        pending = [journal for journal in journals(manager) if journal["name"] == name]
        if pending:
            if expected_token not in (None, pending[0]["old_token"]):
                raise AgentDeliveryError("revive token changed before this operation")
            return _complete(manager, pending[0])
        manager._refuse_pending_rename(name)
        old, content, identity = _read_record(manager, manager._directory(name), name)
        if expected_token is not None and old.token != expected_token:
            raise AgentDeliveryError("agent was replaced before revive")
        policy = _policy(manager, old)
        _name_available(manager, old)
        proof = _proof(manager, old)
        if os.path.lexists(manager.registry / "archive" / f"{name}-{old.token}"):
            raise AgentDeliveryError("revive archive destination already exists")
        parent = manager.registry / ".revives"
        if not os.path.lexists(parent):
            parent.mkdir(mode=0o700)
            agent._fsync_dir(str(manager.registry))
        agent._validate_private_directory(str(parent), "revive journal directory")
        operation = parent / old.token
        _preserve_unlaunched_setup(manager, old, operation, content, identity, proof)
        operation.mkdir(mode=0o700)
        stage = operation / "new"
        stage.mkdir(mode=0o700)
        agent._fsync_dir(str(parent))
        agent._fsync_dir(str(operation))
        metadata = stage.stat(follow_symlinks=False)
        candidate = _new_candidate(old, uuid.uuid4().hex, policy)
        candidate._storage = stage, metadata.st_dev, metadata.st_ino
        manager._save(candidate)
        stopped_document = cast(dict[str, object], json.loads(content))
        stopped_document["lifecycle"] = "stopped"
        stopped = agent._json_text(stopped_document).encode("utf-8")
        if len(stopped) > _MAX_AGENT_RECORD_BYTES:
            raise AgentDeliveryError("prepared stopped record exceeds its size bound")
        with manager._pinned_parent_directory(operation, label="revive operation") as pinned_parent:
            pinned = _PinnedAgentDirectory(name, operation, pinned_parent.descriptor, pinned_parent.device, pinned_parent.inode)
            manager._atomic_snapshot_bytes(pinned, stopped, name="stopped.json")
            manager._verify_pinned_parent_directory(pinned_parent, label="revive operation")
        journal: dict[str, object] = {
            "schema": "agentctl-revive/v1", "name": name, "old_token": old.token, "new_token": candidate.token,
            "phase": "launching", "started_at": time.time(), "old_directory_device": identity[0], "old_directory_inode": identity[1],
            "new_directory_device": metadata.st_dev, "new_directory_inode": metadata.st_ino,
            "old_record_sha256": _digest(content), "stopped_record_sha256": _digest(stopped), "ready_record_sha256": None, "proof": proof,
        }
        _write_journal(manager, journal)
        with manager._pane_lock(cast(str, proof["pane_id"])):
            _read_record(manager, manager._directory(name), name, identity=identity, digest=_digest(content))
            if _proof(manager, old) != proof:
                raise AgentDeliveryError("old revive presentation changed before launch")
            try:
                _candidate_manager(manager, old, proof)._launch_interactive(candidate, policy.workspace_id, policy.environment, manager._project_workspace(),
                    startup_timeout, policy.slot_command, policy.relay_command)
                _ready(manager, candidate, old, proof)
            except (HerdrRunError, OSError, ValueError) as exc:
                candidate.lifecycle = "launch_failed"
                candidate.error = "revive launch failed; inspect its retained terminal" if policy.environment else str(exc)
                manager._save(candidate)
                raise AgentDeliveryError(f"revive launch failed; inspect {stage / 'agent.json'} and its terminal; rerun `agentctl revive {name}`") from exc
        return _complete(manager, journal)


def revive_all(manager: ManagedAgents, *, dry_run: bool = False, startup_timeout: float = 30.0) -> dict[str, object]:
    """Plan or recover all names, retaining individual refusals in the result."""
    if not math.isfinite(startup_timeout) or not 0 < startup_timeout <= 300:
        raise AgentDeliveryError("startup timeout must be between 0 and 300 seconds")
    names = {cast(str, journal["name"]) for journal in journals(manager)}
    if manager.registry.exists():
        agent._validate_private_directory(str(manager.registry), "agent registry")
        names.update(path.name for path in manager.registry.iterdir() if _NAME.fullmatch(path.name) and path.name != "archive")
    rows: list[dict[str, object]] = []
    revived = blocked = 0
    for name in sorted(names):
        try:
            result = plan(manager, name)
            if not dry_run and result["action"] in ("revive", "recover"):
                result = revive(manager, name, expected_token=cast(str, result["token"]), startup_timeout=startup_timeout)
                revived += 1
            elif result["action"] == "blocked":
                blocked += 1
            rows.append(result)
        except (HerdrRunError, ValueError, OSError) as exc:
            blocked += 1
            rows.append({"name": name, "action": "blocked", "reason": str(exc)})
    return {"dry_run": dry_run, "agents": rows, "revived": revived, "blocked": blocked}
