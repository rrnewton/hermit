"""One command must not re-read or re-replay a settled event-log prefix.

As in test_census_cost, every test pins a COST property and an EQUIVALENCE
property together. The costs are counts of event parses and replayed events,
never wall time; the equivalence is that the memoized answer, including every
refusal, is the answer a cold load gives.
"""

from __future__ import annotations

import dataclasses
import json
import os
import shutil
import time
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import TypeVar, cast

import pytest

from wrkslots import cli
from wrkslots.tests.test_lifecycle import (
    commit_validation_removal_schema,
    make_project,
    prepare_cross_repository_target,
    prepare_dead_validate_slots,
    prepare_validation_removal_proof,
    stub_validate_batch_censuses,
)

T = TypeVar("T")


class _Work:
    """Count event parses and the events each replay folds."""

    def __init__(self, monkeypatch: pytest.MonkeyPatch) -> None:
        self.parses = 0
        self.folded: list[int] = []
        self.counting = True
        parse = cli._event_from_path
        resume = cli._resume_replay

        def counted_parse(
            path: Path,
            machine: str,
            expected_sequence: int,
            previous: str,
            *,
            require_filename: bool = True,
        ) -> dict[str, object]:
            if self.counting:
                self.parses += 1
            return parse(
                path,
                machine,
                expected_sequence,
                previous,
                require_filename=require_filename,
            )

        def counted_resume(
            config: cli.Config,
            key: tuple[cli.Config, str, bool],
            events: Sequence[dict[str, object]],
        ) -> tuple[int, cli._ReplayFold]:
            start, fold = resume(config, key, events)
            if self.counting:
                self.folded.append(len(events) - start)
            return start, fold

        monkeypatch.setattr(cli, "_event_from_path", counted_parse)
        monkeypatch.setattr(cli, "_resume_replay", counted_resume)


def _treat_every_event_as_settled(monkeypatch: pytest.MonkeyPatch) -> None:
    later = cli._EVENT_MEMO_STABLE_NS * 10
    monkeypatch.setattr(cli, "_event_memo_clock_ns", lambda: time.time_ns() + later)


def _dead_validate_batch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, count: int
) -> tuple[Path, list[str]]:
    project, repository, _remote = make_project(tmp_path)
    commit_validation_removal_schema(repository)
    slots = [f"memo-{index}" for index in range(count)]
    prepare_dead_validate_slots(project, slots)
    proofs = {slot: prepare_validation_removal_proof(project, slot) for slot in slots}
    stub_validate_batch_censuses(monkeypatch)
    # These tests count parses and folds; the batch's evidence deadline has its
    # own tests.  Under a loaded gate a four-slot batch outlived that
    # wall-clock bound and retained its last slot, failing a count assertion
    # for a reason that had nothing to do with counting.  The lifted bound
    # stays well inside the component step's budget, so a real hang still
    # fails here by name rather than timing out the whole suite.
    monkeypatch.setattr(cli, "_VALIDATE_REMOVE_BATCH_CENSUS_SECONDS", 120.0)
    argv = [
        "--project-root",
        str(project),
        "remove-validate-batch",
        "--coordinator-authorized",
        "--coordinator-pid",
        str(os.getpid()),
        "--format",
        "json",
    ]
    for slot in slots:
        argv += ["--slot", f"{slot}=1"]
    for slot in slots:
        argv += [
            "--validation-proof-manifest",
            proofs[slot][0].relative_to(project).as_posix(),
        ]
    for slot in slots:
        argv += [
            "--completed-record",
            proofs[slot][1]["run-record"].relative_to(project).as_posix(),
        ]
    return project, argv


def _event_paths(project: Path) -> list[Path]:
    config = cli._load_config(str(project), "testhost")
    return cli._event_files(cli._event_directory(config))


