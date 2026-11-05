"""Identity and ownership checks for adopting already-running Herdr agents."""
from __future__ import annotations

import json
import hashlib
import fcntl
import os
import shlex
import threading
import time
from collections.abc import Sequence
from dataclasses import replace
from pathlib import Path
from subprocess import CompletedProcess
from typing import cast

import pytest

from agentctl import agent, cli, legacy_cli
import agentctl.subagents as subagents_module
from agentctl.client import (
    AgentPaneInfo, CustomProcessIdentity, HerdrClient, Pane, PaneShellProof,
)
from agentctl.errors import (
    AgentDeliveryError, AgentPossiblySubmitted, HerdrRunError, HerdrUnavailable,
    RecoveryAction, _with_recovery,
)
from agentctl.sessions import Sessions
from agentctl.stop_recovery import stop_refusal_message
from agentctl.subagents import AgentRecord
import agentctl.codex_goal as native_goal
from .test_herdr_subagents import FakeManagedClient


def setup_foreign(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, native_session: bool = True,
) -> tuple[Sessions, FakeManagedClient, str]:
    monkeypatch.delenv("HERDR_WORKSPACE_ID", raising=False)
    fake = FakeManagedClient()
    fake.workspace = "w1"
    tab = fake.create_tab(workspace_id="w1", label="foreign", cwd=str(tmp_path))
    pane = fake.presentations[-1].pane_id
    fake.infos[pane] = AgentPaneInfo(
        pane, "w1", str(tmp_path), "codex", "idle",
        "codex" if native_session else None,
        "native-session" if native_session else None,
    )
    sessions = Sessions(cast(HerdrClient, fake), tmp_path / "registry")

    def goal(session: str, _command: object = None) -> dict[str, object] | None:
        assert session == "native-session"
        objectives = [text[6:] for text in fake.submitted if text.startswith("/goal ")]
        return {"status": "active", "objective": objectives[-1]} if objectives else None

    monkeypatch.setattr(native_goal, "get_goal", goal)
    assert tab == fake.presentations[-1].tab_id
    return sessions, fake, pane


def adopt(sessions: Sessions, pane: str, cwd: Path, *, name: str = "foreign") -> dict[str, object]:
    return sessions.adopt(name, pane_id=pane, expected_workspace="subagents",
                          expected_cwd=str(cwd), harness="codex")


def test_adopted_agent_supports_named_operations_and_preserves_native_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    result = adopt(sessions, pane, tmp_path)
    assert result["adapter"] == "herdr-foreign"
    assert result["pane_id"] == pane
    assert result["session_agent"] == "codex"
    assert result["session_value"] == "native-session"
    assert result["foreign_shell_identity"] == {
        "version": 1,
        "boot_id": "11111111-2222-3333-4444-555555555555",
        "pid": 100,
        "starttime_ticks": 100,
        "executable_device": 3,
        "executable_inode": 4,
    }
    assert result["capabilities"] == [
        "send", "status", "read", "wait", "stop", "attach", "pause", "resume",
        "terminal-snapshot", "drain", "goal", "bind-session", "anchor", "rename",
    ]
    assert [row["name"] for row in sessions.list()] == ["foreign"]

    sessions.send_session("foreign", "follow up")
    assert fake.submitted == ["follow up"]
    assert sessions.read_session("foreign") == "human and coordinator transcript\n"
    # Just after a confirmed delivery, an idle pane is not yet readiness.
    with pytest.raises(AgentDeliveryError, match="has not been seen working"):
        sessions.wait("foreign", timeout=0)
    later = time.time() + subagents_module.DELIVERY_SETTLE_SECONDS + 1
    assert sessions.wait("foreign", timeout=0, wall=lambda: later)["agent_status"] == "idle"
    assert sessions.goal("foreign", "finish adopted work")["native_status"] == "active"
    assert sessions.bind_session("foreign", "native-session")["source"] == "explicit"
    assert fake.submitted[-1] == "/goal finish adopted work"

    binding = json.loads((tmp_path / "registry/foreign/queue/target.json").read_text())
    assert binding["kind"] == "session"
    assert binding["agent"] == "codex"
    assert binding["value"] == "native-session"


def test_stop_unregisters_foreign_agent_without_closing_or_mutating_runtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    sessions.send_session("foreign", "retained request")
    presentation = fake.presentations[0]

    stopped = sessions.stop("foreign")

    assert stopped["runtime_preserved"] is True
    assert stopped["pane_closed"] is False and stopped["tab_closed"] is False
    assert fake.closed == []
    assert fake.presentations == [presentation]
    assert fake.infos[pane].agent == "codex"
    assert sessions.list() == []
    archive = Path(str(stopped["archive"]))
    saved = json.loads((archive / "agent.json").read_text())
    assert saved["adapter"] == "herdr-foreign"
    assert saved["token"] == original["token"]
    assert (archive / "output.json").is_file()
    assert list((archive / "queue/processed").iterdir())


def test_stop_unregisters_confirmed_dead_foreign_agent_without_closing_shell(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    presentation = fake.presentations[0]
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )

    stopped = sessions.stop("foreign", expected_token=str(original["token"]))

    assert stopped["runtime_preserved"] is True
    assert stopped["pane_closed"] is False and stopped["tab_closed"] is False
    assert fake.closed == []
    assert fake.presentations == [presentation]
    assert fake.infos[pane].agent is None
    assert sessions.list() == []
    archive = Path(str(stopped["archive"]))
    assert (archive / "output.json").is_file()
    assert json.loads((archive / "agent.json").read_text())["token"] == original["token"]


def test_malformed_foreign_shell_identities_are_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, _fake, pane = setup_foreign(tmp_path, monkeypatch)
    adopt(sessions, pane, tmp_path)
    path = sessions.registry / "foreign" / "agent.json"
    original = json.loads(path.read_text(encoding="utf-8"))
    identity = original["foreign_shell_identity"]
    assert isinstance(identity, dict)

    variants: list[dict[str, object]] = []
    for field, value in (
        ("version", True),
        ("pid", 0),
        ("pid", 2_147_483_648),
        ("starttime_ticks", 0),
        ("starttime_ticks", 1 << 64),
        ("executable_device", 0),
        ("executable_inode", 1 << 64),
        ("boot_id", "NOT-A-BOOT-ID"),
    ):
        variant = json.loads(json.dumps(original))
        variant["foreign_shell_identity"][field] = value
        variants.append(variant)
    missing = json.loads(json.dumps(original))
    del missing["foreign_shell_identity"]["starttime_ticks"]
    variants.append(missing)
    unknown = json.loads(json.dumps(original))
    unknown["foreign_shell_identity"]["unexpected"] = 1
    variants.append(unknown)
    wrong_adapter = json.loads(json.dumps(original))
    wrong_adapter["adapter"] = "herdr"
    variants.append(wrong_adapter)

    for document in variants:
        path.write_text(json.dumps(document), encoding="utf-8")
        with pytest.raises(AgentDeliveryError, match="foreign shell identity"):
            sessions.get("foreign")


