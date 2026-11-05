"""Focused contract tests for the shared interactive-agent queue."""

from __future__ import annotations

import errno
import fcntl
import json
import os
import stat
import threading
from pathlib import Path
from typing import cast

import pytest

from agentctl import __version__
from agentctl import cli as primary_cli
import agentctl.agent as agent_api
from agentctl.agent import Target, drain, enqueue, read, send, status
from agentctl.agent import QueueResult
import agentctl.legacy_cli as agent_cli
from agentctl.client import AgentPaneInfo, HerdrClient, Pane
from agentctl.errors import AgentDeliveryError, AgentPending, AgentPossiblySubmitted, HerdrUnavailable


def test_agent_cli_version(capsys: pytest.CaptureFixture[str]) -> None:
    with pytest.raises(SystemExit) as exc_info:
        agent_cli.main(["--version"])
    assert exc_info.value.code == 0
    assert capsys.readouterr().out == f"herdr-agent {__version__}\n"


def test_agent_cli_userguide_option(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(agent_cli, "_guide", lambda: 0)
    assert agent_cli.main(["--userguide"]) == 0


def test_agent_cli_bare_invocation_is_a_successful_orientation(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert agent_cli.main([]) == 0
    captured = capsys.readouterr()
    assert "send" in captured.out
    assert "--session" in captured.out
    assert captured.err == ""


@pytest.mark.parametrize(
    "arguments",
    [
        ["status", "--pane", "p", "--ready-timeout", "nan"],
        ["status", "--pane", "p", "--working-timeout", "inf"],
        ["status", "--pane", "p", "--ready-timeout", "31536001"],
    ],
)
def test_agent_cli_rejects_nonfinite_or_excessive_waits(arguments: list[str]) -> None:
    with pytest.raises(SystemExit) as exc_info:
        agent_cli.main(arguments)
    assert exc_info.value.code == 2


@pytest.mark.parametrize(
    "arguments",
    [
        ["status", "--pane", "--help"],
        ["status", "--pane", "--version"],
        ["status", "--pane", "--queue", "state"],
        ["status", "--lines", "1_0"],
        ["status", "--lines", "١٢"],
        ["status", "--lines", "1000001"],
        ["status", "--ready-timeout", "1_0"],
        ["status", "--ready-timeout", "١.0"],
    ],
)
def test_agent_cli_rejects_stolen_options_and_non_ascii_or_unbounded_numbers(
    arguments: list[str],
) -> None:
    with pytest.raises(SystemExit) as exc_info:
        agent_cli.main(arguments)
    assert exc_info.value.code == 2


def test_agent_cli_accepts_documented_ascii_numeric_forms() -> None:
    args = agent_cli._parser().parse_intermixed_args(
        ["status", "--pane=p", "--ready-timeout=.5", "--lines=00017"]
    )
    assert args.ready_timeout == 0.5
    assert args.lines == 17


class FakeAgentHerdr:
    def __init__(self, states: list[str] | None = None) -> None:
        self.states = states or ["idle"]
        self.index = 0
        self.runs: list[str] = []
        self.waits: list[tuple[str, str, int]] = []
        self.workspace = "acme"
        self.cwd = "/work/mtg"
        self.session = "session-1"
        self.read_text = "agent transcript\n"
        self.run_entered = threading.Event()
        self.run_release: threading.Event | None = None

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        del workspace_id
        return (Pane("w1:p1", "w1:t1", "w1"),)

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        assert pane_id == "w1:p1"
        state = self.states[min(self.index, len(self.states) - 1)]
        self.index += 1
        return AgentPaneInfo(pane_id, "w1", self.cwd, "codex", state, "codex", self.session)

    def workspace_label(self, workspace_id: str) -> str:
        assert workspace_id == "w1"
        return self.workspace

    def prompt_agent(self, pane_id: str, text: str) -> None:
        assert pane_id == "w1:p1"
        self.run_entered.set()
        if self.run_release is not None:
            assert self.run_release.wait(5)
        self.runs.append(text)

    def wait_agent_status(self, pane_id: str, state: str, timeout_ms: int) -> None:
        self.waits.append((pane_id, state, timeout_ms))
        if state != "working":
            raise AssertionError(state)

    def read(self, pane_id: str, *, source: str, lines: int) -> str:
        assert pane_id == "w1:p1" and source in ("recent-unwrapped", "recent") and lines == 17
        return self.read_text


def client(fake: FakeAgentHerdr) -> HerdrClient:
    return cast(HerdrClient, fake)


def target(**changes: str) -> Target:
    values = {
        "pane_id": "w1:p1", "session_agent": "codex", "session_value": "session-1",
        "expected_agent": "codex", "expected_workspace": "acme", "expected_cwd": "/work/mtg",
    }
    values.update(changes)
    return Target(**values)


def test_enqueue_artifact_limit_counts_serialized_bytes_before_temporary_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("agentctl.agent.time.time", lambda: 123.0)
    text = "\x00😀\"\\" * 100
    expected = (json.dumps({
        "id": "bounded", "text": text, "queued_at": 123.0, "delivery_attempts": 0,
    }, indent=2, sort_keys=True) + "\n").encode("utf-8")

    def unexpected_temporary(*args: object, **kwargs: object) -> None:
        raise AssertionError("oversized JSON must be rejected before any temporary file is opened")

    with monkeypatch.context() as scoped:
        scoped.setattr("agentctl.agent.tempfile.NamedTemporaryFile", unexpected_temporary)
        with pytest.raises(AgentDeliveryError, match="exceeding max_artifact_bytes"):
            enqueue(str(tmp_path), text, message_id="bounded", max_artifact_bytes=len(expected) - 1)
    assert list((tmp_path / "inbox").iterdir()) == []
    assert enqueue(str(tmp_path), text, message_id="bounded", max_artifact_bytes=len(expected)) == "bounded"
    assert (tmp_path / "inbox/bounded.json").read_bytes() == expected


def test_bounded_drain_refuses_open_artifact_growth_before_prompt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake = FakeAgentHerdr()
    limit = 4096
    enqueue(str(tmp_path), "bounded prompt", message_id="growing",
            max_artifact_bytes=limit)
    artifact = tmp_path / "inbox" / "growing.json"
    original_read = os.read
    grew = False

    def grow_after_first_read(descriptor: int, count: int) -> bytes:
        nonlocal grew
        block = original_read(descriptor, count)
        try:
            opened = os.readlink(f"/proc/self/fd/{descriptor}")
        except OSError:
            opened = ""
        if not grew and opened == str(artifact):
            grew = True
            append = os.open(artifact, os.O_WRONLY | os.O_APPEND)
            try:
                os.write(append, b"x" * (limit + 1))
            finally:
                os.close(append)
        return block

    monkeypatch.setattr(os, "read", grow_after_first_read)
    with pytest.raises(AgentDeliveryError, match="exceeds max_artifact_bytes"):
        drain(client(fake), target(), str(tmp_path), ready_timeout=0,
              max_artifact_bytes=limit)
    assert grew and fake.runs == []


@pytest.mark.parametrize("limit", [0, -1, True, 1.5])
def test_queue_rejects_invalid_artifact_limits_without_creating_queue(tmp_path: Path, limit: object) -> None:
    root = tmp_path / "queue"
    with pytest.raises(AgentDeliveryError, match="positive integer"):
        enqueue(str(root), "prompt", max_artifact_bytes=cast(int, limit))
    with pytest.raises(AgentDeliveryError, match="positive integer"):
        drain(client(FakeAgentHerdr()), target(), str(root), max_artifact_bytes=cast(int, limit))
    assert not root.exists()


@pytest.mark.parametrize("error_text", ["\x00" * 9000, "é" * 9000, "😀" * 9000])
def test_bounded_pending_diagnostics_fit_update_reservation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, error_text: str,
) -> None:
    fake = FakeAgentHerdr()

    def unavailable(_pane_id: str) -> AgentPaneInfo:
        raise HerdrUnavailable(error_text)

    monkeypatch.setattr(fake, "pane_info", unavailable)
    text = "multiline\n\x00😀"
    limit = agent_api.queue_artifact_reservation_bytes(text, message_id="bounded")
    enqueue(str(tmp_path), text, message_id="bounded", max_artifact_bytes=limit)
    original_size = (tmp_path / "inbox/bounded.json").stat().st_size
    result = drain(client(fake), target(), str(tmp_path), max_artifact_bytes=limit)
    assert result.outcome == "pending"
    assert result.blocked == error_text.encode("utf-8")[:agent_api.QUEUE_ERROR_MAX_BYTES].decode("utf-8")
    assert len(result.blocked.encode("utf-8")) == agent_api.QUEUE_ERROR_MAX_BYTES
    artifact = tmp_path / "inbox/bounded.json"
    document = json.loads(artifact.read_text())
    assert document["delivery_error"] == result.blocked
    assert len(document["delivery_error"].encode("utf-8")) <= agent_api.QUEUE_ERROR_MAX_BYTES
    assert document["text"] == text
    assert document["delivery_attempts"] == 0
    assert artifact.stat().st_size <= limit
    assert artifact.stat().st_size <= original_size + agent_api.QUEUE_UPDATE_MAX_BYTES
    assert fake.runs == []


@pytest.mark.parametrize("bounded", [False, True])
def test_failed_diagnostics_and_sidecar_are_capped_only_for_bounded_callers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, bounded: bool,
) -> None:
    fake = FakeAgentHerdr()
    error_text = "\x00" * 9000

    def unavailable(_pane_id: str, _state: str, _timeout: int) -> None:
        raise HerdrUnavailable(error_text)

    monkeypatch.setattr(fake, "wait_agent_status", unavailable)
    text = "at most once"
    limit = agent_api.queue_artifact_reservation_bytes(text, message_id="bounded") if bounded else None
    with pytest.raises(AgentPossiblySubmitted) as failure:
        send(client(fake), target(), str(tmp_path), text, message_id="bounded", max_artifact_bytes=limit)
    artifact = tmp_path / "failed/bounded.json"
    sidecar = tmp_path / "failed/bounded.json.error"
    document = json.loads(artifact.read_text())
    metadata = json.loads(sidecar.read_text())
    if bounded:
        assert limit is not None
        assert len(str(failure.value).encode("utf-8")) == agent_api.QUEUE_ERROR_MAX_BYTES
        assert len(document["delivery_error"].encode("utf-8")) <= agent_api.QUEUE_ERROR_MAX_BYTES
        assert len(metadata["error"].encode("utf-8")) <= agent_api.QUEUE_ERROR_MAX_BYTES
        assert artifact.stat().st_size <= limit
        assert sidecar.stat().st_size <= min(limit, agent_api.QUEUE_ERROR_SIDECAR_MAX_BYTES)
    else:
        assert error_text in str(failure.value)
        assert document["delivery_error"].endswith(error_text)
        assert metadata["error"].endswith(error_text)
    assert fake.runs == [text]
    restarted = FakeAgentHerdr()
    drain(client(restarted), target(), str(tmp_path), max_artifact_bytes=limit)
    assert restarted.runs == []