def test_validate_batch_parses_each_event_once_and_folds_each_event_once(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The control for the production deadline expiry.

    Without the memo, one four-slot batch parsed every event file on each of its
    119 whole-log loads and replayed the whole log on each of its 56 replays.
    Remove either memo and the matching equality below fails.
    """

    project, argv = _dead_validate_batch(tmp_path, monkeypatch, 4)
    _treat_every_event_as_settled(monkeypatch)
    work = _Work(monkeypatch)

    assert cli.main(argv) == 0

    report = json.loads(capsys.readouterr().out)
    assert len(report["removed"]) == 4
    assert report["retained"] == []
    events = _event_paths(project)
    assert work.parses == len(events)
    assert len(work.folded) >= 4 * 13
    assert sum(work.folded) == len(events)


def test_validate_batch_memoized_replays_equal_cold_replays(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Every incremental replay a batch performs matches a cold replay."""

    _project, argv = _dead_validate_batch(tmp_path, monkeypatch, 2)
    _treat_every_event_as_settled(monkeypatch)
    replay = cli._states_from_events
    comparisons = 0

    def compared(
        config: cli.Config, machine: str, *, require_repository: bool = True
    ) -> tuple[cli.ActiveState, cli.ArchiveState] | None:
        nonlocal comparisons
        warm = replay(config, machine, require_repository=require_repository)
        memo = list(cli._ACTIVE_EVENT_MEMO)
        cli._ACTIVE_EVENT_MEMO.clear()
        try:
            cold = replay(config, machine, require_repository=require_repository)
        finally:
            cli._ACTIVE_EVENT_MEMO.extend(memo)
        assert warm is not None and cold is not None
        assert warm[0] == cold[0]
        assert warm[1] == cold[1]
        assert warm[1].absent_validate_recoveries == cold[1].absent_validate_recoveries
        assert warm[1].absent_agent_recoveries == cold[1].absent_agent_recoveries
        comparisons += 1
        return warm

    monkeypatch.setattr(cli, "_states_from_events", compared)

    assert cli.main(argv) == 0

    report = json.loads(capsys.readouterr().out)
    assert len(report["removed"]) == 2
    assert comparisons >= 2 * 13


def test_event_memo_keeps_only_settled_files_and_rechecks_their_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    paths = _event_paths(project)
    count = len(paths)
    changed = [os.lstat(path).st_ctime_ns for path in paths]
    work = _Work(monkeypatch)
    clock = [min(changed) + cli._EVENT_MEMO_STABLE_NS - 1]
    monkeypatch.setattr(cli, "_event_memo_clock_ns", lambda: clock[0])
    cold = cli._load_event_paths(paths, config.machine)

    with cli._event_memo_scope() as memo:
        # Inside the change-time window nothing is kept, so a rewrite in the
        # same timestamp tick can never be answered from the memo.
        work.parses = 0
        assert cli._load_event_paths(paths, config.machine) == cold
        assert cli._load_event_paths(paths, config.machine) == cold
        assert work.parses == 2 * count
        assert memo.files == {}

        # One nanosecond short of the newest file settling keeps every other file.
        clock[0] = max(changed) + cli._EVENT_MEMO_STABLE_NS - 1
        recent = sum(1 for ns in changed if ns + cli._EVENT_MEMO_STABLE_NS > clock[0])
        assert recent >= 1
        work.parses = 0
        assert cli._load_event_paths(paths, config.machine) == cold
        assert cli._load_event_paths(paths, config.machine) == cold
        assert work.parses == count + recent
        assert len(memo.files) == count - recent
        memo.files.clear()

        clock[0] = max(changed) + cli._EVENT_MEMO_STABLE_NS
        work.parses = 0
        assert cli._load_event_paths(paths, config.machine) == cold
        assert work.parses == count
        assert cli._load_event_paths(paths, config.machine) == cold
        assert cli._load_event_paths(paths[:-1], config.machine) == cold[:-1]
        assert work.parses == count

        # An in-place rewrite that preserves size and mtime still moves ctime.
        target = paths[1]
        original = target.read_bytes()
        before = os.lstat(target)
        digest = cast(str, cold[1]["sha256"])
        replacement = "0" if digest[0] != "0" else "1"
        tampered = original.replace(digest.encode(), (replacement + digest[1:]).encode())
        assert len(tampered) == len(original) and tampered != original
        with target.open("r+b") as handle:
            handle.write(tampered)
        os.utime(target, ns=(before.st_atime_ns, before.st_mtime_ns))
        after = os.lstat(target)
        assert (after.st_ino, after.st_size, after.st_mtime_ns) == (
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
        )
        assert after.st_ctime_ns != before.st_ctime_ns
        with pytest.raises(cli.StateError, match="digest does not match its content"):
            cli._load_event_paths(paths, config.machine)

        with target.open("r+b") as handle:
            handle.write(original)
        assert cli._load_event_paths(paths, config.machine) == cold

        # A symlink is refused exactly as a cold load refuses it.
        kept = tmp_path / "kept-event.json"
        shutil.copy2(target, kept)
        target.unlink()
        target.symlink_to(kept)
        with pytest.raises(cli.StateError) as warm_refusal:
            cli._load_event_paths(paths, config.machine)
        memo_stack = list(cli._ACTIVE_EVENT_MEMO)
        cli._ACTIVE_EVENT_MEMO.clear()
        try:
            with pytest.raises(cli.StateError) as cold_refusal:
                cli._load_event_paths(paths, config.machine)
        finally:
            cli._ACTIVE_EVENT_MEMO.extend(memo_stack)
        assert str(warm_refusal.value) == str(cold_refusal.value)

        # A same-content replacement is a new file and is parsed again.
        target.unlink()
        shutil.copy2(kept, target)
        work.parses = 0
        assert cli._load_event_paths(paths, config.machine) == cold
        assert work.parses == 1


@pytest.mark.parametrize("require_repository", (False, True))
def test_resumed_replay_rechecks_record_paths_like_a_cold_replay(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, require_repository: bool
) -> None:
    project, repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    work = _Work(monkeypatch)
    key = (config, config.machine, require_repository)

    with cli._event_memo_scope() as memo:
        first = cli._states_from_events(
            config, config.machine, require_repository=require_repository
        )
        assert first is not None
        assert key in memo.replays

        moved = repository.with_name("repo-real")
        repository.rename(moved)
        repository.symlink_to(moved, target_is_directory=True)
        with pytest.raises(cli.Refusal) as warm_refusal:
            cli._states_from_events(
                config, config.machine, require_repository=require_repository
            )
        memo_stack = list(cli._ACTIVE_EVENT_MEMO)
        cli._ACTIVE_EVENT_MEMO.clear()
        try:
            with pytest.raises(cli.Refusal) as cold_refusal:
                cli._states_from_events(
                    config, config.machine, require_repository=require_repository
                )
        finally:
            cli._ACTIVE_EVENT_MEMO.extend(memo_stack)
        assert "crosses a symlink" in str(warm_refusal.value)
        assert str(warm_refusal.value) == str(cold_refusal.value)

        repository.unlink()
        moved.rename(repository)
        work.folded.clear()
        again = cli._states_from_events(
            config, config.machine, require_repository=require_repository
        )
        assert again == first
        assert work.folded == [0]


def _warm_and_cold_refusals(
    config: cli.Config, require_repository: bool
) -> tuple[str, str]:
    """Return the refusal a memoized replay gives and the one a cold replay gives."""

    with pytest.raises(cli.Refusal) as warm:
        cli._states_from_events(
            config, config.machine, require_repository=require_repository
        )
    memo_stack = list(cli._ACTIVE_EVENT_MEMO)
    cli._ACTIVE_EVENT_MEMO.clear()
    try:
        with pytest.raises(cli.Refusal) as cold:
            cli._states_from_events(
                config, config.machine, require_repository=require_repository
            )
    finally:
        cli._ACTIVE_EVENT_MEMO.extend(memo_stack)
    return str(warm.value), str(cold.value)


def _replace_with_symlink(path: Path) -> None:
    real = path.with_name(f"{path.name}-real")
    path.rename(real)
    path.symlink_to(real, target_is_directory=True)


@pytest.mark.parametrize("require_repository", (False, True))
def test_resumed_replay_rechecks_records_that_exist_only_in_the_import(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, require_repository: bool
) -> None:
    """A production log begins with a state import that can carry many slots."""

    project, repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    replayed = cli._states_from_events(config, config.machine)
    assert replayed is not None
    active, archive = replayed
    assert [record.slot for record in active.slots] == ["memo"]
    directory = cli._event_directory(config)
    for path in cli._event_files(directory):
        path.unlink()
    cli._write_event_file(
        config,
        config.machine,
        "state-imported",
        {
            "active": cli._active_to_obj(active),
            "archive": cli._archive_to_obj(archive),
            "holds": [],
        },
    )
    assert len(cli._event_files(directory)) == 1
    work = _Work(monkeypatch)

    with cli._event_memo_scope():
        assert cli._states_from_events(
            config, config.machine, require_repository=require_repository
        ) == (active, archive)
        work.folded.clear()
        assert cli._states_from_events(
            config, config.machine, require_repository=require_repository
        ) == (active, archive)
        assert work.folded == [0]
        _replace_with_symlink(repository)
        warm, cold = _warm_and_cold_refusals(config, require_repository)

    assert "crosses a symlink" in cold
    assert warm == cold


def _append_active_record(
    config: cli.Config, record: cli.ActiveRecord, previous: cli.ActiveRecord | None
) -> None:
    state = cli._load_active(config, require_repository=False)
    cli._event_writer(config, config.machine).append(
        "active-state-recorded",
        {
            "action": "memo-test",
            "slot": record.slot,
            "previous_revision": state.revision,
            "revision": state.revision + 1,
            "previous_record_sha256": None
            if previous is None
            else cli._event_digest(cli._record_to_obj(previous)),
            "record": cli._record_to_obj(record),
            "evidence": {},
        },
    )


@pytest.mark.parametrize("require_repository", (False, True))
def test_refused_resumed_replay_leaves_the_remembered_fold_unchanged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, require_repository: bool
) -> None:
    """A replay that folds part of a suffix and then refuses must not alter the fold.

    The retry resumes from the same remembered prefix and refolds the suffix,
    so an event already applied to a shared fold would no longer match, or
    would be applied twice.
    """

    project, repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    other = repository.with_name("other")
    other.mkdir()
    _replace_with_symlink(other)
    work = _Work(monkeypatch)

    def replay() -> tuple[str, str]:
        try:
            states = cli._states_from_events(
                config, config.machine, require_repository=require_repository
            )
        except cli.Refusal as exc:
            return "refused", str(exc)
        return "ok", repr(states)

    def cold(action: Callable[[], T]) -> T:
        memo_stack = list(cli._ACTIVE_EVENT_MEMO)
        cli._ACTIVE_EVENT_MEMO.clear()
        work.counting = False
        try:
            return action()
        finally:
            work.counting = True
            cli._ACTIVE_EVENT_MEMO.extend(memo_stack)

    def replace_path(checkout: cli.Checkout, slot: str) -> cli.Checkout:
        return dataclasses.replace(checkout, path=checkout.path.replace("/memo", f"/{slot}"))

    def append_suffix() -> None:
        # The first suffix event changes a record the fold already holds, the
        # second archives a slot, and the third adds a slot whose repository
        # crosses a symlink.
        record = next(item for item in cli._load_active(config).slots if item.slot == "memo")
        changed = dataclasses.replace(record, heartbeat_at="2030-01-01T00:00:00+00:00")
        _append_active_record(config, changed, record)
        checkout = record.checkouts[0]
        finished_at = "2030-01-01T00:00:01+00:00"
        archived = replace_path(checkout, "memo-archived")
        cli._append_archive_once(
            config,
            cli._load_archive(config),
            {
                "archive_id": f"{config.machine}:memo-archived:1:{finished_at}",
                "slot": "memo-archived",
                "agent": record.agent,
                "task": record.task,
                "purpose": record.purpose,
                "slot_type": record.slot_type,
                "machine": config.machine,
                "generation": 1,
                "created_at": record.created_at,
                "finished_at": finished_at,
                "mode": "remove",
                "actor": "coordinator",
                "physical_storage": "removed",
                "validation": ["memo-test"],
                "limitations": [],
                "continuation": "none",
                "checkouts": [cli._checkout_to_obj(archived)],
            },
            require_repository=False,
        )
        added = dataclasses.replace(
            record,
            slot="memo-other",
            checkouts=(
                dataclasses.replace(replace_path(checkout, "memo-other"), repository="other"),
            ),
        )
        _append_active_record(config, added, None)

    with cli._event_memo_scope():
        assert replay()[0] == "ok"
        cold(append_suffix)
        work.folded.clear()

        refused = replay()
        assert refused == cold(replay)
        assert refused[0] == "refused" and refused[1].endswith(
            f"crosses a symlink: {other}"
        )
        other.unlink()
        other.with_name("other-real").rename(other)
        retried = replay()

        assert work.folded == [3, 3]
        assert retried == cold(replay)
        assert retried[0] == "ok"
        archive = cli._load_archive(config).records
        assert [item["slot"] for item in archive].count("memo-archived") == 1