def test_stop_refuses_absent_foreign_agent_when_pane_is_not_idle_shell(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    fake.custom_at_idle_shell = False

    with pytest.raises(AgentDeliveryError, match="identity-bound idle shell"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_dead_foreign_agent_that_leaves_idle_shell_during_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    calls = 0

    def changing_idle_shell(
        pane_id: str, expected: object,
    ) -> bool:
        nonlocal calls
        assert pane_id == pane
        assert expected == fake.foreign_shell_identity
        calls += 1
        return calls == 1

    monkeypatch.setattr(fake, "pane_is_same_idle_shell", changing_idle_shell)
    with pytest.raises(AgentDeliveryError, match="could not be reverified"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert calls == 2
    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


@pytest.mark.parametrize(
    "replacement",
    (
        CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=101, starttime_ticks=100, executable_device=3, executable_inode=4,
        ),
        CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=100, starttime_ticks=101, executable_device=3, executable_inode=4,
        ),
        CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=100, starttime_ticks=100, executable_device=3, executable_inode=5,
        ),
    ),
    ids=("new-pane-shell-pid", "pid-reuse", "replaced-shell-image"),
)
def test_stop_refuses_absent_foreign_agent_with_replaced_shell_generation(
    replacement: CustomProcessIdentity,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    fake.foreign_shell_identity = replacement

    with pytest.raises(AgentDeliveryError, match="pane shell generation changed"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


@pytest.mark.parametrize(
    "replacement",
    (
        CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=101, starttime_ticks=100, executable_device=3, executable_inode=4,
        ),
        CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=100, starttime_ticks=101, executable_device=3, executable_inode=4,
        ),
        CustomProcessIdentity(
            version=1, boot_id="11111111-2222-3333-4444-555555555555",
            pid=100, starttime_ticks=100, executable_device=3, executable_inode=5,
        ),
    ),
    ids=("new-pane-shell-pid", "pid-reuse", "replaced-shell-image"),
)
def test_stop_refuses_live_foreign_agent_with_replaced_shell_generation(
    replacement: CustomProcessIdentity,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.foreign_shell_identity = replacement

    with pytest.raises(AgentDeliveryError, match="pane shell generation changed"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert fake.infos[pane].agent == "codex"
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_sessionless_live_agent_with_replaced_shell_generation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch, native_session=False)
    original = adopt(sessions, pane, tmp_path)
    assert original["session_value"] is None
    fake.foreign_shell_identity = replace(
        fake.foreign_shell_identity,
        starttime_ticks=fake.foreign_shell_identity.starttime_ticks + 1,
    )

    with pytest.raises(AgentDeliveryError, match="pane shell generation changed"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert fake.infos[pane].agent == "codex"
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_absent_legacy_foreign_record_without_shell_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    path = sessions.registry / "foreign" / "agent.json"
    document = json.loads(path.read_text(encoding="utf-8"))
    del document["foreign_shell_identity"]
    path.write_text(json.dumps(document), encoding="utf-8")
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )

    with pytest.raises(AgentDeliveryError, match="legacy record has no identity-bound"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_live_legacy_foreign_record_without_shell_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    path = sessions.registry / "foreign" / "agent.json"
    document = json.loads(path.read_text(encoding="utf-8"))
    del document["foreign_shell_identity"]
    path.write_text(json.dumps(document), encoding="utf-8")

    with pytest.raises(AgentDeliveryError, match="legacy record has no identity-bound"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert fake.infos[pane].agent == "codex"
    assert sessions.get("foreign").lifecycle == "running"


def prepare_legacy_dead(
    sessions: Sessions, fake: FakeManagedClient, pane: str,
) -> tuple[str, str, bytes]:
    """Turn one newly adopted fixture into the exact old on-disk shape."""
    original = adopt(sessions, pane, Path(fake.infos[pane].cwd))
    record_path = sessions.registry / "foreign" / "agent.json"
    document = json.loads(record_path.read_text(encoding="utf-8"))
    del document["foreign_shell_identity"]
    record_path.write_text(json.dumps(document, separators=(",", ":")), encoding="utf-8")
    raw = record_path.read_bytes()
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    return str(original["token"]), hashlib.sha256(raw).hexdigest(), raw


def test_explicit_legacy_recovery_archives_exact_record_queue_and_output_without_runtime_mutation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    sessions.send_session("foreign", "retained request")
    path = sessions.registry / "foreign" / "agent.json"
    document = json.loads(path.read_text(encoding="utf-8"))
    del document["foreign_shell_identity"]
    path.write_text(json.dumps(document, separators=(",", ":")), encoding="utf-8")
    raw = path.read_bytes()
    queue_before = {
        item.relative_to(sessions.registry / "foreign").as_posix(): item.read_bytes()
        for item in (sessions.registry / "foreign" / "queue").rglob("*")
        if item.is_file()
    }
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    presentation = fake.presentations[0]

    stopped = sessions.stop(
        "foreign", expected_token=str(original["token"]),
        recover_legacy_adoption=True,
        expected_record_sha256=hashlib.sha256(raw).hexdigest(),
    )

    archive = Path(str(stopped["archive"]))
    assert stopped["recovered_legacy_adoption"] is True
    assert stopped["runtime_preserved"] is True
    assert stopped["pane_closed"] is False and stopped["tab_closed"] is False
    assert fake.closed == [] and fake.presentations == [presentation]
    assert (archive / "agent.json").read_bytes() == raw
    assert {
        item.relative_to(archive).as_posix(): item.read_bytes()
        for item in (archive / "queue").rglob("*") if item.is_file()
    } == queue_before
    assert json.loads((archive / "output.json").read_text())["text"] == (
        "human and coordinator transcript\n"
    )


@pytest.mark.parametrize("case", ["missing-token", "missing-hash", "wrong-token", "wrong-hash", "explicit-null", "live"])
def test_explicit_legacy_recovery_refuses_incomplete_or_mismatched_authority(
    case: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    path = sessions.registry / "foreign" / "agent.json"
    if case == "explicit-null":
        document = json.loads(raw)
        document["foreign_shell_identity"] = None
        path.write_text(json.dumps(document), encoding="utf-8")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if case == "live":
        fake.infos[pane] = replace(fake.infos[pane], agent="codex")
    kwargs: dict[str, object] = {
        "expected_token": token,
        "recover_legacy_adoption": True,
        "expected_record_sha256": digest,
    }
    if case == "missing-token":
        kwargs["expected_token"] = None
    elif case == "missing-hash":
        kwargs["expected_record_sha256"] = None
    elif case == "wrong-token":
        kwargs["expected_token"] = "wrong-generation"
    elif case == "wrong-hash":
        kwargs["expected_record_sha256"] = "0" * 64

    with pytest.raises(AgentDeliveryError):
        sessions.stop("foreign", **kwargs)  # type: ignore[arg-type]

    assert fake.closed == []
    assert path.exists()
    assert not list((sessions.registry / "archive").glob("foreign-*")) if (
        sessions.registry / "archive"
    ).exists() else True


@pytest.mark.parametrize("change", ["pid", "start", "boot", "device", "inode", "path", "busy", "session", "tab", "workspace", "sibling"])
def test_explicit_legacy_recovery_refuses_identity_or_presentation_change(
    change: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, _raw = prepare_legacy_dead(sessions, fake, pane)
    original_read = fake.read
    if change in {"pid", "start", "boot", "device", "inode"}:
        identity = fake.foreign_shell_identity
        replacement = (
            replace(identity, pid=identity.pid + 1) if change == "pid" else
            replace(identity, starttime_ticks=identity.starttime_ticks + 1)
            if change == "start" else
            replace(identity, boot_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
            if change == "boot" else
            replace(identity, executable_device=identity.executable_device + 1)
            if change == "device" else
            replace(identity, executable_inode=identity.executable_inode + 1)
        )
        def change_generation(
            pane_id: str, *, source: str, lines: int,
            replacement: CustomProcessIdentity = replacement,
        ) -> str:
            fake.foreign_shell_identity = replacement
            return original_read(pane_id, source=source, lines=lines)

        monkeypatch.setattr(fake, "read", change_generation)
    elif change == "busy":
        fake.custom_at_idle_shell = False
    elif change == "session":
        fake.infos[pane] = replace(
            fake.infos[pane], session_agent="codex", session_value="replacement"
        )
    elif change == "tab":
        fake.presentations[0] = replace(fake.presentations[0], tab_id="w1:moved")
    elif change == "workspace":
        fake.presentations[0] = replace(fake.presentations[0], workspace_id="w2")
    elif change == "sibling":
        fake.presentations.append(Pane("w1:sibling", "w1:t1", "w1"))
    elif change == "path":
        original = fake.pane_idle_shell_identity
        calls = 0

        def wrong_path(pane_id: str) -> PaneShellProof | None:
            nonlocal calls
            calls += 1
            proof = original(pane_id)
            assert proof is not None
            return PaneShellProof(
                proof.identity,
                proof.executable_path if calls == 1 else "/bin/dash",
            )

        monkeypatch.setattr(fake, "pane_idle_shell_identity", wrong_path)

    with pytest.raises(AgentDeliveryError):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )

    assert fake.closed == []
    assert (sessions.registry / "foreign" / "agent.json").exists()


def test_explicit_legacy_recovery_maps_shell_proof_transport_failure_to_busy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, _raw = prepare_legacy_dead(sessions, fake, pane)

    def fail_shell_proof(_pane_id: str) -> PaneShellProof | None:
        raise HerdrUnavailable("injected process-info failure")

    monkeypatch.setattr(fake, "pane_idle_shell_identity", fail_shell_proof)
    with pytest.raises(AgentDeliveryError) as raised:
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )

    assert raised.value.exit_code == 75
    assert "cannot prove supported idle shell generation" in str(raised.value)
    assert fake.closed == []
    assert (sessions.registry / "foreign" / "agent.json").exists()


def test_explicit_legacy_recovery_refuses_record_directory_or_shell_change_during_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    for mutation in ("record", "directory", "shell"):
        root = tmp_path / mutation
        root.mkdir()
        sessions, fake, pane = setup_foreign(root, monkeypatch)
        token, digest, _raw = prepare_legacy_dead(sessions, fake, pane)
        original_read = fake.read

        def changing_read(pane_id: str, *, source: str, lines: int) -> str:
            if mutation == "record":
                record_path = sessions.registry / "foreign" / "agent.json"
                document = json.loads(record_path.read_text())
                document["future_field"] = "replacement"
                record_path.write_text(json.dumps(document), encoding="utf-8")
            elif mutation == "directory":
                active = sessions.registry / "foreign"
                displaced = sessions.registry / ".foreign-displaced"
                if not displaced.exists():
                    active.rename(displaced)
                    active.mkdir(mode=0o700)
                    (active / "agent.json").write_bytes((displaced / "agent.json").read_bytes())
                    (active / "agent.json").chmod(0o600)
            else:
                fake.foreign_shell_identity = replace(
                    fake.foreign_shell_identity,
                    starttime_ticks=fake.foreign_shell_identity.starttime_ticks + 1,
                )
            return original_read(pane_id, source=source, lines=lines)

        monkeypatch.setattr(fake, "read", changing_read)
        with pytest.raises(AgentDeliveryError):
            sessions.stop(
                "foreign", expected_token=token, recover_legacy_adoption=True,
                expected_record_sha256=digest,
            )
        assert fake.closed == []
        assert (sessions.registry / "foreign").is_dir()


def test_explicit_legacy_recovery_binds_absent_key_and_parsed_record_to_same_bytes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    record_path = sessions.registry / "foreign" / "agent.json"
    output_path = sessions.registry / "foreign" / "output.json"
    output_path.write_bytes(b'{"prior":"evidence"}\n')
    output_path.chmod(0o600)
    original_read = fake.read

    def swap_absent_for_null(pane_id: str, *, source: str, lines: int) -> str:
        document = json.loads(raw)
        document["foreign_shell_identity"] = None
        agent._atomic_json(str(record_path), document)
        return original_read(pane_id, source=source, lines=lines)

    monkeypatch.setattr(fake, "read", swap_absent_for_null)
    with pytest.raises(AgentDeliveryError, match="requires foreign_shell_identity to be absent"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )
    assert fake.closed == []
    assert output_path.read_bytes() == b'{"prior":"evidence"}\n'
    assert (sessions.registry / "foreign").is_dir()


def test_explicit_legacy_recovery_rechecks_record_after_output_preparation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    output_path = sessions.registry / "foreign" / "output.json"
    prior = b'{"prior":"evidence"}\n'
    output_path.write_bytes(prior)
    output_path.chmod(0o600)
    original_atomic = subagents_module.ManagedAgents._atomic_snapshot_bytes
    changed = False

    def replace_record_after_output(
        pinned: subagents_module._PinnedAgentDirectory,
        content: bytes,
        *,
        name: str = "output.json",
    ) -> subagents_module._InstalledArtifact:
        nonlocal changed
        installed = original_atomic(pinned, content, name=name)
        if name == "output.json" and not changed:
            changed = True
            document = json.loads(raw)
            document["foreign_shell_identity"] = None
            agent._atomic_json(str(sessions.registry / "foreign/agent.json"), document)
        return installed

    monkeypatch.setattr(
        subagents_module.ManagedAgents,
        "_atomic_snapshot_bytes",
        staticmethod(replace_record_after_output),
    )
    with pytest.raises(AgentDeliveryError, match="foreign_shell_identity to be absent"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )

    assert changed is True
    assert fake.closed == []
    assert output_path.read_bytes() == prior
    assert (sessions.registry / "foreign").is_dir()
    assert not list((sessions.registry / "archive").glob(f"foreign-{token}"))


def test_explicit_legacy_recovery_refuses_oversize_serialized_output(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    def oversized_read(_pane_id: str, *, source: str, lines: int) -> str:
        del source, lines
        return "\x00" * (3 << 20)

    monkeypatch.setattr(fake, "read", oversized_read)

    with pytest.raises(AgentDeliveryError, match="output.json larger"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )

    assert fake.closed == []
    assert (sessions.registry / "foreign/agent.json").read_bytes() == raw
    assert not (sessions.registry / "foreign/output.json").exists()
    assert not list((sessions.registry / "archive").glob(f"foreign-{token}"))


def test_explicit_legacy_recovery_uses_utf8_snapshot_budget(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, _raw = prepare_legacy_dead(sessions, fake, pane)
    text = "é" * (3 << 20)

    def non_ascii_read(_pane_id: str, *, source: str, lines: int) -> str:
        del source, lines
        return text

    monkeypatch.setattr(fake, "read", non_ascii_read)
    result = sessions.stop(
        "foreign", expected_token=token, recover_legacy_adoption=True,
        expected_record_sha256=digest,
    )
    output = Path(str(result["archive"])) / "output.json"

    assert output.stat().st_size < subagents_module._MAX_SNAPSHOT_BYTES
    assert json.loads(output.read_text(encoding="utf-8"))["text"] == text
    assert fake.closed == []


def test_explicit_legacy_recovery_rename_race_rolls_back_output_exactly(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, _raw = prepare_legacy_dead(sessions, fake, pane)
    output_path = sessions.registry / "foreign" / "output.json"
    prior = b'{\n  "prior": "exact evidence"\n}\n'
    output_path.write_bytes(prior)
    output_path.chmod(0o600)
    real_rename = subagents_module._rename_directory_noreplace_at

    def collide(
        source_parent: int, source_name: str,
        destination_parent: int, destination_name: str,
    ) -> None:
        if destination_name.startswith("foreign-"):
            destination = sessions.registry / "archive" / destination_name
            destination.symlink_to(tmp_path / "missing-archive-target", target_is_directory=True)
        real_rename(source_parent, source_name, destination_parent, destination_name)

    monkeypatch.setattr(subagents_module, "_rename_directory_noreplace_at", collide)
    with pytest.raises(AgentDeliveryError, match="existing agent archive"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )
    assert output_path.read_bytes() == prior
    assert fake.closed == []
    assert (sessions.registry / "foreign").is_dir()
    assert os.path.lexists(sessions.registry / "archive" / f"foreign-{token}")


def test_explicit_legacy_recovery_fsync_failure_reports_published_archive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    real_fsync = subagents_module._fsync_pinned_directory

    def fail_archive(descriptor: int, label: str) -> None:
        if label == "published agent archive":
            raise OSError("injected archive fsync failure")
        real_fsync(descriptor, label)

    monkeypatch.setattr(subagents_module, "_fsync_pinned_directory", fail_archive)
    with pytest.raises(AgentDeliveryError, match="published.*durability is uncertain"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )
    destination = sessions.registry / "archive" / f"foreign-{token}"
    assert not (sessions.registry / "foreign").exists()
    assert (destination / "agent.json").read_bytes() == raw
    assert (destination / "output.json").is_file()
    assert fake.closed == []


def test_explicit_legacy_recovery_publication_refuses_postproof_directory_swap(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, _raw = prepare_legacy_dead(sessions, fake, pane)
    real_rename = subagents_module._rename_directory_noreplace_at
    swapped = False

    def swap_before_publish(
        source_parent: int, source_name: str,
        destination_parent: int, destination_name: str,
    ) -> None:
        nonlocal swapped
        if not swapped and destination_name.startswith("foreign-"):
            swapped = True
            displaced = sessions.registry / ".foreign-original"
            active = sessions.registry / source_name
            active.rename(displaced)
            active.mkdir(mode=0o700)
            (active / "agent.json").write_bytes((displaced / "agent.json").read_bytes())
            (active / "agent.json").chmod(0o600)
        real_rename(source_parent, source_name, destination_parent, destination_name)

    monkeypatch.setattr(subagents_module, "_rename_directory_noreplace_at", swap_before_publish)
    with pytest.raises(AgentDeliveryError, match="published directory was not"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )
    assert fake.closed == []
    destination = sessions.registry / "archive" / f"foreign-{token}"
    displaced = sessions.registry / ".foreign-original"
    assert not (sessions.registry / "foreign").exists()
    assert (destination / "agent.json").is_file()
    assert (displaced / "agent.json").is_file()
    assert (displaced / "output.json").is_file()


def test_explicit_legacy_recovery_publication_refuses_postproof_record_swap(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    real_rename = subagents_module._rename_directory_noreplace_at
    changed = False

    def replace_record_before_publish(
        source_parent: int, source_name: str,
        destination_parent: int, destination_name: str,
    ) -> None:
        nonlocal changed
        if not changed and destination_name.startswith("foreign-"):
            changed = True
            document = json.loads(raw)
            document["goal"] = "replacement record"
            agent._atomic_json(
                str(sessions.registry / source_name / "agent.json"), document,
            )
        real_rename(source_parent, source_name, destination_parent, destination_name)

    monkeypatch.setattr(
        subagents_module, "_rename_directory_noreplace_at", replace_record_before_publish,
    )
    with pytest.raises(AgentDeliveryError, match="record changed during archival"):
        sessions.stop(
            "foreign", expected_token=token, recover_legacy_adoption=True,
            expected_record_sha256=digest,
        )

    assert changed is True
    assert fake.closed == []
    assert (sessions.registry / "foreign").is_dir()
    assert (sessions.registry / "foreign" / "output.json").is_file()
    assert not list((sessions.registry / "archive").glob(f"foreign-{token}"))


def test_explicit_legacy_recovery_publication_archives_original_after_safe_aba(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    marker = sessions.registry / "foreign" / "queue" / "pending" / "marker"
    marker.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    marker.write_text("original", encoding="utf-8")
    marker.chmod(0o600)
    real_rename = subagents_module._rename_directory_noreplace_at
    swapped = False

    def aba_before_publish(
        source_parent: int, source_name: str,
        destination_parent: int, destination_name: str,
    ) -> None:
        nonlocal swapped
        if not swapped and destination_name.startswith("foreign-"):
            swapped = True
            held = sessions.registry / ".foreign-held"
            active = sessions.registry / source_name
            active.rename(held)
            active.mkdir(mode=0o700)
            (active / "agent.json").write_bytes(raw)
            (active / "agent.json").chmod(0o600)
            (active / "agent.json").unlink()
            active.rmdir()
            held.rename(active)
        real_rename(source_parent, source_name, destination_parent, destination_name)

    monkeypatch.setattr(subagents_module, "_rename_directory_noreplace_at", aba_before_publish)
    result = sessions.stop(
        "foreign", expected_token=token, recover_legacy_adoption=True,
        expected_record_sha256=digest,
    )
    archive = Path(str(result["archive"]))
    assert (archive / "queue" / "pending" / "marker").read_text() == "original"
    assert (archive / "agent.json").read_bytes() == raw
    assert fake.closed == []


def test_explicit_legacy_recovery_preflights_archive_and_snapshot_failures(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    for failure in ("collision", "snapshot"):
        root = tmp_path / failure
        root.mkdir()
        sessions, fake, pane = setup_foreign(root, monkeypatch)
        token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
        if failure == "collision":
            (sessions.registry / "archive" / f"foreign-{token}").mkdir(parents=True)
        else:
            original_atomic = subagents_module.ManagedAgents._atomic_snapshot_bytes
            failed = False

            def fail_output(
                pinned: subagents_module._PinnedAgentDirectory,
                content: bytes, *, name: str = "output.json",
            ) -> subagents_module._InstalledArtifact:
                nonlocal failed
                if name == "output.json" and not failed:
                    failed = True
                    raise AgentDeliveryError("injected snapshot failure")
                return original_atomic(pinned, content, name=name)

            monkeypatch.setattr(
                subagents_module.ManagedAgents, "_atomic_snapshot_bytes",
                staticmethod(fail_output),
            )
        with pytest.raises(AgentDeliveryError):
            sessions.stop(
                "foreign", expected_token=token, recover_legacy_adoption=True,
                expected_record_sha256=digest,
            )
        assert fake.closed == []
        assert (sessions.registry / "foreign" / "agent.json").read_bytes() == raw


def test_stop_refuses_shell_generation_change_during_output_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    original_read = fake.read

    def read(pane_id: str, *, source: str, lines: int) -> str:
        fake.foreign_shell_identity = replace(
            fake.foreign_shell_identity,
            starttime_ticks=fake.foreign_shell_identity.starttime_ticks + 1,
        )
        return original_read(pane_id, source=source, lines=lines)

    monkeypatch.setattr(fake, "read", read)
    with pytest.raises(AgentDeliveryError, match="could not be reverified"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_live_shell_generation_change_during_output_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    original_read = fake.read

    def read(pane_id: str, *, source: str, lines: int) -> str:
        fake.foreign_shell_identity = replace(
            fake.foreign_shell_identity,
            starttime_ticks=fake.foreign_shell_identity.starttime_ticks + 1,
        )
        return original_read(pane_id, source=source, lines=lines)

    monkeypatch.setattr(fake, "read", read)
    with pytest.raises(AgentDeliveryError, match="could not be reverified"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert fake.infos[pane].agent == "codex"
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_dead_foreign_agent_replaced_during_output_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    original_read = fake.read

    def read(pane_id: str, *, source: str, lines: int) -> str:
        fake.infos[pane_id] = replace(
            fake.infos[pane_id], agent="codex", status="idle",
            session_agent="codex", session_value="replacement-session",
        )
        return original_read(pane_id, source=source, lines=lines)

    monkeypatch.setattr(fake, "read", read)
    with pytest.raises(AgentDeliveryError, match="identity could not be reverified"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_dead_foreign_agent_that_resumes_same_session_during_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The before/after comparison, not only _checked(), must reject a restart."""
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    original_read = fake.read

    def read(pane_id: str, *, source: str, lines: int) -> str:
        fake.infos[pane_id] = replace(
            fake.infos[pane_id], agent="codex", status="idle",
            session_agent="codex", session_value="native-session",
        )
        return original_read(pane_id, source=source, lines=lines)

    monkeypatch.setattr(fake, "read", read)
    with pytest.raises(AgentDeliveryError, match="changed during output capture"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_sessionless_foreign_agent_that_gains_session_during_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(
        tmp_path, monkeypatch, native_session=False
    )
    original = adopt(sessions, pane, tmp_path)
    original_read = fake.read

    def read(pane_id: str, *, source: str, lines: int) -> str:
        fake.infos[pane_id] = replace(
            fake.infos[pane_id], session_agent="codex",
            session_value="new-native-session",
        )
        return original_read(pane_id, source=source, lines=lines)

    monkeypatch.setattr(fake, "read", read)
    with pytest.raises(AgentDeliveryError, match="changed during output capture"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_refuses_live_foreign_agent_with_replaced_native_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], session_value="replacement-session",
    )

    with pytest.raises(AgentDeliveryError, match="exactly one live pane"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


def test_stop_unregisters_revalidated_live_foreign_agent_after_tab_move(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.presentations[0] = replace(fake.presentations[0], tab_id="w1:moved")

    stopped = sessions.stop("foreign", expected_token=str(original["token"]))

    assert stopped["runtime_preserved"] is True
    assert stopped["pane_closed"] is False and stopped["tab_closed"] is False
    assert fake.closed == []
    assert fake.presentations[0].tab_id == "w1:moved"
    assert sessions.list() == []


@pytest.mark.parametrize("presentation_count", [0, 2])
def test_stop_refuses_missing_or_duplicated_recorded_foreign_pane(
    presentation_count: int, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    original_presentation = fake.presentations[0]
    fake.presentations.clear()
    if presentation_count == 2:
        fake.presentations.extend(
            (
                original_presentation,
                replace(original_presentation, tab_id="w1:duplicate"),
            )
        )

    with pytest.raises(
        AgentDeliveryError,
        match=rf"expected one recorded pane, found {presentation_count}",
    ):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


@pytest.mark.parametrize(("closed", "source"), [
    ("workspace", "workspace-missing"), ("pane", "pane-missing"),
])
def test_stop_archives_foreign_agent_whose_workspace_or_pane_herdr_reports_closed(
    closed: str, source: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.presentations.clear()
    tab_labels = fake.tab_labels

    def closed_workspace(workspace_id: str) -> dict[str, str]:
        if closed == "workspace":
            raise HerdrUnavailable(f'tab list: {{"error":{{"code":"workspace_not_found",'
                                   f'"message":"workspace {workspace_id} not found"}}}}')
        return tab_labels(workspace_id)

    def closed_pane(pane_id: str) -> AgentPaneInfo:
        raise HerdrUnavailable(f'pane get: {{"error":{{"code":"pane_not_found",'
                               f'"message":"pane {pane_id} not found"}}}}')

    monkeypatch.setattr(fake, "tab_labels", closed_workspace)
    monkeypatch.setattr(fake, "pane_info", closed_pane)

    stopped = sessions.stop("foreign", expected_token=str(original["token"]))

    assert stopped["source"] == source
    assert stopped["pane_closed"] is False and stopped["tab_closed"] is False
    assert fake.closed == []
    assert sessions.list() == []
    archive = Path(str(stopped["archive"]))
    saved = json.loads((archive / "agent.json").read_text())
    assert saved["token"] == original["token"] and saved["lifecycle"] == "stopped"
    reason = json.loads((archive / "stop.json").read_text())
    assert reason["source"] == source and reason["pane_id"] == pane
    assert "_not_found" in reason["detail"]


def test_stop_refuses_foreign_agent_missing_from_the_pane_list_during_an_outage(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.presentations.clear()

    def outage(_target: str) -> object:
        raise HerdrUnavailable("tab list: connection refused")

    monkeypatch.setattr(fake, "tab_labels", outage)
    monkeypatch.setattr(fake, "pane_info", outage)

    with pytest.raises(AgentDeliveryError, match="expected one recorded pane, found 0"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


@pytest.mark.parametrize("change", ["cwd", "presentation", "session"])
def test_stop_refuses_dead_foreign_agent_with_changed_identity(
    change: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    fake.infos[pane] = replace(
        fake.infos[pane], agent=None, status="unknown",
        session_agent=None, session_value=None,
    )
    if change == "cwd":
        fake.infos[pane] = replace(fake.infos[pane], cwd=str(tmp_path / "other"))
    elif change == "presentation":
        fake.presentations[0] = replace(fake.presentations[0], tab_id="w1:other")
    else:
        fake.infos[pane] = replace(
            fake.infos[pane], session_agent="codex", session_value="stale-session",
        )

    with pytest.raises(AgentDeliveryError, match="refusing"):
        sessions.stop("foreign", expected_token=str(original["token"]))

    assert fake.closed == []
    assert sessions.get("foreign").lifecycle == "running"


@pytest.mark.parametrize(
    ("change", "message"),
    [
        ({"agent": None}, "agent is None"),
        ({"agent": "claude"}, "expected 'codex'"),
        ({"workspace": "wrong"}, "workspace is 'subagents'"),
        ({"cwd": "wrong"}, "cwd is"),
        ({"session": "wrong"}, "exactly one live pane"),
    ],
)
def test_adopt_refuses_live_identity_mismatch_without_registering(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    change: dict[str, str | None], message: str,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    info = fake.infos[pane]
    if "agent" in change:
        fake.infos[pane] = replace(info, agent=change["agent"])
    workspace = str(change.get("workspace", "subagents"))
    cwd = tmp_path
    if change.get("cwd") == "wrong":
        cwd = tmp_path / "other"
        cwd.mkdir()
    session = str(change["session"]) if "session" in change else None

    with pytest.raises(AgentDeliveryError, match=message):
        sessions.adopt("foreign", pane_id=pane, expected_workspace=workspace,
                       expected_cwd=str(cwd), harness="codex", session=session)

    assert not (sessions.registry / "foreign").exists()
    assert fake.closed == [] and fake.submitted == []


def test_adopt_refuses_duplicate_pane_and_keeps_first_generation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, _fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    with pytest.raises(AgentDeliveryError, match="already registered as 'foreign'"):
        adopt(sessions, pane, tmp_path, name="second")
    assert sessions.get("foreign").token == original["token"]
    assert not (sessions.registry / "second").exists()


def test_same_provider_local_session_id_is_allowed_for_different_harnesses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    adopt(sessions, pane, tmp_path)
    fake.create_tab(workspace_id="w1", label="claude", cwd=str(tmp_path))
    claude = fake.presentations[-1].pane_id
    fake.infos[claude] = AgentPaneInfo(
        claude, "w1", str(tmp_path), "claude", "idle",
        "claude", "native-session",
    )
    result = sessions.adopt(
        "claude", pane_id=claude, expected_workspace="subagents",
        expected_cwd=str(tmp_path), harness="claude",
    )
    assert result["session_agent"] == "claude"
    assert result["session_value"] == "native-session"
    assert {row["name"] for row in sessions.list()} == {"foreign", "claude"}


def test_adopt_refuses_same_harness_session_held_by_headless_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, _fake, pane = setup_foreign(tmp_path, monkeypatch)
    sessions._prepare()
    (sessions.registry / "headless").mkdir(mode=0o700)
    sessions._save(AgentRecord(
        "headless", "headless-token", "codex", str(tmp_path), 1.0,
        lifecycle="running", session_value="native-session",
        adapter="turn-runner", mode="headless", backend="tmux",
        runtime_home=str(tmp_path / "runtime"),
    ))
    with pytest.raises(AgentDeliveryError, match="already registered as 'headless'"):
        adopt(sessions, pane, tmp_path)
    assert not (sessions.registry / "foreign").exists()


def test_interactive_start_refuses_resume_claimed_by_adopted_agent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    adopt(sessions, pane, tmp_path)
    presentation = fake.presentations[0]
    with pytest.raises(AgentDeliveryError, match="already registered as 'foreign'"):
        sessions.start(
            "owned", cwd=str(tmp_path), harness="codex",
            resume="native-session",
        )
    assert not (sessions.registry / "owned").exists()
    assert fake.presentations == [presentation]
    assert fake.launched == [] and fake.closed == []


def test_conflicting_started_pane_remains_stoppable_if_initial_close_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    adopt(sessions, pane, tmp_path)
    original_start, original_close = fake.start_agent, fake.close_pane

    def start_agent(
        name: str, kind: str, pane_id: str, arguments: tuple[str, ...], *, timeout: float,
    ) -> None:
        original_start(name, kind, pane_id, arguments, timeout=timeout)
        fake.infos[pane_id] = replace(
            fake.infos[pane_id], session_agent="codex",
            session_value="native-session",
        )

    def fail_close(_pane: str) -> None:
        raise AgentDeliveryError("close failed")

    monkeypatch.setattr(fake, "start_agent", start_agent)
    monkeypatch.setattr(fake, "close_pane", fail_close)
    with pytest.raises(AgentDeliveryError, match="could not close"):
        sessions.start("owned", cwd=str(tmp_path), harness="codex")
    failed = sessions.get("owned")
    assert failed.lifecycle == "launch_failed"
    assert failed.session_agent is None and failed.session_value is None
    monkeypatch.setattr(fake, "close_pane", original_close)
    assert sessions.stop("owned")["pane_closed"] is True
    assert sessions.status("foreign")["agent_status"] == "idle"


def test_headless_start_stops_runtime_if_session_is_claimed_by_adopted_agent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, _fake, pane = setup_foreign(tmp_path, monkeypatch)
    adopt(sessions, pane, tmp_path)
    actions: list[str] = []

    def worker(
        record: AgentRecord, action: str, **_options: object,
    ) -> dict[str, object]:
        actions.append(action)
        return {"record": {"mode": "headless", "backend": "tmux",
                           "session_id": "native-session"}, "result": {}}

    monkeypatch.setattr(sessions, "_worker", worker)
    with pytest.raises(AgentDeliveryError, match="already registered as 'foreign'"):
        sessions.start_session(
            "headless", cwd=str(tmp_path), mode="headless", backend="tmux",
        )
    failed = sessions.get("headless")
    assert failed.lifecycle == "launch_failed"
    assert failed.session_value is None
    assert actions == ["start", "stop"]
    assert sessions.status("foreign")["agent_status"] == "idle"


def test_adopt_refuses_a_reported_session_visible_in_two_live_panes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    fake.create_tab(workspace_id="w1", label="duplicate", cwd=str(tmp_path))
    duplicate = fake.presentations[-1].pane_id
    fake.infos[duplicate] = AgentPaneInfo(
        duplicate, "w1", str(tmp_path), "codex", "idle",
        "codex", "native-session",
    )
    with pytest.raises(AgentDeliveryError, match="exactly one live pane"):
        adopt(sessions, pane, tmp_path)
    assert not (sessions.registry / "foreign").exists()


@pytest.mark.parametrize("native_session", [True, False])
def test_adopt_archives_failed_generation_if_identity_changes_after_save(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, native_session: bool,
) -> None:
    sessions, fake, pane = setup_foreign(
        tmp_path, monkeypatch, native_session=native_session
    )
    original = fake.pane_info

    def changing(pane_id: str) -> AgentPaneInfo:
        info = original(pane_id)
        if (sessions.registry / "foreign/agent.json").exists():
            return replace(info, session_agent="codex",
                           session_value="replacement-session")
        return info

    monkeypatch.setattr(fake, "pane_info", changing)
    with pytest.raises(AgentDeliveryError, match="was not registered"):
        adopt(sessions, pane, tmp_path)

    assert not (sessions.registry / "foreign").exists()
    archives = list((sessions.registry / "archive").glob(
        "foreign-*-adopt-failed/agent.json"
    ))
    assert len(archives) == 1
    saved = json.loads(archives[0].read_text())
    assert saved["lifecycle"] == "adopt_failed"
    assert saved["error"]
    assert fake.closed == [] and fake.submitted == []


def test_adopt_archives_failed_generation_if_shell_changes_after_save(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = fake.pane_shell_identity

    def changing(pane_id: str) -> CustomProcessIdentity:
        identity = original(pane_id)
        if (sessions.registry / "foreign/agent.json").exists():
            return replace(identity, starttime_ticks=identity.starttime_ticks + 1)
        return identity

    monkeypatch.setattr(fake, "pane_shell_identity", changing)
    with pytest.raises(AgentDeliveryError, match="was not registered"):
        adopt(sessions, pane, tmp_path)

    assert not (sessions.registry / "foreign").exists()
    archives = list((sessions.registry / "archive").glob(
        "foreign-*-adopt-failed/agent.json"
    ))
    assert len(archives) == 1
    saved = json.loads(archives[0].read_text(encoding="utf-8"))
    assert saved["lifecycle"] == "adopt_failed"
    assert "shell process identity changed" in saved["error"]
    assert fake.closed == [] and fake.submitted == []


def test_adopt_without_reported_session_can_be_bound_for_native_goal_inspection(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch, native_session=False)
    result = adopt(sessions, pane, tmp_path)
    assert result["session_value"] is None
    assert sessions.goal("foreign")["native_status"] == "unverified"
    sessions.bind_session("foreign", "native-session")
    assert sessions.goal("foreign")["native_status"] == "absent"
    sessions.send_session("foreign", "still pane-bound")
    binding = json.loads((tmp_path / "registry/foreign/queue/target.json").read_text())
    assert binding["kind"] == "pane" and binding["pane_id"] == pane
    assert fake.submitted == ["still pane-bound"]


def test_bind_session_refuses_an_authoritative_owner_in_another_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    adopt(sessions, pane, tmp_path)
    fake.create_tab(workspace_id="w1", label="sessionless", cwd=str(tmp_path))
    second = fake.presentations[-1].pane_id
    fake.infos[second] = AgentPaneInfo(
        second, "w1", str(tmp_path), "codex", "idle", None, None,
    )
    sessions.adopt(
        "second", pane_id=second, expected_workspace="subagents",
        expected_cwd=str(tmp_path), harness="codex",
    )
    with pytest.raises(AgentDeliveryError, match="already registered as 'foreign'"):
        sessions.bind_session("second", "native-session")
    assert sessions.get("second").goal_session_id is None


def test_bind_session_refuses_unregistered_live_session_contradiction(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(
        tmp_path, monkeypatch, native_session=False
    )
    adopt(sessions, pane, tmp_path)
    fake.create_tab(workspace_id="w1", label="unregistered", cwd=str(tmp_path))
    unregistered = fake.presentations[-1].pane_id
    fake.infos[unregistered] = AgentPaneInfo(
        unregistered, "w1", str(tmp_path), "codex", "idle",
        "codex", "reported-elsewhere",
    )
    with pytest.raises(AgentDeliveryError, match="another or ambiguous live pane"):
        sessions.bind_session("foreign", "reported-elsewhere")
    assert sessions.get("foreign").goal_session_id is None


def test_concurrent_bind_session_allows_exactly_one_provider_local_owner(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane = setup_foreign(
        tmp_path, monkeypatch, native_session=False
    )
    adopt(sessions, pane, tmp_path)
    fake.create_tab(workspace_id="w1", label="second", cwd=str(tmp_path))
    second = fake.presentations[-1].pane_id
    fake.infos[second] = AgentPaneInfo(
        second, "w1", str(tmp_path), "codex", "idle", None, None,
    )
    sessions.adopt(
        "second", pane_id=second, expected_workspace="subagents",
        expected_cwd=str(tmp_path), harness="codex",
    )
    barrier = threading.Barrier(3)
    outcomes: list[str] = []
    outcome_lock = threading.Lock()

    def bind(name: str) -> None:
        barrier.wait()
        try:
            sessions.bind_session(name, "shared-session")
        except AgentDeliveryError:
            outcome = "refused"
        else:
            outcome = "bound"
        with outcome_lock:
            outcomes.append(outcome)

    threads = [threading.Thread(target=bind, args=(name,))
               for name in ("foreign", "second")]
    for thread in threads:
        thread.start()
    barrier.wait()
    for thread in threads:
        thread.join(2)
        assert not thread.is_alive()
    assert sorted(outcomes) == ["bound", "refused"]
    claims = [sessions.get(name).goal_session_id
              for name in ("foreign", "second")]
    assert claims.count("shared-session") == 1


def test_cli_adopt_requires_identity_and_stop_is_non_destructive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    monkeypatch.setattr(cli, "HerdrClient", lambda **_kwargs: cast(HerdrClient, fake))
    registry = str(sessions.registry)
    assert cli.main(["adopt", "foreign", "--pane", pane, "--workspace", "subagents",
                     "--cwd", str(tmp_path), "--harness", "codex",
                     "--registry", registry]) == 0
    adopted = json.loads(capsys.readouterr().out)
    assert adopted["adapter"] == "herdr-foreign"
    assert cli.main(["stop", "foreign", "--registry", registry]) == 0
    stopped = json.loads(capsys.readouterr().out)
    assert stopped["runtime_preserved"] is True
    assert fake.closed == []


def test_cli_legacy_recovery_requires_and_forwards_exact_generation_assertions(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    token, digest, raw = prepare_legacy_dead(sessions, fake, pane)
    monkeypatch.setattr(cli, "HerdrClient", lambda **_kwargs: cast(HerdrClient, fake))

    assert cli.main([
        "stop", "foreign", "--registry", str(sessions.registry),
        "--recover-legacy-adoption", "--expected-token", token,
        "--expected-record-sha256", digest,
    ]) == 0

    stopped = json.loads(capsys.readouterr().out)
    assert stopped["recovered_legacy_adoption"] is True
    assert Path(stopped["archive"]).joinpath("agent.json").read_bytes() == raw
    assert fake.closed == []


def test_stop_help_documents_loud_legacy_recovery_gate(
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit) as result:
        cli.parser().parse_args(["stop", "--help"])
    assert result.value.code == 0
    output = capsys.readouterr().out
    for option in (
        "--recover-legacy-adoption", "--expected-token",
        "--expected-record-sha256",
    ):
        assert option in output


def test_adopt_help_names_every_required_identity_assertion(
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit) as result:
        cli.parser().parse_args(["adopt", "--help"])
    assert result.value.code == 0
    output = capsys.readouterr().out
    for value in ("--pane", "--workspace", "--cwd", "--harness", "--session",
                  "without taking ownership"):
        assert value in output
    assert "Muse requires a pinned foreground process" in output


def _suggestion13_foreign_muse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, native_session: bool = True,
    mode: str = "Auto-review",
) -> tuple[Sessions, FakeManagedClient, str, list[str]]:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch, native_session=native_session)
    fake.infos[pane] = replace(
        fake.infos[pane], agent="muse", session_agent="muse" if native_session else None,
        terminal_id="term-1", tab_id="w1:t1",
    )
    draft = ""
    pastes: list[str] = []

    def screen(_pane: str) -> str:
        transcript = "\n".join(fake.transcripts.get(pane, []))
        permission = "" if mode == "standard" else f" · {mode}"
        return ("Muse Code 1.3.0\n" + transcript + "\n" + "─" * 40 + "\n❯"
                + (" " + draft if draft else "") + "\n" + "─" * 40
                + f"\nmodel · high · {tmp_path}{permission}\n")

    def read(pane_id: str, *, source: str, lines: int) -> str:
        assert pane_id == pane and lines > 0
        return "\n".join(fake.transcripts.get(pane, [])) if source == "recent-unwrapped" else screen(pane_id)

    def paste(pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        nonlocal draft
        fake._effect(pane_id, expect_terminal)
        assert text.startswith("\x1b[200~") and text.endswith("\x1b[201~")
        draft = text[len("\x1b[200~"):-len("\x1b[201~")]
        pastes.append(text)

    original_keys = fake.send_keys

    def keys(pane_id: str, value: str, *, expect_terminal: str | None = None) -> None:
        nonlocal draft
        original_keys(pane_id, value, expect_terminal=expect_terminal)
        if value == "Enter":
            fake.submitted.append(draft)
            fake.transcripts.setdefault(pane_id, []).append(f"❯ {draft}")
            draft = ""

    def forbidden_native(_pane: str, _text: str, *, expect_terminal: str | None = None) -> None:
        del expect_terminal
        raise AssertionError("foreign Muse must use guarded editor input")

    monkeypatch.setattr(fake, "read", read)
    monkeypatch.setattr(fake, "read_screen", screen, raising=False)
    monkeypatch.setattr(fake, "send_text", paste, raising=False)
    monkeypatch.setattr(fake, "send_keys", keys)
    monkeypatch.setattr(fake, "agent_prompt", forbidden_native)
    return sessions, fake, pane, pastes


@pytest.mark.parametrize("native_session", [False, True])
def test_suggestion13_adopted_muse_uses_pinned_guarded_editor_and_preserves_runtime_ownership(
    native_session: bool, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, pastes = _suggestion13_foreign_muse(
        tmp_path, monkeypatch, native_session=native_session,
    )
    original_info, original_labels = fake.infos[pane], dict(fake.labels)
    result = sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                            expected_cwd=str(tmp_path), harness="muse")
    record = sessions.get("foreign")
    assert result["adapter"] == "herdr-foreign" and result["agent_status"] == "idle"
    assert record.harness_anchor == fake.harness_identity(pane, "muse")
    assert record.foreign_shell_identity == fake.foreign_shell_identity
    assert record.custom_process_identity is None and not record.pane_reported_by_agentctl
    fake.expect_supported = True
    assert sessions.send_session("foreign", "literal follow up", ready_timeout=0)["delivered"]
    assert fake.submitted == ["literal follow up"]
    assert pastes == ["\x1b[200~literal follow up\x1b[201~"]
    assert fake.keys_sent == [(pane, "Enter")]
    assert fake.expected_terminals == ["term-1", "term-1"]
    sessions.rename("foreign", "reviewer")
    assert sessions.get("reviewer").adapter == "herdr-foreign"
    assert sessions.get("reviewer").token == record.token
    sessions.stop("reviewer")
    assert fake.infos[pane] == original_info and fake.labels == original_labels
    assert fake.launched == [] and fake.closed == []


@pytest.mark.parametrize("mode", ["YOLO", "standard"])
def test_suggestion13_foreign_muse_retains_current_composer_permission_policy(
    mode: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, pastes = _suggestion13_foreign_muse(tmp_path, monkeypatch, mode=mode)
    fake.transcripts[pane] = ["quoted prior model · high · /work · Auto-review"]
    sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                   expected_cwd=str(tmp_path), harness="muse")
    with pytest.raises(AgentPossiblySubmitted, match="Auto-review|YOLO"):
        sessions.send_session("foreign", "must remain untyped", ready_timeout=0)
    assert pastes == [] and fake.keys_sent == [] and fake.submitted == []
    assert sessions.get("foreign").adapter == "herdr-foreign"


@pytest.mark.parametrize("changed", ["harness", "shell", "terminal", "native", "anchor"])
def test_suggestion13_foreign_muse_input_refuses_replacement_or_unanchored_recipient(
    changed: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, pastes = _suggestion13_foreign_muse(tmp_path, monkeypatch)
    sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                   expected_cwd=str(tmp_path), harness="muse")
    if changed == "harness":
        fake.harness_pids[pane] += 1
    elif changed == "shell":
        fake.foreign_shell_identity = replace(fake.foreign_shell_identity,
            starttime_ticks=fake.foreign_shell_identity.starttime_ticks + 1)
    elif changed == "terminal":
        fake.infos[pane] = replace(fake.infos[pane], terminal_id="replacement")
    elif changed == "native":
        fake.infos[pane] = replace(fake.infos[pane], session_value="replacement")
    else:
        record = sessions.get("foreign")
        record.harness_identity = None
        record.anchor_rule = None
        sessions._save(record)
    with pytest.raises((AgentDeliveryError, HerdrUnavailable)):
        sessions.send_session("foreign", "must remain pending", ready_timeout=0)
    assert pastes == [] and fake.keys_sent == [] and fake.submitted == [] and fake.closed == []


def test_suggestion13_foreign_muse_screen_read_cannot_replace_saved_shell_proof(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, _pastes = _suggestion13_foreign_muse(tmp_path, monkeypatch)
    result = sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                            expected_cwd=str(tmp_path), harness="muse")
    original_read = fake.read

    def read(pane_id: str, *, source: str, lines: int) -> str:
        text = original_read(pane_id, source=source, lines=lines)
        fake.foreign_shell_identity = replace(fake.foreign_shell_identity,
            starttime_ticks=fake.foreign_shell_identity.starttime_ticks + 1)
        return text

    monkeypatch.setattr(fake, "read", read)
    status = sessions.status("foreign")
    assert status["agent_status"] == "unknown"
    assert "pinned shell process" in str(status["probe_error"])
    assert status["foreign_shell_identity"] == result["foreign_shell_identity"]
    assert fake.submitted == [] and fake.keys_sent == []


@pytest.mark.parametrize("native_session", [False, True])
def test_suggestion13_muse_adoption_does_not_accept_foreground_replacement_during_census(
    native_session: bool, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, _pastes = _suggestion13_foreign_muse(
        tmp_path, monkeypatch, native_session=native_session,
    )
    original_panes = fake.panes
    replaced = False

    def panes(workspace_id: str | None = None) -> tuple[Pane, ...]:
        nonlocal replaced
        if not replaced:
            fake.harness_pids[pane] += 1
            replaced = True
        return original_panes(workspace_id)

    monkeypatch.setattr(fake, "panes", panes)
    with pytest.raises(AgentDeliveryError, match="Muse foreground process changed"):
        sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                       expected_cwd=str(tmp_path), harness="muse")
    assert not (sessions.registry / "foreign").exists()
    assert fake.launched == [] and fake.closed == [] and fake.submitted == []


def test_suggestion13_muse_adoption_final_process_failure_archives_diagnostic_without_runtime_effects(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, _pastes = _suggestion13_foreign_muse(tmp_path, monkeypatch)
    original_save = sessions._save

    def save(record: AgentRecord) -> None:
        original_save(record)
        fake.harness_pids[pane] += 1

    monkeypatch.setattr(sessions, "_save", save)
    with pytest.raises(AgentDeliveryError, match="diagnostic record archived"):
        sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                       expected_cwd=str(tmp_path), harness="muse")
    assert not (sessions.registry / "foreign").exists()
    archived = list((sessions.registry / "archive").iterdir())
    assert len(archived) == 1
    diagnostic = AgentRecord.load(archived[0] / "agent.json", "foreign")
    assert diagnostic.lifecycle == "adopt_failed" and diagnostic.adapter == "herdr-foreign"
    assert diagnostic.harness_anchor is not None and diagnostic.harness_anchor.pid != fake.harness_pids[pane]
    assert fake.launched == [] and fake.closed == [] and fake.submitted == []


def test_suggestion13_foreign_muse_after_paste_replacement_quarantines_without_enter(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, pane, pastes = _suggestion13_foreign_muse(tmp_path, monkeypatch)
    sessions.adopt("foreign", pane_id=pane, expected_workspace="subagents",
                   expected_cwd=str(tmp_path), harness="muse")
    paste = cast(object, getattr(fake, "send_text"))
    assert callable(paste)

    def replace_after_paste(pane_id: str, text: str, *, expect_terminal: str | None = None) -> None:
        paste(pane_id, text, expect_terminal=expect_terminal)
        fake.harness_pids[pane] += 1

    monkeypatch.setattr(fake, "send_text", replace_after_paste)
    with pytest.raises(AgentPossiblySubmitted, match="MISROUTE|quarantin") as refused:
        sessions.send_session("foreign", "possibly staged", ready_timeout=0)
    assert pastes == ["\x1b[200~possibly staged\x1b[201~"]
    assert fake.keys_sent == [] and fake.submitted == [] and fake.closed == []
    artifact = Path(refused.value.artifact)
    assert artifact.parent.name == "failed" and artifact.is_file()
    document = json.loads(artifact.read_text(encoding="utf-8"))
    assert document["possibly_submitted"] is True and document["probable_misroute"] is True
    assert sessions.drain("foreign", ready_timeout=0).delivered == ()
    assert fake.keys_sent == [] and fake.submitted == []


def _suggestion4_dead_adoption(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> tuple[Sessions, FakeManagedClient, str, str, bytes]:
    sessions, fake, pane = setup_foreign(tmp_path, monkeypatch)
    original = adopt(sessions, pane, tmp_path)
    path = sessions.registry / "foreign" / "agent.json"
    document = json.loads(path.read_bytes())
    document["future_field"] = {"nested": [0.918216731832064, "unchanged"]}
    raw = (json.dumps(document, indent=3) + "\n\n").encode("utf-8")
    path.write_bytes(raw)
    return sessions, fake, str(original["token"]), hashlib.sha256(raw).hexdigest(), raw


def _suggestion4_no_runtime(
    fake: FakeManagedClient, monkeypatch: pytest.MonkeyPatch,
) -> None:
    def forbidden(*_args: object, **_kwargs: object) -> None:
        raise AssertionError("dead-adoption retirement must not query or mutate Herdr")

    for name in (
        "panes", "pane_info", "pane_shell_identity", "pane_is_same_idle_shell",
        "harness_identity", "verify_harness_identity", "read", "close_pane",
        "close_tab", "rename_tab", "rename_agent", "report_pane_agent", "agent_names",
        "tab_labels", "send_text", "send_keys", "launch",
    ):
        monkeypatch.setattr(fake, name, forbidden, raising=False)


def _suggestion4_offline_client() -> HerdrClient:
    def forbidden(_argv: Sequence[str]) -> CompletedProcess[str]:
        raise AssertionError("dead-adoption retirement must not invoke Herdr")

    return HerdrClient(herdr_bin="absent-herdr", run=forbidden)


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux process identity required")
@pytest.mark.parametrize("queued", [False, True])
def test_suggestion4_offline_retirement_preserves_exact_artifacts_and_lazy_queue(
    queued: bool, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    directory = sessions.registry / "foreign"
    agent_inode = (directory / "agent.json").stat().st_ino
    (directory / "output.json").write_bytes(b"previous snapshot: retain exact bytes\x00\n")
    (directory / "output.json").chmod(0o600)
    (directory / "misroutes.jsonl").write_bytes(b'{"quarantined":true}\n')
    (directory / "misroutes.jsonl").chmod(0o600)
    if queued:
        queue = directory / "queue"
        agent.enqueue(str(queue), "never drained", message_id="pending-work")
        assert (queue / ".delivery.lock").exists()
        assert not (queue / ".binding.lock").exists()
        (queue / "failed" / "quarantined.json").write_bytes(b'{"possibly_submitted":true}\n')
        (queue / "failed" / "quarantined.json").chmod(0o600)
        (queue / "future.json").write_bytes(b"opaque future artifact")
        (queue / "future.json").chmod(0o600)
    before = {
        path.relative_to(directory).as_posix(): path.read_bytes()
        for path in directory.rglob("*") if path.is_file()
    }
    sessions.client = _suggestion4_offline_client()
    anchor = sessions.get("foreign").harness_anchor
    assert anchor is not None and sessions.client.process_liveness(anchor) == "dead"

    result = sessions.stop(
        "foreign", retire_dead_adoption=True, expected_token=token,
        expected_record_sha256=digest,
    )

    archive = sessions.registry / "archive" / f"foreign-{token}"
    assert result == {
        "name": "foreign", "archive": str(archive), "pane_closed": False,
        "tab_closed": False, "runtime_preserved": True, "retired_dead_adoption": True,
        "record_sha256": digest,
    }
    assert not directory.exists() and (archive / "agent.json").read_bytes() == raw
    assert (archive / "agent.json").stat().st_ino == agent_inode
    assert json.loads(raw)["lifecycle"] == "running"
    assert {name: (archive / name).read_bytes() for name in before} == before
    if queued:
        assert (archive / "queue/.binding.lock").read_bytes() == b""
        assert not (archive / "queue/target.json").exists()
        assert (archive / "queue/inbox/pending-work.json").read_bytes() == before["queue/inbox/pending-work.json"]
    else:
        assert not (archive / "queue").exists()
    assert fake.closed == [] and fake.submitted == [] and fake.keys_sent == []


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux process identity required")
@pytest.mark.parametrize("generation", ["reused-pid", "live", "exec-image", "probe-error"])
def test_suggestion4_kernel_generation_proof_preserves_live_replacement(
    generation: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, _digest, _raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    client = _suggestion4_offline_client()
    observed = client._process_identity(os.getpid())
    assert observed is not None
    identity = observed[0]
    assert client.process_liveness(identity) == "alive"
    record = sessions.get("foreign")
    if generation == "reused-pid":
        assert identity.starttime_ticks > 1
        record.harness_identity = replace(identity, starttime_ticks=identity.starttime_ticks - 1)
    elif generation == "exec-image":
        record.harness_identity = replace(identity, executable_inode=identity.executable_inode + 1)
    else:
        record.harness_identity = identity
    sessions._save(record)
    path = sessions.registry / "foreign" / "agent.json"
    raw = path.read_bytes()
    sessions.client = client
    if generation == "probe-error":
        def unavailable(_pid: int) -> None:
            raise PermissionError("kernel generation unavailable")
        monkeypatch.setattr(client, "_liveness_process_stat", unavailable)
    if generation == "reused-pid":
        result = sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                               expected_record_sha256=hashlib.sha256(raw).hexdigest())
        assert (Path(str(result["archive"])) / "agent.json").read_bytes() == raw
    else:
        with pytest.raises(AgentDeliveryError, match="alive|unknown"):
            sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                          expected_record_sha256=hashlib.sha256(raw).hexdigest())
        assert path.read_bytes() == raw
    # Even a reused PID belongs to the current live generation and is untouched.
    os.kill(os.getpid(), 0)
    assert fake.closed == [] and fake.keys_sent == []


@pytest.mark.parametrize("case", [
    "missing-token", "missing-hash", "wrong-token", "wrong-hash", "malformed-token",
    "uppercase-hash", "short-hash", "same-token-raw-change", "legacy-overlap",
])
def test_suggestion4_explicit_retirement_requires_exact_assertions_before_runtime(
    case: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    expected_token: str | None = token
    expected_digest: str | None = digest
    if case == "missing-token":
        expected_token = None
    elif case == "missing-hash":
        expected_digest = None
    elif case == "wrong-token":
        expected_token = "replacement"
    elif case == "wrong-hash":
        expected_digest = "0" * 64
    elif case == "malformed-token":
        expected_token = "bad\x00token"
    elif case == "uppercase-hash":
        expected_digest = digest.upper()
    elif case == "short-hash":
        expected_digest = digest[:-1]
    elif case == "same-token-raw-change":
        raw += b" \n"
        (sessions.registry / "foreign/agent.json").write_bytes(raw)
    with pytest.raises(AgentDeliveryError):
        sessions.stop("foreign", retire_dead_adoption=True,
                      recover_legacy_adoption=case == "legacy-overlap",
                      expected_token=expected_token, expected_record_sha256=expected_digest)
    assert (sessions.registry / "foreign/agent.json").read_bytes() == raw
    assert not (sessions.registry / "archive").exists()


@pytest.mark.parametrize("case", [
    "alive", "unknown", "missing-anchor", "old-anchor-rule", "bad-anchor",
    "managed", "custom", "headless", "invalid-cloud", "stopped",
])
def test_suggestion4_retirement_refuses_unproved_or_ineligible_records_without_runtime(
    case: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, _digest, _raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    path = sessions.registry / "foreign/agent.json"
    document = json.loads(path.read_bytes())
    if case == "missing-anchor":
        document["harness_identity"] = None
    elif case == "old-anchor-rule":
        document["anchor_rule"] = 1
    elif case == "bad-anchor":
        document["harness_identity"]["pid"] = 0
    elif case in ("managed", "custom"):
        document["adapter"] = "herdr" if case == "managed" else "herdr-pane"
    elif case in ("headless", "invalid-cloud"):
        document["mode"] = "headless"
        document["backend"] = "tmux" if case == "headless" else "agentcloud"
        document["adapter"] = "turn-runner" if case == "headless" else "agentcloud"
        document["foreign_shell_identity"] = None
        document["harness_identity"] = None
        document["anchor_rule"] = None
    elif case == "stopped":
        document["lifecycle"] = "stopped"
    path.write_text(json.dumps(document), encoding="utf-8")
    raw = path.read_bytes()
    if case == "headless":
        assert sessions.get("foreign").adapter == "turn-runner"
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: case if case in ("alive", "unknown") else "dead", raising=False)

    def no_worker(_record: AgentRecord, _action: str, **_options: object) -> dict[str, object]:
        raise AssertionError("dead-adoption retirement must not dispatch a worker")

    monkeypatch.setattr(sessions, "_worker", no_worker)
    with pytest.raises(AgentDeliveryError):
        sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                      expected_record_sha256=hashlib.sha256(raw).hexdigest())
    assert path.read_bytes() == raw and not (sessions.registry / "archive").exists()


@pytest.mark.parametrize("case", [
    "record", "liveness", "raw-during-proof", "directory", "queue", "queue-appeared",
    "lock", "archive-parent", "archive-collision",
])
def test_suggestion4_final_publication_rechecks_bound_state_and_preserves_replacements(
    case: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    directory = sessions.registry / "foreign"
    queue = directory / "queue"
    if case in ("queue", "lock"):
        agent.enqueue(str(queue), "saved work", message_id="saved")
    _suggestion4_no_runtime(fake, monkeypatch)
    checks = 0

    def liveness(_identity: CustomProcessIdentity) -> str:
        nonlocal checks
        checks += 1
        if checks == 2:
            if case in ("record", "raw-during-proof"):
                (directory / "agent.json").write_bytes(raw + b" \n")
            elif case == "liveness":
                return "alive"
            elif case == "directory":
                directory.rename(sessions.registry / "held-original")
                directory.mkdir(mode=0o700)
                (directory / "agent.json").write_bytes(raw)
                (directory / "agent.json").chmod(0o600)
            elif case == "queue":
                queue.rename(directory / "held-queue")
                queue.mkdir(mode=0o700)
            elif case == "queue-appeared":
                queue.mkdir(mode=0o700)
            elif case == "lock":
                replacement = queue / "new-lock"
                replacement.write_bytes(b"")
                replacement.chmod(0o600)
                os.replace(replacement, queue / ".binding.lock")
            elif case == "archive-parent":
                archive = sessions.registry / "archive"
                archive.rename(sessions.registry / "held-archive")
                archive.mkdir(mode=0o700)
            elif case == "archive-collision":
                target = sessions.registry / "archive" / f"foreign-{token}"
                target.mkdir(mode=0o700)
                (target / "replacement").write_bytes(b"keep this generation")
        return "dead"

    monkeypatch.setattr(fake, "process_liveness", liveness, raising=False)
    if case == "record":
        original_destination = sessions._archive_destination

        def destination(record: AgentRecord) -> tuple[Path, Path]:
            result = original_destination(record)
            (directory / "agent.json").write_bytes(raw + b"\n")
            return result

        monkeypatch.setattr(sessions, "_archive_destination", destination)
    with pytest.raises((AgentDeliveryError, OSError)):
        sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                      expected_record_sha256=digest)
    assert directory.is_dir()
    if case in ("record", "raw-during-proof"):
        assert (directory / "agent.json").read_bytes().startswith(raw)
    else:
        assert (directory / "agent.json").read_bytes() == raw
    archive = sessions.registry / "archive" / f"foreign-{token}"
    if case == "archive-collision":
        assert (archive / "replacement").read_bytes() == b"keep this generation"
        assert not (archive / "agent.json").exists()
    else:
        assert not archive.exists()
    if case == "directory":
        assert (sessions.registry / "held-original/agent.json").read_bytes() == raw
    elif case == "queue":
        assert (directory / "held-queue/inbox/saved.json").is_file()
    assert fake.closed == [] and fake.keys_sent == []


@pytest.mark.parametrize("case", ["reported-error", "active-replacement"])
def test_suggestion4_interrupted_publication_keeps_the_archived_generation_exact(
    case: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    directory = sessions.registry / "foreign"
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    real = subagents_module._rename_directory_noreplace_at

    def interrupted(source_fd: int, source: str, target_fd: int, target: str) -> None:
        real(source_fd, source, target_fd, target)
        if case == "reported-error":
            raise OSError("rename completed but wrapper failed")
        directory.mkdir(mode=0o700)
        (directory / "replacement").write_bytes(b"new generation")

    monkeypatch.setattr(subagents_module, "_rename_directory_noreplace_at", interrupted)
    with pytest.raises(AgentDeliveryError):
        sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                      expected_record_sha256=digest)
    archive = sessions.registry / "archive" / f"foreign-{token}"
    assert (archive / "agent.json").read_bytes() == raw
    assert not (archive / "output.json").exists()
    if case == "active-replacement":
        assert (directory / "replacement").read_bytes() == b"new generation"
    else:
        assert not directory.exists()
    assert fake.closed == [] and fake.submitted == []


@pytest.mark.parametrize("case", [
    "record-mode", "record-symlink", "record-hardlink", "record-fifo", "oversize",
    "queue-mode", "queue-symlink", "lock-mode", "lock-symlink", "lock-hardlink", "lock-fifo",
])
def test_suggestion4_retirement_refuses_unsafe_private_artifacts_without_runtime(
    case: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    directory = sessions.registry / "foreign"
    record_path = directory / "agent.json"
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    if case == "record-mode":
        record_path.chmod(0o644)
    elif case in ("record-symlink", "record-fifo"):
        record_path.rename(directory / "original-record")
        if case == "record-symlink":
            record_path.symlink_to("original-record")
        else:
            os.mkfifo(record_path, 0o600)
    elif case == "record-hardlink":
        os.link(record_path, directory / "record-alias")
    elif case == "oversize":
        document = json.loads(raw)
        document["large_unknown"] = "x" * subagents_module._MAX_AGENT_RECORD_BYTES
        raw = json.dumps(document).encode("utf-8")
        record_path.write_bytes(raw)
        digest = hashlib.sha256(raw).hexdigest()
    else:
        queue = directory / "queue"
        queue.mkdir(mode=0o700)
        if case == "queue-mode":
            queue.chmod(0o755)
        elif case == "queue-symlink":
            queue.rename(directory / "original-queue")
            queue.symlink_to("original-queue", target_is_directory=True)
        else:
            lock = queue / ".delivery.lock"
            if case == "lock-fifo":
                os.mkfifo(lock, 0o600)
            elif case == "lock-symlink":
                (queue / "original-lock").write_bytes(b"")
                (queue / "original-lock").chmod(0o600)
                lock.symlink_to("original-lock")
            else:
                lock.write_bytes(b"")
                lock.chmod(0o644 if case == "lock-mode" else 0o600)
                if case == "lock-hardlink":
                    os.link(lock, queue / "lock-alias")
    with pytest.raises(AgentDeliveryError):
        sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                      expected_record_sha256=digest)
    assert directory.is_dir() and not (sessions.registry / "archive").exists()
    assert fake.closed == [] and fake.keys_sent == []


def test_suggestion4_queue_waiter_cannot_lock_a_replaced_generation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    directory = sessions.registry / "foreign"
    queue = directory / "queue"
    agent.enqueue(str(queue), "saved work", message_id="saved")
    delivery_inode = (queue / ".delivery.lock").stat().st_ino
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    real = fcntl.flock
    swapped = False

    def flock(descriptor: int, operation: int) -> None:
        nonlocal swapped
        if not swapped and os.fstat(descriptor).st_ino == delivery_inode:
            swapped = True
            queue.rename(directory / "held-queue")
            queue.mkdir(mode=0o700)
            (queue / ".delivery.lock").write_bytes(b"replacement coordination")
            (queue / ".delivery.lock").chmod(0o600)
        real(descriptor, operation)

    monkeypatch.setattr(fcntl, "flock", flock)
    with pytest.raises(AgentDeliveryError, match="queue.*changed"):
        sessions.stop("foreign", retire_dead_adoption=True, expected_token=token,
                      expected_record_sha256=digest)
    assert swapped and (directory / "agent.json").read_bytes() == raw
    assert (directory / "held-queue/inbox/saved.json").is_file()
    assert (queue / ".delivery.lock").read_bytes() == b"replacement coordination"
    assert not (queue / ".binding.lock").exists()
    assert not (sessions.registry / "archive").exists()


@pytest.mark.parametrize("transaction", ["rename", "move", "revive"])
@pytest.mark.parametrize("explicit", [False, True])
def test_suggestion4_pending_transactions_retain_authority_before_any_runtime_access(
    transaction: str, explicit: bool, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    record = sessions.get("foreign")
    if transaction == "move":
        sessions._write_move_intent(record, "w2")
    elif transaction == "rename":
        sessions._write_rename_journal({
            "schema": "agentctl-rename/v1", "token": token, "old": "foreign", "new": "reviewer",
            "adapter": record.adapter, "pane_id": record.pane_id, "tab_id": record.tab_id,
            "terminal_id": record.terminal_id, "workspace_id": record.workspace_id,
            "journal_id": "a" * 32, "started_at": 0.0,
        })
    else:
        journal_directory = sessions.registry / ".revives"
        journal_directory.mkdir(mode=0o700)
        metadata = (sessions.registry / "foreign").stat()
        agent._atomic_json(str(journal_directory / f"{token}.json"), {
            "schema": "agentctl-revive/v1", "name": "foreign", "old_token": token,
            "new_token": "new-generation", "phase": "launching", "started_at": 0.0,
            "old_directory_device": metadata.st_dev, "old_directory_inode": metadata.st_ino,
            "new_directory_device": metadata.st_dev, "new_directory_inode": metadata.st_ino + 1,
            "old_record_sha256": digest, "stopped_record_sha256": "0" * 64,
            "ready_record_sha256": None,
            "proof": {
                "kind": "missing", "pane_id": record.pane_id, "tab_id": record.tab_id,
                "workspace_id": record.workspace_id, "cwd": record.cwd,
                "terminal_id": None, "reported_agent": None, "reported_session_agent": None,
                "reported_session_value": None, "shell": None, "shell_executable": None,
            },
        })
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    with pytest.raises(AgentDeliveryError, match="incomplete") as refused:
        sessions.stop("foreign", retire_dead_adoption=explicit, expected_token=token,
                      expected_record_sha256=digest if explicit else None)
    action = refused.value.recovery_action
    assert action is not None and action.command == transaction and action.token == token
    assert (sessions.registry / "foreign/agent.json").read_bytes() == raw
    assert not (sessions.registry / "archive").exists()


@pytest.mark.parametrize("transaction", ["rename", "move", "revive"])
def test_suggestion4_late_corrupt_transaction_refuses_before_optional_advice_or_rpc(
    transaction: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, _token, _digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    original = sessions._dead_adoption_pending

    def pending(record: AgentRecord) -> None:
        if transaction == "move":
            path = sessions.registry / "foreign/move.json"
        else:
            directory = sessions.registry / (".renames" if transaction == "rename" else ".revives")
            directory.mkdir(mode=0o700)
            path = directory / f"{record.token}.json"
        agent._atomic_json(str(path), {})
        original(record)

    monkeypatch.setattr(sessions, "_dead_adoption_pending", pending)
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    with pytest.raises(AgentDeliveryError) as refused:
        sessions.stop("foreign")
    assert refused.value.recovery_action == RecoveryAction("doctor")
    assert (sessions.registry / "foreign/agent.json").read_bytes() == raw
    assert not (sessions.registry / "archive").exists()


@pytest.mark.parametrize("change", ["token", "raw-bytes"])
def test_suggestion4_captured_advice_cannot_retire_a_replacement_and_replays_exact_original(
    change: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    path = sessions.registry / "foreign/agent.json"
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)

    def unavailable(_workspace_id: str | None = None) -> tuple[Pane, ...]:
        # A later runtime probe cannot supply the replacement's token or digest.
        document = json.loads(raw)
        if change == "token":
            document["token"] = "replacement-generation"
        else:
            document["future_field"]["nested"].append("replacement bytes")
        path.write_bytes(json.dumps(document).encode("utf-8"))
        raise HerdrUnavailable("Herdr server is offline")

    monkeypatch.setattr(fake, "panes", unavailable)
    with pytest.raises(HerdrUnavailable, match="server is offline") as refused:
        sessions.stop("foreign")
    assert refused.value.exit_code == 69
    assert refused.value.recovery_action == RecoveryAction(
        "retire-dead-adoption", name="foreign", token=token, record_sha256=digest,
    )
    message = stop_refusal_message(refused.value, prefix="agentctl", registry=str(sessions.registry),
                                   herdr_bin="missing herdr's $(literal)")
    command = shlex.split(message.split("Recovery command: ", 1)[1])
    assert command == [
        "agentctl", "--registry=" + str(sessions.registry), "--herdr-bin=missing herdr's $(literal)",
        "stop", "foreign", "--retire-dead-adoption", "--expected-token=" + token,
        "--expected-record-sha256=" + digest,
    ]
    replacement = path.read_bytes()
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(cli, "Sessions", lambda *_args, **_kwargs: sessions)
    assert cli.main(command[1:]) == 75
    refused_output = capsys.readouterr()
    assert shlex.split(refused_output.err.split("Recovery command: ", 1)[1])[-1] == "doctor"
    assert path.read_bytes() == replacement and not (sessions.registry / "archive").exists()
    path.write_bytes(raw)
    assert cli.main(command[1:]) == 0
    completed = json.loads(capsys.readouterr().out)
    assert completed["retired_dead_adoption"] is True
    assert (Path(completed["archive"]) / "agent.json").read_bytes() == raw


@pytest.mark.parametrize("proof", ["alive", "unknown", "error", "identity-lock-error"])
def test_suggestion4_optional_advice_failure_preserves_successful_ordinary_stop(
    proof: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    sessions, fake, _token, _digest, _raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)

    def liveness(_identity: CustomProcessIdentity) -> str:
        if proof == "error":
            raise PermissionError("kernel proof unavailable")
        return proof

    monkeypatch.setattr(fake, "process_liveness", liveness, raising=False)
    if proof == "identity-lock-error":
        original = agent._open_private_lock

        def open_lock(path: str, purpose: str) -> int:
            if path.endswith("/.identity.lock"):
                raise AgentDeliveryError("supplemental identity lock unavailable")
            return original(path, purpose)

        monkeypatch.setattr(agent, "_open_private_lock", open_lock)
    result = sessions.stop("foreign")
    assert result["runtime_preserved"] is True and "retired_dead_adoption" not in result
    archive = Path(str(result["archive"]))
    assert json.loads((archive / "agent.json").read_bytes())["lifecycle"] == "stopped"
    assert (archive / "output.json").is_file() and fake.closed == []


@pytest.mark.parametrize("digest", [None, "0" * 63, "A" * 64, "0" * 63 + "\x00"])
def test_suggestion4_formatter_refuses_missing_or_malformed_new_digest_authority(
    digest: str | None,
) -> None:
    error = _with_recovery(AgentDeliveryError("refused"), RecoveryAction(
        "retire-dead-adoption", name="foreign", token="original", record_sha256=digest,
    ))
    command = stop_refusal_message(error, prefix="agentctl", registry="records", herdr_bin="herdr")
    assert shlex.split(command.split("Recovery command: ", 1)[1])[-1] == "doctor"


def test_suggestion4_legacy_digest_advice_keeps_the_distinct_existing_recovery_mode() -> None:
    digest = "0" * 64
    error = _with_recovery(AgentDeliveryError("legacy refusal"), RecoveryAction(
        "stop", name="foreign", token="original", record_sha256=digest,
    ))
    command = stop_refusal_message(error, prefix="agentctl", registry="records", herdr_bin="herdr")
    argv = shlex.split(command.split("Recovery command: ", 1)[1])
    assert "--recover-legacy-adoption" in argv and "--retire-dead-adoption" not in argv
    assert "--expected-record-sha256=" + digest in argv


def test_suggestion4_older_cli_retirement_and_stop_only_boundary_use_real_private_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    sessions, fake, token, digest, raw = _suggestion4_dead_adoption(tmp_path, monkeypatch)
    _suggestion4_no_runtime(fake, monkeypatch)
    monkeypatch.setattr(fake, "process_liveness", lambda _identity: "dead", raising=False)
    monkeypatch.setattr(legacy_cli, "HerdrClient", lambda **_kwargs: cast(HerdrClient, fake))
    globals_ = ["--registry=" + str(sessions.registry), "--herdr-bin=offline-herdr"]
    assert legacy_cli.main([*globals_, "status", "foreign", "--retire-dead-adoption"]) == 2
    assert "--retire-dead-adoption is valid only with stop" in capsys.readouterr().err
    assert (sessions.registry / "foreign/agent.json").read_bytes() == raw
    assert not (sessions.registry / "archive").exists()
    assert legacy_cli.main([
        *globals_, "stop", "foreign", "--retire-dead-adoption",
        "--expected-token=" + token, "--expected-record-sha256=" + digest,
    ]) == 0
    captured = capsys.readouterr()
    assert captured.err == ""
    result = json.loads(captured.out)
    assert result["retired_dead_adoption"] is True and result["record_sha256"] == digest
    assert (Path(result["archive"]) / "agent.json").read_bytes() == raw
    assert not (Path(result["archive"]) / "queue").exists()
