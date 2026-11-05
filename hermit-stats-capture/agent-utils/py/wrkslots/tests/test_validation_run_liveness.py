"""A validation row's liveness is answered by its run, not by an agent name.

Validation launchers register a run label such as ``validate-<slot>`` as the
row's agent.  No process carries that label, so the configured agent-liveness
command cannot answer for it: on 2026-10-05 it reported every one of 54
validation rows on one host ``unverifiable``, and ``remove --validate-complete``
refused every completed checkout whose launcher had already exited.  These
tests pin the replacement authority -- retained run handles, their exact
process generations and units, and user-systemd state -- in both directions:
removal succeeds when the run is provably over even though the agent-name
command is unverifiable, and it refuses whenever the run may still be using the
checkout even though the agent-name command says dead.
"""

from __future__ import annotations

import contextlib
import dataclasses
import json
import os
import subprocess
import sys
import time
import uuid
from collections.abc import Iterator, Mapping, Sequence, Set as AbstractSet
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

import pytest

from wrkslots import cli as wrkslots
from wrkslots.tests.test_lifecycle import (
    active_slots,
    allow_test_host_for_absent_validate_recovery,
    checkout,
    create,
    expire_heartbeat,
    interrupt_validate_batch,
    make_project,
    mark_owner_dead,
    prepare_absent_agent_row,
    prepare_absent_validate_row,
    prepare_dead_validate_slots,
    run_absent_agent_recovery,
    run_absent_validate_recovery,
    set_liveness,
    stub_validate_batch_censuses,
    write_absent_validate_input,
)


# These tests supply the host's process and user-systemd evidence themselves,
# below the seam that the suite's idle-host default replaces.
pytestmark = pytest.mark.validation_run_evidence

RUN_UNIT = "validate-run-0001.service"

# Linux's PATH_MAX: the longest path, with its terminating NUL, that one
# system call accepts.  Unit words are read past it; these tests use it as
# the length a word must exceed to show that.
PATH_MAX = 4096


def _unit(**overrides: str) -> dict[str, str]:
    value = {
        "Id": RUN_UNIT,
        "LoadState": "loaded",
        "ActiveState": "inactive",
        "SubState": "dead",
        "MainPID": "0",
        "ControlGroup": f"/user.slice/app.slice/{RUN_UNIT}",
        "WorkingDirectory": "",
        "ExecStart": "",
        "Environment": "",
        "PendingJob": "no",
    }
    value.update(overrides)
    return value


def _prepare(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    agent_liveness: str,
    units: Sequence[Mapping[str, str]] = (),
    processes: Sequence[wrkslots._AbsentProcessObservation] = (),
) -> tuple[Path, Path]:
    """Create one completed validation row whose launcher has exited."""

    project, _repository, _remote = make_project(tmp_path)
    made = create(
        project,
        agent="validate-slot01",
        slot_type="validate",
        branch=None,
    )
    assert made.returncode == 0, made.stderr
    mark_owner_dead(project)
    set_liveness(project, agent_liveness)
    stub_validate_batch_censuses(monkeypatch)
    monkeypatch.setattr(wrkslots, "_assert_slot_unused", lambda *_a, **_k: None)
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: tuple(units))
    monkeypatch.setattr(
        wrkslots,
        "_absent_validate_process_snapshot",
        lambda **_kwargs: tuple(processes),
    )
    # The supplied host's retained units have no control-group members; a
    # test that examines members supplies them itself.
    monkeypatch.setattr(wrkslots, "_retained_unit_cgroup_members", _no_members)
    return project, checkout(project, slot_type="validate")


class _MemberReader(Protocol):
    def __call__(
        self, units: AbstractSet[str], *, root: Path | None = None
    ) -> Mapping[str, int]: ...


def _no_members(
    units: AbstractSet[str], *, root: Path | None = None
) -> Mapping[str, int]:
    return dict.fromkeys(units, 0)


def _write_run_handle(
    project: Path,
    tree: Path,
    *,
    process_identity: Mapping[str, object] | None = None,
    unit: str = RUN_UNIT,
) -> Path:
    handle = project / "ignored" / "validate" / "runs" / (
        unit.removesuffix(".service") + ".json"
    )
    handle.parent.mkdir(parents=True, exist_ok=True)
    value: dict[str, object] = {"checkout": str(tree), "unit": unit}
    if process_identity is not None:
        value["process_identity"] = dict(process_identity)
    handle.write_text(json.dumps(value), encoding="utf-8")
    return handle


def _remove_completed(project: Path) -> int:
    return wrkslots.main(
        [
            "--project-root",
            str(project),
            "remove",
            "slot01",
            "--validate-complete",
            "--coordinator-authorized",
            "--coordinator-pid",
            str(os.getpid()),
            "--expected-generation",
            "1",
        ]
    )


def _assert_retained(project: Path, tree: Path) -> None:
    assert tree.is_dir()
    assert len(active_slots(project)) == 1