@pytest.mark.parametrize("bounded", [False, True])
def test_pending_send_caps_entire_exception_only_for_bounded_callers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, bounded: bool,
) -> None:
    fake = FakeAgentHerdr()
    error_text = "x" * 9000

    def unavailable(_pane_id: str) -> AgentPaneInfo:
        raise HerdrUnavailable(error_text)

    monkeypatch.setattr(fake, "pane_info", unavailable)
    text = "pending prompt"
    limit = agent_api.queue_artifact_reservation_bytes(text, message_id="bounded") if bounded else None
    with pytest.raises(AgentPending) as failure:
        send(client(fake), target(), str(tmp_path), text, message_id="bounded", max_artifact_bytes=limit)
    assert type(failure.value) is AgentPending
    if bounded:
        assert len(str(failure.value).encode("utf-8")) == agent_api.QUEUE_ERROR_MAX_BYTES
    else:
        assert str(failure.value).endswith(error_text)
    assert fake.runs == []


@pytest.mark.parametrize("bounded", [False, True])
def test_exhausted_head_caps_returned_diagnostic_only_for_bounded_callers(tmp_path: Path, bounded: bool) -> None:
    enqueue(str(tmp_path), "prompt", message_id="exhausted")
    artifact = tmp_path / "inbox/exhausted.json"
    document = json.loads(artifact.read_text())
    document.update({"id": "x" * 9000, "delivery_attempts": 1})
    artifact.write_text(json.dumps(document), encoding="utf-8")
    limit = 100000 if bounded else None
    result = drain(client(FakeAgentHerdr()), target(), str(tmp_path), max_attempts=1, max_artifact_bytes=limit)
    assert result.outcome == "pending"
    assert result.blocked is not None
    assert json.loads(artifact.read_text())["delivery_error"] == result.blocked
    if bounded:
        assert len(result.blocked.encode("utf-8")) == agent_api.QUEUE_ERROR_MAX_BYTES
    else:
        assert "x" * 9000 in result.blocked


@pytest.mark.parametrize("bounded", [False, True])
def test_binding_mismatch_caps_exception_only_for_bounded_callers(tmp_path: Path, bounded: bool) -> None:
    drain(client(FakeAgentHerdr()), target(), str(tmp_path))
    expected_workspace = "x" * 9000
    with pytest.raises(AgentDeliveryError) as failure:
        drain(
            client(FakeAgentHerdr()), target(expected_workspace=expected_workspace), str(tmp_path),
            max_artifact_bytes=100000 if bounded else None,
        )
    if bounded:
        assert len(str(failure.value).encode("utf-8")) == agent_api.QUEUE_ERROR_MAX_BYTES
    else:
        assert expected_workspace in str(failure.value)


def test_oversized_inflight_update_preserves_pending_artifact_before_submission(tmp_path: Path) -> None:
    text = "x" * 1000
    enqueue(str(tmp_path), text, message_id="bounded")
    artifact = tmp_path / "inbox/bounded.json"
    original = artifact.read_bytes()
    fake = FakeAgentHerdr()
    with pytest.raises(AgentDeliveryError, match="exceeding max_artifact_bytes"):
        drain(client(fake), target(), str(tmp_path), max_artifact_bytes=len(original))
    assert artifact.read_bytes() == original
    assert list((tmp_path / "inbox").iterdir()) == [artifact]
    assert list((tmp_path / "inflight").iterdir()) == []
    assert fake.runs == []


@pytest.mark.parametrize("confirmed", [False, True])
def test_oversized_post_submission_update_retains_inflight_barrier(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, confirmed: bool,
) -> None:
    monkeypatch.setattr("agentctl.agent.time.time", lambda: 123.0)
    text = "x" * 1000
    enqueue(str(tmp_path), text, message_id="bounded")
    document = json.loads((tmp_path / "inbox/bounded.json").read_text())
    document.update({"possibly_submitted": True, "delivery_state": "inflight", "inflight_at": 123.0})
    inflight_bytes = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8")
    fake = FakeAgentHerdr()

    def unavailable(_pane_id: str, _state: str, _timeout: int) -> None:
        raise HerdrUnavailable("confirmation unavailable")

    if not confirmed:
        monkeypatch.setattr(fake, "wait_agent_status", unavailable)
    with pytest.raises(AgentPossiblySubmitted, match="exceeding max_artifact_bytes") as failure:
        drain(client(fake), target(), str(tmp_path), max_artifact_bytes=len(inflight_bytes))
    artifact = tmp_path / "inflight/bounded.json"
    assert type(failure.value) is AgentPossiblySubmitted
    assert failure.value.outcome == "possibly_submitted"
    assert failure.value.message_id == "bounded"
    assert failure.value.artifact == str(artifact)
    assert artifact.read_bytes() == inflight_bytes
    assert list((tmp_path / "inflight").iterdir()) == [artifact]
    assert fake.runs == [text]
    restarted = FakeAgentHerdr()
    result = drain(client(restarted), target(), str(tmp_path), max_artifact_bytes=len(inflight_bytes))
    assert result.outcome == "possibly_submitted"
    assert (tmp_path / "failed/bounded.json").read_bytes() == inflight_bytes
    assert restarted.runs == []