@pytest.mark.parametrize(
    ("escaped", "refused"),
    ((("memo-b", "memo-a"), "memo-a"), (("memo-b",), "memo-b")),
)
def test_required_recheck_refuses_the_first_record_in_log_order(
    tmp_path: Path, escaped: tuple[str, ...], refused: str
) -> None:
    """Every record is rechecked, and the first failing one in log order refuses."""

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo-a", "memo-b"))
    config = cli._load_config(str(project), "testhost")

    with cli._event_memo_scope():
        assert cli._states_from_events(config, config.machine) is not None
        for slot in escaped:
            _replace_with_symlink(cli._slot_directory(config, slot, "validate"))
        warm, cold = _warm_and_cold_refusals(config, True)

    assert cold.endswith(f"crosses a symlink: {cli._slot_directory(config, refused, 'validate')}")
    assert warm == cold


@pytest.mark.parametrize("both", (True, False))
def test_unrequired_recheck_refuses_the_first_repository_in_log_order(
    tmp_path: Path, both: bool
) -> None:
    """Every repository is rechecked, and the first failing one in log order refuses."""

    project, other, _checkout = prepare_cross_repository_target(tmp_path)
    config = cli._load_config(str(project), "testhost")
    repositories = [
        checkout.repository
        for record in cli._load_active(config).slots
        for checkout in record.checkouts
    ]
    assert len(set(repositories)) == 2

    with cli._event_memo_scope():
        assert cli._states_from_events(
            config, config.machine, require_repository=False
        ) is not None
        _replace_with_symlink(other)
        if both:
            _replace_with_symlink(project / "repo")
        warm, cold = _warm_and_cold_refusals(config, False)

    assert cold.endswith(f"crosses a symlink: {project / 'repo' if both else other}")
    assert warm == cold


