"""The manifest is derived from the guards that implement it, PER LANE.

Two defects met here, and the fix is one mechanism.

#79 derived-enforcement-manifest: the ``capabilities`` manifest used to be a hand-typed JSON
literal with nothing tying it to the code that enforces the guards it advertises, so it could
claim enforcement that was not happening. It is now generated from
:data:`dagrun.capabilities.ENFORCEMENT_REGISTRY`, and five of its nine keys are
consulted by :func:`dagrun.capabilities.is_enforced` at the guard site itself.

#75 cpu-timeout-unboxed-fallback: exact CPU-time enforcement remains a cgroup-only guarantee.
The uncontained lane now attempts a best-effort procfs process-group lower bound, but it can miss
processes that leave the group and CPU from exited descendants before their parent reaps them.
The manifest therefore stays false while the scheduler names and tests the weaker attempt.

So each registry entry carries one flag PER LANE, ``is_enforced`` takes the lane the run is on,
and the manifest publishes both columns. What is pinned here:

* the published bytes, written out LITERALLY -- re-deriving them from the registry would pass no
  matter what the registry said, and a test parametrised over the constant it protects asserts
  nothing;
* each lane's table, likewise written out by hand;
* the contractual behaviour: flipping a flag moves the advertised exact guard; the uncontained
  CPU fallback is explicitly outside that Boolean guarantee; and
* an uncontained run says out loud that its attempted CPU bound is a lower-quality fallback.
"""

from __future__ import annotations

import dataclasses
import json
import subprocess
from collections.abc import Mapping
from pathlib import Path

import pytest

from dagrun import DagConfig, Step, run_dag
from dagrun.capabilities import (
    ENFORCEMENT_REGISTRY,
    Lane,
    enforcement_manifest,
    is_enforced,
    registry_override,
)
from dagrun.cgroup import Cgroups, NoopCgroups
from dagrun.model import ResourceHint
from dagrun.scheduler import run_dag_limited, uncontained_cpu_budget_warning

#: The exact bytes the `capabilities` subcommand promises, and which the Rust engine must
#: print too. Deliberately a literal: a test that rebuilt this from ENFORCEMENT_REGISTRY
#: would pin nothing, because it would agree with any registry at all.
PUBLISHED_MANIFEST = (
    '{"contained":{"cpu_affinity":true,"cpu_bandwidth":true,"cpu_timeout":true,'
    '"memory_max":true,"oom_detection":true,"pids_guard":false,"run_timeout":true,'
    '"wall_timeout":true,"write_domains":true},'
    '"uncontained":{"cpu_affinity":false,"cpu_bandwidth":false,"cpu_timeout":false,'
    '"memory_max":false,"oom_detection":false,"pids_guard":false,"run_timeout":true,'
    '"wall_timeout":true,"write_domains":true}}'
)

#: What each guard is worth on each lane, written out by hand. Duplicating the production table
#: is the point: this file is the second opinion, so it may not import the first one.
_CONTAINED = {
    "cpu_affinity": True,
    "cpu_bandwidth": True,
    "cpu_timeout": True,
    "memory_max": True,
    "oom_detection": True,
    "pids_guard": False,
    "run_timeout": True,
    "wall_timeout": True,
    "write_domains": True,
}
_UNCONTAINED = {
    # `--cores` REFUSES on this lane rather than degrading, so the guard is not in force here.
    "cpu_affinity": False,
    "cpu_bandwidth": False,
    # No cgroup-equivalent guarantee; the procfs lower bound remains non-contractual.
    "cpu_timeout": False,
    "memory_max": False,
    "oom_detection": False,
    "pids_guard": False,
    # Scheduler-side wall bounds and a pre-execution declaration check: no cgroup needed.
    "run_timeout": True,
    "wall_timeout": True,
    "write_domains": True,
}


def _manifest() -> dict[str, dict[str, bool]]:
    parsed = json.loads(enforcement_manifest())
    assert isinstance(parsed, dict)
    return parsed


# --------------------------------------------------------------------------------------------
# What is published
# --------------------------------------------------------------------------------------------


def test_manifest_is_exactly_the_published_bytes() -> None:
    assert enforcement_manifest() == PUBLISHED_MANIFEST


def test_capabilities_subcommand_prints_the_derived_manifest() -> None:
    out = subprocess.run(
        ["python3", "-m", "dagrun", "capabilities"],
        capture_output=True,
        text=True,
        check=True,
        cwd=Path(__file__).resolve().parents[1],
    ).stdout
    assert out == PUBLISHED_MANIFEST + "\n"


