"""Boxed build-job cap wired at the quota boundary (prepare_command).

Three-part bracket per boxing feature:
  * CAUGHT     — an UNPINNED step under a wide scope quota is bounded, not NUM_JOBS=284.
  * LEGITIMATE — N stated honest configs pass unharmed (a clamp-everything mechanism
                 would satisfy CAUGHT alone, so these prove it is not inert-by-blocking).
  * CLEANUP    — the planted cap does not leak into the runner's own environment.

``prepare_command`` writes cgroup control files; against a plain tmp dir those writes land
as regular files and the ``cpu.max`` read-back verification round-trips, so the boxing path
runs end-to-end without a real delegated cgroup.
"""

from __future__ import annotations

import os
import re
import subprocess
from pathlib import Path

from dagrun import cgroup as cgroup_module
from dagrun.cgroup import Cgroups

GIB = 1024**3


def _boxed(root: Path, *, cpu_max: str, memory_max: str) -> Cgroups:
    """A Cgroups whose scope root advertises the given effective caps."""
    (root / "cpu.max").write_text(cpu_max)
    (root / "memory.max").write_text(memory_max)
    cg = Cgroups()
    cg.enabled = True
    cg.root = root
    return cg


def _cargo_jobs(cmd: str) -> int:
    m = re.search(r"export CARGO_BUILD_JOBS=(\d+)", cmd)
    assert m, f"no CARGO_BUILD_JOBS export in wrapped command:\n{cmd}"
    return int(m.group(1))


def test_unpinned_step_is_bounded_not_284(tmp_path: Path) -> None:
    # Scope grants 284 cores (28400000/100000) and an 8 GiB aggregate cap.
    cg = _boxed(tmp_path, cpu_max="28400000 100000", memory_max=str(8 * GIB))
    # CAUGHT: unpinned step (no inner cpu/mem) — inherits scope caps -> j8, never 284.
    wrapped = cg.prepare_command("build.dbi_release", "cargo build --release")
    assert _cargo_jobs(wrapped) == 8
    assert "cargo build --release" in wrapped


def test_legitimate_configs_pass_unharmed(tmp_path: Path) -> None:
    cg = _boxed(tmp_path, cpu_max="28400000 100000", memory_max=str(64 * GIB))
    # LEGITIMATE N=3:
    # (1) a pinned small step keeps its own width (cpu-bound 4, roomy 64 GiB scope).
    assert _cargo_jobs(cg.prepare_command("s1", "cargo build", cpu_count=4, mem_max=64 * GIB)) == 4
    # (2) a pinned step whose memory cap is the binding constraint is mem-bounded.
    assert _cargo_jobs(cg.prepare_command("s2", "cargo build", cpu_count=32, mem_max=8 * GIB)) == 8
    # (3) an explicit ``cargo -j`` in the command survives — the env is only a floor.
    w3 = cg.prepare_command("s3", "cargo build -j2", cpu_count=16, mem_max=64 * GIB)
    assert "cargo build -j2" in w3 and _cargo_jobs(w3) == 16


def test_cap_does_not_leak_into_runner_env(tmp_path: Path) -> None:
    # CLEANUP: prepare_command emits the export INTO the step's shell string only; it must
    # not mutate the runner process env (that would cross-contaminate sibling steps).
    os.environ.pop("CARGO_BUILD_JOBS", None)
    cg = _boxed(tmp_path, cpu_max="28400000 100000", memory_max=str(8 * GIB))
    cg.prepare_command("s", "cargo build", cpu_count=4, mem_max=4 * GIB)
    assert "CARGO_BUILD_JOBS" not in os.environ


def test_cgroup_tag_encoding_does_not_alias_distinct_steps() -> None:
    assert cgroup_module._sanitize("a/b.j") != cgroup_module._sanitize("a_b.j")
    assert cgroup_module._sanitize("a~2fb.j") != cgroup_module._sanitize("a/b.j")


def test_failed_cgroup_migration_refuses_user_command(tmp_path: Path) -> None:
    cg = _boxed(tmp_path, cpu_max="100000 100000", memory_max=str(4 * GIB))
    marker = tmp_path / "user-command-ran"
    wrapped = cg.prepare_command("g.j", f"touch {marker}")
    # A plain temporary directory does not synthesize the kernel-owned roster file, so make the
    # redirect fail the same way an unavailable/unwritable cgroup.procs would fail on cgroupfs.
    (tmp_path / cgroup_module._sanitize("g.j") / "cgroup.procs").mkdir()

    completed = subprocess.run(["bash", "-c", wrapped], check=False)

    assert completed.returncode == 125
    assert not marker.exists(), "the user command ran despite never joining its capped cgroup"


def test_overlong_encoded_cgroup_name_is_rejected_before_user_command(tmp_path: Path) -> None:
    cg = _boxed(tmp_path, cpu_max="100000 100000", memory_max=str(4 * GIB))
    marker = tmp_path / "overlong-user-command-ran"
    wrapped = cg.prepare_command("/" * 100, f"touch {marker}")

    completed = subprocess.run(["bash", "-c", wrapped], check=False)

    assert completed.returncode == 125
    assert not marker.exists()
