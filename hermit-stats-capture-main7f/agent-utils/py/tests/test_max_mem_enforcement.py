"""``--max-mem`` is a containment limit, not only a sizing input.

``dagrun run --max-mem 20G`` used to feed one thing: the modelled active-step
ceiling.  The outer systemd scope was still brought up with 90% of ``MemAvailable``, so on a
128 GiB host a run that announced a 20 GiB budget could grow to ~115 GiB before anything stopped
it.  "Two validates with 20 GiB each" was then true of the arithmetic and of nothing on the
machine — which is the whole reason a share of a host is asked for.

These tests bracket the wiring in both directions: the request must reach the scope, and it must
be unable to WIDEN the boundary it is combined with.  A one-sided test would pass on a change
that simply replaced the derived boundary with whatever the caller asked for.

Not covered here, and deliberately: runtime memory ADMISSION (serialising two individually
fitting steps whose sum exceeds the budget) is unimplemented — the scheduler still has no
memory-budget state.  See #33 max-mem-enforcement.
"""

from __future__ import annotations

import argparse
import inspect
from collections.abc import Sequence

import pytest

from dagrun import cgroup as cg
from dagrun import cli


def test_a_max_mem_spec_becomes_a_positive_byte_ceiling() -> None:
    assert cli._requested_max_mem_bytes("20G") == 20 * 1024**3
    assert cli._requested_max_mem_bytes("1024") == 1024


def test_an_absent_unparseable_or_nonpositive_max_mem_is_not_an_outer_ceiling() -> None:
    # A bad spec is reported by _select_max_steps and the run exits 2 there; making scope
    # bring-up refuse it too would give one typo two different exit codes depending on whether
    # boxing was attempted.  "0" is in this list, not in the refusing one: see the end-to-end
    # test below for what a caller actually gets.
    assert cli._requested_max_mem_bytes(None) is None
    assert cli._requested_max_mem_bytes("") is None
    assert cli._requested_max_mem_bytes("twenty gigs") is None
    assert cli._requested_max_mem_bytes("0") is None


def test_a_nonpositive_max_mem_is_refused_by_name_and_no_step_runs(
    tmp_path: object, capsys: pytest.CaptureFixture[str]
) -> None:
    """WHAT ``--max-mem 0`` ACTUALLY DOES, end to end.

    The ceiling helper refuses a non-positive request, but the CLI never hands it one: the flag
    is dropped at scope bring-up and refused by ``_select_max_steps`` instead, so that one bad
    spec has ONE exit path.  That is worth an executable check rather than a comment, because
    the comment that used to describe this said the opposite ("a non-positive request is
    REFUSED, not dropped" — at the CLI it is dropped, and then the run is refused for a
    different reason).

    The two properties that actually matter to a caller are asserted: the run is REFUSED by
    name, and it refuses BEFORE running anything — a budget of 0 must never be quietly upgraded
    to 90% of the host and then executed.
    """
    import pathlib

    directory = pathlib.Path(str(tmp_path))
    marker = directory / "ran"
    dag = directory / "ok.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "ok", "cmd": "touch %s"}]}' % marker
    )

    code = cli.main(
        [
            "run",
            "--dag",
            str(dag),
            "-q",
            "--no-profile",
            "--allow-cgroup-failure",
            "--max-mem",
            "0",
        ]
    )
    err = capsys.readouterr().err
    assert code == 2, err
    assert "--max-mem 0: REFUSED" in err, err
    assert not marker.exists(), "the run must refuse before any step executes"


def _capture_reexec(
    monkeypatch: pytest.MonkeyPatch, available: int
) -> list[int | None]:
    """Stub scope bring-up and record the ``memory_max`` the re-exec was asked for."""
    seen: list[int | None] = []

    def fake_reexec(
        argv: Sequence[str],
        *,
        memory_max: int | None,
        cpu_count: int | None = None,
        naming: object = None,
        use_aggregate_slice: bool = True,
        skip_in_ci: bool = True,
        runtime_max_s: int | None = None,
    ) -> bool:
        seen.append(memory_max)
        return False  # never re-exec under test; the caller then reports and returns nonzero

    monkeypatch.delenv("DAGRUN_IN_SCOPE", raising=False)
    # This unit test exercises the *outer scope setup* branch even when the whole pytest shard is
    # safely hosted as an opted-in delegated child of the validation DAG. Removing only the
    # delegation capability for this direct function call does not move or escape the test
    # process from its real outer cgroup.
    monkeypatch.delenv(cg.DELEGATED_CGROUP_ENV, raising=False)
    monkeypatch.delenv(cg.DELEGATED_UNBOXED_ENV, raising=False)
    monkeypatch.delenv("DAGRUN_OUTER_RUN", raising=False)
    monkeypatch.delenv(cg.OUTER_MEMORY_MAX_ENV, raising=False)
    monkeypatch.setattr(cg, "mem_available_bytes", lambda: available)
    monkeypatch.setattr(cg, "reexec_in_scope", fake_reexec)
    monkeypatch.setattr(cg, "policy_skip_reason", lambda **_kwargs: None)
    return seen