def test_the_manifest_declares_both_lanes_and_nothing_else() -> None:
    assert sorted(_manifest()) == ["contained", "uncontained"]


def test_the_contained_lane_matches_the_hand_written_table() -> None:
    assert _manifest()["contained"] == _CONTAINED


def test_the_uncontained_lane_matches_the_hand_written_table() -> None:
    assert _manifest()["uncontained"] == _UNCONTAINED


def test_the_uncontained_lane_does_not_claim_the_cgroup_guards() -> None:
    # Named one at a time so a regression says WHICH claim came back.
    uncontained = _manifest()["uncontained"]
    assert uncontained["cpu_timeout"] is False
    assert uncontained["memory_max"] is False
    assert uncontained["oom_detection"] is False
    assert uncontained["cpu_bandwidth"] is False
    assert uncontained["cpu_affinity"] is False


def test_both_lanes_carry_the_same_sorted_key_set() -> None:
    # A reader diffs the two columns, so a key present in one and absent from the other would
    # read as "not applicable" when it means "nobody wrote it down".
    manifest = _manifest()
    keys = sorted(_CONTAINED)
    assert list(manifest["contained"]) == keys
    assert list(manifest["uncontained"]) == keys


def test_the_manifest_is_compact_json_with_no_incidental_whitespace() -> None:
    # The Rust twin derives its own manifest and `make cross` holds the two byte-identical; a
    # stray space here is a cross-language failure, so catch it in-engine first.
    assert enforcement_manifest() == json.dumps(
        _manifest(), separators=(",", ":"), sort_keys=True
    )


def test_every_registry_summary_says_something() -> None:
    # The summaries are the only prose left describing each guard; an empty one would make
    # the registry strictly less informative than the comment block it replaced.
    assert len(ENFORCEMENT_REGISTRY) == 9
    assert all(c.summary.strip() for c in ENFORCEMENT_REGISTRY)
    keys = [c.key for c in ENFORCEMENT_REGISTRY]
    assert keys == sorted(keys), "registry order should read as the manifest does"


def test_every_published_flag_is_what_a_guard_site_would_be_told() -> None:
    # The manifest is DERIVED, so the thing worth pinning is that the derivation agrees with the
    # answer each guard site gets. Both tables above are literals, so this is not circular.
    for lane, table in ((Lane.CONTAINED, _CONTAINED), (Lane.UNCONTAINED, _UNCONTAINED)):
        for key, want in table.items():
            assert is_enforced(key, lane) is want, (
                f"{lane.value}.{key}: the manifest and the guard site disagree"
            )


def test_a_misspelled_guard_site_raises_instead_of_reading_as_unenforced() -> None:
    # The quiet reading -- "unknown, so False" -- would silently disable the guard whose
    # name was typo'd, which is the failure this module exists to prevent.
    with pytest.raises(KeyError, match="unknown enforcement capability 'cpu_timout'"):
        is_enforced("cpu_timout", Lane.CONTAINED)


def test_registry_override_moves_the_manifest_and_restores_it() -> None:
    with registry_override("memory_max", False):
        assert '"memory_max":true' not in enforcement_manifest()
    assert enforcement_manifest() == PUBLISHED_MANIFEST
    with pytest.raises(KeyError):
        with registry_override("no_such_guard", False):
            pass


def test_a_single_lane_override_leaves_the_other_lane_alone() -> None:
    # A bracket that moved both columns could not prove the lane argument is honoured.
    with registry_override("wall_timeout", False, lane=Lane.UNCONTAINED):
        assert is_enforced("wall_timeout", Lane.CONTAINED) is True
        assert is_enforced("wall_timeout", Lane.UNCONTAINED) is False
        manifest = _manifest()
        assert manifest["contained"]["wall_timeout"] is True
        assert manifest["uncontained"]["wall_timeout"] is False
    assert enforcement_manifest() == PUBLISHED_MANIFEST


# --------------------------------------------------------------------------------------------
# The guards follow the flags
# --------------------------------------------------------------------------------------------


def _boxed_scope(root: Path) -> Cgroups:
    """A Cgroups pointed at a plain temp dir standing in for a delegated scope root."""
    (root / "cpu.max").write_text("100000 100000")
    (root / "memory.max").write_text(str(8 * 1024**3))
    cg = Cgroups()
    cg.enabled = True
    cg.root = root
    return cg