def test_oversized_failure_sidecar_preserves_original_raw_artifact(tmp_path: Path) -> None:
    inbox, _inflight, _processed, failed = agent_api._prepare(str(tmp_path))
    source = Path(inbox) / "invalid.json"
    source.write_bytes(b"invalid")
    with pytest.raises(AgentDeliveryError, match="exceeding max_artifact_bytes"):
        agent_api._quarantine_raw(
            str(source), failed, outcome="invalid_message", error="\x00" * 9000, max_artifact_bytes=100,
        )
    assert source.read_bytes() == b"invalid"
    assert list(Path(inbox).iterdir()) == [source]
    assert list(Path(failed).iterdir()) == []
    limit = agent_api.QUEUE_ERROR_SIDECAR_MAX_BYTES
    assert agent_api._quarantine_raw(
        str(source), failed, outcome="invalid_message", error="\x00" * 9000, max_artifact_bytes=limit,
    ) == "invalid"
    assert list(Path(inbox).iterdir()) == []
    assert (Path(failed) / source.name).read_bytes() == b"invalid"
    sidecar = Path(failed) / f"{source.name}.error"
    assert sidecar.stat().st_size <= limit
    assert json.loads(sidecar.read_text())["outcome"] == "invalid_message"


def test_sidecar_disk_full_after_quarantine_keeps_raw_bytes_and_never_resubmits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    inbox, _inflight, _processed, failed = agent_api._prepare(str(tmp_path))
    source = Path(inbox) / "invalid.json"
    source.write_bytes(b"invalid")

    def disk_full(
        path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
    ) -> None:
        raise OSError(errno.ENOSPC, "injected full disk", path)

    with monkeypatch.context() as scoped:
        scoped.setattr(agent_api, "_atomic_json", disk_full)
        with pytest.raises(OSError) as failure:
            agent_api._quarantine_raw(
                str(source), failed, outcome="invalid_message", error="invalid JSON",
                max_artifact_bytes=agent_api.QUEUE_ERROR_SIDECAR_MAX_BYTES,
            )
    assert failure.value.errno == errno.ENOSPC
    destination = Path(failed) / source.name
    assert destination.read_bytes() == b"invalid"
    assert not Path(f"{destination}.error").exists()
    assert list(Path(inbox).iterdir()) == []
    restarted = FakeAgentHerdr()
    drain(client(restarted), target(), str(tmp_path), max_artifact_bytes=agent_api.QUEUE_ERROR_SIDECAR_MAX_BYTES)
    assert destination.read_bytes() == b"invalid"
    assert restarted.runs == []


def test_bounded_target_binding_rejects_oversized_record_before_write(tmp_path: Path) -> None:
    fake = FakeAgentHerdr()
    with pytest.raises(AgentDeliveryError, match="exceeding max_artifact_bytes"):
        drain(client(fake), target(), str(tmp_path), max_artifact_bytes=10)
    assert not (tmp_path / "target.json").exists()
    assert list(tmp_path.glob(".message.*")) == []
    assert fake.runs == []


def test_oversized_atomic_replacement_preserves_existing_bytes_without_temporary_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    artifact = tmp_path / "message.json"
    original = b'{"original":"untouched"}\n'
    artifact.write_bytes(original)

    def unexpected_temporary(*args: object, **kwargs: object) -> None:
        raise AssertionError("oversized replacement must be rejected before a temporary file is opened")

    monkeypatch.setattr("agentctl.agent.tempfile.NamedTemporaryFile", unexpected_temporary)
    with pytest.raises(AgentDeliveryError, match="exceeding max_artifact_bytes"):
        agent_api._atomic_json(str(artifact), {"text": "\x00" * 1000}, max_artifact_bytes=1000)
    assert artifact.read_bytes() == original
    assert list(tmp_path.glob(".message.*")) == []


def test_bounded_drain_preserves_oversized_existing_artifact_without_injection(tmp_path: Path) -> None:
    enqueue(str(tmp_path), "x" * 1000, message_id="bounded")
    artifact = tmp_path / "inbox/bounded.json"
    original = artifact.read_bytes()
    fake = FakeAgentHerdr()
    with pytest.raises(AgentDeliveryError, match="exceeds max_artifact_bytes"):
        drain(client(fake), target(), str(tmp_path), max_artifact_bytes=len(original) - 1)
    assert artifact.read_bytes() == original
    assert list((tmp_path / "inflight").iterdir()) == []
    assert list((tmp_path / "failed").iterdir()) == []
    assert fake.runs == []


def test_multiline_busy_then_idle_is_atomic_and_confirmed(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["working", "working", "idle"])
    text = "first line\nsecond line\nthird line"
    result = send(client(fake), target(), str(tmp_path), text, ready_timeout=1, sleep=lambda _s: None)
    assert fake.runs == [text]
    assert fake.waits == [("w1:p1", "working", 30000)]
    assert result.message_id in result.delivered


def test_busy_for_more_than_thirty_seconds_still_delivers_with_turn_sized_budget(tmp_path: object) -> None:
    # Session resolution reads the matching pane once while listing and once for final
    # validation, both before and after acquiring the canonical pane lock.
    fake = FakeAgentHerdr(["working", "working", "working", "working", "idle", "idle"])
    clock = {"now": 0.0}

    def monotonic() -> float:
        return clock["now"]

    def sleep(_seconds: float) -> None:
        clock["now"] += 31.0

    result = send(
        client(fake), target(), str(tmp_path), "after a normal turn",
        ready_timeout=900, sleep=sleep, monotonic=monotonic,
    )
    assert clock["now"] > 30
    assert result.delivered == (result.message_id,)


def test_done_is_submit_safe(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["done"])
    send(client(fake), target(), str(tmp_path), "from done")
    assert fake.runs == ["from done"]


def test_concurrent_senders_are_serialized_and_fifo(tmp_path: object) -> None:
    root = str(tmp_path)
    fake = FakeAgentHerdr(["idle"])
    fake.run_release = threading.Event()
    errors: list[BaseException] = []

    def invoke(text: str) -> None:
        try:
            send(client(fake), target(), root, text)
        except BaseException as exc:
            errors.append(exc)

    first = threading.Thread(target=invoke, args=("first",), daemon=True)
    second = threading.Thread(target=invoke, args=("second",), daemon=True)
    first.start()
    assert fake.run_entered.wait(5)
    second.start()
    second.join(0.05)
    assert second.is_alive()
    fake.run_release.set()
    first.join(5)
    second.join(5)
    assert errors == []
    assert fake.runs == ["first", "second"]


def test_distinct_queue_roots_serialize_one_shared_target(tmp_path: Path) -> None:
    fake = FakeAgentHerdr(["idle"])
    fake.run_release = threading.Event()
    errors: list[BaseException] = []

    def invoke(root: Path, text: str) -> None:
        try:
            send(client(fake), target(), str(root), text)
        except BaseException as exc:
            errors.append(exc)

    first = threading.Thread(target=invoke, args=(tmp_path / "queue-a", "from a"), daemon=True)
    second = threading.Thread(target=invoke, args=(tmp_path / "queue-b", "from b"), daemon=True)
    first.start()
    assert fake.run_entered.wait(5)
    second.start()
    second.join(0.05)
    assert second.is_alive(), "a different queue root must wait on the host-wide target lock"
    assert fake.runs == []
    fake.run_release.set()
    first.join(5)
    second.join(5)
    assert errors == []
    assert fake.runs == ["from a", "from b"]


def test_exact_pane_and_session_forms_share_one_target_lock(tmp_path: Path) -> None:
    fake = FakeAgentHerdr(["idle"])
    fake.run_release = threading.Event()
    errors: list[BaseException] = []
    pane_target = Target(pane_id="w1:p1")
    session_target = Target(session_agent="codex", session_value="session-1")

    def invoke(root: Path, selected: Target, text: str) -> None:
        try:
            send(client(fake), selected, str(root), text)
        except BaseException as exc:
            errors.append(exc)

    first = threading.Thread(
        target=invoke, args=(tmp_path / "pane-queue", pane_target, "from pane"), daemon=True
    )
    second = threading.Thread(
        target=invoke,
        args=(tmp_path / "session-queue", session_target, "from session"),
        daemon=True,
    )
    first.start()
    assert fake.run_entered.wait(5)
    second.start()
    second.join(0.05)
    assert second.is_alive(), "session targeting bypassed the resolved-pane lock"
    fake.run_release.set()
    first.join(5)
    second.join(5)
    assert errors == []
    assert not first.is_alive() and not second.is_alive()
    assert fake.runs == ["from pane", "from session"]