def test_completed_validation_removal_does_not_ask_agent_liveness_about_a_run_label(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The incident: the agent-name command cannot answer for a run label."""

    project, tree = _prepare(
        tmp_path,
        monkeypatch,
        agent_liveness="unverifiable",
        units=(_unit(),),
    )
    _write_run_handle(project, tree)

    removed = _remove_completed(project)

    assert removed == 0, capsys.readouterr().err
    assert not tree.exists()
    assert active_slots(project) == []


def test_completed_validation_removal_refuses_while_the_run_unit_is_active(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, tree = _prepare(
        tmp_path,
        monkeypatch,
        agent_liveness="dead",
        units=(_unit(ActiveState="active", SubState="running"),),
    )
    _write_run_handle(project, tree)

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority reports the run may still use slot slot01" in error
    assert f"retained validation unit {RUN_UNIT} may still use row slot01" in error
    _assert_retained(project, tree)


@pytest.mark.parametrize("spelling", ["symlink", "descendant", "parent-step"])
def test_a_handle_naming_the_slot_by_another_path_binds_to_the_row(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    spelling: str,
) -> None:
    """A handle whose checkout reaches the slot under another path is its run.

    Its checkout is compared with the row's paths as a unit word is: a
    symlink to the slot, a directory inside it, and a ``..`` after a
    symlink into it all name the row, so the handle's active unit keeps
    the slot.
    """

    project, tree = _prepare(
        tmp_path,
        monkeypatch,
        agent_liveness="dead",
        units=(_unit(ActiveState="active", SubState="running"),),
    )
    (tree / "product").mkdir(exist_ok=True)
    if spelling == "symlink":
        checkout = tmp_path / "alias"
        checkout.symlink_to(tree)
    elif spelling == "descendant":
        checkout = tree / "product"
    else:
        link = tmp_path / "inside"
        link.symlink_to(tree / "product")
        checkout = link / ".."
    _write_run_handle(project, checkout)

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority reports the run may still use slot slot01" in error
    assert f"retained validation unit {RUN_UNIT} may still use row slot01" in error
    _assert_retained(project, tree)


def test_completed_validation_removal_refuses_while_the_run_job_is_queued(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, tree = _prepare(
        tmp_path,
        monkeypatch,
        agent_liveness="dead",
        units=(_unit(PendingJob="yes"),),
    )
    _write_run_handle(project, tree)

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert f"retained validation unit {RUN_UNIT} may still use row slot01" in error
    _assert_retained(project, tree)


def test_completed_validation_removal_refuses_a_live_run_process_generation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, tree = _prepare(
        tmp_path, monkeypatch, agent_liveness="dead", units=(_unit(),)
    )
    start_ticks = wrkslots._process_start_ticks(Path("/proc") / str(os.getpid()))
    assert start_ticks is not None
    _write_run_handle(
        project,
        tree,
        process_identity={
            "pid": os.getpid(),
            "start_ticks": start_ticks,
            "boot_id": wrkslots._boot_id(Path("/proc")),
        },
    )

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert f"has live exact process generation {os.getpid()} for row slot01" in error
    _assert_retained(project, tree)


def test_completed_validation_removal_refuses_a_process_left_in_the_run_cgroup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    straggler = wrkslots._AbsentProcessObservation(
        pid=os.getpid() + 100_000,
        start_ticks=17,
        cgroup_path=f"/user.slice/app.slice/{RUN_UNIT}/payload",
        mount_namespace="mnt:[test]",
    )
    project, tree = _prepare(
        tmp_path,
        monkeypatch,
        agent_liveness="dead",
        units=(_unit(),),
        processes=(straggler,),
    )
    _write_run_handle(project, tree)

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert f"retained validation unit {RUN_UNIT} still has a live cgroup process" in error
    _assert_retained(project, tree)


def test_completed_validation_removal_refuses_an_active_unit_naming_the_checkout(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A run with no retained handle is still visible through its unit."""

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    unrecorded = _unit(
        Id="unrecorded-run.service",
        ActiveState="active",
        SubState="running",
        WorkingDirectory=str(tree),
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (unrecorded,))

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "user-systemd unit unrecorded-run.service names validation row slot01" in error
    _assert_retained(project, tree)


def test_completed_validation_removal_refuses_unreadable_run_evidence(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")

    def unreadable() -> tuple[Mapping[str, str], ...]:
        raise wrkslots.Refusal("cannot enumerate user-systemd state: no user bus")

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", unreadable)

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority is unverifiable for slot slot01" in error
    assert "no user bus" in error
    _assert_retained(project, tree)


def test_audit_judges_a_validation_row_by_its_run(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Audit and remove ask the same authority, so they cannot disagree."""

    project, tree = _prepare(
        tmp_path, monkeypatch, agent_liveness="unverifiable", units=(_unit(),)
    )
    _write_run_handle(project, tree)
    expire_heartbeat(project)

    def audit() -> str:
        code = wrkslots.main(
            ["--project-root", str(project), "audit", "--format", "json"]
        )
        captured = capsys.readouterr()
        assert code == 0, captured.err
        return captured.out

    finished = json.loads(audit())
    phases = {phase["name"]: phase for phase in finished["metrics"]["phases"]}
    assert phases["liveness"]["work"] == {
        "batch_invocations": 0,
        "legacy_invocations": 0,
        "subjects": 0,
        "validation_run_subjects": 1,
    }
    row = finished["slots"][0]
    assert row["liveness_state"] == "dead"
    assert row["verdict"] == "DELETABLE", row["reasons"]

    monkeypatch.setattr(
        wrkslots,
        "_user_systemd_snapshot",
        lambda: (_unit(ActiveState="active", SubState="running"),),
    )
    running = json.loads(audit())
    row = running["slots"][0]
    assert row["liveness_state"] == "alive"
    assert row["verdict"] == "BLOCKED"
    assert any(
        f"retained validation unit {RUN_UNIT} may still use row slot01" in reason
        for reason in row["reasons"]
    )


def _live_identity() -> dict[str, object]:
    start_ticks = wrkslots._process_start_ticks(Path("/proc") / str(os.getpid()))
    assert start_ticks is not None
    return {
        "pid": os.getpid(),
        "start_ticks": start_ticks,
        "boot_id": wrkslots._boot_id(Path("/proc")),
    }


@pytest.mark.parametrize(
    ("duplicated", "trailer"),
    [
        # A later null identity would hide the live process generation.
        ("process_identity", '"process_identity": null'),
        # A later checkout would move the handle off this row entirely.
        ("checkout", '"checkout": "/elsewhere/unrelated-checkout"'),
    ],
)
def test_completed_validation_removal_refuses_a_handle_with_a_duplicate_field(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    duplicated: str,
    trailer: str,
) -> None:
    """A JSON object with two values for one field has no single meaning.

    A permissive decoder keeps the last value.  Here the first values say the
    run's process is live on this row's checkout, and the appended duplicate
    would make the row look dead.  The handle is unreadable evidence instead.
    """

    project, tree = _prepare(
        tmp_path, monkeypatch, agent_liveness="dead", units=(_unit(),)
    )
    handle = _write_run_handle(project, tree, process_identity=_live_identity())
    text = handle.read_text(encoding="utf-8")
    assert text.endswith("}")
    handle.write_text(f"{text[:-1]}, {trailer}}}", encoding="utf-8")

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority is unverifiable for slot slot01" in error
    assert f"duplicate key {duplicated!r}" in error
    _assert_retained(project, tree)


def _slot_directory(project: Path) -> Path:
    config = wrkslots._load_config(str(project), "testhost")
    return wrkslots._slot_directory(config, "slot01", "validate")


@pytest.mark.parametrize(
    ("property_name", "spelling"),
    [
        ("ExecStart", "{parent}/./{name}"),
        ("ExecStart", "--checkout={parent}//{name}/product"),
        ("WorkingDirectory", "{parent}/elsewhere/../{name}"),
        ("Environment", "RUN_ROOT={parent}/./{name}/./product"),
        ("RequiresMountsFor", "{parent}/{name}/"),
    ],
)
def test_completed_validation_removal_refuses_a_queued_unit_naming_an_alias_of_the_row(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    property_name: str,
    spelling: str,
) -> None:
    """A queued job names the row through a different spelling of its path.

    The job has not started, so no process or handle shows it yet.  Its unit
    evidence names the row's directory with ``/./``, ``//`` or ``x/..``
    steps; each spelling is the same path, so the row may still be used.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    slot_directory = _slot_directory(project)
    alias = spelling.format(parent=slot_directory.parent, name=slot_directory.name)
    queued = _unit(Id="queued-run.service", PendingJob="yes", **{property_name: alias})
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (queued,))

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority reports the run may still use slot slot01" in error
    assert "user-systemd unit queued-run.service names validation row slot01" in error
    _assert_retained(project, tree)


@pytest.mark.parametrize("sibling", ["slot010", "slot01-old", "slot01.bak", "slot01_next"])
def test_completed_validation_removal_ignores_an_active_unit_naming_a_sibling_row(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    sibling: str,
) -> None:
    """``slot010`` begins with the text ``slot01`` but is a different path."""

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    other = _slot_directory(project).parent / sibling
    running = _unit(
        Id="sibling-run.service",
        ActiveState="active",
        SubState="running",
        WorkingDirectory=str(other),
        ExecStart=f"/usr/bin/env\n--checkout={other}/product",
        Environment=f"RUN_ROOT={other}",
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (running,))

    removed = _remove_completed(project)

    assert removed == 0, capsys.readouterr().err
    assert not tree.exists()
    assert active_slots(project) == []


def test_unit_words_name_a_row_by_path_identity(tmp_path: Path) -> None:
    """A unit names a row when one of its words is the row's path or inside it.

    Words are compared as whole paths: lexically normalized, symlink
    resolved, relative ones resolved against the unit's working directory,
    and by file identity where the path exists.  A longer path that merely
    contains the row's text is a different path.
    """

    project = tmp_path / "project"
    row = project / "worktrees" / "validate" / "slot01"
    row.mkdir(parents=True)
    (project / "worktrees" / "validate" / "slot010").mkdir()
    alias = tmp_path / "alias"
    alias.symlink_to(row)
    outer = tmp_path / "outer"
    outer.symlink_to(project)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    # One shell command holds several paths; a later ``x/..`` step does not
    # hide the row path that an earlier word names.
    assert names(ExecStart=f"cd {project}/worktrees/tmp/../validate/slot01 && ls build/../out")
    assert names(Environment=f"PATH=/usr/bin:{row}:/bin")
    assert names(ExecStart=f"--checkout={row}//product")
    # A symlink to the row, or to one of its ancestors, is the row.
    assert names(ExecStart=f"{alias}/product")
    assert names(ExecStart=f"{outer}/worktrees/validate/slot01")
    # A relative word is resolved against the unit's working directory.
    assert names(WorkingDirectory=str(project), ExecStart="git\n-C\nworktrees/validate/slot01")
    assert names(WorkingDirectory=f"!{row}/build", ExecStart="make\n..")
    assert names(WorkingDirectory=f"!{row}")
    # A longer path containing the row's text is another path.
    assert not names(ExecStart=f"/other{row}")
    assert not names(ExecStart=f"{row}+other")
    assert not names(ExecStart=f"{row}0")
    assert not names(WorkingDirectory=str(project), ExecStart="worktrees/validate/slot010")

    # A row spelled through a symlink matches its resolved spelling.
    real = tmp_path / "real"
    (real / "worktrees").mkdir(parents=True)
    link = tmp_path / "link"
    link.symlink_to(real)
    linked = wrkslots._row_path_identity(link / "worktrees" / "validate" / "slot01")
    assert wrkslots._UnitPathResolver().names(
        {"ExecStart": f"{real}/worktrees/validate/slot01"}, linked
    )


def test_a_parent_step_after_a_symlink_leaves_the_symlink_target(tmp_path: Path) -> None:
    """``link/..`` is the parent of the link's target, as the kernel reads it.

    Lexical normalization would drop the symlink with the ``..`` and read
    the link's own directory instead.
    """

    row = tmp_path / "project" / "worktrees" / "validate" / "slot01"
    (row / "product" / "build").mkdir(parents=True)
    inside = tmp_path / "inside"
    inside.symlink_to(row / "product")
    deep = tmp_path / "deep"
    deep.symlink_to(row / "product" / "build")
    alias = tmp_path / "alias"
    alias.symlink_to(row)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    assert names(ExecStart=f"{inside}/..")
    assert names(ExecStart=f"make -C {deep}/../..")
    assert names(WorkingDirectory=str(deep), ExecStart="git\n-C\n../..\nstatus")
    # The parent of the row, reached through a symlink to the row, is
    # reached through the row: it stops resolving once the row is gone.
    assert names(ExecStart=f"{alias}/..")
    # The parent reached without passing through the row is outside it.
    (row.parent / "slot02").mkdir()
    assert not names(ExecStart=f"{row.parent}/slot02/..")
    # A row recorded through a symlink and ``..`` is the directory it reaches.
    stepped = wrkslots._row_path_identity(inside / "..")
    assert wrkslots._UnitPathResolver().names({"ExecStart": f"{row}/out"}, stepped)


def test_a_row_path_with_a_space_is_one_argument(tmp_path: Path) -> None:
    """An argv element, option value or quoted shell word is one path.

    Each line of a property is one element; splitting it at whitespace
    alone would break a row path that contains a space into two
    unrelated words.
    """

    row = tmp_path / "with space" / "project" / "worktrees" / "validate" / "slot01"
    row.mkdir(parents=True)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    assert names(ExecStart=f"make\n-C\n{row}/product")
    assert names(ExecStart=f"tool\n--checkout={row}")
    assert names(ExecStart=f"/bin/sh\n-c\ncd '{row}/product' && make")
    assert names(ExecStart=f'/bin/sh\n-c\nmake --checkout="{row}"')
    assert names(Environment=f"CHECKOUT={row}/build")
    assert not names(ExecStart=f"make\n-C\n{row}0")
    assert not names(ExecStart=f"/bin/sh\n-c\ncd '{row}0' && make")


def test_a_row_path_holding_separators_is_one_option_or_assignment_value(
    tmp_path: Path,
) -> None:
    """A row path holding ``:`` and a space is whole in an option or assignment.

    Splitting at separators breaks it apart, so the value after ``=`` (or
    after a separator that ``/`` follows) is a candidate as a whole, and
    the row's own spellings are found as text wherever they stand.
    """

    row = tmp_path / "with: space" / "project" / "worktrees" / "validate" / "slot01"
    row.mkdir(parents=True)
    alias = tmp_path / "alias: link"
    alias.symlink_to(row)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    assert names(ExecStart=f"tool\n--checkout={row}")
    assert names(Environment=f"CHECKOUT={row}")
    assert names(ExecStart=f"/bin/sh\n-c\nmake --checkout='{row}/product'")
    assert names(Environment=f"PATH=/usr/bin:{row}:/bin")
    assert names(ExecStart=f"tool\n--checkout={alias}/product")
    assert names(Environment=f"SEARCH=/usr/lib;{alias}/lib")
    assert not names(ExecStart=f"tool\n--checkout={row}0")
    assert not names(Environment=f"CHECKOUT=/other{row}")
    assert not names(Environment=f"CHECKOUT={row}+other")

    # A row path holding a newline spans two lines of the property, and so
    # two elements, but is still found whole.
    broken = tmp_path / "with\nnewline" / "slot01"
    broken.mkdir(parents=True)
    assert wrkslots._UnitPathResolver().names(
        {"ExecStart": f"make\n-C\n{broken}/product"}, wrkslots._row_path_identity(broken)
    )


def test_a_relative_path_holding_a_newline_is_one_argument(tmp_path: Path) -> None:
    """A relative argument holding a newline is one path below the working directory.

    The row's own spelling is absolute, so it is not found as text; the
    element has to be read whole.  ``_user_systemd_properties`` separates
    the elements of an array property with a NUL character, which no D-Bus
    string holds, and a property string is also read whole.
    """

    row = tmp_path / "with\nnewline" / "slot01"
    row.mkdir(parents=True)
    (tmp_path / "with\nnewline" / "slot010").mkdir()

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    base = str(tmp_path)
    assert names(WorkingDirectory=base, ExecStart="tool\0--checkout=with\nnewline/slot01/product")
    assert names(WorkingDirectory=base, ExecStart="make\0-C\0with\nnewline/slot01")
    assert names(WorkingDirectory=base, ExecStart="tool\n--checkout=with\nnewline/slot01/product")
    assert not names(WorkingDirectory=base, ExecStart="tool\0--checkout=with\nnewline/slot010")
    assert not names(WorkingDirectory=base, ExecStart="make\0-C\0with\nnewline/slot010")


@pytest.mark.parametrize("trailing", ["\n", " ", "\t"], ids=["newline", "space", "tab"])
def test_a_working_directory_ending_in_whitespace_keeps_it(
    tmp_path: Path, trailing: str
) -> None:
    """Whitespace that ends ``WorkingDirectory`` is part of the directory's name.

    The manager reports the property as its own string, so a relative
    argument is resolved against the directory with the whitespace, and
    removing it would resolve the argument below another directory.
    """

    base = tmp_path / f"base{trailing}"
    row = base / "slot01"
    row.mkdir(parents=True)
    (tmp_path / "base" / "slot01").mkdir(parents=True)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    assert names(WorkingDirectory=str(base), ExecStart="/bin/tool\0--checkout=slot01/product")
    assert names(WorkingDirectory=str(base), ExecStart="make\0-C\0slot01")
    assert not names(
        WorkingDirectory=str(tmp_path / "base"), ExecStart="/bin/tool\0--checkout=slot01/product"
    )


def test_user_systemd_string_arrays_separate_elements_with_nul(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Each string of an array property is one NUL-separated element.

    A newline can be part of an argument, so it cannot also mark where one
    argument ends; D-Bus strings never hold a NUL character.
    """

    replies = (
        {"type": "a(sasbttttuii)", "data": [
            ["/bin/tool", ["tool", "--checkout=with\nnewline/slot01"], False,
             0, 0, 0, 0, 0, 0, 0],
        ]},
        {"type": "as", "data": ["A=1", "B=two\nlines"]},
        {"type": "s", "data": "one\nstring"},
    )
    monkeypatch.setattr(
        wrkslots,
        "_run_user_busctl",
        lambda *_args, **_kwargs: b"".join(
            (json.dumps(reply) + "\n").encode("utf-8") for reply in replies
        ),
    )
    budget = wrkslots._ReadOnlyCommandBudget.start(
        timeout_seconds=30, stdout_limit=1024 * 1024, stderr_limit=64 * 1024
    )
    values = wrkslots._user_systemd_properties(
        "/org/freedesktop/systemd1/unit/unit_2eservice",
        "org.freedesktop.systemd1.Service",
        (("ExecStart", "a(sasbttttuii)"), ("Environment", "as"), ("WorkingDirectory", "s")),
        budget,
    )
    assert values == {
        "ExecStart": "/bin/tool\0tool\0--checkout=with\nnewline/slot01",
        "Environment": "A=1\0B=two\nlines",
        "WorkingDirectory": "one\nstring",
    }


def test_a_quoted_shell_word_before_a_control_operator_is_one_path(tmp_path: Path) -> None:
    """A quoted path written just before ``;`` or ``&&`` is one shell word.

    Plain shell-word splitting joins the operator to the word
    (``'/x/alias: link';`` reads as ``/x/alias: link;``), and the
    separators inside the symlink's name split it apart.  A word is also
    read with shell control operators as words of their own, and a shell
    word that holds shell text (``bash -c "..."``) is read again.
    """

    row = tmp_path / "project" / "worktrees" / "validate" / "slot01"
    row.mkdir(parents=True)
    alias = tmp_path / "alias: link"
    alias.symlink_to(row)
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    other = tmp_path / "other: link"
    other.symlink_to(elsewhere)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    assert names(ExecStart=f"/bin/sh\0-c\0cd '{alias}'; make")
    assert names(ExecStart=f"/bin/sh\n-c\ncd '{alias}'; make")
    assert names(ExecStart=f"/bin/sh\0-c\0cd '{alias}/product'&&make")
    assert names(ExecStart=f"/bin/sh\0-c\0bash -c \"cd '{alias}/product'; make\"")
    assert names(ExecStart=f"/bin/sh -c \"bash -c \\\"cd '{alias}'|| exit\\\"\"")
    assert not names(ExecStart=f"/bin/sh\0-c\0cd '{other}'; make")
    assert not names(ExecStart=f"/bin/sh\0-c\0bash -c \"cd '{other}/product'; make\"")


def test_a_path_that_passes_through_the_row_names_it(
    tmp_path: Path,
) -> None:
    """``<row>/../slot02`` ends at the sibling but passes through the row.

    The kernel looks up every component of a path, so ``cd
    <row>/../slot02`` fails once the row is removed: the unit depends on
    the row.  The row's spelling found as text names the row whatever
    follows it, and a word resolved against the working directory names
    it when its resolution passes through the row.  A path that only
    reaches the row's parent or a sibling, without passing through the
    row, does not name it.
    """

    row = tmp_path / "project" / "worktrees" / "validate" / "slot01"
    (row / "product" / "build").mkdir(parents=True)
    (row.parent / "slot02").mkdir()
    (row / "deep").symlink_to(row / "product" / "build")
    spaced = tmp_path / "with space" / "slot01"
    spaced.mkdir(parents=True)
    (spaced.parent / "slot02").mkdir()

    def names(target: Path, **unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(target))

    project = tmp_path / "project"
    assert names(row, ExecStart=f"{row}/../slot02")
    assert names(row, ExecStart=f"make\0-C\0{row}/../slot02/build")
    assert names(row, Environment=f"PATH=/bin:{row}/../slot02:/usr/bin")
    assert names(spaced, ExecStart=f"/bin/sh\0-c\0cd '{spaced}/../slot02' && make")
    relative = "worktrees/validate/slot01/../slot02"
    assert names(row, WorkingDirectory=str(project), ExecStart=f"make\0-C\0{relative}")
    assert names(row, WorkingDirectory=str(project), ExecStart=f"cd {relative} && make")
    assert not names(row, ExecStart=f"{row.parent}/slot02")
    assert not names(
        row, WorkingDirectory=str(project), ExecStart="worktrees/validate/slot02/../slot01x"
    )
    assert not names(row, WorkingDirectory=str(project), ExecStart="worktrees/validate/slot02")
    assert names(row, ExecStart=f"{row}/../slot01/product")
    assert names(row, ExecStart=f"{row}/product/../build")
    assert names(row, ExecStart=f"{row}/deep/../..")
    assert names(spaced, ExecStart=f"/bin/sh\0-c\0cd '{spaced}/../slot01' && make")


def test_a_parent_step_inside_a_symlink_target_names_the_row(tmp_path: Path) -> None:
    """A symlink whose target passes through the row makes every path
    through the symlink depend on the row.

    ``alias -> <row>/../slot02`` ends at the sibling, but the kernel looks
    the row up each time it follows ``alias``, so ``alias/product`` stops
    resolving once the row is gone.  The target is relative or absolute,
    is reached through a chain of symlinks or below another symlink's
    target, and is met twice in one path; each still names the row, for a
    unit word and for a run handle's checkout alike.
    """

    row = tmp_path / "project" / "worktrees" / "validate" / "slot01"
    row.mkdir(parents=True)
    (row.parent / "slot02" / "product").mkdir(parents=True)
    relative = tmp_path / "relative"
    relative.symlink_to("project/worktrees/validate/slot01/../slot02")
    absolute = tmp_path / "absolute"
    absolute.symlink_to(f"{row}/../slot02")
    chain = tmp_path / "chain"
    chain.symlink_to("relative")
    below = tmp_path / "below"
    below.symlink_to(f"{relative}/product")
    clean = tmp_path / "clean"
    clean.symlink_to(row.parent / "slot02")
    identity = wrkslots._row_path_identity(row)

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, identity)

    for word in (
        f"{relative}/product",
        f"{absolute}/product",
        f"{chain}/product",
        str(below),
        f"{absolute}/../../../../absolute/product",
    ):
        assert names(ExecStart=word), word
        assert wrkslots._UnitPathResolver().path_names(word, identity), word
    assert names(WorkingDirectory=str(tmp_path), ExecStart="make\0-C\0chain/product")
    assert not names(ExecStart=f"{clean}/product")
    assert not wrkslots._UnitPathResolver().path_names(f"{clean}/product", identity)


def test_completed_validation_removal_refuses_a_queued_unit_through_a_symlink_via_the_row(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A queued job whose command runs through ``alias -> <row>/../other``
    still needs the row, so the row is not removed."""

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    slot_directory = _slot_directory(project)
    (slot_directory.parent / "other" / "product").mkdir(parents=True)
    alias = tmp_path / "alias"
    alias.symlink_to(f"{slot_directory}/../other")
    queued = _unit(Id="queued-run.service", PendingJob="yes", ExecStart=f"{alias}/product")
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (queued,))

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority reports the run may still use slot slot01" in error
    assert "user-systemd unit queued-run.service names validation row slot01" in error
    _assert_retained(project, tree)


def test_resolving_a_long_path_looks_up_a_bounded_part_of_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A path longer than any system call accepts costs a bounded number of
    lookups, and the resolver keeps only the ancestors that exist.

    The path lies below a directory that does not exist, so no lookup below
    that directory can succeed; resolving all of its 3,002 components, and
    keeping each of their ancestor spellings, is work that grows with the
    square of its length.
    """

    row = tmp_path / "row"
    row.mkdir()
    identity = wrkslots._row_path_identity(row)
    deep = f"/nonexistent-{uuid.uuid4().hex}/" + "a/" * 3000 + "end"
    lookups: list[str] = []
    resolutions: list[str] = []
    real_stat, real_lstat, real_realpath = os.stat, os.lstat, os.path.realpath

    def counted_stat(path: str) -> os.stat_result:
        lookups.append(path)
        return real_stat(path)

    def counted_lstat(path: str) -> os.stat_result:
        lookups.append(path)
        return real_lstat(path)

    def counted_realpath(path: str) -> str:
        resolutions.append(path)
        return real_realpath(path)

    resolver = wrkslots._UnitPathResolver()
    with monkeypatch.context() as patch:
        patch.setattr(os, "stat", counted_stat)
        patch.setattr(os, "lstat", counted_lstat)
        patch.setattr(os.path, "realpath", counted_realpath)
        named = resolver.path_names(deep, identity)

    assert not named
    assert len(lookups) <= PATH_MAX // 2 + 2, len(lookups)
    assert lookups and max(map(len, lookups)) < PATH_MAX
    assert not resolutions
    assert sum(map(len, resolver._files)) < 2 * PATH_MAX


def test_a_long_path_still_names_its_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Bounding the lookups keeps every reading that names a row.

    A long path inside the row, inside it through a symlink, or back at it
    after as many ``..`` steps as it has components names the row; one
    beside the row does not.  A lookup that fails for one directory only
    (here, permission to stat it) does not end the ancestor walk, so the
    row's own file is still found below it.
    """

    row = tmp_path / "row"
    row.mkdir()
    alias = tmp_path / "alias"
    alias.symlink_to(row)
    tail = "a/" * 3000 + "end"
    identity = wrkslots._row_path_identity(row)
    resolver = wrkslots._UnitPathResolver()

    assert resolver.path_names(f"{row}/{tail}", identity)
    assert resolver.path_names(f"{alias}/{tail}", identity)
    assert resolver.path_names(f"{alias}/{tail}/" + "../" * 3001, identity)
    assert resolver.path_names(f"/{tail}/" + "../" * 3001 + str(alias)[1:], identity)
    assert not resolver.path_names(f"{tmp_path}/other/{tail}", identity)
    assert wrkslots._UnitPathResolver().names(
        {"ExecStart": f"tool\n--checkout={alias}/{tail}"}, identity
    )

    by_file = wrkslots._RowPathIdentity(row, ("/elsewhere",), identity.file)
    parent = os.path.realpath(tmp_path)
    denied: list[str] = []
    real_stat = os.stat

    def stat(path: str) -> os.stat_result:
        if path == parent:
            denied.append(path)
            raise PermissionError(13, "Permission denied", path)
        return real_stat(path)

    with monkeypatch.context() as patch:
        patch.setattr(os, "stat", stat)
        assert wrkslots._UnitPathResolver().path_names(f"{row}/product", by_file)
    assert denied == [parent]


def _realpath_tree(root: Path) -> list[str]:
    """Build a tree of directories, files and symlinks; return its names."""

    (root / "a" / "b" / "c").mkdir(parents=True)
    (root / "f").write_text("file")
    (root / "a" / "file").write_text("file")
    links = {
        "rel": "a/b",
        "abs": str(root / "a"),
        "chain1": "chain2",
        "chain2": "chain3",
        "chain3": "a",
        "dangling": "missing/x",
        "loop1": "loop2",
        "loop2": "loop1",
        "self": "self",
        "up": "..",
        "dot": ".",
        "deep": "a/b/c/../../..",
        "filelink": "f",
        "slashed": "a/b/",
        "doubled": "/" + str(root / "a") + "//b",
        "a/b/back": "../../rel/..",
        "a/b/c/home": "../../../abs/b",
        "a/inner": "../loop1/x",
        "a/escape": "../" * 12 + str(root)[1:] + "/a",
    }
    for name, target in links.items():
        (root / name).symlink_to(target)
    return [
        "a", "b", "c", "f", "file", "missing", *(Path(name).name for name in links),
    ]


def test_resolving_a_path_matches_realpath_exactly(tmp_path: Path) -> None:
    """The resolver's walk gives ``os.path.realpath``'s answer for any path.

    Random absolute paths over a tree of relative, absolute, chained,
    dangling, looping and parent symlinks, files, missing names, ``.``,
    ``..``, empty components, names longer than 255 bytes and paths longer
    than 4,096 characters are resolved by a fresh resolver and by one shared
    across all of them, whose memoized lookups must not change an answer.
    Truncating a long path to its first 4,096 characters changed answers:
    ``"/tmp/../" * 600 + "bin/.."`` resolved to ``/`` while ``realpath``
    follows ``/bin``.
    """

    import random

    root = tmp_path / "tree"
    root.mkdir()
    names = _realpath_tree(root)
    tokens = [*names, *names, ".", "..", "..", "", "x" * 300, "../" * 40]
    generator = random.Random(20261005)
    shared = wrkslots._UnitPathResolver()
    paths = [
        "/tmp/../" * 600 + "bin/..",
        f"{root}/" + "../" * 600 + "bin/..",
        f"{root}/loop1/" + "a/" * 2100,
        f"{root}/a/inner///c",
        f"{root}/self//..",
    ]
    for _ in range(4000):
        count = generator.randint(1, 40)
        if generator.random() < 0.05:
            count = generator.randint(200, 700)
        start = generator.choice([str(root), str(root), "/", "//", str(root) + "//"])
        paths.append(
            start + "/" + "/".join(generator.choice(tokens) for _ in range(count))
        )
    for path in paths:
        expected = os.path.realpath(path)
        assert wrkslots._UnitPathResolver()._realpath(path) == expected, path[:200]
        assert shared._realpath(path) == expected, path[:200]


def test_a_long_path_through_a_symlink_parent_names_its_row(tmp_path: Path) -> None:
    """A ``..`` after a symlink far beyond 4,096 characters still leaves its target.

    ``link`` points at ``row/sub``, so ``link/..`` is ``row`` to the kernel
    and to ``realpath``.  Resolving only the first 4,096 characters and
    normalizing the rest as text removed ``link`` with its ``..`` and read
    the path as the row's parent.
    """

    row = tmp_path / "row"
    (row / "sub").mkdir(parents=True)
    (tmp_path / "link").symlink_to(row / "sub")
    identity = wrkslots._row_path_identity(row)
    path = "/" + "../" * 1400 + f"{str(tmp_path)[1:]}/link/.."

    assert len(path) > PATH_MAX
    assert os.path.realpath(path) == os.path.realpath(row)
    assert wrkslots._UnitPathResolver().path_names(path, identity)
    assert wrkslots._UnitPathResolver().names(
        {"ExecStart": f"tool\n--checkout={path}/product"}, identity
    )
    # Stepping on out of the row still passes through it on the way.
    assert wrkslots._UnitPathResolver().path_names(
        "/" + "../" * 1400 + f"{str(tmp_path)[1:]}/link/../../other", identity
    )
    assert not wrkslots._UnitPathResolver().path_names(
        "/" + "../" * 1400 + f"{str(tmp_path)[1:]}/other", identity
    )


def test_resolving_a_long_path_below_a_missing_name_is_linear(tmp_path: Path) -> None:
    """No component below one that cannot be looked up is looked up.

    ``realpath`` looks up each of the 60,000 components below the missing
    name, each lookup longer than the one before; the walk looks up one.
    """

    row = tmp_path / "row"
    row.mkdir()
    deep = f"{tmp_path}/missing/" + "a/" * 60_000 + "../" * 60_001 + "row"
    resolver = wrkslots._UnitPathResolver()
    started = time.monotonic()
    resolved = resolver._realpath(deep)
    elapsed = time.monotonic() - started

    assert resolved == os.path.realpath(row)
    assert len(resolver._kinds) <= len(Path(tmp_path).parts) + 3, len(resolver._kinds)
    assert elapsed < 2.0, elapsed


def test_resolving_refuses_what_realpath_cannot_answer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An unreadable link target or too many nested links refuses."""

    previous = "target"
    (tmp_path / "target").mkdir()
    for index in range(wrkslots._REALPATH_LINK_NESTING + 2):
        name = f"nest{index}"
        (tmp_path / name).symlink_to(f"{previous}/.")
        previous = name
    with pytest.raises(wrkslots.Refusal, match="nests more than"):
        wrkslots._UnitPathResolver()._realpath(f"{tmp_path}/{previous}")

    (tmp_path / "link").symlink_to("target")

    def unreadable(path: str) -> str:
        raise PermissionError(13, "Permission denied", path)

    monkeypatch.setattr(os, "readlink", unreadable)
    with pytest.raises(wrkslots.Refusal, match="cannot be resolved"):
        wrkslots._UnitPathResolver()._realpath(f"{tmp_path}/link/x")


@pytest.mark.parametrize("name", ["\0slot01", "\ud800slot01"], ids=["nul", "unencodable"])
@pytest.mark.parametrize("parent", ["", "absent-parent/"], ids=["present", "missing"])
def test_resolving_refuses_a_path_that_realpath_cannot_look_up(
    tmp_path: Path, name: str, parent: str
) -> None:
    """A name holding a NUL, or a character the file-system encoding cannot
    encode, refuses as ``realpath`` raises for it, below a missing parent
    too, where no lookup reaches it."""

    path = f"{tmp_path}/{parent}{name}"
    with pytest.raises(ValueError):
        os.path.realpath(path)
    with pytest.raises(wrkslots.Refusal, match="cannot be resolved"):
        wrkslots._UnitPathResolver()._realpath(path)


def test_one_resolver_reads_each_unit_property_once_for_every_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Several rows judged against one unit share the parsing of its strings."""

    rows = []
    for name in ("slot01", "slot02", "slot03"):
        (tmp_path / name).mkdir()
        rows.append(wrkslots._row_path_identity(tmp_path / name))
    unit = _unit(ExecStart="/bin/tool\n--checkout=/srv/other", Environment="HOME=/home/x")
    parsed: list[str] = []
    real_words = wrkslots._unit_property_words

    def words(value: str, budget: wrkslots._UnitResolutionBudget) -> tuple[str, ...]:
        parsed.append(value)
        return real_words(value, budget)

    monkeypatch.setattr(wrkslots, "_unit_property_words", words)
    resolver = wrkslots._UnitPathResolver()

    assert not any(resolver.names(unit, row) for row in rows)
    assert sorted(parsed) == sorted(set(unit.values()))


def test_the_retained_handle_census_bounds_its_path_matching_by_time(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Matching a handle's paths with the rows counts against the census's
    time bound, and the census refuses once that bound has passed.

    The clock is simulated: matching the one handle's checkout takes longer
    than the whole bound, after its read was already checked.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    _write_run_handle(project, tree)
    config = wrkslots._load_config(str(project), "testhost")
    rows = [(record, (tree,)) for record in wrkslots._load_active(config).slots]
    assert [handle.unit for handle in wrkslots._retained_handles_for_absent_rows(
        config, rows
    )["slot01"]] == [RUN_UNIT]

    real_monotonic = time.monotonic
    elapsed = [0.0]
    real_path_names = wrkslots._UnitPathResolver.path_names

    def slow_path_names(
        self: wrkslots._UnitPathResolver, joined: str, row: wrkslots._RowPathIdentity
    ) -> bool:
        elapsed[0] += wrkslots._RETAINED_HANDLE_CENSUS_SECONDS + 60.0
        return real_path_names(self, joined, row)

    with monkeypatch.context() as patch:
        patch.setattr(time, "monotonic", lambda: real_monotonic() + elapsed[0])
        patch.setattr(wrkslots._UnitPathResolver, "path_names", slow_path_names)
        with pytest.raises(
            wrkslots.Refusal, match="retained validation handle census exceeded its time bound"
        ):
            wrkslots._retained_handles_for_absent_rows(config, rows)
    assert elapsed[0] > wrkslots._RETAINED_HANDLE_CENSUS_SECONDS


def test_the_retained_handle_census_bound_covers_the_row_identities(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The census's time bound starts before it reads the rows' identities.

    The clock is simulated: reading the row's identity takes longer than
    the whole bound, and the census refuses instead of starting its bound
    afterwards.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    _write_run_handle(project, tree)
    config = wrkslots._load_config(str(project), "testhost")
    rows = [(record, (tree,)) for record in wrkslots._load_active(config).slots]
    real_monotonic = time.monotonic
    elapsed = [0.0]
    real_identity = wrkslots._row_path_identity

    def slow_identity(path: Path) -> wrkslots._RowPathIdentity:
        elapsed[0] += wrkslots._RETAINED_HANDLE_CENSUS_SECONDS + 60.0
        return real_identity(path)

    with monkeypatch.context() as patch:
        patch.setattr(time, "monotonic", lambda: real_monotonic() + elapsed[0])
        patch.setattr(wrkslots, "_row_path_identity", slow_identity)
        with pytest.raises(
            wrkslots.Refusal, match="retained validation handle census exceeded its time bound"
        ):
            wrkslots._retained_handles_for_absent_rows(config, rows)
    assert elapsed[0] > wrkslots._RETAINED_HANDLE_CENSUS_SECONDS


def test_the_retained_handle_census_checks_its_bound_before_each_row_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The census stops at the first row identity read after its bound passed.

    The clock is simulated: reading the first of the row's three paths
    takes longer than the whole bound, and the other two are not read.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    _write_run_handle(project, tree)
    (tree / "product").mkdir()
    (tree / "build").mkdir()
    config = wrkslots._load_config(str(project), "testhost")
    rows = [
        (record, (tree, tree / "product", tree / "build"))
        for record in wrkslots._load_active(config).slots
    ]
    real_monotonic = time.monotonic
    elapsed = [0.0]
    read: list[Path] = []
    real_identity = wrkslots._row_path_identity

    def slow_identity(path: Path) -> wrkslots._RowPathIdentity:
        read.append(path)
        elapsed[0] += wrkslots._RETAINED_HANDLE_CENSUS_SECONDS + 60.0
        return real_identity(path)

    with monkeypatch.context() as patch:
        patch.setattr(time, "monotonic", lambda: real_monotonic() + elapsed[0])
        patch.setattr(wrkslots, "_row_path_identity", slow_identity)
        with pytest.raises(
            wrkslots.Refusal, match="retained validation handle census exceeded its time bound"
        ):
            wrkslots._retained_handles_for_absent_rows(config, rows)
    assert read == [tree]


def test_judging_rows_against_no_units_checks_the_time_bound(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reading the rows' identities for a judgement checks its time bound.

    With no unit to compare, nothing else would: a judgement whose bound has
    passed refuses instead of clearing the row.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    rows = [(record, (tree,)) for record in wrkslots._load_active(config).slots]
    resolver = wrkslots._UnitPathResolver(
        wrkslots._UnitResolutionBudget.start(
            deadline=time.monotonic() - 1.0, expired="the test's bound passed"
        )
    )
    with pytest.raises(wrkslots.Refusal, match="the test's bound passed"):
        wrkslots._assert_absent_validate_systemd_unrelated(
            rows, {}, (), snapshot=(), resolver=resolver
        )


def test_judging_rows_refuses_when_the_last_row_identity_outruns_the_bound(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A row identity read that crosses the time bound gives no answer.

    The clock is simulated: the bound has not passed when the identity
    read starts, and reading it takes 25 seconds, past the 20-second bound
    of one judgement.  With no unit to compare, nothing after the read
    would check the bound again.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    rows = [(record, (tree,)) for record in wrkslots._load_active(config).slots]
    real_monotonic = time.monotonic
    elapsed = [0.0]
    real_identity = wrkslots._row_path_identity

    def slow_identity(path: Path) -> wrkslots._RowPathIdentity:
        elapsed[0] += 25.0
        return real_identity(path)

    with monkeypatch.context() as patch:
        patch.setattr(time, "monotonic", lambda: real_monotonic() + elapsed[0])
        patch.setattr(wrkslots, "_row_path_identity", slow_identity)
        with pytest.raises(wrkslots.Refusal, match="20-second bound of one judgement"):
            wrkslots._assert_absent_validate_systemd_unrelated(rows, {}, (), snapshot=())
    assert elapsed[0] == 25.0


def test_judging_rows_refuses_when_the_last_property_scan_outruns_the_bound(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A unit whose last property is scanned past the time bound names no row only within it.

    The clock is simulated.  The active unit's last property is empty, as
    an earlier one was, so its candidate paths come from the judgement's
    memo without another bound check; the scan of that property for the
    row's spellings is where the time passes.  The judgement is first run
    once to count its scans, then again with the last scan taking 25
    seconds.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    rows = [(record, (tree,)) for record in wrkslots._load_active(config).slots]
    unit = _unit(Id="other.service", ActiveState="active", Description="")
    real_monotonic = time.monotonic
    elapsed = [0.0]
    real_mentions = wrkslots._property_mentions
    scans = [0]
    slow_scan = [0]

    def mentions(value: str, spelling: str) -> bool:
        scans[0] += 1
        if scans[0] == slow_scan[0]:
            elapsed[0] += 25.0
        return real_mentions(value, spelling)

    with monkeypatch.context() as patch:
        patch.setattr(time, "monotonic", lambda: real_monotonic() + elapsed[0])
        patch.setattr(wrkslots, "_property_mentions", mentions)
        wrkslots._assert_absent_validate_systemd_unrelated(rows, {}, (), snapshot=(unit,))
        assert list(unit.values())[-1] == "" and elapsed[0] == 0.0
        slow_scan[0] = scans[0]
        scans[0] = 0
        with pytest.raises(wrkslots.Refusal, match="20-second bound of one judgement"):
            wrkslots._assert_absent_validate_systemd_unrelated(
                rows, {}, (), snapshot=(unit,)
            )
    assert elapsed[0] == 25.0 and scans[0] == slow_scan[0]


def test_an_option_value_longer_than_one_system_call_path_is_read_whole(
    tmp_path: Path,
) -> None:
    """A value after ``=`` is a candidate path however long it is.

    ``/a/..`` repeated 1,000 times before the row makes the value longer
    than 4,096 characters, and the ``:`` in the row's path splits every
    shorter reading apart; only the whole value names the row.
    """

    row = tmp_path / "with:colon" / "slot01"
    row.mkdir(parents=True)
    (row.parent / "slot02").mkdir()

    def names(**unit: str) -> bool:
        return wrkslots._UnitPathResolver().names(unit, wrkslots._row_path_identity(row))

    steps = "/a/.." * 1000
    assert names(ExecStart=f"tool\0--checkout={steps}{row}/product")
    assert names(Environment=f"CHECKOUT={steps}{row}")
    assert not names(ExecStart=f"tool\0--checkout={steps}{row.parent}/slot02")


def test_a_judgement_refuses_once_its_unit_words_exceed_their_bound(
    tmp_path: Path,
) -> None:
    """Reading every value of a long search list costs characters with the
    square of its length, so a 130,001-character property refuses.

    Each ``:`` that a ``/`` follows starts a value running to the end of
    the property; read whole, the 10,000 values of this one hold about
    650 million characters.
    """

    (tmp_path / "slot01").mkdir()
    row = wrkslots._row_path_identity(tmp_path / "slot01")
    value = "X=" + ":".join(f"/no_{i:08d}" for i in range(10000))
    started = time.monotonic()
    with pytest.raises(wrkslots.Refusal, match="character bound of one judgement"):
        wrkslots._UnitPathResolver().names(_unit(Environment=value), row)
    assert time.monotonic() - started < wrkslots._UNIT_RESOLUTION_SECONDS


def test_a_judgement_refuses_once_its_path_lookups_or_time_exceed_their_bounds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Each ``lstat``, ``stat`` and ``readlink`` is counted, and the
    judgement's time bound is checked as the work is charged."""

    (tmp_path / "slot01").mkdir()
    row = wrkslots._row_path_identity(tmp_path / "slot01")
    paths = []
    for index in range(40):
        (tmp_path / f"other{index:02d}").mkdir()
        paths.append(str(tmp_path / f"other{index:02d}" / "build"))
    unit = _unit(ExecStart="\0".join(("/bin/tool", *paths)))
    assert not wrkslots._UnitPathResolver().names(unit, row)

    monkeypatch.setattr(wrkslots, "_UNIT_RESOLUTION_LOOKUP_LIMIT", 40)
    with pytest.raises(wrkslots.Refusal, match="40-lookup bound of one judgement"):
        wrkslots._UnitPathResolver().names(unit, row)

    monkeypatch.setattr(wrkslots, "_UNIT_RESOLUTION_LOOKUP_LIMIT", 1_000_000)
    monkeypatch.setattr(wrkslots, "_UNIT_RESOLUTION_SECONDS", 0.0)
    with pytest.raises(wrkslots.Refusal, match="0-second bound of one judgement"):
        wrkslots._UnitPathResolver().names(unit, row)


def test_reading_one_long_shell_word_stops_at_the_time_bound() -> None:
    """The time bound is checked while one shell word is read, not only
    between words.

    The quoted 2,000,000-character word holds a space, so it is one shell
    word and is read again as shell text; ``shlex`` takes seconds over a
    word that long.  The bound here is a fifth of a second.
    """

    value = "'" + "a" * 2_000_000 + " b'"
    started = time.monotonic()
    budget = wrkslots._UnitResolutionBudget.start(
        deadline=started + 0.2, expired="the test's bound passed"
    )
    with pytest.raises(wrkslots.Refusal, match="the test's bound passed"):
        wrkslots._unit_property_words(value, budget)
    assert time.monotonic() - started < 1.0


def test_a_judgement_past_its_time_bound_refuses_from_its_memos(tmp_path: Path) -> None:
    """Answers memoized for one row are not returned once the bound has passed.

    The first row warms the resolver's memos of the unit's words and paths,
    so asking about the second row reads and looks up nothing new; only
    checking the time bound for each property and each path stops it.
    """

    rows = []
    for name in ("slot01", "slot02", "other"):
        (tmp_path / name).mkdir()
        rows.append(wrkslots._row_path_identity(tmp_path / name))
    joined = f"{tmp_path}/other/build"
    unit = _unit(ExecStart=f"/bin/tool\0--checkout={joined}")
    budget = wrkslots._UnitResolutionBudget.start(expired="the test's bound passed")
    resolver = wrkslots._UnitPathResolver(budget)
    assert not resolver.names(unit, rows[0])
    assert not resolver.path_names(joined, rows[0])

    budget.deadline = time.monotonic() - 1.0
    with pytest.raises(wrkslots.Refusal, match="the test's bound passed"):
        resolver.names(unit, rows[1])
    with pytest.raises(wrkslots.Refusal, match="the test's bound passed"):
        resolver.path_names(joined, rows[1])


def test_a_unit_too_long_to_read_leaves_its_row_unverifiable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A judgement that runs out of its word budget cannot clear the row."""

    value = "X=" + ":".join(f"/no_{i:08d}" for i in range(10000))
    state, message = _judge_with_unit_enumerations(
        tmp_path,
        monkeypatch,
        [[_unit(), _unit(Id="other.service", ActiveState="active", Environment=value)]],
    )
    assert state == "unverifiable"
    assert "character bound of one judgement" in message


UNREADABLE_PID = 4_000_017


def _write_unreadable_generation_handle(
    project: Path, tree: Path, monkeypatch: pytest.MonkeyPatch, evidence: str
) -> None:
    """Record a run process whose generation or boot cannot be read."""

    _write_run_handle(
        project,
        tree,
        process_identity={
            "pid": UNREADABLE_PID,
            "start_ticks": 17,
            "boot_id": wrkslots._boot_id(Path("/proc")),
        },
    )
    if evidence == "boot-id":
        def boot_id(_proc_root: Path) -> str:
            raise wrkslots.Refusal("cannot read the machine boot id: test")

        # Only the run authority reads the boot id after this point in the
        # direct call below.
        monkeypatch.setattr(wrkslots, "_boot_id", boot_id)
        return
    real_start_ticks = wrkslots._process_start_ticks

    def start_ticks(pid_dir: Path) -> int | None:
        if pid_dir.name == str(UNREADABLE_PID):
            raise wrkslots.Refusal(
                f"process generation is indeterminate because {pid_dir / 'stat'} "
                "is unreadable: test"
            )
        return real_start_ticks(pid_dir)

    monkeypatch.setattr(wrkslots, "_process_start_ticks", start_ticks)


@pytest.mark.parametrize(
    ("evidence", "detail"),
    [
        ("boot-id", "cannot read the machine boot id"),
        ("pid-generation", "process generation is indeterminate"),
    ],
)
def test_unreadable_run_process_evidence_is_unverifiable_not_alive(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    evidence: str,
    detail: str,
) -> None:
    """Evidence that cannot be read is not evidence of a run.

    ``alive`` tells the operator to stop the run's unit, which is the wrong
    remedy when nothing shows a run at all.
    """

    project, tree = _prepare(
        tmp_path, monkeypatch, agent_liveness="dead", units=(_unit(),)
    )
    config = wrkslots._load_config(str(project), "testhost")
    records = wrkslots._load_active(config).slots
    _write_unreadable_generation_handle(project, tree, monkeypatch, evidence)

    states = wrkslots._validation_run_liveness_states(config, records)

    assert len(states) == 1
    state, message = next(iter(states.values()))
    assert state == "unverifiable", message
    assert detail in message


def test_completed_validation_removal_names_unreadable_run_evidence_as_unverifiable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    project, tree = _prepare(
        tmp_path, monkeypatch, agent_liveness="dead", units=(_unit(),)
    )
    _write_unreadable_generation_handle(project, tree, monkeypatch, "pid-generation")

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "validation-run authority is unverifiable for slot slot01" in error
    assert "process generation is indeterminate" in error
    assert "stop its unit" not in error
    _assert_retained(project, tree)


def _run_child(cgroup: str, *, pid: int = 4_000_029) -> wrkslots._AbsentProcessObservation:
    return wrkslots._AbsentProcessObservation(
        pid=pid, start_ticks=31, cgroup_path=cgroup, mount_namespace="mnt:[test]"
    )


RUN_CGROUP = f"/user.slice/app.slice/{RUN_UNIT}"


def _judge_with_process_tables(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    before: Sequence[wrkslots._AbsentProcessObservation],
    after: Sequence[wrkslots._AbsentProcessObservation],
) -> tuple[str, str]:
    """Judge one row whose process table changes during the unit enumeration."""

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    _write_run_handle(project, tree)
    enumerated = False

    def units() -> tuple[Mapping[str, str], ...]:
        nonlocal enumerated
        enumerated = True
        # The run started and finished during this enumeration.
        return (_unit(),)

    def processes(**_kwargs: object) -> tuple[wrkslots._AbsentProcessObservation, ...]:
        return tuple(after if enumerated else before)

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
    monkeypatch.setattr(wrkslots, "_absent_validate_process_snapshot", processes)
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )
    assert enumerated
    assert len(states) == 1
    return next(iter(states.values()))


def test_a_run_child_that_appears_during_the_unit_enumeration_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The unit reads inactive and unqueued, but its child is still running.

    The process table read before the enumeration is empty.  Judging the row
    from it alone reports ``dead``, and the later path census does not look
    at the run's control group, so a child holding no path escapes both.
    """

    state, message = _judge_with_process_tables(
        tmp_path, monkeypatch, before=(), after=(_run_child(f"{RUN_CGROUP}/payload"),)
    )

    assert state == "alive", message
    assert f"retained validation unit {RUN_UNIT} still has a live cgroup process" in message


def test_a_run_child_that_leaves_the_run_cgroup_during_the_enumeration_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The same exact process generation keeps the run's control group."""

    state, message = _judge_with_process_tables(
        tmp_path,
        monkeypatch,
        before=(_run_child(f"{RUN_CGROUP}/payload"),),
        after=(_run_child("/user.slice/app.slice/elsewhere.scope"),),
    )

    assert state == "alive", message
    assert f"retained validation unit {RUN_UNIT} still has a live cgroup process" in message


def test_a_run_child_that_exited_during_the_enumeration_is_not_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A generation absent from the later table has exited."""

    state, message = _judge_with_process_tables(
        tmp_path, monkeypatch, before=(_run_child(f"{RUN_CGROUP}/payload"),), after=()
    )

    assert state == "dead", message


def test_absent_row_recovery_sees_a_run_child_that_appears_during_the_unit_enumeration(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """recover-absent-validate-rows judges the same evidence the same way.

    Its census reads the process table, then the unit enumeration runs.  A
    run that starts and finishes inside that enumeration leaves its unit
    inactive and unqueued and its child in the unit's control group, which
    only a table read after the enumeration shows.
    """

    project, repository, _remote = make_project(tmp_path)
    record = prepare_absent_validate_row(project, repository, slot="gone", agent="validate-a")
    config = wrkslots._load_config(str(project), "testhost")
    _write_run_handle(
        project, wrkslots._stored_path(config, record.checkouts[0].path, "checkout")
    )
    input_path = write_absent_validate_input(project, [record])
    allow_test_host_for_absent_validate_recovery(project, monkeypatch)
    enumerated = False

    def units() -> tuple[Mapping[str, str], ...]:
        nonlocal enumerated
        enumerated = True
        return (_unit(),)

    def processes(**_kwargs: object) -> tuple[wrkslots._AbsentProcessObservation, ...]:
        return (_run_child(f"{RUN_CGROUP}/payload"),) if enumerated else ()

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
    monkeypatch.setattr(wrkslots, "_absent_validate_process_snapshot", processes)

    assert run_absent_validate_recovery(project, input_path, apply=False) == 3
    assert enumerated
    error = capsys.readouterr().err
    assert f"retained validation unit {RUN_UNIT} still has a live cgroup process" in error


# A launcher whose handle records its own process generation hands the run to
# a unit: the process queues the unit and then exits.  The control group named
# below holds the launcher, not the run's unit.
LAUNCHER_CGROUP = "/user.slice/app.slice/launcher.scope"


@dataclass(frozen=True)
class _Handoff:
    child: subprocess.Popen[bytes]
    observation: wrkslots._AbsentProcessObservation
    # How many user-systemd enumerations have run.
    enumerations: list[int]


@contextlib.contextmanager
def _handoff_process(
    monkeypatch: pytest.MonkeyPatch, project: Path, tree: Path, *, exits_during: int
) -> Iterator[_Handoff]:
    """Record a live child in the run's handle; it exits during an enumeration.

    The child is in every process table read while it runs.  It exits, and
    is reaped, during enumeration ``exits_during`` (0 is the first; a
    test passing -1 ends it itself), and the run's unit is in neither
    enumeration: it is queued after them, as the child's last act.
    """

    child = subprocess.Popen(
        ["sleep", "300"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        pid_dir = Path("/proc") / str(child.pid)
        start_ticks = wrkslots._process_start_ticks(pid_dir)
        namespace = wrkslots._mount_namespace(pid_dir)
        assert start_ticks is not None and namespace is not None
        _write_run_handle(
            project,
            tree,
            process_identity={
                "pid": child.pid,
                "start_ticks": start_ticks,
                "boot_id": wrkslots._boot_id(Path("/proc")),
            },
        )
        observation = wrkslots._AbsentProcessObservation(
            pid=child.pid,
            start_ticks=start_ticks,
            cgroup_path=LAUNCHER_CGROUP,
            mount_namespace=namespace,
        )
        enumerations = [0]

        def units() -> tuple[Mapping[str, str], ...]:
            if enumerations[0] == exits_during:
                child.kill()
                child.wait()
            enumerations[0] += 1
            return ()

        def processes(**_kwargs: object) -> tuple[wrkslots._AbsentProcessObservation, ...]:
            return (observation,) if child.poll() is None else ()

        monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
        monkeypatch.setattr(wrkslots, "_absent_validate_process_snapshot", processes)
        yield _Handoff(child, observation, enumerations)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()


def _handoff_message(pid: int, slot: str) -> str:
    return (
        f"records process generation {pid}, which ran just before the host evidence "
        f"was read, for row {slot}"
    )


def test_a_run_process_that_hands_off_to_its_unit_after_the_enumerations_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The recorded process ran through both enumerations, then exited.

    It queued its unit after the second enumeration, so no enumeration shows
    the unit, and by the judgment its generation is dead.  Its generation
    was read running just before the host evidence, and nothing read after
    that shows when it queued its unit, so its run is kept.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    with _handoff_process(monkeypatch, project, tree, exits_during=1) as handoff:
        states = wrkslots._validation_run_liveness_states(
            config, wrkslots._load_active(config).slots
        )
        assert handoff.enumerations == [2]
        assert handoff.child.poll() is not None

    assert len(states) == 1
    state, message = next(iter(states.values()))
    assert state == "alive", message
    assert _handoff_message(handoff.child.pid, "slot01") in message


def test_a_run_process_that_exits_during_the_first_enumeration_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A recorded generation read running before the host evidence is a run.

    It exits during the first enumeration, before the later process table.
    Its generation is read once, just before the host evidence, and only a
    generation that does not run then is known to have queued no unit
    since, so this one is kept too.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    with _handoff_process(monkeypatch, project, tree, exits_during=0) as handoff:
        states = wrkslots._validation_run_liveness_states(
            config, wrkslots._load_active(config).slots
        )
        assert handoff.enumerations == [2]

    assert len(states) == 1
    state, message = next(iter(states.values()))
    assert state == "alive", message
    assert _handoff_message(handoff.child.pid, "slot01") in message


def test_a_run_process_that_exits_before_the_host_evidence_is_not_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A recorded process that exited before its generation was read is no run.

    It exits after the handles are read and before its generation is read,
    so a unit it queued was queued before both enumerations, which show
    none.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    real_settle = wrkslots._settle_retained_handles
    with _handoff_process(monkeypatch, project, tree, exits_during=-1) as handoff:

        def settle(
            handles: Mapping[str, tuple[wrkslots._RetainedValidationHandle, ...]],
        ) -> bool:
            handoff.child.kill()
            handoff.child.wait()
            return real_settle(handles)

        monkeypatch.setattr(wrkslots, "_settle_retained_handles", settle)
        states = wrkslots._validation_run_liveness_states(
            config, wrkslots._load_active(config).slots
        )
        assert handoff.enumerations == [2]

    assert len(states) == 1
    state, message = next(iter(states.values()))
    assert state == "dead", message


# The process table reader itself; ``_prepare`` replaces the module's.
_REAL_PROCESS_SNAPSHOT = wrkslots._absent_validate_process_snapshot

# The raw ``exit`` system call ends only the calling thread.
_SYS_EXIT = {"x86_64": 60, "aarch64": 93}

# A child with a second thread, which sleeps; the child prints that
# thread's ID.  With an exit number, the thread group leader then ends
# itself by the raw ``exit`` system call and stays a zombie while the
# other thread runs; without one, it waits for the thread.
_THREADED_CHILD = """
import ctypes, sys, threading, time
thread = threading.Thread(target=time.sleep, args=(300,), daemon=True)
thread.start()
sys.stdout.write(f"{thread.native_id}\\n")
sys.stdout.flush()
if len(sys.argv) > 1:
    ctypes.CDLL(None).syscall(int(sys.argv[1]), 0)
thread.join()
"""


@dataclass(frozen=True)
class _ThreadedHandoff:
    child: subprocess.Popen[bytes]
    recorded: int
    start_ticks: int
    # Every process table the judgment read, and how many user-systemd
    # enumerations have run.
    tables: list[tuple[wrkslots._AbsentProcessObservation, ...]]
    enumerations: list[int]


@contextlib.contextmanager
def _threaded_handoff(
    monkeypatch: pytest.MonkeyPatch,
    project: Path,
    tree: Path,
    proc_root: Path,
    *,
    leader_exits: bool,
) -> Iterator[_ThreadedHandoff]:
    """Record a generation that the process tables do not show; it exits during
    the second enumeration.

    The handle records the thread group leader after it has exited while
    its other thread runs (``leader_exits``), or the other thread's ID.
    The process tables are the real reader over ``proc_root``, which holds
    only the child's thread group, as ``/proc`` lists it.  The run's unit
    is in neither enumeration: it is queued after them, as the child's last
    act.
    """

    arguments = [sys.executable, "-c", _THREADED_CHILD]
    if leader_exits:
        number = _SYS_EXIT.get(os.uname().machine)
        if number is None:
            pytest.skip(f"no raw exit system call number for {os.uname().machine}")
        arguments.append(str(number))
    child = subprocess.Popen(
        arguments, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
    )
    try:
        assert child.stdout is not None
        thread = int(child.stdout.readline())
        recorded = child.pid if leader_exits else thread
        if leader_exits:
            status = Path("/proc") / str(child.pid) / "status"
            deadline = time.monotonic() + 10
            while "\nState:\tZ" not in status.read_text(encoding="utf-8"):
                assert time.monotonic() < deadline, "the thread group leader did not exit"
                time.sleep(0.01)
        start_ticks = wrkslots._process_start_ticks(Path("/proc") / str(recorded))
        assert start_ticks is not None
        _write_run_handle(
            project,
            tree,
            process_identity={
                "pid": recorded,
                "start_ticks": start_ticks,
                "boot_id": wrkslots._boot_id(Path("/proc")),
            },
        )
        (proc_root / str(child.pid)).symlink_to(Path("/proc") / str(child.pid))
        tables: list[tuple[wrkslots._AbsentProcessObservation, ...]] = []
        enumerations = [0]

        def units() -> tuple[Mapping[str, str], ...]:
            if enumerations[0] == 1:
                child.kill()
                child.wait()
            enumerations[0] += 1
            return ()

        def processes(
            **kwargs: bool,
        ) -> tuple[wrkslots._AbsentProcessObservation, ...]:
            table = _REAL_PROCESS_SNAPSHOT(proc_root, **kwargs)
            tables.append(table)
            return table

        monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
        monkeypatch.setattr(wrkslots, "_absent_validate_process_snapshot", processes)
        yield _ThreadedHandoff(child, recorded, start_ticks, tables, enumerations)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()


@pytest.mark.parametrize("leader_exits", [True, False], ids=["exited-leader", "thread-id"])
def test_a_recorded_generation_that_the_process_tables_omit_is_read_directly(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, leader_exits: bool
) -> None:
    """A run process the tables cannot show still hands off to its unit.

    The tables list thread groups and skip terminal ones.  A thread group
    leader that has exited while another of its threads runs is terminal,
    and a thread ID is not a group, so neither table shows the recorded
    generation while the child runs.  The child queues its unit after the
    second enumeration and exits; the generation read just before the host
    evidence keeps its run.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    with _threaded_handoff(
        monkeypatch, project, tree, proc_root, leader_exits=leader_exits
    ) as handoff:
        states = wrkslots._validation_run_liveness_states(
            config, wrkslots._load_active(config).slots
        )
        assert handoff.enumerations == [2]
        assert handoff.child.poll() is not None

    # The process tables read while the child ran omit the recorded generation.
    assert len(handoff.tables) == 2
    for table in handoff.tables:
        assert (handoff.recorded, handoff.start_ticks) not in {
            (process.pid, process.start_ticks) for process in table
        }
    shown = [] if leader_exits else [handoff.child.pid]
    assert [[process.pid for process in table] for table in handoff.tables] == [shown, shown]
    assert len(states) == 1
    state, message = next(iter(states.values()))
    assert state == "alive", message
    assert _handoff_message(handoff.recorded, "slot01") in message


def test_absent_agent_row_recovery_sees_a_run_process_hand_off_to_its_unit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """recover-absent-agent-rows judges the handoff through the same code."""

    project, repository, _remote = make_project(tmp_path)
    record = prepare_absent_agent_row(project, repository)
    config = wrkslots._load_config(str(project), "testhost")
    tree = wrkslots._stored_path(config, record.checkouts[0].path, "checkout")
    _prepare_absent_validate_recovery_host(project, monkeypatch)
    with _handoff_process(monkeypatch, project, tree, exits_during=1) as handoff:
        assert run_absent_agent_recovery(project, record, apply=False) == 3
        assert handoff.enumerations == [2]
        assert handoff.child.poll() is not None

    assert _handoff_message(handoff.child.pid, record.slot) in capsys.readouterr().err


def test_absent_validate_row_recovery_refuses_the_handoff_before_its_host_reads(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """recover-absent-validate-rows reads the recorded generation first.

    Its direct check runs before the host evidence, while the process still
    runs, so it refuses before any enumeration.
    """

    project, repository, _remote = make_project(tmp_path)
    project, input_path = _prepare_absent_validate_recovery(project, repository, monkeypatch)
    config = wrkslots._load_config(str(project), "testhost")
    record = next(
        record for record in wrkslots._load_active(config).slots if record.slot == "gone"
    )
    tree = wrkslots._stored_path(config, record.checkouts[0].path, "checkout")
    with _handoff_process(monkeypatch, project, tree, exits_during=1) as handoff:
        assert run_absent_validate_recovery(project, input_path, apply=False) == 3
        assert handoff.enumerations == [0]

    error = capsys.readouterr().err
    assert f"has live exact process generation {handoff.child.pid} for row gone" in error


# The tests below leave the host-evidence seam in place: the retained handles,
# this host's real process table (read on both sides of the unit enumeration)
# and the boot id are all read for real, and the run's process is a real child.
# Only the user-systemd enumeration is supplied, because some hosts that run
# this suite have no user bus and a busy host's real unit population changes
# under the enumeration; the enumeration's own parsing has its own tests.  The
# run's unit has a unique name so no real unit or process on the host matches
# it.  Each real process-table read is recorded, so a test can show that the
# judgment read this host's table and saw (or no longer saw) the run's process.


@dataclass(frozen=True)
class _RealHost:
    project: Path
    slot_path: Path
    tree: Path
    unit: str
    tables: list[tuple[wrkslots._AbsentProcessObservation, ...]]

    def tables_with(self, generation: tuple[int, int]) -> int:
        return sum(
            any((process.pid, process.start_ticks) == generation for process in table)
            for table in self.tables
        )


def _cgroup_filesystem_type(mount_point: str) -> str | None:
    """The type of the filesystem this process sees mounted at ``mount_point``."""

    found = None
    for line in Path("/proc/self/mountinfo").read_text(encoding="utf-8").splitlines():
        fields = line.split(" ")
        if len(fields) > 4 and fields[4] == mount_point and " - " in line:
            # A later mount at the same point hides an earlier one.
            found = line.split(" - ", 1)[1].split(" ")[0]
    return found


def _require_cgroup2_host() -> None:
    """Skip, saying why, where the real control-group read cannot succeed.

    The validation-run authority reads retained units' control-group members
    from the cgroup v2 hierarchy at /sys/fs/cgroup and refuses without one,
    which is the behaviour these host tests would then observe instead.
    """

    kind = _cgroup_filesystem_type("/sys/fs/cgroup")
    if kind != "cgroup2":
        pytest.skip(
            "this host has no cgroup v2 hierarchy at /sys/fs/cgroup (filesystem "
            f"type {kind or 'none'}), so the real retained-unit control-group read "
            "refuses there"
        )


@pytest.mark.parametrize("kind", ["cgroup", "tmpfs", None])
def test_a_host_without_cgroup2_skips_the_real_host_tests_with_a_reason(
    monkeypatch: pytest.MonkeyPatch, kind: str | None
) -> None:
    """A cgroup v1 or absent hierarchy is a stated skip, never a failure or a pass."""

    module = sys.modules[__name__]
    monkeypatch.setattr(module, "_cgroup_filesystem_type", lambda _mount_point: kind)
    with pytest.raises(pytest.skip.Exception) as skipped:
        _require_cgroup2_host()
    assert "no cgroup v2 hierarchy at /sys/fs/cgroup" in str(skipped.value)
    assert f"filesystem type {kind or 'none'}" in str(skipped.value)


def _prepare_real_host(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> _RealHost:
    """One validation row whose recorded owner has exited, on the real host."""

    _require_cgroup2_host()
    project, _repository, _remote = make_project(tmp_path)
    slot_path = prepare_dead_validate_slots(project, ("slot01",))["slot01"]
    tree = checkout(project, slot="slot01", slot_type="validate")
    unit = f"wrkslots-test-{uuid.uuid4().hex}.service"
    stub_validate_batch_censuses(monkeypatch)
    monkeypatch.setattr(wrkslots, "_assert_slot_unused", lambda *_a, **_k: None)
    monkeypatch.setattr(
        wrkslots,
        "_user_systemd_snapshot",
        lambda: (_unit(Id=unit, ControlGroup=f"/user.slice/app.slice/{unit}"),),
    )
    # The suite's idle-host default replaces this seam; the marker keeps it.
    assert wrkslots._validation_run_host_evidence.__module__ == wrkslots.__name__
    tables: list[tuple[wrkslots._AbsentProcessObservation, ...]] = []
    real_snapshot = wrkslots._absent_validate_process_snapshot

    def recorded_snapshot(
        proc_root: Path = Path("/proc"), *, include_owner_cgroups: bool = True
    ) -> tuple[wrkslots._AbsentProcessObservation, ...]:
        table = real_snapshot(proc_root, include_owner_cgroups=include_owner_cgroups)
        tables.append(table)
        return table

    monkeypatch.setattr(wrkslots, "_absent_validate_process_snapshot", recorded_snapshot)
    return _RealHost(project, slot_path, tree, unit, tables)


@dataclass(frozen=True)
class _RunProcess:
    child: subprocess.Popen[bytes]
    generation: tuple[int, int]


@contextlib.contextmanager
def _real_run_process(host: _RealHost) -> Iterator[_RunProcess]:
    """Record a live child as the run's process generation in its handle."""

    child = subprocess.Popen(
        ["sleep", "300"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        start_ticks = wrkslots._process_start_ticks(Path("/proc") / str(child.pid))
        assert start_ticks is not None
        _write_run_handle(
            host.project,
            host.tree,
            unit=host.unit,
            process_identity={
                "pid": child.pid,
                "start_ticks": start_ticks,
                "boot_id": wrkslots._boot_id(Path("/proc")),
            },
        )
        yield _RunProcess(child, (child.pid, start_ticks))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()


def _end_run(host: _RealHost, run: _RunProcess) -> None:
    """The run's process exits and is reaped, so its generation is gone."""

    run.child.kill()
    run.child.wait()
    host.tables.clear()


def _assert_judged_live_run(host: _RealHost, run: _RunProcess, text: str) -> None:
    """The refusal named the run's process, and the real table showed it."""

    assert f"has live exact process generation {run.child.pid} for row slot01" in text
    # Read on both sides of the unit enumeration.
    assert host.tables_with(run.generation) >= 2


def _assert_judged_ended_run(host: _RealHost, run: _RunProcess) -> None:
    assert len(host.tables) >= 2
    assert host.tables_with(run.generation) == 0


def test_real_host_completed_removal_waits_for_the_run_process(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    host = _prepare_real_host(tmp_path, monkeypatch)

    with _real_run_process(host) as run:
        assert _remove_completed(host.project) == 3
        error = capsys.readouterr().err
        assert "validation-run authority reports the run may still use slot slot01" in error
        _assert_judged_live_run(host, run, error)
        _assert_retained(host.project, host.tree)

        _end_run(host, run)
        assert _remove_completed(host.project) == 0, capsys.readouterr().err
        _assert_judged_ended_run(host, run)

    assert not host.slot_path.exists()
    assert active_slots(host.project) == []


def _remove_batch(project: Path) -> int:
    return wrkslots.main(
        [
            "--project-root",
            str(project),
            "remove-validate-batch",
            "--coordinator-pid",
            str(os.getpid()),
            "--slot",
            "slot01=1",
        ]
    )


def test_real_host_batch_seal_waits_for_the_run_process(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    host = _prepare_real_host(tmp_path, monkeypatch)
    config = wrkslots._load_config(str(host.project), "testhost")

    with _real_run_process(host) as run:
        # A batch retains each refused slot and exits 1.
        assert _remove_batch(host.project) == 1
        output = capsys.readouterr().out
        assert (
            "RETAINED: slot01 reason=validation-run authority reports the run may "
            "still use slot slot01"
        ) in output
        _assert_judged_live_run(host, run, output)
        assert not wrkslots._validate_batch_seal_journal_path(config).exists()
        _assert_retained(host.project, host.tree)

        _end_run(host, run)
        assert _remove_batch(host.project) == 0, capsys.readouterr()
        _assert_judged_ended_run(host, run)

    assert not host.slot_path.exists()
    assert active_slots(host.project) == []


def test_real_host_finish_recovery_waits_for_the_run_process(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    host = _prepare_real_host(tmp_path, monkeypatch)
    interrupt_validate_batch(host.project, monkeypatch, "after-finish-journal", ("slot01",))
    config = wrkslots._load_config(str(host.project), "testhost")
    finish_path = wrkslots._journal_path(config)
    assert finish_path.exists()
    recover = [
        "--project-root",
        str(host.project),
        "recover",
        "--coordinator-pid",
        str(os.getpid()),
    ]

    # A run that starts on the checkout after the batch journaled its finish.
    with _real_run_process(host) as run:
        host.tables.clear()
        assert wrkslots.main(recover) == 3
        error = capsys.readouterr().err
        _assert_judged_live_run(host, run, error)
        assert finish_path.exists()
        _assert_retained(host.project, host.tree)

        _end_run(host, run)
        assert wrkslots.main(recover) == 0, capsys.readouterr().err
        _assert_judged_ended_run(host, run)

    assert not host.slot_path.exists()
    assert not finish_path.exists()
    assert active_slots(host.project) == []


@pytest.mark.parametrize(
    ("lease_host", "expected"), [("foreign-host", "unverifiable"), (None, "dead")]
)
def test_an_ownerless_row_is_judged_local_by_its_coordinator_lease(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    lease_host: str | None,
    expected: str,
) -> None:
    """An ownerless row's lease names the host that registered it.

    With no owner, a host check that looks only at the owner accepts a row
    registered on another host and judges it ``dead`` from this host's empty
    evidence.  The same row is local when its lease names this host.
    """

    project, _tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    config = wrkslots._load_config(str(project), "testhost")
    state = wrkslots._load_active(config)
    record = state.slots[0]
    lease = record.coordinator_lease
    if lease_host is not None:
        lease = dataclasses.replace(lease, host_id=lease_host)
    ownerless = dataclasses.replace(record, owner=None, coordinator_lease=lease)
    wrkslots._write_active_state(
        config,
        wrkslots._replace_record(state, ownerless),
        action="test-ownerless",
        slot=record.slot,
    )

    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state_name, message = states[("testhost", "slot01", 1)]
    assert state_name == expected, message
    if expected == "unverifiable":
        assert "belongs to stable host foreign-host" in message


@pytest.mark.parametrize(
    ("view", "detail"),
    [
        ("pid", "not in the host's initial pid namespace"),
        ("cgroup", "not in the host's initial cgroup namespace"),
        ("hidepid", "/proc is mounted with hidepid=invisible"),
    ],
)
def test_a_restricted_process_view_is_unverifiable_not_dead(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, view: str, detail: str
) -> None:
    """A process table that need not show the host's runs is not evidence.

    In a child PID namespace, or under ``hidepid``, a host run and its
    children are missing from /proc; in a child cgroup namespace their
    control-group paths are relative to another root.  The empty table and
    the empty unit list here would otherwise judge the row ``dead``.
    """

    project, _tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    if view == "hidepid":
        monkeypatch.setattr(
            wrkslots,
            "_proc_superblock_options",
            lambda: ("rw", "hidepid=invisible"),
            raising=False,
        )
    else:
        monkeypatch.setattr(
            wrkslots,
            "_namespace_inode",
            lambda name: (
                4_026_532_999 if name == view else os.stat(f"/proc/self/ns/{name}").st_ino
            ),
            raising=False,
        )
    config = wrkslots._load_config(str(project), "testhost")

    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state, message = states[("testhost", "slot01", 1)]
    assert state == "unverifiable", message
    assert detail in message


_HOST_MOUNTS = (
    "1 0 0:1 / / rw - ext4 /dev/root rw\n"
    "2 1 0:2 / /proc rw,nosuid - proc proc rw\n"
)


@pytest.mark.parametrize(
    ("mount", "detail"),
    [
        ("3 2 0:3 / /proc/4242 rw - tmpfs tmpfs rw\n", "tmpfs mount at /proc/4242 masks"),
        ("3 2 0:3 / /proc/4242/fd rw - tmpfs tmpfs rw\n", "mount at /proc/4242/fd masks"),
        (
            "3 2 0:3 / /proc/77/with\\040space rw - tmpfs tmpfs rw\n",
            "mount at /proc/77/with space masks",
        ),
        ("3 2 0:3 / /proc rw - tmpfs tmpfs rw\n", "no process filesystem is mounted at /proc"),
        ("3 2 0:3 / /proc/sys/fs/binfmt_misc rw - binfmt_misc binfmt_misc rw\n", None),
        (
            "3 2 0:3 / /proc/4242 rw - tmpfs tmpfs rw\n4 2 0:4 / /proc rw - proc proc rw\n",
            None,
        ),
        (
            "3 2 0:4 / /proc rw - proc proc rw\n4 3 0:3 / /proc/4242 rw - tmpfs tmpfs rw\n",
            "tmpfs mount at /proc/4242 masks",
        ),
    ],
)
def test_a_mount_over_a_process_directory_in_proc_restricts_the_view(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, mount: str, detail: str | None
) -> None:
    """A mount over ``/proc/<pid>`` replaces that process's entries.

    The process table then need not show that run, so the view is
    restricted; a mount stacked over ``/proc`` itself hides the process
    filesystem.  A mount elsewhere below ``/proc`` hides no process, and
    neither does a mount over ``/proc/<pid>`` that a process filesystem
    mounted later over ``/proc`` hides; a mount over ``/proc/<pid>`` of that
    later process filesystem still does.
    """

    table = tmp_path / "mountinfo"
    table.write_text(_HOST_MOUNTS + mount, encoding="utf-8")
    monkeypatch.setattr(wrkslots, "_SELF_MOUNTINFO", table)
    initial = dict(wrkslots._INITIAL_NAMESPACE_INODES)
    monkeypatch.setattr(wrkslots, "_namespace_inode", lambda name: initial[name])

    if detail is None:
        wrkslots._assert_host_process_view()
        return
    with pytest.raises(wrkslots.Refusal) as refused:
        wrkslots._assert_host_process_view()
    assert detail in str(refused.value)


def test_this_host_process_view_is_the_host_view() -> None:
    """The suite runs on the host, so the real view passes the proof."""

    wrkslots._assert_host_process_view()


@pytest.mark.parametrize("form", ["symlink", "relative"])
def test_completed_validation_removal_refuses_a_queued_unit_naming_the_row_by_identity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    form: str,
) -> None:
    """A queued job names the row through a symlink or a relative path.

    Neither spelling contains the row's text, and the job has not started,
    so no process or path census can show it later.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    slot_directory = _slot_directory(project)
    if form == "symlink":
        alias = tmp_path / "checkout-alias"
        alias.symlink_to(slot_directory)
        evidence = {"ExecStart": f"/usr/bin/make\n-C\n{alias}/product"}
    else:
        evidence = {
            "WorkingDirectory": str(slot_directory.parent.parent),
            "ExecStart": f"git\n-C\n{slot_directory.parent.name}/{slot_directory.name}",
        }
    queued = _unit(Id="queued-run.service", PendingJob="yes", **evidence)
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (queued,))

    removed = _remove_completed(project)

    error = capsys.readouterr().err
    assert removed != 0
    assert "user-systemd unit queued-run.service names validation row slot01" in error
    _assert_retained(project, tree)


@pytest.mark.parametrize("form", ["/other{row}", "{row}+other"])
def test_completed_validation_removal_ignores_a_unit_naming_a_longer_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    form: str,
) -> None:
    """A path that contains the row's text is not the row's path."""

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    other = form.format(row=_slot_directory(project))
    running = _unit(
        Id="other-run.service",
        ActiveState="active",
        SubState="running",
        ExecStart=f"/usr/bin/env\n--checkout={other}",
        Environment=f"RUN_ROOT={other}",
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (running,))

    removed = _remove_completed(project)

    assert removed == 0, capsys.readouterr().err
    assert not tree.exists()
    assert active_slots(project) == []


class _Interrupted(RuntimeError):
    pass


def _interrupt_once_at(monkeypatch: pytest.MonkeyPatch, boundary: str) -> None:
    def interrupt(point: str) -> None:
        if point == boundary:
            monkeypatch.setattr(wrkslots, "_interrupt_for_test", lambda _point: None)
            raise _Interrupted(point)

    monkeypatch.setattr(wrkslots, "_interrupt_for_test", interrupt)


@pytest.mark.parametrize("evidence", ["queued-unit", "run-handle"])
def test_finish_recovery_judges_the_fenced_checkout(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    evidence: str,
) -> None:
    """Finish recovery moves the checkout to ``.slot01.fenced.1.<hex>`` and
    deletes it there.

    The interrupted removal's finish journal names the fence before the
    slot is renamed.  A queued job or a run handle naming the fenced
    checkout will use the files recovery is about to delete, although
    neither names the recorded path.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    slot_path = _slot_directory(project)
    _interrupt_once_at(monkeypatch, "after-finish-journal")
    with pytest.raises(_Interrupted):
        _remove_completed(project)
    config = wrkslots._load_config(str(project), "testhost")
    journal_paths = [
        path
        for path in (
            wrkslots._journal_path(config),
            wrkslots._finish_journal_path(config, "slot01"),
        )
        if path.exists()
    ]
    assert len(journal_paths) == 1, journal_paths
    journal = json.loads(journal_paths[0].read_text(encoding="utf-8"))
    assert journal["kind"] == "finish"
    fenced_slot = project / journal["fenced"]
    assert fenced_slot.name.startswith(".slot01.fenced.1.")
    assert not fenced_slot.exists()
    fenced_tree = fenced_slot / tree.relative_to(slot_path)
    if evidence == "queued-unit":
        unit = _unit(
            Id="queued-run.service",
            PendingJob="yes",
            ExecStart=f"/usr/bin/make\n-C\n{fenced_tree}",
        )
        expected = "user-systemd unit queued-run.service names validation row slot01"
    else:
        _write_run_handle(project, fenced_tree)
        unit = _unit(ActiveState="active", SubState="running")
        expected = f"retained validation unit {RUN_UNIT} may still use row slot01"
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (unit,))
    recover = [
        "--project-root",
        str(project),
        "recover",
        "--coordinator-pid",
        str(os.getpid()),
    ]

    assert wrkslots.main(recover) == 3
    error = capsys.readouterr().err
    assert "validation-run authority reports the run may still use slot slot01" in error
    assert expected in error
    assert tree.is_dir()
    assert len(active_slots(project)) == 1

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    assert wrkslots.main(recover) == 0, capsys.readouterr().err
    assert not fenced_slot.exists()
    assert not slot_path.exists()
    assert active_slots(project) == []


def test_a_unit_naming_a_fenced_checkout_on_disk_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A slot already renamed to its fence is judged at the fenced path.

    Every ``.slot01.fenced.1.*`` sibling on disk is a place the row's files
    may be, whichever operation renamed it there.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    slot_path = _slot_directory(project)
    fenced_slot = slot_path.parent / f".slot01.fenced.1.{uuid.uuid4().hex}"
    slot_path.rename(fenced_slot)
    fenced_tree = fenced_slot / tree.relative_to(slot_path)
    queued = _unit(
        Id="queued-run.service", PendingJob="yes", ExecStart=f"make\n-C\n{fenced_tree}"
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (queued,))
    config = wrkslots._load_config(str(project), "testhost")

    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state, message = states[("testhost", "slot01", 1)]
    assert state == "alive", message
    assert "user-systemd unit queued-run.service names validation row slot01" in message


def _judge_with_unit_enumerations(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    enumerations: Sequence[Sequence[Mapping[str, str]]],
    *,
    register_during: int | None = None,
    members: _MemberReader | None = None,
) -> tuple[str, str]:
    """Judge one row whose user-systemd units change between enumerations.

    The process tables are empty throughout: the run's processes started
    after each table's PID list.  With ``register_during`` set, the run's
    handle is written during that enumeration instead of beforehand.
    ``members`` replaces the retained-unit control-group read.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    if members is not None:
        monkeypatch.setattr(wrkslots, "_retained_unit_cgroup_members", members)
    if register_during is None:
        _write_run_handle(project, tree)
    calls = 0

    def units() -> tuple[Mapping[str, str], ...]:
        nonlocal calls
        if calls == register_during:
            _write_run_handle(project, tree)
        value = tuple(enumerations[min(calls, len(enumerations) - 1)])
        calls += 1
        return value

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )
    assert len(states) == 1
    return next(iter(states.values()))


def test_a_run_that_starts_during_the_later_process_table_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The first enumeration predates the run; the second sees it running."""

    state, message = _judge_with_unit_enumerations(
        tmp_path,
        monkeypatch,
        ((_unit(),), (_unit(ActiveState="active", SubState="running"),)),
    )

    assert state == "alive", message
    assert f"retained validation unit {RUN_UNIT} may still use row slot01" in message


def test_a_run_registered_during_the_evidence_reads_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A handle written after the first handle read is still judged."""

    state, message = _judge_with_unit_enumerations(
        tmp_path,
        monkeypatch,
        ((_unit(ActiveState="active", SubState="running"),),),
        register_during=0,
    )

    assert state == "alive", message
    assert f"retained validation unit {RUN_UNIT} may still use row slot01" in message


def test_a_handle_registered_after_the_first_enumeration_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A handle the later read found is a run the unit states cannot cover.

    The handle is written during the second user-systemd enumeration, and
    its unit is absent from both: it is queued only after them, and its job
    has not started, so no table, unit state or control group shows it.
    """

    state, message = _judge_with_unit_enumerations(
        tmp_path, monkeypatch, ((), ()), register_during=1
    )

    assert state == "alive", message
    assert "for row slot01 appeared while the host evidence was read" in message
    assert f"its unit {RUN_UNIT} may have been queued" in message


def _change_run_handle(handle: Path, tree: Path, change: str) -> None:
    """Rewrite, replace or remove a run handle as a launcher registering again would."""

    if change == "rewritten":
        value = json.loads(handle.read_text(encoding="utf-8"))
        value["checkout"] = str(tree / "after")
        handle.write_text(json.dumps(value), encoding="utf-8")
    elif change == "rewritten-alike":
        # The same bytes written again in place with a later modification
        # time, as a filesystem whose timestamps are fine-grained once read
        # records a rewrite made after a read: only the file's times differ.
        before = handle.stat()
        handle.write_bytes(handle.read_bytes())
        os.utime(handle, ns=(before.st_atime_ns, before.st_mtime_ns + 1))
    elif change == "replaced":
        fresh = handle.with_name(handle.name + ".new")
        fresh.write_bytes(handle.read_bytes())
        os.replace(fresh, handle)
    else:
        handle.unlink()


_HANDLE_CHANGES = {
    "rewritten": "changed",
    "rewritten-alike": "changed",
    "replaced": "changed",
    "removed": "disappeared or stopped naming the row",
}


@pytest.mark.parametrize("change", sorted(_HANDLE_CHANGES))
def test_a_handle_that_changes_during_the_evidence_reads_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, change: str
) -> None:
    """A handle rewritten, replaced or removed while the host is read is a run event.

    The handle names the row before and after (``rewritten`` moves its
    checkout within the row; ``rewritten-alike`` writes the same bytes in
    place with a later modification time; ``replaced`` puts a byte-identical
    file in its place), so the projected fields the two reads find can be
    equal.  The
    unit is absent from both user-systemd enumerations and every process
    table: a run registered again then is queued only after them.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    calls = 0

    def units() -> tuple[Mapping[str, str], ...]:
        nonlocal calls
        if calls == 1:
            _change_run_handle(handle, tree, change)
        calls += 1
        return ()

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state, message = states[("testhost", "slot01", 1)]
    assert state == "alive", message
    assert f"for row slot01 {_HANDLE_CHANGES[change]} while the host evidence" in message
    assert f"its unit {RUN_UNIT} may have been queued" in message
    # With the handle left as it is, the same evidence reads the run as over.
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )
    assert states[("testhost", "slot01", 1)][0] == "dead", states


def test_a_recently_changed_handle_is_read_again_after_its_timestamp_window(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A run that has just rewritten its handle is judged from a later read.

    A rewrite with the same bytes within one file timestamp tick of the
    write before it leaves the device, inode, size, bytes and times alike,
    so two equal reads within the window of the handle's change time do not
    show that no run registered again between them.  The units are read
    only once the change time is two seconds old, and the finished run then
    reads as over.  This uses this host's clock and the file's real times.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    ages: list[int] = []

    def units() -> tuple[Mapping[str, str], ...]:
        ages.append(time.time_ns() - handle.stat().st_ctime_ns)
        return ()

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    assert states[("testhost", "slot01", 1)][0] == "dead", states
    assert ages and min(ages) > 2_000_000_000, ages


@pytest.mark.parametrize("clock", ["stopped", "behind"])
def test_a_handle_still_inside_its_timestamp_window_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, clock: str
) -> None:
    """Two equal reads of a handle that is still that recent are not trusted.

    ``stopped`` holds the clock at the handle's change time, so the window
    never passes however long the wait; ``behind`` puts the change time ten
    seconds ahead of the clock, which is not waited for.  Either way a
    rewrite alike between the reads would not show, and the unit is absent
    from both user-systemd enumerations, so the run may have been queued
    after them.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    change = handle.stat().st_ctime_ns
    offset = 0 if clock == "stopped" else -10_000_000_000
    monkeypatch.setattr(
        wrkslots, "_retained_handle_clock_ns", lambda: change + offset, raising=False
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    config = wrkslots._load_config(str(project), "testhost")
    started = time.monotonic()
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )
    elapsed = time.monotonic() - started

    state, message = states[("testhost", "slot01", 1)]
    assert state == "alive", message
    assert (
        "for row slot01 may have been rewritten alike within one file timestamp "
        "tick while the host evidence" in message
    )
    assert f"its unit {RUN_UNIT} may have been queued" in message
    if clock == "behind":
        assert elapsed < 2.0, elapsed


@dataclass
class _CensusPhase:
    """Which retained handle census is running, counted from 1 (0 before
    the first), and whether it has begun reading a handle file."""

    census: int = 0
    read: bool = False


def _track_census_phase(monkeypatch: pytest.MonkeyPatch) -> _CensusPhase:
    """Count the handle censuses and note each one's first handle read."""

    phase = _CensusPhase()
    census = wrkslots._retained_handles_for_absent_rows
    read = wrkslots._read_regular_file_identity

    def counted(
        config: wrkslots.Config,
        rows: Sequence[tuple[wrkslots.ActiveRecord, tuple[Path, ...]]],
    ) -> Mapping[str, tuple[wrkslots._RetainedValidationHandle, ...]]:
        phase.census += 1
        phase.read = False
        return census(config, rows)

    def reading(
        path: Path, label: str, limit: int
    ) -> tuple[bytes, wrkslots._RegularFileIdentity]:
        if label == "retained validation handle":
            phase.read = True
        return read(path, label, limit)

    monkeypatch.setattr(wrkslots, "_retained_handles_for_absent_rows", counted)
    monkeypatch.setattr(wrkslots, "_read_regular_file_identity", reading)
    return phase


def _judge_with_census_clocks(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    readings: Sequence[tuple[tuple[int, int], tuple[int, int]]],
) -> tuple[str, str]:
    """Judge one row whose unchanged handle each census reads at given clocks.

    ``readings`` holds, for each handle census in turn, this host's realtime
    and monotonic clocks before the census reads its handle file and from
    that read on, as offsets in nanoseconds from the handle's change time
    and from an arbitrary start.  The handle is never written again, and
    the unit is absent from every enumeration, so only what the clocks say
    about a rewrite alike can keep the run alive.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    change = handle.stat().st_ctime_ns
    phase = _track_census_phase(monkeypatch)

    def clocks() -> tuple[int, int]:
        census = readings[min(max(phase.census, 1), len(readings)) - 1]
        return census[1] if phase.read else census[0]

    monkeypatch.setattr(
        wrkslots, "_retained_handle_clock_ns", lambda: change + clocks()[0], raising=False
    )
    monkeypatch.setattr(
        wrkslots,
        "_retained_handle_monotonic_ns",
        lambda: 10**15 + clocks()[1],
        raising=False,
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )
    return states[("testhost", "slot01", 1)]


_SECOND = 1_000_000_000


@pytest.mark.parametrize(
    ("readings", "change"),
    [
        pytest.param(
            (((5 * _SECOND, 0),) * 2, ((_SECOND // 5, _SECOND),) * 2),
            "may have been rewritten alike within one file timestamp tick",
            id="back-into-the-window",
        ),
        pytest.param(
            (((10 * _SECOND, 0),) * 2, ((7 * _SECOND, _SECOND),) * 2),
            "may have been rewritten alike while this host's realtime clock stepped back",
            id="back-short-of-the-window",
        ),
        pytest.param(
            (
                ((10 * _SECOND, 0),) * 2,
                ((11 * _SECOND, _SECOND), (_SECOND // 5, _SECOND + _SECOND // 10)),
            ),
            "may have been rewritten alike within one file timestamp tick",
            id="back-into-the-window-during-the-later-census",
        ),
        pytest.param(
            (
                ((10 * _SECOND, 0),) * 2,
                ((11 * _SECOND, _SECOND), (8 * _SECOND, _SECOND + _SECOND // 10)),
            ),
            "may have been rewritten alike while this host's realtime clock stepped back",
            id="back-short-of-the-window-during-the-later-census",
        ),
    ],
)
def test_an_equal_handle_read_across_a_realtime_clock_step_back_is_alive(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    readings: Sequence[tuple[tuple[int, int], tuple[int, int]]],
    change: str,
) -> None:
    """A handle the earlier read found old is not trusted after the clock goes back.

    Where file times keep whole seconds, a run that registers again, alike,
    after the realtime clock stepped back into its handle's timestamp tick
    leaves both reads equal.  ``back-into-the-window`` steps the clock from
    five seconds after the change time to a fifth of a second after it, so
    the later read finds the handle recent although the earlier one did not;
    ``back-short-of-the-window`` steps it three seconds back while one
    second passes, so neither read finds the handle recent, and only the
    fall of the realtime clock against the monotonic clock shows the step.
    The ``during-the-later-census`` cases step the clock after the later
    census has read the clocks and before it reads the handle, so only the
    clocks read after its last handle show the step.
    """

    state, message = _judge_with_census_clocks(tmp_path, monkeypatch, readings)

    assert state == "alive", message
    assert f"for row slot01 {change} while the host evidence was read" in message
    assert f"its unit {RUN_UNIT} may have been queued" in message


@pytest.mark.parametrize(
    "readings",
    [
        pytest.param(
            (((10 * _SECOND, 0),) * 2, ((11 * _SECOND, _SECOND),) * 2), id="steady"
        ),
        pytest.param(
            (
                ((10 * _SECOND, 0),) * 2,
                ((10 * _SECOND + _SECOND // 10, _SECOND // 2),) * 2,
            ),
            id="back-less-than-the-step-bound",
        ),
    ],
)
def test_an_equal_old_handle_read_without_a_clock_step_back_is_not_alive(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    readings: Sequence[tuple[tuple[int, int], tuple[int, int]]],
) -> None:
    """Equal old reads with the clocks moving together leave the run over.

    ``back-less-than-the-step-bound`` lets the realtime clock fall 0.4
    seconds against the monotonic clock, under the half-second bound and
    short of what a rewrite alike of a handle two seconds old needs.
    """

    state, message = _judge_with_census_clocks(tmp_path, monkeypatch, readings)

    assert state == "dead", message


@pytest.mark.parametrize(
    ("pauses", "expected"),
    [
        pytest.param(
            (("after", 4 * _SECOND // 10),) * 2,
            "dead",
            id="under-the-bound-after-each-realtime-read",
        ),
        pytest.param(
            (("before", 4 * _SECOND // 10),) * 2,
            "dead",
            id="under-the-bound-before-each-realtime-read",
        ),
        pytest.param((("after", 6 * _SECOND // 10),), "alive", id="past-the-bound-once"),
        pytest.param(
            (("before", 3 * _SECOND // 10), ("after", 3 * _SECOND // 10)),
            "alive",
            id="past-the-bound-together",
        ),
    ],
)
def test_a_pause_between_clock_reads_is_a_step_only_when_it_could_hide_one(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    pauses: tuple[tuple[str, int], ...],
    expected: str,
) -> None:
    """A scheduling pause between the two clocks' reads widens, not moves, a reading.

    Both clocks advance together on one simulated timeline, one microsecond
    per read.  From the later census on, the process pauses, just before or
    just after reading the realtime clock, for each of ``pauses`` in turn,
    one per clock reading.  A reading a pause widened is the same reading as
    one that saw a step back of the pause's length, so its bounds are kept
    whole.  Pauses of 0.4 seconds that each widen the bounds the same way
    stay under the half-second bound, and the old handle reads as over.  One
    pause of 0.6 seconds passes the bound, and so do two of 0.3 seconds that
    widen the bounds in opposite directions; the step they cannot rule out
    keeps the run alive.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    change = handle.stat().st_ctime_ns
    phase = _track_census_phase(monkeypatch)
    timeline = [0]
    remaining = list(pauses)

    def tick() -> int:
        timeline[0] += 1_000
        return timeline[0]

    def realtime() -> int:
        where, pause = ("", 0)
        if phase.census >= 2 and remaining:
            where, pause = remaining.pop(0)
        if where == "before":
            timeline[0] += pause
        value = change + 10 * _SECOND + tick()
        if where == "after":
            timeline[0] += pause
        return value

    monkeypatch.setattr(wrkslots, "_retained_handle_clock_ns", realtime, raising=False)
    monkeypatch.setattr(
        wrkslots, "_retained_handle_monotonic_ns", lambda: 10**15 + tick(), raising=False
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state, message = states[("testhost", "slot01", 1)]
    assert remaining == []
    assert phase.census == 2
    assert state == expected, message
    if expected == "alive":
        assert "realtime clock stepped back while the host evidence was read" in message


@pytest.mark.parametrize(
    ("monotonic", "realtime", "expected"),
    [
        pytest.param((0, 2), (5,), (5, 3, 5), id="bracketed"),
        pytest.param(
            (0, 600_000_002),
            (10_600_000_001,),
            (10_600_000_001, 9_999_999_999, 10_600_000_001),
            id="paused-before-the-realtime-read",
        ),
        pytest.param(
            (0, 600_000_002),
            (10_000_000_001,),
            (10_000_000_001, 9_399_999_999, 10_000_000_001),
            id="paused-after-the-realtime-read",
        ),
    ],
)
def test_one_clock_reading_brackets_the_realtime_clock(
    monkeypatch: pytest.MonkeyPatch,
    monotonic: tuple[int, ...],
    realtime: tuple[int, ...],
    expected: tuple[int, int, int],
) -> None:
    """One reading reads the realtime clock once, between two monotonic reads.

    The offset bounds are the realtime reading less the later and less the
    earlier monotonic reading.  A 0.6-second pause before the realtime read
    or after it widens the bounds by the pause, upward or downward; no
    further reading narrows them, since a pause together with a step back
    the realtime read saw gives the same three readings.
    """

    monotonic_reads = iter(monotonic)
    realtime_reads = iter(realtime)
    monkeypatch.setattr(
        wrkslots, "_retained_handle_monotonic_ns", lambda: next(monotonic_reads)
    )
    monkeypatch.setattr(wrkslots, "_retained_handle_clock_ns", lambda: next(realtime_reads))

    assert wrkslots._retained_handle_clocks() == expected
    assert next(monotonic_reads, None) is None
    assert next(realtime_reads, None) is None


@pytest.mark.parametrize(
    ("back_to", "change"),
    [
        pytest.param(
            _SECOND // 5,
            "may have been rewritten alike within one file timestamp tick",
            id="back-into-the-window",
        ),
        pytest.param(
            2 * _SECOND + _SECOND // 5,
            "may have been rewritten alike while this host's realtime clock stepped back",
            id="back-short-of-the-window",
        ),
    ],
)
def test_a_step_back_one_reading_saw_and_undone_before_the_next_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, back_to: int, change: str
) -> None:
    """A step back that only one clock reading saw is kept after it is undone.

    Both clocks advance together on one simulated timeline, one microsecond
    per read, with the realtime clock ten seconds after the handle's change
    time.  When the later census first reads the realtime clock, the clock
    has stepped back to ``back_to`` after the change time, as if the run
    registered again alike there; the process then pauses for 0.6 seconds
    before its next monotonic read, and the step is undone before the
    census's reading after its last handle, which is bracketed within a
    microsecond.  ``back-into-the-window`` lands in the handle's timestamp
    window, so the earliest realtime value read finds the handle recent;
    ``back-short-of-the-window`` lands just outside it, so only the bounds
    of the reading that saw the step show that the clock fell.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    stamp = handle.stat().st_ctime_ns
    phase = _track_census_phase(monkeypatch)
    timeline = [0]
    stepped: list[int] = []

    def tick() -> int:
        timeline[0] += 1_000
        return timeline[0]

    def realtime() -> int:
        if phase.census >= 2 and not stepped:
            stepped.append(phase.census)
            value = stamp + back_to + tick()
            timeline[0] += 6 * _SECOND // 10
            return value
        return stamp + 10 * _SECOND + tick()

    monkeypatch.setattr(wrkslots, "_retained_handle_clock_ns", realtime, raising=False)
    monkeypatch.setattr(
        wrkslots, "_retained_handle_monotonic_ns", lambda: 10**15 + tick(), raising=False
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state, message = states[("testhost", "slot01", 1)]
    assert stepped == [2]
    assert state == "alive", message
    assert f"for row slot01 {change} while the host evidence was read" in message
    assert f"its unit {RUN_UNIT} may have been queued" in message


@pytest.mark.parametrize(
    ("script", "later_offset"),
    [
        pytest.param(
            {(2, False): ((3_250_000_000, 2_770_000_000, 3_870_000_000),)},
            0,
            id="sampled-then-undone",
        ),
        pytest.param(
            {(2, False): ((3_250_000_000, 2_770_000_000, 3_870_000_000),)},
            -480_000_000,
            id="sampled-then-partly-undone",
        ),
        pytest.param(
            {(1, True): ((4_200_000_000, 2_200_001_000, 4_200_002_000),)},
            0,
            id="sampled-after-the-earlier-census-read-its-handle",
        ),
    ],
)
def test_a_step_back_any_clock_reading_saw_is_alive(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    script: Mapping[tuple[int, bool], tuple[tuple[int, int, int], ...]],
    later_offset: int,
) -> None:
    """A step back of the realtime clock is kept wherever one clock reading saw it.

    Both clocks advance together, one microsecond per read, from 2.05
    seconds after the handle's change time, so the earlier census reads the
    handle as old.  ``script`` gives clock readings, as monotonic, realtime
    and monotonic values on that timeline, for a census and whether it has
    begun reading its handle; every other read follows the timeline, with
    the realtime clock ``later_offset`` against it from the later census on.
    ``sampled-then-undone`` reads the realtime clock 1.1 seconds behind in
    the later census's first reading, which spans 0.62 seconds, and the step
    is undone before the next.  ``sampled-then-partly-undone`` is the same
    until the clock is stepped 0.62 seconds forward, leaving it 0.48 seconds
    behind for the rest of the census, so a narrower reading would agree
    with the first and show a fall of only 0.48 seconds.
    ``sampled-after-the-earlier-census-read-its-handle`` steps the clock back
    two seconds after the earlier census read its handle, where that
    census's reading after its last handle sees it, and undoes the step
    before the later census.  The earliest realtime value either census
    reads leaves the handle old, so only the step keeps the run alive.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    stamp = handle.stat().st_ctime_ns
    phase = _track_census_phase(monkeypatch)
    timeline = [2 * _SECOND + _SECOND // 20]
    pending = {
        key: [
            (clock, value)
            for before, realtime, after in readings
            for clock, value in (
                ("monotonic", before),
                ("realtime", realtime),
                ("monotonic", after),
            )
        ]
        for key, readings in script.items()
    }

    def read(clock: str) -> int:
        reads = pending.get((phase.census, phase.read))
        if reads:
            scripted, value = reads.pop(0)
            assert scripted == clock, (scripted, clock)
            if clock == "monotonic":
                timeline[0] = max(timeline[0], value)
            return value
        timeline[0] += 1_000
        if clock == "realtime" and phase.census >= 2:
            return timeline[0] + later_offset
        return timeline[0]

    monkeypatch.setattr(
        wrkslots,
        "_retained_handle_clock_ns",
        lambda: stamp + read("realtime"),
        raising=False,
    )
    monkeypatch.setattr(
        wrkslots,
        "_retained_handle_monotonic_ns",
        lambda: 10**15 + read("monotonic"),
        raising=False,
    )
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    state, message = states[("testhost", "slot01", 1)]
    assert all(not reads for reads in pending.values()), pending
    assert phase.census == 2
    assert state == "alive", message
    assert (
        "for row slot01 may have been rewritten alike while this host's realtime "
        "clock stepped back while the host evidence was read"
    ) in message
    assert f"its unit {RUN_UNIT} may have been queued" in message


@pytest.mark.parametrize("change", sorted(_HANDLE_CHANGES))
def test_a_handle_that_changes_while_its_window_is_waited_out_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, change: str
) -> None:
    """A handle rewritten, replaced or removed during the settle wait is a run event.

    The first read finds the handle recent, so the window is waited out and
    the handles read again; the run registers again (or its handle goes)
    during that wait.  The reads after the wait agree with each other and
    find nothing recent, so only the difference from the read before the
    wait shows the event.  The unit is absent from both user-systemd
    enumerations and every process table.
    """

    project, tree = _prepare(tmp_path, monkeypatch, agent_liveness="dead")
    handle = _write_run_handle(project, tree / "before")
    start = handle.stat().st_ctime_ns
    phase = _track_census_phase(monkeypatch)
    # The first census and the settle step read the handle's change time;
    # both later censuses read three seconds past this host's clock, so the
    # handle as the wait left it is not recent however long setup took.
    # The monotonic clock keeps pace.
    monotonics = (0, 3 * _SECOND, 3 * _SECOND + _SECOND // 10)
    waits: list[float] = []

    def realtime() -> int:
        return start if phase.census <= 1 else time.time_ns() + 3 * _SECOND

    def wait(seconds: float) -> None:
        waits.append(seconds)
        _change_run_handle(handle, tree, change)

    monkeypatch.setattr(wrkslots, "_retained_handle_clock_ns", realtime, raising=False)
    monkeypatch.setattr(
        wrkslots,
        "_retained_handle_monotonic_ns",
        lambda: 10**15 + monotonics[min(max(phase.census, 1), 3) - 1],
        raising=False,
    )
    monkeypatch.setattr(time, "sleep", wait)
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: ())
    config = wrkslots._load_config(str(project), "testhost")
    states = wrkslots._validation_run_liveness_states(
        config, wrkslots._load_active(config).slots
    )

    assert len(waits) == 1 and 2.0 < waits[0] < 2.3, waits
    state, message = states[("testhost", "slot01", 1)]
    assert state == "alive", message
    assert (
        f"for row slot01 {_HANDLE_CHANGES[change]} while their timestamp window "
        "was waited out" in message
    )
    assert f"its unit {RUN_UNIT} may have been queued" in message


def test_a_retained_unit_cgroup_member_missing_from_every_table_is_alive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The run started and finished between reads and left a child behind.

    Both enumerations read the unit inactive and unqueued, and neither
    process table holds the child; only its control group does.
    """

    read: list[set[str]] = []

    def members(units: AbstractSet[str], *, root: Path | None = None) -> Mapping[str, int]:
        read.append(set(units))
        return {name: 1 for name in units}

    state, message = _judge_with_unit_enumerations(
        tmp_path, monkeypatch, ((_unit(),), (_unit(),)), members=members
    )

    assert state == "alive", message
    assert f"retained validation unit {RUN_UNIT} control group now holds 1" in message
    assert read == [{RUN_UNIT}]


def test_retained_unit_cgroup_members_counts_the_unit_and_its_descendants(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "cgroup"
    root.mkdir()
    (root / "cgroup.controllers").write_text("cpu memory\n")
    manager = root / "user.slice" / f"user-{os.getuid()}.slice" / f"user@{os.getuid()}.service"
    run = manager / "app.slice" / RUN_UNIT
    (run / "payload").mkdir(parents=True)
    (run / "cgroup.procs").write_text("101\n102\n")
    (run / "payload" / "cgroup.procs").write_text("103\n")
    other = manager / "app.slice" / "other.service"
    other.mkdir()
    (other / "cgroup.procs").write_text("201\n")
    absent = "validate-run-0002.service"
    monkeypatch.setattr(wrkslots, "_user_manager_cgroup", lambda _root: manager)

    assert wrkslots._retained_unit_cgroup_members({RUN_UNIT, absent}, root=root) == {
        RUN_UNIT: 3,
        absent: 0,
    }
    (root / "cgroup.controllers").unlink()
    with pytest.raises(wrkslots.Refusal, match="no cgroup v2 hierarchy"):
        wrkslots._retained_unit_cgroup_members({RUN_UNIT}, root=root)
    assert wrkslots._retained_unit_cgroup_members(set(), root=root) == {}


def test_this_host_user_manager_cgroup_is_read() -> None:
    """On this host the reader finds the manager and a fresh unit is empty."""

    _require_cgroup2_host()
    unit = f"wrkslots-test-{uuid.uuid4().hex}.service"
    assert wrkslots._retained_unit_cgroup_members({unit}) == {unit: 0}
    assert wrkslots._user_manager_cgroup(Path("/sys/fs/cgroup")).name == (
        f"user@{os.getuid()}.service"
    )


def test_an_invisible_user_manager_cgroup_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A user manager missing from the hierarchy is no evidence of no members."""

    root = tmp_path / "cgroup"
    root.mkdir()
    (root / "cgroup.controllers").write_text("cpu memory\n")
    missing = root / "user.slice" / f"user-{os.getuid()}.slice" / f"user@{os.getuid()}.service"
    monkeypatch.setattr(wrkslots, "_user_manager_cgroup", lambda _root: missing)

    with pytest.raises(wrkslots.Refusal, match="service manager control group .* is not visible"):
        wrkslots._retained_unit_cgroup_members({RUN_UNIT}, root=root)


@pytest.mark.parametrize(
    ("mount", "detail"),
    [
        (
            "2 1 0:2 /user.slice /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
            "the cgroup v2 mount at /sys/fs/cgroup shows only /user.slice of the hierarchy",
        ),
        (
            "2 1 0:2 / /sys/fs/cgroup rw - tmpfs tmpfs rw\n",
            "no cgroup v2 hierarchy is mounted at /sys/fs/cgroup",
        ),
        (
            "2 1 0:2 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n"
            "3 2 0:3 / /sys/fs/cgroup rw - tmpfs tmpfs rw\n",
            "no cgroup v2 hierarchy is mounted at /sys/fs/cgroup",
        ),
    ],
)
def test_a_partial_cgroup_mount_refuses_the_member_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, mount: str, detail: str
) -> None:
    """Only a cgroup v2 mount of the whole hierarchy at /sys/fs/cgroup is read.

    A mount of a subtree shows the control groups below it and none
    outside it, and another file system stacked over the hierarchy hides
    it; either way a unit's control group would read as empty.
    """

    table = tmp_path / "mountinfo"
    table.write_text("1 0 0:1 / / rw - ext4 /dev/root rw\n" + mount, encoding="utf-8")
    monkeypatch.setattr(wrkslots, "_SELF_MOUNTINFO", table)

    with pytest.raises(wrkslots.Refusal) as refused:
        wrkslots._retained_unit_cgroup_members({RUN_UNIT})
    assert detail in str(refused.value)


_CGROUP_HIERARCHY = "2 1 0:2 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n"
_RUN_UNIT_CGROUP = (
    f"/sys/fs/cgroup/user.slice/user-{os.getuid()}.slice/user@{os.getuid()}.service"
    f"/app.slice/{RUN_UNIT}"
)


@pytest.mark.parametrize(
    ("mount", "point"),
    [
        (
            "3 2 0:3 / /sys/fs/cgroup/user.slice rw - tmpfs tmpfs rw\n",
            "/sys/fs/cgroup/user.slice",
        ),
        (
            f"3 2 0:3 /empty {_RUN_UNIT_CGROUP}/cgroup.procs rw - tmpfs tmpfs rw\n",
            f"{_RUN_UNIT_CGROUP}/cgroup.procs",
        ),
    ],
    ids=["file-system-over-a-slice", "file-over-a-member-list"],
)
def test_a_visible_mount_below_the_cgroup_hierarchy_refuses_the_member_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, mount: str, point: str
) -> None:
    """A mount below /sys/fs/cgroup replaces the control groups it covers.

    The hierarchy-root proof passes for both tables: the mount at
    /sys/fs/cgroup is the whole cgroup v2 tree.  A file system over a slice
    shows none of its control groups, and an empty file bind-mounted over a
    unit's ``cgroup.procs`` lists no member, so the unit would read empty.
    """

    table = tmp_path / "mountinfo"
    table.write_text(
        "1 0 0:1 / / rw - ext4 /dev/root rw\n" + _CGROUP_HIERARCHY + mount, encoding="utf-8"
    )
    monkeypatch.setattr(wrkslots, "_SELF_MOUNTINFO", table)

    with pytest.raises(wrkslots.Refusal) as refused:
        wrkslots._retained_unit_cgroup_members({RUN_UNIT})
    assert f"a tmpfs mount at {point} covers part of the cgroup v2 hierarchy" in str(
        refused.value
    )


def test_a_hidden_mount_below_the_cgroup_hierarchy_is_not_read_as_a_cover(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A mount that a later whole-hierarchy mount at /sys/fs/cgroup hides
    covers nothing that path lookup reaches, so the hierarchy is whole."""

    covered = (
        "1 0 0:1 / / rw - ext4 /dev/root rw\n"
        + _CGROUP_HIERARCHY
        + "3 2 0:3 / /sys/fs/cgroup/user.slice rw - tmpfs tmpfs rw\n"
    )
    table = tmp_path / "mountinfo"
    monkeypatch.setattr(wrkslots, "_SELF_MOUNTINFO", table)
    table.write_text(covered, encoding="utf-8")
    with pytest.raises(wrkslots.Refusal, match="covers part of the cgroup v2 hierarchy"):
        wrkslots._assert_whole_cgroup2_hierarchy(Path("/sys/fs/cgroup"))

    table.write_text(
        covered + "4 2 0:2 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n", encoding="utf-8"
    )
    wrkslots._assert_whole_cgroup2_hierarchy(Path("/sys/fs/cgroup"))


@pytest.mark.parametrize(
    ("table", "check"),
    [
        (
            "2 1 0:2 / /proc rw - proc proc rw\n"
            "3 2 0:3 / /proc/4242 rw - tmpfs tmpfs rw\n"
            "4 2 0:2 / /proc rw - proc proc rw\n",
            "process-view",
        ),
        (
            "2 1 0:2 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n"
            "3 2 0:3 / /sys/fs/cgroup/user.slice rw - tmpfs tmpfs rw\n"
            "4 2 0:2 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
            "cgroup-hierarchy",
        ),
    ],
)
def test_a_hidden_mount_is_not_a_cover_where_the_root_is_not_a_mount_point(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, table: str, check: str
) -> None:
    """A process whose root is not a mount point sees no mount at "/", and
    the mounts on the unlisted mount above the table are where lookup
    starts.  Mount 4, stacked over the same point later, hides mount 3, so
    neither proof reads mount 3 as a cover; without mount 4 both refuse.
    """

    path = tmp_path / "mountinfo"
    monkeypatch.setattr(wrkslots, "_SELF_MOUNTINFO", path)
    initial = dict(wrkslots._INITIAL_NAMESPACE_INODES)
    monkeypatch.setattr(wrkslots, "_namespace_inode", lambda name: initial[name])

    def prove() -> None:
        if check == "process-view":
            wrkslots._assert_host_process_view()
        else:
            wrkslots._assert_whole_cgroup2_hierarchy(Path("/sys/fs/cgroup"))

    covered = "".join(table.splitlines(keepends=True)[:2])
    path.write_text(covered, encoding="utf-8")
    with pytest.raises(wrkslots.Refusal, match="masks|covers part"):
        prove()
    path.write_text(table, encoding="utf-8")
    prove()
    entries = wrkslots._mount_entries(table.encode(), "table")
    hidden = wrkslots._visible_mount(entries, entries[1].point)
    assert hidden is not None and hidden.mount_id == 4


@pytest.mark.parametrize("check", ["process-view", "cgroup-hierarchy"])
def test_finding_the_visible_mounts_of_a_large_table_is_linear(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, check: str
) -> None:
    """3,000 mounts below /proc or /sys/fs/cgroup, all hidden by a later
    mount stacked over that point, are checked in one pass over the table.

    Finding each entry's visible mount anew rebuilt the whole mount graph
    per entry, so this table (about 160 KB, inside the 4 MiB bound) took
    31 seconds; the bound here leaves room for a loaded host.
    """

    point = "/proc" if check == "process-view" else "/sys/fs/cgroup"
    fstype = "proc" if check == "process-view" else "cgroup2"
    lines = ["1 0 0:1 / / rw - ext4 /dev/root rw\n", f"2 1 0:2 / {point} rw - {fstype} x rw\n"]
    lines.extend(
        f"{10 + index} 2 0:3 / {point}/{100000 + index} rw - tmpfs tmpfs rw\n"
        for index in range(3_000)
    )
    lines.append(f"9999 2 0:2 / {point} rw - {fstype} x rw\n")
    path = tmp_path / "mountinfo"
    path.write_text("".join(lines), encoding="utf-8")
    monkeypatch.setattr(wrkslots, "_SELF_MOUNTINFO", path)
    initial = dict(wrkslots._INITIAL_NAMESPACE_INODES)
    monkeypatch.setattr(wrkslots, "_namespace_inode", lambda name: initial[name])

    started = time.monotonic()
    if check == "process-view":
        wrkslots._assert_host_process_view()
    else:
        wrkslots._assert_whole_cgroup2_hierarchy(Path(point))
    assert time.monotonic() - started < 5.0


def test_this_host_counts_the_member_of_this_process_unit() -> None:
    """The real read finds this test process in its own unit's control group.

    The unit is the deepest ``.scope`` or ``.service`` part of this
    process's control group below its user manager, so a reader that
    counted nothing would fail here.
    """

    _require_cgroup2_host()
    if os.stat("/proc/self/ns/cgroup").st_ino != dict(wrkslots._INITIAL_NAMESPACE_INODES)[
        "cgroup"
    ]:
        pytest.skip(
            "this process is not in the initial cgroup namespace, so its control-group "
            "path is not a path in the host hierarchy"
        )
    path = next(
        line.split(":", 2)[2]
        for line in Path("/proc/self/cgroup").read_text(encoding="utf-8").splitlines()
        if line.startswith("0::")
    )
    parts = Path(path).parts
    manager = f"user@{os.getuid()}.service"
    if manager not in parts:
        pytest.skip(
            f"this process's control group is not below {manager}, so no unit of the "
            "user manager holds it"
        )
    units = [
        part
        for part in parts[parts.index(manager) + 1 :]
        if part.endswith((".scope", ".service"))
    ]
    if not units:
        pytest.skip(f"this process's control group names no unit below {manager}")

    members = wrkslots._retained_unit_cgroup_members({units[-1]})

    assert members[units[-1]] >= 1, (units[-1], members)


def _restrict_pid_namespace(monkeypatch: pytest.MonkeyPatch) -> None:
    """This process is in a child PID namespace."""

    real = wrkslots._namespace_inode
    monkeypatch.setattr(
        wrkslots,
        "_namespace_inode",
        lambda name: 4_026_532_999 if name == "pid" else real(name),
    )


def _prepare_absent_validate_recovery(
    project: Path, repository: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[Path, Path]:
    """One absent validation row with a retained run handle, and its input."""

    record = prepare_absent_validate_row(project, repository, slot="gone", agent="validate-a")
    config = wrkslots._load_config(str(project), "testhost")
    _write_run_handle(
        project, wrkslots._stored_path(config, record.checkouts[0].path, "checkout")
    )
    input_path = write_absent_validate_input(project, [record])
    allow_test_host_for_absent_validate_recovery(project, monkeypatch)
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (_unit(),))
    monkeypatch.setattr(wrkslots, "_retained_unit_cgroup_members", _no_members)
    return project, input_path


def _one_member(units: AbstractSet[str], *, root: Path | None = None) -> Mapping[str, int]:
    return dict.fromkeys(units, 1)


_RECOVERY_EVIDENCE = ["restricted-view", "cgroup-member", "rewritten-handle"]


def _late_recovery_evidence(
    project: Path, tree: Path, monkeypatch: pytest.MonkeyPatch, evidence: str
) -> str:
    """Make the host evidence restricted or late; return the expected refusal."""

    if evidence == "restricted-view":
        _restrict_pid_namespace(monkeypatch)
        return "not in the host's initial pid namespace"
    if evidence == "cgroup-member":
        monkeypatch.setattr(wrkslots, "_retained_unit_cgroup_members", _one_member)
        return f"retained validation unit {RUN_UNIT} control group now holds 1"
    handle = project / "ignored" / "validate" / "runs" / (
        RUN_UNIT.removesuffix(".service") + ".json"
    )
    calls = 0

    def units() -> tuple[Mapping[str, str], ...]:
        nonlocal calls
        if calls == 0:
            _change_run_handle(handle, tree, "rewritten")
        calls += 1
        return (_unit(),)

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)
    return "changed while the host evidence was read"


@pytest.mark.parametrize("evidence", _RECOVERY_EVIDENCE)
def test_absent_validate_row_recovery_reads_the_same_late_evidence(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    evidence: str,
) -> None:
    """recover-absent-validate-rows proves the host view and reads members.

    In a child PID namespace a run is missing from the process table, and a
    run's child can be missing from every table but still in its control
    group.  Either way the empty tables are no evidence that the row is free.
    A run handle rewritten while the host is read is a run registered again,
    whose unit may be queued after the enumerations.
    """

    project, repository, _remote = make_project(tmp_path)
    project, input_path = _prepare_absent_validate_recovery(project, repository, monkeypatch)
    config = wrkslots._load_config(str(project), "testhost")
    record = next(
        record for record in wrkslots._load_active(config).slots if record.slot == "gone"
    )
    tree = wrkslots._stored_path(config, record.checkouts[0].path, "checkout")
    expected = _late_recovery_evidence(project, tree, monkeypatch, evidence)

    assert run_absent_validate_recovery(project, input_path, apply=False) == 3
    assert expected in capsys.readouterr().err

    monkeypatch.undo()
    _prepare_absent_validate_recovery_host(project, monkeypatch)
    assert run_absent_validate_recovery(project, input_path, apply=False) == 0, (
        capsys.readouterr().err
    )


def _prepare_absent_validate_recovery_host(
    project: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    allow_test_host_for_absent_validate_recovery(project, monkeypatch)
    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", lambda: (_unit(),))
    monkeypatch.setattr(wrkslots, "_retained_unit_cgroup_members", _no_members)


@pytest.mark.parametrize("evidence", _RECOVERY_EVIDENCE)
def test_absent_agent_row_recovery_reads_the_same_late_evidence(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    evidence: str,
) -> None:
    """recover-absent-agent-row reads the evidence through the same helper.

    A validation run may use an agent checkout as well, so a retained run
    handle naming the agent row's checkout is judged as for a validation row.
    """

    project, repository, _remote = make_project(tmp_path)
    record = prepare_absent_agent_row(project, repository)
    config = wrkslots._load_config(str(project), "testhost")
    tree = wrkslots._stored_path(config, record.checkouts[0].path, "checkout")
    _write_run_handle(project, tree)
    _prepare_absent_validate_recovery_host(project, monkeypatch)
    expected = _late_recovery_evidence(project, tree, monkeypatch, evidence)

    assert run_absent_agent_recovery(project, record, apply=False) == 3
    assert expected in capsys.readouterr().err

    monkeypatch.undo()
    _prepare_absent_validate_recovery_host(project, monkeypatch)
    assert run_absent_agent_recovery(project, record, apply=False) == 0, (
        capsys.readouterr().err
    )


def test_deferred_batch_completion_judges_the_fenced_checkout(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A batch completes each removal after a census outside the locks.

    ``remove-validate-batch`` fences the slot to ``.slot01.fenced.1.<hex>``,
    takes a fresh census of the fence outside the mutation locks, and then
    completes the removal under them (``_complete_prepared_private_finish``).
    A job queued against the fenced checkout in that gap names no recorded
    path, and the completion deletes the files it will use.
    """

    project, _repository, _remote = make_project(tmp_path)
    slot_path = prepare_dead_validate_slots(project, ("slot01",))["slot01"]
    tree = checkout(project, slot="slot01", slot_type="validate")
    stub_validate_batch_censuses(monkeypatch)
    monkeypatch.setattr(wrkslots, "_assert_slot_unused", lambda *_a, **_k: None)
    monkeypatch.setattr(wrkslots, "_absent_validate_process_snapshot", lambda **_k: ())
    monkeypatch.setattr(wrkslots, "_retained_unit_cgroup_members", _no_members, raising=False)
    boundaries: list[str] = []
    monkeypatch.setattr(wrkslots, "_interrupt_for_test", boundaries.append)
    queue = True

    def units() -> tuple[Mapping[str, str], ...]:
        fences = sorted(slot_path.parent.glob(".slot01.fenced.1.*"))
        if not queue or "after-validate-batch-fresh-census" not in boundaries:
            return ()
        assert len(fences) == 1, fences
        fenced_tree = fences[0] / tree.relative_to(slot_path)
        return (
            _unit(
                Id="queued-run.service",
                PendingJob="yes",
                ExecStart=f"/usr/bin/make\n-C\n{fenced_tree}",
            ),
        )

    monkeypatch.setattr(wrkslots, "_user_systemd_snapshot", units)

    assert _remove_batch(project) == 1
    output = capsys.readouterr().out
    assert "after-validate-batch-fresh-census" in boundaries
    assert "RETAINED: slot01" in output
    assert "user-systemd unit queued-run.service names validation row slot01" in output
    _assert_retained(project, tree)

    queue = False
    boundaries.clear()
    assert _remove_batch(project) == 0, capsys.readouterr()
    assert "after-validate-batch-fresh-census" in boundaries
    assert not slot_path.exists()
    assert active_slots(project) == []