def test_inner_memory_cap_follows_the_memory_max_capability_flag(tmp_path: Path) -> None:
    """``memory_max`` is load-bearing: flipping it off must stop the inner cap being written.

    Otherwise the manifest could say ``"memory_max":false`` while the kernel cap was still
    applied -- the same class of lie in the other direction.
    """
    cg = _boxed_scope(tmp_path)
    applied = tmp_path / "step-g.j" / "memory.max"

    cg.prepare_command("g.j", "true", mem_max=4096)
    assert applied.read_text() == "4096"
    assert _manifest()["contained"]["memory_max"] is True

    applied.unlink()
    with registry_override("memory_max", False):
        # Read the CONTAINED column by name: the uncontained one already says false, so a bare
        # substring search for `"memory_max":false` would hold whatever the bracket did.
        assert _manifest()["contained"]["memory_max"] is False
        cg.prepare_command("g.j", "true", mem_max=4096)
    assert not applied.exists(), (
        "memory.max was written even though the registry says memory_max is unenforced"
    )


def test_pids_ceiling_follows_the_pids_guard_capability_flag(tmp_path: Path) -> None:
    """``pids_guard`` is declared FALSE on both lanes, and that declaration is now real.

    Before this change the manifest said "no PID ceiling" while ``prepare_command`` would
    happily write ``pids.max`` for any caller that set the limit. Nothing in the runner does,
    which is why the false went unnoticed -- but "no caller happens to use it" is not the same
    statement as "this engine does not enforce it", and only the second one is publishable.
    """
    cg = _boxed_scope(tmp_path)
    cg.set_worker_pids_max(64)
    applied = tmp_path / "step-g.j" / "pids.max"

    cg.prepare_command("g.j", "true")
    assert not applied.exists(), (
        "pids.max was written although the manifest advertises pids_guard as unenforced"
    )

    with registry_override("pids_guard", True):
        assert _manifest()["contained"]["pids_guard"] is True
        cg.prepare_command("g.j", "true")
    assert applied.read_text() == "64"


def _one_step_dag(cmd: str, *, timeout: int, cpu_timeout: int = 0) -> DagConfig:
    return DagConfig(
        steps=(
            Step(
                group="g",
                job="s",
                desc="",
                cmd=cmd,
                timeout=timeout,
                cpu_timeout=cpu_timeout,
                hint=ResourceHint(),
            ),
        ),
        default_step_cpu_timeout=0,
    )


def test_per_step_wall_ceiling_follows_the_wall_timeout_capability_flag() -> None:
    cfg = _one_step_dag("sleep 2", timeout=1)

    assert _manifest()["contained"]["wall_timeout"] is True
    enforced = run_dag_limited(cfg, max_steps=1, max_cpus=1, verbosity=0)
    assert not enforced.ok, "a 2s step under a 1s wall ceiling must be cut"
    assert "TIMEOUT >1s" in enforced.outcomes[0].reason

    with registry_override("wall_timeout", False):
        assert '"wall_timeout":true' not in enforcement_manifest()
        unenforced = run_dag_limited(cfg, max_steps=1, max_cpus=1, verbosity=0)
    assert unenforced.ok, (
        "with wall_timeout declared unenforced the step must be allowed to finish; it was "
        "still cut, so the manifest and the guard can disagree"
    )


class _OomingCgroups(NoopCgroups):
    """A boxed manager whose cgroup reports two OOM kills for every step.

    The scheduler reads ``memory.events`` once and takes the OOM count from it, so that is the
    method a stand-in has to answer; overriding ``oom_kills`` alone would leave this test
    silently inert.
    """

    enabled = True

    def memory_events(self, tag: str) -> Mapping[str, int]:
        return {"oom_kill": 2, "oom": 2, "low": 0, "high": 0, "max": 0}


def test_oom_attribution_follows_the_oom_detection_capability_flag() -> None:
    cfg = _one_step_dag("exit 1", timeout=30)

    assert _manifest()["contained"]["oom_detection"] is True
    enforced = run_dag_limited(
        cfg, max_steps=1, max_cpus=1, cgroups=_OomingCgroups(), verbosity=0
    )
    assert "OOM-KILLED" in enforced.outcomes[0].reason

    with registry_override("oom_detection", False):
        assert _manifest()["contained"]["oom_detection"] is False
        unenforced = run_dag_limited(
            cfg, max_steps=1, max_cpus=1, cgroups=_OomingCgroups(), verbosity=0
        )
    assert "OOM-KILLED" not in unenforced.outcomes[0].reason, (
        "the memory.events counter was still consulted although oom_detection is declared "
        "unenforced; the manifest would be advertising an attribution it does not make"
    )


class _BurningCgroups(NoopCgroups):
    """A boxed manager whose cpu.stat reports 10 CPU-seconds already consumed."""

    enabled = True

    def cpu_stats(self, tag: str) -> Mapping[str, int]:
        return {"usage_usec": 10_000_000, "user_usec": 10_000_000, "system_usec": 0}


