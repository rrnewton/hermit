"""One registry and lifecycle authority across native terminals and turn runners."""
from __future__ import annotations

import json
import math
import os
import re
import subprocess
import sys
import time
import uuid
from collections.abc import Sequence
from dataclasses import asdict
from pathlib import Path

from agentctl import agent
from agentctl.client import HerdrClient, _bounded_control_command
from agentctl.errors import AgentDeliveryError, HerdrRunError
from agentctl.jsonx import as_mapping
from agentctl.profiles import (
    reasoning_arguments,
    validate_muse_headless_arguments,
    validate_structured_harness_argument_conflicts,
)
from agentctl.subagents import AgentRecord, ManagedAgents, _name, _native_session


class Sessions(ManagedAgents):
    """Route all public operations through one generation-locked session record."""

    def __init__(self, client: HerdrClient | None = None, registry: str | Path = ".agentctl") -> None:
        super().__init__(client or HerdrClient(), registry)

    def _worker(self, record: AgentRecord, action: str, **options: object) -> dict[str, object]:
        if record.runtime_home is None:
            raise AgentDeliveryError("turn-runner session has no runtime directory")
        environment = dict(os.environ)
        environment["HERDR_SUBAGENTS_HOME"] = record.runtime_home
        command = [sys.executable, str(Path(__file__).with_name("worker_rpc.py"))]
        # The adapter process may be interrupted after committing a request. Keep
        # its canonical record and report uncertainty rather than retrying it.
        try:
            completed = subprocess.run(command,
                input=json.dumps({"action": action, "name": record.name, **options}),
                text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env=environment, timeout=900 if action == "migrate" else 90)
        except subprocess.TimeoutExpired as exc:
            raise AgentDeliveryError(f"runtime {action} timed out; inspect session state before retrying") from exc
        try:
            result = as_mapping(json.loads(completed.stdout), "runtime response")
        except (ValueError, TypeError) as exc:
            raise AgentDeliveryError(f"runtime {action} returned no valid receipt: {completed.stderr.strip()}") from exc
        if completed.returncode:
            raise AgentDeliveryError(str(result.get("error", "runtime operation failed")))
        return result

    @staticmethod
    def _capabilities(record: AgentRecord) -> list[str]:
        result = ["send", "status", "read", "wait", "stop", "attach", "pause", "resume"]
        if record.mode == "headless":
            result.extend(("final-answer", "reset", "migrate", "repair"))
        else:
            result.append("terminal-snapshot")
        if record.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            # Goals are slash commands, which a relayed pane cannot take.
            result.extend(("drain", "bind-session") if record.adapter == "herdr-relay"
                          else ("drain", "goal", "bind-session"))
        if record.adapter == "herdr":
            result.append("move")
        if record.adapter in ("herdr", "herdr-foreign"):
            result.extend(("anchor", "rename"))
        if record.mode == "interactive" and record.adapter in ("herdr", "herdr-pane", "herdr-relay"):
            result.append("revive")
        return result

    def start_session(self, name: str, *, cwd: str, mode: str = "interactive",
                      backend: str = "herdr", harness: str = "codex", model: str | None = None,
                      reasoning_effort: str | None = None,
                      resume: str | None = None, harness_args: Sequence[str] = (),
                      environment: Sequence[str] = (), brief: str | None = None,
                      workspace_id: str | None = None, startup_timeout: float = 30.0,
                      ready_timeout: float = 900.0, working_timeout: float = 30.0,
                      max_attempts: int = 3, slot: str | None = None,
                      slot_isolation: str | None = None,
                      slot_project: str | None = None,
                      profile: str | None = None) -> dict[str, object]:
        """Create a native interactive terminal or a persistent headless runner."""
        _name(name)
        if profile is not None and re.fullmatch(r"[a-z][a-z0-9-]{0,31}", profile) is None:
            raise AgentDeliveryError("invalid launch profile name")
        if mode == "interactive":
            if backend != "herdr":
                raise AgentDeliveryError("interactive sessions require Herdr; tmux supports headless runners")
            return super().start(
                name, cwd=cwd, harness=harness, model=model,
                reasoning_effort=reasoning_effort, resume=resume,
                harness_args=harness_args, environment=environment, brief=brief,
                workspace_id=workspace_id, startup_timeout=startup_timeout,
                ready_timeout=ready_timeout, working_timeout=working_timeout,
                max_attempts=max_attempts, slot=slot, slot_isolation=slot_isolation,
                slot_project=slot_project,
                profile=profile,
            )
        if slot is not None:
            raise AgentDeliveryError("--slot is supported for interactive Herdr sessions only")
        if mode != "headless" or backend not in ("herdr", "tmux") or harness not in ("codex", "agy", "muse"):
            raise AgentDeliveryError("headless sessions support codex/agy/muse with herdr/tmux")
        if resume is not None or workspace_id is not None or environment:
            raise AgentDeliveryError(
                "headless start does not accept resume, workspace-id, or environment"
            )
        if isinstance(harness_args, (str, bytes)) or any(
            not isinstance(item, str) or not item or "\0" in item
            for item in harness_args
        ):
            raise AgentDeliveryError("headless harness arguments must be nonempty and contain no NUL")
        if harness != "muse" and harness_args:
            raise AgentDeliveryError("headless harness arguments currently apply only to Muse")
        if harness != "muse" and reasoning_effort is not None:
            raise AgentDeliveryError(
                f"headless {harness} does not support reasoning_effort"
            )
        if harness == "muse":
            validate_structured_harness_argument_conflicts(
                harness, list(harness_args), label="Muse headless launch",
                structured_model=model is not None,
                structured_effort=reasoning_effort is not None,
            )
        arguments = (*reasoning_arguments(harness, reasoning_effort), *harness_args)
        if harness == "muse":
            validate_muse_headless_arguments(list(arguments))
        root = str(Path(cwd).expanduser().resolve())
        if not Path(root).is_dir():
            raise AgentDeliveryError(f"cwd is not a directory: {root}")
        with self._lock(name):
            with self._identity_transaction():
                directory = self._directory(name)
                if os.path.lexists(directory):
                    raise AgentDeliveryError(f"agent {name!r} already registered; stop it before reusing the name")
                directory.mkdir(mode=0o700)
                record = AgentRecord(name, uuid.uuid4().hex, harness, root, time.time(), model=model,
                    profile=profile, reasoning_effort=reasoning_effort,
                    adapter="turn-runner", mode=mode, backend=backend, runtime_home=str(directory / "runtime"))
                self._save(record)
                try:
                    response = self._worker(record, "start", cwd=root, harness=harness,
                                            model=model, backend=backend, brief=brief,
                                            harness_args=list(arguments))
                    self._sync_worker_record(record, response, save=False)
                    if record.session_value is not None:
                        owner = self._identity_owner(
                            record.session_agent or record.harness,
                            record.session_value, exclude=name,
                        )
                        if owner is not None:
                            try:
                                self._worker(record, "stop")
                            except HerdrRunError as stop_error:
                                detail = f"; could not stop the conflicting new runtime: {stop_error}"
                            else:
                                detail = "; stopped the conflicting new runtime"
                            record.session_agent = record.session_value = None
                            raise AgentDeliveryError(
                                f"native session is already registered as {owner.name!r}{detail}"
                            )
                    record.lifecycle = "running"
                    self._save(record)
                except (HerdrRunError, OSError, ValueError) as exc:
                    record.lifecycle, record.error = "launch_failed", str(exc)
                    self._save(record)
                    raise
            return self.status(name)

    def _sync_worker_record(
        self, record: AgentRecord, response: dict[str, object], *, save: bool = True,
    ) -> None:
        value = response.get("record")
        if isinstance(value, dict):
            runtime = as_mapping(value, "runtime record")
            record.mode = "interactive" if runtime.get("mode") == "tui" else "headless"
            record.backend = str(runtime.get("backend", record.backend))
            session = runtime.get("session_id")
            if isinstance(session, str) and (not session or "\0" in session):
                raise AgentDeliveryError("worker reported an invalid native session identity")
            record.session_value = session if isinstance(session, str) else None
            record.native_session = (None if record.session_value is None
                                     else _native_session(record.harness, record.session_value, "observed"))
            pane = runtime.get("presentation_pane")
            record.pane_id = pane if isinstance(pane, str) else None
        if save:
            self._save(record)

    def status(self, name: str) -> dict[str, object]:
        """Distinguish saved lifecycle, runtime liveness, and supported operations."""
        return self._status_record(self._load(name))

    def _status_record(self, record: AgentRecord) -> dict[str, object]:
        if record.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            result = super()._status_record(record)
        else:
            result = record.to_document()
            try:
                response = self._worker(record, "status")
                result["runtime"] = response.get("result")
                runtime_record = response.get("record")
                if isinstance(runtime_record, dict):
                    live_session = runtime_record.get("session_id")
                    if isinstance(live_session, str):
                        result["session_value"] = live_session
                    live_pane = runtime_record.get("presentation_pane")
                    if isinstance(live_pane, str):
                        result["pane_id"] = live_pane
                result["probe_error"] = None
            except (HerdrRunError, OSError, ValueError) as exc:
                result.update(probe_error=str(exc), agent_status="unknown")
        result["capabilities"] = self._capabilities(record)
        return result

    def send_session(self, name: str, text: str, *, message_id: str | None = None,
                     model: str | None = None, **options: object) -> dict[str, object]:
        """Submit to the selected adapter without reusing another generation's name."""
        record = self._load(name)
        if record.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            if model is not None:
                raise AgentDeliveryError("per-turn model overrides require a headless session")
            return asdict(super().send(name, text, message_id=message_id, expected_token=record.token, **options))
        if message_id is not None:
            raise AgentDeliveryError("headless requests use monotonically numbered turns; message-id is interactive-only")
        with self._lock(name):
            current = self._load(name)
            if current.token != record.token:
                raise AgentDeliveryError("session was replaced before submission")
            self._require_automation(current)
            return self._worker(current, "send", text=text, model=model)

    def read_session(self, name: str, *, lines: int = 500, output: str = "tail",
                     since_turn: int | None = None) -> str:
        """Read a terminal snapshot or an explicitly selected transcript boundary."""
        if since_turn is not None and output != "since_turn":
            raise AgentDeliveryError("since-turn requires --output since_turn")
        if output == "since_turn" and since_turn is None:
            raise AgentDeliveryError("--output since_turn requires since-turn")
        with self._lock(name):
            record = self._load(name)
            if record.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
                if output not in ("tail", "all") or since_turn is not None:
                    raise AgentDeliveryError("interactive terminals expose snapshots, not final-answer boundaries")
                self._checked(record)
                text = agent.read(self.client, record.target(), lines=lines)
                agent._atomic_json(str(self._directory(name) / "output.json"),
                    {"text": text, "captured_at": time.time(), "pane_id": record.pane_id})
                return text
            response = self._worker(record, "read", mode=output,
                since_turn=since_turn, tail=lines if output == "tail" else None)
            result = as_mapping(response["result"], "read result")
            return str(result.get("text", result.get("output", "")))

    def stop(
        self, name: str, *, expected_token: str | None = None,
        recover_legacy_adoption: bool = False,
        expected_record_sha256: str | None = None,
        retire_dead_adoption: bool = False,
    ) -> dict[str, object]:
        """Retire a runtime, then archive its canonical identity and artifacts."""
        if retire_dead_adoption and recover_legacy_adoption:
            raise AgentDeliveryError(
                "--retire-dead-adoption and --recover-legacy-adoption are mutually exclusive"
            )
        record = self._load_expected(name, expected_token)
        if record.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            return self._stop(
                name, expected_token=record.token,
                recover_legacy_adoption=recover_legacy_adoption,
                expected_record_sha256=expected_record_sha256,
                expected_token_explicit=expected_token is not None,
                retire_dead_adoption=retire_dead_adoption,
            )
        if retire_dead_adoption:
            raise AgentDeliveryError(
                "dead-adoption retirement is supported only for interactive herdr-foreign records"
            )
        if recover_legacy_adoption or expected_record_sha256 is not None:
            raise AgentDeliveryError(
                "legacy adoption recovery is supported only for interactive herdr-foreign records"
            )
        with self._lock(name):
            current = self._load(name)
            if current.token != record.token:
                raise AgentDeliveryError("session was replaced before stop")
            result = self._worker(current, "stop")
            current.lifecycle = "stopped"
            self._save(current)
            archive = self.registry / "archive"
            archive.mkdir(mode=0o700, exist_ok=True)
            agent._validate_private_directory(str(archive), "agent archive")
            destination = archive / f"{name}-{current.token}"
            os.rename(self._directory(name), destination)
            agent._fsync_dir(str(archive))
            agent._fsync_dir(str(self.registry))
            return {"name": name, "archive": str(destination),
                "runtime": _relocate_receipt_paths(result, self._directory(name), destination)}

    def pause(self, name: str, *, paused: bool = True) -> dict[str, object]:
        """Hand off input after in-flight work; preserve the running conversation."""
        with self._lock(name):
            record = self._load(name)
            if record.adapter == "turn-runner":
                self._worker(record, "pause" if paused else "resume")
            record.paused = paused
            self._save(record)
            return {"name": name, "token": record.token, "paused": paused}

    def runtime_operation(self, name: str, action: str, **options: object) -> dict[str, object]:
        """Run a headless capability under the same canonical lifecycle lock."""
        with self._lock(name):
            record = self._load(name)
            if record.adapter != "turn-runner":
                raise AgentDeliveryError(f"{action} requires a persistent turn-runner session")
            self._require_automation(record)
            result = self._worker(record, action, **options)
            self._sync_worker_record(record, result)
            return result

    def wait(self, name: str, *, timeout: float = 900.0, **options: object) -> dict[str, object]:
        """Wait for readiness of one pinned generation, without claiming goal completion."""
        if not math.isfinite(timeout) or not 0 <= timeout <= 31_536_000:
            raise AgentDeliveryError("wait timeout must be finite and between 0 and 31536000 seconds")
        initial = self._load(name)
        if initial.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            return super().wait(name, timeout=timeout, expected_token=initial.token, **options)  # type: ignore[arg-type]
        deadline = time.monotonic() + timeout
        while True:
            with self._lock(name):
                record = self._load_expected(name, initial.token)
                result = self._status_record(record)
                runtime = result.get("runtime")
                if isinstance(runtime, dict):
                    values = runtime.get("agents")
                    if isinstance(values, list) and len(values) == 1 and isinstance(values[0], dict):
                        status = values[0].get("status")
                        runner_alive = values[0].get("runner_alive")
                        if runner_alive is False:
                            raise AgentDeliveryError("worker runner is not alive; inspect status before restarting")
                        if runner_alive is not True:
                            raise AgentDeliveryError("cannot confirm worker runner liveness; inspect status")
                        if status == "idle" and values[0].get("pending", 0) == 0:
                            return result
                        if status in ("dead", "stopped", "blocked", "error"):
                            raise AgentDeliveryError(f"worker requires attention: {status}")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AgentDeliveryError("timed out waiting for worker readiness")
            time.sleep(min(0.25, remaining))

    def attach(self, name: str, *, expected_token: str | None = None) -> dict[str, object]:
        """Focus a verified Herdr tab or attach the current terminal to an exact tmux window."""
        initial = self._load_expected(name, expected_token)
        if initial.adapter in ("herdr", "herdr-pane", "herdr-foreign", "herdr-relay"):
            return super().attach(name, expected_token=initial.token)
        attach_command: list[str] | None = None
        with self._lock(name):
            record = self._load_expected(name, initial.token)
            response = self._worker(record, "status")
            runtime = as_mapping(response["record"], "worker identity")
            target = str(runtime.get("tmux_target", ""))
            if record.backend == "herdr":
                pane = runtime.get("presentation_pane")
                if not isinstance(pane, str) or not pane:
                    raise AgentDeliveryError("worker has no confirmed Herdr pane; use repair")
                panes = [entry for entry in self.client.panes() if entry.pane_id == pane]
                if len(panes) != 1 or panes[0].tab_id != target:
                    raise AgentDeliveryError("worker presentation pane no longer belongs to its recorded tab; use repair")
                self.client.focus_tab(panes[0].tab_id)
                focused = "tab"
            else:
                if not target or ":" not in target:
                    raise AgentDeliveryError("worker has no confirmed tmux target")
                session, window = target.split(":", 1)
                exact = "=" + session + ":=" + window
                completed = _bounded_control_command(["tmux", "display-message", "-p", "-t", exact, "#{window_id}"])
                window_id = completed.stdout.strip()
                if completed.returncode or not window_id.startswith("@") or not window_id[1:].isdigit():
                    raise AgentDeliveryError("worker tmux window is missing or ambiguous; use repair")
                # Resolve to a server-assigned window ID before focusing or attaching.
                # IDs are not reused while the server lives, unlike user-supplied names.
                if os.environ.get("TMUX"):
                    completed = _bounded_control_command(["tmux", "select-window", "-t", window_id])
                    if completed.returncode:
                        raise AgentDeliveryError(completed.stderr.strip())
                else:
                    if not sys.stdin.isatty() or not sys.stdout.isatty():
                        raise AgentDeliveryError("tmux attach requires an interactive terminal; inspect with agentctl read")
                    attach_command = ["tmux", "attach-session", "-t", window_id]
                focused = "window"
            result: dict[str, object] = {"name": name, "backend": record.backend,
                "target": target, "focused": focused, "paused": record.paused}
        # User attachment can last indefinitely. It must not monopolize lifecycle
        # ownership and prevent another controller from stopping or messaging it.
        if attach_command is not None:
            completed_attach = subprocess.run(attach_command, check=False)
            if completed_attach.returncode:
                raise AgentDeliveryError(f"tmux attachment failed with exit {completed_attach.returncode}")
        return result


def _relocate_receipt_paths(value: object, source: Path, destination: Path) -> object:
    """Keep path receipts truthful when the containing canonical directory moves."""
    if isinstance(value, dict):
        result: dict[str, object] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                continue
            if key in ("archived_to", "state_path", "runtime_home") and isinstance(item, str):
                try:
                    relative = Path(item).relative_to(source)
                except ValueError:
                    result[key] = item
                else:
                    result[key] = str(destination / relative)
            else:
                result[key] = _relocate_receipt_paths(item, source, destination)
        return result
    if isinstance(value, list):
        return [_relocate_receipt_paths(item, source, destination) for item in value]
    return value