def test_max_mem_is_the_memory_max_the_outer_scope_is_created_with(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen = _capture_reexec(monkeypatch, available=100_000_000)
    manager, code = cli._resolve_cgroup_manager(
        False, False, max_cpus=2, run_timeout_s=None, max_mem_bytes=20_000_000
    )
    assert manager is None and code == 3  # the stub refuses to exec; that is not what we assert
    assert seen == [20_000_000], "the requested ceiling never reached the scope"


def test_a_max_mem_larger_than_the_host_boundary_cannot_widen_the_scope(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen = _capture_reexec(monkeypatch, available=100_000_000)
    cli._resolve_cgroup_manager(
        False, False, max_cpus=2, run_timeout_s=None, max_mem_bytes=10**12
    )
    # 90% of MemAvailable, not the trillion bytes asked for.
    assert seen == [90_000_000]


def test_without_max_mem_the_scope_ceiling_is_unchanged(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen = _capture_reexec(monkeypatch, available=100_000_000)
    cli._resolve_cgroup_manager(False, False, max_cpus=2, run_timeout_s=None)
    assert seen == [90_000_000]


def test_the_binding_ceiling_is_named_on_stderr(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    _capture_reexec(monkeypatch, available=100_000_000)
    cli._resolve_cgroup_manager(
        False, False, max_cpus=2, run_timeout_s=None, max_mem_bytes=20_000_000
    )
    bound = capsys.readouterr().err
    assert "--max-mem is the outer scope ceiling: MemoryMax=20000000 bytes." in bound

    _capture_reexec(monkeypatch, available=100_000_000)
    cli._resolve_cgroup_manager(
        False, False, max_cpus=2, run_timeout_s=None, max_mem_bytes=10**12
    )
    unbound = capsys.readouterr().err
    assert "did not bind" in unbound
    assert "MemoryMax=90000000 bytes." in unbound


def test_run_threads_the_parsed_flag_through_to_scope_bring_up(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The flag is read off the parsed namespace by the same accessor the run command uses, so a
    # renamed argparse dest cannot silently stop feeding the scope.
    ns = argparse.Namespace(max_mem="8G")
    assert cli._requested_max_mem_bytes(getattr(ns, "max_mem", None)) == 8 * 1024**3
    missing = argparse.Namespace()
    assert cli._requested_max_mem_bytes(getattr(missing, "max_mem", None)) is None


def test_the_run_command_really_hands_the_flag_to_scope_bring_up(
    monkeypatch: pytest.MonkeyPatch, tmp_path: object
) -> None:
    """The WIRING, not either end of it.

    ``_requested_max_mem_bytes`` can be right and ``_resolve_cgroup_manager`` can be right while
    ``run`` never joins them -- which is exactly the state this issue found, with a correct-looking
    budget on one side and a 90%-of-host scope on the other. So drive the real command.

    Whether a scope can actually be created here is irrelevant and deliberately not asserted: the
    ceiling is chosen before the re-exec is attempted.
    """
    import pathlib

    seen = _capture_reexec(monkeypatch, available=100_000_000)
    dag = pathlib.Path(str(tmp_path)) / "ok.json"
    dag.write_text('{"steps": [{"group": "g", "job": "ok", "cmd": "true"}]}')

    cli.main(["run", "--dag", str(dag), "-q", "--no-profile", "--max-mem", "1M"])
    assert seen == [1024 * 1024], "run did not pass --max-mem to scope bring-up"

    seen.clear()
    cli.main(["run", "--dag", str(dag), "-q", "--no-profile"])
    assert seen == [90_000_000], "a run with no --max-mem must keep the derived boundary"


def test_signature_keeps_max_mem_optional() -> None:
    # Every other caller (sweep, the stdin pre-flight) passes no budget and must keep the
    # previous ceiling; an accidentally required parameter would break them loudly, but a
    # renamed keyword would not.
    signature = inspect.signature(cli._resolve_cgroup_manager)
    parameter = signature.parameters["max_mem_bytes"]
    assert parameter.default is None
