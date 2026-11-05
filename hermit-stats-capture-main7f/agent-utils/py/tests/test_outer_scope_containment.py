import errno
import os
from pathlib import Path

import pytest

from dagrun import cgroup, cli


def test_delegated_root_requires_exact_supervisor_membership_and_kill_boundary(
    tmp_path: Path,
) -> None:
    root = tmp_path / "step-owner"
    supervisor = root / cgroup.DEFAULT_NAMING.supervisor_name
    supervisor.mkdir(parents=True)
    (root / "cgroup.kill").write_text("")
    (supervisor / "cgroup.procs").write_text("123\n")

    assert (
        cgroup._validate_delegated_cgroup_root_at(
            root, supervisor, 123, cgroup_root=tmp_path
        )
        is None
    )
    sibling = tmp_path / "sibling"
    sibling.mkdir()
    (sibling / "cgroup.procs").write_text("123\n")
    assert "not the exact" in str(
        cgroup._validate_delegated_cgroup_root_at(
            root, sibling, 123, cgroup_root=tmp_path
        )
    )
    assert "outside" in str(
        cgroup._validate_delegated_cgroup_root_at(
            tmp_path, supervisor, 123, cgroup_root=tmp_path
        )
    )
    (supervisor / "cgroup.procs").write_text("999\n")
    assert "is not listed" in str(
        cgroup._validate_delegated_cgroup_root_at(
            root, supervisor, 123, cgroup_root=tmp_path
        )
    )


def test_delegated_command_uses_supervisor_leaf_and_cleanup_descends(tmp_path: Path) -> None:
    manager = object.__new__(cgroup.Cgroups)
    manager._naming = cgroup.DEFAULT_NAMING
    manager.enabled = True
    manager.root = tmp_path
    manager._made = set()
    manager._delegated = set()
    manager._delegated_ancestor_memory_max = None
    manager._delegated_ancestor_memory_max_known = True
    manager._delegated_ancestor_max_cpus = None
    manager._delegated_ancestor_cpu_known = True
    manager.worker_pids_max = None

    # A regular preparation creates the fixture's pseudo control files; an empty controller list
    # is enough here because this unit pins the topology and command wrapper, while the real-cgroup
    # smoke test pins controller delegation and kill behavior end to end.
    manager.prepare_command("g.j", "true")
    step_root = tmp_path / "step-g.j"
    (step_root / "cgroup.controllers").write_text("")
    command, exported = manager.prepare_delegated_command("g.j", "true")
    supervisor = step_root / cgroup.DEFAULT_NAMING.supervisor_name
    assert exported == str(step_root)
    assert str(supervisor / "cgroup.procs") in command
    assert str(step_root / "cgroup.procs") not in command

    (supervisor / "step-inner" / "supervisor").mkdir(parents=True)
    for control in ("cgroup.controllers", "memory.high", "memory.oom.group", "memory.swap.max"):
        (step_root / control).unlink()
    manager.cleanup("g.j")
    assert not step_root.exists()


def test_failed_nested_coordinator_verification_rolls_back_to_parent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    parent = tmp_path / "step-owner"
    return_supervisor = parent / cgroup.DEFAULT_NAMING.supervisor_name
    run_root = parent / "nested-run-fixture"
    nested_supervisor = run_root / cgroup.DEFAULT_NAMING.supervisor_name
    nested_supervisor.mkdir(parents=True)
    return_supervisor.mkdir(exist_ok=True)
    wrong = tmp_path / "wrong"
    wrong.mkdir()
    observed = iter((return_supervisor, wrong, return_supervisor))
    monkeypatch.setattr(cgroup, "_my_cgroup_path", lambda: next(observed))
    stack_depth = len(cgroup._NESTED_COORDINATOR_STACK)

    with pytest.raises(OSError, match="did not enter"):
        cgroup._move_current_to_nested_supervisor(run_root)

    assert (return_supervisor / "cgroup.procs").read_text() == str(os.getpid())
    assert len(cgroup._NESTED_COORDINATOR_STACK) == stack_depth


