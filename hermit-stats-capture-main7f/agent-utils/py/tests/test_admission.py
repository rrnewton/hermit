"""Host-wide memory admission: grant, queue, or refuse instead of silently contending.

#78 host-memory-admission. ``--max-mem`` gates one process against a snapshot of the host; it has
no notion of what other runner invocations have already committed to, so two boxes started a
second apart each see the same headroom and both take it.
"""

from __future__ import annotations

import json
import os
import threading
from pathlib import Path

import pytest

from dagrun import admission, cgroup
from dagrun.admission import (
    MEM_BUDGET_BYTES_ENV,
    MEM_HEADROOM_BYTES_ENV,
    Verdict,
    held_bytes,
    host_budget_bytes,
    live_headroom_bytes,
    request_with_limits,
)

_QUIET_HOST = 1_000_000_000


def _ledger(tmp_path: Path) -> Path:
    return tmp_path / "memory-admissions.json"


def test_a_request_larger_than_the_host_budget_is_refused_and_names_the_number(
    tmp_path: Path,
) -> None:
    """REFUSE, not QUEUE: no holder finishing ever makes the host bigger.

    A run told to WAIT here would wait forever for something that cannot happen, which is the
    hang this three-way verdict exists to prevent.
    """
    decision, held = request_with_limits(
        2_000_000,
        tag="t",
        ledger=_ledger(tmp_path),
        budget=1_000_000,
        headroom=1_000_000,
    )
    assert held is None
    assert decision.verdict is Verdict.REFUSE
    assert decision.largest_grantable_bytes == 1_000_000
    assert "1000000 bytes" in decision.reason, (
        "a refusal must give the operator a number to type instead of a closed door"
    )


def test_a_second_request_that_no_longer_fits_is_queued_behind_the_first(
    tmp_path: Path,
) -> None:
    ledger = _ledger(tmp_path)
    first, held = request_with_limits(
        700, tag="a", ledger=ledger, budget=1000, headroom=_QUIET_HOST
    )
    assert first.verdict is Verdict.GRANT
    assert held is not None

    second, none = request_with_limits(
        700, tag="b", ledger=ledger, budget=1000, headroom=_QUIET_HOST
    )
    assert none is None
    assert second.verdict is Verdict.QUEUE
    assert second.reserved_bytes == 700
    assert second.holders_ahead == 1, "one holder releasing is enough, and it must say so"
    assert "QUEUED on OTHER RUNS" in second.reason

    # If releasing the first does not make the same request grant, QUEUE was a lie.
    held.release()
    third, granted = request_with_limits(
        700, tag="b", ledger=ledger, budget=1000, headroom=_QUIET_HOST
    )
    assert third.verdict is Verdict.GRANT, third.reason
    assert granted is not None
    granted.release()


def test_host_memory_held_outside_the_tool_queues_on_a_named_different_cause(
    tmp_path: Path,
) -> None:
    """The ledger cannot see a non-runner tenant; the live reading can.

    The two QUEUE causes need different remedies -- one waits for a peer run, the other waits for
    something this tool does not manage -- so they must not read the same.
    """
    decision, held = request_with_limits(
        500_000, tag="t", ledger=_ledger(tmp_path), budget=_QUIET_HOST, headroom=1000
    )
    assert held is None
    assert decision.verdict is Verdict.QUEUE
    assert "HOST MEMORY held outside this tool" in decision.reason
    assert decision.holders_ahead == 0, (
        "no ledger holder is in the way, so promising that one will free it would be false"
    )