def test_remembered_replay_is_resumed_only_on_its_own_chain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A fold resumes only when the log still holds the chain position it folded."""

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    events = cli._load_events(config, config.machine)
    key = (config, config.machine, False)
    fold = cli._ReplayFold()

    with cli._event_memo_scope():
        cli._remember_replay(key, events[:-1], fold)
        start, resumed = cli._resume_replay(config, key, events)
        assert start == len(events) - 1
        assert resumed == fold and resumed is not fold

        # A different event at the remembered tip position is a different chain.
        forked = [*events[:-2], {**events[-2], "sha256": "0" * 64}, events[-1]]
        assert cli._resume_replay(config, key, forked)[0] == 0
        # A log shorter than the remembered prefix is never resumed.
        assert cli._resume_replay(config, key, events[:-2])[0] == 0
        # The other repository mode keeps its own fold.
        assert cli._resume_replay(config, (config, config.machine, True), events)[0] == 0

    assert cli._resume_replay(config, key, events)[0] == 0


class _ReadRecorder:
    """Record which attributes a record check reads."""

    def __init__(self, target: object, prefix: str, seen: set[str]) -> None:
        self._target = target
        self._prefix = prefix
        self._seen = seen

    def __getattr__(self, name: str) -> object:
        self._seen.add(self._prefix + name)
        value: object = getattr(self._target, name)
        if name == "checkouts":
            assert isinstance(value, tuple)
            return tuple(_ReadRecorder(item, "checkout.", self._seen) for item in value)
        return value


@pytest.mark.parametrize("require_repository", (False, True))
def test_record_path_check_key_covers_every_field_the_check_reads(
    tmp_path: Path, require_repository: bool
) -> None:
    """A resumed replay skips a duplicate check only when its inputs are equal."""

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    record = next(item for item in cli._load_active(config).slots if item.slot == "memo")
    seen: set[str] = set()

    cli._assert_record_paths(
        config,
        cast(cli.ActiveRecord, _ReadRecorder(record, "", seen)),
        require_repository=require_repository,
    )

    assert seen == {
        "slot",
        "slot_type",
        "layout",
        "checkouts",
        "checkout.name",
        "checkout.path",
        "checkout.repository",
        "checkout.remote",
        "checkout.landed_ref",
    }
    key = cli._record_path_check_key(record)
    first = record.checkouts[0]
    rest = record.checkouts[1:]
    changed_records = {
        "slot": dataclasses.replace(record, slot=f"{record.slot}-changed"),
        "slot_type": dataclasses.replace(record, slot_type=f"{record.slot_type}-changed"),
        "layout": dataclasses.replace(record, layout=f"{record.layout}-changed"),
        "checkout.name": dataclasses.replace(
            record, checkouts=(dataclasses.replace(first, name=f"{first.name}-x"), *rest)
        ),
        "checkout.path": dataclasses.replace(
            record, checkouts=(dataclasses.replace(first, path=f"{first.path}-x"), *rest)
        ),
        "checkout.repository": dataclasses.replace(
            record,
            checkouts=(dataclasses.replace(first, repository=f"{first.repository}-x"), *rest),
        ),
        "checkout.remote": dataclasses.replace(
            record, checkouts=(dataclasses.replace(first, remote=f"{first.remote}-x"), *rest)
        ),
        "checkout.landed_ref": dataclasses.replace(
            record,
            checkouts=(dataclasses.replace(first, landed_ref=f"{first.landed_ref}-x"), *rest),
        ),
    }
    assert set(changed_records) == seen - {"checkouts"}
    for name, changed in changed_records.items():
        assert cli._record_path_check_key(changed) != key, name
    # Fields the check never reads must not defeat the duplicate skip.
    unread = dataclasses.replace(
        record,
        heartbeat_at="1970-01-01T00:00:00Z",
        generation=record.generation + 1,
        checkouts=tuple(
            dataclasses.replace(checkout, head="0" * 40, branch="other")
            for checkout in record.checkouts
        ),
    )
    assert cli._record_path_check_key(unread) == key


def test_unrequired_repository_record_check_touches_storage_only_for_the_repository(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A resumed replay rechecks only repositories when paths are not required.

    That is sound only while the rest of the check never inspects storage.
    """

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    record = next(item for item in cli._load_active(config).slots if item.slot == "memo")
    references: list[str] = []

    def reference(_config: cli.Config, raw: str) -> tuple[str, Path, Path]:
        references.append(raw)
        return raw, config.root / raw, config.root

    def storage(*_args: object, **_kwargs: object) -> None:
        raise AssertionError("record path check inspected storage")

    with monkeypatch.context() as patch:
        patch.setattr(cli, "_stored_repository_reference", reference)
        for name in ("lstat", "stat", "readlink", "scandir", "listdir", "open", "access"):
            patch.setattr(os, name, storage)
        patch.setattr("builtins.open", storage)
        cli._assert_record_paths(config, record, require_repository=False)

    assert references == [checkout.repository for checkout in record.checkouts]