def test_failed_nested_coordinator_rollback_retains_a_recovery_entry(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    parent = tmp_path / "step-owner"
    return_supervisor = parent / cgroup.DEFAULT_NAMING.supervisor_name
    run_root = parent / "nested-run-fixture"
    nested_supervisor = run_root / cgroup.DEFAULT_NAMING.supervisor_name
    nested_supervisor.mkdir(parents=True)
    return_supervisor.mkdir(exist_ok=True)
    # Make the rollback write fail reliably on a normal filesystem.
    (return_supervisor / "cgroup.procs").mkdir()
    wrong = tmp_path / "wrong"
    wrong.mkdir()
    observed = iter((return_supervisor, wrong))
    monkeypatch.setattr(cgroup, "_my_cgroup_path", lambda: next(observed))
    stack_depth = cgroup.nested_coordinator_depth()

    try:
        with pytest.raises(OSError, match="did not enter"):
            cgroup._move_current_to_nested_supervisor(run_root)
        assert cgroup.nested_coordinator_depth() == stack_depth + 1
        assert cgroup._NESTED_COORDINATOR_STACK[-1][1] == return_supervisor
    finally:
        while cgroup.nested_coordinator_depth() > stack_depth:
            cgroup._NESTED_COORDINATOR_STACK.pop()


def test_allow_failure_still_refuses_when_failed_nested_setup_cannot_restore(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    class FailedManager:
        enabled = False

        def close(self) -> bool:
            return False

    monkeypatch.setattr(
        cgroup,
        "delegated_cgroup_root",
        lambda _naming: (tmp_path / "step-owner", None),
    )
    monkeypatch.setattr(
        cgroup.Cgroups,
        "from_delegated_root",
        classmethod(lambda _cls, *_args, **_kwargs: FailedManager()),
    )

    manager, code = cli._resolve_cgroup_manager(allow_failure=True)

    assert manager is None
    assert code == 3
    assert "no step will be started" in capsys.readouterr().err


def test_unrelated_main_call_does_not_restore_a_preexisting_nested_manager(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    sentinel = (tmp_path / "run", tmp_path / "return", cgroup.DEFAULT_NAMING)
    cgroup._NESTED_COORDINATOR_STACK.append(sentinel)
    try:
        with pytest.raises(SystemExit) as raised:
            cli.main(["--help"])
        assert raised.value.code == 0
        assert cgroup._NESTED_COORDINATOR_STACK[-1] == sentinel
    finally:
        assert cgroup._NESTED_COORDINATOR_STACK.pop() == sentinel
    capsys.readouterr()


def test_successful_main_fails_closed_when_coordinator_restoration_fails(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.setattr(cli, "_main", lambda _argv: 0)
    monkeypatch.setattr(cgroup, "nested_coordinator_depth", lambda: 0)
    monkeypatch.setattr(cgroup, "restore_nested_coordinators_to", lambda _depth: False)

    assert cli.main([]) == 3
    assert "refusing to report a successful invocation" in capsys.readouterr().err


def test_nested_coordinator_restoration_is_lifo_to_the_exact_previous_cgroup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An inner in-process invocation must not escape its outer aggregate cap."""
    parent_supervisor = tmp_path / "step-owner" / cgroup.DEFAULT_NAMING.supervisor_name
    outer_run = parent_supervisor.parent / "nested-run-outer"
    outer_supervisor = outer_run / cgroup.DEFAULT_NAMING.supervisor_name
    inner_run = outer_run / "step-delegating" / "nested-run-inner"
    inner_supervisor = inner_run / cgroup.DEFAULT_NAMING.supervisor_name
    for supervisor in (parent_supervisor, outer_supervisor, inner_supervisor):
        supervisor.mkdir(parents=True)

    # One current-cgroup observation before and after each migration, followed by one after each
    # restoration. The important third value is outer_supervisor: that exact live placement, not
    # the original delegated root's sibling supervisor, is the inner invocation's return target.
    observations = iter(
        (
            parent_supervisor,
            outer_supervisor,
            outer_supervisor,
            inner_supervisor,
            outer_supervisor,
            parent_supervisor,
        )
    )
    monkeypatch.setattr(cgroup, "_my_cgroup_path", lambda: next(observations))
    initial_depth = cgroup.nested_coordinator_depth()

    try:
        cgroup._move_current_to_nested_supervisor(outer_run)
        cgroup._move_current_to_nested_supervisor(inner_run)
        assert cgroup._NESTED_COORDINATOR_STACK[-1][1] == outer_supervisor

        cgroup.restore_nested_coordinator()
        assert (outer_supervisor / "cgroup.procs").read_text() == str(os.getpid())
        assert cgroup.nested_coordinator_depth() == initial_depth + 1

        cgroup.restore_nested_coordinator()
        assert (parent_supervisor / "cgroup.procs").read_text() == str(os.getpid())
        assert cgroup.nested_coordinator_depth() == initial_depth
    finally:
        while cgroup.nested_coordinator_depth() > initial_depth:
            cgroup._NESTED_COORDINATOR_STACK.pop()


def test_scope_drain_waits_past_first_empty_sample_for_late_member() -> None:
    observations = iter([["runner"], [], ["late-systemd-helper"], [], []])
    moved: list[str] = []
    sleeps: list[float] = []

    cgroup._drain_scope_root_with(
        lambda: next(observations, []),
        moved.append,
        sleeps.append,
    )

    assert moved == ["runner", "late-systemd-helper"]
    assert sleeps == [cgroup._SCOPE_DRAIN_RETRY_SECONDS] * 4


def test_scope_drain_refuses_a_root_that_never_quiesces() -> None:
    moves = 0

    def refuse_move(_pid: str) -> None:
        nonlocal moves
        moves += 1
        raise PermissionError("planted refusal")

    with pytest.raises(BlockingIOError, match="persistent-member"):
        cgroup._drain_scope_root_with(
            lambda: ["persistent-member"],
            refuse_move,
            lambda _seconds: None,
        )

    assert moves == cgroup._SCOPE_DRAIN_ATTEMPTS


def test_scope_drain_propagates_an_unreadable_roster() -> None:
    moved = False

    def unreadable() -> list[str]:
        raise PermissionError("planted unreadable cgroup.procs")

    def move(_pid: str) -> None:
        nonlocal moved
        moved = True

    with pytest.raises(PermissionError, match="planted unreadable"):
        cgroup._drain_scope_root_with(unreadable, move, lambda _seconds: None)

    assert not moved


def test_controller_enable_redrains_a_member_in_the_check_act_gap() -> None:
    observations = iter([[], [], ["late-between-check-and-write"], [], []])
    moved: list[str] = []
    writes = 0

    def write_controller() -> None:
        nonlocal writes
        writes += 1
        if writes == 1:
            raise OSError(errno.EBUSY, "planted late member")

    cgroup._enable_controller_with(
        lambda: next(observations, []),
        moved.append,
        write_controller,
        lambda _seconds: None,
    )

    assert writes == 2
    assert moved == ["late-between-check-and-write"]


def test_outer_memory_cap_is_derived_and_override_only_tightens(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(cgroup, "mem_available_bytes", lambda: 1_000_000)
    monkeypatch.delenv(cgroup.OUTER_MEMORY_MAX_ENV, raising=False)
    assert cgroup.outer_memory_max_bytes() == 900_000

    monkeypatch.setenv(cgroup.OUTER_MEMORY_MAX_ENV, "2000000")
    assert cgroup.outer_memory_max_bytes() == 900_000

    monkeypatch.setenv(cgroup.OUTER_MEMORY_MAX_ENV, "500000")
    assert cgroup.outer_memory_max_bytes() == 500_000


def test_max_mem_becomes_the_outer_scope_ceiling_and_can_only_tighten(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``--max-mem`` is a CONTAINMENT limit, not just a sizing input.

    Before this, ``--max-mem 20G`` sized the schedule and the outer scope still admitted 90% of
    the host, so "two validates with 20 GiB each" held only for as long as the arithmetic did.
    It obeys the same one-way rule as the environment override: tighten, never widen.
    """
    monkeypatch.setattr(cgroup, "mem_available_bytes", lambda: 1_000_000)
    monkeypatch.delenv(cgroup.OUTER_MEMORY_MAX_ENV, raising=False)

    # Tightens: the request is below the derived 90% boundary and becomes the cap.
    assert cgroup.outer_memory_max_bytes(400_000) == 400_000
    # Cannot widen: a request above the derived boundary leaves the boundary in place.
    assert cgroup.outer_memory_max_bytes(5_000_000) == 900_000
    # Absent request is byte-for-byte the previous behaviour.
    assert cgroup.outer_memory_max_bytes(None) == 900_000

    # The SMALLEST of the three wins, whichever it is -- the env override and --max-mem do not
    # override each other, they compose.
    monkeypatch.setenv(cgroup.OUTER_MEMORY_MAX_ENV, "500000")
    assert cgroup.outer_memory_max_bytes(400_000) == 400_000
    assert cgroup.outer_memory_max_bytes(700_000) == 500_000


def test_a_nonpositive_ceiling_request_is_refused_rather_than_widened(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Same treatment as a non-positive environment value: the caller asked for a ceiling this
    # cannot express, and returning the derived boundary instead would hand back a WIDER scope
    # than was asked for.
    #
    # This is the LIBRARY contract for a caller of outer_memory_max_bytes, not a statement about
    # `--max-mem 0`: the CLI drops a non-positive spec before this point and refuses the run by
    # name in _select_max_steps instead.  The end-to-end behaviour of `--max-mem 0` is pinned in
    # test_max_mem_enforcement.py, because a rule stated only in a comment is how the previous
    # version of this one came to be wrong.
    monkeypatch.setattr(cgroup, "mem_available_bytes", lambda: 1_000_000)
    monkeypatch.delenv(cgroup.OUTER_MEMORY_MAX_ENV, raising=False)
    assert cgroup.outer_memory_max_bytes(0) is None
    assert cgroup.outer_memory_max_bytes(-1) is None


def test_outer_oom_group_write_is_read_back(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    control = tmp_path / "memory.oom.group"
    control.write_text("0")
    monkeypatch.setattr(cgroup, "scope_cgroup_from_self", lambda naming: tmp_path)

    assert cgroup.enable_outer_oom_group()
    assert control.read_text() == "1"


def test_outer_oom_group_readback_mismatch_is_refused(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    control = tmp_path / "memory.oom.group"
    control.write_text("0")
    monkeypatch.setattr(cgroup, "scope_cgroup_from_self", lambda naming: tmp_path)
    real_read = cgroup._read_cgroup_value

    def stale_read(group: Path, name: str) -> str | None:
        if name == "memory.oom.group":
            return "0"
        return real_read(group, name)

    monkeypatch.setattr(cgroup, "_read_cgroup_value", stale_read)
    assert not cgroup.enable_outer_oom_group()


def test_scope_limit_audit_requires_all_three_memory_controls(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    (tmp_path / "memory.max").write_text("104857600")
    (tmp_path / "memory.swap.max").write_text("0")
    (tmp_path / "memory.oom.group").write_text("1")
    (tmp_path / "cpu.max").write_text("max 100000")
    monkeypatch.setattr(cgroup, "scope_cgroup_from_self", lambda naming: tmp_path)

    assert cgroup.verify_scope_limits(104857600, None)
    (tmp_path / "memory.oom.group").write_text("0")
    assert not cgroup.verify_scope_limits(104857600, None)


def test_cpuset_verification_requires_the_exact_requested_ids(tmp_path: Path) -> None:
    (tmp_path / "cgroup.controllers").write_text("cpu memory cpuset")
    (tmp_path / "cpuset.cpus").write_text("")
    (tmp_path / "cpuset.cpus.effective").write_text("2-3")
    (tmp_path / "cgroup.subtree_control").write_text("")

    assert not cgroup._try_cgroup_cpuset(tmp_path, [0, 1])
    assert cgroup._try_cgroup_cpuset(tmp_path, [2, 3])


def test_specific_cores_never_mutates_an_unowned_ambient_scope(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    naming = cgroup.DEFAULT_NAMING
    monkeypatch.delenv(naming.env_in_scope, raising=False)
    monkeypatch.delenv(naming.env_direct_cgroup, raising=False)
    monkeypatch.setattr(cgroup, "scope_cgroup_from_self", lambda _naming: tmp_path)

    assert cgroup.apply_specific_cores([0], naming) is None
    assert not (tmp_path / "cpuset.cpus").exists()
