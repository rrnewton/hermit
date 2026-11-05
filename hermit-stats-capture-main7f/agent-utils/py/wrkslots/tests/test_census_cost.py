"""The registry audit and the process census must not re-ask settled questions.

Every test here pins a COST property and an EQUIVALENCE property together,
because a cheaper census is only worth having if it answers what the expensive
one answered. The cost assertions are counts of work -- Git processes, parses --
never wall time, so they mean the same thing on a loaded box as on an idle one.
"""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import subprocess
from collections.abc import Callable, Iterator
from dataclasses import replace
from pathlib import Path

import pytest

from wrkslots import cli


def _config(tmp_path: Path, *, batch: bool = False) -> cli.Config:
    root = tmp_path / "project"
    worktrees = root / "worktrees"
    root.mkdir(exist_ok=True)
    worktrees.mkdir(exist_ok=True)
    liveness = root / "liveness.py"
    liveness.write_text("raise SystemExit(2)\n", encoding="utf-8")
    batch_path = root / "liveness-batch.py"
    if batch:
        batch_path.write_text("raise SystemExit(2)\n", encoding="utf-8")
    return cli.Config(
        root=root,
        config_path=root / ".wrkslots.yml",
        worktrees=worktrees,
        control=worktrees,
        machine="testhost",
        default_remote="origin",
        default_landed_ref="refs/remotes/origin/main",
        heartbeat_ttl_seconds=3600,
        liveness_command=liveness,
        liveness_batch_command=batch_path if batch else None,
    )


def _records(count: int) -> tuple[cli.ActiveRecord, ...]:
    coordinator = cli.ProcessIdentity(1, 1, "boot", "host", "/fixture")
    return tuple(
        cli.ActiveRecord(
            slot=f"slot-{index:03d}",
            agent=f"agent-{index:03d}",
            task=f"task-{index:03d}",
            purpose="batch equivalence fixture",
            slot_type="agent",
            machine="testhost",
            generation=1,
            created_at="2026-01-01T00:00:00+00:00",
            heartbeat_at="2026-01-01T00:00:00+00:00",
            heartbeat_ttl_seconds=3600,
            owner=None,
            coordinator_lease=coordinator,
            coordinator_recovery_note=None,
            handoff=None,
            checkouts=(),
        )
        for index in range(count)
    )


