"""Lifecycle regressions for visible subagents, including failures and ownership changes."""
from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import replace
from pathlib import Path
from typing import cast

import pytest

from agentctl import agent as delivery
from agentctl.client import (
    AgentIdentity, AgentPaneInfo, CustomProcessIdentity, HerdrClient, Pane, PaneShellProof,
)
from agentctl.errors import (
    AgentDeliveryError, AgentPending, HerdrUnavailable, InputExpectationFailed,
)
from agentctl.subagents import (
    ManagedAgents, _GuardedTerminal, _WorkspaceClient, environment_entries, harness_arguments,
)
import agentctl.legacy_cli as cli
import agentctl.codex_goal as native_goal


class FakeManagedClient:
    def __init__(self) -> None:
        self.workspace: str | None = None
        self.infos: dict[str, AgentPaneInfo] = {}
        self.presentations: list[Pane] = []
        self.launched: list[tuple[str, str, str, tuple[str, ...]]] = []
        self.moved_named_panes: dict[str, str] = {}
        self.moves: list[tuple[str, str, str]] = []
        self.environments: list[tuple[str, ...]] = []
        self.closed: list[str] = []
        self.submitted: list[str] = []
        self.panes_calls = 0
        self.pane_info_calls = 0
        self.serial = 0
        self.offline = False
        self.fail_start = False
        self.custom_dies_after_report = False
        self.custom_fails_after_observation = False
        self.custom_running = False
        self.custom_at_idle_shell = True
        self.custom_identity = CustomProcessIdentity(
            version=1, boot_id="00000000-0000-0000-0000-000000000000",
            pid=200, starttime_ticks=200, executable_device=1, executable_inode=2,
        )
        self.foreign_shell_identity = CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=100, starttime_ticks=100, executable_device=3, executable_inode=4,
        )
        self.labels: dict[str, str] = {}
        #: Foreground harness pid per pane; a test replaces the harness by changing it.
        self.harness_pids: dict[str, int] = {}
        self.expect_supported = False
        #: Called with the pane id immediately before each input effect reaches the pane.
        self.before_effect: Callable[[str], None] | None = None
        self.expected_terminals: list[str | None] = []
        #: Text each pane has shown in its scrollback.
        self.transcripts: dict[str, list[str]] = {}
        #: One-shot: input addressed to the key pane is written to the value pane instead.
        self.redirect_once: dict[str, str] = {}
        self.keys_sent: list[tuple[str, str]] = []

    def workspace_id_for_label(self, label: str) -> str | None:
        if label == "project-agents":
            return "w-project"
        assert label == "subagents"
        return self.workspace

    def workspace_label(self, workspace_id: str) -> str:
        if workspace_id == "w-project":
            return "project-agents"
        assert workspace_id == "w1"
        return "subagents"

    def create_workspace(
        self, *, label: str, cwd: str, environment: tuple[str, ...] = (),
    ) -> tuple[str, str, str]:
        self.workspace = "w1"
        tab = self.create_tab(
            workspace_id="w1", label=label, cwd=cwd, environment=environment
        )
        return "w1", tab, self.presentations[-1].pane_id

    def create_tab(
        self, *, workspace_id: str, label: str, cwd: str,
        environment: tuple[str, ...] = (),
    ) -> str:
        self.environments.append(tuple(environment))
        self.serial += 1
        tab, pane = f"w1:t{self.serial}", f"w1:p{self.serial}"
        self.presentations.append(Pane(pane, tab, workspace_id))
        self.labels[tab] = label
        self.infos[pane] = AgentPaneInfo(
            pane, workspace_id, cwd, None, "unknown", None, None,
            terminal_id=f"term-{self.serial}", tab_id=tab,
        )
        return tab

    def rename_tab(self, tab_id: str, label: str) -> None:
        self.labels[tab_id] = label

    def tab_label(self, tab_id: str) -> str:
        if self.offline:
            raise HerdrUnavailable("server unavailable")
        return self.labels[tab_id]

    def tab_labels(self, workspace_id: str) -> dict[str, str]:
        return {pane.tab_id: self.labels.get(pane.tab_id, "") for pane in self.presentations
                if pane.workspace_id == workspace_id}

    def agent_names(self) -> dict[str, str]:
        names = {entry[0]: entry[2] for entry in self.launched}
        names.update(self.moved_named_panes)
        live = {pane.pane_id for pane in self.presentations}
        return {name: pane for name, pane in names.items() if pane in live}

    def agent_identity(self, name: str) -> AgentIdentity:
        pane_id = self.agent_pane(name)
        tab = next((pane.tab_id for pane in self.presentations if pane.pane_id == pane_id), None)
        return AgentIdentity(name, pane_id, tab, self.infos[pane_id].terminal_id)

    def rename_agent(self, pane_id: str, name: str) -> None:
        self.launched = [(name if entry[2] == pane_id else entry[0], *entry[1:])
                         for entry in self.launched]
        for old, pane in list(self.moved_named_panes.items()):
            if pane == pane_id:
                del self.moved_named_panes[old]
                self.moved_named_panes[name] = pane

    def harness_identity(self, pane_id: str, kind: str = "") -> CustomProcessIdentity | None:
        if self.infos[pane_id].agent is None:
            return None
        pid = self.harness_pids.setdefault(pane_id, 300 + len(self.harness_pids))
        return CustomProcessIdentity(
            version=1, boot_id="00000000-0000-0000-0000-000000000000",
            pid=pid, starttime_ticks=pid, executable_device=5, executable_inode=6,
        )

    def verify_harness_identity(self, pane_id: str, expected: CustomProcessIdentity) -> bool:
        return (pane_id in self.infos and self.infos[pane_id].agent is not None
                and self.harness_pids.get(pane_id) == expected.pid)

    def input_expect_supported(self) -> bool:
        return self.expect_supported

    def _effect(self, pane_id: str, expect_terminal: str | None) -> None:
        if self.before_effect is not None:
            self.before_effect(pane_id)
        self.expected_terminals.append(expect_terminal)
        if expect_terminal is not None and self.infos[pane_id].terminal_id != expect_terminal:
            raise InputExpectationFailed(
                f'agent prompt {pane_id}: {{"error":{{"code":"expectation_failed"}}}}'
            )

    def agent_prompt(self, pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        self._effect(pane_id, expect_terminal)
        assert pane_id in self.infos
        written = self.redirect_once.pop(pane_id, pane_id)
        self.submitted.append(text)
        self.transcripts.setdefault(written, []).append(text)

    def create_tab_with_pane(
        self, *, workspace_id: str, label: str, cwd: str,
        environment: tuple[str, ...] = (),
    ) -> tuple[str, str]:
        tab = self.create_tab(
            workspace_id=workspace_id, label=label, cwd=cwd,
            environment=environment,
        )
        return tab, self.presentations[-1].pane_id

    def start_agent(self, name: str, kind: str, pane_id: str, arguments: tuple[str, ...], *, timeout: float) -> None:
        assert timeout > 0
        self.launched.append((name, kind, pane_id, arguments))
        if self.fail_start:
            raise HerdrUnavailable("trust prompt needs attention")
        conversation = (arguments[arguments.index("--session-id") + 1] if "--session-id" in arguments
                        else arguments[arguments.index("--resume") + 1] if "--resume" in arguments
                        else arguments[arguments.index("resume") + 1] if "resume" in arguments
                        else f"session-{self.serial}")
        self.infos[pane_id] = replace(self.infos[pane_id], agent=kind, status="idle", session_agent=kind, session_value=conversation)

    def start_pane_agent(
        self, name: str, kind: str, pane_id: str, arguments: tuple[str, ...], *, timeout: float,
        on_observed: Callable[[CustomProcessIdentity], None] | None = None,
    ) -> CustomProcessIdentity:
        assert timeout > 0
        self.launched.append((name, kind, pane_id, arguments))
        self.custom_running = True
        if on_observed is not None:
            on_observed(self.custom_identity)
        if self.custom_fails_after_observation:
            raise HerdrUnavailable("trust prompt requires human attention")
        self.infos[pane_id] = replace(
            self.infos[pane_id], agent=kind, status="idle",
        )
        self.custom_running = not self.custom_dies_after_report
        return self.custom_identity

    def verify_custom_harness(
        self, pane_id: str, kind: str,
        expected_identity: CustomProcessIdentity | None = None,
    ) -> None:
        if (self.infos[pane_id].agent not in (None, kind) or not self.custom_running
                or (expected_identity is not None
                    and expected_identity != self.custom_identity)):
            raise HerdrUnavailable("custom harness is not the foreground process")

    def pane_is_idle_shell(self, pane_id: str) -> bool:
        assert pane_id in self.infos
        return self.custom_at_idle_shell

    def pane_shell_identity(self, pane_id: str) -> CustomProcessIdentity:
        assert pane_id in self.infos
        return self.foreign_shell_identity

    def pane_is_same_idle_shell(
        self, pane_id: str, expected: CustomProcessIdentity,
    ) -> bool:
        assert pane_id in self.infos
        return self.custom_at_idle_shell and expected == self.foreign_shell_identity

    def pane_idle_shell_identity(self, pane_id: str) -> PaneShellProof | None:
        assert pane_id in self.infos
        if not self.custom_at_idle_shell:
            return None
        return PaneShellProof(
            self.foreign_shell_identity, str(Path("/bin/bash").resolve())
        )

    def agent_pane(self, name: str) -> str:
        if name in self.moved_named_panes:
            return self.moved_named_panes[name]
        matches = [entry[2] for entry in self.launched if entry[0] == name]
        if self.offline or not matches:
            raise HerdrUnavailable("server unavailable or named agent missing")
        return matches[-1]

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        self.pane_info_calls += 1
        if self.offline:
            raise HerdrUnavailable("server unavailable")
        return self.infos[pane_id]

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        self.panes_calls += 1
        if self.offline:
            raise HerdrUnavailable("server unavailable")
        return tuple(
            pane for pane in self.presentations
            if workspace_id is None or pane.workspace_id == workspace_id
        )

    def move_pane_to_new_tab(
        self, pane_id: str, *, workspace_id: str, tab_label: str,
    ) -> Pane:
        source = next(
            (pane for pane in self.presentations if pane.pane_id == pane_id), None
        )
        if source is None:
            raise HerdrUnavailable("missing source pane")
        self.presentations.remove(source)
        moved = Pane("w-project:p1", "w-project:t1", workspace_id)
        self.presentations.append(moved)
        self.labels[moved.tab_id] = tab_label
        old = self.infos.pop(pane_id)
        self.infos[moved.pane_id] = replace(
            old, pane_id=moved.pane_id, workspace_id=workspace_id, tab_id=moved.tab_id,
        )
        if pane_id in self.harness_pids:
            self.harness_pids[moved.pane_id] = self.harness_pids.pop(pane_id)
        self.moved_named_panes[tab_label] = moved.pane_id
        self.moves.append((pane_id, workspace_id, tab_label))
        return moved

    def prompt_agent(self, pane_id: str, text: str, *, terminal: object = None) -> None:
        if terminal is not None:
            cast(_GuardedTerminal, terminal).native_prompt(pane_id, text)
            return
        assert pane_id in self.infos
        self.submitted.append(text)

    def wait_agent_status(self, pane_id: str, status: str, timeout_ms: int) -> None:
        assert pane_id in self.infos and status == "working" and timeout_ms > 0

    def read(self, pane_id: str, *, source: str, lines: int) -> str:
        assert pane_id in self.infos and lines > 0
        return "" if source == "recent-unwrapped" else "human and coordinator transcript\n"

    def read_scrollback(self, pane_id: str) -> str:
        assert pane_id in self.infos
        return "\n".join(self.transcripts.get(pane_id, []))

    def close_tab(self, tab_id: str) -> None:
        self.closed.append(tab_id)
        self.presentations = [pane for pane in self.presentations if pane.tab_id != tab_id]

    def close_pane(self, pane_id: str) -> None:
        tab = next(pane.tab_id for pane in self.presentations if pane.pane_id == pane_id)
        self.closed.append(tab)
        self.presentations = [pane for pane in self.presentations if pane.pane_id != pane_id]

    def focus_pane(self, pane_id: str) -> None:
        assert pane_id in self.infos

    def report_agent_session(self, name: str, pane_id: str, kind: str, session_id: str) -> None:
        assert self.agent_pane(name) == pane_id
        self.infos[pane_id] = replace(self.infos[pane_id], session_agent=kind, session_value=session_id)

    def send_keys(self, pane_id: str, keys: str, *, expect_terminal: str | None = None) -> None:
        if keys != "esc":
            self._effect(pane_id, expect_terminal)
        assert pane_id in self.infos and keys in ("Enter", "esc")
        self.keys_sent.append((pane_id, keys))


def setup(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[ManagedAgents, FakeManagedClient]:
    monkeypatch.delenv("HERDR_WORKSPACE_ID", raising=False)
    fake = FakeManagedClient()
    def goal(_session: str, _command: object = None) -> dict[str, object] | None:
        objectives = [text[6:] for text in fake.submitted if text.startswith("/goal ")]
        return {"status": "active", "objective": objectives[-1]} if objectives else None
    monkeypatch.setattr(native_goal, "get_goal", goal)
    return ManagedAgents(cast(HerdrClient, fake), tmp_path / "registry"), fake


def _nested_v2_record(flat: dict[str, object]) -> dict[str, object]:
    """Encode one historical flat fixture in the canonical launch-only v2 shape."""
    arguments = cast(list[str], flat["arguments"])
    return {
        "schema": "agentctl-session/v2",
        "name": flat["name"],
        "token": flat["token"],
        "created_at": flat["created_at"],
        "lifecycle": flat["lifecycle"],
        "launch": {
            "schema": "agentctl-launch/v1",
            "harness": flat["harness"],
            "cwd": flat["cwd"],
            "adapter": flat["adapter"],
            "mode": flat["mode"],
            "backend": flat["backend"],
            "model": flat["model"],
            "resume": flat["resume"],
            "profile": None,
            "argv": [flat["harness"], *arguments],
            "environment_names": [],
            "runtime_home": flat["runtime_home"],
            "runtime_ownership": "owned",
            "executable": None,
        },
        "workspace_id": flat["workspace_id"],
        "tab_id": flat["tab_id"],
        "pane_id": flat["pane_id"],
        "session_agent": flat["session_agent"],
        "session_value": flat["session_value"],
        "startup_warning": flat["startup_warning"],
        "effective_reasoning_effort": flat["effective_reasoning_effort"],
        "error": flat["error"],
        "goal": flat["goal"],
        "goal_delivery": flat["goal_delivery"],
        "goal_session_id": flat["goal_session_id"],
        "goal_command": flat["goal_command"],
        "goal_messages": flat["goal_messages"],
        "goal_message_id": flat["goal_message_id"],
        "paused": flat["paused"],
        "pane_reported_by_agentctl": flat["pane_reported_by_agentctl"],
        "custom_process_identity": flat["custom_process_identity"],
        "foreign_shell_identity": flat["foreign_shell_identity"],
        "runner_identity": None,
        "extensions": {},
    }


def test_named_workers_share_workspace_but_never_reuse_tabs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    first = manager.start("one", cwd=str(tmp_path), harness="codex", brief="start here")
    second = manager.start("two", cwd=str(tmp_path), harness="claude", model="selected-model")
    assert first["workspace_id"] == second["workspace_id"] == "w1"
    assert first["pane_id"] != second["pane_id"]
    assert fake.submitted == ["start here"]
    assert fake.launched[0][3] == ("--no-alt-screen",)
    assert fake.launched[1][3] == ("--session-id", cast(dict[str, str], second["native_session"])["value"], "--model", "selected-model")
    assert [item["name"] for item in manager.list()] == ["one", "two"]
    with pytest.raises(AgentDeliveryError, match="already registered"):
        manager.start("one", cwd=str(tmp_path))
    assert len(fake.launched) == 2


def test_project_workspace_selects_destination_and_rejects_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.project_workspace = "project-agents"
    status = manager.start("worker", cwd=str(tmp_path), harness="codex")
    assert status["workspace_id"] == "w-project"
    assert status["agent_status"] == "idle"
    assert fake.presentations[0].workspace_id == "w-project"

    rejected, rejected_fake = setup(tmp_path / "mismatch", monkeypatch)
    rejected.project_workspace = "project-agents"
    with pytest.raises(AgentDeliveryError, match="project configuration requires"):
        rejected.start(
            "worker", cwd=str(tmp_path), harness="codex", workspace_id="w1",
        )
    assert rejected_fake.launched == []
    assert rejected_fake.presentations == []


@pytest.mark.parametrize("project_workspace", [None, "project-agents"])
def test_managed_legacy_workspace_binding_is_accepted_without_rewrite(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    project_workspace: str | None,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.project_workspace = project_workspace
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    record = manager.get("worker")
    record.session_agent = record.session_value = None
    manager._save(record)
    assert record.pane_id is not None
    fake.infos[record.pane_id] = replace(
        fake.infos[record.pane_id], session_agent=None, session_value=None,
    )
    manager.send("worker", "create binding")
    target = manager.registry / "worker" / "queue" / "target.json"
    document = json.loads(target.read_text(encoding="utf-8"))
    document["expected_workspace"] = "legacy-project"
    target.write_text(json.dumps(document, sort_keys=True) + "\n", encoding="utf-8")
    before = target.read_bytes()

    assert manager.status("worker")["agent_status"] == "idle"
    manager.send("worker", "legacy compatible")
    assert fake.submitted[-1] == "legacy compatible"
    assert target.read_bytes() == before
    manager.drain("worker")
    assert target.read_bytes() == before
    manager.goal("worker", "legacy compatible goal")
    assert fake.submitted[-1] == "/goal legacy compatible goal"
    assert target.read_bytes() == before


def test_legacy_cli_workspace_pin_is_exact(tmp_path: Path) -> None:
    queue = tmp_path / "queue"
    pinned = delivery.Target(
        pane_id="w1:p1", expected_agent="codex",
        expected_workspace="legacy-project", expected_cwd=str(tmp_path),
    )
    delivery._bind_queue(str(queue), pinned)
    unpinned = replace(pinned, expected_workspace=None)
    with pytest.raises(AgentDeliveryError, match="refusing different target"):
        delivery._validate_existing_binding(str(queue), unpinned)


def test_move_preserves_process_and_commits_new_herdr_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    manager.send("worker", "establish queue binding")
    manager.project_workspace = "project-agents"

    launches = list(fake.launched)
    moved = manager.move_to_project_workspace("worker")
    assert moved["moved"] is True
    assert moved["recovered"] is False
    assert moved["previous_pane_id"] == "w1:p1"
    assert moved["pane_id"] == "w-project:p1"
    assert moved["tab_id"] == "w-project:t1"
    assert moved["workspace_id"] == "w-project"
    assert moved["agent_status"] == "idle"
    assert fake.moves == [("w1:p1", "w-project", "worker")]
    assert fake.launched == launches

    record = manager.get("worker")
    assert record.pane_id == "w-project:p1"
    assert record.tab_id == "w-project:t1"
    assert record.workspace_id == "w-project"
    assert manager.read("worker", lines=10) == "human and coordinator transcript\n"

    already_there = manager.move_to_project_workspace("worker")
    assert already_there["moved"] is False
    assert fake.moves == [("w1:p1", "w-project", "worker")]


def test_move_recovers_only_with_durable_intent_and_rebinds_delivery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    sessionless = manager.get("worker")
    sessionless.session_agent = sessionless.session_value = None
    manager._save(sessionless)
    assert sessionless.pane_id is not None
    fake.infos[sessionless.pane_id] = replace(
        fake.infos[sessionless.pane_id], session_agent=None, session_value=None,
    )
    manager.send("worker", "before move")
    manager.project_workspace = "project-agents"
    record = manager.get("worker")

    fake.move_pane_to_new_tab(
        record.pane_id or "", workspace_id="w-project", tab_label="worker",
    )
    with pytest.raises(AgentDeliveryError, match="no durable move intent"):
        manager.move_to_project_workspace("worker")

    manager._write_move_intent(record, "w-project")
    pending = manager.status("worker")
    assert pending["move_pending"] is True
    assert pending["probe_error"] == (
        "move of 'worker' is incomplete; rerun `agentctl move worker`"
    )
    with pytest.raises(AgentDeliveryError, match="move is incomplete"):
        manager.stop("worker")
    replacement = replace(record.target(), pane_id="w-project:p1")
    delivery._atomic_json(
        str(manager.registry / "worker" / "queue" / "target.json"),
        delivery._binding(replacement),
    )
    recovered = manager.move_to_project_workspace("worker")
    assert recovered["recovered"] is True
    manager.send("worker", "after recovery")
    assert fake.submitted[-1] == "after recovery"
    assert not (manager.registry / "worker" / "move.json").exists()


def test_stale_completed_move_intent_is_row_local_and_rerun_clears_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    before = manager.get("worker")
    manager.project_workspace = "project-agents"
    manager.move_to_project_workspace("worker")
    manager._write_move_intent(before, "w-project")

    status = manager.status("worker")
    assert status["move_pending"] is True
    assert status["probe_error"] == (
        "move of 'worker' is incomplete; rerun `agentctl move worker`"
    )
    rows = manager.list()
    assert len(rows) == 1
    assert rows[0]["name"] == "worker"
    assert "rerun `agentctl move worker`" in str(rows[0]["probe_error"])

    repeated = manager.move_to_project_workspace("worker")
    assert repeated["moved"] is False
    assert not (manager.registry / "worker" / "move.json").exists()

    delivery._atomic_json(str(manager.registry / "worker" / "move.json"), {})
    invalid = manager.status("worker")
    assert invalid["agent_status"] == "unknown"
    assert "rerun `agentctl move worker`" in str(invalid["probe_error"])
    assert len(manager.list()) == 1


def test_pending_move_can_finish_after_workspace_policy_is_removed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    record = manager.get("worker")
    manager.project_workspace = "project-agents"
    manager._write_move_intent(record, "w-project")
    manager.project_workspace = None

    moved = manager.move_to_project_workspace("worker")
    assert moved["workspace_id"] == "w-project"
    assert fake.moves == [("w1:p1", "w-project", "worker")]
    assert not (manager.registry / "worker" / "move.json").exists()
    assert manager.stop("worker")["pane_closed"] is True


def test_queue_rebind_is_idempotent_and_refuses_unrelated_identity(
    tmp_path: Path,
) -> None:
    queue = tmp_path / "queue"
    previous = delivery.Target(
        pane_id="w1:p1", expected_agent="codex",
        expected_workspace="old-label", expected_cwd=str(tmp_path),
    )
    replacement = delivery.Target(
        pane_id="w2:p2", expected_agent="codex",
        expected_workspace="new-label", expected_cwd=str(tmp_path),
    )
    delivery._bind_queue(str(queue), previous)
    delivery.rebind_queue(str(queue), previous, replacement)
    delivery.rebind_queue(str(queue), previous, replacement)
    stored = json.loads((queue / "target.json").read_text(encoding="utf-8"))
    assert stored["pane_id"] == "w2:p2"
    stored["pane_id"] = "foreign"
    (queue / "target.json").write_text(json.dumps(stored), encoding="utf-8")
    with pytest.raises(AgentDeliveryError, match="refusing move"):
        delivery.rebind_queue(str(queue), previous, replacement)


def test_move_preflights_binding_and_preserves_queued_states(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    manager.send("worker", "bind queue")
    queue = manager.registry / "worker" / "queue"
    target = queue / "target.json"
    document = json.loads(target.read_text(encoding="utf-8"))
    document.update({"kind": "pane", "pane_id": "foreign", "value": None})
    document.pop("agent", None)
    target.write_text(json.dumps(document), encoding="utf-8")
    manager.project_workspace = "project-agents"
    with pytest.raises(AgentDeliveryError, match="refusing move"):
        manager.move_to_project_workspace("worker")
    assert fake.moves == []

    # Restore the canonical binding through an ordinary send, then pin queue
    # artifacts that a move must neither replay nor discard.
    target.unlink()
    manager.project_workspace = None
    manager.send("worker", "restore binding")
    for state in ("inbox", "inflight", "processed", "failed"):
        directory = queue / state
        directory.mkdir(mode=0o700, exist_ok=True)
        (directory / f"{state}.json").write_text("{}", encoding="utf-8")
        (directory / f"{state}.json").chmod(0o600)
    manager.project_workspace = "project-agents"
    manager.move_to_project_workspace("worker")
    for state in ("inbox", "inflight", "processed", "failed"):
        assert (queue / state / f"{state}.json").exists()


def test_pane_binding_follows_move_and_sends_to_replacement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    record = manager.get("worker")
    record.session_agent = record.session_value = None
    manager._save(record)
    fake.infos[record.pane_id or ""] = replace(
        fake.infos[record.pane_id or ""], session_agent=None, session_value=None,
    )
    manager.send("worker", "bind pane")
    manager.project_workspace = "project-agents"
    manager.move_to_project_workspace("worker")
    manager.send("worker", "replacement pane")
    assert fake.submitted[-1] == "replacement pane"


def test_move_refuses_sibling_tab_before_touching_herdr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.presentations.append(Pane("w1:sibling", "w1:t1", "w1"))
    fake.infos["w1:sibling"] = AgentPaneInfo(
        "w1:sibling", "w1", str(tmp_path), "codex", "idle", "codex", "other",
    )
    manager.project_workspace = "project-agents"
    with pytest.raises(AgentDeliveryError, match="tab contains another pane"):
        manager.move_to_project_workspace("worker")
    assert fake.moves == []


def test_move_refuses_name_and_final_presentation_mismatches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path / "name", monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    manager.project_workspace = "project-agents"
    original = fake.move_pane_to_new_tab

    def lose_name(pane_id: str, *, workspace_id: str, tab_label: str) -> Pane:
        moved = original(pane_id, workspace_id=workspace_id, tab_label=tab_label)
        fake.moved_named_panes[tab_label] = "missing"
        return moved

    monkeypatch.setattr(fake, "move_pane_to_new_tab", lose_name)
    with pytest.raises(AgentDeliveryError, match="managed name did not follow"):
        manager.move_to_project_workspace("worker")
    assert (manager.registry / "worker" / "move.json").exists()

    presentation, presentation_fake = setup(tmp_path / "presentation", monkeypatch)
    presentation.start("worker", cwd=str(tmp_path))
    presentation.project_workspace = "project-agents"
    presentation_original = presentation_fake.move_pane_to_new_tab

    def wrong_result(pane_id: str, *, workspace_id: str, tab_label: str) -> Pane:
        moved = presentation_original(
            pane_id, workspace_id=workspace_id, tab_label=tab_label,
        )
        return Pane(moved.pane_id, "reported-wrong-tab", moved.workspace_id)

    monkeypatch.setattr(presentation_fake, "move_pane_to_new_tab", wrong_result)
    with pytest.raises(AgentDeliveryError, match="final presentation verification failed"):
        presentation.move_to_project_workspace("worker")
    assert (presentation.registry / "worker" / "move.json").exists()


def test_failed_pre_herdr_move_clears_intent_and_cannot_authorize_recovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    manager.project_workspace = "project-agents"

    def refuse(*args: object, **kwargs: object) -> Pane:
        del args, kwargs
        raise HerdrUnavailable("move refused before mutation")

    monkeypatch.setattr(fake, "move_pane_to_new_tab", refuse)
    with pytest.raises(HerdrUnavailable, match="before mutation"):
        manager.move_to_project_workspace("worker")
    assert not (manager.registry / "worker" / "move.json").exists()

    fake.presentations = [Pane("replacement", "other", "w-project")]
    fake.infos["replacement"] = replace(
        next(iter(fake.infos.values())), pane_id="replacement", workspace_id="w-project",
    )
    fake.moved_named_panes["worker"] = "replacement"
    with pytest.raises(AgentDeliveryError, match="no durable move intent"):
        manager.move_to_project_workspace("worker")


def test_move_refuses_false_recovery_and_unsupported_records(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path / "both", monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    manager.project_workspace = "project-agents"
    record = manager.get("worker")
    manager._write_move_intent(record, "w-project")
    fake.presentations.append(Pane("replacement", "other", "w-project"))
    fake.infos["replacement"] = replace(
        fake.infos[record.pane_id or ""], pane_id="replacement", workspace_id="w-project",
    )
    fake.moved_named_panes["worker"] = "replacement"
    with pytest.raises(AgentDeliveryError, match="both recorded and named panes are live"):
        manager.move_to_project_workspace("worker")

    no_policy, _ = setup(tmp_path / "none", monkeypatch)
    no_policy.start("worker", cwd=str(tmp_path))
    with pytest.raises(AgentDeliveryError, match="requires a workspace field"):
        no_policy.move_to_project_workspace("worker")

    adopted, adopted_fake = setup(tmp_path / "adopt", monkeypatch)
    adopted_fake.workspace = "w1"
    adopted_fake.presentations.append(Pane("w1:p1", "w1:t1", "w1"))
    adopted_fake.infos["w1:p1"] = AgentPaneInfo(
        "w1:p1", "w1", str(tmp_path), "codex", "idle", None, None,
    )
    adopted.adopt(
        "foreign", pane_id="w1:p1", expected_workspace="subagents",
        expected_cwd=str(tmp_path), harness="codex",
    )
    adopted.project_workspace = "project-agents"
    with pytest.raises(AgentDeliveryError, match="supports only"):
        adopted.move_to_project_workspace("foreign")

    custom, _ = setup(tmp_path / "custom", monkeypatch)
    custom.start("worker", cwd=str(tmp_path), harness="muse")
    custom.project_workspace = "project-agents"
    with pytest.raises(AgentDeliveryError, match="supports only"):
        custom.move_to_project_workspace("worker")

    mismatch, _ = setup(tmp_path / "adopt-mismatch", monkeypatch)
    mismatch.project_workspace = "project-agents"
    with pytest.raises(AgentDeliveryError, match="does not match project workspace"):
        mismatch.adopt(
            "foreign", pane_id="w1:p1", expected_workspace="subagents",
            expected_cwd=str(tmp_path), harness="codex",
        )


def test_policy_does_not_block_status_stop_or_existing_queue_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    manager.send("worker", "before policy")
    manager.project_workspace = "project-agents"
    assert manager.status("worker")["agent_status"] == "idle"
    stopped = manager.stop("worker")
    assert stopped["pane_closed"] is True

    adopted, adopted_fake = setup(tmp_path / "adopted", monkeypatch)
    adopted_fake.workspace = "w1"
    adopted_fake.presentations.append(Pane("w1:p1", "w1:t1", "w1"))
    adopted_fake.infos["w1:p1"] = AgentPaneInfo(
        "w1:p1", "w1", str(tmp_path), "codex", "idle", "codex", "foreign",
    )
    adopted.adopt(
        "foreign", pane_id="w1:p1", expected_workspace="subagents",
        expected_cwd=str(tmp_path), harness="codex", session="foreign",
    )
    adopted.project_workspace = "project-agents"
    assert adopted.stop("foreign")["runtime_preserved"] is True


def test_policy_changes_never_become_queue_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original, fake = setup(tmp_path / "owned", monkeypatch)
    original.start("worker", cwd=str(tmp_path))
    original.send("worker", "before policy")
    policy = ManagedAgents(cast(HerdrClient, fake), original.registry)
    policy.project_workspace = "project-agents"
    policy.move_to_project_workspace("worker")
    original.send("worker", "stale manager")
    policy.project_workspace = None
    policy.send("worker", "removed policy")
    assert fake.submitted[-2:] == ["stale manager", "removed policy"]

    adopted, adopted_fake = setup(tmp_path / "adopted-in-place", monkeypatch)
    adopted_fake.presentations.append(Pane("w-project:p1", "w-project:t1", "w-project"))
    adopted_fake.infos["w-project:p1"] = AgentPaneInfo(
        "w-project:p1", "w-project", str(tmp_path), "codex", "idle", None, None,
    )
    adopted.project_workspace = "project-agents"
    adopted.adopt(
        "foreign", pane_id="w-project:p1", expected_workspace="project-agents",
        expected_cwd=str(tmp_path), harness="codex",
    )
    adopted.send("foreign", "adopted in place")
    assert adopted_fake.submitted[-1] == "adopted in place"


def test_workspace_identity_guard_and_policy_override_inherited_workspace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("HERDR_WORKSPACE_ID", "w1")
    manager, fake = setup(tmp_path, monkeypatch)
    monkeypatch.setenv("HERDR_WORKSPACE_ID", "w1")
    manager.project_workspace = "project-agents"
    status = manager.start("worker", cwd=str(tmp_path))
    assert status["workspace_id"] == "w-project"
    pane = status["pane_id"]
    assert isinstance(pane, str)
    manager.project_workspace = None
    fake.infos[pane] = replace(fake.infos[pane], workspace_id="w1")
    with pytest.raises(HerdrUnavailable, match="workspace identity changed"):
        manager.read("worker")


def test_project_policy_blocks_control_until_misplaced_agent_is_moved(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    manager.project_workspace = "project-agents"
    with pytest.raises(
        AgentDeliveryError,
        match=r"refusing pane w1:p1: workspace is 'subagents', expected 'project-agents'",
    ):
        manager.read("worker", lines=10)
    assert fake.moves == []


def test_launch_only_v2_record_is_read_and_rewritten_without_flat_duplicates(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    path.write_text(json.dumps(nested), encoding="utf-8")

    record = manager.get("worker")
    assert record.harness == "codex"
    assert record.arguments == ["--no-alt-screen"]
    assert manager.status("worker")["launch"] == nested["launch"]
    assert manager.pause("worker", paused=True)["paused"] is True

    stored = json.loads(path.read_text(encoding="utf-8"))
    assert stored["schema"] == "agentctl-session/v2"
    assert stored["launch"] == nested["launch"]
    for duplicate in ("harness", "cwd", "adapter", "mode", "backend", "arguments"):
        assert duplicate not in stored


def test_launch_only_v3_record_decodes_nested_goal_and_native_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    nested["schema"] = "agentctl-session/v3"
    session_agent = nested.pop("session_agent")
    session_value = nested.pop("session_value")
    nested["native_session"] = {
        "schema": "agentctl-native-session/v1",
        "agent": session_agent,
        "value": session_value,
        "source": "observed",
    }
    nested["goal"] = {
        "schema": "agentctl-goal/v1",
        "objective": nested.pop("goal"),
        "message_id": nested.pop("goal_message_id"),
        "native_command": nested.pop("goal_command"),
    }
    for key in ("goal_delivery", "goal_session_id", "goal_messages"):
        nested.pop(key)
    path.write_text(json.dumps(nested), encoding="utf-8")

    record = manager.get("worker")
    assert record.harness == "codex"
    assert record.session_agent == "codex"
    assert record.session_value == "session-1"
    assert record.to_document() == nested


@pytest.mark.parametrize("source", ["observed", "asserted"])
def test_v2_observed_and_asserted_session_fields_round_trip_separately(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, source: str,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    if source == "observed":
        nested["session_agent"] = "codex"
        nested["session_value"] = "observed-session"
        nested["goal_session_id"] = None
    else:
        nested["session_agent"] = None
        nested["session_value"] = None
        nested["goal_session_id"] = "asserted-session"
    path.write_text(json.dumps(nested), encoding="utf-8")

    record = manager.get("worker")
    assert record.to_document() == nested
    target = record.target()
    if source == "observed":
        assert target.session_agent == "codex"
        assert target.session_value == "observed-session"
    else:
        assert target.session_agent is None
        assert target.session_value is None


@pytest.mark.parametrize(
    ("source", "expected_status"),
    [("asserted", "idle"), ("observed", "unknown")],
)
def test_v3_native_session_source_controls_live_routing_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    source: str, expected_status: str,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    nested["schema"] = "agentctl-session/v3"
    nested.pop("session_agent")
    nested.pop("session_value")
    nested["native_session"] = {
        "schema": "agentctl-native-session/v1",
        "agent": "codex",
        "value": "asserted-session",
        "source": source,
    }
    nested["goal"] = {
        "schema": "agentctl-goal/v1",
        "objective": nested.pop("goal"),
        "message_id": nested.pop("goal_message_id"),
        "native_command": nested.pop("goal_command"),
    }
    for key in ("goal_delivery", "goal_session_id", "goal_messages"):
        nested.pop(key)
    path.write_text(json.dumps(nested), encoding="utf-8")
    fake.infos["w1:p1"] = replace(
        fake.infos["w1:p1"], session_agent=None, session_value=None,
    )

    status = manager.status("worker")
    assert status["agent_status"] == expected_status
    if source == "asserted":
        assert status["probe_error"] is None
    else:
        assert "expected exactly one live pane" in str(status["probe_error"])


def test_v3_bind_session_persists_one_asserted_native_authority(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    nested["schema"] = "agentctl-session/v3"
    nested.pop("session_agent")
    nested.pop("session_value")
    nested["native_session"] = None
    nested["goal"] = {
        "schema": "agentctl-goal/v1",
        "objective": nested.pop("goal"),
        "message_id": nested.pop("goal_message_id"),
        "native_command": nested.pop("goal_command"),
    }
    for key in ("goal_delivery", "goal_session_id", "goal_messages"):
        nested.pop(key)
    path.write_text(json.dumps(nested), encoding="utf-8")
    fake.infos["w1:p1"] = replace(
        fake.infos["w1:p1"], session_agent=None, session_value=None,
    )

    assert manager.bind_session("worker", "manual-session")["source"] == "explicit"
    stored = json.loads(path.read_text(encoding="utf-8"))
    assert stored["native_session"] == {
        "schema": "agentctl-native-session/v1",
        "agent": "codex",
        "value": "manual-session",
        "source": "asserted",
    }
    reloaded = manager.get("worker")
    assert reloaded.target().session_agent is None
    assert reloaded.target().session_value is None


def test_nested_launch_rejects_duplicate_flat_authority(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    nested["harness"] = "claude"
    path.write_text(json.dumps(nested), encoding="utf-8")
    with pytest.raises(AgentDeliveryError, match="agentctl-session/v2 fields"):
        manager.get("worker")


@pytest.mark.parametrize(
    ("mutation", "error"),
    [
        (lambda row: row.update(lifecycle="banana"), "lifecycle"),
        (
            lambda row: cast(dict[str, object], row["launch"]).update(
                runtime_ownership="foreign",
            ),
            "ownership or adapter",
        ),
        (
            lambda row: cast(dict[str, object], row["launch"]).update(
                argv=["claude", "--no-alt-screen"],
            ),
            "launch argv",
        ),
        (
            lambda row: cast(dict[str, object], row["launch"]).update(
                profile=17,
            ),
            "launch contract",
        ),
        (
            lambda row: row.update(runner_identity={"pid": 1}),
            "runner identity",
        ),
        (
            lambda row: row.update(session_agent="claude"),
            "native session identity",
        ),
    ],
)
def test_nested_launch_rejects_malformed_runtime_authority(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    mutation: Callable[[dict[str, object]], None], error: str,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    mutation(nested)
    path.write_text(json.dumps(nested), encoding="utf-8")

    with pytest.raises(AgentDeliveryError, match=error):
        manager.stop("worker", expected_token=cast(str, nested["token"]))
    assert fake.closed == []


def test_nested_launch_rejects_nonnull_interactive_permission_mode(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    launch = cast(dict[str, object], nested["launch"])
    launch["schema"] = "agentctl-launch/v2"
    launch["permission_mode"] = "bypass"
    path.write_text(json.dumps(nested), encoding="utf-8")
    with pytest.raises(AgentDeliveryError, match="launch contract"):
        manager.get("worker")


def test_running_nested_muse_without_process_identity_cannot_close_pane(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="muse")
    path = manager.registry / "worker" / "agent.json"
    nested = _nested_v2_record(json.loads(path.read_text(encoding="utf-8")))
    assert nested["custom_process_identity"] is not None
    nested["custom_process_identity"] = None
    path.write_text(json.dumps(nested), encoding="utf-8")

    with pytest.raises(AgentDeliveryError, match="no runtime process identity"):
        manager.stop("worker", expected_token=cast(str, nested["token"]))
    assert fake.closed == []


def test_list_reports_malformed_row_without_hiding_healthy_sessions(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="codex")
    invalid = manager.registry / "broken"
    invalid.mkdir(mode=0o700)
    (invalid / "agent.json").write_text("{}", encoding="utf-8")
    (invalid / "agent.json").chmod(0o600)

    rows = manager.list()
    assert [row["name"] for row in rows] == ["broken", "worker"]
    assert rows[0]["record_error"] is True
    assert rows[0]["agent_status"] == "unknown"
    assert "invalid agent record field name" in str(rows[0]["probe_error"])
    assert rows[1]["agent_status"] == "idle"


def test_harness_arguments_preserve_literals_without_implicit_permission_changes() -> None:
    assert harness_arguments("codex", resume="session", extra=("--config", 'value="$(literal)"')) == (
        "resume", "session", "--no-alt-screen", "--config", 'value="$(literal)"')
    assert harness_arguments("claude", resume="session", model="chosen") == ("--resume", "session", "--model", "chosen")
    assert harness_arguments("gemini", extra=("--model=chosen",)) == ("--model=chosen",)
    with pytest.raises(AgentDeliveryError, match="presets"):
        harness_arguments("gemini", model="chosen")


def test_environment_entries_preserve_literal_values() -> None:
    entries = (
        "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
        'LITERAL= spaces $(unexpanded) "quotes" = remain ',
        "UNICODE=snowman-☃\nnext-line",
        "EMPTY=",
    )
    assert environment_entries(entries) == entries


@pytest.mark.parametrize(
    "entry",
    (
        "MISSING_EQUALS",
        "=value",
        "9START=value",
        "BAD-NAME=value",
        "BAD\0NAME=value",
        "GOOD=bad\0value",
    ),
)
def test_environment_entries_reject_invalid_or_nul_names_and_values(entry: str) -> None:
    with pytest.raises(AgentDeliveryError, match="environment"):
        environment_entries((entry,))


def test_start_sets_environment_only_on_the_created_tab(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    entries = (
        "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
        "LITERAL=a b=$(unexpanded)=tail",
    )
    status = manager.start("worker", cwd=str(tmp_path), environment=entries)
    assert fake.environments == [entries]
    assert "environment" not in status
    assert all(value not in json.dumps(status) for value in entries)
    saved = json.loads((tmp_path / "registry/worker/agent.json").read_text())
    assert "environment" not in saved
    assert all(value not in json.dumps(saved) for value in entries)


def test_invalid_environment_is_refused_before_registry_or_tab_creation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    with pytest.raises(AgentDeliveryError, match="environment variable name"):
        manager.start("worker", cwd=str(tmp_path), environment=("BAD-NAME=value",))
    assert not (tmp_path / "registry").exists()
    assert fake.environments == []


def test_interactive_muse_refuses_raw_duplicate_structured_options_before_allocation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    with pytest.raises(AgentDeliveryError, match="model field"):
        manager.start(
            "worker", cwd=str(tmp_path), harness="muse", model="structured",
            harness_args=("--model", "raw"),
        )
    with pytest.raises(AgentDeliveryError, match="reasoning effort"):
        manager.start(
            "worker", cwd=str(tmp_path), harness="muse", reasoning_effort="ultra",
            harness_args=("--reasoning-effort=low",),
        )
    assert not manager.registry.exists()
    assert fake.launched == []
    assert fake.environments == []


def test_interactive_start_rejects_string_harness_arguments_before_allocation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    with pytest.raises(AgentDeliveryError, match="harness arguments must be a sequence"):
        manager.start(
            "worker", cwd=str(tmp_path), harness="muse", model="structured",
            harness_args="--model=raw",
        )
    assert not manager.registry.exists()
    assert fake.launched == []
    assert fake.environments == []


def test_direct_start_preserves_unstructured_raw_policy_and_nonoverriding_codex_config(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start(
        "claude-worker", cwd=str(tmp_path), harness="claude",
        harness_args=("--effort=high",),
    )
    manager.start(
        "codex-worker", cwd=str(tmp_path), harness="codex", model="structured",
        harness_args=("-c", "sandbox_mode=read-only"),
    )
    assert fake.launched[0][3][0] == "--session-id"
    assert fake.launched[0][3][2:] == ("--effort=high",)
    assert fake.launched[1][3] == (
        "--no-alt-screen", "--model", "structured", "-c", "sandbox_mode=read-only",
    )


def test_direct_start_rejects_quoted_config_and_opaque_profile_conflicts_before_allocation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    with pytest.raises(AgentDeliveryError, match="model field"):
        manager.start(
            "worker", cwd=str(tmp_path), model="structured",
            harness_args=('--config="model"="raw"',),
        )
    with pytest.raises(AgentDeliveryError, match="reasoning effort"):
        manager.start(
            "worker", cwd=str(tmp_path), reasoning_effort="ultra",
            harness_args=('-c"model_reasoning_effort"="low"',),
        )
    with pytest.raises(AgentDeliveryError, match="raw Codex profile"):
        manager.start(
            "worker", cwd=str(tmp_path), model="structured",
            harness_args=("--profile", "attacker"),
        )
    with pytest.raises(AgentDeliveryError, match="raw Codex profile"):
        manager.start(
            "worker", cwd=str(tmp_path), reasoning_effort="ultra",
            harness_args=("-pattacker",),
        )
    for arguments in (
        ("-c", "profile=attacker"),
        ("--config=profile=attacker",),
        ('-c"profile"="attacker"',),
    ):
        with pytest.raises(AgentDeliveryError, match="raw Codex profile"):
            manager.start(
                "worker", cwd=str(tmp_path), model="structured",
                harness_args=arguments,
            )
    assert not manager.registry.exists()
    assert fake.launched == []
    assert fake.environments == []


@pytest.mark.parametrize("missing", ["terminal", "process"])
def test_suggestion13_muse_adoption_requires_terminal_and_process_anchors(
    missing: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.workspace = "w1"
    fake.create_tab(workspace_id="w1", label="foreign", cwd=str(tmp_path))
    fake.infos["w1:p1"] = AgentPaneInfo(
        "w1:p1", "w1", str(tmp_path), "muse", "idle", None, None,
        terminal_id=None if missing == "terminal" else "term-1", tab_id="w1:t1",
    )
    if missing == "process":
        monkeypatch.setattr(fake, "harness_identity", lambda _pane, _kind: None)
    with pytest.raises(AgentDeliveryError, match="terminal identity|pin.*foreground process"):
        manager.adopt(
            "worker", pane_id="w1:p1", expected_workspace="subagents",
            expected_cwd=str(tmp_path), harness="muse",
        )
    assert not (manager.registry / "worker").exists()
    assert fake.launched == [] and fake.closed == [] and fake.submitted == []
    assert fake.labels == {"w1:t1": "foreign"}


def _suggestion13_codex_hook(fake: FakeManagedClient, monkeypatch: pytest.MonkeyPatch) -> None:
    def ready(info: AgentPaneInfo) -> bool:
        return HerdrClient.codex_idle_ready(cast(HerdrClient, fake), info)

    monkeypatch.setattr(fake, "codex_idle_ready", ready, raising=False)


def test_suggestion13_unknown_codex_busy_prompt_drains_only_after_verified_idle_composer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="unknown")
    screen = "›\n  esc to interrupt\n  GPT default · /work\n"
    monkeypatch.setattr(fake, "read_screen", lambda _pane: screen, raising=False)
    _suggestion13_codex_hook(fake, monkeypatch)
    with pytest.raises(AgentPending) as pending:
        manager.send("worker", "queued turn", ready_timeout=0)
    assert Path(pending.value.artifact).is_file() and fake.submitted == []
    assert manager.status("worker")["agent_status"] == "unknown"
    screen = "› \x1b[2mAsk Codex to do anything\x1b[0m\n  GPT default · /work\n"
    drained = manager.drain("worker", ready_timeout=0)
    assert drained.delivered == (pending.value.message_id,)
    assert fake.submitted == ["queued turn"]
    assert manager.status("worker")["agent_status"] == "unknown"


@pytest.mark.parametrize("failure", ["draft", "unanchored", "replacement", "during-screen", "no-hook"])
def test_suggestion13_unknown_codex_readiness_cannot_relax_saved_recipient_proof(
    failure: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    info = fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="unknown")
    screen = "›\n  GPT default · /work\n"
    if failure == "draft":
        screen = "› human draft\n  GPT default · /work\n"
    elif failure == "unanchored":
        record = manager.get("worker")
        record.session_agent = record.session_value = None
        record.harness_identity = None
        record.anchor_rule = None
        manager._save(record)
        fake.infos[info.pane_id] = replace(info, session_agent=None, session_value=None)
    elif failure == "replacement":
        fake.harness_pids[info.pane_id] += 1

    def read_screen(_pane: str) -> str:
        if failure == "during-screen":
            fake.harness_pids[info.pane_id] += 1
        return screen

    monkeypatch.setattr(fake, "read_screen", read_screen, raising=False)
    if failure != "no-hook":
        _suggestion13_codex_hook(fake, monkeypatch)
    with pytest.raises(AgentDeliveryError):
        manager.send("worker", "must remain pending", ready_timeout=0)
    assert fake.submitted == [] and fake.keys_sent == []


@pytest.mark.parametrize("status", ["idle", "done"])
def test_suggestion13_native_ready_status_keeps_existing_behavior_without_screen_hook(
    status: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status=status)

    def unexpected(_info: AgentPaneInfo) -> bool:
        raise AssertionError("native readiness must not call compatibility hook")

    monkeypatch.setattr(fake, "codex_idle_ready", unexpected, raising=False)
    assert manager.send("worker", "native ready", ready_timeout=0).delivered
    assert fake.submitted == ["native ready"]


def test_failed_launch_status_does_not_retain_environment_values(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    secret = "ACCESS_TOKEN=literal-sensitive-value"
    fake.fail_start = True
    with pytest.raises(AgentDeliveryError, match="trust prompt"):
        manager.start("worker", cwd=str(tmp_path), environment=(secret,))
    record = manager.get("worker")
    assert record.lifecycle == "launch_failed"
    assert record.error == (
        "launch failed with caller-supplied environment; details omitted from status"
    )
    assert secret not in json.dumps(manager.status("worker"))


def test_failed_launch_is_inspectable_and_can_be_archived(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.fail_start = True
    with pytest.raises(AgentDeliveryError, match="record and any created tab retained"):
        manager.start("failed", cwd=str(tmp_path))
    assert manager.get("failed").lifecycle == "launch_failed"
    assert "trust prompt" in str(manager.get("failed").error)
    stopped = manager.stop("failed")
    assert fake.closed == ["w1:t1"]
    assert (Path(str(stopped["archive"])) / "output.json").exists()


def test_failed_launch_cleanup_refuses_replacement_harness(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.fail_start = True
    with pytest.raises(AgentDeliveryError):
        manager.start("failed", cwd=str(tmp_path))
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], agent="codex", status="idle")
    monkeypatch.setattr(fake, "agent_pane", lambda _name: "w1:p-other")
    with pytest.raises(HerdrUnavailable, match="no longer owns"):
        manager.stop("failed")
    assert fake.closed == []


def test_failed_muse_launch_with_our_stale_pane_report_is_stoppable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.custom_dies_after_report = True
    with pytest.raises(AgentDeliveryError, match="foreground process"):
        manager.start("failed", cwd=str(tmp_path), harness="muse")
    failed = manager.get("failed")
    assert failed.lifecycle == "launch_failed"
    assert failed.pane_reported_by_agentctl is True
    assert manager.stop("failed")["pane_closed"] is True


def test_muse_process_identity_is_saved_before_trust_failure_and_permits_stop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.custom_fails_after_observation = True

    with pytest.raises(AgentDeliveryError, match="trust prompt"):
        manager.start("failed", cwd=str(tmp_path), harness="muse")

    failed = manager.get("failed")
    assert failed.lifecycle == "launch_failed"
    assert failed.pane_reported_by_agentctl is False
    assert failed.custom_process_identity == fake.custom_identity
    assert manager.stop("failed")["pane_closed"] is True


def test_unreported_starting_muse_without_identity_is_not_assumed_to_own_idle_shell(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="muse")
    path = manager.registry / "worker" / "agent.json"
    document = json.loads(path.read_text(encoding="utf-8"))
    document["lifecycle"] = "starting"
    document["pane_reported_by_agentctl"] = False
    document["custom_process_identity"] = None
    path.write_text(json.dumps(document), encoding="utf-8")
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], agent=None)

    with pytest.raises(HerdrUnavailable, match="cannot prove starting"):
        manager.stop("worker")
    assert fake.closed == []
    assert path.exists()


def test_failed_muse_launch_cleanup_refuses_a_replacement_report(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.custom_dies_after_report = True
    with pytest.raises(AgentDeliveryError):
        manager.start("failed", cwd=str(tmp_path), harness="muse")
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], agent="codex")
    with pytest.raises(HerdrUnavailable, match="foreground process"):
        manager.stop("failed")
    assert fake.closed == []


def test_failed_muse_launch_cleanup_refuses_non_shell_foreground_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.custom_dies_after_report = True
    with pytest.raises(AgentDeliveryError):
        manager.start("failed", cwd=str(tmp_path), harness="muse")
    fake.custom_at_idle_shell = False
    with pytest.raises(HerdrUnavailable, match="foreground process"):
        manager.stop("failed")
    assert fake.closed == []


def test_busy_delivery_stays_pending_and_can_drain(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="working")
    with pytest.raises(AgentPending):
        manager.send("worker", "next turn", ready_timeout=0)
    assert fake.submitted == []
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="done")
    assert len(manager.drain("worker").delivered) == 1
    assert fake.submitted == ["next turn"]


def test_verified_muse_yolo_composer_reports_idle_staged_and_paused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="muse")
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="done")
    divider = "─" * 40
    footer = "kiki · xhigh · /work · YOLO\n"
    screen = f"old transcript\n{divider}\n❯\n{divider}\n{footer}"
    monkeypatch.setattr(fake, "read", lambda *_args, **_kwargs: screen)
    assert manager.status("worker")["agent_status"] == "idle"

    screen = screen.replace("❯\n", "❯ queued owner prompt\n")
    assert manager.status("worker")["agent_status"] == "staged"

    screen = screen.replace(
        "❯ queued owner prompt\n", "❯\n",
    ).replace(footer, f"Goal (paused)\n{footer}")
    assert manager.status("worker")["agent_status"] == "paused"


def test_yolo_composer_status_is_readable_but_input_requires_process_guard(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="muse")
    record = manager.get("worker")
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="done")
    divider = "─" * 40
    screen = (
        f"Muse Code 1.4.0\nprior Auto-review note\n{divider}\n❯\n{divider}\n"
        "kiki · xhigh · /work · YOLO\n"
    )
    monkeypatch.setattr(fake, "read", lambda *_args, **_kwargs: screen)
    client = _WorkspaceClient(cast(HerdrClient, fake), record)
    assert client.pane_info("w1:p1").status == "idle"
    with pytest.raises(HerdrUnavailable, match="effect-coupled process-generation"):
        client.prompt_agent("w1:p1", "must remain pending")
    assert fake.submitted == []


def test_session_replacement_and_extra_panes_refuse_mutation(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    original = fake.infos["w1:p1"]
    fake.infos["w1:p1"] = replace(original, session_value="another-session")
    with pytest.raises(AgentDeliveryError, match="exactly one live pane"):
        manager.send("worker", "must not reach replacement")
    fake.infos["w1:p1"] = original
    fake.presentations.append(Pane("w1:p2", "w1:t1", "w1"))
    with pytest.raises(AgentDeliveryError, match="ownership changed"):
        manager.stop("worker")
    assert fake.submitted == fake.closed == []


def test_unreachable_server_preserves_state_and_stop_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.offline = True
    row = manager.list()[0]
    assert row["agent_status"] == "unknown"
    assert "server unavailable" in str(row["probe_error"])
    with pytest.raises(HerdrUnavailable):
        manager.stop("worker")
    assert manager.get("worker").lifecycle == "running"


def test_stop_preserves_queue_and_output_and_allows_name_reuse(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    original = manager.start("worker", cwd=str(tmp_path))
    manager.send("worker", "literal\nmultiline")
    assert manager.read("worker") == "human and coordinator transcript\n"
    result = manager.stop("worker")
    archive = Path(str(result["archive"]))
    assert (archive / "agent.json").exists()
    assert list((archive / "queue" / "processed").iterdir())
    assert "human" in (archive / "output.json").read_text()
    assert fake.workspace == "w1"
    assert manager.list() == []
    fresh = manager.start("worker", cwd=str(tmp_path))
    assert fresh["token"] != original["token"]
    assert fresh["pane_id"] != original["pane_id"]


def test_confirmed_missing_pane_can_be_archived(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.presentations.clear()
    assert manager.stop("worker")["tab_closed"] is False
    assert fake.closed == []


def test_goal_is_native_submission_with_honest_requested_metadata(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    result = manager.goal("worker", "finish the task")
    assert fake.submitted == ["/goal finish the task"]
    assert result["native_status"] == "active"
    assert result["source"] == "native"
    assert manager.goal("worker")["goal"] == "finish the task"
    with pytest.raises(AgentDeliveryError, match="single line"):
        manager.goal("worker", "first\nsecond")


def test_wait_reports_readiness_and_blocked_prompt(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    assert manager.wait("worker", timeout=0)["agent_status"] == "idle"
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="working")
    with pytest.raises(AgentDeliveryError, match="timed out"):
        manager.wait("worker", timeout=0)
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="blocked")
    with pytest.raises(AgentDeliveryError, match="requires attention"):
        manager.wait("worker")



def test_wait_does_not_take_idle_just_after_a_delivery_as_readiness(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    manager.send("worker", "do the task")
    processed = list((manager.registry / "worker" / "queue" / "processed").glob("*.json"))
    delivered_at = float(json.loads(processed[0].read_text())["confirmed_at"])
    # The prompt left the composer, but the terminal server still reads idle.
    with pytest.raises(AgentDeliveryError, match="has not been seen working"):
        manager.wait("worker", timeout=0, wall=lambda: delivered_at + 1)
    # Once this wait sees the turn start, the next idle is readiness.
    states = iter(("idle", "working", "idle"))
    clock = [0.0]

    def step(seconds: float) -> None:
        clock[0] += seconds
        fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status=next(states))

    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="idle")
    result = manager.wait(
        "worker", timeout=5, sleep=step, monotonic=lambda: clock[0], wall=lambda: delivered_at + 1
    )
    assert result["agent_status"] == "idle"
    assert clock[0] > 0
    # Long after the delivery, idle is readiness even if this wait never saw the turn.
    assert manager.wait("worker", timeout=0, wall=lambda: delivered_at + 60)["agent_status"] == "idle"

def test_private_state_and_malformed_record_are_rejected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    record = manager.registry / "worker" / "agent.json"
    document = json.loads(record.read_text())
    document["schema"] = True
    record.write_text(json.dumps(document))
    with pytest.raises(AgentDeliveryError, match="invalid agent record"):
        manager.get("worker")
    record.chmod(0o644)
    with pytest.raises(AgentDeliveryError, match="not private"):
        manager.get("worker")


def test_malformed_custom_process_identities_are_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path), harness="muse")
    path = manager.registry / "worker" / "agent.json"
    original = json.loads(path.read_text(encoding="utf-8"))
    identity = original["custom_process_identity"]
    assert isinstance(identity, dict)

    variants: list[dict[str, object]] = []
    for field, value in (
        ("version", True),
        ("pid", True),
        ("pid", 2_147_483_648),
        ("starttime_ticks", 1 << 64),
        ("executable_device", 0),
        ("executable_device", 1 << 64),
        ("executable_inode", 1 << 64),
        ("boot_id", "NOT-A-BOOT-ID"),
    ):
        variant = json.loads(json.dumps(original))
        variant["custom_process_identity"][field] = value
        variants.append(variant)
    missing = json.loads(json.dumps(original))
    del missing["custom_process_identity"]["starttime_ticks"]
    variants.append(missing)
    unknown = json.loads(json.dumps(original))
    unknown["custom_process_identity"]["unexpected"] = 1
    variants.append(unknown)
    for record_field, record_value in (
        ("adapter", "herdr"),
        ("harness", "codex"),
        ("pane_id", None),
    ):
        variant = json.loads(json.dumps(original))
        variant[record_field] = record_value
        variants.append(variant)

    for document in variants:
        path.write_text(json.dumps(document), encoding="utf-8")
        with pytest.raises(AgentDeliveryError, match="custom process identity|invalid agent record"):
            manager.get("worker")


@pytest.mark.parametrize("name", ["../outside", "archive", "Has-Capitals", "under_score", "0worker", "a" * 33])
def test_bad_names_do_not_allocate_state(name: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    with pytest.raises(AgentDeliveryError, match="agent name"):
        manager.start(name, cwd=str(tmp_path))
    assert not manager.registry.exists()


def test_cli_managed_start_and_message_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    monkeypatch.setattr(cli, "HerdrClient", lambda **_kwargs: cast(HerdrClient, fake))
    root = ["--registry", str(manager.registry)]
    assert cli.main(["start", "worker", "--cwd", str(tmp_path), *root]) == 0
    capsys.readouterr()
    message = tmp_path / "brief.txt"
    message.write_text("line one\nline two")
    assert cli.main(["send", "--name", "worker", "--file", str(message), *root]) == 0
    assert fake.submitted == ["line one\nline two"]
    assert cli.main(["stop", "worker", *root]) == 0


def test_trust_prompt_is_not_a_composer_even_when_herdr_reports_idle(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    trust = "Quick safety check: Is this a project you created or one you trust?\n❯ No, exit\nYes, I trust this folder"
    monkeypatch.setattr(fake, "read", lambda *_args, **_kwargs: trust)
    with pytest.raises(AgentDeliveryError, match="trust prompt requires human attention"):
        manager.start("worker", cwd=str(tmp_path), harness="claude", brief="must not confirm the menu")
    assert fake.submitted == []
    assert manager.get("worker").lifecycle == "launch_failed"


def test_replaced_named_agent_is_not_retargeted_in_same_pane(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    monkeypatch.setattr(fake, "agent_pane", lambda _name: "w1:p-other")
    with pytest.raises(AgentPending, match="no longer owns"):
        manager.send("worker", "do not send to replacement", ready_timeout=0)
    with pytest.raises(HerdrUnavailable, match="no longer owns"):
        manager.stop("worker")
    assert fake.submitted == fake.closed == []


def test_explicit_session_binding_preserves_existing_queue_authority(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    record = manager.get("worker")
    record.session_agent = record.session_value = None
    record.native_session = None
    manager._save(record)
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], session_agent=None, session_value=None)
    manager.send("worker", "before native binding")
    binding = manager.registry / "worker" / "queue" / "target.json"
    original = binding.read_bytes()
    manager.bind_session("worker", "explicit-session", goal_command=("codex", "app-server", "--stdio"))
    assert binding.read_bytes() == original
    manager.send("worker", "after native binding")
    assert fake.submitted == ["before native binding", "after native binding"]
    with pytest.raises(AgentDeliveryError, match="already bound"):
        manager.bind_session("worker", "different-session")
    assert manager.goal("worker")["native_status"] == "absent"


@pytest.mark.parametrize("correct_objective", [True, False])
def test_goal_confirms_only_its_exact_replacement_menu(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, correct_objective: bool) -> None:
    from agentctl.errors import AgentPossiblySubmitted
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    keys: list[str] = []
    def wait(_pane: str, _status: str, _timeout: int) -> None:
        if not keys:
            raise HerdrUnavailable("working transition not observed")
    monkeypatch.setattr(fake, "wait_agent_status", wait)
    monkeypatch.setattr(fake, "send_keys", lambda _pane, key: keys.append(key))
    objective = "finish this task" if correct_objective else "a different task"
    screen = f"Replace goal?\nNew objective: {objective}\n› 1. Replace current goal  Set the new objective and start it now\n2. Cancel  Keep the current goal\nPress enter to confirm or esc to go back"
    monkeypatch.setattr(fake, "read", lambda *_args, **_kwargs: screen)
    if correct_objective:
        assert manager.goal("worker", "finish this task")["native_status"] == "active"
        assert keys == ["Enter"]
    else:
        with pytest.raises(AgentPossiblySubmitted):
            manager.goal("worker", "finish this task")
        assert keys == []
    assert fake.submitted == ["/goal finish this task"]


def test_queued_goals_keep_confirmation_authority_across_restart(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from agentctl.errors import AgentPossiblySubmitted
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="working")
    for objective in ("first objective", "second objective"):
        with pytest.raises(AgentPending):
            manager.goal("worker", objective, ready_timeout=0)
    assert fake.submitted == []
    restarted = ManagedAgents(cast(HerdrClient, fake), manager.registry)
    fake.infos["w1:p1"] = replace(fake.infos["w1:p1"], status="idle")
    confirmations: list[str] = []
    def wait(_pane: str, _status: str, _timeout: int) -> None:
        if len(confirmations) < len(fake.submitted):
            raise HerdrUnavailable("working transition not observed")
    def screen(*_args: object, **_kwargs: object) -> str:
        objective = fake.submitted[-1][6:]
        return f"Replace goal?\nNew objective: {objective}\n› 1. Replace current goal  Set the new objective and start it now\n2. Cancel  Keep the current goal\nPress enter to confirm or esc to go back"
    monkeypatch.setattr(fake, "wait_agent_status", wait)
    monkeypatch.setattr(fake, "send_keys", lambda _pane, key: confirmations.append(key))
    monkeypatch.setattr(fake, "read", screen)
    assert len(restarted.drain("worker").delivered) == 2
    assert fake.submitted == ["/goal first objective", "/goal second objective"]
    assert confirmations == ["Enter", "Enter"]
    assert restarted.goal("worker")["delivery"] == "delivered"
    assert restarted.status("worker")["goal_delivery"] == "delivered"
    # Matching text alone does not authorize confirmation for an ordinary send.
    with pytest.raises(AgentPossiblySubmitted):
        restarted.send("worker", "/goal second objective")
    assert confirmations == ["Enter", "Enter"]


@pytest.mark.parametrize("field,value", [("goal_message_id", "../../outside"), ("goal_messages", {"../outside": "task"})])
def test_goal_operation_identifiers_cannot_escape_queue(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, field: str, value: object) -> None:
    manager, _fake = setup(tmp_path, monkeypatch)
    manager.start("worker", cwd=str(tmp_path))
    path = manager.registry / "worker" / "agent.json"
    record = json.loads(path.read_text())
    record[field] = value
    path.write_text(json.dumps(record))
    with pytest.raises(AgentDeliveryError, match="invalid goal message"):
        manager.goal("worker")