def test_host_target_locks_live_in_account_state_not_a_cleaned_temporary_directory() -> None:
    # The host's daily cleaner deletes files under these directories by modification time,
    # and a lock file is never written, so a held lock there was deleted once it was four days
    # old and a second sender locked a fresh file at the same path.
    import pwd

    host, legacy = agent_api._host_target_lock_roots()
    home = pwd.getpwuid(os.getuid()).pw_dir
    assert host == os.path.join(home, ".local", "state", "agentctl", "target-locks")
    for cleaned in ("/tmp", "/var/tmp", "/dev/shm"):
        assert not (host == cleaned or host.startswith(cleaned + "/")), host
    # Earlier editions keep the legacy name, so new editions must keep locking it too.
    assert legacy == f"/tmp/herdr-agent-target-locks-{os.getuid()}"


def test_a_relative_account_home_is_refused() -> None:
    # Senders started in different directories would resolve a relative home to different
    # lock files and stop excluding each other.
    with pytest.raises(AgentDeliveryError, match="not an absolute path"):
        agent_api._target_lock_root_under("relative-home")
    assert (
        agent_api._target_lock_root_under("/home/someone")
        == "/home/someone/.local/state/agentctl/target-locks"
    )


def test_a_lock_file_unlinked_while_waited_on_is_not_held(tmp_path: Path) -> None:
    # The cleaner unlinks the file while this sender waits on it, and a sender that takes only
    # the /tmp file then locks a fresh file at the same path. Taking the lock on the unlinked
    # file would let both senders in; this sender must contend on the new file.
    import fcntl

    path = str(tmp_path / "target.lock")
    replacement: list[int] = []
    waits = 0

    def wait(descriptor: int, lock_path: str) -> None:
        nonlocal waits
        waits += 1
        if waits == 1:
            os.unlink(lock_path)
            other = agent_api._open_private_lock(lock_path, "replacement lock")
            fcntl.flock(other, fcntl.LOCK_EX)
            replacement.append(other)
        else:
            with pytest.raises(BlockingIOError):
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            os.close(replacement.pop())
        fcntl.flock(descriptor, fcntl.LOCK_EX)

    held: list[int] = []
    try:
        agent_api._lock_current_file(path, "test target lock", held, wait)
        assert waits == 2 and len(held) == 1
        current, locked = os.stat(path), os.fstat(held[0])
        assert (current.st_dev, current.st_ino) == (locked.st_dev, locked.st_ino)
    finally:
        for descriptor in held:
            os.close(descriptor)


@pytest.mark.parametrize("failing", [0, 1], ids=["account-root", "legacy-root"])
def test_a_failed_acquisition_releases_every_file_it_locked(failing: int) -> None:
    # The descriptor belongs to the lock from the moment it is opened, so a failure after a
    # file is locked (here, in the wait itself) releases it instead of leaking a held lock that
    # would block every later sender for the life of the process.
    import fcntl

    name = "failed-acquisition"
    paths = agent_api._target_lock_paths(name)
    calls = 0

    def wait(descriptor: int, _path: str) -> None:
        nonlocal calls
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        if calls == failing:
            raise RuntimeError("injected failure after locking")
        calls += 1

    with pytest.raises(RuntimeError, match="injected failure"):
        agent_api._lock_target(name, "test target lock", wait)
    for path in paths:
        probe = agent_api._open_private_lock(path, "probe lock")
        try:
            fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            os.close(probe)


def test_a_lock_descriptor_is_owned_before_it_is_locked(tmp_path: Path) -> None:
    # Registering the descriptor can itself fail (MemoryError). If that happened after the
    # flock, the caller would never see the descriptor and a long-lived process that survives
    # the error would hold the lock forever. Registration must come first.
    import fcntl

    path = str(tmp_path / "target.lock")
    unregistered: list[int] = []

    class FailingRegistry(list[int]):
        def append(self, descriptor: int) -> None:
            unregistered.append(descriptor)
            raise MemoryError("injected registration failure")

    try:
        with pytest.raises(MemoryError):
            agent_api._lock_current_file(path, "test target lock", FailingRegistry())
        probe = agent_api._open_private_lock(path, "probe lock")
        try:
            fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            os.close(probe)
    finally:
        for descriptor in unregistered:
            os.close(descriptor)


@pytest.mark.parametrize(
    ("marker_age", "refreshed"),
    [(0, False), (60 * 60, True), (-24 * 60 * 60, True), (None, True)],
    ids=["fresh", "due", "future", "missing"],
)
def test_lock_files_are_refreshed_only_when_the_marker_is_due(
    tmp_path: Path, marker_age: int | None, refreshed: bool
) -> None:
    import time

    marker = tmp_path / agent_api._LEGACY_REFRESH_MARKER
    directory = tmp_path / "legacy"
    directory.mkdir()
    lock_file = directory / "idle.lock"
    lock_file.touch()
    stale = time.time() - 5 * 24 * 60 * 60
    os.utime(lock_file, (stale, stale))
    if marker_age is not None:
        marker.touch()
        marked = time.time() - marker_age
        os.utime(marker, (marked, marked))
    agent_api._refresh_lock_files_when_due(str(marker), str(directory))
    assert (lock_file.stat().st_mtime > time.time() - 60) is refreshed
    assert marker.exists()


def test_taking_a_target_lock_marks_every_lock_file_used() -> None:
    # The cleaner deletes by modification time, and only a file it sees as four days old. It
    # also deletes a file a moment after examining it, and a target no one sends to ages out,
    # so every /tmp file is refreshed, not only this target's.
    import time

    name = "marks-files-used"
    paths = (*agent_api._target_lock_paths(name), agent_api._target_lock_paths("an-idle-target")[1])
    stale = time.time() - 5 * 24 * 60 * 60
    for path in paths:
        os.close(agent_api._open_private_lock(path, "stale test lock"))
        os.utime(path, (stale, stale))
    # No refresh recorded yet, as on a host's first new-edition delivery.
    marker = os.path.join(os.path.dirname(paths[0]), agent_api._LEGACY_REFRESH_MARKER)
    if os.path.exists(marker):
        os.unlink(marker)
    before = time.time() - 1
    with agent_api._lock_target(name, "test target lock"):
        for path in paths:
            assert os.stat(path).st_mtime >= before, path


@pytest.mark.parametrize("held", [0, 1], ids=["account-root", "legacy-root"])
def test_a_target_lock_waits_for_a_holder_of_either_root(held: int) -> None:
    # A holder of the legacy file is an earlier edition; a holder of the account file is a
    # current one. A new sender must wait for both.
    import fcntl

    name = "either-root-holder"
    path = agent_api._target_lock_paths(name)[held]
    holder = agent_api._open_private_lock(path, "held test lock")
    fcntl.flock(holder, fcntl.LOCK_EX)
    acquired = threading.Event()

    def contend() -> None:
        with agent_api._lock_target(name, "contended test lock"):
            acquired.set()

    contender = threading.Thread(target=contend, daemon=True)
    try:
        contender.start()
        assert not acquired.wait(0.2), "a held root did not block the sender"
    finally:
        os.close(holder)
    contender.join(5)
    assert acquired.is_set() and not contender.is_alive()