def test_cpu_budget_reaping_follows_the_contained_capability_flag() -> None:
    """The exact cgroup guard remains coupled to its advertised contained flag."""
    cfg = _one_step_dag("sleep 4", timeout=60, cpu_timeout=1)

    assert _manifest()["contained"]["cpu_timeout"] is True
    enforced = run_dag_limited(
        cfg, max_steps=1, max_cpus=1, cgroups=_BurningCgroups(), verbosity=0
    )
    assert not enforced.ok
    assert "CPU-TIMEOUT" in enforced.outcomes[0].reason

    with registry_override("cpu_timeout", False):
        assert _manifest()["contained"]["cpu_timeout"] is False
        unenforced = run_dag_limited(
            cfg, max_steps=1, max_cpus=1, cgroups=_BurningCgroups(), verbosity=0
        )
    assert unenforced.ok, (
        "the step was still reaped over its CPU budget although the exact contained guard is "
        "declared unenforced; the manifest and its contractual guard can disagree"
    )


def test_uncontained_cpu_timeout_contract_remains_false() -> None:
    """The best-effort procfs attempt is not promoted to a cgroup-equivalent guarantee."""
    assert _manifest()["uncontained"]["cpu_timeout"] is False
    assert is_enforced("cpu_timeout", Lane.UNCONTAINED) is False
    with registry_override("cpu_timeout", True, lane=Lane.UNCONTAINED):
        assert _manifest()["uncontained"]["cpu_timeout"] is True
        assert is_enforced("cpu_timeout", Lane.UNCONTAINED) is True


# --------------------------------------------------------------------------------------------
# An uncontained run says so at the console
# --------------------------------------------------------------------------------------------


def _step(cpu_timeout: int) -> Step:
    return Step("g", "j", "burns cpu", "true", cpu_timeout=cpu_timeout, timeout=5)


def test_a_live_budget_produces_a_notice_that_counts_it_and_names_the_largest() -> None:
    cfg = DagConfig(steps=(_step(7), _step(3)))
    notice = uncontained_cpu_budget_warning(cfg)
    assert notice is not None
    assert "UNCONTAINED run" in notice
    assert "best-effort procfs" in notice
    assert "will police 2 step(s)" in notice
    assert "largest 7s" in notice


def test_the_default_budget_counts_for_the_best_effort_floor() -> None:
    # A step that declares nothing still gets `default_step_cpu_timeout`, and that budget is
    # policed by the same best-effort floor as a declared one. Silence would hide the attempt.
    cfg = DagConfig(steps=(Step("g", "j", "declares nothing", "true"),))
    notice = uncontained_cpu_budget_warning(cfg)
    assert notice is not None
    assert "will police 1 step(s)" in notice
    assert "largest 10s" in notice


def test_a_graph_with_the_guard_switched_off_everywhere_is_not_nagged() -> None:
    # The other side of the assertion: this must not become an unconditional banner. A graph
    # that disabled the CPU guard on purpose is owed nothing.
    cfg = DagConfig(steps=(_step(0),), default_step_cpu_timeout=0)
    assert uncontained_cpu_budget_warning(cfg) is None


def test_the_platform_multiplier_is_applied_before_the_notice_quotes_a_number() -> None:
    # The notice must quote the budget that WOULD have been enforced, not the graph's canonical
    # number, or a slow platform reads a figure that never existed.
    cfg = DagConfig(steps=(_step(4),), cpu_timeout_multiplier=2.5)
    notice = uncontained_cpu_budget_warning(cfg)
    assert notice is not None
    assert "largest 10s" in notice


def test_an_uncontained_run_names_the_best_effort_cpu_floor(
    capsys: pytest.CaptureFixture[str],
) -> None:
    cfg = DagConfig(steps=(dataclasses.replace(_step(7), cmd="true"),))
    result = run_dag(cfg, jobs=1, verbosity=0)
    assert result.ok
    message = capsys.readouterr().err
    assert "UNCONTAINED run" in message
    assert "best-effort procfs process-group CPU floor" in message
    assert "largest 7s" in message


def test_a_contained_run_does_not_print_the_uncontained_notice(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # The other side: an enabled manager really does read cpu.stat, so warning there would be a
    # false alarm and would train operators to ignore the line.
    class _Enabled(NoopCgroups):
        enabled = True

    cfg = DagConfig(steps=(dataclasses.replace(_step(7), cmd="true"),))
    result = run_dag(cfg, jobs=1, verbosity=0, cgroups=_Enabled())
    assert result.ok
    assert "UNCONTAINED run" not in capsys.readouterr().err