def _git(repository: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


@pytest.fixture
def repository(tmp_path: Path) -> Path:
    root = tmp_path / "repository"
    root.mkdir()
    _git(root, "init", "--initial-branch=main")
    _git(root, "config", "user.email", "test@example.invalid")
    _git(root, "config", "user.name", "Test")
    (root / "file.txt").write_text("contents\n", encoding="utf-8")
    _git(root, "add", "file.txt")
    _git(root, "commit", "-m", "initial")
    return root


def test_worktree_identity_matches_the_three_separate_questions(
    repository: Path,
) -> None:
    """The merged call must return exactly what the separate spellings return."""

    vcs = cli._GitVcs()

    root, common, head = vcs.worktree_identity(repository)

    assert root == vcs.repository_root(repository)
    assert common == vcs.common_directory(repository)
    assert head == vcs.head(repository)


def test_worktree_identity_matches_on_a_linked_worktree(
    repository: Path, tmp_path: Path
) -> None:
    """A linked worktree is the shape the registry audit actually asks about.

    Its toplevel and its common directory DIFFER, so this is the case that would
    catch a merged call returning the two values in the wrong order -- which the
    single-repository case above cannot see.
    """

    linked = tmp_path / "linked"
    _git(repository, "worktree", "add", "--detach", str(linked), "HEAD")
    vcs = cli._GitVcs()

    root, common, head = vcs.worktree_identity(linked)

    assert root == linked.absolute()
    assert common == (repository / ".git").absolute()
    assert root != common
    assert head == vcs.head(repository)


def test_worktree_identity_uses_one_git_process_for_all_three(
    repository: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The whole point: one process, not three.

    This is the control. Restore the three separate calls and this fails, which
    the equivalence tests above would not.
    """

    invocations: list[list[str]] = []
    original = cli._GitVcs._run

    def counting(
        target: Path,
        arguments: list[str],
        **keywords: object,
    ) -> subprocess.CompletedProcess[str]:
        invocations.append(list(arguments))
        return original(target, arguments, **keywords)  # type: ignore[arg-type]

    monkeypatch.setattr(cli._GitVcs, "_run", staticmethod(counting))

    cli._GitVcs().worktree_identity(repository)

    assert len(invocations) == 1, invocations


def test_worktree_identity_refuses_an_unborn_head_like_the_separate_call(
    tmp_path: Path,
) -> None:
    """A refusal the merged call must keep: HEAD that resolves to no commit."""

    empty = tmp_path / "empty"
    empty.mkdir()
    _git(empty, "init", "--initial-branch=main")
    vcs = cli._GitVcs()

    with pytest.raises(cli.Refusal):
        vcs.head(empty)
    with pytest.raises(cli.Refusal):
        vcs.worktree_identity(empty)


def test_worktree_identity_refuses_a_directory_that_is_not_a_repository(
    tmp_path: Path,
) -> None:
    plain = tmp_path / "plain"
    plain.mkdir()

    with pytest.raises(cli.Refusal):
        cli._GitVcs().worktree_identity(plain)


def test_target_match_cache_agrees_with_the_uncached_walk() -> None:
    """Same answers, hits and misses alike."""

    slot = Path("/slots/validate-fresh-one")
    targets = {slot: "validate-fresh-one"}
    probes = [
        slot,
        slot / "nested" / "deep",
        Path("/"),
        Path("/slots"),
        Path("/slots/validate-fresh-one-but-longer"),
        Path("/unrelated/path"),
        Path("relative/not/absolute"),
    ]
    cache: dict[Path, tuple[Path, str] | None] = {}

    for probe in probes:
        uncached = cli._matching_absent_validate_target(probe, targets)
        cached = cli._matching_absent_validate_target(probe, targets, cache)
        assert cached == uncached, probe
        # Second ask comes from the memo and must not differ.
        assert cli._matching_absent_validate_target(probe, targets, cache) == uncached


def test_target_match_cache_remembers_a_negative_answer() -> None:
    """A stored `None` is a real answer.

    Written because `.get` cannot tell "cached: matches nothing" from "not yet
    asked", and the census is DOMINATED by references that match nothing -- so a
    `.get`-based memo would silently re-walk the common case and buy nothing.
    """

    lookups: list[Path] = []

    class CountingTargets(dict[Path, str]):
        def get(  # type: ignore[override]
            self, key: Path, default: str | None = None
        ) -> str | None:
            lookups.append(key)
            return super().get(key, default)

    targets = CountingTargets({Path("/slots/one"): "one"})
    cache: dict[Path, tuple[Path, str] | None] = {}
    probe = Path("/elsewhere/entirely")

    assert cli._matching_absent_validate_target(probe, targets, cache) is None
    walked = len(lookups)
    assert walked > 0
    assert cli._matching_absent_validate_target(probe, targets, cache) is None

    # The second ask must have come from the memo: no further target lookups.
    assert len(lookups) == walked, lookups
    assert cache[probe] is None


def test_mountinfo_parse_cache_agrees_with_the_uncached_parse() -> None:
    table = (
        "24 30 0:22 / /proc rw,nosuid - proc proc rw\n"
        "25 30 0:5 / /dev rw,nosuid - devtmpfs devtmpfs rw\n"
        "26 30 259:2 /slots/validate-fresh-one /mnt/x rw - btrfs /dev/one rw\n"
    )
    cache: dict[str, tuple[tuple[Path, str], ...]] = {}

    uncached = cli._parse_mountinfo_paths(table, "probe")
    cached = cli._parse_mountinfo_paths(table, "probe", cache)

    assert cached == uncached
    assert cli._parse_mountinfo_paths(table, "probe", cache) == uncached


def test_mountinfo_parse_cache_reuses_one_parse_across_identical_tables() -> None:
    """The control for the memo that matters most on a fleet host.

    Nearly every process there has its own mount namespace listing the same
    table, so the parse must happen once per DISTINCT table rather than once per
    process. Remove the cache and this fails.
    """

    table = "24 30 0:22 / /proc rw,nosuid - proc proc rw\n"
    other = "24 30 0:22 / /sys rw,nosuid - sysfs sysfs rw\n"
    cache: dict[str, tuple[tuple[Path, str], ...]] = {}

    for pid in range(50):
        cli._parse_mountinfo_paths(table, f"mount evidence for PID {pid}", cache)
    cli._parse_mountinfo_paths(other, "mount evidence for PID 99", cache)

    assert set(cache) == {table, other}


def test_mountinfo_parse_cache_never_stores_a_refusal(
) -> None:
    """A malformed table must refuse once PER PROCESS, naming that process.

    Caching the failure would report the first PID's name for every later one,
    which is exactly the kind of misattributed evidence this census exists to
    avoid.
    """

    malformed = "this line has no field separator\n"
    cache: dict[str, tuple[tuple[Path, str], ...]] = {}

    with pytest.raises(cli.Refusal, match="PID 11"):
        cli._parse_mountinfo_paths(malformed, "mount evidence for PID 11", cache)
    with pytest.raises(cli.Refusal, match="PID 22"):
        cli._parse_mountinfo_paths(malformed, "mount evidence for PID 22", cache)

    assert cache == {}


@pytest.mark.parametrize("row_count", (64, 128, 256))
def test_batch_liveness_matches_per_subject_results_with_one_invocation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    row_count: int,
) -> None:
    config = _config(tmp_path, batch=True)
    records = _records(row_count)
    calls = 0

    def run(
        _command: list[str],
        *,
        input_data: bytes,
        **_kwargs: object,
    ) -> tuple[int, bytes, bytes]:
        nonlocal calls
        calls += 1
        request = json.loads(input_data)
        results = []
        codes = {"dead": 0, "alive": 1, "unverifiable": 2}
        for index, subject in enumerate(request["subjects"]):
            state = ("dead", "alive", "unverifiable")[index % 3]
            results.append(
                {
                    "subject_id": subject["subject_id"],
                    "agent": subject["agent"],
                    "state": state,
                    "detail": f"fixture {state} agent={subject['agent']} rc={codes[state]}",
                }
            )
        response = {
            "schema": cli._LIVENESS_BATCH_RESPONSE_SCHEMA,
            "request_sha256": request["request_sha256"],
            "results": results,
        }
        return 0, json.dumps(response).encode("utf-8"), b""

    monkeypatch.setattr(cli, "_run_bounded_read_only_command", run)

    def run_legacy(
        command: list[str],
        **_kwargs: object,
    ) -> subprocess.CompletedProcess[str]:
        index = int(command[-1].rsplit("-", 1)[1])
        state = ("dead", "alive", "unverifiable")[index % 3]
        code = {"dead": 0, "alive": 1, "unverifiable": 2}[state]
        return subprocess.CompletedProcess(
            command,
            code,
            f"fixture {state} agent={command[-1]} rc={code}\n",
            "",
        )

    monkeypatch.setattr(subprocess, "run", run_legacy)

    observed = cli._registered_liveness_states(config, records)
    legacy = cli._registered_liveness_states(
        replace(config, liveness_batch_command=None), records
    )

    assert calls == 1
    assert observed == legacy
    assert len(observed) == row_count


@pytest.mark.parametrize("mutation", ("missing", "duplicate", "misbound", "wrong-request"))
def test_batch_liveness_goalpost_mutations_fail_closed(
    tmp_path: Path,
    mutation: str,
) -> None:
    config = _config(tmp_path, batch=True)
    request = cli._liveness_batch_request(config, _records(2))
    subjects = request["subjects"]
    assert isinstance(subjects, list)
    results = [
        {
            "subject_id": subject["subject_id"],
            "agent": subject["agent"],
            "state": "dead",
            "detail": "fixture dead",
        }
        for subject in subjects
    ]
    response = {
        "schema": cli._LIVENESS_BATCH_RESPONSE_SCHEMA,
        "request_sha256": request["request_sha256"],
        "results": results,
    }
    if mutation == "missing":
        results.pop()
    elif mutation == "duplicate":
        results.append(dict(results[0]))
    elif mutation == "misbound":
        results[0]["agent"] = "another-agent"
    else:
        response["request_sha256"] = "0" * 64

    with pytest.raises(cli.Refusal):
        cli._parse_liveness_batch_response(json.dumps(response), request)


def test_batch_liveness_malformed_response_makes_every_subject_unverifiable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path, batch=True)
    records = _records(4)
    monkeypatch.setattr(
        cli,
        "_run_bounded_read_only_command",
        lambda _command, **_kwargs: (0, b"not-json", b""),
    )

    observed = cli._registered_liveness_states(config, records)

    assert set(state for state, _detail in observed.values()) == {"unverifiable"}
    assert all("not JSON" in detail for _state, detail in observed.values())


def test_batch_liveness_duplicate_state_member_makes_whole_batch_unverifiable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path, batch=True)
    records = _records(3)

    def conflicting_response(
        _command: list[str], *, input_data: bytes, **_kwargs: object
    ) -> tuple[int, bytes, bytes]:
        request = json.loads(input_data)
        results = [
            {
                "subject_id": subject["subject_id"],
                "agent": subject["agent"],
                "state": "dead",
                "detail": "fixture dead",
            }
            for subject in request["subjects"]
        ]
        response = json.dumps(
            {
                "schema": cli._LIVENESS_BATCH_RESPONSE_SCHEMA,
                "request_sha256": request["request_sha256"],
                "results": results,
            }
        ).replace(
            '"state": "dead"',
            '"state": "unverifiable", "state": "dead"',
            1,
        )
        return 0, response.encode("utf-8"), b""

    monkeypatch.setattr(cli, "_run_bounded_read_only_command", conflicting_response)

    observed = cli._registered_liveness_states(config, records)

    assert set(state for state, _detail in observed.values()) == {"unverifiable"}
    assert all("duplicate key 'state'" in detail for _state, detail in observed.values())


def test_cache_census_partial_progress_is_bounded_then_exactly_finalized(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    for index in range(9):
        (cache / f"file-{index}").write_bytes(bytes([index]) * (index + 1))
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    planned = {
        "subject": (
            cli.CacheDirectory(
                path=cache,
                checkout_root=checkout,
                checkout_device=checkout_identity[0],
                checkout_inode=checkout_identity[1],
                checkout_mount_id=checkout_identity[2],
            ),
        )
    }
    states = (cli.ActiveState("testhost", 7, ()),)
    state_path = tmp_path / "cache-state.json"
    statuses: list[str] = []
    work: list[int] = []

    for _ in range(20):
        measured, counters = cli._audit_cache_census(
            config,
            states,
            planned,
            {},
            state_path=state_path,
            work_limit=2,
            wall_seconds=5,
        )
        statuses.append(measured["subject"].status)
        work.append(counters["work_consumed"])
        if measured["subject"].status != "partial":
            break

    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert "exceeding the fixed allowance" in (measured["subject"].error or "")
    measured, final_counters = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )
    assert final_counters["work_consumed"] <= 100
    statuses.append(measured["subject"].status)

    expected = sum(path.stat().st_blocks * 512 for path in (cache, *cache.iterdir()))
    assert statuses[0] == "partial"
    assert statuses[-1] == "complete"
    assert measured["subject"].bytes == expected
    assert all(value <= 2 for value in work)
    assert state_path.is_file()


def test_cache_census_identity_change_never_reuses_completed_bytes(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    (cache / "old").write_bytes(b"old")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")

    def planned() -> dict[str, tuple[cli.CacheDirectory, ...]]:
        return {
            "subject": (
                cli.CacheDirectory(
                    path=cache,
                    checkout_root=checkout,
                    checkout_device=checkout_identity[0],
                    checkout_inode=checkout_identity[1],
                    checkout_mount_id=checkout_identity[2],
                ),
            )
        }

    states = (cli.ActiveState("testhost", 9, ()),)
    state_path = tmp_path / "cache-state.json"
    first, _work = cli._audit_cache_census(
        config,
        states,
        planned(),
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )
    assert first["subject"].status == "complete"
    old_bytes = first["subject"].bytes

    shutil.rmtree(cache)
    cache.mkdir()
    for index in range(5):
        (cache / f"new-{index}").write_bytes(b"new contents")
    second, _work = cli._audit_cache_census(
        config,
        states,
        planned(),
        {},
        state_path=state_path,
        work_limit=1,
        wall_seconds=5,
    )

    assert second["subject"].status == "partial"
    assert second["subject"].bytes is None
    assert second["subject"].bytes != old_bytes
    persisted = json.loads(state_path.read_text(encoding="utf-8"))
    assert len(persisted["roots"]) == 1


def test_cache_census_refuses_mutated_resume_path_outside_root(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    (cache / "inside").write_bytes(b"inside")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    planned = {
        "subject": (
            cli.CacheDirectory(
                path=cache,
                checkout_root=checkout,
                checkout_device=checkout_identity[0],
                checkout_inode=checkout_identity[1],
                checkout_mount_id=checkout_identity[2],
            ),
        )
    }
    states = (cli.ActiveState("testhost", 11, ()),)
    state_path = tmp_path / "cache-state.json"
    first, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=1,
        wall_seconds=5,
    )
    assert first["subject"].status == "partial"
    state = json.loads(state_path.read_text(encoding="utf-8"))
    root = next(iter(state["roots"].values()))
    root["pending"] = [".."]
    root["current"] = None
    root["status"] = "partial"
    state_path.write_text(json.dumps(state), encoding="utf-8")

    observed, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=10,
        wall_seconds=5,
    )

    assert observed["subject"].status == "complete"
    assert observed["subject"].bytes == cli._allocated_cache_bytes(
        config, planned["subject"][0]
    )


def test_cache_census_large_directory_requires_one_bounded_final_sweep(
    tmp_path: Path,
) -> None:
    entry_count = 128
    work_limit = 32
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    for index in range(entry_count):
        (cache / f"artifact-{index:04d}").write_bytes(b"x")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    planned = {
        "subject": (
            cli.CacheDirectory(
                path=cache,
                checkout_root=checkout,
                checkout_device=checkout_identity[0],
                checkout_inode=checkout_identity[1],
                checkout_mount_id=checkout_identity[2],
            ),
        )
    }
    states = (cli.ActiveState("testhost", 12, ()),)
    state_path = tmp_path / "large-cache-state.json"
    consumed: list[int] = []

    for _ in range(10):
        measured, counters = cli._audit_cache_census(
            config,
            states,
            planned,
            {},
            state_path=state_path,
            work_limit=work_limit,
            wall_seconds=5,
        )
        consumed.append(counters["work_consumed"])
        if measured["subject"].status != "partial":
            break

    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert "exceeding the fixed allowance" in (measured["subject"].error or "")
    measured, final_counters = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=entry_count * 2,
        wall_seconds=5,
    )
    assert final_counters["work_consumed"] <= entry_count * 2

    assert measured["subject"].status == "complete"
    assert all(0 < work <= work_limit for work in consumed)
    assert len(consumed) <= 10


def test_cache_census_preserves_nested_git_metadata_refusal(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    nested = cache / "nested"
    nested.mkdir(parents=True)
    (nested / ".git").mkdir()
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )

    with pytest.raises(cli.Refusal, match="nested Git metadata"):
        cli._allocated_cache_bytes(config, directory)
    measured, _work = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 13, ()),),
        {"subject": (directory,)},
        {},
        state_path=tmp_path / "nested-git-state.json",
        work_limit=100,
        wall_seconds=5,
    )

    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert measured["subject"].error is not None
    assert "nested Git metadata" in measured["subject"].error


def test_refused_cache_roots_debit_one_shared_budget_immediately(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    planned: dict[str, tuple[cli.CacheDirectory, ...]] = {}
    for index in range(4):
        checkout = config.root / f"checkout-{index}"
        cache = checkout / "target"
        (cache / ".git").mkdir(parents=True)
        identity = cli._open_directory_identity(checkout, "checkout")
        planned[str(index)] = (cli.CacheDirectory(cache, checkout, *identity),)

    actual_directory_stream = cli._audit_directory_stream

    @contextlib.contextmanager
    def significant_directory_stream(
        directory_fd: int,
        cursor: int,
        label: str,
    ) -> Iterator[Callable[[], tuple[str, int] | None]]:
        with actual_directory_stream(directory_fd, cursor, label) as next_entry:

            def next_significant_entry() -> tuple[str, int] | None:
                while True:
                    item = next_entry()
                    if item is None or item[0] not in {".", ".."}:
                        return item

            yield next_significant_entry

    # Dot entries and their order are filesystem details, not part of this cost contract.
    # Present only significant entries so every nested-Git refusal has the same one-unit cost.
    monkeypatch.setattr(cli, "_audit_directory_stream", significant_directory_stream)

    measured, counters = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 24, ()),),
        planned,
        {},
        state_path=tmp_path / "refused-budget.json",
        work_limit=3,
        wall_seconds=5,
    )

    assert counters["work_consumed"] == 3
    assert counters["work_remaining"] == 0
    assert counters["directories_visited"] == 3
    assert sum(item.status == "error" for item in measured.values()) == 3
    assert sum(item.status == "partial" for item in measured.values()) == 1


def test_cache_census_persisted_cursor_prevents_stable_root_starvation(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    planned: dict[str, tuple[cli.CacheDirectory, ...]] = {}
    for index in range(3):
        checkout = config.root / f"checkout-{index}"
        cache = checkout / "target"
        cache.mkdir(parents=True)
        (cache / "artifact").write_bytes(b"x")
        identity = cli._open_directory_identity(checkout, "checkout")
        planned[f"subject-{index}"] = (
            cli.CacheDirectory(cache, checkout, *identity),
        )
    states = (cli.ActiveState("testhost", 25, ()),)
    state_path = tmp_path / "fair-cursor.json"
    completed: set[str] = set()
    cursors: list[str | None] = []

    for _ in range(6):
        measured, counters = cli._audit_cache_census(
            config,
            states,
            planned,
            {},
            state_path=state_path,
            work_limit=10,
            wall_seconds=5,
        )
        assert counters["work_consumed"] <= 10
        completed.update(
            subject for subject, item in measured.items() if item.status == "complete"
        )
        cursors.append(json.loads(state_path.read_text(encoding="utf-8"))["next_subject"])
        if completed == set(planned) and len(set(cursors)) == len(planned):
            break
    else:
        pytest.fail(
            f"persisted cursor did not visit every subject: "
            f"completed={completed}, cursors={cursors}"
        )

    assert completed == set(planned)
    assert len(set(cursors)) == 3


def test_completed_cache_census_revalidates_nested_directories(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    nested = cache / "nested"
    nested.mkdir(parents=True)
    (nested / "artifact").write_bytes(b"artifact")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    states = (cli.ActiveState("testhost", 14, ()),)
    planned = {"subject": (directory,)}
    state_path = tmp_path / "nested-mutation-state.json"
    complete, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )
    assert complete["subject"].status == "complete"

    (nested / ".git").mkdir()
    changed, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert changed["subject"].status == "error"
    assert changed["subject"].bytes is None
    assert changed["subject"].error is not None
    assert "nested Git metadata" in changed["subject"].error


@pytest.mark.parametrize("forged_status", ("complete", "partial"))
def test_forged_cache_state_cannot_bypass_fresh_recursive_verification(
    tmp_path: Path,
    forged_status: str,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    (cache / ".git").mkdir()
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    states = (cli.ActiveState("testhost", 15, ()),)
    identity = cli._audit_cache_root_identity(config, directory)
    assert identity is not None
    key = cli._audit_cache_root_key(directory, identity)
    forged = cli._new_audit_cache_root(directory, identity)
    metadata = cache.stat()
    forged.update(
        {
            "phase": "finalize" if forged_status == "complete" else "verify",
            "bytes": 0,
            "verify_bytes": 0,
            "pending": [],
            "current": None,
            "status": forged_status,
            "directories_visited": 1,
            "directories_verified": 1,
            "verified_directories": [
                {
                    "path": ".",
                    "identity": [
                        metadata.st_dev,
                        metadata.st_ino,
                        metadata.st_mtime_ns,
                        metadata.st_ctime_ns,
                    ],
                }
            ],
            "finalize_index": 1 if forged_status == "complete" else 0,
            "directories_finalized": 1 if forged_status == "complete" else 0,
        }
    )
    state_path = tmp_path / "forged-complete.json"
    state_path.write_text(
        json.dumps(
            {
                "schema": cli._AUDIT_CACHE_CENSUS_SCHEMA,
                "registry_revision": cli._audit_registry_revision(states),
                "roots": {key: forged},
            }
        ),
        encoding="utf-8",
    )

    measured, _work = cli._audit_cache_census(
        config,
        states,
        {"subject": (directory,)},
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert measured["subject"].error is not None
    assert "nested Git metadata" in measured["subject"].error
    key_path = state_path.with_name(f"{state_path.name}.key")
    assert key_path.stat().st_mode & 0o777 == 0o600


def test_cache_census_finalization_rechecks_earlier_verification_chunks(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    for name in ("first", "second"):
        directory = cache / name
        directory.mkdir(parents=True)
        (directory / "artifact").write_bytes(name.encode("ascii"))
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    cache_directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    states = (cli.ActiveState("testhost", 18, ()),)
    planned = {"subject": (cache_directory,)}
    state_path = tmp_path / "cross-chunk-mutation.json"
    changed: Path | None = None

    for _ in range(40):
        measured, _work = cli._audit_cache_census(
            config,
            states,
            planned,
            {},
            state_path=state_path,
            work_limit=1,
            wall_seconds=5,
        )
        state = json.loads(state_path.read_text(encoding="utf-8"))
        root = next(iter(state["roots"].values()))
        current = root.get("current")
        current_path = current.get("path") if isinstance(current, dict) else None
        completed_nested = [
            item["path"]
            for item in root["verified_directories"]
            if item["path"] != "." and item["path"] != current_path
        ]
        if root["phase"] == "verify" and completed_nested:
            changed = cache / completed_nested[0]
            (changed / ".git").mkdir()
            break
    assert changed is not None
    assert measured["subject"].status == "partial"

    observed, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert observed["subject"].status == "error"
    assert observed["subject"].bytes is None
    assert observed["subject"].error is not None
    assert "changed before final publication" in observed["subject"].error


def test_completed_cache_state_rechecks_file_allocation(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    artifact = cache / "artifact"
    artifact.write_bytes(b"x")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    states = (cli.ActiveState("testhost", 16, ()),)
    planned = {"subject": (directory,)}
    state_path = tmp_path / "file-growth-state.json"
    first, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )
    assert first["subject"].status == "complete"

    artifact.write_bytes(b"x" * (2 * 1024 * 1024))
    historical = cli._allocated_cache_bytes(config, directory)
    assert historical != first["subject"].bytes
    second, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert second["subject"].status == "complete"
    assert second["subject"].bytes == historical


def test_cache_census_counts_symlink_without_following_like_historical_scan(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    outside = checkout / "outside"
    outside.write_bytes(b"outside")
    (cache / "link").symlink_to(outside)
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    measured, _work = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 17, ()),),
        {"subject": (directory,)},
        {},
        state_path=tmp_path / "symlink-state.json",
        work_limit=100,
        wall_seconds=5,
    )

    expected = cli._allocated_cache_bytes(config, directory)
    assert measured["subject"].status == "complete"
    assert measured["subject"].bytes == expected


def test_cache_census_state_path_refuses_external_symlink_into_project(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    alias = tmp_path / "external-alias"
    alias.symlink_to(config.root, target_is_directory=True)

    with pytest.raises(cli.Refusal, match="symlink-free"):
        cli._audit_cache_state_path(config, str(alias / "census.json"))

    assert not (config.root / "census.json").exists()
    assert not (config.root / "census.json.key").exists()


def test_default_cache_parent_creation_refuses_intermediate_symlink(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    cache_home = tmp_path / "cache-home"
    cache_home.mkdir()
    (cache_home / "wrkslots").symlink_to(config.root, target_is_directory=True)
    monkeypatch.setenv("XDG_CACHE_HOME", str(cache_home))

    with pytest.raises(cli.Refusal, match="symlink-free"):
        cli._audit_cache_state_path(config, None)

    assert not (config.root / "audit-cache").exists()


def test_default_cache_home_component_swap_refuses_before_project_write(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    outer = tmp_path / "external"
    ancestor = outer / "ancestor"
    cache_home = ancestor / "cache-home"
    cache_home.mkdir(parents=True)
    preserved = outer / "ancestor-preserved"
    original_open = os.open
    swapped = False

    def swapping_open(
        path: str | bytes | os.PathLike[str] | os.PathLike[bytes],
        flags: int,
        mode: int = 0o777,
        *,
        dir_fd: int | None = None,
    ) -> int:
        nonlocal swapped
        if not swapped and path == "ancestor" and dir_fd is not None:
            ancestor.rename(preserved)
            ancestor.symlink_to(config.root, target_is_directory=True)
            swapped = True
        return original_open(path, flags, mode, dir_fd=dir_fd)

    monkeypatch.setenv("XDG_CACHE_HOME", str(cache_home))
    monkeypatch.setattr(os, "open", swapping_open)
    with pytest.raises(cli.Refusal, match="audit cache home is unavailable"):
        cli._audit_cache_state_path(config, None)

    assert swapped is True
    assert not (config.root / "wrkslots").exists()
    assert not (config.root / "audit-cache").exists()


def test_default_cache_home_is_created_componentwise_for_fresh_home(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    fresh_home = tmp_path / "fresh-home"
    fresh_home.mkdir()
    monkeypatch.delenv("XDG_CACHE_HOME", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda _cls: fresh_home))

    state_path = cli._audit_cache_state_path(config, None)

    assert state_path.parent == fresh_home / ".cache" / "wrkslots" / "audit-cache"
    assert state_path.parent.is_dir()
    assert not state_path.exists()


def test_cache_census_without_roots_does_not_require_state_storage(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    monkeypatch.setattr(
        cli,
        "_open_audit_cache_state_parent",
        lambda *_args: (_ for _ in ()).throw(AssertionError("storage was opened")),
    )

    measured, counters = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 26, ()),),
        {"empty": ()},
        {},
        state_path=None,
        work_limit=3,
        wall_seconds=5,
    )

    assert measured["empty"] == cli._AuditCacheMeasurement(0, "complete")
    assert counters["work_consumed"] == 0
    assert counters["work_remaining"] == 3


def test_bound_cache_parent_survives_path_swap_without_project_write(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    (cache / "artifact").write_bytes(b"artifact")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    state_parent = tmp_path / "state-parent"
    state_parent.mkdir()
    preserved_parent = tmp_path / "state-parent-bound"
    state_path = state_parent / "audit.json"
    original_key = cli._audit_cache_state_key
    swapped = False

    def swap_then_key(path: Path, parent_fd: int) -> bytes:
        nonlocal swapped
        state_parent.rename(preserved_parent)
        state_parent.symlink_to(config.root, target_is_directory=True)
        swapped = True
        return original_key(path, parent_fd)

    monkeypatch.setattr(cli, "_audit_cache_state_key", swap_then_key)
    measured, _work = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 21, ()),),
        {"subject": (directory,)},
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert swapped is True
    assert measured["subject"].status == "complete"
    assert not (config.root / "audit.json").exists()
    assert not (config.root / "audit.json.key").exists()
    assert (preserved_parent / "audit.json").is_file()
    assert (preserved_parent / "audit.json.key").is_file()


def test_cache_parent_component_swap_refuses_before_project_write(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    outer = tmp_path / "external"
    ancestor = outer / "ancestor"
    state_parent = ancestor / "state"
    state_parent.mkdir(parents=True)
    preserved = outer / "ancestor-preserved"
    original_open = os.open
    swapped = False

    def swapping_open(
        path: str | bytes | os.PathLike[str] | os.PathLike[bytes],
        flags: int,
        mode: int = 0o777,
        *,
        dir_fd: int | None = None,
    ) -> int:
        nonlocal swapped
        if not swapped and path == "ancestor" and dir_fd is not None:
            ancestor.rename(preserved)
            ancestor.symlink_to(config.root, target_is_directory=True)
            swapped = True
        return original_open(path, flags, mode, dir_fd=dir_fd)

    monkeypatch.setattr(os, "open", swapping_open)
    with pytest.raises(cli.Refusal, match="cannot bind"):
        cli._open_audit_cache_state_parent(config, state_parent / "audit.json")

    assert swapped is True
    assert not (config.root / "audit.json").exists()
    assert not (config.root / "audit.json.key").exists()


def test_cache_census_closes_bound_parent_on_prewrite_exception(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    state_path = tmp_path / "fd-cleanup-state.json"
    before = len(tuple(Path("/proc/self/fd").iterdir()))
    monkeypatch.setattr(
        cli,
        "_audit_cache_root_identity",
        lambda _config, _cache, **_kwargs: (_ for _ in ()).throw(cli.Refusal("fixture refusal")),
    )

    measured, _counters = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 22, ()),),
        {"subject": (directory,)},
        {},
        state_path=state_path,
        work_limit=10,
        wall_seconds=5,
    )

    after = len(tuple(Path("/proc/self/fd").iterdir()))
    assert measured["subject"].status == "error"
    assert measured["subject"].error == "fixture refusal"
    assert after == before


def test_cache_root_binding_refusal_is_local_to_its_subject(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    directories: dict[str, tuple[cli.CacheDirectory, ...]] = {}
    for subject in ("refused", "healthy"):
        checkout = config.root / subject
        cache = checkout / "target"
        cache.mkdir(parents=True)
        (cache / "artifact").write_bytes(subject.encode("ascii"))
        identity = cli._open_directory_identity(checkout, "checkout")
        directories[subject] = (cli.CacheDirectory(cache, checkout, *identity),)
    original = cli._audit_cache_root_identity

    def bind(
        bound_config: cli.Config, cache: cli.CacheDirectory, *, deadline: float | None = None
    ) -> tuple[int, int, int, int, int] | None:
        if cache.checkout_root.name == "refused":
            raise cli.Refusal("fixture binding refusal")
        return original(bound_config, cache, deadline=deadline)

    monkeypatch.setattr(cli, "_audit_cache_root_identity", bind)

    measured, counters = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 27, ()),),
        directories,
        {},
        state_path=tmp_path / "local-binding-refusal.json",
        work_limit=100,
        wall_seconds=5,
    )

    assert measured["refused"].status == "error"
    assert measured["refused"].error == "fixture binding refusal"
    assert measured["healthy"].status == "complete"
    assert type(measured["healthy"].bytes) is int
    assert counters["subjects"] == 2


def test_cache_census_closes_bound_parent_when_key_setup_raises_unexpectedly(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(cache, checkout, *identity)
    state_path = tmp_path / "key-failure-state.json"
    before = len(tuple(Path("/proc/self/fd").iterdir()))
    monkeypatch.setattr(
        cli,
        "_audit_cache_state_key",
        lambda _path, _parent_fd: (_ for _ in ()).throw(
            RuntimeError("fixture key failure")
        ),
    )

    with pytest.raises(RuntimeError, match="fixture key failure"):
        cli._audit_cache_census(
            config,
            (cli.ActiveState("testhost", 23, ()),),
            {"subject": (directory,)},
            {},
            state_path=state_path,
            work_limit=10,
            wall_seconds=5,
        )

    after = len(tuple(Path("/proc/self/fd").iterdir()))
    assert after == before


@pytest.mark.parametrize("failure", ("fstat", "mount"))
def test_cache_relative_opener_closes_new_fd_on_identity_exception(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    failure: str,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(cache, checkout, *checkout_identity)
    root_identity = cli._audit_cache_root_identity(config, directory)
    assert root_identity is not None
    before = set(os.listdir("/proc/self/fd"))
    if failure == "fstat":
        original_fstat = os.fstat
        calls = 0

        def fail_child_fstat(fd: int) -> os.stat_result:
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("fixture fstat failure")
            return original_fstat(fd)

        monkeypatch.setattr(
            os,
            "fstat",
            fail_child_fstat,
        )
    else:
        original_mount_id = cli._fd_mount_id
        calls = 0

        def fail_child_mount(fd: int, label: str) -> int:
            nonlocal calls
            calls += 1
            if calls == 2:
                raise cli.Refusal("fixture mount failure")
            return original_mount_id(fd, label)

        monkeypatch.setattr(
            cli,
            "_fd_mount_id",
            fail_child_mount,
        )

    with pytest.raises((OSError, cli.Refusal), match=f"fixture {failure} failure"):
        cli._open_audit_cache_relative(directory, root_identity, ".")

    assert set(os.listdir("/proc/self/fd")) == before


def test_cache_relative_opener_refuses_swapped_absolute_checkout_ancestor(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    ancestor = tmp_path / "outer" / "ancestor"
    checkout = ancestor / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(cache, checkout, *checkout_identity)
    root_identity = cli._audit_cache_root_identity(config, directory)
    assert root_identity is not None
    preserved = ancestor.with_name("ancestor-preserved")
    ancestor.rename(preserved)
    ancestor.symlink_to(preserved, target_is_directory=True)

    with pytest.raises(cli.Refusal, match="cannot bind cache census checkout"):
        cli._open_audit_cache_relative(directory, root_identity, ".")


def test_cache_census_refuses_nonprivate_existing_key(tmp_path: Path) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    (cache / "artifact").write_bytes(b"artifact")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    states = (cli.ActiveState("testhost", 19, ()),)
    state_path = tmp_path / "key-mode-state.json"
    first, _work = cli._audit_cache_census(
        config,
        states,
        {"subject": (directory,)},
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )
    assert first["subject"].status == "complete"
    key_path = state_path.with_name(f"{state_path.name}.key")
    key_path.chmod(0o644)

    second, _work = cli._audit_cache_census(
        config,
        states,
        {"subject": (directory,)},
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert second["subject"].status == "error"
    assert second["subject"].bytes is None
    assert second["subject"].error is not None
    assert "owner-bound 0600" in second["subject"].error


def test_cache_census_finalization_rechecks_earlier_file_allocation(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    for index in range(4):
        (cache / f"artifact-{index}").write_bytes(b"x")
    checkout_identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(
        path=cache,
        checkout_root=checkout,
        checkout_device=checkout_identity[0],
        checkout_inode=checkout_identity[1],
        checkout_mount_id=checkout_identity[2],
    )
    states = (cli.ActiveState("testhost", 20, ()),)
    planned = {"subject": (directory,)}
    state_path = tmp_path / "cross-chunk-file-growth.json"
    changed: Path | None = None

    for _ in range(30):
        measured, _work = cli._audit_cache_census(
            config,
            states,
            planned,
            {},
            state_path=state_path,
            work_limit=1,
            wall_seconds=5,
        )
        state = json.loads(state_path.read_text(encoding="utf-8"))
        root = next(iter(state["roots"].values()))
        if root["phase"] == "verify" and root["verified_entries"]:
            changed = cache / root["verified_entries"][0]["path"]
            changed.write_bytes(b"x" * (2 * 1024 * 1024))
            break
    assert changed is not None
    assert measured["subject"].status == "partial"

    observed, _work = cli._audit_cache_census(
        config,
        states,
        planned,
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert observed["subject"].status == "error"
    assert observed["subject"].bytes is None
    assert observed["subject"].error is not None
    assert "entry changed before final publication" in observed["subject"].error


def test_batch_provider_deep_json_transport_preserves_every_unverifiable_subject(
    tmp_path: Path,
) -> None:
    config = _config(tmp_path, batch=True)
    assert config.liveness_batch_command is not None
    config.liveness_batch_command.write_text(
        "import sys\n"
        "sys.stdin.buffer.read()\n"
        "sys.stdout.write('{\"results\":' + '[' * 10000 + '0' + ']' * 10000 + '}')\n",
        encoding="utf-8",
    )
    legacy_marker = config.root / "legacy-was-called"
    config.liveness_command.write_text(
        f"from pathlib import Path\nPath({str(legacy_marker)!r}).touch()\n",
        encoding="utf-8",
    )
    records = _records(3)

    observed = cli._registered_liveness_states(config, records)

    assert set(observed) == {
        ("testhost", f"slot-{index:03d}", 1) for index in range(3)
    }
    assert {state for state, _detail in observed.values()} == {"unverifiable"}
    assert all("not JSON" in detail for _state, detail in observed.values())
    assert not legacy_marker.exists()


@pytest.mark.parametrize("stream", ("stdout", "stderr"))
def test_batch_provider_failed_transport_caps_shared_diagnostic(
    tmp_path: Path,
    stream: str,
) -> None:
    config = _config(tmp_path, batch=True)
    assert config.liveness_batch_command is not None
    size = 512 * 1024 if stream == "stdout" else 60 * 1024
    payload = "provider failure\x1b[31m\r\n\x00\u202e\t" + "x" * size
    config.liveness_batch_command.write_text(
        "import sys\n"
        "sys.stdin.buffer.read()\n"
        f"sys.{stream}.write({payload!r})\n"
        "raise SystemExit(7)\n",
        encoding="utf-8",
    )

    observed = cli._registered_liveness_states(config, _records(256))

    assert set(observed) == {
        ("testhost", f"slot-{index:03d}", 1) for index in range(256)
    }
    assert {state for state, _detail in observed.values()} == {"unverifiable"}
    details = [detail for _state, detail in observed.values()]
    assert len(set(details)) == 1
    detail = details[0]
    assert "command exited 7: provider failure" in detail
    assert detail.endswith("[truncated]")
    assert detail == " ".join(detail.split())
    assert all(char.isprintable() for char in detail)
    assert len(detail.encode("utf-8")) <= 4096
    assert sum(len(value.encode("utf-8")) for value in details) <= 256 * 4096


@pytest.mark.parametrize(
    "mutation",
    ("unknown-field", "unknown-subject", "misbound-agent", "invalid-state", "duplicate-field"),
)
def test_batch_provider_malformed_fields_cap_shared_parser_diagnostic(
    tmp_path: Path,
    mutation: str,
) -> None:
    config = _config(tmp_path, batch=True)
    assert config.liveness_batch_command is not None
    config.liveness_batch_command.write_text(
        "import json\n"
        "import sys\n"
        "request = json.load(sys.stdin)\n"
        "results = [dict(subject_id=subject['subject_id'], agent=subject['agent'], "
        "state='dead', detail='fixture dead') for subject in request['subjects']]\n"
        f"response = dict(schema={cli._LIVENESS_BATCH_RESPONSE_SCHEMA!r}, "
        "request_sha256=request['request_sha256'], results=results)\n"
        "payload = 'untrusted\\x1b[31m\\r\\n\\x00\\u202e\\t' + '\\u754c' * 20000\n"
        f"mutation = {mutation!r}\n"
        "if mutation == 'unknown-field':\n"
        "    response[payload] = None\n"
        "elif mutation == 'unknown-subject':\n"
        "    results[0]['subject_id'] = payload\n"
        "elif mutation == 'misbound-agent':\n"
        "    results[0]['agent'] = payload\n"
        "elif mutation == 'invalid-state':\n"
        "    results[0]['state'] = payload\n"
        "encoded = json.dumps(response)\n"
        "if mutation == 'duplicate-field':\n"
        "    member = json.dumps(payload) + ':null'\n"
        "    encoded = encoded[:-1] + ',' + member + ',' + member + '}'\n"
        "sys.stdout.write(encoded)\n",
        encoding="utf-8",
    )

    observed = cli._registered_liveness_states(config, _records(256))

    assert set(observed) == {
        ("testhost", f"slot-{index:03d}", 1) for index in range(256)
    }
    assert {state for state, _detail in observed.values()} == {"unverifiable"}
    details = [detail for _state, detail in observed.values()]
    assert len(set(details)) == 1
    detail = details[0]
    assert detail.startswith("registered batch liveness authority is unverifiable: ")
    assert detail.endswith("[truncated]")
    assert detail == " ".join(detail.split())
    assert all(char.isprintable() for char in detail)
    assert len(detail.encode("utf-8")) <= 4096
    assert sum(len(value.encode("utf-8")) for value in details) <= 256 * 4096


@pytest.mark.parametrize(
    ("candidate", "parent"),
    (
        ("/a/b", "/a/b"),
        ("/a/b/c", "/a/b"),
        ("/a/bc", "/a/b"),
        ("/a/b", "/a/bc"),
        ("/a", "/a/b"),
        ("/a/b", "/"),
        ("/", "/"),
        ("/", "/a"),
        ("/a/../b", "/a"),
        ("//a/b", "/a"),
        ("/a/b", "//a"),
        ("//a/b", "//a"),
        ("a/b", "a"),
        ("a/b", "."),
        ("a/b", "/"),
        ("/a/b", "a"),
        ("ab", "a"),
    ),
)
def test_path_containment_keeps_component_boundaries(candidate: str, parent: str) -> None:
    """The string comparison must answer exactly what `relative_to` answered."""

    try:
        Path(candidate).relative_to(Path(parent))
        expected = True
    except ValueError:
        expected = False

    assert cli._path_is_within(Path(candidate), Path(parent)) is expected


def test_path_containment_rejects_a_sibling_that_shares_a_prefix() -> None:
    assert cli._path_is_within(Path("/a/b"), Path("/a/b"))
    assert cli._path_is_within(Path("/a/b/c"), Path("/a/b"))
    assert not cli._path_is_within(Path("/a/bc"), Path("/a/b"))
    assert cli._path_is_within(Path("/anything"), Path("/"))


def _mount_table(name: str, lines: int, *, last: str | None = None) -> str:
    rows = [
        f"{index} 1 0:{index} / /{name}/m{index} rw - tmpfs {name}{index} rw"
        for index in range(lines)
    ]
    if last is not None:
        rows[-1] = f"{lines} 1 0:{lines} / {last} rw - tmpfs {name}-last rw"
    return "".join(f"{row}\n" for row in rows)


def _fake_process(
    proc_root: Path,
    pid: int,
    mountinfo: str,
    *,
    namespace: str | None = None,
    root: str | None = None,
) -> Path:
    pid_dir = proc_root / str(pid)
    pid_dir.mkdir(parents=True)
    uid = os.getuid()
    (pid_dir / "status").write_text(f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n", encoding="ascii")
    (pid_dir / "mountinfo").write_text(mountinfo, encoding="utf-8")
    if namespace is not None:
        (pid_dir / "ns").mkdir()
        (pid_dir / "ns" / "mnt").symlink_to(namespace)
    if root is not None:
        (pid_dir / "root").symlink_to(root)
    return pid_dir


class _CensusWork:
    """Counts the census's path comparisons, table parses, and table reads."""

    def __init__(self, monkeypatch: pytest.MonkeyPatch) -> None:
        self.comparisons = 0
        self.parses = 0
        self.reads: list[str] = []
        within = cli._path_is_within
        parse = cli._parse_mountinfo_paths
        read = cli._read_process_mountinfo

        def counted_within(candidate: Path, parent: Path) -> bool:
            self.comparisons += 1
            return within(candidate, parent)

        def counted_parse(
            text: str,
            label: str,
            cache: dict[str, tuple[tuple[Path, str], ...]] | None = None,
        ) -> tuple[tuple[Path, str], ...]:
            self.parses += 1
            return parse(text, label, cache)

        def counted_read(pid_dir: Path) -> str | None:
            self.reads.append(pid_dir.name)
            return read(pid_dir)

        monkeypatch.setattr(cli, "_path_is_within", counted_within)
        monkeypatch.setattr(cli, "_parse_mountinfo_paths", counted_parse)
        monkeypatch.setattr(cli, "_read_process_mountinfo", counted_read)


# Well above any real PID, so the fixture never collides with this process.
FIRST_FAKE_PID = 10_000_000


@pytest.mark.parametrize("processes", (60, 180))
def test_process_census_walks_each_distinct_mount_table_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, processes: int
) -> None:
    """N processes listing 3 distinct M-line tables cost 3 walks, not N.

    This is the shape that timed out `recover --abort-create` on a fleet host:
    thousands of processes, nearly one mount namespace each, few distinct
    tables. Without the memo this census makes N x 2M comparisons (7,200 and
    21,600 here); with it the count is fixed by the distinct tables, whatever N.
    """

    lines = 20
    slot = tmp_path / "slot"
    proc_root = tmp_path / "proc"
    tables = [_mount_table(name, lines) for name in ("red", "green", "blue")]
    for index in range(processes):
        _fake_process(proc_root, FIRST_FAKE_PID + index, tables[index % 3])
    work = _CensusWork(monkeypatch)

    cli._assert_slot_unused(slot, use_lsof=False, proc_root=proc_root)

    # One check of the current directory, then two absolute paths per line.
    assert work.comparisons == 1 + 3 * 2 * lines
    assert work.parses == 3
    assert len(work.reads) == processes

    # Equivalence: a later process whose table references the slot is still found.
    user = FIRST_FAKE_PID + processes
    _fake_process(proc_root, user, _mount_table("user", lines, last=f"{slot}/cache"))
    with pytest.raises(cli.Refusal, match=rf"live process {user} uses slot .*: mount={slot}/cache"):
        cli._assert_slot_unused(slot, use_lsof=False, proc_root=proc_root)


def test_mount_verdicts_read_every_table_but_parse_each_distinct_one_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    slot = tmp_path / "slot"
    proc_root = tmp_path / "proc"
    clean = _mount_table("host", 10)
    shared = [
        _fake_process(proc_root, FIRST_FAKE_PID + index, clean, namespace="mnt:[1]", root="/")
        for index in range(5)
    ]
    work = _CensusWork(monkeypatch)
    verdicts = cli._MountTableVerdicts(slot)

    assert all(verdicts.uses(pid_dir) == [] for pid_dir in shared)
    assert work.reads == [pid_dir.name for pid_dir in shared]
    assert work.parses == 1
    assert work.comparisons == 2 * 10


@pytest.mark.parametrize("root", ("/jail", "/"))
def test_mount_verdicts_never_let_one_process_answer_for_its_namespace(
    tmp_path: Path, root: str
) -> None:
    """Sharing a namespace, or even a rendered root, is not sharing a table.

    A chroot sees only the mounts below its root, and a root the reader cannot
    reach -- a chroot in a lazily unmounted tree -- renders as ``/``.  Keyed on
    either, the first (short, clean) table would stand in for a later process
    that really does mount the slot.
    """

    slot = tmp_path / "slot"
    proc_root = tmp_path / "proc"
    jailed = _fake_process(proc_root, FIRST_FAKE_PID, "", namespace="mnt:[1]", root=root)
    host = _fake_process(
        proc_root,
        FIRST_FAKE_PID + 1,
        _mount_table("host", 4, last=str(slot)),
        namespace="mnt:[1]",
        root="/",
    )
    verdicts = cli._MountTableVerdicts(slot)

    assert verdicts.uses(jailed) == []
    assert verdicts.uses(host) == [f"mount={slot}"]


def test_mount_verdicts_match_processes_without_a_namespace_on_content(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A process whose namespace is unreadable is still read, and still shares."""

    slot = tmp_path / "slot"
    proc_root = tmp_path / "proc"
    clean = _mount_table("host", 10)
    using = _mount_table("host", 10, last=f"{slot}/x")
    protected = [
        _fake_process(proc_root, FIRST_FAKE_PID + index, clean) for index in range(4)
    ]
    user = _fake_process(proc_root, FIRST_FAKE_PID + 4, using)
    work = _CensusWork(monkeypatch)
    verdicts = cli._MountTableVerdicts(slot)

    for pid_dir in protected:
        assert verdicts.uses(pid_dir) == []
    assert verdicts.uses(user) == [f"mount={slot}/x"]
    assert len(work.reads) == 5
    assert work.parses == 2


def test_mount_verdicts_refuse_a_malformed_table_for_each_process(
    tmp_path: Path,
) -> None:
    slot = tmp_path / "slot"
    proc_root = tmp_path / "proc"
    malformed = "this line has no field separator\n"
    first = _fake_process(proc_root, FIRST_FAKE_PID, malformed)
    second = _fake_process(proc_root, FIRST_FAKE_PID + 1, malformed)
    verdicts = cli._MountTableVerdicts(slot)

    with pytest.raises(cli.Refusal, match=f"PID {first.name}"):
        verdicts.uses(first)
    with pytest.raises(cli.Refusal, match=f"PID {second.name}"):
        verdicts.uses(second)


@pytest.mark.skipif(
    not any(
        Path(candidate).is_file() and os.access(candidate, os.X_OK)
        for candidate in ("/usr/bin/lsof", "/usr/sbin/lsof")
    ),
    reason="the lsof branch needs lsof",
)
def test_lsof_census_walks_each_distinct_mount_table_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The branch taken after lsof proves direct use absent shares the memo."""

    lines = 20
    slot = tmp_path / "slot"
    proc_root = tmp_path / "proc"
    tables = [_mount_table(name, lines) for name in ("red", "green")]
    for index in range(40):
        _fake_process(
            proc_root,
            FIRST_FAKE_PID + index,
            tables[index % 2],
            namespace=f"mnt:[{index}]",
            root="/",
        )
    monkeypatch.setattr(cli, "_lsof_slot_use", lambda _lsof, _slot: (None, False, 1))
    work = _CensusWork(monkeypatch)

    cli._assert_slot_unused(slot, proc_root=proc_root)

    assert work.comparisons == 1 + 2 * 2 * lines
    assert work.parses == 2

    # The user shares a namespace with a clean process already inspected, so
    # skipping repeat namespaces in the loop itself would miss it.
    user = FIRST_FAKE_PID + 40
    _fake_process(
        proc_root, user, _mount_table("user", lines, last=str(slot)),
        namespace="mnt:[0]", root="/",
    )
    with pytest.raises(cli.Refusal, match=rf"live process {user} uses slot"):
        cli._assert_slot_unused(slot, proc_root=proc_root)
