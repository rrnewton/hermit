"""A scoped journal path carries one operation after another."""

from __future__ import annotations

import json
import os
import subprocess
from collections.abc import Mapping, Sequence
from pathlib import Path

import pytest

from wrkslots import cli as wrkslots
from wrkslots.tests.test_lifecycle import (
    active_slots,
    allow_test_host_for_absent_validate_recovery,
    create,
    make_project,
    mark_owner_dead,
    raw_command_with_census_authority_stub,
    remove_completed_validation,
    set_liveness,
    run_absent_validate_recovery,
    stub_validate_batch_censuses,
    write_absent_validate_input,
)

CHURN = (
    "same-UID process evidence changed during three liveness attempts; "
    "preserve the slot and rerun remove"
)


def _prepare(tmp_path: Path) -> tuple[Path, wrkslots.Config]:
    project, _repository, _remote = make_project(tmp_path)
    made = create(project, slot="target", slot_type="validate", branch=None)
    assert made.returncode == 0, made.stderr
    mark_owner_dead(project)
    set_liveness(project, "dead")
    return project, wrkslots._load_config(str(project), "testhost")


FinishEvent = tuple[str, str | None, str | None, str | None]


def _finish_events(config: wrkslots.Config) -> list[FinishEvent]:
    """Return (kind, phase, archive_id, fenced) for the target's finish path."""

    path = wrkslots._finish_journal_path(config, "target")
    observed: list[FinishEvent] = []
    for event in wrkslots._load_events(config, config.machine):
        payload = event["payload"]
        assert isinstance(payload, dict)
        if payload.get("journal_path") != path.name:
            continue
        journal = payload.get("journal")
        phase = journal.get("phase") if isinstance(journal, dict) else None
        archive = journal.get("archive_id") if isinstance(journal, dict) else None
        fence = journal.get("fenced") if isinstance(journal, dict) else None
        assert isinstance(event["kind"], str)
        observed.append((event["kind"], phase, archive, fence))
    return observed


def _event_bytes(config: wrkslots.Config) -> dict[str, bytes]:
    directory = config.control / f"EVENTS.{config.machine}"
    return {path.name: path.read_bytes() for path in sorted(directory.glob("*.json"))}


def _refuse_first_fresh_census(monkeypatch: pytest.MonkeyPatch) -> list[int]:
    stub_validate_batch_censuses(monkeypatch)
    calls: list[int] = []

    def fresh(
        _paths: Sequence[Path], *, budget: wrkslots._ReadOnlyCommandBudget
    ) -> wrkslots._ProcessPathCensus:
        del budget
        calls.append(len(calls))
        if len(calls) == 1:
            raise wrkslots.Refusal(CHURN)
        return wrkslots._ProcessPathCensus((), ())

    monkeypatch.setattr(wrkslots, "_capture_same_uid_process_path_census", fresh)
    monkeypatch.setattr(wrkslots, "_assert_slot_unused", lambda *_a, **_k: None)
    return calls