def test_a_dead_holders_reservation_is_reclaimed_rather_than_subtracted_forever(
    tmp_path: Path,
) -> None:
    ledger = _ledger(tmp_path)
    # A record whose holder cannot be alive. The (pid, starttime) fingerprint makes even a
    # recycled PID safe, so a sweep can never free a live peer by accident.
    ledger.write_text(
        json.dumps(
            {
                "admissions": [
                    {
                        "pid": (1 << 32) - 1,
                        "starttime": 1,
                        "bytes": 900,
                        "tag": "crashed",
                        "ts": 1.0,
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    decision, held = request_with_limits(
        700, tag="t", ledger=ledger, budget=1000, headroom=_QUIET_HOST
    )
    assert decision.verdict is Verdict.GRANT, decision.reason
    assert decision.reserved_bytes == 0
    assert held is not None
    held.release()


def test_releasing_is_idempotent_and_does_not_free_a_peers_reservation(
    tmp_path: Path,
) -> None:
    ledger = _ledger(tmp_path)
    _a, held_a = request_with_limits(
        1000, tag="a", ledger=ledger, budget=10_000, headroom=_QUIET_HOST
    )
    _b, held_b = request_with_limits(
        2000, tag="b", ledger=ledger, budget=10_000, headroom=_QUIET_HOST
    )
    assert held_a is not None and held_b is not None
    assert held_bytes(ledger) == 3000
    held_a.release()
    held_a.release()
    assert held_bytes(ledger) == 2000, (
        "releasing twice must not take a peer's reservation with it"
    )
    held_b.release()


def test_two_concurrent_requests_that_only_one_can_have_do_not_both_grant(
    tmp_path: Path,
) -> None:
    """The defect this module exists to remove, reproduced against the module itself.

    Eight threads ask for 700 bytes of a 1000-byte budget at the same instant. Exactly one may
    win: more than one means the exclusive lock did not cover the whole read-decide-record
    section, which is precisely how two runs a second apart each took the same headroom.
    """
    ledger = _ledger(tmp_path)
    start = threading.Barrier(8)
    granted: list[object] = []
    lock = threading.Lock()

    def ask(index: int) -> None:
        start.wait()
        decision, held = request_with_limits(
            700, tag=f"t{index}", ledger=ledger, budget=1000, headroom=_QUIET_HOST
        )
        if decision.verdict is Verdict.GRANT:
            with lock:
                # HOLD it: releasing here would let a peer grant and hide the overcommit.
                granted.append(held)

    threads = [threading.Thread(target=ask, args=(i,)) for i in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=30)
        assert not thread.is_alive()

    assert len(granted) == 1, (
        f"a 1000-byte budget holds exactly one 700-byte reservation; {len(granted)} grants means "
        "the lock did not cover the read-decide-record section"
    )
    assert held_bytes(ledger) == 700


def test_the_grant_is_recorded_where_a_peer_process_can_see_it(tmp_path: Path) -> None:
    """A ledger nobody else reads is not shared state. Pin the on-disk schema."""
    ledger = _ledger(tmp_path)
    _decision, held = request_with_limits(
        4096, tag="run", ledger=ledger, budget=_QUIET_HOST, headroom=_QUIET_HOST
    )
    assert held is not None
    payload = json.loads(ledger.read_text(encoding="utf-8"))
    assert list(payload) == ["admissions"]
    (record,) = payload["admissions"]
    assert record["bytes"] == 4096
    assert record["tag"] == "run"
    assert record["pid"] == os.getpid()
    assert isinstance(record["starttime"], int)
    held.release()
    assert json.loads(ledger.read_text(encoding="utf-8"))["admissions"] == []


def test_the_default_budget_keeps_back_a_fraction_and_a_flat_margin(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Planning to the last byte is planning for the OOM killer to arbitrate."""
    monkeypatch.delenv(MEM_BUDGET_BYTES_ENV, raising=False)
    monkeypatch.setattr(
        admission, "_meminfo_bytes", lambda key: 64 * 1024**3 if key == "MemTotal" else None
    )
    # 85% of 64 GiB, less the flat 8 GiB margin.
    assert host_budget_bytes() == int(64 * 1024**3 * 0.85) - 8 * 1024**3
    assert host_budget_bytes() == 49821620633


def test_the_flat_margin_never_swallows_a_small_hosts_entire_budget(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An uncapped 8 GiB margin is a gate that never opens on an 8 GiB host.

    It would leave an aggregate budget of exactly zero -- every run REFUSED and advised to "ask
    for at most 0 B" -- and a live headroom of zero as well, so nothing could queue its way in
    either. Capping the margin at one eighth keeps the budget a positive share of the host at
    every size.
    """
    monkeypatch.delenv(MEM_BUDGET_BYTES_ENV, raising=False)
    monkeypatch.delenv(MEM_HEADROOM_BYTES_ENV, raising=False)
    monkeypatch.setattr(admission, "_meminfo_bytes", lambda key: 8 * 1024**3)
    # 85% of 8 GiB, less one eighth of 8 GiB rather than the whole 8 GiB.
    assert host_budget_bytes() == 6227702579
    assert live_headroom_bytes() == 8 * 1024**3 - 1024**3
    assert live_headroom_bytes() == 7516192768


def test_an_unmeasurable_host_is_unknown_and_not_zero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Reading an absent ``/proc/meminfo`` as 0 would refuse every run on that host."""
    monkeypatch.delenv(MEM_BUDGET_BYTES_ENV, raising=False)
    monkeypatch.delenv(MEM_HEADROOM_BYTES_ENV, raising=False)
    monkeypatch.setattr(admission, "_meminfo_bytes", lambda key: None)
    assert host_budget_bytes() is None
    assert live_headroom_bytes() is None


@pytest.mark.parametrize("failure", [PermissionError("denied"), b"malformed"])
def test_holder_identity_uncertainty_refuses_to_reclaim_memory(
    monkeypatch: pytest.MonkeyPatch, failure: BaseException | bytes,
) -> None:
    """Only positive procfs disappearance may release another run's reservation."""

    def read_bytes(_path: Path) -> bytes:
        if isinstance(failure, BaseException):
            raise failure
        return failure

    monkeypatch.setattr(Path, "read_bytes", read_bytes)
    record = admission._Record(pid=4242, starttime=7, bytes_=1024, tag="live", ts=1.0)
    with pytest.raises(admission.AdmissionStateError, match="process 4242"):
        admission._sweep([record])


def test_disappeared_holder_can_be_reclaimed_from_memory_ledger(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """ENOENT remains positive evidence that the recorded process is gone."""

    def disappeared(_path: Path) -> bytes:
        raise FileNotFoundError("exited during procfs scan")

    monkeypatch.setattr(Path, "read_bytes", disappeared)
    record = admission._Record(pid=4242, starttime=7, bytes_=1024, tag="dead", ts=1.0)
    assert admission._sweep([record]) == ([], [record])


def test_an_unknown_host_gates_on_nothing_rather_than_refusing_everything(
    tmp_path: Path,
) -> None:
    """With no measurable limits the only honest answer is to let the run proceed.

    Refusing would make an unmeasurable host unusable; queueing would hang it. Not gating is a
    visible non-decision, and the run's own ``--max-mem`` box still applies.
    """
    decision, held = request_with_limits(
        1 << 40, tag="t", ledger=_ledger(tmp_path), budget=None, headroom=None
    )
    assert decision.verdict is Verdict.GRANT
    assert decision.budget_bytes is None
    assert held is not None
    held.release()


def test_an_unparseable_budget_override_is_reported_and_ignored(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A misread override that quietly became 0 would refuse every run on the host."""
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, "loads")
    monkeypatch.setattr(
        admission, "_meminfo_bytes", lambda key: 64 * 1024**3 if key == "MemTotal" else None
    )
    assert host_budget_bytes() == int(64 * 1024**3 * 0.85) - 8 * 1024**3
    out = capsys.readouterr().out
    assert MEM_BUDGET_BYTES_ENV in out and "WARNING" in out


def test_admit_returns_the_queue_decision_when_the_wait_runs_out(tmp_path: Path) -> None:
    """A bounded wait must END, and must hand back the reason it gave up."""
    ledger = _ledger(tmp_path)
    _first, held = request_with_limits(
        700, tag="a", ledger=ledger, budget=1000, headroom=_QUIET_HOST
    )
    assert held is not None
    # Point the live readings at the same tight numbers the first grant used.
    os.environ[MEM_BUDGET_BYTES_ENV] = "1000"
    os.environ[MEM_HEADROOM_BYTES_ENV] = str(_QUIET_HOST)
    try:
        decision, granted = admission.admit(
            700, tag="b", ledger=ledger, poll_s=0.05, wait_s=0.2, announce=False
        )
    finally:
        os.environ.pop(MEM_BUDGET_BYTES_ENV, None)
        os.environ.pop(MEM_HEADROOM_BYTES_ENV, None)
    assert granted is None
    assert decision.verdict is Verdict.QUEUE
    held.release()


def _tiny_dag(tmp_path: Path) -> Path:
    dag = tmp_path / "dag.json"
    dag.write_text(
        json.dumps(
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "ok",
                        "cmd": "true",
                        "timeout": 30,
                        "cpu_timeout": 600,
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    return dag


def test_run_admission_refuses_a_request_bigger_than_the_host_and_exits_four(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Exit 4 is its own code: a retrying scheduler must not have to parse prose.

    2 already means "bad usage" and 3 means "cgroup boxing unavailable"; reusing either would
    make "the host is busy, come back" indistinguishable from "this invocation is wrong".
    """
    from dagrun import cli

    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(_ledger(tmp_path)))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, str(1024 * 1024))
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(_QUIET_HOST))
    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--max-mem",
            "8G",
            "--admission",
        ]
    )
    assert code == cli.ADMISSION_EXIT_CODE
    err = capsys.readouterr().err
    assert "REFUSED" in err
    assert "Ask for at most" in err


def test_run_admission_queues_behind_a_live_holder_rather_than_contending(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The defect in one line: without the ledger this second run would simply start."""
    from dagrun import cli

    # This invocation is deliberately testing admission BEFORE entering a run scope.  A full
    # repository validation executes pytest as a child of an outer dagrun scope, so without this
    # isolation the inherited marker makes the same-process fixture below look like the CLI's own
    # pre-exec reservation.  That correctly skips duplicate admission, but tests the wrong path
    # and reaches the unrelated 8-GiB modeled-footprint floor for ``--max-mem 700``.
    monkeypatch.delenv(cgroup.DEFAULT_NAMING.env_in_scope, raising=False)
    ledger = _ledger(tmp_path)
    _decision, held = request_with_limits(
        700, tag="peer", ledger=ledger, budget=1000, headroom=_QUIET_HOST
    )
    assert held is not None
    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(ledger))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, "1000")
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(_QUIET_HOST))
    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--max-mem",
            "700",
            "--admission",
        ]
    )
    assert code == cli.ADMISSION_EXIT_CODE
    err = capsys.readouterr().err
    assert "QUEUED on OTHER RUNS" in err
    assert "--admission SECONDS to wait" in err
    held.release()


def test_run_admission_grants_and_the_run_proceeds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Admission must not become a gate that never opens."""
    from dagrun import cli

    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(_ledger(tmp_path)))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, str(64 * 1024**3))
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(64 * 1024**3))
    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            # Above the modelled minimum runnable footprint, so the EXISTING --max-mem sizing
            # refusal cannot be what this test is measuring.
            "--max-mem",
            "16G",
            "--admission",
        ]
    )
    captured = capsys.readouterr()
    assert code == 0, captured.out + captured.err
    assert "GRANTED" in captured.out
    assert "16.0 GiB" in captured.out


