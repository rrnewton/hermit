"""Recipient identity around every input effect, anchors, rename transactions, and doctor."""
from __future__ import annotations

import json
import math
import time
from collections.abc import Callable
from dataclasses import replace
from pathlib import Path
from typing import cast

import pytest

import agentctl.subagents as subagents
from agentctl.client import AgentPaneInfo, CustomProcessIdentity, HerdrClient, Pane, ProcessInfo
from agentctl.errors import AgentDeliveryError, AgentPending, AgentPossiblySubmitted, HerdrUnavailable
from agentctl.subagents import ManagedAgents

from .test_herdr_subagents import FakeManagedClient, setup

#: A known, documented limitation of stage 1 (readback-gaps issue, Gap 3): a peer that already
#: shows the prompt and whose count did not rise cannot exclude a new copy, so the send is
#: quarantined (fails safe) until Herdr gives causal evidence. These tests keep the delivery
#: they require; strict, so each fails loudly once it starts passing.
KNOWN_LIMITATION_GAP3 = pytest.mark.xfail(
    strict=True, raises=AgentPossiblySubmitted,
    reason="readback-gaps Gap 3: equal counts in a peer that already shows the prompt quarantine",
)


def _started(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, name: str = "worker",
) -> tuple[ManagedAgents, FakeManagedClient, str]:
    manager, fake = setup(tmp_path, monkeypatch)
    manager.start(name, cwd=str(tmp_path), harness="claude")
    record = manager.get(name)
    assert record.pane_id is not None
    return manager, fake, record.pane_id


def _rows(report: dict[str, object]) -> list[dict[str, object]]:
    return cast(list[dict[str, object]], report["records"])


def _findings(report: dict[str, object]) -> dict[str, list[str]]:
    return {str(row["name"]): cast(list[str], row["findings"]) for row in _rows(report)}


def _failed_documents(manager: ManagedAgents, name: str) -> list[dict[str, object]]:
    failed = Path(manager._queue(name)) / "failed"
    return [json.loads(path.read_text(encoding="utf-8"))
            for path in sorted(failed.glob("*.json"))]


# Anchors recorded by start.