def _refused_then_rolled_back(
    project: Path,
    config: wrkslots.Config,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> str:
    target = wrkslots._slot_directory(config, "target", "validate")
    calls = _refuse_first_fresh_census(monkeypatch)
    assert remove_completed_validation(project) == 3
    assert CHURN in capsys.readouterr().err
    assert calls == [0]
    first = _finish_events(config)
    assert [(kind, phase) for kind, phase, _archive, _fence in first] == [
        ("operation-progress-recorded", "prepared"),
        ("operation-progress-recorded", "fenced"),
        ("operation-progress-recorded", "prepared"),
        ("operation-completed", None),
    ]
    assert len({archive for _kind, _phase, archive, _fence in first[:3]}) == 1
    assert len({fence for _kind, _phase, _archive, fence in first[:3]}) == 1
    assert target.is_dir()
    assert not tuple(target.parent.glob(".target.fenced.1.*"))
    assert not wrkslots._finish_journal_path(config, "target").exists()
    assert not wrkslots._validate_batch_seal_journal_path(config).exists()
    assert [row["slot"] for row in active_slots(project) if isinstance(row, dict)] == [
        "target"
    ]
    fence = first[0][3]
    assert first[0][2] is not None and fence is not None
    return fence


def test_retry_after_refused_private_finish_removes_the_slot(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, config = _prepare(tmp_path)
    first_fence = _refused_then_rolled_back(project, config, monkeypatch, capsys)
    history = _event_bytes(config)

    assert remove_completed_validation(project) == 0, capsys.readouterr().err

    target = wrkslots._slot_directory(config, "target", "validate")
    assert not target.exists()
    assert not tuple(target.parent.glob(".target.fenced.1.*"))
    assert not wrkslots._finish_journal_path(config, "target").exists()
    assert not wrkslots._validate_batch_seal_journal_path(config).exists()
    assert active_slots(project) == []
    after = _event_bytes(config)
    assert {name: after[name] for name in history} == history
    second = _finish_events(config)[4:]
    assert second[0][:2] == ("operation-progress-recorded", "prepared")
    assert second[-1][:2] == ("operation-completed", None)
    # The retry is a new operation. Its archive_id may repeat the first one's
    # within the same second, so the per-operation fence is what must differ.
    fences = {fence for _kind, _phase, _archive, fence in second[:-1]}
    assert len(fences) == 1 and None not in fences and fences != {first_fence}


def test_retry_interrupted_after_fence_is_recoverable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, config = _prepare(tmp_path)
    _refused_then_rolled_back(project, config, monkeypatch, capsys)
    target = wrkslots._slot_directory(config, "target", "validate")
    seal = wrkslots._validate_batch_seal_journal_path(config)
    finish = wrkslots._finish_journal_path(config, "target")

    interrupted = raw_command_with_census_authority_stub(
        project,
        "remove",
        "target",
        "--validate-complete",
        "--coordinator-authorized",
        "--coordinator-pid",
        str(os.getpid()),
        "--expected-generation",
        "1",
        env={"WRKSLOTS_TEST_INTERRUPT": "after-validate-batch-fresh-census"},
    )
    assert interrupted.returncode == 86, interrupted.stderr
    # The production shape: canonical path absent, one private fence, the
    # scoped journal fenced, the paired seal present, the ACTIVE row intact.
    assert not target.exists()
    assert len(tuple(target.parent.glob(".target.fenced.1.*"))) == 1
    assert seal.is_file()
    assert finish.is_file()
    assert [row["slot"] for row in active_slots(project) if isinstance(row, dict)] == [
        "target"
    ]
    history = _event_bytes(config)
    allow_test_host_for_absent_validate_recovery(project, monkeypatch)
    rows = write_absent_validate_input(
        project, list(wrkslots._load_active(config).slots)
    )
    capsys.readouterr()
    planned = run_absent_validate_recovery(project, rows, apply=False)
    # The canonical path is absent only because an outstanding finish fenced
    # it; that finish, not an absent row, is what the plan must report.
    streams = capsys.readouterr()
    assert "reused after completion" not in streams.out + streams.err
    assert planned != 0
    assert "interrupted validation-batch seal" in streams.err
    assert _event_bytes(config) == history

    recovered = raw_command_with_census_authority_stub(
        project,
        "recover",
        "--slot",
        "target",
        "--coordinator-authorized",
        "--coordinator-pid",
        str(os.getpid()),
    )

    assert recovered.returncode == 0, recovered.stderr
    assert not target.exists()
    assert not tuple(target.parent.glob(".target.fenced.1.*"))
    assert not seal.exists()
    assert not finish.exists()
    assert active_slots(project) == []
    after = _event_bytes(config)
    assert {name: after[name] for name in history} == history


def test_create_retried_after_abort_recovers_its_interruption(
    tmp_path: Path,
) -> None:
    project, _repository, _remote = make_project(tmp_path)
    config = wrkslots._load_config(str(project), "testhost")
    journal = wrkslots._create_journal_path(config, "slot01")
    interrupt = {"WRKSLOTS_TEST_INTERRUPT": "after-create-worktree"}
    first = create(project, env=interrupt)
    assert first.returncode == 86, first.stderr
    # The journal retry is the subject, not the host-wide slot-use census; that
    # census has its own deliberate abort test in test_lifecycle.py.
    aborted = _abort_create(project)
    assert aborted.returncode == 0, aborted.stderr
    assert active_slots(project) == []
    assert not journal.exists()

    retried = create(project, env=interrupt)
    assert retried.returncode == 86, retried.stderr
    assert journal.is_file()

    recovered = raw_command_with_census_authority_stub(
        project,
        "recover",
        "--coordinator-authorized",
        "--coordinator-pid",
        str(os.getpid()),
        "--slot",
        "slot01",
    )

    assert recovered.returncode == 0, recovered.stderr
    assert [row["slot"] for row in active_slots(project) if isinstance(row, dict)] == [
        "slot01"
    ]
    assert not journal.exists()


def _abort_create(project: Path) -> subprocess.CompletedProcess[str]:
    """Abort the interrupted create under the census authority stub.

    That stub replaces the host-wide slot-use censuses with a cwd scan and pins
    the frozen-validation authority and ownerless-exclusion root; none of those
    is what a journal test asserts.
    """

    return raw_command_with_census_authority_stub(
        project,
        "recover",
        "--coordinator-authorized",
        "--coordinator-pid",
        str(os.getpid()),
        "--abort-create",
    )


def _episode_events(
    config: wrkslots.Config, path: Path | None = None
) -> tuple[list[Mapping[str, object]], dict[str, object]]:
    if path is None:
        path = wrkslots._finish_journal_path(config, "target")
    events: list[Mapping[str, object]] = [
        event
        for event in wrkslots._load_events(config, config.machine)
        if isinstance(event["payload"], dict)
        and event["payload"].get("journal_path") == path.name
    ]
    payload = events[0]["payload"]
    assert isinstance(payload, dict)
    journal = payload["journal"]
    assert isinstance(journal, dict)
    return events, {str(key): value for key, value in journal.items()}


def _progress(
    template: Mapping[str, object], journal: Mapping[str, object]
) -> dict[str, object]:
    payload = template["payload"]
    assert isinstance(payload, dict)
    return {**template, "payload": {**payload, "journal": dict(journal)}}


REPLAYED = "path was reopened with the identity of a completed operation"
REUSED = "path was reused after completion without a new operation"
SWITCHED = "progress changed its operation identity"


@pytest.mark.parametrize(
    "shape",
    (
        "pending",
        "completed",
        "stale",
        "later-phase",
        "repeated-completion",
        "replayed-opening",
        "reused-fence",
        "continued",
        "replayed-after-reopening",
        "completed-fence-mid-operation",
        "switched-fence",
    ),
)
def test_scoped_finish_episodes_bind_the_latest_operation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    shape: str,
) -> None:
    project, config = _prepare(tmp_path)
    _refused_then_rolled_back(project, config, monkeypatch, capsys)
    history, first = _episode_events(config)
    assert [event["kind"] for event in history] == [
        "operation-progress-recorded",
        "operation-progress-recorded",
        "operation-progress-recorded",
        "operation-completed",
    ]
    fenced = history[1]["payload"]
    assert isinstance(fenced, dict)
    assert isinstance(first["fenced"], str)
    retry = {
        **first,
        "archive_id": f"{first['archive_id']}-retry",
        "fenced": f"{first['fenced'][:-32]}{'0' * 32}",
    }
    assert retry["fenced"] != first["fenced"]
    opened = _progress(history[0], retry)
    events: list[Mapping[str, object]] = [*history, opened]
    raw: Mapping[str, object] = retry
    if shape == "completed":
        events.append(history[3])
    elif shape == "stale":
        # The rolled-back operation's final bytes after a new one has begun.
        stale = history[2]["payload"]
        assert isinstance(stale, dict)
        raw = json.loads(json.dumps(stale["journal"]))
    elif shape == "later-phase":
        events = [*history, history[1]]
        raw = json.loads(json.dumps(fenced["journal"]))
    elif shape == "repeated-completion":
        events = [*history, history[3], opened]
    elif shape == "replayed-opening":
        # The completed operation's first progress, byte for byte.
        events = [*history, history[0]]
        raw = first
    elif shape == "reused-fence":
        # A new archive_id cannot disguise the completed operation's fence.
        raw = {**first, "archive_id": f"{first['archive_id']}-retry"}
        events = [*history, _progress(history[0], raw)]
    elif shape == "continued":
        # The new operation's next phase, under its own fence.
        raw = {**fenced["journal"], "archive_id": retry["archive_id"], "fenced": retry["fenced"]}
        events = [*history, opened, _progress(history[1], raw)]
    elif shape == "replayed-after-reopening":
        # A stale writer replays the completed opening after a retry began.
        events = [*history, opened, history[0]]
        raw = first
    elif shape == "completed-fence-mid-operation":
        events = [*history, opened, history[1]]
        raw = json.loads(json.dumps(fenced["journal"]))
    elif shape == "switched-fence":
        raw = {**fenced["journal"], "archive_id": retry["archive_id"], "fenced": f"{first['fenced'][:-32]}{'1' * 32}"}
        events = [*history, opened, _progress(history[1], raw)]
    monkeypatch.setattr(wrkslots, "_load_events", lambda *_a, **_k: list(events))
    path = wrkslots._finish_journal_path(config, "target")

    if shape == "stale":
        with pytest.raises(wrkslots.StateError, match="differs from append-only"):
            wrkslots._scoped_operation_journal_completed(config, path, raw, "finish")
    elif shape == "later-phase":
        with pytest.raises(wrkslots.StateError, match=REUSED):
            wrkslots._scoped_operation_journal_completed(config, path, raw, "finish")
    elif shape in {
        "replayed-opening",
        "reused-fence",
        "replayed-after-reopening",
        "completed-fence-mid-operation",
    }:
        with pytest.raises(wrkslots.StateError, match=REPLAYED):
            wrkslots._scoped_operation_journal_completed(config, path, raw, "finish")
    elif shape == "switched-fence":
        with pytest.raises(wrkslots.StateError, match=SWITCHED):
            wrkslots._scoped_operation_journal_completed(config, path, raw, "finish")
    else:
        assert wrkslots._scoped_operation_journal_completed(
            config, path, raw, "finish"
        ) == (shape == "completed")


@pytest.fixture(scope="module")
def aborted_create(
    tmp_path_factory: pytest.TempPathFactory,
) -> tuple[wrkslots.Config, Path, list[Mapping[str, object]], dict[str, object]]:
    # One interrupted and aborted create, shared by every shape.  The journal is
    # the subject, so the abort stubs the host-wide slot-use census, which has
    # its own deliberate abort test in test_lifecycle.py.
    project, _repository, _remote = make_project(tmp_path_factory.mktemp("create"))
    config = wrkslots._load_config(str(project), "testhost")
    path = wrkslots._create_journal_path(config, "slot01")
    interrupted = create(
        project, env={"WRKSLOTS_TEST_INTERRUPT": "after-create-worktree"}
    )
    assert interrupted.returncode == 86, interrupted.stderr
    aborted = _abort_create(project)
    assert aborted.returncode == 0, aborted.stderr
    history, first = _episode_events(config, path)
    return config, path, history, first


@pytest.mark.parametrize(
    "shape",
    (
        "new-operation",
        "replayed-opening",
        "later-write",
        "without-identity",
        "replayed-after-reopening",
        "switched-identity",
    ),
)
def test_scoped_create_episodes_need_a_new_operation(
    aborted_create: tuple[
        wrkslots.Config, Path, list[Mapping[str, object]], dict[str, object]
    ],
    monkeypatch: pytest.MonkeyPatch,
    shape: str,
) -> None:
    config, path, history, first = aborted_create
    kinds = [event["kind"] for event in history]
    assert kinds[-1] == "operation-completed"
    assert set(kinds[:-1]) == {"operation-progress-recorded"}
    assert first["created"] == [] and first["hook_progress"] == 0
    identity = first["operation_id"]
    assert isinstance(identity, str) and len(identity) == 32
    later = history[-2]["payload"]
    assert isinstance(later, dict) and isinstance(later["journal"], dict)
    assert later["journal"]["created"] != []
    assert later["journal"]["operation_id"] == identity

    raw: dict[str, object] = {**first, "operation_id": "0" * 32}
    if shape == "replayed-opening":
        # The aborted operation's first progress, byte for byte.
        raw = dict(first)
    elif shape == "later-write":
        raw = {**later["journal"], "operation_id": "0" * 32}
    elif shape == "without-identity":
        # A journal written before create recorded an operation identity.
        raw = {key: value for key, value in first.items() if key != "operation_id"}
    events = [*history, _progress(history[0], raw)]
    if shape == "replayed-after-reopening":
        # A stale writer replays the aborted opening after a new create began.
        raw = dict(first)
        events.append(history[0])
    elif shape == "switched-identity":
        raw = {**first, "operation_id": "1" * 32}
        events.append(_progress(history[0], raw))
    monkeypatch.setattr(wrkslots, "_load_events", lambda *_a, **_k: list(events))

    if shape == "new-operation":
        assert not wrkslots._scoped_operation_journal_completed(
            config, path, raw, "create"
        )
    else:
        expected = {
            "replayed-opening": REPLAYED,
            "replayed-after-reopening": REPLAYED,
            "switched-identity": SWITCHED,
        }.get(shape, REUSED)
        with pytest.raises(wrkslots.StateError, match=expected):
            wrkslots._scoped_operation_journal_completed(config, path, raw, "create")