def test_run_admission_without_max_mem_is_refused_rather_than_guessed(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Guessing would reserve the whole host and turn admission into a global mutex."""
    from dagrun import cli

    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--admission",
        ]
    )
    assert code == 2
    assert "--admission requires --max-mem" in capsys.readouterr().err


def test_the_boxing_re_exec_does_not_make_a_run_queue_behind_its_own_reservation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A boxed run must not wait for itself.

    Cgroup boxing re-execs the runner into a systemd scope with ``execvp``, which keeps BOTH the
    pid and the ``/proc`` start time -- the two fields this ledger fingerprints a holder by. So
    the record written before the exec is still a LIVE reservation belonging to the very process
    that comes back. Admitting again would count one run twice, and on a budget this run only
    just fits into, the second ask queues behind the first: the run waits for a holder that is
    itself, and nothing can ever release it.
    """
    from dagrun import cgroup as cg
    from dagrun import cli

    ledger = _ledger(tmp_path)
    gib = 1024**3
    # Exactly what the pre-exec self leaves behind: this pid, this start time, 16 GiB of a 24 GiB
    # budget. `execvp` does not run atexit handlers, so the record survives into the new image.
    # 24 GiB holds one such run and not two, which is what makes the second ask queue.
    pre_exec, held = request_with_limits(
        16 * gib, tag="run", ledger=ledger, budget=24 * gib, headroom=64 * gib
    )
    assert pre_exec.verdict is Verdict.GRANT
    assert held is not None

    monkeypatch.setenv(cg.DEFAULT_NAMING.env_in_scope, "1")
    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(ledger))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, str(24 * gib))
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(64 * gib))
    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--max-mem",
            "16G",
            "--admission",
        ]
    )
    captured = capsys.readouterr()
    assert code == 0, captured.out + captured.err
    assert "QUEUED" not in captured.err, "the run queued behind its own pre-exec reservation"
    assert held_bytes(ledger) == 16 * gib, (
        "one run must hold ONE reservation across the exec, not two"
    )
    held.release()