def test_start_pins_terminal_and_harness_process(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    record = manager.get("worker")
    assert record.terminal_id == fake.infos[pane].terminal_id
    assert record.harness_identity is not None
    assert record.harness_identity.pid == fake.harness_pids[pane]


# The guard before and after each effect.

def test_harness_replaced_before_send_types_nothing_and_stays_pending(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    fake.harness_pids[pane] = 999  # another program now runs in the same terminal
    with pytest.raises(AgentPending, match="no longer a foreground process"):
        manager.send("worker", "for the original agent")
    assert "for the original agent" not in fake.submitted


def _misroutes(manager: ManagedAgents, name: str) -> list[dict[str, object]]:
    path = manager._directory(name) / "misroutes.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def test_pane_swap_between_check_and_send_interrupts_the_wrong_agent_and_retries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    original = fake.harness_pids[pane]

    def swap_once(effect_pane: str) -> None:
        # Another program takes the pane after the last check, before Herdr writes.
        fake.before_effect = None
        fake.harness_pids[effect_pane] = 999

    fake.before_effect = swap_once
    with pytest.raises(AgentPending):
        manager.send("worker", "work meant for the worker agent")
    assert fake.keys_sent == [(pane, "esc")]
    assert fake.submitted[-2:] == ["work meant for the worker agent", subagents.MISROUTE_NOTE]
    [entry] = _misroutes(manager, "worker")
    assert entry["detection"] == "identity-changed-after-write"
    assert entry["interrupted"] is True and entry["note_sent"] is True
    assert entry["observed_pane"] == pane and entry["message_id"] is not None
    inbox = sorted((Path(manager._queue("worker")) / "inbox").glob("*.json"))
    [pending] = [json.loads(path.read_text(encoding="utf-8")) for path in inbox]
    assert len(pending["misroutes"]) == 1 and "probable_misroute" not in pending
    fake.harness_pids[pane] = original  # the intended agent is back in its pane
    manager.drain("worker")
    assert fake.submitted[-1] == "work meant for the worker agent"


def test_misroute_into_a_shell_types_nothing_more(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)

    def harness_exits(effect_pane: str) -> None:
        fake.before_effect = None
        fake.infos[effect_pane] = replace(fake.infos[effect_pane], agent=None)

    fake.before_effect = harness_exits
    with pytest.raises(AgentPossiblySubmitted, match="not proven countermanded"):
        manager.send("worker", "nothing should follow this text")
    assert fake.keys_sent == [] and subagents.MISROUTE_NOTE not in fake.submitted
    [entry] = _misroutes(manager, "worker")
    assert entry["interrupted"] is False and "nothing typed" in str(entry["skipped"])
    # Not countermanded, so not retried: quarantined for a human to look at.
    assert _failed_documents(manager, "worker")[0]["probable_misroute"] is True


@KNOWN_LIMITATION_GAP3
def test_prompt_found_in_another_agents_pane_interrupts_that_agent_and_retries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    fake.redirect_once[pane] = other  # Herdr writes this one prompt to the wrong pane
    manager.send("worker", "a long enough instruction for the worker only")
    assert (other, "esc") in fake.keys_sent
    assert fake.transcripts[other][-1] == subagents.MISROUTE_NOTE
    assert fake.transcripts[pane][-1] == "a long enough instruction for the worker only"
    [entry] = _misroutes(manager, "worker")
    assert entry["detection"] == "prompt-in-another-pane" and entry["observed_pane"] == other


def test_second_misroute_of_one_message_is_quarantined(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)

    def redirect_every_time(effect_pane: str) -> None:
        if effect_pane == pane:
            fake.redirect_once[pane] = other

    fake.before_effect = redirect_every_time
    with pytest.raises(AgentPossiblySubmitted):
        manager.send("worker", "a long enough instruction that keeps going astray")
    documents = _failed_documents(manager, "worker")
    assert documents[0]["probable_misroute"] is True and len(cast(list[object], documents[0]["misroutes"])) == 2
    assert len(_misroutes(manager, "worker")) == 2


def test_label_drift_refuses_input(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch)
    record = manager.get("worker")
    assert record.tab_id is not None
    fake.labels[record.tab_id] = "someone-else"
    with pytest.raises(AgentPending, match="tab label is 'someone-else'"):
        manager.send("worker", "hello")


def test_terminal_change_refuses_input(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    fake.infos[pane] = replace(fake.infos[pane], terminal_id="term-restarted")
    with pytest.raises(AgentPending, match="terminal is 'term-restarted'"):
        manager.send("worker", "hello")


def test_expect_capability_is_used_and_herdr_refusal_types_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    record = manager.get("worker")
    fake.expect_supported = True
    manager.send("worker", "first")
    assert fake.expected_terminals[-1] == record.terminal_id

    def swap_terminal(effect_pane: str) -> None:
        fake.infos[effect_pane] = replace(fake.infos[effect_pane], terminal_id="term-other")

    fake.before_effect = swap_terminal
    with pytest.raises(AgentPending, match="expectation_failed"):
        manager.send("worker", "second")
    assert "second" not in fake.submitted
    assert _failed_documents(manager, "worker") == []


def test_without_the_capability_no_expectation_is_sent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch)
    manager.send("worker", "first")
    assert fake.expected_terminals == [None]


# Legacy records and explicit anchoring.

def _strip_anchors(manager: ManagedAgents, fake: FakeManagedClient, name: str) -> None:
    record = manager.get(name)
    record.terminal_id = None
    record.harness_identity = None
    record.session_agent = record.session_value = None
    manager._save(record)
    fake.infos[record.pane_id or ""] = replace(
        fake.infos[record.pane_id or ""], session_agent=None, session_value=None,
    )


def test_unanchored_record_refuses_input_until_anchored(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    _strip_anchors(manager, fake, "worker")
    with pytest.raises(AgentPending, match="agentctl anchor worker"):
        manager.send("worker", "queued until anchored")
    result = manager.anchor("worker")
    assert result["harness_pid"] == fake.harness_pids[pane] and result["replaced"] is False
    manager.drain("worker")
    assert fake.submitted[-1] == "queued until anchored"


def test_anchor_refuses_a_changed_harness_without_replace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    fake.harness_pids[pane] = 999
    with pytest.raises(AgentDeliveryError, match="rerun with --replace"):
        manager.anchor("worker")
    assert manager.anchor("worker", replace=True)["replaced"] is True
    manager.send("worker", "after explicit re-anchor")


def test_duplicate_claim_is_refused_at_anchor(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("other", cwd=str(tmp_path), harness="claude")
    other = manager.get("other")
    other.pane_id = pane
    other.terminal_id = None
    manager._save(other)
    with pytest.raises(AgentDeliveryError, match="already registered as"):
        manager.anchor("worker", replace=True)


# Rename.

def test_rename_moves_registry_name_herdr_name_and_label_together(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch, "old")
    tab = manager.get("old").tab_id or ""
    result = manager.rename("old", "new")
    assert result["herdr_steps"] == "done" and result["recovered"] is False
    assert not manager._directory("old").exists()
    record = manager.get("new")
    assert record.pane_id == pane
    assert [entry["name"] for entry in record.name_history] == ["old"]
    assert fake.agent_names()["new"] == pane and "old" not in fake.agent_names()
    assert fake.labels[tab] == "new"
    assert not list((manager.registry / ".renames").glob("*.json"))
    manager.send("new", "work under the new name")
    assert fake.submitted[-1] == "work under the new name"
    with pytest.raises(AgentDeliveryError, match="unknown agent"):
        manager.send("old", "nobody")


@pytest.mark.parametrize("prepare,message", [
    ("registered", "already registered"),
    ("herdr-name", "already named"),
    ("label", "already labelled"),
    ("unanchored", "agentctl anchor old"),
])
def test_rename_preconditions_refuse_before_any_change(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, prepare: str, message: str,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch, "old")
    tab = manager.get("old").tab_id or ""
    if prepare == "registered":
        manager.start("new", cwd=str(tmp_path), harness="claude")
    elif prepare == "herdr-name":
        fake.launched.append(("new", "claude", pane, ()))
        fake.launched.append(("old", "claude", pane, ()))
    elif prepare == "label":
        fake.presentations.append(Pane("w1:p99", "w1:t99", "w1"))
        fake.infos["w1:p99"] = AgentPaneInfo(
            "w1:p99", "w1", str(tmp_path), None, "unknown", None, None,
            terminal_id="term-99", tab_id="w1:t99",
        )
        fake.labels["w1:t99"] = "new"
    else:
        _strip_anchors(manager, fake, "old")
    with pytest.raises(AgentDeliveryError, match=message):
        manager.rename("old", "new")
    assert manager._directory("old").exists()
    assert fake.labels[tab] == "old"
    assert not (manager.registry / ".renames").exists() or not list(
        (manager.registry / ".renames").glob("*.json"))


def test_crash_after_journal_blocks_both_names_and_rerun_completes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch, "old")
    real = fake.rename_tab

    def crash(tab_id: str, label: str) -> None:
        raise HerdrUnavailable("simulated crash inside rename")

    monkeypatch.setattr(fake, "rename_tab", crash)
    with pytest.raises(HerdrUnavailable, match="simulated crash"):
        manager.rename("old", "new")
    assert len(list((manager.registry / ".renames").glob("*.json"))) == 1
    for name in ("old", "new"):
        with pytest.raises(AgentDeliveryError, match="rerun `agentctl rename old new`"):
            manager.send(name, "blocked")
    with pytest.raises(AgentDeliveryError, match="incomplete"):
        manager.start("new", cwd=str(tmp_path), harness="claude")
    with pytest.raises(AgentDeliveryError, match="rerun exactly"):
        manager.rename("old", "other")
    monkeypatch.setattr(fake, "rename_tab", real)
    result = manager.rename("old", "new")
    assert result["recovered"] is True
    assert [entry["name"] for entry in manager.get("new").name_history] == ["old"]
    assert manager.get("new").pane_id == pane


def test_crash_between_publication_and_move_recovers_with_one_history_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake, _pane = _started(tmp_path, monkeypatch, "old")
    real = subagents._rename_directory_noreplace_at
    calls = {"count": 0}

    def crash_once(*arguments: object) -> None:
        calls["count"] += 1
        if calls["count"] == 1:
            raise AgentDeliveryError("simulated crash before the directory move")
        real(*arguments)  # type: ignore[arg-type]

    monkeypatch.setattr(subagents, "_rename_directory_noreplace_at", crash_once)
    with pytest.raises(AgentDeliveryError, match="simulated crash"):
        manager.rename("old", "new")
    on_disk = json.loads((manager._directory("old") / "agent.json").read_text(encoding="utf-8"))
    assert on_disk["name"] == "new"  # content published inside OLD, not moved
    manager.rename("old", "new")
    history = manager.get("new").name_history
    assert [entry["name"] for entry in history] == ["old"]


def test_recovery_after_the_harness_exits_finishes_the_registry_and_leaves_the_pane_alone(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch, "old")
    tab = manager.get("old").tab_id or ""
    real = fake.rename_tab
    monkeypatch.setattr(fake, "rename_tab", lambda tab, label: (_ for _ in ()).throw(
        HerdrUnavailable("simulated crash")))
    with pytest.raises(HerdrUnavailable):
        manager.rename("old", "new")
    monkeypatch.setattr(fake, "rename_tab", real)
    fake.infos[pane] = replace(fake.infos[pane], agent=None)  # back at the shell
    result = manager.rename("old", "new")
    assert str(result["herdr_steps"]).startswith("skipped-recipient-changed")
    assert manager._directory("new").exists() and not manager._directory("old").exists()
    assert fake.labels[tab] == "old"  # the pane's presentation is not changed
    manager.start("old", cwd=str(tmp_path), harness="claude")  # both names usable again


def test_existing_duplicate_claim_refuses_input(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake, pane = _started(tmp_path, monkeypatch)
    manager.start("other", cwd=str(tmp_path), harness="claude")
    other = manager.get("other")
    other.pane_id = pane
    manager._save(other)
    with pytest.raises(AgentPending, match="also claims pane"):
        manager.send("worker", "must not reach a shared pane")


def test_unreadable_record_refuses_input_conservatively(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, _fake, _pane = _started(tmp_path, monkeypatch)
    broken = manager.registry / "broken"
    broken.mkdir(mode=0o700)
    (broken / "agent.json").write_text("{", encoding="utf-8")
    with pytest.raises(AgentPending, match="cannot prove that pane ownership is unique"):
        manager.send("worker", "held until the registry is readable")


def test_session_change_refuses_input_even_with_a_matching_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    assert manager.get("worker").session_value is not None
    fake.infos[pane] = replace(fake.infos[pane], session_value="another-conversation")
    with pytest.raises(AgentPending, match="session"):
        manager.send("worker", "hello")
    assert "session-mismatch" in _findings(manager.doctor())["worker"]


def test_transport_error_after_a_write_checks_the_recipient_and_recovers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    real = fake.agent_prompt
    calls = {"count": 0}

    def accepted_then_lost(pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        calls["count"] += 1
        real(pane_id, text, expect_terminal=expect_terminal)
        if calls["count"] == 1:
            fake.harness_pids[pane_id] = 999
            raise HerdrUnavailable("connection reset after the write")

    monkeypatch.setattr(fake, "agent_prompt", accepted_then_lost)
    with pytest.raises(AgentPending):
        manager.send("worker", "written before the transport failed")
    [entry] = _misroutes(manager, "worker")
    assert "outcome is unknown" in str(entry["detail"])
    assert fake.keys_sent == [(pane, "esc")]


def test_goal_confirmation_misroutes_are_translated_durably() -> None:
    from agentctl import agent as delivery
    from agentctl.errors import MisrouteRecovered, ProbableMisroute

    class Client:
        def __init__(self, error: Exception) -> None:
            self.error = error

        def prompt_agent(self, pane_id: str, text: str) -> None:
            return None

        def wait_agent_status(self, pane_id: str, status: str, timeout_ms: int) -> None:
            raise self.error

    info = AgentPaneInfo("w1:p1", "w1", "/", "codex", "idle", None, None)
    with pytest.raises(delivery._Misrouted):
        delivery._deliver_one(cast(HerdrClient, Client(MisrouteRecovered("x"))), info,
                              "/goal y", working_timeout=1.0)
    with pytest.raises(delivery._PossiblySubmitted) as caught:
        delivery._deliver_one(cast(HerdrClient, Client(ProbableMisroute("x"))), info,
                              "/goal y", working_timeout=1.0)
    assert caught.value.misroute is True


def test_anchor_migrates_a_nested_record_to_schema_1(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    from .test_herdr_subagents import _nested_v2_record

    manager, fake, pane = _started(tmp_path, monkeypatch)
    path = manager.registry / "worker" / "agent.json"
    flat = json.loads(path.read_text(encoding="utf-8"))
    for key in ("terminal_id", "harness_identity", "name_history"):
        flat.pop(key)
    nested = _nested_v2_record(flat)
    nested["session_agent"] = nested["session_value"] = None
    path.write_text(json.dumps(nested), encoding="utf-8")
    fake.infos[pane] = replace(fake.infos[pane], session_agent=None, session_value=None)
    with pytest.raises(AgentPending, match="agentctl anchor worker"):
        manager.send("worker", "queued until anchored")
    result = manager.anchor("worker")
    assert result["migrated_from"] == "agentctl-session/v2"
    stored = json.loads(path.read_text(encoding="utf-8"))
    assert stored["schema"] == 1 and stored["harness_identity"] is not None
    manager.drain("worker")
    assert fake.submitted[-1] == "queued until anchored"


def test_rename_refuses_at_the_history_limit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, _fake, _pane = _started(tmp_path, monkeypatch, "old")
    record = manager.get("old")
    record.name_history = [
        {"name": "earlier", "renamed_at": 1.0, "journal_id": f"{index:032x}"}
        for index in range(256)
    ]
    manager._save(record)
    with pytest.raises(AgentDeliveryError, match="the most a record keeps"):
        manager.rename("old", "new")
    assert manager._directory("old").exists()


def test_recovery_with_the_pane_gone_finishes_the_registry_only(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch, "old")
    real = fake.rename_tab
    monkeypatch.setattr(fake, "rename_tab", lambda tab, label: (_ for _ in ()).throw(
        HerdrUnavailable("simulated crash")))
    with pytest.raises(HerdrUnavailable):
        manager.rename("old", "new")
    monkeypatch.setattr(fake, "rename_tab", real)
    fake.presentations = [entry for entry in fake.presentations if entry.pane_id != pane]
    result = manager.rename("old", "new")
    assert result["herdr_steps"] == "skipped-pane-missing"
    assert manager._directory("new").exists() and not manager._directory("old").exists()


def test_recovery_refuses_when_both_directories_exist(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch, "old")
    monkeypatch.setattr(fake, "rename_tab", lambda tab, label: (_ for _ in ()).throw(
        HerdrUnavailable("simulated crash")))
    with pytest.raises(HerdrUnavailable):
        manager.rename("old", "new")
    manager._directory("new").mkdir(mode=0o700)
    with pytest.raises(AgentDeliveryError, match="both agent directories"):
        manager.rename("old", "new")


def test_adopted_rename_changes_only_the_registry_alias(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = setup(tmp_path, monkeypatch)
    fake.workspace = "w1"
    fake.serial = 40
    fake.presentations.append(Pane("w1:p40", "w1:t40", "w1"))
    fake.labels["w1:t40"] = "human-chosen"
    fake.infos["w1:p40"] = AgentPaneInfo(
        "w1:p40", "w1", str(tmp_path), "claude", "idle", None, None,
        terminal_id="term-40", tab_id="w1:t40",
    )
    manager.adopt("foreign", pane_id="w1:p40", expected_workspace="subagents",
                  expected_cwd=str(tmp_path), harness="claude")
    result = manager.rename("foreign", "renamed")
    assert result["herdr_steps"] == "done"
    assert fake.labels["w1:t40"] == "human-chosen"
    assert manager.get("renamed").adapter == "herdr-foreign"


# Doctor.

def test_doctor_reports_drift_and_repairs_only_presentation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    assert manager.doctor()["clean"] is True
    tab = manager.get("worker").tab_id or ""
    fake.labels[tab] = "retitled-by-hand"
    report = manager.doctor()
    assert report["clean"] is False
    assert _rows(report)[0]["findings"] == ["label-mismatch"]
    assert fake.labels[tab] == "retitled-by-hand"  # read-only by default
    repaired = manager.doctor(repair_labels=True)
    assert _rows(repaired)[0]["repaired"] is True and fake.labels[tab] == "worker"
    fake.labels[tab] = "retitled-by-hand"
    fake.harness_pids[pane] = 999
    report = manager.doctor(repair_labels=True)
    assert set(_findings(report)["worker"]) == {"label-mismatch", "harness-replaced"}
    assert "repaired" not in _rows(report)[0] and fake.labels[tab] == "retitled-by-hand"


def test_doctor_finds_missing_panes_legacy_records_and_unmanaged_tabs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("legacy", cwd=str(tmp_path), harness="claude")
    _strip_anchors(manager, fake, "legacy")
    fake.presentations.append(Pane("w1:p77", "w1:t77", "w1"))
    fake.infos["w1:p77"] = AgentPaneInfo(
        "w1:p77", "w1", str(tmp_path), None, "unknown", None, None,
        terminal_id="term-77", tab_id="w1:t77",
    )
    fake.labels["w1:t77"] = "hand-made"
    fake.presentations = [entry for entry in fake.presentations if entry.pane_id != pane]
    report = manager.doctor()
    findings = _findings(report)
    assert findings["worker"] == ["pane-missing"]
    assert findings["legacy"] == ["unanchored"]
    assert {"finding": "unmanaged-tab", "tab_id": "w1:t77", "label": "hand-made"} in cast(
        list[dict[str, object]], report["workspace"])


def test_doctor_reports_duplicate_claims(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, _fake, pane = _started(tmp_path, monkeypatch)
    manager.start("other", cwd=str(tmp_path), harness="claude")
    other = manager.get("other")
    other.pane_id = pane
    manager._save(other)
    findings = _findings(manager.doctor())
    assert "duplicate-claim" in findings["worker"] and "duplicate-claim" in findings["other"]


def test_doctor_reports_an_incomplete_rename(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch, "old")
    monkeypatch.setattr(fake, "rename_tab", lambda tab, label: (_ for _ in ()).throw(
        HerdrUnavailable("simulated crash")))
    with pytest.raises(HerdrUnavailable):
        manager.rename("old", "new")
    report = manager.doctor()
    assert report["journals"] == [{"old": "old", "new": "new"}]
    assert "rename-incomplete" in _findings(report)["old"]
    assert report["clean"] is False


def test_doctor_reports_closed_workspaces_and_panes_per_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch)
    for name in ("stale", "closing"):
        manager.start(name, cwd=str(tmp_path), harness="claude")
    stale = manager.get("stale")
    stale.workspace_id = "wP"  # a workspace Herdr has since closed
    manager._save(stale)
    closing = manager.get("closing").pane_id or ""
    tab_labels, pane_info = fake.tab_labels, fake.pane_info

    def refuse_closed_workspace(workspace_id: str) -> dict[str, str]:
        if workspace_id == "wP":
            raise HerdrUnavailable('tab list: {"error":{"code":"workspace_not_found",'
                                   '"message":"workspace wP not found"}}')
        return tab_labels(workspace_id)

    def refuse_closed_pane(pane_id: str) -> AgentPaneInfo:
        if pane_id == closing:  # closed after the pane list was read
            raise HerdrUnavailable(f'pane get: {{"error": {{"code": "pane_not_found", '
                                   f'"message": "pane {pane_id} not found"}}}}')
        return pane_info(pane_id)

    monkeypatch.setattr(fake, "tab_labels", refuse_closed_workspace)
    monkeypatch.setattr(fake, "pane_info", refuse_closed_pane)
    report = manager.doctor()
    findings = _findings(report)
    assert findings["stale"] == ["workspace-missing"]
    assert findings["closing"] == ["pane-missing"]
    assert findings["worker"] == []
    assert report["clean"] is False
    # Any other failure is an outage, not a finding: doctor still refuses to guess.
    monkeypatch.setattr(fake, "tab_labels", lambda workspace_id: (_ for _ in ()).throw(
        HerdrUnavailable("tab list: connection refused")))
    with pytest.raises(HerdrUnavailable, match="connection refused"):
        manager.doctor()


def test_harness_identity_refuses_an_ambiguous_foreground(monkeypatch: pytest.MonkeyPatch) -> None:
    def identity(pid: int) -> CustomProcessIdentity:
        return CustomProcessIdentity(
            version=1, boot_id="00000000-0000-0000-0000-000000000000",
            pid=pid, starttime_ticks=pid, executable_device=1, executable_inode=pid,
        )

    # A launcher named like the harness, with the real program as its child.
    foreground = [(200, "bash", "bash /opt/claude", "/bin/bash", None),
                  (201, "node", "node /opt/claude/cli.js", "/usr/bin/node", None)]

    class Client(HerdrClient):
        def process_info(self, pane_id: str) -> ProcessInfo:
            return ProcessInfo(pane_id, 100, 200, tuple(foreground))

    monkeypatch.setattr(HerdrClient, "_process_identity",
                        staticmethod(lambda pid: (identity(pid), 200, "/x")))
    monkeypatch.setattr(HerdrClient, "_process_executable", staticmethod(lambda pid: "/x"))
    client = Client(herdr_bin="herdr")
    assert client.harness_identity("w1:p1", "claude") is None
    # An anchor pinned earlier to the launcher no longer verifies either.
    assert not client.verify_harness_identity("w1:p1", identity(200))
    foreground[:] = [(201, "claude", "claude", "/usr/local/bin/claude", None)]
    pinned = client.harness_identity("w1:p1", "claude")
    assert pinned is not None and pinned.pid == 201
    assert client.verify_harness_identity("w1:p1", pinned)
    foreground[:] = [(202, "claude", "claude", "/usr/local/bin/claude", None)]
    assert not client.verify_harness_identity("w1:p1", pinned)


def test_countermand_stops_when_the_occupant_changes_after_the_interrupt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    real_keys = fake.send_keys

    def swap_harness_on_esc(pane_id: str, keys: str, *, expect_terminal: str | None = None) -> None:
        real_keys(pane_id, keys, expect_terminal=expect_terminal)
        if keys == "esc":
            fake.harness_pids[pane_id] = 777  # yet another program arrives after the Esc

    monkeypatch.setattr(fake, "send_keys", swap_harness_on_esc)

    def swap_once(effect_pane: str) -> None:
        fake.before_effect = None
        fake.harness_pids[effect_pane] = 999

    fake.before_effect = swap_once
    with pytest.raises(AgentPossiblySubmitted, match="note not sent"):
        manager.send("worker", "work meant for the worker agent")
    assert subagents.MISROUTE_NOTE not in fake.submitted
    [entry] = _misroutes(manager, "worker")
    assert entry["interrupted"] is True and entry["note_sent"] is False


@KNOWN_LIMITATION_GAP3
def test_old_matching_text_in_another_pane_is_not_a_misroute(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    text = "an instruction that also appeared in yesterday's transcript"
    fake.transcripts[other] = [text]  # history, not this send
    manager.send("worker", text)
    assert fake.keys_sent == [] and _misroutes(manager, "worker") == []
    assert fake.transcripts[pane][-1] == text


@KNOWN_LIMITATION_GAP3
def test_lost_acknowledgement_after_a_write_elsewhere_is_countermanded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    real = fake.agent_prompt
    calls = {"count": 0}

    def written_elsewhere_then_lost(pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        calls["count"] += 1
        if calls["count"] == 1:
            fake.redirect_once[pane_id] = other
            real(pane_id, text, expect_terminal=expect_terminal)
            raise HerdrUnavailable("acknowledgement lost")
        real(pane_id, text, expect_terminal=expect_terminal)

    monkeypatch.setattr(fake, "agent_prompt", written_elsewhere_then_lost)
    manager.send("worker", "a long enough instruction for the worker only")
    assert (other, "esc") in fake.keys_sent
    assert fake.transcripts[pane][-1] == "a long enough instruction for the worker only"
    [entry] = _misroutes(manager, "worker")
    assert entry["detection"] == "prompt-in-another-pane"


def test_prompt_the_target_never_shows_is_quarantined_as_unproven(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A native prompt (no screen receipt) that never shows: Herdr's working event alone
    # is not proof that this recipient got it.
    manager, fake, pane = _started(tmp_path, monkeypatch)
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    fake.infos["w1:void"] = fake.infos[pane]
    fake.redirect_once[pane] = "w1:void"
    with pytest.raises(AgentPossiblySubmitted, match="delivery is unproven"):
        manager.send("worker", "continue")
    readback = (manager._directory("worker") / "readback.jsonl").read_text(encoding="utf-8")
    assert '"result": "not-seen"' in readback


def test_session_provider_change_refuses_input(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    record = manager.get("worker")
    record.harness_identity = None  # session-anchored only
    manager._save(record)
    fake.infos[pane] = replace(fake.infos[pane], session_agent="codex")
    with pytest.raises(AgentPending):
        manager.send("worker", "hello")
    assert "session-mismatch" in _findings(manager.doctor())["worker"]


def test_old_journal_at_the_history_limit_completes_with_the_newest_names(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch, "old")
    record = manager.get("old")
    record.name_history = [
        {"name": "earlier", "renamed_at": 1.0, "journal_id": f"{index:032x}"}
        for index in range(255)
    ]
    manager._save(record)
    monkeypatch.setattr(fake, "rename_tab", lambda tab, label: (_ for _ in ()).throw(
        HerdrUnavailable("simulated crash")))
    with pytest.raises(HerdrUnavailable):
        manager.rename("old", "new")
    monkeypatch.undo()
    monkeypatch.delenv("HERDR_WORKSPACE_ID", raising=False)
    record_path = manager._directory("old") / "agent.json"
    document = json.loads(record_path.read_text(encoding="utf-8"))
    document["name_history"].append(
        {"name": "earlier", "renamed_at": 1.0, "journal_id": f"{999:032x}"})  # now full
    record_path.write_text(json.dumps(document), encoding="utf-8")
    manager.rename("old", "new")
    history = manager.get("new").name_history
    assert len(history) == 256 and history[-1]["name"] == "old"


def test_note_that_never_shows_in_the_wrong_pane_is_not_a_recovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    manager, fake, pane = _started(tmp_path, monkeypatch)
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    real = fake.agent_prompt

    def note_vanishes(pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        if text == subagents.MISROUTE_NOTE:
            fake.redirect_once[pane_id] = "w1:void"  # the note goes somewhere else
            fake.infos.setdefault("w1:void", fake.infos[pane_id])
        real(pane_id, text, expect_terminal=expect_terminal)

    monkeypatch.setattr(fake, "agent_prompt", note_vanishes)

    def swap_once(effect_pane: str) -> None:
        fake.before_effect = None
        fake.harness_pids[effect_pane] = 999

    fake.before_effect = swap_once
    with pytest.raises(AgentPossiblySubmitted, match="recipient is unproven"):
        manager.send("worker", "work meant for the worker agent")
    [entry] = _misroutes(manager, "worker")
    assert entry["note_sent"] is True and entry["note_confirmed"] is False


def test_countermand_needs_the_wrong_panes_input_lock(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(subagents.COUNTERMAND_ENV, "1")  # opt-in countermand
    from agentctl import agent as delivery

    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    monkeypatch.setattr(subagents, "_COUNTERMAND_LOCK_SECONDS", 0.1)
    fake.redirect_once[pane] = other
    held = delivery._lock_target(other, "another sender")
    try:
        with pytest.raises(AgentPossiblySubmitted, match="held by another sender"):
            manager.send("worker", "a long enough instruction for the worker only")
    finally:
        held.close()
    assert fake.keys_sent == []


def test_window_moving_past_an_old_match_in_a_peer_is_uncertain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    text = "a long enough instruction for the worker only"
    fake.transcripts[other] = [text, "output after the old match"]

    def scroll_peer(effect_pane: str) -> None:
        fake.before_effect = None
        # The old match scrolls out of the peer's window as a new one arrives.
        fake.transcripts[other] = ["output after the old match", text]

    fake.before_effect = scroll_peer
    fake.infos["w1:void"] = fake.infos[pane]
    fake.redirect_once[pane] = "w1:void"
    with pytest.raises(AgentPossiblySubmitted, match="cannot be attributed"):
        manager.send("worker", text)
    assert fake.keys_sent == []


def test_by_default_a_pane_swap_is_quarantined_and_nothing_is_typed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv(subagents.COUNTERMAND_ENV, raising=False)
    manager, fake, _pane = _started(tmp_path, monkeypatch)

    def swap_once(effect_pane: str) -> None:
        fake.before_effect = None
        fake.harness_pids[effect_pane] = 999

    fake.before_effect = swap_once
    with pytest.raises(AgentPossiblySubmitted, match="nothing typed into any pane"):
        manager.send("worker", "work meant for the worker agent")
    assert fake.keys_sent == [] and subagents.MISROUTE_NOTE not in fake.submitted
    [entry] = _misroutes(manager, "worker")
    assert entry["interrupted"] is False and "countermand is off" in str(entry["skipped"])
    assert _failed_documents(manager, "worker")[0]["probable_misroute"] is True


def test_by_default_a_prompt_in_another_pane_is_quarantined_and_nothing_is_typed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv(subagents.COUNTERMAND_ENV, raising=False)
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    fake.redirect_once[pane] = other
    with pytest.raises(AgentPossiblySubmitted, match=f"probably reached pane {other}"):
        manager.send("worker", "a long enough instruction for the worker only")
    assert fake.keys_sent == [] and subagents.MISROUTE_NOTE not in fake.submitted


def test_prompt_new_in_target_and_peer_is_quarantined_without_a_note(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    text = "a long enough instruction for the worker only"

    def echo_in_peer_too(effect_pane: str) -> None:
        fake.before_effect = None
        fake.transcripts.setdefault(other, []).append(text)

    fake.before_effect = echo_in_peer_too
    with pytest.raises(AgentPossiblySubmitted, match="cannot be attributed"):
        manager.send("worker", text)
    assert fake.keys_sent == [] and subagents.MISROUTE_NOTE not in fake.submitted


def test_anchor_from_an_earlier_rule_does_not_count(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    record = manager.get("worker")
    record.anchor_rule = None  # pinned before the only-foreground-process rule
    record.session_agent = record.session_value = None
    manager._save(record)
    fake.infos[pane] = replace(fake.infos[pane], session_agent=None, session_value=None)
    with pytest.raises(AgentPending, match="agentctl anchor worker"):
        manager.send("worker", "held until re-anchored")
    assert manager.anchor("worker")["replaced"] is False
    assert manager.get("worker").anchor_rule == subagents.ANCHOR_RULE


def test_snapshot_failure_before_any_write_leaves_the_message_pending(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    real = fake.read_scrollback
    bystander = manager.get("bystander").pane_id

    def dead_peer(pane_id: str) -> str:
        if pane_id == bystander:
            raise HerdrUnavailable("peer pane is gone")
        return real(pane_id)

    monkeypatch.setattr(fake, "read_scrollback", dead_peer)
    with pytest.raises(AgentPending, match="cannot read scrollback before sending"):
        manager.send("worker", "a long enough instruction for the worker only")
    assert _failed_documents(manager, "worker") == []


def test_peer_pane_herdr_reports_missing_is_skipped_by_the_snapshot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    real = fake.read_scrollback
    bystander = manager.get("bystander").pane_id
    text = "a long enough instruction for the worker only"

    def closed_peer(pane_id: str) -> str:
        if pane_id == bystander:
            raise HerdrUnavailable(
                f'pane read {pane_id}: {{"error":{{"code":"pane_not_found",'
                f'"message":"pane {pane_id} not found"}}}}')
        return real(pane_id)

    monkeypatch.setattr(fake, "read_scrollback", closed_peer)
    manager.send("worker", text)
    assert text in fake.submitted
    assert _failed_documents(manager, "worker") == []
    readback = [json.loads(line) for line in
                (manager._directory("worker") / "readback.jsonl").read_text(encoding="utf-8").splitlines()]
    assert [(entry["result"], entry["evidence"]["peer"], entry["evidence"]["finding"])
            for entry in readback] == [("peer-skipped", bystander, "pane-missing")]


def test_failed_readback_log_on_an_unverified_accept_quarantines(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, pane = _started(tmp_path, monkeypatch)
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    monkeypatch.setattr(subagents._WorkspaceClient, "_append_log", lambda self, name, entry: False)
    fake.infos["w1:void"] = fake.infos[pane]
    fake.redirect_once[pane] = "w1:void"
    with pytest.raises(AgentPossiblySubmitted, match="read-back log could not be written"):
        manager.send("worker", "a long enough instruction for the worker only")


@KNOWN_LIMITATION_GAP3
def test_a_peer_that_keeps_working_after_an_old_match_is_not_uncertain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake, _pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    text = "the same instruction broadcast to several agents"
    fake.transcripts[other] = ["older output", text]  # it got the broadcast earlier

    def peer_works_on(effect_pane: str) -> None:
        fake.before_effect = None
        fake.transcripts[other] = [text, "more of its own output"]

    fake.before_effect = peer_works_on
    manager.send("worker", text)
    assert fake.keys_sent == [] and fake.submitted[-1] == text


def test_full_peer_window_with_an_old_match_is_uncertain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The peer's window is full, so an old match may have scrolled out as a new one came
    # in: equal counts cannot tell those histories apart, even though the target shows it.
    manager, fake, pane = _started(tmp_path, monkeypatch)
    manager.start("bystander", cwd=str(tmp_path), harness="claude")
    other = manager.get("bystander").pane_id or ""
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    text = "a long enough instruction for the worker only"
    filler = [f"line {index}" for index in range(subagents.SCROLLBACK_LINES)]
    fake.transcripts[other] = [*filler[:-1], text]
    with pytest.raises(AgentPossiblySubmitted, match="cannot be attributed"):
        manager.send("worker", text)
    assert fake.keys_sent == [] and fake.transcripts[pane][-1] == text


def test_confirmation_with_a_replaced_harness_is_quarantined(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Herdr confirms the working state, but by then another program holds the pane.
    manager, fake, pane = _started(tmp_path, monkeypatch)

    def working_then_replaced(pane_id: str, status: str, timeout_ms: int) -> None:
        fake.harness_pids[pane_id] = 999

    monkeypatch.setattr(fake, "wait_agent_status", working_then_replaced)
    with pytest.raises(AgentPossiblySubmitted, match="did not confirm"):
        manager.send("worker", "a long enough instruction for the worker only")


def test_relay_submission_needs_evidence_in_the_target(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    from agentctl.submission import SubmissionReceipt

    manager, fake, pane = _started(tmp_path, monkeypatch)
    monkeypatch.setattr(subagents, "READBACK_SECONDS", 0.0)
    record = manager.get("worker")
    record.adapter = "herdr-relay"
    record.custom_process_identity = fake.custom_identity
    monkeypatch.setattr(fake, "relay_process", lambda pane_id: fake.custom_identity, raising=False)
    # The relay's composer reported only an active turn, never the prompt itself.
    monkeypatch.setattr(subagents, "submit_verified", lambda terminal, pane_id, harness, text:
                        SubmissionReceipt("Enter", 1, 0.1, "agent reports an active turn"))
    client = subagents._WorkspaceClient(cast(HerdrClient, fake), record,
                                        queue=manager._queue("worker"))
    monkeypatch.setattr(client, "pane_info", lambda pane_id: replace(fake.infos[pane_id], status="idle"))
    with pytest.raises(AgentDeliveryError, match="delivery is unproven"):
        client.prompt_agent(pane, "a long enough instruction for the relayed worker")


# The first prompt after start, and how a harness prints a prompt.

class _VirtualClock:
    """Monotonic time for read-back waits, advanced by sleeping instead of waiting."""

    def __init__(self) -> None:
        self.now = 1000.0
        self.time, self.time_ns = time.time, time.time_ns

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += max(seconds, 0.0)


def _harness_prints_late(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, after: float | None,
    shown: Callable[[str], str] = lambda text: text,
) -> tuple[ManagedAgents, FakeManagedClient]:
    """Each harness prints a prompt ``after`` seconds once it is submitted (never for
    None), as ``shown`` renders it."""
    manager, fake = setup(tmp_path, monkeypatch)
    clock = _VirtualClock()
    monkeypatch.setattr(subagents, "time", clock)
    printed_at: dict[str, float] = {}
    submit = fake.agent_prompt

    def agent_prompt(pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        submit(pane_id, text, expect_terminal=expect_terminal)
        printed_at[text] = math.inf if after is None else clock.now + after

    def read_scrollback(pane_id: str) -> str:
        assert pane_id in fake.infos
        return "\n".join(shown(text) for text in fake.transcripts.get(pane_id, [])
                         if printed_at.get(text, -math.inf) <= clock.now)

    monkeypatch.setattr(fake, "agent_prompt", agent_prompt)
    monkeypatch.setattr(fake, "read_scrollback", read_scrollback)
    return manager, fake


def test_start_brief_a_new_harness_prints_after_five_seconds_is_delivered(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = _harness_prints_late(tmp_path, monkeypatch, after=6.0)
    brief = "a brief the new harness prints only after it finishes starting"
    assert manager.start("worker", cwd=str(tmp_path), harness="claude", brief=brief)["lifecycle"] == "running"
    assert fake.submitted == [brief] and _failed_documents(manager, "worker") == []


def test_start_brief_a_new_harness_never_prints_is_quarantined_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    manager, fake = _harness_prints_late(tmp_path, monkeypatch, after=None)
    brief = "a brief the new harness never prints anywhere"
    with pytest.raises(AgentPossiblySubmitted, match="never showed in pane .* within 30s"):
        manager.start("worker", cwd=str(tmp_path), harness="claude", brief=brief,
                      working_timeout=30.0)
    assert fake.submitted == [brief]  # never retried
    [document] = _failed_documents(manager, "worker")
    assert document["possibly_submitted"] is True


def test_later_prompt_printed_after_five_seconds_stays_unproven(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The longer window belongs to the start brief alone.
    manager, fake = _harness_prints_late(tmp_path, monkeypatch, after=6.0)
    manager.start("worker", cwd=str(tmp_path), harness="claude")
    with pytest.raises(AgentPossiblySubmitted, match="never showed in pane .* within 5s"):
        manager.send("worker", "a later instruction printed only after six seconds")


def test_prompt_printed_without_its_markdown_markup_is_delivered(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Claude prints `code` without backticks and **bold** without asterisks.
    def rendered(text: str) -> str:
        return "".join(char for char in text if char not in "`*")

    manager, fake = _harness_prints_late(tmp_path, monkeypatch, after=0.0, shown=rendered)
    brief = "read **issue 233** first; Eastern times come from `TZ=America/New_York date`"
    manager.start("worker", cwd=str(tmp_path), harness="claude", brief=brief)
    later = "then run `make validate` and report the **head** commit"
    manager.send("worker", later)
    assert fake.submitted == [brief, later] and _failed_documents(manager, "worker") == []