# The tests below pin the verified-chain memo.  A read-only
# classify-create-journals loads the whole log seven times in one invocation.


def _classify_argv(project: Path) -> list[str]:
    return [
        "--project-root",
        str(project),
        "classify-create-journals",
        "probe-slot",
        "--slot-type",
        "validate",
        "--agent",
        "validate-probe-slot",
        "--format",
        "json",
    ]


def _cold(action: Callable[[], T]) -> T:
    """Run an action with no memo scope, as a fresh command would."""

    memo_stack = list(cli._ACTIVE_EVENT_MEMO)
    cli._ACTIVE_EVENT_MEMO.clear()
    try:
        return action()
    finally:
        cli._ACTIVE_EVENT_MEMO.extend(memo_stack)


def _chain_key(config: cli.Config) -> tuple[str, str]:
    return (str(cli._event_directory(config)), config.machine)


def test_classify_create_journals_parses_each_event_once(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The read-only entry command parses each event file once however often it loads.

    A production report counted seven parses of every one of 16,155 event files
    in one invocation of an older release, which spent most of a caller's
    30-second entry budget.
    """

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo-a", "memo-b"))
    events = _event_paths(project)
    _treat_every_event_as_settled(monkeypatch)
    work = _Work(monkeypatch)
    loads = 0
    load = cli._load_events

    def counted_load(
        config: cli.Config, machine: str | None = None
    ) -> tuple[dict[str, object], ...]:
        nonlocal loads
        loads += 1
        return load(config, machine)

    monkeypatch.setattr(cli, "_load_events", counted_load)

    assert cli.main(_classify_argv(project)) == 0

    assert json.loads(capsys.readouterr().out)["journals"] == []
    assert loads >= 6
    assert work.parses == len(events)


def test_classify_create_journals_reuses_the_verified_chain_and_still_stats_every_file(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Later loads return the chain the first load verified, after an lstat of every file.

    The first load checks each file's identity before and after parsing it;
    every later load checks each file once.  Checking fewer files would let an
    earlier event rewritten between two loads go unnoticed until the next
    command.
    """

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo-a", "memo-b"))
    events = _event_paths(project)
    _treat_every_event_as_settled(monkeypatch)
    loaded: list[tuple[dict[str, object], ...]] = []
    load = cli._load_events
    identity = cli._event_file_identity
    identity_checks = 0

    def recorded_load(
        config: cli.Config, machine: str | None = None
    ) -> tuple[dict[str, object], ...]:
        result = load(config, machine)
        loaded.append(result)
        return result

    def counted_identity(path: str | Path) -> tuple[int, int, int, int, int] | None:
        nonlocal identity_checks
        identity_checks += 1
        return identity(path)

    monkeypatch.setattr(cli, "_load_events", recorded_load)
    monkeypatch.setattr(cli, "_event_file_identity", counted_identity)

    assert cli.main(_classify_argv(project)) == 0

    assert json.loads(capsys.readouterr().out)["journals"] == []
    config = cli._load_config(str(project), "testhost")
    cold = load(config, config.machine)
    assert len(loaded) >= 6
    assert all(result == cold for result in loaded)
    assert all(result is loaded[0] for result in loaded)
    assert identity_checks == (len(loaded) + 1) * len(events)


def test_an_append_inside_a_command_loads_exactly_what_a_cold_load_loads(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    _treat_every_event_as_settled(monkeypatch)
    key = _chain_key(config)

    with cli._event_memo_scope() as memo:
        before = cli._load_events(config)
        assert memo.chains[key].events is before
        record = next(item for item in cli._load_active(config).slots if item.slot == "memo")
        changed = dataclasses.replace(record, heartbeat_at="2030-01-01T00:00:00+00:00")
        _append_active_record(config, changed, record)
        # The writer forgets the chain before the directory changes.
        assert key not in memo.chains

        warm = cli._load_events(config)
        cold = _cold(lambda: cli._load_events(config))
        assert warm == cold
        assert len(warm) == len(before) + 1 and warm[:-1] == before
        assert warm[-1]["kind"] == "active-state-recorded"
        assert memo.chains[key].events is warm
        assert cli._load_events(config) is warm
        assert cli._states_from_events(config, config.machine) == _cold(
            lambda: cli._states_from_events(config, config.machine)
        )
        assert next(
            item for item in cli._load_active(config).slots if item.slot == "memo"
        ).heartbeat_at == "2030-01-01T00:00:00+00:00"


def test_a_recovered_event_inside_a_command_loads_exactly_what_a_cold_load_loads(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Recovery renames an event into place without the writer; the load must see it."""

    project, _repository, _remote = make_project(tmp_path)
    config = cli._load_config(str(project), "testhost")
    written = cli._write_event_file(
        config, config.machine, "test-observation", {"fact": "complete before rename"}
    )
    event_path = cli._event_directory(config) / f"{written['sequence']:020d}.json"
    event_path.rename(event_path.with_name(f"{event_path.name}.tmp.crash"))
    _treat_every_event_as_settled(monkeypatch)
    key = _chain_key(config)

    with cli._event_memo_scope() as memo:
        before = cli._load_events(config)
        assert memo.chains[key].events is before
        assert cli._recover_partial_updates(config, True)
        warm = cli._load_events(config)
        cold = _cold(lambda: cli._load_events(config))

    assert "recovered append-only event" in capsys.readouterr().out
    assert warm == cold
    assert len(warm) == len(before) + 1 and warm[:-1] == before
    assert warm[-1] == written


def _rewrite_in_place(path: Path, original: bytes, replacement: bytes) -> None:
    """Rewrite an event file in place, keeping its inode, size and mtime."""

    before = os.lstat(path)
    contents = path.read_bytes()
    assert contents.count(original) == 1
    rewritten = contents.replace(original, replacement)
    assert len(rewritten) == len(contents) and rewritten != contents
    with path.open("r+b") as handle:
        handle.write(rewritten)
    os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
    after = os.lstat(path)
    assert (after.st_ino, after.st_size, after.st_mtime_ns) == (
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
    )


def _corrupt(paths: Sequence[Path], events: Sequence[dict[str, object]], kind: str) -> str:
    """Corrupt the second event of the chain and return the refusal a load must give."""

    target = paths[1]
    digest = cast(str, events[1]["sha256"])
    if kind == "digest":
        other = ("0" if digest[0] != "0" else "1") + digest[1:]
        _rewrite_in_place(target, digest.encode(), other.encode())
        return f"append-only event digest does not match its content: {target}"
    if kind == "link":
        # Keep the event's own digest valid so only the link to its predecessor breaks.
        previous = cast(str, events[1]["previous_sha256"])
        other_previous = ("0" if previous[0] != "0" else "1") + previous[1:]
        core = {
            key: value
            for key, value in events[1].items()
            if key != "sha256"
        }
        core["previous_sha256"] = other_previous
        _rewrite_in_place(target, previous.encode(), other_previous.encode())
        _rewrite_in_place(target, digest.encode(), cli._event_digest(core).encode())
        return f"append-only event hash chain is broken at {target}"
    if kind == "gap":
        target.unlink()
        return (
            f"append-only event log has a sequence gap at {paths[2]}; "
            "preserve the log and run 'wrkslots doctor'"
        )
    assert kind == "stray-entry"
    stray = target.with_name("stray")
    stray.write_text("not an event\n", encoding="utf-8")
    return (
        f"append-only event log contains an unexpected entry: {stray}; "
        "preserve it and run 'wrkslots doctor' before changing slot state"
    )


@pytest.mark.parametrize("kept_chain", (False, True), ids=("first-load", "after-kept-chain"))
@pytest.mark.parametrize("kind", ("digest", "link", "gap", "stray-entry"))
def test_a_corrupted_chain_is_refused_on_the_first_load_and_after_a_kept_chain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, kind: str, kept_chain: bool
) -> None:
    """Every refusal a cold load gives is given inside a command, before or after a kept chain.

    The corrupted event is not the last one: a kept chain must not be trusted on
    the identity of its tail alone.
    """

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    paths = _event_paths(project)
    assert len(paths) >= 3
    events = cli._load_events(config)
    _treat_every_event_as_settled(monkeypatch)
    key = _chain_key(config)

    with cli._event_memo_scope() as memo:
        if kept_chain:
            assert cli._load_events(config) == events
            assert memo.chains[key].events == events
        expected = _corrupt(paths, events, kind)
        with pytest.raises(cli.StateError) as warm:
            cli._load_events(config)
        if kind != "stray-entry":
            # A refused verification keeps no chain.  A stray entry is refused
            # by the listing, which every load performs before any chain lookup.
            assert key not in memo.chains
        with pytest.raises(cli.StateError) as again:
            cli._load_events(config)
        with pytest.raises(cli.StateError) as cold:
            _cold(lambda: cli._load_events(config))

    assert str(cold.value) == expected
    assert str(warm.value) == expected
    assert str(again.value) == expected


def test_a_verified_chain_is_kept_only_when_every_event_has_settled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The chain inherits the per-file change-time window."""

    project, _repository, _remote = make_project(tmp_path)
    prepare_dead_validate_slots(project, ("memo",))
    config = cli._load_config(str(project), "testhost")
    paths = _event_paths(project)
    changed = [os.lstat(path).st_ctime_ns for path in paths]
    clock = [max(changed) + cli._EVENT_MEMO_STABLE_NS - 1]
    monkeypatch.setattr(cli, "_event_memo_clock_ns", lambda: clock[0])
    recent = sum(1 for ns in changed if ns + cli._EVENT_MEMO_STABLE_NS > clock[0])
    assert 1 <= recent < len(paths)
    work = _Work(monkeypatch)
    key = _chain_key(config)

    with cli._event_memo_scope() as memo:
        first = cli._load_events(config)
        assert work.parses == len(paths)
        assert memo.chains == {}
        work.parses = 0
        assert cli._load_events(config) == first
        assert work.parses == recent
        assert memo.chains == {}

        clock[0] = max(changed) + cli._EVENT_MEMO_STABLE_NS
        work.parses = 0
        kept = cli._load_events(config)
        assert kept == first
        assert work.parses == recent
        assert memo.chains[key].events is kept
        work.parses = 0
        assert cli._load_events(config) is kept
        assert work.parses == 0