def test_working_confirmation_failure_never_resubmits_and_marks_ambiguous(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle"])

    def fail(*_args: object) -> None:
        raise HerdrUnavailable("no working transition")

    fake.wait_agent_status = fail  # type: ignore[assignment]
    with pytest.raises(AgentDeliveryError, match="retained"):
        send(client(fake), target(), str(tmp_path), "preserve me", max_attempts=2)
    failed = list((tmp_path / "failed").glob("*.json"))  # type: ignore[operator]
    assert len(failed) == 1
    document = json.loads(failed[0].read_text())
    assert document["text"] == "preserve me"
    assert document["delivery_attempts"] == 1
    assert document["possibly_submitted"] is True
    assert len(fake.runs) == 1


def test_send_inspects_terminal_artifact_when_another_drain_consumed_its_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    original_enqueue = agent_api._enqueue

    def consumed_enqueue(
        root: str,
        text: str,
        *,
        message_id: str | None,
        serialize: bool,
        max_artifact_bytes: int | None = None,
    ) -> str:
        del message_id, serialize
        identifier = original_enqueue(
            root, text, message_id="cross-drained", serialize=True,
            max_artifact_bytes=max_artifact_bytes,
        )
        source = tmp_path / "inbox/cross-drained.json"
        document = json.loads(source.read_text())
        document["delivery_error"] = "other drain lost confirmation"
        destination = tmp_path / "failed/cross-drained.json"
        source.replace(destination)
        destination.write_text(json.dumps(document), encoding="utf-8")
        return identifier

    monkeypatch.setattr(agent_api, "_enqueue", consumed_enqueue)
    monkeypatch.setattr(
        agent_api,
        "drain",
        lambda *_args, **_kwargs: QueueResult("", (), (), (), None, "delivered"),
    )

    with pytest.raises(AgentPossiblySubmitted, match="other drain lost confirmation"):
        agent_api.send(client(FakeAgentHerdr()), target(), str(tmp_path), "raced")


@pytest.mark.parametrize(
    "change,expected",
    [
        ({"session_value": "wrong"}, "found 0"),
        ({"expected_workspace": "wrong"}, "workspace"),
        ({"expected_cwd": "/wrong"}, "cwd"),
        ({"expected_agent": "claude"}, "agent"),
    ],
)
def test_wrong_identity_workspace_or_cwd_refuses(
    tmp_path: object, change: dict[str, str], expected: str
) -> None:
    fake = FakeAgentHerdr(["idle"])
    with pytest.raises(AgentDeliveryError, match=expected):
        send(client(fake), target(**change), str(tmp_path), "never submit", max_attempts=1)
    assert fake.runs == []
    assert len(list((tmp_path / "inbox").glob("*.json"))) == 1  # type: ignore[operator]
    assert len(list((tmp_path / "failed").glob("*.json"))) == 0  # type: ignore[operator]


def test_session_resolution_cannot_silently_override_asserted_pane(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle"])
    with pytest.raises(AgentDeliveryError, match="expected exact pane"):
        send(client(fake), target(pane_id="w9:p9"), str(tmp_path), "never retarget")
    assert fake.runs == []


@pytest.mark.parametrize("identifier", ["../escape", "/absolute", ".hidden", "bad/name", ""])
def test_enqueue_rejects_unsafe_message_ids(tmp_path: object, identifier: str) -> None:
    with pytest.raises(AgentDeliveryError, match="message id"):
        enqueue(str(tmp_path), "safe text", message_id=identifier)


def test_concurrent_explicit_message_id_is_created_exactly_once(tmp_path: Path) -> None:
    barrier = threading.Barrier(2)
    successes: list[str] = []
    failures: list[BaseException] = []

    def create() -> None:
        barrier.wait()
        try:
            successes.append(enqueue(str(tmp_path), "one durable value", message_id="same-id"))
        except BaseException as exc:
            failures.append(exc)

    threads = [threading.Thread(target=create) for _ in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(5)

    assert successes == ["same-id"]
    assert len(failures) == 1
    assert isinstance(failures[0], AgentDeliveryError)
    assert json.loads((tmp_path / "inbox/same-id.json").read_text())["text"] == "one durable value"


@pytest.mark.parametrize("missing", [Target(), Target(pane_id="")])
def test_missing_target_is_rejected_before_queue_mutation(
    tmp_path: Path, missing: Target
) -> None:
    root = tmp_path / "queue"
    with pytest.raises(AgentDeliveryError, match="target needs"):
        send(client(FakeAgentHerdr()), missing, str(root), "do not strand me")
    assert not root.exists()


@pytest.mark.parametrize(
    "payload",
    [
        '{"id":"bad","text":NaN}\n',
        '{"id":"bad","text":"prompt","delivery_attempts":true}\n',
        '{"id":"bad","text":""}\n',
    ],
)
def test_nonstandard_or_unsafe_queue_documents_are_quarantined(
    tmp_path: Path, payload: str
) -> None:
    inbox = tmp_path / "inbox"
    inbox.mkdir(parents=True, mode=0o700)
    (inbox / "bad.json").write_text(payload, encoding="utf-8")

    result = drain(client(FakeAgentHerdr()), target(), str(tmp_path))

    assert result.quarantined == ("bad",)
    assert list((tmp_path / "failed").glob("bad.json"))


def test_exhausted_fifo_head_is_pending_not_success(tmp_path: Path) -> None:
    inbox = tmp_path / "inbox"
    inbox.mkdir(parents=True, mode=0o700)
    (inbox / "exhausted.json").write_text(
        '{"id":"exhausted","text":"keep me","delivery_attempts":1}\n',
        encoding="utf-8",
    )

    result = drain(
        client(FakeAgentHerdr()), target(), str(tmp_path), max_attempts=1
    )

    assert result.outcome == "pending"
    assert result.pending == ("exhausted",)
    assert result.blocked is not None and "maximum delivery-attempt" in result.blocked


def test_fifo_queue_artifact_is_quarantined_without_blocking(tmp_path: Path) -> None:
    inbox = tmp_path / "inbox"
    inbox.mkdir(parents=True, mode=0o700)
    os.mkfifo(inbox / "pipe.json", mode=0o600)

    result = drain(client(FakeAgentHerdr()), target(), str(tmp_path))

    assert result.quarantined == ("pipe",)
    assert stat.S_ISFIFO((tmp_path / "failed/pipe.json").lstat().st_mode)


def test_queue_directories_are_tightened_and_symlink_lock_is_refused(tmp_path: Path) -> None:
    inbox = tmp_path / "inbox"
    inbox.mkdir(mode=0o755)
    enqueue(str(tmp_path), "queued")
    assert stat.S_IMODE(inbox.stat().st_mode) == 0o700

    victim = tmp_path / "victim"
    victim.write_text("do not touch", encoding="utf-8")
    (tmp_path / ".delivery.lock").unlink()
    (tmp_path / ".delivery.lock").symlink_to(victim)
    with pytest.raises(AgentDeliveryError, match="cannot open queue delivery lock"):
        drain(client(FakeAgentHerdr(["idle"])), target(), str(tmp_path))
    assert victim.read_text(encoding="utf-8") == "do not touch"


@pytest.mark.parametrize("operation", ["enqueue", "drain", "drain-binding"])
@pytest.mark.parametrize("replacement", ["directory", "directory-reused-lock", "lock"])
def test_queue_waiter_refuses_archived_directory_or_replaced_lock(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, operation: str, replacement: str,
) -> None:
    active = tmp_path / "worker"
    queue = active / "queue"
    archive = tmp_path / "archive" / "worker-old"
    archive.parent.mkdir(mode=0o700)
    enqueue(str(queue), "old pending prompt", message_id="old")
    agent_api._bind_queue(str(queue), target())
    (active / "agent.json").write_text("old lifetime\n", encoding="utf-8")
    for folder in ("inflight", "failed", "processed"):
        agent_api._atomic_json(str(queue / folder / "retained.json"), {"text": folder})

    def artifacts(root: Path) -> dict[str, bytes]:
        return {str(path.relative_to(root)): path.read_bytes()
                for path in root.rglob("*") if path.is_file()}

    lock_name = ".binding.lock" if operation == "drain-binding" else ".delivery.lock"
    holder = agent_api._open_private_lock(str(queue / lock_name), "test queue holder")
    original_flock = fcntl.flock
    original_flock(holder, fcntl.LOCK_EX)
    held = os.fstat(holder)
    attempted = False
    expected: dict[Path, dict[str, bytes]] = {}

    def replace_while_waiting(descriptor: int, mode: int) -> None:
        nonlocal attempted
        observed = os.fstat(descriptor)
        if (not attempted and mode == fcntl.LOCK_EX
                and (observed.st_dev, observed.st_ino) == (held.st_dev, held.st_ino)):
            attempted = True
            with pytest.raises(BlockingIOError):
                original_flock(descriptor, mode | fcntl.LOCK_NB)
            if replacement == "lock":
                path = queue / lock_name
                path.rename(path.with_name(f".retired{lock_name}"))
                os.close(agent_api._open_private_lock(str(path), "replacement queue lock"))
                expected[active] = artifacts(active)
            else:
                active.rename(archive)
                enqueue(str(queue), "new lifetime prompt", message_id="new")
                agent_api._bind_queue(str(queue), target(session_value="session-2"))
                (active / "agent.json").write_text("new lifetime\n", encoding="utf-8")
                if replacement == "directory-reused-lock":
                    path = queue / lock_name
                    path.rename(path.with_name(f".replacement{lock_name}"))
                    # Directory identity is independent of the exact lock inode.
                    os.link(archive / "queue" / lock_name, path)
                expected[archive] = artifacts(archive)
                expected[active] = artifacts(active)
            original_flock(holder, fcntl.LOCK_UN)
        original_flock(descriptor, mode)

    monkeypatch.setattr(fcntl, "flock", replace_while_waiting)
    fake = FakeAgentHerdr(["idle", "working", "idle"])
    try:
        with pytest.raises(AgentDeliveryError, match="changed while waiting") as error:
            if operation == "enqueue":
                enqueue(str(queue), "stale writer prompt", message_id="stale")
            else:
                drain(client(fake), target(), str(queue))
    finally:
        os.close(holder)
    assert attempted, "the operation must actually contend on the old lock"
    assert type(error.value) is AgentDeliveryError
    assert fake.runs == []
    assert expected
    for root, snapshot in expected.items():
        assert artifacts(root) == snapshot


def test_drain_keeps_queue_generation_from_binding_until_delivery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    queue = tmp_path / "queue"
    archive = tmp_path / "old-queue"
    enqueue(str(queue), "old pending prompt", message_id="old")
    original_bind = agent_api._bind_queue

    def bind_then_replace(
        root: str, bound_target: Target, *, max_artifact_bytes: int | None = None,
        allow_legacy_workspace_binding: bool = False,
    ) -> tuple[int, int]:
        identity = original_bind(
            root, bound_target, max_artifact_bytes=max_artifact_bytes,
            allow_legacy_workspace_binding=allow_legacy_workspace_binding,
        )
        queue.rename(archive)
        enqueue(str(queue), "new pending prompt", message_id="new")
        original_bind(str(queue), target(session_value="session-2"))
        return identity

    monkeypatch.setattr(agent_api, "_bind_queue", bind_then_replace)
    fake = FakeAgentHerdr(["idle", "working", "idle"])
    with pytest.raises(AgentDeliveryError, match="queue directory changed while waiting"):
        drain(client(fake), target(), str(queue))
    assert fake.runs == []
    assert json.loads((archive / "inbox/old.json").read_text())["delivery_attempts"] == 0
    assert json.loads((queue / "inbox/new.json").read_text())["delivery_attempts"] == 0
    assert list((queue / "failed").iterdir()) == []
    assert list((queue / "processed").iterdir()) == []


def test_timeout_retains_prompt_and_loud_error(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["working"])
    with pytest.raises(AgentDeliveryError, match="remains pending"):
        send(client(fake), target(), str(tmp_path), "poll artifact survives", ready_timeout=0, max_attempts=1)
    pending = next((tmp_path / "inbox").glob("*.json"))  # type: ignore[operator]
    document = json.loads(pending.read_text())
    assert document["text"] == "poll artifact survives"
    assert document["delivery_attempts"] == 0
    assert list((tmp_path / "failed").glob("*.json")) == []  # type: ignore[operator]


def test_fast_idle_working_idle_transition_is_confirmed_without_screen_matching(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle", "idle"])

    def fast_transition(pane_id: str, state: str, timeout_ms: int) -> None:
        assert (pane_id, state, timeout_ms) == ("w1:p1", "working", 30000)
        # Native wait observed working even though a later point probe is already idle.
        fake.states = ["idle"]

    fake.wait_agent_status = fast_transition  # type: ignore[method-assign]
    result = send(client(fake), target(), str(tmp_path), "fast turn")
    assert result.delivered == (result.message_id,)
    assert fake.runs == ["fast turn"]
    assert status(client(fake), target(), str(tmp_path))["agent_status"] == "idle"


def test_post_run_transport_crash_is_quarantined_once_as_possibly_submitted(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle"])

    def crash_after_possible_accept(_pane_id: str, text: str) -> None:
        fake.runs.append(text)
        raise RuntimeError("connection vanished after write")

    fake.prompt_agent = crash_after_possible_accept  # type: ignore[assignment]
    with pytest.raises(AgentDeliveryError, match="may have been submitted"):
        send(client(fake), target(), str(tmp_path), "only once", max_attempts=3)
    assert fake.runs == ["only once"]
    document = json.loads(next((tmp_path / "failed").glob("*.json")).read_text())  # type: ignore[operator]
    assert document["possibly_submitted"] is True
    assert document["delivery_attempts"] == 1


def test_queue_binding_refuses_a_different_session_without_moving_prompt(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["working"])
    with pytest.raises(AgentDeliveryError, match="remains pending"):
        send(client(fake), target(), str(tmp_path), "bound prompt", ready_timeout=0)
    with pytest.raises(AgentDeliveryError, match="bound to"):
        drain(client(fake), target(session_value="different"), str(tmp_path))
    pending = next((tmp_path / "inbox").glob("*.json"))  # type: ignore[operator]
    assert "bound prompt" in pending.read_text()


def test_status_and_read_cover_arbitrary_target(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle"])
    queued = enqueue(str(tmp_path), "queued")
    snapshot = status(client(fake), target(), str(tmp_path))
    assert snapshot["pending"] == [queued]
    assert snapshot["session_value"] == "session-1"
    assert read(client(fake), target(), lines=17) == "agent transcript\n"


def test_status_on_absent_queue_creates_no_files(tmp_path: Path) -> None:
    root = tmp_path / "does-not-exist"
    fake = FakeAgentHerdr(["idle"])
    snapshot = status(client(fake), target(), str(root))
    assert snapshot["pending"] == []
    assert snapshot["inflight"] == []
    assert snapshot["failed"] == []
    assert not root.exists()


def test_status_refuses_symlinked_queue_or_binding_without_touching_target(tmp_path: Path) -> None:
    real = tmp_path / "real"
    real.mkdir(mode=0o700)
    alias = tmp_path / "alias"
    alias.symlink_to(real, target_is_directory=True)
    fake = FakeAgentHerdr(["idle"])
    with pytest.raises(AgentDeliveryError, match="unsafe queue directory"):
        status(client(fake), target(), str(alias))

    binding = real / "target.json"
    victim = tmp_path / "victim.json"
    victim.write_text("{}", encoding="utf-8")
    binding.symlink_to(victim)
    with pytest.raises(AgentDeliveryError, match="cannot read queue target binding"):
        status(client(fake), target(), str(real))
    assert victim.read_text(encoding="utf-8") == "{}"


def test_status_validates_processed_directory_too(tmp_path: Path) -> None:
    enqueue(str(tmp_path), "queued")
    processed = tmp_path / "processed"
    processed.chmod(0o755)

    with pytest.raises(AgentDeliveryError, match="queue state directory is not private"):
        status(client(FakeAgentHerdr()), target(), str(tmp_path))

    assert stat.S_IMODE(processed.stat().st_mode) == 0o755


def test_status_read_only_validates_existing_binding(tmp_path: Path) -> None:
    fake = FakeAgentHerdr(["working"])
    with pytest.raises(AgentPending):
        send(client(fake), target(), str(tmp_path), "bound", ready_timeout=0)
    before = sorted(str(path.relative_to(tmp_path)) for path in tmp_path.rglob("*"))
    with pytest.raises(AgentDeliveryError, match="bound to"):
        status(client(fake), target(session_value="different"), str(tmp_path))
    after = sorted(str(path.relative_to(tmp_path)) for path in tmp_path.rglob("*"))
    assert after == before


def test_read_falls_back_when_unwrapped_source_is_empty(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle"])
    sources: list[str] = []

    def source_read(_pane_id: str, *, source: str, lines: int) -> str:
        assert lines == 17
        sources.append(source)
        return "" if source == "recent-unwrapped" else "fallback transcript\n"

    fake.read = source_read  # type: ignore[assignment]
    assert read(client(fake), target(), lines=17) == "fallback transcript\n"
    assert sources == ["recent-unwrapped", "recent"]


def test_drain_accepts_existing_subagent_message_shape(tmp_path: object) -> None:
    inbox = tmp_path / "inbox"  # type: ignore[operator]
    inbox.mkdir()
    path = inbox / "000000000007.json"
    path.write_text(json.dumps({"seq": 7, "text": "legacy fifo", "tui_delivery_attempts": 0}))
    fake = FakeAgentHerdr(["idle"])
    result = drain(client(fake), target(), str(tmp_path))
    assert result.delivered == ("000000000007",)
    assert fake.runs == ["legacy fifo"]
    assert os.path.isfile(tmp_path / "processed" / path.name)  # type: ignore[operator]


def _queue_at(inbox: Path, name: str, queued_at: object, text: str) -> None:
    """Write one inbox entry as ``enqueue`` would, with the given ``queued_at``."""
    entry = {"id": name, "text": text, "queued_at": queued_at, "delivery_attempts": 0}
    (inbox / f"{name}.json").write_text(json.dumps(entry) + "\n")


def test_drain_types_the_inbox_in_queue_order_not_file_name_order(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A chat request's file is named for its key, a hash, so names sort in no useful order.
    # Here they sort opposite to the queue times, two entries share a time, and the directory
    # lists them in reverse name order, so only the sort key can put them right.
    inbox = tmp_path / "inbox"
    inbox.mkdir()
    _queue_at(inbox, "chat-d", 100.5, "first")
    _queue_at(inbox, "chat-c", 200, "second")
    _queue_at(inbox, "chat-a", 300, "third")
    _queue_at(inbox, "chat-b", 300.0, "fourth")
    listdir = os.listdir
    monkeypatch.setattr(os, "listdir", lambda path=".": sorted(listdir(path), reverse=True))
    fake = FakeAgentHerdr(["idle"])
    result = drain(client(fake), target(), str(tmp_path))
    assert fake.runs == ["first", "second", "third", "fourth"]
    assert result.delivered == ("chat-d", "chat-c", "chat-a", "chat-b")
    assert result.pending == ()


def test_a_held_oldest_prompt_holds_newer_prompts_whose_names_sort_first(tmp_path: Path) -> None:
    inbox = tmp_path / "inbox"
    inbox.mkdir()
    _queue_at(inbox, "chat-zzz", 100, "oldest")
    _queue_at(inbox, "chat-aaa", 200, "middle")
    _queue_at(inbox, "chat-mmm", 300, "newest")
    busy = FakeAgentHerdr(["working"])
    held = drain(client(busy), target(), str(tmp_path), ready_timeout=0)
    assert held.outcome == "pending"
    assert held.delivered == ()
    assert busy.runs == []
    oldest = json.loads((inbox / "chat-zzz.json").read_text())
    assert oldest["delivery_state"] == "pending"
    assert oldest["delivery_error"]
    for name in ("chat-aaa", "chat-mmm"):
        assert "delivery_state" not in json.loads((inbox / f"{name}.json").read_text()), name

    idle = FakeAgentHerdr(["idle"])
    result = drain(client(idle), target(), str(tmp_path))
    assert idle.runs == ["oldest", "middle", "newest"]
    assert result.delivered == ("chat-zzz", "chat-aaa", "chat-mmm")


@pytest.mark.parametrize(
    "queued_at",
    [None, '"2026-10-03T10:00:00Z"', "true", "null", "1e400", "1" + "0" * 400],
    ids=["absent", "text", "bool", "null", "overflowing-float", "overflowing-int"],
)
def test_drain_puts_an_entry_without_a_finite_numeric_queue_time_first(
    tmp_path: Path, queued_at: str | None,
) -> None:
    # The subagent message shape numbers its file names in sequence and records `queued_at` as
    # text, if at all, so it cannot be placed by time; nor can a `queued_at` that is true, null,
    # or too large for a finite float. The timed entry is queued at time 0, so an untimed entry
    # taken as queued at time 0 would follow it by name.
    inbox = tmp_path / "inbox"
    inbox.mkdir()
    _queue_at(inbox, "chat-a", 0, "timed")
    field = "" if queued_at is None else f', "queued_at": {queued_at}'
    (inbox / "chat-z.json").write_text('{"text": "untimed"' + field + "}\n")
    fake = FakeAgentHerdr(["idle"])
    drain(client(fake), target(), str(tmp_path))
    assert fake.runs == ["untimed", "timed"]


def test_drain_takes_entries_without_a_queue_time_by_file_name(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The directory lists the entries in reverse name order, so only the sort key can put
    # the two untimed entries ahead of the timed one in name order.
    inbox = tmp_path / "inbox"
    inbox.mkdir()
    _queue_at(inbox, "chat-a", 0, "timed")
    (inbox / "chat-y.json").write_text('{"text": "untimed y"}\n')
    (inbox / "chat-z.json").write_text('{"text": "untimed z", "queued_at": "10:00"}\n')
    listdir = os.listdir
    monkeypatch.setattr(os, "listdir", lambda path=".": sorted(listdir(path), reverse=True))
    fake = FakeAgentHerdr(["idle"])
    drain(client(fake), target(), str(tmp_path))
    assert fake.runs == ["untimed y", "untimed z", "timed"]


def test_bounded_drain_types_nothing_before_an_oversized_entry(tmp_path: Path) -> None:
    # The drain cannot read an oversized entry's queue time, so it cannot tell which prompts
    # were queued before it. It refuses the entry, as before, before it types any prompt, even
    # one queued earlier: chat-z records a later time than chat-a, which is queued at time 0.
    inbox = tmp_path / "inbox"
    inbox.mkdir()
    _queue_at(inbox, "chat-a", 0, "timed")
    _queue_at(inbox, "chat-z", 2.0, "x" * 1000)
    fake = FakeAgentHerdr()
    with pytest.raises(AgentDeliveryError, match="exceeds max_artifact_bytes"):
        drain(client(fake), target(), str(tmp_path), max_artifact_bytes=1000)
    assert fake.runs == []
    assert sorted(path.name for path in inbox.iterdir()) == ["chat-a.json", "chat-z.json"]


def test_drain_cli_returns_temporary_failure_when_fifo_remains_blocked(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        agent_cli,
        "drain",
        lambda *_args, **_kwargs: QueueResult("", (), (), ("pending",), "agent still working"),
    )
    assert agent_cli.main(["drain", "--pane", "w1:p1", "--queue", str(tmp_path)]) == 75


@pytest.mark.parametrize(
    "raw",
    [b"{not json\n", b'{"id":"bad","text":7}\n', b'{"id":[],"text":"bad"}\n'],
)
def test_invalid_fifo_head_preserves_raw_and_does_not_block_valid(
    tmp_path: object, raw: bytes
) -> None:
    inbox = tmp_path / "inbox"  # type: ignore[operator]
    inbox.mkdir()
    bad = inbox / "000000000001.json"
    bad.write_bytes(raw)
    good = inbox / "000000000002.json"
    good.write_text(json.dumps({"id": "good", "text": "deliver me"}) + "\n")
    fake = FakeAgentHerdr(["idle"])
    result = drain(client(fake), target(), str(tmp_path))
    failed = tmp_path / "failed" / bad.name  # type: ignore[operator]
    assert failed.read_bytes() == raw
    metadata = json.loads((tmp_path / "failed" / f"{bad.name}.error").read_text())  # type: ignore[operator]
    assert metadata["outcome"] == "invalid_message"
    assert result.quarantined == ("000000000001",)
    assert result.delivered == ("good",)
    assert fake.runs == ["deliver me"]


def test_crash_after_run_before_wait_is_never_resubmitted_on_restart(tmp_path: object) -> None:
    fake = FakeAgentHerdr(["idle"])

    def crash_after_run(_pane: str, _state: str, _timeout: int) -> None:
        raise KeyboardInterrupt("simulated process death")

    fake.wait_agent_status = crash_after_run  # type: ignore[assignment]
    with pytest.raises(KeyboardInterrupt):
        send(client(fake), target(), str(tmp_path), "at most once")
    assert fake.runs == ["at most once"]
    assert len(list((tmp_path / "inflight").glob("*.json"))) == 1  # type: ignore[operator]

    restarted = FakeAgentHerdr(["idle"])
    result = drain(client(restarted), target(), str(tmp_path))
    assert restarted.runs == []
    assert len(list((tmp_path / "inflight").glob("*.json"))) == 0  # type: ignore[operator]
    assert len(list((tmp_path / "failed").glob("*.json"))) == 1  # type: ignore[operator]
    assert result.outcome == "possibly_submitted"


def test_crash_after_inflight_rename_before_metadata_or_run_is_never_resubmitted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = FakeAgentHerdr(["idle"])
    original = agent_api._atomic_json

    def crash_on_inflight(
        path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
    ) -> None:
        if Path(path).parent.name == "inflight":
            raise KeyboardInterrupt("simulated death immediately after durable rename")
        original(path, document, max_artifact_bytes=max_artifact_bytes)

    monkeypatch.setattr(agent_api, "_atomic_json", crash_on_inflight)
    with pytest.raises(KeyboardInterrupt):
        send(client(fake), target(), str(tmp_path), "rename barrier")
    assert fake.runs == []
    assert list((tmp_path / "inbox").glob("*.json")) == []
    assert len(list((tmp_path / "inflight").glob("*.json"))) == 1

    monkeypatch.setattr(agent_api, "_atomic_json", original)
    restarted = FakeAgentHerdr(["idle"])
    result = drain(client(restarted), target(), str(tmp_path))
    assert restarted.runs == []
    assert result.outcome == "possibly_submitted"
    assert len(list((tmp_path / "failed").glob("*.json"))) == 1


def test_crash_during_busy_wait_stays_pending_and_delivers_after_restart(tmp_path: Path) -> None:
    busy = FakeAgentHerdr(["working"])

    def crash_while_waiting(_seconds: float) -> None:
        raise KeyboardInterrupt("simulated process death before injection")

    with pytest.raises(KeyboardInterrupt):
        send(
            client(busy),
            target(),
            str(tmp_path),
            "still safe",
            ready_timeout=900,
            sleep=crash_while_waiting,
        )
    assert busy.runs == []
    assert len(list((tmp_path / "inbox").glob("*.json"))) == 1
    assert list((tmp_path / "inflight").glob("*.json")) == []
    assert list((tmp_path / "failed").glob("*.json")) == []

    restarted = FakeAgentHerdr(["idle"])
    result = drain(client(restarted), target(), str(tmp_path))
    assert restarted.runs == ["still safe"]
    assert result.outcome == "delivered"
    assert result.pending == ()


def test_send_failures_have_distinct_typed_machine_outcomes(tmp_path: Path) -> None:
    busy = FakeAgentHerdr(["working"])
    with pytest.raises(AgentPending) as pending:
        send(client(busy), target(), str(tmp_path / "pending"), "safe", ready_timeout=0)
    assert pending.value.outcome == "pending"
    assert os.path.isfile(pending.value.artifact)

    ambiguous = FakeAgentHerdr(["idle"])
    ambiguous.wait_agent_status = lambda *_args: (_ for _ in ()).throw(HerdrUnavailable("lost"))  # type: ignore[method-assign]
    with pytest.raises(AgentPossiblySubmitted) as submitted:
        send(client(ambiguous), target(), str(tmp_path / "ambiguous"), "unsafe")
    assert submitted.value.outcome == "possibly_submitted"
    assert os.path.isfile(submitted.value.artifact)


def test_send_cli_emits_structured_distinct_failure(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    artifact = str(tmp_path / "inbox" / "m.json")
    monkeypatch.setattr(
        agent_cli,
        "send",
        lambda *_a, **_k: (_ for _ in ()).throw(
            AgentPending("busy", message_id="m", artifact=artifact)
        ),
    )
    assert agent_cli.main(["send", "--pane", "w1:p1", "hello"]) == 75
    assert json.loads(capsys.readouterr().out) == {
        "artifact": artifact,
        "error": "busy",
        "message_id": "m",
        "outcome": "pending",
        "safe_to_retry": True,
    }

    failed_artifact = str(tmp_path / "failed" / "m2.json")
    monkeypatch.setattr(
        agent_cli,
        "send",
        lambda *_a, **_k: (_ for _ in ()).throw(
            AgentPossiblySubmitted("unknown", message_id="m2", artifact=failed_artifact)
        ),
    )
    assert agent_cli.main(["send", "--pane", "w1:p1", "hello"]) == 76
    assert json.loads(capsys.readouterr().out)["safe_to_retry"] is False


def test_drain_cli_distinguishes_quarantine_from_pending(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        agent_cli,
        "drain",
        lambda *_args, **_kwargs: QueueResult(
            "", (), ("ambiguous",), (), None, "possibly_submitted"
        ),
    )
    assert agent_cli.main(["drain", "--pane", "w1:p1", "--queue", str(tmp_path)]) == 76


def test_enqueue_and_delivery_state_transitions_fsync(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    calls: list[int] = []
    real_fsync = os.fsync

    def recording_fsync(descriptor: int) -> None:
        calls.append(descriptor)
        real_fsync(descriptor)

    monkeypatch.setattr("agentctl.agent.os.fsync", recording_fsync)
    fake = FakeAgentHerdr(["idle"])
    send(client(fake), target(), str(tmp_path), "durable")
    # Enqueue file+directory, inbox->inflight, inflight update, and
    # inflight->processed each require syncs; keep the assertion structural.
    assert len(calls) >= 8


@pytest.mark.parametrize("ordering", ["between-positionals", "prefix-and-middle", "named-suffix"])
def test_stop_refusal_legacy_prepass_preserves_valid_intermixed_syntax(
    ordering: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    registry = str(tmp_path / "unused-registry")
    binary = "literal-herdr"
    seen: dict[str, object] = {}

    class Backend:
        def stop(self, name: str, **options: object) -> dict[str, object]:
            seen["name"] = name
            seen["options"] = options
            return {"name": name, "stopped": True}

    def client_factory(*, herdr_bin: str) -> HerdrClient:
        seen["binary"] = herdr_bin
        return cast(HerdrClient, object())

    def manager_factory(_client: HerdrClient, selected_registry: str) -> Backend:
        seen["registry"] = selected_registry
        return Backend()

    monkeypatch.setattr(agent_cli, "HerdrClient", client_factory)
    monkeypatch.setattr(agent_cli, "ManagedAgents", manager_factory)
    if ordering == "between-positionals":
        arguments = ["stop", "--registry", registry, "worker", "--herdr-bin", binary]
    elif ordering == "prefix-and-middle":
        arguments = ["--registry", registry, "stop", "--herdr-bin", binary, "worker"]
    else:
        arguments = ["--herdr-bin", binary, "stop", "--registry", registry, "--name", "worker"]
    assert agent_cli.main(arguments) == 0
    captured = capsys.readouterr()
    assert json.loads(captured.out) == {"name": "worker", "stopped": True} and captured.err == ""
    assert seen == {
        "registry": registry, "binary": binary, "name": "worker",
        "options": {"expected_token": None, "recover_legacy_adoption": False,
                    "retire_dead_adoption": False, "expected_record_sha256": None},
    }
    assert not (tmp_path / "unused-registry").exists()


@pytest.mark.parametrize("edition", ["primary", "legacy"])
@pytest.mark.parametrize("possibly_submitted", [False, True])
def test_stop_refusal_changes_leave_ordinary_send_pending_json_and_stderr_unchanged(
    edition: str, possibly_submitted: bool, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    error = (AgentPossiblySubmitted("uncertain", message_id="m", artifact="retained.json")
             if possibly_submitted else AgentPending("busy", message_id="m", artifact="retained.json"))

    class Backend:
        def send_session(self, _name: str, _text: str, **_options: object) -> dict[str, object]:
            raise error

    def fail_send(*_args: object, **_options: object) -> QueueResult:
        raise error

    monkeypatch.setattr(primary_cli, "Sessions", lambda *_args, **_kwargs: Backend())
    monkeypatch.setattr(agent_cli, "send", fail_send)
    run = primary_cli.main if edition == "primary" else agent_cli.main
    arguments = ["send", "worker", "instruction"] if edition == "primary" else ["send", "--pane=p", "instruction"]
    assert run(arguments) == error.exit_code
    captured = capsys.readouterr()
    assert captured.err == ""
    assert json.loads(captured.out) == {
        "outcome": error.outcome, "message_id": "m", "artifact": "retained.json",
        "error": str(error), "safe_to_retry": not possibly_submitted,
    }