def test_an_inherited_in_scope_sentinel_does_not_wave_an_unreserved_run_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The skip belongs to the run that HOLDS the reservation, not to anything that inherits a flag.

    ``systemd-run --setenv=DAGRUN_IN_SCOPE=1`` sets the sentinel for the whole scope, and every
    step's environment is built from ``os.environ``, so a runner invoked as a STEP of a boxed run
    reads exactly the same "1" -- a different pid, holding nothing. Skipping on the flag alone
    reserved nothing, printed no verdict and exited 0 for a 16 GiB request against a 1 GiB budget.
    The ledger, not the environment, says whether this process is already admitted.
    """
    from dagrun import cgroup as cg
    from dagrun import cli

    ledger = _ledger(tmp_path)
    gib = 1024**3
    monkeypatch.setenv(cg.DEFAULT_NAMING.env_in_scope, "1")
    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(ledger))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, str(gib))
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(64 * gib))
    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--max-mem",
            "16G",
            "--admission",
        ]
    )
    captured = capsys.readouterr()
    assert code == cli.ADMISSION_EXIT_CODE, captured.out + captured.err
    assert "REFUSED: 16.0 GiB exceeds the whole-host budget of 1.0 GiB" in captured.err
    assert held_bytes(ledger) == 0, "a refused run must hold nothing"


def test_only_this_processes_own_record_counts_as_already_admitted(tmp_path: Path) -> None:
    """The fingerprint is ``(pid, /proc starttime)``, which ``execvp`` carries across the re-exec.

    A peer's live reservation is visible in the aggregate and is emphatically NOT this process's
    licence to skip admission: pid 1 is alive on every Linux host, and counting it here would
    hand the bypass to any run that started second.
    """
    ledger = _ledger(tmp_path)
    assert admission.held_by_this_process(ledger) == 0

    _decision, held = request_with_limits(
        4096, tag="run", ledger=ledger, budget=1 << 30, headroom=1 << 30
    )
    assert held is not None
    assert admission.held_by_this_process(ledger) == 4096

    ledger.write_text(
        json.dumps(
            {
                "admissions": [
                    {"pid": 1, "starttime": None, "bytes": 8192, "tag": "run", "ts": 1.0}
                ]
            }
        ),
        encoding="utf-8",
    )
    assert held_bytes(ledger) == 8192, "a live peer's reservation is still counted in aggregate"
    assert admission.held_by_this_process(ledger) == 0
    held.release()


def test_the_default_budget_arithmetic_is_pinned_to_the_named_fraction() -> None:
    """The fraction is a production constant TWO engines share through one ledger.

    Pinned by value, and by the same literals the Rust suite pins, because every other test in
    this module supplies the budget explicitly: shrinking the constant used to leave both suites
    and the differential green while the engines disagreed about what a real host may hold.
    """
    assert admission.DEFAULT_MEM_BUDGET_FRACTION == 0.85
    # 85% of 64 GiB, less the flat 8 GiB margin.
    assert admission._budget_from_total(64 * 1024**3) == 49821620633
    # 85% of 8 GiB, less one eighth of 8 GiB rather than the whole 8 GiB.
    assert admission._budget_from_total(8 * 1024**3) == 6227702579
    assert admission._budget_from_total(0) == 0
    assert admission._headroom_from(8 * 1024**3, 8 * 1024**3) == 7516192768
    assert admission._headroom_from(1024, 8 * 1024**3) == 0


def test_a_wait_beyond_the_ceiling_is_refused_rather_than_accepted_and_then_aborted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """``--admission 1e19`` is a usage error in both engines, and says the same ceiling.

    It used to be accepted here and to ABORT the Rust engine (exit 101, "overflow when adding
    duration to instant") on the very same command line. A validated input that then panics is
    worse than a rejected one, so both editions reject it, in words that name the bound.
    """
    from dagrun import cli

    gib = 1024**3
    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(_ledger(tmp_path)))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, str(64 * gib))
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(64 * gib))
    dag = str(_tiny_dag(tmp_path))
    base = ["run", "--dag", dag, "--unsafe-no-cgroups", "--no-profile", "--max-mem", "16G"]

    for absurd in ("1e19", "1e15", "99999999999999999999", "86400.001", "nan", "-1"):
        assert cli.main([*base, "--admission", absurd]) == 2, absurd
        err = capsys.readouterr().err
        assert (
            f"--admission WAIT_S must be a finite number of seconds in [0, 86400] "
            f"(got {absurd!r})" in err
        ), err

    # The ceiling itself is accepted: the bound is inclusive, and a granted run never waits.
    assert cli.main([*base, "--admission", "86400"]) == 0, capsys.readouterr().err


def test_an_absurd_wait_is_clamped_rather_than_honoured_forever(tmp_path: Path) -> None:
    """``admit`` is a library entry point, so it bounds the wait itself as the Rust engine does.

    There the deadline is ``Instant + Duration`` and an unclamped seconds value aborts the
    process; here it would simply never return. One finite ceiling, spelled the same in both.
    """
    assert admission.MAX_WAIT_SECONDS == 31536000
    assert admission._wait_budget_s(1e19) == 31536000.0
    assert admission._wait_budget_s(1e20) == 31536000.0
    assert admission._wait_budget_s(float("nan")) == 0.0
    assert admission._wait_budget_s(-5.0) == 0.0
    assert admission._wait_budget_s(30.5) == 30.5
    decision, granted = admission.admit(
        1 << 62, tag="run", ledger=_ledger(tmp_path), poll_s=0.01, wait_s=1e19, announce=False
    )
    assert decision.verdict is not Verdict.QUEUE, (
        "a request bigger than any host is refused outright, not queued behind a year-long wait"
    )
    if granted is not None:  # only on a host whose memory cannot be measured at all
        granted.release()


def test_a_ledger_this_process_cannot_read_stops_the_run_rather_than_waving_it_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A corrupt ledger is not permission to proceed.

    The ledger IS the shared state admission consults. Guessing past an unreadable one would
    silently restore the contention this flag was asked for, and would do it in exactly the
    circumstance where nobody is watching. Exit 4, and name the file.
    """
    from dagrun import cli

    ledger = _ledger(tmp_path)
    ledger.write_text("{not json at all", encoding="utf-8")
    monkeypatch.setenv(admission.MEM_LEDGER_ENV, str(ledger))
    monkeypatch.setenv(MEM_BUDGET_BYTES_ENV, str(64 * 1024**3))
    monkeypatch.setenv(MEM_HEADROOM_BYTES_ENV, str(64 * 1024**3))
    code = cli.main(
        [
            "run",
            "--dag",
            str(_tiny_dag(tmp_path)),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--max-mem",
            "16G",
            "--admission",
        ]
    )
    assert code == cli.ADMISSION_EXIT_CODE
    err = capsys.readouterr().err
    assert "admission ledger unusable" in err
    assert str(ledger) in err
